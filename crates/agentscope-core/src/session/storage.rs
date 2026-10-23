//! 原子存储能力（对应 Java `AtomicSessionStorage`）。
//!
//! **契约要点**（照抄 Java 注释里的硬要求）：
//! - CAS 必须在共享同一命名空间的所有写者之间原子；
//! - 缺失返回 `Ok(None)`，读写失败必须返回 `Err`——**不能**把失败伪装成「不存在」；
//! - 实现必须在 ACK 之前把字节持久化；
//! - 运行时会话日志的存储**不得**能被模型的文件系统工具写入（否则模型能改写自己的历史）。
//!
//! Java 的文件后端（`LocalSessionStorage` / `SqliteSessionStorage`）在 harness 层，
//! 属于阶段 3；本模块只提供内存实现（测试与嵌入式场景）。

use std::collections::HashMap;
use std::sync::Mutex;

use super::SessionError;

/// 一次读取的结果（对应 Java `AtomicSessionStorage.Value`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageValue {
    /// 存储版本号：写入成功一次 +1，`0` 表示「对象不存在」。CAS 就是拿它做条件。
    pub version: i64,
    pub bytes: Vec<u8>,
}

impl StorageValue {
    /// 构造一个读取结果。
    pub fn new(version: i64, bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            version,
            bytes: bytes.into(),
        }
    }

    /// 按 UTF-8 解码（日志里除了 blob 都是 JSON 文本）。
    pub fn as_str(&self) -> Result<&str, SessionError> {
        std::str::from_utf8(&self.bytes)
            .map_err(|e| SessionError::log(format!("session value is not utf-8: {e}")))
    }
}

/// 窄口径的原子存储能力（对应 Java `AtomicSessionStorage`）。
///
/// 只有「按路径读」「按路径 CAS」两个必需动作；列目录是可选的只读发现能力，
/// **恢复流程从不依赖列目录**（否则上传进来的孤儿对象会污染历史）。
pub trait AtomicSessionStorage: Send + Sync {
    /// 读取一个对象；不存在返回 `Ok(None)`。
    fn read(&self, path: &str) -> Result<Option<StorageValue>, SessionError>;

    /// 只读发现前缀下的对象路径。默认不支持。
    fn list_paths(&self, prefix: &str) -> Result<Vec<String>, SessionError> {
        Err(SessionError::unsupported(format!(
            "session storage does not support discovery: {prefix}"
        )))
    }

    /// 版本匹配则写入（写入后版本 = `expected_version + 1`）。
    ///
    /// `expected_version == 0` 表示「不存在才创建」。版本不匹配返回 `Ok(false)`；
    /// 存储故障必须返回 `Err`。
    fn compare_and_set(
        &self,
        path: &str,
        expected_version: i64,
        bytes: &[u8],
    ) -> Result<bool, SessionError>;
}

/// 内存实现（对应 Java `InMemorySessionLogStore.MemoryStorage`）。
///
/// 显式的「临时后端」：用于测试与非持久化的嵌入场景。进程退出即丢失。
#[derive(Debug, Default)]
pub struct MemorySessionStorage {
    values: Mutex<HashMap<String, StorageValue>>,
}

impl MemorySessionStorage {
    /// 空存储。
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前对象数量（测试与诊断用）。
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 删除一个对象（仅内存后端提供，供测试注入「已提交数据丢失」场景）。
    pub fn remove(&self, path: &str) -> Option<StorageValue> {
        self.lock().remove(path)
    }

    /// 删除所有匹配前缀的对象，返回删除数量（测试注入用）。
    pub fn remove_matching(&self, needle: &str) -> usize {
        let mut values = self.lock();
        let before = values.len();
        values.retain(|path, _| !path.contains(needle));
        before - values.len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, StorageValue>> {
        self.values.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl AtomicSessionStorage for MemorySessionStorage {
    fn read(&self, path: &str) -> Result<Option<StorageValue>, SessionError> {
        Ok(self.lock().get(path).cloned())
    }

    fn list_paths(&self, prefix: &str) -> Result<Vec<String>, SessionError> {
        let mut paths: Vec<String> = self
            .lock()
            .keys()
            .filter(|path| path.starts_with(prefix))
            .cloned()
            .collect();
        paths.sort();
        Ok(paths)
    }

    fn compare_and_set(
        &self,
        path: &str,
        expected_version: i64,
        bytes: &[u8],
    ) -> Result<bool, SessionError> {
        let mut values = self.lock();
        let current = values.get(path).map(|value| value.version).unwrap_or(0);
        if current != expected_version {
            return Ok(false);
        }
        values.insert(path.to_string(), StorageValue::new(expected_version + 1, bytes));
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cas_requires_matching_version() {
        let storage = MemorySessionStorage::new();
        assert!(storage.read("a").unwrap().is_none());

        // 0 = 不存在才创建。
        assert!(storage.compare_and_set("a", 0, b"v1").unwrap());
        assert_eq!(storage.read("a").unwrap().unwrap().version, 1);
        // 版本已变 → 失败且不写入。
        assert!(!storage.compare_and_set("a", 0, b"v2").unwrap());
        assert_eq!(storage.read("a").unwrap().unwrap().bytes, b"v1");
        assert!(storage.compare_and_set("a", 1, b"v2").unwrap());
        assert_eq!(storage.read("a").unwrap().unwrap().bytes, b"v2");
    }

    #[test]
    fn discovery_is_optional_and_sorted() {
        let storage = MemorySessionStorage::new();
        storage.compare_and_set("p/b", 0, b"1").unwrap();
        storage.compare_and_set("p/a", 0, b"2").unwrap();
        storage.compare_and_set("q/c", 0, b"3").unwrap();
        assert_eq!(storage.list_paths("p/").unwrap(), vec!["p/a", "p/b"]);
        assert_eq!(storage.remove_matching("p/"), 2);
        assert_eq!(storage.len(), 1);
    }
}
