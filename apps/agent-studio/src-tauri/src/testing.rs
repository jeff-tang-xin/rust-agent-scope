//! 测试工作区数据层：需求 / 测试用例 / 测试任务 / Bug 的持久化、导入解析与
//! 报告生成。
//!
//! 数据存 `<workspace_root>/.ox/testing/`（与记忆/黑板同规范，非业务数据
//! 不散落项目根）：
//! - `requirements.json` 需求列表
//! - `cases.json`        测试用例
//! - `tasks.json`        测试任务（含逐用例执行结果）
//! - `bugs.json`         Bug 报告
//!
//! 分工：用例生成由前端编排（sendMessage(mode="test") + 解析 agent 的
//! 结构化 JSON 回复，见 TestWorkbench），本模块负责数据 CRUD、Excel/XMind
//! 导入解析与报告生成；**任务执行编排已交给 LLM**（`test_run_task` 一次性
//! 下发待跑用例，LLM 调 `testing_tools` 自回写状态，后端监督者只保活）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

use crate::commands::{app_log, AppState};
use crate::factory;
use crate::session::AgentHandle;

// ---------- 数据结构 ----------

/// 需求（测试的对象；用例生成与任务关联的锚点）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Requirement {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub content: String,
    /// 被测系统当前的数据现状（数据量、时间范围、账号/权限分布等）。
    ///
    /// 用例里的分页页码、日期断言必须来自这里：数据只有 10 条却写「翻到第
    /// 100 页」、数据是 2025 年的却断言 2026 年，都是凭空捏造的假用例。
    #[serde(default)]
    pub data_profile: String,
    /// 需求附图（流程图/页面截图），文件名（`.ox/screenshots/` 下，复用
    /// test_read_screenshot 读取）。生成用例时作为图片块随提示词发给智能体——
    /// 文字说不清「页面怎么操作」时，图是唯一可靠的操作依据。
    #[serde(default)]
    pub images: Vec<String>,
    /// 需求澄清问答记录（"问：…／答：…"）。智能体判定信息不足时先提问，
    /// 用户回答后存回需求，下次生成不再重复问。
    #[serde(default)]
    pub clarifications: Vec<String>,
    /// 需求来源：`collab` = 协同工作区创建（讨论沉淀/协同面板手动建），
    /// 空/`test` = 测试侧创建。测试侧列表据此显示来源徽标。
    #[serde(default)]
    pub origin: String,
    #[serde(default)]
    pub created_at: u64,
}

/// 测试用例。`source` 记录来源：manual（手工）/ ai（需求生成）/ excel / xmind。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TestCase {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub precondition: String,
    #[serde(default)]
    pub steps: Vec<String>,
    #[serde(default)]
    pub expected: String,
    /// P1 | P2 | P3（空 = 未设置）。
    #[serde(default)]
    pub priority: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// 关联需求（空 = 未关联）。
    #[serde(default)]
    pub requirement_id: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub created_at: u64,
}

/// 单步执行结果。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StepResult {
    pub no: u32,
    /// pass | fail | blocked
    pub status: String,
    #[serde(default)]
    pub note: String,
}

/// 用例执行结果（任务内逐用例一条）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CaseResult {
    pub case_id: String,
    /// pending | running | pass | fail | blocked | skipped
    pub status: String,
    #[serde(default)]
    pub actual: String,
    #[serde(default)]
    pub steps: Vec<StepResult>,
    /// 截图文件名（`.ox/screenshots/` 下，不含路径）。
    #[serde(default)]
    pub screenshots: Vec<String>,
    /// 关联 Bug（fail/blocked 时由编排器落库后回填）。
    #[serde(default)]
    pub bug_id: String,
    /// 结果 JSON 解析失败时保留原始回复（排查与手工提取用）。
    #[serde(default)]
    pub raw_reply: String,
    #[serde(default)]
    pub started_at: u64,
    #[serde(default)]
    pub finished_at: u64,
}

/// 测试任务：关联需求与用例，执行结果逐用例记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestTask {
    pub id: String,
    pub name: String,
    /// 被测系统地址（执行 prompt 注入；空 = 用例自带完整地址）。
    #[serde(default)]
    pub base_url: String,
    /// 引用的测试配置（TestConfig.id；空 = 未引用，用 base_url 手填值）。
    #[serde(default)]
    pub config_id: String,
    #[serde(default)]
    pub requirement_ids: Vec<String>,
    #[serde(default)]
    pub case_ids: Vec<String>,
    /// pending | running | paused | done
    pub status: String,
    #[serde(default)]
    pub results: Vec<CaseResult>,
    /// 最近一次开工登记时刻（test_start_task 工具回写；turn 收尾据此
    /// 识别「本 turn 内开工」的任务并挂看门狗——对话触发路径的挂载点）。
    #[serde(default)]
    pub started_at: u64,
    /// 最近一次交付说明（test_finish_task 工具回写；paused 时前端展示）。
    #[serde(default)]
    pub pause_note: String,
    /// 已归档（列表默认隐藏，归档区查看；done 任务手动归档，防列表膨胀）。
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub finished_at: u64,
}

/// Bug 报告（fail/blocked 自动生成 + 手工补录）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bug {
    pub id: String,
    pub title: String,
    /// P1 | P2 | P3
    #[serde(default)]
    pub severity: String,
    /// 复现步骤（文本，编号列表）。
    #[serde(default)]
    pub steps: String,
    #[serde(default)]
    pub expected: String,
    #[serde(default)]
    pub actual: String,
    /// 截图文件名（`.ox/screenshots/` 下）。
    #[serde(default)]
    pub screenshots: Vec<String>,
    #[serde(default)]
    pub case_id: String,
    #[serde(default)]
    pub task_id: String,
    /// open | fixed | closed（协同修复后置 fixed，测试回归通过后置 closed）。
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub created_at: u64,
    /// 修复说明（协同修复 Bug 时由智能体/用户填写：改了什么、怎么修的）。
    #[serde(default)]
    pub fix_note: String,
}

/// 测试账号（可复用配置里的一组登录凭据；明文存本地 `.ox/testing/`，
/// 仅测试环境凭据，UI 侧密码输入框做遮蔽）。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct TestAccount {
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    /// 备注（如「管理员账号」「只读账号」）。
    #[serde(default)]
    pub note: String,
}

/// 测试配置：被测环境 + 测试账号等可复用资产，任务创建时引用（config_id），
/// 执行时由前端编排器把地址与账号注入执行提示词——账号密码只配一次，
/// 多个任务复用，不必每次重填。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TestConfig {
    #[serde(default)]
    pub id: String,
    /// 配置名（如「测试环境-管理员」）。
    #[serde(default)]
    pub name: String,
    /// 被测系统地址。
    #[serde(default)]
    pub base_url: String,
    /// 测试账号（可多组，执行时全部带给智能体备用）。
    #[serde(default)]
    pub accounts: Vec<TestAccount>,
    /// 环境说明 / 其他约定（如「测试数据随便造」「勿动生产配置」）。
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub created_at: u64,
}

/// 测试工作区全量数据（test_list_data 一次拉全；数据量为人工规模，无分页需求）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TestingData {
    #[serde(default)]
    pub requirements: Vec<Requirement>,
    #[serde(default)]
    pub cases: Vec<TestCase>,
    #[serde(default)]
    pub tasks: Vec<TestTask>,
    #[serde(default)]
    pub bugs: Vec<Bug>,
    /// 可复用测试配置（环境 + 账号）。
    #[serde(default)]
    pub configs: Vec<TestConfig>,
}

// ---------- 存储 ----------

/// 测试数据根：`<workspace_root>/.ox/testing`。测试工作区绑定协同工作区时
/// 跟随其目录——协同与测试读写同一份数据（打通的关键）。
pub(crate) fn testing_root(state: &AppState, workspace_id: &str) -> Result<PathBuf, String> {
    let config = state.snapshot();
    let ws = config
        .workspace(workspace_id)
        .ok_or_else(|| format!("unknown workspace `{workspace_id}`"))?;
    let root = factory::effective_workspace_root(&state.data_dir, &config, ws);
    Ok(root.join(".ox").join("testing"))
}

/// 读 JSON 文件；缺失/损坏按默认值（空表）处理——数据文件损坏不阻断面板打开。
pub(crate) fn load_json<T: Default + serde::de::DeserializeOwned>(path: &Path) -> T {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// 原子写：先写 `.tmp` 再 rename（进程崩溃不留半个文件）。
pub(crate) fn save_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create dir: {e}"))?;
    }
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| format!("serialize: {e}"))?;
    std::fs::write(&tmp, bytes).map_err(|e| format!("write: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename: {e}"))
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub(crate) fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn decode_b64(b64: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|e| format!("base64 解码失败: {e}"))
}

// ---------- 命令：数据 CRUD ----------

/// 全量拉取测试工作区数据（需求/用例/任务/Bug）。
#[tauri::command]
pub fn test_list_data(
    state: State<'_, AppState>,
    workspace_id: String,
) -> Result<TestingData, String> {
    let root = testing_root(&state, &workspace_id)?;
    Ok(TestingData {
        requirements: load_json(&root.join("requirements.json")),
        cases: load_json(&root.join("cases.json")),
        tasks: load_json(&root.join("tasks.json")),
        bugs: load_json(&root.join("bugs.json")),
        configs: load_json(&root.join("configs.json")),
    })
}

/// 新建/更新需求（id 空 = 新建）。
#[tauri::command]
pub fn test_save_requirement(
    state: State<'_, AppState>,
    workspace_id: String,
    requirement: Requirement,
) -> Result<Requirement, String> {
    if requirement.title.trim().is_empty() {
        return Err("需求标题不能为空".into());
    }
    let root = testing_root(&state, &workspace_id)?;
    let path = root.join("requirements.json");
    let mut items: Vec<Requirement> = load_json(&path);
    let mut requirement = requirement;
    match items.iter_mut().find(|r| r.id == requirement.id) {
        Some(existing) => {
            let created_at = existing.created_at;
            *existing = requirement.clone();
            existing.created_at = created_at;
        }
        None => {
            requirement.id = new_id();
            requirement.created_at = now_ms();
            items.push(requirement.clone());
        }
    }
    save_json(&path, &items)?;
    Ok(requirement)
}

/// 删除需求：关联用例的 requirement_id 置空（用例保留，只解除关联）。
#[tauri::command]
pub fn test_delete_requirement(
    state: State<'_, AppState>,
    workspace_id: String,
    id: String,
) -> Result<bool, String> {
    let root = testing_root(&state, &workspace_id)?;
    let req_path = root.join("requirements.json");
    let mut requirements: Vec<Requirement> = load_json(&req_path);
    let before = requirements.len();
    requirements.retain(|r| r.id != id);
    let deleted = requirements.len() != before;
    if deleted {
        save_json(&req_path, &requirements)?;
        let case_path = root.join("cases.json");
        let mut cases: Vec<TestCase> = load_json(&case_path);
        let mut touched = false;
        for case in cases.iter_mut() {
            if case.requirement_id == id {
                case.requirement_id = String::new();
                touched = true;
            }
        }
        if touched {
            save_json(&case_path, &cases)?;
        }
    }
    Ok(deleted)
}

/// 新建/更新用例（id 空 = 新建）。
#[tauri::command]
pub fn test_save_case(
    state: State<'_, AppState>,
    workspace_id: String,
    case: TestCase,
) -> Result<TestCase, String> {
    if case.title.trim().is_empty() {
        return Err("用例标题不能为空".into());
    }
    let root = testing_root(&state, &workspace_id)?;
    let path = root.join("cases.json");
    let mut items: Vec<TestCase> = load_json(&path);
    let mut case = case;
    match items.iter_mut().find(|c| c.id == case.id) {
        Some(existing) => {
            let (id, created_at) = (existing.id.clone(), existing.created_at);
            *existing = case.clone();
            existing.id = id;
            existing.created_at = created_at;
        }
        None => {
            case.id = new_id();
            case.created_at = now_ms();
            if case.source.is_empty() {
                case.source = "manual".into();
            }
            items.push(case.clone());
        }
    }
    save_json(&path, &items)?;
    Ok(case)
}

/// 删除用例。
#[tauri::command]
pub fn test_delete_case(
    state: State<'_, AppState>,
    workspace_id: String,
    id: String,
) -> Result<bool, String> {
    let root = testing_root(&state, &workspace_id)?;
    let path = root.join("cases.json");
    let mut items: Vec<TestCase> = load_json(&path);
    let before = items.len();
    items.retain(|c| c.id != id);
    let deleted = items.len() != before;
    if deleted {
        save_json(&path, &items)?;
    }
    Ok(deleted)
}

/// 批量导入用例（AI 生成确认后 / Excel、XMind 导入确认后）：补 id 与时间戳，
/// source 缺省按导入渠道由前端传。
#[tauri::command]
pub fn test_import_cases(
    state: State<'_, AppState>,
    workspace_id: String,
    cases: Vec<TestCase>,
) -> Result<Vec<TestCase>, String> {
    if cases.is_empty() {
        return Err("没有可导入的用例".into());
    }
    let root = testing_root(&state, &workspace_id)?;
    let path = root.join("cases.json");
    let mut items: Vec<TestCase> = load_json(&path);
    let ts = now_ms();
    let mut imported = Vec::with_capacity(cases.len());
    for mut case in cases {
        if case.title.trim().is_empty() {
            continue;
        }
        case.id = new_id();
        case.created_at = ts;
        if case.source.is_empty() {
            case.source = "manual".into();
        }
        items.push(case.clone());
        imported.push(case);
    }
    if imported.is_empty() {
        return Err("导入内容里没有有效用例（标题全为空）".into());
    }
    save_json(&path, &items)?;
    Ok(imported)
}

/// 新建/更新任务（id 空 = 新建；新建时按 case_ids 初始化逐用例 pending 结果）。
/// 编辑只改元数据，不动已有执行结果（重置执行走 test_update_task 全量替换）。
#[tauri::command]
pub fn test_save_task(
    state: State<'_, AppState>,
    workspace_id: String,
    task: TestTask,
) -> Result<TestTask, String> {
    if task.name.trim().is_empty() {
        return Err("任务名称不能为空".into());
    }
    if task.case_ids.is_empty() {
        return Err("任务至少关联一个测试用例".into());
    }
    let root = testing_root(&state, &workspace_id)?;
    let path = root.join("tasks.json");
    let mut items: Vec<TestTask> = load_json(&path);
    let mut task = task;
    match items.iter_mut().find(|t| t.id == task.id) {
        Some(existing) => {
            let (id, created_at, results, status) = (
                existing.id.clone(),
                existing.created_at,
                existing.results.clone(),
                existing.status.clone(),
            );
            *existing = task.clone();
            existing.id = id;
            existing.created_at = created_at;
            existing.results = results;
            existing.status = status;
        }
        None => {
            task.id = new_id();
            task.created_at = now_ms();
            task.status = "pending".into();
            task.results = task
                .case_ids
                .iter()
                .map(|cid| CaseResult {
                    case_id: cid.clone(),
                    status: "pending".into(),
                    actual: String::new(),
                    steps: Vec::new(),
                    screenshots: Vec::new(),
                    bug_id: String::new(),
                    raw_reply: String::new(),
                    started_at: 0,
                    finished_at: 0,
                })
                .collect();
            items.push(task.clone());
        }
    }
    save_json(&path, &items)?;
    Ok(task)
}

/// 全量替换任务（编排器每跑完一个用例回写进度；也用于重置执行结果）。
#[tauri::command]
pub fn test_update_task(
    state: State<'_, AppState>,
    workspace_id: String,
    task: TestTask,
) -> Result<(), String> {
    let root = testing_root(&state, &workspace_id)?;
    let path = root.join("tasks.json");
    let mut items: Vec<TestTask> = load_json(&path);
    match items.iter_mut().find(|t| t.id == task.id) {
        Some(existing) => *existing = task,
        None => return Err(format!("任务 `{}` 不存在", task.id)),
    }
    save_json(&path, &items)
}

/// 任务归档/取消归档（done 任务手动归档，列表默认只看未归档的）。
#[tauri::command]
pub fn test_archive_task(
    state: State<'_, AppState>,
    workspace_id: String,
    task_id: String,
    archived: bool,
) -> Result<(), String> {
    let root = testing_root(&state, &workspace_id)?;
    let path = root.join("tasks.json");
    let mut items: Vec<TestTask> = load_json(&path);
    match items.iter_mut().find(|t| t.id == task_id) {
        Some(t) => t.archived = archived,
        None => return Err(format!("任务 `{task_id}` 不存在")),
    }
    save_json(&path, &items)
}

// ---------- 任务执行：LLM 自主编排 + 后端监督保活 ----------

/// 编排进度事件（`test-orchestration`）：前端测试面板据此更新进度与状态。
/// 「每一步都在界面上体现」的载体——LLM 的工具调用卡片走 agent-event 流，
/// 编排层的状态变化（等待授权/自动续跑/收尾）走本事件。
#[derive(Debug, Clone, Serialize)]
pub struct TestOrchestration {
    pub workspace_id: String,
    pub task_id: String,
    /// started | waiting_confirm | resumed | done | stopped | aborted
    pub phase: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// 已到终态的用例数 / 计划执行总数。
    pub done_count: usize,
    pub total: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct TestRunStarted {
    pub turn_id: String,
    /// 本次交给 LLM 的用例数。
    pub targets: usize,
    /// 跳过数（已通过 / 待修 Bug 未处理）。
    pub skipped: usize,
    /// true = 该任务本就在看门狗看护下执行（重复点火），本次未做任何事；
    /// 前端据此恢复「执行中」显示而不是报错。
    #[serde(default)]
    pub already_running: bool,
}

/// 启动测试任务：筛出待跑用例一次性交给测试智能体（LLM 自主编排），
/// 并拉起后端看门狗保活。
///
/// 编排权在 LLM：逐条执行、每条完成调 `test_update_case` 回写状态、
/// fail/blocked 调 `test_submit_bug` 提 Bug。后端不做逐条编排——
/// 旧「前端逐条发消息 + 解析回复 JSON」模式在授权挂起时会撞墙刷屏
/// （每条后续消息都报 invalid input），且页面一关编排就断。
///
/// 看门狗只做保活：轮询 session 状态，LLM 停转（模型 5xx / 步数耗尽）
/// 而任务未完成时自动续跑；等授权（HITL 挂起）时原地等待用户裁决，
/// 绝不代答。注册表 session_id → task_id：一个会话同时只跑一个任务，
/// interrupt_turn 据此让看门狗退场（人接管，狗不拉起）。
#[tauri::command]
pub async fn test_run_task(
    app: AppHandle,
    state: State<'_, AppState>,
    workspace_id: String,
    mode: String,
    session_id: String,
    task_id: String,
    force: bool,
) -> Result<TestRunStarted, String> {
    // 防重入：同一会话已有任务在看门狗看护下执行。
    // 同一任务重复点火（刷新页面后点「继续执行」/手抖双击）是幂等场景——
    // 返回 already_running 让前端恢复显示，不算错误；不同任务才是真冲突。
    {
        let runs = state
            .test_runs
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(running) = runs.get(&session_id) {
            if *running == task_id {
                return Ok(TestRunStarted {
                    turn_id: String::new(),
                    targets: 0,
                    skipped: 0,
                    already_running: true,
                });
            }
            return Err(format!(
                "该会话正在执行任务 `{running}`（看门狗看护中）；先停止或等它完成"
            ));
        }
    }

    let root = testing_root(&state, &workspace_id)?;
    let data = TestingData {
        requirements: load_json(&root.join("requirements.json")),
        cases: load_json(&root.join("cases.json")),
        tasks: load_json(&root.join("tasks.json")),
        bugs: load_json(&root.join("bugs.json")),
        configs: load_json(&root.join("configs.json")),
    };
    let task = data
        .tasks
        .iter()
        .find(|t| t.id == task_id)
        .cloned()
        .ok_or_else(|| format!("任务 `{task_id}` 不存在"))?;

    // 筛待跑用例：force 全量重跑；否则只跑「没跑完的（pending/running，
    // 含上次中断残留）」与「Bug 已修复的（回归验证）」——已通过的和
    // 待修 Bug 未处理（修完再跑才有意义）的跳过。
    let latest_bug_by_case: HashMap<String, Bug> = {
        let mut m: HashMap<String, Bug> = HashMap::new();
        for b in &data.bugs {
            if b.case_id.is_empty() {
                continue;
            }
            match m.get(&b.case_id) {
                Some(prev) if prev.created_at >= b.created_at => {}
                _ => {
                    m.insert(b.case_id.clone(), b.clone());
                }
            }
        }
        m
    };
    let mut skipped_passed = 0usize;
    let mut skipped_open_bug = 0usize;
    let targets: Vec<TestCase> = task
        .case_ids
        .iter()
        .filter_map(|cid| data.cases.iter().find(|c| &c.id == cid).cloned())
        .filter(|c| {
            if force {
                return true;
            }
            let st = task
                .results
                .iter()
                .find(|r| r.case_id == c.id)
                .map(|r| r.status.as_str())
                .unwrap_or("pending");
            if matches!(st, "pending" | "running") {
                return true;
            }
            if st == "pass" {
                skipped_passed += 1;
                return false;
            }
            match latest_bug_by_case.get(&c.id) {
                Some(b) if b.status == "open" => {
                    skipped_open_bug += 1;
                    false
                }
                _ => true,
            }
        })
        .collect();
    if targets.is_empty() {
        return Err(format!(
            "没有需要执行的用例：已通过 {skipped_passed} 条、待修 Bug 未处理 {skipped_open_bug} 条（全量重跑请用「全量重跑」）"
        ));
    }

    // 初始化结果槽：每条 target 一个 running 槽（LLM 只能更新这些槽）。
    let now = now_ms();
    let mut results: Vec<CaseResult> = Vec::new();
    for r in &task.results {
        if targets.iter().any(|c| c.id == r.case_id) {
            results.push(CaseResult {
                started_at: now,
                bug_id: String::new(),
                raw_reply: String::new(),
                ..r.clone()
            });
        } else {
            results.push(r.clone());
        }
    }
    for c in &targets {
        if !results.iter().any(|r| r.case_id == c.id) {
            results.push(CaseResult {
                case_id: c.id.clone(),
                status: "running".into(),
                started_at: now,
                ..Default::default()
            });
        }
    }
    {
        let path = root.join("tasks.json");
        let mut items: Vec<TestTask> = load_json(&path);
        if let Some(t) = items.iter_mut().find(|t| t.id == task_id) {
            t.results = results;
            t.status = "running".into();
            t.finished_at = 0;
        }
        save_json(&path, &items)?;
    }

    // 执行提示词：任务/环境/账号/数据现状 + 待跑用例全量内容 + 执行协议。
    let config = if task.config_id.is_empty() {
        None
    } else {
        data.configs
            .iter()
            .find(|c| c.id == task.config_id)
            .cloned()
    };
    let prompt = build_exec_prompt(&task, &targets, config.as_ref(), &data);
    // 需求附图随消息发给智能体（执行时页面长什么样，图比文字可靠）。
    let attachments = load_requirement_attachments(&root, &targets, &data);

    let handle = state.handle(&workspace_id, &mode)?;
    // 看门狗注册（session_id → task_id）后拉起：它每 3s 巡检 session 状态，
    // 首条消息由下面 start_turn 同步发出，无事件竞态。
    {
        let mut runs = state.test_runs.lock().unwrap_or_else(|e| e.into_inner());
        runs.insert(session_id.clone(), task_id.clone());
    }
    let dog = TestWatchdog {
        app: app.clone(),
        handle: Arc::clone(&handle),
        workspace_id: workspace_id.clone(),
        session_id: session_id.clone(),
        task_id: task_id.clone(),
        task_name: task.name.clone(),
        ox_root: root.clone(),
        runs: Arc::clone(&state.test_runs),
    };
    tokio::spawn(dog.run());

    let turn = crate::commands::start_turn(
        &app,
        &handle,
        &workspace_id,
        &session_id,
        prompt,
        (!attachments.is_empty()).then_some(attachments),
        None,
        None,
    )
    .await;

    // 启动失败（会话忙等）：注销看门狗（它下轮巡检发现注册表里没有自己
    // 就退场），返回错误。turn 忙常见于「用户停止后自己在对话里触发了执行」
    // ——智能体本就在跑，提示无需重新点火而不是甩英文报错。
    if let Err(e) = &turn {
        state
            .test_runs
            .lock()
            .unwrap_or_else(|e2| e2.into_inner())
            .remove(&session_id);
        let msg = e.clone();
        if msg.contains("already running") {
            return Err(
                "会话正在对话中：智能体可能正在执行任务，无需重新点火，等它回写结果即可；\
                 若是普通对话，请等它结束再执行任务"
                    .into(),
            );
        }
        return Err(msg);
    }
    let turn = turn.unwrap();
    let _ = app.emit(
        "test-orchestration",
        TestOrchestration {
            workspace_id,
            task_id,
            phase: "started".into(),
            message: Some(format!(
                "已交给测试智能体执行 {} 条用例（跳过 {} 条）",
                targets.len(),
                skipped_passed + skipped_open_bug
            )),
            done_count: 0,
            total: targets.len(),
        },
    );
    Ok(TestRunStarted {
        turn_id: turn.turn_id,
        targets: targets.len(),
        skipped: skipped_passed + skipped_open_bug,
        already_running: false,
    })
}

/// 查询会话当前在看门狗看护下执行的任务（无则返回 null）。
/// 前端加载/刷新后据此恢复「执行中」显示——编排循环活在后端，
/// 刷新页面不中断执行，但前端的执行中状态是内存态，需要重新对齐。
#[tauri::command]
pub async fn test_run_active(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<Option<String>, String> {
    let runs = state
        .test_runs
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    Ok(runs.get(&session_id).cloned())
}

/// 需求附图 → 附件块：读 `.ox/screenshots/<name>` 转 base64（与前端
/// loadRequirementImages 同源；单张读不到不阻断执行）。
fn load_requirement_attachments(
    root: &Path,
    targets: &[TestCase],
    data: &TestingData,
) -> Vec<crate::commands::AttachmentInput> {
    let mut names: Vec<String> = Vec::new();
    for c in targets {
        if c.requirement_id.is_empty() {
            continue;
        }
        if let Some(r) = data.requirements.iter().find(|r| r.id == c.requirement_id) {
            for img in &r.images {
                if !names.contains(img) {
                    names.push(img.clone());
                }
            }
        }
    }
    let shots = root.join("..").join("screenshots");
    let mut out = Vec::new();
    for name in names {
        let ext = name.rsplit('.').next().unwrap_or("png").to_lowercase();
        let media_type = match ext.as_str() {
            "jpg" | "jpeg" => "image/jpeg",
            "webp" => "image/webp",
            "gif" => "image/gif",
            _ => "image/png",
        };
        if let Ok(bytes) = std::fs::read(shots.join(&name)) {
            use base64::Engine as _;
            out.push(crate::commands::AttachmentInput {
                name,
                media_type: media_type.into(),
                data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
            });
        }
    }
    out
}

/// 执行提示词：任务/环境/账号/数据现状 + 待跑用例全量内容 + 执行协议。
/// （旧模式这段在前端 execPrompt 逐条构造；编排交给 LLM 后一次下发全部。）
fn build_exec_prompt(
    task: &TestTask,
    targets: &[TestCase],
    config: Option<&TestConfig>,
    data: &TestingData,
) -> String {
    let base_url = config
        .map(|c| c.base_url.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| task.base_url.clone());
    let mut p = String::new();
    p.push_str(&format!(
        "执行测试任务「{}」（task_id=`{}`，共 {} 条用例，按顺序逐条执行）。\n\n",
        task.name,
        task.id,
        targets.len()
    ));
    p.push_str(&format!(
        "工具回写（test_update_case / test_submit_bug / test_start_task / test_finish_task）的 task_id 参数一律传上面的 task_id（`{}`）——它是任务 id 不是任务名，传任务名会报「任务不存在」。\n\n",
        task.id
    ));
    p.push_str(&format!(
        "被测系统地址：{}\n",
        if base_url.is_empty() {
            "（未填写，按用例步骤中的地址）"
        } else {
            &base_url
        }
    ));
    if let Some(cfg) = config {
        if !cfg.accounts.is_empty() {
            p.push_str("测试账号（如需登录，按顺序尝试；全部失败记 blocked 并说明）：\n");
            for (i, a) in cfg.accounts.iter().enumerate() {
                p.push_str(&format!(
                    "{}. 用户名：{}  密码：{}{}\n",
                    i + 1,
                    a.username,
                    a.password,
                    if a.note.is_empty() {
                        String::new()
                    } else {
                        format!("（{}）", a.note)
                    }
                ));
            }
        }
        if !cfg.notes.is_empty() {
            p.push_str(&format!("环境约定：{}\n", cfg.notes));
        }
    }
    // 数据现状（用例里的具体值必须以此为准）。
    let mut profiles: Vec<String> = Vec::new();
    for c in targets {
        if c.requirement_id.is_empty() {
            continue;
        }
        if let Some(r) = data.requirements.iter().find(|r| r.id == c.requirement_id) {
            if !r.data_profile.is_empty() && !profiles.contains(&r.data_profile) {
                profiles.push(r.data_profile.clone());
            }
        }
    }
    if !profiles.is_empty() {
        p.push_str("\n被测系统当前真实情况（用例里的具体值都必须以此为准，不要臆造）：\n");
        for pr in &profiles {
            p.push_str(pr);
            p.push('\n');
        }
    }
    p.push_str("\n## 待执行用例\n");
    for (i, c) in targets.iter().enumerate() {
        p.push_str(&format!(
            "\n### 用例 {} [case_id={}]（{}）\n",
            i + 1,
            c.id,
            if c.priority.is_empty() { "P2" } else { &c.priority }
        ));
        p.push_str(&format!("标题：{}\n", c.title));
        p.push_str(&format!(
            "前置条件：{}\n",
            if c.precondition.is_empty() { "无" } else { &c.precondition }
        ));
        p.push_str("测试步骤：\n");
        if c.steps.is_empty() {
            p.push_str("（无步骤，按标题理解执行）\n");
        } else {
            for (si, s) in c.steps.iter().enumerate() {
                p.push_str(&format!("{}. {}\n", si + 1, s));
            }
        }
        p.push_str(&format!("预期结果：{}\n", c.expected));
    }
    p.push_str(
        "\n## 执行协议（状态只认工具回写）\n\
1. 按顺序逐条执行：一条用例从第一步走到结论，状态回写后才开始下一条；不要并行穿插多条用例。\n\
2. 每条用例执行完**立即调 test_update_case**（task_id 见上，case_id 见用例标题行）——只在回复里写结果等于没执行，任务面板不会更新。\n\
3. fail/blocked：先 test_update_case 记状态，再调 test_submit_bug 提 Bug（工具自动防重复：同用例已有待处理 Bug 会返回既有条目，此时不要重复提交）。\n\
4. 接口步骤用 api_request；页面步骤用浏览器工具（视觉驱动：截图 → 坐标操作），关键节点截图留证并把文件名写进 screenshots。\n\
5. 某一步失败或被阻塞：立即停止该用例，不重试、不换路径、不绕过，如实记 fail/blocked。\n\
6. 全部用例回写完毕后，回复一段汇总：每条用例一行「标题 → 结果」。\n\
7. 输出纪律：思维链只输出核心决策节点（每个节点 1 个短句，禁止展开推导过程）；工具调用直接给出最终执行参数，中间校验步骤省略；回复末尾在第 6 条用例清单之后，按【完成项 / 遗留问题】两栏总结本轮（每条不超过 15 字）。\n",
    );
    p
}

/// 任务看门狗：轮询 session 状态保活，等授权时原地等待。
///
/// 测试任务跑在绑定 session 里，看门狗只看两个事实源：
/// 1. **session 状态**（`is_running`）：在跑 = LLM 在干活，不干预；
///    空闲 = LLM 停转（模型 5xx / 步数耗尽 / 正常结束但没跑完）。
/// 2. **任务落库状态**（tasks.json，LLM 调工具回写即真相）：全部结果槽
///    终态 = 任务完成；有 ASKING 挂起 = 等用户授权，绝不代答。
///
/// 空闲且未完成且无挂起 → 踢一脚续跑（保活；每个新 turn 重置步数预算，
/// 顺带解决单轮 max_iters 不够长任务的问题）。上限：连续 5 次续跑无进展
/// 放弃、总续跑 30 轮兜底——监督是让任务能跑完，不是无限重试机器人。
/// 用户中断（interrupt_turn）会移除看门狗：人接管了，狗退场。
struct TestWatchdog {
    app: AppHandle,
    handle: Arc<AgentHandle>,
    workspace_id: String,
    session_id: String,
    task_id: String,
    task_name: String,
    /// `.ox` 根（读 tasks.json 判断任务状态）。
    ox_root: PathBuf,
    /// 活跃看门狗注册表（session_id → task_id）：interrupt_turn 据此退场，
    /// test_run_task 据此防同 session 双任务。
    runs: Arc<std::sync::Mutex<HashMap<String, String>>>,
}

/// turn 链携带的测试监督上下文（start_turn 由 send_message 构造传入；
/// 点火与看门狗续跑路径传 None——它们自己已挂看门狗）。turn 收尾据此
/// 认领「本 turn 内开工」的任务，覆盖对话触发执行的看门狗挂载。
pub(crate) struct TestWatchdogCtx {
    pub handle: Arc<AgentHandle>,
    /// `.ox` 根（tasks.json 在 `<ox_root>/testing/` 下）。
    pub ox_root: PathBuf,
    pub runs: Arc<std::sync::Mutex<HashMap<String, String>>>,
}

/// turn 收尘认领：本 turn 内开工（test_start_task 登记 started_at）且尚无
/// 看门狗的 running 任务 → 挂看门狗 + 广播 running。这是「对话触发执行」
/// 的监督挂载点——用户在会话里让智能体跑任务，LLM 调开工登记，turn 结束
/// 后监督自动跟上，与点火路径（test_run_task 自己挂）殊途同归。
pub(crate) fn adopt_running_task(
    app: &AppHandle,
    ctx: &TestWatchdogCtx,
    workspace_id: &str,
    session_id: &str,
    turn_started_ms: u64,
) {
    // 已有看门狗在跑（点火路径挂的）→ 不重复挂。
    if ctx
        .runs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(session_id)
    {
        return;
    }
    let tasks: Vec<TestTask> = load_json(&ctx.ox_root.join("testing").join("tasks.json"));
    // 本 turn 期间开工的 running 任务（取最新一条；历史残留 running 的
    // started_at 早于本 turn，不会被认领）。
    let Some(task) = tasks
        .iter()
        .filter(|t| t.status == "running" && t.started_at >= turn_started_ms)
        .max_by_key(|t| t.started_at)
        .cloned()
    else {
        return;
    };
    if task
        .results
        .iter()
        .all(|r| matches!(r.status.as_str(), "pass" | "fail" | "blocked" | "skipped" | "error"))
    {
        return; // 全部终态，无需监督
    }
    ctx.runs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(session_id.to_string(), task.id.clone());
    let dog = TestWatchdog {
        app: app.clone(),
        handle: Arc::clone(&ctx.handle),
        workspace_id: workspace_id.to_string(),
        session_id: session_id.to_string(),
        task_id: task.id.clone(),
        task_name: task.name.clone(),
        ox_root: ctx.ox_root.clone(),
        runs: Arc::clone(&ctx.runs),
    };
    let (done, total, _) = dog.task_progress();
    dog.emit(
        "running",
        Some("检测到执行开工，看门狗已接管保活".into()),
        done,
        total,
    );
    app_log(
        "INFO",
        &format!(
            "测试看门狗认领开工任务 task={} session={session_id}（对话触发路径）",
            task.id
        ),
    );
    tokio::spawn(dog.run());
}

impl TestWatchdog {
    fn tasks_path(&self) -> PathBuf {
        self.ox_root.join("testing").join("tasks.json")
    }

    /// 编排进度事件；`dedup` 相同 phase 不重复发（running/waiting_confirm
    /// 是持续态，每 3s 巡检一次不能刷屏）。
    fn emit(&self, phase: &str, message: Option<String>, done: usize, total: usize) {
        let _ = self.app.emit(
            "test-orchestration",
            TestOrchestration {
                workspace_id: self.workspace_id.clone(),
                task_id: self.task_id.clone(),
                phase: phase.into(),
                message,
                done_count: done,
                total,
            },
        );
    }

    /// 会话是否在跑（LLM 执行中）。
    async fn session_running(&self) -> bool {
        self.handle
            .session(&self.session_id)
            .await
            .is_running()
            .await
    }

    /// 看门狗主循环：每 3s 巡检一次，直到任务完成 / 被中断 / 放弃。
    async fn run(self) {
        const POLL: std::time::Duration = std::time::Duration::from_secs(3);
        const MAX_STALLED: u32 = 5;
        const MAX_ROUNDS: u32 = 30;

        let mut last_phase = String::new();
        let mut stalled: u32 = 0;
        let mut resumed_rounds: u32 = 0;
        let mut kick_failures: u32 = 0;
        const MAX_KICK_FAILURES: u32 = 3;
        let mut last_done: usize = self.done_count();
        let mut reason = String::new();

        loop {
            tokio::time::sleep(POLL).await;
            // 注册表里自己还在吗？不在 = 用户已中断/停止（interrupt_turn
            // 移除）——人接管了，狗退场，绝不把任务再踢起来。
            let alive = self
                .runs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&self.session_id)
                == Some(&self.task_id);
            if !alive {
                reason = "任务已被手动中断或停止".into();
                break;
            }
            // 在跑 = LLM 在干活，不干预。
            if self.session_running().await {
                let (done, total, _) = self.task_progress();
                // 进度变化即推送：LLM 逐条回写 tasks.json，前端用例列表跟着
                // 实时刷新（否则面板冻结在进入执行的时刻，必须手动刷新才能
                // 看到进展）。同 phase 且进度没变不重发（防刷屏）。
                if last_phase != "running" || done != last_done {
                    self.emit("running", None, done, total);
                    last_phase = "running".into();
                    last_done = done;
                }
                continue;
            }
            // 空闲：查任务状态（LLM 工具回写即真相）。
            let Some(task) = self.load_task() else {
                reason = "任务已被删除".into();
                break;
            };
            // 正常交付暂停（LLM 调 test_finish_task 置 paused）：本轮执行
            // 已按协议交付，不是失联——退场不踢，等用户对话或手动点火续跑。
            if task.status == "paused" {
                let (done, total, _) = self.task_progress();
                self.emit(
                    "paused",
                    Some(format!(
                        "任务「{}」已交付暂停：{}",
                        self.task_name,
                        if task.pause_note.is_empty() {
                            "（无说明）".to_string()
                        } else {
                            task.pause_note.clone()
                        }
                    )),
                    done,
                    total,
                );
                reason = "LLM 已交付暂停".into();
                break;
            }
            let total = task.results.len();
            let done = task
                .results
                .iter()
                .filter(|r| {
                    matches!(r.status.as_str(), "pass" | "fail" | "blocked" | "skipped" | "error")
                })
                .count();
            if task.status == "done" || (total > 0 && done == total) {
                self.emit(
                    "done",
                    Some(format!("任务「{}」全部用例执行完毕", self.task_name)),
                    done,
                    total,
                );
                break;
            }
            // 等授权：会话有挂起的工具确认 → 原地等待用户裁决，绝不代答、
            // 不续跑（批准/拒绝后 LLM 继续跑，看门狗下轮巡检自然跟进）。
            if self.session_asking().await {
                if last_phase != "waiting_confirm" {
                    self.emit(
                        "waiting_confirm",
                        Some("智能体在等待工具授权——请在会话中批准或拒绝".into()),
                        done,
                        total,
                    );
                    last_phase = "waiting_confirm".into();
                }
                continue;
            }
            // 空闲 + 未完成 + 无挂起：LLM 停转了。先看任务状态有没有更新——
            // 有进展说明在推进（失败只是暂时的），重置无进展计数。
            if done > last_done {
                stalled = 0;
                last_done = done;
            } else {
                stalled += 1;
            }
            if stalled > MAX_STALLED {
                let (done, total, _) = self.task_progress();
                self.emit(
                    "aborted",
                    Some(format!(
                        "连续 {stalled} 次巡检毫无进展（{done}/{total}），已停止保活。请检查模型服务后在任务面板手动继续"
                    )),
                    done,
                    total,
                );
                self.mark_paused("连续巡检无进展，已停止保活");
                reason = format!("续跑无进展放弃（{done}/{total}）");
                break;
            }
            resumed_rounds += 1;
            if resumed_rounds > MAX_ROUNDS {
                let (done, total, _) = self.task_progress();
                self.emit(
                    "aborted",
                    Some(format!(
                        "自动续跑已达 {MAX_ROUNDS} 轮上限仍未完成（{done}/{total}），已停止保活。请查看会话排查后手动继续"
                    )),
                    done,
                    total,
                );
                self.mark_paused("自动续跑轮数超限，已停止保活");
                reason = "续跑轮数超限".into();
                break;
            }
            let remaining: Vec<String> = task
                .results
                .iter()
                .filter(|r| {
                    !matches!(r.status.as_str(), "pass" | "fail" | "blocked" | "skipped" | "error")
                })
                .map(|r| r.case_id.clone())
                .collect();
            if remaining.is_empty() {
                // 结果槽全部终态但任务状态没跟上（旧数据）——不续跑。
                self.emit("done", Some("全部用例已有结论".into()), done, total);
                break;
            }
            self.emit(
                "resumed",
                Some(format!(
                    "检测到执行中断，自动续跑剩余 {} 条用例",
                    remaining.len()
                )),
                done,
                total,
            );
            last_phase = "resumed".into();
            let msg = format!(
                "任务「{}」（task_id=`{}`）尚未完成（执行中断）。请继续执行剩余用例（case_id：{}）。\
                 纪律不变：回写与登记工具的 task_id 一律传 `{}`（不是任务名，也不是会话历史里出现过的其他 id），\
                 每条完成立即调 test_update_case 回写状态，\
                 fail/blocked 再调 test_submit_bug；此前已回写过结论的用例不要重跑。",
                self.task_name,
                self.task_id,
                remaining.join(", "),
                self.task_id
            );
            // 发送失败（模型服务连不上/网络断等环境问题）：连续计数，超过
            // 阈值判定环境异常——放弃保活并置 paused（不是任务问题，是环境
            // 问题，恢复后用户手动继续）。
            if crate::commands::start_turn(
                &self.app,
                &self.handle,
                &self.workspace_id,
                &self.session_id,
                msg,
                None,
                None,
                None,
            )
            .await
            .is_err()
            {
                kick_failures += 1;
                app_log(
                    "WARN",
                    &format!(
                        "测试看门狗续跑发送失败（{kick_failures}/{MAX_KICK_FAILURES}）task={}",
                        self.task_id
                    ),
                );
                if kick_failures >= MAX_KICK_FAILURES {
                    let (done, total, _) = self.task_progress();
                    self.emit(
                        "aborted",
                        Some(format!(
                            "连续 {kick_failures} 次续跑发送失败（{done}/{total}），判定环境异常（模型服务/网络不可达），已停止保活。恢复后在任务面板手动继续"
                        )),
                        done,
                        total,
                    );
                    self.mark_paused("环境异常：续跑发送连续失败");
                    reason = "环境异常（续跑发送连续失败）".into();
                    break;
                }
            } else {
                kick_failures = 0;
            }
        }
        // 退出清理：注册表里还是自己才移除（防误删后来者）。
        let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        if runs.get(&self.session_id) == Some(&self.task_id) {
            runs.remove(&self.session_id);
        }
        drop(runs);
        if !reason.is_empty() {
            app_log(
                "INFO",
                &format!("测试看门狗退出 task={}: {reason}", self.task_id),
            );
        }
    }

    fn load_task(&self) -> Option<TestTask> {
        let tasks: Vec<TestTask> = load_json(&self.tasks_path());
        tasks.into_iter().find(|t| t.id == self.task_id)
    }

    /// 放弃保活时把任务置 paused（附原因）：不留 running 残留——turn 收尾
    /// 的认领逻辑按 started_at 识别「本 turn 开工」，残留 running 会被误认领。
    fn mark_paused(&self, note: &str) {
        let path = self.tasks_path();
        let mut tasks: Vec<TestTask> = load_json(&path);
        if let Some(t) = tasks.iter_mut().find(|t| t.id == self.task_id) {
            if t.status == "running" {
                t.status = "paused".into();
                t.pause_note = note.to_string();
                save_json(&path, &tasks);
            }
        }
    }

    fn task_progress(&self) -> (usize, usize, bool) {
        match self.load_task() {
            Some(t) => {
                let total = t.results.len();
                let done = t
                    .results
                    .iter()
                    .filter(|r| {
                        matches!(r.status.as_str(), "pass" | "fail" | "blocked" | "skipped" | "error")
                    })
                    .count();
                (done, total, total > 0 && done == total)
            }
            None => (0, 0, false),
        }
    }

    fn done_count(&self) -> usize {
        self.task_progress().0
    }

    /// 会话是否有挂起的工具授权（HITL ASKING）。
    async fn session_asking(&self) -> bool {
        match self
            .handle
            .agent
            .delegate()
            .agent_state_for(Some(crate::commands::USER_ID), &self.session_id)
        {
            Ok(st) => !agentscope_core::message::asking_tool_calls(&st.context).is_empty(),
            Err(_) => false,
        }
    }
}

/// 删除任务（执行结果与任务一起删；Bug 保留——bug 是独立资产）。
#[tauri::command]
pub fn test_delete_task(
    state: State<'_, AppState>,
    workspace_id: String,
    id: String,
) -> Result<bool, String> {
    let root = testing_root(&state, &workspace_id)?;
    let path = root.join("tasks.json");
    let mut items: Vec<TestTask> = load_json(&path);
    let before = items.len();
    items.retain(|t| t.id != id);
    let deleted = items.len() != before;
    if deleted {
        save_json(&path, &items)?;
    }
    Ok(deleted)
}

/// 新建/更新 Bug（id 空 = 新建）。
#[tauri::command]
pub fn test_save_bug(
    state: State<'_, AppState>,
    workspace_id: String,
    bug: Bug,
) -> Result<Bug, String> {
    if bug.title.trim().is_empty() {
        return Err("Bug 标题不能为空".into());
    }
    let root = testing_root(&state, &workspace_id)?;
    let path = root.join("bugs.json");
    let mut items: Vec<Bug> = load_json(&path);
    let mut bug = bug;
    match items.iter_mut().find(|b| b.id == bug.id) {
        Some(existing) => {
            let (id, created_at) = (existing.id.clone(), existing.created_at);
            *existing = bug.clone();
            existing.id = id;
            existing.created_at = created_at;
        }
        None => {
            bug.id = new_id();
            bug.created_at = now_ms();
            if bug.status.is_empty() {
                bug.status = "open".into();
            }
            items.push(bug.clone());
        }
    }
    save_json(&path, &items)?;
    Ok(bug)
}

/// 删除 Bug。
#[tauri::command]
pub fn test_delete_bug(
    state: State<'_, AppState>,
    workspace_id: String,
    id: String,
) -> Result<bool, String> {
    let root = testing_root(&state, &workspace_id)?;
    let path = root.join("bugs.json");
    let mut items: Vec<Bug> = load_json(&path);
    let before = items.len();
    items.retain(|b| b.id != id);
    let deleted = items.len() != before;
    if deleted {
        save_json(&path, &items)?;
    }
    Ok(deleted)
}

/// 新建/更新测试配置（id 空 = 新建）。配置是被测环境与账号的可复用资产，
/// 任务按 config_id 引用；删除配置不影响已有任务（任务里保留 base_url 快照）。
#[tauri::command]
pub fn test_save_config(
    state: State<'_, AppState>,
    workspace_id: String,
    config: TestConfig,
) -> Result<TestConfig, String> {
    if config.name.trim().is_empty() {
        return Err("配置名不能为空".into());
    }
    let root = testing_root(&state, &workspace_id)?;
    let path = root.join("configs.json");
    let mut items: Vec<TestConfig> = load_json(&path);
    let mut config = config;
    config.name = config.name.trim().to_string();
    // 账号行去掉全空行（用户点了「添加账号」没填就保存）。
    config
        .accounts
        .retain(|a| !a.username.trim().is_empty() && !a.password.trim().is_empty());
    match items.iter_mut().find(|c| c.id == config.id) {
        Some(existing) => {
            let created_at = existing.created_at;
            *existing = config.clone();
            existing.created_at = created_at;
        }
        None => {
            config.id = new_id();
            config.created_at = now_ms();
            items.push(config.clone());
        }
    }
    save_json(&path, &items)?;
    Ok(config)
}

/// 删除测试配置。引用它的任务不级联改（config_id 悬空时编排器回退用任务
/// 自带的 base_url，前端展示「配置已删除」）。
#[tauri::command]
pub fn test_delete_config(
    state: State<'_, AppState>,
    workspace_id: String,
    id: String,
) -> Result<bool, String> {
    let root = testing_root(&state, &workspace_id)?;
    let path = root.join("configs.json");
    let mut items: Vec<TestConfig> = load_json(&path);
    let before = items.len();
    items.retain(|c| c.id != id);
    let deleted = items.len() != before;
    if deleted {
        save_json(&path, &items)?;
    }
    Ok(deleted)
}

/// 保存需求附图（需求表单贴图）：base64 解码写入 `.ox/screenshots/`，
/// 返回文件名。与执行截图同目录，读取复用 test_read_screenshot。
/// 扩展名白名单 + 只允许字母数字，防路径拼接注入。
#[tauri::command]
pub fn test_save_requirement_image(
    state: State<'_, AppState>,
    workspace_id: String,
    data_base64: String,
    ext: String,
) -> Result<String, String> {
    let ext = ext.to_ascii_lowercase();
    if !matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "webp" | "gif") {
        return Err(format!("不支持的图片格式: {ext}"));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_base64.trim())
        .map_err(|e| format!("图片 base64 解码失败: {e}"))?;
    if bytes.is_empty() {
        return Err("图片内容为空".into());
    }
    let root = testing_root(&state, &workspace_id)?;
    let dir = root.parent().ok_or_else(|| "路径解析失败".to_string())?.join("screenshots");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建截图目录失败: {e}"))?;
    let file_name = format!("req-{}.{ext}", now_ms());
    std::fs::write(dir.join(&file_name), &bytes)
        .map_err(|e| format!("写入图片失败: {e}"))?;
    Ok(file_name)
}

/// 读执行截图（Bug/报告面板展示用）：返回 base64 PNG。文件名不允许路径
/// 分隔符与 `..`（截图固定在 `.ox/screenshots/` 下，防越界读文件）。
#[tauri::command]
pub fn test_read_screenshot(
    state: State<'_, AppState>,
    workspace_id: String,
    file_name: String,
) -> Result<String, String> {
    if file_name.is_empty()
        || file_name.contains('/')
        || file_name.contains('\\')
        || file_name.contains("..")
    {
        return Err("非法的截图文件名".into());
    }
    let root = testing_root(&state, &workspace_id)?;
    let path = root
        .parent()
        .map(|p| p.join("screenshots").join(&file_name))
        .ok_or_else(|| "路径解析失败".to_string())?;
    let bytes = std::fs::read(&path).map_err(|e| format!("读取截图失败: {e}"))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

// ---------- 导入解析：Excel ----------

/// 表头列映射（识别到的列名 → 语义列）。关键词按优先级排列，命中即停。
const COL_TITLE: &[&str] = &["用例标题", "用例名称", "用例名", "用例", "标题", "用例点", "测试点", "用例描述"];
const COL_PRECONDITION: &[&str] = &["前置条件", "前置"];
const COL_STEPS: &[&str] = &["测试步骤", "操作步骤", "执行步骤", "步骤"];
const COL_EXPECTED: &[&str] = &["预期结果", "预期"];
const COL_PRIORITY: &[&str] = &["优先级"];
const COL_TAGS: &[&str] = &["所属模块", "模块", "标签", "用例类型"];

fn header_col(headers: &[String], keywords: &[&str]) -> Option<usize> {
    for kw in keywords {
        for (i, h) in headers.iter().enumerate() {
            let h = h.trim();
            if !h.is_empty() && h.contains(kw) {
                return Some(i);
            }
        }
    }
    None
}

/// 去掉步骤文本的前导编号（"1." "2、" "3)" "4："）——顺序信息已由数组
/// 顺序承载，编号留着会干扰流程图节点渲染。
fn strip_step_number(s: &str) -> String {
    let t = s.trim();
    let mut digits_end = 0;
    for (i, c) in t.char_indices() {
        if c.is_ascii_digit() {
            digits_end = i + c.len_utf8();
        } else {
            break;
        }
    }
    if digits_end == 0 {
        return t.to_string();
    }
    let rest = t[digits_end..].trim_start();
    match rest.chars().next() {
        Some(c) if matches!(c, '.' | '、' | ')' | '：' | ':') => {
            rest[c.len_utf8()..].trim().to_string()
        }
        _ => t.to_string(),
    }
}

/// 步骤单元格 → 步骤列表：按换行拆分 + 去前导编号 + 去空行。
fn steps_from_cell(text: &str) -> Vec<String> {
    text.split(['\n', '\r'])
        .map(strip_step_number)
        .filter(|s| !s.is_empty())
        .collect()
}

/// 解析 Excel（xlsx/xls/xlsm）为用例草稿：逐 sheet 识别表头行（含「用例/
/// 标题」关键词的行），表头映射列语义，数据行转用例。识别不到表头的 sheet
/// 跳过（不猜列序——猜错比拒绝更糟）。
pub fn parse_excel_cases(bytes: Vec<u8>) -> Result<Vec<TestCase>, String> {
    use calamine::Reader as _;
    let cursor = std::io::Cursor::new(bytes);
    let mut workbook =
        calamine::open_workbook_auto_from_rs(cursor).map_err(|e| format!("打开 Excel 失败: {e}"))?;
    let mut cases = Vec::new();
    for sheet_name in workbook.sheet_names() {
        let range = match workbook.worksheet_range(&sheet_name) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let mut rows = range.rows();
        // 找表头行：前 5 行内含标题关键词的行。
        let mut header: Option<(usize, Vec<String>)> = None;
        let mut scanned = 0usize;
        for row in rows.by_ref() {
            if scanned >= 5 {
                break;
            }
            scanned += 1;
            let cells: Vec<String> = row.iter().map(|c| c.to_string()).collect();
            if header_col(&cells, COL_TITLE).is_some() {
                header = Some((scanned, cells));
                break;
            }
        }
        let Some((_, headers)) = header else { continue };
        let col_title = header_col(&headers, COL_TITLE);
        let col_pre = header_col(&headers, COL_PRECONDITION);
        let col_steps = header_col(&headers, COL_STEPS);
        let col_expected = header_col(&headers, COL_EXPECTED);
        let col_priority = header_col(&headers, COL_PRIORITY);
        let col_tags = header_col(&headers, COL_TAGS);
        let Some(col_title) = col_title else { continue };
        let cell = |row: &[calamine::Data], col: Option<usize>| -> String {
            col.and_then(|i| row.get(i))
                .map(|c| c.to_string().trim().to_string())
                .unwrap_or_default()
        };
        for row in rows {
            let title = cell(row, Some(col_title));
            if title.is_empty() {
                continue;
            }
            let steps_raw = cell(row, col_steps);
            let mut tags = Vec::new();
            let tag_raw = cell(row, col_tags);
            if !tag_raw.is_empty() {
                tags.push(tag_raw);
            }
            cases.push(TestCase {
                title,
                precondition: cell(row, col_pre),
                steps: steps_from_cell(&steps_raw),
                expected: cell(row, col_expected),
                priority: cell(row, col_priority),
                tags,
                requirement_id: String::new(),
                source: "excel".into(),
                created_at: 0,
                id: String::new(),
            });
        }
    }
    if cases.is_empty() {
        return Err(
            "未识别到用例：需要表头行包含「用例标题」列（支持 前置条件/步骤/预期结果/优先级/所属模块 列）"
                .into(),
        );
    }
    Ok(cases)
}

/// Excel 导入命令（base64 传输，与附件同通道）。
#[tauri::command]
pub fn test_parse_excel(
    state: State<'_, AppState>,
    workspace_id: String,
    file_base64: String,
) -> Result<Vec<TestCase>, String> {
    let _ = testing_root(&state, &workspace_id)?; // 校验工作区存在
    parse_excel_cases(decode_b64(&file_base64)?)
}

// ---------- 导入解析：XMind ----------

/// XMind 主题节点（content.json / content.xml 统一投影）。
#[derive(Debug, Clone, Default)]
struct XNode {
    title: String,
    children: Vec<XNode>,
}

/// 去掉子主题的标签前缀（「前置条件：」「步骤1.」等）——标签语义由分支
/// 位置承载，文本里留着会污染字段内容。
fn strip_label(s: &str) -> String {
    let t = s.trim();
    for label in ["前置条件", "前置", "预期结果", "预期", "优先级", "步骤"] {
        if let Some(rest) = t.strip_prefix(label) {
            let rest = rest.trim_start_matches(['：', ':', ' ', '.', '、']);
            return rest.trim().to_string();
        }
    }
    t.to_string()
}

/// 节点文本：无子节点 = 标题去标签；有子节点 = 标题 + 各子标题逐行合并。
fn node_text(n: &XNode) -> String {
    let own = strip_label(&n.title);
    if n.children.is_empty() {
        return own;
    }
    let mut lines: Vec<String> = Vec::new();
    if !own.is_empty() {
        lines.push(own);
    }
    for c in &n.children {
        let t = strip_label(&c.title);
        if !t.is_empty() {
            lines.push(t);
        }
    }
    lines.join("\n")
}

/// 节点步骤列表：有子节点 = 各子标题；无子节点 = 标题按行拆。
fn node_lines(n: &XNode) -> Vec<String> {
    if n.children.is_empty() {
        strip_label(&n.title)
            .lines()
            .map(|s| strip_step_number(s))
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        n.children
            .iter()
            .map(|c| strip_step_number(&c.title))
            .filter(|s| !s.is_empty())
            .collect()
    }
}

/// 主题树 → 用例：根主题的直接子主题 = 一条用例；用例的子主题按标题关键词
/// 分类（前置/步骤/预期/优先级），无法分类的子主题：有孩子 → 孩子们当步骤，
/// 无孩子 → 自身当一步。启发式解析，覆盖常见导图结构。
fn cases_from_topic_roots(roots: &[XNode]) -> Vec<TestCase> {
    let mut cases = Vec::new();
    for root in roots {
        for case_topic in &root.children {
            let mut case = TestCase {
                title: case_topic.title.trim().to_string(),
                source: "xmind".into(),
                ..Default::default()
            };
            for child in &case_topic.children {
                let label = child.title.trim();
                if label.contains("前置") {
                    case.precondition = node_text(child);
                } else if label.contains("步骤") {
                    case.steps = node_lines(child);
                } else if label.contains("预期") {
                    case.expected = node_text(child);
                } else if label.contains("优先级") {
                    case.priority = strip_label(label);
                } else if child.children.is_empty() {
                    let step = strip_step_number(label);
                    if !step.is_empty() {
                        case.steps.push(step);
                    }
                } else {
                    for g in &child.children {
                        let step = strip_step_number(&g.title);
                        if !step.is_empty() {
                            case.steps.push(step);
                        }
                    }
                }
            }
            if !case.title.is_empty() {
                cases.push(case);
            }
        }
    }
    cases
}

/// 解析新版 XMind（Zen/2020+，content.json）：sheet 数组，rootTopic 树。
fn parse_xmind_json(text: &str) -> Option<Vec<TestCase>> {
    let sheets: Vec<serde_json::Value> = serde_json::from_str(text).ok()?;
    let mut roots = Vec::new();
    for sheet in sheets {
        let root = sheet.get("rootTopic")?;
        roots.push(json_to_node(root));
    }
    (!roots.is_empty()).then(|| cases_from_topic_roots(&roots))
}

fn json_to_node(v: &serde_json::Value) -> XNode {
    let title = v
        .get("title")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    let mut children = Vec::new();
    if let Some(attached) = v
        .get("children")
        .and_then(|c| c.get("attached"))
        .and_then(|a| a.as_array())
    {
        for child in attached {
            children.push(json_to_node(child));
        }
    }
    XNode { title, children }
}

/// 解析旧版 XMind（content.xml）：最小 topic/title 树提取（quick-xml 事件流，
/// 只关心 topic 嵌套与 title 文本，其余元素忽略）。
fn parse_xmind_xml(xml: &str) -> Option<Vec<TestCase>> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut stack: Vec<XNode> = Vec::new();
    let mut roots: Vec<XNode> = Vec::new();
    let mut in_title = false;
    let mut title_buf = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match e.name().as_ref() {
                "topic" => stack.push(XNode::default()),
                "title" => {
                    in_title = true;
                    title_buf.clear();
                }
                _ => {}
            },
            Ok(Event::Text(t)) => {
                // 0.42 起 reader 产出的文本已反转义，xml10_content 仅做 EOL 规范化
                if in_title {
                    title_buf.push_str(&t.xml10_content());
                }
            }
            Ok(Event::End(e)) => match e.name().as_ref() {
                "title" => {
                    in_title = false;
                    if let Some(node) = stack.last_mut() {
                        node.title = title_buf.trim().to_string();
                    }
                }
                "topic" => {
                    let node = stack.pop()?;
                    match stack.last_mut() {
                        Some(parent) => parent.children.push(node),
                        None => roots.push(node),
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(_) => return None,
            _ => {}
        }
    }
    (!roots.is_empty()).then(|| cases_from_topic_roots(&roots))
}

/// 解析 XMind（.xmind = zip 包）：优先 content.json（新版），回退
/// content.xml（旧版）。都识别不到报错。
pub fn parse_xmind_cases(bytes: Vec<u8>) -> Result<Vec<TestCase>, String> {
    let reader = std::io::Cursor::new(bytes);
    let mut archive =
        zip::ZipArchive::new(reader).map_err(|e| format!("打开 XMind 失败（非 zip 包）: {e}"))?;
    // 新版：content.json
    if let Ok(mut file) = archive.by_name("content.json") {
        let mut text = String::new();
        use std::io::Read as _;
        file.read_to_string(&mut text)
            .map_err(|e| format!("读取 content.json 失败: {e}"))?;
        if let Some(cases) = parse_xmind_json(&text) {
            if !cases.is_empty() {
                return Ok(cases);
            }
        }
    }
    // 旧版：content.xml
    if let Ok(mut file) = archive.by_name("content.xml") {
        let mut text = String::new();
        use std::io::Read as _;
        file.read_to_string(&mut text)
            .map_err(|e| format!("读取 content.xml 失败: {e}"))?;
        if let Some(cases) = parse_xmind_xml(&text) {
            if !cases.is_empty() {
                return Ok(cases);
            }
        }
    }
    Err("未识别到用例：XMind 里需要二级主题为用例（子主题含 前置/步骤/预期 结构或直接列步骤）".into())
}

/// XMind 导入命令。
#[tauri::command]
pub fn test_parse_xmind(
    state: State<'_, AppState>,
    workspace_id: String,
    file_base64: String,
) -> Result<Vec<TestCase>, String> {
    let _ = testing_root(&state, &workspace_id)?; // 校验工作区存在
    parse_xmind_cases(decode_b64(&file_base64)?)
}

// ---------- 报告生成 ----------

fn fmt_ts(ms: u64) -> String {
    if ms == 0 {
        return "—".into();
    }
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    // 与 commands.rs 相同的 civil-from-days 算法（无 chrono 依赖）。
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp <= 2 { y + 1 } else { y };
    let (h, mi, s) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    format!("{m:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

fn status_mark(status: &str) -> &'static str {
    match status {
        "pass" => "✅ 通过",
        "fail" => "❌ 失败",
        "blocked" => "⛔ 阻塞",
        "running" => "🔄 执行中",
        "skipped" => "⏭ 跳过",
        _ => "⏳ 未执行",
    }
}

/// 生成测试任务报告（markdown）。结果 JSON 解析失败的用例附原始回复片段，
/// 不静默丢信息。
pub fn build_task_report(data: &TestingData, task: &TestTask) -> String {
    let case_of = |id: &str| data.cases.iter().find(|c| c.id == id);
    let req_titles: Vec<String> = task
        .requirement_ids
        .iter()
        .filter_map(|id| data.requirements.iter().find(|r| &r.id == id))
        .map(|r| format!("「{}」", r.title))
        .collect();
    let (mut pass, mut fail, mut blocked, mut other) = (0u32, 0u32, 0u32, 0u32);
    for r in &task.results {
        match r.status.as_str() {
            "pass" => pass += 1,
            "fail" => fail += 1,
            "blocked" => blocked += 1,
            _ => other += 1,
        }
    }
    let total = task.results.len() as u32;
    let executed = pass + fail + blocked;
    let rate = if executed > 0 {
        format!("{:.0}%", pass as f64 * 100.0 / executed as f64)
    } else {
        "—".into()
    };

    let mut md = String::new();
    md.push_str(&format!("# 测试报告：{}\n\n", task.name));
    md.push_str(&format!("- 被测系统：{}\n", if task.base_url.is_empty() { "—" } else { &task.base_url }));
    md.push_str(&format!(
        "- 关联需求：{}\n",
        if req_titles.is_empty() { "—".to_string() } else { req_titles.join("、") }
    ));
    md.push_str(&format!("- 任务状态：{}\n", task.status));
    md.push_str(&format!(
        "- 执行时间：{} → {}\n\n",
        fmt_ts(task.created_at),
        fmt_ts(task.finished_at)
    ));
    md.push_str("## 结果汇总\n\n");
    md.push_str(&format!(
        "| 总用例 | 通过 | 失败 | 阻塞 | 未执行 | 通过率（已执行） |\n|---|---|---|---|---|---|\n| {total} | {pass} | {fail} | {blocked} | {other} | {rate} |\n\n"
    ));

    md.push_str("## 用例明细\n\n");
    for result in &task.results {
        let Some(case) = case_of(&result.case_id) else { continue };
        md.push_str(&format!("### {} {}\n\n", status_mark(&result.status), case.title));
        if !case.precondition.is_empty() {
            md.push_str(&format!("- 前置条件：{}\n", case.precondition));
        }
        if !case.expected.is_empty() {
            md.push_str(&format!("- 预期结果：{}\n", case.expected));
        }
        if !result.actual.is_empty() {
            md.push_str(&format!("- 实际结果：{}\n", result.actual));
        }
        if !result.steps.is_empty() {
            md.push_str("- 步骤执行：\n");
            for step in &result.steps {
                let mark = match step.status.as_str() {
                    "pass" => "✅",
                    "fail" => "❌",
                    _ => "⛔",
                };
                md.push_str(&format!("  - {} {}. {}\n", mark, step.no, step.note));
            }
        }
        if !result.screenshots.is_empty() {
            md.push_str(&format!("- 截图：{}\n", result.screenshots.join("、")));
        }
        if !result.bug_id.is_empty() {
            if let Some(bug) = data.bugs.iter().find(|b| b.id == result.bug_id) {
                md.push_str(&format!(
                    "- 关联 Bug：[{}] {}（{}）\n",
                    bug.severity, bug.title, bug.id
                ));
            }
        }
        if result.raw_reply.is_empty() && result.status == "blocked" && result.actual.is_empty() {
            md.push_str("- ⚠ 结果未解析（agent 未按协议输出）\n");
        }
        md.push('\n');
    }

    let task_bugs: Vec<&Bug> = data
        .bugs
        .iter()
        .filter(|b| b.task_id == task.id)
        .collect();
    if !task_bugs.is_empty() {
        md.push_str("## Bug 清单\n\n");
        for bug in &task_bugs {
            md.push_str(&format!(
                "### [{}] {}（{}）\n\n",
                if bug.severity.is_empty() { "P?" } else { &bug.severity },
                bug.title,
                if bug.status == "resolved" { "已解决" } else { "待处理" }
            ));
            if !bug.steps.is_empty() {
                md.push_str(&format!("**复现步骤**：\n\n{}\n\n", bug.steps));
            }
            md.push_str(&format!("**预期**：{}\n\n**实际**：{}\n\n", bug.expected, bug.actual));
            if !bug.screenshots.is_empty() {
                md.push_str(&format!("**截图**：{}\n\n", bug.screenshots.join("、")));
            }
        }
    }
    md
}

/// 生成单个 Bug 报告（markdown）。
pub fn build_bug_report(data: &TestingData, bug: &Bug) -> String {
    let mut md = String::new();
    md.push_str(&format!("# Bug 报告：{}\n\n", bug.title));
    md.push_str(&format!(
        "- 严重级别：{}\n- 状态：{}\n- 提出时间：{}\n",
        if bug.severity.is_empty() { "P?" } else { &bug.severity },
        if bug.status == "resolved" { "已解决" } else { "待处理" },
        fmt_ts(bug.created_at)
    ));
    if let Some(case) = data.cases.iter().find(|c| c.id == bug.case_id) {
        md.push_str(&format!("- 关联用例：{}\n", case.title));
    }
    if let Some(task) = data.tasks.iter().find(|t| t.id == bug.task_id) {
        md.push_str(&format!("- 所属任务：{}\n", task.name));
    }
    md.push_str("\n## 复现步骤\n\n");
    md.push_str(if bug.steps.is_empty() { "（未提供）\n" } else { &bug.steps });
    md.push_str("\n\n## 预期结果\n\n");
    md.push_str(if bug.expected.is_empty() { "（未提供）\n" } else { &bug.expected });
    md.push_str("\n\n## 实际结果\n\n");
    md.push_str(if bug.actual.is_empty() { "（未提供）\n" } else { &bug.actual });
    if !bug.screenshots.is_empty() {
        md.push_str("\n## 截图\n\n");
        for s in &bug.screenshots {
            md.push_str(&format!("- `{s}`（.ox/screenshots/）\n"));
        }
    }
    md
}

/// 生成任务报告命令。
#[tauri::command]
pub fn test_build_report(
    state: State<'_, AppState>,
    workspace_id: String,
    task_id: String,
) -> Result<String, String> {
    let root = testing_root(&state, &workspace_id)?;
    let data: TestingData = TestingData {
        requirements: load_json(&root.join("requirements.json")),
        cases: load_json(&root.join("cases.json")),
        tasks: load_json(&root.join("tasks.json")),
        bugs: load_json(&root.join("bugs.json")),
        configs: load_json(&root.join("configs.json")),
    };
    let task = data
        .tasks
        .iter()
        .find(|t| t.id == task_id)
        .ok_or_else(|| format!("任务 `{task_id}` 不存在"))?;
    Ok(build_task_report(&data, task))
}

/// 生成 Bug 报告命令。
#[tauri::command]
pub fn test_build_bug_report(
    state: State<'_, AppState>,
    workspace_id: String,
    bug_id: String,
) -> Result<String, String> {
    let root = testing_root(&state, &workspace_id)?;
    let data: TestingData = TestingData {
        requirements: load_json(&root.join("requirements.json")),
        cases: load_json(&root.join("cases.json")),
        tasks: load_json(&root.join("tasks.json")),
        bugs: load_json(&root.join("bugs.json")),
        configs: load_json(&root.join("configs.json")),
    };
    let bug = data
        .bugs
        .iter()
        .find(|b| b.id == bug_id)
        .ok_or_else(|| format!("Bug `{bug_id}` 不存在"))?;
    Ok(build_bug_report(&data, bug))
}

// ---------- 测试 ----------

#[cfg(test)]
mod tests {
    use super::*;

    /// 内存里构造一个最小 xlsx（zip + OOXML 骨架 + inlineStr 单元格）。
    fn make_xlsx(sheet_rows: &[&str]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default();
            let mut add = |name: &str, content: &str| {
                w.start_file(name, opts).unwrap();
                use std::io::Write as _;
                w.write_all(content.as_bytes()).unwrap();
            };
            add(
                "[Content_Types].xml",
                r#"<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
</Types>"#,
            );
            add(
                "_rels/.rels",
                r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#,
            );
            add(
                "xl/workbook.xml",
                r#"<?xml version="1.0" encoding="UTF-8"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<sheets><sheet name="用例" sheetId="1" r:id="rId1"/></sheets>
</workbook>"#,
            );
            add(
                "xl/_rels/workbook.xml.rels",
                r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>"#,
            );
            let mut sheet = String::from(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>"#,
            );
            for (i, row) in sheet_rows.iter().enumerate() {
                sheet.push_str(&format!(r#"<row r="{}">"#, i + 1));
                for (ci, cell) in row.split('\u{1}').enumerate() {
                    let col = (b'A' + ci as u8) as char;
                    sheet.push_str(&format!(
                        r#"<c r="{col}{}" t="inlineStr"><is><t>{}</t></is></c>"#,
                        i + 1,
                        cell
                    ));
                }
                sheet.push_str("</row>");
            }
            sheet.push_str("</sheetData></worksheet>");
            add("xl/worksheets/sheet1.xml", &sheet);
            w.finish().unwrap();
        }
        buf.into_inner()
    }

    /// 内存里构造一个最小 xmind（zip + content.json / content.xml）。
    fn make_xmind(content_json: Option<&str>, content_xml: Option<&str>) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default();
            let mut add = |name: &str, content: &str| {
                w.start_file(name, opts).unwrap();
                use std::io::Write as _;
                w.write_all(content.as_bytes()).unwrap();
            };
            if let Some(json) = content_json {
                add("content.json", json);
            }
            if let Some(xml) = content_xml {
                add("content.xml", xml);
            }
            w.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn excel_cases_parse_with_header_mapping() {
        let xlsx = make_xlsx(&[
            "用例标题\u{1}前置条件\u{1}步骤\u{1}预期结果\u{1}优先级\u{1}所属模块",
            "登录成功\u{1}已注册账号\u{1}1. 打开登录页\n2、输入正确密码\u{1}跳转首页\u{1}P1\u{1}登录模块",
            "密码错误提示\u{1}\u{1}输入错误密码\u{1}提示密码错误\u{1}P2\u{1}登录模块",
        ]);
        let cases = parse_excel_cases(xlsx).unwrap();
        assert_eq!(cases.len(), 2);
        assert_eq!(cases[0].title, "登录成功");
        assert_eq!(cases[0].precondition, "已注册账号");
        assert_eq!(cases[0].steps, vec!["打开登录页", "输入正确密码"]);
        assert_eq!(cases[0].expected, "跳转首页");
        assert_eq!(cases[0].priority, "P1");
        assert_eq!(cases[0].tags, vec!["登录模块"]);
        assert_eq!(cases[0].source, "excel");
        assert_eq!(cases[1].steps, vec!["输入错误密码"]);
    }

    #[test]
    fn excel_without_header_row_is_rejected() {
        let xlsx = make_xlsx(&["随便一行", "另一行"]);
        assert!(parse_excel_cases(xlsx).is_err());
    }

    #[test]
    fn xmind_json_cases_parse() {
        let json = r#"[
          {"id":"s1","title":"登录","rootTopic":{"id":"r","title":"登录模块用例","children":{"attached":[
            {"id":"c1","title":"登录成功","children":{"attached":[
              {"id":"p","title":"前置条件：已注册账号"},
              {"id":"st","title":"步骤","children":{"attached":[
                {"id":"s1","title":"1. 打开登录页"},
                {"id":"s2","title":"2、输入正确密码"}
              ]}},
              {"id":"e","title":"预期结果","children":{"attached":[{"id":"et","title":"跳转首页"}]}},
              {"id":"pr","title":"优先级：P1"}
            ]}},
            {"id":"c2","title":"直接列步骤的用例","children":{"attached":[
              {"id":"x1","title":"打开页面"},
              {"id":"x2","title":"点击按钮"}
            ]}}
          ]}}}
        ]"#;
        let xmind = make_xmind(Some(json), None);
        let cases = parse_xmind_cases(xmind).unwrap();
        assert_eq!(cases.len(), 2);
        assert_eq!(cases[0].title, "登录成功");
        assert_eq!(cases[0].precondition, "已注册账号");
        assert_eq!(cases[0].steps, vec!["打开登录页", "输入正确密码"]);
        assert_eq!(cases[0].expected, "跳转首页");
        assert_eq!(cases[0].priority, "P1");
        assert_eq!(cases[0].source, "xmind");
        // 无法分类的子主题：无孩子 → 自身当一步
        assert_eq!(cases[1].steps, vec!["打开页面", "点击按钮"]);
    }

    #[test]
    fn xmind_legacy_xml_cases_parse() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<xmap-content xmlns="urn:xmind:xmap:xmlns:content:2.0" version="2.0">
  <sheet id="s1">
    <topic id="r"><title>登录模块用例</title><children>
      <topics type="attached">
        <topic id="c1"><title>登录成功</title><children>
          <topics type="attached">
            <topic id="p"><title>前置条件：已注册账号</title></topic>
            <topic id="st"><title>步骤</title><children>
              <topics type="attached">
                <topic id="s1"><title>1. 打开登录页</title></topic>
                <topic id="s2"><title>2、输入正确密码</title></topic>
              </topics>
            </children></topic>
            <topic id="e"><title>预期结果：跳转首页</title></topic>
          </topics>
        </children></topic>
      </topics>
    </children></topic>
  </sheet>
</xmap-content>"#;
        let xmind = make_xmind(None, Some(xml));
        let cases = parse_xmind_cases(xmind).unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].title, "登录成功");
        assert_eq!(cases[0].precondition, "已注册账号");
        assert_eq!(cases[0].steps, vec!["打开登录页", "输入正确密码"]);
        assert_eq!(cases[0].expected, "跳转首页");
    }

    #[test]
    fn strip_helpers_clean_labels_and_numbers() {
        assert_eq!(strip_step_number("1. 打开登录页"), "打开登录页");
        assert_eq!(strip_step_number("2、输入密码"), "输入密码");
        assert_eq!(strip_step_number("3) 点击按钮"), "点击按钮");
        assert_eq!(strip_step_number("无编号步骤"), "无编号步骤");
        assert_eq!(strip_label("前置条件：已注册账号"), "已注册账号");
        assert_eq!(strip_label("优先级：P1"), "P1");
        assert_eq!(strip_label("普通文本"), "普通文本");
    }

    #[test]
    fn task_report_contains_summary_and_bugs() {
        let data = TestingData {
            requirements: vec![Requirement {
                id: "r1".into(),
                title: "登录功能".into(),
                content: String::new(),
                data_profile: String::new(),
                images: Vec::new(),
                clarifications: Vec::new(),
                origin: String::new(),
                created_at: 0,
            }],
            configs: vec![],
            cases: vec![TestCase {
                id: "c1".into(),
                title: "登录成功".into(),
                precondition: String::new(),
                steps: vec!["打开登录页".into()],
                expected: "跳转首页".into(),
                priority: "P1".into(),
                tags: vec![],
                requirement_id: "r1".into(),
                source: "manual".into(),
                created_at: 0,
            }],
            tasks: vec![TestTask {
                id: "t1".into(),
                name: "登录回归".into(),
                base_url: "http://demo".into(),
                config_id: String::new(),
                requirement_ids: vec!["r1".into()],
                case_ids: vec!["c1".into()],
                status: "done".into(),
                started_at: 0,
                pause_note: String::new(),
                results: vec![CaseResult {
                    case_id: "c1".into(),
                    status: "fail".into(),
                    actual: "停在登录页".into(),
                    steps: vec![StepResult {
                        no: 1,
                        status: "fail".into(),
                        note: "按钮无响应".into(),
                    }],
                    screenshots: vec!["shot-1.png".into()],
                    bug_id: "b1".into(),
                    raw_reply: String::new(),
                    started_at: 0,
                    finished_at: 0,
                }],
                created_at: 0,
                finished_at: 0,
                archived: false,
            }],
            bugs: vec![Bug {
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
            }],
        };
        let md = build_task_report(&data, &data.tasks[0]);
        assert!(md.contains("# 测试报告：登录回归"));
        assert!(md.contains("「登录功能」"));
        assert!(md.contains("❌ 失败 登录成功"));
        assert!(md.contains("登录按钮无响应"));
        assert!(md.contains("shot-1.png"));
        let bug_md = build_bug_report(&data, &data.bugs[0]);
        assert!(bug_md.contains("# Bug 报告：登录按钮无响应"));
        assert!(bug_md.contains("登录回归"));
    }

    #[test]
    fn json_store_roundtrip_and_atomic_write() {
        let dir = std::env::temp_dir().join(format!("ox-testing-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("cases.json");
        let cases = vec![TestCase {
            id: "c1".into(),
            title: "用例一".into(),
            ..Default::default()
        }];
        save_json(&path, &cases).unwrap();
        // 原子写不留 tmp 残留
        let residues: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(residues.is_empty(), "tmp residue: {residues:?}");
        let back: Vec<TestCase> = load_json(&path);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].title, "用例一");
        // 损坏文件 → 空表兜底（不 panic）
        std::fs::write(&path, b"not json").unwrap();
        let back: Vec<TestCase> = load_json(&path);
        assert!(back.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_roundtrip_and_old_task_without_config_id() {
        // 配置存取回环（含账号列表）。
        let dir = std::env::temp_dir().join(format!(
            "ox-testing-cfg-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("configs.json");
        let configs = vec![TestConfig {
            id: "cfg1".into(),
            name: "测试环境-管理员".into(),
            base_url: "http://localhost:3000".into(),
            accounts: vec![TestAccount {
                username: "admin".into(),
                password: "secret".into(),
                note: "管理员".into(),
            }],
            notes: "测试数据随便造".into(),
            created_at: 1,
        }];
        save_json(&path, &configs).unwrap();
        let back: Vec<TestConfig> = load_json(&path);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].accounts.len(), 1);
        assert_eq!(back[0].accounts[0].username, "admin");
        let _ = std::fs::remove_dir_all(&dir);

        // 旧任务 JSON 没有 config_id 字段 → serde default 兜底（向后兼容）。
        let legacy = r#"{"id":"t1","name":"旧任务","base_url":"http://x","requirement_ids":[],"case_ids":[],"status":"pending","results":[],"created_at":0,"finished_at":0}"#;
        let task: TestTask = serde_json::from_str(legacy).unwrap();
        assert!(task.config_id.is_empty());
    }
}
