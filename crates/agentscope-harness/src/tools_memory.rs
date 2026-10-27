//! 长期记忆的 LLM 工具（对应 Java `MemorySaveTool` / `MemoryGetTool` /
//! `MemorySearchTool`）：保存、分页读取、大小写不敏感搜索工作区记忆文件。

use std::sync::Arc;

use serde_json::{json, Value};

use agentscope_core::tool::{Tool, ToolOutput};
use agentscope_core::RuntimeContext;

use crate::tools_filesystem::{compile_keywords, KeywordMode, KeywordPredicate};
use crate::workspace::constants::MEMORY_MD;
use crate::workspace::WorkspaceManager;

/// 搜索命中上限（对齐 Java `MemorySearchTool` 的 100 条）。
const SEARCH_MAX_HITS: usize = 100;

/// 大小写不敏感字面量匹配器工厂（对齐 Java `Pattern.quote + CASE_INSENSITIVE`）。
fn case_insensitive_literal(term: String) -> KeywordPredicate {
    let needle = term.to_lowercase();
    Box::new(move |line: &str| line.to_lowercase().contains(&needle))
}

/// 解析 matchMode 参数（默认 phrase；非法值报错）。
fn match_mode(input: &Value) -> Result<KeywordMode, ToolOutput> {
    match input.get("matchMode").and_then(Value::as_str) {
        None => Ok(KeywordMode::Phrase),
        Some("phrase") => Ok(KeywordMode::Phrase),
        Some("all") => Ok(KeywordMode::All),
        Some("any") => Ok(KeywordMode::Any),
        Some(other) => Err(ToolOutput::error(format!(
            "Invalid matchMode: {other}. Must be one of: phrase, all, any"
        ))),
    }
}

// ---------------------------------------------------------------------------
// memory_save
// ---------------------------------------------------------------------------

/// 保存长期记忆（对应 Java `MemorySaveTool`）：追加到 MEMORY.md 与今日台账。
pub struct MemorySaveTool {
    workspace: Arc<WorkspaceManager>,
}

impl MemorySaveTool {
    pub fn new(workspace: Arc<WorkspaceManager>) -> Self {
        Self { workspace }
    }
}

#[async_trait::async_trait]
impl Tool for MemorySaveTool {
    fn name(&self) -> &str {
        "memory_save"
    }

    fn description(&self) -> &str {
        "Save important information to long-term memory. Use this to persist \
         facts, decisions, preferences, or context that should be remembered \
         across conversations. Content should be markdown bullet points."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The memory content to save (markdown bullet points recommended)"
                }
            },
            "required": ["content"]
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput, agentscope_core::Error> {
        let Some(content) = input.get("content").and_then(Value::as_str) else {
            return Ok(ToolOutput::error("content is required"));
        };
        if content.trim().is_empty() {
            return Ok(ToolOutput::error("content is required"));
        }
        let rc = RuntimeContext::new();
        let today = crate::memory::today_iso_date();
        let daily_rel_path = format!("memory/{today}.md");
        let daily_entry = format!("\n## Memory Save — {}\n{content}\n", now_instant());

        if let Err(error) = self
            .workspace
            .append_utf8_workspace_relative(&rc, MEMORY_MD, &format!("\n{content}\n"))
            .await
        {
            return Ok(ToolOutput::error(format!("Failed to save memory: {error}")));
        }
        if let Err(error) = self
            .workspace
            .append_utf8_workspace_relative(&rc, &daily_rel_path, &daily_entry)
            .await
        {
            return Ok(ToolOutput::error(format!(
                "Failed to append to daily ledger: {error}"
            )));
        }
        let count = content.lines().filter(|l| l.trim_start().starts_with('-')).count();
        Ok(ToolOutput::ok(format!(
            "Saved {} memories to MEMORY.md",
            count.max(1)
        )))
    }
}

/// 当前时刻 ISO-8601（秒级；与 memory.rs 的 flush 时间戳同格式）。
fn now_instant() -> String {
    let secs = crate::coordination::now_epoch_millis().div_euclid(1000);
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

// ---------------------------------------------------------------------------
// memory_get
// ---------------------------------------------------------------------------

/// 按行分页读取记忆文件（对应 Java `MemoryGetTool`；行号 1-based，闭区间）。
pub struct MemoryGetTool {
    workspace: Arc<WorkspaceManager>,
}

impl MemoryGetTool {
    pub fn new(workspace: Arc<WorkspaceManager>) -> Self {
        Self { workspace }
    }
}

#[async_trait::async_trait]
impl Tool for MemoryGetTool {

    /// 只读工具：权限模式检查（EXPLORE/ACCEPT_EDITS）与 studio 免确认白名单依据。
    fn is_read_only(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "memory_get"
    }

    fn description(&self) -> &str {
        "Read specific lines from a memory file. Use after memory_search to \
         pull full context around matched lines. Line numbers are 1-based and \
         inclusive."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Memory file path relative to workspace (e.g. MEMORY.md or memory/2026-01-01.md)"},
                "startLine": {"type": "integer", "description": "Start line number (1-based, inclusive)"},
                "endLine": {"type": "integer", "description": "End line number (1-based, inclusive)"}
            },
            "required": ["path"]
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput, agentscope_core::Error> {
        let Some(path) = input.get("path").and_then(Value::as_str) else {
            return Ok(ToolOutput::error("path is required"));
        };
        let start_line = input
            .get("startLine")
            .and_then(Value::as_i64)
            .unwrap_or(1);
        let end_line = input.get("endLine").and_then(Value::as_i64);
        if start_line < 1 {
            return Ok(ToolOutput::error(format!(
                "startLine must be >= 1, got {start_line}"
            )));
        }
        if let Some(end) = end_line {
            if end < start_line {
                return Ok(ToolOutput::error(format!(
                    "endLine ({end}) must be >= startLine ({start_line})"
                )));
            }
        }
        let rc = RuntimeContext::new();
        let content = self
            .workspace
            .read_managed_workspace_file_utf8(&rc, path)
            .await;
        if content.trim().is_empty() {
            return Ok(ToolOutput::ok("(empty file)"));
        }
        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len() as i64;
        if start_line > total {
            return Ok(ToolOutput::error(format!(
                "startLine ({start_line}) is beyond the end of the file ({total} lines)"
            )));
        }
        let end = end_line.unwrap_or(total).min(total);
        let mut out = String::new();
        for (idx, line) in lines.iter().enumerate() {
            let n = (idx + 1) as i64;
            if n >= start_line && n <= end {
                out.push_str(&format!("{n}|{line}\n"));
            }
        }
        Ok(ToolOutput::ok(out.trim_end().to_string()))
    }
}

// ---------------------------------------------------------------------------
// memory_search
// ---------------------------------------------------------------------------

/// 大小写不敏感搜索记忆文件（对应 Java `MemorySearchTool`）。
pub struct MemorySearchTool {
    workspace: Arc<WorkspaceManager>,
}

impl MemorySearchTool {
    pub fn new(workspace: Arc<WorkspaceManager>) -> Self {
        Self { workspace }
    }
}

#[async_trait::async_trait]
impl Tool for MemorySearchTool {

    /// 只读工具：权限模式检查（EXPLORE/ACCEPT_EDITS）与 studio 免确认白名单依据。
    fn is_read_only(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "memory_search"
    }

    fn description(&self) -> &str {
        "Search long-term memory files for a keyword or phrase (case-insensitive). \
         Returns matching lines with their source location."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Search query (keywords or phrase)"},
                "matchMode": {
                    "type": "string",
                    "enum": ["phrase", "all", "any"],
                    "description": "phrase = whole query as one literal (default); all = every whitespace-separated term must match; any = at least one term matches"
                }
            },
            "required": ["query"]
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput, agentscope_core::Error> {
        let Some(query) = input.get("query").and_then(Value::as_str) else {
            return Ok(ToolOutput::error("query is required"));
        };
        if query.trim().is_empty() {
            return Ok(ToolOutput::error("query is required"));
        }
        let mode = match match_mode(&input) {
            Ok(mode) => mode,
            Err(output) => return Ok(output),
        };
        let predicate = match compile_keywords(query, mode, case_insensitive_literal) {
            Ok(predicate) => predicate,
            Err(error) => return Ok(ToolOutput::error(error)),
        };

        let rc = RuntimeContext::new();
        let paths = self.workspace.list_memory_file_paths(&rc).await;
        let mut hits = String::new();
        let mut hit_count = 0usize;
        'outer: for path in &paths {
            let content = self
                .workspace
                .read_managed_workspace_file_utf8(&rc, path)
                .await;
            if content.trim().is_empty() {
                continue;
            }
            for (idx, line) in content.lines().enumerate() {
                if predicate(line) {
                    hits.push_str(&format!("Source: {path}#{}: {line}\n", idx + 1));
                    hit_count += 1;
                    if hit_count >= SEARCH_MAX_HITS {
                        break 'outer;
                    }
                }
            }
        }
        if hits.is_empty() {
            Ok(ToolOutput::ok(format!(
                "No matching memories found for: {query}"
            )))
        } else {
            Ok(ToolOutput::ok(hits.trim_end().to_string()))
        }
    }
}

/// 三个记忆工具的集合体（对齐 Java 各工具独立类的装配形态）。
pub struct MemoryToolSet {
    pub memory_save: MemorySaveTool,
    pub memory_get: MemoryGetTool,
    pub memory_search: MemorySearchTool,
}

impl MemoryToolSet {
    pub fn new(workspace: Arc<WorkspaceManager>) -> Self {
        Self {
            memory_save: MemorySaveTool::new(Arc::clone(&workspace)),
            memory_get: MemoryGetTool::new(Arc::clone(&workspace)),
            memory_search: MemorySearchTool::new(workspace),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::InMemoryFilesystem;
    use serde_json::json;

    fn memory_workspace(files: Vec<(&str, &str)>) -> Arc<WorkspaceManager> {
        let fs: Arc<dyn crate::workspace::Filesystem> = Arc::new(InMemoryFilesystem::with_files(
            files
                .into_iter()
                .map(|(p, c)| (p.to_string(), c.as_bytes().to_vec())),
        ));
        Arc::new(WorkspaceManager::with_filesystem("test-ws", fs))
    }

    // ---------- memory_save ----------

    #[tokio::test]
    async fn save_appends_memory_and_daily_ledger() {
        let ws = memory_workspace(vec![("MEMORY.md", "# Existing\n")]);
        let tool = MemorySaveTool::new(Arc::clone(&ws));
        let output = tool
            .call(json!({"content": "- user prefers Rust\n- project deadline is Friday"}))
            .await
            .unwrap();
        assert_eq!(output.state, agentscope_core::message::ToolResultState::Success);
        assert!(output.content.contains("Saved 2 memories"));

        let rc = RuntimeContext::new();
        let memory = ws.read_managed_workspace_file_utf8(&rc, MEMORY_MD).await;
        assert!(memory.contains("# Existing"));
        assert!(memory.contains("- user prefers Rust"));

        let today = crate::memory::today_iso_date();
        let daily = ws
            .read_managed_workspace_file_utf8(&rc, &format!("memory/{today}.md"))
            .await;
        assert!(daily.contains("## Memory Save —"));
        assert!(daily.contains("- project deadline is Friday"));
    }

    #[tokio::test]
    async fn save_rejects_blank_content() {
        let ws = memory_workspace(vec![]);
        let tool = MemorySaveTool::new(ws);
        let output = tool.call(json!({"content": "   "})).await.unwrap();
        assert_eq!(output.state, agentscope_core::message::ToolResultState::Error);
        assert!(output.content.contains("content is required"));
        // 缺参同样报错。
        let output = tool.call(json!({})).await.unwrap();
        assert_eq!(output.state, agentscope_core::message::ToolResultState::Error);
    }

    #[tokio::test]
    async fn save_counts_single_line_as_one_memory() {
        let ws = memory_workspace(vec![]);
        let tool = MemorySaveTool::new(ws);
        let output = tool
            .call(json!({"content": "plain sentence without bullets"}))
            .await
            .unwrap();
        assert!(output.content.contains("Saved 1 memories"));
    }

    // ---------- memory_get ----------

    #[tokio::test]
    async fn get_reads_line_ranges() {
        let ws = memory_workspace(vec![("MEMORY.md", "line one\nline two\nline three")]);
        let tool = MemoryGetTool::new(ws);
        let output = tool
            .call(json!({"path": "MEMORY.md", "startLine": 2, "endLine": 3}))
            .await
            .unwrap();
        assert_eq!(output.content, "2|line two\n3|line three");
    }

    #[tokio::test]
    async fn get_defaults_end_to_file_end() {
        let ws = memory_workspace(vec![("MEMORY.md", "a\nb\nc\nd")]);
        let tool = MemoryGetTool::new(ws);
        let output = tool
            .call(json!({"path": "MEMORY.md", "startLine": 3}))
            .await
            .unwrap();
        assert_eq!(output.content, "3|c\n4|d");
    }

    #[tokio::test]
    async fn get_validates_ranges() {
        let ws = memory_workspace(vec![("MEMORY.md", "a\nb")]);
        let tool = MemoryGetTool::new(ws);
        // startLine < 1。
        let output = tool
            .call(json!({"path": "MEMORY.md", "startLine": 0}))
            .await
            .unwrap();
        assert!(output.content.contains("startLine must be >= 1"));
        // endLine < startLine。
        let output = tool
            .call(json!({"path": "MEMORY.md", "startLine": 2, "endLine": 1}))
            .await
            .unwrap();
        assert!(output.content.contains("must be >= startLine"));
        // 越过文件末尾。
        let output = tool
            .call(json!({"path": "MEMORY.md", "startLine": 10}))
            .await
            .unwrap();
        assert!(output.content.contains("beyond the end of the file (2 lines)"));
        // 空文件。
        let ws2 = memory_workspace(vec![("MEMORY.md", "   ")]);
        let tool2 = MemoryGetTool::new(ws2);
        let output = tool2.call(json!({"path": "MEMORY.md"})).await.unwrap();
        assert_eq!(output.content, "(empty file)");
    }

    // ---------- memory_search ----------

    #[tokio::test]
    async fn search_phrase_case_insensitive() {
        let ws = memory_workspace(vec![(
            "MEMORY.md",
            "- User prefers Rust\n- Deployment on Friday\n- Likes coffee",
        )]);
        let tool = MemorySearchTool::new(ws);
        let output = tool
            .call(json!({"query": "prefers RUST"}))
            .await
            .unwrap();
        assert!(output.content.contains("Source: MEMORY.md#1: - User prefers Rust"));
        // 无命中。
        let output = tool.call(json!({"query": "nonexistent"})).await.unwrap();
        assert!(output.content.contains("No matching memories found for: nonexistent"));
    }

    #[tokio::test]
    async fn search_all_and_any_modes() {
        let ws = memory_workspace(vec![(
            "MEMORY.md",
            "- Rust deployment Friday\n- Python script\n- Friday meeting",
        )]);
        let tool = MemorySearchTool::new(ws);
        // all：两个词都要命中同一行。
        let output = tool
            .call(json!({"query": "rust friday", "matchMode": "all"}))
            .await
            .unwrap();
        assert!(output.content.contains("#1:"));
        assert!(!output.content.contains("#3:"));
        // any：任一词命中。
        let output = tool
            .call(json!({"query": "python meeting", "matchMode": "any"}))
            .await
            .unwrap();
        assert!(output.content.contains("#2:"));
        assert!(output.content.contains("#3:"));
    }

    #[tokio::test]
    async fn search_validates_params() {
        let ws = memory_workspace(vec![]);
        let tool = MemorySearchTool::new(ws);
        // 空查询。
        let output = tool.call(json!({"query": "  "})).await.unwrap();
        assert!(output.content.contains("query is required"));
        // 非法 matchMode。
        let output = tool
            .call(json!({"query": "x", "matchMode": "regex"}))
            .await
            .unwrap();
        assert!(output.content.contains("Invalid matchMode"));
    }

    #[tokio::test]
    async fn search_spans_memory_dir_files() {
        let ws = memory_workspace(vec![
            ("MEMORY.md", "- top level fact"),
            ("memory/2026-01-01.md", "- daily ledger fact"),
        ]);
        let tool = MemorySearchTool::new(ws);
        let output = tool.call(json!({"query": "fact"})).await.unwrap();
        assert!(output.content.contains("MEMORY.md#1:"));
        assert!(output.content.contains("memory/2026-01-01.md#1:"));
    }
}
