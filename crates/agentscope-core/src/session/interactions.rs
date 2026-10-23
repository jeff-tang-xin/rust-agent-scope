//! 未决交互视图（对应 Java `SessionInteractions`）。
//!
//! 「交互请求」指框架向会话外的一方要东西并挂起的事件：外部工具执行
//! （`kind = external_execution`）、用户确认等。请求与解决各是一条事件，
//! 投影即「已请求 − 已解决」。
//!
//! **调用方不能自造工具结果**：只有曾在已提交前缀里请求过、且类型匹配的
//! `requestId` 才允许被续跑，否则外部副作用无从核对。

use serde_json::Value;

use super::events::SessionEvent;
use super::journal::SessionLog;
use super::SessionError;

/// 已提交前缀上的未决请求集合（插入序，对应 Java 的 `LinkedHashMap`）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PendingInteractions {
    requests: Vec<(String, SessionEvent)>,
}

impl PendingInteractions {
    /// 扫描已提交前缀，得到未决请求。
    pub fn read(log: &dyn SessionLog) -> Result<Self, SessionError> {
        let upper = log.head()?.seq;
        let mut cursor = 0;
        let mut requests: Vec<(String, SessionEvent)> = Vec::new();
        while cursor < upper {
            let batch = log.read_after(cursor, 256)?;
            if batch.is_empty() {
                return Err(SessionError::log("missing interaction prefix"));
            }
            for event in batch {
                if event.seq > upper {
                    break;
                }
                match event.kind.as_str() {
                    "interaction/requested" => {
                        if let Some(id) = request_id(&event)? {
                            requests.retain(|(existing, _)| existing != &id);
                            requests.push((id, event.clone()));
                        }
                    }
                    "interaction/resolved" => {
                        if let Some(id) = request_id(&event)? {
                            requests.retain(|(existing, _)| existing != &id);
                        }
                    }
                    _ => {}
                }
                cursor = event.seq;
            }
        }
        Ok(Self { requests })
    }

    /// 是否没有未决请求。
    pub fn is_empty(&self) -> bool {
        self.requests.is_empty()
    }

    /// 未决请求数量。
    pub fn len(&self) -> usize {
        self.requests.len()
    }

    /// 按 id 取请求事件。
    pub fn get(&self, request_id: &str) -> Option<&SessionEvent> {
        self.requests
            .iter()
            .find(|(id, _)| id == request_id)
            .map(|(_, event)| event)
    }

    /// 按插入序遍历 `(requestId, 事件)`。
    pub fn iter(&self) -> impl Iterator<Item = (&str, &SessionEvent)> {
        self.requests
            .iter()
            .map(|(id, event)| (id.as_str(), event))
    }
}

/// 校验一组工具结果对应的未决请求是否合法，并返回它们所属的逻辑回合。
///
/// 全部请求必须存在、必须是 `external_execution` 类型、必须带逻辑回合，
/// 且必须同属一个回合——否则一次续跑会跨回合污染上下文。
pub fn continuation_turn(log: &dyn SessionLog, request_ids: &[String]) -> Result<String, SessionError> {
    if request_ids.is_empty() {
        return Err(SessionError::invalid("tool result IDs are required"));
    }
    let pending = PendingInteractions::read(log)?;
    let mut turn: Option<String> = None;
    for id in request_ids {
        let request = pending
            .get(id)
            .ok_or_else(|| SessionError::invalid(format!("no pending request: {id}")))?;
        let data = request.data()?;
        if data.get("kind").and_then(Value::as_str) != Some("external_execution") {
            return Err(SessionError::invalid(format!(
                "no pending external execution request: {id}"
            )));
        }
        let request_turn = request
            .turn_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                SessionError::invalid(format!("pending request has no logical turn: {id}"))
            })?;
        match &turn {
            Some(existing) if existing != request_turn => {
                return Err(SessionError::invalid(
                    "tool results belong to different turns",
                ))
            }
            _ => turn = Some(request_turn.to_string()),
        }
    }
    Ok(turn.expect("非空校验已在上方完成"))
}

fn request_id(event: &SessionEvent) -> Result<Option<String>, SessionError> {
    let data = event.data()?;
    Ok(data
        .get("requestId")
        .and_then(Value::as_str)
        .map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::store::{InMemorySessionLogStore, SessionLogStore};
    use crate::session::SessionKey;
    use serde_json::json;
    use std::sync::Arc;

    fn log() -> Arc<dyn SessionLog> {
        InMemorySessionLogStore::new()
            .open(&SessionKey::new(Some("u"), "a", "s").unwrap(), None)
            .unwrap()
    }

    fn event(seq: i64, kind: &str, turn: Option<&str>, payload: Value) -> SessionEvent {
        SessionEvent::new(
            format!("e{seq}"),
            seq,
            1_000,
            kind,
            Some("run-1".to_string()),
            turn.map(str::to_string),
            true,
            serde_json::to_string(&payload).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn requested_minus_resolved_is_pending() {
        let log = log();
        let writer = log.acquire("w", std::time::Duration::from_secs(60)).unwrap();
        log.commit(
            &writer,
            "b1",
            0,
            vec![
                event(
                    1,
                    "interaction/requested",
                    Some("turn-1"),
                    json!({ "requestId": "call-1", "kind": "external_execution" }),
                ),
                event(
                    2,
                    "interaction/requested",
                    Some("turn-1"),
                    json!({ "requestId": "call-2", "kind": "external_execution" }),
                ),
                event(
                    3,
                    "interaction/resolved",
                    Some("turn-1"),
                    json!({ "requestId": "call-1", "status": "provided" }),
                ),
            ],
        )
        .unwrap();

        let pending = PendingInteractions::read(log.as_ref()).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending.get("call-2").is_some());
        assert!(pending.get("call-1").is_none());
        assert_eq!(continuation_turn(log.as_ref(), &["call-2".into()]).unwrap(), "turn-1");
    }

    #[test]
    fn continuation_rejects_unknown_kind_and_mixed_turns() {
        let log = log();
        let writer = log.acquire("w", std::time::Duration::from_secs(60)).unwrap();
        log.commit(
            &writer,
            "b1",
            0,
            vec![
                event(
                    1,
                    "interaction/requested",
                    Some("turn-1"),
                    json!({ "requestId": "call-1", "kind": "external_execution" }),
                ),
                event(
                    2,
                    "interaction/requested",
                    Some("turn-2"),
                    json!({ "requestId": "call-2", "kind": "external_execution" }),
                ),
                event(
                    3,
                    "interaction/requested",
                    Some("turn-1"),
                    json!({ "requestId": "call-3", "kind": "user_confirm" }),
                ),
                event(
                    4,
                    "interaction/requested",
                    None,
                    json!({ "requestId": "call-4", "kind": "external_execution" }),
                ),
            ],
        )
        .unwrap();

        // 空集合。
        assert!(continuation_turn(log.as_ref(), &[]).is_err());
        // 未知 id。
        assert!(continuation_turn(log.as_ref(), &["nope".into()]).is_err());
        // 类型不匹配（不是外部执行）。
        assert!(continuation_turn(log.as_ref(), &["call-3".into()]).is_err());
        // 缺逻辑回合。
        assert!(continuation_turn(log.as_ref(), &["call-4".into()]).is_err());
        // 跨回合。
        assert!(
            continuation_turn(log.as_ref(), &["call-1".into(), "call-2".into()]).is_err()
        );
        // 单回合合法。
        assert_eq!(continuation_turn(log.as_ref(), &["call-1".into()]).unwrap(), "turn-1");
    }

    #[test]
    fn later_request_with_same_id_replaces_earlier_one() {
        let log = log();
        let writer = log.acquire("w", std::time::Duration::from_secs(60)).unwrap();
        log.commit(
            &writer,
            "b1",
            0,
            vec![
                event(
                    1,
                    "interaction/requested",
                    Some("turn-1"),
                    json!({ "requestId": "call-1", "kind": "external_execution" }),
                ),
                event(
                    2,
                    "interaction/requested",
                    Some("turn-2"),
                    json!({ "requestId": "call-1", "kind": "external_execution" }),
                ),
            ],
        )
        .unwrap();
        let pending = PendingInteractions::read(log.as_ref()).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending.get("call-1").unwrap().turn_id.as_deref(),
            Some("turn-2")
        );
    }
}
