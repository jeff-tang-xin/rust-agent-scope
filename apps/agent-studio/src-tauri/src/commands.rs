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

use crate::config::{AgentConfig, ConfigStore, ModelProviderConfig, StudioConfig};
use crate::factory;
use crate::session::{AgentHandle, SessionHandle, TurnInfo};

/// 统一 user id（桌面单用户；对齐 service 的 owner 概念的最小形态）。
const USER_ID: &str = "studio";

/// 事件信封：前端按 (agentId, sessionId, turnId) 归流，event 为 AgentEvent JSON。
#[derive(Debug, Clone, Serialize)]
struct EventEnvelope {
    agent_id: String,
    session_id: String,
    turn_id: String,
    event: serde_json::Value,
}

/// turn 终态通知（对齐 Java Task.settled() 的前端可观测形态）。
#[derive(Debug, Clone, Serialize)]
struct TurnFinished {
    agent_id: String,
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

    /// 取（或懒装配）agent 句柄（对齐 service 的 agent 按需实例化）。
    /// 缓存 key = `{agent_id}:{mode}`——同一 agent 可按运行方式（solo/team）
    /// 装配不同实例（工具面与成员装配不同）。
    pub fn handle(&self, agent_id: &str, mode: &str) -> Result<Arc<AgentHandle>, String> {
        let key = format!("{agent_id}:{mode}");
        if let Some(handle) = self
            .handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            return Ok(Arc::clone(handle));
        }
        let config = self.snapshot();
        let cfg = config
            .agent(agent_id)
            .cloned()
            .ok_or_else(|| format!("unknown agent `{agent_id}`"))?;
        let provider = config
            .provider(&cfg.provider)
            .cloned()
            .ok_or_else(|| format!("agent `{agent_id}` references unknown provider `{}`", cfg.provider))?;
        let built = factory::build_agent(&cfg, &provider, &self.data_dir, mode)?;
        let handle = Arc::new(AgentHandle::new(built.agent));
        self.handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, Arc::clone(&handle));
        Ok(handle)
    }

    /// 配置变更后失效缓存（下次 handle 重建）；移除该 agent 所有运行方式的实例。
    fn invalidate(&self, agent_id: &str) {
        let prefix = format!("{agent_id}:");
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

// ---------- agents ----------

#[tauri::command]
pub fn list_agents(state: State<'_, AppState>) -> Vec<AgentConfig> {
    state.snapshot().agents
}

#[tauri::command]
pub fn create_agent(state: State<'_, AppState>, mut agent: AgentConfig) -> Result<AgentConfig, String> {
    if agent.id.is_empty() {
        agent.id = uuid::Uuid::new_v4().simple().to_string();
    }
    if agent.created_at == 0 {
        agent.created_at = unix_now();
    }
    factory::StudioMode::parse(&agent.mode)
        .ok_or_else(|| format!("unknown mode `{}`", agent.mode))?;
    let mut config = state.snapshot();
    if config.agents.iter().any(|a| a.id == agent.id) {
        return Err(format!("agent `{}` already exists", agent.id));
    }
    config.agents.push(agent.clone());
    state.persist(&config)?;
    Ok(agent)
}

#[tauri::command]
pub fn update_agent(state: State<'_, AppState>, agent: AgentConfig) -> Result<(), String> {
    let mut config = state.snapshot();
    let slot = config
        .agents
        .iter_mut()
        .find(|a| a.id == agent.id)
        .ok_or_else(|| format!("unknown agent `{}`", agent.id))?;
    *slot = agent;
    let id = slot.id.clone();
    state.persist(&config)?;
    state.invalidate(&id);
    Ok(())
}

#[tauri::command]
pub fn delete_agent(state: State<'_, AppState>, agent_id: String) -> Result<(), String> {
    let mut config = state.snapshot();
    config.agents.retain(|a| a.id != agent_id);
    state.persist(&config)?;
    state.invalidate(&agent_id);
    // 数据目录保留（对话状态与 workspace 不丢；彻底清除属破坏性操作，由用户手动做）。
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
pub async fn send_message(
    app: AppHandle,
    state: State<'_, AppState>,
    agent_id: String,
    mode: String,
    session_id: String,
    message: String,
    // 附件（图片 → Image 块，其他文件 → Data 块；base64 内联）。
    attachments: Option<Vec<AttachmentInput>>,
) -> Result<TurnInfo, String> {
    let handle = state.handle(&agent_id, &mode)?;
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
    if blocks.is_empty() {
        return Err("message is empty".to_string());
    }
    let user_msg = Msg::new(Role::User, "user", blocks);

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
            agent_id,
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
    agent_id: String,
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
        agent_id,
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
                        agent_id,
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
                agent_id: agent_id.clone(),
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
                    agent_id,
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
                        agent_id,
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
                agent_id: agent_id.clone(),
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
                agent_id: agent_id.clone(),
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
    agent_id: String,
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
    agent_id: String,
    mode: String,
    session_id: String,
    message: String,
) -> Result<bool, String> {
    if message.trim().is_empty() {
        return Err("steer message is empty".to_string());
    }
    let handle = state.handle(&agent_id, &mode)?;
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
    agent_id: String,
    mode: String,
    session_id: String,
) -> Result<bool, String> {
    let handle = state.handle(&agent_id, &mode)?;
    let session = handle.session(&session_id).await;
    let interrupted = session.interrupt().await;
    if interrupted {
        let _ = app.emit(
            "turn-finished",
            TurnFinished {
                agent_id,
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
    agent_id: String,
    mode: String,
    session_id: String,
    active: bool,
) -> Result<PlanStatus, String> {
    let handle = state.handle(&agent_id, &mode)?;
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
    agent_id: String,
    mode: String,
    session_id: String,
) -> Result<PlanStatus, String> {
    let handle = state.handle(&agent_id, &mode)?;
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
    agent_id: String,
) -> Result<Vec<SessionSummary>, String> {
    // JsonFileAgentStateStore 布局：<root>/<safe(user)>/<safe(session)>/；
    // USER_ID="studio" 与前端生成的 uuid（hex）都是安全命名，不触发转义。
    let user_dir = factory::agent_dir(&state.data_dir, &agent_id)
        .join("state")
        .join(USER_ID);
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&user_dir) {
        Ok(entries) => entries,
        Err(_) => return Ok(out), // 还没有任何会话
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
    agent_id: String,
    mode: String,
    session_id: String,
) -> Result<Vec<HistoryItem>, String> {
    let handle = state.handle(&agent_id, &mode)?;
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
    agent_id: String,
    mode: String,
    session_id: String,
) -> Result<(), String> {
    let handle = state.handle(&agent_id, &mode)?;
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

/// Builder 装配（lib.rs 调用）。
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .setup(|app| {
            use tauri::Manager;
            let data_dir = app
                .path()
                .app_data_dir()
                .unwrap_or_else(|_| std::env::temp_dir().join("agent-studio"));
            std::fs::create_dir_all(&data_dir).ok();
            let config_dir = app
                .path()
                .app_config_dir()
                .unwrap_or_else(|_| data_dir.clone());
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
            list_agents,
            create_agent,
            update_agent,
            delete_agent,
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
