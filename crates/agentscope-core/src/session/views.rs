//! 会话视图（对应 Java `SessionViews`）：从不可变事件日志重建人类/历史可读视图。
//!
//! 关键语义：上下文替换（`context/replaced`）不会抹掉更早的消息——视图按消息 id
//! 去重合并，先出现的消息保留。

use std::collections::HashMap;

use serde_json::Value;

use super::journal::SessionLog;
use super::SessionError;
use crate::message::Msg;
use crate::state::AgentState;

/// 转录视图：投影到的日志序号 + 按出现顺序排列的消息。
#[derive(Debug, Clone)]
pub struct Transcript {
    /// 视图覆盖到的最后一条事件序号。
    pub as_of_seq: i64,
    /// 按出现顺序排列的消息（同 id 保留首次出现，`turn/output` 例外：覆盖）。
    pub messages: Vec<Msg>,
}

/// 从会话日志重建转录视图（对应 Java `SessionViews.transcript`）。
///
/// 收录的事件：
/// - `migration/baseline`：`stateJson` 反序列化为 [`AgentState`]，其 context 消息
///   全部并入（put-if-absent）；
/// - `message/user` / `message/assistant` / `tool/result` / `turn/output`：
///   payload 的 `message` 字段反序列化为 [`Msg`]（`turn/output` 覆盖同 id 旧消息，
///   其余 put-if-absent）。
pub fn transcript(log: &dyn SessionLog) -> Result<Transcript, SessionError> {
    let target = log.head()?.seq;
    // Java 用 LinkedHashMap<msgId, Msg> 保插入序；Rust 用 Vec + 索引表等价实现。
    let mut messages: Vec<Msg> = Vec::new();
    let mut index_by_id: HashMap<String, usize> = HashMap::new();

    for event in log.scan(0, target)? {
        match event.kind.as_str() {
            "migration/baseline" => {
                let payload: Value = serde_json::from_str(&event.payload_json)
                    .map_err(|error| SessionError::log(format!("invalid baseline payload: {error}")))?;
                if let Some(state_json) = payload.get("stateJson").and_then(Value::as_str) {
                    let state = AgentState::from_json_str(state_json)
                        .map_err(|error| SessionError::log(format!("invalid baseline state: {error}")))?;
                    for msg in state.context {
                        if !index_by_id.contains_key(&msg.id) {
                            index_by_id.insert(msg.id.clone(), messages.len());
                            messages.push(msg);
                        }
                    }
                }
            }
            "message/user" | "message/assistant" | "tool/result" | "turn/output" => {
                let payload: Value = serde_json::from_str(&event.payload_json)
                    .map_err(|error| SessionError::log(format!("invalid message payload: {error}")))?;
                let Some(message_value) = payload.get("message") else {
                    continue;
                };
                if message_value.is_null() {
                    continue;
                }
                let msg: Msg = serde_json::from_value(message_value.clone()).map_err(|error| {
                    SessionError::log(format!("invalid message in session log: {error}"))
                })?;
                match index_by_id.get(&msg.id) {
                    // turn/output 语义 = 最终输出覆盖同 id 的占位消息。
                    Some(&idx) if event.kind == "turn/output" => messages[idx] = msg,
                    Some(_) => {}
                    None => {
                        index_by_id.insert(msg.id.clone(), messages.len());
                        messages.push(msg);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(Transcript {
        as_of_seq: target,
        messages,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ContentBlock, Role, ToolResultState};
    use crate::session::store::InMemorySessionLogStore;
    use crate::session::{SessionKey, SessionLogStore};
    use serde_json::json;
    use std::time::Duration;

    fn key(session_id: &str) -> SessionKey {
        SessionKey::new(Some("user-a"), "agent-x", session_id).unwrap()
    }

    /// 本地事件构造辅助（对齐 recorder 的 payload 形态：`{ "message": msg }`）。
    fn message_event(seq: i64, kind: &str, msg: &Msg) -> crate::session::SessionEvent {
        crate::session::SessionEvent::new(
            uuid::Uuid::new_v4().to_string(),
            seq,
            crate::session::now_millis(),
            kind,
            Some("run-1".into()),
            Some("turn-1".into()),
            true,
            serde_json::to_string(&json!({ "message": msg })).unwrap(),
        )
        .unwrap()
    }

    fn user_msg(id: &str, text: &str) -> Msg {
        Msg {
            id: id.into(),
            name: "user".into(),
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: text.into(),
            }],
            metadata: None,
            timestamp: None,
            usage: None,
            generate_reason: None,
        }
    }

    #[tokio::test]
    async fn transcript_replays_messages_in_order() {
        let store = InMemorySessionLogStore::new();
        let k = key("s1");
        let log = store.open(&k, None).unwrap();
        let writer = log.acquire("tester", Duration::from_secs(60)).unwrap();
        let msg1 = user_msg("m1", "hello");
        let msg2 = user_msg("m2", "world");
        log.commit(
            &writer,
            "b1",
            0,
            vec![
                message_event(1, "message/user", &msg1),
                message_event(2, "message/assistant", &msg2),
            ],
        )
        .unwrap();
        log.release(&writer).unwrap();

        let view = transcript(log.as_ref()).unwrap();
        assert_eq!(view.as_of_seq, 2);
        assert_eq!(view.messages.len(), 2);
        assert_eq!(view.messages[0].id, "m1");
        assert_eq!(view.messages[1].id, "m2");
    }

    #[tokio::test]
    async fn transcript_dedupes_by_id_and_turn_output_overrides() {
        let store = InMemorySessionLogStore::new();
        let k = key("s2");
        let log = store.open(&k, None).unwrap();
        let writer = log.acquire("tester", Duration::from_secs(60)).unwrap();
        let placeholder = user_msg("m1", "placeholder");
        let final_msg = user_msg("m1", "final answer");
        log.commit(
            &writer,
            "b1",
            0,
            vec![
                message_event(1, "message/user", &placeholder),
                message_event(2, "turn/output", &final_msg),
                // 重复的 message/user 不覆盖 turn/output 的结果。
                message_event(3, "message/user", &placeholder),
            ],
        )
        .unwrap();
        log.release(&writer).unwrap();

        let view = transcript(log.as_ref()).unwrap();
        assert_eq!(view.messages.len(), 1);
        assert!(view.messages[0].content[0]
            == ContentBlock::Text {
                text: "final answer".into()
            });
    }

    #[tokio::test]
    async fn transcript_includes_tool_result_messages() {
        let store = InMemorySessionLogStore::new();
        let k = key("s3");
        let log = store.open(&k, None).unwrap();
        let writer = log.acquire("tester", Duration::from_secs(60)).unwrap();
        let tool_msg = Msg {
            id: "t1".into(),
            name: "tool".into(),
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult {
                id: "call-1".into(),
                name: "search".into(),
                output: vec![ContentBlock::Text {
                    text: "result payload".into(),
                }],
                state: ToolResultState::Success,
                execution_details: None,
            }],
            metadata: None,
            timestamp: None,
            usage: None,
            generate_reason: None,
        };
        log.commit(
            &writer,
            "b1",
            0,
            vec![message_event(1, "tool/result", &tool_msg)],
        )
        .unwrap();
        log.release(&writer).unwrap();

        let view = transcript(log.as_ref()).unwrap();
        assert_eq!(view.messages.len(), 1);
        assert_eq!(view.messages[0].id, "t1");
    }

    #[tokio::test]
    async fn transcript_empty_log_is_empty() {
        let store = InMemorySessionLogStore::new();
        let k = key("s4");
        let log = store.open(&k, None).unwrap();
        let view = transcript(log.as_ref()).unwrap();
        assert_eq!(view.as_of_seq, 0);
        assert!(view.messages.is_empty());
    }

    /// baseline 事件里的 stateJson 消息也要并入（对齐 Java migration/baseline 分支）。
    #[tokio::test]
    async fn transcript_merges_baseline_state_messages() {
        let store = InMemorySessionLogStore::new();
        let k = key("s5");
        let log = store.open(&k, None).unwrap();
        let writer = log.acquire("tester", Duration::from_secs(60)).unwrap();
        let mut state = AgentState::default();
        state.context.push(user_msg("b1", "from baseline"));
        let baseline = json!({
            "stateJson": state.to_json().unwrap(),
            "coverage": "baseline_only",
        });
        let event = crate::session::SessionEvent::new(
            uuid::Uuid::new_v4().to_string(),
            1,
            crate::session::now_millis(),
            "migration/baseline",
            Some("run-1".into()),
            Some("turn-1".into()),
            true,
            serde_json::to_string(&baseline).unwrap(),
        )
        .unwrap();
        log.commit(&writer, "b1", 0, vec![event]).unwrap();
        log.release(&writer).unwrap();

        let view = transcript(log.as_ref()).unwrap();
        assert_eq!(view.messages.len(), 1);
        assert_eq!(view.messages[0].id, "b1");
    }
}
