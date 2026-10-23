//! 会话持久化（对应 Java `io.agentscope.core.session`，29 个类）。
//!
//! Java 2.0 的会话不再是「整份状态快照覆盖写」，而是一条**只追加的事件日志**：
//! 每次模型调用、工具执行、任务状态变化都落成一条 [`SessionEvent`]，按 batch 提交，
//! 提交头是一次带围栏（fence）的 CAS。恢复时重放日志重建 `AgentState`。
//!
//! 本模块分两层：
//!
//! | 层 | 内容 | 对应 Java |
//! |----|------|-----------|
//! | 事件与身份 | [`SessionEvent`] / [`SessionEventTypes`] / [`SessionEventCodecRegistry`] / [`SessionKey`] / [`SessionHeader`] / [`SessionHistoryMode`] | `SessionEvent`、`SessionEventTypes`、`SessionEventCodecRegistry`、`SessionKey`、`SessionHeader`、`SessionHistoryMode` |
//! | 日志与存储 | [`AtomicSessionStorage`] / [`SessionLog`] / [`JournalSessionLog`] / [`SessionLogStore`] / [`InMemorySessionLogStore`] | `AtomicSessionStorage`、`SessionLog`、`JournalSessionLog`、`SessionLogStore`、`InMemorySessionLogStore` |
//!
//! 第三层是**视图与运维**（全部只读重放或有围栏写入）：
//!
//! | 视图 | 内容 | 对应 Java |
//! |------|------|-----------|
//! | [`SessionProjection`] | 状态快照 + 未决工具调用 + 活跃运行 | `SessionProjection` |
//! | [`PendingInteractions`] | 已请求 − 已解决；续跑前校验合法性 | `SessionInteractions` |
//! | [`SessionTurns`] | 逻辑回合（回合 ≠ 运行） | `SessionTurns` |
//! | [`SessionInbox`] | 持久化命令收件箱（独立子日志） | `SessionInbox` |
//! | [`state_access`] | 只读快照 / 有围栏的管理性变更 | `SessionStateAccess` |
//! | [`recovery`] | 取消未决交互 / 接收外部对账结论 | `SessionRecovery` |
//!
//! 第四层是**写入侧**（唯一会推进日志头的部分）：
//!
//! | 写入者 | 内容 | 对应 Java |
//! |--------|------|-----------|
//! | [`recorder::SessionRecorder`] | 持写者租约，把运行事实写成事件；`capture_state` 做状态快照 + diff | `SessionRecorder` |
//! | [`execution::SessionExecution`] | 消费收件箱命令：准入校验、注入转向指令、回合结算 | `SessionExecution` |
//!
//! 尚未复刻：`SessionTranscriptExport` / `SessionExportSink` / `SessionLogExporter`、
//! `SessionModelPolicy`、`SessionMigration`（旧格式迁移）。
//!
//! # 形态映射
//!
//! - Java 的 `SessionLogException`（RuntimeException）→ [`SessionError::Log`]；
//!   持久化失败必须向上传播，**绝不能**变成「模型/工具成功」的结果。
//! - Java 的 `SessionEventCodecRegistry.defaultRegistry()`（静态单例）→ Rust 侧不做全局单例
//!   （反模式禁令⑤），registry 由调用方创建并显式传入重放逻辑；默认空表。
//! - Java 的 `Iterable<SessionEvent>`（`Iterator.hasNext/next` 抛异常）→ Rust 的
//!   `Result<Vec<SessionEvent>>`：错误在返回值里，不在迭代过程中。
//! - Java 的阻塞 SPI（`boundedElastic` 调度）→ Rust 侧同步 trait，调用方可自行
//!   `spawn_blocking`；内存后端与文件后端的语义一致。

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::memory::Memory;
use crate::message::Msg;

pub mod events;
pub mod export;
pub mod execution;
pub mod inbox;
pub mod interactions;
pub mod journal;
pub mod key;
pub mod migration;
pub mod models;
pub mod projection;
pub mod recorder;
pub mod recovery;
pub mod state_access;
pub mod storage;
pub mod store;
pub mod turns;
pub mod views;

pub use events::{SessionEvent, SessionEventCodec, SessionEventCodecRegistry, SessionEventTypes};
pub use export::{transcript_entries, SessionExportSink, SessionLogExporter, TranscriptEntry};
pub use migration::{export_snapshot, fork, import_baseline, ExportedSnapshot};
pub use models::{RecordedModel, SessionModelPolicy};
pub use inbox::{Command, SessionInbox, Snapshot, INBOX_LEASE};
pub use interactions::{continuation_turn, PendingInteractions};
pub use journal::{sha256_hex, Head, JournalSessionLog, SessionLog, Writer};
pub use key::{SessionHeader, SessionKey};
pub use projection::SessionProjection;
pub use recovery::{cancel_pending, reconcile};
pub use state_access::{read as read_state, replace as replace_state, update as update_state, ADMIN_LEASE};
pub use storage::{AtomicSessionStorage, MemorySessionStorage, StorageValue};
pub use store::{InMemorySessionLogStore, SessionLogStore};
pub use turns::{SessionTurns, Turn};
pub use views::{transcript, Transcript};

/// 会话持久化错误（对应 Java `SessionLogException` + `IllegalArgumentException`）。
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// 日志层失败（提交冲突、链路损坏、围栏失效等），对应 `SessionLogException`。
    #[error("session log: {0}")]
    Log(String),
    /// 参数非法，对应 `IllegalArgumentException`。
    #[error("invalid session argument: {0}")]
    Invalid(String),
    /// 后端不支持该能力（对应 `UnsupportedOperationException`）。
    #[error("session backend does not support: {0}")]
    Unsupported(String),
    /// 存储 IO 失败。
    #[error("session io: {0}")]
    Io(#[from] std::io::Error),
    /// JSON 编解码失败。
    #[error("session json: {0}")]
    Serde(#[from] serde_json::Error),
}

impl SessionError {
    pub(crate) fn log(message: impl Into<String>) -> Self {
        Self::Log(message.into())
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }

    pub(crate) fn unsupported(message: impl Into<String>) -> Self {
        Self::Unsupported(message.into())
    }
}

/// 当前毫秒时间戳（对应 Java `System.currentTimeMillis()`）。
pub(crate) fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 会话历史的**唯一权威来源**（对应 Java `SessionHistoryMode`）。
///
/// 二选一：独立状态库持久化（LEGACY）或原生已提交历史（EVENT_LOG）。不允许混用。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionHistoryMode {
    /// 旧式：整份 `AgentState` 走 `AgentStateStore`，会话日志不参与恢复。
    Legacy,
    /// 新式（默认）：以已提交事件日志为唯一真相，`AgentState` 只是重放产物。
    #[default]
    EventLog,
}

// ---------------------------------------------------------------------------
// LEGACY：整份快照存取（阶段 1 产物，保留以支持简单场景与向后兼容）
// ---------------------------------------------------------------------------

/// 会话快照：可序列化的对话状态。
///
/// 这是阶段 1 的简化持久化（整份消息表覆盖写）。它与事件日志互斥：
/// 选择 [`SessionHistoryMode::EventLog`] 时不应再用快照恢复。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub messages: Vec<Msg>,
}

impl SessionSnapshot {
    /// 从消息序列构造快照。
    pub fn from_messages(messages: Vec<Msg>) -> Self {
        Self { messages }
    }
}

/// 把快照保存为 JSON 文件（父目录不存在时自动创建）。
pub fn save_to_file(snapshot: &SessionSnapshot, path: impl AsRef<Path>) -> Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(snapshot)?;
    std::fs::write(path, json)?;
    Ok(())
}

/// 从 JSON 文件加载快照。
pub fn load_from_file(path: impl AsRef<Path>) -> Result<SessionSnapshot> {
    let json = std::fs::read_to_string(path.as_ref())?;
    Ok(serde_json::from_str(&json)?)
}

/// 把快照按序恢复进记忆实现（不清空已有内容，需要干净会话请先 `Memory::clear`）。
pub async fn restore_into(memory: &dyn Memory, snapshot: &SessionSnapshot) -> Result<()> {
    for msg in &snapshot.messages {
        memory.add(msg.clone()).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::InMemoryMemory;
    use crate::message::ContentBlock;
    use std::sync::Arc;

    #[test]
    fn file_roundtrip() {
        let snapshot = SessionSnapshot::from_messages(vec![
            Msg::user("user", "你好"),
            Msg::assistant(
                "assistant",
                vec![ContentBlock::Text { text: "你好！".into() }],
            ),
        ]);

        let dir = std::env::temp_dir().join("agentscope-rs-session-test");
        let path = dir.join("roundtrip.json");
        save_to_file(&snapshot, &path).unwrap();
        let loaded = load_from_file(&path).unwrap();

        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.messages[0].text(), "你好");
        assert_eq!(loaded, snapshot);
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn restore_into_memory() {
        let snapshot = SessionSnapshot::from_messages(vec![
            Msg::user("user", "第一轮"),
            Msg::user("user", "第二轮"),
        ]);
        let memory = Arc::new(InMemoryMemory::new());
        restore_into(memory.as_ref(), &snapshot).await.unwrap();

        let msgs = memory.get_messages().await.unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1].text(), "第二轮");
    }

    #[test]
    fn history_mode_defaults_to_event_log() {
        assert_eq!(SessionHistoryMode::default(), SessionHistoryMode::EventLog);
        assert_eq!(
            serde_json::to_string(&SessionHistoryMode::Legacy).unwrap(),
            "\"legacy\""
        );
    }
}
