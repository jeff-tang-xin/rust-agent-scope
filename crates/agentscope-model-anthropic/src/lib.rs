//! # agentscope-model-anthropic
//!
//! Anthropic Messages API 模型提供商（对应 Java 版 `agentscope-extensions-model-anthropic`）。
//!
//! 走 Messages 协议（`POST {base_url}/v1/messages`，`x-api-key` +
//! `anthropic-version: 2023-06-01` 头，SSE 流式）。
//!
//! 第一阶段支持：system/user/assistant/tool 消息、image 输入、function calling、
//! temperature / max_tokens、SSE 事件级流式输出。
//!
//! 流式行为对齐 Java 版 `AnthropicChatModel`：
//! - 事件流 message_start → content_block_start/delta/stop → message_delta → message_stop；
//! - text_delta / thinking_delta 即时透传为 [`ModelEvent::TextDelta`] / [`ModelEvent::ThinkingDelta`]；
//! - 工具调用分片（`input_json_delta` 按 index 拼 `partial_json`）在实现方聚合完整后，
//!   于 message_stop / 断流时统一产出 [`ModelEvent::ToolUse`]；
//! - usage 对齐 Java 不变量：prompt = input_tokens + cache_read + cache_creation，
//!   completion = output_tokens；stop_reason 原样透传；
//! - 流以 [`ModelEvent::Done`]（usage / finish_reason）收尾。

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
pub use formatter::{AnthropicChatFormatter, AnthropicFormattedRequest};

/// Anthropic API 版本头（对齐 Java 版 `anthropic-version: 2023-06-01`）。
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Anthropic Messages 聊天模型。
pub struct AnthropicChatModel {
    api_key: String,
    base_url: String,
    model: String,
    temperature: Option<f64>,
    max_tokens: Option<u32>,
    /// 上下文窗口（token；0 = 未知）——压缩预算按「窗口 − reserved」计算。
    context_window: usize,
    client: Client,
    /// 消息与参数的格式化器（对应 Java `AnthropicChatFormatter`）。
    formatter: AnthropicChatFormatter,
}

impl AnthropicChatModel {
    /// 创建模型实例。`base_url` 默认为 `https://api.anthropic.com`。
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: "https://api.anthropic.com".to_string(),
            model: model.into(),
            temperature: None,
            max_tokens: None,
            context_window: 0,
            client: Client::new(),
            formatter: AnthropicChatFormatter::new(),
        }
    }

    /// 替换服务地址（用于网关 / 代理等 Anthropic 协议兼容服务）。
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

    /// 默认最大生成 token 数（请求级 options 可覆盖；缺省 4096 兜底）。
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// 组装请求体：消息、工具定义、生成参数全部经由 formatter 写入。
    ///
    /// 对应 Java `AnthropicChatModel` 的 prepareRequest：模型只负责传输，
    /// 格式转换由 formatter 完成，`defaults` 为模型实例上的默认选项。
    fn build_request(&self, request: &ChatRequest, stream: bool) -> Result<AnthropicRequest> {
        let defaults = GenerateOptions {
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            ..Default::default()
        };
        let formatted = self.formatter.format_request(&request.messages);
        let mut body = AnthropicRequest {
            model: request
                .options
                .model
                .clone()
                .unwrap_or_else(|| self.model.clone()),
            system: formatted.system,
            messages: formatted.messages,
            max_tokens: formatter::DEFAULT_MAX_TOKENS,
            temperature: None,
            tools: Vec::new(),
            tool_choice: None,
            stream,
        };
        self.formatter
            .apply_options(&mut body, &request.options, Some(&defaults));
        self.formatter.apply_tools(&mut body, &request.options.tools);
        self.formatter
            .apply_tool_choice(&mut body, request.options.tool_choice.as_ref());
        Ok(body)
    }

    fn url(&self) -> String {
        format!("{}/v1/messages", self.base_url.trim_end_matches('/'))
    }
}

// ---------- Anthropic 协议结构体 ----------

/// Messages API 请求体（`POST {base_url}/v1/messages`）。
#[derive(Serialize, Default)]
pub struct AnthropicRequest {
    pub model: String,
    /// 顶层系统提示词（Anthropic 协议不走 messages 列表）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// Anthropic 必填字段（缺省 4096 兜底，对齐 Java builder）。
    pub max_tokens: u32,
    pub messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<AnthropicTool>,
    /// 工具选择策略（`{"type":"auto"|"none"|"any"}` 或 `{"type":"tool","name":...}`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    #[serde(skip_serializing_if = "is_false")]
    pub stream: bool,
}

fn is_false(v: &bool) -> bool {
    !*v
}

/// Anthropic 消息：role 仅 user/assistant，content 为块数组。
#[derive(Serialize, Debug, Clone)]
pub struct AnthropicMessage {
    pub role: String,
    pub content: Vec<Value>,
}

/// 工具定义（Anthropic 用 `input_schema` 承载 JSON Schema）。
#[derive(Serialize, Debug, Clone)]
pub struct AnthropicTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Messages API 响应体（content 为块数组，按 `type` 字段分派）。
#[derive(Deserialize)]
pub struct AnthropicResponse {
    #[serde(default)]
    pub content: Vec<Value>,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub usage: Option<AnthropicUsage>,
}

/// 用量（cache 字段缺失时按 0 计）。
#[derive(Deserialize, Default)]
pub struct AnthropicUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

// ---------- Model 实现 ----------

#[async_trait]
impl Model for AnthropicChatModel {
    fn context_window_size(&self) -> usize {
        self.context_window
    }

    async fn call(&self, request: ChatRequest) -> Result<ChatResponse> {
        let body = self.build_request(&request, false)?;

        let resp = self
            .client
            .post(self.url())
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
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

        let response: AnthropicResponse = serde_json::from_str(&text)
            .map_err(|e| Error::Model(format!("failed to parse response: {e}")))?;

        self.formatter.parse_response(response)
    }

    async fn call_stream(
        &self,
        request: ChatRequest,
    ) -> Result<mpsc::Receiver<Result<ModelEvent>>> {
        let (tx, rx) = mpsc::channel::<Result<ModelEvent>>(64);

        let body = self.build_request(&request, true)?;

        let resp = self
            .client
            .post(self.url())
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
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

/// SSE `data:` 行的事件载荷（Anthropic 事件流按 `type` 字段分派）。
#[derive(Deserialize)]
struct StreamEvent {
    #[serde(rename = "type")]
    kind: String,
    /// content_block_start / content_block_delta 携带的块序号。
    #[serde(default)]
    index: Option<u32>,
    /// content_block_start 携带的完整块（tool_use 块带 id/name）。
    #[serde(default)]
    content_block: Option<Value>,
    /// content_block_delta / message_delta 携带的增量。
    #[serde(default)]
    delta: Option<Value>,
    /// message_start 携带的 message 头（含初始 usage）。
    #[serde(default)]
    message: Option<Value>,
    /// message_delta 携带的累计用量。
    #[serde(default)]
    usage: Option<StreamUsage>,
    /// error 事件携带的错误体。
    #[serde(default)]
    error: Option<StreamError>,
}

#[derive(Deserialize, Default)]
struct StreamUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

#[derive(Deserialize)]
struct StreamError {
    #[serde(default)]
    message: Option<String>,
}

/// 聚合中的工具调用：参数经 `input_json_delta` 按 index 分片到达，拼 `partial_json`。
#[derive(Default)]
struct PendingToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// 单个事件解析后的产出。
struct EventOutcome {
    text_delta: Option<String>,
    thinking_delta: Option<String>,
    /// message_stop 事件：调用方需冲刷聚合状态并发出 Done。
    finished: bool,
}

/// 流式聚合状态：token 计数、结束原因与按 index 聚合中的工具调用。
#[derive(Default)]
struct StreamAccumulator {
    input_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    output_tokens: u64,
    has_usage: bool,
    finish_reason: Option<String>,
    pending: BTreeMap<u32, PendingToolCall>,
}

impl StreamAccumulator {
    /// 汇总用量（对齐 Java 不变量：prompt = input + cache_read + cache_creation）。
    fn usage(&self) -> Option<Usage> {
        if !self.has_usage {
            return None;
        }
        let prompt_tokens =
            self.input_tokens + self.cache_read_tokens + self.cache_creation_tokens;
        Some(Usage {
            prompt_tokens,
            completion_tokens: self.output_tokens,
            total_tokens: prompt_tokens + self.output_tokens,
        })
    }
}

/// 把一个流式事件应用到聚合状态（对齐 Java 版事件流语义；
/// 工具调用分片在此聚合完整，message_stop / 断流时统一 flush）。
fn apply_stream_event(event: StreamEvent, acc: &mut StreamAccumulator) -> EventOutcome {
    let mut outcome = EventOutcome {
        text_delta: None,
        thinking_delta: None,
        finished: false,
    };
    match event.kind.as_str() {
        "message_start" => {
            if let Some(usage) = event
                .message
                .as_ref()
                .and_then(|m| m.get("usage"))
                .and_then(|u| serde_json::from_value::<StreamUsage>(u.clone()).ok())
            {
                acc.has_usage = true;
                acc.input_tokens = usage.input_tokens;
                acc.cache_read_tokens = usage.cache_read_input_tokens;
                acc.cache_creation_tokens = usage.cache_creation_input_tokens;
                if usage.output_tokens > 0 {
                    acc.output_tokens = usage.output_tokens;
                }
            }
        }
        "content_block_start" => {
            // tool_use 块登记 id/name，参数经 input_json_delta 增量到达
            if let (Some(index), Some(block)) = (event.index, event.content_block) {
                if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                    let entry = acc.pending.entry(index).or_default();
                    if let Some(id) = block.get("id").and_then(Value::as_str) {
                        entry.id = id.to_string();
                    }
                    if let Some(name) = block.get("name").and_then(Value::as_str) {
                        entry.name = name.to_string();
                    }
                }
            }
        }
        "content_block_delta" => {
            if let Some(delta) = event.delta {
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                outcome.text_delta = Some(text.to_string());
                            }
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                            if !text.is_empty() {
                                outcome.thinking_delta = Some(text.to_string());
                            }
                        }
                    }
                    Some("input_json_delta") => {
                        if let (Some(index), Some(partial)) = (
                            event.index,
                            delta.get("partial_json").and_then(Value::as_str),
                        ) {
                            acc.pending
                                .entry(index)
                                .or_default()
                                .arguments
                                .push_str(partial);
                        }
                    }
                    _ => {}
                }
            }
        }
        "message_delta" => {
            if let Some(reason) = event
                .delta
                .as_ref()
                .and_then(|d| d.get("stop_reason"))
                .and_then(Value::as_str)
            {
                acc.finish_reason = Some(reason.to_string());
            }
            if let Some(usage) = event.usage {
                acc.has_usage = true;
                if usage.output_tokens > 0 {
                    acc.output_tokens = usage.output_tokens;
                }
            }
        }
        "message_stop" => {
            outcome.finished = true;
        }
        // ping / content_block_stop / 其他事件无需处理
        _ => {}
    }
    outcome
}

/// 把聚合完成的工具调用作为 [`ModelEvent::ToolUse`] 逐个发出。
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

/// 冲刷聚合完成的工具调用并发出 Done 事件（message_stop / 断流共用收尾路径）。
async fn finish_stream(
    acc: &mut StreamAccumulator,
    tx: &mpsc::Sender<Result<ModelEvent>>,
) -> Result<()> {
    if let Err(e) = flush_pending_tools(&mut acc.pending, tx).await {
        let _ = tx.send(Err(e)).await;
        return Ok(());
    }
    let _ = tx
        .send(Ok(ModelEvent::Done {
            usage: acc.usage(),
            finish_reason: acc.finish_reason.clone(),
        }))
        .await;
    Ok(())
}

/// 逐行解析 SSE 字节流，把模型事件推入 channel，直到 message_stop 或流断开。
async fn pump_sse(resp: reqwest::Response, tx: mpsc::Sender<Result<ModelEvent>>) -> Result<()> {
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    let mut acc = StreamAccumulator::default();

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
            let event: StreamEvent = match serde_json::from_str(data) {
                Ok(e) => e,
                Err(e) => {
                    let _ = tx
                        .send(Err(Error::Model(format!(
                            "failed to parse stream event: {e}"
                        ))))
                        .await;
                    return Ok(());
                }
            };
            if event.kind == "error" {
                let _ = tx
                    .send(Err(Error::Model(format!(
                        "stream error: {}",
                        event
                            .error
                            .and_then(|e| e.message)
                            .unwrap_or_else(|| "unknown".into())
                    ))))
                    .await;
                return Ok(());
            }

            let outcome = apply_stream_event(event, &mut acc);
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
            if outcome.finished {
                return finish_stream(&mut acc, &tx).await;
            }
        }
    }

    // 服务端未发 message_stop 直接断流：同样冲刷聚合状态并收尾
    finish_stream(&mut acc, &tx).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::message::{ContentBlock, Msg};
    use serde_json::json;

    #[test]
    fn context_window_defaults_unknown_and_is_settable() {
        assert_eq!(AnthropicChatModel::new("k", "m").context_window_size(), 0);
        assert_eq!(
            AnthropicChatModel::new("k", "m")
                .with_context_window(200_000)
                .context_window_size(),
            200_000
        );
    }

    /// 请求体形状：system 进顶层字段、stream 字段按需序列化、max_tokens 必填。
    #[test]
    fn build_request_shape() {
        let model = AnthropicChatModel::new("k", "claude-sonnet-4-5").with_max_tokens(1024);
        let request = ChatRequest {
            messages: vec![Msg::system("system", "你是助手"), Msg::user("user", "你好")],
            options: GenerateOptions::default(),
        };
        let body = model.build_request(&request, true).unwrap();
        assert_eq!(body.model, "claude-sonnet-4-5");
        assert_eq!(body.system.as_deref(), Some("你是助手"));
        assert_eq!(body.messages.len(), 1);
        assert!(body.stream);

        let v = serde_json::to_value(&body).unwrap();
        assert_eq!(v["max_tokens"], 1024);
        assert_eq!(v["stream"], true);
        assert_eq!(v["system"], "你是助手");
        assert_eq!(v["messages"][0]["role"], "user");

        // 非流式请求省略 stream 字段
        let body = model.build_request(&request, false).unwrap();
        let v = serde_json::to_value(&body).unwrap();
        assert!(v.get("stream").is_none());
    }

    /// 流式工具调用聚合：content_block_start 登记 id/name，
    /// input_json_delta 按 index 拼 partial_json，message_stop 触发收尾。
    #[test]
    fn stream_tool_call_aggregation() {
        let mut acc = StreamAccumulator::default();
        let start: StreamEvent = serde_json::from_str(
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"add","input":{}}}"#,
        )
        .unwrap();
        let delta1: StreamEvent = serde_json::from_str(
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"a\":"}}"#,
        )
        .unwrap();
        let delta2: StreamEvent = serde_json::from_str(
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"1}"}}"#,
        )
        .unwrap();
        let stop: StreamEvent =
            serde_json::from_str(r#"{"type":"message_stop"}"#).unwrap();

        for event in [start, delta1, delta2] {
            let outcome = apply_stream_event(event, &mut acc);
            assert!(outcome.text_delta.is_none());
            assert!(!outcome.finished);
        }
        let outcome = apply_stream_event(stop, &mut acc);
        assert!(outcome.finished);

        assert_eq!(acc.pending.len(), 1);
        let call = &acc.pending[&1];
        assert_eq!(call.id, "toolu_1");
        assert_eq!(call.name, "add");
        assert_eq!(call.arguments, r#"{"a":1}"#);
    }

    /// 文本与思考增量即时透传；ping / content_block_stop 不产出事件。
    #[test]
    fn stream_text_and_thinking_deltas() {
        let mut acc = StreamAccumulator::default();
        let text: StreamEvent = serde_json::from_str(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"你"}}"#,
        )
        .unwrap();
        let thinking: StreamEvent = serde_json::from_str(
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"thinking_delta","thinking":"推理中"}}"#,
        )
        .unwrap();
        let ping: StreamEvent = serde_json::from_str(r#"{"type":"ping"}"#).unwrap();
        let block_stop: StreamEvent =
            serde_json::from_str(r#"{"type":"content_block_stop","index":0}"#).unwrap();

        let outcome = apply_stream_event(text, &mut acc);
        assert_eq!(outcome.text_delta.as_deref(), Some("你"));
        assert!(outcome.thinking_delta.is_none());

        let outcome = apply_stream_event(thinking, &mut acc);
        assert_eq!(outcome.thinking_delta.as_deref(), Some("推理中"));
        assert!(outcome.text_delta.is_none());

        let outcome = apply_stream_event(ping, &mut acc);
        assert!(outcome.text_delta.is_none() && outcome.thinking_delta.is_none());
        let outcome = apply_stream_event(block_stop, &mut acc);
        assert!(outcome.text_delta.is_none() && outcome.thinking_delta.is_none());
    }

    /// usage 聚合：message_start 给 input + cache 字段，message_delta 给累计 output；
    /// 对齐 Java 不变量 prompt = input + cache_read + cache_creation。
    #[test]
    fn stream_usage_accumulation() {
        let mut acc = StreamAccumulator::default();
        assert!(acc.usage().is_none());

        let start: StreamEvent = serde_json::from_str(
            r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":100,"output_tokens":1,"cache_read_input_tokens":5,"cache_creation_input_tokens":3}}}"#,
        )
        .unwrap();
        apply_stream_event(start, &mut acc);
        let usage = acc.usage().unwrap();
        assert_eq!(usage.prompt_tokens, 108);
        assert_eq!(usage.completion_tokens, 1);

        let delta: StreamEvent = serde_json::from_str(
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":42}}"#,
        )
        .unwrap();
        let outcome = apply_stream_event(delta, &mut acc);
        assert!(!outcome.finished);
        assert_eq!(acc.finish_reason.as_deref(), Some("tool_use"));
        let usage = acc.usage().unwrap();
        assert_eq!(usage.prompt_tokens, 108);
        assert_eq!(usage.completion_tokens, 42);
        assert_eq!(usage.total_tokens, 150);
    }

    /// 流内 error 事件可解析（pump_sse 会将其转为 Err 并终止）。
    #[test]
    fn stream_error_event_detected() {
        let event: StreamEvent =
            serde_json::from_str(r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#).unwrap();
        assert_eq!(event.kind, "error");
        assert_eq!(event.error.unwrap().message.as_deref(), Some("Overloaded"));
    }

    /// 多工具调用按 index 独立聚合（并行 tool_use 块互不串扰）。
    #[test]
    fn stream_multiple_tool_calls_by_index() {
        let mut acc = StreamAccumulator::default();
        let start0: StreamEvent = serde_json::from_str(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_a","name":"add","input":{}}}"#,
        )
        .unwrap();
        let start1: StreamEvent = serde_json::from_str(
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_b","name":"mul","input":{}}}"#,
        )
        .unwrap();
        let delta1: StreamEvent = serde_json::from_str(
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"x\":2}"}}"#,
        )
        .unwrap();
        for event in [start0, start1, delta1] {
            apply_stream_event(event, &mut acc);
        }
        assert_eq!(acc.pending.len(), 2);
        assert_eq!(acc.pending[&0].name, "add");
        assert_eq!(acc.pending[&0].arguments, "");
        assert_eq!(acc.pending[&1].name, "mul");
        assert_eq!(acc.pending[&1].arguments, r#"{"x":2}"#);
    }

    /// assistant 消息含 tool_use 块时请求体形状（历史回传 + tool_result 展开）。
    #[test]
    fn assistant_history_with_tool_use_roundtrip() {
        let model = AnthropicChatModel::new("k", "claude-sonnet-4-5");
        let request = ChatRequest {
            messages: vec![
                Msg::user("user", "算一下"),
                Msg::assistant(
                    "assistant",
                    vec![ContentBlock::ToolUse {
                        id: "toolu_1".into(),
                        name: "add".into(),
                        input: json!({"a": 1}),
                        state: None,
                        content: None,
                    }],
                ),
                Msg::tool_result(
                    "tool",
                    "toolu_1",
                    "add",
                    "2",
                    agentscope_core::message::ToolResultState::Success,
                ),
            ],
            options: GenerateOptions::default(),
        };
        let body = model.build_request(&request, false).unwrap();
        assert_eq!(body.messages.len(), 3);
        assert_eq!(body.messages[0].role, "user");
        assert_eq!(body.messages[1].role, "assistant");
        assert_eq!(body.messages[1].content[0]["type"], "tool_use");
        assert_eq!(body.messages[2].role, "user");
        assert_eq!(body.messages[2].content[0]["type"], "tool_result");
        assert_eq!(body.messages[2].content[0]["tool_use_id"], "toolu_1");
    }
}
