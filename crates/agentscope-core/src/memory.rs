//! 对话记忆（对应 Java 版 Memory）。
//!
//! 第一阶段提供进程内实现 [`InMemoryMemory`]；分布式实现
//! （Redis / MySQL / PostgreSQL，对应 Java 版 DistributedBackend）留作扩展。

use tokio::sync::RwLock;

use crate::error::Result;
use crate::message::Msg;

/// 记忆抽象：按序追加消息、读取全部、清空。
#[async_trait::async_trait]
pub trait Memory: Send + Sync {
    async fn add(&self, msg: Msg) -> Result<()>;
    async fn get_messages(&self) -> Result<Vec<Msg>>;
    async fn clear(&self) -> Result<()>;
}

/// 进程内记忆实现（读写锁保护的消息序列）。
#[derive(Default)]
pub struct InMemoryMemory {
    messages: RwLock<Vec<Msg>>,
}

impl InMemoryMemory {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl Memory for InMemoryMemory {
    async fn add(&self, msg: Msg) -> Result<()> {
        self.messages.write().await.push(msg);
        Ok(())
    }

    async fn get_messages(&self) -> Result<Vec<Msg>> {
        Ok(self.messages.read().await.clone())
    }

    async fn clear(&self) -> Result<()> {
        self.messages.write().await.clear();
        Ok(())
    }
}

/// 把 [`Memory`] 读写全部委托给 [`crate::state::AgentState`] 的对话区
/// （对应 Java `StateBackedMemory`，`@Deprecated(forRemoval = true)`）。
///
/// 2.0 之后对话上下文的**唯一权威位置**是 `AgentState::context`；这个适配器只为
/// 「仍按 1.x 习惯通过 memory 读写历史」的旧代码而存在。新代码请直接用 `AgentState`。
pub struct StateBackedMemory {
    state: std::sync::Arc<std::sync::Mutex<crate::state::AgentState>>,
}

impl StateBackedMemory {
    /// 绑定一份共享状态。
    pub fn new(state: std::sync::Arc<std::sync::Mutex<crate::state::AgentState>>) -> Self {
        Self { state }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, crate::state::AgentState> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}

#[async_trait::async_trait]
impl Memory for StateBackedMemory {
    async fn add(&self, msg: Msg) -> Result<()> {
        self.lock().context.push(msg);
        Ok(())
    }

    async fn get_messages(&self) -> Result<Vec<Msg>> {
        Ok(self.lock().context.clone())
    }

    async fn clear(&self) -> Result<()> {
        self.lock().context.clear();
        Ok(())
    }
}

/// 只读的 [`Memory`] 视图：`get_messages()` 每次调用时**懒取**当前状态
/// （对应 Java `AgentStateMemoryView`，`@Deprecated(forRemoval = true)`）。
///
/// 用途：给「构造时还拿不到 agent」的组件（如长期记忆 hook）一个安全的历史读取口。
/// 任何写操作都返回错误——写历史请走 [`crate::state::AgentState`]。
pub struct AgentStateMemoryView {
    state: Box<dyn Fn() -> Option<crate::state::AgentState> + Send + Sync>,
}

impl AgentStateMemoryView {
    /// 用「懒取状态」的闭包构造；闭包返回 `None` 表示 agent 尚未绑定，读取得到空列表。
    pub fn new(
        state: impl Fn() -> Option<crate::state::AgentState> + Send + Sync + 'static,
    ) -> Self {
        Self {
            state: Box::new(state),
        }
    }
}

#[async_trait::async_trait]
impl Memory for AgentStateMemoryView {
    async fn add(&self, _msg: Msg) -> Result<()> {
        Err(read_only())
    }

    async fn get_messages(&self) -> Result<Vec<Msg>> {
        Ok((self.state)().map(|state| state.context).unwrap_or_default())
    }

    async fn clear(&self) -> Result<()> {
        Err(read_only())
    }
}

fn read_only() -> crate::error::Error {
    crate::error::Error::InvalidInput(
        "this memory view is read-only; mutate AgentState.context instead".into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn add_and_read() {
        let mem = InMemoryMemory::new();
        mem.add(Msg::user("user", "你好")).await.unwrap();
        mem.add(Msg::user("user", "再见")).await.unwrap();
        let msgs = mem.get_messages().await.unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].text(), "你好");

        mem.clear().await.unwrap();
        assert!(mem.get_messages().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn state_backed_memory_delegates_to_agent_state() {
        use crate::state::AgentState;
        use std::sync::{Arc, Mutex};

        let state = Arc::new(Mutex::new(AgentState::with_session_id("s")));
        let memory = StateBackedMemory::new(state.clone());
        memory.add(Msg::user("user", "你好")).await.unwrap();
        assert_eq!(memory.get_messages().await.unwrap().len(), 1);
        // 同一个对象，两条通道读到的是同一份数据。
        assert_eq!(state.lock().unwrap().context.len(), 1);

        state.lock().unwrap().context.push(Msg::user("user", "外部写入"));
        assert_eq!(memory.get_messages().await.unwrap().len(), 2);

        memory.clear().await.unwrap();
        assert!(state.lock().unwrap().context.is_empty());
    }

    #[tokio::test]
    async fn state_view_is_read_only_and_lazy() {
        use crate::state::AgentState;
        use std::sync::{Arc, Mutex};

        let state = Arc::new(Mutex::new(AgentState::with_session_id("s")));
        let shared = state.clone();
        let view = AgentStateMemoryView::new(move || Some(shared.lock().unwrap().clone()));

        assert!(view.get_messages().await.unwrap().is_empty());
        state.lock().unwrap().context.push(Msg::user("user", "迟到写入"));
        // 懒取：构造之后的变化能读到。
        assert_eq!(view.get_messages().await.unwrap().len(), 1);

        assert!(view.add(Msg::user("user", "写")).await.is_err());
        assert!(view.clear().await.is_err());

        // 未绑定时读空列表，而不是报错。
        let unbound = AgentStateMemoryView::new(|| None);
        assert!(unbound.get_messages().await.unwrap().is_empty());
    }
}
