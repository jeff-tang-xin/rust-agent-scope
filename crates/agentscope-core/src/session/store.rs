//! 会话日志后端（对应 Java `SessionLogStore` / `InMemorySessionLogStore`）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::journal::{JournalSessionLog, SessionLog};
use super::key::SessionKey;
use super::storage::{AtomicSessionStorage, MemorySessionStorage};
use super::SessionError;
use crate::context::RuntimeContext;

/// 可扩展的会话日志后端（对应 Java `SessionLogStore`）。
///
/// **打开一个读视图不得申请写者、也不得修复历史**——否则「看一眼」就会改状态。
pub trait SessionLogStore: Send + Sync {
    /// 打开某个会话的日志视图。
    fn open(
        &self,
        key: &SessionKey,
        context: Option<&RuntimeContext>,
    ) -> Result<Arc<dyn SessionLog>, SessionError>;

    /// 发现调用者命名空间下的会话（只读发现，不是事务快照）。
    ///
    /// 实现应把结果限制在调用者的用户身份内。默认不支持。
    fn list(&self, context: Option<&RuntimeContext>) -> Result<Vec<SessionKey>, SessionError> {
        let _ = context;
        Err(SessionError::unsupported(
            "this session log store does not support discovery",
        ))
    }
}

/// 内存后端（对应 Java `InMemorySessionLogStore`）。
///
/// 显式的「临时后端」：测试与非持久化嵌入。进程退出即丢失。
#[derive(Debug, Default)]
pub struct InMemorySessionLogStore {
    stores: Mutex<HashMap<SessionKey, Arc<MemorySessionStorage>>>,
}

impl InMemorySessionLogStore {
    /// 空后端。
    pub fn new() -> Self {
        Self::default()
    }

    /// 已登记的会话数量（含尚未落盘的）。
    pub fn session_count(&self) -> usize {
        self.lock().len()
    }

    /// 直接拿到某个会话的底层存储（测试注入故障用）。
    pub fn storage(&self, key: &SessionKey) -> Option<Arc<MemorySessionStorage>> {
        self.lock().get(key).cloned()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<SessionKey, Arc<MemorySessionStorage>>> {
        self.stores.lock().unwrap_or_else(|error| error.into_inner())
    }
}

impl SessionLogStore for InMemorySessionLogStore {
    fn open(
        &self,
        key: &SessionKey,
        _context: Option<&RuntimeContext>,
    ) -> Result<Arc<dyn SessionLog>, SessionError> {
        let storage: Arc<MemorySessionStorage> = {
            let mut stores = self.lock();
            stores
                .entry(key.clone())
                .or_insert_with(|| Arc::new(MemorySessionStorage::new()))
                .clone()
        };
        let erased: Arc<dyn AtomicSessionStorage> = storage;
        Ok(Arc::new(JournalSessionLog::new(erased, key.storage_path())))
    }

    fn list(&self, context: Option<&RuntimeContext>) -> Result<Vec<SessionKey>, SessionError> {
        let user_id = context.and_then(|context| context.user_id().map(str::to_string));
        let entries: Vec<(SessionKey, Arc<MemorySessionStorage>)> = self
            .lock()
            .iter()
            .filter(|(key, _)| key.user_id == user_id)
            .map(|(key, storage)| (key.clone(), storage.clone()))
            .collect();

        let mut visible = Vec::new();
        for (key, storage) in entries {
            let root = key.storage_path();
            // 只认「真正开过日志」的会话：头部或收件箱头部存在。
            let opened = storage.read(&format!("{root}/session.json"))?.is_some()
                || storage.read(&format!("{root}/inbox/session.json"))?.is_some();
            if opened {
                visible.push(key);
            }
        }
        visible.sort_by(|left, right| {
            left.agent_id
                .cmp(&right.agent_id)
                .then_with(|| left.session_id.cmp(&right.session_id))
        });
        Ok(visible)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(user: Option<&str>, agent: &str, session: &str) -> SessionKey {
        SessionKey::new(user.map(str::to_string), agent, session).unwrap()
    }

    #[test]
    fn discovery_only_lists_opened_sessions_of_the_caller() {
        let store = InMemorySessionLogStore::new();
        let mine = key(Some("u"), "a", "s1");
        let other = key(Some("v"), "a", "s2");
        let unopened = key(Some("u"), "a", "s3");

        // 只 open 不 acquire：没有 session.json，不应被发现。
        store.open(&unopened, None).unwrap();

        let log = store.open(&mine, None).unwrap();
        log.acquire("w", std::time::Duration::from_secs(60)).unwrap();
        let other_log = store.open(&other, None).unwrap();
        other_log
            .acquire("w", std::time::Duration::from_secs(60))
            .unwrap();

        let context = RuntimeContext::new()
            .with_user_id("u")
            .with_session_id("s1");
        let listed = store.list(Some(&context)).unwrap();
        assert_eq!(listed, vec![mine.clone()]);

        // 匿名上下文看不到任何具名用户的会话。
        assert!(store.list(None).unwrap().is_empty());
        assert_eq!(store.session_count(), 3);
    }

    #[test]
    fn open_is_stable_and_returns_the_same_storage() {
        let store = InMemorySessionLogStore::new();
        let key = key(None, "a", "s");
        let first = store.open(&key, None).unwrap();
        first
            .acquire("w", std::time::Duration::from_secs(60))
            .unwrap();
        let second = store.open(&key, None).unwrap();
        assert_eq!(second.head().unwrap().epoch, 1);
        assert!(store.storage(&key).is_some());
    }
}
