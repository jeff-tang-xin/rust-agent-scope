//! # agentscope-model-dashscope
//!
//! 阿里 DashScope 原生协议模型提供商（对应 Java 版
//! `agentscope-extensions-model-dashscope`；OpenAI 兼容模式请直接用
//! `agentscope-model-openai` 指向 DashScope 兼容端点）。
//!
//! 走原生协议（`POST {base_url}/api/v1/services/aigc/text-generation/generation`）：
//! - 请求体 `model` + `input.messages` + `parameters`（`result_format: "message"`）；
//! - 流式加请求头 `X-DashScope-SSE: enable`，SSE 每行 `data: {完整响应 JSON}`，
//!   `content` 为增量（`parameters.incremental_output = true`）；
//! - 鉴权：`Authorization: Bearer <key>`。
//!
//! 第一阶段支持：system/user/assistant/tool 消息、function calling、
//! temperature / max_tokens、`reasoning_content` 思考增量、SSE 流式输出。
//!
//! 流式行为对齐 Java 版 `DashScopeChatModel`：
//! - 工具调用分片（首块带 id/name + 部分参数，后续块只有参数片段）在实现方
//!   按 index 聚合完整后才产出 `ModelEvent::ToolUse`（Java 版以 `__fragment__`
//!   占位抛给上层聚合，此处聚合位置不同、最终语义一致）；
//! - 流以 `ModelEvent::Done`（usage / finish_reason）收尾；DashScope 无 `[DONE]`
//!   哨兵，以流关闭为结束信号（遇到 `[DONE]` 也兼容收尾）。
//!
//! 与 Java 版的已知差异（裁剪口径，见 formatter 模块注释）：
//! - 只做 text-generation 端点（多模态 multimodal-generation 端点与按模型名
//!   AUTO 路由未复刻），媒体块转文本占位；
//! - `DashScopeMultiModalTool`（内置多模态工具）、`EncryptionUtils`（API 加密）、
//!   search_options 等扩展参数未复刻。

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
pub use formatter::DashScopeChatFormatter;

/// 默认服务地址（DashScope 新加坡/北京区共用域名）。
const DEFAULT_BASE_URL: &str = "https://dashscope.aliyuncs.com";

/// 文本生成端点（多模态端点 `/api/v1/services/aigc/multimodal-generation/generation`
/// 未复刻，见模块注释的裁剪口径）。
const TEXT_GENERATION_PATH: &str = "/api/v1/services/aigc/text-generation/generation";

/// DashScope 原生协议聊天模型。
pub struct DashScopeChatModel {
    api_key: String,
    base_url: String,
    model: String,
    temperature: Option<f64>,
    max_tokens: Option<u32>,
    /// 上下文窗口（token；0 = 未知）——压缩预算按「窗口 − reserved」计算。
    context_window: usize,
    client: Client,
    /// 消息与参数的格式化器（对应 Java `DashScopeChatFormatter`）。
    formatter: DashScopeChatFormatter,
}

impl DashScopeChatModel {
    /// 创建模型实例。`base_url` 默认为 `https://dashscope.aliyuncs.com`。
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
            model: model.into(),
            temperature: None,
            max_tokens: None,
            context_window: 0,
            client: Client::new(),
            formatter: DashScopeChatFormatter::new(),
        }
    }

    /// 替换服务地址。
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
    /// 对应 Java `DashScopeChatModel` 的请求装配：模型只负责传输，
    /// 格式转换由 formatter 完成，`defaults` 为模型实例上的默认选项。
    /// `stream = true` 时自动带 `parameters.incremental_output = true`
    /// （DashScope 流式增量输出的必要开关）。
    fn build_request(
        &self,
        request: &ChatRequest,
        stream: bool,
    ) -> Result<DashScopeRequest> {
        let defaults = GenerateOptions {
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            ..Default::default()
        };
        let mut body = DashScopeRequest {
            model: request
                .options
                .model
                .clone()
                .unwrap_or_else(|| self.model.clone()),
            input: DashScopeInput {
                messages: self.formatter.format(&request.messages, Some(&request.options))?,
            },
            parameters: Some(DashScopeParameters {
                result_format: "message".into(),
                incremental_output: if stream { Some(true) } else { None },
                ..Default::default()
            }),
        };
        if let Some(params) = body.parameters.as_mut() {
            self.formatter
                .apply_options(params, &request.options, Some(&defaults));
            self.formatter.apply_tools(params, &request.options.tools);
            self.formatter
                .apply_tool_choice(params, request.options.tool_choice.as_ref());
        }
        Ok(body)
    }

    fn endpoint(&self) -> String {
        format!("{}{}", self.base_url.trim_end_matches('/'), TEXT_GENERATION_PATH)
    }
}

// ---------- DashScope 协议结构体 ----------

/// 请求体（对应 Java `DashScopeRequest`）。
#[derive(Serialize, Default)]
pub struct DashScopeRequest {
    pub model: String,
    pub input: DashScopeInput,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<DashScopeParameters>,
}

/// 请求输入（对应 Java `DashScopeInput`）。
#[derive(Serialize, Default)]
pub struct DashScopeInput {
    pub messages: Vec<DashScopeMessage>,
}

/// 消息（对应 Java `DashScopeMessage`）。
///
/// `content` 简单格式为字符串；保留 `Value` 形态以对齐协议允许的
/// 多模态数组（本 crate 只发字符串，媒体块由 formatter 转文本占位）。
#[derive(Serialize, Deserialize, Default)]
pub struct DashScopeMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
    /// 工具名（tool 消息携带，对齐 Java `convertToolRoleMessage`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<DashScopeToolCall>>,
    /// 思考 / 推理内容（Qwen3 等思考模型；请求历史回传与响应解析共用）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

/// 生成参数（对应 Java `DashScopeParameters`；只落本 crate 支持的字段）。
#[derive(Serialize, Default)]
pub struct DashScopeParameters {
    /// 响应格式，固定 `"message"`（结构化消息形态）。
    pub result_format: String,
    /// 流式增量输出开关（流式请求必须为 true）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incremental_output: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<DashScopeTool>,
    /// 工具选择策略（`"auto"` / `"none"` / 指定函数对象）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
}

/// 工具定义（对应 Java `DashScopeTool`，OpenAI 兼容的 function 形态）。
#[derive(Serialize, Deserialize)]
pub struct DashScopeTool {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: DashScopeToolFunction,
}

/// 工具函数定义（对应 Java `DashScopeToolFunction`）。
#[derive(Serialize, Deserialize)]
pub struct DashScopeToolFunction {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// 工具调用（对应 Java `DashScopeToolCall`；`arguments` 是 JSON 字符串，
/// 流式分片共享相同 `index`）。
#[derive(Serialize, Deserialize, Clone)]
pub struct DashScopeToolCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: DashScopeFunction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<u32>,
}

/// 工具调用函数部分（对应 Java `DashScopeFunction`）。
#[derive(Serialize, Deserialize, Clone)]
pub struct DashScopeFunction {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

/// 响应体（对应 Java `DashScopeResponse`；同时承载错误形态——
/// DashScope 错误为顶层 `code` / `message` / `request_id`，无 `output`）。
#[derive(Deserialize, Default)]
pub struct DashScopeResponse {
    #[serde(default)]
    pub request_id: Option<String>,
    #[serde(default)]
    pub output: Option<DashScopeOutput>,
    #[serde(default)]
    pub usage: Option<DashScopeUsage>,
    // 错误响应字段（正常响应不出现）
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

/// 响应输出（对应 Java `DashScopeOutput`）。
#[derive(Deserialize, Default)]
pub struct DashScopeOutput {
    #[serde(default)]
    pub choices: Vec<DashScopeChoice>,
    /// output 级结束原因（choice 级缺失时回退，对齐 Java 解析顺序）。
    #[serde(default)]
    pub finish_reason: Option<String>,
    /// `result_format: "text"` 时的纯文本形态（message 形态下不出现，保留兼容）。
    #[serde(default)]
    pub text: Option<String>,
}

/// 响应选项（对应 Java `DashScopeChoice`）。
#[derive(Deserialize, Default)]
pub struct DashScopeChoice {
    #[serde(default)]
    pub message: Option<DashScopeMessage>,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// 用量（对应 Java `DashScopeUsage`；字段名与 OpenAI 不同：
/// `input_tokens` / `output_tokens` / `total_tokens`）。
#[derive(Deserialize, Default)]
pub struct DashScopeUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

#[async_trait]
impl Model for DashScopeChatModel {
    fn context_window_size(&self) -> usize {
        self.context_window
    }

    async fn call(&self, request: ChatRequest) -> Result<ChatResponse> {
        let body = self.build_request(&request, false)?;

        let resp = self
            .client
            .post(self.endpoint())
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

        let response: DashScopeResponse = serde_json::from_str(&text)
            .map_err(|e| Error::Model(format!("failed to parse response: {e}")))?;
        check_response_error(&response)?;
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
            .post(self.endpoint())
            .bearer_auth(&self.api_key)
            .header("X-DashScope-SSE", "enable")
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

/// DashScope 错误响应检查：顶层 `code` / `message` 存在且无 `output` 即错误
/// （对齐 Java `DashScopeHttpClient` 的错误分支）。
fn check_response_error(response: &DashScopeResponse) -> Result<()> {
    if response.output.is_none() {
        if let Some(code) = &response.code {
            return Err(Error::Model(format!(
                "dashscope error {code}: {} (request_id: {})",
                response.message.as_deref().unwrap_or("unknown"),
                response.request_id.as_deref().unwrap_or("-")
            )));
        }
    }
    Ok(())
}

// ---------- 流式（SSE）解析 ----------

/// 单个 chunk 解析后的产出。
struct ChunkOutcome {
    text_delta: Option<String>,
    thinking_delta: Option<String>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
}

/// 聚合中的工具调用：流式协议把 name / arguments 分片发送，按 index 拼接
/// （首块带 id/name + 部分参数，后续块只有参数片段，对齐 Java `__fragment__`
/// 分片的实际线上形态）。
#[derive(Default)]
struct PendingToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// 把一个流式 chunk 应用到聚合状态（对齐 Java 版 `DashScopeResponseParser`
/// 的语义，但工具调用分片在此聚合完整，而非以 `__fragment__` 占位抛给上层）。
fn apply_stream_chunk(
    chunk: DashScopeResponse,
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
            prompt_tokens: u.input_tokens,
            completion_tokens: u.output_tokens,
            total_tokens: u.total_tokens,
        });
    }

    let Some(output) = chunk.output else {
        return outcome;
    };
    if output.finish_reason.is_some() {
        outcome.finish_reason = output.finish_reason.clone();
    }

    for choice in output.choices {
        if choice.finish_reason.is_some() {
            // choice 级优先（对齐 Java 解析顺序）
            outcome.finish_reason = choice.finish_reason.clone();
        }
        let Some(message) = choice.message else {
            continue;
        };
        if let Some(thinking) = message.reasoning_content {
            if !thinking.is_empty() {
                outcome.thinking_delta = Some(thinking);
            }
        }
        if let Some(content) = message.content {
            // 增量 content：简单格式为字符串；数组形态（多模态）取文本部分
            match content {
                Value::String(text) => {
                    if !text.is_empty() {
                        outcome.text_delta = Some(text);
                    }
                }
                Value::Array(parts) => {
                    let text = parts
                        .iter()
                        .filter_map(|p| p.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("");
                    if !text.is_empty() {
                        outcome.text_delta = Some(text);
                    }
                }
                _ => {}
            }
        }
        if let Some(calls) = message.tool_calls {
            for call in calls {
                let index = call.index.unwrap_or_default();
                let entry = pending.entry(index).or_default();
                if let Some(id) = call.id {
                    if !id.is_empty() {
                        entry.id = id;
                    }
                }
                if let Some(name) = call.function.name {
                    if !name.is_empty() && name != "__fragment__" {
                        entry.name = name;
                    }
                }
                if let Some(args) = call.function.arguments {
                    entry.arguments.push_str(&args);
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

/// 逐行解析 SSE 字节流，把模型事件推入 channel，直到流关闭
/// （DashScope 无 `[DONE]` 哨兵，遇到也兼容收尾）。
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
                // 兼容形态：正常 DashScope 流以连接关闭结束
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

            let parsed: DashScopeResponse = match serde_json::from_str(data) {
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
            // 流内错误响应（顶层 code/message，对齐 Java SSE 错误分支）
            if parsed.output.is_none() && parsed.code.is_some() {
                let _ = tx
                    .send(Err(Error::Model(format!(
                        "dashscope stream error {}: {} (request_id: {})",
                        parsed.code.as_deref().unwrap_or("unknown"),
                        parsed.message.as_deref().unwrap_or("unknown"),
                        parsed.request_id.as_deref().unwrap_or("-")
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

    // 流关闭：冲刷聚合状态并收尾
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
    use super::*;
    use serde_json::json;

    #[test]
    fn context_window_defaults_unknown_and_is_settable() {
        assert_eq!(DashScopeChatModel::new("k", "m").context_window_size(), 0);
        assert_eq!(
            DashScopeChatModel::new("k", "m")
                .with_context_window(32_000)
                .context_window_size(),
            32_000
        );
    }

    #[test]
    fn endpoint_joins_base_and_path() {
        assert_eq!(
            DashScopeChatModel::new("k", "m").endpoint(),
            "https://dashscope.aliyuncs.com/api/v1/services/aigc/text-generation/generation"
        );
        assert_eq!(
            DashScopeChatModel::new("k", "m")
                .with_base_url("https://dashscope.aliyuncs.com/")
                .endpoint(),
            "https://dashscope.aliyuncs.com/api/v1/services/aigc/text-generation/generation"
        );
    }

    /// 流式请求体：incremental_output=true；非流式不带该开关。
    #[test]
    fn stream_request_sets_incremental_output() {
        let model = DashScopeChatModel::new("k", "qwen-plus");
        let request = ChatRequest {
            messages: vec![agentscope_core::message::Msg::user("user", "你好")],
            options: GenerateOptions::default(),
        };
        let stream_body = model.build_request(&request, true).unwrap();
        let v = serde_json::to_value(&stream_body).unwrap();
        assert_eq!(v["model"], "qwen-plus");
        assert_eq!(v["parameters"]["result_format"], "message");
        assert_eq!(v["parameters"]["incremental_output"], true);
        assert_eq!(v["input"]["messages"][0]["role"], "user");

        let plain_body = model.build_request(&request, false).unwrap();
        let v = serde_json::to_value(&plain_body).unwrap();
        assert!(v["parameters"].get("incremental_output").is_none());
    }

    /// 增量 content chunk：字符串增量产出 TextDelta；usage / finish_reason 记录。
    #[test]
    fn stream_text_usage_and_finish_chunks() {
        let mut pending = BTreeMap::new();
        let chunk: DashScopeResponse = serde_json::from_str(
            r#"{"output":{"choices":[{"message":{"role":"assistant","content":"你"},"finish_reason":null}]},"usage":null}"#,
        )
        .unwrap();
        let o = apply_stream_chunk(chunk, &mut pending);
        assert_eq!(o.text_delta.as_deref(), Some("你"));
        assert!(o.usage.is_none());

        let chunk: DashScopeResponse = serde_json::from_str(
            r#"{"output":{"choices":[{"message":{"role":"assistant","content":""},"finish_reason":"stop"}]},"usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15},"request_id":"rid"}"#,
        )
        .unwrap();
        let o = apply_stream_chunk(chunk, &mut pending);
        assert_eq!(o.finish_reason.as_deref(), Some("stop"));
        let usage = o.usage.unwrap();
        assert_eq!((usage.prompt_tokens, usage.completion_tokens, usage.total_tokens), (10, 5, 15));
    }

    /// 思考增量 chunk：reasoning_content 增量产出 ThinkingDelta。
    #[test]
    fn stream_thinking_chunks() {
        let mut pending = BTreeMap::new();
        let chunk: DashScopeResponse = serde_json::from_str(
            r#"{"output":{"choices":[{"message":{"role":"assistant","content":"","reasoning_content":"思考中"}}]}}"#,
        )
        .unwrap();
        let o = apply_stream_chunk(chunk, &mut pending);
        assert_eq!(o.thinking_delta.as_deref(), Some("思考中"));
        assert!(o.text_delta.is_none());
    }

    /// 工具调用分片聚合：首块带 id/name + 部分参数，后续块只有参数片段
    /// （对齐 Java `__fragment__` 分片的线上形态，按 index 拼接）。
    #[test]
    fn stream_tool_call_fragment_aggregation() {
        let mut pending = BTreeMap::new();
        let first: DashScopeResponse = serde_json::from_str(
            r#"{"output":{"choices":[{"message":{"role":"assistant","content":"","tool_calls":[{"id":"call_1","type":"function","function":{"name":"add","arguments":"{\"a\":"},"index":0}]}}]}}"#,
        )
        .unwrap();
        let o = apply_stream_chunk(first, &mut pending);
        assert!(o.text_delta.is_none());

        let second: DashScopeResponse = serde_json::from_str(
            r#"{"output":{"choices":[{"message":{"role":"assistant","content":"","tool_calls":[{"type":"function","function":{"arguments":"1}"},"index":0}]}}]}}"#,
        )
        .unwrap();
        apply_stream_chunk(second, &mut pending);

        assert_eq!(pending.len(), 1);
        let call = &pending[&0];
        assert_eq!(call.id, "call_1");
        assert_eq!(call.name, "add");
        assert_eq!(call.arguments, r#"{"a":1}"#);
    }

    /// 错误响应检查：顶层 code/message 且无 output → 报错；正常响应不误报。
    #[test]
    fn error_response_detected() {
        let err: DashScopeResponse = serde_json::from_str(
            r#"{"code":"InvalidApiKey","message":"Invalid API-key provided.","request_id":"rid-1"}"#,
        )
        .unwrap();
        let e = check_response_error(&err).unwrap_err();
        assert!(e.to_string().contains("InvalidApiKey"));

        let ok: DashScopeResponse = serde_json::from_str(
            r#"{"output":{"choices":[{"message":{"role":"assistant","content":"hi"}}]},"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2},"request_id":"rid"}"#,
        )
        .unwrap();
        assert!(check_response_error(&ok).is_ok());
    }
}
