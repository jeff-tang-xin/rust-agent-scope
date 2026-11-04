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
//! 分工：用例生成与执行由**前端编排**（sendMessage(mode="test") + 解析
//! agent 的结构化 JSON 回复，见 TestWorkbench），本模块只负责数据 CRUD、
//! Excel/XMind 导入解析与报告生成——后端不做编排，保持命令面无状态。

use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::commands::AppState;
use crate::factory;

// ---------- 数据结构 ----------

/// 需求（测试的对象；用例生成与任务关联的锚点）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Requirement {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub content: String,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// open | resolved
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub created_at: u64,
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

/// 测试数据根：`<workspace_root>/.ox/testing`。
fn testing_root(state: &AppState, workspace_id: &str) -> Result<PathBuf, String> {
    let config = state.snapshot();
    let ws = config
        .workspace(workspace_id)
        .ok_or_else(|| format!("unknown workspace `{workspace_id}`"))?;
    let root = factory::resolve_workspace_root(&state.data_dir, ws);
    Ok(root.join(".ox").join("testing"))
}

/// 读 JSON 文件；缺失/损坏按默认值（空表）处理——数据文件损坏不阻断面板打开。
fn load_json<T: Default + serde::de::DeserializeOwned>(path: &Path) -> T {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// 原子写：先写 `.tmp` 再 rename（进程崩溃不留半个文件）。
fn save_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create dir: {e}"))?;
    }
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| format!("serialize: {e}"))?;
    std::fs::write(&tmp, bytes).map_err(|e| format!("write: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename: {e}"))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn new_id() -> String {
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
