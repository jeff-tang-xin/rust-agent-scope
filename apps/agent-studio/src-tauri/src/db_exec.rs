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
use agentscope_core::tool::{Tool, ToolOutput};
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
        self.map
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
            .ok_or_else(|| format!("数据库 `{name}` 不存在，请先在设置页添加数据库连接"))
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

/// 网络数据库：拼 CLI 命令执行（密码经环境变量/参数传递，仅本机进程可见）。
async fn execute_cli(
    db: &DatabaseServerConfig,
    sql: &str,
) -> std::result::Result<String, String> {
    let (mut cmd, envs): (tokio::process::Command, Vec<(String, String)>) =
        match db.db_type.as_str() {
            "postgres" => {
                let mut c = tokio::process::Command::new("psql");
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
                (c, vec![("PGPASSWORD".into(), db.password.clone())])
            }
            "mysql" => {
                let mut c = tokio::process::Command::new("mysql");
                c.args([
                    "--batch",
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
                let mut c = tokio::process::Command::new("sqlcmd");
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

    let child = cmd
        .spawn()
        .map_err(|e| {
            format!(
                "启动 {} 失败（{}）：请确认已安装该 CLI 并加入 PATH",
                db.db_type, e
            )
        })?;
    let output = tokio::time::timeout(CLI_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| "执行超时（30s）".to_string())?
        .map_err(|e| format!("wait failed: {e}"))?;

    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    let err = String::from_utf8_lossy(&output.stderr);
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
}

impl DbQueryTool {
    pub fn new(catalog: Arc<DbCatalog>, allowed_databases: Option<Vec<String>>) -> Self {
        Self {
            catalog,
            allowed_databases,
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
         Use this to inspect schema and data; no confirmation needed."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "database": {"type": "string", "description": "Database connection name (from the databases panel)."},
                "sql": {"type": "string", "description": "A single read-only SQL statement."}
            },
            "required": ["database", "sql"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn call(&self, input: Value) -> Result<ToolOutput> {
        let name = input
            .get("database")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let sql = input.get("sql").and_then(Value::as_str).unwrap_or_default();
        if name.is_empty() || sql.is_empty() {
            return Ok(ToolOutput::error("database and sql are required"));
        }
        if let Some(allowed) = &self.allowed_databases
            && !allowed.iter().any(|n| n == name) {
                return Ok(ToolOutput::error(format!(
                    "database `{name}` is not bound to this workspace (bound: {allowed:?})"
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
}

impl DbDmlTool {
    pub fn new(catalog: Arc<DbCatalog>, allowed_databases: Option<Vec<String>>) -> Self {
        Self {
            catalog,
            allowed_databases,
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
         confirmation — explain the affected tables and expected row count first."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "database": {"type": "string", "description": "Database connection name (from the databases panel)."},
                "sql": {"type": "string", "description": "A single data-changing SQL statement."}
            },
            "required": ["database", "sql"]
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput> {
        let name = input
            .get("database")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let sql = input.get("sql").and_then(Value::as_str).unwrap_or_default();
        if name.is_empty() || sql.is_empty() {
            return Ok(ToolOutput::error("database and sql are required"));
        }
        if let Some(allowed) = &self.allowed_databases
            && !allowed.iter().any(|n| n == name) {
                return Ok(ToolOutput::error(format!(
                    "database `{name}` is not bound to this workspace (bound: {allowed:?})"
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
