//! 动作观察的持久化（对应 Java `harness.agent.observation` 包 2 文件）。
//!
//! 把不可变的 start/settlement 记录与可压缩的对话分开持久化：
//! **有 start 而无 settlement 是一次未决尝试，绝不是重放它的授权**
//! （对齐 Java `StateStoreActionObserver` 类注释）。
//!
//! - [`StoredActionObservation`]：运行时持有的证据；原始结果与 session state
//!   同一访问/保留边界（对应 Java `implements State`）。
//! - [`StateStoreActionObserver`]：[`AgentStateStore`] 后端的观察者，键约定
//!   `action_{id}_started` / `action_{id}_settled`；重复记录必须逐字节一致
//!   （JSON 相等），冲突即报错——同一动作绝不允许两种结算。

use agentscope_core::message::ContentBlock;
use agentscope_core::observation::{ActionObservation, ActionObservationStatus, ActionObserver};
use agentscope_core::state::{AgentStateStore, UNVERSIONED};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 运行时持有的证据（对应 Java `StoredActionObservation` record）。
///
/// `result` 为 `None` 表示 start / 取消 / 未捕获异常（对齐 Java `null`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredActionObservation {
    pub observation: ActionObservation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ContentBlock>,
}

impl StoredActionObservation {
    pub fn new(observation: ActionObservation, result: Option<ContentBlock>) -> Self {
        Self { observation, result }
    }
}

/// [`AgentStateStore`] 后端的观察者（对应 Java `StateStoreActionObserver`）。
///
/// 持久化不可变的 start/settlement 记录；同一键的重复写入必须与既有值
/// 完全一致，否则报「同一动作的冲突结算」。
pub struct StateStoreActionObserver {
    store: std::sync::Arc<dyn AgentStateStore>,
}

impl std::fmt::Debug for StateStoreActionObserver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateStoreActionObserver").finish_non_exhaustive()
    }
}

impl StateStoreActionObserver {
    pub fn new(store: std::sync::Arc<dyn AgentStateStore>) -> Self {
        Self { store }
    }

    /// 存储键约定（对应 Java `StateStoreActionObserver.key`）。
    pub fn key(action_id: &str, started: bool) -> String {
        format!("action_{}{}", action_id, if started { "_started" } else { "_settled" })
    }

    /// 读取一条已持久化的观察。
    pub fn load(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        action_id: &str,
        started: bool,
    ) -> Result<Option<StoredActionObservation>, String> {
        let value = self
            .store
            .get_raw(user_id, session_id, &Self::key(action_id, started))
            .map_err(|e| e.to_string())?;
        match value {
            Some(v) => serde_json::from_value(v)
                .map(Some)
                .map_err(|e| format!("failed to decode stored action observation: {e}")),
            None => Ok(None),
        }
    }

    /// 记录一条观察（对应 Java `record` 的同步主体）。
    ///
    /// - 会话身份必须存在（无 session 的观察无法归属，直接报错）；
    /// - 既有值存在时要求 JSON 完全一致（幂等重放放行，冲突拒绝）；
    /// - 支持版本化的后端走 CAS（expected=0），冲突后读回校验一致性。
    pub fn record_observation(
        &self,
        observation: &ActionObservation,
        result: Option<&ContentBlock>,
    ) -> Result<(), String> {
        let session_id = observation.session_id.as_deref().unwrap_or("");
        if session_id.trim().is_empty() {
            return Err("Action observation requires a session identity".into());
        }
        let started = observation.status == ActionObservationStatus::Started;
        let key = Self::key(&observation.action_id, started);
        let value = StoredActionObservation::new(observation.clone(), result.cloned());
        let encoded = serde_json::to_value(&value).map_err(|e| e.to_string())?;

        let user_id = observation.user_id.as_deref();
        if let Some(previous) = self.load(user_id, session_id, &observation.action_id, started)? {
            return require_identical(&previous, &value);
        }
        if self.store.supports_versioning() {
            let version = self
                .store
                .save_if_version_raw(user_id, session_id, &key, encoded, 0)
                .map_err(|e| e.to_string())?;
            if version == UNVERSIONED {
                // CAS 冲突：读回既有值，一致则视为幂等重放，否则冲突。
                let committed = self
                    .load(user_id, session_id, &observation.action_id, started)?
                    .ok_or_else(|| "Observation commit was not acknowledged".to_string())?;
                require_identical(&committed, &value)?;
            }
            return Ok(());
        }
        self.store
            .save_raw(user_id, session_id, &key, encoded)
            .map_err(|e| e.to_string())
    }
}

fn require_identical(previous: &StoredActionObservation, next: &StoredActionObservation) -> Result<(), String> {
    let a = serde_json::to_value(previous).map_err(|e| e.to_string())?;
    let b = serde_json::to_value(next).map_err(|e| e.to_string())?;
    if a != b {
        return Err("Conflicting settlement for the same action".into());
    }
    Ok(())
}

#[async_trait::async_trait]
impl ActionObserver for StateStoreActionObserver {
    async fn record(
        &self,
        observation: &ActionObservation,
        result: Option<&ContentBlock>,
    ) -> agentscope_core::error::Result<()> {
        self.record_observation(observation, result)
            .map_err(agentscope_core::error::Error::other)
    }
}

/// 把观察序列化成事件载荷里的 `Value`（供 verification 等下游复用）。
pub fn observation_to_value(value: &StoredActionObservation) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::message::{ContentBlock, ToolResultState};
    use agentscope_core::observation::ActionObservation;
    use agentscope_core::state::InMemoryAgentStateStore;
    use std::sync::Arc;

    fn observer() -> StateStoreActionObserver {
        StateStoreActionObserver::new(Arc::new(InMemoryAgentStateStore::new()))
    }

    fn started_obs() -> ActionObservation {
        ActionObservation::started(
            "action-1",
            "call-1",
            "shell",
            Some("agent-a"),
            Some("u"),
            "s1",
            None,
        )
    }

    fn result_block() -> ContentBlock {
        ContentBlock::ToolResult {
            id: "call-1".into(),
            name: "shell".into(),
            output: vec![],
            state: ToolResultState::Success,
            execution_details: None,
        }
    }

    #[test]
    fn key_convention_matches_java() {
        assert_eq!(StateStoreActionObserver::key("a1", true), "action_a1_started");
        assert_eq!(StateStoreActionObserver::key("a1", false), "action_a1_settled");
    }

    #[test]
    fn record_requires_session_identity() {
        let obs = StateStoreActionObserver::new(Arc::new(InMemoryAgentStateStore::new()));
        let mut observation = started_obs();
        observation.session_id = None;
        let err = obs
            .record_observation(&observation, None)
            .unwrap_err();
        assert!(err.contains("session identity"));
    }

    #[test]
    fn start_and_settlement_are_stored_separately() {
        let obs = observer();
        obs.record_observation(&started_obs(), None).unwrap();

        let settled = started_obs().settled(
            ActionObservationStatus::Returned,
            None,
            None,
        );
        obs.record_observation(&settled, Some(&result_block())).unwrap();

        let started_loaded = obs.load(Some("u"), "s1", "action-1", true).unwrap().unwrap();
        assert_eq!(started_loaded.observation.status, ActionObservationStatus::Started);
        assert!(started_loaded.result.is_none());

        let settled_loaded = obs.load(Some("u"), "s1", "action-1", false).unwrap().unwrap();
        assert_eq!(settled_loaded.observation.status, ActionObservationStatus::Returned);
        assert!(settled_loaded.result.is_some());
    }

    #[test]
    fn identical_replay_is_idempotent_but_conflict_is_rejected() {
        let obs = observer();
        let settled = started_obs().settled(ActionObservationStatus::Returned, None, None);
        obs.record_observation(&settled, Some(&result_block())).unwrap();

        // 完全一致的重放：放行。
        obs.record_observation(&settled, Some(&result_block())).unwrap();

        // 同一动作不同结算：拒绝。
        let conflicting = started_obs().settled(ActionObservationStatus::Failed, Some("Boom".into()), None);
        let err = obs
            .record_observation(&conflicting, Some(&result_block()))
            .unwrap_err();
        assert!(err.contains("Conflicting settlement"));
    }

    #[test]
    fn observation_roundtrips_through_serde() {
        let settled = started_obs().settled(ActionObservationStatus::Returned, None, None);
        let stored = StoredActionObservation::new(settled, Some(result_block()));
        let value = serde_json::to_value(&stored).unwrap();
        let back: StoredActionObservation = serde_json::from_value(value).unwrap();
        assert_eq!(back.observation.action_id, "action-1");
        assert!(back.result.is_some());
    }
}
