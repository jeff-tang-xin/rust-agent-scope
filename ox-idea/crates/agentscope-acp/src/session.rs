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
use agentscope_core::message::ToolResultState;
use agentscope_core::{ContentBlock, Model, Msg, RuntimeContext, Toolkit};
use agentscope_harness::agent::{HarnessAgent, HarnessAgentBuilder};
use agentscope_harness::middleware_memory::{MemoryFlushMiddleware, MemoryMaintenanceMiddleware};
use agentscope_harness::tools::{LocalShellBackend, ShellExecuteTool};
use agentscope_harness::tools_filesystem::FilesystemToolSet;
use agentscope_harness::tools_memory::MemoryToolSet;
use agentscope_harness::workspace::WorkspaceManager;
use agentscope_harness::MemoryConfig;
use agentscope_harness::{Filesystem, LocalFilesystem, LocalFsMode, PathPolicy};
use serde_json::{json, Value};

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
        self.sessions.lock().unwrap().insert(
            session_id.clone(),
            Arc::new(AcpSession {
                agent,
                cwd: PathBuf::from(&cwd),
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
        let (session_id, text) = protocol::parse_prompt(&params)?;
        let session = match self.sessions.lock().unwrap().get(&session_id).cloned() {
            Some(session) => session,
            None => {
                return Err(RpcError::invalid_params(format!(
                    "未知 sessionId: {}",
                    session_id
                )))
            }
        };
        let outcome = drive_turn(&session, &session_id, text, conn).await;
        crate::prompt_response(outcome)
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
    let mut toolkit = Toolkit::new();
    for tool in FilesystemToolSet::new(fs, None).all() {
        toolkit.register(tool);
    }
    toolkit.register(Arc::new(ShellExecuteTool::new(Arc::new(
        LocalShellBackend::new_anchored(PathBuf::from(cwd)),
    ))));
    // IDE 宿主注入：语义工具（进程内直调）+ 记忆锚点（`<项目根>/.oxidea/agent`，跨 worktree 共享）。
    let host_ctx = crate::ox_context_for(Path::new(cwd));
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
    // 记忆管线（仅宿主注入 memory_root 时启用）：memory_save/get/search 工具 +
    // flush（Layer1 每日台账）与 maintenance（Layer2 MEMORY.md 合并）两个中间件。
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
    let memory_section = if memory_ws.is_some() {
        "\n## 记忆\n\
         - 你有两层长期记忆：MEMORY.md（精炼的长期知识）与 memory/ 每日台账。\n\
         - 用 memory_save 沉淀值得跨会话保留的结论（用户偏好、项目约定、架构决策、踩坑记录）。\n\
         - 用 memory_search / memory_get 回忆过往结论，避免重复劳动与已知坑。\n"
            .to_owned()
    } else {
        String::new()
    };
    let sys_prompt = format!(
        "你是 OX Hareness，OXidea 内置的资深全栈工程师智能体——为真实工程项目而生：读得懂有历史包袱的代码库，改得动有约束的生产代码，对交付质量负责。\n\n\
         ## 环境\n\
         - 你运行在用户的 IDE（OXidea）中，用户通过 IDE 的智能体面板与你对话。\n\
         - 当前工作目录：{cwd}——这是你的工作区根，所有文件操作与命令都在此执行。\n\n\
         ## 基础工具\n\
         - 文件工具：读取、写入、搜索、列出工作区内的文件；一切路径限制在工作区内。\n\
         - Shell 工具：在当前工作目录执行非交互、有界的命令（编译、测试、git 等）。\n\
         {ide_section}{memory_section}\n\
         ## 分析方法\n\
         - 理解先于动作：改代码前先读相关文件，理清调用链与既有约定，不凭猜测下手。\n\
         - 追链路：从一个符号出发，用引用查找看谁在调它、用符号信息看类型与签名，沿调用链评估改动影响面，再决定改哪里。\n\
         - 事实与推测分开：读过代码得出的是事实，没验证过的是推断；表述时区分两者，不确定就说不确定。\n\n\
         ## 纪律\n\
         - 外科手术式修改：只改必须改的地方，沿用项目既有风格（命名、分层、错误处理），不顺手重构无关代码。\n\
         - 自我闭环：改完自行验证（编译、测试、关键路径检查），确认可用再汇报；「编译一下」意味着执行、看输出、修问题、报结果，不是跑完命令就停。\n\
         - 失败止损：同一手段连续两次不奏效就停下，说明卡在哪、报什么错、试过什么、你的判断——不要换着花样瞎试。\n\
         - 安全边界：不碰工作区外的路径；删除、覆盖、推送等不可逆动作先说明影响范围并征得同意；密钥与凭证绝不写进文件或打印。\n\n\
         ## 回复\n\
         - 简洁直接，用 Markdown；涉及代码时给完整可运行的片段。\n\
         - 汇报包含：做了什么、怎么验证的、还剩什么风险；部分完成时明确说明哪些完成、哪些没有。\n\
         - 与用户同语言交流：用户用中文就用中文，用英文就用英文。"
    );
    let mut builder = HarnessAgentBuilder::new()
        .name("ox-hareness")
        .sys_prompt(sys_prompt)
        .model(Arc::clone(&model))
        .toolkit(toolkit)
        .workspace(PathBuf::from(cwd))
        // ReAct 迭代上限（一轮 = 一次模型调用 + 工具执行）：`AGENTSCOPE_MAX_ITERS`
        // 可配（设置界面），缺省 100——真实工程任务 30 轮经常不够，跑到一半被截断。
        .max_iters(max_iters_from(env));
    if let Some(ws) = &memory_ws {
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
async fn drive_turn<W>(
    session: &AcpSession,
    session_id: &str,
    text: String,
    conn: &RpcConn<W>,
) -> PromptOutcome
where
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // 首条：用户消息；权限往返后：确认答复消息（带 agentscope_confirm_results
    // 元数据，agent 侧 apply 后续跑，不会重放成聊天内容）。
    let mut msg = Msg::user("user", text);
    loop {
        let runtime = RuntimeContext::new().with_session_id(session_id.to_owned());
        let mut rx = match session.agent.stream_events(runtime, msg.clone()).await {
            Ok(rx) => rx,
            Err(error) => return PromptOutcome::Error(error.to_string()),
        };
        let mut confirm_reply_id: Option<String> = None;
        let mut confirm_results: Vec<ConfirmResult> = Vec::new();
        let mut outcome: Option<PromptOutcome> = None;
        while let Some(event) = rx.recv().await {
            match event.kind {
                AgentEventKind::TextBlockDelta { delta, .. } => {
                    conn.notify(
                        "session/update",
                        protocol::agent_message_chunk(session_id, &delta),
                    )
                    .await;
                }
                AgentEventKind::ThinkingBlockDelta { delta, .. } => {
                    conn.notify(
                        "session/update",
                        protocol::agent_thought_chunk(session_id, &delta),
                    )
                    .await;
                }
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
        match (outcome, confirm_reply_id) {
            // 事件流自然结束：本轮终局。
            (Some(outcome), _) => return outcome,
            // 事件流在权限挂起后结束（agent ASKING 挂起返回）：回灌确认结果续跑。
            (None, Some(reply_id)) => {
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
    }

    impl MockModel {
        fn new(responses: Vec<ChatResponse>) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(responses),
            })
        }
    }

    #[async_trait::async_trait]
    impl Model for MockModel {
        async fn call(
            &self,
            _request: agentscope_core::ChatRequest,
        ) -> Result<ChatResponse, Error> {
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
    ) -> Arc<HarnessAgent> {
        let mut toolkit = Toolkit::new();
        if with_echo {
            toolkit.register(Arc::new(EchoTool));
        }
        let mut builder = HarnessAgentBuilder::new()
            .name("test")
            .sys_prompt("test")
            .model(MockModel::new(responses))
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
        Arc::new(builder.build().expect("build agent"))
    }

    /// 模拟 ACP client 端：读 agent 发出的帧；遇到 request_permission
    /// 就回指定 optionId 的响应；收集全部通知供断言。
    /// 同时起读循环消费 server 端入站帧（响应帧 → resolve_pending 配对）。
    /// 返回 (收集的通知帧列表, drive 终局)。
    async fn run_drive_turn(
        agent: Arc<HarnessAgent>,
        text: &str,
        grant_option: Option<&str>,
    ) -> (Vec<Value>, PromptOutcome) {
        let (client, server) = tokio::io::duplex(256 * 1024);
        let (server_read, server_write) = tokio::io::split(server);
        let conn = Arc::new(RpcConn::new(server_write));
        let (mut client_read, mut client_write) = tokio::io::split(client);

        let session = AcpSession {
            agent,
            cwd: PathBuf::from("."),
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

        let drive = drive_turn(&session, "sess-1", text.to_string(), conn.as_ref());

        // client 端循环：读帧 → request_permission 时回响应。
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
        let agent = build_agent(vec![ChatResponse::from_text("你好，世界")], false, false);
        let (notifications, outcome) = run_drive_turn(agent, "打个招呼", None).await;
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
        let agent = build_agent(
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
        let (notifications, outcome) = run_drive_turn(agent, "echo 一下", None).await;
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
        let agent = build_agent(
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
        let (notifications, outcome) = run_drive_turn(agent, "跑 echo", Some("allow_once")).await;
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
        let agent = build_agent(
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
        let (notifications, outcome) = run_drive_turn(agent, "跑 echo", Some("reject_once")).await;
        assert_eq!(outcome, PromptOutcome::EndTurn);
        let updates: Vec<&Value> = notifications.iter().map(update_of).collect();
        assert!(
            updates
                .iter()
                .any(|u| u["sessionUpdate"] == "tool_call_update" && u["status"] == "failed"),
            "拒绝后工具应标记失败: {updates:?}"
        );
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
