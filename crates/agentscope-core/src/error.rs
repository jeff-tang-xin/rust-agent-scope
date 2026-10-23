//! 错误类型（对应 Java 版 `io.agentscope.core.exception`）。

use thiserror::Error;

/// agentscope-core 统一错误类型。
#[derive(Debug, Error)]
pub enum Error {
    /// 模型调用失败（网络、HTTP 状态码、响应解析等）。
    #[error("model error: {0}")]
    Model(String),

    /// 工具执行失败。
    #[error("tool error: {0}")]
    Tool(String),

    /// 工具请求挂起，等待外部执行（对应 Java `ToolSuspendException`）。
    /// 挂起的工具调用以 `Suspended` 状态结果回传，由 HITL / 外部执行回路提供真实结果。
    #[error("tool suspended: {0}")]
    ToolSuspended(String),

    /// ReAct 循环超过最大迭代次数仍未产出最终回复。
    #[error("max iterations ({0}) exceeded without a final answer")]
    MaxIterations(usize),

    /// 消息或参数格式不合法。
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// JSON 序列化/反序列化错误。
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// IO 错误。
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// 状态存储失败（对应 Java `AgentStateStore` 抛出的异常）。
    #[error("state error: {0}")]
    State(#[from] crate::state::StateError),

    /// 会话日志失败（对应 Java `SessionLogException`）。
    ///
    /// 持久化失败必须向上传播——绝不能降级成「模型/工具成功」。
    #[error("session error: {0}")]
    Session(#[from] crate::session::SessionError),

    /// 系统正在优雅关闭，请求被拒绝或中断（对应 Java `AgentShuttingDownException`）。
    /// 调用方应稍后重试。
    #[error("operation interrupted due to system shutting down, please retry")]
    AgentShuttingDown,

    /// 其他未归类错误。
    #[error("{0}")]
    Other(String),
}

/// 统一 Result 别名。
pub type Result<T> = std::result::Result<T, Error>;
