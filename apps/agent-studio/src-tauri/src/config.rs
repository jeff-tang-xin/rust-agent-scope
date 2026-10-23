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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// 稳定 id（uuid simple；workspace / 会话目录都用它）。
    pub id: String,
    /// 展示名（同时是 harness agent_id，经 sanitize）。
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// 系统提示（≈ Java Agent 定义里的 instructions）。
    #[serde(default)]
    pub sys_prompt: String,
    /// 工作区模式：[`MODES`] 之一。
    pub mode: String,
    /// 引用的 [`ModelProviderConfig::name`]。
    pub provider: String,
    /// 最大推理迭代数（0 = harness 默认）。
    #[serde(default)]
    pub max_iters: u32,
    /// team 模式的成员声明（leader 之外的成员；对齐 TeamMemberSpec 的
    /// name/prompt 面，agentRef 统一走内联声明）。
    #[serde(default)]
    pub members: Vec<TeamMemberConfig>,
    /// 自定义工作区目录（绝对路径；空 = 默认 `{data_dir}/agents/{id}/workspace`）。
    /// 指定后 agent 的文件工具锚定该目录（Sandboxed 模式，越界路径被拒）——
    /// 可指向真实项目目录让 agent 直接在项目里干活。
    #[serde(default)]
    pub workspace: String,
    #[serde(default)]
    pub created_at: u64,
}

/// team 成员（≈ Java `TeamMemberSpec{name, agentRef, prompt, deployMode}` 的
/// 本地形态：成员即一个内联声明的子代理）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamMemberConfig {
    pub name: String,
    /// 交给 leader 模型看的描述（决定何时委派）。
    #[serde(default)]
    pub description: String,
    /// 成员系统提示正文（≈ AGENTS.md inline body）。
    #[serde(default)]
    pub prompt: String,
}

impl AgentConfig {
    // team 模式的判定走 factory::StudioMode，这里不留重复谓词。
}

/// Studio 全量配置。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StudioConfig {
    #[serde(default)]
    pub providers: Vec<ModelProviderConfig>,
    #[serde(default)]
    pub agents: Vec<AgentConfig>,
}

impl StudioConfig {
    pub fn provider(&self, name: &str) -> Option<&ModelProviderConfig> {
        self.providers.iter().find(|p| p.name == name)
    }

    pub fn agent(&self, id: &str) -> Option<&AgentConfig> {
        self.agents.iter().find(|a| a.id == id)
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
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => StudioConfig::default(),
        }
    }

    pub fn save(&self, config: &StudioConfig) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(config)
            .map_err(|e| format!("serialize config: {e}"))?;
        std::fs::write(&self.path, bytes).map_err(|e| format!("write config: {e}"))
    }
}
