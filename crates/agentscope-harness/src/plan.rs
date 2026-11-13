//! 计划模式：进入/退出与计划文件写入（对应 Java `workspace.plan.PlanModeManager`），
//! 以及运行中拦截中间件（对应 Java `middleware.PlanModeMiddleware`）。
//!
//! 与 Java 一致：计划文件路径记录在 `AgentState.plan_mode_context.current_plan_file`
//! （随会话状态持久化），默认 `{plan_dir}/spec.md`；`enter`/`exit` 只改状态标志
//! （幂等），文件写入走 `write_plan`。计划模式是「只读设计阶段」——运行中的
//! 写操作由 [`PlanModeMiddleware`] 在 `on_acting` 确定性拦截：只放行只读工具与
//! 白名单，被拒调用合成 `Denied` 工具结果写进上下文（模型下一轮可见）。

use std::sync::{Arc, Mutex};

use agentscope_core::context::RuntimeContext;
use agentscope_core::message::{Msg, ToolResultState};
use agentscope_core::middleware::{ActingInput, Middleware, ReasoningInput};
use agentscope_core::state::{AgentState, PlanModeContextState};

use crate::workspace::WorkspaceManager;

/// 默认计划目录（工作区相对）。
pub const DEFAULT_PLAN_DIR: &str = "plans";

/// 计划模式管理器。
pub struct PlanModeManager {
    plan_dir: String,
}

impl PlanModeManager {
    /// 默认 `plans/` 目录。
    pub fn new() -> Self {
        Self {
            plan_dir: DEFAULT_PLAN_DIR.to_string(),
        }
    }

    /// 自定义计划目录。
    pub fn with_dir(plan_dir: impl Into<String>) -> Self {
        Self {
            plan_dir: normalize_dir(plan_dir),
        }
    }

    /// 计划目录（工作区相对）。
    pub fn plan_dir(&self) -> &str {
        &self.plan_dir
    }

    /// 会话是否处于计划模式（`state` 为 `None` 视为未进入）。
    pub fn is_plan_active(&self, state: Option<&AgentState>) -> bool {
        state.is_some_and(|state| state.plan_mode_context.plan_active)
    }

    /// 进入计划模式（幂等）：置位标志，未记录计划文件时补默认路径。
    /// 返回当前计划文件的工作区相对路径。
    pub fn enter(&self, state: &mut AgentState) -> String {
        let context: &mut PlanModeContextState = &mut state.plan_mode_context;
        context.plan_active = true;
        if context
            .current_plan_file
            .as_deref()
            .is_none_or(|path| path.trim().is_empty())
        {
            context.current_plan_file = Some(self.default_plan_file());
        }
        context
            .current_plan_file
            .clone()
            .unwrap_or_else(|| self.default_plan_file())
    }

    /// 退出计划模式（回到 BUILD，幂等）：仅清标志，保留 `current_plan_file` 供参考。
    pub fn exit(&self, state: &mut AgentState) {
        state.plan_mode_context.plan_active = false;
        // 清掉计划文件路径：Java 的 exit 保留 currentPlanFile 供 BUILD 态
        // 提示（BUILD_MODE_PLAN_HINT）引用；studio 产品语义下用户不要
        // 「已退出 plan 模式还看到 plan 提示」的残留——计划内容已在对话
        // 上下文里，这里清掉让系统提示回归干净（与 Java 的有意差异）。
        state.plan_mode_context.current_plan_file = None;
    }

    /// 当前会话关联的计划文件路径（未记录时返回默认路径）。
    pub fn plan_file_path(&self, state: Option<&AgentState>) -> String {
        state
            .and_then(|state| state.plan_mode_context.current_plan_file.clone())
            .filter(|path| !path.trim().is_empty())
            .unwrap_or_else(|| self.default_plan_file())
    }

    /// 写入（创建或覆写）计划 markdown 文件，并把路径记录进会话状态。
    /// 返回实际写入的工作区相对路径。
    pub async fn write_plan(
        &self,
        workspace: &WorkspaceManager,
        rc: &RuntimeContext,
        state: &mut AgentState,
        content: &str,
    ) -> Result<String, String> {
        let path = self.plan_file_path(Some(state));
        workspace
            .write_utf8_workspace_relative(rc, &path, content)
            .await?;
        state.plan_mode_context.current_plan_file = Some(path.clone());
        Ok(path)
    }

    /// 按需求分目录的 spec 路径（spec 模式）：`{plan_dir}/{feature}/spec.md`。
    /// 每个需求一个目录，装该需求的完整文档集（spec.md + tasks.md），
    /// 归档时整目录移入 archived/。
    pub fn feature_plan_file(&self, feature: &str) -> String {
        let feature = feature.trim();
        if feature.is_empty() {
            return self.default_plan_file();
        }
        // 防路径穿越：只保留路径分量字符，拒绝分隔符与 ..
        let safe: String = feature
            .chars()
            .filter(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
            .collect();
        if safe.is_empty() || safe.contains("..") {
            return self.default_plan_file();
        }
        format!("{}/{}/spec.md", self.plan_dir.trim_end_matches('/'), safe)
    }

    fn default_plan_file(&self) -> String {
        format!("{}/spec.md", self.plan_dir.trim_end_matches('/'))
    }
}

fn normalize_dir(dir: impl Into<String>) -> String {
    let dir: String = dir.into();
    let trimmed = dir.trim();
    if trimmed.is_empty() {
        DEFAULT_PLAN_DIR.to_string()
    } else {
        trimmed.to_string()
    }
}

impl Default for PlanModeManager {
    fn default() -> Self {
        Self::new()
    }
}

// ===========================================================================
// PlanModeMiddleware（对应 Java `middleware.PlanModeMiddleware`）
// ===========================================================================

/// plan 模式下始终放行的工具（对齐 Java `ALWAYS_ALLOWED`：plan 自身三件套 +
/// todo + 子代理/任务工具——它们是计划与投递语义的一部分，无副作用）。
const PLAN_ALWAYS_ALLOWED: &[&str] = &[
    "plan_write",
    "plan_exit",
    "todo_write",
    "agent_spawn",
    "agent_send",
    "agent_list",
    "task_output",
    "task_list",
];

/// 追踪的并发调用状态句柄上限（同 `TaskReminderMiddleware` 的防增长策略）。
const PLAN_MAX_TRACKED_CALLS: usize = 64;

/// 计划模式中间件：
/// - `on_acting`：plan 激活时只放行只读工具与白名单；被拒调用合成 `Denied`
///   工具结果写进上下文（对齐 Java 的 DENIED ToolResult——模型下一轮可见，
///   tool_use/tool_result 配对完整，不会打爆 API）；
/// - `on_system_prompt`：plan 激活注入指令横幅（ACT 必须调工具/信息不足就问/
///   写完 plan 提交评审）；BUILD 态有计划文件时注入「路径≠批准证据」提示；
/// - `on_reasoning`：注入 `<RUNTIME_STATE>` 计划状态尾消息（每次模型调用重建，
///   不进上下文——与 SubagentsMiddleware 的易变状态同一模式）。
///
/// 状态句柄经 `on_agent_state_ready` 登记（同 `TaskReminderMiddleware` 的
/// remember/state_for 模式，按 reply_id 寻址）。
/// plan 态默认 banner 正文（装配方未自定义 `plan_prompt` 时使用）。
const DEFAULT_PLAN_BANNER: &str = "PLAN MODE ACTIVE: research and design only — \
do not execute changes.
- In each ACT step you MUST call a tool (read files, search, plan_write); do not \
narrate an action without calling.
- If required information is missing, ask the user instead of guessing.
- Write the plan with plan_write, then present it for review.";

pub struct PlanModeMiddleware {
    is_read_only: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    additional_allowed: Vec<String>,
    /// 进入 plan 态时注入的场景提示词（装配方自定义，如 studio 的 solo:plan
    /// 场景层；`None` = 内置默认 banner）。plan 文件行始终由中间件追加。
    plan_prompt: Option<String>,
    state_handles: Mutex<Vec<(String, Arc<Mutex<AgentState>>)>>,
}

impl PlanModeMiddleware {
    /// 装配：管理器 + 只读判定谓词（装配方从 toolkit 构建，见
    /// `HarnessAgentBuilder::build`）+ 额外放行名单（Java `additionalAllowed`，
    /// `planModeAllowShell` 等价于把 shell 工具名加进此名单）。
    pub fn new(
        is_read_only: Arc<dyn Fn(&str) -> bool + Send + Sync>,
        additional_allowed: Vec<String>,
    ) -> Self {
        Self {
            is_read_only,
            additional_allowed,
            plan_prompt: None,
            state_handles: Mutex::new(Vec::new()),
        }
    }

    /// 自定义 plan 态场景提示词（替换内置 banner 正文；`<plan-mode>` 包裹与
    /// 计划文件行仍由中间件统一处理）。
    pub fn with_plan_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.plan_prompt = Some(prompt.into());
        self
    }

    /// 登记本次调用的状态句柄（同 reply_id 重复登记覆盖，超上限淘汰最旧）。
    fn remember_state(&self, reply_id: &str, state: &Arc<Mutex<AgentState>>) {
        let mut handles = self
            .state_handles
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        handles.retain(|(id, _)| id != reply_id);
        handles.push((reply_id.to_string(), Arc::clone(state)));
        while handles.len() > PLAN_MAX_TRACKED_CALLS {
            handles.remove(0);
        }
    }

    /// 取本次调用的状态句柄。先克隆 Arc 再出锁——绝不在持锁期间进入另一个
    /// lock（临时 MutexGuard 存活到语句结束的自死锁坑，项目已多次踩中）。
    fn state_for(&self, reply_id: &str) -> Option<Arc<Mutex<AgentState>>> {
        self.state_handles
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .find(|(id, _)| id == reply_id)
            .map(|(_, handle)| Arc::clone(handle))
    }

    /// 非 plan 态下短路 `plan_exit` 调用。典型场景：计划批准后用户反馈修改
    /// 意见，模型想「更新计划并重新提交批准」却已不在 plan 态（plan_write
    /// 不可用），转而误调 plan_exit——而 PlanExitTool 的 check_permissions
    /// 无条件 RequireApproval，每次误调都会弹批准骚扰用户。这里在执行前
    /// 摘除调用并合成 Denied 结果（与 plan 态 denied 分支同模式，保持
    /// tool_use/tool_result 配对），不执行、不触发权限确认，同时教育模型
    /// 正确路径。`plan_status` 为 `Some` 蕴含 `state_for` 为 `Some`（同一
    /// 句柄），摘除必然伴随写结果，不会留下悬空 tool_use。
    fn short_circuit_plan_exit_off_mode(
        &self,
        ctx: &RuntimeContext,
        input: ActingInput,
    ) -> ActingInput {
        if !input
            .tool_calls
            .iter()
            .any(|call| call.name == super::tools::PlanExitTool::NAME)
        {
            return input;
        }
        let Some(handle) = self.state_for(ctx.reply_id()) else {
            return input;
        };
        let (exits, rest): (Vec<_>, Vec<_>) = input
            .tool_calls
            .into_iter()
            .partition(|call| call.name == super::tools::PlanExitTool::NAME);
        let mut state = handle.lock().unwrap_or_else(|error| error.into_inner());
        for call in exits {
            state.context.push(Msg::tool_result(
                "plan-mode",
                call.id.clone(),
                call.name.clone(),
                "[plan mode] Not in plan mode; nothing to exit. To revise the plan, edit \
the plan file directly with file tools and keep executing, or ask the user to \
switch to plan mode."
                    .to_owned(),
                ToolResultState::Denied,
            ));
        }
        ActingInput { tool_calls: rest }
    }

    /// 当前计划状态：`(plan_active, current_plan_file)`。无状态句柄 → `None`
    /// （调用方透传——中间件不参与无状态调用）。
    fn plan_status(&self, reply_id: &str) -> Option<(bool, Option<String>)> {
        let handle = self.state_for(reply_id)?;
        let state = handle.lock().unwrap_or_else(|error| error.into_inner());
        Some((
            state.plan_mode_context.plan_active,
            state.plan_mode_context.current_plan_file.clone(),
        ))
    }
}

impl Middleware for PlanModeMiddleware {
    fn name(&self) -> &str {
        "plan-mode"
    }

    fn on_agent_state_ready(
        &self,
        ctx: &RuntimeContext,
        _input_msgs: &mut Vec<Msg>,
        state: &Arc<Mutex<AgentState>>,
    ) {
        self.remember_state(ctx.reply_id(), state);
    }

    fn on_acting(&self, ctx: &RuntimeContext, input: ActingInput) -> ActingInput {
        let Some((active, _)) = self.plan_status(ctx.reply_id()) else {
            return input;
        };
        if !active {
            return self.short_circuit_plan_exit_off_mode(ctx, input);
        }
        let (allowed, denied): (Vec<_>, Vec<_>) = input.tool_calls.into_iter().partition(|call| {
            (self.is_read_only)(&call.name)
                || PLAN_ALWAYS_ALLOWED.contains(&call.name.as_str())
                || self.additional_allowed.iter().any(|a| a == &call.name)
        });
        if !denied.is_empty() {
            if let Some(handle) = self.state_for(ctx.reply_id()) {
                let mut state = handle.lock().unwrap_or_else(|error| error.into_inner());
                for call in denied {
                    state.context.push(Msg::tool_result(
                        "plan-mode",
                        call.id.clone(),
                        call.name.clone(),
                        format!(
                            "[plan mode] tool `{}` was denied: plan mode is read-only research \
and design. Record the intended action in the plan file instead, or ask the user.",
                            call.name
                        ),
                        ToolResultState::Denied,
                    ));
                }
            }
        }
        ActingInput { tool_calls: allowed }
    }

    fn on_system_prompt(&self, ctx: &RuntimeContext, current: String) -> String {
        let Some((active, plan_file)) = self.plan_status(ctx.reply_id()) else {
            return current;
        };
        if active {
            let body = self
                .plan_prompt
                .clone()
                .unwrap_or_else(|| DEFAULT_PLAN_BANNER.to_string());
            let mut banner = format!("{current}\n\n<plan-mode>\n{body}");
            if let Some(file) = plan_file {
                banner.push_str(&format!("\n- Plan file: {file}"));
            }
            banner.push_str("\n</plan-mode>");
            banner
        } else if let Some(file) = plan_file {
            // BUILD 态：已有计划文件 → 逐步执行提示（对齐 Java BUILD_MODE_PLAN_HINT）。
            format!(
                "{current}\n\n<plan-mode-hint>\nAn approved plan exists at {file}. The plan \
path is NOT approval evidence by itself — read it and execute it step by step.\n</plan-mode-hint>"
            )
        } else {
            current
        }
    }

    fn on_reasoning(&self, ctx: &RuntimeContext, input: ReasoningInput) -> ReasoningInput {
        let Some((active, plan_file)) = self.plan_status(ctx.reply_id()) else {
            return input;
        };
        let mode = if active {
            "ACTIVE (PLAN — read-only design phase; code changes are blocked)"
        } else {
            "OFF (execution mode — plan_write is unavailable; do not call plan tools)"
        };
        let mut sb = String::from("<RUNTIME_STATE>\n");
        sb.push_str(&format!("Plan mode: {mode}\n"));
        match &plan_file {
            Some(file) => {
                sb.push_str(&format!("Plan reference: {file}\n"));
                sb.push_str(
                    "The plan file is a design record, not approval evidence; execute it \
step by step after leaving plan mode.\n",
                );
            }
            None => sb.push_str("No plan file recorded.\n"),
        }
        sb.push_str("</RUNTIME_STATE>");
        let mut messages = input.messages;
        messages.push(Msg::system("system", sb));
        ReasoningInput {
            messages,
            options: input.options,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_workspace() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "harness-plan-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn plan_enter_exit_is_idempotent_and_records_path() {
        let plans = PlanModeManager::new();
        let mut state = AgentState::with_session_id("sess-9");

        assert!(!plans.is_plan_active(Some(&state)));
        let path = plans.enter(&mut state);
        assert_eq!(path, "plans/spec.md");
        assert!(plans.is_plan_active(Some(&state)));
        assert_eq!(plans.plan_file_path(Some(&state)), "plans/spec.md");

        // 幂等：再次 enter 不改路径
        assert_eq!(plans.enter(&mut state), "plans/spec.md");

        // exit 只清标志，保留路径
        plans.exit(&mut state);
        assert!(!plans.is_plan_active(Some(&state)));
        assert_eq!(plans.plan_file_path(Some(&state)), "plans/spec.md");

        // None 状态视为未进入
        assert!(!plans.is_plan_active(None));
        assert_eq!(plans.plan_file_path(None), "plans/spec.md");
    }

    #[test]
    fn plan_enter_keeps_existing_custom_path() {
        let plans = PlanModeManager::with_dir("design");
        let mut state = AgentState::with_session_id("sess-1");
        state.plan_mode_context.current_plan_file = Some("design/custom.md".to_string());
        assert_eq!(plans.enter(&mut state), "design/custom.md");
        // 空目录名回落默认
        assert_eq!(PlanModeManager::with_dir("  ").plan_dir(), "plans");
    }

    #[tokio::test]
    async fn write_plan_writes_file_and_records_path() {
        let root = temp_workspace();
        let manager = WorkspaceManager::new(&root);
        let plans = PlanModeManager::new();
        let rc = RuntimeContext::new().with_session_id("sess-9");
        let mut state = AgentState::with_session_id("sess-9");

        plans.enter(&mut state);
        let path = plans
            .write_plan(&manager, &rc, &mut state, "# updated plan")
            .await
            .unwrap();
        assert_eq!(path, "plans/spec.md");
        assert_eq!(
            manager
                .read_managed_workspace_file_utf8(&rc, "plans/spec.md")
                .await,
            "# updated plan"
        );
        assert_eq!(
            state.plan_mode_context.current_plan_file.as_deref(),
            Some("plans/spec.md")
        );
        std::fs::remove_dir_all(&root).ok();
    }

    // ---------- PlanModeMiddleware ----------

    use agentscope_core::middleware::ActingInput;
    use agentscope_core::model::GenerateOptions;
    use agentscope_core::message::ContentBlock;
    use agentscope_core::middleware::ToolCallSpec;
    use serde_json::json;

    fn middleware_with_state(
        state: &Arc<Mutex<AgentState>>,
        reply_id: &str,
    ) -> PlanModeMiddleware {
        let is_ro: Arc<dyn Fn(&str) -> bool + Send + Sync> = Arc::new(|n| n == "read_file");
        let mw = PlanModeMiddleware::new(is_ro, vec![]);
        mw.remember_state(reply_id, state);
        mw
    }

    #[tokio::test]
    async fn plan_middleware_blocks_writes_and_injects_state() {
        let manager = Arc::new(PlanModeManager::new());
        let state = Arc::new(Mutex::new(AgentState::default()));
        let mw = middleware_with_state(&state, "r1");
        manager.enter(&mut state.lock().unwrap());
        let ctx = RuntimeContext::new().with_reply_id("r1");

        // on_acting：只读放行、写工具拦截并合成 Denied 结果进上下文。
        let input = ActingInput {
            tool_calls: vec![
                ToolCallSpec { id: "1".into(), name: "read_file".into(), input: json!({}) },
                ToolCallSpec { id: "2".into(), name: "write_file".into(), input: json!({}) },
            ],
        };
        let out = mw.on_acting(&ctx, input);
        assert_eq!(out.tool_calls.len(), 1);
        assert_eq!(out.tool_calls[0].name, "read_file");
        {
            let s = state.lock().unwrap();
            assert_eq!(s.context.len(), 1, "被拒调用应有合成的 Denied 结果");
            match &s.context[0].content[0] {
                ContentBlock::ToolResult { name, output, state, .. } => {
                    assert_eq!(name, "write_file");
                    assert!(matches!(state, ToolResultState::Denied));
                    assert!(format!("{output:?}").contains("denied"));
                }
                other => panic!("expected tool result block, got {other:?}"),
            }
        }

        // on_reasoning：PLAN 态注入 RUNTIME_STATE 尾消息。
        let ri = mw.on_reasoning(
            &ctx,
            ReasoningInput { messages: vec![], options: GenerateOptions::default() },
        );
        let last = ri.messages.last().unwrap().text();
        assert!(last.contains("Plan mode: ACTIVE"), "{last}");

        // BUILD 态（exit 后）：不再拦截，状态消息切 BUILD。
        manager.exit(&mut state.lock().unwrap());
        let input = ActingInput {
            tool_calls: vec![ToolCallSpec {
                id: "3".into(),
                name: "write_file".into(),
                input: json!({}),
            }],
        };
        let out = mw.on_acting(&ctx, input);
        assert_eq!(out.tool_calls.len(), 1, "BUILD 态透传");
        let ri = mw.on_reasoning(
            &ctx,
            ReasoningInput { messages: vec![], options: GenerateOptions::default() },
        );
        assert!(ri.messages.last().unwrap().text().contains("Plan mode: OFF"));
    }

    #[tokio::test]
    async fn plan_middleware_short_circuits_plan_exit_when_off() {
        // BUILD 态（计划已批准退出）下模型误调 plan_exit：摘除调用、合成
        // Denied 结果（不执行、不触发 RequireApproval 弹批准），其余放行。
        let state = Arc::new(Mutex::new(AgentState::default()));
        let mw = middleware_with_state(&state, "r2");
        let ctx = RuntimeContext::new().with_reply_id("r2");

        let input = ActingInput {
            tool_calls: vec![
                ToolCallSpec { id: "1".into(), name: "read_file".into(), input: json!({}) },
                ToolCallSpec { id: "2".into(), name: "plan_exit".into(), input: json!({}) },
            ],
        };
        let out = mw.on_acting(&ctx, input);
        assert_eq!(out.tool_calls.len(), 1, "plan_exit 应被摘除");
        assert_eq!(out.tool_calls[0].name, "read_file");
        {
            let s = state.lock().unwrap();
            assert_eq!(s.context.len(), 1, "误调应有合成的 Denied 结果");
            match &s.context[0].content[0] {
                ContentBlock::ToolResult { name, output, state, .. } => {
                    assert_eq!(name, "plan_exit");
                    assert!(matches!(state, ToolResultState::Denied));
                    assert!(format!("{output:?}").contains("Not in plan mode"));
                }
                other => panic!("expected tool result block, got {other:?}"),
            }
        }

        // 无 plan_exit 调用时透传，不产生合成结果。
        let plain = ActingInput {
            tool_calls: vec![ToolCallSpec {
                id: "3".into(),
                name: "read_file".into(),
                input: json!({}),
            }],
        };
        let out2 = mw.on_acting(&ctx, plain);
        assert_eq!(out2.tool_calls.len(), 1);
        {
            let s = state.lock().unwrap();
            assert_eq!(s.context.len(), 1, "无误调不应新增结果");
        }
    }

    #[tokio::test]
    async fn plan_middleware_passthrough_without_state() {
        // 无状态句柄（未经 on_agent_state_ready）→ 全部透传。
        let is_ro: Arc<dyn Fn(&str) -> bool + Send + Sync> = Arc::new(|_| false);
        let mw = PlanModeMiddleware::new(is_ro, vec![]);
        let ctx = RuntimeContext::new().with_reply_id("nope");
        let input = ActingInput {
            tool_calls: vec![ToolCallSpec {
                id: "1".into(),
                name: "write_file".into(),
                input: json!({}),
            }],
        };
        let out = mw.on_acting(&ctx, input);
        assert_eq!(out.tool_calls.len(), 1);
    }
}
