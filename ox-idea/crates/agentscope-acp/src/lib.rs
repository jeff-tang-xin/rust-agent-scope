//! ACP（Agent Client Protocol）server 适配器：把 agentscope agent 以 ACP v1
//! 协议暴露给 ACP client（OXidea / Zed 等）。
//!
//! 分层：
//! - [`rpc`]：行分帧 JSON-RPC 连接（读分发 / 写串行 / 出站请求 id 配对）；
//! - [`protocol`]：ACP v1 帧构造与入参解析（宽松解析，必要字段校验）；
//! - [`session`]：ACP 会话 ↔ agentscope agent 的桥（事件流映射 + 权限 HITL 映射）。
//!
//! 协议面（v1 子集）：
//! - client → agent：`initialize`、`session/new`、`session/prompt`、`session/cancel`；
//! - agent → client：通知 `session/update`（agent_message_chunk / agent_thought_chunk /
//!   tool_call / tool_call_update），请求 `session/request_permission`。
//!
//! `session/load` 未声明（`loadSession: false`）：会话状态由 agentscope 的
//! state store 自管，进程重启后靠 agentscope 侧恢复，不走 ACP 重放。

pub mod protocol;
pub mod rpc;
pub mod session;

use agentscope_core::Tool;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::{Arc, OnceLock};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};

use crate::rpc::RpcConn;
use crate::session::{AcpSessionHub, PromptOutcome};

/// 单个 ACP 连接的服务循环：读入站消息 → 分发到会话/协议处理 → 写响应。
/// 泛型 IO 便于测试与进程内宿主用内存 duplex 替代 stdio。
/// IDE 宿主按任务工作目录提供的上下文：语义工具 + 记忆锚点。
pub struct OxHostContext {
    /// IDE 语义工具（搜索/符号/补丁/构建/VCS 等），注册进 agent 的 Toolkit。
    pub tools: Vec<Arc<dyn Tool>>,
    /// 记忆工作区锚点（如 `<项目根>/.ox`，与 memory_save 工具同根，跨 worktree
    /// 共享项目记忆）；`None` 时该会话不接记忆管线。
    pub memory_root: Option<std::path::PathBuf>,
}

/// IDE 语义工具与记忆锚点的提供者：按任务工作目录返回上下文。
/// 宿主（IDE）在启动时通过 [`set_ox_tool_provider`] 注册一次；进程内会话
/// 装配 agent 时按 cwd 取用（worktree 目录会命中其所属项目）。
pub type OxToolProvider = Arc<dyn Fn(&Path) -> OxHostContext + Send + Sync>;

static OX_TOOLS: OnceLock<OxToolProvider> = OnceLock::new();

/// 宿主启动时注册 IDE 语义工具的来源。重复注册以第一次为准（进程内单例语义）。
pub fn set_ox_tool_provider(provider: OxToolProvider) {
    let _ = OX_TOOLS.set(provider);
}

/// 装配 agent 时取用：未注册或调用失败都返回空上下文，agent 优雅降级为文件 + shell 工具。
pub(crate) fn ox_context_for(cwd: &Path) -> OxHostContext {
    OX_TOOLS
        .get()
        .map(|provider| provider(cwd))
        .unwrap_or(OxHostContext { tools: Vec::new(), memory_root: None })
}

/// `env` 是模型配置（`AGENTSCOPE_*`），独立进程模式传进程环境，进程内模式由宿主注入。
pub async fn serve<R, W>(reader: R, writer: W, env: Vec<(String, String)>) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let conn = RpcConn::new(writer);
    let hub = AcpSessionHub::new(env);
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(error) => {
                // 无法解析的行：无法取 id，只能记日志丢弃（JSON-RPC 无 id 错误帧意义有限）。
                tracing::warn!(%error, "acp: 丢弃无法解析的入站行");
                continue;
            }
        };
        let id = msg.get("id").cloned().filter(|v| !v.is_null());
        let method = msg.get("method").and_then(Value::as_str).map(str::to_owned);
        match (method, id) {
            // 入站请求 → 分发
            (Some(method), Some(id)) => {
                let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
                match handle_request(&hub, &conn, &method, params).await {
                    Ok(result) => conn.respond(id, result).await,
                    Err(error) => conn.respond_error(id, error.code, &error.message).await,
                }
            }
            // 通知（无 id）：目前只有 session/cancel，MVP 记录后忽略（见 session 模块 TODO）。
            (Some(method), None) => {
                tracing::debug!(%method, "acp: 收到通知（当前协议面不处理）");
            }
            // 出站请求的响应（有 id 无 method）→ 配对回等待者
            (None, Some(id)) => conn.resolve_pending(id, msg).await,
            (None, None) => tracing::warn!("acp: 丢弃既无 method 也无 id 的帧"),
        }
    }
    Ok(())
}

/// 入站请求处理结果错误：JSON-RPC error code + message。
#[derive(Debug)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(-32602, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(-32603, message)
    }

    pub fn method_not_found(method: &str) -> Self {
        Self::new(-32601, format!("method not found: {method}"))
    }
}

/// 分发一个入站 ACP 请求。
async fn handle_request(
    hub: &AcpSessionHub,
    conn: &RpcConn<impl AsyncWrite + Unpin + Send + 'static>,
    method: &str,
    params: Value,
) -> Result<Value, RpcError> {
    match method {
        "initialize" => Ok(protocol::initialize_result()),
        "authenticate" => {
            // 模型凭证来自环境变量，无需 ACP 认证握手；authMethods 为空时
            // client 本不该调用，回空对象兜底。
            Ok(json!({}))
        }
        "session/new" => hub.create_session(params).await,
        "session/prompt" => hub.prompt(params, conn).await,
        "session/load" => Err(RpcError::new(
            -32601,
            "session/load not supported (loadSession: false)",
        )),
        other => Err(RpcError::method_not_found(other)),
    }
}

/// prompt 的终局转 ACP 响应：正常结束给 stopReason，失败转 JSON-RPC error。
pub fn prompt_response(outcome: PromptOutcome) -> Result<Value, RpcError> {
    match outcome {
        PromptOutcome::EndTurn => Ok(json!({ "stopReason": "end_turn" })),
        PromptOutcome::Refusal => Ok(json!({ "stopReason": "refusal" })),
        PromptOutcome::Cancelled => Ok(json!({ "stopReason": "cancelled" })),
        PromptOutcome::Error(message) => Err(RpcError::internal(message)),
    }
}
