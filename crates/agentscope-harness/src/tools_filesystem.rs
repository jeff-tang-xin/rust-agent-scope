//! 文件工具集（对应 Java harness `agent.tool` 包的 `FilesystemTool` /
//! `FuzzyTextMatcher` / `KeywordMatcher`）。
//!
//! 6 个 LLM 工具（read_file / write_file / edit_file / grep_files / glob_files /
//! list_files）共享一个 [`Filesystem`] 后端与可选的 [`WorkspacePathNormalizer`]；
//! 输出经 [`bounded_listing`] 截断（条数上限 + 16K 字符上限 + 续读指引）。
//!
//! [`fuzzy_search`] 是三级模糊阶梯（精确 → 去行尾空白 → 折叠全部空白），
//! 供 skill patch 等场景使用：每级维护到原串的逐字节映射，替换只动命中的
//! 区间，LLM 没要求改的空白原样保留。

use std::sync::Arc;

use agentscope_core::{Tool, ToolOutput};
use serde_json::{json, Value};

use crate::workspace::{Filesystem, WorkspacePathNormalizer};

/// grep 默认返回上限（对齐 Java `DEFAULT_GREP_LIMIT`）。
pub const DEFAULT_GREP_LIMIT: usize = 100;
/// glob 默认返回上限（对齐 Java `DEFAULT_GLOB_LIMIT`）。
pub const DEFAULT_GLOB_LIMIT: usize = 200;
/// 搜索类工具的硬上限（对齐 Java `MAX_SEARCH_LIMIT`）。
pub const MAX_SEARCH_LIMIT: usize = 1_000;
/// list_files 条数上限（对齐 Java `MAX_LISTING_ENTRIES`）。
pub const MAX_LISTING_ENTRIES: usize = 200;
/// 列表输出的字符上限（对齐 Java `MAX_LISTING_CHARS`）。
pub const MAX_LISTING_CHARS: usize = 16_000;

/// 有界列表输出：按 `limit` 截断条目，单次输出不超过 [`MAX_LISTING_CHARS`]
/// 字符；发生截断时追加续读指引（对齐 Java `boundedListing`）。
pub fn bounded_listing<T>(
    items: impl IntoIterator<Item = T>,
    total: usize,
    limit: usize,
    result_label: &str,
    mut render: impl FnMut(&T) -> String,
) -> String {
    let mut out = String::new();
    let mut count = 0usize;
    let mut character_limit_reached = false;
    for item in items {
        if count >= limit {
            break;
        }
        let line = render(&item);
        let separator_len = if count > 0 { 1 } else { 0 };
        if out.len() + line.len() + separator_len > MAX_LISTING_CHARS {
            character_limit_reached = true;
            break;
        }
        if count > 0 {
            out.push('\n');
        }
        out.push_str(&line);
        count += 1;
    }
    if count == total {
        return out;
    }
    let guidance = if character_limit_reached {
        format!("Output character limit of {MAX_LISTING_CHARS} reached; narrow the path/pattern.")
    } else if result_label == "entries" {
        format!("Listing limit of {limit} reached; narrow the directory.")
    } else if limit < MAX_SEARCH_LIMIT {
        format!("Narrow the path/pattern or increase limit (hard maximum: {MAX_SEARCH_LIMIT}).")
    } else {
        format!("Hard maximum of {MAX_SEARCH_LIMIT} reached; results truncated.")
    };
    format!("{out}\n\n{guidance}")
}

// ---------------------------------------------------------------------------
// FuzzyTextMatcher（对应 Java `FuzzyTextMatcher`）
// ---------------------------------------------------------------------------

/// 匹配严格度：从严到宽排列（对齐 Java `Level`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MatchLevel {
    /// 逐字节精确匹配。
    Exact,
    /// 双方逐行去掉行尾空白后匹配（CRLF 的 `\r` 归入行终止符）。
    TrailingWsStripped,
    /// 再去掉行首空白、行内空白串折叠为单个空格后匹配。
    WhitespaceCollapsed,
}

/// 一次命中在原串中的字节区间（含头不含尾）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchRange {
    pub start: usize,
    pub end: usize,
    pub level: MatchLevel,
}

/// 搜索结果：首个非空级别的全部命中。
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub matches: Vec<MatchRange>,
    pub level: MatchLevel,
}

impl SearchResult {
    pub fn is_empty(&self) -> bool {
        self.matches.is_empty()
    }
}

/// 归一化视图 + 逐字节到原串的映射。
struct Normalized {
    text: String,
    /// `text` 每个字节对应的原串字节偏移。
    original_index: Vec<usize>,
}

/// 在 `existing` 中搜索 `needle`：从最严级别逐级放宽，返回首个非空级别的
/// 全部命中（供调用方做同级别唯一性检查）；任何级别都不中则返回空结果。
///
/// 归一化级别的区间边界永远落在行终止符边界上（CRLF 的 `\r` 映射给发出的
/// `\n`），替换拼接不会把终止符劈成两半。
pub fn fuzzy_search(existing: &str, needle: &str) -> SearchResult {
    if needle.is_empty() {
        return SearchResult {
            matches: Vec::new(),
            level: MatchLevel::Exact,
        };
    }

    // Level 1: 精确。
    let exact = find_all(existing, needle, MatchLevel::Exact);
    if !exact.is_empty() {
        return SearchResult {
            matches: exact,
            level: MatchLevel::Exact,
        };
    }

    // Level 2: 去行尾空白。
    let existing_tws = strip_trailing_whitespace(existing);
    let needle_tws = strip_trailing_whitespace(needle);
    let tws = find_all_normalized(existing, &existing_tws, &needle_tws, MatchLevel::TrailingWsStripped);
    if !tws.is_empty() {
        return SearchResult {
            matches: tws,
            level: MatchLevel::TrailingWsStripped,
        };
    }

    // Level 3: 全空白折叠。
    let existing_wsc = collapse_whitespace(existing);
    let needle_wsc = collapse_whitespace(needle);
    let wsc = find_all_normalized(existing, &existing_wsc, &needle_wsc, MatchLevel::WhitespaceCollapsed);
    if !wsc.is_empty() {
        return SearchResult {
            matches: wsc,
            level: MatchLevel::WhitespaceCollapsed,
        };
    }

    SearchResult {
        matches: Vec::new(),
        level: MatchLevel::Exact,
    }
}

fn find_all(haystack: &str, needle: &str, level: MatchLevel) -> Vec<MatchRange> {
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some(idx) = haystack[from..].find(needle) {
        let start = from + idx;
        out.push(MatchRange {
            start,
            end: start + needle.len(),
            level,
        });
        from = start + needle.len();
    }
    out
}

fn find_all_normalized(
    source: &str,
    haystack: &Normalized,
    needle: &Normalized,
    level: MatchLevel,
) -> Vec<MatchRange> {
    let mut out = Vec::new();
    if needle.text.is_empty() {
        return out;
    }
    let mut from = 0usize;
    while let Some(idx) = haystack.text[from..].find(&needle.text) {
        let start = from + idx;
        let needle_end = start + needle.text.len();
        let orig_start = haystack.original_index[start];
        let orig_end = if needle_end < haystack.original_index.len() {
            haystack.original_index[needle_end]
        } else {
            source.len()
        };
        out.push(MatchRange {
            start: orig_start,
            end: orig_end,
            level,
        });
        from = needle_end;
    }
    out
}

/// 行体终点（不含）：紧邻 `\n` 的 `\r` 属于 CRLF 终止符而非行内容。
fn line_body_end(s: &str, line_start: usize, line_end: usize) -> usize {
    if line_end < s.len() && line_end > line_start && s.as_bytes()[line_end - 1] == b'\r' {
        line_end - 1
    } else {
        line_end
    }
}

/// 逐行去掉行尾 `' '`/`'\t'`；换行保留，发出的 `\n` 映射到行终止符起点
/// （CRLF 时是 `\r` 的位置）。
fn strip_trailing_whitespace(s: &str) -> Normalized {
    let mut text = String::with_capacity(s.len());
    let mut map: Vec<usize> = Vec::with_capacity(s.len() + 1);
    let mut line_start = 0usize;
    loop {
        let line_end = s[line_start..]
            .find('\n')
            .map(|p| line_start + p)
            .unwrap_or(s.len());
        let has_newline = line_end < s.len();
        let body_end = line_body_end(s, line_start, line_end);
        let mut trim_end = body_end;
        while trim_end > line_start {
            let c = s[..trim_end].chars().last().unwrap();
            if c == ' ' || c == '\t' {
                trim_end -= c.len_utf8();
            } else {
                break;
            }
        }
        for (idx, c) in s[line_start..trim_end].char_indices() {
            for _ in 0..c.len_utf8() {
                map.push(line_start + idx);
            }
            text.push(c);
        }
        if has_newline {
            text.push('\n');
            map.push(body_end);
            line_start = line_end + 1;
        } else {
            break;
        }
    }
    Normalized { text, original_index: map }
}

/// 激进归一化：逐行去行首/行尾空白，行内空白串折叠为单个 `' '`（映射到
/// 串起点）；全空行不发出换行，保持折叠视图紧凑。
fn collapse_whitespace(s: &str) -> Normalized {
    let mut text = String::with_capacity(s.len());
    let mut map: Vec<usize> = Vec::with_capacity(s.len() + 1);
    let mut line_start = 0usize;
    loop {
        let line_end = s[line_start..]
            .find('\n')
            .map(|p| line_start + p)
            .unwrap_or(s.len());
        let has_newline = line_end < s.len();
        let body_end = line_body_end(s, line_start, line_end);
        let content_emitted_at = map.len();
        let mut in_run = false;
        let mut run_orig_start = 0usize;
        let mut idx = line_start;
        for c in s[line_start..body_end].chars() {
            let advance = c.len_utf8();
            if c == ' ' || c == '\t' {
                if !in_run {
                    in_run = true;
                    run_orig_start = idx;
                }
            } else {
                if in_run {
                    text.push(' ');
                    map.push(run_orig_start);
                    in_run = false;
                }
                for _ in 0..advance {
                    map.push(idx);
                }
                text.push(c);
            }
            idx += advance;
        }
        // 行尾空白随 in_run 一起丢弃；只有写过内容才发换行。
        if has_newline && map.len() > content_emitted_at {
            text.push('\n');
            map.push(body_end);
        }
        if !has_newline {
            break;
        }
        line_start = line_end + 1;
    }
    Normalized { text, original_index: map }
}

// ---------------------------------------------------------------------------
// KeywordMatcher（对应 Java `KeywordMatcher`）
// ---------------------------------------------------------------------------

/// 关键词匹配模式（对齐 Java `KeywordMatcher.compile` 的 matchMode）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeywordMode {
    /// 整个查询串作为一个字面量。
    Phrase,
    /// 按空白拆词后全部命中。
    All,
    /// 按空白拆词后任一命中。
    Any,
}

/// 编译关键词谓词：`literal` 把单个词变成字面量匹配器（各工具保留自己的
/// 大小写语义）。phrase 模式整串一个词；all/any 按空白拆词、去重、去空。
/// all/any 拆不出任何词时报错（不允许空查询匹配一切）。
/// 编译后的关键词谓词。
pub type KeywordPredicate = Box<dyn Fn(&str) -> bool + Send + Sync>;
/// 字面量匹配器工厂：把单个关键词变成谓词（各工具保留自己的大小写语义）。
pub type LiteralMatcherFactory = fn(String) -> KeywordPredicate;

pub fn compile_keywords(
    query: &str,
    mode: KeywordMode,
    literal: LiteralMatcherFactory,
) -> Result<KeywordPredicate, String> {
    if mode == KeywordMode::Phrase {
        return Ok(literal(query.to_string()));
    }
    let terms: Vec<String> = query
        .split_whitespace()
        .filter(|term| !term.is_empty())
        .map(str::to_string)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    if terms.is_empty() {
        return Err("query must contain at least one keyword".to_string());
    }
    let matchers: Vec<KeywordPredicate> = terms.into_iter().map(literal).collect();
    match mode {
        KeywordMode::All => Ok(Box::new(move |text: &str| {
            matchers.iter().all(|matcher| matcher(text))
        })),
        KeywordMode::Any => Ok(Box::new(move |text: &str| {
            matchers.iter().any(|matcher| matcher(text))
        })),
        KeywordMode::Phrase => unreachable!("phrase handled above"),
    }
}

// ---------------------------------------------------------------------------
// 工具集
// ---------------------------------------------------------------------------

/// 6 个文件工具的集合：一次构建，逐个注册进 `Toolkit`。
pub struct FilesystemToolSet {
    pub read_file: ReadFileTool,
    pub write_file: WriteFileTool,
    pub edit_file: EditFileTool,
    pub grep_files: GrepFilesTool,
    pub glob_files: GlobFilesTool,
    pub list_files: ListFilesTool,
}

impl FilesystemToolSet {
    pub fn new(
        fs: Arc<dyn Filesystem>,
        normalizer: Option<Arc<WorkspacePathNormalizer>>,
    ) -> Self {
        Self {
            read_file: ReadFileTool {
                fs: Arc::clone(&fs),
                normalizer: normalizer.clone(),
            },
            write_file: WriteFileTool {
                fs: Arc::clone(&fs),
                normalizer: normalizer.clone(),
            },
            edit_file: EditFileTool {
                fs: Arc::clone(&fs),
                normalizer: normalizer.clone(),
            },
            grep_files: GrepFilesTool {
                fs: Arc::clone(&fs),
                normalizer: normalizer.clone(),
            },
            glob_files: GlobFilesTool {
                fs: Arc::clone(&fs),
                normalizer: normalizer.clone(),
            },
            list_files: ListFilesTool {
                fs,
                normalizer,
            },
        }
    }

    /// 全部 6 个工具（注册顺序稳定）。
    pub fn all(self) -> Vec<Arc<dyn Tool>> {
        vec![
            Arc::new(self.read_file),
            Arc::new(self.write_file),
            Arc::new(self.edit_file),
            Arc::new(self.grep_files),
            Arc::new(self.glob_files),
            Arc::new(self.list_files),
        ]
    }
}

/// 归一化输入路径（配置了 normalizer 时剥掉注册前缀）。
fn normalize_path(
    normalizer: &Option<Arc<WorkspacePathNormalizer>>,
    path: &str,
) -> String {
    match normalizer {
        Some(n) => n.normalize(path),
        None => path.to_string(),
    }
}

fn string_param(input: &Value, key: &str) -> Result<String, ToolOutput> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            ToolOutput::error(format!("missing required string parameter: {key}"))
        })
}

fn usize_param(input: &Value, key: &str, default: usize) -> usize {
    input
        .get(key)
        .and_then(Value::as_u64)
        .map(|v| v as usize)
        .unwrap_or(default)
}

/// 读取文件内容（对应 Java `readFile`；`offset` 0-indexed 起始行，
/// `limit` 最大行数，0 = 全部）。
pub struct ReadFileTool {
    fs: Arc<dyn Filesystem>,
    normalizer: Option<Arc<WorkspacePathNormalizer>>,
}

#[async_trait::async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read file content with line numbers. Supports pagination via offset and limit."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path to read"},
                "offset": {"type": "integer", "description": "Start line (0-indexed). Default: 0 (from beginning)"},
                "limit": {"type": "integer", "description": "Max lines to return. Default: 0 (all lines)"}
            },
            "required": ["path"]
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput, agentscope_core::Error> {
        let path = match string_param(&input, "path") {
            Ok(path) => path,
            Err(output) => return Ok(output),
        };
        let offset = usize_param(&input, "offset", 0);
        let limit = usize_param(&input, "limit", 0);
        let rc = agentscope_core::RuntimeContext::new();
        match self
            .fs
            .read_lines(&rc, &normalize_path(&self.normalizer, &path), offset, limit)
            .await
        {
            Ok(Some(content)) => Ok(ToolOutput::ok(content)),
            Ok(None) => Ok(ToolOutput::error(format!("File not found: {path}"))),
            Err(error) => Ok(ToolOutput::error(error)),
        }
    }
}

/// 创建新文件（对应 Java `writeFile`；父目录自动创建）。
pub struct WriteFileTool {
    fs: Arc<dyn Filesystem>,
    normalizer: Option<Arc<WorkspacePathNormalizer>>,
}

#[async_trait::async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Create a new file with the given full content (parent directories are created). \
         Fails if the path already exists — read_file the existing file and use edit_file \
         to change it instead of overwriting."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Target file path"},
                "content": {"type": "string", "description": "File content to write"}
            },
            "required": ["path", "content"]
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput, agentscope_core::Error> {
        let (path, content) = match (string_param(&input, "path"), string_param(&input, "content")) {
            (Ok(path), Ok(content)) => (path, content),
            (Err(output), _) | (_, Err(output)) => return Ok(output),
        };
        let rc = agentscope_core::RuntimeContext::new();
        match self
            .fs
            .write(&rc, &normalize_path(&self.normalizer, &path), &content)
            .await
        {
            Ok(()) => Ok(ToolOutput::ok(format!("Written to {path}"))),
            Err(error) => Ok(ToolOutput::error(error)),
        }
    }
}

/// 精确字符串替换（对应 Java `editFile`；`replace_all=false` 时要求唯一）。
pub struct EditFileTool {
    fs: Arc<dyn Filesystem>,
    normalizer: Option<Arc<WorkspacePathNormalizer>>,
}

#[async_trait::async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Perform exact string replacement in a file. The old_string must be unique unless \
         replace_all is true."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File to edit"},
                "old_string": {"type": "string", "description": "Text to find"},
                "new_string": {"type": "string", "description": "Replacement text"},
                "replace_all": {"type": "boolean", "description": "Replace all occurrences (default: false)"}
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput, agentscope_core::Error> {
        let path = match string_param(&input, "path") {
            Ok(path) => path,
            Err(output) => return Ok(output),
        };
        let old_string = match string_param(&input, "old_string") {
            Ok(v) => v,
            Err(output) => return Ok(output),
        };
        let new_string = match string_param(&input, "new_string") {
            Ok(v) => v,
            Err(output) => return Ok(output),
        };
        let replace_all = input.get("replace_all").and_then(Value::as_bool).unwrap_or(false);
        let rc = agentscope_core::RuntimeContext::new();
        match self
            .fs
            .edit(
                &rc,
                &normalize_path(&self.normalizer, &path),
                &old_string,
                &new_string,
                replace_all,
            )
            .await
        {
            Ok(occurrences) => Ok(ToolOutput::ok(format!(
                "Edited {path} ({occurrences} replacement(s))"
            ))),
            Err(error) => Ok(ToolOutput::error(error)),
        }
    }
}

/// 内容字面量搜索（对应 Java `grepFiles`；默认 100 条，硬上限 1000）。
pub struct GrepFilesTool {
    fs: Arc<dyn Filesystem>,
    normalizer: Option<Arc<WorkspacePathNormalizer>>,
}

#[async_trait::async_trait]
impl Tool for GrepFilesTool {
    fn name(&self) -> &str {
        "grep_files"
    }

    fn description(&self) -> &str {
        "Search file contents for a literal text pattern. Returns at most 100 matches by \
         default to keep tool output bounded."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Literal text pattern to search for"},
                "path": {"type": "string", "description": "Directory or file to search"},
                "glob": {"type": "string", "description": "Optional file glob filter (e.g., *.java)"},
                "limit": {"type": "integer", "description": "Maximum matches to return (default/recommended: 100; hard maximum: 1000)"}
            },
            "required": ["pattern"]
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput, agentscope_core::Error> {
        let pattern = match string_param(&input, "pattern") {
            Ok(v) => v,
            Err(output) => return Ok(output),
        };
        let path = input.get("path").and_then(Value::as_str);
        let glob = input.get("glob").and_then(Value::as_str);
        let requested = usize_param(&input, "limit", DEFAULT_GREP_LIMIT);
        let limit = requested.min(MAX_SEARCH_LIMIT);
        if limit == 0 {
            return Ok(ToolOutput::error("limit must be greater than 0"));
        }
        let rc = agentscope_core::RuntimeContext::new();
        let normalized = path.map(|p| normalize_path(&self.normalizer, p));
        match self.fs.grep(&rc, &pattern, normalized.as_deref(), glob).await {
            Ok(matches) => {
                let total = matches.len();
                let output = bounded_listing(
                    matches,
                    total,
                    limit,
                    "matches",
                    |m: &crate::workspace::GrepMatch| format!("{}:{}:{}", m.path, m.line, m.text),
                );
                Ok(ToolOutput::ok(output))
            }
            Err(error) => Ok(ToolOutput::error(error)),
        }
    }
}

/// 文件名 glob 搜索（对应 Java `globFiles`；默认 200 条，硬上限 1000）。
pub struct GlobFilesTool {
    fs: Arc<dyn Filesystem>,
    normalizer: Option<Arc<WorkspacePathNormalizer>>,
}

#[async_trait::async_trait]
impl Tool for GlobFilesTool {
    fn name(&self) -> &str {
        "glob_files"
    }

    fn description(&self) -> &str {
        "Find files matching a glob pattern. Returns at most 200 files by default to keep \
         tool output bounded."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Glob pattern (e.g., **/*.java)"},
                "path": {"type": "string", "description": "Base directory to search from"},
                "limit": {"type": "integer", "description": "Maximum files to return (default/recommended: 200; hard maximum: 1000)"}
            },
            "required": ["pattern"]
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput, agentscope_core::Error> {
        let pattern = match string_param(&input, "pattern") {
            Ok(v) => v,
            Err(output) => return Ok(output),
        };
        let path = input.get("path").and_then(Value::as_str).unwrap_or(".");
        let requested = usize_param(&input, "limit", DEFAULT_GLOB_LIMIT);
        let limit = requested.min(MAX_SEARCH_LIMIT);
        if limit == 0 {
            return Ok(ToolOutput::error("limit must be greater than 0"));
        }
        let rc = agentscope_core::RuntimeContext::new();
        match self
            .fs
            .glob(&rc, &pattern, &normalize_path(&self.normalizer, path))
            .await
        {
            Ok(files) => {
                let total = files.len();
                let output = bounded_listing(
                    files,
                    total,
                    limit,
                    "files",
                    |f: &crate::workspace::FileInfo| {
                        if f.is_directory {
                            f.path.clone()
                        } else {
                            format!("{} ({} bytes)", f.path, f.size.unwrap_or(0))
                        }
                    },
                );
                Ok(ToolOutput::ok(output))
            }
            Err(error) => Ok(ToolOutput::error(error)),
        }
    }
}

/// 目录列表（对应 Java `listFiles`；`[DIR]`/`[FILE]` 前缀 + 字节数）。
pub struct ListFilesTool {
    fs: Arc<dyn Filesystem>,
    normalizer: Option<Arc<WorkspacePathNormalizer>>,
}

#[async_trait::async_trait]
impl Tool for ListFilesTool {
    fn name(&self) -> &str {
        "list_files"
    }

    fn description(&self) -> &str {
        "List files and directories at the given path."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Directory path to list"}
            },
            "required": ["path"]
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput, agentscope_core::Error> {
        let path = match string_param(&input, "path") {
            Ok(v) => v,
            Err(output) => return Ok(output),
        };
        let rc = agentscope_core::RuntimeContext::new();
        match self
            .fs
            .ls(&rc, &normalize_path(&self.normalizer, &path))
            .await
        {
            Ok(entries) => {
                if entries.is_empty() {
                    return Ok(ToolOutput::ok(format!("Empty directory: {path}")));
                }
                let total = entries.len();
                let output = bounded_listing(
                    entries,
                    total,
                    MAX_LISTING_ENTRIES,
                    "entries",
                    |f: &crate::workspace::FileInfo| {
                        if f.is_directory {
                            format!("[DIR]  {}", f.path)
                        } else {
                            format!("[FILE] {} ({} bytes)", f.path, f.size.unwrap_or(0))
                        }
                    },
                );
                Ok(ToolOutput::ok(output))
            }
            Err(error) => Ok(ToolOutput::error(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::InMemoryFilesystem;

    // ---------- bounded_listing ----------

    #[test]
    fn bounded_listing_no_truncation_when_under_limit() {
        let out = bounded_listing(["a", "b"], 2, 100, "matches", |s: &&str| s.to_string());
        assert_eq!(out, "a\nb");
    }

    #[test]
    fn bounded_listing_truncation_guidance_variants() {
        // matches/files 类：提示放宽或提高 limit。
        let items: Vec<&str> = (0..5).map(|i| match i { 0 => "a", 1 => "b", 2 => "c", 3 => "d", _ => "e" }).collect();
        let out = bounded_listing(items.clone(), 5, 3, "matches", |s: &&str| s.to_string());
        assert!(out.starts_with("a\nb\nc\n\n"));
        assert!(out.contains("Narrow the path/pattern or increase limit (hard maximum: 1000)."));

        // entries 类：提示收窄目录。
        let out = bounded_listing(items.clone(), 5, 3, "entries", |s: &&str| s.to_string());
        assert!(out.contains("Listing limit of 3 reached; narrow the directory."));

        // 硬上限：条目数超过 MAX_SEARCH_LIMIT 且 limit 已顶格。
        let many: Vec<String> = (0..MAX_SEARCH_LIMIT + 5).map(|i| format!("item{i}")).collect();
        let out = bounded_listing(many.iter(), many.len(), MAX_SEARCH_LIMIT, "matches", |s: &&String| s.to_string());
        assert!(out.contains("Hard maximum of 1000 reached; results truncated."));
    }

    #[test]
    fn bounded_listing_character_cap() {
        let long_line = "x".repeat(MAX_LISTING_CHARS + 100);
        let items = vec![long_line.as_str(), "second"];
        let out = bounded_listing(items, 2, 10, "matches", |s: &&str| s.to_string());
        assert!(out.contains("Output character limit of 16000 reached"));
        assert!(!out.contains("second"));
    }

    // ---------- fuzzy_search ----------

    #[test]
    fn fuzzy_exact_level() {
        let result = fuzzy_search("hello world", "world");
        assert_eq!(result.level, MatchLevel::Exact);
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].start, 6);
        assert_eq!(result.matches[0].end, 11);
    }

    #[test]
    fn fuzzy_empty_needle() {
        assert!(fuzzy_search("abc", "").is_empty());
    }

    #[test]
    fn fuzzy_trailing_ws_level_maps_back_to_original() {
        // 行尾空白差异 → Level 2 命中，区间映射回原串（含行尾空白）。
        let existing = "line one   \nline two\t\nend";
        let needle = "line one\nline two\nend";
        let result = fuzzy_search(existing, needle);
        assert_eq!(result.level, MatchLevel::TrailingWsStripped);
        assert_eq!(result.matches.len(), 1);
        let m = &result.matches[0];
        // 替换整个区间应得到 needle 的内容（原串区间 == 全文）。
        assert_eq!(&existing[m.start..m.end], existing);
    }

    #[test]
    fn fuzzy_crlf_tolerance_at_trailing_level() {
        // Windows 行尾文件 vs LF-only needle：Level 2 必须命中。
        let existing = "fn main() {\r\n    let x = 1;\r\n}\r\n";
        let needle = "fn main() {\n    let x = 1;\n}\n";
        let result = fuzzy_search(existing, needle);
        assert_eq!(result.level, MatchLevel::TrailingWsStripped);
        assert_eq!(result.matches.len(), 1);
        let m = &result.matches[0];
        assert_eq!(m.start, 0);
        assert_eq!(m.end, existing.len());
    }

    #[test]
    fn fuzzy_collapse_level_handles_indentation_drift() {
        // 缩进漂移（tab vs 空格）+ 行首空白差异 → Level 3。
        let existing = "if (x) {\n\t\treturn 1;\n}";
        let needle = "if (x) {\n    return 1;\n}";
        let result = fuzzy_search(existing, needle);
        assert_eq!(result.level, MatchLevel::WhitespaceCollapsed);
        assert_eq!(result.matches.len(), 1);
        let m = &result.matches[0];
        assert_eq!(&existing[m.start..m.end], existing);
    }

    #[test]
    fn fuzzy_no_match_returns_empty_exact() {
        let result = fuzzy_search("alpha beta", "gamma");
        assert!(result.is_empty());
        assert_eq!(result.level, MatchLevel::Exact);
    }

    #[test]
    fn fuzzy_partial_span_replacement_preserves_rest() {
        // 精确命中：只替换命中区间，区间外字节原样保留。
        let existing = "keep1\n  indented  \nkeep2";
        let result = fuzzy_search(existing, "indented");
        assert_eq!(result.level, MatchLevel::Exact);
        let m = &result.matches[0];
        let replaced = format!("{}NEW{}", &existing[..m.start], &existing[m.end..]);
        assert_eq!(replaced, "keep1\n  NEW  \nkeep2");

        // Level 2 部分区间：needle 跨行且行尾空白有差异，区间边界落在终止符上。
        let existing2 = "head\nbody one   \nbody two\n\ntrail";
        let needle2 = "body one\nbody two\n";
        let result2 = fuzzy_search(existing2, needle2);
        assert_eq!(result2.level, MatchLevel::TrailingWsStripped);
        let m2 = &result2.matches[0];
        assert_eq!(&existing2[m2.start..m2.end], "body one   \nbody two\n");
    }

    // ---------- fuzzy_search：折叠级别区间边界穷举 ----------

    #[test]
    fn fuzzy_collapse_level_partial_span_runs_included() {
        // 区间端点语义：命中首字符 map 起点 → 命中结束处下一折叠字符 map 起点，
        // 参与匹配的空白 run（头/尾）都含入区间。
        // "xa  by" 折叠 "xa by"（map: x→0 a→1 ' '→2 b→4 y→5），
        // needle "a b" 命中折叠 [1,4) → 原串 [1,5) = "a  b"（尾部 run 含入）。
        let existing = "xa  by";
        let result = fuzzy_search(existing, "a b");
        assert_eq!(result.level, MatchLevel::WhitespaceCollapsed);
        assert_eq!(result.matches.len(), 1);
        let m = &result.matches[0];
        assert_eq!(m.start, 1);
        assert_eq!(m.end, 5);
        assert_eq!(&existing[m.start..m.end], "a  b");
        // 部分区间（非全文）：needle_end 落在折叠视图内部时取 map[needle_end]。
        // "xab    cy" 折叠 "xab cy"（map: x→0 a→1 b→2 ' '→3 c→7 y→8），
        // needle "ab c" 命中 [1,5) → 原串 [1,8) = "ab    c"。
        let existing2 = "xab    cy";
        let r2 = fuzzy_search(existing2, "ab c");
        assert_eq!(r2.level, MatchLevel::WhitespaceCollapsed);
        let m2 = &r2.matches[0];
        assert_eq!(m2.start, 1);
        assert_eq!(m2.end, 8);
        assert_eq!(&existing2[m2.start..m2.end], "ab    c");
    }

    #[test]
    fn fuzzy_collapse_level_indent_width_drift_partial() {
        // 行首缩进宽度漂移（2 vs 1 空格）：精确与 Level 2 都不中（行首空白
        // 不属行尾），仅 Level 3 命中；区间映射回原串全文。
        let existing = "a\n  b";
        let needle = "a\n b";
        assert_ne!(fuzzy_search(existing, needle).level, MatchLevel::Exact, "前置确认：精确不中");
        let result = fuzzy_search(existing, needle);
        assert_eq!(result.level, MatchLevel::WhitespaceCollapsed);
        let m = &result.matches[0];
        assert_eq!(m.start, 0);
        assert_eq!(m.end, existing.len());
    }

    #[test]
    fn fuzzy_collapse_level_end_to_end_replacement() {
        // 端到端替换：区间外字节原样保留，替换后无残留空白串。
        let existing = "fn a()  {\n\t\treturn 1;\n}";
        let result = fuzzy_search(existing, "fn a() {\n  return 1;\n}");
        assert_eq!(result.level, MatchLevel::WhitespaceCollapsed);
        let m = &result.matches[0];
        assert_eq!(m.start, 0);
        assert_eq!(m.end, existing.len());
        let replaced = format!("{}NEW{}", &existing[..m.start], &existing[m.end..]);
        assert_eq!(replaced, "NEW");
    }

    #[test]
    fn fuzzy_collapse_level_blank_line_merging() {
        // 连续空行折叠合并（全空行不发出换行）：多空行 vs 单空行 Level 3 命中。
        // （空行缩进漂移在 Level 2 即解决——整行空白属行尾空白，非 Level 3 专属。）
        let existing = "a\n\n\nb";
        let needle = "a\n\nb";
        assert_ne!(fuzzy_search(existing, needle).level, MatchLevel::Exact, "前置确认：精确不中");
        let result = fuzzy_search(existing, needle);
        assert_eq!(result.level, MatchLevel::WhitespaceCollapsed);
        assert_eq!(result.matches.len(), 1);
        let m = &result.matches[0];
        assert_eq!(m.start, 0);
        assert_eq!(m.end, existing.len());
    }

    #[test]
    fn fuzzy_collapse_level_crlf_tolerance() {
        // CRLF 文件 + 缩进漂移：\r 属终止符不参与折叠，Level 3 仍命中。
        let existing = "if (x) {\r\n\t\treturn 1;\r\n}\r\n";
        let needle = "if (x) {\n    return 1;\n}\n";
        let result = fuzzy_search(existing, needle);
        assert_eq!(result.level, MatchLevel::WhitespaceCollapsed);
        assert_eq!(result.matches.len(), 1);
        let m = &result.matches[0];
        assert_eq!(m.start, 0);
        assert_eq!(m.end, existing.len());
    }

    #[test]
    fn fuzzy_collapse_level_multiple_matches_same_level() {
        // 同级别多命中全部返回（供调用方做唯一性检查）。
        // 两处命中都需折叠（各 2 空格 vs needle 1 空格）：
        let existing = "a  b x a  b";
        let result = fuzzy_search(existing, "a b");
        assert_eq!(result.level, MatchLevel::WhitespaceCollapsed);
        assert_eq!(result.matches.len(), 2);
        assert_eq!(&existing[result.matches[0].start..result.matches[0].end], "a  b");
        assert_eq!(&existing[result.matches[1].start..result.matches[1].end], "a  b");
        // 对照：存在精确命中时只返回精确级别（首个 "a b" 精确出现）。
        let result2 = fuzzy_search("a b x a  b", "a b");
        assert_eq!(result2.level, MatchLevel::Exact);
        assert_eq!(result2.matches.len(), 1);
    }

    #[test]
    fn fuzzy_whitespace_only_needle_never_matches() {
        // needle 全空白：折叠后为空 → 任何级别都不命中（防止空替换）。
        let result = fuzzy_search("a b c", "   ");
        assert!(result.is_empty());
        assert_eq!(result.level, MatchLevel::Exact);
    }

    #[test]
    fn fuzzy_collapse_level_interior_run_mapping() {
        // 行内空白串折叠为单空格、映射到 run 起点：区间端点落在 run 两端。
        // "key:    value" 折叠 "key: value"（map: k→0 e→1 y→2 :→3 ' '→4 v→9），
        // needle "key: value" 命中 [0,10) → 原串 [0,13) 全覆盖。
        let existing = "key:    value";
        let result = fuzzy_search(existing, "key: value");
        assert_eq!(result.level, MatchLevel::WhitespaceCollapsed);
        let m = &result.matches[0];
        assert_eq!(m.start, 0);
        assert_eq!(m.end, existing.len());
        // 部分区间：needle "value" 精确子串走 Level 1。
        let r2 = fuzzy_search(existing, "value");
        assert_eq!(r2.level, MatchLevel::Exact);
        assert_eq!(&existing[r2.matches[0].start..r2.matches[0].end], "value");
    }

    // ---------- compile_keywords ----------

    #[test]
    fn keywords_phrase_matches_whole_query() {
        let pred = compile_keywords("hello world", KeywordMode::Phrase, |term| {
            Box::new(move |text: &str| text.contains(term.as_str()))
        })
        .unwrap();
        assert!(pred("say hello world now"));
        assert!(!pred("say hello big world"));
    }

    #[test]
    fn keywords_all_requires_every_term() {
        let pred = compile_keywords("alpha beta", KeywordMode::All, |term| {
            Box::new(move |text: &str| text.contains(term.as_str()))
        })
        .unwrap();
        assert!(pred("alpha and beta"));
        assert!(!pred("alpha only"));
    }

    #[test]
    fn keywords_any_needs_one_term() {
        let pred = compile_keywords("alpha beta", KeywordMode::Any, |term| {
            Box::new(move |text: &str| text.contains(term.as_str()))
        })
        .unwrap();
        assert!(pred("has beta"));
        assert!(!pred("neither here"));
    }

    #[test]
    fn keywords_empty_all_query_rejected() {
        let result = compile_keywords("   ", KeywordMode::All, |term| {
            Box::new(move |text: &str| text.contains(term.as_str()))
        });
        let err = match result {
            Err(message) => message,
            Ok(_) => panic!("empty all-query must be rejected"),
        };
        assert!(err.contains("at least one keyword"));
    }

    // ---------- 工具 ----------

    fn toolset() -> FilesystemToolSet {
        let fs = Arc::new(InMemoryFilesystem::with_files([
            ("docs/a.md", b"# Title\nhello world".to_vec()),
            ("docs/b.md", b"alpha beta\nalpha gamma".to_vec()),
            ("src/main.rs", b"fn main() {}\n".to_vec()),
        ]));
        FilesystemToolSet::new(fs, None)
    }

    #[tokio::test]
    async fn read_file_tool_pagination() {
        let tools = toolset();
        let out = tools
            .read_file
            .call(json!({"path": "docs/a.md", "offset": 1, "limit": 1}))
            .await
            .unwrap();
        assert_eq!(out.content, "hello world");
        let missing = tools
            .read_file
            .call(json!({"path": "nope.md"}))
            .await
            .unwrap();
        assert_eq!(missing.state, agentscope_core::message::ToolResultState::Error);
    }

    #[tokio::test]
    async fn write_then_edit_file_tool() {
        let tools = toolset();
        let out = tools
            .write_file
            .call(json!({"path": "new/dir/f.txt", "content": "one two one"}))
            .await
            .unwrap();
        assert_eq!(out.content, "Written to new/dir/f.txt");

        // 唯一性检查：2 处出现且未 replace_all → 报错。
        let ambiguous = tools
            .edit_file
            .call(json!({"path": "new/dir/f.txt", "old_string": "one", "new_string": "1"}))
            .await
            .unwrap();
        assert_eq!(ambiguous.state, agentscope_core::message::ToolResultState::Error);

        let out = tools
            .edit_file
            .call(json!({"path": "new/dir/f.txt", "old_string": "one", "new_string": "1", "replace_all": true}))
            .await
            .unwrap();
        assert_eq!(out.content, "Edited new/dir/f.txt (2 replacement(s))");
    }

    #[tokio::test]
    async fn grep_files_tool_format_and_limit() {
        let tools = toolset();
        let out = tools
            .grep_files
            .call(json!({"pattern": "alpha"}))
            .await
            .unwrap();
        assert_eq!(out.content, "docs/b.md:1:alpha beta\ndocs/b.md:2:alpha gamma");

        let limited = tools
            .grep_files
            .call(json!({"pattern": "alpha", "limit": 1}))
            .await
            .unwrap();
        assert!(limited.content.starts_with("docs/b.md:1:alpha beta\n\n"));
        assert!(limited.content.contains("Narrow the path/pattern"));

        let globbed = tools
            .grep_files
            .call(json!({"pattern": "main", "glob": "*.rs"}))
            .await
            .unwrap();
        assert_eq!(globbed.content, "src/main.rs:1:fn main() {}");
    }

    #[tokio::test]
    async fn glob_and_list_files_tool() {
        let tools = toolset();
        let out = tools
            .glob_files
            .call(json!({"pattern": "*.md", "path": "docs"}))
            .await
            .unwrap();
        // glob 返回相对 cwd 的路径（对齐 Java searchPath.relativize）。
        assert_eq!(out.content, "a.md (19 bytes)\nb.md (22 bytes)");

        let listed = tools
            .list_files
            .call(json!({"path": "docs"}))
            .await
            .unwrap();
        assert_eq!(
            listed.content,
            "[FILE] a.md (19 bytes)\n[FILE] b.md (22 bytes)"
        );

        // src 下有 main.rs，不为空；换一个真正空的目录。
        let fs = Arc::new(InMemoryFilesystem::new());
        let tools2 = FilesystemToolSet::new(fs, None);
        let empty = tools2.list_files.call(json!({"path": "."})).await.unwrap();
        assert_eq!(empty.content, "Empty directory: .");
    }

    #[tokio::test]
    async fn tools_reject_missing_required_params() {
        let tools = toolset();
        let out = tools.read_file.call(json!({})).await.unwrap();
        assert_eq!(out.state, agentscope_core::message::ToolResultState::Error);
        assert!(out.content.contains("missing required string parameter: path"));

        let out = tools
            .edit_file
            .call(json!({"path": "x", "old_string": "a"}))
            .await
            .unwrap();
        assert!(out.content.contains("new_string"));
    }

    #[test]
    fn tool_names_and_schemas() {
        let tools = toolset();
        assert_eq!(tools.read_file.name(), "read_file");
        assert_eq!(tools.write_file.name(), "write_file");
        assert_eq!(tools.edit_file.name(), "edit_file");
        assert_eq!(tools.grep_files.name(), "grep_files");
        assert_eq!(tools.glob_files.name(), "glob_files");
        assert_eq!(tools.list_files.name(), "list_files");
        for tool in tools.all() {
            let schema = tool.parameters();
            assert_eq!(schema["type"], "object");
            assert!(schema["properties"].is_object());
        }
    }
}
