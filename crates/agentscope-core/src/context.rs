//! 每次调用的运行时上下文（对应 Java `io.agentscope.core.RuntimeContext`）。
//!
//! Java 2.0 把「一次调用」的运行时信息收进 `RuntimeContext`：replyId、会话标识、
//! 用户标识与调用级属性袋。中间件的每个扩展点、工具执行上下文都从这里取信息，
//! 而不依赖任何全局状态。
//!
//! 设计约束（反模式⑤）：**不做全局单例**——每次 call 由调用方创建（或由
//! [`crate::agent::ReActAgent`] 内部按默认构造），以 `&RuntimeContext` 传入执行循环；
//! 属性袋跨线程共享，用 `Arc<RwLock<_>>` 只保护真正共享的部分。

use std::sync::{Arc, RwLock};

use serde_json::{Map, Value};
use uuid::Uuid;

/// 一次 agent 调用的运行时上下文。
///
/// `Clone` 是**浅共享**：属性袋是 `Arc`，clone 出的副本读写同一份属性。
#[derive(Clone, Debug)]
pub struct RuntimeContext {
    reply_id: String,
    session_id: Option<String>,
    user_id: Option<String>,
    attributes: Arc<RwLock<Map<String, Value>>>,
}

impl Default for RuntimeContext {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeContext {
    /// 创建上下文并生成本次调用的 replyId。
    pub fn new() -> Self {
        Self {
            reply_id: Uuid::new_v4().simple().to_string(),
            session_id: None,
            user_id: None,
            attributes: Arc::new(RwLock::new(Map::new())),
        }
    }

    /// 指定 replyId（会话恢复、重放等场景需要沿用旧 id 时使用）。
    pub fn with_reply_id(mut self, reply_id: impl Into<String>) -> Self {
        self.reply_id = reply_id.into();
        self
    }

    /// 指定会话标识（session 批次接入后由会话槽位提供）。
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// 指定用户标识。
    pub fn with_user_id(mut self, user_id: impl Into<String>) -> Self {
        self.user_id = Some(user_id.into());
        self
    }

    /// 本次调用的 replyId（贯穿该次调用的所有事件与工具调用）。
    pub fn reply_id(&self) -> &str {
        &self.reply_id
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn user_id(&self) -> Option<&str> {
        self.user_id.as_deref()
    }

    /// 写入调用级属性（对应 Java `RuntimeContext#put`）。
    pub fn put(&self, key: impl Into<String>, value: Value) {
        if let Ok(mut map) = self.attributes.write() {
            map.insert(key.into(), value);
        }
    }

    /// 读取调用级属性（对应 Java `RuntimeContext#get`）。
    pub fn get(&self, key: &str) -> Option<Value> {
        self.attributes.read().ok().and_then(|m| m.get(key).cloned())
    }

    /// 属性袋快照。
    pub fn attributes(&self) -> Map<String, Value> {
        self.attributes
            .read()
            .map(|m| m.clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_id_is_generated_and_preserved_on_clone() {
        let ctx = RuntimeContext::new();
        assert!(!ctx.reply_id().is_empty());
        let cloned = ctx.clone();
        assert_eq!(ctx.reply_id(), cloned.reply_id());
    }

    #[test]
    fn attributes_are_shared_across_clones() {
        let ctx = RuntimeContext::new();
        let cloned = ctx.clone();
        cloned.put("agentscope.mode", Value::from("test"));
        assert_eq!(
            ctx.get("agentscope.mode"),
            Some(Value::from("test")),
            "属性袋应跨 clone 共享"
        );
    }

    #[test]
    fn session_and_user_ids_are_optional() {
        let ctx = RuntimeContext::new();
        assert!(ctx.session_id().is_none());
        let ctx = ctx.with_session_id("s1").with_user_id("u1");
        assert_eq!(ctx.session_id(), Some("s1"));
        assert_eq!(ctx.user_id(), Some("u1"));
    }
}
