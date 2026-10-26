//! Agent 装配（≈ Java `HarnessAgentBuilderSupport` + service 的 agent 类型分化）。
//!
//! 装配顺序对齐 Java harness 的语义：
//! 1. workspace（每 agent 独立，目录布局对齐 `WorkspaceConstants`）；
//! 2. state store（JsonFile，会话状态跨重启持久）；
//! 3. team 运行方式先建成员 agent（先有成员再有 team），再建 lead；
//! 4. 工具面按模式分化（solo/team/ops/db），基础面 = filesystem 6 + memory 3；
//! 5. `HarnessAgent::builder()` 组装并 `build()`。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentscope_core::model::Model;
use agentscope_core::permission::{PermissionBehavior, PermissionContextState, PermissionRule};
use agentscope_core::state::{AgentStateStore, JsonFileAgentStateStore};
use agentscope_core::tool::Toolkit;
use crate::db_exec::{DbCatalog, DbDmlTool, DbQueryTool};
use crate::ssh_tool::{RemoteShellTool, SshPool};
use agentscope_core::agent::ReActAgent;
use agentscope_harness::agent::{
    build_subagent_sys_prompt, HarnessAgent, HarnessAgentBuilder, GENERAL_PURPOSE_DESCRIPTION,
    GENERAL_PURPOSE_NAME,
};
use agentscope_harness::subagent::{
    DeclarationMode, DefaultAgentManager, SubagentDeclaration, SubagentEntry, SubagentFactory,
};
use agentscope_harness::tools_agent_spawn::{
    AgentListTool, AgentSendTool, AgentSpawnCore, AgentSpawnTool,
};
use agentscope_harness::task::{InMemoryTaskRepository, TaskRepository};
use agentscope_harness::tools::{WebFetchTool, WebSearchTool};
use agentscope_harness::tools_filesystem::FilesystemToolSet;
use agentscope_harness::tools_memory::MemoryToolSet;
use agentscope_harness::workspace::{LocalFilesystem, LocalFsMode, PathPolicy, WorkspaceManager};
use agentscope_model_openai::OpenAIChatModel;

use crate::config::{AgentConfig, ModelProviderConfig, WorkspaceConfig};
use crate::team::{MemberRegistry, TeamDelegateTool};

/// 工作区模式（类型化，替代字符串魔法值；对齐 service 的 agent 类型分化）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StudioMode {
    Solo,
    Team,
    Ops,
    Db,
}

/// 工具面共享的资源句柄（AppState 持有，装配时传给工具）。
#[derive(Clone)]
pub struct ResourceHandles {
    pub ssh_pool: Arc<SshPool>,
    pub db_catalog: Arc<DbCatalog>,
}

impl StudioMode {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "solo" => Some(Self::Solo),
            "team" => Some(Self::Team),
            "ops" => Some(Self::Ops),
            "db" => Some(Self::Db),
            _ => None,
        }
    }
}

/// 装配产物：leader agent（team 成员已在装配期注入 TeamDelegateTool 与
/// subagent 声明，无需外露）。
pub struct BuiltAgent {
    pub agent: HarnessAgent,
}

/// 工作区数据目录（workspace / state / members 都挂在它下面；会话状态
/// 按工作区隔离——同一智能体在不同工作区各有独立会话）。
pub(crate) fn workspace_dir(data_dir: &Path, workspace_id: &str) -> PathBuf {
    data_dir.join("workspaces").join(workspace_id)
}

/// 工作区文件根（agent 文件工具的锚定目录；文件列表命令用同一解析，
/// 保证面板看到的与 agent 干活的目录一致）。
pub fn resolve_workspace_root(data_dir: &Path, ws: &WorkspaceConfig) -> PathBuf {
    if ws.dir.trim().is_empty() {
        workspace_dir(data_dir, &ws.id).join("workspace")
    } else {
        std::path::PathBuf::from(ws.dir.trim())
    }
}

/// 构建 OpenAI 兼容模型。
fn build_model(provider: &ModelProviderConfig) -> Result<Arc<dyn Model>, String> {
    let mut model = OpenAIChatModel::new(&provider.api_key, &provider.model);
    if !provider.base_url.is_empty() {
        model = model.with_base_url(&provider.base_url);
    }
    Ok(Arc::new(model))
}

/// 构建工作区：本地磁盘 + Sandboxed 模式（所有路径锚定 workspace 根，
/// 拒绝越界——桌面应用的文件安全边界）。
fn build_workspace(root: &Path) -> Result<Arc<WorkspaceManager>, String> {
    std::fs::create_dir_all(root).map_err(|e| format!("create workspace dir: {e}"))?;
    let fs = Arc::new(LocalFilesystem::new(
        root,
        LocalFsMode::Sandboxed,
        PathPolicy::empty(),
    ));
    Ok(Arc::new(WorkspaceManager::with_filesystem(root, fs)))
}

/// 基础工具面：filesystem 6 + memory 3（所有模式共有）。
fn register_base_tools(
    toolkit: &mut Toolkit,
    workspace: &Arc<WorkspaceManager>,
    memory_workspace: Arc<WorkspaceManager>,
) -> Result<(), String> {
    let fs = workspace
        .filesystem()
        .cloned()
        .ok_or_else(|| "workspace has no filesystem layer".to_string())?;
    for tool in FilesystemToolSet::new(fs, None).all() {
        toolkit.register(tool);
    }
    // 记忆工具锚定独立的 .ox 记忆工作区（<项目>/.ox/）：MEMORY.md 与
    // memory/ 台账不落项目根，非业务产出与业务代码分离（.ox 数据规范）。
    let memory = MemoryToolSet::new(memory_workspace);
    toolkit.register(Arc::new(memory.memory_save));
    toolkit.register(Arc::new(memory.memory_get));
    toolkit.register(Arc::new(memory.memory_search));
    Ok(())
}

/// 记忆工作区根：<工作区根>/.ox（Sandboxed 锚定，记忆读写不出 .ox）。
fn build_memory_workspace(root: &Path) -> Result<Arc<WorkspaceManager>, String> {
    build_workspace(&root.join(".ox"))
}

/// 单个成员 agent（team 模式；独立 workspace + 独立 state，Isolated 语义）。
/// 成员是**引用的已有智能体**：用成员自己的人设（sys_prompt）与 provider 装配
/// ——协同 = 多个智能体，各自带定义入队。成员工作区根由 build_agent 决定。
fn build_member(
    workspace_id: &str,
    member: &AgentConfig,
    provider: &ModelProviderConfig,
    data_dir: &Path,
    members_workspace_root: &Path,
) -> Result<HarnessAgent, String> {
    let member_id = format!("{}-{}", workspace_id, member.id);
    let dir = workspace_dir(data_dir, &member_id);
    let workspace = build_workspace(&members_workspace_root.join(&member.name))?;
    let memory_workspace = build_memory_workspace(workspace.root())?;
    let state_store = build_state_store(&dir.join("state"))?;
    let mut toolkit = Toolkit::new();
    register_base_tools(&mut toolkit, &workspace, memory_workspace)?;
    HarnessAgentBuilder::new()
        .name(&member.name)
        .sys_prompt(agentscope_harness::agent::build_subagent_sys_prompt(
            &member.sys_prompt,
        ))
        .model(build_model(provider)?)
        .toolkit(toolkit)
        .workspace_manager(workspace)
        .state_store(state_store)
        .default_session_id("main")
        .as_leaf_subagent()
        .build()
}

/// 构建一个 spawn 子代理实例（ReActAgent）：lead 的分身——同文件/记忆工具面、
/// 同模型；会话状态复用工作区 state store（spawn 派生的 child session id 不冲突）。
fn build_spawn_agent(
    name: &str,
    sys_prompt_body: &str,
    provider: &ModelProviderConfig,
    workspace_root: &Path,
    state_store: Arc<dyn AgentStateStore>,
) -> Result<Arc<ReActAgent>, String> {
    let workspace = build_workspace(workspace_root)?;
    let memory_workspace = build_memory_workspace(workspace.root())?;
    let mut toolkit = Toolkit::new();
    register_base_tools(&mut toolkit, &workspace, memory_workspace)?;
    let agent = ReActAgent::builder()
        .name(name)
        .sys_prompt(build_subagent_sys_prompt(sys_prompt_body))
        .model(build_model(provider)?)
        .toolkit(toolkit)
        .state_store(state_store)
        .default_session_id("subagent")
        .build()
        .map_err(|e| format!("build subagent `{name}`: {e}"))?;
    Ok(Arc::new(agent))
}

/// solo 的 spawn 子代理注册表：一个条目 = lead 的分身（general-purpose，
/// 继承 lead 的 sys_prompt 与模型）。team 的成员协作走 TeamDelegateTool，
/// 不进 spawn 池（AgentScope 语义：solo 派分身，team 派成员）。
fn build_subagent_manager(
    provider: &ModelProviderConfig,
    workspace_root: &Path,
    lead_sys_prompt: &str,
    state_store: Arc<dyn AgentStateStore>,
) -> Result<Arc<DefaultAgentManager>, String> {
    let manager = Arc::new(DefaultAgentManager::new());
    let root = workspace_root.to_path_buf();
    let store = Arc::clone(&state_store);
    let body = lead_sys_prompt.to_string();
    let provider = provider.clone();
    let factory: Arc<SubagentFactory> = Arc::new(move |_rc| {
        build_spawn_agent(GENERAL_PURPOSE_NAME, &body, &provider, &root, Arc::clone(&store))
    });
    let decl: SubagentDeclaration = serde_json::from_value(serde_json::json!({
        "name": GENERAL_PURPOSE_NAME,
        "description": GENERAL_PURPOSE_DESCRIPTION,
    }))
    .map_err(|e| format!("general-purpose declaration: {e}"))?;
    manager
        .register(SubagentEntry::new(GENERAL_PURPOSE_NAME, Some(decl), factory))
        .map_err(|e| format!("register general-purpose: {e}"))?;
    Ok(manager)
}

/// 构建 JsonFile 状态库（会话状态跨重启持久）。
fn build_state_store(root: &Path) -> Result<Arc<dyn AgentStateStore>, String> {
    let store = JsonFileAgentStateStore::new(root)
        .map_err(|e| format!("create state store: {e}"))?;
    Ok(Arc::new(store))
}

/// 装配一个工作区的执行 agent（含 team 成员）。
///
/// `runtime_mode` 是运行方式（solo/team），仅协同工作区可切；运维/数据库
/// 运维工作区的工具面由工作区类型决定（kind=ops/db），运行方式参数被忽略。
/// team 成员取 `member_defs`——工作区 members 引用的已有智能体。
pub fn build_agent(
    ws: &WorkspaceConfig,
    agent: &AgentConfig,
    provider: &ModelProviderConfig,
    data_dir: &Path,
    runtime_mode: &str,
    member_defs: &[(AgentConfig, ModelProviderConfig)],
    resources: &ResourceHandles,
) -> Result<BuiltAgent, String> {
    // 运行方式解析：协同 → solo/team（用户可切）；ops/db → 由类型决定。
    let mode = match ws.kind.as_str() {
        "ops" => StudioMode::Ops,
        "db" => StudioMode::Db,
        _ => StudioMode::parse(runtime_mode)
            .filter(|m| matches!(m, StudioMode::Solo | StudioMode::Team))
            .unwrap_or(StudioMode::Solo),
    };
    let dir = workspace_dir(data_dir, &ws.id);
    // 工作目录：配置指定绝对路径则用之（agent 直接在真实项目里干活），
    // 否则用默认目录。Sandboxed 模式把所有文件工具锚定在该根上。
    let workspace_root = resolve_workspace_root(data_dir, ws);
    let workspace = build_workspace(&workspace_root)?;
    let memory_workspace = build_memory_workspace(&workspace_root)?;
    let state_store = build_state_store(&dir.join("state"))?;

    // team：先建成员（对齐 Java「先有成员再有 team」的装配顺序）。
    // 成员工作区根：leader 用自定义目录时在其下 `.studio-members/` 隔离
    // （成员在项目里干活但不与 leader 抢同一目录）；默认时保持各自
    // agent_dir/workspace（原行为）。
    let members_ws_root = if ws.dir.trim().is_empty() {
        None
    } else {
        // 成员工作目录也归 .ox（非业务内容不散落项目根）。
        Some(workspace_root.join(".ox").join("members"))
    };
    let mut members: HashMap<String, Arc<HarnessAgent>> = HashMap::new();
    if mode == StudioMode::Team {
        for (member, member_provider) in member_defs {
            let agent = match &members_ws_root {
                Some(root) => build_member(&ws.id, member, member_provider, data_dir, root)?,
                None => {
                    let fallback = workspace_dir(data_dir, &ws.id)
                        .join("members")
                        .join(&member.id)
                        .join("workspace");
                    build_member(&ws.id, member, member_provider, data_dir, &fallback)?
                }
            };
            members.insert(member.name.clone(), Arc::new(agent));
        }
    }
    let members: MemberRegistry = Arc::new(members);

    let mut toolkit = Toolkit::new();
    register_base_tools(&mut toolkit, &workspace, memory_workspace)?;
    match mode {
        StudioMode::Ops => {
            // 远程运维工具面：web 检索（remote_shell 在 match 后统一注册）。
            toolkit.register(Arc::new(WebFetchTool::new()));
            toolkit.register(Arc::new(WebSearchTool::new()));
        }
        StudioMode::Db => {
            // 数据库工具面：无类别特有工具（db_query/db_dml 在 match 后统一注册）。
        }
        StudioMode::Team => {
            // 任务工具（对齐 Java subagent/task 体系的工具面）。
            let repo: Arc<dyn TaskRepository> = Arc::new(InMemoryTaskRepository::new());
            toolkit.register(Arc::new(
                agentscope_harness::tools::TaskOutputTool::new(Arc::clone(&repo)),
            ));
            toolkit.register(Arc::new(
                agentscope_harness::tools::TaskListTool::new(Arc::clone(&repo)),
            ));
            toolkit.register(Arc::new(
                agentscope_harness::tools::TaskCancelTool::new(Arc::clone(&repo)),
            ));
            toolkit.register(Arc::new(
                agentscope_harness::tools::WaitAsyncResultsTool::new(repo, None),
            ));
            // 进程内委派（≈ LocalTeamClient）。
            toolkit.register(Arc::new(TeamDelegateTool::new(Arc::clone(&members))));
        }
        StudioMode::Solo => {
            // spawn 分身体系（AgentScope 语义：solo 派自己的分身，独立会话跑
            // 子任务；team 的成员协作走 TeamDelegateTool，不在此处）。
            let manager = build_subagent_manager(
                provider,
                &workspace_root,
                &agent.sys_prompt,
                Arc::clone(&state_store),
            )?;
            let repo: Arc<dyn TaskRepository> = Arc::new(InMemoryTaskRepository::new());
            let core = AgentSpawnCore::new(manager, Arc::clone(&repo), 0);
            toolkit.register(Arc::new(AgentSpawnTool::new(Arc::clone(&core))));
            toolkit.register(Arc::new(AgentSendTool::new(Arc::clone(&core))));
            toolkit.register(Arc::new(AgentListTool::new(core)));
            toolkit.register(Arc::new(
                agentscope_harness::tools::TaskOutputTool::new(Arc::clone(&repo)),
            ));
            toolkit.register(Arc::new(
                agentscope_harness::tools::TaskListTool::new(Arc::clone(&repo)),
            ));
            toolkit.register(Arc::new(
                agentscope_harness::tools::TaskCancelTool::new(Arc::clone(&repo)),
            ));
            toolkit.register(Arc::new(
                agentscope_harness::tools::WaitAsyncResultsTool::new(repo, None),
            ));
        }
    }
    // 远程能力统一注册（ops/db 的类别默认 + 各类别的绑定开放，一套语义）：
    // - ops 类别默认开放 remote_shell；db 类别默认开放 db_query/db_dml；
    // - 其余类别（collab）绑定非空才开放对应工具；
    // - 绑定列表非空时一律收窄为白名单（工具内校验，越界即拒绝）。
    if mode == StudioMode::Ops || !ws.bound_servers.is_empty() {
        toolkit.register(Arc::new(RemoteShellTool::new(
            Arc::clone(&resources.ssh_pool),
            allowlist(&ws.bound_servers),
        )));
    }
    if mode == StudioMode::Db || !ws.bound_databases.is_empty() {
        let allowed = allowlist(&ws.bound_databases);
        toolkit.register(Arc::new(DbQueryTool::new(
            Arc::clone(&resources.db_catalog),
            allowed.clone(),
        )));
        toolkit.register(Arc::new(DbDmlTool::new(
            Arc::clone(&resources.db_catalog),
            allowed,
        )));
    }

    let mut builder = HarnessAgentBuilder::new()
        .name(&agent.name)
        .sys_prompt(sys_prompt_for(agent, mode))
        .model(build_model(provider)?)
        .toolkit(toolkit)
        .workspace_manager(Arc::clone(&workspace))
        .state_store(state_store)
        .default_session_id("main");
    if agent.max_iters > 0 {
        builder = builder.max_iters(agent.max_iters as usize);
    }
    // 危险工具默认走用户授权（AgentScope 权限引擎 ask 规则，会话级持久化）：
    // shell 执行与文件写入/编辑需用户批准后才会执行；目录穿越由 Sandboxed
    // 文件层硬拒（path escapes workspace root），不在此列。
    builder = builder.permission_context(default_permission_context(mode, ws));

    if mode == StudioMode::Solo {
        // plan/free 切换仅 solo 运行方式提供（产品语义）。必须显式启用 plan
        // 能力，否则 enter_plan_mode 报 "plan mode is not enabled on this agent"。
        builder = builder.enable_plan_mode();
    }
    if mode == StudioMode::Team {
        // 成员声明注册：SubagentsMiddleware 会把成员指引并入 leader 系统提示
        // （对齐 TeamContext 注入成员快照的语义）。成员是引用的已有智能体：
        // name/description/sys_prompt 都取成员自己的定义。
        for (member, _) in member_defs {
            builder = builder.subagent(SubagentDeclaration {
                name: member.name.clone(),
                description: member.description.clone(),
                workspace_mode: Default::default(),
                workspace_path: None,
                inline_agents_body: Some(member.sys_prompt.clone()),
                model: None,
                temperature: None,
                top_p: None,
                variant: None,
                steps: 10,
                mode: DeclarationMode::Subagent,
                hidden: false,
                persist_session: true,
                inherit_parent_permissions: true,
                expose_to_user: None,
                enable_pending_tool_recovery: None,
                tools: Vec::new(),
                skills: Vec::new(),
                url: None,
                headers: None,
            });
        }
        // 任务仓储挂到 builder（task 工具与 harness 内部共用同一仓储）。
        builder = builder.task_repository(Arc::new(InMemoryTaskRepository::new()) as Arc<dyn TaskRepository>);
    }
    let agent = builder.build()?;
    Ok(BuiltAgent { agent })
}

/// 新会话的默认权限上下文：危险工具（shell 执行、文件写入/编辑）→ ask 行为，
/// 由 AgentScope 权限引擎在执行前挂起等待用户确认（对齐 Java ask 规则语义）。
/// 用户批准时可附带 allow 规则（「始终允许」），写进会话权限上下文后生效。
fn default_permission_context(mode: StudioMode, ws: &WorkspaceConfig) -> PermissionContextState {
    // ask 工具集按工作区类别区分：
    // - collab（solo/team）：shell 执行与文件写入/编辑；按绑定开放远程能力时
    //   追加对应 ask 规则（remote_shell / db_dml 每次确认，语义对齐 ops/db）；
    // - ops：remote_shell（远程命令每次确认）；
    // - db：db_dml（数据变更每次确认；db_query 只读免确认）。
    let mut ask_tools: Vec<&'static str> = match mode {
        StudioMode::Ops => vec!["remote_shell"],
        StudioMode::Db => vec!["db_dml"],
        StudioMode::Solo | StudioMode::Team => {
            let mut tools = vec!["execute", "write_file", "edit_file"];
            if !ws.bound_servers.is_empty() {
                tools.push("remote_shell");
            }
            if !ws.bound_databases.is_empty() {
                tools.push("db_dml");
            }
            tools
        }
    };
    ask_tools.dedup();
    let mut context = PermissionContextState::default();
    for tool in ask_tools {
        context.add_ask_rule(PermissionRule {
            tool_name: tool.to_string(),
            rule_content: None,
            behavior: PermissionBehavior::Ask,
            source: "project".to_string(),
        });
    }
    context
}

/// 绑定列表 → 工具白名单：空 = 不限（`None`），非空 = 白名单（`Some`）。
fn allowlist(bound: &[String]) -> Option<Vec<String>> {
    if bound.is_empty() {
        None
    } else {
        Some(bound.to_vec())
    }
}

/// 系统提示：用户定义 + 模式职责说明（对齐 service 的 agent instructions 面）。
fn sys_prompt_for(agent: &AgentConfig, mode: StudioMode) -> String {
    let mut prompt = String::new();
    if !agent.description.is_empty() {
        prompt.push_str(&agent.description);
        prompt.push_str("\n\n");
    }
    prompt.push_str(agent.sys_prompt.trim());
    let role_note: String = match mode {
        StudioMode::Solo => {
            "You are working in solo mode: one assistant, one user, full file and memory \
             tools in your workspace. When asked to plan first, write the plan into the \
             plan file before executing.\n\nYou can spawn subagents that are clones of \
             yourself (same instructions, model and tools) with the agent_spawn tool; \
             they run in isolated sessions, so delegate self-contained subtasks with \
             all the context they need, then verify and integrate their results. Use \
             agent_send for follow-up messages and agent_list to check active ones."
                .to_string()
        }
        StudioMode::Team => {
            "You are the team lead. Delegate self-contained tasks to your members with \
             the team_delegate tool, review their results, and synthesize the final \
             answer for the user. Do the work yourself only when delegation is not \
             warranted."
                .to_string()
        }
        StudioMode::Ops => {
            "You are an operations engineer. You can run shell commands and fetch web \
             pages. Be precise and conservative: prefer read-only inspection before \
             mutating actions, and explain the impact of destructive commands before \
             running them."
                .to_string()
        }
        StudioMode::Db => {
            "You are a database analyst. You can run read-only SQL (SELECT/WITH/EXPLAIN/\
             PRAGMA) against the workspace SQLite database via sql_query. Inspect the \
             schema first, then answer with evidence from actual query results."
                .to_string()
        }
    };
    prompt.push_str("\n\n");
    prompt.push_str(&role_note);
    // .ox 数据规范：非业务产出与业务代码分离（记忆工具已自动锚定 .ox/）。
    prompt.push_str(
        "\n\nWorkspace data convention: keep the project root clean. All          non-business artifacts you produce (plans, task lists, notes,          summaries, reports, scratch files) must live under the `.ox/`          directory at the workspace root; memory tools are already rooted          there. Only business code and documents requested by the user go          elsewhere. Never create memory/ or MEMORY.md at the project root.",
    );
    prompt
}
