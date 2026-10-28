//! # agentscope-model-openai
//!
//! OpenAI 兼容的模型提供商实现（对应 Java 版 `agentscope-extensions-model-openai`）。
//!
//! 走标准 Chat Completions 协议（`POST {base_url}/chat/completions`），
//! 因此同时兼容所有 OpenAI 协议兼容服务（DashScope 兼容模式、DeepSeek、
//! Ollama 的 OpenAI 端点、vLLM 等）——只需替换 `base_url` 与 `model`。
//!
//! 第一阶段支持：system/user/assistant/tool 消息、多模态 image 输入、
//! function calling、temperature / max_tokens、SSE token 级流式输出。
//!
//! 流式行为对齐 Java 版 `OpenAIChatModel`：
//! - 请求带 `stream_options.include_usage`（DashScope 等兼容 API 依赖它返回用量）；
//! - 工具调用分片在实现方聚合完整后才产出 `ModelEvent::ToolUse`
//!   （Java 版把 `__fragment__` 分片抛给上层聚合，此处聚合位置不同、最终语义一致）；
//! - 流以 `ModelEvent::Done`（usage / finish_reason）收尾。

use std::collections::BTreeMap;

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
pub use formatter::OpenAIChatFormatter;

/// OpenAI 兼容聊天模型。
pub struct OpenAIChatModel {
    api_key: String,
    base_url: String,
    model: String,
    temperature: Option<f64>,
    max_tokens: Option<u32>,
    /// 上下文窗口（token；0 = 未知）——压缩预算按「窗口 − reserved」计算。
    context_window: usize,
    client: Client,
    /// 消息与参数的格式化器（对应 Java `OpenAIChatFormatter`）。
    formatter: OpenAIChatFormatter,
}

impl OpenAIChatModel {
    /// 创建模型实例。`base_url` 默认为 `https://api.openai.com/v1`。
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: "https://api.openai.com/v1".to_string(),
            model: model.into(),
            temperature: None,
            max_tokens: None,
            context_window: 0,
            client: Client::new(),
            formatter: OpenAIChatFormatter::new(),
        }
    }

    /// 替换服务地址（用于 DashScope 兼容模式、DeepSeek、Ollama、vLLM 等）。
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

    /// 组装请求体：消息、工具定义、生成参数全部经由 formatter 写入。
    ///
    /// 对应 Java `OpenAIChatModel` 的 prepareRequest：模型只负责传输，
    /// 格式转换由 formatter 完成，`defaults` 为模型实例上的默认选项。
    fn build_request(
        &self,
        request: &ChatRequest,
        stream: bool,
    ) -> Result<ChatCompletionRequest> {
        let defaults = GenerateOptions {
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            ..Default::default()
        };
        let mut body = ChatCompletionRequest {
            model: request
                .options
                .model
                .clone()
                .unwrap_or_else(|| self.model.clone()),
            messages: self.formatter.format(&request.messages, Some(&request.options))?,
            stream,
            stream_options: if stream {
                Some(StreamOptions {
                    include_usage: true,
                })
            } else {
                None
            },
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

// ---------- OpenAI 协议结构体 ----------

#[derive(Serialize, Default)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<OpenAIMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<OpenAITool>,
    /// 工具选择策略（`"auto"` / `"none"` / `"required"` / 指定函数对象）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    /// 结构化输出格式（对齐 Java `ResponseFormat`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<agentscope_core::formatter::ResponseFormat>,
    #[serde(skip_serializing_if = "is_false")]
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
}

/// 对齐 Java 版：流式请求一律带 `include_usage`，确保最后一个 chunk 携带用量
/// （DashScope、百炼等 OpenAI 兼容 API 依赖此字段）。
#[derive(Serialize)]
pub struct StreamOptions {
    include_usage: bool,
}

fn is_false(v: &bool) -> bool {
    !*v
}

#[derive(Serialize, Deserialize)]
pub struct OpenAIMessage {
    pub role: String,
    /// 纯文本时为字符串；多模态时为 content 数组（OpenAI 协议允许两种形态）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
    /// 消息名（对齐 Java 版：system/user/assistant 携带 sanitize 后的 name）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<OpenAIToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// assistant 历史回传的思考内容（对齐 Java `OpenAIMessageConverter`：
    /// 推理模型要求 reasoning 保留在请求历史里，DeepSeek thinking 模式
    /// 带工具调用时缺失会 HTTP 400）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct OpenAIToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: OpenAIFunction,
}

#[derive(Serialize, Deserialize)]
pub struct OpenAIFunction {
    pub name: String,
    /// OpenAI 协议中 arguments 是 JSON 字符串而非对象。
    pub arguments: String,
}

#[derive(Serialize)]
pub struct OpenAITool {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: OpenAIFunctionDef,
}

#[derive(Serialize)]
pub struct OpenAIFunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Deserialize)]
pub struct ChatCompletionResponse {
    pub choices: Vec<Choice>,
    pub usage: Option<OpenAIUsage>,
}

#[derive(Deserialize)]
pub struct Choice {
    pub message: ChoiceMessage,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Deserialize)]
pub struct ChoiceMessage {
    #[serde(default)]
    pub content: Option<String>,
    /// 思考 / 推理内容（非流式；`reasoning_content` 优先，回退 `reasoning`）。
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<OpenAIToolCall>>,
}

#[derive(Deserialize)]
pub struct OpenAIUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

// ---------- Msg <-> OpenAI 消息转换 ----------

#[async_trait]
impl Model for OpenAIChatModel {
    fn context_window_size(&self) -> usize {
        self.context_window
    }

    async fn call(&self, request: ChatRequest) -> Result<ChatResponse> {
        let body = self.build_request(&request, false)?;

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
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

        let completion: ChatCompletionResponse = serde_json::from_str(&text)
            .map_err(|e| Error::Model(format!("failed to parse response: {e}")))?;

        self.formatter.parse_response(completion)
    }

    async fn call_stream(
        &self,
        request: ChatRequest,
    ) -> Result<mpsc::Receiver<Result<ModelEvent>>> {
        let (tx, rx) = mpsc::channel::<Result<ModelEvent>>(64);

        let body = self.build_request(&request, true)?;

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
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

// ---------- 流式（SSE）解析 ----------

/// SSE `data:` 行的 JSON 载荷（流式 chunk）。
#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<OpenAIUsage>,
    /// OpenAI 兼容服务可能在流中返回错误对象（对齐 Java 版 chunk error 检查）。
    #[serde(default)]
    error: Option<StreamError>,
}

#[derive(Deserialize)]
struct StreamError {
    #[serde(default)]
    message: Option<String>,
}

#[derive(Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: StreamDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    /// 思考 / 推理增量（DeepSeek、SiliconFlow、DashScope 等用 `reasoning_content`，
    /// OpenRouter 及部分 vLLM 部署用 `reasoning`；两者都不出现即非推理模型）。
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<StreamToolCall>>,
}

#[derive(Deserialize)]
struct StreamToolCall {
    /// 同一工具调用的分片共享相同 index。
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<StreamToolFunction>,
}

#[derive(Deserialize)]
struct StreamToolFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

/// 聚合中的工具调用：流式协议把 name / arguments 分片发送，按 index 拼接。
#[derive(Default)]
struct PendingToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// 单个 chunk 解析后的产出。
struct ChunkOutcome {
    text_delta: Option<String>,
    thinking_delta: Option<String>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
}

/// 取思考增量：`reasoning_content` 优先，缺失时回退 `reasoning`。
fn delta_thinking(delta: &StreamDelta) -> Option<String> {
    delta
        .reasoning_content
        .clone()
        .or_else(|| delta.reasoning.clone())
        .filter(|t| !t.is_empty())
}

/// 把一个流式 chunk 应用到聚合状态（对齐 Java 版 `parseChunkResponse` 的语义，
/// 但工具调用分片在此聚合完整，而非以 `__fragment__` 占位抛给上层）。
fn apply_stream_chunk(
    chunk: StreamChunk,
    pending: &mut BTreeMap<u32, PendingToolCall>,
) -> ChunkOutcome {
    let mut outcome = ChunkOutcome {
        text_delta: None,
        thinking_delta: None,
        finish_reason: None,
        usage: None,
    };

    if let Some(u) = chunk.usage {
        outcome.usage = Some(Usage {
            prompt_tokens: u.prompt_tokens,
            completion_tokens: u.completion_tokens,
            total_tokens: u.total_tokens,
        });
    }

    for choice in chunk.choices {
        if choice.finish_reason.is_some() {
            outcome.finish_reason = choice.finish_reason;
        }
        // 先取思考增量（借用），再 move content，避免部分移动后借用
        let thinking = delta_thinking(&choice.delta);
        if let Some(text) = choice.delta.content {
            if !text.is_empty() {
                outcome.text_delta = Some(text);
            }
        }
        if let Some(thinking) = thinking {
            outcome.thinking_delta = Some(thinking);
        }
        if let Some(calls) = choice.delta.tool_calls {
            for call in calls {
                let entry = pending.entry(call.index).or_default();
                if let Some(id) = call.id {
                    entry.id = id;
                }
                if let Some(function) = call.function {
                    if let Some(name) = function.name {
                        entry.name = name;
                    }
                    if let Some(args) = function.arguments {
                        entry.arguments.push_str(&args);
                    }
                }
            }
        }
    }
    outcome
}

/// 把聚合完成的工具调用作为 `ModelEvent::ToolUse` 逐个发出。
async fn flush_pending_tools(
    pending: &mut BTreeMap<u32, PendingToolCall>,
    tx: &mpsc::Sender<Result<ModelEvent>>,
) -> Result<()> {
    for (_, call) in std::mem::take(pending) {
        let input: Value = if call.arguments.trim().is_empty() {
            Value::Object(serde_json::Map::new())
        } else {
            serde_json::from_str(&call.arguments)
                .map_err(|e| Error::Model(format!("invalid tool arguments json: {e}")))?
        };
        if tx
            .send(Ok(ModelEvent::ToolUse {
                id: call.id,
                name: call.name,
                input,
            }))
            .await
            .is_err()
        {
            break; // 接收方已关闭，停止发送
        }
    }
    Ok(())
}

/// 逐行解析 SSE 字节流，把模型事件推入 channel，直到 `[DONE]` 或流断开。
async fn pump_sse(
    resp: reqwest::Response,
    tx: mpsc::Sender<Result<ModelEvent>>,
) -> Result<()> {
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    let mut pending: BTreeMap<u32, PendingToolCall> = BTreeMap::new();
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
            if data == "[DONE]" {
                if let Err(e) = flush_pending_tools(&mut pending, &tx).await {
                    let _ = tx.send(Err(e)).await;
                    return Ok(());
                }
                let _ = tx
                    .send(Ok(ModelEvent::Done {
                        usage,
                        finish_reason,
                    }))
                    .await;
                return Ok(());
            }

            let parsed: StreamChunk = match serde_json::from_str(data) {
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

            let outcome = apply_stream_chunk(parsed, &mut pending);
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
            if outcome.finish_reason.is_some() {
                finish_reason = outcome.finish_reason;
            }
            if outcome.usage.is_some() {
                usage = outcome.usage;
            }
        }
    }

    // 服务端未发 [DONE] 直接断流：同样冲刷聚合状态并收尾
    if let Err(e) = flush_pending_tools(&mut pending, &tx).await {
        let _ = tx.send(Err(e)).await;
        return Ok(());
    }
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
    #[test]
    fn context_window_defaults_unknown_and_is_settable() {
        assert_eq!(OpenAIChatModel::new("k", "m").context_window_size(), 0);
        assert_eq!(
            OpenAIChatModel::new("k", "m")
                .with_context_window(128_000)
                .context_window_size(),
            128_000
        );
    }

    use super::*;

    /// 流式工具调用分片按 index 聚合：首片带 id/name，后续片只有 arguments 增量。
    #[test]
    fn stream_tool_call_aggregation() {
        let mut pending = BTreeMap::new();
        let first: StreamChunk = serde_json::from_str(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"add","arguments":""}}]}}]}"#,
        )
        .unwrap();
        let second: StreamChunk = serde_json::from_str(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"a\":"}}]}}]}"#,
        )
        .unwrap();
        let third: StreamChunk = serde_json::from_str(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]},"finish_reason":"tool_calls"}]}"#,
        )
        .unwrap();

        let o1 = apply_stream_chunk(first, &mut pending);
        assert!(o1.text_delta.is_none());
        apply_stream_chunk(second, &mut pending);
        let o3 = apply_stream_chunk(third, &mut pending);
        assert_eq!(o3.finish_reason.as_deref(), Some("tool_calls"));

        assert_eq!(pending.len(), 1);
        let call = &pending[&0];
        assert_eq!(call.id, "call_1");
        assert_eq!(call.name, "add");
        assert_eq!(call.arguments, r#"{"a":1}"#);
    }

    /// 文本增量 chunk + 最后一个 chunk 携带 usage（include_usage 生效时）。
    #[test]
    fn stream_text_and_usage_chunks() {
        let mut pending = BTreeMap::new();
        let text_chunk: StreamChunk =
            serde_json::from_str(r#"{"choices":[{"delta":{"content":"你"}}]}"#).unwrap();
        let outcome = apply_stream_chunk(text_chunk, &mut pending);
        assert_eq!(outcome.text_delta.as_deref(), Some("你"));
        assert!(outcome.usage.is_none());

        let usage_chunk: StreamChunk = serde_json::from_str(
            r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#,
        )
        .unwrap();
        let outcome = apply_stream_chunk(usage_chunk, &mut pending);
        let usage = outcome.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.total_tokens, 15);
    }

    /// 流中错误对象（对齐 Java 版 chunk error 检查）。
    #[test]
    fn stream_error_chunk_detected() {
        let chunk: StreamChunk =
            serde_json::from_str(r#"{"error":{"message":"rate limited"}}"#).unwrap();
        assert_eq!(chunk.error.unwrap().message.as_deref(), Some("rate limited"));
    }

    /// 思考增量 chunk：`reasoning_content`（DeepSeek/DashScope）与 `reasoning`
    /// （vLLM 部署，对齐 Java @JsonAlias）两种字段名都解析；空串不产出增量。
    #[test]
    fn stream_reasoning_chunks() {
        let mut pending = BTreeMap::new();
        let chunk: StreamChunk = serde_json::from_str(
            r#"{"choices":[{"delta":{"reasoning_content":"思考中"}}]}"#,
        )
        .unwrap();
        let o = apply_stream_chunk(chunk, &mut pending);
        assert_eq!(o.thinking_delta.as_deref(), Some("思考中"));
        assert!(o.text_delta.is_none());

        let chunk: StreamChunk = serde_json::from_str(
            r#"{"choices":[{"delta":{"reasoning":"alias thinking"}}]}"#,
        )
        .unwrap();
        let o = apply_stream_chunk(chunk, &mut pending);
        assert_eq!(o.thinking_delta.as_deref(), Some("alias thinking"));

        let chunk: StreamChunk = serde_json::from_str(
            r#"{"choices":[{"delta":{"reasoning_content":""}}]}"#,
        )
        .unwrap();
        let o = apply_stream_chunk(chunk, &mut pending);
        assert!(o.thinking_delta.is_none());
    }

    /// 流式请求体必须带 stream=true 与 stream_options.include_usage（对齐 Java 版）。
    #[test]
    fn stream_request_body_fields() {
        let body = ChatCompletionRequest {
            model: "gpt-4o".into(),
            stream: true,
            stream_options: Some(StreamOptions { include_usage: true }),
            ..Default::default()
        };
        let v = serde_json::to_value(&body).unwrap();
        assert_eq!(v["stream"], true);
        assert_eq!(v["stream_options"]["include_usage"], true);

        // 非流式请求不携带 stream_options，stream=false 时省略 stream 字段
        let body = ChatCompletionRequest {
            model: "gpt-4o".into(),
            ..Default::default()
        };
        let v = serde_json::to_value(&body).unwrap();
        assert!(v.get("stream").is_none());
        assert!(v.get("stream_options").is_none());
    }
}
