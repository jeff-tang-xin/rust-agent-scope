//! 桌面悬浮球（OXidea 版）：应用启动时在屏幕右上角显示一个终端助手小机器人
//! （透明、无边框、置顶、不进任务栏、不抢焦点）。交互全部在前端
//! （`features/floating-ball`）：单击打开/聚焦主项目窗口、按住拖拽（位移阈值
//! 区分单击）、右键菜单（菜单开合由前端 setSize/setPosition 扩缩窗口，机器人
//! 锚定右上角不动）——因此本模块只负责创建窗口，不注册任何命令。
//!
//! 开关：`Settings.appearance.floating_ball`（serde default `true`，旧配置兼容）。
//! 停用写设置后重启不再出现；「本次隐藏」只 hide 窗口，下次启动仍显示。

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

const BALL_LABEL: &str = "floating-ball";
const SMALL_W: f64 = 92.0;
const SMALL_H: f64 = 92.0;

/// 创建悬浮球窗口（启动时 `appearance.floating_ball` 为 `true` 才调用）。
/// 位置：主显示器右上角，不抢焦点。
pub fn spawn_floating_ball(app: &AppHandle) {
    if app.get_webview_window(BALL_LABEL).is_some() {
        return;
    }
    let (x, y) = match app.primary_monitor() {
        Ok(Some(m)) => (m.size().width as f64 - SMALL_W - 24.0, 48.0),
        _ => (800.0, 48.0),
    };
    let builder = WebviewWindowBuilder::new(app, BALL_LABEL, WebviewUrl::App("index.html".into()))
        .title("OXidea Assistant")
        .inner_size(SMALL_W, SMALL_H)
        .position(x, y)
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
    if let Err(error) = builder.build() {
        tracing::error!("cannot create the floating ball window: {error}");
        return;
    }
    // Windows 上 builder 的 transparent 只作用于窗口层；WebView2 自身的背景色
    // 默认不透明白，必须显式设为全透明，否则 DOM 再透明也盖着一层白底。
    if let Some(ball) = app.get_webview_window(BALL_LABEL) {
        if let Err(error) = ball.set_background_color(Some(tauri::window::Color(0, 0, 0, 0))) {
            tracing::error!("cannot make the floating ball background transparent: {error}");
        }
    }
}
