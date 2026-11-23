//! 浏览器自动化工具集（框架级，studio 与 ox-idea 共用）：给 Agent 一个真实
//! Chromium 浏览器（headless，真实渲染不失真），支持导航、读页面结构、点击、
//! 输入、截图、执行 JS、关闭。
//!
//! 底层：chromiumoxide（CDP 协议）驱动本机 Edge/Chrome/Chromium——三者同为
//! Chromium 内核，CDP 协议通用，不挑品牌。浏览器进程懒启动（首次工具调用
//! 才拉起），会话级复用同一标签页；用户手动关掉浏览器后下次调用自动重启。
//!
//! 浏览器可执行文件发现（跨平台）：环境变量 `AGENTSCOPE_BROWSER_PATH`（或
//! 兼容旧名 `STUDIO_BROWSER_PATH`）显式指定优先；否则用 chromiumoxide 自带
//! 发现（CHROME 环境变量 → PATH which → Windows 注册表 → 各平台常见安装
//! 路径），Edge/Chrome/Chromium 均可。
//!
//! 截图目录由宿主经 [`BrowserCtx::screenshots_dir`] 注入（studio 用
//! `.ox/screenshots/`，ox-idea 用 `.oxidea/screenshots/`）。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams;
use chromiumoxide::cdp::browser_protocol::page::{
    EventScreencastFrame, ScreencastFrameAckParams, StartScreencastFormat, StartScreencastParams,
    StopScreencastParams,
};
use futures::StreamExt as _;
use serde::Serialize;
use serde_json::{json, Value};

use agentscope_core::error::Result;
use agentscope_core::tool::{Tool, ToolOutput};

// ---------- 浏览器可执行文件发现 ----------

/// 探测本机 Chromium 系浏览器：STUDIO_BROWSER_PATH 显式覆盖优先；否则用
/// chromiumoxide 自带的跨平台发现（CHROME 环境变量 → PATH which → Windows
/// 注册表 → Windows/macOS 常见安装路径），Edge/Chrome/Chromium 均可。
pub fn find_browser_executable() -> Option<PathBuf> {
    for key in ["AGENTSCOPE_BROWSER_PATH", "STUDIO_BROWSER_PATH"] {
        if let Ok(p) = std::env::var(key) {
            let p = PathBuf::from(p);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    // 优先 Chrome/Chromium：Edge headless 在 Windows 易秒退，与面板自愈循环
    // 叠加曾引发浏览器进程风暴。两者都在时选 Chrome；只有 Edge 才回退默认探测。
    if let Some(chrome) = prefer_chrome() {
        return Some(chrome);
    }
    chromiumoxide::detection::default_executable(
        chromiumoxide::detection::DetectionOptions::default(),
    )
    .ok()
}

/// 常见 Chrome/Chromium 安装位置（不含 Edge），返回第一个存在的可执行文件。
fn prefer_chrome() -> Option<PathBuf> {
    #[cfg(windows)]
    let candidates: Vec<PathBuf> = {
        let bases: Vec<PathBuf> = ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"]
            .iter()
            .filter_map(std::env::var_os)
            .map(PathBuf::from)
            .collect();
        let rels = [r"Google\Chrome\Application\chrome.exe", r"Chromium\Application\chrome.exe"];
        bases.iter().flat_map(|b| rels.iter().map(move |r| b.join(r))).collect()
    };
    #[cfg(target_os = "macos")]
    let candidates: Vec<PathBuf> = vec![
        PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
        PathBuf::from("/Applications/Chromium.app/Contents/MacOS/Chromium"),
    ];
    #[cfg(not(any(windows, target_os = "macos")))]
    let candidates: Vec<PathBuf> = {
        let dirs: Vec<PathBuf> = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect())
            .unwrap_or_default();
        let cmds = ["google-chrome", "google-chrome-stable", "chromium", "chromium-browser"];
        dirs.iter().flat_map(|d| cmds.iter().map(move |c| d.join(c))).collect()
    };
    candidates.into_iter().find(|p| p.is_file())
}

// ---------- 浏览器会话（懒启动 + 复用 + 死亡自愈） ----------

struct BrowserHandle {
    /// 保活：drop 即结束浏览器进程。brother::Browser 底层即 chromiumoxide
    /// （同 0.9 版本），launch/connect 语义一致；快照与 ref 解析走 brother。
    _browser: brother::Browser,
    /// chromiumoxide 视角的页面（brother::Page::inner() 取回）——既有工具
    /// 代码（CDP 原语、screencast、console 监听）全部继续用它。
    page: Arc<chromiumoxide::Page>,
    /// brother 视角的页面：AX 快照（nth 消歧）与 ref 统一寻址（ref/CSS，
    /// stale 时按 role+name 兜底重定位）。
    bpage: Arc<brother::Page>,
}

pub struct BrowserSession {
    inner: tokio::sync::Mutex<Option<BrowserHandle>>,
    /// 页面控制台错误缓冲（console.error + 未捕获异常）：后台事件监听任务
    /// 写入，工具输出时 drain——Agent 能「看到」页面 JS 报错（白屏根因）。
    console: Arc<std::sync::Mutex<Vec<String>>>,
    /// 直播（screencast）状态：帧广播 + 是否开启。面板与 Agent 工具共用。
    cast: Arc<CastState>,
    /// 启动护栏：浏览器起不来（典型 Edge headless 秒退）时，面板自愈循环
    /// 会反复重 launch 拖垮系统。这里记连续秒退次数与上次启动时刻，限速 +
    /// 熔断，绝不允许无限 spawn。
    launch_guard: std::sync::Mutex<LaunchGuard>,
}

/// 启动熔断参数：浏览器在 `RAPID_DEATH_WINDOW` 内就死掉算一次「秒退」，
/// 连续 `MAX_RAPID_FAILURES` 次后熔断（停止自动重 launch，报错让用户处理）；
/// 两次启动至少间隔 `MIN_LAUNCH_INTERVAL`（限速兜底）；静默 `RESET_WINDOW`
/// 后计数清零，给再次尝试的机会。
const MAX_RAPID_FAILURES: u32 = 3;
const RAPID_DEATH_WINDOW: Duration = Duration::from_secs(5);
const MIN_LAUNCH_INTERVAL: Duration = Duration::from_secs(2);
const RESET_WINDOW: Duration = Duration::from_secs(30);
const LAUNCH_CIRCUIT_MSG: &str =
    "浏览器连续多次启动后立即退出，已停止自动重试以免拖垮系统（常见于 Edge headless 兼容问题）。\
     请安装 Google Chrome，或用环境变量 AGENTSCOPE_BROWSER_PATH 指定可用的 Chromium 浏览器，再重新打开浏览器面板。";

#[derive(Default)]
struct LaunchGuard {
    rapid_failures: u32,
    last_launch: Option<std::time::Instant>,
}

impl LaunchGuard {
    /// 启动前检查：返回本应冷却的时长（限速）；连续秒退达到上限则 Err（熔断，
    /// 调用方据此停止重 launch）。浏览器稳定存活时由 page() 清零 rapid_failures。
    fn pre_launch(&mut self) -> std::result::Result<Duration, String> {
        if let Some(last) = self.last_launch {
            let elapsed = last.elapsed();
            if elapsed >= RESET_WINDOW {
                self.rapid_failures = 0; // 静默足够久，给再次尝试的机会
            } else if elapsed < RAPID_DEATH_WINDOW {
                self.rapid_failures += 1; // 上一个浏览器在窗口内就死了
            }
        }
        if self.rapid_failures >= MAX_RAPID_FAILURES {
            return Err(LAUNCH_CIRCUIT_MSG.to_string());
        }
        Ok(self
            .last_launch
            .map(|last| MIN_LAUNCH_INTERVAL.saturating_sub(last.elapsed()))
            .unwrap_or(Duration::ZERO))
    }
}

/// screencast 广播通道与开关：`tx` 常驻（订阅者随时 subscribe），监听任务
/// 退出以 `active` 为准（stop 后任务在超时轮询里自行结束）。
struct CastState {
    tx: tokio::sync::broadcast::Sender<ScreencastFrame>,
    active: AtomicBool,
}

/// 一帧直播画面：base64 JPEG + 视口尺寸（消费方按比例换算输入坐标）。
#[derive(Clone, Debug, Serialize)]
pub struct ScreencastFrame {
    /// Base64 编码的 JPEG 图像。
    pub data: String,
    /// 视口宽（CSS 像素）——输入坐标按 `device_width / 显示宽` 换算。
    pub device_width: f64,
    /// 视口高（CSS 像素）。
    pub device_height: f64,
}

/// 按工作区装配的浏览器工具上下文：全局共享一个浏览器会话，截图目录
/// 归各工作区（`.ox/screenshots/`）。
pub struct BrowserCtx {
    pub session: Arc<BrowserSession>,
    pub screenshots_dir: PathBuf,
}

fn push_console(buf: &std::sync::Mutex<Vec<String>>, msg: String) {
    let mut guard = buf.lock().unwrap_or_else(|e| e.into_inner());
    if guard.len() < 50 {
        guard.push(msg);
    }
}

impl BrowserSession {
    pub fn new() -> Self {
        let (tx, _) = tokio::sync::broadcast::channel(4);
        Self {
            inner: tokio::sync::Mutex::new(None),
            console: Arc::new(std::sync::Mutex::new(Vec::new())),
            cast: Arc::new(CastState { tx, active: AtomicBool::new(false) }),
            launch_guard: std::sync::Mutex::new(LaunchGuard::default()),
        }
    }

    /// 取走并清空累积的页面控制台错误（工具输出时调用）。
    pub async fn drain_console(&self) -> Vec<String> {
        std::mem::take(&mut *self.console.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// 当前活跃页面（预览面板用）；浏览器未启动返回 None——预览不拉起
    /// 浏览器，只有 Agent 工具调用才启动。
    pub async fn current_page(&self) -> Option<Arc<chromiumoxide::Page>> {
        let guard = self.inner.lock().await;
        guard.as_ref().map(|h| h.page.clone())
    }

    // ---------- 进程级共享会话（面板与 Agent 工具用同一浏览器实例） ----------

    /// 进程级单例：ox-idea 的 agent 工具与浏览器直播面板都拿它，保证
    /// 「agent 操作的页面 = 用户面板里看到的页面」。
    pub fn shared() -> Arc<Self> {
        static SHARED: std::sync::OnceLock<Arc<BrowserSession>> = std::sync::OnceLock::new();
        SHARED.get_or_init(|| Arc::new(BrowserSession::new())).clone()
    }

    /// 主动拉起浏览器（懒启动的公开入口）：面板 attach 时调用；已在运行
    /// 则直接返回现有页面。
    pub async fn ensure_started(&self) -> std::result::Result<Arc<chromiumoxide::Page>, String> {
        self.page().await
    }

    /// 显式关闭浏览器进程。宿主应用退出时必须调用：会话是 `OnceLock` 进程级
    /// 单例，静态变量在进程退出时不析构，`BrowserHandle` 永不 drop，Chrome
    /// 依赖的 kill_on_drop 就永远不会触发——浏览器进程成为孤儿（吃内存、
    /// 占 CDP 端口）。take 出 handle 后 drop 即触发 kill_on_drop 杀进程树。
    pub async fn shutdown(&self) {
        let mut guard = self.inner.lock().await;
        if let Some(handle) = guard.take() {
            drop(handle);
        }
        self.cast.active.store(false, std::sync::atomic::Ordering::SeqCst);
    }

    // ---------- 直播（screencast）：面板画面 ----------

    /// 开启 screencast 帧流（幂等）：返回帧接收端（broadcast，容量 4，慢消费
    /// 丢最旧帧）。每帧收到后立即向浏览器确认——不确认浏览器只发一帧。
    pub async fn start_screencast(
        &self,
    ) -> std::result::Result<tokio::sync::broadcast::Receiver<ScreencastFrame>, String> {
        let page = self.page().await?;
        if self.cast.active.swap(true, Ordering::SeqCst) {
            return Ok(self.cast.tx.subscribe());
        }
        page.execute(
            StartScreencastParams::builder()
                .format(StartScreencastFormat::Jpeg)
                .quality(60)
                // 直播帧与 Agent 视口解耦：视口仍 1680×1200（Agent 测 ~1600px
                // 中台布局必需，device_width/height 元数据随之不变，面板坐标
                // 换算口径不动），但 screencast JPEG 只需面板显示分辨率——按
                // 1.4:1 等比降到 1280×914，帧体积/解码量降约 40%。面板显示宽
                // 通常 <1280，仍过采样，观感基本无损。
                .max_width(1280)
                .max_height(914)
                // 源端降编码率：disable_gpu 下 Chrome 纯 CPU 软件编码，页面有
                // 动画/加载时按 30-60fps 全帧率编码，而转发层只用 10fps，多编
                // 的帧全是与 IDE 抢 CPU 的浪费。every_nth_frame(3) 让 Chrome 每
                // 3 帧才采 1 帧（≈20fps 源），编码开销降约 3 倍；IPC 侧 10fps
                // 节流仍兜底。直播看页面状态非视频，此帧率足够。
                .every_nth_frame(3)
                .build(),
        )
        .await
        .map_err(|e| format!("开启 screencast 失败: {e}"))?;
        let mut events = page
            .event_listener::<EventScreencastFrame>()
            .await
            .map_err(|e| format!("订阅 screencast 帧失败: {e}"))?;
        let tx = self.cast.tx.clone();
        let page_for_ack = page.clone();
        let cast = Arc::clone(&self.cast);
        tokio::spawn(async move {
            loop {
                // 超时轮询 active：stop 后没有新帧来，靠超时退出任务。
                let frame = match tokio::time::timeout(Duration::from_secs(1), events.next()).await {
                    Ok(Some(frame)) => frame,
                    Ok(None) => {
                        // 页面/浏览器关闭，事件流结束：清 active，让面板的
                        // 转发任务感知断流并走重建-重订阅路径（自愈）。
                        cast.active.store(false, std::sync::atomic::Ordering::SeqCst);
                        break;
                    }
                    Err(_) => {
                        if !cast.active.load(Ordering::SeqCst) {
                            break;
                        }
                        continue;
                    }
                };
                // 必须确认，否则浏览器停止发帧。
                let ack = ScreencastFrameAckParams::builder()
                    .session_id(frame.session_id)
                    .build()
                    .map_err(|e| format!("ack 参数错误: {e}"));
                if let Ok(ack) = ack {
                    let _ = page_for_ack.execute(ack).await;
                }
                let meta = &frame.metadata;
                let _ = tx.send(ScreencastFrame {
                    data: String::from(frame.data.clone()),
                    device_width: meta.device_width,
                    device_height: meta.device_height,
                });
            }
        });
        Ok(self.cast.tx.subscribe())
    }

    /// 停止 screencast（幂等）：监听任务在超时轮询里自行退出。
    pub async fn stop_screencast(&self) {
        if !self.cast.active.swap(false, Ordering::SeqCst) {
            return;
        }
        if let Some(page) = self.current_page().await {
            let _ = page.execute(StopScreencastParams::default()).await;
        }
    }

    /// screencast 是否开启（转发任务据此退出）。
    pub fn screencast_active(&self) -> bool {
        self.cast.active.load(Ordering::SeqCst)
    }

    // ---------- 输入回传：面板里的用户操作 → 页面 ----------

    /// 鼠标事件：`event_type` ∈ pressed/released/moved/wheel；`button` ∈
    /// left/middle/right（pressed/released 时传）；wheel 时传 delta。
    pub async fn dispatch_mouse(
        &self,
        event_type: &str,
        x: f64,
        y: f64,
        button: Option<&str>,
        click_count: i64,
        delta_x: f64,
        delta_y: f64,
    ) -> std::result::Result<(), String> {
        let page = self.page().await?;
        let ty = match event_type {
            "pressed" => DispatchMouseEventType::MousePressed,
            "released" => DispatchMouseEventType::MouseReleased,
            "moved" => DispatchMouseEventType::MouseMoved,
            "wheel" => DispatchMouseEventType::MouseWheel,
            other => return Err(format!("未知鼠标事件类型: {other}")),
        };
        let mut params = DispatchMouseEventParams::builder().r#type(ty).x(x).y(y);
        if let Some(btn) = button {
            let b = match btn {
                "left" => MouseButton::Left,
                "middle" => MouseButton::Middle,
                "right" => MouseButton::Right,
                _ => MouseButton::None,
            };
            params = params.button(b);
        }
        if click_count > 0 {
            params = params.click_count(click_count);
        }
        if event_type == "wheel" {
            params = params.delta_x(delta_x).delta_y(delta_y);
        }
        page.execute(params.build().map_err(|e| format!("鼠标事件参数错误: {e}"))?)
            .await
            .map_err(|e| format!("派发鼠标事件失败: {e}"))?;
        Ok(())
    }

    /// 键盘事件：`event_type` ∈ keydown/keyup/char；`modifiers` 为 CDP 位掩码
    /// （Alt=1, Ctrl=2, Meta=4, Shift=8）；`text` 仅 char 事件需要；
    /// `windows_vk` 为 Windows 虚拟键码（特殊键传，普通字符传 0）。
    pub async fn dispatch_key(
        &self,
        event_type: &str,
        key: &str,
        code: &str,
        text: Option<&str>,
        modifiers: i64,
        windows_vk: i64,
    ) -> std::result::Result<(), String> {
        let page = self.page().await?;
        let ty = match event_type {
            "keydown" => DispatchKeyEventType::KeyDown,
            "keyup" => DispatchKeyEventType::KeyUp,
            "char" => DispatchKeyEventType::Char,
            other => return Err(format!("未知键盘事件类型: {other}")),
        };
        let mut params = DispatchKeyEventParams::builder()
            .r#type(ty)
            .key(key)
            .code(code);
        if modifiers != 0 {
            params = params.modifiers(modifiers);
        }
        if let Some(t) = text {
            params = params.text(t);
        }
        if windows_vk != 0 {
            params = params.windows_virtual_key_code(windows_vk);
        }
        page.execute(params.build().map_err(|e| format!("键盘事件参数错误: {e}"))?)
            .await
            .map_err(|e| format!("派发键盘事件失败: {e}"))?;
        Ok(())
    }

    /// 直接插入文本（输入法/粘贴场景：不经键盘事件，按字符插入）。
    pub async fn insert_text(&self, text: &str) -> std::result::Result<(), String> {
        let page = self.page().await?;
        page.execute(InsertTextParams::new(text))
            .await
            .map_err(|e| format!("插入文本失败: {e}"))?;
        Ok(())
    }

    /// 面板人机协同：按 0-1 视口比例点击（实时换算 CSS 像素，与 Agent
    /// 工具同一坐标口径，截图缩放/面板宽度变化不影响命中）。
    pub async fn click_ratio(&self, rx: f64, ry: f64) -> std::result::Result<(), String> {
        let page = self.page().await?;
        let (x, y, _, _) = ratio_to_css(&page, rx, ry).await;
        self.dispatch_mouse("pressed", x, y, Some("left"), 1, 0.0, 0.0)
            .await?;
        self.dispatch_mouse("released", x, y, Some("left"), 1, 0.0, 0.0)
            .await
    }

    /// 面板人机协同：按比例定位滚轮事件（delta 为 CSS 像素，正 y 向下）。
    pub async fn scroll_ratio(
        &self,
        rx: f64,
        ry: f64,
        delta_x: f64,
        delta_y: f64,
    ) -> std::result::Result<(), String> {
        let page = self.page().await?;
        let (x, y, _, _) = ratio_to_css(&page, rx, ry).await;
        self.dispatch_mouse("wheel", x, y, None, 0, delta_x, delta_y)
            .await
    }

    // ---------- 面板导航 ----------

    /// 导航到 `url`，返回页面最终 URL（重定向后）。
    pub async fn navigate(&self, url: &str) -> std::result::Result<String, String> {
        let page = self.page().await?;
        page.goto(url).await.map_err(|e| format!("导航失败: {e}"))?;
        self.current_url().await
    }

    /// 当前页面 URL（evaluate `location.href`，跨 CDP 版本稳定）。
    pub async fn current_url(&self) -> std::result::Result<String, String> {
        let page = self.page().await?;
        let url = page
            .evaluate("location.href")
            .await
            .map_err(|e| format!("读取 URL 失败: {e}"))?
            .into_value::<String>()
            .map_err(|e| format!("读取 URL 失败: {e}"))?;
        Ok(url)
    }

    /// 历史导航：back/forward/reload（evaluate 实现，不依赖 chromiumoxide
    /// 的历史 API 版本差异）。
    pub async fn history_go(&self, action: &str) -> std::result::Result<(), String> {
        let page = self.page().await?;
        let js = match action {
            "back" => "history.back()",
            "forward" => "history.forward()",
            "reload" => "location.reload()",
            other => return Err(format!("未知导航动作: {other}")),
        };
        page.evaluate(js)
            .await
            .map_err(|e| format!("导航动作失败: {e}"))?;
        Ok(())
    }

    /// 取活跃页面；无浏览器或已死亡时（重）启动。headless 静默运行。
    async fn page(&self) -> std::result::Result<Arc<chromiumoxide::Page>, String> {
        let mut guard = self.inner.lock().await;
        if let Some(handle) = guard.as_ref() {
            // 轻量探测：浏览器被用户手动关闭后 evaluate 会失败 → 重建。
            if handle.page.evaluate("1").await.is_ok() {
                // 浏览器稳定存活：清零秒退计数（熔断器复位）。
                self.launch_guard.lock().unwrap_or_else(|e| e.into_inner()).rapid_failures = 0;
                return Ok(handle.page.clone());
            }
            *guard = None;
            // 旧页面已死：screencast 监听任务随之退出（Ok(None) 分支也会
            // 自行清 active），这里同步重置，保证状态一致。
            self.cast.active.store(false, std::sync::atomic::Ordering::SeqCst);
        }
        // 启动护栏：浏览器反复秒退（典型 Edge headless）时，面板自愈循环会
        // 反复重 launch。这里限速 + 熔断，杜绝无限 spawn 浏览器进程拖垮系统。
        let cooldown = self.launch_guard.lock().unwrap_or_else(|e| e.into_inner()).pre_launch()?;
        if !cooldown.is_zero() {
            tokio::time::sleep(cooldown).await;
        }
        self.launch_guard.lock().unwrap_or_else(|e| e.into_inner()).last_launch = Some(std::time::Instant::now());
        let exe = find_browser_executable().ok_or_else(|| {
            "未找到 Chromium 系浏览器。请安装 Microsoft Edge 或 Google Chrome（任一即可），\
             或设置环境变量 AGENTSCOPE_BROWSER_PATH 指向浏览器可执行文件"
                .to_string()
        })?;
        // headless 静默运行：不弹浏览器窗口，画面经 CDP 截图流到右侧
        // 「浏览器直播」面板（等比缩放不失真）。window_size 决定视口：
        // 宽 1680 = 被测中台系统需要的 ~1600px + 滚动条/余量，保证页面
        // 按自然宽度渲染不被挤压（面板 contain 会自动缩小完整显示）；
        // 高 1200 让画面在面板里尽量撑满。
        //
        // user-data-dir 必须每次启动独立：chromiumoxide 默认固定用
        // %TEMP%\chromiumoxide-runner，上次会话的浏览器进程若残留
        // （studio 强退时 drop 未执行），新进程会因 Chrome 单实例机制
        // 把参数转交给残留实例后立即退出（exit 21、stderr 空），之后
        // 每次启动都撞同一把锁、永远起不来。每次换新目录天然绕开，
        // 死亡重建（下方 *guard = None 后重入）也随之自愈。
        let user_data_dir = std::env::temp_dir().join(format!(
            "agentscope-browser-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        ));
        // brother::BrowserConfig 底层转成 chromiumoxide::BrowserConfig，默认已带
        // --headless=new / --no-first-run / --disable-extensions / --disable-sync /
        // --disable-translate / --disable-background-networking / --disable-component-update，
        // 此处只补我们独有的优化 flags。
        let mut config = brother::BrowserConfig {
            headless: true,
            viewport_width: 1680,
            viewport_height: 1200,
            executable: Some(exe),
            user_data_dir: Some(user_data_dir),
            disable_gpu: true,
            ..Default::default()
        };
        config.args = vec![
            // 后台节流三件套：headless 下页面被判定「不可见」后定时器/渲染
            // 会被节流，表现为工具操作莫名变慢、wait_for 超时——全关。
            "--disable-background-timer-throttling".into(),
            "--disable-backgrounding-occluded-windows".into(),
            "--disable-renderer-backgrounding".into(),
            "--no-default-browser-check".into(),
            "--metrics-recording-only".into(),
            "--mute-audio".into(),
            // Edge 后台进程收敛：startup boost 会预拉起常驻后台进程、background
            // mode 让浏览器关窗后仍驻留——headless 自动化里这些只会徒增进程数，
            // 与「秒退-重 launch」叠加时是进程风暴的放大器，全关。
            "--disable-background-mode".into(),
            // 进程数收敛：默认每站点一个 renderer 进程（每个 100-300MB），
            // 测试场景通常单页面——process-per-site 合并同站点进程，
            // renderer-process-limit 兜底上限。低配机器（16G）上 IDEA/studio
            // 与 Chrome 同跑，进程数直接决定内存与调度压力。
            "--process-per-site".into(),
            "--renderer-process-limit=2".into(),
            "--disable-features=Translate,BackForwardCache,AcceptCHFrame,MediaRouter,OptimizationHints,InterestFeedContentSuggestions,msStartupBoost,msBackgroundExtensions,msEfficiencyMode".into(),
        ];
        let (browser, mut handler) = brother::Browser::launch(config)
            .await
            .map_err(|e| {
                format!(
                    "启动浏览器失败: {e}（若错误含 exit status 21 且 stderr 为空，\
                     多为上次残留的浏览器进程占用了会话目录，重试即可换新目录启动）"
                )
            })?;
        // CDP 事件循环必须持续驱动，否则所有操作挂起。
        tokio::spawn(async move {
            while let Some(event) = handler.next().await {
                if event.is_err() {
                    break;
                }
            }
        });
        let bpage = Arc::new(
            browser
                .new_page("about:blank")
                .await
                .map_err(|e| format!("打开标签页失败: {e}"))?,
        );
        let page = Arc::new(bpage.inner().clone());
        // 显式设定页面视口：window_size 只决定浏览器窗口，headless 新标签页
        // 的页面视口仍是 Chromium 默认 800×600——页面按窄屏渲染（响应式布局
        // 塌缩、面板画面浪费），与窗口 1680×1200 不一致。
        // setDeviceMetricsOverride 让 CSS 视口 = 1680×1200，screencast 帧的
        // device_width/height 随之对齐（取决于 CSS 视口，与 screencast JPEG
        // 的降采样上限无关），面板坐标换算（device_width/显示宽）不变。
        page.execute(
            SetDeviceMetricsOverrideParams::builder()
                .width(1680)
                .height(1200)
                .device_scale_factor(1.0)
                .mobile(false)
                .build()
                .map_err(|e| format!("视口参数错误: {e}"))?,
        )
        .await
        .map_err(|e| format!("设置页面视口失败: {e}"))?;
        // 新会话清空旧缓冲（旧页面的报错对新页面无意义）。
        self.console.lock().unwrap_or_else(|e| e.into_inner()).clear();
        // 后台监听 console.error 与未捕获异常 → 缓冲（Agent 测试页面时
        // 能看到 JS 报错，白屏/接口异常不再静默）。
        let console_buf = Arc::clone(&self.console);
        let page_for_console = Arc::clone(&page);
        tokio::spawn(async move {
            if let Ok(mut events) = page_for_console
                .event_listener::<chromiumoxide::cdp::js_protocol::runtime::EventConsoleApiCalled>()
                .await
            {
                while let Some(event) = events.next().await {
                    if matches!(event.r#type, chromiumoxide::cdp::js_protocol::runtime::ConsoleApiCalledType::Error) {
                        let text = event
                            .args
                            .iter()
                            .filter_map(|arg| arg.value.as_ref().map(|v| v.to_string()))
                            .collect::<Vec<_>>()
                            .join(" ");
                        push_console(&console_buf, format!("console.error: {text}"));
                    }
                }
            }
        });
        let console_buf = Arc::clone(&self.console);
        let page_for_exception = Arc::clone(&page);
        tokio::spawn(async move {
            if let Ok(mut events) = page_for_exception
                .event_listener::<chromiumoxide::cdp::js_protocol::runtime::EventExceptionThrown>()
                .await
            {
                while let Some(event) = events.next().await {
                    let details = &event.exception_details;
                    let desc = details
                        .exception
                        .as_ref()
                        .and_then(|ex| ex.description.clone())
                        .unwrap_or_else(|| details.text.clone());
                    push_console(&console_buf, format!("未捕获异常: {desc}"));
                }
            }
        });
        *guard = Some(BrowserHandle {
            _browser: browser,
            page: page.clone(),
            bpage,
        });
        Ok(page)
    }

    /// 取 brother::Page（AX 快照与 ref 解析用）。复用 page() 的存活探测
    /// 与重建逻辑，确保返回时浏览器一定活着。
    async fn bpage(&self) -> std::result::Result<Arc<brother::Page>, String> {
        self.page().await?;
        let guard = self.inner.lock().await;
        guard
            .as_ref()
            .map(|h| h.bpage.clone())
            .ok_or_else(|| "浏览器会话不可用".to_string())
    }

    async fn invalidate(&self) {
        *self.inner.lock().await = None;
    }
}

// ---------- 页面快照（Agent「看」页面的眼睛） ----------

/// 抽取页面标题、可见文本与可交互元素清单（每个元素带可用的 CSS selector）。
const SNAPSHOT_JS: &str = r#"(() => {
  const sel = (el) => {
    if (el.id) return '#' + CSS.escape(el.id);
    const path = [];
    let node = el;
    while (node && node.nodeType === 1 && path.length < 4) {
      let s = node.tagName.toLowerCase();
      if (node.id) { path.unshift('#' + CSS.escape(node.id)); break; }
      const parent = node.parentElement;
      if (parent) {
        const same = [...parent.children].filter(c => c.tagName === node.tagName);
        if (same.length > 1) s += ':nth-of-type(' + (same.indexOf(node) + 1) + ')';
      }
      path.unshift(s);
      node = parent;
    }
    return path.join(' > ');
  };
  const visible = (el) => !!(el.offsetParent || el.getClientRects().length);
  const allInteractive = [...document.querySelectorAll('a[href],button,input,select,textarea,[role="button"],[onclick],[contenteditable]')]
    .filter(visible);
  const elements = allInteractive.slice(0, 200)
    .map(el => ({
      tag: el.tagName.toLowerCase(),
      text: ((el.innerText || el.value || el.placeholder || el.getAttribute('aria-label') || '') + '').trim().slice(0, 50),
      selector: sel(el),
    }));
  const rawText = (document.body ? document.body.innerText : '').replace(/\n{3,}/g, '\n\n');
  return {
    title: document.title,
    url: location.href,
    text: rawText.slice(0, 6000),
    textTruncated: rawText.length > 6000,
    interactiveTotal: allInteractive.length,
    elements
  };
})()"#;

async fn snapshot(page: &chromiumoxide::Page) -> std::result::Result<Value, String> {
    let result = page
        .evaluate(SNAPSHOT_JS)
        .await
        .map_err(|e| format!("读取页面失败: {e}"))?;
    let value: Value = result
        .into_value()
        .map_err(|e| format!("解析页面快照失败: {e}"))?;
    Ok(value)
}

// ---------- 直播画面自适应缩小 ----------

/// 截图前自适应缩小：页面内容宽（固定宽度布局的中台系统常见 1920 设计稿）
/// 超出视口时注入 CSS zoom 把整页等比缩小到刚好装进视口——内容完整不裁切，
/// 看缩小画面足够（DOM 不变；getBoundingClientRect 与 CDP 输入事件同处
/// 缩放后坐标系，点击仍然准确）。内容不超宽时自动恢复 100%。
/// 返回 Some(说明) 表示本次调整了缩放；页面异常时静默跳过（不影响截图）。
pub async fn fit_page_zoom(page: &chromiumoxide::Page) -> Option<String> {
    let result = page.evaluate(FIT_ZOOM_JS).await.ok()?;
    let note: Option<String> = result.into_value().ok()?;
    note
}

const FIT_ZOOM_JS: &str = r#"(() => {
  const de = document.documentElement;
  const body = document.body;
  if (!de || !body) return null;
  const href = location.href;
  if (de.dataset.zoomFitFor !== href) {
    // 新页面（导航/SPA 换路由）：重置决策状态。zoom 残留一并清掉（自愈）。
    de.dataset.zoomFitFor = href;
    de.style.zoom = '';
  }
  // 已注入过 zoom 的页面锁定不动：zoom 会改变 scrollWidth/clientWidth 的
  // 测量坐标系，注入后再重算会形成反馈循环（实测页面被逐步缩到极小）。
  if (de.style.zoom) return null;
  const vw = de.clientWidth;           // 可用宽（不含滚动条），未缩放坐标系
  const contentW = body.scrollWidth;   // 内容宽，测量干净
  if (contentW > vw + 20) {            // 20px≈滚动条宽，边缘溢出不缩
    const z = vw / contentW;
    if (z < 0.5) return '内容过宽（' + Math.round(contentW) + 'px），放弃自动缩放';
    de.style.zoom = z.toFixed(4);
    return '页面已等比缩小到 ' + Math.round(z * 100) + '%（内容 ' + Math.round(contentW) + 'px 超出视口 ' + Math.round(vw) + 'px）';
  }
  return null; // 未注入：下次截图继续检测（适应先窄后宽的页面）
})()"#;

fn render_snapshot(info: &Value) -> String {
    let title = info["title"].as_str().unwrap_or("");
    let url = info["url"].as_str().unwrap_or("");
    let text = info["text"].as_str().unwrap_or("");
    let mut out = format!("页面: {title}\nURL: {url}\n\n[可见文本]\n{text}\n\n[可交互元素]\n");
    if let Some(elements) = info["elements"].as_array() {
        if elements.is_empty() {
            out.push_str("(无)\n");
        }
        for el in elements {
            let tag = el["tag"].as_str().unwrap_or("");
            let text = el["text"].as_str().unwrap_or("");
            let selector = el["selector"].as_str().unwrap_or("");
            out.push_str(&format!("- <{tag}> {text:?} → selector: {selector}\n"));
        }
    }
    // 截断必须显式告知：否则 Agent 会把「快照里没有」误判为「页面上不存在」
    // （实测：列表页 114 个可交互元素，旧上限 60 时弹层/抽屉控件永远列不进来）。
    let total = info["interactiveTotal"].as_u64().unwrap_or(0);
    if total > 200 {
        out.push_str(&format!(
            "\n⚠ 可交互元素共 {total} 个，上方仅列前 200 个；弹层/抽屉内的控件可能未列出——\
             用 browser_evaluate 传 IIFE 精查目标容器\n"
        ));
    }
    if info["textTruncated"].as_bool() == Some(true) {
        out.push_str(
            "\n⚠ 可见文本过长已截断——用 browser_evaluate 精查特定区域的 innerText\n",
        );
    }
    out
}

/// 轮询等待元素出现（替代固定 sleep；SPA 慢加载时 Agent 传 wait_for）。
async fn wait_for_selector(
    page: &chromiumoxide::Page,
    selector: &str,
    timeout_ms: u64,
) -> std::result::Result<(), String> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        if page.find_element(selector).await.is_ok() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("等待元素超时: {selector}"));
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// snapshot 输出 + 附加累积的页面控制台错误（drain）。
async fn render_with_console(session: &BrowserSession, info: &Value) -> String {
    let mut out = render_snapshot(info);
    let console = session.drain_console().await;
    if !console.is_empty() {
        out.push_str("\n[页面控制台错误]\n");
        for line in console {
            out.push_str(&format!("- {line}\n"));
        }
    }
    out
}

/// 动作后的迷你回传：只报 title + URL 一行——动作类工具默认不再回全量页面
/// 快照。全量快照每步几千 token（可见文本 + 最多 200 个交互元素），模型每步
/// 读完整个页面再推理下一步，是浏览器流程缓慢的主因；动作本身不需要看页面，
/// 模型需要判断时主动调 browser_snapshot（或动作参数 detail=true）。
async fn mini_state(page: &chromiumoxide::Page) -> String {
    let fallback = "页面状态读取失败（页面可能正在跳转）".to_owned();
    match page
        .evaluate("(() => ({ title: document.title, url: location.href }))()")
        .await
    {
        Ok(result) => match result.into_value::<Value>() {
            Ok(v) => format!(
                "页面: {} | URL: {}",
                v["title"].as_str().unwrap_or("(无标题)"),
                v["url"].as_str().unwrap_or("(未知)")
            ),
            Err(_) => fallback,
        },
        Err(_) => fallback,
    }
}

fn input_str(input: &Value, key: &str) -> std::result::Result<String, ToolOutput> {
    input
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            ToolOutput::error(format!("缺少必填参数 `{key}`"))
        })
}

fn url_scheme_allowed(url: &str) -> bool {
    url.starts_with("http://")
        || url.starts_with("https://")
        || url == "about:blank"
        || url.starts_with("file://")
}

// ---------- 工具：browser_navigate ----------

pub struct BrowserNavigate {
    pub ctx: Arc<BrowserCtx>,
}

#[async_trait::async_trait]
impl Tool for BrowserNavigate {
    /// 浏览器工具默认免确认（headless 沙箱内操作，用户已确认永久授权）。
    fn is_read_only(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "browser_navigate"
    }

    fn description(&self) -> &str {
        "Open a URL in the built-in real browser (visible window, true rendering). \
         Use it to let the user see a page you built, or to start a browser automation \
         session for testing. Returns the page title, visible text and interactive \
         elements (with CSS selectors for browser_click / browser_type)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "Absolute URL (http/https/file)."},
                "wait_for": {"type": "string", "description": "Optional CSS selector to wait for (up to 5s) before reading the page - use for SPAs."},
                "detail": {"type": "boolean", "description": "Also include title/URL/visible text with CSS selectors. Default false."}
            },
            "required": ["url"]
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput> {
        let url = match input_str(&input, "url") {
            Ok(v) => v,
            Err(e) => return Ok(e),
        };
        if !url_scheme_allowed(&url) {
            return Ok(ToolOutput::error("仅支持 http/https/file URL"));
        }
        let page = match self.ctx.session.page().await {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::error(e)),
        };
        if let Err(e) = page.goto(&url).await {
            // 区分失败原因：浏览器死了才重建；页面加载失败（超时/DNS/内网
            // 不可达）时浏览器还健康，误杀会让面板画面一起丢、下次调用再
            // 白白重启一轮。
            let browser_dead = page.evaluate("1").await.is_err();
            if browser_dead {
                self.ctx.session.invalidate().await;
                return Ok(ToolOutput::error(format!(
                    "导航失败（浏览器已被关闭，下次调用将自动重启）: {e}"
                )));
            }
            let console = self.ctx.session.drain_console().await;
            let mut msg = format!("页面加载失败（浏览器正常，可重试或换 URL）: {e}");
            if !console.is_empty() {
                msg.push_str("\n页面控制台:\n");
                msg.push_str(&console.join("\n"));
            }
            return Ok(ToolOutput::error(msg));
        }
        // SPA 首帧渲染缓冲。
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let mut note = String::new();
        if let Some(selector) = input.get("wait_for").and_then(|v| v.as_str()) {
            if let Err(e) = wait_for_selector(&page, selector, 5000).await {
                note = format!("\n⚠ {e}");
            }
        }
        // 导航后直接给 AX 快照（ref 句柄）——模型拿到即可 click(ref="eN")，
        // 不必再单独 snapshot；DOM 摘要（selector 路径）按需附加。
        let detail_requested = input.get("detail").and_then(|v| v.as_bool()).unwrap_or(false);
        let mut out = match self.ctx.session.bpage().await {
            Ok(b) => match brother_snapshot_text(&b).await {
                Ok(ax) => format!(
                    "可交互元素（ref 句柄，点击/输入用 browser_click / browser_type 的 ref 参数）:\n{ax}"
                ),
                Err(e) => format!("（AX 快照失败: {e}——可调 browser_snapshot 重试）"),
            },
            Err(e) => format!("（AX 快照失败: {e}）"),
        };
        if detail_requested {
            match snapshot(&page).await {
                Ok(info) => {
                    out.push_str(&format!("\n\n{}", render_with_console(&self.ctx.session, &info).await));
                }
                Err(e) => out.push_str(&format!("\n\n（DOM 摘要失败: {e}）")),
            }
        } else {
            let console = self.ctx.session.drain_console().await;
            if !console.is_empty() {
                out.push_str("\n[页面控制台错误]");
                for line in console {
                    out.push_str(&format!("\n- {line}"));
                }
            }
        }
        out.push_str(&note);
        Ok(ToolOutput::ok(out))
    }
}

// ---------- 工具：browser_snapshot ----------

pub struct BrowserSnapshot {
    pub ctx: Arc<BrowserCtx>,
}

#[async_trait::async_trait]
impl Tool for BrowserSnapshot {
    /// 浏览器工具默认免确认（headless 沙箱内操作，用户已确认永久授权）。
    fn is_read_only(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "browser_snapshot"
    }

    fn description(&self) -> &str {
        "Get a compact accessibility-tree snapshot of the current page: one line per \
         interactive element like `[ref=e5] button \"提交\" [disabled]`. This is the \
         PRIMARY perception channel — click/type via ref handles (browser_click / \
         browser_type with ref=\"e5\"), NOT pixel coordinates. Re-snapshot after \
         navigation or when an action returns stale_ref; refs are short-lived \
         handles re-assigned on every snapshot. Set detail=true to also get \
         title/URL/visible text with CSS selectors."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "detail": {"type": "boolean", "description": "Also include title/URL/visible text with CSS selectors. Default false."}
            }
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput> {
        let detail = input.get("detail").and_then(|v| v.as_bool()).unwrap_or(false);
        let page = match self.ctx.session.page().await {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::error(e)),
        };
        let bpage = match self.ctx.session.bpage().await {
            Ok(b) => b,
            Err(e) => return Ok(ToolOutput::error(e)),
        };
        let ax = match brother_snapshot_text(&bpage).await {
            Ok(t) => t,
            Err(e) => return Ok(ToolOutput::error(e)),
        };
        let mut out = format!("可交互元素（ref 句柄，点击/输入用 browser_click / browser_type 的 ref 参数）:\n{ax}");
        if detail {
            match snapshot(&page).await {
                Ok(info) => {
                    out.push_str(&format!("\n\n{}", render_with_console(&self.ctx.session, &info).await));
                }
                Err(e) => out.push_str(&format!("\n\n（DOM 摘要失败: {e}）")),
            }
        } else {
            let console = self.ctx.session.drain_console().await;
            if !console.is_empty() {
                out.push_str("\n[页面控制台错误]");
                for line in console {
                    out.push_str(&format!("\n- {line}"));
                }
            }
        }
        Ok(ToolOutput::ok(out))
    }
}

// ---------- 工具：browser_click ----------

pub struct BrowserClick {
    pub ctx: Arc<BrowserCtx>,
}

#[async_trait::async_trait]
impl Tool for BrowserClick {
    /// 浏览器工具默认免确认（headless 沙箱内操作，用户已确认永久授权）。
    fn is_read_only(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "browser_click"
    }

    fn description(&self) -> &str {
        "Click an element — THREE addressing modes, pick ONE per call: ref=\"e5\" \
         from browser_snapshot's accessibility tree (PREFERRED — copy the ref from \
         `[ref=e5] button \"提交\"`), selector=CSS (fallback when you have a selector \
         but no snapshot), or x,y viewport RATIO 0-1 (last resort for canvas content \
         with no accessibility node). All modes resolve coordinates in REAL TIME at \
         click time (never cached), auto-scroll the element into view, click with \
         real mouse events, and VERIFY via DOM fingerprint + pixel receipt: success \
         returns a one-line event with NO screenshot; stale_ref means the ref could \
         not be resolved (re-snapshot); dead_click auto-retries once then returns a \
         small cropped diagnostic image."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "ref": {"type": "string", "description": "Element ref from browser_snapshot, e.g. \"e5\" (from [ref=e5]). PREFERRED."},
                "selector": {"type": "string", "description": "CSS selector (fallback when no snapshot ref)."},
                "x": {"type": "number", "description": "Viewport X as RATIO 0-1 (canvas fallback; requires y)."},
                "y": {"type": "number", "description": "Viewport Y as RATIO 0-1 (canvas fallback; requires x)."},
                "wait_for": {"type": "string", "description": "Optional CSS selector to wait for (up to 5s) after the click."}
            }
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput> {
        let page = match self.ctx.session.page().await {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::error(e)),
        };
        // 寻址三选一：ref > selector > 比例坐标。ref/selector 统一交给
        // brother 的 resolve_target_center（ref 优先、CSS 兜底，stale 时按
        // role+name 自动重定位），走同一条「实时解析 → 抖动点击 → 验证」管线。
        let target: Option<String> = if let Some(r) = input.get("ref").and_then(|v| v.as_str()) {
            Some(r.to_string())
        } else if let Some(r) = input.get("ref").and_then(|v| v.as_i64()) {
            Some(format!("e{r}"))
        } else if let Ok(sel) = input_str(&input, "selector") {
            Some(sel)
        } else {
            None
        };
        if let Some(t) = target {
            let bpage = match self.ctx.session.bpage().await {
                Ok(b) => b,
                Err(e) => return Ok(ToolOutput::error(e)),
            };
            let out = match click_ref_verified(&self.ctx, &page, &bpage, &t).await {
                Ok(out) => out,
                Err(stale) => return Ok(ToolOutput::error(stale)),
            };
            // wait_for 附加等待（SPA 场景）。
            let mut content = out.content;
            if let Some(sel) = input.get("wait_for").and_then(|v| v.as_str()) {
                if let Err(e) = wait_for_selector(&page, sel, 5000).await {
                    content.push_str(&format!("\n⚠ {e}"));
                }
            }
            return Ok(ToolOutput { content, ..out });
        }
        // 比例坐标兜底（canvas）。
        let (rx, ry) = match (coord(&input, "x"), coord(&input, "y")) {
            (Ok(x), Ok(y)) => (x, y),
            (Ok(_), Err(_)) | (Err(_), Ok(_)) | (Err(_), Err(_)) => {
                return Ok(ToolOutput::error(
                    "寻址三选一：ref=N（首选，来自 browser_snapshot）/ selector=CSS / x,y=视口比例 0-1（canvas 兜底）"
                ))
            }
        };
        if !(0.0..=1.0).contains(&rx) || !(0.0..=1.0).contains(&ry) {
            return Ok(ToolOutput::error(format!(
                "x/y 必须是 0-1 的视口比例坐标（收到 {rx}, {ry}）——例如 0.5 表示视口中部"
            )));
        }
        let (x, y, vw, vh) = ratio_to_css(&page, rx, ry).await;
        let before_fp = capture_fingerprint(&page).await;
        let before_img = capture_region_jpeg(&page, x, y, 96.0).await;
        if let Err(e) = mouse_click(&page, x, y).await {
            return Ok(ToolOutput::error(format!("点击失败: {e}")));
        }
        let note = |why: &str| {
            format!(
                "✓ 已在比例 ({rx:.3}, {ry:.3}) → 像素 ({x:.0}, {y:.0})/视口 {vw:.0}×{vh:.0} 点击——{why}"
            )
        };
        match verify_action_at(&page, x, y, &before_fp, &before_img).await {
            ActionVerify::Hit(why) => {
                let console = self.ctx.session.drain_console().await;
                let mut out = note(&why);
                if !console.is_empty() {
                    out.push_str("\n[页面控制台错误]");
                    for line in console {
                        out.push_str(&format!("\n- {line}"));
                    }
                }
                out.push_str(&format!("\n{}", mini_state(&page).await));
                Ok(ToolOutput::ok(out))
            }
            ActionVerify::Dead => {
                let _ = mouse_click(&page, x, y).await;
                match verify_action_at(&page, x, y, &before_fp, &before_img).await {
                    ActionVerify::Hit(why) => {
                        Ok(ToolOutput::ok(format!("{}（首次点击无效果，重试后生效）", note(&why))))
                    }
                    ActionVerify::Dead => {
                        Ok(dead_click_diagnostic(&self.ctx, &page, x, y, "点击无效果（DOM 与像素均无变化）").await)
                    }
                }
            }
        }
    }
}

// ---------- 工具：browser_type ----------

pub struct BrowserType {
    pub ctx: Arc<BrowserCtx>,
}

#[async_trait::async_trait]
impl Tool for BrowserType {
    /// 浏览器工具默认免确认（headless 沙箱内操作，用户已确认永久授权）。
    fn is_read_only(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "browser_type"
    }

    fn description(&self) -> &str {
        "Type text into an input — THREE addressing modes, pick ONE per call: \
         ref=\"e5\" from browser_snapshot (PREFERRED), selector=CSS, or x,y viewport \
         RATIO 0-1 (canvas fallback). Clicks the element to focus, inserts text via \
         IME (CJK safe — Chinese input works), optionally presses Enter \
         (submit=true). Set clear=true to empty the field first (replace semantics; \
         default appends). VERIFIED like browser_click: one-line event on success, \
         stale_ref / dead_click feedback otherwise."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "ref": {"type": "string", "description": "Input element ref from browser_snapshot, e.g. \"e5\". PREFERRED."},
                "selector": {"type": "string", "description": "CSS selector (fallback when no snapshot ref)."},
                "x": {"type": "number", "description": "Viewport X as RATIO 0-1 (canvas fallback; requires y)."},
                "y": {"type": "number", "description": "Viewport Y as RATIO 0-1 (canvas fallback; requires x)."},
                "text": {"type": "string", "description": "Text to type."},
                "submit": {"type": "boolean", "description": "Press Enter after typing. Default false."},
                "clear": {"type": "boolean", "description": "Empty the field before typing (replace). Default false (append)."},
                "wait_for": {"type": "string", "description": "Optional CSS selector to wait for (up to 5s) after typing/submit."}
            },
            "required": ["text"]
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput> {
        let text = match input_str(&input, "text") {
            Ok(v) => v.to_string(),
            Err(e) => return Ok(e),
        };
        let submit = input.get("submit").and_then(|v| v.as_bool()).unwrap_or(false);
        let clear = input.get("clear").and_then(|v| v.as_bool()).unwrap_or(false);
        let page = match self.ctx.session.page().await {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::error(e)),
        };
        // 寻址三选一：ref > selector > 比例坐标。ref/selector 统一交给
        // brother 的 resolve_target_center，走同一条「聚焦 → 输入 → 验证」管线。
        let target: Option<String> = if let Some(r) = input.get("ref").and_then(|v| v.as_str()) {
            Some(r.to_string())
        } else if let Some(r) = input.get("ref").and_then(|v| v.as_i64()) {
            Some(format!("e{r}"))
        } else if let Ok(sel) = input_str(&input, "selector") {
            Some(sel)
        } else {
            None
        };
        if let Some(t) = target {
            let bpage = match self.ctx.session.bpage().await {
                Ok(b) => b,
                Err(e) => return Ok(ToolOutput::error(e)),
            };
            let (cx, cy) = match resolve_target_in_viewport(&bpage, &t).await {
                Ok(c) => c,
                Err(stale) => return Ok(ToolOutput::error(stale)),
            };
            let label = format!("type(ref={t}, {text:?})");
            let out = type_at_verified(&self.ctx, &page, label, cx, cy, &text, submit, clear).await;
            let mut content = out.content;
            if let Some(sel) = input.get("wait_for").and_then(|v| v.as_str()) {
                if let Err(e) = wait_for_selector(&page, sel, 5000).await {
                    content.push_str(&format!("\n⚠ {e}"));
                }
            }
            return Ok(ToolOutput { content, ..out });
        }
        // 比例坐标兜底（canvas）。
        let (rx, ry) = match (coord(&input, "x"), coord(&input, "y")) {
            (Ok(x), Ok(y)) => (x, y),
            _ => {
                return Ok(ToolOutput::error(
                    "寻址三选一：ref=N（首选）/ selector=CSS / x,y=视口比例 0-1（canvas 兜底）"
                ))
            }
        };
        if !(0.0..=1.0).contains(&rx) || !(0.0..=1.0).contains(&ry) {
            return Ok(ToolOutput::error(format!(
                "x/y 必须是 0-1 的视口比例坐标（收到 {rx}, {ry}）"
            )));
        }
        let (x, y, _, _) = ratio_to_css(&page, rx, ry).await;
        let label = format!("type({text:?})");
        Ok(type_at_verified(&self.ctx, &page, label, x, y, &text, submit, clear).await)
    }
}

// ---------- 工具：browser_screenshot ----------

pub struct BrowserScreenshot {
    pub ctx: Arc<BrowserCtx>,
}

#[async_trait::async_trait]
impl Tool for BrowserScreenshot {
    /// 浏览器工具默认免确认（headless 沙箱内操作，用户已确认永久授权）。
    fn is_read_only(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "browser_screenshot"
    }

    fn description(&self) -> &str {
        "Take a screenshot of the current browser page and RETURN THE IMAGE to you \
         (multimodal) — you see exactly what the page looks like. Viewport screenshot \
         by default: image coordinates map 1:1 to browser_click / browser_type / \
         browser_hover / browser_scroll_at / browser_drag. Set full_page=true for \
         the whole scrollable page (coordinates then do NOT map to viewport tools). \
         Also saved under the workspace `.ox/screenshots/`."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "full_page": {"type": "boolean", "description": "Capture the whole scrollable page instead of the viewport. Default false."}
            }
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput> {
        let full_page = input
            .get("full_page")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let page = match self.ctx.session.page().await {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::error(e)),
        };
        let bytes = match page
            .screenshot(
                chromiumoxide::page::ScreenshotParams::builder()
                    .format(chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat::Png)
                    .full_page(full_page)
                    .build(),
            )
            .await
        {
            Ok(b) => b,
            Err(e) => return Ok(ToolOutput::error(format!("截图失败: {e}"))),
        };
        let dir = &self.ctx.screenshots_dir;
        if std::fs::create_dir_all(dir).is_err() {
            return Ok(ToolOutput::error(format!("无法创建截图目录: {}", dir.display())));
        }
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default();
        let path = dir.join(format!("shot-{stamp}.png"));
        if let Err(e) = std::fs::write(&path, &bytes) {
            return Ok(ToolOutput::error(format!("写入截图失败: {e}")));
        }
        // 图像随结果回传：模型直接「看见」页面（视觉驱动操作的基础）。
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let scope = if full_page { "整页" } else { "视口" };
        Ok(ToolOutput::ok(format!(
            "{scope}截图已保存: {}（画面已随本结果回传——你看到的就是当前页面；\
             视口截图的像素坐标可直接用于 click_at / type_at / hover_at / scroll_at / drag）",
            path.display()
        ))
        .with_image("image/png", b64))
    }
}

// ---------- 工具：browser_close ----------

pub struct BrowserClose {
    pub ctx: Arc<BrowserCtx>,
}

#[async_trait::async_trait]
impl Tool for BrowserClose {
    /// 浏览器工具默认免确认（headless 沙箱内操作，用户已确认永久授权）。
    fn is_read_only(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "browser_close"
    }

    fn description(&self) -> &str {
        "Close the built-in browser and end the automation session. Call it when \
         page testing is done to free resources."
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    async fn call(&self, _input: Value) -> Result<ToolOutput> {
        self.ctx.session.invalidate().await;
        Ok(ToolOutput::ok("浏览器已关闭".to_string()))
    }
}

// ---------- 工具：browser_evaluate ----------

pub struct BrowserEvaluate {
    pub ctx: Arc<BrowserCtx>,
}

#[async_trait::async_trait]
impl Tool for BrowserEvaluate {
    /// 浏览器工具默认免确认（headless 沙箱内操作，用户已确认永久授权）。
    fn is_read_only(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "browser_evaluate"
    }

    fn description(&self) -> &str {
        "Execute arbitrary JavaScript in the current page and return the JSON-serialized \
         result. Use it to probe DOM state that browser_snapshot cannot reach (Shadow DOM, \
         iframes, computed styles, JS variables) or to diagnose rendering/interaction \
         issues. Pass a single expression or an IIFE like `(() => { ... return {...}; })()`. \
         Prefer browser_click / browser_type for interactions instead of DOM writes."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "script": {"type": "string", "description": "JavaScript expression or IIFE to evaluate. The return value must be JSON-serializable plain data."}
            },
            "required": ["script"]
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput> {
        let script = match input_str(&input, "script") {
            Ok(v) => v,
            Err(e) => return Ok(e),
        };
        let page = match self.ctx.session.page().await {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::error(e)),
        };
        let result = match page.evaluate(script.as_str()).await {
            Ok(r) => r,
            Err(e) => return Ok(ToolOutput::error(format!("执行失败: {e}"))),
        };
        let value: Value = match result.into_value() {
            Ok(v) => v,
            Err(e) => {
                return Ok(ToolOutput::error(format!(
                    "结果不可 JSON 序列化（请返回纯数据对象）: {e}"
                )))
            }
        };
        let mut out = serde_json::to_string_pretty(&value).unwrap_or_else(|_| format!("{value:?}"));
        // 防止大结果（如整段 innerHTML）撑爆上下文。
        const MAX_CHARS: usize = 100_000;
        if out.len() > MAX_CHARS {
            out.truncate(MAX_CHARS);
            out.push_str("\n…（结果过长已截断，请缩小返回范围）");
        }
        // 执行的脚本可能触发页面报错，与其他工具一致附加控制台错误。
        let console = self.ctx.session.drain_console().await;
        if !console.is_empty() {
            out.push_str("\n[页面控制台错误]\n");
            for line in console {
                out.push_str(&format!("- {line}\n"));
            }
        }
        Ok(ToolOutput::ok(out))
    }
}

// ---------- 坐标操作工具集（视觉驱动，像人一样操作） ----------
//
// 模型先 browser_screenshot「看见」页面，再按像素坐标发真实输入事件
// （CDP Input 域：dispatchMouseEvent / insertText）——与真人操作走同一
// 输入管线，hover/焦点/事件冒泡全部真实发生，不依赖 CSS selector。
// 每个操作后回传新截图，模型连续看画面决定下一步（丝滑循环）。

use chromiumoxide::cdp::browser_protocol::input::{
    DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams,
    DispatchMouseEventParamsBuilder, DispatchMouseEventType, InsertTextParams, MouseButton,
};

/// 发一个鼠标事件（builder 缺参 / CDP 报错统一转 String）。
async fn mouse_event(
    page: &chromiumoxide::Page,
    event: std::result::Result<DispatchMouseEventParams, String>,
) -> std::result::Result<(), String> {
    let cmd = event?;
    page.execute(cmd)
        .await
        .map_err(|e| format!("鼠标事件发送失败: {e}"))?;
    Ok(())
}

fn mouse_builder(
    event_type: DispatchMouseEventType,
    x: f64,
    y: f64,
) -> DispatchMouseEventParamsBuilder {
    DispatchMouseEventParams::builder()
        .r#type(event_type)
        .x(x)
        .y(y)
}

/// 在 (x, y) 按下并释放左键（真实点击）。
async fn mouse_click(page: &chromiumoxide::Page, x: f64, y: f64) -> std::result::Result<(), String> {
    mouse_event(
        page,
        mouse_builder(DispatchMouseEventType::MousePressed, x, y)
            .button(MouseButton::Left)
            .click_count(1)
            .build(),
    )
    .await?;
    mouse_event(
        page,
        mouse_builder(DispatchMouseEventType::MouseReleased, x, y)
            .button(MouseButton::Left)
            .click_count(1)
            .build(),
    )
    .await
}

/// 操作后的视口截图：存文件 + base64 回传（模型看效果）。
async fn screenshot_with_image(ctx: &BrowserCtx) -> std::result::Result<ToolOutput, String> {
    let page = ctx.session.page().await.map_err(|e| e)?;
    let bytes = page
        .screenshot(
            chromiumoxide::page::ScreenshotParams::builder()
                .format(chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat::Png)
                .full_page(false)
                .build(),
        )
        .await
        .map_err(|e| format!("截图失败: {e}"))?;
    let dir = &ctx.screenshots_dir;
    std::fs::create_dir_all(dir).map_err(|e| format!("无法创建截图目录: {e}"))?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let path = dir.join(format!("shot-{stamp}.png"));
    std::fs::write(&path, &bytes).map_err(|e| format!("写入截图失败: {e}"))?;
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(ToolOutput::ok(format!(
        "操作完成，最新画面如上（已存 {}）",
        path.display()
    ))
    .with_image("image/png", b64))
}

/// 操作工具的公共收尾：附加控制台错误 + 截图回传。
async fn finish_with_screenshot(ctx: &BrowserCtx, note: String) -> ToolOutput {
    let console = ctx.session.drain_console().await;
    let mut note = note;
    if !console.is_empty() {
        note.push_str("\n[页面控制台错误]");
        for line in console {
            note.push_str(&format!("\n- {line}"));
        }
    }
    match screenshot_with_image(ctx).await {
        Ok(out) => out,
        Err(e) => ToolOutput::error(format!("{note}\n截图失败: {e}")),
    }
}

/// 坐标参数解析（x/y 必填，f64）。
fn coord(input: &Value, key: &str) -> std::result::Result<f64, ToolOutput> {    match input.get(key).and_then(|v| v.as_f64()) {
        Some(v) if v.is_finite() => Ok(v),
        _ => Err(ToolOutput::error(format!("{key} 必须是数字坐标"))),
    }
}

// ---------- 动作验证：像素 receipt + DOM fingerprint（截图只做失败诊断） ----------

/// DOM 指纹采集：URL/标题/节点数/文本哈希/滚动/焦点/视口 + 同源子 frame。
/// 文本用哈希而非长度——等长的 tooltip 替换不会被漏掉；子 frame 必须覆盖，
/// 否则 iframe 内一次成功的点击会被误报为「无变化」。跨域 iframe 拿不到
/// contentDocument（标记 cross），由像素 receipt 兜底（截图是合成后画面）。
const FINGERPRINT_JS: &str = r#"(() => {
  const h = (s) => { let x = 5381; for (let i = 0; i < s.length; i++) x = ((x << 5) + x + s.charCodeAt(i)) | 0; return x; };
  const fp = (d) => {
    try {
      return {
        url: d.location.href, title: d.title,
        nodes: d.getElementsByTagName('*').length,
        textHash: h(d.body ? d.body.innerText : ''),
        scrollX: Math.round(d.scrollX), scrollY: Math.round(d.scrollY),
        active: d.activeElement ? d.activeElement.tagName : '',
        vw: d.documentElement.clientWidth, vh: d.documentElement.clientHeight
      };
    } catch (e) { return { cross: true }; }
  };
  const frames = [];
  for (const f of document.querySelectorAll('iframe')) {
    try { frames.push(fp(f.contentDocument)); } catch (e) { frames.push({ cross: true }); }
  }
  return { main: fp(document), frames };
})()"#;

/// 采集 DOM 指纹；页面不可评估（跳转中）返回 None——验证层静默降级。
async fn capture_fingerprint(page: &chromiumoxide::Page) -> Option<Value> {
    let result = page.evaluate(FINGERPRINT_JS).await.ok()?;
    result.into_value::<Value>().ok()
}

/// 指纹差异的人类可读摘要（用于成功回传的一行说明）。
fn fingerprint_diff(before: &Value, after: &Value) -> Option<String> {
    let (b, a) = (before.get("main")?, after.get("main")?);
    let mut parts: Vec<String> = Vec::new();
    for (key, label) in [
        ("url", "URL"),
        ("title", "标题"),
        ("nodes", "节点数"),
        ("textHash", "文本"),
        ("scrollY", "滚动"),
        ("active", "焦点"),
    ] {
        let (bv, av) = (b.get(key), a.get(key));
        if bv.is_some() && av.is_some() && bv != av {
            match key {
                "nodes" => parts.push(format!(
                    "{label} {}→{}",
                    bv.and_then(|v| v.as_u64()).unwrap_or(0),
                    av.and_then(|v| v.as_u64()).unwrap_or(0)
                )),
                "scrollY" => parts.push(format!(
                    "滚动 {}→{}",
                    bv.and_then(|v| v.as_i64()).unwrap_or(0),
                    av.and_then(|v| v.as_i64()).unwrap_or(0)
                )),
                _ => parts.push(label.to_owned()),
            }
        }
    }
    // 子 frame 变化也算命中（iframe 内点击）。
    if parts.is_empty() && before.get("frames") != after.get("frames") {
        parts.push("子 frame 状态变化".to_owned());
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// 裁剪截图：只取 (x, y) 附近 size×size 的区域（CSS 像素，JPEG）。
/// 返回原始字节（None = 截图失败，验证层降级）。
async fn capture_region_jpeg(
    page: &chromiumoxide::Page,
    x: f64,
    y: f64,
    size: f64,
) -> Option<Vec<u8>> {
    use chromiumoxide::cdp::browser_protocol::page::{
        CaptureScreenshotFormat, CaptureScreenshotParams, Viewport,
    };
    let vw = eval_f64(page, "(() => document.documentElement.clientWidth)").await.unwrap_or(0.0);
    let vh = eval_f64(page, "(() => document.documentElement.clientHeight)").await.unwrap_or(0.0);
    // 裁剪框夹到视口内（clip 越界部分 Chromium 会按视口截断，显式夹取更稳）。
    let cx = (x - size / 2.0).max(0.0).min((vw - size).max(0.0));
    let cy = (y - size / 2.0).max(0.0).min((vh - size).max(0.0));
    let clip = Viewport::builder()
        .x(cx)
        .y(cy)
        .width(size.min(vw))
        .height(size.min(vh))
        .scale(1.0)
        .build()
        .ok()?;
    let bytes = page
        .screenshot(
            CaptureScreenshotParams::builder()
                .format(CaptureScreenshotFormat::Jpeg)
                .quality(60)
                .clip(clip)
                .capture_beyond_viewport(false)
                .build(),
        )
        .await
        .ok()?;
    Some(bytes)
}

/// evaluate 取单个 f64（视口尺寸等）；失败返回 None。
async fn eval_f64(page: &chromiumoxide::Page, expr: &str) -> Option<f64> {
    page.evaluate(expr).await.ok()?.into_value::<f64>().ok()
}

/// 像素 receipt：比较两张小截图，返回是否发生显著变化。
/// 阈值容错：单通道差 >40 记为显著像素，>1% 显著像素算「有变化」——
/// 抗时钟/光标闪烁这类微噪声，又不漏真实 UI 变化。
fn region_changed(before: &[u8], after: &[u8]) -> Option<bool> {
    let a = image::load_from_memory(before).ok()?.to_rgb8();
    let b = image::load_from_memory(after).ok()?.to_rgb8();
    if a.dimensions() != b.dimensions() {
        return Some(true);
    }
    let (w, h) = a.dimensions();
    let total = u64::from(w) * u64::from(h);
    let mut significant = 0u64;
    for (pa, pb) in a.pixels().zip(b.pixels()) {
        let d = i32::from(pa.0[0]).abs_diff(i32::from(pb.0[0]))
            + i32::from(pa.0[1]).abs_diff(i32::from(pb.0[1]))
            + i32::from(pa.0[2]).abs_diff(i32::from(pb.0[2]));
        if d > 40 {
            significant += 1;
        }
    }
    Some(significant * 100 > total)
}

/// 动作验证结果：命中（带证据摘要）或沉默（dead action）。
enum ActionVerify {
    /// DOM 或像素任一通道确认变化——动作生效。
    Hit(String),
    /// 两通道都沉默——点击可能被吞（遮挡/跨域 iframe/动画元素）。
    Dead,
}

/// 点击后验证：先比 DOM 指纹（结构化、抗动画噪声），再比坐标邻域像素
/// （覆盖跨域 iframe 等指纹够不到的区域）。两通道任一命中即成功。
async fn verify_action_at(
    page: &chromiumoxide::Page,
    x: f64,
    y: f64,
    before_fp: &Option<Value>,
    before_img: &Option<Vec<u8>>,
) -> ActionVerify {
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    // 通道 1：DOM fingerprint。
    if let (Some(before), Some(after)) = (capture_fingerprint(page).await, before_fp) {
        if let Some(summary) = fingerprint_diff(&before, after) {
            return ActionVerify::Hit(format!("DOM 变化确认: {summary}"));
        }
    }
    // 通道 2：坐标邻域像素 receipt。
    if let (Some(before), Some(after)) =
        (before_img, capture_region_jpeg(page, x, y, 96.0).await)
    {
        if region_changed(before, &after) == Some(true) {
            return ActionVerify::Hit("点击邻域像素变化确认（DOM 指纹未覆盖的区域，如跨域 iframe/canvas）".to_owned());
        }
    }
    ActionVerify::Dead
}

/// 实时视口尺寸：点击那一刻测量，不依赖截图时缓存的值——窗口宽度延迟
/// 稳定、DPI 缩放误报都会让缓存坐标漂移，实时测量消除这类可避免误差。
async fn viewport_size(page: &chromiumoxide::Page) -> (f64, f64) {
    let w = eval_f64(page, "(() => document.documentElement.clientWidth)").await.unwrap_or(0.0);
    let h = eval_f64(page, "(() => document.documentElement.clientHeight)").await.unwrap_or(0.0);
    // 兜底：测量失败用设计视口（与 SetDeviceMetricsOverride 一致）。
    (if w > 0.0 { w } else { 1680.0 }, if h > 0.0 { h } else { 1200.0 })
}

/// 比例坐标 → CSS 像素：模型回传 0-1 视口比例（对图像缩放/分块变换不变，
/// 换模型/换后端不会静默失效），点击前按实时视口换算成 CDP 需要的 CSS 像素。
async fn ratio_to_css(page: &chromiumoxide::Page, rx: f64, ry: f64) -> (f64, f64, f64, f64) {
    let (vw, vh) = viewport_size(page).await;
    (
        (rx * vw).clamp(0.0, vw - 1.0),
        (ry * vh).clamp(0.0, vh - 1.0),
        vw,
        vh,
    )
}

/// dead click 的诊断回传：自动重试一次后仍沉默，裁剪点击邻域（240×240）
/// 作为局部诊断证据回传——截图出现的频率由失败率决定，而不是由步数决定。
async fn dead_click_diagnostic(
    ctx: &BrowserCtx,
    page: &chromiumoxide::Page,
    x: f64,
    y: f64,
    note: &str,
) -> ToolOutput {
    let console = ctx.session.drain_console().await;
    let mut text = format!(
        "{note}\n已自动重试一次仍无效果——点击可能被遮挡、落在跨域 iframe/canvas 上，\
         或目标需要先 hover/滚动到位。下方是点击点附近的局部诊断截图（非全屏）。"
    );
    if !console.is_empty() {
        text.push_str("\n[页面控制台错误]");
        for line in console {
            text.push_str(&format!("\n- {line}"));
        }
    }
    match capture_region_jpeg(page, x, y, 240.0).await {
        Some(bytes) => {
            use base64::Engine as _;
            ToolOutput::ok(text).with_image(
                "image/jpeg",
                base64::engine::general_purpose::STANDARD.encode(&bytes),
            )
        }
        None => ToolOutput::ok(format!("{text}\n（诊断截图失败，可调 browser_screenshot 看全屏）")),
    }
}

// ---------- ref 层：AX 快照（brother）+ ref 实时解析（LLM 只操作 ref，不接触像素坐标） ----------

/// ref 是短期句柄（brother 的 e1/e2，映射到 CDP backendNodeId）：快照重建后
/// 重新分配，动作执行前必须实时解析为坐标，绝不缓存坐标——快照时和点击时
/// 页面可能已变。brother 的解析自带 stale 兜底：backendNodeId 失效时按
/// role+name(+nth) 重新定位，页面小变化不必强制重快照；彻底失败才返回
/// stale_ref，让 LLM 基于新快照重新决策。
/// brother AX 快照 → 文本（可交互元素 + ref 句柄，形如
/// `- button "提交" [ref=e5]`）。brother 的快照带 nth 消歧（同名元素标注
/// 第 N 个）与 interactive_only 过滤；ref 在 stale 时按 role+name 自动兜底。
async fn brother_snapshot_text(bpage: &brother::Page) -> std::result::Result<String, String> {
    let snap = bpage
        .snapshot_with(
            brother::SnapshotOptions::default()
                .interactive_only(true)
                .compact(true),
        )
        .await
        .map_err(|e| format!("AX 快照失败: {e}"))?;
    let tree = snap.tree();
    if tree.trim().is_empty() {
        return Ok("（可访问性树为空——页面可能还在加载，稍后重试）".to_owned());
    }
    Ok(tree.to_string())
}

/// ref/selector → 实时视口坐标（brother resolve_target_center：ref 优先、
/// CSS 兜底）。元素在视口外时先滚到视口中部再取一次坐标
/// （getBoundingClientRect 是视口相对坐标，滚动后变化）。解析彻底失败 =
/// stale_ref（节点消失且 role+name 兜底也无果）。
async fn resolve_target_in_viewport(
    bpage: &brother::Page,
    target: &str,
) -> std::result::Result<(f64, f64), String> {
    let p = bpage.resolve_target_center(target).await.map_err(|e| {
        format!(
            "stale_ref: {target} 无法解析（{e}）——页面可能已变化，请重新 browser_snapshot 获取新 ref"
        )
    })?;
    let (vw, vh) = viewport_size(bpage.inner()).await;
    if p.x < 0.0 || p.y < 0.0 || p.x > vw || p.y > vh {
        let dy = p.y - vh / 2.0;
        let _ = bpage
            .inner()
            .evaluate(format!("window.scrollBy(0, {dy})"))
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let p2 = bpage.resolve_target_center(target).await.map_err(|e| {
            format!("stale_ref: {target} 滚动后仍无法解析（{e}）——请重新 browser_snapshot")
        })?;
        return Ok((p2.x, p2.y));
    }
    Ok((p.x, p.y))
}

/// 中心点附近小抖动（±3px）：模拟人类落点，避免精确正中心命中覆盖层或
/// 触发站点边缘检测（参考 crawlex ref_resolver 的抖动采样思路——它按元素
/// 框采样，brother 只回中心点，这里在中心邻域内抖动）。
fn jitter_center(x: f64, y: f64) -> (f64, f64) {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0) as f64;
    let t = (nanos / 1e9).fract();
    let t2 = (t * 7919.0).fract();
    (x + (t - 0.5) * 6.0, y + (t2 - 0.5) * 6.0)
}

/// ref/selector 点击的公共执行：解析（视口外自动滚动）→ 抖动采样 → 点击 →
/// 双通道验证 → dead 重试 → 裁剪诊断。返回 Ok(ToolOutput)（成功或诊断），
/// Err(String)（stale_ref 等前置失败）。
async fn click_ref_verified(
    ctx: &BrowserCtx,
    page: &chromiumoxide::Page,
    bpage: &brother::Page,
    target: &str,
) -> std::result::Result<ToolOutput, String> {
    let (x, y) = resolve_target_in_viewport(bpage, target).await?;
    let (cx, cy) = jitter_center(x, y);
    let before_fp = capture_fingerprint(page).await;
    let before_img = capture_region_jpeg(page, cx, cy, 96.0).await;
    if let Err(e) = mouse_click(page, cx, cy).await {
        return Ok(ToolOutput::error(format!("点击失败: {e}")));
    }
    let note = |why: &str| {
        format!("✓ click(ref={target}) → 像素 ({cx:.0}, {cy:.0})——{why}")
    };
    match verify_action_at(page, cx, cy, &before_fp, &before_img).await {
        ActionVerify::Hit(why) => {
            let console = ctx.session.drain_console().await;
            let mut out = note(&why);
            if !console.is_empty() {
                out.push_str("\n[页面控制台错误]");
                for line in console {
                    out.push_str(&format!("\n- {line}"));
                }
            }
            out.push_str(&format!("\n{}", mini_state(page).await));
            Ok(ToolOutput::ok(out))
        }
        ActionVerify::Dead => {
            let _ = mouse_click(page, cx, cy).await;
            match verify_action_at(page, cx, cy, &before_fp, &before_img).await {
                ActionVerify::Hit(why) => Ok(ToolOutput::ok(format!(
                    "{}（首次点击无效果，重试后生效）",
                    note(&why)
                ))),
                ActionVerify::Dead => Ok(dead_click_diagnostic(
                    ctx,
                    page,
                    cx,
                    cy,
                    "dead_click: 点击无效果（DOM 与像素均无变化）——元素可能被遮挡、\
                     是 canvas 内控件、或需要先 hover/滚动。若目标在诊断截图中可见，\
                     可用 browser_click 的 x,y 比例坐标兜底。",
                )
                .await),
            }
        }
    }
}

/// 输入管线（ref/selector/比例坐标三路共用）：点击聚焦 → 可选清空（React
/// 兼容的 native setter + input 事件）→ IME insertText → 可选回车 → 双通道
/// 验证 → dead 重试 → 裁剪诊断。label 由调用方拼好（含 ref 或坐标语义）。
async fn type_at_verified(
    ctx: &BrowserCtx,
    page: &chromiumoxide::Page,
    label: String,
    cx: f64,
    cy: f64,
    text: &str,
    submit: bool,
    clear: bool,
) -> ToolOutput {
    const CLEAR_JS: &str = r#"(() => {
      const el = document.activeElement;
      if (!el || (el.tagName !== 'INPUT' && el.tagName !== 'TEXTAREA')) return false;
      const proto = el.tagName === 'TEXTAREA' ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
      const setter = Object.getOwnPropertyDescriptor(proto, 'value').set;
      setter.call(el, '');
      el.dispatchEvent(new Event('input', { bubbles: true }));
      return true;
    })()"#;
    let before_fp = capture_fingerprint(page).await;
    let before_img = capture_region_jpeg(page, cx, cy, 96.0).await;
    if let Err(e) = mouse_click(page, cx, cy).await {
        return ToolOutput::error(format!("聚焦失败: {e}"));
    }
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    if clear {
        let _ = page.evaluate(CLEAR_JS).await;
    }
    if let Err(e) = page
        .execute(InsertTextParams::new(text.to_string()))
        .await
        .map_err(|e| format!("输入失败: {e}"))
    {
        return ToolOutput::error(e);
    }
    if submit {
        for key_type in [DispatchKeyEventType::KeyDown, DispatchKeyEventType::KeyUp] {
            let ev = DispatchKeyEventParams::builder()
                .r#type(key_type)
                .key("Enter")
                .code("Enter")
                .windows_virtual_key_code(13)
                .build();
            if let Ok(cmd) = ev {
                let _ = page.execute(cmd).await;
            }
        }
    }
    let note = |why: &str| {
        format!(
            "✓ {label}{} → 像素 ({cx:.0}, {cy:.0})——{why}",
            if submit { "并回车" } else { "" }
        )
    };
    match verify_action_at(page, cx, cy, &before_fp, &before_img).await {
        ActionVerify::Hit(why) => {
            let console = ctx.session.drain_console().await;
            let mut out = note(&why);
            if !console.is_empty() {
                out.push_str("\n[页面控制台错误]");
                for line in console {
                    out.push_str(&format!("\n- {line}"));
                }
            }
            out.push_str(&format!("\n{}", mini_state(page).await));
            ToolOutput::ok(out)
        }
        ActionVerify::Dead => {
            // 重试：再聚焦再输入。
            let _ = mouse_click(page, cx, cy).await;
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            if clear {
                let _ = page.evaluate(CLEAR_JS).await;
            }
            let _ = page.execute(InsertTextParams::new(text.to_string())).await;
            if submit {
                for key_type in [DispatchKeyEventType::KeyDown, DispatchKeyEventType::KeyUp] {
                    let ev = DispatchKeyEventParams::builder()
                        .r#type(key_type)
                        .key("Enter")
                        .code("Enter")
                        .windows_virtual_key_code(13)
                        .build();
                    if let Ok(cmd) = ev {
                        let _ = page.execute(cmd).await;
                    }
                }
            }
            match verify_action_at(page, cx, cy, &before_fp, &before_img).await {
                ActionVerify::Hit(why) => {
                    ToolOutput::ok(format!("{}（首次无效果，重试后生效）", note(&why)))
                }
                ActionVerify::Dead => {
                    dead_click_diagnostic(
                        ctx,
                        page,
                        cx,
                        cy,
                        "dead_type: 输入无效果（焦点可能没落在输入框上）——确认目标确实是文本框，                         或改用 browser_snapshot 重新定位。",
                    )
                    .await
                }
            }
        }
    }
}

// ---------- 工具：browser_hover ----------

pub struct BrowserHover {
    pub ctx: Arc<BrowserCtx>,
}

#[async_trait::async_trait]
impl Tool for BrowserHover {
    fn is_read_only(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "browser_hover"
    }

    fn description(&self) -> &str {
        "Move the mouse to an element or position — triggers hover menus, tooltips \
         and hover styles. Addressing: ref=\"e5\" from browser_snapshot (PREFERRED) \
         or x,y viewport RATIO 0-1 (canvas fallback). Returns a fresh screenshot so \
         you can see what appeared, plus a pixel-receipt note telling you whether \
         the hover actually triggered a visual change."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "ref": {"type": "string", "description": "Element ref from browser_snapshot, e.g. \"e5\". PREFERRED."},
                "x": {"type": "number", "description": "Viewport X as RATIO 0-1 (when no ref)."},
                "y": {"type": "number", "description": "Viewport Y as RATIO 0-1 (when no ref)."}
            }
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput> {
        let page = match self.ctx.session.page().await {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::error(e)),
        };
        // 寻址二选一：ref > 比例坐标。
        let (x, y, where_note) = if let Some(t) = input
            .get("ref")
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| input.get("ref").and_then(|v| v.as_i64()).map(|r| format!("e{r}")))
        {
            let bpage = match self.ctx.session.bpage().await {
                Ok(b) => b,
                Err(e) => return Ok(ToolOutput::error(e)),
            };
            match resolve_target_in_viewport(&bpage, &t).await {
                Ok((px, py)) => (px, py, format!("ref={t}")),
                Err(stale) => return Ok(ToolOutput::error(stale)),
            }
        } else {
            let (rx, ry) = match (coord(&input, "x"), coord(&input, "y")) {
                (Ok(x), Ok(y)) => (x, y),
                (Err(e), _) | (_, Err(e)) => return Ok(e),
            };
            if !(0.0..=1.0).contains(&rx) || !(0.0..=1.0).contains(&ry) {
                return Ok(ToolOutput::error(format!(
                    "x/y 必须是 0-1 的视口比例坐标（收到 {rx}, {ry}）"
                )));
            }
            let (px, py, _, _) = ratio_to_css(&page, rx, ry).await;
            (px, py, format!("比例 ({rx:.3}, {ry:.3})"))
        };
        let before_img = capture_region_jpeg(&page, x, y, 96.0).await;
        if let Err(e) = mouse_event(
            &page,
            mouse_builder(DispatchMouseEventType::MouseMoved, x, y).build(),
        )
        .await
        {
            return Ok(ToolOutput::error(format!("移动失败: {e}")));
        }
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        // hover 的效果多在 CSS :hover（不进 DOM 指纹）——像素 receipt 是主验证。
        let receipt = match (before_img, capture_region_jpeg(&page, x, y, 96.0).await) {
            (Some(b), Some(a)) => match region_changed(&b, &a) {
                Some(true) => "hover 已触发视觉变化（菜单/tooltip 出现）".to_owned(),
                Some(false) => "hover 邻域无视觉变化——目标可能没有 hover 效果，或效果出现在别处（看截图确认）".to_owned(),
                None => "像素比较失败".to_owned(),
            },
            _ => "像素 receipt 不可用".to_owned(),
        };
        Ok(finish_with_screenshot(&self.ctx, format!("鼠标已移至 {where_note} → 像素 ({x:.0}, {y:.0})——{receipt}")).await)
    }
}

// ---------- 工具：browser_scroll_at ----------

pub struct BrowserScrollAt {
    pub ctx: Arc<BrowserCtx>,
}

#[async_trait::async_trait]
impl Tool for BrowserScrollAt {
    fn is_read_only(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "browser_scroll_at"
    }

    fn description(&self) -> &str {
        "Scroll the mouse wheel at viewport RATIO coordinates (x, y in 0-1) by \
         delta_y pixels (negative = up, positive = down). Returns a fresh screenshot \
         (you scroll to SEE content), plus a scroll-receipt note — \"no effect\" \
         usually means the page is already at the boundary."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "x": {"type": "number", "description": "Viewport X (scroll focus), as RATIO 0-1."},
                "y": {"type": "number", "description": "Viewport Y (scroll focus), as RATIO 0-1."},
                "delta_y": {"type": "number", "description": "Scroll amount in pixels; negative = up."}
            },
            "required": ["x", "y", "delta_y"]
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput> {
        let (rx, ry) = match (coord(&input, "x"), coord(&input, "y")) {
            (Ok(x), Ok(y)) => (x, y),
            (Err(e), _) | (_, Err(e)) => return Ok(e),
        };
        if !(0.0..=1.0).contains(&rx) || !(0.0..=1.0).contains(&ry) {
            return Ok(ToolOutput::error(format!(
                "x/y 必须是 0-1 的视口比例坐标（收到 {rx}, {ry}）"
            )));
        }
        let dy = match coord(&input, "delta_y") {
            Ok(v) => v,
            Err(e) => return Ok(e),
        };
        let page = match self.ctx.session.page().await {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::error(e)),
        };
        let (x, y, _, _) = ratio_to_css(&page, rx, ry).await;
        let before_fp = capture_fingerprint(&page).await;
        if let Err(e) = mouse_event(
            &page,
            mouse_builder(DispatchMouseEventType::MouseWheel, x, y)
                .delta_x(0.0)
                .delta_y(dy)
                .build(),
        )
        .await
        {
            return Ok(ToolOutput::error(format!("滚动失败: {e}")));
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        // 滚动 receipt：scrollY 没变 = 已到边界（或滚动容器不在该点）。
        let receipt = match (before_fp, capture_fingerprint(&page).await) {
            (Some(b), Some(a)) => {
                let (by, ay) = (
                    b["main"]["scrollY"].as_i64().unwrap_or(0),
                    a["main"]["scrollY"].as_i64().unwrap_or(0),
                );
                if by != ay {
                    format!("滚动生效: scrollY {by}→{ay}")
                } else {
                    "滚动无效果（scrollY 未变）——页面可能已在边界，或该点下是不可滚动的区域".to_owned()
                }
            }
            _ => "滚动 receipt 不可用".to_owned(),
        };
        Ok(finish_with_screenshot(
            &self.ctx,
            format!("已在比例 ({rx:.3}, {ry:.3}) 滚动 {dy:.0}px——{receipt}"),
        )
        .await)
    }
}

// ---------- 工具：browser_drag ----------

pub struct BrowserDrag {
    pub ctx: Arc<BrowserCtx>,
}

#[async_trait::async_trait]
impl Tool for BrowserDrag {
    fn is_read_only(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "browser_drag"
    }

    fn description(&self) -> &str {
        "Drag from (from_x, from_y) to (to_x, to_y) — viewport RATIO coordinates \
         (0-1): mouse press, interpolated moves (real drag trajectory), release — \
         for sliders and drag-and-drop. Returns a fresh screenshot plus a \
         fingerprint receipt telling you whether the page state actually changed."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "from_x": {"type": "number", "description": "Start viewport X, as RATIO 0-1."},
                "from_y": {"type": "number", "description": "Start viewport Y, as RATIO 0-1."},
                "to_x": {"type": "number", "description": "End viewport X, as RATIO 0-1."},
                "to_y": {"type": "number", "description": "End viewport Y, as RATIO 0-1."}
            },
            "required": ["from_x", "from_y", "to_x", "to_y"]
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput> {
        let (frx, fry) = match (coord(&input, "from_x"), coord(&input, "from_y")) {
            (Ok(x), Ok(y)) => (x, y),
            (Err(e), _) | (_, Err(e)) => return Ok(e),
        };
        let (trx, try_) = match (coord(&input, "to_x"), coord(&input, "to_y")) {
            (Ok(x), Ok(y)) => (x, y),
            (Err(e), _) | (_, Err(e)) => return Ok(e),
        };
        for (name, v) in [("from_x", frx), ("from_y", fry), ("to_x", trx), ("to_y", try_)] {
            if !(0.0..=1.0).contains(&v) {
                return Ok(ToolOutput::error(format!(
                    "{name} 必须是 0-1 的视口比例坐标（收到 {v}）"
                )));
            }
        }
        let page = match self.ctx.session.page().await {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::error(e)),
        };
        let (fx, fy, _, _) = ratio_to_css(&page, frx, fry).await;
        let (tx, ty, _, _) = ratio_to_css(&page, trx, try_).await;
        let before_fp = capture_fingerprint(&page).await;
        let run = async {
            mouse_event(
                &page,
                mouse_builder(DispatchMouseEventType::MousePressed, fx, fy)
                    .button(MouseButton::Left)
                    .click_count(1)
                    .build(),
            )
            .await?;
            // 插值移动（12 步）：真实拖拽轨迹，drop 目标才能识别。
            let steps = 12;
            for i in 1..=steps {
                let t = i as f64 / steps as f64;
                let mx = fx + (tx - fx) * t;
                let my = fy + (ty - fy) * t;
                mouse_event(
                    &page,
                    mouse_builder(DispatchMouseEventType::MouseMoved, mx, my)
                        .button(MouseButton::Left)
                        .buttons(1)
                        .build(),
                )
                .await?;
            }
            mouse_event(
                &page,
                mouse_builder(DispatchMouseEventType::MouseReleased, tx, ty)
                    .button(MouseButton::Left)
                    .click_count(1)
                    .build(),
            )
            .await
        };
        if let Err(e) = run.await {
            return Ok(ToolOutput::error(format!("拖拽失败: {e}")));
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        // 拖拽 receipt：指纹无变化 = 拖拽可能没被目标识别（HTML5 DnD 需要
        // dragstart 事件，纯鼠标轨迹对部分组件无效）。
        let receipt = match (before_fp, capture_fingerprint(&page).await) {
            (Some(b), Some(a)) => fingerprint_diff(&b, &a)
                .map(|s| format!("拖拽生效: {s}"))
                .unwrap_or_else(|| "拖拽后 DOM 无变化——目标可能需要 HTML5 dragstart 事件（纯鼠标轨迹对部分组件无效），改用 selector 路径或组件专用 API".to_owned()),
            _ => "拖拽 receipt 不可用".to_owned(),
        };
        Ok(finish_with_screenshot(
            &self.ctx,
            format!("已从比例 ({frx:.3}, {fry:.3}) 拖拽到 ({trx:.3}, {try_:.3}) → 像素 ({fx:.0}, {fy:.0})→({tx:.0}, {ty:.0})——{receipt}"),
        )
        .await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_launch_has_no_cooldown_and_no_failure() {
        let mut g = LaunchGuard::default();
        assert!(g.pre_launch().unwrap().is_zero());
        assert_eq!(g.rapid_failures, 0);
    }

    #[test]
    fn rapid_relaunches_trip_the_circuit() {
        let mut g = LaunchGuard::default();
        // 模拟浏览器每次启动后立即死亡：last_launch 紧贴现在（elapsed≈0）。
        for i in 0..MAX_RAPID_FAILURES {
            g.last_launch = Some(std::time::Instant::now());
            let r = g.pre_launch();
            if i + 1 < MAX_RAPID_FAILURES {
                assert!(r.is_ok(), "第 {i} 次秒退不应熔断");
            } else {
                assert!(r.is_err(), "连续秒退达上限应熔断");
            }
        }
        // 熔断后持续拒绝重 launch（不再 spawn 浏览器进程）。
        g.last_launch = Some(std::time::Instant::now());
        assert!(g.pre_launch().is_err());
    }

    #[test]
    fn stable_browser_resets_the_circuit() {
        // 已熔断状态，page() 探测到浏览器稳定存活时清零 rapid_failures。
        let mut g = LaunchGuard {
            rapid_failures: MAX_RAPID_FAILURES,
            last_launch: Some(std::time::Instant::now()),
        };
        g.rapid_failures = 0; // page() 存活分支的复位动作
        assert!(g.pre_launch().is_ok(), "复位后应允许再次启动");
    }

    #[test]
    fn long_silence_resets_failures() {
        let mut g = LaunchGuard {
            rapid_failures: MAX_RAPID_FAILURES,
            last_launch: std::time::Instant::now().checked_sub(RESET_WINDOW + Duration::from_secs(1)),
        };
        assert!(g.pre_launch().is_ok());
        assert_eq!(g.rapid_failures, 0);
    }

    #[test]
    fn recent_launch_yields_cooldown() {
        let mut g = LaunchGuard::default();
        g.last_launch = Some(std::time::Instant::now());
        let cd = g.pre_launch().unwrap();
        assert!(cd > Duration::ZERO, "刚启动过应限速");
        assert!(cd <= MIN_LAUNCH_INTERVAL);
    }
}
