// 协同智能体的需求协作工具（req_create / req_list / req_detail / bug_set_status）。
//
// 打通协同与测试的闭环：协同会话里讨论收敛出需求 → req_create 沉淀为正式
// 需求（origin=collab）；测试工作区（绑定同一目录）看到需求生成用例、跑任务、
// 产出 Bug；协同侧 req_detail 查需求下的 Bug → 修代码 → bug_set_status 置
// fixed（带修复说明）→ 测试侧「继续执行」自动回归（重跑筛选只放行 fixed/closed）。
//
// 数据锚定 `<生效工作区根>/.ox/testing/`——测试工作区绑定协同工作区时两边
// 解析到同一目录，读写的是同一份数据（S1 的 effective_workspace_root）。
// 工具每次调用时从共享配置取快照解析，绑定关系变更即时生效。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use agentscope_core::error::Result;
use agentscope_core::tool::{Tool, ToolOutput};

use crate::config::StudioConfig;
use crate::factory;
use crate::testing::{load_json, save_json, Bug, Requirement, TestCase};

/// 工具上下文：装配时注入，调用时解析生效工作区根下的测试数据目录。
#[derive(Clone)]
pub struct ReqCtx {
    pub data_dir: PathBuf,
    /// 共享配置（Mutex）：每次调用取快照，绑定关系变更即时生效。
    pub config: Arc<Mutex<StudioConfig>>,
    pub workspace_id: String,
}

impl ReqCtx {
    /// 测试数据根：`<生效工作区根>/.ox/testing`。
    fn root(&self) -> std::result::Result<PathBuf, String> {
        let snapshot = self
            .config
            .lock()
            .map_err(|e| format!("config lock: {e}"))?
            .clone();
        let ws = snapshot
            .workspace(&self.workspace_id)
            .ok_or_else(|| format!("unknown workspace `{}`", self.workspace_id))?;
        let root = factory::effective_workspace_root(&self.data_dir, &snapshot, ws);
        Ok(root.join(".ox").join("testing"))
    }
}

fn input_str<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(|v| v.as_str())
}

/// 需求列表行摘要（req_list / req_detail 复用）。
fn requirement_line(req: &Requirement, cases: &[TestCase], bugs: &[Bug]) -> String {
    let case_ids: Vec<&str> = cases
        .iter()
        .filter(|c| c.requirement_id == req.id)
        .map(|c| c.id.as_str())
        .collect();
    let related: Vec<&Bug> = bugs
        .iter()
        .filter(|b| case_ids.contains(&b.case_id.as_str()))
        .collect();
    let open = related.iter().filter(|b| b.status == "open").count();
    let origin = if req.origin == "collab" { "协同" } else { "测试" };
    format!(
        "- id={} | {} | 用例 {} 条 | Bug {}（open {}）| 来源 {}",
        req.id,
        req.title,
        case_ids.len(),
        related.len(),
        open,
        origin
    )
}

// ---------- 工具：req_create ----------

/// 讨论沉淀需求：协同会话里收敛出明确需求后创建（origin=collab）。
pub struct ReqCreate {
    pub ctx: ReqCtx,
}

#[async_trait::async_trait]
impl Tool for ReqCreate {
    fn name(&self) -> &str {
        "req_create"
    }

    fn description(&self) -> &str {
        "Create a formal requirement from the current discussion (origin=collab). \
Use it when the discussion converges on a concrete, testable requirement — the user \
says things like '记下这个需求' / '创建需求'. The testing workspace (bound to the \
same directory) will see it and can generate test cases from it. Returns the \
requirement id."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": {"type": "string", "description": "Requirement title (short, imperative)."},
                "content": {"type": "string", "description": "Full requirement description: goal, scope, acceptance criteria."},
                "data_profile": {"type": "string", "description": "Optional. Current data facts of the system under test (row counts, date ranges, account/permission distribution). Test cases must be grounded in this — omit if unknown."}
            },
            "required": ["title", "content"]
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput> {
        let title = match input_str(&input, "title") {
            Some(v) if !v.trim().is_empty() => v.trim().to_string(),
            _ => return Ok(ToolOutput::error("title 不能为空")),
        };
        let content = input_str(&input, "content").unwrap_or("").to_string();
        let data_profile = input_str(&input, "data_profile").unwrap_or("").to_string();

        let root = match self.ctx.root() {
            Ok(r) => r,
            Err(e) => return Ok(ToolOutput::error(e)),
        };
        let path = root.join("requirements.json");
        let mut items: Vec<Requirement> = load_json(&path);
        let req = Requirement {
            id: crate::testing::new_id(),
            title,
            content,
            data_profile,
            images: Vec::new(),
            clarifications: Vec::new(),
            origin: "collab".into(),
            created_at: crate::testing::now_ms(),
        };
        let rid = req.id.clone();
        let rtitle = req.title.clone();
        items.push(req);
        if let Err(e) = save_json(&path, &items) {
            return Ok(ToolOutput::error(e));
        }
        Ok(ToolOutput::ok(format!(
            "需求已创建：id={rid} 标题「{rtitle}」。\n\
             绑定同一目录的测试工作区将看到该需求，可基于它生成用例并执行测试；\n\
             测出的 Bug 会挂在该需求下，用 req_detail 可查。"
        )))
    }
}

// ---------- 工具：req_list ----------

/// 需求列表（摘要）：协同侧了解当前需求全貌与 Bug 概况。
pub struct ReqList {
    pub ctx: ReqCtx,
}

#[async_trait::async_trait]
impl Tool for ReqList {
    fn name(&self) -> &str {
        "req_list"
    }

    fn description(&self) -> &str {
        "List requirements shared with the testing workspace (id, title, case count, \
bug counts, origin). Read-only. Use it to see what requirements exist and which \
have open bugs before diving into details."
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    async fn call(&self, _input: Value) -> Result<ToolOutput> {
        let root = match self.ctx.root() {
            Ok(r) => r,
            Err(e) => return Ok(ToolOutput::error(e)),
        };
        let requirements: Vec<Requirement> = load_json(&root.join("requirements.json"));
        if requirements.is_empty() {
            return Ok(ToolOutput::ok(
                "暂无需求。讨论收敛出明确需求后用 req_create 创建。",
            ));
        }
        let cases: Vec<TestCase> = load_json(&root.join("cases.json"));
        let bugs: Vec<Bug> = load_json(&root.join("bugs.json"));
        let lines: Vec<String> = requirements
            .iter()
            .map(|r| requirement_line(r, &cases, &bugs))
            .collect();
        Ok(ToolOutput::ok(format!(
            "共 {} 条需求：\n{}",
            requirements.len(),
            lines.join("\n")
        )))
    }
}

// ---------- 工具：req_detail ----------

/// 需求详情 + 关联用例 + Bug 列表：修复 Bug 前的情报入口。
pub struct ReqDetail {
    pub ctx: ReqCtx,
}

#[async_trait::async_trait]
impl Tool for ReqDetail {
    fn name(&self) -> &str {
        "req_detail"
    }

    fn description(&self) -> &str {
        "Show one requirement in full: description, data profile, clarification Q&A, \
related test cases and their bugs (steps / expected / actual / screenshots / fix \
note / status). Read-only. Use it before fixing a bug — the bug report contains \
reproduction steps and failure evidence."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "requirement_id": {"type": "string", "description": "Requirement id (from req_list)."}
            },
            "required": ["requirement_id"]
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput> {
        let rid = match input_str(&input, "requirement_id") {
            Some(v) if !v.trim().is_empty() => v.trim().to_string(),
            _ => return Ok(ToolOutput::error("requirement_id 不能为空")),
        };
        let root = match self.ctx.root() {
            Ok(r) => r,
            Err(e) => return Ok(ToolOutput::error(e)),
        };
        let requirements: Vec<Requirement> = load_json(&root.join("requirements.json"));
        let Some(req) = requirements.iter().find(|r| r.id == rid) else {
            return Ok(ToolOutput::error(format!("需求 `{rid}` 不存在（req_list 查看全部）")));
        };
        let cases: Vec<TestCase> = load_json(&root.join("cases.json"));
        let bugs: Vec<Bug> = load_json(&root.join("bugs.json"));

        let mut out = String::new();
        out.push_str(&format!("需求 {}「{}」\n", req.id, req.title));
        if !req.content.is_empty() {
            out.push_str(&format!("\n【描述】\n{}\n", req.content));
        }
        if !req.data_profile.is_empty() {
            out.push_str(&format!("\n【数据现状】\n{}\n", req.data_profile));
        }
        if !req.clarifications.is_empty() {
            out.push_str("\n【澄清记录】\n");
            for c in &req.clarifications {
                out.push_str(&format!("- {c}\n"));
            }
        }
        if !req.images.is_empty() {
            out.push_str(&format!(
                "\n【附图】{}（.ox/screenshots/ 下，可用 read_file 工具查看图片文件）\n",
                req.images.join(", ")
            ));
        }

        let related_cases: Vec<&TestCase> =
            cases.iter().filter(|c| c.requirement_id == rid).collect();
        if related_cases.is_empty() {
            out.push_str("\n【关联用例】暂无——测试侧尚未基于该需求生成用例。\n");
        } else {
            out.push_str(&format!("\n【关联用例】{} 条\n", related_cases.len()));
            for c in &related_cases {
                out.push_str(&format!("- id={} {}\n", c.id, c.title));
            }
        }

        let case_ids: Vec<&str> = related_cases.iter().map(|c| c.id.as_str()).collect();
        let related_bugs: Vec<&Bug> = bugs
            .iter()
            .filter(|b| case_ids.contains(&b.case_id.as_str()))
            .collect();
        if related_bugs.is_empty() {
            out.push_str("\n【关联 Bug】暂无。\n");
        } else {
            out.push_str(&format!("\n【关联 Bug】{} 个\n", related_bugs.len()));
            for b in &related_bugs {
                out.push_str(&format!(
                    "\n- id={} 「{}」 严重度 {} 状态 {}\n",
                    b.id, b.title, b.severity, b.status
                ));
                if !b.steps.is_empty() {
                    out.push_str(&format!("  复现步骤：\n{}\n", b.steps));
                }
                if !b.expected.is_empty() || !b.actual.is_empty() {
                    out.push_str(&format!("  预期：{}｜实际：{}\n", b.expected, b.actual));
                }
                if !b.fix_note.is_empty() {
                    out.push_str(&format!("  修复说明：{}\n", b.fix_note));
                }
                if !b.screenshots.is_empty() {
                    out.push_str(&format!(
                        "  截图：{}（.ox/screenshots/ 下）\n",
                        b.screenshots.join(", ")
                    ));
                }
            }
            let open = related_bugs.iter().filter(|b| b.status == "open").count();
            if open > 0 {
                out.push_str(&format!(
                    "\n{open} 个 open 状态的 Bug 待修复：定位问题 → 修改代码 → \
bug_set_status 置 fixed 并写清修复说明（测试侧会自动回归）。\n"
                ));
            }
        }
        Ok(ToolOutput::ok(out))
    }
}

// ---------- 工具：bug_set_status ----------

/// Bug 状态流转：协同修复后置 fixed（写修复说明），回归通过后置 closed。
pub struct BugSetStatus {
    pub ctx: ReqCtx,
}

#[async_trait::async_trait]
impl Tool for BugSetStatus {
    fn name(&self) -> &str {
        "bug_set_status"
    }

    fn description(&self) -> &str {
        "Update a bug's status: 'fixed' after you fixed the code (write a fix note \
explaining what was changed — the testing workspace will re-run the case to \
regress it), or 'closed' when the regression passed. Read-only otherwise."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "bug_id": {"type": "string", "description": "Bug id (from req_detail)."},
                "status": {"type": "string", "enum": ["open", "fixed", "closed"], "description": "New status."},
                "fix_note": {"type": "string", "description": "Required for 'fixed': what was changed and how (files, root cause, fix approach)."}
            },
            "required": ["bug_id", "status"]
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput> {
        let bug_id = match input_str(&input, "bug_id") {
            Some(v) if !v.trim().is_empty() => v.trim().to_string(),
            _ => return Ok(ToolOutput::error("bug_id 不能为空")),
        };
        let status = match input_str(&input, "status") {
            Some(v) => v.trim().to_string(),
            None => return Ok(ToolOutput::error("status 不能为空")),
        };
        if !matches!(status.as_str(), "open" | "fixed" | "closed") {
            return Ok(ToolOutput::error(format!(
                "status 只能是 open/fixed/closed，收到 `{status}`"
            )));
        }
        let fix_note = input_str(&input, "fix_note").unwrap_or("").trim().to_string();
        if status == "fixed" && fix_note.is_empty() {
            return Ok(ToolOutput::error(
                "置 fixed 必须写 fix_note（改了什么、怎么修的）——测试侧据此理解修复内容",
            ));
        }

        let root = match self.ctx.root() {
            Ok(r) => r,
            Err(e) => return Ok(ToolOutput::error(e)),
        };
        let path = root.join("bugs.json");
        let mut bugs: Vec<Bug> = load_json(&path);
        let Some(bug) = bugs.iter_mut().find(|b| b.id == bug_id) else {
            return Ok(ToolOutput::error(format!("Bug `{bug_id}` 不存在")));
        };
        bug.status = status.clone();
        if !fix_note.is_empty() {
            bug.fix_note = fix_note.clone();
        }
        let title = bug.title.clone();
        if let Err(e) = save_json(&path, &bugs) {
            return Ok(ToolOutput::error(e));
        }
        let next = if status == "fixed" {
            "测试侧「继续执行」将自动重跑该用例回归；回归通过后置 closed。"
        } else {
            ""
        };
        Ok(ToolOutput::ok(format!(
            "Bug `{bug_id}`「{title}」状态已置 {status}。{}",
            next
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::message::ToolResultState;

    fn ok_text(out: ToolOutput) -> String {
        assert_eq!(out.state, ToolResultState::Success, "expected success: {}", out.content);
        out.content
    }

    fn ctx_in(dir: &std::path::Path, ws_id: &str) -> ReqCtx {
        ReqCtx {
            data_dir: dir.to_path_buf(),
            config: Arc::new(Mutex::new(StudioConfig::default())),
            workspace_id: ws_id.to_string(),
        }
    }

    /// 空配置下 workspace 解析不到 → 工具报错而不是 panic（防御路径）。
    #[tokio::test]
    async fn tools_error_on_unknown_workspace() {
        let dir = std::env::temp_dir().join(format!("req_tool_unknown_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ctx = ctx_in(&dir, "ws-missing");
        assert!(ctx.root().is_err());

        let out = ReqList { ctx: ctx.clone() }
            .call(json!({}))
            .await
            .unwrap();
        assert_eq!(out.state, ToolResultState::Error);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 创建 → 列表 → 详情 → 置 fixed 全链路（默认目录形态，绑定字段为空）。
    #[tokio::test]
    async fn create_list_detail_fix_roundtrip() {
        let dir = std::env::temp_dir().join(format!("req_tool_rt_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ws_id = "ws-rt";
        let mut config = StudioConfig::default();
        config.workspaces.push(crate::config::WorkspaceConfig {
            id: ws_id.into(),
            name: "t".into(),
            kind: "collab".into(),
            agent_id: "a".into(),
            dir: dir.to_string_lossy().to_string(),
            members: vec![],
            bound_servers: vec![],
            bound_databases: vec![],
            created_at: 0,
            auto_resource: None,
            bound_workspace_id: None,
        });
        let ctx = ReqCtx {
            data_dir: dir.clone(),
            config: Arc::new(Mutex::new(config)),
            workspace_id: ws_id.to_string(),
        };

        // 创建需求。
        let created = ok_text(
            ReqCreate { ctx: ctx.clone() }
                .call(json!({
                    "title": "登录功能",
                    "content": "支持账号密码登录",
                    "data_profile": "用户表 10 条"
                }))
                .await
                .unwrap(),
        );
        assert!(created.contains("id="));

        // 空标题被拒。
        let out = ReqCreate { ctx: ctx.clone() }
            .call(json!({"title": "  "}))
            .await
            .unwrap();
        assert_eq!(out.state, ToolResultState::Error);

        // 列表可见（origin=collab）。
        let listed = ok_text(ReqList { ctx: ctx.clone() }.call(json!({})).await.unwrap());
        assert!(listed.contains("登录功能") && listed.contains("协同"));

        // 详情：无用例无 Bug 的空态。
        let rid = created
            .split("id=")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .unwrap_or("")
            .to_string();
        let detail = ok_text(
            ReqDetail { ctx: ctx.clone() }
                .call(json!({"requirement_id": rid}))
                .await
                .unwrap(),
        );
        assert!(detail.contains("暂无——测试侧尚未基于该需求生成用例"));

        // 不存在的需求报错。
        let out = ReqDetail { ctx: ctx.clone() }
            .call(json!({"requirement_id": "nope"}))
            .await
            .unwrap();
        assert_eq!(out.state, ToolResultState::Error);

        // 直接落一个 Bug（模拟测试侧产出），再走状态流转。
        let root = ctx.root().unwrap();
        let bug = Bug {
            id: "b1".into(),
            title: "登录按钮无响应".into(),
            severity: "P1".into(),
            steps: "1. 打开登录页\n2. 点击登录".into(),
            expected: "跳转首页".into(),
            actual: "停在登录页".into(),
            screenshots: vec!["shot-1.png".into()],
            case_id: "c1".into(),
            task_id: "t1".into(),
            status: "open".into(),
            created_at: 0,
            fix_note: String::new(),
        };
        save_json(&root.join("bugs.json"), &vec![bug]).unwrap();

        // 缺 fix_note 的 fixed 被拒。
        let out = BugSetStatus { ctx: ctx.clone() }
            .call(json!({"bug_id": "b1", "status": "fixed"}))
            .await
            .unwrap();
        assert_eq!(out.state, ToolResultState::Error);

        // 非法状态被拒。
        let out = BugSetStatus { ctx: ctx.clone() }
            .call(json!({"bug_id": "b1", "status": "done"}))
            .await
            .unwrap();
        assert_eq!(out.state, ToolResultState::Error);

        // 置 fixed 成功，fix_note 落盘。
        let fixed = ok_text(
            BugSetStatus { ctx: ctx.clone() }
                .call(json!({
                    "bug_id": "b1",
                    "status": "fixed",
                    "fix_note": "修复 Login.tsx 按钮未绑定 onClick"
                }))
                .await
                .unwrap(),
        );
        assert!(fixed.contains("fixed") && fixed.contains("回归"));

        let bugs: Vec<Bug> = load_json(&root.join("bugs.json"));
        assert_eq!(bugs[0].status, "fixed");
        assert!(bugs[0].fix_note.contains("Login.tsx"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
