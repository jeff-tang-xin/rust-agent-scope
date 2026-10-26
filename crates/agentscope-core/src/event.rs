//! 类型化事件流（对应 Java 版 `io.agentscope.core.event`）。
//!
//! 复刻要点：
//! - **信封模式**：每个事件携带公共头（id / created_at / source / metadata）+
//!   具体载荷 [`AgentEventKind`]（Java 用抽象基类 + 子类，Rust 用 struct + 枚举组合）。
//! - **三段式块流**：文本 / 思考 / 数据块均为 Start → Delta* → End，
//!   工具调用与工具结果同理——前端可按 blockId / toolCallId 定位聚合。
//! - **replyId 贯穿**：一次 `stream_events()` 调用产生一个 reply_id，
//!   该次执行的所有事件都携带它，用于多路复用时区分归属。
//! - **HITL 事件**：`RequireUserConfirm` / `UserConfirmResult`、
//!   `RequireExternalExecution` / `ExternalExecutionResult` 支撑确认与外部执行回路。
//!
//! Java 的 32 种事件类型以 [`AgentEventType`] 枚举表达，[`AgentEvent::event_type`]
//! 提供穷尽映射（新增变体时编译器强制补全）。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

use crate::message::{ContentBlock, GenerateReason, Msg, ToolResultState};
use crate::model::Usage;

/// 事件发射器：向 `stream_events()` 的事件流注入事件
/// （对应 Java `AgentEventEmitter` 函数式接口；工具执行代码可通过它
/// 发出自定义事件，如子 agent 暴露通知）。
pub type AgentEventEmitter = tokio::sync::mpsc::Sender<AgentEvent>;

/// 用户对工具调用确认的结果（对应 Java `ConfirmResult`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfirmResult {
    pub confirmed: bool,
    /// 被确认的工具调用。
    pub tool_call: ContentBlock,
    /// 批准时用户选择的建议规则（如「始终允许」→ allow 规则），批准后
    /// 写入会话权限上下文（对应 Java `permissionEngine.addRule`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<Vec<crate::permission::PermissionRule>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// agent 事件信封：公共头 + 具体载荷。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentEvent {
    /// 事件 id（UUID，无横线，对齐 Java 构造器）。
    pub id: String,
    /// 创建时间（ISO-8601 UTC）。
    pub created_at: String,
    /// 事件来源（通常是 agent 名；子 agent 转发事件带来源路径）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub metadata: serde_json::Map<String, Value>,
    #[serde(flatten)]
    pub kind: AgentEventKind,
}

/// 事件载荷（对应 Java 的 32 个具体事件类）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AgentEventKind {
    // ---------- 生命周期 ----------
    /// 一次 agent 执行开始。
    AgentStart {
        reply_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        name: String,
        role: String,
    },
    /// 一次 agent 执行结束（在 AgentResult 之后，标志事件流收尾）。
    AgentEnd { reply_id: String },
    /// 执行结束，携带最终回复消息。
    AgentResult { result: Msg },

    // ---------- 模型调用 ----------
    /// 一轮模型调用开始。
    ModelCallStart { reply_id: String },
    /// 一轮模型调用结束，携带本轮用量。
    ModelCallEnd {
        reply_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },

    // ---------- 文本块流（Start → Delta* → End） ----------
    TextBlockStart { reply_id: String, block_id: String },
    TextBlockDelta {
        reply_id: String,
        block_id: String,
        delta: String,
    },
    TextBlockEnd { reply_id: String, block_id: String },

    // ---------- 思考块流 ----------
    ThinkingBlockStart { reply_id: String, block_id: String },
    ThinkingBlockDelta {
        reply_id: String,
        block_id: String,
        delta: String,
    },
    ThinkingBlockEnd { reply_id: String, block_id: String },

    // ---------- 数据块流 ----------
    DataBlockStart { reply_id: String, block_id: String },
    DataBlockDelta {
        reply_id: String,
        block_id: String,
        delta: String,
    },
    DataBlockEnd { reply_id: String, block_id: String },

    // ---------- 工具调用流 ----------
    ToolCallStart {
        reply_id: String,
        tool_call_id: String,
        tool_call_name: String,
    },
    /// 工具调用参数增量（JSON 分片；非流式执行时为一次完整序列化）。
    ToolCallDelta {
        reply_id: String,
        tool_call_id: String,
        tool_call_name: String,
        delta: String,
    },
    ToolCallEnd {
        reply_id: String,
        tool_call_id: String,
        tool_call_name: String,
    },

    // ---------- 工具结果流 ----------
    ToolResultStart {
        reply_id: String,
        tool_call_id: String,
        tool_call_name: String,
    },
    /// 长任务的执行进度（工具通过 emitter 主动推送）。
    ToolProgress {
        reply_id: String,
        tool_call_id: String,
        tool_call_name: String,
        content: ContentBlock,
    },
    ToolResultTextDelta {
        reply_id: String,
        tool_call_id: String,
        tool_call_name: String,
        delta: String,
    },
    ToolResultDataDelta {
        reply_id: String,
        tool_call_id: String,
        tool_call_name: String,
        data: ContentBlock,
    },
    /// 工具结果结束，携带最终状态（success / error / denied / ...）。
    ToolResultEnd {
        reply_id: String,
        tool_call_id: String,
        tool_call_name: String,
        state: ToolResultState,
    },

    // ---------- 提示块 ----------
    /// RAG 等机制注入的提示块。
    HintBlock {
        reply_id: String,
        block_id: String,
        hint_source: String,
        hint: String,
    },

    // ---------- HITL：用户确认回路 ----------
    /// 权限引擎请求用户确认工具调用（对应 GenerateReason::PermissionAsking）。
    RequireUserConfirm {
        reply_id: String,
        tool_calls: Vec<ContentBlock>,
    },
    /// 用户确认结果回传。
    UserConfirmResult {
        reply_id: String,
        confirm_results: Vec<ConfirmResult>,
    },

    // ---------- 外部执行回路 ----------
    /// 工具调用需在 agent 运行时之外执行（如浏览器操作）。
    RequireExternalExecution {
        reply_id: String,
        tool_calls: Vec<ContentBlock>,
    },
    /// 外部执行结果回传。
    ExternalExecutionResult {
        reply_id: String,
        tool_results: Vec<ContentBlock>,
    },

    // ---------- 控制流 ----------
    /// 中间件 / Hook 请求停止（携带停止原因，供恢复流程使用）。
    RequestStop {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        generate_reason: Option<GenerateReason>,
    },
    /// 达到最大迭代次数。
    ExceedMaxIters {
        reply_id: String,
        max_iters: u32,
        current_iter: u32,
    },
    /// 所有工具调用都被拒绝。
    AllToolsDenied { denied_tool_calls: Vec<ContentBlock> },

    // ---------- 子 agent ----------
    /// 子 agent 暴露为可寻址线程（对应 Java SubagentExposedEvent）。
    SubagentExposed {
        subagent_id: String,
        agent_id: String,
        session_id: String,
        label: String,
    },

    // ---------- 自定义 ----------
    /// 工具 / 扩展通过 emitter 发出的自定义事件。
    Custom {
        name: String,
        #[serde(default)]
        value: serde_json::Map<String, Value>,
    },
}

/// 事件类型标识（对应 Java `AgentEventType`，32 值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AgentEventType {
    AgentStart,
    AgentEnd,
    AgentResult,
    ModelCallStart,
    ModelCallEnd,
    TextBlockStart,
    TextBlockDelta,
    TextBlockEnd,
    ThinkingBlockStart,
    ThinkingBlockDelta,
    ThinkingBlockEnd,
    DataBlockStart,
    DataBlockDelta,
    DataBlockEnd,
    ToolCallStart,
    ToolCallDelta,
    ToolCallEnd,
    ToolResultStart,
    ToolProgress,
    ToolResultTextDelta,
    ToolResultDataDelta,
    ToolResultEnd,
    ExceedMaxIters,
    RequireUserConfirm,
    RequireExternalExecution,
    UserConfirmResult,
    ExternalExecutionResult,
    RequestStop,
    SubagentExposed,
    HintBlock,
    AllToolsDenied,
    Custom,
}

impl AgentEvent {
    /// 以当前时间构造事件（id 与 created_at 自动生成）。
    pub fn new(kind: AgentEventKind) -> Self {
        Self {
            id: Uuid::new_v4().simple().to_string(),
            created_at: now_iso8601(),
            source: None,
            metadata: serde_json::Map::new(),
            kind,
        }
    }

    /// 设置事件来源（builder 风格）。
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// 事件类型（对齐 Java `getType()`，match 穷尽映射）。
    pub fn event_type(&self) -> AgentEventType {
        match &self.kind {
            AgentEventKind::AgentStart { .. } => AgentEventType::AgentStart,
            AgentEventKind::AgentEnd { .. } => AgentEventType::AgentEnd,
            AgentEventKind::AgentResult { .. } => AgentEventType::AgentResult,
            AgentEventKind::ModelCallStart { .. } => AgentEventType::ModelCallStart,
            AgentEventKind::ModelCallEnd { .. } => AgentEventType::ModelCallEnd,
            AgentEventKind::TextBlockStart { .. } => AgentEventType::TextBlockStart,
            AgentEventKind::TextBlockDelta { .. } => AgentEventType::TextBlockDelta,
            AgentEventKind::TextBlockEnd { .. } => AgentEventType::TextBlockEnd,
            AgentEventKind::ThinkingBlockStart { .. } => AgentEventType::ThinkingBlockStart,
            AgentEventKind::ThinkingBlockDelta { .. } => AgentEventType::ThinkingBlockDelta,
            AgentEventKind::ThinkingBlockEnd { .. } => AgentEventType::ThinkingBlockEnd,
            AgentEventKind::DataBlockStart { .. } => AgentEventType::DataBlockStart,
            AgentEventKind::DataBlockDelta { .. } => AgentEventType::DataBlockDelta,
            AgentEventKind::DataBlockEnd { .. } => AgentEventType::DataBlockEnd,
            AgentEventKind::ToolCallStart { .. } => AgentEventType::ToolCallStart,
            AgentEventKind::ToolCallDelta { .. } => AgentEventType::ToolCallDelta,
            AgentEventKind::ToolCallEnd { .. } => AgentEventType::ToolCallEnd,
            AgentEventKind::ToolResultStart { .. } => AgentEventType::ToolResultStart,
            AgentEventKind::ToolProgress { .. } => AgentEventType::ToolProgress,
            AgentEventKind::ToolResultTextDelta { .. } => AgentEventType::ToolResultTextDelta,
            AgentEventKind::ToolResultDataDelta { .. } => AgentEventType::ToolResultDataDelta,
            AgentEventKind::ToolResultEnd { .. } => AgentEventType::ToolResultEnd,
            AgentEventKind::HintBlock { .. } => AgentEventType::HintBlock,
            AgentEventKind::ExceedMaxIters { .. } => AgentEventType::ExceedMaxIters,
            AgentEventKind::RequireUserConfirm { .. } => AgentEventType::RequireUserConfirm,
            AgentEventKind::RequireExternalExecution { .. } => {
                AgentEventType::RequireExternalExecution
            }
            AgentEventKind::UserConfirmResult { .. } => AgentEventType::UserConfirmResult,
            AgentEventKind::ExternalExecutionResult { .. } => {
                AgentEventType::ExternalExecutionResult
            }
            AgentEventKind::RequestStop { .. } => AgentEventType::RequestStop,
            AgentEventKind::SubagentExposed { .. } => AgentEventType::SubagentExposed,
            AgentEventKind::AllToolsDenied { .. } => AgentEventType::AllToolsDenied,
            AgentEventKind::Custom { .. } => AgentEventType::Custom,
        }
    }
}

/// 当前 UTC 时间的 ISO-8601 / RFC 3339 字符串（如 `2026-10-22T07:30:00Z`）。
///
/// 对齐 Java `Instant.now().toString()` 的形态；不引入 chrono，
/// 用 civil-date 算法从 Unix 秒换算（Howard Hinnant 算法）。
fn now_iso8601() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    epoch_to_iso8601(secs)
}

/// Unix 秒 → ISO-8601 UTC 字符串（civil-from-days，Howard Hinnant 算法）。
pub(crate) fn epoch_to_iso8601(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let (h, min, s) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 事件类型穷尽映射：抽验各分类变体。
    #[test]
    fn event_type_mapping() {
        let cases = [
            (AgentEventKind::AgentStart { reply_id: "r".into(), session_id: None, name: "a".into(), role: "assistant".into() }, AgentEventType::AgentStart),
            (AgentEventKind::ModelCallEnd { reply_id: "r".into(), usage: None }, AgentEventType::ModelCallEnd),
            (AgentEventKind::TextBlockDelta { reply_id: "r".into(), block_id: "b".into(), delta: "x".into() }, AgentEventType::TextBlockDelta),
            (AgentEventKind::ToolCallDelta { reply_id: "r".into(), tool_call_id: "c".into(), tool_call_name: "t".into(), delta: "{}".into() }, AgentEventType::ToolCallDelta),
            (AgentEventKind::ToolResultEnd { reply_id: "r".into(), tool_call_id: "c".into(), tool_call_name: "t".into(), state: crate::message::ToolResultState::Success }, AgentEventType::ToolResultEnd),
            (AgentEventKind::RequireUserConfirm { reply_id: "r".into(), tool_calls: vec![] }, AgentEventType::RequireUserConfirm),
            (AgentEventKind::Custom { name: "n".into(), value: Default::default() }, AgentEventType::Custom),
        ];
        for (kind, expected) in cases {
            let ev = AgentEvent::new(kind).with_source("assistant");
            assert_eq!(ev.event_type(), expected);
            assert_eq!(ev.source.as_deref(), Some("assistant"));
            assert!(!ev.id.is_empty());
            assert!(ev.created_at.ends_with('Z'));
        }
    }

    /// 信封 + 载荷 serde 往返：type discriminator 为 SCREAMING_SNAKE。
    #[test]
    fn serde_roundtrip_with_type_tag() {
        let ev = AgentEvent::new(AgentEventKind::TextBlockDelta {
            reply_id: "reply_1".into(),
            block_id: "block_1".into(),
            delta: "你好".into(),
        });
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["type"], "TEXT_BLOCK_DELTA");
        assert_eq!(json["delta"], "你好");
        let back: AgentEvent = serde_json::from_value(json).unwrap();
        assert_eq!(back, ev);
    }

    /// HITL 确认回路事件往返（ConfirmResult + ToolUse 块）。
    #[test]
    fn confirm_flow_events_roundtrip() {
        let tool_use = ContentBlock::ToolUse {
            id: "call_1".into(),
            name: "shell".into(),
            input: json!({"cmd": "rm -rf /"}),
            state: Some(crate::message::ToolCallState::Asking),
            content: None,
        };
        let ev = AgentEvent::new(AgentEventKind::UserConfirmResult {
            reply_id: "reply_1".into(),
            confirm_results: vec![ConfirmResult {
                confirmed: false,
                tool_call: tool_use,
                rules: None,
                reason: Some("危险命令".into()),
            }],
        });
        let json = serde_json::to_string(&ev).unwrap();
        let back: AgentEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ev);
        assert_eq!(back.event_type(), AgentEventType::UserConfirmResult);
    }

    /// ISO-8601 时间戳格式（已知 Unix 秒 → 固定输出）。
    #[test]
    fn iso8601_format() {
        // 2026-10-22T00:00:00Z = 1792627200
        assert_eq!(epoch_to_iso8601(1_792_627_200), "2026-10-22T00:00:00Z");
        // epoch 起点
        assert_eq!(epoch_to_iso8601(0), "1970-01-01T00:00:00Z");
        // 闰年 2024-02-29 = 1709164800
        assert_eq!(epoch_to_iso8601(1_709_164_800), "2024-02-29T00:00:00Z");
        // 当天中段
        assert_eq!(epoch_to_iso8601(1_792_630_861), "2026-10-22T01:01:01Z");
    }
}
