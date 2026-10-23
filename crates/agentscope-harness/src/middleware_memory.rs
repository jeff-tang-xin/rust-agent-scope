//! 记忆中间件（对应 Java `MemoryFlushMiddleware` / `MemoryMaintenanceMiddleware`）：
//! 挂在「调用完成」扩展点上，fire-and-forget 地驱动 flush（抽取）与
//! consolidation（合并）两条 LLM 管线。
//!
//! - flush：每次调用后按 [`FlushTrigger`] 节流，per-conversation 队列合并
//!   （运行中再来一次调用 → 合并进在途任务，不重复起 LLM）。
//! - maintenance：按 [`IsolationScope`] 维度经 [`PeriodicGate`] 节流，
//!   依次做过期台账归档 + consolidation。

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use agentscope_core::middleware::Middleware;
use agentscope_core::model::Model;
use agentscope_core::state::AgentState;
use agentscope_core::RuntimeContext;

use crate::coordination::{blank_to_empty, now_epoch_millis, IsolationScope, PeriodicGate};
use crate::memory::{
    MemoryBackgroundTasks, MemoryConfig, MemoryConsolidator, MemoryFlushManager, FlushTrigger,
};
use crate::workspace::WorkspaceManager;

/// flush 队列的会话键：中间件实例 + 隔离维度标识（对齐 Java `ConversationKey`；
/// Rust 侧 middleware 拿不到 agent 引用，用实例 id 区分）。
type ConversationKey = (u64, String, String);

struct FlushQueue {
    /// 等待合并进在途任务的会话（键存在即有待办）。
    pending: Vec<ConversationKey>,
    /// 是否已有在途 flush 任务。
    running: bool,
}

static FLUSH_QUEUES: LazyLock<Mutex<HashMap<ConversationKey, FlushQueue>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 全局中间件实例 id 分配器（队列键的去重成分）。
static INSTANCE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// 每次 agent 调用完成后，把对话窗口交给 [`MemoryFlushManager`] 抽取长期记忆
/// （对应 Java `MemoryFlushMiddleware`）。
pub struct MemoryFlushMiddleware {
    instance_id: u64,
    workspace: Arc<WorkspaceManager>,
    model: Arc<dyn Model>,
    flush_prompt: String,
    flush_trigger: FlushTrigger,
    isolation_scope: IsolationScope,
    periodic_gate: Arc<dyn PeriodicGate>,
}

impl MemoryFlushMiddleware {
    pub fn new(workspace: Arc<WorkspaceManager>, model: Arc<dyn Model>, config: &MemoryConfig) -> Self {
        Self::with_gate(
            workspace,
            model,
            config,
            Arc::new(crate::coordination::LocalPeriodicGate::new()),
        )
    }

    pub fn with_gate(
        workspace: Arc<WorkspaceManager>,
        model: Arc<dyn Model>,
        config: &MemoryConfig,
        periodic_gate: Arc<dyn PeriodicGate>,
    ) -> Self {
        Self {
            instance_id: INSTANCE_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
            workspace,
            model,
            flush_prompt: config
                .flush_prompt
                .clone()
                .unwrap_or_default(),
            flush_trigger: config.flush_trigger.clone(),
            isolation_scope: config.isolation_scope,
            periodic_gate,
        }
    }

    /// 节流判定（对齐 Java `shouldFlushNow`）：Always 恒真；Never 恒假；
    /// Throttled 经 [`PeriodicGate`] 按隔离维度占用槽位。
    fn should_flush_now(&self, rc: &RuntimeContext) -> bool {
        match &self.flush_trigger {
            FlushTrigger::Always => true,
            FlushTrigger::Never => false,
            FlushTrigger::Throttled(min_gap) => {
                let timer_key = format!(
                    "memory-flush:{}:{}",
                    self.instance_id,
                    self.isolation_scope.timer_key(rc)
                );
                self.periodic_gate.try_claim(&timer_key, *min_gap)
            }
        }
    }

    /// 调度一次 flush：在途任务存在则合并（去重），否则 spawn 新任务。
    fn schedule_flush(self: &Arc<Self>, rc: &RuntimeContext, state: Arc<Mutex<AgentState>>) {
        let rc = rc.clone();
        let key = self.conversation_key(&rc);
        let should_start = {
            let mut queues = FLUSH_QUEUES.lock().unwrap();
            let queue = queues.entry(key.clone()).or_insert_with(|| FlushQueue {
                pending: Vec::new(),
                running: false,
            });
            if queue.running {
                if !queue.pending.contains(&key) {
                    queue.pending.push(key.clone());
                }
                false
            } else {
                queue.running = true;
                true
            }
        };
        if !should_start {
            return;
        }
        let middleware = Arc::clone(self);
        MemoryBackgroundTasks::begin();
        tokio::spawn(async move {
            middleware.run_flush_task(key, rc, state).await;
        });
    }

    fn conversation_key(&self, rc: &RuntimeContext) -> ConversationKey {
        (
            self.instance_id,
            blank_to_empty(rc.user_id()),
            blank_to_empty(rc.session_id()),
        )
    }

    /// 在途 flush 任务：跑一轮抽取，然后吸收排队期间合并进来的后续调用。
    async fn run_flush_task(
        self: Arc<Self>,
        key: ConversationKey,
        rc: RuntimeContext,
        state: Arc<Mutex<AgentState>>,
    ) {
        loop {
            // 快照对话窗口（锁内取值即释放，不跨 await 持锁）。
            let messages = {
                let state_guard = state.lock().unwrap_or_else(|e| e.into_inner());
                state_guard.context.clone()
            };
            if let Err(error) = self.flush_manager().flush_memories(&rc, &messages).await {
                // flush 是尽力而为的后台收尾：失败只记录，不影响主调用。
                eprintln!("[memory-flush] flush failed: {error}");
            }
            // 吸收排队期间的后续调用；没有则退出并标记空闲。
            let next = {
                let mut queues = FLUSH_QUEUES.lock().unwrap();
                let queue = queues.get_mut(&key);
                match queue {
                    Some(queue) if !queue.pending.is_empty() => {
                        queue.pending.clear();
                        true
                    }
                    Some(queue) => {
                        queue.running = false;
                        false
                    }
                    None => false,
                }
            };
            if !next {
                break;
            }
        }
        MemoryBackgroundTasks::end();
    }

    fn flush_manager(&self) -> MemoryFlushManager {
        if self.flush_prompt.is_empty() {
            MemoryFlushManager::new(Arc::clone(&self.workspace), Arc::clone(&self.model))
        } else {
            MemoryFlushManager::with_prompt(
                Arc::clone(&self.workspace),
                Arc::clone(&self.model),
                self.flush_prompt.clone(),
            )
        }
    }
}

impl Middleware for MemoryFlushMiddleware {
    fn name(&self) -> &str {
        "memory-flush"
    }

    fn on_agent_complete(&self, ctx: &RuntimeContext, state: &Arc<Mutex<AgentState>>) {
        if !self.should_flush_now(ctx) {
            return;
        }
        // on_agent_complete 的 &self 生命周期短于 spawn 的任务：借 Arc<Self>
        // 的共享语义需要自持——middleware 以 Arc<dyn Middleware> 存于链中，
        // 这里通过全局队列 + 实例 id 保证任务引用安全（clone 出 Arc<Self>
        // 的等价物：用 unsafe 不可取，改为把所需字段全部 move 进任务）。
        //
        // 简化实现：任务持有字段克隆（Arc 字段廉价），实例方法改为自由函数。
        let middleware = Self {
            instance_id: self.instance_id,
            workspace: Arc::clone(&self.workspace),
            model: Arc::clone(&self.model),
            flush_prompt: self.flush_prompt.clone(),
            flush_trigger: self.flush_trigger.clone(),
            isolation_scope: self.isolation_scope,
            periodic_gate: Arc::clone(&self.periodic_gate),
        };
        let rc = ctx.clone();
        let middleware = Arc::new(middleware);
        middleware.schedule_flush(&rc, Arc::clone(state));
    }
}

/// 周期性记忆维护（对应 Java `MemoryMaintenanceMiddleware`）：过期每日台账
/// 归档 + consolidation 合并，经 [`PeriodicGate`] 按 [`IsolationScope`] 节流。
pub struct MemoryMaintenanceMiddleware {
    instance_id: u64,
    workspace: Arc<WorkspaceManager>,
    model: Arc<dyn Model>,
    consolidation_prompt: String,
    consolidation_max_tokens: usize,
    consolidation_min_gap: Duration,
    daily_file_retention_days: u32,
    isolation_scope: IsolationScope,
    periodic_gate: Arc<dyn PeriodicGate>,
}

impl MemoryMaintenanceMiddleware {
    pub fn new(workspace: Arc<WorkspaceManager>, model: Arc<dyn Model>, config: &MemoryConfig) -> Self {
        Self::with_gate(
            workspace,
            model,
            config,
            Arc::new(crate::coordination::LocalPeriodicGate::new()),
        )
    }

    pub fn with_gate(
        workspace: Arc<WorkspaceManager>,
        model: Arc<dyn Model>,
        config: &MemoryConfig,
        periodic_gate: Arc<dyn PeriodicGate>,
    ) -> Self {
        Self {
            instance_id: INSTANCE_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
            workspace,
            model,
            consolidation_prompt: config.consolidation_prompt.clone().unwrap_or_default(),
            consolidation_max_tokens: config.consolidation_max_tokens,
            consolidation_min_gap: config.consolidation_min_gap,
            daily_file_retention_days: config.daily_file_retention_days,
            isolation_scope: config.isolation_scope,
            periodic_gate,
        }
    }

    fn maintenance_gate_key(&self, rc: &RuntimeContext) -> String {
        format!(
            "memory-maintenance:{}:{}",
            self.instance_id,
            self.isolation_scope.timer_key(rc)
        )
    }

}

impl Middleware for MemoryMaintenanceMiddleware {
    fn name(&self) -> &str {
        "memory-maintenance"
    }

    fn on_agent_complete(&self, ctx: &RuntimeContext, _state: &Arc<Mutex<AgentState>>) {
        if !self
            .periodic_gate
            .try_claim(&self.maintenance_gate_key(ctx), self.consolidation_min_gap)
        {
            return;
        }
        let workspace = Arc::clone(&self.workspace);
        let model = Arc::clone(&self.model);
        let consolidation_prompt = self.consolidation_prompt.clone();
        let consolidation_max_tokens = self.consolidation_max_tokens;
        let retention_days = self.daily_file_retention_days;
        let rc = ctx.clone();
        MemoryBackgroundTasks::begin();
        tokio::spawn(async move {
            if let Err(error) = expire_daily_files(&rc, &workspace, retention_days).await {
                eprintln!("[memory-maintenance] expire failed: {error}");
            }
            let consolidator = if consolidation_prompt.is_empty() {
                MemoryConsolidator::new(workspace, model)
            } else {
                MemoryConsolidator::with_options(workspace, model, consolidation_prompt, consolidation_max_tokens)
            };
            if let Err(error) = consolidator.consolidate(&rc).await {
                eprintln!("[memory-maintenance] consolidate failed: {error}");
            }
            MemoryBackgroundTasks::end();
        });
    }
}

/// 把超过保留期的每日台账移入 `memory/archive/`（对齐 Java
/// `MemoryMaintenanceMiddleware.expireDailyFiles`；移动失败只记录不中断）。
async fn expire_daily_files(
    rc: &RuntimeContext,
    workspace: &Arc<WorkspaceManager>,
    retention_days: u32,
) -> agentscope_core::Result<()> {
    use crate::workspace::constants::MEMORY_DIR;

    let Some(fs) = workspace.filesystem() else {
        return Ok(());
    };
    let cutoff_millis = now_epoch_millis() - (retention_days as i64) * 86_400_000;
    let matches = match fs.glob(rc, "*.md", MEMORY_DIR).await {
        Ok(matches) => matches,
        Err(_) => return Ok(()),
    };
    for fi in matches {
        if fi.is_directory {
            continue;
        }
        let name = fi
            .path
            .trim_start_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("");
        // 只处理 YYYY-MM-DD.md 形态的每日台账。
        if !is_daily_ledger_name(name) {
            continue;
        }
        let Some(modified_at) = fi.modified_at else {
            continue;
        };
        if modified_at >= cutoff_millis {
            continue;
        }
        let rel = fi.path.trim_start_matches('/').to_string();
        let content = workspace.read_managed_workspace_file_utf8(rc, &rel).await;
        if content.trim().is_empty() {
            continue;
        }
        let archive_rel = format!("{MEMORY_DIR}/archive/{name}");
        let _ = fs.move_path(rc, &rel, &archive_rel).await;
    }
    Ok(())
}

/// `YYYY-MM-DD.md` 形态判定。
fn is_daily_ledger_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".md") else {
        return false;
    };
    let bytes = stem.as_bytes();
    if bytes.len() != 10 {
        return false;
    }
    bytes.iter().enumerate().all(|(i, b)| match i {
        4 | 7 => *b == b'-',
        _ => b.is_ascii_digit(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::InMemoryFilesystem;
    use crate::memory::FlushTrigger;
    use agentscope_core::model::{ChatRequest, ChatResponse, Model};
    use agentscope_core::Result;
    use std::sync::Arc;

    struct NoopModel;

    #[async_trait::async_trait]
    impl Model for NoopModel {
        async fn call(&self, _request: ChatRequest) -> Result<ChatResponse> {
            Ok(ChatResponse::from_text("NO_REPLY"))
        }
    }

    fn test_config(trigger: FlushTrigger) -> MemoryConfig {
        MemoryConfig {
            flush_trigger: trigger,
            ..MemoryConfig::default()
        }
    }

    fn memory_workspace() -> Arc<WorkspaceManager> {
        let fs: Arc<dyn crate::workspace::Filesystem> = Arc::new(InMemoryFilesystem::new());
        Arc::new(WorkspaceManager::with_filesystem("test-ws", fs))
    }

    // ---------- should_flush_now 三模式 ----------

    #[test]
    fn flush_trigger_modes() {
        // middleware 实例 id 全局递增 → gate key 天然唯一，无需清表。
        let ws = memory_workspace();
        let model: Arc<dyn Model> = Arc::new(NoopModel);

        let always = MemoryFlushMiddleware::new(
            Arc::clone(&ws),
            Arc::clone(&model),
            &test_config(FlushTrigger::Always),
        );
        let rc = RuntimeContext::new();
        assert!(always.should_flush_now(&rc));

        let never = MemoryFlushMiddleware::new(
            Arc::clone(&ws),
            Arc::clone(&model),
            &test_config(FlushTrigger::Never),
        );
        assert!(!never.should_flush_now(&rc));

        let throttled = MemoryFlushMiddleware::new(
            Arc::clone(&ws),
            Arc::clone(&model),
            &test_config(FlushTrigger::Throttled(Duration::from_secs(60))),
        );
        // 首次立即放行，间隔内拒绝（节流是「首过后续」语义）。
        assert!(throttled.should_flush_now(&rc));
        assert!(!throttled.should_flush_now(&rc));
    }

    #[test]
    fn flush_trigger_isolation_dimensions() {
        let ws = memory_workspace();
        let model: Arc<dyn Model> = Arc::new(NoopModel);
        let mut config = test_config(FlushTrigger::Throttled(Duration::from_secs(60)));
        config.isolation_scope = IsolationScope::Session;
        let middleware = MemoryFlushMiddleware::new(Arc::clone(&ws), Arc::clone(&model), &config);

        let rc_a = RuntimeContext::new().with_session_id("s-a");
        let rc_b = RuntimeContext::new().with_session_id("s-b");
        assert!(middleware.should_flush_now(&rc_a));
        // 不同会话互不影响节流窗口。
        assert!(middleware.should_flush_now(&rc_b));
        assert!(!middleware.should_flush_now(&rc_a));
    }

    // ---------- 队列合并 ----------

    #[tokio::test]
    async fn flush_queue_merges_pending_into_running() {
        // 直接操作队列结构：running=true 时再调度 → 进 pending 而不起新任务。
        let key: ConversationKey = (42, "u".into(), "s".into());
        {
            let mut queues = FLUSH_QUEUES.lock().unwrap();
            queues.insert(
                key.clone(),
                FlushQueue {
                    pending: Vec::new(),
                    running: true,
                },
            );
        }
        // 模拟 schedule_flush 的合并分支（不真正 spawn：running=true → 只入队）。
        let merged = {
            let mut queues = FLUSH_QUEUES.lock().unwrap();
            let queue = queues.get_mut(&key).unwrap();
            if queue.running {
                if !queue.pending.contains(&key) {
                    queue.pending.push(key.clone());
                }
                false
            } else {
                queue.running = true;
                true
            }
        };
        assert!(!merged);
        let queues = FLUSH_QUEUES.lock().unwrap();
        assert_eq!(queues.get(&key).unwrap().pending.len(), 1);
        // 清理，避免污染其他测试。
        drop(queues);
        FLUSH_QUEUES.lock().unwrap().remove(&key);
    }

    // ---------- 过期归档 ----------

    #[tokio::test]
    async fn expire_skips_files_without_mtime() {
        // InMemoryFilesystem 的 glob 无 mtime（None）→ 一律跳过，不归档。
        let ws = memory_workspace();
        let rc = RuntimeContext::new();
        ws.write_utf8_workspace_relative(&rc, "memory/2020-01-01.md", "- old")
            .await
            .unwrap();
        expire_daily_files(&rc, &ws, 90).await.unwrap();
        // 文件仍在原位。
        assert!(!ws
            .read_managed_workspace_file_utf8(&rc, "memory/2020-01-01.md")
            .await
            .trim()
            .is_empty());
    }

    #[test]
    fn daily_ledger_name_shape() {
        assert!(is_daily_ledger_name("2026-01-01.md"));
        assert!(is_daily_ledger_name("1999-12-31.md"));
        assert!(!is_daily_ledger_name("MEMORY.md"));
        assert!(!is_daily_ledger_name("notes.md"));
        assert!(!is_daily_ledger_name("2026-1-1.md"));
        assert!(!is_daily_ledger_name("20260101.md"));
        assert!(!is_daily_ledger_name("2026-01-01.txt"));
        assert!(!is_daily_ledger_name(".consolidation_state"));
    }

    // ---------- middleware 元数据 ----------

    #[test]
    fn middleware_names_and_order() {
        let ws = memory_workspace();
        let model: Arc<dyn Model> = Arc::new(NoopModel);
        let flush = MemoryFlushMiddleware::new(Arc::clone(&ws), model.clone(), &MemoryConfig::default());
        let maintenance =
            MemoryMaintenanceMiddleware::new(Arc::clone(&ws), model, &MemoryConfig::default());
        assert_eq!(flush.name(), "memory-flush");
        assert_eq!(maintenance.name(), "memory-maintenance");
        assert_eq!(flush.order(), 1);
    }
}
