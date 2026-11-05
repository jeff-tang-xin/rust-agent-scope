//! Anthropic Messages API 的消息格式化实现。
//!
//! 对应 Java 版 `io.agentscope.extensions.model.anthropic.formatter` 包：
//! - [`AnthropicChatFormatter`] ↔ `AnthropicChatFormatter`（format / applyOptions / applyTools / applyToolChoice）
//! - 模块内 `convert_message` ↔ `AnthropicMessageConverter`（Msg → Anthropic 消息的逐角色转换）
//! - `response_to_chat_response` ↔ `AnthropicResponseParser`（响应 → `ChatResponse`）
//!
//! 与 Java 一致：模型实现（`AnthropicChatModel`）只负责 HTTP 传输与流式解析，
//! 请求消息与参数的装配全部由 formatter 完成。
//!
//! 协议要点（对齐 Java 版）：
//! - system 取**第一条** System 消息的 Text 块（`\n` 连接）放请求顶层 `system`
//!   字段并从 messages 剔除；非首条 System 一律转 user；
//! - role 仅 user/assistant（Tool/System → user）；
//! - 工具结果块每条展开成独立 user 消息（`tool_result` 块，`tool_use_id` + `content[]`）；
//! - 请求里的 Thinking 块降级为纯 text 块回传（Anthropic 要求 thinking 块携带
//!   签名才能回放，响应侧仍解析 thinking）。

use serde_json::{json, Value};

use agentscope_core::error::Result;
use agentscope_core::formatter::{convert_tool_result_to_string, Formatter};
use agentscope_core::message::{ContentBlock, Msg, Role, Source};
use agentscope_core::model::{ChatResponse, GenerateOptions, ToolChoice, ToolSchema, Usage};

use crate::{AnthropicMessage, AnthropicRequest, AnthropicResponse, AnthropicTool};

/// `max_tokens` 缺省兜底（对齐 Java 版 builder 硬编码 4096）。
pub(crate) const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Anthropic Messages 格式化器（对应 Java `AnthropicChatFormatter`）。
#[derive(Debug, Default, Clone, Copy)]
pub struct AnthropicChatFormatter;

impl AnthropicChatFormatter {
    pub fn new() -> Self {
        Self
    }

    /// 完整请求装配：顶层 `system` 提示词 + messages 列表。
    ///
    /// Anthropic 协议的 system 不走 messages 列表，而是请求顶层字段——
    /// 取第一条 System 消息的 Text 块（`\n` 连接）提取，其余 System 消息按 user 转换。
    pub fn format_request(&self, msgs: &[Msg]) -> AnthropicFormattedRequest {
        let system = msgs
            .iter()
            .find(|m| m.role == Role::System)
            .map(|m| {
                m.content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .filter(|s| !s.is_empty());

        let mut messages = Vec::new();
        let mut first_system_taken = false;
        for msg in msgs {
            if msg.role == Role::System && !first_system_taken {
                first_system_taken = true;
                continue; // 已提取进顶层 system 字段
            }
            convert_message(msg, &mut messages);
        }
        AnthropicFormattedRequest { system, messages }
    }
}

/// 格式化结果：顶层 system 提示词 + messages 列表。
#[derive(Debug, Clone)]
pub struct AnthropicFormattedRequest {
    pub system: Option<String>,
    pub messages: Vec<AnthropicMessage>,
}

impl Formatter for AnthropicChatFormatter {
    type RequestMessage = AnthropicMessage;
    type RequestParams = AnthropicRequest;
    type Response = AnthropicResponse;

    /// 消息格式化。注意：顶层 `system` 字段经 [`AnthropicChatFormatter::format_request`]
    /// 单独提取，本 trait 方法只返回 messages 部分。
    fn format(
        &self,
        msgs: &[Msg],
        _options: Option<&GenerateOptions>,
    ) -> Result<Vec<Self::RequestMessage>> {
        Ok(self.format_request(msgs).messages)
    }

    fn parse_response(&self, response: Self::Response) -> Result<ChatResponse> {
        Ok(response_to_chat_response(response))
    }

    /// 生成参数写入请求体：请求级 `options` 优先，缺失值时回退模型实例的 `defaults`；
    /// `max_tokens` 是 Anthropic 必填字段，最终以 4096 兜底（对齐 Java builder）。
    fn apply_options(
        &self,
        params: &mut Self::RequestParams,
        options: &GenerateOptions,
        defaults: Option<&GenerateOptions>,
    ) {
        params.temperature = options
            .temperature
            .or_else(|| defaults.and_then(|d| d.temperature));
        params.max_tokens = options
            .max_tokens
            .or_else(|| defaults.and_then(|d| d.max_tokens))
            .unwrap_or(DEFAULT_MAX_TOKENS);
    }

    fn apply_tools(&self, params: &mut Self::RequestParams, tools: &[ToolSchema]) {
        if tools.is_empty() {
            return;
        }
        params.tools = tools
            .iter()
            .map(|s| AnthropicTool {
                name: s.name.clone(),
                description: s.description.clone(),
                input_schema: s.parameters.clone(),
            })
            .collect();
    }

    /// 工具选择策略：仅在携带工具时下发（对齐 Java `applyToolChoice`）。
    fn apply_tool_choice(
        &self,
        params: &mut Self::RequestParams,
        tool_choice: Option<&ToolChoice>,
    ) {
        if params.tools.is_empty() {
            return;
        }
        params.tool_choice = Some(match tool_choice {
            None | Some(ToolChoice::Auto) => json!({ "type": "auto" }),
            Some(ToolChoice::None) => json!({ "type": "none" }),
            // Anthropic 无 required 选项，Java 显式降级为 any（强制工具调用）
            Some(ToolChoice::Required) => json!({ "type": "any" }),
            Some(ToolChoice::Specific { tool_name }) => {
                json!({ "type": "tool", "name": tool_name })
            }
        });
    }
}

// ---------- Msg → Anthropic 消息 ----------

/// 把一条 [`Msg`] 转换为 Anthropic 协议消息（对应 Java `AnthropicMessageConverter`）。
///
/// role 仅 user/assistant：Tool/System 一律按 user 发送；工具结果块每条展开成
/// 独立 user 消息（`tool_result` 块）；Thinking 块降级为纯 text 块回传。
fn convert_message(msg: &Msg, out: &mut Vec<AnthropicMessage>) {
    let role = match msg.role {
        Role::Assistant => "assistant",
        Role::User | Role::Tool | Role::System => "user",
    };

    let mut blocks: Vec<Value> = Vec::new();
    let mut tool_results: Vec<Value> = Vec::new();

    for block in &msg.content {
        match block {
            ContentBlock::Text { text } => {
                // Anthropic 拒绝空 text 块，空文本直接跳过
                if !text.is_empty() {
                    blocks.push(json!({ "type": "text", "text": text }));
                }
            }
            // Hint 块按文本注入（对齐 core extract_text_content 的语义）
            ContentBlock::Hint { hint, .. } => {
                if !hint.is_empty() {
                    blocks.push(json!({ "type": "text", "text": hint }));
                }
            }
            // 对齐 Java：请求历史里的 Thinking 块降级为纯 text 块回传
            ContentBlock::Thinking { thinking } => {
                if !thinking.is_empty() {
                    blocks.push(json!({ "type": "text", "text": thinking }));
                }
            }
            ContentBlock::Image { source, .. } => blocks.push(image_block(source)),
            // 音频/视频/通用数据块：Messages API 不接受，转文本占位避免静默丢失
            ContentBlock::Audio { source }
            | ContentBlock::Video { source, .. }
            | ContentBlock::Data { source, .. } => {
                blocks.push(json!({
                    "type": "text",
                    "text": format!("[Media - unsupported MIME type: {}]", media_type_of(source))
                }));
            }
            ContentBlock::ToolUse { id, name, input, .. } => {
                blocks.push(json!({
                    "type": "tool_use",
                    "id": id,
                    "name": name,
                    "input": input,
                }));
            }
            // 工具结果块每条展开成独立 user 消息（对齐 Java 逐条展开；
            // is_error 不设置，错误信息靠结果文本传达）
            ContentBlock::ToolResult { id, output, .. } => {
                tool_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": id,
                    "content": [{ "type": "text", "text": convert_tool_result_to_string(output) }]
                }));
            }
        }
    }

    // 主消息（非工具结果块）；空内容不产出，避免空 content 数组被 API 拒绝
    if !blocks.is_empty() {
        out.push(AnthropicMessage {
            role: role.into(),
            content: blocks,
        });
    }
    for tool_result in tool_results {
        out.push(AnthropicMessage {
            role: "user".into(),
            content: vec![tool_result],
        });
    }
}

/// Image 块 → Anthropic image 块（base64 与 URL 两种来源都支持）。
fn image_block(source: &Source) -> Value {
    match source {
        Source::Base64 { media_type, data } => json!({
            "type": "image",
            "source": { "type": "base64", "media_type": media_type, "data": data }
        }),
        Source::Url { url, .. } => json!({
            "type": "image",
            "source": { "type": "url", "url": url }
        }),
    }
}

fn media_type_of(source: &Source) -> String {
    match source {
        Source::Base64 { media_type, .. } => media_type.clone(),
        Source::Url { url, mime_type } => mime_type
            .clone()
            .unwrap_or_else(|| format!("unknown, url: {url}")),
    }
}

// ---------- Anthropic 响应 → ChatResponse ----------

/// 把 Messages API 响应转换回 [`ChatResponse`]（对应 Java `AnthropicResponseParser`）。
///
/// 块顺序按响应 content 数组原样保留（text / thinking / tool_use）；
/// 未知块类型（如 `redacted_thinking`）跳过不中断解析；
/// usage 对齐 Java 不变量：prompt = input_tokens + cache_read + cache_creation。
pub(crate) fn response_to_chat_response(response: AnthropicResponse) -> ChatResponse {
    let mut content = Vec::new();
    for block in &response.content {
        let Some(kind) = block.get("type").and_then(Value::as_str) else {
            continue;
        };
        match kind {
            "text" => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        content.push(ContentBlock::Text { text: text.into() });
                    }
                }
            }
            "thinking" => {
                if let Some(thinking) = block.get("thinking").and_then(Value::as_str) {
                    if !thinking.is_empty() {
                        content.push(ContentBlock::Thinking {
                            thinking: thinking.into(),
                        });
                    }
                }
            }
            "tool_use" => {
                content.push(ContentBlock::ToolUse {
                    id: block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    name: block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    input: block
                        .get("input")
                        .cloned()
                        .unwrap_or_else(|| Value::Object(serde_json::Map::new())),
                    state: None,
                    content: None,
                });
            }
            // 未知块类型（redacted_thinking 等）跳过
            _ => {}
        }
    }
    let usage = response.usage.map(|u| {
        let prompt_tokens =
            u.input_tokens + u.cache_read_input_tokens + u.cache_creation_input_tokens;
        Usage {
            prompt_tokens,
            completion_tokens: u.output_tokens,
            total_tokens: prompt_tokens + u.output_tokens,
        }
    });
    ChatResponse {
        content,
        usage,
        finish_reason: response.stop_reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::message::ToolResultState;
    use serde_json::json;

    #[test]
    fn user_msg_conversion() {
        let msg = Msg::user("user", "你好");
        let formatted = AnthropicChatFormatter::new().format_request(&[msg]);
        assert!(formatted.system.is_none());
        assert_eq!(formatted.messages.len(), 1);
        let m = &formatted.messages[0];
        assert_eq!(m.role, "user");
        assert_eq!(m.content.len(), 1);
        assert_eq!(m.content[0]["type"], "text");
        assert_eq!(m.content[0]["text"], "你好");
    }

    /// system 提取：第一条 System 消息进顶层 system 字段并从 messages 剔除；
    /// 多 Text 块以 `\n` 连接；非首条 System 一律转 user。
    #[test]
    fn system_extracted_to_top_level_and_non_first_becomes_user() {
        let msgs = vec![
            Msg::new(
                Role::System,
                "system",
                vec![
                    ContentBlock::Text { text: "第一段".into() },
                    ContentBlock::Text { text: "第二段".into() },
                ],
            ),
            Msg::user("user", "你好"),
            Msg::system("system", "补充指令"),
        ];
        let formatted = AnthropicChatFormatter::new().format_request(&msgs);
        assert_eq!(formatted.system.as_deref(), Some("第一段\n第二段"));
        assert_eq!(formatted.messages.len(), 2);
        assert_eq!(formatted.messages[0].role, "user");
        assert_eq!(formatted.messages[1].role, "user");
        assert_eq!(formatted.messages[1].content[0]["text"], "补充指令");
    }

    /// assistant 历史：Text 保留、ToolUse 转 tool_use 块、Thinking 降级为 text 块。
    #[test]
    fn assistant_conversion_with_thinking_downgrade() {
        let msg = Msg::assistant(
            "assistant",
            vec![
                ContentBlock::Thinking { thinking: "内心推理".into() },
                ContentBlock::Text { text: "我来算".into() },
                ContentBlock::ToolUse {
                    id: "toolu_1".into(),
                    name: "add".into(),
                    input: json!({"a": 1, "b": 2}),
                    state: None,
                    content: None,
                },
            ],
        );
        let formatted = AnthropicChatFormatter::new().format_request(&[msg]);
        assert_eq!(formatted.messages.len(), 1);
        let m = &formatted.messages[0];
        assert_eq!(m.role, "assistant");
        assert_eq!(m.content.len(), 3);
        // Thinking 降级为纯 text 块回传（对齐 Java AnthropicMessageConverter）
        assert_eq!(m.content[0]["type"], "text");
        assert_eq!(m.content[0]["text"], "内心推理");
        assert_eq!(m.content[1]["text"], "我来算");
        assert_eq!(m.content[2]["type"], "tool_use");
        assert_eq!(m.content[2]["id"], "toolu_1");
        assert_eq!(m.content[2]["name"], "add");
        assert_eq!(m.content[2]["input"]["a"], 1);
    }

    /// 工具结果块每条展开成独立 user 消息（tool_result 块，tool_use_id + content[]）。
    #[test]
    fn tool_result_expands_to_independent_user_messages() {
        let msg = Msg::new(
            Role::Tool,
            "tool",
            vec![
                ContentBlock::ToolResult {
                    id: "call_1".into(),
                    name: "get_time".into(),
                    output: vec![ContentBlock::Text { text: "12:00".into() }],
                    state: ToolResultState::Success,
                    execution_details: None,
                },
                ContentBlock::ToolResult {
                    id: "call_2".into(),
                    name: "shell".into(),
                    output: vec![ContentBlock::Text { text: "ok".into() }],
                    state: ToolResultState::Error,
                    execution_details: None,
                },
            ],
        );
        let formatted = AnthropicChatFormatter::new().format_request(&[msg]);
        assert_eq!(formatted.messages.len(), 2);
        for (i, m) in formatted.messages.iter().enumerate() {
            assert_eq!(m.role, "user");
            assert_eq!(m.content.len(), 1);
            assert_eq!(m.content[0]["type"], "tool_result");
            assert_eq!(m.content[0]["tool_use_id"], format!("call_{}", i + 1));
            assert_eq!(m.content[0]["content"][0]["type"], "text");
        }
        assert_eq!(formatted.messages[0].content[0]["content"][0]["text"], "12:00");
        assert_eq!(formatted.messages[1].content[0]["content"][0]["text"], "ok");
    }

    /// 多模态：Image 块 → Anthropic image 块（base64 / URL 两种来源）。
    #[test]
    fn image_block_conversion() {
        let msg = Msg::new(
            Role::User,
            "user",
            vec![
                ContentBlock::Image {
                    source: Source::Base64 {
                        media_type: "image/png".into(),
                        data: "aGVsbG8=".into(),
                    },
                    min_pixels: None,
                    max_pixels: None,
                },
                ContentBlock::Image {
                    source: Source::Url {
                        url: "https://example.com/a.png".into(),
                        mime_type: None,
                    },
                    min_pixels: None,
                    max_pixels: None,
                },
            ],
        );
        let formatted = AnthropicChatFormatter::new().format_request(&[msg]);
        let m = &formatted.messages[0];
        assert_eq!(m.content[0]["type"], "image");
        assert_eq!(m.content[0]["source"]["type"], "base64");
        assert_eq!(m.content[0]["source"]["media_type"], "image/png");
        assert_eq!(m.content[1]["source"]["type"], "url");
        assert_eq!(m.content[1]["source"]["url"], "https://example.com/a.png");
    }

    /// 空文本块跳过、纯空消息不产出（Anthropic 拒绝空 text 块与空 content 数组）。
    #[test]
    fn empty_text_and_empty_message_are_dropped() {
        let empty = Msg::user("user", "");
        let formatted = AnthropicChatFormatter::new().format_request(&[empty]);
        assert!(formatted.messages.is_empty());
    }

    // ---------- apply_options / apply_tools / apply_tool_choice ----------

    fn schema(name: &str) -> ToolSchema {
        ToolSchema {
            name: name.into(),
            description: "d".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    /// 请求级 options 优先，缺失时回退模型默认值；max_tokens 最终 4096 兜底。
    #[test]
    fn apply_options_prefers_request_then_defaults_then_fallback() {
        let f = AnthropicChatFormatter::new();
        let defaults = GenerateOptions {
            temperature: Some(0.1),
            max_tokens: Some(512),
            ..Default::default()
        };

        let mut params = AnthropicRequest::default();
        let options = GenerateOptions {
            temperature: Some(0.9),
            ..Default::default()
        };
        f.apply_options(&mut params, &options, Some(&defaults));
        assert_eq!(params.temperature, Some(0.9));
        assert_eq!(params.max_tokens, 512);

        // 两级都缺 → 4096 兜底（对齐 Java builder 硬编码）
        let mut params = AnthropicRequest::default();
        f.apply_options(&mut params, &GenerateOptions::default(), None);
        assert_eq!(params.max_tokens, 4096);
    }

    /// 工具定义下发：Anthropic 用 `input_schema` 承载 JSON Schema。
    #[test]
    fn apply_tools_builds_input_schema() {
        let f = AnthropicChatFormatter::new();
        let mut params = AnthropicRequest::default();
        f.apply_tools(&mut params, &[schema("add")]);
        assert_eq!(params.tools.len(), 1);
        assert_eq!(params.tools[0].name, "add");
        assert_eq!(params.tools[0].description, "d");
        assert_eq!(params.tools[0].input_schema["type"], "object");
    }

    /// 无工具时即使指定 tool_choice 也不下发（对齐 Java `applyToolChoice` 的前置判断）。
    #[test]
    fn apply_tool_choice_skipped_without_tools() {
        let f = AnthropicChatFormatter::new();
        let mut params = AnthropicRequest::default();
        f.apply_tool_choice(&mut params, Some(&ToolChoice::Required));
        assert!(params.tool_choice.is_none());
    }

    /// tool_choice 恒为对象：auto / none / any（Required 降级）/ tool+name。
    #[test]
    fn apply_tool_choice_variants() {
        let f = AnthropicChatFormatter::new();
        let mut params = AnthropicRequest::default();
        f.apply_tools(&mut params, &[schema("add")]);

        f.apply_tool_choice(&mut params, None);
        assert_eq!(params.tool_choice, Some(json!({"type": "auto"})));
        f.apply_tool_choice(&mut params, Some(&ToolChoice::Auto));
        assert_eq!(params.tool_choice, Some(json!({"type": "auto"})));
        f.apply_tool_choice(&mut params, Some(&ToolChoice::None));
        assert_eq!(params.tool_choice, Some(json!({"type": "none"})));
        f.apply_tool_choice(&mut params, Some(&ToolChoice::Required));
        assert_eq!(params.tool_choice, Some(json!({"type": "any"})));
        f.apply_tool_choice(
            &mut params,
            Some(&ToolChoice::Specific { tool_name: "add".into() }),
        );
        assert_eq!(
            params.tool_choice,
            Some(json!({"type": "tool", "name": "add"}))
        );
    }

    // ---------- 响应解析 ----------

    /// 响应解析：thinking → text → tool_use 按原序保留，未知块跳过；
    /// usage 对齐 Java 不变量 prompt = input + cache_read + cache_creation。
    #[test]
    fn response_parsing() {
        let raw = json!({
            "id": "msg_1",
            "content": [
                {"type": "thinking", "thinking": "推理过程"},
                {"type": "text", "text": "答案是 3"},
                {"type": "tool_use", "id": "toolu_1", "name": "add", "input": {"a": 1}},
                {"type": "redacted_thinking", "data": "xxx"}
            ],
            "stop_reason": "tool_use",
            "usage": {
                "input_tokens": 100,
                "output_tokens": 20,
                "cache_read_input_tokens": 5,
                "cache_creation_input_tokens": 3
            }
        });
        let response: AnthropicResponse = serde_json::from_value(raw).unwrap();
        let resp = AnthropicChatFormatter::new().parse_response(response).unwrap();
        assert_eq!(resp.content.len(), 3);
        assert!(matches!(&resp.content[0], ContentBlock::Thinking { thinking } if thinking == "推理过程"));
        assert!(matches!(&resp.content[1], ContentBlock::Text { text } if text == "答案是 3"));
        assert!(matches!(
            &resp.content[2],
            ContentBlock::ToolUse { id, name, input, .. }
                if id == "toolu_1" && name == "add" && input["a"] == 1
        ));
        assert_eq!(resp.finish_reason.as_deref(), Some("tool_use"));
        let usage = resp.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 108);
        assert_eq!(usage.completion_tokens, 20);
        assert_eq!(usage.total_tokens, 128);
    }

    /// 无 usage / 无 stop_reason 时字段缺省（cache 字段缺失按 0 计）。
    #[test]
    fn response_without_usage_and_stop_reason() {
        let raw = json!({
            "content": [{"type": "text", "text": "hi"}]
        });
        let response: AnthropicResponse = serde_json::from_value(raw).unwrap();
        let resp = AnthropicChatFormatter::new().parse_response(response).unwrap();
        assert!(resp.usage.is_none());
        assert!(resp.finish_reason.is_none());
        assert_eq!(resp.content.len(), 1);
    }
}
