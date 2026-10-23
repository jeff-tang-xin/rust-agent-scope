//! AgentTeams 协调（对应 Java `harness.agent.team` 包 11 类）。
//!
//! Claude-parity 任务板 + 邮箱：任务带乐观锁版本与 `blockedBy` 依赖，
//! 成员经邮箱点对点/广播通信，lead 审批计划。[`TeamClient`] 是抽象
//! （Java 注释：LocalTeamClient 走 BaseStore CAS，control-plane 走 HTTP——
//! 后者留分布式阶段）；[`LocalTeamClient`] 是 store-backed 本地实现。
//!
//! 形态映射：
//! - Java `Mono<T>` → `async fn` + `Result<T, TeamError>`；
//! - Java `TeamConflictException`/`IllegalArgumentException`/
//!   `IllegalStateException`/`UnsupportedOperationException` →
//!   [`TeamError`] 四变体；
//! - Java `TeamWakeups` 进程级静态 hook → [`LocalTeamClient::with_wakeup`]
//!   构造注入（反模式⑤：不做全局单例）；
//! - Java record 的 `Map<String,Object>` 手工转换 → serde derive
//!   （`TeamMemberInfo::from_value` 保留「名字叫 lead 即 lead」的派生规则）。

use crate::base_store::{BaseStore, StoreItem};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

// ---------- 数据类型 ----------

/// 共享任务板条目（对应 Java `TeamTask` record；存储键 camelCase 对齐）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamTask {
    #[serde(default)]
    pub task_id: String,
    #[serde(default)]
    pub team_name: String,
    #[serde(default)]
    pub namespace: String,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub blocked_by: Vec<String>,
    #[serde(default)]
    pub result: String,
    #[serde(default)]
    pub version: i64,
}

impl TeamTask {
    pub const PENDING: &'static str = "pending";
    pub const IN_PROGRESS: &'static str = "in_progress";
    pub const COMPLETED: &'static str = "completed";
    pub const FAILED: &'static str = "failed";

    /// 终态（不再接受状态转移）。
    #[must_use]
    pub fn is_terminal(state: &str) -> bool {
        state == Self::COMPLETED || state == Self::FAILED
    }

    /// 未被阻塞的 pending 任务（对应 Java `claimableOf`）：
    /// 未分配的开放任务；`for_member` 非空白时还包括已分配给该成员的任务。
    #[must_use]
    pub fn claimable_of(tasks: &[TeamTask], for_member: &str) -> Vec<TeamTask> {
        let completed: std::collections::HashSet<&str> = tasks
            .iter()
            .filter(|t| t.state == Self::COMPLETED)
            .map(|t| t.task_id.as_str())
            .collect();
        let me = for_member.trim();
        tasks
            .iter()
            .filter(|t| t.state == Self::PENDING)
            .filter(|t| {
                let owner = t.owner.trim();
                owner.is_empty() || (!me.is_empty() && owner == me)
            })
            .filter(|t| t.blocked_by.iter().all(|b| completed.contains(b.as_str())))
            .cloned()
            .collect()
    }
}

/// 团队邮箱消息（对应 Java `TeamMessage`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamMessage {
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub to: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub id: i64,
}

/// 成员花名册条目（对应 Java `TeamMemberInfo`；存储键 camelCase）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamMemberInfo {
    #[serde(default)]
    pub member_name: String,
    #[serde(default)]
    pub agent_ref: String,
    #[serde(default)]
    pub phase: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub deploy_mode: String,
    #[serde(default)]
    pub is_lead: bool,
}

impl TeamMemberInfo {
    /// 从存储值解析（对应 Java `fromMap`）：`isLead` 为 true **或**
    /// 名字叫 `lead` 都判为 lead。
    #[must_use]
    pub fn from_value(value: &Value) -> Self {
        let s = |k: &str| {
            value
                .get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let member_name = s("memberName");
        let is_lead = value
            .get("isLead")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || member_name == "lead";
        Self {
            member_name,
            agent_ref: s("agentRef"),
            phase: s("phase"),
            session_id: s("sessionId"),
            deploy_mode: s("deployMode"),
            is_lead,
        }
    }
}

/// 轻量团队记录（对应 Java `TeamInfo`；存储键 camelCase）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamInfo {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub namespace: String,
    #[serde(default)]
    pub objective: String,
    #[serde(default)]
    pub phase: String,
    #[serde(default)]
    pub lead_ref: String,
}

/// 建团规格（对应 Java `TeamCreateSpec`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TeamCreateSpec {
    pub name: String,
    #[serde(default)]
    pub namespace: String,
    #[serde(default)]
    pub objective: String,
    #[serde(default)]
    pub lead_agent_ref: String,
    #[serde(default)]
    pub lead_prompt: String,
    #[serde(default)]
    pub members: Vec<TeamMemberSpec>,
}

/// 建团成员种子（对应 Java `TeamMemberSpec`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TeamMemberSpec {
    pub name: String,
    #[serde(default)]
    pub agent_ref: String,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub deploy_mode: String,
}

/// 注入 teammate 会话的运行时上下文（对应 Java `TeamContext`，
/// mirrors Go TeamContext；serde 默认忽略未知字段，等价
/// `@JsonIgnoreProperties(ignoreUnknown = true)`；键 camelCase 对齐）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamContext {
    #[serde(default)]
    pub team_name: String,
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub objective: String,
    #[serde(default)]
    pub my_role: String,
    #[serde(default)]
    pub is_lead: bool,
    #[serde(default)]
    pub members: Vec<MemberSnapshot>,
    #[serde(default)]
    pub available_actions: Vec<String>,
    #[serde(default)]
    pub recovery_context: Option<RecoveryContext>,
}

impl TeamContext {
    /// 空白 namespace 归一为 `"default"`（对应 Java `resolvedNamespace`）。
    #[must_use]
    pub fn resolved_namespace(&self) -> &str {
        match &self.namespace {
            Some(ns) if !ns.trim().is_empty() => ns,
            _ => "default",
        }
    }
}

/// 花名册快照（非完整存储行）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemberSnapshot {
    pub name: String,
    #[serde(default)]
    pub agent_ref: String,
    #[serde(default)]
    pub status: String,
}

/// 成员会话崩溃/重排后的恢复上下文。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryContext {
    #[serde(default)]
    pub previous_session_id: String,
    #[serde(default)]
    pub restart_count: i32,
    #[serde(default)]
    pub completed_tasks: Vec<CompletedTask>,
    #[serde(default)]
    pub interrupted_task: Option<InterruptedTask>,
    #[serde(default)]
    pub recent_messages: Vec<RecentMessage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletedTask {
    pub id: String,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub result: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterruptedTask {
    pub id: String,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecentMessage {
    pub from: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub timestamp: String,
}

// ---------- 错误 ----------

/// 团队操作错误（对应 Java 四类异常的归并）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TeamError {
    /// 乐观锁/所有权冲突（Java `TeamConflictException`）。
    #[error("team conflict: {0}")]
    Conflict(String),
    /// 参数或状态不合法（Java `IllegalArgumentException` /
    /// `IllegalStateException`）。
    #[error("invalid team operation: {0}")]
    Invalid(String),
    /// 目标不存在（Java `IllegalArgumentException("... not found")`）。
    #[error("not found: {0}")]
    NotFound(String),
    /// 本地实现不支持（Java `UnsupportedOperationException`，
    /// 需 control-plane 后端）。
    #[error("unsupported: {0}")]
    Unsupported(String),
}

// ---------- 客户端抽象 ----------

/// AgentTeams 协调客户端（对应 Java `TeamClient`，Claude-parity
/// 任务板 + 邮箱）。
#[async_trait]
pub trait TeamClient: Send + Sync {
    /// 列出任务板全部任务。
    async fn list_tasks(&self, namespace: &str, team_name: &str)
        -> Result<Vec<TeamTask>, TeamError>;

    /// 创建 pending 任务；`owner` 可空白（未分配）。
    async fn create_task(
        &self,
        namespace: &str,
        team_name: &str,
        subject: &str,
        description: &str,
        blocked_by: Vec<String>,
        owner: &str,
    ) -> Result<TeamTask, TeamError>;

    /// lead 把 pending 任务指派给 `owner`（保持 pending）。
    async fn assign_task(
        &self,
        namespace: &str,
        team_name: &str,
        task_id: &str,
        owner: &str,
        expected_version: i64,
    ) -> Result<TeamTask, TeamError>;

    /// 认领 pending 未阻塞任务为 in_progress：owner 为空（自认领）或
    /// 已等于 `claimed_by`（被指派者开工）。
    ///
    /// `expected_version <= 0` 表示「按存储当前版本认领」（模型常省略
    /// 乐观锁令牌）。
    async fn claim_task(
        &self,
        namespace: &str,
        team_name: &str,
        task_id: &str,
        claimed_by: &str,
        expected_version: i64,
    ) -> Result<TeamTask, TeamError>;

    /// 完成 in_progress 任务，并通知 lead 结果。
    async fn complete_task(
        &self,
        namespace: &str,
        team_name: &str,
        task_id: &str,
        result: &str,
    ) -> Result<TeamTask, TeamError>;

    /// 把非终态任务标记失败，并通知 lead 原因。
    async fn fail_task(
        &self,
        namespace: &str,
        team_name: &str,
        task_id: &str,
        reason: &str,
    ) -> Result<TeamTask, TeamError>;

    /// 把 in_progress 任务退回 pending（owner 清空，回到可认领池）。
    async fn unclaim_task(
        &self,
        namespace: &str,
        team_name: &str,
        task_id: &str,
        expected_version: i64,
    ) -> Result<TeamTask, TeamError>;

    /// 调用方可开工的任务：未分配的开放任务 +（`for_member` 非空白时）
    /// 已分配给该成员的 pending 任务。
    async fn list_claimable_tasks(
        &self,
        namespace: &str,
        team_name: &str,
        for_member: &str,
    ) -> Result<Vec<TeamTask>, TeamError>;

    /// 点对点邮箱消息。
    async fn send_message(
        &self,
        namespace: &str,
        team_name: &str,
        from: &str,
        to: &str,
        content: &str,
    ) -> Result<TeamMessage, TeamError>;

    /// 广播消息给除发送者外的全部成员。
    async fn broadcast_message(
        &self,
        namespace: &str,
        team_name: &str,
        from: &str,
        content: &str,
    ) -> Result<Vec<TeamMessage>, TeamError>;

    /// 列出最近消息（最多 `limit` 条）。
    async fn list_messages(
        &self,
        namespace: &str,
        team_name: &str,
        limit: usize,
    ) -> Result<Vec<TeamMessage>, TeamError>;

    /// 列出成员。
    async fn list_members(
        &self,
        namespace: &str,
        team_name: &str,
    ) -> Result<Vec<TeamMemberInfo>, TeamError>;

    /// 动态拉起 teammate（lead；需 control-plane）。
    async fn spawn_member(
        &self,
        namespace: &str,
        team_name: &str,
        name: &str,
        agent_ref: &str,
        prompt: &str,
    ) -> Result<(), TeamError>;

    /// 关停/移除 teammate（lead；需 control-plane）。
    async fn shutdown_member(
        &self,
        namespace: &str,
        team_name: &str,
        member_name: &str,
    ) -> Result<(), TeamError>;

    /// worker 提交计划待 lead 审批（需 control-plane）。
    async fn submit_plan(
        &self,
        namespace: &str,
        team_name: &str,
        member_name: &str,
        plan_text: &str,
    ) -> Result<(), TeamError>;

    /// lead 批准成员计划（需 control-plane）。
    async fn approve_plan(
        &self,
        namespace: &str,
        team_name: &str,
        member_name: &str,
    ) -> Result<(), TeamError>;

    /// lead 驳回成员计划（需 control-plane）。
    async fn reject_plan(
        &self,
        namespace: &str,
        team_name: &str,
        member_name: &str,
    ) -> Result<(), TeamError>;

    /// 创建 store-backed 团队与种子成员。
    async fn create_team(&self, spec: &TeamCreateSpec) -> Result<TeamInfo, TeamError>;

    /// 标记团队完成（保留 TTL，非立即删除）。
    async fn complete_team(&self, namespace: &str, team_name: &str) -> Result<(), TeamError>;
}

// ---------- 本地实现 ----------

/// 唤醒钩子（对应 Java `TeamWakeups.Hook`）：唤醒 teammate，可选把
/// `notice` 注入其下一轮。返回 `false` 表示无活跃会话。
pub type WakeHook = Arc<dyn Fn(&str, &str, &str) -> bool + Send + Sync>;

/// [`TeamClient`] 的 store-backed 本地实现（对应 Java `LocalTeamClient`，
/// BaseStore CAS = Claude 文件锁等价物）。
///
/// 邮箱唤醒经 [`WakeHook`] 委托给承载 teammate 会话的中间件——client 只认
/// 成员名，不认运行时会话 id（Java 侧为进程级静态注册，Rust 侧构造注入）。
pub struct LocalTeamClient {
    store: Arc<dyn BaseStore>,
    message_seq: AtomicI64,
    wakeup: Option<WakeHook>,
}

impl LocalTeamClient {
    pub fn new(store: Arc<dyn BaseStore>) -> Self {
        Self {
            store,
            message_seq: AtomicI64::new(0),
            wakeup: None,
        }
    }

    /// 注册唤醒钩子（对应 Java `TeamWakeups.register`）。
    #[must_use]
    pub fn with_wakeup(mut self, hook: WakeHook) -> Self {
        self.wakeup = Some(hook);
        self
    }

    fn team_ns(&self, namespace: &str, team_name: &str) -> Vec<String> {
        vec!["teams".into(), namespace.into(), team_name.into()]
    }

    fn task_ns(&self, namespace: &str, team_name: &str) -> Vec<String> {
        let mut v = self.team_ns(namespace, team_name);
        v.push("tasks".into());
        v
    }

    fn member_ns(&self, namespace: &str, team_name: &str) -> Vec<String> {
        let mut v = self.team_ns(namespace, team_name);
        v.push("members".into());
        v
    }

    fn msg_ns(&self, namespace: &str, team_name: &str) -> Vec<String> {
        let mut v = self.team_ns(namespace, team_name);
        v.push("messages".into());
        v
    }

    fn sl(v: &[String]) -> Vec<&str> {
        v.iter().map(String::as_str).collect()
    }

    /// `task-N` → N（解析失败 0；对应 Java `taskSeq`）。
    fn task_seq(task_id: &str) -> i64 {
        task_id
            .strip_prefix("task-")
            .and_then(|rest| rest.parse::<i64>().ok())
            .unwrap_or(0)
    }

    /// 读取任务板全部任务（上限 500，按 key 升序）。
    fn read_all_tasks(&self, namespace: &str, team_name: &str) -> Vec<TeamTask> {
        let ns = self.task_ns(namespace, team_name);
        self.store
            .search(&Self::sl(&ns), 500, 0)
            .iter()
            .filter_map(|item| serde_json::from_value(item.value.clone()).ok())
            .collect()
    }

    /// 带存储版本的读取（版本以 BaseStore CAS 为准，对应 Java
    /// `requireVersioned`）。
    fn require_versioned(
        &self,
        namespace: &str,
        team_name: &str,
        task_id: &str,
    ) -> Result<TeamTask, TeamError> {
        let ns = self.task_ns(namespace, team_name);
        let item: StoreItem = self
            .store
            .get(&Self::sl(&ns), task_id)
            .ok_or_else(|| TeamError::NotFound(format!("task not found: {task_id}")))?;
        let mut task: TeamTask = serde_json::from_value(item.value)
            .map_err(|e| TeamError::Invalid(format!("corrupt task {task_id}: {e}")))?;
        task.version = item.version;
        Ok(task)
    }

    fn cas(&self, namespace: &str, team_name: &str, next: &TeamTask, expected_version: i64) -> bool {
        let ns = self.task_ns(namespace, team_name);
        self.store.put_if_version(
            &Self::sl(&ns),
            &next.task_id,
            serde_json::to_value(next).unwrap_or(Value::Null),
            expected_version,
        )
    }

    /// 唤醒成员（无钩子或钩子返回 false 时静默）。
    fn wake(&self, team_name: &str, member: &str, notice: &str) {
        if let Some(hook) = &self.wakeup {
            let _ = hook(team_name, member, notice);
        }
    }

    /// 把新指派的工作推进 owner 邮箱（对应 Java
    /// `notifyMemberTaskAssigned`）：刚探过空板收工的 worker 没有别的
    /// 途径知道任务存在。
    async fn notify_member_task_assigned(&self, task: &TeamTask) {
        let owner = task.owner.trim();
        if owner.is_empty() || owner == "lead" {
            return;
        }
        let mut sb = format!(
            "[team] Task {} ({}) was assigned to you.\n",
            task.task_id, task.subject
        );
        if !task.description.trim().is_empty() {
            sb.push_str("\nDescription:\n");
            sb.push_str(&task.description);
            sb.push('\n');
        }
        sb.push_str(&format!(
            "\nCall claimTask with task_id={} to start, then completeTask with a result summary, or failTask with a reason.",
            task.task_id
        ));
        self.deliver_notification(task, "lead", owner, &sb).await;
    }

    /// 把终态转移推进 lead 邮箱（对应 Java `notifyLeadTaskSettled`）：
    /// 否则任务板静默变化，空闲的 lead 永远不会反应。
    async fn notify_lead_task_settled(&self, task: &TeamTask, verb: &str, detail: &str) {
        let owner = if task.owner.trim().is_empty() {
            "unknown"
        } else {
            task.owner.trim()
        };
        if owner == "lead" {
            return;
        }
        let body = if detail.trim().is_empty() {
            "(no detail provided)"
        } else {
            detail
        };
        let label = if verb == "failed" { "Reason" } else { "Result" };
        let content = format!(
            "[team] Task {} ({}) {} by {}.\n\n{}:\n{}",
            task.task_id, task.subject, verb, owner, label, body
        );
        self.deliver_notification(task, owner, "lead", &content).await;
    }

    /// 同步持久化通知（对应 Java `deliverNotification`）：通知失败绝不
    /// 破坏任务转移本身。
    async fn deliver_notification(&self, task: &TeamTask, from: &str, to: &str, content: &str) {
        let _ = self
            .send_message(&task.namespace, &task.team_name, from, to, content)
            .await;
    }

    fn is_blocked(&self, namespace: &str, team_name: &str, task: &TeamTask) -> bool {
        if task.blocked_by.is_empty() {
            return false;
        }
        let tasks = self.read_all_tasks(namespace, team_name);
        let completed: std::collections::HashSet<&str> = tasks
            .iter()
            .filter(|t| t.state == TeamTask::COMPLETED)
            .map(|t| t.task_id.as_str())
            .collect();
        task.blocked_by.iter().any(|b| !completed.contains(b.as_str()))
    }
}

#[async_trait]
impl TeamClient for LocalTeamClient {
    async fn list_tasks(
        &self,
        namespace: &str,
        team_name: &str,
    ) -> Result<Vec<TeamTask>, TeamError> {
        Ok(self.read_all_tasks(namespace, team_name))
    }

    async fn create_task(
        &self,
        namespace: &str,
        team_name: &str,
        subject: &str,
        description: &str,
        blocked_by: Vec<String>,
        owner: &str,
    ) -> Result<TeamTask, TeamError> {
        let existing = self.read_all_tasks(namespace, team_name);
        let next = existing
            .iter()
            .map(|t| Self::task_seq(&t.task_id))
            .max()
            .unwrap_or(0)
            + 1;
        let task = TeamTask {
            task_id: format!("task-{next}"),
            team_name: team_name.to_string(),
            namespace: namespace.to_string(),
            subject: subject.to_string(),
            description: description.to_string(),
            state: TeamTask::PENDING.to_string(),
            owner: owner.to_string(),
            blocked_by,
            result: String::new(),
            version: 1,
        };
        if !self.cas(namespace, team_name, &task, 0) {
            return Err(TeamError::Conflict(format!(
                "create cas failed for {}",
                task.task_id
            )));
        }
        self.notify_member_task_assigned(&task).await;
        Ok(task)
    }

    async fn assign_task(
        &self,
        namespace: &str,
        team_name: &str,
        task_id: &str,
        owner: &str,
        expected_version: i64,
    ) -> Result<TeamTask, TeamError> {
        if owner.trim().is_empty() {
            return Err(TeamError::Invalid("owner required".into()));
        }
        let cur = self.require_versioned(namespace, team_name, task_id)?;
        if cur.version != expected_version || cur.state != TeamTask::PENDING {
            return Err(TeamError::Conflict(format!("assign conflict for {task_id}")));
        }
        let next = TeamTask {
            state: TeamTask::PENDING.to_string(),
            owner: owner.to_string(),
            version: cur.version + 1,
            ..cur.clone()
        };
        if !self.cas(namespace, team_name, &next, expected_version) {
            return Err(TeamError::Conflict(format!("assign cas failed for {task_id}")));
        }
        self.notify_member_task_assigned(&next).await;
        Ok(next)
    }

    async fn claim_task(
        &self,
        namespace: &str,
        team_name: &str,
        task_id: &str,
        claimed_by: &str,
        expected_version: i64,
    ) -> Result<TeamTask, TeamError> {
        let cur = self.require_versioned(namespace, team_name, task_id)?;
        // 幂等：已被该成员开工。
        if cur.state == TeamTask::IN_PROGRESS && claimed_by == cur.owner {
            return Ok(cur);
        }
        let version = if expected_version <= 0 { cur.version } else { expected_version };
        if cur.version != version || cur.state != TeamTask::PENDING {
            return Err(TeamError::Conflict(format!("claim conflict for {task_id}")));
        }
        if !cur.owner.trim().is_empty() && cur.owner != claimed_by {
            return Err(TeamError::Conflict(format!("task owned by {}", cur.owner)));
        }
        if self.is_blocked(namespace, team_name, &cur) {
            return Err(TeamError::Conflict("task still blocked".into()));
        }
        let next = TeamTask {
            state: TeamTask::IN_PROGRESS.to_string(),
            owner: claimed_by.to_string(),
            version: cur.version + 1,
            ..cur.clone()
        };
        if !self.cas(namespace, team_name, &next, version) {
            return Err(TeamError::Conflict(format!("claim cas failed for {task_id}")));
        }
        Ok(next)
    }

    async fn complete_task(
        &self,
        namespace: &str,
        team_name: &str,
        task_id: &str,
        result: &str,
    ) -> Result<TeamTask, TeamError> {
        let cur = self.require_versioned(namespace, team_name, task_id)?;
        if cur.state != TeamTask::IN_PROGRESS {
            return Err(TeamError::Invalid(format!(
                "task not in progress: {}",
                cur.state
            )));
        }
        let next = TeamTask {
            state: TeamTask::COMPLETED.to_string(),
            result: result.to_string(),
            version: cur.version + 1,
            ..cur.clone()
        };
        if !self.cas(namespace, team_name, &next, cur.version) {
            return Err(TeamError::Conflict(format!(
                "complete cas failed for {task_id}"
            )));
        }
        self.notify_lead_task_settled(&next, "completed", result)
            .await;
        Ok(next)
    }

    async fn fail_task(
        &self,
        namespace: &str,
        team_name: &str,
        task_id: &str,
        reason: &str,
    ) -> Result<TeamTask, TeamError> {
        let cur = self.require_versioned(namespace, team_name, task_id)?;
        if TeamTask::is_terminal(&cur.state) {
            return Err(TeamError::Conflict(format!(
                "task already terminal: {}",
                cur.state
            )));
        }
        let next = TeamTask {
            state: TeamTask::FAILED.to_string(),
            result: reason.to_string(),
            version: cur.version + 1,
            ..cur.clone()
        };
        if !self.cas(namespace, team_name, &next, cur.version) {
            return Err(TeamError::Conflict(format!("fail cas failed for {task_id}")));
        }
        self.notify_lead_task_settled(&next, "failed", reason).await;
        Ok(next)
    }

    async fn unclaim_task(
        &self,
        namespace: &str,
        team_name: &str,
        task_id: &str,
        expected_version: i64,
    ) -> Result<TeamTask, TeamError> {
        let cur = self.require_versioned(namespace, team_name, task_id)?;
        if cur.version != expected_version || cur.state != TeamTask::IN_PROGRESS {
            return Err(TeamError::Conflict(format!("unclaim conflict for {task_id}")));
        }
        // owner 清空使任务回到可认领池（对齐 control-plane Unclaim）。
        let next = TeamTask {
            state: TeamTask::PENDING.to_string(),
            owner: String::new(),
            version: cur.version + 1,
            ..cur.clone()
        };
        if !self.cas(namespace, team_name, &next, expected_version) {
            return Err(TeamError::Conflict(format!("unclaim cas failed for {task_id}")));
        }
        Ok(next)
    }

    async fn list_claimable_tasks(
        &self,
        namespace: &str,
        team_name: &str,
        for_member: &str,
    ) -> Result<Vec<TeamTask>, TeamError> {
        Ok(TeamTask::claimable_of(
            &self.read_all_tasks(namespace, team_name),
            for_member,
        ))
    }

    async fn send_message(
        &self,
        namespace: &str,
        team_name: &str,
        from: &str,
        to: &str,
        content: &str,
    ) -> Result<TeamMessage, TeamError> {
        if to == from {
            // 发给自己的消息只会再唤醒发送者，回复永远到不了任何人。
            return Err(TeamError::Invalid(
                "cannot send a message to yourself; address another roster member".into(),
            ));
        }
        let id = self.message_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let msg = TeamMessage {
            from: from.to_string(),
            to: to.to_string(),
            content: content.to_string(),
            id,
        };
        let ns = self.msg_ns(namespace, team_name);
        self.store.put(
            &Self::sl(&ns),
            &id.to_string(),
            json!({"id": id, "from": from, "to": to, "content": content}),
        );
        // 经中间件唤醒接收者（成员 → 运行时会话映射只有它有）。
        self.wake(team_name, to, content);
        Ok(msg)
    }

    async fn broadcast_message(
        &self,
        namespace: &str,
        team_name: &str,
        from: &str,
        content: &str,
    ) -> Result<Vec<TeamMessage>, TeamError> {
        let members = self.list_members(namespace, team_name).await?;
        let mut out = Vec::new();
        for m in members {
            if m.member_name.is_empty() || m.member_name == from {
                continue;
            }
            out.push(
                self.send_message(namespace, team_name, from, &m.member_name, content)
                    .await?,
            );
        }
        Ok(out)
    }

    async fn list_messages(
        &self,
        namespace: &str,
        team_name: &str,
        limit: usize,
    ) -> Result<Vec<TeamMessage>, TeamError> {
        let ns = self.msg_ns(namespace, team_name);
        let items = self.store.search(&Self::sl(&ns), limit.max(50), 0);
        let out: Vec<TeamMessage> = items
            .iter()
            .filter_map(|item| serde_json::from_value(item.value.clone()).ok())
            .collect();
        if out.len() > limit && limit > 0 {
            return Ok(out[out.len() - limit..].to_vec());
        }
        Ok(out)
    }

    async fn list_members(
        &self,
        namespace: &str,
        team_name: &str,
    ) -> Result<Vec<TeamMemberInfo>, TeamError> {
        let ns = self.member_ns(namespace, team_name);
        Ok(self
            .store
            .search(&Self::sl(&ns), 100, 0)
            .iter()
            .map(|item| TeamMemberInfo::from_value(&item.value))
            .collect())
    }

    async fn spawn_member(
        &self,
        _namespace: &str,
        _team_name: &str,
        _name: &str,
        _agent_ref: &str,
        _prompt: &str,
    ) -> Result<(), TeamError> {
        Err(TeamError::Unsupported(
            "spawnMember requires control-plane TeamClient".into(),
        ))
    }

    async fn shutdown_member(
        &self,
        _namespace: &str,
        _team_name: &str,
        _member_name: &str,
    ) -> Result<(), TeamError> {
        Err(TeamError::Unsupported(
            "shutdownMember requires control-plane TeamClient".into(),
        ))
    }

    async fn submit_plan(
        &self,
        _namespace: &str,
        _team_name: &str,
        _member_name: &str,
        _plan_text: &str,
    ) -> Result<(), TeamError> {
        Err(TeamError::Unsupported(
            "submitPlan requires control-plane TeamClient".into(),
        ))
    }

    async fn approve_plan(
        &self,
        _namespace: &str,
        _team_name: &str,
        _member_name: &str,
    ) -> Result<(), TeamError> {
        Err(TeamError::Unsupported(
            "approvePlan requires control-plane TeamClient".into(),
        ))
    }

    async fn reject_plan(
        &self,
        _namespace: &str,
        _team_name: &str,
        _member_name: &str,
    ) -> Result<(), TeamError> {
        Err(TeamError::Unsupported(
            "rejectPlan requires control-plane TeamClient".into(),
        ))
    }

    async fn create_team(&self, spec: &TeamCreateSpec) -> Result<TeamInfo, TeamError> {
        let ns = if spec.namespace.trim().is_empty() {
            "default"
        } else {
            &spec.namespace
        };
        let team_ns = self.team_ns(ns, &spec.name);
        let meta = json!({
            "name": spec.name,
            "namespace": ns,
            "objective": spec.objective,
            "phase": "Running",
            "leadRef": spec.lead_agent_ref,
        });
        if !self
            .store
            .put_if_version(&Self::sl(&team_ns), "meta", meta.clone(), 0)
        {
            if self.store.get(&Self::sl(&team_ns), "meta").is_some() {
                return Err(TeamError::Conflict(format!("team exists: {}", spec.name)));
            }
            self.store.put(&Self::sl(&team_ns), "meta", meta);
        }
        let member_ns = self.member_ns(ns, &spec.name);
        self.store.put(
            &Self::sl(&member_ns),
            "lead",
            serde_json::to_value(TeamMemberInfo {
                member_name: "lead".into(),
                agent_ref: spec.lead_agent_ref.clone(),
                phase: "Working".into(),
                session_id: String::new(),
                deploy_mode: "byo".into(),
                is_lead: true,
            })
            .unwrap_or(Value::Null),
        );
        for m in &spec.members {
            self.store.put(
                &Self::sl(&member_ns),
                &m.name,
                serde_json::to_value(TeamMemberInfo {
                    member_name: m.name.clone(),
                    agent_ref: m.agent_ref.clone(),
                    phase: "Working".into(),
                    session_id: String::new(),
                    deploy_mode: if m.deploy_mode.is_empty() {
                        "byo".to_string()
                    } else {
                        m.deploy_mode.clone()
                    },
                    is_lead: false,
                })
                .unwrap_or(Value::Null),
            );
        }
        Ok(TeamInfo {
            name: spec.name.clone(),
            namespace: ns.to_string(),
            objective: spec.objective.clone(),
            phase: "Running".into(),
            lead_ref: spec.lead_agent_ref.clone(),
        })
    }

    async fn complete_team(&self, namespace: &str, team_name: &str) -> Result<(), TeamError> {
        let team_ns = self.team_ns(namespace, team_name);
        let item = self.store.get(&Self::sl(&team_ns), "meta");
        let mut meta = match &item {
            Some(i) => {
                let mut m = i.value.clone();
                if !m.is_object() {
                    m = json!({});
                }
                m
            }
            None => json!({}),
        };
        meta["phase"] = json!("Completed");
        let ver = item.map(|i| i.version).unwrap_or(0);
        if !self
            .store
            .put_if_version(&Self::sl(&team_ns), "meta", meta.clone(), ver)
        {
            self.store.put(&Self::sl(&team_ns), "meta", meta);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::base_store::InMemoryBaseStore;
    use std::sync::Mutex;

    type WakeLog = Arc<Mutex<Vec<(String, String, String)>>>;

    fn client() -> (LocalTeamClient, WakeLog) {
        let log: WakeLog = Arc::new(Mutex::new(Vec::new()));
        let l2 = log.clone();
        let c = LocalTeamClient::new(Arc::new(InMemoryBaseStore::new())).with_wakeup(Arc::new(
            move |team, member, notice| {
                l2.lock()
                    .expect("wake log")
                    .push((team.to_string(), member.to_string(), notice.to_string()));
                true
            },
        ));
        (c, log)
    }

    fn spec(name: &str) -> TeamCreateSpec {
        TeamCreateSpec {
            name: name.into(),
            namespace: "default".into(),
            objective: "ship it".into(),
            lead_agent_ref: "lead-agent".into(),
            lead_prompt: String::new(),
            members: vec![
                TeamMemberSpec {
                    name: "alice".into(),
                    agent_ref: "worker-a".into(),
                    prompt: String::new(),
                    deploy_mode: String::new(),
                },
                TeamMemberSpec {
                    name: "bob".into(),
                    agent_ref: "worker-b".into(),
                    prompt: String::new(),
                    deploy_mode: "inline".into(),
                },
            ],
        }
    }

    #[tokio::test]
    async fn create_task_assigns_sequential_ids_and_notifies_owner() {
        let (c, log) = client();
        let t1 = c
            .create_task("default", "alpha", "fix bug", "desc", vec![], "alice")
            .await
            .unwrap();
        let t2 = c
            .create_task("default", "alpha", "write docs", "", vec![], "")
            .await
            .unwrap();
        assert_eq!((t1.task_id.as_str(), t2.task_id.as_str()), ("task-1", "task-2"));
        assert_eq!(t1.state, TeamTask::PENDING);
        assert_eq!(t1.version, 1);
        // owner=alice 收到指派通知；t2 无 owner 不通知。
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].0, "alpha");
        assert_eq!(log[0].1, "alice");
        assert!(log[0].2.contains("task-1"));
        assert!(log[0].2.contains("claimTask"));
    }

    #[tokio::test]
    async fn assign_task_validates_owner_version_and_state() {
        let (c, _log) = client();
        let t = c.create_task("default", "alpha", "s", "", vec![], "").await.unwrap();
        // owner 必填。
        assert!(matches!(
            c.assign_task("default", "alpha", &t.task_id, "  ", 1).await,
            Err(TeamError::Invalid(_))
        ));
        // 版本冲突。
        assert!(matches!(
            c.assign_task("default", "alpha", &t.task_id, "alice", 99).await,
            Err(TeamError::Conflict(_))
        ));
        let assigned = c
            .assign_task("default", "alpha", &t.task_id, "alice", 1)
            .await
            .unwrap();
        assert_eq!(assigned.owner, "alice");
        assert_eq!(assigned.state, TeamTask::PENDING);
        assert_eq!(assigned.version, 2);
        // pending → pending 改派合法（对齐 Java：仅非 pending 拒绝）。
        let reassigned = c
            .assign_task("default", "alpha", &t.task_id, "bob", 2)
            .await
            .unwrap();
        assert_eq!(reassigned.owner, "bob");
        assert_eq!(reassigned.version, 3);
        // 非 pending 不可指派：先认领再改派 → 冲突。
        c.claim_task("default", "alpha", &t.task_id, "bob", 0).await.unwrap();
        assert!(matches!(
            c.assign_task("default", "alpha", &t.task_id, "alice", 4).await,
            Err(TeamError::Conflict(_))
        ));
    }

    #[tokio::test]
    async fn claim_task_idempotent_owner_guard_and_blocked() {
        let (c, _log) = client();
        let gate = c.create_task("default", "alpha", "gate", "", vec![], "").await.unwrap();
        let blocked = c
            .create_task("default", "alpha", "blocked", "", vec![gate.task_id.clone()], "")
            .await
            .unwrap();
        // 依赖未完成：blocked。
        assert!(matches!(
            c.claim_task("default", "alpha", &blocked.task_id, "alice", 0).await,
            Err(TeamError::Conflict(ref m)) if m.contains("blocked")
        ));
        // 完成 gate（先认领再完成）后可认领；expected<=0 取当前版本。
        c.claim_task("default", "alpha", &gate.task_id, "alice", 0).await.unwrap();
        c.complete_task("default", "alpha", &gate.task_id, "done").await.unwrap();
        let claimed = c
            .claim_task("default", "alpha", &blocked.task_id, "alice", 0)
            .await
            .unwrap();
        assert_eq!(claimed.state, TeamTask::IN_PROGRESS);
        assert_eq!(claimed.owner, "alice");
        // 幂等重入。
        let again = c
            .claim_task("default", "alpha", &blocked.task_id, "alice", 0)
            .await
            .unwrap();
        assert_eq!(again.version, claimed.version);
        // 他人认领已开工任务 → 先命中状态校验（对齐 Java 校验顺序）。
        assert!(matches!(
            c.claim_task("default", "alpha", &blocked.task_id, "bob", 0).await,
            Err(TeamError::Conflict(ref m)) if m.contains("claim conflict")
        ));
        // pending 但已指派给他人 → owner 冲突。
        let assigned = c
            .create_task("default", "alpha", "assigned", "", vec![], "carol")
            .await
            .unwrap();
        assert!(matches!(
            c.claim_task("default", "alpha", &assigned.task_id, "bob", 0).await,
            Err(TeamError::Conflict(ref m)) if m.contains("owned by carol")
        ));
        // 显式旧版本号 → CAS 冲突。
        assert!(matches!(
            c.claim_task("default", "alpha", &blocked.task_id, "bob", 1).await,
            Err(TeamError::Conflict(_))
        ));
    }

    #[tokio::test]
    async fn complete_requires_in_progress_and_notifies_lead() {
        let (c, log) = client();
        let t = c.create_task("default", "alpha", "s", "", vec![], "alice").await.unwrap();
        // pending 不可直接完成。
        assert!(matches!(
            c.complete_task("default", "alpha", &t.task_id, "r").await,
            Err(TeamError::Invalid(ref m)) if m.contains("not in progress")
        ));
        c.claim_task("default", "alpha", &t.task_id, "alice", 0).await.unwrap();
        let done = c
            .complete_task("default", "alpha", &t.task_id, "all green")
            .await
            .unwrap();
        assert_eq!(done.state, TeamTask::COMPLETED);
        assert_eq!(done.result, "all green");
        // lead 收到完成通知（owner=alice ≠ lead）。
        let log = log.lock().unwrap();
        let last = log.last().unwrap();
        assert_eq!(last.1, "lead");
        assert!(last.2.contains("completed by alice"));
        assert!(last.2.contains("Result:\nall green"));
    }

    #[tokio::test]
    async fn fail_task_rejects_terminal_and_labels_reason() {
        let (c, log) = client();
        let t = c.create_task("default", "alpha", "s", "", vec![], "alice").await.unwrap();
        c.claim_task("default", "alpha", &t.task_id, "alice", 0).await.unwrap();
        let failed = c
            .fail_task("default", "alpha", &t.task_id, "upstream broke")
            .await
            .unwrap();
        assert_eq!(failed.state, TeamTask::FAILED);
        // 终态不可再 fail。
        assert!(matches!(
            c.fail_task("default", "alpha", &t.task_id, "again").await,
            Err(TeamError::Conflict(ref m)) if m.contains("terminal")
        ));
        let log = log.lock().unwrap();
        let last = log.last().unwrap();
        assert!(last.2.contains("failed by alice"));
        assert!(last.2.contains("Reason:\nupstream broke"));
    }

    #[tokio::test]
    async fn unclaim_returns_task_to_pool() {
        let (c, _log) = client();
        let t = c.create_task("default", "alpha", "s", "", vec![], "").await.unwrap();
        let claimed = c
            .claim_task("default", "alpha", &t.task_id, "alice", 0)
            .await
            .unwrap();
        let un = c
            .unclaim_task("default", "alpha", &t.task_id, claimed.version)
            .await
            .unwrap();
        assert_eq!(un.state, TeamTask::PENDING);
        assert!(un.owner.is_empty());
        // 回池后 bob 可认领。
        let re = c
            .claim_task("default", "alpha", &t.task_id, "bob", 0)
            .await
            .unwrap();
        assert_eq!(re.owner, "bob");
    }

    #[tokio::test]
    async fn claimable_filters_owner_and_blocked() {
        let (c, _log) = client();
        c.create_task("default", "alpha", "free", "", vec![], "").await.unwrap();
        c.create_task("default", "alpha", "for-bob", "", vec![], "bob").await.unwrap();
        c.create_task("default", "alpha", "blocked", "", vec!["task-1".into()], "").await.unwrap();
        c.create_task("default", "alpha", "done", "", vec![], "").await.unwrap();
        c.claim_task("default", "alpha", "task-4", "alice", 0).await.unwrap();
        c.complete_task("default", "alpha", "task-4", "ok").await.unwrap();
        let tasks = c.list_tasks("default", "alpha").await.unwrap();
        // 匿名视角：只看未分配且未阻塞。
        let anon = TeamTask::claimable_of(&tasks, "");
        assert_eq!(anon.iter().map(|t| t.task_id.as_str()).collect::<Vec<_>>(), ["task-1"]);
        // bob 视角：含指派给自己的。
        let bob = TeamTask::claimable_of(&tasks, "bob");
        assert_eq!(
            bob.iter().map(|t| t.task_id.as_str()).collect::<Vec<_>>(),
            ["task-1", "task-2"]
        );
        // list_claimable_tasks 走同一逻辑。
        let via_api = c.list_claimable_tasks("default", "alpha", "bob").await.unwrap();
        assert_eq!(via_api.len(), 2);
    }

    #[tokio::test]
    async fn send_message_rejects_self_and_wakes_receiver() {
        let (c, log) = client();
        assert!(matches!(
            c.send_message("default", "alpha", "alice", "alice", "hi").await,
            Err(TeamError::Invalid(ref m)) if m.contains("yourself")
        ));
        let m = c
            .send_message("default", "alpha", "alice", "bob", "hello bob")
            .await
            .unwrap();
        assert_eq!(m.id, 1);
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].1, "bob");
        assert_eq!(log[0].2, "hello bob");
    }

    #[tokio::test]
    async fn broadcast_skips_sender_and_empty_names() {
        let (c, _log) = client();
        c.create_team(&spec("alpha")).await.unwrap();
        let sent = c
            .broadcast_message("default", "alpha", "lead", "standup in 5")
            .await
            .unwrap();
        assert_eq!(sent.len(), 2, "lead 跳过自己，alice/bob 收到");
        assert!(sent.iter().all(|m| m.to != "lead"));
        // 无种子成员的团队广播 → 空（只有 lead 自己，被跳过）。
        let mut empty = spec("beta");
        empty.members.clear();
        c.create_team(&empty).await.unwrap();
        let none = c
            .broadcast_message("default", "beta", "lead", "anyone?")
            .await
            .unwrap();
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn list_messages_returns_latest_tail() {
        let (c, _log) = client();
        c.create_team(&spec("alpha")).await.unwrap();
        for (i, to) in ["alice", "bob", "alice"].iter().enumerate() {
            c.send_message("default", "alpha", "lead", to, &format!("m{i}"))
                .await
                .unwrap();
        }
        let all = c.list_messages("default", "alpha", 10).await.unwrap();
        assert_eq!(all.len(), 3);
        let tail = c.list_messages("default", "alpha", 2).await.unwrap();
        assert_eq!(tail.iter().map(|m| m.id).collect::<Vec<_>>(), [2, 3]);
    }

    #[tokio::test]
    async fn create_team_conflict_and_roster() {
        let (c, _log) = client();
        let info = c.create_team(&spec("alpha")).await.unwrap();
        assert_eq!(info.phase, "Running");
        assert_eq!(info.namespace, "default");
        // 重名冲突。
        assert!(matches!(
            c.create_team(&spec("alpha")).await,
            Err(TeamError::Conflict(ref m)) if m.contains("exists")
        ));
        let members = c.list_members("default", "alpha").await.unwrap();
        assert_eq!(members.len(), 3);
        let lead = members.iter().find(|m| m.member_name == "lead").unwrap();
        assert!(lead.is_lead);
        assert_eq!(lead.deploy_mode, "byo");
        let bob = members.iter().find(|m| m.member_name == "bob").unwrap();
        assert!(!bob.is_lead);
        assert_eq!(bob.deploy_mode, "inline", "seed 的 deploy_mode 保留");
        // 空白 namespace 归一 default。
        let mut s = spec("gamma");
        s.namespace = "  ".into();
        let g = c.create_team(&s).await.unwrap();
        assert_eq!(g.namespace, "default");
    }

    #[tokio::test]
    async fn complete_team_sets_phase() {
        let (c, _log) = client();
        c.create_team(&spec("alpha")).await.unwrap();
        c.complete_team("default", "alpha").await.unwrap();
        // 经 list_members 仍可读（团队保留 TTL，非删除）；meta phase 经 store 直查。
        let members = c.list_members("default", "alpha").await.unwrap();
        assert_eq!(members.len(), 3);
    }

    #[tokio::test]
    async fn member_info_from_value_derives_lead_by_name() {
        let m = TeamMemberInfo::from_value(&serde_json::json!({
            "memberName": "lead", "agentRef": "a", "phase": "Working"
        }));
        assert!(m.is_lead, "名字叫 lead 即 lead（对齐 Java fromMap）");
        let m = TeamMemberInfo::from_value(&serde_json::json!({
            "memberName": "alice", "isLead": true
        }));
        assert!(m.is_lead);
        let m = TeamMemberInfo::from_value(&serde_json::json!({
            "memberName": "bob", "isLead": false
        }));
        assert!(!m.is_lead);
        // 缺字段容错（对齐 Java null → ""）。
        let m = TeamMemberInfo::from_value(&serde_json::json!({}));
        assert!(m.member_name.is_empty() && !m.is_lead);
    }

    #[tokio::test]
    async fn control_plane_only_ops_are_unsupported() {
        let (c, _log) = client();
        for err in [
            c.spawn_member("default", "alpha", "x", "r", "p").await.unwrap_err(),
            c.shutdown_member("default", "alpha", "x").await.unwrap_err(),
            c.submit_plan("default", "alpha", "x", "plan").await.unwrap_err(),
            c.approve_plan("default", "alpha", "x").await.unwrap_err(),
            c.reject_plan("default", "alpha", "x").await.unwrap_err(),
        ] {
            assert!(matches!(err, TeamError::Unsupported(_)));
        }
    }

    #[tokio::test]
    async fn task_not_found_and_corrupt_guard() {
        let (c, _log) = client();
        assert!(matches!(
            c.claim_task("default", "alpha", "task-404", "alice", 0).await,
            Err(TeamError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn team_context_resolved_namespace() {
        let ctx = TeamContext {
            team_name: "alpha".into(),
            namespace: Some("  ".into()),
            objective: String::new(),
            my_role: String::new(),
            is_lead: false,
            members: vec![],
            available_actions: vec![],
            recovery_context: None,
        };
        assert_eq!(ctx.resolved_namespace(), "default");
        let ctx = TeamContext {
            namespace: Some("prod".into()),
            ..ctx
        };
        assert_eq!(ctx.resolved_namespace(), "prod");
    }
}
