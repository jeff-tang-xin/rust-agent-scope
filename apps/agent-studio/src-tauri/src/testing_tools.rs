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

use crate::testing::{
    load_json, new_id, now_ms, save_json, Bug, CaseResult, StepResult, TestTask,
};

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

/// 执行图谱导航：按任务 case_ids 顺序找下一个未终态的结果槽。工具返回值
/// 据此告诉 LLM「下一条跑什么」——把「任务id → caseIds」的执行图谱落到
/// 每一步工具响应里，LLM 不用自己记进度，也不会跳步、漏步或重复执行。
fn next_pending(task: &TestTask) -> Option<&CaseResult> {
    task.case_ids
        .iter()
        .filter_map(|cid| task.results.iter().find(|r| &r.case_id == cid))
        .find(|r| !is_terminal(&r.status))
}

/// 待跑用例的有序队列文本（开工登记时随返回值下发）。
fn pending_queue(task: &TestTask) -> Vec<String> {
    task.case_ids
        .iter()
        .filter_map(|cid| task.results.iter().find(|r| &r.case_id == cid))
        .filter(|r| !is_terminal(&r.status))
        .map(|r| format!("`{}`", r.case_id))
        .collect()
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
        let Some(task) = find_task_mut(&mut tasks, task_id) else {
            return Ok(ToolOutput::error(format!(
                "任务 `{task_id}` 不存在（task_id 是任务 id——创建任务时生成的 32 位串，不是任务名）。{}",
                task_list_hint(&self.store)
            )));
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
        let next = next_pending(task).map(|r| r.case_id.clone());
        // 跳步检测：回写的不是队列第一个未终态用例时提醒——串行是导航的
        // 前提，一口气跑多条再批量回写会让「下一条」失去意义。不硬拒绝
        // （指定顺序/补跑是合法场景），但每次跳步都显式拉回。
        let skipped: Vec<String> = task
            .case_ids
            .iter()
            .filter_map(|cid| task.results.iter().find(|r| &r.case_id == cid))
            .take_while(|r| r.case_id != case_id)
            .filter(|r| !is_terminal(&r.status))
            .map(|r| format!("`{}`", r.case_id))
            .collect();
        let skip_warn = if skipped.is_empty() {
            String::new()
        } else {
            format!(
                "⚠️ 你跳过了队列中更早的未回写用例：{}——若非有意跳过，请先回去执行它们。",
                skipped.join("、")
            )
        };
        if let Err(e) = save_json(&self.store.tasks_path(), &tasks) {
            return Ok(ToolOutput::error(e));
        }
        Ok(ToolOutput::ok(if finished {
            format!(
                "用例 `{case_id}` 已记录为 {status}。{summary}——全部用例已执行完毕，\
                 **调 test_finish_task(task_id, note) 收尾交付**（note 写本轮执行汇总）。{skip_warn}"
            )
        } else {
            format!(
                "用例 `{case_id}` 已记录为 {status}。{summary}。{skip_warn}\
                 fail/blocked 时先 test_submit_bug 提 Bug 再继续。\
                 **下一条：case_id=`{}`**——先执行它，结束后立即 test_update_case，\
                 不要一次执行多条再批量回写。",
                next.unwrap_or_default()
            )
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
        if let Some(task) = find_task_mut(&mut tasks, task_id) {
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

// ---------- test_list_tasks ----------

/// 任务清单：LLM 自助查询当前任务（task_id/名称/状态/进度）——执行前确认
/// task_id 用。任务删除重建后 id 会变，凭会话历史里的旧 id 回写必然报
/// 「不存在」；有清单工具后 LLM 可立即自查自愈，不用猜、不用问用户。
pub struct TestListTasks {
    store: TestingStore,
}

impl TestListTasks {
    pub fn new(store: TestingStore) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl Tool for TestListTasks {
    fn name(&self) -> &str {
        "test_list_tasks"
    }

    fn description(&self) -> &str {
        "List test tasks in this workspace with task_id / name / status / progress. \
Call it to confirm the task_id BEFORE executing a task: task_id is the 32-char \
task id, NOT the task name, and NOT any id seen earlier in conversation history."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "include_archived": {
                    "type": "boolean",
                    "description": "是否包含已归档任务，默认 false。"
                }
            },
            "required": []
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput> {
        let include_archived = input
            .get("include_archived")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mut tasks = self.store.load_tasks();
        tasks.retain(|t| include_archived || !t.archived);
        if tasks.is_empty() {
            return Ok(ToolOutput::ok(
                "当前没有测试任务（尚未创建或已全部归档）。",
            ));
        }
        let lines: Vec<String> = tasks
            .iter()
            .map(|t| {
                let (mut pass, mut fail, mut blocked) = (0u32, 0u32, 0u32);
                for r in &t.results {
                    match r.status.as_str() {
                        "pass" => pass += 1,
                        "fail" => fail += 1,
                        "blocked" => blocked += 1,
                        _ => {}
                    }
                }
                let note = if t.status == "paused" && !t.pause_note.is_empty() {
                    format!("；交付说明：{}", t.pause_note)
                } else {
                    String::new()
                };
                format!(
                    "- task_id=`{}`「{}」状态={}，进度 {}/{}（pass {pass} / fail {fail} / blocked {blocked}）{}",
                    t.id,
                    t.name,
                    t.status,
                    pass + fail + blocked,
                    t.results.len(),
                    note
                )
            })
            .collect();
        Ok(ToolOutput::ok(format!(
            "共 {} 个任务：\n{}",
            lines.len(),
            lines.join("\n")
        )))
    }
}

// ---------- test_start_task / test_finish_task ----------

/// 错误提示附当前任务清单：LLM 手里的 task_id 过期（任务删除重建、会话
/// 历史残留旧 id）时能立即换用正确 id 自愈，而不是反复重试同一参数。
fn task_list_hint(store: &TestingStore) -> String {
    let tasks: Vec<String> = store
        .load_tasks()
        .iter()
        .filter(|t| !t.archived)
        .map(|t| format!("task_id=`{}`「{}」（{}）", t.id, t.name, t.status))
        .collect();
    if tasks.is_empty() {
        "当前没有未归档的测试任务（可能已被删除重建，请让用户在任务面板确认）".to_string()
    } else {
        format!("当前任务清单：{}", tasks.join("；"))
    }
}

/// 按 id 精确查找任务；未命中再按名称唯一匹配（LLM 常把任务名当 id 传，
/// 报「不存在」不如容错接住）。0 个或多个同名匹配返回 None。
fn find_task_mut<'a>(tasks: &'a mut [TestTask], task_id: &str) -> Option<&'a mut TestTask> {
    if let Some(pos) = tasks.iter().position(|t| t.id == task_id) {
        return Some(&mut tasks[pos]);
    }
    let hits: Vec<usize> = tasks
        .iter()
        .enumerate()
        .filter(|(_, t)| t.name == task_id)
        .map(|(i, _)| i)
        .collect();
    if hits.len() == 1 {
        Some(&mut tasks[hits[0]])
    } else {
        None
    }
}

/// 开工登记：LLM 开始执行任务前调它——后端据此把任务置 running 并
/// 记录开工时刻（turn 收尾据此识别「本 turn 内开工」的任务自动挂看门狗，
/// 覆盖对话触发路径）。幂等：重复登记只刷新时刻。
pub struct TestStartTask {
    store: TestingStore,
}

impl TestStartTask {
    pub fn new(store: TestingStore) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl Tool for TestStartTask {
    fn name(&self) -> &str {
        "test_start_task"
    }

    fn description(&self) -> &str {
        "Register the start of a test-task execution round. Call it BEFORE running \
any case of that task — the backend uses it to attach the watchdog (keep-alive) \
and notify the UI. Idempotent: calling again just refreshes the start time."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {"type": "string", "description": "测试任务 id（执行提示词或对话上下文里给出）。"}
            },
            "required": ["task_id"]
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput> {
        let task_id = input_str(&input, "task_id").unwrap_or("").trim();
        if task_id.is_empty() {
            return Ok(ToolOutput::error("task_id 不能为空"));
        }
        let mut tasks = self.store.load_tasks();
        let Some(task) = find_task_mut(&mut tasks, task_id) else {
            return Ok(ToolOutput::error(format!(
                "任务 `{task_id}` 不存在（task_id 是任务 id——创建任务时生成的 32 位串，不是任务名）。{}",
                task_list_hint(&self.store)
            )));
        };
        if task.status == "done" {
            return Ok(ToolOutput::error(format!(
                "任务 `{}` 已完成（done），如需重跑请在任务面板操作",
                task.name
            )));
        }
        task.status = "running".into();
        task.started_at = now_ms();
        task.pause_note.clear();
        let summary = progress_line(task);
        let name = task.name.clone();
        let queue = pending_queue(task);
        if let Err(e) = save_json(&self.store.tasks_path(), &tasks) {
            return Ok(ToolOutput::error(e));
        }
        if queue.is_empty() {
            return Ok(ToolOutput::ok(format!(
                "开工已登记，任务 `{name}` 进入执行中。{summary}——没有待执行用例，直接调 test_finish_task(task_id, note) 交付。"
            )));
        }
        Ok(ToolOutput::ok(format!(
            "开工已登记，任务 `{name}` 进入执行中。{summary}。\
             执行队列（按序逐条，一条回写后再取下一条，不要并行穿插）：{}。\
             **下一条：case_id={}`——执行它，完成后立即 test_update_case。",
            queue.join(" → "),
            queue[0]
        )))
    }
}

/// 交付登记：LLM 本轮执行结束（无论跑完多少条）必须调它——汇报进度与
/// 暂停原因，后端据此把任务置 paused（正常交付暂停，看门狗不再踢续跑）。
/// 全部用例已有结论时任务自动收尾为 done。
pub struct TestFinishTask {
    store: TestingStore,
}

impl TestFinishTask {
    pub fn new(store: TestingStore) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl Tool for TestFinishTask {
    fn name(&self) -> &str {
        "test_finish_task"
    }

    fn description(&self) -> &str {
        "Deliver the current execution round: call it when you stop working on the \
task (turn ending), reporting progress and why you paused. REQUIRED even if you \
finished nothing — silence makes the watchdog treat the run as lost and re-kick. \
All cases concluded => task auto-closes as done."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {"type": "string", "description": "测试任务 id。"},
                "note": {"type": "string", "description": "交付说明：本轮进度（如 12/33）、暂停原因（如接口 500 / 等待修复）、下一步建议。"}
            },
            "required": ["task_id", "note"]
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput> {
        let task_id = input_str(&input, "task_id").unwrap_or("").trim();
        let note = input_str(&input, "note").unwrap_or("").trim().to_string();
        if task_id.is_empty() {
            return Ok(ToolOutput::error("task_id 不能为空"));
        }
        if note.is_empty() {
            return Ok(ToolOutput::error(
                "note 不能为空：写清本轮进度与暂停原因（如「12/33，接口 500 暂停」）",
            ));
        }
        let mut tasks = self.store.load_tasks();
        let Some(task) = find_task_mut(&mut tasks, task_id) else {
            return Ok(ToolOutput::error(format!(
                "任务 `{task_id}` 不存在（task_id 是任务 id——创建任务时生成的 32 位串，不是任务名）。{}",
                task_list_hint(&self.store)
            )));
        };
        let all_done = task.results.iter().all(|r| is_terminal(&r.status));
        if all_done {
            task.status = "done".into();
            task.finished_at = now_ms();
        } else {
            task.status = "paused".into();
        }
        task.pause_note = note.clone();
        let summary = progress_line(task);
        let name = task.name.clone();
        let done = task.status == "done";
        let pending: Vec<String> = task
            .case_ids
            .iter()
            .filter_map(|cid| task.results.iter().find(|r| &r.case_id == cid))
            .filter(|r| !is_terminal(&r.status))
            .map(|r| format!("`{}`（{}）", r.case_id, r.status))
            .collect();
        if let Err(e) = save_json(&self.store.tasks_path(), &tasks) {
            return Ok(ToolOutput::error(e));
        }
        Ok(ToolOutput::ok(if done {
            format!("交付已登记：任务「{name}」全部用例执行完毕，已收尾（done）。{summary}")
        } else {
            format!(
                "交付已登记：任务「{name}」暂停（paused）。{summary}。\
                 未回写用例：{}。暂停说明：{note}。用户继续执行时从这些用例接着跑。",
                if pending.is_empty() {
                    "（无）".to_string()
                } else {
                    pending.join("、")
                }
            )
        }))
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
            started_at: 0,
            pause_note: String::new(),
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
        assert!(out.content.contains("下一条：case_id=`c2`"), "{}", out.content);
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

    #[tokio::test]
    async fn start_then_finish_task_lifecycle() {
        let store = store("lifecycle");
        seed_task(&store, &["c1"]);
        let start = TestStartTask::new(store.clone());
        let finish = TestFinishTask::new(store.clone());

        // 开工：pending → running + started_at 刷新。
        let out = start.call(json!({"task_id": "t1"})).await.unwrap();
        assert!(out.content.contains("开工已登记"), "{}", out.content);
        let t = &store.load_tasks()[0];
        assert_eq!(t.status, "running");
        assert!(t.started_at > 0);

        // 交付（未跑完）：→ paused + pause_note。
        let out = finish
            .call(json!({"task_id": "t1", "note": "0/1，接口服务 500 暂停"}))
            .await
            .unwrap();
        assert!(out.content.contains("paused"), "{}", out.content);
        let t = &store.load_tasks()[0];
        assert_eq!(t.status, "paused");
        assert!(t.pause_note.contains("500"));

        // 再开工 → 交付（全部终态）→ done。
        start.call(json!({"task_id": "t1"})).await.unwrap();
        let mut tasks = store.load_tasks();
        tasks[0].results[0].status = "pass".into();
        save_json(&store.tasks_path(), &tasks).unwrap();
        let out = finish
            .call(json!({"task_id": "t1", "note": "1/1 全部通过"}))
            .await
            .unwrap();
        assert!(out.content.contains("done"), "{}", out.content);
        assert_eq!(store.load_tasks()[0].status, "done");

        // done 任务不允许再开工。
        let out = start.call(json!({"task_id": "t1"})).await.unwrap();
        assert!(out.state == agentscope_core::message::ToolResultState::Error);
    }

    #[tokio::test]
    async fn task_id_by_name_fallback() {
        // LLM 常把任务名当 task_id 传：按 id 未命中时按名称唯一匹配接住。
        let store = store("byname");
        seed_task(&store, &["c1"]);
        let update = TestUpdateCase::new(store.clone());

        let out = update
            .call(json!({"task_id": "冒烟任务", "case_id": "c1", "status": "pass", "actual": "ok"}))
            .await
            .unwrap();
        assert!(out.content.contains("已回写") || out.content.contains("pass"), "{}", out.content);
        assert_eq!(store.load_tasks()[0].results[0].status, "pass");

        // 真不存在的还是报错，且错误信息附当前任务清单（LLM 可自愈换 id）。
        let out = update
            .call(json!({"task_id": "没有的任务", "case_id": "c1", "status": "pass", "actual": "x"}))
            .await
            .unwrap();
        assert!(out.state == agentscope_core::message::ToolResultState::Error);
        assert!(out.content.contains("当前任务清单"), "{}", out.content);
        assert!(out.content.contains("冒烟任务"), "{}", out.content);
    }
}
