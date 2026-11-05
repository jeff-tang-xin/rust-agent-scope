//! OpenAI Responses API 的消息格式化实现。
//!
//! 对应 Java 版 `io.agentscope.extensions.model.openaiofficial.formatter` 包：
//! - [`OpenAiResponsesFormatter`] ↔ `ResponsesChatFormatter` 一族
//!   （format / applyOptions / applyTools / applyToolChoice）；
//! - `convert_message` ↔ `ResponsesRequestMapper`（Msg → `input[]` 条目）；
//! - `parse_response` ↔ `ResponsesResponseParser`（响应 → `ChatResponse`）。
//!
//! 与 Java 版的已知差异（裁剪口径）：
//! - ServerTool（web_search / file_search / tool_search）、MultiAgentFormatter、
//!   stateful 会话（`previous_response_id`）未复刻；
//! - reasoning 回放不做：Java 要求 ThinkingBlock 带 `encrypted_content` 元数据
//!   才回放为 reasoning 条目，Rust `ThinkingBlock` 无 metadata 载体——
//!   assistant 历史 Thinking 块直接跳过（响应侧仍解析思考增量）；
//! - 非 image MIME 媒体块不抛异常、转文本占位（对齐 openai crate 的 MIME
//!   路由，比 Java 宽容）；
//! - `additionalBodyParams` 白名单、retry/timeout 执行配置未复刻。

use serde_json::{json, Value};

use agentscope_core::error::{Error, Result};
use agentscope_core::formatter::{convert_tool_result_to_string, extract_text_content, Formatter};
use agentscope_core::message::{ContentBlock, Msg, Role, Source};
use agentscope_core::model::{ChatResponse, GenerateOptions, ToolChoice, ToolSchema, Usage};

use crate::{
    InputItem, OutputContentPart, OutputItem, ReasoningPart, ResponsesError, ResponsesRequest,
    ResponsesResponse, ResponsesTool, ResponsesUsage,
};

/// OpenAI Responses API 格式化器（对应 Java `ResponsesChatFormatter` 一族）。
#[derive(Debug, Default, Clone, Copy)]
pub struct OpenAiResponsesFormatter;

impl OpenAiResponsesFormatter {
    pub fn new() -> Self {
        Self
    }

    /// 单条消息转换（对齐 Java `ResponsesRequestMapper`）：
    /// - ToolResult 块 → `function_call_output` 条目（全文本 → 字符串，
    ///   混合 → `input_text` / `input_image` parts 数组）；
    /// - assistant：ToolUse → `function_call` 条目、Text → `message` 条目
    ///   （`output_text` parts）、Thinking 跳过（reasoning 回放不做）；
    /// - 其他 → `message` 条目（纯文本 → 字符串 content，含图 → parts 数组）。
    ///
    /// 一条消息可产出多个条目（如 assistant 带工具调用 = message + function_call）。
    pub fn convert_message(&self, msg: &Msg) -> Vec<InputItem> {
        let mut items = Vec::new();

        // 1) 工具结果 → function_call_output（一条消息多个结果块逐条展开）
        for block in &msg.content {
            if let ContentBlock::ToolResult { id, output, .. } = block {
                items.push(InputItem::FunctionCallOutput {
                    call_id: id.clone(),
                    output: tool_output_value(output),
                });
            }
        }
        if !items.is_empty() && msg.role != Role::Assistant {
            // 非 assistant 消息只含工具结果时不再追加 message 条目
            // （对齐 Java：tool 角色消息只产出 function_call_output）
            if msg.content.iter().all(|b| matches!(b, ContentBlock::ToolResult { .. })) {
                return items;
            }
        }

        // 2) assistant：工具调用 + 文本（Thinking 跳过）
        if msg.role == Role::Assistant {
            for block in &msg.content {
                match block {
                    ContentBlock::ToolUse { id, name, input, .. } => {
                        items.push(InputItem::FunctionCall {
                            call_id: id.clone(),
                            name: name.clone(),
                            arguments: input.to_string(),
                        });
                    }
                    ContentBlock::Text { text } if !text.is_empty() => {
                        items.push(InputItem::Message {
                            role: "assistant".into(),
                            content: json!([{ "type": "output_text", "text": text }]),
                        });
                    }
                    // Thinking / 空 Text / 其他块跳过
                    _ => {}
                }
            }
            return items;
        }

        // 3) system / user → message 条目
        let role = match msg.role {
            Role::System => "system",
            Role::User => "user",
            // Tool 角色无结果块的裸消息按 user 处理（协议无 tool 角色 message）
            Role::Tool => "user",
            Role::Assistant => unreachable!("assistant handled above"),
        };
        items.push(InputItem::Message {
            role: role.into(),
            content: user_content_value(msg),
        });
        items
    }

    // ---------- 响应解析 ----------

    /// 非流式响应解析（对齐 Java `ResponsesResponseParser`）：
    /// `output[]` 顺序遍历——reasoning（summary 与原始文本 → Thinking）、
    /// message（`output_text` → Text；`refusal` 短路报错）、function_call →
    /// ToolUse；usage 按 input/output/total_tokens 映射；status 作为
    /// finish_reason 透传。
    pub fn parse_response(&self, response: ResponsesResponse) -> Result<ChatResponse> {
        if let Some(err) = response.error {
            return Err(Error::Model(format!(
                "openai responses error {}: {}",
                err.code.as_deref().unwrap_or("unknown"),
                err.message.as_deref().unwrap_or("unknown")
            )));
        }

        let mut content = Vec::new();
        for item in response.output {
            match item {
                OutputItem::Reasoning { summary, content: parts } => {
                    let mut texts: Vec<String> = summary
                        .into_iter()
                        .map(|ReasoningPart { text }| text)
                        .collect();
                    texts.extend(parts.into_iter().map(|ReasoningPart { text }| text));
                    let thinking = texts.join("");
                    if !thinking.is_empty() {
                        content.push(ContentBlock::Thinking { thinking });
                    }
                }
                OutputItem::Message { content: parts, .. } => {
                    for part in parts {
                        match part.kind.as_str() {
                            "output_text" if !part.text.is_empty() => {
                                content.push(ContentBlock::Text { text: part.text });
                            }
                            "refusal" if !part.refusal.is_empty() => {
                                // refusal 短路报错（对齐 Java）
                                return Err(Error::Model(format!(
                                    "model refused: {}",
                                    part.refusal
                                )));
                            }
                            _ => {}
                        }
                    }
                }
                OutputItem::FunctionCall {
                    call_id,
                    name,
                    arguments,
                } => {
                    let input: Value = if arguments.trim().is_empty() {
                        Value::Object(serde_json::Map::new())
                    } else {
                        serde_json::from_str(&arguments)
                            .map_err(|e| Error::Model(format!("invalid tool arguments json: {e}")))?
                    };
                    content.push(ContentBlock::ToolUse {
                        id: call_id,
                        name,
                        input,
                        state: None,
                        content: None,
                    });
                }
                OutputItem::Other => {}
            }
        }

        let usage = response.usage.map(|u: ResponsesUsage| Usage {
            prompt_tokens: u.input_tokens,
            completion_tokens: u.output_tokens,
            total_tokens: u.total_tokens,
        });
        Ok(ChatResponse {
            content,
            usage,
            finish_reason: response.status,
        })
    }

    // ---------- 参数 / 工具 ----------

    /// 生成参数写入请求体：请求级 `options` 优先，缺失回退模型默认。
    /// 裁剪口径：只落 temperature / max_output_tokens。
    pub fn apply_options(
        &self,
        req: &mut ResponsesRequest,
        options: &GenerateOptions,
        defaults: Option<&GenerateOptions>,
    ) {
        req.temperature = options.temperature.or(defaults.and_then(|d| d.temperature));
        req.max_output_tokens = match (options.max_tokens, defaults.and_then(|d| d.max_tokens)) {
            (Some(v), _) => Some(v),
            (None, Some(v)) => Some(v),
            (None, None) => None,
        };
    }

    /// 工具定义写入 `tools`（扁平结构：`{type:"function",name,description,
    /// parameters,strict}`；空参数兜底 `{type:object,properties:{},required:[]}`）。
    pub fn apply_tools(&self, req: &mut ResponsesRequest, tools: &[ToolSchema]) {
        req.tools = tools
            .iter()
            .map(|t| {
                let parameters = if t.parameters.is_null() {
                    json!({"type": "object", "properties": {}, "required": []})
                } else {
                    t.parameters.clone()
                };
                ResponsesTool {
                    kind: "function".into(),
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters,
                    strict: false,
                }
            })
            .collect();
    }

    /// 工具选择写入 `tool_choice`（对齐 Java `ResponsesRequestMapper`）：
    /// Auto → `"auto"`；None → `"none"`；Required → `"required"`；
    /// Specific → 扁平 `{"type":"function","name":...}`。
    pub fn apply_tool_choice(
        &self,
        req: &mut ResponsesRequest,
        tool_choice: Option<&ToolChoice>,
    ) {
        req.tool_choice = match tool_choice {
            None | Some(ToolChoice::Auto) => Some(json!("auto")),
            Some(ToolChoice::None) => Some(json!("none")),
            Some(ToolChoice::Required) => Some(json!("required")),
            Some(ToolChoice::Specific { tool_name }) => Some(json!({
                "type": "function",
                "name": tool_name
            })),
        };
    }
}

/// 工具结果 output 块列表 → `function_call_output.output` 值：
/// 全文本 → 字符串；混合（含图）→ `input_text` / `input_image` parts 数组
/// （对齐 Java `ResponsesRequestMapper` 的 output 形态路由）。
fn tool_output_value(output: &[ContentBlock]) -> Value {
    let has_media = output.iter().any(|b| {
        matches!(
            b,
            ContentBlock::Image { .. }
                | ContentBlock::Audio { .. }
                | ContentBlock::Video { .. }
                | ContentBlock::Data { .. }
        )
    });
    if !has_media {
        return Value::String(convert_tool_result_to_string(output));
    }
    let mut parts = Vec::new();
    for block in output {
        match block {
            ContentBlock::Text { text } => parts.push(json!({
                "type": "input_text",
                "text": text
            })),
            ContentBlock::Image { source, .. } => {
                if let Some(url) = image_url(source) {
                    parts.push(json!({ "type": "input_image", "image_url": url }));
                }
            }
            // 音视频等 Responses API 不支持，跳过（文本结果已由 convert 兜底）
            _ => {}
        }
    }
    Value::Array(parts)
}

/// user / system 消息 → `message.content` 值：纯文本 → 字符串；
/// 含图 → `input_text` / `input_image` parts 数组。
fn user_content_value(msg: &Msg) -> Value {
    let has_media = msg.content.iter().any(|b| {
        matches!(
            b,
            ContentBlock::Image { .. }
                | ContentBlock::Audio { .. }
                | ContentBlock::Video { .. }
                | ContentBlock::Data { .. }
        )
    });
    if !has_media {
        return Value::String(extract_text_content(msg));
    }
    let mut parts = Vec::new();
    for block in &msg.content {
        match block {
            ContentBlock::Text { text } => parts.push(json!({
                "type": "input_text",
                "text": text
            })),
            ContentBlock::Hint { hint, .. } => parts.push(json!({
                "type": "input_text",
                "text": hint
            })),
            ContentBlock::Image { source, .. } => {
                if let Some(url) = image_url(source) {
                    parts.push(json!({ "type": "input_image", "image_url": url }));
                }
            }
            // 音视频等 Responses API 不支持，转占位避免静默丢失
            ContentBlock::Audio { .. } | ContentBlock::Video { .. } | ContentBlock::Data { .. } => {
                parts.push(json!({
                    "type": "input_text",
                    "text": "[Media - unsupported MIME type]"
                }));
            }
            // 思考块不回传；工具块不进 message content
            ContentBlock::Thinking { .. } | ContentBlock::ToolUse { .. } | ContentBlock::ToolResult { .. } => {}
        }
    }
    Value::Array(parts)
}

/// Source → 图片 URL：Url 直取；Base64 转 data URL（Responses API 只收 URL）。
fn image_url(source: &Source) -> Option<String> {
    match source {
        Source::Url { url, .. } => Some(url.clone()),
        Source::Base64 {
            media_type,
            data,
        } => Some(format!("data:{media_type};base64,{data}")),
    }
}

impl Formatter for OpenAiResponsesFormatter {
    type RequestMessage = InputItem;
    type RequestParams = ResponsesRequest;
    type Response = ResponsesResponse;

    fn format(
        &self,
        msgs: &[Msg],
        _options: Option<&GenerateOptions>,
    ) -> Result<Vec<Self::RequestMessage>> {
        Ok(msgs
            .iter()
            .flat_map(|m| self.convert_message(m))
            .collect())
    }

    fn parse_response(&self, response: Self::Response) -> Result<ChatResponse> {
        self.parse_response(response)
    }

    fn apply_options(
        &self,
        params: &mut Self::RequestParams,
        options: &GenerateOptions,
        defaults: Option<&GenerateOptions>,
    ) {
        self.apply_options(params, options, defaults)
    }

    fn apply_tools(&self, params: &mut Self::RequestParams, tools: &[ToolSchema]) {
        self.apply_tools(params, tools)
    }

    fn apply_tool_choice(
        &self,
        params: &mut Self::RequestParams,
        tool_choice: Option<&ToolChoice>,
    ) {
        self.apply_tool_choice(params, tool_choice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::message::ToolResultState;
    use serde_json::json;

    /// user / system / assistant 消息 → message 条目（system 走 input 条目，
    /// 不用 instructions 字段）。
    #[test]
    fn converts_plain_messages() {
        let f = OpenAiResponsesFormatter::new();
        let msgs = vec![
            Msg::new(
                Role::System,
                "system",
                vec![ContentBlock::Text {
                    text: "你是助手".into(),
                }],
            ),
            Msg::user("user", "你好"),
            Msg::assistant(
                "assistant",
                vec![
                    ContentBlock::Thinking {
                        thinking: "推理".into(),
                    },
                    ContentBlock::Text {
                        text: "答案".into(),
                    },
                ],
            ),
        ];
        let items = f.format(&msgs, None).unwrap();
        assert_eq!(items.len(), 3);
        let serde_vals: Vec<Value> = items
            .iter()
            .map(|i| serde_json::to_value(i).unwrap())
            .collect();
        // system 走 input 条目
        assert_eq!(serde_vals[0]["type"], "message");
        assert_eq!(serde_vals[0]["role"], "system");
        assert_eq!(serde_vals[0]["content"], json!("你是助手"));
        assert_eq!(serde_vals[1]["role"], "user");
        // assistant：Thinking 跳过，Text → output_text parts
        assert_eq!(serde_vals[2]["role"], "assistant");
        assert_eq!(
            serde_vals[2]["content"],
            json!([{ "type": "output_text", "text": "答案" }])
        );
    }

    /// assistant 带工具调用 → message + function_call 条目。
    #[test]
    fn converts_assistant_tool_calls() {
        let f = OpenAiResponsesFormatter::new();
        let msg = Msg::assistant(
            "assistant",
            vec![
                ContentBlock::Text {
                    text: "我来算".into(),
                },
                ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "add".into(),
                    input: json!({"a": 1}),
                    state: None,
                    content: None,
                },
            ],
        );
        let items = f.convert_message(&msg);
        assert_eq!(items.len(), 2);
        let v0 = serde_json::to_value(&items[0]).unwrap();
        let v1 = serde_json::to_value(&items[1]).unwrap();
        assert_eq!(v0["type"], "message");
        assert_eq!(v1["type"], "function_call");
        assert_eq!(v1["call_id"], "call_1");
        assert_eq!(v1["name"], "add");
        assert_eq!(v1["arguments"], json!({"a": 1}).to_string());
    }

    /// 工具结果 → function_call_output（全文本 → 字符串）。
    #[test]
    fn converts_tool_result_to_string_output() {
        let f = OpenAiResponsesFormatter::new();
        let msg = Msg::new(
            Role::Tool,
            "tool",
            vec![ContentBlock::ToolResult {
                id: "call_1".into(),
                name: "add".into(),
                output: vec![ContentBlock::Text {
                    text: "3".into(),
                }],
                state: ToolResultState::Success,
                execution_details: None,
            }],
        );
        let items = f.convert_message(&msg);
        assert_eq!(items.len(), 1);
        let v = serde_json::to_value(&items[0]).unwrap();
        assert_eq!(v["type"], "function_call_output");
        assert_eq!(v["call_id"], "call_1");
        assert_eq!(v["output"], json!("3"));
    }

    /// 含图 user 消息 → input_text / input_image parts 数组。
    #[test]
    fn converts_user_image_to_parts() {
        let f = OpenAiResponsesFormatter::new();
        let msg = Msg::new(
            Role::User,
            "user",
            vec![
                ContentBlock::Text {
                    text: "看图".into(),
                },
                ContentBlock::Image {
                    source: Source::Url {
                        url: "https://example.com/a.png".into(),
                        mime_type: Some("image/png".into()),
                    },
                    min_pixels: None,
                    max_pixels: None,
                },
            ],
        );
        let items = f.convert_message(&msg);
        let v = serde_json::to_value(&items[0]).unwrap();
        assert_eq!(
            v["content"],
            json!([
                {"type": "input_text", "text": "看图"},
                {"type": "input_image", "image_url": "https://example.com/a.png"}
            ])
        );
    }

    /// 响应解析：reasoning → Thinking、output_text → Text、function_call →
    /// ToolUse；refusal 短路报错；status 作 finish_reason；usage 映射。
    #[test]
    fn parses_response_items() {
        let f = OpenAiResponsesFormatter::new();
        let response: ResponsesResponse = serde_json::from_str(
            r#"{
                "status": "completed",
                "output": [
                    {"type": "reasoning", "summary": [{"type": "summary_text", "text": "想一想"}]},
                    {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "答案"}]},
                    {"type": "function_call", "call_id": "call_1", "name": "add", "arguments": "{\"a\":1}"}
                ],
                "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15}
            }"#,
        )
        .unwrap();
        let parsed = f.parse_response(response).unwrap();
        assert_eq!(parsed.finish_reason.as_deref(), Some("completed"));
        assert_eq!(parsed.content.len(), 3);
        assert!(matches!(&parsed.content[0], ContentBlock::Thinking { thinking } if thinking == "想一想"));
        assert!(matches!(&parsed.content[1], ContentBlock::Text { text } if text == "答案"));
        assert!(matches!(&parsed.content[2], ContentBlock::ToolUse { name, input, .. } if name == "add" && input == &json!({"a": 1})));
        let usage = parsed.usage.unwrap();
        assert_eq!(
            (usage.prompt_tokens, usage.completion_tokens, usage.total_tokens),
            (10, 5, 15)
        );
    }

    /// refusal 短路报错。
    #[test]
    fn refusal_short_circuits_with_error() {
        let f = OpenAiResponsesFormatter::new();
        let response: ResponsesResponse = serde_json::from_str(
            r#"{"status": "completed", "output": [{"type": "message", "role": "assistant", "content": [{"type": "refusal", "refusal": "不行"}]}]}"#,
        )
        .unwrap();
        let e = f.parse_response(response).unwrap_err();
        assert!(e.to_string().contains("不行"));
    }

    /// 顶层 error 对象 → 报错。
    #[test]
    fn error_object_short_circuits() {
        let f = OpenAiResponsesFormatter::new();
        let response: ResponsesResponse = serde_json::from_str(
            r#"{"status": "failed", "error": {"code": "invalid_key", "message": "bad key"}}"#,
        )
        .unwrap();
        let e = f.parse_response(response).unwrap_err();
        assert!(e.to_string().contains("invalid_key"));
    }

    /// 工具定义扁平结构 + 空参数兜底；tool_choice 四变体（Specific 扁平形态）。
    #[test]
    fn applies_tools_and_tool_choice() {
        let f = OpenAiResponsesFormatter::new();
        let mut req = ResponsesRequest {
            model: "gpt-5".into(),
            input: Vec::new(),
            store: false,
            stream: false,
            temperature: None,
            max_output_tokens: None,
            tools: Vec::new(),
            tool_choice: None,
        };
        let tools = vec![
            ToolSchema {
                name: "add".into(),
                description: "加法".into(),
                parameters: json!({"type": "object"}),
            },
            ToolSchema {
                name: "now".into(),
                description: "时间".into(),
                parameters: Value::Null,
            },
        ];
        f.apply_tools(&mut req, &tools);
        assert_eq!(req.tools.len(), 2);
        let v = serde_json::to_value(&req.tools).unwrap();
        assert_eq!(v[0]["type"], "function");
        assert_eq!(v[0]["name"], "add");
        assert_eq!(v[0]["strict"], false);
        // 空参数兜底
        assert_eq!(
            v[1]["parameters"],
            json!({"type": "object", "properties": {}, "required": []})
        );

        f.apply_tool_choice(&mut req, Some(&ToolChoice::Auto));
        assert_eq!(req.tool_choice.as_ref().unwrap(), &json!("auto"));
        f.apply_tool_choice(&mut req, Some(&ToolChoice::None));
        assert_eq!(req.tool_choice.as_ref().unwrap(), &json!("none"));
        f.apply_tool_choice(&mut req, Some(&ToolChoice::Required));
        assert_eq!(req.tool_choice.as_ref().unwrap(), &json!("required"));
        f.apply_tool_choice(
            &mut req,
            Some(&ToolChoice::Specific {
                tool_name: "add".into(),
            }),
        );
        assert_eq!(
            req.tool_choice.as_ref().unwrap(),
            &json!({"type": "function", "name": "add"})
        );
    }

    /// 参数应用：请求级优先，缺失回退默认（max_output_tokens 字段名）。
    #[test]
    fn applies_options_with_fallback() {
        let f = OpenAiResponsesFormatter::new();
        let mut req = ResponsesRequest {
            model: "gpt-5".into(),
            input: Vec::new(),
            store: false,
            stream: false,
            temperature: None,
            max_output_tokens: None,
            tools: Vec::new(),
            tool_choice: None,
        };
        let defaults = GenerateOptions {
            temperature: Some(0.5),
            max_tokens: Some(128),
            ..Default::default()
        };
        let options = GenerateOptions {
            temperature: Some(0.9),
            ..Default::default()
        };
        f.apply_options(&mut req, &options, Some(&defaults));
        assert_eq!(req.temperature, Some(0.9));
        assert_eq!(req.max_output_tokens, Some(128));
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["max_output_tokens"], 128);
    }
}
