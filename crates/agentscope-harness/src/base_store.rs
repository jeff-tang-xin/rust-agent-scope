//! 通用命名空间 KV 存储（对应 Java `filesystem.remote.store` 的
//! `BaseStore` / `StoreItem` / `InMemoryStore`）。
//!
//! Java 侧该接口放在 remote 包下，实际是 harness 多个本地包（team、
//! coordination、gateway、skill usage、task repository）的通用基座：
//! 层级命名空间 + 乐观锁 CAS + 前缀列举。Rust 侧独立于
//! [`agentscope_core::AgentStateStore`]（后者是 `(user, session, key)`
//! 语义，前者是 namespace 树语义，与 Java 一样是两个体系）。
//!
//! 值统一用 `serde_json::Value`（对应 Java `Map<String, Object>`）；
//! `NamespaceFactory` 与远端后端（OSS/COS/Mongo/SQL）留分布式阶段。

use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Mutex;

/// 单条存储项（对应 Java `StoreItem`）。
///
/// `version` 从 1 起，每次成功写入递增；`0` 表示版本未知
/// （不支持版本化的旧实现）。
#[derive(Debug, Clone, PartialEq)]
pub struct StoreItem {
    pub key: String,
    pub value: Value,
    pub version: i64,
}

impl StoreItem {
    pub fn new(key: impl Into<String>, value: Value, version: i64) -> Self {
        Self {
            key: key.into(),
            value,
            version,
        }
    }
}

/// 命名空间 KV 存储（对应 Java `BaseStore`）。
///
/// 同步 trait（Java 侧也是同步接口，响应式包装发生在 `LocalTeamClient`
/// 一层）。实现方决定并发策略；[`InMemoryBaseStore`] 用进程内锁。
pub trait BaseStore: Send + Sync {
    /// 显式声明支持版本读与跨写者 CAS（对应 Java
    /// `supportsAtomicSessionStorage`，默认 `false`）。
    fn supports_atomic_session_storage(&self) -> bool {
        false
    }

    /// 读取单条；不存在返回 `None`。
    fn get(&self, namespace: &[&str], key: &str) -> Option<StoreItem>;

    /// 写入或更新（整体替换），版本递增。
    fn put(&self, namespace: &[&str], key: &str, value: Value);

    /// 比较并交换写入。
    ///
    /// - `expected == 0`：仅当键不存在时创建（create-if-absent）；
    /// - 其余：仅当当前版本等于 `expected` 时写入（新版本 = 当前 + 1）。
    ///
    /// 返回是否写入。
    fn put_if_version(&self, namespace: &[&str], key: &str, value: Value, expected_version: i64)
        -> bool;

    /// 列举命名空间下的项（按 key 升序，`offset` 跳过、`limit` 截取）。
    fn search(&self, namespace: &[&str], limit: usize, offset: usize) -> Vec<StoreItem>;

    /// 删除单条。
    fn delete(&self, namespace: &[&str], key: &str);
}

/// 进程内线程安全实现（对应 Java `InMemoryStore`）。
///
/// 复合键 = 命名空间各组件 + `'\0'` + key（对齐 Java 的
/// `namespacePrefix + key`），天然隔离前缀同名（`"ab"` 与 `"a\0b"`）。
/// `BTreeMap` 迭代有序，`search` 无需再排序。
#[derive(Default)]
pub struct InMemoryBaseStore {
    store: Mutex<BTreeMap<String, StoreItem>>,
}

impl InMemoryBaseStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前条目数（对应 Java `size()`，测试用）。
    pub fn size(&self) -> usize {
        self.store.lock().expect("store poisoned").len()
    }

    /// 清空（对应 Java `clear()`，测试用）。
    pub fn clear(&self) {
        self.store.lock().expect("store poisoned").clear();
    }

    fn compound_key(namespace: &[&str], key: &str) -> String {
        let mut ck = String::new();
        for c in namespace {
            ck.push_str(c);
            ck.push('\0');
        }
        ck.push_str(key);
        ck
    }
}

impl BaseStore for InMemoryBaseStore {
    fn supports_atomic_session_storage(&self) -> bool {
        true
    }

    fn get(&self, namespace: &[&str], key: &str) -> Option<StoreItem> {
        let ck = Self::compound_key(namespace, key);
        self.store.lock().expect("store poisoned").get(&ck).cloned()
    }

    fn put(&self, namespace: &[&str], key: &str, value: Value) {
        let ck = Self::compound_key(namespace, key);
        let mut map = self.store.lock().expect("store poisoned");
        let next_version = map.get(&ck).map(|e| e.version + 1).unwrap_or(1);
        map.insert(
            ck,
            StoreItem {
                key: key.to_string(),
                value,
                version: next_version,
            },
        );
    }

    fn put_if_version(
        &self,
        namespace: &[&str],
        key: &str,
        value: Value,
        expected_version: i64,
    ) -> bool {
        let ck = Self::compound_key(namespace, key);
        let mut map = self.store.lock().expect("store poisoned");
        let current = map.get(&ck).map(|e| e.version).unwrap_or(0);
        if current != expected_version {
            return false;
        }
        map.insert(
            ck,
            StoreItem {
                key: key.to_string(),
                value,
                version: current + 1,
            },
        );
        true
    }

    fn search(&self, namespace: &[&str], limit: usize, offset: usize) -> Vec<StoreItem> {
        let prefix = {
            let mut p = String::new();
            for c in namespace {
                p.push_str(c);
                p.push('\0');
            }
            p
        };
        let map = self.store.lock().expect("store poisoned");
        map.iter()
            .filter(|(k, _)| k.starts_with(&prefix))
            .skip(offset)
            .take(limit)
            .map(|(_, v)| v.clone())
            .collect()
    }

    fn delete(&self, namespace: &[&str], key: &str) {
        let ck = Self::compound_key(namespace, key);
        self.store.lock().expect("store poisoned").remove(&ck);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ns() -> Vec<&'static str> {
        vec!["teams", "default", "alpha"]
    }

    #[test]
    fn put_get_version_increments() {
        let s = InMemoryBaseStore::new();
        assert!(s.get(&ns(), "a").is_none());
        s.put(&ns(), "a", json!({"v": 1}));
        let item = s.get(&ns(), "a").unwrap();
        assert_eq!(item.version, 1);
        assert_eq!(item.value["v"], 1);
        s.put(&ns(), "a", json!({"v": 2}));
        let item = s.get(&ns(), "a").unwrap();
        assert_eq!(item.version, 2);
        assert_eq!(item.value["v"], 2);
    }

    #[test]
    fn cas_create_if_absent_and_conflict() {
        let s = InMemoryBaseStore::new();
        // expected=0：不存在才创建。
        assert!(s.put_if_version(&ns(), "k", json!(1), 0));
        // 已存在（版本 1）后 expected=0 失败。
        assert!(!s.put_if_version(&ns(), "k", json!(2), 0));
        // 版本匹配才写入。
        assert!(s.put_if_version(&ns(), "k", json!(3), 1));
        assert_eq!(s.get(&ns(), "k").unwrap().version, 2);
        // 旧版本号冲突。
        assert!(!s.put_if_version(&ns(), "k", json!(4), 1));
        assert_eq!(s.get(&ns(), "k").unwrap().value, json!(3));
    }

    #[test]
    fn search_prefix_sorted_paginated() {
        let s = InMemoryBaseStore::new();
        for k in ["t2", "t1", "t3", "other"] {
            s.put(&ns(), k, json!({"k": k}));
        }
        let all = s.search(&ns(), 10, 0);
        let keys: Vec<&str> = all.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, ["other", "t1", "t2", "t3"], "按 key 升序");
        let page = s.search(&ns(), 2, 1);
        let keys: Vec<&str> = page.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, ["t1", "t2"]);
        // 命名空间隔离：不同 team 前缀不可见。
        let other_ns = vec!["teams", "default", "beta"];
        assert!(s.search(&other_ns, 10, 0).is_empty());
        // 组件含前缀串不串位（"alph" vs "alpha"）。
        let tricky = vec!["teams", "default", "alph"];
        assert!(s.search(&tricky, 10, 0).is_empty());
    }

    #[test]
    fn delete_removes_item() {
        let s = InMemoryBaseStore::new();
        s.put(&ns(), "x", json!(1));
        s.delete(&ns(), "x");
        assert!(s.get(&ns(), "x").is_none());
        // 删除后 CAS expected=0 可重建。
        assert!(s.put_if_version(&ns(), "x", json!(2), 0));
        assert_eq!(s.get(&ns(), "x").unwrap().version, 1);
    }

    #[test]
    fn supports_atomic_flag() {
        let s = InMemoryBaseStore::new();
        assert!(s.supports_atomic_session_storage());
    }
}
