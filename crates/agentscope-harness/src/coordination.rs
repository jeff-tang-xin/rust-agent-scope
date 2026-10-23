//! 周期性后台工作的节流协调（对应 Java harness `agent.coordination` 包的
//! `PeriodicGate` / `LocalPeriodicGate` 与 `IsolationScope`）。
//!
//! [`PeriodicGate`] 用「最小间隔」节流跨调用（以及可选跨节点）的周期性后台任务：
//! `try_claim` 成功表示本次可以执行，失败表示最近已有调用者占用了该槽位。
//! `StoreBackedPeriodicGate`（分布式 CAS 版）依赖 `BaseStore`，留到分布式阶段。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
// UNIX_EPOCH 仍被 now_epoch_millis 使用

use agentscope_core::RuntimeContext;

/// 状态隔离维度（对应 Java `IsolationScope`）：决定节流窗口与存储命名空间
/// 按 userId / sessionId / agent / 全局哪一维共享。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IsolationScope {
    /// 按会话隔离。
    Session,
    /// 按用户共享（默认；userId 缺失时 Java 侧回退 SESSION，节流键此处
    /// 退化为空串槽位，与 Java `blankToEmpty` 行为一致）。
    User,
    /// 同一 agent 实例内共享。
    Agent,
    /// 同一 workspace/store 实例内全局共享。
    Global,
}

impl IsolationScope {
    /// 节流键的「调用身份」部分（对齐 Java `timerKeyFor`）：User → userId、
    /// Session → sessionId（空/缺失归一为 `""` 共享一个槽位）、
    /// Agent/Global → 恒 `""`（全部调用者共享一个槽位）。
    pub fn timer_key(&self, rc: &RuntimeContext) -> String {
        match self {
            IsolationScope::User => blank_to_empty(rc.user_id()),
            IsolationScope::Session => blank_to_empty(rc.session_id()),
            IsolationScope::Agent | IsolationScope::Global => String::new(),
        }
    }
}

/// 把 `None`/空白标识归一为 `""`，让缺失 id 的调用共享一个槽位
/// （对齐 Java `MemoryFlushMiddleware.blankToEmpty`）。
pub fn blank_to_empty(s: Option<&str>) -> String {
    match s {
        Some(s) if !s.trim().is_empty() => s.to_string(),
        _ => String::new(),
    }
}

/// 当前时刻的 epoch 毫秒。
pub fn now_epoch_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 周期性后台任务的节流门（对应 Java `PeriodicGate`）。
pub trait PeriodicGate: Send + Sync {
    /// 尝试占用名为 `name` 的周期槽位：距上次成功占用不足 `min_gap` 则失败。
    fn try_claim(&self, name: &str, min_gap: Duration) -> bool;
}

/// 进程级共享的最近占用时间表（对齐 Java `LocalPeriodicGate` 的 static map：
/// 节流窗口跨 agent 重建存活，实例级 map 会在每次 build 后重置）。
static SHARED_LAST_CLAIM_AT: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// JVM 本地 [`PeriodicGate`]：进程级共享表 + 原子占用。单进程部署的默认实现。
///
/// Rust 侧锁内「读-判-写」天然原子（等价 Java 的 `AtomicReference` CAS）。
#[derive(Debug, Default, Clone, Copy)]
pub struct LocalPeriodicGate;

impl LocalPeriodicGate {
    pub fn new() -> Self {
        Self
    }

    /// 清空全部槽位——仅供单元测试。
    pub fn clear_for_tests() {
        SHARED_LAST_CLAIM_AT.lock().unwrap().clear();
    }

    /// 预置某槽位的最近占用时刻——仅供单元测试。
    pub fn seed_last_claim_at_for_tests(name: &str, at: Instant) {
        SHARED_LAST_CLAIM_AT
            .lock()
            .unwrap()
            .insert(name.to_string(), at);
    }

    /// 某槽位是否存在——仅供单元测试。
    pub fn has_claim_for_tests(name: &str) -> bool {
        SHARED_LAST_CLAIM_AT.lock().unwrap().contains_key(name)
    }
}

impl PeriodicGate for LocalPeriodicGate {
    fn try_claim(&self, name: &str, min_gap: Duration) -> bool {
        if name.trim().is_empty() {
            return false;
        }
        let now = Instant::now();
        let mut map = SHARED_LAST_CLAIM_AT.lock().unwrap();
        // 从未占用 → 视为「很久以前」（Instant 无 epoch 零点，用 checked_sub
        // 近似；进程启动 10 年内必然成功）。
        let long_ago = now.checked_sub(Duration::from_secs(86_400 * 3650));
        let last = map.get(name).copied().or(long_ago);
        let eligible = match last {
            Some(last) => now.duration_since(last) >= min_gap,
            None => true,
        };
        if !eligible {
            return false;
        }
        map.insert(name.to_string(), now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 注意：SHARED_LAST_CLAIM_AT 是进程级静态表且测试并行执行，
    /// 各测试必须用互不冲突的唯一 key，禁止依赖 clear_for_tests 的时序。
    #[test]
    fn gate_first_claim_passes_then_throttles() {
        let gate = LocalPeriodicGate::new();
        let key = "test-gate:first-claim";
        assert!(gate.try_claim(key, Duration::from_secs(60)));
        // 间隔内再次占用失败。
        assert!(!gate.try_claim(key, Duration::from_secs(60)));
        // 预置一个 2 分钟前的占用 → 可再次通过。
        LocalPeriodicGate::seed_last_claim_at_for_tests(
            key,
            Instant::now() - Duration::from_secs(120),
        );
        assert!(gate.try_claim(key, Duration::from_secs(60)));
    }

    #[test]
    fn gate_keys_are_independent() {
        let gate = LocalPeriodicGate::new();
        let (ka, kb) = ("test-gate:k-a", "test-gate:k-b");
        assert!(gate.try_claim(ka, Duration::from_secs(60)));
        assert!(gate.try_claim(kb, Duration::from_secs(60)));
        assert!(!gate.try_claim(ka, Duration::from_secs(60)));
    }

    #[test]
    fn gate_rejects_blank_name() {
        let gate = LocalPeriodicGate::new();
        assert!(!gate.try_claim("", Duration::from_secs(60)));
        assert!(!gate.try_claim("   ", Duration::from_secs(60)));
    }

    #[test]
    fn timer_key_isolation_dimensions() {
        let rc = RuntimeContext::new()
            .with_session_id("s-1")
            .with_user_id("u-1");
        assert_eq!(IsolationScope::User.timer_key(&rc), "u-1");
        assert_eq!(IsolationScope::Session.timer_key(&rc), "s-1");
        assert_eq!(IsolationScope::Agent.timer_key(&rc), "");
        assert_eq!(IsolationScope::Global.timer_key(&rc), "");

        // 缺失/空白 id 归一为空串（共享槽位）。
        let anon = RuntimeContext::new();
        assert_eq!(IsolationScope::User.timer_key(&anon), "");
        let blank = RuntimeContext::new().with_user_id("   ");
        assert_eq!(IsolationScope::User.timer_key(&blank), "");
    }
}
