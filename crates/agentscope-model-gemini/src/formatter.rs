//! Gemini generateContent 协议的消息格式化实现。
//!
//! 对应 Java 版 `io.agentscope.extensions.model.gemini.formatter` 包：
//! - [`GeminiChatFormatter`] ↔ `GeminiChatFormatter`（format / applyOptions / applyTools / applyToolChoice）
//! - 模块内的 `*_part` 系列辅助 ↔ `GeminiMessageConverter`（Msg → contents 的逐角色转换）
//! - `parse_response` / `parse_parts` ↔ `GeminiResponseParser`（响应 → `ChatResponse`）
//!
//! 与 Java 版的已知差异（裁剪口径）：
//! - system 消息提取为顶层 `systemInstruction`（Java 版塞进 user 角色 contents）——
//!   Gemini 原生支持该字段，语义更直接；
//! - http(s) 媒体 URL 以 `fileData` 透传（Java 版同步下载后内联，此处保持纯同步
//!   不做网络下载）；
//! - 并行工具结果合并进同一 user Content（对齐 Java `appendFunctionResponses` 语义）；
//! - `functionResponse.response` 固定形状 `{"output": "<文本>"}`（对齐 Java）；
//! - ServerTool / code execution / MultiAgentFormatter / ConversationMerger 未复刻。

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};

use agentscope_core::error::{Error, Result};
use agentscope_core::formatter::{convert_tool_result_to_string, extract_text_content, media, Formatter};
use agentscope_core::message::{ContentBlock, Msg, Role, Source};
use agentscope_core::model::{ChatResponse, GenerateOptions, ToolChoice, ToolSchema, Usage};

use crate::{
    GenerateContentRequest, GenerateContentResponse, GeminiBlob, GeminiContent, GeminiFileData,
    GeminiFunctionCall, GeminiFunctionCallingConfig, GeminiFunctionDeclaration,
    GeminiFunctionResponse, GeminiGenerationConfig, GeminiPart, GeminiTool, GeminiToolConfig,
    GeminiUsageMetadata,
};

/// Gemini generateContent 格式化器（对应 Java `GeminiChatFormatter`）。
#[derive(Debug, Default, Clone, Copy)]
pub struct GeminiChatFormatter;

impl GeminiChatFormatter {
    pub fn new() -> Self {
        Self
    }

    /// 提取 system 消息为顶层 `systemInstruction`（Gemini 无 system 角色）。
    ///
    /// 多条 system 消息的文本以换行合并；载体 [`GeminiContent`] 不携带 `role`
    /// （协议约定 `systemInstruction` 无角色）。无 system 消息时返回 None。
    pub fn system_instruction(&self, msgs: &[Msg]) -> Option<GeminiContent> {
        let text = msgs
            .iter()
            .filter(|m| m.role == Role::System)
            .map(extract_text_content)
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        if text.is_empty() {
            return None;
        }
        Some(GeminiContent {
            role: None,
            parts: vec![text_part(&text)],
        })
    }
}

impl Formatter for GeminiChatFormatter {
    type RequestMessage = GeminiContent;
    type RequestParams = GenerateContentRequest;
    type Response = GenerateContentResponse;

    /// 消息 → `contents`。system 消息跳过（由 [`GeminiChatFormatter::system_instruction`]
    /// 提取为顶层字段）；工具结果块转为 functionResponse part 并合并进同一 user Content。
    fn format(
        &self,
        msgs: &[Msg],
        _options: Option<&GenerateOptions>,
    ) -> Result<Vec<Self::RequestMessage>> {
        let mut contents: Vec<GeminiContent> = Vec::new();
        for msg in msgs {
            if msg.role == Role::System {
                continue;
            }
            let mut function_responses: Vec<GeminiPart> = Vec::new();
            let mut other_parts: Vec<GeminiPart> = Vec::new();
            for block in &msg.content {
                match block {
                    ContentBlock::ToolResult { id, name, output, .. } => {
                        function_responses.push(function_response_part(id, name, output));
                    }
                    other => {
                        if let Some(part) = block_to_part(other) {
                            other_parts.push(part);
                        }
                    }
                }
            }
            // 并行工具结果合并进同一 user Content（对齐 Java appendFunctionResponses）：
            // 上一条 contents 已是携带 functionResponse 的 user Content 时直接追加。
            if !function_responses.is_empty() {
                match contents.last_mut() {
                    Some(last)
                        if last.role.as_deref() == Some("user")
                            && last.parts.iter().any(|p| p.function_response.is_some()) =>
                    {
                        last.parts.extend(function_responses);
                    }
                    _ => contents.push(GeminiContent {
                        role: Some("user".into()),
                        parts: function_responses,
                    }),
                }
            }
            if !other_parts.is_empty() {
                let role = match msg.role {
                    Role::Assistant => "model",
                    _ => "user",
                };
                contents.push(GeminiContent {
                    role: Some(role.into()),
                    parts: other_parts,
                });
            }
        }
        Ok(contents)
    }

    /// 响应 → [`ChatResponse`]：candidates[0] 的 parts 按序转为内容块
    /// （thought part → Thinking、text → Text、functionCall → ToolUse）。
    fn parse_response(&self, response: Self::Response) -> Result<ChatResponse> {
        if let Some(err) = response.error {
            return Err(Error::Model(format!(
                "gemini error {}: {}",
                err.code.unwrap_or(0),
                err.message.unwrap_or_else(|| "unknown".into())
            )));
        }
        let candidate = response
            .candidates
            .into_iter()
            .next()
            .ok_or_else(|| Error::Model("response contains no candidates".into()))?;
        let mut content = Vec::new();
        if let Some(c) = candidate.content {
            for part in parse_parts(c.parts) {
                match part {
                    ParsedPart::Thinking(thinking) => {
                        content.push(ContentBlock::Thinking { thinking });
                    }
                    ParsedPart::Text(text) => content.push(ContentBlock::Text { text }),
                    ParsedPart::ToolUse { id, name, input } => {
                        content.push(ContentBlock::ToolUse {
                            id,
                            name,
                            input,
                            state: None,
                            content: None,
                        });
                    }
                }
            }
        }
        Ok(ChatResponse {
            content,
            usage: response.usage_metadata.as_ref().map(usage_from_metadata),
            finish_reason: candidate.finish_reason,
        })
    }

    /// 生成参数写入 `generationConfig`：请求级 `options` 优先，缺失时回退模型默认。
    /// 裁剪口径：只落 temperature / maxOutputTokens。
    fn apply_options(
        &self,
        params: &mut Self::RequestParams,
        options: &GenerateOptions,
        defaults: Option<&GenerateOptions>,
    ) {
        let temperature = options
            .temperature
            .or_else(|| defaults.and_then(|d| d.temperature));
        let max_output_tokens = options
            .max_tokens
            .or_else(|| defaults.and_then(|d| d.max_tokens));
        if temperature.is_some() || max_output_tokens.is_some() {
            params.generation_config = Some(GeminiGenerationConfig {
                temperature,
                max_output_tokens,
            });
        }
    }

    /// 工具定义：Gemini 协议下单个 Tool 携带全部 functionDeclarations。
    fn apply_tools(&self, params: &mut Self::RequestParams, tools: &[ToolSchema]) {
        if tools.is_empty() {
            return;
        }
        params.tools = vec![GeminiTool {
            function_declarations: tools
                .iter()
                .map(|s| GeminiFunctionDeclaration {
                    name: Some(s.name.clone()),
                    description: Some(s.description.clone()),
                    parameters: Some(s.parameters.clone()),
                })
                .collect(),
        }];
    }

    /// 工具选择策略（对齐 Java `applyToolChoice`）：
    /// Auto / 未指定 → 不下发 toolConfig（Gemini 默认即 AUTO）；
    /// None → NONE；Required → ANY；Specific → ANY + allowedFunctionNames。
    fn apply_tool_choice(
        &self,
        params: &mut Self::RequestParams,
        tool_choice: Option<&ToolChoice>,
    ) {
        if params.tools.is_empty() {
            return;
        }
        let config = match tool_choice {
            None | Some(ToolChoice::Auto) => return,
            Some(ToolChoice::None) => GeminiFunctionCallingConfig {
                mode: Some("NONE".into()),
                allowed_function_names: None,
            },
            Some(ToolChoice::Required) => GeminiFunctionCallingConfig {
                mode: Some("ANY".into()),
                allowed_function_names: None,
            },
            Some(ToolChoice::Specific { tool_name }) => GeminiFunctionCallingConfig {
                mode: Some("ANY".into()),
                allowed_function_names: Some(vec![tool_name.clone()]),
            },
        };
        params.tool_config = Some(GeminiToolConfig {
            function_calling_config: Some(config),
        });
    }
}

// ---------- Msg → Gemini parts ----------

/// 文本 part。
fn text_part(text: &str) -> GeminiPart {
    GeminiPart {
        text: Some(text.to_string()),
        ..Default::default()
    }
}

/// 把一个非工具结果的内容块转为 part；不回传的块（Thinking 等）返回 None。
fn block_to_part(block: &ContentBlock) -> Option<GeminiPart> {
    match block {
        ContentBlock::Text { text } => Some(text_part(text)),
        ContentBlock::Hint { hint, .. } => Some(text_part(hint)),
        ContentBlock::Image { source, .. }
        | ContentBlock::Audio { source }
        | ContentBlock::Video { source, .. }
        | ContentBlock::Data { source, .. } => Some(media_part(source)),
        ContentBlock::ToolUse { id, name, input, .. } => {
            Some(function_call_part(id, name, input))
        }
        // Thinking 不回传（对齐 core extract_text_content 的约定）
        _ => None,
    }
}

/// 媒体块 → part：Base64 → `inlineData`；http(s) URL → `fileData` 透传
/// （不做网络下载，对齐裁剪口径）。
fn media_part(source: &Source) -> GeminiPart {
    match source {
        Source::Base64 { media_type, data } => {
            if data.starts_with("http://") || data.starts_with("https://") {
                GeminiPart {
                    file_data: Some(GeminiFileData {
                        mime_type: Some(media_type.clone()),
                        file_uri: Some(data.clone()),
                    }),
                    ..Default::default()
                }
            } else {
                GeminiPart {
                    inline_data: Some(GeminiBlob {
                        mime_type: Some(media_type.clone()),
                        data: Some(data.clone()),
                    }),
                    ..Default::default()
                }
            }
        }
        Source::Url { url, mime_type } => GeminiPart {
            file_data: Some(GeminiFileData {
                mime_type: mime_type.clone().or_else(|| infer_mime_type(url)),
                file_uri: Some(url.clone()),
            }),
            ..Default::default()
        },
    }
}

/// 按 URL 扩展名推断 MIME；推断不出（octet-stream）时返回 None
/// （`fileData` 的 mimeType 可省略，交由服务端按扩展名识别）。
fn infer_mime_type(url: &str) -> Option<String> {
    match media::determine_media_type(url).as_str() {
        "application/octet-stream" => None,
        mime => Some(mime.to_string()),
    }
}

/// 历史里的工具调用 → functionCall part（`args` 是 JSON 对象，非字符串）。
fn function_call_part(id: &str, name: &str, input: &Value) -> GeminiPart {
    GeminiPart {
        function_call: Some(GeminiFunctionCall {
            id: if id.is_empty() {
                None
            } else {
                Some(id.to_string())
            },
            name: Some(name.to_string()),
            args: Some(input.clone()),
        }),
        ..Default::default()
    }
}

/// 工具结果 → functionResponse part：结果文本固定放 `response.output` 键下
/// （对齐 Java）；name 为空时兜底 `unknown`（协议必填字段）。
fn function_response_part(id: &str, name: &str, output: &[ContentBlock]) -> GeminiPart {
    GeminiPart {
        function_response: Some(GeminiFunctionResponse {
            id: if id.is_empty() {
                None
            } else {
                Some(id.to_string())
            },
            name: Some(if name.is_empty() {
                "unknown".to_string()
            } else {
                name.to_string()
            }),
            response: Some(json!({ "output": convert_tool_result_to_string(output) })),
        }),
        ..Default::default()
    }
}

// ---------- 响应解析 ----------

/// 流式 / 非流式共用的 part 解析产出。
pub(crate) enum ParsedPart {
    /// `thought: true` 的 text part（推理输出）。
    Thinking(String),
    /// 普通文本。
    Text(String),
    /// functionCall part（Gemini 整体到达，无需分片聚合）。
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
}

/// functionCall 无 id 时生成的本地调用 id 序号（`tool_call_{name}_{序号}`）。
static TOOL_CALL_SEQ: AtomicU64 = AtomicU64::new(0);

fn generate_tool_call_id(name: &str) -> String {
    let seq = TOOL_CALL_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("tool_call_{name}_{seq}")
}

/// 把响应 parts 按序解析为 [`ParsedPart`]：functionCall 优先识别，
/// `thought: true` 的 text 归为思考，其余（inlineData / fileData / functionResponse）跳过。
pub(crate) fn parse_parts(parts: Vec<GeminiPart>) -> Vec<ParsedPart> {
    parts
        .into_iter()
        .filter_map(|part| {
            if let Some(call) = part.function_call {
                let name = call.name.unwrap_or_default();
                let id = match call.id {
                    Some(id) if !id.is_empty() => id,
                    _ => generate_tool_call_id(&name),
                };
                return Some(ParsedPart::ToolUse {
                    id,
                    name,
                    input: call.args.unwrap_or_else(|| json!({})),
                });
            }
            if let Some(text) = part.text {
                if part.thought == Some(true) {
                    return Some(ParsedPart::Thinking(text));
                }
                return Some(ParsedPart::Text(text));
            }
            None
        })
        .collect()
}

/// 用量换算（对齐 Java `GenerateContentResponseUsageMetadata` 的取用口径）：
/// completion = candidatesTokenCount + thoughtsTokenCount（思考 token 计入 completion）；
/// totalTokenCount 缺失时按 prompt + completion 兜底。
pub(crate) fn usage_from_metadata(metadata: &GeminiUsageMetadata) -> Usage {
    let prompt_tokens = metadata.prompt_token_count.unwrap_or(0);
    let completion_tokens = metadata.candidates_token_count.unwrap_or(0)
        + metadata.thoughts_token_count.unwrap_or(0);
    Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens: metadata
            .total_token_count
            .unwrap_or(prompt_tokens + completion_tokens),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::message::ToolResultState;

    fn formatter() -> GeminiChatFormatter {
        GeminiChatFormatter::new()
    }

    fn schema(name: &str) -> ToolSchema {
        ToolSchema {
            name: name.into(),
            description: "d".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    // ---------- 消息转换 ----------

    /// user 文本消息 → role=user + 单个 text part。
    #[test]
    fn user_msg_conversion() {
        let contents = formatter()
            .format(&[Msg::user("user", "你好")], None)
            .unwrap();
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].role.as_deref(), Some("user"));
        assert_eq!(contents[0].parts.len(), 1);
        assert_eq!(contents[0].parts[0].text.as_deref(), Some("你好"));
    }

    /// assistant 文本 + ToolUse → role=model + text part + functionCall part
    /// （args 为 JSON 对象非字符串，调用 id 原样保留）。
    #[test]
    fn assistant_with_function_call_conversion() {
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
        let contents = formatter().format(&[msg], None).unwrap();
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].role.as_deref(), Some("model"));
        assert_eq!(contents[0].parts[0].text.as_deref(), Some("我来算"));
        let call = contents[0].parts[1].function_call.as_ref().unwrap();
        assert_eq!(call.id.as_deref(), Some("call_1"));
        assert_eq!(call.name.as_deref(), Some("add"));
        assert_eq!(call.args, Some(json!({"a": 1, "b": 2})));
    }

    /// assistant 历史的 Thinking 块不回传（对齐 core 约定：思考内容存于记忆不回传）。
    #[test]
    fn assistant_thinking_not_sent_back() {
        let msg = Msg::assistant(
            "assistant",
            vec![
                ContentBlock::Thinking {
                    thinking: "推理".into(),
                },
                ContentBlock::Text {
                    text: "答案".into(),
                },
            ],
        );
        let contents = formatter().format(&[msg], None).unwrap();
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].parts.len(), 1);
        assert_eq!(contents[0].parts[0].text.as_deref(), Some("答案"));
    }

    /// tool 消息 → user Content + functionResponse part（response 固定 {"output": text}，
    /// 线格式 camelCase：functionResponse）。
    #[test]
    fn tool_result_conversion() {
        let msg = Msg::tool_result(
            "assistant",
            "call_1",
            "get_time",
            "12:00",
            ToolResultState::Success,
        );
        let contents = formatter().format(&[msg], None).unwrap();
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].role.as_deref(), Some("user"));
        let fr = contents[0].parts[0].function_response.as_ref().unwrap();
        assert_eq!(fr.id.as_deref(), Some("call_1"));
        assert_eq!(fr.name.as_deref(), Some("get_time"));
        assert_eq!(fr.response, Some(json!({"output": "12:00"})));
        let v = serde_json::to_value(&contents[0]).unwrap();
        assert_eq!(v["parts"][0]["functionResponse"]["name"], "get_time");
        assert_eq!(
            v["parts"][0]["functionResponse"]["response"]["output"],
            "12:00"
        );
    }

    /// 并行工具结果合并进同一 user Content（对齐 Java appendFunctionResponses）。
    #[test]
    fn parallel_tool_results_merged_into_one_user_content() {
        let a = Msg::tool_result("assistant", "call_1", "add", "2", ToolResultState::Success);
        let b = Msg::tool_result("assistant", "call_2", "mul", "3", ToolResultState::Success);
        let contents = formatter().format(&[a, b], None).unwrap();
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].role.as_deref(), Some("user"));
        assert_eq!(contents[0].parts.len(), 2);
        assert_eq!(
            contents[0].parts[0]
                .function_response
                .as_ref()
                .unwrap()
                .name
                .as_deref(),
            Some("add")
        );
        assert_eq!(
            contents[0].parts[1]
                .function_response
                .as_ref()
                .unwrap()
                .name
                .as_deref(),
            Some("mul")
        );
    }

    /// 中间隔了普通消息时不合并（只合并相邻的工具结果）。
    #[test]
    fn tool_results_not_merged_across_other_messages() {
        let a = Msg::tool_result("assistant", "call_1", "add", "2", ToolResultState::Success);
        let user = Msg::user("user", "继续");
        let b = Msg::tool_result("assistant", "call_2", "mul", "3", ToolResultState::Success);
        let contents = formatter().format(&[a, user, b], None).unwrap();
        assert_eq!(contents.len(), 3);
    }

    /// 多模态：Base64 → inlineData；http(s) URL → fileData 透传（MIME 按扩展名推断）。
    #[test]
    fn multimodal_inline_data_and_file_data() {
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
        let contents = formatter().format(&[msg], None).unwrap();
        let parts = &contents[0].parts;
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].text.as_deref(), Some("看图"));
        let inline = parts[1].inline_data.as_ref().unwrap();
        assert_eq!(inline.mime_type.as_deref(), Some("image/png"));
        assert_eq!(inline.data.as_deref(), Some("aGVsbG8="));
        let file = parts[2].file_data.as_ref().unwrap();
        assert_eq!(file.file_uri.as_deref(), Some("https://example.com/a.png"));
        assert_eq!(file.mime_type.as_deref(), Some("image/png"));
        // 线格式 camelCase：inlineData / fileData / mimeType / fileUri
        let v = serde_json::to_value(&contents[0]).unwrap();
        assert_eq!(v["parts"][1]["inlineData"]["mimeType"], "image/png");
        assert_eq!(
            v["parts"][2]["fileData"]["fileUri"],
            "https://example.com/a.png"
        );
    }

    // ---------- systemInstruction ----------

    /// system 消息不进 contents，提取为顶层 systemInstruction（载体不带 role）。
    #[test]
    fn system_extracted_to_system_instruction() {
        let msgs = vec![Msg::system("system", "你是助手"), Msg::user("user", "hi")];
        let f = formatter();
        let contents = f.format(&msgs, None).unwrap();
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].role.as_deref(), Some("user"));

        let si = f.system_instruction(&msgs).unwrap();
        assert!(si.role.is_none());
        assert_eq!(si.parts.len(), 1);
        assert_eq!(si.parts[0].text.as_deref(), Some("你是助手"));
    }

    /// 多条 system 消息文本换行合并；无 system 消息时返回 None。
    #[test]
    fn system_instruction_merges_and_skips_when_absent() {
        let f = formatter();
        let msgs = vec![
            Msg::system("system", "规则一"),
            Msg::system("system", "规则二"),
            Msg::user("user", "hi"),
        ];
        let si = f.system_instruction(&msgs).unwrap();
        assert_eq!(si.parts[0].text.as_deref(), Some("规则一\n规则二"));
        assert!(f.system_instruction(&[Msg::user("user", "hi")]).is_none());
    }

    // ---------- 响应解析 ----------

    /// 响应解析：thought part → Thinking、text → Text、functionCall → ToolUse；
    /// usage 的 thoughtsTokenCount 计入 completion（对齐 Java outputTokens 口径）。
    #[test]
    fn response_parsing_blocks_and_usage() {
        let raw = json!({
            "candidates": [{
                "content": { "role": "model", "parts": [
                    {"text": "推理中", "thought": true},
                    {"text": "答案是 3"},
                    {"functionCall": {"name": "add", "args": {"a": 1}}}
                ]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 5,
                "thoughtsTokenCount": 3,
                "totalTokenCount": 18
            }
        });
        let resp: GenerateContentResponse = serde_json::from_value(raw).unwrap();
        let out = formatter().parse_response(resp).unwrap();
        assert_eq!(out.content.len(), 3);
        assert!(matches!(&out.content[0], ContentBlock::Thinking { thinking } if thinking == "推理中"));
        assert!(matches!(&out.content[1], ContentBlock::Text { text } if text == "答案是 3"));
        match &out.content[2] {
            ContentBlock::ToolUse { id, name, input, .. } => {
                assert_eq!(name, "add");
                assert_eq!(*input, json!({"a": 1}));
                assert!(id.starts_with("tool_call_add_"), "无 id 时生成本地 id: {id}");
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
        let usage = out.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 8); // 5 + 3（thoughts 计入 completion）
        assert_eq!(usage.total_tokens, 18);
        assert_eq!(out.finish_reason.as_deref(), Some("STOP"));
    }

    /// functionCall 自带 id 时原样使用（兼容网关回传）；args 缺省兜底空对象。
    #[test]
    fn response_function_call_with_id() {
        let raw = json!({
            "candidates": [{
                "content": { "parts": [
                    {"functionCall": {"id": "call_9", "name": "add"}}
                ]}
            }]
        });
        let resp: GenerateContentResponse = serde_json::from_value(raw).unwrap();
        let out = formatter().parse_response(resp).unwrap();
        match &out.content[0] {
            ContentBlock::ToolUse { id, input, .. } => {
                assert_eq!(id, "call_9");
                assert_eq!(*input, json!({}));
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    /// 错误对象 / 空 candidates → 报错（不吞）。
    #[test]
    fn response_error_and_empty_candidates() {
        let f = formatter();
        let err_resp: GenerateContentResponse = serde_json::from_value(json!({
            "error": {"code": 400, "message": "bad request", "status": "INVALID_ARGUMENT"}
        }))
        .unwrap();
        assert!(f.parse_response(err_resp).is_err());

        let empty: GenerateContentResponse = serde_json::from_value(json!({})).unwrap();
        assert!(f.parse_response(empty).is_err());
    }

    // ---------- apply_options / apply_tools / apply_tool_choice ----------

    /// 请求级 options 优先，缺失时回退模型默认（对齐 Java getOptionOrDefault）；
    /// 两级都没有时不下发 generationConfig。
    #[test]
    fn apply_options_prefers_request_then_defaults() {
        let f = formatter();
        let defaults = GenerateOptions {
            temperature: Some(0.1),
            max_tokens: Some(512),
            ..Default::default()
        };
        let options = GenerateOptions {
            temperature: Some(0.9),
            ..Default::default()
        };
        let mut body = GenerateContentRequest::default();
        f.apply_options(&mut body, &options, Some(&defaults));
        let config = body.generation_config.unwrap();
        assert_eq!(config.temperature, Some(0.9));
        assert_eq!(config.max_output_tokens, Some(512));

        let mut body = GenerateContentRequest::default();
        f.apply_options(&mut body, &GenerateOptions::default(), None);
        assert!(body.generation_config.is_none());
    }

    /// 工具定义：单个 Tool 携带全部 functionDeclarations；空列表不下发。
    #[test]
    fn apply_tools_builds_function_declarations() {
        let f = formatter();
        let mut body = GenerateContentRequest::default();
        f.apply_tools(&mut body, &[schema("add"), schema("mul")]);
        assert_eq!(body.tools.len(), 1);
        let decls = &body.tools[0].function_declarations;
        assert_eq!(decls.len(), 2);
        assert_eq!(decls[0].name.as_deref(), Some("add"));
        assert_eq!(
            decls[0].parameters,
            Some(json!({"type": "object", "properties": {}}))
        );

        let mut empty = GenerateContentRequest::default();
        f.apply_tools(&mut empty, &[]);
        assert!(empty.tools.is_empty());
    }

    /// ToolChoice 映射（对齐 Java）：Auto / 未指定 → 不下发 toolConfig；
    /// None → NONE；Required → ANY；Specific → ANY + allowedFunctionNames；
    /// 无工具时一律不下发。
    #[test]
    fn apply_tool_choice_variants() {
        let f = formatter();
        let tools = [schema("add")];

        let mut body = GenerateContentRequest::default();
        f.apply_tools(&mut body, &tools);
        f.apply_tool_choice(&mut body, Some(&ToolChoice::Auto));
        assert!(body.tool_config.is_none());
        f.apply_tool_choice(&mut body, None);
        assert!(body.tool_config.is_none());

        f.apply_tool_choice(&mut body, Some(&ToolChoice::None));
        let config = body.tool_config.as_ref().unwrap();
        let config = config.function_calling_config.as_ref().unwrap();
        assert_eq!(config.mode.as_deref(), Some("NONE"));
        assert!(config.allowed_function_names.is_none());

        f.apply_tool_choice(&mut body, Some(&ToolChoice::Required));
        let config = body.tool_config.as_ref().unwrap();
        let config = config.function_calling_config.as_ref().unwrap();
        assert_eq!(config.mode.as_deref(), Some("ANY"));
        assert!(config.allowed_function_names.is_none());

        f.apply_tool_choice(
            &mut body,
            Some(&ToolChoice::Specific {
                tool_name: "add".into(),
            }),
        );
        let config = body.tool_config.as_ref().unwrap();
        let config = config.function_calling_config.as_ref().unwrap();
        assert_eq!(config.mode.as_deref(), Some("ANY"));
        assert_eq!(config.allowed_function_names, Some(vec!["add".to_string()]));

        // 线格式 camelCase：toolConfig.functionCallingConfig.allowedFunctionNames
        let v = serde_json::to_value(&body).unwrap();
        assert_eq!(v["toolConfig"]["functionCallingConfig"]["mode"], "ANY");
        assert_eq!(
            v["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"][0],
            "add"
        );

        // 无工具时即使指定 choice 也不下发
        let mut no_tools = GenerateContentRequest::default();
        f.apply_tool_choice(&mut no_tools, Some(&ToolChoice::Required));
        assert!(no_tools.tool_config.is_none());
    }
}
