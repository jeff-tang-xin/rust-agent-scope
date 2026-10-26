//! 上下文压缩（对应 Java `memory.compaction` 包 + `middleware.CompactionMiddleware`）。
//!
//! 与 Java 一致的核心语义：
//! - 触发：消息数 ≥ `trigger_messages` 或估算 token ≥ 有效触发阈值（动态模式下
//!   阈值 = 模型窗口 − `reserved`，窗口未知时回落 [`FALLBACK_TRIGGER_TOKENS`]）；
//! - 切分：prefix（system + 合成消息）与 conversation 分离，只压缩 conversation；
//!   cutoff 按 `keep_tokens`（二分）或 `keep_messages` 确定，并回退到安全点——
//!   绝不把 ASSISTANT 工具调用与其 TOOL 结果拆到两侧；
//! - 摘要：一次辅助 LLM 调用把 prefix 蒸馏成结构化摘要（保留旧摘要一起输入，
//!   链式压缩逐次累积）；摘要消息 name 为 [`COMPACTION_SUMMARY_MSG_NAME`]，
//!   id 由内容稳定派生（重复摘要可识别）；
//! - flush：`flush_before_compact` 开启时先对新压缩的原始消息做记忆抽取
//!   （best-effort，失败不阻断压缩；旧摘要消息排除，避免重复抽取）；
//! - 提交：压缩结果写回走 [`ReActAgent::save_agent_state_for`] 的 CAS——
//!   并发变更时保存失败即放弃本次压缩（对齐 Java `canCommit` 语义）。
//!
//! 与 Java 的有意差异（lib.rs 差异声明同步维护）：
//! - Java 在每轮 ReAct 迭代的 `onReasoning` 钩子检查；Rust 中间件钩子均为同步，
//!   压缩需要 await（flush + 摘要 LLM 调用），故挂在 [`HarnessAgent`] 的 turn
//!   入口（`call`/`stream_events` 进 delegate 之前）——每 turn 检查一次；
//! - Java 的 overflow 恢复（`overflowRecovery`，模型报上下文溢出后强制压缩重试）
//!   依赖错误恢复钩子，Rust 中间件无此扩展点，未复刻；
//! - `truncateArgs` / `pruneToolResults`（压缩前轻量裁剪）与
//!   `ToolResultEvictionMiddleware` 未复刻；
//! - 自定义 `ConversationCompactionStrategy` 未复刻（Java 侧也是可选扩展）。

use std::sync::Arc;

use agentscope_core::context::RuntimeContext;
use agentscope_core::message::metadata_keys::SYNTHETIC;
use agentscope_core::message::{ContentBlock, Msg, Role};
use agentscope_core::model::{ChatRequest, GenerateOptions, Model};
use serde_json::json;

use crate::memory::{MemoryFlushManager, COMPACTION_SUMMARY_MSG_NAME};

/// 模型窗口未知时的触发预算回落值（对齐 Java `FALLBACK_TRIGGER_TOKENS`）。
pub const FALLBACK_TRIGGER_TOKENS: usize = 160_000;

// ---------------------------------------------------------------------------
// token 估算（对齐 Java `TokenCounterUtil`）
// ---------------------------------------------------------------------------

const CHARS_PER_TOKEN: f64 = 2.5;
const MESSAGE_OVERHEAD: usize = 5;
const TOOL_CALL_OVERHEAD: usize = 10;
const TOOL_RESULT_OVERHEAD: usize = 8;
const OTHER_BLOCK_OVERHEAD: usize = 5;

/// 消息列表的估算 token 数（字符数 / 2.5 + 结构开销；对齐 Java `calculateToken`）。
pub fn estimate_tokens(messages: &[Msg]) -> usize {
    messages.iter().map(estimate_message_tokens as fn(&Msg) -> usize).sum()
}

fn estimate_message_tokens(msg: &Msg) -> usize {
    let mut tokens = MESSAGE_OVERHEAD;
    if !msg.name.is_empty() {
        tokens += estimate_text(&msg.name);
    }
    for block in &msg.content {
        tokens += estimate_block_tokens(block);
    }
    tokens
}

fn estimate_block_tokens(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Text { text } => estimate_text(text),
        ContentBlock::Thinking { thinking } => estimate_text(thinking),
        ContentBlock::ToolUse { name, input, .. } => {
            TOOL_CALL_OVERHEAD + estimate_text(name) + estimate_text(&input.to_string())
        }
        ContentBlock::ToolResult { name, id, output, .. } => {
            let mut tokens = TOOL_RESULT_OVERHEAD;
            tokens += estimate_text(name);
            tokens += estimate_text(id);
            for block in output {
                tokens += estimate_block_tokens(block);
            }
            tokens
        }
        _ => OTHER_BLOCK_OVERHEAD,
    }
}

fn estimate_text(text: &str) -> usize {
    (text.chars().count() as f64 / CHARS_PER_TOKEN).ceil() as usize
}

// ---------------------------------------------------------------------------
// 配置（对齐 Java `CompactionConfig` 的默认值）
// ---------------------------------------------------------------------------

/// 默认摘要提示词（对齐 Java `DEFAULT_SUMMARY_PROMPT`，结构化抽取 + 逐字保留
/// 可复用字符串 + 链式合并 + 不提及压缩步骤本身）。
pub const DEFAULT_SUMMARY_PROMPT: &str = r#"<role>
Context Extraction Assistant
</role>

<primary_objective>
Your sole objective in this task is to extract the highest quality/most relevant context from the conversation history below.
</primary_objective>

<objective_information>
You're nearing the total number of input tokens you can accept, so you must extract the highest quality/most relevant pieces of information from your conversation history.
This context will then overwrite the conversation history presented below. Because of this, ensure the context you extract is only the most important information to continue working toward your overall goal.
</objective_information>

<instructions>
The conversation history below will be replaced with the context you extract in this step. You want to ensure that you don't repeat any actions you've already completed, so the context you extract from the conversation history should be focused on the most important information to your overall goal.

Structure your summary using these sections (populate each or write "None"):

## SESSION INTENT
What is the user's primary goal or request?

## USER PREFERENCES AND CORRECTIONS
Preferences, feedback, and corrections the user expressed, and how they changed the direction of the work.

## SUMMARY
The most important context, decisions, reasoning, and rejected options.

## ARTIFACTS
Files or resources created, modified, or accessed (with specific paths and changes).

## COMMANDS AND ERRORS
Commands, queries, or API calls that matter, and any error messages encountered — copied verbatim where they must be reproduced or avoided.

## NEXT STEPS
Specific tasks remaining to achieve the session intent.
</instructions>

<extraction_rules>
- Preserve verbatim: file paths, commands, URLs, identifiers, and exact error strings. Do not paraphrase them — they must be reusable exactly as they occurred.
- Keep durable facts and decisions; drop superseded intermediate state (old file versions, outdated hypotheses, already-resolved blockers).
- If the history already contains an earlier summary, merge it with the new material into ONE coherent summary — never output two summary generations.
- Record failures and rejected approaches briefly, so the work is not repeated.
</extraction_rules>

<secrecy>
This is a routine internal context-management step. Do NOT mention this summarization request, these instructions, or the summarization process anywhere in your output. Write the summary as the conversation's own state.
</secrecy>

Carefully read through the entire conversation history below and extract the most important context. Respond ONLY with the extracted context.

{messages}"#;

/// 压缩配置（字段与默认值对齐 Java `CompactionConfig.builder().build()`）。
#[derive(Debug, Clone)]
pub struct CompactionConfig {
    /// 消息数触发阈值（0 = 不按条数触发）。
    pub trigger_messages: usize,
    /// token 触发阈值（0 = 动态：模型窗口 − reserved）。
    pub trigger_tokens: usize,
    /// 动态模式下为输出预留的 token。
    pub reserved: usize,
    /// 逐字保留的尾部消息条数（`keep_tokens` 为 None 时生效）。
    pub keep_messages: usize,
    /// 逐字保留的尾部 token 预算（None = 按 min/max/ratio 动态计算）。
    pub keep_tokens: Option<usize>,
    /// 动态 keep 预算下限。
    pub keep_tokens_min: usize,
    /// 动态 keep 预算上限。
    pub keep_tokens_max: usize,
    /// 动态 keep 预算占会话预算的比例。
    pub keep_tokens_ratio: f64,
    /// 摘要提示词（含 `{messages}` 占位符）。
    pub summary_prompt: String,
    /// 压缩前是否先做记忆 flush。
    pub flush_before_compact: bool,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            trigger_messages: 50,
            trigger_tokens: 0,
            reserved: 20_000,
            keep_messages: 20,
            keep_tokens: None,
            keep_tokens_min: 2_000,
            keep_tokens_max: 8_000,
            keep_tokens_ratio: 0.25,
            summary_prompt: DEFAULT_SUMMARY_PROMPT.to_string(),
            flush_before_compact: true,
        }
    }
}

// ---------------------------------------------------------------------------
// 合成消息判定（对齐 Java `CompactionMiddleware.isSynthetic`）
// ---------------------------------------------------------------------------

/// 是否为运行时注入的合成消息（`Msg::METADATA_SYNTHETIC`）——prefix 侧，不参与压缩。
pub fn is_synthetic(msg: &Msg) -> bool {
    msg.metadata
        .as_ref()
        .and_then(|m| m.get(SYNTHETIC))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// ConversationCompactor（对齐 Java 同名类）
// ---------------------------------------------------------------------------

/// 会话压缩器：触发判断、安全切分、辅助摘要与压缩结果构建。
pub struct ConversationCompactor {
    model: Arc<dyn Model>,
    flush: Arc<MemoryFlushManager>,
}

/// 一次压缩的有效参数（对齐 Java `withEffective` 的派生结果）。
struct EffectiveConfig<'a> {
    trigger_tokens: usize,
    keep_tokens: usize,
    base: &'a CompactionConfig,
}

impl ConversationCompactor {
    pub fn new(model: Arc<dyn Model>, flush: Arc<MemoryFlushManager>) -> Self {
        Self { model, flush }
    }

    /// 压缩入口：未触发或压缩失败返回 `None`（调用方保持原状）。
    ///
    /// `conversation` 是已剥离 prefix 的对话消息；`budget` 是本次调用的会话
    /// token 预算（动态模式 = 模型窗口 − reserved，窗口未知 = FALLBACK）。
    pub async fn compact_if_needed(
        &self,
        rc: &RuntimeContext,
        conversation: &[Msg],
        config: &CompactionConfig,
        budget: usize,
    ) -> Result<Option<Vec<Msg>>, agentscope_core::Error> {
        if conversation.is_empty() {
            return Ok(None);
        }
        let trigger = if config.trigger_tokens > 0 {
            config.trigger_tokens.min(budget).max(1)
        } else {
            budget.max(1)
        };
        let dynamic_keep = (budget as f64 * config.keep_tokens_ratio) as usize;
        let keep = config
            .keep_tokens
            .unwrap_or_else(|| config.keep_tokens_max.min(dynamic_keep.max(config.keep_tokens_min)))
            .min(budget / 2);
        let effective = EffectiveConfig {
            trigger_tokens: trigger,
            keep_tokens: keep,
            base: config,
        };

        let total_tokens = estimate_tokens(conversation);
        if !should_compact(conversation, total_tokens, &effective) {
            return Ok(None);
        }

        let cutoff = determine_cutoff_index(conversation, total_tokens, &effective);
        if cutoff == 0 {
            return Ok(None);
        }

        // 旧摘要保留在摘要输入里（链式累积），但排除出 flush 输入避免重复抽取。
        let summary_input = &conversation[..cutoff];
        let tail = &conversation[cutoff..];

        if config.flush_before_compact {
            let flush_input = filter_summary_messages(summary_input);
            // best-effort：flush 失败不阻断压缩（对齐 Java onErrorResume → Mono.empty；
            // harness 无 tracing 依赖，静默降级，与 Java 一致不向调用方传播）。
            let _ = self.flush.flush_memories(rc, &flush_input).await;
        }

        let summary = self.summarize_prefix(summary_input, config).await?;
        let summary_msg = build_summary_message(&summary);
        let mut compacted = Vec::with_capacity(tail.len() + 1);
        compacted.push(summary_msg);
        compacted.extend_from_slice(tail);
        Ok(Some(compacted))
    }

    /// 一次辅助 LLM 调用把 prefix 蒸馏成摘要（对齐 Java `summarizePrefix`）。
    async fn summarize_prefix(
        &self,
        prefix: &[Msg],
        config: &CompactionConfig,
    ) -> Result<String, agentscope_core::Error> {
        if prefix.is_empty() {
            return Ok("No previous conversation history.".to_string());
        }
        let formatted = format_messages_for_summary(prefix);
        let prompt = config.summary_prompt.replace("{messages}", &formatted);
        let request = ChatRequest {
            messages: vec![Msg::user("user", prompt)],
            options: GenerateOptions::default(),
        };
        let response = self.model.call(request).await?;
        let text = response
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
            .trim()
            .to_string();
        if text.is_empty() {
            return Err(agentscope_core::Error::InvalidInput(
                "summary model returned no content".into(),
            ));
        }
        Ok(text)
    }
}

/// 触发判断（对齐 Java `shouldCompact`）。
fn should_compact(messages: &[Msg], total_tokens: usize, config: &EffectiveConfig) -> bool {
    if config.base.trigger_messages > 0 && messages.len() >= config.base.trigger_messages {
        return true;
    }
    if config.trigger_tokens > 0 && total_tokens >= config.trigger_tokens {
        return true;
    }
    false
}

/// 切分点：prefix（待摘要）与 tail（逐字保留）的边界（对齐 Java `determineCutoffIndex`）。
fn determine_cutoff_index(messages: &[Msg], total_tokens: usize, config: &EffectiveConfig) -> usize {
    // keep_tokens 已在 compact_if_needed 里解析（显式值或动态 min/max/ratio），
    // > 0 走 token 预算切分，否则按条数（对齐 Java withEffective + determineCutoffIndex）。
    let raw = if config.keep_tokens > 0 {
        find_token_based_cutoff(messages, total_tokens, config.keep_tokens)
    } else {
        find_message_based_cutoff(messages, config.base.keep_messages)
    };
    find_safe_cutoff_point(messages, raw)
}

/// 最早的 index 使 `messages[index:]` 的 token 数 ≤ keep（二分；对齐 Java）。
fn find_token_based_cutoff(messages: &[Msg], total_tokens: usize, keep_tokens: usize) -> usize {
    if total_tokens <= keep_tokens {
        return 0;
    }
    let mut left = 0usize;
    let mut right = messages.len();
    let mut candidate = messages.len();
    while left < right {
        let mid = (left + right) / 2;
        if estimate_tokens(&messages[mid..]) <= keep_tokens {
            candidate = mid;
            right = mid;
        } else {
            left = mid + 1;
        }
    }
    candidate.min(messages.len() - 1) // 至少保留 1 条
}

/// 逐字保留最后 `keep_messages` 条（对齐 Java `findMessageBasedCutoff`）。
fn find_message_based_cutoff(messages: &[Msg], keep_messages: usize) -> usize {
    if messages.len() <= keep_messages {
        return 0;
    }
    messages.len() - keep_messages
}

/// 回退到安全切分点：绝不把 ASSISTANT 工具调用与其 TOOL 结果拆到两侧
/// （对齐 Java `findSafeCutoffPoint`）。
fn find_safe_cutoff_point(messages: &[Msg], cutoff: usize) -> usize {
    if cutoff == 0 || cutoff >= messages.len() {
        return cutoff;
    }
    if messages[cutoff].role != Role::Tool {
        return cutoff;
    }
    // 收集 cutoff 起连续 TOOL 消息携带的工具结果 id。
    let mut tool_call_ids = Vec::new();
    let mut idx = cutoff;
    while idx < messages.len() && messages[idx].role == Role::Tool {
        for block in &messages[idx].content {
            if let ContentBlock::ToolResult { id, .. } = block {
                tool_call_ids.push(id.clone());
            }
        }
        idx += 1;
    }
    if tool_call_ids.is_empty() {
        return idx; // 无 id：跳过所有 TOOL 消息，避免孤儿结果
    }
    // 向前找发出这些调用的 ASSISTANT 消息，切在它之前。
    for i in (0..cutoff).rev() {
        if messages[i].role == Role::Assistant {
            let issues = messages[i].content.iter().any(|block| {
                matches!(block, ContentBlock::ToolUse { id, .. } if tool_call_ids.contains(id))
            });
            if issues {
                return i;
            }
        }
    }
    idx
}

// ---------------------------------------------------------------------------
// 摘要渲染与摘要消息（对齐 Java `formatMessagesForSummary` / `buildSummaryMessage`）
// ---------------------------------------------------------------------------

const SUMMARY_PREVIEW_CHARS: usize = 600;

/// 把消息渲染成人类可读文本供摘要 LLM 使用（工具调用保留名字与参数预览，
/// 工具结果保留名字、错误状态与首尾预览；对齐 Java 同名方法）。
pub fn format_messages_for_summary(messages: &[Msg]) -> String {
    messages
        .iter()
        .filter(|m| m.role != Role::System)
        .map(render_message_for_summary)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn render_message_for_summary(msg: &Msg) -> String {
    let role_label = match msg.role {
        Role::User => "Human",
        Role::Assistant => "AI",
        Role::Tool => "Tool",
        Role::System => return String::new(),
    };
    let mut parts: Vec<String> = Vec::new();
    for block in &msg.content {
        match block {
            ContentBlock::Text { text } if !text.trim().is_empty() => {
                parts.push(text.trim().to_string());
            }
            ContentBlock::ToolUse { name, input, .. } => {
                parts.push(format!(
                    "[tool_call: {} {}]",
                    name,
                    render_tool_args_for_summary(input)
                ));
            }
            ContentBlock::ToolResult { name, state, output, .. } => {
                let mut desc = format!("[tool_result: {name}");
                if *state == agentscope_core::message::ToolResultState::Error {
                    desc.push_str(" (error)");
                }
                desc.push(']');
                desc.push(' ');
                desc.push_str(&prune_preview(&extract_tool_result_text(output), SUMMARY_PREVIEW_CHARS));
                parts.push(desc);
            }
            _ => {}
        }
    }
    format!("{role_label}: {}", parts.join(" "))
        .trim()
        .to_string()
}

/// 参数预览：整体不超预算原样保留；超了则逐键截断（对齐 Java `renderToolArgsForSummary`）。
fn render_tool_args_for_summary(input: &serde_json::Value) -> String {
    let json = input.to_string();
    if json.chars().count() <= SUMMARY_PREVIEW_CHARS {
        return json;
    }
    match input.as_object() {
        Some(map) => {
            let inner = map
                .iter()
                .map(|(key, value)| format!("\"{key}\":{}", prune_preview(&value.to_string(), SUMMARY_PREVIEW_CHARS)))
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{inner}}}")
        }
        None => prune_preview(&json, SUMMARY_PREVIEW_CHARS),
    }
}

fn extract_tool_result_text(output: &[ContentBlock]) -> String {
    output
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } if !text.trim().is_empty() => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// 首尾保留的预览（对齐 Java `buildPrunePreview`）。
fn prune_preview(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let head = max_chars * 2 / 3;
    let tail = max_chars - head;
    let head_text: String = text.chars().take(head).collect();
    let tail_text: String = text.chars().skip(count - tail).collect();
    format!("{head_text}\n...[omitted {} chars]...\n{tail_text}", count - max_chars)
}

/// 摘要消息：USER 角色 + 固定 name + 内容稳定 id（对齐 Java `buildSummaryMessage`）。
fn build_summary_message(summary: &str) -> Msg {
    let content = format!(
        "You are continuing a conversation that has been summarized.\n\n\
A condensed summary of the conversation so far follows. Treat it as established \
background: continue the work directly — do not recap the summary, do not greet \
the user anew, and do not mention the summarization.\n\n<summary>\n{summary}\n</summary>"
    );
    let mut msg = Msg::user(COMPACTION_SUMMARY_MSG_NAME, content.clone());
    msg.id = format!(
        "{COMPACTION_SUMMARY_MSG_NAME}:{:016x}{:016x}",
        stable_hash(&content),
        content.chars().count()
    );
    msg.metadata = Some(json!({
        "context.summary_version": 1,
        "context.summary_model": "compaction",
    }));
    msg
}

fn stable_hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// 剔除旧摘要消息（flush 输入用；对齐 Java `filterSummaryMessages`）。
fn filter_summary_messages(messages: &[Msg]) -> Vec<Msg> {
    messages
        .iter()
        .filter(|m| m.name != COMPACTION_SUMMARY_MSG_NAME)
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// HarnessAgent 接线（对齐 Java CompactionMiddleware.prepare 的 prefix 分离 + 提交）
// ---------------------------------------------------------------------------

/// turn 入口的压缩检查：读 delegate 状态 → prefix/conversation 分离 → 触发则
/// 压缩并 CAS 写回。任何失败都降级为「本次不压缩」，绝不阻断调用
/// （对齐 Java `prepareCandidate` 的 onErrorResume → Optional.empty）。
pub(crate) async fn maybe_compact_context(
    delegate: &Arc<agentscope_core::agent::ReActAgent>,
    compactor: &ConversationCompactor,
    config: &CompactionConfig,
    rc: &RuntimeContext,
    window: usize,
) -> Result<(), agentscope_core::Error> {
    let session_id = rc.session_id().unwrap_or("default");
    let user_id = rc.user_id();
    let Ok(state) = delegate.agent_state_for(user_id, session_id) else {
        return Ok(()); // EventLog 模式等：管理性访问不可用 → 跳过压缩
    };
    if state.context.is_empty() {
        return Ok(());
    }
    let (prefix, conversation): (Vec<Msg>, Vec<Msg>) = state
        .context
        .iter()
        .cloned()
        .partition(|m| m.role == Role::System || is_synthetic(m));

    let budget = if window > 0 {
        window.saturating_sub(config.reserved).max(1)
    } else {
        FALLBACK_TRIGGER_TOKENS
    };

    match compactor.compact_if_needed(rc, &conversation, config, budget).await {
        Ok(Some(compacted)) => {
            let mut next = state.clone();
            next.context = prefix.into_iter().chain(compacted).collect();
            // CAS 写回：并发变更时保存失败即放弃（对齐 Java canCommit；静默降级）。
            let _ = delegate.save_agent_state_for(user_id, session_id, &next);
        }
        Ok(None) => {}
        Err(_) => {
            // 摘要模型失败等：降级为本次不压缩（对齐 Java prepareCandidate 的
            // onErrorResume → Optional.empty），绝不阻断调用。
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_msg(text: &str) -> Msg {
        Msg::user("user", text)
    }

    fn assistant_tool_call(id: &str) -> Msg {
        Msg {
            id: format!("a-{id}"),
            name: "assistant".into(),
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.into(),
                name: "read_file".into(),
                input: json!({}),
                state: None,
                content: None,
            }],
            metadata: None,
            timestamp: None,
            usage: None,
            generate_reason: None,
        }
    }

    fn tool_result(id: &str) -> Msg {
        Msg {
            id: format!("t-{id}"),
            name: "read_file".into(),
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult {
                id: id.into(),
                name: "read_file".into(),
                output: vec![ContentBlock::Text { text: "ok".into() }],
                state: agentscope_core::message::ToolResultState::Success,
                execution_details: None,
            }],
            metadata: None,
            timestamp: None,
            usage: None,
            generate_reason: None,
        }
    }

    #[test]
    fn token_estimation_matches_java_ratios() {
        // 25 字符 ≈ 10 token（2.5 字符/token）+ 消息开销 5 + name "user" 4 字符 ≈ 2。
        let msgs = vec![user_msg(&"x".repeat(25))];
        assert_eq!(estimate_tokens(&msgs), 17);
        assert_eq!(estimate_tokens(&[]), 0);
    }

    #[test]
    fn message_based_cutoff_keeps_tail() {
        let messages: Vec<Msg> = (0..10).map(|i| user_msg(&format!("m{i}"))).collect();
        assert_eq!(find_message_based_cutoff(&messages, 4), 6);
        assert_eq!(find_message_based_cutoff(&messages, 20), 0);
    }

    #[test]
    fn safe_cutoff_never_splits_tool_pairs() {
        // cutoff 落在 TOOL 结果上 → 回退到发出调用的 ASSISTANT 之前。
        let messages = vec![
            user_msg("q1"),
            assistant_tool_call("call-1"),
            tool_result("call-1"),
            user_msg("q2"),
        ];
        let raw = 2; // 落在 tool_result("call-1")
        assert_eq!(find_safe_cutoff_point(&messages, raw), 1);
        // cutoff 落在普通 USER 消息上 → 原样。
        assert_eq!(find_safe_cutoff_point(&messages, 3), 3);
        assert_eq!(find_safe_cutoff_point(&messages, 0), 0);
    }

    #[test]
    fn summary_message_is_stable_and_named() {
        let a = build_summary_message("hello");
        let b = build_summary_message("hello");
        assert_eq!(a.id, b.id, "同内容摘要 id 稳定");
        assert_eq!(a.name, COMPACTION_SUMMARY_MSG_NAME);
        assert_eq!(a.role, Role::User);
        assert!(a.text().contains("<summary>"));
    }

    #[test]
    fn filter_summary_messages_drops_only_summaries() {
        let msgs = vec![build_summary_message("s"), user_msg("q")];
        let filtered = filter_summary_messages(&msgs);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].text(), "q");
    }

    #[test]
    fn synthetic_detection() {
        let mut msg = user_msg("reminder");
        msg.metadata = Some(json!({ (SYNTHETIC): true }));
        assert!(is_synthetic(&msg));
        assert!(!is_synthetic(&user_msg("plain")));
    }

    // ---------- 端到端（compact_if_needed，MockModel 固定响应） ----------

    use agentscope_core::model::ChatResponse;
    use std::sync::Mutex;

    /// 固定响应序列的 Mock 模型（记录收到的请求供断言；pop 为 LIFO，响应倒序给）。
    struct MockModel {
        responses: Mutex<Vec<ChatResponse>>,
        requests: Mutex<Vec<ChatRequest>>,
    }

    impl MockModel {
        fn new(responses: Vec<ChatResponse>) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(responses),
                requests: Mutex::new(Vec::new()),
            })
        }

        fn requests(&self) -> Vec<ChatRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl Model for MockModel {
        async fn call(&self, request: ChatRequest) -> agentscope_core::Result<ChatResponse> {
            self.requests.lock().unwrap().push(request);
            self.responses
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| agentscope_core::Error::Model("no more mock responses".into()))
        }
    }

    fn flush_manager(model: Arc<dyn Model>) -> Arc<MemoryFlushManager> {
        let fs: Arc<dyn crate::workspace::Filesystem> =
            Arc::new(crate::filesystem::InMemoryFilesystem::new());
        Arc::new(MemoryFlushManager::new(
            Arc::new(crate::workspace::WorkspaceManager::with_filesystem("test-ws", fs)),
            model,
        ))
    }

    #[tokio::test]
    async fn compacts_when_message_threshold_reached() {
        let model = MockModel::new(vec![ChatResponse::from_text(
            "## SESSION INTENT\nTest goal.",
        )]);
        let compactor = ConversationCompactor::new(model.clone(), flush_manager(model.clone()));
        let config = CompactionConfig {
            trigger_messages: 4,
            keep_tokens: Some(15), // 每条 ~11 token：tail 只装得下 1 条
            flush_before_compact: false,
            ..Default::default()
        };
        let conversation: Vec<Msg> = (0..6).map(|i| user_msg(&format!("message {i}"))).collect();
        let rc = RuntimeContext::new();
        let compacted = compactor
            .compact_if_needed(&rc, &conversation, &config, 100_000)
            .await
            .unwrap()
            .expect("threshold reached, should compact");
        // summary + 1 条 tail（keep 预算 30 token 只够最后 1 条）
        assert_eq!(compacted.len(), 2);
        assert_eq!(compacted[0].name, COMPACTION_SUMMARY_MSG_NAME);
        assert!(compacted[0].text().contains("Test goal."));
        assert_eq!(compacted[1].text(), "message 5");
        // 摘要请求只含 prefix（前 4 条），不含 tail
        let reqs = model.requests();
        assert_eq!(reqs.len(), 1);
        let prompt = reqs[0].messages[0].text();
        assert!(prompt.contains("message 0"));
        assert!(prompt.contains("message 3"));
        assert!(!prompt.contains("message 5"));
    }

    #[tokio::test]
    async fn no_compaction_below_threshold() {
        let model = MockModel::new(vec![]);
        let compactor = ConversationCompactor::new(model.clone(), flush_manager(model.clone()));
        let config = CompactionConfig {
            trigger_messages: 50,
            flush_before_compact: false,
            ..Default::default()
        };
        let conversation = vec![user_msg("a"), user_msg("b")];
        let rc = RuntimeContext::new();
        let result = compactor
            .compact_if_needed(&rc, &conversation, &config, 100_000)
            .await
            .unwrap();
        assert!(result.is_none());
        assert!(model.requests().is_empty(), "未触发不应调用摘要模型");
    }

    #[tokio::test]
    async fn empty_summary_response_is_error_not_panic() {
        let model = MockModel::new(vec![ChatResponse::from_text("   ")]);
        let compactor = ConversationCompactor::new(model.clone(), flush_manager(model.clone()));
        let config = CompactionConfig {
            trigger_messages: 2,
            keep_tokens: Some(15),
            flush_before_compact: false,
            ..Default::default()
        };
        let conversation = vec![user_msg("a"), user_msg("b")];
        let rc = RuntimeContext::new();
        let result = compactor
            .compact_if_needed(&rc, &conversation, &config, 100_000)
            .await;
        assert!(result.is_err(), "空摘要响应应报错（调用方降级为不压缩）");
    }

    #[tokio::test]
    async fn message_trigger_with_roomy_token_budget_is_noop() {
        // 对齐 Java：cutoff<=0 时返回 lightweight result（不压缩）——
        // 消息数触发但 keep 预算装得下全部对话时，摘要反而会丢信息，不做。
        let model = MockModel::new(vec![]);
        let compactor = ConversationCompactor::new(model.clone(), flush_manager(model.clone()));
        let config = CompactionConfig {
            trigger_messages: 4,
            flush_before_compact: false,
            ..Default::default()
        };
        let conversation: Vec<Msg> = (0..6).map(|i| user_msg(&format!("message {i}"))).collect();
        let rc = RuntimeContext::new();
        let result = compactor
            .compact_if_needed(&rc, &conversation, &config, 100_000)
            .await
            .unwrap();
        assert!(result.is_none());
        assert!(model.requests().is_empty());
    }

    #[tokio::test]
    async fn flush_runs_before_summary_when_enabled() {
        // flush 先调（pop 到第二个响应），摘要后调（pop 到第一个响应）。
        let model = MockModel::new(vec![
            ChatResponse::from_text("## SESSION INTENT\nGoal."),
            ChatResponse::from_text("- user asked things"),
        ]);
        let compactor = ConversationCompactor::new(model.clone(), flush_manager(model.clone()));
        let config = CompactionConfig {
            trigger_messages: 2,
            keep_tokens: Some(15),
            flush_before_compact: true,
            ..Default::default()
        };
        let conversation = vec![user_msg("a"), user_msg("b")];
        let rc = RuntimeContext::new();
        let compacted = compactor
            .compact_if_needed(&rc, &conversation, &config, 100_000)
            .await
            .unwrap()
            .expect("should compact");
        assert_eq!(compacted[0].name, COMPACTION_SUMMARY_MSG_NAME);
        assert!(compacted[0].text().contains("Goal."));
        assert_eq!(model.requests().len(), 2, "flush + 摘要各一次模型调用");
    }
}
