//! 数据库执行层（db 工作区）：`db_query`（只读，免确认）+ `db_dml`（变更，
//! ask 规则 → 每次确认）工具 + 供前端查询模式直接调用的执行函数。
//!
//! 驱动策略遵循 `db_kind.rs` 的既定约定：网络数据库不编译驱动，执行时调用
//! 用户机器上已装的官方 CLI（`psql` / `mysql` / `sqlcmd`），按 PATH 探测；
//! sqlite 走 rusqlite bundled。输出统一为 CSV 文本（psql --csv / mysql
//! --batch / sqlcmd -s,），agent 与前端查询模式共用同一格式。

use std::collections::HashMap;
use std::sync::Arc;

use agentscope_core::error::Result;
use agentscope_core::tool::{Tool, ToolContext, ToolOutput};
use serde_json::{json, Value};

use crate::config::DatabaseServerConfig;

/// 单次输出上限（防上下文打爆）。
const MAX_OUTPUT: usize = 64 * 1024;
/// CLI 执行超时。
const CLI_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// 数据库档案目录（AppState 持有；save/delete 时同步更新，工具按名查档案）。
#[derive(Default)]
pub struct DbCatalog {
    map: std::sync::RwLock<HashMap<String, DatabaseServerConfig>>,
}

impl DbCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn replace_all(&self, databases: &[DatabaseServerConfig]) {
        let mut map = self.map.write().unwrap_or_else(|e| e.into_inner());
        map.clear();
        for d in databases {
            map.insert(d.name.clone(), d.clone());
        }
    }

    pub fn get(&self, name: &str) -> std::result::Result<DatabaseServerConfig, String> {
        let map = self.map.read().unwrap_or_else(|e| e.into_inner());
        map.get(name).cloned().ok_or_else(|| {
            // 报错带可用 id 清单：模型传错名时下一轮即可自纠。
            let mut ids: Vec<&String> = map.keys().collect();
            ids.sort();
            let list = if ids.is_empty() {
                "（当前没有任何数据库档案，请先在设置页添加）".to_string()
            } else {
                ids.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
            };
            format!("数据库 `{name}` 不存在。可用数据库 id：{list}")
        })
    }

    /// 可用数据库 id 列表（按名字排序；allowed 非空时只列白名单内的）。
    pub fn ids(&self, allowed: Option<&[String]>) -> Vec<String> {
        let map = self.map.read().unwrap_or_else(|e| e.into_inner());
        let mut names: Vec<String> = map
            .keys()
            .filter(|n| match allowed {
                Some(a) => a.iter().any(|x| x == *n),
                None => true,
            })
            .cloned()
            .collect();
        names.sort();
        names
    }

    /// 可用数据库摘要（供工具参数 schema 动态生成）：`id (类型) 地址 库名`。
    /// 刻意不含账号密码——LLM 只需要 id 与位置信息，凭证永不进模型上下文。
    pub fn summaries(&self, allowed: Option<&[String]>) -> Vec<String> {
        let map = self.map.read().unwrap_or_else(|e| e.into_inner());
        let mut out: Vec<String> = map
            .values()
            .filter(|d| match allowed {
                Some(a) => a.iter().any(|x| x == &d.name),
                None => true,
            })
            .map(|d| match d.db_type.as_str() {
                "sqlite" => format!("{} (sqlite) path={}", d.name, d.host),
                _ => format!(
                    "{} ({}) {}:{} database={}",
                    d.name, d.db_type, d.host, d.port, d.database
                ),
            })
            .collect();
        out.sort();
        out
    }
}

/// 语句分类（词法级，与 sql_tool.rs 的只读白名单同一口径）。
pub fn classify(sql: &str) -> &'static str {
    let head = sql
        .trim()
        .trim_start_matches('(')
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match head.as_str() {
        "select" | "with" | "explain" | "show" | "describe" | "desc" | "pragma" => "read",
        "insert" | "update" | "delete" | "replace" | "merge" | "create" | "drop" | "alter"
        | "truncate" | "grant" | "revoke" | "begin" | "commit" | "rollback" | "call" => "write",
        _ => "unknown",
    }
}

/// 执行一条 SQL（单条；分号结尾允许，多语句拒绝——与只读工具同口径）。
pub async fn execute(db: &DatabaseServerConfig, sql: &str) -> std::result::Result<String, String> {
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        return Err("sql is empty".into());
    }
    if trimmed.trim_end_matches(';').contains(';') {
        return Err("multiple statements are not allowed".into());
    }
    match db.db_type.as_str() {
        "sqlite" => execute_sqlite(db, trimmed).await,
        "postgres" => execute_cli(db, trimmed).await,
        "mysql" => execute_cli(db, trimmed).await,
        "sqlserver" => execute_cli(db, trimmed).await,
        other => Err(format!("数据库类型 `{other}` 暂不支持在线执行（可先在设置页确认类型）")),
    }
}

/// sqlite：rusqlite 直连（host 字段 = 文件路径）。
async fn execute_sqlite(
    db: &DatabaseServerConfig,
    sql: &str,
) -> std::result::Result<String, String> {
    let path = std::path::PathBuf::from(&db.host);
    let sql = sql.to_string();
    let readonly = classify(&sql) == "read";
    tokio::task::spawn_blocking(move || -> std::result::Result<String, String> {
        if readonly {
            let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
            let conn = rusqlite::Connection::open_with_flags(&path, flags)
                .map_err(|e| format!("open database: {e}"))?;
            let mut stmt = conn.prepare(&sql).map_err(|e| format!("prepare: {e}"))?;
            let cols: Vec<String> = stmt
                .column_names()
                .into_iter()
                .map(|s| s.to_string())
                .collect();
            let mut rows_out: Vec<Vec<String>> = Vec::new();
            let mut rows = stmt
                .query([])
                .map_err(|e| format!("query: {e}"))?;
            while let Some(row) = rows.next().map_err(|e| format!("fetch: {e}"))? {
                let mut r = Vec::with_capacity(cols.len());
                for i in 0..cols.len() {
                    let v: String = match row.get_ref(i) {
                        Ok(rusqlite::types::ValueRef::Null) => String::new(),
                        Ok(rusqlite::types::ValueRef::Integer(n)) => n.to_string(),
                        Ok(rusqlite::types::ValueRef::Real(f)) => f.to_string(),
                        Ok(rusqlite::types::ValueRef::Text(t)) => {
                            String::from_utf8_lossy(t).into_owned()
                        }
                        Ok(rusqlite::types::ValueRef::Blob(b)) => {
                            format!("<blob {} bytes>", b.len())
                        }
                        Err(e) => format!("<err {e}>"),
                    };
                    r.push(v);
                }
                rows_out.push(r);
                if rows_out.len() > 500 {
                    break;
                }
            }
            Ok(to_csv(&cols, &rows_out))
        } else {
            let conn = rusqlite::Connection::open(&path)
                .map_err(|e| format!("open database: {e}"))?;
            let changed = conn.execute(&sql, []).map_err(|e| format!("execute: {e}"))?;
            Ok(format!("OK, {changed} row(s) affected"))
        }
    })
    .await
    .map_err(|e| format!("task failed: {e}"))?
}

/// Windows 下补全可执行文件后缀（其他平台原样）。
fn exe_name(program: &str) -> String {
    if cfg!(windows) && !program.ends_with(".exe") {
        format!("{program}.exe")
    } else {
        program.to_string()
    }
}

/// 按 PATH 环境变量逐目录探测可执行文件。
fn find_in_path(program: &str) -> Option<std::path::PathBuf> {
    let exe = exe_name(program);
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let cand = dir.join(&exe);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// 各数据库 CLI 的常见安装根目录（PATH 之外的兜底探索；Windows 为主——
/// MySQL/PostgreSQL/SQL Server 装完不进 PATH 是常态）。
fn common_cli_roots(db_type: &str) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if cfg!(windows) {
        let pf = std::env::var("ProgramFiles")
            .unwrap_or_else(|_| r"C:\Program Files".to_string());
        match db_type {
            // C:\Program Files\MySQL\MySQL Server 8.0\bin\mysql.exe
            "mysql" => {
                out.push(std::path::PathBuf::from(&pf).join("MySQL"));
                out.push(std::path::PathBuf::from(r"C:\xampp\mysql\bin"));
            }
            // C:\Program Files\PostgreSQL\16\bin\psql.exe
            "postgres" => out.push(std::path::PathBuf::from(&pf).join("PostgreSQL")),
            // C:\Program Files\Microsoft SQL Server\MSSQL15.X\Tools\Binn\SQLCMD.EXE
            "sqlserver" => {
                out.push(std::path::PathBuf::from(&pf).join("Microsoft SQL Server"))
            }
            _ => {}
        }
    } else {
        // Unix：PATH 通常已覆盖；补 homebrew 等常见位。
        for d in ["/usr/local/bin", "/opt/homebrew/bin", "/usr/bin"] {
            out.push(std::path::PathBuf::from(d));
        }
    }
    out
}

/// 在目录树下找名为 `exe` 的文件（深度受限；文件名比较忽略大小写——
/// Windows 上 SQLCMD.EXE 常为大写）。同层先文件后子目录，子目录倒序
/// （版本号大的优先，如 PostgreSQL\16 排在 15 前）。
fn find_exe_under(dir: &std::path::Path, exe: &str, depth: usize) -> Option<std::path::PathBuf> {
    if depth == 0 {
        return None;
    }
    let rd = std::fs::read_dir(dir).ok()?;
    let mut entries: Vec<std::fs::DirEntry> = rd.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in &entries {
        let p = e.path();
        if p.is_file() && e.file_name().eq_ignore_ascii_case(exe) {
            return Some(p);
        }
    }
    entries.reverse();
    for e in &entries {
        let p = e.path();
        if p.is_dir() {
            if let Some(f) = find_exe_under(&p, exe, depth - 1) {
                return Some(f);
            }
        }
    }
    None
}

/// 自动探索 CLI：PATH → 常见安装目录。找到返回完整路径。
fn auto_discover_cli(db_type: &str, program: &str) -> Option<std::path::PathBuf> {
    if let Some(p) = find_in_path(program) {
        return Some(p);
    }
    let exe = exe_name(program);
    for root in common_cli_roots(db_type) {
        if let Some(p) = find_exe_under(&root, &exe, 4) {
            return Some(p);
        }
    }
    None
}

/// 探测并返回人类可读结果（Ok=完整路径，Err=带下一步建议的提示）。
/// 供保存档案时的验证与前端「探测」按钮共用。
pub fn probe_cli(
    db_type: &str,
    cli_path: &str,
    program: &str,
) -> std::result::Result<String, String> {
    let spec = cli_path.trim();
    if !spec.is_empty() {
        let p = std::path::Path::new(spec);
        if p.is_file() {
            return Ok(spec.to_string());
        }
        if p.is_dir() {
            let cand = p.join(exe_name(program));
            if cand.is_file() {
                return Ok(cand.to_string_lossy().into_owned());
            }
            return Err(format!(
                "指定目录 `{spec}` 里没有 {program}（找 {}）",
                exe_name(program)
            ));
        }
        return Err(format!("指定的 CLI 路径 `{spec}` 不存在"));
    }
    if let Some(p) = auto_discover_cli(db_type, program) {
        return Ok(p.to_string_lossy().into_owned());
    }
    Err(format!(
        "未找到 {program}（已尝试 PATH 与常见安装目录）。请安装后重试，或在数据库档案里手动指定 CLI 路径"
    ))
}

/// 解析执行时实际 spawn 的程序路径：档案显式指定的 `cli_path` 优先——
/// 填可执行文件完整路径直接用，填目录则拼接程序名；指定了但不可用时
/// 原样返回（spawn 报错带上该路径，用户可据此自查，不静默回退）。
/// 未指定时走自动探索（PATH → 常见安装目录），都失败返回裸程序名。
fn resolve_cli_program(db: &DatabaseServerConfig, program: &str) -> String {
    let spec = db.cli_path.trim();
    if !spec.is_empty() {
        let p = std::path::Path::new(spec);
        if p.is_file() {
            return spec.to_string();
        }
        if p.is_dir() {
            // 无条件拼接：即使程序不在，spawn 的报错也会带上完整拼接
            // 路径（比拿目录路径去 spawn 的报错更可读）。
            return p.join(exe_name(program)).to_string_lossy().into_owned();
        }
        return spec.to_string();
    }
    auto_discover_cli(&db.db_type, program)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.to_string())
}

/// 子进程输出解码：UTF-8 优先，失败按 GBK 兜底——Windows 下 CLI 默认输出
/// OEM/ANSI 代码页（中文系统为 GBK）；上方已通过参数/环境变量让各 CLI 输出
/// UTF-8，此处兜底参数不生效的旧版 CLI。
fn decode_output(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    let (s, _enc, _had_errors) = encoding_rs::GBK.decode(bytes);
    s.into_owned()
}

/// 网络数据库：拼 CLI 命令执行（密码经环境变量/参数传递，仅本机进程可见）。
async fn execute_cli(
    db: &DatabaseServerConfig,
    sql: &str,
) -> std::result::Result<String, String> {
    let (mut cmd, envs): (tokio::process::Command, Vec<(String, String)>) =
        match db.db_type.as_str() {
            "postgres" => {
                let mut c = tokio::process::Command::new(resolve_cli_program(db, "psql"));
                c.args([
                    "--csv",
                    "-h",
                    &db.host,
                    "-p",
                    &db.port.to_string(),
                    "-U",
                    &db.username,
                    "-d",
                    &db.database,
                    "-c",
                    sql,
                ]);
                (
                    c,
                    vec![
                        ("PGPASSWORD".into(), db.password.clone()),
                        // Windows 下 psql 输出编码跟 client_encoding（默认跟
                        // locale，中文系统为 GBK）——强制 UTF-8。
                        ("PGCLIENTENCODING".into(), "UTF8".into()),
                    ],
                )
            }
            "mysql" => {
                let mut c = tokio::process::Command::new(resolve_cli_program(db, "mysql"));
                c.args([
                    "--batch",
                    // 输出按该字符集编码（不设时 Windows 下跟平台，中文系统
                    // 常为 GBK）——强制 UTF-8。
                    "--default-character-set=utf8mb4",
                    "-h",
                    &db.host,
                    "-P",
                    &db.port.to_string(),
                    "-u",
                    &db.username,
                    &format!("-p{}", db.password),
                    &db.database,
                    "-e",
                    sql,
                ]);
                (c, vec![])
            }
            "sqlserver" => {
                let mut c = tokio::process::Command::new(resolve_cli_program(db, "sqlcmd"));
                c.args([
                    "-S",
                    &format!("{},{}", db.host, db.port),
                    "-U",
                    &db.username,
                    "-P",
                    &db.password,
                    "-d",
                    &db.database,
                    "-Q",
                    sql,
                    "-s",
                    ",",
                    "-W",
                    // 输出代码页 UTF-8（不设时默认 OEM，中文系统为 GBK）。
                    "-f",
                    "65001",
                ]);
                (c, vec![])
            }
            other => return Err(format!("`{other}` 无 CLI 执行路径")),
        };
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let child = cmd.spawn().map_err(|e| {
        if db.cli_path.trim().is_empty() {
            format!(
                "启动 {} 失败（{e}）：请确认已安装该 CLI 并加入 PATH，或在数据库档案里指定 CLI 路径",
                db.db_type
            )
        } else {
            format!(
                "启动 {} 失败（{e}）：档案指定的 CLI 路径 `{}` 不可用，请检查文件是否存在（可填目录或完整可执行文件路径）",
                db.db_type,
                db.cli_path.trim()
            )
        }
    })?;
    let output = tokio::time::timeout(CLI_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| "执行超时（30s）".to_string())?
        .map_err(|e| format!("wait failed: {e}"))?;

    let mut text = decode_output(&output.stdout);
    let err = decode_output(&output.stderr);
    if !err.trim().is_empty() {
        text.push_str(&format!("\n[stderr] {}", err.trim()));
    }
    if text.len() > MAX_OUTPUT {
        text.truncate(MAX_OUTPUT);
        text.push_str("\n…(output truncated)");
    }
    if !output.status.success() && err.trim().is_empty() {
        return Err(format!("exit {}: {}", output.status, text.trim()));
    }
    if !output.status.success() {
        return Err(format!("exit {}: {}", output.status, err.trim()));
    }
    Ok(text)
}

/// 行集 → CSV（值含逗号/引号/换行时引号包裹）。
pub fn to_csv(header: &[String], rows: &[Vec<String>]) -> String {
    let esc = |s: &str| -> String {
        if s.contains(',') || s.contains('"') || s.contains('\n') {
            format!("\"{}\"", s.replace('"', "\"\""))
        } else {
            s.to_string()
        }
    };
    let mut out = header
        .iter()
        .map(|h| esc(h))
        .collect::<Vec<_>>()
        .join(",");
    for r in rows {
        out.push('\n');
        out.push_str(&r.iter().map(|v| esc(v)).collect::<Vec<_>>().join(","));
    }
    out
}

/// `db_query`：只读 SQL（SELECT/WITH/EXPLAIN/SHOW/DESC），免确认。
pub struct DbQueryTool {
    catalog: Arc<DbCatalog>,
    /// 允许访问的数据库档案名白名单；`None` = 不限（工作区未绑定列表时
    /// 按类别默认开放）。`Some` 时调用参数 `database` 不在名单内即拒绝。
    allowed_databases: Option<Vec<String>>,
    /// 会话状态目录（`<workspace>/state/studio`）：执行时按 `ctx.session_id`
    /// 读会话绑定（binding.json），把白名单收窄到会话绑定的那一个库。
    workspace_state_dir: Option<std::path::PathBuf>,
}

impl DbQueryTool {
    pub fn new(
        catalog: Arc<DbCatalog>,
        allowed_databases: Option<Vec<String>>,
        workspace_state_dir: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            catalog,
            allowed_databases,
            workspace_state_dir,
        }
    }

    /// 会话绑定的数据库档案名（未绑定/读取失败 → None）。
    fn session_binding(&self, session_id: &str) -> Option<String> {
        let dir = self.workspace_state_dir.as_ref()?;
        let bytes = std::fs::read(dir.join(session_id).join("binding.json")).ok()?;
        let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        value
            .get("database")?
            .as_str()
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
    }

    /// 本次调用实际可用的库名单：工作区白名单 ∩ 会话绑定（绑定非空时收窄
    /// 到绑定那一个；绑定不在白名单内 → 空名单 = 全部拒绝）。
    fn effective_allowed(&self, session_id: Option<&str>) -> Option<Vec<String>> {
        let bound = session_id.and_then(|sid| self.session_binding(sid));
        match (self.allowed_databases.clone(), bound) {
            (None, None) => None,
            (None, Some(b)) => Some(vec![b]),
            (Some(w), None) => Some(w),
            (Some(w), Some(b)) => Some(if w.contains(&b) { vec![b] } else { vec![] }),
        }
    }
}

#[async_trait::async_trait]
impl Tool for DbQueryTool {
    fn name(&self) -> &str {
        "db_query"
    }

    fn description(&self) -> &str {
        "Run a single read-only SQL statement (SELECT/WITH/EXPLAIN/SHOW/DESCRIBE) \
         against a configured database and return the result as CSV text. \
         Use this to inspect schema and data (SHOW TABLES / information_schema / \
         pragma table_info all work); no confirmation needed. The `database` \
         parameter must be one of the ids enumerated in the parameter schema — \
         never guess an id."
    }

    fn parameters(&self) -> Value {
        // 动态生成：可用库清单（id + 类型 + 地址 + 库名，无账号密码）实时读
        // catalog——设置页增删库后，下一个 turn 的工具 schema 即为最新。
        let allowed = self.allowed_databases.as_deref();
        let ids = self.catalog.ids(allowed);
        let summaries = self.catalog.summaries(allowed);
        let mut database = json!({
            "type": "string",
            "description": "Database id — must be one of the enum values; never guess.",
        });
        if ids.is_empty() {
            database["description"] = json!(
                "Database id — no database is available in this workspace yet; \
                 tell the user to add one in Settings first."
            );
        } else {
            database["description"] = json!(format!(
                "Database id — must be one of the enum values; never guess. \
                 Available: {}",
                summaries.join("; ")
            ));
            database["enum"] = json!(ids);
        }
        json!({
            "type": "object",
            "properties": {
                "database": database,
                "sql": {"type": "string", "description": "A single read-only SQL statement."}
            },
            "required": ["database", "sql"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn call(&self, input: Value) -> Result<ToolOutput> {
        self.run(input, None).await
    }

    async fn call_with_context(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        // 会话绑定收窄：agent loop 携带 session_id，按会话绑定的库收窄白名单。
        self.run(input, Some(&ctx.session_id)).await
    }
}

impl DbQueryTool {
    async fn run(&self, input: Value, session_id: Option<&str>) -> Result<ToolOutput> {
        let name = input
            .get("database")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let sql = input.get("sql").and_then(Value::as_str).unwrap_or_default();
        if name.is_empty() || sql.is_empty() {
            return Ok(ToolOutput::error("database and sql are required"));
        }
        let allowed = self.effective_allowed(session_id);
        if let Some(allowed) = &allowed
            && !allowed.iter().any(|n| n == name) {
                if allowed.is_empty() {
                    return Ok(ToolOutput::error(format!(
                        "database `{name}` is not available in this session: the session is bound to a database outside the workspace allowlist"
                    )));
                }
                return Ok(ToolOutput::error(format!(
                    "database `{name}` is not bound to this session (allowed: {allowed:?})"
                )));
            }
        if classify(sql) != "read" {
            return Ok(ToolOutput::error(
                "db_query 只接受只读语句（SELECT/WITH/EXPLAIN/SHOW/DESCRIBE）；变更请用 db_dml",
            ));
        }
        let db = match self.catalog.get(name) {
            Ok(d) => d,
            Err(e) => return Ok(ToolOutput::error(e)),
        };
        match execute(&db, sql).await {
            Ok(text) => Ok(ToolOutput::ok(text)),
            Err(e) => Ok(ToolOutput::error(e)),
        }
    }
}

/// `db_dml`：变更 SQL（INSERT/UPDATE/DELETE/DDL），ask 规则 → 每次确认。
pub struct DbDmlTool {
    catalog: Arc<DbCatalog>,
    /// 允许访问的数据库档案名白名单（语义同 [`DbQueryTool`]）。
    allowed_databases: Option<Vec<String>>,
    /// 会话状态目录（语义同 [`DbQueryTool`]）：按会话绑定收窄白名单。
    workspace_state_dir: Option<std::path::PathBuf>,
}

impl DbDmlTool {
    pub fn new(
        catalog: Arc<DbCatalog>,
        allowed_databases: Option<Vec<String>>,
        workspace_state_dir: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            catalog,
            allowed_databases,
            workspace_state_dir,
        }
    }

    /// 会话绑定的数据库档案名（未绑定/读取失败 → None）。
    fn session_binding(&self, session_id: &str) -> Option<String> {
        let dir = self.workspace_state_dir.as_ref()?;
        let bytes = std::fs::read(dir.join(session_id).join("binding.json")).ok()?;
        let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        value
            .get("database")?
            .as_str()
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
    }

    /// 本次调用实际可用的库名单（语义同 [`DbQueryTool::effective_allowed`]）。
    fn effective_allowed(&self, session_id: Option<&str>) -> Option<Vec<String>> {
        let bound = session_id.and_then(|sid| self.session_binding(sid));
        match (self.allowed_databases.clone(), bound) {
            (None, None) => None,
            (None, Some(b)) => Some(vec![b]),
            (Some(w), None) => Some(w),
            (Some(w), Some(b)) => Some(if w.contains(&b) { vec![b] } else { vec![] }),
        }
    }
}

#[async_trait::async_trait]
impl Tool for DbDmlTool {
    fn name(&self) -> &str {
        "db_dml"
    }

    fn description(&self) -> &str {
        "Run a single data-changing SQL statement (INSERT/UPDATE/DELETE/DDL) \
         against a configured database. Every execution requires user \
         confirmation — explain the affected tables and expected row count first. \
         The `database` parameter must be one of the ids enumerated in the \
         parameter schema — never guess an id."
    }

    fn parameters(&self) -> Value {
        // 动态生成（语义同 DbQueryTool::parameters）。
        let allowed = self.allowed_databases.as_deref();
        let ids = self.catalog.ids(allowed);
        let summaries = self.catalog.summaries(allowed);
        let mut database = json!({
            "type": "string",
            "description": "Database id — must be one of the enum values; never guess.",
        });
        if ids.is_empty() {
            database["description"] = json!(
                "Database id — no database is available in this workspace yet; \
                 tell the user to add one in Settings first."
            );
        } else {
            database["description"] = json!(format!(
                "Database id — must be one of the enum values; never guess. \
                 Available: {}",
                summaries.join("; ")
            ));
            database["enum"] = json!(ids);
        }
        json!({
            "type": "object",
            "properties": {
                "database": database,
                "sql": {"type": "string", "description": "A single data-changing SQL statement."}
            },
            "required": ["database", "sql"]
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput> {
        self.run(input, None).await
    }

    async fn call_with_context(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        // 会话绑定收窄：agent loop 携带 session_id，按会话绑定的库收窄白名单。
        self.run(input, Some(&ctx.session_id)).await
    }
}

impl DbDmlTool {
    async fn run(&self, input: Value, session_id: Option<&str>) -> Result<ToolOutput> {
        let name = input
            .get("database")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let sql = input.get("sql").and_then(Value::as_str).unwrap_or_default();
        if name.is_empty() || sql.is_empty() {
            return Ok(ToolOutput::error("database and sql are required"));
        }
        let allowed = self.effective_allowed(session_id);
        if let Some(allowed) = &allowed
            && !allowed.iter().any(|n| n == name) {
                if allowed.is_empty() {
                    return Ok(ToolOutput::error(format!(
                        "database `{name}` is not available in this session: the session is bound to a database outside the workspace allowlist"
                    )));
                }
                return Ok(ToolOutput::error(format!(
                    "database `{name}` is not bound to this session (allowed: {allowed:?})"
                )));
            }
        if classify(sql) == "read" {
            return Ok(ToolOutput::error(
                "db_dml 用于变更语句；只读查询请用 db_query（免确认）",
            ));
        }
        let db = match self.catalog.get(name) {
            Ok(d) => d,
            Err(e) => return Ok(ToolOutput::error(e)),
        };
        match execute(&db, sql).await {
            Ok(text) => Ok(ToolOutput::ok(text)),
            Err(e) => Ok(ToolOutput::error(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db_with_cli_path(cli_path: &str) -> DatabaseServerConfig {
        DatabaseServerConfig {
            name: "t".into(),
            db_type: "mysql".into(),
            host: "10.0.0.8".into(),
            port: 3306,
            database: "orders".into(),
            username: "u".into(),
            password: "p".into(),
            cli_path: cli_path.into(),
            description: String::new(),
        }
    }

    #[test]
    fn cli_path_empty_falls_back_to_discovery() {
        let got = resolve_cli_program(&db_with_cli_path(""), "mysql");
        assert!(!got.is_empty());
    }

    #[test]
    fn cli_path_file_used_directly() {
        let dir = std::env::temp_dir().join(format!("ox-cli-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("my-mysql.exe");
        std::fs::write(&file, b"").unwrap();
        let got = resolve_cli_program(&db_with_cli_path(file.to_str().unwrap()), "mysql");
        assert_eq!(got, file.to_str().unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cli_path_dir_joins_program_name() {
        let dir = std::env::temp_dir().join(format!("ox-cli-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let got = resolve_cli_program(&db_with_cli_path(dir.to_str().unwrap()), "mysql");
        assert_eq!(std::path::Path::new(&got), &dir.join(exe_name("mysql")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cli_path_missing_returned_as_is_for_spawn_error() {
        let got = resolve_cli_program(&db_with_cli_path(r"X:\no\such\dir"), "mysql");
        assert_eq!(got, r"X:\no\such\dir");
    }

    #[test]
    fn find_exe_under_locates_nested_binary() {
        // MySQL Server 式目录树：<root>\MySQL Server 8.0\bin\mysql.exe
        let root = std::env::temp_dir().join(format!("ox-probe-{}", std::process::id()));
        let bin = root.join("MySQL Server 8.0").join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let exe = bin.join(if cfg!(windows) { "mysql.exe" } else { "mysql" });
        std::fs::write(&exe, b"").unwrap();
        let got = find_exe_under(&root, &exe_name("mysql"), 4);
        assert_eq!(got.as_deref(), Some(exe.as_path()));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn find_exe_under_is_case_insensitive_on_file_name() {
        let root = std::env::temp_dir().join(format!("ox-probe-ci-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let exe = root.join(if cfg!(windows) { "MYSQL.EXE" } else { "mysql" });
        std::fs::write(&exe, b"").unwrap();
        let got = find_exe_under(&root, &exe_name("mysql"), 2);
        assert_eq!(got.as_deref(), Some(exe.as_path()));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn effective_allowed_narrows_by_session_binding() {
        let dir = std::env::temp_dir().join(format!("ox-bind-{}", std::process::id()));
        let sess = dir.join("sess1");
        std::fs::create_dir_all(&sess).unwrap();
        std::fs::write(sess.join("binding.json"), br#"{"database":"my"}"#).unwrap();
        let tool = DbQueryTool::new(
            Arc::new(DbCatalog::new()),
            Some(vec!["my".into(), "platform".into()]),
            Some(dir.clone()),
        );
        // 会话绑定 my → 白名单收窄到 my
        assert_eq!(
            tool.effective_allowed(Some("sess1")),
            Some(vec!["my".into()])
        );
        // 无绑定/未知会话 → 工作区白名单原样
        assert_eq!(
            tool.effective_allowed(Some("nope")),
            Some(vec!["my".into(), "platform".into()])
        );
        assert_eq!(
            tool.effective_allowed(None),
            Some(vec!["my".into(), "platform".into()])
        );
        // 绑定不在白名单内 → 空名单（全部拒绝）
        std::fs::write(sess.join("binding.json"), br#"{"database":"other"}"#).unwrap();
        assert_eq!(tool.effective_allowed(Some("sess1")), Some(Vec::<String>::new()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn probe_explicit_file_and_dir() {
        let dir = std::env::temp_dir().join(format!("ox-probe-exp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // 文件：直接用
        let file = dir.join("my-mysql.exe");
        std::fs::write(&file, b"").unwrap();
        assert_eq!(
            probe_cli("mysql", file.to_str().unwrap(), "mysql").unwrap(),
            file.to_str().unwrap()
        );
        // 目录：拼接程序名（目录里需真的有该程序）
        std::fs::write(dir.join(exe_name("mysql")), b"").unwrap();
        assert_eq!(
            probe_cli("mysql", dir.to_str().unwrap(), "mysql").unwrap(),
            dir.join(exe_name("mysql")).to_string_lossy()
        );
        // 目录里没有目标程序：报错带程序名
        let err = probe_cli("psql", dir.to_str().unwrap(), "psql").unwrap_err();
        assert!(err.contains("psql"), "err: {err}");
        // 路径不存在：报错带路径
        let err = probe_cli("mysql", r"X:\no\such", "mysql").unwrap_err();
        assert!(err.contains("不存在"), "err: {err}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
