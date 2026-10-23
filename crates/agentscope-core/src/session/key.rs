//! 会话身份与格式标记（对应 Java `SessionKey` / `SessionHeader`）。

use base64::Engine;
use serde::{Deserialize, Serialize};

use super::{now_millis, SessionError};

/// 会话的逻辑身份（对应 Java `record SessionKey`）。
///
/// 只表达「谁的、哪个 agent、哪个会话」；具体命名空间落在哪个后端由 `SessionLogStore` 决定
/// （Java 里是 workspace backend 的职责）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionKey {
    #[serde(default)]
    pub user_id: Option<String>,
    pub agent_id: String,
    pub session_id: String,
}

impl SessionKey {
    /// 构造并校验身份（agentId / sessionId 不能为空）。
    pub fn new(
        user_id: Option<impl Into<String>>,
        agent_id: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Result<Self, SessionError> {
        let key = Self {
            user_id: user_id.map(Into::into),
            agent_id: agent_id.into(),
            session_id: session_id.into(),
        };
        if key.agent_id.trim().is_empty() || key.session_id.trim().is_empty() {
            return Err(SessionError::invalid("empty session identity"));
        }
        Ok(key)
    }

    /// 存储路径：`agents/<agent>/sessions/<user>/<session>`，每段都做 Base64URL 编码。
    pub fn storage_path(&self) -> String {
        format!(
            "agents/{}/sessions/{}/{}",
            encode_segment(&self.agent_id),
            encode_segment(self.user_id.as_deref().unwrap_or("")),
            encode_segment(&self.session_id)
        )
    }
}

fn encode_segment(value: &str) -> String {
    format!(
        "s_{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.as_bytes())
    )
}

/// 会话格式/权威来源的不可变标记（对应 Java `record SessionHeader`）。
///
/// 「checkpoint 基线」不会假装自己包含更早的历史——`replay_coverage` 明确写清覆盖范围。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHeader {
    pub format_version: i32,
    pub authority: String,
    pub replay_coverage: String,
    pub created_at: i64,
}

/// 当前会话格式的权威来源名。
pub const AUTHORITY_EVENT_LOG: &str = "event_log";
/// 当前会话格式覆盖范围。
pub const COVERAGE_ADAPTER_CHUNKS: &str = "adapter_chunks";

impl SessionHeader {
    /// 当前格式（`formatVersion=1`，`authority=event_log`）。
    pub fn current() -> Self {
        Self {
            format_version: 1,
            authority: AUTHORITY_EVENT_LOG.to_string(),
            replay_coverage: COVERAGE_ADAPTER_CHUNKS.to_string(),
            created_at: now_millis(),
        }
    }

    /// 校验格式与权威来源；不认识的一律拒绝（fail-closed）。
    pub fn validate(&self) -> Result<(), SessionError> {
        if self.format_version != 1 || self.authority != AUTHORITY_EVENT_LOG {
            return Err(SessionError::log(format!(
                "unsupported session format/authority: v{} / {}",
                self.format_version, self.authority
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_encodes_every_segment() {
        let key = SessionKey::new(Some("u"), "a", "s").unwrap();
        assert_eq!(key.storage_path(), "agents/s_YQ/sessions/s_dQ/s_cw");

        // 空用户与无用户等价（都编码成空串的 Base64）。
        let anonymous = SessionKey::new(None::<String>, "a", "s").unwrap();
        assert_eq!(anonymous.storage_path(), "agents/s_YQ/sessions/s_/s_cw");
    }

    #[test]
    fn key_rejects_blank_identity() {
        assert!(SessionKey::new(None::<String>, " ", "s").is_err());
        assert!(SessionKey::new(None::<String>, "a", "").is_err());
    }

    #[test]
    fn header_accepts_only_current_format() {
        SessionHeader::current().validate().unwrap();

        let shadow = SessionHeader {
            format_version: 1,
            authority: "shadow".into(),
            replay_coverage: COVERAGE_ADAPTER_CHUNKS.into(),
            created_at: 0,
        };
        assert!(shadow.validate().is_err());

        let old = SessionHeader {
            format_version: 0,
            ..SessionHeader::current()
        };
        assert!(old.validate().is_err());
    }
}
