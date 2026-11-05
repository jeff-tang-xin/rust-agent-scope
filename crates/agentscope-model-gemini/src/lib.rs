//! # agentscope-model-gemini
//!
//! Google Gemini generateContent API 模型提供商（对应 Java 版
//! `agentscope-extensions-model-gemini`）。
//!
//! 走 REST 协议（Java 版用官方 GenAI SDK，Rust 无对应 SDK，传输层手写）：
//! - 非流式：`POST {base_url}/v1beta/models/{model}:generateContent`；
//! - 流式：`POST {base_url}/v1beta/models/{model}:streamGenerateContent?alt=sse`
//!   （SSE，每行 `data: {JSON}`，无 `[DONE]` 哨兵，断流即收尾）；
//! - 鉴权：请求头 `x-goog-api-key: <key>`。
//!
//! 第一阶段支持：system/user/assistant/tool 消息、多模态 inlineData 输入、
//! function calling、temperature / max_tokens、SSE token 级流式输出。
//!
//! 流式行为对齐 Java 版 `GeminiChatModel`：
//! - Gemini 的 `functionCall` part 整体到达（不像 OpenAI 按 index 分片），
//!   因此无需分片聚合，每个 functionCall 直接产出 `ModelEvent::ToolUse`；
//! - 思考内容以 part 的 `thought: true` 标记承载，产出 `ModelEvent::ThinkingDelta`；
//! - 流以 `ModelEvent::Done`（usage / finishReason）收尾。
//!
//! 与 Java 版的已知差异（裁剪口径，见 formatter 模块注释）：
//! - system 消息提取为顶层 `systemInstruction`（Java 版塞进 user 角色 contents）；
//! - http(s) 图片 URL 以 `fileData` 透传（Java 版同步下载后内联，Rust formatter
//!   保持纯同步不做网络下载）；
//! - ServerTool / code execution / MultiAgentFormatter / ConversationMerger 未复刻。

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

use agentscope_core::error::{Error, Result};
use agentscope_core::formatter::Formatter;
use agentscope_core::model::{ChatRequest, ChatResponse, GenerateOptions, Model, ModelEvent, Usage};

mod formatter;
pub use formatter::GeminiChatFormatter;

/// 默认服务地址（Gemini API，非 Vertex AI）。
const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";

/// Google Gemini 聊天模型。
pub struct GeminiChatModel {
    api_key: String,
    base_url: String,
    model: String,
    temperature: Option<f64>,
    max_tokens: Option<u32>,
    /// 上下文窗口（token；0 = 未知）——压缩预算按「窗口 − reserved」计算。
    context_window: usize,
    client: Client,
    /// 消息与参数的格式化器（对应 Java `GeminiChatFormatter`）。
    formatter: GeminiChatFormatter,
}

impl GeminiChatModel {
    /// 创建模型实例。`base_url` 默认为 `https://generativelanguage.googleapis.com`。
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
            model: model.into(),
            temperature: None,
            max_tokens: None,
            context_window: 0,
            client: Client::new(),
            formatter: GeminiChatFormatter,
        }
    }

    /// 替换服务地址（自建代理 / 兼容网关等）。
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// 设置上下文窗口（token；0 = 未知）——压缩触发预算的依据。
    pub fn with_context_window(mut self, tokens: usize) -> Self {
        self.context_window = tokens;
        self
    }

    /// 默认采样温度（请求级 options 可覆盖）。
    pub fn with_temperature(mut self, temperature: f64) -> Self {
        self.temperature = Some(temperature);
        self
    }

    /// 默认最大生成 token 数（请求级 options 可覆盖）。
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// 请求级 `options.model` 覆盖实例默认模型名（对齐 openai crate）。
    fn resolved_model(&self, request: &ChatRequest) -> String {
        request
            .options
            .model
            .clone()
            .unwrap_or_else(|| self.model.clone())
    }

    /// 端点 URL：非流式 `:generateContent`，流式 `:streamGenerateContent?alt=sse`。
    fn endpoint(&self, model: &str, stream: bool) -> String {
        let base = self.base_url.trim_end_matches('/');
        if stream {
            format!("{base}/v1beta/models/{model}:streamGenerateContent?alt=sse")
        } else {
            format!("{base}/v1beta/models/{model}:generateContent")
        }
    }

    /// 组装请求体：消息、工具定义、生成参数全部经由 formatter 写入。
    ///
    /// 对应 Java `GeminiChatModel` 的 buildGenerateContentConfig：模型只负责传输，
    /// 格式转换由 formatter 完成，`defaults` 为模型实例上的默认选项。
    /// 流式与非流式共用请求体（Gemini 以端点区分，无 stream 字段）。
    fn build_request(&self, request: &ChatRequest) -> Result<GenerateContentRequest> {
        let defaults = GenerateOptions {
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            ..Default::default()
        };
        let mut body = GenerateContentRequest {
            contents: self
                .formatter
                .format(&request.messages, Some(&request.options))?,
            system_instruction: self.formatter.system_instruction(&request.messages),
            ..Default::default()
        };
        self.formatter
            .apply_options(&mut body, &request.options, Some(&defaults));
        self.formatter.apply_tools(&mut body, &request.options.tools);
        self.formatter
            .apply_tool_choice(&mut body, request.options.tool_choice.as_ref());
        Ok(body)
    }
}

#[async_trait]
impl Model for GeminiChatModel {
    fn model_name(&self) -> &str {
        &self.model
    }

    fn context_window_size(&self) -> usize {
        self.context_window
    }

    async fn call(&self, request: ChatRequest) -> Result<ChatResponse> {
        let body = self.build_request(&request)?;
        let url = self.endpoint(&self.resolved_model(&request), false);

        let resp = self
            .client
            .post(&url)
            .header("x-goog-api-key", &self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Model(format!("request failed: {e}")))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| Error::Model(format!("failed to read response body: {e}")))?;

        if !status.is_success() {
            return Err(Error::Model(format!("http {status}: {text}")));
        }

        let completion: GenerateContentResponse = serde_json::from_str(&text)
            .map_err(|e| Error::Model(format!("failed to parse response: {e}")))?;

        self.formatter.parse_response(completion)
    }

    async fn call_stream(
        &self,
        request: ChatRequest,
    ) -> Result<mpsc::Receiver<Result<ModelEvent>>> {
        let (tx, rx) = mpsc::channel::<Result<ModelEvent>>(64);

        let body = self.build_request(&request)?;
        let url = self.endpoint(&self.resolved_model(&request), true);

        let resp = self
            .client
            .post(&url)
            .header("x-goog-api-key", &self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Model(format!("request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp
                .text()
                .await
                .map_err(|e| Error::Model(format!("failed to read response body: {e}")))?;
            return Err(Error::Model(format!("http {status}: {text}")));
        }

        // SSE 解析在独立任务中进行，通过 channel 把事件推给调用方
        tokio::spawn(async move {
            let _ = pump_sse(resp, tx).await;
        });
        Ok(rx)
    }
}

// ---------- Gemini generateContent 协议结构体 ----------

/// 请求体（字段名按 Gemini REST 协议 camelCase 序列化）。
#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentRequest {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub contents: Vec<GeminiContent>,
    /// system 消息独立顶层字段（Gemini 无 system 角色）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_instruction: Option<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation_config: Option<GeminiGenerationConfig>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<GeminiTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_config: Option<GeminiToolConfig>,
}

/// 一条对话内容：角色 + parts（对应 Java `Content`）。
#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct GeminiContent {
    /// `"user"` 或 `"model"`；`systemInstruction` 载体不带 role。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default)]
    pub parts: Vec<GeminiPart>,
}

/// 内容分片（对应 Java `Part`）：text / inlineData / fileData / functionCall /
/// functionResponse 五类，协议上互斥，解析时按字段独立识别。
#[derive(Serialize, Deserialize, Default, Debug, Clone)]
#[serde(default, rename_all = "camelCase")]
pub struct GeminiPart {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// 思考内容标记（`thought: true` 的 text part 是推理输出）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inline_data: Option<GeminiBlob>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_data: Option<GeminiFileData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_call: Option<GeminiFunctionCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_response: Option<GeminiFunctionResponse>,
}

/// 内联媒体（对应 Java `Blob`）：`data` 为 base64 字符串。
#[derive(Serialize, Deserialize, Default, Debug, Clone)]
#[serde(default, rename_all = "camelCase")]
pub struct GeminiBlob {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
}

/// 远程媒体引用（对应 Java `FileData`）。
#[derive(Serialize, Deserialize, Default, Debug, Clone)]
#[serde(default, rename_all = "camelCase")]
pub struct GeminiFileData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_uri: Option<String>,
}

/// 模型发起的函数调用（对应 Java `FunctionCall`）。
/// 注意 `args` 是 JSON 对象而非字符串（与 OpenAI 协议的关键差异）。
#[derive(Serialize, Deserialize, Default, Debug, Clone)]
#[serde(default, rename_all = "camelCase")]
pub struct GeminiFunctionCall {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
}

/// 回传给模型的函数执行结果（对应 Java `FunctionResponse`）。
#[derive(Serialize, Deserialize, Default, Debug, Clone)]
#[serde(default, rename_all = "camelCase")]
pub struct GeminiFunctionResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 固定形状 `{"output": "<文本>"}`（对齐 Java：结果文本放 output 键下）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<Value>,
}

/// 采样参数（裁剪口径：只落 temperature / maxOutputTokens）。
#[derive(Serialize, Deserialize, Default, Debug, Clone)]
#[serde(default, rename_all = "camelCase")]
pub struct GeminiGenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
}

/// 工具定义（对应 Java `Tool`）：单个 Tool 携带全部函数声明。
#[derive(Serialize, Deserialize, Default, Debug, Clone)]
#[serde(default, rename_all = "camelCase")]
pub struct GeminiTool {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub function_declarations: Vec<GeminiFunctionDeclaration>,
}

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
#[serde(default, rename_all = "camelCase")]
pub struct GeminiFunctionDeclaration {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,
}

/// 工具选择策略载体（对应 Java `ToolConfig`）。
#[derive(Serialize, Deserialize, Default, Debug, Clone)]
#[serde(default, rename_all = "camelCase")]
pub struct GeminiToolConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_calling_config: Option<GeminiFunctionCallingConfig>,
}

/// 函数调用模式（对应 Java `FunctionCallingConfig`）。
#[derive(Serialize, Deserialize, Default, Debug, Clone)]
#[serde(default, rename_all = "camelCase")]
pub struct GeminiFunctionCallingConfig {
    /// `"AUTO"` / `"ANY"` / `"NONE"`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// mode=ANY 且指定具体函数时下发。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_function_names: Option<Vec<String>>,
}

/// 响应体（流式 chunk 与非流式响应同构）。
#[derive(Deserialize, Default, Debug, Clone)]
#[serde(default)]
pub struct GenerateContentResponse {
    #[serde(default)]
    pub candidates: Vec<GeminiCandidate>,
    #[serde(default, rename = "usageMetadata")]
    pub usage_metadata: Option<GeminiUsageMetadata>,
    /// API 错误对象（流中也可能出现）。
    #[serde(default)]
    pub error: Option<GeminiError>,
}

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(default, rename_all = "camelCase")]
pub struct GeminiCandidate {
    #[serde(default)]
    pub content: Option<GeminiContent>,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// 用量元数据（对齐 Java `GenerateContentResponseUsageMetadata` 的取用字段）。
#[derive(Deserialize, Default, Debug, Clone)]
#[serde(default, rename_all = "camelCase")]
pub struct GeminiUsageMetadata {
    #[serde(default)]
    pub prompt_token_count: Option<u64>,
    #[serde(default)]
    pub candidates_token_count: Option<u64>,
    /// 思考 token（推理模型），计入 completion（对齐 Java outputTokens 口径）。
    #[serde(default)]
    pub thoughts_token_count: Option<u64>,
    #[serde(default)]
    pub total_token_count: Option<u64>,
}

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(default)]
pub struct GeminiError {
    #[serde(default)]
    pub code: Option<i64>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
}

// ---------- 流式（SSE）解析 ----------

/// 流式工具调用（Gemini 的 functionCall 整体到达，无需分片聚合）。
struct StreamToolUse {
    id: String,
    name: String,
    input: Value,
}

/// 单个 chunk 解析后的产出。
struct ChunkOutcome {
    thinking_delta: Option<String>,
    text_delta: Option<String>,
    tool_uses: Vec<StreamToolUse>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
}

/// 把一个流式 chunk 应用为事件产出（对齐 Java `parseResponse` 的 part 语义，
/// 文本/思考增量按 part 顺序拼接，functionCall 直接产出完整调用）。
fn apply_stream_chunk(chunk: GenerateContentResponse) -> ChunkOutcome {
    let mut outcome = ChunkOutcome {
        thinking_delta: None,
        text_delta: None,
        tool_uses: Vec::new(),
        finish_reason: None,
        usage: None,
    };

    if let Some(u) = chunk.usage_metadata {
        outcome.usage = Some(formatter::usage_from_metadata(&u));
    }

    for candidate in chunk.candidates {
        if candidate.finish_reason.is_some() {
            outcome.finish_reason = candidate.finish_reason;
        }
        let Some(content) = candidate.content else {
            continue;
        };
        for part in formatter::parse_parts(content.parts) {
            match part {
                formatter::ParsedPart::Thinking(text) => {
                    outcome
                        .thinking_delta
                        .get_or_insert_with(String::new)
                        .push_str(&text);
                }
                formatter::ParsedPart::Text(text) => {
                    outcome
                        .text_delta
                        .get_or_insert_with(String::new)
                        .push_str(&text);
                }
                formatter::ParsedPart::ToolUse { id, name, input } => {
                    outcome.tool_uses.push(StreamToolUse { id, name, input });
                }
            }
        }
    }
    outcome
}

/// 逐行解析 SSE 字节流，把模型事件推入 channel，直到流断开。
///
/// Gemini 流没有 `[DONE]` 哨兵：服务端发完最后一个 chunk（带 finishReason /
/// usageMetadata）后直接断流，此处以断流作为收尾信号。
async fn pump_sse(resp: reqwest::Response, tx: mpsc::Sender<Result<ModelEvent>>) -> Result<()> {
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    let mut finish_reason: Option<String> = None;
    let mut usage: Option<Usage> = None;

    loop {
        let chunk = match stream.next().await {
            Some(Ok(bytes)) => bytes,
            Some(Err(e)) => {
                let _ = tx
                    .send(Err(Error::Model(format!("stream read error: {e}"))))
                    .await;
                return Ok(());
            }
            None => break,
        };
        buf.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(pos) = buf.find('\n') {
            let line: String = buf.drain(..=pos).collect();
            let line = line.trim();
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data.is_empty() {
                continue;
            }
            // 兼容网关注入的 OpenAI 风格结束哨兵（Gemini 原生流不发送）
            if data == "[DONE]" {
                let _ = tx
                    .send(Ok(ModelEvent::Done {
                        usage,
                        finish_reason,
                    }))
                    .await;
                return Ok(());
            }

            let parsed: GenerateContentResponse = match serde_json::from_str(data) {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx
                        .send(Err(Error::Model(format!(
                            "failed to parse stream chunk: {e}"
                        ))))
                        .await;
                    return Ok(());
                }
            };
            if let Some(err) = parsed.error {
                let _ = tx
                    .send(Err(Error::Model(format!(
                        "stream error: {}",
                        err.message.unwrap_or_else(|| "unknown".into())
                    ))))
                    .await;
                return Ok(());
            }

            let outcome = apply_stream_chunk(parsed);
            if let Some(text) = outcome.thinking_delta {
                if tx
                    .send(Ok(ModelEvent::ThinkingDelta { text }))
                    .await
                    .is_err()
                {
                    return Ok(());
                }
            }
            if let Some(text) = outcome.text_delta {
                if tx.send(Ok(ModelEvent::TextDelta { text })).await.is_err() {
                    return Ok(());
                }
            }
            for call in outcome.tool_uses {
                if tx
                    .send(Ok(ModelEvent::ToolUse {
                        id: call.id,
                        name: call.name,
                        input: call.input,
                    }))
                    .await
                    .is_err()
                {
                    return Ok(());
                }
            }
            if outcome.finish_reason.is_some() {
                finish_reason = outcome.finish_reason;
            }
            if outcome.usage.is_some() {
                usage = outcome.usage;
            }
        }
    }

    // 服务端断流即收尾（Gemini 无 [DONE] 哨兵）
    let _ = tx
        .send(Ok(ModelEvent::Done {
            usage,
            finish_reason,
        }))
        .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn context_window_defaults_unknown_and_is_settable() {
        assert_eq!(GeminiChatModel::new("k", "m").context_window_size(), 0);
        assert_eq!(
            GeminiChatModel::new("k", "m")
                .with_context_window(1_000_000)
                .context_window_size(),
            1_000_000
        );
    }

    #[test]
    fn model_name_returns_model() {
        assert_eq!(
            GeminiChatModel::new("k", "gemini-2.5-flash").model_name(),
            "gemini-2.5-flash"
        );
    }

    /// 端点 URL：默认 base + v1beta 路径；流式带 alt=sse；自定义 base 去尾部斜杠。
    #[test]
    fn endpoint_urls() {
        let model = GeminiChatModel::new("k", "gemini-2.5-flash");
        assert_eq!(
            model.endpoint("gemini-2.5-flash", false),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent"
        );
        assert_eq!(
            model.endpoint("gemini-2.5-flash", true),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
        );
        let custom = GeminiChatModel::new("k", "m").with_base_url("http://localhost:8080/");
        assert_eq!(
            custom.endpoint("m", false),
            "http://localhost:8080/v1beta/models/m:generateContent"
        );
    }

    /// 请求体序列化：Gemini 协议字段全部 camelCase（systemInstruction /
    /// generationConfig.maxOutputTokens / functionDeclarations / functionCallingConfig）。
    #[test]
    fn request_body_serialization_camel_case() {
        let body = GenerateContentRequest {
            contents: vec![GeminiContent {
                role: Some("user".into()),
                parts: vec![GeminiPart {
                    text: Some("hi".into()),
                    ..Default::default()
                }],
            }],
            system_instruction: Some(GeminiContent {
                role: None,
                parts: vec![GeminiPart {
                    text: Some("sys".into()),
                    ..Default::default()
                }],
            }),
            generation_config: Some(GeminiGenerationConfig {
                temperature: Some(0.5),
                max_output_tokens: Some(100),
            }),
            tools: vec![GeminiTool {
                function_declarations: vec![GeminiFunctionDeclaration {
                    name: Some("add".into()),
                    description: Some("d".into()),
                    parameters: Some(json!({"type": "object"})),
                }],
            }],
            tool_config: Some(GeminiToolConfig {
                function_calling_config: Some(GeminiFunctionCallingConfig {
                    mode: Some("ANY".into()),
                    allowed_function_names: Some(vec!["add".into()]),
                }),
            }),
        };
        let v = serde_json::to_value(&body).unwrap();
        assert_eq!(v["contents"][0]["role"], "user");
        assert_eq!(v["contents"][0]["parts"][0]["text"], "hi");
        assert_eq!(v["systemInstruction"]["parts"][0]["text"], "sys");
        assert!(v["systemInstruction"].get("role").is_none());
        assert_eq!(v["generationConfig"]["temperature"], 0.5);
        assert_eq!(v["generationConfig"]["maxOutputTokens"], 100);
        assert_eq!(v["tools"][0]["functionDeclarations"][0]["name"], "add");
        assert_eq!(v["toolConfig"]["functionCallingConfig"]["mode"], "ANY");
        assert_eq!(
            v["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"][0],
            "add"
        );
    }

    /// 文本 chunk 与 functionCall chunk：Gemini 的调用整体到达，无需聚合。
    #[test]
    fn stream_text_then_function_call_chunks() {
        let text_chunk: GenerateContentResponse = serde_json::from_str(
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"你"}]}}]}"#,
        )
        .unwrap();
        let o = apply_stream_chunk(text_chunk);
        assert_eq!(o.text_delta.as_deref(), Some("你"));
        assert!(o.tool_uses.is_empty());
        assert!(o.finish_reason.is_none());
        assert!(o.usage.is_none());

        let call_chunk: GenerateContentResponse = serde_json::from_str(
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"add","args":{"a":1}}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"totalTokenCount":15}}"#,
        )
        .unwrap();
        let o = apply_stream_chunk(call_chunk);
        assert_eq!(o.tool_uses.len(), 1);
        assert_eq!(o.tool_uses[0].name, "add");
        assert_eq!(o.tool_uses[0].input, json!({"a": 1}));
        assert!(o.tool_uses[0].id.starts_with("tool_call_add_"));
        assert_eq!(o.finish_reason.as_deref(), Some("STOP"));
        let usage = o.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.total_tokens, 15);
    }

    /// 思考 chunk：`thought: true` 的 text part 产出 ThinkingDelta。
    #[test]
    fn stream_thought_chunk() {
        let chunk: GenerateContentResponse = serde_json::from_str(
            r#"{"candidates":[{"content":{"parts":[{"text":"推理中","thought":true},{"text":"答案"}]}}]}"#,
        )
        .unwrap();
        let o = apply_stream_chunk(chunk);
        assert_eq!(o.thinking_delta.as_deref(), Some("推理中"));
        assert_eq!(o.text_delta.as_deref(), Some("答案"));
    }

    /// 同一 chunk 内多个 text part 拼接为一个增量（保持顺序）。
    #[test]
    fn stream_multiple_text_parts_concatenated() {
        let chunk: GenerateContentResponse = serde_json::from_str(
            r#"{"candidates":[{"content":{"parts":[{"text":"a"},{"text":"b"}]}}]}"#,
        )
        .unwrap();
        let o = apply_stream_chunk(chunk);
        assert_eq!(o.text_delta.as_deref(), Some("ab"));
    }

    /// 流中错误对象（对齐 Java 版 chunk error 检查）。
    #[test]
    fn stream_error_chunk_detected() {
        let chunk: GenerateContentResponse = serde_json::from_str(
            r#"{"error":{"code":429,"message":"rate limited","status":"RESOURCE_EXHAUSTED"}}"#,
        )
        .unwrap();
        let err = chunk.error.unwrap();
        assert_eq!(err.message.as_deref(), Some("rate limited"));
        assert_eq!(err.code, Some(429));
    }

    /// 空 chunk（只有 usageMetadata / 无 candidates）不产出事件也不报错。
    #[test]
    fn stream_chunk_without_candidates_is_safe() {
        let chunk: GenerateContentResponse =
            serde_json::from_str(r#"{"usageMetadata":{"totalTokenCount":1}}"#).unwrap();
        let o = apply_stream_chunk(chunk);
        assert!(o.text_delta.is_none());
        assert_eq!(o.usage.unwrap().total_tokens, 1);
    }
}
