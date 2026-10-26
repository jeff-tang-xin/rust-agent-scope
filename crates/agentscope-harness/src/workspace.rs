//! 工作区：路径常量、路径策略与两层读写的工作区管理器。
//!
//! 对应 Java `io.agentscope.harness.agent.workspace` 包（`WorkspaceConstants` /
//! `LocalFsMode` / `PathPolicy` / `WorkspacePathNormalizer` / `WorkspaceManager`）。
//!
//! 读路径：先问 [`Filesystem`]（可注入沙箱/远端后端），非空即用；否则回退本地磁盘。
//! 写路径：全部走 [`Filesystem`]（未配置时写本地磁盘），本地写用「临时文件 + 原子改名」。
//! 列表：filesystem 与本地磁盘取并集、按相对路径去重。

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agentscope_core::context::RuntimeContext;

use crate::task::{TaskRecord, TaskStatus};

/// 工作区目录结构常量（对应 Java `WorkspaceConstants`）。
pub mod constants {
    /// 默认工作区根目录。
    pub const DEFAULT_WORKSPACE_ROOT: &str = ".agentscope/workspace";
    pub const AGENTS_MD: &str = "AGENTS.md";
    pub const MEMORY_MD: &str = "MEMORY.md";
    pub const TOOLS_JSON: &str = "tools.json";
    pub const MEMORY_DIR: &str = "memory";
    pub const SKILLS_DIR: &str = "skills";
    pub const KNOWLEDGE_DIR: &str = "knowledge";
    pub const KNOWLEDGE_MD: &str = "KNOWLEDGE.md";
    pub const RULES_DIR: &str = "rules";
    pub const AGENTS_DIR: &str = "agents";
    pub const SESSIONS_DIR: &str = "sessions";
    pub const TASKS_DIR: &str = "tasks";
}

pub use constants as WorkspaceConstants;

/// 宿主根文件系统的路径解析策略（对应 Java `LocalFsMode`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalFsMode {
    /// 所有路径锚定到文件系统根，拒绝 `..` 与越界绝对路径。
    Sandboxed,
    /// 绝对路径仅当落在 [`PathPolicy`] 配置的根之下才接受（默认）。
    Rooted,
    /// 绝对路径原样放行（逃生舱）。
    Unrestricted,
}

/// 宿主目录白名单：`Rooted` 模式下绝对路径必须落在某个根之下（对应 Java `PathPolicy`）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PathPolicy {
    roots: Vec<PathBuf>,
}

impl PathPolicy {
    /// 不允许任何绝对路径的策略。
    pub fn empty() -> Self {
        Self { roots: Vec::new() }
    }

    /// 从若干根构建策略；根会被规范化为绝对路径。
    pub fn of<I, P>(roots: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut normalized = Vec::new();
        for root in roots {
            let path = root.as_ref();
            let absolute = if path.is_absolute() {
                path.to_path_buf()
            } else {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join(path)
            };
            normalized.push(normalize_absolute(&absolute));
        }
        Self { roots: normalized }
    }

    /// 配置的根（插入顺序）。
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// 是否没有配置任何根。
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// `candidate`（必须为绝对路径）是否等于或位于某个根之下。
    pub fn is_allowed(&self, candidate: &Path) -> bool {
        if !candidate.is_absolute() {
            return false;
        }
        let normalized = normalize_absolute(candidate);
        self.roots.iter().any(|root| normalized.starts_with(root))
    }
}

/// Windows/Unix 统一的轻量规范化：解析 `.` 与 `..` 段（不触碰文件系统）。
fn normalize_absolute(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// 把文件路径规范化为工作区相对形式：剥离已注册的前缀（对应 Java `WorkspacePathNormalizer`）。
#[derive(Debug, Clone, Default)]
pub struct WorkspacePathNormalizer {
    prefixes: Vec<String>,
}

impl WorkspacePathNormalizer {
    /// 用单个前缀构建（如沙箱的 `/workspace` 或本地工作区绝对路径）。
    pub fn of(prefix: impl Into<String>) -> Self {
        let mut prefixes = Vec::with_capacity(1);
        let trimmed = trim_trailing_slash(&prefix.into());
        if !trimmed.is_empty() {
            prefixes.push(trimmed);
        }
        Self { prefixes }
    }

    /// 用多个前缀构建（本地 + shell 模式下项目目录与工作区目录都是合法根）。
    pub fn of_many<I, S>(prefixes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut list = Vec::new();
        for prefix in prefixes {
            let trimmed = trim_trailing_slash(&prefix.into());
            if !trimmed.is_empty() {
                list.push(trimmed);
            }
        }
        Self { prefixes: list }
    }

    /// 已注册的前缀。
    pub fn prefixes(&self) -> &[String] {
        &self.prefixes
    }

    /// 剥离匹配的前缀得到工作区相对路径；无前缀命中时原样返回。
    pub fn normalize(&self, path: &str) -> String {
        if path.trim().is_empty() {
            return path.to_string();
        }
        for prefix in &self.prefixes {
            if let Some(stripped) = try_strip(path, prefix) {
                return stripped;
            }
        }
        path.to_string()
    }
}

fn trim_trailing_slash(s: &str) -> String {
    if s.len() > 1 && s.ends_with('/') {
        s.trim_end_matches('/').to_string()
    } else {
        s.to_string()
    }
}

fn try_strip(path: &str, prefix: &str) -> Option<String> {
    let normalized_path = path.replace('\\', "/");
    let normalized_prefix = prefix.replace('\\', "/");
    if normalized_path.starts_with(&format!("{normalized_prefix}/")) {
        return Some(normalized_path[normalized_prefix.len() + 1..].to_string());
    }
    if normalized_path == normalized_prefix {
        return Some(".".to_string());
    }
    None
}

/// 校验工作区相对路径安全：非空、无 `..` 穿越（对应 Java `AbstractFilesystem.validatePath`）。
pub fn validate_path(path: &str) -> Result<(), String> {
    if path.trim().is_empty() {
        return Err("Path must not be null or blank".to_string());
    }
    let portable = path.replace('\\', "/");
    for segment in portable.split('/') {
        if segment == ".." {
            return Err(format!("Path traversal ('..') not allowed: {path}"));
        }
    }
    Ok(())
}

/// glob 结果里的一条文件信息（对应 Java `FileInfo`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    /// 工作区相对路径（目录项以 `/` 结尾，对齐 Java `FileInfo.ofDir`）。
    pub path: String,
    /// 文件大小（字节；目录或后端不支持时为 `None`）。
    pub size: Option<u64>,
    /// 是否目录。
    pub is_directory: bool,
    /// 最后修改时间（epoch 毫秒；后端不支持时为 `None`）。
    pub modified_at: Option<i64>,
}

/// grep 命中的一行（对应 Java `GrepMatch`；行号 1-based）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepMatch {
    /// 工作区相对文件路径。
    pub path: String,
    /// 行号（1-based）。
    pub line: usize,
    /// 该行原文。
    pub text: String,
}

/// 批量下载的单文件结果（对应 Java `FileDownloadResponse`）。
#[derive(Debug, Clone)]
pub struct DownloadedFile {
    /// 请求时的原始路径。
    pub path: String,
    /// 文件字节；读取失败时为 `Err`（单文件失败不拖垮整批）。
    pub content: Result<Vec<u8>, String>,
}

/// 文件系统抽象：`WorkspaceManager` 与文件工具的统一读写通道
/// （对应 Java `AbstractFilesystem`，11 方法全量）。
///
/// 路径一律为工作区相对路径（`/` 分隔）；实现负责圈定可访问范围。
#[async_trait::async_trait]
pub trait Filesystem: Send + Sync {
    /// 读取文本文件；不存在时返回 `None`。
    async fn read(&self, rc: &RuntimeContext, path: &str) -> Result<Option<String>, String>;

    /// 按行分页读取（对应 Java `read(rc, path, offset, limit)`）：
    /// `offset` 为 0-indexed 起始行，`limit` 为最大行数（0 = 全部）。
    /// 默认实现：全文读取后按行切。
    async fn read_lines(
        &self,
        rc: &RuntimeContext,
        path: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Option<String>, String> {
        let Some(content) = self.read(rc, path).await? else {
            return Ok(None);
        };
        Ok(Some(paginate_lines(&content, offset, limit)))
    }

    /// 文件是否存在。
    async fn exists(&self, rc: &RuntimeContext, path: &str) -> Result<bool, String>;

    /// 列出目录的单层条目（不递归；目录项路径以 `/` 结尾，按路径排序）。
    async fn ls(&self, rc: &RuntimeContext, path: &str) -> Result<Vec<FileInfo>, String>;

    /// 在既有文件中做精确字符串替换，返回替换次数。
    /// `replace_all=false` 时 `old_string` 必须唯一。
    async fn edit(
        &self,
        rc: &RuntimeContext,
        path: &str,
        old_string: &str,
        new_string: &str,
        replace_all: bool,
    ) -> Result<usize, String>;

    /// 递归搜索文件内容的字面量模式（大小写敏感；`glob` 按文件名过滤）。
    async fn grep(
        &self,
        rc: &RuntimeContext,
        pattern: &str,
        path: Option<&str>,
        glob: Option<&str>,
    ) -> Result<Vec<GrepMatch>, String>;

    /// 在 `cwd`（工作区相对目录）下按模式匹配文件。
    async fn glob(
        &self,
        rc: &RuntimeContext,
        pattern: &str,
        cwd: &str,
    ) -> Result<Vec<FileInfo>, String>;
    /// 批量写入（覆盖）文件。
    async fn upload_files(
        &self,
        rc: &RuntimeContext,
        files: &[(String, Vec<u8>)],
    ) -> Result<(), String>;
    /// 批量下载文件字节（单文件失败不拖垮整批，逐项报错）。
    async fn download_files(
        &self,
        rc: &RuntimeContext,
        paths: &[String],
    ) -> Result<Vec<DownloadedFile>, String>;
    /// 覆写单个文本文件。
    async fn write(&self, rc: &RuntimeContext, path: &str, content: &str) -> Result<(), String>;
    /// 删除文件。
    async fn delete(&self, rc: &RuntimeContext, path: &str) -> Result<(), String>;
    /// 移动/重命名。
    async fn move_path(
        &self,
        rc: &RuntimeContext,
        from: &str,
        to: &str,
    ) -> Result<(), String>;
}

/// 按行分页：`offset` 0-indexed 起始行，`limit` 最大行数（0 = 全部）。
fn paginate_lines(content: &str, offset: usize, limit: usize) -> String {
    let lines: Vec<&str> = content.lines().collect();
    if offset >= lines.len() {
        return String::new();
    }
    let end = if limit == 0 {
        lines.len()
    } else {
        (offset + limit).min(lines.len())
    };
    lines[offset..end].join("\n")
}

/// glob 递归遍历的目录访问上限（防巨型仓库失控）。
const GLOB_WALK_LIMIT: usize = 20_000;
/// glob 递归遍历跳过的目录（依赖与构建产物：噪音大且拖慢遍历）。
const GLOB_SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "dist",
    "build",
    ".idea",
    ".easyClaw",
    ".venv",
    "__pycache__",
];

/// 本地磁盘实现：以工作区根为界（对应 Java `LocalFilesystem`）。
///
/// `mode` 控制绝对路径的处理（见 [`LocalFsMode`]）；`Rooted` 模式用 [`PathPolicy`]
/// 圈定可访问的宿主目录（Claude-Code 风格「项目 + 附加目录」白名单）。
pub struct LocalFilesystem {
    root: PathBuf,
    mode: LocalFsMode,
    policy: PathPolicy,
    /// grep 单文件大小上限（字节；对齐 Java `maxFileSizeBytes`，默认 5 MiB）。
    max_grep_file_bytes: u64,
}

impl LocalFilesystem {
    /// 以 `root` 为工作区根构建；`Rooted` 模式下 `policy` 圈定绝对路径白名单。
    pub fn new(root: impl Into<PathBuf>, mode: LocalFsMode, policy: PathPolicy) -> Self {
        Self {
            root: root.into(),
            mode,
            policy,
            max_grep_file_bytes: 5 * 1024 * 1024,
        }
    }

    fn resolve(&self, path: &str) -> Result<PathBuf, String> {
        let portable = path.replace('\\', "/");
        let candidate = Path::new(&portable);
        if candidate.is_absolute() {
            match self.mode {
                LocalFsMode::Sandboxed => {
                    return Err(format!("absolute path not allowed in sandboxed mode: {path}"));
                }
                LocalFsMode::Rooted => {
                    if !self.policy.is_allowed(candidate) {
                        return Err(format!("absolute path outside path policy: {path}"));
                    }
                }
                LocalFsMode::Unrestricted => {}
            }
            return Ok(candidate.to_path_buf());
        }
        let resolved = self.root.join(&portable);
        let normalized = normalize_absolute(&resolved);
        if !normalized.starts_with(normalize_absolute(&self.root)) {
            return Err(format!("path escapes workspace root: {path}"));
        }
        Ok(normalized)
    }
}

#[async_trait::async_trait]
impl Filesystem for LocalFilesystem {
    async fn read(&self, _rc: &RuntimeContext, path: &str) -> Result<Option<String>, String> {
        let resolved = self.resolve(path)?;
        match tokio::fs::read_to_string(&resolved).await {
            Ok(content) => Ok(Some(content)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    async fn exists(&self, _rc: &RuntimeContext, path: &str) -> Result<bool, String> {
        let resolved = self.resolve(path)?;
        Ok(resolved.is_file())
    }

    async fn ls(&self, _rc: &RuntimeContext, path: &str) -> Result<Vec<FileInfo>, String> {
        let dir = self.resolve(path)?;
        if !dir.exists() {
            return Err(format!("Path does not exist: {path}"));
        }
        if !dir.is_dir() {
            return Err(format!("Not a directory: {path}"));
        }
        let mut out = Vec::new();
        let mut entries = tokio::fs::read_dir(&dir)
            .await
            .map_err(|error| error.to_string())?;
        while let Some(entry) = entries.next_entry().await.map_err(|e| e.to_string())? {
            let meta = match entry.metadata().await {
                Ok(meta) => meta,
                Err(_) => continue,
            };
            let name = entry.file_name().to_string_lossy().to_string();
            let modified_at = meta
                .modified()
                .ok()
                .and_then(|time| {
                    time.duration_since(std::time::UNIX_EPOCH)
                        .ok()
                        .map(|d| d.as_millis() as i64)
                });
            let (entry_path, is_directory, size) = if meta.is_dir() {
                (format!("{name}/"), true, None)
            } else {
                (name.clone(), false, Some(meta.len()))
            };
            out.push(FileInfo {
                path: entry_path,
                size,
                is_directory,
                modified_at,
            });
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    async fn edit(
        &self,
        _rc: &RuntimeContext,
        path: &str,
        old_string: &str,
        new_string: &str,
        replace_all: bool,
    ) -> Result<usize, String> {
        let resolved = self.resolve(path)?;
        let content = tokio::fs::read_to_string(&resolved)
            .await
            .map_err(|error| error.to_string())?;
        // 行尾宽容：read_file 经 lines().join("\n") 返回，CRLF 文件在 LLM 眼里
        // 是 LF——它照抄回的 old_string 与磁盘逐字节比对必然失败（Windows 项目
        // 高频）。精确匹配失败时按文件主行尾归一化 old/new 重试，替换仍保持
        // 文件原有行尾风格；精确匹配成功则行为与从前完全一致。
        let (old_string, new_string, occurrences) = match content.matches(old_string).count() {
            0 => {
                let crlf = content.contains("\r\n");
                let normalize = |s: &str| -> String {
                    if crlf {
                        s.replace("\r\n", "\n").replace('\n', "\r\n")
                    } else {
                        s.replace("\r\n", "\n")
                    }
                };
                let old_normalized = normalize(old_string);
                let count = content.matches(&old_normalized).count();
                if count == 0 {
                    return Err(format!("String not found in file: {path}"));
                }
                (old_normalized, normalize(new_string), count)
            }
            n => (old_string.to_string(), new_string.to_string(), n),
        };
        if !replace_all && occurrences > 1 {
            return Err(format!(
                "Found {occurrences} occurrences of the string in {path}; \
                 add surrounding context to make it unique or set replace_all=true"
            ));
        }
        let updated = if replace_all {
            content.replace(&old_string, &new_string)
        } else {
            content.replacen(&old_string, &new_string, 1)
        };
        tokio::fs::write(&resolved, updated)
            .await
            .map_err(|error| error.to_string())?;
        Ok(occurrences)
    }

    async fn grep(
        &self,
        _rc: &RuntimeContext,
        pattern: &str,
        path: Option<&str>,
        glob: Option<&str>,
    ) -> Result<Vec<GrepMatch>, String> {
        let base = self.resolve(path.unwrap_or("."))?;
        if !base.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let mut stack = vec![base.clone()];
        while let Some(dir) = stack.pop() {
            let mut entries = match tokio::fs::read_dir(&dir).await {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            while let Some(entry) = entries.next_entry().await.map_err(|e| e.to_string())? {
                let entry_path = entry.path();
                let meta = match entry.metadata().await {
                    Ok(meta) => meta,
                    Err(_) => continue,
                };
                if meta.is_dir() {
                    stack.push(entry_path);
                    continue;
                }
                if !meta.is_file() || meta.len() > self.max_grep_file_bytes {
                    continue;
                }
                let name = entry
                    .file_name()
                    .to_string_lossy()
                    .to_string();
                if let Some(glob_pattern) = glob {
                    if !glob_match(glob_pattern, &name) {
                        continue;
                    }
                }
                let content = match tokio::fs::read_to_string(&entry_path).await {
                    Ok(content) => content,
                    Err(_) => continue, // 二进制/非 UTF-8 文件跳过
                };
                let relative = entry_path
                    .strip_prefix(&self.root)
                    .unwrap_or(&entry_path)
                    .to_string_lossy()
                    .replace('\\', "/");
                for (index, line) in content.lines().enumerate() {
                    if line.contains(pattern) {
                        out.push(GrepMatch {
                            path: relative.clone(),
                            line: index + 1,
                            text: line.to_string(),
                        });
                    }
                }
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
        Ok(out)
    }

    async fn glob(
        &self,
        _rc: &RuntimeContext,
        pattern: &str,
        cwd: &str,
    ) -> Result<Vec<FileInfo>, String> {
        let dir = self.resolve(cwd)?;
        let matcher = glob_to_regex(pattern)?;
        let mut out = Vec::new();
        // 递归遍历（`**` 语义需要）；跳过依赖/构建产物目录，设访问上限防失控。
        let mut stack = vec![dir.clone()];
        let mut visited = 0usize;
        while let Some(current) = stack.pop() {
            visited += 1;
            if visited > GLOB_WALK_LIMIT {
                break;
            }
            let mut entries = tokio::fs::read_dir(&current)
                .await
                .map_err(|error| error.to_string())?;
            while let Some(entry) = entries.next_entry().await.map_err(|e| e.to_string())? {
                let file_type = entry.file_type().await.map_err(|e| e.to_string())?;
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if file_type.is_dir() {
                    if GLOB_SKIP_DIRS.contains(&name.as_str()) {
                        continue;
                    }
                    stack.push(path);
                    continue;
                }
                if !file_type.is_file() {
                    continue;
                }
                // 匹配用相对 cwd 的路径（`**` 语义基准）；返回值拼回 cwd
                // 前缀——Local 后端 glob 返回带 cwd 前缀的路径是既有约定
                // （skill 仓库的 normalize_glob_path 依赖该形态）。
                let rel = path
                    .strip_prefix(&dir)
                    .map_err(|e| e.to_string())?
                    .to_string_lossy()
                    .replace('\\', "/");
                if !matcher.is_match(&rel) {
                    continue;
                }
                let full = if cwd.trim().is_empty() || cwd == "." {
                    rel.clone()
                } else {
                    format!("{}/{}", cwd.trim_end_matches('/'), rel)
                };
                let meta = entry.metadata().await.map_err(|e| e.to_string())?;
                let modified_at = meta
                    .modified()
                    .ok()
                    .and_then(|time| {
                        time.duration_since(std::time::UNIX_EPOCH)
                            .ok()
                            .map(|d| d.as_millis() as i64)
                    });
                out.push(FileInfo {
                    path: full,
                    size: Some(meta.len()),
                    is_directory: false,
                    modified_at,
                });
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    async fn upload_files(
        &self,
        _rc: &RuntimeContext,
        files: &[(String, Vec<u8>)],
    ) -> Result<(), String> {
        for (path, bytes) in files {
            let resolved = self.resolve(path)?;
            if let Some(parent) = resolved.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            tokio::fs::write(&resolved, bytes)
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    async fn download_files(
        &self,
        _rc: &RuntimeContext,
        paths: &[String],
    ) -> Result<Vec<DownloadedFile>, String> {
        let mut out = Vec::with_capacity(paths.len());
        for path in paths {
            let content = match self.resolve(path) {
                Ok(resolved) => match tokio::fs::read(&resolved).await {
                    Ok(bytes) => Ok(bytes),
                    Err(error) => Err(error.to_string()),
                },
                Err(error) => Err(error),
            };
            out.push(DownloadedFile {
                path: path.clone(),
                content,
            });
        }
        Ok(out)
    }

    async fn write(&self, rc: &RuntimeContext, path: &str, content: &str) -> Result<(), String> {
        self.upload_files(rc, &[(path.to_string(), content.as_bytes().to_vec())])
            .await
    }

    async fn delete(&self, _rc: &RuntimeContext, path: &str) -> Result<(), String> {
        let resolved = self.resolve(path)?;
        match tokio::fs::remove_file(&resolved).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }

    async fn move_path(&self, _rc: &RuntimeContext, from: &str, to: &str) -> Result<(), String> {
        let source = self.resolve(from)?;
        let target = self.resolve(to)?;
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| error.to_string())?;
        }
        tokio::fs::rename(&source, &target)
            .await
            .map_err(|error| error.to_string())
    }
}

/// glob → 正则编译：`**/` 匹配零或多层目录段、`**` 跨段通配、
/// `*` 段内通配（不跨 `/`）、`?` 单字符；其余字符按字面转义。
/// 匹配对象是 `/` 分隔的相对路径（或裸文件名）。
pub(crate) fn glob_to_regex(pattern: &str) -> Result<regex::Regex, String> {
    let mut re = String::from("^");
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if i + 1 < chars.len() && chars[i + 1] == '*' => {
                if i + 2 < chars.len() && chars[i + 2] == '/' {
                    re.push_str("(?:[^/]*/)*");
                    i += 3;
                } else {
                    re.push_str(".*");
                    i += 2;
                }
            }
            '*' => {
                re.push_str("[^/]*");
                i += 1;
            }
            '?' => {
                re.push_str("[^/]");
                i += 1;
            }
            other => {
                re.push_str(&regex::escape(&other.to_string()));
                i += 1;
            }
        }
    }
    re.push('$');
    regex::Regex::new(&re).map_err(|e| format!("invalid glob pattern `{pattern}`: {e}"))
}

/// glob 匹配（每次调用编译正则；调用方在循环里过滤时应先编译一次复用）。
pub(crate) fn glob_match(pattern: &str, name: &str) -> bool {
    match glob_to_regex(pattern) {
        Ok(re) => re.is_match(name),
        Err(_) => pattern == name,
    }
}

/// 工作区管理器：两层读（filesystem 优先、本地回退）+ 全走 filesystem 的写
/// （对应 Java `WorkspaceManager`）。
///
/// 进程内 per-path 锁串行化「读-改-写」；跨副本是 last-writer-wins（真正的乐观并发
/// 在状态库的 `save_if_version` 上，与 Java 一致）。
pub struct WorkspaceManager {
    workspace: PathBuf,
    filesystem: Option<Arc<dyn Filesystem>>,
    path_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl WorkspaceManager {
    /// 只用本地磁盘（无 filesystem 层）。
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            filesystem: None,
            path_locks: Mutex::new(HashMap::new()),
        }
    }

    /// 本地磁盘 + 可注入的 filesystem 层（两层读架构）。
    pub fn with_filesystem(
        workspace: impl Into<PathBuf>,
        filesystem: Arc<dyn Filesystem>,
    ) -> Self {
        Self {
            workspace: workspace.into(),
            filesystem: Some(filesystem),
            path_locks: Mutex::new(HashMap::new()),
        }
    }

    /// 工作区根目录。
    pub fn root(&self) -> &Path {
        &self.workspace
    }

    /// 注入的 filesystem 层。
    pub fn filesystem(&self) -> Option<&Arc<dyn Filesystem>> {
        self.filesystem.as_ref()
    }

    /// per-path 异步锁（tokio Mutex：读-合并-写必须跨 await 原子，std Mutex 跨 await 有死锁风险）。
    fn lock_for(&self, path: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.path_locks.lock().unwrap_or_else(|e| e.into_inner());
        locks
            .entry(path.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// 构建时校验：目录存在、AGENTS.md 存在（缺失只告警不报错，与 Java 一致）。
    pub fn validate(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if !self.workspace.is_dir() {
            warnings.push(format!(
                "Workspace directory does not exist: {}. Please create it and add AGENTS.md.",
                self.workspace.display()
            ));
            return warnings;
        }
        if !self.workspace.join(constants::AGENTS_MD).is_file() {
            warnings.push(format!(
                "AGENTS.md not found in workspace: {}. AGENTS.md defines persona and local conventions for the agent.",
                self.workspace.display()
            ));
        }
        warnings
    }

    /// 运行时数据路径解析（本批无命名空间，直接拼相对路径）。
    pub fn resolve_runtime_data_path(&self, relative_path: &str) -> PathBuf {
        self.workspace.join(relative_path)
    }

    /// 读 AGENTS.md；不存在返回空串。
    pub async fn read_agents_md(&self, rc: &RuntimeContext) -> String {
        self.read_with_override(rc, constants::AGENTS_MD).await
    }

    /// 读 knowledge/KNOWLEDGE.md。
    pub async fn read_knowledge_md(&self, rc: &RuntimeContext) -> String {
        self.read_with_override(rc, &format!("{}/{}", constants::KNOWLEDGE_DIR, constants::KNOWLEDGE_MD))
            .await
    }

    /// 读 MEMORY.md。
    pub async fn read_memory_md(&self, rc: &RuntimeContext) -> String {
        self.read_with_override(rc, constants::MEMORY_MD).await
    }

    /// 读工作区下任意 UTF-8 文件（带路径安全校验）。
    pub async fn read_managed_workspace_file_utf8(
        &self,
        rc: &RuntimeContext,
        relative_path: &str,
    ) -> String {
        let normalized = normalize_relative_path(relative_path);
        if normalized.is_empty() {
            return String::new();
        }
        if validate_path(&normalized).is_err() {
            return String::new();
        }
        self.read_with_override(rc, &normalized).await
    }

    /// 追加 UTF-8 文本到工作区相对文件（自动建父目录；per-path 锁串行化）。
    pub async fn append_utf8_workspace_relative(
        &self,
        rc: &RuntimeContext,
        relative_path: &str,
        content: &str,
    ) -> Result<(), String> {
        let normalized = require_safe_relative_path(relative_path)?;
        if normalized.is_empty() {
            return Ok(());
        }
        let lock = self.lock_for(&normalized);
        let _guard = lock.lock().await;
        match &self.filesystem {
            None => self.append_local_file(&normalized, content),
            Some(fs) => {
                let existing = fs.read(rc, &normalized).await?.unwrap_or_default();
                let merged = format!("{existing}{content}");
                fs.upload_files(
                    rc,
                    &[(normalized.clone(), merged.into_bytes())],
                )
                .await
            }
        }
    }

    /// 覆写工作区相对 UTF-8 文件。
    pub async fn write_utf8_workspace_relative(
        &self,
        rc: &RuntimeContext,
        relative_path: &str,
        content: &str,
    ) -> Result<(), String> {
        let normalized = require_safe_relative_path(relative_path)?;
        if normalized.is_empty() {
            return Ok(());
        }
        match &self.filesystem {
            None => self.write_local_file(&normalized, content),
            Some(fs) => {
                fs.upload_files(rc, &[(normalized.clone(), content.as_bytes().to_vec())])
                    .await
            }
        }
    }

    /// 列出全部 memory 文件（MEMORY.md + memory/*.md；filesystem 与本地取并集去重）。
    pub async fn list_memory_file_paths(&self, rc: &RuntimeContext) -> Vec<String> {
        let mut paths = HashSet::new();
        if let Some(fs) = &self.filesystem {
            if fs.read(rc, constants::MEMORY_MD).await.is_ok() {
                // read 成功不代表存在；用 exists 精确判断
            }
            if fs
                .exists(rc, constants::MEMORY_MD)
                .await
                .unwrap_or(false)
            {
                paths.insert(constants::MEMORY_MD.to_string());
            }
            if let Ok(matches) = fs.glob(rc, "*.md", constants::MEMORY_DIR).await {
                for info in matches {
                    // glob 契约返回相对 cwd（memory/）的路径，统一拼回目录前缀，
                    // 与本地磁盘分支的 `memory/<name>` 形态一致。
                    let name = info.path.trim().trim_start_matches('/');
                    if name.is_empty() || info.is_directory {
                        continue;
                    }
                    paths.insert(format!("{}/{}", constants::MEMORY_DIR, name));
                }
            }
        }
        if self
            .workspace
            .join(constants::MEMORY_MD)
            .is_file()
        {
            paths.insert(constants::MEMORY_MD.to_string());
        }
        let memory_dir = self.workspace.join(constants::MEMORY_DIR);
        if let Ok(entries) = std::fs::read_dir(&memory_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file()
                    && path
                        .extension()
                        .is_some_and(|ext| ext == "md")
                {
                    if let Some(name) = path.file_name() {
                        paths.insert(format!(
                            "{}/{}",
                            constants::MEMORY_DIR,
                            name.to_string_lossy()
                        ));
                    }
                }
            }
        }
        let mut out: Vec<String> = paths.into_iter().collect();
        out.sort();
        out
    }

    /// 列出 knowledge 目录树下的全部文件（并集去重）。
    pub async fn list_knowledge_files(&self, rc: &RuntimeContext) -> Vec<PathBuf> {
        let mut relative = HashSet::new();
        if let Some(fs) = &self.filesystem {
            if let Ok(matches) = fs.glob(rc, "*", constants::KNOWLEDGE_DIR).await {
                for info in matches {
                    let rel = normalize_relative_path(info.path.trim());
                    if !rel.is_empty() {
                        relative.insert(rel);
                    }
                }
            }
        }
        let knowledge_dir = self.workspace.join(constants::KNOWLEDGE_DIR);
        collect_files_recursive(&knowledge_dir, &knowledge_dir, &mut relative);
        let mut out: Vec<PathBuf> = relative
            .into_iter()
            .map(|rel| self.workspace.join(&rel))
            .collect();
        out.sort();
        out
    }

    // ==================== 两层读写内部实现 ====================

    async fn read_with_override(&self, rc: &RuntimeContext, relative_path: &str) -> String {
        if let Some(fs) = &self.filesystem {
            if let Ok(Some(content)) = fs.read(rc, relative_path).await {
                if !content.is_empty() {
                    return content;
                }
            }
        }
        read_file_quietly(&self.workspace.join(relative_path))
    }

    fn append_local_file(&self, relative_path: &str, content: &str) -> Result<(), String> {
        let local = self.workspace.join(relative_path);
        if !local.starts_with(&self.workspace) {
            return Err(format!("Refusing to write outside workspace: {relative_path}"));
        }
        if let Some(parent) = local.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&local)
            .map_err(|error| error.to_string())?;
        file.write_all(content.as_bytes())
            .map_err(|error| error.to_string())
    }

    /// 原子覆写本地文件：先写临时文件再改名，防止并发读者看到半截内容。
    fn write_local_file(&self, relative_path: &str, content: &str) -> Result<(), String> {
        let local = self.workspace.join(relative_path);
        if !local.starts_with(&self.workspace) {
            return Err(format!("Refusing to write outside workspace: {relative_path}"));
        }
        if let Some(parent) = local.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let temp = local.with_extension(format!(
            "tmp.{}",
            uuid::Uuid::new_v4().simple()
        ));
        let result = (|| -> Result<(), String> {
            std::fs::write(&temp, content).map_err(|error| error.to_string())?;
            // Windows 上 rename 不能覆盖已存在文件：先删目标再改名（尽力原子）。
            if local.is_file() {
                std::fs::remove_file(&local).map_err(|error| error.to_string())?;
            }
            std::fs::rename(&temp, &local).map_err(|error| error.to_string())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }

    // ==================== 任务记录存储（对应 Java WorkspaceManager 的
    // TaskRecord 职责：tasks/ 目录下每任务一个 JSON 文件） ====================

    fn task_record_path(&self, task_id: &str) -> Result<PathBuf, String> {
        // taskId 直接用作文件名：只允许安全字符（Java 侧为 UUID）
        if task_id.is_empty()
            || !task_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(format!("Invalid task id: {task_id}"));
        }
        Ok(self
            .workspace
            .join(constants::TASKS_DIR)
            .join(format!("{task_id}.json")))
    }

    /// 读取任务记录；不存在或损坏时返回 `None`。
    pub fn read_task_record(&self, task_id: &str) -> Option<TaskRecord> {
        let path = self.task_record_path(task_id).ok()?;
        let content = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// 写入任务记录（原子写：临时文件 + 改名）。
    pub fn write_task_record(&self, record: &TaskRecord) -> Result<(), String> {
        let path = self.task_record_path(&record.task_id)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let json = serde_json::to_string_pretty(record).map_err(|error| error.to_string())?;
        let temp = path.with_extension(format!("json.tmp.{}", uuid::Uuid::new_v4().simple()));
        let result = (|| -> Result<(), String> {
            std::fs::write(&temp, json).map_err(|error| error.to_string())?;
            if path.is_file() {
                std::fs::remove_file(&path).map_err(|error| error.to_string())?;
            }
            std::fs::rename(&temp, &path).map_err(|error| error.to_string())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }

    /// 列出全部任务记录（按 taskId 字典序）。
    pub fn list_task_records(&self) -> Vec<TaskRecord> {
        let tasks_dir = self.workspace.join(constants::TASKS_DIR);
        let Ok(entries) = std::fs::read_dir(&tasks_dir) else {
            return Vec::new();
        };
        let mut records = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file()
                || !path
                    .extension()
                    .is_some_and(|ext| ext == "json")
            {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(record) = serde_json::from_str::<TaskRecord>(&content) {
                    records.push(record);
                }
            }
        }
        records.sort_by(|a, b| a.task_id.cmp(&b.task_id));
        records
    }

    /// 删除任务记录文件；不存在视为成功。
    pub fn delete_task_record(&self, task_id: &str) -> Result<(), String> {
        let path = self.task_record_path(task_id)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }

    /// 孤儿清扫（对应 Java `failOrphanedTasks`）：把 `tasks/` 下仍处于
    /// RUNNING / PENDING、且不在 `active_task_ids` 里的记录标为 FAILED。
    ///
    /// 进程重启后，上一进程的执行体永远回不来——依据「未知 ≠ 成功」，
    /// 这些任务必须显式失败而不是悬挂。返回被清扫的记录。
    pub fn fail_orphaned_tasks(&self, active_task_ids: &[String]) -> Vec<TaskRecord> {
        let mut swept = Vec::new();
        for mut record in self.list_task_records() {
            if record.status.is_terminal() || active_task_ids.contains(&record.task_id) {
                continue;
            }
            record.status = TaskStatus::Failed;
            record.error_message = Some("orphaned: owning process no longer running".to_string());
            record.last_updated_at = Some(crate::task::now_millis());
            if self.write_task_record(&record).is_ok() {
                swept.push(record);
            }
        }
        swept
    }
}

/// 规范化相对路径：统一分隔符、去前导斜杠（对应 Java `normalizeRelativePath`）。
pub fn normalize_relative_path(relative_path: &str) -> String {
    if relative_path.trim().is_empty() {
        return String::new();
    }
    let mut s = relative_path.replace('\\', "/");
    while s.starts_with('/') {
        s = s[1..].to_string();
    }
    s.trim().to_string()
}

fn require_safe_relative_path(relative_path: &str) -> Result<String, String> {
    let normalized = normalize_relative_path(relative_path);
    if !normalized.is_empty() {
        validate_path(&normalized)?;
    }
    Ok(normalized)
}

fn read_file_quietly(path: &Path) -> String {
    if !path.is_file() {
        return String::new();
    }
    std::fs::read_to_string(path).unwrap_or_default()
}

fn collect_files_recursive(dir: &Path, root: &Path, out: &mut HashSet<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files_recursive(&path, root, out);
        } else if path.is_file() {
            if let Ok(rel) = path.strip_prefix(root) {
                out.insert(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::TaskRecord;

    #[test]
    fn glob_matches_recursive_and_single_star_semantics() {
        // `**/` 跨任意深度（含零层）
        assert!(glob_match("**/GenericRepositoryImpl.java", "GenericRepositoryImpl.java"));
        assert!(glob_match(
            "**/GenericRepositoryImpl.java",
            "a/b/c/GenericRepositoryImpl.java"
        ));
        assert!(!glob_match("**/GenericRepositoryImpl.java", "Other.java"));
        // `*` 不跨目录段
        assert!(glob_match("*.java", "Foo.java"));
        assert!(!glob_match("*.java", "a/Foo.java"));
        assert!(glob_match("src/**/*.java", "src/a/b/Foo.java"));
        assert!(glob_match("src/**/*.java", "src/Foo.java"));
        // `?` 单字符
        assert!(glob_match("file?.txt", "file1.txt"));
        assert!(!glob_match("file?.txt", "file10.txt"));
        // 旧用法兼容：`*.md` / `*.json` 文件名匹配
        assert!(glob_match("*.md", "AGENTS.md"));
        assert!(glob_match("*.json", "config.json"));
        // 字面量
        assert!(glob_match("exact.txt", "exact.txt"));
        assert!(!glob_match("exact.txt", "other.txt"));
    }

    fn temp_workspace(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "harness-ws-{}-{}",
            tag,
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn path_policy_allows_only_registered_roots() {
        let root = temp_workspace("policy");
        let nested = root.join("project");
        std::fs::create_dir_all(&nested).unwrap();
        let policy = PathPolicy::of([&nested]);
        assert!(policy.is_allowed(&nested.join("src").join("main.rs")));
        assert!(!policy.is_allowed(&root.join("outside.txt")));
        assert!(!policy.is_allowed(Path::new("relative/path.txt")));
        assert!(PathPolicy::empty().is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn normalizer_strips_registered_prefixes() {
        let normalizer = WorkspacePathNormalizer::of("F:/work/project");
        assert_eq!(
            normalizer.normalize("F:/work/project/src/main.rs"),
            "src/main.rs"
        );
        assert_eq!(normalizer.normalize("F:/work/project"), ".");
        assert_eq!(normalizer.normalize("other/file.txt"), "other/file.txt");
        let multi = WorkspacePathNormalizer::of_many(["/a", "/b"]);
        assert_eq!(multi.normalize("/b/c.txt"), "c.txt");
        assert_eq!(multi.prefixes().len(), 2);
    }

    #[test]
    fn validate_path_rejects_traversal() {
        assert!(validate_path("a/b.txt").is_ok());
        assert!(validate_path("a/../b.txt").is_err());
        assert!(validate_path("").is_err());
        assert!(validate_path("  ").is_err());
    }

    #[tokio::test]
    async fn local_filesystem_read_write_glob_roundtrip() {
        let root = temp_workspace("fs");
        let fs = LocalFilesystem::new(&root, LocalFsMode::Rooted, PathPolicy::empty());
        let rc = RuntimeContext::new();
        fs.write(&rc, "notes/hello.txt", "hello").await.unwrap();
        assert_eq!(
            fs.read(&rc, "notes/hello.txt").await.unwrap(),
            Some("hello".to_string())
        );
        assert!(fs.exists(&rc, "notes/hello.txt").await.unwrap());
        fs.write(&rc, "notes/other.md", "x").await.unwrap();
        let matches = fs.glob(&rc, "*.md", "notes").await.unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, "notes/other.md");
        fs.move_path(&rc, "notes/hello.txt", "notes/moved.txt")
            .await
            .unwrap();
        assert_eq!(
            fs.read(&rc, "notes/moved.txt").await.unwrap(),
            Some("hello".to_string())
        );
        fs.delete(&rc, "notes/moved.txt").await.unwrap();
        assert!(!fs.exists(&rc, "notes/moved.txt").await.unwrap());
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn local_filesystem_edit_tolerates_line_endings() {
        let root = temp_workspace("edit-eol");
        let fs = LocalFilesystem::new(&root, LocalFsMode::Rooted, PathPolicy::empty());
        let rc = RuntimeContext::new();
        // CRLF 文件 + LF old_string（LLM 从 read_file 看到的行尾就是 LF）。
        fs.write(
            &rc,
            "page.jsp",
            "<div>\r\n  <p>hello</p>\r\n</div>\r\n",
        )
        .await
        .unwrap();
        let n = fs
            .edit(&rc, "page.jsp", "<p>hello</p>", "<p>hi</p>", false)
            .await
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            fs.read(&rc, "page.jsp").await.unwrap().unwrap(),
            "<div>\r\n  <p>hi</p>\r\n</div>\r\n"
        );
        // 反向：LF 文件 + CRLF old_string 同样宽容，替换保持文件 LF 风格。
        fs.write(&rc, "plain.txt", "a\nb\nc\n").await.unwrap();
        fs.edit(&rc, "plain.txt", "a\r\nb", "x\r\ny", false)
            .await
            .unwrap();
        assert_eq!(
            fs.read(&rc, "plain.txt").await.unwrap().unwrap(),
            "x\ny\nc\n"
        );
        // 真正不存在的内容仍报原错误。
        assert!(fs.edit(&rc, "plain.txt", "nope", "x", false).await.is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn local_filesystem_rejects_escape_and_unrooted_absolute() {
        let root = temp_workspace("escape");
        let fs = LocalFilesystem::new(&root, LocalFsMode::Rooted, PathPolicy::empty());
        let rc = RuntimeContext::new();
        assert!(fs.read(&rc, "../outside.txt").await.is_err());
        assert!(fs.read(&rc, "C:/Windows/win.ini").await.is_err());
        let sandboxed = LocalFilesystem::new(&root, LocalFsMode::Sandboxed, PathPolicy::empty());
        assert!(sandboxed.read(&rc, "C:/Windows/win.ini").await.is_err());
        let unrestricted =
            LocalFilesystem::new(&root, LocalFsMode::Unrestricted, PathPolicy::empty());
        // Unrestricted 放行绝对路径（文件不存在 → None 而非报错）
        assert_eq!(
            unrestricted
                .read(&rc, "F:/definitely/not/here.txt")
                .await
                .unwrap(),
            None
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn workspace_manager_write_read_append_roundtrip() {
        let root = temp_workspace("mgr");
        std::fs::write(root.join("AGENTS.md"), "# agent").unwrap();
        let manager = WorkspaceManager::new(&root);
        assert!(manager.validate().is_empty());
        let rc = RuntimeContext::new();
        manager
            .write_utf8_workspace_relative(&rc, "MEMORY.md", "- fact one\n")
            .await
            .unwrap();
        manager
            .append_utf8_workspace_relative(&rc, "MEMORY.md", "- fact two\n")
            .await
            .unwrap();
        assert_eq!(
            manager.read_memory_md(&rc).await,
            "- fact one\n- fact two\n"
        );
        // 越界写拒绝
        assert!(manager
            .write_utf8_workspace_relative(&rc, "../escape.txt", "x")
            .await
            .is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn workspace_manager_lists_memory_files() {
        let root = temp_workspace("mem");
        std::fs::write(root.join("MEMORY.md"), "root").unwrap();
        std::fs::create_dir_all(root.join("memory")).unwrap();
        std::fs::write(root.join("memory").join("2026-01-01.md"), "daily").unwrap();
        std::fs::write(root.join("memory").join("notes.txt"), "ignored").unwrap();
        let manager = WorkspaceManager::new(&root);
        let rc = RuntimeContext::new();
        let paths = manager.list_memory_file_paths(&rc).await;
        assert_eq!(
            paths,
            vec![
                "MEMORY.md".to_string(),
                "memory/2026-01-01.md".to_string()
            ]
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn task_record_storage_roundtrip_and_orphan_sweep() {
        let root = temp_workspace("tasks");
        let manager = WorkspaceManager::new(&root);
        let mut record = TaskRecord::pending("task-1", "coder", "parent", "sess-1", "sess-1");
        record.status = TaskStatus::Running;
        manager.write_task_record(&record).unwrap();
        let mut other = TaskRecord::pending("task-2", "coder", "parent", "sess-1", "sess-1");
        other.status = TaskStatus::Completed;
        manager.write_task_record(&other).unwrap();

        assert!(manager.read_task_record("task-1").is_some());
        assert!(manager.read_task_record("missing").is_none());
        assert_eq!(manager.list_task_records().len(), 2);

        // task-1 不在 active 列表（模拟进程重启后执行体丢失）→ 清扫为 FAILED；
        // task-2 已终态，不受影响
        let swept = manager.fail_orphaned_tasks(&[]);
        assert_eq!(swept.len(), 1);
        assert_eq!(swept[0].task_id, "task-1");
        assert_eq!(swept[0].status, TaskStatus::Failed);
        let reloaded = manager.read_task_record("task-1").unwrap();
        assert_eq!(reloaded.status, TaskStatus::Failed);
        assert!(reloaded.error_message.unwrap().contains("orphaned"));

        manager.delete_task_record("task-1").unwrap();
        assert!(manager.read_task_record("task-1").is_none());
        // 非法 taskId 拒绝（路径穿越）
        assert!(manager.read_task_record("../evil").is_none());
        std::fs::remove_dir_all(&root).ok();
    }
}
