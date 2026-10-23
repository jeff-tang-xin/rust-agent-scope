//! 模型凭证（对应 Java `io.agentscope.core.credential`）。
//!
//! 承载某个 provider 需要的连接信息（API key、base URL 等），可 JSON 往返
//! 用于存储兼容。Java 侧是 `CredentialBase` 抽象类 + 三个子类；Rust 侧三个
//! 凭证字段同构（api_key + 可选 base_url），按反模式②用**枚举**而非 trait 体系。
//!
//! Java `getChatModelClass()` 在 core 侧本就抛 `UnsupportedOperationException`
//! （core 未内置这些 provider，官方指引是用 OpenAI 兼容模型指向对应 base_url），
//! Rust 侧同样不提供模型绑定。`listModels` 默认不支持（对齐 Java 默认抛异常）。
//!
//! `id` 是自动生成的 32 位 hex，随 JSON 往返但不参与鉴权（对齐 Java 注释）。

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// DeepSeek API 默认地址（对应 Java `DeepSeekCredential.DEFAULT_BASE_URL`）。
pub const DEEPSEEK_DEFAULT_BASE_URL: &str = "https://api.deepseek.com";
/// Kimi (Moonshot AI) API 默认地址（对应 Java `KimiCredential.DEFAULT_BASE_URL`）。
pub const KIMI_DEFAULT_BASE_URL: &str = "https://api.moonshot.cn/v1";

/// 类型判别值（对应 Java 各凭证类的 `TYPE` 常量）。
pub mod credential_type {
    pub const DEEPSEEK: &str = "deepseek_credential";
    pub const KIMI: &str = "kimi_credential";
    pub const XAI: &str = "xai_credential";
}

fn new_credential_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn default_deepseek_base_url() -> String {
    DEEPSEEK_DEFAULT_BASE_URL.to_string()
}

fn default_kimi_base_url() -> String {
    KIMI_DEFAULT_BASE_URL.to_string()
}

/// 某个 provider 下一个候选对话模型的描述（对应 Java `ModelCard` record）。
///
/// 最小占位形态：能力开关与参数 schema 待模型发现基础设施成熟后补充
/// （对齐 Java 注释）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCard {
    #[serde(rename = "model_name")]
    pub model_name: String,
    #[serde(rename = "display_name", default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(rename = "context_size", default, skip_serializing_if = "Option::is_none")]
    pub context_size: Option<u64>,
}

/// 模型 provider 凭证。
///
/// 序列化形态对齐 Java：`type` 判别 + `id` / `api_key` / `base_url` 字段；
/// 反序列化时缺省 `id` 自动生成、缺省 `base_url` 落到 provider 默认值。
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Credential {
    /// DeepSeek API 凭证。
    #[serde(rename = "deepseek_credential")]
    DeepSeek {
        #[serde(default = "new_credential_id")]
        id: String,
        api_key: String,
        #[serde(default = "default_deepseek_base_url")]
        base_url: String,
    },
    /// Kimi (Moonshot AI) API 凭证。
    #[serde(rename = "kimi_credential")]
    Kimi {
        #[serde(default = "new_credential_id")]
        id: String,
        api_key: String,
        #[serde(default = "default_kimi_base_url")]
        base_url: String,
    },
    /// xAI API 凭证（无 base_url 字段，对齐 Java `XAICredential`）。
    #[serde(rename = "xai_credential")]
    Xai {
        #[serde(default = "new_credential_id")]
        id: String,
        api_key: String,
    },
}

impl Credential {
    /// 凭证实例 id（32 位 hex，随 JSON 往返）。
    pub fn id(&self) -> &str {
        match self {
            Credential::DeepSeek { id, .. } | Credential::Kimi { id, .. } | Credential::Xai { id, .. } => id,
        }
    }

    /// API key。
    pub fn api_key(&self) -> &str {
        match self {
            Credential::DeepSeek { api_key, .. }
            | Credential::Kimi { api_key, .. }
            | Credential::Xai { api_key, .. } => api_key,
        }
    }

    /// base URL（xAI 无此字段，返回 `None`）。
    pub fn base_url(&self) -> Option<&str> {
        match self {
            Credential::DeepSeek { base_url, .. } | Credential::Kimi { base_url, .. } => {
                Some(base_url)
            }
            Credential::Xai { .. } => None,
        }
    }

    /// 枚举候选模型（对齐 Java 默认实现：尚不支持，显式报错）。
    pub fn list_models(&self) -> Result<Vec<ModelCard>> {
        let kind = match self {
            Credential::DeepSeek { .. } => credential_type::DEEPSEEK,
            Credential::Kimi { .. } => credential_type::KIMI,
            Credential::Xai { .. } => credential_type::XAI,
        };
        Err(Error::Other(format!(
            "{kind} does not yet support listModels; provide a manual model catalog or wait \
             for ChatModelBase.listModels support."
        )))
    }
}

impl std::fmt::Debug for Credential {
    /// 脱敏输出：api_key 一律 `***`（对齐 Java `toString`）。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Credential::DeepSeek { id, base_url, .. } => f
                .debug_struct("DeepSeekCredential")
                .field("id", id)
                .field("base_url", base_url)
                .field("api_key", &"***")
                .finish(),
            Credential::Kimi { id, base_url, .. } => f
                .debug_struct("KimiCredential")
                .field("id", id)
                .field("base_url", base_url)
                .field("api_key", &"***")
                .finish(),
            Credential::Xai { id, .. } => f
                .debug_struct("XAICredential")
                .field("id", id)
                .field("api_key", &"***")
                .finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_with_type_tag_and_field_names() {
        let cred = Credential::DeepSeek {
            id: "abc123".into(),
            api_key: "sk-test".into(),
            base_url: DEEPSEEK_DEFAULT_BASE_URL.into(),
        };
        let json = serde_json::to_value(&cred).unwrap();
        assert_eq!(json["type"], "deepseek_credential");
        assert_eq!(json["id"], "abc123");
        assert_eq!(json["api_key"], "sk-test");
        assert_eq!(json["base_url"], DEEPSEEK_DEFAULT_BASE_URL);
        let back: Credential = serde_json::from_value(json).unwrap();
        assert_eq!(back, cred);
    }

    #[test]
    fn missing_id_is_generated_and_missing_base_url_falls_back() {
        let json = serde_json::json!({
            "type": "kimi_credential",
            "api_key": "sk-kimi"
        });
        let cred: Credential = serde_json::from_value(json).unwrap();
        assert_eq!(cred.id().len(), 32, "缺省 id 应自动生成 32 位 hex");
        assert_eq!(cred.base_url(), Some(KIMI_DEFAULT_BASE_URL));
    }

    #[test]
    fn xai_has_no_base_url_field() {
        let cred = Credential::Xai {
            id: "x1".into(),
            api_key: "sk-xai".into(),
        };
        let json = serde_json::to_value(&cred).unwrap();
        assert_eq!(json["type"], "xai_credential");
        assert!(json.get("base_url").is_none(), "xAI 不应序列化 base_url 字段");
        assert_eq!(cred.base_url(), None);
    }

    #[test]
    fn debug_never_leaks_api_key() {
        let creds = [
            Credential::DeepSeek {
                id: "d".into(),
                api_key: "SECRET-DEEPSEEK".into(),
                base_url: String::new(),
            },
            Credential::Kimi {
                id: "k".into(),
                api_key: "SECRET-KIMI".into(),
                base_url: String::new(),
            },
            Credential::Xai {
                id: "x".into(),
                api_key: "SECRET-XAI".into(),
            },
        ];
        for cred in creds {
            let rendered = format!("{cred:?}");
            assert!(!rendered.contains("SECRET"), "Debug 输出泄漏了 api_key: {rendered}");
            assert!(rendered.contains("***"));
        }
    }

    #[test]
    fn list_models_is_unsupported_by_default() {
        let cred = Credential::DeepSeek {
            id: "d".into(),
            api_key: "k".into(),
            base_url: String::new(),
        };
        assert!(cred.list_models().is_err());
    }

    #[test]
    fn model_card_serializes_with_java_field_names() {
        let card = ModelCard {
            model_name: "deepseek-chat".into(),
            display_name: Some("DeepSeek Chat".into()),
            context_size: Some(65536),
        };
        let json = serde_json::to_value(&card).unwrap();
        assert_eq!(json["model_name"], "deepseek-chat");
        assert_eq!(json["display_name"], "DeepSeek Chat");
        assert_eq!(json["context_size"], 65536);
    }
}
