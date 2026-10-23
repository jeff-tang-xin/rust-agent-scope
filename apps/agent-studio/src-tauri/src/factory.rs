//! Agent 装配（≈ Java `HarnessAgentBuilderSupport` + service 的 agent 类型分化）。
//!
//! 装配顺序对齐 Java harness 的语义：
//! 1. workspace（每 agent 独立，目录布局对齐 `WorkspaceConstants`）；
//! 2. state store（JsonFile，会话状态跨重启持久）；
//! 3. team 模式先建成员 agent（先有成员再有 team），再建 leader；
//! 4. 工具面按模式分化（solo/team/ops/db），基础面 = filesystem 6 + memory 3；
//! 5. `HarnessAgent::builder()` 组装并 `build()`。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentscope_core::model::Model;
use agentscope_core::state::{AgentStateStore, JsonFileAgentStateStore};
use agentscope_core::tool::Toolkit;
use agentscope_harness::agent::{HarnessAgent, HarnessAgentBuilder};
use agentscope_harness::subagent::{DeclarationMode, SubagentDeclaration};
use agentscope_harness::task::{InMemoryTaskRepository, TaskRepository};
use agentscope_harness::tools::{ShellExecuteTool, WebFetchTool, WebSearchTool};
use agentscope_harness::tools_filesystem::FilesystemToolSet;
use agentscope_harness::tools_memory::MemoryToolSet;
use agentscope_harness::workspace::{LocalFilesystem, LocalFsMode, PathPolicy, WorkspaceManager};
use agentscope_model_openai::OpenAIChatModel;

use crate::config::{AgentConfig, ModelProviderConfig, TeamMemberConfig};
use crate::sql_tool::SqlQueryTool;
use crate::team::{MemberRegistry, TeamDelegateTool};

/// 工作区模式（类型化，替代字符串魔法值；对齐 service 的 agent 类型分化）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StudioMode {
    Solo,
    Team,
    Ops,
    Db,
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

/// agent 数据目录（workspace / state / members 都挂在它下面）。
pub(crate) fn agent_dir(data_dir: &Path, agent_id: &str) -> PathBuf {
    data_dir.join("agents").join(agent_id)
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
fn register_base_tools(toolkit: &mut Toolkit, workspace: &Arc<WorkspaceManager>) -> Result<(), String> {
    let fs = workspace
        .filesystem()
        .cloned()
        .ok_or_else(|| "workspace has no filesystem layer".to_string())?;
    for tool in FilesystemToolSet::new(fs, None).all() {
        toolkit.register(tool);
    }
    let memory = MemoryToolSet::new(Arc::clone(workspace));
    toolkit.register(Arc::new(memory.memory_save));
    toolkit.register(Arc::new(memory.memory_get));
    toolkit.register(Arc::new(memory.memory_search));
    Ok(())
}

/// 单个成员 agent（team 模式；独立 workspace + 独立 state，Isolated 语义）。
/// 成员工作区根由 build_agent 决定（leader 自定义目录时在其下隔离，见 build_agent）。
fn build_member(
    leader: &AgentConfig,
    provider: &ModelProviderConfig,
    data_dir: &Path,
    member: &TeamMemberConfig,
    members_workspace_root: &Path,
) -> Result<HarnessAgent, String> {
    let member_id = format!("{}-{}", leader.id, member.name);
    let dir = agent_dir(data_dir, &member_id);
    let workspace = build_workspace(&members_workspace_root.join(&member.name))?;
    let state_store = build_state_store(&dir.join("state"))?;
    let mut toolkit = Toolkit::new();
    register_base_tools(&mut toolkit, &workspace)?;
    HarnessAgentBuilder::new()
        .name(&member.name)
        .sys_prompt(agentscope_harness::agent::build_subagent_sys_prompt(&member.prompt))
        .model(build_model(provider)?)
        .toolkit(toolkit)
        .workspace_manager(workspace)
        .state_store(state_store)
        .default_session_id("main")
        .as_leaf_subagent()
        .build()
}

/// 构建 JsonFile 状态库（会话状态跨重启持久）。
fn build_state_store(root: &Path) -> Result<Arc<dyn AgentStateStore>, String> {
    let store = JsonFileAgentStateStore::new(root)
        .map_err(|e| format!("create state store: {e}"))?;
    Ok(Arc::new(store))
}

/// 装配一个 agent（含 team 成员）。
/// `runtime_mode` 是运行方式（solo/team/ops/db），与 agent 配置解耦——
/// 协同类 agent 配置固定存 "solo"，进入会话时由用户选择以 solo 或 team
/// 运行（team 以该 agent 为 lead，成员取配置里的 members）。
pub fn build_agent(
    cfg: &AgentConfig,
    provider: &ModelProviderConfig,
    data_dir: &Path,
    runtime_mode: &str,
) -> Result<BuiltAgent, String> {
    let mode = StudioMode::parse(runtime_mode)
        .ok_or_else(|| format!("unknown mode `{}` (expected solo|team|ops|db)", runtime_mode))?;
    let dir = agent_dir(data_dir, &cfg.id);
    // 工作区：配置指定绝对路径则用之（agent 直接在真实项目里干活），
    // 否则用默认目录。Sandboxed 模式把所有文件工具锚定在该根上。
    let workspace_root = if cfg.workspace.trim().is_empty() {
        dir.join("workspace")
    } else {
        std::path::PathBuf::from(cfg.workspace.trim())
    };
    let workspace = build_workspace(&workspace_root)?;
    let state_store = build_state_store(&dir.join("state"))?;

    // team：先建成员（对齐 Java「先有成员再有 team」的装配顺序）。
    // 成员工作区根：leader 用自定义目录时在其下 `.studio-members/` 隔离
    // （成员在项目里干活但不与 leader 抢同一目录）；默认时保持各自
    // agent_dir/workspace（原行为）。
    let members_ws_root = if cfg.workspace.trim().is_empty() {
        None
    } else {
        Some(workspace_root.join(".studio-members"))
    };
    let mut members: HashMap<String, Arc<HarnessAgent>> = HashMap::new();
    if mode == StudioMode::Team {
        for member in &cfg.members {
            let agent = match &members_ws_root {
                Some(root) => build_member(cfg, provider, data_dir, member, root)?,
                None => {
                    let fallback = agent_dir(data_dir, &format!("{}-{}", cfg.id, member.name))
                        .join("workspace");
                    build_member(cfg, provider, data_dir, member, &fallback)?
                }
            };
            members.insert(member.name.clone(), Arc::new(agent));
        }
    }
    let members: MemberRegistry = Arc::new(members);

    let mut toolkit = Toolkit::new();
    register_base_tools(&mut toolkit, &workspace)?;
    match mode {
        StudioMode::Ops => {
            toolkit.register(Arc::new(ShellExecuteTool::new(Arc::new(
                agentscope_harness::tools::LocalShellBackend::new(),
            ))));
            toolkit.register(Arc::new(WebFetchTool::new()));
            toolkit.register(Arc::new(WebSearchTool::new()));
        }
        StudioMode::Db => {
            toolkit.register(Arc::new(SqlQueryTool::new(
                workspace.root().join(crate::sql_tool::DEFAULT_DB_FILE),
            )));
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
        StudioMode::Solo => {}
    }

    let mut builder = HarnessAgentBuilder::new()
        .name(&cfg.name)
        .sys_prompt(sys_prompt_for(cfg, mode))
        .model(build_model(provider)?)
        .toolkit(toolkit)
        .workspace_manager(Arc::clone(&workspace))
        .state_store(state_store)
        .default_session_id("main");
    if cfg.max_iters > 0 {
        builder = builder.max_iters(cfg.max_iters as usize);
    }
    if mode == StudioMode::Team {
        // 成员声明注册：SubagentsMiddleware 会把成员指引并入 leader 系统提示
        // （对齐 TeamContext 注入成员快照的语义）。
        for member in &cfg.members {
            // SubagentDeclaration 未实现 Default：显式给出全部字段（默认值
            // 对齐 serde defaults：steps=10、inherit_parent_permissions=true）。
            builder = builder.subagent(SubagentDeclaration {
                name: member.name.clone(),
                description: member.description.clone(),
                workspace_mode: Default::default(),
                workspace_path: None,
                inline_agents_body: Some(member.prompt.clone()),
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

/// 系统提示：用户定义 + 模式职责说明（对齐 service 的 agent instructions 面）。
fn sys_prompt_for(cfg: &AgentConfig, mode: StudioMode) -> String {
    let mut prompt = String::new();
    if !cfg.description.is_empty() {
        prompt.push_str(&cfg.description);
        prompt.push_str("\n\n");
    }
    prompt.push_str(cfg.sys_prompt.trim());
    let role_note = match mode {
        StudioMode::Solo => {
            "You are working in solo mode: one assistant, one user, full file and memory \
             tools in your workspace. When asked to plan first, write the plan into the \
             plan file before executing."
        }
        StudioMode::Team => {
            "You are the team lead. Delegate self-contained tasks to your members with \
             the team_delegate tool, review their results, and synthesize the final \
             answer for the user. Do the work yourself only when delegation is not \
             warranted."
        }
        StudioMode::Ops => {
            "You are an operations engineer. You can run shell commands and fetch web \
             pages. Be precise and conservative: prefer read-only inspection before \
             mutating actions, and explain the impact of destructive commands before \
             running them."
        }
        StudioMode::Db => {
            "You are a database analyst. You can run read-only SQL (SELECT/WITH/EXPLAIN/\
             PRAGMA) against the workspace SQLite database via sql_query. Inspect the \
             schema first, then answer with evidence from actual query results."
        }
    };
    prompt.push_str("\n\n");
    prompt.push_str(role_note);
    prompt
}
