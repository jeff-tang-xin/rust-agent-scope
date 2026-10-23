//! 长期记忆管线（对应 Java harness `agent.memory` 包）。
//!
//! 两层记忆模型：
//! - **Layer 1 — 每日台账**：`memory/YYYY-MM-DD.md`，只追加，由
//!   [`MemoryFlushManager`]（每次调用后的抽取）与 `memory_save` 工具写入。
//! - **Layer 2 — 策展 MEMORY.md**：由 [`MemoryConsolidator`] 周期性把每日
//!   台账合并/去重/裁剪进 `MEMORY.md`，是跨天跨会话知识的唯一权威。
//!
//! [`MemoryConfig`] 统一配置 flush（抽取）与 consolidation（合并）两条
//! LLM 驱动的管线；[`MemoryBackgroundTasks`] 跟踪在途后台任务供关闭时等待静默。

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use agentscope_core::message::ContentBlock;
use agentscope_core::model::{ChatRequest, ChatResponse, GenerateOptions, Model};
use agentscope_core::{Error, Msg, Result, Role, RuntimeContext};

use crate::coordination::IsolationScope;
use crate::workspace::constants::{MEMORY_DIR, MEMORY_MD};
use crate::workspace::WorkspaceManager;

/// 默认 consolidation token 预算（对齐 Java `DEFAULT_CONSOLIDATION_MAX_TOKENS`）。
pub const DEFAULT_CONSOLIDATION_MAX_TOKENS: usize = 4_000;
/// 默认 consolidation 最小间隔（对齐 Java `DEFAULT_CONSOLIDATION_MIN_GAP`）。
pub const DEFAULT_CONSOLIDATION_MIN_GAP: Duration = Duration::from_secs(30 * 60);
/// 每日台账归档前的保留天数（对齐 Java `DEFAULT_DAILY_FILE_RETENTION_DAYS`）。
pub const DEFAULT_DAILY_FILE_RETENTION_DAYS: u32 = 90;

/// 隐藏的状态文件：记录上次成功 consolidation 的时刻（ISO-8601）。
pub const CONSOLIDATION_STATE_FILE: &str = "memory/.consolidation_state";

/// compaction 摘要消息的 name（flush 序列化时跳过；对齐 Java
/// `ConversationCompactor.SUMMARY_MSG_NAME`）。
pub const COMPACTION_SUMMARY_MSG_NAME: &str = "compaction_summary";

/// 默认 flush 抽取提示词（对齐 Java `MemoryFlushManager.DEFAULT_FLUSH_PROMPT`）。
pub const DEFAULT_FLUSH_PROMPT: &str = r#"You are a memory extraction assistant. Analyze the conversation below and extract important facts, decisions, preferences, and contextual information that should be remembered for future conversations.

Output ONLY the extracted memories as a markdown bullet list. Each item should be a concise, self-contained fact. Include dates, names, and specifics when available.

If there is nothing worth remembering, respond with exactly: NO_REPLY

Guidelines:
- Extract user preferences, personal information, project decisions
- Capture important technical decisions and their rationale
- Note any commitments, deadlines, or action items
- Record relationship context (who works on what, team structure)
- Ignore routine greetings, tool invocations, and ephemeral status updates

IMPORTANT — write target and append-only rules:
- You are writing to TODAY'S daily memory ledger (memory/YYYY-MM-DD.md), NOT to MEMORY.md. The daily ledger is append-only — your output will be appended after the entries already shown below.
- MEMORY.md is the curated long-term memory and is shown ONLY as read-only context. Do NOT restate facts already covered by MEMORY.md or by today's earlier entries; a separate consolidation step periodically merges new daily entries into MEMORY.md.
- Keep each bullet point independent and self-contained so entries can be searched individually."#;

/// 默认 consolidation 提示词模板：恰好两个 `%d` 占位符（max-tokens、max-chars）。
pub const DEFAULT_CONSOLIDATION_PROMPT: &str = r#"You are a memory consolidation assistant. You own the curated long-term memory file MEMORY.md. Your job is to merge new daily ledger entries into MEMORY.md while keeping it concise, deduplicated, and high-signal.

You are given two inputs:
1. The current MEMORY.md content (the existing curated long-term memory).
2. New daily ledger entries that have been appended since the last consolidation.

Rules:
- MEMORY.md is the single source of truth for cross-day, cross-session knowledge. Keep it stable and authoritative.
- Daily ledger entries are stream-of-consciousness flush logs — they may be noisy, redundant with MEMORY.md, or redundant with each other. Promote only what is durable and reusable.
- Deduplicate: if a new entry restates something MEMORY.md already covers, skip it.
- Merge related facts: combine entries about the same topic into cohesive paragraphs with clear section headers.
- Update or remove stale information when new entries supersede it.
- Keep total output within %d tokens (approximately %d characters); prioritize recent and frequently-referenced information when trimming.

Output the COMPLETE new MEMORY.md content (not just a diff). Use markdown."#;

/// 统计 `haystack` 中 `needle` 的出现次数。
fn count_occurrences(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

/// 当前 UTC 日期（`YYYY-MM-DD`；civil date 算法，不引 chrono）。
pub fn today_iso_date() -> String {
    let secs = crate::coordination::now_epoch_millis().div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// 当前时刻的 ISO-8601 UTC 字符串（秒级精度）。
fn now_iso8601() -> String {
    millis_to_iso8601(crate::coordination::now_epoch_millis())
}

// ---------------------------------------------------------------------------
// MemoryConfig
// ---------------------------------------------------------------------------

/// 每次 agent 调用后的 flush 触发策略（对齐 Java `MemoryConfig.FlushTrigger`）。
///
/// 节流策略首次符合条件的调用立即放行，最小间隔只约束后续两次 flush 之间。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlushTrigger {
    /// 每次 agent 调用后都 flush（默认）。
    Always,
    /// 禁用每次调用的 flush（compaction / 溢出恢复路径不受影响）。
    Never,
    /// 每 `min_gap` 至多 flush 一次；`Duration::ZERO` 归一为 [`FlushTrigger::Always`]。
    Throttled(Duration),
}

impl FlushTrigger {
    /// 构造节流触发器：负间隔报错，零间隔归一为 Always。
    pub fn throttled(min_gap: Duration) -> Result<Self> {
        if min_gap.is_zero() {
            return Ok(FlushTrigger::Always);
        }
        Ok(FlushTrigger::Throttled(min_gap))
    }
}

/// 长期记忆管线统一配置（flush + consolidation；对齐 Java `MemoryConfig`）。
#[derive(Clone)]
pub struct MemoryConfig {
    /// 记忆操作（flush + consolidation）的专用模型；`None` 用 agent 主模型。
    pub model: Option<Arc<dyn Model>>,
    /// flush 抽取提示词；`None` 用 [`DEFAULT_FLUSH_PROMPT`]。
    pub flush_prompt: Option<String>,
    /// consolidation 提示词模板（恰好两个 `%d`）；`None` 用默认。
    pub consolidation_prompt: Option<String>,
    /// consolidation token 预算。
    pub consolidation_max_tokens: usize,
    /// consolidation/维护运行的最小间隔（首次立即，非初始延迟）。
    pub consolidation_min_gap: Duration,
    /// 每日台账归档前的保留天数。
    pub daily_file_retention_days: u32,
    /// 每次 agent 调用后的 flush 触发策略。
    pub flush_trigger: FlushTrigger,
    /// 节流与维护的隔离维度（对齐 Java `MemoryConfig.isolationScope`，默认 User）。
    pub isolation_scope: IsolationScope,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            model: None,
            flush_prompt: None,
            consolidation_prompt: None,
            consolidation_max_tokens: DEFAULT_CONSOLIDATION_MAX_TOKENS,
            consolidation_min_gap: DEFAULT_CONSOLIDATION_MIN_GAP,
            daily_file_retention_days: DEFAULT_DAILY_FILE_RETENTION_DAYS,
            flush_trigger: FlushTrigger::Always,
            isolation_scope: IsolationScope::User,
        }
    }
}

impl std::fmt::Debug for MemoryConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryConfig")
            .field("model", &self.model.as_ref().map(|_| "<dyn Model>"))
            .field("flush_prompt", &self.flush_prompt.as_ref().map(|p| p.len()))
            .field(
                "consolidation_prompt",
                &self.consolidation_prompt.as_ref().map(|p| p.len()),
            )
            .field("consolidation_max_tokens", &self.consolidation_max_tokens)
            .field("consolidation_min_gap", &self.consolidation_min_gap)
            .field("daily_file_retention_days", &self.daily_file_retention_days)
            .field("flush_trigger", &self.flush_trigger)
            .field("isolation_scope", &self.isolation_scope)
            .finish()
    }
}

impl MemoryConfig {
    /// 等价于 harness 历史默认行为的配置。
    pub fn defaults() -> Self {
        Self::default()
    }

    /// 校验 consolidation 提示词：必须恰好两个 `%d` 占位符。
    pub fn validate_consolidation_prompt(prompt: &str) -> Result<()> {
        let count = count_occurrences(prompt, "%d");
        if count != 2 {
            return Err(Error::InvalidInput(format!(
                "consolidationPrompt must contain exactly two %d placeholders (max-tokens, max-chars), found {count}"
            )));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// MemoryBackgroundTasks
// ---------------------------------------------------------------------------

/// 在途 fire-and-forget 记忆后台任务计数（flush / maintenance），供
/// `HarnessAgent::close()` 等待静默（对齐 Java `MemoryBackgroundTasks`）。
pub struct MemoryBackgroundTasks;

static IN_FLIGHT: Mutex<usize> = Mutex::new(0);
static IN_FLIGHT_ZERO: Condvar = Condvar::new();

impl MemoryBackgroundTasks {
    /// 标记一个后台任务开始（必须与 [`Self::end`] 配对）。
    pub fn begin() {
        let mut guard = IN_FLIGHT.lock().unwrap();
        *guard += 1;
    }

    /// 标记一个后台任务结束；计数归零时唤醒等待者。
    pub fn end() {
        let mut guard = IN_FLIGHT.lock().unwrap();
        *guard = guard.saturating_sub(1);
        if *guard == 0 {
            IN_FLIGHT_ZERO.notify_all();
        }
    }

    /// 当前在途任务数。
    pub fn in_flight() -> usize {
        *IN_FLIGHT.lock().unwrap()
    }

    /// 阻塞直到在途任务归零或超时；返回是否在时限内静默。
    pub fn await_quiescence(timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        let mut guard = IN_FLIGHT.lock().unwrap();
        while *guard > 0 {
            let now = std::time::Instant::now();
            if now >= deadline {
                return false;
            }
            let (g, _wait_result) = IN_FLIGHT_ZERO
                .wait_timeout(guard, deadline - now)
                .unwrap();
            guard = g;
        }
        true
    }
}

// ---------------------------------------------------------------------------
// MemoryFlushManager（Layer 1：每日台账）
// ---------------------------------------------------------------------------

/// 从对话窗口抽取长期记忆并追加到今日台账（对齐 Java `MemoryFlushManager`）。
pub struct MemoryFlushManager {
    workspace: Arc<WorkspaceManager>,
    model: Arc<dyn Model>,
    flush_prompt: String,
}

impl MemoryFlushManager {
    pub fn new(workspace: Arc<WorkspaceManager>, model: Arc<dyn Model>) -> Self {
        Self::with_prompt(workspace, model, DEFAULT_FLUSH_PROMPT.to_string())
    }

    pub fn with_prompt(
        workspace: Arc<WorkspaceManager>,
        model: Arc<dyn Model>,
        flush_prompt: impl Into<String>,
    ) -> Self {
        let prompt = flush_prompt.into();
        Self {
            workspace,
            model,
            flush_prompt: if prompt.is_empty() {
                DEFAULT_FLUSH_PROMPT.to_string()
            } else {
                prompt
            },
        }
    }

    /// 抽取长期记忆并写入今日台账。提供既有 MEMORY.md 与今日台账内容作为
    /// 只读上下文，让抽取模型去重、避免重复抽取已知事实。
    pub async fn flush_memories(&self, rc: &RuntimeContext, messages: &[Msg]) -> Result<()> {
        let conversation_text = serialize_messages(messages);
        if conversation_text.trim().is_empty() {
            return Ok(());
        }

        let existing_memory = self
            .read_existing(rc, MEMORY_MD)
            .await
            .unwrap_or_default();
        let today = today_iso_date();
        let daily_rel_path = format!("{MEMORY_DIR}/{today}.md");
        let existing_daily = self
            .read_existing(rc, &daily_rel_path)
            .await
            .unwrap_or_default();

        let mut user_prompt = String::new();
        if !existing_memory.trim().is_empty() {
            user_prompt.push_str(
                "MEMORY.md (read-only curated long-term memory — do NOT restate):\n",
            );
            user_prompt.push_str(&existing_memory);
            user_prompt.push_str("\n\n");
        }
        if !existing_daily.trim().is_empty() {
            user_prompt
                .push_str("Today's daily ledger so far (your output will be appended after):\n");
            user_prompt.push_str(&existing_daily);
            user_prompt.push_str("\n\n");
        }
        user_prompt.push_str(
            "Extract NEW memories from this conversation window (skip anything already covered above):\n\n",
        );
        user_prompt.push_str(&conversation_text);

        let flush_input = vec![
            Msg::system("memory", &self.flush_prompt),
            Msg::user("memory", user_prompt),
        ];

        let response = self
            .model
            .call(ChatRequest {
                messages: flush_input,
                options: GenerateOptions::default(),
            })
            .await?;
        let extracted = render_text_blocks(&response);
        if extracted.trim().is_empty() || extracted.trim() == "NO_REPLY" {
            return Ok(());
        }
        self.write_daily_entry(rc, &today, &extracted).await
    }

    /// 把抽取内容追加到今日台账（MEMORY.md 不在此处触碰——它归
    /// [`MemoryConsolidator`] 所有）。
    async fn write_daily_entry(&self, rc: &RuntimeContext, today: &str, content: &str) -> Result<()> {
        let daily_entry = format!("\n## Memory Flush — {}\n{}\n", now_iso8601(), content);
        let daily_rel_path = format!("{MEMORY_DIR}/{today}.md");
        self.workspace
            .append_utf8_workspace_relative(rc, &daily_rel_path, &daily_entry)
            .await
            .map_err(Error::Other)
    }

    /// 读取既有文件内容；不存在/为空返回 `None`（read_managed 对缺失返回空串）。
    async fn read_existing(&self, rc: &RuntimeContext, relative_path: &str) -> Option<String> {
        let content = self
            .workspace
            .read_managed_workspace_file_utf8(rc, relative_path)
            .await;
        if content.trim().is_empty() {
            None
        } else {
            Some(content)
        }
    }
}

/// 拼接响应里的全部 Text 块。
fn render_text_blocks(response: &ChatResponse) -> String {
    let mut out = String::new();
    for block in &response.content {
        if let ContentBlock::Text { text } = block {
            out.push_str(text);
        }
    }
    out
}

/// `<session_context>` 标签（内部上下文消息标记）。
const SESSION_CONTEXT_TAG: &str = "<session_context>";

/// 把消息序列化为抽取模型的文本表示：跳过 SYSTEM 与内部上下文消息；
/// 工具调用/结果渲染为紧凑文本（input 截 500 字符、结果截 1000 字符）。
pub fn serialize_messages(messages: &[Msg]) -> String {
    messages
        .iter()
        .filter(|m| m.role != Role::System)
        .filter(|m| !is_internal_context_message(m))
        .filter_map(render_message)
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_internal_context_message(msg: &Msg) -> bool {
    is_session_context_message(msg) || is_compaction_summary_message(msg)
}

fn is_compaction_summary_message(msg: &Msg) -> bool {
    msg.name == COMPACTION_SUMMARY_MSG_NAME
}

fn is_session_context_message(msg: &Msg) -> bool {
    if msg.role != Role::User {
        return false;
    }
    msg.text().contains(SESSION_CONTEXT_TAG)
}

fn render_message(msg: &Msg) -> Option<String> {
    let body = render_content_blocks(msg)?;
    Some(format!("[{:?}]: {body}", msg.role))
}

/// 渲染消息的全部内容块；无可渲染内容返回 `None`。
fn render_content_blocks(msg: &Msg) -> Option<String> {
    if msg.content.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    for block in &msg.content {
        match block {
            ContentBlock::Text { text } if !text.trim().is_empty() => {
                parts.push(text.clone());
            }
            ContentBlock::ToolUse { name, input, .. } => {
                parts.push(render_tool_use(name, input));
            }
            ContentBlock::ToolResult { name, output, .. } => {
                parts.push(render_tool_result(name, output));
            }
            _ => {}
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

fn render_tool_use(name: &str, input: &serde_json::Value) -> String {
    let mut sb = format!("[tool_call: {name}");
    let input_json = input.to_string();
    if !input.is_null() && input_json != "{}" {
        if input_json.len() > 500 {
            // 按字符边界截断（JSON 可能含多字节字符）。
            let cut = input_json
                .char_indices()
                .map(|(i, _)| i)
                .take_while(|&i| i <= 500)
                .last()
                .unwrap_or(0);
            sb.push_str(&format!("({}...)", &input_json[..cut]));
        } else {
            sb.push_str(&format!("({input_json})"));
        }
    }
    sb.push(']');
    sb
}

fn render_tool_result(name: &str, output: &[ContentBlock]) -> String {
    let mut sb = String::from("[tool_result");
    if !name.is_empty() {
        sb.push_str(": ");
        sb.push_str(name);
    }
    sb.push_str("] ");
    for out in output {
        if let ContentBlock::Text { text } = out {
            if text.len() > 1000 {
                let cut = text
                    .char_indices()
                    .map(|(i, _)| i)
                    .take_while(|&i| i <= 1000)
                    .last()
                    .unwrap_or(0);
                sb.push_str(&text[..cut]);
                sb.push_str("...(truncated)");
            } else {
                sb.push_str(text);
            }
        }
    }
    sb
}

// ---------------------------------------------------------------------------
// MemoryConsolidator（Layer 2：策展 MEMORY.md）
// ---------------------------------------------------------------------------

/// LLM 驱动的每日台账合并（对齐 Java `MemoryConsolidator`）：读取自上次
/// watermark 以来修改过的每日文件 + 当前 MEMORY.md，让 LLM 合并/去重/裁剪，
/// 覆写 MEMORY.md 并推进 watermark。无新条目时为 no-op。
///
/// watermark 记录在 `memory/.consolidation_state`（ISO-8601）；分布式
/// store 版（CAS）留到分布式阶段。
pub struct MemoryConsolidator {
    workspace: Arc<WorkspaceManager>,
    model: Arc<dyn Model>,
    consolidation_prompt: String,
    max_memory_tokens: usize,
}

impl MemoryConsolidator {
    pub fn new(workspace: Arc<WorkspaceManager>, model: Arc<dyn Model>) -> Self {
        Self::with_options(
            workspace,
            model,
            DEFAULT_CONSOLIDATION_PROMPT.to_string(),
            4_000,
        )
    }

    pub fn with_options(
        workspace: Arc<WorkspaceManager>,
        model: Arc<dyn Model>,
        consolidation_prompt: impl Into<String>,
        max_memory_tokens: usize,
    ) -> Self {
        let prompt = consolidation_prompt.into();
        Self {
            workspace,
            model,
            consolidation_prompt: if prompt.is_empty() {
                DEFAULT_CONSOLIDATION_PROMPT.to_string()
            } else {
                prompt
            },
            max_memory_tokens,
        }
    }

    /// 运行合并：无新每日条目时直接返回。
    pub async fn consolidate(&self, rc: &RuntimeContext) -> Result<()> {
        let watermark = self.read_watermark(rc).await;
        let run_start = now_iso8601();

        let current_memory = self.workspace.read_memory_md(rc).await;
        let daily_entries = self.read_daily_entries(rc, &watermark).await;

        if daily_entries.trim().is_empty() {
            return Ok(());
        }

        let max_chars = self.max_memory_tokens * 4;
        let system_prompt = format_prompt_two_ints(
            &self.consolidation_prompt,
            self.max_memory_tokens,
            max_chars,
        )?;

        let mut user_content = String::from("Current MEMORY.md:\n");
        user_content.push_str(if current_memory.trim().is_empty() {
            "(empty)"
        } else {
            &current_memory
        });
        user_content.push_str("\n\nNew daily ledger entries to merge");
        if watermark != "1970-01-01T00:00:00Z" {
            user_content.push_str(&format!(" (since {watermark})"));
        }
        user_content.push_str(":\n");
        user_content.push_str(&daily_entries);

        let messages = vec![
            Msg::system("memory", &system_prompt),
            Msg::user("memory", user_content),
        ];
        let response = self
            .model
            .call(ChatRequest {
                messages,
                options: GenerateOptions::default(),
            })
            .await?;
        let consolidated = render_text_blocks(&response);
        if consolidated.trim().is_empty() {
            return Ok(());
        }
        self.workspace
            .write_utf8_workspace_relative(rc, MEMORY_MD, consolidated.trim())
            .await
            .map_err(Error::Other)?;
        self.workspace
            .write_utf8_workspace_relative(rc, CONSOLIDATION_STATE_FILE, &run_start)
            .await
            .map_err(Error::Other)
    }

    /// 读取上次合并时刻；无记录/解析失败按 EPOCH 处理（首次全量合并）。
    async fn read_watermark(&self, rc: &RuntimeContext) -> String {
        let value = self
            .workspace
            .read_managed_workspace_file_utf8(rc, CONSOLIDATION_STATE_FILE)
            .await;
        if value.trim().is_empty() {
            "1970-01-01T00:00:00Z".to_string()
        } else {
            value.trim().to_string()
        }
    }

    /// 读取自 `watermark`（ISO-8601 字符串比较语义：同格式下字典序即时间序）
    /// 之后修改的每日文件，按文件名排序拼接为 `### <name>\n<content>` 段落。
    async fn read_daily_entries(&self, rc: &RuntimeContext, watermark: &str) -> String {
        let Some(fs) = self.workspace.filesystem() else {
            return String::new();
        };
        let matches = match fs.glob(rc, "*.md", MEMORY_DIR).await {
            Ok(matches) => matches,
            Err(_) => return String::new(),
        };
        let mut eligible: Vec<&crate::workspace::FileInfo> = matches
            .iter()
            .filter(|fi| !fi.is_directory)
            .filter(|fi| {
                let name = file_name(&fi.path);
                name != ".consolidation_state" && name != "archive" && name.ends_with(".md")
            })
            .filter(|fi| is_modified_after(fi.modified_at, watermark))
            .collect();
        eligible.sort_by(|a, b| file_name(&a.path).cmp(file_name(&b.path)));

        let mut sb = String::new();
        for fi in eligible {
            // glob 契约返回相对 cwd（memory/）的路径，读回时拼回目录前缀。
            let rel = format!("{}/{}", MEMORY_DIR, fi.path.trim().trim_start_matches('/'));
            let content = self
                .workspace
                .read_managed_workspace_file_utf8(rc, &rel)
                .await;
            if !content.trim().is_empty() {
                sb.push_str(&format!("### {}\n", file_name(&fi.path)));
                sb.push_str(content.trim());
                sb.push_str("\n\n");
            }
        }
        sb
    }
}

/// 取路径最后一段（对齐 Java `fileName`）。
fn file_name(path: &str) -> &str {
    let stripped = path.strip_suffix('/').unwrap_or(path);
    match stripped.rfind('/') {
        Some(idx) => &stripped[idx + 1..],
        None => stripped,
    }
}

/// mtime 过滤：未知/解析失败一律包含（宁可多读，不丢条目）。
///
/// watermark 与 modified_at 都是 ISO-8601 UTC 字符串（同格式下字典序即
/// 时间序），直接比较；modified_at 是 epoch 毫秒，转成同格式再比。
fn is_modified_after(modified_at: Option<i64>, watermark: &str) -> bool {
    let Some(millis) = modified_at else {
        return true;
    };
    let modified_iso = millis_to_iso8601(millis);
    modified_iso.as_str() > watermark
}

/// epoch 毫秒 → ISO-8601 UTC（毫秒精度截断到秒，与 watermark 写入格式一致）。
pub(crate) fn millis_to_iso8601(millis: i64) -> String {
    let secs = millis.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let (h, min, s) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}Z")
}

/// 把模板中的两个 `%d` 依次替换为 `first`、`second`（对齐 Java `String.format`）。
fn format_prompt_two_ints(template: &str, first: usize, second: usize) -> Result<String> {
    MemoryConfig::validate_consolidation_prompt(template)?;
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    for value in [first, second] {
        match rest.find("%d") {
            Some(idx) => {
                out.push_str(&rest[..idx]);
                out.push_str(&value.to_string());
                rest = &rest[idx + 2..];
            }
            None => {
                out.push_str(rest);
                return Ok(out);
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::InMemoryFilesystem;
    use agentscope_core::message::ToolResultState;
    use agentscope_core::model::ChatResponse;
    use std::sync::Mutex;

    /// 固定响应序列的 Mock 模型（记录收到的请求供断言）。
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
        async fn call(&self, request: ChatRequest) -> Result<ChatResponse> {
            self.requests.lock().unwrap().push(request);
            self.responses
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| Error::Model("no more mock responses".into()))
        }
    }

    fn memory_workspace(files: Vec<(&str, &str)>) -> Arc<WorkspaceManager> {
        let fs: Arc<dyn crate::workspace::Filesystem> = Arc::new(InMemoryFilesystem::with_files(
            files
                .into_iter()
                .map(|(p, c)| (p.to_string(), c.as_bytes().to_vec())),
        ));
        Arc::new(WorkspaceManager::with_filesystem("test-ws", fs))
    }

    // ---------- MemoryConfig ----------

    #[test]
    fn consolidation_prompt_validation() {
        assert!(MemoryConfig::validate_consolidation_prompt("a %d b %d c").is_ok());
        assert!(MemoryConfig::validate_consolidation_prompt("no placeholder").is_err());
        assert!(MemoryConfig::validate_consolidation_prompt("one %d only").is_err());
        assert!(MemoryConfig::validate_consolidation_prompt("three %d %d %d").is_err());
    }

    #[test]
    fn throttled_zero_gap_normalizes_to_always() {
        assert_eq!(
            FlushTrigger::throttled(Duration::ZERO).unwrap(),
            FlushTrigger::Always
        );
        assert_eq!(
            FlushTrigger::throttled(Duration::from_secs(5)).unwrap(),
            FlushTrigger::Throttled(Duration::from_secs(5))
        );
    }

    #[test]
    fn default_config_is_valid() {
        MemoryConfig::validate_consolidation_prompt(DEFAULT_CONSOLIDATION_PROMPT).unwrap();
    }

    // ---------- format_prompt_two_ints ----------

    #[test]
    fn prompt_placeholder_filling() {
        let filled =
            format_prompt_two_ints("budget %d tokens / %d chars", 4000, 16000).unwrap();
        assert_eq!(filled, "budget 4000 tokens / 16000 chars");
        // 占位符数量不对时报错。
        assert!(format_prompt_two_ints("no slots", 1, 2).is_err());
    }

    // ---------- serialize_messages ----------

    #[test]
    fn serialize_skips_system_and_internal_context() {
        let msgs = vec![
            Msg::system("sys", "system prompt"),
            Msg::user("user", "hello world"),
            Msg::user("ctx", "<session_context>internal</session_context>"),
            Msg::assistant(
                COMPACTION_SUMMARY_MSG_NAME,
                vec![ContentBlock::Text {
                    text: "compacted summary".into(),
                }],
            ),
            Msg::user("user2", "second question"),
        ];
        let text = serialize_messages(&msgs);
        assert!(!text.contains("system prompt"));
        assert!(!text.contains("session_context"));
        assert!(!text.contains("compacted summary"));
        assert!(text.contains("hello world"));
        assert!(text.contains("second question"));
        assert!(text.starts_with("[User]:"));
    }

    #[test]
    fn serialize_truncates_tool_payloads() {
        let long_input = "x".repeat(800);
        let long_output = "y".repeat(2000);
        let msgs = vec![Msg {
            id: "m1".into(),
            name: "assistant".into(),
            role: Role::Assistant,
            content: vec![
                ContentBlock::ToolUse {
                    id: "call-1".into(),
                    name: "search".into(),
                    input: serde_json::json!({"q": long_input}),
                    state: None,
                    content: None,
                },
                ContentBlock::ToolResult {
                    id: "call-1".into(),
                    name: "search".into(),
                    output: vec![ContentBlock::Text {
                        text: long_output,
                    }],
                    state: ToolResultState::Success,
                    execution_details: None,
                },
            ],
            metadata: None,
            timestamp: None,
            usage: None,
            generate_reason: None,
        }];
        let text = serialize_messages(&msgs);
        assert!(text.contains("[tool_call: search("));
        assert!(!text.contains(&"x".repeat(501)));
        assert!(text.contains("...(truncated)"));
        assert!(!text.contains(&"y".repeat(1001)));
    }

    // ---------- MemoryFlushManager ----------

    #[tokio::test]
    async fn flush_writes_daily_ledger() {
        let ws = memory_workspace(vec![("MEMORY.md", "# Existing memory\n- known fact")]);
        let model = MockModel::new(vec![ChatResponse::from_text(
            "- new fact from conversation",
        )]);
        let manager = MemoryFlushManager::new(Arc::clone(&ws), model.clone());
        let rc = RuntimeContext::new();
        let msgs = vec![Msg::user("user", "tell me something new")];
        manager.flush_memories(&rc, &msgs).await.unwrap();

        let today = today_iso_date();
        let daily = ws
            .read_managed_workspace_file_utf8(&rc, &format!("{MEMORY_DIR}/{today}.md"))
            .await;
        assert!(daily.contains("## Memory Flush —"));
        assert!(daily.contains("- new fact from conversation"));
        // MEMORY.md 不被 flush 触碰。
        assert_eq!(
            ws.read_managed_workspace_file_utf8(&rc, MEMORY_MD).await,
            "# Existing memory\n- known fact"
        );
        // 抽取请求带上了既有记忆作为只读上下文。
        let requests = model.requests();
        assert_eq!(requests.len(), 1);
        let prompt_text = requests[0]
            .messages
            .iter()
            .map(|m| m.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(prompt_text.contains("known fact"));
    }

    #[tokio::test]
    async fn flush_no_reply_is_noop() {
        let ws = memory_workspace(vec![]);
        let model = MockModel::new(vec![ChatResponse::from_text("NO_REPLY")]);
        let manager = MemoryFlushManager::new(Arc::clone(&ws), model);
        let rc = RuntimeContext::new();
        manager
            .flush_memories(&rc, &[Msg::user("user", "hi")])
            .await
            .unwrap();
        let today = today_iso_date();
        let daily = ws
            .read_managed_workspace_file_utf8(&rc, &format!("{MEMORY_DIR}/{today}.md"))
            .await;
        assert!(daily.trim().is_empty());
    }

    #[tokio::test]
    async fn flush_empty_conversation_is_noop() {
        let ws = memory_workspace(vec![]);
        let model = MockModel::new(vec![]);
        let manager = MemoryFlushManager::new(Arc::clone(&ws), model);
        let rc = RuntimeContext::new();
        // 只有 SYSTEM 消息 → 序列化为空 → 不调模型。
        manager
            .flush_memories(&rc, &[Msg::system("sys", "only system")])
            .await
            .unwrap();
        // MockModel 无响应：若被调用会报错，这里不报错即未调用。
    }

    // ---------- MemoryConsolidator ----------

    #[tokio::test]
    async fn consolidate_merges_and_sets_watermark() {
        let ws = memory_workspace(vec![(
            "memory/2026-01-01.md",
            "- daily fact one\n- daily fact two",
        )]);
        let model = MockModel::new(vec![ChatResponse::from_text(
            "# MEMORY\n- merged durable fact",
        )]);
        let consolidator = MemoryConsolidator::new(Arc::clone(&ws), model.clone());
        let rc = RuntimeContext::new();
        consolidator.consolidate(&rc).await.unwrap();

        assert_eq!(
            ws.read_managed_workspace_file_utf8(&rc, MEMORY_MD).await,
            "# MEMORY\n- merged durable fact"
        );
        let watermark = ws
            .read_managed_workspace_file_utf8(&rc, CONSOLIDATION_STATE_FILE)
            .await;
        assert!(!watermark.trim().is_empty());
        assert!(watermark.contains("T"));
        // 合并请求带上了当前 MEMORY.md（空 → "(empty)"）与每日条目。
        let requests = model.requests();
        let prompt_text = requests[0]
            .messages
            .iter()
            .map(|m| m.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(prompt_text.contains("(empty)"));
        assert!(prompt_text.contains("daily fact one"));
    }

    #[tokio::test]
    async fn consolidate_skips_when_no_daily_entries() {
        let ws = memory_workspace(vec![("MEMORY.md", "# Existing")]);
        let model = MockModel::new(vec![]);
        let consolidator = MemoryConsolidator::new(Arc::clone(&ws), model);
        let rc = RuntimeContext::new();
        consolidator.consolidate(&rc).await.unwrap();
        // 无新条目：MEMORY.md 不被改写，模型未被调用。
        assert_eq!(
            ws.read_managed_workspace_file_utf8(&rc, MEMORY_MD).await,
            "# Existing"
        );
    }

    #[tokio::test]
    async fn consolidate_ignores_non_daily_files() {
        // .consolidation_state 与 archive/ 不算每日条目。
        let ws = memory_workspace(vec![(
            "memory/.consolidation_state",
            "2026-01-01T00:00:00Z",
        )]);
        let model = MockModel::new(vec![]);
        let consolidator = MemoryConsolidator::new(Arc::clone(&ws), model);
        let rc = RuntimeContext::new();
        consolidator.consolidate(&rc).await.unwrap();
        // 只有状态文件 → 无新条目 → no-op。
        assert!(
            ws.read_managed_workspace_file_utf8(&rc, MEMORY_MD)
                .await
                .trim()
                .is_empty()
        );
    }

    // ---------- MemoryBackgroundTasks ----------

    #[test]
    fn background_tasks_quiescence() {
        assert!(MemoryBackgroundTasks::await_quiescence(Duration::from_millis(50)));
        MemoryBackgroundTasks::begin();
        MemoryBackgroundTasks::begin();
        assert_eq!(MemoryBackgroundTasks::in_flight(), 2);
        MemoryBackgroundTasks::end();
        MemoryBackgroundTasks::end();
        // 归零后等待立即返回 true。
        assert!(MemoryBackgroundTasks::await_quiescence(Duration::from_secs(1)));
        // 多余的 end 不应把计数打成负数。
        MemoryBackgroundTasks::end();
        assert_eq!(MemoryBackgroundTasks::in_flight(), 0);
    }

    // ---------- 时间工具 ----------

    #[test]
    fn today_shape_and_millis_conversion() {
        let today = today_iso_date();
        assert_eq!(today.len(), 10);
        assert_eq!(&today[4..5], "-");
        assert_eq!(&today[7..8], "-");
        // 已知时刻：2026-01-01T00:00:00Z = 1767225600_000 ms。
        assert_eq!(millis_to_iso8601(1_767_225_600_000), "2026-01-01T00:00:00Z");
        assert_eq!(millis_to_iso8601(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn file_name_extracts_last_segment() {
        assert_eq!(file_name("memory/2026-01-01.md"), "2026-01-01.md");
        assert_eq!(file_name("MEMORY.md"), "MEMORY.md");
        assert_eq!(file_name("memory/"), "memory");
    }
}
