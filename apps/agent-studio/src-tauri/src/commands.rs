//! Tauri 命令面（≈ dataplane REST 面的本地化翻译）。
//!
//! 资源与操作对齐 agentscope-service：
//! - providers / agents：资源 CRUD（≈ Environment / Managed Agent 定义）；
//! - send_message / interrupt_turn：turn 生命周期（≈ SessionTurnRunner）；
//! - set_plan_mode / get_plan_status：plan 模式（≈ harness plan API）；
//! - get_history / clear_session：会话视图与运维（≈ AgentSession 视图面）。
//!
//! 事件投影：AgentEvent serde 序列化后装进信封 emit（前端按 kind 渲染，
//! 对齐 AgentSessionView.apply 的视图维护思路）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agentscope_core::message::{ContentBlock, Role, Source};
use agentscope_core::{Msg, RuntimeContext};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

use crate::config::{AgentConfig, ConfigStore, DatabaseServerConfig, ModelProviderConfig, StudioConfig, WorkspaceConfig, WORKSPACE_KINDS};
use crate::config::ServerConfig;
use crate::db_exec::{self, DbCatalog};
use crate::factory;
use crate::ssh_tool::{self, SshPool};
use crate::session::{AgentHandle, SessionHandle, TurnInfo};

/// 统一 user id（桌面单用户；对齐 service 的 owner 概念的最小形态）。
pub(crate) const USER_ID: &str = "studio";

/// 事件信封：前端按 (workspaceId, sessionId, turnId) 归流，event 为 AgentEvent JSON。
#[derive(Debug, Clone, Serialize)]
struct EventEnvelope {
    workspace_id: String,
    session_id: String,
    turn_id: String,
    event: serde_json::Value,
}

/// turn 终态通知（对齐 Java Task.settled() 的前端可观测形态）。
#[derive(Debug, Clone, Serialize)]
struct TurnFinished {
    workspace_id: String,
    session_id: String,
    turn_id: String,
    /// completed | interrupted | failed
    outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// plan 模式状态（对齐 harness plan API 的返回面）。
#[derive(Debug, Clone, Serialize)]
pub struct PlanStatus {
    pub active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_file: Option<String>,
}

/// 会话历史条目（块级视图：blocks 是 ContentBlock 序列化值，前端按
/// type 渲染 text/thinking/tool_use/tool_result；text 是纯文本摘要）。
#[derive(Debug, Clone, Serialize)]
pub struct HistoryItem {
    pub role: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    pub blocks: Vec<serde_json::Value>,
}

/// 会话摘要（list_sessions 返回；对齐 service 的会话发现面）。
#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    pub session_id: String,
    /// 对话真相里的消息条数（AgentState.context）。
    pub messages: usize,
    /// 状态文件最后修改时间（unix 秒；用于排序）。
    pub updated_at: u64,
    /// 派生本会话的父会话（spawn 子代理会话才有；来自父会话
    /// tool_context.spawn_registry，前端据此树形归属）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// 会话名：自定义名（title.txt）优先，否则自动取首条用户消息摘要；
    /// 都没有（新会话未发消息）为 None，前端回退显示 session id。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// 全局应用状态。
pub struct AppState {
    pub config_store: ConfigStore,
    pub config: Mutex<StudioConfig>,
    /// 已装配 agent 缓存（懒装配；配置变更后失效重建）。
    pub handles: Mutex<HashMap<String, Arc<AgentHandle>>>,
    pub data_dir: std::path::PathBuf,
    /// SSH 连接池（ops 工作区：remote_shell 工具与服务器面板共用）。
    pub ssh_pool: Arc<SshPool>,
    /// 数据库档案目录（db 工作区：db_query/db_dml 工具与查询模式共用）。
    pub db_catalog: Arc<DbCatalog>,
}

impl AppState {
    fn snapshot(&self) -> StudioConfig {
        self.config.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn persist(&self, config: &StudioConfig) -> Result<(), String> {
        *self.config.lock().unwrap_or_else(|e| e.into_inner()) = config.clone();
        self.config_store.save(config)
    }

    /// 取（或懒装配）工作区句柄（对齐 service 的 agent 按需实例化）。
    /// 缓存 key = `{workspace_id}:{runtime_mode}`——协同工作区可按运行方式
    /// （solo/team）装配不同实例（工具面与成员装配不同）。
    pub fn handle(&self, workspace_id: &str, runtime_mode: &str) -> Result<Arc<AgentHandle>, String> {
        let key = format!("{workspace_id}:{runtime_mode}");
        if let Some(handle) = self
            .handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            return Ok(Arc::clone(handle));
        }
        let config = self.snapshot();
        let ws = config
            .workspace(workspace_id)
            .cloned()
            .ok_or_else(|| format!("unknown workspace `{workspace_id}`"))?;
        let cfg = config
            .agent(&ws.agent_id)
            .cloned()
            .ok_or_else(|| format!("workspace `{workspace_id}` 引用的智能体 `{}` 不存在", ws.agent_id))?;
        // 未绑定 provider（如内置默认智能体首次使用）给明确指引，而非
        // `references unknown provider \`\`` 的空名报错。
        if cfg.provider.trim().is_empty() {
            return Err(format!(
                "智能体 `{}` 尚未绑定模型 Provider，请在设置页配置后重试",
                cfg.name
            ));
        }
        let provider = config
            .provider(&cfg.provider)
            .cloned()
            .ok_or_else(|| format!("智能体 `{}` 引用了未知 provider `{}`", cfg.name, cfg.provider))?;
        // team 运行方式：成员是工作区引用的已有智能体（协同 = 多个智能体），
        // 此处解析成员定义与其 provider，缺失即报错（保存时也校验）。
        let mut member_defs = Vec::new();
        for mid in &ws.members {
            if mid == &ws.agent_id {
                return Err("team 成员不能是 lead 智能体自己".to_string());
            }
            let m = config
                .agent(mid)
                .cloned()
                .ok_or_else(|| format!("team 成员 `{mid}` 不存在，请检查工作区成员配置"))?;
            let mp = config
                .provider(&m.provider)
                .cloned()
                .ok_or_else(|| format!("team 成员 `{mid}` 引用了未知 provider `{}`", m.provider))?;
            member_defs.push((m, mp));
        }
        let resources = factory::ResourceHandles {
            ssh_pool: Arc::clone(&self.ssh_pool),
            db_catalog: Arc::clone(&self.db_catalog),
        };
        let built = factory::build_agent(&ws, &cfg, &provider, &self.data_dir, runtime_mode, &member_defs, &resources)?;
        let handle = Arc::new(AgentHandle::new(built.agent));
        self.handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, Arc::clone(&handle));
        Ok(handle)
    }

    /// 配置变更后失效缓存（下次 handle 重建）；移除该工作区所有运行方式的实例。
    fn invalidate(&self, workspace_id: &str) {
        let prefix = format!("{workspace_id}:");
        self.handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|k, _| !k.starts_with(&prefix));
    }
}

// ---------- providers ----------

#[tauri::command]
pub fn list_providers(state: State<'_, AppState>) -> Vec<ModelProviderConfig> {
    state.snapshot().providers
}

#[tauri::command]
pub fn save_provider(state: State<'_, AppState>, provider: ModelProviderConfig) -> Result<(), String> {
    let mut config = state.snapshot();
    config.providers.retain(|p| p.name != provider.name);
    config.providers.push(provider);
    state.persist(&config)
}

#[tauri::command]
pub fn delete_provider(state: State<'_, AppState>, name: String) -> Result<(), String> {
    let mut config = state.snapshot();
    if config.agents.iter().any(|a| a.provider == name) {
        return Err(format!("provider `{name}` is referenced by an agent"));
    }
    config.providers.retain(|p| p.name != name);
    state.persist(&config)
}

// ---------- databases ----------

#[tauri::command]
pub fn list_databases(state: State<'_, AppState>) -> Vec<DatabaseServerConfig> {
    state.snapshot().databases
}

#[tauri::command]
pub fn save_database(
    state: State<'_, AppState>,
    database: DatabaseServerConfig,
) -> Result<SaveDatabaseOutcome, String> {
    let name = database.name.trim().to_string();
    if name.is_empty() {
        return Err("database name is empty".to_string());
    }
    // 类型合法性查注册表（新增类型在 db_kind::DATABASE_KINDS 加一行即可）。
    let kind = crate::db_kind::DatabaseKind::find(&database.db_type)
        .ok_or_else(|| format!("unknown db_type `{}`", database.db_type))?;
    if kind.needs_host && database.host.trim().is_empty() {
        return Err(format!("{} requires a host (or file path)", kind.label));
    }
    let mut database = database;
    database.name = name;
    // CLI 驱动类型：保存后立即验证 CLI 可用性（自动探索 PATH 与常见安装
    // 目录，或用档案指定的 cli_path）——只提示不阻断：用户可能先存档案
    // 后装 CLI，或档案先建、机器稍后配好。
    let outcome = match kind.cli_program() {
        Some(program) => match crate::db_exec::probe_cli(
            &database.db_type,
            &database.cli_path,
            program,
        ) {
            Ok(path) => SaveDatabaseOutcome {
                cli_resolved: Some(path),
                cli_error: None,
            },
            Err(e) => SaveDatabaseOutcome {
                cli_resolved: None,
                cli_error: Some(e),
            },
        },
        None => SaveDatabaseOutcome {
            cli_resolved: None,
            cli_error: None,
        },
    };
    let mut config = state.snapshot();
    config.databases.retain(|d| d.name != database.name);
    config.databases.push(database.clone());
    state.db_catalog.replace_all(&config.databases);
    state.persist(&config).map(|_| outcome)
}

/// 保存数据库档案的验证结果（CLI 驱动类型才有内容；不阻断保存）。
#[derive(serde::Serialize)]
pub struct SaveDatabaseOutcome {
    /// 自动探索/指定路径解析到的 CLI 完整路径。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cli_resolved: Option<String>,
    /// 未找到时的提示（含已尝试的位置与下一步建议）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cli_error: Option<String>,
}

/// 手动触发 CLI 探测（前端「探测」按钮）：Ok=解析到的完整路径。
#[tauri::command]
pub fn probe_database_cli(db_type: String, cli_path: String) -> std::result::Result<String, String> {
    let kind = crate::db_kind::DatabaseKind::find(&db_type)
        .ok_or_else(|| format!("unknown db_type `{db_type}`"))?;
    let program = kind
        .cli_program()
        .ok_or_else(|| "该数据库为内置驱动，无需外部 CLI".to_string())?;
    crate::db_exec::probe_cli(&db_type, &cli_path, program)
}

#[tauri::command]
pub fn delete_database(state: State<'_, AppState>, name: String) -> Result<(), String> {
    let mut config = state.snapshot();
    config.databases.retain(|d| d.name != name);
    state.db_catalog.replace_all(&config.databases);
    state.persist(&config)
}

/// 数据库类型注册表（前端下拉与字段启停的数据源；新增类型自动跟随）。
#[tauri::command]
pub fn list_database_kinds() -> &'static [crate::db_kind::DatabaseKind] {
    crate::db_kind::DATABASE_KINDS
}

// ---------- ssh servers（ops 工作区：连接 + 目录/文件操作） ----------

/// SSH 服务器档案 CRUD（对齐数据库档案：本地配置文件存储，密码不进日志）。
#[tauri::command]
pub fn list_servers(state: State<'_, AppState>) -> Vec<ServerConfig> {
    state.snapshot().servers
}

#[tauri::command]
pub fn save_server(state: State<'_, AppState>, server: ServerConfig) -> Result<(), String> {
    let name = server.name.trim().to_string();
    if name.is_empty() {
        return Err("server name is empty".to_string());
    }
    if server.host.trim().is_empty() {
        return Err("server host is empty".to_string());
    }
    let mut server = server;
    server.name = name;
    let mut config = state.snapshot();
    config.servers.retain(|s| s.name != server.name);
    config.servers.push(server);
    state.persist(&config)
}

#[tauri::command]
pub fn delete_server(state: State<'_, AppState>, name: String) -> Result<(), String> {
    let mut config = state.snapshot();
    config.servers.retain(|s| s.name != name);
    state.persist(&config)
}

/// 按名取 SSH 服务器档案。
fn find_server(config: &StudioConfig, name: &str) -> Result<ServerConfig, String> {
    config
        .servers
        .iter()
        .find(|s| s.name == name)
        .cloned()
        .ok_or_else(|| format!("server `{name}` not found"))
}

/// 连接 SSH 服务器（密码认证；成功后缓存进连接池）。
#[tauri::command]
pub async fn ssh_connect(state: State<'_, AppState>, name: String) -> Result<(), String> {
    let server = find_server(&state.snapshot(), &name)?;
    state.ssh_pool.connect(server).await.map(|_| ())
}

/// 断开并移除连接。
#[tauri::command]
pub async fn ssh_disconnect(state: State<'_, AppState>, name: String) -> Result<(), String> {
    state.ssh_pool.disconnect(&name).await;
    Ok(())
}

/// 连接状态查询。
#[tauri::command]
pub async fn ssh_status(state: State<'_, AppState>, name: String) -> Result<bool, String> {
    Ok(state.ssh_pool.is_connected(&name).await)
}

/// 手动执行远程命令（sh 模式：用户在服务器面板直接输入命令；用户本人
/// 操作不需要 HITL 确认——确认机制针对 agent 自主调用 remote_shell）。
#[tauri::command]
pub async fn ssh_exec(
    state: State<'_, AppState>,
    name: String,
    command: String,
) -> Result<ssh_tool::ExecResult, String> {
    let conn = state.ssh_pool.get(&name).await?;
    conn.exec(&command).await.map(|o| ssh_tool::ExecResult {
        exit_code: o.exit_code,
        output: o.text,
    })
}

/// 列远程目录（名称 + 类型 + 大小；目录在前）。
#[tauri::command]
pub async fn ssh_list_dir(
    state: State<'_, AppState>,
    name: String,
    path: String,
) -> Result<Vec<ssh_tool::DirEntry>, String> {
    let conn = state.ssh_pool.get(&name).await?;
    conn.list_dir(&path).await
}

/// 上传本地文件到远程（local_path 来自前端文件选择对话框）。
#[tauri::command]
pub async fn ssh_upload(
    state: State<'_, AppState>,
    name: String,
    local_path: String,
    remote_path: String,
) -> Result<u64, String> {
    let conn = state.ssh_pool.get(&name).await?;
    conn.upload(std::path::Path::new(&local_path), &remote_path)
        .await
}

/// 下载远程文件到本地。
#[tauri::command]
pub async fn ssh_download(
    state: State<'_, AppState>,
    name: String,
    remote_path: String,
    local_path: String,
) -> Result<u64, String> {
    let conn = state.ssh_pool.get(&name).await?;
    conn.download(&remote_path, std::path::Path::new(&local_path))
        .await
}

/// 删除远程文件（前端二次确认后调用）。
#[tauri::command]
pub async fn ssh_delete_file(
    state: State<'_, AppState>,
    name: String,
    remote_path: String,
) -> Result<(), String> {
    let conn = state.ssh_pool.get(&name).await?;
    conn.delete_file(&remote_path).await
}

// ---------- db execute（查询模式：前端确认后直接执行） ----------

/// 直接执行一条 SQL（查询模式）。AI 模式的确认由权限引擎 ask 规则兜底；
/// 查询模式的 DML 确认由前端弹窗承担（本命令不做二次拦截）。
#[tauri::command]
pub async fn db_execute(
    state: State<'_, AppState>,
    database: String,
    sql: String,
) -> Result<String, String> {
    let db = state.db_catalog.get(&database)?;
    db_exec::execute(&db, &sql).await
}

// ---------- workspace files（会话右侧文件列表） ----------

/// 文件条目：相对工作区根的路径（/ 分隔）+ 类型 + 大小。
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceFileEntry {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
}

/// 单层目录条目上限（懒加载：前端点开哪层列哪层，无深度限制）。
const DIR_LIST_LIMIT: usize = 2000;

/// 解析工作区文件根（与 build_agent 的锚定目录同一解析），不存在则创建。
fn resolve_ws_root(state: &AppState, workspace_id: &str) -> Result<std::path::PathBuf, String> {
    let config = state.snapshot();
    let ws = config
        .workspaces
        .iter()
        .find(|w| w.id == workspace_id)
        .ok_or_else(|| format!("workspace `{}` not found", workspace_id))?;
    let root = factory::resolve_workspace_root(&state.data_dir, ws);
    if !root.exists() {
        std::fs::create_dir_all(&root).map_err(|e| format!("create workspace dir failed: {e}"))?;
    }
    Ok(root)
}

/// 单层列目录：目录优先、名称升序；跳过隐藏条目（`.ox` 例外——数据规范
/// 目录要可见）与常见噪音目录；单层上限防大目录拖垮 IPC。
fn list_dir_entries(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<WorkspaceFileEntry>) {
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return, // 无权限/已消失的目录静默跳过
    };
    let mut entries: Vec<_> = read.filter_map(|e| e.ok()).collect();
    entries.sort_by(|a, b| {
        let da = a.path().is_dir();
        let db = b.path().is_dir();
        db.cmp(&da).then_with(|| a.file_name().cmp(&b.file_name()))
    });
    for e in entries.into_iter().take(DIR_LIST_LIMIT) {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') && name != ".ox" {
            continue;
        }
        let p = e.path();
        let is_dir = p.is_dir();
        if is_dir
            && matches!(
                name.as_str(),
                "node_modules" | "target" | "dist" | "__pycache__" | ".studio-members"
            )
        {
            continue;
        }
        let rel = p
            .strip_prefix(root)
            .unwrap_or(&p)
            .to_string_lossy()
            .replace('\\', "/");
        let size = if is_dir { 0 } else { e.metadata().map(|m| m.len()).unwrap_or(0) };
        out.push(WorkspaceFileEntry { path: rel, is_dir, size });
    }
}

/// 列工作区文件（懒加载）：`path` 为空列根层，否则列该目录的直接子项。
/// 深层目录由前端逐层请求——不再受递归深度/总量限制，大项目也不卡。
#[tauri::command]
pub fn list_workspace_files(
    state: State<'_, AppState>,
    workspace_id: String,
    path: Option<String>,
) -> Result<Vec<WorkspaceFileEntry>, String> {
    let root = resolve_ws_root(&state, &workspace_id)?;
    let canon_root = root.canonicalize().map_err(|e| format!("resolve root failed: {e}"))?;
    let target = match path.as_deref().map(str::trim) {
        None | Some("") => canon_root.clone(),
        Some(rel) => {
            let canon = root
                .join(rel)
                .canonicalize()
                .map_err(|e| format!("resolve path failed: {e}"))?;
            if !canon.starts_with(&canon_root) {
                return Err("path escapes workspace root".to_string());
            }
            if !canon.is_dir() {
                return Err("path is not a directory".to_string());
            }
            canon
        }
    };
    let mut out = Vec::new();
    list_dir_entries(&canon_root, &target, &mut out);
    Ok(out)
}

/// 读工作区文件文本（预览用；canonicalize 校验防目录穿越，512KB 上限）。
#[tauri::command]
pub fn read_workspace_file(
    state: State<'_, AppState>,
    workspace_id: String,
    path: String,
) -> Result<String, String> {
    let root = resolve_ws_root(&state, &workspace_id)?;
    if path.trim().is_empty() {
        return Err("path is empty".to_string());
    }
    let canon_root = root.canonicalize().map_err(|e| format!("resolve root failed: {e}"))?;
    let target = root.join(&path);
    let canon = target
        .canonicalize()
        .map_err(|e| format!("resolve path failed: {e}"))?;
    if !canon.starts_with(&canon_root) {
        return Err("path escapes workspace root".to_string());
    }
    if canon.is_dir() {
        return Err("path is a directory".to_string());
    }
    let meta = std::fs::metadata(&canon).map_err(|e| e.to_string())?;
    if meta.len() > 512 * 1024 {
        return Err("file too large to preview (>512KB)".to_string());
    }
    let bytes = std::fs::read(&canon).map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&bytes).to_string())
}

// ---------- agents ----------

#[tauri::command]
pub fn list_agents(state: State<'_, AppState>) -> Vec<AgentConfig> {
    state.snapshot().agents
}

/// 校验 team 成员引用完整性：成员必须是已存在的其他智能体（协同 = 多个
/// 智能体，lead 引用自己会造成自委派环）。
fn validate_members(config: &StudioConfig, ws: &WorkspaceConfig) -> Result<(), String> {
    for mid in &ws.members {
        if *mid == ws.agent_id {
            return Err("team 成员不能是 lead 智能体自己".to_string());
        }
        if !config.agents.iter().any(|a| &a.id == mid) {
            return Err(format!("team 成员 `{mid}` 不存在，请先在设置页创建该智能体"));
        }
    }
    Ok(())
}

#[tauri::command]
pub fn create_agent(state: State<'_, AppState>, mut agent: AgentConfig) -> Result<AgentConfig, String> {
    if agent.id.is_empty() {
        agent.id = uuid::Uuid::new_v4().simple().to_string();
    }
    if agent.created_at == 0 {
        agent.created_at = unix_now();
    }
    let mut config = state.snapshot();
    if config.agents.iter().any(|a| a.id == agent.id) {
        return Err(format!("agent `{}` already exists", agent.id));
    }
    config.agents.push(agent.clone());
    state.persist(&config)?;
    Ok(agent)
}

// ---------- workspaces（工作区 = 针对一个项目的工作环境） ----------

/// 校验工作区引用完整性：类型合法、智能体存在、team 成员存在。
fn validate_workspace(config: &StudioConfig, ws: &WorkspaceConfig) -> Result<(), String> {
    if !ws.kind_valid() {
        return Err(format!(
            "unknown workspace kind `{}` (expected one of {:?})",
            ws.kind, WORKSPACE_KINDS
        ));
    }
    if !config.agents.iter().any(|a| a.id == ws.agent_id) {
        return Err(format!("工作区引用的智能体 `{}` 不存在", ws.agent_id));
    }
    validate_members(config, ws)
}

/// 永久授权（「永久允许」）：跨会话的工具 allow 名单，存
/// `<workspace_dir>/permissions.json`，装配时由 factory 合并进权限上下文。
#[tauri::command]
pub fn save_permanent_permission(
    state: State<'_, AppState>,
    workspace_id: String,
    tool_name: String,
) -> Result<(), String> {
    factory::save_permanent_permission(&state.data_dir, &workspace_id, &tool_name)
}

/// 撤销永久授权（下次装配生效；已装配实例的会话内规则不受影响）。
#[tauri::command]
pub fn delete_permanent_permission(
    state: State<'_, AppState>,
    workspace_id: String,
    tool_name: String,
) -> Result<(), String> {
    factory::delete_permanent_permission(&state.data_dir, &workspace_id, &tool_name)
}

/// 列出永久授权工具（设置面板/调试用）。
#[tauri::command]
pub fn list_permanent_permissions(
    state: State<'_, AppState>,
    workspace_id: String,
) -> Result<Vec<String>, String> {
    Ok(factory::load_permanent_permissions(&state.data_dir, &workspace_id))
}

#[tauri::command]
pub fn list_workspaces(state: State<'_, AppState>) -> Result<Vec<WorkspaceConfig>, String> {
    Ok(state.snapshot().workspaces)
}

#[tauri::command]
pub fn create_workspace(
    state: State<'_, AppState>,
    mut workspace: WorkspaceConfig,
) -> Result<WorkspaceConfig, String> {
    if workspace.id.is_empty() {
        workspace.id = uuid::Uuid::new_v4().simple().to_string();
    }
    if workspace.created_at == 0 {
        workspace.created_at = unix_now();
    }
    let mut config = state.snapshot();
    if config.workspaces.iter().any(|w| w.id == workspace.id) {
        return Err(format!("workspace `{}` already exists", workspace.id));
    }
    validate_workspace(&config, &workspace)?;
    config.workspaces.push(workspace.clone());
    state.persist(&config)?;
    Ok(workspace)
}

#[tauri::command]
pub fn update_workspace(
    state: State<'_, AppState>,
    workspace: WorkspaceConfig,
) -> Result<(), String> {
    let mut config = state.snapshot();
    validate_workspace(&config, &workspace)?;
    let slot = config
        .workspaces
        .iter_mut()
        .find(|w| w.id == workspace.id)
        .ok_or_else(|| format!("unknown workspace `{}`", workspace.id))?;
    *slot = workspace;
    let id = slot.id.clone();
    state.persist(&config)?;
    state.invalidate(&id);
    Ok(())
}

#[tauri::command]
pub fn delete_workspace(state: State<'_, AppState>, workspace_id: String) -> Result<(), String> {
    let mut config = state.snapshot();
    config.workspaces.retain(|w| w.id != workspace_id);
    state.persist(&config)?;
    state.invalidate(&workspace_id);
    // 数据目录保留（会话状态不丢；彻底清除属破坏性操作，由用户手动做）。
    Ok(())
}

#[tauri::command]
pub fn update_agent(state: State<'_, AppState>, agent: AgentConfig) -> Result<(), String> {
    let mut config = state.snapshot();
    let slot = config
        .agents
        .iter_mut()
        .find(|a| a.id == agent.id)
        .ok_or_else(|| format!("unknown agent `{}`", agent.id))?;
    // 内置智能体同样全字段可编辑（预置默认值的普通智能体）；不可删除由
    // delete_agent 拒绝 + ensure_default_agent 兜底保证。
    *slot = agent;
    let id = slot.id.clone();
    state.persist(&config)?;
    // 智能体定义变了：引用它的所有工作区句柄全部失效重建。
    let affected: Vec<String> = config
        .workspaces
        .iter()
        .filter(|w| w.agent_id == id)
        .map(|w| w.id.clone())
        .collect();
    for wid in affected {
        state.invalidate(&wid);
    }
    Ok(())
}

#[tauri::command]
pub fn delete_agent(state: State<'_, AppState>, agent_id: String) -> Result<(), String> {
    if agent_id == crate::config::DEFAULT_AGENT_ID {
        // 内置 agent 不可删除（前端不渲染 Delete；这里是保护边界）。
        return Err("内置 Agent「全能助手」不可删除".to_string());
    }
    let mut config = state.snapshot();
    // 引用检查（对齐 delete_provider）：有工作区在用就不许删。
    let used_by: Vec<String> = config
        .workspaces
        .iter()
        .filter(|w| w.agent_id == agent_id)
        .map(|w| w.name.clone())
        .collect();
    if !used_by.is_empty() {
        return Err(format!(
            "智能体正被工作区使用：{}；请先删除或改绑对应工作区",
            used_by.join("、")
        ));
    }
    let member_of: Vec<String> = config
        .workspaces
        .iter()
        .filter(|w| w.members.iter().any(|m| m == &agent_id))
        .map(|w| w.name.clone())
        .collect();
    if !member_of.is_empty() {
        return Err(format!(
            "智能体是工作区 {} 的 team 成员；请先在工作区中移除该成员",
            member_of.join("、")
        ));
    }
    config.agents.retain(|a| a.id != agent_id);
    state.persist(&config)?;
    // 数据目录保留（彻底清除属破坏性操作，由用户手动做）。
    Ok(())
}

// ---------- turns ----------

/// 合并排队中的 steer 消息与本次输入（steer 文本在前——先说的先被看到）。
fn merge_with_pending(agent: &agentscope_core::ReActAgent, session_id: &str, message: &str) -> String {
    // 上次运行残留的 steer（循环退出后才到达、没被步边界消费的）合并进本次输入。
    // 与本次输入同文本的残留直接丢弃：前端 busy 防抖失效窗口内「sendMessage
    // 被拒 → 降级 steerTurn」会把同一条消息既当输入又留一份在队列里，合并
    // 会变成同一句话跑两轮（has_duplicate_steer 只防队列内重复，防不住这个
    // 跨通道组合）。
    let steers: Vec<Msg> = agent
        .take_steers(session_id)
        .into_iter()
        .filter(|m| message.is_empty() || m.text() != message)
        .collect();
    if steers.is_empty() {
        return message.to_string();
    }
    let mut parts: Vec<String> = steers.iter().map(|m| m.text()).collect();
    if !message.is_empty() {
        parts.push(message.to_string());
    }
    parts.join("\n\n")
}

/// 发送消息并开启一个 turn（≈ SessionTurnRunner.runTurnAsync）：
/// turn 互斥 → 合并排队 steer → 事件循环逐事件 emit → 结算（有未消费
/// steer 自动续轮，对齐 Java settleSession）→ 终态通知。
#[tauri::command]
#[allow(clippy::too_many_arguments)] // Tauri command 参数即 invoke 协议，不宜打包结构体
pub async fn send_message(
    app: AppHandle,
    state: State<'_, AppState>,
    workspace_id: String,
    mode: String,
    session_id: String,
    message: String,
    // 附件（图片 → Image 块，其他文件 → Data 块；base64 内联）。
    attachments: Option<Vec<AttachmentInput>>,
    // 权限确认结果（AgentScope HITL：对 REQUIRE_USER_CONFIRM 挂起调用的答复）。
    // 非空时本消息是「确认答复」：携带 CONFIRM_RESULTS metadata，不进聊天输入。
    confirm_results: Option<Vec<agentscope_core::event::ConfirmResult>>,
) -> Result<TurnInfo, String> {
    let handle = state.handle(&workspace_id, &mode)?;
    let session = handle.session(&session_id).await;
    // turn 互斥：guard 所有权 move 进事件循环 task，持锁到 turn 链结束。
    let turn_guard = session
        .try_begin_turn()
        .map_err(|_| "a turn is already running in this session".to_string())?;
    // 新链开始：清掉上一链可能残留的中断标志（interrupt 后链已死、标志
    // 未被收尾检查消费的场景），避免误杀本次 turn。
    session.clear_interrupted();

    let text = merge_with_pending(handle.agent.delegate(), &session_id, &message);
    // 消息构造：文本块 + 附件块（图片走 Image，其余走 Data；对齐 Java 多模态块）。
    let mut blocks: Vec<ContentBlock> = Vec::new();
    if !text.is_empty() {
        blocks.push(ContentBlock::Text { text });
    }
    for att in attachments.unwrap_or_default() {
        let is_image = att.media_type.starts_with("image/");
        let source = Source::Base64 {
            media_type: att.media_type,
            data: att.data_base64,
        };
        if is_image {
            blocks.push(ContentBlock::Image {
                source,
                min_pixels: None,
                max_pixels: None,
            });
        } else {
            blocks.push(ContentBlock::Data {
                source,
                name: Some(att.name),
            });
        }
    }
    // 确认答复：无聊天内容，metadata 携带确认结果（core 恢复时提取应用，
    // 且不重放成用户消息——对齐 Java METADATA_CONFIRM_RESULTS 语义）。
    let user_msg = if let Some(results) = confirm_results.filter(|r| !r.is_empty()) {
        Msg::new(Role::User, "user", Vec::new()).with_metadata(serde_json::json!({
            agentscope_core::message::metadata_keys::CONFIRM_RESULTS: results,
        }))
    } else {
        if blocks.is_empty() {
            return Err("message is empty".to_string());
        }
        Msg::new(Role::User, "user", blocks)
    };

    let turn_id = uuid::Uuid::new_v4().to_string();
    let agent = Arc::clone(handle.agent.delegate());
    // plan 状态在 turn 链开始前查询一次：busy 期间 UI 锁定 plan 切换，
    // 整条链（含续轮）的注入标志一致。
    let plan_active = handle.agent.is_plan_mode_active(Some(USER_ID), &session_id);

    tokio::spawn(turn_chain(
        app.clone(),
        agent,
        Arc::clone(&session),
        turn_guard,
        TurnChainCtx {
            workspace_id,
            session_id: session_id.clone(),
            plan_active,
        },
        turn_id.clone(),
        user_msg,
    ));
    // 执行 task 的句柄由 turn_chain 内部登记（track 的是 run task 而非本
    // turn_chain task），interrupt 才能真正停掉执行。

    Ok(TurnInfo {
        turn_id,
        session_id,
        status: "running",
    })
}

/// 前端上传的附件（base64 内联；大小限制由前端把守）。
#[derive(Debug, Clone, Deserialize)]
pub struct AttachmentInput {
    pub name: String,
    pub media_type: String,
    pub data_base64: String,
}

/// turn 链的静态上下文（收拢参数，避免过长签名）。
struct TurnChainCtx {
    workspace_id: String,
    session_id: String,
    /// plan 注入标志（busy 期间 UI 锁定 plan 切换，整条链一致）。
    plan_active: bool,
}

/// turn 链：跑一个 turn → 结算排队 steer → 有则自动续轮（新 turn_id），
/// 直到没有未消费 steer 或被中断（abort 杀掉整个 task，含续轮）。
///
/// 每个续轮开始时 emit `turn-started`（前端更新当前 turn 并保持 busy），
/// 每个 turn 结束时 emit `turn-finished`（链的最后一个 turn 携带最终终态）。
async fn turn_chain(
    app: AppHandle,
    agent: Arc<agentscope_core::ReActAgent>,
    session: Arc<SessionHandle>,
    mut turn_guard: tokio::sync::OwnedMutexGuard<()>,
    ctx: TurnChainCtx,
    mut turn_id: String,
    mut msg: Msg,
) {
    let TurnChainCtx {
        workspace_id,
        session_id,
        plan_active,
    } = ctx;
    loop {
        // plan 模式注入（对齐 Java system-reminder 投递思路：运行时约束以
        // system-reminder 消息形态进入上下文，而非改系统提示）。标志整条链
        // 一致（busy 期间 UI 锁定 plan 切换），作为前置 Text 块插入（不改
        // 用户原始块——附件消息的 Image/Data 块保持原位）。
        if plan_active {
            msg.content.insert(
                0,
                ContentBlock::Text {
                    text: "<system-reminder>\nPLAN MODE ACTIVE: you are in plan mode. Research and design \
                    only — do not execute changes. Write the plan to the plan file, then present it \
                    for review. Exit plan mode happens from the UI after user approval.\n</system-reminder>"
                        .to_string(),
                },
            );
        }

        let rc = RuntimeContext::new()
            .with_user_id(USER_ID)
            .with_session_id(&session_id);
        // 执行 task 句柄登记进 SessionHandle：interrupt abort 的是它（事件
        // 发送端 drop → 下面的 recv 循环自然退出），而不是本 turn_chain task
        // ——abort 本 task 会留下继续跑的执行 task（LLM/工具/状态写入全继续）。
        let mut rx = match agent
            .stream_events_with_context(rc, msg.clone())
            .await
        {
            Ok((rx, task)) => {
                session.track(task).await;
                rx
            }
            Err(e) => {
                let _ = app.emit(
                    "turn-finished",
                    TurnFinished {
                        workspace_id,
                        session_id,
                        turn_id,
                        outcome: "failed",
                        error: Some(format!("start stream: {e}")),
                    },
                );
                return;
            }
        };

        // delta 微批：TextBlockDelta/ThinkingBlockDelta 在 16ms 窗口内同块
        // 合并成一条再 emit——长输出时每秒数百条 delta 逐条 IPC + 前端逐条
        // setState 是主要渲染瓶颈；合并后 IPC 与前端重渲染都压到每帧一次。
        const DELTA_WINDOW: std::time::Duration = std::time::Duration::from_millis(16);
        let mut delta_buf: Option<(bool, String, String, String)> = None;
        let mut delta_deadline: Option<std::time::Instant> = None;
        macro_rules! flush_delta {
            () => {
                if let Some((is_text, reply_id, block_id, acc)) = delta_buf.take() {
                    let kind = if is_text {
                        agentscope_core::event::AgentEventKind::TextBlockDelta {
                            reply_id,
                            block_id,
                            delta: acc,
                        }
                    } else {
                        agentscope_core::event::AgentEventKind::ThinkingBlockDelta {
                            reply_id,
                            block_id,
                            delta: acc,
                        }
                    };
                    let payload = serde_json::to_value(&agentscope_core::event::AgentEvent::new(kind))
                        .unwrap_or(serde_json::Value::Null);
                    let envelope = EventEnvelope {
                        workspace_id: workspace_id.clone(),
                        session_id: session_id.clone(),
                        turn_id: turn_id.clone(),
                        event: payload,
                    };
                    let _ = app.emit("agent-event", envelope);
                }
            };
        }
        loop {
            // 窗口到点：先 flush 再继续收（保证前端每 16ms 至少看到一次更新）。
            let wait = match delta_deadline.take() {
                Some(d) if delta_buf.is_some() => {
                    let now = std::time::Instant::now();
                    if now >= d {
                        flush_delta!();
                        None
                    } else {
                        delta_deadline = Some(d);
                        Some(d - now)
                    }
                }
                _ => None,
            };
            // rx 是 tauri::async_runtime::Receiver（无 recv_timeout），
            // 用 tokio::time::timeout 包 recv() 实现窗口等待。
            let recv_fut = rx.recv();
            let event = match wait {
                Some(d) => match tokio::time::timeout(d, recv_fut).await {
                    Ok(Some(ev)) => ev,
                    Ok(None) => {
                        flush_delta!();
                        break;
                    }
                    Err(_) => continue, // 窗口到点（上面已 flush）
                },
                None => match recv_fut.await {
                    Some(ev) => ev,
                    None => {
                        flush_delta!();
                        break;
                    }
                },
            };
            let delta_info = match &event.kind {
                agentscope_core::event::AgentEventKind::TextBlockDelta { reply_id, block_id, delta } => {
                    Some((true, reply_id.clone(), block_id.clone(), delta.clone()))
                }
                agentscope_core::event::AgentEventKind::ThinkingBlockDelta { reply_id, block_id, delta } => {
                    Some((false, reply_id.clone(), block_id.clone(), delta.clone()))
                }
                _ => None,
            };
            match delta_info {
                Some((is_text, reply_id, block_id, delta)) => {
                    match &mut delta_buf {
                        Some((t, r, b, acc)) if *t == is_text && *r == reply_id && *b == block_id => {
                            acc.push_str(&delta);
                        }
                        _ => {
                            flush_delta!();
                            delta_buf = Some((is_text, reply_id, block_id, delta));
                        }
                    }
                    delta_deadline.get_or_insert(std::time::Instant::now() + DELTA_WINDOW);
                }
                None => {
                    flush_delta!();
                    // 后台完成/等授权系统通知：仅主窗口非聚焦时发（聚焦时用户就在看，
                    // 不打扰；keep-alive 多工作区场景靠它感知后台进度）。
                    notify_if_background(&app, &event.kind);
                    let payload = serde_json::to_value(&event).unwrap_or(serde_json::Value::Null);
                    let envelope = EventEnvelope {
                        workspace_id: workspace_id.clone(),
                        session_id: session_id.clone(),
                        turn_id: turn_id.clone(),
                        event: payload,
                    };
                    let _ = app.emit("agent-event", envelope);
                }
            }
        }
        session.untrack().await;

        // 被中断：emit interrupted 终态后直接返回，不结算、不续轮。
        // （中断可能发生在 recv 中——执行 task 被 abort；也可能发生在续轮
        // 间隙——标志已置位，这里统一拦住。interrupt_turn 命令不再补发。）
        if session.take_interrupted() {
            let _ = app.emit(
                "turn-finished",
                TurnFinished {
                    workspace_id,
                    session_id,
                    turn_id,
                    outcome: "interrupted",
                    error: None,
                },
            );
            return;
        }

        // 结算：还有未消费的 steer（循环退出后才到达的）→ 自动续轮
        // （对齐 Java settleSession 语义；步边界已消费的不在这里）。
        let pending = agent.take_steers(&session_id);
        drop(turn_guard);
        let next_text = pending
            .iter()
            .map(|m| m.text())
            .collect::<Vec<_>>()
            .join("\n\n");
        if next_text.is_empty() {
            let _ = app.emit(
                "turn-finished",
                TurnFinished {
                    workspace_id,
                    session_id,
                    turn_id,
                    outcome: "completed",
                    error: None,
                },
            );
            return;
        }

        // 续轮：重新拿 turn 锁（同一 task 串行，理论必成功；拿不到就按完成收尾）。
        turn_guard = match session.try_begin_turn() {
            Ok(guard) => guard,
            Err(_) => {
                let _ = app.emit(
                    "turn-finished",
                    TurnFinished {
                        workspace_id,
                        session_id,
                        turn_id,
                        outcome: "completed",
                        error: None,
                    },
                );
                return;
            }
        };
        // 上一 turn 正常收尾；新 turn 开启通知（前端保持 busy 并更新 turn）。
        let _ = app.emit(
            "turn-finished",
            TurnFinished {
                workspace_id: workspace_id.clone(),
                session_id: session_id.clone(),
                turn_id: turn_id.clone(),
                outcome: "completed",
                error: None,
            },
        );
        turn_id = uuid::Uuid::new_v4().to_string();
        msg = Msg::user("user", next_text);
        let _ = app.emit(
            "turn-started",
            TurnStarted {
                workspace_id: workspace_id.clone(),
                session_id: session_id.clone(),
                turn_id: turn_id.clone(),
                reason: "steer",
            },
        );
    }
}

/// 续轮开始通知（steer 驱动；前端更新当前 turn 并保持 busy）。
#[derive(Debug, Clone, Serialize)]
struct TurnStarted {
    workspace_id: String,
    session_id: String,
    turn_id: String,
    reason: &'static str,
}

/// 运行中插入一条转向指令（steer，≈ AgentSession.steer）。
///
/// 直接进 agent 的步边界注入通道：当前 turn 的**下一个 step** 生效
/// （对齐 Java beforeStep 语义）；若调用已结束则留在队列，由
/// `send_message` 合并或 `turn_chain` 结算续轮消费（见各自注释）。
#[tauri::command]
pub async fn steer_turn(
    state: State<'_, AppState>,
    workspace_id: String,
    mode: String,
    session_id: String,
    message: String,
    // 插入消息可携带附件（图片 → Image 块，其他文件 → Data 块），与 send_message 同构。
    attachments: Option<Vec<AttachmentInput>>,
) -> Result<bool, String> {
    if message.trim().is_empty() && attachments.as_deref().unwrap_or(&[]).is_empty() {
        return Err("steer message is empty".to_string());
    }
    let handle = state.handle(&workspace_id, &mode)?;
    let session = handle.session(&session_id).await;
    // 守卫：无运行中 turn 时拒绝入队——否则消息会残留到队列里，被
    // merge_with_pending 混进用户下一条输入、或在后续 turn 结算时触发
    // 「莫名其妙」的自动续轮（busy 状态失真窗口的兜底）。
    if !session.is_running().await {
        return Err("no turn is running in this session".to_string());
    }
    // 消息构造：文本块 + 附件块（对齐 send_message：图片走 Image，其余走 Data）。
    let mut blocks: Vec<ContentBlock> = Vec::new();
    if !message.trim().is_empty() {
        blocks.push(ContentBlock::Text {
            text: message.clone(),
        });
    }
    for att in attachments.unwrap_or_default() {
        let is_image = att.media_type.starts_with("image/");
        let source = Source::Base64 {
            media_type: att.media_type,
            data: att.data_base64,
        };
        if is_image {
            blocks.push(ContentBlock::Image {
                source,
                min_pixels: None,
                max_pixels: None,
            });
        } else {
            blocks.push(ContentBlock::Data {
                source,
                name: Some(att.name),
            });
        }
    }
    // 同一条 steer 重复入队没有价值：IME 连击等极端竞态下前端防重可能
    // 失效，模型会把重复指令当两轮各处理一遍。队列已有相同内容时静默
    // 忽略（对前端表现为成功，条目只显示一条、模型只处理一遍）。
    // 纯附件（无文本）不去重：空串文本会让两条不同附件的插入被误判重复。
    let delegate = handle.agent.delegate();
    let text_key = message.trim().to_string();
    if text_key.is_empty() || !delegate.has_duplicate_steer(&session_id, &text_key) {
        delegate.push_steer(&session_id, Msg::new(Role::User, "user", blocks));
    }
    Ok(true)
}

/// 中断运行中的 turn（≈ SessionTurnRunner.interrupt）。
///
/// abort 的是 agent 执行 task（run()）：事件发送端 drop → turn_chain 的事件
/// 循环自然退出，由它统一 emit `turn-finished(interrupted)` 并跳过结算/续轮
/// （中断标志在 SessionHandle，续轮间隙的中断同样被拦住）。
#[tauri::command]
pub async fn interrupt_turn(
    _app: AppHandle,
    state: State<'_, AppState>,
    workspace_id: String,
    mode: String,
    session_id: String,
) -> Result<bool, String> {
    let handle = state.handle(&workspace_id, &mode)?;
    let session = handle.session(&session_id).await;
    let task = session.interrupt().await;
    // 中断后丢弃未消费的 steer：用户已主动终止，残留的插入消息
    // 不应混进后续输入或触发自动续轮。
    handle.agent.delegate().clear_steers(&session_id);
    let had = task.is_some();
    if let Some(task) = task {
        task.abort();
    }
    Ok(had)
}

// ---------- plan mode ----------

#[tauri::command]
pub async fn set_plan_mode(
    state: State<'_, AppState>,
    workspace_id: String,
    mode: String,
    session_id: String,
    active: bool,
) -> Result<PlanStatus, String> {
    let handle = state.handle(&workspace_id, &mode)?;
    if active {
        let plan_file = handle
            .agent
            .enter_plan_mode(Some(USER_ID), &session_id)
            .map_err(|e| e.to_string())?;
        Ok(PlanStatus {
            active: true,
            plan_file: Some(plan_file),
        })
    } else {
        handle
            .agent
            .exit_plan_mode(Some(USER_ID), &session_id)
            .map_err(|e| e.to_string())?;
        Ok(PlanStatus {
            active: false,
            plan_file: handle
                .agent
                .plan_file_path(Some(USER_ID), &session_id),
        })
    }
}

#[tauri::command]
pub async fn get_plan_status(
    state: State<'_, AppState>,
    workspace_id: String,
    mode: String,
    session_id: String,
) -> Result<PlanStatus, String> {
    let handle = state.handle(&workspace_id, &mode)?;
    Ok(PlanStatus {
        active: handle
            .agent
            .is_plan_mode_active(Some(USER_ID), &session_id),
        plan_file: handle.agent.plan_file_path(Some(USER_ID), &session_id),
    })
}

/// 后台事件系统通知：主窗口非聚焦时发（任务完成 / 等待授权）。
/// 通知点击不导航（Tauri 通知无默认点击行为），仅提醒用户回来看。
fn notify_if_background(
    app: &AppHandle,
    kind: &agentscope_core::event::AgentEventKind,
) {
    use tauri::Manager;
    use tauri_plugin_notification::NotificationExt;
    let focused = app
        .get_webview_window("main")
        .and_then(|w| w.is_focused().ok())
        .unwrap_or(true);
    if focused {
        return;
    }
    let (title, body) = match kind {
        agentscope_core::event::AgentEventKind::AgentEnd { .. } => {
            ("任务完成", "智能体已完成本轮任务")
        }
        agentscope_core::event::AgentEventKind::RequireUserConfirm { .. } => {
            ("等待授权", "有工具调用需要你确认后才能继续")
        }
        _ => return,
    };
    let _ = app
        .notification()
        .builder()
        .title(title)
        .body(body)
        .show();
}

/// 会话导出写盘：前端经系统保存对话框拿到路径后调用（用户主动选择的
/// 路径，写它即用户意图）。独立 command 免装 fs 插件全家桶。
#[tauri::command]
pub async fn write_export_file(path: String, contents: String) -> Result<(), String> {
    // 只允许写 .md（导出语义），防被滥用成任意文件写。
    if !path.to_lowercase().ends_with(".md") {
        return Err("export only supports .md files".into());
    }
    tokio::fs::write(&path, contents.as_bytes())
        .await
        .map_err(|e| format!("write export file: {e}"))
}

// ---------- sessions ----------

/// 会话与连接的绑定（db 工作区绑数据库、ops 工作区绑服务器）。
/// 存 `<session-dir>/binding.json`，面板跟随会话切换显示对应连接。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionBinding {
    /// db 工作区：该会话绑定的数据库档案名。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// ops 工作区：该会话绑定的服务器档案名。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
}

/// 会话目录（校验 session_id 防路径穿越；与 list_sessions 的目录布局一致）。
fn session_binding_path(
    state: &State<'_, AppState>,
    workspace_id: &str,
    session_id: &str,
) -> Result<std::path::PathBuf, String> {
    if session_id.is_empty()
        || !session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("invalid session id".to_string());
    }
    Ok(factory::workspace_dir(&state.data_dir, workspace_id)
        .join("state")
        .join(USER_ID)
        .join(session_id)
        .join("binding.json"))
}

#[tauri::command]
pub fn get_session_binding(
    state: State<'_, AppState>,
    workspace_id: String,
    session_id: String,
) -> SessionBinding {
    session_binding_path(&state, &workspace_id, &session_id)
        .ok()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// 绑定/解绑（kind = "database" | "server"；name = None 解除该类绑定）。
#[tauri::command]
pub fn set_session_binding(
    state: State<'_, AppState>,
    workspace_id: String,
    session_id: String,
    kind: String,
    name: Option<String>,
) -> Result<(), String> {
    let path = session_binding_path(&state, &workspace_id, &session_id)?;
    let mut binding: SessionBinding = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    match kind.as_str() {
        "database" => binding.database = name.filter(|n| !n.trim().is_empty()),
        "server" => binding.server = name.filter(|n| !n.trim().is_empty()),
        other => return Err(format!("unknown binding kind `{other}`")),
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let bytes = serde_json::to_vec_pretty(&binding).map_err(|e| e.to_string())?;
    std::fs::write(&path, bytes).map_err(|e| e.to_string())
}

/// 列出某 agent 的已有会话（读 JsonFile 状态库目录布局
/// `<state-root>/<user>/<session>/agent_state.json`；对齐 service 的会话发现）。
#[tauri::command]
pub fn list_sessions(
    state: State<'_, AppState>,
    workspace_id: String,
) -> Result<Vec<SessionSummary>, String> {
    // JsonFileAgentStateStore 布局：<root>/<safe(user)>/<safe(session)>/；
    // USER_ID="studio" 与前端生成的 uuid（hex）都是安全命名，不触发转义。
    let user_dir = factory::workspace_dir(&state.data_dir, &workspace_id)
        .join("state")
        .join(USER_ID);
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&user_dir) {
        Ok(entries) => entries,
        Err(_) => return Ok(out), // 还没有任何会话
    };
    // 子会话归属：父会话 agent_state.json 的 tool_context.spawn_registry
    // 记录了它派生的子会话 id——先收集 child→parent 映射，再填进摘要。
    let mut parent_of: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let parent_id = entry.file_name().to_string_lossy().to_string();
        let state_path = entry.path().join("agent_state.json");
        let registry = std::fs::read(&state_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|v| {
                v.get("tool_context")
                    .and_then(|t| t.get("spawn_registry"))
                    .and_then(|r| r.as_object())
                    .cloned()
            });
        if let Some(registry) = registry {
            for (_k, sp) in registry {
                if let Some(child) = sp.get("session_id").and_then(|s| s.as_str()) {
                    parent_of.entry(child.to_string()).or_insert_with(|| parent_id.clone());
                }
            }
        }
    }
    let entries = std::fs::read_dir(&user_dir);
    let entries = match entries {
        Ok(entries) => entries,
        Err(_) => return Ok(out),
    };
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let session_id = entry.file_name().to_string_lossy().to_string();
        // 成员会话残留过滤：team 委派的成员会话是固定 id（team-<成员名>），
        // 现已落 studio-member 独立域；历史版本留在 studio 域的（team- 前缀
        // 但后缀既非 12 位 hex 也非 main）不是用户会话，温和隐藏不列出。
        if is_member_session_id(&session_id) {
            continue;
        }
        let state_path = entry.path().join("agent_state.json");
        let updated_at = state_path
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let state_json = std::fs::read(&state_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
        let messages = state_json
            .as_ref()
            .and_then(|v| {
                v.get("context")
                    .and_then(|c| c.as_array())
                    .map(|a| a.len())
            })
            .unwrap_or(0);
        // 会话名：自定义 title.txt 优先；否则自动取首条用户消息摘要
        // （让列表直接可读「这个 session 做了什么」）。
        let title = std::fs::read_to_string(entry.path().join("title.txt"))
            .ok()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .or_else(|| state_json.as_ref().and_then(first_user_text));
        out.push(SessionSummary {
            title,
            parent_session_id: parent_of.get(&session_id).cloned(),
            session_id,
            messages,
            updated_at,
        });
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
    Ok(out)
}

#[tauri::command]
pub async fn get_history(
    state: State<'_, AppState>,
    workspace_id: String,
    mode: String,
    session_id: String,
) -> Result<Vec<HistoryItem>, String> {
    let handle = state.handle(&workspace_id, &mode)?;
    // AgentState.context 是唯一对话真相（对齐 Java contextMutable 语义）：
    // 历史视图直接从状态读，重启后依然完整。
    let agent_state = handle
        .agent
        .delegate()
        .agent_state_for(Some(USER_ID), &session_id)
        .map_err(|e| e.to_string())?;
    // 压缩摘要消息（compaction 注入给模型的上下文衔接指令）不进历史视图：
    // 它是内部实现细节，整段英文+summary 全文对用户是噪音。
    Ok(agent_state
        .context
        .iter()
        .filter(|msg| msg.name != agentscope_harness::memory::COMPACTION_SUMMARY_MSG_NAME)
        .map(|msg| HistoryItem {
            role: format!("{:?}", msg.role).to_lowercase(),
            text: msg.text(),
            timestamp: msg.timestamp.clone(),
            blocks: msg
                .content
                .iter()
                .map(|block| serde_json::to_value(block).unwrap_or(serde_json::Value::Null))
                .collect(),
        })
        .collect())
}

/// 清空会话上下文（管理性变更；对齐 AgentSession 的 clear 语义）。
#[tauri::command]
pub async fn clear_session(
    state: State<'_, AppState>,
    workspace_id: String,
    mode: String,
    session_id: String,
) -> Result<(), String> {
    let handle = state.handle(&workspace_id, &mode)?;
    // 运行中的 turn 先停掉（abort 执行 task 并等死透），否则清空的上下文
    // 会被继续跑的 turn 重新写回。
    stop_session_turn(&handle, &session_id).await;
    handle
        .agent
        .delegate()
        .clear_context_for(Some(USER_ID), &session_id)
        .map_err(|e| e.to_string())?;
    handle.agent.delegate().clear_steers(&session_id);
    // plan 标志随会话状态持久化——清空会话即全新开始，plan_active 残留
    // 会让新对话一上来就被 PlanModeMiddleware 拦截写操作（幂等，未激活时无副作用）。
    let _ = handle.agent.exit_plan_mode(Some(USER_ID), &session_id);
    handle.drop_session(&session_id).await;
    Ok(())
}

/// 停掉一个会话的运行中 turn：abort 执行 task 并等它死透（删除/清空会话
/// 前调用，避免与后续的目录/状态操作竞态）。
async fn stop_session_turn(handle: &Arc<AgentHandle>, session_id: &str) {
    let session = handle.session(session_id).await;
    if let Some(task) = session.interrupt().await {
        task.abort();
        let _ = task.await;
    }
}

/// 从 agent_state.json 提取首条用户消息文本摘要（≤40 字符）。
/// 宽容解析：role 序列化为 lowercase（`user`），文本块为
/// `{"type":"text","text":...}`（ContentBlock 的 serde tag）。
fn first_user_text(state_json: &serde_json::Value) -> Option<String> {
    let ctx = state_json.get("context")?.as_array()?;
    for msg in ctx {
        if msg.get("role").and_then(|r| r.as_str()) != Some("user") {
            continue;
        }
        let blocks = match msg.get("content").and_then(|c| c.as_array()) {
            Some(b) => b,
            None => continue,
        };
        for block in blocks {
            if block.get("type").and_then(|t| t.as_str()) == Some("text")
                && let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    let text = text.trim();
                    if !text.is_empty() {
                        let summary: String = text.chars().take(40).collect();
                        return Some(summary);
                    }
                }
        }
    }
    None
}

/// 会话目录（布局对齐 JsonFileAgentStateStore：`<state-root>/<user>/<session>/`）。
/// session_id 由前端生成（`<mode>-<uuid hex>`），这里按 safe_segment 的安全
/// 字符集校验，防路径拼接越界（core 的 safe_segment 是私有函数，本地复刻规则）。
fn session_dir(state: &AppState, workspace_id: &str, session_id: &str) -> Result<std::path::PathBuf, String> {
    let valid = !session_id.is_empty()
        && session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if !valid {
        return Err(format!("invalid session id: `{session_id}`"));
    }
    Ok(factory::workspace_dir(&state.data_dir, workspace_id)
        .join("state")
        .join(USER_ID)
        .join(session_id))
}

/// 重命名会话（自定义名；空串 = 清除自定义名，回到自动摘要）。
#[tauri::command]
pub fn rename_session(
    state: State<'_, AppState>,
    workspace_id: String,
    session_id: String,
    title: String,
) -> Result<(), String> {
    let dir = session_dir(&state, &workspace_id, &session_id)?;
    let path = dir.join("title.txt");
    let title = title.trim();
    if title.is_empty() {
        let _ = std::fs::remove_file(&path);
    } else {
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        std::fs::write(&path, title).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// 删除会话：运行中的 turn 先中止，清内存运行态，再删整个状态目录
/// （agent_state.json + title.txt；不可逆，前端负责确认）。
#[tauri::command]
pub async fn delete_session(
    state: State<'_, AppState>,
    workspace_id: String,
    mode: String,
    session_id: String,
) -> Result<(), String> {
    // mode 参数保留（前端 invoke 签名兼容）；清理范围已覆盖所有 mode。
    let _ = &mode;
    // 跨 mode 清理：solo/team 是两个 handle 实例（缓存 key 含 mode），
    // 只清当前 mode 的话，另一 mode 内存里的运行态还在——若该会话正在
    // 那边跑 turn，结束后会重新落盘，表现为「删了又复活」。
    let prefix = format!("{workspace_id}:");
    let affected: Vec<Arc<AgentHandle>> = state
        .handles
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|(k, _)| k.starts_with(&prefix))
        .map(|(_, h)| Arc::clone(h))
        .collect();
    for handle in &affected {
        // 先停 turn（abort 执行 task 并等死透）：否则继续跑的 turn 会把
        // 状态重新落盘（「删了又复活」），事件也会继续涌向已删除的会话。
        stop_session_turn(handle, &session_id).await;
        handle.agent.delegate().clear_steers(&session_id);
        handle.drop_session(&session_id).await;
    }
    let dir = session_dir(&state, &workspace_id, &session_id)?;
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// 运行中 turn 的描述（前端刷新后恢复 busy/running 状态用）。
#[derive(Debug, Clone, Serialize)]
pub struct RunningTurn {
    pub workspace_id: String,
    pub mode: String,
    pub session_id: String,
}

/// 列出所有运行中的 turn。事件监听是全局的，页面刷新丢的只是内存里的
/// busy/running 状态——前端加载时按此恢复，运行中的 turn 无需重启。
#[tauri::command]
pub async fn list_running_turns(state: State<'_, AppState>) -> Result<Vec<RunningTurn>, String> {
    let handles: Vec<(String, Arc<AgentHandle>)> = state
        .handles
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(k, h)| (k.clone(), Arc::clone(h)))
        .collect();
    let mut out = Vec::new();
    for (key, handle) in handles {
        // 缓存 key = "{workspace_id}:{mode}"（见 AppState::handle）。
        let Some((workspace_id, mode)) = key.rsplit_once(':') else {
            continue;
        };
        for session_id in handle.running_session_ids().await {
            out.push(RunningTurn {
                workspace_id: workspace_id.to_string(),
                mode: mode.to_string(),
                session_id,
            });
        }
    }
    Ok(out)
}

/// 成员会话 id 判定：`team-<成员名>`（成员名 sanitize 后非 12 位 hex、
/// 非 `main`）。用户会话是 `team-main` / `team-<12hex>` / `solo-*` / `main`。
fn is_member_session_id(session_id: &str) -> bool {
    let Some(suffix) = session_id.strip_prefix("team-") else {
        return false;
    };
    if suffix == "main" {
        return false;
    }
    suffix.len() != 12 || !suffix.chars().all(|c| c.is_ascii_hexdigit())
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------- app setup ----------

/// 旧版数据一次性迁移：AppData 下的 workspaces/ 与 studio-config.json
/// 搬到 ~/.ox（仅当新位置还没有数据时执行，幂等可重入）。
fn migrate_legacy_data(legacy: &std::path::Path, data_dir: &std::path::Path) {
    if legacy == data_dir || !legacy.exists() {
        return;
    }
    let new_workspaces = data_dir.join("workspaces");
    if !new_workspaces.exists() {
        let old_workspaces = legacy.join("workspaces");
        if old_workspaces.exists() && copy_dir_all(&old_workspaces, &new_workspaces).is_ok() {
            eprintln!("[.ox] migrated workspaces data to {}", data_dir.display());
        }
    }
    let new_config = data_dir.join("config").join("studio-config.json");
    if !new_config.exists() {
        for old_cfg in [
            legacy.join("config").join("studio-config.json"),
            legacy.join("studio-config.json"),
        ] {
            if old_cfg.exists() {
                if std::fs::copy(&old_cfg, &new_config).is_ok() {
                    eprintln!("[.ox] migrated studio-config.json to {}", new_config.display());
                }
                break;
            }
        }
    }
}

/// 递归复制目录（std 无此 API；迁移专用，失败不阻塞启动）。
fn copy_dir_all(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let target = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

// ===========================================================================
// .ox 数据管理（用户面板）：共享黑板条目删改 + 记忆文件管理
// ===========================================================================
//
// 黑板：模型侧只增不改（blackboard_append/read 工具），用户侧经本组命令
// 删改——同一 JSON 文件（<工作区根>/.ox/blackboard.json），进程级全局锁
// 串行化（见 harness blackboard 模块）。
// 记忆：MEMORY.md 与 memory/YYYY-MM-DD.md 是 markdown 台账（无条目结构），
// 面板做文件级管理（列出/读取/保存/删除），不做条目级拆分。

use agentscope_harness::blackboard::BlackboardStore;

/// 工作区黑板存储（与 factory 注册给模型的工具同一路径）。
fn blackboard_store(state: &AppState, workspace_id: &str) -> Result<BlackboardStore, String> {
    let root = resolve_ws_root(state, workspace_id)?;
    Ok(BlackboardStore::new(root.join(".ox").join("blackboard.json")))
}

/// 校验记忆文件相对路径：仅允许 MEMORY.md 与 memory/*.md（防目录穿越）。
fn validate_memory_rel_path(rel: &str) -> Result<String, String> {
    let rel = rel.trim().replace('\\', "/");
    if rel == "MEMORY.md" {
        return Ok(rel);
    }
    let ok = rel.starts_with("memory/")
        && !rel.contains("..")
        && !rel.contains("//")
        && rel.ends_with(".md")
        && !rel["memory/".len()..].contains('/');
    if ok {
        Ok(rel)
    } else {
        Err("memory path must be MEMORY.md or memory/<name>.md".to_string())
    }
}

/// 记忆工作区根（与 factory::build_memory_workspace 同一约定：<工作区根>/.ox）。
fn memory_root(state: &AppState, workspace_id: &str) -> Result<std::path::PathBuf, String> {
    let root = resolve_ws_root(state, workspace_id)?;
    let dir = root.join(".ox");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create .ox dir failed: {e}"))?;
    Ok(dir)
}

#[derive(Clone, serde::Serialize)]
pub struct BlackboardEntryDto {
    pub id: u64,
    pub ts: i64,
    #[serde(rename = "type")]
    pub kind: String,
    pub author: String,
    pub content: String,
}

fn to_dto(e: agentscope_harness::blackboard::BlackboardEntry) -> BlackboardEntryDto {
    BlackboardEntryDto { id: e.id, ts: e.ts, kind: e.kind, author: e.author, content: e.content }
}

/// 列出黑板条目（最新在前；limit 空 = 全量）。
#[tauri::command]
pub fn blackboard_list(
    state: State<'_, AppState>,
    workspace_id: String,
    limit: Option<i64>,
) -> Result<Vec<BlackboardEntryDto>, String> {
    let store = blackboard_store(&state, &workspace_id)?;
    let limit = limit.filter(|l| *l > 0).map(|l| l as usize);
    Ok(store.list(limit)?.into_iter().map(to_dto).collect())
}

/// 删除黑板条目（用户面板操作；模型侧无此能力）。
#[tauri::command]
pub fn blackboard_delete(
    state: State<'_, AppState>,
    workspace_id: String,
    id: u64,
) -> Result<bool, String> {
    blackboard_store(&state, &workspace_id)?.delete(id)
}

/// 修改黑板条目（kind/content 传 null = 保持不变）。
#[tauri::command]
pub fn blackboard_update(
    state: State<'_, AppState>,
    workspace_id: String,
    id: u64,
    kind: Option<String>,
    content: Option<String>,
) -> Result<BlackboardEntryDto, String> {
    Ok(to_dto(blackboard_store(&state, &workspace_id)?.update(id, kind.as_deref(), content.as_deref())?))
}

#[derive(Clone, serde::Serialize)]
pub struct MemoryFileEntry {
    pub path: String,
    pub size: u64,
    pub modified_ms: i64,
}

/// 列出记忆文件（MEMORY.md + memory/*.md；不存在返回空表）。
#[tauri::command]
pub fn memory_list_files(
    state: State<'_, AppState>,
    workspace_id: String,
) -> Result<Vec<MemoryFileEntry>, String> {
    let root = memory_root(&state, &workspace_id)?;
    let mut out: Vec<MemoryFileEntry> = Vec::new();
    let mut push = |rel: &str, abs: std::path::PathBuf| {
        if let Ok(meta) = std::fs::metadata(&abs) {
            if meta.is_file() {
                out.push(MemoryFileEntry {
                    path: rel.to_string(),
                    size: meta.len(),
                    modified_ms: meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0),
                });
            }
        }
    };
    push("MEMORY.md", root.join("MEMORY.md"));
    let daily = root.join("memory");
    if let Ok(read) = std::fs::read_dir(&daily) {
        let mut names: Vec<String> = read
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
            .filter(|n| n.ends_with(".md"))
            .collect();
        names.sort();
        for n in names {
            push(&format!("memory/{n}"), daily.join(&n));
        }
    }
    Ok(out)
}

/// 读记忆文件文本（路径白名单校验；512KB 上限）。
#[tauri::command]
pub fn memory_read_file(
    state: State<'_, AppState>,
    workspace_id: String,
    path: String,
) -> Result<String, String> {
    let rel = validate_memory_rel_path(&path)?;
    let abs = memory_root(&state, &workspace_id)?.join(&rel);
    let meta = std::fs::metadata(&abs).map_err(|e| format!("stat failed: {e}"))?;
    if meta.len() > 512 * 1024 {
        return Err("file too large (>512KB)".to_string());
    }
    std::fs::read_to_string(&abs).map_err(|e| format!("read failed: {e}"))
}

/// 保存记忆文件文本（编辑用；父目录自动创建）。
#[tauri::command]
pub fn memory_save_file(
    state: State<'_, AppState>,
    workspace_id: String,
    path: String,
    content: String,
) -> Result<(), String> {
    let rel = validate_memory_rel_path(&path)?;
    let abs = memory_root(&state, &workspace_id)?.join(&rel);
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create dir failed: {e}"))?;
    }
    std::fs::write(&abs, content).map_err(|e| format!("write failed: {e}"))
}

/// 删除记忆文件（MEMORY.md 删除后下次 memory_save 会重建）。
#[tauri::command]
pub fn memory_delete_file(
    state: State<'_, AppState>,
    workspace_id: String,
    path: String,
) -> Result<(), String> {
    let rel = validate_memory_rel_path(&path)?;
    let abs = memory_root(&state, &workspace_id)?.join(&rel);
    match std::fs::remove_file(&abs) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("delete failed: {e}")),
    }
}

/// Builder 装配（lib.rs 调用）。
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .setup(|app| {
            use tauri::Manager;
            // .ox 数据规范：应用数据统一放 ~/.ox/（会话历史、工作区状态、
            // 配置都在 .ox 下），不再散落在系统 AppData 目录。
            let data_dir = match app.path().home_dir() {
                Ok(home) => home.join(".ox"),
                Err(_) => std::env::temp_dir().join(".ox"),
            };
            std::fs::create_dir_all(&data_dir).ok();
            // 一次性迁移：旧版数据在 Tauri AppData 目录下，搬进 ~/.ox。
            if let Ok(legacy) = app.path().app_data_dir() {
                migrate_legacy_data(&legacy, &data_dir);
            }
            let config_dir = data_dir.join("config");
            let config_store = ConfigStore::new(&config_dir);
            let config = config_store.load_with_data_dir(Some(&data_dir));
            let db_catalog = Arc::new(DbCatalog::new());
            db_catalog.replace_all(&config.databases);
            app.manage(AppState {
                config_store,
                config: Mutex::new(config),
                handles: Mutex::new(HashMap::new()),
                data_dir,
                ssh_pool: Arc::new(SshPool::new()),
                db_catalog,
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_providers,
            save_provider,
            delete_provider,
            list_databases,
            save_database,
            delete_database,
            list_database_kinds,
            probe_database_cli,
            get_session_binding,
            set_session_binding,
            list_servers,
            save_server,
            delete_server,
            ssh_connect,
            ssh_disconnect,
            ssh_exec,
            ssh_status,
            ssh_list_dir,
            ssh_upload,
            ssh_download,
            ssh_delete_file,
            db_execute,
            list_workspace_files,
            read_workspace_file,
            list_agents,
            create_agent,
            update_agent,
            delete_agent,
            list_workspaces,
            save_permanent_permission,
            delete_permanent_permission,
            list_permanent_permissions,
            create_workspace,
            update_workspace,
            delete_workspace,
            send_message,
            steer_turn,
            interrupt_turn,
            set_plan_mode,
            get_plan_status,
            list_sessions,
            get_history,
            clear_session,
            rename_session,
            delete_session,
            list_running_turns,
            blackboard_list,
            blackboard_delete,
            blackboard_update,
            memory_list_files,
            memory_read_file,
            memory_save_file,
            memory_delete_file,
            write_export_file,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
