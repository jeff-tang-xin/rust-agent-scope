//! 中断控制（对应 Java `io.agentscope.core.interruption`）。
//!
//! 运行时协作中断信号：**属于单次执行，绝不入会话状态**（Java `InterruptControl`
//! 类注释明确此边界）。三个类型：
//!
//! - [`InterruptSource`]：中断来源（用户 / 工具 / 系统）；
//! - [`InterruptControl`]：一次执行持有的可变信号（触发 / 查询 / 复位）；
//! - [`InterruptContext`]：触发时刻的不可变快照（含被中断的待执行工具调用）。
//!
//! 与 agent 循环的接线（Java `RunControl` 持有 `InterruptControl` 并在阶段边界
//! 检查）属 agent 运行控制批次，本模块只提供信号本体。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::message::{ContentBlock, Msg};

/// 中断来源（对应 Java `InterruptSource`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum InterruptSource {
    /// 用户请求触发。
    #[default]
    User,
    /// 工具执行逻辑触发。
    Tool,
    /// 系统触发（超时、资源限制等）。
    System,
}

/// 当前 epoch 毫秒（对齐 event 层的时间表示，不引 chrono）。
pub(crate) fn now_epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 一次执行持有的协作中断信号（对应 Java `InterruptControl`）。
///
/// 线程安全：中断标志用原子量（高频读），来源与用户消息用 `Mutex`（低频写）。
/// **不派生 `Clone`**——信号属于单次执行，克隆会让两个持有者共享状态却以为是
/// 各自的；需要跨任务共享时用 `Arc<InterruptControl>`。
#[derive(Debug, Default)]
pub struct InterruptControl {
    flag: AtomicBool,
    inner: Mutex<InterruptInner>,
}

#[derive(Debug, Default)]
struct InterruptInner {
    source: InterruptSource,
    user_message: Option<Msg>,
}

impl InterruptControl {
    /// 初始为「未中断」状态。
    pub fn new() -> Self {
        Self::default()
    }

    /// 触发中断信号（对应 Java `trigger(src, msg)`；Java 侧 `src == null` 视为
    /// USER，Rust 侧由调用方直接传值）。
    pub fn trigger(&self, source: InterruptSource, user_message: Option<Msg>) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.source = source;
            if user_message.is_some() {
                inner.user_message = user_message;
            }
        }
        self.flag.store(true, Ordering::Release);
    }

    /// 是否已中断且未复位。
    pub fn is_interrupted(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    /// 当前中断来源（未触发时为默认值 [`InterruptSource::User`]）。
    pub fn source(&self) -> InterruptSource {
        self.inner
            .lock()
            .ok()
            .map(|inner| inner.source)
            .unwrap_or_default()
    }

    /// 与中断关联的用户消息（未设置时为 `None`）。
    pub fn user_message(&self) -> Option<Msg> {
        self.inner.lock().ok().and_then(|inner| inner.user_message.clone())
    }

    /// 复位到初始（未中断）状态。
    pub fn reset(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.user_message = None;
            inner.source = InterruptSource::User;
        }
        self.flag.store(false, Ordering::Release);
    }

    /// 生成当前信号的不可变快照。
    pub fn to_context(&self) -> InterruptContext {
        let (source, user_message) = match self.inner.lock() {
            Ok(inner) => (inner.source, inner.user_message.clone()),
            Err(_) => (InterruptSource::User, None),
        };
        InterruptContext {
            source,
            timestamp: now_epoch_millis(),
            user_message,
            pending_tool_calls: Vec::new(),
        }
    }
}

/// 中断快照（对应 Java `InterruptContext`）。
///
/// 捕获中断发生的时间与原因：来源、时间戳、关联的用户消息（如有）、以及中断时
/// 仍待执行的工具调用。
#[derive(Debug, Clone, Default)]
pub struct InterruptContext {
    /// 中断来源。
    pub source: InterruptSource,
    /// 中断时刻的 epoch 毫秒。
    pub timestamp: u64,
    /// 触发中断的用户消息（如有）。
    pub user_message: Option<Msg>,
    /// 中断时仍待执行的工具调用（`ContentBlock::ToolUse` 变体）。
    pub pending_tool_calls: Vec<ContentBlock>,
}

impl InterruptContext {
    /// 以当前时刻构建快照。
    pub fn new(source: InterruptSource) -> Self {
        Self {
            source,
            timestamp: now_epoch_millis(),
            user_message: None,
            pending_tool_calls: Vec::new(),
        }
    }

    /// 覆盖时间戳（重放 / 测试场景）。
    pub fn with_timestamp(mut self, timestamp: u64) -> Self {
        self.timestamp = timestamp;
        self
    }

    /// 关联用户消息。
    pub fn with_user_message(mut self, user_message: Option<Msg>) -> Self {
        self.user_message = user_message;
        self
    }

    /// 记录中断时仍待执行的工具调用。
    pub fn with_pending_tool_calls(mut self, pending: Vec<ContentBlock>) -> Self {
        self.pending_tool_calls = pending;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_state_is_not_interrupted() {
        let control = InterruptControl::new();
        assert!(!control.is_interrupted());
        assert_eq!(control.source(), InterruptSource::User);
        assert!(control.user_message().is_none());
    }

    #[test]
    fn trigger_sets_flag_source_and_message() {
        let control = InterruptControl::new();
        let msg = Msg::user("user", "stop now");
        control.trigger(InterruptSource::Tool, Some(msg.clone()));
        assert!(control.is_interrupted());
        assert_eq!(control.source(), InterruptSource::Tool);
        assert_eq!(control.user_message(), Some(msg));
    }

    #[test]
    fn trigger_without_message_keeps_previous_message() {
        // 对齐 Java：msg == null 时不清空已设置的用户消息。
        let control = InterruptControl::new();
        control.trigger(InterruptSource::User, Some(Msg::user("user", "first")));
        control.trigger(InterruptSource::System, None);
        assert_eq!(control.source(), InterruptSource::System);
        assert!(control.user_message().is_some());
    }

    #[test]
    fn reset_clears_everything() {
        let control = InterruptControl::new();
        control.trigger(InterruptSource::System, Some(Msg::user("user", "bye")));
        control.reset();
        assert!(!control.is_interrupted());
        assert_eq!(control.source(), InterruptSource::User);
        assert!(control.user_message().is_none());
    }

    #[test]
    fn to_context_snapshots_current_signal() {
        let control = InterruptControl::new();
        control.trigger(InterruptSource::User, Some(Msg::user("user", "halt")));
        let ctx = control.to_context();
        assert_eq!(ctx.source, InterruptSource::User);
        assert!(ctx.user_message.is_some());
        assert!(ctx.pending_tool_calls.is_empty());
        // 快照之后复位不影响已生成的快照。
        control.reset();
        assert_eq!(ctx.source, InterruptSource::User);
        assert!(ctx.user_message.is_some());
    }

    #[test]
    fn context_builder_style_fields() {
        let tool_use = ContentBlock::ToolUse {
            id: "call-1".into(),
            name: "web_search".into(),
            input: serde_json::json!({"query": "rust"}),
            state: None,
            content: None,
        };
        let ctx = InterruptContext::new(InterruptSource::System)
            .with_timestamp(1_700_000_000_000)
            .with_pending_tool_calls(vec![tool_use]);
        assert_eq!(ctx.timestamp, 1_700_000_000_000);
        assert_eq!(ctx.pending_tool_calls.len(), 1);
    }
}
