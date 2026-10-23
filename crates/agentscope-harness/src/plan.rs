//! 计划模式：进入/退出与计划文件写入（对应 Java `workspace.plan.PlanModeManager`）。
//!
//! 与 Java 一致：计划文件路径记录在 `AgentState.plan_mode_context.current_plan_file`
//! （随会话状态持久化），默认 `{plan_dir}/PLAN.md`；`enter`/`exit` 只改状态标志
//! （幂等），文件写入走 `write_plan`。计划模式是「只读设计阶段」——运行中的
//! 写操作拦截由 `PlanModeMiddleware` 负责（未复刻，见 lib.rs 差异声明）。

use agentscope_core::context::RuntimeContext;
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

    fn default_plan_file(&self) -> String {
        format!("{}/PLAN.md", self.plan_dir.trim_end_matches('/'))
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
        assert_eq!(path, "plans/PLAN.md");
        assert!(plans.is_plan_active(Some(&state)));
        assert_eq!(plans.plan_file_path(Some(&state)), "plans/PLAN.md");

        // 幂等：再次 enter 不改路径
        assert_eq!(plans.enter(&mut state), "plans/PLAN.md");

        // exit 只清标志，保留路径
        plans.exit(&mut state);
        assert!(!plans.is_plan_active(Some(&state)));
        assert_eq!(plans.plan_file_path(Some(&state)), "plans/PLAN.md");

        // None 状态视为未进入
        assert!(!plans.is_plan_active(None));
        assert_eq!(plans.plan_file_path(None), "plans/PLAN.md");
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
        assert_eq!(path, "plans/PLAN.md");
        assert_eq!(
            manager
                .read_managed_workspace_file_utf8(&rc, "plans/PLAN.md")
                .await,
            "# updated plan"
        );
        assert_eq!(
            state.plan_mode_context.current_plan_file.as_deref(),
            Some("plans/PLAN.md")
        );
        std::fs::remove_dir_all(&root).ok();
    }
}
