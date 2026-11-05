//! # agentscope-model-openai-official
//!
//! OpenAI Responses API 模型提供商（对应 Java 版
//! `agentscope-extensions-model-openai-official`；Chat Completions 协议请用
//! `agentscope-model-openai`）。
//!
//! 走 Responses API（`POST {base_url}/v1/responses`，默认
//! `https://api.openai.com`，鉴权 `Authorization: Bearer <key>`）：
//! - 请求体 `model` + `input[]`（条目式：message / function_call /
//!   function_call_output）+ `store: false`（Responses API 默认有状态，
//!   省略会保留服务端会话）+ `temperature` / `max_output_tokens` / `tools` /
//!   `tool_choice`；
//! - system 消息走 `input[]` 里的 `{type:"message",role:"system"}` 条目，
//!   不用独立 `instructions` 字段（对齐 Java `ResponsesRequestMapper`）；
//! - 工具定义扁平 `{type:"function",name,description,parameters,strict}`；
//! - 工具历史回传：assistant 侧 `function_call{call_id,name,arguments}` +
//!   tool 侧 `function_call_output{call_id,output}`。
//!
//! 流式（SSE）按事件 `type` 分派（对齐 Java `ResponsesStreamHandler`）：
//! - `response.output_text.delta` → `ModelEvent::TextDelta`；
//! - `response.reasoning_summary_text.delta` / `response.reasoning_text.delta`
//!   → `ModelEvent::ThinkingDelta`；
//! - `response.output_item.added` 登记 function_call 条目，
//!   `response.function_call_arguments.delta` 按 `item_id` 拼接参数，
//!   `response.output_item.done` 整块产出 `ModelEvent::ToolUse`；
//! - `response.completed` / `response.incomplete` 终态取
//!   `response.usage{input_tokens,output_tokens,total_tokens}`，
//!   `status` 作为 finish_reason；
//! - `response.failed` / `error` → 报错。
//!
//! 与 Java 版的已知差异（裁剪口径，见 formatter 模块注释）：
//! - ServerTool（web_search / file_search）、stateful 会话
//!   （`previous_response_id`）、reasoning 回放（assistant 历史 Thinking 块
//!   直接跳过）、`additionalBodyParams`、retry/timeout 配置未复刻。

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
pub use formatter::OpenAiResponsesFormatter;

/// 默认服务地址。
const DEFAULT_BASE_URL: &str = "https://api.openai.com";

/// Responses API 端点路径。
const RESPONSES_PATH: &str = "/v1/responses";

/// OpenAI Responses API 聊天模型。
pub struct OpenAiResponsesChatModel {
    api_key: String,
    base_url: String,
    model: String,
    temperature: Option<f64>,
    max_tokens: Option<u32>,
    /// 上下文窗口（token；0 = 未知）——压缩预算按「窗口 − reserved」计算。
    context_window: usize,
    client: Client,
    /// 消息与参数的格式化器（对应 Java `ResponsesChatFormatter` 一族）。
    formatter: OpenAiResponsesFormatter,
}

impl OpenAiResponsesChatModel {
    /// 创建模型实例。`base_url` 默认为 `https://api.openai.com`。
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
            model: model.into(),
            temperature: None,
            max_tokens: None,
            context_window: 0,
            client: Client::new(),
            formatter: OpenAiResponsesFormatter::new(),
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
    /// 对应 Java `OpenAIResponsesChatModel` 的请求装配：模型只负责传输，
    /// 格式转换由 formatter 完成，`defaults` 为模型实例上的默认选项。
    /// `stream = true` 时置 `stream` 字段（Responses API 流式开关）。
    fn build_request(
        &self,
        request: &ChatRequest,
        stream: bool,
    ) -> Result<ResponsesRequest> {
        let defaults = GenerateOptions {
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            ..Default::default()
        };
        let mut body = ResponsesRequest {
            model: request
                .options
                .model
                .clone()
                .unwrap_or_else(|| self.model.clone()),
            input: self.formatter.format(&request.messages, Some(&request.options))?,
            store: false,
            stream,
            temperature: None,
            max_output_tokens: None,
            tools: Vec::new(),
            tool_choice: None,
        };
        self.formatter
            .apply_options(&mut body, &request.options, Some(&defaults));
        self.formatter.apply_tools(&mut body, &request.options.tools);
        self.formatter
            .apply_tool_choice(&mut body, request.options.tool_choice.as_ref());
        Ok(body)
    }

    fn endpoint(&self) -> String {
        format!("{}{}", self.base_url.trim_end_matches('/'), RESPONSES_PATH)
    }
}

// ---------- Responses API 协议结构体 ----------

/// 请求体（对应 Java `ResponsesRequest`）。
#[derive(Serialize)]
pub struct ResponsesRequest {
    pub model: String,
    pub input: Vec<InputItem>,
    /// Responses API 默认 `true`（服务端保留会话状态）；恒置 `false`
    /// 保持无状态（对齐 Java `store=false`）。
    pub store: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ResponsesTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
}

/// `input[]` 条目（对应 Java `ResponsesInputItem`；`type` 为 tag）。
#[derive(Serialize, Clone)]
#[serde(tag = "type")]
pub enum InputItem {
    /// 消息条目：`content` 为字符串（纯文本）或 parts 数组（含图）。
    #[serde(rename = "message")]
    Message { role: String, content: Value },
    /// assistant 历史里的工具调用。
    #[serde(rename = "function_call")]
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    /// 工具结果回传：`output` 为字符串（纯文本）或 parts 数组（含图）。
    #[serde(rename = "function_call_output")]
    FunctionCallOutput { call_id: String, output: Value },
}

/// 工具定义（对应 Java `ResponsesToolDefinition`；扁平结构，非嵌套 function）。
#[derive(Serialize, Deserialize, Clone)]
pub struct ResponsesTool {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub strict: bool,
}

/// 非流式响应体（对应 Java `ResponsesResponse`）。
#[derive(Deserialize, Default)]
pub struct ResponsesResponse {
    /// `completed` / `incomplete` / `failed`（作为 finish_reason 透传）。
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub output: Vec<OutputItem>,
    #[serde(default)]
    pub usage: Option<ResponsesUsage>,
    #[serde(default)]
    pub error: Option<ResponsesError>,
}

/// `output[]` 条目（对应 Java `ResponsesOutputItem`；`type` 为 tag）。
#[derive(Deserialize, Clone)]
#[serde(tag = "type")]
pub enum OutputItem {
    /// 推理条目：summary 与原始 reasoning 文本（裁剪口径：两者都归 Thinking）。
    #[serde(rename = "reasoning")]
    Reasoning {
        #[serde(default)]
        summary: Vec<ReasoningPart>,
        #[serde(default)]
        content: Vec<ReasoningPart>,
    },
    /// 助手消息条目。
    #[serde(rename = "message")]
    Message {
        #[serde(default)]
        role: String,
        #[serde(default)]
        content: Vec<OutputContentPart>,
    },
    /// 工具调用条目（`arguments` 是 JSON 字符串）。
    #[serde(rename = "function_call")]
    FunctionCall {
        #[serde(default)]
        call_id: String,
        #[serde(default)]
        name: String,
        #[serde(default)]
        arguments: String,
    },
    /// 未识别条目（裁剪口径：跳过）。
    #[serde(other)]
    Other,
}

/// reasoning 条目的文本部分（`summary_text` / `reasoning_text` 共用形状）。
#[derive(Deserialize, Clone)]
pub struct ReasoningPart {
    #[serde(default)]
    pub text: String,
}

/// message 条目的内容部分（`output_text` / `refusal`）。
#[derive(Deserialize, Clone)]
pub struct OutputContentPart {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub refusal: String,
}

/// 用量（对应 Java `ResponsesUsage`；字段名与 Chat Completions 不同）。
#[derive(Deserialize, Default, Clone)]
pub struct ResponsesUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

/// 错误对象（对应 Java `ResponsesError`）。
#[derive(Deserialize, Default, Clone)]
pub struct ResponsesError {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

/// 流式事件（SSE `data:` 行；按 `type` 字段分派）。
#[derive(Deserialize)]
pub struct StreamEvent {
    #[serde(rename = "type")]
    pub kind: String,
    /// delta 类事件的增量文本。
    #[serde(default)]
    pub delta: Option<String>,
    /// `function_call_arguments.delta` 的目标条目 id。
    #[serde(default)]
    pub item_id: Option<String>,
    /// output_item.added / output_item.done 携带的条目。
    #[serde(default)]
    pub item: Option<OutputItem>,
    /// completed / incomplete / failed 携带的完整响应。
    #[serde(default)]
    pub response: Option<ResponsesResponse>,
}

#[async_trait]
impl Model for OpenAiResponsesChatModel {
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

        let response: ResponsesResponse = serde_json::from_str(&text)
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

/// 聚合中的工具调用：`output_item.added` 登记，`function_call_arguments.delta`
/// 按 `item_id` 拼接参数，`output_item.done` 整块产出（断流时兜底 flush）。
#[derive(Default, Clone)]
struct PendingToolCall {
    call_id: String,
    name: String,
    arguments: String,
}

/// 处理单个流式事件，产出零或多个模型事件（对齐 Java
/// `ResponsesStreamHandler` 的分派表）。
fn apply_stream_event(
    event: StreamEvent,
    pending: &mut BTreeMap<String, PendingToolCall>,
) -> Result<Vec<ModelEvent>> {
    let mut events = Vec::new();
    match event.kind.as_str() {
        "response.output_text.delta" => {
            if let Some(text) = event.delta {
                if !text.is_empty() {
                    events.push(ModelEvent::TextDelta { text });
                }
            }
        }
        // summary 与原始 reasoning 文本都归思考增量（裁剪口径）
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            if let Some(text) = event.delta {
                if !text.is_empty() {
                    events.push(ModelEvent::ThinkingDelta { text });
                }
            }
        }
        "response.output_item.added" => {
            if let Some(OutputItem::FunctionCall {
                call_id,
                name,
                arguments,
            }) = event.item
            {
                let item_id = event.item_id.unwrap_or_else(|| call_id.clone());
                pending.insert(
                    item_id,
                    PendingToolCall {
                        call_id,
                        name,
                        arguments,
                    },
                );
            }
        }
        "response.function_call_arguments.delta" => {
            if let (Some(item_id), Some(delta)) = (event.item_id, event.delta) {
                if let Some(entry) = pending.get_mut(&item_id) {
                    entry.arguments.push_str(&delta);
                }
            }
        }
        "response.output_item.done" => {
            if let Some(OutputItem::FunctionCall {
                call_id,
                name,
                arguments,
            }) = event.item
            {
                // done 事件携带完整条目，登记态可移除
                if let Some(item_id) = event.item_id {
                    pending.remove(&item_id);
                }
                let input: Value = if arguments.trim().is_empty() {
                    Value::Object(serde_json::Map::new())
                } else {
                    serde_json::from_str(&arguments)
                        .map_err(|e| Error::Model(format!("invalid tool arguments json: {e}")))?
                };
                events.push(ModelEvent::ToolUse {
                    id: call_id,
                    name,
                    input,
                });
            }
        }
        "response.completed" | "response.incomplete" => {
            // 终态：先兜底 flush 未 done 的聚合条目，再产出 Done
            for (_, call) in std::mem::take(pending) {
                let input: Value = if call.arguments.trim().is_empty() {
                    Value::Object(serde_json::Map::new())
                } else {
                    serde_json::from_str(&call.arguments)
                        .map_err(|e| Error::Model(format!("invalid tool arguments json: {e}")))?
                };
                events.push(ModelEvent::ToolUse {
                    id: call.call_id,
                    name: call.name,
                    input,
                });
            }
            let response = event.response.unwrap_or_default();
            let usage = response.usage.map(|u| Usage {
                prompt_tokens: u.input_tokens,
                completion_tokens: u.output_tokens,
                total_tokens: u.total_tokens,
            });
            // status（completed/incomplete）作为 finish_reason 透传
            events.push(ModelEvent::Done {
                usage,
                finish_reason: response.status,
            });
        }
        "response.failed" => {
            let response = event.response.unwrap_or_default();
            let err = response.error.unwrap_or_default();
            return Err(Error::Model(format!(
                "openai responses error {}: {}",
                err.code.as_deref().unwrap_or("unknown"),
                err.message.as_deref().unwrap_or("unknown")
            )));
        }
        // 其余事件（response.created / output_text.done / 内容部分事件等）跳过
        _ => {}
    }
    Ok(events)
}

/// 逐行解析 SSE 字节流，把模型事件推入 channel，直到流关闭
/// （Responses API 以 `response.completed` 事件收尾，断流时兜底产出 Done）。
async fn pump_sse(
    resp: reqwest::Response,
    tx: mpsc::Sender<Result<ModelEvent>>,
) -> Result<()> {
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    let mut pending: BTreeMap<String, PendingToolCall> = BTreeMap::new();
    let mut finished = false;

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
            if data.is_empty() || data == "[DONE]" {
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

            match apply_stream_event(event, &mut pending) {
                Ok(events) => {
                    for ev in events {
                        if matches!(ev, ModelEvent::Done { .. }) {
                            finished = true;
                        }
                        if tx.send(Ok(ev)).await.is_err() {
                            return Ok(());
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    return Ok(());
                }
            }
        }
        if finished {
            return Ok(());
        }
    }

    // 断流兜底：flush 未完成的聚合条目并产出 Done（无 usage / finish_reason）
    if !finished {
        for (_, call) in std::mem::take(&mut pending) {
            let input: Value = if call.arguments.trim().is_empty() {
                Value::Object(serde_json::Map::new())
            } else {
                match serde_json::from_str(&call.arguments) {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = tx
                            .send(Err(Error::Model(format!(
                                "invalid tool arguments json: {e}"
                            ))))
                            .await;
                        return Ok(());
                    }
                }
            };
            if tx
                .send(Ok(ModelEvent::ToolUse {
                    id: call.call_id,
                    name: call.name,
                    input,
                }))
                .await
                .is_err()
            {
                return Ok(());
            }
        }
        let _ = tx
            .send(Ok(ModelEvent::Done {
                usage: None,
                finish_reason: None,
            }))
            .await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn context_window_defaults_unknown_and_is_settable() {
        assert_eq!(
            OpenAiResponsesChatModel::new("k", "m").context_window_size(),
            0
        );
        assert_eq!(
            OpenAiResponsesChatModel::new("k", "m")
                .with_context_window(128_000)
                .context_window_size(),
            128_000
        );
    }

    #[test]
    fn endpoint_joins_base_and_path() {
        assert_eq!(
            OpenAiResponsesChatModel::new("k", "m").endpoint(),
            "https://api.openai.com/v1/responses"
        );
        assert_eq!(
            OpenAiResponsesChatModel::new("k", "m")
                .with_base_url("https://api.openai.com/")
                .endpoint(),
            "https://api.openai.com/v1/responses"
        );
    }

    /// 请求体：store 恒 false、stream 开关、model 透传。
    #[test]
    fn request_body_store_false_and_stream_flag() {
        let model = OpenAiResponsesChatModel::new("k", "gpt-5");
        let request = ChatRequest {
            messages: vec![agentscope_core::message::Msg::user("user", "你好")],
            options: GenerateOptions::default(),
        };
        let stream_body = model.build_request(&request, true).unwrap();
        let v = serde_json::to_value(&stream_body).unwrap();
        assert_eq!(v["model"], "gpt-5");
        assert_eq!(v["store"], false);
        assert_eq!(v["stream"], true);
        assert_eq!(v["input"][0]["type"], "message");
        assert_eq!(v["input"][0]["role"], "user");

        let plain_body = model.build_request(&request, false).unwrap();
        let v = serde_json::to_value(&plain_body).unwrap();
        assert_eq!(v["store"], false);
        assert!(v.get("stream").is_none());
    }

    /// 文本增量事件 → TextDelta；空 delta 不产出。
    #[test]
    fn stream_text_delta_events() {
        let mut pending = BTreeMap::new();
        let event: StreamEvent = serde_json::from_str(
            r#"{"type":"response.output_text.delta","delta":"你好"}"#,
        )
        .unwrap();
        let events = apply_stream_event(event, &mut pending).unwrap();
        assert!(matches!(&events[0], ModelEvent::TextDelta { text } if text == "你好"));

        let event: StreamEvent =
            serde_json::from_str(r#"{"type":"response.output_text.delta","delta":""}"#).unwrap();
        assert!(apply_stream_event(event, &mut pending).unwrap().is_empty());
    }

    /// 思考增量事件：summary 与原始 reasoning 文本都归 ThinkingDelta。
    #[test]
    fn stream_thinking_delta_events() {
        let mut pending = BTreeMap::new();
        for kind in [
            "response.reasoning_summary_text.delta",
            "response.reasoning_text.delta",
        ] {
            let event: StreamEvent = serde_json::from_str(
                r#"{"type":"PLACEHOLDER","delta":"思考"}"#.replace("PLACEHOLDER", kind).as_str(),
            )
            .unwrap();
            let events = apply_stream_event(event, &mut pending).unwrap();
            assert!(matches!(&events[0], ModelEvent::ThinkingDelta { text } if text == "思考"));
        }
    }

    /// 工具调用流式聚合：added 登记 → arguments.delta 拼接 → done 整块产出。
    #[test]
    fn stream_tool_call_lifecycle() {
        let mut pending = BTreeMap::new();
        let added: StreamEvent = serde_json::from_str(
            r#"{"type":"response.output_item.added","item_id":"item_1","item":{"type":"function_call","call_id":"call_1","name":"add","arguments":""}}"#,
        )
        .unwrap();
        assert!(apply_stream_event(added, &mut pending).unwrap().is_empty());
        assert_eq!(pending.len(), 1);

        let delta: StreamEvent = serde_json::from_str(
            r#"{"type":"response.function_call_arguments.delta","item_id":"item_1","delta":"{\"a\":"}"#,
        )
        .unwrap();
        assert!(apply_stream_event(delta, &mut pending).unwrap().is_empty());

        let done: StreamEvent = serde_json::from_str(
            r#"{"type":"response.output_item.done","item_id":"item_1","item":{"type":"function_call","call_id":"call_1","name":"add","arguments":"{\"a\":1}"}}"#,
        )
        .unwrap();
        let events = apply_stream_event(done, &mut pending).unwrap();
        assert!(matches!(
            &events[0],
            ModelEvent::ToolUse { id, name, input }
                if id == "call_1" && name == "add" && input == &json!({"a": 1})
        ));
        assert!(pending.is_empty());
    }

    /// 终态事件：completed 取 usage 与 status；未 done 的聚合条目兜底 flush。
    #[test]
    fn stream_completed_event_flushes_and_finishes() {
        let mut pending = BTreeMap::new();
        // 模拟只收到 added + arguments.delta、未收到 done 的条目
        let added: StreamEvent = serde_json::from_str(
            r#"{"type":"response.output_item.added","item_id":"item_2","item":{"type":"function_call","call_id":"call_2","name":"sub","arguments":""}}"#,
        )
        .unwrap();
        apply_stream_event(added, &mut pending).unwrap();
        let delta: StreamEvent = serde_json::from_str(
            r#"{"type":"response.function_call_arguments.delta","item_id":"item_2","delta":"{\"b\":2}"}"#,
        )
        .unwrap();
        apply_stream_event(delta, &mut pending).unwrap();

        let completed: StreamEvent = serde_json::from_str(
            r#"{"type":"response.completed","response":{"status":"completed","output":[],"usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}}"#,
        )
        .unwrap();
        let events = apply_stream_event(completed, &mut pending).unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[0],
            ModelEvent::ToolUse { id, name, input }
                if id == "call_2" && name == "sub" && input == &json!({"b": 2})
        ));
        assert!(matches!(
            &events[1],
            ModelEvent::Done { usage, finish_reason } if finish_reason.as_deref() == Some("completed")
                && usage.as_ref().map(|u| u.total_tokens) == Some(15)
        ));
    }

    /// failed 事件 → 报错（带 code 与 message）。
    #[test]
    fn stream_failed_event_errors() {
        let mut pending = BTreeMap::new();
        let failed: StreamEvent = serde_json::from_str(
            r#"{"type":"response.failed","response":{"status":"failed","error":{"code":"rate_limit","message":"too many requests"}}}"#,
        )
        .unwrap();
        let e = apply_stream_event(failed, &mut pending).unwrap_err();
        assert!(e.to_string().contains("rate_limit"));
    }
}
