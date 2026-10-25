//! Studio 配置（模型 Provider + Agent 定义）的持久化。
//!
//! 对齐 agentscope-service 的资源模型（README：Workspace / Environment / Vault
//! 组织执行资源）的本地化最小形态：
//! - `ModelProviderConfig` ≈ Environment 里的模型凭据（本地桌面应用直接存配置文件，
//!   不建 Vault；文件位于 Tauri app config dir，不进代码仓库）；
//! - `AgentConfig` ≈ Managed Agent 定义（指令、模型绑定、工具面由 mode 决定）。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 模型 Provider 配置（OpenAI 兼容协议；`base_url` 可指向 DashScope 兼容模式、
/// DeepSeek、Ollama、vLLM 等，对齐 `OpenAIChatModel::with_base_url` 的语义）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelProviderConfig {
    /// 展示名（Agent 通过名字引用）。
    pub name: String,
    /// OpenAI 兼容服务地址。
    pub base_url: String,
    /// API key（仅存本地配置文件；日志与事件中不回显）。
    #[serde(default)]
    pub api_key: String,
    /// 默认模型名（如 `deepseek-chat`、`qwen-plus`）。
    pub model: String,
}

/// Agent 定义（≈ Managed Agent 定义的最小面）。
///
/// **纯智能体定义**：人设 + Provider 绑定。不含工作区目录、类别、team
/// 成员——那些都是工作区的属性（见 [`WorkspaceConfig`]）。一个智能体可被
/// 任意多个工作区选用（不同项目）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// 稳定 id（uuid simple）。
    pub id: String,
    /// 展示名。
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// 系统提示（≈ Java Agent 定义里的 instructions）。
    #[serde(default)]
    pub sys_prompt: String,
    /// 引用的 [`ModelProviderConfig::name`]。
    pub provider: String,
    /// 最大推理迭代数（0 = harness 默认）。
    #[serde(default)]
    pub max_iters: u32,
    #[serde(default)]
    pub created_at: u64,
}

/// 工作区（≈ service 的 Workspace）：**针对一个项目的工作环境**。
///
/// 三类（[`WORKSPACE_KINDS`]）：协同（solo/team 运行方式可切换，solo 下
/// plan/free 可切换）、运维（shell + web）、数据库运维（只读 SQL）。
/// 工作区选定一个智能体干活 + 绑定工作目录；同一智能体可被多个工作区
/// 选用，会话状态按工作区隔离。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    /// 稳定 id（uuid simple；数据目录 / 会话状态都挂在它下面）。
    pub id: String,
    /// 展示名。
    pub name: String,
    /// 工作区类型：collab | ops | db（决定工具面与运行方式语义）。
    pub kind: String,
    /// 选定的智能体（[`AgentConfig::id`]）。
    pub agent_id: String,
    /// 工作目录（绝对路径；空 = 默认 `{data_dir}/workspaces/{id}/workspace`）。
    /// 指定后 agent 的文件工具锚定该目录（Sandboxed 模式，越界路径被拒）——
    /// 可指向真实项目目录让 agent 直接在项目里干活。
    #[serde(default)]
    pub dir: String,
    /// 协同工作区 team 运行方式下的成员：**已有智能体的 id 列表**
    /// （lead 之外；每个成员是独立子代理，用自己的人设与 provider）。
    #[serde(default)]
    pub members: Vec<String>,
    #[serde(default)]
    pub created_at: u64,
}

/// 工作区类型（协同 / 运维 / 数据库运维——都是工作区，针对不同的项目）。
pub const WORKSPACE_KINDS: &[&str] = &["collab", "ops", "db"];

impl WorkspaceConfig {
    pub fn kind_valid(&self) -> bool {
        WORKSPACE_KINDS.contains(&self.kind.as_str())
    }
}

/// 数据库服务器资源（≈ service 资源模型里的数据源；供数据库运维类
/// agent 后续绑定的连接档案）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseServerConfig {
    /// 展示名（唯一键；agent/会话通过名字引用）。
    pub name: String,
    /// 数据库类型：mysql | postgres | sqlite | sqlserver（驱动选择与展示用）。
    pub db_type: String,
    /// 主机（sqlite 类型时为文件路径，host/port 留空）。
    #[serde(default)]
    pub host: String,
    /// 端口（0 = 未设置；sqlite 无需端口）。
    #[serde(default)]
    pub port: u16,
    /// 库名（sqlite 为文件路径时与 host 二选一，约定填 host）。
    #[serde(default)]
    pub database: String,
    #[serde(default)]
    pub username: String,
    /// 密码（仅存本地配置文件，与 api_key 同策略；不进日志与事件）。
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub description: String,
}

impl DatabaseServerConfig {
    // 连接摘要由前端按 list_database_kinds 的元数据拼装（needs_port/needs_database），
    // 后端不再重复实现一份 endpoint()。
}

/// Studio 全量配置。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StudioConfig {
    #[serde(default)]
    pub providers: Vec<ModelProviderConfig>,
    #[serde(default)]
    pub agents: Vec<AgentConfig>,
    #[serde(default)]
    pub workspaces: Vec<WorkspaceConfig>,
    #[serde(default)]
    pub databases: Vec<DatabaseServerConfig>,
}

impl StudioConfig {
    /// 确保内置默认 agent 在列（无条件）：首次初始化、老配置升级、配置损坏
    /// 兜底、甚至历史遗留的删除标记都挡不住它——「不可删除」由后端
    /// delete_agent 拒绝 + 这里兜底共同保证。
    pub fn ensure_default_agent(&mut self) {
        if !self.agents.iter().any(|a| a.id == DEFAULT_AGENT_ID) {
            self.agents.push(default_agent());
        }
    }
    pub fn provider(&self, name: &str) -> Option<&ModelProviderConfig> {
        self.providers.iter().find(|p| p.name == name)
    }

    pub fn agent(&self, id: &str) -> Option<&AgentConfig> {
        self.agents.iter().find(|a| a.id == id)
    }

    pub fn workspace(&self, id: &str) -> Option<&WorkspaceConfig> {
        self.workspaces.iter().find(|w| w.id == id)
    }

    /// 按名取数据库资源（agent 接线批次启用；当前仅资源 CRUD）。
    #[allow(dead_code)]
    pub fn database(&self, name: &str) -> Option<&DatabaseServerConfig> {
        self.databases.iter().find(|d| d.name == name)
    }
}

/// 内置默认智能体的稳定 id（前后端共享：不可删除，其余字段可改）。
pub const DEFAULT_AGENT_ID: &str = "default-assistant";

/// 内置默认智能体：一个开箱即用的全能型助手（预置默认值的普通智能体，
/// 可改不可删；由 [`StudioConfig::ensure_default_agent`] 保证始终在列）。
fn default_agent() -> AgentConfig {
    AgentConfig {
        id: DEFAULT_AGENT_ID.into(),
        name: "全能助手".into(),
        description: "内置的全能型助手：编码、文件、检索、规划开箱即用。".into(),
        sys_prompt: [
            "你是全能助手，一个通用的工程与效率助手。你可以：",
            "- 读写与整理工作区文件（文件工具锚定工作区根，越界路径会被拒绝）；",
            "- 用记忆工具（memory_save / memory_get / memory_search）沉淀与检索跨会话的长期信息；",
            "- 先理解再动手：改动代码前先读相关文件，遵循既有风格，做最小必要修改；",
            "- 复杂任务先列计划（plan mode），执行后如实汇报做了什么、怎么验证的、还剩什么风险。",
            "不确定就说不确定，不编造事实。用中文与用户交流。",
        ]
        .join("\n"),
        // provider 留空：用户在设置页绑定模型 Provider 后即可对话（进入会话
        // 时会得到明确指引）。
        provider: String::new(),
        max_iters: 0,
        created_at: 0,
    }
}

/// 配置文件读写（JSON；位于 app config dir，不进代码仓库）。
pub struct ConfigStore {
    path: PathBuf,
}

impl ConfigStore {
    pub fn new(dir: &Path) -> Self {
        let _ = std::fs::create_dir_all(dir);
        Self {
            path: dir.join("studio-config.json"),
        }
    }

    pub fn load(&self) -> StudioConfig {
        match std::fs::read(&self.path) {
            Ok(bytes) => {
                let mut config: StudioConfig =
                    serde_json::from_slice(&bytes).unwrap_or_default();
                config.ensure_default_agent();
                config
            }
            // 首次初始化（文件不存在）：注入内置默认 agent，开箱即用。
            Err(_) => {
                let mut config = StudioConfig::default();
                config.ensure_default_agent();
                config
            }
        }
    }

    pub fn save(&self, config: &StudioConfig) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(config)
            .map_err(|e| format!("serialize config: {e}"))?;
        std::fs::write(&self.path, bytes).map_err(|e| format!("write config: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 旧配置文件（无 databases 键）加载不炸：serde(default) 兜底为空表。
    #[test]
    fn legacy_config_without_databases_loads() {
        let raw = br#"{"providers":[],"agents":[]}"#;
        let config: StudioConfig = serde_json::from_slice(raw).unwrap();
        assert!(config.databases.is_empty());
    }

    /// 内置默认 agent：始终在列、不可删除——首次初始化、老配置升级、配置
    /// 损坏兜底、甚至历史遗留的删除标记都注入（delete_agent 已在后端拒绝）。
    #[test]
    fn default_agent_always_present() {
        let dir = std::env::temp_dir().join(format!("studio-cfg-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = ConfigStore::new(&dir);

        // 首次 load（文件不存在）：注入内置全能助手。
        let first = store.load();
        assert_eq!(first.agents.len(), 1);
        let d = &first.agents[0];
        assert_eq!(d.id, DEFAULT_AGENT_ID);
        assert_eq!(d.name, "全能助手");
        assert!(d.provider.is_empty());

        // 老配置文件升级（无 default）：load 自动补上。
        let legacy = br#"{"providers":[],"agents":[]}"#;
        std::fs::write(dir.join("studio-config.json"), legacy).unwrap();
        assert!(store.load().agents.iter().any(|a| a.id == DEFAULT_AGENT_ID));

        // 历史遗留的删除标记（旧版本语义）：同样注入（标记已废弃、被忽略）。
        let marked = br#"{"providers":[],"agents":[],"default_agent_removed":true}"#;
        std::fs::write(dir.join("studio-config.json"), marked).unwrap();
        assert!(store.load().agents.iter().any(|a| a.id == DEFAULT_AGENT_ID));

        // 损坏的配置文件：兜底为空配置 + 注入内置 agent（至少开箱可用）。
        std::fs::write(dir.join("studio-config.json"), b"not json").unwrap();
        assert!(store.load().agents.iter().any(|a| a.id == DEFAULT_AGENT_ID));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 工作区 serde 往返：kind/agent_id/dir/members 全字段保持；
    /// 旧配置文件（无 workspaces 键）serde(default) 兜底为空表。
    #[test]
    fn workspace_config_roundtrip() {
        let ws = WorkspaceConfig {
            id: "ws1".into(),
            name: "我的项目".into(),
            kind: "collab".into(),
            agent_id: DEFAULT_AGENT_ID.into(),
            dir: r"F:\java\my-project".into(),
            members: vec!["agent-b".into()],
            created_at: 42,
        };
        let bytes = serde_json::to_vec(&ws).unwrap();
        let back: WorkspaceConfig = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.kind, "collab");
        assert_eq!(back.agent_id, DEFAULT_AGENT_ID);
        assert_eq!(back.members, vec!["agent-b".to_string()]);
        assert!(back.kind_valid());
        assert!(!WorkspaceConfig {
            kind: "nope".into(),
            id: String::new(),
            name: String::new(),
            agent_id: String::new(),
            dir: String::new(),
            members: Vec::new(),
            created_at: 0,
        }
        .kind_valid());

        let legacy: StudioConfig = serde_json::from_slice(br#"{"providers":[],"agents":[]}"#).unwrap();
        assert!(legacy.workspaces.is_empty());
    }

    /// 数据库资源 serde 往返保持全部字段（含密码——本地单用户桌面应用，
    /// 与 provider api_key 同策略存本地配置文件）。
    #[test]
    fn database_config_roundtrip() {
        let db = DatabaseServerConfig {
            name: "prod-mysql".into(),
            db_type: "mysql".into(),
            host: "10.0.0.8".into(),
            port: 3306,
            database: "orders".into(),
            username: "ro_user".into(),
            password: "secret".into(),
            description: "生产库只读".into(),
        };
        let bytes = serde_json::to_vec(&db).unwrap();
        let back: DatabaseServerConfig = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.name, "prod-mysql");
        assert_eq!(back.port, 3306);
        assert_eq!(back.password, "secret");

        // sqlite：host 复用为文件路径，port/database 可空（serde default 兜底）。
        let raw = br#"{"name":"local","db_type":"sqlite","host":"C:\\data\\studio.db"}"#;
        let lite: DatabaseServerConfig = serde_json::from_slice(raw).unwrap();
        assert_eq!(lite.port, 0);
        assert!(lite.database.is_empty());
    }
}
