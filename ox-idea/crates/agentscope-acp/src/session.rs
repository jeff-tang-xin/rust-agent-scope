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
use agentscope_core::{ContentBlock, Model, Msg, RuntimeContext, Toolkit};
use agentscope_harness::agent::{HarnessAgent, HarnessAgentBuilder};
use agentscope_harness::blackboard::{BlackboardAppendTool, BlackboardReadTool, BlackboardStore};
use agentscope_harness::middleware_memory::{MemoryFlushMiddleware, MemoryMaintenanceMiddleware};
use agentscope_harness::tools::{LocalShellBackend, ShellExecuteTool};
use agentscope_harness::tools_filesystem::FilesystemToolSet;
use agentscope_harness::tools_memory::MemoryToolSet;
use agentscope_harness::workspace::WorkspaceManager;
use agentscope_harness::MemoryConfig;
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
        Ok(protocol::new_session_result(&session_id))
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
        session
            .agent
            .push_steer(session_id.clone(), Msg::new(Role::User, "user", content));
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
    for tool in FilesystemToolSet::new(fs, None).all() {
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
        format!(
            "\n## IDE 语义工具\n\
             - 你还有一组 IDE 语义工具（{}），它们理解项目索引、编辑器与构建系统，优先用它们而非裸 shell。\n\
             - 找代码、理链路：语义搜索定位实现，引用查找追调用方，符号信息看类型与签名——评估改动影响面时先追链路再动手。\n\
             - 改代码：优先用补丁/编辑工具（理解项目结构、能检查冲突），不要用 shell 重定向整文件覆盖。\n\
             - 构建与检查：用构建与问题检查工具拿结构化结果，比解析裸命令输出更可靠。\n\
             - 每个工具的详细用途与参数见其工具定义（description），不确定就先读再调，不要猜参数。\n",
            names.join(", ")
        )
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
        .map(|root| {
            Arc::new(WorkspaceManager::with_filesystem(
                root.clone(),
                sandboxed(root),
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
    // `.oxidea/screenshots/`。浏览器懒启动：首次工具调用才拉起进程。
    let browser_ctx = Arc::new(agentscope_browser::BrowserCtx {
        session: Arc::new(agentscope_browser::BrowserSession::new()),
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
         ## 1. 五条铁律（违反任何一条即为失败）\n\
         1. 理解先于动作：改任何代码前先读它，理清调用链与副作用。读过代码得出的是事实，没验证过的是推断——表述永远区分两者，不确定就说不确定，编造比无知危害更大。\n\
         2. 外科手术：只改必须改的地方。不顺手重构、不调整无关风格、不引入没被要求的依赖与抽象。改动越小越易验证、越易回滚。\n\
         3. 自我闭环：用户说「编译一下」意味着执行→看输出→修问题→报结果，不是跑完命令就停。回合结束要么交付结果，要么说清卡在哪、需要什么决定。\n\
         4. 失败止损：同一手段连续两次不奏效就停下，说明卡在哪一步、报什么错、试过什么、你的判断——换着花样瞎试只会烧时间并留下半成品。\n\
         5. 安全红线：不碰工作区外的路径；删除、覆盖、推送等不可逆动作先说明影响范围并征得同意；密钥与凭证绝不写进文件、不打印。\n\n\
         ## 2. 标准工作流（非平凡任务按此五阶段走，平凡问题直接答）\n\
         ### Phase 1 建立事实（禁止跳过）\n\
         1. blackboard_read 看已有结论（避免重复劳动与冲突）。\n\
         2. memory_search 回忆相关经验与项目约定。\n\
         3. 定位代码：知道符号名用 search_symbol；只知道报错文本用 search_text/search_regex；找文件用 glob_files；看结构用 list_directory_tree。\n\
         4. 对目标符号 get_symbol_info 看签名与文档，find_references 看调用链，再 read_file 读实现——确认约束后才算理解。\n\
         产出：能一句话说清「问题在哪、涉及哪些文件、现有约定是什么」。说不清就继续 Phase 1，不许动手。\n\
         ### Phase 2 方案\n\
         有取舍时列出选项与理由再选最优；无取舍直接说按什么做。方案含破坏性操作（删除/覆盖/推送）必须先征得同意。\n\
         ### Phase 3 实施\n\
         edit_file 精确替换，old_string 从真实文件内容取（不凭记忆拼）；每处修改都能回答「为什么改它」；沿用项目既有命名与分层，一致性比个人偏好重要。\n\
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
         ### 改代码的完整闭环\n\
         改前：read_file 拿真实文本 + get_file_problems 记基线 → 改后：get_file_problems 对比 + 编译 + 定向测试。语义级重命名用 rename_refactoring（连带所有引用），格式化用 reformat_file。\n\
         ### 构建与运行\n\
         build_project 拿结构化错误，逐个修完再 build；跑应用用 get_run_configurations → execute_run_configuration；长命令输出重定向到日志文件再读日志（两步，不用 && 串联）。\n\
         ### 数据库（面板连接已注册时才有 db 工具）\n\
         {datasource_section}\
         先 db_query 摸清 schema（只读免确认），再写 SQL——不要猜列名表名；变更数据/DDL 用 db_dml（会请求用户确认，这是设计不是故障）。\n\
         ### 浏览器（需要看真实页面时）\n\
         browser_navigate → browser_snapshot 读 DOM 结构（比截图省且准）→ browser_click_at/browser_type_at 坐标操作；验证视觉效果才用 browser_screenshot。\n\
         ### Shell 纪律\n\
         只跑非交互、有界、能自行结束的命令；判定成功看 exit code 与 Tests run 计数等客观信号，不看输出「看起来正常」。\n\n\
         ## 4. 记忆机制（你的工作必须可追溯）\n\
         {memory_section}{blackboard_section}\
         - blackboard（任务内共享）：开工先读；得出结论、发现风险随手记（不要攒到收尾）；收尾必写。\n\
         - memory（跨会话）：新任务先 memory_search；用户偏好、项目约定、踩坑记录用 memory_save 沉淀——记住自己做过什么、没做什么，避免重复踩坑。\n\
         - 数据边界：写记忆只能 memory_save；禁止用文件工具直接改记忆与黑板文件。\n\n\
         ## 5. 输出规范\n\
         - Markdown，简洁直接，与用户同语言；代码给完整可运行片段。\n\
         - 汇报固定四段：做了什么 / 关键改动 / 如何验证 / 遗留风险；部分完成时明确说明哪些完成、哪些没有、当前处于什么中间态。\n\
         - {report_items}。"
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
    let mut builder = HarnessAgentBuilder::new()
        .name("ox-hareness")
        .sys_prompt(sys_prompt)
        .model(Arc::clone(&model))
        .toolkit(toolkit)
        .workspace(PathBuf::from(cwd))
        // ReAct 迭代上限（一轮 = 一次模型调用 + 工具执行）：`AGENTSCOPE_MAX_ITERS`
        // 可配（设置界面），缺省 100——真实工程任务 30 轮经常不够，跑到一半被截断。
        .max_iters(max_iters_from(env))
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
                    conn.notify(
                        "session/update",
                        protocol::tool_call_finished(session_id, &tool_call_id, status, &note),
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
                    // 模拟 _oxidea/steer：权限挂起（busy）窗口内插队，消息进
                    // agent 的 steer 队列，续跑后应出现在下一轮模型输入里。
                    if let Some(steer) = steer_text {
                        steer_agent.push_steer(
                            "sess-1".to_string(),
                            Msg::new(
                                Role::User,
                                "user",
                                vec![ContentBlock::Text { text: steer.to_string() }],
                            ),
                        );
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
        assert!(
            second_call.iter().any(|t| t.contains("插队：改跑两次")),
            "steer 消息应注入下一轮模型输入: {second_call:?}"
        );
        let updates: Vec<&Value> = notifications.iter().map(update_of).collect();
        assert!(
            updates
                .iter()
                .any(|u| u["sessionUpdate"] == "tool_call_update" && u["status"] == "completed"),
            "插队不应影响工具完成: {updates:?}"
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

    // AgentEvent 构造引用（防 import 漂移）。
    #[allow(dead_code)]
    fn event_type_is_stable(event: &AgentEvent) -> &str {
        &event.id
    }
}
