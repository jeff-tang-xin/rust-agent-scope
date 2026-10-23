//! 中间件（对应 Java `io.agentscope.core.middleware`）。
//!
//! Java 2.0 用 `MiddlewareBase` 取代了旧的 `hook` 包（后者在 2.0.4 已标
//! `@Deprecated(forRemoval = true)`），提供 **6 个扩展点**：
//!
//! | 扩展点 | 模式 | Java | Rust |
//! |--------|------|------|-------|
//! | `on_agent` | 洋葱 | `Flux onAgent(agent, ctx, AgentInput, next)` | 输入变换（返回值） |
//! | `on_reasoning` | 洋葱 | 同上，`ReasoningInput` | 输入变换 |
//! | `on_acting` | 洋葱 | 同上，`ActingInput` | 输入变换 |
//! | `on_model_call` | 洋葱 | 同上，`ModelCallInput` | 输入变换 |
//! | `on_system_prompt` | 管道 | `Mono<String>` | `String -> String` |
//! | `on_agent_state_ready` | 通知 | `void` | `&mut Vec<Msg>` + `&Arc<Mutex<AgentState>>` |
//!
//! # 形态映射（为什么不是 `Function<Input, Flux<AgentEvent>> next`）
//!
//! Java 的洋葱靠 `next: Function<I, Flux<AgentEvent>>` 回调实现：中间件既能改写输入，
//! 也能用 Flux 操作符包装下游事件流。本实现的事件出口是 **推模式**（`mpsc::Sender`），
//! 没有「下游事件流」这个可包装对象，因此拆成两个正交机制：
//!
//! - **输入变换**：中间件返回新的输入值（纯函数），由 [`MiddlewareChain`] 依次应用后
//!   交给下一环 —— 这正是反模式禁令③要求的「hook 返回 patch，由统一方应用」，
//!   不传 `&mut Context`，也不在中间件间共享可变状态。
//! - **事件旁路**：[`EventFilter`] —— Java 里包事件流的 Flux 操作符（如
//!   `FinalAnswerFilterMiddleware` 的 `concatMap`）在 Rust 侧的等价物。过滤器实例
//!   **每次调用新建**（对齐 Java 在 `Flux.defer` 内建 `RoundState` 的做法），
//!   状态随调用结束丢弃。
//!
//! 顺序语义保持一致：`order()` 越大越靠外层，同值按注册顺序；输入变换按「外→内」应用，
//! 过滤器按「外→内」依次过事件。

use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::context::RuntimeContext;
use crate::event::{AgentEvent, AgentEventKind};
use crate::message::{ContentBlock, Msg};
use crate::model::{GenerateOptions, Model};
use crate::state::{
    AgentState, Task, TaskAuthority, TaskContextState, TaskRequirement, TaskRequirementKind,
    TaskRequirementStatus, TaskScope, TaskState, VerificationOutcome,
};

/// 中间件扩展点（对应 Java `MiddlewareBase.ExtensionPoint`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExtensionPoint {
    OnAgent,
    OnReasoning,
    OnActing,
    OnModelCall,
    OnSystemPrompt,
    OnAgentStateReady,
    /// 本次调用完成通知（对应 Java `on_agent` 的 `doOnComplete` 语义）：
    /// 事件流结束后、状态已落盘时触发，用于 fire-and-forget 的后台收尾
    /// （如 memory flush / maintenance）。
    OnAgentComplete,
}

/// 全部扩展点：`active_points()` 的默认值。
pub const ALL_EXTENSION_POINTS: &[ExtensionPoint] = &[
    ExtensionPoint::OnAgent,
    ExtensionPoint::OnReasoning,
    ExtensionPoint::OnActing,
    ExtensionPoint::OnModelCall,
    ExtensionPoint::OnSystemPrompt,
    ExtensionPoint::OnAgentStateReady,
    ExtensionPoint::OnAgentComplete,
];

// ---------- 各扩展点的输入载体（对应 Java 的 record） ----------

/// `on_agent` 的输入：本次调用的输入消息。
#[derive(Debug, Clone, Default)]
pub struct AgentInput {
    pub msgs: Vec<Msg>,
}

impl AgentInput {
    pub fn new(msgs: Vec<Msg>) -> Self {
        Self { msgs }
    }

    /// 单条输入（一次 `call()` 的常见形态）。
    pub fn single(msg: Msg) -> Self {
        Self { msgs: vec![msg] }
    }
}

/// `on_reasoning` 的输入：本轮发给模型的消息与生成参数。
///
/// 注：Java 把 `tools` 与 `options` 并列（`GenerateOptions` 不含工具），
/// Rust 侧 `GenerateOptions` 已含 `tools`，这里合并进 `options` 以避免同一事实两处真相。
#[derive(Debug, Clone, Default)]
pub struct ReasoningInput {
    pub messages: Vec<Msg>,
    pub options: GenerateOptions,
}

/// `on_model_call` 的输入：真正要发出的请求，外加可替换的模型实例。
#[derive(Clone)]
pub struct ModelCallInput {
    pub messages: Vec<Msg>,
    pub options: GenerateOptions,
    /// 本次调用使用的模型；中间件可换成别的实现（对应 Java `ModelCallInput#model`）。
    pub model: Arc<dyn Model>,
}

impl ModelCallInput {
    pub fn new(messages: Vec<Msg>, options: GenerateOptions, model: Arc<dyn Model>) -> Self {
        Self {
            messages,
            options,
            model,
        }
    }
}

impl std::fmt::Debug for ModelCallInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelCallInput")
            .field("messages", &self.messages)
            .field("options", &self.options)
            .field("model", &"<dyn Model>")
            .finish()
    }
}

/// 一次工具调用的描述（对应 Java `ToolUseBlock` 的可改写视图）。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallSpec {
    pub id: String,
    pub name: String,
    pub input: Value,
}

/// `on_acting` 的输入：本轮待执行的工具调用（可整体改写）。
#[derive(Debug, Clone, Default)]
pub struct ActingInput {
    pub tool_calls: Vec<ToolCallSpec>,
}

// ---------- 事件旁路 ----------

/// 事件过滤器：Java 里包住事件流的 Flux 操作符在推模式下的等价物。
///
/// 由 [`Middleware::event_filters`] 在**每次调用**时新建，`filter` 返回要实际发出的事件
/// （空表示抑制），`finish` 在调用结束时清理（对应 `doFinally`）。
pub trait EventFilter: Send {
    /// 处理一个事件，返回替代事件序列（空 = 丢弃该事件）。
    fn filter(&mut self, event: AgentEvent) -> Vec<AgentEvent>;

    /// 本次调用结束（含提前返回与出错）。
    fn finish(&mut self) {}
}

// ---------- 中间件契约 ----------

/// 中间件：在 6 个扩展点介入 agent 执行。
///
/// 全部方法都有默认实现（透传 / 空操作），只需覆盖关心的点。
pub trait Middleware: Send + Sync {
    /// 中间件名（日志与调试用；Java 无此方法，Rust 侧为便于定位而加）。
    fn name(&self) -> &str;

    /// 执行顺序：**越大越靠外层**（先于更小的执行），同值保持注册顺序。默认 1
    /// （对齐 Java `MiddlewareBase#order`）。
    fn order(&self) -> i32 {
        1
    }

    /// 参与哪些扩展点。默认全参与；返回空切片等于「注册了但不生效」。
    ///
    /// 这是**参与开关**而非「哪些方法被覆盖」：激活了但没覆盖的扩展点只是走默认透传，
    /// 未激活的扩展点即使覆盖了也不会被调用。
    fn active_points(&self) -> &[ExtensionPoint] {
        ALL_EXTENSION_POINTS
    }

    /// 该中间件是否参与某个扩展点。
    fn is_active(&self, point: ExtensionPoint) -> bool {
        self.active_points().contains(&point)
    }

    /// 拦截整个 agent 调用：可改写输入消息。
    fn on_agent(&self, _ctx: &RuntimeContext, input: AgentInput) -> AgentInput {
        input
    }

    /// 拦截 reasoning 阶段：可改写本轮消息与生成参数。
    fn on_reasoning(&self, _ctx: &RuntimeContext, input: ReasoningInput) -> ReasoningInput {
        input
    }

    /// 拦截 acting 阶段：可改写待执行的工具调用。
    fn on_acting(&self, _ctx: &RuntimeContext, input: ActingInput) -> ActingInput {
        input
    }

    /// 拦截模型调用：可改写请求或替换模型实例。
    fn on_model_call(&self, _ctx: &RuntimeContext, input: ModelCallInput) -> ModelCallInput {
        input
    }

    /// 变换 system prompt（管道模式：多个中间件依次串联）。
    fn on_system_prompt(&self, _ctx: &RuntimeContext, current: String) -> String {
        current
    }

    /// 本次调用的 AgentState 就绪通知（单向，无后续委托）。
    ///
    /// `input_msgs` 是本次调用的输入私有副本（尚未并入对话历史），就地修改会影响整次调用。
    /// `state` 是本次调用的活状态：需要「读历史 / 往上下文里塞提醒消息」的中间件
    /// （对应 Java `TaskReminderMiddleware`）用它，而不是去碰 agent 内部字段。
    fn on_agent_state_ready(
        &self,
        _ctx: &RuntimeContext,
        _input_msgs: &mut Vec<Msg>,
        _state: &Arc<Mutex<AgentState>>,
    ) {
    }

    /// 本次调用完成通知（对应 Java `on_agent` 包装流的 `doOnComplete`）：
    /// 事件流结束、状态已快照/落盘后触发。`state` 是本次调用的最终状态句柄
    /// （含全部对话消息）。适合 fire-and-forget 的后台收尾（memory flush /
    /// maintenance）；实现方自行 spawn，不得阻塞返回。
    fn on_agent_complete(&self, _ctx: &RuntimeContext, _state: &Arc<Mutex<AgentState>>) {}

    /// 注册本次调用的事件过滤器（默认无）。
    fn event_filters(&self) -> Vec<Box<dyn EventFilter>> {
        Vec::new()
    }
}

// ---------- 链 ----------

/// 中间件链：按 `order()` 排好序后统一应用（对应 Java `MiddlewareChain`）。
#[derive(Clone, Default)]
pub struct MiddlewareChain {
    middlewares: Vec<Arc<dyn Middleware>>,
}

impl MiddlewareChain {
    /// 建链：稳定排序为「order 大者在前」（外层先应用）。
    pub fn new(mut middlewares: Vec<Arc<dyn Middleware>>) -> Self {
        middlewares.sort_by_key(|m| std::cmp::Reverse(m.order()));
        Self { middlewares }
    }

    pub fn is_empty(&self) -> bool {
        self.middlewares.is_empty()
    }

    pub fn len(&self) -> usize {
        self.middlewares.len()
    }

    /// 参与该扩展点的中间件（保持链顺序）。
    fn active_at(
        &self,
        point: ExtensionPoint,
    ) -> impl Iterator<Item = &Arc<dyn Middleware>> {
        self.middlewares.iter().filter(move |m| m.is_active(point))
    }

    /// 顺序应用 `on_agent`。
    pub fn apply_agent(&self, ctx: &RuntimeContext, mut input: AgentInput) -> AgentInput {
        for mw in self.active_at(ExtensionPoint::OnAgent) {
            input = mw.on_agent(ctx, input);
        }
        input
    }

    /// 顺序应用 `on_reasoning`。
    pub fn apply_reasoning(&self, ctx: &RuntimeContext, mut input: ReasoningInput) -> ReasoningInput {
        for mw in self.active_at(ExtensionPoint::OnReasoning) {
            input = mw.on_reasoning(ctx, input);
        }
        input
    }

    /// 顺序应用 `on_acting`。
    pub fn apply_acting(&self, ctx: &RuntimeContext, mut input: ActingInput) -> ActingInput {
        for mw in self.active_at(ExtensionPoint::OnActing) {
            input = mw.on_acting(ctx, input);
        }
        input
    }

    /// 顺序应用 `on_model_call`。
    pub fn apply_model_call(&self, ctx: &RuntimeContext, mut input: ModelCallInput) -> ModelCallInput {
        for mw in self.active_at(ExtensionPoint::OnModelCall) {
            input = mw.on_model_call(ctx, input);
        }
        input
    }

    /// 管道应用 `on_system_prompt`：前一个的产出是后一个的输入。
    pub fn apply_system_prompt(&self, ctx: &RuntimeContext, prompt: String) -> String {
        let mut current = prompt;
        for mw in self.active_at(ExtensionPoint::OnSystemPrompt) {
            current = mw.on_system_prompt(ctx, current);
        }
        current
    }

    /// 广播 `on_agent_state_ready`（无 next 委托，顺序即 order 顺序）。
    pub fn notify_state_ready(
        &self,
        ctx: &RuntimeContext,
        input_msgs: &mut Vec<Msg>,
        state: &Arc<Mutex<AgentState>>,
    ) {
        for mw in self.active_at(ExtensionPoint::OnAgentStateReady) {
            mw.on_agent_state_ready(ctx, input_msgs, state);
        }
    }

    /// 调用完成通知（对应 Java `on_agent doOnComplete`）。
    pub fn notify_agent_complete(&self, ctx: &RuntimeContext, state: &Arc<Mutex<AgentState>>) {
        for mw in self.active_at(ExtensionPoint::OnAgentComplete) {
            mw.on_agent_complete(ctx, state);
        }
    }

    /// 为本次调用新建过滤器链（外层在前）。
    pub fn event_filters(&self) -> Vec<Box<dyn EventFilter>> {
        self.middlewares
            .iter()
            .flat_map(|m| m.event_filters())
            .collect()
    }
}

// ---------- 内置中间件 ----------

/// 只放行「最终回复那一轮」的文本事件（对应 Java `FinalAnswerFilterMiddleware`）。
///
/// 文本事件先缓冲：若本轮模型发起了工具调用，则本轮文本与后续文本一并抑制；
/// 否则在 `ModelCallEnd` 之前一次性发出缓冲的文本事件。opt-in，不改变默认行为。
pub struct FinalAnswerFilter;

impl Middleware for FinalAnswerFilter {
    fn name(&self) -> &str {
        "final-answer-filter"
    }

    fn active_points(&self) -> &[ExtensionPoint] {
        &[ExtensionPoint::OnReasoning]
    }

    fn event_filters(&self) -> Vec<Box<dyn EventFilter>> {
        vec![Box::new(RoundState::default())]
    }
}

/// 单轮过滤状态：对应 Java `FinalAnswerFilterMiddleware.RoundState`（每次调用新建）。
#[derive(Default)]
pub struct RoundState {
    buffered: Vec<AgentEvent>,
    reply_id: Option<String>,
    tool_call_seen: bool,
}

impl RoundState {
    fn is_current(&self, reply_id: Option<&str>) -> bool {
        match (&self.reply_id, reply_id) {
            (Some(cur), Some(ev)) => cur == ev,
            _ => false,
        }
    }

    fn clear(&mut self) {
        self.buffered.clear();
        self.reply_id = None;
        self.tool_call_seen = false;
    }
}

impl EventFilter for RoundState {
    fn filter(&mut self, event: AgentEvent) -> Vec<AgentEvent> {
        match &event.kind {
            // 新的一轮：记录 replyId、重置状态、透传
            AgentEventKind::ModelCallStart { reply_id } => {
                self.reply_id = Some(reply_id.clone());
                self.tool_call_seen = false;
                self.buffered.clear();
                vec![event]
            }
            // 文本块：当前轮且未见工具调用 → 缓冲；当前轮已见工具调用 → 丢弃
            AgentEventKind::TextBlockStart { reply_id, .. }
            | AgentEventKind::TextBlockDelta { reply_id, .. }
            | AgentEventKind::TextBlockEnd { reply_id, .. } => {
                if self.is_current(Some(reply_id)) {
                    if self.tool_call_seen {
                        return Vec::new();
                    }
                    self.buffered.push(event);
                    return Vec::new();
                }
                vec![event]
            }
            // 当前轮出现工具调用 → 本轮文本属于中间产物
            AgentEventKind::ToolCallStart { reply_id, .. } => {
                if self.is_current(Some(reply_id)) {
                    self.tool_call_seen = true;
                    self.buffered.clear();
                }
                vec![event]
            }
            // 本轮模型调用结束：最终轮则先吐缓冲再发 ModelCallEnd
            AgentEventKind::ModelCallEnd { reply_id, .. } => {
                if self.is_current(Some(reply_id)) {
                    if self.tool_call_seen {
                        self.clear();
                        return vec![event];
                    }
                    let mut out = std::mem::take(&mut self.buffered);
                    out.push(event);
                    self.clear();
                    return out;
                }
                vec![event]
            }
            _ => vec![event],
        }
    }

    fn finish(&mut self) {
        self.clear();
    }
}

// ---------- 任务提醒 / 任务上下文投影 / 模型请求准备器 ----------
//
// 对应 Java `TaskReminderMiddleware` / `TaskContextProjection` / `ModelRequestPreparer`。
//
// ## 与 Java 的形态差异（读代码前先看这里）
//
// 1. **状态获取**：Java 的 `TaskReminderMiddleware.onReasoning` 每轮通过
//    `RuntimeContext.resolveAgentState(ctx, agent)` 现取状态；Rust 的 `on_reasoning`
//    只有 `&RuntimeContext`，拿不到 `AgentState`。因此本实现借用
//    `on_agent_state_ready`（每次调用开始必达，携带 `Arc<Mutex<AgentState>>`）把
//    状态句柄按 `reply_id` 登记进中间件内部的有界缓存，`on_reasoning` 每轮现取现锁
//    —— 与 Java 一样，同一次调用的后续迭代能看到本轮工具写入的最新任务状态。
// 2. **ToolResultBlock 元数据**：Java 用 `context.state_key` / `context.state_revision` /
//    `context.representation` 三个元数据键识别「当前完整回执」；Rust
//    `ContentBlock::ToolResult` 没有元数据通道，改用「工具名 + 单 Text 输出与当前
//    渲染逐字相等」判定（语义等价：文本等于当前状态渲染即当前回执）。历史回执的
//    降级文案因此不含修订号。
// 3. **context_build 事件**：core 事件模型已有 `AgentEventKind::Custom { name, value }`，
//    直接用它承载 `context_build`，不新增事件变体。
// 4. **投影抑制**：Java 在 agent 配置了 `handlesTaskProjection()` 的 preparer 时跳过
//    `onReasoning` 投影；Rust 中间件拿不到 agent 配置，该抑制由装配方负责
//    （`ModelRequestPreparer::handles_task_projection` 返回 true 时不要再注册本中间件）。

/// `todo_write` 工具名（对应 Java `TodoTools` 的工具名，投影用它识别任务回执）。
pub const TODO_TOOL_NAME: &str = "todo_write";

/// 合成提醒消息的 reminder kind 值（对应 Java `Msg.METADATA_REMINDER_KIND` 的 "todo_state"）。
const TODO_STATE_REMINDER_KIND: &str = "todo_state";

/// 历史回执降级后的占位文案（Java 版含修订号，Rust 无元数据通道故省略）。
const STALE_RECEIPT_TEXT: &str = "Todo update recorded; use the current task state, not this historical status.";

/// 渲染任务列表（对应 Java `TodoTools.render`；投影与工具回执共用同一格式，
/// 「回执文本 == 当前渲染」是投影判定当前回执的依据）。
pub fn render_tasks(tasks: &[Task]) -> String {
    if tasks.is_empty() {
        return "Todo list cleared (0 items).".to_string();
    }
    let open = tasks
        .iter()
        .filter(|task| task.state != TaskState::Completed)
        .count();
    let mut text = format!("{open} open todo(s):\n");
    for task in tasks {
        let marker = match task.state {
            TaskState::Completed => "- [x]",
            TaskState::InProgress => "- [~]",
            TaskState::Pending => "- [ ]",
        };
        text.push_str(marker);
        text.push(' ');
        text.push_str(&task.subject);
        if let Some(priority) = task.metadata.get("priority") {
            // JSON 字符串不带引号地渲染，其余类型用 JSON 字面量（对齐 Java toString）。
            let rendered = match priority {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            text.push_str(" (priority: ");
            text.push_str(&rendered);
            text.push(')');
        }
        text.push('\n');
    }
    text.trim_end().to_string()
}

/// 任务上下文投影（对应 Java `TaskContextProjection`）：纯函数、瞬态，
/// 绝不把提醒追加进持久对话历史 —— 只改写发给模型的副本。
pub struct TaskContextProjection;

impl TaskContextProjection {
    /// 对齐 Java 四参重载 `project(input, state, requirements, verificationResults)`：
    /// 需求与验证投影独立开关。Java 两参 / 三参重载等价于 `requirements == verification_results`。
    pub fn project(
        input: Vec<Msg>,
        state: &TaskContextState,
        requirements: bool,
        verification_results: bool,
    ) -> Vec<Msg> {
        let state = state.snapshot();
        let rendered = render_tasks(state.tasks());

        // 第一遍：找「当前完整回执」的块位置（跨消息统一计数，对齐 Java 的 position 语义）。
        let mut last_full: Option<usize> = None;
        let mut position = 0usize;
        for message in &input {
            for block in &message.content {
                if Self::is_current_receipt(block, &rendered) {
                    last_full = Some(position);
                }
                position += 1;
            }
        }
        let visible = last_full.is_some();

        // 第二遍：跳过旧合成消息（保持块位置对齐），把非当前的 todo_write 回执降级为占位文案。
        let mut messages: Vec<Msg> = Vec::with_capacity(input.len() + 1);
        let mut position = 0usize;
        for mut message in input {
            if is_synthetic_todo_state(&message) {
                position += message.content.len();
                messages.push(message);
                continue;
            }
            for index in 0..message.content.len() {
                let tracked = matches!(&message.content[index], ContentBlock::ToolResult { name, .. } if name.as_str() == TODO_TOOL_NAME);
                if tracked && Some(position) != last_full {
                    if let ContentBlock::ToolResult {
                        id,
                        name,
                        state: result_state,
                        execution_details,
                        ..
                    } = message.content[index].clone()
                    {
                        message.content[index] = ContentBlock::ToolResult {
                            id,
                            name,
                            output: vec![ContentBlock::Text {
                                text: STALE_RECEIPT_TEXT.to_string(),
                            }],
                            state: result_state,
                            execution_details,
                        };
                    }
                }
                position += 1;
            }
            messages.push(message);
        }

        // 注入条件与 Java 逐条对应：历史里没有当前回执但任务状态非空、或需求 / 范围 / 验证需要露出。
        let needs_state = (!visible && (!state.tasks().is_empty() || state.revision() > 0))
            || (requirements && !state.requirements().is_empty())
            || ((requirements || verification_results) && state.scope().is_some())
            || (verification_results && !state.verifications().is_empty());
        if needs_state {
            let mut text = format!("<TASK_STATE revision=\"{}\">\n", state.revision());
            if requirements || verification_results {
                text.push_str(&render_scope(state.scope()));
            }
            if visible {
                text.push_str("Todo progress: use the current full tool receipt.\n");
            } else {
                text.push_str(
                    "Current todo status (agent-maintained, not independent verification):\n",
                );
                text.push_str(&escape(&rendered));
            }
            if requirements {
                text.push_str(&render_requirements(state.requirements()));
            }
            if verification_results {
                text.push_str(&render_verifications(&state));
            }
            text.push_str("\n</TASK_STATE>");
            let mut reminder = Msg::user("system", text).with_metadata(serde_json::json!({
                (crate::message::metadata_keys::SYNTHETIC): true,
                (crate::message::metadata_keys::REMINDER_KIND): TODO_STATE_REMINDER_KIND,
            }));
            reminder.id = "harness:todo_state".to_string();
            messages.push(reminder);
        }
        messages
    }

    /// 是否为「当前完整回执」：todo_write 的单 Text 输出与当前渲染逐字相等。
    /// Java 额外校验 `context.state_revision` 与 `context.representation=full` 元数据；
    /// Rust 无元数据通道，文本等于当前状态渲染即语义等价（历史回执文本必然不同）。
    fn is_current_receipt(block: &ContentBlock, rendered: &str) -> bool {
        let ContentBlock::ToolResult { name, output, .. } = block else {
            return false;
        };
        if name.as_str() != TODO_TOOL_NAME {
            return false;
        }
        matches!(output.as_slice(), [ContentBlock::Text { text }] if text == rendered)
    }
}

/// 是否为旧的 todo_state 合成提醒消息（对应 Java 按 `METADATA_SYNTHETIC` +
/// `METADATA_REMINDER_KIND=todo_state` 识别）。
fn is_synthetic_todo_state(message: &Msg) -> bool {
    let Some(metadata) = &message.metadata else {
        return false;
    };
    metadata.get(crate::message::metadata_keys::SYNTHETIC) == Some(&Value::Bool(true))
        && metadata
            .get(crate::message::metadata_keys::REMINDER_KIND)
            .and_then(Value::as_str)
            == Some(TODO_STATE_REMINDER_KIND)
}

/// XML 转义（对应 Java `TaskContextProjection.escape`）。
fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// 渲染任务范围（对应 Java `renderScope`）。
fn render_scope(scope: Option<&TaskScope>) -> String {
    let Some(scope) = scope else {
        return String::new();
    };
    format!(
        "Task: {}\nObjective: {}\nObjective source: {}\n",
        escape(&scope.task_id),
        escape(&scope.objective),
        escape(&scope.source_ref)
    )
}

/// 渲染需求列表（对应 Java `renderRequirements`；枚举标签用 serde wire 名）。
fn render_requirements(requirements: &[TaskRequirement]) -> String {
    if requirements.is_empty() {
        return String::new();
    }
    let mut text = String::from(
        "\n<REQUIREMENTS>\n\
         CANDIDATE = unconfirmed proposal; CONFIRMED = authorized requirement, not verified \
         satisfaction; REJECTED = inactive. Proposed source references are unverified.\n",
    );
    for requirement in requirements {
        text.push_str(&escape(&requirement.id));
        text.push_str(" | ");
        text.push_str(requirement_kind_label(requirement.kind));
        text.push_str(" | ");
        text.push_str(requirement_status_label(requirement.status));
        text.push_str(" | ");
        text.push_str(&escape(&requirement.text));
        text.push_str("\nProposed source: ");
        text.push_str(&escape(&requirement.proposed_source_ref));
        if let Some(decision) = &requirement.decision {
            text.push_str("\nDecision: ");
            text.push_str(authority_label(decision.authority));
            text.push_str(" | ");
            text.push_str(&escape(&decision.source_ref));
        }
        text.push('\n');
    }
    text.push_str("</REQUIREMENTS>");
    text
}

/// 渲染验证记录（对应 Java `renderVerifications`；绑定不匹配的结论一律标 STALE）。
fn render_verifications(state: &TaskContextState) -> String {
    if state.verifications().is_empty() {
        return String::new();
    }
    let mut text = String::from(
        "\n<VERIFICATION>\n\
         These are bounded checks, not an overall task completion verdict. STALE evidence must \
         not be used for current acceptance.\n",
    );
    let binding = state.evidence_binding();
    for report in state.verifications() {
        let current = binding.as_ref() == Some(&report.binding);
        text.push_str(&escape(&report.requirement_id));
        text.push_str(" | ");
        if current {
            text.push_str(outcome_label(report.outcome));
        } else {
            text.push_str("STALE");
        }
        text.push_str(" | verifier=");
        text.push_str(&escape(&report.verifier_id));
        text.push_str(" | evidence=verification_");
        text.push_str(&escape(&report.id));
        text.push_str(" | action=");
        text.push_str(&escape(&report.action_id));
        text.push_str("\nChecked version: ");
        text.push_str(&escape(&report.binding.subject_version));
        text.push_str("\nReason: ");
        text.push_str(&escape(&report.reason));
        text.push('\n');
    }
    text.push_str("</VERIFICATION>");
    text
}

fn requirement_kind_label(kind: TaskRequirementKind) -> &'static str {
    match kind {
        TaskRequirementKind::Constraint => "CONSTRAINT",
        TaskRequirementKind::AcceptanceCriterion => "ACCEPTANCE_CRITERION",
    }
}

fn requirement_status_label(status: TaskRequirementStatus) -> &'static str {
    match status {
        TaskRequirementStatus::Candidate => "CANDIDATE",
        TaskRequirementStatus::Confirmed => "CONFIRMED",
        TaskRequirementStatus::Rejected => "REJECTED",
    }
}

fn authority_label(authority: TaskAuthority) -> &'static str {
    match authority {
        TaskAuthority::User => "USER",
        TaskAuthority::Caller => "CALLER",
    }
}

fn outcome_label(outcome: VerificationOutcome) -> &'static str {
    match outcome {
        VerificationOutcome::Passed => "PASSED",
        VerificationOutcome::Failed => "FAILED",
        VerificationOutcome::Unknown => "UNKNOWN",
        VerificationOutcome::Error => "ERROR",
    }
}

/// todo 使用指引（对应 Java `TaskReminderMiddleware.GROUNDING`，逐字移植）。
const GROUNDING: &str = "\n## Task List\n\
You have a `todo_write` tool that maintains a structured task list for this session.\n\
Use it for multi-step work: capture the plan as todos, keep exactly one task\n\
`in_progress`, and update the whole list as you make progress. Your current list (if\n\
any) is available as the latest complete tool receipt or a `<TASK_STATE>` projection.\n\
Use the latest state, not older statuses. These are agent-maintained progress records,\n\
not independent evidence of successful verification.\n\
Never infer task acceptance from all todos being completed.";

/// 需求使用指引（对应 Java `REQUIREMENTS_GROUNDING`，逐字移植）。
const REQUIREMENTS_GROUNDING: &str = "\n## Candidate requirements\n\
Use task_requirement_propose to capture candidate constraints or acceptance criteria.\n\
A user-message or PLAN.md reference does not itself confirm a proposal. Only explicit\n\
caller/user decisions can confirm it. CONFIRMED means authorized, not verified or satisfied.\n\
Never infer task acceptance from all todos being completed.";

/// `on_agent_state_ready` 登记的状态句柄上限（防长跑进程泄漏；同并发的调用数远小于此）。
const MAX_TRACKED_CALLS: usize = 8;

/// 任务提醒中间件（对应 Java `TaskReminderMiddleware`）。
///
/// 两件事：① `on_system_prompt` 追加 todo / 需求的使用指引；② `on_reasoning` 把
/// [`TaskContextProjection`] 应用在本轮消息副本上 —— 合成提醒只进模型请求，
/// 绝不落进持久历史。默认 `todo = true`、`requirements = false`（对齐 Java 无参构造）。
///
/// 状态获取见模块注释「与 Java 的形态差异」第 1 条：`on_agent_state_ready` 登记
/// 状态句柄，`on_reasoning` 每轮现取现锁；未经过 `on_agent_state_ready`（独立单测、
/// 无 agent 驱动）时无状态可用，`on_reasoning` 原样透传（对齐 Java `state == null`）。
pub struct TaskReminderMiddleware {
    todo: bool,
    requirements: bool,
    /// reply_id → 状态句柄（`on_agent_state_ready` 登记，`on_reasoning` 消费）。
    state_handles: Mutex<Vec<(String, Arc<Mutex<AgentState>>)>>,
}

impl Default for TaskReminderMiddleware {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskReminderMiddleware {
    /// 默认配置：todo 指引开、需求指引关（对齐 Java 无参构造）。
    pub fn new() -> Self {
        Self::with_flags(true, false)
    }

    /// 指定两个指引开关（对齐 Java 双参构造）。
    pub fn with_flags(todo: bool, requirements: bool) -> Self {
        Self {
            todo,
            requirements,
            state_handles: Mutex::new(Vec::new()),
        }
    }

    pub fn todo_enabled(&self) -> bool {
        self.todo
    }

    pub fn requirements_enabled(&self) -> bool {
        self.requirements
    }

    /// 登记本次调用的状态句柄（同 reply_id 重复登记覆盖，超上限淘汰最旧）。
    fn remember_state(&self, reply_id: &str, state: &Arc<Mutex<AgentState>>) {
        let mut handles = self.state_handles.lock().unwrap_or_else(|error| error.into_inner());
        handles.retain(|(id, _)| id != reply_id);
        handles.push((reply_id.to_string(), Arc::clone(state)));
        while handles.len() > MAX_TRACKED_CALLS {
            handles.remove(0);
        }
    }

    /// 取本次调用的状态句柄。先克隆 Arc 再出锁 —— 绝不在持锁期间进入另一个 lock
    /// （临时 MutexGuard 存活到语句结束的自死锁坑，本项目已踩中 4 次）。
    fn state_for(&self, reply_id: &str) -> Option<Arc<Mutex<AgentState>>> {
        self.state_handles
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .find(|(id, _)| id == reply_id)
            .map(|(_, handle)| Arc::clone(handle))
    }
}

impl Middleware for TaskReminderMiddleware {
    fn name(&self) -> &str {
        "task-reminder"
    }

    /// 窄声明（对齐 Java `activePoints`）；Rust 额外需要 `OnAgentStateReady` 做状态登记。
    fn active_points(&self) -> &[ExtensionPoint] {
        &[
            ExtensionPoint::OnSystemPrompt,
            ExtensionPoint::OnReasoning,
            ExtensionPoint::OnAgentStateReady,
        ]
    }

    fn on_system_prompt(&self, _ctx: &RuntimeContext, mut current: String) -> String {
        // 对齐 Java：base + (todo ? GROUNDING : "") + (requirements ? REQUIREMENTS_GROUNDING : "")
        if self.todo {
            current.push_str(GROUNDING);
        }
        if self.requirements {
            current.push_str(REQUIREMENTS_GROUNDING);
        }
        current
    }

    fn on_agent_state_ready(
        &self,
        ctx: &RuntimeContext,
        _input_msgs: &mut Vec<Msg>,
        state: &Arc<Mutex<AgentState>>,
    ) {
        self.remember_state(ctx.reply_id(), state);
    }

    fn on_reasoning(&self, ctx: &RuntimeContext, input: ReasoningInput) -> ReasoningInput {
        // 无状态可用（未经 on_agent_state_ready）→ 原样透传，对齐 Java `state == null`。
        let Some(handle) = self.state_for(ctx.reply_id()) else {
            return input;
        };
        // 块作用域内取快照，锁不跨语句存活。
        let tasks_context = {
            let guard = handle.lock().unwrap_or_else(|error| error.into_inner());
            guard.tasks_context.snapshot()
        };
        // 对齐 Java 三参重载：assurance = requirements，验证投影跟随需求开关。
        let ReasoningInput { messages, options } = input;
        let messages = TaskContextProjection::project(
            messages,
            &tasks_context,
            self.requirements,
            self.requirements,
        );
        ReasoningInput { messages, options }
    }
}

// ---------- 模型请求准备器 ----------

/// `prepare_observed` 登记清单属性的键前缀（对应 Java `MANIFEST_ATTRIBUTE_PREFIX`）。
pub const MODEL_REQUEST_MANIFEST_PREFIX: &str = "agentscope.model_request_manifest.";

/// 构建结果事件名（对应 Java `BUILD_EVENT_NAME`）。
pub const CONTEXT_BUILD_EVENT_NAME: &str = "context_build";

/// 请求目的（对应 Java `ModelRequestPreparer.Purpose`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// 主对话推理请求。
    Reasoning,
    /// 会话摘要（压缩）请求。
    Summary,
}

impl Purpose {
    /// 事件载荷里的标识（对齐 Java `Purpose.name()` 的 SCREAMING_SNAKE_CASE）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Reasoning => "REASONING",
            Self::Summary => "SUMMARY",
        }
    }
}

/// 最终的模型请求准备器（对应 Java `ModelRequestPreparer`）。
///
/// 在所有 model-call 中间件变换之后、真正发请求之前调用；每次订阅（含重试）都重新
/// 调用。**实现方不得在 [`Self::prepare`] 里再调主模型。**
///
/// Rust 侧为同步 trait（core 中间件无 async）：Java 的 `Mono` 错误通道对应
/// [`crate::error::Result`]，`Consumer<AgentEvent>` 对应 `&mut dyn FnMut(AgentEvent)`。
pub trait ModelRequestPreparer: Send + Sync {
    /// 组装最终请求。实现方需要上报上下文清单时，通过
    /// `ctx.put(MODEL_REQUEST_MANIFEST_PREFIX + call_id, ...)` 写入，
    /// [`Self::prepare_observed`] 会把它带进 `context_build` 事件。
    fn prepare(
        &self,
        ctx: &RuntimeContext,
        input: ModelCallInput,
        call_id: &str,
        purpose: Purpose,
    ) -> crate::error::Result<ModelCallInput>;

    /// 是否由本准备器负责任务投影（对应 Java `handlesTaskProjection`）。
    /// 返回 true 时装配方不得再注册 [`TaskReminderMiddleware`] 的投影，避免双重注入。
    fn handles_task_projection(&self) -> bool {
        false
    }

    /// 带观测的准备（对应 Java `prepareObserved`）：重置清单属性 → `prepare` →
    /// 发一条 [`CONTEXT_BUILD_EVENT_NAME`] 自定义事件（成功与失败都发，失败在
    /// 发请求之前就能被观测到）。失败时错误只记类型不记内容 —— 错误文本可能
    /// 携带源内容（对齐 Java 注释），并把清单属性清空。
    fn prepare_observed(
        &self,
        ctx: &RuntimeContext,
        input: ModelCallInput,
        call_id: &str,
        purpose: Purpose,
        events: &mut dyn FnMut(AgentEvent),
    ) -> crate::error::Result<ModelCallInput> {
        let key = format!("{MODEL_REQUEST_MANIFEST_PREFIX}{call_id}");
        ctx.put(&key, Value::Null);
        match self.prepare(ctx, input, call_id, purpose) {
            Ok(result) => {
                emit_build_event(ctx, &key, call_id, purpose, None, events);
                Ok(result)
            }
            Err(error) => {
                emit_build_event(ctx, &key, call_id, purpose, Some(&error), events);
                ctx.put(&key, Value::Null);
                Err(error)
            }
        }
    }
}

/// 发 `context_build` 事件（对应 Java `emitBuild`）。
fn emit_build_event(
    ctx: &RuntimeContext,
    key: &str,
    call_id: &str,
    purpose: Purpose,
    error: Option<&crate::error::Error>,
    events: &mut dyn FnMut(AgentEvent),
) {
    let mut value = serde_json::Map::new();
    value.insert("model_call_id".into(), Value::from(call_id));
    value.insert("purpose".into(), Value::from(purpose.as_str()));
    value.insert(
        "status".into(),
        Value::from(if error.is_none() { "passed" } else { "failed" }),
    );
    if let Some(manifest) = ctx.get(key) {
        if !manifest.is_null() {
            value.insert("context_manifest".into(), manifest);
        }
    }
    if let Some(error) = error {
        value.insert("error_type".into(), Value::from(error_kind(error)));
    }
    events(AgentEvent::new(AgentEventKind::Custom {
        name: CONTEXT_BUILD_EVENT_NAME.to_string(),
        value,
    }));
}

/// 错误类型标识（对应 Java `error.getClass().getSimpleName()`：只记类型，不记内容）。
fn error_kind(error: &crate::error::Error) -> &'static str {
    match error {
        crate::error::Error::Model(_) => "Model",
        crate::error::Error::Tool(_) => "Tool",
        crate::error::Error::ToolSuspended(_) => "ToolSuspended",
        crate::error::Error::MaxIterations(_) => "MaxIterations",
        crate::error::Error::InvalidInput(_) => "InvalidInput",
        crate::error::Error::Json(_) => "Json",
        crate::error::Error::Io(_) => "Io",
        crate::error::Error::State(_) => "State",
        crate::error::Error::Session(_) => "Session",
        crate::error::Error::AgentShuttingDown => "AgentShuttingDown",
        crate::error::Error::Other(_) => "Other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 记录调用轨迹的中间件
    struct Recorder {
        name: &'static str,
        order: i32,
        points: &'static [ExtensionPoint],
        log: Arc<Mutex<Vec<String>>>,
    }

    impl Recorder {
        fn new(
            name: &'static str,
            order: i32,
            points: &'static [ExtensionPoint],
            log: Arc<Mutex<Vec<String>>>,
        ) -> Arc<Self> {
            Arc::new(Self {
                name,
                order,
                points,
                log,
            })
        }
        fn record(&self, what: &str) {
            self.log.lock().unwrap().push(what.to_string());
        }
    }

    impl Middleware for Recorder {
        fn name(&self) -> &str {
            self.name
        }
        fn order(&self) -> i32 {
            self.order
        }
        fn active_points(&self) -> &[ExtensionPoint] {
            self.points
        }
        fn on_agent(&self, _ctx: &RuntimeContext, input: AgentInput) -> AgentInput {
            self.record(&format!("{}:on_agent", self.name));
            input
        }
        fn on_reasoning(&self, _ctx: &RuntimeContext, input: ReasoningInput) -> ReasoningInput {
            self.record(&format!("{}:on_reasoning", self.name));
            input
        }
        fn on_system_prompt(&self, _ctx: &RuntimeContext, current: String) -> String {
            self.record(&format!("{}:on_system_prompt", self.name));
            format!("{current}[{}]", self.name)
        }
    }

    fn ctx() -> RuntimeContext {
        RuntimeContext::new()
    }

    #[test]
    fn chain_applies_higher_order_first() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let chain = MiddlewareChain::new(vec![
            Recorder::new("low", 1, ALL_EXTENSION_POINTS, log.clone()),
            Recorder::new("high", 5, ALL_EXTENSION_POINTS, log.clone()),
        ]);
        chain.apply_agent(&ctx(), AgentInput::default());
        assert_eq!(*log.lock().unwrap(), vec!["high:on_agent", "low:on_agent"]);
    }

    #[test]
    fn same_order_keeps_registration_order() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let chain = MiddlewareChain::new(vec![
            Recorder::new("first", 1, ALL_EXTENSION_POINTS, log.clone()),
            Recorder::new("second", 1, ALL_EXTENSION_POINTS, log.clone()),
        ]);
        chain.apply_agent(&ctx(), AgentInput::default());
        assert_eq!(*log.lock().unwrap(), vec!["first:on_agent", "second:on_agent"]);
    }

    #[test]
    fn inactive_point_is_skipped_even_if_overridden() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let chain = MiddlewareChain::new(vec![Recorder::new(
            "prompt-only",
            1,
            &[ExtensionPoint::OnSystemPrompt],
            log.clone(),
        )]);
        chain.apply_agent(&ctx(), AgentInput::default());
        chain.apply_reasoning(&ctx(), ReasoningInput::default());
        assert!(log.lock().unwrap().is_empty(), "未激活的扩展点不应被调用");

        let out = chain.apply_system_prompt(&ctx(), "sys".into());
        assert_eq!(out, "sys[prompt-only]");
    }

    #[test]
    fn system_prompt_pipeline_composes_outer_to_inner() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let chain = MiddlewareChain::new(vec![
            Recorder::new("inner", 1, ALL_EXTENSION_POINTS, log.clone()),
            Recorder::new("outer", 2, ALL_EXTENSION_POINTS, log.clone()),
        ]);
        // 外层先跑：outer 拿到原始串，inner 拿到 outer 的产出
        assert_eq!(chain.apply_system_prompt(&ctx(), "base".into()), "base[outer][inner]");
    }

    #[test]
    fn on_agent_can_rewrite_input() {
        struct Rewriter;
        impl Middleware for Rewriter {
            fn name(&self) -> &str {
                "rewriter"
            }
            fn on_agent(&self, _ctx: &RuntimeContext, mut input: AgentInput) -> AgentInput {
                input.msgs.push(Msg::user("system", "injected"));
                input
            }
        }
        let chain = MiddlewareChain::new(vec![Arc::new(Rewriter)]);
        let out = chain.apply_agent(&ctx(), AgentInput::single(Msg::user("user", "hi")));
        assert_eq!(out.msgs.len(), 2);
        assert_eq!(out.msgs[1].text(), "injected");
    }

    #[test]
    fn state_ready_notification_mutates_input_copy() {
        struct Inserter;
        impl Middleware for Inserter {
            fn name(&self) -> &str {
                "inserter"
            }
            fn active_points(&self) -> &[ExtensionPoint] {
                &[ExtensionPoint::OnAgentStateReady]
            }
            fn on_agent_state_ready(
                &self,
                _ctx: &RuntimeContext,
                msgs: &mut Vec<Msg>,
                state: &Arc<Mutex<AgentState>>,
            ) {
                // 状态里已有的历史条数可读，用来验证「拿到的是真实状态」
                let existing = state.lock().unwrap().context.len();
                msgs.insert(0, Msg::system("system", format!("state ready ({existing})")));
            }
        }
        let chain = MiddlewareChain::new(vec![Arc::new(Inserter)]);
        let mut msgs = vec![Msg::user("user", "hi")];
        let state = Arc::new(Mutex::new(AgentState::with_session_id("s")));
        chain.notify_state_ready(&ctx(), &mut msgs, &state);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].text(), "state ready (0)");
    }

    // ---------- FinalAnswerFilter ----------

    fn text_start(reply: &str, block: &str) -> AgentEvent {
        AgentEvent::new(AgentEventKind::TextBlockStart {
            reply_id: reply.into(),
            block_id: block.into(),
        })
    }
    fn text_delta(reply: &str, block: &str, delta: &str) -> AgentEvent {
        AgentEvent::new(AgentEventKind::TextBlockDelta {
            reply_id: reply.into(),
            block_id: block.into(),
            delta: delta.into(),
        })
    }
    fn text_end(reply: &str, block: &str) -> AgentEvent {
        AgentEvent::new(AgentEventKind::TextBlockEnd {
            reply_id: reply.into(),
            block_id: block.into(),
        })
    }
    fn model_start(reply: &str) -> AgentEvent {
        AgentEvent::new(AgentEventKind::ModelCallStart {
            reply_id: reply.into(),
        })
    }
    fn model_end(reply: &str) -> AgentEvent {
        AgentEvent::new(AgentEventKind::ModelCallEnd {
            reply_id: reply.into(),
            usage: None,
        })
    }
    fn tool_start(reply: &str, id: &str) -> AgentEvent {
        AgentEvent::new(AgentEventKind::ToolCallStart {
            reply_id: reply.into(),
            tool_call_id: id.into(),
            tool_call_name: "calc".into(),
        })
    }

    fn run_filter(state: &mut dyn EventFilter, events: Vec<AgentEvent>) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        for e in events {
            out.extend(state.filter(e));
        }
        out
    }

    #[test]
    fn final_answer_filter_suppresses_intermediate_round_text() {
        let mut state = RoundState::default();
        let out = run_filter(
            &mut state,
            vec![
                model_start("r1"),
                text_start("r1", "b1"),
                text_delta("r1", "b1", "thinking..."),
                text_end("r1", "b1"),
                tool_start("r1", "call-1"),
                model_end("r1"),
            ],
        );
        let kinds: Vec<&AgentEventKind> = out.iter().map(|e| &e.kind).collect();
        assert!(matches!(kinds[0], AgentEventKind::ModelCallStart { .. }));
        assert!(
            !out.iter().any(|e| matches!(e.kind, AgentEventKind::TextBlockDelta { .. })),
            "本轮有工具调用 → 中间文本必须被抑制"
        );
        assert!(matches!(kinds.last().unwrap(), AgentEventKind::ModelCallEnd { .. }));
    }

    #[test]
    fn final_answer_filter_flushes_text_before_model_call_end() {
        let mut state = RoundState::default();
        let out = run_filter(
            &mut state,
            vec![
                model_start("r2"),
                text_start("r2", "b2"),
                text_delta("r2", "b2", "final answer"),
                text_end("r2", "b2"),
                model_end("r2"),
            ],
        );
        let kinds: Vec<&AgentEventKind> = out.iter().map(|e| &e.kind).collect();
        assert_eq!(kinds.len(), 5, "最终轮：ModelCallStart + 三个文本事件 + ModelCallEnd");
        assert!(matches!(kinds[1], AgentEventKind::TextBlockStart { .. }));
        assert!(matches!(kinds[2], AgentEventKind::TextBlockDelta { .. }));
        assert!(matches!(kinds[3], AgentEventKind::TextBlockEnd { .. }));
        assert!(matches!(kinds[4], AgentEventKind::ModelCallEnd { .. }));
        assert!(state.buffered.is_empty(), "发出后应清空缓冲");
    }

    #[test]
    fn middleware_chain_collects_filters_in_order() {
        let chain = MiddlewareChain::new(vec![
            Arc::new(FinalAnswerFilter),
            Arc::new(FinalAnswerFilter),
        ]);
        assert_eq!(chain.event_filters().len(), 2, "每个中间件按需贡献过滤器");
    }
}

#[cfg(test)]
mod task_middleware_tests {
    use super::*;
    use crate::error::Error;
    use crate::event::{AgentEvent, AgentEventKind};
    use crate::message::{Role, metadata_keys};
    use crate::model::{ChatRequest, ChatResponse};
    use crate::state::{TaskDecision, TaskVerification};

    /// prepare_observed 透传测试的占位模型（不会被调用）。
    struct UnusedModel;

    #[async_trait::async_trait]
    impl Model for UnusedModel {
        async fn call(&self, _request: ChatRequest) -> Result<ChatResponse, Error> {
            unreachable!("prepare_observed tests never call the model")
        }
    }

    fn test_ctx(reply_id: &str) -> RuntimeContext {
        RuntimeContext::new()
            .with_reply_id(reply_id)
            .with_session_id("session-1")
            .with_user_id("user-1")
    }

    fn user_msg(text: &str) -> Msg {
        Msg::user("user", text)
    }

    /// 一条 todo_write 工具回执消息（工具结果以 User 角色回传）。
    fn receipt_msg(call_id: &str, text: &str) -> Msg {
        Msg::new(
            Role::User,
            "tool",
            vec![ContentBlock::tool_result_text(call_id, TODO_TOOL_NAME, text)],
        )
    }

    /// 带两个任务的任务状态（begin_task → revision 1，replace_tasks → revision 2）。
    fn task_state_with_tasks() -> TaskContextState {
        let mut tasks = TaskContextState::default();
        tasks
            .begin_task(TaskScope::new("t-1", "修好登录", "user:req").unwrap(), tasks.revision())
            .unwrap();
        tasks
            .replace_tasks(
                vec![
                    Task::new("alpha", "第一项"),
                    Task::new("beta", "第二项").with_state(TaskState::InProgress),
                ],
                tasks.revision(),
            )
            .unwrap();
        tasks
    }

    // ---------- render_tasks（TodoTools.render 对齐） ----------

    #[test]
    fn render_tasks_matches_todo_tools_format() {
        assert_eq!(render_tasks(&[]), "Todo list cleared (0 items).");
        let tasks = vec![
            Task::new("alpha", ""),
            Task::new("beta", "").with_state(TaskState::InProgress),
            Task::new("gamma", "").with_state(TaskState::Completed),
        ];
        assert_eq!(
            render_tasks(&tasks),
            "2 open todo(s):\n- [ ] alpha\n- [~] beta\n- [x] gamma"
        );
        let mut priority_task = Task::new("delta", "");
        priority_task.metadata.insert("priority".into(), Value::from("high"));
        assert_eq!(
            render_tasks(std::slice::from_ref(&priority_task)),
            "1 open todo(s):\n- [ ] delta (priority: high)"
        );
    }

    // ---------- TaskContextProjection ----------

    #[test]
    fn projection_injects_task_state_when_no_receipt() {
        let tasks = task_state_with_tasks();
        let messages =
            TaskContextProjection::project(vec![user_msg("开始吧")], &tasks, false, false);
        assert_eq!(messages.len(), 2, "应追加一条合成提醒");
        let reminder = &messages[1];
        assert_eq!(reminder.role, Role::User);
        assert_eq!(reminder.name, "system");
        assert_eq!(reminder.id, "harness:todo_state");
        let text = reminder.text();
        assert!(text.starts_with("<TASK_STATE revision=\"2\">"));
        assert!(text.contains(
            "Current todo status (agent-maintained, not independent verification):"
        ));
        assert!(text.contains("- [ ] alpha"));
        assert!(text.contains("- [~] beta"));
        assert!(text.ends_with("</TASK_STATE>"));
        let metadata = reminder.metadata.as_ref().unwrap();
        assert_eq!(
            metadata.get(metadata_keys::SYNTHETIC),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            metadata
                .get(metadata_keys::REMINDER_KIND)
                .and_then(Value::as_str),
            Some("todo_state")
        );
    }

    #[test]
    fn projection_keeps_current_receipt_and_demotes_history() {
        let tasks = task_state_with_tasks();
        let rendered = render_tasks(tasks.tasks());
        let messages = vec![
            user_msg("开始"),
            receipt_msg("call-1", "Todo list cleared (0 items)."),
            receipt_msg("call-2", &rendered),
        ];
        let projected = TaskContextProjection::project(messages, &tasks, false, false);
        assert_eq!(projected.len(), 3, "有当前回执时不再追加 TASK_STATE");
        let ContentBlock::ToolResult { output, .. } = &projected[1].content[0] else {
            panic!("应为工具结果块");
        };
        let ContentBlock::Text { text } = &output[0] else {
            panic!("应为文本");
        };
        assert_eq!(text, STALE_RECEIPT_TEXT, "历史回执应被降级");
        let ContentBlock::ToolResult { output, .. } = &projected[2].content[0] else {
            panic!("应为工具结果块");
        };
        let ContentBlock::Text { text } = &output[0] else {
            panic!("应为文本");
        };
        assert_eq!(text, &rendered, "当前回执必须原样保留");
    }

    #[test]
    fn projection_skips_old_synthetic_state_messages() {
        let tasks = task_state_with_tasks();
        let mut old_reminder = Msg::user(
            "system",
            "<TASK_STATE revision=\"1\">\n旧状态\n</TASK_STATE>",
        )
        .with_metadata(serde_json::json!({
            (metadata_keys::SYNTHETIC): true,
            (metadata_keys::REMINDER_KIND): "todo_state",
        }));
        old_reminder.id = "harness:todo_state".to_string();
        // 旧合成消息里塞一个 todo_write 回执块：合成消息整体跳过，块不参与降级
        old_reminder.content =
            vec![ContentBlock::tool_result_text("call-0", TODO_TOOL_NAME, "旧回执")];
        let messages = vec![old_reminder, user_msg("继续")];
        let projected = TaskContextProjection::project(messages, &tasks, false, false);
        assert_eq!(projected.len(), 3, "旧合成消息保留 + 末尾追加新 TASK_STATE");
        assert_eq!(projected[0].id, "harness:todo_state");
        let ContentBlock::ToolResult { output, .. } = &projected[0].content[0] else {
            panic!("应为工具结果块");
        };
        let ContentBlock::Text { text } = &output[0] else {
            panic!("应为文本");
        };
        assert_eq!(text, "旧回执", "合成消息内的块不参与降级");
        assert!(
            projected[2].text().contains("revision=\"2\""),
            "末尾应注入当前状态"
        );
    }

    #[test]
    fn projection_renders_requirements_and_verifications() {
        let mut tasks = TaskContextState::default();
        tasks
            .begin_task(TaskScope::new("t-1", "修好登录", "user:req").unwrap(), tasks.revision())
            .unwrap();
        let requirement = tasks
            .propose(TaskRequirementKind::AcceptanceCriterion, "能登录", "model:1")
            .unwrap();
        tasks
            .decide(
                &requirement.id,
                TaskRequirementStatus::Confirmed,
                TaskDecision {
                    authority: TaskAuthority::User,
                    source_ref: "user:ok".into(),
                },
                tasks.revision(),
            )
            .unwrap();
        tasks.set_subject_version("commit-abc", tasks.revision()).unwrap();
        let binding = tasks.evidence_binding().unwrap();
        tasks
            .record_verification(
                TaskVerification {
                    id: "v-1".into(),
                    requirement_id: requirement.id.clone(),
                    action_id: "a-1".into(),
                    verifier_id: "tester".into(),
                    outcome: VerificationOutcome::Passed,
                    reason: "登录成功".into(),
                    binding,
                    recorded_at: 1_700_000_000_000,
                },
                tasks.revision(),
            )
            .unwrap();

        let messages =
            TaskContextProjection::project(vec![user_msg("开始")], &tasks, true, true);
        let text = messages.last().unwrap().text();
        assert!(text.contains("Task: t-1"));
        assert!(text.contains("Objective: 修好登录"));
        assert!(text.contains("<REQUIREMENTS>"));
        assert!(text.contains(&requirement.id));
        assert!(text.contains(" | ACCEPTANCE_CRITERION | CONFIRMED | "));
        assert!(text.contains("Decision: USER | user:ok"));
        assert!(text.contains("<VERIFICATION>"));
        assert!(text.contains(" | PASSED | verifier=tester"));
        assert!(text.contains("Checked version: commit-abc"));
    }

    #[test]
    fn projection_escapes_xml_sensitive_characters() {
        let mut tasks = TaskContextState::default();
        tasks
            .begin_task(TaskScope::new("t-1", "a<b>&c", "src").unwrap(), tasks.revision())
            .unwrap();
        tasks
            .replace_tasks(vec![Task::new("x & y <z>", "")], tasks.revision())
            .unwrap();
        let messages =
            TaskContextProjection::project(vec![user_msg("开始")], &tasks, true, false);
        let text = messages.last().unwrap().text();
        assert!(text.contains("Objective: a&lt;b&gt;&amp;c"));
        assert!(text.contains("- [ ] x &amp; y &lt;z&gt;"));
        assert!(!text.contains("<z>"), "原始尖括号不得出现");
    }

    // ---------- TaskReminderMiddleware ----------

    #[test]
    fn system_prompt_grounding_follows_flags() {
        let ctx = test_ctx("reply-grounding");
        let base = "你是助手".to_string();
        let todo_only = TaskReminderMiddleware::new();
        let prompt = todo_only.on_system_prompt(&ctx, base.clone());
        assert!(prompt.starts_with("你是助手"));
        assert!(prompt.contains("## Task List"));
        assert!(!prompt.contains("## Candidate requirements"));
        let req_only = TaskReminderMiddleware::with_flags(false, true);
        let prompt = req_only.on_system_prompt(&ctx, base.clone());
        assert!(!prompt.contains("## Task List"));
        assert!(prompt.contains("## Candidate requirements"));
        let none = TaskReminderMiddleware::with_flags(false, false);
        assert_eq!(none.on_system_prompt(&ctx, base.clone()), base);
    }

    #[test]
    fn active_points_declare_only_needed_hooks() {
        let middleware = TaskReminderMiddleware::new();
        assert_eq!(
            middleware.active_points(),
            &[
                ExtensionPoint::OnSystemPrompt,
                ExtensionPoint::OnReasoning,
                ExtensionPoint::OnAgentStateReady,
            ]
        );
        assert!(middleware.todo_enabled());
        assert!(!middleware.requirements_enabled());
    }

    #[test]
    fn reasoning_projects_state_captured_at_state_ready() {
        let ctx = test_ctx("reply-project");
        let chain = MiddlewareChain::new(vec![Arc::new(TaskReminderMiddleware::new())]);
        let state = Arc::new(Mutex::new(AgentState::default()));
        {
            let mut guard = state.lock().unwrap();
            let rev = guard.tasks_context.revision();
            guard
                .tasks_context
                .begin_task(TaskScope::new("t-1", "目标", "src").unwrap(), rev)
                .unwrap();
            let rev = guard.tasks_context.revision();
            guard
                .tasks_context
                .replace_tasks(vec![Task::new("alpha", "")], rev)
                .unwrap();
        }
        let mut input_msgs = vec![user_msg("开始")];
        chain.notify_state_ready(&ctx, &mut input_msgs, &state);
        let reasoning = chain.apply_reasoning(
            &ctx,
            ReasoningInput {
                messages: vec![user_msg("开始")],
                options: GenerateOptions::default(),
            },
        );
        assert_eq!(reasoning.messages.len(), 2, "应追加 TASK_STATE 合成提醒");
        assert!(reasoning.messages[1].text().contains("- [ ] alpha"));
    }

    #[test]
    fn reasoning_without_state_ready_passes_through() {
        let ctx = test_ctx("reply-passthrough");
        let chain = MiddlewareChain::new(vec![Arc::new(TaskReminderMiddleware::new())]);
        let reasoning = chain.apply_reasoning(
            &ctx,
            ReasoningInput {
                messages: vec![user_msg("开始")],
                options: GenerateOptions::default(),
            },
        );
        assert_eq!(reasoning.messages.len(), 1, "无状态可用时必须原样透传");
    }

    #[test]
    fn reasoning_sees_fresh_state_each_iteration() {
        let ctx = test_ctx("reply-fresh");
        let chain = MiddlewareChain::new(vec![Arc::new(TaskReminderMiddleware::new())]);
        let state = Arc::new(Mutex::new(AgentState::default()));
        let mut input_msgs = vec![user_msg("开始")];
        chain.notify_state_ready(&ctx, &mut input_msgs, &state);

        // 迭代 1：任务状态为空 → 不注入
        let first = chain.apply_reasoning(
            &ctx,
            ReasoningInput {
                messages: vec![user_msg("开始")],
                options: GenerateOptions::default(),
            },
        );
        assert_eq!(first.messages.len(), 1, "空状态不应注入");

        // 模拟 acting 阶段 todo_write 写入任务
        {
            let mut guard = state.lock().unwrap();
            let rev = guard.tasks_context.revision();
            guard
                .tasks_context
                .begin_task(TaskScope::new("t-1", "目标", "src").unwrap(), rev)
                .unwrap();
            let rev = guard.tasks_context.revision();
            guard
                .tasks_context
                .replace_tasks(vec![Task::new("alpha", "")], rev)
                .unwrap();
        }
        // 迭代 2：同一 reply_id 现取现锁 → 看到最新修订
        let second = chain.apply_reasoning(
            &ctx,
            ReasoningInput {
                messages: vec![user_msg("开始")],
                options: GenerateOptions::default(),
            },
        );
        assert_eq!(second.messages.len(), 2);
        let text = second.messages[1].text();
        assert!(text.contains("revision=\"2\""), "每轮都应看到最新修订号");
        assert!(text.contains("- [ ] alpha"));
    }

    // ---------- ModelRequestPreparer ----------

    struct ManifestPreparer {
        manifest: Option<Value>,
        fail: bool,
    }

    impl ModelRequestPreparer for ManifestPreparer {
        fn prepare(
            &self,
            ctx: &RuntimeContext,
            input: ModelCallInput,
            call_id: &str,
            _purpose: Purpose,
        ) -> crate::error::Result<ModelCallInput> {
            if self.fail {
                return Err(Error::InvalidInput("secret source content".to_string()));
            }
            if let Some(manifest) = &self.manifest {
                ctx.put(
                    format!("{MODEL_REQUEST_MANIFEST_PREFIX}{call_id}"),
                    manifest.clone(),
                );
            }
            Ok(input)
        }
    }

    fn event_sink() -> (Arc<Mutex<Vec<AgentEvent>>>, impl FnMut(AgentEvent)) {
        let events: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        let emit = move |event: AgentEvent| {
            sink.lock().unwrap().push(event);
        };
        (events, emit)
    }

    #[test]
    fn prepare_observed_emits_passed_build_event_with_manifest() {
        let ctx = test_ctx("reply-prepare");
        let preparer = ManifestPreparer {
            manifest: Some(Value::from("manifest-1")),
            fail: false,
        };
        let (events, mut emit) = event_sink();
        let input = ModelCallInput::new(
            vec![user_msg("hi")],
            GenerateOptions::default(),
            Arc::new(UnusedModel),
        );
        let result = preparer
            .prepare_observed(&ctx, input, "call-1", Purpose::Reasoning, &mut emit)
            .unwrap();
        assert_eq!(result.messages.len(), 1, "透传型 preparer 不改消息");

        let recorded = events.lock().unwrap();
        assert_eq!(recorded.len(), 1, "成功也应发一条构建事件");
        let AgentEventKind::Custom { name, value } = &recorded[0].kind else {
            panic!("应为自定义事件");
        };
        assert_eq!(name, CONTEXT_BUILD_EVENT_NAME);
        assert_eq!(
            value.get("model_call_id").and_then(Value::as_str),
            Some("call-1")
        );
        assert_eq!(
            value.get("purpose").and_then(Value::as_str),
            Some("REASONING")
        );
        assert_eq!(value.get("status").and_then(Value::as_str), Some("passed"));
        assert_eq!(
            value.get("context_manifest").and_then(Value::as_str),
            Some("manifest-1")
        );
    }

    #[test]
    fn prepare_observed_emits_failed_event_without_error_content() {
        let ctx = test_ctx("reply-prepare-fail");
        // 预置一份上一次尝试的陈旧清单：prepare_observed 必须先重置
        ctx.put(
            format!("{MODEL_REQUEST_MANIFEST_PREFIX}call-2"),
            Value::from("stale-manifest"),
        );
        let preparer = ManifestPreparer {
            manifest: None,
            fail: true,
        };
        let (events, mut emit) = event_sink();
        let input = ModelCallInput::new(
            vec![user_msg("hi")],
            GenerateOptions::default(),
            Arc::new(UnusedModel),
        );
        let error = preparer
            .prepare_observed(&ctx, input, "call-2", Purpose::Summary, &mut emit)
            .unwrap_err();
        assert!(matches!(error, Error::InvalidInput(_)));

        let recorded = events.lock().unwrap();
        assert_eq!(recorded.len(), 1, "失败也要发构建事件");
        let AgentEventKind::Custom { value, .. } = &recorded[0].kind else {
            panic!("应为自定义事件");
        };
        assert_eq!(value.get("status").and_then(Value::as_str), Some("failed"));
        assert_eq!(
            value.get("error_type").and_then(Value::as_str),
            Some("InvalidInput")
        );
        assert!(
            value.get("context_manifest").is_none(),
            "重置后未写清单时不得带清单"
        );
        let serialized = Value::Object(value.clone()).to_string();
        assert!(!serialized.contains("secret"), "错误内容不得进事件载荷");
    }

    #[test]
    fn preparer_defaults_and_purpose_labels() {
        let preparer = ManifestPreparer {
            manifest: None,
            fail: false,
        };
        assert!(!preparer.handles_task_projection(), "默认不接管任务投影");
        assert_eq!(Purpose::Reasoning.as_str(), "REASONING");
        assert_eq!(Purpose::Summary.as_str(), "SUMMARY");
    }
}
