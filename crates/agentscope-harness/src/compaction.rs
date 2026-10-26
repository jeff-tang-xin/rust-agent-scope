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
//! 与 Java 一致的核心语义（续）：
//! - 粒度：经 [`agentscope_core::agent::IterationHook`] 挂在每轮 ReAct 迭代边界
//!   （reasoning 之前），与 Java 每轮 `onReasoning` 检查对齐；模型报上下文溢出时
//!   以 forced 配置（trigger=1/keep=1/keepTokens=0）强制压缩并同轮重试一次
//!   （对齐 Java `overflowRecovery`，溢出判定见
//!   [`agentscope_core::agent::is_context_overflow_error`]）；
//! - 轻量裁剪：摘要前先做非 LLM 的 `truncate_args`（旧消息超大工具参数截断）与
//!   `prune_tool_results`（旧 tool-result 首尾预览替换），裁剪后未触发摘要也
//!   会提交裁剪结果（对齐 Java `compactStandard` 的 Step 1a/1b 与 lightweight
//!   result）。
//!
//! 与 Java 的有意差异（lib.rs 差异声明同步维护）：
//! - `ToolResultEvictionMiddleware`（超大 tool-result 落盘为文件引用）未复刻；
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
    /// 摘要前的工具参数截断（Step 1a，对齐 Java `TruncateArgsConfig`；None = 关闭）。
    pub truncate_args: Option<TruncateArgsConfig>,
    /// 摘要前的 tool-result 剪枝（Step 1b，对齐 Java `PruneConfig`；None = 关闭）。
    pub prune: Option<PruneConfig>,
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
            truncate_args: Some(TruncateArgsConfig::default()),
            prune: Some(PruneConfig::default()),
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
// 轻量裁剪配置（对齐 Java `CompactionConfig.TruncateArgsConfig` / `PruneConfig`）
// ---------------------------------------------------------------------------

/// 工具参数截断配置（Step 1a；字段与默认值对齐 Java `TruncateArgsConfig`）。
///
/// 独立于摘要触发的更低阈值：达到触发条件后，keep 窗口之前的 ASSISTANT 消息里
/// 超长的工具参数值被替换为「前 20 字符 + 截断标记」。
#[derive(Debug, Clone)]
pub struct TruncateArgsConfig {
    /// 消息数触发阈值（0 = 不按条数触发）。
    pub trigger_messages: usize,
    /// token 触发阈值（0 = 不按 token 触发）。
    pub trigger_tokens: usize,
    /// 逐字保留的尾部消息条数（`keep_tokens` 为 0 时生效）。
    pub keep_messages: usize,
    /// 逐字保留的尾部 token 预算（> 0 时优先于条数）。
    pub keep_tokens: usize,
    /// 单个工具参数值的最大字符数。
    pub max_arg_length: usize,
    /// 截断后追加的标记文本。
    pub truncation_text: String,
}

impl Default for TruncateArgsConfig {
    fn default() -> Self {
        Self {
            trigger_messages: 25,
            trigger_tokens: 40_000,
            keep_messages: 20,
            keep_tokens: 0,
            max_arg_length: 2_000,
            truncation_text: "...(argument truncated)".to_string(),
        }
    }
}

/// tool-result 剪枝配置（Step 1b；字段与默认值对齐 Java `PruneConfig`）。
///
/// 从尾部向前保护最近 `protect_tokens` 的工具输出；更早的超长结果替换为
/// 首尾预览，可剪总量达到 `minimum_tokens` 才实际执行。
#[derive(Debug, Clone)]
pub struct PruneConfig {
    /// 最近工具输出的保护 token 预算（永不剪枝）。
    pub protect_tokens: usize,
    /// 可剪 token 总量达到该值才执行剪枝。
    pub minimum_tokens: usize,
    /// 剪枝后保留的首尾预览字符数。
    pub max_output_chars: usize,
    /// 永不剪枝的工具名（如 read_file——剪掉会导致重读循环）。
    pub excluded_tools: Vec<String>,
}

impl Default for PruneConfig {
    fn default() -> Self {
        Self {
            protect_tokens: 40_000,
            minimum_tokens: 20_000,
            max_output_chars: 2_000,
            excluded_tools: vec![
                "read_file".to_string(),
                "memory_search".to_string(),
                "memory_get".to_string(),
                "session_search".to_string(),
            ],
        }
    }
}

/// 工具参数截断（Step 1a，对齐 Java `ConversationCompactor.truncateArgs`）。
///
/// 达到触发条件后，keep 窗口之前的 ASSISTANT 消息里超长的字符串参数值被
/// 截断；无改动时原样返回。
pub fn truncate_args(messages: &[Msg], config: &TruncateArgsConfig) -> Vec<Msg> {
    let total_tokens = estimate_tokens(messages);
    let triggered = (config.trigger_messages > 0 && messages.len() >= config.trigger_messages)
        || (config.trigger_tokens > 0 && total_tokens >= config.trigger_tokens);
    if !triggered {
        return messages.to_vec();
    }
    let cutoff = if config.keep_tokens > 0 {
        // token 预算 keep 窗口：从尾扫描（对齐 Java `determineTruncateCutoff`）
        let mut kept = 0usize;
        let mut cut = 0usize;
        for i in (0..messages.len()).rev() {
            let t = estimate_tokens(&messages[i..i + 1]);
            if kept + t > config.keep_tokens {
                cut = i + 1;
                break;
            }
            kept += t;
        }
        cut
    } else {
        messages.len().saturating_sub(config.keep_messages)
    };
    let mut result = Vec::with_capacity(messages.len());
    let mut changed = false;
    for (i, msg) in messages.iter().enumerate() {
        if i < cutoff && msg.role == Role::Assistant {
            match truncate_tool_use_args(msg, config) {
                Some(truncated) => {
                    changed = true;
                    result.push(truncated);
                }
                None => result.push(msg.clone()),
            }
        } else {
            result.push(msg.clone());
        }
    }
    if changed {
        result
    } else {
        messages.to_vec()
    }
}

/// 截断一条 ASSISTANT 消息里的超长工具参数值；无改动返回 `None`（对齐 Java
/// `truncateToolUseArgs`：只处理字符串值，前 20 字符 + 截断标记）。
fn truncate_tool_use_args(msg: &Msg, config: &TruncateArgsConfig) -> Option<Msg> {
    let mut changed = false;
    let mut blocks = msg.content.clone();
    for block in blocks.iter_mut() {
        if let ContentBlock::ToolUse { input, .. } = block {
            if truncate_input_value(input, config) {
                changed = true;
            }
        }
    }
    if !changed {
        return None;
    }
    let mut next = msg.clone();
    next.content = blocks;
    Some(next)
}

/// 就地截断 input 对象里的超长字符串值（对齐 Java `truncateToolUseBlock`：
/// 只截顶层字符串值，非对象/嵌套结构不动）。
fn truncate_input_value(input: &mut serde_json::Value, config: &TruncateArgsConfig) -> bool {
    let serde_json::Value::Object(map) = input else {
        return false;
    };
    let mut changed = false;
    for value in map.values_mut() {
        if let serde_json::Value::String(text) = value {
            if text.chars().count() > config.max_arg_length {
                let head: String = text.chars().take(20).collect();
                *value = serde_json::Value::String(format!("{head}{}", config.truncation_text));
                changed = true;
            }
        }
    }
    changed
}

/// tool-result 剪枝（Step 1b，对齐 Java `ConversationCompactor.pruneToolResults`）。
///
/// 从尾部向前保护最近 `protect_tokens` 的工具输出；更早的超长结果替换为
/// 首尾预览。可剪总量达到 `minimum_tokens` 才实际执行，否则原样返回。
pub fn prune_tool_results(messages: &[Msg], config: &PruneConfig) -> Vec<Msg> {
    let mut protected = 0usize;
    let mut prunable = 0usize;
    let mut to_prune: Vec<(usize, usize)> = Vec::new();
    for mi in (0..messages.len()).rev() {
        let msg = &messages[mi];
        if msg.role != Role::Tool {
            continue;
        }
        for bi in (0..msg.content.len()).rev() {
            let ContentBlock::ToolResult { name, output, .. } = &msg.content[bi] else {
                continue;
            };
            if config.excluded_tools.iter().any(|tool| tool == name) {
                continue;
            }
            let text = extract_tool_result_text(output);
            if text.trim().is_empty() {
                continue;
            }
            // 只计这一块的 token（对齐 Java `msg.withContent(List.of(tr))` 的单块估算）
            let mut single = msg.clone();
            single.content = vec![msg.content[bi].clone()];
            let tokens = estimate_tokens(&[single]);
            if protected < config.protect_tokens {
                protected += tokens;
                continue;
            }
            if text.chars().count() > config.max_output_chars {
                prunable += tokens;
                to_prune.push((mi, bi));
            }
        }
    }
    if prunable < config.minimum_tokens || to_prune.is_empty() {
        return messages.to_vec();
    }
    let mut result = messages.to_vec();
    for (mi, bi) in to_prune {
        let msg = &mut result[mi];
        if let ContentBlock::ToolResult { output, .. } = &mut msg.content[bi] {
            let text = extract_tool_result_text(output);
            let preview = prune_preview_half(&text, config.max_output_chars);
            *output = vec![ContentBlock::Text { text: preview }];
        }
    }
    result
}

/// 首尾各半的剪枝预览（对齐 Java `buildPrunePreview`——注意与摘要渲染用的
/// [`prune_preview`]（2/3 + 1/3）是两个不同用途的实现）。
fn prune_preview_half(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let half = max_chars / 2;
    let head: String = text.chars().take(half).collect();
    let tail: String = text.chars().skip(count - half).collect();
    // count > max_chars 保证 removed > 0（Java 侧是 int 才需要显式判断）
    let removed = count - head.chars().count() - tail.chars().count();
    format!("{head}\n\n...({removed} chars pruned)...\n\n{tail}")
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

        // Step 1a/1b：非 LLM 轻量裁剪（对齐 Java `compactStandard` 开头的
        // pruneToolResults(truncateArgs(...))）；裁剪结果参与后续触发判断与切分。
        let mut working: Vec<Msg> = conversation.to_vec();
        if let Some(cfg) = &config.truncate_args {
            working = truncate_args(&working, cfg);
        }
        if let Some(cfg) = &config.prune {
            working = prune_tool_results(&working, cfg);
        }
        let lightweight = working != conversation;

        let total_tokens = estimate_tokens(&working);
        if !should_compact(&working, total_tokens, &effective) {
            // 未触发摘要：裁剪有改动则提交裁剪结果（对齐 Java lightweight result）
            return Ok(lightweight.then_some(working));
        }

        let cutoff = determine_cutoff_index(&working, total_tokens, &effective);
        if cutoff == 0 {
            return Ok(lightweight.then_some(working));
        }

        // 旧摘要保留在摘要输入里（链式累积），但排除出 flush 输入避免重复抽取。
        let summary_input = &working[..cutoff];
        let tail = &working[cutoff..];

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
// 迭代边界接线（对齐 Java CompactionMiddleware.onReasoning 每轮 prepare）
// ---------------------------------------------------------------------------

/// 迭代边界的压缩检查：直接读写本次调用的活状态（对话真相），不走管理性
/// 访问——EventLog 模式下也可用。任何失败都降级为「本次不压缩」，绝不阻断
/// 调用（对齐 Java `prepareCandidate` 的 onErrorResume → Optional.empty）。
///
/// `forced=true`（overflow 恢复，见
/// [`agentscope_core::agent::is_context_overflow_error`]）时用最激进配置
/// （trigger=1/keep=1/keepTokens=0，对齐 Java `overflowRecovery`）。
pub(crate) async fn compact_in_state(
    rc: &RuntimeContext,
    compactor: &ConversationCompactor,
    config: &CompactionConfig,
    state: &std::sync::Arc<std::sync::Mutex<agentscope_core::state::AgentState>>,
    window: usize,
    forced: bool,
) {
    let snapshot: Vec<Msg> = state
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .context
        .clone();
    if snapshot.is_empty() {
        return;
    }
    let (prefix, conversation): (Vec<Msg>, Vec<Msg>) = snapshot
        .iter()
        .cloned()
        .partition(|m| m.role == Role::System || is_synthetic(m));

    let budget = if window > 0 {
        window.saturating_sub(config.reserved).max(1)
    } else {
        FALLBACK_TRIGGER_TOKENS
    };
    let effective = if forced {
        CompactionConfig {
            trigger_messages: 1,
            keep_messages: 1,
            keep_tokens: Some(0),
            ..config.clone()
        }
    } else {
        config.clone()
    };

    match compactor
        .compact_if_needed(rc, &conversation, &effective, budget)
        .await
    {
        Ok(Some(compacted)) => {
            let mut guard = state.lock().unwrap_or_else(|error| error.into_inner());
            // canCommit：仅当 context 未被并发改动（对齐 Java prepare 的 equals 检查）
            if guard.context == snapshot {
                let mut next = prefix;
                next.extend(compacted);
                guard.context = next;
            }
        }
        _ => {
            // 未触发 / 摘要模型失败等：降级为本次不压缩（对齐 Java
            // prepareCandidate 的 onErrorResume → Optional.empty）。
        }
    }
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

    // ---- Step 1a/1b：轻量裁剪（对齐 Java truncateArgs / pruneToolResults）----

    fn assistant_with_long_arg(id: &str, chars: usize) -> Msg {
        Msg {
            id: format!("a-{id}"),
            name: "assistant".into(),
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: format!("call_{id}"),
                name: "read_file".into(),
                input: json!({"path": "x".repeat(chars)}),
                state: None,
                content: None,
            }],
            metadata: None,
            timestamp: None,
            usage: None,
            generate_reason: None,
        }
    }

    fn tool_result_with_text(id: &str, name: &str, text: &str) -> Msg {
        Msg {
            id: format!("t-{id}"),
            name: name.into(),
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult {
                id: format!("call_{id}"),
                name: name.into(),
                output: vec![ContentBlock::Text { text: text.into() }],
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
    fn truncate_args_truncates_long_values_before_keep_window() {
        // 5 条带超长参数的 ASSISTANT + 20 条 user = 25 条 → 触发（trigger_messages=25）
        // keep_messages=20 → cutoff=5 → 前 5 条 ASSISTANT 的参数被截断
        let mut messages: Vec<Msg> = (0..5)
            .map(|i| assistant_with_long_arg(&i.to_string(), 3_000))
            .collect();
        messages.extend((0..20).map(|i| user_msg(&format!("m{i}"))));
        let config = TruncateArgsConfig::default();
        let result = truncate_args(&messages, &config);
        assert_eq!(result.len(), 25);
        for msg in result.iter().take(5) {
            let ContentBlock::ToolUse { input, .. } = &msg.content[0] else {
                panic!("expect tool use");
            };
            let path = input["path"].as_str().unwrap();
            assert_eq!(path, format!("{}{}", "x".repeat(20), config.truncation_text));
        }
        // keep 窗口内的消息不动
        for i in 5..25 {
            assert_eq!(result[i], messages[i]);
        }
    }

    #[test]
    fn truncate_args_noop_below_trigger() {
        let messages = vec![assistant_with_long_arg("0", 3_000), user_msg("hi")];
        let config = TruncateArgsConfig::default();
        let result = truncate_args(&messages, &config);
        assert_eq!(result, messages);
    }

    #[test]
    fn prune_tool_results_protects_recent_and_prunes_old() {
        // 从尾向前保护最近 protect_tokens=100：尾两条（各 ~85 token）受保护，
        // 第 1 条可剪（85 ≥ minimum 20）→ 替换为首尾预览
        let big = "y".repeat(200);
        let messages = vec![
            tool_result_with_text("0", "search", &big),
            tool_result_with_text("1", "search", &big),
            tool_result_with_text("2", "search", &big),
        ];
        let config = PruneConfig {
            protect_tokens: 150, // 单块 ~102 token：尾两条累计 204 超预算，仅尾两条受保护
            minimum_tokens: 20,
            max_output_chars: 100,
            excluded_tools: vec![],
        };
        let result = prune_tool_results(&messages, &config);
        assert_eq!(result.len(), 3);
        let ContentBlock::ToolResult { output, .. } = &result[0].content[0] else {
            panic!("expect tool result");
        };
        let preview = match output.as_slice() {
            [ContentBlock::Text { text }] => text.clone(),
            other => panic!("expect single text, got {other:?}"),
        };
        assert!(preview.contains("chars pruned"));
        assert!(preview.starts_with('y') && preview.ends_with('y'));
        // 受保护的尾两条原样
        assert_eq!(result[1], messages[1]);
        assert_eq!(result[2], messages[2]);
    }

    #[test]
    fn prune_tool_results_skips_excluded_and_below_minimum() {
        let big = "y".repeat(200);
        // excluded 工具不剪
        let excluded = vec![tool_result_with_text("0", "read_file", &big)];
        let config = PruneConfig {
            protect_tokens: 0,
            minimum_tokens: 1,
            max_output_chars: 100,
            excluded_tools: vec!["read_file".into()],
        };
        assert_eq!(prune_tool_results(&excluded, &config), excluded);
        // 可剪总量低于 minimum_tokens 不剪
        let small = vec![tool_result_with_text("0", "search", &"y".repeat(30))];
        let config2 = PruneConfig {
            protect_tokens: 0,
            minimum_tokens: 10_000,
            max_output_chars: 10,
            excluded_tools: vec![],
        };
        assert_eq!(prune_tool_results(&small, &config2), small);
    }

    #[tokio::test]
    async fn compact_if_needed_submits_lightweight_prune_result() {
        // 摘要阈值（50）不触发，但参数截断阈值（5）触发 → 只提交裁剪结果，
        // 不调用摘要模型（对齐 Java lightweight result）
        let model = MockModel::new(vec![]);
        let compactor = ConversationCompactor::new(model.clone(), flush_manager(model.clone()));
        let config = CompactionConfig {
            trigger_messages: 50,
            flush_before_compact: false,
            truncate_args: Some(TruncateArgsConfig {
                trigger_messages: 5,
                keep_messages: 2,
                ..Default::default()
            }),
            prune: None,
            ..Default::default()
        };
        let mut conversation: Vec<Msg> = (0..4)
            .map(|i| assistant_with_long_arg(&i.to_string(), 3_000))
            .collect();
        conversation.extend([user_msg("m4"), user_msg("m5")]);
        let rc = RuntimeContext::new();
        let result = compactor
            .compact_if_needed(&rc, &conversation, &config, 100_000)
            .await
            .unwrap()
            .expect("lightweight prune result should be submitted");
        assert_eq!(result.len(), 6);
        for msg in result.iter().take(4) {
            let ContentBlock::ToolUse { input, .. } = &msg.content[0] else {
                panic!("expect tool use");
            };
            assert!(input["path"].as_str().unwrap().contains("argument truncated"));
        }
        assert!(model.requests().is_empty(), "lightweight 路径不应调用摘要模型");
    }

    // ---- compact_in_state：迭代边界接线（对齐 Java 每轮 onReasoning prepare）----

    fn state_with(context: Vec<Msg>) -> std::sync::Arc<std::sync::Mutex<agentscope_core::state::AgentState>> {
        std::sync::Arc::new(std::sync::Mutex::new(agentscope_core::state::AgentState {
            context,
            ..Default::default()
        }))
    }

    #[tokio::test]
    async fn compact_in_state_writes_back_on_trigger() {
        let model = MockModel::new(vec![ChatResponse::from_text(
            "## SESSION INTENT\nTest goal.",
        )]);
        let compactor = ConversationCompactor::new(model.clone(), flush_manager(model.clone()));
        let config = CompactionConfig {
            trigger_messages: 4,
            keep_tokens: Some(15),
            flush_before_compact: false,
            ..Default::default()
        };
        let state = state_with((0..6).map(|i| user_msg(&format!("message {i}"))).collect());
        let rc = RuntimeContext::new();
        compact_in_state(&rc, &compactor, &config, &state, 100_000, false).await;
        let guard = state.lock().unwrap();
        assert_eq!(guard.context.len(), 2);
        assert_eq!(guard.context[0].name, COMPACTION_SUMMARY_MSG_NAME);
        assert_eq!(guard.context[1].text(), "message 5");
    }

    #[tokio::test]
    async fn compact_in_state_forced_compacts_below_trigger() {
        // forced=true（overflow 恢复）：即使低于常规触发阈值也强制压缩到 summary+1
        let model = MockModel::new(vec![ChatResponse::from_text(
            "## SESSION INTENT\nForced goal.",
        )]);
        let compactor = ConversationCompactor::new(model.clone(), flush_manager(model.clone()));
        let config = CompactionConfig {
            trigger_messages: 50, // 常规阈值远未达到
            flush_before_compact: false,
            ..Default::default()
        };
        let state = state_with((0..6).map(|i| user_msg(&format!("message {i}"))).collect());
        let rc = RuntimeContext::new();
        compact_in_state(&rc, &compactor, &config, &state, 100_000, true).await;
        let guard = state.lock().unwrap();
        assert_eq!(guard.context.len(), 2);
        assert_eq!(guard.context[0].name, COMPACTION_SUMMARY_MSG_NAME);
        assert!(guard.context[0].text().contains("Forced goal."));
        assert_eq!(guard.context[1].text(), "message 5");
    }

    #[tokio::test]
    async fn compact_in_state_noop_below_trigger_without_forced() {
        let model = MockModel::new(vec![]);
        let compactor = ConversationCompactor::new(model.clone(), flush_manager(model.clone()));
        let config = CompactionConfig {
            trigger_messages: 50,
            flush_before_compact: false,
            ..Default::default()
        };
        let state = state_with(vec![user_msg("a"), user_msg("b")]);
        let rc = RuntimeContext::new();
        compact_in_state(&rc, &compactor, &config, &state, 100_000, false).await;
        assert_eq!(state.lock().unwrap().context.len(), 2);
        assert!(model.requests().is_empty());
    }
}
