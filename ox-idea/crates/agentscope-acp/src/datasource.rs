//! 项目数据源探测：扫项目根的常见配置文件，发现已配置的数据库。
//!
//! 设计约束（用户定案）：**不新开数据库能力**——不引任何数据库驱动，探测
//! 结果只注入系统提示告知 LLM「本项目有哪些库、怎么连」，执行一律走已有的
//! shell 工具（psql / mysql / sqlite3 / docker exec 等命令行客户端）。
//! 没探测到就不注入、不提——工具面与提示词保持干净。
//!
//! 探测是启发式的行扫描（不做完整 YAML/properties 解析，零新依赖）：
//! - `docker-compose*.yml` / `compose*.yml`：db 服务镜像（postgres/mysql/
//!   mariadb/mongo/redis）+ 端口映射；
//! - `.env*`：DATABASE_URL / DB_HOST / DB_PORT / DB_NAME / POSTGRES_* / MYSQL_*；
//! - `application*.yml|properties`（根、`config/`、`src/main/resources/`）：
//!   Spring `jdbc:` 连接串；
//! - `schema.prisma`：datasource provider + url；
//! - `settings.py`：Django `DATABASES` ENGINE；
//! - `database.yml`：Rails adapter；
//! - 根目录 `*.db|*.sqlite|*.sqlite3`：SQLite 文件。

use std::fs;
use std::path::Path;

use agentscope_db::DatabaseServerConfig;

/// 单个探测到的数据源：`kind` 是数据库类型，`detail` 是连接要点，
/// `hint` 是给 LLM 的建议验证命令（走 shell 工具执行）；
/// `config` 是提取出的连接档案（能拿到完整连接信息时才有——有档案的库
/// 注册 `db_query`/`db_dml` 工具，没有的保持走 shell 客户端）。
#[derive(Clone)]
pub struct Datasource {
    kind: &'static str,
    detail: String,
    hint: String,
    config: Option<DatabaseServerConfig>,
}

/// 单文件读取上限：配置文件不该更大，超了按截断处理（探测是启发式的）。
const MAX_FILE: u64 = 64 * 1024;
/// 注入提示的数据源条数上限：再多说明探测误报，宁缺毋滥。
const MAX_ITEMS: usize = 8;

/// 探测 `root` 项目下的数据源，一次扫描同时返回提示词数据与连接档案。
pub fn probe_full(root: &Path) -> (Vec<Datasource>, Vec<DatabaseServerConfig>) {
    let found = probe(root);
    let mut names: Vec<String> = Vec::new();
    let configs = found
        .iter()
        .filter_map(|ds| ds.config.clone())
        .map(|mut cfg| {
            let base = cfg.name.clone();
            let mut n = 2;
            while names.iter().any(|x| x == &cfg.name) {
                cfg.name = format!("{base}-{n}");
                n += 1;
            }
            names.push(cfg.name.clone());
            cfg
        })
        .collect();
    (found, configs)
}

/// 探测 `root` 项目下的数据源；渲染成系统提示节。没有发现返回 `None`
/// （调用方不注入、不提数据库）。
pub fn probe_section(root: &Path) -> Option<String> {
    let found = probe_full(root).0;
    if found.is_empty() {
        None
    } else {
        Some(render_section(&found))
    }
}

/// 渲染数据源提示词节（`probe_full` 的调用方在注册完工具后用它拼提示词）。
pub fn render_section(found: &[Datasource]) -> String {
    let mut section = String::from("\n## 本项目检测到的数据源\n\
         - 以下数据库来自项目配置（未验证连通性）。标注了「工具 id」的库已注册 \
         db_query（只读，免确认）/ db_dml（变更，每次需人工确认）工具，优先用工具；\
         其余连接与查询一律用 shell 工具跑命令行客户端，\
         不要尝试用文件工具读数据库文件（SQLite 除外，sqlite3 可直接打开）。\n\
         - 凭证在配置文件/环境变量里，不要把密码写进回复或日志；只读查询优先，\
         写操作先向用户确认。\n");
    for ds in found {
        if let Some(cfg) = &ds.config {
            section.push_str(&format!(
                "- **{}** — {}（工具 id：`{}`；用 db_query/db_dml，database 参数填此 id）\n",
                ds.kind, ds.detail, cfg.name
            ));
        } else {
            section.push_str(&format!("- **{}** — {}（验证：`{}`）\n", ds.kind, ds.detail, ds.hint));
        }
    }
    section
}

/// 探测 `root` 项目下的数据源，返回提取出的连接档案（供工具注册）。
/// 档案名已去重（同名追加 `-2`、`-3`…）；只含能拿到完整连接信息的库。
pub fn probe_configs(root: &Path) -> Vec<DatabaseServerConfig> {
    probe_full(root).1
}

/// 扫描项目根（一层 + Spring 固定资源目录），返回去重后的数据源清单。
fn probe(root: &Path) -> Vec<Datasource> {
    let mut found: Vec<Datasource> = Vec::new();
    // 根目录 + Spring 资源目录的命名配置文件。
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    for dir in [root.join("config"), root.join("src/main/resources")] {
        if let Ok(rd) = fs::read_dir(&dir) {
            for e in rd.flatten() {
                candidates.push(e.path());
            }
        }
    }
    if let Ok(rd) = fs::read_dir(root) {
        for e in rd.flatten() {
            let p = e.path();
            let is_db_file = {
                let s = p.extension().and_then(|x| x.to_str()).unwrap_or("").to_ascii_lowercase();
                matches!(s.as_str(), "db" | "sqlite" | "sqlite3")
            };
            if is_db_file {
                let size = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                if size > 0 {
                    let file = p.file_name().and_then(|n| n.to_str()).unwrap_or("sqlite").to_owned();
                    found.push(Datasource {
                        kind: "SQLite",
                        detail: format!("库文件 `{}`（{} bytes）", p.display(), size),
                        hint: format!("sqlite3 \"{}\" \".tables\"", p.display()),
                        config: Some(DatabaseServerConfig {
                            name: file,
                            db_type: "sqlite".to_owned(),
                            host: p.display().to_string(),
                            port: 0,
                            database: String::new(),
                            username: String::new(),
                            password: String::new(),
                            cli_path: String::new(),
                            description: "项目根目录探测到的 SQLite 库文件".to_owned(),
                        }),
                    });
                }
                continue;
            }
            candidates.push(p);
        }
    }

    for path in candidates {
        if found.len() >= MAX_ITEMS {
            break;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_ascii_lowercase();
        let hit = if name.starts_with("docker-compose") || name.starts_with("compose") {
            scan_compose(&path)
        } else if name.starts_with(".env") {
            scan_dotenv(&path)
        } else if name.starts_with("application") && (name.ends_with(".yml") || name.ends_with(".yaml") || name.ends_with(".properties")) {
            scan_jdbc(&path)
        } else if name == "schema.prisma" {
            scan_prisma(&path)
        } else if name == "settings.py" {
            scan_django(&path)
        } else if name == "database.yml" {
            scan_rails(&path)
        } else {
            Vec::new()
        };
        for ds in hit {
            if !found.iter().any(|f| f.detail == ds.detail) {
                found.push(ds);
            }
        }
    }
    found.truncate(MAX_ITEMS);
    found
}

/// 读文件（超限截断），返回原始行列表供模式匹配。
/// 注意：凭证值大小写敏感，行不做整体小写化；需要大小写无关匹配的
/// 调用方在循环内自行 `to_lowercase()`。
fn lines(path: &Path) -> Vec<String> {
    let ok = fs::metadata(path).map(|m| m.len() <= MAX_FILE).unwrap_or(false);
    if !ok {
        return Vec::new();
    }
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_owned())
        .collect()
}

/// docker-compose：`image: postgres:16` + `ports: "5432:5432"`。
fn scan_compose(path: &Path) -> Vec<Datasource> {
    let ls = lines(path);
    let mut found = Vec::new();
    let mut current_image: Option<(&'static str, String)> = None;
    for l in &ls {
        let l = l.to_ascii_lowercase();
        if let Some(img) = l.strip_prefix("- image:").or_else(|| l.split_once("image:").map(|(_, v)| v)) {
            let img = img.trim().trim_matches('"').trim_matches('\'');
            let kind = if img.starts_with("postgres") {
                Some("PostgreSQL")
            } else if img.starts_with("mysql") {
                Some("MySQL")
            } else if img.starts_with("mariadb") {
                Some("MariaDB")
            } else if img.starts_with("mongo") {
                Some("MongoDB")
            } else if img.starts_with("redis") {
                Some("Redis")
            } else {
                None
            };
            current_image = kind.map(|k| (k, img.to_owned()));
        } else if let Some((k, img)) = &current_image {
            // 端口映射行（"5432:5432" 或 - "5432:5432"）。
            if l.contains(":") && (l.contains("- \"") || l.contains("- '") || l.starts_with('-')) {
                let port = l
                    .trim_start_matches('-')
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .split(':')
                    .next()
                    .unwrap_or("")
                    .to_owned();
                let (svc, cmd) = match *k {
                    "PostgreSQL" | "MariaDB" => ("psql", format!("docker compose exec postgres psql -U postgres -l")),
                    "MySQL" => ("mysql", "docker compose exec mysql mysql -e 'show databases;'".to_owned()),
                    "MongoDB" => ("mongosh", "docker compose exec mongo mongosh --eval 'db.adminCommand({listDatabases:1})'".to_owned()),
                    "Redis" => ("redis-cli", "docker compose exec redis redis-cli dbsize".to_owned()),
                    _ => ("客户端", "docker compose ps".to_owned()),
                };
                let _ = svc;
                found.push(Datasource {
                    kind: k,
                    detail: format!("docker-compose 服务镜像 `{img}`，宿主端口 {port}"),
                    hint: cmd,
                    config: None,
                });
                current_image = None;
            }
        }
    }
    found
}

/// .env：DATABASE_URL / DB_* / POSTGRES_* / MYSQL_*。
/// `DATABASE_URL` 能完整解析出 host/port/db/user/password 时生成连接档案；
/// 分离变量（DB_HOST/DB_PORT/DB_NAME/DB_USER/DB_PASSWORD 及 POSTGRES_/MYSQL_ 前缀）
/// 凑齐 host+database 时也生成档案。
fn scan_dotenv(path: &Path) -> Vec<Datasource> {
    let ls = lines(path);
    let mut found = Vec::new();
    let mut parts: Vec<String> = Vec::new();
    // 分离变量的原始值（小写键 → 原值），供档案提取。
    let mut vars: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for raw in &ls {
        let Some((k_raw, v_raw)) = raw.split_once('=') else { continue };
        let k = k_raw.trim().to_ascii_lowercase();
        let v = v_raw.trim().trim_matches('"').trim_matches('\'');
        if k == "database_url" {
            parts.push(format!("{k}={}", mask_password(v)));
            vars.insert("database_url_raw".to_owned(), v.to_owned());
        } else if matches!(k.as_str(), "db_host" | "db_port" | "db_name" | "db_user" | "db_username" | "db_password" | "postgres_host" | "postgres_port" | "postgres_db" | "postgres_user" | "postgres_password" | "mysql_host" | "mysql_port" | "mysql_database" | "mysql_user" | "mysql_password") {
            parts.push(format!("{k}={}", mask_password(v)));
            vars.insert(k, v.to_owned());
        }
    }
    // DATABASE_URL → 完整档案。
    if let Some(raw) = vars.get("database_url_raw") {
        if let Some((db_type, user, pass, host, port, database)) = parse_db_url(raw) {
            let kind = kind_of(db_type);
            found.push(Datasource {
                kind,
                detail: format!(".env `DATABASE_URL={}`", mask_password(raw)),
                hint: "已注册 db_query/db_dml 工具，直接用工具查询".to_owned(),
                config: Some(DatabaseServerConfig {
                    name: if database.is_empty() { host.clone() } else { database.clone() },
                    db_type: db_type.to_owned(),
                    host,
                    port,
                    database,
                    username: user,
                    password: pass,
                    cli_path: String::new(),
                    description: ".env DATABASE_URL 提取".to_owned(),
                }),
            });
        }
    }
    // 分离变量：凑齐 host + database 即建档案（密码可选）。
    if vars.contains_key("db_host") || vars.contains_key("postgres_host") || vars.contains_key("mysql_host") {
        let (host, port, database, user, pass) = pick_env_vars(&vars);
        if !host.is_empty() && !database.is_empty() {
            let db_type = if vars.contains_key("mysql_host") {
                "mysql"
            } else if vars.contains_key("postgres_host") {
                "postgres"
            } else if port == 3306 {
                "mysql"
            } else {
                "postgres"
            };
            found.push(Datasource {
                kind: kind_of(db_type),
                detail: format!(".env {}", parts.join("、")),
                hint: "已注册 db_query/db_dml 工具，直接用工具查询".to_owned(),
                config: Some(DatabaseServerConfig {
                    name: database.clone(),
                    db_type: db_type.to_owned(),
                    host,
                    port,
                    database,
                    username: user,
                    password: pass,
                    cli_path: String::new(),
                    description: ".env 分离变量提取".to_owned(),
                }),
            });
        } else if !parts.is_empty() {
            found.push(Datasource {
                kind: "数据库（.env 变量）",
                detail: format!(".env {}", parts.join("、")),
                hint: "按变量判断类型后用对应命令行客户端".to_owned(),
                config: None,
            });
        }
    } else if !parts.is_empty() {
        found.push(Datasource {
            kind: "数据库（.env 变量）",
            detail: format!(".env {}", parts.join("、")),
            hint: "按变量判断类型后用对应命令行客户端".to_owned(),
            config: None,
        });
    }
    found
}

/// 从 .env 分离变量里按优先级取连接五元组（DB_* → POSTGRES_* → MYSQL_*）。
fn pick_env_vars(vars: &std::collections::HashMap<String, String>) -> (String, u16, String, String, String) {
    let get = |keys: &[&str]| -> Option<String> {
        keys.iter().find_map(|k| vars.get(*k).filter(|v| !v.is_empty()).cloned())
    };
    let host = get(&["db_host", "postgres_host", "mysql_host"]).unwrap_or_default();
    let port = get(&["db_port", "postgres_port", "mysql_port"])
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(0);
    let database = get(&["db_name", "postgres_db", "mysql_database"]).unwrap_or_default();
    let user = get(&["db_user", "db_username", "postgres_user", "mysql_user"]).unwrap_or_default();
    let pass = get(&["db_password", "postgres_password", "mysql_password"]).unwrap_or_default();
    (host, port, database, user, pass)
}

/// 解析数据库连接 URL（`scheme://user:pass@host:port/db`）为
/// `(db_type, user, password, host, port, database)`；解析不出 host 返回 None。
fn parse_db_url(url: &str) -> Option<(&'static str, String, String, String, u16, String)> {
    let (scheme, rest) = url.split_once("://")?;
    let db_type = match scheme.to_ascii_lowercase().as_str() {
        "postgres" | "postgresql" => "postgres",
        "mysql" | "mariadb" => "mysql",
        "sqlite" => "sqlite",
        _ => return None,
    };
    if db_type == "sqlite" {
        // sqlite:path 或 sqlite://path —— 路径即 host。
        let p = rest.trim_start_matches('/');
        return Some((db_type, String::new(), String::new(), p.to_owned(), 0, String::new()));
    }
    let (creds, hostpart) = match rest.split_once('@') {
        Some((c, h)) => (Some(c), h),
        None => (None, rest),
    };
    let (user, pass) = creds
        .map(|c| match c.split_once(':') {
            Some((u, p)) => (u.to_owned(), p.to_owned()),
            None => (c.to_owned(), String::new()),
        })
        .unwrap_or_default();
    let (hostport, database) = match hostpart.split_once('/') {
        Some((hp, db)) => (hp, db.to_owned()),
        None => (hostpart, String::new()),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h.to_owned(), p.parse::<u16>().unwrap_or(0)),
        None => (hostport.to_owned(), 0),
    };
    if host.is_empty() {
        return None;
    }
    Some((db_type, user, pass, host, port, database))
}

/// db_type → 展示类型名。
fn kind_of(db_type: &str) -> &'static str {
    match db_type {
        "postgres" => "PostgreSQL",
        "mysql" => "MySQL",
        "sqlite" => "SQLite",
        _ => "数据库",
    }
}

/// Spring `jdbc:postgresql://host:port/db` 连接串 + 紧邻的 username/password。
/// yml（`username: x`）与 properties（`spring.datasource.username=x`）都识别；
/// username/password 归最近一条 jdbc url。
fn scan_jdbc(path: &Path) -> Vec<Datasource> {
    let ls = lines(path);
    let mut found = Vec::new();
    // (kind, url, user, pass) —— url 开新组，username/password 归最近组。
    let mut current: Option<(&'static str, String, String, String)> = None;
    let mut done: Vec<Datasource> = Vec::new();
    for raw in &ls {
        let lower = raw.to_ascii_lowercase();
        let l = lower.as_str();
        if let Some(i) = l.find("jdbc:") {
            // 上一组先落盘。
            if let Some((kind, url, user, pass)) = current.take() {
                done.push(build_jdbc_ds(kind, &url, &user, &pass));
            }
            let rest = &l[i + 5..];
            let kind = if rest.starts_with("postgresql") || rest.starts_with("postgis") {
                Some("PostgreSQL")
            } else if rest.starts_with("mysql") || rest.starts_with("mariadb") {
                Some("MySQL")
            } else if rest.starts_with("sqlite") {
                Some("SQLite")
            } else if rest.starts_with("h2") {
                None // H2 是应用内嵌，不建档案
            } else {
                None
            };
            if let Some(kind) = kind {
                let url = rest.split_whitespace().next().unwrap_or(rest).to_owned();
                current = Some((kind, url, String::new(), String::new()));
            }
        } else if current.is_some() {
            // 凭证值大小写敏感：从原始行提取（键匹配在 extract_kv 内小写化）。
            let (user, pass): (Option<String>, Option<String>) = (
                extract_kv(raw, &["username", "datasource.username", "user"]),
                extract_kv(raw, &["password", "datasource.password"]),
            );
            if let Some((kind, url, u, p)) = current.as_mut() {
                if let Some(x) = user {
                    *u = x;
                }
                if let Some(x) = pass {
                    *p = x;
                }
                let _ = (kind, url);
            }
        }
    }
    if let Some((kind, url, user, pass)) = current.take() {
        done.push(build_jdbc_ds(kind, &url, &user, &pass));
    }
    found.extend(done);
    found
}

/// 由 jdbc url + 凭证构造 Datasource（能解析出 host 时带档案）。
fn build_jdbc_ds(kind: &'static str, url: &str, user: &str, pass: &str) -> Datasource {
    // jdbc:postgresql://host:port/db → 去掉 jdbc: 前缀后按标准 URL 解析。
    let bare = url.strip_prefix("jdbc:").unwrap_or(url);
    let config = parse_db_url(bare).map(|(db_type, u, p, host, port, database)| DatabaseServerConfig {
        name: if database.is_empty() { host.clone() } else { database.clone() },
        db_type: db_type.to_owned(),
        host,
        port,
        database,
        username: if user.is_empty() { u } else { user.to_owned() },
        password: if pass.is_empty() { p } else { pass.to_owned() },
        cli_path: String::new(),
        description: "Spring 配置 jdbc 连接串提取".to_owned(),
    });
    let hint = if config.is_some() {
        "已注册 db_query/db_dml 工具，直接用工具查询".to_owned()
    } else {
        "用命令行客户端连接（地址见连接串）".to_owned()
    };
    Datasource {
        kind,
        detail: format!("`jdbc:{}`", mask_password(url)),
        hint,
        config,
    }
}

/// 从小写化配置行里提取 `key: value` 或 `key=value`（key 支持点路径后缀匹配）。
fn extract_kv(line: &str, keys: &[&str]) -> Option<String> {
    let (k, v) = if let Some((k, v)) = line.split_once(':') {
        (k, v)
    } else if let Some((k, v)) = line.split_once('=') {
        (k, v)
    } else {
        return None;
    };
    let k = k.trim().to_ascii_lowercase();
    if !keys.iter().any(|key| {
        k == *key || k.ends_with(&format!(".{key}")) || k.ends_with(&format!("_{key}"))
    }) {
        return None;
    }
    let v = v.trim().trim_matches('"').trim_matches('\'');
    if v.is_empty() {
        None
    } else {
        Some(v.to_owned())
    }
}

/// Prisma `datasource db { provider = "postgresql" ; url = ... }`。
fn scan_prisma(path: &Path) -> Vec<Datasource> {
    let ls = lines(path);
    let mut found = Vec::new();
    let mut provider: Option<&str> = None;
    for raw in &ls {
        let l = raw.to_ascii_lowercase();
        if l.contains("provider") {
            for (k, kind) in [
                ("postgresql", "PostgreSQL"),
                ("mysql", "MySQL"),
                ("sqlite", "SQLite"),
                ("mongo", "MongoDB"),
            ] {
                if l.contains(k) {
                    provider = Some(kind);
                }
            }
        } else if l.contains("url") {
            if let Some(kind) = provider {
                let url = raw.split('=').nth(1).unwrap_or("").trim().trim_matches('"');
                found.push(Datasource {
                    kind,
                    detail: format!("prisma datasource `{}`", mask_password(url)),
                    hint: "按 provider 选命令行客户端".to_owned(),
                    config: None,
                });
                provider = None;
            }
        }
    }
    found
}

/// Django `DATABASES` ENGINE。
fn scan_django(path: &Path) -> Vec<Datasource> {
    let ls = lines(path);
    let mut found = Vec::new();
    for l in &ls {
        let l = l.to_ascii_lowercase();
        for (k, kind) in [
            ("django.db.backends.postgresql", "PostgreSQL"),
            ("django.db.backends.mysql", "MySQL"),
            ("django.db.backends.sqlite3", "SQLite"),
        ] {
            if l.contains(k) {
                found.push(Datasource {
                    kind,
                    detail: format!("Django `ENGINE={k}`"),
                    hint: "NAME/HOST 见 settings.py 的 DATABASES".to_owned(),
                    config: None,
                });
            }
        }
    }
    found
}

/// Rails `adapter: postgresql`。
fn scan_rails(path: &Path) -> Vec<Datasource> {
    let ls = lines(path);
    let mut found = Vec::new();
    for l in &ls {
        let l = l.to_ascii_lowercase();
        for (k, kind) in [("postgresql", "PostgreSQL"), ("mysql2", "MySQL"), ("sqlite3", "SQLite")] {
            if l.contains(&format!("adapter: {k}")) {
                found.push(Datasource {
                    kind,
                    detail: format!("Rails `adapter: {k}`"),
                    hint: "连接参数见 database.yml 对应节".to_owned(),
                    config: None,
                });
            }
        }
    }
    found
}

/// 掩去 URL 里的密码段（`user:***@`），凭证不进提示词。
fn mask_password(url: &str) -> String {
    match (url.find("://"), url.find('@')) {
        (Some(s), Some(a)) if a > s => {
            let head = &url[..s + 3];
            let tail = &url[a..];
            let creds = &url[s + 3..a];
            let user = creds.split(':').next().unwrap_or("");
            format!("{head}{user}:***{tail}")
        }
        _ => url.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn empty_project_yields_no_section() {
        let dir = std::env::temp_dir().join(format!("ds-probe-empty-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        assert!(probe_section(&dir).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dotenv_url_is_detected_and_masked() {
        let dir = std::env::temp_dir().join(format!("ds-probe-env-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(".env"),
            "DATABASE_URL=postgres://alice:s3cret@localhost:5432/appdb\n",
        )
        .unwrap();
        let section = probe_section(&dir).unwrap();
        assert!(section.contains("PostgreSQL"));
        assert!(section.contains("alice:***"));
        assert!(!section.contains("s3cret"));
        // 档案提取：完整连接信息（密码只进档案，不进提示词）。
        let configs = probe_configs(&dir);
        assert_eq!(configs.len(), 1, "DATABASE_URL 应提取出一个档案");
        let cfg = &configs[0];
        assert_eq!(cfg.db_type, "postgres");
        assert_eq!(cfg.host, "localhost");
        assert_eq!(cfg.port, 5432);
        assert_eq!(cfg.database, "appdb");
        assert_eq!(cfg.username, "alice");
        assert_eq!(cfg.password, "s3cret");
        assert_eq!(cfg.name, "appdb");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn spring_jdbc_yml_extracts_profile() {
        let dir = std::env::temp_dir().join(format!("ds-probe-jdbc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("application.yml"),
            "spring:\n  datasource:\n    url: jdbc:postgresql://10.0.0.8:5433/orders\n    username: svc\n    password: P@ss\n",
        )
        .unwrap();
        let configs = probe_configs(&dir);
        assert_eq!(configs.len(), 1);
        let cfg = &configs[0];
        assert_eq!(cfg.db_type, "postgres");
        assert_eq!(cfg.host, "10.0.0.8");
        assert_eq!(cfg.port, 5433);
        assert_eq!(cfg.database, "orders");
        assert_eq!(cfg.username, "svc");
        assert_eq!(cfg.password, "P@ss");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sqlite_file_yields_profile_and_env_password_case_preserved() {
        let dir = std::env::temp_dir().join(format!("ds-probe-sqlite-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("data.sqlite3"), "x").unwrap();
        fs::write(
            dir.join(".env"),
            "DB_HOST=127.0.0.1\nDB_PORT=3306\nDB_NAME=shop\nDB_USER=root\nDB_PASSWORD=Mix3dCase\n",
        )
        .unwrap();
        let configs = probe_configs(&dir);
        // SQLite 文件档案 + .env 分离变量档案。
        assert_eq!(configs.len(), 2);
        let sqlite = configs.iter().find(|c| c.db_type == "sqlite").unwrap();
        assert!(sqlite.host.ends_with("data.sqlite3"));
        let mysql = configs.iter().find(|c| c.db_type == "mysql").unwrap();
        assert_eq!(mysql.password, "Mix3dCase", "密码大小写必须保留");
        assert_eq!(mysql.name, "shop");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn compose_service_and_sqlite_file_are_detected() {
        let dir = std::env::temp_dir().join(format!("ds-probe-mix-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("docker-compose.yml"),
            "services:\n  db:\n    image: postgres:16\n    ports:\n      - \"5432:5432\"\n",
        )
        .unwrap();
        fs::write(dir.join("app.db"), "x").unwrap();
        let section = probe_section(&dir).unwrap();
        assert!(section.contains("PostgreSQL"));
        assert!(section.contains("SQLite"));
        assert!(section.contains("5432"));
        let _ = fs::remove_dir_all(&dir);
    }
}
