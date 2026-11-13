//! 桌面悬浮球：应用启动时在屏幕右上角显示一个可拖拽的小机器人（透明、
//! 无边框、置顶、不进任务栏、不抢焦点）。左键按住拖拽移动，右键弹菜单
//! （打开主窗 / 切换外观 / 本次隐藏 / 停用）。停用状态持久化到
//! StudioConfig.floating_ball_disabled，设置页「通用 → 界面」可重新打开。
//!
//! 窗口尺寸策略：平时 92×92 只容机器人（透明窗口的透明像素在 Windows 上
//! 不穿透点击，窗口大了会挡住桌面）；右键菜单弹出时临时扩到 240×320 并
//! 左移窗口保持机器人原位（菜单渲染在机器人下方），菜单关闭缩回——扩缩
//! 都走 `ball_menu_resize`。菜单另在窗口失焦时由前端关闭（点窗口外空白）。

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

const BALL_LABEL: &str = "floating-ball";
const SMALL_W: i32 = 92;
const SMALL_H: i32 = 92;
const BIG_W: i32 = 240;
const BIG_H: i32 = 320;

/// 创建悬浮球窗口（启动时 `floating_ball_disabled=false` 才调用）。
/// 位置：主显示器右上角（对齐参考产品），不抢焦点。
pub fn spawn_floating_ball(app: &AppHandle) {
    if app.get_webview_window(BALL_LABEL).is_some() {
        return;
    }
    let (x, y) = match app.primary_monitor() {
        Ok(Some(m)) => {
            let w = m.size().width as i32;
            (w - SMALL_W - 24, 48)
        }
        _ => (800, 48),
    };
    let builder = WebviewWindowBuilder::new(
        app,
        BALL_LABEL,
        WebviewUrl::App("index.html?view=ball".into()),
    )
    .title("Ox Floating Ball")
    .inner_size(SMALL_W as f64, SMALL_H as f64)
    .position(x as f64, y as f64)
    .decorations(false)
    .resizable(false)
    .maximizable(false)
    .minimizable(false)
    .skip_taskbar(true)
    .always_on_top(true)
    .focused(false)
    .shadow(false)
    .transparent(true);
    #[cfg(target_os = "macos")]
    let builder = builder.macos_private_api(true);
    if let Err(e) = builder.build() {
        crate::commands::app_log("ERROR", &format!("悬浮球窗口创建失败: {e}"));
    }
}

/// 菜单开合时扩缩窗口并保持机器人（窗口左上角）屏幕位置不变：
/// 扩大 = 向左下扩展（菜单渲染在机器人左下，对齐参考产品布局）。
/// 全程逻辑坐标（outer_position/inner_size 是物理像素，先除 scale_factor）。
#[tauri::command]
pub fn ball_menu_resize(window: tauri::Window, expanded: bool) -> Result<(), String> {
    let (w, h) = if expanded { (BIG_W, BIG_H) } else { (SMALL_W, SMALL_H) };
    let scale = window.scale_factor().unwrap_or(1.0);
    let pos = window
        .outer_position()
        .map_err(|e| e.to_string())?;
    // 当前逻辑宽与目标逻辑宽之差 = 左边界的平移量（机器人锚定右上角不动）。
    let cur_w = window
        .inner_size()
        .map(|s| s.width as f64 / scale)
        .unwrap_or(SMALL_W as f64);
    let dx = cur_w - w as f64;
    window
        .set_size(tauri::LogicalSize::new(w as f64, h as f64))
        .map_err(|e| e.to_string())?;
    window
        .set_position(tauri::LogicalPosition::new(
            pos.x as f64 / scale + dx,
            pos.y as f64 / scale,
        ))
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn ball_show_main(app: AppHandle) -> Result<(), String> {
    let win = app
        .get_webview_window("main")
        .ok_or_else(|| "main window not found".to_string())?;
    let _ = win.unminimize();
    win.show().map_err(|e| e.to_string())?;
    win.set_focus().map_err(|e| e.to_string())
}

/// 本次隐藏（不写配置——下次启动仍显示）。
#[tauri::command]
pub fn ball_hide(window: tauri::Window) -> Result<(), String> {
    window.hide().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn ball_get_disabled(state: tauri::State<'_, crate::commands::AppState>) -> bool {
    state.snapshot().floating_ball_disabled
}

/// 停用/恢复：写配置（跨启动持久）+ 立即隐藏或重建窗口。
#[tauri::command]
pub fn ball_set_disabled(
    app: AppHandle,
    state: tauri::State<'_, crate::commands::AppState>,
    disabled: bool,
) -> Result<(), String> {
    let mut config = state.snapshot();
    config.floating_ball_disabled = disabled;
    state.persist(&config)?;
    if disabled {
        if let Some(win) = app.get_webview_window(BALL_LABEL) {
            let _ = win.hide();
        }
    } else {
        if let Some(win) = app.get_webview_window(BALL_LABEL) {
            let _ = win.show();
        } else {
            spawn_floating_ball(&app);
        }
    }
    Ok(())
}
