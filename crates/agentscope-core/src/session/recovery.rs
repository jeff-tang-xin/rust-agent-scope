//! 显式的运维性对账（对应 Java `SessionRecovery`）。
//!
//! 这条模块的立场很硬：**结果未知的副作用绝不自动重跑**。工具已经派发、结果没
//! 回来的调用，只能由外部（人 / 上层调度器）说「它其实成功了」或「它没发生」，
//! 框架负责把这段结论写进日志并让会话回到可继续的状态。
//!
//! 两个入口：
//!
//! - [`cancel_pending`]：把一个被打断回合里所有未决交互标记为取消，为每个请求
//!   合成一条 `interrupted` 工具结果，并明确写出「外部副作用未经验证」——
//!   不伪造成功，也不重放。
//! - [`reconcile`]：接收**恰好等于**结果未知集合的一组工具结果，把它们作为事实
//!   写进日志，并记一条 `recovery/applied` 收尾。

use std::collections::HashSet;

use serde_json::{json, Value};

use super::events::SessionEvent;
use super::inbox::SessionInbox;
use super::interactions::PendingInteractions;
use super::projection::SessionProjection;
use super::state_access::ADMIN_LEASE;
use super::turns::SessionTurns;
use super::{SessionError, SessionLog};
use crate::message::{ContentBlock, Msg, Role, ToolCallState, ToolResultState};

/// 取消文本：必须让读日志的人知道「副作用是否发生无从判断」。
const CANCELLED_TEXT: &str = "Caller cancelled this pending interaction; external side effects, \
                              if any, were not verified.";

/// 取消某个回合里所有未决交互（幂等：已取消的回合只清理转向指令）。
///
/// 不做任何自动重放，也不假装成功。存在结果未知的工具调用时拒绝执行。
pub fn cancel_pending(log: &dyn SessionLog, turn_id: &str) -> Result<(), SessionError> {
    let guard = Lease::acquire(log)?;
    let projection = SessionProjection::read(log)?;
    if !projection.uncertain_tool_calls().is_empty() {
        return Err(SessionError::log(
            "reconcile uncertain tool outcomes before cancelling this interrupted turn",
        ));
    }
    let Some(mut state) = projection.restore() else {
        // 没有状态基线：这条会话从未提交过状态，没东西可取消。
        return Ok(());
    };

    // 已经取消过：只补做「拒绝该回合的 steer 指令」，不重复追加事实。
    let upper = log.head()?.seq;
    for event in log.scan(0, upper)? {
        if event.kind == "turn/cancelled" && event.turn_id.as_deref() == Some(turn_id) {
            reject_steering(log, turn_id)?;
            return Ok(());
        }
    }

    let run = guard.writer.owner.clone();
    let mut seq = projection.as_of_seq();
    let mut facts: Vec<SessionEvent> = Vec::new();
    let mut cancelled_calls: HashSet<String> = HashSet::new();

    for (request_id, request) in PendingInteractions::read(log)?.iter() {
        if request.turn_id.as_deref() != Some(turn_id) {
            continue;
        }
        let data = request.data()?;
        let call = data.get("call").cloned().unwrap_or(Value::Null);
        let call_id = call
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or(request_id)
            .to_string();
        let call_name = call
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        cancelled_calls.insert(call_id.clone());

        let result = ContentBlock::ToolResult {
            id: call_id.clone(),
            name: call_name,
            output: vec![ContentBlock::Text {
                text: CANCELLED_TEXT.to_string(),
            }],
            state: ToolResultState::Interrupted,
            execution_details: None,
        };
        let message = Msg::new(Role::Tool, "", vec![result]);

        facts.push(next_event(
            &mut seq,
            "interaction/resolved",
            Some(&run),
            Some(turn_id),
            json!({
                "requestId": call_id,
                "kind": data.get("kind").cloned().unwrap_or(Value::Null),
                "status": "cancelled",
            }),
        )?);
        facts.push(next_event(
            &mut seq,
            "tool/result",
            Some(&run),
            Some(turn_id),
            json!({ "message": message, "synthetic": true }),
        )?);
        state.context.push(message);
    }

    // 被取消的调用在对话里留下「已结束」的痕迹：不要让它看起来还在跑。
    for message in &mut state.context {
        for block in &mut message.content {
            if let ContentBlock::ToolUse { id, state, .. } = block {
                if cancelled_calls.contains(id) {
                    *state = Some(ToolCallState::Finished);
                }
            }
        }
    }

    facts.push(next_event(
        &mut seq,
        "state/checkpoint",
        Some(&run),
        Some(turn_id),
        json!({
            "stateJson": state.to_json().map_err(state_error)?,
            "projectorVersion": 1,
            "reason": "pending_cancelled",
        }),
    )?);
    if !projection.active_runs().is_empty() {
        facts.push(next_event(
            &mut seq,
            "recovery/applied",
            Some(&run),
            Some(turn_id),
            json!({
                "interruptedRuns": projection.active_runs(),
                "reason": "caller_cancelled",
            }),
        )?);
    }
    facts.push(next_event(
        &mut seq,
        "turn/cancelled",
        Some(&run),
        Some(turn_id),
        json!({ "status": "cancelled" }),
    )?);

    log.commit(
        &guard.writer,
        &uuid::Uuid::new_v4().to_string(),
        projection.as_of_seq(),
        facts,
    )?;
    reject_steering(log, turn_id)
}

/// 用外部提供的结论对账「结果未知」的工具调用。
///
/// `outcomes` 必须**恰好**覆盖全部未知调用：多一个、少一个、id 不匹配、或给出
/// `suspended` 占位结果，都直接拒绝。
pub fn reconcile(
    log: &dyn SessionLog,
    outcomes: &[ContentBlock],
    reason: &str,
) -> Result<(), SessionError> {
    if reason.trim().is_empty() {
        return Err(SessionError::invalid("a reconciliation reason is required"));
    }
    let guard = Lease::acquire(log)?;
    let projection = SessionProjection::read(log)?;
    let uncertain: HashSet<String> = projection.uncertain_tool_calls().iter().cloned().collect();

    let mut provided: HashSet<String> = HashSet::new();
    for outcome in outcomes {
        let ContentBlock::ToolResult { id, state, .. } = outcome else {
            return Err(SessionError::invalid(
                "reconciliation outcomes must be tool result blocks",
            ));
        };
        if *state == ToolResultState::Suspended {
            return Err(SessionError::invalid(
                "mismatched or suspended reconciliation result",
            ));
        }
        if !uncertain.contains(id) {
            return Err(SessionError::invalid(
                "supply exactly the unresolved tool outcomes",
            ));
        }
        if !provided.insert(id.clone()) {
            return Err(SessionError::invalid("duplicate reconciliation outcome"));
        }
    }
    if provided != uncertain {
        return Err(SessionError::invalid(
            "supply exactly the unresolved tool outcomes",
        ));
    }

    let Some(mut state) = projection.restore() else {
        return Err(SessionError::log("no recoverable state baseline"));
    };

    let run = uuid::Uuid::new_v4().to_string();
    let mut seq = projection.as_of_seq();
    let mut repairs: Vec<SessionEvent> = Vec::new();
    let mut resolved: Vec<String> = Vec::new();

    for outcome in outcomes {
        let message = Msg::new(Role::Tool, "", vec![outcome.clone()]);
        state.context.push(message.clone());
        if let ContentBlock::ToolResult { id, .. } = outcome {
            resolved.push(id.clone());
        }
        repairs.push(next_event(
            &mut seq,
            "tool/result",
            Some(&run),
            None,
            json!({ "message": message, "reconciled": true }),
        )?);
    }

    repairs.push(next_event(
        &mut seq,
        "recovery/applied",
        Some(&run),
        None,
        json!({
            "resolvedToolCallIds": resolved,
            "reason": reason,
            "outcomes": outcomes,
            "interruptedRuns": projection.active_runs(),
        }),
    )?);
    repairs.push(next_event(
        &mut seq,
        "state/checkpoint",
        Some(&run),
        None,
        json!({
            "stateJson": state.to_json().map_err(state_error)?,
            "projectorVersion": 1,
        }),
    )?);

    log.commit(
        &guard.writer,
        &uuid::Uuid::new_v4().to_string(),
        projection.as_of_seq(),
        repairs,
    )?;
    Ok(())
}

/// 拒绝该回合尚未消费的 `steer` 指令（回合已取消，转向没有意义）。
fn reject_steering(log: &dyn SessionLog, turn_id: &str) -> Result<(), SessionError> {
    let applied = SessionTurns::read(log)?.applied_commands().to_vec();
    let inbox = SessionInbox::new(log)?;
    inbox.locked(|tx| {
        let snapshot = tx.snapshot()?;
        for command in &snapshot.commands {
            if command.kind == "steer"
                && command.turn_id.as_deref() == Some(turn_id)
                && !applied.contains(&command.id)
                && snapshot.rejected_reason(&command.id).is_none()
            {
                tx.reject(command, "turn_cancelled")?;
            }
        }
        Ok(())
    })
}

fn next_event(
    seq: &mut i64,
    kind: &str,
    run: Option<&str>,
    turn: Option<&str>,
    payload: Value,
) -> Result<SessionEvent, SessionError> {
    *seq += 1;
    SessionEvent::new(
        uuid::Uuid::new_v4().to_string(),
        *seq,
        super::now_millis(),
        kind,
        run.map(str::to_string),
        turn.map(str::to_string),
        true,
        serde_json::to_string(&payload)?,
    )
}

fn state_error(error: crate::state::StateError) -> SessionError {
    SessionError::log(format!("recovery: {error}"))
}

/// 租约守卫：pin 住写者，Drop 时释放（含 panic 路径）。
struct Lease<'a> {
    log: &'a dyn SessionLog,
    writer: super::Writer,
}

impl<'a> Lease<'a> {
    fn acquire(log: &'a dyn SessionLog) -> Result<Self, SessionError> {
        let writer = log.acquire(&uuid::Uuid::new_v4().to_string(), ADMIN_LEASE)?;
        Ok(Self { log, writer })
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        let _ = self.log.release(&self.writer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::inbox::Command;
    use crate::session::store::{InMemorySessionLogStore, SessionLogStore};
    use crate::session::{SessionKey, Writer};
    use crate::state::AgentState;
    use std::sync::Arc;
    use std::time::Duration;

    fn log() -> Arc<dyn SessionLog> {
        InMemorySessionLogStore::new()
            .open(&SessionKey::new(Some("u"), "a", "s").unwrap(), None)
            .unwrap()
    }

    fn event(seq: i64, kind: &str, turn: Option<&str>, payload: Value) -> SessionEvent {
        SessionEvent::new(
            format!("e{seq}"),
            seq,
            super::super::now_millis(),
            kind,
            Some("run-1".to_string()),
            turn.map(str::to_string),
            true,
            serde_json::to_string(&payload).unwrap(),
        )
        .unwrap()
    }

    fn commit(log: &Arc<dyn SessionLog>, writer: &Writer, expected: i64, events: Vec<SessionEvent>) {
        log.commit(writer, &format!("b{expected}"), expected, events).unwrap();
    }

    /// 准备：一条带工具调用的基线 + 一个外部执行请求。
    fn seed_pending(log: &Arc<dyn SessionLog>) {
        let mut state = AgentState::with_session_id("s").with_user_id(Some("u".into()));
        state.context.push(Msg::user("user", "跑个命令"));
        state.context.push(Msg::new(
            Role::Assistant,
            "",
            vec![ContentBlock::ToolUse {
                id: "call-1".into(),
                name: "bash".into(),
                input: json!({ "cmd": "ls" }),
                state: Some(ToolCallState::Submitted),
                content: None,
            }],
        ));
        let writer = log.acquire("seed", Duration::from_secs(60)).unwrap();
        commit(
            log,
            &writer,
            0,
            vec![
                event(
                    1,
                    "state/checkpoint",
                    None,
                    json!({ "stateJson": state.to_json().unwrap(), "projectorVersion": 1 }),
                ),
                event(
                    2,
                    "interaction/requested",
                    Some("turn-1"),
                    json!({
                        "requestId": "call-1",
                        "kind": "external_execution",
                        "call": {
                            "type": "tool_use",
                            "id": "call-1",
                            "name": "bash",
                            "input": { "cmd": "ls" },
                            "state": "submitted",
                        },
                    }),
                ),
            ],
        );
        log.release(&writer).unwrap();
    }

    fn kinds(log: &Arc<dyn SessionLog>) -> Vec<String> {
        log.scan(0, log.head().unwrap().seq)
            .unwrap()
            .into_iter()
            .map(|event| event.kind)
            .collect()
    }

    #[test]
    fn cancel_pending_records_interrupted_results_without_claiming_success() {
        let log = log();
        seed_pending(&log);
        cancel_pending(log.as_ref(), "turn-1").unwrap();

        // 未决请求已被清空。
        assert!(PendingInteractions::read(log.as_ref()).unwrap().is_empty());

        let state = SessionProjection::read(log.as_ref()).unwrap().restore().unwrap();
        let recovered = state
            .context
            .iter()
            .find(|message| {
                message.content.iter().any(|block| {
                    matches!(block, ContentBlock::ToolResult { state: ToolResultState::Interrupted, .. })
                })
            })
            .expect("应合成一条 interrupted 结果");
        let ContentBlock::ToolResult { output, .. } = &recovered.content[0] else {
            panic!("应为工具结果块")
        };
        let text = match &output[0] {
            ContentBlock::Text { text } => text.clone(),
            other => panic!("unexpected: {other:?}"),
        };
        assert!(text.contains("not verified"), "必须说明副作用未验证：{text}");

        let kinds = kinds(&log);
        for expected in [
            "interaction/resolved",
            "tool/result",
            "state/checkpoint",
            "turn/cancelled",
        ] {
            assert!(kinds.iter().any(|kind| kind == expected), "缺少 {expected}");
        }
        // 工具调用状态改为 finished，不能看起来还在跑。
        assert!(state.context.iter().any(|message| message.content.iter().any(|block| {
            matches!(block, ContentBlock::ToolUse { state: Some(ToolCallState::Finished), .. })
        })));
    }

    #[test]
    fn cancel_pending_is_idempotent() {
        let log = log();
        seed_pending(&log);
        cancel_pending(log.as_ref(), "turn-1").unwrap();
        let before = log.head().unwrap().seq;
        cancel_pending(log.as_ref(), "turn-1").unwrap();
        assert_eq!(log.head().unwrap().seq, before, "重复取消不应追加事实");
    }

    #[test]
    fn cancel_pending_requires_reconciled_uncertain_calls() {
        let log = log();
        seed_pending(&log);
        let writer = log.acquire("x", Duration::from_secs(60)).unwrap();
        let seq = log.head().unwrap().seq;
        commit(
            &log,
            &writer,
            seq,
            vec![event(
                seq + 1,
                "tool/dispatch",
                Some("turn-1"),
                json!({ "toolCallId": "call-2" }),
            )],
        );
        log.release(&writer).unwrap();

        let error = cancel_pending(log.as_ref(), "turn-1").unwrap_err();
        assert!(matches!(error, SessionError::Log(_)), "必须拒绝：{error}");
    }

    #[test]
    fn cancel_pending_without_baseline_is_a_noop() {
        let log = log();
        cancel_pending(log.as_ref(), "turn-1").unwrap();
        assert_eq!(log.head().unwrap().seq, 0);
    }

    #[test]
    fn cancel_pending_rejects_unconsumed_steering() {
        let log = log();
        seed_pending(&log);
        let inbox = SessionInbox::new(log.as_ref()).unwrap();
        inbox
            .locked(|tx| tx.accept(Command::new("steer-1", "steer").with_turn("turn-1")))
            .unwrap();

        cancel_pending(log.as_ref(), "turn-1").unwrap();
        let snapshot = inbox.snapshot().unwrap();
        assert_eq!(
            snapshot.rejected_reason("steer-1"),
            Some("turn_cancelled"),
            "回合已取消，转向指令必须被拒"
        );
    }

    #[test]
    fn reconcile_requires_exactly_the_unresolved_set() {
        let log = log();
        seed_pending(&log);
        let writer = log.acquire("x", Duration::from_secs(60)).unwrap();
        let seq = log.head().unwrap().seq;
        commit(
            &log,
            &writer,
            seq,
            vec![event(
                seq + 1,
                "tool/dispatch",
                Some("turn-1"),
                json!({ "toolCallId": "call-1" }),
            )],
        );
        log.release(&writer).unwrap();

        let outcome = |id: &str, state: ToolResultState| ContentBlock::ToolResult {
            id: id.into(),
            name: "bash".into(),
            output: vec![ContentBlock::Text { text: "ok".into() }],
            state,
            execution_details: None,
        };

        // 空集合：少一个。
        assert!(reconcile(log.as_ref(), &[], "对账").is_err());
        // 未知 id：多一个。
        assert!(reconcile(log.as_ref(), &[outcome("call-9", ToolResultState::Success)], "对账").is_err());
        // suspended 占位：不算结论。
        assert!(reconcile(
            log.as_ref(),
            &[outcome("call-1", ToolResultState::Suspended)],
            "对账"
        )
        .is_err());
        // 空 reason。
        assert!(reconcile(
            log.as_ref(),
            &[outcome("call-1", ToolResultState::Success)],
            "  "
        )
        .is_err());

        // 恰好覆盖：成功。
        reconcile(
            log.as_ref(),
            &[outcome("call-1", ToolResultState::Success)],
            "命令其实执行成功了",
        )
        .unwrap();
        assert!(SessionProjection::read(log.as_ref())
            .unwrap()
            .uncertain_tool_calls()
            .is_empty());
        let kinds = kinds(&log);
        assert!(kinds.contains(&"recovery/applied".to_string()));
        assert!(kinds.contains(&"state/checkpoint".to_string()));

        // 重复对账：未知集合已空，再对账必须失败（不能重复写事实）。
        assert!(reconcile(
            log.as_ref(),
            &[outcome("call-1", ToolResultState::Success)],
            "重复"
        )
        .is_err());
    }
}
