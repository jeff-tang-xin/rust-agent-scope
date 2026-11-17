//! UI action dispatch: the agent runs registered interface actions by their stable
//! actionId — the same registry behind keybindings, menus and the command palette —
//! instead of simulating keystrokes. Execution happens in the project window
//! (`McpUiRequest::ExecuteAction` round trip) with visible UI feedback; `when`
//! context gating is evaluated window-side, and unknown ids answer with near-match
//! suggestions so the agent can self-correct.

use crate::mcp::tool::{Call, Output, Param, ParamType, Tool};
use ide_core::{IdeError, event::McpUiRequest};

pub fn tools() -> Vec<Tool> {
    vec![Tool::new(
        "ide_action",
        "Run a registered IDE UI action by its stable id — the same registry behind keybindings, menus and the command palette (e.g. `navigate.gotoFile`, `toolwindow.terminal`, `code.reformat`). Use it for interface operations the semantic tools don't cover (open panels, jump, refactor menus, settings); prefer the semantic tools for code search/edit/build/run/debug/git, and never simulate keystrokes. The action runs in the project window with visible UI feedback; an action gated by a `when` clause fails with an error in the current context, and unknown ids return near-match suggestions. Requires user confirmation.",
        |call: Call| async move {
            let action_id = call.str("actionId")?;
            if action_id.trim().is_empty() {
                return Err(IdeError::invalid("actionId must not be empty"));
            }
            let arg = call.opt_str("arg")?;
            call.confirm(&format!("action `{action_id}`"), arg.clone()).await?;
            call.ask(McpUiRequest::ExecuteAction { action_id: action_id.clone(), arg }).await?;
            Ok(Output::text(format!("action `{action_id}` executed")))
        },
    )
    .write()
    .param(Param::required(
        "actionId",
        ParamType::String,
        "Stable action id, e.g. `run.debug`, `navigate.gotoSymbol`, `toolwindow.project`. Ids are keymap-independent; the oxidea-usage skill lists the high-frequency catalog.",
    ))
    .param(Param::optional(
        "arg",
        ParamType::String,
        "JSON text argument for actions that take one; omit when unsure.",
    ))]
}

#[cfg(test)]
mod tests {
    /// The agent permission layer keys off `read_only`: action dispatch can change
    /// files/state, so it must stay a write tool (HITL + trust gating).
    #[test]
    fn ide_action_is_a_write_tool_with_required_action_id() {
        let tools = super::tools();
        let tool = tools.iter().find(|t| t.metadata.name == "ide_action").unwrap();
        assert!(!tool.metadata.read_only);
        let required = tool.metadata.input_schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "actionId"));
    }
}
