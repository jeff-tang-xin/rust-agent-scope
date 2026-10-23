//! 子代理：运行时 workspace 决策、声明、frontmatter 加载与管理器
//! （对应 Java `subagent` 包的 `WorkspaceMode` / `SubagentDeclaration` /
//! `AgentSpecLoader` / `SubagentFactory` / `DefaultAgentManager`）。
//!
//! 远端子代理（`url` 模式）本批只保留声明字段位；transport 与
//! `RemoteStreamDetail` / `RemoteAskPolicy` 随分布式批次补齐。

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use agentscope_core::agent::ReActAgent;
use agentscope_core::context::RuntimeContext;
use serde::{Deserialize, Serialize};

use crate::workspace::WorkspacePathNormalizer;

/// 声明式子代理的运行时 workspace 决策（对应 Java `WorkspaceMode`）。
///
/// 五行决策表（`workspace_path` = 声明里的 `workspace.path`）：
///
/// ```text
/// workspace_path  mode      运行时 workspace 根
/// ─────────────────────────────────────────────────────────────────
/// 有              ISOLATED  workspace_path（定义目录兼运行时根）
/// 有              SHARED    main_workspace（定义目录的 skills/knowledge 忽略）
/// 无              ISOLATED  main_workspace/agents/<name>/workspace/（自动创建）
/// 无              SHARED    main_workspace
/// （general-purpose 恒为 SHARED）main_workspace
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum WorkspaceMode {
    /// 子代理拥有独立工作区（默认）。
    #[default]
    Isolated,
    /// 子代理与主代理共享工作区。
    Shared,
}

impl WorkspaceMode {
    /// 决策运行时 workspace 根（对应 Java 五行决策表）。
    pub fn resolve_workspace_root(
        &self,
        workspace_path: Option<&str>,
        main_workspace: &str,
        agent_name: &str,
    ) -> String {
        match self {
            WorkspaceMode::Isolated => match workspace_path {
                Some(path) => path.to_string(),
                None => format!("{main_workspace}/agents/{agent_name}/workspace"),
            },
            WorkspaceMode::Shared => main_workspace.to_string(),
        }
    }

    /// Java wire 值。
    pub fn as_str(self) -> &'static str {
        match self {
            WorkspaceMode::Isolated => "ISOLATED",
            WorkspaceMode::Shared => "SHARED",
        }
    }
}

/// 声明的使用方式（对应 Java `SubagentDeclaration.Mode`）。
///
/// `PRIMARY`-only 的声明不能被 spawn 成 worker（[`DefaultAgentManager`]
/// 的 `create_agent_if_present` 会拒绝）；顶层启动不走声明，因此
/// `SUBAGENT`-only 的入口检查由启动方自行负责。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DeclarationMode {
    /// 只能作为顶层主代理。
    Primary,
    /// 只能作为被委派的子代理。
    Subagent,
    /// 两者皆可（默认）。
    #[default]
    All,
}

impl DeclarationMode {
    /// Java wire 值。
    pub fn as_str(self) -> &'static str {
        match self {
            DeclarationMode::Primary => "PRIMARY",
            DeclarationMode::Subagent => "SUBAGENT",
            DeclarationMode::All => "ALL",
        }
    }
}

/// 子代理声明：身份、workspace 决策策略与可选能力白名单
/// （对应 Java `SubagentDeclaration`）。
///
/// 三种来源互斥：`workspace_path`（定义工作区，至少含 `AGENTS.md`）、
/// `inline_agents_body`（内联系统提示）、`url`（远端任务服务器）——
/// 至多设置其一，[`SubagentDeclaration::validate`] 强制校验。
///
/// `tools` 非空时作为**继承父工具的白名单过滤器**：只有列名的继承工具保留；
/// 子代理本地注册的工具不受影响。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentDeclaration {
    /// 唯一名 / agent-id。
    pub name: String,
    /// 交给主模型看的描述（决定何时委派）。
    pub description: String,
    /// workspace 决策策略（默认 [`WorkspaceMode::Isolated`]）。
    #[serde(default)]
    pub workspace_mode: WorkspaceMode,
    /// 定义工作区目录（至少含 `AGENTS.md`）；与 `inline_agents_body` 互斥。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_path: Option<String>,
    /// 内联系统提示正文（`workspace_path` 为空时使用）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline_agents_body: Option<String>,
    /// 模型覆盖（空则继承父模型）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 采样温度覆盖（空则继承父值）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// nucleus 采样覆盖（空则继承父值）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// 模型变体标识（如 DashScope thinking 模式）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    /// 最大推理迭代数（默认 10）。
    #[serde(default = "default_steps")]
    pub steps: i32,
    /// 声明使用方式（默认 [`DeclarationMode::All`]）。
    #[serde(default)]
    pub mode: DeclarationMode,
    /// 是否对 LLM 隐藏（内部子代理如压缩/摘要/标题）。
    #[serde(default)]
    pub hidden: bool,
    /// 会话状态是否跨父调用持久化（true 时 spawn 键由身份确定性派生）。
    #[serde(default)]
    pub persist_session: bool,
    /// 是否继承父代理的 DENY 权限规则（默认 true）。
    #[serde(default = "default_true")]
    pub inherit_parent_permissions: bool,
    /// 是否把 spawn 的实例暴露为用户可寻址线程（tri-state：None = 交给运行时决定）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expose_to_user: Option<bool>,
    /// 孤儿工具调用恢复策略（tri-state：None = 继承父设置）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_pending_tool_recovery: Option<bool>,
    /// 继承父工具的白名单（空 = 全部继承）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    /// 远端任务服务器 base URL（与 workspace/inline 互斥；transport 随分布式批次）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// 远端请求头（如鉴权）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
}

fn default_steps() -> i32 {
    10
}

fn default_true() -> bool {
    true
}

impl SubagentDeclaration {
    /// 最小声明起点（name/description 必填，其余默认）。
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            workspace_mode: WorkspaceMode::Isolated,
            workspace_path: None,
            inline_agents_body: None,
            model: None,
            temperature: None,
            top_p: None,
            variant: None,
            steps: default_steps(),
            mode: DeclarationMode::All,
            hidden: false,
            persist_session: false,
            inherit_parent_permissions: true,
            expose_to_user: None,
            enable_pending_tool_recovery: None,
            tools: Vec::new(),
            skills: Vec::new(),
            url: None,
            headers: None,
        }
    }

    pub fn with_workspace_mode(mut self, mode: WorkspaceMode) -> Self {
        self.workspace_mode = mode;
        self
    }

    /// 指向外部定义工作区（与 [`Self::with_inline_agents_body`] 互斥）。
    pub fn with_workspace_path(mut self, path: impl Into<String>) -> Self {
        self.workspace_path = Some(path.into());
        self
    }

    /// 设置内联系统提示（与 [`Self::with_workspace_path`] 互斥）。
    pub fn with_inline_agents_body(mut self, body: impl Into<String>) -> Self {
        self.inline_agents_body = Some(body.into());
        self
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn with_temperature(mut self, temperature: f64) -> Self {
        self.temperature = Some(temperature);
        self
    }

    pub fn with_top_p(mut self, top_p: f64) -> Self {
        self.top_p = Some(top_p);
        self
    }

    pub fn with_variant(mut self, variant: impl Into<String>) -> Self {
        self.variant = Some(variant.into());
        self
    }

    /// 最大推理迭代数（默认 10）。
    pub fn with_steps(mut self, steps: i32) -> Self {
        self.steps = steps;
        self
    }

    pub fn with_mode(mut self, mode: DeclarationMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn with_hidden(mut self, hidden: bool) -> Self {
        self.hidden = hidden;
        self
    }

    pub fn with_persist_session(mut self, persist: bool) -> Self {
        self.persist_session = persist;
        self
    }

    pub fn with_inherit_parent_permissions(mut self, inherit: bool) -> Self {
        self.inherit_parent_permissions = inherit;
        self
    }

    pub fn with_expose_to_user(mut self, expose: Option<bool>) -> Self {
        self.expose_to_user = expose;
        self
    }

    pub fn with_enable_pending_tool_recovery(mut self, enable: Option<bool>) -> Self {
        self.enable_pending_tool_recovery = enable;
        self
    }

    /// 继承父工具的白名单（空 = 全部继承）。
    pub fn with_tools(mut self, tools: Vec<String>) -> Self {
        self.tools = tools;
        self
    }

    pub fn with_skills(mut self, skills: Vec<String>) -> Self {
        self.skills = skills;
        self
    }

    /// 指向远端任务服务器（与 workspace/inline 互斥）。
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }

    pub fn with_headers(mut self, headers: HashMap<String, String>) -> Self {
        self.headers = Some(headers);
        self
    }

    /// 是否远端模式。
    pub fn is_remote(&self) -> bool {
        self.url.as_deref().is_some_and(|url| !url.trim().is_empty())
    }

    /// 是否指向外部定义工作区。
    pub fn has_definition_workspace(&self) -> bool {
        self.workspace_path.is_some()
    }

    /// 声明校验（对应 Java `Builder.build()` 的参数检查）：
    /// name/description 非空；workspace / inline / url 三源互斥。
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("SubagentDeclaration requires a non-blank name".to_string());
        }
        if self.description.trim().is_empty() {
            return Err(format!(
                "SubagentDeclaration requires a non-blank description for '{}'",
                self.name
            ));
        }
        let inline_set = self
            .inline_agents_body
            .as_deref()
            .is_some_and(|body| !body.trim().is_empty());
        if self.is_remote() {
            if self.workspace_path.is_some() {
                return Err(format!(
                    "url() and workspace(Path) are mutually exclusive for subagent '{}'",
                    self.name
                ));
            }
            if inline_set {
                return Err(format!(
                    "url() and inlineAgentsBody() are mutually exclusive for subagent '{}'",
                    self.name
                ));
            }
        } else if self.workspace_path.is_some() && inline_set {
            return Err(format!(
                "workspace(Path) and inlineAgentsBody() are mutually exclusive; \
                 set at most one for subagent '{}'",
                self.name
            ));
        }
        Ok(())
    }
}

/// 子代理工厂：按父运行时上下文创建新的 agent 实例
/// （对应 Java `SubagentFactory` 函数式接口）。
///
/// 直接产出 [`ReActAgent`]——当前唯一的 `Agent` 实现，不建 trait 抽象
/// （反模式禁令②）。实现方应把父身份（user/session）烘进子代理的
/// 状态隔离键，使 `AgentState` 按 (user, parent-session) 隔离。
pub type SubagentFactory =
    dyn Fn(&RuntimeContext) -> Result<Arc<ReActAgent>, String> + Send + Sync;

/// 管理器里的一个子代理条目：工厂 + 可选声明元数据。
#[derive(Clone)]
pub struct SubagentEntry {
    pub name: String,
    pub declaration: Option<SubagentDeclaration>,
    pub factory: Arc<SubagentFactory>,
}

impl SubagentEntry {
    /// 工厂 + 声明齐全的条目。
    pub fn new(
        name: impl Into<String>,
        declaration: Option<SubagentDeclaration>,
        factory: Arc<SubagentFactory>,
    ) -> Self {
        Self {
            name: name.into(),
            declaration,
            factory,
        }
    }
}

/// 默认子代理管理器：纯工厂与调用层——没有会话注册表、没有 lane 管理
/// （对应 Java `DefaultAgentManager`）。
///
/// 注册表整体快照替换（对应 Java 的 volatile 引用换快照）：并发读者要么看到
/// 旧快照要么看到新快照，不会看到半新半旧。
pub struct DefaultAgentManager {
    entries: Mutex<HashMap<String, SubagentEntry>>,
    normalizer: WorkspacePathNormalizer,
}

impl DefaultAgentManager {
    /// 空管理器（无路径规范化）。
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            normalizer: WorkspacePathNormalizer::default(),
        }
    }

    /// 带路径规范化器的管理器（把宿主绝对路径归一为工作区相对路径）。
    pub fn with_normalizer(normalizer: WorkspacePathNormalizer) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            normalizer,
        }
    }

    /// 路径规范化器。
    pub fn normalizer(&self) -> &WorkspacePathNormalizer {
        &self.normalizer
    }

    /// 注册单个子代理；名字重复或声明校验失败时报错。
    pub fn register(&self, entry: SubagentEntry) -> Result<(), String> {
        if entry.name.trim().is_empty() {
            return Err("Subagent name must not be blank".to_string());
        }
        if let Some(declaration) = &entry.declaration {
            declaration.validate()?;
        }
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if entries.contains_key(&entry.name) {
            return Err(format!("Subagent '{}' is already registered", entry.name));
        }
        entries.insert(entry.name.clone(), entry);
        Ok(())
    }

    /// 用新条目列表整体替换注册表（对应 Java `refreshEntries`）。
    pub fn refresh_entries(&self, entries: Vec<SubagentEntry>) {
        let mut map = HashMap::new();
        for entry in entries {
            map.insert(entry.name.clone(), entry);
        }
        let mut guard = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        *guard = map;
    }

    /// [`Self::refresh_entries`] 的别名（对应 Java `replaceAgents`，
    /// 供动态子代理中间件在每个 reasoning 步原子换装）。
    pub fn replace_agents(&self, entries: Vec<SubagentEntry>) {
        self.refresh_entries(entries);
    }

    /// 是否存在该名字的工厂。
    pub fn has_agent(&self, name: &str) -> bool {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(name)
    }

    /// 该名字是否注册为 `PRIMARY`-only 声明（用于给出更友好的错误信息）。
    pub fn is_primary_only(&self, name: &str) -> bool {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .and_then(|entry| entry.declaration.as_ref())
            .is_some_and(|declaration| declaration.mode == DeclarationMode::Primary)
    }

    /// 全部注册名（字典序）。
    pub fn list_agent_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    }

    /// 取声明（用于把子代理清单拼进主代理系统提示）。
    pub fn get_declaration(&self, name: &str) -> Option<SubagentDeclaration> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .and_then(|entry| entry.declaration.clone())
    }

    /// 竞态安全的查找并创建（对应 Java `createAgentIfPresent`）：
    /// 未注册、或声明为 `PRIMARY`-only（不能被 spawn 成 worker）时返回 `Err`。
    pub fn create_agent_if_present(
        &self,
        rc: &RuntimeContext,
        agent_id: &str,
    ) -> Result<Arc<ReActAgent>, String> {
        let factory = {
            let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            // 未注册 → Unknown（未注册的 agent 不可能有 PRIMARY 声明，
            // 因此不需要在这里查 is_primary_only——那会在持锁期间再锁同一把 Mutex，自死锁）
            let entry = entries
                .get(agent_id)
                .ok_or_else(|| format!("Unknown agent_id: {agent_id}"))?;
            if entry
                .declaration
                .as_ref()
                .is_some_and(|declaration| declaration.mode == DeclarationMode::Primary)
            {
                return Err(format!(
                    "Agent '{agent_id}' is PRIMARY-only and cannot be spawned"
                ));
            }
            entry.factory.clone()
        };
        factory(rc)
    }

    /// 调用子代理：创建实例 → child context → 执行 → 返回最终回复文本
    /// （对应 Java `invokeAgent`）。
    ///
    /// child context 换新 session id、继承 user id；父会话的 recorder /
    /// execution 键不随行（Rust 侧 `RuntimeContext` 按值构建，天然隔离），
    /// 父链信息写入 `agentscope_parent` 属性供子代理侧追溯。
    pub async fn invoke_agent(
        &self,
        rc: &RuntimeContext,
        agent_id: &str,
        message: agentscope_core::message::Msg,
    ) -> Result<String, String> {
        let agent = self.create_agent_if_present(rc, agent_id)?;
        let child = child_context(rc, agent_id);
        let reply = agent
            .call_with_context(child, message)
            .await
            .map_err(|error| error.to_string())?;
        Ok(reply.text())
    }
}

impl Default for DefaultAgentManager {
    fn default() -> Self {
        Self::new()
    }
}

/// 构造子代理的 child 运行时上下文（对应 Java `childContext`）：
/// 新 session id（`<agentId>-<uuid>`）、继承 user id、父链写属性。
fn child_context(parent: &RuntimeContext, agent_id: &str) -> RuntimeContext {
    let child = RuntimeContext::new()
        .with_user_id(parent.user_id().unwrap_or("").to_string())
        .with_session_id(format!("{agent_id}-{}", uuid::Uuid::new_v4().simple()));
    let parent_chain = serde_json::json!({
        "parentAgentId": agent_id,
        "parentSessionId": parent.session_id().unwrap_or(""),
        "parentReplyId": parent.reply_id(),
    });
    child.put("agentscope_parent", parent_chain);
    child
}

/// 从 Markdown frontmatter 加载 [`SubagentDeclaration`]
/// （对应 Java `AgentSpecLoader`）。
///
/// 文件名（去 `.md`）即子代理名；frontmatter 不得含 `name` 字段。
/// 解析规则与 Java 对齐：
/// - `description` 必填，缺失视为 malformed（调用方跳过该文件）；
/// - `workspace.path` 存在时正文必须为空，否则忽略正文（系统提示从
///   `<path>/AGENTS.md` 读取）；
/// - `steps` 优先于废弃别名 `maxIters`，默认 10；
/// - `mode` 是声明使用方式（primary/subagent/all），workspace 模式在
///   `workspace.mode` 下；
/// - `top_p`/`topP`、`expose_to_user`/`exposeToUser` 双写法兼容；
/// - 未知键忽略（与 Java 宽松解析一致）。
///
/// frontmatter 用手写受限解析器（扁平 `key: value`、一层 `workspace.` 嵌套、
/// `[a, b]` 行内列表与 `- item` 块列表），不引入 YAML 依赖。
pub struct AgentSpecLoader {
    normalizer: WorkspacePathNormalizer,
}

impl AgentSpecLoader {
    /// 用工作区根构建（把宿主绝对路径归一为工作区相对路径）。
    pub fn new(workspace_root: impl Into<String>) -> Self {
        Self {
            normalizer: WorkspacePathNormalizer::of(workspace_root),
        }
    }

    /// 用自定义规范化器构建。
    pub fn with_normalizer(normalizer: WorkspacePathNormalizer) -> Self {
        Self { normalizer }
    }

    /// 非递归扫描目录下的 `*.md` 并逐个解析；malformed 文件跳过
    /// （对应 Java `loadFromDirectory` 的 warn-and-skip）。
    pub fn load_from_directory(
        &self,
        dir: impl AsRef<Path>,
    ) -> Vec<SubagentDeclaration> {
        let mut declarations = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir.as_ref()) else {
            return declarations;
        };
        let mut files: Vec<_> = entries
            .flatten()
            .filter(|entry| entry.path().is_file())
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|ext| ext == "md")
            })
            .collect();
        files.sort_by_key(|entry| entry.file_name());
        for entry in files {
            let path = entry.path();
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            match self.parse(file_name, &content) {
                Ok(declaration) => declarations.push(declaration),
                Err(error) => eprintln!(
                    "Failed to load subagent declaration from {}: {error}",
                    path.display()
                ),
            }
        }
        declarations
    }

    /// 解析单个 `.md` 声明（frontmatter + 正文）。
    pub fn parse(&self, file_name: &str, content: &str) -> Result<SubagentDeclaration, String> {
        let name = strip_md_extension(file_name);
        if name.is_empty() {
            return Err("Declaration file name must not be blank".to_string());
        }
        let (frontmatter, body) = split_frontmatter(content)?;
        let fields = parse_frontmatter(&frontmatter)?;

        let description = lookup(&fields, "description")
            .map(frontmatter_string)
            .unwrap_or_default();
        if description.trim().is_empty() {
            return Err(format!(
                "Subagent declaration '{name}' missing required 'description' in front matter"
            ));
        }

        // ---- workspace 段 ----
        let mut workspace_mode = WorkspaceMode::Isolated;
        let mut workspace_path: Option<String> = None;
        if let Some(mode) = lookup(&fields, "workspace.mode") {
            match frontmatter_string(mode).to_ascii_lowercase().as_str() {
                "shared" => workspace_mode = WorkspaceMode::Shared,
                "isolated" => workspace_mode = WorkspaceMode::Isolated,
                other => {
                    return Err(format!(
                        "Unknown workspace.mode '{other}' in declaration '{name}'"
                    ));
                }
            }
        }
        if let Some(path) = lookup(&fields, "workspace.path") {
            let raw = frontmatter_string(path);
            if !raw.trim().is_empty() {
                workspace_path = Some(self.normalizer.normalize(raw.trim()));
            }
        }

        // ---- 正文 / 内联系统提示 ----
        // workspace.path 存在时正文必须为空；非空则忽略正文（Java 侧告警后忽略）。
        let body = body.trim().to_string();
        let inline_agents_body =
            if workspace_path.is_some() || body.is_empty() { None } else { Some(body) };

        // ---- 可选字段 ----
        let model = lookup(&fields, "model")
            .map(frontmatter_string)
            .filter(|value| !value.trim().is_empty());
        // steps（优先）> maxIters（废弃别名），默认 10。
        let steps = lookup(&fields, "steps")
            .or_else(|| lookup(&fields, "maxIters"))
            .and_then(|value| frontmatter_string(value).trim().parse::<i32>().ok())
            .unwrap_or(10);
        let temperature = lookup(&fields, "temperature")
            .and_then(|value| frontmatter_string(value).trim().parse::<f64>().ok());
        let top_p = lookup(&fields, "top_p")
            .or_else(|| lookup(&fields, "topP"))
            .and_then(|value| frontmatter_string(value).trim().parse::<f64>().ok());
        let variant = lookup(&fields, "variant")
            .map(frontmatter_string)
            .filter(|value| !value.trim().is_empty());
        let mode = lookup(&fields, "mode")
            .map(|value| parse_declaration_mode(&frontmatter_string(value), &name))
            .unwrap_or(DeclarationMode::All);
        let hidden = lookup(&fields, "hidden")
            .map(|value| parse_bool(&frontmatter_string(value)))
            .unwrap_or(false);
        let expose_to_user = lookup(&fields, "expose_to_user")
            .or_else(|| lookup(&fields, "exposeToUser"))
            .and_then(|value| parse_nullable_bool(&frontmatter_string(value)));
        let enable_pending_tool_recovery = lookup(&fields, "enable_pending_tool_recovery")
            .or_else(|| lookup(&fields, "enablePendingToolRecovery"))
            .and_then(|value| parse_nullable_bool(&frontmatter_string(value)));
        let tools = lookup(&fields, "tools")
            .map(|value| parse_tool_names(&frontmatter_string(value)))
            .unwrap_or_default();
        let skills = lookup(&fields, "skills")
            .map(|value| parse_tool_names(&frontmatter_string(value)))
            .unwrap_or_default();

        let mut declaration = SubagentDeclaration::new(name, description)
            .with_workspace_mode(workspace_mode)
            .with_steps(steps)
            .with_mode(mode)
            .with_hidden(hidden)
            .with_expose_to_user(expose_to_user)
            .with_enable_pending_tool_recovery(enable_pending_tool_recovery)
            .with_tools(tools)
            .with_skills(skills);
        if let Some(path) = workspace_path {
            declaration = declaration.with_workspace_path(path);
        }
        if let Some(body) = inline_agents_body {
            declaration = declaration.with_inline_agents_body(body);
        }
        if let Some(model) = model {
            declaration = declaration.with_model(model);
        }
        if let Some(temperature) = temperature {
            declaration = declaration.with_temperature(temperature);
        }
        if let Some(top_p) = top_p {
            declaration = declaration.with_top_p(top_p);
        }
        if let Some(variant) = variant {
            declaration = declaration.with_variant(variant);
        }
        declaration.validate()?;
        Ok(declaration)
    }
}

/// frontmatter 值：标量或块列表。
#[derive(Debug, Clone)]
pub(crate) enum FrontmatterValue {
    Scalar(String),
    List(Vec<String>),
}

/// 取标量；列表值折叠为逗号串（交给下游 `parse_tool_names` 再拆）。
pub(crate) fn frontmatter_string(value: &FrontmatterValue) -> String {
    match value {
        FrontmatterValue::Scalar(text) => text.clone(),
        FrontmatterValue::List(items) => format!("[{}]", items.join(", ")),
    }
}

/// 拆出 `---` 包裹的 frontmatter 与正文；无 frontmatter 时正文即全部。
pub(crate) fn split_frontmatter(content: &str) -> Result<(String, String), String> {
    let trimmed_start = content.trim_start_matches('\u{feff}');
    let mut lines = trimmed_start.lines();
    let first = lines.next().unwrap_or("").trim();
    if first != "---" {
        return Ok((String::new(), trimmed_start.to_string()));
    }
    let mut frontmatter = Vec::new();
    let mut rest = Vec::new();
    let mut in_frontmatter = true;
    for line in lines {
        if in_frontmatter {
            if line.trim() == "---" {
                in_frontmatter = false;
                continue;
            }
            frontmatter.push(line);
        } else {
            rest.push(line);
        }
    }
    if in_frontmatter {
        return Err("Unterminated frontmatter (missing closing '---')".to_string());
    }
    Ok((frontmatter.join("\n"), rest.join("\n")))
}

/// 解析受限 frontmatter：`key: value`、一层缩进嵌套（`workspace:` 下的
/// `mode:`/`path:` 记为 `workspace.mode`/`workspace.path`）、`- item` 块列表。
/// 空值键（如 `workspace:`）只作为嵌套前缀，本身不落库。
pub(crate) fn parse_frontmatter(frontmatter: &str) -> Result<Vec<(String, FrontmatterValue)>, String> {
    let mut fields: Vec<(String, FrontmatterValue)> = Vec::new();
    // 最近一个空值键（嵌套前缀，如 "workspace"）
    let mut pending_parent: Option<String> = None;
    // 挂起块列表的键
    let mut list_key: Option<String> = None;
    let mut list_items: Vec<String> = Vec::new();
    for (index, raw_line) in frontmatter.lines().enumerate() {
        let line_no = index + 1;
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(item) = trimmed.strip_prefix("- ") {
            if list_key.is_none() {
                return Err(format!(
                    "Frontmatter line {line_no}: list item without a preceding key"
                ));
            }
            list_items.push(item.trim().trim_matches('"').to_string());
            continue;
        }
        // 新 key 出现：先把挂起的块列表落库
        if let Some(key) = list_key.take() {
            if !list_items.is_empty() {
                fields.push((key, FrontmatterValue::List(std::mem::take(&mut list_items))));
            }
        }
        let indent = raw_line.len() - raw_line.trim_start().len();
        let Some((key, value)) = trimmed.split_once(':') else {
            return Err(format!("Frontmatter line {line_no}: expected 'key: value'"));
        };
        let key = key.trim().to_string();
        let value = value.trim();
        // 一层缩进嵌套：挂到最近空值键下（`workspace:` + `  mode:` → workspace.mode）
        let qualified = if indent > 0 {
            match &pending_parent {
                Some(parent) => format!("{parent}.{key}"),
                None => key.clone(),
            }
        } else {
            // 新的顶层键：清掉嵌套前缀
            pending_parent = None;
            key.clone()
        };
        if value.is_empty() {
            // 空值键：既挂起块列表（`tools:` + `- item`），也挂起嵌套前缀
            // （`workspace:` + `  mode:`）。列表项为空时落库阶段自然丢弃。
            pending_parent = Some(qualified.clone());
            list_key = Some(qualified);
            continue;
        }
        fields.push((
            qualified,
            FrontmatterValue::Scalar(value.trim_matches('"').to_string()),
        ));
    }
    if let Some(key) = list_key.take() {
        if !list_items.is_empty() {
            fields.push((key, FrontmatterValue::List(list_items)));
        }
    }
    Ok(fields)
}

/// 按键取值（重复键取首个非空）。
fn lookup<'a>(
    fields: &'a [(String, FrontmatterValue)],
    key: &str,
) -> Option<&'a FrontmatterValue> {
    fields
        .iter()
        .filter(|(name, _)| name == key)
        .find(|(_, value)| !frontmatter_string(value).trim().is_empty())
        .map(|(_, value)| value)
}

fn parse_declaration_mode(raw: &str, name: &str) -> DeclarationMode {
    match raw.trim().to_ascii_lowercase().as_str() {
        "primary" => DeclarationMode::Primary,
        "subagent" => DeclarationMode::Subagent,
        "all" => DeclarationMode::All,
        _ => {
            eprintln!(
                "Unknown mode '{raw}' in subagent declaration '{name}', defaulting to all"
            );
            DeclarationMode::All
        }
    }
}

fn parse_bool(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "true" | "yes" | "1"
    )
}

/// tri-state 布尔：空串 = 未表态（None），否则解析值。
fn parse_nullable_bool(raw: &str) -> Option<bool> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(parse_bool(trimmed))
}

/// 工具名解析（对应 Java `parseToolNames`）：逗号分隔或 `[a, b]` 形式。
fn parse_tool_names(raw: &str) -> Vec<String> {
    let mut inner = raw.trim();
    if inner.starts_with('[') && inner.ends_with(']') {
        inner = &inner[1..inner.len() - 1];
    }
    inner
        .split(',')
        .map(|part| part.trim().trim_matches('"').to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

/// `code-reviewer.md` → `code-reviewer`（对应 Java `stripMdExtension`）。
fn strip_md_extension(file_name: &str) -> String {
    file_name
        .trim()
        .strip_suffix(".md")
        .unwrap_or_else(|| file_name.trim())
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn stub_factory() -> Arc<SubagentFactory> {
        Arc::new(|_rc| Err("stub factory".to_string()))
    }

    #[test]
    fn workspace_mode_decision_table() {
        // 有 path + ISOLATED → path 即根
        assert_eq!(
            WorkspaceMode::Isolated.resolve_workspace_root(Some("defs/reviewer"), "main", "r"),
            "defs/reviewer"
        );
        // 有 path + SHARED → main workspace（定义目录资源忽略）
        assert_eq!(
            WorkspaceMode::Shared.resolve_workspace_root(Some("defs/reviewer"), "main", "r"),
            "main"
        );
        // 无 path + ISOLATED → main/agents/<name>/workspace
        assert_eq!(
            WorkspaceMode::Isolated.resolve_workspace_root(None, "main", "reviewer"),
            "main/agents/reviewer/workspace"
        );
        // 无 path + SHARED → main workspace
        assert_eq!(
            WorkspaceMode::Shared.resolve_workspace_root(None, "main", "reviewer"),
            "main"
        );
    }

    #[test]
    fn declaration_rejects_conflicting_sources() {
        let declaration =
            SubagentDeclaration::new("a", "d").with_workspace_path("ws").with_url("http://x");
        assert!(declaration.validate().is_err());

        let declaration = SubagentDeclaration::new("a", "d")
            .with_workspace_path("ws")
            .with_inline_agents_body("body");
        assert!(declaration.validate().is_err());

        let declaration =
            SubagentDeclaration::new("a", "d").with_url("http://x").with_inline_agents_body("b");
        assert!(declaration.validate().is_err());

        let ok = SubagentDeclaration::new("a", "d").with_workspace_path("ws");
        assert!(ok.validate().is_ok());
        assert!(ok.has_definition_workspace());
        assert!(!ok.is_remote());

        let remote = SubagentDeclaration::new("a", "d").with_url("http://x");
        assert!(remote.validate().is_ok());
        assert!(remote.is_remote());

        let blank = SubagentDeclaration::new("  ", "d");
        assert!(blank.validate().is_err());
    }

    #[test]
    fn spec_loader_parses_full_frontmatter() {
        let loader = AgentSpecLoader::new("F:/main-ws");
        let markdown = "---\ndescription: Reviews code for security issues.\nmode: subagent\nhidden: true\nmodel: qwen3-max\nsteps: 12\ntemperature: 0.2\ntopP: 0.9\nvariant: thinking\nexpose_to_user: false\nenable_pending_tool_recovery: true\ntools: [read_file, grep_files, edit_file]\nworkspace:\n  mode: isolated\n  path: ./defs/reviewer\n---\n\nBody must be ignored when workspace.path is present.\n";
        let declaration = loader.parse("code-reviewer.md", markdown).unwrap();
        assert_eq!(declaration.name, "code-reviewer");
        assert_eq!(declaration.description, "Reviews code for security issues.");
        assert_eq!(declaration.mode, DeclarationMode::Subagent);
        assert!(declaration.hidden);
        assert_eq!(declaration.model.as_deref(), Some("qwen3-max"));
        assert_eq!(declaration.steps, 12);
        assert_eq!(declaration.temperature, Some(0.2));
        assert_eq!(declaration.top_p, Some(0.9));
        assert_eq!(declaration.variant.as_deref(), Some("thinking"));
        assert_eq!(declaration.expose_to_user, Some(false));
        assert_eq!(declaration.enable_pending_tool_recovery, Some(true));
        assert_eq!(declaration.tools, vec!["read_file", "grep_files", "edit_file"]);
        assert_eq!(declaration.workspace_mode, WorkspaceMode::Isolated);
        // 相对 path 以 main workspace 为根解析（normalizer 未命中前缀则原样保留）
        assert_eq!(declaration.workspace_path.as_deref(), Some("./defs/reviewer"));
        // workspace.path 存在 → 正文被忽略
        assert!(declaration.inline_agents_body.is_none());
    }

    #[test]
    fn spec_loader_inline_body_and_block_list() {
        let loader = AgentSpecLoader::new("F:/main-ws");
        let markdown = "---\ndescription: Quick helper.\ntools:\n  - read_file\n  - write_file\n---\n\nYou are a helper.\n";
        let declaration = loader.parse("helper.md", markdown).unwrap();
        assert_eq!(declaration.tools, vec!["read_file", "write_file"]);
        assert_eq!(declaration.inline_agents_body.as_deref(), Some("You are a helper."));
        assert_eq!(declaration.steps, 10);
        assert_eq!(declaration.workspace_mode, WorkspaceMode::Isolated);
    }

    #[test]
    fn spec_loader_max_iters_alias_and_missing_description() {
        let loader = AgentSpecLoader::new("F:/main-ws");
        let markdown = "---\ndescription: d\nmaxIters: 7\n---\nbody";
        let declaration = loader.parse("legacy.md", markdown).unwrap();
        assert_eq!(declaration.steps, 7);

        // description 缺失 → malformed（调用方跳过该文件）
        let markdown = "---\nmodel: qwen\n---\nbody";
        assert!(loader.parse("bad.md", markdown).is_err());
        // frontmatter 未闭合
        assert!(loader.parse("bad.md", "---\ndescription: d\nbody").is_err());
    }

    #[test]
    fn manager_registration_and_primary_rejection() {
        let manager = DefaultAgentManager::new();
        let declaration = SubagentDeclaration::new("reviewer", "Reviews code");
        manager
            .register(SubagentEntry::new("reviewer", Some(declaration), stub_factory()))
            .unwrap();
        let primary =
            SubagentDeclaration::new("main", "Top-level only").with_mode(DeclarationMode::Primary);
        manager
            .register(SubagentEntry::new("main", Some(primary), stub_factory()))
            .unwrap();

        assert!(manager.has_agent("reviewer"));
        assert_eq!(manager.list_agent_names(), vec!["main", "reviewer"]);
        assert!(manager.is_primary_only("main"));
        assert!(!manager.is_primary_only("reviewer"));

        // PRIMARY-only 拒绝 spawn
        let error = manager.create_agent_if_present(&RuntimeContext::new(), "main").unwrap_err();
        assert!(error.contains("PRIMARY-only"), "got: {error}");
        // 未注册报 Unknown
        let error = manager.create_agent_if_present(&RuntimeContext::new(), "ghost").unwrap_err();
        assert!(error.contains("Unknown agent_id"), "got: {error}");

        // 重复注册拒绝
        assert!(manager
            .register(SubagentEntry::new("reviewer", None, stub_factory()))
            .is_err());
    }

    #[test]
    fn manager_snapshot_replacement() {
        let manager = DefaultAgentManager::new();
        manager
            .register(SubagentEntry::new("old", None, stub_factory()))
            .unwrap();
        manager.replace_agents(vec![
            SubagentEntry::new("new-a", None, stub_factory()),
            SubagentEntry::new("new-b", None, stub_factory()),
        ]);
        assert!(!manager.has_agent("old"));
        assert!(manager.has_agent("new-a"));
        assert!(manager.has_agent("new-b"));
    }

    #[test]
    fn load_from_directory_skips_malformed_files() {
        let root = std::env::temp_dir().join(format!("harness-specs-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("good.md"),
            "---\ndescription: Good agent\n---\nYou are good.\n",
        )
        .unwrap();
        std::fs::write(root.join("bad.md"), "no frontmatter here").unwrap();
        std::fs::write(root.join("ignored.txt"), "not markdown").unwrap();

        let loader = AgentSpecLoader::new(root.to_string_lossy().to_string());
        let declarations = loader.load_from_directory(&root);
        assert_eq!(declarations.len(), 1);
        assert_eq!(declarations[0].name, "good");
        assert_eq!(declarations[0].inline_agents_body.as_deref(), Some("You are good."));
        std::fs::remove_dir_all(&root).ok();
    }
}
