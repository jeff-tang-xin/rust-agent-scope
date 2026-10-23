//! 离线迁移 / 分叉原语（对应 Java `SessionMigration`）。
//!
//! 源归档保留；检查点从不虚构过去的事实——导入的是**当时**的状态基线，
//! 不补写任何「看起来应该有」的事件。

use serde_json::Value;
use std::time::Duration;

use super::events::SessionEvent;
use super::interactions::PendingInteractions;
use super::journal::{sha256_hex, SessionLog};
use super::projection::SessionProjection;
use super::{now_millis, SessionError, SessionKey};
use crate::state::AgentState;

/// 写者租约时长（迁移是短事务，2 分钟足够）。
const LEASE: Duration = Duration::from_secs(120);

/// 把一份既有状态作为基线导入**空**日志（对应 Java `importBaseline`）。
///
/// 幂等：目标日志第一条事件已是 `migration/baseline` 且 `sourceHash` 相同时直接成功
/// （重试安全）；目标日志非空且不是同一基线时报错——迁移目的地必须从零开始。
pub fn import_baseline(
    target: &dyn SessionLog,
    state: &AgentState,
    source: &str,
    source_version: i64,
) -> Result<(), SessionError> {
    let json = state
        .to_json()
        .map_err(|error| SessionError::log(format!("state encoding failed: {error}")))?;
    let hash = sha256_hex(json.as_bytes());
    let writer = target.acquire(&uuid::Uuid::new_v4().to_string(), LEASE)?;
    let result = import_locked(target, &writer, &json, &hash, source, source_version);
    target.release(&writer)?;
    result
}

fn import_locked(
    target: &dyn SessionLog,
    writer: &super::journal::Writer,
    json: &str,
    hash: &str,
    source: &str,
    source_version: i64,
) -> Result<(), SessionError> {
    let head = target.head()?;
    if head.seq > 0 {
        let first = target.read_after(0, 1)?;
        if let Some(first) = first.first() {
            if first.kind == "migration/baseline" {
                let data = first.data()?;
                if data.get("sourceHash").and_then(Value::as_str) == Some(hash) {
                    return Ok(());
                }
            }
        }
        return Err(SessionError::invalid("Migration destination is not empty"));
    }
    let payload = serde_json::json!({
        "stateJson": json,
        "coverage": "baseline_only",
        "source": source,
        "sourceVersion": source_version,
        "sourceHash": hash,
    });
    let event = SessionEvent::new(
        uuid::Uuid::new_v4().to_string(),
        1,
        now_millis(),
        "migration/baseline",
        Some(writer.owner.clone()),
        None,
        true,
        payload.to_string(),
    )?;
    target.commit(
        writer,
        &format!("baseline-{hash}"),
        0,
        vec![event],
    )?;
    Ok(())
}

/// 导出的状态快照：状态 + 投影覆盖到的序号（分叉时作为 `sourceVersion`）。
#[derive(Debug, Clone)]
pub struct ExportedSnapshot {
    pub state: AgentState,
    pub as_of_seq: i64,
}

/// 从日志导出状态（对应 Java `exportSnapshot`）。
///
/// 前置条件缺一不可：源会话必须暂停（无写者）、无结果未知的工具调用、
/// 无进行中的运行、无未决交互、有可恢复的基线——否则导出的状态是「半截事实」。
pub fn export_snapshot(source: &dyn SessionLog) -> Result<ExportedSnapshot, SessionError> {
    if source.head()?.owner.is_some() {
        return Err(SessionError::invalid(
            "Pause the source session before migration",
        ));
    }
    // 占住写者位：导出期间不允许新的运行把日志写走
    let writer = source.acquire(&uuid::Uuid::new_v4().to_string(), LEASE)?;
    let result = (|| {
        let projection = SessionProjection::read(source)?;
        if !projection.uncertain_tool_calls().is_empty()
            || !projection.active_runs().is_empty()
        {
            return Err(SessionError::invalid(
                "Reconcile interrupted execution before exporting",
            ));
        }
        if !PendingInteractions::read(source)?.is_empty() {
            return Err(SessionError::invalid(
                "Resolve pending interactions before exporting or forking",
            ));
        }
        let state = projection
            .restore()
            .ok_or_else(|| SessionError::invalid("No recoverable baseline"))?;
        Ok(ExportedSnapshot {
            state,
            as_of_seq: projection.as_of_seq(),
        })
    })();
    source.release(&writer)?;
    result
}

/// 把源会话分叉到目标日志（对应 Java `fork`）：导出快照 → 改身份 → 作为基线导入。
///
/// 目标状态的身份换成 `destination_key` 的（身份不可改原则只约束既有会话，
/// 分叉产生的是新会话，身份在导入前重写）。
pub fn fork(
    source: &dyn SessionLog,
    destination: &dyn SessionLog,
    destination_key: &SessionKey,
    source_reference: &str,
) -> Result<(), SessionError> {
    let snapshot = export_snapshot(source)?;
    // 分叉产生的是新会话：身份在导入前重写（身份不可改原则只约束既有会话）
    let mut state = snapshot.state;
    state.session_id = destination_key.session_id.clone();
    state.user_id = destination_key.user_id.clone();
    import_baseline(
        destination,
        &state,
        &format!("fork:{source_reference}"),
        snapshot.as_of_seq,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::store::{InMemorySessionLogStore, SessionLogStore};

    fn log_of(store: &InMemorySessionLogStore, session: &str) -> std::sync::Arc<dyn SessionLog> {
        store
            .open(
                &SessionKey::new(Some("u"), "a", session).unwrap(),
                None,
            )
            .unwrap()
    }

    fn state(session: &str) -> AgentState {
        let mut state = AgentState::with_session_id(session);
        state.context.push(crate::message::Msg::user("user", "历史"));
        state
    }

    #[test]
    fn import_baseline_is_idempotent_for_same_hash() {
        let store = InMemorySessionLogStore::new();
        let target = log_of(&store, "t1");
        // 同一状态实例导入两次：消息 id 相同 → hash 相同 → 幂等成功
        let baseline = state("t1");
        import_baseline(target.as_ref(), &baseline, "legacy", 7).unwrap();
        import_baseline(target.as_ref(), &baseline, "legacy", 7).unwrap();
        // 不同状态（消息 id 不同 → hash 不同）：目的地非空，报错
        let error = import_baseline(target.as_ref(), &state("t1"), "other", 1).unwrap_err();
        assert!(error.to_string().contains("not empty"), "{error}");
    }

    #[test]
    fn export_snapshot_requires_quiesced_source() {
        let store = InMemorySessionLogStore::new();
        let log = log_of(&store, "t2");
        import_baseline(log.as_ref(), &state("t2"), "legacy", 1).unwrap();

        // 有写者持有 → 必须先暂停
        let writer = log.acquire("w1", LEASE).unwrap();
        let error = export_snapshot(log.as_ref()).unwrap_err();
        assert!(error.to_string().contains("Pause"), "{error}");
        log.release(&writer).unwrap();

        // 静默后可导出，状态与序号一致
        let snapshot = export_snapshot(log.as_ref()).unwrap();
        assert_eq!(snapshot.as_of_seq, 1);
        assert_eq!(snapshot.state.session_id, "t2");
    }

    #[test]
    fn fork_rewrites_identity_and_keeps_history() {
        let store = InMemorySessionLogStore::new();
        let source = log_of(&store, "src");
        import_baseline(source.as_ref(), &state("src"), "legacy", 3).unwrap();

        let destination = log_of(&store, "dst");
        let destination_key = SessionKey::new(Some("u2"), "a", "dst").unwrap();
        fork(
            source.as_ref(),
            destination.as_ref(),
            &destination_key,
            "src@3",
        )
        .unwrap();

        let snapshot = export_snapshot(destination.as_ref()).unwrap();
        // 身份已重写，历史保留
        assert_eq!(snapshot.state.session_id, "dst");
        assert_eq!(snapshot.state.user_id.as_deref(), Some("u2"));
        assert_eq!(snapshot.state.context.len(), 1);
        // 基线事件记录了来源与版本
        let first = destination.read_after(0, 1).unwrap();
        let data = first[0].data().unwrap();
        assert_eq!(
            data.get("source").and_then(Value::as_str),
            Some("fork:src@3")
        );
        assert_eq!(data.get("sourceVersion").and_then(Value::as_i64), Some(1));
    }
}
