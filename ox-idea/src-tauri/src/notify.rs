//! 系统通知栏：agent 任务进入需要用户关注的状态时，按平台原生通知提醒
//! （Windows Toast / macOS 通知中心 / Linux DBus，经 tauri-plugin-notification
//! 统一封装）。仅在所有窗口都未聚焦时打扰；同一任务的同一状态只发一次
//! （每个窗口都订阅 agent 事件流，去重防多窗口重复弹）。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use ide_agent::{AgentEvent, AgentStatus, AgentTask};
use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;

/// 同键重复抑制窗口：多窗口转发同一事件几乎同时到达（间隔远小于 1s），
/// 而任务新一轮进入同一状态（如再次 Ready）至少隔几秒——放行。
const DEDUP_WINDOW: Duration = Duration::from_secs(1);

fn notified() -> &'static Mutex<HashMap<(String, &'static str), Instant>> {
    static NOTIFIED: OnceLock<Mutex<HashMap<(String, &'static str), Instant>>> = OnceLock::new();
    NOTIFIED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Agent 事件 → 系统通知（需要用户关注的状态 + 全部窗口未聚焦时才发）。
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
    // 用户正盯着任何一个 OXidea 窗口时不打扰。
    let focused = app
        .webview_windows()
        .values()
        .any(|w| w.is_focused().unwrap_or(false));
    if focused {
        return;
    }
    let _ = app.notification().builder().title(title).body(&body).show();
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
