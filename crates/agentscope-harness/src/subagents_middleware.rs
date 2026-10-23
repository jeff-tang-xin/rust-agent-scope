//! 子代理中间件（对应 Java `SubagentsMiddleware`）。
//!
//! 职责（对齐 Java）：
//! 1. 在每轮 reasoning 前，把「已终态未投递」的后台任务聚合成一条
//!    `<system-reminder>` 投递消息——先写入 `AgentState.context`（持久），
//!    再标记已投递（写入成功后才标记，无丢失窗口），最后注入本轮消息。
//! 2. 把稳定的 `## Subagents` 使用指引拼进首条 SYSTEM 消息（字节稳定，
//!    prefix-cache 友好）。
//! 3. 把易变状态（可见子代理目录 + 异步任务摘要）作为 `<RUNTIME_STATE>`
//!    尾消息追加——每轮重建、不累积。
//!
//! 与 Java 的有意差异：
//! - Java 在推理完成后（`doOnComplete`）才 `markDelivered`；Rust 的
//!   `on_reasoning` 是同步变换、无完成回调，改为「写 context 成功后立即
//!   标记」。安全性等价：context 写入成功即消息已持久，标记不会造成丢失；
//!   崩溃窗口只会产生重复投递（冗余但安全），与 Java 的失败方向一致。
//! - Java 在 `onAgent` 里从 namespace-scoped 文件系统重载子代理声明
//!   （per-user 隔离）；Rust 未复刻 remote filesystem，声明在
//!   [`crate::agent::HarnessAgentBuilder`] 装配时加载一次，`on_agent`
//!   保留扩展点不做重载。
//! - `AgentSpawnTool` / `TaskTool` 属 tool 包（后续批次），本中间件只做
//!   提示注入与任务投递推送。

use std::sync::Arc;

use agentscope_core::context::RuntimeContext;
use agentscope_core::message::metadata_keys;
use agentscope_core::message::{Msg, Role};
use agentscope_core::middleware::{ExtensionPoint, Middleware, ReasoningInput};
use agentscope_core::agent::ReActAgent;

use crate::subagent::{DefaultAgentManager, DeclarationMode, SubagentDeclaration};
use crate::task::{TaskDelivery, TaskRepository, TaskStatus};

/// 任务摘要最多列出的条数（对应 Java `MAX_TASK_SUMMARY_ENTRIES`）。
pub const MAX_TASK_SUMMARY_ENTRIES: usize = 10;

/// 单条投递提醒最多聚合的任务数（对应 Java `MAX_DELIVERIES_PER_REMINDER`）；
/// 超出部分在尾行提示「还有 N 条——调用 task_list() 查看」。
pub const MAX_DELIVERIES_PER_REMINDER: usize = 10;

/// 稳定的子代理使用指引段（对应 Java `SUBAGENT_SECTION_TEMPLATE`）。
/// `{spawn}` / `{send}` / `{list}` 为工具名占位（默认模式 agent_spawn /
/// agent_send / agent_list；session 模式为 sessions_*，本批未接线）。
const SUBAGENT_SECTION_TEMPLATE: &str = r#"
## Subagents

You have access to subagent tools for spawning and coordinating isolated subagents.
Subagents are ephemeral — they live only for the duration of the task and return a single result.

### Agent Tools

**`{spawn}`** — Spawn an isolated subagent
- `agent_id` (required): which subagent to instantiate
- `task` (optional): initial prompt; omit to create a persistent session
- `label` (optional): human-readable name for referencing via send
- `timeout_seconds`: wait time; 0=fire-and-forget (returns task_id), default=30, max=600
- Response always includes `agent_key:` (opaque handle) — save it for follow-up sends

**`{send}`** — Send a follow-up message to an existing subagent
- `agent_key`: copy the **full value** after `agent_key:` from spawn output (starts with `agent:`). This is NOT `agent_id`, NOT `session_id`, and NOT `task_id`
- Or use `label` if you set one at spawn (mutually exclusive with agent_key)
- `message` (required): content to send
- `timeout_seconds`: 0=fire-and-forget, >0=wait for reply (default: 30)

**`{list}`** — List active subagents

### Task Tools (for async/background operations)

**`task_output`** — Retrieve the result of a background task by task_id.
- **You rarely need this.** Completed tasks are pushed back to you automatically as a `<system-reminder>` block before your next reasoning step.
- Use `task_output(block=false)` when you need a specific task's latest status/result, the pushed summary was truncated, or you intentionally want to check progress while continuing other reasoning.
- Use `task_output(block=true)` only for one specific task you are ready to wait for; for multiple tasks use `wait_async_results`.

**`wait_async_results`** — Wait for background-task results when the next step depends on them.
- Prefer `wait_async_results(task_ids=...)`: waits until those tasks are terminal and **returns their results in the tool output**.
- Prefer `wait_async_results(wait_all=true)`: waits for the snapshot of currently non-terminal background tasks (tasks started later are not added) and **returns their results**.
- Without `task_ids` and without `wait_all`: legacy **inbox-any** mode — returns when ANY inbox message arrives. This is NOT wait-all; use `task_ids` / `wait_all=true` when every task in a group must finish.

**`task_cancel`** — Cancel a running background task by task_id. No effect on already-completed tasks.

**`task_list`** — List all in-flight background tasks (durable, accurate across compaction and migration). Completed tasks fall off this list after they're pushed to you.

### Background task flow
1. Spawn with `timeout_seconds=0` to fire-and-forget; the response gives you a task_id.
2. Continue with independent work. If you need fresh state, use `task_output(block=false)` for selected tasks.
3. If a later step must wait for a known group, call `wait_async_results(task_ids=...)`; if it must wait for every current background task, call `wait_async_results(wait_all=true)`.
4. If the agent has nothing useful to do, hand control back to the user — they'll prompt again when ready and the next reasoning round will surface any completions.

### Timeout promotion
When a sync spawn/send exceeds its timeout, the task is **not lost** — it is automatically promoted to a background task. You receive `status: timeout_promoted` with a `task_id`. Treat it like any async task: the result will be pushed back to you automatically as a `<system-reminder>`. Do NOT retry the same task — it is already running in the background.

### When to use subagents
- When a task is complex and multi-step, and can be fully delegated in isolation
- When a task is independent of other tasks and can run in parallel
- When a task requires focused reasoning or heavy context usage that would bloat the main thread
- When sandboxing improves reliability (e.g. code analysis, structured searches, data formatting)
- When you only care about the output, not the intermediate steps (e.g. research → synthesized report)

### When NOT to use subagents
- If the task is trivial (a few tool calls or simple lookup)
- If you need to see intermediate reasoning or steps after completion
- If delegating does not reduce token usage, complexity, or context switching
- If splitting would add latency without benefit

### Subagent lifecycle
1. **Spawn** → Provide clear role, instructions, and expected output format
2. **Run** → The subagent completes the task autonomously
3. **Return** → The subagent provides a single structured result
4. **Reconcile** → Incorporate or synthesize the result into the main thread

### Usage patterns
- **Parallel async execution**: Split work by independence/dependency. Launch independent, non-conflicting tasks with `timeout_seconds=0`; continue reasoning, then use `task_output(block=false)` for selective progress checks or `wait_async_results(task_ids=...)` / `wait_async_results(wait_all=true)` when a barrier is required
- **Parallel sync delegation**: Multiple sync `agent_spawn` / `agent_send` calls in one turn run concurrently by default (Toolkit parallel=true); the parent waits for that batch of tool results before continuing. Pass a Toolkit with parallel=false to opt out.
- **Mixed short/long work**: Wait for short prerequisite tasks first, continue reasoning with those results, and merge long-running async results later through `task_output` or `wait_async_results`
- **Sync delegation**: Use default timeout for simple one-shot delegation when one result is needed before the next reasoning step
- **Persistent session**: Spawn without a task, then use send for multi-turn interaction
- **Cancel stale work**: Use task_cancel to stop background tasks that are no longer needed
- Subagent results are NOT visible to the user — always summarize them in your response"#;

/// 渲染 `## Subagents` 指引段（对应 Java `renderSubagentSection`）。
///
/// 只含按配置稳定的指引；易变的代理目录与任务摘要走尾状态消息
/// （见 [`build_subagent_state_msg`]），保证系统头跨步字节稳定。
#[must_use]
pub fn render_subagent_section(session_mode: bool) -> String {
    let (spawn, send, list) = if session_mode {
        ("sessions_spawn", "sessions_send", "sessions_list")
    } else {
        ("agent_spawn", "agent_send", "agent_list")
    };
    SUBAGENT_SECTION_TEMPLATE
        .replace("{spawn}", spawn)
        .replace("{send}", send)
        .replace("{list}", list)
}

/// 把 `extra` 拼进首条 SYSTEM 消息；无 SYSTEM 消息时在头部插入一条
/// （对应 Java `prependToSystemMessage`）。
#[must_use]
pub fn prepend_to_system_message(messages: Vec<Msg>, extra: &str) -> Vec<Msg> {
    if extra.is_empty() {
        return messages;
    }
    let mut out = Vec::with_capacity(messages.len() + 1);
    match messages.first() {
        Some(first) if first.role == Role::System => {
            let existing = first.text();
            let merged = if existing.is_empty() {
                extra.to_string()
            } else {
                format!("{existing}\n{extra}")
            };
            let mut new_sys = Msg::system(first.name.clone(), merged);
            new_sys.id = first.id.clone();
            new_sys.metadata = first.metadata.clone();
            new_sys.timestamp = first.timestamp.clone();
            out.push(new_sys);
            out.extend(messages.into_iter().skip(1));
        }
        _ => {
            out.push(Msg::system("system", extra));
            out.extend(messages);
        }
    }
    out
}

/// 该条目是否应出现在 LLM 可见的子代理清单里（对应 Java `isVisibleToLlm`）：
/// 无声明的编程注册默认可见；`hidden=true` 或 `mode=PRIMARY` 不可见。
#[must_use]
pub fn is_visible_to_llm(declaration: Option<&SubagentDeclaration>) -> bool {
    match declaration {
        None => true,
        Some(decl) => !decl.hidden && decl.mode != DeclarationMode::Primary,
    }
}

/// 渲染可见代理目录行（对应 Java `renderSubagentCatalog`）；无可见条目时为空。
#[must_use]
pub fn render_subagent_catalog(manager: &DefaultAgentManager) -> String {
    manager
        .list_agent_names()
        .iter()
        .filter_map(|name| {
            let declaration = manager.get_declaration(name);
            if !is_visible_to_llm(declaration.as_ref()) {
                return None;
            }
            let description = declaration
                .map(|d| d.description.clone())
                .unwrap_or_default();
            Some(format!("- `{name}`: {description}"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 构造易变状态尾消息（对应 Java `buildSubagentStateMsg`）：可见代理目录 +
/// 异步任务摘要；两者皆空时返回 `None`。
#[must_use]
pub fn build_subagent_state_msg(
    manager: &DefaultAgentManager,
    task_summary: Option<&str>,
) -> Option<Msg> {
    let catalog = render_subagent_catalog(manager);
    let has_tasks = task_summary.is_some_and(|s| !s.trim().is_empty());
    if catalog.is_empty() && !has_tasks {
        return None;
    }
    let mut sb = String::from("<RUNTIME_STATE>\n");
    if !catalog.is_empty() {
        sb.push_str("### Available agent ids\n");
        sb.push_str(&catalog);
        sb.push('\n');
    }
    if let Some(summary) = task_summary {
        sb.push_str(summary.trim());
        sb.push('\n');
    }
    sb.push_str("</RUNTIME_STATE>");
    Some(synthetic_user_msg(sb, "subagent_state"))
}

/// 构造合成消息（role=user、name=system、带 synthetic 元数据）。
fn synthetic_user_msg(text: String, kind: &str) -> Msg {
    Msg::user("system", text).with_metadata(serde_json::json!({
        metadata_keys::SYNTHETIC: true,
        metadata_keys::REMINDER_KIND: kind,
    }))
}

/// 任务状态字面量（对应 Java `stateLiteral`）。
#[must_use]
pub fn state_literal(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "error",
        TaskStatus::Cancelled => "cancelled",
        TaskStatus::Pending => "pending",
        TaskStatus::Running => "running",
    }
}

/// 构造聚合的 `<system-reminder>` 投递消息（对应 Java `buildDeliveryReminder`）：
/// 把「已终态未投递」的后台任务面呈给 LLM。最多
/// [`MAX_DELIVERIES_PER_REMINDER`] 条，超出部分在尾行提示。
#[must_use]
pub fn build_delivery_reminder(deliveries: &[TaskDelivery]) -> Msg {
    let total = deliveries.len();
    let shown = total.min(MAX_DELIVERIES_PER_REMINDER);
    let mut sb = String::from("<system-reminder>\n");
    sb.push_str(&total.to_string());
    sb.push_str(if total == 1 {
        " background subagent task has completed since your last turn. "
    } else {
        " background subagent tasks have completed since your last turn. "
    });
    sb.push_str("These results are now part of your conversation history.\n\n");
    for delivery in &deliveries[..shown] {
        sb.push_str("<task id=\"");
        sb.push_str(&delivery.task_id);
        sb.push_str("\" state=\"");
        sb.push_str(state_literal(delivery.status));
        sb.push('"');
        sb.push_str(" agent=\"");
        sb.push_str(&delivery.agent_id);
        sb.push_str("\">\n");
        match delivery.status {
            TaskStatus::Completed => {
                sb.push_str("<task_result>\n");
                sb.push_str(delivery.result.as_deref().unwrap_or(""));
                sb.push_str("\n</task_result>\n");
            }
            TaskStatus::Failed => {
                sb.push_str("<task_error>\n");
                sb.push_str(delivery.error_message.as_deref().unwrap_or("(no error message)"));
                sb.push_str("\n</task_error>\n");
            }
            TaskStatus::Cancelled => {
                sb.push_str("Task was cancelled before producing a result.\n");
            }
            _ => {
                sb.push_str("Task ended in non-terminal state ");
                sb.push_str(state_literal(delivery.status));
                sb.push('\n');
            }
        }
        sb.push_str("</task>\n");
    }
    if total > shown {
        sb.push_str(&format!(
            "\n... and {} more — call task_list() to inspect.\n",
            total - shown
        ));
    }
    sb.push_str("\nIf you need a single task's full output, call task_output(task_id=..., block=false).\n");
    sb.push_str("</system-reminder>");
    synthetic_user_msg(sb, "task_delivery")
}

/// 构造当前会话的在途任务摘要（对应 Java `buildTaskSummary`）；无可见任务时
/// 返回 `None`。已投递的终态任务被过滤——它们已作为 `<system-reminder>` 进了
/// 对话历史，重复罗列只会浪费 token 并诱导 LLM 重新拉取结果。
#[must_use]
pub fn build_task_summary(
    repo: &dyn TaskRepository,
    ctx: &RuntimeContext,
    session_id: &str,
) -> Option<String> {
    let tasks = repo.list_tasks(ctx, session_id, None);
    if tasks.is_empty() {
        return None;
    }
    // 注意锁序：先取 task_id 并释放 record 锁，再调 is_delivered——
    // is_delivered → get_task → settle 会再拿同一把 record 锁，
    // 临时 guard 活到语句结束会造成同线程自死锁。
    let visible: Vec<_> = tasks
        .iter()
        .filter(|task| {
            let task_id = task
                .record
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .task_id
                .clone();
            !repo.is_delivered(ctx, session_id, &task_id)
        })
        .collect();
    if visible.is_empty() {
        return None;
    }
    let mut sb = String::from("\n### Async tasks (current session)\n");
    for (count, task) in visible.iter().enumerate() {
        if count >= MAX_TASK_SUMMARY_ENTRIES {
            sb.push_str(&format!(
                "- ... ({} more — use task_list() to see all)\n",
                visible.len() - MAX_TASK_SUMMARY_ENTRIES
            ));
            break;
        }
        let record = task.record.lock().unwrap_or_else(|e| e.into_inner());
        sb.push_str("- task_id: ");
        sb.push_str(&record.task_id);
        sb.push_str("  agent: ");
        sb.push_str(&record.sub_agent_id);
        sb.push_str("  status: ");
        sb.push_str(state_literal(record.status));
        sb.push_str("  started: ");
        sb.push_str(&format_epoch_short(record.created_at));
        sb.push('\n');
    }
    sb.push_str(
        "(Completed/failed/cancelled tasks are pushed back to you as <system-reminder> blocks, \
         not listed here — no need to call task_output unless you need the full payload.)\n",
    );
    Some(sb)
}

/// epoch 毫秒 → `HH:mm'Z'`（UTC）短格式（对应 Java `ISO_SHORT`）。
fn format_epoch_short(epoch_millis: i64) -> String {
    let secs = epoch_millis.div_euclid(1000).rem_euclid(86_400);
    let (h, m) = (secs / 3600, (secs % 3600) / 60);
    format!("{h:02}:{m:02}Z")
}

/// 子代理中间件：把子代理机制面呈给主代理（提示注入 + 任务投递推送）。
///
/// 持有 [`ReActAgent`] 引用用于把投递消息写进 `AgentState.context`
/// （对应 Java `RuntimeContext.resolveAgentState(rc, reAct)`——Java 从
/// RuntimeContext 解析 agent，Rust 侧 RuntimeContext 是纯属性袋，改为
/// 构造时注入 delegate 引用）。
pub struct SubagentsMiddleware {
    /// delegate 引用槽：`HarnessAgent` 装配时先建中间件、后建 delegate，
    /// 构建完成后经 [`Self::bind_agent`] 回填。用 `Weak` 避免互相持有
    /// （delegate 的 middlewares 持有本中间件）造成泄漏。
    agent_slot: Arc<std::sync::OnceLock<std::sync::Weak<ReActAgent>>>,
    manager: Arc<DefaultAgentManager>,
    task_repository: Arc<dyn TaskRepository>,
}

impl SubagentsMiddleware {
    /// 新建中间件（对应 Java 默认模式构造器；session 模式本批未接线）。
    ///
    /// delegate 引用由 [`Self::bind_agent`] 在装配完成后回填。
    pub fn new(
        manager: Arc<DefaultAgentManager>,
        task_repository: Arc<dyn TaskRepository>,
    ) -> Self {
        Self {
            agent_slot: Arc::new(std::sync::OnceLock::new()),
            manager,
            task_repository,
        }
    }

    /// 回填 delegate 引用（`HarnessAgent` 装配的最后一步）。
    pub fn bind_agent(&self, agent: &Arc<ReActAgent>) {
        let _ = self.agent_slot.set(Arc::downgrade(agent));
    }

    /// 把投递消息写进会话状态的 `context`（持久）；状态不存在时静默跳过
    /// （对应 Java 写失败时降级为「仅本轮注入」）。
    fn append_delivery_to_context(&self, ctx: &RuntimeContext, session_id: &str, msg: &Msg) {
        let Some(agent) = self.agent_slot.get().and_then(|weak| weak.upgrade()) else {
            return;
        };
        let user_id = ctx.user_id();
        let state = agent.state_for(user_id, session_id);
        if let Ok(Some(mut state)) = state {
            state.context.push(msg.clone());
            let _ = agent.save_agent_state_for(user_id, session_id, &state);
        }
    }
}

impl Middleware for SubagentsMiddleware {
    fn name(&self) -> &str {
        "subagents"
    }

    fn active_points(&self) -> &[ExtensionPoint] {
        &[ExtensionPoint::OnAgent, ExtensionPoint::OnReasoning]
    }

    fn on_agent(&self, _ctx: &RuntimeContext, input: agentscope_core::middleware::AgentInput) -> agentscope_core::middleware::AgentInput {
        // Java 在此从 namespace-scoped 文件系统重载子代理声明（per-user 隔离）；
        // Rust 未复刻 remote filesystem，声明在 HarnessAgent 装配时加载一次。
        // 保留扩展点：后续接入 workspace 重载时在此刷新 manager。
        input
    }

    fn on_reasoning(&self, ctx: &RuntimeContext, input: ReasoningInput) -> ReasoningInput {
        let session_id = ctx.session_id().unwrap_or("").to_string();

        // ---- Phase B-3 投递推送 -------------------------------------------------
        // 先 drain 新终态任务，让随后构造的系统摘要可以省略它们。
        let pending = self.task_repository.find_pending_deliveries(ctx, &session_id);
        let mut messages = input.messages;
        if !pending.is_empty() {
            let delivery_msg = build_delivery_reminder(&pending);
            // 先持久化（写 context），成功后再标记——顺序保证崩溃只产生
            // 重复投递（冗余但安全），不会丢消息。
            self.append_delivery_to_context(ctx, &session_id, &delivery_msg);
            for delivery in pending.iter().take(MAX_DELIVERIES_PER_REMINDER) {
                self.task_repository
                    .mark_delivered(ctx, &session_id, &delivery.task_id);
            }
            // 本轮也注入（与 AgentState 写入并行——无论框架下一轮何时从状态
            // 重导消息，本轮 LLM 调用立即可见）。
            messages.push(delivery_msg);
        }

        // 稳定指引段 → 首条 SYSTEM 消息（字节稳定，prefix-cache 友好）。
        let mut messages = prepend_to_system_message(messages, &render_subagent_section(false));

        // 易变状态 → 尾消息（每轮重建，不累积）。
        let task_summary = build_task_summary(self.task_repository.as_ref(), ctx, &session_id);
        if let Some(state_msg) = build_subagent_state_msg(&self.manager, task_summary.as_deref()) {
            messages.push(state_msg);
        }

        ReasoningInput {
            messages,
            options: input.options,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::HarnessAgentBuilder;
    use crate::agent::GENERAL_PURPOSE_NAME;
    use crate::subagent::SubagentEntry;
    use crate::task::{TaskRunSpec, TaskStatus, WorkspaceTaskRepository};
    use crate::workspace::WorkspaceManager;
    use agentscope_core::error::Error;
    use agentscope_core::message::ContentBlock;
    use agentscope_core::message::Role;
    use agentscope_core::model::{ChatRequest, ChatResponse, Model};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    struct MockModel {
        responses: Mutex<Vec<ChatResponse>>,
    }

    impl MockModel {
        fn single(text: &str) -> Arc<dyn Model> {
            Arc::new(Self {
                responses: Mutex::new(vec![ChatResponse {
                    content: vec![ContentBlock::Text {
                        text: text.to_string(),
                    }],
                    usage: None,
                    finish_reason: Some("stop".to_string()),
                }]),
            })
        }
    }

    #[async_trait::async_trait]
    impl Model for MockModel {
        async fn call(&self, _request: ChatRequest) -> Result<ChatResponse, Error> {
            self.responses
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| Error::Model("no more mock responses".into()))
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "subagents-mw-{tag}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn synthetic_kind(msg: &Msg) -> Option<String> {
        msg.metadata
            .as_ref()
            .and_then(|m| m.get(metadata_keys::REMINDER_KIND))
            .and_then(|v| v.as_str().map(str::to_string))
    }

    // ---------- 纯函数 ----------

    #[test]
    fn render_subagent_section_uses_default_tool_names() {
        let section = render_subagent_section(false);
        assert!(section.starts_with("\n## Subagents"));
        assert!(section.contains("**`agent_spawn`**"));
        assert!(section.contains("**`agent_send`**"));
        assert!(section.contains("**`agent_list`**"));
        // session 模式换名
        let session = render_subagent_section(true);
        assert!(session.contains("sessions_spawn"));
        assert!(!session.contains("**`agent_spawn`**"));
    }

    #[test]
    fn prepend_to_system_message_merges_head_or_inserts() {
        // 有 SYSTEM 头 → 合并（保留 id/metadata/timestamp）
        let mut sys = Msg::system("system", "base prompt");
        sys.id = "sys-1".to_string();
        sys.timestamp = Some("2026-01-01T00:00:00Z".to_string());
        let user = Msg::user("user", "hi");
        let out = prepend_to_system_message(vec![sys, user], "\n## Subagents");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].role, Role::System);
        assert_eq!(out[0].id, "sys-1");
        assert_eq!(out[0].timestamp.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert!(out[0].text().starts_with("base prompt\n\n## Subagents"));
        assert_eq!(out[1].role, Role::User);
        // 无 SYSTEM 头 → 头部插入
        let out = prepend_to_system_message(vec![Msg::user("user", "hi")], "extra");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].role, Role::System);
        assert_eq!(out[0].text(), "extra");
        // 空 extra → 原样返回
        let out = prepend_to_system_message(vec![Msg::user("user", "hi")], "");
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn is_visible_to_llm_filters_hidden_and_primary() {
        // 无声明 → 可见
        assert!(is_visible_to_llm(None));
        // 默认声明（mode=All，非 hidden）→ 可见
        let decl = SubagentDeclaration::new("worker", "does work");
        assert!(is_visible_to_llm(Some(&decl)));
        // hidden → 不可见
        let decl = decl.with_hidden(true);
        assert!(!is_visible_to_llm(Some(&decl)));
        // PRIMARY → 不可见
        let decl = SubagentDeclaration::new("boss", "primary")
            .with_mode(DeclarationMode::Primary);
        assert!(!is_visible_to_llm(Some(&decl)));
    }

    #[test]
    fn build_subagent_state_msg_carries_catalog_and_tasks() {
        let manager = Arc::new(DefaultAgentManager::new());
        manager.refresh_entries(vec![
            SubagentEntry::new("worker", None, Arc::new(|_rc: &RuntimeContext| {
                Err("unused".to_string())
            })),
            SubagentEntry::new(
                "ghost",
                Some(SubagentDeclaration::new("ghost", "hidden one").with_hidden(true)),
                Arc::new(|_rc: &RuntimeContext| Err("unused".to_string())),
            ),
        ]);
        // 目录 + 任务摘要
        let msg = build_subagent_state_msg(&manager, Some("\n### Async tasks\n- task_id: t1"))
            .unwrap();
        let text = msg.text();
        assert!(text.starts_with("<RUNTIME_STATE>\n### Available agent ids\n"));
        assert!(text.contains("- `worker`:"));
        // 无声明的编程注册条目 description 为空
        assert!(text.contains("- `worker`: \n") || text.contains("- `worker`:
"));
        assert!(!text.contains("ghost"), "hidden entry must be filtered out");
        assert!(text.contains("### Async tasks"));
        assert!(text.ends_with("</RUNTIME_STATE>"));
        assert_eq!(synthetic_kind(&msg).as_deref(), Some("subagent_state"));
        // 两者皆空 → None
        let empty = Arc::new(DefaultAgentManager::new());
        assert!(build_subagent_state_msg(&empty, None).is_none());
        assert!(build_subagent_state_msg(&empty, Some("  ")).is_none());
    }

    #[test]
    fn build_delivery_reminder_aggregates_and_caps() {
        let deliveries = vec![
            TaskDelivery {
                task_id: "t1".to_string(),
                agent_id: "worker".to_string(),
                status: TaskStatus::Completed,
                result: Some("done!".to_string()),
                error_message: None,
                completed_at: None,
            },
            TaskDelivery {
                task_id: "t2".to_string(),
                agent_id: "worker".to_string(),
                status: TaskStatus::Failed,
                result: None,
                error_message: Some("boom".to_string()),
                completed_at: None,
            },
        ];
        let msg = build_delivery_reminder(&deliveries);
        let text = msg.text();
        assert!(text.starts_with("<system-reminder>\n2 background subagent tasks have"));
        assert!(text.contains("<task id=\"t1\" state=\"completed\" agent=\"worker\">"));
        assert!(text.contains("<task_result>\ndone!\n</task_result>"));
        assert!(text.contains("<task id=\"t2\" state=\"error\" agent=\"worker\">"));
        assert!(text.contains("<task_error>\nboom\n</task_error>"));
        assert!(text.ends_with("</system-reminder>"));
        assert_eq!(synthetic_kind(&msg).as_deref(), Some("task_delivery"));
        // 超量截断提示：MAX 条全展示，其余只出现在尾行计数里
        let extra = 3;
        let many: Vec<TaskDelivery> = (0..(MAX_DELIVERIES_PER_REMINDER + extra))
            .map(|i| TaskDelivery {
                task_id: format!("t{i}"),
                agent_id: "worker".to_string(),
                status: TaskStatus::Completed,
                result: Some("ok".to_string()),
                error_message: None,
                completed_at: None,
            })
            .collect();
        let text = build_delivery_reminder(&many).text();
        assert!(text.contains(&format!(
            "... and {extra} more — call task_list() to inspect."
        )));
        // 最后一条展示的是 t{MAX-1}，第一条被截断的是 t{MAX}
        let last_shown = format!("id=\"t{}\"", MAX_DELIVERIES_PER_REMINDER - 1);
        let first_hidden = format!("id=\"t{}\"", MAX_DELIVERIES_PER_REMINDER);
        assert!(text.contains(&last_shown));
        assert!(!text.contains(&first_hidden));
    }

    #[tokio::test]
    async fn build_task_summary_skips_delivered_and_caps() {
        let workspace = temp_dir("summary");
        let manager = Arc::new(WorkspaceManager::new(&workspace));
        let repo = WorkspaceTaskRepository::new(manager.clone(), "parent");
        let ctx = RuntimeContext::new().with_session_id("sess-1");
        // 无任务 → None
        assert!(build_task_summary(&repo, &ctx, "sess-1").is_none());
        // 提交两个任务（一个已投递）
        repo.put_task(
            &ctx,
            "t1",
            "worker",
            "sess-1",
            TaskRunSpec::Local {
                execution: Arc::new(|| Box::pin(async { Ok("r1".to_string()) })),
            },
        )
        .unwrap();
        repo.put_task(
            &ctx,
            "t2",
            "worker",
            "sess-1",
            TaskRunSpec::Local {
                execution: Arc::new(|| Box::pin(async { Ok("r2".to_string()) })),
            },
        )
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        repo.mark_delivered(&ctx, "sess-1", "t1");
        let summary = build_task_summary(&repo, &ctx, "sess-1").unwrap();
        assert!(summary.contains("### Async tasks (current session)"));
        assert!(!summary.contains("task_id: t1"), "delivered task must be skipped");
        assert!(summary.contains("task_id: t2"));
        assert!(summary.contains("status: completed"));
        repo.shutdown();
        std::fs::remove_dir_all(&workspace).ok();
    }

    // ---------- 集成：HarnessAgent 装配 + on_reasoning ----------

    #[tokio::test]
    async fn harness_agent_wires_subagents_middleware() {
        let workspace = temp_dir("wired");
        let agent = HarnessAgentBuilder::new()
            .model(MockModel::single("ok"))
            .workspace(&workspace)
            .build()
            .unwrap();
        // 默认装配（general-purpose 注册 → middleware 存在）
        assert!(agent.subagents_middleware().is_some());
        // disable_subagents 时不装配
        let bare = HarnessAgentBuilder::new()
            .model(MockModel::single("ok"))
            .workspace(&workspace)
            .disable_subagents()
            .build()
            .unwrap();
        assert!(bare.subagents_middleware().is_none());
        agent.close();
        bare.close();
        std::fs::remove_dir_all(&workspace).ok();
    }

    #[tokio::test]
    async fn on_reasoning_injects_section_state_and_delivery() {
        let workspace = temp_dir("inject");
        let agent = HarnessAgentBuilder::new()
            .model(MockModel::single("ok"))
            .workspace(&workspace)
            .build()
            .unwrap();
        let middleware = agent.subagents_middleware().unwrap().clone();

        // 先建立会话状态（call 一次）
        let ctx = RuntimeContext::new().with_session_id("sess-9");
        agent
            .call(RuntimeContext::new().with_session_id("sess-9"), Msg::user("user", "hi"))
            .await
            .unwrap();

        // 提交一个会完成的任务
        let repo = agent.task_repository().unwrap();
        repo.put_task(
            &ctx,
            "t9",
            "worker",
            "sess-9",
            TaskRunSpec::Local {
                execution: Arc::new(|| Box::pin(async { Ok("payload-9".to_string()) })),
            },
        )
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;

        // 触发 on_reasoning
        let input = ReasoningInput {
            messages: vec![Msg::system("system", "base"), Msg::user("user", "hi")],
            options: Default::default(),
        };
        let out = middleware.on_reasoning(&ctx, input);
        let messages = &out.messages;

        // 1) 稳定指引段并入 SYSTEM 头
        assert_eq!(messages[0].role, Role::System);
        assert!(messages[0].text().starts_with("base\n\n## Subagents"));
        // 2) 投递提醒在尾部
        let delivery = messages
            .iter()
            .find(|m| synthetic_kind(m).as_deref() == Some("task_delivery"))
            .expect("delivery reminder injected");
        assert!(delivery.text().contains("payload-9"));
        assert_eq!(delivery.role, Role::User);
        // 3) 运行态尾消息（目录 + 任务摘要）
        let state = messages
            .iter()
            .find(|m| synthetic_kind(m).as_deref() == Some("subagent_state"))
            .expect("state message injected");
        assert!(state.text().contains("### Available agent ids"));
        assert!(state.text().contains(GENERAL_PURPOSE_NAME));
        // 已投递任务不再出现在摘要里
        assert!(!state.text().contains("task_id: t9"));
        // 4) 投递消息已持久化进 AgentState.context
        let state_after = agent
            .delegate()
            .state_for(None, "sess-9")
            .unwrap()
            .expect("state exists");
        assert!(state_after
            .context
            .iter()
            .any(|m| synthetic_kind(m).as_deref() == Some("task_delivery")));
        // 5) 已标记投递：再次 on_reasoning 不再注入
        let out2 = middleware.on_reasoning(
            &ctx,
            ReasoningInput {
                messages: vec![Msg::system("system", "base")],
                options: Default::default(),
            },
        );
        assert!(out2
            .messages
            .iter()
            .all(|m| synthetic_kind(m).as_deref() != Some("task_delivery")));
        agent.close();
        std::fs::remove_dir_all(&workspace).ok();
    }

    #[tokio::test]
    async fn on_reasoning_without_tasks_only_adds_section_and_state() {
        let workspace = temp_dir("clean");
        let agent = HarnessAgentBuilder::new()
            .model(MockModel::single("ok"))
            .workspace(&workspace)
            .build()
            .unwrap();
        let middleware = agent.subagents_middleware().unwrap().clone();
        let ctx = RuntimeContext::new().with_session_id("sess-clean");
        let out = middleware.on_reasoning(
            &ctx,
            ReasoningInput {
                messages: vec![Msg::system("system", "base")],
                options: Default::default(),
            },
        );
        // SYSTEM 头 + 运行态尾消息，无投递
        assert_eq!(out.messages.len(), 2);
        assert!(out.messages[0].text().contains("## Subagents"));
        assert_eq!(
            synthetic_kind(&out.messages[1]).as_deref(),
            Some("subagent_state")
        );
        agent.close();
        std::fs::remove_dir_all(&workspace).ok();
    }
}
