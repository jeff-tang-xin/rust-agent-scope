// 内置浏览器自动化工具集：给 Agent 一个真实 Chromium 浏览器（headless，
// 真实渲染不失真），支持导航、读页面结构、点击、输入、截图、关闭。
//
// 底层：chromiumoxide（CDP 协议）驱动本机 Edge/Chrome/Chromium——三者同为
// Chromium 内核，CDP 协议通用，不挑品牌。浏览器进程懒启动（首次工具调用
// 才拉起），会话级复用同一标签页；用户手动关掉浏览器后下次调用自动重启。
//
// 浏览器可执行文件发现（跨平台）：环境变量 STUDIO_BROWSER_PATH 显式指定
// 优先；否则用 chromiumoxide 自带发现（CHROME 环境变量 → PATH which →
// Windows 注册表 → 各平台常见安装路径），Edge/Chrome/Chromium 均可。
//
// 截图保存到工作区 `.ox/screenshots/`（用户可在文件面板查看）。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use chromiumoxide::Browser;
use serde_json::{json, Value};

use agentscope_core::error::Result;
use agentscope_core::tool::{Tool, ToolOutput};

// ---------- 浏览器可执行文件发现 ----------

/// 探测本机 Chromium 系浏览器：STUDIO_BROWSER_PATH 显式覆盖优先；否则用
/// chromiumoxide 自带的跨平台发现（CHROME 环境变量 → PATH which → Windows
/// 注册表 → Windows/macOS 常见安装路径），Edge/Chrome/Chromium 均可。
pub fn find_browser_executable() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("STUDIO_BROWSER_PATH") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
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
        Self {
            inner: tokio::sync::Mutex::new(None),
            console: Arc::new(std::sync::Mutex::new(Vec::new())),
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

    /// 取活跃页面；无浏览器或已死亡时（重）启动。headless 静默运行。
    async fn page(&self) -> std::result::Result<Arc<chromiumoxide::Page>, String> {
        let mut guard = self.inner.lock().await;
        if let Some(handle) = guard.as_ref() {
            // 轻量探测：浏览器被用户手动关闭后 evaluate 会失败 → 重建。
            if handle.page.evaluate("1").await.is_ok() {
                return Ok(handle.page.clone());
            }
            *guard = None;
        }
        let exe = find_browser_executable().ok_or_else(|| {
            "未找到 Chromium 系浏览器。请安装 Microsoft Edge 或 Google Chrome（任一即可），\
             或设置环境变量 STUDIO_BROWSER_PATH 指向浏览器可执行文件"
                .to_string()
        })?;
        // headless 静默运行：不弹浏览器窗口，画面经 CDP 截图流到右侧
        // 「浏览器直播」面板（等比缩放不失真）。window_size 决定视口：
        // 宽 1680 = 被测中台系统需要的 ~1600px + 滚动条/余量，保证页面
        // 按自然宽度渲染不被挤压（面板 contain 会自动缩小完整显示）；
        // 高 1200 让画面在面板里尽量撑满。
        let config = chromiumoxide::BrowserConfig::builder()
            .chrome_executable(exe)
            .window_size(1680, 1200)
            .build()
            .map_err(|e| format!("浏览器配置失败: {e}"))?;
        let (browser, mut handler) = Browser::launch(config)
            .await
            .map_err(|e| format!("启动浏览器失败: {e}"))?;
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

use futures::StreamExt as _;

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
  const elements = [...document.querySelectorAll('a[href],button,input,select,textarea,[role="button"],[onclick],[contenteditable]')]
    .filter(visible).slice(0, 60)
    .map(el => ({
      tag: el.tagName.toLowerCase(),
      text: ((el.innerText || el.value || el.placeholder || el.getAttribute('aria-label') || '') + '').trim().slice(0, 50),
      selector: sel(el),
    }));
  return {
    title: document.title,
    url: location.href,
    text: (document.body ? document.body.innerText : '').replace(/\n{3,}/g, '\n\n').slice(0, 2500),
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
  const cur = parseFloat(de.style.zoom || '1') || 1;
  const vw = de.clientWidth * cur;      // 视口可用宽（视觉 px，不含滚动条）
  const contentW = body.scrollWidth;    // 页面内容宽（缩放坐标系）
  if (contentW * cur > vw + 1) {
    const z = vw / contentW;
    if (Math.abs(cur - z) <= 0.002) return null;
    de.style.zoom = z.toFixed(4);
    return '页面已等比缩小到 ' + Math.round(z * 100) + '%（内容 ' + Math.round(contentW) + 'px 超出视口 ' + Math.round(vw) + 'px）';
  }
  if (Math.abs(cur - 1) > 0.002) {
    de.style.zoom = '';
    return '已恢复 100% 缩放';
  }
  return null;
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
            self.ctx.session.invalidate().await;
            return Ok(ToolOutput::error(format!(
                "导航失败（浏览器可能已被关闭，下次调用将自动重启）: {e}"
            )));
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
        "Take a full-page screenshot of the current browser page, save it under the \
         workspace `.ox/screenshots/` directory and return the file path (user can \
         open it from the file panel)."
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    async fn call(&self, _input: Value) -> Result<ToolOutput> {
        let page = match self.ctx.session.page().await {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::error(e)),
        };
        let bytes = match page
            .screenshot(
                chromiumoxide::page::ScreenshotParams::builder()
                    .format(chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat::Png)
                    .full_page(true)
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
        Ok(ToolOutput::ok(format!(
            "截图已保存: {}\n（可在文件面板的 .ox/screenshots/ 下查看）",
            path.display()
        )))
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
