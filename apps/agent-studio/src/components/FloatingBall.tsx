/**
 * 桌面悬浮球（floating-ball 窗口，index.html?view=ball）：
 * 可拖拽的小机器人——左键按住拖拽（data-tauri-drag-region），右键弹菜单
 * （打开主窗 / 切换外观 / 本次隐藏 / 停用）。机器人常驻动画：上下漂浮 +
 * 眨眼（纯 CSS，见 styles.css 的 fb- 段）。
 *
 * 窗口尺寸由后端 ball.rs 管理：平时 92×92，菜单弹出时 invoke
 * ball_menu_resize(true) 扩到 240×300（机器人锚定右上角不动），关闭缩回。
 */

import { useEffect, useRef, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { api } from "../api";

type Appearance = "light" | "dark";
const APPEARANCE_KEY = "studio-ball-appearance";

/** 机器人本体：白色方块头 + 黑脸屏 + 白菱形眼 + 身体 + 绿色电路板。 */
function Robot({ dark }: { dark: boolean }) {
  const shell = dark ? "#334155" : "#f8fafc";
  const shellStroke = dark ? "#475569" : "#e2e8f0";
  const body = dark ? "#1e293b" : "#f1f5f9";
  const screen = dark ? "#e2e8f0" : "#0f172a";
  const eye = dark ? "#0f172a" : "#ffffff";
  return (
    <svg className="fb-robot" viewBox="0 0 96 96" aria-hidden>
      {/* 天线 */}
      <line x1="48" y1="9" x2="48" y2="17" stroke={shellStroke} strokeWidth="3" strokeLinecap="round" />
      <circle cx="48" cy="7" r="3.5" fill="#4ade80" className="fb-antenna" />
      {/* 头 */}
      <rect x="18" y="17" width="60" height="44" rx="10" fill={shell} stroke={shellStroke} strokeWidth="1.5" />
      {/* 脸屏 */}
      <rect x="26" y="25" width="44" height="28" rx="6" fill={screen} />
      {/* 眼睛（菱形；眨眼动画作用在 g 上，避免与 rect 的 rotate 冲突） */}
      <g className="fb-eyes">
        <rect x="35" y="32" width="8" height="8" fill={eye} transform="rotate(45 39 36)" />
        <rect x="51" y="32" width="8" height="8" fill={eye} transform="rotate(45 55 36)" />
      </g>
      {/* 身体 */}
      <rect x="30" y="63" width="36" height="24" rx="8" fill={body} stroke={shellStroke} strokeWidth="1.5" />
      {/* 电路板（右手） */}
      <g className="fb-board">
        <rect x="60" y="66" width="22" height="15" rx="3" fill="#14532d" stroke="#22c55e" strokeWidth="1.2" />
        <path d="M63 70 h5 v3 h4 v-3 h4 M63 76 h6 M72 76 h6" stroke="#4ade80" strokeWidth="1" fill="none" />
        <circle cx="79" cy="70" r="1.6" fill="#4ade80" />
      </g>
    </svg>
  );
}

export default function FloatingBallApp() {
  const [appearance, setAppearance] = useState<Appearance>(
    () => (localStorage.getItem(APPEARANCE_KEY) as Appearance) || "light",
  );
  const [menuOpen, setMenuOpen] = useState(false);
  const menuRef = useRef<HTMLDivElement | null>(null);

  // 菜单开合 → 后端扩缩窗口（机器人锚定右上角不动）。
  useEffect(() => {
    void api.ballMenuResize(menuOpen).catch(() => {});
  }, [menuOpen]);

  // 全局样式给 body 设了 var(--bg) 背景，会盖掉窗口透明——悬浮球窗口置空。
  useEffect(() => {
    document.documentElement.style.background = "transparent";
    document.body.style.background = "transparent";
  }, []);

  // 点击菜单外 / Escape / 窗口失焦（点窗口外的桌面空白）关闭。
  useEffect(() => {
    if (!menuOpen) return;
    const close = (e: MouseEvent) => {
      if (menuRef.current && !menuRef.current.contains(e.target as Node)) setMenuOpen(false);
    };
    const esc = (e: KeyboardEvent) => {
      if (e.key === "Escape") setMenuOpen(false);
    };
    window.addEventListener("mousedown", close);
    window.addEventListener("keydown", esc);
    // 点窗口外（透明区域之外）收不到 mousedown——失焦兜底。
    const unlisten = getCurrentWindow().onFocusChanged((focused) => {
      if (!focused) setMenuOpen(false);
    });
    return () => {
      window.removeEventListener("mousedown", close);
      window.removeEventListener("keydown", esc);
      void unlisten.then((fn) => fn());
    };
  }, [menuOpen]);

  const toggleAppearance = () => {
    setAppearance((prev) => {
      const next = prev === "light" ? "dark" : "light";
      localStorage.setItem(APPEARANCE_KEY, next);
      return next;
    });
    setMenuOpen(false);
  };

  return (
    <div className={`fb-root ${appearance}`}>
      <div
        className="fb-ball"
        data-tauri-drag-region
        onContextMenu={(e) => {
          e.preventDefault();
          setMenuOpen(true);
        }}
      >
        <Robot dark={appearance === "dark"} />
      </div>
      {menuOpen && (
        <div className="fb-menu" ref={menuRef}>
          <button
            className="fb-menu-item"
            onClick={() => {
              setMenuOpen(false);
              void api.ballShowMain().catch(() => {});
            }}
          >
            Open Main Window
          </button>
          <button className="fb-menu-item" onClick={toggleAppearance}>
            Switch Appearance
          </button>
          <div className="fb-menu-sep" />
          <button
            className="fb-menu-item"
            onClick={() => {
              setMenuOpen(false);
              void api.ballHide().catch(() => {});
            }}
          >
            Hide Until Next Restart
          </button>
          <button
            className="fb-menu-item"
            onClick={() => {
              setMenuOpen(false);
              void api.ballSetDisabled(true).catch(() => {});
            }}
          >
            Disable
          </button>
        </div>
      )}
    </div>
  );
}
