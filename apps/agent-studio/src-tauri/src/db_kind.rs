//! 数据库类型注册表（抽象层）。
//!
//! 「支持哪些数据库」集中在这张表里：新增类型 = 在 [`DATABASE_KINDS`] 加一个
//! 描述符，命令面校验（`save_database`）、连接摘要（`endpoint`）、前端下拉
//! （`list_database_kinds`）自动跟随，无需再改任何散落的白名单。
//!
//! **驱动策略（不内置）**：网络数据库（MySQL/PostgreSQL/Oracle/SQL Server）
//! 不编译任何客户端驱动进应用——执行时调用用户机器上已装的官方 CLI 客户端
//! （`mysql` / `psql` / `sqlplus` / `sqlcmd`），按 [`DatabaseKind::driver`]
//! 声明的程序名探测 PATH，缺失时报错并提示安装。唯一例外是 sqlite：rusqlite
//! bundled 随应用编译（纯 Rust 单文件库，无系统依赖），走 [`DatabaseDriver::Bundled`]。
//!
//! 执行层抽象（`trait DatabaseExecutor`：按档案拼 CLI 命令 / 走 rusqlite，
//! 解析输出）留到 agent 接线批次再做——当前批次只做资源档案管理，提前引入
//! trait 只会产生无实现的死代码。

use serde::Serialize;

/// 驱动形态（注册表数据；执行层按它分派，应用不内置网络数据库驱动）。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "mode", content = "program")]
pub enum DatabaseDriver {
    /// 外部 CLI 客户端：`program` 为可执行名（如 `psql`），执行时探测 PATH。
    Cli { program: &'static str },
    /// 内置驱动（仅 sqlite：rusqlite bundled，无系统依赖）。
    Bundled,
}

/// 数据库类型描述符。
#[derive(Debug, Clone, Serialize)]
pub struct DatabaseKind {
    /// 稳定 id（存配置文件的 `db_type` 字段）。
    pub id: &'static str,
    /// 展示名（前端下拉）。
    pub label: &'static str,
    /// 默认端口（0 = 不适用，如 sqlite 文件库）。
    pub default_port: u16,
    /// 是否需要主机地址（sqlite 复用该字段存文件路径）。
    pub needs_host: bool,
    /// 是否需要端口。
    pub needs_port: bool,
    /// 是否需要库名。
    pub needs_database: bool,
    /// 驱动形态（不内置网络驱动；CLI 程序名在此声明）。
    pub driver: DatabaseDriver,
}

/// 注册表：新增数据库类型在这里加一行。
pub const DATABASE_KINDS: &[DatabaseKind] = &[
    DatabaseKind {
        id: "mysql",
        label: "MySQL",
        default_port: 3306,
        needs_host: true,
        needs_port: true,
        needs_database: true,
        driver: DatabaseDriver::Cli { program: "mysql" },
    },
    DatabaseKind {
        id: "postgres",
        label: "PostgreSQL",
        default_port: 5432,
        needs_host: true,
        needs_port: true,
        needs_database: true,
        driver: DatabaseDriver::Cli { program: "psql" },
    },
    DatabaseKind {
        id: "oracle",
        label: "Oracle",
        default_port: 1521,
        needs_host: true,
        needs_port: true,
        needs_database: true,
        driver: DatabaseDriver::Cli {
            program: "sqlplus",
        },
    },
    DatabaseKind {
        id: "sqlserver",
        label: "SQL Server",
        default_port: 1433,
        needs_host: true,
        needs_port: true,
        needs_database: true,
        driver: DatabaseDriver::Cli { program: "sqlcmd" },
    },
    DatabaseKind {
        id: "sqlite",
        label: "SQLite（本地文件）",
        default_port: 0,
        needs_host: true,
        needs_port: false,
        needs_database: false,
        driver: DatabaseDriver::Bundled,
    },
];

impl DatabaseKind {
    /// 按 id 查描述符（配置校验与展示共用的唯一入口）。
    pub fn find(id: &str) -> Option<&'static DatabaseKind> {
        DATABASE_KINDS.iter().find(|k| k.id == id)
    }

    /// CLI 程序名（Bundled 类型返回 None）。执行层接线批次启用。
    #[allow(dead_code)]
    pub fn cli_program(&self) -> Option<&'static str> {
        match &self.driver {
            DatabaseDriver::Cli { program } => Some(program),
            DatabaseDriver::Bundled => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_known_kinds() {
        assert_eq!(DatabaseKind::find("mysql").unwrap().default_port, 3306);
        assert_eq!(DatabaseKind::find("postgres").unwrap().default_port, 5432);
        assert_eq!(DatabaseKind::find("oracle").unwrap().default_port, 1521);
        assert_eq!(DatabaseKind::find("sqlserver").unwrap().default_port, 1433);
        let sqlite = DatabaseKind::find("sqlite").unwrap();
        assert!(!sqlite.needs_port);
        assert!(!sqlite.needs_database);
        assert!(DatabaseKind::find("mongodb").is_none());
    }

    /// 驱动策略：网络数据库全部走外部 CLI（不内置），仅 sqlite 内置。
    #[test]
    fn drivers_are_cli_except_sqlite() {
        for kind in DATABASE_KINDS {
            let program = kind.cli_program();
            if kind.id == "sqlite" {
                assert!(program.is_none(), "sqlite should be bundled");
            } else {
                assert!(program.is_some(), "{} must declare a CLI program", kind.id);
            }
        }
        assert_eq!(DatabaseKind::find("oracle").unwrap().cli_program(), Some("sqlplus"));
        assert_eq!(DatabaseKind::find("sqlserver").unwrap().cli_program(), Some("sqlcmd"));
    }

    #[test]
    fn ids_are_unique() {
        let mut ids: Vec<&str> = DATABASE_KINDS.iter().map(|k| k.id).collect();
        let n = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), n, "duplicate database kind id");
    }
}
