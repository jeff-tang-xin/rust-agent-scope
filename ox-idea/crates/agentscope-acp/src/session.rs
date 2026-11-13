//! ACP 会话 ↔ agentscope agent 的桥。
//!
//! - `session/new`：按 ACP 的 cwd 装配 [`HarnessAgent`]（文件工具 + 本地 shell
//!   锚定该目录；模型从环境变量读取）；
//! - `session/prompt`：驱动 agent 事件流并映射为 `session/update` 通知；
//!   权限 HITL（`RequireUserConfirm`）映射为 ACP `session/request_permission`，
//!   答复以确认消息元数据回灌 agent 续跑（对齐 agentscope 的确认协议）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agentscope_core::event::{AgentEventKind, ConfirmResult};
use agentscope_core::message::{Role, ToolResultState};
use agentscope_core::AgentStateStore;
use agentscope_core::{ContentBlock, Model, Msg, RuntimeContext, Toolkit};
use agentscope_harness::agent::{HarnessAgent, HarnessAgentBuilder};
use agentscope_harness::blackboard::{BlackboardAppendTool, BlackboardReadTool, BlackboardStore};
use agentscope_harness::middleware_memory::{MemoryFlushMiddleware, MemoryMaintenanceMiddleware};
use agentscope_harness::tools::{LocalShellBackend, ShellExecuteTool};
use agentscope_harness::tools_filesystem::FilesystemToolSet;
use agentscope_harness::skill::{SkillUtil, WorkspaceSkillRepository};
use agentscope_harness::tools_memory::MemoryToolSet;
use agentscope_harness::workspace::WorkspaceManager;
use agentscope_harness::{LoadSkillTool, MemoryConfig, ProposeSkillTool, SkillManageConfig, SkillManageTool};
use agentscope_harness::{Filesystem, LocalFilesystem, LocalFsMode, PathPolicy};
use serde_json::{json, Value};
use tokio::sync::watch;

use crate::protocol;
use crate::rpc::RpcConn;
use crate::RpcError;

/// prompt 的终局。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptOutcome {
    /// 正常结束（模型给出最终回复）。
    EndTurn,
    /// 模型拒绝执行。
    Refusal,
    /// 被取消（MVP 预留，当前协议面不触发）。
    Cancelled,
    /// 执行失败（转 JSON-RPC error）。
    Error(String),
}

/// 一个 ACP 会话：一个装配好的 agent + 它的工作目录。
struct AcpSession {
    agent: Arc<HarnessAgent>,
    #[allow(dead_code)]
    cwd: PathBuf,
    /// turn 是否在跑：steer 只在运行中接受（空闲时插队没有消费者，消息会
    /// 泄漏在队列里），prompt 防重入也靠它。
    busy: std::sync::atomic::AtomicBool,
    /// 取消信号：`session/cancel` 置 true，drive_turn 的 select 竞争感知。
    /// watch 保留最新值——通知到达时 drive_turn 即使还没进入等待也不会丢。
    cancel_tx: watch::Sender<bool>,
}

/// 会话表：sessionId → 会话。
pub struct AcpSessionHub {
    sessions: Mutex<HashMap<String, Arc<AcpSession>>>,
    /// 模型配置（provider/api key/model/base_url），进程内模式下由宿主注入，
    /// 独立进程模式下来自进程环境。
    env: Vec<(String, String)>,
}

impl AcpSessionHub {
    pub fn new(env: Vec<(String, String)>) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            env,
        }
    }

    /// `session/new`：解析 cwd → 装配 agent → 登记。
    pub async fn create_session(&self, params: Value) -> Result<Value, RpcError> {
        let cwd = protocol::parse_new_session(&params)?;
        let model = build_model(&self.env).map_err(RpcError::internal)?;
        let session_id = uuid::Uuid::new_v4().to_string();
        let agent = assemble_agent(&cwd, model, &self.env).map_err(RpcError::internal)?;
        let (cancel_tx, _cancel_rx) = watch::channel(false);
        self.sessions.lock().unwrap().insert(
            session_id.clone(),
            Arc::new(AcpSession {
                agent,
                cwd: PathBuf::from(&cwd),
                busy: std::sync::atomic::AtomicBool::new(false),
                cancel_tx,
            }),
        );
        // 新会话固定 Code 态：plan 由用户切换，装配时不预设。
        Ok(protocol::session_result(&session_id, false))
    }

    /// `session/load`：按 sessionId 重建 agent，Legacy state store 自动重放
    /// 对话历史（模型上下文恢复；ide-agent 侧 timeline 自有持久化，历史
    /// update 帧无需重放）。同进程还活着的会话幂等返回；持久层里没有的
    /// 会话报错——client 收到错误会 fallback `session/new` 开新会话。
    pub async fn load_session(&self, params: Value) -> Result<Value, RpcError> {
        let (session_id, cwd) = protocol::parse_load_session(&params)?;
        if self.sessions.lock().unwrap().contains_key(&session_id) {
            return Ok(protocol::new_session_result(&session_id));
        }
        let root = sessions_root(&cwd).ok_or_else(|| {
            RpcError::invalid_params(format!(
                "会话 {session_id} 无持久层（宿主未注入记忆锚点），无法恢复"
            ))
        })?;
        let store = agentscope_core::state::JsonFileAgentStateStore::new(root)
            .map_err(|e| RpcError::internal(format!("打开会话存储失败: {e}")))?;
        let known = store
            .list_session_ids(None)
            .map_err(|e| RpcError::internal(format!("读取会话列表失败: {e}")))?
            .into_iter()
            .any(|id| id == session_id);
        if !known {
            return Err(RpcError::invalid_params(format!("未知会话: {session_id}")));
        }
        let model = build_model(&self.env).map_err(RpcError::internal)?;
        let agent = assemble_agent(&cwd, model, &self.env).map_err(RpcError::internal)?;
        let (cancel_tx, _cancel_rx) = watch::channel(false);
        self.sessions.lock().unwrap().insert(
            session_id.clone(),
            Arc::new(AcpSession {
                agent: Arc::clone(&agent),
                cwd: PathBuf::from(&cwd),
                busy: std::sync::atomic::AtomicBool::new(false),
                cancel_tx,
            }),
        );
        // 恢复会话的 plan 态随 AgentState 持久化（plan_mode_context），重建后
        // 如实上报——前端下拉与真实拦截状态一致。
        let plan_active = agent.is_plan_mode_active(None, &session_id);
        Ok(protocol::session_result(&session_id, plan_active))
    }

    /// `session/set_config_option`：用户切换 Code/Plan 模式。plan 是用户手动
    /// 开启的硬约束（写操作由 PlanModeMiddleware 确定性拦截），agent 不自行
    /// 进入；切换幂等，响应带更新后的 configOptions 并推 config_option_update
    /// 通知（前端下拉同步）。
    pub async fn set_config_option<W>(
        &self,
        params: Value,
        conn: &RpcConn<W>,
    ) -> Result<Value, RpcError>
    where
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (session_id, value) = protocol::parse_set_config_option(&params)?;
        let session = match self.sessions.lock().unwrap().get(&session_id).cloned() {
            Some(session) => session,
            None => {
                return Err(RpcError::invalid_params(format!(
                    "未知 sessionId: {session_id}"
                )))
            }
        };
        // 执行中拒绝切换：plan 标志立即生效，turn 中途切换会让模型下一步
        // 突然被锁写/解锁，行为怪异。等当前回合结束再切。
        if session.busy.load(std::sync::atomic::Ordering::Acquire) {
            return Err(RpcError::new(
                -32000,
                "会话正在执行中，请等当前回合结束再切换 Code/Plan 模式",
            ));
        }
        let switch = if value == "plan" {
            session.agent.enter_plan_mode(None, &session_id).map(|_| ())
        } else {
            session.agent.exit_plan_mode(None, &session_id).map(|_| ())
        };
        match switch {
            Ok(()) => {
                let plan_active = value == "plan";
                let options = protocol::mode_config_options(plan_active);
                conn.notify("session/update", protocol::config_option_update(&session_id, &options))
                    .await;
                Ok(json!({ "configOptions": options }))
            }
            Err(error) => Err(RpcError::internal(error)),
        }
    }

    /// `session/prompt`：驱动一轮完整 ReAct（含权限 HITL 往返），期间把
    /// 事件流映射为 `session/update` 通知。串行语义：同一会话的 prompt
    /// 由 agentscope 的槽位门串行化，ACP 层不再加锁。
    pub async fn prompt<W>(&self, params: Value, conn: &RpcConn<W>) -> Result<Value, RpcError>
    where
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (session_id, content) = protocol::parse_prompt(&params)?;
        let session = match self.sessions.lock().unwrap().get(&session_id).cloned() {
            Some(session) => session,
            None => {
                return Err(RpcError::invalid_params(format!(
                    "未知 sessionId: {}",
                    session_id
                )))
            }
        };
        // 防重入：上一 turn 未结束时拒绝新 prompt（ide-agent 的 run 循环串行
        // 发 prompt，这里是 spawn 化后主循环不再串行化的防御）。
        if session.busy.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return Err(RpcError::invalid_params("该会话已有 turn 在运行"));
        }
        // 清掉上一 turn 可能残留的取消信号。
        let _ = session.cancel_tx.send(false);
        let outcome = drive_turn(&session, &session_id, content, conn).await;
        session.busy.store(false, std::sync::atomic::Ordering::Release);
        crate::prompt_response(outcome)
    }

    /// `_oxidea/steer`：运行中插队。消息进 agent 的 steer 队列，ReAct 循环在
    /// 下一步开始前注入（`steer_injected`）。空闲会话拒绝——没有消费者，
    /// 消息会泄漏在队列里；ide-agent 收到错误后把消息退回排队队列。
    pub fn steer(&self, params: &Value) -> Result<Value, RpcError> {
        let (session_id, content) = protocol::parse_steer(params)?;
        let session = match self.sessions.lock().unwrap().get(&session_id).cloned() {
            Some(session) => session,
            None => {
                return Err(RpcError::invalid_params(format!(
                    "未知 sessionId: {}",
                    session_id
                )))
            }
        };
        if !session.busy.load(std::sync::atomic::Ordering::Acquire) {
            return Err(RpcError::invalid_params("会话空闲，无需插队"));
        }
        // 插队消息加引导块：模型看到裸的用户消息时容易把它当新指令处理完
        // 就停（EndTurn），用户观感是「介入就结束」。引导它吸收反馈继续当前
        // 工作；方向变更时先简要说明调整再继续，不要只回复确认就停下。
        // 引导是消息首部的独立 Text 块，原始内容块（文本/图片）原样保留，
        // UI 显示不受影响（ide-agent 的 User entry 用 prompt 原文）。
        let mut guided = vec![ContentBlock::Text {
            text: "[用户在任务执行中插入的消息]（运行中插队：吸收以下反馈并继续当前工作。\
若它改变需求方向，先简要说明调整再继续执行；不要只回复确认就停下。）"
                .to_owned(),
        }];
        guided.extend(content);
        session
            .agent
            .push_steer(session_id.clone(), Msg::new(Role::User, "user", guided));
        Ok(json!({}))
    }

    /// `session/cancel` 通知：置取消信号，drive_turn 的 select 竞争感知后
    /// 以 `stopReason: "cancelled"` 结束当前 turn。
    pub fn cancel(&self, params: &Value) {
        let Some(session_id) = params.get("sessionId").and_then(Value::as_str) else {
            return;
        };
        if let Some(session) = self.sessions.lock().unwrap().get(session_id).cloned() {
            let _ = session.cancel_tx.send(true);
        }
    }
}

/// ReAct 迭代上限：`AGENTSCOPE_MAX_ITERS` 可配（设置界面写入 env），缺省 100。
/// 一轮 = 一次模型调用 + 工具执行；真实工程任务（追调用链、多文件改动、编译验证）
/// 几十轮很常见，上限太低会在任务中途以 "max iterations exceeded" 截断。
fn max_iters_from(env: &[(String, String)]) -> usize {
    env.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("AGENTSCOPE_MAX_ITERS"))
        .map(|(_, v)| v.trim().parse::<usize>().ok())
        .flatten()
        .filter(|n| *n > 0)
        .unwrap_or(100)
}

/// prompt 是否为 `/compact` 命令：全部文本块拼接后 trim 等于命令名。
/// 纯函数便于测试；带附件/图片的 prompt 不算命令（原样进模型）。
fn is_compact_command(content: &[ContentBlock]) -> bool {
    let text = content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    text.trim().eq_ignore_ascii_case("/compact")
}

/// 执行 `/compact`：强制压缩会话上下文，进度与结果以 `compaction_update` 帧
/// 通知 client（ide-agent normalize 成「Compact conversation」工具卡片）。
/// 不调模型——压缩摘要由 harness 的辅助压缩模型完成。
async fn compact_command<W>(
    session: &AcpSession,
    session_id: &str,
    conn: &RpcConn<W>,
) -> PromptOutcome
where
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // 每次命令一个新 id：normalize 按 id 记录压缩状态，复用固定 id 会让
    // 第二次 /compact 被当成旧卡片的更新而丢失。
    let compaction_id = format!(
        "manual-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    );
    conn.notify(
        "session/update",
        protocol::compaction_update(session_id, &compaction_id, "in_progress", ""),
    )
    .await;
    let outcome = match session.agent.compact_now(None, session_id).await {
        Ok(Some(detail)) => {
            conn.notify(
                "session/update",
                protocol::compaction_update(session_id, &compaction_id, "completed", &detail),
            )
            .await;
            PromptOutcome::EndTurn
        }
        Ok(None) => {
            conn.notify(
                "session/update",
                protocol::compaction_update(
                    session_id,
                    &compaction_id,
                    "completed",
                    "对话为空，无需压缩",
                ),
            )
            .await;
            PromptOutcome::EndTurn
        }
        Err(error) => {
            conn.notify(
                "session/update",
                protocol::compaction_update(session_id, &compaction_id, "failed", &error),
            )
            .await;
            PromptOutcome::EndTurn
        }
    };
    outcome
}

/// 会话持久层根：`<memory_root>/sessions/`（与记忆/黑板同域锚定项目根，
/// 跨 worktree 共享）。宿主未注入 memory_root 时返回 None（不持久化）。
fn sessions_root(cwd: &str) -> Option<PathBuf> {
    crate::ox_context_for(Path::new(cwd))
        .memory_root
        .map(|r| r.join("sessions"))
}

/// IDE 注入的系统提示词节：项目环境简报（宿主预计算，开局事实）+ 语义工具
/// 使用引导。独立成纯函数便于测试断言简报确实进入提示词。
fn ide_section_text(names: &[String], brief: Option<&str>) -> String {
    let brief = brief.unwrap_or("");
    format!(
        "{brief}\n## IDE 语义工具\n\
         - 你还有一组 IDE 语义工具（{}），它们理解项目索引、编辑器与构建系统，优先用它们而非裸 shell。\n\
         - 找代码、理链路：语义搜索定位实现，引用查找追调用方，符号信息看类型与签名——评估改动影响面时先追链路再动手。\n\
         - 改代码：优先用补丁/编辑工具（理解项目结构、能检查冲突），不要用 shell 重定向整文件覆盖。\n\
         - 构建与检查：用构建与问题检查工具拿结构化结果，比解析裸命令输出更可靠。\n\
         - 编辑器上下文：用户说「这个文件」「我选中的」时，先用 get_current_file / get_all_open_file_paths 确认所指，不要猜。\n\
         - 每个工具的详细用途与参数见其工具定义（description），不确定就先读再调，不要猜参数。\n",
        names.join(", ")
    )
}

/// plan 态场景提示词（用户手动开启 Plan 模式后由 PlanModeMiddleware 注入，
/// 替换内置英文 banner）。复用 studio 已评审的 SOLO_PLAN_PROMPT 文案：死的是
/// 节奏（只读研究 → plan_write → plan_exit 审批），活的是内容（计划按任务写）。
const PLAN_PROMPT: &str = "PLAN MODE ACTIVE（用户已手动开启）：只做研究与设计，禁止执行任何修改。\n\
     - 每个 ACT 步骤必须调用工具（读文件、搜索、plan_write），不要只叙述不动手。\n\
     - 信息缺失就问用户，不要用猜测填补。\n\
     - 用 plan_write 把完整计划写入计划文件，然后 plan_exit 请求用户批准。计划要可执行：目标、分步步骤（每步有可检验的完成标准）、涉及文件、验证方式、风险与回滚。\n\
     - 计划文件是设计记录，本身不构成批准证据；批准通过后你将回到执行态，届时按计划逐步实施，不要跳步、不要顺手做计划外的事。";

/// 装配一个面向 ACP 会话的 agent：文件工具 + 本地 shell 锚定 cwd，
/// 系统提示词声明工作目录与能力边界。
fn assemble_agent(cwd: &str, model: Arc<dyn Model>, env: &[(String, String)]) -> Result<Arc<HarnessAgent>, String> {
    // 本地磁盘文件系统层（Sandboxed：文件工具的绝对路径一律拒绝，锚定 cwd）。
    let sandboxed = |root: PathBuf| {
        Arc::new(LocalFilesystem::new(
            root.clone(),
            LocalFsMode::Sandboxed,
            PathPolicy::empty(),
        )) as Arc<dyn Filesystem>
    };
    let workspace = Arc::new(WorkspaceManager::with_filesystem(
        PathBuf::from(cwd),
        sandboxed(PathBuf::from(cwd)),
    ));
    let fs = workspace
        .filesystem()
        .cloned()
        .ok_or_else(|| "workspace has no filesystem layer".to_string())?;
    // IDE 宿主注入：语义工具（进程内直调）+ 记忆锚点（`<项目根>/.oxidea/agent`，跨 worktree 共享）。
    // 提前获取：fs 层同名工具跳过注册需要它的工具名清单。
    let host_ctx = crate::ox_context_for(Path::new(cwd));
    let mut toolkit = Toolkit::new();
    // IDE 语义工具优先：与 IDE 工具同名的 fs 层工具跳过注册（当前只有
    // `read_file`——IDE 版支持 jar/zip 内读取、依赖源码根与行号渲染，比
    // fs 层沙箱版更强；Toolkit::register 是同名后者胜，靠注册顺序隐式
    // 覆盖会让行为依赖注册次序，这里显式跳过保持工具面无重名）。
    let ide_names: std::collections::HashSet<String> = host_ctx
        .tools
        .iter()
        .map(|t| t.name().to_owned())
        .collect();
    for tool in FilesystemToolSet::new(Arc::clone(&fs), None).all() {
        if ide_names.contains(tool.name()) {
            continue;
        }
        toolkit.register(tool);
    }
    toolkit.register(Arc::new(ShellExecuteTool::new(Arc::new(
        LocalShellBackend::new_anchored(PathBuf::from(cwd)),
    ))));
    // IDE 宿主注入：语义工具（进程内直调）+ 记忆锚点（`<项目根>/.oxidea/agent`，跨 worktree 共享）。
    // host_ctx 已在上方提前获取（fs 层同名工具跳过注册需要它的工具名清单）。
    let ide_section = if host_ctx.tools.is_empty() {
        String::new()
    } else {
        let names: Vec<String> = host_ctx.tools.iter().map(|t| t.name().to_owned()).collect();
        for tool in host_ctx.tools {
            toolkit.register(tool);
        }
        ide_section_text(&names, host_ctx.project_brief.as_deref())
    };
    // 黑板报（与记忆同域锚定 `<项目根>/.oxidea/agent/blackboard.json`）：工作现场的
    // 信号流（发现/结论/风险/提醒），跨会话共享、只增不改——长任务上下文被压缩后
    // 仍可读回的持久化通道。与记忆管线同进退（宿主注入 memory_root 才启用）。
    let mut has_blackboard = false;
    if let Some(root) = &host_ctx.memory_root {
        let store = BlackboardStore::new(root.join("blackboard.json"));
        toolkit.register(Arc::new(BlackboardAppendTool::new(store.clone())));
        toolkit.register(Arc::new(BlackboardReadTool::new(store)));
        has_blackboard = true;
    }
    // 记忆管线（仅宿主注入 memory_root 时启用）：memory_save/get/search 工具 +
    // flush（Layer1 每日台账）与 maintenance（Layer2 MEMORY.md 合并）两个中间件。
    // MEMORY.md 的启动注入在 harness 层统一做（HarnessAgentBuilder::build）。
    let memory_ws = host_ctx
        .memory_root
        .as_ref()
        .map(|root| {
            Arc::new(WorkspaceManager::with_filesystem(
                root.clone(),
                sandboxed(root.clone()),
            ))
        });
    if let Some(ws) = &memory_ws {
        let tools = MemoryToolSet::new(Arc::clone(ws));
        toolkit.register(Arc::new(tools.memory_save));
        toolkit.register(Arc::new(tools.memory_get));
        toolkit.register(Arc::new(tools.memory_search));
    }
    // 浏览器工具集（CDP 驱动本机 Chromium 系浏览器，headless）：与 studio 共用
    // 框架级 crate。全部 is_read_only=true → 自动进权限白名单免确认；截图存
    // `.oxidea/screenshots/`。浏览器懒启动：首次工具调用或面板打开才拉起进程。
    // 进程级共享会话：agent 工具与浏览器直播面板用同一实例——agent 操作的
    // 页面就是用户面板里看到的页面。
    let browser_ctx = Arc::new(agentscope_browser::BrowserCtx {
        session: agentscope_browser::BrowserSession::shared(),
        screenshots_dir: Path::new(cwd).join(".oxidea/screenshots"),
    });
    macro_rules! register_browser {
        ($($tool:ident),+ $(,)?) => {
            $(toolkit.register(Arc::new(agentscope_browser::$tool { ctx: Arc::clone(&browser_ctx) }));)+
        };
    }
    register_browser!(
        BrowserNavigate, BrowserSnapshot, BrowserClick, BrowserType, BrowserScreenshot,
        BrowserClose, BrowserEvaluate, BrowserClickAt, BrowserTypeAt, BrowserHoverAt,
        BrowserScrollAt, BrowserDrag,
    );
    // Skill 生态（对齐 Anthropic Agent Skills 通用原则）：项目级 `.claude/skills/`
    // 可写（跟项目走、可提交版本库），用户级 `~/.claude/skills/` 只读（跨项目
    // 复用；安装只落项目级——工作区红线不放宽）。skill_manage 自带静态安全扫描
    // （危险内容拒绝落盘并归档），auto_promote 直写正式根（个人用途，安装即生效）。
    let skill_main = Arc::new(WorkspaceSkillRepository::writable(
        Arc::clone(&fs),
        ".claude/skills",
        Some("project"),
    ));
    let skill_drafts = Arc::new(WorkspaceSkillRepository::writable(
        Arc::clone(&fs),
        ".claude/skills/_drafts",
        Some("project"),
    ));
    let skill_manage = Arc::new(SkillManageTool::new(
        Arc::clone(&skill_main),
        skill_drafts,
        SkillManageConfig::defaults()
            .with_main_dir(".claude/skills")
            .with_drafts_dir(".claude/skills/_drafts")
            .with_auto_promote(true),
    ));
    toolkit.register(Arc::new(ProposeSkillTool::new(Arc::clone(&skill_manage))));
    toolkit.register(skill_manage);
    let skill_home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .filter(|h| !h.trim().is_empty())
        .map(PathBuf::from);
    let mut skill_repos: Vec<Arc<WorkspaceSkillRepository>> = vec![skill_main];
    if let Some(home) = &skill_home {
        skill_repos.push(Arc::new(WorkspaceSkillRepository::new(
            sandboxed(home.clone()),
            ".claude/skills",
            Some("user"),
            false,
        )));
    }
    toolkit.register(Arc::new(LoadSkillTool::new(skill_repos)));
    let blackboard_section = if has_blackboard {
        "\n## 黑板报\n\
         - 黑板是工作现场的信号流（区别于记忆的沉淀知识）：发现（finding）、结论（conclusion）、风险（risk）、提醒（note），跨会话共享、只增不改。\n\
         - 行动前先 blackboard_read：提取与当前任务相关的事实、约定、决策、风险与历史记录作为上下文基线，避免重复劳动与已知坑；黑板为空就说明后继续。\n\
         - 得出关键结论、发现风险或重要事实时立即 blackboard_append——一句话说清「是什么 + 意味着什么」，不记过程日志与草稿；写错了追加一条更正，不要试图覆盖。\n\
         - 长任务中上下文可能被压缩：黑板是压缩后仍可读回的持久化通道，关键中间结论务必落盘。\n\
         - 收尾前把本次的发现、风险、结论与记录（改动位置、验证命令、待办）写进黑板。\n"
            .to_owned()
    } else {
        String::new()
    };
    let memory_section = if memory_ws.is_some() {
        "\n## 记忆\n\
         - 你有两层长期记忆：MEMORY.md（精炼的长期知识）与 memory/ 每日台账。\n\
         - 用 memory_save 沉淀值得跨会话保留的结论（用户偏好、项目约定、架构决策、踩坑记录）——任务收尾时务必沉淀，不要只留在对话里。\n\
         - 用 memory_search / memory_get 回忆过往结论，避免重复劳动与已知坑。\n\
         - 数据边界：写记忆只能 memory_save（系统落盘，你只提供内容）；禁止用 write_file/edit_file 创建或修改 .oxidea/agent 下的 MEMORY.md、memory/**、blackboard.json——那会绕过系统结构造成数据错乱。\n"
            .to_owned()
    } else {
        String::new()
    };
    // 项目数据源探测：一次扫描同时拿提示词数据与连接档案。提取出完整连接
    // 信息的库注册 db 工具（db_query 只读免确认 / db_dml 变更走人工确认，
    // 与 studio 共用框架级 agentscope-db crate）；没有档案的库保持走 shell
    // 客户端（提示词节里说明）。没有发现就不注入、不提。
    let (datasources, db_configs) = crate::datasource::probe_full(Path::new(cwd));
    let datasource_section = if datasources.is_empty() {
        String::new()
    } else {
        crate::datasource::render_section(&datasources)
    };
    // Skill 清单（L1 元数据，渐进披露第一层）：项目级在前、用户级在后，同名项目级
    // 优先——与 load_skill 的仓库检索顺序一致。
    let skill_summaries =
        agentscope_harness::skill::collect_skill_summaries(Path::new(cwd), skill_home.as_deref());
    // Skill 清单（L1 元数据，渐进披露第一层）：外置清单由上方扫描得出；
    // 内置分组、渐进披露与冲突裁决引导都在 harness 的 skill_section_markdown
    // 里统一渲染（框架行为，acp 只做产品化组装）。
    let skill_section =
        agentscope_harness::skill::skill_section_markdown(&skill_summaries, true);
    if !db_configs.is_empty() {
        let catalog = Arc::new(agentscope_db::DbCatalog::new());
        catalog.replace_all(&db_configs);
        toolkit.register(Arc::new(agentscope_db::DbQueryTool::new(
            Arc::clone(&catalog),
            None,
            None,
        )));
        toolkit.register(Arc::new(agentscope_db::DbDmlTool::new(catalog, None, None)));
    }
    let report_items = if has_blackboard {
        "做了什么、关键改动、如何验证、遗留风险、黑板更新"
    } else {
        "做了什么、怎么验证的、还剩什么风险"
    };
    let sys_prompt = format!(
        "你是 OX Hareness，运行在用户 IDE（OXidea）中的资深全栈工程师智能体。\
         使命：以最小必要改动达成用户的真实意图；每一步可验证；交付如实。\
         当前工作目录：{cwd}——一切文件操作与命令限制在此，路径一律相对它。\n\n\
         ## 1. 六条铁律（违反任何一条即为失败）\n\
         1. 理解先于动作：改任何代码前先读它，理清调用链与副作用。读过代码得出的是事实，没验证过的是推断——表述永远区分两者，不确定就说不确定，编造比无知危害更大。\n\
         2. 外科手术：只改必须改的地方。不顺手重构、不调整无关风格、不引入没被要求的依赖与抽象。改动越小越易验证、越易回滚。\n\
         3. 自我闭环：用户说「编译一下」意味着执行→看输出→修问题→报结果，不是跑完命令就停。回合结束要么交付结果，要么说清卡在哪、需要什么决定。\n\
         4. 失败止损：同一手段连续两次不奏效就停下，说明卡在哪一步、报什么错、试过什么、你的判断——换着花样瞎试只会烧时间并留下半成品。\n\
         5. 安全红线：不碰工作区外的路径；删除、覆盖、推送等不可逆动作先说明影响范围并征得同意；密钥与凭证绝不写进文件、不打印。\n\
         6. 模式边界：Plan 模式只能由用户在界面开启，你不能自行进入或退出——复杂/高风险任务（大范围重构、依赖变更、不可逆操作）可在动手前建议用户切到 Plan 模式，由用户决定。\n\n\
         ## 2. 标准工作流（非平凡任务按此五阶段走，平凡问题直接答）\n\
         ### Phase 1 建立事实（禁止跳过）\n\
         1. blackboard_read 看已有结论（避免重复劳动与冲突）。\n\
         2. memory_search 回忆相关经验与项目约定。\n\
         3. 定位代码：知道符号名用 search_symbol；只知道报错文本用 search_text/search_regex；找文件用 glob_files；看结构用 list_directory_tree。\n\
         4. 对目标符号 get_symbol_info 看签名与文档，find_references 看调用链，再 read_file 读实现——确认约束后才算理解。\n\
         产出：能一句话说清「问题在哪、涉及哪些文件、现有约定是什么」。说不清就继续 Phase 1，不许动手。\n\
         ### Phase 2 方案\n\
         有取舍时列出选项与理由再选最优；无取舍直接说按什么做。\n\
         ### Phase 3 实施\n\
         edit_file 精确替换，old_string 从真实文件内容取（不凭记忆拼）；语义级重命名用 rename_refactoring（连带所有引用），格式化用 reformat_file；沿用项目既有命名与分层，一致性比个人偏好重要。\n\
         ### Phase 4 验证（没验证不算完成）\n\
         1. 编译：build_project 或 shell 构建，以 BUILD SUCCESS 等客观信号为准，不看「应该没问题」。\n\
         2. 问题检查：get_file_problems 对改动文件——改前先记基线，改后对比，新问题就是你引入的，修掉。\n\
         3. 定向测试：跑与改动相关的测试并核对通过数；全量测试在收尾阶段兜底。\n\
         ### Phase 5 收尾\n\
         1. blackboard_append 登记结论/风险（同伴与下次会话在等它）。\n\
         2. memory_save 沉淀跨会话有价值的结论（用户偏好、架构决策、踩坑）。\n\
         3. 汇报（格式见第 5 节）。\n\n\
         {ide_section}\
         ## 3. 工具使用规程（按场景查表执行，不要凭直觉乱试）\n\
         \n\
         ### 能力总纲：工具 / skill / MCP\n\
         \n\
         你的每一步动作都由三类外部能力承载，先认清它们是什么，再决定怎么用：\n\
         - **工具（tool）= 原子动作**：一次调用一个明确效果（读文件、改代码、跑命令），结果直接回给你。考量：选最小够用的——有专用工具（如语义检索）就不用 shell 兜底；破坏性动作（删除/覆盖/推送）按铁律 5 征得同意。\n\
         - **skill = 战术手册**：某类任务的领域知识包（流程、模板、坑），按需加载不占常驻上下文。考量：任务与某 skill 的 description 匹配时，先 load_skill 读全文再动手——方法论来自 skill，动作交给工具；skill 细化本提示词——与本提示词冲突时以本提示词为准，与工具描述冲突时以 skill 为准。\n\
         - **MCP = 外部服务**：宿主接入的外部系统（数据库、远程运维、第三方 API），工具面由服务端定义。考量：调用前读工具描述确认副作用与必填参数；外部返回是事实来源，不臆测；凭证与敏感数据不进参数、不落盘。\n\
         决策顺序：想清楚要达成什么 → 有匹配 skill 先加载（怎么做）→ 选最小够用的工具/MCP 工具（去做）→ 结果异常先读报错再重试，同一手段两次失败就止损（铁律 4）。\n\
         \n\
         ### 构建与运行\n\
         跑应用用 get_run_configurations → execute_run_configuration；长驻服务用 waitForExit=true + 短 timeout 启动拿输出快照，进程转后台后 read_file 该日志文件看新输出。\n\
         {skill_section}\
         ### 数据库（面板连接已注册时才有 db 工具）\n\
         {datasource_section}\
         用法见工具描述；纪律：变更数据/DDL 走 db_dml 的用户确认（这是设计不是故障），不要用 shell 直连数据库绕过它。\n\
         ### Shell 纪律\n\
         只跑非交互、有界、能自行结束的命令；长命令输出重定向到日志文件再读日志（两步，不用 && 串联）；不要用 shell 杀进程或直连数据库——那会绕过 IDE 进程簿记与 db_dml 的用户确认；判定成功看 exit code 与 Tests run 计数等客观信号，不看输出「看起来正常」。\n\
         ### Spec 模式：文档即状态\n\
         你的状态在文档里，不在对话记忆里。每次开工先读文档恢复上下文，按循环工作：理解 → 澄清 → 实施 → 维护。\n\
         - 进入 plan 模式（spec 阶段）先加载 spec-workflow skill 按其执行——目录约定、spec.md/tasks.md 模板与生命周期都在其中；执行期需求/方案变更同样按它维护文档。\n\
         - 测试失败必记 .oxidea/agent/specs/bugs.json 并维护状态流转（字段与流转契约见 spec-workflow skill）：自测通过才置 fixed，回归通过才置 closed。每次 update_plan 顺带播报 Bug 统计（open/fixing/fixed/closed 各多少）。\n\
         ## 4. 记忆机制（你的工作必须可追溯）\n\
         {memory_section}{blackboard_section}\
         - blackboard 是任务内共享记录本（时机见 Phase 1/5），memory 是跨会话沉淀——两者只经专用工具读写；禁止用文件工具直接改记忆与黑板文件。\n\
         ## 5. 输出规范\n\
         - Markdown，简洁直接，与用户同语言；代码给完整可运行片段。\n\
         - 汇报固定段落：{report_items}；部分完成时明确说明哪些完成、哪些没有、当前处于什么中间态。"
    );
    // 权限上下文：只读工具免确认（按 is_read_only 动态收集，新只读工具无需改这里），
    // 写工具（write/edit/execute 类）走 HITL 往返；`AGENTSCOPE_PERMISSION_MODE=bypass`
    // 时全放行（对齐 Claude bypassPermissions 语义，ide-agent 随任务启动注入）。
    let mut permission = agentscope_core::permission::PermissionContextState::default();
    for name in toolkit.tool_names() {
        if toolkit.get(&name).is_some_and(|tool| tool.is_read_only()) {
            permission.add_allow_rule(agentscope_core::permission::PermissionRule {
                tool_name: name,
                rule_content: None,
                behavior: agentscope_core::permission::PermissionBehavior::Allow,
                source: "project".to_owned(),
            });
        }
    }
    if env.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("AGENTSCOPE_PERMISSION_MODE") && v.trim().eq_ignore_ascii_case("bypass")
    }) {
        permission = permission.with_mode(agentscope_core::permission::PermissionMode::Bypass);
    }
    // 会话持久层（Legacy 模式）：`<memory_root>/sessions/` 下的
    // JsonFileAgentStateStore，会话键 = ACP sessionId（drive_turn 的 runtime
    // 携带，Legacy 有持久层时每次 run 自动重载）。IDE 重启后 session/load 按
    // sessionId 重建 agent 即恢复对话历史；宿主未注入记忆锚点（独立 stdio
    // 进程）时不持久化，session/load 会报错让 client fallback 新会话。
    let state_store = host_ctx
        .memory_root
        .as_ref()
        .map(|root| root.join("sessions"))
        .map(|root| {
            agentscope_core::state::JsonFileAgentStateStore::new(root).map(|s| {
                Arc::new(s) as Arc<dyn agentscope_core::state::AgentStateStore>
            })
        })
        .transpose()
        .map_err(|e| format!("打开会话存储失败: {e}"))?;
    let mut builder = HarnessAgentBuilder::new()
        .name("ox-hareness")
        .sys_prompt(sys_prompt)
        .model(Arc::clone(&model))
        .toolkit(toolkit)
        .workspace(PathBuf::from(cwd))
        // ReAct 迭代上限（一轮 = 一次模型调用 + 工具执行）：`AGENTSCOPE_MAX_ITERS`
        // 可配（设置界面），缺省 100——真实工程任务 30 轮经常不够，跑到一半被截断。
        .max_iters(max_iters_from(env))
        // plan 模式：用户经 Composer 模式下拉手动开启（session/set_config_option），
        // agent 不自行进入。写操作由 PlanModeMiddleware 确定性拦截（白名单不放
        // shell）。spec 模式：按需求分目录（`.oxidea/agent/specs/<NNN-短名>/`，
        // 内含 spec.md + tasks.md），完成整目录归档 archived/；记忆同域（版本库外）。
        .enable_plan_mode()
        .plan_file_directory(".oxidea/agent/specs")
        .plan_prompt(PLAN_PROMPT)
        .permission_context(permission);    if let Some(ws) = &memory_ws {
        builder = builder
            .with_memory_workspace(Arc::clone(ws))
            .middleware(Arc::new(MemoryFlushMiddleware::new(
                Arc::clone(ws),
                Arc::clone(&model),
                &MemoryConfig::default(),
            )))
            .middleware(Arc::new(MemoryMaintenanceMiddleware::new(
                Arc::clone(ws),
                Arc::clone(&model),
                &MemoryConfig::default(),
            )));
    }
    if let Some(store) = &state_store {
        builder = builder.state_store(Arc::clone(store));
    }
    let agent = builder.build().map_err(|e| format!("装配 agent 失败: {e}"))?;
    Ok(Arc::new(agent))
}

/// 从给定环境构建模型实例（对齐 studio factory 的分发模式）：
/// - `AGENTSCOPE_PROVIDER`：dashscope（默认）/ openai / anthropic / gemini / ollama
/// - `AGENTSCOPE_API_KEY` / `AGENTSCOPE_MODEL` / `AGENTSCOPE_BASE_URL`（可选）
pub fn build_model(env: &[(String, String)]) -> Result<Arc<dyn Model>, String> {
    let var = |name: &str| {
        env.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    let provider = match var("AGENTSCOPE_PROVIDER").is_empty() {
        false => var("AGENTSCOPE_PROVIDER"),
        true => "dashscope".into(),
    };
    let model_name = {
        let name = var("AGENTSCOPE_MODEL");
        if name.is_empty() {
            return Err("缺少 AGENTSCOPE_MODEL 配置（如 qwen3-coder-plus）".to_string());
        }
        name
    };
    let api_key = var("AGENTSCOPE_API_KEY");
    let base_url = var("AGENTSCOPE_BASE_URL");
    let model: Arc<dyn Model> = match provider.as_str() {
        "dashscope" => {
            let mut m = agentscope_model_dashscope::DashScopeChatModel::new(&api_key, &model_name);
            if !base_url.is_empty() {
                m = m.with_base_url(&base_url);
            }
            Arc::new(m)
        }
        "openai" => {
            let mut m = agentscope_model_openai::OpenAIChatModel::new(&api_key, &model_name);
            if !base_url.is_empty() {
                m = m.with_base_url(&base_url);
            }
            Arc::new(m)
        }
        "anthropic" => {
            let mut m = agentscope_model_anthropic::AnthropicChatModel::new(&api_key, &model_name);
            if !base_url.is_empty() {
                m = m.with_base_url(&base_url);
            }
            Arc::new(m)
        }
        "gemini" => {
            let mut m = agentscope_model_gemini::GeminiChatModel::new(&api_key, &model_name);
            if !base_url.is_empty() {
                m = m.with_base_url(&base_url);
            }
            Arc::new(m)
        }
        "ollama" => Arc::new(agentscope_model_ollama::OllamaChatModel::new(&model_name)),
        other => return Err(format!("未知 AGENTSCOPE_PROVIDER: {other}")),
    };
    Ok(model)
}

/// 驱动一轮 prompt：外层循环处理「权限挂起 → 确认回灌 → 续跑」的往返；
/// 内层把事件流映射为 ACP 通知。事件流结束即本轮终局。
/// delta 微批的攒批阈值（字节）：同块连续增量攒到这个量再发一帧。
const DELTA_FLUSH_BYTES: usize = 256;

/// flush 一块增量；写失败（消费者断开，如 ide-agent abort/kill）→ 置取消
/// 终局并返回 false，drive_turn 据此终止而非对着断开的连接空转。
async fn flush_chunk<W>(
    conn: &RpcConn<W>,
    session_id: &str,
    buffer: &mut String,
    thought: bool,
    outcome: &mut Option<PromptOutcome>,
) -> bool
where
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    if buffer.is_empty() {
        return true;
    }
    let update = if thought {
        protocol::agent_thought_chunk(session_id, buffer)
    } else {
        protocol::agent_message_chunk(session_id, buffer)
    };
    if conn.try_notify("session/update", update).await.is_err() {
        if outcome.is_none() {
            *outcome = Some(PromptOutcome::Cancelled);
        }
        buffer.clear();
        return false;
    }
    buffer.clear();
    true
}

async fn drive_turn<W>(
    session: &AcpSession,
    session_id: &str,
    content: Vec<ContentBlock>,
    conn: &RpcConn<W>,
) -> PromptOutcome
where
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // `/compact` 命令：手动压缩上下文，不进模型。ide-agent 的命令列表只是
    // composer 补全展示，发过来的是纯文本 prompt——在这里拦截执行，否则
    // "/compact" 会被当成普通消息喂给 LLM，真正的压缩从不发生（前端也就
    // 永远看不到压缩卡片）。
    if is_compact_command(&content) {
        return compact_command(session, session_id, conn).await;
    }
    // 首条：用户消息（文本 + 可选图片，多模态模型直接看图）；权限往返后：
    // 确认答复消息（带 agentscope_confirm_results 元数据，agent 侧 apply 后续跑，
    // 不会重放成聊天内容）。
    let mut msg = Msg::new(Role::User, "user", content);
    loop {
        let runtime = RuntimeContext::new().with_session_id(session_id.to_owned());
        // 拿执行句柄（detach 版会留下继续跑的执行 task——LLM 调用/工具执行/
        // 状态写入全继续，表现为「停了还在后台跑」，新 prompt 还会与它并发
        // 写同一 AgentState）。取消时必须 abort 这个句柄。
        let (mut rx, exec_task) = match session
            .agent
            .delegate()
            .stream_events_with_context(runtime, msg.clone())
            .await
        {
            Ok(pair) => pair,
            Err(error) => return PromptOutcome::Error(error.to_string()),
        };
        let mut confirm_reply_id: Option<String> = None;
        let mut confirm_results: Vec<ConfirmResult> = Vec::new();
        let mut outcome: Option<PromptOutcome> = None;
        // 工具参数/结果累积：core 的事件序列是 Start → Delta(完整 JSON) → End →
        // 执行 → ResultStart → ResultTextDelta(完整文本) → ResultEnd(state)。
        // Start/End 帧本身不带参数与结果文本，靠这两张表在 Delta 时攒、终态时取。
        let mut pending_inputs: HashMap<String, String> = HashMap::new();
        let mut pending_outputs: HashMap<String, String> = HashMap::new();
        // 工具名表（Start 记、终态查）：update_plan 的 plan_update 帧要在
        // ResultEnd(Success) 才发——End 时工具还没执行（校验可能失败），提前发
        // 会让前端显示一份工具实际拒绝的清单。
        let mut tool_names: HashMap<String, String> = HashMap::new();
        let mut pending_plan_inputs: HashMap<String, Value> = HashMap::new();
        // delta 微批 buffer：同块连续增量攒到 ~256 字节再发一帧，块切换或
        // 其它事件时先清空——把每 token 一帧 JSON 压成更少的帧流。
        let mut pending_text = String::new();
        let mut pending_thought = String::new();
        let mut cancel_rx = session.cancel_tx.subscribe();
        'events: while let Some(event) = tokio::select! {
            biased;
            // `session/cancel`：置终局 Cancelled（循环后的公共收尾把攒住的
            // 增量发完），prompt 响应 stopReason=cancelled，ide-agent 据此
            // 把会话状态归位。watch 保留最新值，通知先到也不丢。
            changed = cancel_rx.changed() => match changed {
                Ok(()) if *cancel_rx.borrow() => {
                    outcome = Some(PromptOutcome::Cancelled);
                    break 'events;
                }
                // 取消信号被重置（新 turn 开始）：继续等事件。
                Ok(()) => continue 'events,
                // 发送端没了（会话销毁）：按事件流关闭处理。
                Err(_) => None,
            },
            event = rx.recv() => event,
        } {
            match event.kind {
                AgentEventKind::TextBlockDelta { delta, .. } => {
                    if !pending_thought.is_empty()
                        && !flush_chunk(conn, session_id, &mut pending_thought, true, &mut outcome)
                            .await
                    {
                        break 'events;
                    }
                    pending_text.push_str(&delta);
                    if pending_text.len() >= DELTA_FLUSH_BYTES
                        && !flush_chunk(conn, session_id, &mut pending_text, false, &mut outcome)
                            .await
                    {
                        break 'events;
                    }
                    continue;
                }
                AgentEventKind::ThinkingBlockDelta { delta, .. } => {
                    if !pending_text.is_empty()
                        && !flush_chunk(conn, session_id, &mut pending_text, false, &mut outcome)
                            .await
                    {
                        break 'events;
                    }
                    pending_thought.push_str(&delta);
                    if pending_thought.len() >= DELTA_FLUSH_BYTES
                        && !flush_chunk(conn, session_id, &mut pending_thought, true, &mut outcome)
                            .await
                    {
                        break 'events;
                    }
                    continue;
                }
                _ => {}
            }
            // 非 delta 事件：先把攒住的增量发完，保持时间线顺序。
            if !flush_chunk(conn, session_id, &mut pending_text, false, &mut outcome).await {
                break 'events;
            }
            if !flush_chunk(conn, session_id, &mut pending_thought, true, &mut outcome).await {
                break 'events;
            }
            match event.kind {
                AgentEventKind::ToolCallStart {
                    tool_call_id,
                    tool_call_name,
                    ..
                } => {
                    tool_names.insert(tool_call_id.clone(), tool_call_name.clone());
                    conn.notify(
                        "session/update",
                        protocol::tool_call_started(
                            session_id,
                            &tool_call_id,
                            &tool_call_name,
                            protocol::tool_kind(&tool_call_name),
                            json!({}),
                        ),
                    )
                    .await;
                }
                AgentEventKind::ToolCallDelta {
                    tool_call_id,
                    delta,
                    ..
                } => {
                    // 参数分片（当前为一次完整 JSON）：攒到 End 时随 update 补发。
                    pending_inputs.entry(tool_call_id).or_default().push_str(&delta);
                }
                AgentEventKind::ToolCallEnd { tool_call_id, .. } => {
                    // 参数到位：补发 rawInput，前端卡片展开即可见完整参数。
                    if let Some(input) = pending_inputs.remove(&tool_call_id) {
                        let raw = serde_json::from_str::<Value>(&input)
                            .unwrap_or(Value::String(input));
                        // update_plan / plan_write：各留一份入参，ResultEnd(Success)
                        // 时分别转 items / markdown 形态的 plan_update 帧。
                        if matches!(
                            tool_names.get(&tool_call_id).map(String::as_str),
                            Some("update_plan") | Some("plan_write")
                        ) {
                            pending_plan_inputs.insert(tool_call_id.clone(), raw.clone());
                        }
                        conn.notify(
                            "session/update",
                            protocol::tool_call_input(session_id, &tool_call_id, raw),
                        )
                        .await;
                    }
                }
                AgentEventKind::ToolProgress {
                    tool_call_id,
                    content,
                    ..
                } => {
                    let text = content_block_text(&content);
                    conn.notify(
                        "session/update",
                        protocol::tool_call_progress(session_id, &tool_call_id, &text),
                    )
                    .await;
                }
                AgentEventKind::ToolResultTextDelta {
                    tool_call_id,
                    delta,
                    ..
                } => {
                    // 结果文本（当前为一次完整文本）：攒到 End 时随终态发出。
                    pending_outputs.entry(tool_call_id).or_default().push_str(&delta);
                }
                AgentEventKind::ToolResultEnd {
                    tool_call_id,
                    state,
                    ..
                } => {
                    let (status, note) = match state {
                        ToolResultState::Success => ("completed", String::new()),
                        ToolResultState::Error => ("failed", "工具执行失败".to_string()),
                        ToolResultState::Denied => ("failed", "用户拒绝了该操作".to_string()),
                        ToolResultState::Interrupted => ("failed", "执行被用户中断".to_string()),
                        // Running/Suspended 不会出现在终态事件里，兜底按失败上报。
                        ToolResultState::Running | ToolResultState::Suspended => {
                            ("failed", format!("工具处于异常终态: {state:?}"))
                        }
                    };
                    // 结果文本优先用工具真实输出；没有输出时退回状态说明。
                    let output = pending_outputs.remove(&tool_call_id).unwrap_or(note.clone());
                    let output = if output.is_empty() { note } else { output };
                    // update_plan 成功：把入参条目转 plan_update 帧推给前端
                    // （失败/被拒不发——前端清单保持上一次成功状态）。解析失败
                    // 只丢帧不报错：工具侧已校验过，这里兜底不该打断 turn。
                    // plan_write 成功：spec 全文转 markdown 形态帧——卡片实时
                    // 展示实现方案；随后 update_plan 的 items 帧全量替换为
                    // 执行清单（一张卡片贯穿 spec → plan → 归档）。
                    if let Some(raw) = pending_plan_inputs.remove(&tool_call_id) {
                        let name = tool_names.get(&tool_call_id).map(String::as_str);
                        if state == ToolResultState::Success {
                            if name == Some("update_plan")
                                && let Ok(entries) = protocol::parse_plan_entries(&raw)
                            {
                                conn.notify(
                                    "session/update",
                                    protocol::plan_update_frame(session_id, &entries),
                                )
                                .await;
                            }
                            if name == Some("plan_write")
                                && let Some(markdown) = raw.get("content").and_then(Value::as_str)
                            {
                                conn.notify(
                                    "session/update",
                                    protocol::plan_update_markdown_frame(session_id, markdown),
                                )
                                .await;
                            }
                        }
                    }
                    conn.notify(
                        "session/update",
                        protocol::tool_call_finished(session_id, &tool_call_id, status, &output),
                    )
                    .await;
                }
                AgentEventKind::ContextCompactionStart { reply_id } => {
                    // 压缩开始：摘要由模型生成，可能耗时数秒——先给前端
                    // 「压缩中」状态，避免误判为卡死。
                    conn.notify(
                        "session/update",
                        protocol::compaction_update(session_id, &reply_id, "in_progress", ""),
                    )
                    .await;
                }
                AgentEventKind::ContextCompactionEnd { reply_id, detail } => {
                    // 压缩结束：detail（如「57 条消息合并为 1 条摘要」）作为
                    // 卡片摘要文本；失败降级时不发 End，卡片停在 in_progress
                    // 由后续轮次的新事件覆盖。
                    conn.notify(
                        "session/update",
                        protocol::compaction_update(
                            session_id,
                            &reply_id,
                            "completed",
                            detail.as_deref().unwrap_or(""),
                        ),
                    )
                    .await;
                }
                AgentEventKind::RequireUserConfirm {
                    reply_id,
                    tool_calls,
                } => {
                    // 权限 HITL：逐个工具调用向 client 请求授权。
                    for call in &tool_calls {
                        if let ContentBlock::ToolUse {
                            id, name, input, ..
                        } = call
                        {
                            let response = conn
                                .request(
                                    "session/request_permission",
                                    protocol::request_permission_params(
                                        session_id,
                                        id,
                                        name,
                                        protocol::tool_kind(name),
                                        input.clone(),
                                    ),
                                )
                                .await;
                            let confirmed = match response {
                                Ok(value) => {
                                    option_allows(value.get("outcome").unwrap_or(&Value::Null))
                                }
                                Err((_, message)) => {
                                    tracing::warn!(%message, "acp: 授权请求失败，按拒绝处理");
                                    false
                                }
                            };
                            confirm_results.push(ConfirmResult {
                                confirmed,
                                tool_call: call.clone(),
                                rules: None,
                                reason: None,
                            });
                        }
                    }
                    confirm_reply_id = Some(reply_id);
                    break;
                }
                AgentEventKind::AgentResult { .. } => {
                    // 最终回复已通过 TextBlockDelta 流式推送过，这里只定终局。
                    outcome = Some(PromptOutcome::EndTurn);
                }
                AgentEventKind::Error { message, .. } => {
                    outcome = Some(PromptOutcome::Error(message));
                }
                // 其余事件（AgentStart/ModelCall*/块边界/ToolCallDelta/ToolResultStart/
                // 中断/子代理转发）MVP 不映射——client 侧时间线已够用。
                _ => {}
            }
        }
        // 事件流关闭：把攒住的增量发完再定终局。
        // 收尾 flush：写失败只置取消终局（flush_chunk 内部处理），不再 break。
        let _ = flush_chunk(conn, session_id, &mut pending_text, false, &mut outcome).await;
        let _ = flush_chunk(conn, session_id, &mut pending_thought, true, &mut outcome).await;
        // 取消终局（session/cancel 或写失败断连）：abort 执行 task，真正中断
        // agent——否则旧 turn 继续跑（烧 token、写状态），新 prompt 与它并发。
        if matches!(outcome, Some(PromptOutcome::Cancelled)) {
            exec_task.abort();
        }
        match (outcome, confirm_reply_id) {
            // 事件流自然结束：本轮终局。
            (Some(outcome), _) => return outcome,
            // 事件流在权限挂起后结束（agent ASKING 挂起返回）：回灌确认结果续跑。
            (None, Some(reply_id)) => {
                // 权限往返期间收到取消：不回灌续跑，直接终局（agent 侧停在
                // ASKING，未决工具由 pending recovery 兜底）。
                if *session.cancel_tx.subscribe().borrow() {
                    return PromptOutcome::Cancelled;
                }
                msg = confirm_message(&reply_id, confirm_results);
            }
            // 事件流关闭但既无终局也无权限挂起：视为异常中断。
            (None, None) => {
                return PromptOutcome::Error("agent 事件流在无终局的情况下关闭".into());
            }
        }
    }
}

/// 构造确认答复消息：确认结果放元数据键 `agentscope_confirm_results`
/// （对齐 core `metadata_keys::CONFIRM_RESULTS`，值直接是 `Vec<ConfirmResult>`），
/// 文本为空——agent 侧 `has_confirm_results` 判定后不进对话历史。
fn confirm_message(_reply_id: &str, results: Vec<ConfirmResult>) -> Msg {
    let payload = json!({ "agentscope_confirm_results": results });
    Msg::user("user", String::new()).with_metadata(payload)
}

/// ACP 授权响应的 outcome → 是否放行：
/// `{ "outcome": { "outcome": "selected", "optionId": "allow_once" } }`。
fn option_allows(outcome: &Value) -> bool {
    match outcome.get("outcome").and_then(Value::as_str) {
        Some("selected") => outcome
            .get("optionId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .starts_with("allow"),
        // cancelled / 未知 → 拒绝。
        _ => false,
    }
}

/// 取 content block 的文本（ToolProgress 的内容块）。
fn content_block_text(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text { text } => text.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::event::AgentEvent;
    use agentscope_core::message::Role;
    use agentscope_core::permission::{PermissionBehavior, PermissionContextState, PermissionRule};
    use agentscope_core::tool::{Tool, Toolkit};
    use agentscope_core::{ChatResponse, Error};
    use serde_json::json;
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;

    /// 环境简报注入：宿主给的 project_brief 原样进入 IDE 节，置于工具引导之前；
    /// 无简报时该节省略但工具引导仍在。
    #[test]
    fn ide_section_embeds_project_brief() {
        let brief = "\n## 当前项目环境（IDE 检测）\n- [cargo] ox-idea — `cargo build`\n";
        let section = ide_section_text(&["get_current_file".to_owned()], Some(brief));
        assert!(section.contains("当前项目环境"), "brief missing: {section}");
        assert!(section.contains("cargo build"));
        assert!(section.contains("## IDE 语义工具"));
        assert!(section.contains("get_current_file"));
        // 简报在工具引导之前（开局事实优先）
        let brief_pos = section.find("当前项目环境").unwrap();
        let tools_pos = section.find("## IDE 语义工具").unwrap();
        assert!(brief_pos < tools_pos);

        let plain = ide_section_text(&["x".to_owned()], None);
        assert!(!plain.contains("当前项目环境"));
        assert!(plain.contains("## IDE 语义工具"));
    }

    /// 预置响应序列的 Mock 模型（对齐 core 测试的形态）。
    struct MockModel {
        responses: Mutex<Vec<ChatResponse>>,
        /// 每次调用收到的消息文本（验证 steer 注入等输入侧行为）。
        seen: Mutex<Vec<Vec<String>>>,
    }

    impl MockModel {
        fn new(responses: Vec<ChatResponse>) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(responses),
                seen: Mutex::new(Vec::new()),
            })
        }

        /// 第 `n` 次调用（0 起）收到的消息文本列表。
        fn seen_texts(&self, call: usize) -> Vec<String> {
            self.seen
                .lock()
                .unwrap()
                .get(call)
                .cloned()
                .unwrap_or_default()
        }
    }

    #[async_trait::async_trait]
    impl Model for MockModel {
        async fn call(
            &self,
            request: agentscope_core::ChatRequest,
        ) -> Result<ChatResponse, Error> {
            let texts: Vec<String> = request
                .messages
                .iter()
                .filter_map(|m| match m.content.first() {
                    Some(ContentBlock::Text { text }) => Some(text.clone()),
                    _ => None,
                })
                .collect();
            self.seen.lock().unwrap().push(texts);
            self.responses
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| Error::Model("no more mock responses".into()))
        }
    }

    /// 测试用 echo 工具。
    struct EchoTool;

    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "原样返回输入"
        }
        fn parameters(&self) -> Value {
            json!({ "type": "object", "properties": { "text": { "type": "string" } } })
        }
        async fn call(&self, input: Value) -> Result<agentscope_core::ToolOutput, Error> {
            Ok(agentscope_core::ToolOutput::ok(format!(
                "echo: {}",
                input.get("text").and_then(Value::as_str).unwrap_or("")
            )))
        }
    }

    /// 构造最小 HarnessAgent：MockModel + 可选工具 + 可选 ask 规则。
    fn build_agent(
        responses: Vec<ChatResponse>,
        with_echo: bool,
        ask_echo: bool,
    ) -> (Arc<HarnessAgent>, Arc<MockModel>) {
        let mut toolkit = Toolkit::new();
        if with_echo {
            toolkit.register(Arc::new(EchoTool));
        }
        let model = MockModel::new(responses);
        let mut builder = HarnessAgentBuilder::new()
            .name("test")
            .sys_prompt("test")
            .model(model.clone())
            .toolkit(toolkit)
            .max_iters(5);
        if ask_echo {
            let state = PermissionContextState {
                ask_rules: std::collections::BTreeMap::from([(
                    "echo".to_string(),
                    vec![PermissionRule {
                        tool_name: "echo".to_string(),
                        rule_content: None,
                        behavior: PermissionBehavior::Ask,
                        source: "test".to_string(),
                    }],
                )]),
                ..Default::default()
            };
            builder = builder.permission_context(state);
        }
        let agent = Arc::new(builder.build().expect("build agent"));
        (agent, model)
    }

    /// 模拟 ACP client 端：读 agent 发出的帧；遇到 request_permission
    /// 就回指定 optionId 的响应；收集全部通知供断言。
    /// 同时起读循环消费 server 端入站帧（响应帧 → resolve_pending 配对）。
    /// 返回 (收集的通知帧列表, drive 终局)。
    async fn run_drive_turn(
        agent: Arc<HarnessAgent>,
        text: &str,
        grant_option: Option<&str>,
        cancel_before_grant: bool,
        steer_text: Option<&str>,
    ) -> (Vec<Value>, PromptOutcome) {
        let (client, server) = tokio::io::duplex(256 * 1024);
        let (server_read, server_write) = tokio::io::split(server);
        let conn = Arc::new(RpcConn::new(server_write));
        let (mut client_read, mut client_write) = tokio::io::split(client);

        let (cancel_tx, _cancel_rx) = tokio::sync::watch::channel(false);
        let session = AcpSession {
            agent,
            cwd: PathBuf::from("."),
            busy: std::sync::atomic::AtomicBool::new(false),
            cancel_tx,
        };

        // 读循环：消费 server 端入站帧（client 回的响应），配对给等待者。
        let reader_conn = Arc::clone(&conn);
        let reader_loop = async move {
            use tokio::io::AsyncBufReadExt;
            let mut reader = tokio::io::BufReader::new(server_read);
            let mut buf = String::new();
            loop {
                buf.clear();
                let n = reader.read_line(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                let frame: Value = match serde_json::from_str(buf.trim()) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if frame.get("id").is_some() && frame.get("method").is_none() {
                    reader_conn
                        .resolve_pending(frame["id"].clone(), frame)
                        .await;
                }
            }
        };
        tokio::spawn(reader_loop);

        let drive = drive_turn(
            &session,
            "sess-1",
            vec![ContentBlock::Text { text: text.to_string() }],
            conn.as_ref(),
        );

        // client 端循环：读帧 → request_permission 时回响应。
        let cancel_tx = session.cancel_tx.clone();
        let steer_agent = session.agent.clone();
        let client_loop = async move {
            let mut reader = tokio::io::BufReader::new(&mut client_read);
            let mut notifications = Vec::new();
            use tokio::io::AsyncBufReadExt;
            loop {
                let mut line = String::new();
                match tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
                    .await
                {
                    Ok(Ok(0)) | Err(_) | Ok(Err(_)) => break,
                    Ok(Ok(_)) => {}
                }
                let frame: Value = match serde_json::from_str(line.trim()) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if frame.get("method").and_then(Value::as_str) == Some("session/request_permission")
                {
                    // 模拟 serve 主循环转发 cancel 通知：权限答复前置取消信号
                    // （真实链路里通知先于答复帧到达）。
                    if cancel_before_grant {
                        let _ = cancel_tx.send(true);
                    }
                    // 模拟 _oxidea/steer：权限挂起（busy）窗口内插队。走真实
                    // hub.steer() 路径（含引导块包装），消息进 agent 的 steer
                    // 队列，续跑后应出现在下一轮模型输入里。
                    if let Some(steer) = steer_text {
                        let hub = crate::AcpSessionHub::new(Vec::new());
                        hub.sessions.lock().unwrap().insert(
                            "sess-1".to_string(),
                            Arc::new(AcpSession {
                                agent: Arc::clone(&steer_agent),
                                cwd: PathBuf::from("."),
                                busy: std::sync::atomic::AtomicBool::new(true),
                                cancel_tx: cancel_tx.clone(),
                            }),
                        );
                        hub.steer(&json!({
                            "sessionId": "sess-1",
                            "prompt": [{ "type": "text", "text": steer }]
                        }))
                        .unwrap();
                    }
                    let id = frame["id"].clone();
                    let option_id = grant_option.unwrap_or("reject_once");
                    let response = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "outcome": { "outcome": "selected", "optionId": option_id } },
                    });
                    client_write
                        .write_all(format!("{response}\n").as_bytes())
                        .await
                        .unwrap();
                } else if frame.get("method").is_some() {
                    notifications.push(frame);
                }
            }
            notifications
        };

        // 整体 30s 超时兜底：任何挂死都让测试失败而非卡住。
        let (outcome, notifications) = tokio::time::timeout(Duration::from_secs(30), async {
            tokio::join!(drive, client_loop)
        })
        .await
        .expect("drive_turn 测试整体超时（疑似挂死）");
        (notifications, outcome)
    }

    fn update_of(frame: &Value) -> &Value {
        &frame["params"]["update"]
    }

    /// update_plan 端到端：模型调用成功 → plan_update 帧（entries 规范化，
    /// priority 缺省 medium）；调用失败（非法 status）→ 不发 plan_update 帧，
    /// 前端清单保持上一次成功状态。
    #[tokio::test]
    async fn update_plan_tool_emits_plan_update_frames() {
        let build = |responses: Vec<ChatResponse>| {
            let mut toolkit = Toolkit::new();
            toolkit.register(Arc::new(EchoTool));
            toolkit.register(Arc::new(agentscope_harness::tools::UpdatePlanTool));
            let model = MockModel::new(responses);
            Arc::new(
                HarnessAgentBuilder::new()
                    .name("test")
                    .sys_prompt("test")
                    .model(model.clone())
                    .toolkit(toolkit)
                    .max_iters(5)
                    .build()
                    .expect("build agent"),
            )
        };
        let tool_use = |id: &str, input: Value| ChatResponse {
            content: vec![ContentBlock::ToolUse {
                id: id.into(),
                name: "update_plan".into(),
                input,
                state: None,
                content: None,
            }],
            usage: None,
            finish_reason: None,
        };

        // MockModel.pop() 从尾部弹出：执行顺序 call_ok（成功）→ call_bad（非法）
        // → 文本收尾，故数组倒着放。
        let agent = build(vec![
            ChatResponse::from_text("完成"),
            tool_use(
                "call_bad",
                json!({"entries": [{"content": "x", "status": "done"}]}),
            ),
            tool_use(
                "call_ok",
                json!({"entries": [
                    {"content": "read code", "status": "completed"},
                    {"content": "fix bug", "status": "in_progress"}
                ]}),
            ),
        ]);
        let (notifications, outcome) = run_drive_turn(agent, "更新任务清单", None, false, None).await;
        assert_eq!(outcome, PromptOutcome::EndTurn);
        let updates: Vec<&Value> = notifications.iter().map(update_of).collect();
        let plan_frames: Vec<&Value> = updates
            .iter()
            .filter(|u| u["sessionUpdate"] == "plan_update")
            .copied()
            .collect();
        assert_eq!(plan_frames.len(), 1, "只应有一次成功的 plan_update: {updates:?}");
        let plan = &plan_frames[0]["plan"];
        assert_eq!(plan["type"], "items");
        assert_eq!(plan["planId"], "tasks");
        let entries = plan["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["content"], "read code");
        assert_eq!(entries[0]["status"], "completed");
        assert_eq!(entries[1]["priority"], "medium", "priority 缺省 medium");
        // 失败调用（非法 status）：ToolOutput::error → Error 终态（failed 卡片），
        // 不产生 plan_update——前端清单保持上一次成功状态。
        assert!(
            updates.iter().any(|u| {
                u["sessionUpdate"] == "tool_call_update"
                    && u["toolCallId"] == "call_bad"
                    && u["status"] == "failed"
            }),
            "非法调用应有 failed 终态卡片，实际帧序列: {updates:?}"
        );
    }

    /// plan_write 端到端：写入成功 → markdown 形态 plan_update 帧（spec 全文
    /// 实时上卡片）；feature 参数按需求分目录落盘（spec 模式）。
    #[tokio::test]
    async fn plan_write_emits_markdown_plan_frame() {
        let tmp = std::env::temp_dir().join(format!("oxidea-acp-planwrite-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let build = |responses: Vec<ChatResponse>| {
            let manager = Arc::new(agentscope_harness::PlanModeManager::with_dir(
                ".oxidea/agent/specs",
            ));
            let fs = Arc::new(LocalFilesystem::new(
                tmp.clone(),
                LocalFsMode::Sandboxed,
                PathPolicy::empty(),
            )) as Arc<dyn Filesystem>;
            let workspace = Arc::new(WorkspaceManager::with_filesystem(tmp.clone(), fs));
            let mut toolkit = Toolkit::new();
            toolkit.register(Arc::new(
                agentscope_harness::tools::PlanWriteTool::new(manager, workspace),
            ));
            let model = MockModel::new(responses);
            Arc::new(
                HarnessAgentBuilder::new()
                    .name("test")
                    .sys_prompt("test")
                    .model(model.clone())
                    .toolkit(toolkit)
                    .max_iters(5)
                    .build()
                    .expect("build agent"),
            )
        };
        let tool_use = |id: &str, input: Value| ChatResponse {
            content: vec![ContentBlock::ToolUse {
                id: id.into(),
                name: "plan_write".into(),
                input,
                state: None,
                content: None,
            }],
            usage: None,
            finish_reason: None,
        };
        let agent = build(vec![
            ChatResponse::from_text("已写入"),
            tool_use("call_ok", json!({"feature": "001-login", "content": "# 登录方案"})),
        ]);
        let (notifications, outcome) = run_drive_turn(agent, "写 spec", None, false, None).await;
        assert_eq!(outcome, PromptOutcome::EndTurn);
        let updates: Vec<&Value> = notifications.iter().map(update_of).collect();
        let frames: Vec<&Value> = updates
            .iter()
            .filter(|u| u["sessionUpdate"] == "plan_update")
            .copied()
            .collect();
        assert_eq!(frames.len(), 1, "plan_write 成功应发一个 plan_update 帧: {updates:?}");
        let plan = &frames[0]["plan"];
        assert_eq!(plan["type"], "markdown");
        assert_eq!(plan["planId"], "tasks");
        assert_eq!(plan["markdown"], "# 登录方案");
        // feature 分目录落盘（spec 模式：每个需求一个目录）
        assert!(
            tmp.join(".oxidea/agent/specs/001-login/spec.md").exists(),
            "feature 目录应落盘"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn plain_text_reply_streams_chunks_and_ends_turn() {
        let (agent, _model) = build_agent(vec![ChatResponse::from_text("你好，世界")], false, false);
        let (notifications, outcome) = run_drive_turn(agent, "打个招呼", None, false, None).await;
        assert_eq!(outcome, PromptOutcome::EndTurn);
        let chunks: Vec<&Value> = notifications.iter().map(update_of).collect();
        let text: String = chunks
            .iter()
            .filter(|u| u["sessionUpdate"] == "agent_message_chunk")
            .map(|u| u["content"]["text"].as_str().unwrap_or(""))
            .collect();
        assert!(text.contains("你好，世界"), "实际通知: {notifications:?}");
    }

    #[tokio::test]
    async fn tool_call_maps_to_started_and_completed() {
        // 注意：MockModel.pop() 从尾部弹出，故先弹的（工具调用）放后面。
        let (agent, _model) = build_agent(
            vec![
                ChatResponse::from_text("工具跑完了"),
                ChatResponse {
                    content: vec![ContentBlock::ToolUse {
                        id: "call_1".into(),
                        name: "echo".into(),
                        input: json!({ "text": "hi" }),
                        state: None,
                        content: None,
                    }],
                    usage: None,
                    finish_reason: None,
                },
            ],
            true,
            false,
        );
        let (notifications, outcome) = run_drive_turn(agent, "echo 一下", None, false, None).await;
        assert_eq!(outcome, PromptOutcome::EndTurn);
        let updates: Vec<&Value> = notifications.iter().map(update_of).collect();
        let started = updates
            .iter()
            .find(|u| u["sessionUpdate"] == "tool_call")
            .expect("缺 tool_call 开始帧");
        assert_eq!(started["toolCallId"], "call_1");
        assert_eq!(started["title"], "echo");
        assert_eq!(started["status"], "in_progress");
        let finished = updates
            .iter()
            .find(|u| u["sessionUpdate"] == "tool_call_update" && u["status"] == "completed")
            .expect("缺 tool_call 完成帧");
        assert_eq!(finished["toolCallId"], "call_1");
        // 参数补全帧：ToolCallEnd 时 rawInput 已完整（前端卡片展开可见参数）。
        let input_frame = updates
            .iter()
            .find(|u| u["sessionUpdate"] == "tool_call_update" && u.get("rawInput").is_some())
            .expect("缺 rawInput 补全帧");
        assert_eq!(input_frame["rawInput"], json!({ "text": "hi" }));
        // 完成帧携带工具真实输出（不再是空的/纯状态说明）。
        assert!(
            finished["content"].as_array().map(|c| !c.is_empty()).unwrap_or(false),
            "完成帧应携带工具输出: {finished}"
        );
    }

    #[tokio::test]
    async fn permission_ask_round_trip_grants_and_continues() {
        // pop 从尾部弹出：先弹工具调用，后弹最终回复。
        let (agent, _model) = build_agent(
            vec![
                ChatResponse::from_text("授权后完成"),
                ChatResponse {
                    content: vec![ContentBlock::ToolUse {
                        id: "call_9".into(),
                        name: "echo".into(),
                        input: json!({ "text": "需要授权" }),
                        state: None,
                        content: None,
                    }],
                    usage: None,
                    finish_reason: None,
                },
            ],
            true,
            true, // echo 配 ask 规则 → 触发 RequireUserConfirm
        );
        let (notifications, outcome) = run_drive_turn(agent, "跑 echo", Some("allow_once"), false, None).await;
        assert_eq!(outcome, PromptOutcome::EndTurn, "授权后应续跑到终局");
        let updates: Vec<&Value> = notifications.iter().map(update_of).collect();
        // 工具最终 completed（授权生效）
        assert!(
            updates
                .iter()
                .any(|u| u["sessionUpdate"] == "tool_call_update" && u["status"] == "completed"),
            "授权后工具应完成: {updates:?}"
        );
        // 最终回复也送达
        assert!(updates
            .iter()
            .any(|u| u["sessionUpdate"] == "agent_message_chunk"));
    }

    #[tokio::test]
    async fn permission_rejection_marks_tool_failed() {
        // pop 从尾部弹出：先弹工具调用，后弹最终回复。
        let (agent, _model) = build_agent(
            vec![
                ChatResponse::from_text("那就算了"),
                ChatResponse {
                    content: vec![ContentBlock::ToolUse {
                        id: "call_9".into(),
                        name: "echo".into(),
                        input: json!({ "text": "需要授权" }),
                        state: None,
                        content: None,
                    }],
                    usage: None,
                    finish_reason: None,
                },
            ],
            true,
            true,
        );
        let (notifications, outcome) = run_drive_turn(agent, "跑 echo", Some("reject_once"), false, None).await;
        assert_eq!(outcome, PromptOutcome::EndTurn);
        let updates: Vec<&Value> = notifications.iter().map(update_of).collect();
        assert!(
            updates
                .iter()
                .any(|u| u["sessionUpdate"] == "tool_call_update" && u["status"] == "failed"),
            "拒绝后工具应标记失败: {updates:?}"
        );
    }

    #[tokio::test]
    async fn cancel_during_permission_round_trip_ends_turn_cancelled() {
        // pop 从尾部弹出：先弹工具调用，后弹最终回复。
        let (agent, _model) = build_agent(
            vec![
                ChatResponse::from_text("授权后完成"),
                ChatResponse {
                    content: vec![ContentBlock::ToolUse {
                        id: "call_c".into(),
                        name: "echo".into(),
                        input: json!({ "text": "需要授权" }),
                        state: None,
                        content: None,
                    }],
                    usage: None,
                    finish_reason: None,
                },
            ],
            true,
            true,
        );
        // 权限答复前注入取消信号（模拟 session/cancel 通知先到）→ 不回灌
        // 续跑，直接终局 cancelled；第二轮回复不应出现。
        let (notifications, outcome) =
            run_drive_turn(agent, "跑 echo", Some("allow_once"), true, None).await;
        assert_eq!(outcome, PromptOutcome::Cancelled);
        let updates: Vec<&Value> = notifications.iter().map(update_of).collect();
        assert!(
            !updates
                .iter()
                .any(|u| u["sessionUpdate"] == "agent_message_chunk"
                    && u["content"]["text"] == "授权后完成"),
            "取消后不应有第二轮回复: {updates:?}"
        );
    }

    #[tokio::test]
    async fn steer_joins_running_turn_and_reaches_model() {
        // pop 从尾部弹出：先弹工具调用，后弹最终回复。
        let (agent, model) = build_agent(
            vec![
                ChatResponse::from_text("授权后完成"),
                ChatResponse {
                    content: vec![ContentBlock::ToolUse {
                        id: "call_s".into(),
                        name: "echo".into(),
                        input: json!({ "text": "需要授权" }),
                        state: None,
                        content: None,
                    }],
                    usage: None,
                    finish_reason: None,
                },
            ],
            true,
            true,
        );
        // 权限挂起（busy）窗口内插队 → 续跑后消息应出现在下一轮模型输入里。
        let (notifications, outcome) =
            run_drive_turn(agent, "跑 echo", Some("allow_once"), false, Some("插队：改跑两次"))
                .await;
        assert_eq!(outcome, PromptOutcome::EndTurn);
        let second_call = model.seen_texts(1);
        // seen 只抽样每条消息的首个 Text 块：引导块随 steer 消息注入且位于
        // 首块，它的出现即证明 steer 消息到达模型（原文块跟在引导块后，
        // 模型输入是完整消息列表，不受 seen 抽样影响）。
        assert!(
            second_call.iter().any(|t| t.contains("运行中插队")),
            "steer 消息应注入下一轮模型输入（引导块为到达标记）: {second_call:?}"
        );
        let updates: Vec<&Value> = notifications.iter().map(update_of).collect();
        assert!(
            updates
                .iter()
                .any(|u| u["sessionUpdate"] == "tool_call_update" && u["status"] == "completed"),
            "插队不应影响工具完成: {updates:?}"
        );
    }

    /// session/load 幂等：同进程还活着的会话直接返回，不重建。
    #[tokio::test]
    async fn load_session_reuses_live_session() {
        let hub = crate::AcpSessionHub::new(Vec::new());
        let (agent, _model) = build_agent(vec![], false, false);
        let (cancel_tx, _rx) = tokio::sync::watch::channel(false);
        hub.sessions.lock().unwrap().insert(
            "s1".to_string(),
            Arc::new(AcpSession {
                agent,
                cwd: PathBuf::from("."),
                busy: std::sync::atomic::AtomicBool::new(false),
                cancel_tx,
            }),
        );
        let params = json!({ "sessionId": "s1", "cwd": "/p", "mcpServers": [] });
        let result = hub.load_session(params).await.unwrap();
        assert_eq!(result["sessionId"], "s1");
    }

    /// session/load 拒绝未知会话（无持久层或 store 里没有）——client 收到
    /// 错误会 fallback session/new。
    #[tokio::test]
    async fn load_session_rejects_unknown_session() {
        let hub = crate::AcpSessionHub::new(Vec::new());
        let params = json!({ "sessionId": "nope", "cwd": ".", "mcpServers": [] });
        assert!(hub.load_session(params).await.is_err());
    }

    /// set_config_option 切换 plan/code：plan 态由用户开启（enter_plan_mode），
    /// 切换幂等；响应 configOptions 的 currentValue 如实反映，agent 侧
    /// is_plan_mode_active 同步。
    #[tokio::test]
    async fn set_config_option_switches_plan_mode() {
        let hub = crate::AcpSessionHub::new(Vec::new());
        let model = MockModel::new(vec![]);
        let agent = Arc::new(
            HarnessAgentBuilder::new()
                .name("test")
                .sys_prompt("test")
                .model(model.clone())
                .max_iters(5)
                .enable_plan_mode()
                .build()
                .expect("build agent"),
        );
        let (cancel_tx, _rx) = tokio::sync::watch::channel(false);
        hub.sessions.lock().unwrap().insert(
            "s1".to_string(),
            Arc::new(AcpSession {
                agent,
                cwd: PathBuf::from("."),
                busy: std::sync::atomic::AtomicBool::new(false),
                cancel_tx,
            }),
        );
        let (_client, server) = tokio::io::duplex(64 * 1024);
        let (_server_read, server_write) = tokio::io::split(server);
        let conn = RpcConn::new(server_write);

        // 切 plan：响应 currentValue=plan，agent 真实进入 plan 态
        let params = json!({ "sessionId": "s1", "configId": "mode", "value": "plan" });
        let result = hub.set_config_option(params, &conn).await.unwrap();
        assert_eq!(result["configOptions"][0]["currentValue"], "plan");
        assert!(hub.sessions.lock().unwrap()["s1"]
            .agent
            .is_plan_mode_active(None, "s1"));

        // 切回 code：幂等退出
        let params = json!({ "sessionId": "s1", "configId": "mode", "value": "code" });
        let result = hub.set_config_option(params, &conn).await.unwrap();
        assert_eq!(result["configOptions"][0]["currentValue"], "code");
        assert!(!hub.sessions.lock().unwrap()["s1"]
            .agent
            .is_plan_mode_active(None, "s1"));

        // busy（执行中）拒绝切换：plan 标志立即生效，turn 中途切换会让模型
        // 下一步突然被锁写/解锁——等当前回合结束再切。
        hub.sessions.lock().unwrap()["s1"]
            .busy
            .store(true, std::sync::atomic::Ordering::Release);
        let params = json!({ "sessionId": "s1", "configId": "mode", "value": "plan" });
        let error = hub.set_config_option(params, &conn).await.unwrap_err();
        assert!(error.message.contains("执行中"), "busy 拒绝: {}", error.message);
        hub.sessions.lock().unwrap()["s1"]
            .busy
            .store(false, std::sync::atomic::Ordering::Release);

        // 未知配置项 / 未知值 → invalid params
        let params = json!({ "sessionId": "s1", "configId": "model", "value": "m1" });
        assert!(hub.set_config_option(params, &conn).await.is_err());
        let params = json!({ "sessionId": "s1", "configId": "mode", "value": "yolo" });
        assert!(hub.set_config_option(params, &conn).await.is_err());
        let params = json!({ "sessionId": "ghost", "configId": "mode", "value": "plan" });
        assert!(hub.set_config_option(params, &conn).await.is_err());
    }

    /// plan 态随 AgentState 持久化：enter_plan_mode 落盘后，全新装配的 agent
    /// （模拟 IDE 重启）同 sessionId 能读到 plan 态——session/load 的
    /// configOptions 据此如实上报。
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn plan_mode_state_survives_reassembly() {
        use crate::{set_ox_tool_provider, OxHostContext};
        // 有意持锁跨 await：串行化对全局 provider 的切换，防止并行测试互扰
        //（lint 报告点在 await 处，allow 标注在测试函数上）。
        let _guard = crate::OX_PROVIDER_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("ox-acp-plan-{}", std::process::id()));
        let cwd = root.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let memory_root = root.join("memory-anchor");
        set_ox_tool_provider(Arc::new(move |_| OxHostContext {
            tools: Vec::new(),
            memory_root: Some(memory_root.clone()),
            project_brief: None,
        }));
        let env = vec![("AGENTSCOPE_MAX_ITERS".to_owned(), "5".to_owned())];

        let model1 = MockModel::new(vec![]);
        let agent1 = assemble_agent(cwd.to_str().unwrap(), model1, &env).unwrap();
        assert!(!agent1.is_plan_mode_active(None, "s-plan"));
        agent1.enter_plan_mode(None, "s-plan").unwrap();
        assert!(agent1.is_plan_mode_active(None, "s-plan"));

        let model2 = MockModel::new(vec![]);
        let agent2 = assemble_agent(cwd.to_str().unwrap(), model2, &env).unwrap();
        assert!(
            agent2.is_plan_mode_active(None, "s-plan"),
            "plan 态应随 AgentState 持久化并在重建后恢复"
        );
        agent2.exit_plan_mode(None, "s-plan").unwrap();
        assert!(!agent2.is_plan_mode_active(None, "s-plan"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 会话持久化端到端：assemble_agent（宿主注入 memory_root）跑一轮后
    /// 历史落盘；全新 agent（模拟 IDE 重启）同 sessionId 再跑一轮，模型
    /// 输入应包含第一轮的问答历史。
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn assemble_agent_persists_and_reloads_history() {
        use crate::{set_ox_tool_provider, OxHostContext};
        // 有意持锁跨 await：串行化对全局 provider 的切换，防止并行测试互扰
        //（lint 报告点在 await 处，allow 标注在测试函数上）。
        let _guard = crate::OX_PROVIDER_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("ox-acp-load-{}", std::process::id()));
        let cwd = root.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let memory_root = root.join("memory-anchor");
        set_ox_tool_provider(Arc::new(move |_| OxHostContext {
            tools: Vec::new(),
            memory_root: Some(memory_root.clone()),
            project_brief: None,
        }));
        let env = vec![("AGENTSCOPE_MAX_ITERS".to_owned(), "5".to_owned())];

        let model1 = MockModel::new(vec![ChatResponse::from_text("第一轮回复")]);
        let agent1 = assemble_agent(cwd.to_str().unwrap(), model1.clone(), &env).unwrap();
        let (_n1, outcome1) = run_drive_turn(agent1, "第一轮提问", None, false, None).await;
        assert_eq!(outcome1, PromptOutcome::EndTurn);

        let model2 = MockModel::new(vec![ChatResponse::from_text("第二轮回复")]);
        let agent2 = assemble_agent(cwd.to_str().unwrap(), model2.clone(), &env).unwrap();
        let (_n2, outcome2) = run_drive_turn(agent2, "第二轮提问", None, false, None).await;
        assert_eq!(outcome2, PromptOutcome::EndTurn);
        let seen = model2.seen_texts(0);
        assert!(
            seen.iter().any(|t| t.contains("第一轮提问")),
            "重载后模型输入应含第一轮用户消息: {seen:?}"
        );
        assert!(
            seen.iter().any(|t| t.contains("第一轮回复")),
            "重载后模型输入应含第一轮助手回复: {seen:?}"
        );
    }

    #[test]
    fn steer_rejects_idle_session() {
        let hub = crate::AcpSessionHub::new(Vec::new());
        let (agent, _model) = build_agent(vec![], false, false);
        let (cancel_tx, _rx) = tokio::sync::watch::channel(false);
        hub.sessions.lock().unwrap().insert(
            "s1".to_string(),
            Arc::new(AcpSession {
                agent,
                cwd: PathBuf::from("."),
                busy: std::sync::atomic::AtomicBool::new(false),
                cancel_tx,
            }),
        );
        let params = json!({
            "sessionId": "s1",
            "prompt": [{ "type": "text", "text": "插队" }],
        });
        // 空闲会话拒绝插队（没有消费者，消息会泄漏在队列里）。
        assert!(hub.steer(&params).is_err());
        // busy 后接受。
        if let Some(session) = hub.sessions.lock().unwrap().get("s1") {
            session
                .busy
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        assert!(hub.steer(&params).is_ok());
    }

    #[test]
    fn option_allows_semantics() {
        let selected_allow = json!({ "outcome": "selected", "optionId": "allow_once" });
        let selected_reject = json!({ "outcome": "selected", "optionId": "reject_once" });
        let cancelled = json!({ "outcome": "cancelled" });
        assert!(option_allows(&selected_allow));
        assert!(!option_allows(&selected_reject));
        assert!(!option_allows(&cancelled));
        assert!(!option_allows(&Value::Null));
    }

    #[test]
    fn confirm_message_carries_metadata_without_text() {
        let results = vec![ConfirmResult {
            confirmed: true,
            tool_call: ContentBlock::ToolUse {
                id: "c1".into(),
                name: "echo".into(),
                input: json!({}),
                state: None,
                content: None,
            },
            rules: None,
            reason: None,
        }];
        let msg = confirm_message("reply-1", results);
        assert_eq!(msg.role, Role::User);
        assert!(
            msg.metadata
                .as_ref()
                .and_then(|m| m.get("agentscope_confirm_results"))
                .is_some(),
            "确认结果应在元数据里: {:?}",
            msg.metadata
        );
    }

    #[test]
    fn content_block_text_extracts_text() {
        let block = ContentBlock::Text {
            text: "进度 50%".into(),
        };
        assert_eq!(content_block_text(&block), "进度 50%");
        let other = ContentBlock::ToolUse {
            id: "c".into(),
            name: "n".into(),
            input: json!({ "k": 1 }),
            state: None,
            content: None,
        };
        assert!(
            content_block_text(&other).contains("\"k\":1")
                || content_block_text(&other).contains("\"k\": 1")
        );
    }

    #[test]
    fn max_iters_reads_env_with_sane_fallback() {
        let env = |v: &str| vec![("AGENTSCOPE_MAX_ITERS".to_owned(), v.to_owned())];
        assert_eq!(max_iters_from(&env("200")), 200);
        assert_eq!(max_iters_from(&env(" 50 ")), 50);
        // 非法与无意义值回落缺省；空 env 同。
        assert_eq!(max_iters_from(&env("abc")), 100);
        assert_eq!(max_iters_from(&env("0")), 100);
        assert_eq!(max_iters_from(&env("-5")), 100);
        assert_eq!(max_iters_from(&[]), 100);
        // 大小写不敏感（env 键约定大写，宿主注入路径不保证）。
        assert_eq!(max_iters_from(&[("agentscope_max_iters".to_owned(), "30".to_owned())]), 30);
    }

    #[test]
    fn compact_command_matches_bare_slash_only() {
        let text = |s: &str| vec![ContentBlock::Text { text: s.into() }];
        assert!(is_compact_command(&text("/compact")));
        // 前后空白容忍（composer 回车带空格）。
        assert!(is_compact_command(&text(" /compact ")));
        // 其他文本不是命令——原样进模型。
        assert!(!is_compact_command(&text("/compact 压缩一下")));
        assert!(!is_compact_command(&text("请帮我 /compact")));
        assert!(!is_compact_command(&text("")));
        // 带图片的 prompt 仍是命令：敲了 /compact 就是想压缩，图片块被忽略
        // （压缩的是已有上下文，当前 prompt 不进模型）。
        let with_image = vec![
            ContentBlock::Text { text: "/compact".into() },
            ContentBlock::Image {
                source: agentscope_core::message::Source::Base64 {
                    media_type: "image/png".into(),
                    data: "aGk=".into(),
                },
                min_pixels: None,
                max_pixels: None,
            },
        ];
        assert!(is_compact_command(&with_image));
    }

    // AgentEvent 构造引用（防 import 漂移）。
    #[allow(dead_code)]
    fn event_type_is_stable(event: &AgentEvent) -> &str {
        &event.id
    }

    // ---------- Skill 生态接线 ----------

    /// 项目级 + 用户级技能根的扫描：项目级优先、同名去重、用户级补充。
    #[test]
    fn collect_skill_summaries_scans_project_and_user() {
        let root = std::env::temp_dir().join(format!("ox-acp-skills-{}", std::process::id()));
        let proj = root.join("proj");
        let home = root.join("home");
        for base in [&proj, &home] {
            std::fs::create_dir_all(base.join(".claude").join("skills")).unwrap();
        }
        for d in [
            proj.join(".claude").join("skills").join("demo"),
            home.join(".claude").join("skills").join("demo"),
            home.join(".claude").join("skills").join("lint"),
        ] {
            std::fs::create_dir_all(&d).unwrap();
        }
        std::fs::write(
            proj.join(".claude").join("skills").join("demo").join("SKILL.md"),
            "---\nname: demo\ndescription: Project demo skill\n---\n# Demo\n",
        )
        .unwrap();
        std::fs::write(
            home.join(".claude").join("skills").join("demo").join("SKILL.md"),
            "---\nname: demo\ndescription: User demo skill\n---\n# Demo\n",
        )
        .unwrap();
        std::fs::write(
            home.join(".claude").join("skills").join("lint").join("SKILL.md"),
            "---\nname: lint\ndescription: Lint the code\n---\n# Lint\n",
        )
        .unwrap();
        let got = agentscope_harness::skill::collect_skill_summaries(&proj, Some(&home));
        assert_eq!(
            got,
            vec![
                ("demo".to_owned(), "Project demo skill".to_owned()),
                ("lint".to_owned(), "Lint the code".to_owned()),
            ],
            "项目级在前且同名优先，用户级补充"
        );
        // 无技能根 → 空清单。
        let empty = agentscope_harness::skill::collect_skill_summaries(&root.join("nope"), Some(&root.join("nohome")));
        assert!(empty.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 端到端：assemble_agent 注册 skill 工具 + sys_prompt 注入 L1 清单 +
    /// load_skill 真实执行（模型 tool_call → 工具结果回灌）。
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn assemble_agent_wires_skills_end_to_end() {
        use crate::{set_ox_tool_provider, OxHostContext};
        // 有意持锁跨 await：串行化对全局 provider 的切换，防止并行测试互扰
        //（lint 报告点在 await 处，allow 标注在测试函数上）。
        let _guard = crate::OX_PROVIDER_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("ox-acp-skill-e2e-{}", std::process::id()));
        let cwd = root.join("proj");
        std::fs::create_dir_all(cwd.join(".claude").join("skills").join("demo")).unwrap();
        std::fs::write(
            cwd.join(".claude").join("skills").join("demo").join("SKILL.md"),
            "---\nname: demo\ndescription: Demo skill for e2e\n---\n# Demo Body\nStep one.\n",
        )
        .unwrap();
        set_ox_tool_provider(Arc::new(move |_| OxHostContext {
            tools: Vec::new(),
            memory_root: None,
            project_brief: None,
        }));
        let env = vec![("AGENTSCOPE_MAX_ITERS".to_owned(), "5".to_owned())];

        let tool_use = ChatResponse {
            content: vec![ContentBlock::ToolUse {
                id: "call_load".into(),
                name: "load_skill".into(),
                input: json!({ "skill": "demo" }),
                state: None,
                content: None,
            }],
            usage: None,
            finish_reason: None,
        };
        let model = MockModel::new(vec![ChatResponse::from_text("完成"), tool_use]);
        let agent = assemble_agent(cwd.to_str().unwrap(), model.clone(), &env).unwrap();
        let (notifications, outcome) =
            run_drive_turn(agent, "用 demo skill", None, false, None).await;
        assert_eq!(outcome, PromptOutcome::EndTurn);

        // 第一轮输入：sys_prompt 含 L1 清单（name + description）。
        let first = model.seen_texts(0).join("\n");
        assert!(
            first.contains("demo: Demo skill for e2e"),
            "sys_prompt 应含 skill 清单: {first}"
        );
        // 内置 skill 单独分组并标注适用模式（分模式加载）。
        assert!(
            first.contains("spec-workflow [plan 模式]"),
            "sys_prompt 应含内置 skill 分组: {first}"
        );
        // 能力总纲：三类外部能力的定义与决策顺序。
        assert!(
            first.contains("能力总纲") && first.contains("MCP = 外部服务"),
            "sys_prompt 应含能力总纲: {first}"
        );
        // load_skill 真实执行：完成帧携带 SKILL.md 正文（工具结果回灌走
        // ToolResult 块，不进 MockModel 的 seen 文本，故从事件帧断言）。
        let updates: Vec<&Value> = notifications.iter().map(update_of).collect();
        let finished = updates
            .iter()
            .find(|u| {
                u["sessionUpdate"] == "tool_call_update"
                    && u["toolCallId"] == "call_load"
                    && u["status"] == "completed"
            })
            .expect("load_skill 应完成");
        let out_text = serde_json::to_string(&finished["content"]).unwrap();
        assert!(
            out_text.contains("Demo Body") && out_text.contains("Step one."),
            "load_skill 输出应含 SKILL.md 正文: {out_text}"
        );
        // 模型确实被第二次调用（工具结果入历史后继续推理）。
        assert!(!model.seen_texts(1).is_empty(), "模型应被第二次调用");
        let _ = std::fs::remove_dir_all(&root);
    }
}
