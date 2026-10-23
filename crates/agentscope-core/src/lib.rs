//! # agentscope-core
//!
//! 用 Rust 仿写 AgentScope Java 2.0 的核心抽象层（对应 Java 版 `agentscope-core` 模块）。
//!
//! 核心概念对照：
//! - [`message::Msg`]      ↔ `io.agentscope.core.message`（消息与内容块）
//! - [`model::Model`]      ↔ `io.agentscope.core.model`（模型抽象，提供商模块化）
//! - [`formatter::Formatter`] ↔ `io.agentscope.core.formatter`（消息 ↔ 提供商格式的转换层）
//! - [`tool::Toolkit`]     ↔ Toolkit（工具注册、分组、schema 生成与调用分发）
//! - [`tool::ToolGroup`]   ↔ ToolGroup / ToolGroupScope（工具分组与激活状态）
//! - [`tool::ToolContext`] ↔ ToolExecutionContext / ToolEmitter（调用上下文与进度推送）
//! - [`tool::ToolCallInterceptor`] ↔ ToolCallInterceptor（per-call 参数级拦截链）
//! - [`memory::Memory`]    ↔ Memory（对话记忆）
//! - [`agent::ReActAgent`] ↔ `io.agentscope.core.ReActAgent`（reasoning-acting 循环）
//! - [`event::AgentEvent`] ↔ `io.agentscope.core.event`（类型化事件流）
//! - [`middleware::Middleware`] ↔ `io.agentscope.core.middleware.MiddlewareBase`
//!   （6 个扩展点：onAgent / onReasoning / onActing / onModelCall / onSystemPrompt /
//!   onAgentStateReady，已接入执行循环）
//! - [`context::RuntimeContext`] ↔ `io.agentscope.core.RuntimeContext`（每次调用的运行时上下文）
//! - [`state::AgentState`] ↔ `io.agentscope.core.state.AgentState`（会话单一真相）
//! - [`state::AgentStateStore`] ↔ `AgentStateStore`（内存 / JSON 文件两种后端，含 CAS）
//! - [`session::SessionLog`] ↔ `io.agentscope.core.session.SessionLog`（只追加事件日志 + 围栏写者）
//! - [`session::SessionEvent`] ↔ `SessionEvent`（不可变事实 + 内置类型表 + 扩展 codec 登记）
//! - [`permission`]        ↔ 工具调用门控（已接入工具执行路径）+ 权限模式/规则状态

pub mod agent;
pub mod context;
pub mod credential;
pub mod error;
pub mod event;
pub mod formatter;
pub mod interruption;
pub mod memory;
pub mod message;
pub mod middleware;
pub mod model;
pub mod observation;
pub mod permission;
pub mod session;
pub mod shutdown;
pub mod state;
pub mod tool;

pub use agent::{Agent, ReActAgent};
pub use context::RuntimeContext;
pub use credential::{Credential, ModelCard};
pub use error::{Error, Result};
pub use event::AgentEvent;
pub use formatter::{Formatter, JsonSchema, ResponseFormat};
pub use interruption::{InterruptContext, InterruptControl, InterruptSource};
pub use memory::{InMemoryMemory, Memory};
pub use message::{ContentBlock, Msg, Role};
pub use model::{
    ChatRequest, ChatResponse, GenerateOptions, Model, ModelEvent, ToolChoice, ToolSchema, Usage,
};
pub use observation::{
    ActionObservation, ActionObservationStatus, ActionObserver, CONTEXT_KEY as OBSERVER_CONTEXT_KEY,
    EVENT_NAME as OBSERVER_EVENT_NAME, status_of as observation_status_of,
};
pub use shutdown::{
    GracefulShutdownConfig, GracefulShutdownManager, GracefulShutdownMiddleware,
    PartialReasoningPolicy, ShutdownState, ShutdownStateSaver, REQUEST_ID_KEY,
};
pub use session::{
    load_from_file, restore_into, save_to_file, AtomicSessionStorage, Head, InMemorySessionLogStore,
    JournalSessionLog, MemorySessionStorage, SessionError, SessionEvent, SessionEventCodec,
    SessionEventCodecRegistry, SessionEventTypes, SessionHeader, SessionHistoryMode, SessionKey,
    SessionLog, SessionLogStore, SessionSnapshot, StorageValue, Writer,
};
pub use state::{
    AgentState, AgentStateStore, AgentStateStoreExt, InMemoryAgentStateStore,
    JsonFileAgentStateStore, PlanModeContextState, Task, TaskContextState, TaskRequirement,
    TaskRequirementKind, TaskRequirementStatus, TaskScope, TaskState, TaskVerification,
    ToolContextState, VersionedState,
};
pub use tool::{
    Tool, ToolActivation, ToolCallInterceptor, ToolContext, ToolGroup, ToolGroupScope,
    ToolInvocation, ToolOutput, Toolkit,
};
