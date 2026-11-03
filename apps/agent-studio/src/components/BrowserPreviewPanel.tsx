// 浏览器直播面板：轮询后端 CDP 视口截图，等比缩放显示 Agent 正在操作的
// 页面——与 Agent 看到的画面完全同步（同一 Page 的屏幕共享），真实渲染
// 不失真不挤压。交互仍在独立浏览器窗口（headful），本面板只读观察。
//
// 轮询间隔 600ms：低于人眼卡顿感知，CPU 开销可控（viewport 截图非全页）。
// 浏览器未启动时显示等待态，不主动拉起浏览器（只有 Agent 工具调用才启动）。

import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

const POLL_MS = 600;

export default function BrowserPreviewPanel({ onCollapse }: { onCollapse: () => void }) {
  const [shot, setShot] = useState<string>("");
  const [error, setError] = useState("");

  useEffect(() => {
    let stopped = false;
    const tick = async () => {
      try {
        const data = await invoke<string>("browser_preview_shot");
        if (stopped) return;
        setError("");
        setShot(data);
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

  return (
    <aside className="file-panel browser-preview-panel">
      <div className="file-panel-head">
        <span className="browser-preview-title">
          {shot ? (
            <>
              <span className="live-dot" /> 浏览器直播
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
      <div className="browser-preview-host">
        {error ? (
          <div className="file-empty">{error}</div>
        ) : shot ? (
          <img
            className="browser-preview-img"
            src={`data:image/png;base64,${shot}`}
            alt="浏览器当前画面"
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
  );
}
