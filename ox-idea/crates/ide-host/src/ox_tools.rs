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

/// 构造工具提供者：未命中项目或项目不可用时返回空工具集与无记忆锚点
/// （agent 优雅降级为文件 + shell）。
pub fn ox_tool_provider(host: Arc<Host>) -> OxToolProvider {
    Arc::new(move |cwd| {
        let Some(project) = project_for(&host, cwd) else {
            return OxHostContext { tools: Vec::new(), memory_root: None };
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
        // 记忆锚 `<项目根>/.ox`：与 memory_save 工具同根，跨 worktree 共享项目记忆。
        OxHostContext { tools, memory_root: Some(project.join(".ox")) }
    })
}

/// 宿主启动时安装一次：此后进程内 ox-hareness 会话自动获得 IDE 语义工具。
pub fn install_ox_tools(host: Arc<Host>) {
    set_ox_tool_provider(ox_tool_provider(host));
}
