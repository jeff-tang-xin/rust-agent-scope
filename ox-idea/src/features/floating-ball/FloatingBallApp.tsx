/**
 * 桌面悬浮球（floating-ball 窗口）——终端助手：`>_` 提示符脸屏（工程感），
 * 单击打开/聚焦主项目窗口（最常用动作零成本），按住拖拽移动（位移 >5px
 * 才算拖拽，与单击自然区分），右键弹菜单。
 *
 * 窗口由后端 ball.rs 创建（92×92，透明无边框置顶）。菜单开合由前端
 * setSize/setPosition 扩缩窗口（240×320，机器人锚定右上角不动），关闭缩回。
 * 位置记忆：拖拽结束存 localStorage，启动时恢复（前端 setPosition）。
 */

import { useEffect, useRef, useState } from "react";
import { getAllWindows, getCurrentWindow, LogicalPosition, LogicalSize } from "@tauri-apps/api/window";
import { isTauri } from "../../platform/ipc";
import "./floating-ball.css";

const POSITION_KEY = "oxidea-ball-pos";
/** 平时窗口尺寸（与 ball.rs 的 SMALL_W/H 一致）。 */
const SMALL_W = 92;
const SMALL_H = 92;
/** 菜单弹出时的窗口尺寸（机器人锚定右上角，向左下扩展）。 */
const MENU_W = 240;
const MENU_H = 320;
/** 位移超过该像素数判定为拖拽（否则单击）。 */
const DRAG_THRESHOLD = 5;

/** 主项目窗口：`main` 或 `project-*`（见 src-tauri/src/windows.rs）。 */
function isMainLabel(label: string): boolean {
  return label === "main" || label.startsWith("project-");
}

/** 终端助手本体：方块头 + `>_` 提示符脸屏 + 天线灯。
 * 运行中=疯狂打字（身体抖动 + 胸口齿轮狂转 + 天线橙快闪）；
 * 空闲=发呆（身体轻摇 + 胸口 zzz + 天线绿慢呼吸）。 */
function TerminalBot({ working, attention }: { working: boolean; attention: string | null }) {
  return (
    <svg className={`fb-robot ${working ? "fb-robot--working" : "fb-robot--idle"}`} viewBox="0 0 96 96" aria-hidden>
      {/* 天线灯：空闲绿呼吸 / 工作中橙快闪 */}
      <line x1="48" y1="9" x2="48" y2="17" stroke="#475569" strokeWidth="3" strokeLinecap="round" />
      <circle
        cx="48"
        cy="7"
        r="3.5"
        fill={working ? "#fb923c" : "#4ade80"}
        className={working ? "fb-antenna fb-antenna--working" : "fb-antenna"}
      />
      {/* 头 */}
      <rect x="18" y="17" width="60" height="44" rx="10" fill="#f8fafc" stroke="#e2e8f0" strokeWidth="1.5" />
      {/* 脸屏：终端黑底 + `>_` 提示符（工程感） */}
      <rect x="26" y="25" width="44" height="28" rx="6" fill="#0f172a" />
      <text x="33" y="45" className="fb-prompt" fontSize="16" fontFamily="monospace" fill="#4ade80">
        &gt;_
      </text>
      {/* 身体（动画作用在 g 上：运行抖动 / 空闲摇摆） */}
      <g className="fb-body">
        <rect x="30" y="63" width="36" height="24" rx="8" fill="#f1f5f9" stroke="#e2e8f0" strokeWidth="1.5" />
        {/* 胸口终端屏：运行=齿轮狂转；空闲=zzz 打盹 */}
        <rect x="38" y="69" width="20" height="12" rx="2" fill="#0f172a" />
        {working ? (
          <g className="fb-gear">
            <circle cx="48" cy="75" r="3.2" fill="none" stroke="#4ade80" strokeWidth="1.2" />
            <g stroke="#4ade80" strokeWidth="1.2">
              <line x1="48" y1="70.6" x2="48" y2="72.4" />
              <line x1="48" y1="77.6" x2="48" y2="79.4" />
              <line x1="43.6" y1="75" x2="45.4" y2="75" />
              <line x1="50.6" y1="75" x2="52.4" y2="75" />
              <line x1="45" y1="72" x2="46.2" y2="73.2" />
              <line x1="49.8" y1="76.8" x2="51" y2="78" />
              <line x1="51" y1="72" x2="49.8" y2="73.2" />
              <line x1="46.2" y1="76.8" x2="45" y2="78" />
            </g>
          </g>
        ) : (
          <text x="41" y="78" fontSize="7" fontFamily="monospace" fill="#4ade80" className="fb-zzz">
            zzz
          </text>
        )}
      </g>
      {/* 提醒徽标：审批橙 / 失败红 / 完成绿（锚定右上角） */}
      {attention && (
        <g className="fb-badge">
          <circle cx="82" cy="20" r="9" fill={attention === "needs_input" ? "#fb923c" : attention === "ready" ? "#4ade80" : "#f87171"} />
          {attention === "needs_input" && (
            <path d="M82 16 v5 M82 23.2 v0.1" stroke="#0f172a" strokeWidth="1.8" strokeLinecap="round" fill="none" />
          )}
          {attention === "ready" && <path d="M78 20 l2.8 2.8 5-5.4" stroke="#0f172a" strokeWidth="1.8" strokeLinecap="round" fill="none" />}
          {(attention === "failed" || attention === "limited") && (
            <path d="M78.5 16.5 l7 7 M85.5 16.5 l-7 7" stroke="#0f172a" strokeWidth="1.8" strokeLinecap="round" />
          )}
        </g>
      )}
    </svg>
  );
}

export default function FloatingBallApp() {
  const [menuOpen, setMenuOpen] = useState(false);
  const menuRef = useRef<HTMLDivElement | null>(null);
  /** agent 任务状态（`oxidea://agent-attention` 全局广播，按 task_id 聚合）。 */
  const [tasks, setTasks] = useState<Map<string, { status: string; window: string; title: string }>>(new Map());

  // ---- agent 状态广播 → 状态灯（任一任务 running=工作中）+ 提醒徽标 ----
  useEffect(() => {
    if (!isTauri()) return;
    let off: (() => void) | undefined;
    void import("@tauri-apps/api/event").then(({ listen }) =>
      listen<{ task_id: string; status: string; title: string; window: string }>(
        "oxidea://agent-attention",
        ({ payload }) => {
          setTasks((prev) => {
            const next = new Map(prev);
            next.set(payload.task_id, { status: payload.status, window: payload.window, title: payload.title });
            return next;
          });
        },
      ).then((fn) => {
        off = fn;
      }),
    );
    return () => off?.();
  }, []);

  const working = [...tasks.values()].some((t) => t.status === "running");
  // 提醒徽标取优先级最高的一条：审批 > 失败/限额 > 完成。
  const attention = [...tasks.values()].reduce<{ status: string; window: string; title: string } | null>((best, t) => {
    const rank = (s: string) => (s === "needs_input" ? 3 : s === "failed" || s === "limited" ? 2 : s === "ready" ? 1 : 0);
    if (rank(t.status) === 0) return best;
    if (!best || rank(t.status) > rank(best.status)) return t;
    return best;
  }, null);

  /** 打开/聚焦主项目窗口：优先提醒来源窗口（发起任务的那个），否则任意主窗。 */
  const openMain = (preferred?: string) => {
    void (async () => {
      const windows = await getAllWindows();
      const main = windows.find((w) => w.label === preferred) ?? windows.find((w) => isMainLabel(w.label));
      if (!main) return;
      await main.show().catch(() => {});
      await main.unminimize().catch(() => {});
      await main.setFocus().catch(() => {});
    })().catch(() => {});
  };

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
      // 单击：有提醒时直达来源窗口并清除该提醒；否则打开任意主窗。
      if (attention) {
        const source = attention.window;
        setTasks((prev) => {
          const next = new Map(prev);
          for (const [id, t] of next) if (t.status === attention.status && t.window === source) next.delete(id);
          return next;
        });
        openMain(source);
      } else {
        openMain();
      }
    }
  };

  // 菜单开合 → 前端扩缩窗口（机器人锚定右上角不动：向左下扩展/缩回）。
  useEffect(() => {
    const win = getCurrentWindow();
    if (menuOpen) {
      void win
        .outerPosition()
        .then((pos) => {
          const scale = window.devicePixelRatio || 1;
          const x = pos.x / scale;
          const y = pos.y / scale;
          return win.setPosition(new LogicalPosition(x - (MENU_W - SMALL_W), y));
        })
        .then(() => win.setSize(new LogicalSize(MENU_W, MENU_H)))
        .catch(() => {});
    } else {
      void win.setSize(new LogicalSize(SMALL_W, SMALL_H)).catch(() => {});
    }
  }, [menuOpen]);

  // 全局样式给 body 设了背景，会盖掉窗口透明——悬浮球窗口置空。
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

  return (
    <div className="fb-root">
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
        <TerminalBot working={working} attention={attention?.status ?? null} />
      </div>
      {menuOpen && (
        <div className="fb-menu" ref={menuRef}>
          <button
            className="fb-menu-item"
            onClick={() => {
              setMenuOpen(false);
              openMain();
            }}
          >
            Open Project Window
          </button>
          <div className="fb-menu-sep" />
          <button
            className="fb-menu-item"
            onClick={() => {
              setMenuOpen(false);
              void getCurrentWindow().hide().catch(() => {});
            }}
          >
            Hide Until Next Restart
          </button>
        </div>
      )}
    </div>
  );
}
