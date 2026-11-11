//! 数据库执行层已下沉至 [`agentscope_db`] crate（studio 与 ox-idea 共用同一实现）。
//! 本模块保留原路径的 re-export，`crate::db_exec::…` 的既有引用无需改动。

pub use agentscope_db::{execute, probe_cli, DbCatalog, DbDmlTool, DbQueryTool};
