//! 模型调用的记录包装（对应 Java `SessionModels` / `SessionModelPolicy`）。
//!
//! [`RecordedModel`] 把真正的模型包一层：每次调用写 `request/prepared` +
//! `model/dispatch`，流式分片写 `model/chunk`，结束写 `model/end`（含状态与用量）。
//! 记录的是**最终适配器边界**——辅助调用、回退调用与取消都会各自成对记录。
//!
//! `state/checkpoint{before_model}` 仍由 agent 循环在调用前拍（对应 Java
//! `prepareModel` 的快照部分），这里不重复写。

use std::sync::Arc;

use serde_json::json;
use tokio::sync::mpsc;

use crate::context::RuntimeContext;
use crate::error::Result;
use crate::model::{ChatRequest, ChatResponse, Model, ModelEvent};
use crate::session::recorder::SessionRecorder;

/// 可选的准入 / 计费策略（对应 Java `SessionModelPolicy`）。
///
/// 在记录边界之外再挂一层托管方钩子：调用前准入、调用后记账。
/// 失败不阻断调用（策略是观测面，不是门）。
pub trait SessionModelPolicy: Send + Sync {
    /// 模型调用发出前。
    fn before_call(&self, _call_id: &str, _model: &str, _ctx: &RuntimeContext) {}

    /// 模型调用结束后（`status`：`completed` / `failed` / `cancelled`）。
    fn after_call(
        &self,
        _call_id: &str,
        _model: &str,
        _status: &str,
        _usage: Option<&crate::model::Usage>,
        _ctx: &RuntimeContext,
    ) {
    }
}

/// 记录模型：把每次调用写成会话事件（对应 Java `SessionModels.Recorded`）。
pub struct RecordedModel {
    delegate: Arc<dyn Model>,
    recorder: Arc<SessionRecorder>,
    policy: Option<Arc<dyn SessionModelPolicy>>,
    runtime: RuntimeContext,
    purpose: String,
}

impl RecordedModel {
    /// 包装模型；`recorder` 为 `None` 时原样返回（无日志不记录）。
    pub fn wrap(
        model: Arc<dyn Model>,
        recorder: Option<Arc<SessionRecorder>>,
        policy: Option<Arc<dyn SessionModelPolicy>>,
        runtime: RuntimeContext,
        purpose: impl Into<String>,
    ) -> Arc<dyn Model> {
        match recorder {
            Some(recorder) => Arc::new(Self {
                delegate: model,
                recorder,
                policy,
                runtime,
                purpose: purpose.into(),
            }),
            None => model,
        }
    }

    fn prepare(&self, call_id: &str) -> Result<()> {
        let model = self.delegate.model_name();
        self.recorder.append(
            "request/prepared",
            json!({
                "modelCallId": call_id,
                "purpose": self.purpose,
                "model": model,
            }),
        )?;
        self.recorder.record_now(
            "model/dispatch",
            json!({
                "modelCallId": call_id,
                "purpose": self.purpose,
                "model": model,
            }),
        )?;
        Ok(())
    }

    /// 结束事件 + 策略回调（对应 Java `end`）。
    fn end(&self, call_id: &str, status: &str, response: Option<&ChatResponse>) {
        let mut payload = json!({
            "modelCallId": call_id,
            "status": status,
            "model": self.delegate.model_name(),
            "purpose": self.purpose,
        });
        if let Some(response) = response {
            if let Some(usage) = &response.usage {
                payload["usage"] = json!({
                    "promptTokens": usage.prompt_tokens,
                    "completionTokens": usage.completion_tokens,
                    "totalTokens": usage.total_tokens,
                });
            }
            if let Some(finish_reason) = &response.finish_reason {
                payload["finishReason"] = json!(finish_reason);
            }
        }
        // 记录失败不该毁掉调用本身：尽力而为
        let _ = self.recorder.record_now("model/end", payload);
        if let Some(policy) = &self.policy {
            policy.after_call(
                call_id,
                self.delegate.model_name(),
                status,
                response.and_then(|response| response.usage.as_ref()),
                &self.runtime,
            );
        }
    }
}

#[async_trait::async_trait]
impl Model for RecordedModel {
    fn model_name(&self) -> &str {
        self.delegate.model_name()
    }

    async fn call(&self, request: ChatRequest) -> Result<ChatResponse> {
        let call_id = uuid::Uuid::new_v4().to_string();
        if let Some(policy) = &self.policy {
            policy.before_call(&call_id, self.delegate.model_name(), &self.runtime);
        }
        self.prepare(&call_id)?;
        match self.delegate.call(request).await {
            Ok(response) => {
                self.end(&call_id, "completed", Some(&response));
                Ok(response)
            }
            Err(error) => {
                self.end(&call_id, "failed", None);
                Err(error)
            }
        }
    }

    async fn call_stream(
        &self,
        request: ChatRequest,
    ) -> Result<mpsc::Receiver<Result<ModelEvent>>> {
        let call_id = uuid::Uuid::new_v4().to_string();
        if let Some(policy) = &self.policy {
            policy.before_call(&call_id, self.delegate.model_name(), &self.runtime);
        }
        self.prepare(&call_id)?;
        let mut rx = self.delegate.call_stream(request).await?;
        let (tx, out) = mpsc::channel(16);
        let recorder = self.recorder.clone();
        let purpose = self.purpose.clone();
        let model = self.delegate.model_name().to_string();
        let policy = self.policy.clone();
        let runtime = self.runtime.clone();
        let call_id_stream = call_id.clone();
        tokio::spawn(async move {
            let mut last_usage = None;
            let mut last_finish: Option<String> = None;
            while let Some(event) = rx.recv().await {
                let mut failed = false;
                match &event {
                    Ok(ModelEvent::Done {
                        usage,
                        finish_reason,
                    }) => {
                        last_usage = *usage;
                        last_finish = finish_reason.clone();
                    }
                    Ok(other) => {
                        // 分片逐条落日志（尽力而为：日志写失败不中断流）
                        let chunk = match other {
                            ModelEvent::TextDelta { text } => json!({"text": text}),
                            ModelEvent::ThinkingDelta { text } => json!({"thinking": text}),
                            ModelEvent::ToolUse { id, name, input } => {
                                json!({"toolUse": {"id": id, "name": name, "input": input}})
                            }
                            other => json!({"event": format!("{other:?}")}),
                        };
                        let _ = recorder.append(
                            "model/chunk",
                            json!({
                                "modelCallId": call_id_stream,
                                "purpose": purpose,
                                "chunk": chunk,
                            }),
                        );
                    }
                    Err(error) => {
                        failed = true;
                        let _ = recorder.record_now(
                            "model/end",
                            json!({
                                "modelCallId": call_id_stream,
                                "status": "failed",
                                "model": model,
                                "purpose": purpose,
                                "errorType": format!("{error}"),
                            }),
                        );
                        if let Some(policy) = &policy {
                            policy.after_call(&call_id_stream, &model, "failed", None, &runtime);
                        }
                    }
                }
                if tx.send(event).await.is_err() {
                    return;
                }
                if failed {
                    return;
                }
            }
            let _ = recorder.record_now(
                "model/end",
                json!({
                    "modelCallId": call_id_stream,
                    "status": "completed",
                    "model": model,
                    "purpose": purpose,
                    "usage": last_usage.map(|usage| json!({
                        "promptTokens": usage.prompt_tokens,
                        "completionTokens": usage.completion_tokens,
                        "totalTokens": usage.total_tokens,
                    })),
                    "finishReason": last_finish,
                }),
            );
            if let Some(policy) = &policy {
                policy.after_call(
                    &call_id_stream,
                    &model,
                    "completed",
                    last_usage.as_ref(),
                    &runtime,
                );
            }
            drop(tx);
        });
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ChatResponse, Usage};
    use crate::session::store::{InMemorySessionLogStore, SessionLogStore};
    use crate::session::{SessionKey, SessionProjection};
    use std::sync::Mutex;

    struct CountingPolicy {
        calls: Mutex<Vec<String>>,
    }

    impl CountingPolicy {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
            })
        }
    }

    impl SessionModelPolicy for CountingPolicy {
        fn before_call(&self, call_id: &str, _model: &str, _ctx: &RuntimeContext) {
            self.calls.lock().unwrap().push(format!("before:{call_id}"));
        }

        fn after_call(
            &self,
            call_id: &str,
            _model: &str,
            status: &str,
            _usage: Option<&crate::model::Usage>,
            _ctx: &RuntimeContext,
        ) {
            self.calls
                .lock()
                .unwrap()
                .push(format!("after:{call_id}:{status}"));
        }
    }

    struct EchoModel;

    #[async_trait::async_trait]
    impl Model for EchoModel {
        fn model_name(&self) -> &str {
            "echo-1"
        }

        async fn call(&self, _request: ChatRequest) -> Result<ChatResponse> {
            Ok(ChatResponse {
                content: vec![crate::message::ContentBlock::Text {
                    text: "ok".into(),
                }],
                usage: Some(Usage {
                    prompt_tokens: 3,
                    completion_tokens: 1,
                    total_tokens: 4,
                }),
                finish_reason: Some("stop".into()),
            })
        }
    }

    fn recorder() -> (Arc<dyn crate::session::SessionLog>, Arc<SessionRecorder>) {
        let log: Arc<dyn crate::session::SessionLog> = InMemorySessionLogStore::new()
            .open(&SessionKey::new(Some("u"), "a", "s").unwrap(), None)
            .unwrap();
        let recorder = Arc::new(SessionRecorder::new(log.clone(), None, "r1").unwrap());
        (log, recorder)
    }

    fn kinds(log: &dyn crate::session::SessionLog) -> Vec<String> {
        SessionProjection::read(log)
            .unwrap();
        let head = log.head().unwrap().seq;
        log.scan(0, head)
            .unwrap()
            .into_iter()
            .map(|event| event.kind)
            .collect()
    }

    #[tokio::test]
    async fn call_records_prepared_dispatch_and_end() {
        let (log, recorder) = recorder();
        let policy = CountingPolicy::new();
        let model = RecordedModel::wrap(
            Arc::new(EchoModel),
            Some(recorder.clone()),
            Some(policy.clone()),
            RuntimeContext::new(),
            "reasoning",
        );
        let response = model
            .call(ChatRequest {
                messages: vec![],
                options: Default::default(),
            })
            .await
            .unwrap();
        assert_eq!(response.usage.as_ref().unwrap().total_tokens, 4);

        let events = kinds(log.as_ref());
        assert!(events.contains(&"request/prepared".to_string()), "{events:?}");
        assert!(events.contains(&"model/dispatch".to_string()), "{events:?}");
        assert!(events.contains(&"model/end".to_string()), "{events:?}");
        let calls = policy.calls.lock().unwrap();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(calls[0].starts_with("before:"), "{calls:?}");
        assert!(calls[1].starts_with("after:") && calls[1].ends_with(":completed"), "{calls:?}");
        assert_eq!(calls[0].split(':').nth(1), calls[1].split(':').nth(1));
    }

    #[tokio::test]
    async fn stream_records_chunks_and_end() {
        let (log, recorder) = recorder();
        let model = RecordedModel::wrap(
            Arc::new(EchoModel),
            Some(recorder.clone()),
            None,
            RuntimeContext::new(),
            "reasoning",
        );
        let mut rx = model
            .call_stream(ChatRequest {
                messages: vec![],
                options: Default::default(),
            })
            .await
            .unwrap();
        while let Some(event) = rx.recv().await {
            event.unwrap();
        }

        let events = kinds(log.as_ref());
        assert!(events.contains(&"model/chunk".to_string()), "{events:?}");
        assert!(events.contains(&"model/end".to_string()), "{events:?}");
    }

    #[tokio::test]
    async fn wrap_without_recorder_passes_through() {
        let model = RecordedModel::wrap(
            Arc::new(EchoModel),
            None,
            None,
            RuntimeContext::new(),
            "reasoning",
        );
        let response = model
            .call(ChatRequest {
                messages: vec![],
                options: Default::default(),
            })
            .await
            .unwrap();
        assert_eq!(model.model_name(), "echo-1");
        assert!(response.finish_reason.is_some());
    }
}
