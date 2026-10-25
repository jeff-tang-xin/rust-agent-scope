//! 模型抽象（对应 Java 版 `io.agentscope.core.model`）。
//!
//! Java 2.0 将模型提供商模块化为独立扩展（openai / anthropic / gemini / ollama / dashscope），
//! Rust 版同样只在此处定义契约，具体提供商实现在独立 crate
//! （如 `agentscope-model-openai`）中提供。

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::error::{Error, Result};
use crate::message::{ContentBlock, Msg};

/// 工具的 JSON Schema 描述（传给模型的 function 定义）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    /// JSON Schema 格式的参数定义。
    pub parameters: serde_json::Value,
}

/// 工具选择策略（对应 Java `ToolChoice` 的 sealed 接口四个实现）。
///
/// Java 用 `Auto` / `None` / `Required` / `Specific` 四个 record 加 `instanceof` 分派，
/// Rust 用枚举 + `match` 穷尽分派表达同一语义。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    /// 由模型自行决定（默认）。
    Auto,
    /// 禁止调用工具。
    None,
    /// 必须调用某个工具。
    Required,
    /// 指定必须调用某个具体工具。
    Specific { tool_name: String },
}

/// 生成选项。
#[derive(Debug, Clone, Default)]
pub struct GenerateOptions {
    /// 覆盖模型提供商实例上的默认模型名。
    pub model: Option<String>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
    /// 本次调用可用的工具列表。
    pub tools: Vec<ToolSchema>,
    /// 工具选择策略（为空时交由提供商默认，通常等价 `Auto`）。
    pub tool_choice: Option<ToolChoice>,
    /// 响应格式（结构化输出）；为空表示纯文本。
    pub response_format: Option<crate::formatter::ResponseFormat>,
}

/// 一次模型调用的请求：完整消息列表 + 生成选项。
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub messages: Vec<Msg>,
    pub options: GenerateOptions,
}

/// token 用量统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

/// 一次模型调用的响应：内容块（文本 / 工具调用）+ 用量 + 结束原因。
#[derive(Debug, Clone, Default)]
pub struct ChatResponse {
    pub content: Vec<ContentBlock>,
    pub usage: Option<Usage>,
    /// 模型停止生成的原因（如 `stop` / `tool_calls` / `length`），提供商未返回时为 None。
    pub finish_reason: Option<String>,
}

impl ChatResponse {
    /// 纯文本响应的便捷构造器。
    pub fn from_text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::Text { text: text.into() }],
            ..Default::default()
        }
    }
}

/// 流式模型事件：文本增量、思考增量、聚合完成的工具调用、结束元信息。
#[derive(Debug, Clone)]
pub enum ModelEvent {
    /// 文本增量分片。
    TextDelta { text: String },
    /// 思考 / 推理增量分片（推理模型输出，如 DeepSeek `reasoning_content`）。
    ThinkingDelta { text: String },
    /// 聚合完成的工具调用（实现方负责把协议层的分片拼完整再产出）。
    ToolUse { id: String, name: String, input: serde_json::Value },
    /// 流结束元信息（最后发出；对应 Java 版 chunk 携带的 usage / finish_reason）。
    Done {
        usage: Option<Usage>,
        finish_reason: Option<String>,
    },
}

/// 模型抽象。实现方负责把 [`ChatRequest`] 翻译为具体提供商协议并解析回 [`ChatResponse`]。
#[async_trait::async_trait]
pub trait Model: Send + Sync {
    /// 模型名（对应 Java `getModelName`，用于日志与事件记录）。
    fn model_name(&self) -> &str {
        "model"
    }

    /// 非流式调用：一次性返回完整响应。
    async fn call(&self, request: ChatRequest) -> Result<ChatResponse>;

    /// 流式调用：通过 channel 推送 [`ModelEvent`]，channel 关闭即调用结束。
    ///
    /// 默认实现退化为非流式：把 [`Model::call`] 的结果包装成单事件流，
    /// 因此未实现流式的模型提供商也能直接接入流式 agent。
    async fn call_stream(
        &self,
        request: ChatRequest,
    ) -> Result<mpsc::Receiver<Result<ModelEvent>>> {
        let (tx, rx) = mpsc::channel(16);
        let response = self.call(request).await?;
        tokio::spawn(async move {
            for block in response.content {
                let event = match block {
                    ContentBlock::Text { text } => ModelEvent::TextDelta { text },
                    ContentBlock::Thinking { thinking } => ModelEvent::ThinkingDelta { text: thinking },
                    ContentBlock::ToolUse { id, name, input, .. } => ModelEvent::ToolUse { id, name, input },
                    other => {
                        let _ = tx
                            .send(Err(Error::Other(format!(
                                "unsupported content block in stream: {other:?}"
                            ))))
                            .await;
                        break;
                    }
                };
                if tx.send(Ok(event)).await.is_err() {
                    break;
                }
            }
            let _ = tx
                .send(Ok(ModelEvent::Done {
                    usage: response.usage,
                    finish_reason: response.finish_reason,
                }))
                .await;
        });
        Ok(rx)
    }
}
