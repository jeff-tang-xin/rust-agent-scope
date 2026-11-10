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

/// 单个探测到的数据源：`kind` 是数据库类型，`detail` 是连接要点，
/// `hint` 是给 LLM 的建议验证命令（走 shell 工具执行）。
pub struct Datasource {
    kind: &'static str,
    detail: String,
    hint: String,
}

/// 单文件读取上限：配置文件不该更大，超了按截断处理（探测是启发式的）。
const MAX_FILE: u64 = 64 * 1024;
/// 注入提示的数据源条数上限：再多说明探测误报，宁缺毋滥。
const MAX_ITEMS: usize = 8;

/// 探测 `root` 项目下的数据源；渲染成系统提示节。没有发现返回 `None`
/// （调用方不注入、不提数据库）。
pub fn probe_section(root: &Path) -> Option<String> {
    let found = probe(root);
    if found.is_empty() {
        return None;
    }
    let mut section = String::from("\n## 本项目检测到的数据源\n\
         - 以下数据库来自项目配置（未验证连通性）：连接与查询一律用 shell 工具跑命令行客户端，\
         不要尝试用文件工具读数据库文件（SQLite 除外，sqlite3 可直接打开）。\n\
         - 凭证在配置文件/环境变量里，不要把密码写进回复或日志；只读查询优先，\
         写操作先向用户确认。\n");
    for ds in &found {
        section.push_str(&format!("- **{}** — {}（验证：`{}`）\n", ds.kind, ds.detail, ds.hint));
    }
    Some(section)
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
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_ascii_lowercase();
            let is_db_file = {
                let s = p.extension().and_then(|x| x.to_str()).unwrap_or("").to_ascii_lowercase();
                matches!(s.as_str(), "db" | "sqlite" | "sqlite3")
            };
            if is_db_file {
                let size = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                if size > 0 {
                    found.push(Datasource {
                        kind: "SQLite",
                        detail: format!("库文件 `{}`（{} bytes）", p.display(), size),
                        hint: format!("sqlite3 \"{}\" \".tables\"", p.display()),
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

/// 读文件（超限截断），返回小写化的行列表供模式匹配。
fn lines(path: &Path) -> Vec<String> {
    let ok = fs::metadata(path).map(|m| m.len() <= MAX_FILE).unwrap_or(false);
    if !ok {
        return Vec::new();
    }
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_ascii_lowercase())
        .collect()
}

/// docker-compose：`image: postgres:16` + `ports: "5432:5432"`。
fn scan_compose(path: &Path) -> Vec<Datasource> {
    let ls = lines(path);
    let mut found = Vec::new();
    let mut current_image: Option<(&'static str, String)> = None;
    for l in &ls {
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
                });
                current_image = None;
            }
        }
    }
    found
}

/// .env：DATABASE_URL / DB_* / POSTGRES_* / MYSQL_*。
fn scan_dotenv(path: &Path) -> Vec<Datasource> {
    let ls = lines(path);
    let mut found = Vec::new();
    let mut url: Option<String> = None;
    let mut parts: Vec<String> = Vec::new();
    for l in &ls {
        let Some((k, v)) = l.split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim().trim_matches('"').trim_matches('\''));
        if k == "database_url" {
            url = Some(mask_password(v));
        } else if matches!(k, "db_host" | "db_port" | "db_name" | "postgres_host" | "postgres_port" | "postgres_db" | "postgres_user" | "mysql_host" | "mysql_port" | "mysql_database") {
            parts.push(format!("{k}={v}"));
        }
    }
    if let Some(u) = url {
        let kind = if u.contains("postgres") {
            "PostgreSQL"
        } else if u.contains("mysql") || u.contains("mariadb") {
            "MySQL"
        } else if u.contains("sqlite") {
            "SQLite"
        } else if u.contains("mongo") {
            "MongoDB"
        } else if u.contains("redis") {
            "Redis"
        } else {
            "数据库"
        };
        found.push(Datasource {
            kind,
            detail: format!(".env `DATABASE_URL={u}`"),
            hint: "按 URL 里的驱动选命令行客户端（psql / mysql / sqlite3）".to_owned(),
        });
    }
    if !parts.is_empty() {
        found.push(Datasource {
            kind: "数据库（.env 变量）",
            detail: format!(".env {}", parts.join("、")),
            hint: "按变量判断类型后用对应命令行客户端".to_owned(),
        });
    }
    found
}

/// Spring `jdbc:postgresql://host:port/db` 连接串。
fn scan_jdbc(path: &Path) -> Vec<Datasource> {
    let ls = lines(path);
    let mut found = Vec::new();
    for l in &ls {
        if let Some(i) = l.find("jdbc:") {
            let rest = &l[i + 5..];
            let (kind, client) = if rest.starts_with("postgresql") || rest.starts_with("postgis") {
                ("PostgreSQL", "psql")
            } else if rest.starts_with("mysql") || rest.starts_with("mariadb") {
                ("MySQL", "mysql")
            } else if rest.starts_with("sqlite") {
                ("SQLite", "sqlite3")
            } else if rest.starts_with("h2") {
                ("H2", "Java 应用内嵌")
            } else {
                continue;
            };
            let url = rest.split_whitespace().next().unwrap_or(rest);
            found.push(Datasource {
                kind,
                detail: format!("`jdbc:{}`", mask_password(url)),
                hint: format!("用 {client} 连接（地址见连接串）"),
            });
        }
    }
    found
}

/// Prisma `datasource db { provider = "postgresql" ; url = ... }`。
fn scan_prisma(path: &Path) -> Vec<Datasource> {
    let ls = lines(path);
    let mut found = Vec::new();
    let mut provider: Option<&str> = None;
    for l in &ls {
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
                let url = l.split('=').nth(1).unwrap_or("").trim().trim_matches('"');
                found.push(Datasource {
                    kind,
                    detail: format!("prisma datasource `{}`", mask_password(url)),
                    hint: "按 provider 选命令行客户端".to_owned(),
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
        for (k, kind) in [("postgresql", "PostgreSQL"), ("mysql2", "MySQL"), ("sqlite3", "SQLite")] {
            if l.contains(&format!("adapter: {k}")) {
                found.push(Datasource {
                    kind,
                    detail: format!("Rails `adapter: {k}`"),
                    hint: "连接参数见 database.yml 对应节".to_owned(),
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
