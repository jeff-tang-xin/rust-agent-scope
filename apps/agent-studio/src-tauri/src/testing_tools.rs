//!
//! 测试状态工具：LLM 自主编排时的状态自回写面。
//!
//! 执行编排交给 LLM（`test_run_task` 一次性把待跑用例交给测试智能体，
//! 由它逐条执行），用例状态与 Bug 由 LLM 在执行过程中调本组工具直接
//! 落库——取代旧「前端逐条编排 + 解析回复 JSON 回写」的模式：
//! - 前端编排要求页面全程在线，页面一关编排就断；
//! - 解析 JSON 块对「回复里混了别的内容」毫无抵抗力；
//! - 授权挂起（HITL）时前端循环会撞墙刷屏（invalid input 风暴）。
//!
//! 工具面刻意收窄：只能更新「已存在的结果槽」（test_run_task 启动时为
//! 每条待跑用例初始化 running 槽），不能凭空造结果、不能改别的任务。

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use agentscope_core::error::Result;
use agentscope_core::tool::{Tool, ToolOutput};

use crate::testing::{load_json, new_id, now_ms, save_json, Bug, StepResult, TestTask};

/// `.ox` 数据根（factory 装配时注入；数据文件在 `.ox/testing/` 下）。
#[derive(Debug, Clone)]
pub struct TestingStore(PathBuf);

impl TestingStore {
    pub fn new(ox_root: &Path) -> Self {
        Self(ox_root.join("testing"))
    }

    fn tasks_path(&self) -> PathBuf {
        self.0.join("tasks.json")
    }

    fn bugs_path(&self) -> PathBuf {
        self.0.join("bugs.json")
    }

    fn load_tasks(&self) -> Vec<TestTask> {
        load_json(&self.tasks_path())
    }

    fn load_bugs(&self) -> Vec<Bug> {
        load_json(&self.bugs_path())
    }
}

fn input_str<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(|v| v.as_str())
}

/// 用例终态：全部结果槽到达终态 → 任务收尾（done + finished_at）。
fn is_terminal(status: &str) -> bool {
    matches!(status, "pass" | "fail" | "blocked" | "skipped" | "error")
}

/// 任务进度摘要（工具返回给 LLM，让它知道还剩多少）。
fn progress_line(task: &TestTask) -> String {
    let total = task.results.len();
    let done = task
        .results
        .iter()
        .filter(|r| is_terminal(&r.status))
        .count();
    format!("任务「{}」进度 {}/{}", task.name, done, total)
}

// ---------- test_update_case ----------

/// 用例状态回写：LLM 执行完一条用例后调它落库。
pub struct TestUpdateCase {
    store: TestingStore,
}

impl TestUpdateCase {
    pub fn new(store: TestingStore) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl Tool for TestUpdateCase {
    fn name(&self) -> &str {
        "test_update_case"
    }

    fn description(&self) -> &str {
        "Report a test case execution result — the ONLY way case status is recorded. \
Call it immediately after finishing each case (pass/fail/blocked), before moving \
to the next one. Results written only in your reply text are NOT saved."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {"type": "string", "description": "测试任务 id（执行提示词里给出）。"},
                "case_id": {"type": "string", "description": "用例 id（执行提示词里给出）。"},
                "status": {"type": "string", "enum": ["pass", "fail", "blocked"], "description": "pass=全部步骤通过；fail=行为与预期不符；blocked=无法继续执行。"},
                "actual": {"type": "string", "description": "实际结果摘要（看到了什么，与预期差异在哪）。"},
                "steps": {"type": "array", "description": "逐步结果（可选但强烈建议）。",
                    "items": {"type": "object", "properties": {
                        "no": {"type": "integer", "description": "步骤序号（从 1）。"},
                        "status": {"type": "string", "enum": ["pass", "fail", "blocked"], "description": "该步结果。"},
                        "note": {"type": "string", "description": "该步实际看到什么。"}
                    }, "required": ["no", "status"]}},
                "screenshots": {"type": "array", "items": {"type": "string"}, "description": "截图文件名（browser_screenshot 保存的，不含路径）。"}
            },
            "required": ["task_id", "case_id", "status", "actual"]
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput> {
        let task_id = input_str(&input, "task_id").unwrap_or("").trim();
        let case_id = input_str(&input, "case_id").unwrap_or("").trim();
        let status = input_str(&input, "status").unwrap_or("").trim();
        if task_id.is_empty() || case_id.is_empty() {
            return Ok(ToolOutput::error("task_id 与 case_id 不能为空"));
        }
        if !matches!(status, "pass" | "fail" | "blocked") {
            return Ok(ToolOutput::error(format!(
                "非法 status `{status}`：只能是 pass / fail / blocked"
            )));
        }
        let actual = input_str(&input, "actual").unwrap_or("").trim().to_string();
        if actual.is_empty() {
            return Ok(ToolOutput::error("actual 不能为空：写清实际看到了什么"));
        }
        let steps = input
            .get("steps")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|s| {
                        let no = s.get("no")?.as_u64()? as u32;
                        let st = s.get("status")?.as_str()?.to_string();
                        let note = s
                            .get("note")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        Some(StepResult {
                            no,
                            status: st,
                            note,
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let screenshots = input
            .get("screenshots")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let mut tasks = self.store.load_tasks();
        let Some(task) = tasks.iter_mut().find(|t| t.id == task_id) else {
            return Ok(ToolOutput::error(format!("任务 `{task_id}` 不存在")));
        };
        let Some(ri) = task.results.iter().position(|r| r.case_id == case_id) else {
            return Ok(ToolOutput::error(format!(
                "任务 `{task_id}` 里没有用例 `{case_id}` 的结果槽（只允许更新本次执行计划内的用例）"
            )));
        };
        let result = &mut task.results[ri];
        result.status = status.to_string();
        result.actual = actual;
        result.steps = steps;
        result.screenshots = screenshots;
        result.finished_at = now_ms();

        // 任务收尾：全部结果槽到达终态 → done（LLM 不需要单独「完成任务」）。
        let all_done = task.results.iter().all(|r| is_terminal(&r.status));
        if all_done {
            task.status = "done".into();
            task.finished_at = now_ms();
        }
        let summary = progress_line(task);
        let finished = all_done;
        if let Err(e) = save_json(&self.store.tasks_path(), &tasks) {
            return Ok(ToolOutput::error(e));
        }
        Ok(ToolOutput::ok(if finished {
            format!("用例 `{case_id}` 已记录为 {status}。{summary}——全部用例已执行完毕，任务收尾。")
        } else {
            format!("用例 `{case_id}` 已记录为 {status}。{summary}，继续下一条。")
        }))
    }
}

// ---------- test_submit_bug ----------

/// Bug 提交：fail/blocked 时由 LLM 调它落库并回填用例结果。
pub struct TestSubmitBug {
    store: TestingStore,
}

impl TestSubmitBug {
    pub fn new(store: TestingStore) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl Tool for TestSubmitBug {
    fn name(&self) -> &str {
        "test_submit_bug"
    }

    fn description(&self) -> &str {
        "Submit a bug report for a failed/blocked test case. The tool deduplicates: \
if the case already has an open (unfixed) bug, it returns the existing one instead \
of creating a duplicate. Call it right after test_update_case with status fail/blocked."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {"type": "string", "description": "测试任务 id。"},
                "case_id": {"type": "string", "description": "关联用例 id。"},
                "title": {"type": "string", "description": "一句话 Bug 标题（现象 + 差异）。"},
                "severity": {"type": "string", "enum": ["P1", "P2", "P3"], "description": "严重级别。默认 P2。"},
                "steps": {"type": "string", "description": "复现步骤（编号列表文本）。"},
                "expected": {"type": "string", "description": "预期结果。"},
                "actual": {"type": "string", "description": "实际结果。"},
                "screenshots": {"type": "array", "items": {"type": "string"}, "description": "截图文件名（不含路径）。"}
            },
            "required": ["task_id", "case_id", "title", "actual"]
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput> {
        let task_id = input_str(&input, "task_id").unwrap_or("").trim();
        let case_id = input_str(&input, "case_id").unwrap_or("").trim();
        let title = input_str(&input, "title").unwrap_or("").trim();
        if task_id.is_empty() || case_id.is_empty() || title.is_empty() {
            return Ok(ToolOutput::error("task_id / case_id / title 不能为空"));
        }
        let severity = match input_str(&input, "severity").unwrap_or("P2").trim() {
            "P1" => "P1",
            "P3" => "P3",
            _ => "P2",
        };
        let text = |key: &str| {
            input_str(&input, key).unwrap_or("").trim().to_string()
        };
        let screenshots = input
            .get("screenshots")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        // 防重复：同用例已有 open（待处理）Bug → 返回既有条目。
        let mut bugs = self.store.load_bugs();
        if let Some(existing) = bugs
            .iter()
            .find(|b| b.case_id == case_id && b.status == "open")
        {
            return Ok(ToolOutput::ok(format!(
                "该用例已存在待处理 Bug「{}」（{}），本次不重复创建；若为新问题请先在描述里区分。",
                existing.title, existing.id
            )));
        }

        let bug = Bug {
            id: new_id(),
            title: title.to_string(),
            severity: severity.to_string(),
            steps: text("steps"),
            expected: text("expected"),
            actual: text("actual"),
            screenshots,
            case_id: case_id.to_string(),
            task_id: task_id.to_string(),
            status: "open".into(),
            created_at: now_ms(),
            fix_note: String::new(),
        };
        let bug_id = bug.id.clone();
        let bug_title = bug.title.clone();
        bugs.push(bug);
        if let Err(e) = save_json(&self.store.bugs_path(), &bugs) {
            return Ok(ToolOutput::error(e));
        }

        // 回填用例结果的 bug_id（fail/blocked 的结果挂着 Bug 才完整）。
        let mut tasks = self.store.load_tasks();
        if let Some(task) = tasks.iter_mut().find(|t| t.id == task_id) {
            if let Some(r) = task.results.iter_mut().find(|r| r.case_id == case_id) {
                r.bug_id = bug_id.clone();
                if let Err(e) = save_json(&self.store.tasks_path(), &tasks) {
                    return Ok(ToolOutput::error(e));
                }
            }
        }
        Ok(ToolOutput::ok(format!(
            "Bug 已提交：{bug_id}「{bug_title}」"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::CaseResult;

    fn store(tag: &str) -> TestingStore {
        let dir = std::env::temp_dir().join(format!("ox_testing_tools_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("testing")).unwrap();
        TestingStore::new(&dir)
    }

    fn seed_task(store: &TestingStore, case_ids: &[&str]) -> TestTask {
        let mut task = TestTask {
            id: "t1".into(),
            name: "冒烟任务".into(),
            base_url: "http://localhost:8080".into(),
            config_id: String::new(),
            requirement_ids: vec![],
            case_ids: case_ids.iter().map(|s| s.to_string()).collect(),
            status: "running".into(),
            results: case_ids
                .iter()
                .map(|cid| CaseResult {
                    case_id: cid.to_string(),
                    status: "running".into(),
                    ..Default::default()
                })
                .collect(),
            created_at: now_ms(),
            finished_at: 0,
            archived: false,
        };
        save_json(&store.tasks_path(), &vec![task.clone()]).unwrap();
        task
    }

    #[tokio::test]
    async fn update_case_records_status_and_finishes_task() {
        let store = store("update");
        seed_task(&store, &["c1", "c2"]);
        let tool = TestUpdateCase::new(store.clone());
        let out = tool
            .call(json!({
                "task_id": "t1", "case_id": "c1", "status": "pass",
                "actual": "返回 200，列表 10 条",
                "steps": [{"no": 1, "status": "pass", "note": "GET /list"}],
                "screenshots": ["s1.png"]
            }))
            .await
            .unwrap();
        assert!(out.content.contains("继续下一条"), "{}", out.content);
        let tasks = store.load_tasks();
        assert_eq!(tasks[0].results[0].status, "pass");
        assert_eq!(tasks[0].results[0].screenshots, vec!["s1.png"]);
        assert_eq!(tasks[0].status, "running");

        // 第二条也终态 → 任务自动收尾。
        let out = tool
            .call(json!({
                "task_id": "t1", "case_id": "c2", "status": "fail",
                "actual": "返回 500"
            }))
            .await
            .unwrap();
        assert!(out.content.contains("收尾"), "{}", out.content);
        let tasks = store.load_tasks();
        assert_eq!(tasks[0].status, "done");
        assert!(tasks[0].finished_at > 0);
    }

    #[tokio::test]
    async fn update_case_rejects_unknown_slot_and_bad_status() {
        let store = store("reject");
        seed_task(&store, &["c1"]);
        let tool = TestUpdateCase::new(store.clone());
        let out = tool
            .call(json!({"task_id": "t1", "case_id": "ghost", "status": "pass", "actual": "x"}))
            .await
            .unwrap();
        assert!(out.state == agentscope_core::message::ToolResultState::Error);
        let out = tool
            .call(json!({"task_id": "t1", "case_id": "c1", "status": "skipped", "actual": "x"}))
            .await
            .unwrap();
        assert!(out.state == agentscope_core::message::ToolResultState::Error);
    }

    #[tokio::test]
    async fn submit_bug_dedupes_open_and_backfills_result() {
        let store = store("bug");
        seed_task(&store, &["c1"]);
        let tool = TestSubmitBug::new(store.clone());
        let out = tool
            .call(json!({
                "task_id": "t1", "case_id": "c1", "title": "下单返回 500",
                "severity": "P1", "expected": "200", "actual": "500", "steps": "1. 下单"
            }))
            .await
            .unwrap();
        assert!(out.content.contains("已提交"), "{}", out.content);
        let bugs = store.load_bugs();
        assert_eq!(bugs.len(), 1);
        assert_eq!(bugs[0].severity, "P1");
        // 回填 bug_id。
        let tasks = store.load_tasks();
        assert_eq!(tasks[0].results[0].bug_id, bugs[0].id);

        // 同用例再提 → 去重。
        let out = tool
            .call(json!({"task_id": "t1", "case_id": "c1", "title": "又一个 500", "actual": "500"}))
            .await
            .unwrap();
        assert!(out.content.contains("不重复创建"), "{}", out.content);
        assert_eq!(store.load_bugs().len(), 1);
    }

    #[tokio::test]
    async fn submit_bug_after_fix_creates_new_one() {
        let store = store("regress");
        seed_task(&store, &["c1"]);
        let tool = TestSubmitBug::new(store.clone());
        tool.call(json!({"task_id": "t1", "case_id": "c1", "title": "v1", "actual": "x"}))
            .await
            .unwrap();
        // 旧 Bug 修复后（fixed）再失败 → 允许新建（回归或新问题）。
        let mut bugs = store.load_bugs();
        bugs[0].status = "fixed".into();
        save_json(&store.bugs_path(), &bugs).unwrap();
        let out = tool
            .call(json!({"task_id": "t1", "case_id": "c1", "title": "v2 回归", "actual": "y"}))
            .await
            .unwrap();
        assert!(out.content.contains("已提交"), "{}", out.content);
        assert_eq!(store.load_bugs().len(), 2);
    }

    #[test]
    fn task_archived_defaults_false_for_old_data() {
        // 旧数据无 archived 字段 → default false（归档功能对旧任务零迁移）。
        let t: TestTask = serde_json::from_str("{\"id\":\"x\",\"name\":\"n\",\"status\":\"done\"}").unwrap();
        assert!(!t.archived);
    }
}
