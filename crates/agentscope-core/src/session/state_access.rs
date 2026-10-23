//! 对原生历史的只读快照与「有围栏的管理性状态变更」
//! （对应 Java `SessionStateAccess`）。
//!
//! 两个入口的差别只在**要不要写**：
//!
//! - [`read`]：只重放已提交前缀，不抢写者、不改历史，得到的 `AgentState` 是
//!   一份游离副本，随便改。
//! - [`update`] / [`replace`]：在写者租约内重新加载状态、应用改动、提交一批
//!   事实事件（新增消息 / 子上下文变更 / 新的 `state/checkpoint`）。
//!
//! 三道围栏是这套设计的核心，任何一条不满足都直接失败：
//!
//! 1. **未决工具结果**：`tool/dispatch` 之后没有 `action/end` 时，外部副作用
//!    是否发生未知——此时改状态等于把未知当成已知。
//! 2. **过期快照**：`replace` 带来的 `as_of_seq` 必须仍等于当前链头，否则说明
//!    别处已经推进过会话，覆盖会静默吞掉那次执行。
//! 3. **未决交互 + 改上下文**：还有等外部回复的请求时替换 `context`，
//!    回填的消息就无处安放。

use std::collections::HashSet;
use std::time::Duration;

use serde_json::{json, Value};

use super::events::SessionEvent;
use super::interactions::PendingInteractions;
use super::journal::SessionLog;
use super::projection::SessionProjection;
use super::SessionError;
use crate::message::{ContentBlock, Role};
use crate::state::AgentState;

/// 管理性变更使用的状态改写闭包。
type Mutation<'a> = Box<dyn FnOnce(&mut AgentState) + 'a>;

/// 管理性变更的租约时长（对应 Java `Duration.ofMinutes(2)`）。
pub const ADMIN_LEASE: Duration = Duration::from_secs(120);

/// 一份游离状态，以及它来自哪个已提交前缀（对应 Java `Snapshot` record）。
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    /// 恢复时覆盖到的最后一条序号。
    pub as_of_seq: i64,
    /// 恢复出的状态（游离副本）。
    pub state: AgentState,
}

impl Snapshot {
    /// 新建快照。
    pub fn new(as_of_seq: i64, state: AgentState) -> Self {
        Self { as_of_seq, state }
    }
}

/// 只读读取：不抢租约、不改历史。
///
/// 前缀里没有状态基线时用 `initial_state()` 兜底（新会话场景）。
pub fn read(log: &dyn SessionLog, initial_state: impl FnOnce() -> AgentState) -> Result<Snapshot, SessionError> {
    let projection = SessionProjection::read(log)?;
    let state = projection.restore().unwrap_or_else(initial_state);
    Ok(Snapshot::new(projection.as_of_seq(), state))
}

/// 持租约重新加载、应用改动并提交检查点。
pub fn update(
    log: &dyn SessionLog,
    initial_state: impl FnOnce() -> AgentState,
    reason: &str,
    mutation: impl FnOnce(&mut AgentState),
) -> Result<Snapshot, SessionError> {
    change(log, initial_state, reason, None, Some(Box::new(mutation)))
}

/// 仅在已提交前缀仍是最新时，保存一份外部编辑过的快照。
pub fn replace(
    log: &dyn SessionLog,
    initial_state: impl FnOnce() -> AgentState,
    reason: &str,
    snapshot: Snapshot,
) -> Result<Snapshot, SessionError> {
    // 冻结：序列化往返一次，丢弃运行态缓存等不该持久化的字段。
    let frozen = AgentState::from_json_str(&snapshot.state.to_json().map_err(state_error)?)
        .map_err(state_error)?;
    change(
        log,
        initial_state,
        reason,
        Some(Snapshot::new(snapshot.as_of_seq, frozen)),
        None,
    )
}

fn change(
    log: &dyn SessionLog,
    initial_state: impl FnOnce() -> AgentState,
    reason: &str,
    replacement: Option<Snapshot>,
    mutation: Option<Mutation<'_>>,
) -> Result<Snapshot, SessionError> {
    if reason.trim().is_empty() {
        return Err(SessionError::invalid("a state change reason is required"));
    }
    let guard = leases::Lease::acquire(log)?;

    let projection = SessionProjection::read(log)?;
    // 围栏 1：存在结果未知的工具调用 → 先和解，再谈改状态。
    if !projection.uncertain_tool_calls().is_empty() {
        return Err(SessionError::log(
            "reconcile unresolved tool outcomes before changing session state",
        ));
    }
    // 围栏 2：快照已过期。
    if let Some(replacement) = &replacement {
        if replacement.as_of_seq != projection.as_of_seq() {
            return Err(SessionError::log(
                "session state changed since inspection; reload or use an atomic update",
            ));
        }
    }

    let current = projection.restore().unwrap_or_else(initial_state);
    let user_id = current.user_id.clone();
    let session_id = current.session_id.clone();
    let before_json = current.to_json().map_err(state_error)?;
    let previous_ids: HashSet<String> = current.context.iter().map(|message| message.id.clone()).collect();

    let mut changed = match replacement {
        Some(replacement) => replacement.state,
        None => current,
    };
    if let Some(mutation) = mutation {
        mutation(&mut changed);
    }

    // 会话身份是恢复的锚点，任何管理性变更都不得改写它。
    if changed.user_id != user_id || changed.session_id != session_id {
        return Err(SessionError::invalid(
            "state changes must preserve session identity",
        ));
    }

    let state_json = changed.to_json().map_err(state_error)?;
    let before: Value = serde_json::from_str(&before_json)?;
    let after: Value = serde_json::from_str(&state_json)?;
    let context_changed = before.get("context") != after.get("context");

    // 围栏 3：还有等外部回复的请求时，不能替换上下文。
    if context_changed && !PendingInteractions::read(log)?.is_empty() {
        return Err(SessionError::log(
            "resolve pending interactions before replacing context",
        ));
    }

    let mut facts: Vec<SessionEvent> = Vec::new();
    if context_changed {
        append_fact(
            &mut facts,
            projection.as_of_seq(),
            "context/replaced",
            json!({
                "reason": reason,
                "messages": changed.context,
                "previousAsOfSeq": projection.as_of_seq(),
            }),
        )?;
    }
    for message in &changed.context {
        // 只记新增消息：已有消息的历史事实早已在日志里，重复记会让重放翻倍。
        if previous_ids.contains(&message.id) {
            continue;
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
        append_fact(
            &mut facts,
            projection.as_of_seq(),
            kind,
            json!({ "message": message }),
        )?;
    }
    for (field, kind) in [
        ("tasks_context", "task/changed"),
        ("plan_mode_context", "plan/changed"),
        ("permission_context", "permission/changed"),
    ] {
        let value = after.get(field);
        if let Some(value) = value {
            if Some(value) != before.get(field) {
                append_fact(&mut facts, projection.as_of_seq(), kind, json!({ "state": value }))?;
            }
        }
    }
    append_fact(
        &mut facts,
        projection.as_of_seq(),
        "state/checkpoint",
        json!({
            "stateJson": state_json,
            "projectorVersion": 1,
            "reason": reason,
        }),
    )?;

    let head = log.commit(
        &guard.writer,
        &uuid::Uuid::new_v4().to_string(),
        projection.as_of_seq(),
        facts,
    )?;
    Ok(Snapshot::new(
        head.seq,
        AgentState::from_json_str(&state_json).map_err(state_error)?,
    ))
}

fn append_fact(
    facts: &mut Vec<SessionEvent>,
    previous_seq: i64,
    kind: &str,
    payload: Value,
) -> Result<(), SessionError> {
    facts.push(SessionEvent::new(
        uuid::Uuid::new_v4().to_string(),
        previous_seq + facts.len() as i64 + 1,
        super::now_millis(),
        kind,
        None,
        None,
        true,
        serde_json::to_string(&payload)?,
    )?);
    Ok(())
}

fn state_error(error: crate::state::StateError) -> SessionError {
    SessionError::log(format!("state access: {error}"))
}

/// 写者租约的 RAII 守卫：即使中途 panic 也会释放，不让租约泄漏。
mod leases {
    use super::{SessionError, SessionLog, ADMIN_LEASE};
    use crate::session::journal::Writer;

    pub(super) struct Lease<'a> {
        log: &'a dyn SessionLog,
        pub(super) writer: Writer,
    }

    impl<'a> Lease<'a> {
        pub(super) fn acquire(log: &'a dyn SessionLog) -> Result<Self, SessionError> {
            let writer = log.acquire(&uuid::Uuid::new_v4().to_string(), ADMIN_LEASE)?;
            Ok(Self { log, writer })
        }
    }

    impl Drop for Lease<'_> {
        fn drop(&mut self) {
            let _ = self.log.release(&self.writer);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Msg;
    use crate::session::store::{InMemorySessionLogStore, SessionLogStore};
    use crate::session::SessionKey;
    use std::sync::Arc;

    fn log() -> Arc<dyn SessionLog> {
        InMemorySessionLogStore::new()
            .open(&SessionKey::new(Some("u"), "a", "s").unwrap(), None)
            .unwrap()
    }

    fn initial() -> impl FnOnce() -> AgentState {
        || AgentState::with_session_id("s").with_user_id(Some("u".into()))
    }

    fn commit_event(log: &Arc<dyn SessionLog>, kind: &str, payload: Value) {
        let writer = log.acquire("w", Duration::from_secs(60)).unwrap();
        let seq = log.head().unwrap().seq;
        let event = SessionEvent::new(
            uuid::Uuid::new_v4().to_string(),
            seq + 1,
            super::super::now_millis(),
            kind,
            None,
            None,
            true,
            serde_json::to_string(&payload).unwrap(),
        )
        .unwrap();
        log.commit(&writer, "b", seq, vec![event]).unwrap();
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
    fn read_without_baseline_returns_initial_state() {
        let log = log();
        let snapshot = read(log.as_ref(), initial()).unwrap();
        assert_eq!(snapshot.as_of_seq, 0);
        assert_eq!(snapshot.state.session_id, "s");
        assert!(log.head().unwrap().seq == 0, "只读不能写任何东西");
    }

    #[test]
    fn update_commits_checkpoint_and_new_messages() {
        let log = log();
        let snapshot = update(log.as_ref(), initial(), "seed", |state| {
            state.context.push(Msg::user("user", "你好"));
        })
        .unwrap();
        assert!(snapshot.as_of_seq > 0);
        assert_eq!(snapshot.state.context.len(), 1);

        let restored = SessionProjection::read(log.as_ref()).unwrap().restore().unwrap();
        assert_eq!(restored.context.len(), 1);
        assert_eq!(restored.context[0].text(), "你好");

        let kinds = kinds(&log);
        assert!(kinds.contains(&"message/user".to_string()));
        assert_eq!(kinds.last().map(String::as_str), Some("state/checkpoint"));
    }

    #[test]
    fn update_rejects_unresolved_tool_dispatch() {
        let log = log();
        update(log.as_ref(), initial(), "seed", |_| {}).unwrap();
        commit_event(&log, "tool/dispatch", json!({ "toolCallId": "call-1" }));

        let error = update(log.as_ref(), initial(), "mischief", |state| {
            state.cur_iter += 1;
        })
        .unwrap_err();
        assert!(matches!(error, SessionError::Log(_)), "必须拒绝：{error}");
    }

    #[test]
    fn update_rejects_context_change_while_interaction_pending() {
        let log = log();
        update(log.as_ref(), initial(), "seed", |_| {}).unwrap();
        commit_event(
            &log,
            "interaction/requested",
            json!({ "requestId": "call-1", "kind": "external_execution" }),
        );

        // 不动 context 的管理性变更仍然允许。
        update(log.as_ref(), initial(), "bump", |state| state.cur_iter += 1).unwrap();

        // 换 context 则必须拒绝。
        let error = update(log.as_ref(), initial(), "rewrite", |state| {
            state.context.push(Msg::user("user", "插一句"));
        })
        .unwrap_err();
        assert!(matches!(error, SessionError::Log(_)), "必须拒绝：{error}");
    }

    #[test]
    fn update_rejects_identity_change_and_blank_reason() {
        let log = log();
        let error = update(log.as_ref(), initial(), "   ", |_| {}).unwrap_err();
        assert!(matches!(error, SessionError::Invalid(_)));

        let error = update(log.as_ref(), initial(), "rename", |state| {
            state.session_id = "other".into();
        })
        .unwrap_err();
        assert!(matches!(error, SessionError::Invalid(_)));

        // 失败不能留下任何事件。
        assert_eq!(log.head().unwrap().seq, 0);
    }

    #[test]
    fn replace_requires_current_prefix() {
        let log = log();
        let first = update(log.as_ref(), initial(), "seed", |state| {
            state.context.push(Msg::user("user", "第一句"));
        })
        .unwrap();

        // 别处又推进了一次：旧快照必须被拒绝。
        update(log.as_ref(), initial(), "elsewhere", |state| {
            state.context.push(Msg::user("user", "别处"));
        })
        .unwrap();
        let error = replace(log.as_ref(), initial(), "restore old", first.clone()).unwrap_err();
        assert!(matches!(error, SessionError::Log(_)), "必须拒绝：{error}");

        // 用最新前缀的快照则可以整份替换。
        let latest = read(log.as_ref(), initial()).unwrap();
        let mut edited = latest.clone();
        edited.state.context = vec![Msg::user("user", "只有这一句")];
        let replaced = replace(log.as_ref(), initial(), "rewrite", edited).unwrap();
        assert_eq!(replaced.state.context.len(), 1);
        assert_eq!(replaced.state.context[0].text(), "只有这一句");

        let restored = SessionProjection::read(log.as_ref()).unwrap().restore().unwrap();
        assert_eq!(restored.context.len(), 1);
        assert_eq!(restored.context[0].text(), "只有这一句");
        assert!(kinds(&log).contains(&"context/replaced".to_string()));
    }

    #[test]
    fn sub_context_changes_are_recorded_as_facts() {
        let log = log();
        update(log.as_ref(), initial(), "permit", |state| {
            state.permission_context.deny_rules.insert(
                "bash".into(),
                vec![crate::permission::PermissionRule {
                    tool_name: "bash".into(),
                    rule_content: Some("rm -rf".into()),
                    behavior: crate::permission::PermissionBehavior::Deny,
                    source: "session".into(),
                }],
            );
        })
        .unwrap();
        assert!(kinds(&log).contains(&"permission/changed".to_string()));
    }
}
