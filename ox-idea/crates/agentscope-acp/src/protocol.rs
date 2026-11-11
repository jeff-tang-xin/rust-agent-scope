//! ACP v1 帧构造与入参解析。
//!
//! 字段形态的准绳是 OXidea 的 v1 处理层（`ide-agent` 的 normalize/tests）
//! 与 ACP v1 规范：`session/update` 以 `update.sessionUpdate` 判别；
//! `tool_call` 的 `kind`/`status` 用 v1 枚举字符串。

use agentscope_core::message::Source;
use agentscope_core::ContentBlock;
use serde_json::{json, Value};

use crate::RpcError;

/// `initialize` 响应（v1 形态）。`loadSession: false`——会话恢复走 agentscope
/// 侧 state store，不经 ACP 重放；fs 能力不声明（agent 自带文件工具，
/// 不依赖 client 的 fs 代理）。
pub fn initialize_result() -> Value {
    json!({
        "protocolVersion": 1,
        "agentCapabilities": {
            "loadSession": false,
            "promptCapabilities": { "image": true },
            // ide-agent 从 _meta 读扩展能力：steer=true 声明支持运行中插队
            // （_oxidea/steer 请求），false 时插队消息退回队列等 turn 结束。
            "_meta": { "/oxidea/steer": true },
        },
        "agentInfo": {
            "name": "agentscope",
            "title": "AgentScope",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "authMethods": [],
    })
}

/// `session/new` 响应。
pub fn new_session_result(session_id: &str) -> Value {
    // promptCapabilities.image：声明接受图片输入（ide-agent 据此放行贴图，
    // 否则 "This agent can't read images." 直接拒绝）。
    json!({
        "sessionId": session_id,
        "agentCapabilities": {
            "loadSession": false,
            "promptCapabilities": { "image": true },
            "_meta": { "/oxidea/steer": true },
        },
    })
}

/// `session/update` 通知帧（外层）。
fn update_frame(session_id: &str, update: Value) -> Value {
    json!({ "sessionId": session_id, "update": update })
}

/// agent 文本增量（流式回复）。
pub fn agent_message_chunk(session_id: &str, delta: &str) -> Value {
    update_frame(
        session_id,
        json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": delta },
        }),
    )
}

/// agent 思考增量（推理模型可见）。
pub fn agent_thought_chunk(session_id: &str, delta: &str) -> Value {
    update_frame(
        session_id,
        json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": { "type": "text", "text": delta },
        }),
    )
}

/// 工具调用开始（`tool_call`）：status 直接给 in_progress（agentscope 的
/// ToolCallStart 到 ToolResultEnd 之间就是执行中，没有独立的 pending 阶段）。
pub fn tool_call_started(
    session_id: &str,
    tool_call_id: &str,
    title: &str,
    kind: &str,
    raw_input: Value,
) -> Value {
    update_frame(
        session_id,
        json!({
            "sessionUpdate": "tool_call",
            "toolCallId": tool_call_id,
            "title": title,
            "kind": kind,
            "status": "in_progress",
            "rawInput": raw_input,
        }),
    )
}

/// 工具进度（长任务 emitter 推送）→ `tool_call_update`，追加输出内容。
pub fn tool_call_progress(session_id: &str, tool_call_id: &str, content_text: &str) -> Value {
    update_frame(
        session_id,
        json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": tool_call_id,
            "status": "in_progress",
            "content": [{ "type": "text", "text": content_text }],
        }),
    )
}

/// 工具调用终态（completed / failed）。
pub fn tool_call_finished(
    session_id: &str,
    tool_call_id: &str,
    status: &str,
    output_text: &str,
) -> Value {
    let mut update = json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": tool_call_id,
        "status": status,
    });
    if !output_text.is_empty() {
        update["content"] = json!([{ "type": "text", "text": output_text }]);
    }
    json!({ "sessionId": session_id, "update": update })
}

/// 上下文压缩状态（`compaction_update`）：Start → in_progress，End →
/// completed（`detail` 作为摘要文本）。同一轮压缩的 Start/End 用同一个
/// compactionId（取 reply_id），client 侧据此把「压缩中」卡片更新为完成。
/// ide-agent 的 normalize.rs 已支持渲染该 sessionUpdate。
pub fn compaction_update(session_id: &str, compaction_id: &str, status: &str, summary_text: &str) -> Value {
    let mut update = json!({
        "sessionUpdate": "compaction_update",
        "compactionId": compaction_id,
        "status": status,
    });
    if !summary_text.is_empty() {
        update["summary"] = json!([{ "type": "text", "text": summary_text }]);
    }
    update_frame(session_id, update)
}

/// `session/request_permission` 请求参数：单个工具调用的授权询问。
/// options 对齐 v1 的 kind 枚举（allow_once / allow_always / reject_once）。
pub fn request_permission_params(
    session_id: &str,
    tool_call_id: &str,
    title: &str,
    kind: &str,
    raw_input: Value,
) -> Value {
    json!({
        "sessionId": session_id,
        "toolCall": {
            "toolCallId": tool_call_id,
            "kind": kind,
            "title": title,
            "rawInput": raw_input,
        },
        "options": [
            { "optionId": "allow_once", "name": "Allow once", "kind": "allow_once" },
            { "optionId": "allow_always", "name": "Always allow", "kind": "allow_always" },
            { "optionId": "reject_once", "name": "Reject", "kind": "reject_once" },
        ],
    })
}

/// 从 `session/prompt` 参数取 (sessionId, 拼接后的文本)。
/// prompt 是 content block 数组，取全部 text 块拼接（图片等块 MVP 忽略）。
pub fn parse_prompt(params: &Value) -> Result<(String, Vec<ContentBlock>), RpcError> {
    let session_id = params
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("session/prompt 缺 sessionId"))?
        .to_owned();
    let blocks = params
        .get("prompt")
        .and_then(Value::as_array)
        .ok_or_else(|| RpcError::invalid_params("session/prompt 缺 prompt 数组"))?;
    let content = content_blocks(blocks)?
        .ok_or_else(|| RpcError::invalid_params("session/prompt 无可识别内容"))?;
    Ok((session_id, content))
}

/// `_oxidea/steer` 参数：`{ sessionId, prompt: [blocks] }`（prompt 形态与
/// session/prompt 相同，ide-agent 侧 `content()` 产出）。
pub fn parse_steer(params: &Value) -> Result<(String, Vec<ContentBlock>), RpcError> {
    let session_id = params
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("_oxidea/steer 缺 sessionId"))?
        .to_owned();
    let blocks = params
        .get("prompt")
        .and_then(Value::as_array)
        .ok_or_else(|| RpcError::invalid_params("_oxidea/steer 缺 prompt 数组"))?;
    let content = content_blocks(blocks)?
        .ok_or_else(|| RpcError::invalid_params("_oxidea/steer 无可识别内容"))?;
    Ok((session_id, content))
}

/// ACP content 块数组 → 框架内容块：text 块拼接成一条文本；image 块（base64）
/// 转成框架的 Image 内容块——多模态模型直接看图（ide-agent 侧已按
/// capabilities.image 协商过能力）。`Ok(None)` = 无可识别内容。
fn content_blocks(blocks: &[Value]) -> Result<Option<Vec<ContentBlock>>, RpcError> {
    let mut text = String::new();
    let mut content: Vec<ContentBlock> = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(t);
                }
            }
            Some("image") => {
                let data = block.get("data").and_then(Value::as_str).ok_or_else(|| {
                    RpcError::invalid_params("image 块缺 data（仅支持 base64 内联图片）")
                })?;
                let mime_type = block
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("image/png")
                    .to_owned();
                content.push(ContentBlock::Image {
                    source: Source::Base64 { media_type: mime_type, data: data.to_owned() },
                    min_pixels: None,
                    max_pixels: None,
                });
            }
            _ => {}
        }
    }
    if text.trim().is_empty() && content.is_empty() {
        return Ok(None);
    }
    if !text.trim().is_empty() {
        content.insert(0, ContentBlock::Text { text });
    }
    Ok(Some(content))
}

/// 从 `session/new` 参数取 cwd（工作目录，agent 的文件工具锚定于此）。
pub fn parse_new_session(params: &Value) -> Result<String, RpcError> {
    let cwd = params
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| RpcError::invalid_params("session/new 缺 cwd"))?;
    Ok(cwd.to_owned())
}

/// 工具名 → ACP v1 kind：按名前缀粗分类（够 client 渲染图标用）。
pub fn tool_kind(name: &str) -> &'static str {
    let lower = name.to_lowercase();
    if lower.contains("read")
        || lower.contains("list")
        || lower.contains("glob")
        || lower.contains("grep")
        || lower.contains("search")
    {
        "read"
    } else if lower.contains("write") || lower.contains("edit") || lower.contains("create") {
        "edit"
    } else if lower.contains("delete") || lower.contains("remove") {
        "delete"
    } else if lower.contains("shell")
        || lower.contains("exec")
        || lower.contains("command")
        || lower.contains("run")
    {
        "execute"
    } else if lower.contains("fetch") || lower.contains("http") || lower.contains("web") {
        "fetch"
    } else {
        "other"
    }
}
