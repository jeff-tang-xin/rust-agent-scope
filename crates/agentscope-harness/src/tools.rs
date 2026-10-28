//! Harness 平台工具（对应 Java `agent.tool` 包的 ShellExecute / Web / Task /
//! WaitAsyncResults / PlanMode / SessionSearch / ArtifactDelivery / AgentGenerate
//! 工具类；AgentSpawn / Team / Skill* 随后续批次补齐）。
//!
//! 与 Java 的形态差异：Java 一个工具类可含多个 `@Tool` 方法；Rust [`Tool`]
//! trait 一个实现对应一个工具名，故每工具一个结构体 + `*ToolSet` 聚合装配。
//! Java 工具方法注入的 `RuntimeContext` 由 [`ToolContext::session_id`] /
//! [`ToolContext::state`] 承载（`call_with_context` 入口）。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentscope_core::message::{ToolExecutionDetails, ToolExecutionOutcome, ToolResultState};
use agentscope_core::permission::PermissionDecision;
use agentscope_core::session::{transcript, SessionKey, SessionLogStore};
use agentscope_core::tool::{Tool, ToolContext, ToolOutput};
use agentscope_core::RuntimeContext;
use serde_json::{json, Value};

use crate::plan::PlanModeManager;
use crate::subagent::DefaultAgentManager;
use crate::task::{TaskRepository, TaskStatus};
use crate::workspace::WorkspaceManager;

// ---------- 共享参数辅助（对齐 tools_filesystem / tools_memory 的形态） ----------

fn string_param(input: &Value, key: &str) -> Result<String, ToolOutput> {
    match input.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(Value::Null) | None => Err(ToolOutput::error(format!("{key} is required"))),
        Some(other) => Ok(other.to_string()),
    }
}

fn optional_string(input: &Value, key: &str) -> Option<String> {
    match input.get(key) {
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.clone()),
        _ => None,
    }
}

fn optional_bool(input: &Value, key: &str) -> Option<bool> {
    input.get(key).and_then(Value::as_bool)
}

fn optional_i64(input: &Value, key: &str) -> Option<i64> {
    input.get(key).and_then(Value::as_i64)
}

// ===========================================================================
// Shell 执行（对应 Java ShellExecuteTool + AbstractSandboxFilesystem.execute）
// ===========================================================================

/// 沙箱执行响应（对应 Java `ExecuteResponse`）。
#[derive(Debug, Clone)]
pub struct ExecuteResponse {
    /// 合并的 stdout + stderr 文本。
    pub output: String,
    /// 退出码；`None` = 未能确定（超时被杀、启动失败等）。
    pub exit_code: Option<i32>,
    /// 输出是否被后端截断。
    pub truncated: bool,
}

impl ExecuteResponse {
    /// `exit_code == Some(0)` 即成功（对齐 Java `isSuccess`）。
    pub fn is_success(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// Shell 执行后端（对应 Java `AbstractSandboxFilesystem.execute` 的窄接口）。
#[async_trait::async_trait]
pub trait ShellBackend: Send + Sync {
    /// 执行命令，`timeout_secs` 到点终止进程并返回 `exit_code: None`。
    async fn execute(&self, command: &str, timeout_secs: u64) -> ExecuteResponse;
}

/// 本地进程执行后端（Rust 侧补充实现；Java 侧由各 sandbox 提供真实后端）。
///
/// Windows 走 `cmd /C`，其余走 `sh -c`；输出超过 [`LocalShellBackend::MAX_OUTPUT_CHARS`]
/// 截断并置 `truncated`。
pub struct LocalShellBackend {
    max_output_chars: usize,
    /// 执行锚定目录（沙箱根）：设置后每次执行 `current_dir` 到该目录，
    /// `working_directory` 相对路径在此之下解析（对齐 Java 沙箱 execute 语义）。
    root: Option<std::path::PathBuf>,
}

impl LocalShellBackend {
    /// 本地后端的单次输出上限（字符）。
    pub const MAX_OUTPUT_CHARS: usize = 30_000;

    /// 默认上限的后端（无锚定，继承进程 cwd；仅测试/工具内部使用）。
    pub fn new() -> Self {
        Self {
            max_output_chars: Self::MAX_OUTPUT_CHARS,
            root: None,
        }
    }

    /// 自定义输出上限。
    pub fn with_max_output_chars(max_output_chars: usize) -> Self {
        Self {
            max_output_chars,
            root: None,
        }
    }

    /// 锚定到指定根目录（生产装配用）：所有命令在该目录下执行，
    /// 工具层已拒绝绝对路径 / `~` / `..` 的 working_directory，二者共同构成沙箱边界。
    pub fn new_anchored(root: impl Into<std::path::PathBuf>) -> Self {
        Self {
            max_output_chars: Self::MAX_OUTPUT_CHARS,
            root: Some(root.into()),
        }
    }
}

impl Default for LocalShellBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// 解码子进程输出：UTF-8 严格解码优先（git/现代工具链输出 UTF-8）；
/// 失败时 Windows 按 GBK 兜底（中文系统 cmd/PowerShell 5.x 输出 ANSI/GBK），
/// 其余平台 lossy UTF-8。stdout 与 stderr 分开解码，避免混合编码互相污染。
fn decode_process_output(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) if cfg!(windows) => {
            let (cow, _, _) = encoding_rs::GBK.decode(bytes);
            cow.into_owned()
        }
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

#[async_trait::async_trait]
impl ShellBackend for LocalShellBackend {
    async fn execute(&self, command: &str, timeout_secs: u64) -> ExecuteResponse {
        use tokio::io::AsyncReadExt;
        use tokio::process::Command;

        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.arg("/C").arg(command);
            // CREATE_NO_WINDOW：桌面应用里执行 shell 不弹 cmd 黑窗（每次
            // 工具调用闪一个窗口，体验极差）。
            #[cfg(windows)]
            {
                c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
            }
            c
        } else {
            let mut c = Command::new("sh");
            c.arg("-c").arg(command);
            c
        };
        cmd.kill_on_drop(true)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(root) = &self.root {
            cmd.current_dir(root);
        }

        // spawn + 独立任务收集管道：超时被杀时已产出的输出仍可回收
        // （`timeout(cmd.output())` 会在掐断时连同已读输出一起丢弃）。
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                return ExecuteResponse {
                    output: format!("命令启动失败：{e}"),
                    exit_code: None,
                    truncated: false,
                };
            }
        };
        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();
        let stdout_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            if let Some(mut pipe) = stdout_pipe {
                let _ = pipe.read_to_end(&mut buf).await;
            }
            buf
        });
        let stderr_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            if let Some(mut pipe) = stderr_pipe {
                let _ = pipe.read_to_end(&mut buf).await;
            }
            buf
        });

        let status = tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait()).await;
        let timed_out = status.is_err();
        if timed_out {
            // wait 的 future 被 drop 后 kill_on_drop 兜底终止；显式 kill 加速管道 EOF。
            let _ = child.kill().await;
        }
        // 进程终止（或已死）后管道 EOF，收集任务必然结束。
        let stdout_bytes = stdout_task.await.unwrap_or_default();
        let stderr_bytes = stderr_task.await.unwrap_or_default();

        let mut text = decode_process_output(&stdout_bytes);
        text.push_str(&decode_process_output(&stderr_bytes));
        if timed_out {
            text.push_str(&format!(
                "\n[命令执行超时（{timeout_secs}s），进程已终止；以上为终止前已产出的部分输出。长命令可用 timeout 参数调大上限后重试。]"
            ));
        }
        let mut truncated = false;
        if text.chars().count() > self.max_output_chars {
            text = text.chars().take(self.max_output_chars).collect();
            truncated = true;
        }
        ExecuteResponse {
            output: text,
            exit_code: if timed_out {
                None
            } else {
                match status {
                    Ok(Ok(s)) => s.code(),
                    _ => None,
                }
            },
            truncated,
        }
    }
}

/// `execute` 工具（对应 Java `ShellExecuteTool`，工具名 `execute`）。
pub struct ShellExecuteTool {
    backend: Arc<dyn ShellBackend>,
}

impl ShellExecuteTool {
    /// 绑定执行后端。
    pub fn new(backend: Arc<dyn ShellBackend>) -> Self {
        Self { backend }
    }

    /// 工具名。
    pub const NAME: &'static str = "execute";

    /// working_directory 拼接（对齐 Java `commandWithWorkingDirectory`）。
    pub fn command_with_working_directory(
        working_directory: &str,
        command: &str,
        windows: bool,
    ) -> String {
        if windows {
            format!(
                "cd /d \"{}\" && {}",
                working_directory.replace('"', "\"\""),
                command
            )
        } else {
            format!("cd '{}' && {}", working_directory.replace('\'', "'\\''"), command)
        }
    }
}

#[async_trait::async_trait]
impl Tool for ShellExecuteTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Execute a shell command. Use for git, npm, build, test, and other terminal operations. \
Returns combined output and exit code. On timeout the process is killed and the partial output \
produced so far is returned with a timeout notice — retry with a larger `timeout` for long-running \
commands (default 30s). If a dedicated tool exists (e.g., read_file, write_file), \
you MUST use it instead of shell commands."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to execute"
                },
                "working_directory": {
                    "type": "string",
                    "description": "Working directory (relative to workspace root, optional)"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in seconds (default: 30)"
                }
            },
            "required": ["command"]
        })
    }

    async fn call(&self, input: Value) -> agentscope_core::Result<ToolOutput> {
        let command = match string_param(&input, "command") {
            Ok(c) => c,
            Err(output) => return Ok(output),
        };
        let mut effective = command;
        if let Some(wd) = optional_string(&input, "working_directory") {
            let wd = wd.trim().to_string();
            let windows_drive = wd.len() >= 2 && wd.as_bytes()[1] == b':';
            if wd.starts_with('/')
                || wd.starts_with('\\')
                || wd.starts_with('~')
                || windows_drive
                || wd.contains("..")
            {
                return Ok(ToolOutput::error(
                    "working_directory must be a relative path within the workspace \
(absolute paths, '~', and '..' are not allowed).",
                ));
            }
            effective = Self::command_with_working_directory(&wd, &effective, cfg!(windows));
        }
        let timeout_secs = optional_i64(&input, "timeout")
            .filter(|t| *t > 0)
            .unwrap_or(30) as u64;

        let result = self.backend.execute(&effective, timeout_secs).await;
        let mut sb = String::new();
        sb.push_str(&format!("Exit code: {}", result.exit_code.map_or("null".into(), |c| c.to_string())));
        sb.push('\n');
        if !result.output.trim().is_empty() {
            sb.push('\n');
            sb.push_str(&result.output);
        }
        if result.truncated {
            sb.push_str("\n(output was truncated)");
        }
        let outcome = if result.exit_code.is_none() {
            ToolExecutionOutcome::Unknown
        } else if result.is_success() {
            ToolExecutionOutcome::Succeeded
        } else {
            ToolExecutionOutcome::Failed
        };
        let details = ToolExecutionDetails {
            kind: "shell".into(),
            outcome,
            exit_code: result.exit_code,
            output_truncated: result.truncated,
        };
        let state = if result.is_success() {
            ToolResultState::Success
        } else {
            ToolResultState::Error
        };
        Ok(ToolOutput {
            content: sb,
            state,
            execution_details: Some(details),
        })
    }
}

// ===========================================================================
// Web 工具（对应 Java WebTools.WebFetchTool / WebSearchTool）
// ===========================================================================

/// `web_fetch` 工具：抓取 HTTP(S) URL 并返回截断文本预览。
pub struct WebFetchTool {
    client: reqwest::Client,
}

impl WebFetchTool {
    /// 工具名。
    pub const NAME: &'static str = "web_fetch";
    /// 默认返回字符上限。
    pub const DEFAULT_MAX_CHARS: usize = 20_000;
    /// 返回字符硬上限。
    pub const MAX_CHARS_CEILING: usize = 100_000;

    /// 默认客户端（20s 连接超时）。
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(20))
                .build()
                .unwrap_or_default(),
        }
    }

    /// 注入自定义客户端（代理 / SSL / HTTP 版本定制）。
    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Fetch content from an HTTP(S) URL and return a truncated text preview."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "HTTP or HTTPS URL to fetch"
                },
                "max_chars": {
                    "type": "integer",
                    "description": "Max characters to return (default 20000)"
                }
            },
            "required": ["url"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn call(&self, input: Value) -> agentscope_core::Result<ToolOutput> {
        let Some(url) = optional_string(&input, "url") else {
            return Ok(ToolOutput::error("url is required"));
        };
        let trimmed = url.trim().to_string();
        if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
            return Ok(ToolOutput::error("only http/https URLs are allowed"));
        }
        let limit = optional_i64(&input, "max_chars")
            .filter(|v| *v > 0)
            .map_or(Self::DEFAULT_MAX_CHARS, |v| {
                (v as usize).min(Self::MAX_CHARS_CEILING)
            });

        let response = match self
            .client
            .get(&trimmed)
            .timeout(Duration::from_secs(30))
            .header("User-Agent", "AgentScope-Harness-WebFetch/1.0")
            .send()
            .await
        {
            Ok(response) => response,
            Err(e) => return Ok(ToolOutput::error(format!("web_fetch failed: {e}"))),
        };
        let status = response.status().as_u16();
        let mut body = response.text().await.unwrap_or_default();
        if body.chars().count() > limit {
            body = body.chars().take(limit).collect();
            body.push_str("\n...[truncated]");
        }
        let payload = format!("status={status}\n\n{body}");
        if !(200..300).contains(&status) {
            return Ok(ToolOutput {
                content: payload,
                state: ToolResultState::Error,
                execution_details: None,
            });
        }
        Ok(ToolOutput::ok(payload))
    }
}

/// `web_search` 工具：Tavily API 检索（需 `TAVILY_API_KEY`）。
pub struct WebSearchTool {
    client: reqwest::Client,
}

impl WebSearchTool {
    /// 工具名。
    pub const NAME: &'static str = "web_search";
    /// Tavily 检索端点。
    pub const TAVILY_API: &'static str = "https://api.tavily.com/search";
    /// 默认结果数。
    pub const DEFAULT_MAX_RESULTS: i64 = 5;
    /// 结果数硬上限。
    pub const MAX_RESULTS_CEILING: i64 = 10;

    /// 默认客户端。
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(20))
                .build()
                .unwrap_or_default(),
        }
    }

    /// 注入自定义客户端。
    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Search the web for information. Requires TAVILY_API_KEY. Returns titles, URLs and snippets."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query"
                },
                "max_results": {
                    "type": "integer",
                    "description": "Maximum results (default 5)"
                }
            },
            "required": ["query"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn call(&self, input: Value) -> agentscope_core::Result<ToolOutput> {
        let Some(query) = optional_string(&input, "query") else {
            return Ok(ToolOutput::error("query is required"));
        };
        let Some(api_key) = std::env::var("TAVILY_API_KEY")
            .ok()
            .filter(|k| !k.trim().is_empty())
        else {
            return Ok(ToolOutput::error(
                "TAVILY_API_KEY is not set. Configure the key via Environment vault \
credentials or process env to enable web_search.",
            ));
        };
        let limit = optional_i64(&input, "max_results")
            .filter(|v| *v > 0)
            .map_or(Self::DEFAULT_MAX_RESULTS, |v| {
                v.min(Self::MAX_RESULTS_CEILING)
            });

        let response = match self
            .client
            .post(Self::TAVILY_API)
            .timeout(Duration::from_secs(30))
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {api_key}"))
            .json(&json!({
                "query": query,
                "max_results": limit,
                "search_depth": "basic"
            }))
            .send()
            .await
        {
            Ok(response) => response,
            Err(e) => return Ok(ToolOutput::error(format!("web_search failed: {e}"))),
        };
        let status = response.status().as_u16();
        if status != 200 {
            return Ok(ToolOutput::error(format!("Tavily API returned {status}")));
        }
        let root: Value = match response.json().await {
            Ok(root) => root,
            Err(e) => return Ok(ToolOutput::error(format!("web_search failed: {e}"))),
        };
        let Some(results) = root.get("results").and_then(Value::as_array) else {
            return Ok(ToolOutput::ok("No results."));
        };
        if results.is_empty() {
            return Ok(ToolOutput::ok("No results."));
        }
        let mut sb = String::new();
        for (i, r) in results.iter().enumerate() {
            sb.push_str(&format!(
                "{}. {}\n   {}\n   {}\n\n",
                i + 1,
                r.get("title").and_then(Value::as_str).unwrap_or(""),
                r.get("url").and_then(Value::as_str).unwrap_or(""),
                r.get("content").and_then(Value::as_str).unwrap_or(""),
            ));
        }
        Ok(ToolOutput::ok(sb.trim().to_string()))
    }
}

// ===========================================================================
// 后台任务工具（对应 Java TaskTool：task_output / task_cancel / task_list）
// ===========================================================================

/// epoch 毫秒 → `yyyy-MM-dd'T'HH:mm:ss'Z'`（对齐 Java TaskTool 的 ISO_FORMATTER）。
fn iso_from_millis(millis: i64) -> String {
    crate::memory::millis_to_iso8601(millis)
}

fn task_status_from_filter(filter: Option<&str>) -> Option<TaskStatus> {
    let filter = filter?.trim();
    if filter.is_empty() || filter.eq_ignore_ascii_case("all") {
        return None;
    }
    // 非法值按 Java 语义静默忽略（返回 None = 不过滤）。
    TaskStatus::as_str_all()
        .iter()
        .find(|(name, _)| filter.eq_ignore_ascii_case(name))
        .map(|(_, status)| *status)
}

fn task_result_state(status: TaskStatus) -> ToolResultState {
    match status {
        TaskStatus::Failed => ToolResultState::Error,
        TaskStatus::Cancelled => ToolResultState::Interrupted,
        // 未终态（PENDING/RUNNING）按 RUNNING 语义回传。
        TaskStatus::Pending | TaskStatus::Running => ToolResultState::Running,
        TaskStatus::Completed => ToolResultState::Success,
    }
}

/// `task_output` 工具：取任务结果（阻塞或非阻塞）。
pub struct TaskOutputTool {
    repository: Arc<dyn TaskRepository>,
    /// 阻塞轮询间隔（毫秒；测试注入小值）。
    poll_interval_ms: u64,
}

impl TaskOutputTool {
    /// 工具名。
    pub const NAME: &'static str = "task_output";
    /// 默认阻塞超时（毫秒）。
    pub const DEFAULT_TIMEOUT_MS: i64 = 30_000;
    /// 阻塞超时硬上限（毫秒）。
    pub const MAX_TIMEOUT_MS: i64 = 600_000;

    /// 绑定任务仓库（生产轮询间隔 100ms）。
    pub fn new(repository: Arc<dyn TaskRepository>) -> Self {
        Self {
            repository,
            poll_interval_ms: 100,
        }
    }

    /// 自定义轮询间隔（测试用）。
    pub fn with_poll_interval(repository: Arc<dyn TaskRepository>, poll_interval_ms: u64) -> Self {
        Self {
            repository,
            poll_interval_ms,
        }
    }
}

#[async_trait::async_trait]
impl Tool for TaskOutputTool {

    /// 只读工具：权限模式检查（EXPLORE/ACCEPT_EDITS）与 studio 免确认白名单依据。
    fn is_read_only(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Retrieve the output of a background task by task_id. Prefer block=false to check status \
without waiting. Use block=true only for one specific task you are ready to block on. For several \
async subagent tasks, prefer wait_async_results(task_ids=...) or wait_async_results(wait_all=true) \
when you need a barrier. A returned task ID is immediately queryable. not_found is a \
tracking/scope error, never evidence that work is running."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "The task_id returned by agent_spawn or agent_send when timeout_seconds was 0"
                },
                "block": {
                    "type": "boolean",
                    "description": "Whether to wait for completion (default: true). Prefer false for status checks to avoid blocking."
                },
                "timeout": {
                    "type": "integer",
                    "description": "Max wait time in milliseconds (default: 30000, max: 600000)"
                }
            },
            "required": ["task_id"]
        })
    }

    async fn call_with_context(&self, input: Value, ctx: &ToolContext) -> agentscope_core::Result<ToolOutput> {
        let Some(task_id) = optional_string(&input, "task_id") else {
            return Ok(ToolOutput::error("status: invalid_request\ntask_id is required"));
        };
        let session_id = ctx.session_id.as_str();
        let rc = RuntimeContext::new();

        let Some(mut task) = self.repository.get_task(&rc, session_id, &task_id)
        else {
            return Ok(ToolOutput::error(format!(
                "status: not_found\ntask_id: {task_id}\n\
No task with this ID exists in the current session. This is not running; use task_list to \
reconcile the ID and report a tracking error if missing."
            )));
        };
        task.update_last_checked_at();

        let should_block = optional_bool(&input, "block").unwrap_or(true);
        let timeout_ms = optional_i64(&input, "timeout")
            .map_or(Self::DEFAULT_TIMEOUT_MS, |v| v.clamp(0, Self::MAX_TIMEOUT_MS));

        if should_block && !task.status().is_terminal() {
            let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms as u64);
            loop {
                // 经仓库重取：触发句柄结算（settle），状态才会推进。
                let refreshed = self.repository.get_task(&rc, session_id, &task_id);
                if refreshed.as_ref().is_some_and(|t| t.status().is_terminal()) {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    return Ok(ToolOutput::ok(format!(
                        "task_id: {task_id}\nstatus: running\nnote: Task is running (possibly on \
another node). Use task_output(block=false) to poll for completion."
                    )));
                }
                tokio::time::sleep(Duration::from_millis(self.poll_interval_ms)).await;
            }
            // settle 后重取终态任务，渲染最新结果。
            if let Some(refreshed) = self.repository.get_task(&rc, session_id, &task_id) {
                task = refreshed;
            }
        }
        let status = task.status();
        if status.is_terminal() {
            // 取到终态结果即标记已投递（幂等；失败只风险重复推送）。
            self.repository
                .mark_delivered(&RuntimeContext::new(), session_id, &task_id);
        }
        Ok(ToolOutput {
            content: format_task_detail(&task),
            state: task_result_state(status),
            execution_details: None,
        })
    }

    async fn call(&self, input: Value) -> agentscope_core::Result<ToolOutput> {
        let ctx = ToolContext::new(Self::NAME, Self::NAME);
        self.call_with_context(input, &ctx).await
    }
}

/// `task_cancel` 工具：取消运行中的后台任务。
pub struct TaskCancelTool {
    repository: Arc<dyn TaskRepository>,
}

impl TaskCancelTool {
    /// 工具名。
    pub const NAME: &'static str = "task_cancel";

    /// 绑定任务仓库。
    pub fn new(repository: Arc<dyn TaskRepository>) -> Self {
        Self { repository }
    }
}

#[async_trait::async_trait]
impl Tool for TaskCancelTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    /// 取消后台任务可重跑、非破坏性：免确认白名单依据（对齐 Java
    /// ALWAYS_ALLOWED 的 task_output/task_list 同族开发辅助语义）。
    fn is_read_only(&self) -> bool {
        true
    }

    fn description(&self) -> &str {
        "Cancel a running background task. Use to stop a task that is no longer needed. \
Has no effect on already-completed tasks."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "The task_id to cancel"
                }
            },
            "required": ["task_id"]
        })
    }

    async fn call_with_context(&self, input: Value, ctx: &ToolContext) -> agentscope_core::Result<ToolOutput> {
        let Some(task_id) = optional_string(&input, "task_id") else {
            return Ok(ToolOutput::error("status: invalid_request\ntask_id is required"));
        };
        let session_id = ctx.session_id.as_str();
        let Some(task) = self.repository.get_task(&RuntimeContext::new(), session_id, &task_id)
        else {
            return Ok(ToolOutput::error(format!(
                "status: not_found\ntask_id: {task_id}"
            )));
        };
        let status = task.status();
        if status.is_terminal() {
            return Ok(ToolOutput::ok(format!(
                "task_id: {task_id}\nstatus: {}\nnote: Task already in terminal state, cannot cancel.",
                status_wire_lower(status)
            )));
        }
        self.repository
            .cancel_task(&RuntimeContext::new(), session_id, &task_id);
        Ok(ToolOutput::ok(format!(
            "task_id: {task_id}\nstatus: cancelled\nCancellation requested successfully."
        )))
    }

    async fn call(&self, input: Value) -> agentscope_core::Result<ToolOutput> {
        let ctx = ToolContext::new(Self::NAME, Self::NAME);
        self.call_with_context(input, &ctx).await
    }
}

/// `task_list` 工具：列出当前会话的后台任务。
pub struct TaskListTool {
    repository: Arc<dyn TaskRepository>,
}

impl TaskListTool {
    /// 工具名。
    pub const NAME: &'static str = "task_list";

    /// 绑定任务仓库。
    pub fn new(repository: Arc<dyn TaskRepository>) -> Self {
        Self { repository }
    }
}

#[async_trait::async_trait]
impl Tool for TaskListTool {

    /// 只读工具：权限模式检查（EXPLORE/ACCEPT_EDITS）与 studio 免确认白名单依据。
    fn is_read_only(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "List all background tasks for the current session with their current statuses. Reads \
from durable workspace storage — always accurate even after conversation compaction or node \
migration. Optionally filter by status (running, completed, failed, cancelled). Use this to \
recover task IDs and state after compaction."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "status_filter": {
                    "type": "string",
                    "description": "Filter by status: running, completed, failed, cancelled, or omit for all tasks"
                }
            },
            "required": []
        })
    }

    async fn call_with_context(&self, input: Value, ctx: &ToolContext) -> agentscope_core::Result<ToolOutput> {
        let filter = task_status_from_filter(optional_string(&input, "status_filter").as_deref());
        let session_id = ctx.session_id.as_str();
        let tasks = self
            .repository
            .list_tasks(&RuntimeContext::new(), session_id, filter);
        if tasks.is_empty() {
            let filter_desc = filter
                .map(|f| format!(" with status '{}'", status_wire_lower(f)))
                .unwrap_or_default();
            return Ok(ToolOutput::ok(format!(
                "No background tasks tracked{filter_desc}."
            )));
        }
        let mut sb = String::new();
        sb.push_str(&format!("{} tracked task(s):\n", tasks.len()));
        for task in &tasks {
            let record = task.record.lock().unwrap_or_else(|e| e.into_inner());
            sb.push_str(&format!("- task_id: {}", record.task_id));
            sb.push_str(&format!("  agent: {}", record.sub_agent_id));
            sb.push_str(&format!("  status: {}", status_wire_lower(record.status)));
            sb.push_str(&format!("  created: {}", iso_from_millis(record.created_at)));
            sb.push('\n');
        }
        Ok(ToolOutput::ok(sb.trim().to_string()))
    }

    async fn call(&self, input: Value) -> agentscope_core::Result<ToolOutput> {
        let ctx = ToolContext::new(Self::NAME, Self::NAME);
        self.call_with_context(input, &ctx).await
    }
}

fn status_wire_lower(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "pending",
        TaskStatus::Running => "running",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
        TaskStatus::Cancelled => "cancelled",
    }
}

/// 任务详情渲染（对齐 Java `formatTaskDetail`）。
///
/// 注意：先在锁外取 `status_text()`（其内部会锁 record），再锁 record 取字段——
/// 持锁期间调用会再拿同一把锁的方法 = 自死锁。
fn format_task_detail(task: &Arc<crate::task::BackgroundTask>) -> String {
    let status_text = task.status_text();
    let record = task.record.lock().unwrap_or_else(|e| e.into_inner());
    let mut sb = String::new();
    sb.push_str(&format!("task_id: {}\n", record.task_id));
    sb.push_str(&format!("agent_id: {}\n", record.sub_agent_id));
    sb.push_str(&format!("status: {status_text}\n"));
    sb.push_str(&format!("created_at: {}\n", iso_from_millis(record.created_at)));
    if record.status.is_terminal() {
        if let Some(result) = &record.result {
            sb.push_str(&format!("\nResult:\n{result}"));
        } else if let Some(error) = &record.error_message {
            sb.push_str(&format!("\nError:\n{error}"));
        } else {
            sb.push_str("\nTask completed with no result.");
        }
    } else {
        sb.push_str("\nTask still running...");
    }
    sb
}

// ===========================================================================
// 等待异步结果（对应 Java WaitAsyncResultsTool）
// ===========================================================================

/// 收件箱探测（对应 Java 注入的 `SessionInbox` 探针；Rust 侧窄接口）。
pub trait InboxProbe: Send + Sync {
    /// 指定会话的收件箱是否有未读消息。
    fn inbox_has_messages(&self, session_id: &str) -> bool;
}

/// `wait_async_results` 工具：等待后台任务完成或收件箱消息到达。
///
/// 语义（对齐 Java）：`task_ids` 给定 = 屏障模式（等全部终态）；未给定 =
/// inbox-any 模式（任一待投递任务或收件箱消息即返回）。轮询到超时预算耗尽，
/// sleep 截断到剩余预算。
pub struct WaitAsyncResultsTool {
    repository: Arc<dyn TaskRepository>,
    inbox: Option<Arc<dyn InboxProbe>>,
    /// 轮询间隔（毫秒；测试注入小值）。
    poll_interval_ms: u64,
}

impl WaitAsyncResultsTool {
    /// 工具名。
    pub const NAME: &'static str = "wait_async_results";
    /// 默认等待预算（秒）。
    pub const DEFAULT_TIMEOUT_SECS: i64 = 60;
    /// 等待预算硬上限（秒）。
    pub const MAX_TIMEOUT_SECS: i64 = 120;

    /// 绑定任务仓库与可选收件箱探针（生产轮询间隔 200ms）。
    pub fn new(repository: Arc<dyn TaskRepository>, inbox: Option<Arc<dyn InboxProbe>>) -> Self {
        Self {
            repository,
            inbox,
            poll_interval_ms: 200,
        }
    }

    /// 自定义轮询间隔（测试用）。
    pub fn with_poll_interval(
        repository: Arc<dyn TaskRepository>,
        inbox: Option<Arc<dyn InboxProbe>>,
        poll_interval_ms: u64,
    ) -> Self {
        Self {
            repository,
            inbox,
            poll_interval_ms,
        }
    }
}

#[async_trait::async_trait]
impl Tool for WaitAsyncResultsTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    /// 纯等待聚合，无任何副作用：免确认白名单依据。
    fn is_read_only(&self) -> bool {
        true
    }

    fn description(&self) -> &str {
        "Wait for background async tool, subagent, or teammate results. Prefer barrier mode: \
task_ids waits until those specific tasks are all terminal and returns their results in this \
tool output; wait_all=true waits for the snapshot of currently running tasks. Without task_ids \
and without wait_all, this is legacy inbox-any mode: returns when ANY inbox message arrives — \
not wait-all. For must-collect-all groups use task_ids or wait_all=true. Max timeout is 120 \
seconds. If you have already waited without results, use task_list or task_output(block=false) \
to check status instead of waiting again."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_ids": {
                    "type": "string",
                    "description": "Optional comma-separated task IDs to wait for. When set, the tool returns only after all listed tasks are terminal (or timeout), and embeds each task's result in the response."
                },
                "timeout_seconds": {
                    "type": "integer",
                    "description": "Maximum seconds to wait. Default 60, max 120. Values above 120 are clamped."
                },
                "wait_all": {
                    "type": "boolean",
                    "description": "Optional barrier mode. When true and task_ids is empty, wait for the snapshot of currently non-terminal tasks in this session and embed their results."
                }
            },
            "required": []
        })
    }

    async fn call_with_context(&self, input: Value, ctx: &ToolContext) -> agentscope_core::Result<ToolOutput> {
        let session_id = ctx.session_id.as_str();
        let rc = RuntimeContext::new();

        // 屏障模式：显式 task_ids。
        let task_ids: Vec<String> = match optional_string(&input, "task_ids") {
            Some(raw) => raw
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
            None => Vec::new(),
        };
        let wait_all = optional_bool(&input, "wait_all").unwrap_or(false);
        let timeout_secs = optional_i64(&input, "timeout_seconds")
            .map_or(Self::DEFAULT_TIMEOUT_SECS, |v| v.clamp(1, Self::MAX_TIMEOUT_SECS));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs as u64);

        // 解析等待集合。
        let targets: Vec<Arc<crate::task::BackgroundTask>> = if !task_ids.is_empty() {
            let mut found = Vec::new();
            let mut missing = Vec::new();
            for id in &task_ids {
                match self.repository.get_task(&rc, session_id, id) {
                    Some(task) => found.push(task),
                    None => missing.push(id.clone()),
                }
            }
            if !missing.is_empty() {
                return Ok(ToolOutput::error(format!(
                    "status: not_found\nmissing task_ids: {}\n\
No task with these IDs exists in the current session. Use task_list to reconcile.",
                    missing.join(", ")
                )));
            }
            found
        } else if wait_all {
            self.repository
                .list_tasks(&rc, session_id, None)
                .into_iter()
                .filter(|task| !task.status().is_terminal())
                .collect()
        } else {
            Vec::new()
        };

        loop {
            if !targets.is_empty() {
                // 经仓库重取：触发句柄结算（settle），状态才会推进。
                let refreshed: Vec<Arc<crate::task::BackgroundTask>> = targets
                    .iter()
                    .filter_map(|task| {
                        let id = task
                            .record
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .task_id
                            .clone();
                        self.repository.get_task(&rc, session_id, &id)
                    })
                    .collect();
                if refreshed.iter().all(|task| task.status().is_terminal()) {
                    for task in &refreshed {
                        task.update_last_checked_at();
                        // 先取值释放锁再调仓库（参数位置临时 MutexGuard 活到语句
                        // 结束，mark_delivered 内部会再锁同一 record = 自死锁）。
                        let task_id = task
                            .record
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .task_id
                            .clone();
                        self.repository.mark_delivered(&rc, session_id, &task_id);
                    }
                    let mut sb = String::new();
                    for task in &refreshed {
                        sb.push_str(&format_task_detail(task));
                        sb.push_str("\n---\n");
                    }
                    return Ok(ToolOutput::ok(sb.trim_end().to_string()));
                }
            } else {
                // inbox-any：任一待投递任务或收件箱消息。
                let pending = self.repository.find_pending_deliveries(&rc, session_id);
                if !pending.is_empty() {
                    let ids: Vec<String> = pending.iter().map(|d| d.task_id.clone()).collect();
                    return Ok(ToolOutput::ok(format!(
                        "results_ready: true\ntask_ids: {}\n\
Background task results are ready. Use task_output(task_id=...) to collect each result.",
                        ids.join(", ")
                    )));
                }
                if let Some(inbox) = &self.inbox {
                    if inbox.inbox_has_messages(session_id) {
                        return Ok(ToolOutput::ok(
                            "inbox_message: true\nA new inbox message arrived. Read it with the \
inbox tool before continuing.",
                        ));
                    }
                }
            }

            let now = tokio::time::Instant::now();
            if now >= deadline {
                if !targets.is_empty() {
                    let pending: Vec<String> = targets
                        .iter()
                        .filter(|task| !task.status().is_terminal())
                        .map(|task| {
                            task.record
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .task_id
                                .clone()
                        })
                        .collect();
                    return Ok(ToolOutput::ok(format!(
                        "timed_out: true\nstill_running: {}\n\
Wait budget exhausted before all tasks reached a terminal state. Use task_output(block=false) \
to poll, or wait again with a fresh budget.",
                        pending.join(", ")
                    )));
                }
                return Ok(ToolOutput::ok(
                    "timed_out: true\nNo inbox message or completed task arrived within the wait \
budget. Use task_list to check tracked tasks, or continue with other work.",
                ));
            }
            // sleep 截断到剩余预算。
            let remaining = deadline - now;
            let sleep_for = remaining.min(Duration::from_millis(self.poll_interval_ms));
            tokio::time::sleep(sleep_for).await;
        }
    }

    async fn call(&self, input: Value) -> agentscope_core::Result<ToolOutput> {
        let ctx = ToolContext::new(Self::NAME, Self::NAME);
        self.call_with_context(input, &ctx).await
    }
}

// ===========================================================================
// Plan 模式工具（对应 Java PlanModeTools：plan_enter / plan_write / plan_exit）
// ===========================================================================

/// `plan_enter` 工具：进入计划模式。
pub struct PlanEnterTool {
    manager: Arc<PlanModeManager>,
}

impl PlanEnterTool {
    /// 工具名。
    pub const NAME: &'static str = "plan_enter";

    /// 绑定计划模式管理器。
    pub fn new(manager: Arc<PlanModeManager>) -> Self {
        Self { manager }
    }
}

#[async_trait::async_trait]
impl Tool for PlanEnterTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Enter plan mode. Use when the user asks you to just plan a task, or when you need to \
outline the implementation steps for a non-trivial task before coding."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn call_with_context(&self, _input: Value, ctx: &ToolContext) -> agentscope_core::Result<ToolOutput> {
        let Some(state_handle) = &ctx.state else {
            return Ok(ToolOutput::error(
                "plan_enter requires an active agent state (call inside an agent run).",
            ));
        };
        let mut state = state_handle.lock().unwrap_or_else(|e| e.into_inner());
        if self.manager.is_plan_active(Some(&state)) {
            return Ok(ToolOutput::ok(
                "Already in plan mode. Continue researching and writing your plan with plan_write.",
            ));
        }
        self.manager.enter(&mut state);
        Ok(ToolOutput::ok(
            "Entered plan mode. Research the task, then write your plan to the plan file with \
plan_write. Do not make any code changes while in plan mode.",
        ))
    }

    async fn call(&self, input: Value) -> agentscope_core::Result<ToolOutput> {
        let ctx = ToolContext::new(Self::NAME, Self::NAME);
        self.call_with_context(input, &ctx).await
    }
}

/// `plan_write` 工具：把计划写入计划文件。
pub struct PlanWriteTool {
    manager: Arc<PlanModeManager>,
    workspace: Arc<WorkspaceManager>,
}

impl PlanWriteTool {
    /// 工具名。
    pub const NAME: &'static str = "plan_write";

    /// 绑定管理器与工作区。
    pub fn new(manager: Arc<PlanModeManager>, workspace: Arc<WorkspaceManager>) -> Self {
        Self { manager, workspace }
    }
}

#[async_trait::async_trait]
impl Tool for PlanWriteTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Write your plan to the plan file. Only available in plan mode. The plan should contain: \
a short goal statement, the ordered implementation steps, and the verification steps."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The full plan content (markdown)"
                }
            },
            "required": ["content"]
        })
    }

    async fn call_with_context(&self, input: Value, ctx: &ToolContext) -> agentscope_core::Result<ToolOutput> {
        let Some(content) = optional_string(&input, "content") else {
            return Ok(ToolOutput::error("content is required"));
        };
        let Some(state_handle) = &ctx.state else {
            return Ok(ToolOutput::error(
                "plan_write requires an active agent state (call inside an agent run).",
            ));
        };
        // guard 限定在块内释放，不跨 await（std MutexGuard 非 Send）。
        let path = {
            let state = state_handle.lock().unwrap_or_else(|e| e.into_inner());
            if !self.manager.is_plan_active(Some(&state)) {
                return Ok(ToolOutput::error(
                    "Plan mode is OFF — it is controlled by the user in the UI, not by tools. \
Do not call plan_enter (it does not exist). Present your plan directly in the reply instead.",
                ));
            }
            self.manager.plan_file_path(Some(&state))
        };
        match self
            .workspace
            .write_utf8_workspace_relative(&RuntimeContext::new(), &path, &content)
            .await
        {
            Ok(()) => Ok(ToolOutput::ok(format!(
                "Plan written to {path}. Review it, then call plan_exit when ready to implement."
            ))),
            Err(error) => Ok(ToolOutput::error(format!("failed to write plan: {error}"))),
        }
    }

    async fn call(&self, input: Value) -> agentscope_core::Result<ToolOutput> {
        let ctx = ToolContext::new(Self::NAME, Self::NAME);
        self.call_with_context(input, &ctx).await
    }
}

/// `plan_exit` 工具：退出计划模式（需要用户批准，对应 Java checkPermissions → ask）。
pub struct PlanExitTool {
    manager: Arc<PlanModeManager>,
}

impl PlanExitTool {
    /// 工具名。
    pub const NAME: &'static str = "plan_exit";

    /// 绑定计划模式管理器。
    pub fn new(manager: Arc<PlanModeManager>) -> Self {
        Self { manager }
    }
}

#[async_trait::async_trait]
impl Tool for PlanExitTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Exit plan mode. Requires user approval. Call this only after the plan has been written \
with plan_write and you are ready to start implementing."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn check_permissions(
        &self,
        _input: &Value,
        _context: &agentscope_core::permission::PermissionContextState,
    ) -> Option<PermissionDecision> {
        // 对齐 Java PlanModeTools.planExit 的 checkPermissions → ask。
        Some(PermissionDecision::RequireApproval)
    }

    async fn call_with_context(&self, _input: Value, ctx: &ToolContext) -> agentscope_core::Result<ToolOutput> {
        let Some(state_handle) = &ctx.state else {
            return Ok(ToolOutput::error(
                "plan_exit requires an active agent state (call inside an agent run).",
            ));
        };
        let mut state = state_handle.lock().unwrap_or_else(|e| e.into_inner());
        if !self.manager.is_plan_active(Some(&state)) {
            return Ok(ToolOutput::ok("Not in plan mode; nothing to exit."));
        }
        self.manager.exit(&mut state);
        Ok(ToolOutput::ok(
            "Exited plan mode. You may now implement the plan.",
        ))
    }

    async fn call(&self, input: Value) -> agentscope_core::Result<ToolOutput> {
        let ctx = ToolContext::new(Self::NAME, Self::NAME);
        self.call_with_context(input, &ctx).await
    }
}

// ===========================================================================
// 会话检索工具（对应 Java SessionSearchTool：session_search / session_list /
// session_history）
// ===========================================================================

/// `session_search` 工具：在知识库里按关键词全文搜索会话条目。
pub struct SessionSearchTool {
    store: Arc<dyn SessionLogStore>,
    /// 当前 agent 身份（SessionKey 的 agent 维度）。
    agent_id: String,
}

impl SessionSearchTool {
    /// 工具名。
    pub const NAME: &'static str = "session_search";
    /// 默认返回条数。
    pub const DEFAULT_LIMIT: usize = 10;

    /// 绑定日志存储与 agent 身份。
    pub fn new(store: Arc<dyn SessionLogStore>, agent_id: impl Into<String>) -> Self {
        Self {
            store,
            agent_id: agent_id.into(),
        }
    }

    fn open_log(&self, ctx: &ToolContext) -> Result<Arc<dyn agentscope_core::session::SessionLog>, ToolOutput> {
        let key = SessionKey::new(ctx.user_id.as_deref(), &self.agent_id, &ctx.session_id)
            .map_err(|e| ToolOutput::error(format!("invalid session key: {e}")))?;
        self.store
            .open(&key, None)
            .map_err(|e| ToolOutput::error(format!("failed to open session log: {e}")))
    }
}

#[async_trait::async_trait]
impl Tool for SessionSearchTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Search through past session transcripts for a keyword or phrase. Returns matching \
entries with session context."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query (keyword or phrase)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results to return (default: 10)"
                }
            },
            "required": ["query"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn call_with_context(&self, input: Value, ctx: &ToolContext) -> agentscope_core::Result<ToolOutput> {
        let Some(query) = optional_string(&input, "query") else {
            return Ok(ToolOutput::error("query is required"));
        };
        let limit = optional_i64(&input, "limit")
            .filter(|v| *v > 0)
            .map_or(Self::DEFAULT_LIMIT, |v| v as usize);
        let needle = query.to_lowercase();

        let log = match self.open_log(ctx) {
            Ok(log) => log,
            Err(output) => return Ok(output),
        };
        let view = match transcript(log.as_ref()) {
            Ok(view) => view,
            Err(e) => return Ok(ToolOutput::error(format!("failed to replay transcript: {e}"))),
        };

        let mut matches = Vec::new();
        for msg in &view.messages {
            // 对齐 Java：按消息内容的 JSON 序列化做大小写不敏感包含匹配。
            let content_json = serde_json::to_string(&msg.content).unwrap_or_default();
            if content_json.to_lowercase().contains(&needle) {
                matches.push(json!({
                    "messageId": msg.id,
                    "role": msg.role,
                    "asOfSeq": view.as_of_seq,
                    "message": msg,
                }));
                if matches.len() >= limit {
                    break;
                }
            }
        }
        if matches.is_empty() {
            return Ok(ToolOutput::ok(format!("No matches for '{query}'.")));
        }
        let payload = json!({
            "sessionId": ctx.session_id,
            "asOfSeq": view.as_of_seq,
            "matches": matches,
        });
        Ok(ToolOutput::ok(
            serde_json::to_string_pretty(&payload).unwrap_or_default(),
        ))
    }

    async fn call(&self, input: Value) -> agentscope_core::Result<ToolOutput> {
        let ctx = ToolContext::new(Self::NAME, Self::NAME);
        self.call_with_context(input, &ctx).await
    }
}

/// `session_list` 工具：列出当前身份可见的会话。
pub struct SessionListTool {
    store: Arc<dyn SessionLogStore>,
}

impl SessionListTool {
    /// 工具名。
    pub const NAME: &'static str = "session_list";

    /// 绑定日志存储。
    pub fn new(store: Arc<dyn SessionLogStore>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl Tool for SessionListTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "List available sessions for an agent, showing session IDs and metadata."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent_id": {
                    "type": "string",
                    "description": "Agent ID to list sessions for"
                }
            },
            "required": []
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn call_with_context(&self, input: Value, ctx: &ToolContext) -> agentscope_core::Result<ToolOutput> {
        let agent_id = optional_string(&input, "agent_id").unwrap_or_else(|| ctx.session_id.clone());
        let rc = RuntimeContext::new();
        match self.store.list(Some(&rc)) {
            Ok(keys) => {
                let filtered: Vec<&SessionKey> = keys
                    .iter()
                    .filter(|key| key.agent_id == agent_id || agent_id.is_empty())
                    .collect();
                if filtered.is_empty() {
                    return Ok(ToolOutput::ok("No sessions found."));
                }
                let mut sb = String::new();
                for key in filtered {
                    sb.push_str(&format!(
                        "- agent: {} session: {} user: {}\n",
                        key.agent_id,
                        key.session_id,
                        key.user_id.as_deref().unwrap_or("")
                    ));
                }
                Ok(ToolOutput::ok(sb.trim_end().to_string()))
            }
            Err(e) => Ok(ToolOutput::error(format!(
                "session discovery not supported by this store: {e}"
            ))),
        }
    }

    async fn call(&self, input: Value) -> agentscope_core::Result<ToolOutput> {
        let ctx = ToolContext::new(Self::NAME, Self::NAME);
        self.call_with_context(input, &ctx).await
    }
}

/// `session_history` 工具：读取指定会话的最近消息。
pub struct SessionHistoryTool {
    store: Arc<dyn SessionLogStore>,
    agent_id: String,
}

impl SessionHistoryTool {
    /// 工具名。
    pub const NAME: &'static str = "session_history";
    /// 默认返回条数。
    pub const DEFAULT_LAST_N: usize = 20;

    /// 绑定日志存储与 agent 身份。
    pub fn new(store: Arc<dyn SessionLogStore>, agent_id: impl Into<String>) -> Self {
        Self {
            store,
            agent_id: agent_id.into(),
        }
    }
}

#[async_trait::async_trait]
impl Tool for SessionHistoryTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Get the conversation history for a specific session. Returns the messages in the session."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "Session ID to read"
                },
                "last_n": {
                    "type": "integer",
                    "description": "Number of recent messages to return (default: 20)"
                }
            },
            "required": []
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn call_with_context(&self, input: Value, ctx: &ToolContext) -> agentscope_core::Result<ToolOutput> {
        let session_id = optional_string(&input, "session_id").unwrap_or_else(|| ctx.session_id.clone());
        let last_n = optional_i64(&input, "last_n")
            .filter(|v| *v > 0)
            .map_or(Self::DEFAULT_LAST_N, |v| v as usize);
        let key = match SessionKey::new(ctx.user_id.as_deref(), &self.agent_id, &session_id) {
            Ok(key) => key,
            Err(e) => return Ok(ToolOutput::error(format!("invalid session key: {e}"))),
        };
        let log = match self.store.open(&key, None) {
            Ok(log) => log,
            Err(e) => return Ok(ToolOutput::error(format!("failed to open session log: {e}"))),
        };
        let view = match transcript(log.as_ref()) {
            Ok(view) => view,
            Err(e) => return Ok(ToolOutput::error(format!("failed to replay transcript: {e}"))),
        };
        let start = view.messages.len().saturating_sub(last_n);
        let payload = json!({
            "sessionId": session_id,
            "asOfSeq": view.as_of_seq,
            "totalMessages": view.messages.len(),
            "messages": &view.messages[start..],
        });
        Ok(ToolOutput::ok(
            serde_json::to_string_pretty(&payload).unwrap_or_default(),
        ))
    }

    async fn call(&self, input: Value) -> agentscope_core::Result<ToolOutput> {
        let ctx = ToolContext::new(Self::NAME, Self::NAME);
        self.call_with_context(input, &ctx).await
    }
}

// ===========================================================================
// 产物交付（对应 Java ArtifactDeliveryTool + ArtifactDeliveryTarget）
// ===========================================================================

/// 交付请求（对应 Java `ArtifactDeliveryRequest`）。
#[derive(Debug, Clone)]
pub struct ArtifactDeliveryRequest {
    /// 产物内容。
    pub content: Vec<u8>,
    /// 目标文件名。
    pub file_name: String,
    /// 给用户的说明。
    pub description: Option<String>,
    /// 同名冲突时是否覆盖。
    pub force: bool,
}

/// 交付结果（对应 Java `ArtifactDeliveryResult`）。
#[derive(Debug, Clone)]
pub struct ArtifactDeliveryResult {
    /// 是否成功。
    pub successful: bool,
    /// 是否因同名冲突被拒。
    pub conflict: bool,
    /// 面向用户的消息。
    pub message: Option<String>,
    /// 错误信息。
    pub error: Option<String>,
}

/// 交付目标（对应 Java `ArtifactDeliveryTarget`；Rust 侧窄接口，无 rc 注入）。
#[async_trait::async_trait]
pub trait ArtifactDeliveryTarget: Send + Sync {
    /// 交付产物；返回 `None` 表示无可用目标（工具据此报错）。
    async fn deliver(&self, request: ArtifactDeliveryRequest) -> Option<ArtifactDeliveryResult>;
}

/// 内存交付目标（测试与嵌入用；进程内保存已交付产物）。
#[derive(Default)]
pub struct InMemoryArtifactDeliveryTarget {
    files: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl InMemoryArtifactDeliveryTarget {
    /// 空目标。
    pub fn new() -> Self {
        Self::default()
    }

    /// 已交付产物（文件名 → 内容）。
    pub fn files(&self) -> BTreeMap<String, Vec<u8>> {
        self.files.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

#[async_trait::async_trait]
impl ArtifactDeliveryTarget for InMemoryArtifactDeliveryTarget {
    async fn deliver(&self, request: ArtifactDeliveryRequest) -> Option<ArtifactDeliveryResult> {
        let mut files = self.files.lock().unwrap_or_else(|e| e.into_inner());
        if files.contains_key(&request.file_name) && !request.force {
            return Some(ArtifactDeliveryResult {
                successful: false,
                conflict: true,
                message: None,
                error: Some(format!(
                    "a file named '{}' already exists; set force=true to overwrite",
                    request.file_name
                )),
            });
        }
        files.insert(request.file_name.clone(), request.content);
        Some(ArtifactDeliveryResult {
            successful: true,
            conflict: false,
            message: Some(format!("delivered {}", request.file_name)),
            error: None,
        })
    }
}

/// `artifact_delivery` 工具：把产物交付给用户（下载/导出通道）。
pub struct ArtifactDeliveryTool {
    target: Arc<dyn ArtifactDeliveryTarget>,
}

impl ArtifactDeliveryTool {
    /// 工具名。
    pub const NAME: &'static str = "artifact_delivery";

    /// 绑定交付目标。
    pub fn new(target: Arc<dyn ArtifactDeliveryTarget>) -> Self {
        Self { target }
    }
}

#[async_trait::async_trait]
impl Tool for ArtifactDeliveryTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Deliver a generated artifact (file) to the user. Use when the user asks to export, \
download, or save a generated file outside the workspace."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "Artifact content (UTF-8 text)"
                },
                "file_name": {
                    "type": "string",
                    "description": "Target file name"
                },
                "description": {
                    "type": "string",
                    "description": "Short description shown to the user"
                },
                "force": {
                    "type": "boolean",
                    "description": "Overwrite an existing file with the same name (default: false)"
                }
            },
            "required": ["content", "file_name"]
        })
    }

    async fn call(&self, input: Value) -> agentscope_core::Result<ToolOutput> {
        let Some(content) = optional_string(&input, "content") else {
            return Ok(ToolOutput::error("content is required"));
        };
        let Some(file_name) = optional_string(&input, "file_name") else {
            return Ok(ToolOutput::error("file_name is required"));
        };
        if file_name.contains('/') || file_name.contains('\\') || file_name.contains("..") {
            return Ok(ToolOutput::error(
                "file_name must be a plain file name (no path separators).",
            ));
        }
        let request = ArtifactDeliveryRequest {
            content: content.into_bytes(),
            file_name,
            description: optional_string(&input, "description"),
            force: optional_bool(&input, "force").unwrap_or(false),
        };
        match self.target.deliver(request).await {
            Some(result) if result.successful => Ok(ToolOutput::ok(
                result.message.unwrap_or_else(|| "delivered".into()),
            )),
            Some(result) => Ok(ToolOutput::error(
                result.error.unwrap_or_else(|| "delivery failed".into()),
            )),
            None => Ok(ToolOutput::error(
                "no artifact delivery target is configured for this session",
            )),
        }
    }
}

// ===========================================================================
// 子代理规格生成（对应 Java AgentGenerateTool + SubagentSpecGenerator）
// ===========================================================================

/// 生成的子代理规格（对应 Java `GeneratedSubagentSpec`）。
#[derive(Debug, Clone)]
pub struct GeneratedSpec {
    /// 子代理名（kebab-case）。
    pub name: String,
    /// 规格正文（markdown，含 frontmatter）。
    pub markdown: String,
}

/// 子代理规格生成器（对应 Java `SubagentSpecGenerator`；LLM 生成由上层注入）。
#[async_trait::async_trait]
pub trait SubagentSpecGenerator: Send + Sync {
    /// 生成并校验规格；失败返回原因。
    async fn generate_and_validate(
        &self,
        description: &str,
        name: Option<&str>,
        existing_agents: &[String],
        workspace_path: Option<&str>,
    ) -> Result<GeneratedSpec, String>;
}

/// `agents_generate` 工具：按任务描述生成子代理规格（对应 Java `AgentGenerateTool`）。
pub struct AgentGenerateTool {
    generator: Arc<dyn SubagentSpecGenerator>,
    manager: Arc<DefaultAgentManager>,
    /// 本工具已生成的名字（AgentSpawn 批次接线前先自记防重；对齐 Java
    /// `agentManager.hasAgent` 的重复名拒绝语义）。
    generated: Mutex<std::collections::HashSet<String>>,
}

impl AgentGenerateTool {
    /// 工具名。
    pub const NAME: &'static str = "agents_generate";

    /// 绑定生成器与代理管理器。
    pub fn new(generator: Arc<dyn SubagentSpecGenerator>, manager: Arc<DefaultAgentManager>) -> Self {
        Self {
            generator,
            manager,
            generated: Mutex::new(std::collections::HashSet::new()),
        }
    }

    /// kebab-case 名校验（对齐 Java `isValidAgentName`：小写字母数字与连字符，
    /// 不以连字符开头/结尾，长度 1..=64）。
    pub fn is_valid_agent_name(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= 64
            && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !name.starts_with('-')
            && !name.ends_with('-')
            && !name.contains("--")
    }
}

#[async_trait::async_trait]
impl Tool for AgentGenerateTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Generate a new subagent definition from a task description. The generated subagent \
becomes available for delegation. Provide a clear description of the responsibilities, tools, \
and constraints of the subagent."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": "What the subagent should do, its expertise and constraints"
                },
                "name": {
                    "type": "string",
                    "description": "Optional subagent name (kebab-case). Auto-derived from the description when omitted."
                },
                "workspace_path": {
                    "type": "string",
                    "description": "Optional workspace path the subagent operates in"
                }
            },
            "required": ["description"]
        })
    }

    async fn call(&self, input: Value) -> agentscope_core::Result<ToolOutput> {
        let Some(description) = optional_string(&input, "description") else {
            return Ok(ToolOutput::error("description is required"));
        };
        let name = optional_string(&input, "name");
        if let Some(name) = &name {
            if !Self::is_valid_agent_name(name) {
                return Ok(ToolOutput::error(format!(
                    "invalid agent name '{name}': use lowercase letters, digits and hyphens; \
no leading/trailing/double hyphens; at most 64 characters."
                )));
            }
            if self.manager.has_agent(name) || self.generated.lock().unwrap_or_else(|e| e.into_inner()).contains(name) {
                return Ok(ToolOutput::error(format!(
                    "an agent named '{name}' already exists. Pick a different name."
                )));
            }
        }
        let existing = self.manager.list_agent_names();
        let workspace_path = optional_string(&input, "workspace_path");
        match self
            .generator
            .generate_and_validate(&description, name.as_deref(), &existing, workspace_path.as_deref())
            .await
        {
            Ok(spec) => {
                if !Self::is_valid_agent_name(&spec.name) {
                    return Ok(ToolOutput::error(format!(
                        "generator produced an invalid agent name '{}'",
                        spec.name
                    )));
                }
                if self.manager.has_agent(&spec.name)
                    || self
                        .generated
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .contains(&spec.name)
                {
                    return Ok(ToolOutput::error(format!(
                        "an agent named '{}' already exists. Pick a different name.",
                        spec.name
                    )));
                }
                self.generated
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(spec.name.clone());
                Ok(ToolOutput::ok(format!(
                    "Generated subagent '{}'. It is now available for delegation.",
                    spec.name
                )))
            }
            Err(error) => Ok(ToolOutput::error(format!("failed to generate subagent: {error}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::PlanModeManager;
    use crate::subagent::DefaultAgentManager;
    use crate::task::{InMemoryTaskRepository, TaskRunSpec};
    use agentscope_core::session::{InMemorySessionLogStore, SessionLogStore};
    use agentscope_core::state::AgentState;

    fn tool_ctx(session_id: &str) -> ToolContext {
        let mut ctx = ToolContext::new("call-1", "test");
        ctx.session_id = session_id.into();
        ctx.user_id = Some("user-a".into());
        ctx
    }

    fn state_ctx(session_id: &str, state: &Arc<Mutex<AgentState>>) -> ToolContext {
        let mut ctx = tool_ctx(session_id);
        ctx.state = Some(state.clone());
        ctx
    }

    fn local_spec(result: Result<String, String>) -> TaskRunSpec {
        TaskRunSpec::Local {
            execution: Arc::new(move || {
                let result = result.clone();
                Box::pin(async move { result })
            }),
        }
    }

    // ---------- ShellExecuteTool ----------

    #[tokio::test]
    async fn local_backend_reports_exit_code_and_output() {
        let backend = LocalShellBackend::new();
        let response = backend.execute("echo hello", 10).await;
        assert!(response.is_success());
        assert!(response.output.contains("hello"));
        assert!(!response.truncated);
    }

    #[tokio::test]
    async fn local_backend_reports_failure_exit_code() {
        let backend = LocalShellBackend::new();
        let command = if cfg!(windows) { "exit /b 3" } else { "exit 3" };
        let response = backend.execute(command, 10).await;
        assert_eq!(response.exit_code, Some(3));
        assert!(!response.is_success());
    }

    #[tokio::test]
    async fn anchored_backend_executes_in_root() {
        // 锚定后端：命令在 root 下执行——root 内放置标记文件，列目录应可见。
        let root = std::env::temp_dir().join(format!("agentscope-anchor-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("marker.txt"), b"ok").unwrap();
        let backend = LocalShellBackend::new_anchored(&root);
        let command = if cfg!(windows) { "dir /b" } else { "ls" };
        let response = backend.execute(command, 10).await;
        assert!(response.is_success(), "output: {}", response.output);
        assert!(response.output.contains("marker.txt"), "output: {}", response.output);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn timeout_returns_partial_output_with_notice() {
        // 超时被杀：已产出的部分输出仍返回，并附超时提示（exit_code 为 None）。
        let backend = LocalShellBackend::new();
        let command = if cfg!(windows) {
            // timeout.exe 在无控制台 stdin 下会立即报错退出，用 ping 充当 sleep。
            "echo partial-line & ping -n 6 127.0.0.1 >nul"
        } else {
            "echo partial-line; sleep 5"
        };
        let response = backend.execute(command, 1).await;
        assert_eq!(response.exit_code, None);
        assert!(response.output.contains("partial-line"), "output: {}", response.output);
        assert!(response.output.contains("超时"), "output: {}", response.output);
    }

    #[test]
    fn decode_output_prefers_utf8_and_falls_back_to_gbk_on_windows() {
        // UTF-8 字节：严格解码成功，原样返回。
        assert_eq!(decode_process_output("中文ok".as_bytes()), "中文ok");
        // GBK 字节（"中文" 的 GBK 编码 D6 D0 CE C4）：Windows 上解码回 "中文"。
        let gbk_bytes = [0xD6u8, 0xD0, 0xCE, 0xC4];
        let decoded = decode_process_output(&gbk_bytes);
        if cfg!(windows) {
            assert_eq!(decoded, "中文");
        } else {
            // 非 Windows 无 GBK 兜底：lossy 解码产生替换符，不 panic 即可。
            assert!(decoded.contains('\u{FFFD}') || decoded.is_empty());
        }
    }

    #[test]
    fn command_with_working_directory_quotes() {
        let windows = ShellExecuteTool::command_with_working_directory("C:\\my dir", "git status", true);
        assert_eq!(windows, "cd /d \"C:\\my dir\" && git status");
        let unix = ShellExecuteTool::command_with_working_directory("/tmp/x", "ls", false);
        assert_eq!(unix, "cd '/tmp/x' && ls");
    }

    #[tokio::test]
    async fn execute_rejects_absolute_working_directory() {
        let tool = ShellExecuteTool::new(Arc::new(LocalShellBackend::new()));
        let output = tool
            .call(json!({"command": "echo hi", "working_directory": "C:\\abs"}))
            .await
            .unwrap();
        assert_eq!(output.state, ToolResultState::Error);
        assert!(output.content.contains("relative path"));
    }

    #[tokio::test]
    async fn execute_requires_command() {
        let tool = ShellExecuteTool::new(Arc::new(LocalShellBackend::new()));
        let output = tool.call(json!({})).await.unwrap();
        assert_eq!(output.state, ToolResultState::Error);
        assert!(output.content.contains("command is required"));
    }

    #[tokio::test]
    async fn execute_carries_execution_details() {
        let tool = ShellExecuteTool::new(Arc::new(LocalShellBackend::new()));
        let output = tool.call(json!({"command": "echo ok"})).await.unwrap();
        assert_eq!(output.state, ToolResultState::Success);
        let details = output.execution_details.expect("details");
        assert_eq!(details.kind, "shell");
        assert_eq!(details.outcome, ToolExecutionOutcome::Succeeded);
        assert_eq!(details.exit_code, Some(0));
    }

    // ---------- Web 工具 ----------

    #[tokio::test]
    async fn web_fetch_requires_url() {
        let tool = WebFetchTool::new();
        let output = tool.call(json!({})).await.unwrap();
        assert_eq!(output.state, ToolResultState::Error);
        assert!(output.content.contains("url is required"));
    }

    #[tokio::test]
    async fn web_fetch_rejects_non_http_url() {
        let tool = WebFetchTool::new();
        let output = tool.call(json!({"url": "ftp://example.com"})).await.unwrap();
        assert_eq!(output.state, ToolResultState::Error);
        assert!(output.content.contains("http/https"));
    }

    #[tokio::test]
    async fn web_search_requires_query() {
        let tool = WebSearchTool::new();
        let output = tool.call(json!({})).await.unwrap();
        assert_eq!(output.state, ToolResultState::Error);
        assert!(output.content.contains("query is required"));
    }

    // ---------- Task 工具 ----------

    #[tokio::test]
    async fn task_output_not_found_is_tracking_error() {
        let repo = Arc::new(InMemoryTaskRepository::new());
        let tool = TaskOutputTool::with_poll_interval(repo, 10);
        let ctx = tool_ctx("s1");
        let output = tool
            .call_with_context(json!({"task_id": "missing"}), &ctx)
            .await
            .unwrap();
        assert_eq!(output.state, ToolResultState::Error);
        assert!(output.content.contains("status: not_found"));
    }

    #[tokio::test]
    async fn task_output_blocks_until_terminal() {
        let repo = Arc::new(InMemoryTaskRepository::new());
        let rc = RuntimeContext::new();
        repo.put_task(&rc, "t1", "worker", "s1", local_spec(Ok("done!".into())))
            .unwrap();
        let tool = TaskOutputTool::with_poll_interval(repo.clone(), 10);
        let ctx = tool_ctx("s1");
        let output = tool
            .call_with_context(json!({"task_id": "t1", "block": true, "timeout": 5000}), &ctx)
            .await
            .unwrap();
        assert_eq!(output.state, ToolResultState::Success);
        assert!(output.content.contains("status:"));
        assert!(output.content.contains("done!"));
    }

    #[tokio::test]
    async fn task_output_non_block_reports_running() {
        let repo = Arc::new(InMemoryTaskRepository::new());
        let rc = RuntimeContext::new();
        // 长任务：sleep 500ms 后完成。
        let spec = TaskRunSpec::Local {
            execution: Arc::new(|| {
                Box::pin(async {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    Ok("late".into())
                })
            }),
        };
        repo.put_task(&rc, "t2", "worker", "s1", spec).unwrap();
        let tool = TaskOutputTool::with_poll_interval(repo, 10);
        let ctx = tool_ctx("s1");
        let output = tool
            .call_with_context(json!({"task_id": "t2", "block": false}), &ctx)
            .await
            .unwrap();
        assert_eq!(output.state, ToolResultState::Running);
        assert!(output.content.contains("running"));
    }

    #[tokio::test]
    async fn task_list_filters_by_status() {
        let repo = Arc::new(InMemoryTaskRepository::new());
        let rc = RuntimeContext::new();
        repo.put_task(&rc, "t1", "worker", "s1", local_spec(Ok("ok".into())))
            .unwrap();
        repo.put_task(&rc, "t2", "worker", "s1", local_spec(Err("boom".into())))
            .unwrap();
        // 等待两个任务完成。
        tokio::time::sleep(Duration::from_millis(150)).await;

        let tool = TaskListTool::new(repo.clone());
        let ctx = tool_ctx("s1");
        let all = tool.call_with_context(json!({}), &ctx).await.unwrap();
        assert!(all.content.contains("2 tracked task(s)"));

        let failed = tool
            .call_with_context(json!({"status_filter": "failed"}), &ctx)
            .await
            .unwrap();
        assert!(failed.content.contains("t2"));
        assert!(!failed.content.contains("t1"));

        let none = tool
            .call_with_context(json!({"status_filter": "cancelled"}), &ctx)
            .await
            .unwrap();
        assert!(none.content.contains("No background tasks"));
    }

    #[tokio::test]
    async fn task_cancel_requests_cancellation() {
        let repo = Arc::new(InMemoryTaskRepository::new());
        let rc = RuntimeContext::new();
        let spec = TaskRunSpec::Local {
            execution: Arc::new(|| {
                Box::pin(async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Ok("never".into())
                })
            }),
        };
        repo.put_task(&rc, "t1", "worker", "s1", spec).unwrap();
        let tool = TaskCancelTool::new(repo.clone());
        let ctx = tool_ctx("s1");
        let output = tool
            .call_with_context(json!({"task_id": "t1"}), &ctx)
            .await
            .unwrap();
        assert!(output.content.contains("Cancellation requested"));

        // 已终态任务不可取消。
        repo.put_task(&rc, "t2", "worker", "s1", local_spec(Ok("ok".into())))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        let output = tool
            .call_with_context(json!({"task_id": "t2"}), &ctx)
            .await
            .unwrap();
        assert!(output.content.contains("already in terminal state"));
    }

    // ---------- WaitAsyncResultsTool ----------

    #[tokio::test]
    async fn wait_barrier_collects_all_terminal_tasks() {
        let repo = Arc::new(InMemoryTaskRepository::new());
        let rc = RuntimeContext::new();
        repo.put_task(&rc, "a", "worker", "s1", local_spec(Ok("A".into())))
            .unwrap();
        repo.put_task(&rc, "b", "worker", "s1", local_spec(Ok("B".into())))
            .unwrap();
        let tool = WaitAsyncResultsTool::with_poll_interval(repo, None, 10);
        let ctx = tool_ctx("s1");
        let output = tool
            .call_with_context(json!({"task_ids": "a, b", "timeout_seconds": 5}), &ctx)
            .await
            .unwrap();
        assert!(output.content.contains("A"));
        assert!(output.content.contains("B"));
    }

    #[tokio::test]
    async fn wait_barrier_missing_task_is_not_found() {
        let repo = Arc::new(InMemoryTaskRepository::new());
        let tool = WaitAsyncResultsTool::with_poll_interval(repo, None, 10);
        let ctx = tool_ctx("s1");
        let output = tool
            .call_with_context(json!({"task_ids": "ghost", "timeout_seconds": 1}), &ctx)
            .await
            .unwrap();
        assert_eq!(output.state, ToolResultState::Error);
        assert!(output.content.contains("missing task_ids"));
    }

    #[tokio::test]
    async fn wait_barrier_times_out_with_still_running() {
        let repo = Arc::new(InMemoryTaskRepository::new());
        let rc = RuntimeContext::new();
        let spec = TaskRunSpec::Local {
            execution: Arc::new(|| {
                Box::pin(async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Ok("never".into())
                })
            }),
        };
        repo.put_task(&rc, "slow", "worker", "s1", spec).unwrap();
        let tool = WaitAsyncResultsTool::with_poll_interval(repo, None, 10);
        let ctx = tool_ctx("s1");
        let output = tool
            .call_with_context(json!({"task_ids": "slow", "timeout_seconds": 1}), &ctx)
            .await
            .unwrap();
        assert!(output.content.contains("timed_out: true"));
        assert!(output.content.contains("still_running: slow"));
    }

    #[tokio::test]
    async fn wait_inbox_any_reports_pending_delivery() {
        let repo = Arc::new(InMemoryTaskRepository::new());
        let rc = RuntimeContext::new();
        repo.put_task(&rc, "done", "worker", "s1", local_spec(Ok("ok".into())))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        let tool = WaitAsyncResultsTool::with_poll_interval(repo, None, 10);
        let ctx = tool_ctx("s1");
        let output = tool.call_with_context(json!({"timeout_seconds": 1}), &ctx).await.unwrap();
        assert!(output.content.contains("results_ready: true"));
        assert!(output.content.contains("done"));
    }

    #[tokio::test]
    async fn wait_inbox_any_times_out_when_idle() {
        let repo = Arc::new(InMemoryTaskRepository::new());
        let tool = WaitAsyncResultsTool::with_poll_interval(repo, None, 10);
        let ctx = tool_ctx("s1");
        let output = tool.call_with_context(json!({"timeout_seconds": 1}), &ctx).await.unwrap();
        assert!(output.content.contains("timed_out: true"));
    }

    /// 收件箱探针：有消息即返回。
    struct StaticInbox(bool);

    impl InboxProbe for StaticInbox {
        fn inbox_has_messages(&self, _session_id: &str) -> bool {
            self.0
        }
    }

    #[tokio::test]
    async fn wait_inbox_any_reports_inbox_message() {
        let repo = Arc::new(InMemoryTaskRepository::new());
        let tool = WaitAsyncResultsTool::with_poll_interval(
            repo,
            Some(Arc::new(StaticInbox(true))),
            10,
        );
        let ctx = tool_ctx("s1");
        let output = tool.call_with_context(json!({"timeout_seconds": 1}), &ctx).await.unwrap();
        assert!(output.content.contains("inbox_message: true"));
    }

    // ---------- PlanModeTools ----------

    fn temp_workspace(tag: &str) -> Arc<WorkspaceManager> {
        let dir = std::env::temp_dir().join(format!(
            "agentscope-tools-{tag}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Arc::new(WorkspaceManager::new(dir))
    }

    #[tokio::test]
    async fn plan_enter_write_exit_flow() {
        let manager = Arc::new(PlanModeManager::new());
        let workspace = temp_workspace("plan");
        let state = Arc::new(Mutex::new(AgentState::default()));
        let ctx = state_ctx("s1", &state);

        let enter = PlanEnterTool::new(manager.clone());
        let output = enter.call_with_context(json!({}), &ctx).await.unwrap();
        assert!(output.content.contains("Entered plan mode"));

        // 重复进入给提示。
        let output = enter.call_with_context(json!({}), &ctx).await.unwrap();
        assert!(output.content.contains("Already in plan mode"));

        let write = PlanWriteTool::new(manager.clone(), workspace.clone());
        let output = write
            .call_with_context(json!({"content": "# Plan\n1. do it"}), &ctx)
            .await
            .unwrap();
        assert!(output.content.contains("Plan written to"));

        let exit = PlanExitTool::new(manager.clone());
        // 退出需要用户批准。
        let decision = exit
            .check_permissions(&json!({}), &Default::default())
            .await;
        assert!(matches!(decision, Some(PermissionDecision::RequireApproval)));

        let output = exit.call_with_context(json!({}), &ctx).await.unwrap();
        assert!(output.content.contains("Exited plan mode"));

        // 退出后 plan_write 报错。
        let output = write
            .call_with_context(json!({"content": "x"}), &ctx)
            .await
            .unwrap();
        assert_eq!(output.state, ToolResultState::Error);
        assert!(output.content.contains("Plan mode is OFF"));
    }

    #[tokio::test]
    async fn plan_write_requires_active_plan() {
        let manager = Arc::new(PlanModeManager::new());
        let workspace = temp_workspace("plan2");
        let state = Arc::new(Mutex::new(AgentState::default()));
        let ctx = state_ctx("s1", &state);
        let write = PlanWriteTool::new(manager, workspace);
        let output = write
            .call_with_context(json!({"content": "x"}), &ctx)
            .await
            .unwrap();
        assert_eq!(output.state, ToolResultState::Error);
        assert!(output.content.contains("Plan mode is OFF"));
    }

    #[tokio::test]
    async fn plan_tools_require_state() {
        let manager = Arc::new(PlanModeManager::new());
        let ctx = tool_ctx("s1");
        let enter = PlanEnterTool::new(manager.clone());
        let output = enter.call_with_context(json!({}), &ctx).await.unwrap();
        assert_eq!(output.state, ToolResultState::Error);
        assert!(output.content.contains("requires an active agent state"));
    }

    // ---------- SessionSearchTool ----------

    async fn seeded_log(
        store: &InMemorySessionLogStore,
        session_id: &str,
    ) -> Arc<dyn agentscope_core::session::SessionLog> {
        use agentscope_core::message::{ContentBlock, Role};
        use agentscope_core::session::SessionEvent;

        let key = SessionKey::new(Some("user-a"), "agent-x", session_id).unwrap();
        let log = store.open(&key, None).unwrap();
        let writer = log.acquire("tester", Duration::from_secs(60)).unwrap();
        let msg = agentscope_core::message::Msg {
            id: "m1".into(),
            name: "user".into(),
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "the quick brown fox".into(),
            }],
            metadata: None,
            timestamp: None,
            usage: None,
            generate_reason: None,
        };
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let event = SessionEvent::new(
            uuid::Uuid::new_v4().to_string(),
            1,
            now_ms,
            "message/user",
            Some("run-1".into()),
            Some("turn-1".into()),
            true,
            serde_json::to_string(&json!({ "message": msg })).unwrap(),
        )
        .unwrap();
        log.commit(&writer, "b1", 0, vec![event]).unwrap();
        log.release(&writer).unwrap();
        log
    }

    #[tokio::test]
    async fn session_search_matches_keyword() {
        let store = Arc::new(InMemorySessionLogStore::new());
        seeded_log(&store, "s1").await;
        let tool = SessionSearchTool::new(store, "agent-x");
        let ctx = tool_ctx("s1");

        let hit = tool
            .call_with_context(json!({"query": "QUICK brown"}), &ctx)
            .await
            .unwrap();
        assert_eq!(hit.state, ToolResultState::Success);
        assert!(hit.content.contains("quick brown fox"));

        let miss = tool
            .call_with_context(json!({"query": "zebra"}), &ctx)
            .await
            .unwrap();
        assert!(miss.content.contains("No matches"));
    }

    #[tokio::test]
    async fn session_history_returns_recent_messages() {
        let store = Arc::new(InMemorySessionLogStore::new());
        seeded_log(&store, "s1").await;
        let tool = SessionHistoryTool::new(store, "agent-x");
        let ctx = tool_ctx("s1");
        let output = tool.call_with_context(json!({"last_n": 5}), &ctx).await.unwrap();
        assert!(output.content.contains("quick brown fox"));
        assert!(output.content.contains("totalMessages"));
    }

    // ---------- ArtifactDeliveryTool ----------

    #[tokio::test]
    async fn artifact_delivery_success_and_conflict() {
        let target = Arc::new(InMemoryArtifactDeliveryTarget::new());
        let tool = ArtifactDeliveryTool::new(target.clone());

        let output = tool
            .call(json!({"content": "hello", "file_name": "greeting.txt", "description": "demo"}))
            .await
            .unwrap();
        assert_eq!(output.state, ToolResultState::Success);
        assert!(output.content.contains("delivered greeting.txt"));

        // 同名冲突。
        let output = tool
            .call(json!({"content": "other", "file_name": "greeting.txt"}))
            .await
            .unwrap();
        assert_eq!(output.state, ToolResultState::Error);
        assert!(output.content.contains("already exists"));

        // force 覆盖。
        let output = tool
            .call(json!({"content": "other", "file_name": "greeting.txt", "force": true}))
            .await
            .unwrap();
        assert_eq!(output.state, ToolResultState::Success);
        let files = target.files();
        assert_eq!(files.get("greeting.txt").unwrap(), b"other");
    }

    #[tokio::test]
    async fn artifact_delivery_rejects_path_separators() {
        let target = Arc::new(InMemoryArtifactDeliveryTarget::new());
        let tool = ArtifactDeliveryTool::new(target);
        let output = tool
            .call(json!({"content": "x", "file_name": "a/b.txt"}))
            .await
            .unwrap();
        assert_eq!(output.state, ToolResultState::Error);
        assert!(output.content.contains("plain file name"));
    }

    // ---------- AgentGenerateTool ----------

    #[test]
    fn agent_name_validation() {
        assert!(AgentGenerateTool::is_valid_agent_name("code-reviewer"));
        assert!(AgentGenerateTool::is_valid_agent_name("a1"));
        assert!(!AgentGenerateTool::is_valid_agent_name(""));
        assert!(!AgentGenerateTool::is_valid_agent_name("-lead"));
        assert!(!AgentGenerateTool::is_valid_agent_name("lead-"));
        assert!(!AgentGenerateTool::is_valid_agent_name("a--b"));
        assert!(!AgentGenerateTool::is_valid_agent_name("Big"));
        assert!(!AgentGenerateTool::is_valid_agent_name(&"x".repeat(65)));
    }

    struct FixedGenerator(GeneratedSpec);

    #[async_trait::async_trait]
    impl SubagentSpecGenerator for FixedGenerator {
        async fn generate_and_validate(
            &self,
            _description: &str,
            _name: Option<&str>,
            _existing: &[String],
            _workspace_path: Option<&str>,
        ) -> Result<GeneratedSpec, String> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn agent_generate_rejects_duplicate_name() {
        let manager = Arc::new(DefaultAgentManager::new());
        let generator = Arc::new(FixedGenerator(GeneratedSpec {
            name: "researcher".into(),
            markdown: "---\nname: researcher\n---\nbody".into(),
        }));
        let tool = AgentGenerateTool::new(generator, manager.clone());

        let output = tool
            .call(json!({"description": "does research", "name": "researcher"}))
            .await
            .unwrap();
        assert_eq!(output.state, ToolResultState::Success);
        assert!(output.content.contains("Generated subagent 'researcher'"));

        // 同名再生成被拒。
        let output = tool
            .call(json!({"description": "does research", "name": "researcher"}))
            .await
            .unwrap();
        assert_eq!(output.state, ToolResultState::Error);
        assert!(output.content.contains("already exists"));
    }

    #[tokio::test]
    async fn agent_generate_rejects_invalid_name() {
        let manager = Arc::new(DefaultAgentManager::new());
        let generator = Arc::new(FixedGenerator(GeneratedSpec {
            name: "x".into(),
            markdown: String::new(),
        }));
        let tool = AgentGenerateTool::new(generator, manager);
        let output = tool
            .call(json!({"description": "d", "name": "Bad Name"}))
            .await
            .unwrap();
        assert_eq!(output.state, ToolResultState::Error);
        assert!(output.content.contains("invalid agent name"));
    }
}
