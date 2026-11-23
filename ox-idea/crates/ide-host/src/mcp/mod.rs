//! Built-in IDE tools. Transport and discovery live in `ide-mcp`.
mod actions;
mod edit;
mod editors;
mod requests;
mod run;
mod vcs;
pub use requests::Requests;
mod debug;
mod files;
mod reports;
mod search;
mod symbols;
mod tool;
mod util;

use crate::Host;
use ide_core::{CancellationToken, IdeError, Severity};
use ide_mcp::{ToolCallError, ToolRegistry};
pub use reports::{FileProblems, FocusedEditor, Reports};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct Registry(Arc<Host>);
impl Registry {
    pub fn new(host: Arc<Host>) -> Self {
        Self(host)
    }
}

/// 「IDE 控制」类工具：仅内置 OX agent（进程内 ox-hareness）可用，外部 ACP
/// agent 经 MCP server 既看不到也调不到（ide-mcp 按 `builtin_only` 过滤）。
/// 外部 agent 保留「上下文 + 基础编辑」子集（文件/搜索/编辑器上下文/符号/
/// create_new_file/apply_patch/git 读）。调整对外范围只需增删此清单；
/// `builtin_only_tools_are_marked` 契约测试保证清单名都真实存在（防改名漂移）。
const BUILTIN_ONLY: &[&str] = &[
    // UI 控制
    "ide_action",
    "open_file_in_editor",
    // 调试 DAP
    "start_debug_session",
    "stop_debug_session",
    "set_breakpoint",
    "remove_breakpoint",
    "debug_control",
    "get_debug_state",
    "evaluate_expression",
    // 构建/运行/进程
    "build_project",
    "get_run_configurations",
    "execute_run_configuration",
    "get_running_processes",
    "stop_process",
    // Git 写
    "git_commit",
    "git_branch",
    // 语义重构/格式化
    "rename_refactoring",
    "refactor_code_action",
    "reformat_file",
];

fn tools() -> Vec<tool::Tool> {
    let mut tools = files::tools();
    tools.extend(search::tools());
    tools.extend(editors::tools());
    tools.push(symbols::get_symbol_info());
    tools.push(symbols::find_references());
    tools.push(symbols::call_hierarchy());
    tools.extend(edit::tools());
    tools.extend(run::tools());
    tools.extend(debug::tools());
    tools.extend(vcs::tools());
    tools.extend(actions::tools());
    tools.into_iter().map(|t| if BUILTIN_ONLY.contains(&t.metadata.name.as_str()) { t.builtin() } else { t }).collect()
}

/// 项目级外部工具范围配置（可选，`.oxidea/agent/tool-scope.json`）：
/// `{ "builtinOnly": ["tool", …], "allowExternal": ["tool", …] }`。
/// - `builtinOnly`：追加标记内置专属（外部 MCP client 不可见不可调）。
/// - `allowExternal`：豁免默认内置专属，放给外部 agent。
/// 只改 `builtin_only` 标志（外部边界过滤）；内置 ox-hareness 进程内注入忽略
/// 该标志，始终全量。无配置沿用默认；解析失败 warn 后沿用默认（不阻断工具面）。
fn apply_tool_scope_config(tools: &mut [ide_mcp::Tool], root: &Path) {
    let path = ide_core::project_dir::path(root, "agent").join("tool-scope.json");
    let Ok(text) = std::fs::read_to_string(&path) else { return };
    let cfg: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("tool-scope.json 解析失败，沿用默认工具范围（{}）: {e}", path.display());
            return;
        }
    };
    let names = |key: &str| -> Vec<String> {
        cfg.get(key)
            .and_then(serde_json::Value::as_array)
            .map(|a| a.iter().filter_map(serde_json::Value::as_str).map(str::to_owned).collect())
            .unwrap_or_default()
    };
    let builtin_only = names("builtinOnly");
    let allow_external = names("allowExternal");
    for t in tools.iter_mut() {
        if builtin_only.iter().any(|n| n == &t.name) {
            t.builtin_only = true;
        }
        if allow_external.iter().any(|n| n == &t.name) {
            t.builtin_only = false;
        }
    }
}

// Checked both when listing and calling, so a later trust change takes effect immediately.
fn allowed(tool: &ide_mcp::Tool, trusted: bool) -> bool {
    trusted || tool.read_only
}

impl Registry {
    fn target(&self, root: &Path) -> Result<(String, Arc<crate::Project>), String> {
        if self.0.mcp_reports.remote_roots().iter().any(|p| p == root) {
            return Err("not supported for remote projects yet".into());
        }
        let mut windows: Vec<_> = self
            .0
            .projects
            .read()
            .iter()
            .filter(|(_, p)| p.root == root && !p.light)
            .map(|(w, p)| (w.clone(), p.clone()))
            .collect();
        let focused = self.0.mcp_reports.focused_window();
        windows.sort_by(|a, b| {
            (Some(&b.0) == focused.as_ref()).cmp(&(Some(&a.0) == focused.as_ref())).then(a.0.cmp(&b.0))
        });
        windows.into_iter().next().ok_or_else(|| "project is no longer open".into())
    }
}
impl ToolRegistry for Registry {
    fn projects(&self) -> Vec<PathBuf> {
        self.0
            .projects()
            .into_iter()
            .filter(|p| !p.light)
            .map(|p| p.root.clone())
            .chain(self.0.mcp_reports.remote_roots())
            .collect()
    }
    fn tools(&self, root: &Path) -> Result<Vec<ide_mcp::Tool>, String> {
        let (_, project) = self.target(root)?;
        let mut all: Vec<_> = tools().into_iter().map(|t| t.metadata).collect();
        if let Some(plugins) = self.0.plugin_host() {
            all.extend(plugins.mcp_tools(root).into_iter().map(|(_, _, tool)| tool));
        }
        apply_tool_scope_config(&mut all, &project.root);
        Ok(all.into_iter().filter(|t| allowed(t, project.is_trusted())).collect())
    }
    fn call(
        &self,
        root: &Path,
        name: &str,
        arguments: &str,
        cancel: CancellationToken,
    ) -> Result<String, ToolCallError> {
        self.call_with_client(root, name, arguments, cancel, "MCP client")
    }
    fn call_with_client(
        &self,
        root: &Path,
        name: &str,
        arguments: &str,
        cancel: CancellationToken,
        client: &str,
    ) -> Result<String, ToolCallError> {
        let (window, project) = self.target(root).map_err(ToolCallError::Failed)?;
        if let Some(plugins) = self.0.plugin_host()
            && let Some((plugin, tool, metadata)) =
                plugins.mcp_tools(root).into_iter().find(|(_, _, t)| t.name == name && allowed(t, project.is_trusted()))
        {
            return self.call_plugin(window, project, plugins, plugin, tool, metadata, arguments, cancel, client);
        }
        let tool = tools()
            .into_iter()
            .find(|t| t.metadata.name == name && allowed(&t.metadata, project.is_trusted()))
            .ok_or_else(|| ToolCallError::UnknownTool(format!("unknown tool: {name}")))?;
        let args = serde_json::from_str(arguments).map_err(|e| ToolCallError::Failed(e.to_string()))?;
        let call = tool::Call {
            host: self.0.clone(),
            project,
            window,
            args,
            cancel: cancel.clone(),
            client: client.to_owned(),
            tool: name.to_owned(),
        };
        if cancel.is_cancelled() {
            return Err(ToolCallError::Failed(IdeError::cancelled().message));
        }
        let result = self.0.runtime.block_on((tool.handler)(call));
        if cancel.is_cancelled() {
            return Err(ToolCallError::Failed(IdeError::cancelled().message));
        }
        result.map(|out| out.0).map_err(|e| ToolCallError::Failed(e.message))
    }
}

impl Host {
    fn mcp_windows(&self, root: &Path) -> Vec<String> {
        let mut windows: Vec<_> =
            self.projects.read().iter().filter(|(_, p)| p.root == root).map(|(w, _)| w.clone()).collect();
        windows.sort();
        windows
    }

    /// 每轮 IDE 现场快照（B7）：当前聚焦文件/选区 + 该文件顶部错误诊断，供内置
    /// agent 开局即知用户在看什么，不必先调 get_current_file。聚焦编辑器不属于
    /// 该项目或无聚焦编辑器时返回 None。同步读宿主状态，可在 agent 运行时调用。
    pub(crate) fn ide_snapshot_text(&self, root: &Path) -> Option<String> {
        let window = self.mcp_reports.focused_window()?;
        let editor = self.mcp_reports.focused(&window)?;
        if !editor.path.0.starts_with(root) {
            return None;
        }
        let mut s = String::from("[IDE 现场快照]（用户当前聚焦，仅供参考；细节以工具现查为准）\n");
        s.push_str(&format!("- 当前文件: {}\n", editor.path.0.display()));
        let sel = &editor.selection;
        if editor.selected_text.is_empty() {
            s.push_str(&format!("- 光标: 行 {} 列 {}\n", sel.start.line + 1, sel.start.col + 1));
        } else {
            let text: String = editor.selected_text.chars().take(2000).collect();
            s.push_str(&format!("- 选区（行 {}-{}）:\n```\n{}\n```\n", sel.start.line + 1, sel.end.line + 1, text));
        }
        let errors: Vec<_> = self
            .mcp_reports
            .problems(&self.mcp_windows(root))
            .into_iter()
            .filter(|f| f.path.0 == editor.path.0)
            .flat_map(|f| f.items)
            .filter(|d| d.severity == Severity::Error)
            .take(5)
            .collect();
        if !errors.is_empty() {
            s.push_str("- 该文件错误:\n");
            for d in errors {
                s.push_str(&format!("  - 行 {}: {}\n", d.range.start.line + 1, d.message));
            }
        }
        if let Some(stat) = self.workspace_diff_stat(root) {
            s.push_str("- 工作区改动（git 未暂存）:\n");
            for line in stat.lines() {
                s.push_str(&format!("  {line}\n"));
            }
        }
        Some(s)
    }

    /// 工作区未暂存改动摘要（`git diff --stat`），有界裁剪供快照使用。无仓库 /
    /// git 不可用 / 命令失败 / 无改动时返回 None，不影响快照其余内容。git 从
    /// 项目 root 自行发现仓库（root 是仓库子目录也可），环境口径同 vcs 读工具。
    fn workspace_diff_stat(&self, root: &Path) -> Option<String> {
        let git = {
            let projects = self.projects.read();
            let settings = projects
                .values()
                .find(|p| p.root == root && p.is_trusted())
                .map(|p| p.settings.effective())
                .unwrap_or_else(|| self.global_settings.effective());
            let exe = settings.vcs.git.executable.trim().to_owned();
            if exe.is_empty() { String::from("git") } else { exe }
        };
        let mut cmd = vcs::read_command(&git, root);
        cmd.args(["--no-pager", "diff", "--no-ext-diff", "--no-textconv", "--stat"]);
        ide_core::process::prepare(&mut cmd);
        let out = cmd.output().ok().filter(|o| o.status.success())?;
        let stat = clip_diff_stat(&String::from_utf8_lossy(&out.stdout), 15, 1500);
        (!stat.is_empty()).then_some(stat)
    }
}

/// 裁剪 `git diff --stat` 输出：最多保留前 `max_lines` 行文件统计 + 末尾 summary
/// 行，超出部分以省略标记代替；总长不超过 `max_chars` 字符。空输出返回空串。
fn clip_diff_stat(text: &str, max_lines: usize, max_chars: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let Some((summary, files)) = lines.split_last() else { return String::new() };
    let mut s = String::new();
    for line in files.iter().take(max_lines) {
        s.push_str(line);
        s.push('\n');
    }
    if files.len() > max_lines {
        s.push_str(&format!("  …（其余 {} 个文件省略）\n", files.len() - max_lines));
    }
    s.push_str(summary);
    if s.chars().count() > max_chars {
        s = s.chars().take(max_chars.saturating_sub(1)).collect();
        s.push('…');
    }
    s
}

impl Registry {
    #[allow(clippy::too_many_arguments)]
    fn call_plugin(
        &self,
        window: String,
        project: Arc<crate::Project>,
        plugins: Arc<crate::plugins::PluginHost>,
        plugin: String,
        tool: String,
        metadata: ide_mcp::Tool,
        arguments: &str,
        cancel: CancellationToken,
        client: &str,
    ) -> Result<String, ToolCallError> {
        let run = || -> ide_core::IdeResult<String> {
            let args = serde_json::from_str(arguments).map_err(|e| IdeError::invalid(e.to_string()))?;
            let call = tool::Call {
                host: self.0.clone(),
                project: project.clone(),
                window: window.clone(),
                args,
                cancel: cancel.clone(),
                client: client.into(),
                tool: metadata.name,
            };
            if cancel.is_cancelled() {
                return Err(IdeError::cancelled());
            }
            if metadata.destructive {
                self.0.runtime.block_on(call.confirm(&metadata.description, None))?;
            }
            if !metadata.read_only {
                project.require_trust()?;
            }
            let invocation = ide_plugin::Invocation {
                client: Some(window),
                workspace_root: Some(project.root.clone()),
                ..Default::default()
            };
            plugins.call_mcp_tool(&plugin, &tool, arguments, invocation, &cancel)
        };
        run().map_err(|e| ToolCallError::Failed(e.message))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn restricted_mode_excludes_execution_tools() {
        let mut tool = super::tools().remove(0).metadata;
        assert!(super::allowed(&tool, false));
        tool.read_only = false;
        assert!(!super::allowed(&tool, false));
        assert!(super::allowed(&tool, true));
    }

    /// 范围契约：BUILTIN_ONLY 清单名都真实存在（防改名漂移），且恰好这些工具被
    /// 标记 builtin_only，其余对外部可见。模块 tools() 仍返回全量——内置 agent
    /// 进程内注入用全量，外部过滤只在 ide-mcp 边界（见 ide-mcp server 测试）。
    #[test]
    fn builtin_only_tools_are_marked() {
        let tools = super::tools();
        let names: Vec<&str> = tools.iter().map(|t| t.metadata.name.as_str()).collect();
        for want in super::BUILTIN_ONLY {
            assert!(names.contains(want), "BUILTIN_ONLY 清单含不存在的工具: {want}");
        }
        for t in &tools {
            assert_eq!(
                t.metadata.builtin_only,
                super::BUILTIN_ONLY.contains(&t.metadata.name.as_str()),
                "{} 的 builtin_only 标记与清单不符",
                t.metadata.name
            );
        }
        let external: Vec<&str> =
            tools.iter().filter(|t| !t.metadata.builtin_only).map(|t| t.metadata.name.as_str()).collect();
        assert!(external.contains(&"read_file") && external.contains(&"apply_patch"));
        assert!(!external.contains(&"ide_action") && !external.contains(&"start_debug_session"));
    }

    /// 外部工具范围配置：builtinOnly 追加标记、allowExternal 豁免默认；
    /// 非法配置沿用当前不 panic。只动 builtin_only 标志（内置注入忽略它）。
    #[test]
    fn tool_scope_config_overrides_builtin_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let agent_dir = ide_core::project_dir::path(root, "agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let mut tools: Vec<ide_mcp::Tool> = super::tools().into_iter().map(|t| t.metadata).collect();
        let flag = |ts: &[ide_mcp::Tool], n: &str| ts.iter().find(|t| t.name == n).unwrap().builtin_only;
        // 默认：read_file 外部可见，ide_action 内置专属。
        assert!(!flag(&tools, "read_file"));
        assert!(flag(&tools, "ide_action"));
        // 配置：read_file 收为内置专属，ide_action 放给外部。
        std::fs::write(
            agent_dir.join("tool-scope.json"),
            r#"{"builtinOnly":["read_file"],"allowExternal":["ide_action"]}"#,
        )
        .unwrap();
        super::apply_tool_scope_config(&mut tools, root);
        assert!(flag(&tools, "read_file"));
        assert!(!flag(&tools, "ide_action"));
        // 非法配置：沿用当前标记，不 panic。
        std::fs::write(agent_dir.join("tool-scope.json"), "{not json").unwrap();
        super::apply_tool_scope_config(&mut tools, root);
        assert!(flag(&tools, "read_file"));
        assert!(!flag(&tools, "ide_action"));
    }

    /// 防漂移契约：内置 skill（oxidea-usage）必须覆盖全部 IDE 语义工具名。
    /// 新增工具不写进 skill，本测试即红——「智能体深刻了解 IDE」是可维护的
    /// 不变式，不是一次性文档。确属无需引导的工具须显式列名豁免并说明理由。
    #[test]
    fn oxidea_usage_skill_covers_all_builtin_tools() {
        const EXEMPT: &[&str] = &[];
        let skill = agentscope_acp::oxidea_skill::OXIDEA_USAGE_SKILL;
        let missing: Vec<String> = super::tools()
            .into_iter()
            .map(|t| t.metadata.name)
            .filter(|name| !EXEMPT.contains(&name.as_str()) && !skill.contains(name.as_str()))
            .collect();
        assert!(
            missing.is_empty(),
            "oxidea-usage skill 未覆盖工具: {missing:?}——补进 agentscope-acp/src/oxidea_skill.rs 或显式豁免"
        );
    }

    /// 快照 git 摘要裁剪：空输出、小改动原样、超行省略标记 + 保留 summary、字符上限。
    #[test]
    fn diff_stat_clipping_keeps_summary_and_bounds() {
        // 无改动 / 空白输出 → 空串（快照跳过该段）。
        assert_eq!(super::clip_diff_stat("", 15, 1500), "");
        assert_eq!(super::clip_diff_stat("  \n\n", 15, 1500), "");
        // 小改动：原样保留（含末尾 summary 行）。
        let small = " a.rs | 2 +-\n b.rs | 1 +\n 2 files changed, 2 insertions(+), 1 deletion(-)\n";
        assert_eq!(super::clip_diff_stat(small, 15, 1500), small.trim_end_matches('\n'));
        // 超出 max_lines：前 15 行 + 省略标记 + summary 行仍在。
        let mut many = String::new();
        for i in 0..40 {
            many.push_str(&format!(" file{i}.rs | 1 +\n"));
        }
        many.push_str(" 40 files changed, 40 insertions(+)\n");
        let clipped = super::clip_diff_stat(&many, 15, 1500);
        assert!(clipped.contains("file0.rs") && clipped.contains("file14.rs"));
        assert!(!clipped.contains("file15.rs"));
        assert!(clipped.contains("其余 25 个文件省略"));
        assert!(clipped.ends_with("40 files changed, 40 insertions(+)"));
        // 字符上限：超长路径也不撑爆快照。
        let long = format!(" {} | 1 +\n 1 file changed, 1 insertion(+)\n", "x".repeat(5000));
        let capped = super::clip_diff_stat(&long, 15, 1500);
        assert!(capped.chars().count() <= 1500);
        assert!(capped.ends_with('…'));
    }
}
