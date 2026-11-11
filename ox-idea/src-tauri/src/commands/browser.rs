//! 内置浏览器直播面板：screencast 帧流 + 输入回传。
//!
//! 与 agent 的浏览器工具共用进程级同一 `BrowserSession`（`BrowserSession::shared`）——
//! agent 导航/点击的页面就是用户面板里看到的页面。帧流走 `Channel` 推送，
//! 输入走普通命令；attach 幂等（重复打开面板不重启浏览器）。

use agentscope_browser::BrowserSession;
use ide_core::error::ErrorKind;
use ide_core::{IdeError, IdeResult};
use serde::Serialize;
use specta::Type;
use tauri::ipc::Channel;

/// 面板帧（`ScreencastFrame` 的 IPC 形态）：base64 JPEG + 视口尺寸。
#[derive(Clone, Debug, Serialize, Type)]
pub struct BrowserFrame {
    pub data: String,
    pub device_width: f64,
    pub device_height: f64,
}

/// attach 返回：当前页面 URL（地址栏初值）。
#[derive(Clone, Debug, Serialize, Type)]
pub struct BrowserPanelState {
    pub url: String,
}

fn err(e: String) -> IdeError {
    IdeError::new(ErrorKind::Unavailable, e)
}

/// 打开面板：拉起浏览器（如未运行）+ 开启 screencast，帧经 `on_frame` 推送。
/// 返回当前 URL。detach（或面板关闭）后转发任务自动退出。
#[tauri::command]
#[specta::specta]
pub async fn browser_panel_attach(on_frame: Channel<BrowserFrame>) -> IdeResult<BrowserPanelState> {
    let session = BrowserSession::shared();
    session.ensure_started().await.map_err(err)?;
    let mut rx = session.start_screencast().await.map_err(err)?;
    let url = session.current_url().await.map_err(err)?;
    tokio::spawn(async move {
        loop {
            // 超时轮询 screencast 开关：detach 后转发任务在 1s 内退出。
            match tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await {
                Ok(Ok(frame)) => {
                    let _ = on_frame.send(BrowserFrame {
                        data: frame.data,
                        device_width: frame.device_width,
                        device_height: frame.device_height,
                    });
                }
                Ok(Err(_lagged)) => continue, // 消费慢丢帧，正常
                Err(_) => {
                    if BrowserSession::shared().screencast_active() {
                        continue;
                    }
                    // 断流自愈：screencast 已失效（浏览器被关闭后重建、监听
                    // 任务退出时清 active）。面板还开着就重新拉起浏览器并
                    // 重订阅帧流——agent 与面板共用同一实例，画面自动接上；
                    // 浏览器起不来才退出转发，下次 attach 再走完整流程。
                    match BrowserSession::shared().start_screencast().await {
                        Ok(new_rx) => {
                            rx = new_rx;
                        }
                        Err(_) => break,
                    }
                }
            }
        }
    });
    Ok(BrowserPanelState { url })
}

/// 关闭面板：停 screencast（转发任务随之退出）。浏览器进程保留——agent
/// 工具还在用，下次 attach 直接复用。
#[tauri::command]
#[specta::specta]
pub async fn browser_panel_detach() -> IdeResult<()> {
    BrowserSession::shared().stop_screencast().await;
    Ok(())
}

/// 地址栏导航，返回最终 URL（重定向后）。
#[tauri::command]
#[specta::specta]
pub async fn browser_panel_navigate(url: String) -> IdeResult<String> {
    BrowserSession::shared().navigate(&url).await.map_err(err)
}

/// 后退/前进/刷新（action ∈ back/forward/reload）。
#[tauri::command]
#[specta::specta]
pub async fn browser_panel_history(action: String) -> IdeResult<()> {
    BrowserSession::shared().history_go(&action).await.map_err(err)
}

/// 当前页面 URL（页面内部跳转后前端刷新地址栏用）。
#[tauri::command]
#[specta::specta]
pub async fn browser_panel_url() -> IdeResult<String> {
    BrowserSession::shared().current_url().await.map_err(err)
}

/// 鼠标事件：event_type ∈ pressed/released/moved/wheel；坐标为页面 CSS 像素
/// （前端按帧的 device_width 与显示宽换算）。
#[tauri::command]
#[specta::specta]
pub async fn browser_panel_mouse(
    event_type: String,
    x: f64,
    y: f64,
    button: Option<String>,
    click_count: i64,
    delta_x: f64,
    delta_y: f64,
) -> IdeResult<()> {
    BrowserSession::shared()
        .dispatch_mouse(&event_type, x, y, button.as_deref(), click_count, delta_x, delta_y)
        .await
        .map_err(err)
}

/// 键盘事件：event_type ∈ keydown/keyup/char；modifiers 为 CDP 位掩码
/// （Alt=1, Ctrl=2, Meta=4, Shift=8）；text 仅 char 事件；windows_vk 为
/// Windows 虚拟键码（特殊键传，普通字符传 0）。
#[tauri::command]
#[specta::specta]
pub async fn browser_panel_key(
    event_type: String,
    key: String,
    code: String,
    text: Option<String>,
    modifiers: i64,
    windows_vk: i64,
) -> IdeResult<()> {
    BrowserSession::shared()
        .dispatch_key(&event_type, &key, &code, text.as_deref(), modifiers, windows_vk)
        .await
        .map_err(err)
}

/// 直接插入文本（输入法/粘贴）。
#[tauri::command]
#[specta::specta]
pub async fn browser_panel_insert_text(text: String) -> IdeResult<()> {
    BrowserSession::shared().insert_text(&text).await.map_err(err)
}
