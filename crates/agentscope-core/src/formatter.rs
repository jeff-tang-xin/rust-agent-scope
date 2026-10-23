//! 消息格式化层（对应 Java 版 `io.agentscope.core.formatter`）。
//!
//! 设计要点：Formatter 是「AgentScope 消息 ↔ 提供商格式」的独立抽象。
//! Model 实现只负责传输（HTTP / SSE），格式转换、参数装配、工具定义下发给 Formatter。
//!
//! Java 用泛型 `Formatter<TReq, TResp, TParams>`（请求消息 / 响应 / 参数构建器），
//! Rust 用 trait 关联类型表达同样的类型安全，且不需要装箱。

use std::path::Path;

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};
use crate::message::{metadata_keys, ContentBlock, Msg, Role, Source};
use crate::model::{ChatResponse, GenerateOptions, ToolChoice, ToolSchema};

// ---------- 结构化输出 ----------

/// 结构化输出的 JSON Schema（对应 Java `JsonSchema`）。
///
/// 用于 `response_format = json_schema`：约束模型输出必须符合给定结构。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonSchema {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

impl JsonSchema {
    pub fn new(name: impl Into<String>, schema: Value) -> Self {
        Self {
            name: name.into(),
            description: None,
            schema,
            strict: None,
        }
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// 开启严格校验（提供商支持时按 schema 强约束输出）。
    pub fn with_strict(mut self, strict: bool) -> Self {
        self.strict = Some(strict);
        self
    }
}

/// 响应格式（对应 Java `ResponseFormat`）：text / json_object / json_schema。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseFormat {
    /// 纯文本（默认）。
    Text,
    /// 合法 JSON 对象（无 schema 约束）。
    JsonObject,
    /// 符合指定 JSON Schema 的结构化输出。
    JsonSchema {
        #[serde(rename = "json_schema")]
        json_schema: JsonSchema,
    },
}

impl ResponseFormat {
    pub fn text() -> Self {
        Self::Text
    }

    pub fn json_object() -> Self {
        Self::JsonObject
    }

    pub fn json_schema(schema: JsonSchema) -> Self {
        Self::JsonSchema {
            json_schema: schema,
        }
    }
}

// ---------- 媒体工具 ----------

/// 多模态媒体处理工具（对应 Java `MediaUtils`）。
///
/// 与 Java 的差异：Java 用 JDK 内置 `HttpURLConnection` 做 URL 下载；
/// Rust 核心 crate 不引入 HTTP 依赖，网络侧处理（下载转 base64）由提供商 crate 负责，
/// 此处只保留纯本地逻辑（路径判定、MIME 推断、读取编码、file:// 转换）。
pub mod media {
    use super::*;

    /// 超过该体积告警（10MB）。
    pub const WARN_SIZE_BYTES: u64 = 10 * 1024 * 1024;
    /// 超过该体积直接拒绝（50MB）。
    pub const MAX_SIZE_BYTES: u64 = 50 * 1024 * 1024;

    pub const SUPPORTED_IMAGE_EXTENSIONS: &[&str] =
        &["png", "jpg", "jpeg", "gif", "webp", "heic", "heif"];
    pub const SUPPORTED_AUDIO_EXTENSIONS: &[&str] = &["wav", "mp3"];
    pub const SUPPORTED_VIDEO_EXTENSIONS: &[&str] = &[
        "mp4", "mpeg", "mpg", "mov", "avi", "webm", "wmv", "flv", "3gp", "3gpp",
    ];

    /// 是否为本地文件路径（无协议头，对应 Java `isLocalFile`）。
    pub fn is_local_file(url: &str) -> bool {
        if url.trim().is_empty() {
            return false;
        }
        !["http://", "https://", "ftp://", "file://", "oss://"]
            .iter()
            .any(|p| url.starts_with(p))
    }

    /// 取扩展名（不含点，小写）。
    pub fn get_extension(path: &str) -> String {
        Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase()
    }

    /// 从扩展名推断 MIME 类型（对应 Java `determineMediaType`）。
    pub fn determine_media_type(path: &str) -> String {
        match get_extension(path).as_str() {
            // 图像
            "jpg" | "jpeg" => "image/jpeg",
            "png" => "image/png",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "heic" => "image/heic",
            "heif" => "image/heif",
            // 音频
            "wav" => "audio/wav",
            "mp3" => "audio/mp3",
            // 视频
            "mp4" => "video/mp4",
            "mpeg" | "mpg" => "video/mpeg",
            "mov" => "video/quicktime",
            "avi" => "video/x-msvideo",
            "webm" => "video/webm",
            "wmv" => "video/x-ms-wmv",
            "flv" => "video/x-flv",
            "3gp" | "3gpp" => "video/3gpp",
            _ => "application/octet-stream",
        }
        .to_string()
    }

    /// 从音频路径推断音频格式（对应 Java `determineAudioFormat`）。
    pub fn determine_audio_format(path: &str) -> &'static str {
        match get_extension(path).as_str() {
            "wav" => "wav",
            "mp3" => "mp3",
            _ => "mp3",
        }
    }

    /// 由 MIME 类型推断音频格式（对应 Java `inferAudioFormatFromMediaType`）。
    pub fn infer_audio_format_from_media_type(media_type: &str) -> &'static str {
        if media_type.contains("wav") {
            "wav"
        } else {
            "mp3"
        }
    }

    /// 解析来源的 MIME 类型（对应 Java `resolveMimeType`）：
    /// base64 来源自带 media_type；URL 来源按扩展名推断。
    pub fn resolve_mime_type(source: &Source) -> String {
        match source {
            Source::Base64 { media_type, .. } => media_type.clone(),
            Source::Url { url, mime_type } => mime_type
                .clone()
                .unwrap_or_else(|| determine_media_type(url)),
        }
    }

    pub fn is_file_exists(path: &str) -> bool {
        Path::new(path).is_file()
    }

    /// 校验文件体积（对应 Java `checkFileSize`）。
    fn check_file_size(path: &str) -> Result<()> {
        let size = std::fs::metadata(path)?.len();
        if size > MAX_SIZE_BYTES {
            return Err(Error::InvalidInput(format!(
                "file too large: {path} ({size} bytes, max {MAX_SIZE_BYTES})"
            )));
        }
        Ok(())
    }

    /// 本地文件转 base64（对应 Java `fileToBase64`）。
    pub fn file_to_base64(path: &str) -> Result<String> {
        let p = Path::new(path);
        if !p.exists() {
            return Err(Error::InvalidInput(format!("file does not exist: {path}")));
        }
        check_file_size(path)?;
        let bytes = std::fs::read(p)?;
        Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    /// 本地路径转 `file://` 协议 URL（对应 Java `toFileProtocolUrl`），
    /// 部分提供商（如 DashScope 视觉模型）接受该协议读取本地文件。
    pub fn to_file_protocol_url(path: &str) -> Result<String> {
        let abs = std::fs::canonicalize(path)
            .map_err(|_| Error::InvalidInput(format!("file does not exist: {path}")))?;
        let display = abs.display().to_string().replace('\\', "/");
        Ok(format!("file://{display}"))
    }

    /// 校验图像扩展名（对应 Java `validateImageExtension`）。
    pub fn validate_image_extension(url: &str) -> Result<()> {
        validate_extension(url, SUPPORTED_IMAGE_EXTENSIONS, "image")
    }

    /// 校验音频扩展名。
    pub fn validate_audio_extension(url: &str) -> Result<()> {
        validate_extension(url, SUPPORTED_AUDIO_EXTENSIONS, "audio")
    }

    /// 校验视频扩展名。
    pub fn validate_video_extension(url: &str) -> Result<()> {
        validate_extension(url, SUPPORTED_VIDEO_EXTENSIONS, "video")
    }

    fn validate_extension(url: &str, supported: &[&str], kind: &str) -> Result<()> {
        let ext = get_extension(url);
        if supported.contains(&ext.as_str()) {
            Ok(())
        } else {
            Err(Error::InvalidInput(format!(
                "unsupported {kind} extension: {ext} (supported: {supported:?})"
            )))
        }
    }

    /// 把 base64 数据写入临时文件，返回绝对路径
    /// （对应 Java `saveBase64DataToTempFile`：工具结果里的媒体块转文本引用时使用）。
    pub fn save_base64_to_temp_file(media_type: &str, base64_data: &str) -> Result<String> {
        let extension = media_type
            .split('/')
            .nth(1)
            .unwrap_or(media_type)
            .split(';')
            .next()
            .unwrap_or("bin");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(base64_data)
            .map_err(|e| Error::InvalidInput(format!("invalid base64 data: {e}")))?;
        let dir = std::env::temp_dir();
        let file = dir.join(format!("agentscope_{}.{extension}", uuid::Uuid::new_v4().simple()));
        std::fs::write(&file, bytes)?;
        Ok(file.display().to_string())
    }
}

// ---------- 共享格式化逻辑 ----------

/// 提取消息的纯文本内容（对应 Java `AbstractBaseFormatter#extractTextContent`）。
///
/// 规则：取 `Text` 与 `Hint`；**跳过 `Thinking`**（思考内容存于记忆但不回传 LLM）；
/// `ToolResult` 取其文本输出。多段用换行连接。
pub fn extract_text_content(msg: &Msg) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for block in &msg.content {
        match block {
            ContentBlock::Text { text } => parts.push(text),
            ContentBlock::Hint { hint, .. } => parts.push(hint),
            ContentBlock::Thinking { .. } => {}
            ContentBlock::ToolResult { output, .. } => {
                parts.extend(output.iter().filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                }));
            }
            _ => {}
        }
    }
    parts.join("\n")
}

/// 消息是否含多模态内容（图像 / 音频 / 视频 / 通用数据）。
pub fn has_media_content(msg: &Msg) -> bool {
    msg.content.iter().any(|block| {
        matches!(
            block,
            ContentBlock::Image { .. }
                | ContentBlock::Audio { .. }
                | ContentBlock::Video { .. }
                | ContentBlock::Data { .. }
        )
    })
}

/// 角色标签（多 agent 对话历史里标识发言者角色）。
pub fn role_label(role: Role) -> &'static str {
    match role {
        Role::User => "User",
        Role::Assistant => "Assistant",
        Role::System => "System",
        Role::Tool => "Tool",
    }
}

/// 该消息是否应绕过历史合并（多 agent 格式化器用）。
pub fn should_bypass_history(msg: &Msg) -> bool {
    msg.metadata
        .as_ref()
        .and_then(|m| m.get(metadata_keys::BYPASS_MULTIAGENT_HISTORY_MERGE))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// 把工具结果块列表转为字符串（对应 Java `convertToolResultToString`）。
///
/// 媒体块（图像/音频/视频/数据）转为文本引用：URL 直接给地址，
/// base64 落到临时文件后给路径。单条直接返回，多条以 "- " 前缀逐行列出。
pub fn convert_tool_result_to_string(output: &[ContentBlock]) -> String {
    if output.is_empty() {
        return String::new();
    }
    let mut textual: Vec<String> = Vec::new();
    for block in output {
        match block {
            ContentBlock::Text { text } => textual.push(text.clone()),
            ContentBlock::Image { source, .. } => {
                textual.push(media_block_reference(source, "image"))
            }
            ContentBlock::Audio { source } => textual.push(media_block_reference(source, "audio")),
            ContentBlock::Video { source, .. } => textual.push(media_block_reference(source, "video")),
            ContentBlock::Data { source, .. } => textual.push(media_block_reference(source, "data")),
            // 其他块（如 Thinking）不回传
            _ => {}
        }
    }
    if textual.len() == 1 {
        textual.into_iter().next().unwrap_or_default()
    } else {
        textual
            .iter()
            .map(|s| format!("- {s}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn media_block_reference(source: &Source, media_type: &str) -> String {
    match source {
        Source::Url { url, .. } => format!("The returned {media_type} can be found at: {url}"),
        Source::Base64 {
            media_type: mime,
            data,
        } => match media::save_base64_to_temp_file(mime, data) {
            Ok(path) => format!("The returned {media_type} can be found at: {path}"),
            Err(e) => format!("[{media_type} - failed to save file: {e}]"),
        },
    }
}

// ---------- Formatter 契约 ----------

/// 消息格式化契约（对应 Java `Formatter<TReq, TResp, TParams>`）。
///
/// 职责：
/// 1. `Msg` 列表 → 提供商请求消息；
/// 2. 提供商响应 → [`ChatResponse`]；
/// 3. 生成参数与工具定义写入提供商请求参数。
///
/// Java `AbstractBaseFormatter#getOptionOrDefault`（请求级优先、模型默认兜底）在 Rust 里
/// 等价于 `options.x.or_else(|| defaults.and_then(|d| d.x))`，由各实现直接展开，不额外抽函数。
pub trait Formatter: Send + Sync {
    /// 提供商请求消息类型（如 OpenAI `ChatCompletionMessageParam`）。
    type RequestMessage;
    /// 提供商请求参数类型（请求体 / builder）。
    type RequestParams;
    /// 提供商响应类型（完整响应或流式 chunk）。
    type Response;

    /// 消息格式化；`options` 为本次请求的生成选项（部分提供商从中派生消息字段）。
    fn format(
        &self,
        msgs: &[Msg],
        options: Option<&GenerateOptions>,
    ) -> Result<Vec<Self::RequestMessage>>;

    /// 解析提供商响应为统一 [`ChatResponse`]。
    fn parse_response(&self, response: Self::Response) -> Result<ChatResponse>;

    /// 应用生成参数：`options` 优先，缺失时回退 `defaults`。
    fn apply_options(
        &self,
        params: &mut Self::RequestParams,
        options: &GenerateOptions,
        defaults: Option<&GenerateOptions>,
    );

    /// 应用工具定义（schema 列表可能为空）。
    fn apply_tools(&self, params: &mut Self::RequestParams, tools: &[ToolSchema]);

    /// 应用工具选择策略（默认不设置，交由提供商默认行为）。同步方法，不加 `async_trait`。
    fn apply_tool_choice(
        &self,
        _params: &mut Self::RequestParams,
        _tool_choice: Option<&ToolChoice>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 对齐 Java `extractTextContent`：取 Text 与 Hint，跳过 Thinking，ToolResult 取文本输出。
    #[test]
    fn extract_text_content_rules() {
        let msg = Msg::new(
            Role::Assistant,
            "a",
            vec![
                ContentBlock::Text {
                    text: "答案".into(),
                },
                ContentBlock::Thinking {
                    thinking: "内心独白".into(),
                },
                ContentBlock::Hint {
                    id: "h1".into(),
                    hint: "提示".into(),
                    source: None,
                },
                ContentBlock::ToolResult {
                    id: "c1".into(),
                    name: "t".into(),
                    output: vec![ContentBlock::Text {
                        text: "结果".into(),
                    }],
                    state: crate::message::ToolResultState::Success,
                    execution_details: None,
                },
            ],
        );
        let text = extract_text_content(&msg);
        assert_eq!(text, "答案\n提示\n结果");
        assert!(!text.contains("内心独白"), "thinking 不回传模型");
    }

    #[test]
    fn tool_result_string_single_and_multiple() {
        // 单条直接返回
        let single = vec![ContentBlock::Text {
            text: "3".into(),
        }];
        assert_eq!(convert_tool_result_to_string(&single), "3");

        // 多条以 "- " 前缀逐行列出
        let multi = vec![
            ContentBlock::Text { text: "a".into() },
            ContentBlock::Text { text: "b".into() },
        ];
        assert_eq!(convert_tool_result_to_string(&multi), "- a\n- b");
        assert_eq!(convert_tool_result_to_string(&[]), "");
    }

    /// 媒体块在工具结果里转文本引用（URL 直接给地址），避免多模态信息在纯文本通道静默丢失。
    #[test]
    fn tool_result_media_becomes_text_reference() {
        let output = vec![ContentBlock::Image {
            source: Source::Url {
                url: "https://example.com/a.png".into(),
                mime_type: None,
            },
            min_pixels: None,
            max_pixels: None,
        }];
        assert_eq!(
            convert_tool_result_to_string(&output),
            "The returned image can be found at: https://example.com/a.png"
        );
    }

    #[test]
    fn has_media_content_detection() {
        let text_only = Msg::user("u", "hi");
        assert!(!has_media_content(&text_only));
        let with_image = Msg::new(
            Role::User,
            "u",
            vec![ContentBlock::Image {
                source: Source::Base64 {
                    media_type: "image/png".into(),
                    data: "aGk=".into(),
                },
                min_pixels: None,
                max_pixels: None,
            }],
        );
        assert!(has_media_content(&with_image));
    }

    #[test]
    fn should_bypass_history_reads_metadata_flag() {
        let plain = Msg::user("u", "hi");
        assert!(!should_bypass_history(&plain));
        let flagged = plain.clone().with_metadata(json!({
            metadata_keys::BYPASS_MULTIAGENT_HISTORY_MERGE: true
        }));
        assert!(should_bypass_history(&flagged));
    }

    /// 结构化输出的线格式：{"type":"json_schema","json_schema":{...}}。
    #[test]
    fn response_format_serialization() {
        let format = ResponseFormat::json_schema(
            JsonSchema::new("Answer", json!({"type": "object"})).with_strict(true),
        );
        let v = serde_json::to_value(&format).unwrap();
        assert_eq!(v["type"], "json_schema");
        assert_eq!(v["json_schema"]["name"], "Answer");
        assert_eq!(v["json_schema"]["strict"], true);
        assert_eq!(
            serde_json::to_value(ResponseFormat::text()).unwrap(),
            json!({"type": "text"})
        );
        assert_eq!(
            serde_json::to_value(ResponseFormat::json_object()).unwrap(),
            json!({"type": "json_object"})
        );
    }

    #[test]
    fn media_utils_helpers() {
        assert!(media::is_local_file("/tmp/a.png"));
        assert!(!media::is_local_file("https://example.com/a.png"));
        assert_eq!(media::determine_media_type("a.JPG"), "image/jpeg");
        assert_eq!(media::determine_media_type("a.mp3"), "audio/mp3");
        assert_eq!(media::determine_media_type("a.unknown"), "application/octet-stream");
        assert!(media::validate_image_extension("a.png").is_ok());
        assert!(media::validate_image_extension("a.mp3").is_err());
        assert_eq!(
            media::resolve_mime_type(&Source::Base64 {
                media_type: "image/webp".into(),
                data: String::new(),
            }),
            "image/webp"
        );
    }

    /// base64 数据落临时文件（工具结果媒体转引用的落盘路径）。
    #[test]
    fn save_base64_to_temp_file_writes_bytes() {
        let path = media::save_base64_to_temp_file("image/png", "aGVsbG8=").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        let _ = std::fs::remove_file(path);
    }
}
