//! 行分帧 JSON-RPC 2.0 连接：ACP 的传输层（对齐 OpenIDEA `Framing::Lines`）。
//!
//! - 每行一个 JSON 对象（UTF-8，`\n` 结尾）；
//! - 入站请求（method+id）由上层 [`serve`](crate::serve) 分发；
//! - 出站请求（我们发 `session/request_permission`）登记 pending map，
//!   响应到达时按 id 配对唤醒；
//! - 写出串行化（Mutex），避免通知/响应/请求交错撕裂行。

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;

use serde_json::{json, Value};
use tokio::io::AsyncWrite;
use tokio::sync::oneshot;

/// 一条 ACP 连接的写出端 + 出站请求配对表。
pub struct RpcConn<W> {
    writer: tokio::sync::Mutex<W>,
    pending: Mutex<HashMap<i64, oneshot::Sender<Result<Value, (i64, String)>>>>,
    next_id: AtomicI64,
}

impl<W: AsyncWrite + Unpin + Send + 'static> RpcConn<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer: tokio::sync::Mutex::new(writer),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicI64::new(1),
        }
    }

    /// 写一行 JSON（内部统一出口）。
    async fn send_line(&self, frame: &Value) -> std::io::Result<()> {
        use tokio::io::AsyncWriteExt;
        let mut writer = self.writer.lock().await;
        let mut line = serde_json::to_string(frame).unwrap_or_else(|_| "{}".into());
        line.push('\n');
        writer.write_all(line.as_bytes()).await?;
        writer.flush().await
    }

    /// 回复入站请求（成功）。
    pub async fn respond(&self, id: Value, result: Value) {
        let frame = json!({ "jsonrpc": "2.0", "id": id, "result": result });
        if let Err(error) = self.send_line(&frame).await {
            tracing::warn!(%error, "acp: 写响应失败");
        }
    }

    /// 回复入站请求（失败）。
    pub async fn respond_error(&self, id: Value, code: i64, message: &str) {
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        });
        if let Err(error) = self.send_line(&frame).await {
            tracing::warn!(%error, "acp: 写错误响应失败");
        }
    }

    /// 发通知（无 id，无响应）。
    pub async fn notify(&self, method: &str, params: Value) {
        let frame = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        if let Err(error) = self.send_line(&frame).await {
            tracing::warn!(%error, "acp: 写通知失败");
        }
    }

    /// 发出站请求并等待响应（`session/request_permission` 等）。
    /// 连接对端回错误帧时转 `Err((code, message))`。
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, (i64, String)> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let frame = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(error) = self.send_line(&frame).await {
            self.pending.lock().unwrap().remove(&id);
            return Err((-32603, format!("写出站请求失败: {error}")));
        }
        match rx.await {
            Ok(result) => result,
            // 发送方被移除（连接关闭）——不可能走到，兜底成内部错误。
            Err(_) => Err((-32603, "出站请求的配对发送端被丢弃".into())),
        }
    }

    /// 读循环侧：把一条出站响应帧配对给等待者。未知 id 静默丢弃（记日志）。
    pub async fn resolve_pending(&self, id: Value, msg: Value) {
        let numeric_id = match id.as_i64() {
            Some(v) => v,
            None => {
                tracing::warn!(?id, "acp: 响应 id 非整数，无法配对");
                return;
            }
        };
        let sender = self.pending.lock().unwrap().remove(&numeric_id);
        match sender {
            Some(sender) => {
                let result = match msg.get("error") {
                    Some(error) => Err((
                        error.get("code").and_then(Value::as_i64).unwrap_or(-32603),
                        error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown error")
                            .to_owned(),
                    )),
                    None => Ok(msg.get("result").cloned().unwrap_or_else(|| json!({}))),
                };
                let _ = sender.send(result);
            }
            None => tracing::warn!(id = numeric_id, "acp: 收到未知 id 的响应帧"),
        }
    }
}
