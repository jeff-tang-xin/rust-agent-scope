//! # agentscope-model-ollama
//!
//! Ollama 原生 `/api/chat` 模型提供商（对应 Java 版 `agentscope-extensions-model-ollama`）。
//!
//! 走 Ollama 原生协议（非 OpenAI 兼容端点）：
//! - 端点：`POST {base_url}/api/chat`（默认 `http://localhost:11434`，无认证头）；
//! - 非流式：单个 JSON 响应对象（`stream: false` 显式下发——Ollama 默认流式）；
//! - 流式：**NDJSON 行流**（每行独立 JSON，非 SSE：无 `data:` 前缀、无 `[DONE]` 哨兵），
//!   逐行解析直到 `done=true` 或断流；
//! - 行内 `error` 字段 = 流内错误；坏行跳过不中断流（对齐 Java `OllamaHttpClient.stream`）。
//!
//! 第一阶段支持：system/user/assistant/tool 消息、image（裸 base64）输入、
//! function calling、temperature / num_predict、NDJSON token 级流式输出。
//!
//! 流式行为对齐 Java 版 `OllamaChatModel`：
//! - 工具调用 `arguments` 是 JSON 对象整块到达（不分片），无需聚合，
//!   但协议不返回调用 id，实现方生成 UUID（对齐 Java `randomUUID`）；
//! - usage 映射：`prompt_eval_count` → prompt_tokens、`eval_count` → completion_tokens；
//! - `done_reason` 缺失但 `done=true` → finish_reason 回落 `"stop"`；
//! - 流以 `ModelEvent::Done`（usage / finish_reason）收尾。

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use uuid::Uuid;

use agentscope_core::error::{Error, Result};
use agentscope_core::formatter::Formatter;
use agentscope_core::model::{ChatRequest, ChatResponse, GenerateOptions, Model, ModelEvent, Usage};

mod formatter;
pub use formatter::OllamaChatFormatter;

/// 默认服务地址（本地 Ollama 守护进程）。
const DEFAULT_BASE_URL: &str = "http://localhost:11434";

/// Ollama 聊天模型。
pub struct OllamaChatModel {
    base_url: String,
    model: String,
    temperature: Option<f64>,
    max_tokens: Option<u32>,
    /// 是否下发 `think` 开关（思考模型如 qwen3 的思考模式控制；None = 不下发）。
    think: Option<bool>,
    /// 上下文窗口（token；0 = 未知）——压缩预算按「窗口 − reserved」计算。
    context_window: usize,
    client: Client,
    /// 消息与参数的格式化器（对应 Java `OllamaChatFormatter`）。
    formatter: OllamaChatFormatter,
}

impl OllamaChatModel {
    /// 创建模型实例。`base_url` 默认为 `http://localhost:11434`（本地 Ollama，无认证）。
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            model: model.into(),
            temperature: None,
            max_tokens: None,
            think: None,
            context_window: 0,
            client: Client::new(),
            formatter: OllamaChatFormatter,
        }
    }

    /// 替换服务地址（远程 Ollama / 反向代理等）。
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

    /// 默认最大生成 token 数（请求级 options 可覆盖，映射为 `num_predict`）。
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// 默认思考模式开关（下发请求体 `think` 字段；None = 不下发）。
    pub fn with_think(mut self, think: bool) -> Self {
        self.think = Some(think);
        self
    }

    /// 端点 URL：流式与非流式同路径，以请求体 `stream` 字段区分。
    fn endpoint(&self) -> String {
        format!("{}/api/chat", self.base_url.trim_end_matches('/'))
    }

    /// 组装请求体：消息、工具定义、生成参数全部经由 formatter 写入。
    ///
    /// 对应 Java `OllamaChatModel` 的 prepareRequest：模型只负责传输，
    /// 格式转换由 formatter 完成，`defaults` 为模型实例上的默认选项。
    fn build_request(&self, request: &ChatRequest, stream: bool) -> Result<OllamaRequest> {
        let defaults = GenerateOptions {
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            ..Default::default()
        };
        let mut body = OllamaRequest {
            model: request
                .options
                .model
                .clone()
                .unwrap_or_else(|| self.model.clone()),
            messages: self.formatter.format(&request.messages, Some(&request.options))?,
            stream,
            think: self.think,
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
impl Model for OllamaChatModel {
    fn model_name(&self) -> &str {
        &self.model
    }

    fn context_window_size(&self) -> usize {
        self.context_window
    }

    async fn call(&self, request: ChatRequest) -> Result<ChatResponse> {
        let body = self.build_request(&request, false)?;

        let resp = self
            .client
            .post(self.endpoint())
            // Ollama 本地服务无认证头（对齐 Java OllamaHttpClient）
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

        let response: OllamaResponse = serde_json::from_str(&text)
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
            .post(self.endpoint())
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

        // NDJSON 行解析在独立任务中进行，通过 channel 把事件推给调用方
        tokio::spawn(async move {
            let _ = pump_ndjson(resp, tx).await;
        });
        Ok(rx)
    }
}

// ---------- Ollama /api/chat 协议结构体 ----------

/// `/api/chat` 请求体。
#[derive(Serialize, Default, Debug, Clone)]
pub struct OllamaRequest {
    pub model: String,
    pub messages: Vec<OllamaMessage>,
    /// Ollama 默认流式，非流式调用必须显式下发 `false`，故恒序列化。
    pub stream: bool,
    /// 思考模式开关（思考模型如 qwen3；None = 不下发）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub think: Option<bool>,
    /// 采样参数（temperature / num_predict）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<OllamaOptions>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<OllamaTool>,
    /// 工具选择策略（`"auto"` / `"none"` / 指定函数对象）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
}

/// 采样参数（裁剪口径：只落 temperature / num_predict，max_tokens → num_predict）。
#[derive(Serialize, Deserialize, Default, Debug, Clone, Copy, PartialEq)]
pub struct OllamaOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_predict: Option<u32>,
}

/// 一条对话消息（请求与响应共用：响应 message 是其子集形态）。
#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct OllamaMessage {
    #[serde(default)]
    pub role: String,
    /// 文本内容（tool 消息恒有；assistant 增量可为空串）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// 思考内容（思考模型输出）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// 裸 base64 字符串数组（无 data URI 前缀）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<OllamaToolCall>>,
    /// 仅 tool 消息携带：对应的工具调用 id。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// 仅 tool 消息携带：工具名。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// 模型发起的工具调用。Ollama 协议无 id / type 字段（对齐 Java `OllamaToolCall`）。
#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct OllamaToolCall {
    pub function: OllamaToolFunction,
}

/// 工具调用的函数体：`arguments` 是 JSON 对象（非字符串，与 OpenAI 协议的关键差异）。
#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct OllamaToolFunction {
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Value>,
}

/// 工具定义（`{"type":"function","function":{...}}`，对齐 Java `OllamaTool`）。
#[derive(Serialize, Debug, Clone)]
pub struct OllamaTool {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: OllamaToolDef,
}

#[derive(Serialize, Debug, Clone)]
pub struct OllamaToolDef {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// `/api/chat` 响应体（非流式完整响应与流式单行 chunk 同构）。
#[derive(Deserialize, Default, Debug, Clone)]
#[serde(default)]
pub struct OllamaResponse {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub message: Option<OllamaMessage>,
    #[serde(default)]
    pub done: Option<bool>,
    #[serde(default)]
    pub done_reason: Option<String>,
    /// 提示 token 数（usage 映射 → prompt_tokens）。
    #[serde(default)]
    pub prompt_eval_count: Option<u64>,
    /// 生成 token 数（usage 映射 → completion_tokens）。
    #[serde(default)]
    pub eval_count: Option<u64>,
    /// 错误字段（非流式响应与流内均可出现；通常是字符串）。
    #[serde(default)]
    pub error: Option<Value>,
}

// ---------- 流式（NDJSON 行流）解析 ----------

/// 流式泵的跨行聚合状态（usage / finish_reason 在终止行或断流时统一收尾）。
#[derive(Default)]
struct StreamState {
    usage: Option<Usage>,
    finish_reason: Option<String>,
}

/// 解析单个 NDJSON 行；空行 / 坏行返回 `None`（调用方跳过，不中断流）。
fn parse_ndjson_line(line: &str) -> Option<OllamaResponse> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    serde_json::from_str(trimmed).ok()
}

/// 把单个 NDJSON 行转换为待发送事件序列。
///
/// 返回 `(事件列表, 是否继续流)`：
/// - 坏行 / 空行：跳过不中断流（对齐 Java `OllamaHttpClient.stream`）；
/// - 行内 `error` 字段：流内错误，发出 `Err` 并终止；
/// - `done=true`：发出 `Done`（usage / finish_reason）并终止；
/// - 工具调用整块到达，调用 id 在此生成 UUID（Ollama 协议不返回 id）。
fn line_to_events(line: &str, state: &mut StreamState) -> (Vec<Result<ModelEvent>>, bool) {
    let Some(chunk) = parse_ndjson_line(line) else {
        return (Vec::new(), true);
    };
    if let Some(err) = chunk.error {
        return (
            vec![Err(Error::Model(format!(
                "stream error: {}",
                formatter::error_text(&err)
            )))],
            false,
        );
    }

    let outcome = formatter::apply_stream_chunk(chunk);
    let mut events = Vec::new();
    if let Some(text) = outcome.thinking_delta {
        events.push(Ok(ModelEvent::ThinkingDelta { text }));
    }
    if let Some(text) = outcome.text_delta {
        events.push(Ok(ModelEvent::TextDelta { text }));
    }
    for (name, input) in outcome.tool_calls {
        events.push(Ok(ModelEvent::ToolUse {
            id: Uuid::new_v4().to_string(),
            name,
            input,
        }));
    }
    if outcome.usage.is_some() {
        state.usage = outcome.usage;
    }
    if outcome.done {
        if outcome.finish_reason.is_some() {
            state.finish_reason = outcome.finish_reason;
        }
        events.push(Ok(ModelEvent::Done {
            usage: state.usage,
            finish_reason: state.finish_reason.clone(),
        }));
        return (events, false);
    }
    (events, true)
}

/// 逐行解析 NDJSON 字节流，把模型事件推入 channel，直到 `done=true` 或流断开。
///
/// 与 SSE 的区别：无 `data:` 前缀、无 `[DONE]` 哨兵，每行就是完整 JSON。
async fn pump_ndjson(resp: reqwest::Response, tx: mpsc::Sender<Result<ModelEvent>>) -> Result<()> {
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    let mut state = StreamState::default();

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
            let (events, continue_stream) = line_to_events(line.trim(), &mut state);
            for event in events {
                if tx.send(event).await.is_err() {
                    return Ok(()); // 接收方已关闭，停止发送
                }
            }
            if !continue_stream {
                return Ok(());
            }
        }
    }

    // 服务端断流时可能残留最后一行（无换行符结尾），补处理
    let remainder = buf.trim().to_string();
    if !remainder.is_empty() {
        let (events, _) = line_to_events(&remainder, &mut state);
        for event in events {
            if tx.send(event).await.is_err() {
                return Ok(());
            }
        }
    }

    // 断流未收到 done=true：以已累积的 usage / finish_reason 收尾
    let _ = tx
        .send(Ok(ModelEvent::Done {
            usage: state.usage,
            finish_reason: state.finish_reason,
        }))
        .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::message::Msg;
    use agentscope_core::model::{ToolChoice, ToolSchema};
    use serde_json::json;

    #[test]
    fn context_window_defaults_unknown_and_is_settable() {
        assert_eq!(OllamaChatModel::new("m").context_window_size(), 0);
        assert_eq!(
            OllamaChatModel::new("m")
                .with_context_window(32_000)
                .context_window_size(),
            32_000
        );
    }

    #[test]
    fn model_name_returns_model() {
        assert_eq!(OllamaChatModel::new("qwen3").model_name(), "qwen3");
    }

    /// 端点 URL：默认 base + /api/chat；自定义 base 去尾部斜杠。
    #[test]
    fn endpoint_url() {
        assert_eq!(
            OllamaChatModel::new("m").endpoint(),
            "http://localhost:11434/api/chat"
        );
        assert_eq!(
            OllamaChatModel::new("m")
                .with_base_url("http://192.168.1.5:11434/")
                .endpoint(),
            "http://192.168.1.5:11434/api/chat"
        );
    }

    /// 请求体序列化：stream 恒下发（Ollama 默认流式）；think / options / tools /
    /// tool_choice 缺省省略；工具定义 type 字段改名；options 嵌套对象。
    #[test]
    fn request_body_serialization() {
        let body = OllamaRequest {
            model: "qwen3".into(),
            messages: vec![OllamaMessage {
                role: "user".into(),
                content: Some("hi".into()),
                ..Default::default()
            }],
            stream: true,
            think: Some(true),
            options: Some(OllamaOptions {
                temperature: Some(0.5),
                num_predict: Some(64),
            }),
            tools: vec![OllamaTool {
                kind: "function".into(),
                function: OllamaToolDef {
                    name: "add".into(),
                    description: "加法".into(),
                    parameters: json!({"type": "object"}),
                },
            }],
            tool_choice: Some(json!("auto")),
        };
        let v = serde_json::to_value(&body).unwrap();
        assert_eq!(v["model"], "qwen3");
        assert_eq!(v["messages"][0]["role"], "user");
        assert_eq!(v["stream"], true);
        assert_eq!(v["think"], true);
        assert_eq!(v["options"]["temperature"], 0.5);
        assert_eq!(v["options"]["num_predict"], 64);
        assert_eq!(v["tools"][0]["type"], "function");
        assert_eq!(v["tools"][0]["function"]["name"], "add");
        assert_eq!(v["tool_choice"], "auto");

        // 默认体：stream 显式 false，可省字段全部省略
        let v = serde_json::to_value(OllamaRequest::default()).unwrap();
        assert_eq!(v["stream"], false);
        assert!(v.get("think").is_none());
        assert!(v.get("options").is_none());
        assert!(v.get("tools").is_none());
        assert!(v.get("tool_choice").is_none());
    }

    /// build_request 集成：formatter 完成消息转换 / 参数 / 工具 / 工具选择写入，
    /// options.model 覆盖实例默认模型名。
    #[test]
    fn build_request_applies_formatter() {
        let model = OllamaChatModel::new("qwen3").with_think(false);
        let request = ChatRequest {
            messages: vec![Msg::system("system", "你是助手"), Msg::user("user", "你好")],
            options: GenerateOptions {
                model: Some("qwen3:8b".into()),
                temperature: Some(0.7),
                max_tokens: Some(256),
                tools: vec![ToolSchema {
                    name: "add".into(),
                    description: "加法".into(),
                    parameters: json!({"type": "object", "properties": {}}),
                }],
                tool_choice: Some(ToolChoice::Specific {
                    tool_name: "add".into(),
                }),
                ..Default::default()
            },
        };
        let body = model.build_request(&request, false).unwrap();
        assert_eq!(body.model, "qwen3:8b");
        assert_eq!(body.messages.len(), 2);
        assert_eq!(body.messages[0].role, "system");
        assert_eq!(body.messages[1].role, "user");
        assert_eq!(body.stream, false);
        assert_eq!(body.think, Some(false));
        assert_eq!(
            body.options,
            Some(OllamaOptions {
                temperature: Some(0.7),
                num_predict: Some(256)
            })
        );
        assert_eq!(body.tools[0].kind, "function");
        assert_eq!(
            body.tool_choice,
            Some(json!({"type": "function", "function": {"name": "add"}}))
        );
    }

    /// NDJSON 文本 / 思考行：先思考增量后文本增量，空 content 不产出增量。
    #[test]
    fn ndjson_text_and_thinking_lines() {
        let mut state = StreamState::default();
        let (events, cont) = line_to_events(
            r#"{"model":"qwen3","message":{"role":"assistant","content":"你","thinking":"想"},"done":false}"#,
            &mut state,
        );
        assert!(cont);
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], Ok(ModelEvent::ThinkingDelta { text }) if text == "想"));
        assert!(matches!(&events[1], Ok(ModelEvent::TextDelta { text }) if text == "你"));

        // 空 content（done 行前的收尾增量）不产出事件
        let (events, cont) = line_to_events(
            r#"{"message":{"role":"assistant","content":""},"done":false}"#,
            &mut state,
        );
        assert!(cont);
        assert!(events.is_empty());
    }

    /// 坏行 / 空行跳过不中断流，后续正常行继续处理。
    #[test]
    fn ndjson_bad_lines_are_skipped() {
        let mut state = StreamState::default();
        for bad in ["not a json line", "{\"broken\":", "   ", ""] {
            let (events, cont) = line_to_events(bad, &mut state);
            assert!(cont, "bad line must not terminate: {bad:?}");
            assert!(events.is_empty(), "bad line must emit nothing: {bad:?}");
        }
        let (events, cont) = line_to_events(
            r#"{"message":{"role":"assistant","content":"好"},"done":false}"#,
            &mut state,
        );
        assert!(cont);
        assert_eq!(events.len(), 1);
    }

    /// 终止行：usage 映射（prompt_eval_count/eval_count → prompt/completion/total）
    /// 与 done_reason；done=true 缺 done_reason 回落 "stop"。
    #[test]
    fn ndjson_done_line_maps_usage_and_finish_reason() {
        let mut state = StreamState::default();
        let (events, cont) = line_to_events(
            r#"{"message":{"role":"assistant","content":""},"done":true,"done_reason":"stop","prompt_eval_count":10,"eval_count":5}"#,
            &mut state,
        );
        assert!(!cont);
        assert_eq!(events.len(), 1);
        match &events[0] {
            Ok(ModelEvent::Done { usage, finish_reason }) => {
                let u = usage.unwrap();
                assert_eq!(
                    (u.prompt_tokens, u.completion_tokens, u.total_tokens),
                    (10, 5, 15)
                );
                assert_eq!(finish_reason.as_deref(), Some("stop"));
            }
            other => panic!("expected Done, got {other:?}"),
        }

        let mut state = StreamState::default();
        let (events, cont) = line_to_events(
            r#"{"message":{"role":"assistant","content":"完"},"done":true}"#,
            &mut state,
        );
        assert!(!cont);
        // content 非空先产出 TextDelta，随后 done=true 产出 Done（finish_reason 回落 "stop"）
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], Ok(ModelEvent::TextDelta { text }) if text == "完"));
        assert!(matches!(
            &events[1],
            Ok(ModelEvent::Done { finish_reason: Some(r), .. }) if r == "stop"
        ));
    }

    /// 工具调用整块到达：id 本地生成 UUID 且互不相同，name / arguments 原样透传。
    #[test]
    fn ndjson_tool_calls_get_generated_uuid_ids() {
        let mut state = StreamState::default();
        let (events, cont) = line_to_events(
            r#"{"message":{"role":"assistant","tool_calls":[{"function":{"name":"add","arguments":{"a":1}}},{"function":{"name":"sub","arguments":{}}}]},"done":false}"#,
            &mut state,
        );
        assert!(cont);
        assert_eq!(events.len(), 2);
        let mut ids = Vec::new();
        for event in &events {
            match event {
                Ok(ModelEvent::ToolUse { id, name, input }) => {
                    assert!(uuid::Uuid::parse_str(id).is_ok(), "id must be a UUID");
                    ids.push(id.clone());
                    assert!(!name.is_empty());
                    assert!(input.is_object());
                }
                other => panic!("expected ToolUse, got {other:?}"),
            }
        }
        assert_ne!(ids[0], ids[1], "each call gets a distinct generated id");
    }

    /// 行内 error 字段 = 流内错误：发出 Err 并终止流。
    #[test]
    fn ndjson_error_line_terminates_stream() {
        let mut state = StreamState::default();
        let (events, cont) = line_to_events(r#"{"error":"model not found"}"#, &mut state);
        assert!(!cont);
        assert_eq!(events.len(), 1);
        let err = events[0].as_ref().unwrap_err();
        assert!(err.to_string().contains("model not found"));
    }
}