//! 把数据库面板的连接暴露为 Agent 工具（`db_query` 只读免确认 / `db_dml` 走人工确认）。
//!
//! 面板连接（`Project.db`）已含连接池、SSH 隧道与密码管理——Agent 直接复用这些
//! 连接执行 SQL，不重复配置、不接触明文密码；面板断开/改配置对 Agent 立即生效。
//! 工具经 `OxHostContext` 注入 agent 的 Toolkit（与 IDE 语义工具同一条进程内通道，
//! `assemble_agent` 无需感知）。只读口径与框架级 `agentscope-db` 的 `classify`
//! 一致：SELECT/WITH/EXPLAIN/SHOW/DESC/PRAGMA 才算读。

use std::path::Path;
use std::sync::Arc;

use agentscope_core::{Tool, ToolOutput};
use async_trait::async_trait;
use ide_core::CancellationToken;
use ide_db::{DataSource, DbDriver, DbManager};
use serde_json::{json, Value};

use crate::Host;

/// 每条查询最多返回的行数（超出截断并提示）。
const MAX_ROWS: usize = 200;

/// 按任务工作目录挑项目（worktree 是项目目录的子目录，取包含 `cwd` 的最深项目）。
fn project_for(host: &Host, cwd: &Path) -> Option<Arc<crate::Project>> {
    host.projects()
        .into_iter()
        .filter(|p| !p.light && cwd.starts_with(p.root.as_path()))
        .max_by_key(|p| p.root.components().count())
}

/// 连接的一句话描述（进工具描述，模型据此选择 `connection` 参数）。
fn describe(source: &DataSource) -> String {
    let driver = match source.driver {
        DbDriver::Sqlite => "sqlite".to_owned(),
        DbDriver::Postgres => "postgres".to_owned(),
        DbDriver::Mysql => "mysql".to_owned(),
        DbDriver::SqlServer => "sqlserver".to_owned(),
        DbDriver::Clickhouse => "clickhouse".to_owned(),
        DbDriver::Oracle => "oracle".to_owned(),
    };
    let target = if source.driver == DbDriver::Sqlite {
        source.file.as_ref().map(|f| f.as_os_str().to_string_lossy().into_owned()).unwrap_or_else(|| source.database.clone())
    } else {
        format!(
            "{}@{}:{}",
            source.database,
            source.host,
            source.port.map(|p| p.to_string()).unwrap_or_else(|| "default".to_owned())
        )
    };
    format!("{} — {} {target}", source.name, driver)
}

/// 从入参解析目标连接：缺省时唯一连接直接用，多连接要求显式指定（按 name 或 id）。
fn resolve<'a>(sources: &'a [DataSource], arg: Option<&str>) -> Result<&'a DataSource, String> {
    match (arg, sources.len()) {
        (None, 1) => Ok(&sources[0]),
        (None, 0) => Err("no data source configured in the database panel".to_owned()),
        (None, _) => Err(format!(
            "multiple connections available, pass `connection`: {}",
            sources.iter().map(describe).collect::<Vec<_>>().join("; ")
        )),
        (Some(given), _) => sources
            .iter()
            .find(|s| s.id == given || s.name == given)
            .ok_or_else(|| format!(
                "unknown connection `{given}`; available: {}",
                sources.iter().map(describe).collect::<Vec<_>>().join("; ")
            )),
    }
}

/// 拒绝多语句（一次一条，与 agentscope-db CLI 口径一致）。
fn single_statement(sql: &str) -> Result<(), String> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        return Err("empty statement".to_owned());
    }
    // 粗粒度检查：字符串字面量外的分号视为多语句分隔（引号内的分号合法）。
    let mut in_single = false;
    let mut in_double = false;
    for ch in trimmed.chars() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            ';' if !in_single && !in_double => {
                return Err("multiple statements are not allowed; run one statement at a time".to_owned());
            }
            _ => {}
        }
    }
    Ok(())
}

/// `QueryResult` → 模型可读的 markdown 表格。
fn render(result: &ide_db::QueryResult) -> String {
    let mut out = String::new();
    if !result.columns.is_empty() {
        out.push_str("| ");
        out.push_str(&result.columns.iter().map(|c| c.name.replace('|', "\\|")).collect::<Vec<_>>().join(" | "));
        out.push_str(" |\n|");
        out.push_str(&" --- |".repeat(result.columns.len()));
        out.push('\n');
        for row in &result.rows {
            out.push_str("| ");
            out.push_str(
                &row.iter()
                    .map(|c| c.as_deref().unwrap_or("NULL").replace('|', "\\|"))
                    .collect::<Vec<_>>()
                    .join(" | "),
            );
            out.push_str(" |\n");
        }
    }
    let mut notes = Vec::new();
    match (result.rows.len(), result.affected) {
        (0, Some(n)) => notes.push(format!("{n} row(s) affected")),
        (n, Some(a)) => notes.push(format!("{n} row(s) returned, {a} affected")),
        (n, None) => notes.push(format!("{n} row(s) returned")),
    }
    if result.has_more {
        notes.push(format!("truncated at {MAX_ROWS} rows"));
    }
    notes.push(format!("elapsed {:.1}ms", result.execution_ms));
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(&notes.join(", "));
    out
}

/// 把面板错误翻译成模型能自救的提示（密码缺失 → 去面板连接/存密码）。
fn tool_error(e: ide_core::IdeError) -> ToolOutput {
    let hint = if e.kind == ide_core::error::ErrorKind::PermissionDenied {
        " (connection needs a password: connect it once in the database panel, or save the password there)"
    } else {
        ""
    };
    ToolOutput::error(format!("{}{hint}", e.message))
}

/// 共享状态：项目 db 管理器 + 连接清单快照（构造时取，描述随之固定）。
struct AgentDbTool {
    db: Arc<DbManager>,
    sources: Vec<DataSource>,
    name: &'static str,
    description: String,
    read_only: bool,
}

impl AgentDbTool {
    fn parameters() -> Value {
        json!({
            "type": "object",
            "properties": {
                "sql": { "type": "string", "description": "One SQL statement (no multiple statements)." },
                "connection": { "type": "string", "description": "Connection name or id; only needed when several connections exist." }
            },
            "required": ["sql"]
        })
    }

    async fn run(&self, input: Value) -> Result<ToolOutput, agentscope_core::Error> {
        let sql = input.get("sql").and_then(Value::as_str).unwrap_or("").to_owned();
        if sql.is_empty() {
            return Ok(ToolOutput::error("missing `sql` parameter"));
        }
        if let Err(e) = single_statement(&sql) {
            return Ok(ToolOutput::error(e));
        }
        if self.read_only && agentscope_db::classify(&sql) != "read" {
            return Ok(ToolOutput::error(
                "db_query only runs read statements (SELECT/WITH/EXPLAIN/SHOW/DESC/PRAGMA); use db_dml for changes",
            ));
        }
        let connection = input.get("connection").and_then(Value::as_str);
        let source = match resolve(&self.sources, connection) {
            Ok(s) => s,
            Err(e) => return Ok(ToolOutput::error(e)),
        };
        match self
            .db
            .execute_from(&source.id, &sql, 0, MAX_ROWS, &CancellationToken::new(), None)
            .await
        {
            Ok(result) => Ok(ToolOutput::ok(render(&result))),
            Err(e) => Ok(tool_error(e)),
        }
    }
}

#[async_trait]
impl Tool for AgentDbTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters(&self) -> Value {
        Self::parameters()
    }
    fn is_read_only(&self) -> bool {
        self.read_only
    }
    async fn call(&self, input: Value) -> Result<ToolOutput, agentscope_core::Error> {
        self.run(input).await
    }
}

/// 面板连接 → Agent 工具：有连接才注册（`db_query` + `db_dml` 各一个，连接清单
/// 写进描述，多连接用 `connection` 参数区分）。无项目或无连接返回空。
pub fn db_tools(host: Arc<Host>, cwd: &Path) -> Vec<Arc<dyn Tool>> {
    let Some(project) = project_for(&host, cwd) else {
        return Vec::new();
    };
    let Ok(sources) = project.db.sources() else {
        return Vec::new();
    };
    if sources.is_empty() {
        return Vec::new();
    }
    let listing = sources.iter().map(describe).collect::<Vec<_>>().join("; ");
    let db = project.db.clone();
    vec![
        Arc::new(AgentDbTool {
            db: db.clone(),
            sources: sources.clone(),
            name: "db_query",
            description: format!(
                "Run one read-only SQL statement (SELECT/WITH/EXPLAIN/SHOW/DESC/PRAGMA) on the IDE database panel connections. No confirmation needed. Connections: {listing}"
            ),
            read_only: true,
        }),
        Arc::new(AgentDbTool {
            db,
            sources,
            name: "db_dml",
            description: format!(
                "Run one SQL change (INSERT/UPDATE/DELETE/DDL) on the IDE database panel connections. Requires user confirmation. Connections: {listing}"
            ),
            read_only: false,
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_statement_allows_semicolons_inside_strings() {
        assert!(single_statement("SELECT ';'").is_ok());
        assert!(single_statement("SELECT \"a;b\"").is_ok());
        assert!(single_statement("SELECT 1;").is_ok());
        assert!(single_statement("SELECT 1; SELECT 2").is_err());
        assert!(single_statement("   ").is_err());
    }

    #[test]
    fn resolve_picks_single_or_matches_by_name_or_id() {
        let mk = |id: &str, name: &str| DataSource { id: id.into(), name: name.into(), ..Default::default() };
        let one = vec![mk("a", "Local")];
        assert!(resolve(&one, None).is_ok());
        assert!(resolve(&one, Some("Local")).is_ok());
        assert!(resolve(&one, Some("nope")).is_err());
        let two = vec![mk("a", "Local"), mk("b", "Remote")];
        assert!(resolve(&two, None).is_err());
        assert!(resolve(&two, Some("Remote")).is_ok());
        assert!(resolve(&two, Some("b")).is_ok());
    }

    #[test]
    fn render_handles_nulls_and_pipes() {
        let result = ide_db::QueryResult {
            statement: String::new(),
            columns: vec![
                ide_db::ResultColumn { name: "a|b".into(), type_name: String::new() },
                ide_db::ResultColumn { name: "c".into(), type_name: String::new() },
            ],
            rows: vec![vec![Some("x".into()), None]],
            affected: None,
            has_more: false,
            elapsed_ms: 1.0,
            execution_ms: 1.0,
        };
        let text = render(&result);
        assert!(text.contains("a\\|b"));
        assert!(text.contains("NULL"));
        assert!(text.contains("1 row(s) returned"));
    }
}
