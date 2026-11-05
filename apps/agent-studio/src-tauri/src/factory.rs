//! Agent 装配（≈ Java `HarnessAgentBuilderSupport` + service 的 agent 类型分化）。
//!
//! 装配顺序对齐 Java harness 的语义：
//! 1. workspace（每 agent 独立，目录布局对齐 `WorkspaceConstants`）；
//! 2. state store（JsonFile，会话状态跨重启持久）；
//! 3. team 运行方式先建成员 agent（先有成员再有 team），再建 lead；
//! 4. 工具面按模式分化（solo/team/ops/db），基础面 = filesystem 6 + memory 3 + shell 1；
//! 5. `HarnessAgent::builder()` 组装并 `build()`。

use crate::commands::USER_ID;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentscope_core::model::Model;
use agentscope_core::permission::{PermissionBehavior, PermissionContextState, PermissionRule};
use agentscope_core::state::{AgentStateStore, JsonFileAgentStateStore};
use agentscope_core::tool::Toolkit;
use crate::config;
use crate::db_exec::{DbCatalog, DbDmlTool, DbQueryTool};
use crate::ssh_tool::{RemoteShellTool, SshPool};
use agentscope_core::agent::ReActAgent;
use agentscope_harness::agent::{
    allowlisted_inherited_toolkit, build_subagent_sys_prompt, HarnessAgent, HarnessAgentBuilder,
    GENERAL_PURPOSE_DESCRIPTION, GENERAL_PURPOSE_NAME,
};
use agentscope_harness::subagent::{
    DeclarationMode, DefaultAgentManager, SubagentDeclaration, SubagentEntry, SubagentFactory,
};
use agentscope_harness::tools_agent_spawn::{
    AgentListTool, AgentSendTool, AgentSpawnCore, AgentSpawnTool,
};
use agentscope_harness::task::{InMemoryTaskRepository, TaskRepository, WorkspaceTaskRepository};
use agentscope_harness::tools::{
    LocalShellBackend, ShellExecuteTool, WebFetchTool, WebSearchTool,
};
use agentscope_harness::tools_filesystem::FilesystemToolSet;
use agentscope_harness::tools_memory::MemoryToolSet;
use agentscope_harness::workspace::{LocalFilesystem, LocalFsMode, PathPolicy, WorkspaceManager};
use agentscope_model_openai::OpenAIChatModel;

use crate::config::{AgentConfig, ModelProviderConfig, StudioConfig, WorkspaceConfig};
use crate::team::{MemberRegistry, TeamDelegateTool};

/// 工作区模式（类型化，替代字符串魔法值；对齐 service 的 agent 类型分化）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StudioMode {
    Solo,
    Team,
    Ops,
    Db,
    Test,
}

/// 工具面共享的资源句柄（AppState 持有，装配时传给工具）。
#[derive(Clone)]
pub struct ResourceHandles {
    pub ssh_pool: Arc<SshPool>,
    pub db_catalog: Arc<DbCatalog>,
    /// 内存配置副本（remote_shell 做 host 一致性校验用）。
    pub config: Arc<std::sync::Mutex<crate::config::StudioConfig>>,
    /// 全局共享浏览器会话（内置浏览器自动化工具；懒启动 headful）。
    pub browser: Arc<crate::browser_tool::BrowserSession>,
}

impl StudioMode {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "solo" => Some(Self::Solo),
            "team" => Some(Self::Team),
            "ops" => Some(Self::Ops),
            "db" => Some(Self::Db),
            "test" => Some(Self::Test),
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

/// 用户主目录（Windows USERPROFILE / unix HOME）。
fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("USERPROFILE")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(std::path::PathBuf::from))
}

/// 展开 `~` 前缀为用户主目录（如 `~/.ox/db/my`）；其余原样。
/// 不展开时 `~` 会被 Sandboxed 工作区当成字面目录名在根下创建。
fn expand_home(dir: &str) -> std::path::PathBuf {
    let t = dir.trim();
    if let Some(rest) = t.strip_prefix("~/").or_else(|| t.strip_prefix("~\\")) {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    if t == "~" {
        if let Some(home) = home_dir() {
            return home;
        }
    }
    std::path::PathBuf::from(t)
}

/// 工作区文件根（agent 文件工具的锚定目录；文件列表命令用同一解析，
/// 保证面板看到的与 agent 干活的目录一致）。支持 `~` 前缀。
pub fn resolve_workspace_root(data_dir: &Path, ws: &WorkspaceConfig) -> PathBuf {
    if ws.dir.trim().is_empty() {
        workspace_dir(data_dir, &ws.id).join("workspace")
    } else {
        expand_home(&ws.dir)
    }
}

/// 工作区文件根的**生效解析**：测试工作区绑定了协同工作区时，跟随目标的
/// 工作目录（协同改目录测试自动跟随）——两边 `.ox/` 数据由此落在同一处。
/// 其余情况与 [`resolve_workspace_root`] 一致。绑定目标缺失（被删后未解绑
/// 的脏数据）时回落自己的目录，不报错——面板与装配不能因脏绑定而崩。
pub fn effective_workspace_root(
    data_dir: &Path,
    config: &StudioConfig,
    ws: &WorkspaceConfig,
) -> PathBuf {
    if ws.kind == "test" {
        if let Some(bid) = ws.bound_workspace_id.as_deref() {
            if !bid.trim().is_empty() {
                if let Some(target) = config.workspace(bid) {
                    return resolve_workspace_root(data_dir, target);
                }
            }
        }
    }
    resolve_workspace_root(data_dir, ws)
}

/// 构建 OpenAI 兼容模型。
fn build_model(provider: &ModelProviderConfig) -> Result<Arc<dyn Model>, String> {
    let mut model = OpenAIChatModel::new(&provider.api_key, &provider.model);
    if !provider.base_url.is_empty() {
        model = model.with_base_url(&provider.base_url);
    }
    if provider.context_window > 0 {
        model = model.with_context_window(provider.context_window);
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
    // 本地 shell 执行（对齐 Java ShellExecuteTool）：锚定工作区根——
    // working_directory 相对路径在沙箱根下解析，绝对路径 / `~` / `..`
    // 已在工具层拒绝；权限面默认 ask（每次执行需用户批准）。
    toolkit.register(Arc::new(ShellExecuteTool::new(Arc::new(
        LocalShellBackend::new_anchored(workspace.root().to_path_buf()),
    ))));
    // 记忆工具锚定独立的 .ox 记忆工作区（<项目>/.ox/）：MEMORY.md 与
    // memory/ 台账不落项目根，非业务产出与业务代码分离（.ox 数据规范）。
    let memory = MemoryToolSet::new(memory_workspace.clone());
    toolkit.register(Arc::new(memory.memory_save));
    toolkit.register(Arc::new(memory.memory_get));
    toolkit.register(Arc::new(memory.memory_search));
    // 共享黑板（工作区级、跨会话共享）：模型侧只增不改；用户侧经管理面板
    // 删改（commands 直接操作同一 JSON 文件）。与记忆同锚 .ox/ 目录。
    let bb_store = agentscope_harness::blackboard::BlackboardStore::new(
        memory_workspace.root().join("blackboard.json"),
    );
    toolkit.register(Arc::new(
        agentscope_harness::blackboard::BlackboardAppendTool::new(bb_store.clone()),
    ));
    toolkit.register(Arc::new(
        agentscope_harness::blackboard::BlackboardReadTool::new(bb_store),
    ));
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
        // 成员同样是持久化会话：孤儿工具调用恢复与主 agent 保持一致，
        // 否则成员会话一旦留下孤儿就会永久卡死。
        .enable_pending_tool_recovery(Some(true))
        .build()
}

/// 构建一个 spawn 子代理实例（ReActAgent）：lead 的分身——**全量继承 lead 的
/// 工具面**（对齐 Java `buildGeneralPurposeFactory` 的 `capturedParentToolkit.copy()`：
/// 分身能干 lead 能干的活，含 shell/web/db），同模型；会话状态复用工作区
/// state store（spawn 派生的 child session id 不冲突）。
fn build_spawn_agent(
    name: &str,
    sys_prompt_body: &str,
    provider: &ModelProviderConfig,
    state_store: Arc<dyn AgentStateStore>,
    lead_toolkit: &Toolkit,
) -> Result<Arc<ReActAgent>, String> {
    let toolkit = allowlisted_inherited_toolkit(Some(lead_toolkit), &[]);
    let agent = ReActAgent::builder()
        .name(name)
        .sys_prompt(build_subagent_sys_prompt(sys_prompt_body))
        .model(build_model(provider)?)
        .toolkit(toolkit)
        .state_store(state_store)
        .default_session_id("subagent")
        // 分身与主 agent 同策略：孤儿工具调用补齐**错误**结果，避免分身
        // 会话因缺配对被模型协议层判 400 后永久卡死。
        .enable_pending_tool_recovery(true)
        .build()
        .map_err(|e| format!("build subagent `{name}`: {e}"))?;
    Ok(Arc::new(agent))
}

/// solo 的 spawn 子代理注册表：一个条目 = lead 的分身（general-purpose，
/// 继承 lead 的 sys_prompt、模型与**完整工具面**）。team 的成员协作走
/// TeamDelegateTool，不进 spawn 池（AgentScope 语义：solo 派分身，team 派成员）。
///
/// 必须在 lead 的全部业务工具（shell/web/db）注册完成之后调用——分身继承的
/// 是调用此刻的 toolkit 快照。
fn build_subagent_manager(
    provider: &ModelProviderConfig,
    lead_sys_prompt: &str,
    state_store: Arc<dyn AgentStateStore>,
    lead_toolkit: &Toolkit,
) -> Result<Arc<DefaultAgentManager>, String> {
    let manager = Arc::new(DefaultAgentManager::new());
    let store = Arc::clone(&state_store);
    let body = lead_sys_prompt.to_string();
    let provider = provider.clone();
    let lead_toolkit = lead_toolkit.copy();
    let factory: Arc<SubagentFactory> = Arc::new(move |_rc| {
        build_spawn_agent(
            GENERAL_PURPOSE_NAME,
            &body,
            &provider,
            Arc::clone(&store),
            &lead_toolkit,
        )
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
    config: &StudioConfig,
    agent: &AgentConfig,
    provider: &ModelProviderConfig,
    data_dir: &Path,
    runtime_mode: &str,
    member_defs: &[(AgentConfig, ModelProviderConfig)],
    resources: &ResourceHandles,
) -> Result<BuiltAgent, String> {
    // 运行方式解析：协同 → solo/team（用户可切）；ops/db/test → 由类型决定。
    let mode = match ws.kind.as_str() {
        "ops" => StudioMode::Ops,
        "db" => StudioMode::Db,
        "test" => StudioMode::Test,
        _ => StudioMode::parse(runtime_mode)
            .filter(|m| matches!(m, StudioMode::Solo | StudioMode::Team))
            .unwrap_or(StudioMode::Solo),
    };
    let dir = workspace_dir(data_dir, &ws.id);
    // 工作目录：配置指定绝对路径则用之（agent 直接在真实项目里干活），
    // 否则用默认目录。Sandboxed 模式把所有文件工具锚定在该根上。
    // 测试工作区绑定协同工作区时跟随目标的目录（两边 .ox 数据同处）。
    let workspace_root = effective_workspace_root(data_dir, config, ws);
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
    register_base_tools(&mut toolkit, &workspace, memory_workspace.clone())?;
    // 内置浏览器自动化（所有场景开放）：headful 真浏览器 + CDP，截图归
    // 工作区 `.ox/screenshots/`。
    let browser_ctx = Arc::new(crate::browser_tool::BrowserCtx {
        session: Arc::clone(&resources.browser),
        screenshots_dir: workspace_root.join(".ox").join("screenshots"),
    });
    toolkit.register(Arc::new(crate::browser_tool::BrowserNavigate { ctx: Arc::clone(&browser_ctx) }));
    toolkit.register(Arc::new(crate::browser_tool::BrowserSnapshot { ctx: Arc::clone(&browser_ctx) }));
    toolkit.register(Arc::new(crate::browser_tool::BrowserClick { ctx: Arc::clone(&browser_ctx) }));
    toolkit.register(Arc::new(crate::browser_tool::BrowserType { ctx: Arc::clone(&browser_ctx) }));
    toolkit.register(Arc::new(crate::browser_tool::BrowserScreenshot { ctx: Arc::clone(&browser_ctx) }));
    toolkit.register(Arc::new(crate::browser_tool::BrowserClose { ctx: Arc::clone(&browser_ctx) }));
    toolkit.register(Arc::new(crate::browser_tool::BrowserEvaluate { ctx: Arc::clone(&browser_ctx) }));
    // 旧版任务记录锚在 workspace_root（用户项目根）下的 tasks/，违反 .ox
    // 数据规范（非业务内容不散落项目根）——一次性迁入 .ox/tasks/。
    // tmp 残留（原子写的 *.json.tmp.*）直接删；目录空了才移除。
    let legacy_tasks = workspace_root.join("tasks");
    if legacy_tasks.is_dir() {
        let ox_tasks = memory_workspace.root().join("tasks");
        std::fs::create_dir_all(&ox_tasks).ok();
        if let Ok(entries) = std::fs::read_dir(&legacy_tasks) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.extension().map(|x| x == "json").unwrap_or(false) {
                    let _ = std::fs::rename(&p, ox_tasks.join(entry.file_name()));
                } else {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
        let _ = std::fs::remove_dir(&legacy_tasks);
    }
    // 任务仓储：collab 模式用工作区持久化仓储（任务记录与结果落工作区，
    // 进程重启后 task_list/task_output 仍能恢复历史任务——修复纯内存仓储
    // 导致的后台任务句柄丢失）。同一装配单元内 task 工具、spawn core 与
    // builder 必须共用同一实例：原先各自 new 的 InMemory 实例互不相通，
    // task_output 查不到 spawn 写入的任务（not_found 的另一半根因）。
    // 锚 .ox 数据根（memory_workspace 即 <workspace_root>/.ox）：任务记录
    // 与记忆/黑板同规范，不落项目根。
    let task_repo: Arc<dyn TaskRepository> = match mode {
        StudioMode::Solo | StudioMode::Team => Arc::new(WorkspaceTaskRepository::new(
            Arc::clone(&memory_workspace),
            agent.id.clone(),
        )),
        StudioMode::Ops | StudioMode::Db | StudioMode::Test => Arc::new(InMemoryTaskRepository::new()),
    };
    match mode {
        StudioMode::Ops => {
            // 远程运维工具面：web 检索（remote_shell 在 match 后统一注册）。
            toolkit.register(Arc::new(WebFetchTool::new()));
            toolkit.register(Arc::new(WebSearchTool::new()));
        }
        StudioMode::Test => {
            // 测试工具面：web 检索（查文档/排错资料）；浏览器 7 件套已在
            // 上方对所有模式统一注册。
            toolkit.register(Arc::new(WebFetchTool::new()));
            toolkit.register(Arc::new(WebSearchTool::new()));
        }
        StudioMode::Db => {
            // 数据库工具面：无类别特有工具（db_query/db_dml 在 match 后统一注册）。
        }
        StudioMode::Team => {
            // 任务工具（对齐 Java subagent/task 体系的工具面）。
            let repo = Arc::clone(&task_repo);
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
            // 任务工具（spawn 三件套在下方「远程能力统一注册」之后装配——
            // 分身要继承 lead 的完整工具面，必须等 shell/db 注册完成）。
            let repo = Arc::clone(&task_repo);
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
    // 需求协作工具（协同专属：Solo/Team 两种运行方式都注册；ops/db/test 不注册）。
    // 打通协同↔测试闭环：讨论沉淀需求（req_create）、查需求与 Bug（req_list/
    // req_detail）、修复后置状态（bug_set_status）。数据锚定生效工作区根下的
    // .ox/testing/——测试工作区绑定协同工作区时两边读写同一份数据。
    if matches!(mode, StudioMode::Solo | StudioMode::Team) {
        let req_ctx = crate::req_tool::ReqCtx {
            data_dir: data_dir.to_path_buf(),
            config: Arc::clone(&resources.config),
            workspace_id: ws.id.clone(),
        };
        toolkit.register(Arc::new(crate::req_tool::ReqCreate { ctx: req_ctx.clone() }));
        toolkit.register(Arc::new(crate::req_tool::ReqList { ctx: req_ctx.clone() }));
        toolkit.register(Arc::new(crate::req_tool::ReqDetail { ctx: req_ctx.clone() }));
        toolkit.register(Arc::new(crate::req_tool::BugSetStatus { ctx: req_ctx }));
    }
    // 远程能力统一注册（ops/db 的类别默认 + 各类别的绑定开放，一套语义）：
    // - ops 类别默认开放 remote_shell；db 类别默认开放 db_query/db_dml；
    // - 其余类别（collab）绑定非空才开放对应工具；
    // - 绑定列表非空时一律收窄为白名单（工具内校验，越界即拒绝）。
    // - test 类别工具面固定：既不开放也不因绑定而开放（测试只做浏览器测试）。
    if mode != StudioMode::Test && (mode == StudioMode::Ops || !ws.bound_servers.is_empty()) {
        toolkit.register(Arc::new(RemoteShellTool::new(
            Arc::clone(&resources.ssh_pool),
            allowlist(&ws.bound_servers),
        ).with_terminal_workspace((mode == StudioMode::Ops).then(|| ws.id.clone()))
        .with_session_dir(workspace_dir(data_dir, &ws.id).join("state").join(USER_ID))
        .with_config(Arc::clone(&resources.config))));
    }
    if mode != StudioMode::Test && (mode == StudioMode::Db || !ws.bound_databases.is_empty()) {
        let allowed = allowlist(&ws.bound_databases);
        // 会话状态目录：db 工具执行时按会话绑定（binding.json）收窄白名单。
        let session_state_dir = Some(
            workspace_dir(data_dir, &ws.id).join("state").join(USER_ID),
        );
        toolkit.register(Arc::new(DbQueryTool::new(
            Arc::clone(&resources.db_catalog),
            allowed.clone(),
            session_state_dir.clone(),
        )));
        toolkit.register(Arc::new(DbDmlTool::new(
            Arc::clone(&resources.db_catalog),
            allowed,
            session_state_dir,
        )));
    }

    // solo 的 spawn 分身体系（AgentScope 语义：solo 派自己的分身，独立会话跑
    // 子任务）。必须在全部业务工具注册完成之后构建——分身全量继承 lead 的
    // 工具面（对齐 Java buildGeneralPurposeFactory 的 capturedParentToolkit.copy()），
    // 否则分身缺 shell/web/db，接到需要执行命令的子任务只能失败重试。
    if mode == StudioMode::Solo {
        let manager = build_subagent_manager(
            provider,
            &agent.sys_prompt,
            Arc::clone(&state_store),
            &toolkit,
        )?;
        let core = AgentSpawnCore::new(manager, Arc::clone(&task_repo), 0);
        toolkit.register(Arc::new(AgentSpawnTool::new(Arc::clone(&core))));
        toolkit.register(Arc::new(AgentSendTool::new(Arc::clone(&core))));
        toolkit.register(Arc::new(AgentListTool::new(core)));
    }

    // 只读工具名单：toolkit move 进 builder 前收集（权限白名单依据）。
    let read_only_tools: Vec<String> = toolkit
        .tool_names()
        .into_iter()
        .filter(|name| toolkit.get(name).is_some_and(|tool| tool.is_read_only()))
        .collect();
    let mut builder = HarnessAgentBuilder::new()
        .name(&agent.name)
        .sys_prompt(sys_prompt_for(agent, mode))
        .model(build_model(provider)?)
        .toolkit(toolkit)
        .workspace_manager(Arc::clone(&workspace))
        // compaction flush/consolidation 与 memory_save 同锚 .ox 记忆工作区：
        // 否则压缩时的记忆抽取会把 memory/ 台账写到项目根（曾污染用户仓库）。
        .with_memory_workspace(memory_workspace)
        .state_store(state_store)
        .default_session_id("main");
    if agent.max_iters > 0 {
        builder = builder.max_iters(agent.max_iters as usize);
    } else {
        // 0 = 未配置（含旧版本配置文件落盘的 0）→ 兜底 50：比 core 默认 10
        // 宽松，但低于内置智能体显式配置的 80（DEFAULT_MAX_ITERS）。
        builder = builder.max_iters(config::FALLBACK_MAX_ITERS as usize);
    }
    // 孤儿工具调用恢复：**显式开启**（Java 侧全局默认是 false）。
    //
    // 偏离理由：Studio 是会话持久化的桌面应用，工具执行前进程退出/回合被中断
    // 是常态而非边缘情况。恢复关闭时，历史里一旦留下「有 tool_use 无 tool_result」
    // 的孤儿，该会话就会永久卡死——每次调用都因缺配对被模型协议层判 400，
    // 用户既看不到原因也无法继续。开启后孤儿会被补上**错误**结果（不伪造成功），
    // 会话可以继续。
    //
    // 补齐只影响「最后一条 assistant 消息里没有结果的调用」，绝不覆盖已有真实
    // 结果，也不碰仍在等用户授权（ASKING）的调用。
    builder = builder.enable_pending_tool_recovery(Some(true));
    // 危险工具默认走用户授权（AgentScope 权限引擎 ask 规则，会话级持久化）：
    // shell 执行与文件写入/编辑需用户批准后才会执行；目录穿越由 Sandboxed
    // 文件层硬拒（path escapes workspace root），不在此列。
    builder = builder.permission_context(default_permission_context(
        &read_only_tools,
        mode,
        ws,
        data_dir,
        &ws.id,
    ));

    if mode == StudioMode::Solo {
        // plan/free 切换仅 solo 运行方式提供（产品语义）。必须显式启用 plan
        // 能力，否则 enter_plan_mode 报 "plan mode is not enabled on this agent"。
        // 计划文件系统强制锚定 .ox/plans/（.ox 数据规范：运行时产物不落
        // 项目根；机制对齐 Java PlanModeManager 的可配置 planDir）。
        builder = builder
            .enable_plan_mode()
            .plan_file_directory(".ox/plans")
            .plan_prompt(crate::default_prompt::SOLO_PLAN_PROMPT);
    }
    // 测试工作区不接 harness 的 plan 状态机：那里的 plan 语义是「代码修改前的
    // 只读设计阶段」（拦写工具 + 注入「只做设计不执行」提示 + plan_exit 需人工
    // 批准），与测试自动执行（模型要真的点按钮填表单）直接冲突。测试的「计划」
    // 是产品语义层的用例集（用例即计划），由 TEST_PROMPT 与测试工作台把关。
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
        builder = builder.task_repository(Arc::clone(&task_repo));
    }
    let agent = builder.build()?;
    Ok(BuiltAgent { agent })
}

/// 新会话的默认权限上下文：危险工具（shell 执行、文件写入/编辑）→ ask 行为，
/// 由 AgentScope 权限引擎在执行前挂起等待用户确认（对齐 Java ask 规则语义）。
/// 用户批准时可附带 allow 规则（「始终允许」），写进会话权限上下文后生效。
///
/// 免确认白名单（allow 规则；ask 工具不进白名单，不会被误放）：
/// - 只读工具：按 `Tool::is_read_only()` 从 toolkit 动态收集（查询/读取/记忆
///   等非编辑类工具不弹授权）；
/// - 永久授权：用户在确认弹窗选「永久允许」时写入 `<workspace_dir>/permissions.json`，
///   装配时读回合并（source: "user"），跨会话生效。
///
/// 低风险开发操作白名单（免确认）：有写语义但面向开发辅助流程、不触碰
/// 用户代码资产——记忆保存（.ox 内）、子代理派发/续聊、plan 流程工具。
/// 对齐 Java PlanModeMiddleware.ALWAYS_ALLOWED 的开发辅助语义（plan_exit
/// 除外：其 check_permissions 恒 RequireApproval，审批走 HITL）。
const DEVELOP_ALLOWLIST: &[&str] = &[
    "memory_save",
    // 黑板登记：写 .ox/blackboard.json（非用户代码资产），协议要求「结论/风险
    // 即时登记不攒收尾」——走 ask 会把即时性打断，与 memory_save 同语义免确认。
    "blackboard_append",
    "agent_spawn",
    "agent_send",
    "plan_write",
    "team_delegate",
];

fn default_permission_context(
    read_only_tools: &[String],
    mode: StudioMode,
    ws: &WorkspaceConfig,
    data_dir: &Path,
    workspace_id: &str,
) -> PermissionContextState {
    // ask 工具集按工作区类别区分：
    // - collab（solo/team）：shell 执行与文件写入/编辑；按绑定开放远程能力时
    //   追加对应 ask 规则（remote_shell / db_dml 每次确认，语义对齐 ops/db）；
    // - ops：remote_shell（远程命令每次确认）；
    // - db：db_dml（数据变更每次确认；db_query 只读免确认）。
    let mut ask_tools: Vec<&'static str> = match mode {
        StudioMode::Ops => vec!["remote_shell"],
        StudioMode::Db => vec!["db_dml"],
        // 测试工作区：浏览器工具全部只读免确认（headless 沙箱内操作），
        // 文件写入/编辑/shell 不在工具面，无需 ask 规则。
        StudioMode::Test => vec![],
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
    // 只读工具免确认：名单由调用方在 toolkit move 前按 `Tool::is_read_only()`
    // 收集，新只读工具无需改这里。
    // 另追加低风险开发操作（有写语义但面向开发辅助、非用户代码资产）：
    // 记忆保存、子代理派发/续聊、plan 流程（plan_exit 除外——工具自裁决
    // RequireApproval，对齐 Java PlanModeTools.planExit）。编辑/写文件/shell/
    // 远程/数据变更仍走 ask。
    for name in read_only_tools
        .iter()
        .map(String::as_str)
        .chain(DEVELOP_ALLOWLIST.iter().copied())
    {
        if !ask_tools.contains(&name) {
            context.add_allow_rule(PermissionRule {
                tool_name: name.to_string(),
                rule_content: None,
                behavior: PermissionBehavior::Allow,
                source: "project".to_string(),
            });
        }
    }
    // 用户永久授权（跨会话）：覆盖同工具的 ask 配置（add_allow_rule 自带
    // 移除同工具 ask 的语义，用户显式意愿优先）。
    // db_dml 永不永久授权：数据变更每条语句语义不同，必须逐次确认——
    // 写入口已拒绝，这里再过滤一次（堵存量 permissions.json 里的旧记录）。
    let mut permanent = load_permanent_permissions(data_dir, workspace_id);
    permanent.retain(|t| t != "db_dml");
    for tool_name in permanent {
        ask_tools.retain(|ask| ask != &tool_name);
        context.add_allow_rule(PermissionRule {
            tool_name,
            rule_content: None,
            behavior: PermissionBehavior::Allow,
            source: "user".to_string(),
        });
    }
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


/// 永久授权存储：`<workspace_dir>/permissions.json`（运行时数据，随工作区隔离）。
fn permissions_file(data_dir: &Path, workspace_id: &str) -> PathBuf {
    workspace_dir(data_dir, workspace_id).join("permissions.json")
}

/// 读永久授权工具名单；文件缺失/损坏按空处理（不阻断装配）。
pub(crate) fn load_permanent_permissions(data_dir: &Path, workspace_id: &str) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(permissions_file(data_dir, workspace_id)) else {
        return Vec::new();
    };
    serde_json::from_str::<PermanentPermissions>(&text)
        .map(|stored| stored.allowed_tools)
        .unwrap_or_default()
}

/// 永久授权持久化结构（`{"allowed_tools": [...]}`，按工具名整工具授权）。
#[derive(serde::Serialize, serde::Deserialize, Default)]
pub(crate) struct PermanentPermissions {
    #[serde(default)]
    pub allowed_tools: Vec<String>,
}

/// 保存永久授权（追加去重）；已装配 agent 的缓存由调用方失效。
pub(crate) fn save_permanent_permission(
    data_dir: &Path,
    workspace_id: &str,
    tool_name: &str,
) -> Result<(), String> {
    // db_dml 永不永久授权：数据变更必须逐次人工确认（每条语句影响面不同）。
    if tool_name == "db_dml" {
        return Err("数据变更（db_dml）不支持永久授权，每次执行都需确认".to_string());
    }
    let mut stored = PermanentPermissions {
        allowed_tools: load_permanent_permissions(data_dir, workspace_id),
    };
    if !stored.allowed_tools.iter().any(|t| t == tool_name) {
        stored.allowed_tools.push(tool_name.to_string());
    }
    let path = permissions_file(data_dir, workspace_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(&stored).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| e.to_string())
}

/// 删除永久授权（撤销「永久允许」；名单清空则移除文件）。
pub(crate) fn delete_permanent_permission(
    data_dir: &Path,
    workspace_id: &str,
    tool_name: &str,
) -> Result<(), String> {
    let mut stored = PermanentPermissions {
        allowed_tools: load_permanent_permissions(data_dir, workspace_id),
    };
    stored.allowed_tools.retain(|t| t != tool_name);
    let path = permissions_file(data_dir, workspace_id);
    if stored.allowed_tools.is_empty() {
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }
    let text = serde_json::to_string_pretty(&stored).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| e.to_string())
}

/// 绑定列表 → 工具白名单：空 = 不限（`None`），非空 = 白名单（`Some`）。
fn allowlist(bound: &[String]) -> Option<Vec<String>> {
    if bound.is_empty() {
        None
    } else {
        Some(bound.to_vec())
    }
}

/// 系统提示：description → 基座协议（[`default_prompt::BASE_PROMPT`]，四类
/// 模式共用——文件/shell/记忆工具全模式注册，web/remote/db 等模式特有工具
/// 在各场景层说明）→ 用户自定义 sys_prompt → 场景层（solo 自由 / team 领队 /
/// ops 运维 / db 数据库 / test 测试；ops/db/test 无 team 与 plan 运行方式，
/// 场景层仅 free 语义）
/// → .ox 数据规范。
///
/// plan 场景层（[`default_prompt::SOLO_PLAN_PROMPT`]）不在此拼接——plan 由
/// 用户手动开启，经 `PlanModeMiddleware` 在进入 plan 态时动态注入。
fn sys_prompt_for(agent: &AgentConfig, mode: StudioMode) -> String {
    let mut prompt = String::new();
    if !agent.description.is_empty() {
        prompt.push_str(&agent.description);
        prompt.push_str("\n\n");
    }
    prompt.push_str(crate::default_prompt::BASE_PROMPT.trim());
    prompt.push_str("\n\n");
    prompt.push_str(agent.sys_prompt.trim());
    let role_note: String = match mode {
        StudioMode::Solo => crate::default_prompt::SOLO_FREE_PROMPT.to_string(),
        StudioMode::Team => crate::default_prompt::TEAM_PROMPT.to_string(),
        StudioMode::Ops => crate::default_prompt::OPS_PROMPT.to_string(),
        StudioMode::Db => crate::default_prompt::DB_PROMPT.to_string(),
        StudioMode::Test => crate::default_prompt::TEST_PROMPT.to_string(),
    };
    prompt.push_str("\n\n");
    prompt.push_str(&role_note);
    // 需求-Bug 闭环协议：协同侧（solo/team）追加——与测试工作区共享需求/Bug
    // 数据，req_* 工具只在协同面注册，协议也只对协同面拼接。
    if matches!(mode, StudioMode::Solo | StudioMode::Team) {
        prompt.push_str("\n\n");
        prompt.push_str(crate::default_prompt::REQ_LOOP_PROMPT.trim());
    }
    // .ox 数据规范：.ox/ 是系统数据区（记忆/黑板/会话/计划，路径系统锚定），
    // 模型自由写文件不得进入；业务代码之外的非业务文件放用户指定位置或系统临时目录。
    prompt.push_str(
        "\n\nWorkspace data convention: the project root holds business code only. \
         `.ox/` is the system data area (memory, blackboard, sessions, plans — paths are \
         system-anchored). NEVER write there with write_file/edit_file; use the dedicated \
         tools (memory_save / blackboard_append / plan_write) instead. Scratch files go to \
         the system temp directory or a user-specified location, not `.ox/`.",
    );
    prompt
}

#[cfg(test)]
mod tests {
    #[test]
    fn expand_home_resolves_user_profile() {
        // 主目录变量存在时 ~ 展开为主目录前缀（不再以字面 ~ 开头）
        if std::env::var_os("USERPROFILE").is_some() || std::env::var_os("HOME").is_some() {
            let p = expand_home("~/.ox/db/my");
            assert!(!p.starts_with("~"), "got: {p:?}");
            assert!(p.ends_with(".ox/db/my") || p.ends_with(".ox\\db\\my"), "got: {p:?}");
        }
        // 无 ~ 前缀原样
        assert_eq!(
            expand_home("F:\\java\\x"),
            std::path::PathBuf::from("F:\\java\\x")
        );
    }

    use super::*;

    /// 测试工作区绑定协同工作区的目录解析：跟随目标（目标有自定义 dir 用之，
    /// 空则用目标默认目录）；目标缺失（脏绑定）回落自己的目录；非 test 不受影响。
    #[test]
    fn effective_workspace_root_follows_bound_collab() {
        let data_dir = std::env::temp_dir().join(format!("eff_root_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);

        let mk = |id: &str, kind: &str, dir: &str, bound: Option<&str>| WorkspaceConfig {
            id: id.into(),
            name: id.into(),
            kind: kind.into(),
            agent_id: "a".into(),
            dir: dir.into(),
            members: vec![],
            bound_servers: vec![],
            bound_databases: vec![],
            created_at: 0,
            auto_resource: None,
            bound_workspace_id: bound.map(|s| s.to_string()),
        };
        let mut config = StudioConfig::default();
        // 协同工作区：自定义目录（真实项目目录的典型形态）。
        config.workspaces.push(mk("ws-collab", "collab", "F:\\proj\\demo", None));
        // 测试工作区：绑定协同，自己不设 dir。
        config.workspaces.push(mk("ws-test", "test", "", Some("ws-collab")));
        // 对照：未绑定的测试工作区。
        config.workspaces.push(mk("ws-test-free", "test", "", None));

        // 绑定 → 跟随协同的自定义目录（两边 .ox 数据同处）。
        let test_ws = config.workspace("ws-test").unwrap();
        assert_eq!(
            effective_workspace_root(&data_dir, &config, test_ws),
            std::path::PathBuf::from("F:\\proj\\demo")
        );
        // 协同自己解析不变。
        let collab_ws = config.workspace("ws-collab").unwrap();
        assert_eq!(
            effective_workspace_root(&data_dir, &config, collab_ws),
            std::path::PathBuf::from("F:\\proj\\demo")
        );
        // 未绑定 → 自己的默认目录（行为与旧版一致）。
        let free_ws = config.workspace("ws-test-free").unwrap();
        assert_eq!(
            effective_workspace_root(&data_dir, &config, free_ws),
            workspace_dir(&data_dir, "ws-test-free").join("workspace")
        );

        // 协同目录为空 → 跟随目标的默认目录（引用先结束，再改配置）。
        config.workspaces[0].dir = String::new();
        let test_ws = config.workspace("ws-test").unwrap();
        assert_eq!(
            effective_workspace_root(&data_dir, &config, test_ws),
            workspace_dir(&data_dir, "ws-collab").join("workspace")
        );

        // 脏绑定（目标已删）→ 回落自己的目录，不 panic。
        config.workspaces.retain(|w| w.id != "ws-collab");
        let test_ws = config.workspace("ws-test").unwrap();
        assert_eq!(
            effective_workspace_root(&data_dir, &config, test_ws),
            workspace_dir(&data_dir, "ws-test").join("workspace")
        );

        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn permanent_permissions_roundtrip_and_delete() {
        let dir = std::env::temp_dir().join(format!("perm_test_{}", std::process::id()));
        let ws = "ws_roundtrip";
        let _ = std::fs::remove_dir_all(&dir);

        assert!(load_permanent_permissions(&dir, ws).is_empty());
        save_permanent_permission(&dir, ws, "execute").unwrap();
        save_permanent_permission(&dir, ws, "execute").unwrap(); // 去重
        save_permanent_permission(&dir, ws, "remote_shell").unwrap();
        let mut got = load_permanent_permissions(&dir, ws);
        got.sort();
        assert_eq!(got, vec!["execute".to_string(), "remote_shell".to_string()]);

        delete_permanent_permission(&dir, ws, "execute").unwrap();
        assert_eq!(load_permanent_permissions(&dir, ws), vec!["remote_shell".to_string()]);
        delete_permanent_permission(&dir, ws, "remote_shell").unwrap();
        // 名单清空后文件移除，回到空。
        assert!(load_permanent_permissions(&dir, ws).is_empty());
        assert!(!permissions_file(&dir, ws).exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_context_ask_tools_not_in_readonly_allowlist() {
        // ask 工具即使被误标只读也不进白名单（ask 优先级保护）。
        let read_only = vec!["grep_files".to_string(), "execute".to_string()];
        let ws = WorkspaceConfig {
            id: "ws_t".into(),
            name: "t".into(),
            kind: "collab".into(),
            agent_id: "a".into(),
            dir: String::new(),
            members: vec![],
            bound_servers: vec![],
            bound_databases: vec![],
            created_at: 0,
            auto_resource: None,
            bound_workspace_id: None,
        };
        let context = default_permission_context(
            &read_only,
            StudioMode::Solo,
            &ws,
            std::path::Path::new("nonexistent-dir"),
            "ws_t",
        );
        assert!(context.allow_rules.contains_key("grep_files"));
        assert!(!context.allow_rules.contains_key("execute"));
        assert!(context.ask_rules.contains_key("execute"));
        // 低风险开发操作免确认（DEVELOP_ALLOWLIST），编辑/写/shell 仍 ask。
        assert!(context.allow_rules.contains_key("memory_save"));
        assert!(context.allow_rules.contains_key("blackboard_append"));
        assert!(context.allow_rules.contains_key("agent_spawn"));
        assert!(context.allow_rules.contains_key("plan_write"));
        // plan_exit 不进白名单：工具自裁决 RequireApproval（审批走 HITL）。
        assert!(!context.allow_rules.contains_key("plan_exit"));
    }
}