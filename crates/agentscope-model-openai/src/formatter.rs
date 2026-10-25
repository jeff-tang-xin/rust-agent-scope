//! OpenAI 协议的消息格式化实现。
//!
//! 对应 Java 版 `io.agentscope.extensions.model.openai.formatter` 包：
//! - [`OpenAIChatFormatter`] ↔ `OpenAIChatFormatter`（format / applyOptions / applyTools / applyToolChoice）
//! - 模块内的 `convert_message` ↔ `OpenAIMessageConverter`（Msg → OpenAI 消息的逐角色转换）
//! - `response_to_chat_response` ↔ `OpenAIResponseParser`（响应 → `ChatResponse`）
//!
//! 与 Java 一致：模型实现（`OpenAIChatModel`）只负责 HTTP 传输与流式解析，
//! 请求消息与参数的装配全部由 formatter 完成。

use serde_json::{json, Value};

use agentscope_core::error::{Error, Result};
use agentscope_core::formatter::{convert_tool_result_to_string, extract_text_content, Formatter};
use agentscope_core::message::{ContentBlock, Msg, Role, Source};
use agentscope_core::model::{ChatResponse, GenerateOptions, ToolChoice, ToolSchema, Usage};

use crate::{
    ChatCompletionRequest, ChatCompletionResponse, OpenAIFunction, OpenAIFunctionDef, OpenAIMessage,
    OpenAITool, OpenAIToolCall, OpenAIUsage,
};

/// OpenAI Chat Completions 格式化器（对应 Java `OpenAIChatFormatter`）。
#[derive(Debug, Default, Clone, Copy)]
pub struct OpenAIChatFormatter;

impl OpenAIChatFormatter {
    pub fn new() -> Self {
        Self
    }
}

impl Formatter for OpenAIChatFormatter {
    type RequestMessage = OpenAIMessage;
    type RequestParams = ChatCompletionRequest;
    type Response = ChatCompletionResponse;

    fn format(
        &self,
        msgs: &[Msg],
        _options: Option<&GenerateOptions>,
    ) -> Result<Vec<Self::RequestMessage>> {
        Ok(msgs.iter().map(convert_message).collect())
    }

    fn parse_response(&self, response: Self::Response) -> Result<ChatResponse> {
        let choice = response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| Error::Model("response contains no choices".into()))?;
        response_to_chat_response(choice.message, response.usage, choice.finish_reason)
    }

    /// 生成参数写入请求体：请求级 `options` 优先，缺失值时回退模型实例的 `defaults`。
    fn apply_options(
        &self,
        params: &mut Self::RequestParams,
        options: &GenerateOptions,
        defaults: Option<&GenerateOptions>,
    ) {
        params.temperature = options
            .temperature
            .or_else(|| defaults.and_then(|d| d.temperature));
        params.max_tokens = options.max_tokens.or_else(|| defaults.and_then(|d| d.max_tokens));
        params.response_format = options
            .response_format
            .clone()
            .or_else(|| defaults.and_then(|d| d.response_format.clone()));
    }

    fn apply_tools(&self, params: &mut Self::RequestParams, tools: &[ToolSchema]) {
        if tools.is_empty() {
            return;
        }
        params.tools = tools_to_openai(tools);
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
            None | Some(ToolChoice::Auto) => json!("auto"),
            Some(ToolChoice::None) => json!("none"),
            Some(ToolChoice::Required) => json!("required"),
            // OpenAI 协议：{"type": "function", "function": {"name": "..."}}
            Some(ToolChoice::Specific { tool_name }) => {
                json!({ "type": "function", "function": { "name": tool_name } })
            }
        });
    }
}

// ---------- Msg → OpenAI 消息 ----------

/// 把 [`ToolSchema`] 列表转换为 OpenAI `tools` 参数。
pub(crate) fn tools_to_openai(tools: &[ToolSchema]) -> Vec<OpenAITool> {
    tools
        .iter()
        .map(|s| OpenAITool {
            kind: "function".into(),
            function: OpenAIFunctionDef {
                name: s.name.clone(),
                description: s.description.clone(),
                parameters: s.parameters.clone(),
            },
        })
        .collect()
}

/// 把一条 [`Msg`] 转换为 OpenAI 协议消息（对应 Java `OpenAIMessageConverter#convertToMessage`）。
pub(crate) fn convert_message(msg: &Msg) -> OpenAIMessage {
    // 对齐 Java：SYSTEM 角色承载 ToolResult 时按 tool 消息发送
    if msg.role == Role::System && has_tool_result(msg) {
        return convert_tool_message(msg);
    }
    let text = extract_text_content(msg);
    let name = Some(sanitize_name(&msg.name));
    // assistant 历史回传思考内容（对齐 Java convertAssistantMessage：取首个
    // ThinkingBlock；推理模型要求 reasoning 保留在请求历史中）。
    let reasoning_content = match msg.role {
        Role::Assistant => msg.content.iter().find_map(|b| match b {
            ContentBlock::Thinking { thinking } => {
                if thinking.is_empty() { None } else { Some(thinking.clone()) }
            }
            _ => None,
        }),
        _ => None,
    };
    match msg.role {
        Role::System => OpenAIMessage {
            role: "system".into(),
            content: Some(Value::String(text)),
            name,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        },
        Role::User => OpenAIMessage {
            role: "user".into(),
            content: Some(user_content_value(msg, text)),
            name,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        },
        Role::Assistant => {
            let tool_calls: Vec<OpenAIToolCall> = msg
                .tool_uses()
                .into_iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse { id, name, input, .. } => Some(OpenAIToolCall {
                        id,
                        kind: "function".into(),
                        function: OpenAIFunction {
                            name,
                            arguments: input.to_string(),
                        },
                    }),
                    _ => None,
                })
                .collect();
            OpenAIMessage {
                role: "assistant".into(),
                // 对齐 Java 版：assistant 的 content 总是发送（空则发 ""）——
                // vLLM 等严格实现要求该字段必须存在，省略会报 "Field required"
                content: Some(Value::String(text)),
                name,
                tool_calls: if tool_calls.is_empty() {
                    None
                } else {
                    Some(tool_calls)
                },
                tool_call_id: None,
                reasoning_content,
            }
        }
        Role::Tool => convert_tool_message(msg),
    }
}

fn has_tool_result(msg: &Msg) -> bool {
    msg.content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
}

/// tool 消息：内容取工具结果的文本形式（媒体块转文本引用），携带 `tool_call_id`、不带 name。
fn convert_tool_message(msg: &Msg) -> OpenAIMessage {
    let (tool_call_id, content) = msg
        .content
        .iter()
        .find_map(|b| match b {
            ContentBlock::ToolResult { id, output, .. } => {
                Some((id.clone(), convert_tool_result_to_string(output)))
            }
            _ => None,
        })
        .unwrap_or_default();
    OpenAIMessage {
        role: "tool".into(),
        content: Some(Value::String(content)),
        name: None,
        tool_calls: None,
        tool_call_id: Some(tool_call_id),
        reasoning_content: None,
    }
}

/// 清洗消息名以满足 OpenAI 的 `^[a-zA-Z0-9_-]{1,64}$` 约束
/// （对齐 Java 版 `OpenAIMessageConverter.sanitizeName`）。
pub(crate) fn sanitize_name(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // 字符已全部映射为 ASCII，按字节截断安全
    let mut sanitized = if sanitized.len() > 64 {
        sanitized[..64].to_string()
    } else {
        sanitized
    };
    if sanitized.is_empty() {
        sanitized = "agent".to_string();
    }
    sanitized
}

/// user 消息内容：无媒体时为纯字符串；含媒体块时为多模态 content 数组。
///
/// MIME 路由对齐 Java 版 `convertDataBlock` 的设计：`image/*` → `image_url`
/// （base64 包成 data URI，http(s) URL 直接透传）；其余 MIME 以文本占位，
/// 避免静默丢失（音频/视频映射留待后续版本）。
fn user_content_value(msg: &Msg, text: String) -> Value {
    let mut arr: Vec<Value> = Vec::new();
    let mut has_media = false;

    for block in &msg.content {
        // 媒体块 → Source；非媒体块为 None
        let media = match block {
            ContentBlock::Image { source, .. }
            | ContentBlock::Audio { source }
            | ContentBlock::Video { source, .. }
            | ContentBlock::Data { source, .. } => Some(source),
            _ => None,
        };
        if let Some(source) = media {
            let (mime_type, url) = match source {
                Source::Base64 { media_type, data } => {
                    let url = if data.starts_with("http://") || data.starts_with("https://") {
                        data.clone()
                    } else {
                        format!("data:{media_type};base64,{data}")
                    };
                    (media_type.clone(), url)
                }
                Source::Url { url, mime_type } => {
                    (mime_type.clone().unwrap_or_default(), url.clone())
                }
            };
            has_media = true;
            if mime_type.starts_with("image/") {
                arr.push(json!({ "type": "image_url", "image_url": { "url": url } }));
            } else {
                arr.push(json!({
                    "type": "text",
                    "text": format!("[Media - unsupported MIME type: {mime_type}]")
                }));
            }
        }
    }

    if !has_media {
        return Value::String(text);
    }
    if !text.is_empty() {
        arr.insert(0, json!({ "type": "text", "text": text }));
    }
    Value::Array(arr)
}

// ---------- OpenAI 响应 → ChatResponse ----------

/// 把 OpenAI 响应消息转换回 [`ChatResponse`]（对应 Java `OpenAIResponseParser`）。
///
/// 块顺序对齐 Java：Thinking（reasoning_content，`reasoning` 为其别名回退）
/// → Text → ToolUse。`reasoning_details`（OpenRouter/Gemini 签名回传）未复刻，
/// Rust 侧 Thinking 块暂无 metadata 载体。
pub(crate) fn response_to_chat_response(
    message: crate::ChoiceMessage,
    usage: Option<OpenAIUsage>,
    finish_reason: Option<String>,
) -> Result<ChatResponse> {
    let mut content = Vec::new();
    let thinking = message
        .reasoning_content
        .filter(|t| !t.is_empty())
        .or_else(|| message.reasoning.filter(|t| !t.is_empty()));
    if let Some(thinking) = thinking {
        content.push(ContentBlock::Thinking { thinking });
    }
    if let Some(text) = message.content {
        if !text.is_empty() {
            content.push(ContentBlock::Text { text });
        }
    }
    if let Some(tool_calls) = message.tool_calls {
        for call in tool_calls {
            let input: Value = serde_json::from_str(&call.function.arguments)
                .map_err(|e| Error::Model(format!("invalid tool arguments json: {e}")))?;
            content.push(ContentBlock::ToolUse {
                id: call.id,
                name: call.function.name,
                input,
                state: None,
                content: None,
            });
        }
    }
    Ok(ChatResponse {
        content,
        usage: usage.map(|u| Usage {
            prompt_tokens: u.prompt_tokens,
            completion_tokens: u.completion_tokens,
            total_tokens: u.total_tokens,
        }),
        finish_reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::formatter::{JsonSchema, ResponseFormat};
    use agentscope_core::message::ToolResultState;

    #[test]
    fn user_msg_conversion() {
        let msg = Msg::user("user", "你好");
        let oai = convert_message(&msg);
        assert_eq!(oai.role, "user");
        assert_eq!(oai.name.as_deref(), Some("user"));
        assert_eq!(oai.content, Some(Value::String("你好".into())));
        assert!(oai.tool_calls.is_none());
    }

    #[test]
    fn assistant_with_tool_calls_conversion() {
        let msg = Msg::assistant(
            "assistant",
            vec![
                ContentBlock::Text {
                    text: "我来算".into(),
                },
                ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "add".into(),
                    input: json!({"a": 1, "b": 2}),
                    state: None,
                    content: None,
                },
            ],
        );
        let oai = convert_message(&msg);
        assert_eq!(oai.role, "assistant");
        assert_eq!(oai.content, Some(Value::String("我来算".into())));
        let calls = oai.tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "add");
        assert_eq!(calls[0].function.arguments, r#"{"a":1,"b":2}"#);
    }

    /// 对齐 Java 版：assistant 的 content 总是发送（空则发 ""），
    /// vLLM 等严格实现要求该字段必须存在。
    #[test]
    fn assistant_empty_content_sends_empty_string() {
        let msg = Msg::assistant(
            "assistant",
            vec![ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "add".into(),
                input: json!({}),
                state: None,
                content: None,
            }],
        );
        let oai = convert_message(&msg);
        assert_eq!(oai.content, Some(Value::String(String::new())));
        assert!(oai.tool_calls.is_some());
    }

    /// 对齐 Java 版 sanitizeName：非法字符 → `_`，超长截断 64，空则 `agent`。
    #[test]
    fn sanitize_name_rules() {
        assert_eq!(sanitize_name("张三 agent#1"), "___agent_1");
        assert_eq!(sanitize_name("agent"), "agent");
        let long = "a".repeat(100);
        assert_eq!(sanitize_name(&long).len(), 64);
    }

    #[test]
    fn tool_result_conversion() {
        let msg = Msg::tool_result(
            "assistant",
            "call_1",
            "get_time",
            "3",
            ToolResultState::Success,
        );
        let oai = convert_message(&msg);
        assert_eq!(oai.role, "tool");
        assert_eq!(oai.tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(oai.content, Some(Value::String("3".into())));
    }

    /// 对齐 Java：SYSTEM 角色承载 ToolResult 块时按 tool 消息发送。
    #[test]
    fn system_with_tool_result_becomes_tool_message() {
        let mut msg = Msg::system("system", "");
        msg.content = vec![ContentBlock::ToolResult {
            id: "call_7".into(),
            name: "get_time".into(),
            output: vec![ContentBlock::Text {
                text: "12:00".into(),
            }],
            state: ToolResultState::Success,
            execution_details: None,
        }];
        let oai = convert_message(&msg);
        assert_eq!(oai.role, "tool");
        assert_eq!(oai.tool_call_id.as_deref(), Some("call_7"));
        assert_eq!(oai.content, Some(Value::String("12:00".into())));
    }

    /// 多模态：Image 块 → image_url（base64 包 data URI）；Data 块按 MIME 路由，
    /// 非 image MIME → 文本占位（对齐 Java 版 MIME 路由设计）。
    #[test]
    fn multimodal_image_and_unsupported_mime() {
        let msg = Msg::new(
            Role::User,
            "user",
            vec![
                ContentBlock::Text {
                    text: "看图".into(),
                },
                ContentBlock::Image {
                    source: Source::Base64 {
                        media_type: "image/png".into(),
                        data: "aGVsbG8=".into(),
                    },
                    min_pixels: None,
                    max_pixels: None,
                },
                ContentBlock::Data {
                    source: Source::Base64 {
                        media_type: "audio/wav".into(),
                        data: "cnVieQ==".into(),
                    },
                    name: None,
                },
            ],
        );
        let oai = convert_message(&msg);
        let content = oai.content.unwrap();
        let arr = content.as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(arr[1]["type"], "image_url");
        assert_eq!(arr[1]["image_url"]["url"], "data:image/png;base64,aGVsbG8=");
        // 非 image MIME 以文本占位，不静默丢失
        assert_eq!(arr[2]["type"], "text");
        assert!(arr[2]["text"].as_str().unwrap().contains("audio/wav"));
    }

    #[test]
    fn response_parsing() {
        let raw = json!({
            "content": "答案是 3",
            "tool_calls": [{
                "id": "call_9",
                "type": "function",
                "function": { "name": "add", "arguments": "{\"a\":1,\"b\":2}" }
            }]
        });
        let message: crate::ChoiceMessage = serde_json::from_value(raw).unwrap();
        let resp = response_to_chat_response(message, None, Some("tool_calls".into())).unwrap();
        assert_eq!(resp.content.len(), 2);
        assert!(matches!(&resp.content[0], ContentBlock::Text { text } if text == "答案是 3"));
        assert!(matches!(&resp.content[1], ContentBlock::ToolUse { name, .. } if name == "add"));
        assert_eq!(resp.finish_reason.as_deref(), Some("tool_calls"));
    }

    /// 非流式思考内容：reasoning_content → Thinking 块，排在 Text 之前
    /// （对齐 Java parseCompletionResponse 的块顺序 Thinking → Text → ToolUse）。
    #[test]
    fn response_parses_thinking_before_text() {
        let raw = json!({ "content": "答案", "reasoning_content": "推理过程" });
        let message: crate::ChoiceMessage = serde_json::from_value(raw).unwrap();
        let resp = response_to_chat_response(message, None, Some("stop".into())).unwrap();
        assert_eq!(resp.content.len(), 2);
        assert!(matches!(&resp.content[0], ContentBlock::Thinking { thinking } if thinking == "推理过程"));
        assert!(matches!(&resp.content[1], ContentBlock::Text { text } if text == "答案"));
    }

    /// `reasoning` 字段名回退（vLLM 部署形态；对齐 Java @JsonAlias("reasoning")）。
    #[test]
    fn response_parses_reasoning_alias() {
        let raw = json!({ "content": "答案", "reasoning": "alias 推理" });
        let message: crate::ChoiceMessage = serde_json::from_value(raw).unwrap();
        let resp = response_to_chat_response(message, None, None).unwrap();
        assert!(matches!(&resp.content[0], ContentBlock::Thinking { thinking } if thinking == "alias 推理"));
    }

    /// assistant 历史回传：Thinking 块 → reasoning_content 字段
    /// （对齐 Java convertAssistantMessage；DeepSeek thinking 模式要求保留）。
    #[test]
    fn assistant_thinking_backfills_reasoning_content() {
        let msg = Msg::assistant(
            "assistant",
            vec![
                ContentBlock::Thinking { thinking: "推理".into() },
                ContentBlock::Text { text: "答案".into() },
            ],
        );
        let oai = convert_message(&msg);
        assert_eq!(oai.reasoning_content.as_deref(), Some("推理"));
        let v = serde_json::to_value(&oai).unwrap();
        assert_eq!(v["reasoning_content"], "推理");

        // 无思考块时不携带该字段（skip_serializing_if 生效）
        let plain = Msg::assistant("assistant", vec![ContentBlock::Text { text: "hi".into() }]);
        let v = serde_json::to_value(convert_message(&plain)).unwrap();
        assert!(v.get("reasoning_content").is_none());
    }

    // ---------- apply_options / apply_tools / apply_tool_choice ----------

    fn schema(name: &str) -> ToolSchema {
        ToolSchema {
            name: name.into(),
            description: "d".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    /// 请求级 options 优先，缺失时回退模型默认值（对齐 Java `getOptionOrDefault`）。
    #[test]
    fn apply_options_prefers_request_then_defaults() {
        let f = OpenAIChatFormatter::new();
        let defaults = GenerateOptions {
            temperature: Some(0.1),
            max_tokens: Some(512),
            ..Default::default()
        };
        let options = GenerateOptions {
            temperature: Some(0.9),
            ..Default::default()
        };
        let mut params = ChatCompletionRequest::default();
        f.apply_options(&mut params, &options, Some(&defaults));
        assert_eq!(params.temperature, Some(0.9));
        assert_eq!(params.max_tokens, Some(512));
    }

    /// 工具定义下发：即使没有 strict 字段也保持原有形状（strict 属待补能力）。
    #[test]
    fn apply_tools_builds_function_defs() {
        let f = OpenAIChatFormatter::new();
        let mut params = ChatCompletionRequest::default();
        f.apply_tools(&mut params, &[schema("add")]);
        assert_eq!(params.tools.len(), 1);
        assert_eq!(params.tools[0].kind, "function");
        assert_eq!(params.tools[0].function.name, "add");
    }

    /// 无工具时即使指定 tool_choice 也不下发（对齐 Java `applyToolChoice` 的前置判断）。
    #[test]
    fn apply_tool_choice_skipped_without_tools() {
        let f = OpenAIChatFormatter::new();
        let mut params = ChatCompletionRequest::default();
        f.apply_tool_choice(&mut params, Some(&ToolChoice::Required));
        assert!(params.tool_choice.is_none());
    }

    #[test]
    fn apply_tool_choice_variants() {
        let f = OpenAIChatFormatter::new();
        let mut params = ChatCompletionRequest {
            tools: tools_to_openai(&[schema("add")]),
            ..Default::default()
        };
        f.apply_tool_choice(&mut params, Some(&ToolChoice::Auto));
        assert_eq!(params.tool_choice, Some(json!("auto")));
        f.apply_tool_choice(&mut params, Some(&ToolChoice::None));
        assert_eq!(params.tool_choice, Some(json!("none")));
        f.apply_tool_choice(&mut params, Some(&ToolChoice::Required));
        assert_eq!(params.tool_choice, Some(json!("required")));
        f.apply_tool_choice(
            &mut params,
            Some(&ToolChoice::Specific {
                tool_name: "add".into(),
            }),
        );
        assert_eq!(
            params.tool_choice,
            Some(json!({"type": "function", "function": {"name": "add"}}))
        );
    }

    /// 结构化输出：response_format 原样进入请求体（json_schema 形态）。
    #[test]
    fn apply_options_carries_response_format() {
        let f = OpenAIChatFormatter::new();
        let mut params = ChatCompletionRequest::default();
        let options = GenerateOptions {
            response_format: Some(ResponseFormat::json_schema(
                JsonSchema::new("Answer", json!({"type": "object"}))
                    .with_description("答案结构")
                    .with_strict(true),
            )),
            ..Default::default()
        };
        f.apply_options(&mut params, &options, None);
        let v = serde_json::to_value(&params).unwrap();
        assert_eq!(v["response_format"]["type"], "json_schema");
        assert_eq!(v["response_format"]["json_schema"]["name"], "Answer");
        assert_eq!(v["response_format"]["json_schema"]["strict"], true);
    }
}
