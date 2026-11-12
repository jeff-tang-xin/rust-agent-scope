//! HarnessAgent：`ReActAgent` 的 harness 编排外壳（对应 Java `agent.HarnessAgent`
//! 与 `HarnessAgentBuilderSupport`）。
//!
//! 职责（对齐 Java `buildInstance` 装配顺序，裁剪到已实现组件）：
//! 1. 独立 toolkit（`Toolkit::copy` 语义——子代理工厂各自拷贝，不与用户工具串扰）；
//! 2. workspace 装配（显式 manager > 显式路径 > 默认 `.agents/<agent_id>/workspace`）；
//! 3. 从 `workspace/subagents/*.md` 加载声明式子代理 + 固定 `general-purpose` 条目
//!    + 自定义工厂条目，注册进 [`DefaultAgentManager`]；
//! 4. 默认任务仓库 [`WorkspaceTaskRepository`]（workspace 持久化 + 孤儿清扫）；
//! 5. 计划模式（[`PlanModeManager`]，路径记录进会话状态）。
//!
//! 与 Java 的有意差异（后续批次补齐，见 lib.rs 与 ARCHITECTURE.md §3.11）：
//! - **子代理工厂构建 [`ReActAgent`] 而非递归构建 `HarnessAgent`**：Java 递归装配
//!   完整 harness（workspace 上下文中间件、filesystem 工具、skill 等）；Rust 侧这些
//!   包未复刻，子代理只注入系统提示（声明 AGENTS.md/inline body + 子代理上下文节），
//!   workspace 上下文（AGENTS.md/MEMORY.md 自动注入）随 middleware 包批次补齐；
//! - sandbox / skill / MCP / teams / filesystem 工具 / CompactionMiddleware 不在本批；
//! - `SubagentsMiddleware`（task 工具接线）随 middleware 包批次实现；
//! - `workspace_for` 本地模式无 per-user 命名空间（lib.rs 已声明），共享同一 manager。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentscope_core::agent::ReActAgent;
use agentscope_core::context::RuntimeContext;
use agentscope_core::message::Msg;
use agentscope_core::middleware::Middleware;
use agentscope_core::model::Model;
use agentscope_core::session::SessionLogStore;
use agentscope_core::state::AgentStateStore;
use agentscope_core::permission::PermissionContextState;
use agentscope_core::tool::Toolkit;
use tokio::sync::mpsc;

use crate::compaction::{compact_in_state, CompactionConfig, ConversationCompactor};
use agentscope_core::agent::IterationHook;
use crate::memory::MemoryFlushManager;
use crate::plan::{PlanModeManager, PlanModeMiddleware};
use crate::tools::{PlanExitTool, PlanWriteTool};
use crate::subagent::{
    AgentSpecLoader, DefaultAgentManager, SubagentDeclaration, SubagentEntry,
    SubagentFactory, WorkspaceMode,
};
use crate::subagents_middleware::SubagentsMiddleware;
use crate::task::{TaskRepository, WorkspaceTaskRepository};
use crate::tools_agent_spawn::{AgentListTool, AgentSendTool, AgentSpawnCore, AgentSpawnTool};
use crate::workspace::WorkspaceManager;

/// 注入每个子代理系统提示的上下文节（逐字对齐 Java `SUBAGENT_CONTEXT_SECTION`）：
/// 确立叶子工人的身份、规则、输出格式与禁止行为。任务本身作为首条用户消息投递，
/// 不在此重复。
pub const SUBAGENT_CONTEXT_SECTION: &str = r#"# Subagent Context

You are a **subagent** spawned by the main agent for a specific task.

## Your Role
- Complete the assigned task. That's your entire purpose.
- You are NOT the main agent. Don't try to be.

## Rules
1. **Stay focused** — Do your assigned task, nothing else
2. **Complete the task** — Your final message will be automatically reported to the main agent
3. **Don't initiate** — No heartbeats, no proactive actions, no side quests
4. **Be ephemeral** — You may be terminated after task completion. That's fine.
5. **Recover from truncated tool output** — If you see `[truncated: output exceeded context limit]`, re-read only what you need using smaller chunks (read with offset/limit, or targeted grep/head/tail) instead of full re-reads
6. **Land everything in your final message** — It is the ONLY thing the main agent receives. Every conclusion, finding, and answer must be in it, and there must be no tool calls after it: your run ends there
7. **Cite evidence** — Back each claim with `path:line` references (e.g. `src/Main.java:42`) so the main agent can verify without re-reading
8. **No report files** — Do not write your results to a file as the deliverable; the main agent reads your final text message only. Write files only when the task itself asks for them

## Output Format
When complete, your final response should include:
- What you accomplished or found
- Any relevant details the main agent should know
- Keep it concise but informative

## What You DON'T Do
- NO user conversations (that's the main agent's job)
- NO spawning further subagents — you are a leaf worker
- NO pretending to be the main agent
- Return plain text results; let the main agent deliver them to the user
"#;

/// `general-purpose` 子代理的基础提示（对齐 Java `GENERAL_PURPOSE_BASE_PROMPT`）。
pub const GENERAL_PURPOSE_BASE_PROMPT: &str = "You are a highly capable general-purpose subagent.";

/// `general-purpose` 条目名（对齐 Java 固定条目）。
pub const GENERAL_PURPOSE_NAME: &str = "general-purpose";

/// MEMORY.md 注入系统提示的字符上限：超出按字符截断（UTF-8 安全），
/// 细节引导模型用 memory_search 查询每日台账。
const MEMORY_INJECT_CHARS: usize = 12_000;

/// `general-purpose` 条目描述（对齐 Java 固定描述）。
pub const GENERAL_PURPOSE_DESCRIPTION: &str = "General-purpose subagent with same capabilities as the main agent. Use for any isolated task that can be fully delegated.";

/// 子代理模型覆盖解析器：按声明里的模型名解析出模型实例；返回 `None` 回落父模型。
pub type ModelResolver = Arc<dyn Fn(&str) -> Option<Arc<dyn Model>> + Send + Sync>;

// -----------------------------------------------------------------
//  纯函数（对应 HarnessAgentBuilderSupport 的静态方法）
// -----------------------------------------------------------------

/// 组装子代理系统提示：基础提示 + 上下文节；基础为空时只用上下文节
/// （对齐 Java `buildSubagentSysPrompt`）。
pub fn build_subagent_sys_prompt(base_prompt: &str) -> String {
    let base = base_prompt.trim_end();
    if base.is_empty() {
        SUBAGENT_CONTEXT_SECTION.to_string()
    } else {
        format!("{base}\n\n{SUBAGENT_CONTEXT_SECTION}")
    }
}

/// 标识符安全化：`null`/空白 → `None`；路径混淆字符（斜杠、反斜杠、空白、控制符）
/// 替换为下划线（对齐 Java `sanitizeIdentifier`）。
pub fn sanitize_identifier(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(
        trimmed
            .chars()
            .map(|c| {
                if c == '/' || c == '\\' || c.is_whitespace() || c.is_control() {
                    '_'
                } else {
                    c
                }
            })
            .collect(),
    )
}

/// 组装子代理的持久化会话 id，按声明名与派生时的父身份分桶（对齐 Java
/// `deriveChildSessionId`）：`{name}[@{parentSessionId}][#{userId}]`。
///
/// 空/空白段丢弃；`SHARED` 子代理有意回落单桶形式（与父共享全部状态树）。
pub fn derive_child_session_id(decl: &SubagentDeclaration, parent_rc: &RuntimeContext) -> String {
    if decl.workspace_mode == WorkspaceMode::Shared {
        return decl.name.clone();
    }
    let sid = parent_rc.session_id().and_then(sanitize_identifier);
    let uid = parent_rc.user_id().and_then(sanitize_identifier);
    if sid.is_none() && uid.is_none() {
        return decl.name.clone();
    }
    let mut id = decl.name.clone();
    if let Some(sid) = &sid {
        id.push('@');
        id.push_str(sid);
    }
    if let Some(uid) = &uid {
        id.push('#');
        id.push_str(uid);
    }
    id
}

/// 解析声明式子代理的运行时 workspace 根；ISOLATED 且无路径时自动创建
/// `agents/<name>/workspace/`（对齐 Java `resolveDeclaredWorkspace`）。
pub fn resolve_declared_workspace(decl: &SubagentDeclaration, main_workspace: &Path) -> PathBuf {
    if let Some(path) = &decl.workspace_path {
        if decl.workspace_mode == WorkspaceMode::Shared {
            return main_workspace.to_path_buf();
        }
        return PathBuf::from(path);
    }
    if decl.workspace_mode == WorkspaceMode::Shared {
        return main_workspace.to_path_buf();
    }
    let isolated = main_workspace
        .join("agents")
        .join(&decl.name)
        .join("workspace");
    if let Err(error) = std::fs::create_dir_all(&isolated) {
        eprintln!(
            "failed to create isolated workspace for subagent '{}' at {}: {error}",
            decl.name,
            isolated.display()
        );
    }
    isolated
}

/// 解析声明式子代理的系统提示基础：声明目录的 `AGENTS.md` 内容，或 inline body
/// （对齐 Java `resolveDeclaredSysPromptBase`）。
pub fn resolve_declared_sys_prompt_base(decl: &SubagentDeclaration) -> String {
    if let Some(path) = &decl.workspace_path {
        let agents_md = PathBuf::from(path).join("AGENTS.md");
        if agents_md.is_file() {
            return std::fs::read_to_string(&agents_md).unwrap_or_default();
        }
        return String::new();
    }
    decl.inline_agents_body.clone().unwrap_or_default()
}

/// 继承父工具集并按可选白名单裁剪（防御性拷贝，对齐 Java
/// `allowlistedInheritedToolkit`）：白名单为空 → 全量继承；否则删除不在名单内的工具。
pub fn allowlisted_inherited_toolkit(
    parent_toolkit: Option<&Toolkit>,
    allowlist: &[String],
) -> Toolkit {
    let mut toolkit = match parent_toolkit {
        Some(parent) => parent.copy(),
        None => Toolkit::new(),
    };
    if allowlist.is_empty() {
        return toolkit;
    }
    let to_remove: Vec<String> = toolkit
        .schemas()
        .into_iter()
        .map(|schema| schema.name)
        .filter(|name| !allowlist.contains(name))
        .collect();
    for name in to_remove {
        toolkit.remove_tool(&name);
    }
    toolkit
}

/// 解析子代理生效模型：覆盖名缺失/解析失败回落父模型（对齐 Java `resolveModel`）。
fn resolve_model(
    model_override: Option<&str>,
    parent_model: &Arc<dyn Model>,
    resolver: Option<&ModelResolver>,
) -> Arc<dyn Model> {
    let Some(name) = model_override.map(str::trim).filter(|s| !s.is_empty()) else {
        return parent_model.clone();
    };
    if let Some(resolver) = resolver {
        if let Some(model) = resolver(name) {
            return model;
        }
    }
    parent_model.clone()
}

// -----------------------------------------------------------------
//  子代理工厂（构建 ReActAgent——有意差异，见模块文档）
// -----------------------------------------------------------------

/// `general-purpose` 工厂：共享父 workspace、继承父工具集与模型、短命无持久化
/// （对齐 Java `buildGeneralPurposeFactory` 的核心语义，裁剪到已实现组件）。
pub(crate) fn build_general_purpose_factory(
    parent_model: &Arc<dyn Model>,
    parent_toolkit: Option<&Toolkit>,
    max_iters: Option<usize>,
    enable_pending_tool_recovery: Option<bool>,
) -> Arc<SubagentFactory> {
    let model = parent_model.clone();
    let toolkit = allowlisted_inherited_toolkit(parent_toolkit, &[]);
    Arc::new(move |_parent_rc: &RuntimeContext| {
        let mut builder = ReActAgent::builder()
            .name("general-purpose-subagent")
            .sys_prompt(build_subagent_sys_prompt(GENERAL_PURPOSE_BASE_PROMPT))
            .model(model.clone())
            .toolkit(toolkit.copy());
        if let Some(max_iters) = max_iters {
            builder = builder.max_iters(max_iters);
        }
        // 声明里没写就继承父设置（tri-state：None = 继承）。
        if let Some(enable) = enable_pending_tool_recovery {
            builder = builder.enable_pending_tool_recovery(enable);
        }
        builder
            .build()
            .map(Arc::new)
            .map_err(|error| format!("failed to build general-purpose subagent: {error}"))
    })
}

/// 声明式子代理工厂（对齐 Java `buildDeclaredFactory` 的核心语义）：
/// 会话 id 分桶、AGENTS.md/inline 系统提示、workspace 决策表、工具白名单、
/// 模型覆盖、`persist_session` 持久化、`steps` 迭代上限。
pub(crate) fn build_declared_factory(
    decl: &SubagentDeclaration,
    parent_model: &Arc<dyn Model>,
    parent_toolkit: Option<&Toolkit>,
    model_resolver: Option<&ModelResolver>,
    main_workspace: &Path,
    state_store: Option<Arc<dyn AgentStateStore>>,
    max_iters: Option<usize>,
    parent_enable_pending_tool_recovery: Option<bool>,
) -> Arc<SubagentFactory> {
    let decl = decl.clone();
    let parent_model = parent_model.clone();
    let toolkit = allowlisted_inherited_toolkit(parent_toolkit, &decl.tools);
    let model_resolver = model_resolver.cloned();
    let main_workspace = main_workspace.to_path_buf();
    Arc::new(move |parent_rc: &RuntimeContext| {
        let child_session_id = derive_child_session_id(&decl, parent_rc);
        let base = resolve_declared_sys_prompt_base(&decl);
        let sys_prompt = build_subagent_sys_prompt(&base);
        let workspace = resolve_declared_workspace(&decl, &main_workspace);
        let model = resolve_model(decl.model.as_deref(), &parent_model, model_resolver.as_ref());

        // workspace 仅在 resolve_declared_workspace 里用于 AGENTS.md 解析与
        // 隔离目录创建；ReActAgent 本身无 workspace 概念（差异声明见模块文档）。
        let _ = &workspace;
        let mut builder = ReActAgent::builder()
            .name(&decl.name)
            .sys_prompt(sys_prompt)
            .model(model)
            .toolkit(toolkit.copy());
        if decl.steps > 0 {
            builder = builder.max_iters(decl.steps as usize);
        } else if let Some(max_iters) = max_iters {
            builder = builder.max_iters(max_iters);
        }
        // 孤儿工具调用恢复：声明显式指定优先，否则继承父设置（tri-state）。
        let enable_pending_tool_recovery = decl
            .enable_pending_tool_recovery
            .or(parent_enable_pending_tool_recovery);
        if let Some(enable) = enable_pending_tool_recovery {
            builder = builder.enable_pending_tool_recovery(enable);
        }
        if decl.persist_session {
            if let Some(store) = &state_store {
                builder = builder.state_store(store.clone());
            }
            builder = builder.default_session_id(child_session_id);
        }
        builder
            .build()
            .map(Arc::new)
            .map_err(|error| format!("failed to build subagent '{}': {error}", decl.name))
    })
}

// -----------------------------------------------------------------
//  HarnessAgent
// -----------------------------------------------------------------

/// Harness 编排外壳：包装 [`ReActAgent`]，附加 workspace、计划模式、子代理管理
/// 与任务仓库（对应 Java `HarnessAgent`）。
pub struct HarnessAgent {
    delegate: Arc<ReActAgent>,
    agent_id: String,
    default_session_id: Option<String>,
    subagents_middleware: Option<Arc<SubagentsMiddleware>>,
    workspace_manager: Option<Arc<WorkspaceManager>>,
    plan_mode_manager: Option<Arc<PlanModeManager>>,
    subagent_manager: Option<Arc<DefaultAgentManager>>,
    task_repository: Option<Arc<dyn TaskRepository>>,
    /// 手动压缩（[`HarnessAgent::compact_now`]）所需的运行时：与迭代钩子共用
    /// 同一 compactor/config/窗口（build 时一并构造；压缩禁用则为 None）。
    compaction: Option<Arc<CompactionRuntime>>,
}

/// 压缩运行时：`build()` 里构造迭代钩子时顺带留存，供手动 `/compact` 复用。
struct CompactionRuntime {
    compactor: Arc<ConversationCompactor>,
    config: CompactionConfig,
    window: usize,
}

impl HarnessAgent {
    /// 单轮调用（会话 id 缺省回落 `default_session_id` / agent id）。
    pub async fn call(
        &self,
        runtime: RuntimeContext,
        msg: Msg,
    ) -> Result<Msg, agentscope_core::Error> {
        let runtime = self.ensure_session_defaults(runtime);
        self.delegate.call_with_context(runtime, msg).await
    }

    /// 流式调用：返回事件接收端（会话 id 缺省规则同 [`HarnessAgent::call`]）。
    /// 执行句柄在此丢弃（detach）；需要中断能力的调用方直接用
    /// `delegate().stream_events_with_context` 拿句柄。
    pub async fn stream_events(
        &self,
        runtime: RuntimeContext,
        msg: Msg,
    ) -> Result<mpsc::Receiver<agentscope_core::event::AgentEvent>, agentscope_core::Error> {
        let runtime = self.ensure_session_defaults(runtime);
        let (rx, _task) = self
            .delegate
            .stream_events_with_context(runtime, msg)
            .await?;
        Ok(rx)
    }

    /// 运行中插入消息（steer）：ReAct 循环在下一步开始前注入（对应 Java
    /// `pushSteer`）。调用已结束则留在队列里，由调用方结算时取走。
    pub fn push_steer(&self, session_id: impl Into<String>, msg: Msg) {
        self.delegate.push_steer(session_id, msg);
    }

    /// 会话 id 缺省：`RuntimeContext.session_id` > builder 的 `default_session_id` >
    /// agent id（对齐 Java `ensureSessionDefaults` 的会话部分；workspace 属性袋注入
    /// 随 SubagentsMiddleware 批次补齐）。
    fn ensure_session_defaults(&self, mut runtime: RuntimeContext) -> RuntimeContext {
        let blank = runtime.session_id().is_none_or(|id| id.trim().is_empty());
        if blank {
            let fallback = self
                .default_session_id
                .clone()
                .unwrap_or_else(|| self.agent_id.clone());
            runtime = runtime.with_session_id(fallback);
        }
        runtime
    }

    /// 取某会话的 workspace 管理器。本地模式无 per-user 命名空间（lib.rs 差异
    /// 声明），恒返回装配时的共享 manager。
    pub fn workspace_for(
        &self,
        _user_id: Option<&str>,
        _session_id: &str,
    ) -> Result<Arc<WorkspaceManager>, String> {
        self.workspace_manager
            .clone()
            .ok_or_else(|| "workspace is not configured".to_string())
    }

    /// 进入计划模式（幂等）：置位会话状态标志并补默认计划文件路径，返回路径。
    pub fn enter_plan_mode(
        &self,
        user_id: Option<&str>,
        session_id: &str,
    ) -> Result<String, String> {
        let Some(plan) = &self.plan_mode_manager else {
            return Err("plan mode is not enabled on this agent".to_string());
        };
        let mut state = self
            .delegate
            .agent_state_for(user_id, session_id)
            .map_err(|error| error.to_string())?;
        let path = plan.enter(&mut state);
        self.delegate
            .save_agent_state_for(user_id, session_id, &state)
            .map_err(|error| error.to_string())?;
        Ok(path)
    }

    /// 退出计划模式（回到 BUILD，幂等）：仅清标志，保留计划文件路径供参考。
    pub fn exit_plan_mode(&self, user_id: Option<&str>, session_id: &str) -> Result<(), String> {
        let Some(plan) = &self.plan_mode_manager else {
            return Err("plan mode is not enabled on this agent".to_string());
        };
        let mut state = self
            .delegate
            .agent_state_for(user_id, session_id)
            .map_err(|error| error.to_string())?;
        plan.exit(&mut state);
        self.delegate
            .save_agent_state_for(user_id, session_id, &state)
            .map_err(|error| error.to_string())
    }

    /// 会话是否处于计划模式。
    pub fn is_plan_mode_active(&self, user_id: Option<&str>, session_id: &str) -> bool {
        let Some(plan) = &self.plan_mode_manager else {
            return false;
        };
        self.delegate
            .agent_state_for(user_id, session_id)
            .ok()
            .as_ref()
            .is_some_and(|state| plan.is_plan_active(Some(state)))
    }

    /// 当前会话关联的计划文件路径（未启用计划模式时返回 `None`）。
    pub fn plan_file_path(&self, user_id: Option<&str>, session_id: &str) -> Option<String> {
        let plan = self.plan_mode_manager.as_ref()?;
        let state = self.delegate.agent_state_for(user_id, session_id).ok();
        Some(plan.plan_file_path(state.as_ref()))
    }

    /// 清空某会话的对话上下文（身份与非对话状态保留；不取消进行中的调用）。
    pub fn clear_context(&self, user_id: Option<&str>, session_id: &str) -> Result<(), String> {
        self.delegate
            .clear_context_for(user_id, session_id)
            .map_err(|error| error.to_string())
    }

    /// 手动强制压缩某会话的对话上下文（`/compact` 命令入口）：以 forced=true
    /// 跑一次与迭代钩子相同的压缩逻辑（摘要模型蒸馏旧消息为一条摘要），
    /// 返回压缩说明（如「57 条消息合并为 1 条摘要」）。
    ///
    /// 与自动压缩的区别：无视触发阈值直接压（`/compact` 的语义就是「现在压」）；
    /// 不发事件（调用方——ACP 层——自行向 client 发 `compaction_update` 帧）。
    /// 无可压缩内容（对话为空 / 全为系统与合成消息）返回 `Ok(None)`。
    pub async fn compact_now(
        &self,
        user_id: Option<&str>,
        session_id: &str,
    ) -> Result<Option<String>, String> {
        let Some(compaction) = &self.compaction else {
            return Err("上下文压缩未启用".into());
        };
        // 管理性访问拿状态克隆（Legacy 模式），压缩后写回——/compact 只在
        // 空闲 turn 发出（ACP 层 busy 防重入），与运行中 call 无并发写。
        let state = self
            .delegate
            .agent_state_for(user_id, session_id)
            .map_err(|error| error.to_string())?;
        let shared = Arc::new(std::sync::Mutex::new(state));
        let rc = RuntimeContext::new().with_session_id(session_id.to_owned());
        let detail = compact_in_state(
            &rc,
            &compaction.compactor,
            &compaction.config,
            &shared,
            compaction.window,
            true,
            None,
        )
        .await;
        if detail.is_some() {
            let compacted = shared.lock().unwrap_or_else(|error| error.into_inner());
            self.delegate
                .save_agent_state_for(user_id, session_id, &compacted)
                .map_err(|error| error.to_string())?;
        }
        Ok(detail)
    }

    /// 淘汰全部槽位缓存（持久层保留）。
    pub fn clear_state_cache(&self) {
        self.delegate.clear_state_cache();
    }

    /// 关停：中止后台任务执行体（对应 Java `close()` 的任务仓库 shutdown）。
    pub fn close(&self) {
        if let Some(repo) = &self.task_repository {
            repo.shutdown();
        }
    }

    /// 被包装的 [`ReActAgent`]。
    pub fn delegate(&self) -> &Arc<ReActAgent> {
        &self.delegate
    }

    /// agent id（name 缺省 `ReActAgent`）。
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// workspace 管理器（未配置时 `None`）。
    pub fn workspace_manager(&self) -> Option<&Arc<WorkspaceManager>> {
        self.workspace_manager.as_ref()
    }

    /// 计划模式管理器（未启用时 `None`）。
    pub fn plan_mode_manager(&self) -> Option<&Arc<PlanModeManager>> {
        self.plan_mode_manager.as_ref()
    }

    /// 子代理管理器（禁用/叶子模式时 `None`）。
    pub fn subagent_manager(&self) -> Option<&Arc<DefaultAgentManager>> {
        self.subagent_manager.as_ref()
    }

    /// 子代理中间件（禁用/叶子模式时 `None`；测试与后续 gateway 接线用）。
    pub fn subagents_middleware(&self) -> Option<&Arc<SubagentsMiddleware>> {
        self.subagents_middleware.as_ref()
    }

    /// 任务仓库。
    pub fn task_repository(&self) -> Option<&Arc<dyn TaskRepository>> {
        self.task_repository.as_ref()
    }
}

// -----------------------------------------------------------------
//  Builder
// -----------------------------------------------------------------

/// [`HarnessAgent`] 构建器（对应 Java `HarnessAgent.Builder`，裁剪到已实现组件：
/// sandbox/skill/MCP/teams/filesystem 工具/compaction 等配置项随对应批次补齐）。
pub struct HarnessAgentBuilder {
    name: Option<String>,
    sys_prompt: Option<String>,
    model: Option<Arc<dyn Model>>,
    toolkit: Option<Toolkit>,
    max_iters: Option<usize>,
    workspace: Option<PathBuf>,
    workspace_manager: Option<Arc<WorkspaceManager>>,
    subagent_declarations: Vec<SubagentDeclaration>,
    custom_subagent_entries: Vec<SubagentEntry>,
    disable_subagents: bool,
    as_leaf_subagent: bool,
    task_repository: Option<Arc<dyn TaskRepository>>,
    state_store: Option<Arc<dyn AgentStateStore>>,
    session_log_store: Option<Arc<dyn SessionLogStore>>,
    default_session_id: Option<String>,
    enable_plan_mode: bool,
    plan_file_dir: Option<String>,
    /// plan 态场景提示词（透传给 `PlanModeMiddleware::with_plan_prompt`）。
    plan_prompt: Option<String>,
    middlewares: Vec<Arc<dyn Middleware>>,
    model_resolver: Option<ModelResolver>,
    /// 新会话的初始权限上下文（透传给 delegate ReActAgent，见
    /// `ReActAgentBuilder::permission_context`）。
    permission_context: Option<PermissionContextState>,
    /// 上下文压缩配置（默认开启，对齐 Java `CompactionConfig.builder().build()`；
    /// `disable_compaction` 置 None）。
    compaction_config: Option<CompactionConfig>,
    /// 记忆工作区（flush/consolidation 的落盘根）：`None` 用主工作区。
    /// studio 把它锚到 `<项目>/.ox`，与 memory_save 工具同根——记忆不落项目根。
    memory_workspace: Option<Arc<WorkspaceManager>>,
    /// 孤儿工具调用恢复策略（对应 Java `enablePendingToolRecovery`）：
    /// `Some(true)` 开启，`Some(false)` 关闭，`None` = 不指定（子代理继承父设置）。
    enable_pending_tool_recovery: Option<bool>,
}

impl HarnessAgentBuilder {
    /// 空构建器。
    pub fn new() -> Self {
        Self {
            name: None,
            sys_prompt: None,
            model: None,
            toolkit: None,
            max_iters: None,
            workspace: None,
            workspace_manager: None,
            subagent_declarations: Vec::new(),
            custom_subagent_entries: Vec::new(),
            disable_subagents: false,
            as_leaf_subagent: false,
            task_repository: None,
            state_store: None,
            session_log_store: None,
            default_session_id: None,
            enable_plan_mode: false,
            plan_file_dir: None,
            plan_prompt: None,
            middlewares: Vec::new(),
            model_resolver: None,
            permission_context: None,
            compaction_config: Some(CompactionConfig::default()),
            memory_workspace: None,
            enable_pending_tool_recovery: None,
        }
    }

    /// agent 名（缺省 `ReActAgent`）。
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// 系统提示。
    pub fn sys_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.sys_prompt = Some(prompt.into());
        self
    }

    /// 模型（必填）。
    pub fn model(mut self, model: Arc<dyn Model>) -> Self {
        self.model = Some(model);
        self
    }

    /// 工具集（缺省空 toolkit；子代理工厂各自拷贝，不与用户工具串扰）。
    pub fn toolkit(mut self, toolkit: Toolkit) -> Self {
        self.toolkit = Some(toolkit);
        self
    }

    /// 最大迭代轮数。
    pub fn max_iters(mut self, max_iters: usize) -> Self {
        self.max_iters = Some(max_iters);
        self
    }

    /// 孤儿工具调用恢复策略（对应 Java `enablePendingToolRecovery`）。
    ///
    /// 开启后，历史里「有 tool_use 无 tool_result」的调用会被补上**错误**结果，
    /// 使后续模型请求满足工具调用配对约束（否则 OpenAI 兼容协议直接 400）。
    ///
    /// `None`（默认）= 不指定：主 agent 用核心层默认值（关闭），子代理继承父设置。
    pub fn enable_pending_tool_recovery(mut self, enable: Option<bool>) -> Self {
        self.enable_pending_tool_recovery = enable;
        self
    }

    /// 新会话的初始权限上下文（透传给 delegate ReActAgent）。
    pub fn permission_context(mut self, context: PermissionContextState) -> Self {
        self.permission_context = Some(context);
        self
    }

    /// 自定义上下文压缩配置（默认开启，默认值对齐 Java `CompactionConfig`）。
    pub fn compaction(mut self, config: CompactionConfig) -> Self {
        self.compaction_config = Some(config);
        self
    }

    /// 禁用上下文压缩（对齐 Java `disableCompaction`）。
    pub fn disable_compaction(mut self) -> Self {
        self.compaction_config = None;
        self
    }

    /// workspace 根目录（与 [`HarnessAgentBuilder::workspace_manager`] 二选一）。
    pub fn workspace(mut self, workspace: impl Into<PathBuf>) -> Self {
        self.workspace = Some(workspace.into());
        self
    }

    /// 直接注入装配好的 workspace 管理器（优先于 [`HarnessAgentBuilder::workspace`]）。
    pub fn workspace_manager(mut self, manager: Arc<WorkspaceManager>) -> Self {
        self.workspace_manager = Some(manager);
        self
    }

    /// 追加声明式子代理（`workspace/subagents/*.md` 之外的手工声明）。
    pub fn subagent(mut self, declaration: SubagentDeclaration) -> Self {
        self.subagent_declarations.push(declaration);
        self
    }

    /// 追加自定义子代理条目（自带工厂）。
    pub fn custom_subagent(mut self, entry: SubagentEntry) -> Self {
        self.custom_subagent_entries.push(entry);
        self
    }

    /// 禁用子代理编排（不加载声明、不注册 general-purpose）。
    pub fn disable_subagents(mut self) -> Self {
        self.disable_subagents = true;
        self
    }

    /// 叶子模式：本代理自身不派生子代理。
    pub fn as_leaf_subagent(mut self) -> Self {
        self.as_leaf_subagent = true;
        self
    }

    /// 显式任务仓库（缺省 [`WorkspaceTaskRepository`]）。
    pub fn task_repository(mut self, repo: Arc<dyn TaskRepository>) -> Self {
        self.task_repository = Some(repo);
        self
    }

    /// 状态库（透传给 delegate 与子代理持久化）。
    pub fn state_store(mut self, store: Arc<dyn AgentStateStore>) -> Self {
        self.state_store = Some(store);
        self
    }

    /// 会话日志库（透传给 delegate）。
    pub fn session_log_store(mut self, store: Arc<dyn SessionLogStore>) -> Self {
        self.session_log_store = Some(store);
        self
    }

    /// 默认会话 id。
    pub fn default_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.default_session_id = Some(session_id.into());
        self
    }

    /// 启用计划模式。
    /// 记忆工作区（flush/consolidation 落盘根；`None` 用主工作区）。
    /// studio 锚到 `<项目>/.ox`，与 memory_save 工具同根。
    pub fn with_memory_workspace(mut self, ws: Arc<WorkspaceManager>) -> Self {
        self.memory_workspace = Some(ws);
        self
    }

    pub fn enable_plan_mode(mut self) -> Self {
        self.enable_plan_mode = true;
        self
    }

    /// 计划文件目录（工作区相对，缺省 `plans`）。
    pub fn plan_file_directory(mut self, dir: impl Into<String>) -> Self {
        self.plan_file_dir = Some(dir.into());
        self
    }

    /// plan 态场景提示词（进入计划模式时注入系统提示，替换内置 banner）。
    pub fn plan_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.plan_prompt = Some(prompt.into());
        self
    }

    /// 追加中间件（透传给 delegate）。
    pub fn middleware(mut self, middleware: Arc<dyn Middleware>) -> Self {
        self.middlewares.push(middleware);
        self
    }

    /// 子代理模型覆盖解析器。
    pub fn model_resolver(mut self, resolver: ModelResolver) -> Self {
        self.model_resolver = Some(resolver);
        self
    }

    /// 装配（对齐 Java `buildInstance` 顺序，裁剪到已实现组件）。
    pub fn build(self) -> Result<HarnessAgent, String> {
        let agent_id = self
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or("ReActAgent")
            .to_string();
        let model = self.model.ok_or_else(|| "model is required".to_string())?;
        // 压缩器与 flush 共用主模型（对齐 Java：compactionModel 缺省用主 model）；
        // 先 clone，model 本体随后 move 进 delegate。
        let compaction_model = model.clone();

        // 1) workspace：显式 manager > 显式路径 > 默认 `.agents/<agent_id>/workspace`
        let workspace_manager = match self.workspace_manager {
            Some(manager) => manager,
            None => Arc::new(WorkspaceManager::new(self.workspace.unwrap_or_else(|| {
                PathBuf::from(format!(".agents/{agent_id}/workspace"))
            }))),
        };

        // 2) 声明加载：builder 手工声明 + `workspace/subagents/*.md`
        let mut declarations = self.subagent_declarations;
        if !self.disable_subagents {
            let dir = workspace_manager.root().join("subagents");
            if dir.is_dir() {
                let loader = AgentSpecLoader::new(
                    workspace_manager.root().to_string_lossy().to_string(),
                );
                declarations.extend(loader.load_from_directory(&dir));
            }
        }

        // 3) 任务仓库：显式 > workspace 持久化（Java 默认）
        let task_repository: Arc<dyn TaskRepository> = self.task_repository.unwrap_or_else(|| {
            Arc::new(WorkspaceTaskRepository::new(
                workspace_manager.clone(),
                agent_id.clone(),
            ))
        });

        // 4) 工具箱与子代理管理器（先于 delegate 构建——SubagentsMiddleware
        //    要注册进 delegate 的中间件链，需要 manager 与任务仓库就绪）
        let mut toolkit = self.toolkit.unwrap_or_default();
        let subagent_manager = if self.disable_subagents || self.as_leaf_subagent {
            None
        } else {
            let manager = Arc::new(DefaultAgentManager::new());
            let mut entries: Vec<SubagentEntry> = Vec::new();
            let has_general_purpose = declarations
                .iter()
                .any(|decl| decl.name == GENERAL_PURPOSE_NAME);
            if !has_general_purpose {
                entries.push(SubagentEntry::new(
                    GENERAL_PURPOSE_NAME,
                    None,
                    build_general_purpose_factory(
                        &model,
                        Some(&toolkit),
                        self.max_iters,
                        self.enable_pending_tool_recovery,
                    ),
                ));
            }
            for decl in &declarations {
                entries.push(SubagentEntry::new(
                    decl.name.clone(),
                    Some(decl.clone()),
                    build_declared_factory(
                        decl,
                        &model,
                        Some(&toolkit),
                        self.model_resolver.as_ref(),
                        workspace_manager.root(),
                        self.state_store.clone(),
                        self.max_iters,
                        self.enable_pending_tool_recovery,
                    ),
                ));
            }
            entries.extend(self.custom_subagent_entries);
            manager.refresh_entries(entries);
            Some(manager)
        };

        // 5) SubagentsMiddleware：子代理清单注入系统提示 + 任务投递推送。
        //    delegate 引用在构建完成后经 bind_agent 回填（Weak 防循环持有）。
        let subagents_middleware = subagent_manager.as_ref().map(|manager| {
            Arc::new(SubagentsMiddleware::new(
                manager.clone(),
                task_repository.clone(),
            ))
        });
        // 5.4) 子代理派发工具：SubagentsMiddleware 的系统提示（SUBAGENT_SECTION_TEMPLATE）
        //      承诺了 agent_spawn / agent_send / agent_list，这三个工具必须真的进
        //      function list——提示词承诺了工具面没有的工具，模型就会按提示调用、
        //      收到 "tool not registered"，然后在既有工具里乱选（曾导致模型想派
        //      子代理却反复误调 browser_navigate）。注册在子代理工厂固化 toolkit
        //      快照之后：子代理是 leaf（提示词禁止再派生），不该拿到派发工具。
        if let Some(manager) = &subagent_manager {
            let core = AgentSpawnCore::new(
                Arc::clone(manager),
                Arc::clone(&task_repository),
                0, // HarnessAgentBuilder 装配的是顶层主代理
            );
            toolkit.register(Arc::new(AgentSpawnTool::new(Arc::clone(&core))));
            toolkit.register(Arc::new(AgentSendTool::new(Arc::clone(&core))));
            toolkit.register(Arc::new(AgentListTool::new(core)));
        }
        let mut plan_middleware: Option<Arc<PlanModeMiddleware>> = None;

        // 5.5) 计划模式：plan 三工具 + PlanModeMiddleware（对齐 Java HarnessAgent
        //      2972-2992 的 enablePlanMode 装配）。必须在 delegate builder 之前——
        //      工具要进 toolkit（760 行 copy 给 delegate），中间件要进中间件链。
        //      只读谓词从 toolkit 按名查（对齐 Java toolkit.isReadOnly 谓词）。
        let plan_mode_manager = if self.enable_plan_mode {
            let manager = Arc::new(match self.plan_file_dir {
                Some(dir) => PlanModeManager::with_dir(dir),
                None => PlanModeManager::new(),
            });
            // plan_enter 不注册：plan 模式由用户在宿主 UI 开关（set_plan_mode），
            // 模型无权自行进入——否则模型看到 plan 工具就自进 plan（曾导致
            // 「用户没开 plan 但 AI 一直认为自己在 plan 里」）。plan_write 供
            // 用户开启后的写计划用，plan_exit 供模型完成规划后退出（ask 审批）。
            toolkit.register(Arc::new(PlanWriteTool::new(
                Arc::clone(&manager),
                Arc::clone(&workspace_manager),
            )));
            toolkit.register(Arc::new(PlanExitTool::new(Arc::clone(&manager))));
            let ro_toolkit = toolkit.copy();
            let is_read_only: Arc<dyn Fn(&str) -> bool + Send + Sync> = Arc::new(move |name| {
                ro_toolkit.get(name).map(|t| t.is_read_only()).unwrap_or(false)
            });
            let mut plan_mw = PlanModeMiddleware::new(is_read_only, Vec::new());
            if let Some(prompt) = &self.plan_prompt {
                plan_mw = plan_mw.with_plan_prompt(prompt.clone());
            }
            plan_middleware = Some(Arc::new(plan_mw));
            Some(manager)
        } else {
            None
        };

        // 6) 上下文压缩（默认开启；flush 复用 MemoryFlushManager 的默认抽取提示词）。
        // 压缩经 IterationHook 挂在每轮 ReAct 迭代边界（对齐 Java 每轮 onReasoning），
        // 闭包只捕获 compactor/config/window，不捕获 delegate（避免 Arc 循环引用）。
        let compaction_window = model.context_window_size();
        let mut iteration_hook = None;
        let compaction_runtime = match self.compaction_config {
            Some(config) => {
                let flush = Arc::new(MemoryFlushManager::new(
                    Arc::clone(
                        self.memory_workspace
                            .as_ref()
                            .unwrap_or(&workspace_manager),
                    ),
                    Arc::clone(&compaction_model),
                ));
                let compactor = Arc::new(ConversationCompactor::new(compaction_model, flush));
                let runtime = Arc::new(CompactionRuntime {
                    compactor: Arc::clone(&compactor),
                    config: config.clone(),
                    window: compaction_window,
                });
                let hook: IterationHook = {
                    let compactor = Arc::clone(&compactor);
                    let config = config.clone();
                    Arc::new(move |rc, state, forced, sink| {
                        let compactor = Arc::clone(&compactor);
                        let config = config.clone();
                        Box::pin(async move {
                            // 返回值（压缩说明）只对手动 /compact 有意义，钩子路径丢弃。
                            let _ = compact_in_state(
                                rc,
                                &compactor,
                                &config,
                                state,
                                compaction_window,
                                forced,
                                sink,
                            )
                            .await;
                            Ok(())
                        })
                    })
                };
                iteration_hook = Some(hook);
                Some(runtime)
            }
            None => None,
        };

        // 7) delegate ReActAgent
        let mut builder = ReActAgent::builder()
            .name(&agent_id)
            .model(model.clone());
        // 启动即载入 MEMORY.md（对齐 Easy-Claw 平台的 memory_context 机制）：
        // 新会话带着过往沉淀的上下文——记忆管线的「读」不依赖模型主动 search，
        // 否则每次启动都是失忆状态（工具在、管线死）。超出上限按字符截断，
        // 细节引导模型用 memory_search 查询。
        let sys_prompt = self.sys_prompt.map(|prompt| {
            self.memory_workspace
                .as_ref()
                .and_then(|ws| {
                    let md = std::fs::read_to_string(ws.root().join("MEMORY.md")).ok()?;
                    let md = md.trim();
                    (!md.is_empty()).then(|| {
                        let mut injected = md.to_owned();
                        if injected.chars().count() > MEMORY_INJECT_CHARS {
                            injected = injected.chars().take(MEMORY_INJECT_CHARS).collect();
                            injected.push_str("\n…（已截断，细节用 memory_search 查询）");
                        }
                        format!("{prompt}\n### 已载入的 MEMORY.md\n{injected}\n")
                    })
                })
                .unwrap_or(prompt)
        });
        if let Some(prompt) = sys_prompt {
            builder = builder.sys_prompt(prompt);
        }
        builder = builder.toolkit(toolkit.copy());
        if let Some(max_iters) = self.max_iters {
            builder = builder.max_iters(max_iters);
        }
        if let Some(enable) = self.enable_pending_tool_recovery {
            builder = builder.enable_pending_tool_recovery(enable);
        }
        if let Some(store) = &self.state_store {
            builder = builder.state_store(store.clone());
        }
        if let Some(store) = self.session_log_store {
            builder = builder.session_log_store(store);
        }
        if let Some(session_id) = &self.default_session_id {
            builder = builder.default_session_id(session_id.clone());
        }
        if let Some(context) = self.permission_context {
            builder = builder.permission_context(context);
        }
        for middleware in self.middlewares {
            builder = builder.middleware(middleware);
        }
        if let Some(middleware) = &subagents_middleware {
            builder = builder.middleware(middleware.clone());
        }
        if let Some(middleware) = &plan_middleware {
            builder = builder.middleware(middleware.clone());
        }
        if let Some(hook) = iteration_hook {
            builder = builder.iteration_hook(hook);
        }
        let delegate = Arc::new(builder.build().map_err(|error| error.to_string())?);
        if let Some(middleware) = &subagents_middleware {
            middleware.bind_agent(&delegate);
        }

        Ok(HarnessAgent {
            delegate,
            agent_id,
            default_session_id: self.default_session_id,
            subagents_middleware,
            workspace_manager: Some(workspace_manager),
            plan_mode_manager,
            subagent_manager,
            task_repository: Some(task_repository),
            compaction: compaction_runtime,
        })
    }
}

impl Default for HarnessAgentBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{TaskRecord, TaskRunSpec, TaskStatus};
    use agentscope_core::error::Error;
    use agentscope_core::message::ContentBlock;
    use agentscope_core::model::ChatResponse;
    use agentscope_core::tool::{Tool, ToolOutput};
    use serde_json::{json, Value};
    use std::sync::Mutex;

    /// 预置响应序列的 Mock 模型（对齐 core 测试写法）。
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
        async fn call(&self, _request: agentscope_core::model::ChatRequest) -> Result<ChatResponse, Error> {
            self.responses
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| Error::Model("no more mock responses".into()))
        }
    }

    /// 固定名工具（白名单过滤测试用）。
    struct EchoTool {
        name: &'static str,
    }

    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "echo input"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        async fn call(&self, input: Value) -> Result<ToolOutput, Error> {
            Ok(ToolOutput::ok(input.to_string()))
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "harness-agent-{tag}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // ---------- 纯函数 ----------

    #[test]
    fn sanitize_identifier_strips_path_confusers() {
        assert_eq!(
            sanitize_identifier("a/b\\c d").as_deref(),
            Some("a_b_c_d")
        );
        assert_eq!(sanitize_identifier("  "), None);
        assert_eq!(sanitize_identifier(""), None);
        assert_eq!(sanitize_identifier("sess-1").as_deref(), Some("sess-1"));
    }

    #[test]
    fn derive_child_session_id_buckets_by_parent_identity() {
        let mut decl = SubagentDeclaration::new("worker", "does work");
        let rc = RuntimeContext::new()
            .with_session_id("main sess")
            .with_user_id("u/1");
        // ISOLATED + 父身份 → name@sid#uid（空白/斜杠安全化）
        assert_eq!(derive_child_session_id(&decl, &rc), "worker@main_sess#u_1");
        // SHARED → 单桶（声明名）
        decl = decl.with_workspace_mode(WorkspaceMode::Shared);
        assert_eq!(derive_child_session_id(&decl, &rc), "worker");
        // 无父身份 → 声明名
        decl = decl.with_workspace_mode(WorkspaceMode::Isolated);
        assert_eq!(
            derive_child_session_id(&decl, &RuntimeContext::new()),
            "worker"
        );
    }

    #[test]
    fn build_subagent_sys_prompt_appends_section() {
        let with_base = build_subagent_sys_prompt("You are a worker.");
        assert!(with_base.starts_with("You are a worker.\n\n# Subagent Context"));
        let empty = build_subagent_sys_prompt("   ");
        assert!(empty.starts_with("# Subagent Context"));
        assert_eq!(empty, SUBAGENT_CONTEXT_SECTION);
    }

    #[test]
    fn resolve_declared_workspace_decision_table() {
        let main = temp_dir("ws-main");
        // ISOLATED + 无路径 → 自动建 agents/<name>/workspace
        let decl = SubagentDeclaration::new("worker", "d");
        let isolated = resolve_declared_workspace(&decl, &main);
        assert_eq!(
            isolated,
            main.join("agents").join("worker").join("workspace")
        );
        assert!(isolated.is_dir());
        // ISOLATED + 有路径 → 声明路径
        let custom = main.join("custom-ws");
        let decl = decl.with_workspace_path(custom.to_string_lossy().to_string());
        assert_eq!(resolve_declared_workspace(&decl, &main), custom);
        // SHARED → 主 workspace（即使有路径）
        let decl = decl.with_workspace_mode(WorkspaceMode::Shared);
        assert_eq!(resolve_declared_workspace(&decl, &main), main);
        std::fs::remove_dir_all(&main).ok();
    }

    #[test]
    fn resolve_declared_sys_prompt_base_prefers_agents_md() {
        let dir = temp_dir("sysprompt");
        std::fs::write(dir.join("AGENTS.md"), "# worker rules").unwrap();
        let decl = SubagentDeclaration::new("worker", "d")
            .with_workspace_path(dir.to_string_lossy().to_string());
        assert_eq!(resolve_declared_sys_prompt_base(&decl), "# worker rules");
        // 无 AGENTS.md → 空
        let empty_dir = temp_dir("sysprompt-empty");
        let decl = decl.with_workspace_path(empty_dir.to_string_lossy().to_string());
        assert_eq!(resolve_declared_sys_prompt_base(&decl), "");
        // inline body
        let decl = SubagentDeclaration::new("worker", "d").with_inline_agents_body("inline body");
        assert_eq!(resolve_declared_sys_prompt_base(&decl), "inline body");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&empty_dir).ok();
    }

    #[test]
    fn allowlisted_inherited_toolkit_filters_tools() {
        let mut parent = Toolkit::new();
        parent.register(Arc::new(EchoTool { name: "echo" }));
        parent.register(Arc::new(EchoTool { name: "extra" }));
        // 空 allowlist → 全量继承
        let full = allowlisted_inherited_toolkit(Some(&parent), &[]);
        assert_eq!(full.tool_names().len(), 2);
        // 白名单 → 只留 echo
        let filtered = allowlisted_inherited_toolkit(
            Some(&parent),
            &["echo".to_string()],
        );
        let names = filtered.tool_names();
        assert_eq!(names, vec!["echo".to_string()]);
        // 无父 → 新空 toolkit
        assert!(allowlisted_inherited_toolkit(None, &[]).tool_names().is_empty());
    }

    // ---------- Builder / HarnessAgent ----------

    #[test]
    fn builder_requires_model() {
        let error = match HarnessAgentBuilder::new().build() {
            Err(error) => error,
            Ok(_) => panic!("expected build to fail without a model"),
        };
        assert_eq!(error, "model is required");
    }

    #[test]
    fn build_defaults_and_general_purpose_entry() {
        let workspace = temp_dir("defaults");
        let agent = HarnessAgentBuilder::new()
            .model(MockModel::single("ok"))
            .workspace(&workspace)
            .build()
            .unwrap();
        // name 缺省 ReActAgent
        assert_eq!(agent.agent_id(), "ReActAgent");
        // workspace manager 已装配
        assert!(agent.workspace_manager().is_some());
        // general-purpose 已注册
        let manager = agent.subagent_manager().unwrap();
        assert!(manager.has_agent(GENERAL_PURPOSE_NAME));
        // general-purpose 无声明（declaration = None），不参与 PRIMARY 声明判定
        assert!(!manager.is_primary_only(GENERAL_PURPOSE_NAME));
        // 任务仓库默认 workspace 持久化实现
        assert!(agent.task_repository().is_some());
        // 计划模式未启用
        assert!(agent.plan_mode_manager().is_none());
        assert!(!agent.is_plan_mode_active(None, "s1"));
        agent.close();
        std::fs::remove_dir_all(&workspace).ok();
    }

    #[tokio::test]
    async fn compact_now_forces_compaction_and_persists() {
        let workspace = temp_dir("compact-now");
        let agent = HarnessAgentBuilder::new()
            .model(MockModel::single("## SESSION INTENT\nSummary goal."))
            .workspace(&workspace)
            .compaction(CompactionConfig {
                trigger_messages: 50, // 常规阈值远未达到，compact_now 仍强制压
                flush_before_compact: false,
                ..Default::default()
            })
            .build()
            .unwrap();
        // 空对话：无需压缩，摘要模型不被调用
        assert!(agent.compact_now(None, "s1").await.unwrap().is_none());
        // 预置 6 条对话（delegate 的状态管理 API；HarnessAgent 测试同模块可访问）
        let mut state = agent.delegate.agent_state_for(None, "s1").unwrap();
        state.context = (0..6)
            .map(|i| Msg::user("user", &format!("message {i}")))
            .collect();
        agent
            .delegate
            .save_agent_state_for(None, "s1", &state)
            .unwrap();
        // 强制压缩：返回压缩说明，状态写回为 摘要 + 尾部 1 条
        let detail = agent.compact_now(None, "s1").await.unwrap();
        assert_eq!(detail.as_deref(), Some("5 条消息合并为 1 条摘要"));
        let compacted = agent.delegate.agent_state_for(None, "s1").unwrap();
        assert_eq!(compacted.context.len(), 2);
        assert_eq!(
            compacted.context[0].name,
            crate::memory::COMPACTION_SUMMARY_MSG_NAME
        );
        assert_eq!(compacted.context[1].text(), "message 5");
        agent.close();
        std::fs::remove_dir_all(&workspace).ok();
    }

    #[tokio::test]
    async fn compact_now_errors_when_compaction_disabled() {
        let workspace = temp_dir("compact-off");
        let agent = HarnessAgentBuilder::new()
            .model(MockModel::single("ok"))
            .workspace(&workspace)
            .disable_compaction()
            .build()
            .unwrap();
        let error = agent.compact_now(None, "s1").await.unwrap_err();
        assert!(error.contains("压缩未启用"));
        agent.close();
        std::fs::remove_dir_all(&workspace).ok();
    }

    #[tokio::test]
    async fn declared_subagent_factory_builds_agent() {
        let workspace = temp_dir("declared");
        let dir = workspace.join("subagents");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("worker.md"),
            "---\nname: worker\ndescription: does work\n---\nWorker prompt body.\n",
        )
        .unwrap();
        let agent = HarnessAgentBuilder::new()
            .model(MockModel::single("ok"))
            .workspace(&workspace)
            .build()
            .unwrap();
        let manager = agent.subagent_manager().unwrap();
        // 声明已加载
        let decl = manager.get_declaration("worker").unwrap();
        assert_eq!(decl.description, "does work");
        // 工厂可产出子代理
        let rc = RuntimeContext::new().with_session_id("parent-sess");
        let child = manager.create_agent_if_present(&rc, "worker").unwrap();
        // 子代理系统提示 = 声明 body + 上下文节（经 sys_prompt 注入，间接验证工厂装配）
        let _ = child; // ReActAgent 字段私有，行为验证由 call 集成路径覆盖
        assert!(manager.has_agent("worker"));
        agent.close();
        std::fs::remove_dir_all(&workspace).ok();
    }

    #[tokio::test]
    async fn plan_mode_roundtrip_via_harness_agent() {
        let workspace = temp_dir("plan");
        let agent = HarnessAgentBuilder::new()
            .model(MockModel::single("ok"))
            .workspace(&workspace)
            .enable_plan_mode()
            .plan_file_directory("design")
            .build()
            .unwrap();
        // 未进入 → 非激活
        assert!(!agent.is_plan_mode_active(None, "sess-1"));
        // 进入 → 激活 + 默认路径（design/PLAN.md）
        let path = agent.enter_plan_mode(None, "sess-1").unwrap();
        assert_eq!(path, "design/PLAN.md");
        assert!(agent.is_plan_mode_active(None, "sess-1"));
        assert_eq!(
            agent.plan_file_path(None, "sess-1").as_deref(),
            Some("design/PLAN.md")
        );
        // 状态已持久化（重新查询走状态库/槽位）
        assert!(agent.is_plan_mode_active(None, "sess-1"));
        // 退出 → 非激活，路径保留
        agent.exit_plan_mode(None, "sess-1").unwrap();
        assert!(!agent.is_plan_mode_active(None, "sess-1"));
        assert_eq!(
            agent.plan_file_path(None, "sess-1").as_deref(),
            Some("design/PLAN.md")
        );
        agent.close();
        std::fs::remove_dir_all(&workspace).ok();
    }

    #[tokio::test]
    async fn workspace_task_repository_persists_and_sweeps() {
        let workspace_dir = temp_dir("tasks");
        let manager = Arc::new(WorkspaceManager::new(&workspace_dir));
        let repo = WorkspaceTaskRepository::new(manager.clone(), "parent-agent");
        let rc = RuntimeContext::new().with_session_id("sess-1");

        // 提交本地任务
        let task = repo
            .put_task(
                &rc,
                "t1",
                "worker",
                "sess-1",
                TaskRunSpec::Local {
                    execution: Arc::new(|| {
                        Box::pin(async { Ok("done!".to_string()) })
                    }),
                },
            )
            .unwrap();
        // 等执行体结束
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let settled = repo.get_task(&rc, "sess-1", "t1").unwrap();
        assert_eq!(settled.status(), TaskStatus::Completed);
        assert_eq!(
            settled.record.lock().unwrap().result.as_deref(),
            Some("done!")
        );
        // 记录已持久化到 workspace
        let persisted = manager.read_task_record("t1").unwrap();
        assert_eq!(persisted.status, TaskStatus::Completed);
        assert_eq!(persisted.parent_agent_id, "parent-agent");
        // 投递
        assert!(!repo.is_delivered(&rc, "sess-1", "t1"));
        repo.mark_delivered(&rc, "sess-1", "t1");
        assert!(repo.is_delivered(&rc, "sess-1", "t1"));
        assert!(repo.find_pending_deliveries(&rc, "sess-1").is_empty());
        drop(task);

        // 孤儿清扫：workspace 里非终态记录、内存无执行体 → 失败
        let mut orphan = TaskRecord::pending("t2", "worker", "parent-agent", "sess-1", "sess-1");
        orphan.status = TaskStatus::Running;
        manager.write_task_record(&orphan).unwrap();
        let repo2 = WorkspaceTaskRepository::new(manager.clone(), "parent-agent");
        let swept = repo2.get_task(&rc, "sess-1", "t2").unwrap();
        assert_eq!(swept.status(), TaskStatus::Failed);
        assert!(swept.record.lock().unwrap().error_message.is_some());
        repo.shutdown();
        repo2.shutdown();
        std::fs::remove_dir_all(&workspace_dir).ok();
    }

    #[tokio::test]
    async fn call_delegates_with_default_session() {
        let workspace = temp_dir("call");
        let agent = HarnessAgentBuilder::new()
            .name("harness-main")
            .model(MockModel::single("hello from model"))
            .workspace(&workspace)
            .build()
            .unwrap();
        // 不带 session_id → 回落 agent_id
        let reply = agent
            .call(RuntimeContext::new(), Msg::user("user", "hi"))
            .await
            .unwrap();
        assert_eq!(reply.text(), "hello from model");
        // 会话状态已建立（session id = agent id）
        let state = agent
            .delegate()
            .state_for(None, "harness-main")
            .unwrap()
            .expect("state should exist");
        assert_eq!(state.session_id, "harness-main");
        // clear_context：上下文清空、身份保留
        agent.clear_context(None, "harness-main").unwrap();
        let cleared = agent
            .delegate()
            .state_for(None, "harness-main")
            .unwrap()
            .expect("state should survive clear");
        assert!(cleared.context.is_empty());
        assert_eq!(cleared.session_id, "harness-main");
        agent.close();
        std::fs::remove_dir_all(&workspace).ok();
    }

    /// 复现 studio「失忆」：JsonFileAgentStateStore（Legacy 模式）+ 固定 session，
    /// 第二次 call 时模型必须能看到第一轮的问答（studio 同款装配的最小复现）。
    #[tokio::test]
    async fn legacy_jsonfile_context_reaches_model_across_calls() {
        use agentscope_core::model::ChatRequest;
        use std::sync::Mutex as StdMutex;

        struct RecordingModel {
            requests: StdMutex<Vec<ChatRequest>>,
        }
        #[async_trait::async_trait]
        impl Model for RecordingModel {
            async fn call(&self, request: ChatRequest) -> agentscope_core::error::Result<ChatResponse> {
                self.requests.lock().unwrap().push(request);
                Ok(ChatResponse {
                    content: vec![ContentBlock::Text {
                        text: "好的".to_string(),
                    }],
                    usage: None,
                    finish_reason: Some("stop".to_string()),
                })
            }
        }

        let workspace = temp_dir("legacy-ctx");
        let store = Arc::new(
            agentscope_core::state::JsonFileAgentStateStore::new(workspace.join("state")).unwrap(),
        );
        let model = Arc::new(RecordingModel {
            requests: StdMutex::new(Vec::new()),
        });
        let agent = HarnessAgentBuilder::new()
            .name("assistant")
            .sys_prompt("你是助手")
            .model(model.clone() as Arc<dyn Model>)
            .workspace(&workspace)
            .state_store(store)
            .default_session_id("main")
            .build()
            .unwrap();

        let rc = || {
            RuntimeContext::new()
                .with_user_id("studio")
                .with_session_id("main")
        };
        agent
            .call(rc(), Msg::user("user", "第一问：我叫什么？"))
            .await
            .unwrap();
        agent
            .call(rc(), Msg::user("user", "第二问：我刚才问了什么？"))
            .await
            .unwrap();

        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "模型应被调用两次");
        let joined = requests[1]
            .messages
            .iter()
            .map(|m| m.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("第一问"), "第二次请求缺少第一轮用户消息：{joined}");
        assert!(joined.contains("好的"), "第二次请求缺少第一轮助手回复：{joined}");
        assert!(joined.contains("第二问"), "缺少本次输入：{joined}");
        agent.close();
        std::fs::remove_dir_all(&workspace).ok();
    }

    /// 回归：SubagentsMiddleware 的系统提示（SUBAGENT_SECTION_TEMPLATE）承诺了
    /// agent_spawn / agent_send / agent_list，build() 必须把它们注册进 function
    /// list——提示词承诺了工具面没有的工具，模型按提示调用会收到
    /// "tool not registered"，然后在既有工具里乱选（曾导致想派子代理却反复
    /// 误调 browser_navigate）。用真实 agent_spawn 调用走通注册面。
    #[tokio::test]
    async fn build_registers_promised_subagent_tools() {
        let workspace = temp_dir("spawn-registered");
        let model = Arc::new(MockModel {
            // pop 从尾部取：先弹 ToolUse（第一轮），再弹 Text（第二轮收尾）。
            responses: Mutex::new(vec![
                ChatResponse {
                    content: vec![ContentBlock::Text {
                        text: "done".to_string(),
                    }],
                    usage: None,
                    finish_reason: Some("stop".to_string()),
                },
                ChatResponse {
                    content: vec![ContentBlock::ToolUse {
                        id: "call_1".to_string(),
                        name: "agent_spawn".to_string(),
                        input: json!({
                            "agent_id": "general-purpose",
                            "task": "say hi",
                            "timeout_seconds": 0
                        }),
                        state: None,
                        content: None,
                    }],
                    usage: None,
                    finish_reason: Some("tool_calls".to_string()),
                },
            ]),
        });
        let agent = HarnessAgentBuilder::new()
            .name("harness-main")
            .model(model)
            .workspace(&workspace)
            // agent_spawn 非只读：默认权限会 RequireApproval 挂起，测试用
            // Bypass 直接放行，专注验证「工具已注册并被执行」。
            .permission_context(
                agentscope_core::permission::PermissionContextState::default()
                    .with_mode(agentscope_core::permission::PermissionMode::Bypass),
            )
            .build()
            .unwrap();
        let reply = agent
            .call(RuntimeContext::new(), Msg::user("user", "hi"))
            .await
            .unwrap();
        assert_eq!(reply.text(), "done");
        // 工具结果回传：修复前 agent_spawn 未注册，结果为
        // "tool not registered: agent_spawn"；修复后是 spawn 的真实输出。
        let state = agent
            .delegate()
            .state_for(None, "harness-main")
            .unwrap()
            .expect("state should exist");
        let mut spawn_result = String::new();
        for msg in &state.context {
            for block in &msg.content {
                if let ContentBlock::ToolResult { name, output, .. } = block {
                    if name == "agent_spawn" {
                        for ob in output {
                            if let ContentBlock::Text { text } = ob {
                                spawn_result.push_str(text);
                            }
                        }
                    }
                }
            }
        }
        assert!(
            !spawn_result.is_empty(),
            "应存在 agent_spawn 的工具结果（工具已注册并被执行）"
        );
        assert!(
            !spawn_result.contains("tool not registered"),
            "agent_spawn 应已注册进 function list，实际结果: {spawn_result}"
        );
        agent.close();
        std::fs::remove_dir_all(&workspace).ok();
    }
}
