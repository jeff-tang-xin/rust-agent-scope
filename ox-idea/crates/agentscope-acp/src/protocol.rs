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
            // loadSession：会话恢复走 Legacy state store（`<memory_root>/sessions/`
            // 下的 JsonFileAgentStateStore，会话键 = ACP sessionId）——session/load
            // 按 sessionId 重建 agent，持久层自动重放对话历史。
            "loadSession": true,
            // embeddedContext：声明接受内嵌文件（ide-agent 据此把 @ 引用的
            // ≤256KB 文本文件以 resource 块随 prompt 发来，而非只给链接）。
            "promptCapabilities": { "image": true, "embeddedContext": true },
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
///
/// `commands` 是可斜杠调用的 skill 清单（availableCommands 条目，source=skill）：
/// ide-agent 的 on_setup 把它转成 `available_commands_update`，前端 `/` 面板
/// 的 Skills 分组才有数据——磁盘扫描 + 宿主专属内置，由装配方算好传入。
pub fn new_session_result(session_id: &str, commands: &[Value]) -> Value {
    // promptCapabilities：image 声明接受图片输入（ide-agent 据此放行贴图，
    // 否则 "This agent can't read images." 直接拒绝）；embeddedContext 声明
    // 接受内嵌文件（@ 引用的文件内容随 prompt 到达，见 content_blocks）。
    let mut result = json!({
        "sessionId": session_id,
        "agentCapabilities": {
            "loadSession": true,
            "promptCapabilities": { "image": true, "embeddedContext": true },
            "_meta": { "/oxidea/steer": true },
        },
    });
    if !commands.is_empty() {
        result["availableCommands"] = Value::Array(commands.to_vec());
    }
    result
}

/// mode configOption（category "mode"）：Code/Plan 两态下拉，前端 Composer
/// 现成渲染（ide-agent `config.rs::parse_options` 只收 select 类型）。plan 态
/// 由用户切换（`session/set_config_option`），agent 不自行进入——开关归用户，
/// 计划内容由 plan_prompt 引导。
pub fn mode_config_options(plan_active: bool) -> Value {
    json!([{
        "id": "mode",
        "name": "Mode",
        "category": "mode",
        "type": "select",
        "currentValue": if plan_active { "plan" } else { "code" },
        "options": [
            {
                "value": "code",
                "name": "Code",
                "description": "完整执行：读写文件、跑命令、改代码",
            },
            {
                "value": "plan",
                "name": "Plan",
                "description": "只读研究与设计：写操作被拦截，产出计划待批准",
            },
        ],
    }])
}

/// `session/new` / `session/load` 响应（带 mode configOption，`plan_active`
/// 反映恢复会话的真实 plan 态；`commands` 见 [`new_session_result`]）。
pub fn session_result(session_id: &str, plan_active: bool, commands: &[Value]) -> Value {
    let mut result = new_session_result(session_id, commands);
    result["configOptions"] = mode_config_options(plan_active);
    result
}

/// 解析 `session/set_config_option`：只认 mode 选项（configId=="mode"，
/// value ∈ code/plan）。ide-agent 转发时会补 `type: "id"` 标签，这里不校验。
pub fn parse_set_config_option(params: &Value) -> Result<(String, String), RpcError> {
    let session_id = params
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| RpcError::invalid_params("sessionId 缺失"))?
        .to_owned();
    let config_id = params
        .get("configId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if config_id != "mode" {
        return Err(RpcError::invalid_params(format!(
            "未知配置项: {config_id}（仅支持 mode）"
        )));
    }
    let value = params
        .get("value")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if value != "code" && value != "plan" {
        return Err(RpcError::invalid_params(format!(
            "未知模式: {value}（code / plan）"
        )));
    }
    Ok((session_id, value))
}

/// 模式变化通知（`config_option_update`）：agent 侧切换后推给 client，前端
/// 下拉同步（ide-agent `lib.rs` 的 update 分发已处理该 sessionUpdate）。
pub fn config_option_update(session_id: &str, options: &Value) -> Value {
    update_frame(
        session_id,
        json!({ "sessionUpdate": "config_option_update", "configOptions": options }),
    )
}

/// 校验并规范化 `update_plan` 工具入参里的任务条目：content 必填非空、
/// status ∈ pending/in_progress/completed、priority 缺省 medium（非法值报错，
/// 与工具侧校验一致——双保险，工具先拦、这里兜底）。
pub fn parse_plan_entries(raw: &Value) -> Result<Vec<Value>, String> {
    let entries = raw
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(|| "entries (array) missing".to_owned())?;
    let mut out = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let content = entry
            .get("content")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .ok_or_else(|| format!("entries[{index}].content missing"))?;
        let status = entry.get("status").and_then(Value::as_str).unwrap_or_default();
        if !matches!(status, "pending" | "in_progress" | "completed") {
            return Err(format!("entries[{index}].status invalid: {status}"));
        }
        let priority = match entry.get("priority").and_then(Value::as_str) {
            Some(p @ ("high" | "medium" | "low")) => p,
            Some(other) => return Err(format!("entries[{index}].priority invalid: {other}")),
            None => "medium",
        };
        out.push(json!({
            "content": content,
            "priority": priority,
            "status": status,
        }));
    }
    Ok(out)
}

/// 任务清单通知（`plan_update`，type=items）：ide-agent `normalize.rs` 只转
/// items 形态（markdown/file 不渲染），前端 `sessionUpdate: "plan"` 现成渲染，
/// 前端零改动。planId 固定 "tasks"——单会话单清单，全量替换语义。
pub fn plan_update_frame(session_id: &str, entries: &[Value]) -> Value {
    update_frame(
        session_id,
        json!({
            "sessionUpdate": "plan_update",
            "plan": { "type": "items", "planId": "tasks", "entries": entries },
        }),
    )
}

/// 实现方案通知（`plan_update`，type=markdown）：plan_write 写入 spec.md
/// 成功后推 spec 全文，前端卡片实时展示方案内容；执行期 update_plan 的
/// items 帧全量替换为执行清单。planId 同为 "tasks"——单会话单卡片。
pub fn plan_update_markdown_frame(session_id: &str, markdown: &str) -> Value {
    update_frame(
        session_id,
        json!({
            "sessionUpdate": "plan_update",
            "plan": { "type": "markdown", "planId": "tasks", "markdown": markdown },
        }),
    )
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

/// 工具参数补全（`ToolCallEnd` 时参数已完整）→ `tool_call_update`，携带
/// 完整 `rawInput`——首帧 `tool_call` 发出时参数尚未流完，只能后补。
pub fn tool_call_input(session_id: &str, tool_call_id: &str, raw_input: Value) -> Value {
    update_frame(
        session_id,
        json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": tool_call_id,
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
            // ide-agent 的 timeline.merge_tool 只认 v2 形态（外层 type:"content"
            // 包一层）——裸 {type:"text"} 会被丢弃，工具输出到不了前端卡片。
            "content": [{ "type": "content", "content": { "type": "text", "text": content_text } }],
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
        // 形态同 tool_call_progress：v2 content 包裹，timeline 才提取为 output。
        update["content"] = json!([{ "type": "content", "content": { "type": "text", "text": output_text } }]);
    }
    json!({ "sessionId": session_id, "update": update })
}

/// 上下文压缩开始（v1 形态）：「Compact conversation」工具卡片首帧，
/// in_progress——摘要由模型生成可能耗时数秒，先给前端「压缩中」状态。
///
/// agentscope-acp 是 v1 agent（protocolVersion: 1），ide-agent 对 v1 通知
/// **不走 normalizer**，`compaction_update`（v2 sessionUpdate 类型）到达
/// timeline 后被静默丢弃——压缩必须按 v1 的表达方式报告：一个工具调用
/// （与 normalize.rs 对 v2 agent 的降级映射一致）。同一轮压缩的
/// Start/End 用同一个 compactionId（取 reply_id），toolCallId 加
/// `compaction:` 前缀与真实工具调用隔离。
pub fn compaction_tool_call(session_id: &str, compaction_id: &str) -> Value {
    update_frame(
        session_id,
        json!({
            "sessionUpdate": "tool_call",
            "toolCallId": format!("compaction:{compaction_id}"),
            "title": "Compact conversation",
            "kind": "other",
            "status": "in_progress",
        }),
    )
}

/// 上下文压缩结束（v1 形态）：`tool_call_update` 收尾——completed/failed
/// 加摘要文本（`detail`，如「57 条消息合并为 1 条摘要」）。
pub fn compaction_tool_result(
    session_id: &str,
    compaction_id: &str,
    status: &str,
    summary_text: &str,
) -> Value {
    let mut update = json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": format!("compaction:{compaction_id}"),
        "status": status,
    });
    if !summary_text.is_empty() {
        // 形态同 tool_call content：v2 包裹（timeline 的 output 提取只认它）。
        update["content"] = json!([{ "type": "content", "content": { "type": "text", "text": summary_text } }]);
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
            Some("resource") => {
                // ide-agent 内嵌的 @ 引用文件（embeddedContext 协商后 ≤256KB 的
                // 文本文件）：带路径标注拼进文本，模型直接看到内容，无需再调
                // read_file。缺 text 的 resource（如二进制引用）跳过。
                let resource = block.get("resource");
                let uri = resource
                    .and_then(|r| r.get("uri"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let body = resource
                    .and_then(|r| r.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if !body.is_empty() {
                    let path = file_uri_to_path(uri).unwrap_or_else(|| uri.to_owned());
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&format!("\n<file path=\"{path}\">\n{body}\n</file>"));
                }
            }
            Some("resource_link") => {
                // 过大未内嵌的 @ 引用文件：转成路径提及，模型需要时用 read_file
                // 读取（read_file 吃文件系统路径，故把 file:// URI 解码回路径）。
                let uri = block.get("uri").and_then(Value::as_str).unwrap_or("");
                if !uri.is_empty() {
                    let path = file_uri_to_path(uri).unwrap_or_else(|| uri.to_owned());
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&format!(
                        "\n[引用文件（过大未内嵌，需要时用 read_file 读取）: {path}]"
                    ));
                }
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

/// 从 `session/load` 参数取 (sessionId, cwd)。`mcpServers` 由 ide-agent 发送，
/// 但 OX agent 的工具全部进程内注入，忽略。
pub fn parse_load_session(params: &Value) -> Result<(String, String), RpcError> {
    let session_id = params
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| RpcError::invalid_params("session/load 缺 sessionId"))?
        .to_owned();
    let cwd = parse_new_session(params)?;
    Ok((session_id, cwd))
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

/// `file://` URI → 文件系统路径（剥前缀 + percent-decode）；非 file URI 返回 None。
/// Windows 盘符形态 `file:///C:/x` 剥掉多余的 `/`，Unix `file:///home/x` 保留首 `/`。
fn file_uri_to_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    // `file:///C:/x` 剥前缀后是 "/C:/x"：index 0 为 '/'、index 2 为 ':'（盘符
    // 在 1）即 Windows，剥掉多余的 '/'；Unix "/home/x" 原样保留。
    let raw = if rest.len() >= 3 && rest.as_bytes()[0] == b'/' && rest.as_bytes()[2] == b':' {
        rest.strip_prefix('/').unwrap_or(rest)
    } else {
        rest
    };
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16)?;
                let lo = (bytes[i + 2] as char).to_digit(16)?;
                out.push((hi * 16 + lo) as u8);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_declares_embedded_context() {
        let v = initialize_result();
        assert_eq!(v["agentCapabilities"]["promptCapabilities"]["embeddedContext"], json!(true));
    }

    #[test]
    fn new_session_declares_embedded_context() {
        let v = new_session_result("s1", &[]);
        assert_eq!(v["agentCapabilities"]["promptCapabilities"]["embeddedContext"], json!(true));
    }

    #[test]
    fn new_session_declares_skill_commands() {
        let commands = vec![
            json!({ "name": "oxidea-usage", "description": "OXidea 操作指南", "source": "skill" }),
            json!({ "name": "spec-workflow", "description": "spec 文档生命周期", "source": "skill" }),
        ];
        let v = new_session_result("s1", &commands);
        let listed = v["availableCommands"].as_array().expect("availableCommands 缺失");
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0]["name"], "oxidea-usage");
        assert_eq!(listed[0]["source"], "skill");
        // 空清单不声明该字段（ide-agent on_setup 对缺失字段直接跳过）。
        assert!(new_session_result("s2", &[]).get("availableCommands").is_none());
    }

    #[test]
    fn resource_block_embeds_file_with_path_tag() {
        let blocks = json!([
            { "type": "text", "text": "看看这个" },
            { "type": "resource", "resource": { "uri": "file:///C:/proj/src/a.rs", "text": "fn main() {}" } },
        ]);
        let content = content_blocks(blocks.as_array().unwrap()).unwrap().unwrap();
        match &content[0] {
            ContentBlock::Text { text } => {
                assert!(text.contains("看看这个"));
                assert!(text.contains("<file path=\"C:/proj/src/a.rs\">"));
                assert!(text.contains("fn main() {}"));
            }
            _ => panic!("expected text block, got {:?}", content[0]),
        }
    }

    #[test]
    fn resource_without_text_is_skipped() {
        let blocks = json!([
            { "type": "text", "text": "hi" },
            { "type": "resource", "resource": { "uri": "file:///C:/bin.dat" } },
        ]);
        let content = content_blocks(blocks.as_array().unwrap()).unwrap().unwrap();
        match &content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "hi"),
            _ => panic!("expected text block"),
        }
    }

    #[test]
    fn resource_link_becomes_path_mention() {
        let blocks = json!([
            { "type": "text", "text": "看下" },
            { "type": "resource_link", "uri": "file:///C:/proj/big.log", "name": "big.log" },
        ]);
        let content = content_blocks(blocks.as_array().unwrap()).unwrap().unwrap();
        match &content[0] {
            ContentBlock::Text { text } => {
                assert!(text.contains("C:/proj/big.log"));
                assert!(text.contains("read_file"));
            }
            _ => panic!("expected text block"),
        }
    }

    #[test]
    fn file_uri_decodes_percent_and_windows_drive() {
        assert_eq!(
            file_uri_to_path("file:///C:/Users%20name/a%5Cb.rs").as_deref(),
            Some("C:/Users name/a\\b.rs")
        );
        assert_eq!(file_uri_to_path("file:///home/x/y.rs").as_deref(), Some("/home/x/y.rs"));
        assert_eq!(file_uri_to_path("http://x/y"), None);
    }

    #[test]
    fn initialize_declares_load_session() {
        let v = initialize_result();
        assert_eq!(v["agentCapabilities"]["loadSession"], json!(true));
    }

    #[test]
    fn parse_load_session_extracts_id_and_cwd() {
        let (id, cwd) =
            parse_load_session(&json!({ "sessionId": "s1", "cwd": "/p", "mcpServers": [] })).unwrap();
        assert_eq!(id, "s1");
        assert_eq!(cwd, "/p");
        assert!(parse_load_session(&json!({ "cwd": "/p" })).is_err());
        assert!(parse_load_session(&json!({ "sessionId": "s1" })).is_err());
    }
}
