//! AgentSpawnTool 三工具（对应 Java `AgentSpawnTool` 的本地路径）：
//!
//! - `agent_spawn` — spawn 子代理并执行任务（同步等待或 `timeout_seconds=0` 异步）
//! - `agent_send` — 向已 spawn 的子代理发后续消息（复用同一实例与会话）
//! - `agent_list` — 列出活跃子代理
//!
//! 与 Java 的有意差异（本批范围，随后续批次补齐）：
//! - 远端声明（`url`）→ 直接报错（transport 随分布式批次实现）；
//! - gateway 暴露（`expose_to_user`）按 Java 优先级解析，但无 bridge 时不生效；
//! - 子代理事件转发进父事件流：待 emitter 贯穿工具执行后补（当前走非流式路径）；
//! - plan mode / DENY 规则向子代理传播：待 permission 接线与 HarnessAgent
//!   句柄可达后补（Java 侧 `propagatePlanMode` / `propagateParentDenyRules`）；
//! - `RuntimeContext` 的 per-call 覆盖键（`CTX_FORCE_SYNC` 等）从
//!   [`ToolContext::metadata`] 读取——接线把 `rc.attributes` 桥接进 metadata
//!   后即自动生效。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentscope_core::context::RuntimeContext;
use agentscope_core::message::Msg;
use agentscope_core::state::SpawnEntry;
use agentscope_core::tool::{Tool, ToolContext, ToolOutput};
use agentscope_core::Result;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use agentscope_core::agent::ReActAgent;
use crate::subagent::DefaultAgentManager;
use crate::task::{TaskRepository, TaskRunSpec};

const DEFAULT_TIMEOUT_SECONDS: i64 = 30;
const MAX_TIMEOUT_SECONDS: i64 = 600;
const MAX_SPAWN_DEPTH: i32 = 3;

/// [`ToolContext::metadata`] 键：强制本次调用同步执行（忽略 `timeout_seconds=0`，
/// 超时中断而不晋升后台任务）。
pub const CTX_FORCE_SYNC: &str = "agentscope.subagent.force_sync";

/// [`ToolContext::metadata`] 键：force-sync 生效时的同步等待秒数覆盖。
pub const CTX_FORCE_SYNC_TIMEOUT_SECONDS: &str = "agentscope.subagent.force_sync_timeout_seconds";

/// [`ToolContext::metadata`] 键：per-call 覆盖子代理用户暴露决策。
pub const CTX_EXPOSE_TO_USER: &str = "agentscope.subagent.expose_to_user";

/// 已 spawn 子代理的进程内登记（对应 Java `SpawnedAgent` record）。
#[derive(Clone)]
struct SpawnedAgent {
    key: String,
    agent_id: String,
    session_id: String,
    label: Option<String>,
    agent: Arc<ReActAgent>,
    depth: i32,
}

/// label 反查键（对应 Java `LabelKey`）：按 (user, parent session, 小写 label) 隔离。
#[derive(Clone, PartialEq, Eq, Hash)]
struct LabelKey {
    user_id: Option<String>,
    parent_session_id: Option<String>,
    label: String,
}

/// 三个工具共享的核心状态与逻辑（对应 Java `AgentSpawnTool` 实例本体）。
pub struct AgentSpawnCore {
    agent_manager: Arc<DefaultAgentManager>,
    task_repository: Arc<dyn TaskRepository>,
    parent_spawn_depth: i32,
    agents: Mutex<HashMap<String, SpawnedAgent>>,
    label_to_key: Mutex<HashMap<LabelKey, String>>,
}

impl AgentSpawnCore {
    /// 构造核心（`parent_spawn_depth`：父代理的 spawn 深度，顶层主代理为 0）。
    pub fn new(
        agent_manager: Arc<DefaultAgentManager>,
        task_repository: Arc<dyn TaskRepository>,
        parent_spawn_depth: i32,
    ) -> Arc<Self> {
        Arc::new(Self {
            agent_manager,
            task_repository,
            parent_spawn_depth,
            agents: Mutex::new(HashMap::new()),
            label_to_key: Mutex::new(HashMap::new()),
        })
    }

    /// `agent_spawn` 主流程（对齐 Java `agentSpawn`）。
    async fn spawn(&self, ctx: &ToolContext, input: &Value) -> ToolOutput {
        let Some(agent_id) = str_param(input, "agent_id") else {
            return ToolOutput::error("agent_id is required");
        };
        let task = str_param(input, "task").filter(|t| !t.trim().is_empty());
        let label = str_param(input, "label")
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty());
        let timeout_seconds = i64_param(input, "timeout_seconds");

        let next_depth = self.parent_spawn_depth + 1;
        if next_depth > MAX_SPAWN_DEPTH {
            return ToolOutput::error(format!(
                "Maximum spawn depth exceeded (max={MAX_SPAWN_DEPTH})"
            ));
        }

        if self.agent_manager.is_primary_only(&agent_id) {
            return ToolOutput::error(format!(
                "agent_id '{agent_id}' is PRIMARY-only and cannot be spawned as a subagent."
            ));
        }
        if !self.agent_manager.has_agent(&agent_id) {
            return ToolOutput::error(format!("Unknown agent_id: {agent_id}"));
        }
        let decl = self.agent_manager.get_declaration(&agent_id);
        let persist = decl.as_ref().is_some_and(|d| d.persist_session);
        let remote = decl.as_ref().is_some_and(|d| d.url.is_some());

        let user_id = ctx.user_id.clone();
        let parent_session_id = blank_to_none(&ctx.session_id);

        let (key, session_id) = if persist {
            let user_part = identity_part(user_id.as_deref());
            let session_part = identity_part(parent_session_id.as_deref());
            let agent_part = identity_part(Some(&agent_id));
            let label_part = identity_part(label.as_deref());
            let hash = deterministic_hash(&[&user_part, &session_part, &agent_part, &label_part]);
            (format!("agent:{agent_id}:{hash}"), format!("sub-{hash}"))
        } else {
            let id = Uuid::new_v4().simple().to_string();
            (format!("agent:{agent_id}:{id}"), format!("sub-{id}"))
        };

        // label 冲突检查（对应 Java：已有映射且 (!persist || 指向不同 key) 则拒绝）。
        if let Some(canon) = &label {
            let lk = label_key(&user_id, &parent_session_id, canon);
            let conflict = self
                .label_to_key
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&lk)
                .is_some_and(|existing| !persist || existing != &key);
            if conflict {
                return ToolOutput::error(format!("Label already in use: {canon}"));
            }
        }

        // 取建（对应 Java computeIfAbsent）：先查缓存，未命中则在锁外建实例，
        // 再锁内双检插入（并发 spawn 同一身份只建一份）。
        let mut created = false;
        // 先取值再判断：if let 的临时 MutexGuard 会活到整个块结束，
        // else 分支再 lock 同锁即自死锁（项目已多次踩坑）。
        let cached = self
            .agents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned();
        let agent = if let Some(existing) = cached {
            existing.agent
        } else {
            let parent_rc = parent_runtime_context(ctx);
            let new_agent = match self
                .agent_manager
                .create_agent_if_present(&parent_rc, &agent_id)
            {
                Ok(a) => a,
                Err(_) => return ToolOutput::error(format!("Unknown agent_id: {agent_id}")),
            };
            created = true;
            let mut agents = self.agents.lock().unwrap_or_else(|e| e.into_inner());
            match agents.get(&key) {
                Some(existing) => existing.agent.clone(),
                None => {
                    agents.insert(
                        key.clone(),
                        SpawnedAgent {
                            key: key.clone(),
                            agent_id: agent_id.clone(),
                            session_id: session_id.clone(),
                            label: label.clone(),
                            agent: new_agent.clone(),
                            depth: next_depth,
                        },
                    );
                    new_agent
                }
            }
        };
        if let Some(canon) = &label {
            self.label_to_key
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(label_key(&user_id, &parent_session_id, canon), key.clone());
        }
        self.persist_spawn_entry(ctx, &key, &agent_id, &session_id, label.as_deref(), next_depth);

        let spawn_info = format!("agent_key: {key}\nagent_id: {agent_id}\nsession_id: {session_id}");

        match task {
            None => {
                if created {
                    ToolOutput::ok(format!("{spawn_info}\nstatus: accepted"))
                } else {
                    ToolOutput::ok(format!("{spawn_info}\nstatus: accepted (reused)"))
                }
            }
            Some(task) => {
                self.exec_spawn_task(
                    ctx,
                    agent,
                    &agent_id,
                    &session_id,
                    &spawn_info,
                    &task,
                    timeout_seconds,
                    remote,
                )
                .await
            }
        }
    }

    /// 对已 spawn（或复用）的子代理执行任务（对齐 Java `execSpawnTask`）。
    #[allow(clippy::too_many_arguments)]
    async fn exec_spawn_task(
        &self,
        ctx: &ToolContext,
        agent: Arc<ReActAgent>,
        agent_id: &str,
        session_id: &str,
        spawn_info: &str,
        task: &str,
        timeout_seconds: Option<i64>,
        remote: bool,
    ) -> ToolOutput {
        if remote {
            return ToolOutput::error(
                "remote subagent transport is not supported yet (distributed phase)",
            );
        }
        let force_sync = is_force_sync(ctx);
        let timeout_ms = resolve_effective_timeout_ms(timeout_seconds, ctx);

        if timeout_ms == 0 {
            return self
                .submit_background(ctx, agent, session_id, agent_id, spawn_info, task)
                .await;
        }
        self.exec_with_timeout_promotion(
            ctx,
            agent,
            session_id,
            spawn_info,
            timeout_ms,
            agent_id,
            force_sync,
            task,
        )
        .await
    }

    /// `timeout_seconds=0`：提交后台任务并返回 `task_id`（对齐 Java BG_RESULT_TEMPLATE）。
    async fn submit_background(
        &self,
        ctx: &ToolContext,
        agent: Arc<ReActAgent>,
        session_id: &str,
        agent_id: &str,
        header: &str,
        task: &str,
    ) -> ToolOutput {
        let task_id = format!("task_{}", Uuid::new_v4());
        let sid = session_id.to_string();
        let uid = ctx.user_id.clone();
        let prompt = task.to_string();
        let spec = TaskRunSpec::Local {
            execution: Arc::new(move || {
                let agent = agent.clone();
                let sid = sid.clone();
                let uid = uid.clone();
                let prompt = prompt.clone();
                Box::pin(async move { invoke_agent(agent, sid, uid, prompt).await })
            }),
        };
        // 空 session 不设置（保持 None → 仓库侧落 "default" 槽）；
        // with_session_id("") 会存 Some("")，与 list 侧的 default 推导不一致。
        let rc = parent_runtime_context(ctx);
        match self
            .task_repository
            .put_task(&rc, &task_id, agent_id, &ctx.session_id, spec)
        {
            Ok(_) => ToolOutput::ok(format!("{header}\n{}", bg_result_template(&task_id))),
            Err(e) => ToolOutput::error(format!("failed to submit background task: {e}")),
        }
    }

    /// 同步执行 + 超时晋升（对齐 Java `execWithTimeoutPromotion`）：
    ///
    /// - 规定时间内完成 → `status: ok` + 回复文本；
    /// - 超时且 force-sync → 中断任务，返回 `status: timeout`（不晋升）；
    /// - 超时（默认）→ 任务继续在后台跑，句柄以 [`TaskRunSpec::Adopted`]
    ///   登记进 [`TaskRepository`]，返回 `status: timeout_promoted` + `task_id`；
    /// - 执行报错 → `status: error`。
    #[allow(clippy::too_many_arguments)]
    async fn exec_with_timeout_promotion(
        &self,
        ctx: &ToolContext,
        agent: Arc<ReActAgent>,
        session_id: &str,
        header: &str,
        timeout_ms: u64,
        agent_id: &str,
        force_sync: bool,
        task: &str,
    ) -> ToolOutput {
        let sid = session_id.to_string();
        let uid = ctx.user_id.clone();
        let prompt = task.to_string();
        let mut handle = tokio::spawn(async move { invoke_agent(agent, sid, uid, prompt).await });

        // 借用 &mut JoinHandle 等待：超时只丢弃借用的 future，句柄本体仍在，
        // 既可 abort（force-sync）也可移交 Adopted 规格（晋升）。
        match tokio::time::timeout(Duration::from_millis(timeout_ms), &mut handle).await {
            Ok(Ok(Ok(text))) => ToolOutput::ok(format!("{header}\nstatus: ok\nreply:\n{text}")),
            Ok(Ok(Err(e))) => ToolOutput::error(format!("{header}\nstatus: error\nerror: {e}")),
            Ok(Err(join_err)) => {
                ToolOutput::error(format!("{header}\nstatus: error\nerror: {join_err}"))
            }
            Err(_elapsed) => {
                if force_sync {
                    handle.abort();
                    ToolOutput::error(format!(
                        "{header}\n{}",
                        format_force_sync_timeout(timeout_ms)
                    ))
                } else {
                    let task_id = format!("task_{}", Uuid::new_v4());
                    let rc = parent_runtime_context(ctx);
                    let spec = TaskRunSpec::Adopted { handle };
                    match self
                        .task_repository
                        .put_task(&rc, &task_id, agent_id, &ctx.session_id, spec)
                    {
                        Ok(_) => ToolOutput::ok(format!(
                            "{header}\n{}",
                            format_timeout_promoted(&task_id, timeout_ms)
                        )),
                        Err(e) => ToolOutput::error(format!(
                            "{header}\nstatus: error\nerror: failed to promote task: {e}"
                        )),
                    }
                }
            }
        }
    }

    /// `agent_send` 主流程（对齐 Java `agentSend`）。
    async fn send(&self, ctx: &ToolContext, input: &Value) -> ToolOutput {
        let agent_key = str_param(input, "agent_key")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let label = str_param(input, "label")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let message = str_param(input, "message").unwrap_or_default();
        let timeout_seconds = i64_param(input, "timeout_seconds");

        let has_key = agent_key.is_some();
        let has_label = label.is_some();
        if has_key && has_label {
            return ToolOutput::error("Provide either agent_key or label, not both.");
        }
        if !has_key && !has_label {
            return ToolOutput::error("Either agent_key or label is required.");
        }
        if message.trim().is_empty() {
            return ToolOutput::error("message is required");
        }

        let user_id = ctx.user_id.clone();
        let parent_session_id = blank_to_none(&ctx.session_id);

        let key = if let Some(k) = &agent_key {
            k.clone()
        } else {
            let l = label.as_ref().expect("label checked above");
            let lk = label_key(&user_id, &parent_session_id, l);
            let cached = self
                .label_to_key
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&lk)
                .cloned();
            match cached.or_else(|| self.resolve_label_from_state(ctx, l)) {
                Some(k) => k,
                None => return ToolOutput::error(format!("Unknown label: {l}")),
            }
        };

        // 先取值再 match：match scrutinee 的临时 MutexGuard 活到整个 match，
        // restore_from_state 内部再 lock 同锁即自死锁。
        let cached = self
            .agents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned();
        let spawned = match cached {
            Some(s) => s,
            None => match self.restore_from_state(ctx, &key) {
                Some(s) => s,
                None => return ToolOutput::error(format!("Unknown agent_key: {key}")),
            },
        };

        let remote = self
            .agent_manager
            .get_declaration(&spawned.agent_id)
            .is_some_and(|d| d.url.is_some());
        if remote {
            return ToolOutput::error(
                "remote subagent transport is not supported yet (distributed phase)",
            );
        }

        let force_sync = is_force_sync(ctx);
        let timeout_ms = resolve_effective_timeout_ms(timeout_seconds, ctx);
        let header = format!("agent_key: {key}");

        if timeout_ms == 0 {
            return self
                .submit_background(
                    ctx,
                    spawned.agent.clone(),
                    &spawned.session_id,
                    &spawned.agent_id,
                    &header,
                    message.trim(),
                )
                .await;
        }
        self.exec_with_timeout_promotion(
            ctx,
            spawned.agent.clone(),
            &spawned.session_id,
            &header,
            timeout_ms,
            &spawned.agent_id,
            force_sync,
            message.trim(),
        )
        .await
    }

    /// `agent_list`（对齐 Java `agentList`）。
    fn list(&self) -> ToolOutput {
        let agents = self.agents.lock().unwrap_or_else(|e| e.into_inner());
        if agents.is_empty() {
            return ToolOutput::ok("No active subagents.");
        }
        let mut sb = format!("Active subagents ({}):\n", agents.len());
        for a in agents.values() {
            sb.push_str(&format!("- agent_key: {}\n", a.key));
            sb.push_str(&format!("  agent_id: {}\n", a.agent_id));
            if let Some(label) = &a.label {
                sb.push_str(&format!("  label: {label}\n"));
            }
            sb.push_str(&format!("  spawn_depth: {}\n", a.depth));
        }
        ToolOutput::ok(sb.trim_end().to_string())
    }

    /// 把 spawn 登记写进父会话状态（对应 Java `persistSpawnEntry`）。
    fn persist_spawn_entry(
        &self,
        ctx: &ToolContext,
        key: &str,
        agent_id: &str,
        session_id: &str,
        label: Option<&str>,
        depth: i32,
    ) {
        if let Some(state) = &ctx.state {
            if let Ok(mut st) = state.lock() {
                st.tool_context.put_spawn_entry(SpawnEntry {
                    key: key.to_string(),
                    agent_id: agent_id.to_string(),
                    session_id: session_id.to_string(),
                    label: label.unwrap_or_default().to_string(),
                    depth,
                });
            }
        }
    }

    /// 从父会话状态的 spawn 登记表按 label 反查 key（小写匹配，
    /// 对应 Java `tryResolveLabelFromState`）。
    fn resolve_label_from_state(&self, ctx: &ToolContext, label: &str) -> Option<String> {
        let state = ctx.state.as_ref()?;
        let st = state.lock().ok()?;
        let lower = label.to_lowercase();
        st.tool_context
            .spawn_registry()
            .values()
            .find(|entry| !entry.label.is_empty() && entry.label.to_lowercase() == lower)
            .map(|entry| entry.key.clone())
    }

    /// 从父会话状态懒重建子代理实例（对应 Java `tryRestoreFromState`）：
    /// 会话恢复或跨副本路由后 `agent_send` 仍可按 key 找回活实例。
    fn restore_from_state(&self, ctx: &ToolContext, key: &str) -> Option<SpawnedAgent> {
        let state = ctx.state.as_ref()?;
        let entry = {
            let st = state.lock().ok()?;
            st.tool_context.spawn_registry().get(key)?.clone()
        };
        let parent_rc = parent_runtime_context(ctx);
        let agent = self
            .agent_manager
            .create_agent_if_present(&parent_rc, &entry.agent_id)
            .ok()?;
        let label = if entry.label.is_empty() {
            None
        } else {
            Some(entry.label.clone())
        };
        let restored = SpawnedAgent {
            key: key.to_string(),
            agent_id: entry.agent_id.clone(),
            session_id: entry.session_id.clone(),
            label: label.clone(),
            agent,
            depth: entry.depth,
        };
        self.agents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.to_string(), restored.clone());
        if let Some(l) = &label {
            self.label_to_key
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    label_key(&ctx.user_id.clone(), &blank_to_none(&ctx.session_id), l),
                    key.to_string(),
                );
        }
        Some(restored)
    }
}

// ---------------------------------------------------------------------------
// 三个 Tool 包装（Java 一个类三个 @Tool 方法；Rust 一个类型一个 Tool，
// 因此按 TeamTool 模式拆三个共享 core 的包装类型）。
// ---------------------------------------------------------------------------

/// `agent_spawn` 工具。
pub struct AgentSpawnTool(Arc<AgentSpawnCore>);

impl AgentSpawnTool {
    /// 用共享 core 构造。
    pub fn new(core: Arc<AgentSpawnCore>) -> Self {
        Self(core)
    }
}

#[async_trait::async_trait]
impl Tool for AgentSpawnTool {
    fn name(&self) -> &str {
        "agent_spawn"
    }

    fn description(&self) -> &str {
        "Spawn an isolated subagent for delegated or background work. \
Every response starts with three lines: agent_key (pass this verbatim to \
agent_send as agent_key), agent_id (the subagent type name), and session_id \
(internal; do not use as agent_key). Sync mode returns the reply below that; \
async (timeout_seconds=0) adds task_id for task_output or wait_async_results; \
task_id is NOT agent_key. Multiple sync tool calls in one turn run in parallel \
by default; pass a Toolkit with parallel=false to serialize, \
or use async tasks for fire-and-forget parallelism. \
The subagent does not inherit the parent conversation, so include every \
file path, constraint, and expected output format the task needs. New \
sessions start from the supplied task; persistent sessions may retain their own history. \
Do not re-spawn the same task reworded to bypass a permission denial; \
report the limitation instead."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent_id": {"type": "string", "description": "Subagent identifier to instantiate"},
                "task": {"type": "string", "description": "Task or prompt to send to the spawned agent"},
                "label": {"type": "string", "description": "Optional human-readable label for referencing via agent_send"},
                "timeout_seconds": {"type": "integer", "description": "Max seconds to wait for the task result. 0=fire-and-forget, returns task_id. Default: 30. Max: 600."},
                "expose_to_user": {"type": "boolean", "description": "When true, the spawned subagent becomes directly addressable by the user via a thread_id handle. Requires a gateway bridge to be configured."}
            },
            "required": ["agent_id"]
        })
    }

    async fn call_with_context(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        Ok(self.0.spawn(ctx, &input).await)
    }

    async fn call(&self, input: Value) -> Result<ToolOutput> {
        Ok(self
            .0
            .spawn(&ToolContext::new("direct", self.name()), &input)
            .await)
    }
}

/// `agent_send` 工具。
pub struct AgentSendTool(Arc<AgentSpawnCore>);

impl AgentSendTool {
    /// 用共享 core 构造。
    pub fn new(core: Arc<AgentSpawnCore>) -> Self {
        Self(core)
    }
}

#[async_trait::async_trait]
impl Tool for AgentSendTool {
    fn name(&self) -> &str {
        "agent_send"
    }

    fn description(&self) -> &str {
        "Send a message to an existing subagent. Use the exact string from the \
agent_key line of agent_spawn output (starts with agent:), or the label \
you set at spawn. Do not pass agent_id, session_id, or task_id here. \
timeout_seconds=0 returns task_id for task_output or wait_async_results. \
Sends are follow-ups to a subagent that already holds the context you \
gave it. Use follow-ups to steer the task or continue its work. Do not \
re-send the same request reworded to bypass a permission denial."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent_key": {"type": "string", "description": "Exact value from agent_spawn's first line after 'agent_key: ' (format agent:<type>:<uuid>). Not agent_id, session_id, or task_id. Mutually exclusive with label."},
                "label": {"type": "string", "description": "Agent label assigned at spawn time. Mutually exclusive with agent_key."},
                "message": {"type": "string", "description": "Message to send to the subagent"},
                "timeout_seconds": {"type": "integer", "description": "Max seconds to wait for a reply. 0=fire-and-forget, returns task_id. Default: 30. Max: 600."}
            }
        })
    }

    async fn call_with_context(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        Ok(self.0.send(ctx, &input).await)
    }

    async fn call(&self, input: Value) -> Result<ToolOutput> {
        Ok(self
            .0
            .send(&ToolContext::new("direct", self.name()), &input)
            .await)
    }
}

/// `agent_list` 工具。
pub struct AgentListTool(Arc<AgentSpawnCore>);

impl AgentListTool {
    /// 用共享 core 构造。
    pub fn new(core: Arc<AgentSpawnCore>) -> Self {
        Self(core)
    }
}

#[async_trait::async_trait]
impl Tool for AgentListTool {

    /// 只读工具：权限模式检查（EXPLORE/ACCEPT_EDITS）与 studio 免确认白名单依据。
    fn is_read_only(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "agent_list"
    }

    fn description(&self) -> &str {
        "List active subagents spawned by this agent."
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    async fn call_with_context(&self, _input: Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        Ok(self.0.list())
    }

    async fn call(&self, _input: Value) -> Result<ToolOutput> {
        Ok(self.0.list())
    }
}

// ---------------------------------------------------------------------------
//  辅助函数
// ---------------------------------------------------------------------------

/// 调用子代理：child 运行时上下文（继承 user、指定 session）→ 执行 → 回复文本
/// （对应 Java `DefaultAgentManager.invokeAgent` 的本地路径）。
async fn invoke_agent(
    agent: Arc<ReActAgent>,
    session_id: String,
    user_id: Option<String>,
    prompt: String,
) -> std::result::Result<String, String> {
    let rc = RuntimeContext::new()
        .with_user_id(user_id.unwrap_or_default())
        .with_session_id(session_id);
    let msg = Msg::user("user", prompt);
    agent
        .call_with_context(rc, msg)
        .await
        .map(|m| m.text())
        .map_err(|e| e.to_string())
}

/// 从工具上下文构造父运行时上下文（工厂与状态恢复用）。
fn parent_runtime_context(ctx: &ToolContext) -> RuntimeContext {
    let rc = RuntimeContext::new();
    let rc = match &ctx.user_id {
        Some(u) => rc.with_user_id(u.clone()),
        None => rc,
    };
    if ctx.session_id.is_empty() {
        rc
    } else {
        rc.with_session_id(ctx.session_id.clone())
    }
}

fn label_key(
    user_id: &Option<String>,
    parent_session_id: &Option<String>,
    label: &str,
) -> LabelKey {
    LabelKey {
        user_id: user_id.clone(),
        parent_session_id: parent_session_id.clone(),
        label: label.to_lowercase(),
    }
}

/// 确定性 12 字符 hex 哈希（对应 Java `SessionIdUtils.deterministicHash`：
/// SHA-256(parts 以 "/" 连接) 取前 6 字节 hex）。
pub fn deterministic_hash(parts: &[&str]) -> String {
    let seed = parts.join("/");
    let digest = Sha256::digest(seed.as_bytes());
    digest[..6].iter().map(|b| format!("{b:02x}")).collect()
}

/// 身份段（对应 Java `identityPart`）：`None` → `-1:`；否则 `长度:值`。
fn identity_part(value: Option<&str>) -> String {
    match value {
        None => "-1:".to_string(),
        Some(v) => format!("{}:{v}", v.chars().count()),
    }
}

fn blank_to_none(s: &str) -> Option<String> {
    if s.trim().is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

fn str_param(input: &Value, key: &str) -> Option<String> {
    input.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

fn i64_param(input: &Value, key: &str) -> Option<i64> {
    input.get(key).and_then(|v| v.as_i64())
}

/// metadata 里的布尔（Boolean 或其字符串形态，对应 Java `asBoolean`）。
fn metadata_bool(ctx: &ToolContext, key: &str) -> Option<bool> {
    match ctx.metadata.get(key) {
        Some(Value::Bool(b)) => Some(*b),
        Some(Value::String(s)) if !s.trim().is_empty() => {
            Some(s.trim().eq_ignore_ascii_case("true"))
        }
        _ => None,
    }
}

/// metadata 里的整数（Number 或数字字符串，对应 Java `asPositiveOrZeroInt`）。
fn metadata_int(ctx: &ToolContext, key: &str) -> Option<i64> {
    match ctx.metadata.get(key) {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) if !s.trim().is_empty() => s.trim().parse().ok(),
        _ => None,
    }
}

/// 是否启用 force-sync（对应 Java `isForceSync`）。
fn is_force_sync(ctx: &ToolContext) -> bool {
    metadata_bool(ctx, CTX_FORCE_SYNC).unwrap_or(false)
}

/// 基础超时解析（对应 Java `resolveTimeoutMs`）：缺省 30s；`<=0` → 0（异步）；
/// 上限 600s。
fn resolve_timeout_ms(timeout_seconds: Option<i64>) -> u64 {
    match timeout_seconds {
        None => (DEFAULT_TIMEOUT_SECONDS as u64) * 1000,
        Some(s) if s <= 0 => 0,
        Some(s) => (s.min(MAX_TIMEOUT_SECONDS) as u64) * 1000,
    }
}

/// 生效超时解析（对应 Java `resolveEffectiveTimeoutMs`）：force-sync 时
/// 覆盖值优先，`0` 被强制为默认同步超时。
fn resolve_effective_timeout_ms(timeout_seconds: Option<i64>, ctx: &ToolContext) -> u64 {
    if is_force_sync(ctx) {
        if let Some(override_seconds) = metadata_int(ctx, CTX_FORCE_SYNC_TIMEOUT_SECONDS) {
            let seconds = if override_seconds <= 0 {
                DEFAULT_TIMEOUT_SECONDS
            } else {
                override_seconds
            };
            return (seconds.min(MAX_TIMEOUT_SECONDS) as u64) * 1000;
        }
    }
    let ms = resolve_timeout_ms(timeout_seconds);
    if is_force_sync(ctx) && ms == 0 {
        return (DEFAULT_TIMEOUT_SECONDS as u64) * 1000;
    }
    ms
}

/// 后台任务结果模板（对齐 Java `BG_RESULT_TEMPLATE`）。
fn bg_result_template(task_id: &str) -> String {
    format!(
        "status: accepted\ntask_id: {task_id}\n\
Use task_output(task_id='{task_id}', block=false) to check status, \
wait_async_results(task_ids=...) to wait for a chosen group, \
wait_async_results(wait_all=true) to wait for all current background tasks, \
task_cancel(task_id='{task_id}') to cancel, or task_list() to see all tasks. \
Do NOT call task_output immediately — the task has just started. \
Do not redo the task's work yourself while it runs in the background. \
Never mention task_id, agent_key, or session_id to the user — they are internal handles. \
Do not read the task's output files to watch progress; wait for the result instead."
    )
}

/// 超时晋升模板（对齐 Java `formatTimeoutPromoted`）。
fn format_timeout_promoted(task_id: &str, timeout_ms: u64) -> String {
    format!(
        "status: timeout_promoted\ntask_id: {task_id}\n\
The task exceeded the {}s sync timeout but is still running in the background. \
Use task_output(task_id='{task_id}', block=false) to check status, \
wait_async_results(task_ids=...) when this task is part of a required barrier, \
or wait — completed tasks are pushed back to you automatically. \
Do NOT retry the same task.",
        timeout_ms / 1000
    )
}

/// force-sync 超时模板（对齐 Java `formatForceSyncTimeout`）。
fn format_force_sync_timeout(timeout_ms: u64) -> String {
    format!(
        "status: timeout\n\
The task exceeded the {}s sync timeout and was interrupted because force_sync \
is enabled. Background promotion is disabled for this call.",
        timeout_ms / 1000
    )
}

// ---------------------------------------------------------------------------
//  测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subagent::{DeclarationMode, SubagentEntry, SubagentFactory};
    use crate::task::InMemoryTaskRepository;
    use agentscope_core::error::Error;
    use agentscope_core::message::ToolResultState;
    use agentscope_core::model::{ChatRequest, ChatResponse};
    use crate::subagent::SubagentDeclaration;
    use crate::task::TaskStatus;
    use agentscope_core::state::AgentState;
    use std::sync::Mutex as StdMutex;

    fn tool_ctx(session_id: &str) -> ToolContext {
        let mut ctx = ToolContext::new("call-1", "test");
        ctx.session_id = session_id.into();
        ctx.user_id = Some("user-a".into());
        ctx
    }

    fn state_ctx(session_id: &str, state: &Arc<StdMutex<AgentState>>) -> ToolContext {
        let mut ctx = tool_ctx(session_id);
        ctx.state = Some(state.clone());
        ctx
    }

    /// 预置响应序列的 Mock 模型（对齐 agent.rs 测试写法）。
    struct MockModel {
        responses: StdMutex<Vec<ChatResponse>>,
    }

    impl MockModel {
        fn single(text: &str) -> Arc<dyn agentscope_core::model::Model> {
            Arc::new(Self {
                responses: StdMutex::new(vec![text_response(text)]),
            })
        }

        fn sequence(texts: &[&str]) -> Arc<dyn agentscope_core::model::Model> {
            // Vec::pop 从尾部弹出 → 反转存入使消费顺序与传入顺序一致。
            Arc::new(Self {
                responses: StdMutex::new(
                    texts.iter().rev().map(|t| text_response(t)).collect(),
                ),
            })
        }
    }

    fn text_response(text: &str) -> ChatResponse {
        ChatResponse::from_text(text)
    }

    #[async_trait::async_trait]
    impl agentscope_core::model::Model for MockModel {
        async fn call(&self, _request: ChatRequest) -> std::result::Result<ChatResponse, Error> {
            self.responses
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| Error::Model("no more mock responses".into()))
        }
    }

    /// 一直卡住的模型（超时晋升测试用）。
    struct BlockingModel;

    #[async_trait::async_trait]
    impl agentscope_core::model::Model for BlockingModel {
        async fn call(&self, _request: ChatRequest) -> std::result::Result<ChatResponse, Error> {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Err(Error::Model("should not reach".into()))
        }
    }

    fn worker_entry(model: Arc<dyn agentscope_core::model::Model>) -> SubagentEntry {
        worker_entry_named("worker", model, None)
    }

    fn worker_entry_named(
        name: &str,
        model: Arc<dyn agentscope_core::model::Model>,
        decl: Option<SubagentDeclaration>,
    ) -> SubagentEntry {
        let name = name.to_string();
        let closure_name = name.clone();
        let factory: Arc<SubagentFactory> = Arc::new(move |_rc| {
            ReActAgent::builder()
                .name(closure_name.clone())
                .sys_prompt("worker prompt")
                .model(model.clone())
                .build()
                .map(Arc::new)
                .map_err(|e| format!("build failed: {e}"))
        });
        SubagentEntry::new(name, decl, factory)
    }

    fn core_with(
        entries: Vec<SubagentEntry>,
        depth: i32,
    ) -> (Arc<AgentSpawnCore>, Arc<InMemoryTaskRepository>) {
        let manager = Arc::new(DefaultAgentManager::new());
        for entry in entries {
            manager.register(entry).unwrap();
        }
        let repo = Arc::new(InMemoryTaskRepository::new());
        (AgentSpawnCore::new(manager, repo.clone(), depth), repo)
    }

    fn spawn_input(extra: Value) -> Value {
        let mut base = json!({"agent_id": "worker"});
        if let (Value::Object(b), Value::Object(e)) = (&mut base, extra) {
            for (k, v) in e {
                b.insert(k, v);
            }
        }
        base
    }

    // ---------- 基础 spawn ----------

    #[tokio::test]
    async fn spawn_without_task_returns_accepted() {
        let (core, _repo) = core_with(
            vec![worker_entry(MockModel::single("x"))],
            0,
        );
        let tool = AgentSpawnTool::new(core.clone());
        let out = tool.call(spawn_input(json!({}))).await.unwrap();
        assert_eq!(out.state, ToolResultState::Success);
        assert!(out.content.contains("agent_key: agent:worker:"), "{}", out.content);
        assert!(out.content.contains("agent_id: worker"));
        assert!(out.content.contains("session_id: sub-"));
        assert!(out.content.contains("status: accepted"));
        assert!(!out.content.contains("reused"));
    }

    #[tokio::test]
    async fn spawn_unknown_agent_id_errors() {
        let (core, _repo) = core_with(vec![], 0);
        let tool = AgentSpawnTool::new(core);
        let out = tool.call(spawn_input(json!({}))).await.unwrap();
        assert_eq!(out.state, ToolResultState::Error);
        assert!(out.content.contains("Unknown agent_id: worker"));
    }

    #[tokio::test]
    async fn spawn_primary_only_errors() {
        let mut decl = SubagentDeclaration::new("boss", "the boss");
        decl.mode = DeclarationMode::Primary;
        let (core, _repo) = core_with(
            vec![worker_entry_named("boss", MockModel::single("x"), Some(decl))],
            0,
        );
        let tool = AgentSpawnTool::new(core);
        let out = tool
            .call(spawn_input(json!({"agent_id": "boss"})))
            .await
            .unwrap();
        assert_eq!(out.state, ToolResultState::Error);
        assert!(out.content.contains("PRIMARY-only"));
    }

    #[tokio::test]
    async fn spawn_depth_exceeded_errors() {
        let (core, _repo) = core_with(
            vec![worker_entry(MockModel::single("x"))],
            MAX_SPAWN_DEPTH,
        );
        let tool = AgentSpawnTool::new(core);
        let out = tool.call(spawn_input(json!({}))).await.unwrap();
        assert_eq!(out.state, ToolResultState::Error);
        assert!(out.content.contains("Maximum spawn depth exceeded"));
    }

    #[tokio::test]
    async fn spawn_label_conflict_errors() {
        let (core, _repo) = core_with(
            vec![worker_entry(MockModel::single("x"))],
            0,
        );
        let tool = AgentSpawnTool::new(core);
        let first = tool
            .call(spawn_input(json!({"label": "researcher"})))
            .await
            .unwrap();
        assert_eq!(first.state, ToolResultState::Success);
        let second = tool
            .call(spawn_input(json!({"label": "Researcher"})))
            .await
            .unwrap();
        assert_eq!(second.state, ToolResultState::Error);
        assert!(second.content.contains("Label already in use"));
    }

    #[tokio::test]
    async fn spawn_with_task_sync_executes() {
        let (core, _repo) = core_with(
            vec![worker_entry(MockModel::single("hello from worker"))],
            0,
        );
        let tool = AgentSpawnTool::new(core);
        let out = tool
            .call(spawn_input(json!({"task": "do something"})))
            .await
            .unwrap();
        assert_eq!(out.state, ToolResultState::Success, "{}", out.content);
        assert!(out.content.contains("status: ok"));
        assert!(out.content.contains("reply:\nhello from worker"));
    }

    #[tokio::test]
    async fn spawn_async_returns_task_id() {
        let (core, repo) = core_with(
            vec![worker_entry(MockModel::single("async done"))],
            0,
        );
        let tool = AgentSpawnTool::new(core);
        let out = tool
            .call(spawn_input(json!({"task": "run", "timeout_seconds": 0})))
            .await
            .unwrap();
        assert_eq!(out.state, ToolResultState::Success, "{}", out.content);
        assert!(out.content.contains("status: accepted"));
        assert!(out.content.contains("task_id: task_"));
        // 等任务完成后可从仓库取回。
        tokio::time::sleep(Duration::from_millis(200)).await;
        // call 路径 ctx.session_id 为空 → put 落在 "default" 槽。
        let tasks = repo.list_tasks(&RuntimeContext::new(), "", None);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].status(), TaskStatus::Completed);
    }

    #[tokio::test]
    async fn spawn_reuse_persistent_session() {
        let mut decl = SubagentDeclaration::new("keeper", "persistent");
        decl.persist_session = true;
        let (core, _repo) = core_with(
            vec![worker_entry_named("keeper", MockModel::single("x"), Some(decl))],
            0,
        );
        let tool = AgentSpawnTool::new(core);
        let first = tool
            .call(spawn_input(json!({"agent_id": "keeper", "label": "k1"})))
            .await
            .unwrap();
        assert!(first.content.contains("status: accepted"), "{}", first.content);
        let key_line = first
            .content
            .lines()
            .find(|l| l.starts_with("agent_key: "))
            .unwrap()
            .to_string();

        // 同身份（user/session/agent/label）再 spawn：同 key、复用。
        let second = tool
            .call(spawn_input(json!({"agent_id": "keeper", "label": "k1"})))
            .await
            .unwrap();
        assert!(
            second.content.contains("status: accepted (reused)"),
            "{}",
            second.content
        );
        let key_line2 = second
            .content
            .lines()
            .find(|l| l.starts_with("agent_key: "))
            .unwrap()
            .to_string();
        assert_eq!(key_line, key_line2);
    }

    #[tokio::test]
    async fn persist_session_key_is_deterministic() {
        let mut decl = SubagentDeclaration::new("keeper", "persistent");
        decl.persist_session = true;
        let (core, _repo) = core_with(
            vec![worker_entry_named("keeper", MockModel::single("x"), Some(decl.clone()))],
            0,
        );
        let tool = AgentSpawnTool::new(core);
        let a = tool
            .call(spawn_input(json!({"agent_id": "keeper"})))
            .await
            .unwrap();
        // 新 core（同声明、同身份）派生同 key。
        let (core2, _repo2) = core_with(
            vec![worker_entry_named("keeper", MockModel::single("x"), Some(decl))],
            0,
        );
        let tool2 = AgentSpawnTool::new(core2);
        let b = tool2
            .call(spawn_input(json!({"agent_id": "keeper"})))
            .await
            .unwrap();
        let ka = a.content.lines().find(|l| l.starts_with("agent_key: ")).unwrap();
        let kb = b.content.lines().find(|l| l.starts_with("agent_key: ")).unwrap();
        assert_eq!(ka, kb);
    }

    #[tokio::test]
    async fn spawn_persists_entry_into_state() {
        let (core, _repo) = core_with(
            vec![worker_entry(MockModel::single("x"))],
            0,
        );
        let tool = AgentSpawnTool::new(core);
        let state = Arc::new(StdMutex::new(AgentState::new()));
        let ctx = state_ctx("s1", &state);
        let out = tool
            .call_with_context(spawn_input(json!({"label": "lab"})), &ctx)
            .await
            .unwrap();
        assert_eq!(out.state, ToolResultState::Success);
        let st = state.lock().unwrap();
        let registry = st.tool_context.spawn_registry();
        assert_eq!(registry.len(), 1);
        let entry = registry.values().next().unwrap();
        assert_eq!(entry.agent_id, "worker");
        assert_eq!(entry.label, "lab");
        assert_eq!(entry.depth, 1);
        assert!(entry.key.starts_with("agent:worker:"));
    }

    // ---------- agent_send ----------

    #[tokio::test]
    async fn send_requires_key_or_label() {
        let (core, _repo) = core_with(vec![], 0);
        let tool = AgentSendTool::new(core);
        let out = tool.call(json!({"message": "hi"})).await.unwrap();
        assert_eq!(out.state, ToolResultState::Error);
        assert!(out.content.contains("Either agent_key or label is required"));

        let out = tool
            .call(json!({"agent_key": "agent:worker:1", "label": "l", "message": "hi"}))
            .await
            .unwrap();
        assert!(out.content.contains("not both"));
    }

    #[tokio::test]
    async fn send_requires_message() {
        let (core, _repo) = core_with(vec![], 0);
        let tool = AgentSendTool::new(core);
        let out = tool.call(json!({"label": "l"})).await.unwrap();
        assert_eq!(out.state, ToolResultState::Error);
        assert!(out.content.contains("message is required"));
    }

    #[tokio::test]
    async fn send_unknown_label_errors() {
        let (core, _repo) = core_with(vec![], 0);
        let tool = AgentSendTool::new(core);
        let out = tool.call(json!({"label": "ghost", "message": "hi"})).await.unwrap();
        assert_eq!(out.state, ToolResultState::Error);
        assert!(out.content.contains("Unknown label: ghost"));
    }

    #[tokio::test]
    async fn send_by_label_reuses_session() {
        let (core, _repo) = core_with(
            vec![worker_entry(MockModel::sequence(&["first reply", "second reply"]))],
            0,
        );
        let spawn = AgentSpawnTool::new(core.clone());
        let send = AgentSendTool::new(core);
        let spawned = spawn
            .call(spawn_input(json!({"task": "start", "label": "w"})))
            .await
            .unwrap();
        assert!(spawned.content.contains("status: ok"), "{}", spawned.content);

        let out = send
            .call(json!({"label": "w", "message": "continue"}))
            .await
            .unwrap();
        assert_eq!(out.state, ToolResultState::Success, "{}", out.content);
        assert!(out.content.contains("agent_key: agent:worker:"));
        assert!(out.content.contains("reply:\nsecond reply"));
    }

    #[tokio::test]
    async fn send_restores_from_state() {
        let (core, _repo) = core_with(
            vec![worker_entry(MockModel::sequence(&["first", "restored reply"]))],
            0,
        );
        let state = Arc::new(StdMutex::new(AgentState::new()));
        let ctx = state_ctx("s1", &state);

        // 第一次会话：spawn 并登记进 state。
        let spawn = AgentSpawnTool::new(core.clone());
        let spawned = spawn
            .call_with_context(spawn_input(json!({"task": "start", "label": "w"})), &ctx)
            .await
            .unwrap();
        let key = spawned
            .content
            .lines()
            .find(|l| l.starts_with("agent_key: "))
            .unwrap()
            .trim_start_matches("agent_key: ")
            .to_string();

        // 模拟进程重启：全新 core（同 manager 工厂可用），state 仍持有登记；
        // 新实例的模型是全新的，send 是它的第一次调用。
        let (core2, _repo2) = core_with(
            vec![worker_entry(MockModel::single("restored reply"))],
            0,
        );
        let send = AgentSendTool::new(core2);
        let out = send
            .call_with_context(
                json!({"agent_key": key, "message": "are you there"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out.state, ToolResultState::Success, "{}", out.content);
        assert!(out.content.contains("reply:\nrestored reply"));
    }

    // ---------- agent_list ----------

    #[tokio::test]
    async fn agent_list_empty_and_populated() {
        let (core, _repo) = core_with(
            vec![worker_entry(MockModel::single("x"))],
            0,
        );
        let list = AgentListTool::new(core.clone());
        let empty = list.call(json!({})).await.unwrap();
        assert_eq!(empty.content, "No active subagents.");

        let spawn = AgentSpawnTool::new(core);
        spawn
            .call(spawn_input(json!({"label": "a"})))
            .await
            .unwrap();
        spawn
            .call(spawn_input(json!({"label": "b"})))
            .await
            .unwrap();
        let out = list.call(json!({})).await.unwrap();
        assert!(out.content.contains("Active subagents (2)"));
        assert!(out.content.contains("label: a"));
        assert!(out.content.contains("label: b"));
        assert!(out.content.contains("spawn_depth: 1"));
    }

    // ---------- 超时晋升 / force-sync ----------

    #[tokio::test]
    async fn sync_timeout_promotes_to_background() {
        let (core, repo) = core_with(
            vec![worker_entry(Arc::new(BlockingModel))],
            0,
        );
        let tool = AgentSpawnTool::new(core);
        let out = tool
            .call(spawn_input(json!({"task": "slow", "timeout_seconds": 1})))
            .await
            .unwrap();
        assert_eq!(out.state, ToolResultState::Success, "{}", out.content);
        assert!(out.content.contains("status: timeout_promoted"));
        assert!(out.content.contains("task_id: task_"));
        // 晋升后的任务在仓库里可见且仍在跑（Running）。
        tokio::time::sleep(Duration::from_millis(100)).await;
        let tasks = repo.list_tasks(&RuntimeContext::new(), "", None);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].status(), TaskStatus::Running);
        tasks[0].record.lock().unwrap().cancel_requested = true;
    }

    #[tokio::test]
    async fn force_sync_timeout_interrupts_without_promotion() {
        let (core, repo) = core_with(
            vec![worker_entry(Arc::new(BlockingModel))],
            0,
        );
        let tool = AgentSpawnTool::new(core);
        let mut ctx = tool_ctx("s1");
        ctx.metadata.insert(
            CTX_FORCE_SYNC.to_string(),
            Value::Bool(true),
        );
        let out = tool
            .call_with_context(
                spawn_input(json!({"task": "slow", "timeout_seconds": 1})),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out.state, ToolResultState::Error, "{}", out.content);
        assert!(out.content.contains("status: timeout"));
        assert!(!out.content.contains("task_id"));
        // 没有晋升：仓库无任务。
        tokio::time::sleep(Duration::from_millis(100)).await;
        let tasks = repo.list_tasks(&RuntimeContext::new(), "", None);
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn force_sync_coerces_zero_timeout_to_default() {
        // force-sync 下 timeout_seconds=0 被强制为默认同步超时（不再异步）。
        let (core, repo) = core_with(
            vec![worker_entry(MockModel::single("quick"))],
            0,
        );
        let tool = AgentSpawnTool::new(core);
        let mut ctx = tool_ctx("s1");
        ctx.metadata.insert(
            CTX_FORCE_SYNC.to_string(),
            Value::Bool(true),
        );
        let out = tool
            .call_with_context(
                spawn_input(json!({"task": "run", "timeout_seconds": 0})),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out.state, ToolResultState::Success, "{}", out.content);
        assert!(out.content.contains("status: ok"));
        assert!(out.content.contains("reply:\nquick"));
        let tasks = repo.list_tasks(&RuntimeContext::new(), "", None);
        assert!(tasks.is_empty());
    }

    // ---------- 远端声明 ----------

    #[tokio::test]
    async fn remote_declaration_reports_unsupported() {
        let mut decl = SubagentDeclaration::new("remote-worker", "remote");
        decl.url = Some("http://example.com".to_string());
        let (core, _repo) = core_with(
            vec![worker_entry_named("remote-worker", MockModel::single("x"), Some(decl))],
            0,
        );
        let tool = AgentSpawnTool::new(core);
        let out = tool
            .call(spawn_input(json!({"agent_id": "remote-worker", "task": "run"})))
            .await
            .unwrap();
        assert_eq!(out.state, ToolResultState::Error);
        assert!(out.content.contains("remote subagent transport"));
    }

    // ---------- 纯函数 ----------

    #[test]
    fn deterministic_hash_is_12_hex_and_stable() {
        let h1 = deterministic_hash(&["-1:", "-1:", "6:worker", "-1:"]);
        assert_eq!(h1.len(), 12);
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
        let h2 = deterministic_hash(&["-1:", "-1:", "6:worker", "-1:"]);
        assert_eq!(h1, h2);
        let h3 = deterministic_hash(&["-1:", "-1:", "7:worker2", "-1:"]);
        assert_ne!(h1, h3);
    }

    #[test]
    fn identity_part_matches_java_semantics() {
        assert_eq!(identity_part(None), "-1:");
        assert_eq!(identity_part(Some("worker")), "6:worker");
        assert_eq!(identity_part(Some("")), "0:");
    }

    #[test]
    fn timeout_resolution_matches_java() {
        // 缺省 30s。
        assert_eq!(resolve_timeout_ms(None), 30_000);
        // 0 → 异步。
        assert_eq!(resolve_timeout_ms(Some(0)), 0);
        assert_eq!(resolve_timeout_ms(Some(-5)), 0);
        // 上限 600s。
        assert_eq!(resolve_timeout_ms(Some(10_000)), 600_000);
        assert_eq!(resolve_timeout_ms(Some(120)), 120_000);
    }

    #[tokio::test]
    async fn force_sync_timeout_override_applies() {
        let (core, _repo) = core_with(
            vec![worker_entry(Arc::new(BlockingModel))],
            0,
        );
        let tool = AgentSpawnTool::new(core);
        let mut ctx = tool_ctx("s1");
        ctx.metadata.insert(CTX_FORCE_SYNC.to_string(), Value::Bool(true));
        ctx.metadata.insert(
            CTX_FORCE_SYNC_TIMEOUT_SECONDS.to_string(),
            json!(2),
        );
        // LLM 请求 0（异步），但 force-sync + override=2 → 同步等 2s 后超时。
        let out = tool
            .call_with_context(
                spawn_input(json!({"task": "slow", "timeout_seconds": 0})),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.content.contains("status: timeout"), "{}", out.content);
    }
}
