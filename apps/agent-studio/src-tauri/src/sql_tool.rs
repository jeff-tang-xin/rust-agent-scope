//! `db` 模式的只读 SQL 工具（对齐 `db` 子智能体的「只读 SQL 查询、严谨克制」语义）。
//!
//! 安全边界：
//! - 仅接受单条 `SELECT` / `WITH` / `EXPLAIN` / `PRAGMA` 语句（词法级白名单 +
//!   禁止多语句）；
//! - 连接以只读打开（`OpenFlags::SQLITE_OPEN_READ_ONLY`），即使白名单被绕过，
//!   文件层面也写不进去；
//! - 行数上限 200，超限截断并注明（防上下文打爆）。

use std::sync::Arc;

use agentscope_core::error::Result;
use agentscope_core::tool::{Tool, ToolOutput};
use serde_json::{json, Value};

/// 单文件 SQLite 数据库的默认位置（agent workspace 相对）。
pub const DEFAULT_DB_FILE: &str = "studio.db";
/// 单次查询返回的最大行数。
pub const MAX_ROWS: usize = 200;

/// 只读 SQL 查询工具。每个 agent 实例绑定自己的 db 文件（workspace 内隔离）。
pub struct SqlQueryTool {
    db_path: Arc<std::path::PathBuf>,
}

impl SqlQueryTool {
    pub fn new(db_path: std::path::PathBuf) -> Self {
        Self {
            db_path: Arc::new(db_path),
        }
    }

    /// 语句白名单校验：单条 + 只读前缀。
    fn validate(sql: &str) -> std::result::Result<(), String> {
        let trimmed = sql.trim();
        if trimmed.is_empty() {
            return Err("sql is empty".into());
        }
        // 禁多语句：简单分号检查（字符串字面量里的分号极少见于只读查询，
        // 保守处理——出现即拒绝）。
        if trimmed.trim_end_matches(';').contains(';') {
            return Err("multiple statements are not allowed".into());
        }
        let head = trimmed
            .trim_start_matches('(')
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let allowed = ["select", "with", "explain", "pragma"];
        if !allowed.contains(&head.as_str()) {
            return Err(format!(
                "only read-only statements are allowed (select/with/explain/pragma), got `{head}`"
            ));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Tool for SqlQueryTool {
    fn name(&self) -> &str {
        "sql_query"
    }

    fn description(&self) -> &str {
        "Run a single read-only SQL statement (SELECT/WITH/EXPLAIN/PRAGMA) against the \
         workspace SQLite database and return the rows as a JSON array. Multiple \
         statements and any write action are rejected."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "sql": {
                    "type": "string",
                    "description": "A single read-only SQL statement."
                }
            },
            "required": ["sql"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn call(&self, input: Value) -> Result<ToolOutput> {
        let Some(sql) = input.get("sql").and_then(Value::as_str) else {
            return Ok(ToolOutput::error("missing required parameter `sql`"));
        };
        if let Err(reason) = Self::validate(sql) {
            return Ok(ToolOutput::error(reason));
        }
        // rusqlite 是阻塞调用：丢到阻塞线程池，避免卡住 async 执行器。
        let db_path = self.db_path.clone();
        let sql = sql.to_string();
        let result =
            tokio::task::spawn_blocking(move || execute_readonly(&db_path, &sql)).await;
        match result {
            Ok(Ok(payload)) => Ok(ToolOutput::ok(payload)),
            Ok(Err(reason)) => Ok(ToolOutput::error(reason)),
            Err(join) => Ok(ToolOutput::error(format!("query task failed: {join}"))),
        }
    }
}

/// 阻塞侧执行：只读打开 + 行数上限。
fn execute_readonly(db_path: &std::path::Path, sql: &str) -> std::result::Result<String, String> {
    if !db_path.exists() {
        return Err(format!(
            "database file not found: {}. Create it first (e.g. by asking the ops agent to run a migration script).",
            db_path.display()
        ));
    }
    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
        | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = rusqlite::Connection::open_with_flags(db_path, flags)
        .map_err(|e| format!("open database: {e}"))?;
    let mut stmt = conn.prepare(sql).map_err(|e| format!("prepare: {e}"))?;
    let col_names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("execute: {e}"))?;

    let mut out_rows: Vec<Value> = Vec::new();
    let mut truncated = false;
    loop {
        match rows.next() {
            Ok(Some(row)) => {
                if out_rows.len() >= MAX_ROWS {
                    truncated = true;
                    break;
                }
                let mut obj = serde_json::Map::new();
                for (col, name) in col_names.iter().enumerate() {
                    let value = match row.get_ref(col) {
                        Ok(rusqlite::types::ValueRef::Null) => Value::Null,
                        Ok(rusqlite::types::ValueRef::Integer(v)) => json!(v),
                        Ok(rusqlite::types::ValueRef::Real(v)) => json!(v),
                        Ok(rusqlite::types::ValueRef::Text(t)) => {
                            json!(String::from_utf8_lossy(t).to_string())
                        }
                        Ok(rusqlite::types::ValueRef::Blob(b)) => {
                            json!(format!("<blob {} bytes>", b.len()))
                        }
                        Err(e) => json!(format!("<error: {e}>")),
                    };
                    obj.insert(name.clone(), value);
                }
                out_rows.push(Value::Object(obj));
            }
            Ok(None) => break,
            Err(e) => return Err(format!("fetch: {e}")),
        }
    }

    let mut payload = json!({
        "columns": col_names,
        "rows": out_rows,
        "rowCount": out_rows.len(),
    });
    if truncated {
        payload["truncated"] = json!(format!(
            "result truncated at {MAX_ROWS} rows; refine the query (LIMIT / WHERE) for more"
        ));
    }
    serde_json::to_string_pretty(&payload).map_err(|e| format!("serialize result: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_writes_and_multi_statements() {
        assert!(SqlQueryTool::validate("SELECT 1").is_ok());
        assert!(SqlQueryTool::validate("  WITH t AS (SELECT 1) SELECT * FROM t").is_ok());
        assert!(SqlQueryTool::validate("EXPLAIN SELECT 1").is_ok());
        assert!(SqlQueryTool::validate("PRAGMA table_info(t)").is_ok());
        assert!(SqlQueryTool::validate("DROP TABLE users").is_err());
        assert!(SqlQueryTool::validate("INSERT INTO t VALUES (1)").is_err());
        assert!(SqlQueryTool::validate("UPDATE t SET a=1").is_err());
        assert!(SqlQueryTool::validate("SELECT 1; DROP TABLE users").is_err());
        assert!(SqlQueryTool::validate("").is_err());
    }

    #[tokio::test]
    async fn query_against_missing_db_is_tool_error() {
        let tool = SqlQueryTool::new(std::env::temp_dir().join("studio-missing.db"));
        let out = tool
            .call(json!({"sql": "SELECT 1"}))
            .await
            .expect("tool call itself succeeds");
        assert_eq!(out.state, agentscope_core::message::ToolResultState::Error);
    }

    #[tokio::test]
    async fn query_returns_rows_as_json() {
        let dir = std::env::temp_dir().join(format!("studio-db-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("test.db");
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch("CREATE TABLE t(id INTEGER, name TEXT); INSERT INTO t VALUES (1, 'a'), (2, 'b');").unwrap();
        }
        let tool = SqlQueryTool::new(db_path);
        let out = tool.call(json!({"sql": "SELECT * FROM t ORDER BY id"})).await.unwrap();
        assert_eq!(out.state, agentscope_core::message::ToolResultState::Success);
        assert!(out.content.contains("\"name\": \"a\""));
        // 写语句被拒。
        let denied = tool.call(json!({"sql": "DELETE FROM t"})).await.unwrap();
        assert_eq!(denied.state, agentscope_core::message::ToolResultState::Error);
        std::fs::remove_dir_all(&dir).ok();
    }
}
