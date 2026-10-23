//! AgentTeams 统一工具（对应 Java `harness.agent.tool.TeamTool`）。
//!
//! Java 侧是 1 个统一入口 `team(action, ...)` + 17 个 per-action `@Tool`
//! 别名方法（模型常把 createTask/sendMessage 当工具名调用）。Rust 侧
//! [`TeamTool`] 为统一入口，[`TeamActionTool`] 按 action 元数据参数化出
//! 17 个别名实例（[`team_action_tools`]），转发时仅做参数重映射
//! （failTask 的 `reason` → 统一入口的 `result` 位，对齐 Java 形参位置）。
//!
//! 动作门控：[`TeamContext::available_actions`] 非空时按 normalize 后的
//! 名单放行，并保留三条别名放宽（对齐 Java `isAllowed`）——
//! claimTask 放行 listClaimableTasks/unclaimTask，sendMessage/broadcastMessage
//! 放行 listMessages，completeTask 放行 failTask。

use crate::team::{TeamClient, TeamContext, TeamError};
use agentscope_core::tool::ToolOutput;
use agentscope_core::Tool;
use serde_json::{json, Value};
use std::sync::Arc;

/// 统一入口工具（Java `team` 方法；name = `"team"`）。
pub struct TeamTool {
    client: Arc<dyn TeamClient>,
    context: TeamContext,
}

impl TeamTool {
    pub fn new(client: Arc<dyn TeamClient>, context: TeamContext) -> Self {
        Self { client, context }
    }

    /// 动作名归一（对齐 Java `normalize`：小写 + 去 `_`/`-`）。
    fn normalize(s: &str) -> String {
        s.to_lowercase().replace(['_', '-'], "")
    }

    /// 角色门控（对齐 Java `isAllowed`）：名单空 = 全放行；别名放宽保证
    /// 上下文里只点名主动作时，配套查询/收尾动作仍可用。
    fn is_allowed(&self, action: &str) -> bool {
        let allowed = &self.context.available_actions;
        if allowed.is_empty() {
            return true;
        }
        let set: std::collections::HashSet<String> =
            allowed.iter().map(|a| Self::normalize(a.as_str())).collect();
        let n = Self::normalize(action);
        if (n == "listclaimabletasks" || n == "unclaimtask") && set.contains("claimtask") {
            return true;
        }
        if n == "listmessages"
            && (set.contains("sendmessage") || set.contains("broadcastmessage"))
        {
            return true;
        }
        if n == "failtask" && set.contains("completetask") {
            return true;
        }
        set.contains(&n)
    }

    fn error(msg: impl Into<String>) -> ToolOutput {
        ToolOutput::error(format!("{{\"error\":{}}}", quote(&msg.into())))
    }

    /// 统一执行入口（对齐 Java `team` 方法：错误全部转为 error 输出，
    /// 不向上抛）。
    pub async fn execute(&self, input: &Value) -> ToolOutput {
        let act = input
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        if act.is_empty() {
            return Self::error("action is required");
        }
        if !self.is_allowed(&act) {
            return Self::error(format!(
                "action '{}' is not allowed for role {}",
                act,
                self.context.my_role
            ));
        }
        let ns = self.context.resolved_namespace().to_string();
        let team = self.context.team_name.clone();
        let me = self.context.my_role.clone();
        let p = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or_default();
        let require = |k: &str| -> Result<String, ToolOutput> {
            let v = p(k);
            if v.trim().is_empty() {
                Err(Self::error(format!("{k} is required")))
            } else {
                Ok(v.to_string())
            }
        };
        let int_of = |k: &str| input.get(k).and_then(Value::as_i64);
        let n = Self::normalize(&act);
        match n.as_str() {
            "listtasks" => json_out(self.client.list_tasks(&ns, &team).await),
            "listclaimabletasks" => {
                json_out(self.client.list_claimable_tasks(&ns, &team, &me).await)
            }
            "createtask" => json_out(
                self.client
                    .create_task(&ns, &team, p("subject"), p("description"), vec![], p("owner"))
                    .await,
            ),
            "assigntask" => {
                let task_id = match require("task_id") {
                    Ok(v) => v,
                    Err(e) => return e,
                };
                let owner = match require("owner") {
                    Ok(v) => v,
                    Err(e) => return e,
                };
                json_out(
                    self.client
                        .assign_task(&ns, &team, &task_id, &owner, int_of("expected_version").unwrap_or(0))
                        .await,
                )
            }
            "claimtask" => {
                let task_id = match require("task_id") {
                    Ok(v) => v,
                    Err(e) => return e,
                };
                let owner = if p("owner").trim().is_empty() {
                    me.clone()
                } else {
                    p("owner").to_string()
                };
                json_out(
                    self.client
                        .claim_task(&ns, &team, &task_id, &owner, int_of("expected_version").unwrap_or(0))
                        .await,
                )
            }
            "unclaimtask" => {
                let task_id = match require("task_id") {
                    Ok(v) => v,
                    Err(e) => return e,
                };
                json_out(
                    self.client
                        .unclaim_task(&ns, &team, &task_id, int_of("expected_version").unwrap_or(0))
                        .await,
                )
            }
            "completetask" => {
                let task_id = match require("task_id") {
                    Ok(v) => v,
                    Err(e) => return e,
                };
                json_out(self.client.complete_task(&ns, &team, &task_id, p("result")).await)
            }
            "failtask" => {
                let task_id = match require("task_id") {
                    Ok(v) => v,
                    Err(e) => return e,
                };
                json_out(self.client.fail_task(&ns, &team, &task_id, p("result")).await)
            }
            "sendmessage" => {
                let to = match require("to_member") {
                    Ok(v) => v,
                    Err(e) => return e,
                };
                json_out(self.client.send_message(&ns, &team, &me, &to, p("content")).await)
            }
            "broadcastmessage" => {
                json_out(self.client.broadcast_message(&ns, &team, &me, p("content")).await)
            }
            "listmessages" => {
                json_out(self.client.list_messages(&ns, &team, int_of("limit").unwrap_or(50).max(0) as usize).await)
            }
            "listmembers" => json_out(self.client.list_members(&ns, &team).await),
            "spawnmember" => {
                let (name, agent_ref) = match (require("member_name"), require("agent_ref")) {
                    (Ok(a), Ok(b)) => (a, b),
                    (Err(e), _) | (_, Err(e)) => return e,
                };
                match self
                    .client
                    .spawn_member(&ns, &team, &name, &agent_ref, p("prompt"))
                    .await
                {
                    Ok(()) => ToolOutput::ok("{\"ok\":true}"),
                    Err(e) => team_err(e),
                }
            }
            "shutdownmember" => {
                let name = match require("member_name") {
                    Ok(v) => v,
                    Err(e) => return e,
                };
                match self.client.shutdown_member(&ns, &team, &name).await {
                    Ok(()) => ToolOutput::ok("{\"ok\":true}"),
                    Err(e) => team_err(e),
                }
            }
            "submitplan" => {
                let text = if !p("plan_text").trim().is_empty() {
                    p("plan_text").to_string()
                } else {
                    p("prompt").to_string()
                };
                let member = if p("member_name").trim().is_empty() {
                    me.clone()
                } else {
                    p("member_name").to_string()
                };
                match self.client.submit_plan(&ns, &team, &member, &text).await {
                    Ok(()) => ToolOutput::ok("{\"ok\":true}"),
                    Err(e) => team_err(e),
                }
            }
            "approveplan" => {
                let name = match require("member_name") {
                    Ok(v) => v,
                    Err(e) => return e,
                };
                match self.client.approve_plan(&ns, &team, &name).await {
                    Ok(()) => ToolOutput::ok("{\"ok\":true}"),
                    Err(e) => team_err(e),
                }
            }
            "rejectplan" => {
                let name = match require("member_name") {
                    Ok(v) => v,
                    Err(e) => return e,
                };
                match self.client.reject_plan(&ns, &team, &name).await {
                    Ok(()) => ToolOutput::ok("{\"ok\":true}"),
                    Err(e) => team_err(e),
                }
            }
            "completeteam" => match self.client.complete_team(&ns, &team).await {
                Ok(()) => ToolOutput::ok("{\"ok\":true}"),
                Err(e) => team_err(e),
            },
            _ => Self::error(format!("unknown action: {act}")),
        }
    }
}

#[async_trait::async_trait]
impl Tool for TeamTool {
    fn name(&self) -> &str {
        "team"
    }
    fn description(&self) -> &str {
        "AgentTeams coordination. Set action to one of: listTasks, createTask, assignTask, claimTask, unclaimTask, completeTask, failTask, listClaimableTasks, sendMessage, broadcastMessage, listMessages, listMembers, spawnMember, shutdownMember, submitPlan, approvePlan, rejectPlan, completeTeam. Lead-only actions require isLead. expected_version is optional for claimTask (omit to claim at the current board version)."
    }

    fn parameters(&self) -> Value {
        action_schema(
            &[
                ("task_id", "Task id", false, "string"),
                ("subject", "Task subject", false, "string"),
                ("description", "Task description", false, "string"),
                ("owner", "Assignee / claimer", false, "string"),
                ("expected_version", "Optimistic lock version", false, "integer"),
                ("result", "Completion result text", false, "string"),
                ("to_member", "Mailbox recipient", false, "string"),
                ("content", "Mailbox message body", false, "string"),
                ("limit", "List limit", false, "integer"),
                ("member_name", "Teammate name", false, "string"),
                ("agent_ref", "Agent ref for spawn", false, "string"),
                ("prompt", "Member prompt / plan text", false, "string"),
                ("plan_text", "Plan body for submitPlan", false, "string"),
            ],
            Some("Team action name"),
        )
    }

    async fn call(&self, input: Value) -> agentscope_core::Result<ToolOutput> {
        Ok(self.execute(&input).await)
    }
}

/// per-action 别名工具（对齐 Java 17 个 `@Tool` 别名方法）：转发统一入口，
/// 仅 `failTask` 需要把 `reason` 重映射到统一入口的 `result` 形参位。
pub struct TeamActionTool {
    inner: Arc<TeamTool>,
    tool_name: &'static str,
    description: &'static str,
    params: &'static [(&'static str, &'static str, bool, &'static str)],
    remap: &'static [(&'static str, &'static str)],
}

impl TeamActionTool {
    fn forward(&self, action: &str, input: &Value) -> Value {
        let mut obj = match input {
            Value::Object(map) => map.clone(),
            _ => serde_json::Map::new(),
        };
        for (from, to) in self.remap {
            if let Some(v) = obj.remove(*from) {
                obj.insert((*to).to_string(), v);
            }
        }
        obj.insert("action".to_string(), json!(action));
        Value::Object(obj)
    }
}

#[async_trait::async_trait]
impl Tool for TeamActionTool {
    fn name(&self) -> &str {
        self.tool_name
    }

    fn description(&self) -> &str {
        self.description
    }

    fn parameters(&self) -> Value {
        action_schema(self.params, None)
    }

    async fn call(&self, input: Value) -> agentscope_core::Result<ToolOutput> {
        Ok(self.inner.execute(&self.forward(self.tool_name, &input)).await)
    }
}

/// 别名工具参数定义：(参数名, 描述, 是否必填, JSON 类型)。
type ParamDef = (&'static str, &'static str, bool, &'static str);
/// 别名工具定义：(工具名, 描述, 参数表, 重映射表)。
type ActionDef = (&'static str, &'static str, &'static [ParamDef], &'static [(&'static str, &'static str)]);

/// 产出 18 个 per-action 别名工具（对齐 Java `TeamTool` 的 18 个
/// `@Tool` 别名方法；统一入口 `team` 单独注册）。
#[must_use]
pub fn team_action_tools(inner: Arc<TeamTool>) -> Vec<Arc<dyn Tool>> {
    const S: &str = "string";
    const I: &str = "integer";
    let defs: &[ActionDef] = &[
        ("listTasks", "List team tasks on the shared board.", &[], &[]),
        (
            "listClaimableTasks",
            "List unblocked pending tasks you can start: unassigned board tasks and tasks already assigned to you. Prefer this (or listTasks) before product work; then claimTask.",
            &[],
            &[],
        ),
        (
            "createTask",
            "Create a task on the shared team board. Lead typically sets owner to assign.",
            &[
                ("subject", "Short task title", true, S),
                ("description", "Task details", false, S),
                ("owner", "Optional assignee: a member name from the team roster (e.g. w1), not the agent name", false, S),
            ],
            &[],
        ),
        (
            "assignTask",
            "Assign a pending task to a member (lead).",
            &[
                ("task_id", "Task id", true, S),
                ("owner", "Assignee: a member name from the team roster, not the agent name", true, S),
                ("expected_version", "Optimistic lock version", false, I),
            ],
            &[],
        ),
        (
            "claimTask",
            "Claim a pending task for yourself (or start work already assigned to you). expected_version is optional — omit it to claim at the current version.",
            &[
                ("task_id", "Task id", true, S),
                ("expected_version", "Optimistic lock version (optional)", false, I),
            ],
            &[],
        ),
        (
            "unclaimTask",
            "Return a claimed task to the board.",
            &[
                ("task_id", "Task id", true, S),
                ("expected_version", "Optimistic lock version", false, I),
            ],
            &[],
        ),
        (
            "failTask",
            "Mark a task failed with a reason when you cannot finish it. The lead is notified automatically. Use this instead of silently abandoning work.",
            &[
                ("task_id", "Task id", true, S),
                ("reason", "Why the task failed", true, S),
            ],
            &[("reason", "result")],
        ),
        (
            "completeTask",
            "Mark a task completed with a result summary. The lead is notified automatically, so summarize the outcome in result.",
            &[
                ("task_id", "Task id", true, S),
                ("result", "Completion result text", false, S),
            ],
            &[],
        ),
        (
            "sendMessage",
            "Send a mailbox message to one teammate.",
            &[
                ("to_member", "Recipient member name", true, S),
                ("content", "Message body", true, S),
            ],
            &[],
        ),
        (
            "broadcastMessage",
            "Broadcast a mailbox message to all teammates.",
            &[("content", "Message body", true, S)],
            &[],
        ),
        (
            "listMessages",
            "List recent team mailbox messages.",
            &[("limit", "Max messages", false, I)],
            &[],
        ),
        ("listMembers", "List team members and phases.", &[], &[]),
        (
            "spawnMember",
            "Spawn a dynamic teammate (lead).",
            &[
                ("member_name", "New member name", true, S),
                ("agent_ref", "Registry agentRef", true, S),
                ("prompt", "Role prompt", false, S),
            ],
            &[],
        ),
        (
            "shutdownMember",
            "Shut down a teammate (lead).",
            &[("member_name", "Member to remove", true, S)],
            &[],
        ),
        (
            "submitPlan",
            "Submit a plan for lead approval.",
            &[
                ("plan_text", "Plan body", true, S),
                ("member_name", "Member name", false, S),
            ],
            &[],
        ),
        (
            "approvePlan",
            "Approve a member plan (lead).",
            &[("member_name", "Member whose plan to approve", true, S)],
            &[],
        ),
        (
            "rejectPlan",
            "Reject a member plan (lead).",
            &[("member_name", "Member whose plan to reject", true, S)],
            &[],
        ),
        ("completeTeam", "Mark the team objective complete (lead).", &[], &[]),
    ];
    defs.iter()
        .map(|(name, desc, params, remap)| {
            Arc::new(TeamActionTool {
                inner: inner.clone(),
                tool_name: name,
                description: desc,
                params,
                remap,
            }) as Arc<dyn Tool>
        })
        .collect()
}

// ---------- 共享辅助 ----------

/// JSON Schema 构造：`action` 可选传入（统一入口 required，别名工具由
/// 转发层注入故不出现在 schema）。
fn action_schema(
    params: &[(&str, &str, bool, &str)],
    action_desc: Option<&str>,
) -> Value {
    let mut props = serde_json::Map::new();
    let mut required = Vec::new();
    if let Some(desc) = action_desc {
        props.insert("action".to_string(), json!({"type": "string", "description": desc}));
        required.push(json!("action"));
    }
    for (name, desc, req, ty) in params {
        props.insert(
            (*name).to_string(),
            json!({"type": ty, "description": desc}),
        );
        if *req {
            required.push(json!(name));
        }
    }
    json!({
        "type": "object",
        "properties": Value::Object(props),
        "required": required,
    })
}

/// `TeamError` → error 输出（对齐 Java catch 顺序：Conflict 加前缀，
/// 其余直接用消息）。
fn team_err(e: TeamError) -> ToolOutput {
    match e {
        TeamError::Conflict(m) => TeamTool::error(format!("conflict: {m}")),
        other => TeamTool::error(other.to_string()),
    }
}

/// 成功结果序列化（对齐 Java `json`：null → "null"，其余 serde 序列化）。
fn json_out<T: serde::Serialize>(value: Result<T, TeamError>) -> ToolOutput {
    match value {
        Ok(v) => match serde_json::to_string(&v) {
            Ok(s) => ToolOutput::ok(s),
            Err(_) => TeamTool::error("json encode failed"),
        },
        Err(e) => team_err(e),
    }
}

/// 最小 JSON 字符串转义（对齐 Java `quote`）。
fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// lead 视角上下文（全动作放行）。
#[must_use]
pub fn lead_context(team_name: &str, namespace: &str) -> TeamContext {
    TeamContext {
        team_name: team_name.to_string(),
        namespace: Some(namespace.to_string()),
        objective: String::new(),
        my_role: "lead".to_string(),
        is_lead: true,
        members: vec![],
        available_actions: vec![],
        recovery_context: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::base_store::InMemoryBaseStore;
    use crate::team::{LocalTeamClient, TeamCreateSpec, TeamMemberSpec};

    fn setup() -> (Arc<LocalTeamClient>, TeamTool) {
        let client: Arc<LocalTeamClient> =
            Arc::new(LocalTeamClient::new(Arc::new(InMemoryBaseStore::new())));
        let tool = TeamTool::new(client.clone(), lead_context("alpha", "default"));
        (client, tool)
    }

    fn dyn_client(c: &Arc<LocalTeamClient>) -> Arc<dyn TeamClient> {
        c.clone()
    }

    async fn seeded(client: &LocalTeamClient) {
        client
            .create_team(&TeamCreateSpec {
                name: "alpha".into(),
                namespace: "default".into(),
                objective: "obj".into(),
                lead_agent_ref: "lead-agent".into(),
                lead_prompt: String::new(),
                members: vec![TeamMemberSpec {
                    name: "alice".into(),
                    agent_ref: "worker-a".into(),
                    prompt: String::new(),
                    deploy_mode: String::new(),
                }],
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn action_required_and_unknown() {
        let (_c, tool) = setup();
        let out = tool.execute(&json!({})).await;
        assert!(out.is_error());
        assert!(out.content.contains("action is required"));
        let out = tool.execute(&json!({"action": "dance"})).await;
        assert!(out.is_error());
        assert!(out.content.contains("unknown action: dance"));
    }

    #[tokio::test]
    async fn normalize_strips_underscores_and_case() {
        let (_c, tool) = setup();
        for act in ["listTasks", "list_tasks", "LIST-TASKS", "listtasks"] {
            let out = tool.execute(&json!({"action": act})).await;
            assert!(!out.is_error(), "{act} 应命中 listtasks");
        }
    }

    #[tokio::test]
    async fn create_task_via_unified_entry() {
        let (_c, tool) = setup();
        let out = tool
            .execute(&json!({"action": "createTask", "subject": "fix", "owner": "alice"}))
            .await;
        assert!(!out.is_error(), "{}", out.content);
        let v: Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(v["taskId"], "task-1");
        assert_eq!(v["owner"], "alice");
        assert_eq!(v["state"], "pending");
    }

    #[tokio::test]
    async fn require_validation_messages() {
        let (_c, tool) = setup();
        let out = tool
            .execute(&json!({"action": "assignTask", "task_id": "task-1"}))
            .await;
        assert!(out.is_error());
        assert!(out.content.contains("owner is required"));
        let out = tool.execute(&json!({"action": "claimTask"})).await;
        assert!(out.content.contains("task_id is required"));
    }

    #[tokio::test]
    async fn claim_defaults_owner_to_me() {
        let (c, tool) = setup();
        seeded(&c).await;
        c.create_task("default", "alpha", "s", "", vec![], "").await.unwrap();
        // lead 认领：owner 缺省 → myRole（lead）。
        let out = tool
            .execute(&json!({"action": "claimTask", "task_id": "task-1"}))
            .await;
        assert!(!out.is_error(), "{}", out.content);
        let v: Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(v["owner"], "lead");
        assert_eq!(v["state"], "in_progress");
    }

    #[tokio::test]
    async fn conflict_maps_to_prefixed_error() {
        let (c, tool) = setup();
        seeded(&c).await;
        c.create_task("default", "alpha", "s", "", vec![], "alice").await.unwrap();
        // alice 已被指派，bob 认领 → owned by 冲突。
        let out = tool
            .execute(&json!({"action": "claimTask", "task_id": "task-1", "owner": "bob"}))
            .await;
        assert!(out.is_error());
        assert!(out.content.contains("conflict:"));
        assert!(out.content.contains("owned by alice"));
    }

    #[tokio::test]
    async fn unsupported_maps_to_plain_error() {
        let (_c, tool) = setup();
        let out = tool
            .execute(&json!({"action": "spawnMember", "member_name": "w2", "agent_ref": "r"}))
            .await;
        assert!(out.is_error());
        assert!(out.content.contains("unsupported"));
    }

    #[tokio::test]
    async fn list_actions_serialize_camel_case() {
        let (c, tool) = setup();
        seeded(&c).await;
        let out = tool.execute(&json!({"action": "listMembers"})).await;
        assert!(out.content.contains("memberName"), "camelCase 存储键: {}", out.content);
        assert!(out.content.contains("isLead"));
        let out = tool.execute(&json!({"action": "listTasks"})).await;
        assert_eq!(out.content, "[]");
        let out = tool
            .execute(&json!({"action": "sendMessage", "to_member": "alice", "content": "hi"}))
            .await;
        let v: Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(v["from"], "lead");
    }

    #[tokio::test]
    async fn role_gate_blocks_worker_from_lead_actions() {
        let (c, _tool) = setup();
        seeded(&c).await;
        let worker_ctx = TeamContext {
            team_name: "alpha".into(),
            namespace: Some("default".into()),
            objective: String::new(),
            my_role: "alice".into(),
            is_lead: false,
            members: vec![],
            available_actions: vec![
                "listTasks".into(),
                "claimTask".into(),
                "completeTask".into(),
            ],
            recovery_context: None,
        };
        let worker = TeamTool::new(dyn_client(&c), worker_ctx);
        // 名单内放行。
        let out = worker.execute(&json!({"action": "listTasks"})).await;
        assert!(!out.is_error());
        // 名单外拒绝（lead 专属）。
        let out = worker
            .execute(&json!({"action": "assignTask", "task_id": "task-1", "owner": "bob"}))
            .await;
        assert!(out.is_error());
        assert!(out.content.contains("not allowed for role alice"));
        // 别名放宽：有 claimTask → listClaimableTasks / unclaimTask 放行。
        let out = worker.execute(&json!({"action": "listClaimableTasks"})).await;
        assert!(!out.is_error());
        // 别名放宽：有 completeTask → failTask 放行。
        let out = worker
            .execute(&json!({"action": "failTask", "task_id": "task-404"}))
            .await;
        assert!(out.is_error());
        assert!(out.content.contains("not found"), "门控已过，走到执行层: {}", out.content);
        // 无放宽关系：有 claimTask 也不放行 sendMessage。
        let out = worker
            .execute(&json!({"action": "sendMessage", "to_member": "lead", "content": "x"}))
            .await;
        assert!(out.is_error());
        assert!(out.content.contains("not allowed"));
    }

    #[tokio::test]
    async fn alias_gate_message_and_fail() {
        let (c, _tool) = setup();
        seeded(&c).await;
        let ctx = TeamContext {
            available_actions: vec!["sendMessage".into()],
            my_role: "alice".into(),
            ..lead_context("alpha", "default")
        };
        let tool = TeamTool::new(dyn_client(&c), ctx);
        // sendmessage → listmessages 放行。
        let out = tool.execute(&json!({"action": "listMessages"})).await;
        assert!(!out.is_error());
        // broadcastmessage 也在名单时同样放行（二选一即可）。
        let ctx = TeamContext {
            available_actions: vec!["broadcastMessage".into()],
            my_role: "alice".into(),
            ..lead_context("alpha", "default")
        };
        let tool = TeamTool::new(dyn_client(&c), ctx);
        let out = tool.execute(&json!({"action": "listMessages"})).await;
        assert!(!out.is_error());
    }

    #[tokio::test]
    async fn empty_action_list_allows_everything() {
        let (c, _tool) = setup();
        seeded(&c).await;
        c.create_task("default", "alpha", "s", "", vec![], "").await.unwrap();
        let ctx = TeamContext {
            available_actions: vec![],
            my_role: "alice".into(),
            ..lead_context("alpha", "default")
        };
        let tool = TeamTool::new(dyn_client(&c), ctx);
        // 无参动作直接成功。
        let out = tool.execute(&json!({"action": "listTasks"})).await;
        assert!(!out.is_error(), "名单空 = 全放行: {}", out.content);
        // lead 专属动作也放行（错误来自执行层而非门控）。
        let out = tool
            .execute(&json!({"action": "assignTask", "task_id": "task-1", "owner": "bob"}))
            .await;
        assert!(
            !(out.is_error() && out.content.contains("not allowed")),
            "不应被门控拒绝: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn action_tools_registry_and_forwarding() {
        let (c, tool) = setup();
        seeded(&c).await;
        let inner = Arc::new(tool);
        let tools = team_action_tools(inner.clone());
        assert_eq!(tools.len(), 18);
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"createTask"));
        assert!(names.contains(&"failTask"));
        assert!(names.contains(&"completeTeam"));
        // 别名转发：createTask 工具直接建任务。
        let create = tools.iter().find(|t| t.name() == "createTask").unwrap();
        let out = create.call(json!({"subject": "via alias", "owner": "alice"})).await.unwrap();
        assert!(!out.is_error(), "{}", out.content);
        let v: Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(v["taskId"], "task-1");
        // failTask 别名：reason 重映射到统一入口 result 位。
        c.claim_task("default", "alpha", "task-1", "alice", 0).await.unwrap();
        let fail = tools.iter().find(|t| t.name() == "failTask").unwrap();
        let out = fail
            .call(json!({"task_id": "task-1", "reason": "blocked upstream"}))
            .await
            .unwrap();
        assert!(!out.is_error(), "{}", out.content);
        let v: Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(v["result"], "blocked upstream", "reason → result 重映射");
        assert_eq!(v["state"], "failed");
        // 无参别名 listMembers。
        let list = tools.iter().find(|t| t.name() == "listMembers").unwrap();
        let out = list.call(json!({})).await.unwrap();
        assert!(out.content.contains("alice"));
    }

    #[tokio::test]
    async fn submit_plan_prefers_plan_text_over_prompt() {
        let (_c, tool) = setup();
        // 本地实现不支持 submitPlan，但参数组装路径可经 unsupported 错误验证
        // （错误发生在 client 调用时，说明 require/文本选择已通过）。
        let out = tool
            .execute(&json!({"action": "submitPlan", "plan_text": "  ", "prompt": "fallback"}))
            .await;
        assert!(out.is_error());
        assert!(out.content.contains("unsupported"), "文本选择不应触发 require 错误");
        // member_name 缺省 → me，同样走到 client 层。
        let out = tool.execute(&json!({"action": "submitPlan", "plan_text": "p"})).await;
        assert!(out.content.contains("unsupported"));
    }

    #[tokio::test]
    async fn unified_tool_metadata() {
        let (_c, tool) = setup();
        assert_eq!(Tool::name(&tool), "team");
        let schema = Tool::parameters(&tool);
        assert_eq!(schema["required"][0], "action");
        assert!(schema["properties"]["expected_version"].is_object());
        assert_eq!(schema["properties"]["expected_version"]["type"], "integer");
        // 别名工具 schema 的 required 不含 action。
        let inner = Arc::new(tool);
        let tools = team_action_tools(inner);
        let create = tools.iter().find(|t| t.name() == "createTask").unwrap();
        let schema = create.parameters();
        let req = schema["required"].as_array().unwrap();
        assert!(req.iter().all(|r| r != "action"));
        assert!(req.contains(&json!("subject")));
    }
}
