import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import FloatingBallApp from "./components/FloatingBall";

// 悬浮球窗口与主窗口共用同一 vite 入口，按 URL 参数分流：
// index.html?view=ball → 悬浮球（透明小窗，见 src-tauri/src/ball.rs）。
const view = new URLSearchParams(window.location.search).get("view");
if (view === "ball") {
  ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
    <React.StrictMode>
      <FloatingBallApp />
    </React.StrictMode>,
  );
} else {
  ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
    <React.StrictMode>
      <App />
    </React.StrictMode>,
  );
}
