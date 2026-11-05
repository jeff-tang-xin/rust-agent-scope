//! Ollama 协议的消息格式化实现。
//!
//! 对应 Java 版 `io.agentscope.extensions.model.ollama.formatter` 包：
//! - [`OllamaChatFormatter`] ↔ `OllamaChatFormatter`（format / applyOptions / applyTools / applyToolChoice）
//! - 模块内的 `convert_message` ↔ `OllamaMessageConverter` + `OllamaChatFormatter#processMessage`
//! - `response_to_chat_response` ↔ `OllamaResponseParser`（响应 → `ChatResponse`）
//!
//! 与 Java 一致的关键语义：
//! - 任意角色消息里的 ToolResult 块拆成独立 `role=tool` 消息（带 `tool_call_id` + `name`），
//!   且**先于**主消息加入结果列表（对齐 Java `processMessage` 的添加顺序）；
//! - 主消息 content = 文本块 join("\n")，thinking 直接拼接（无分隔符），
//!   images = 裸 base64 字符串数组（不带 data URI 前缀）；
//! - Hint / Audio / Video / Data 块跳过（对齐 Java unsupported-warn-skip）；
//! - 工具调用 arguments 是 JSON 对象整块到达、无调用 id，响应侧生成 UUID。

use serde_json::{json, Value};
use uuid::Uuid;

use agentscope_core::error::{Error, Result};
use agentscope_core::formatter::Formatter;
use agentscope_core::message::{ContentBlock, Msg, Source};
use agentscope_core::model::{ChatResponse, GenerateOptions, ToolChoice, ToolSchema, Usage};

use crate::{
    OllamaMessage, OllamaOptions, OllamaRequest, OllamaResponse, OllamaTool, OllamaToolCall,
    OllamaToolDef, OllamaToolFunction,
};

/// Ollama `/api/chat` 格式化器（对应 Java `OllamaChatFormatter`）。
#[derive(Debug, Default, Clone, Copy)]
pub struct OllamaChatFormatter;

impl OllamaChatFormatter {
    pub fn new() -> Self {
        Self
    }
}

impl Formatter for OllamaChatFormatter {
    type RequestMessage = OllamaMessage;
    type RequestParams = OllamaRequest;
    type Response = OllamaResponse;

    fn format(
        &self,
        msgs: &[Msg],
        _options: Option<&GenerateOptions>,
    ) -> Result<Vec<Self::RequestMessage>> {
        // 一条 Msg 可能拆出多条协议消息（ToolResult 独立成 tool 消息）
        Ok(msgs.iter().flat_map(convert_message).collect())
    }

    fn parse_response(&self, response: Self::Response) -> Result<ChatResponse> {
        response_to_chat_response(response)
    }

    /// 生成参数写入 `options`：请求级 `options` 优先，缺失时回退模型实例默认值。
    /// 裁剪口径：Ollama 只落 temperature / num_predict（max_tokens → num_predict）。
    fn apply_options(
        &self,
        params: &mut Self::RequestParams,
        options: &GenerateOptions,
        defaults: Option<&GenerateOptions>,
    ) {
        let temperature = options
            .temperature
            .or_else(|| defaults.and_then(|d| d.temperature));
        let num_predict = options
            .max_tokens
            .or_else(|| defaults.and_then(|d| d.max_tokens));
        if temperature.is_some() || num_predict.is_some() {
            params.options = Some(OllamaOptions {
                temperature,
                num_predict,
            });
        }
    }

    fn apply_tools(&self, params: &mut Self::RequestParams, tools: &[ToolSchema]) {
        if tools.is_empty() {
            return;
        }
        params.tools = tools_to_ollama(tools);
    }

    /// 工具选择策略（对齐 Java `OllamaToolsHelper.applyToolChoice`）：
    /// Auto → `"auto"`、None → `"none"`、Required → `"auto"`（Ollama 无原生 required，
    /// Java 显式回落）、Specific → `{"type":"function","function":{"name":...}}`。
    fn apply_tool_choice(
        &self,
        params: &mut Self::RequestParams,
        tool_choice: Option<&ToolChoice>,
    ) {
        params.tool_choice = tool_choice.map(|choice| match choice {
            ToolChoice::Auto | ToolChoice::Required => json!("auto"),
            ToolChoice::None => json!("none"),
            ToolChoice::Specific { tool_name } => {
                json!({ "type": "function", "function": { "name": tool_name } })
            }
        });
    }
}

// ---------- Msg → Ollama 消息 ----------

/// 把一条 [`Msg`] 转换为 Ollama 协议消息列表（对应 Java `OllamaChatFormatter#processMessage`）。
///
/// ToolResult 块拆成独立 `role=tool` 消息且**先于**主消息加入结果列表
/// （对齐 Java `processMessage` 的添加顺序）；主消息仅在携带
/// 文本 / thinking / images / 工具调用时才产出。
pub(crate) fn convert_message(msg: &Msg) -> Vec<OllamaMessage> {
    let mut result: Vec<OllamaMessage> = Vec::new();
    let mut text = String::new();
    let mut thinking = String::new();
    let mut images: Vec<String> = Vec::new();
    let mut tool_calls: Vec<OllamaToolCall> = Vec::new();

    for block in &msg.content {
        match block {
            ContentBlock::Text { text: t } => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(t);
            }
            // 思考内容直接拼接、无分隔符（对齐 Java thinkingMsg.append）
            ContentBlock::Thinking { thinking: t } => thinking.push_str(t),
            ContentBlock::ToolUse { name, input, .. } => {
                tool_calls.push(OllamaToolCall {
                    function: OllamaToolFunction {
                        name: name.clone(),
                        // Ollama arguments 是 JSON 对象；非对象输入兜底为空对象
                        arguments: Some(if input.is_object() {
                            input.clone()
                        } else {
                            json!({})
                        }),
                    },
                });
            }
            ContentBlock::ToolResult {
                id,
                name,
                output,
                ..
            } => {
                result.push(tool_result_message(id, name, output));
            }
            ContentBlock::Image { source, .. } => {
                // Ollama images 是裸 base64 数组；URL 来源无法在纯转换层内联，跳过
                // （对齐 Java OllamaMediaConverter 转换失败 warn-skip 的宽容行为）
                if let Source::Base64 { data, .. } = source {
                    images.push(data.clone());
                }
            }
            // Hint / Audio / Video / Data：Ollama /api/chat 不支持，跳过
            _ => {}
        }
    }

    let content = (!text.is_empty()).then_some(text);
    let thinking = (!thinking.is_empty()).then_some(thinking);
    let images = (!images.is_empty()).then_some(images);
    let tool_calls = (!tool_calls.is_empty()).then_some(tool_calls);

    // 对齐 Java：主消息仅在至少携带一种内容时才产出
    if content.is_some() || thinking.is_some() || images.is_some() || tool_calls.is_some() {
        result.push(OllamaMessage {
            role: msg.role.as_str().into(),
            content,
            thinking,
            images,
            tool_calls,
            tool_call_id: None,
            name: None,
        });
    }
    result
}

/// tool 结果消息：content = 结果文本块 join("\n")，携带 `tool_call_id` 与 `name`
/// （对齐 Java `processToolResultBlock`；结果里的媒体块不提升，跳过）。
fn tool_result_message(id: &str, name: &str, output: &[ContentBlock]) -> OllamaMessage {
    let mut content = String::new();
    for block in output {
        if let ContentBlock::Text { text } = block {
            if !content.is_empty() {
                content.push('\n');
            }
            content.push_str(text);
        }
    }
    OllamaMessage {
        role: "tool".into(),
        content: Some(content),
        tool_call_id: Some(id.into()),
        name: Some(name.into()),
        ..Default::default()
    }
}

/// 把 [`ToolSchema`] 列表转换为 Ollama `tools` 参数
/// （`{"type":"function","function":{name,description,parameters}}`，对齐 Java `OllamaTool`）。
pub(crate) fn tools_to_ollama(tools: &[ToolSchema]) -> Vec<OllamaTool> {
    tools
        .iter()
        .map(|s| OllamaTool {
            kind: "function".into(),
            function: OllamaToolDef {
                name: s.name.clone(),
                description: s.description.clone(),
                parameters: s.parameters.clone(),
            },
        })
        .collect()
}

// ---------- Ollama 响应 → ChatResponse ----------

/// 把 Ollama 响应转换回 [`ChatResponse`]（对应 Java `OllamaResponseParser`）。
///
/// 块顺序对齐 Java：Thinking → Text → ToolUse（Ollama 不返回调用 id，生成 UUID）。
/// usage 映射：prompt_eval_count → prompt_tokens、eval_count → completion_tokens
/// （total = 两者之和）；finish_reason 取 done_reason，缺失但 done=true 时回落 "stop"。
pub(crate) fn response_to_chat_response(response: OllamaResponse) -> Result<ChatResponse> {
    if let Some(err) = response.error {
        return Err(Error::Model(format!(
            "ollama error: {}",
            error_text(&err)
        )));
    }

    let mut content = Vec::new();
    if let Some(message) = &response.message {
        if let Some(thinking) = message.thinking.as_deref().filter(|t| !t.is_empty()) {
            content.push(ContentBlock::Thinking {
                thinking: thinking.into(),
            });
        }
        if let Some(text) = message.content.as_deref().filter(|t| !t.is_empty()) {
            content.push(ContentBlock::Text { text: text.into() });
        }
        for call in message.tool_calls.iter().flatten() {
            if call.function.name.is_empty() {
                // 对齐 Java ToolValidator.requireNonBlank 失败跳过
                continue;
            }
            content.push(ContentBlock::ToolUse {
                id: Uuid::new_v4().to_string(),
                name: call.function.name.clone(),
                input: call
                    .function
                    .arguments
                    .clone()
                    .unwrap_or_else(|| json!({})),
                state: None,
                content: None,
            });
        }
    }

    Ok(ChatResponse {
        content,
        usage: map_usage(response.prompt_eval_count, response.eval_count),
        finish_reason: response
            .done_reason
            .or_else(|| response.done.filter(|d| *d).map(|_| "stop".to_string())),
    })
}

/// usage 映射：prompt_eval_count → prompt_tokens、eval_count → completion_tokens，
/// total = 两者之和；两者都缺失时返回 None（Rust 侧 usage 是 Option，
/// 无计数时不产出空用量，与 Java 恒建 ChatUsage(0,0) 的差异点）。
pub(crate) fn map_usage(prompt_eval_count: Option<u64>, eval_count: Option<u64>) -> Option<Usage> {
    if prompt_eval_count.is_none() && eval_count.is_none() {
        return None;
    }
    let prompt_tokens = prompt_eval_count.unwrap_or(0);
    let completion_tokens = eval_count.unwrap_or(0);
    Some(Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens: prompt_tokens + completion_tokens,
    })
}

/// 流内 / 响应内 error 字段的文本提取（通常是字符串，兜底序列化整个值）。
pub(crate) fn error_text(err: &Value) -> String {
    err.as_str()
        .map(str::to_string)
        .unwrap_or_else(|| err.to_string())
}

// ---------- 流式（NDJSON 行）解析 ----------

/// 单个 NDJSON 行解析后的产出。
pub(crate) struct ChunkOutcome {
    pub text_delta: Option<String>,
    pub thinking_delta: Option<String>,
    /// 完整到达的工具调用（Ollama 不分片，arguments 是 JSON 对象）。
    pub tool_calls: Vec<(String, Value)>,
    pub done: bool,
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
}

/// 把一个 NDJSON 行 chunk 应用为流事件产出（对齐 Java `OllamaHttpClient.stream`
/// 行解析 + `OllamaResponseParser` 的字段映射语义）。
pub(crate) fn apply_stream_chunk(chunk: OllamaResponse) -> ChunkOutcome {
    let mut outcome = ChunkOutcome {
        text_delta: None,
        thinking_delta: None,
        tool_calls: Vec::new(),
        done: false,
        finish_reason: None,
        usage: None,
    };

    if let Some(message) = &chunk.message {
        // 空串不产出增量（done 行的 content 常为空）
        if let Some(text) = message.content.as_deref().filter(|t| !t.is_empty()) {
            outcome.text_delta = Some(text.to_string());
        }
        if let Some(thinking) = message.thinking.as_deref().filter(|t| !t.is_empty()) {
            outcome.thinking_delta = Some(thinking.to_string());
        }
        for call in message.tool_calls.iter().flatten() {
            if call.function.name.is_empty() {
                continue;
            }
            outcome.tool_calls.push((
                call.function.name.clone(),
                call.function
                    .arguments
                    .clone()
                    .unwrap_or_else(|| json!({})),
            ));
        }
    }

    outcome.usage = map_usage(chunk.prompt_eval_count, chunk.eval_count);
    if chunk.done.unwrap_or(false) {
        outcome.done = true;
        // done_reason 缺失但 done=true → "stop"（对齐 Java OllamaResponseParser 回落）
        outcome.finish_reason = Some(chunk.done_reason.unwrap_or_else(|| "stop".into()));
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::message::{Role, ToolResultState};
    use serde_json::json;

    #[test]
    fn user_and_system_msg_conversion() {
        let user = convert_message(&Msg::user("user", "你好"));
        assert_eq!(user.len(), 1);
        assert_eq!(user[0].role, "user");
        assert_eq!(user[0].content.as_deref(), Some("你好"));
        assert!(user[0].images.is_none());
        assert!(user[0].tool_calls.is_none());

        let system = convert_message(&Msg::system("system", "你是助手"));
        assert_eq!(system.len(), 1);
        assert_eq!(system[0].role, "system");
        assert_eq!(system[0].content.as_deref(), Some("你是助手"));
    }

    /// 多文本块 join("\n")（对齐 Java contentMsg 循环）。
    #[test]
    fn text_blocks_joined_with_newline() {
        let msg = Msg::new(
            Role::User,
            "user",
            vec![
                ContentBlock::Text { text: "第一段".into() },
                ContentBlock::Text { text: "第二段".into() },
            ],
        );
        let msgs = convert_message(&msg);
        assert_eq!(msgs[0].content.as_deref(), Some("第一段\n第二段"));
    }

    /// assistant 工具调用：arguments 是 JSON 对象、无 id / type 字段。
    #[test]
    fn assistant_tool_calls_use_object_arguments_without_id() {
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
        let msgs = convert_message(&msg);
        assert_eq!(msgs.len(), 1);
        let m = &msgs[0];
        assert_eq!(m.role, "assistant");
        let calls = m.tool_calls.as_ref().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "add");
        assert_eq!(calls[0].function.arguments, Some(json!({"a": 1})));

        let v = serde_json::to_value(m).unwrap();
        assert!(v["tool_calls"][0].get("id").is_none());
        assert!(v["tool_calls"][0].get("type").is_none());
        assert_eq!(v["tool_calls"][0]["function"]["arguments"]["a"], 1);
    }

    /// thinking 直接拼接、无分隔符（对齐 Java thinkingMsg.append）。
    #[test]
    fn assistant_thinking_concatenated_without_separator() {
        let msg = Msg::assistant(
            "assistant",
            vec![
                ContentBlock::Thinking { thinking: "先想".into() },
                ContentBlock::Thinking { thinking: "再想".into() },
                ContentBlock::Text { text: "答案".into() },
            ],
        );
        let msgs = convert_message(&msg);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].thinking.as_deref(), Some("先想再想"));
        assert_eq!(msgs[0].content.as_deref(), Some("答案"));
    }

    /// ToolResult 块拆成独立 role=tool 消息（带 tool_call_id + name），
    /// 且先于主消息加入结果列表（对齐 Java processMessage 添加顺序）。
    #[test]
    fn tool_result_blocks_split_into_tool_messages_before_main() {
        let msg = Msg::new(
            Role::User,
            "user",
            vec![
                ContentBlock::Text { text: "看结果".into() },
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
                    output: vec![
                        ContentBlock::Text { text: "stdout".into() },
                        ContentBlock::Text { text: "stderr".into() },
                    ],
                    state: ToolResultState::Success,
                    execution_details: None,
                },
            ],
        );
        let msgs = convert_message(&msg);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].role, "tool");
        assert_eq!(msgs[0].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(msgs[0].name.as_deref(), Some("get_time"));
        assert_eq!(msgs[0].content.as_deref(), Some("12:00"));
        assert_eq!(msgs[1].role, "tool");
        assert_eq!(msgs[1].tool_call_id.as_deref(), Some("call_2"));
        // 结果内多文本块同样 join("\n")
        assert_eq!(msgs[1].content.as_deref(), Some("stdout\nstderr"));
        assert_eq!(msgs[2].role, "user");
        assert_eq!(msgs[2].content.as_deref(), Some("看结果"));
        assert!(msgs[2].tool_call_id.is_none());
    }

    /// 仅含 ToolResult 块的消息：只产出 tool 消息，不产出空主消息。
    #[test]
    fn tool_result_only_message_produces_no_empty_main() {
        let msg = Msg::tool_result(
            "assistant",
            "call_1",
            "get_time",
            "3",
            ToolResultState::Success,
        );
        let msgs = convert_message(&msg);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "tool");
        assert_eq!(msgs[0].tool_call_id.as_deref(), Some("call_1"));
    }

    /// Image 块 → 裸 base64 字符串数组（无 data URI 前缀）。
    #[test]
    fn image_blocks_become_raw_base64_array() {
        let msg = Msg::new(
            Role::User,
            "user",
            vec![
                ContentBlock::Text { text: "看这两张图".into() },
                ContentBlock::Image {
                    source: Source::Base64 {
                        media_type: "image/png".into(),
                        data: "aGk=".into(),
                    },
                    min_pixels: None,
                    max_pixels: None,
                },
                ContentBlock::Image {
                    source: Source::Base64 {
                        media_type: "image/jpeg".into(),
                        data: "aGVsbG8=".into(),
                    },
                    min_pixels: None,
                    max_pixels: None,
                },
            ],
        );
        let msgs = convert_message(&msg);
        assert_eq!(msgs.len(), 1);
        assert_eq!(
            msgs[0].images.as_ref().unwrap(),
            &["aGk=".to_string(), "aGVsbG8=".to_string()]
        );
        assert_eq!(msgs[0].content.as_deref(), Some("看这两张图"));

        // 序列化后 images 是裸 base64 字符串数组
        let v = serde_json::to_value(&msgs[0]).unwrap();
        assert_eq!(v["images"][0], "aGk=");
        assert_eq!(v["images"][1], "aGVsbG8=");
    }

    /// Hint / Audio / Video / Data 块跳过；仅含不支持块的消息不产出任何协议消息。
    #[test]
    fn unsupported_blocks_are_skipped() {
        let msg = Msg::new(
            Role::User,
            "user",
            vec![
                ContentBlock::Hint {
                    id: "h1".into(),
                    hint: "提示".into(),
                    source: None,
                },
                ContentBlock::Audio {
                    source: Source::Url {
                        url: "https://example.com/a.mp3".into(),
                        mime_type: None,
                    },
                },
                ContentBlock::Video {
                    source: Source::Url {
                        url: "https://example.com/v.mp4".into(),
                        mime_type: None,
                    },
                    fps: None,
                    max_frames: None,
                    min_pixels: None,
                    max_pixels: None,
                    total_pixels: None,
                },
                ContentBlock::Data {
                    source: Source::Base64 {
                        media_type: "application/pdf".into(),
                        data: "JVBERi0=".into(),
                    },
                    name: None,
                },
            ],
        );
        assert!(convert_message(&msg).is_empty());

        // 混合：文本保留，不支持块跳过
        let msg = Msg::new(
            Role::User,
            "user",
            vec![
                ContentBlock::Hint {
                    id: "h1".into(),
                    hint: "提示".into(),
                    source: None,
                },
                ContentBlock::Text { text: "正文".into() },
            ],
        );
        let msgs = convert_message(&msg);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content.as_deref(), Some("正文"));
    }

    /// 工具定义：{"type":"function","function":{name,description,parameters}}。
    #[test]
    fn tools_to_ollama_shape() {
        let tools = tools_to_ollama(&[ToolSchema {
            name: "add".into(),
            description: "加法".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }]);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].kind, "function");
        assert_eq!(tools[0].function.name, "add");
        assert_eq!(tools[0].function.description, "加法");
        assert_eq!(tools[0].function.parameters["type"], "object");
    }

    /// 请求级 options 优先、缺失回退默认；max_tokens → num_predict；双缺省不下发 options。
    #[test]
    fn apply_options_maps_max_tokens_to_num_predict() {
        let f = OllamaChatFormatter::new();
        let defaults = GenerateOptions {
            temperature: Some(0.1),
            max_tokens: Some(512),
            ..Default::default()
        };
        let options = GenerateOptions {
            temperature: Some(0.9),
            ..Default::default()
        };
        let mut params = OllamaRequest::default();
        f.apply_options(&mut params, &options, Some(&defaults));
        let opts = params.options.unwrap();
        assert_eq!(opts.temperature, Some(0.9));
        assert_eq!(opts.num_predict, Some(512));

        let mut params = OllamaRequest::default();
        f.apply_options(&mut params, &GenerateOptions::default(), None);
        assert!(params.options.is_none());
    }

    /// 工具选择策略：Auto/Required → "auto"、None → "none"、Specific → 函数对象。
    #[test]
    fn apply_tool_choice_variants() {
        let f = OllamaChatFormatter::new();
        let mut params = OllamaRequest::default();
        f.apply_tool_choice(&mut params, Some(&ToolChoice::Auto));
        assert_eq!(params.tool_choice, Some(json!("auto")));
        f.apply_tool_choice(&mut params, Some(&ToolChoice::None));
        assert_eq!(params.tool_choice, Some(json!("none")));
        // Ollama 无原生 required，回落 auto（对齐 Java）
        f.apply_tool_choice(&mut params, Some(&ToolChoice::Required));
        assert_eq!(params.tool_choice, Some(json!("auto")));
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
        f.apply_tool_choice(&mut params, None);
        assert!(params.tool_choice.is_none());
    }

    /// 非流式响应解析：Thinking → Text → ToolUse 顺序、UUID 调用 id、usage 映射。
    #[test]
    fn response_parses_thinking_text_tool_use_in_order() {
        let raw = json!({
            "model": "qwen3",
            "message": {
                "role": "assistant",
                "content": "答案是 3",
                "thinking": "推理过程",
                "tool_calls": [
                    { "function": { "name": "add", "arguments": {"a": 1, "b": 2} } }
                ]
            },
            "done": true,
            "done_reason": "stop",
            "prompt_eval_count": 100,
            "eval_count": 20
        });
        let response: OllamaResponse = serde_json::from_value(raw).unwrap();
        let resp = response_to_chat_response(response).unwrap();
        assert_eq!(resp.content.len(), 3);
        assert!(matches!(&resp.content[0], ContentBlock::Thinking { thinking } if thinking == "推理过程"));
        assert!(matches!(&resp.content[1], ContentBlock::Text { text } if text == "答案是 3"));
        match &resp.content[2] {
            ContentBlock::ToolUse { id, name, input, .. } => {
                assert_eq!(name, "add");
                assert_eq!(input["a"], 1);
                // Ollama 不返回调用 id，生成合法 UUID
                assert!(uuid::Uuid::parse_str(id).is_ok());
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
        let usage = resp.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 20);
        assert_eq!(usage.total_tokens, 120);
        assert_eq!(resp.finish_reason.as_deref(), Some("stop"));
    }

    /// finish_reason：done_reason 缺失但 done=true → "stop"；done=false → 无。
    #[test]
    fn finish_reason_falls_back_to_stop_when_done() {
        let response: OllamaResponse = serde_json::from_str(
            r#"{"done":true,"message":{"role":"assistant","content":"hi"}}"#,
        )
        .unwrap();
        let resp = response_to_chat_response(response).unwrap();
        assert_eq!(resp.finish_reason.as_deref(), Some("stop"));

        let response: OllamaResponse = serde_json::from_str(
            r#"{"done":false,"message":{"role":"assistant","content":"hi"}}"#,
        )
        .unwrap();
        let resp = response_to_chat_response(response).unwrap();
        assert!(resp.finish_reason.is_none());
    }

    /// usage 映射：双缺省 → None；单缺省按 0 计；total = 两者之和。
    #[test]
    fn usage_mapping_and_missing_counts() {
        assert_eq!(map_usage(None, None), None);
        let usage = map_usage(Some(10), None).unwrap();
        assert_eq!(
            (usage.prompt_tokens, usage.completion_tokens, usage.total_tokens),
            (10, 0, 10)
        );
        let usage = map_usage(Some(7), Some(3)).unwrap();
        assert_eq!(usage.total_tokens, 10);
    }

    /// 响应内 error 字段 → Model 错误。
    #[test]
    fn response_error_field_becomes_model_error() {
        let response: OllamaResponse =
            serde_json::from_str(r#"{"error":"model not found"}"#).unwrap();
        let err = response_to_chat_response(response).unwrap_err();
        assert!(err.to_string().contains("model not found"));
    }

    /// 流式行：文本 + 思考增量；空 content 不产出增量。
    #[test]
    fn stream_chunk_text_and_thinking_deltas() {
        let chunk: OllamaResponse = serde_json::from_str(
            r#"{"model":"qwen3","message":{"role":"assistant","content":"你","thinking":"想"},"done":false}"#,
        )
        .unwrap();
        let o = apply_stream_chunk(chunk);
        assert_eq!(o.text_delta.as_deref(), Some("你"));
        assert_eq!(o.thinking_delta.as_deref(), Some("想"));
        assert!(o.tool_calls.is_empty());
        assert!(!o.done);
        assert!(o.usage.is_none());

        let chunk: OllamaResponse = serde_json::from_str(
            r#"{"message":{"role":"assistant","content":""},"done":false}"#,
        )
        .unwrap();
        let o = apply_stream_chunk(chunk);
        assert!(o.text_delta.is_none());
    }

    /// 流式收尾行：工具调用整块到达 + usage 映射 + done_reason。
    #[test]
    fn stream_final_chunk_tool_calls_usage_and_done() {
        let chunk: OllamaResponse = serde_json::from_str(
            r#"{"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"add","arguments":{"a":1}}}]},"done":true,"done_reason":"stop","prompt_eval_count":10,"eval_count":5}"#,
        )
        .unwrap();
        let o = apply_stream_chunk(chunk);
        assert!(o.text_delta.is_none());
        assert_eq!(o.tool_calls.len(), 1);
        assert_eq!(o.tool_calls[0].0, "add");
        assert_eq!(o.tool_calls[0].1["a"], 1);
        assert!(o.done);
        assert_eq!(o.finish_reason.as_deref(), Some("stop"));
        let usage = o.usage.unwrap();
        assert_eq!(
            (usage.prompt_tokens, usage.completion_tokens, usage.total_tokens),
            (10, 5, 15)
        );
    }

    /// 流式 done=true 但无 done_reason → finish_reason 回落 "stop"。
    #[test]
    fn stream_done_without_done_reason_falls_back_to_stop() {
        let chunk: OllamaResponse = serde_json::from_str(
            r#"{"message":{"role":"assistant","content":"完"},"done":true}"#,
        )
        .unwrap();
        let o = apply_stream_chunk(chunk);
        assert!(o.done);
        assert_eq!(o.finish_reason.as_deref(), Some("stop"));
    }
}
