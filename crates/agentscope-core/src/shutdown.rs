//! 优雅关闭（对应 Java `io.agentscope.core.shutdown`）。
//!
//! 关闭时序（对齐 Java `AgentScopeJvmShutdownHook` 注释）：
//! 1. 触发优雅关闭（拒绝新请求，在 reasoning/acting 检查点中断）；
//! 2. 等待全部活跃请求完成或超时（超时后强制中断并保存状态）；
//! 3. （应用层负责）关闭 HTTP 连接等外部资源。
//!
//! # 与 Java 的形态差异（有意为之，勿当 bug 修）
//!
//! 1. Java `GracefulShutdownManager` 是 JVM 级全局单例，且在构造时自动注册
//!    JVM shutdown hook；Rust 侧按反模式⑤**不做全局单例**——由应用层创建
//!    `Arc<GracefulShutdownManager>` 并显式组装信号处理（如
//!    `tokio::signal::ctrl_c` 后调用 [`GracefulShutdownManager::perform_graceful_shutdown`]），
//!    库不抢注册时机。
//! 2. Java `ShutdownSessionBinding` 已 `@Deprecated(forRemoval = true)`，不复刻。
//! 3. Java 的超时监视是每秒轮询的 `ScheduledExecutorService`；Rust 侧同样用
//!    每秒轮询任务（对齐「config 可能在关闭开始后被修改」的语义），任务持有
//!    `Weak` 引用，`Terminated` 后自行退出，不阻止 manager 释放。
//!
//! # 请求跟踪的键
//!
//! 活跃请求按**每次调用的 requestId** 跟踪，而非 agent id：同一 agent 实例可
//! 并发服务多个会话，按 agent id 归并会让一次调用的完成注销另一次在途调用
//! （对齐 Java `activeRequestsById` 的注释语义）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use tokio::sync::{Notify, watch};

use crate::error::{Error, Result};
use crate::interruption::{InterruptControl, InterruptSource};
use crate::state::AgentState;

/// 关闭期间请求 id 在 [`crate::context::RuntimeContext`] 属性袋中的键
/// （对应 Java `AgentBase.SHUTDOWN_REQUEST_ID_KEY`）。
pub const REQUEST_ID_KEY: &str = "agentscope.shutdown.request_id";

/// 全局框架关闭状态（对应 Java `ShutdownState`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownState {
    /// 正常运行，接受新请求。
    Running,
    /// 正在关闭：拒绝新请求，在检查点中断在途请求。
    ShuttingDown,
    /// 全部活跃请求已结束。
    Terminated,
}

impl ShutdownState {
    fn code(self) -> u8 {
        match self {
            ShutdownState::Running => 0,
            ShutdownState::ShuttingDown => 1,
            ShutdownState::Terminated => 2,
        }
    }

    fn from_code(code: u8) -> Self {
        match code {
            1 => ShutdownState::ShuttingDown,
            2 => ShutdownState::Terminated,
            _ => ShutdownState::Running,
        }
    }
}

/// 关闭中断时对未完成推理内容的处理策略（对应 Java `PartialReasoningPolicy`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PartialReasoningPolicy {
    /// 保存未完成的推理内容。
    #[default]
    Save,
    /// 丢弃未完成的推理内容。
    Discard,
}

/// 优雅关闭配置（对应 Java `GracefulShutdownConfig`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GracefulShutdownConfig {
    /// 等待关闭完成的最长时间；`None` 表示无限等待。
    pub shutdown_timeout: Option<Duration>,
    /// 未完成推理内容的处理策略。
    pub partial_reasoning_policy: PartialReasoningPolicy,
}

impl Default for GracefulShutdownConfig {
    fn default() -> Self {
        // 对齐 Java DEFAULT：无限等待 + 保存部分推理。
        Self {
            shutdown_timeout: None,
            partial_reasoning_policy: PartialReasoningPolicy::Save,
        }
    }
}

impl GracefulShutdownConfig {
    /// 带校验的构造（对齐 Java compact constructor：timeout 必须为正或 None）。
    pub fn new(
        shutdown_timeout: Option<Duration>,
        partial_reasoning_policy: PartialReasoningPolicy,
    ) -> Result<Self> {
        if let Some(t) = shutdown_timeout {
            if t.is_zero() {
                return Err(Error::InvalidInput(
                    "shutdown_timeout must be > 0 or None (infinite)".into(),
                ));
            }
        }
        Ok(Self {
            shutdown_timeout,
            partial_reasoning_policy,
        })
    }
}

/// 关闭期间持久化 [`AgentState`] 的策略（对应 Java `ShutdownStateSaver` 函数式接口）。
///
/// 调用时机：状态已置 `shutdown_interrupted = true` 之后。实现方**不得**在闭包内
/// 再去锁同一个状态句柄（会自死锁）。
pub type ShutdownStateSaver = Arc<dyn Fn(&AgentState) + Send + Sync>;

/// 单个活跃请求的运行时上下文（对应 Java `ActiveRequestContext`，包私有）。
///
/// `call()` 解析出会话槽位后经 [`GracefulShutdownManager::bind_request_state`]
/// 绑定状态句柄，使中断与保存精确作用于该次调用的 `(userId, sessionId)` 会话；
/// 排队中的请求无绑定状态，取消时不读写、不保存其他执行的状态。
struct ActiveRequestContext {
    saver: Option<ShutdownStateSaver>,
    control: Arc<InterruptControl>,
    shutdown_interrupt_issued: AtomicBool,
    bound_state: Mutex<Option<Arc<Mutex<AgentState>>>>,
}

impl ActiveRequestContext {
    fn bind_state(&self, state: Arc<Mutex<AgentState>>) {
        if let Ok(mut slot) = self.bound_state.lock() {
            *slot = Some(state);
        }
    }

    /// 置中断标记并保存状态（对齐 Java `saveState`：缺省 saver 不抑制中断标记）。
    fn save_state(&self) {
        let state = match self.bound_state.lock() {
            Ok(slot) => slot.clone(),
            Err(_) => return,
        };
        let Some(state) = state else { return };
        // 先取值再进闭包，避免持 state 锁期间做多余动作。
        {
            let mut guard = match state.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.shutdown_interrupted = true;
            if let Some(saver) = &self.saver {
                saver(&guard);
            }
        }
    }

    /// 发出系统中断（CAS 保证只发一次；返回 false 表示已发过）。
    fn interrupt_for_shutdown(&self) -> bool {
        if self
            .shutdown_interrupt_issued
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        self.control.trigger(InterruptSource::System, None);
        true
    }
}

/// 优雅关闭全局管理器（对应 Java `GracefulShutdownManager`）。
///
/// 由应用层持有 `Arc`；`perform_graceful_shutdown` / `await_termination` /
/// `reset_for_testing` 要求 `&Arc<Self>`（内部需要 spawn 监视任务）。
pub struct GracefulShutdownManager {
    state: AtomicU8,
    config: RwLock<GracefulShutdownConfig>,
    active_requests: Mutex<HashMap<String, Arc<ActiveRequestContext>>>,
    state_savers: Mutex<HashMap<String, ShutdownStateSaver>>,
    shutdown_started_at: Mutex<Option<Instant>>,
    timeout_fired: AtomicBool,
    timeout_signal: Notify,
    terminated_tx: watch::Sender<bool>,
}

impl Default for GracefulShutdownManager {
    fn default() -> Self {
        Self::new()
    }
}

impl GracefulShutdownManager {
    /// 创建管理器（状态 RUNNING、默认配置）。
    pub fn new() -> Self {
        let (terminated_tx, _rx) = watch::channel(false);
        Self {
            state: AtomicU8::new(ShutdownState::Running.code()),
            config: RwLock::new(GracefulShutdownConfig::default()),
            active_requests: Mutex::new(HashMap::new()),
            state_savers: Mutex::new(HashMap::new()),
            shutdown_started_at: Mutex::new(None),
            timeout_fired: AtomicBool::new(false),
            timeout_signal: Notify::new(),
            terminated_tx,
        }
    }

    /// 当前全局关闭状态。
    pub fn state(&self) -> ShutdownState {
        ShutdownState::from_code(self.state.load(Ordering::Acquire))
    }

    /// 当前配置快照。
    pub fn config(&self) -> GracefulShutdownConfig {
        self.config.read().map(|c| c.clone()).unwrap_or_default()
    }

    /// 更新配置（关闭进行中修改超时同样生效，对齐 Java 每秒读最新 config）。
    pub fn set_config(&self, config: GracefulShutdownConfig) {
        if let Ok(mut slot) = self.config.write() {
            *slot = config;
        }
    }

    /// 是否仍接受新请求。
    pub fn is_accepting_requests(&self) -> bool {
        self.state() == ShutdownState::Running
    }

    /// 活跃请求数。
    pub fn active_request_count(&self) -> usize {
        self.active_requests.lock().map(|m| m.len()).unwrap_or(0)
    }

    /// 不在接受请求的状态时返回 [`Error::AgentShuttingDown`]
    /// （对应 Java `ensureAcceptingRequests` 抛 `AgentShuttingDownException`）。
    pub fn ensure_accepting_requests(&self) -> Result<()> {
        if self.is_accepting_requests() {
            Ok(())
        } else {
            Err(Error::AgentShuttingDown)
        }
    }

    /// 为 agent 注册关闭时的状态保存器（对应 Java `bindStateSaver`）。
    pub fn bind_state_saver(&self, agent_id: impl Into<String>, saver: ShutdownStateSaver) {
        if let Ok(mut map) = self.state_savers.lock() {
            map.insert(agent_id.into(), saver);
        }
    }

    /// 移除 agent 的状态保存器（对应 Java `unbindStateSaver`；agent 关闭时必须
    /// 调用，避免临时 agent 实例在表中累积）。
    pub fn unbind_state_saver(&self, agent_id: &str) {
        if let Ok(mut map) = self.state_savers.lock() {
            map.remove(agent_id);
        }
    }

    /// 注册一次调用，返回本次调用的 requestId（对应 Java `registerRequest`）。
    ///
    /// `agent_id` 为空时返回空串（对齐 Java 非 AgentBase 直接返回 `""`）。
    /// `control` 是该次执行的中断信号；关闭中断将经由它发出。
    pub fn register_request(
        &self,
        agent_id: &str,
        control: Arc<InterruptControl>,
    ) -> String {
        if agent_id.is_empty() {
            return String::new();
        }
        let saver = self
            .state_savers
            .lock()
            .ok()
            .and_then(|map| map.get(agent_id).cloned());
        let request_id = uuid::Uuid::new_v4().simple().to_string();
        if let Ok(mut map) = self.active_requests.lock() {
            map.insert(
                request_id.clone(),
                Arc::new(ActiveRequestContext {
                    saver,
                    control,
                    shutdown_interrupt_issued: AtomicBool::new(false),
                    bound_state: Mutex::new(None),
                }),
            );
        }
        request_id
    }

    /// 绑定该请求解析出的会话状态句柄（对应 Java `bindRequestState`）。
    pub fn bind_request_state(&self, request_id: &str, state: Arc<Mutex<AgentState>>) {
        if let Some(ctx) = self.active_request(request_id) {
            ctx.bind_state(state);
        }
    }

    /// 注销请求（成功、失败、取消三路都必须调用，对齐 Java `doFinally` 语义）。
    pub fn unregister_request(&self, request_id: &str) {
        if request_id.is_empty() {
            return;
        }
        if let Ok(mut map) = self.active_requests.lock() {
            map.remove(request_id);
        }
        self.update_terminated_if_no_requests();
    }

    /// 正在关闭时对该请求发出检查点中断。
    pub fn interrupt_if_shutting_down(&self, request_id: &str) {
        if self.state() != ShutdownState::ShuttingDown {
            return;
        }
        if let Some(ctx) = self.active_request(request_id) {
            ctx.interrupt_for_shutdown();
        }
    }

    /// agent 观察到 SYSTEM 中断后调用：总是再保存一次
    /// （对齐 Java `saveOnInterruptObserved`——上次安全点之后内存可能又变了）。
    pub fn save_on_interrupt_observed(&self, request_id: &str) {
        if let Some(ctx) = self.active_request(request_id) {
            ctx.save_state();
        }
    }

    /// 检查并清除状态上的「被关闭中断」标记（对应 Java
    /// `checkAndClearShutdownInterruptedForState`）。
    pub fn check_and_clear_shutdown_interrupted_for_state(&self, state: &mut AgentState) -> bool {
        if state.shutdown_interrupted {
            state.shutdown_interrupted = false;
            true
        } else {
            false
        }
    }

    /// 触发优雅关闭。返回是否真正启动（已 `Terminated` 时返回 false；
    /// 重复调用幂等返回 true，对齐 Java `getAndUpdate` 语义）。
    pub fn perform_graceful_shutdown(self: &Arc<Self>) -> bool {
        loop {
            let current = self.state.load(Ordering::Acquire);
            if current == ShutdownState::Terminated.code() {
                return false;
            }
            if current == ShutdownState::Running.code() {
                if self
                    .state
                    .compare_exchange(
                        ShutdownState::Running.code(),
                        ShutdownState::ShuttingDown.code(),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    if let Ok(mut slot) = self.shutdown_started_at.lock() {
                        *slot = Some(Instant::now());
                    }
                    break;
                }
                // CAS 失败说明并发迁移，重读状态。
                continue;
            }
            // 已是 SHUTTING_DOWN：幂等成功。
            break;
        }
        self.start_monitor_if_needed(Arc::downgrade(self));
        true
    }

    fn start_monitor_if_needed(self: &Arc<Self>, weak: std::sync::Weak<Self>) {
        // 每次关闭启动一个监视任务；重复调用由状态机挡住（TERMINATED/RUNNING
        // 之外才 spawn，且 spawn 前已处于 SHUTTING_DOWN）。
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let Some(manager) = weak.upgrade() else { return };
                if manager.state() == ShutdownState::Terminated {
                    return;
                }
                manager.enforce_timeout_and_interrupt();
            }
        });
    }

    /// 超时检查：超过配置超时则广播超时信号并对全部活跃请求保存 + 强制中断
    /// （对应 Java `enforceTimeoutAndInterrupt`）。
    fn enforce_timeout_and_interrupt(&self) {
        if self.state() != ShutdownState::ShuttingDown {
            return;
        }
        let started = match self.shutdown_started_at.lock() {
            Ok(slot) => *slot,
            Err(_) => return,
        };
        let Some(started) = started else { return };
        let Some(timeout) = self.config().shutdown_timeout else {
            return;
        };
        if started.elapsed() < timeout {
            return;
        }
        self.timeout_fired.store(true, Ordering::Release);
        self.timeout_signal.notify_waiters();
        if let Ok(map) = self.active_requests.lock() {
            for ctx in map.values() {
                ctx.save_state();
                ctx.interrupt_for_shutdown();
            }
        }
    }

    /// 等待关闭完成（对应 Java `awaitTermination`）。返回是否到达
    /// [`ShutdownState::Terminated`]；`timeout` 为 `None` 表示无限等待。
    pub async fn await_termination(&self, timeout: Option<Duration>) -> bool {
        self.update_terminated_if_no_requests();
        let mut rx = self.terminated_tx.subscribe();
        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            if *rx.borrow_and_update() {
                return true;
            }
            match deadline {
                Some(d) => {
                    let remaining = d.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return *rx.borrow_and_update();
                    }
                    if tokio::time::timeout(remaining, rx.changed()).await.is_err() {
                        return *rx.borrow_and_update();
                    }
                }
                None => {
                    // 发送端已 drop（manager 释放）时 changed 返回 Err：视为终止。
                    let _ = rx.changed().await;
                }
            }
        }
    }

    /// 等待关闭超时信号（对应 Java `getShutdownTimeoutSignal` 的 Mono）。
    /// 工具执行方可与执行过程 race，超时后提前中止。
    pub async fn timeout_signal(&self) {
        if self.timeout_fired.load(Ordering::Acquire) {
            return;
        }
        // 标志与通知之间存在窗口：wait 返回后再查一次标志。
        let notified = self.timeout_signal.notified();
        if self.timeout_fired.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }

    /// 超时信号是否已触发。
    pub fn is_timeout_fired(&self) -> bool {
        self.timeout_fired.load(Ordering::Acquire)
    }

    /// 重置管理器状态（仅测试 / 演示用，对齐 Java `resetForTesting`）。
    pub fn reset_for_testing(self: &Arc<Self>) {
        self.state
            .store(ShutdownState::Running.code(), Ordering::Release);
        if let Ok(mut map) = self.active_requests.lock() {
            map.clear();
        }
        if let Ok(mut map) = self.state_savers.lock() {
            map.clear();
        }
        if let Ok(mut slot) = self.shutdown_started_at.lock() {
            *slot = None;
        }
        self.timeout_fired.store(false, Ordering::Release);
        self.terminated_tx.send_replace(false);
    }

    fn active_request(&self, request_id: &str) -> Option<Arc<ActiveRequestContext>> {
        // ActiveRequestContext 存于 HashMap 值中；为让锁尽早释放，这里以
        // 「取出所需信息」而非「返回引用」的方式使用。中断与保存都只碰
        // request 内部原子量 / 独立 Mutex，不回调 manager，故直接在锁内
        // 完成是安全的（无重入路径）。
        let map = self.active_requests.lock().ok()?;
        // 通过 Arc 共享：为最小化改动，ActiveRequestContext 以 Arc 存储。
        // 见 register_request 的插入逻辑。
        let ctx = map.get(request_id)?;
        Some(Arc::clone(ctx))
    }

    fn update_terminated_if_no_requests(&self) {
        if self.state() != ShutdownState::ShuttingDown {
            return;
        }
        let empty = self.active_requests.lock().map(|m| m.is_empty()).unwrap_or(false);
        if !empty {
            return;
        }
        if self
            .state
            .compare_exchange(
                ShutdownState::ShuttingDown.code(),
                ShutdownState::Terminated.code(),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            // 必须用 send_replace：send 在所有 receiver 已 drop 时返回 Err 且
            // 不更新值（本管理器的初始 receiver 在 new() 中即被释放，
            // await_termination 每次调用才 subscribe——用 send 会让终止信号
            // 静默丢失）。
            self.terminated_tx.send_replace(true);
        }
    }
}

/// 系统中间件：把优雅关闭接入 agent 生命周期（对应 Java `GracefulShutdownMiddleware`）。
///
/// Java 语义：`onReasoning` / `onActing` 在 **next 之后**（`doOnComplete`）检查
/// 「正在关闭则发检查点中断」——即 reasoning/acting 允许完整结束，输出不浪费；
/// 只有全局超时才在阶段中途强制中断（由 manager 的监视任务处理）。
///
/// Rust 侧中间件是**输入变换**形态（无「阶段后」挂点），因此本中间件当前为
/// 透传占位：阶段完成点的中断检查由 agent 循环在 reasoning/acting 收尾处调用
/// [`GracefulShutdownManager::interrupt_if_shutting_down`]（request_id 从
/// [`crate::context::RuntimeContext`] 属性袋按 [`REQUEST_ID_KEY`] 取），
/// 接线随 agent 运行控制批次落地。保留中间件形态以对齐 Java 装配位置。
pub struct GracefulShutdownMiddleware {
    manager: Arc<GracefulShutdownManager>,
}

impl GracefulShutdownMiddleware {
    /// 绑定管理器。
    pub fn new(manager: Arc<GracefulShutdownManager>) -> Self {
        Self { manager }
    }

    /// 管理器句柄（供 agent 循环取用）。
    pub fn manager(&self) -> &Arc<GracefulShutdownManager> {
        &self.manager
    }
}

impl crate::middleware::Middleware for GracefulShutdownMiddleware {
    fn name(&self) -> &str {
        "graceful_shutdown"
    }

    fn active_points(&self) -> &[crate::middleware::ExtensionPoint] {
        &[
            crate::middleware::ExtensionPoint::OnReasoning,
            crate::middleware::ExtensionPoint::OnActing,
        ]
    }

    fn on_reasoning(
        &self,
        _ctx: &crate::context::RuntimeContext,
        input: crate::middleware::ReasoningInput,
    ) -> crate::middleware::ReasoningInput {
        input
    }

    fn on_acting(
        &self,
        _ctx: &crate::context::RuntimeContext,
        input: crate::middleware::ActingInput,
    ) -> crate::middleware::ActingInput {
        input
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interruption::InterruptControl;

    fn manager() -> Arc<GracefulShutdownManager> {
        Arc::new(GracefulShutdownManager::new())
    }

    #[test]
    fn config_rejects_non_positive_timeout() {
        assert!(GracefulShutdownConfig::new(Some(Duration::ZERO), PartialReasoningPolicy::Save).is_err());
        assert!(GracefulShutdownConfig::new(None, PartialReasoningPolicy::Discard).is_ok());
        assert_eq!(GracefulShutdownConfig::default(), GracefulShutdownConfig {
            shutdown_timeout: None,
            partial_reasoning_policy: PartialReasoningPolicy::Save,
        });
    }

    #[test]
    fn register_and_unregister_tracks_requests() {
        let m = manager();
        assert!(m.is_accepting_requests());
        let control = Arc::new(InterruptControl::new());
        let rid = m.register_request("agent-1", control);
        assert!(!rid.is_empty());
        assert_eq!(m.active_request_count(), 1);
        m.unregister_request(&rid);
        assert_eq!(m.active_request_count(), 0);
        // 空 agent_id：对齐 Java 返回空串。
        assert_eq!(m.register_request("", Arc::new(InterruptControl::new())), "");
    }

    #[tokio::test]
    async fn ensure_accepting_requests_rejects_after_shutdown() {
        let m = manager();
        assert!(m.ensure_accepting_requests().is_ok());
        assert!(m.perform_graceful_shutdown());
        assert_eq!(m.state(), ShutdownState::ShuttingDown);
        assert!(!m.is_accepting_requests());
        assert!(matches!(
            m.ensure_accepting_requests(),
            Err(Error::AgentShuttingDown)
        ));
        // 幂等：再次触发仍返回 true。
        assert!(m.perform_graceful_shutdown());
    }

    #[tokio::test]
    async fn shutdown_interrupts_active_request_at_checkpoint() {
        let m = manager();
        let control = Arc::new(InterruptControl::new());
        let rid = m.register_request("agent-1", Arc::clone(&control));
        assert!(m.perform_graceful_shutdown());
        m.interrupt_if_shutting_down(&rid);
        assert!(control.is_interrupted());
        assert_eq!(control.source(), InterruptSource::System);
        // 注销最后一个请求 → TERMINATED。
        m.unregister_request(&rid);
        assert_eq!(m.state(), ShutdownState::Terminated);
        assert!(m.await_termination(Some(Duration::from_secs(1))).await);
    }

    #[tokio::test]
    async fn queued_request_without_state_is_cancelled_not_saved() {
        // 排队请求未绑定状态：save_state 是 no-op，注销后直接 TERMINATED。
        let m = manager();
        let saved = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let flag = saved.clone();
        m.bind_state_saver(
            "agent-1",
            Arc::new(move |_state| {
                flag.fetch_add(1, Ordering::SeqCst);
            }),
        );
        let control = Arc::new(InterruptControl::new());
        let rid = m.register_request("agent-1", Arc::clone(&control));
        assert!(m.perform_graceful_shutdown());
        m.save_on_interrupt_observed(&rid);
        assert_eq!(saved.load(Ordering::SeqCst), 0, "未绑定状态不应触发 saver");
        m.unregister_request(&rid);
        assert_eq!(m.state(), ShutdownState::Terminated);
    }

    #[tokio::test]
    async fn timeout_force_interrupts_and_fires_signal() {
        let m = manager();
        m.set_config(
            GracefulShutdownConfig::new(Some(Duration::from_millis(150)), PartialReasoningPolicy::Save)
                .unwrap(),
        );
        let control = Arc::new(InterruptControl::new());
        let state = Arc::new(Mutex::new(AgentState::default()));
        let rid = m.register_request("agent-1", Arc::clone(&control));
        m.bind_request_state(&rid, Arc::clone(&state));
        assert!(m.perform_graceful_shutdown());
        // 等待监视任务在超时后强制中断。
        tokio::time::timeout(Duration::from_secs(3), m.timeout_signal())
            .await
            .expect("超时信号应在配置超时后触发");
        assert!(m.is_timeout_fired());
        assert!(control.is_interrupted());
        assert!(state.lock().unwrap().shutdown_interrupted);
        m.reset_for_testing();
    }

    #[tokio::test]
    async fn await_termination_times_out_while_requests_active() {
        let m = manager();
        let control = Arc::new(InterruptControl::new());
        let _rid = m.register_request("agent-1", control);
        assert!(m.perform_graceful_shutdown());
        let reached = m.await_termination(Some(Duration::from_millis(120))).await;
        assert!(!reached, "仍有活跃请求时不应到达 TERMINATED");
        m.reset_for_testing();
    }

    #[test]
    fn check_and_clear_shutdown_interrupted_flag() {
        let m = manager();
        let mut state = AgentState::default();
        assert!(!m.check_and_clear_shutdown_interrupted_for_state(&mut state));
        state.shutdown_interrupted = true;
        assert!(m.check_and_clear_shutdown_interrupted_for_state(&mut state));
        assert!(!state.shutdown_interrupted, "检查后应清除标记");
    }

    #[test]
    fn state_saver_receives_flagged_state() {
        let m = manager();
        let seen = Arc::new(Mutex::new(None::<bool>));
        let slot = seen.clone();
        m.bind_state_saver(
            "agent-1",
            Arc::new(move |state| {
                *slot.lock().unwrap() = Some(state.shutdown_interrupted);
            }),
        );
        let control = Arc::new(InterruptControl::new());
        let rid = m.register_request("agent-1", control);
        m.bind_request_state(&rid, Arc::new(Mutex::new(AgentState::default())));
        m.save_on_interrupt_observed(&rid);
        assert_eq!(*seen.lock().unwrap(), Some(true), "saver 应看到已置位的标记");
        m.reset_for_testing();
    }
}
