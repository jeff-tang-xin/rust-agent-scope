//! 把 IDE 的 MCP 语义工具适配为 AgentScope 工具，进程内注入 ox-hareness。
//!
//! 元数据来自 [`ide_mcp::ToolRegistry::tools`]，执行走 [`ide_mcp::ToolRegistry::call`]
//! （同步阻塞，内部 `block_on`——适配器用 `spawn_blocking` 桥接，避免在 async
//! 上下文里嵌套阻塞）。信任语义沿用 MCP 侧：项目受信任时写工具才可用。

use crate::mcp::Registry;
use crate::Host;
use agentscope_acp::{OxHostContext, OxToolProvider, set_ox_tool_provider};
use agentscope_core::{Tool, ToolOutput};
use ide_core::CancellationToken;
use ide_mcp::ToolRegistry;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

/// 一个 IDE 语义工具：元数据来自 `tools/list`，执行走 `ToolRegistry::call`。
struct IdeTool {
    registry: Arc<Registry>,
    project: PathBuf,
    meta: ide_mcp::Tool,
}

#[async_trait::async_trait]
impl Tool for IdeTool {
    fn name(&self) -> &str {
        &self.meta.name
    }
    fn description(&self) -> &str {
        &self.meta.description
    }
    fn parameters(&self) -> Value {
        self.meta.input_schema.clone()
    }
    fn is_read_only(&self) -> bool {
        self.meta.read_only
    }
    async fn call(&self, input: Value) -> Result<ToolOutput, agentscope_core::Error> {
        let registry = self.registry.clone();
        let project = self.project.clone();
        let name = self.meta.name.clone();
        let arguments = if input.is_object() { input.to_string() } else { "{}".to_owned() };
        // `call` 内部 block_on 宿主 runtime，必须离开 async worker 线程再调。
        let result = tokio::task::spawn_blocking(move || {
            registry.call(&project, &name, &arguments, CancellationToken::new())
        })
        .await;
        let text = match result {
            Ok(Ok(text)) => text,
            Ok(Err(ide_mcp::ToolCallError::UnknownTool(name))) => {
                return Ok(ToolOutput::error(format!("unknown tool: {name}")));
            }
            Ok(Err(ide_mcp::ToolCallError::Failed(message)) ) => {
                return Ok(ToolOutput::error(message));
            }
            Err(join) => return Ok(ToolOutput::error(format!("tool task failed: {join}"))),
        };
        Ok(ToolOutput::ok(text))
    }
}

/// 按任务工作目录挑项目：worktree 是项目目录的子目录，取包含 `cwd` 的最深已打开项目。
fn project_for(host: &Host, cwd: &std::path::Path) -> Option<PathBuf> {
    host.projects()
        .iter()
        .filter(|p| !p.light && cwd.starts_with(p.root.as_path()))
        .max_by_key(|p| p.root.components().count())
        .map(|p| p.root.clone())
}

/// 项目环境简报：复用 ide-run 的多语言检测（package.json scripts / Cargo.toml /
/// pom.xml / build.gradle / go.mod / pyproject / Makefile / CMake 等），把可跑
/// 配置清单拼成系统提示词节——agent 开局即知项目类型与可用运行配置，不必拿
/// 目录树瞎摸，也不会用 shell 拼启动命令绕过 IDE 进程簿记。
fn project_brief(root: &std::path::Path) -> Option<String> {
    let configs = ide_run::detect(root);
    if configs.is_empty() {
        return None;
    }
    let mut brief = String::from(
        "\n## 当前项目环境（IDE 检测）\n\
         - 启动/调试一律走 run 配置（get_run_configurations → execute_run_configuration），\n\
         不要用 shell 拼启动命令——IDE 负责进程簿记，shell 起的进程你停不掉。\n\
         - 大型项目（Rust/Java/Go 多模块）首次构建常见 5-10 分钟：编译期持续输出属正常，\n\
         耐心等待命令自然结束，不要中途判定卡死/超时而取消；增量构建通常秒级。\n\
         - 以下是自动检测到的配置；用户自定义配置用 get_run_configurations 现查。\n",
    );
    for config in configs.iter().take(20) {
        let cwd = config.cwd.as_ref().map(|p| p.0.display().to_string()).unwrap_or_default();
        brief.push_str(&format!(
            "- [{}] {} — `{} {}`{}\n",
            config.kind,
            config.name,
            config.command,
            config.args.join(" "),
            if cwd.is_empty() { String::new() } else { format!("（cwd: {cwd}）") }
        ));
    }
    if configs.len() > 20 {
        brief.push_str(&format!("- …等共 {} 个，其余用 get_run_configurations 现查\n", configs.len()));
    }
    Some(brief)
}

/// 构造工具提供者：未命中项目或项目不可用时返回空工具集与无记忆锚点
/// （agent 优雅降级为文件 + shell）。
pub fn ox_tool_provider(host: Arc<Host>) -> OxToolProvider {
    Arc::new(move |cwd| {
        let Some(project) = project_for(&host, cwd) else {
            return OxHostContext { tools: Vec::new(), memory_root: None, project_brief: None };
        };
        let registry = Arc::new(Registry::new(host.clone()));
        let tools = match registry.tools(&project) {
            Ok(tools) => tools
                .into_iter()
                .map(|meta| {
                    Arc::new(IdeTool { registry: registry.clone(), project: project.clone(), meta })
                        as Arc<dyn Tool>
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        // 数据库面板连接 → db_query/db_dml 工具（复用面板连接池与密码管理，
        // 无连接时为空不注册）。
        let mut tools = tools;
        tools.extend(crate::ox_db_tools::db_tools(host.clone(), cwd));
        // 记忆锚 `<项目根>/.oxidea/agent`：与 memory_save 工具同根，跨 worktree 共享项目记忆。
        // 记忆是 per-user 状态：确保它留在版本库之外（与 /shelf/、/changelists.json 同一套机制）。
        let memory_root = ide_core::project_dir::path(&project, "agent");
        if let Err(e) =
            ide_vcs::changelists::ensure_ignored(&ide_core::project_dir::path(&project, ""), &["/agent/"])
        {
            tracing::warn!("could not update .oxidea/.gitignore: {e}");
        }
        let project_brief = project_brief(&project);
        OxHostContext { tools, memory_root: Some(memory_root), project_brief }
    })
}

/// 宿主启动时安装一次：此后进程内 ox-hareness 会话自动获得 IDE 语义工具。
pub fn install_ox_tools(host: Arc<Host>) {
    set_ox_tool_provider(ox_tool_provider(host));
}
