//! team 模式的成员委派工具（≈ Java `LocalTeamClient` 的进程内委派语义）。
//!
//! Java harness 的 team 体系里，lead 通过 TeamClient 把任务派给成员、成员在
//! 自己的会话里执行并回传结果（TeamContext 注入角色与目标）。Rust harness 的
//! subagent spawn 工具尚未接线，因此本应用层工具承担同一职责：
//! - leader 模型调用 `team_delegate`，指定成员与任务；
//! - 工具在进程内直接驱动成员 agent 的 ReAct 循环（`call_with_context`）；
//! - 成员会话固定（`team-<member>`），跨 turn 持久——成员有记忆，对齐
//!   TeamContext 的成员会话语义。

use std::collections::HashMap;
use std::sync::Arc;

use agentscope_core::error::Result;
use agentscope_core::tool::{Tool, ToolOutput};
use agentscope_core::{Msg, RuntimeContext};
use agentscope_harness::HarnessAgent;
use serde_json::{json, Value};

/// 成员表：成员名 → 成员 agent（装配期固定）。
pub type MemberRegistry = Arc<HashMap<String, Arc<HarnessAgent>>>;

pub struct TeamDelegateTool {
    members: MemberRegistry,
}

impl TeamDelegateTool {
    pub fn new(members: MemberRegistry) -> Self {
        Self { members }
    }
}

#[async_trait::async_trait]
impl Tool for TeamDelegateTool {
    fn name(&self) -> &str {
        "team_delegate"
    }

    fn description(&self) -> &str {
        "Delegate a task to a team member and block until the member finishes. \
         Use this to hand off self-contained work; the member runs in its own \
         session with its own tools and returns its final answer as text."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "member": {
                    "type": "string",
                    "description": "Exact member name to delegate to."
                },
                "task": {
                    "type": "string",
                    "description": "Self-contained task description with all context the member needs."
                }
            },
            "required": ["member", "task"]
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput> {
        let Some(member) = input.get("member").and_then(Value::as_str) else {
            return Ok(ToolOutput::error("missing required parameter `member`"));
        };
        let Some(task) = input.get("task").and_then(Value::as_str) else {
            return Ok(ToolOutput::error("missing required parameter `task`"));
        };
        let Some(agent) = self.members.get(member) else {
            let known = {
                let mut names: Vec<&String> = self.members.keys().collect();
                names.sort();
                names.into_iter().map(String::as_str).collect::<Vec<_>>().join(", ")
            };
            return Ok(ToolOutput::error(format!(
                "unknown member `{member}`; available: {known}"
            )));
        };
        // 成员会话 id 固定：同一成员跨委派共享上下文（对齐 TeamContext 恢复语义
        // 的持久成员会话）。名字经 sanitize 防路径/存储键注入。
        let session_key = agentscope_harness::agent::sanitize_identifier(member)
            .unwrap_or_else(|| "member".to_string());
        let rc = RuntimeContext::new()
            .with_user_id("studio")
            .with_session_id(format!("team-{session_key}"));
        match agent.delegate().call_with_context(rc, Msg::user("leader", task)).await {
            Ok(reply) => Ok(ToolOutput::ok(reply.text())),
            Err(error) => Ok(ToolOutput::error(format!(
                "member `{member}` failed: {error}"
            ))),
        }
    }
}
