//! DashScope 原生协议的消息格式化实现。
//!
//! 对应 Java 版 `io.agentscope.extensions.model.dashscope.formatter` 包：
//! - [`DashScopeChatFormatter`] ↔ `DashScopeChatFormatter`
//!   （format / applyOptions / applyTools / applyToolChoice）；
//! - `convert_message` ↔ `DashScopeMessageConverter`（简单 content 形态，
//!   即 Java `convertToSimpleContent`；多模态数组形态未复刻）；
//! - `parse_response` ↔ `DashScopeResponseParser`（响应 → `ChatResponse`）；
//! - `apply_tool_choice` ↔ `DashScopeToolsHelper.convertToolChoice`。
//!
//! 与 Java 版的已知差异（裁剪口径）：
//! - 只做简单 content（字符串）形态；媒体块转文本占位（Java 简单格式直接
//!   丢弃媒体块，此处保留占位避免静默丢失，多模态数组形态未复刻）；
//! - 工具调用分片（`__fragment__`）不在 formatter 聚合——流式分片由
//!   `crate::pump_sse` 按 index 聚合完整后产出，非流式响应的 tool_calls
//!   已是完整调用；
//! - `DashScopeMultiModalTool` / search_options / encryption 未复刻。

use serde_json::{json, Value};

use agentscope_core::error::{Error, Result};
use agentscope_core::formatter::{convert_tool_result_to_string, extract_text_content, media, Formatter};
use agentscope_core::message::{ContentBlock, Msg, Role, Source};
use agentscope_core::model::{ChatResponse, GenerateOptions, ToolChoice, ToolSchema, Usage};

use crate::{
    DashScopeFunction, DashScopeMessage, DashScopeParameters, DashScopeResponse, DashScopeTool,
    DashScopeToolCall, DashScopeToolFunction,
};

/// DashScope 原生协议格式化器（对应 Java `DashScopeChatFormatter`）。
#[derive(Debug, Default, Clone, Copy)]
pub struct DashScopeChatFormatter;

impl DashScopeChatFormatter {
    pub fn new() -> Self {
        Self
    }

    /// 单条消息转换（对齐 Java `convertToSimpleContent`）：
    /// - 含 ToolResult 块 → `role="tool"` + `tool_call_id` + `name` + 文本 content
    ///   （Java 限定 TOOL/SYSTEM 角色，Rust `Role` 无 Tool 变体，按块判定）；
    /// - assistant 带 ToolUse → `tool_calls` + content（空文本发 `""`——
    ///   DashScope 要求 content 字段必须存在，Qwen3 思考模式的历史消息
    ///   可能只有 reasoning_content + tool_calls）；
    /// - 其他 → content 为全部文本拼接；媒体块转文本占位。
    pub fn convert_message(&self, msg: &Msg) -> DashScopeMessage {
        // 1) 工具结果 → role="tool"
        let tool_results: Vec<&ContentBlock> = msg
            .content
            .iter()
            .filter(|b| matches!(b, ContentBlock::ToolResult { .. }))
            .collect();
        if let Some(ContentBlock::ToolResult { id, name, output, .. }) = tool_results.first() {
            return DashScopeMessage {
                role: "tool".into(),
                tool_call_id: Some(id.clone()),
                name: Some(name.clone()),
                content: Some(Value::String(convert_tool_result_to_string(output))),
                ..Default::default()
            };
        }

        let role = match msg.role {
            Role::System => "system",
            Role::Assistant => "assistant",
            // Tool 角色消息已被上面的 ToolResult 分支拦截；走到这里说明
            // 是无结果块的裸 tool 消息，仍以 role="tool" 发出（对齐 Java
            // TOOL 角色语义）。
            Role::Tool => "tool",
            Role::User => "user",
        }
        .to_string();

        // 2) assistant 历史里的工具调用 → tool_calls
        if msg.role == Role::Assistant {
            let tool_calls: Vec<DashScopeToolCall> = msg
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse { id, name, input, .. } => Some(DashScopeToolCall {
                        id: Some(id.clone()),
                        kind: "function".into(),
                        function: DashScopeFunction {
                            name: Some(name.clone()),
                            arguments: Some(input.to_string()),
                        },
                        index: None,
                    }),
                    _ => None,
                })
                .collect();
            if !tool_calls.is_empty() {
                let text = extract_text_content(msg);
                let reasoning: Option<String> = msg.content.iter().find_map(|b| match b {
                    ContentBlock::Thinking { thinking } => Some(thinking.clone()),
                    _ => None,
                });
                return DashScopeMessage {
                    role,
                    // DashScope 要求 content 字段必须存在（空串也行）
                    content: Some(Value::String(text)),
                    tool_calls: Some(tool_calls),
                    reasoning_content: reasoning,
                    ..Default::default()
                };
            }
        }

        // 3) 普通消息：文本拼接 + 媒体占位；Thinking 块走 reasoning_content
        // 字段回传（对齐 Java DashScopeMessage.reasoningContent）
        let reasoning: Option<String> = msg.content.iter().find_map(|b| match b {
            ContentBlock::Thinking { thinking } => Some(thinking.clone()),
            _ => None,
        });
        DashScopeMessage {
            role,
            content: Some(Value::String(self.text_with_media_placeholders(msg))),
            reasoning_content: reasoning,
            ..Default::default()
        }
    }

    /// 文本内容 + 媒体块占位（简单格式不发多模态数组，占位避免静默丢失）。
    fn text_with_media_placeholders(&self, msg: &Msg) -> String {
        let mut parts: Vec<String> = Vec::new();
        for block in &msg.content {
            match block {
                ContentBlock::Text { text } => parts.push(text.clone()),
                ContentBlock::Hint { hint, .. } => parts.push(hint.clone()),
                ContentBlock::Image { source, .. }
                | ContentBlock::Audio { source }
                | ContentBlock::Video { source, .. }
                | ContentBlock::Data { source, .. } => {
                    let mime = match source {
                        Source::Url { url, .. } => media::determine_media_type(url),
                        Source::Base64 { media_type, .. } => media_type.clone(),
                    };
                    parts.push(format!("[Media - unsupported MIME type: {mime}]"));
                }
                // 思考块不回传为普通文本（历史回传走 reasoning_content 字段）
                ContentBlock::Thinking { .. } => {}
                ContentBlock::ToolUse { .. } | ContentBlock::ToolResult { .. } => {}
            }
        }
        parts.join("")
    }

    // ---------- 响应解析 ----------

    /// 非流式响应解析（对齐 Java `DashScopeResponseParser.parseResponse`）：
    /// `reasoning_content` → Thinking、`content` → Text、`tool_calls` → ToolUse；
    /// finish_reason 取 choice 级、缺失回退 output 级；usage 按
    /// input/output/total_tokens 映射。
    pub fn parse_response(&self, response: DashScopeResponse) -> Result<ChatResponse> {
        let output = response
            .output
            .ok_or_else(|| Error::Model("response contains no output".into()))?;
        let choice = output
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| Error::Model("response contains no choices".into()))?;
        let finish_reason = choice.finish_reason.or(output.finish_reason);
        let message = choice
            .message
            .ok_or_else(|| Error::Model("response choice contains no message".into()))?;

        let mut content = Vec::new();
        if let Some(reasoning) = message.reasoning_content {
            if !reasoning.is_empty() {
                content.push(ContentBlock::Thinking { thinking: reasoning });
            }
        }
        match message.content {
            Some(Value::String(text)) if !text.is_empty() => {
                content.push(ContentBlock::Text { text });
            }
            Some(Value::Array(parts)) => {
                // 多模态数组形态：取文本部分（裁剪口径）
                let text = parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("");
                if !text.is_empty() {
                    content.push(ContentBlock::Text { text });
                }
            }
            _ => {}
        }
        if let Some(calls) = message.tool_calls {
            for call in calls {
                let name = call.function.name.unwrap_or_default();
                let args = call.function.arguments.unwrap_or_default();
                let input: Value = if args.trim().is_empty() {
                    Value::Object(serde_json::Map::new())
                } else {
                    serde_json::from_str(&args)
                        .map_err(|e| Error::Model(format!("invalid tool arguments json: {e}")))?
                };
                content.push(ContentBlock::ToolUse {
                    id: call.id.unwrap_or_default(),
                    name,
                    input,
                    state: None,
                    content: None,
                });
            }
        }

        let usage = response.usage.map(|u| Usage {
            prompt_tokens: u.input_tokens,
            completion_tokens: u.output_tokens,
            total_tokens: u.total_tokens,
        });
        Ok(ChatResponse {
            content,
            usage,
            finish_reason,
        })
    }

    // ---------- 参数 / 工具 ----------

    /// 生成参数写入 `parameters`：请求级 `options` 优先，缺失回退模型默认。
    /// 裁剪口径：只落 temperature / max_tokens（top_p / top_k / stop /
    /// enable_thinking 等扩展参数未复刻）。
    pub fn apply_options(
        &self,
        params: &mut DashScopeParameters,
        options: &GenerateOptions,
        defaults: Option<&GenerateOptions>,
    ) {
        let pick = |opt: Option<f64>, def: Option<f64>| opt.or(def);
        params.temperature = pick(options.temperature, defaults.and_then(|d| d.temperature));
        let max_tokens = match (options.max_tokens, defaults.and_then(|d| d.max_tokens)) {
            (Some(v), _) => Some(v),
            (None, Some(v)) => Some(v),
            (None, None) => None,
        };
        params.max_tokens = max_tokens;
    }

    /// 工具定义写入 `parameters.tools`（OpenAI 兼容 function 形态）。
    pub fn apply_tools(&self, params: &mut DashScopeParameters, tools: &[ToolSchema]) {
        params.tools = tools
            .iter()
            .map(|t| DashScopeTool {
                kind: "function".into(),
                function: DashScopeToolFunction {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.parameters.clone(),
                },
            })
            .collect();
    }

    /// 工具选择写入 `parameters.tool_choice`（对齐 Java
    /// `DashScopeToolsHelper.convertToolChoice`）：
    /// Auto → `"auto"`；None → `"none"`；Required → DashScope 不支持，
    /// 降级 `"auto"`；Specific → `{"type":"function","function":{"name":...}}`。
    pub fn apply_tool_choice(
        &self,
        params: &mut DashScopeParameters,
        tool_choice: Option<&ToolChoice>,
    ) {
        let value = match tool_choice {
            None | Some(ToolChoice::Auto) => Some(json!("auto")),
            Some(ToolChoice::None) => Some(json!("none")),
            // DashScope 原生协议不支持强制调用，降级为 auto（对齐 Java 行为）
            Some(ToolChoice::Required) => Some(json!("auto")),
            Some(ToolChoice::Specific { tool_name }) => Some(json!({
                "type": "function",
                "function": { "name": tool_name }
            })),
        };
        params.tool_choice = value;
    }
}

impl Formatter for DashScopeChatFormatter {
    type RequestMessage = DashScopeMessage;
    type RequestParams = DashScopeParameters;
    type Response = DashScopeResponse;

    fn format(
        &self,
        msgs: &[Msg],
        _options: Option<&GenerateOptions>,
    ) -> Result<Vec<Self::RequestMessage>> {
        Ok(msgs.iter().map(|m| self.convert_message(m)).collect())
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
    use agentscope_core::message::{Source, ToolResultState};
    use serde_json::json;

    fn user_msg(text: &str) -> Msg {
        Msg::user("user", text)
    }

    /// user / assistant / system 消息 → 简单字符串 content。
    #[test]
    fn converts_plain_messages() {
        let f = DashScopeChatFormatter::new();
        let msgs = vec![
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
            user_msg("你好"),
        ];
        let out = f.format(&msgs, None).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].role, "assistant");
        assert_eq!(out[0].content.as_ref().unwrap(), &json!("答案"));
        assert_eq!(out[0].reasoning_content.as_deref(), Some("推理"));
        assert_eq!(out[1].role, "user");
        assert_eq!(out[1].content.as_ref().unwrap(), &json!("你好"));
    }

    /// 工具结果消息 → role="tool" + tool_call_id + name + 文本 content。
    #[test]
    fn converts_tool_result_message() {
        let f = DashScopeChatFormatter::new();
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
        let out = f.convert_message(&msg);
        assert_eq!(out.role, "tool");
        assert_eq!(out.tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(out.name.as_deref(), Some("add"));
        assert_eq!(out.content.as_ref().unwrap(), &json!("3"));
    }

    /// assistant 带工具调用 → tool_calls + content 空串（DashScope 要求
    /// content 字段必须存在）。
    #[test]
    fn converts_assistant_tool_calls_with_empty_content() {
        let f = DashScopeChatFormatter::new();
        let msg = Msg::assistant(
            "assistant",
            vec![ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "add".into(),
                input: json!({"a": 1}),
                state: None,
                content: None,
            }],
        );
        let out = f.convert_message(&msg);
        assert_eq!(out.role, "assistant");
        assert_eq!(out.content.as_ref().unwrap(), &json!(""));
        let calls = out.tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id.as_deref(), Some("call_1"));
        assert_eq!(calls[0].function.name.as_deref(), Some("add"));
        assert_eq!(
            calls[0].function.arguments.as_deref(),
            Some(r#"{"a":1}"#)
        );
    }

    /// 媒体块 → 文本占位（简单格式不发多模态数组）。
    #[test]
    fn converts_media_blocks_to_placeholders() {
        let f = DashScopeChatFormatter::new();
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
        let out = f.convert_message(&msg);
        let text = out.content.as_ref().unwrap().as_str().unwrap();
        assert!(text.starts_with("看图"));
        assert!(text.contains("[Media - unsupported MIME type:"));
    }

    /// 响应解析：reasoning → Thinking、content → Text、tool_calls → ToolUse，
    /// finish_reason choice 级优先回退 output 级，usage 按 input/output/total 映射。
    #[test]
    fn parses_response_blocks_usage_and_finish_reason() {
        let f = DashScopeChatFormatter::new();
        let response: DashScopeResponse = serde_json::from_str(
            r#"{
                "request_id": "rid",
                "output": {
                    "finish_reason": "stop",
                    "choices": [{
                        "finish_reason": "tool_calls",
                        "message": {
                            "role": "assistant",
                            "reasoning_content": "想一想",
                            "content": "",
                            "tool_calls": [{
                                "id": "call_1",
                                "type": "function",
                                "function": {"name": "add", "arguments": "{\"a\":1}"}
                            }]
                        }
                    }]
                },
                "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15}
            }"#,
        )
        .unwrap();
        let parsed = f.parse_response(response).unwrap();
        assert_eq!(parsed.finish_reason.as_deref(), Some("tool_calls"));
        // 空串 content 不产出 Text 块：Thinking + ToolUse 共 2 块
        assert_eq!(parsed.content.len(), 2);
        assert!(matches!(&parsed.content[0], ContentBlock::Thinking { thinking } if thinking == "想一想"));
        assert!(matches!(&parsed.content[1], ContentBlock::ToolUse { name, input, .. } if name == "add" && input == &json!({"a": 1})));
        let usage = parsed.usage.unwrap();
        assert_eq!(
            (usage.prompt_tokens, usage.completion_tokens, usage.total_tokens),
            (10, 5, 15)
        );
    }

    /// 纯文本响应：content → Text，finish_reason 回退 output 级。
    #[test]
    fn parses_plain_text_response() {
        let f = DashScopeChatFormatter::new();
        let response: DashScopeResponse = serde_json::from_str(
            r#"{"output":{"choices":[{"message":{"role":"assistant","content":"你好"}}],"finish_reason":"stop"},"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}"#,
        )
        .unwrap();
        let parsed = f.parse_response(response).unwrap();
        assert_eq!(parsed.finish_reason.as_deref(), Some("stop"));
        assert!(matches!(&parsed.content[0], ContentBlock::Text { text } if text == "你好"));
    }

    /// 无 choices 的响应报错（对齐 gemini 的 no candidates 语义）。
    #[test]
    fn errors_on_empty_choices() {
        let f = DashScopeChatFormatter::new();
        let response: DashScopeResponse =
            serde_json::from_str(r#"{"output":{"choices":[]}}"#).unwrap();
        assert!(f.parse_response(response).is_err());
    }

    /// 参数应用：请求级优先，缺失回退默认。
    #[test]
    fn applies_options_with_fallback() {
        let f = DashScopeChatFormatter::new();
        let mut params = DashScopeParameters::default();
        let defaults = GenerateOptions {
            temperature: Some(0.5),
            max_tokens: Some(128),
            ..Default::default()
        };
        let options = GenerateOptions {
            temperature: Some(0.9),
            ..Default::default()
        };
        f.apply_options(&mut params, &options, Some(&defaults));
        assert_eq!(params.temperature, Some(0.9));
        assert_eq!(params.max_tokens, Some(128));
    }

    /// 工具定义与 tool_choice 四变体（Required 降级 auto）。
    #[test]
    fn applies_tools_and_tool_choice() {
        let f = DashScopeChatFormatter::new();
        let mut params = DashScopeParameters::default();
        let tools = vec![ToolSchema {
            name: "add".into(),
            description: "加法".into(),
            parameters: json!({"type": "object"}),
        }];
        f.apply_tools(&mut params, &tools);
        assert_eq!(params.tools.len(), 1);
        assert_eq!(params.tools[0].kind, "function");
        assert_eq!(params.tools[0].function.name, "add");

        f.apply_tool_choice(&mut params, Some(&ToolChoice::Auto));
        assert_eq!(params.tool_choice.as_ref().unwrap(), &json!("auto"));
        f.apply_tool_choice(&mut params, Some(&ToolChoice::None));
        assert_eq!(params.tool_choice.as_ref().unwrap(), &json!("none"));
        f.apply_tool_choice(&mut params, Some(&ToolChoice::Required));
        assert_eq!(params.tool_choice.as_ref().unwrap(), &json!("auto"));
        f.apply_tool_choice(
            &mut params,
            Some(&ToolChoice::Specific {
                tool_name: "add".into(),
            }),
        );
        assert_eq!(
            params.tool_choice.as_ref().unwrap(),
            &json!({"type": "function", "function": {"name": "add"}})
        );
    }
}
