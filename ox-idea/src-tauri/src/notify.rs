//! 系统通知栏：agent 任务进入需要用户关注的状态时，按平台原生通知提醒
//! （Windows Toast / macOS 通知中心 / Linux DBus，经 tauri-plugin-notification
//! 统一封装）。无论窗口是否聚焦都发（工具确认、任务完成必须让用户知道）；
//! 同一任务的同一状态只发一次（每个窗口都订阅 agent 事件流，去重防多窗口重复弹）。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use ide_agent::{AgentEvent, AgentStatus, AgentTask};
use tauri::{AppHandle, Emitter};
use tauri_plugin_notification::NotificationExt;

/// 同键重复抑制窗口：多窗口转发同一事件几乎同时到达（间隔远小于 1s），
/// 而任务新一轮进入同一状态（如再次 Ready）至少隔几秒——放行。
const DEDUP_WINDOW: Duration = Duration::from_secs(1);

/// 把应用的 AppUserModelId 注册进当前用户的注册表。Windows 的 Toast 按 AUMID
/// 归属应用：打包安装版由快捷方式注册，而 dev 直接跑的 exe 没有快捷方式——
/// 不注册的话 `Show()` 不报错但通知被系统静默丢弃（正是「做了通知却看不到」的原因）。
/// 幂等：每次启动重写，值不变时无副作用。
#[cfg(windows)]
pub fn register_app_id(identifier: &str, name: &str) {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let Ok(root) = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(r"SOFTWARE\Classes\AppUserModelId", winreg::enums::KEY_WRITE)
        .or_else(|_| RegKey::predef(HKEY_CURRENT_USER).create_subkey(r"SOFTWARE\Classes\AppUserModelId").map(|(k, _)| k))
    else {
        eprintln!("[notify] could not open AppUserModelId registry key; toasts may be dropped");
        return;
    };
    match root.create_subkey(identifier) {
        Ok((key, _)) => {
            let _ = key.set_value("DisplayName", &name.to_string());
            // 通知上显示应用图标；失败无碍（图标缺失只影响外观）。
            if let Ok(exe) = std::env::current_exe() {
                let _ = key.set_value("IconUri", &exe.to_string_lossy().to_string());
            }
        }
        Err(err) => eprintln!("[notify] could not register app id {identifier}: {err}"),
    }
}

#[cfg(not(windows))]
pub fn register_app_id(_identifier: &str, _name: &str) {}

fn notified() -> &'static Mutex<HashMap<(String, &'static str), Instant>> {
    static NOTIFIED: OnceLock<Mutex<HashMap<(String, &'static str), Instant>>> = OnceLock::new();
    NOTIFIED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 悬浮球状态广播：agent 任务的每次状态变化全局 emit（`app.emit` 所有窗口
/// 都收到，floating-ball 据此驱动天线灯与提醒徽标）。与系统通知（`agent_event`）
/// 同源但职责不同：通知只挑需要关注的状态且去重；这里全量广播——运行/空闲
/// 驱动状态灯，完成/审批/失败/限额驱动徽标，前端按 task_id 幂等聚合。
pub fn agent_attention(app: &AppHandle, window: &str, event: &AgentEvent) {
    let AgentEvent::Task { task } = event else { return };
    let status = match task.status {
        AgentStatus::Starting | AgentStatus::Running => "running",
        AgentStatus::Ready => "ready",
        AgentStatus::NeedsInput => "needs_input",
        AgentStatus::Failed => "failed",
        AgentStatus::Limited => "limited",
        AgentStatus::Archived => return,
    };
    let _ = app.emit(
        "oxidea://agent-attention",
        serde_json::json!({
            "task_id": task.id,
            "status": status,
            "title": task.title,
            "window": window,
        }),
    );
}

/// Agent 事件 → 系统通知（需要用户关注的状态；同一任务的同一状态只发一次）。
pub fn agent_event(app: &AppHandle, event: &AgentEvent) {
    let AgentEvent::Task { task } = event else { return };
    let Some((key, title, body)) = agent_notification(task) else { return };
    // 多窗口订阅去重：同键在抑制窗口内的重复事件丢弃。
    {
        let mut seen = notified().lock().unwrap();
        let now = Instant::now();
        if seen.get(&key).map(|t| now.duration_since(*t) < DEDUP_WINDOW).unwrap_or(false) {
            return;
        }
        seen.insert(key, now);
    }
    // 无论窗口是否聚焦都发：确认与完成必须让用户知道（聚焦时也弹，用户明确要求）。
    if let Err(err) = app.notification().builder().title(title).body(&body).show() {
        eprintln!("[notify] failed to show notification: {err}");
    }
}

/// 需要用户关注的状态 → (去重键, 标题, 正文)；其余状态返回 None（不打扰）。
fn agent_notification(task: &AgentTask) -> Option<((String, &'static str), &'static str, String)> {
    let title = task.title.trim();
    let title = if title.is_empty() { "Agent task" } else { title };
    let (key, title, body) = match task.status {
        // 一轮结束：结果可查看、会话可继续。
        AgentStatus::Ready => (
            "ready",
            "Agent finished",
            format!("{title} — the result is ready to review."),
        ),
        // 等权限答复/提问：不回应 agent 就一直卡着。
        AgentStatus::NeedsInput => (
            "needs_input",
            "Agent needs your input",
            format!("{title} is waiting for your answer."),
        ),
        AgentStatus::Failed => {
            let why = task.error.as_deref().unwrap_or("unknown error");
            ("failed", "Agent failed", format!("{title} — {why}"))
        }
        // 账号用量到顶：重置后才能继续。
        AgentStatus::Limited => (
            "limited",
            "Agent hit its usage limit",
            format!("{title} pauses until the limit resets."),
        ),
        AgentStatus::Starting | AgentStatus::Running | AgentStatus::Archived => return None,
    };
    Some(((task.id.clone(), key), title, body))
}
