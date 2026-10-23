//! 执行侧的收件箱消费者（对应 Java `SessionExecution`）。
//!
//! 收件箱里的命令由外部投递，但**能不能执行、执行什么**必须由执行侧在持有写者租约
//! 时判定——否则两条并发命令可以同时通过校验，把同一个回合推进两次。
//!
//! 三类命令语义不同：
//!
//! | 命令 | 前置条件 | 并入对话的内容 |
//! |------|----------|----------------|
//! | `submit` | 最新回合已终结、无未决交互 | 命令自带的输入消息 |
//! | `resume` | 必须是最新**未终结**回合 | 该回合的原始输入，但要滤掉确认答复 |
//! | `respond` | 每个 `requestId` 仍在未决集合且属本回合 | 命令自带的输入消息 |
//!
//! `resume` 的过滤条件是这里最容易被忽略的一点：确认结果改的是工具/权限状态，
//! 它不是聊天内容。若原样重放，续跑时会凭空多出一条用户消息。

use std::sync::Arc;

use serde_json::json;

use super::inbox::{Command, SessionInbox};
use super::interactions::PendingInteractions;
use super::journal::SessionLog;
use super::recorder::SessionRecorder;
use super::turns::SessionTurns;
use super::SessionError;
use crate::message::{metadata_keys, Msg};
use crate::state::AgentState;

/// 一条收件箱命令在一次运行里的消费过程。
pub struct SessionExecution {
    log: Arc<dyn SessionLog>,
    command: Command,
    inputs: Vec<Msg>,
    opened: bool,
}

impl SessionExecution {
    /// 绑定一条待执行命令。
    pub fn new(log: Arc<dyn SessionLog>, command: Command) -> Self {
        Self {
            log,
            command,
            inputs: Vec::new(),
            opened: false,
        }
    }

    /// 本次执行真的要并入对话的输入。
    pub fn inputs(&self) -> &[Msg] {
        &self.inputs
    }

    /// 持有的命令。
    pub fn command(&self) -> &Command {
        &self.command
    }

    /// 校验并开启回合：`inbox/started` + `inbox/opened`。
    ///
    /// 校验失败时命令仍留在收件箱里（拒绝要由外部显式做），但本次运行不会开始。
    pub fn prepare(&mut self, recorder: &SessionRecorder) -> Result<(), SessionError> {
        let view = SessionTurns::read(self.log.as_ref())?;
        let turn_id = self
            .command
            .turn_id
            .clone()
            .ok_or_else(|| SessionError::invalid("command has no turn id"))?;
        if view
            .started_commands()
            .iter()
            .any(|id| id == &self.command.id)
        {
            return Err(SessionError::invalid("command already executed"));
        }
        let latest = view.turns().last();
        let pending = PendingInteractions::read(self.log.as_ref())?;

        match self.command.kind.as_str() {
            "submit" => {
                if latest.map(|turn| !turn.ended()).unwrap_or(false) {
                    return Err(SessionError::invalid(
                        "continue the unfinished task before starting another task",
                    ));
                }
                if !pending.is_empty() {
                    return Err(SessionError::invalid(
                        "answer pending interactions first",
                    ));
                }
                self.inputs = self.command.messages.clone();
            }
            "resume" => {
                let latest = latest.ok_or_else(|| {
                    SessionError::invalid("only the latest unfinished task can continue")
                })?;
                if latest.turn_id != turn_id || latest.ended() {
                    return Err(SessionError::invalid(
                        "only the latest unfinished task can continue",
                    ));
                }
                if !pending.is_empty() {
                    return Err(SessionError::invalid(
                        "answer pending interactions first",
                    ));
                }
                // 确认答复不是聊天内容：续跑时不得重放成用户消息。
                self.inputs = view
                    .inputs(&turn_id)
                    .iter()
                    .filter(|message| !has_confirm_results(message))
                    .cloned()
                    .collect();
            }
            "respond" => {
                let latest = latest.ok_or_else(|| {
                    SessionError::invalid("only the latest unfinished task can continue")
                })?;
                if latest.turn_id != turn_id || latest.ended() {
                    return Err(SessionError::invalid(
                        "only the latest unfinished task can continue",
                    ));
                }
                for id in &self.command.request_ids {
                    match pending.get(id) {
                        Some(request) if request.turn_id.as_deref() == Some(turn_id.as_str()) => {}
                        _ => {
                            return Err(SessionError::invalid(format!(
                                "interaction is no longer pending: {id}"
                            )))
                        }
                    }
                }
                self.inputs = self.command.messages.clone();
            }
            other => {
                return Err(SessionError::invalid(format!(
                    "command kind does not drive an execution: {other}"
                )))
            }
        }

        recorder.append(
            "inbox/started",
            json!({ "commandId": self.command.id, "kind": self.command.kind }),
        )?;
        let run_id = recorder.run_id();
        let inbox = SessionInbox::new(self.log.as_ref())?;
        inbox.locked(|tx| tx.open(&turn_id, &run_id))?;
        self.opened = true;
        Ok(())
    }

    /// 每步开始前注入 `inject` / `steer` 命令携带的消息。
    ///
    /// 注入与「标记命令已消费」必须一起做：先提交消息再标消费，重试时靠消息 id 去重。
    pub fn before_step(
        &self,
        recorder: &SessionRecorder,
        state: &mut AgentState,
    ) -> Result<(), SessionError> {
        let inbox = SessionInbox::new(self.log.as_ref())?;
        let applied = SessionTurns::read(self.log.as_ref())?.applied_commands().to_vec();
        let mut known: Vec<String> = recorder.accepted_message_ids().into_iter().collect();
        known.extend(state.context.iter().map(|message| message.id.clone()));

        let accepted: Vec<String> = inbox.locked(|tx| {
            let snapshot = tx.snapshot()?;
            let mut accepted = Vec::new();
            for command in &snapshot.commands {
                if snapshot.rejected_reason(&command.id).is_some()
                    || applied.iter().any(|id| id == &command.id)
                    || !matches!(command.kind.as_str(), "inject" | "steer")
                {
                    continue;
                }
                if command.kind == "steer"
                    && command.turn_id.as_deref() != self.command.turn_id.as_deref()
                {
                    continue;
                }
                for message in &command.messages {
                    if known.iter().any(|id| id == &message.id) {
                        continue;
                    }
                    known.push(message.id.clone());
                    state.context.push(message.clone());
                }
                accepted.push(command.id.clone());
            }
            Ok(accepted)
        })?;

        if !accepted.is_empty() {
            recorder.checkpoint(state, "session_input")?;
            recorder.record_now("inbox/applied", json!({ "commandIds": accepted }))?;
        }
        Ok(())
    }

    /// 回合结算：还有未消费的 `steer` 就继续跑，否则关闭回合。
    ///
    /// 与「接受转向指令」串行化，避免刚接受就被关掉的丢指令窗口。
    pub fn continue_or_close(&mut self, recorder: &SessionRecorder) -> Result<bool, SessionError> {
        let inbox = SessionInbox::new(self.log.as_ref())?;
        let applied = SessionTurns::read(self.log.as_ref())?.applied_commands().to_vec();
        let pending = inbox.locked(|tx| {
            let snapshot = tx.snapshot()?;
            let pending = snapshot.commands.iter().any(|command| {
                command.kind == "steer"
                    && command.turn_id == self.command.turn_id
                    && !applied.iter().any(|id| id == &command.id)
                    && snapshot.rejected_reason(&command.id).is_none()
            });
            if !pending {
                self.close_locked(tx, recorder)?;
            }
            Ok(pending)
        })?;
        Ok(pending)
    }

    /// 关闭回合（幂等）。
    pub fn close(&mut self, recorder: &SessionRecorder) -> Result<(), SessionError> {
        if !self.opened {
            return Ok(());
        }
        let inbox = SessionInbox::new(self.log.as_ref())?;
        inbox.locked(|tx| self.close_locked(tx, recorder))
    }

    fn close_locked(
        &mut self,
        tx: &super::inbox::Transaction<'_>,
        recorder: &SessionRecorder,
    ) -> Result<(), SessionError> {
        if !self.opened {
            return Ok(());
        }
        let turn_id = self.command.turn_id.clone().unwrap_or_default();
        tx.close(&turn_id, &recorder.run_id())?;
        self.opened = false;
        Ok(())
    }
}

fn has_confirm_results(message: &Msg) -> bool {
    message
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.as_object())
        .map(|object| object.contains_key(metadata_keys::CONFIRM_RESULTS))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ToolResultState;
    use crate::session::store::{InMemorySessionLogStore, SessionLogStore};
    use crate::session::SessionKey;
    use serde_json::json;

    fn log() -> Arc<dyn SessionLog> {
        InMemorySessionLogStore::new()
            .open(&SessionKey::new(Some("u"), "a", "s").unwrap(), None)
            .unwrap()
    }

    fn state() -> AgentState {
        AgentState::with_session_id("s")
    }

    fn accept(log: &Arc<dyn SessionLog>, command: Command) -> Command {
        let inbox = SessionInbox::new(log.as_ref()).unwrap();
        inbox.locked(|tx| tx.accept(command)).unwrap()
    }

    fn snapshot(log: &Arc<dyn SessionLog>) -> super::super::inbox::Snapshot {
        SessionInbox::new(log.as_ref()).unwrap().snapshot().unwrap()
    }

    #[test]
    fn submit_opens_the_turn_and_carries_its_inputs() {
        let log = log();
        let command = accept(
            &log,
            Command::new("c1", "submit")
                .with_turn("t1")
                .with_messages(vec![Msg::user("user", "你好")]),
        );
        let recorder = SessionRecorder::new(log.clone(), Some("t1".into()), "r1").unwrap();
        let mut execution = SessionExecution::new(log.clone(), command);
        execution.prepare(&recorder).unwrap();
        assert_eq!(execution.inputs().len(), 1);
        assert_eq!(snapshot(&log).turn_id.as_deref(), Some("t1"));

        execution.close(&recorder).unwrap();
        assert!(snapshot(&log).turn_id.is_none());
        recorder.close().unwrap();
    }

    #[test]
    fn submit_is_rejected_while_a_turn_is_unfinished() {
        let log = log();
        let recorder = SessionRecorder::new(log.clone(), Some("t1".into()), "r1").unwrap();
        recorder.start(&[], &state()).unwrap();

        let command = accept(
            &log,
            Command::new("c2", "submit")
                .with_turn("t2")
                .with_messages(vec![Msg::user("user", "另一个任务")]),
        );
        let mut execution = SessionExecution::new(log.clone(), command);
        let error = execution.prepare(&recorder).err().unwrap();
        assert!(error.to_string().contains("unfinished"), "{error}");
        recorder.close().unwrap();
    }

    #[test]
    fn resume_drops_confirmation_messages() {
        let log = log();
        let input = Msg::user("user", "原始任务");
        let confirm = Msg::user("user", "确认").with_metadata(json!({
            metadata_keys::CONFIRM_RESULTS: json!({"req1": true}),
        }));
        let recorder = SessionRecorder::new(log.clone(), Some("t1".into()), "r1").unwrap();
        recorder.start(&[input, confirm], &state()).unwrap();
        recorder.close().unwrap();

        let command = accept(&log, Command::new("c3", "resume").with_turn("t1"));
        let recorder = SessionRecorder::new(log.clone(), Some("t1".into()), "r2").unwrap();
        let mut execution = SessionExecution::new(log.clone(), command);
        execution.prepare(&recorder).unwrap();
        // 确认答复不是聊天内容，续跑时只重放真正的输入
        assert_eq!(execution.inputs().len(), 1);
        assert_eq!(execution.inputs()[0].text(), "原始任务");
        recorder.close().unwrap();
    }

    #[test]
    fn respond_requires_still_pending_requests() {
        let log = log();
        let recorder = SessionRecorder::new(log.clone(), Some("t1".into()), "r1").unwrap();
        recorder.start(&[], &state()).unwrap();

        let command = accept(
            &log,
            Command::new("c4", "respond")
                .with_turn("t1")
                .with_request_ids(vec!["req-x".into()])
                .with_messages(vec![Msg::tool_result(
                    "tool",
                    "call1",
                    "search",
                    "外部结果",
                    ToolResultState::Success,
                )]),
        );
        let mut execution = SessionExecution::new(log.clone(), command);
        let error = execution.prepare(&recorder).err().unwrap();
        assert!(error.to_string().contains("no longer pending"), "{error}");
        recorder.close().unwrap();
    }

    #[test]
    fn before_step_injects_steering_messages_once() {
        let log = log();
        let recorder = SessionRecorder::new(log.clone(), Some("t1".into()), "r1").unwrap();
        recorder.start(&[], &state()).unwrap();
        accept(
            &log,
            Command::new("s1", "steer")
                .with_turn("t1")
                .with_messages(vec![Msg::user("user", "换个方向")]),
        );

        let command = accept(&log, Command::new("c5", "submit").with_turn("t1"));
        let execution = SessionExecution::new(log.clone(), command);
        let mut state = state();
        execution.before_step(&recorder, &mut state).unwrap();
        assert_eq!(state.context.len(), 1);
        assert_eq!(state.context[0].text(), "换个方向");

        // 命令已消费：第二次注入不会重复
        execution.before_step(&recorder, &mut state).unwrap();
        assert_eq!(state.context.len(), 1);
        recorder.close().unwrap();
    }

    #[test]
    fn continue_or_close_waits_for_unapplied_steering() {
        let log = log();
        let command = accept(&log, Command::new("c6", "submit").with_turn("t1"));
        let recorder = SessionRecorder::new(log.clone(), Some("t1".into()), "r1").unwrap();
        let mut execution = SessionExecution::new(log.clone(), command);
        execution.prepare(&recorder).unwrap();

        // 没有转向指令 → 直接关回合
        assert!(!execution.continue_or_close(&recorder).unwrap());
        assert!(snapshot(&log).turn_id.is_none());
        // 释放写者租约，才轮得到下一个运行
        recorder.close().unwrap();

        // 再来一个回合，这回有未消费的 steer → 继续跑
        let command = accept(&log, Command::new("c7", "submit").with_turn("t2"));
        let recorder = SessionRecorder::new(log.clone(), Some("t2".into()), "r2").unwrap();
        let mut execution = SessionExecution::new(log.clone(), command);
        execution.prepare(&recorder).unwrap();
        accept(
            &log,
            Command::new("s2", "steer")
                .with_turn("t2")
                .with_messages(vec![Msg::user("user", "追加要求")]),
        );
        assert!(execution.continue_or_close(&recorder).unwrap());
        assert_eq!(snapshot(&log).turn_id.as_deref(), Some("t2"));
        recorder.close().unwrap();
    }
}
