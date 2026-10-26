//! 组合式文件系统后端（对应 Java harness `filesystem` 包的 Overlay/Composite）。
//!
//! - [`OverlayFilesystem`]：双层叠加（upper 可写层 + lower 共享只读层），
//!   读合并、写上浮、编辑/移动 copy-on-write（对应 Java `OverlayFilesystem`）。
//! - [`CompositeFilesystem`]：按最长前缀路由到多个后端，目录路由与精确文件路由
//!   两种形态，结果路径重映射回原前缀形式（对应 Java `CompositeFilesystem`）。
//! - [`InMemoryFilesystem`]：内存实现，测试与路由目标用。

use std::collections::BTreeMap;
use std::sync::Arc;

use agentscope_core::RuntimeContext;

use crate::workspace::{DownloadedFile, FileInfo, Filesystem, GrepMatch};

/// 双层叠加：`upper` 为用户可写层，`lower` 为共享只读层。
///
/// 语义（对齐 Java `OverlayFilesystem`）：
/// - `ls`/`grep`/`glob`：双层结果合并，upper 条目覆盖同键 lower 条目；
/// - `read`/`exists`：upper 优先，miss 时落 lower；
/// - `write`：恒写 upper；
/// - `edit`/`move`：对 lower 文件 copy-on-write（先读 lower 全文写 upper，再操作 upper）；
/// - `delete`：只删 upper；lower 文件不可删（共享层保护）。
pub struct OverlayFilesystem {
    upper: Arc<dyn Filesystem>,
    lower: Arc<dyn Filesystem>,
}

impl OverlayFilesystem {
    pub fn new(upper: Arc<dyn Filesystem>, lower: Arc<dyn Filesystem>) -> Self {
        Self { upper, lower }
    }

    /// 用户可写层。
    pub fn upper(&self) -> &Arc<dyn Filesystem> {
        &self.upper
    }

    /// 共享层（经此 overlay 只读）。
    pub fn lower(&self) -> &Arc<dyn Filesystem> {
        &self.lower
    }
}

#[async_trait::async_trait]
impl Filesystem for OverlayFilesystem {
    async fn read(&self, rc: &RuntimeContext, path: &str) -> Result<Option<String>, String> {
        if self.upper.exists(rc, path).await? {
            return self.upper.read(rc, path).await;
        }
        self.lower.read(rc, path).await
    }

    async fn exists(&self, rc: &RuntimeContext, path: &str) -> Result<bool, String> {
        Ok(self.upper.exists(rc, path).await? || self.lower.exists(rc, path).await?)
    }

    async fn ls(&self, rc: &RuntimeContext, path: &str) -> Result<Vec<FileInfo>, String> {
        let upper_result = self.upper.ls(rc, path).await;
        let lower_result = self.lower.ls(rc, path).await;
        if upper_result.is_err() && lower_result.is_err() {
            return lower_result;
        }
        let mut merged: BTreeMap<String, FileInfo> = BTreeMap::new();
        if let Ok(entries) = &lower_result {
            for info in entries {
                merged.insert(info.path.clone(), info.clone());
            }
        }
        if let Ok(entries) = &upper_result {
            for info in entries {
                merged.insert(info.path.clone(), info.clone());
            }
        }
        Ok(merged.into_values().collect())
    }

    async fn edit(
        &self,
        rc: &RuntimeContext,
        path: &str,
        old_string: &str,
        new_string: &str,
        replace_all: bool,
    ) -> Result<usize, String> {
        if self.upper.exists(rc, path).await? {
            return self
                .upper
                .edit(rc, path, old_string, new_string, replace_all)
                .await;
        }
        if self.lower.exists(rc, path).await? {
            // copy-on-write：把 lower 全文搬到 upper 再编辑。
            let content = self
                .lower
                .read(rc, path)
                .await?
                .ok_or_else(|| format!("Cannot read shared file for copy-on-write: {path}"))?;
            self.upper.write(rc, path, &content).await?;
            return self
                .upper
                .edit(rc, path, old_string, new_string, replace_all)
                .await;
        }
        Err(format!("File not found: {path}"))
    }

    async fn grep(
        &self,
        rc: &RuntimeContext,
        pattern: &str,
        path: Option<&str>,
        glob: Option<&str>,
    ) -> Result<Vec<GrepMatch>, String> {
        let upper_result = self.upper.grep(rc, pattern, path, glob).await;
        let lower_result = self.lower.grep(rc, pattern, path, glob).await;
        if upper_result.is_err() && lower_result.is_err() {
            return lower_result;
        }
        let mut merged: BTreeMap<(String, usize), GrepMatch> = BTreeMap::new();
        if let Ok(matches) = &lower_result {
            for m in matches {
                merged.insert((m.path.clone(), m.line), m.clone());
            }
        }
        if let Ok(matches) = &upper_result {
            for m in matches {
                merged.insert((m.path.clone(), m.line), m.clone());
            }
        }
        Ok(merged.into_values().collect())
    }

    async fn glob(
        &self,
        rc: &RuntimeContext,
        pattern: &str,
        cwd: &str,
    ) -> Result<Vec<FileInfo>, String> {
        let upper_result = self.upper.glob(rc, pattern, cwd).await;
        let lower_result = self.lower.glob(rc, pattern, cwd).await;
        if upper_result.is_err() && lower_result.is_err() {
            return lower_result;
        }
        let mut merged: BTreeMap<String, FileInfo> = BTreeMap::new();
        if let Ok(entries) = &lower_result {
            for info in entries {
                merged.insert(info.path.clone(), info.clone());
            }
        }
        if let Ok(entries) = &upper_result {
            for info in entries {
                merged.insert(info.path.clone(), info.clone());
            }
        }
        Ok(merged.into_values().collect())
    }

    async fn upload_files(
        &self,
        rc: &RuntimeContext,
        files: &[(String, Vec<u8>)],
    ) -> Result<(), String> {
        self.upper.upload_files(rc, files).await
    }

    async fn download_files(
        &self,
        rc: &RuntimeContext,
        paths: &[String],
    ) -> Result<Vec<DownloadedFile>, String> {
        // upper 有则取 upper，否则落 lower（对齐 read 的优先级）。
        let mut out = Vec::with_capacity(paths.len());
        for path in paths {
            if self.upper.exists(rc, path).await? {
                let mut result = self.upper.download_files(rc, std::slice::from_ref(path)).await?;
                if let Some(file) = result.pop() {
                    out.push(file);
                    continue;
                }
            }
            let mut result = self.lower.download_files(rc, std::slice::from_ref(path)).await?;
            if let Some(file) = result.pop() {
                out.push(file);
            } else {
                out.push(DownloadedFile {
                    path: path.clone(),
                    content: Err(format!("File not found: {path}")),
                });
            }
        }
        Ok(out)
    }

    async fn write(&self, rc: &RuntimeContext, path: &str, content: &str) -> Result<(), String> {
        self.upper.write(rc, path, content).await
    }

    async fn delete(&self, rc: &RuntimeContext, path: &str) -> Result<(), String> {
        if self.upper.exists(rc, path).await? {
            return self.upper.delete(rc, path).await;
        }
        if self.lower.exists(rc, path).await? {
            return Err(format!("Cannot delete shared file: {path}"));
        }
        Ok(())
    }

    async fn move_path(&self, rc: &RuntimeContext, from: &str, to: &str) -> Result<(), String> {
        if self.upper.exists(rc, from).await? {
            return self.upper.move_path(rc, from, to).await;
        }
        if self.lower.exists(rc, from).await? {
            // copy-on-write move：读 lower 写 upper 目标（lower 原件保留）。
            let content = self
                .lower
                .read(rc, from)
                .await?
                .ok_or_else(|| format!("Cannot read source for move: {from}"))?;
            return self.upper.write(rc, to, &content).await;
        }
        Err(format!("Source not found: {from}"))
    }
}

/// 单条路由：`prefix` 以 `/` 结尾表示目录路由（子树整体交给后端），
/// 否则为精确文件路由。构造时按前缀长度降序排序（最长优先）。
struct RouteEntry {
    prefix: String,
    backend: Arc<dyn Filesystem>,
}

/// 路由结果：目标后端 + 后端内路径 + 命中的原始前缀（用于结果重映射）。
struct RouteResult {
    backend: Arc<dyn Filesystem>,
    backend_path: String,
    route_prefix: Option<String>,
}

/// 前缀路由组合：无路由命中时落 `default_backend`。
///
/// 语义（对齐 Java `CompositeFilesystem`）：
/// - 最长前缀优先；前导 `/` 在匹配时双方都剥掉；
/// - 目录路由把子树路径剥掉前缀后交给后端，结果路径加回前缀（重映射）；
/// - `ls("/")` 列出 default 根条目 + 各目录路由（作为目录项）+ 各精确文件路由（存在时）；
/// - 跨后端 move = read → write → delete。
pub struct CompositeFilesystem {
    default_backend: Arc<dyn Filesystem>,
    routes: Vec<RouteEntry>,
}

impl CompositeFilesystem {
    /// `routes` 为 `(prefix, backend)`；prefix 以 `/` 结尾为目录路由。
    pub fn new(
        default_backend: Arc<dyn Filesystem>,
        routes: Vec<(String, Arc<dyn Filesystem>)>,
    ) -> Self {
        let mut routes: Vec<RouteEntry> = routes
            .into_iter()
            .map(|(prefix, backend)| RouteEntry { prefix, backend })
            .collect();
        routes.sort_by_key(|entry| std::cmp::Reverse(entry.prefix.len()));
        Self {
            default_backend,
            routes,
        }
    }

    fn route_for_path(&self, path: &str) -> RouteResult {
        let match_path = strip_leading_slash(path);
        for entry in &self.routes {
            let canonical = strip_leading_slash(&entry.prefix);
            let prefix_no_slash = canonical.trim_end_matches('/');
            if match_path == prefix_no_slash {
                if canonical.ends_with('/') {
                    // 目录路由的根本身。
                    return RouteResult {
                        backend: Arc::clone(&entry.backend),
                        backend_path: "/".to_string(),
                        route_prefix: Some(entry.prefix.clone()),
                    };
                }
                // 精确文件路由：路径原样透传。
                return RouteResult {
                    backend: Arc::clone(&entry.backend),
                    backend_path: path.to_string(),
                    route_prefix: Some(entry.prefix.clone()),
                };
            }
            if canonical.ends_with('/')
                && (match_path.starts_with(&format!("{prefix_no_slash}/"))
                    || prefix_no_slash.is_empty())
            {
                // 目录路由子树：剥掉前缀交给后端。
                let backend_path = match_path
                    .strip_prefix(prefix_no_slash)
                    .unwrap_or(&match_path)
                    .trim_start_matches('/')
                    .to_string();
                let backend_path = if backend_path.is_empty() {
                    "/".to_string()
                } else {
                    format!("/{backend_path}")
                };
                return RouteResult {
                    backend: Arc::clone(&entry.backend),
                    backend_path,
                    route_prefix: Some(entry.prefix.clone()),
                };
            }
        }
        RouteResult {
            backend: Arc::clone(&self.default_backend),
            backend_path: path.to_string(),
            route_prefix: None,
        }
    }

    /// 把后端返回的路径重映射回组合视图（加回路由前缀）。
    fn remap(&self, route: &RouteResult, backend_path: &str) -> String {
        match &route.route_prefix {
            Some(prefix) => {
                let prefix_no_slash = prefix.trim_end_matches('/');
                let suffix = backend_path.trim_start_matches('/');
                if suffix.is_empty() {
                    prefix_no_slash.to_string()
                } else {
                    format!("{prefix_no_slash}/{suffix}")
                }
            }
            None => backend_path.to_string(),
        }
    }
}

fn strip_leading_slash(path: &str) -> String {
    path.strip_prefix('/').unwrap_or(path).to_string()
}

#[async_trait::async_trait]
impl Filesystem for CompositeFilesystem {
    async fn read(&self, rc: &RuntimeContext, path: &str) -> Result<Option<String>, String> {
        let route = self.route_for_path(path);
        route.backend.read(rc, &route.backend_path).await
    }

    async fn exists(&self, rc: &RuntimeContext, path: &str) -> Result<bool, String> {
        if path.trim().is_empty() {
            return Ok(false);
        }
        let route = self.route_for_path(path);
        route.backend.exists(rc, &route.backend_path).await
    }

    async fn ls(&self, rc: &RuntimeContext, path: &str) -> Result<Vec<FileInfo>, String> {
        let route = self.route_for_path(path);
        if route.route_prefix.is_some() {
            let entries = route.backend.ls(rc, &route.backend_path).await?;
            return Ok(entries
                .into_iter()
                .map(|info| FileInfo {
                    path: self.remap(&route, &info.path),
                    ..info
                })
                .collect());
        }
        // 根目录：default 根条目 + 目录路由项 + 精确文件路由项。
        let normalized = strip_leading_slash(path);
        if normalized.is_empty() || normalized == "." {
            let mut out = Vec::new();
            if let Ok(entries) = self.default_backend.ls(rc, "/").await {
                out.extend(entries);
            }
            for entry in &self.routes {
                let canonical = strip_leading_slash(&entry.prefix);
                if canonical.ends_with('/') {
                    out.push(FileInfo {
                        path: canonical.trim_end_matches('/').to_string(),
                        size: None,
                        is_directory: true,
                        modified_at: None,
                    });
                } else if entry.backend.exists(rc, &canonical).await.unwrap_or(false) {
                    out.push(FileInfo {
                        path: canonical.clone(),
                        size: None,
                        is_directory: false,
                        modified_at: None,
                    });
                }
            }
            out.sort_by(|a, b| a.path.cmp(&b.path));
            return Ok(out);
        }
        route.backend.ls(rc, &route.backend_path).await
    }

    async fn edit(
        &self,
        rc: &RuntimeContext,
        path: &str,
        old_string: &str,
        new_string: &str,
        replace_all: bool,
    ) -> Result<usize, String> {
        let route = self.route_for_path(path);
        route
            .backend
            .edit(rc, &route.backend_path, old_string, new_string, replace_all)
            .await
    }

    async fn grep(
        &self,
        rc: &RuntimeContext,
        pattern: &str,
        path: Option<&str>,
        glob: Option<&str>,
    ) -> Result<Vec<GrepMatch>, String> {
        match path {
            Some(p) => {
                let route = self.route_for_path(p);
                let mut matches = route
                    .backend
                    .grep(rc, pattern, Some(&route.backend_path), glob)
                    .await?;
                for m in &mut matches {
                    m.path = self.remap(&route, &m.path);
                }
                Ok(matches)
            }
            None => {
                // 无路径：default 全域 + 各目录路由子树。
                let mut out = self.default_backend.grep(rc, pattern, None, glob).await?;
                for entry in &self.routes {
                    let canonical = strip_leading_slash(&entry.prefix);
                    if !canonical.ends_with('/') {
                        continue;
                    }
                    let mut matches = entry
                        .backend
                        .grep(rc, pattern, None, glob)
                        .await?;
                    for m in &mut matches {
                        m.path = format!("{}/{}", canonical.trim_end_matches('/'), m.path);
                    }
                    out.extend(matches);
                }
                out.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
                Ok(out)
            }
        }
    }

    async fn glob(
        &self,
        rc: &RuntimeContext,
        pattern: &str,
        cwd: &str,
    ) -> Result<Vec<FileInfo>, String> {
        let route = self.route_for_path(cwd);
        let entries = route.backend.glob(rc, pattern, &route.backend_path).await?;
        Ok(entries
            .into_iter()
            .map(|info| FileInfo {
                path: self.remap(&route, &info.path),
                ..info
            })
            .collect())
    }

    async fn upload_files(
        &self,
        rc: &RuntimeContext,
        files: &[(String, Vec<u8>)],
    ) -> Result<(), String> {
        // 按路由逐文件写入（单文件批，路由各自正确）。
        for (path, bytes) in files {
            let route = self.route_for_path(path);
            route
                .backend
                .upload_files(rc, std::slice::from_ref(&(route.backend_path.clone(), bytes.clone())))
                .await?;
        }
        Ok(())
    }

    async fn download_files(
        &self,
        rc: &RuntimeContext,
        paths: &[String],
    ) -> Result<Vec<DownloadedFile>, String> {
        let mut out = Vec::with_capacity(paths.len());
        for path in paths {
            let route = self.route_for_path(path);
            let mut result = route
                .backend
                .download_files(rc, std::slice::from_ref(path))
                .await?;
            if let Some(mut file) = result.pop() {
                file.path = path.clone();
                out.push(file);
            } else {
                out.push(DownloadedFile {
                    path: path.clone(),
                    content: Err(format!("File not found: {path}")),
                });
            }
        }
        Ok(out)
    }

    async fn write(&self, rc: &RuntimeContext, path: &str, content: &str) -> Result<(), String> {
        let route = self.route_for_path(path);
        route.backend.write(rc, &route.backend_path, content).await
    }

    async fn delete(&self, rc: &RuntimeContext, path: &str) -> Result<(), String> {
        let route = self.route_for_path(path);
        route.backend.delete(rc, &route.backend_path).await
    }

    async fn move_path(&self, rc: &RuntimeContext, from: &str, to: &str) -> Result<(), String> {
        let src = self.route_for_path(from);
        let dst = self.route_for_path(to);
        if Arc::ptr_eq(&src.backend, &dst.backend) {
            return src.backend.move_path(rc, &src.backend_path, &dst.backend_path).await;
        }
        // 跨后端 move：read → write → delete。
        let content = src
            .backend
            .read(rc, &src.backend_path)
            .await?
            .ok_or_else(|| format!("Cannot read source for cross-backend move: {from}"))?;
        dst.backend.write(rc, &dst.backend_path, &content).await?;
        src.backend.delete(rc, &src.backend_path).await
    }
}

/// 内存文件系统：`BTreeMap<路径, 字节>`，测试与路由目标用。
pub struct InMemoryFilesystem {
    files: std::sync::Mutex<BTreeMap<String, Vec<u8>>>,
}

impl InMemoryFilesystem {
    pub fn new() -> Self {
        Self {
            files: std::sync::Mutex::new(BTreeMap::new()),
        }
    }

    /// 预置文件。
    pub fn with_files<I, S>(files: I) -> Self
    where
        I: IntoIterator<Item = (S, Vec<u8>)>,
        S: Into<String>,
    {
        Self {
            files: std::sync::Mutex::new(
                files
                    .into_iter()
                    .map(|(path, bytes)| (Self::normalize_key(&path.into()), bytes))
                    .collect(),
            ),
        }
    }

    fn normalize_key(path: &str) -> String {
        let key = strip_leading_slash(path);
        if key.is_empty() || key == "." {
            String::new()
        } else {
            key
        }
    }
}

impl Default for InMemoryFilesystem {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Filesystem for InMemoryFilesystem {
    async fn read(&self, _rc: &RuntimeContext, path: &str) -> Result<Option<String>, String> {
        let files = self.files.lock().map_err(|e| e.to_string())?;
        let key = Self::normalize_key(path);
        match files.get(&key) {
            Some(bytes) => Ok(Some(
                String::from_utf8(bytes.clone())
                    .map_err(|e| format!("not valid utf-8: {e}"))?,
            )),
            None => Ok(None),
        }
    }

    async fn exists(&self, _rc: &RuntimeContext, path: &str) -> Result<bool, String> {
        let files = self.files.lock().map_err(|e| e.to_string())?;
        Ok(files.contains_key(&Self::normalize_key(path)))
    }

    async fn ls(&self, _rc: &RuntimeContext, path: &str) -> Result<Vec<FileInfo>, String> {
        let files = self.files.lock().map_err(|e| e.to_string())?;
        let key = Self::normalize_key(path);
        if !key.is_empty() && !files.keys().any(|p| p == &key || p.starts_with(&format!("{key}/")))
        {
            return Err(format!("Path does not exist: {path}"));
        }
        let prefix = if key.is_empty() {
            String::new()
        } else {
            format!("{key}/")
        };
        let mut out: BTreeMap<String, FileInfo> = BTreeMap::new();
        for (file_path, bytes) in files.iter() {
            let Some(relative) = file_path.strip_prefix(&prefix) else {
                continue;
            };
            if relative.is_empty() {
                continue;
            }
            match relative.split_once('/') {
                Some((dir, _)) => {
                    out.entry(dir.to_string()).or_insert_with(|| FileInfo {
                        path: format!("{dir}/"),
                        size: None,
                        is_directory: true,
                        modified_at: None,
                    });
                }
                None => {
                    out.insert(
                        relative.to_string(),
                        FileInfo {
                            path: relative.to_string(),
                            size: Some(bytes.len() as u64),
                            is_directory: false,
                            modified_at: None,
                        },
                    );
                }
            }
        }
        Ok(out.into_values().collect())
    }

    async fn edit(
        &self,
        _rc: &RuntimeContext,
        path: &str,
        old_string: &str,
        new_string: &str,
        replace_all: bool,
    ) -> Result<usize, String> {
        let mut files = self.files.lock().map_err(|e| e.to_string())?;
        let key = Self::normalize_key(path);
        let bytes = files
            .get_mut(&key)
            .ok_or_else(|| format!("File not found: {path}"))?;
        let content = String::from_utf8(bytes.clone())
            .map_err(|e| format!("not valid utf-8: {e}"))?;
        let occurrences = content.matches(old_string).count();
        if occurrences == 0 {
            return Err(format!("String not found in file: {path}"));
        }
        if !replace_all && occurrences > 1 {
            return Err(format!(
                "Found {occurrences} occurrences of the string in {path}; \
                 add surrounding context to make it unique or set replace_all=true"
            ));
        }
        let updated = if replace_all {
            content.replace(old_string, new_string)
        } else {
            content.replacen(old_string, new_string, 1)
        };
        *bytes = updated.into_bytes();
        Ok(occurrences)
    }

    async fn grep(
        &self,
        _rc: &RuntimeContext,
        pattern: &str,
        path: Option<&str>,
        glob: Option<&str>,
    ) -> Result<Vec<GrepMatch>, String> {
        let files = self.files.lock().map_err(|e| e.to_string())?;
        let base = path.map(Self::normalize_key).unwrap_or_default();
        let mut out = Vec::new();
        for (file_path, bytes) in files.iter() {
            if !base.is_empty()
                && file_path != &base
                && !file_path.starts_with(&format!("{base}/"))
            {
                continue;
            }
            if let Some(glob_pattern) = glob {
                let name = file_path.rsplit('/').next().unwrap_or(file_path);
                if !crate::workspace::glob_match(glob_pattern, name) {
                    continue;
                }
            }
            let Ok(content) = std::str::from_utf8(bytes) else {
                continue;
            };
            for (index, line) in content.lines().enumerate() {
                if line.contains(pattern) {
                    out.push(GrepMatch {
                        path: file_path.clone(),
                        line: index + 1,
                        text: line.to_string(),
                    });
                }
            }
        }
        Ok(out)
    }

    async fn glob(
        &self,
        _rc: &RuntimeContext,
        pattern: &str,
        cwd: &str,
    ) -> Result<Vec<FileInfo>, String> {
        let files = self.files.lock().map_err(|e| e.to_string())?;
        let prefix = {
            let key = Self::normalize_key(cwd);
            if key.is_empty() || key == "." {
                String::new()
            } else {
                format!("{key}/")
            }
        };
        let mut out = Vec::new();
        for (file_path, bytes) in files.iter() {
            let Some(relative) = file_path.strip_prefix(&prefix) else {
                continue;
            };
            // 文件名匹配（与旧语义一致：`*` 枚举全部、`*.md` 按名过滤；
            // `**/SKILL.md` 这类前缀模式对裸文件名同样成立——`**/` 匹配零段）。
            let name = relative.rsplit('/').next().unwrap_or(relative);
            if !crate::workspace::glob_match(pattern, name) {
                continue;
            }
            out.push(FileInfo {
                path: relative.to_string(),
                size: Some(bytes.len() as u64),
                is_directory: false,
                modified_at: None,
            });
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    async fn upload_files(
        &self,
        _rc: &RuntimeContext,
        files: &[(String, Vec<u8>)],
    ) -> Result<(), String> {
        let mut store = self.files.lock().map_err(|e| e.to_string())?;
        for (path, bytes) in files {
            store.insert(Self::normalize_key(path), bytes.clone());
        }
        Ok(())
    }

    async fn download_files(
        &self,
        _rc: &RuntimeContext,
        paths: &[String],
    ) -> Result<Vec<DownloadedFile>, String> {
        let files = self.files.lock().map_err(|e| e.to_string())?;
        Ok(paths
            .iter()
            .map(|path| DownloadedFile {
                path: path.clone(),
                content: match files.get(&Self::normalize_key(path)) {
                    Some(bytes) => Ok(bytes.clone()),
                    None => Err(format!("File not found: {path}")),
                },
            })
            .collect())
    }

    async fn write(&self, rc: &RuntimeContext, path: &str, content: &str) -> Result<(), String> {
        self.upload_files(rc, &[(path.to_string(), content.as_bytes().to_vec())])
            .await
    }

    async fn delete(&self, _rc: &RuntimeContext, path: &str) -> Result<(), String> {
        let mut files = self.files.lock().map_err(|e| e.to_string())?;
        files.remove(&Self::normalize_key(path));
        Ok(())
    }

    async fn move_path(&self, _rc: &RuntimeContext, from: &str, to: &str) -> Result<(), String> {
        let mut files = self.files.lock().map_err(|e| e.to_string())?;
        let from_key = Self::normalize_key(from);
        let bytes = files
            .remove(&from_key)
            .ok_or_else(|| format!("Source not found: {from}"))?;
        files.insert(Self::normalize_key(to), bytes);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rc() -> RuntimeContext {
        RuntimeContext::new()
    }

    // ---------- InMemoryFilesystem ----------

    #[tokio::test]
    async fn in_memory_roundtrip() {
        let fs = InMemoryFilesystem::with_files([("docs/a.md", b"hello".to_vec())]);
        let rc = rc();
        assert_eq!(fs.read(&rc, "docs/a.md").await.unwrap().unwrap(), "hello");
        assert_eq!(fs.read(&rc, "/docs/a.md").await.unwrap().unwrap(), "hello");
        assert!(fs.exists(&rc, "docs/a.md").await.unwrap());
        assert!(!fs.exists(&rc, "docs/b.md").await.unwrap());

        fs.write(&rc, "docs/b.md", "world").await.unwrap();
        assert_eq!(fs.read(&rc, "docs/b.md").await.unwrap().unwrap(), "world");

        let entries = fs.ls(&rc, "docs").await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "a.md");

        let moved = fs.move_path(&rc, "docs/b.md", "docs/c.md").await;
        assert!(moved.is_ok());
        assert!(!fs.exists(&rc, "docs/b.md").await.unwrap());
        assert!(fs.exists(&rc, "docs/c.md").await.unwrap());

        fs.delete(&rc, "docs/c.md").await.unwrap();
        assert!(!fs.exists(&rc, "docs/c.md").await.unwrap());
    }

    #[tokio::test]
    async fn in_memory_read_lines_pagination() {
        let fs = InMemoryFilesystem::with_files([("f.txt", b"l1\nl2\nl3\nl4".to_vec())]);
        let rc = rc();
        let page = fs.read_lines(&rc, "f.txt", 1, 2).await.unwrap().unwrap();
        assert_eq!(page, "l2\nl3");
        let all = fs.read_lines(&rc, "f.txt", 0, 0).await.unwrap().unwrap();
        assert_eq!(all, "l1\nl2\nl3\nl4");
        let tail = fs.read_lines(&rc, "f.txt", 3, 10).await.unwrap().unwrap();
        assert_eq!(tail, "l4");
        let empty = fs.read_lines(&rc, "f.txt", 99, 1).await.unwrap().unwrap();
        assert_eq!(empty, "");
    }

    #[tokio::test]
    async fn in_memory_grep_and_glob() {
        let fs = InMemoryFilesystem::with_files([
            ("src/a.rs", b"fn main() {}\n// TODO fix".to_vec()),
            ("src/b.rs", b"let x = 1;".to_vec()),
            ("docs/c.md", b"TODO write docs".to_vec()),
        ]);
        let rc = rc();
        let matches = fs.grep(&rc, "TODO", None, None).await.unwrap();
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].path, "docs/c.md");
        assert_eq!(matches[0].line, 1);
        assert_eq!(matches[1].path, "src/a.rs");
        assert_eq!(matches[1].line, 2);

        let scoped = fs.grep(&rc, "TODO", Some("src"), Some("*.rs")).await.unwrap();
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].path, "src/a.rs");

        let globbed = fs.glob(&rc, "*.rs", "src").await.unwrap();
        assert_eq!(globbed.len(), 2);
    }

    #[tokio::test]
    async fn in_memory_edit_uniqueness() {
        let fs = InMemoryFilesystem::with_files([("f.txt", b"aa bb aa".to_vec())]);
        let rc = rc();
        let err = fs.edit(&rc, "f.txt", "aa", "cc", false).await.unwrap_err();
        assert!(err.contains("2 occurrences"), "got: {err}");
        let n = fs.edit(&rc, "f.txt", "aa", "cc", true).await.unwrap();
        assert_eq!(n, 2);
        assert_eq!(fs.read(&rc, "f.txt").await.unwrap().unwrap(), "cc bb cc");
        let missing = fs.edit(&rc, "f.txt", "zz", "yy", false).await.unwrap_err();
        assert!(missing.contains("not found"));
    }

    // ---------- OverlayFilesystem ----------

    #[tokio::test]
    async fn overlay_read_prefers_upper_and_merges_listing() {
        let lower: Arc<dyn Filesystem> = Arc::new(InMemoryFilesystem::with_files([
            ("shared.txt", b"lower version".to_vec()),
            ("only-lower.txt", b"x".to_vec()),
        ]));
        let upper: Arc<dyn Filesystem> = Arc::new(InMemoryFilesystem::with_files([
            ("shared.txt", b"upper version".to_vec()),
        ]));
        let overlay = OverlayFilesystem::new(Arc::clone(&upper), Arc::clone(&lower));
        let rc = rc();

        assert_eq!(
            overlay.read(&rc, "shared.txt").await.unwrap().unwrap(),
            "upper version"
        );
        assert_eq!(
            overlay.read(&rc, "only-lower.txt").await.unwrap().unwrap(),
            "x"
        );
        assert!(overlay.exists(&rc, "only-lower.txt").await.unwrap());

        let entries = overlay.ls(&rc, ".").await.unwrap();
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["only-lower.txt", "shared.txt"]);
    }

    #[tokio::test]
    async fn overlay_edit_copies_on_write() {
        let lower: Arc<dyn Filesystem> = Arc::new(InMemoryFilesystem::with_files([
            ("shared.txt", b"alpha beta".to_vec()),
        ]));
        let upper: Arc<dyn Filesystem> = Arc::new(InMemoryFilesystem::new());
        let overlay = OverlayFilesystem::new(Arc::clone(&upper), Arc::clone(&lower));
        let rc = rc();

        let n = overlay
            .edit(&rc, "shared.txt", "alpha", "gamma", false)
            .await
            .unwrap();
        assert_eq!(n, 1);
        // upper 现在持有副本，lower 原件不变。
        assert_eq!(
            upper.read(&rc, "shared.txt").await.unwrap().unwrap(),
            "gamma beta"
        );
        assert_eq!(
            lower.read(&rc, "shared.txt").await.unwrap().unwrap(),
            "alpha beta"
        );
    }

    #[tokio::test]
    async fn overlay_delete_protects_shared_layer() {
        let lower: Arc<dyn Filesystem> = Arc::new(InMemoryFilesystem::with_files([
            ("shared.txt", b"x".to_vec()),
        ]));
        let upper: Arc<dyn Filesystem> = Arc::new(InMemoryFilesystem::with_files([
            ("mine.txt", b"y".to_vec()),
        ]));
        let overlay = OverlayFilesystem::new(Arc::clone(&upper), Arc::clone(&lower));
        let rc = rc();

        let err = overlay.delete(&rc, "shared.txt").await.unwrap_err();
        assert!(err.contains("Cannot delete shared file"), "got: {err}");
        assert!(lower.exists(&rc, "shared.txt").await.unwrap());

        overlay.delete(&rc, "mine.txt").await.unwrap();
        assert!(!upper.exists(&rc, "mine.txt").await.unwrap());
    }

    #[tokio::test]
    async fn overlay_move_from_lower_copies_to_upper() {
        let lower: Arc<dyn Filesystem> = Arc::new(InMemoryFilesystem::with_files([
            ("shared.txt", b"content".to_vec()),
        ]));
        let upper: Arc<dyn Filesystem> = Arc::new(InMemoryFilesystem::new());
        let overlay = OverlayFilesystem::new(Arc::clone(&upper), Arc::clone(&lower));
        let rc = rc();

        overlay.move_path(&rc, "shared.txt", "moved.txt").await.unwrap();
        assert_eq!(
            upper.read(&rc, "moved.txt").await.unwrap().unwrap(),
            "content"
        );
        // lower 原件保留（copy-on-write move）。
        assert!(lower.exists(&rc, "shared.txt").await.unwrap());
    }

    #[tokio::test]
    async fn overlay_grep_merges_both_layers() {
        let lower: Arc<dyn Filesystem> = Arc::new(InMemoryFilesystem::with_files([
            ("a.txt", b"needle in lower".to_vec()),
        ]));
        let upper: Arc<dyn Filesystem> = Arc::new(InMemoryFilesystem::with_files([
            ("b.txt", b"needle in upper".to_vec()),
            ("a.txt", b"needle twice\nneedle again".to_vec()),
        ]));
        let overlay = OverlayFilesystem::new(Arc::clone(&upper), Arc::clone(&lower));
        let rc = rc();

        let matches = overlay.grep(&rc, "needle", None, None).await.unwrap();
        // a.txt 的 lower 命中（1:1）被 upper 的两行覆盖，b.txt 新增一条。
        assert_eq!(matches.len(), 3);
        assert_eq!(matches[0].path, "a.txt");
        assert_eq!(matches[0].line, 1);
        assert_eq!(matches[1].path, "a.txt");
        assert_eq!(matches[1].line, 2);
        assert_eq!(matches[2].path, "b.txt");
    }

    // ---------- CompositeFilesystem ----------

    #[tokio::test]
    async fn composite_routes_by_longest_prefix() {
        // 注意：最长前缀路由是排他的——`skills/` 子树整体归路由后端，
        // default 后端里的 skills/* 文件不会被读到（对齐 Java 语义）。
        let default_fs: Arc<dyn Filesystem> = Arc::new(InMemoryFilesystem::with_files([
            ("root.txt", b"default".to_vec()),
            ("docs/old.md", b"default doc".to_vec()),
        ]));
        let skills_fs: Arc<dyn Filesystem> = Arc::new(InMemoryFilesystem::with_files([
            ("code-review.md", b"routed skill".to_vec()),
        ]));
        let composite = CompositeFilesystem::new(
            Arc::clone(&default_fs),
            vec![("skills/".to_string(), Arc::clone(&skills_fs))],
        );
        let rc = rc();

        assert_eq!(
            composite.read(&rc, "skills/code-review.md").await.unwrap().unwrap(),
            "routed skill"
        );
        assert_eq!(
            composite.read(&rc, "/skills/code-review.md").await.unwrap().unwrap(),
            "routed skill"
        );
        assert_eq!(
            composite.read(&rc, "root.txt").await.unwrap().unwrap(),
            "default"
        );
        // 未被路由覆盖的路径落 default。
        assert_eq!(
            composite.read(&rc, "docs/old.md").await.unwrap().unwrap(),
            "default doc"
        );
        // 路由子树内 default 后端的内容不可达（排他路由）。
        assert_eq!(composite.read(&rc, "skills/missing.md").await.unwrap(), None);
    }

    #[tokio::test]
    async fn composite_ls_root_lists_routes() {
        let default_fs: Arc<dyn Filesystem> = Arc::new(InMemoryFilesystem::with_files([
            ("root.txt", b"default".to_vec()),
        ]));
        let skills_fs: Arc<dyn Filesystem> = Arc::new(InMemoryFilesystem::new());
        let composite = CompositeFilesystem::new(
            Arc::clone(&default_fs),
            vec![
                ("skills/".to_string(), Arc::clone(&skills_fs)),
                ("config.json".to_string(), Arc::clone(&skills_fs)),
            ],
        );
        let rc = rc();

        skills_fs.write(&rc, "config.json", "{}").await.unwrap();
        let entries = composite.ls(&rc, "/").await.unwrap();
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"root.txt"), "got: {paths:?}");
        assert!(paths.contains(&"skills"), "got: {paths:?}");
        assert!(paths.contains(&"config.json"), "got: {paths:?}");
        let skills_entry = entries.iter().find(|e| e.path == "skills").unwrap();
        assert!(skills_entry.is_directory);
    }

    #[tokio::test]
    async fn composite_write_remaps_and_routes() {
        let default_fs: Arc<dyn Filesystem> = Arc::new(InMemoryFilesystem::new());
        let skills_fs: Arc<dyn Filesystem> = Arc::new(InMemoryFilesystem::new());
        let composite = CompositeFilesystem::new(
            Arc::clone(&default_fs),
            vec![("skills/".to_string(), Arc::clone(&skills_fs))],
        );
        let rc = rc();

        composite.write(&rc, "skills/new.md", "routed").await.unwrap();
        assert_eq!(
            skills_fs.read(&rc, "/new.md").await.unwrap().unwrap(),
            "routed"
        );
        assert!(!default_fs.exists(&rc, "skills/new.md").await.unwrap());

        let entries = composite.ls(&rc, "skills").await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "skills/new.md");
    }

    #[tokio::test]
    async fn composite_cross_backend_move() {
        let default_fs: Arc<dyn Filesystem> = Arc::new(InMemoryFilesystem::new());
        let skills_fs: Arc<dyn Filesystem> = Arc::new(InMemoryFilesystem::with_files([
            ("a.md", b"payload".to_vec()),
        ]));
        let composite = CompositeFilesystem::new(
            Arc::clone(&default_fs),
            vec![("skills/".to_string(), Arc::clone(&skills_fs))],
        );
        let rc = rc();

        composite
            .move_path(&rc, "skills/a.md", "archive/a.md")
            .await
            .unwrap();
        assert!(!skills_fs.exists(&rc, "/a.md").await.unwrap());
        assert_eq!(
            default_fs.read(&rc, "archive/a.md").await.unwrap().unwrap(),
            "payload"
        );
    }

    #[tokio::test]
    async fn composite_edit_and_grep_scope() {
        let default_fs: Arc<dyn Filesystem> = Arc::new(InMemoryFilesystem::with_files([
            ("notes.txt", b"TODO default".to_vec()),
        ]));
        let skills_fs: Arc<dyn Filesystem> = Arc::new(InMemoryFilesystem::with_files([
            ("review.md", b"TODO routed\ndone".to_vec()),
        ]));
        let composite = CompositeFilesystem::new(
            Arc::clone(&default_fs),
            vec![("skills/".to_string(), Arc::clone(&skills_fs))],
        );
        let rc = rc();

        let n = composite
            .edit(&rc, "skills/review.md", "TODO routed", "routed ok", false)
            .await
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            skills_fs.read(&rc, "/review.md").await.unwrap().unwrap(),
            "routed ok\ndone"
        );

        let matches = composite.grep(&rc, "TODO", None, None).await.unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, "notes.txt");

        let scoped = composite.grep(&rc, "routed", Some("skills"), None).await.unwrap();
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].path, "skills/review.md");
    }
}
