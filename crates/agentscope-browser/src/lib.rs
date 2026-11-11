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

use chromiumoxide::Browser;
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
    chromiumoxide::detection::default_executable(
        chromiumoxide::detection::DetectionOptions::default(),
    )
    .ok()
}

// ---------- 浏览器会话（懒启动 + 复用 + 死亡自愈） ----------

struct BrowserHandle {
    /// 保活：drop 即结束浏览器进程。
    _browser: Browser,
    page: Arc<chromiumoxide::Page>,
}

pub struct BrowserSession {
    inner: tokio::sync::Mutex<Option<BrowserHandle>>,
    /// 页面控制台错误缓冲（console.error + 未捕获异常）：后台事件监听任务
    /// 写入，工具输出时 drain——Agent 能「看到」页面 JS 报错（白屏根因）。
    console: Arc<std::sync::Mutex<Vec<String>>>,
    /// 直播（screencast）状态：帧广播 + 是否开启。面板与 Agent 工具共用。
    cast: Arc<CastState>,
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
                .max_width(1680)
                .max_height(1200)
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
                return Ok(handle.page.clone());
            }
            *guard = None;
            // 旧页面已死：screencast 监听任务随之退出（Ok(None) 分支也会
            // 自行清 active），这里同步重置，保证状态一致。
            self.cast.active.store(false, std::sync::atomic::Ordering::SeqCst);
        }
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
        let config = chromiumoxide::BrowserConfig::builder()
            .chrome_executable(exe)
            .user_data_dir(&user_data_dir)
            .window_size(1680, 1200)
            .build()
            .map_err(|e| format!("浏览器配置失败: {e}"))?;
        let (browser, mut handler) = Browser::launch(config)
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
        let page = Arc::new(
            browser
                .new_page("about:blank")
                .await
                .map_err(|e| format!("打开标签页失败: {e}"))?,
        );
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
        });
        Ok(page)
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

/// 操作高亮：给元素画 1.5s 红框，用户在右侧「浏览器直播」面板里能看清
/// Agent 正在操作谁（headless 下这是唯一的视觉通道）。
async fn highlight(element: &chromiumoxide::Element) {
    let _ = element
        .call_js_fn(
            "function() { const el = this; el.style.outline = '2px solid #ff4d4f'; el.style.outlineOffset = '2px'; setTimeout(() => { el.style.outline = ''; el.style.outlineOffset = ''; }, 1500); }",
            false,
        )
        .await;
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
                "wait_for": {"type": "string", "description": "Optional CSS selector to wait for (up to 5s) before reading the page - use for SPAs."}
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
        match snapshot(&page).await {
            Ok(info) => Ok(ToolOutput::ok(format!(
                "{}{}",
                render_with_console(&self.ctx.session, &info).await,
                note
            ))),
            Err(e) => Ok(ToolOutput::error(e)),
        }
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
        "Read the current browser page: title, visible text and interactive elements \
         with CSS selectors. Use after navigation or actions to verify page state."
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    async fn call(&self, _input: Value) -> Result<ToolOutput> {
        let page = match self.ctx.session.page().await {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::error(e)),
        };
        match snapshot(&page).await {
            Ok(info) => {
                let out = render_with_console(&self.ctx.session, &info).await;
                Ok(ToolOutput::ok(out))
            }
            Err(e) => Ok(ToolOutput::error(e)),
        }
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
        "Click an element on the current browser page by CSS selector (from \
         browser_navigate / browser_snapshot output). Returns the page state after \
         the click."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "selector": {"type": "string", "description": "CSS selector of the element to click."},
                "wait_for": {"type": "string", "description": "Optional CSS selector to wait for (up to 5s) after the click."}
            },
            "required": ["selector"]
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput> {
        let selector = match input_str(&input, "selector") {
            Ok(v) => v,
            Err(e) => return Ok(e),
        };
        let page = match self.ctx.session.page().await {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::error(e)),
        };
        let element = match page.find_element(&selector).await {
            Ok(el) => el,
            Err(_) => {
                return Ok(ToolOutput::error(format!(
                    "未找到元素: {selector}（先 browser_snapshot 获取可用 selector）"
                )))
            }
        };
        highlight(&element).await;
        if let Err(e) = element.click().await {
            return Ok(ToolOutput::error(format!("点击失败: {e}")));
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let mut note = String::new();
        if let Some(selector) = input.get("wait_for").and_then(|v| v.as_str()) {
            if let Err(e) = wait_for_selector(&page, selector, 5000).await {
                note = format!("\n⚠ {e}");
            }
        }
        match snapshot(&page).await {
            Ok(info) => Ok(ToolOutput::ok(format!(
                "{}{}",
                render_with_console(&self.ctx.session, &info).await,
                note
            ))),
            Err(e) => Ok(ToolOutput::error(e)),
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
        "Clear an input/textarea (CSS selector) and type text into it. Set submit=true \
         to press Enter afterwards (form submit / search). Returns the page state."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "selector": {"type": "string", "description": "CSS selector of the input element."},
                "text": {"type": "string", "description": "Text to type (replaces existing content)."},
                "submit": {"type": "boolean", "description": "Optional. Press Enter after typing."},
                "wait_for": {"type": "string", "description": "Optional CSS selector to wait for (up to 5s) after typing/submit."}
            },
            "required": ["selector", "text"]
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput> {
        let selector = match input_str(&input, "selector") {
            Ok(v) => v,
            Err(e) => return Ok(e),
        };
        let text = match input_str(&input, "text") {
            Ok(v) => v,
            Err(e) => return Ok(e),
        };
        let submit = input.get("submit").and_then(|v| v.as_bool()).unwrap_or(false);
        let page = match self.ctx.session.page().await {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::error(e)),
        };
        let element = match page.find_element(&selector).await {
            Ok(el) => el,
            Err(_) => {
                return Ok(ToolOutput::error(format!(
                    "未找到元素: {selector}（先 browser_snapshot 获取可用 selector）"
                )))
            }
        };
        // 清空已有内容再输入（replace 语义）。
        let _ = element.call_js_fn("function() { this.focus(); this.value = ''; }", false).await;
        highlight(&element).await;
        if let Err(e) = element.type_str(&text).await {
            return Ok(ToolOutput::error(format!("输入失败: {e}")));
        }
        if submit {
            let _ = element.press_key("Enter").await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let mut note = String::new();
        if let Some(selector) = input.get("wait_for").and_then(|v| v.as_str()) {
            if let Err(e) = wait_for_selector(&page, selector, 5000).await {
                note = format!("\n⚠ {e}");
            }
        }
        match snapshot(&page).await {
            Ok(info) => Ok(ToolOutput::ok(format!(
                "{}{}",
                render_with_console(&self.ctx.session, &info).await,
                note
            ))),
            Err(e) => Ok(ToolOutput::error(e)),
        }
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
         by default: image coordinates map 1:1 to browser_click_at / browser_type_at / \
         browser_hover_at / browser_scroll_at / browser_drag. Set full_page=true for \
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
fn coord(input: &Value, key: &str) -> std::result::Result<f64, ToolOutput> {
    match input.get(key).and_then(|v| v.as_f64()) {
        Some(v) if v.is_finite() => Ok(v),
        _ => Err(ToolOutput::error(format!("{key} 必须是数字坐标"))),
    }
}

// ---------- 工具：browser_click_at ----------

pub struct BrowserClickAt {
    pub ctx: Arc<BrowserCtx>,
}

#[async_trait::async_trait]
impl Tool for BrowserClickAt {
    fn is_read_only(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "browser_click_at"
    }

    fn description(&self) -> &str {
        "Click at viewport pixel coordinates (x, y) with real mouse events — no CSS \
         selector needed. Workflow: browser_screenshot → look at the image → click \
         the pixel position of the target. Returns a fresh screenshot."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "x": {"type": "number", "description": "Viewport X in pixels (from the last screenshot)."},
                "y": {"type": "number", "description": "Viewport Y in pixels (from the last screenshot)."}
            },
            "required": ["x", "y"]
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput> {
        let (x, y) = match (coord(&input, "x"), coord(&input, "y")) {
            (Ok(x), Ok(y)) => (x, y),
            (Err(e), _) | (_, Err(e)) => return Ok(e),
        };
        let page = match self.ctx.session.page().await {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::error(e)),
        };
        if let Err(e) = mouse_click(&page, x, y).await {
            return Ok(ToolOutput::error(format!("点击失败: {e}")));
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        Ok(finish_with_screenshot(&self.ctx, format!("已在 ({x:.0}, {y:.0}) 点击")).await)
    }
}

// ---------- 工具：browser_type_at ----------

pub struct BrowserTypeAt {
    pub ctx: Arc<BrowserCtx>,
}

#[async_trait::async_trait]
impl Tool for BrowserTypeAt {
    fn is_read_only(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "browser_type_at"
    }

    fn description(&self) -> &str {
        "Click at (x, y) to focus an input, then type text via IME insertText (CJK \
         safe — Chinese input works). Set submit=true to press Enter afterwards \
         (form submit / search). Returns a fresh screenshot."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "x": {"type": "number", "description": "Viewport X of the input field."},
                "y": {"type": "number", "description": "Viewport Y of the input field."},
                "text": {"type": "string", "description": "Text to type."},
                "submit": {"type": "boolean", "description": "Press Enter after typing. Default false."}
            },
            "required": ["x", "y", "text"]
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput> {
        let (x, y) = match (coord(&input, "x"), coord(&input, "y")) {
            (Ok(x), Ok(y)) => (x, y),
            (Err(e), _) | (_, Err(e)) => return Ok(e),
        };
        let text = match input_str(&input, "text") {
            Ok(v) => v.to_string(),
            Err(e) => return Ok(e),
        };
        let submit = input.get("submit").and_then(|v| v.as_bool()).unwrap_or(false);
        let page = match self.ctx.session.page().await {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::error(e)),
        };
        if let Err(e) = mouse_click(&page, x, y).await {
            return Ok(ToolOutput::error(format!("聚焦失败: {e}")));
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        if let Err(e) = page
            .execute(InsertTextParams::new(text.as_str().to_string()))
            .await
            .map_err(|e| format!("输入失败: {e}"))
        {
            return Ok(ToolOutput::error(e));
        }
        if submit {
            // Enter：keydown + keyup（windowsVirtualKeyCode 13）。
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
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let note = if submit {
            format!("已在 ({x:.0}, {y:.0}) 输入并回车")
        } else {
            format!("已在 ({x:.0}, {y:.0}) 输入")
        };
        Ok(finish_with_screenshot(&self.ctx, note).await)
    }
}

// ---------- 工具：browser_hover_at ----------

pub struct BrowserHoverAt {
    pub ctx: Arc<BrowserCtx>,
}

#[async_trait::async_trait]
impl Tool for BrowserHoverAt {
    fn is_read_only(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "browser_hover_at"
    }

    fn description(&self) -> &str {
        "Move the mouse to (x, y) — triggers hover menus, tooltips and hover styles. \
         Returns a fresh screenshot so you can see what appeared."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "x": {"type": "number", "description": "Viewport X."},
                "y": {"type": "number", "description": "Viewport Y."}
            },
            "required": ["x", "y"]
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput> {
        let (x, y) = match (coord(&input, "x"), coord(&input, "y")) {
            (Ok(x), Ok(y)) => (x, y),
            (Err(e), _) | (_, Err(e)) => return Ok(e),
        };
        let page = match self.ctx.session.page().await {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::error(e)),
        };
        if let Err(e) = mouse_event(
            &page,
            mouse_builder(DispatchMouseEventType::MouseMoved, x, y).build(),
        )
        .await
        {
            return Ok(ToolOutput::error(format!("移动失败: {e}")));
        }
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        Ok(finish_with_screenshot(&self.ctx, format!("鼠标已移至 ({x:.0}, {y:.0})")).await)
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
        "Scroll the mouse wheel at (x, y) by delta_y pixels (negative = up, positive \
         = down). Returns a fresh screenshot."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "x": {"type": "number", "description": "Viewport X (scroll focus)."},
                "y": {"type": "number", "description": "Viewport Y (scroll focus)."},
                "delta_y": {"type": "number", "description": "Scroll amount in pixels; negative = up."}
            },
            "required": ["x", "y", "delta_y"]
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput> {
        let (x, y) = match (coord(&input, "x"), coord(&input, "y")) {
            (Ok(x), Ok(y)) => (x, y),
            (Err(e), _) | (_, Err(e)) => return Ok(e),
        };
        let dy = match coord(&input, "delta_y") {
            Ok(v) => v,
            Err(e) => return Ok(e),
        };
        let page = match self.ctx.session.page().await {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::error(e)),
        };
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
        Ok(finish_with_screenshot(
            &self.ctx,
            format!("已在 ({x:.0}, {y:.0}) 滚动 {dy:.0}px"),
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
        "Drag from (from_x, from_y) to (to_x, to_y): mouse press, interpolated moves \
         (real drag trajectory), release — for sliders and drag-and-drop. Returns a \
         fresh screenshot."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "from_x": {"type": "number", "description": "Start viewport X."},
                "from_y": {"type": "number", "description": "Start viewport Y."},
                "to_x": {"type": "number", "description": "End viewport X."},
                "to_y": {"type": "number", "description": "End viewport Y."}
            },
            "required": ["from_x", "from_y", "to_x", "to_y"]
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput> {
        let (fx, fy) = match (coord(&input, "from_x"), coord(&input, "from_y")) {
            (Ok(x), Ok(y)) => (x, y),
            (Err(e), _) | (_, Err(e)) => return Ok(e),
        };
        let (tx, ty) = match (coord(&input, "to_x"), coord(&input, "to_y")) {
            (Ok(x), Ok(y)) => (x, y),
            (Err(e), _) | (_, Err(e)) => return Ok(e),
        };
        let page = match self.ctx.session.page().await {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::error(e)),
        };
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
        Ok(finish_with_screenshot(
            &self.ctx,
            format!("已从 ({fx:.0}, {fy:.0}) 拖拽到 ({tx:.0}, {ty:.0})"),
        )
        .await)
    }
}
