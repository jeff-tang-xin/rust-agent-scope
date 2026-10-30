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
    /// 上下文窗口（token；0/缺省 = 未知）——压缩触发预算 = 窗口 − 20000；
    /// 未配置时回落 160k fallback，条数阈值成为实际触发器。
    #[serde(default)]
    pub context_window: usize,
}

/// Agent 定义（≈ Managed Agent 定义的最小面）。
///
/// **纯智能体定义**：人设 + Provider 绑定。不含工作区目录、类别、team
/// 成员——那些都是工作区的属性（见 [`WorkspaceConfig`]）。一个智能体可被
/// 任意多个工作区选用（不同项目）。
///
/// 默认推理步数：ReAct 循环 reasoning+acting 的最大轮数，耗尽即以
/// MaxIterations 失败收尾。内置智能体统一 80（core 默认 10 对复杂工程
/// 任务偏紧）；0 视为未配置，装配层兜底 [`FALLBACK_MAX_ITERS`]。
pub const DEFAULT_MAX_ITERS: u32 = 80;

/// `max_iters = 0`（未配置）时的装配兜底值。
pub const FALLBACK_MAX_ITERS: u32 = 50;

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
    /// 最大推理迭代数（0 = 装配层兜底 [`FALLBACK_MAX_ITERS`]）。
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
    /// 绑定的服务器档案名列表（[`ServerConfig::name`]）。
    ///
    /// 能力语义：ops 类别默认开放 remote_shell（空 = 不限全部服务器）；
    /// collab 类别默认无远程能力，绑定非空才开放（且只能操作列表内的
    /// 服务器）。ops 类别绑定非空时同样收窄为白名单。
    #[serde(default)]
    pub bound_servers: Vec<String>,
    /// 绑定的数据库档案名列表（[`DatabaseServerConfig::name`]）。
    ///
    /// 能力语义：db 类别默认开放 db_query/db_dml（空 = 不限全部数据库）；
    /// collab 类别默认无数据库能力，绑定非空才开放（且只能访问列表内的
    /// 数据库）。db 类别绑定非空时同样收窄为白名单。
    #[serde(default)]
    pub bound_databases: Vec<String>,
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
    /// 外部 CLI 可执行文件路径（可选，仅 Cli 驱动类型生效）：CLI 不在 PATH
    /// 时手动指定——填可执行文件完整路径直接用，填目录则拼接程序名
    /// （Windows 补 .exe）；留空按 PATH 探测（类似游戏盒子指定游戏地址）。
    #[serde(default)]
    pub cli_path: String,
    #[serde(default)]
    pub description: String,
}

impl DatabaseServerConfig {
    // 连接摘要由前端按 list_database_kinds 的元数据拼装（needs_port/needs_database），
    // 后端不再重复实现一份 endpoint()。
}

/// SSH 服务器档案（ops 工作区的远程资源；remote_shell / SFTP 文件操作都
/// 挂在它上面）。密码仅存本地配置文件（与数据库密码、api_key 同策略），
/// 不进日志与事件；留空则走用户机器上已配置的 ssh agent/默认密钥。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// 仅为本服务器允许旧 RSA/SHA-1 主机签名；默认关闭。
    #[serde(default)]
    pub allow_legacy_rsa: bool,
    /// 展示名（唯一键）。
    pub name: String,
    pub host: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    #[serde(default)]
    pub username: String,
    /// 密码（可空：空 = 依赖本机 ssh agent / 默认密钥）。
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub description: String,
}

fn default_ssh_port() -> u16 {
    22
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
    /// SSH 服务器档案（ops 工作区资源）。
    #[serde(default)]
    pub servers: Vec<ServerConfig>,
}

impl StudioConfig {
    /// 确保内置默认 agent 在列（无条件）：首次初始化、老配置升级、配置损坏
    /// 兜底、甚至历史遗留的删除标记都挡不住它——「不可删除」由后端
    /// delete_agent 拒绝 + 这里兜底共同保证。
    ///
    /// 另做预置提示词指纹迁移：内置智能体的 sys_prompt 仍等于上一版预置
    /// 文本（用户从未编辑）时升级为新版预置；用户自定义过的原样保留。
    pub fn ensure_default_agent(&mut self) {
        match self.agents.iter_mut().find(|a| a.id == DEFAULT_AGENT_ID) {
            Some(agent) if agent.sys_prompt == LEGACY_DEFAULT_AGENT_PROMPT => {
                agent.sys_prompt = default_agent().sys_prompt;
            }
            Some(_) => {}
            None => self.agents.push(default_agent()),
        }
        match self.agents.iter_mut().find(|a| a.id == OPS_AGENT_ID) {
            Some(agent) if agent.sys_prompt == LEGACY_OPS_AGENT_PROMPT => {
                agent.sys_prompt = default_ops_agent().sys_prompt;
            }
            Some(_) => {}
            None => self.agents.push(default_ops_agent()),
        }
        match self.agents.iter_mut().find(|a| a.id == DB_AGENT_ID) {
            Some(agent) if agent.sys_prompt == LEGACY_DB_AGENT_PROMPT => {
                agent.sys_prompt = default_db_agent().sys_prompt;
            }
            Some(_) => {}
            None => self.agents.push(default_db_agent()),
        }
    }

    /// 确保内置 ops/db 工作区在列（幂等）：目录固定在 `{data_dir}/ops`、
    /// `{data_dir}/db`（≈ ~/.ox/ops、~/.ox/db），智能体指向内置运维/数据库
    /// 智能体。用户仍可自建同类别工作区；内置项可改不可删。
    pub fn ensure_default_workspaces(&mut self, data_dir: &std::path::Path) {
        let builtins = [
            (
                BUILTIN_OPS_WS_ID,
                "运维工作区",
                "ops",
                OPS_AGENT_ID,
                data_dir.join("ops"),
            ),
            (
                BUILTIN_DB_WS_ID,
                "数据库运维工作区",
                "db",
                DB_AGENT_ID,
                data_dir.join("db"),
            ),
        ];
        for (id, name, kind, agent_id, dir) in builtins {
            if !self.workspaces.iter().any(|w| w.id == id) {
                self.workspaces.push(WorkspaceConfig {
                    id: id.into(),
                    name: name.into(),
                    kind: kind.into(),
                    agent_id: agent_id.into(),
                    dir: dir.to_string_lossy().into_owned(),
                    members: Vec::new(),
                    // 内置工作区不预绑定：ops/db 按类别默认开放，collab 无远程能力。
                    bound_servers: Vec::new(),
                    bound_databases: Vec::new(),
                    created_at: 0,
                });
            }
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
/// 内置运维智能体（ops 工作区默认绑定）。
pub const OPS_AGENT_ID: &str = "builtin-ops-agent";
/// 内置数据库智能体（db 工作区默认绑定）。
pub const DB_AGENT_ID: &str = "builtin-db-agent";
/// 内置运维工作区（目录 `{data_dir}/ops`）。
pub const BUILTIN_OPS_WS_ID: &str = "builtin-ops";
/// 内置数据库运维工作区（目录 `{data_dir}/db`）。
pub const BUILTIN_DB_WS_ID: &str = "builtin-db";

/// 内置默认智能体的上一版预置提示词（指纹）：用户从未编辑过时升级为新版，
/// 编辑过则原样保留——区分「还是旧默认」与「用户自定义」的唯一手段。
const LEGACY_DEFAULT_AGENT_PROMPT: &str = concat!(
    "你是全能助手，一个通用的工程与效率助手。你可以：\n",
    "- 读写与整理工作区文件（文件工具锚定工作区根，越界路径会被拒绝）；\n",
    "- 用记忆工具（memory_save / memory_get / memory_search）沉淀与检索跨会话的长期信息（记忆统一存放在工作区根的 .ox/ 目录下）；\n",
    "- 保持项目根目录干净：计划、任务清单、笔记、总结等非业务产出统一写入 .ox/ 目录，只有用户要求的业务代码与文档放其他位置；\n",
    "- 先理解再动手：改动代码前先读相关文件，遵循既有风格，做最小必要修改；\n",
    "- 复杂任务先列计划（plan mode），执行后如实汇报做了什么、怎么验证的、还剩什么风险。\n",
    "不确定就说不确定，不编造事实。用中文与用户交流。\n",
    "方法论：单智能体闭环——由你独立完成从理解到验证的全过程。\n",
    "1. 先建立事实：读相关代码、确认现有结构与约定，不凭猜测动手；\n",
    "2. 遵循既有风格：命名、分层、错误处理沿用项目现状，不引入个人偏好的新范式；\n",
    "3. 外科手术式修改：只改必要的地方，不顺手重构无关代码；\n",
    "4. 方案有取舍时先说明选项与理由，再按最优方案实现；\n",
    "5. 改完必须自测（编译/运行/关键路径检查），确认可用再汇报；\n",
    "6. 汇报格式：做了什么 / 关键改动 / 如何验证 / 遗留风险。",
);

/// 内置默认智能体：一个开箱即用的全能型助手（预置默认值的普通智能体，
/// 可改不可删；由 [`StudioConfig::ensure_default_agent`] 保证始终在列）。
/// sys_prompt 只保留身份与能力概述——工具协议、任务闭环、安全红线、
/// 方法论由装配层 `sys_prompt_for` 的内置基座（BASE_PROMPT）与场景层
/// （SOLO_FREE_PROMPT）统一提供，此处不再重复。
fn default_agent() -> AgentConfig {
    AgentConfig {
        id: DEFAULT_AGENT_ID.into(),
        name: "全能助手".into(),
        description: "内置的全能型助手：编码、文件、检索、规划开箱即用。".into(),
        sys_prompt: [
            "你是全能助手，一个通用的工程与效率助手。你可以：",
            "- 读写与整理工作区文件（文件工具锚定工作区根，越界路径会被拒绝）；",
            "- 用记忆工具（memory_save / memory_get / memory_search）沉淀与检索跨会话的长期信息；",
            "- 复杂任务可拆给子代理（agent_spawn）并行处理；计划模式由用户在界面手动开启，开启后先规划、经批准再执行。",
            "不确定就说不确定，不编造事实。用中文与用户交流。",
        ]
        .join("\n"),
        // provider 留空：用户在设置页绑定模型 Provider 后即可对话（进入会话
        // 时会得到明确指引）。
        provider: String::new(),
        max_iters: DEFAULT_MAX_ITERS,
        created_at: 0,
    }
}

/// 内置运维智能体的上一版预置提示词（指纹，用途同上）。
const LEGACY_OPS_AGENT_PROMPT: &str = concat!(
    "你是运维助手，负责远程服务器的日常运维。你可以：\n",
    "- 用 remote_shell 在已连接的远程服务器上执行命令（每次执行都需要用户确认，请把要做什么、为什么说清楚）；\n",
    "- 服务器目录浏览、文件上传/下载/删除由用户在右侧服务器面板操作，你也可以用 remote_shell 完成（ls/cat/df/free 等）；\n",
    "- 用 web_fetch / web_search 查文档与排错资料；\n",
    "- 输出结论时给出关键证据（命令输出摘录），下一步建议明确可执行。\n",
    "谨慎对待破坏性操作（rm、systemctl stop、重启、改配置），执行前向用户说明影响。用中文与用户交流。\n",
    "方法论：单智能体闭环——由你独立完成从了解到验证的全过程。\n",
    "1. 先建立事实：先看服务器现状（系统版本、服务状态、资源占用、相关配置），不凭猜测动手；\n",
    "2. 遵循现状：沿用服务器上已有的部署方式、配置风格与目录约定，不引入个人偏好的新范式；\n",
    "3. 外科手术式变更：只动必须动的地方，不顺手重启/升级/修改无关服务；\n",
    "4. 方案有取舍时先说明选项与风险（停机窗口、回滚方式），再按最优方案执行；\n",
    "5. 变更后必须自测：核对命令输出与服务状态（systemctl status、日志、端口探活），确认可用再汇报；\n",
    "6. 汇报格式：做了什么 / 关键命令与输出摘录 / 如何验证 / 遗留风险与回滚方式。",
);

/// 内置运维智能体：远程服务器运维（remote_shell + SFTP 文件管理 + web 检索）。
/// 每次执行远程命令都会请求用户确认（权限引擎 ask 规则），提示词对齐该语义。
/// sys_prompt 只保留身份与能力概述——协议与方法论由 BASE_PROMPT + OPS_PROMPT
/// 场景层统一提供。
fn default_ops_agent() -> AgentConfig {
    AgentConfig {
        id: OPS_AGENT_ID.into(),
        name: "运维助手".into(),
        description: "内置的远程运维助手：remote_shell 执行、文件上传下载、web 检索。".into(),
        sys_prompt: [
            "你是运维助手，负责远程服务器的日常运维。",
            "- 用 remote_shell 在已连接的远程服务器上执行命令（每次执行需用户确认，说清要做什么、为什么）；",
            "- 服务器目录浏览、文件上传/下载/删除由用户在右侧服务器面板操作，你也可以用 remote_shell 完成（ls/cat/df/free 等）；",
            "- 用 web_fetch / web_search 查文档与排错资料。",
            "不确定就说不确定，不编造命令输出。用中文与用户交流。",
        ]
        .join("\n"),
        provider: String::new(),
        max_iters: DEFAULT_MAX_ITERS,
        created_at: 0,
    }
}

/// 内置数据库智能体的上一版预置提示词（指纹，用途同上）。
const LEGACY_DB_AGENT_PROMPT: &str = concat!(
    "你是数据库助手，负责已连接数据库的查询与变更。你可以：\n",
    "- 用 db_query 执行只读 SQL（SELECT/WITH/EXPLAIN/SHOW），无需确认，适合探查表结构与数据；\n",
    "- 用 db_dml 执行变更（INSERT/UPDATE/DELETE/DDL），每次执行都需要用户确认，请先说明影响范围（表、预计行数）；\n",
    "- 先 db_query 摸清 schema 与数据分布，再提出变更方案，避免盲改；\n",
    "- 输出结论时给出关键数字与口径（schema、过滤条件、行数）。\n",
    "不确定就说不确定，不编造查询结果。用中文与用户交流。\n",
    "方法论：单智能体闭环——由你独立完成从探查到验证的全过程。\n",
    "1. 先建立事实：先 db_query 摸清 schema、索引与数据分布（行数、过滤条件命中率），不凭猜测动手；\n",
    "2. 遵循既有约定：沿用库表的命名、字段口径与既有查询写法，不引入个人偏好的新范式；\n",
    "3. 外科手术式变更：只改必须改的表与行，变更前先评估影响范围（预计行数、关联表），避免盲改；\n",
    "4. 方案有取舍时先说明选项与风险（锁表、事务、回滚方式），再按最优方案执行；\n",
    "5. 变更后必须自测：用 db_query 复查变更结果（行数、抽样核对），确认符合预期再汇报；\n",
    "6. 汇报格式：做了什么 / 关键 SQL 与影响行数 / 如何验证 / 遗留风险与回滚方式。",
);

/// 内置数据库智能体：远程数据库查询与 DML（query 只读免确认，DML 每次确认）。
/// sys_prompt 只保留身份与能力概述——协议与方法论由 BASE_PROMPT + DB_PROMPT
/// 场景层统一提供。
fn default_db_agent() -> AgentConfig {
    AgentConfig {
        id: DB_AGENT_ID.into(),
        name: "数据库助手".into(),
        description: "内置的数据库助手：db_query 只读查询、db_dml 变更（需确认）。".into(),
        sys_prompt: [
            "你是数据库助手，负责已连接数据库的查询与变更。",
            "- 用 db_query 执行只读 SQL（SELECT/WITH/EXPLAIN/SHOW），无需确认，适合探查表结构与数据；",
            "- 用 db_dml 执行变更（INSERT/UPDATE/DELETE/DDL），每次执行需用户确认，先说明影响范围（表、预计行数）。",
            "不确定就说不确定，不编造查询结果。用中文与用户交流。",
        ]
        .join("\n"),
        provider: String::new(),
        max_iters: DEFAULT_MAX_ITERS,
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

    /// 加载配置并保证内置 agent/工作区在列。`data_dir` 给定时同时 seed
    /// 内置 ops/db 工作区（目录挂 `{data_dir}/ops`、`{data_dir}/db`）。
    pub fn load_with_data_dir(&self, data_dir: Option<&std::path::Path>) -> StudioConfig {
        let mut config = match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice::<StudioConfig>(&bytes).unwrap_or_default(),
            // 首次初始化（文件不存在）：注入内置默认 agent，开箱即用。
            Err(_) => StudioConfig::default(),
        };
        config.ensure_default_agent();
        if let Some(dir) = data_dir {
            config.ensure_default_workspaces(dir);
        }
        config
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

    /// 预置提示词指纹迁移：内置智能体仍为旧默认文本 → 升级为新默认；
    /// 用户编辑过 → 原样保留。
    #[test]
    fn builtin_agent_prompt_fingerprint_migration() {
        let mk = |sys_prompt: &str| AgentConfig {
            id: DEFAULT_AGENT_ID.into(),
            name: "全能助手".into(),
            description: String::new(),
            sys_prompt: sys_prompt.into(),
            provider: String::new(),
            max_iters: 0,
            created_at: 0,
        };
        // 旧默认 → 升级为新默认
        let mut config = StudioConfig::default();
        config.agents.push(mk(LEGACY_DEFAULT_AGENT_PROMPT));
        config.ensure_default_agent();
        let upgraded = config.agent(DEFAULT_AGENT_ID).unwrap();
        assert_eq!(upgraded.sys_prompt, default_agent().sys_prompt);
        assert_ne!(upgraded.sys_prompt, LEGACY_DEFAULT_AGENT_PROMPT);
        // 用户自定义 → 保留
        let mut config = StudioConfig::default();
        config.agents.push(mk("我的自定义提示词"));
        config.ensure_default_agent();
        assert_eq!(
            config.agent(DEFAULT_AGENT_ID).unwrap().sys_prompt,
            "我的自定义提示词"
        );
    }

    /// 内置默认 agent：始终在列、不可删除——首次初始化、老配置升级、配置
    /// 损坏兜底、甚至历史遗留的删除标记都注入（delete_agent 已在后端拒绝）。
    #[test]
    fn default_agent_always_present() {
        let dir = std::env::temp_dir().join(format!("studio-cfg-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = ConfigStore::new(&dir);

        // 首次 load（文件不存在）：注入三个内置智能体（全能/运维/数据库）。
        let first = store.load_with_data_dir(None);
        assert_eq!(first.agents.len(), 3);
        let d = first.agent(DEFAULT_AGENT_ID).unwrap();
        assert_eq!(d.name, "全能助手");
        assert!(d.provider.is_empty());
        assert!(first.agent(OPS_AGENT_ID).is_some());
        assert!(first.agent(DB_AGENT_ID).is_some());

        // 老配置文件升级（无 default）：load 自动补上。
        let legacy = br#"{"providers":[],"agents":[]}"#;
        std::fs::write(dir.join("studio-config.json"), legacy).unwrap();
        assert!(store.load_with_data_dir(None).agents.iter().any(|a| a.id == DEFAULT_AGENT_ID));

        // 历史遗留的删除标记（旧版本语义）：同样注入（标记已废弃、被忽略）。
        let marked = br#"{"providers":[],"agents":[],"default_agent_removed":true}"#;
        std::fs::write(dir.join("studio-config.json"), marked).unwrap();
        assert!(store.load_with_data_dir(None).agents.iter().any(|a| a.id == DEFAULT_AGENT_ID));

        // 损坏的配置文件：兜底为空配置 + 注入内置 agent（至少开箱可用）。
        std::fs::write(dir.join("studio-config.json"), b"not json").unwrap();
        assert!(store.load_with_data_dir(None).agents.iter().any(|a| a.id == DEFAULT_AGENT_ID));

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
            bound_servers: vec!["prod-1".into()],
            bound_databases: Vec::new(),
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
            bound_servers: Vec::new(),
            bound_databases: Vec::new(),
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
            cli_path: r"D:\mysql\bin".into(),
            description: "生产库只读".into(),
        };
        let bytes = serde_json::to_vec(&db).unwrap();
        let back: DatabaseServerConfig = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.name, "prod-mysql");
        assert_eq!(back.port, 3306);
        assert_eq!(back.password, "secret");
        assert_eq!(back.cli_path, r"D:\mysql\bin");

        // sqlite：host 复用为文件路径，port/database 可空（serde default 兜底）。
        let raw = br#"{"name":"local","db_type":"sqlite","host":"C:\\data\\studio.db"}"#;
        let lite: DatabaseServerConfig = serde_json::from_slice(raw).unwrap();
        assert_eq!(lite.port, 0);
        assert!(lite.database.is_empty());
        // 旧配置无 cli_path 键：serde default 兜底为空（按 PATH 探测）。
        assert!(lite.cli_path.is_empty());
    }
}
