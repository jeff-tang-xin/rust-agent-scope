/**
 * 桌面悬浮球（floating-ball 窗口，index.html?view=ball）——工作台管家：
 * 单击即开主窗（最常用动作零成本），按住拖拽移动（位移 >5px 才算拖拽，
 * 与单击自然区分），右键弹菜单。天线灯状态感知：监听全局 turn 事件，
 * 绿灯呼吸=空闲、橙灯快闪=有 turn 在跑。
 *
 * 窗口尺寸由后端 ball.rs 管理：平时 92×92，菜单弹出时 invoke
 * ball_menu_resize(true) 扩到 240×320（机器人锚定右上角不动），关闭缩回。
 * 位置记忆：拖拽结束存 localStorage，启动时恢复（前端 setPosition）。
 */

import { useEffect, useRef, useState } from "react";
import { getCurrentWindow, LogicalPosition } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import { api } from "../api";

type Appearance = "light" | "dark";
const APPEARANCE_KEY = "studio-ball-appearance";
const POSITION_KEY = "studio-ball-pos";
/** 位移超过该像素数判定为拖拽（否则单击）。 */
const DRAG_THRESHOLD = 5;

/** 机器人本体：白色方块头 + 黑脸屏 + 白菱形眼 + 身体 + 绿色电路板。
 * 运行中=疯狂工作（身体抖动 + 电路板狂闪 + 天线橙快闪）；
 * 空闲=发呆（身体轻摇 + 眨眼 + 天线绿呼吸）。 */
function Robot({
  dark,
  working,
  attention,
}: {
  dark: boolean;
  working: boolean;
  attention: string | null;
}) {
  const shell = dark ? "#334155" : "#f8fafc";
  const shellStroke = dark ? "#475569" : "#e2e8f0";
  const body = dark ? "#1e293b" : "#f1f5f9";
  const screen = dark ? "#e2e8f0" : "#0f172a";
  const eye = dark ? "#0f172a" : "#ffffff";
  return (
    <svg className="fb-robot" viewBox="0 0 96 96" aria-hidden>
      {/* 天线灯：空闲绿呼吸 / 工作中橙快闪（动画类由 working 切换） */}
      <line x1="48" y1="9" x2="48" y2="17" stroke={shellStroke} strokeWidth="3" strokeLinecap="round" />
      <circle
        cx="48"
        cy="7"
        r="3.5"
        fill={working ? "#fb923c" : "#4ade80"}
        className={working ? "fb-antenna fb-antenna--working" : "fb-antenna"}
      />
      {/* 头 */}
      <rect x="18" y="17" width="60" height="44" rx="10" fill={shell} stroke={shellStroke} strokeWidth="1.5" />
      {/* 脸屏 */}
      <rect x="26" y="25" width="44" height="28" rx="6" fill={screen} />
      {/* 眼睛（菱形；眨眼动画作用在 g 上，避免与 rect 的 rotate 冲突） */}
      <g className="fb-eyes">
        <rect x="35" y="32" width="8" height="8" fill={eye} transform="rotate(45 39 36)" />
        <rect x="51" y="32" width="8" height="8" fill={eye} transform="rotate(45 55 36)" />
      </g>
      {/* 身体（动画作用在 g 上：运行抖动 / 空闲摇摆） */}
      <g className={working ? "fb-body fb-body--working" : "fb-body fb-body--idle"}>
        <rect x="30" y="63" width="36" height="24" rx="8" fill={body} stroke={shellStroke} strokeWidth="1.5" />
        {/* 电路板（右手）：运行中狂闪（焊接中） */}
        <g className={working ? "fb-board fb-board--flash" : "fb-board"}>
          <rect x="60" y="66" width="22" height="15" rx="3" fill="#14532d" stroke="#22c55e" strokeWidth="1.2" />
          <path d="M63 70 h5 v3 h4 v-3 h4 M63 76 h6 M72 76 h6" stroke="#4ade80" strokeWidth="1" fill="none" />
          <circle cx="79" cy="70" r="1.6" fill="#4ade80" />
        </g>
      </g>
      {/* 提醒徽标：等授权橙 / 失败红 / 完成绿（锚定右上角） */}
      {attention && (
        <g className="fb-badge">
          <circle
            cx="82"
            cy="20"
            r="9"
            fill={attention === "needs_input" ? "#fb923c" : attention === "ready" ? "#4ade80" : "#f87171"}
          />
          {attention === "needs_input" && (
            <path d="M82 16 v5 M82 23.2 v0.1" stroke="#0f172a" strokeWidth="1.8" strokeLinecap="round" fill="none" />
          )}
          {attention === "ready" && (
            <path d="M78 20 l2.8 2.8 5-5.4" stroke="#0f172a" strokeWidth="1.8" strokeLinecap="round" fill="none" />
          )}
          {(attention === "failed" || attention === "limited") && (
            <path d="M78.5 16.5 l7 7 M85.5 16.5 l-7 7" stroke="#0f172a" strokeWidth="1.8" strokeLinecap="round" />
          )}
        </g>
      )}
    </svg>
  );
}

export default function FloatingBallApp() {
  const [appearance, setAppearance] = useState<Appearance>(
    () => (localStorage.getItem(APPEARANCE_KEY) as Appearance) || "light",
  );
  const [menuOpen, setMenuOpen] = useState(false);
  const [working, setWorking] = useState(false);
  /** 提醒徽标：needs_input（等授权）> failed > ready；点击球清除。 */
  const [attention, setAttention] = useState<string | null>(null);
  const menuRef = useRef<HTMLDivElement | null>(null);

  // ---- 状态灯 + 徽标：全局 turn / 授权事件（app.emit 广播到所有窗口）----
  useEffect(() => {
    const unlisteners = [
      listen("turn-started", () => {
        setWorking(true);
        setAttention(null);
      }),
      listen<{ outcome: string }>("turn-finished", ({ payload }) => {
        setWorking(false);
        // 中断（interrupted）是用户自己停的，不提醒；完成/失败才上徽标。
        if (payload.outcome === "failed") setAttention("failed");
        else if (payload.outcome === "completed") setAttention("ready");
      }),
      // 工具授权请求（turn 还在跑，turn-finished 不会来）——最高优先级提醒。
      listen<{ kind: string }>("agent-attention", ({ payload }) => {
        if (payload.kind === "needs_input") setAttention("needs_input");
      }),
    ];
    return () => {
      void Promise.all(unlisteners).then((fns) => fns.forEach((fn) => fn()));
    };
  }, []);

  // ---- 启动时恢复上次拖拽到的位置（localStorage 记忆）----
  useEffect(() => {
    const raw = localStorage.getItem(POSITION_KEY);
    if (!raw) return;
    try {
      const { x, y } = JSON.parse(raw) as { x: number; y: number };
      void getCurrentWindow().setPosition(new LogicalPosition(x, y)).catch(() => {});
    } catch {
      localStorage.removeItem(POSITION_KEY);
    }
  }, []);

  // ---- 单击开主窗 / 按住拖拽（位移阈值区分），拖拽结束记忆位置 ----
  const dragRef = useRef<{ startX: number; startY: number; dragging: boolean; poll?: number } | null>(null);
  const onPointerDown = (e: React.PointerEvent) => {
    if (e.button !== 0) return;
    dragRef.current = { startX: e.clientX, startY: e.clientY, dragging: false };
  };
  const onPointerMove = (e: React.PointerEvent) => {
    const d = dragRef.current;
    if (!d || d.dragging) return;
    if (Math.abs(e.clientX - d.startX) < DRAG_THRESHOLD && Math.abs(e.clientY - d.startY) < DRAG_THRESHOLD) return;
    d.dragging = true;
    // 系统拖拽循环可能吞掉后续 pointer 事件——轮询窗口位置兜底记忆。
    const win = getCurrentWindow();
    d.poll = window.setInterval(() => {
      void win
        .outerPosition()
        .then((pos) => {
          const scale = window.devicePixelRatio || 1;
          localStorage.setItem(POSITION_KEY, JSON.stringify({ x: pos.x / scale, y: pos.y / scale }));
        })
        .catch(() => {});
    }, 400);
    void win.startDragging().catch(() => {});
  };
  const onPointerUp = () => {
    const d = dragRef.current;
    dragRef.current = null;
    if (d?.poll) window.clearInterval(d.poll);
    if (d && !d.dragging) {
      // 单击：打开主窗（工作台管家最常用动作，零成本直达）；有提醒时顺带清除。
      setAttention(null);
      void api.ballShowMain().catch(() => {});
    }
  };

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
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={onPointerUp}
        onContextMenu={(e) => {
          e.preventDefault();
          setMenuOpen(true);
        }}
      >
        <Robot dark={appearance === "dark"} working={working} attention={attention} />
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
