//! Agent 抽象与 ReAct 实现（对应 Java 版 `io.agentscope.core.agent` 与 `ReActAgent`）。
//!
//! Java 2.0 的双层架构中，`agentscope-core` 层提供 `ReActAgent`（reasoning-acting
//! 循环），`agentscope-harness` 层在其上封装 Workspace / Session / Subagent 等工程能力。
//! Rust 版保持同样的分层：本 crate 只做 ReAct 循环，工程化封装留待 harness crate。

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::context::RuntimeContext;
use crate::error::{Error, Result};
use crate::event::{AgentEvent, AgentEventEmitter, AgentEventKind, ConfirmResult};
use crate::memory::Memory;
use crate::message::{ContentBlock, GenerateReason, Msg, ToolCallState};
use crate::session::execution::SessionExecution;
use crate::session::journal::SessionLog;
use crate::session::projection::SessionProjection;
use crate::session::recorder::SessionRecorder;
use crate::session::store::SessionLogStore;
use crate::session::projection::SessionSnapshot;
use crate::session::{SessionError, SessionHistoryMode, SessionKey};
use crate::state::{AgentState, AgentStateStore, AgentStateStoreExt};
use crate::middleware::{
    ActingInput, AgentInput, EventFilter, Middleware, MiddlewareChain, ModelCallInput,
    ReasoningInput, ToolCallSpec,
};
use crate::model::{ChatRequest, GenerateOptions, Model, ModelEvent};
use crate::permission::{
    PermissionChecker, PermissionContextState, PermissionDecision, PermissionEngine,
};
use crate::tool::{ToolContext, ToolOutput, Toolkit};

/// Agent 抽象：统一执行入口（对应 Java 版统一的 `call()` / `streamEvents()`）。
#[async_trait::async_trait]
pub trait Agent: Send + Sync {
    /// 执行完整 ReAct 循环，返回最终回复消息。
    async fn call(&self, msg: Msg) -> Result<Msg>;

    /// 执行完整 ReAct 循环，通过 channel 实时推送事件；
    /// 最后一条事件必为 [`AgentEventKind::AgentResult`]（出错时 channel 提前关闭）。
    async fn stream_events(&self, msg: Msg) -> Result<mpsc::Receiver<AgentEvent>>;
}

/// ReAct Agent：reasoning（模型推理）与 acting（工具执行）交替循环，
/// 直到模型产出不含工具调用的最终回复。
/// 迭代边界异步钩子（对应 Java `HarnessRuntimeMiddleware` 每轮 `onReasoning` 的
/// 检查点，如 `CompactionMiddleware` 的上下文压缩）。
///
/// 中间件钩子是同步的，做不了需要 await 的检查（LLM 调用 / 持久化 IO）；需要
/// 每轮迭代边界的异步逻辑时用本钩子。参数：
/// - `&RuntimeContext`：本次调用的运行时身份（session/user）；
/// - `&Arc<Mutex<AgentState>>`：本次调用的活状态（对话真相），可读可写；
/// - `forced: bool`：`true` 表示模型刚报上下文溢出（见
///   [`is_context_overflow_error`]），钩子应做最激进的压缩（对齐 Java
///   `overflowRecovery` 的 triggerMessages=1/keepMessages=1/keepTokens=0）。
///
/// 返回 `Err` 会终止本次调用；best-effort 型钩子（如压缩）应自行降级返回 `Ok`。
pub type IterationHook = Arc<
    dyn for<'a> Fn(&'a RuntimeContext, &'a Arc<Mutex<AgentState>>, bool)
        -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>
        + Send
        + Sync,
>;

/// 判断错误是否为「上下文溢出」类（对齐 Java
/// `CompactionMiddleware.isContextOverflowError` 的消息模式匹配）。
pub fn is_context_overflow_error(error: &Error) -> bool {
    let message = error.to_string().to_lowercase();
    [
        "context_length_exceeded",
        "context length",
        "maximum context",
        "token limit",
        "too many tokens",
        "exceeds the model's maximum",
        "reduce the length",
    ]
    .iter()
    .any(|pattern| message.contains(pattern))
}

#[derive(Clone)]
pub struct ReActAgent {
    name: String,
    sys_prompt: String,
    model: Arc<dyn Model>,
    toolkit: Arc<Toolkit>,
    /// 1.x 兼容的历史镜像。**不是对话真相**：真相永远在 [`AgentState`]。
    ///
    /// 配置后每次调用结束会把最终历史同步进去，只为让仍按旧习惯读 `memory`
    /// 的代码不至于失效；新代码请用 [`ReActAgent::state`]。
    memory: Option<Arc<dyn Memory>>,
    /// 状态存储（对应 Java `stateStore`）：`LEGACY` 模式下的持久化后端。
    state_store: Option<Arc<dyn AgentStateStore>>,
    /// 会话日志存储（对应 Java `sessionLogStore`）：`EVENT_LOG` 模式下的事实来源。
    session_log_store: Option<Arc<dyn SessionLogStore>>,
    /// 历史来源二选一（对应 Java `SessionHistoryMode`），不允许混用。
    history_mode: SessionHistoryMode,
    /// 调用方没给 session id 时的兜底（对应 Java `defaultSessionId`）。
    default_session_id: Option<String>,
    /// 槽位缓存：`(userId, sessionId)` → 该槽位的活状态与互斥门。
    ///
    /// 与 Java 的 `stateCache` 同样的用途：同一槽位的调用共享一份可变状态并**排队执行**
    /// （同一会话的两次并发运行会互相踩状态与日志租约，必须串行）；但**不能**把它
    /// 当权威来源——有持久层时每次调用都会重新加载。
    slots: Arc<Mutex<HashMap<String, SlotEntry>>>,
    /// 运行中插入的转向消息（steer）：`sessionId → 待注入消息`。
    ///
    /// 对应 Java `AgentSession.steer`：调用方在 turn 运行中投递，ReAct 循环在
    /// **下一个步边界**注入对话真相（beforeStep 语义），而不是等本次调用结束。
    /// Legacy 模式（无 execution 链路）也走这里；调用结束后未被消费的残留
    /// 由调用方经 [`Self::take_steers`] 取走做结算续轮。
    steers: Arc<Mutex<HashMap<String, Vec<Msg>>>>,
    /// 中间件链（构造时按 `order()` 排好序，不可变）
    middlewares: MiddlewareChain,
    permission: Arc<dyn PermissionChecker>,
    /// 新会话的初始权限上下文（见 builder `permission_context`）。
    permission_context: Option<PermissionContextState>,
    max_iters: usize,
    /// 迭代边界异步钩子（见 [`IterationHook`]；None = 无）。
    iteration_hook: Option<IterationHook>,
}

// trait 对象字段（model/memory）不满足自动 derive 的 bound，手动实现 Debug
impl std::fmt::Debug for ReActAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReActAgent")
            .field("name", &self.name)
            .field("max_iters", &self.max_iters)
            .finish_non_exhaustive()
    }
}

/// [`ReActAgent`] 构建器。
pub struct ReActAgentBuilder {
    name: String,
    sys_prompt: String,
    model: Option<Arc<dyn Model>>,
    toolkit: Arc<Toolkit>,
    memory: Option<Arc<dyn Memory>>,
    state_store: Option<Arc<dyn AgentStateStore>>,
    session_log_store: Option<Arc<dyn SessionLogStore>>,
    history_mode: SessionHistoryMode,
    default_session_id: Option<String>,
    middlewares: Vec<Arc<dyn Middleware>>,
    permission: Arc<dyn PermissionChecker>,
    /// 新会话的初始权限上下文（对应 Java builder `permissionContext`）：
    /// 仅在会话状态首次创建时写入；已有会话以持久化状态为准。
    permission_context: Option<PermissionContextState>,
    max_iters: usize,
    iteration_hook: Option<IterationHook>,
}

impl ReActAgent {
    /// 创建构建器。
    pub fn builder() -> ReActAgentBuilder {
        ReActAgentBuilder {
            name: "assistant".to_string(),
            sys_prompt: "You are a helpful assistant.".to_string(),
            model: None,
            toolkit: Arc::new(Toolkit::new()),
            memory: None,
            state_store: None,
            session_log_store: None,
            history_mode: SessionHistoryMode::default(),
            default_session_id: None,
            middlewares: Vec::new(),
            permission: Arc::new(crate::permission::AllowAll),
            permission_context: None,
            max_iters: 10,
            iteration_hook: None,
        }
    }

}

/// 事件出口封装：`None` 时静默丢弃（非流式 `call()` 无事件流）。
struct EventSink(Option<mpsc::Sender<AgentEvent>>);

/// 一次执行的运行上下文：reply_id 贯穿本次执行的所有事件
/// （对齐 Java：一次 streamEvents() 调用产生一个 replyId），
/// 同时持有本次调用的 [`RuntimeContext`] 与中间件事件过滤器。
struct RunContext {
    reply_id: String,
    runtime: RuntimeContext,
    events: EventSink,
    /// 本次调用的事件过滤器（由中间件在调用开始时新建，对齐 Java 在 `Flux.defer`
    /// 内建状态的做法）。锁只在过滤期间持有，发送事件时已释放。
    filters: Mutex<Vec<Box<dyn EventFilter>>>,
    /// 本次调用的活状态——**对话真相**。对应 Java 每次调用从 store 加载后
    /// 挂在调用作用域上的 `AgentState`（`ctx.setAgentState(scope.state)`）。
    state: Arc<Mutex<AgentState>>,
    /// 会话日志记录器；`LEGACY` 模式或未配置日志后端时为 `None`。
    recorder: Option<Arc<SessionRecorder>>,
    /// 会话执行命令（对应 Java 挂在 RuntimeContext 上的 `SessionExecution`）。
    execution: Option<Arc<Mutex<SessionExecution>>>,
    /// 1.x 兼容的历史镜像（只在调用结束时同步）。
    memory: Option<Arc<dyn Memory>>,
}

impl RunContext {
    #[allow(clippy::too_many_arguments)]
    fn new(
        runtime: RuntimeContext,
        events: Option<mpsc::Sender<AgentEvent>>,
        filters: Vec<Box<dyn EventFilter>>,
        state: Arc<Mutex<AgentState>>,
        recorder: Option<Arc<SessionRecorder>>,
        execution: Option<Arc<Mutex<SessionExecution>>>,
        memory: Option<Arc<dyn Memory>>,
    ) -> Self {
        Self {
            reply_id: runtime.reply_id().to_string(),
            runtime,
            events: EventSink(events),
            filters: Mutex::new(filters),
            state,
            recorder,
            execution,
            memory,
        }
    }

    /// 活状态的共享句柄（中间件可读可改）。
    fn state_handle(&self) -> Arc<Mutex<AgentState>> {
        self.state.clone()
    }

    /// 当前状态的独立副本。
    fn snapshot(&self) -> AgentState {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// 追加一条消息到对话真相。
    fn push(&self, msg: Msg) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .context
            .push(msg);
    }

    /// 当前历史（对话真相互制副本）。
    fn history(&self) -> Vec<Msg> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .context
            .clone()
    }

    /// 把当前状态写进会话日志（无记录器时是 no-op）。
    fn capture(&self, reason: &str) -> Result<()> {
        if let Some(recorder) = &self.recorder {
            recorder.capture_state(&self.snapshot(), reason)?;
        }
        Ok(())
    }

    /// 记录一步开始（无记录器时是 no-op）。
    fn step(&self, iteration: usize) -> Result<()> {
        if let Some(recorder) = &self.recorder {
            recorder.step(iteration)?;
        }
        Ok(())
    }

    /// 追加一条会话事件（无记录器时是 no-op）。
    fn record(&self, kind: &str, payload: Value) -> Result<()> {
        if let Some(recorder) = &self.recorder {
            recorder.append(kind, payload)?;
        }
        Ok(())
    }

    /// 把最终历史同步进 1.x 兼容镜像（尽力而为：镜像失败不该毁掉整次调用）。
    async fn mirror_history(&self) {
        let Some(memory) = &self.memory else {
            return;
        };
        let history = self.history();
        if memory.clear().await.is_err() {
            return;
        }
        for message in history {
            if memory.add(message).await.is_err() {
                return;
            }
        }
    }

    async fn emit(&self, kind: AgentEventKind) {
        self.emit_event(AgentEvent::new(kind)).await;
    }

    /// 发出事件：先过中间件过滤器链，再写事件出口。
    async fn emit_event(&self, event: AgentEvent) {
        let filtered = {
            let mut out = vec![event];
            if let Ok(mut guard) = self.filters.lock() {
                for filter in guard.iter_mut() {
                    out = out.into_iter().flat_map(|e| filter.filter(e)).collect();
                    if out.is_empty() {
                        break;
                    }
                }
            }
            out
        };
        if let Some(tx) = &self.events.0 {
            for event in filtered {
                let _ = tx.send(event).await;
            }
        }
    }

    /// 调用结束：清理各过滤器的状态（对应 `doFinally`）。
    fn finish_filters(&self) {
        if let Ok(mut guard) = self.filters.lock() {
            for filter in guard.iter_mut() {
                filter.finish();
            }
        }
    }

    /// 工具执行的事件出口（对应 Java ToolEmitter 注入；`None` 时进度静默丢弃）。
    fn tool_emitter(&self) -> Option<AgentEventEmitter> {
        self.events.0.clone()
    }
}

/// 流式聚合器：把模型事件流转换为内容块，同时转发块级 agent 事件。
///
/// 对应 Java `agent.accumulator` 包的设计（分片聚合与事件转发一体）；
/// Java 按 Text/Thinking/ToolCalls 拆分累加器类，Rust 用枚举 match 统一分派。
struct StreamAggregator {
    blocks: Vec<ContentBlock>,
    current_block_id: Option<String>,
    /// 当前思考块的 block id（思考与文本块可交替，各自独立开合）。
    current_thinking_id: Option<String>,
}

impl StreamAggregator {
    fn new() -> Self {
        Self {
            blocks: Vec::new(),
            current_block_id: None,
            current_thinking_id: None,
        }
    }

    /// 吸收一个模型事件：更新聚合状态并转发对应 agent 事件。
    async fn absorb(&mut self, ctx: &RunContext, event: ModelEvent) {
        match event {
            ModelEvent::TextDelta { text } => {
                // 文本块与思考块互斥开合：文本到来前先闭合思考块。
                self.close_thinking_block(ctx).await;
                let block_id = match self.current_block_id.clone() {
                    Some(id) => id,
                    None => {
                        let id = Uuid::new_v4().simple().to_string();
                        ctx.emit(AgentEventKind::TextBlockStart {
                            reply_id: ctx.reply_id.clone(),
                            block_id: id.clone(),
                        })
                        .await;
                        self.current_block_id = Some(id.clone());
                        id
                    }
                };
                ctx.emit(AgentEventKind::TextBlockDelta {
                    reply_id: ctx.reply_id.clone(),
                    block_id: block_id.clone(),
                    delta: text.clone(),
                })
                .await;
                match self.blocks.last_mut() {
                    Some(ContentBlock::Text { text: acc }) => acc.push_str(&text),
                    _ => self.blocks.push(ContentBlock::Text { text }),
                }
            }
            ModelEvent::ThinkingDelta { text } => {
                // 思考块与文本块互斥开合：思考到来前先闭合文本块。
                self.close_text_block(ctx).await;
                let block_id = match self.current_thinking_id.clone() {
                    Some(id) => id,
                    None => {
                        let id = Uuid::new_v4().simple().to_string();
                        ctx.emit(AgentEventKind::ThinkingBlockStart {
                            reply_id: ctx.reply_id.clone(),
                            block_id: id.clone(),
                        })
                        .await;
                        self.current_thinking_id = Some(id.clone());
                        id
                    }
                };
                ctx.emit(AgentEventKind::ThinkingBlockDelta {
                    reply_id: ctx.reply_id.clone(),
                    block_id: block_id.clone(),
                    delta: text.clone(),
                })
                .await;
                match self.blocks.last_mut() {
                    Some(ContentBlock::Thinking { thinking: acc }) => acc.push_str(&text),
                    _ => self.blocks.push(ContentBlock::Thinking { thinking: text }),
                }
            }
            ModelEvent::ToolUse { id, name, input } => {
                self.close_text_block(ctx).await;
                self.close_thinking_block(ctx).await;
                ctx.emit(AgentEventKind::ToolCallStart {
                    reply_id: ctx.reply_id.clone(),
                    tool_call_id: id.clone(),
                    tool_call_name: name.clone(),
                })
                .await;
                // 参数以一次完整 JSON 分片发出（对齐 Java：非流式场景聚合后单发）
                ctx.emit(AgentEventKind::ToolCallDelta {
                    reply_id: ctx.reply_id.clone(),
                    tool_call_id: id.clone(),
                    tool_call_name: name.clone(),
                    delta: input.to_string(),
                })
                .await;
                ctx.emit(AgentEventKind::ToolCallEnd {
                    reply_id: ctx.reply_id.clone(),
                    tool_call_id: id.clone(),
                    tool_call_name: name.clone(),
                })
                .await;
                self.blocks.push(ContentBlock::ToolUse {
                    id,
                    name,
                    input,
                    state: None,
                    content: None,
                });
            }
            ModelEvent::Done { usage, .. } => {
                self.close_text_block(ctx).await;
                self.close_thinking_block(ctx).await;
                ctx.emit(AgentEventKind::ModelCallEnd {
                    reply_id: ctx.reply_id.clone(),
                    usage,
                })
                .await;
            }
        }
    }

    /// 关闭当前文本块（块切换或流结束时调用，发 TextBlockEnd）。
    async fn close_text_block(&mut self, ctx: &RunContext) {
        if let Some(block_id) = self.current_block_id.take() {
            ctx.emit(AgentEventKind::TextBlockEnd {
                reply_id: ctx.reply_id.clone(),
                block_id,
            })
            .await;
        }
    }

    /// 关闭当前思考块（块切换或流结束时调用，发 ThinkingBlockEnd）。
    async fn close_thinking_block(&mut self, ctx: &RunContext) {
        if let Some(block_id) = self.current_thinking_id.take() {
            ctx.emit(AgentEventKind::ThinkingBlockEnd {
                reply_id: ctx.reply_id.clone(),
                block_id,
            })
            .await;
        }
    }
}

/// `LEGACY` 模式下整份状态在状态库里的键名（对应 Java `"agent_state"`）。
const LEGACY_STATE_KEY: &str = "agent_state";

/// 一次调用绑定到的会话槽位（对应 Java 的 `slotKey(userId, sessionId)` 解析结果）。
struct Slot {
    user_id: Option<String>,
    session_id: String,
}

/// 槽位缓存里的一项：活状态 + 该槽位的排队门（同槽位调用串行）。
#[derive(Clone)]
struct SlotEntry {
    state: Arc<Mutex<AgentState>>,
    /// 异步互斥：等锁期间不阻塞 runtime 线程（Java 侧是对 slot 对象加 `synchronized`）。
    gate: Arc<tokio::sync::Mutex<()>>,
    /// `EVENT_LOG` 模式的投影快照缓存：上次重放到的游标 + 状态基线 + 未决集合。
    /// 下次 reload 从这里增量续放，避免每次 call 全量扫日志（O(会话长度) → O(增量)）。
    snapshot: Arc<Mutex<Option<SessionSnapshot>>>,
}

impl SlotEntry {
    fn new(state: AgentState) -> Self {
        Self {
            state: Arc::new(Mutex::new(state)),
            gate: Arc::new(tokio::sync::Mutex::new(())),
            snapshot: Arc::new(Mutex::new(None)),
        }
    }
}

impl ReActAgent {
    /// 槽位键：`(userId, sessionId)`，用户标识为空表示匿名。
    fn slot_key(user_id: Option<&str>, session_id: &str) -> String {
        format!("{}\u{1f}{}", user_id.unwrap_or(""), session_id)
    }

    /// 是否配置了持久层（决定要不要每次调用都重载状态）。
    fn persistent(&self) -> bool {
        self.state_store.is_some() || self.session_log_store.is_some()
    }

    /// 打开某会话的日志读视图（未配置日志后端时返回 `None`）。
    fn open_log(&self, user_id: Option<&str>, session_id: &str) -> Result<Option<Arc<dyn SessionLog>>> {
        let Some(store) = &self.session_log_store else {
            return Ok(None);
        };
        let key = SessionKey::new(
            user_id.map(str::to_string),
            self.name.clone(),
            session_id.to_string(),
        )?;
        Ok(Some(store.open(&key, None)?))
    }

    /// 加载状态：`EVENT_LOG` 走日志重放（有快照时增量续放），`LEGACY` 走状态库整份读取。
    fn load_state(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        entry: &SlotEntry,
    ) -> Result<Option<AgentState>> {
        if self.history_mode == SessionHistoryMode::EventLog {
            let Some(log) = self.open_log(user_id, session_id)? else {
                return Ok(None);
            };
            // 增量重放：槽位缓存着上次投影快照（游标 + 基线），从游标续放即可，
            // 不必每次 call 从头扫整条日志。快照缺失（新槽位）时全量重放并落快照。
            let projection = {
                let mut cached = entry
                    .snapshot
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                let previous = cached.take();
                match previous {
                    Some(snapshot) => SessionProjection::advance(log.as_ref(), &snapshot)?,
                    None => SessionProjection::read(log.as_ref())?,
                }
            };
            *entry
                .snapshot
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(projection.snapshot());
            return Ok(projection.restore());
        }
        let Some(store) = &self.state_store else {
            return Ok(None);
        };
        Ok(store.get::<AgentState>(user_id, session_id, LEGACY_STATE_KEY)?)
    }

    /// 原子地取得（或创建）本次调用的槽位项（对应 Java `activateSlot` 的槽位定位）。
    ///
    /// 返回的第三项表示「这次是新建的槽位项」。**状态的加载不在这里做**：加载需要
    /// 先排队拿到槽位门，否则两个并发调用会各自加载、各自写入，最后一份覆盖另一份。
    async fn slot_entry(&self, runtime: &RuntimeContext) -> Result<(Slot, SlotEntry, bool)> {
        let session_id = runtime
            .session_id()
            .map(str::to_string)
            .filter(|id| !id.trim().is_empty())
            .or_else(|| self.default_session_id.clone())
            .ok_or_else(|| {
                Error::InvalidInput(
                    "session id is required: set RuntimeContext.session_id or default_session_id"
                        .into(),
                )
            })?;
        let user_id = runtime.user_id().map(str::to_string);
        let key = Self::slot_key(user_id.as_deref(), &session_id);
        let mut slots = self.slots.lock().unwrap_or_else(|error| error.into_inner());
        // 顺带淘汰空闲槽位：有持久层时，历史可随时从后端重放回来，进程内槽位只是缓存，
        // 长跑多会话不能无限增长。只清理「无外部引用且未被持有」的槽位
        // （state 强计数 = 1 且 gate 空闲），活跃会话不受影响；
        // 无持久层时 state 只活在缓存里，清理即丢历史，绝不清理。
        if self.persistent() {
            slots.retain(|_, entry| {
                Arc::strong_count(&entry.state) > 1 || entry.gate.try_lock().is_err()
            });
        }
        let is_new = !slots.contains_key(&key);
        let initial_permission = self.permission_context.clone();
        let entry = slots
            .entry(key)
            .or_insert_with(|| {
                let mut state =
                    AgentState::with_session_id(&session_id).with_user_id(user_id.clone());
                // 新会话注入 builder 的初始权限上下文（对应 Java initialPermissionContext）；
                // 已有会话由 reload_state 从持久层恢复，不受影响。
                if let Some(context) = initial_permission {
                    state.permission_context = context;
                }
                SlotEntry::new(state)
            })
            .clone();
        Ok((
            Slot {
                user_id,
                session_id,
            },
            entry,
            is_new,
        ))
    }

    /// 持槽位门加载状态（对应 Java `activateSlot` 的「从 store 重新加载」）。
    ///
    /// 有持久层时**每次都重载**：分布式部署下同一 `sessionId` 可能已被别的实例推进过，
    /// 用本地缓存会写出一份陈旧状态把它覆盖掉。无持久层时才复用进程内缓存，
    /// 否则每次调用都会从零开始，历史直接丢掉。
    fn reload_state(&self, slot: &Slot, entry: &SlotEntry, is_new: bool) -> Result<()> {
        if !is_new && !self.persistent() {
            return Ok(());
        }
        let Some(loaded) = self.load_state(slot.user_id.as_deref(), &slot.session_id, entry)? else {
            return Ok(());
        };
        *entry.state.lock().unwrap_or_else(|error| error.into_inner()) = loaded;
        Ok(())
    }

    /// 为本次运行打开记录器（仅 `EVENT_LOG` 模式 + 配置了日志后端）。
    ///
    /// `turn_id`：执行命令驱动时用命令的回合 id（记录器与收件箱对上同一回合）；
    /// 普通调用传 `None`，由记录器自动生成。
    fn open_recorder(
        &self,
        slot: &Slot,
        turn_id: Option<String>,
    ) -> Result<Option<Arc<SessionRecorder>>> {
        if self.history_mode != SessionHistoryMode::EventLog {
            return Ok(None);
        }
        let Some(log) = self.open_log(slot.user_id.as_deref(), &slot.session_id)? else {
            return Ok(None);
        };
        Ok(Some(Arc::new(SessionRecorder::new(
            log,
            turn_id,
            Uuid::new_v4().to_string(),
        )?)))
    }

    /// `LEGACY` 模式下把整份状态写回状态库（CAS，冲突即失败）。
    fn persist_state(&self, state: &AgentState) -> Result<()> {
        if self.history_mode != SessionHistoryMode::Legacy {
            return Ok(());
        }
        let Some(store) = &self.state_store else {
            return Ok(());
        };
        let user_id = state.user_id.as_deref();
        let current = store.get_versioned::<AgentState>(user_id, &state.session_id, LEGACY_STATE_KEY)?;
        store.save_if_version(user_id, &state.session_id, LEGACY_STATE_KEY, state, current.version)?;
        Ok(())
    }

    /// 某会话当前的状态快照（只读用途：外部查询、测试、管理面展示）。
    ///
    /// 有持久层时以持久层为准（只读查询不写投影缓存，直接全量重放）；
    /// 否则返回槽位缓存里的活状态。
    pub fn state_for(&self, user_id: Option<&str>, session_id: &str) -> Result<Option<AgentState>> {
        if self.history_mode == SessionHistoryMode::EventLog {
            let Some(log) = self.open_log(user_id, session_id)? else {
                // 未配置日志后端：对话真相只在槽位缓存里
                return Ok(self.slot_state(user_id, session_id));
            };
            let projection = SessionProjection::read(log.as_ref())?;
            return Ok(projection.restore());
        }
        let Some(store) = &self.state_store else {
            return Ok(self.slot_state(user_id, session_id));
        };
        Ok(store.get::<AgentState>(user_id, session_id, LEGACY_STATE_KEY)?)
    }

    /// 槽位缓存里的活状态副本（无持久层时的对话真相）。
    fn slot_state(&self, user_id: Option<&str>, session_id: &str) -> Option<AgentState> {
        let key = Self::slot_key(user_id, session_id);
        self.slots
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&key)
            .map(|entry| entry.state.lock().unwrap_or_else(|error| error.into_inner()).clone())
    }

    /// 取或建某会话的 `AgentState`（管理性用途，对应 Java `getAgentState(uid, sid)`）。
    ///
    /// 仅支持 `LEGACY` 模式（状态库 / 槽位缓存）；`EVENT_LOG` 模式的对话真相在
    /// 事实日志里，管理性变更须走 `SessionStateAccess` 围栏（未接线），此处明确报错。
    pub fn agent_state_for(&self, user_id: Option<&str>, session_id: &str) -> Result<AgentState> {
        // EventLog + 已配置日志后端：对话真相在事实日志里，管理性变更须走
        // SessionStateAccess 围栏（未接线），此处明确报错而不是绕过不变式。
        if self.history_mode == SessionHistoryMode::EventLog && self.session_log_store.is_some() {
            return Err(Error::InvalidInput(
                "agent_state_for with a session log store requires SessionStateAccess \
                 fences (not wired yet); drop the log store or use Legacy mode"
                    .into(),
            ));
        }
        if let Some(state) = self.slot_state(user_id, session_id) {
            return Ok(state);
        }
        if let Some(store) = &self.state_store {
            if let Some(loaded) = store.get::<AgentState>(user_id, session_id, LEGACY_STATE_KEY)? {
                return Ok(loaded);
            }
        }
        // 新建状态放进槽位缓存（对齐 Java `getAgentState` 的 computeIfAbsent）：
        // 后续 save_agent_state_for / 运行中 call 都以这份缓存为准。
        let mut state = AgentState::with_session_id(session_id);
        state.user_id = user_id.map(str::to_string);
        self.slots
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .entry(Self::slot_key(user_id, session_id))
            .or_insert_with(|| SlotEntry {
                state: Arc::new(std::sync::Mutex::new(state.clone())),
                gate: Arc::new(tokio::sync::Mutex::new(())),
                snapshot: Arc::new(std::sync::Mutex::new(None)),
            });
        Ok(state)
    }

    /// 保存某会话的 `AgentState`（管理性变更后写回，对应 Java `saveAgentState(uid, sid)`）：
    /// CAS 写入状态库并同步槽位缓存。仅 Legacy 模式。
    pub fn save_agent_state_for(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state: &AgentState,
    ) -> Result<()> {
        if self.history_mode == SessionHistoryMode::EventLog && self.session_log_store.is_some() {
            return Err(Error::InvalidInput(
                "save_agent_state_for with a session log store requires SessionStateAccess \
                 fences (not wired yet); drop the log store or use Legacy mode"
                    .into(),
            ));
        }
        if let Some(store) = &self.state_store {
            let current = store.get_versioned::<AgentState>(user_id, session_id, LEGACY_STATE_KEY)?;
            store.save_if_version(user_id, session_id, LEGACY_STATE_KEY, state, current.version)?;
        }
        // 同步槽位缓存（存在才更新；不主动建槽位——那会绕过 gate 语义）
        let key = Self::slot_key(user_id, session_id);
        if let Some(entry) = self
            .slots
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&key)
        {
            *entry.state.lock().unwrap_or_else(|error| error.into_inner()) = state.clone();
        }
        Ok(())
    }

    /// 清空某会话的对话上下文（身份与非对话状态保留，对应 Java `clearContext(uid, sid)`）。
    ///
    /// 状态库与槽位缓存都没有该会话时是空操作（与 Java 一致）。
    /// 不取消进行中的调用。
    pub fn clear_context_for(&self, user_id: Option<&str>, session_id: &str) -> Result<()> {
        if self.history_mode == SessionHistoryMode::EventLog && self.session_log_store.is_some() {
            return Err(Error::InvalidInput(
                "clear_context_for with a session log store requires SessionStateAccess \
                 fences (not wired yet); drop the log store or use Legacy mode"
                    .into(),
            ));
        }
        // 槽位缓存有 → 直接改活状态；否则看状态库；都没有 → 空操作
        let key = Self::slot_key(user_id, session_id);
        let cached = self
            .slots
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&key)
            .map(|entry| entry.state.clone());
        if let Some(state_cell) = cached {
            let mut state = state_cell.lock().unwrap_or_else(|error| error.into_inner());
            state.context.clear();
            state.summary.clear();
            drop(state);
            if let Some(store) = &self.state_store {
                let guard = self
                    .slots
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .get(&key)
                    .map(|entry| entry.state.clone());
                if let Some(cell) = guard {
                    let state = cell.lock().unwrap_or_else(|error| error.into_inner());
                    let current =
                        store.get_versioned::<AgentState>(user_id, session_id, LEGACY_STATE_KEY)?;
                    store.save_if_version(
                        user_id,
                        session_id,
                        LEGACY_STATE_KEY,
                        &*state,
                        current.version,
                    )?;
                }            }
            return Ok(());
        }
        let Some(store) = &self.state_store else {
            return Ok(());
        };
        let Some(mut state) = store.get::<AgentState>(user_id, session_id, LEGACY_STATE_KEY)? else {
            return Ok(());
        };
        state.context.clear();
        state.summary.clear();
        let current = store.get_versioned::<AgentState>(user_id, session_id, LEGACY_STATE_KEY)?;
        store.save_if_version(user_id, session_id, LEGACY_STATE_KEY, &state, current.version)?;
        Ok(())
    }

    /// 淘汰全部槽位缓存（持久层保留，对应 Java `clearStateCache()`）。
    pub fn clear_state_cache(&self) {
        self.slots
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
    }

    /// 淘汰指定槽位缓存（对应 Java `clearStateCache(uid, sid)`）。
    pub fn clear_state_cache_for(&self, user_id: Option<&str>, session_id: &str) {
        let key = Self::slot_key(user_id, session_id);
        self.slots
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&key);
    }

    /// 运行中插入一条转向消息（steer，对应 Java `AgentSession.steer`）。
    ///
    /// 在该会话**当前调用的下一个步边界**注入对话真相（beforeStep 语义）；
    /// 若调用已结束则留在队列里，由调用方结算时经 [`Self::take_steers`] 取走。
    pub fn push_steer(&self, session_id: impl Into<String>, msg: Msg) {
        self.steers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .entry(session_id.into())
            .or_default()
            .push(msg);
    }

    /// 该会话是否已有内容完全相同的未消费 steer（重复入队防御：
    /// IME 连击等极端竞态下同一句插话入队两次，模型会当两轮指令各处理一遍）。
    pub fn has_duplicate_steer(&self, session_id: &str, text: &str) -> bool {
        self.steers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(session_id)
            .map(|queue| queue.iter().any(|m| m.text() == text))
            .unwrap_or(false)
    }

    /// 丢弃该会话全部未消费的 steer 消息（中断/删除会话时调用）：
    /// 中断后残留的 steer 不应再驱动后续续轮。
    pub fn clear_steers(&self, session_id: &str) {
        self.steers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(session_id);
    }

    /// 取走该会话全部未消费的 steer 消息（turn 结算时调用）。
    pub fn take_steers(&self, session_id: &str) -> Vec<Msg> {
        self.steers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(session_id)
            .unwrap_or_default()
    }

    /// 步边界取走本调用的待注入 steer（无 session id 的调用不会有入队）。
    fn take_steers_for(&self, session_id: Option<&str>) -> Vec<Msg> {
        let Some(session_id) = session_id else {
            return Vec::new();
        };
        self.take_steers(session_id)
    }

    /// 默认会话（`default_session_id`）的状态快照。
    pub fn state(&self) -> Result<Option<AgentState>> {
        match &self.default_session_id {
            Some(session_id) => self.state_for(None, session_id),
            None => Ok(None),
        }
    }

    /// ReAct 主循环。`events` 为 Some 时推送事件；`streaming` 为 true 时
    /// reasoning 走模型流式接口并转发块级增量；`runtime` 是本调用的运行时上下文。
    async fn run(
        &self,
        runtime: RuntimeContext,
        msgs: Vec<Msg>,
        events: Option<mpsc::Sender<AgentEvent>>,
        streaming: bool,
        execution: Option<Arc<Mutex<SessionExecution>>>,
    ) -> Result<Msg> {
        // 1) 定位槽位 → 排队 → 加载状态：顺序不能反，先排队再读才不会互相覆盖
        let (slot, entry, is_new) = self.slot_entry(&runtime).await?;
        // 同一槽位串行：两次并发运行共享一份状态与一条日志链，抢跑一定互相踩
        let _gate = entry.gate.lock().await;
        self.reload_state(&slot, &entry, is_new)?;
        let state = entry.state.clone();
        // 2) 打开记录器：打开失败说明历史有歧义（未对账/运行中/回合已完成），必须炸出
        // 执行命令的回合 id：记录器与收件箱必须对上同一个回合（对应 Java TURN_ID_KEY）
        let command_turn = execution
            .as_ref()
            .and_then(|execution| {
                execution
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .command()
                    .turn_id
                    .clone()
            });
        if execution.is_some() && command_turn.as_deref().map(str::is_empty) != Some(false) {
            return Err(Error::Session(SessionError::invalid(
                "execution command has no turn id",
            )));
        }
        let recorder = self.open_recorder(&slot, command_turn)?;
        if let Some(execution) = &execution {
            let Some(recorder) = recorder.as_ref() else {
                return Err(Error::Session(SessionError::invalid(
                    "session execution requires an event-log session backend",
                )));
            };
            // 持有写者租约时做准入校验（对应 Java prepare）；失败要关干净再传播。
            // 注意先把 Result 绑出来：if-let 条件里的 MutexGuard 临时值会活到整个
            // 块结束，块内再 lock 同一把锁就是自死锁。
            let prepared = execution
                .lock()
                .unwrap_or_else(|lock_error| lock_error.into_inner())
                .prepare(recorder);
            if let Err(error) = prepared {
                let _ = execution
                    .lock()
                    .unwrap_or_else(|lock_error| lock_error.into_inner())
                    .close(recorder);
                let _ = recorder.close();
                return Err(Error::Session(error));
            }
        }
        let ctx = RunContext::new(
            runtime,
            events,
            self.middlewares.event_filters(),
            state,
            recorder,
            execution,
            self.memory.clone(),
        );
        let outcome = self.drive(&ctx, msgs, streaming).await;
        ctx.finish_filters();
        // 3) 落状态：失败必须向上传播，不能把「写不进历史」伪装成成功
        let status = match &outcome {
            Ok(_) => "completed",
            Err(_) => "failed",
        };
        let state = ctx.snapshot();
        match &ctx.recorder {
            Some(recorder) => {
                // 先关执行（幂等：结算时已关过则无操作），再落终态
                if let Some(execution) = &ctx.execution {
                    let _ = execution
                        .lock()
                        .unwrap_or_else(|lock_error| lock_error.into_inner())
                        .close(recorder);
                }
                recorder.finish(&state, status)?
            }
            None => self.persist_state(&state)?,
        }
        // 4) 兼容镜像（尽力而为）
        ctx.mirror_history().await;
        // 5) 调用完成通知（对应 Java on_agent doOnComplete）：状态已落盘，
        //    中间件可在此 spawn fire-and-forget 的后台收尾（memory flush 等）。
        //    仅成功路径通知：失败时调用方拿到 Err，后台收尾没有意义。
        if outcome.is_ok() {
            let state_handle = ctx.state_handle();
            self.middlewares
                .notify_agent_complete(&ctx.runtime, &state_handle);
        }
        outcome
    }

    /// 循环主体：与 [`Self::run`] 分离，保证过滤器状态在任何出口都被清理。
    async fn drive(&self, ctx: &RunContext, msgs: Vec<Msg>, streaming: bool) -> Result<Msg> {
        // onAgent：中间件可改写本次调用的输入消息
        let mut input = self.middlewares.apply_agent(&ctx.runtime, AgentInput::new(msgs));
        // onAgentStateReady：本次调用的状态就绪通知（可就地改写输入副本与状态）
        let state_handle = ctx.state_handle();
        self.middlewares
            .notify_state_ready(&ctx.runtime, &mut input.msgs, &state_handle);
        // 权限 HITL：输入里的确认答复先应用（批准/拒绝），再过滤掉不进对话历史
        // （对齐 Java `validateAndAcceptConfirmResults` + resume 滤掉确认答复）。
        let confirm_results = extract_confirm_results(&input.msgs);
        if !confirm_results.is_empty() {
            self.apply_confirm_results(ctx, confirm_results).await?;
        }
        for msg in &input.msgs {
            if has_confirm_results(msg) {
                continue;
            }
            ctx.push(msg.clone());
        }
        // 会话开始：基线 / 运行 / 回合 / 输入
        // 执行命令驱动时，日志里记录的是命令准入后的输入（resume 会滤掉确认答复）
        if let Some(recorder) = &ctx.recorder {
            let inputs = match &ctx.execution {
                Some(execution) => execution
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .inputs()
                    .to_vec(),
                None => input.msgs.clone(),
            };
            recorder.start(&inputs, &ctx.snapshot())?;
        }
        ctx.emit(AgentEventKind::AgentStart {
            reply_id: ctx.reply_id.clone(),
            session_id: ctx.runtime.session_id().map(str::to_string),
            name: self.name.clone(),
            role: "assistant".into(),
        })
        .await;

        // 空响应重试计数：reasoning 模型偶发只输出 thinking、正文为空且无
        // 工具调用（见下方防御），限重试一次防循环。
        let mut empty_retries = 0usize;
        for iteration in 0..self.max_iters {
            // 一步 = 一轮 reasoning（+ acting）。步边界是恢复时的最小定位单位。
            ctx.step(iteration)?;
            // 每步开始前注入收件箱里的 steer / inject 消息（对应 Java beforeStep）
            if let (Some(recorder), Some(execution)) = (&ctx.recorder, &ctx.execution) {
                let mut state = ctx.state.lock().unwrap_or_else(|error| error.into_inner());
                execution
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .before_step(recorder, &mut state)
                    .map_err(Error::Session)?;
            }
            // 运行中插入（steer）：Legacy 模式（无 execution 链路）的步边界注入，
            // 语义与上面 inbox steer 一致——插入消息当步生效，而非等调用结束。
            for msg in self.take_steers_for(ctx.runtime.session_id()) {
                ctx.push(msg);
                ctx.capture("steer_injected")?;
            }
            // 迭代边界异步钩子（对应 Java 每轮 onReasoning 的检查点，如压缩）：
            // 在 reasoning 之前执行；失败即终止调用（钩子自行决定是否降级）。
            if let Some(hook) = &self.iteration_hook {
                hook(&ctx.runtime, &ctx.state, false).await?;
            }
            let assistant = match self.reasoning(ctx, streaming).await {
                Ok(msg) => msg,
                Err(error) => {
                    // 上下文溢出恢复（对应 Java CompactionMiddleware.overflowRecovery）：
                    // 以 forced=true 调钩子做最激进压缩后，同轮重试一次 reasoning；
                    // 仍失败（或无钩子/非溢出错误）则照常传播。
                    let recoverable =
                        self.iteration_hook.is_some() && is_context_overflow_error(&error);
                    if !recoverable {
                        return Err(error);
                    }
                    if let Some(hook) = &self.iteration_hook {
                        hook(&ctx.runtime, &ctx.state, true).await?;
                    }
                    self.reasoning(ctx, streaming).await?
                }
            };
            if assistant.tool_uses().is_empty() {
                // 空响应防御：模型偶发只产出 thinking、正文为空且无工具调用——
                // 直接当最终回复会让回合「无疾而终」（前端无任何可见输出）。
                // 注入提示重试一次；仍空则照常结束（前端有占位兜底）。
                if assistant.text().trim().is_empty() && empty_retries == 0 {
                    empty_retries += 1;
                    ctx.push(Msg::user(
                        "system",
                        "你的上一条回复没有正文内容（可能只有思考过程）。请直接给出最终回答。",
                    ));
                    continue;
                }
                // 无工具调用 → 最终回复（携带停止原因，对齐 Java GenerateReason）
                let final_msg = assistant.with_generate_reason(GenerateReason::ModelStop);
                // 落进对话真相的必须是带 reason 的那条，否则历史与返回值不一致
                ctx.push(final_msg.clone());
                if let Some(recorder) = &ctx.recorder {
                    recorder.result(&final_msg);
                }
                // 回合结算：还有未消费的 steer 就继续下一轮（对应 Java settleSession）
                if let (Some(recorder), Some(execution)) = (&ctx.recorder, &ctx.execution) {
                    let again = execution
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .continue_or_close(recorder)
                        .map_err(Error::Session)?;
                    if again {
                        continue;
                    }
                }
                ctx.emit(AgentEventKind::AgentResult {
                    result: final_msg.clone(),
                })
                .await;
                ctx.emit(AgentEventKind::AgentEnd {
                    reply_id: ctx.reply_id.clone(),
                })
                .await;
                return Ok(final_msg);
            }
            ctx.push(assistant.clone());
            if self.acting(ctx, &assistant).await? {
                // 权限 HITL 挂起：本轮终止，等用户确认（对齐 Java ASKING pause，
                // caller MUST supply confirmations）。回合正常结算：状态已落盘，
                // ASKING 块在 context 里；确认结果随下一次 call 的
                // `CONFIRM_RESULTS` metadata 传入。
                let final_msg = assistant.with_generate_reason(GenerateReason::PermissionAsking);
                if let Some(recorder) = &ctx.recorder {
                    recorder.result(&final_msg);
                }
                if let (Some(recorder), Some(execution)) = (&ctx.recorder, &ctx.execution) {
                    execution
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .continue_or_close(recorder)
                        .map_err(Error::Session)?;
                }
                ctx.emit(AgentEventKind::AgentResult {
                    result: final_msg.clone(),
                })
                .await;
                ctx.emit(AgentEventKind::AgentEnd {
                    reply_id: ctx.reply_id.clone(),
                })
                .await;
                return Ok(final_msg);
            }
        }

        ctx.emit(AgentEventKind::ExceedMaxIters {
            reply_id: ctx.reply_id.clone(),
            max_iters: self.max_iters as u32,
            current_iter: self.max_iters as u32,
        })
        .await;
        // 终态由 `run` 统一写 `failed`（Java 侧 MAX_ITERATIONS 也是 failed outcome）
        Err(Error::MaxIterations(self.max_iters))
    }

    /// 一轮 reasoning：组请求 → 中间件改写 → 模型调用（流式/非流式）→ 聚合为 assistant 消息。
    async fn reasoning(&self, ctx: &RunContext, streaming: bool) -> Result<Msg> {
        // system prompt 走管道模式（多个中间件依次串联）
        let sys_prompt = self
            .middlewares
            .apply_system_prompt(&ctx.runtime, self.sys_prompt.clone());
        let history = ctx.history();
        let mut messages = Vec::with_capacity(history.len() + 1);
        messages.push(Msg::system(&self.name, &sys_prompt));
        messages.extend(history);

        // onReasoning：改写本轮消息与生成参数
        // 工具 schema 按会话激活集过滤（ToolContextState.activated_groups 是真相，
        // 元工具上一轮的装备结果在这里生效；对应 Java `getToolSchemas(activeGroups)`）
        let active_groups: Vec<String> = ctx
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .tool_context
            .activated_groups()
            .to_vec();
        let reasoning = self.middlewares.apply_reasoning(
            &ctx.runtime,
            ReasoningInput {
                messages,
                options: GenerateOptions {
                    tools: self.toolkit.schemas_with(&active_groups),
                    ..Default::default()
                },
            },
        );

        // onModelCall：发出前的最后一道改写（可替换模型实例）
        let call = self.middlewares.apply_model_call(
            &ctx.runtime,
            ModelCallInput::new(reasoning.messages, reasoning.options, self.model.clone()),
        );
        let model = call.model;

        // 模型调用前拍一次状态快照（对应 Java prepareModel 的 `state/checkpoint{before_model}`）：
        // 请求发出后进程若消失，快照是判断「这轮是否已发生过」的唯一依据。
        ctx.capture("before_model")?;

        ctx.emit(AgentEventKind::ModelCallStart {
            reply_id: ctx.reply_id.clone(),
        })
        .await;

        let request = ChatRequest {
            messages: call.messages,
            options: call.options,
        };

        let content: Vec<ContentBlock> = if streaming {
            let mut rx = model.call_stream(request).await?;
            let mut aggregator = StreamAggregator::new();
            while let Some(event) = rx.recv().await {
                aggregator.absorb(ctx, event?).await;
            }
            aggregator.blocks
        } else {
            let response = model.call(request).await?;
            ctx.emit(AgentEventKind::ModelCallEnd {
                reply_id: ctx.reply_id.clone(),
                usage: response.usage,
            })
            .await;
            response.content
        };

        Ok(Msg::assistant(&self.name, content))
    }

    /// 一轮 acting：中间件改写待执行的工具调用后逐个执行，结果写回状态。
    async fn acting(&self, ctx: &RunContext, assistant: &Msg) -> Result<bool> {
        let tool_calls: Vec<ToolCallSpec> = assistant
            .tool_uses()
            .into_iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, name, input, .. } => Some(ToolCallSpec {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                }),
                _ => None,
            })
            .collect();
        let input = self
            .middlewares
            .apply_acting(&ctx.runtime, ActingInput { tool_calls });
        // 原地打转检测：本轮工具调用与上一条 assistant 的完全一致（name+input
        // 逐个相同）说明模型在重复同一动作——多半是工具反复失败后固执重试。
        // 给这轮的失败结果附加换策略提示，打破「思考→失败→同样思考」循环。
        let repeated_calls = {
            let state = ctx.state.lock().unwrap_or_else(|error| error.into_inner());
            let last_assistant = state
                .context
                .iter()
                .rev()
                .find(|msg| msg.role == crate::message::Role::Assistant);
            match last_assistant {
                Some(prev) => {
                    let prev_uses = prev.tool_uses();
                    !input.tool_calls.is_empty()
                        && prev_uses.len() == input.tool_calls.len()
                        && prev_uses.iter().zip(input.tool_calls.iter()).all(|(a, b)| {
                            matches!(
                                (a, b),
                                (
                                    ContentBlock::ToolUse { name: an, input: ai, .. },
                                    ToolCallSpec { name: bn, input: bi, .. }
                                ) if an == bn && ai == bi
                            )
                        })
                }
                None => false,
            }
        };
        // 批量预裁决（对齐 Java `evaluatePermissions`）：RequireApproval 的调用
        // 标 ASKING 挂起、不执行；其余按裁决执行。
        let mut asking: Vec<ContentBlock> = Vec::new();
        for call in input.tool_calls {
            let decision = self.decide_permission(ctx, &call.name, &call.input).await;
            if matches!(decision, PermissionDecision::RequireApproval) {
                self.mark_tool_call_state(ctx, &call.id, ToolCallState::Asking)?;
                asking.push(ContentBlock::ToolUse {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    input: call.input.clone(),
                    state: Some(ToolCallState::Asking),
                    content: None,
                });
                continue;
            }
            self.execute_tool(ctx, &call.id, &call.name, &call.input, decision, repeated_calls)
                .await?;
        }
        if !asking.is_empty() {
            // 权限 HITL：挂起等待用户确认（对齐 Java `RequireUserConfirmEvent`）；
            // 确认结果经 metadata `CONFIRM_RESULTS` 随下一次 call 传入。
            ctx.emit(AgentEventKind::RequireUserConfirm {
                reply_id: ctx.reply_id.clone(),
                tool_calls: asking,
            })
            .await;
            return Ok(true);
        }
        Ok(false)
    }

    /// 权限裁决（对齐 Java `evaluateOne`）：会话权限上下文非 trivial（配置了规则
    /// 或非默认模式）时走规则引擎；trivial 时退化为工具自检（PermissionChecker，
    /// builder 默认 AllowAll）。引擎在锁外裁决：锁内只 clone 快照，
    /// 避免 std Mutex 跨 await 持有。
    async fn decide_permission(
        &self,
        ctx: &RunContext,
        name: &str,
        input: &Value,
    ) -> PermissionDecision {
        let permission_context = ctx
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .permission_context
            .clone();
        if permission_context.is_trivial() {
            self.permission.check(name, input).await
        } else {
            let tool = self.toolkit.get(name).map(|tool| tool.as_ref());
            PermissionEngine::new(permission_context)
                .check(tool, name, input)
                .await
        }
    }

    /// 把 context 里指定 id 的 ToolUse 块标记为目标状态（对齐 Java
    /// `MessageUtils.replaceToolUseBlocks` + `ToolUseBlock.withState`）。
    fn mark_tool_call_state(
        &self,
        ctx: &RunContext,
        id: &str,
        target: ToolCallState,
    ) -> Result<()> {
        let mut state = ctx.state.lock().unwrap_or_else(|error| error.into_inner());
        'outer: for msg in state.context.iter_mut().rev() {
            for block in msg.content.iter_mut() {
                if let ContentBlock::ToolUse {
                    id: block_id,
                    state: block_state,
                    ..
                } = block
                {
                    if block_id == id {
                        *block_state = Some(target);
                        break 'outer;
                    }
                }
            }
        }
        Ok(())
    }

    /// 指定 id 的 ToolUse 块当前是否处于目标状态。
    fn tool_call_state_is(
        &self,
        ctx: &RunContext,
        id: &str,
        expected: ToolCallState,
    ) -> Result<bool> {
        let state = ctx.state.lock().unwrap_or_else(|error| error.into_inner());
        for msg in state.context.iter().rev() {
            for block in &msg.content {
                if let ContentBlock::ToolUse {
                    id: block_id,
                    state: block_state,
                    ..
                } = block
                {
                    if block_id == id {
                        return Ok(*block_state == Some(expected));
                    }
                }
            }
        }
        Ok(false)
    }

    /// 应用权限 HITL 确认结果（对齐 Java `applyConfirmResults`）：
    /// 批准 → ToolUse 块升 ALLOWED + 建议规则写进会话权限上下文，随后直接执行
    /// （对齐 Java resume 的 direct execute，不再过权限门）；拒绝 → 写 DENIED
    /// 工具结果（完整 denied 生命周期）。确认只能引用当前 ASKING 的调用
    /// （对齐 Java `validateAndAcceptConfirmResults`）。
    async fn apply_confirm_results(&self, ctx: &RunContext, results: Vec<ConfirmResult>) -> Result<()> {
        let mut approved: Vec<ToolCallSpec> = Vec::new();
        for result in results {
            let ContentBlock::ToolUse {
                id,
                name,
                input,
                ..
            } = result.tool_call
            else {
                continue;
            };
            if !self.tool_call_state_is(ctx, &id, ToolCallState::Asking)? {
                return Err(Error::InvalidInput(format!(
                    "confirm result references a tool call that is not asking: {id}"
                )));
            }
            if result.confirmed {
                // 建议规则写进会话权限上下文（对齐 Java `permissionEngine.addRule`；
                // Rust 侧引擎每次从状态快照构造，写状态即对后续裁决生效）
                if let Some(rules) = result.rules {
                    let mut state = ctx.state.lock().unwrap_or_else(|error| error.into_inner());
                    // add_allow_rule 自带「移除同工具 ask」语义（用户显式授权
                    // 覆盖项目 ask 配置，否则「始终允许」对 ask 工具永不生效）。
                    for rule in rules {
                        state.permission_context.add_allow_rule(rule);
                    }
                }
                self.mark_tool_call_state(ctx, &id, ToolCallState::Allowed)?;
                approved.push(ToolCallSpec { id, name, input });
            } else {
                let reason = result
                    .reason
                    .unwrap_or_else(|| "Permission denied by user".to_string());
                self.execute_tool(
                    ctx,
                    &id,
                    &name,
                    &input,
                    PermissionDecision::Deny(reason),
                    false,
                )
                .await?;
            }
        }
        for call in approved {
            self.execute_tool(
                ctx,
                &call.id,
                &call.name,
                &call.input,
                PermissionDecision::Allow,
                false,
            )
            .await?;
        }
        Ok(())
    }

    /// 执行单个工具调用：权限门控 → 会话事件 → 执行 → ToolResult 三段事件。
    ///
    /// "模型发起了什么调用"已由 reasoning 阶段的 ToolCall 三段事件表达
    /// （对齐 Java：执行开始由 ToolResultStart 承担，不重复发 ToolCallStart）。
    ///
    /// 会话事件严格按 Java 的顺序写：`action/start` → `tool/decision` →
    /// `tool/dispatch`（仅放行时）→ …执行… → `action/end`。
    /// `dispatch` 没有配对的 `action/end` 就是「结果未知」，重放会把它钉在
    /// `uncertain_tool_calls` 上，逼调用方先对账而不是猜。
    async fn execute_tool(
        &self,
        ctx: &RunContext,
        id: &str,
        name: &str,
        input: &Value,
        decision: PermissionDecision,
        repeated_call: bool,
    ) -> Result<()> {
        // 工具调用上下文：reply_id / 调用 id / agent 身份 / 事件出口
        // （对应 Java ToolExecutionContext + ToolEmitter 注入）
        let tool_ctx = ToolContext {
            reply_id: ctx.reply_id.clone(),
            tool_call_id: id.to_string(),
            tool_name: name.to_string(),
            agent_id: self.name.clone(),
            agent_name: self.name.clone(),
            session_id: ctx.runtime.session_id().unwrap_or_default().to_string(),
            user_id: ctx.runtime.user_id().map(str::to_string),
            metadata: serde_json::Map::new(),
            emitter: ctx.tool_emitter(),
            // 元工具经它改写会话级激活组（ToolContextState.activated_groups）
            state: Some(ctx.state_handle()),
        };

        let action_id = Uuid::new_v4().to_string();
        if let Some(recorder) = &ctx.recorder {
            recorder.action_started(id, &action_id);
        }
        ctx.record(
            "action/start",
            serde_json::json!({
                "observation": {
                    "actionId": action_id,
                    "toolCallId": id,
                    "name": name,
                    "agentId": self.name,
                    "userId": ctx.runtime.user_id(),
                    "sessionId": ctx.runtime.session_id(),
                    "status": "started",
                },
                "toolCallId": id,
                "result": Value::Null,
            }),
        )?;

        // 权限门控：裁决由 acting 预裁决传入（`decide_permission`）；恢复路径对
        // 已批准的调用传 Allow 直执行（对齐 Java resume 的 direct execute）。
        // 非 Allow 以对应状态的结果回传给模型，由模型自行调整；工具执行失败
        // 同样不中断循环（对齐 Java ToolResultState 语义：denied 是独立状态，
        // 与执行错误 error 区分）。
        let allowed = matches!(decision, PermissionDecision::Allow);
        if allowed {
            // 执行前快照：工具一旦产生副作用，执行前的状态就是唯一的现场
            ctx.capture("before_tool")?;
        }
        ctx.record(
            "tool/decision",
            serde_json::json!({
                "toolCallId": id,
                "decision": if allowed { "allowed" } else { "denied" },
            }),
        )?;
        if allowed {
            ctx.record(
                "tool/dispatch",
                serde_json::json!({
                    "toolCallId": id,
                    "actionId": action_id,
                    "name": name,
                    "arguments": input,
                }),
            )?;
        }

        let output = match decision {
            PermissionDecision::Allow => {
                match self
                    .toolkit
                    .call_with_context(name, input.clone(), &tool_ctx)
                    .await
                {
                    Ok(out) => out,
                    // 挂起：工具要求外部执行（对齐 Java ToolSuspendException 语义）；
                    // 完整等待回路待 HITL 批次接入
                    Err(Error::ToolSuspended(msg)) => ToolOutput::suspended(msg),
                    Err(e) => ToolOutput::error(e.to_string()),
                }
            }
            PermissionDecision::Deny(reason) => {
                ToolOutput::denied(format!("permission denied: {reason}"))
            }
            // 正常流程下 RequireApproval 在 acting 预裁决时已被拦截挂起，
            // 不会走到这里；保留分支仅为 match 穷尽（防御性兜底）。
            PermissionDecision::RequireApproval => ToolOutput::denied(
                "permission denied: user approval required",
            ),
        };
        // 熔断提示：模型在原地打转（与上一轮完全相同的调用）且这次又失败时，
        // 在结果里明确要求换策略——否则「同样思考→同样调用→同样失败」会烧满
        // max_iters，用户看到的就是一串重复的思考卡片。
        let mut output = output;
        if repeated_call && output.state == crate::message::ToolResultState::Error {
            output.content.push_str(
                "

[system notice] This tool call is identical to your previous one and failed again. Do not retry the same call. Change the approach (different tool, different arguments, or gather more context first), or briefly explain the blocker to the user and stop.",
            );
        }
        let result_state = output.state;

        // ToolResult 三段事件：Start → 文本增量 → End(state)
        // （ToolOutput 当前为纯文本；多模态工具输出接入后在此分派 DataDelta）
        ctx.emit(AgentEventKind::ToolResultStart {
            reply_id: ctx.reply_id.clone(),
            tool_call_id: id.into(),
            tool_call_name: name.into(),
        })
        .await;
        ctx.emit(AgentEventKind::ToolResultTextDelta {
            reply_id: ctx.reply_id.clone(),
            tool_call_id: id.into(),
            tool_call_name: name.into(),
            delta: output.content.clone(),
        })
        .await;
        ctx.emit(AgentEventKind::ToolResultEnd {
            reply_id: ctx.reply_id.clone(),
            tool_call_id: id.into(),
            tool_call_name: name.into(),
            state: result_state,
        })
        .await;

        // 动作收尾：把结果块挂到动作上，重放据此清除「结果未知」
        let result_block = ContentBlock::ToolResult {
            id: id.to_string(),
            name: name.to_string(),
            output: vec![ContentBlock::Text {
                text: output.content.clone(),
            }],
            state: result_state,
            execution_details: output.execution_details.clone(),
        };
        ctx.record(
            "action/end",
            serde_json::json!({ "toolCallId": id, "result": result_block }),
        )?;

        let result = Msg::tool_result(&self.name, id, name, output.content, result_state);
        ctx.push(result);
        Ok(())
    }
}

/// 提取输入消息里的权限确认结果（metadata `agentscope_confirm_results`，
/// 对齐 Java `Msg.METADATA_CONFIRM_RESULTS` + `extractConfirmResults`）。
fn extract_confirm_results(msgs: &[Msg]) -> Vec<ConfirmResult> {
    let mut collected = Vec::new();
    for msg in msgs {
        let Some(raw) = msg
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.as_object())
            .and_then(|object| object.get(crate::message::metadata_keys::CONFIRM_RESULTS))
        else {
            continue;
        };
        match serde_json::from_value::<Vec<ConfirmResult>>(raw.clone()) {
            Ok(results) => collected.extend(results),
            Err(_) => continue,
        }
    }
    collected
}

/// 消息是否携带权限确认结果（这类消息是「对未决请求的答复」，不进对话历史；
/// 与 `session::execution` 的同名判定同源）。
fn has_confirm_results(message: &Msg) -> bool {
    message
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.as_object())
        .map(|object| object.contains_key(crate::message::metadata_keys::CONFIRM_RESULTS))
        .unwrap_or(false)
}

#[async_trait::async_trait]
impl Agent for ReActAgent {
    async fn call(&self, msg: Msg) -> Result<Msg> {
        self.run(RuntimeContext::new(), vec![msg], None, false, None)
            .await
    }

    async fn stream_events(&self, msg: Msg) -> Result<mpsc::Receiver<AgentEvent>> {
        self.stream_events_with_context(RuntimeContext::new(), msg)
            .await
    }
}

impl ReActAgent {
    /// 带运行时上下文执行（会话标识、用户标识、调用级属性由调用方提供）。
    pub async fn call_with_context(&self, runtime: RuntimeContext, msg: Msg) -> Result<Msg> {
        self.run(runtime, vec![msg], None, false, None).await
    }

    /// 带运行时上下文执行并推送事件流。
    pub async fn stream_events_with_context(
        &self,
        runtime: RuntimeContext,
        msg: Msg,
    ) -> Result<mpsc::Receiver<AgentEvent>> {
        let (tx, rx) = mpsc::channel::<AgentEvent>(32);
        let this = self.clone();
        tokio::spawn(async move {
            // 出错时仅关闭 channel：错误语义由 call() 承担，事件流只负责过程呈现
            let _ = this.run(runtime, vec![msg], Some(tx), true, None).await;
        });
        Ok(rx)
    }

    /// 消费一条收件箱命令并驱动执行（对应 Java harness 把 `SessionExecution`
    /// 挂进 RuntimeContext 后调 `prepareRun`）。
    ///
    /// 命令须先经 [`crate::session::SessionInbox`] 接受；本方法在持有写者租约时
    /// 校验并消费它：`submit` 开新回合、`resume`/`respond` 续跑未终结回合。
    /// `msgs` 是并入对话的输入（对应 Java `prepareRun` 的消息列表）——`resume`
    /// 续跑时传空列表，原始输入已在上一轮写进对话真相，重放会造成重复。
    pub async fn call_with_execution(
        &self,
        execution: SessionExecution,
        runtime: RuntimeContext,
        msgs: Vec<Msg>,
    ) -> Result<Msg> {
        self.run(
            runtime,
            msgs,
            None,
            false,
            Some(Arc::new(Mutex::new(execution))),
        )
        .await
    }

    /// 消费收件箱命令执行并推送事件流（[`Self::call_with_execution`] 的事件版）。
    pub async fn stream_events_with_execution(
        &self,
        execution: SessionExecution,
        runtime: RuntimeContext,
        msgs: Vec<Msg>,
    ) -> Result<mpsc::Receiver<AgentEvent>> {
        let (tx, rx) = mpsc::channel::<AgentEvent>(32);
        let this = self.clone();
        tokio::spawn(async move {
            let _ = this
                .run(
                    runtime,
                    msgs,
                    Some(tx),
                    true,
                    Some(Arc::new(Mutex::new(execution))),
                )
                .await;
        });
        Ok(rx)
    }
}

impl ReActAgentBuilder {
    /// agent 名称（写入消息的 name 字段）。
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// 系统提示词。
    pub fn sys_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.sys_prompt = prompt.into();
        self
    }

    /// 模型实例（如 `agentscope-model-openai` 的 `OpenAIChatModel`）。
    pub fn model(mut self, model: Arc<dyn Model>) -> Self {
        self.model = Some(model);
        self
    }

    /// 工具集（`Toolkit` 或 `Arc<Toolkit>`）。
    pub fn toolkit(mut self, toolkit: impl Into<Arc<Toolkit>>) -> Self {
        self.toolkit = toolkit.into();
        self
    }

    /// 1.x 记忆实现（如 `Arc<InMemoryMemory>`）。
    ///
    /// **兼容用途**：对话真相永远是 [`AgentState`]，这里配置的对象只会在每次调用
    /// 结束后收到一份历史镜像，好让仍按旧习惯读 `memory` 的代码不至于失效。
    /// 新代码请用 [`ReActAgent::state`] 或直接读状态库/会话日志。
    pub fn memory(mut self, memory: Arc<dyn Memory>) -> Self {
        self.memory = Some(memory);
        self
    }

    /// 状态存储（对应 Java `stateStore`）：`LEGACY` 模式下整份 `AgentState` 的持久化后端。
    pub fn state_store(mut self, store: Arc<dyn AgentStateStore>) -> Self {
        self.state_store = Some(store);
        self
    }

    /// 会话日志存储（对应 Java `sessionLogStore`）：`EVENT_LOG` 模式下的事实来源。
    pub fn session_log_store(mut self, store: Arc<dyn SessionLogStore>) -> Self {
        self.session_log_store = Some(store);
        self
    }

    /// 历史来源（对应 Java `SessionHistoryMode`，默认 `EVENT_LOG`）。
    ///
    /// 只配了一种后端时会被自动纠正：只配状态库 → `LEGACY`，只配日志库 → `EVENT_LOG`；
    /// 两种都配才按这里的设置执行（混用是不允许的）。
    pub fn history_mode(mut self, mode: SessionHistoryMode) -> Self {
        self.history_mode = mode;
        self
    }

    /// 调用方没提供 session id 时的兜底会话（对应 Java `defaultSessionId`）。
    ///
    /// 不设置时 build 会生成一个随机 id：单进程内多次 `call()` 共享同一会话，
    /// 不同 agent 实例之间互不干扰。
    pub fn default_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.default_session_id = Some(session_id.into());
        self
    }

    /// 注册中间件（可多次调用，按注册顺序执行）。
    pub fn middleware(mut self, middleware: Arc<dyn Middleware>) -> Self {
        self.middlewares.push(middleware);
        self
    }

    /// 权限检查器（默认全部放行）。
    pub fn permission(mut self, permission: Arc<dyn PermissionChecker>) -> Self {
        self.permission = permission;
        self
    }

    /// 新会话的初始权限上下文（对应 Java builder `permissionContext`）。
    /// 规则表非空或模式非 Default 时，工具执行路径启用 PermissionEngine 门控。
    pub fn permission_context(mut self, context: PermissionContextState) -> Self {
        self.permission_context = Some(context);
        self
    }

    /// 迭代边界异步钩子（见 [`IterationHook`]）：每轮 ReAct 迭代开始前、
    /// reasoning 之前调用；模型报上下文溢出时以 `forced=true` 再调一次并重试本轮。
    pub fn iteration_hook(mut self, hook: IterationHook) -> Self {
        self.iteration_hook = Some(hook);
        self
    }

    /// ReAct 循环最大迭代次数（默认 10）。
    pub fn max_iters(mut self, max_iters: usize) -> Self {
        self.max_iters = max_iters;
        self
    }

    /// 构建 agent。只有 `model` 是必填项。
    pub fn build(self) -> Result<ReActAgent> {
        let model = self
            .model
            .ok_or_else(|| Error::InvalidInput("model is required".into()))?;
        // 历史来源必须与配置的后端一致：只配一种后端时自动纠正，否则状态会
        // 「写不进去也读不出来」——这种静默丢数据比直接报错难查得多。
        let history_mode = match (self.state_store.is_some(), self.session_log_store.is_some()) {
            (true, false) => SessionHistoryMode::Legacy,
            (false, true) => SessionHistoryMode::EventLog,
            _ => self.history_mode,
        };
        let default_session_id = self
            .default_session_id
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        Ok(ReActAgent {
            name: self.name,
            sys_prompt: self.sys_prompt,
            model,
            toolkit: self.toolkit,
            memory: self.memory,
            state_store: self.state_store,
            session_log_store: self.session_log_store,
            history_mode,
            default_session_id: Some(default_session_id),
            slots: Arc::new(Mutex::new(HashMap::new())),
            steers: Arc::new(Mutex::new(HashMap::new())),
            middlewares: MiddlewareChain::new(self.middlewares),
            permission: self.permission,
            permission_context: self.permission_context,
            max_iters: self.max_iters,
            iteration_hook: self.iteration_hook,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::InMemoryMemory;
    use crate::message::ToolResultState;
    use crate::message::Role;
    use crate::middleware::FinalAnswerFilter;
    use crate::tool::{Tool, ToolGroupScope, Toolkit};
    use serde_json::{json, Value};
    use std::sync::Mutex;

    /// 预置响应序列的 Mock 模型：按序弹出响应。
    struct MockModel {
        responses: Mutex<Vec<crate::model::ChatResponse>>,
    }

    #[async_trait::async_trait]
    impl Model for MockModel {
        async fn call(&self, _request: ChatRequest) -> Result<crate::model::ChatResponse> {
            self.responses
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| Error::Model("no more mock responses".into()))
        }
    }

    struct AddTool;

    #[async_trait::async_trait]
    impl Tool for AddTool {
        fn name(&self) -> &str {
            "add"
        }
        fn description(&self) -> &str {
            "两数相加"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}})
        }
        async fn call(&self, input: Value) -> Result<ToolOutput> {
            Ok(ToolOutput::ok((input["a"].as_f64().unwrap() + input["b"].as_f64().unwrap()).to_string()))
        }
    }

    /// 记录每次请求工具名的 Mock 模型（用于验证会话激活集影响模型可见工具）。
    struct RecordingModel {
        responses: Mutex<Vec<crate::model::ChatResponse>>,
        requests: Arc<Mutex<Vec<Vec<String>>>>,
    }

    #[async_trait::async_trait]
    impl Model for RecordingModel {
        async fn call(&self, request: ChatRequest) -> Result<crate::model::ChatResponse> {
            self.requests
                .lock()
                .unwrap()
                .push(request.options.tools.iter().map(|t| t.name.clone()).collect());
            self.responses
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| Error::Model("no more mock responses".into()))
        }
    }

    struct WebSearchTool;

    #[async_trait::async_trait]
    impl Tool for WebSearchTool {
        fn name(&self) -> &str {
            "web_search"
        }
        fn description(&self) -> &str {
            "联网检索"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object", "properties": {"q": {"type": "string"}}})
        }
        async fn call(&self, _input: Value) -> Result<ToolOutput> {
            Ok(ToolOutput::ok("results"))
        }
    }

    fn agent_with(responses: Vec<crate::model::ChatResponse>) -> ReActAgent {
        let mut toolkit = Toolkit::new();
        toolkit.register(Arc::new(AddTool));
        ReActAgent::builder()
            .name("assistant")
            .sys_prompt("你是计算助手")
            .model(Arc::new(MockModel { responses: Mutex::new(responses) }))
            .toolkit(toolkit)
            .memory(Arc::new(InMemoryMemory::new()))
            .max_iters(5)
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn react_loop_with_tool_call() {
        // 第一轮：模型要求调用 add(1, 2)；第二轮：基于工具结果给出最终回复
        let agent = agent_with(vec![
            crate::model::ChatResponse::from_text("1 加 2 等于 3"),
            crate::model::ChatResponse {
                content: vec![
                    ContentBlock::Text { text: "我来算一下".into() },
                    ContentBlock::ToolUse {
                        id: "call_1".into(),
                        name: "add".into(),
                        input: json!({"a": 1, "b": 2}),
                        state: None,
                        content: None,
                    },
                ],
                ..Default::default()
            },
        ]);

        let reply = agent.call(Msg::user("user", "1加2等于几？")).await.unwrap();
        assert_eq!(reply.text(), "1 加 2 等于 3");

        // 对话真相里应有 4 条：user / assistant(tool_use) / tool / assistant(final)
        let history = history(&agent);
        assert_eq!(history.len(), 4);
        assert!(history[1].has_tool_use());
        assert!(matches!(
            history[2].content[0],
            ContentBlock::ToolResult { ref output, state: ToolResultState::Success, .. }
                if matches!(output.as_slice(), [ContentBlock::Text { text }] if text == "3")
        ));
    }

    #[tokio::test]
    async fn direct_answer_without_tool() {
        let agent = agent_with(vec![crate::model::ChatResponse::from_text("直接回答")]);
        let reply = agent.call(Msg::user("user", "你好")).await.unwrap();
        assert_eq!(reply.text(), "直接回答");
        assert_eq!(history(&agent).len(), 2);
    }

    #[tokio::test]
    async fn empty_response_retries_once_then_answers() {
        // 第一轮：模型只产出 thinking、正文为空且无工具调用（空响应）——
        // 注入提示重试；第二轮：正常回答。最终回复不能是空。
        // 注意 MockModel 用 Vec::pop（LIFO）：序列按调用逆序书写。
        let agent = agent_with(vec![
            crate::model::ChatResponse::from_text("这次有正文了"),
            crate::model::ChatResponse {
                content: vec![ContentBlock::Thinking {
                    thinking: "只想不说".into(),
                }],
                ..Default::default()
            },
        ]);
        let reply = agent.call(Msg::user("user", "你好")).await.unwrap();
        assert_eq!(reply.text(), "这次有正文了");
        // 空响应的 assistant 不入历史：user / system(重试提示) / assistant(最终)
        let history = history(&agent);
        assert_eq!(history.len(), 3);
        assert_eq!(history[1].role, Role::User);
        assert!(history[1].text().contains("没有正文内容"));
        assert_eq!(history[2].text(), "这次有正文了");
    }

    #[tokio::test]
    async fn empty_response_twice_ends_with_empty_result() {
        // 两次都空：不再无限重试，回合以空正文结束（前端有占位兜底）。
        let empty = crate::model::ChatResponse {
            content: vec![ContentBlock::Thinking {
                thinking: "只想不说".into(),
            }],
            ..Default::default()
        };
        // 两个响应相同，pop 顺序无影响。
        let agent = agent_with(vec![empty.clone(), empty]);
        let reply = agent.call(Msg::user("user", "你好")).await.unwrap();
        assert_eq!(reply.text(), "");
        // user / system(重试提示) / assistant(空，第二次仍空照常结束)
        assert_eq!(history(&agent).len(), 3);
    }

    #[tokio::test]
    async fn max_iterations_error() {
        // 模型永远要求调用工具 → 耗尽 max_iters
        let tool_use = crate::model::ChatResponse {
            content: vec![ContentBlock::ToolUse {
                id: "call_x".into(),
                name: "add".into(),
                input: json!({"a": 1, "b": 2}),
                state: None,
                content: None,
            }],
            ..Default::default()
        };
        let agent = agent_with(vec![tool_use.clone(), tool_use.clone(), tool_use.clone(), tool_use.clone(), tool_use]);
        let err = agent.call(Msg::user("user", "1加2")).await.unwrap_err();
        assert!(matches!(err, Error::MaxIterations(5)));
    }

    #[tokio::test]
    async fn stream_events_flow() {
        let agent = agent_with(vec![
            crate::model::ChatResponse::from_text("最终回复"),
            crate::model::ChatResponse {
                content: vec![ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "add".into(),
                    input: json!({"a": 1, "b": 2}),
                    state: None,
                    content: None,
                }],
                ..Default::default()
            },
        ]);

        let mut rx = agent.stream_events(Msg::user("user", "1加2")).await.unwrap();
        let mut kinds = Vec::new();
        while let Some(event) = rx.recv().await {
            kinds.push(match event.kind {
                AgentEventKind::AgentStart { .. } => "agent_start",
                AgentEventKind::AgentEnd { .. } => "agent_end",
                AgentEventKind::AgentResult { .. } => "agent_result",
                AgentEventKind::ModelCallStart { .. } => "model_call_start",
                AgentEventKind::ModelCallEnd { .. } => "model_call_end",
                AgentEventKind::TextBlockStart { .. } => "text_block_start",
                AgentEventKind::TextBlockDelta { .. } => "text_block_delta",
                AgentEventKind::TextBlockEnd { .. } => "text_block_end",
                AgentEventKind::ToolCallStart { .. } => "tool_call_start",
                AgentEventKind::ToolCallDelta { .. } => "tool_call_delta",
                AgentEventKind::ToolCallEnd { .. } => "tool_call_end",
                AgentEventKind::ToolResultStart { .. } => "tool_result_start",
                AgentEventKind::ToolResultTextDelta { .. } => "tool_result_text_delta",
                AgentEventKind::ToolResultEnd { .. } => "tool_result_end",
                other => unreachable!("unexpected event in flow test: {other:?}"),
            });
        }
        // MockModel 走 call_stream 默认降级（整块单发）：
        // 第一轮 ToolUse 响应 → ToolCall 三段；第二轮纯文本 → TextBlock 三段
        assert_eq!(
            kinds,
            vec![
                "agent_start",
                "model_call_start",
                "tool_call_start",
                "tool_call_delta",
                "tool_call_end",
                "model_call_end",
                "tool_result_start",
                "tool_result_text_delta",
                "tool_result_end",
                "model_call_start",
                "text_block_start",
                "text_block_delta",
                "text_block_end",
                "model_call_end",
                "agent_result",
                "agent_end",
            ]
        );
    }

    /// replyId 贯穿：同一次执行的所有事件共享同一 reply_id。
    #[tokio::test]
    async fn reply_id_shared_across_events() {
        let agent = agent_with(vec![crate::model::ChatResponse::from_text("直接回答")]);
        let mut rx = agent.stream_events(Msg::user("user", "你好")).await.unwrap();
        let mut reply_ids = Vec::new();
        while let Some(event) = rx.recv().await {
            if let AgentEventKind::ModelCallStart { reply_id } = event.kind {
                reply_ids.push(reply_id);
            }
        }
        assert_eq!(reply_ids.len(), 1);
    }

    /// 思考块流：Thinking 块经默认 call_stream 降级映射为 ThinkingDelta，
    /// 聚合器产出 ThinkingBlock 三段事件且先于文本块（对齐 Java chunk
    /// 处理顺序 Thinking → Text），对话真相保留思考块。
    #[tokio::test]
    async fn stream_events_thinking_block_flow() {
        let agent = agent_with(vec![crate::model::ChatResponse {
            content: vec![
                ContentBlock::Thinking { thinking: "先想一想".into() },
                ContentBlock::Text { text: "答案是 3".into() },
            ],
            ..Default::default()
        }]);

        let mut rx = agent
            .stream_events(Msg::user("user", "1加2等于几？"))
            .await
            .unwrap();
        let mut kinds = Vec::new();
        while let Some(event) = rx.recv().await {
            kinds.push(match event.kind {
                AgentEventKind::AgentStart { .. } => "agent_start",
                AgentEventKind::AgentEnd { .. } => "agent_end",
                AgentEventKind::AgentResult { .. } => "agent_result",
                AgentEventKind::ModelCallStart { .. } => "model_call_start",
                AgentEventKind::ModelCallEnd { .. } => "model_call_end",
                AgentEventKind::ThinkingBlockStart { .. } => "thinking_start",
                AgentEventKind::ThinkingBlockDelta { .. } => "thinking_delta",
                AgentEventKind::ThinkingBlockEnd { .. } => "thinking_end",
                AgentEventKind::TextBlockStart { .. } => "text_start",
                AgentEventKind::TextBlockDelta { .. } => "text_delta",
                AgentEventKind::TextBlockEnd { .. } => "text_end",
                other => unreachable!("unexpected event in thinking flow test: {other:?}"),
            });
        }
        assert_eq!(
            kinds,
            vec![
                "agent_start",
                "model_call_start",
                "thinking_start",
                "thinking_delta",
                "thinking_end",
                "text_start",
                "text_delta",
                "text_end",
                "model_call_end",
                "agent_result",
                "agent_end",
            ]
        );

        // 对话真相保留思考块（存于记忆但不回传 LLM——formatter 跳过 Thinking）
        let history = history(&agent);
        assert!(matches!(
            &history[1].content[0],
            ContentBlock::Thinking { thinking } if thinking == "先想一想"
        ));
    }

    // ---------- middleware / permission ----------

    /// 记录扩展点调用轨迹的中间件（新 Middleware API）
    struct TracingMiddleware {
        name: &'static str,
        order: i32,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl TracingMiddleware {
        fn new(name: &'static str, order: i32, log: Arc<Mutex<Vec<String>>>) -> Arc<Self> {
            Arc::new(Self { name, order, log })
        }
        fn record(&self, what: &str) {
            self.log.lock().unwrap().push(format!("{}:{}", self.name, what));
        }
    }

    impl Middleware for TracingMiddleware {
        fn name(&self) -> &str {
            self.name
        }
        fn order(&self) -> i32 {
            self.order
        }
        fn on_agent(&self, _ctx: &RuntimeContext, input: AgentInput) -> AgentInput {
            self.record("on_agent");
            input
        }
        fn on_reasoning(&self, _ctx: &RuntimeContext, input: ReasoningInput) -> ReasoningInput {
            self.record("on_reasoning");
            input
        }
        fn on_system_prompt(&self, _ctx: &RuntimeContext, current: String) -> String {
            self.record("on_system_prompt");
            format!("{current}[{}]", self.name)
        }
    }

    struct DenyAll;

    #[async_trait::async_trait]
    impl crate::permission::PermissionChecker for DenyAll {
        async fn check(
            &self,
            _tool_name: &str,
            _input: &Value,
        ) -> crate::permission::PermissionDecision {
            crate::permission::PermissionDecision::Deny("tool is not allowed".into())
        }
    }

    /// 捕获每次请求消息的模型
    struct CapturingModel {
        seen: Arc<Mutex<Vec<Vec<Msg>>>>,
        response: Mutex<Option<crate::model::ChatResponse>>,
    }

    #[async_trait::async_trait]
    impl Model for CapturingModel {
        async fn call(&self, request: ChatRequest) -> Result<crate::model::ChatResponse> {
            self.seen.lock().unwrap().push(request.messages);
            self.response
                .lock()
                .unwrap()
                .take()
                .ok_or_else(|| Error::Model("no more mock responses".into()))
        }
    }

    fn build_agent(model: Arc<dyn Model>) -> ReActAgentBuilder {
        ReActAgent::builder()
            .name("assistant")
            .sys_prompt("base prompt")
            .model(model)
            .memory(Arc::new(InMemoryMemory::new()))
    }

    #[tokio::test]
    async fn middleware_applies_higher_order_first() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let agent = build_agent(Arc::new(MockModel {
            responses: Mutex::new(vec![crate::model::ChatResponse::from_text("ok")]),
        }))
        .middleware(TracingMiddleware::new("low", 1, log.clone()))
        .middleware(TracingMiddleware::new("high", 5, log.clone()))
        .build()
        .unwrap();

        agent.call(Msg::user("user", "hi")).await.unwrap();
        assert_eq!(
            *log.lock().unwrap(),
            vec![
                "high:on_agent",
                "low:on_agent",
                "high:on_system_prompt",
                "low:on_system_prompt",
                "high:on_reasoning",
                "low:on_reasoning",
            ],
            "order 大者在外层：先于更小的应用"
        );
    }

    #[tokio::test]
    async fn middleware_can_rewrite_prompt_and_reasoning_input() {
        struct Injector;
        impl Middleware for Injector {
            fn name(&self) -> &str {
                "injector"
            }
            fn on_system_prompt(&self, _ctx: &RuntimeContext, current: String) -> String {
                format!("{current} plus")
            }
            fn on_reasoning(&self, _ctx: &RuntimeContext, mut input: ReasoningInput) -> ReasoningInput {
                input.messages.push(Msg::user("system", "injected hint"));
                input
            }
        }

        let seen = Arc::new(Mutex::new(Vec::new()));
        let agent = build_agent(Arc::new(CapturingModel {
            seen: seen.clone(),
            response: Mutex::new(Some(crate::model::ChatResponse::from_text("ok"))),
        }))
        .middleware(Arc::new(Injector))
        .build()
        .unwrap();

        agent.call(Msg::user("user", "hi")).await.unwrap();
        let requests = seen.lock().unwrap();
        let messages = &requests[0];
        assert_eq!(messages[0].text(), "base prompt plus", "system prompt 管道生效");
        assert_eq!(messages.last().unwrap().text(), "injected hint", "注入消息位于末尾");
    }

    #[tokio::test]
    async fn middleware_can_replace_model_in_model_call() {
        struct SwapModel(Arc<dyn Model>);
        impl Middleware for SwapModel {
            fn name(&self) -> &str {
                "swap-model"
            }
            fn on_model_call(&self, _ctx: &RuntimeContext, mut input: ModelCallInput) -> ModelCallInput {
                input.model = self.0.clone();
                input
            }
        }

        let original = Arc::new(MockModel {
            responses: Mutex::new(vec![crate::model::ChatResponse::from_text("original")]),
        });
        let replacement = Arc::new(MockModel {
            responses: Mutex::new(vec![crate::model::ChatResponse::from_text("replaced")]),
        });
        let agent = build_agent(original)
            .middleware(Arc::new(SwapModel(replacement)))
            .build()
            .unwrap();

        assert_eq!(agent.call(Msg::user("user", "hi")).await.unwrap().text(), "replaced");
    }

    #[tokio::test]
    async fn runtime_context_flows_into_middleware_and_agent_start() {
        struct SessionProbe(Arc<Mutex<Vec<Option<String>>>>);
        impl Middleware for SessionProbe {
            fn name(&self) -> &str {
                "session-probe"
            }
            fn on_agent(&self, ctx: &RuntimeContext, input: AgentInput) -> AgentInput {
                self.0.lock().unwrap().push(ctx.session_id().map(str::to_string));
                ctx.put("probe", json!(true));
                input
            }
        }

        let probe = Arc::new(Mutex::new(Vec::new()));
        let agent = build_agent(Arc::new(MockModel {
            responses: Mutex::new(vec![crate::model::ChatResponse::from_text("ok")]),
        }))
        .middleware(Arc::new(SessionProbe(probe.clone())))
        .build()
        .unwrap();

        let runtime = RuntimeContext::new().with_session_id("session-42").with_user_id("u1");
        agent.call_with_context(runtime, Msg::user("user", "hi")).await.unwrap();

        assert_eq!(probe.lock().unwrap()[0].as_deref(), Some("session-42"));
    }

    #[tokio::test]
    async fn agent_start_carries_session_id() {
        let agent = build_agent(Arc::new(MockModel {
            responses: Mutex::new(vec![crate::model::ChatResponse::from_text("ok")]),
        }))
        .build()
        .unwrap();

        let runtime = RuntimeContext::new().with_session_id("session-7");
        let mut rx = agent
            .stream_events_with_context(runtime, Msg::user("user", "hi"))
            .await
            .unwrap();
        let mut seen = None;
        while let Some(event) = rx.recv().await {
            if let AgentEventKind::AgentStart { session_id, .. } = event.kind {
                seen = session_id;
            }
        }
        assert_eq!(seen.as_deref(), Some("session-7"));
    }

    #[tokio::test]
    async fn final_answer_filter_suppresses_intermediate_round_text() {
        // 第一轮：文本 + 工具调用（中间产物）；第二轮：纯文本最终回复
        let agent = build_agent(Arc::new(MockModel {
            responses: Mutex::new(vec![
                crate::model::ChatResponse::from_text("最终回复"),
                crate::model::ChatResponse {
                    content: vec![
                        ContentBlock::Text { text: "让我算一下".into() },
                        ContentBlock::ToolUse {
                            id: "call_1".into(),
                            name: "add".into(),
                            input: json!({"a": 1, "b": 2}),
                            state: None,
                            content: None,
                        },
                    ],
                    ..Default::default()
                },
            ]),
        }))
        .toolkit({
            let mut tk = Toolkit::new();
            tk.register(Arc::new(AddTool));
            tk
        })
        .middleware(Arc::new(FinalAnswerFilter))
        .build()
        .unwrap();

        let mut rx = agent.stream_events(Msg::user("user", "1加2")).await.unwrap();
        let mut deltas = Vec::new();
        while let Some(event) = rx.recv().await {
            if let AgentEventKind::TextBlockDelta { delta, .. } = event.kind {
                deltas.push(delta);
            }
        }
        assert_eq!(deltas, vec!["最终回复"], "中间轮的文本必须被抑制");
    }

    #[tokio::test]
    async fn permission_deny_returns_error_to_model() {
        let mut toolkit = Toolkit::new();
        toolkit.register(Arc::new(AddTool));
        let agent = ReActAgent::builder()
            .name("assistant")
            .model(Arc::new(MockModel {
                responses: Mutex::new(vec![
                    crate::model::ChatResponse::from_text("好的，不调用了"),
                    crate::model::ChatResponse {
                        content: vec![ContentBlock::ToolUse {
                            id: "call_1".into(),
                            name: "add".into(),
                            input: json!({"a": 1, "b": 2}),
                            state: None,
                            content: None,
                        }],
                        ..Default::default()
                    },
                ]),
            }))
            .toolkit(toolkit)
            .memory(Arc::new(InMemoryMemory::new()))
            .permission(Arc::new(DenyAll))
            .build()
            .unwrap();

        let reply = agent.call(Msg::user("user", "1加2")).await.unwrap();
        assert_eq!(reply.text(), "好的，不调用了");

        // 被拒绝的工具调用以 denied 状态结果回传给模型（对齐 Java ToolResultState.DENIED）
        let history = history(&agent);
        assert!(
            matches!(history[2].content[0],
                ContentBlock::ToolResult { ref output, state: ToolResultState::Denied, .. }
                if output.iter().any(|b| matches!(b, ContentBlock::Text { text } if text.contains("denied"))))
        );
    }

    #[tokio::test]
    async fn tool_error_does_not_break_loop() {
        // 模型调用未注册工具 → 错误结果回传 → 模型给出最终回复
        let agent = agent_with(vec![
            crate::model::ChatResponse::from_text("好的，那个工具不存在"),
            crate::model::ChatResponse {
                content: vec![ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "not_registered".into(),
                    input: json!({}),
                    state: None,
                    content: None,
                }],
                ..Default::default()
            },
        ]);
        let reply = agent.call(Msg::user("user", "调用一下")).await.unwrap();
        assert_eq!(reply.text(), "好的，那个工具不存在");

        let history = history(&agent);
        assert!(matches!(
            history[2].content[0],
            ContentBlock::ToolResult { state: ToolResultState::Error, .. }
        ));
    }

    /// 对话真相：`AgentState.context`。
    fn history(agent: &ReActAgent) -> Vec<Msg> {
        agent
            .state()
            .unwrap()
            .expect("agent 总有默认会话")
            .context
    }

    #[test]
    fn build_requires_model() {
        // 只有 model 是必填项：状态后端可以都不配（纯进程内）
        let err = ReActAgent::builder().build().unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)));
        assert!(ReActAgent::builder()
            .model(Arc::new(MockModel { responses: Mutex::new(Vec::new()) }))
            .build()
            .is_ok());
    }

    /// 元工具改变模型可见工具：第一轮调用 reset_equipped_tools 装备 web 组，
    /// 第二轮请求的工具列表即包含该组工具（会话激活集生效）。
    #[tokio::test]
    async fn meta_tool_changes_visible_tools_between_turns() {
        let mut toolkit = Toolkit::new();
        toolkit.register(Arc::new(AddTool));
        toolkit.register(Arc::new(WebSearchTool));
        toolkit.create_group("web", "联网检索", false, ToolGroupScope::Meta);
        toolkit.add_tool_to_group("web", "web_search").unwrap();

        toolkit.register_meta_tool();

        let requests = Arc::new(Mutex::new(Vec::new()));
        let agent = ReActAgent::builder()
            .name("assistant")
            .sys_prompt("你是助手")
            .model(Arc::new(RecordingModel {
                responses: Mutex::new(vec![
                    // 后进先出：第二轮为最终回复
                    crate::model::ChatResponse::from_text("done"),
                    // 第一轮：模型要求装备 web 组
                    crate::model::ChatResponse {
                        content: vec![ContentBlock::ToolUse {
                            id: "call_1".into(),
                            name: "reset_equipped_tools".into(),
                            input: json!({"to_activate": ["web"]}),
                            state: None,
                            content: None,
                        }],
                        ..Default::default()
                    },
                ]),
                requests: requests.clone(),
            }))
            .toolkit(toolkit)
            .memory(Arc::new(InMemoryMemory::new()))
            .max_iters(5)
            .build()
            .unwrap();

        agent.call(Msg::user("user", "帮我查点资料")).await.unwrap();

        let reqs = requests.lock().unwrap();
        assert_eq!(reqs.len(), 2);
        // 第一轮：未分组工具 + 元工具可见，web 组工具不可见
        assert!(reqs[0].contains(&"add".to_string()));
        assert!(reqs[0].contains(&"reset_equipped_tools".to_string()));
        assert!(!reqs[0].contains(&"web_search".to_string()));
        // 第二轮：装备后 web_search 可见
        assert!(reqs[1].contains(&"web_search".to_string()));
    }

    // ---------- 状态接线（AgentState 作为对话真相） ----------

    use crate::session::store::{InMemorySessionLogStore, SessionLogStore};
    use crate::session::{SessionKey, SessionProjection};
    use crate::state::{AgentStateStore, InMemoryAgentStateStore};

    fn plain(responses: Vec<crate::model::ChatResponse>) -> ReActAgentBuilder {
        ReActAgent::builder()
            .name("assistant")
            .sys_prompt("你是助手")
            .model(Arc::new(MockModel {
                responses: Mutex::new(responses),
            }))
            .max_iters(5)
    }

    #[tokio::test]
    async fn repeated_calls_accumulate_history_in_one_slot() {
        let agent = plain(vec![
            crate::model::ChatResponse::from_text("第二轮回复"),
            crate::model::ChatResponse::from_text("第一轮回复"),
        ])
        .build()
        .unwrap();
        agent.call(Msg::user("user", "第一句")).await.unwrap();
        agent.call(Msg::user("user", "第二句")).await.unwrap();
        // 同一默认会话：两次调用的输入与回复都在同一份状态里
        let history = history(&agent);
        assert_eq!(history.len(), 4);
        assert_eq!(history[0].text(), "第一句");
        assert_eq!(history[3].text(), "第二轮回复");
    }

    #[tokio::test]
    async fn legacy_state_store_survives_agent_rebuild() {
        let store: Arc<dyn AgentStateStore> = Arc::new(InMemoryAgentStateStore::new());
        let build = |text: &str| {
            plain(vec![crate::model::ChatResponse::from_text(text)])
                .state_store(store.clone())
                .default_session_id("session-1")
                .build()
                .unwrap()
        };

        build("第一次回复")
            .call(Msg::user("user", "第一次输入"))
            .await
            .unwrap();

        // 新实例、同一状态库：历史从 store 读回（不是进程内缓存）
        let second = build("第二次回复");
        assert_eq!(history(&second).len(), 2);
        second.call(Msg::user("user", "第二次输入")).await.unwrap();
        assert_eq!(history(&second).len(), 4);
    }

    /// 有持久层时空闲槽位被顺带淘汰（slots 缓存不无限增长）；无持久层时永不清理。
    #[tokio::test]
    async fn slots_evict_idle_entries_only_when_persistent() {
        // 有持久层：s1 的槽位在 s2 调用时被淘汰
        let store: Arc<dyn AgentStateStore> = Arc::new(InMemoryAgentStateStore::new());
        let agent = plain(vec![
            crate::model::ChatResponse::from_text("回复二"),
            crate::model::ChatResponse::from_text("回复一"),
        ])
        .state_store(store.clone())
        .build()
        .unwrap();
        agent
            .call_with_context(
                RuntimeContext::new().with_session_id("s1"),
                Msg::user("user", "一"),
            )
            .await
            .unwrap();
        assert_eq!(agent.slots.lock().unwrap().len(), 1);
        agent
            .call_with_context(
                RuntimeContext::new().with_session_id("s2"),
                Msg::user("user", "二"),
            )
            .await
            .unwrap();
        {
            let slots = agent.slots.lock().unwrap();
            assert_eq!(slots.len(), 1, "s1 空闲槽位应被 s2 调用顺带淘汰");
            assert!(slots.contains_key(&ReActAgent::slot_key(None, "s2")));
        }

        // 无持久层：两个会话的槽位都保留（state 只活在缓存里，清理即丢历史）
        let plain_agent = plain(vec![
            crate::model::ChatResponse::from_text("回复二"),
            crate::model::ChatResponse::from_text("回复一"),
        ])
        .build()
        .unwrap();
        plain_agent
            .call_with_context(
                RuntimeContext::new().with_session_id("s1"),
                Msg::user("user", "一"),
            )
            .await
            .unwrap();
        plain_agent
            .call_with_context(
                RuntimeContext::new().with_session_id("s2"),
                Msg::user("user", "二"),
            )
            .await
            .unwrap();
        assert_eq!(plain_agent.slots.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn event_log_records_run_and_replays_context() {
        let logs = Arc::new(InMemorySessionLogStore::new());
        let build = |text: &str| {
            plain(vec![crate::model::ChatResponse::from_text(text)])
                .session_log_store(logs.clone())
                .default_session_id("session-1")
                .build()
                .unwrap()
        };

        build("第一次回复")
            .call(Msg::user("user", "第一次输入"))
            .await
            .unwrap();

        let key = SessionKey::new(None::<String>, "assistant", "session-1").unwrap();
        let log = logs.open(&key, None).unwrap();
        let kinds: Vec<String> = {
            let head = log.head().unwrap();
            log.scan(0, head.seq)
                .unwrap()
                .into_iter()
                .map(|event| event.kind)
                .collect()
        };
        assert_eq!(kinds.first().map(String::as_str), Some("migration/baseline"));
        assert!(kinds.contains(&"run/start".to_string()));
        assert!(kinds.contains(&"turn/output".to_string()));
        assert_eq!(kinds.last().map(String::as_str), Some("run/end"));
        // 没有工具调用 → 没有悬念
        let projection = SessionProjection::read(log.as_ref()).unwrap();
        assert!(projection.uncertain_tool_calls().is_empty());
        assert_eq!(projection.restore().unwrap().context.len(), 2);

        // 换实例续跑：历史从日志重放
        let second = build("第二次回复");
        assert_eq!(history(&second).len(), 2);
        second.call(Msg::user("user", "第二次输入")).await.unwrap();
        assert_eq!(history(&second).len(), 4);
    }

    #[tokio::test]
    async fn tool_call_leaves_no_unknown_outcome_in_log() {
        let logs = Arc::new(InMemorySessionLogStore::new());
        let mut toolkit = Toolkit::new();
        toolkit.register(Arc::new(AddTool));
        let agent = ReActAgent::builder()
            .name("assistant")
            .model(Arc::new(MockModel {
                responses: Mutex::new(vec![
                    crate::model::ChatResponse::from_text("算好了"),
                    crate::model::ChatResponse {
                        content: vec![ContentBlock::ToolUse {
                            id: "call_1".into(),
                            name: "add".into(),
                            input: json!({"a": 1, "b": 2}),
                            state: None,
                            content: None,
                        }],
                        ..Default::default()
                    },
                ]),
            }))
            .toolkit(toolkit)
            .session_log_store(logs.clone())
            .default_session_id("session-1")
            .max_iters(5)
            .build()
            .unwrap();
        agent.call(Msg::user("user", "1加2")).await.unwrap();

        let key = SessionKey::new(None::<String>, "assistant", "session-1").unwrap();
        let log = logs.open(&key, None).unwrap();
        let head = log.head().unwrap();
        let events = log.scan(0, head.seq).unwrap();
        let kinds: Vec<&str> = events.iter().map(|event| event.kind.as_str()).collect();
        // action/start 在 dispatch 之前，action/end 之后才清掉「结果未知」
        assert!(kinds.contains(&"action/start"));
        assert!(kinds.contains(&"tool/decision"));
        assert!(kinds.contains(&"tool/dispatch"));
        assert!(kinds.contains(&"action/end"));
        let start = kinds.iter().position(|k| *k == "action/start").unwrap();
        let dispatch = kinds.iter().position(|k| *k == "tool/dispatch").unwrap();
        let end = kinds.iter().position(|k| *k == "action/end").unwrap();
        assert!(start < dispatch && dispatch < end);

        let projection = SessionProjection::read(log.as_ref()).unwrap();
        assert!(projection.uncertain_tool_calls().is_empty());
    }

    #[tokio::test]
    async fn execution_submit_drives_and_closes_the_turn() {
        use crate::session::execution::SessionExecution;
        use crate::session::inbox::SessionInbox;
        use crate::session::{Command, SessionKey};

        let logs = Arc::new(InMemorySessionLogStore::new());
        let agent = plain(vec![crate::model::ChatResponse::from_text("回复")])
            .session_log_store(logs.clone())
            .default_session_id("session-1")
            .build()
            .unwrap();
        let key = SessionKey::new(None::<String>, "assistant", "session-1").unwrap();
        let log = logs.open(&key, None).unwrap();
        let inbox = SessionInbox::new(log.as_ref()).unwrap();
        let command = inbox
            .locked(|tx| {
                tx.accept(
                    Command::new("c1", "submit")
                        .with_turn("t1")
                        .with_messages(vec![Msg::user("user", "你好")]),
                )
            })
            .unwrap();

        let reply = agent
            .call_with_execution(
                SessionExecution::new(log.clone(), command),
                RuntimeContext::new().with_session_id("session-1"),
                vec![Msg::user("user", "你好")],
            )
            .await
            .unwrap();
        assert_eq!(reply.text(), "回复");
        // 回合已正常关闭
        assert!(inbox.snapshot().unwrap().turn_id.is_none());
        let kinds: Vec<String> = {
            let head = log.head().unwrap();
            log.scan(0, head.seq)
                .unwrap()
                .into_iter()
                .map(|event| event.kind)
                .collect()
        };
        assert!(kinds.contains(&"inbox/started".to_string()));
        // inbox/opened 与 inbox/closed 写在收件箱子日志（租约与主日志写者独立）
        let inbox_log = log.inbox().unwrap();
        let inbox_kinds: Vec<String> = {
            let head = inbox_log.head().unwrap();
            inbox_log
                .scan(0, head.seq)
                .unwrap()
                .into_iter()
                .map(|event| event.kind)
                .collect()
        };
        assert!(inbox_kinds.contains(&"inbox/opened".to_string()));
        assert!(inbox_kinds.contains(&"inbox/closed".to_string()));
        // 对话真相：输入 + 回复
        assert_eq!(history(&agent).len(), 2);
    }

    #[tokio::test]
    async fn execution_rejects_resume_of_unknown_turn_and_keeps_command() {
        use crate::session::execution::SessionExecution;
        use crate::session::inbox::SessionInbox;
        use crate::session::{Command, SessionKey};

        let logs = Arc::new(InMemorySessionLogStore::new());
        let agent = plain(vec![crate::model::ChatResponse::from_text("不该出现")])
            .session_log_store(logs.clone())
            .default_session_id("session-1")
            .build()
            .unwrap();
        let key = SessionKey::new(None::<String>, "assistant", "session-1").unwrap();
        let log = logs.open(&key, None).unwrap();
        let inbox = SessionInbox::new(log.as_ref()).unwrap();
        let command = inbox
            .locked(|tx| tx.accept(Command::new("c1", "resume").with_turn("t-missing")))
            .unwrap();

        let error = agent
            .call_with_execution(
                SessionExecution::new(log.clone(), command),
                RuntimeContext::new().with_session_id("session-1"),
                vec![],
            )
            .await
            .err()
            .unwrap();
        assert!(
            error.to_string().contains("unfinished"),
            "unexpected error: {error}"
        );
        // 校验失败时命令仍留在收件箱里（拒绝要由外部显式做），回合也没被打开
        let snapshot = inbox.snapshot().unwrap();
        assert_eq!(snapshot.commands.len(), 1);
        assert!(snapshot.turn_id.is_none());
    }

    /// 第一次模型调用时向收件箱投递转向指令（模拟运行中途用户转向）。
    use crate::session::inbox::SessionInbox;
    use crate::session::Command;

    struct SteerOnFirstCall {
        responses: Mutex<Vec<crate::model::ChatResponse>>,
        log: Arc<dyn SessionLog>,
        injected: Mutex<bool>,
    }

    #[async_trait::async_trait]
    impl Model for SteerOnFirstCall {
        async fn call(&self, _request: ChatRequest) -> Result<crate::model::ChatResponse> {
            {
                let mut injected = self.injected.lock().unwrap_or_else(|error| error.into_inner());
                if !*injected {
                    *injected = true;
                    drop(injected);
                    let inbox = SessionInbox::new(self.log.as_ref()).unwrap();
                    inbox
                        .locked(|tx| {
                            tx.accept(
                                Command::new("s1", "steer")
                                    .with_turn("t1")
                                    .with_messages(vec![Msg::user("user", "换个方向")]),
                            )
                        })
                        .unwrap();
                }
            }
            self.responses
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .pop()
                .ok_or_else(|| Error::Model("no more mock responses".into()))
        }
    }

    #[test]
    fn steer_dedup_detects_identical_pending_message() {
        let agent = ReActAgent::builder()
            .name("assistant")
            .sys_prompt("p")
            .model(Arc::new(MockModel { responses: Mutex::new(vec![]) }))
            .build()
            .unwrap();
        agent.push_steer("s1", Msg::user("user", "不要吞异常"));
        assert!(agent.has_duplicate_steer("s1", "不要吞异常"));
        assert!(!agent.has_duplicate_steer("s1", "别的插话"));
        assert!(!agent.has_duplicate_steer("s2", "不要吞异常"));
        // 取走后不再重复
        assert_eq!(agent.take_steers("s1").len(), 1);
        assert!(!agent.has_duplicate_steer("s1", "不要吞异常"));
    }

    #[tokio::test]
    async fn execution_steering_continues_the_turn() {
        use crate::session::execution::SessionExecution;
        use crate::session::SessionKey;

        let logs = Arc::new(InMemorySessionLogStore::new());
        let key = SessionKey::new(None::<String>, "assistant", "session-1").unwrap();
        let log = logs.open(&key, None).unwrap();
        // pop 从尾部弹出：第一轮拿到「第一版」，转向后的第二轮拿到「第二版」
        let agent = ReActAgent::builder()
            .name("assistant")
            .sys_prompt("你是助手")
            .model(Arc::new(SteerOnFirstCall {
                responses: Mutex::new(vec![
                    crate::model::ChatResponse::from_text("第二版回复"),
                    crate::model::ChatResponse::from_text("第一版回复"),
                ]),
                log: log.clone(),
                injected: Mutex::new(false),
            }))
            .session_log_store(logs.clone())
            .default_session_id("session-1")
            .max_iters(5)
            .build()
            .unwrap();
        let inbox = SessionInbox::new(log.as_ref()).unwrap();
        inbox
            .locked(|tx| tx.accept(Command::new("c1", "submit").with_turn("t1")))
            .unwrap();

        let reply = agent
            .call_with_execution(
                SessionExecution::new(log.clone(), Command::new("c1", "submit").with_turn("t1")),
                RuntimeContext::new().with_session_id("session-1"),
                vec![Msg::user("user", "你好")],
            )
            .await
            .unwrap();
        // 结算发现未消费的 steer → 继续一轮，最终回复是第二轮的
        assert_eq!(reply.text(), "第二版回复");
        assert!(inbox.snapshot().unwrap().turn_id.is_none());
        // steer 消息由下一轮 before_step 注入对话真相
        let context = history(&agent);
        assert_eq!(context.len(), 4);
        assert_eq!(context[2].text(), "换个方向");
    }

    #[tokio::test]
    async fn legacy_memory_still_receives_a_history_mirror() {
        let memory = Arc::new(InMemoryMemory::new());
        let agent = plain(vec![crate::model::ChatResponse::from_text("回复")])
            .memory(memory.clone())
            .build()
            .unwrap();
        agent.call(Msg::user("user", "输入")).await.unwrap();
        // 镜像只为兼容旧读法，真相同样完整
        let mirrored = memory.get_messages().await.unwrap();
        assert_eq!(mirrored.len(), 2);
        assert_eq!(mirrored[1].text(), "回复");
        assert_eq!(history(&agent).len(), 2);
    }

    /// 中间件通过 `on_agent_state_ready` 拿到的必须是真实状态：能读也能改。
    struct StateInjector;

    #[async_trait::async_trait]
    impl Middleware for StateInjector {
        fn name(&self) -> &str {
            "state-injector"
        }
        fn active_points(&self) -> &[crate::middleware::ExtensionPoint] {
            &[crate::middleware::ExtensionPoint::OnAgentStateReady]
        }
        fn on_agent_state_ready(
            &self,
            _ctx: &RuntimeContext,
            _input_msgs: &mut Vec<Msg>,
            state: &Arc<Mutex<crate::state::AgentState>>,
        ) {
            // 读：看到本次调用前的历史条数
            let existing = state.lock().unwrap().context.len();
            // 写：往上下文里塞一条提醒（对应 Java TaskReminderMiddleware）
            state
                .lock()
                .unwrap()
                .context
                .push(Msg::system("system", format!("提醒#{existing}")));
        }
    }

    #[tokio::test]
    async fn middleware_state_ready_sees_and_mutates_real_state() {
        let agent = plain(vec![
            crate::model::ChatResponse::from_text("第二次"),
            crate::model::ChatResponse::from_text("第一次"),
        ])
        .middleware(Arc::new(StateInjector))
        .build()
        .unwrap();

        agent.call(Msg::user("user", "第一次输入")).await.unwrap();
        agent.call(Msg::user("user", "第二次输入")).await.unwrap();

        let history = history(&agent);
        // 第一次：0 条历史 → 提醒#0；第二次：3 条（提醒+输入+回复）→ 提醒#3
        let reminders: Vec<String> = history
            .iter()
            .filter(|message| message.text().starts_with("提醒#"))
            .map(|message| message.text())
            .collect();
        assert_eq!(reminders, vec!["提醒#0".to_string(), "提醒#3".to_string()]);
    }

    /// 慢模型：每次调用先睡一会，用来暴露并发下的状态交错。
    struct SlowModel {
        delay: std::time::Duration,
        responses: Mutex<Vec<crate::model::ChatResponse>>,
    }

    #[async_trait::async_trait]
    impl Model for SlowModel {
        async fn call(&self, _request: ChatRequest) -> Result<crate::model::ChatResponse> {
            tokio::time::sleep(self.delay).await;
            self.responses
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| Error::Model("no more mock responses".into()))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn same_slot_calls_are_serialized() {
        let agent = Arc::new(
            ReActAgent::builder()
                .name("assistant")
                .sys_prompt("你是助手")
                .model(Arc::new(SlowModel {
                    delay: std::time::Duration::from_millis(30),
                    responses: Mutex::new(vec![
                        crate::model::ChatResponse::from_text("第二个回复"),
                        crate::model::ChatResponse::from_text("第一个回复"),
                    ]),
                }))
                .default_session_id("shared")
                .max_iters(3)
                .build()
                .unwrap(),
        );

        let a = {
            let agent = agent.clone();
            tokio::spawn(async move { agent.call(Msg::user("user", "第一句")).await })
        };
        let b = {
            let agent = agent.clone();
            tokio::spawn(async move { agent.call(Msg::user("user", "第二句")).await })
        };
        a.await.unwrap().unwrap();
        b.await.unwrap().unwrap();

        // 串行的判据：输入与回复严格成对，不出现「两次输入挤出两次回复」的交错
        let history = history(&agent);
        assert_eq!(history.len(), 4);
        assert_eq!(history[0].role, crate::message::Role::User);
        assert_eq!(history[1].role, crate::message::Role::Assistant);
        assert_eq!(history[2].role, crate::message::Role::User);
        assert_eq!(history[3].role, crate::message::Role::Assistant);
    }

    // ---- 迭代边界钩子与 overflow 恢复（对应 Java CompactionMiddleware）----

    /// 第一次调用报上下文溢出，之后正常响应。
    struct OverflowThenOkModel {
        responses: Mutex<Vec<crate::model::ChatResponse>>,
        failed_once: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl Model for OverflowThenOkModel {
        async fn call(&self, _request: ChatRequest) -> Result<crate::model::ChatResponse> {
            use std::sync::atomic::Ordering;
            if !self.failed_once.swap(true, Ordering::SeqCst) {
                return Err(Error::Model(
                    "This model's maximum context length is 4096 tokens. However, your messages resulted in 5000 tokens. Please reduce the length of the messages.".into(),
                ));
            }
            self.responses
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| Error::Model("no more mock responses".into()))
        }
    }

    #[test]
    fn overflow_error_patterns_match_java() {
        assert!(is_context_overflow_error(&Error::Model(
            "request failed: context_length_exceeded".into()
        )));
        assert!(is_context_overflow_error(&Error::Model(
            "the context length of 8192 tokens has been exceeded".into()
        )));
        assert!(is_context_overflow_error(&Error::Model(
            "maximum context size reached".into()
        )));
        assert!(is_context_overflow_error(&Error::Model(
            "token limit reached for this model".into()
        )));
        assert!(is_context_overflow_error(&Error::Model(
            "too many tokens in request".into()
        )));
        assert!(is_context_overflow_error(&Error::Model(
            "input exceeds the model's maximum context".into()
        )));
        assert!(is_context_overflow_error(&Error::Model(
            "please reduce the length of the messages".into()
        )));
        // 非溢出错误不命中
        assert!(!is_context_overflow_error(&Error::Model("connection refused".into())));
        assert!(!is_context_overflow_error(&Error::InvalidInput("bad input".into())));
    }

    #[tokio::test]
    async fn iteration_hook_runs_before_each_reasoning() {
        // 两轮迭代（tool call → final）：钩子应在每轮 reasoning 前各调一次，forced=false
        let calls: Arc<Mutex<Vec<bool>>> = Arc::new(Mutex::new(Vec::new()));
        let calls_for_hook = Arc::clone(&calls);
        let mut toolkit = Toolkit::new();
        toolkit.register(Arc::new(AddTool));
        let agent = ReActAgent::builder()
            .name("assistant")
            .model(Arc::new(MockModel {
                responses: Mutex::new(vec![
                    crate::model::ChatResponse::from_text("1 加 2 等于 3"),
                    crate::model::ChatResponse {
                        content: vec![
                            ContentBlock::Text { text: "我来算一下".into() },
                            ContentBlock::ToolUse {
                                id: "call_1".into(),
                                name: "add".into(),
                                input: json!({"a": 1, "b": 2}),
                                state: None,
                                content: None,
                            },
                        ],
                        ..Default::default()
                    },
                ]),
            }))
            .toolkit(toolkit)
            .memory(Arc::new(InMemoryMemory::new()))
            .iteration_hook(Arc::new(move |_rc, _state, forced| {
                calls_for_hook.lock().unwrap().push(forced);
                Box::pin(async { Ok(()) })
            }))
            .build()
            .unwrap();

        let reply = agent.call(Msg::user("user", "1加2等于几？")).await.unwrap();
        assert_eq!(reply.text(), "1 加 2 等于 3");
        assert_eq!(*calls.lock().unwrap(), vec![false, false]);
    }

    #[tokio::test]
    async fn overflow_recovery_forces_compact_and_retries() {
        // 第一次 reasoning 报溢出 → 钩子以 forced=true 被调（模拟强制压缩：清空历史）
        // → 同轮重试成功。
        let forced_calls: Arc<Mutex<Vec<bool>>> = Arc::new(Mutex::new(Vec::new()));
        let forced_for_hook = Arc::clone(&forced_calls);
        let agent = ReActAgent::builder()
            .name("assistant")
            .model(Arc::new(OverflowThenOkModel {
                responses: Mutex::new(vec![crate::model::ChatResponse::from_text("恢复后的回答")]),
                failed_once: std::sync::atomic::AtomicBool::new(false),
            }))
            .memory(Arc::new(InMemoryMemory::new()))
            .iteration_hook(Arc::new(move |_rc, state, forced| {
                forced_for_hook.lock().unwrap().push(forced);
                if forced {
                    // 模拟强制压缩：把对话真相清到只剩一条（真实实现是摘要替换）
                    let mut guard = state.lock().unwrap();
                    let keep = guard.context.last().cloned();
                    guard.context.clear();
                    if let Some(msg) = keep {
                        guard.context.push(msg);
                    }
                }
                Box::pin(async { Ok(()) })
            }))
            .build()
            .unwrap();

        let reply = agent.call(Msg::user("user", "你好")).await.unwrap();
        assert_eq!(reply.text(), "恢复后的回答");
        // 第一次迭代边界（false）+ 溢出恢复（true）
        assert_eq!(*forced_calls.lock().unwrap(), vec![false, true]);
        // 恢复成功：历史里有 user + 最终回复
        let history = history(&agent);
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].text(), "恢复后的回答");
    }

    #[tokio::test]
    async fn non_overflow_error_propagates_without_hook_retry() {
        // 非溢出错误：即使有钩子也不做 forced 恢复，错误照常传播
        let calls: Arc<Mutex<Vec<bool>>> = Arc::new(Mutex::new(Vec::new()));
        let calls_for_hook = Arc::clone(&calls);
        struct AlwaysErr;
        #[async_trait::async_trait]
        impl Model for AlwaysErr {
            async fn call(&self, _request: ChatRequest) -> Result<crate::model::ChatResponse> {
                Err(Error::Model("connection refused".into()))
            }
        }
        let agent = ReActAgent::builder()
            .name("assistant")
            .model(Arc::new(AlwaysErr))
            .memory(Arc::new(InMemoryMemory::new()))
            .iteration_hook(Arc::new(move |_rc, _state, forced| {
                calls_for_hook.lock().unwrap().push(forced);
                Box::pin(async { Ok(()) })
            }))
            .build()
            .unwrap();

        let result = agent.call(Msg::user("user", "你好")).await;
        assert!(result.is_err());
        // 只有迭代边界的常规调用，没有 forced 恢复
        assert_eq!(*calls.lock().unwrap(), vec![false]);
    }
}
