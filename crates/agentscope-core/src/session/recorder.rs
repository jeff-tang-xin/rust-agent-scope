//! 运行记录器（对应 Java `SessionRecorder`，613 行）。
//!
//! 一次执行（run）对应一个记录器：它持有会话日志的写者租约，把 ReAct 循环里发生的
//! 事实按批次提交。**它不做业务判断**，只负责「把已经发生的事写成不可变事件」，
//! 以及决定一个事实什么时候必须落盘。
//!
//! # 关键设计：状态快照 + diff，而不是逐点埋点
//!
//! 对话历史的事件不是「每加一条消息就写一条」，而是 [`SessionRecorder::capture_state`]
//! 对整份 [`AgentState`] 做一次快照，与上一次快照比对后产出增量：
//!
//! 1. 历史前缀被改过（不是纯追加）→ `context/replaced`；
//! 2. 每条新消息按类型写 `message/*` 或 `tool/result`，其中每个 `ToolUse` 块再展开
//!    一条 `tool/requested`；
//! 3. 三组子上下文按字段 diff 写 `task/changed` / `plan/changed` / `permission/changed`；
//! 4. 最后写 `state/checkpoint`（新基线）。
//!
//! 这样 agent 内部可以随意改 `AgentState.context`，不必在每个写入点记得调 recorder；
//! 代价是每个检查点都要序列化一次状态（Java 同样如此）。
//!
//! # 提交与失败
//!
//! `append` 只进内存缓冲，`flush` 才提交。缓冲上限 16MB，超了直接报错——宁可炸，
//! 也不要写出一条残缺的历史。提交失败后记录器进入失败态，**后续任何 append 都报错**：
//! 持久化失败必须让调用方看见，不能悄悄降级成「模型成功」。
//!
//! # 与 Java 的有意差异
//!
//! Java 起了一个 1 秒周期的心跳线程（续租 + flush + 导出）。Rust 侧不起后台线程：
//! 续租与 flush 发生在每次 [`SessionRecorder::flush_now`]，由调用方在关键点驱动
//! （`start` / `checkpoint` / `step` / `finish`）。少一个线程，也少一处隐蔽的竞态。

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{json, Value};
use uuid::Uuid;

use super::events::SessionEvent;
use super::journal::{SessionLog, Writer};
use super::projection::SessionProjection;
use super::SessionError;
use crate::message::{ContentBlock, GenerateReason, Msg, Role};
use crate::state::AgentState;

/// 记录器持有的写者租约时长。
pub const RECORDER_LEASE: Duration = Duration::from_secs(120);

/// 待提交缓冲上限（对应 Java 的 `16 * 1024 * 1024`）。
const MAX_PENDING_BYTES: usize = 16 * 1024 * 1024;

/// 一次运行的记录器。
pub struct SessionRecorder {
    log: Arc<dyn SessionLog>,
    inner: Mutex<Inner>,
}

struct Inner {
    writer: Writer,
    run_id: String,
    turn_id: String,
    /// 已提交到日志的最后一个序号。
    durable_seq: i64,
    pending: Vec<SessionEvent>,
    pending_bytes: usize,
    batch_id: Option<String>,
    /// 已经写进日志的消息 id（`capture_state` 靠它判「新增」）。
    recorded_messages: HashSet<String>,
    /// 本轮输入消息 id（用于补 `input/applied`）。
    input_messages: HashSet<String>,
    /// 上一次快照的状态 JSON 与逐条消息 JSON（diff 基线）。
    previous_state: Option<String>,
    previous_messages: Vec<String>,
    /// 打开记录器时读到的投影（`start` 用它决定要不要补基线/对账事件）。
    initial: SessionProjection,
    /// 这个回合此前已有事件 → 写 `turn/resumed` 而不是 `turn/start`。
    resumed_turn: bool,
    step_id: Option<String>,
    result_json: Option<String>,
    result_reason: Option<GenerateReason>,
    result_status: Option<String>,
    /// 工具调用 id → 动作 id。
    action_ids: Vec<(String, String)>,
    failure: Option<String>,
    closed: bool,
}

impl Inner {
    fn check(&self) -> Result<(), SessionError> {
        if self.closed {
            return Err(SessionError::log("session recorder closed"));
        }
        if let Some(failure) = &self.failure {
            return Err(SessionError::log(format!(
                "session persistence failed: {failure}"
            )));
        }
        Ok(())
    }
}

impl SessionRecorder {
    /// 打开一次运行的记录器。
    ///
    /// 会先读一遍已提交前缀做三项前置检查：`run_id` 不能复用、已完成的回合不能续写、
    /// 存在结果未知的工具调用时必须先对账。任一不满足即拒绝打开——记录器一旦打开
    /// 就会开始写事实，不能让它在有歧义的历史上继续。
    pub fn new(
        log: Arc<dyn SessionLog>,
        turn_id: Option<String>,
        run_id: impl Into<String>,
    ) -> Result<Self, SessionError> {
        let run_id = run_id.into();
        if run_id.trim().is_empty() {
            return Err(SessionError::invalid("run id must not be blank"));
        }
        let turn_id = turn_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        if turn_id.trim().is_empty() {
            return Err(SessionError::invalid("turn id must not be blank"));
        }
        let writer = log.acquire(&run_id, RECORDER_LEASE)?;
        // 打开失败必须把租约还回去，否则这个会话就再没人能写了。
        let prepared = Self::inspect(log.as_ref(), &run_id, &turn_id)
            .inspect_err(|_error| {
                let _ = log.release(&writer);
            })?;
        Ok(Self {
            log,
            inner: Mutex::new(Inner {
                writer,
                run_id,
                turn_id,
                durable_seq: prepared.initial.as_of_seq(),
                pending: Vec::new(),
                pending_bytes: 0,
                batch_id: None,
                recorded_messages: HashSet::new(),
                input_messages: HashSet::new(),
                previous_state: prepared.previous_state,
                previous_messages: prepared.previous_messages,
                initial: prepared.initial,
                resumed_turn: prepared.resumed_turn,
                step_id: None,
                result_json: None,
                result_reason: None,
                result_status: None,
                action_ids: Vec::new(),
                failure: None,
                closed: false,
            }),
        })
    }

    /// 打开前的历史体检。
    fn inspect(
        log: &dyn SessionLog,
        run_id: &str,
        turn_id: &str,
    ) -> Result<Prepared, SessionError> {
        let initial = SessionProjection::read(log)?;
        let mut resumed_turn = false;
        for event in log.scan(0, initial.as_of_seq())? {
            if event.execution_run_id.as_deref() == Some(run_id) {
                return Err(SessionError::log(
                    "run id already used; prepare a new execution to resume",
                ));
            }
            if event.turn_id.as_deref() != Some(turn_id) {
                continue;
            }
            resumed_turn = true;
            let completed = event.kind == "turn/completed"
                || (event.kind == "turn/end"
                    && event
                        .data()
                        .ok()
                        .and_then(|data| data.get("status").and_then(Value::as_str).map(str::to_string))
                        .as_deref()
                        == Some("completed"));
            if completed {
                return Err(SessionError::log(
                    "turn already completed; use a new turn id",
                ));
            }
        }
        if !initial.uncertain_tool_calls().is_empty() {
            return Err(SessionError::log(format!(
                "explicit reconciliation required for unknown tool outcomes: {:?}",
                initial.uncertain_tool_calls()
            )));
        }
        let previous_state = initial.state_json().map(str::to_string);
        let previous_messages = match initial.restore() {
            Some(state) => message_json(&state)?,
            None => Vec::new(),
        };
        Ok(Prepared {
            initial,
            resumed_turn,
            previous_state,
            previous_messages,
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// 本次运行的标识。
    pub fn run_id(&self) -> String {
        self.lock().run_id.clone()
    }

    /// 本次逻辑回合的标识。
    pub fn turn_id(&self) -> String {
        self.lock().turn_id.clone()
    }

    /// 已提交的最后一个序号（不含内存缓冲）。
    pub fn durable_seq(&self) -> i64 {
        self.lock().durable_seq
    }

    /// 打开时读到的投影。
    pub fn initial(&self) -> SessionProjection {
        self.lock().initial.clone()
    }

    /// 已写进日志的消息 id 集合。
    pub fn accepted_message_ids(&self) -> HashSet<String> {
        self.lock().recorded_messages.clone()
    }

    /// 记下某个工具调用对应的动作 id（`tool/dispatch` 会带上它）。
    pub fn action_started(&self, tool_call_id: &str, action_id: &str) {
        let mut inner = self.lock();
        inner.action_ids.retain(|(id, _)| id != tool_call_id);
        inner
            .action_ids
            .push((tool_call_id.to_string(), action_id.to_string()));
    }

    /// 取某个工具调用的动作 id（没有则空串，与 Java 一致）。
    pub fn action_id(&self, tool_call_id: &str) -> String {
        self.lock()
            .action_ids
            .iter()
            .find(|(id, _)| id == tool_call_id)
            .map(|(_, action)| action.clone())
            .unwrap_or_default()
    }

    /// 追加一条必须被理解的事件。
    pub fn append(&self, kind: &str, payload: Value) -> Result<i64, SessionError> {
        self.append_with(kind, payload, true)
    }

    /// 追加一条事件（`required=false` 表示旧版本重放时可跳过）。
    pub fn append_with(
        &self,
        kind: &str,
        payload: Value,
        required: bool,
    ) -> Result<i64, SessionError> {
        let mut inner = self.lock();
        self.append_locked(&mut inner, kind, payload, required)
    }

    /// 追加并立即提交（同步域里需要一个已确认的边界时用它）。
    pub fn record_now(&self, kind: &str, payload: Value) -> Result<(), SessionError> {
        self.append(kind, payload)?;
        self.flush_now()
    }

    /// 只在记录器仍打开时记录（父运行结束时，子事件可能晚到）。
    pub fn record_if_open(&self, kind: &str, payload: Value) -> Result<(), SessionError> {
        if self.lock().closed {
            return Ok(());
        }
        self.record_now(kind, payload)
    }

    /// 记录器内部追加：序号 = 已提交水位 + 缓冲长度 + 1。
    fn append_locked(
        &self,
        inner: &mut Inner,
        kind: &str,
        payload: Value,
        required: bool,
    ) -> Result<i64, SessionError> {
        inner.check()?;
        let payload_json = serde_json::to_string(&payload)?;
        let bytes = payload_json.len();
        if inner.pending_bytes + bytes > MAX_PENDING_BYTES {
            return Err(SessionError::log("session pending buffer exhausted"));
        }
        let seq = inner.durable_seq + inner.pending.len() as i64 + 1;
        let event = SessionEvent::new(
            Uuid::new_v4().to_string(),
            seq,
            super::now_millis(),
            kind,
            Some(inner.run_id.clone()),
            Some(inner.turn_id.clone()),
            required,
            payload_json,
        )?;
        inner.pending.push(event);
        inner.pending_bytes += bytes;
        Ok(seq)
    }

    /// 提交缓冲（同时续租）。
    ///
    /// 提交失败会记入失败态并返回错误：调用方必须把它当失败处理。
    pub fn flush_now(&self) -> Result<(), SessionError> {
        let mut inner = self.lock();
        inner.check()?;
        self.log.renew(&inner.writer, RECORDER_LEASE)?;
        if inner.pending.is_empty() {
            return Ok(());
        }
        let batch = inner
            .batch_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        // 批次 id 在重试之间保持不变：同一批次重复提交不会变成两批。
        inner.batch_id = Some(batch.clone());
        let pending = std::mem::take(&mut inner.pending);
        let expected = inner.durable_seq;
        match self.log.commit(&inner.writer, &batch, expected, pending) {
            Ok(head) => {
                inner.durable_seq = head.seq;
                inner.pending_bytes = 0;
                inner.batch_id = None;
                Ok(())
            }
            Err(error) => {
                inner.failure = Some(error.to_string());
                Err(SessionError::log(format!(
                    "failed to commit session prefix: {error}"
                )))
            }
        }
    }

    /// 会话开始：补基线/对账、写 `run/start`、`turn/start|resumed`、`input/received`。
    pub fn start(&self, input: &[Msg], state: &AgentState) -> Result<(), SessionError> {
        {
            let mut inner = self.lock();
            inner.check()?;
            if inner.initial.state_json().is_none() {
                let state_json = state.to_json().map_err(state_error)?;
                self.append_locked(
                    &mut inner,
                    "migration/baseline",
                    json!({ "stateJson": state_json, "coverage": "baseline_only" }),
                    true,
                )?;
            }
            let interrupted = inner.initial.active_runs().to_vec();
            if !interrupted.is_empty() {
                let source_seq = inner.initial.as_of_seq();
                self.append_locked(
                    &mut inner,
                    "recovery/applied",
                    json!({ "interruptedRuns": interrupted, "sourceSeq": source_seq }),
                    true,
                )?;
            }
            let run_id = inner.run_id.clone();
            self.append_locked(
                &mut inner,
                "run/start",
                json!({ "executionRunId": run_id, "coverage": "adapter_chunks" }),
                true,
            )?;
            let (kind, turn_id) = if inner.resumed_turn {
                ("turn/resumed", inner.turn_id.clone())
            } else {
                ("turn/start", inner.turn_id.clone())
            };
            self.append_locked(&mut inner, kind, json!({ "turnId": turn_id }), true)?;
            for message in input {
                inner.input_messages.insert(message.id.clone());
            }
            // 空输入也写一条：`input/received` 是「这个回合收到了什么」的事实，
            // 缺了它续跑就无从判断输入是否已并入状态。
            let messages: Vec<&Msg> = input.iter().collect();
            self.append_locked(
                &mut inner,
                "input/received",
                json!({ "messages": messages }),
                true,
            )?;
        }
        self.flush_now()
    }

    /// 进入新一轮迭代：收尾上一步，开新一步。
    pub fn step(&self, iteration: usize) -> Result<(), SessionError> {
        {
            let mut inner = self.lock();
            inner.check()?;
            if let Some(step_id) = inner.step_id.clone() {
                self.append_locked(
                    &mut inner,
                    "step/end",
                    json!({ "stepId": step_id, "status": "continued" }),
                    true,
                )?;
            }
            let step_id = Uuid::new_v4().to_string();
            self.append_locked(
                &mut inner,
                "step/start",
                json!({ "stepId": step_id, "iteration": iteration }),
                true,
            )?;
            inner.step_id = Some(step_id);
        }
        self.flush_now()
    }

    /// 记下本次运行的最终回复（`finish` 会用它写 `turn/output` 与终态）。
    pub fn result(&self, result: &Msg) {
        let mut inner = self.lock();
        inner.result_json = serde_json::to_string(result).ok();
        if let Some(reason) = result.generate_reason {
            inner.result_reason = Some(reason);
            inner.result_status = Some(outcome(Some(reason)).to_string());
        }
    }

    /// 状态快照 + diff（本模块的核心）。
    pub fn capture_state(&self, state: &AgentState, reason: &str) -> Result<(), SessionError> {
        let mut inner = self.lock();
        inner.check()?;
        let state_json = state.to_json().map_err(state_error)?;
        let messages = state.context.clone();
        let messages_json = message_json(state)?;

        // 快路径：状态没变，且所有消息都已写过 → 无事可做。
        if inner.previous_state.as_deref() == Some(state_json.as_str())
            && messages
                .iter()
                .all(|message| inner.recorded_messages.contains(&message.id))
        {
            return Ok(());
        }

        // 1) 前缀被改过（不是纯追加）→ 整段上下文替换。
        let mut prefix = 0;
        while prefix < inner.previous_messages.len()
            && prefix < messages_json.len()
            && inner.previous_messages[prefix] == messages_json[prefix]
        {
            prefix += 1;
        }
        if prefix < inner.previous_messages.len() {
            let previous_as_of_seq = inner.durable_seq;
            self.append_locked(
                &mut inner,
                "context/replaced",
                json!({
                    "reason": reason,
                    "messages": messages,
                    "previousAsOfSeq": previous_as_of_seq,
                }),
                true,
            )?;
        }

        // 2) 逐条新增消息（顺带展开工具调用请求）。
        for message in &messages {
            if !inner.recorded_messages.insert(message.id.clone()) {
                continue;
            }
            if inner.input_messages.contains(&message.id) {
                let input_id = message.id.clone();
                self.append_locked(
                    &mut inner,
                    "input/applied",
                    json!({ "inputId": input_id }),
                    true,
                )?;
            }
            let kind = if message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
            {
                "tool/result"
            } else {
                match message.role {
                    Role::Assistant => "message/assistant",
                    Role::System => "message/system",
                    _ => "message/user",
                }
            };
            self.append_locked(
                &mut inner,
                kind,
                json!({ "message": message }),
                true,
            )?;
            for block in &message.content {
                if let ContentBlock::ToolUse { id, .. } = block {
                    self.append_locked(
                        &mut inner,
                        "tool/requested",
                        json!({
                            "messageId": message.id,
                            "toolCallId": id,
                            "call": block,
                        }),
                        true,
                    )?;
                }
            }
        }

        // 3) 三组子上下文按字段 diff。
        let current: Value = serde_json::from_str(&state_json)?;
        let previous: Value = match &inner.previous_state {
            Some(text) => serde_json::from_str(text)?,
            None => json!({}),
        };
        for (field, kind) in [
            ("tasks_context", "task/changed"),
            ("plan_mode_context", "plan/changed"),
            ("permission_context", "permission/changed"),
        ] {
            let Some(value) = current.get(field) else {
                continue;
            };
            if value.is_null() || Some(value) == previous.get(field) {
                continue;
            }
            let payload = json!({ "state": value });
            self.append_locked(&mut inner, kind, payload, true)?;
        }

        // 4) 新基线：重放从这里取代之前的所有增量。
        self.append_locked(
            &mut inner,
            "state/checkpoint",
            json!({
                "projectorVersion": 1,
                "stateJson": state_json,
                "reason": reason,
            }),
            true,
        )?;
        inner.previous_state = Some(state_json);
        inner.previous_messages = messages_json;
        Ok(())
    }

    /// 快照 + 立即提交。
    pub fn checkpoint(&self, state: &AgentState, reason: &str) -> Result<(), SessionError> {
        self.capture_state(state, reason)?;
        self.flush_now()
    }

    /// 运行结束：终态快照、输出、`step/end`、`turn/<status>`、`run/end`，然后释放租约。
    pub fn finish(&self, state: &AgentState, status: &str) -> Result<(), SessionError> {
        self.capture_state(state, &format!("run_{status}"))?;
        {
            let mut inner = self.lock();
            inner.check()?;
            let terminal = if status == "completed" {
                inner
                    .result_status
                    .clone()
                    .unwrap_or_else(|| status.to_string())
            } else {
                status.to_string()
            };
            if let Some(result_json) = inner.result_json.clone() {
                if terminal != "suspended" {
                    let message: Value = serde_json::from_str(&result_json)?;
                    let output_status = if terminal == "completed" {
                        "completed"
                    } else {
                        "incomplete"
                    };
                    self.append_locked(
                        &mut inner,
                        "turn/output",
                        json!({ "message": message, "status": output_status }),
                        true,
                    )?;
                }
            }
            let reason = inner.result_reason;
            let outcome_payload = json!({ "status": terminal, "reason": reason });
            if let Some(step_id) = inner.step_id.clone() {
                self.append_locked(
                    &mut inner,
                    "step/end",
                    json!({ "stepId": step_id, "status": terminal }),
                    true,
                )?;
                inner.step_id = None;
            }
            self.append_locked(
                &mut inner,
                &format!("turn/{terminal}"),
                outcome_payload.clone(),
                true,
            )?;
            self.append_locked(&mut inner, "run/end", outcome_payload, true)?;
        }
        self.flush_now()?;
        self.close()
    }

    /// 释放写者租约（幂等）。
    pub fn close(&self) -> Result<(), SessionError> {
        let writer = {
            let mut inner = self.lock();
            if inner.closed {
                return Ok(());
            }
            inner.closed = true;
            inner.writer.clone()
        };
        self.log.release(&writer)
    }
}

impl Drop for SessionRecorder {
    fn drop(&mut self) {
        // 租约是资源：调用方忘了 close 也要还回去，否则这个会话就废了。
        let _ = self.close();
    }
}

/// 打开记录器时算出的历史基线。
struct Prepared {
    initial: SessionProjection,
    resumed_turn: bool,
    previous_state: Option<String>,
    previous_messages: Vec<String>,
}

fn message_json(state: &AgentState) -> Result<Vec<String>, SessionError> {
    state
        .context
        .iter()
        .map(|message| serde_json::to_string(message).map_err(Into::into))
        .collect()
}

fn state_error(error: crate::state::StateError) -> SessionError {
    SessionError::log(format!("state encoding failed: {error}"))
}

/// 生成原因 → 运行终态（对应 Java `SessionRecorder.outcome`）。
///
/// 这是**逻辑结果**，与响应式句柄的状态无关：挂起不是失败，达到迭代上限才是失败。
pub fn outcome(reason: Option<GenerateReason>) -> &'static str {
    match reason {
        None => "completed",
        Some(
            GenerateReason::ToolSuspended
            | GenerateReason::PermissionAsking
            | GenerateReason::ReasoningStopRequested
            | GenerateReason::ActingStopRequested
            | GenerateReason::MiddlewareStopRequested,
        ) => "suspended",
        Some(GenerateReason::Interrupted) => "interrupted",
        Some(GenerateReason::MaxIterations | GenerateReason::AllToolsDenied) => "failed",
        Some(_) => "completed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ContentBlock, ToolResultState};
    use crate::session::store::{InMemorySessionLogStore, SessionLogStore};
    use crate::session::SessionKey;

    fn log() -> Arc<dyn SessionLog> {
        InMemorySessionLogStore::new()
            .open(&SessionKey::new(Some("u"), "a", "s").unwrap(), None)
            .unwrap()
    }

    fn kinds(log: &Arc<dyn SessionLog>) -> Vec<String> {
        let head = log.head().unwrap();
        log.scan(0, head.seq)
            .unwrap()
            .into_iter()
            .map(|event| event.kind)
            .collect()
    }

    fn state() -> AgentState {
        AgentState::with_session_id("s")
    }

    #[test]
    fn run_lifecycle_is_recorded_in_order() {
        let log = log();
        let recorder = SessionRecorder::new(log.clone(), Some("t1".into()), "r1").unwrap();
        let mut state = state();
        let input = Msg::user("user", "你好");
        recorder.start(std::slice::from_ref(&input), &state).unwrap();

        state.context.push(input.clone());
        recorder.capture_state(&state, "input_applied").unwrap();
        state.context.push(Msg::assistant(
            "assistant",
            vec![
                ContentBlock::Text {
                    text: "我来查一下".into(),
                },
                ContentBlock::ToolUse {
                    id: "call1".into(),
                    name: "search".into(),
                    input: json!({"q": "rust"}),
                    state: None,
                    content: None,
                },
            ],
        ));
        recorder.step(1).unwrap();
        state
            .context
            .push(Msg::tool_result("assistant", "call1", "search", "结果", ToolResultState::Success));
        let answer = Msg::assistant("assistant", vec![ContentBlock::Text { text: "好了".into() }])
            .with_generate_reason(GenerateReason::ModelStop);
        state.context.push(answer.clone());
        recorder.result(&answer);
        recorder.finish(&state, "completed").unwrap();

        let kinds = kinds(&log);
        // 生命周期骨架
        assert_eq!(kinds[0], "migration/baseline");
        assert!(kinds.contains(&"run/start".to_string()));
        assert!(kinds.contains(&"turn/start".to_string()));
        assert!(kinds.contains(&"input/received".to_string()));
        // 输入并入状态时补一条 input/applied
        assert!(kinds.contains(&"input/applied".to_string()));
        assert!(kinds.contains(&"message/user".to_string()));
        assert!(kinds.contains(&"message/assistant".to_string()));
        // 每个 ToolUse 块展开成 tool/requested
        assert!(kinds.contains(&"tool/requested".to_string()));
        assert!(kinds.contains(&"tool/result".to_string()));
        assert!(kinds.contains(&"state/checkpoint".to_string()));
        assert!(kinds.contains(&"step/start".to_string()));
        assert!(kinds.contains(&"step/end".to_string()));
        assert!(kinds.contains(&"turn/output".to_string()));
        assert_eq!(kinds.last().unwrap(), "run/end");
        assert!(kinds.contains(&"turn/completed".to_string()));

        // 重放出来的状态与最终状态一致
        let projection = SessionProjection::read(log.as_ref()).unwrap();
        let restored = projection.restore().unwrap();
        assert_eq!(restored.context.len(), state.context.len());
        assert!(projection.uncertain_tool_calls().is_empty());
    }

    #[test]
    fn repeated_capture_without_change_writes_nothing() {
        let log = log();
        let recorder = SessionRecorder::new(log.clone(), Some("t1".into()), "r1").unwrap();
        let mut state = state();
        state.context.push(Msg::user("user", "你好"));
        recorder.checkpoint(&state, "first").unwrap();
        let before = kinds(&log).len();
        // 状态没变：第二次快照不产生任何事件
        recorder.checkpoint(&state, "second").unwrap();
        assert_eq!(kinds(&log).len(), before);
        recorder.close().unwrap();
    }

    #[test]
    fn rewritten_history_emits_context_replaced() {
        let log = log();
        let recorder = SessionRecorder::new(log.clone(), Some("t1".into()), "r1").unwrap();
        let mut state = state();
        state.context.push(Msg::user("user", "原始"));
        state.context.push(Msg::user("user", "第二条"));
        recorder.checkpoint(&state, "first").unwrap();
        // 改写已在日志里的第一条（不是纯追加）
        state.context[0] = Msg::user("user", "改写过");
        recorder.checkpoint(&state, "rewrite").unwrap();
        let kinds = kinds(&log);
        assert!(kinds.contains(&"context/replaced".to_string()));
        recorder.close().unwrap();
    }

    #[test]
    fn finish_maps_suspended_reason_without_output() {
        let log = log();
        let recorder = SessionRecorder::new(log.clone(), Some("t1".into()), "r1").unwrap();
        let mut state = state();
        let result = Msg::assistant("assistant", vec![ContentBlock::Text { text: "确认?".into() }])
            .with_generate_reason(GenerateReason::PermissionAsking);
        recorder.result(&result);
        state.context.push(result);
        recorder.finish(&state, "completed").unwrap();
        let kinds = kinds(&log);
        // 挂起不是完成：不得写 turn/output，终态是 suspended
        assert!(!kinds.contains(&"turn/output".to_string()));
        assert!(kinds.contains(&"turn/suspended".to_string()));
        assert_eq!(outcome(Some(GenerateReason::PermissionAsking)), "suspended");
        assert_eq!(outcome(Some(GenerateReason::MaxIterations)), "failed");
        assert_eq!(outcome(Some(GenerateReason::ModelStop)), "completed");
        assert_eq!(outcome(None), "completed");
    }

    #[test]
    fn recorder_refuses_reused_run_and_finished_turn() {
        let log = log();
        let recorder = SessionRecorder::new(log.clone(), Some("t1".into()), "r1").unwrap();
        let state = state();
        recorder.start(&[], &state).unwrap();
        recorder.finish(&state, "completed").unwrap();

        let reused_run = SessionRecorder::new(log.clone(), Some("t2".into()), "r1");
        assert!(reused_run.is_err());

        let finished_turn = SessionRecorder::new(log.clone(), Some("t1".into()), "r2");
        assert!(finished_turn.is_err());
    }

    #[test]
    fn recorder_refuses_unknown_tool_outcomes() {
        let log = log();
        let writer = log.acquire("w", Duration::from_secs(60)).unwrap();
        // 只有 dispatch，没有 action/end → 结果未知
        let event = SessionEvent::new(
            "e1".to_string(),
            1,
            1_000,
            "tool/dispatch",
            Some("old".into()),
            Some("t".into()),
            true,
            json!({"toolCallId": "call1", "name": "search"}).to_string(),
        )
        .unwrap();
        log.commit(&writer, "b1", 0, vec![event]).unwrap();
        log.release(&writer).unwrap();

        let error = SessionRecorder::new(log, Some("t9".into()), "r9").err().unwrap();
        assert!(error.to_string().contains("reconciliation"), "{error}");
    }

    #[test]
    fn closed_recorder_rejects_further_appends() {
        let log = log();
        let recorder = SessionRecorder::new(log, Some("t1".into()), "r1").unwrap();
        recorder.close().unwrap();
        let error = recorder.append("run/start", json!({})).unwrap_err();
        assert!(error.to_string().contains("closed"), "{error}");
    }
}
