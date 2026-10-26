// db 工作区右侧数据库面板：数据库列表 → 连接 → 选库/schema → 表列表 +
// 查询模式（SQL 直接执行：只读免确认，DML 弹窗确认）。AI 模式 = 左侧会话
// （db_query 免确认 / db_dml 走审批卡）。
import { useCallback, useEffect, useState } from "react";
import { api, type DatabaseServerConfig } from "../api";

/** 元数据查询（按 db_type 拼只读 SQL，走 db_execute）。 */
const META_SQL = {
  // 库/schema 列表（sqlite 无此层，返回空）。
  catalogs: (t: string) => {
    switch (t) {
      case "postgres":
        return "SELECT schema_name AS name FROM information_schema.schemata WHERE schema_name NOT IN ('pg_catalog','information_schema','pg_toast') ORDER BY 1";
      case "mysql":
        return "SHOW DATABASES";
      case "sqlserver":
        return "SELECT name FROM sys.databases WHERE name NOT IN ('master','tempdb','model','msdb') ORDER BY 1";
      default:
        return "";
    }
  },
  // 表列表（catalog = 库或 schema；sqlite 用 sqlite_master）。
  tables: (t: string, catalog: string) => {
    switch (t) {
      case "postgres":
        return `SELECT table_name AS name FROM information_schema.tables WHERE table_schema = '${catalog.replace(/'/g, "''")}' AND table_type = 'BASE TABLE' ORDER BY 1`;
      case "mysql":
        return `SELECT table_name AS name FROM information_schema.tables WHERE table_schema = '${catalog.replace(/'/g, "''")}' AND table_type = 'BASE TABLE' ORDER BY 1`;
      case "sqlserver":
        return `SELECT name FROM sys.tables ORDER BY 1`;
      case "sqlite":
        return "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY 1";
      default:
        return "";
    }
  },
};

/** CSV 文本 → { header, rows }（与后端 to_csv 同转义规则）。 */
function parseCsv(text: string): { header: string[]; rows: string[][] } {
  const lines: string[][] = [];
  let cur: string[] = [];
  let field = "";
  let inQ = false;
  for (let i = 0; i < text.length; i++) {
    const ch = text[i];
    if (inQ) {
      if (ch === '"') {
        if (text[i + 1] === '"') {
          field += '"';
          i++;
        } else inQ = false;
      } else field += ch;
    } else if (ch === '"') inQ = true;
    else if (ch === ",") {
      cur.push(field);
      field = "";
    } else if (ch === "\n" || ch === "\r") {
      if (ch === "\r" && text[i + 1] === "\n") i++;
      cur.push(field);
      field = "";
      if (cur.length > 1 || cur[0] !== "") lines.push(cur);
      cur = [];
    } else field += ch;
  }
  if (field !== "" || cur.length > 0) {
    cur.push(field);
    if (cur.length > 1 || cur[0] !== "") lines.push(cur);
  }
  if (lines.length === 0) return { header: [], rows: [] };
  return { header: lines[0], rows: lines.slice(1) };
}

/** 语句分类（与后端 classify 同口径；前端用于决定是否弹确认）。 */
function isWriteSql(sql: string): boolean {
  const head = sql
    .trim()
    .replace(/^\(+/, "")
    .split(/\s+/)[0]
    ?.toLowerCase();
  return [
    "insert", "update", "delete", "replace", "merge", "create", "drop",
    "alter", "truncate", "grant", "revoke", "begin", "commit", "rollback", "call",
  ].includes(head ?? "");
}

export default function DatabasesPanel({
  onCollapse,
  boundDatabases,
}: {
  onCollapse: () => void;
  /** 工作区绑定的数据库白名单；非空时只显示绑定的（能力收窄的可视化）。 */
  boundDatabases?: string[];
}) {
  const [databases, setDatabases] = useState<DatabaseServerConfig[]>([]);
  const [active, setActive] = useState(""); // 当前展开的数据库
  const [error, setError] = useState("");

  const reload = useCallback(async () => {
    try {
      const all = await api.listDatabases();
      // 绑定白名单非空 → 只显示绑定的数据库（与工具面能力一致）。
      setDatabases(
        boundDatabases?.length
          ? all.filter((d) => boundDatabases.includes(d.name))
          : all,
      );
    } catch (e) {
      setError(String(e));
    }
  }, [boundDatabases]);

  useEffect(() => {
    void reload();
  }, [reload]);

  return (
    <aside className="file-panel">
      <div className="file-panel-head">
        <span>数据库</span>
        <span className="spacer" />
        <button className="icon-btn" title="刷新" onClick={() => void reload()}>
          ⟳
        </button>
        <button className="icon-btn" title="收起面板（AI 模式）" onClick={onCollapse}>
          »
        </button>
      </div>
      {error && <div className="file-panel-error">{error}</div>}
      {databases.length === 0 ? (
        <div className="file-empty">暂无数据库，请在设置页添加数据库连接。</div>
      ) : (
        <div className="server-list">
          {databases.map((d) => (
            <DatabaseItem
              key={d.name}
              database={d}
              expanded={active === d.name}
              onToggle={() => setActive(active === d.name ? "" : d.name)}
            />
          ))}
        </div>
      )}
    </aside>
  );
}

/** 单个数据库：头部 + 展开后的（库/schema → 表）+ 查询模式。 */
function DatabaseItem({
  database,
  expanded,
  onToggle,
}: {
  database: DatabaseServerConfig;
  expanded: boolean;
  onToggle: () => void;
}) {
  return (
    <div className="server-item">
      <div className="server-head" onClick={onToggle}>
        <span className="dot on" />
        <span className="server-name" title={database.description || database.name}>
          {database.name}
        </span>
        <span className="server-host">
          {database.db_type}
          {database.database ? ` · ${database.database}` : ""}
        </span>
      </div>
      {expanded && <DatabaseBrowser database={database} />}
    </div>
  );
}

/** 连接浏览：sqlite 直接列表；网络库先列库/schema（点击选择）再列表。 */
function DatabaseBrowser({ database }: { database: DatabaseServerConfig }) {
  const [catalog, setCatalog] = useState(""); // 选中的库/schema（sqlite 为空）
  const [catalogs, setCatalogs] = useState<string[]>([]);
  const [tables, setTables] = useState<string[]>([]);
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);

  const runMeta = useCallback(
    async (sql: string): Promise<string[]> => {
      const text = await api.dbExecute(database.name, sql);
      const { header, rows } = parseCsv(text);
      const col = header.findIndex((h) => h.toLowerCase().includes("name") || h.toLowerCase().includes("database"));
      const idx = col >= 0 ? col : 0;
      return rows.map((r) => r[idx]).filter(Boolean);
    },
    [database.name],
  );

  useEffect(() => {
    let cancelled = false;
    setError("");
    setLoading(true);
    (async () => {
      try {
        if (database.db_type === "sqlite") {
          setCatalogs([]);
          setCatalog("");
          setTables(await runMeta(META_SQL.tables("sqlite", "")));
        } else {
          const sql = META_SQL.catalogs(database.db_type);
          if (!sql) throw new Error(`类型 ${database.db_type} 暂不支持在线浏览`);
          const list = await runMeta(sql);
          if (cancelled) return;
          setCatalogs(list);
          // 默认选中第一个（mysql 常见为 information_schema——优先选配置库）。
          const preferred = list.includes(database.database) ? database.database : list[0] ?? "";
          setCatalog(preferred);
        }
      } catch (e) {
        if (!cancelled) setError(String(e));
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [database, runMeta]);

  // catalog 变化 → 拉表列表。
  useEffect(() => {
    if (!catalog || database.db_type === "sqlite") return;
    let cancelled = false;
    setError("");
    setLoading(true);
    (async () => {
      try {
        const list = await runMeta(META_SQL.tables(database.db_type, catalog));
        if (!cancelled) setTables(list);
      } catch (e) {
        if (!cancelled) setError(String(e));
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [catalog, database, runMeta]);

  return (
    <div className="db-browser">
      {error && <div className="file-panel-error">{error}</div>}
      {loading && <div className="file-empty">连接中…</div>}
      {catalogs.length > 0 && (
        <div className="db-catalogs">
          <div className="db-section-title">库 / Schema</div>
          <div className="db-chips">
            {catalogs.map((c) => (
              <button
                key={c}
                className={`chip ${c === catalog ? "active" : ""}`}
                onClick={() => setCatalog(c)}
              >
                {c}
              </button>
            ))}
          </div>
        </div>
      )}
      {tables.length > 0 && (
        <div className="db-tables">
          <div className="db-section-title">表（{tables.length}）</div>
          <div className="db-table-list">
            {tables.map((t) => (
              <div key={t} className="file-row" style={{ paddingLeft: 8 }}>
                <span className="file-icon">▤</span>
                <span className="file-name" title={t}>
                  {t}
                </span>
              </div>
            ))}
          </div>
        </div>
      )}
      <QueryConsole database={database} catalog={catalog} />
    </div>
  );
}

/** 查询模式：SQL 输入 + 执行 + 结果表格。只读直接执行；DML 弹窗确认。 */
function QueryConsole({
  database,
  catalog,
}: {
  database: DatabaseServerConfig;
  catalog: string;
}) {
  const [sql, setSql] = useState("");
  const [result, setResult] = useState<{ header: string[]; rows: string[][]; text: string } | null>(
    null,
  );
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  const run = async () => {
    if (!sql.trim() || busy) return;
    if (isWriteSql(sql)) {
      const target = catalog || database.database || database.name;
      const ok = window.confirm(
        `即将在 ${database.name}${target ? `（${target}）` : ""} 执行变更语句：\n\n${sql}\n\n确认执行？`,
      );
      if (!ok) return;
    }
    setBusy(true);
    setError("");
    try {
      const text = await api.dbExecute(database.name, sql);
      const parsed = parseCsv(text);
      setResult({ ...parsed, text });
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="query-console">
      <div className="db-section-title">查询模式</div>
      <textarea
        className="query-input mono"
        rows={3}
        value={sql}
        placeholder={
          "SELECT * FROM ... （只读直接执行）\nINSERT/UPDATE/DELETE ... （执行前确认）"
        }
        onChange={(e) => setSql(e.target.value)}
        onKeyDown={(e) => {
          if ((e.ctrlKey || e.metaKey) && e.key === "Enter") void run();
        }}
      />
      <div className="query-actions">
        <span className="hint">Ctrl+Enter 执行</span>
        <span className="spacer" />
        <button className="btn" onClick={() => void run()} disabled={busy || !sql.trim()}>
          {busy ? "执行中…" : "执行"}
        </button>
      </div>
      {error && <div className="file-panel-error">{error}</div>}
      {result && (
        <div className="query-result">
          {result.header.length > 0 ? (
            <table className="result-table">
              <thead>
                <tr>
                  {result.header.map((h, i) => (
                    <th key={i}>{h}</th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {result.rows.slice(0, 200).map((r, i) => (
                  <tr key={i}>
                    {r.map((c, j) => (
                      <td key={j}>{c}</td>
                    ))}
                  </tr>
                ))}
              </tbody>
            </table>
          ) : (
            <pre className="mono">{result.text}</pre>
          )}
          {result.rows.length > 200 && (
            <div className="hint">仅显示前 200 行（共 {result.rows.length} 行）</div>
          )}
        </div>
      )}
    </div>
  );
}
