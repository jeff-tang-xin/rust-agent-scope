//! 会话状态与状态存储（对应 Java `io.agentscope.core.state`，19 个类）。
//!
//! 三块内容：
//! 1. **状态数据结构**：会话的单一真相 [`AgentState`]，内含对话缓冲、滚动摘要、reply 标识、
//!    以及四组子上下文（权限 / 工具 / 任务 / 计划模式）；
//! 2. **存储契约**：[`AgentStateStore`]——按 `(userId, sessionId) → key → 值` 存取，
//!    附带列表存取与可选乐观并发（CAS）；
//! 3. **存储实现**：[`InMemoryAgentStateStore`]（进程内）与 [`JsonFileAgentStateStore`]
//!    （每 key 一个 JSON 文件，跨进程可恢复）。
//!
//! ## 形态映射（Java → Rust）
//!
//! | Java | Rust |
//! |------|------|
//! | `interface State {}` 标记接口 | [`State`] trait（要求 serde 可序列化），blanket impl |
//! | `Class<T> type` 运行时反序列化 | 泛型 `T: State` + serde 编译期生成 |
//! | `VersionedState<T>` record | [`VersionedState<T>`] 结构体 |
//! | `synchronized` + `revision` 乐观锁 | 修订号字段 + [`StateError::ConcurrentModification`] |
//!
//! ## 与 Java 的已知差异
//!
//! - Java 的 `JsonFileAgentStateStore` 对列表做**增量追加**（每批元素按其哈希落到独立文件，
//!   见 `ListHashUtil`）；Rust 版整表重写 + 原子 rename，语义等价、写入放大更明显。
//! - Java 的 `LegacyStateLoader`（旧版快照迁移）未复刻：本项目尚无历史数据格式需要迁移。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::message::Msg;
use crate::permission::PermissionContextState;

/// 可持久化的状态值。
///
/// 对应 Java 的空标记接口 `State`；Rust 里把「可序列化」这一隐含前提写进 trait bound，
/// 于是存储层不再需要 `Class<T>` 做运行时类型判断（Java 的 `type.cast` 校验在 Rust 中
/// 由 serde 反序列化失败直接报错）。
pub trait State: Serialize + DeserializeOwned + Send + Sync + 'static {}

impl<T> State for T where T: Serialize + DeserializeOwned + Send + Sync + 'static {}

/// 不支持版本化的后端（或 CAS 冲突时）返回的哨兵版本号。
pub const UNVERSIONED: i64 = -1;

/// 带存储版本的状态读取结果（对应 Java `VersionedState`）。
#[derive(Debug, Clone, PartialEq)]
pub struct VersionedState<T> {
    /// 键不存在时为 `None`。
    pub value: Option<T>,
    /// 存储版本；键不存在时为 `0`，非版本化后端为 [`UNVERSIONED`]。
    pub version: i64,
}

impl<T> VersionedState<T> {
    /// 构造一个「不存在」的结果。
    pub fn absent(version: i64) -> Self {
        Self {
            value: None,
            version,
        }
    }

    /// 是否存在值。
    pub fn is_present(&self) -> bool {
        self.value.is_some()
    }
}

/// 冲突处理策略（对应 Java `ConflictPolicy`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConflictPolicy {
    /// 直接覆盖（last-writer-wins）。
    #[default]
    Overwrite,
    /// 版本不匹配时报错。
    Fail,
    /// 列表追加合并。
    AppendMerge,
}

/// 会话概要（对应 Java `SessionInfo`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub session_id: String,
    /// 该会话持久化数据的总字节数。
    pub size: u64,
    /// 最近修改时间（Unix 毫秒）。
    pub last_modified: i64,
    /// 已持久化的状态条目数。
    pub component_count: usize,
}

/// 状态层错误。
#[derive(Debug)]
pub enum StateError {
    /// 修订号不匹配（对应 Java `ConcurrentSessionModificationException`）。
    ConcurrentModification(String),
    /// sessionId 为空。
    InvalidSessionId,
    /// 序列化 / 反序列化失败。
    Serde(String),
    /// 底层 IO 失败。
    Io(String),
    /// 参数非法。
    Invalid(String),
}

impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StateError::ConcurrentModification(msg) => write!(f, "concurrent modification: {msg}"),
            StateError::InvalidSessionId => write!(f, "sessionId must not be blank"),
            StateError::Serde(msg) => write!(f, "state serde error: {msg}"),
            StateError::Io(msg) => write!(f, "state io error: {msg}"),
            StateError::Invalid(msg) => write!(f, "invalid state: {msg}"),
        }
    }
}

impl std::error::Error for StateError {}

impl From<serde_json::Error> for StateError {
    fn from(e: serde_json::Error) -> Self {
        StateError::Serde(e.to_string())
    }
}

/// 状态层统一 `Result`。
pub type StateResult<T> = Result<T, StateError>;

/// 生成 32 位十六进制标识（对应 Java `UUID.randomUUID().toString().replace("-","")`）。
fn new_hex_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// 校验文本字段（对应 Java `TaskRequirement.requireText`）。
fn require_text(value: &str, label: &str, max_len: usize) -> StateResult<()> {
    if value.trim().is_empty() || value.chars().count() > max_len {
        return Err(StateError::Invalid(format!(
            "{label} must be nonblank and at most {max_len} characters"
        )));
    }
    Ok(())
}

// ---------- 任务（对应 Java Task / TaskRequirement / TaskVerification） ----------

/// 任务状态的 wire 值（对应 Java `Task.State`，小写下划线）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    #[default]
    Pending,
    InProgress,
    Completed,
}

/// 一条用户可见的工作项（对应 Java `Task`）。
///
/// 依赖关系用两个方向表达：`blocks`（等我的下游任务）与 `blocked_by`（卡我的上游任务）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub subject: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub metadata: serde_json::Map<String, Value>,
    /// ISO-8601 创建时间（由 [`Task::new`] 自动填充）。
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub state: TaskState,
    #[serde(default)]
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "blocked_by")]
    pub blocked_by: Vec<String>,
}

impl Task {
    /// 创建一条 `pending` 任务，自动生成 id 与创建时间。
    pub fn new(subject: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            subject: subject.into(),
            description: description.into(),
            metadata: serde_json::Map::new(),
            created_at: crate::event::epoch_to_iso8601(now_millis() / 1000),
            state: TaskState::Pending,
            id: new_hex_id(),
            owner: None,
            blocks: Vec::new(),
            blocked_by: Vec::new(),
        }
    }

    /// 设置状态（builder 风格，便于链式构造）。
    pub fn with_state(mut self, state: TaskState) -> Self {
        self.state = state;
        self
    }

    /// 设置归属者。
    pub fn with_owner(mut self, owner: impl Into<String>) -> Self {
        self.owner = Some(owner.into());
        self
    }

    /// 设置上游依赖。
    pub fn with_blocked_by(mut self, ids: Vec<String>) -> Self {
        self.blocked_by = ids;
        self
    }
}

/// 需求的来源可信度（对应 Java `TaskRequirement.Authority`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskAuthority {
    /// 用户本人确认。
    User,
    /// 调用方（上层系统）确认。
    Caller,
}

/// 需求裁决记录（对应 Java `TaskRequirement.Decision`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskDecision {
    pub authority: TaskAuthority,
    pub source_ref: String,
}

/// 需求类型（对应 Java `TaskRequirement.Kind`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskRequirementKind {
    /// 约束（必须遵守的规则）。
    Constraint,
    /// 验收标准（完成判定依据）。
    AcceptanceCriterion,
}

/// 需求状态（对应 Java `TaskRequirement.Status`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskRequirementStatus {
    /// 模型提出、尚未被可信方确认的候选。
    Candidate,
    Confirmed,
    Rejected,
}

/// 一条任务需求（对应 Java `TaskRequirement`）。
///
/// `proposed_source_ref` 是**不可信声明**，只有 `decision` 里的 `source_ref` 才算可信来源。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRequirement {
    pub id: String,
    pub kind: TaskRequirementKind,
    pub text: String,
    #[serde(rename = "proposedSourceRef")]
    pub proposed_source_ref: String,
    pub status: TaskRequirementStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<TaskDecision>,
}

impl TaskRequirement {
    /// 构造候选需求（对应 Java `TaskContextState.propose` 里 new 出来的对象）。
    pub fn candidate(
        kind: TaskRequirementKind,
        text: impl Into<String>,
        proposed_source_ref: impl Into<String>,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            kind,
            text: text.into(),
            proposed_source_ref: proposed_source_ref.into(),
            status: TaskRequirementStatus::Candidate,
            decision: None,
        }
    }

    /// 校验字段（对应 Java compact constructor）。
    fn validate(&self) -> StateResult<()> {
        require_text(&self.id, "id", 200)?;
        require_text(&self.text, "requirement", 2000)?;
        require_text(&self.proposed_source_ref, "proposed source", 2000)?;
        let decided = self.status != TaskRequirementStatus::Candidate;
        if decided != self.decision.is_some() {
            return Err(StateError::Invalid(
                "only decided requirements must have a decision source".into(),
            ));
        }
        if let Some(decision) = &self.decision {
            require_text(&decision.source_ref, "decision source", 2000)?;
        }
        Ok(())
    }
}

/// 验证结论（对应 Java `TaskVerification.Outcome`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum VerificationOutcome {
    Passed,
    Failed,
    Unknown,
    Error,
}

/// 证据绑定：把结论钉在「哪份任务契约 + 哪个被检对象版本」上（对应 Java `EvidenceBinding`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceBinding {
    pub task_id: String,
    pub contract_version: i64,
    pub subject_version: String,
}

impl EvidenceBinding {
    fn validate(&self) -> StateResult<()> {
        require_text(&self.task_id, "task ID", 200)?;
        if self.contract_version < 0 {
            return Err(StateError::Invalid("negative contract version".into()));
        }
        require_text(&self.subject_version, "subject version", 2000)
    }
}

/// 一次验证记录（对应 Java `TaskVerification`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskVerification {
    pub id: String,
    pub requirement_id: String,
    pub action_id: String,
    pub verifier_id: String,
    pub outcome: VerificationOutcome,
    pub reason: String,
    pub binding: EvidenceBinding,
    pub recorded_at: i64,
}

impl TaskVerification {
    /// 校验字段（对应 Java compact constructor）。
    fn validate(&self) -> StateResult<()> {
        require_text(&self.id, "verification ID", 200)?;
        require_text(&self.requirement_id, "requirement ID", 200)?;
        require_text(&self.action_id, "action ID", 200)?;
        require_text(&self.verifier_id, "verifier ID", 200)?;
        require_text(&self.reason, "reason", 2000)?;
        self.binding.validate()
    }
}

/// 当前 Unix 毫秒时间戳。
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------- 任务上下文（对应 Java TaskContextState） ----------

/// 任务范围：一次应用级任务的标识与目标（对应 Java `TaskContextState.Scope`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskScope {
    #[serde(rename = "taskId")]
    pub task_id: String,
    pub objective: String,
    #[serde(rename = "sourceRef")]
    pub source_ref: String,
}

impl TaskScope {
    /// 构造并校验范围。
    pub fn new(
        task_id: impl Into<String>,
        objective: impl Into<String>,
        source_ref: impl Into<String>,
    ) -> StateResult<Self> {
        let scope = Self {
            task_id: task_id.into(),
            objective: objective.into(),
            source_ref: source_ref.into(),
        };
        scope.validate()?;
        Ok(scope)
    }

    fn validate(&self) -> StateResult<()> {
        require_text(&self.task_id, "task ID", 200)?;
        require_text(&self.objective, "objective", 4000)?;
        require_text(&self.source_ref, "objective source", 2000)
    }
}

/// 会话任务聚合（对应 Java `TaskContextState`）。
///
/// 进度（tasks）与需求（requirements）共享同一个 `revision`：任何一方变更都会推进修订号，
/// 调用方必须带上自己读到的 `expected_revision`，不匹配即
/// [`StateError::ConcurrentModification`]。
///
/// `contract_version` 在「契约」变化时递增（换任务、需求被裁决、被检对象版本重钉），
/// 验证结论用 [`EvidenceBinding`] 钉住「任务 + 契约版本 + 被检对象版本」，
/// 三者任一变化都让旧结论失效。**任务完成度不由验证记录推断**。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TaskContextState {
    #[serde(default)]
    tasks: Vec<Task>,
    #[serde(default)]
    requirements: Vec<TaskRequirement>,
    #[serde(default)]
    revision: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scope: Option<TaskScope>,
    #[serde(default, rename = "contractVersion")]
    contract_version: i64,
    #[serde(default, rename = "subjectVersion", skip_serializing_if = "Option::is_none")]
    subject_version: Option<String>,
    #[serde(default)]
    verifications: Vec<TaskVerification>,
}

impl TaskContextState {
    /// 当前修订号。
    pub fn revision(&self) -> i64 {
        self.revision
    }

    /// 契约版本。
    pub fn contract_version(&self) -> i64 {
        self.contract_version
    }

    /// 任务范围。
    pub fn scope(&self) -> Option<&TaskScope> {
        self.scope.as_ref()
    }

    /// 被检对象版本。
    pub fn subject_version(&self) -> Option<&str> {
        self.subject_version.as_deref()
    }

    /// 任务列表快照。
    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    /// 需求列表快照。
    pub fn requirements(&self) -> &[TaskRequirement] {
        &self.requirements
    }

    /// 验证记录快照。
    pub fn verifications(&self) -> &[TaskVerification] {
        &self.verifications
    }

    /// 当前证据绑定（缺范围或缺被检对象版本时为 `None`）。
    pub fn evidence_binding(&self) -> Option<EvidenceBinding> {
        match (&self.scope, &self.subject_version) {
            (Some(scope), Some(subject)) => Some(EvidenceBinding {
                task_id: scope.task_id.clone(),
                contract_version: self.contract_version,
                subject_version: subject.clone(),
            }),
            _ => None,
        }
    }

    /// 显式切换任务：**同时清空**旧任务的进度与需求，并推进契约版本。
    ///
    /// 目标变化时必须换一个新 taskId（同 id 换目标会报错），避免新旧证据混用。
    pub fn begin_task(&mut self, next: TaskScope, expected_revision: i64) -> StateResult<()> {
        self.check_revision(expected_revision)?;
        next.validate()?;
        if self.scope.as_ref() == Some(&next) {
            return Ok(());
        }
        if let Some(current) = &self.scope {
            if current.task_id == next.task_id {
                return Err(StateError::Invalid(
                    "use a new task identity when changing the objective".into(),
                ));
            }
        }
        self.scope = Some(next);
        self.tasks.clear();
        self.requirements.clear();
        self.verifications.clear();
        self.subject_version = None;
        self.contract_version += 1;
        self.revision += 1;
        Ok(())
    }

    /// 原子替换进度列表，**不动需求**。
    pub fn replace_tasks(
        &mut self,
        replacement: Vec<Task>,
        expected_revision: i64,
    ) -> StateResult<()> {
        self.check_revision(expected_revision)?;
        self.tasks = replacement;
        self.revision += 1;
        Ok(())
    }

    /// 提出一条需求（去重：同类型 + 同文本 + 同来源已存在则原样返回）。
    ///
    /// 此处传入的来源是**不可信声明**（`proposed_source_ref`），需 [`Self::decide`] 才可信。
    pub fn propose(
        &mut self,
        kind: TaskRequirementKind,
        text: impl Into<String>,
        proposed_source_ref: impl Into<String>,
    ) -> StateResult<TaskRequirement> {
        let text = text.into();
        let proposed_source_ref = proposed_source_ref.into();
        let candidate = TaskRequirement::candidate(kind, text.clone(), proposed_source_ref.clone());
        candidate.validate()?;
        if let Some(existing) = self.requirements.iter().find(|existing| {
            existing.kind == kind
                && existing.text == text
                && existing.proposed_source_ref == proposed_source_ref
        }) {
            return Ok(existing.clone());
        }
        if self.requirements.len() >= 64 {
            return Err(StateError::Invalid("task requirement limit reached".into()));
        }
        self.requirements.push(candidate.clone());
        self.revision += 1;
        Ok(candidate)
    }

    /// 可信裁决入口（应用侧 API，不注册为模型可调用工具）。
    pub fn decide(
        &mut self,
        id: &str,
        status: TaskRequirementStatus,
        decision: TaskDecision,
        expected_revision: i64,
    ) -> StateResult<()> {
        self.check_revision(expected_revision)?;
        if status == TaskRequirementStatus::Candidate {
            return Err(StateError::Invalid(
                "decision must confirm or reject".into(),
            ));
        }
        for index in 0..self.requirements.len() {
            let existing = &self.requirements[index];
            if existing.id != id {
                continue;
            }
            if existing.status == status && existing.decision.as_ref() == Some(&decision) {
                return Ok(());
            }
            let mut updated = existing.clone();
            updated.status = status;
            updated.decision = Some(decision);
            updated.validate()?;
            self.requirements[index] = updated;
            self.revision += 1;
            self.contract_version += 1;
            return Ok(());
        }
        Err(StateError::Invalid(format!("unknown requirement: {id}")))
    }

    /// 深拷贝快照。
    pub fn snapshot(&self) -> Self {
        self.clone()
    }

    /// 更新被检对象版本（工作区/产物变化后、执行前调用）。
    pub fn set_subject_version(
        &mut self,
        version: impl Into<String>,
        expected_revision: i64,
    ) -> StateResult<()> {
        self.check_revision(expected_revision)?;
        let version = version.into();
        require_text(&version, "subject version", 2000)?;
        if self.scope.is_none() {
            return Err(StateError::Invalid(
                "task scope required for evidence".into(),
            ));
        }
        if self.subject_version.as_deref() == Some(version.as_str()) {
            return Ok(());
        }
        self.subject_version = Some(version);
        self.contract_version += 1;
        self.revision += 1;
        Ok(())
    }

    /// 让证据失效（相关变更开始时立刻调用，落定后再重新钉版本）。
    pub fn invalidate_subject(&mut self, expected_revision: i64) -> StateResult<()> {
        self.check_revision(expected_revision)?;
        self.subject_version = None;
        self.contract_version += 1;
        self.revision += 1;
        Ok(())
    }

    /// 记录一条验证结论（须在验证结果已持久化后调用）。
    ///
    /// 只登记结论，**不**顺手把任务标记完成。绑定不匹配 = 契约或被检对象已变，视为并发冲突。
    pub fn record_verification(
        &mut self,
        report: TaskVerification,
        expected_revision: i64,
    ) -> StateResult<()> {
        self.check_revision(expected_revision)?;
        report.validate()?;
        if Some(&report.binding) != self.evidence_binding().as_ref() {
            return Err(StateError::ConcurrentModification(
                "verification subject or contract changed".into(),
            ));
        }
        let confirmed = self.requirements.iter().any(|requirement| {
            requirement.id == report.requirement_id
                && requirement.kind == TaskRequirementKind::AcceptanceCriterion
                && requirement.status == TaskRequirementStatus::Confirmed
        });
        if !confirmed {
            return Err(StateError::Invalid(
                "confirmed acceptance criterion required".into(),
            ));
        }
        self.verifications
            .retain(|previous| previous.requirement_id != report.requirement_id);
        self.verifications.push(report);
        self.revision += 1;
        Ok(())
    }

    /// 序列化（对应 Java `toJson`）。
    pub fn to_json(&self) -> StateResult<String> {
        serde_json::to_string_pretty(self).map_err(Into::into)
    }

    /// 反序列化（对应 Java `fromJsonString`）。
    pub fn from_json_str(json: &str) -> StateResult<Self> {
        serde_json::from_str(json).map_err(Into::into)
    }

    fn check_revision(&self, expected_revision: i64) -> StateResult<()> {
        if self.revision != expected_revision {
            return Err(StateError::ConcurrentModification(format!(
                "task revision changed: expected {expected_revision}, actual {}",
                self.revision
            )));
        }
        Ok(())
    }
}

// ---------- 工具上下文（对应 Java ToolContextState） ----------

/// 文件读缓存条目（对应 Java `ReadCacheEntry`）。
///
/// `bytes` 是「千字节近似值」（对文件中各行的 UTF-8 字节数求和后除以 1000），
/// 沿用 Java 的口径，便于与 Java 版持久化数据互换。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReadCacheEntry {
    pub lines: Vec<String>,
    #[serde(rename = "updated_at")]
    pub updated_at: f64,
    pub bytes: f64,
    #[serde(rename = "file_path")]
    pub file_path: String,
}

/// 已派生 subagent 的可序列化登记项（对应 Java `ToolContextState.SpawnEntry`）。
///
/// 目的是会话恢复或跨副本路由后 `agent_send` 能懒重建活的 Agent 实例。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnEntry {
    pub key: String,
    #[serde(rename = "agent_id")]
    pub agent_id: String,
    #[serde(rename = "session_id")]
    pub session_id: String,
    pub label: String,
    pub depth: i32,
}

/// 工具上下文（对应 Java `ToolContextState`）。
///
/// 只有配置字段参与序列化：`activated_groups`（会话级工具激活集）与 `spawn_registry`；
/// 文件读缓存是运行态，反序列化后为空。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolContextState {
    #[serde(default = "default_max_cache_files", rename = "max_cache_files")]
    max_cache_files: usize,
    #[serde(default = "default_max_cache_bytes", rename = "max_cache_bytes")]
    max_cache_bytes: f64,
    #[serde(default, rename = "activated_groups")]
    activated_groups: Vec<String>,
    #[serde(default, rename = "spawn_registry")]
    spawn_registry: std::collections::BTreeMap<String, SpawnEntry>,
    /// 运行态 LRU（旧 → 新），不序列化。
    #[serde(skip)]
    read_file_cache: Vec<ReadCacheEntry>,
}

fn default_max_cache_files() -> usize {
    100
}

fn default_max_cache_bytes() -> f64 {
    25_000.0
}

impl Default for ToolContextState {
    fn default() -> Self {
        Self {
            max_cache_files: default_max_cache_files(),
            max_cache_bytes: default_max_cache_bytes(),
            activated_groups: Vec::new(),
            spawn_registry: std::collections::BTreeMap::new(),
            read_file_cache: Vec::new(),
        }
    }
}

impl ToolContextState {
    /// 指定缓存上限构造（对应 Java builder 校验：文件数 > 1，字节 > 10000）。
    pub fn with_limits(
        max_cache_files: usize,
        max_cache_bytes: f64,
    ) -> StateResult<Self> {
        if max_cache_files <= 1 {
            return Err(StateError::Invalid("maxCacheFiles must be > 1".into()));
        }
        if max_cache_bytes <= 10_000.0 {
            return Err(StateError::Invalid("maxCacheBytes must be > 10000".into()));
        }
        Ok(Self {
            max_cache_files,
            max_cache_bytes,
            ..Default::default()
        })
    }

    pub fn max_cache_files(&self) -> usize {
        self.max_cache_files
    }

    pub fn max_cache_bytes(&self) -> f64 {
        self.max_cache_bytes
    }

    /// 缓存内容快照（旧 → 新）。
    pub fn read_file_cache(&self) -> &[ReadCacheEntry] {
        &self.read_file_cache
    }

    /// 会话级激活工具组。
    pub fn activated_groups(&self) -> &[String] {
        &self.activated_groups
    }

    /// 整体替换激活组（保持顺序、去重、忽略空值）。
    pub fn set_activated_groups(&mut self, groups: Option<&[String]>) {
        self.activated_groups.clear();
        self.add_activated_groups(groups);
    }

    /// 追加激活组（原子读改写：并发调用不会互相覆盖）。
    pub fn add_activated_groups(&mut self, groups: Option<&[String]>) {
        let Some(groups) = groups else { return };
        for group in groups {
            if !group.is_empty() && !self.activated_groups.contains(group) {
                self.activated_groups.push(group.clone());
            }
        }
    }

    /// 移除激活组。
    pub fn remove_activated_groups(&mut self, groups: Option<&[String]>) {
        let Some(groups) = groups else { return };
        self.activated_groups.retain(|group| !groups.contains(group));
    }

    /// 单次读改写：先移除 `to_remove` 再加入 `to_add`。
    ///
    /// Java 这样做是为了让「重置装备」与「增量激活」并发时不会互相吞掉对方的写。
    pub fn replace_activated_groups(&mut self, to_remove: Option<&[String]>, to_add: Option<&[String]>) {
        if let Some(to_remove) = to_remove {
            self.activated_groups.retain(|group| !to_remove.contains(group));
        }
        self.add_activated_groups(to_add);
    }

    /// subagent 登记表快照。
    pub fn spawn_registry(&self) -> &std::collections::BTreeMap<String, SpawnEntry> {
        &self.spawn_registry
    }

    /// 登记一个派生的 subagent。
    pub fn put_spawn_entry(&mut self, entry: SpawnEntry) {
        if !entry.key.is_empty() {
            self.spawn_registry.insert(entry.key.clone(), entry);
        }
    }

    /// 移除登记项。
    pub fn remove_spawn_entry(&mut self, key: &str) {
        self.spawn_registry.remove(key);
    }

    /// 查询文件读缓存；文件 mtime 变化或文件已消失则淘汰并返回 `None`。
    pub fn get_cache(&mut self, file_path: &str) -> Option<ReadCacheEntry> {
        let index = self
            .read_file_cache
            .iter()
            .position(|entry| entry.file_path == file_path)?;
        match current_mtime(file_path) {
            Some(current) if current == self.read_file_cache[index].updated_at => {
                Some(self.read_file_cache[index].clone())
            }
            _ => {
                self.read_file_cache.remove(index);
                None
            }
        }
    }

    /// 插入文件读缓存，按文件数与总字节双上限淘汰最旧条目。
    ///
    /// 文件 mtime 不可读时静默跳过（对应 Java `cacheFile`）。
    pub fn cache_file(&mut self, file_path: &str, lines: Vec<String>) {
        let Some(updated_at) = current_mtime(file_path) else {
            return;
        };
        let new_bytes = approximate_kilobytes(&lines);
        self.read_file_cache
            .retain(|entry| entry.file_path != file_path);
        while self.read_file_cache.len() >= self.max_cache_files {
            self.read_file_cache.remove(0);
        }
        let mut current_bytes: f64 = self.read_file_cache.iter().map(|e| e.bytes).sum();
        while !self.read_file_cache.is_empty()
            && current_bytes + new_bytes > self.max_cache_bytes
        {
            current_bytes -= self.read_file_cache.remove(0).bytes;
        }
        self.read_file_cache.push(ReadCacheEntry {
            lines,
            updated_at,
            bytes: new_bytes,
            file_path: file_path.to_string(),
        });
    }

    /// 序列化（对应 Java `toJson`）。
    pub fn to_json(&self) -> StateResult<String> {
        serde_json::to_string_pretty(self).map_err(Into::into)
    }
}

/// 文件最后修改时间（对齐 Java：`FileTime.toMillis() / 1000.0`）；不可读时返回 `None`。
fn current_mtime(file_path: &str) -> Option<f64> {
    let metadata = std::fs::metadata(file_path).ok()?;
    let modified = metadata.modified().ok()?;
    let since_epoch = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(since_epoch.as_millis() as f64 / 1000.0)
}

/// 千字节近似值（对齐 Java `approximateKilobytes`：UTF-8 字节和 / 1000）。
fn approximate_kilobytes(lines: &[String]) -> f64 {
    let bytes: usize = lines.iter().map(|line| line.len()).sum();
    bytes as f64 / 1000.0
}

// ---------- 计划模式（对应 Java PlanModeContextState） ----------

/// 计划模式状态（对应 Java `PlanModeContextState`）。
///
/// 计划模式是「先设计、不改动」的只读阶段：`plan_active` 为真时
/// `PlanModeMiddleware` 拒绝所有写操作类工具调用。它必须随 [`AgentState`] 持久化，
/// 因为 agent 是可恢复、可跨节点接管的。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PlanModeContextState {
    #[serde(default, rename = "plan_active")]
    pub plan_active: bool,
    /// 工作区相对路径的计划文件。
    #[serde(default, rename = "current_plan_file", skip_serializing_if = "Option::is_none")]
    pub current_plan_file: Option<String>,
}

// ---------- 会话总状态（对应 Java AgentState） ----------

/// 一个会话的单一真相（对应 Java `AgentState`）。
///
/// 承载：对话缓冲 `context`、滚动摘要 `summary`、当前回复标识 `reply_id`、
/// 迭代计数 `cur_iter`、中断标记 `shutdown_interrupted`，以及四组子上下文。
///
/// Java 用 builder 保证「字段缺省即填默认值」；Rust 用 [`Default`] + `serde` 的
/// `#[serde(default)]` 达到同样效果：旧版本 JSON 缺字段也能读。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentState {
    #[serde(default = "new_hex_id", rename = "session_id")]
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "user_id")]
    pub user_id: Option<String>,
    /// 滚动摘要（长对话压缩后替代被裁掉的历史）。
    #[serde(default)]
    pub summary: String,
    /// 对话缓冲（全部消息，含工具调用与结果）。
    #[serde(default)]
    pub context: Vec<Msg>,
    #[serde(default = "new_hex_id", rename = "reply_id")]
    pub reply_id: String,
    #[serde(default, rename = "cur_iter")]
    pub cur_iter: i32,
    #[serde(default, rename = "shutdown_interrupted")]
    pub shutdown_interrupted: bool,
    #[serde(default, rename = "permission_context")]
    pub permission_context: PermissionContextState,
    #[serde(default, rename = "tool_context")]
    pub tool_context: ToolContextState,
    #[serde(default, rename = "tasks_context")]
    pub tasks_context: TaskContextState,
    #[serde(default, rename = "plan_mode_context")]
    pub plan_mode_context: PlanModeContextState,
}

impl AgentState {
    /// 新建会话状态（自动生成 session_id 与 reply_id）。
    pub fn new() -> Self {
        Self {
            session_id: new_hex_id(),
            user_id: None,
            summary: String::new(),
            context: Vec::new(),
            reply_id: new_hex_id(),
            cur_iter: 0,
            shutdown_interrupted: false,
            permission_context: PermissionContextState::default(),
            tool_context: ToolContextState::default(),
            tasks_context: TaskContextState::default(),
            plan_mode_context: PlanModeContextState::default(),
        }
    }

    /// 指定会话标识新建（对应 Java builder 的 `sessionId`）。
    pub fn with_session_id(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            ..Self::new()
        }
    }

    /// 绑定用户标识（`None` = 匿名会话）。
    pub fn with_user_id(mut self, user_id: Option<String>) -> Self {
        self.user_id = user_id;
        self
    }

    /// 追加一条消息到对话缓冲。
    pub fn push_context(&mut self, msg: Msg) {
        self.context.push(msg);
    }

    /// 换一个新的回复标识（每次 agent 调用开始时刷新）。
    pub fn rotate_reply_id(&mut self) {
        self.reply_id = new_hex_id();
    }

    /// 序列化（对应 Java `toJson`，pretty 输出）。
    pub fn to_json(&self) -> StateResult<String> {
        serde_json::to_string_pretty(self).map_err(Into::into)
    }

    /// 反序列化（对应 Java `fromJsonString`）。
    pub fn from_json_str(json: &str) -> StateResult<Self> {
        serde_json::from_str(json).map_err(Into::into)
    }
}

impl Default for AgentState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------- 存储契约（对应 Java AgentStateStore） ----------

/// 匿名用户桶名：`userId` 为空时归到此桶，避免与真的叫 `""` 的用户冲突。
pub const ANON_USER: &str = "__anon__";

/// 状态存储（对应 Java `AgentStateStore`）。
///
/// 寻址单位是 `(userId, sessionId) → key → 值`。`userId` 为 `None` 表示匿名调用者
/// （CLI、测试），所有匿名会话共享一个命名空间；`sessionId` 必须非空。
/// 实现方决定如何把二元组落成物理键（文件路径 / Redis 前缀 / SQL 列），
/// **调用方不要自己拼接**。
///
/// ## 为什么方法收 `serde_json::Value` 而不是泛型
///
/// Java 接口靠 `Class<T>` 在运行时反序列化，是反射；Rust 里若把 `T: State` 写进方法签名，
/// trait 就带泛型方法、失去 object safety，无法 `Arc<dyn AgentStateStore>` 运行时换后端。
/// 所以底层收 `Value`（object-safe），类型安全的便利方法放进
/// [`AgentStateStoreExt`]——编译期由 serde 完成，不碰 `Any`/downcast。
pub trait AgentStateStore: Send + Sync {
    /// 写入单个状态值（整体替换）。
    fn save_raw(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: Value,
    ) -> StateResult<()>;

    /// 写入列表状态值（调用方**总是**传完整列表，存储策略由实现决定）。
    fn save_list_raw(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        values: Vec<Value>,
    ) -> StateResult<()>;

    /// 读取单个状态值。
    fn get_raw(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> StateResult<Option<Value>>;

    /// 读取列表状态值（不存在时返回空列表）。
    fn get_list_raw(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> StateResult<Vec<Value>>;

    /// 是否支持乐观并发（CAS）。默认 `false`：`save_if_version_raw` 退化为无条件写。
    fn supports_versioning(&self) -> bool {
        false
    }

    /// 带版本的读取。默认实现把 `get_raw` 包一层并报 [`UNVERSIONED`]。
    fn get_versioned_raw(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> StateResult<VersionedState<Value>> {
        Ok(VersionedState {
            value: self.get_raw(user_id, session_id, key)?,
            version: UNVERSIONED,
        })
    }

    /// 比较并交换写入。
    ///
    /// - `expected == 0`：不存在才创建（键存在但版本仍为 0 时 CAS 视为满足，写入版本 1）；
    /// - `expected == UNVERSIONED`：无条件覆盖（等同 `save_raw`）；
    /// - 其余：仅当存储版本等于 `expected` 时写入。
    ///
    /// 成功返回新版本；冲突返回 [`UNVERSIONED`] 且不写入。
    fn save_if_version_raw(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: Value,
        expected_version: i64,
    ) -> StateResult<i64> {
        let _ = expected_version;
        self.save_raw(user_id, session_id, key, value)?;
        Ok(UNVERSIONED)
    }

    /// 会话是否存在（有任何持久化状态）。
    fn exists(&self, user_id: Option<&str>, session_id: &str) -> StateResult<bool>;

    /// 删除整个会话。
    fn delete_session(&self, user_id: Option<&str>, session_id: &str) -> StateResult<()>;

    /// 删除会话内的单个键。默认空实现（不支持按键删除的后端不必覆写）。
    fn delete_key(
        &self,
        _user_id: Option<&str>,
        _session_id: &str,
        _key: &str,
    ) -> StateResult<()> {
        Ok(())
    }

    /// 列出某用户命名空间下的会话标识（`None` 列匿名会话）。
    fn list_session_ids(&self, user_id: Option<&str>) -> StateResult<Vec<String>>;

    /// 释放资源。默认空实现。
    fn close(&self) {}
}

/// 类型安全便利方法（对应 Java 接口里 `Class<T> type` 的那部分能力）。
///
/// 对任意 [`AgentStateStore`] 自动可用，方法名与 Java 接口保持一致。
pub trait AgentStateStoreExt: AgentStateStore {
    /// 写入单个状态值。
    fn save<T: State>(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: &T,
    ) -> StateResult<()> {
        let raw = serde_json::to_value(value)?;
        self.save_raw(user_id, session_id, key, raw)
    }

    /// 写入列表状态值。
    fn save_list<T: State>(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        values: &[T],
    ) -> StateResult<()> {
        let raw = values
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()?;
        self.save_list_raw(user_id, session_id, key, raw)
    }

    /// 读取单个状态值（类型不匹配即报 serde 错误）。
    fn get<T: State>(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> StateResult<Option<T>> {
        match self.get_raw(user_id, session_id, key)? {
            Some(value) => Ok(Some(serde_json::from_value(value)?)),
            None => Ok(None),
        }
    }

    /// 读取列表状态值。
    fn get_list<T: State>(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> StateResult<Vec<T>> {
        self.get_list_raw(user_id, session_id, key)?
            .into_iter()
            .map(|value| serde_json::from_value(value).map_err(Into::into))
            .collect()
    }

    /// 带版本的读取。
    fn get_versioned<T: State>(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> StateResult<VersionedState<T>> {
        let raw = self.get_versioned_raw(user_id, session_id, key)?;
        let value = match raw.value {
            Some(value) => Some(serde_json::from_value(value)?),
            None => None,
        };
        Ok(VersionedState {
            value,
            version: raw.version,
        })
    }

    /// 比较并交换写入。
    fn save_if_version<T: State>(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: &T,
        expected_version: i64,
    ) -> StateResult<i64> {
        let raw = serde_json::to_value(value)?;
        self.save_if_version_raw(user_id, session_id, key, raw, expected_version)
    }
}

impl<S: AgentStateStore + ?Sized> AgentStateStoreExt for S {}

/// 校验并规范化会话标识。
fn require_session_id(session_id: &str) -> StateResult<&str> {
    if session_id.trim().is_empty() {
        return Err(StateError::InvalidSessionId);
    }
    Ok(session_id)
}

/// 规范化用户命名空间（`None`/空白 → 匿名桶）。
fn normalize_user(user_id: Option<&str>) -> String {
    match user_id {
        Some(user) if !user.trim().is_empty() => user.to_string(),
        _ => ANON_USER.to_string(),
    }
}

/// 内存存储（对应 Java `InMemoryAgentStateStore`）。
///
/// 适合单进程、无需跨重启持久化的场景；进程退出即丢失，内存随会话数增长。
#[derive(Default)]
pub struct InMemoryAgentStateStore {
    /// users → (sessionId → SessionData)
    users: RwLock<HashMap<String, HashMap<String, SessionData>>>,
}

#[derive(Default, Clone)]
struct SessionData {
    singles: HashMap<String, VersionedValue>,
    lists: HashMap<String, Vec<Value>>,
}

#[derive(Clone)]
struct VersionedValue {
    value: Value,
    version: i64,
}

impl InMemoryAgentStateStore {
    /// 新建空存储。
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前存活的 `(userId, sessionId)` 槽位总数。
    pub fn session_count(&self) -> usize {
        self.users
            .read()
            .unwrap()
            .values()
            .map(HashMap::len)
            .sum()
    }

    /// 清空全部数据（测试用）。
    pub fn clear_all(&self) {
        self.users.write().unwrap().clear();
    }
}

impl AgentStateStore for InMemoryAgentStateStore {
    fn save_raw(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: Value,
    ) -> StateResult<()> {
        let session = require_session_id(session_id)?.to_string();
        let user = normalize_user(user_id);
        let mut guard = self.users.write().unwrap();
        let slot = guard
            .entry(user)
            .or_default()
            .entry(session)
            .or_default();
        let next = match slot.singles.get(key) {
            Some(prev) => prev.version + 1,
            None => 1,
        };
        slot.singles
            .insert(key.to_string(), VersionedValue { value, version: next });
        Ok(())
    }

    fn save_list_raw(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        values: Vec<Value>,
    ) -> StateResult<()> {
        let session = require_session_id(session_id)?.to_string();
        let user = normalize_user(user_id);
        let mut guard = self.users.write().unwrap();
        guard
            .entry(user)
            .or_default()
            .entry(session)
            .or_default()
            .lists
            .insert(key.to_string(), values);
        Ok(())
    }

    fn get_raw(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> StateResult<Option<Value>> {
        let session = require_session_id(session_id)?.to_string();
        let user = normalize_user(user_id);
        let guard = self.users.read().unwrap();
        Ok(guard
            .get(&user)
            .and_then(|bucket| bucket.get(&session))
            .and_then(|slot| slot.singles.get(key))
            .map(|entry| entry.value.clone()))
    }

    fn get_list_raw(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> StateResult<Vec<Value>> {
        let session = require_session_id(session_id)?.to_string();
        let user = normalize_user(user_id);
        let guard = self.users.read().unwrap();
        Ok(guard
            .get(&user)
            .and_then(|bucket| bucket.get(&session))
            .and_then(|slot| slot.lists.get(key))
            .cloned()
            .unwrap_or_default())
    }

    fn supports_versioning(&self) -> bool {
        true
    }

    fn get_versioned_raw(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> StateResult<VersionedState<Value>> {
        let session = require_session_id(session_id)?.to_string();
        let user = normalize_user(user_id);
        let guard = self.users.read().unwrap();
        let entry = guard
            .get(&user)
            .and_then(|bucket| bucket.get(&session))
            .and_then(|slot| slot.singles.get(key));
        Ok(match entry {
            Some(entry) => VersionedState {
                value: Some(entry.value.clone()),
                version: entry.version,
            },
            None => VersionedState::absent(0),
        })
    }

    fn save_if_version_raw(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: Value,
        expected_version: i64,
    ) -> StateResult<i64> {
        let session = require_session_id(session_id)?.to_string();
        let user = normalize_user(user_id);
        let mut guard = self.users.write().unwrap();
        let slot = guard
            .entry(user)
            .or_default()
            .entry(session)
            .or_default();
        // 读版本与写入在同一把写锁内完成，避免和并发写者交错。
        let current = slot
            .singles
            .get(key)
            .map(|entry| entry.version)
            .unwrap_or(0);
        if expected_version == UNVERSIONED {
            let next = current + 1;
            slot.singles
                .insert(key.to_string(), VersionedValue { value, version: next });
            return Ok(next);
        }
        if current != expected_version {
            return Ok(UNVERSIONED);
        }
        let next = expected_version + 1;
        slot.singles
            .insert(key.to_string(), VersionedValue { value, version: next });
        Ok(next)
    }

    fn exists(&self, user_id: Option<&str>, session_id: &str) -> StateResult<bool> {
        let session = require_session_id(session_id)?.to_string();
        let user = normalize_user(user_id);
        let guard = self.users.read().unwrap();
        Ok(guard
            .get(&user)
            .map(|bucket| bucket.contains_key(&session))
            .unwrap_or(false))
    }

    fn delete_session(&self, user_id: Option<&str>, session_id: &str) -> StateResult<()> {
        let session = require_session_id(session_id)?.to_string();
        let user = normalize_user(user_id);
        let mut guard = self.users.write().unwrap();
        if let Some(bucket) = guard.get_mut(&user) {
            bucket.remove(&session);
        }
        Ok(())
    }

    fn delete_key(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> StateResult<()> {
        let session = require_session_id(session_id)?.to_string();
        let user = normalize_user(user_id);
        let mut guard = self.users.write().unwrap();
        if let Some(slot) = guard
            .get_mut(&user)
            .and_then(|bucket| bucket.get_mut(&session))
        {
            slot.singles.remove(key);
            slot.lists.remove(key);
        }
        Ok(())
    }

    fn list_session_ids(&self, user_id: Option<&str>) -> StateResult<Vec<String>> {
        let user = normalize_user(user_id);
        let guard = self.users.read().unwrap();
        let mut ids: Vec<String> = guard
            .get(&user)
            .map(|bucket| bucket.keys().cloned().collect())
            .unwrap_or_default();
        ids.sort();
        Ok(ids)
    }
}

// ---------- 文件存储（对应 Java JsonFileAgentStateStore） ----------

/// 基于文件系统的状态存储（对应 Java `JsonFileAgentStateStore`）。
///
/// 布局 `<root>/<user>/<session>/`：
/// - 单值键：`<key>.json`（pretty JSON）
/// - 列表键：`<key>.jsonl`（每行一条 JSON）
///
/// 目录名走 `safe_segment`：只允许 `[A-Za-z0-9_.-]`，其余字符用 `~` 前缀 + URL-safe
/// Base64（无 padding）编码，读取时反向解码。正常命名的用户/会话目录与 Java 版布局一致。
///
/// ## 与 Java 的差异
///
/// - **列表写入**：Java 用 `.hash` 文件判断能否增量追加；Rust 版读回既有行、判断「新列表以旧列表
///   为前缀」后决定追加还是整表重写，省掉一个旁路文件，语义等价。
/// - **版本化**：Java 版不覆写 `supportsVersioning()`（即不支持 CAS），Rust 版保持一致。
/// - 写入走 `临时文件 + rename`，避免半截文件。
pub struct JsonFileAgentStateStore {
    root: PathBuf,
}

impl JsonFileAgentStateStore {
    /// 以默认根目录 `~/.agentscope/state` 构造（对应 Java 无参构造）。
    pub fn with_default_root() -> StateResult<Self> {
        let home = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .map_err(|_| StateError::Io("cannot resolve home directory".into()))?;
        Self::new(Path::new(&home).join(".agentscope").join("state"))
    }

    /// 以指定根目录构造（自动创建目录）。
    pub fn new(root: impl Into<PathBuf>) -> StateResult<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)
            .map_err(|e| StateError::Io(format!("failed to create root {root:?}: {e}")))?;
        Ok(Self { root })
    }

    /// 根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 会话目录。
    fn session_dir(&self, user_id: Option<&str>, session_id: &str) -> StateResult<PathBuf> {
        let session = require_session_id(session_id)?;
        Ok(self
            .root
            .join(safe_segment(&normalize_user(user_id)))
            .join(safe_segment(session)))
    }

    fn state_path(&self, user_id: Option<&str>, session_id: &str, key: &str) -> StateResult<PathBuf> {
        Ok(self.session_dir(user_id, session_id)?.join(format!("{key}.json")))
    }

    fn list_path(&self, user_id: Option<&str>, session_id: &str, key: &str) -> StateResult<PathBuf> {
        Ok(self.session_dir(user_id, session_id)?.join(format!("{key}.jsonl")))
    }
}

impl AgentStateStore for JsonFileAgentStateStore {
    fn save_raw(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: Value,
    ) -> StateResult<()> {
        let path = self.state_path(user_id, session_id, key)?;
        let json = serde_json::to_string_pretty(&value)?;
        atomic_write(&path, &json)
    }

    fn save_list_raw(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        values: Vec<Value>,
    ) -> StateResult<()> {
        let path = self.list_path(user_id, session_id, key)?;
        let existing = read_jsonl(&path)?;
        let can_append = values.len() >= existing.len()
            && values[..existing.len()]
                .iter()
                .zip(existing.iter())
                .all(|(new, old)| new == old);
        if can_append && !existing.is_empty() {
            if values.len() == existing.len() {
                return Ok(()); // 无变化，跳过写入
            }
            let mut appended = std::fs::read_to_string(&path)
                .map_err(|e| StateError::Io(format!("failed to read {path:?}: {e}")))?;
            if !appended.ends_with('\n') && !appended.is_empty() {
                appended.push('\n');
            }
            for value in &values[existing.len()..] {
                appended.push_str(&serde_json::to_string(value)?);
                appended.push('\n');
            }
            return atomic_write(&path, &appended);
        }
        let mut content = String::new();
        for value in &values {
            content.push_str(&serde_json::to_string(value)?);
            content.push('\n');
        }
        atomic_write(&path, &content)
    }

    fn get_raw(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> StateResult<Option<Value>> {
        let path = self.state_path(user_id, session_id, key)?;
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| StateError::Io(format!("failed to read {path:?}: {e}")))?;
        Ok(Some(serde_json::from_str(&text)?))
    }

    fn get_list_raw(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> StateResult<Vec<Value>> {
        read_jsonl(&self.list_path(user_id, session_id, key)?)
    }

    fn exists(&self, user_id: Option<&str>, session_id: &str) -> StateResult<bool> {
        Ok(self.session_dir(user_id, session_id)?.is_dir())
    }

    fn delete_session(&self, user_id: Option<&str>, session_id: &str) -> StateResult<()> {
        let dir = self.session_dir(user_id, session_id)?;
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .map_err(|e| StateError::Io(format!("failed to delete {dir:?}: {e}")))?;
        }
        Ok(())
    }

    fn delete_key(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> StateResult<()> {
        for path in [
            self.state_path(user_id, session_id, key)?,
            self.list_path(user_id, session_id, key)?,
        ] {
            if path.exists() {
                std::fs::remove_file(&path)
                    .map_err(|e| StateError::Io(format!("failed to delete {path:?}: {e}")))?;
            }
        }
        Ok(())
    }

    fn list_session_ids(&self, user_id: Option<&str>) -> StateResult<Vec<String>> {
        let dir = self.root.join(safe_segment(&normalize_user(user_id)));
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut ids = Vec::new();
        for entry in std::fs::read_dir(&dir)
            .map_err(|e| StateError::Io(format!("failed to list {dir:?}: {e}")))?
        {
            let entry = entry.map_err(|e| StateError::Io(e.to_string()))?;
            if entry.path().is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    ids.push(decode_segment(name));
                }
            }
        }
        ids.sort();
        Ok(ids)
    }

    fn close(&self) {}
}

/// 目录/文件名安全化（对应 Java `safeSegment`）。
///
/// 与 Java 的差异：Java 直接输出 Base64URL 结果，而 Base64URL 的字符集本身就是
/// 「安全字符」，于是 `decodeSegment` 无法把它与普通名字区分开（原样返回，不还原）。
/// Rust 版给编码结果加 `~` 前缀（`~` 不属于安全字符集），使编解码可逆。
fn safe_segment(value: &str) -> String {
    if is_safe_segment(value) {
        return value.to_string();
    }
    format!("~{}", base64_url_no_pad(value.as_bytes()))
}

/// 反解 [`safe_segment`]（对应 Java `decodeSegment`）。
fn decode_segment(segment: &str) -> String {
    match segment.strip_prefix('~') {
        Some(encoded) => base64_url_decode(encoded).unwrap_or_else(|| segment.to_string()),
        None => segment.to_string(),
    }
}

fn is_safe_segment(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// URL-safe Base64（无 padding）编码。
fn base64_url_no_pad(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// URL-safe Base64 解码。
fn base64_url_decode(text: &str) -> Option<String> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
}

/// 原子写：先写 `.tmp` 再 rename 覆盖。
fn atomic_write(path: &Path, content: &str) -> StateResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| StateError::Io(format!("failed to create {parent:?}: {e}")))?;
    }
    let tmp = path.with_extension(format!(
        "{}tmp",
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| format!("{ext}."))
            .unwrap_or_default()
    ));
    std::fs::write(&tmp, content)
        .map_err(|e| StateError::Io(format!("failed to write {tmp:?}: {e}")))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| StateError::Io(format!("failed to rename {tmp:?}: {e}")))
}

/// 读取 jsonl 文件（不存在返回空；空行跳过）。
fn read_jsonl(path: &Path) -> StateResult<Vec<Value>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| StateError::Io(format!("failed to read {path:?}: {e}")))?;
    let mut values = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        values.push(serde_json::from_str(line)?);
    }
    Ok(values)
}

/// 汇总会话概要（对应 Java `SessionInfo` 的装配逻辑；文件后端才有的能力）。
impl JsonFileAgentStateStore {
    /// 列出某用户下所有会话的概要（大小、最近修改、条目数）。
    pub fn list_sessions(&self, user_id: Option<&str>) -> StateResult<Vec<SessionInfo>> {
        let mut infos = Vec::new();
        for session_id in self.list_session_ids(user_id)? {
            let dir = self.session_dir(user_id, &session_id)?;
            let mut size = 0u64;
            let mut count = 0usize;
            let mut last_modified = 0i64;
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    if let Ok(metadata) = entry.metadata() {
                        size += metadata.len();
                        count += 1;
                        if let Ok(modified) = metadata.modified() {
                            if let Ok(since) = modified.duration_since(std::time::UNIX_EPOCH) {
                                last_modified = last_modified.max(since.as_millis() as i64);
                            }
                        }
                    }
                }
            }
            infos.push(SessionInfo {
                session_id,
                size,
                last_modified,
                component_count: count,
            });
        }
        Ok(infos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Msg;

    // ---------- AgentState ----------

    #[test]
    fn agent_state_json_roundtrip_fills_defaults() {
        let mut state = AgentState::with_session_id("s-1");
        state.summary = "滚动摘要".into();
        state.cur_iter = 3;
        state.push_context(Msg::user("user", "你好"));

        let json = state.to_json().unwrap();
        let back = AgentState::from_json_str(&json).unwrap();
        assert_eq!(back.session_id, "s-1");
        assert_eq!(back.summary, "滚动摘要");
        assert_eq!(back.cur_iter, 3);
        assert_eq!(back.context.len(), 1);
        assert_eq!(back.reply_id, state.reply_id);

        // 旧版本 JSON 缺字段时应全部走默认值，不报错。
        let legacy = AgentState::from_json_str("{\"session_id\":\"s-2\"}").unwrap();
        assert_eq!(legacy.session_id, "s-2");
        assert_eq!(legacy.cur_iter, 0);
        assert!(!legacy.shutdown_interrupted);
        assert!(legacy.tool_context.activated_groups().is_empty());
        assert_eq!(legacy.tasks_context.revision(), 0);
        assert!(!legacy.plan_mode_context.plan_active);
    }

    #[test]
    fn rotate_reply_id_changes_id() {
        let mut state = AgentState::new();
        let before = state.reply_id.clone();
        state.rotate_reply_id();
        assert_ne!(state.reply_id, before);
        assert_eq!(state.reply_id.len(), 32);
    }

    // ---------- TaskContextState ----------

    fn confirmed_scope(state: &mut TaskContextState) {
        state
            .begin_task(TaskScope::new("t-1", "修好登录", "user:req").unwrap(), 0)
            .unwrap();
    }

    #[test]
    fn task_revision_guards_writes() {
        let mut state = TaskContextState::default();
        confirmed_scope(&mut state);
        assert_eq!(state.revision(), 1);
        // 用过期的修订号写，必须失败。
        let err = state
            .replace_tasks(vec![Task::new("a", "b")], 0)
            .unwrap_err();
        assert!(matches!(err, StateError::ConcurrentModification(_)));
        state.replace_tasks(vec![Task::new("a", "b")], 1).unwrap();
        assert_eq!(state.tasks().len(), 1);
        assert_eq!(state.revision(), 2);
    }

    #[test]
    fn begin_task_requires_new_identity_and_clears_progress() {
        let mut state = TaskContextState::default();
        confirmed_scope(&mut state);
        state
            .replace_tasks(vec![Task::new("旧任务", "d")], state.revision())
            .unwrap();
        // 同 taskId 换目标：拒绝，避免新旧证据混用。
        let err = state
            .begin_task(TaskScope::new("t-1", "换个目标", "user:req").unwrap(), 2)
            .unwrap_err();
        assert!(matches!(err, StateError::Invalid(_)));
        // 新 taskId：清空进度与需求，契约版本递增。
        let contract_before = state.contract_version();
        state
            .begin_task(TaskScope::new("t-2", "新目标", "user:req2").unwrap(), 2)
            .unwrap();
        assert!(state.tasks().is_empty());
        assert!(state.requirements().is_empty());
        assert_eq!(state.contract_version(), contract_before + 1);
    }

    #[test]
    fn propose_is_idempotent_and_decide_bumps_contract() {
        let mut state = TaskContextState::default();
        confirmed_scope(&mut state);
        let first = state
            .propose(TaskRequirementKind::AcceptanceCriterion, "能登录", "model:1")
            .unwrap();
        assert_eq!(first.status, TaskRequirementStatus::Candidate);
        let rev = state.revision();
        let again = state
            .propose(TaskRequirementKind::AcceptanceCriterion, "能登录", "model:1")
            .unwrap();
        assert_eq!(again.id, first.id);
        assert_eq!(state.revision(), rev, "重复提出不应推进修订号");

        let contract_before = state.contract_version();
        state
            .decide(
                &first.id,
                TaskRequirementStatus::Confirmed,
                TaskDecision {
                    authority: TaskAuthority::User,
                    source_ref: "user:ok".into(),
                },
                rev,
            )
            .unwrap();
        assert_eq!(state.contract_version(), contract_before + 1);
        assert_eq!(
            state.requirements()[0].status,
            TaskRequirementStatus::Confirmed
        );
    }

    #[test]
    fn record_verification_requires_confirmed_criterion_and_matching_binding() {
        let mut state = TaskContextState::default();
        confirmed_scope(&mut state);
        let requirement = state
            .propose(TaskRequirementKind::AcceptanceCriterion, "能登录", "model:1")
            .unwrap();
        state
            .decide(
                &requirement.id,
                TaskRequirementStatus::Confirmed,
                TaskDecision {
                    authority: TaskAuthority::User,
                    source_ref: "user:ok".into(),
                },
                state.revision(),
            )
            .unwrap();
        state
            .set_subject_version("commit-abc", state.revision())
            .unwrap();
        let binding = state.evidence_binding().unwrap();

        let report = TaskVerification {
            id: "v-1".into(),
            requirement_id: requirement.id.clone(),
            action_id: "a-1".into(),
            verifier_id: "tester".into(),
            outcome: VerificationOutcome::Passed,
            reason: "登录成功".into(),
            binding: binding.clone(),
            recorded_at: 1_700_000_000_000,
        };
        state
            .record_verification(report.clone(), state.revision())
            .unwrap();
        assert_eq!(state.verifications().len(), 1);

        // 被检对象版本变化 → 旧绑定失效，再记录必须冲突。
        let rev = state.revision();
        state.invalidate_subject(rev).unwrap();
        let err = state
            .record_verification(report, state.revision())
            .unwrap_err();
        assert!(matches!(err, StateError::ConcurrentModification(_)));
    }

    #[test]
    fn task_requirement_decision_consistency_is_enforced() {
        let mut requirement =
            TaskRequirement::candidate(TaskRequirementKind::Constraint, "别动线上库", "model:1");
        assert!(requirement.validate().is_ok());
        requirement.status = TaskRequirementStatus::Confirmed;
        // 已裁决但缺 decision：非法。
        assert!(requirement.validate().is_err());
    }

    // ---------- ToolContextState ----------

    #[test]
    fn activated_groups_keep_order_and_dedupe() {
        let mut ctx = ToolContextState::default();
        ctx.add_activated_groups(Some(&["b".to_string(), "a".to_string()]));
        ctx.add_activated_groups(Some(&["a".to_string(), "c".to_string()]));
        assert_eq!(ctx.activated_groups(), ["b", "a", "c"]);
        ctx.replace_activated_groups(Some(&["b".to_string()]), Some(&["d".to_string()]));
        assert_eq!(ctx.activated_groups(), ["a", "c", "d"]);
        ctx.set_activated_groups(Some(&["x".to_string()]));
        assert_eq!(ctx.activated_groups(), ["x"]);
        ctx.remove_activated_groups(Some(&["x".to_string()]));
        assert!(ctx.activated_groups().is_empty());
    }

    #[test]
    fn tool_context_rejects_invalid_limits() {
        assert!(ToolContextState::with_limits(1, 20_000.0).is_err());
        assert!(ToolContextState::with_limits(10, 5_000.0).is_err());
        assert!(ToolContextState::with_limits(10, 20_000.0).is_ok());
    }

    #[test]
    fn read_cache_evicts_by_count_and_invalidates_on_mtime_change() {
        let dir = std::env::temp_dir().join(format!("as-state-cache-{}", new_hex_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut ctx = ToolContextState::with_limits(2, 25_000.0).unwrap();

        for index in 0..3 {
            let path = dir.join(format!("f{index}.txt"));
            std::fs::write(&path, format!("line {index}\n")).unwrap();
            ctx.cache_file(path.to_str().unwrap(), vec![format!("line {index}")]);
        }
        // 数量上限 2：最旧的 f0 被淘汰。
        assert_eq!(ctx.read_file_cache().len(), 2);
        let f0 = dir.join("f0.txt");
        assert!(ctx.get_cache(f0.to_str().unwrap()).is_none());

        // 命中正常返回。
        let f2 = dir.join("f2.txt");
        assert!(ctx.get_cache(f2.to_str().unwrap()).is_some());

        // 文件被改写（mtime 变化）→ 缓存失效。
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(&f2, "changed\n").unwrap();
        assert!(ctx.get_cache(f2.to_str().unwrap()).is_none());
        assert_eq!(ctx.read_file_cache().len(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_cache_is_not_serialized() {
        let dir = std::env::temp_dir().join(format!("as-state-cache2-{}", new_hex_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.txt");
        std::fs::write(&path, "x\n").unwrap();
        let mut ctx = ToolContextState::default();
        ctx.cache_file(path.to_str().unwrap(), vec!["x".into()]);
        let json = ctx.to_json().unwrap();
        let back: ToolContextState = serde_json::from_str(&json).unwrap();
        assert!(back.read_file_cache().is_empty());
        assert_eq!(back.max_cache_files(), 100);
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---------- InMemoryAgentStateStore ----------

    #[test]
    fn in_memory_store_roundtrips_values_and_lists() {
        let store = InMemoryAgentStateStore::new();
        let state = AgentState::with_session_id("s-1");
        store.save(None, "s-1", "agent_state", &state).unwrap();
        let loaded: AgentState = store.get(None, "s-1", "agent_state").unwrap().unwrap();
        assert_eq!(loaded.session_id, "s-1");

        store
            .save_list(None, "s-1", "messages", &[Msg::user("user", "a"), Msg::user("user", "b")])
            .unwrap();
        let messages: Vec<Msg> = store.get_list(None, "s-1", "messages").unwrap();
        assert_eq!(messages.len(), 2);

        assert!(store.exists(None, "s-1").unwrap());
        assert_eq!(store.list_session_ids(None).unwrap(), vec!["s-1".to_string()]);
        store.delete_key(None, "s-1", "messages").unwrap();
        assert!(store.get_list::<Msg>(None, "s-1", "messages").unwrap().is_empty());
        store.delete_session(None, "s-1").unwrap();
        assert!(!store.exists(None, "s-1").unwrap());
    }

    #[test]
    fn in_memory_store_buckets_anonymous_and_named_users() {
        let store = InMemoryAgentStateStore::new();
        store.save(None, "s-1", "k", &1i64).unwrap();
        store.save(Some("alice"), "s-1", "k", &2i64).unwrap();
        assert_eq!(store.get::<i64>(None, "s-1", "k").unwrap(), Some(1));
        assert_eq!(store.get::<i64>(Some("alice"), "s-1", "k").unwrap(), Some(2));
        assert_eq!(store.session_count(), 2);
        // 匿名会话落在 __anon__ 桶，不会与同名用户冲突。
        assert_eq!(store.list_session_ids(None).unwrap().len(), 1);
        assert_eq!(store.list_session_ids(Some("alice")).unwrap().len(), 1);
    }

    #[test]
    fn in_memory_store_cas_semantics() {
        let store = InMemoryAgentStateStore::new();
        assert!(store.supports_versioning());

        // 不存在 → 版本 0；create-if-absent 成功写入版本 1。
        let absent: VersionedState<i64> = store.get_versioned(None, "s", "k").unwrap();
        assert!(!absent.is_present());
        assert_eq!(absent.version, 0);
        assert_eq!(store.save_if_version(None, "s", "k", &10i64, 0).unwrap(), 1);

        // 版本不匹配 → 冲突，返回 UNVERSIONED 且不写入。
        assert_eq!(
            store.save_if_version(None, "s", "k", &99i64, 0).unwrap(),
            UNVERSIONED
        );
        assert_eq!(store.get::<i64>(None, "s", "k").unwrap(), Some(10));

        // 版本匹配 → 写入并推进。
        assert_eq!(store.save_if_version(None, "s", "k", &11i64, 1).unwrap(), 2);
        // UNVERSIONED 期望值 = 无条件覆盖。
        assert_eq!(store.save_if_version(None, "s", "k", &12i64, UNVERSIONED).unwrap(), 3);
        assert_eq!(store.get::<i64>(None, "s", "k").unwrap(), Some(12));
    }

    #[test]
    fn blank_session_id_is_rejected() {
        let store = InMemoryAgentStateStore::new();
        let err = store.save(None, "  ", "k", &1i64).unwrap_err();
        assert!(matches!(err, StateError::InvalidSessionId));
    }

    #[test]
    fn type_mismatch_reports_serde_error() {
        let store = InMemoryAgentStateStore::new();
        store.save(None, "s", "k", &"text".to_string()).unwrap();
        let err = store.get::<i64>(None, "s", "k").unwrap_err();
        assert!(matches!(err, StateError::Serde(_)));
    }

    // ---------- JsonFileAgentStateStore ----------

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("as-state-{tag}-{}", new_hex_id()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn json_file_store_roundtrips_state_and_lists() {
        let root = temp_root("rt");
        let store = JsonFileAgentStateStore::new(&root).unwrap();
        let mut state = AgentState::with_session_id("s-1");
        state.summary = "摘要".into();
        store.save(Some("alice"), "s-1", "agent_state", &state).unwrap();
        let loaded: AgentState = store
            .get(Some("alice"), "s-1", "agent_state")
            .unwrap()
            .unwrap();
        assert_eq!(loaded.summary, "摘要");
        assert!(root.join("alice").join("s-1").join("agent_state.json").exists());

        store
            .save_list(Some("alice"), "s-1", "messages", &[Msg::user("user", "a")])
            .unwrap();
        store
            .save_list(Some("alice"), "s-1", "messages", &[Msg::user("user", "a"), Msg::user("user", "b")])
            .unwrap();
        let messages: Vec<Msg> = store.get_list(Some("alice"), "s-1", "messages").unwrap();
        assert_eq!(messages.len(), 2);
        // 追加写生效：jsonl 里是两行。
        let text = std::fs::read_to_string(root.join("alice").join("s-1").join("messages.jsonl")).unwrap();
        assert_eq!(text.lines().count(), 2);

        // 缩短列表 → 整表重写。
        store
            .save_list(Some("alice"), "s-1", "messages", &[Msg::user("user", "c")])
            .unwrap();
        let messages: Vec<Msg> = store.get_list(Some("alice"), "s-1", "messages").unwrap();
        assert_eq!(messages.len(), 1);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn json_file_store_encodes_unsafe_segments() {
        let root = temp_root("seg");
        let store = JsonFileAgentStateStore::new(&root).unwrap();
        let session = "会话/带空格:1";
        store.save(None, session, "k", &42i64).unwrap();
        assert_eq!(store.get::<i64>(None, session, "k").unwrap(), Some(42));
        // 目录名不该包含原始非法字符，且能反解回来。
        let user_dir = root.join(ANON_USER);
        let names: Vec<String> = std::fs::read_dir(&user_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names.len(), 1);
        assert!(!names[0].contains('/'));
        assert!(names[0].starts_with('~'), "编码段应带可逆前缀: {}", names[0]);
        assert_eq!(decode_segment(&names[0]), session);
        assert_eq!(store.list_session_ids(None).unwrap(), vec![session.to_string()]);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn json_file_store_delete_and_session_info() {
        let root = temp_root("del");
        let store = JsonFileAgentStateStore::new(&root).unwrap();
        store.save(Some("bob"), "s-1", "k", &1i64).unwrap();
        store.save(Some("bob"), "s-2", "k", &2i64).unwrap();
        assert!(store.exists(Some("bob"), "s-1").unwrap());

        let infos = store.list_sessions(Some("bob")).unwrap();
        assert_eq!(infos.len(), 2);
        assert!(infos.iter().all(|info| info.component_count == 1 && info.size > 0));

        store.delete_key(Some("bob"), "s-1", "k").unwrap();
        assert!(store.get::<i64>(Some("bob"), "s-1", "k").unwrap().is_none());
        store.delete_session(Some("bob"), "s-1").unwrap();
        assert!(!store.exists(Some("bob"), "s-1").unwrap());
        assert_eq!(store.list_session_ids(Some("bob")).unwrap(), vec!["s-2".to_string()]);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn json_file_store_is_not_versioned() {
        let root = temp_root("cas");
        let store = JsonFileAgentStateStore::new(&root).unwrap();
        assert!(!store.supports_versioning());
        // 非版本化后端：CAS 退化为无条件写，返回 UNVERSIONED。
        assert_eq!(
            store.save_if_version(None, "s", "k", &1i64, 7).unwrap(),
            UNVERSIONED
        );
        assert_eq!(store.get::<i64>(None, "s", "k").unwrap(), Some(1));
        std::fs::remove_dir_all(&root).ok();
    }
}
