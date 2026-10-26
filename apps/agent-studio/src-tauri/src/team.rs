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
use agentscope_core::event::AgentEventKind;
use agentscope_core::tool::{Tool, ToolContext, ToolOutput};
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
        self.delegate(input, None).await
    }

    /// 带事件流的执行入口：成员改走流式路径，其全部事件（思考/工具/文本）
    /// 以 `source = 成员名` 转发进父事件流——前端据此显示「谁在执行」。
    /// 无 emitter（直接调用）时退化为非流式路径。
    async fn call_with_context(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        self.delegate(input, ctx.emitter.clone()).await
    }
}

impl TeamDelegateTool {
    /// 委派主体：emitter 为 None 时成员走非流式（事件不外露），
    /// Some 时走流式并逐事件转发（打上成员来源标签）。
    async fn delegate(
        &self,
        input: Value,
        emitter: Option<agentscope_core::event::AgentEventEmitter>,
    ) -> Result<ToolOutput> {
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

        // 无事件出口：保持原非流式路径（黑盒执行，只回最终答复）。
        let Some(tx) = emitter else {
            return match agent.delegate().call_with_context(rc, Msg::user("leader", task)).await {
                Ok(reply) => Ok(ToolOutput::ok(reply.text())),
                Err(error) => Ok(ToolOutput::error(format!(
                    "member `{member}` failed: {error}"
                ))),
            };
        };

        // 流式路径：消费成员事件流，逐事件打上成员来源后转发进父流；
        // AgentResult 携带成员最终答复（作为本工具的返回值）。
        let mut rx = agent
            .delegate()
            .stream_events_with_context(rc, Msg::user("leader", task))
            .await?;
        let mut final_text = String::new();
        let mut aborted = false;
        while let Some(mut ev) = rx.recv().await {
            if let AgentEventKind::AgentResult { result } = &ev.kind {
                final_text = result.text();
            }
            ev.source = Some(member.to_string());
            if tx.send(ev).await.is_err() {
                // 父流已关闭（turn 被中断等）：停止转发，成员循环随 rx drop 收尾。
                aborted = true;
                break;
            }
        }
        if aborted {
            return Ok(ToolOutput::error(format!(
                "member `{member}` aborted: parent event stream closed"
            )));
        }
        if final_text.is_empty() {
            return Ok(ToolOutput::error(format!(
                "member `{member}` finished without a reply"
            )));
        }
        Ok(ToolOutput::ok(final_text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::event::AgentEvent;
    use agentscope_core::message::ContentBlock;
    use agentscope_core::model::{ChatRequest, ChatResponse, Model};
    use agentscope_core::tool::ToolContext;
    use agentscope_harness::agent::HarnessAgentBuilder;
    use std::path::PathBuf;
    use std::sync::Mutex;

    struct MockModel {
        responses: Mutex<Vec<ChatResponse>>,
    }

    impl MockModel {
        fn single(text: &str) -> Arc<dyn Model> {
            Arc::new(Self {
                responses: Mutex::new(vec![ChatResponse {
                    content: vec![ContentBlock::Text {
                        text: text.to_string(),
                    }],
                    usage: None,
                    finish_reason: Some("stop".to_string()),
                }]),
            })
        }
    }

    #[async_trait::async_trait]
    impl Model for MockModel {
        async fn call(&self, _request: ChatRequest) -> agentscope_core::error::Result<ChatResponse> {
            self.responses
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| agentscope_core::error::Error::Model("no more mock responses".into()))
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "team-delegate-{tag}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn member_agent(text: &str, tag: &str) -> Arc<HarnessAgent> {
        let workspace = temp_dir(tag);
        Arc::new(
            HarnessAgentBuilder::new()
                .model(MockModel::single(text))
                .workspace(&workspace)
                .disable_subagents()
                .build()
                .unwrap(),
        )
    }

    /// 流式委派：成员事件以 source=成员名 转发进父流，最终答复作为工具输出。
    #[tokio::test]
    async fn delegate_forwards_member_events_with_source() {
        let member = member_agent("member reply", "fwd");
        let mut members = std::collections::HashMap::new();
        members.insert("alice".to_string(), member.clone());
        let tool = TeamDelegateTool::new(Arc::new(members));

        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
        let mut ctx = ToolContext::new("call-1", "team_delegate");
        ctx.emitter = Some(tx);

        let out = tool
            .call_with_context(
                serde_json::json!({"member": "alice", "task": "do it"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out.content, "member reply");
        assert!(!out.is_error());

        // 父流收到的每个事件都带成员来源；且至少有 AgentStart 与 AgentResult。
        let mut saw_start = false;
        let mut saw_result = false;
        while let Ok(ev) = rx.try_recv() {
            assert_eq!(ev.source.as_deref(), Some("alice"), "事件必须带成员来源");
            match &ev.kind {
                agentscope_core::event::AgentEventKind::AgentStart { .. } => saw_start = true,
                agentscope_core::event::AgentEventKind::AgentResult { result } => {
                    saw_result = true;
                    assert_eq!(result.text(), "member reply");
                }
                _ => {}
            }
        }
        assert!(saw_start, "应转发 AgentStart");
        assert!(saw_result, "应转发 AgentResult");
        member.close();
    }

    /// 无 emitter（直接调用）：走非流式路径，只回最终答复。
    #[tokio::test]
    async fn delegate_without_emitter_stays_non_streaming() {
        let member = member_agent("plain reply", "plain");
        let mut members = std::collections::HashMap::new();
        members.insert("bob".to_string(), member.clone());
        let tool = TeamDelegateTool::new(Arc::new(members));

        let out = tool
            .call(serde_json::json!({"member": "bob", "task": "do it"}))
            .await
            .unwrap();
        assert_eq!(out.content, "plain reply");
        member.close();
    }

    /// 未知成员：报错并列出可用成员。
    #[tokio::test]
    async fn delegate_unknown_member_lists_available() {
        let member = member_agent("x", "unknown");
        let mut members = std::collections::HashMap::new();
        members.insert("alice".to_string(), member.clone());
        let tool = TeamDelegateTool::new(Arc::new(members));

        let out = tool
            .call(serde_json::json!({"member": "carol", "task": "t"}))
            .await
            .unwrap();
        assert!(out.is_error());
        assert!(out.content.contains("unknown member `carol`"));
        assert!(out.content.contains("alice"));
        member.close();
    }
}
