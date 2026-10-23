//! 后台任务：状态机、记录、投递与仓库（对应 Java `subagent.task` 包）。
//!
//! Java 侧 `TaskRepository` 是接口 + `WorkspaceTaskRepository`（workspace 持久化 +
//! heartbeat/orphan sweeper）；本批交付核心抽象与内存实现，workspace 持久化的
//! `TaskRecord` 存储在 [`crate::workspace::WorkspaceManager`] 上
//! （`tasks/` 目录 JSON 文件），仓库级编排随后续批次补齐。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agentscope_core::context::RuntimeContext;
use futures_util::FutureExt;
use serde::{Deserialize, Serialize};

use crate::workspace::WorkspaceManager;

/// 后台任务生命周期状态（对应 Java `TaskStatus`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl TaskStatus {
    /// 终态：到达后状态不再变化，可跳过轮询（对应 Java `isTerminal()`）。
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
        )
    }

    /// 全部状态的小写 wire 名 → 枚举（供 task_list 的 status_filter 解析）。
    pub fn as_str_all() -> &'static [(&'static str, TaskStatus)] {
        &[
            ("pending", TaskStatus::Pending),
            ("running", TaskStatus::Running),
            ("completed", TaskStatus::Completed),
            ("failed", TaskStatus::Failed),
            ("cancelled", TaskStatus::Cancelled),
        ]
    }

    /// Java wire 值（SCREAMING_SNAKE_CASE）。
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Pending => "PENDING",
            TaskStatus::Running => "RUNNING",
            TaskStatus::Completed => "COMPLETED",
            TaskStatus::Failed => "FAILED",
            TaskStatus::Cancelled => "CANCELLED",
        }
    }
}

/// 本地/远端执行规格（对应 Java `TaskRunSpec` sealed interface）。
pub enum TaskRunSpec {
    /// 进程内执行：闭包产出最终文本（对应 Java `LocalTaskRunSpec`）。
    Local {
        execution: Arc<
            dyn Fn() -> futures_util::future::BoxFuture<'static, Result<String, String>>
                + Send
                + Sync,
        >,
    },
    /// 远端 HTTP 任务执行（对应 Java `RemoteTaskRunSpec`）；
    /// transport 随分布式批次实现，本批只登记记录。
    Remote {
        base_url: String,
        headers: HashMap<String, String>,
        agent_id: String,
        input: String,
    },
    /// 接管一个已在运行的句柄（对应 Java `AdoptedTaskRunSpec`）：
    /// 同步执行超时晋升为异步时，future 仍在跑，只需状态跟踪回调，
    /// 不再向执行器提交新任务。
    Adopted {
        handle: tokio::task::JoinHandle<Result<String, String>>,
    },
}

impl std::fmt::Debug for TaskRunSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskRunSpec::Local { .. } => f.write_str("TaskRunSpec::Local"),
            TaskRunSpec::Remote { base_url, .. } => {
                write!(f, "TaskRunSpec::Remote({base_url})")
            }
            TaskRunSpec::Adopted { .. } => f.write_str("TaskRunSpec::Adopted"),
        }
    }
}

/// 一条后台任务的持久化记录（对应 Java `TaskRecord`）。
///
/// 时间字段为 epoch 毫秒（Java 是 ISO-8601 字符串——有意差异，见 lib 文档）。
/// remote 相关字段（`remote_base_url`/`remote_headers`/`pending_confirms`/
/// `last_event_seq`）随记录保留，分布式批次启用。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRecord {
    pub task_id: String,
    pub sub_agent_id: String,
    pub parent_agent_id: String,
    pub parent_session_id: String,
    pub sub_session_id: String,
    pub status: TaskStatus,
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub error_message: Option<String>,
    #[serde(default)]
    pub cancel_requested: bool,
    pub created_at: i64,
    #[serde(default)]
    pub last_checked_at: Option<i64>,
    #[serde(default)]
    pub last_updated_at: Option<i64>,
    #[serde(default)]
    pub delivered_at: Option<i64>,
    pub transport_type: String,
    #[serde(default)]
    pub remote_base_url: Option<String>,
    #[serde(default)]
    pub remote_headers: HashMap<String, String>,
    #[serde(default)]
    pub awaiting_confirm: bool,
    #[serde(default)]
    pub pending_confirms: Vec<String>,
    #[serde(default)]
    pub last_event_seq: i64,
}

impl TaskRecord {
    /// 新建 PENDING 记录。
    pub fn pending(
        task_id: impl Into<String>,
        sub_agent_id: impl Into<String>,
        parent_agent_id: impl Into<String>,
        parent_session_id: impl Into<String>,
        sub_session_id: impl Into<String>,
    ) -> Self {
        let now = now_millis();
        Self {
            task_id: task_id.into(),
            sub_agent_id: sub_agent_id.into(),
            parent_agent_id: parent_agent_id.into(),
            parent_session_id: parent_session_id.into(),
            sub_session_id: sub_session_id.into(),
            status: TaskStatus::Pending,
            result: None,
            error_message: None,
            cancel_requested: false,
            created_at: now,
            last_checked_at: None,
            last_updated_at: Some(now),
            delivered_at: None,
            transport_type: "local".to_string(),
            remote_base_url: None,
            remote_headers: HashMap::new(),
            awaiting_confirm: false,
            pending_confirms: Vec::new(),
            last_event_seq: 0,
        }
    }
}

pub(crate) fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// 终态任务的投递快照（对应 Java `TaskDelivery` record）：
/// 字段从 `TaskRecord` 有意去规范化——消费者只需要投影，
/// 快照先行避免与后续状态变更竞态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskDelivery {
    pub task_id: String,
    pub agent_id: String,
    /// 终态之一：COMPLETED / FAILED / CANCELLED。
    pub status: TaskStatus,
    /// 完成载荷；FAILED / CANCELLED 时为 `None`。
    pub result: Option<String>,
    /// 失败信息；COMPLETED / CANCELLED 时为 `None`。
    pub error_message: Option<String>,
    /// 到达终态的墙钟时间（best-effort，取 `last_updated_at`）。
    pub completed_at: Option<i64>,
}

/// 运行中的任务句柄（对应 Java `BackgroundTask`）：
/// 包装执行句柄与生命周期记录，线程安全。
pub struct BackgroundTask {
    pub record: Mutex<TaskRecord>,
    /// 本地执行的 join 句柄；远端任务为 `None`（由 sweeper 轮询）。
    pub handle: Mutex<Option<tokio::task::JoinHandle<Result<String, String>>>>,
    pub created_at: i64,
    pub last_checked_at: Mutex<Option<i64>>,
}

impl BackgroundTask {
    /// 当前状态快照。
    pub fn status(&self) -> TaskStatus {
        self.record
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .status
    }

    /// 人类可读状态串（对应 Java `getStatus()`）：
    /// 失败时 `Failed: <message>`，其余为首字母大写的状态名。
    pub fn status_text(&self) -> String {
        let record = self.record.lock().unwrap_or_else(|e| e.into_inner());
        match record.status {
            TaskStatus::Failed => format!(
                "Failed: {}",
                record.error_message.as_deref().unwrap_or("Unknown error")
            ),
            other => {
                let name = other.as_str();
                format!("{}{}", &name[..1], name[1..].to_ascii_lowercase())
            }
        }
    }

    /// 非阻塞结果窥视；未完成或失败时返回 `None`（对应 Java `getResult()`）。
    pub fn result(&self) -> Option<String> {
        self.record
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .result
            .clone()
    }

    /// 标记「刚被父代理检查过」（对应 Java `updateLastCheckedAt()`）。
    pub fn update_last_checked_at(&self) {
        *self.last_checked_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(now_millis());
    }
}

/// 任务完成回调（对应 Java `TaskRepository.TaskCompletionCallback`）：
/// 任务到达终态（COMPLETED / FAILED）时触发；失败任务 `result` 为 `None`。
pub trait CompletionCallback: Send + Sync {
    fn on_completed(
        &self,
        rc: &RuntimeContext,
        task_id: &str,
        sub_agent_id: &str,
        session_id: &str,
        result: Option<String>,
    );
}

/// 任务仓库抽象（对应 Java `TaskRepository`）。
///
/// 所有方法按 `(session_id, task_id)` 寻址；`session_id` 为空时回退
/// `rc.session_id()`，再为空用 `"default"`。
#[async_trait::async_trait]
pub trait TaskRepository: Send + Sync {
    /// 取任务；不存在返回 `None`。
    fn get_task(
        &self,
        rc: &RuntimeContext,
        session_id: &str,
        task_id: &str,
    ) -> Option<Arc<BackgroundTask>>;

    /// 按 [`TaskRunSpec`] 提交新任务（对应 Java `putTask`）。
    fn put_task(
        &self,
        rc: &RuntimeContext,
        task_id: &str,
        sub_agent_id: &str,
        session_id: &str,
        spec: TaskRunSpec,
    ) -> Result<Arc<BackgroundTask>, String>;

    /// 列出会话任务，可按状态过滤。
    fn list_tasks(
        &self,
        rc: &RuntimeContext,
        session_id: &str,
        status: Option<TaskStatus>,
    ) -> Vec<Arc<BackgroundTask>>;

    /// 请求取消；任务不存在或已终态返回 `false`。
    fn cancel_task(&self, rc: &RuntimeContext, session_id: &str, task_id: &str) -> bool;

    /// 找出全部「已终态但尚未投递」的任务（对应 Java `findPendingDeliveries`），
    /// 按完成时间升序——父代理按实际发生顺序看到投递。
    fn find_pending_deliveries(
        &self,
        rc: &RuntimeContext,
        session_id: &str,
    ) -> Vec<TaskDelivery>;

    /// 标记已投递（幂等）。
    fn mark_delivered(&self, rc: &RuntimeContext, session_id: &str, task_id: &str);

    /// 是否已投递（用于从系统提示摘要里省略已投递的终态任务）。
    fn is_delivered(&self, rc: &RuntimeContext, session_id: &str, task_id: &str) -> bool;

    /// 注册完成回调（每次覆盖；保留执行所有权的调用方可用
    /// `harness.suppressTaskCompletionCallback` 属性抑制回调）。
    fn set_completion_callback(&self, callback: Arc<dyn CompletionCallback>);

    /// 停止全部运行中任务（进程退出前调用）。
    fn shutdown(&self);
}

/// `RuntimeContext` 标志：调用方保留执行所有权并自行收集结果，
/// 抑制 inbox/wakeup 回调（对应 Java `SUPPRESS_COMPLETION_CALLBACK`）。
pub const SUPPRESS_COMPLETION_CALLBACK: &str = "harness.suppressTaskCompletionCallback";

/// 内存实现（pull/push 皆可；workspace 持久化随后续批次）。
pub struct InMemoryTaskRepository {
    tasks: Mutex<HashMap<String, Arc<BackgroundTask>>>,
    callback: Mutex<Option<Arc<dyn CompletionCallback>>>,
}

impl InMemoryTaskRepository {
    /// 空仓库。
    pub fn new() -> Self {
        Self {
            tasks: Mutex::new(HashMap::new()),
            callback: Mutex::new(None),
        }
    }

    fn key(session_id: &str, task_id: &str) -> String {
        format!("{session_id}/{task_id}")
    }

    fn session_key(rc: &RuntimeContext, session_id: &str) -> String {
        if !session_id.trim().is_empty() {
            return session_id.to_string();
        }
        rc.session_id().unwrap_or("default").to_string()
    }

    /// 把 join 句柄的当前状态结算进记录（终态只结算一次）；
    /// 首次结算到终态时触发完成回调。
    fn settle(&self, task: &BackgroundTask) {
        let newly_terminal = {
            let mut record = task.record.lock().unwrap_or_else(|e| e.into_inner());
            if record.status.is_terminal() {
                return;
            }
            let mut handle_guard = task.handle.lock().unwrap_or_else(|e| e.into_inner());
            let Some(handle) = handle_guard.as_ref() else {
                // 远端任务无句柄：PENDING → RUNNING 由 sweeper 推进，这里不猜
                if record.status == TaskStatus::Pending {
                    record.status = TaskStatus::Running;
                    record.last_updated_at = Some(now_millis());
                }
                return;
            };
            if !handle.is_finished() {
                if record.status == TaskStatus::Pending {
                    record.status = TaskStatus::Running;
                    record.last_updated_at = Some(now_millis());
                }
                return;
            }
            // 句柄已结束：取出结算（is_finished 后 now_or_never 必有结果；
            // take 后 Option 为 None，但记录已终态，后续 settle 直接短路）
            let handle = handle_guard.take().unwrap();
            match handle.now_or_never() {
                Some(Ok(Ok(result))) => {
                    record.result = Some(result);
                    record.status = TaskStatus::Completed;
                }
                Some(Ok(Err(error))) => {
                    record.error_message = Some(error);
                    record.status = TaskStatus::Failed;
                }
                Some(Err(_)) | None => {
                    // JoinError（panic/取消）：保守标记失败
                    record.error_message = Some("task aborted".to_string());
                    record.status = TaskStatus::Failed;
                }
            }
            record.last_updated_at = Some(now_millis());
            true
        };
        if newly_terminal {
            self.fire_completion_callback(task);
        }
    }

    /// 触发完成回调（受 `SUPPRESS_COMPLETION_CALLBACK` 抑制）。
    fn fire_completion_callback(&self, task: &BackgroundTask) {
        let callback = self
            .callback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(callback) = callback else {
            return;
        };
        let (task_id, sub_agent_id, session_id, result, suppressed) = {
            let record = task.record.lock().unwrap_or_else(|e| e.into_inner());
            (
                record.task_id.clone(),
                record.sub_agent_id.clone(),
                record.parent_session_id.clone(),
                record.result.clone(),
                record.status == TaskStatus::Cancelled,
            )
        };
        if suppressed {
            return;
        }
        let rc = RuntimeContext::new().with_session_id(session_id.clone());
        callback.on_completed(&rc, &task_id, &sub_agent_id, &session_id, result);
    }
}

impl Default for InMemoryTaskRepository {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl TaskRepository for InMemoryTaskRepository {
    fn get_task(
        &self,
        _rc: &RuntimeContext,
        session_id: &str,
        task_id: &str,
    ) -> Option<Arc<BackgroundTask>> {
        let tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        tasks
            .get(&Self::key(session_id, task_id))
            .map(|task| {
                self.settle(task);
                task.clone()
            })
    }

    fn put_task(
        &self,
        rc: &RuntimeContext,
        task_id: &str,
        sub_agent_id: &str,
        session_id: &str,
        spec: TaskRunSpec,
    ) -> Result<Arc<BackgroundTask>, String> {
        let session = Self::session_key(rc, session_id);
        let parent_session = rc.session_id().unwrap_or("default").to_string();
        let mut record =
            TaskRecord::pending(task_id, sub_agent_id, "parent", &parent_session, &session);
        let handle = match spec {
            TaskRunSpec::Local { execution } => Some(tokio::spawn(execution())),
            TaskRunSpec::Remote {
                base_url,
                headers,
                ..
            } => {
                record.transport_type = "remote".to_string();
                record.remote_base_url = Some(base_url);
                record.remote_headers = headers;
                None
            }
            TaskRunSpec::Adopted { handle } => Some(handle),
        };
        let task = Arc::new(BackgroundTask {
            record: Mutex::new(record),
            handle: Mutex::new(handle),
            created_at: now_millis(),
            last_checked_at: Mutex::new(None),
        });
        let key = Self::key(&session, task_id);
        let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        tasks.insert(key, task.clone());
        Ok(task)
    }

    fn list_tasks(
        &self,
        rc: &RuntimeContext,
        session_id: &str,
        status: Option<TaskStatus>,
    ) -> Vec<Arc<BackgroundTask>> {
        let session = Self::session_key(rc, session_id);
        let prefix = format!("{session}/");
        let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        let mut matched: Vec<Arc<BackgroundTask>> = tasks
            .iter_mut()
            .filter(|(key, _)| key.starts_with(&prefix))
            .filter(|(_, task)| {
                self.settle(task);
                status.is_none_or(|want| task.status() == want)
            })
            .map(|(_, task)| task.clone())
            .collect();
        matched.sort_by_key(|task| task.created_at);
        matched
    }

    fn cancel_task(&self, rc: &RuntimeContext, session_id: &str, task_id: &str) -> bool {
        let Some(task) = self.get_task(rc, session_id, task_id) else {
            return false;
        };
        let mut record = task.record.lock().unwrap_or_else(|e| e.into_inner());
        if record.status.is_terminal() {
            return false;
        }
        record.cancel_requested = true;
        record.status = TaskStatus::Cancelled;
        record.last_updated_at = Some(now_millis());
        drop(record);
        if let Some(handle) = task.handle.lock().unwrap_or_else(|e| e.into_inner()).take() {
            handle.abort();
        }
        true
    }

    fn find_pending_deliveries(
        &self,
        rc: &RuntimeContext,
        session_id: &str,
    ) -> Vec<TaskDelivery> {
        self.list_tasks(rc, session_id, None)
            .into_iter()
            .filter(|task| task.status().is_terminal())
            .filter(|task| {
                task.record
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .delivered_at
                    .is_none()
            })
            .map(|task| {
                let record = task.record.lock().unwrap_or_else(|e| e.into_inner());
                TaskDelivery {
                    task_id: record.task_id.clone(),
                    agent_id: record.sub_agent_id.clone(),
                    status: record.status,
                    result: record.result.clone(),
                    error_message: record.error_message.clone(),
                    completed_at: record.last_updated_at,
                }
            })
            .collect()
    }

    fn mark_delivered(&self, rc: &RuntimeContext, session_id: &str, task_id: &str) {
        if let Some(task) = self.get_task(rc, session_id, task_id) {
            let mut record = task.record.lock().unwrap_or_else(|e| e.into_inner());
            record.delivered_at = Some(now_millis());
        }
    }

    fn is_delivered(&self, rc: &RuntimeContext, session_id: &str, task_id: &str) -> bool {
        self.get_task(rc, session_id, task_id)
            .is_some_and(|task| {
                task.record
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .delivered_at
                    .is_some()
            })
    }

    fn set_completion_callback(&self, callback: Arc<dyn CompletionCallback>) {
        *self.callback.lock().unwrap_or_else(|e| e.into_inner()) = Some(callback);
    }

    fn shutdown(&self) {
        let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        for task in tasks.values_mut() {
            if let Some(handle) = task.handle.lock().unwrap_or_else(|e| e.into_inner()).take() {
                handle.abort();
            }
        }
    }
}

/// [`TaskRepository`] 的工作区持久化实现（对应 Java `WorkspaceTaskRepository`）。
///
/// 内存层持有执行体句柄与活记录；每次状态变更同步写工作区任务记录
/// （`workspace.write_task_record`），进程重启后从记录恢复视图（孤儿清扫：
/// 构造时把「非终态且无执行体」的记录标记为失败——对应 Java `sweepOrphanedTasks`）。
/// 远端任务（`TaskRunSpec::Remote`）的 transport 随分布式批次实现，本实现拒绝提交。
pub struct WorkspaceTaskRepository {
    workspace: Arc<WorkspaceManager>,
    parent_agent_id: String,
    tasks: Mutex<HashMap<String, Arc<BackgroundTask>>>,
    callback: Mutex<Option<Arc<dyn CompletionCallback>>>,
}

impl WorkspaceTaskRepository {
    /// 绑定工作区构建；构造时执行孤儿清扫（非终态且无执行体的记录 → 失败）。
    pub fn new(workspace: Arc<WorkspaceManager>, parent_agent_id: impl Into<String>) -> Self {
        let repo = Self {
            workspace,
            parent_agent_id: parent_agent_id.into(),
            tasks: Mutex::new(HashMap::new()),
            callback: Mutex::new(None),
        };
        repo.sweep_orphaned_tasks();
        repo
    }

    /// 孤儿清扫：工作区记录里非终态、且内存中没有执行体的任务标记为失败。
    fn sweep_orphaned_tasks(&self) {
        let active: Vec<String> = self
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        for mut record in self.workspace.fail_orphaned_tasks(&active) {
            record.status = TaskStatus::Failed;
            record.error_message = Some("orphaned: process restarted before completion".into());
            record.last_updated_at = Some(now_millis());
            self.workspace.write_task_record(&record).ok();
        }
    }

    fn key(session_id: &str, task_id: &str) -> String {
        format!("{session_id}/{task_id}")
    }

    fn session_key(rc: &RuntimeContext, session_id: &str) -> String {
        if !session_id.trim().is_empty() {
            return session_id.to_string();
        }
        rc.session_id().unwrap_or("default").to_string()
    }

    /// 结算句柄状态并持久化（终态只结算一次；首次终态触发完成回调）。
    fn settle(&self, task: &BackgroundTask) {
        let newly_terminal = {
            let mut record = task.record.lock().unwrap_or_else(|e| e.into_inner());
            if record.status.is_terminal() {
                return;
            }
            let mut handle_guard = task.handle.lock().unwrap_or_else(|e| e.into_inner());
            let Some(handle) = handle_guard.as_ref() else {
                if record.status == TaskStatus::Pending {
                    record.status = TaskStatus::Running;
                    record.last_updated_at = Some(now_millis());
                    self.workspace.write_task_record(&record).ok();
                }
                return;
            };
            if !handle.is_finished() {
                if record.status == TaskStatus::Pending {
                    record.status = TaskStatus::Running;
                    record.last_updated_at = Some(now_millis());
                    self.workspace.write_task_record(&record).ok();
                }
                return;
            }
            let handle = handle_guard.take().unwrap();
            match handle.now_or_never() {
                Some(Ok(Ok(result))) => {
                    record.result = Some(result);
                    record.status = TaskStatus::Completed;
                }
                Some(Ok(Err(error))) => {
                    record.error_message = Some(error);
                    record.status = TaskStatus::Failed;
                }
                Some(Err(_)) | None => {
                    record.error_message = Some("task aborted".to_string());
                    record.status = TaskStatus::Failed;
                }
            }
            record.last_updated_at = Some(now_millis());
            self.workspace.write_task_record(&record).ok();
            true
        };
        if newly_terminal {
            self.fire_completion_callback(task);
        }
    }

    fn fire_completion_callback(&self, task: &BackgroundTask) {
        let callback = self
            .callback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(callback) = callback else {
            return;
        };
        let (task_id, sub_agent_id, session_id, result, suppressed) = {
            let record = task.record.lock().unwrap_or_else(|e| e.into_inner());
            (
                record.task_id.clone(),
                record.sub_agent_id.clone(),
                record.parent_session_id.clone(),
                record.result.clone(),
                record.status == TaskStatus::Cancelled,
            )
        };
        if suppressed {
            return;
        }
        let rc = RuntimeContext::new().with_session_id(session_id.clone());
        callback.on_completed(&rc, &task_id, &sub_agent_id, &session_id, result);
    }

    /// 从工作区记录重建只读任务视图（无执行体句柄——进程重启后的历史任务）。
    fn synthetic_task(record: TaskRecord) -> Arc<BackgroundTask> {
        Arc::new(BackgroundTask {
            record: Mutex::new(record),
            handle: Mutex::new(None),
            created_at: now_millis(),
            last_checked_at: Mutex::new(None),
        })
    }
}

impl TaskRepository for WorkspaceTaskRepository {
    fn get_task(
        &self,
        rc: &RuntimeContext,
        session_id: &str,
        task_id: &str,
    ) -> Option<Arc<BackgroundTask>> {
        let key = Self::key(&Self::session_key(rc, session_id), task_id);
        if let Some(task) = self
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            self.settle(task);
            return Some(task.clone());
        }
        // 内存没有：从工作区记录恢复（历史任务）
        self.workspace.read_task_record(task_id).map(Self::synthetic_task)
    }

    fn put_task(
        &self,
        rc: &RuntimeContext,
        task_id: &str,
        sub_agent_id: &str,
        session_id: &str,
        spec: TaskRunSpec,
    ) -> Result<Arc<BackgroundTask>, String> {
        let session = Self::session_key(rc, session_id);
        let parent_session = rc.session_id().unwrap_or("default").to_string();
        let record = TaskRecord::pending(
            task_id,
            sub_agent_id,
            &self.parent_agent_id,
            &parent_session,
            &session,
        );
        let handle = match spec {
            TaskRunSpec::Local { execution } => Some(tokio::spawn(execution())),
            TaskRunSpec::Remote { .. } => {
                return Err(
                    "remote task transport is not implemented yet (distributed batch)".into(),
                );
            }
            TaskRunSpec::Adopted { handle } => Some(handle),
        };
        self.workspace.write_task_record(&record)?;
        let task = Arc::new(BackgroundTask {
            record: Mutex::new(record),
            handle: Mutex::new(handle),
            created_at: now_millis(),
            last_checked_at: Mutex::new(None),
        });
        let key = Self::key(&session, task_id);
        self.tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, task.clone());
        Ok(task)
    }

    fn list_tasks(
        &self,
        rc: &RuntimeContext,
        session_id: &str,
        status: Option<TaskStatus>,
    ) -> Vec<Arc<BackgroundTask>> {
        let session = Self::session_key(rc, session_id);
        let prefix = format!("{session}/");
        let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        let mut seen: Vec<String> = Vec::new();
        let mut matched: Vec<Arc<BackgroundTask>> = tasks
            .iter_mut()
            .filter(|(key, _)| key.starts_with(&prefix))
            .filter(|(_, task)| {
                self.settle(task);
                status.is_none_or(|want| task.status() == want)
            })
            .map(|(key, task)| {
                seen.push(key.trim_start_matches(&prefix).to_string());
                task.clone()
            })
            .collect();
        // 工作区记录补充内存里没有的（进程重启后的历史任务）
        for record in self.workspace.list_task_records() {
            if record.parent_session_id != session || seen.contains(&record.task_id) {
                continue;
            }
            if status.is_some_and(|want| record.status != want) {
                continue;
            }
            matched.push(Self::synthetic_task(record));
        }
        matched.sort_by_key(|task| task.created_at);
        matched
    }

    fn cancel_task(&self, rc: &RuntimeContext, session_id: &str, task_id: &str) -> bool {
        let Some(task) = self.get_task(rc, session_id, task_id) else {
            return false;
        };
        let mut record = task.record.lock().unwrap_or_else(|e| e.into_inner());
        if record.status.is_terminal() {
            return false;
        }
        record.cancel_requested = true;
        record.status = TaskStatus::Cancelled;
        record.last_updated_at = Some(now_millis());
        self.workspace.write_task_record(&record).ok();
        drop(record);
        if let Some(handle) = task.handle.lock().unwrap_or_else(|e| e.into_inner()).take() {
            handle.abort();
        }
        true
    }

    fn find_pending_deliveries(
        &self,
        rc: &RuntimeContext,
        session_id: &str,
    ) -> Vec<TaskDelivery> {
        self.list_tasks(rc, session_id, None)
            .into_iter()
            .filter(|task| task.status().is_terminal())
            .filter(|task| {
                task.record
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .delivered_at
                    .is_none()
            })
            .map(|task| {
                let record = task.record.lock().unwrap_or_else(|e| e.into_inner());
                TaskDelivery {
                    task_id: record.task_id.clone(),
                    agent_id: record.sub_agent_id.clone(),
                    status: record.status,
                    result: record.result.clone(),
                    error_message: record.error_message.clone(),
                    completed_at: record.last_updated_at,
                }
            })
            .collect()
    }

    fn mark_delivered(&self, rc: &RuntimeContext, session_id: &str, task_id: &str) {
        if let Some(task) = self.get_task(rc, session_id, task_id) {
            let mut record = task.record.lock().unwrap_or_else(|e| e.into_inner());
            record.delivered_at = Some(now_millis());
            self.workspace.write_task_record(&record).ok();
        }
    }

    fn is_delivered(&self, rc: &RuntimeContext, session_id: &str, task_id: &str) -> bool {
        self.get_task(rc, session_id, task_id)
            .is_some_and(|task| {
                task.record
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .delivered_at
                    .is_some()
            })
    }

    fn set_completion_callback(&self, callback: Arc<dyn CompletionCallback>) {
        *self.callback.lock().unwrap_or_else(|e| e.into_inner()) = Some(callback);
    }

    fn shutdown(&self) {
        let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        for task in tasks.values_mut() {
            if let Some(handle) = task.handle.lock().unwrap_or_else(|e| e.into_inner()).take() {
                handle.abort();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn local_spec(value: &'static str) -> TaskRunSpec {
        TaskRunSpec::Local {
            execution: Arc::new(move || {
                Box::pin(async move {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    Ok(value.to_string())
                }) as futures_util::future::BoxFuture<'static, Result<String, String>>
            }),
        }
    }

    fn failing_spec() -> TaskRunSpec {
        TaskRunSpec::Local {
            execution: Arc::new(|| {
                Box::pin(async { Err("boom".to_string()) })
                    as futures_util::future::BoxFuture<'static, Result<String, String>>
            }),
        }
    }

    #[test]
    fn task_status_terminality() {
        assert!(!TaskStatus::Pending.is_terminal());
        assert!(!TaskStatus::Running.is_terminal());
        assert!(TaskStatus::Completed.is_terminal());
        assert!(TaskStatus::Failed.is_terminal());
        assert!(TaskStatus::Cancelled.is_terminal());
    }

    #[tokio::test]
    async fn local_task_completes_and_delivers_once() {
        let repo = InMemoryTaskRepository::new();
        let rc = RuntimeContext::new().with_session_id("sess-1");
        repo.put_task(&rc, "t1", "coder", "sess-1", local_spec("done!"))
            .unwrap();

        // 轮询到完成
        let mut status = TaskStatus::Pending;
        for _ in 0..100 {
            let task = repo.get_task(&rc, "sess-1", "t1").unwrap();
            status = task.status();
            if status.is_terminal() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(status, TaskStatus::Completed);

        // 待投递 → 标记 → 已投递
        let deliveries = repo.find_pending_deliveries(&rc, "sess-1");
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].task_id, "t1");
        assert_eq!(deliveries[0].result.as_deref(), Some("done!"));
        assert_eq!(deliveries[0].status, TaskStatus::Completed);
        repo.mark_delivered(&rc, "sess-1", "t1");
        assert!(repo.is_delivered(&rc, "sess-1", "t1"));
        assert!(repo.find_pending_deliveries(&rc, "sess-1").is_empty());
    }

    #[tokio::test]
    async fn failing_task_records_error_and_is_delivered() {
        let repo = InMemoryTaskRepository::new();
        let rc = RuntimeContext::new().with_session_id("sess-1");
        repo.put_task(&rc, "t2", "coder", "sess-1", failing_spec())
            .unwrap();
        let mut status = TaskStatus::Pending;
        for _ in 0..100 {
            let task = repo.get_task(&rc, "sess-1", "t2").unwrap();
            status = task.status();
            if status.is_terminal() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(status, TaskStatus::Failed);
        // FAILED 也是终态，同样进入待投递（Java 语义：terminal-but-not-delivered）
        let deliveries = repo.find_pending_deliveries(&rc, "sess-1");
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].status, TaskStatus::Failed);
        assert_eq!(deliveries[0].error_message.as_deref(), Some("boom"));
        assert!(deliveries[0].result.is_none());
    }

    #[tokio::test]
    async fn cancel_running_task() {
        let repo = InMemoryTaskRepository::new();
        let rc = RuntimeContext::new().with_session_id("sess-1");
        let spec = TaskRunSpec::Local {
            execution: Arc::new(|| {
                Box::pin(async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Ok("never".to_string())
                })
                    as futures_util::future::BoxFuture<'static, Result<String, String>>
            }),
        };
        repo.put_task(&rc, "t3", "coder", "sess-1", spec).unwrap();
        assert!(repo.cancel_task(&rc, "sess-1", "t3"));
        let task = repo.get_task(&rc, "sess-1", "t3").unwrap();
        assert_eq!(task.status(), TaskStatus::Cancelled);
        // 终态后重复取消返回 false
        assert!(!repo.cancel_task(&rc, "sess-1", "t3"));
        // 不存在的任务取消返回 false
        assert!(!repo.cancel_task(&rc, "sess-1", "ghost"));
    }

    #[tokio::test]
    async fn adopted_spec_takes_over_running_handle() {
        let repo = InMemoryTaskRepository::new();
        let rc = RuntimeContext::new().with_session_id("sess-1");
        let handle = tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            Ok::<String, String>("adopted".to_string())
        });
        repo.put_task(&rc, "t4", "coder", "sess-1", TaskRunSpec::Adopted { handle })
            .unwrap();
        let mut status = TaskStatus::Pending;
        for _ in 0..100 {
            let task = repo.get_task(&rc, "sess-1", "t4").unwrap();
            status = task.status();
            if status.is_terminal() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(status, TaskStatus::Completed);
        let task = repo.get_task(&rc, "sess-1", "t4").unwrap();
        assert_eq!(task.result().as_deref(), Some("adopted"));
    }

    #[tokio::test]
    async fn completion_callback_fires_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingCallback(AtomicUsize);
        impl CompletionCallback for CountingCallback {
            fn on_completed(
                &self,
                _rc: &RuntimeContext,
                task_id: &str,
                _sub_agent_id: &str,
                _session_id: &str,
                result: Option<String>,
            ) {
                self.0.fetch_add(1, Ordering::SeqCst);
                assert_eq!(task_id, "t5");
                assert_eq!(result.as_deref(), Some("cb"));
            }
        }

        let repo = InMemoryTaskRepository::new();
        repo.set_completion_callback(Arc::new(CountingCallback(AtomicUsize::new(0))));
        let rc = RuntimeContext::new().with_session_id("sess-1");
        repo.put_task(&rc, "t5", "coder", "sess-1", local_spec("cb"))
            .unwrap();
        for _ in 0..100 {
            let task = repo.get_task(&rc, "sess-1", "t5").unwrap();
            if task.status().is_terminal() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // 多次 settle（get_task/list_tasks）不重复触发
        let _ = repo.get_task(&rc, "sess-1", "t5");
        let _ = repo.list_tasks(&rc, "sess-1", None);
        tokio::time::sleep(Duration::from_millis(30)).await;
        // 回调在独立线程触发，给一点时间
        tokio::time::sleep(Duration::from_millis(50)).await;
        let task = repo.get_task(&rc, "sess-1", "t5").unwrap();
        let _ = task.status_text();
    }

    #[tokio::test]
    async fn remote_spec_registers_record_without_execution() {
        let repo = InMemoryTaskRepository::new();
        let rc = RuntimeContext::new().with_session_id("sess-1");
        let task = repo
            .put_task(
                &rc,
                "t6",
                "remote-agent",
                "sess-1",
                TaskRunSpec::Remote {
                    base_url: "http://localhost:8080".to_string(),
                    headers: HashMap::new(),
                    agent_id: "remote-agent".to_string(),
                    input: "hello".to_string(),
                },
            )
            .unwrap();
        let record = task.record.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(record.transport_type, "remote");
        assert_eq!(record.remote_base_url.as_deref(), Some("http://localhost:8080"));
        assert!(task.handle.lock().unwrap_or_else(|e| e.into_inner()).is_none());
    }

    #[test]
    fn background_task_status_text() {
        let record = TaskRecord::pending("t", "a", "p", "s", "s");
        let task = BackgroundTask {
            record: Mutex::new(record),
            handle: Mutex::new(None),
            created_at: 0,
            last_checked_at: Mutex::new(None),
        };
        assert_eq!(task.status_text(), "Pending");
        task.record
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .status = TaskStatus::Failed;
        assert_eq!(task.status_text(), "Failed: Unknown error");
    }
}
