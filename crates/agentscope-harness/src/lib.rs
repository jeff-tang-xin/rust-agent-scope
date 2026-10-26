//! Harness 层：工作区（workspace）、子代理声明（subagent）、后台任务（task）、
//! 计划模式（plan）与 HarnessAgent 编排外壳（agent）。
//!
//! 对应 Java `agentscope-harness` 模块的 `workspace` / `subagent` / `agent` 子包；
//! filesystem 全家桶（overlay/sandbox/remote）、gateway、skill、sandbox、memory、
//! SubagentsMiddleware 等随后续批次补齐。
//!
//! 与 Java 的有意差异（详见 `docs/ARCHITECTURE.md`）：
//! - `WorkspaceIndex`（SQLite best-effort 索引）未复刻——Java 侧本身允许为 null；
//! - `NamespaceFactory` / remote filesystem 未复刻，运行时数据路径不做命名空间前缀；
//! - `TaskRecord` 的时间字段用 epoch 毫秒整数（Java 是 ISO-8601 字符串）；
//! - 子代理工厂直接产出 `Arc<ReActAgent>`（当前唯一 Agent 实现，不建 trait 抽象），
//!   且不递归构建 `HarnessAgent`（workspace 上下文中间件未复刻，见 agent 模块文档）；
//! - 上下文压缩挂在 turn 入口（`HarnessAgent::call`/`stream_events`）而非每轮 ReAct
//!   迭代（中间件钩子全同步，压缩需 await）；overflow 恢复、压缩前轻量裁剪
//!   （truncateArgs/pruneToolResults）、ToolResultEvictionMiddleware 与自定义
//!   CompactionStrategy 未复刻（详见 compaction 模块文档）。

pub mod agent;
pub mod base_store;
pub mod compaction;
pub mod coordination;
pub mod filesystem;
pub mod memory;
pub mod middleware_memory;
pub mod plan;
pub mod skill;
pub mod subagents_middleware;
pub mod subagent;
pub mod task;
pub mod team;
pub mod tools;
pub mod tools_agent_spawn;
pub mod tools_filesystem;
pub mod tools_memory;
pub mod tools_skill;
pub mod tools_team;
pub mod workspace;

pub use agent::{
    allowlisted_inherited_toolkit, build_subagent_sys_prompt, derive_child_session_id,
    resolve_declared_sys_prompt_base, resolve_declared_workspace, sanitize_identifier,
    HarnessAgent, HarnessAgentBuilder, ModelResolver, GENERAL_PURPOSE_BASE_PROMPT,
    GENERAL_PURPOSE_DESCRIPTION, GENERAL_PURPOSE_NAME, SUBAGENT_CONTEXT_SECTION,
};
pub use coordination::{blank_to_empty, IsolationScope, LocalPeriodicGate, PeriodicGate};
pub use compaction::{CompactionConfig, ConversationCompactor, DEFAULT_SUMMARY_PROMPT, FALLBACK_TRIGGER_TOKENS};
pub use memory::{
    MemoryBackgroundTasks, MemoryConfig, MemoryConsolidator, MemoryFlushManager, FlushTrigger,
    COMPACTION_SUMMARY_MSG_NAME, CONSOLIDATION_STATE_FILE, DEFAULT_CONSOLIDATION_MAX_TOKENS,
    DEFAULT_CONSOLIDATION_MIN_GAP, DEFAULT_DAILY_FILE_RETENTION_DAYS,
};
pub use middleware_memory::{MemoryFlushMiddleware, MemoryMaintenanceMiddleware};
pub use plan::{PlanModeManager, PlanModeMiddleware};
pub use subagents_middleware::{
    build_delivery_reminder, build_subagent_state_msg, build_task_summary,
    is_visible_to_llm, prepend_to_system_message, render_subagent_catalog,
    render_subagent_section, state_literal, SubagentsMiddleware, MAX_DELIVERIES_PER_REMINDER,
    MAX_TASK_SUMMARY_ENTRIES,
};
pub use subagent::{
    AgentSpecLoader, DeclarationMode, DefaultAgentManager, SubagentDeclaration, SubagentEntry,
    SubagentFactory, WorkspaceMode,
};
pub use task::{
    BackgroundTask, CompletionCallback, InMemoryTaskRepository, SUPPRESS_COMPLETION_CALLBACK,
    TaskDelivery, TaskRecord, TaskRepository, TaskRunSpec, TaskStatus, WorkspaceTaskRepository,
};
pub use filesystem::{CompositeFilesystem, InMemoryFilesystem, OverlayFilesystem};
pub use tools_filesystem::{
    bounded_listing, compile_keywords, fuzzy_search, FilesystemToolSet, KeywordMode, MatchLevel,
    MatchRange, SearchResult, DEFAULT_GLOB_LIMIT, DEFAULT_GREP_LIMIT, MAX_LISTING_CHARS,
    MAX_LISTING_ENTRIES, MAX_SEARCH_LIMIT,
};
pub use tools_memory::{MemoryGetTool, MemorySaveTool, MemorySearchTool, MemoryToolSet};
pub use skill::{
    AgentSkill, SkillSecurityScanner, SkillUtil, WorkspaceSkillRepository, SKILL_FILE,
};
pub use base_store::{BaseStore, InMemoryBaseStore, StoreItem};
pub use team::{
    CompletedTask, InterruptedTask, LocalTeamClient, MemberSnapshot, RecentMessage,
    RecoveryContext, TeamClient, TeamContext, TeamCreateSpec, TeamError, TeamInfo, TeamMemberInfo,
    TeamMemberSpec, TeamMessage, TeamTask, WakeHook,
};
pub use tools_skill::{
    harness_platform_tools, manage_entry, ProposeSkillTool, SkillAuditEntry, SkillAuditLog,
    SkillManageConfig, SkillManageTool, SkillUsageState, SkillUsageStore, ALLOWED_SUBDIRS,
    DEFAULT_DRAFTS_DIR, DEFAULT_MAIN_DIR, MAX_DESCRIPTION_LENGTH, MAX_NAME_LENGTH,
    MAX_SKILL_CONTENT_CHARS, MAX_SKILL_FILE_BYTES,
};
pub use tools_team::{lead_context, team_action_tools, TeamActionTool, TeamTool};
pub use tools_agent_spawn::{
    deterministic_hash, AgentListTool, AgentSendTool, AgentSpawnCore, AgentSpawnTool,
    CTX_EXPOSE_TO_USER, CTX_FORCE_SYNC, CTX_FORCE_SYNC_TIMEOUT_SECONDS,
};
pub use workspace::{
    DownloadedFile, FileInfo, Filesystem, GrepMatch, LocalFilesystem, LocalFsMode, PathPolicy,
    WorkspaceConstants, WorkspaceManager, WorkspacePathNormalizer,
};
