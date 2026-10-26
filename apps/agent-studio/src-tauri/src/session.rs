//! 会话门面与 turn 生命周期（≈ Java `AgentSession` + dataplane `SessionTurnRunner`
//! 的 Rust 惯用翻译）。
//!
//! Java 侧的设计：
//! - `AgentSession` 是每会话门面：submit / steer / interrupt / respond + 视图；
//! - `SessionTurnRunner.runTurnAsync`：per-session turn 互斥（turnMutex）→
//!   订阅事件流 → 逐事件投影给前端（SSE）→ turn 结束清理；
//! - `Task{commandId, turnId, runId, status, error}` 描述一次 turn。
//!
//! Rust 翻译：
//! - `SessionHandle`：per-session turn 锁（`tokio::sync::Mutex`，try_lock 拒绝
//!   并发 turn）+ 运行中 turn 的 AbortHandle（interrupt 语义）；
//! - `AgentHandle`：agent + 成员表 + 会话表（懒建，对齐 AgentSession 的创建时机）；
//! - `TurnInfo`：turn 描述 DTO（对齐 Task record 的最小面）；
//! - 事件投影：AgentEvent 直接 serde 序列化进信封 emit（Rust 侧事件已自带
//!   类型标签，无需 Java 的 Map 投影中间层）。

use std::collections::HashMap;
use std::sync::Arc;

use agentscope_harness::HarnessAgent;
use serde::Serialize;
use tokio::sync::Mutex as AsyncMutex;

/// 一次 turn 的描述（对齐 Java `AgentSession.Task` 的最小面）。
#[derive(Debug, Clone, Serialize)]
pub struct TurnInfo {
    pub turn_id: String,
    pub session_id: String,
    /// running | finished（创建时恒为 running；终态由 turn-finished 事件携带）。
    pub status: &'static str,
}

/// per-session 运行态：turn 互斥 + 可中断句柄。
///
/// steer 不在这里排队：运行中插入直接进 `ReActAgent::push_steer` 的步边界
/// 注入通道（下一 step 生效）；调用结束后未被消费的残留由 agent
/// `take_steers` 取走（send_message 合并 / turn_chain 结算续轮）。
pub struct SessionHandle {
    /// turn 互斥（同一会话串行 turn；try_lock 失败即拒绝，前端提示等待）。
    turn_lock: Arc<AsyncMutex<()>>,
    /// 运行中 turn 的中止句柄（interrupt 语义；turn 结束时清空）。
    running: AsyncMutex<Option<tokio::task::AbortHandle>>,
}

impl SessionHandle {
    /// 尝试开始一个 turn：成功返回锁 guard（所有权 move 进 turn task，
    /// 持锁到结束）；失败说明该会话已有运行中 turn（对齐 turnMutex 串行语义）。
    pub fn try_begin_turn(
        &self,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, tokio::sync::TryLockError> {
        Arc::clone(&self.turn_lock).try_lock_owned()
    }

    /// 是否有运行中 turn（steer 入队前的守卫依据）。
    pub async fn is_running(&self) -> bool {
        self.running.lock().await.is_some()
    }

    /// 中断当前 turn（对齐 `AgentSession.interrupt`）。返回是否有运行中 turn。
    pub async fn interrupt(&self) -> bool {
        let mut guard = self.running.lock().await;
        if let Some(handle) = guard.take() {
            handle.abort();
            true
        } else {
            false
        }
    }

    /// 记录运行中 turn 的中止句柄。
    pub async fn track(&self, handle: tokio::task::AbortHandle) {
        *self.running.lock().await = Some(handle);
    }

    /// turn 结束后清空中止句柄（幂等）。
    pub async fn untrack(&self) {
        *self.running.lock().await = None;
    }
}

/// 一个已装配 agent 的运行时句柄（≈ Java HarnessAgent + 会话表）。
pub struct AgentHandle {
    pub agent: HarnessAgent,
    sessions: AsyncMutex<HashMap<String, Arc<SessionHandle>>>,
}

impl AgentHandle {
    pub fn new(agent: HarnessAgent) -> Self {
        Self {
            agent,
            sessions: AsyncMutex::new(HashMap::new()),
        }
    }

    /// 取（或懒建）会话句柄（对齐 AgentSession 的按需创建）。
    pub async fn session(&self, session_id: &str) -> Arc<SessionHandle> {
        let mut sessions = self.sessions.lock().await;
        sessions
            .entry(session_id.to_string())
            .or_insert_with(|| {
                Arc::new(SessionHandle {
                    turn_lock: Arc::new(AsyncMutex::new(())),
                    running: AsyncMutex::new(None),
                })
            })
            .clone()
    }

    /// 丢弃会话运行态（clear_session 时调用；AgentState 由 state store 管理，
    /// 这里只清内存中的 turn 状态）。
    pub async fn drop_session(&self, session_id: &str) {
        self.sessions.lock().await.remove(session_id);
    }
}
