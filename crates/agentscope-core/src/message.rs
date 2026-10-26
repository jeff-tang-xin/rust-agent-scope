//! 消息模型（对应 Java 版 `io.agentscope.core.message` 的设计思路）。
//!
//! 复刻要点：
//! - 内容块化：一条消息由多个内容块组成，块类型覆盖文本 / 思考 / 图像 / 音频 / 视频 /
//!   工具调用 / 工具结果 / 提示 / 通用数据（对齐 Java `ContentBlock` sealed 层次）。
//! - 多模态来源抽象 [`Source`]：URL 或 Base64，块只引用来源（对齐 Java `Source` 层次）。
//! - 工具调用状态机：[`ToolCallState`]（权限门控 / HITL 流程）与
//!   [`ToolResultState`]（成功 / 错误 / 中断 / 拒绝 / 运行中 / 挂起）。
//! - 工具结果是块列表而非纯字符串：工具可以返回多模态内容。
//! - [`GenerateReason`]：agent 返回消息时说明"为什么停"，调用方据此决定下一步
//!   （继续执行 / 处理挂起工具 / 处理确认请求 / 恢复中断）。
//! - Java 的类继承用 Rust 枚举 + `match` 穷尽分派表达：新增块类型时编译器
//!   强制补全所有分派点。

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 消息角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// 多模态内容来源：URL 引用或 Base64 内联数据。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Source {
    /// Base64 内联数据（对应 Java `Base64Source`）。
    Base64 {
        media_type: String,
        data: String,
    },
    /// URL 引用（对应 Java `URLSource`）。
    Url {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
    },
}

/// 工具调用请求的状态（对应 Java `ToolCallState`）。
///
/// 支撑权限门控与 human-in-the-loop 流程：模型产出调用后先 `pending`，
/// 权限引擎询问用户时置 `asking`，获批 `allowed`，提交执行 `submitted`，完成 `finished`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolCallState {
    Pending,
    Asking,
    Allowed,
    Submitted,
    Finished,
}

/// 工具执行结果的状态（对应 Java `ToolResultState`）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolResultState {
    Success,
    Error,
    /// 执行被用户中断。
    Interrupted,
    /// 调用被权限系统或用户拒绝。
    Denied,
    /// 正在执行（占位结果；对齐 Java 构造器：state 缺省即为 RUNNING）。
    #[default]
    Running,
    /// 调用被挂起，等待 agent 运行时之外的一方提供真实结果（对应 Java SUSPENDED）。
    Suspended,
}

/// agent 返回消息的原因（对应 Java `GenerateReason`）。
///
/// 这是 ReAct 处理流程的状态载体：`Agent::call` 的返回值携带它，
/// 调用方据此决定后续动作（直接消费 / 处理挂起工具 / 二次 call 携带确认结果 / 恢复中断）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerateReason {
    /// 模型正常停止，任务完成。
    ModelStop,
    /// 模型返回了工具调用（框架将继续执行）。
    ToolCalls,
    /// 结构化输出完成。
    StructuredOutput,
    /// 工具执行被挂起，等待调用方提供结果后恢复。
    ToolSuspended,
    /// 推理阶段被 Hook 请求停止。
    ReasoningStopRequested,
    /// 行动阶段被 Hook 请求停止。
    ActingStopRequested,
    /// 权限引擎正在请求用户确认工具调用（消息中的 ToolUse 块 state=asking）。
    PermissionAsking,
    /// 中间件通过停止请求暂停了 agent（无参二次 call 恢复）。
    MiddlewareStopRequested,
    /// 所有工具调用都被拒绝且 Hook 请求停止。
    AllToolsDenied,
    /// 执行被用户中断。
    Interrupted,
    /// 达到最大迭代次数。
    MaxIterations,
    /// 工具声明了 return_direct，其结果直接作为最终回复。
    ToolReturnDirect,
}

/// 工具执行的客观事实（对应 Java `ToolExecutionDetails` record）。
///
/// 与模型可见文本分离：exit_code 缺席表示未知而非零；进程成功不等于任务验证通过。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolExecutionDetails {
    pub kind: String,
    pub outcome: ToolExecutionOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub output_truncated: bool,
}

/// 工具执行结果大类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionOutcome {
    Succeeded,
    Failed,
    Partial,
    Unknown,
}

/// 消息内容块。一条消息可由多个块组成。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// 纯文本块。
    Text { text: String },
    /// 思考 / 推理内容块（推理模型输出，对应 Java `ThinkingBlock`）。
    Thinking { thinking: String },
    /// 图像块（对应 Java `ImageBlock`）。
    Image {
        source: Source,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min_pixels: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_pixels: Option<u32>,
    },
    /// 音频块（对应 Java `AudioBlock`）。
    Audio { source: Source },
    /// 视频块（对应 Java `VideoBlock`）。
    Video {
        source: Source,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fps: Option<f32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_frames: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min_pixels: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_pixels: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total_pixels: Option<u32>,
    },
    /// 模型发起的工具调用请求。
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        /// 调用在权限 / 执行流程中的状态（未进入门控流程时为 None）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<ToolCallState>,
        /// 提供商返回的原始内容（部分协议携带，对应 Java `content` 字段）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<String>,
    },
    /// 工具执行结果（由 agent 侧回填；output 是块列表，可携带多模态）。
    ToolResult {
        /// 对应的工具调用 id。
        id: String,
        /// 工具名。
        #[serde(default)]
        name: String,
        /// 结果内容（块列表；纯文本结果即单个 Text 块）。
        #[serde(default)]
        output: Vec<ContentBlock>,
        #[serde(default)]
        state: ToolResultState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        execution_details: Option<ToolExecutionDetails>,
    },
    /// 给 LLM 的提示块（如 RAG 检索结果注入，对应 Java `HintBlock`）。
    Hint {
        id: String,
        hint: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    /// 通用二进制数据块（对应 Java `DataBlock`，统一承载图像 / 音频 / 视频之外的媒体）。
    Data {
        source: Source,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

impl ContentBlock {
    /// 成功文本结果块（对应 Java `ToolResultBlock.success`）。
    pub fn tool_result_text(
        id: impl Into<String>,
        name: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        Self::ToolResult {
            id: id.into(),
            name: name.into(),
            output: vec![Self::Text { text: text.into() }],
            state: ToolResultState::Success,
            execution_details: None,
        }
    }

    /// 错误结果块（对应 Java `ToolResultBlock.error`）。
    pub fn tool_result_error(
        id: impl Into<String>,
        name: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::ToolResult {
            id: id.into(),
            name: name.into(),
            output: vec![Self::Text { text: message.into() }],
            state: ToolResultState::Error,
            execution_details: None,
        }
    }
}

/// 消息元数据的协议键（对应 Java `MessageMetadataKeys`）。
///
/// 框架通过这些保留键在 metadata 中传递控制信息；应用自定义键不应使用下划线前缀。
pub mod metadata_keys {
    /// 跳过多 agent 历史合并。
    pub const BYPASS_MULTIAGENT_HISTORY_MERGE: &str = "_bypass_multiagent_history_merge";
    /// 结构化输出提醒标记。
    pub const STRUCTURED_OUTPUT_REMINDER: &str = "_structured_output_reminder";
    /// 结构化输出提醒类型。
    pub const STRUCTURED_OUTPUT_REMINDER_TYPE: &str = "_structured_output_reminder_type";
    /// 摘要失败标记。
    pub const SUMMARY_FAILED: &str = "_summary_failed";
    /// 聊天用量（与 `Msg.usage` 字段冗余的元数据通道）。
    pub const CHAT_USAGE: &str = "_chat_usage";
    /// 结构化输出结果。
    pub const STRUCTURED_OUTPUT: &str = "_structured_output";
    /// 提示词缓存控制。
    pub const CACHE_CONTROL: &str = "_cache_control";
    /// 确认结果（对应 Java `Msg.METADATA_CONFIRM_RESULTS`）：
    /// 携带它的消息是「对未决请求的答复」，续跑时不得重放成聊天内容。
    pub const CONFIRM_RESULTS: &str = "agentscope_confirm_results";
    /// 合成消息标记（对应 Java `Msg.METADATA_SYNTHETIC`）：由运行时注入的
    /// 系统级内容（任务投递提醒、运行态摘要），不是用户输入；压缩与持久化
    /// 层可据此把它排除出持久摘要。
    pub const SYNTHETIC: &str = "agentscope_synthetic";
    /// 合成消息的种类（对应 Java `Msg.METADATA_REMINDER_KIND`），仅在
    /// [`SYNTHETIC`] 为 true 时有意义，如 `task_delivery` / `subagent_state`。
    pub const REMINDER_KIND: &str = "agentscope_reminder_kind";
    /// 工具声明 return_direct。
    pub const TOOL_RETURN_DIRECT: &str = "_tool_return_direct";
}

/// 消息：agent 间通信的基本单元。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Msg {
    pub id: String,
    pub name: String,
    pub role: Role,
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    /// 消息产生时间（ISO-8601 字符串；框架未注入时为 None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    /// 产生本消息的模型调用用量。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<crate::model::Usage>,
    /// 本消息的生成原因（见 [`GenerateReason`]）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generate_reason: Option<GenerateReason>,
}

/// 当前 UTC 时间的 ISO-8601 字符串（如 `2026-10-26T08:15:30.123Z`）。
/// 核心库不引 chrono：unix 秒 → civil 日期用 Hinnant 的 civil-from-days 算法。
fn now_iso() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64;
    let millis = now.subsec_millis();
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
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
    let (h, mi, s) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{millis:03}Z")
}

impl Msg {
    /// 用完整内容块列表构造消息。
    pub fn new(role: Role, name: impl Into<String>, content: Vec<ContentBlock>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name: name.into(),
            role,
            content,
            metadata: None,
            // 消息产生时间在构造点统一注入（历史视图/会话列表都依赖它；
            // 反序列化旧状态文件时缺字段仍回落 None，serde default 兜底）。
            timestamp: Some(now_iso()),
            usage: None,
            generate_reason: None,
        }
    }

    /// 构造用户文本消息。
    pub fn user(name: impl Into<String>, text: impl Into<String>) -> Self {
        Self::new(Role::User, name, vec![ContentBlock::Text { text: text.into() }])
    }

    /// 构造系统文本消息。
    pub fn system(name: impl Into<String>, text: impl Into<String>) -> Self {
        Self::new(Role::System, name, vec![ContentBlock::Text { text: text.into() }])
    }

    /// 构造助手消息（可含文本与工具调用块）。
    pub fn assistant(name: impl Into<String>, content: Vec<ContentBlock>) -> Self {
        Self::new(Role::Assistant, name, content)
    }

    /// 构造工具结果消息（对应 Java `ToolResultMessage.result(toolCallId, toolName, text)`）。
    pub fn tool_result(
        name: impl Into<String>,
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        text: impl Into<String>,
        state: ToolResultState,
    ) -> Self {
        Self::new(
            Role::Tool,
            name,
            vec![ContentBlock::ToolResult {
                id: tool_call_id.into(),
                name: tool_name.into(),
                output: vec![ContentBlock::Text { text: text.into() }],
                state,
                execution_details: None,
            }],
        )
    }

    /// 拼接消息中所有文本块，块间以换行连接（对齐 Java `getTextContent` 的 joining("\n")）。
    pub fn text(&self) -> String {
        self.text_blocks().join("\n")
    }

    /// 取出所有文本块的文本（对应 Java 按 TextBlock 过滤）。
    pub fn text_blocks(&self) -> Vec<String> {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// 取出消息中所有工具调用块（克隆）。
    pub fn tool_uses(&self) -> Vec<ContentBlock> {
        self.content
            .iter()
            .filter(|b| matches!(b, ContentBlock::ToolUse { .. }))
            .cloned()
            .collect()
    }

    /// 是否包含工具调用块。
    pub fn has_tool_use(&self) -> bool {
        self.content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
    }

    /// 第一个内容块（对应 Java `getFirstContentBlock`）。
    pub fn first_content_block(&self) -> Option<&ContentBlock> {
        self.content.first()
    }

    /// 是否携带结构化输出结果（metadata `_structured_output` 键）。
    pub fn has_structured_data(&self) -> bool {
        self.structured_data().is_some()
    }

    /// 读取结构化输出结果。
    pub fn structured_data(&self) -> Option<&serde_json::Value> {
        self.metadata
            .as_ref()
            .and_then(|m| m.get(metadata_keys::STRUCTURED_OUTPUT))
    }

    /// 附加元数据（builder 风格，消费 self 返回新消息）。
    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = Some(metadata);
        self
    }

    /// 设置生成原因（builder 风格）。
    pub fn with_generate_reason(mut self, reason: GenerateReason) -> Self {
        self.generate_reason = Some(reason);
        self
    }

    /// 设置模型用量（builder 风格）。
    pub fn with_usage(mut self, usage: crate::model::Usage) -> Self {
        self.usage = Some(usage);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn user_text_message() {
        let msg = Msg::user("user", "你好");
        assert_eq!(msg.role, Role::User);
        assert_eq!(msg.text(), "你好");
    }

    #[test]
    fn new_message_stamps_iso_timestamp() {
        // 构造点统一注入时间戳：格式必须是 ISO-8601 UTC（前端 fmtTime 依赖）。
        let msg = Msg::user("user", "hi");
        let ts = msg.timestamp.expect("timestamp should be stamped");
        let re = regex_check(&ts);
        assert!(re, "not ISO-8601: {ts}");
    }

    /// 轻量校验 `YYYY-MM-DDTHH:MM:SS.mmmZ` 形状（不引 regex 依赖）。
    fn regex_check(ts: &str) -> bool {
        let b = ts.as_bytes();
        b.len() == 24
            && b[4] == b'-'
            && b[7] == b'-'
            && b[10] == b'T'
            && b[13] == b':'
            && b[16] == b':'
            && b[19] == b'.'
            && b[23] == b'Z'
            && ts[..23].chars().skip(4).all(|c| c.is_ascii_digit() || c == '-' || c == 'T' || c == ':' || c == '.')
    }

    #[test]
    fn civil_from_days_known_epochs() {
        // 已知历元往返：1970-01-01 → 2026-10-26（含闰年 2024-02-29）。
        fn iso_at(secs: i64, millis: u32) -> String {
            let days = secs.div_euclid(86_400);
            let sod = secs.rem_euclid(86_400);
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
            let (h, mi, s) = (sod / 3600, (sod % 3600) / 60, sod % 60);
            format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{millis:03}Z")
        }
        assert_eq!(iso_at(0, 0), "1970-01-01T00:00:00.000Z");
        // 2026-10-26 00:00:00 UTC = 1790304000
        assert_eq!(iso_at(1_792_972_800, 123), "2026-10-26T00:00:00.123Z");
        // 2024-02-29 12:34:56 UTC（闰日）= 1709212496
        assert_eq!(iso_at(1_709_210_096, 0), "2024-02-29T12:34:56.000Z");
    }

    #[test]
    fn serde_roundtrip() {
        let msg = Msg::assistant(
            "assistant",
            vec![
                ContentBlock::Text { text: "我来查一下".into() },
                ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "get_time".into(),
                    input: json!({}),
                    state: None,
                    content: None,
                },
            ],
        );
        let json = serde_json::to_string(&msg).unwrap();
        let back: Msg = serde_json::from_str(&json).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn tool_uses_filter() {
        let msg = Msg::assistant(
            "assistant",
            vec![
                ContentBlock::Text { text: "查时间".into() },
                ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "get_time".into(),
                    input: json!({}),
                    state: None,
                    content: None,
                },
            ],
        );
        assert!(msg.has_tool_use());
        assert_eq!(msg.tool_uses().len(), 1);
    }

    /// 多文本块以换行连接（对齐 Java getTextContent 的 joining("\n")）。
    #[test]
    fn text_blocks_joined_with_newline() {
        let msg = Msg::assistant(
            "assistant",
            vec![
                ContentBlock::Text { text: "第一段".into() },
                ContentBlock::Thinking { thinking: "推理过程".into() },
                ContentBlock::Text { text: "第二段".into() },
            ],
        );
        assert_eq!(msg.text(), "第一段\n第二段");
        assert_eq!(msg.text_blocks().len(), 2);
    }

    /// 工具结果块：状态机 + 块列表 output + 工厂便捷构造。
    #[test]
    fn tool_result_state_and_output_blocks() {
        let ok = ContentBlock::tool_result_text("call_1", "get_time", "1700000000");
        match &ok {
            ContentBlock::ToolResult { id, name, output, state, execution_details } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "get_time");
                assert_eq!(output.len(), 1);
                assert_eq!(*state, ToolResultState::Success);
                assert!(execution_details.is_none());
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }

        let err = ContentBlock::tool_result_error("call_2", "shell", "command not found");
        assert!(matches!(
            &err,
            ContentBlock::ToolResult { state: ToolResultState::Error, .. }
        ));

        // 状态序列化为小写字符串（对齐 Java @JsonValue）
        let json = serde_json::to_string(&ok).unwrap();
        assert!(json.contains(r#""state":"success""#), "json = {json}");
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ok);
    }

    /// 工具调用状态机序列化（pending/asking/allowed/submitted/finished）。
    #[test]
    fn tool_call_state_serde() {
        let block = ContentBlock::ToolUse {
            id: "call_1".into(),
            name: "shell".into(),
            input: json!({"cmd": "ls"}),
            state: Some(ToolCallState::Asking),
            content: None,
        };
        let json = serde_json::to_string(&block).unwrap();
        assert!(json.contains(r#""state":"asking""#), "json = {json}");
        assert!(json.contains(r#""type":"tool_use""#), "json = {json}");
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(back, block);
    }

    /// 多模态块：Source 抽象（untagged Base64/Url）+ 专用块类型。
    #[test]
    fn multimodal_blocks_with_source() {
        let image = ContentBlock::Image {
            source: Source::Base64 {
                media_type: "image/png".into(),
                data: "aGVsbG8=".into(),
            },
            min_pixels: None,
            max_pixels: Some(1024),
        };
        let json = serde_json::to_string(&image).unwrap();
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(back, image);

        let audio = ContentBlock::Audio {
            source: Source::Url { url: "https://example.com/a.mp3".into(), mime_type: None },
        };
        let json = serde_json::to_string(&audio).unwrap();
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(back, audio);

        let video = ContentBlock::Video {
            source: Source::Url { url: "https://example.com/v.mp4".into(), mime_type: Some("video/mp4".into()) },
            fps: Some(24.0),
            max_frames: Some(64),
            min_pixels: None,
            max_pixels: None,
            total_pixels: None,
        };
        let json = serde_json::to_string(&video).unwrap();
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(back, video);
    }

    /// GenerateReason 序列化为 snake_case，覆盖 ReAct 流程的全部停止原因。
    #[test]
    fn generate_reason_serde() {
        for (reason, expected) in [
            (GenerateReason::ModelStop, "model_stop"),
            (GenerateReason::ToolCalls, "tool_calls"),
            (GenerateReason::ToolSuspended, "tool_suspended"),
            (GenerateReason::PermissionAsking, "permission_asking"),
            (GenerateReason::MaxIterations, "max_iterations"),
            (GenerateReason::ToolReturnDirect, "tool_return_direct"),
        ] {
            let json = serde_json::to_string(&reason).unwrap();
            assert_eq!(json, format!("\"{expected}\""));
            let back: GenerateReason = serde_json::from_str(&json).unwrap();
            assert_eq!(back, reason);
        }
    }

    /// Msg 携带 usage / generate_reason / timestamp，序列化往返保持。
    #[test]
    fn msg_flow_metadata_roundtrip() {
        let usage = crate::model::Usage {
            prompt_tokens: 100,
            completion_tokens: 20,
            total_tokens: 120,
        };
        let msg = Msg::assistant("assistant", vec![ContentBlock::Text { text: "done".into() }])
            .with_usage(usage)
            .with_generate_reason(GenerateReason::ModelStop);
        let json = serde_json::to_string(&msg).unwrap();
        let back: Msg = serde_json::from_str(&json).unwrap();
        assert_eq!(back.usage, Some(usage));
        assert_eq!(back.generate_reason, Some(GenerateReason::ModelStop));
    }

    /// 结构化输出：metadata `_structured_output` 键（对应 Java MessageMetadataKeys）。
    #[test]
    fn structured_data_via_metadata_key() {
        let msg = Msg::assistant("assistant", vec![ContentBlock::Text { text: "{}".into() }])
            .with_metadata(json!({ metadata_keys::STRUCTURED_OUTPUT: {"city": "杭州"} }));
        assert!(msg.has_structured_data());
        assert_eq!(
            msg.structured_data().unwrap()["city"],
            json!("杭州")
        );
    }

    /// Hint 块（RAG 提示注入）与 Data 块（通用媒体）。
    #[test]
    fn hint_and_data_blocks() {
        let hint = ContentBlock::Hint {
            id: "h1".into(),
            hint: "相关文档片段……".into(),
            source: Some("rag".into()),
        };
        let data = ContentBlock::Data {
            source: Source::Base64 { media_type: "application/pdf".into(), data: "JVBERi0=".into() },
            name: Some("report.pdf".into()),
        };
        let msg = Msg::new(Role::User, "user", vec![hint, data]);
        // Hint/Data 不参与文本拼接
        assert_eq!(msg.text(), "");
        let json = serde_json::to_string(&msg).unwrap();
        let back: Msg = serde_json::from_str(&json).unwrap();
        assert_eq!(back, msg);
    }
}
