//! 只读恢复视图（对应 Java `SessionProjection`）。
//!
//! 投影**只读**：不修复记录、不调用模型/工具、不写任何东西。它把已提交前缀重放成
//! 「一个状态快照 + 两个未决集合」：
//!
//! - `uncertain_tool_calls`：已发出 `tool/dispatch` 但没等到 `action/end` 的调用 id。
//!   只要这个集合非空，就**不允许**做管理性状态变更——外部副作用是否发生未知，
//!   框架绝不擅自重放或伪造结果。
//! - `active_runs`：`run/start` 之后没有 `run/end` 的执行，即被中断的运行。
//!
//! 重放顺序遵循「事实不可变」：每一步只看事件负载，不依赖当前内存状态，
//! 因此同一前缀在任何进程里重放结果都一致。

use serde_json::Value;

use super::events::{SessionEvent, SessionEventCodecRegistry, SessionEventTypes};
use super::journal::SessionLog;
use super::SessionError;
use crate::message::{ContentBlock, Msg, Role};
use crate::state::{AgentState, TaskVerification};

/// 一个已提交前缀上的恢复视图（对应 Java `record SessionProjection`）。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionProjection {
    as_of_seq: i64,
    state_json: Option<String>,
    uncertain_tool_calls: Vec<String>,
    active_runs: Vec<String>,
}

impl SessionProjection {
    /// 读取链头之前的全部已提交事件并投影（使用空扩展登记表）。
    pub fn read(log: &dyn SessionLog) -> Result<Self, SessionError> {
        Self::read_with(log, &SessionEventCodecRegistry::new())
    }

    /// 带扩展 codec 登记表的投影（扩展事件需要回放进状态时必须用它）。
    pub fn read_with(
        log: &dyn SessionLog,
        registry: &SessionEventCodecRegistry,
    ) -> Result<Self, SessionError> {
        let upper = log.head()?.seq;
        let mut fold = Fold::empty(0);
        fold.absorb(log.scan(0, upper)?, registry)?;
        fold.finish(registry)
    }

    /// 从上次投影快照继续增量重放（对应「检查点缓存」：不必每次从头扫日志）。
    ///
    /// 快照只携带基线与游标；`action/end` 等增量在本次重放中重新累积并补进状态。
    /// 日志比快照还短（被重建/截断）时回退全量重放。
    pub fn advance(log: &dyn SessionLog, from: &SessionSnapshot) -> Result<Self, SessionError> {
        Self::advance_with(log, &SessionEventCodecRegistry::new(), from)
    }

    /// 带扩展 codec 登记表的增量投影。
    pub fn advance_with(
        log: &dyn SessionLog,
        registry: &SessionEventCodecRegistry,
        from: &SessionSnapshot,
    ) -> Result<Self, SessionError> {
        let upper = log.head()?.seq;
        if upper < from.as_of_seq {
            return Self::read_with(log, registry);
        }
        let mut fold = Fold {
            cursor: from.as_of_seq,
            state_json: from.state_json.clone(),
            uncertain: from.uncertain_tool_calls.clone(),
            runs: from.active_runs.clone(),
            results: Vec::new(),
            verifications: Vec::new(),
            extensions: Vec::new(),
        };
        if upper > from.as_of_seq {
            fold.absorb(log.scan(from.as_of_seq, upper)?, registry)?;
        }
        fold.finish(registry)
    }

    /// 导出增量重放的起点快照（游标 + 基线 + 未决集合）。
    pub fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            as_of_seq: self.as_of_seq,
            state_json: self.state_json.clone(),
            uncertain_tool_calls: self.uncertain_tool_calls.clone(),
            active_runs: self.active_runs.clone(),
        }
    }

    /// 投影覆盖到的最后一条序号（0 表示没有任何已提交事件）。
    pub fn as_of_seq(&self) -> i64 {
        self.as_of_seq
    }

    /// 投影出的状态 JSON（`None` = 前缀里没有状态基线）。
    pub fn state_json(&self) -> Option<&str> {
        self.state_json.as_deref()
    }

    /// 重建状态（每次调用返回一份独立副本，调用方随便改）。
    pub fn restore(&self) -> Option<AgentState> {
        self.state_json
            .as_ref()
            .and_then(|json| AgentState::from_json_str(json).ok())
    }

    /// 结果未知的工具调用 id（已派发但未见 `action/end`）。
    pub fn uncertain_tool_calls(&self) -> &[String] {
        &self.uncertain_tool_calls
    }

    /// 仍未结束的运行标识（被中断的执行）。
    pub fn active_runs(&self) -> &[String] {
        &self.active_runs
    }
}

/// 增量重放的起点快照：上次投影的游标、状态基线与未决集合。
#[derive(Debug, Clone, Default)]
pub struct SessionSnapshot {
    /// 上次投影覆盖到的最后一条事件序号。
    pub as_of_seq: i64,
    /// 上次投影出的状态 JSON（`None` = 尚无基线）。
    pub state_json: Option<String>,
    /// 上次投影时的结果未知工具调用。
    pub uncertain_tool_calls: Vec<String>,
    /// 上次投影时仍未结束的运行。
    pub active_runs: Vec<String>,
}

/// 投影的中间累积态：`read`（全量）与 `advance`（增量）共用同一套吸收与收尾逻辑。
struct Fold {
    cursor: i64,
    state_json: Option<String>,
    uncertain: Vec<String>,
    runs: Vec<String>,
    results: Vec<(String, ContentBlock)>,
    verifications: Vec<TaskVerification>,
    extensions: Vec<SessionEvent>,
}

impl Fold {
    fn empty(cursor: i64) -> Self {
        Self {
            cursor,
            state_json: None,
            uncertain: Vec::new(),
            runs: Vec::new(),
            results: Vec::new(),
            verifications: Vec::new(),
            extensions: Vec::new(),
        }
    }

    /// 吸收一段事件（插入序去重：Java 用 LinkedHashSet/LinkedHashMap，
    /// 顺序影响「恢复出的消息顺序」）。
    fn absorb(
        &mut self,
        events: Vec<SessionEvent>,
        registry: &SessionEventCodecRegistry,
    ) -> Result<(), SessionError> {
        for event in events {
            // 未知 required 事件必须在这里失败：否则状态会「少一段」却看起来正常。
            registry.validate(&event)?;
            let data = event.data()?;
            match event.kind.as_str() {
                // 新的状态基线：之前累积的增量全部作废。
                "state/checkpoint" | "migration/baseline" | "state/restored" => {
                    self.state_json = data
                        .get("stateJson")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    self.results.clear();
                    self.verifications.clear();
                    self.extensions.clear();
                }
                "verification/result" => {
                    if let Some(value) = data.get("verification") {
                        if !value.is_null() {
                            self.verifications.push(serde_json::from_value(value.clone())?);
                        }
                    }
                }
                "tool/dispatch" => {
                    if let Some(id) = data.get("toolCallId").and_then(Value::as_str) {
                        push_unique(&mut self.uncertain, id);
                    }
                }
                "action/end" => {
                    if let Some(value) = data.get("result") {
                        if !value.is_null() {
                            let id = data
                                .get("toolCallId")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            self.uncertain.retain(|existing| existing != &id);
                            let block = decode_tool_result(value)?;
                            self.results.retain(|(existing, _)| existing != &id);
                            self.results.push((id, block));
                        }
                    }
                }
                "recovery/applied" => {
                    if let Some(Value::Array(ids)) = data.get("interruptedRuns") {
                        self.runs.retain(|run| {
                            !ids.iter().any(|id| id.as_str() == Some(run.as_str()))
                        });
                    }
                    if let Some(Value::Array(ids)) = data.get("resolvedToolCallIds") {
                        self.uncertain.retain(|id| {
                            !ids
                                .iter()
                                .any(|resolved| resolved.as_str() == Some(id.as_str()))
                        });
                    }
                }
                "run/start" => {
                    if let Some(run) = &event.execution_run_id {
                        push_unique(&mut self.runs, run);
                    }
                }
                "run/end" => {
                    if let Some(run) = &event.execution_run_id {
                        self.runs.retain(|active| active != run);
                    }
                }
                other => {
                    // 内置类型没有增量语义的（如 model/chunk）不参与投影；
                    // 非内置的扩展事件留给 codec 回放。
                    if !SessionEventTypes::is_builtin(other) {
                        self.extensions.push(event.clone());
                    }
                }
            }
            self.cursor = event.seq;
        }
        Ok(())
    }

    /// 收尾：有基线且存在需要补进状态的增量时，只在此处构造一次 AgentState。
    fn finish(
        mut self,
        registry: &SessionEventCodecRegistry,
    ) -> Result<SessionProjection, SessionError> {
        if let Some(json) = &self.state_json {
            if !self.results.is_empty()
                || !self.verifications.is_empty()
                || !self.extensions.is_empty()
            {
                let mut state = AgentState::from_json_str(json).map_err(|error| {
                    SessionError::log(format!("unreadable state baseline: {error}"))
                })?;
                let existing: Vec<String> = state
                    .context
                    .iter()
                    .flat_map(|message| message.content.iter())
                    .filter_map(|block| match block {
                        ContentBlock::ToolResult { id, .. } => Some(id.clone()),
                        _ => None,
                    })
                    .collect();
                for (id, block) in &self.results {
                    let suspended = matches!(
                        block,
                        ContentBlock::ToolResult {
                            state: crate::message::ToolResultState::Suspended,
                            ..
                        }
                    );
                    if existing.contains(id) || suspended {
                        continue;
                    }
                    // 恢复出的结果消息用 `recovered_<id>` 作消息 id：既与原始消息区分，
                    // 又保证同一前缀重复投影得到同样的 id（幂等）。
                    let mut message = Msg::new(Role::Tool, "", vec![block.clone()]);
                    message.id = format!("recovered_{id}");
                    state.context.push(message);
                }
                for report in &self.verifications {
                    let revision = state.tasks_context.revision();
                    state
                        .tasks_context
                        .record_verification(report.clone(), revision)
                        .map_err(|error| {
                            SessionError::log(format!("verification replay failed: {error}"))
                        })?;
                }
                for extension in &self.extensions {
                    registry.apply_extension(&mut state, extension)?;
                }
                self.state_json = Some(
                    state
                        .to_json()
                        .map_err(|error| SessionError::log(format!("state encoding failed: {error}")))?,
                );
            }
        }

        Ok(SessionProjection {
            as_of_seq: self.cursor,
            state_json: self.state_json,
            uncertain_tool_calls: self.uncertain,
            active_runs: self.runs,
        })
    }
}


fn push_unique(target: &mut Vec<String>, value: &str) {
    if !target.iter().any(|existing| existing == value) {
        target.push(value.to_string());
    }
}

/// 把 `action/end` 的 `result` 负载解成工具结果块。
fn decode_tool_result(value: &Value) -> Result<ContentBlock, SessionError> {
    let mut value = value.clone();
    // 早期原生负载没写多态判别字段，补一个再解。
    if let Value::Object(map) = &mut value {
        map.entry("type")
            .or_insert_with(|| Value::from("tool_result"));
    }
    let block: ContentBlock = serde_json::from_value(value)?;
    if !matches!(block, ContentBlock::ToolResult { .. }) {
        return Err(SessionError::log(
            "action result is not a tool result block",
        ));
    }
    Ok(block)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Role as MsgRole, ToolResultState};
    use crate::session::store::{InMemorySessionLogStore, SessionLogStore};
    use crate::session::{SessionKey, Writer};
    use std::sync::Arc;

    fn log() -> Arc<dyn SessionLog> {
        let store = InMemorySessionLogStore::new();
        store
            .open(&SessionKey::new(Some("u"), "a", "s").unwrap(), None)
            .unwrap()
    }

    fn commit(log: &Arc<dyn SessionLog>, writer: &Writer, expected: i64, kind: &str, payload: Value) {
        let event = SessionEvent::new(
            format!("e{expected}-{kind}"),
            expected + 1,
            1_000,
            kind,
            None,
            None,
            true,
            serde_json::to_string(&payload).unwrap(),
        )
        .unwrap();
        log.commit(writer, &format!("b{expected}-{kind}"), expected, vec![event])
            .unwrap();
    }

    fn baseline() -> AgentState {
        let mut state = AgentState::with_session_id("s").with_user_id(Some("u".into()));
        state.context.push(Msg::user("user", "你好"));
        state
    }

    #[test]
    fn empty_log_yields_empty_projection() {
        let log = log();
        let projection = SessionProjection::read(log.as_ref()).unwrap();
        assert_eq!(projection.as_of_seq(), 0);
        assert!(projection.restore().is_none());
        assert!(projection.uncertain_tool_calls().is_empty());
        assert!(projection.active_runs().is_empty());
    }

    #[test]
    fn checkpoint_roundtrips_state() {
        let log = log();
        let writer = log.acquire("w", std::time::Duration::from_secs(60)).unwrap();
        let state = baseline();
        commit(
            &log,
            &writer,
            0,
            "state/checkpoint",
            serde_json::json!({
                "stateJson": state.to_json().unwrap(),
                "projectorVersion": 1,
                "reason": "test",
            }),
        );
        let projection = SessionProjection::read(log.as_ref()).unwrap();
        assert_eq!(projection.as_of_seq(), 1);
        let restored = projection.restore().unwrap();
        assert_eq!(restored.session_id, "s");
        assert_eq!(restored.context.len(), 1);
        assert_eq!(restored.context[0].text(), "你好");
    }

    #[test]
    fn dispatch_without_action_end_is_uncertain() {
        let log = log();
        let writer = log.acquire("w", std::time::Duration::from_secs(60)).unwrap();
        let up = baseline().to_json().unwrap();
        commit(
            &log,
            &writer,
            0,
            "state/checkpoint",
            serde_json::json!({ "stateJson": up }),
        );
        commit(
            &log,
            &writer,
            1,
            "tool/dispatch",
            serde_json::json!({ "toolCallId": "call-1" }),
        );
        let projection = SessionProjection::read(log.as_ref()).unwrap();
        assert_eq!(projection.uncertain_tool_calls(), ["call-1".to_string()]);
        assert!(projection.active_runs().is_empty());
    }

    #[test]
    fn action_end_resolves_and_rehydrates_result_message() {
        let log = log();
        let writer = log.acquire("w", std::time::Duration::from_secs(60)).unwrap();
        let up = baseline().to_json().unwrap();
        commit(
            &log,
            &writer,
            0,
            "state/checkpoint",
            serde_json::json!({ "stateJson": up }),
        );
        commit(
            &log,
            &writer,
            1,
            "tool/dispatch",
            serde_json::json!({ "toolCallId": "call-1" }),
        );
        // 早期负载没有 type 判别字段，投影要能补上。
        commit(
            &log,
            &writer,
            2,
            "action/end",
            serde_json::json!({
                "toolCallId": "call-1",
                "result": {
                    "id": "call-1",
                    "name": "search",
                    "output": [{ "type": "text", "text": "结果" }],
                    "state": "success"
                }
            }),
        );
        let projection = SessionProjection::read(log.as_ref()).unwrap();
        assert!(projection.uncertain_tool_calls().is_empty());
        let restored = projection.restore().unwrap();
        let recovered = restored
            .context
            .iter()
            .find(|message| message.id == "recovered_call-1")
            .expect("恢复出的工具结果消息");
        assert_eq!(recovered.role, MsgRole::Tool);
        // `Msg::text()` 只看顶层文本块，工具结果要进块里取（与 Java `getTextContent` 一致）。
        match &recovered.content[0] {
            ContentBlock::ToolResult {
                id, output, state, ..
            } => {
                assert_eq!(id, "call-1");
                assert_eq!(output[0], ContentBlock::Text { text: "结果".into() });
                assert_eq!(*state, ToolResultState::Success);
            }
            other => panic!("unexpected block: {other:?}"),
        }
    }

    #[test]
    fn suspended_and_existing_results_are_not_duplicated() {
        let log = log();
        let writer = log.acquire("w", std::time::Duration::from_secs(60)).unwrap();
        let mut state = baseline();
        // 状态里已经有 call-1 的结果。
        let mut result = Msg::new(
            MsgRole::Tool,
            "",
            vec![ContentBlock::tool_result_text("call-1", "search", "已有结果")],
        );
        result.id = "already".into();
        state.context.push(result);
        let up = state.to_json().unwrap();
        commit(
            &log,
            &writer,
            0,
            "state/checkpoint",
            serde_json::json!({ "stateJson": up }),
        );
        commit(
            &log,
            &writer,
            1,
            "action/end",
            serde_json::json!({
                "toolCallId": "call-1",
                "result": { "type": "tool_result", "id": "call-1", "name": "search",
                            "output": [{ "type": "text", "text": "重复结果" }], "state": "success" }
            }),
        );
        commit(
            &log,
            &writer,
            2,
            "action/end",
            serde_json::json!({
                "toolCallId": "call-2",
                "result": { "type": "tool_result", "id": "call-2", "name": "search",
                            "output": [], "state": "suspended" }
            }),
        );
        let projection = SessionProjection::read(log.as_ref()).unwrap();
        let restored = projection.restore().unwrap();
        assert_eq!(restored.context.len(), 2, "两条都不应追加");
        match &restored.context[1].content[0] {
            ContentBlock::ToolResult { output, .. } => {
                assert_eq!(output[0], ContentBlock::Text { text: "已有结果".into() })
            }
            other => panic!("unexpected block: {other:?}"),
        }
    }

    #[test]
    fn run_lifecycle_tracks_active_runs_and_recovery_clears_them() {
        let log = log();
        let writer = log.acquire("w", std::time::Duration::from_secs(60)).unwrap();
        let event = |seq: i64, kind: &str, run: &str, payload: Value| {
            SessionEvent::new(
                format!("e{seq}"),
                seq,
                1_000,
                kind,
                Some(run.to_string()),
                Some("turn-1".to_string()),
                true,
                serde_json::to_string(&payload).unwrap(),
            )
            .unwrap()
        };
        log.commit(
            &writer,
            "b1",
            0,
            vec![
                event(1, "run/start", "run-1", serde_json::json!({})),
                event(2, "run/start", "run-2", serde_json::json!({})),
                event(3, "run/end", "run-1", serde_json::json!({ "status": "completed" })),
            ],
        )
        .unwrap();
        let projection = SessionProjection::read(log.as_ref()).unwrap();
        assert_eq!(projection.active_runs(), ["run-2".to_string()]);

        log.commit(
            &writer,
            "b2",
            3,
            vec![event(
                4,
                "recovery/applied",
                "run-3",
                serde_json::json!({ "interruptedRuns": ["run-2"], "reason": "cancelled" }),
            )],
        )
        .unwrap();
        let projection = SessionProjection::read(log.as_ref()).unwrap();
        assert!(projection.active_runs().is_empty());
    }

    #[test]
    fn new_baseline_discards_accumulated_deltas() {
        let log = log();
        let writer = log.acquire("w", std::time::Duration::from_secs(60)).unwrap();
        let first = baseline().to_json().unwrap();
        commit(
            &log,
            &writer,
            0,
            "state/checkpoint",
            serde_json::json!({ "stateJson": first }),
        );
        commit(
            &log,
            &writer,
            1,
            "action/end",
            serde_json::json!({
                "toolCallId": "call-1",
                "result": { "id": "call-1", "name": "search",
                            "output": [{ "type": "text", "text": "旧结果" }], "state": "success" }
            }),
        );
        // 新基线（state/restored）之后，旧增量必须被丢弃。
        let second = baseline().to_json().unwrap();
        commit(
            &log,
            &writer,
            2,
            "state/restored",
            serde_json::json!({ "stateJson": second }),
        );
        let projection = SessionProjection::read(log.as_ref()).unwrap();
        let restored = projection.restore().unwrap();
        assert_eq!(restored.context.len(), 1);
        assert!(!restored
            .context
            .iter()
            .any(|message| message.id == "recovered_call-1"));
    }

    #[test]
    fn unknown_required_extension_fails_closed() {
        let log = log();
        let writer = log.acquire("w", std::time::Duration::from_secs(60)).unwrap();
        commit(
            &log,
            &writer,
            0,
            "plugin/mystery",
            serde_json::json!({ "x": 1 }),
        );
        let error = SessionProjection::read(log.as_ref()).unwrap_err();
        assert!(matches!(error, SessionError::Log(_)));

        // 登记 codec 后即可投影，并把扩展回放进状态。
        let registry = SessionEventCodecRegistry::new();
        registry
            .register(
                "plugin/mystery",
                crate::session::SessionEventCodec::new(
                    |_| Ok(()),
                    |state, value| {
                        state.cur_iter += value["x"].as_i64().unwrap_or(0) as i32;
                        Ok(())
                    },
                ),
            )
            .unwrap();
        let projection = SessionProjection::read_with(log.as_ref(), &registry).unwrap();
        assert_eq!(projection.as_of_seq(), 1);
        // 没有基线 → 不回放，也就没有状态。
        assert!(projection.restore().is_none());
    }

    #[test]
    fn verification_results_are_replayed_into_task_context() {
        use crate::state::{
            EvidenceBinding, TaskAuthority, TaskContextState, TaskDecision, TaskRequirementKind,
            TaskRequirementStatus, TaskScope, TaskVerification, VerificationOutcome,
        };

        let log = log();
        let writer = log.acquire("w", std::time::Duration::from_secs(60)).unwrap();
        let mut state = baseline();
        state.tasks_context = TaskContextState::default();
        state
            .tasks_context
            .begin_task(TaskScope::new("t-1", "实现投影", "user:req").unwrap(), 0)
            .unwrap();
        let requirement = state
            .tasks_context
            .propose(
                TaskRequirementKind::AcceptanceCriterion,
                "重放验证记录",
                "model:1",
            )
            .unwrap();
        state
            .tasks_context
            .decide(
                &requirement.id,
                TaskRequirementStatus::Confirmed,
                TaskDecision {
                    authority: TaskAuthority::User,
                    source_ref: "user:ok".into(),
                },
                state.tasks_context.revision(),
            )
            .unwrap();
        state
            .tasks_context
            .set_subject_version("commit-abc", state.tasks_context.revision())
            .unwrap();
        let binding = state.tasks_context.evidence_binding().unwrap();
        let up = state.to_json().unwrap();
        commit(
            &log,
            &writer,
            0,
            "state/checkpoint",
            serde_json::json!({ "stateJson": up }),
        );
        let verification = TaskVerification {
            id: "v-1".into(),
            requirement_id: requirement.id.clone(),
            action_id: "a-1".into(),
            verifier_id: "tester".into(),
            outcome: VerificationOutcome::Passed,
            reason: "测试通过".into(),
            binding: EvidenceBinding {
                task_id: binding.task_id.clone(),
                contract_version: binding.contract_version,
                subject_version: binding.subject_version.clone(),
            },
            recorded_at: 1_000,
        };
        commit(
            &log,
            &writer,
            1,
            "verification/result",
            serde_json::json!({ "verification": verification }),
        );
        let projection = SessionProjection::read(log.as_ref()).unwrap();
        let restored = projection.restore().unwrap();
        assert_eq!(restored.tasks_context.verifications().len(), 1);
        assert_eq!(restored.tasks_context.verifications()[0].id, "v-1");
    }
}
