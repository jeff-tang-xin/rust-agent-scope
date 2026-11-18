// 浏览器直播面板：轮询后端 CDP 视口截图，等比缩放显示 Agent 正在操作的
// 页面——与 Agent 看到的画面完全同步（同一 Page 的屏幕共享），真实渲染
// 不失真不挤压。
//
// 人机协同：协同模式下可直接在截图上点击/滚动/输入/导航，与 Agent 共驾
// 同一个浏览器页面（坐标按截图矩形换算 0-1 比例，后端实时换算 CSS 像素，
// 与 Agent 工具同一口径）；只读模式回落纯观察。
//
// 轮询间隔 600ms：低于人眼卡顿感知，CPU 开销可控（viewport 截图非全页）。
// 交互后立即补刷一帧，不等轮询。浏览器未启动时显示等待态，不主动拉起
// 浏览器（只有 Agent 工具调用才启动）。

import { useCallback, useEffect, useRef, useState } from "react";
import type { KeyboardEvent as ReactKeyboardEvent, MouseEvent as ReactMouseEvent } from "react";
import { invoke } from "@tauri-apps/api/core";
import { usePanelWidth } from "./usePanelWidth";

const POLL_MS = 600;

// 特殊键的 Windows 虚拟键码（CDP keydown/keyup 需要；后端也有兜底映射）。
const SPECIAL_KEYS: Record<string, number> = {
  Enter: 13,
  Tab: 9,
  Backspace: 8,
  Escape: 27,
  ArrowLeft: 37,
  ArrowUp: 38,
  ArrowRight: 39,
  ArrowDown: 40,
};

export default function BrowserPreviewPanel({ onCollapse }: { onCollapse: () => void }) {
  const [shot, setShot] = useState<string>("");
  const [error, setError] = useState("");
  // 协同模式（默认开）：开=截图可交互，关=只读观察。
  const [coop, setCoop] = useState(true);
  // 地址栏：随截图轮询同步当前 URL；输入框聚焦时不回写，避免打断编辑。
  const [url, setUrl] = useState("");
  // 交互动作的瞬时错误（独立于轮询 error，不遮挡画面，4s 自动消失）。
  const [actionError, setActionError] = useState("");
  const hostRef = useRef<HTMLDivElement>(null);
  const imgRef = useRef<HTMLImageElement>(null);
  const urlFocusRef = useRef(false);
  // 面板宽度（左缘拖拽调宽，localStorage 记忆；320–1200px，对话区保底 420）。
  // 默认值对齐原 CSS min(46vw, 100vw-420px, 720px) 的常见落点。
  const { w: panelWidth, startResize } = usePanelWidth(
    "ox-browser-width",
    Math.min(720, Math.max(320, window.innerWidth - 620)),
    320,
    1200,
  );

  // 交互后主动补刷一帧，让人立即看到效果（不等 600ms 轮询）。
  const refreshShot = useCallback(async () => {
    try {
      setShot(await invoke<string>("browser_preview_shot"));
    } catch (e) {
      setActionError(String(e));
    }
  }, []);

  // 统一的动作包装：执行 → 补刷截图；失败显示瞬时错误条。
  const runAction = useCallback(
    async (fn: () => Promise<unknown>) => {
      try {
        await fn();
        setActionError("");
        await refreshShot();
      } catch (e) {
        setActionError(String(e));
      }
    },
    [refreshShot],
  );

  useEffect(() => {
    if (!actionError) return;
    const timer = window.setTimeout(() => setActionError(""), 4000);
    return () => window.clearTimeout(timer);
  }, [actionError]);

  useEffect(() => {
    let stopped = false;
    const tick = async () => {
      try {
        const data = await invoke<string>("browser_preview_shot");
        if (stopped) return;
        setError("");
        setShot(data);
        if (data) {
          try {
            const u = await invoke<string>("browser_human_url");
            // 地址栏聚焦编辑中不回写；读取失败（页面跳转中等）保留旧值，
            // 下一轮轮询自愈，不影响画面。
            if (!stopped && !urlFocusRef.current) setUrl(u);
          } catch {
            /* 非关键路径：URL 轮询失败静默保留旧值 */
          }
        }
      } catch (e) {
        if (!stopped) setError(String(e));
      }
    };
    void tick();
    const timer = window.setInterval(() => void tick(), POLL_MS);
    return () => {
      stopped = true;
      window.clearInterval(timer);
    };
  }, []);

  // 滚轮必须原生监听：React 的 onWheel 挂在 root 且 passive，
  // preventDefault 无效，无法阻止面板自身滚动。
  useEffect(() => {
    if (!coop) return;
    const host = hostRef.current;
    if (!host) return;
    const onWheel = (e: WheelEvent) => {
      const img = imgRef.current;
      if (!img) return;
      const rect = img.getBoundingClientRect();
      if (rect.width === 0 || rect.height === 0) return;
      // 截图矩形外：保留面板自身滚动。
      if (
        e.clientX < rect.left ||
        e.clientX > rect.right ||
        e.clientY < rect.top ||
        e.clientY > rect.bottom
      ) {
        return;
      }
      e.preventDefault();
      const x = Math.min(1, Math.max(0, (e.clientX - rect.left) / rect.width));
      const y = Math.min(1, Math.max(0, (e.clientY - rect.top) / rect.height));
      // deltaMode 归一为 CSS 像素：0=像素 1=行(×16) 2=页(×视口高)。
      const scale = e.deltaMode === 1 ? 16 : e.deltaMode === 2 ? rect.height : 1;
      void runAction(() =>
        invoke("browser_human_scroll", {
          x,
          y,
          deltaX: e.deltaX * scale,
          deltaY: e.deltaY * scale,
        }),
      );
    };
    host.addEventListener("wheel", onWheel, { passive: false });
    return () => host.removeEventListener("wheel", onWheel);
  }, [coop, runAction]);

  // 截图点击：按 img 矩形换算 0-1 比例（img 为 auto 尺寸等比缩放，
  // 元素盒即图像盒，无内部留白，rect 换算即准确）。
  const onShotClick = (e: ReactMouseEvent<HTMLImageElement>) => {
    if (!coop) return;
    const rect = e.currentTarget.getBoundingClientRect();
    if (rect.width === 0 || rect.height === 0) return;
    const x = (e.clientX - rect.left) / rect.width;
    const y = (e.clientY - rect.top) / rect.height;
    // 键盘焦点跟随点击：点哪输哪。
    hostRef.current?.focus();
    void runAction(() => invoke("browser_human_click", { x, y }));
  };

  const onHostKeyDown = (e: ReactKeyboardEvent<HTMLDivElement>) => {
    if (!coop) return;
    // 单独按下修饰键不转发。
    if (["Shift", "Control", "Alt", "Meta"].includes(e.key)) return;
    const modifiers =
      (e.altKey ? 1 : 0) | (e.ctrlKey ? 2 : 0) | (e.metaKey ? 4 : 0) | (e.shiftKey ? 8 : 0);
    // 可打印字符（含 Shift 组合出的符号）走文本插入，中文输入法友好。
    if (e.key.length === 1 && !e.ctrlKey && !e.metaKey && !e.altKey) {
      e.preventDefault();
      void runAction(() => invoke("browser_human_type", { text: e.key }));
      return;
    }
    // 特殊键与 Ctrl/Cmd/Alt 组合键走 keydown/keyup；preventDefault 避免
    // 触发宿主快捷键（如 Backspace 后退、Tab 切焦点）。
    if (e.key in SPECIAL_KEYS || ((e.ctrlKey || e.metaKey || e.altKey) && e.key.length === 1)) {
      e.preventDefault();
      void runAction(() =>
        invoke("browser_human_key", {
          key: e.key,
          code: e.code,
          modifiers,
          windowsVk: SPECIAL_KEYS[e.key] ?? 0,
        }),
      );
    }
  };

  const gotoUrl = () => {
    const target = url.trim();
    if (!target) return;
    void (async () => {
      try {
        const finalUrl = await invoke<string>("browser_human_navigate", { url: target });
        setUrl(finalUrl);
        setActionError("");
        await refreshShot();
      } catch (e) {
        setActionError(String(e));
      }
    })();
  };

  const historyGo = (action: "back" | "forward" | "reload") =>
    void runAction(() => invoke("browser_human_history", { action }));

  return (
    <>
      <div className="fp-resize" onMouseDown={startResize} aria-hidden />
      <aside className="file-panel browser-preview-panel" style={{ width: panelWidth }}>
      <div className="file-panel-head">
        <span className="browser-preview-title">
          {shot ? (
            <>
              <span className="live-dot" /> 浏览器直播 · {coop ? "协同操作" : "只读观察"}
            </>
          ) : (
            "🌐 浏览器直播 · 等待 Agent 打开页面"
          )}
        </span>
        <span className="spacer" />
        <button className="icon-btn" title="收起面板" onClick={onCollapse}>
          »
        </button>
      </div>
      <div className="browser-preview-nav">
        <button className="icon-btn" title="后退" onClick={() => historyGo("back")}>
          ‹
        </button>
        <button className="icon-btn" title="前进" onClick={() => historyGo("forward")}>
          ›
        </button>
        <button className="icon-btn" title="刷新" onClick={() => historyGo("reload")}>
          ⟳
        </button>
        <input
          className="browser-preview-url"
          value={url}
          placeholder="输入 URL，回车转到"
          spellCheck={false}
          onChange={(e) => setUrl(e.target.value)}
          onFocus={() => {
            urlFocusRef.current = true;
          }}
          onBlur={() => {
            urlFocusRef.current = false;
          }}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              gotoUrl();
            }
          }}
        />
        <button className="icon-btn" title="转到" onClick={gotoUrl}>
          →
        </button>
        <button
          className={`browser-preview-mode${coop ? " active" : ""}`}
          title={coop ? "切换为只读观察（不拦截鼠标键盘）" : "切换为协同操作（点击/输入/滚动直达页面）"}
          onClick={() => setCoop((v) => !v)}
        >
          {coop ? "🖱 协同操作" : "👁 只读观察"}
        </button>
      </div>
      {actionError && <div className="browser-preview-action-error">{actionError}</div>}
      <div
        className={`browser-preview-host${coop ? " coop" : ""}`}
        ref={hostRef}
        tabIndex={coop ? 0 : -1}
        onKeyDown={onHostKeyDown}
      >
        {error ? (
          <div className="file-empty">{error}</div>
        ) : shot ? (
          <img
            className="browser-preview-img"
            ref={imgRef}
            src={`data:image/png;base64,${shot}`}
            alt="浏览器当前画面"
            draggable={false}
            onClick={onShotClick}
          />
        ) : (
          <div className="file-empty">
            浏览器未启动。
            <br />
            Agent 调用 browser_navigate 后，这里实时显示它正在操作的页面。
          </div>
        )}
      </div>
    </aside>
    </>
  );
}
