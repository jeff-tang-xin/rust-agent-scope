//! 逻辑回合视图（对应 Java `SessionTurns`）。
//!
//! 逻辑回合（`turn_id`）与执行运行（`run_id`）**不是一回事**：一次用户输入引发
//! 的完整处理是一个回合，中途可能经历多次运行（被打断、恢复、续跑）。所以
//! 「这一轮是谁开始的、现在到哪了、有没有回执」必须从事件日志里重放得出，
//! 不能靠内存里的当前运行推断。

use serde_json::Value;

use super::events::SessionEvent;
use super::journal::SessionLog;
use super::SessionError;
use crate::message::Msg;

/// 一个逻辑回合的当前视图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    /// 逻辑回合标识。
    pub turn_id: String,
    /// 最近一次承接它的运行标识。
    pub run_id: Option<String>,
    /// 回合状态（`running` / `completed` / `cancelled` …）。
    pub status: String,
}

impl Turn {
    /// 回合是否已终结（只有终结的回合才允许 `turn/cancelled` 之类只写一次的事）。
    pub fn ended(&self) -> bool {
        self.status != "running"
    }

    /// 回合是否属于某次运行。
    pub fn belongs_to(&self, run_id: &str) -> bool {
        self.run_id.as_deref() == Some(run_id)
    }
}

/// 已提交前缀上的回合视图。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionTurns {
    turns: Vec<Turn>,
    inputs: Vec<(String, Vec<Msg>)>,
    started_commands: Vec<String>,
    applied_commands: Vec<String>,
}

impl SessionTurns {
    /// 重放已提交前缀得到回合视图。
    pub fn read(log: &dyn SessionLog) -> Result<Self, SessionError> {
        let upper = log.head()?.seq;
        let mut view = Self::default();
        let mut cursor = 0;
        while cursor < upper {
            let batch = log.read_after(cursor, 64)?;
            if batch.is_empty() {
                return Err(SessionError::log("missing turn prefix"));
            }
            for event in batch {
                if event.seq > upper {
                    break;
                }
                view.apply(&event)?;
                cursor = event.seq;
            }
        }
        Ok(view)
    }

    fn apply(&mut self, event: &SessionEvent) -> Result<(), SessionError> {
        let data = event.data()?;
        match event.kind.as_str() {
            // 换基线：之前的回合归属作废。
            "state/restored" => {
                self.turns.clear();
                self.inputs.clear();
            }
            "turn/cancelled" => {
                if let Some(turn_id) = &event.turn_id {
                    self.upsert(turn_id, event.execution_run_id.clone(), "cancelled");
                }
            }
            "run/start" => {
                if let Some(turn_id) = &event.turn_id {
                    self.upsert(turn_id, event.execution_run_id.clone(), "running");
                }
            }
            "run/end" => {
                if let Some(turn_id) = &event.turn_id {
                    let status = data
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    self.upsert(turn_id, event.execution_run_id.clone(), status);
                }
            }
            "input/received" => {
                if let Some(turn_id) = &event.turn_id {
                    if let Some(raw) = data.get("messages") {
                        if !raw.is_null() {
                            let messages: Vec<Msg> = serde_json::from_value(raw.clone())?;
                            let slot = self.inputs_for(turn_id);
                            for message in messages {
                                if slot.iter().any(|existing| existing.id == message.id) {
                                    continue;
                                }
                                slot.push(message);
                            }
                        }
                    }
                }
            }
            "inbox/started" => {
                if let Some(id) = data.get("commandId").and_then(Value::as_str) {
                    self.started_commands.push(id.to_string());
                }
            }
            "inbox/applied" => {
                if let Some(Value::Array(ids)) = data.get("commandIds") {
                    for id in ids {
                        if let Some(id) = id.as_str() {
                            self.applied_commands.push(id.to_string());
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// 插入或更新回合（保首次出现位置）。
    fn upsert(&mut self, turn_id: &str, run_id: Option<String>, status: &str) {
        match self.turns.iter_mut().find(|turn| turn.turn_id == turn_id) {
            Some(turn) => {
                if run_id.is_some() {
                    turn.run_id = run_id;
                }
                turn.status = status.to_string();
            }
            None => self.turns.push(Turn {
                turn_id: turn_id.to_string(),
                run_id,
                status: status.to_string(),
            }),
        }
    }

    fn inputs_for(&mut self, turn_id: &str) -> &mut Vec<Msg> {
        if let Some(index) = self.inputs.iter().position(|(id, _)| id == turn_id) {
            return &mut self.inputs[index].1;
        }
        self.inputs.push((turn_id.to_string(), Vec::new()));
        &mut self.inputs.last_mut().expect("刚刚插入").1
    }

    /// 全部回合（插入序）。
    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }

    /// 按 id 取回合。
    pub fn get(&self, turn_id: &str) -> Option<&Turn> {
        self.turns.iter().find(|turn| turn.turn_id == turn_id)
    }

    /// 某个回合收到的输入消息。
    pub fn inputs(&self, turn_id: &str) -> &[Msg] {
        self.inputs
            .iter()
            .find(|(id, _)| id == turn_id)
            .map(|(_, messages)| messages.as_slice())
            .unwrap_or(&[])
    }

    /// 某个回合是否已被某个收件箱命令开启过。
    pub fn was_started(&self, turn_id: &str, command_id: &str) -> bool {
        !self.inputs(turn_id).is_empty() && self.started_commands.iter().any(|id| id == command_id)
    }

    /// 已被收件箱命令开启过的命令 id（插入序）。
    pub fn started_commands(&self) -> &[String] {
        &self.started_commands
    }

    /// 已被消费的收件箱命令 id（插入序）。
    pub fn applied_commands(&self) -> &[String] {
        &self.applied_commands
    }

    /// 该回合的第一条用户输入文本（标题摘要用）。
    pub fn opening_input(&self, turn_id: &str) -> Option<&Msg> {
        self.inputs(turn_id).first()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Role as MsgRole;
    use crate::session::store::{InMemorySessionLogStore, SessionLogStore};
    use crate::session::SessionKey;
    use serde_json::json;
    use std::sync::Arc;

    fn log() -> Arc<dyn SessionLog> {
        InMemorySessionLogStore::new()
            .open(&SessionKey::new(Some("u"), "a", "s").unwrap(), None)
            .unwrap()
    }

    fn event(
        seq: i64,
        kind: &str,
        run: Option<&str>,
        turn: Option<&str>,
        payload: Value,
    ) -> SessionEvent {
        SessionEvent::new(
            format!("e{seq}"),
            seq,
            1_000,
            kind,
            run.map(str::to_string),
            turn.map(str::to_string),
            true,
            serde_json::to_string(&payload).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn run_lifecycle_drives_turn_status() {
        let log = log();
        let writer = log.acquire("w", std::time::Duration::from_secs(60)).unwrap();
        log.commit(
            &writer,
            "b1",
            0,
            vec![
                event(1, "run/start", Some("run-1"), Some("turn-1"), json!({})),
                event(
                    2,
                    "run/end",
                    Some("run-1"),
                    Some("turn-1"),
                    json!({ "status": "completed" }),
                ),
            ],
        )
        .unwrap();

        let turns = SessionTurns::read(log.as_ref()).unwrap();
        let turn = turns.get("turn-1").expect("回合应存在");
        assert_eq!(turn.status, "completed");
        assert!(turn.ended());
        assert!(turn.belongs_to("run-1"));
    }

    #[test]
    fn resumption_updates_run_but_keeps_position() {
        let log = log();
        let writer = log.acquire("w", std::time::Duration::from_secs(60)).unwrap();
        log.commit(
            &writer,
            "b1",
            0,
            vec![
                event(1, "run/start", Some("run-1"), Some("turn-a"), json!({})),
                event(
                    2,
                    "run/end",
                    Some("run-1"),
                    Some("turn-a"),
                    json!({ "status": "interrupted" }),
                ),
                event(3, "run/start", Some("run-2"), Some("turn-b"), json!({})),
                event(4, "run/start", Some("run-3"), Some("turn-a"), json!({})),
            ],
        )
        .unwrap();

        let turns = SessionTurns::read(log.as_ref()).unwrap();
        assert_eq!(turns.turns().len(), 2);
        assert_eq!(turns.turns()[0].turn_id, "turn-a", "续跑不应改变位置");
        assert_eq!(turns.turns()[0].run_id.as_deref(), Some("run-3"));
        assert_eq!(turns.turns()[0].status, "running");
        assert!(turns.get("turn-b").is_some());
    }

    #[test]
    fn cancellation_marks_turn_ended() {
        let log = log();
        let writer = log.acquire("w", std::time::Duration::from_secs(60)).unwrap();
        log.commit(
            &writer,
            "b1",
            0,
            vec![
                event(1, "run/start", Some("run-1"), Some("turn-1"), json!({})),
                event(
                    2,
                    "turn/cancelled",
                    Some("run-1"),
                    Some("turn-1"),
                    json!({ "reason": "user" }),
                ),
            ],
        )
        .unwrap();
        let turns = SessionTurns::read(log.as_ref()).unwrap();
        assert_eq!(turns.get("turn-1").unwrap().status, "cancelled");
    }

    #[test]
    fn inputs_are_deduplicated_by_message_id() {
        let log = log();
        let writer = log.acquire("w", std::time::Duration::from_secs(60)).unwrap();
        let first = Msg::user("user", "第一句");
        let second = Msg::user("user", "第二句");
        log.commit(
            &writer,
            "b1",
            0,
            vec![
                event(
                    1,
                    "input/received",
                    None,
                    Some("turn-1"),
                    json!({ "messages": [first, second] }),
                ),
                event(
                    2,
                    "input/received",
                    None,
                    Some("turn-1"),
                    json!({ "messages": [second.clone(), Msg::user("user", "第三句")] }),
                ),
            ],
        )
        .unwrap();

        let turns = SessionTurns::read(log.as_ref()).unwrap();
        let inputs = turns.inputs("turn-1");
        assert_eq!(inputs.len(), 3, "重复消息不重复计入");
        assert_eq!(inputs[0].role, MsgRole::User);
        assert_eq!(turns.opening_input("turn-1").unwrap().text(), "第一句");
    }

    #[test]
    fn inbox_commands_are_tracked() {
        let log = log();
        let writer = log.acquire("w", std::time::Duration::from_secs(60)).unwrap();
        log.commit(
            &writer,
            "b1",
            0,
            vec![
                event(
                    1,
                    "input/received",
                    None,
                    Some("turn-1"),
                    json!({ "messages": [Msg::user("user", "喂")] }),
                ),
                event(
                    2,
                    "inbox/started",
                    Some("run-1"),
                    Some("turn-1"),
                    json!({ "commandId": "cmd-1" }),
                ),
                event(
                    3,
                    "inbox/applied",
                    Some("run-1"),
                    Some("turn-1"),
                    json!({ "commandIds": ["cmd-1"] }),
                ),
            ],
        )
        .unwrap();

        let turns = SessionTurns::read(log.as_ref()).unwrap();
        assert!(turns.was_started("turn-1", "cmd-1"));
        assert!(!turns.was_started("turn-1", "cmd-2"));
        assert_eq!(turns.applied_commands(), ["cmd-1".to_string()]);
    }

    #[test]
    fn state_restored_clears_previous_turns() {
        let log = log();
        let writer = log.acquire("w", std::time::Duration::from_secs(60)).unwrap();
        log.commit(
            &writer,
            "b1",
            0,
            vec![
                event(1, "run/start", Some("run-1"), Some("turn-1"), json!({})),
                event(
                    2,
                    "state/restored",
                    None,
                    None,
                    json!({ "stateJson": "{}" }),
                ),
            ],
        )
        .unwrap();
        let turns = SessionTurns::read(log.as_ref()).unwrap();
        assert!(turns.turns().is_empty());
    }
}
