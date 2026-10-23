//! 会话事件（对应 Java `SessionEvent` / `SessionEventTypes` / `SessionEventCodecRegistry`）。
//!
//! 事件是**不可变事实**：`payload_json` 在受理时就冻结成 JSON 字符串，绝不是可变的 `Msg`
//! 引用。这样提交后的历史不可能被后续内存改动污染。

use std::collections::HashMap;
use std::sync::Mutex;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::SessionError;
use crate::state::AgentState;

/// 事件信封的 schema 版本。只接受 1。
pub const SCHEMA_VERSION: i32 = 1;

/// 一条已受理的会话事件（对应 Java `record SessionEvent`）。
///
/// 字段名沿用 Java record 组件名（camelCase），保证与 Java 写出的日志互通。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEvent {
    pub schema_version: i32,
    pub event_id: String,
    /// 会话内单调递增的序号，从 1 开始；提交时要求批次连续。
    pub seq: i64,
    pub occurred_at: i64,
    /// 事件类型（Java 里叫 `type`，是 Rust 关键字，故字段名用 `kind`）。
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub execution_run_id: Option<String>,
    #[serde(default)]
    pub turn_id: Option<String>,
    /// `true` 表示重放时必须被理解，否则恢复失败（fail-closed）。
    pub required: bool,
    /// 事件负载的 JSON 文本，受理时冻结。
    pub payload_json: String,
}

impl SessionEvent {
    /// 构造并校验一条事件。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        event_id: impl Into<String>,
        seq: i64,
        occurred_at: i64,
        kind: impl Into<String>,
        execution_run_id: Option<String>,
        turn_id: Option<String>,
        required: bool,
        payload_json: impl Into<String>,
    ) -> Result<Self, SessionError> {
        let event = Self {
            schema_version: SCHEMA_VERSION,
            event_id: event_id.into(),
            seq,
            occurred_at,
            kind: kind.into(),
            execution_run_id,
            turn_id,
            required,
            payload_json: payload_json.into(),
        };
        event.validate()?;
        Ok(event)
    }

    /// 用 JSON 负载构造（`payload` 需可序列化）。
    pub fn with_payload<T: Serialize>(
        event_id: impl Into<String>,
        seq: i64,
        occurred_at: i64,
        kind: impl Into<String>,
        required: bool,
        payload: &T,
    ) -> Result<Self, SessionError> {
        Self::new(
            event_id,
            seq,
            occurred_at,
            kind,
            None,
            None,
            required,
            serde_json::to_string(payload)?,
        )
    }

    /// 校验信封不变量（对应 Java record 构造器里的检查）。
    ///
    /// 反序列化不会经过 [`SessionEvent::new`]，所以读取路径必须显式调用本方法。
    pub fn validate(&self) -> Result<(), SessionError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(SessionError::log(format!(
                "unsupported event schema version: {}",
                self.schema_version
            )));
        }
        if self.seq < 1 {
            return Err(SessionError::log("event seq must be >= 1"));
        }
        if self.event_id.is_empty() {
            return Err(SessionError::log("event id is required"));
        }
        if self.kind.is_empty() {
            return Err(SessionError::log("event type is required"));
        }
        // payload 必须是合法 JSON：日志一旦写坏，重放就没法 fail-closed 地报错。
        let _: Value = serde_json::from_str(&self.payload_json)?;
        Ok(())
    }

    /// 把负载解析成具体类型。
    pub fn payload<T: DeserializeOwned>(&self) -> Result<T, SessionError> {
        Ok(serde_json::from_str(&self.payload_json)?)
    }

    /// 负载作为 JSON 值（对应 Java `data()`）。
    pub fn data(&self) -> Result<Value, SessionError> {
        Ok(serde_json::from_str(&self.payload_json)?)
    }

    /// 复制信封并替换负载文本（提交时大负载外置到 blob 用）。
    pub(crate) fn with_payload_json(&self, payload_json: impl Into<String>) -> Self {
        let mut cloned = self.clone();
        cloned.payload_json = payload_json.into();
        cloned
    }

    /// 负载字节数（判断是否要外置成 blob 用）。
    pub(crate) fn payload_len(&self) -> usize {
        self.payload_json.len()
    }
}

/// 原生 schema v1 的内置事件类型表（对应 Java `SessionEventTypes.BUILTIN`）。
///
/// 传输名独立版本化：重放时遇到 **未知但 required** 的类型必须失败。
pub struct SessionEventTypes;

impl SessionEventTypes {
    /// 内置类型集合（顺序与 Java 一致，便于对照）。
    pub const BUILTIN: &'static [&'static str] = &[
        "run/start",
        "run/end",
        "run/stop_requested",
        "turn/start",
        // 兼容 2.0 之前写出的日志；新代码不再产出该事件。
        "turn/end",
        "turn/resumed",
        "turn/completed",
        "turn/suspended",
        "turn/failed",
        "turn/cancelled",
        "turn/interrupted",
        "turn/output",
        "step/start",
        "step/end",
        "input/received",
        "input/applied",
        "input/discarded",
        "inbox/accepted",
        "inbox/started",
        "inbox/applied",
        "inbox/opened",
        "inbox/closed",
        "inbox/rejected",
        "inbox/handled",
        "message/system",
        "message/user",
        "message/assistant",
        "context/build",
        "request/prepared",
        "model/dispatch",
        "model/end",
        "model/chunk",
        "model/retry",
        "tool/requested",
        "tool/decision",
        "action/start",
        "action/end",
        "tool/dispatch",
        "tool/chunk",
        "tool/result",
        "interaction/requested",
        "interaction/resolved",
        "compaction/start",
        "compaction/end",
        "context/replaced",
        "task/changed",
        "plan/changed",
        "permission/changed",
        "verification/result",
        "subagent/spawned",
        "subagent/completed",
        "subagent/linked",
        "recovery/applied",
        "migration/baseline",
        "presentation/hint",
        "state/checkpoint",
        "state/restored",
    ];

    /// 是否为内置类型。
    pub fn is_builtin(event_type: &str) -> bool {
        Self::BUILTIN.contains(&event_type)
    }
}

/// 扩展事件的负载校验函数。
pub type SessionEventValidate = dyn Fn(&Value) -> Result<(), SessionError> + Send + Sync;

/// 扩展事件的回放函数（把负载应用到 [`AgentState`]）。
pub type SessionEventApply =
    dyn Fn(&mut AgentState, &Value) -> Result<(), SessionError> + Send + Sync;

/// 扩展事件编解码器（对应 Java `SessionEventCodecRegistry.Codec`）。
#[derive(Clone)]
pub struct SessionEventCodec {
    validate: std::sync::Arc<SessionEventValidate>,
    apply: std::sync::Arc<SessionEventApply>,
}

impl std::fmt::Debug for SessionEventCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionEventCodec { .. }")
    }
}

impl SessionEventCodec {
    /// 用校验函数与回放函数构造 codec。
    pub fn new(
        validate: impl Fn(&Value) -> Result<(), SessionError> + Send + Sync + 'static,
        apply: impl Fn(&mut AgentState, &Value) -> Result<(), SessionError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            validate: std::sync::Arc::new(validate),
            apply: std::sync::Arc::new(apply),
        }
    }
}

/// 扩展事件的显式 schema 登记表（对应 Java `SessionEventCodecRegistry`）。
///
/// 与 Java 的差异：**不做全局单例**（反模式禁令⑤）。由调用方创建并传给恢复/投影逻辑；
/// `Default` 是空表，此时任何未知的 required 事件都会让恢复失败。
#[derive(Debug, Default)]
pub struct SessionEventCodecRegistry {
    codecs: Mutex<HashMap<String, SessionEventCodec>>,
}

impl SessionEventCodecRegistry {
    /// 空表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个扩展类型。
    ///
    /// 类型名必须带 `/` 命名空间，且不能占用内置类型名（对应 Java 的同一检查）。
    pub fn register(
        &self,
        event_type: &str,
        codec: SessionEventCodec,
    ) -> Result<(), SessionError> {
        if event_type.is_empty() || !event_type.contains('/') {
            return Err(SessionError::invalid(format!(
                "use a non-reserved namespaced event type: {event_type}"
            )));
        }
        if SessionEventTypes::is_builtin(event_type) {
            return Err(SessionError::invalid(format!(
                "event type is reserved by the built-in schema: {event_type}"
            )));
        }
        let mut codecs = self.lock();
        if codecs.contains_key(event_type) {
            return Err(SessionError::invalid(format!(
                "event codec already registered: {event_type}"
            )));
        }
        codecs.insert(event_type.to_string(), codec);
        Ok(())
    }

    /// 已注册的扩展类型数量。
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// 是否为空表。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 校验一条事件（对应 Java `validate`）。
    ///
    /// 已注册：交给扩展 codec 校验；未注册且 required 且非内置：**恢复必须失败**。
    pub fn validate(&self, event: &SessionEvent) -> Result<(), SessionError> {
        event.validate()?;
        let codec = self.lock().get(&event.kind).cloned();
        match codec {
            Some(codec) => (codec.validate)(&event.data()?),
            None if event.required && !SessionEventTypes::is_builtin(&event.kind) => Err(
                SessionError::log(format!("unknown required event: {}", event.kind)),
            ),
            None => Ok(()),
        }
    }

    /// 把扩展事件回放进状态（对应 Java `applyExtension`）。
    pub fn apply_extension(
        &self,
        state: &mut AgentState,
        event: &SessionEvent,
    ) -> Result<(), SessionError> {
        let codec = self.lock().get(&event.kind).cloned();
        match codec {
            Some(codec) => (codec.apply)(state, &event.data()?),
            None => Ok(()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, SessionEventCodec>> {
        self.codecs.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(seq: i64, kind: &str, required: bool) -> SessionEvent {
        SessionEvent::new("e", seq, 1, kind, None, None, required, "{}").unwrap()
    }

    #[test]
    fn envelope_rejects_bad_schema_and_seq() {
        assert!(SessionEvent::new("e", 0, 1, "run/start", None, None, true, "{}").is_err());
        assert!(SessionEvent::new("", 1, 1, "run/start", None, None, true, "{}").is_err());
        assert!(SessionEvent::new("e", 1, 1, "run/start", None, None, true, "not json").is_err());

        // 反序列化绕过构造器 → 必须靠 validate 兜住。
        let broken: SessionEvent = serde_json::from_str(
            r#"{"schemaVersion":2,"eventId":"e","seq":1,"occurredAt":0,"type":"run/start","required":true,"payloadJson":"{}"}"#,
        )
        .unwrap();
        assert!(broken.validate().is_err());
    }

    #[test]
    fn envelope_uses_java_wire_names() {
        let event = event(7, "run/start", true);
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["schemaVersion"], 1);
        assert_eq!(json["eventId"], "e");
        assert_eq!(json["type"], "run/start");
        assert_eq!(json["payloadJson"], "{}");
        // 可选字段缺失时能读回 None。
        let parsed: SessionEvent = serde_json::from_str(
            r#"{"schemaVersion":1,"eventId":"e","seq":3,"occurredAt":9,"type":"run/end","required":false,"payloadJson":"{\"a\":1}"}"#,
        )
        .unwrap();
        assert!(parsed.execution_run_id.is_none());
        assert_eq!(parsed.data().unwrap()["a"], 1);
    }

    #[test]
    fn builtin_table_covers_key_lifecycle_events() {
        assert!(SessionEventTypes::is_builtin("run/start"));
        assert!(SessionEventTypes::is_builtin("state/checkpoint"));
        assert!(!SessionEventTypes::is_builtin("plugin/unknown"));
        // Java `SessionEventTypes.BUILTIN`（`Set.of` 字面量）共 57 项。
        assert_eq!(SessionEventTypes::BUILTIN.len(), 57);
        // 传输名不重复。
        let unique: std::collections::HashSet<&str> =
            SessionEventTypes::BUILTIN.iter().copied().collect();
        assert_eq!(unique.len(), 57);
    }

    #[test]
    fn unknown_required_event_blocks_validation_but_optional_passes() {
        let registry = SessionEventCodecRegistry::new();
        assert!(registry.validate(&event(1, "run/start", true)).is_ok());
        assert!(registry.validate(&event(1, "plugin/unknown", false)).is_ok());
        let err = registry.validate(&event(1, "plugin/unknown", true)).unwrap_err();
        assert!(matches!(err, SessionError::Log(_)));
    }

    #[test]
    fn extension_codec_validates_and_applies() {
        let registry = SessionEventCodecRegistry::new();
        registry
            .register(
                "plugin/counter",
                SessionEventCodec::new(
                    |value| {
                        if value.get("delta").and_then(Value::as_i64).is_some() {
                            Ok(())
                        } else {
                            Err(SessionError::log("delta is required"))
                        }
                    },
                    |state, value| {
                        state.cur_iter += value["delta"].as_i64().unwrap_or(0) as i32;
                        Ok(())
                    },
                ),
            )
            .unwrap();

        let bad = SessionEvent::new("e", 1, 1, "plugin/counter", None, None, true, "{}").unwrap();
        assert!(registry.validate(&bad).is_err());

        let good =
            SessionEvent::new("e", 1, 1, "plugin/counter", None, None, true, r#"{"delta":3}"#)
                .unwrap();
        registry.validate(&good).unwrap();
        let mut state = AgentState::new();
        registry.apply_extension(&mut state, &good).unwrap();
        assert_eq!(state.cur_iter, 3);
    }

    #[test]
    fn registry_rejects_reserved_and_duplicate_types() {
        let registry = SessionEventCodecRegistry::new();
        let codec = || SessionEventCodec::new(|_| Ok(()), |_, _| Ok(()));
        assert!(registry.register("noNamespace", codec()).is_err());
        assert!(registry.register("run/start", codec()).is_err());
        registry.register("plugin/a", codec()).unwrap();
        assert!(registry.register("plugin/a", codec()).is_err());
        assert_eq!(registry.len(), 1);
        assert!(!registry.is_empty());
    }
}
