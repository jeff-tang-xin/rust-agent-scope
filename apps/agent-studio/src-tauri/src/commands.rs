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
use crate::factory;
use crate::session::{AgentHandle, SessionHandle, TurnInfo};

/// 统一 user id（桌面单用户；对齐 service 的 owner 概念的最小形态）。
const USER_ID: &str = "studio";

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
}

/// 全局应用状态。
pub struct AppState {
    pub config_store: ConfigStore,
    pub config: Mutex<StudioConfig>,
    /// 已装配 agent 缓存（懒装配；配置变更后失效重建）。
    pub handles: Mutex<HashMap<String, Arc<AgentHandle>>>,
    pub data_dir: std::path::PathBuf,
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
        let built = factory::build_agent(&ws, &cfg, &provider, &self.data_dir, runtime_mode, &member_defs)?;
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
) -> Result<(), String> {
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
    let mut config = state.snapshot();
    config.databases.retain(|d| d.name != database.name);
    config.databases.push(database);
    state.persist(&config)
}

#[tauri::command]
pub fn delete_database(state: State<'_, AppState>, name: String) -> Result<(), String> {
    let mut config = state.snapshot();
    // 目前尚无 agent 绑定数据库资源（接线后这里补引用检查，对齐 delete_provider）。
    config.databases.retain(|d| d.name != name);
    state.persist(&config)
}

/// 数据库类型注册表（前端下拉与字段启停的数据源；新增类型自动跟随）。
#[tauri::command]
pub fn list_database_kinds() -> &'static [crate::db_kind::DatabaseKind] {
    crate::db_kind::DATABASE_KINDS
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
fn merge_with_pending(session: &SessionHandle, message: &str) -> String {
    let steers = session.take_pending_steers();
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

    let text = merge_with_pending(&session, &message);
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

    let join = tokio::spawn(turn_chain(
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
    session.track(join.abort_handle()).await;

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
        let mut rx = match agent
            .stream_events_with_context(rc, msg.clone())
            .await
        {
            Ok(rx) => rx,
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

        while let Some(event) = rx.recv().await {
            let payload = serde_json::to_value(&event).unwrap_or(serde_json::Value::Null);
            let envelope = EventEnvelope {
                workspace_id: workspace_id.clone(),
                session_id: session_id.clone(),
                turn_id: turn_id.clone(),
                event: payload,
            };
            let _ = app.emit("agent-event", envelope);
        }
        session.untrack().await;

        // 结算：还有排队中的 steer → 自动续轮（对齐 Java settleSession 语义）。
        let pending = session.take_pending_steers();
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

/// 排队一条转向指令（steer，≈ AgentSession.steer）。
///
/// Legacy 历史模式下无法在当前 turn 运行中注入（execution 链路需要 EventLog +
/// 持久日志后端），这里排队：turn 结算时自动续轮消费（见 `turn_chain`）。
#[tauri::command]
pub async fn steer_turn(
    state: State<'_, AppState>,
    workspace_id: String,
    mode: String,
    session_id: String,
    message: String,
) -> Result<bool, String> {
    if message.trim().is_empty() {
        return Err("steer message is empty".to_string());
    }
    let handle = state.handle(&workspace_id, &mode)?;
    let session = handle.session(&session_id).await;
    session.queue_steer(Msg::user("user", message));
    Ok(true)
}

/// 中断运行中的 turn（≈ SessionTurnRunner.interrupt）。
///
/// abort 杀掉的是整个 turn 链 task（含未开始的续轮），task 内的终态 emit
/// 不会执行，这里补发 `turn-finished(interrupted)` 让前端可靠复位 busy。
#[tauri::command]
pub async fn interrupt_turn(
    app: AppHandle,
    state: State<'_, AppState>,
    workspace_id: String,
    mode: String,
    session_id: String,
) -> Result<bool, String> {
    let handle = state.handle(&workspace_id, &mode)?;
    let session = handle.session(&session_id).await;
    let interrupted = session.interrupt().await;
    if interrupted {
        let _ = app.emit(
            "turn-finished",
            TurnFinished {
                workspace_id,
                session_id,
                turn_id: String::new(),
                outcome: "interrupted",
                error: None,
            },
        );
    }
    Ok(interrupted)
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

// ---------- sessions ----------

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
        let state_path = entry.path().join("agent_state.json");
        let updated_at = state_path
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let messages = std::fs::read(&state_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|v| {
                v.get("context")
                    .and_then(|c| c.as_array())
                    .map(|a| a.len())
            })
            .unwrap_or(0);
        out.push(SessionSummary {
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
    Ok(agent_state
        .context
        .iter()
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
    handle
        .agent
        .delegate()
        .clear_context_for(Some(USER_ID), &session_id)
        .map_err(|e| e.to_string())?;
    handle.drop_session(&session_id).await;
    Ok(())
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

/// Builder 装配（lib.rs 调用）。
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
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
            let config = config_store.load();
            app.manage(AppState {
                config_store,
                config: Mutex::new(config),
                handles: Mutex::new(HashMap::new()),
                data_dir,
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
            list_workspace_files,
            read_workspace_file,
            list_agents,
            create_agent,
            update_agent,
            delete_agent,
            list_workspaces,
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
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
