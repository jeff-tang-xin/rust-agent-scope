//! 动作观察（对应 Java `io.agentscope.core.observation`）。
//!
//! 「已确认的持久化边界，而非尽力而为的遥测回调」（Java `ActionObserver` 注释）：
//! 观察记录失败必须显式暴露，**绝不能静默降级成可重试的工具结果**。
//!
//! - [`ActionObservation`]：一次执行尝试的生命周期事实；`Returned` 不是业务
//!   成功判定，只表示「工具返回了」。
//! - [`ActionObserver`]：观察者扩展点（注册进 `RuntimeContext` 属性袋，
//!   键 [`CONTEXT_KEY`]）。
//! - [`status_of`]：工具结果 → 观察状态的映射（对齐 Java `ActionObservations.status`）。
//!
//! 事件载荷（[`ActionObservation::event_payload`]）**不拷贝工具参数、原始输出
//! 或异常消息**——只带结构化事实（对齐 Java 注释的安全语义）。
//!
//! 包装工具执行的 `ActionObservations.observe`（Java 侧挂接 `ToolCallParam` /
//! `RuntimeContext` / `SessionRecorder`，含 start/settle/fail/interrupt 四路
//! 记录与 `action_observation` 自定义事件）随工具执行路径接线批次落地，
//! 本模块当前只含数据层。

use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::Result;
use crate::message::{ContentBlock, ToolExecutionDetails, ToolResultState};
use crate::state::EvidenceBinding;

/// 观察者在 `RuntimeContext` 属性袋中的注册键（对应 Java `ActionObserver.CONTEXT_KEY`）。
pub const CONTEXT_KEY: &str = "agentscope.action.observer";

/// 观察事件的自定义事件名（对应 Java `ActionObserver.EVENT_NAME`）。
pub const EVENT_NAME: &str = "action_observation";

/// 一次执行尝试的生命周期状态（对应 Java `ActionObservation.Status`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ActionObservationStatus {
    /// 已开始（尚未结算）。
    Started,
    /// 工具已返回（**不是**业务成功判定）。
    Returned,
    /// 执行失败。
    Failed,
    /// 调用被权限系统或用户拒绝。
    Denied,
    /// 调用被挂起，等待外部提供真实结果。
    Suspended,
    /// 执行被中断。
    Interrupted,
}

/// 一次执行尝试的生命周期事实（对应 Java `ActionObservation` record）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionObservation {
    /// 本次观察的唯一 id。
    pub action_id: String,
    /// 关联的工具调用 id。
    pub tool_call_id: Option<String>,
    /// 工具名。
    pub tool_name: String,
    /// 发起调用的 agent id。
    pub agent_id: Option<String>,
    /// 用户 id。
    pub user_id: Option<String>,
    /// 会话 id。
    pub session_id: Option<String>,
    /// 开始时刻（epoch 毫秒）。
    pub started_at: i64,
    /// 结束时刻（epoch 毫秒；未结算为 `None`）。
    pub ended_at: Option<i64>,
    /// 生命周期状态。
    pub status: ActionObservationStatus,
    /// 失败时的错误类型名（对齐 Java 取 `error.getClass().getSimpleName()`）。
    pub error_type: Option<String>,
    /// 工具执行细节（由结果携带）。
    pub execution_details: Option<ToolExecutionDetails>,
    /// 任务证据绑定（来自 `AgentState.tasks_context`）。
    pub evidence_binding: Option<EvidenceBinding>,
}

impl ActionObservation {
    /// 构造一条 STARTED 观察（对齐 Java `observe` 里的起始记录）。
    pub fn started(
        action_id: impl Into<String>,
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        agent_id: Option<String>,
        user_id: Option<String>,
        session_id: Option<String>,
        evidence_binding: Option<EvidenceBinding>,
    ) -> Self {
        Self {
            action_id: action_id.into(),
            tool_call_id: Some(tool_call_id.into()),
            tool_name: tool_name.into(),
            agent_id,
            user_id,
            session_id,
            started_at: crate::interruption::now_epoch_millis() as i64,
            ended_at: None,
            status: ActionObservationStatus::Started,
            error_type: None,
            execution_details: None,
            evidence_binding,
        }
    }

    /// 结算副本：更新状态 / 错误类型 / 执行细节，结束时刻取当前时间
    /// （对应 Java `settled`）。
    pub fn settled(
        mut self,
        status: ActionObservationStatus,
        error_type: Option<String>,
        execution_details: Option<ToolExecutionDetails>,
    ) -> Self {
        self.ended_at = Some(crate::interruption::now_epoch_millis() as i64);
        self.status = status;
        self.error_type = error_type;
        self.execution_details = execution_details;
        self
    }

    /// 事件载荷（对应 Java `eventPayload`）。
    ///
    /// **不拷贝工具参数、原始输出或异常消息**；`agent_id` / `user_id` /
    /// `session_id` 与 Java 一致不进载荷（仅在记录字段上）。
    pub fn event_payload(&self) -> Map<String, Value> {
        let mut payload = Map::new();
        payload.insert("action_id".into(), Value::from(self.action_id.as_str()));
        if let Some(tool_call_id) = &self.tool_call_id {
            payload.insert("tool_call_id".into(), Value::from(tool_call_id.as_str()));
        }
        payload.insert("tool_name".into(), Value::from(self.tool_name.as_str()));
        payload.insert(
            "status".into(),
            Value::from(self.status_name().as_ref()),
        );
        payload.insert("started_at".into(), Value::from(self.started_at));
        if let Some(ended_at) = self.ended_at {
            payload.insert("ended_at".into(), Value::from(ended_at));
        }
        if let Some(error_type) = &self.error_type {
            payload.insert("error_type".into(), Value::from(error_type.as_str()));
        }
        if let Some(details) = &self.execution_details {
            if let Ok(value) = serde_json::to_value(details) {
                payload.insert("execution_details".into(), value);
            }
        }
        if let Some(binding) = &self.evidence_binding {
            if let Ok(value) = serde_json::to_value(binding) {
                payload.insert("evidence_binding".into(), value);
            }
        }
        payload
    }

    fn status_name(&self) -> Cow<'static, str> {
        match self.status {
            ActionObservationStatus::Started => Cow::Borrowed("STARTED"),
            ActionObservationStatus::Returned => Cow::Borrowed("RETURNED"),
            ActionObservationStatus::Failed => Cow::Borrowed("FAILED"),
            ActionObservationStatus::Denied => Cow::Borrowed("DENIED"),
            ActionObservationStatus::Suspended => Cow::Borrowed("SUSPENDED"),
            ActionObservationStatus::Interrupted => Cow::Borrowed("INTERRUPTED"),
        }
    }
}

/// 观察者扩展点（对应 Java `ActionObserver` 函数式接口）。
///
/// `result` 为 `None` 表示 start、取消或未捕获的执行异常（对齐 Java 注释）。
/// 实现必须保证持久化确认后才返回 `Ok`；失败返回 `Err`——调用方不得把它
/// 转换成可重试的工具结果。
#[async_trait::async_trait]
pub trait ActionObserver: Send + Sync {
    /// 记录一次观察。
    async fn record(
        &self,
        observation: &ActionObservation,
        result: Option<&ContentBlock>,
    ) -> Result<()>;
}

/// 工具结果 → 观察状态映射（对应 Java `ActionObservations.status`，检查顺序一致）。
pub fn status_of(result: &ContentBlock) -> ActionObservationStatus {
    match result {
        ContentBlock::ToolResult { state, .. } => match state {
            ToolResultState::Suspended => ActionObservationStatus::Suspended,
            ToolResultState::Denied => ActionObservationStatus::Denied,
            ToolResultState::Interrupted => ActionObservationStatus::Interrupted,
            ToolResultState::Error => ActionObservationStatus::Failed,
            _ => ActionObservationStatus::Returned,
        },
        // 非 ToolResult 块不该出现在结算路径上；防御性归为 Returned。
        _ => ActionObservationStatus::Returned,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_result(state: ToolResultState) -> ContentBlock {
        ContentBlock::ToolResult {
            id: "call-1".into(),
            name: "web_search".into(),
            output: vec![],
            state,
            execution_details: None,
        }
    }

    #[test]
    fn status_mapping_matches_java_order() {
        assert_eq!(status_of(&tool_result(ToolResultState::Suspended)), ActionObservationStatus::Suspended);
        assert_eq!(status_of(&tool_result(ToolResultState::Denied)), ActionObservationStatus::Denied);
        assert_eq!(status_of(&tool_result(ToolResultState::Interrupted)), ActionObservationStatus::Interrupted);
        assert_eq!(status_of(&tool_result(ToolResultState::Error)), ActionObservationStatus::Failed);
        assert_eq!(status_of(&tool_result(ToolResultState::Success)), ActionObservationStatus::Returned);
        assert_eq!(status_of(&tool_result(ToolResultState::Running)), ActionObservationStatus::Returned);
    }

    #[test]
    fn settled_sets_ended_at_and_fields() {
        let obs = ActionObservation::started(
            "action-1",
            "call-1",
            "web_search",
            Some("agent-1".into()),
            Some("u1".into()),
            Some("s1".into()),
            None,
        );
        assert_eq!(obs.status, ActionObservationStatus::Started);
        assert!(obs.ended_at.is_none());
        let settled = obs.settled(ActionObservationStatus::Returned, None, None);
        assert_eq!(settled.status, ActionObservationStatus::Returned);
        assert!(settled.ended_at.is_some());
        assert!(settled.ended_at.unwrap() >= settled.started_at);
    }

    #[test]
    fn event_payload_carries_facts_only() {
        let obs = ActionObservation::started(
            "action-1",
            "call-1",
            "web_search",
            Some("agent-1".into()),
            Some("u1".into()),
            Some("s1".into()),
            None,
        )
        .settled(ActionObservationStatus::Failed, Some("IllegalState".into()), None);
        let payload = obs.event_payload();
        assert_eq!(payload["action_id"], "action-1");
        assert_eq!(payload["tool_call_id"], "call-1");
        assert_eq!(payload["tool_name"], "web_search");
        assert_eq!(payload["status"], "FAILED");
        assert!(payload.get("started_at").is_some());
        assert!(payload.get("ended_at").is_some());
        assert_eq!(payload["error_type"], "IllegalState");
        // 对齐 Java：身份字段不进事件载荷。
        assert!(payload.get("agent_id").is_none());
        assert!(payload.get("user_id").is_none());
        assert!(payload.get("session_id").is_none());
    }

    #[tokio::test]
    async fn observer_trait_receives_none_result_on_start() {
        struct RecordingObserver;
        #[async_trait::async_trait]
        impl ActionObserver for RecordingObserver {
            async fn record(
                &self,
                observation: &ActionObservation,
                result: Option<&ContentBlock>,
            ) -> Result<()> {
                assert_eq!(observation.action_id, "action-1");
                // start 阶段 result 为 None（对齐 Java 语义）。
                assert!(result.is_none());
                Ok(())
            }
        }
        let obs = ActionObservation::started("action-1", "call-1", "web_search", None, None, None, None);
        RecordingObserver.record(&obs, None).await.unwrap();
    }
}
