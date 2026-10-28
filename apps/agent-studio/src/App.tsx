// 应用骨架：顶部导航条（品牌 + 协同/运维/数据库运维/设置 + 主题）+ 主区。
// 工作区三大类对齐产品概念：协同（solo/team 可切换，plan/free 可切换）、
// 运维（shell/web）、数据库运维（只读 SQL）——都是工作区，针对不同的项目。
//
// 布局：无工作区 → 空态创建页；有工作区 → 会话视图（左栏工作区+session 列表
// + 消息流 | 右侧文件面板），导航在顶部横条。
// 会话 keep-alive：所有工作区的会话常挂（仅隐藏非当前）——事件监听持续归流，
// 多项目并行时切走再切回，流式内容/工具进度/busy 状态无缝续接。

import { useCallback, useEffect, useState } from "react";
import {
  api,
  onAgentEvent,
  onTurnFinished,
  onTurnStarted,
  type AgentConfig,
  type DatabaseServerConfig,
  type ServerConfig,
  type WorkspaceConfig,
  type WorkspaceKind,
} from "./api";
import Conversation from "./components/Conversation";
import SessionSidebar from "./components/SessionSidebar";
import WorkspaceFormModal from "./components/WorkspaceFormModal";
import WorkspacePage, { type Category } from "./pages/WorkspacePage";
import SettingsPage from "./pages/SettingsPage";
import "./styles.css";

type Tab = Category | "settings";

const CATS: { key: WorkspaceKind; label: string }[] = [
  { key: "collab", label: "协同" },
  { key: "ops", label: "运维" },
  { key: "db", label: "数据库运维" },
];

/** 工作区默认 session（与 Conversation 原逻辑一致：solo/team 各自空间）。 */
const defaultSession = (mode: "solo" | "team") => (mode === "team" ? "team-main" : "main");

export default function App() {
  const [tab, setTab] = useState<Tab>("collab");
  const [agents, setAgents] = useState<AgentConfig[]>([]);
  const [workspaces, setWorkspaces] = useState<WorkspaceConfig[]>([]);
  // 服务器/数据库档案（工作区表单的绑定选择；加载失败不阻塞主流程）。
  const [servers, setServers] = useState<ServerConfig[]>([]);
  const [databases, setDatabases] = useState<DatabaseServerConfig[]>([]);
  const [loaded, setLoaded] = useState(false);
  // 全局 toast 栈（右上角，自动消失；error 停留更久且可手动关闭）。
  const [toasts, setToasts] = useState<{ id: number; kind: "info" | "error"; text: string }[]>([]);
  // 确认弹窗（替换 window.confirm）：promise resolve 挂在 state 上。
  const [confirmState, setConfirmState] = useState<{
    title: string;
    message: string;
    resolve: (ok: boolean) => void;
  } | null>(null);
  // 当前选中的工作区（切类别时自动落到该类第一个）。
  const [selectedId, setSelectedId] = useState("");
  // 导航显隐配置（Settings 页「界面」可改，localStorage 持久 + CustomEvent 通知）。
  const readNav = (key: string) => localStorage.getItem(key) !== "0";
  const [showOps, setShowOps] = useState(() => readNav("ox-nav:show-ops"));
  const [showDb, setShowDb] = useState(() => readNav("ox-nav:show-db"));
  useEffect(() => {
    const onNavChange = () => {
      setShowOps(readNav("ox-nav:show-ops"));
      setShowDb(readNav("ox-nav:show-db"));
    };
    window.addEventListener("ox-nav-change", onNavChange);
    return () => window.removeEventListener("ox-nav-change", onNavChange);
  }, []);
  const cats = CATS.filter((c) =>
    c.key === "ops" ? showOps : c.key === "db" ? showDb : true,
  );
  // 正在查看的类别被隐藏时回落到「协同」，避免停在不可达的 tab 上。
  useEffect(() => {
    if (tab !== "settings" && !cats.some((c) => c.key === tab)) setTab("collab");
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [showOps, showDb]);
  // 每个工作区独立的运行方式（solo/team）与当前 session——多项目互不干扰。
  // 上次会话/运行方式按工作区记忆（localStorage）：刷新后回到原会话，
  // 而不是回落到固定 defaultSession（"main"）——那会显示另一个会话的历史，
  // 造成「刷新后和点击会话看到的不一样」。
  const [modes, setModes] = useState<Record<string, "solo" | "team">>(() => {
    try {
      return JSON.parse(localStorage.getItem("studio-modes") ?? "{}");
    } catch {
      return {};
    }
  });  const [sessionByWs, setSessionByWs] = useState<Record<string, string>>(() => {
    try {
      return JSON.parse(localStorage.getItem("studio-sessions") ?? "{}");
    } catch {
      return {};
    }
  });
  // 运行中的 "workspaceId:sessionId"（全局事件监听维护，左栏显示状态）。
  const [running, setRunning] = useState<Set<string>>(new Set());
  // 工作区表单（新建/编辑）。
  const [wsForm, setWsForm] = useState<{ kind: "new" | "edit"; initial?: WorkspaceConfig } | null>(null);
  // 主题：localStorage 优先，否则跟随系统；浅色为默认观感。
  const [theme, setTheme] = useState<"light" | "dark">(() => {
    const saved = localStorage.getItem("studio-theme");
    if (saved === "light" || saved === "dark") return saved;
    return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
  });

  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    localStorage.setItem("studio-theme", theme);
  }, [theme]);
  // 主题跟随系统：仅在用户从未手动切换过（localStorage 无记录）时生效，
  // 手动选择后尊重用户选择。
  useEffect(() => {
    const mq = window.matchMedia("(prefers-color-scheme: dark)");
    const onChange = (e: MediaQueryListEvent) => {
      if (!localStorage.getItem("studio-theme")) setTheme(e.matches ? "dark" : "light");
    };
    mq.addEventListener("change", onChange);
    return () => mq.removeEventListener("change", onChange);
  }, []);

  // 界面缩放（设置页可调，localStorage 记忆；默认「标准」110%——
  // 14px 基准在高分屏上偏小，取中档）。zoom 设在根元素 html 上：
  // 等价浏览器原生缩放，ICB 随缩放，100%/vh/fixed 自动自洽（见 styles.css）。
  useEffect(() => {
    const z = localStorage.getItem("studio-zoom") ?? "1.1";
    document.documentElement.style.zoom = z;
  }, []);

  const reload = useCallback(async () => {
    const [a, ws] = await Promise.all([api.listAgents(), api.listWorkspaces()]);
    setAgents(a);
    setWorkspaces(ws);
    // 档案列表给工作区表单做绑定选择；失败不阻塞（表单里显示为空）。
    api.listServers().then(setServers).catch(() => {});
    api.listDatabases().then(setDatabases).catch(() => {});
    // 刷新恢复：事件监听是全局的，丢的只是内存 running 集合——按后端
    // 运行态重建，运行中的 turn 无需重启。
    api
      .listRunningTurns()
      .then((runs) =>
        setRunning(new Set(runs.map((r) => `${r.workspace_id}:${r.session_id}`))),
      )
      .catch(() => {});
    setLoaded(true);
  }, []);

  useEffect(() => {
    void reload();
  }, [reload]);

  // 会话/运行方式记忆落 localStorage（任何变更即写回；解析失败静默降级为内存态）。
  useEffect(() => {
    try {
      localStorage.setItem("studio-modes", JSON.stringify(modes));
      localStorage.setItem("studio-sessions", JSON.stringify(sessionByWs));
    } catch {
      // 存储不可用（隐私模式等）：仅失去刷新恢复能力，不影响功能。
    }
  }, [modes, sessionByWs]);

  // 全局运行状态：任何事件到达即标记 (workspace, session) 在跑，turn 终态移除。
  // keep-alive 下切走的事件也到达这里，左栏状态始终准确。
  useEffect(() => {
    const mark = (wsId: string, sessionId: string) => {
      const key = `${wsId}:${sessionId}`;
      setRunning((prev) => (prev.has(key) ? prev : new Set(prev).add(key)));
    };
    const un1 = onAgentEvent((p) => mark(p.workspace_id, p.session_id));
    const un2 = onTurnStarted((p) => mark(p.workspace_id, p.session_id));
    const un3 = onTurnFinished((p) =>
      setRunning((prev) => {
        const key = `${p.workspace_id}:${p.session_id}`;
        if (!prev.has(key)) return prev;
        const next = new Set(prev);
        next.delete(key);
        return next;
      }),
    );
    return () => {
      un1.then((f) => f());
      un2.then((f) => f());
      un3.then((f) => f());
    };
  }, []);

  const dismissToast = (id: number) =>
    setToasts((prev) => prev.filter((t) => t.id !== id));

  const toast = (kind: "info" | "error", text: string) => {
    const id = Date.now() + Math.random();
    setToasts((prev) => [...prev.slice(-3), { id, kind, text }]);
    // 成功提示短驻；错误停留更久（可手动关）。
    setTimeout(() => dismissToast(id), kind === "error" ? 8000 : 2500);
  };
  const flash = (msg: string) => toast("info", msg);
  const fail = (msg: string) => toast("error", msg);

  /** 统一确认弹窗（替代 window.confirm）：resolve(true)=确认。 */
  const confirm = (title: string, message: string) =>
    new Promise<boolean>((resolve) => setConfirmState({ title, message, resolve }));

  // 确认弹窗的 Esc = 取消（桌面应用基本预期）。
  useEffect(() => {
    if (!confirmState) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        confirmState.resolve(false);
        setConfirmState(null);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [confirmState]);

  const cat = CATS.find((c) => c.key === tab);
  const inCat = cat ? workspaces.filter((w) => w.kind === cat.key) : [];
  // 选中工作区：手动选的（且属于当前类）优先，否则该类第一个。
  const activeWs =
    workspaces.find((w) => w.id === selectedId && w.kind === tab) ?? inCat[0];

  const openWorkspace = (wsId: string) => {
    setSelectedId(wsId);
    setModes((prev) => (prev[wsId] ? prev : { ...prev, [wsId]: "solo" }));
    setSessionByWs((prev) =>
      prev[wsId] ? prev : { ...prev, [wsId]: defaultSession(modes[wsId] ?? "solo") },
    );
  };

  const newSession = (wsId: string) => {
    const mode = modes[wsId] ?? "solo";
    const id = `${mode}-${crypto.randomUUID().replace(/-/g, "").slice(0, 12)}`;
    setSessionByWs((prev) => ({ ...prev, [wsId]: id }));
  };

  const removeWorkspace = async (w: WorkspaceConfig) => {
    const ok = await confirm(
      "删除工作区",
      `确定删除工作区「${w.name}」？\n会话数据保留，仅移除工作区配置。`,
    );
    if (!ok) return;
    try {
      await api.deleteWorkspace(w.id);
      flash("工作区已删除（会话数据保留）");
      await reload();
    } catch (e) {
      fail(String(e));
    }
  };

  /** 删除会话（SessionSidebar 调用）：确认 → 删除 → 反馈；删当前会话自动开新会话。 */
  const removeSession = async (wsId: string, sessionId: string) => {
    const ok = await confirm(
      "删除会话",
      `确定删除会话「${sessionId}」？\n对话历史将被清除，不可恢复。`,
    );
    if (!ok) return;
    try {
      await api.deleteSession(wsId, modes[wsId] ?? "solo", sessionId);
      flash("会话已删除");
      // 当前生效会话（显式选中或 default 回退）被删时才开新会话——
      // 只比 sessionByWs 会漏掉「尚未显式选中、正用着 default」的情形，
      // 删除后继续指向已删 id，下次发消息就把会话「复活」了。
      const current = sessionByWs[wsId] ?? defaultSession(modes[wsId] ?? "solo");
      if (current === sessionId) newSession(wsId);
    } catch (e) {
      fail(String(e));
    }
  };

  return (
    <div className="app">
      {/* 顶部导航条：品牌 + 类别导航 + 主题切换（原左侧竖栏上移，四栏变三栏） */}
      <header className="topbar">
        <span className="brand">Ox Agent Studio</span>
        <nav className="topbar-nav">
          {cats.map((c) => {
            // 该类别下是否有运行中的 turn（跨工作区聚合，切走也能看到）。
            const catRunning = workspaces.some(
              (w) => w.kind === c.key && [...running].some((k) => k.startsWith(`${w.id}:`)),
            );
            return (
              <button
                key={c.key}
                className={`nav-item ${tab === c.key ? "active" : ""}`}
                onClick={() => setTab(c.key)}
              >
                {c.label}
                <span className="mode-tag">
                  {workspaces.filter((w) => w.kind === c.key).length}
                </span>
                {catRunning && <span className="running-dot" title="有运行中的任务" />}
              </button>
            );
          })}
          <button
            className={`nav-item ${tab === "settings" ? "active" : ""}`}
            onClick={() => setTab("settings")}
          >
            智能体与模型
          </button>
        </nav>
        <span className="spacer" />
        <button
          className="nav-item"
          onClick={() => setTheme(theme === "dark" ? "light" : "dark")}
          title="切换深浅主题"
          aria-label="切换深浅主题"
        >
          {theme === "dark" ? "☀ 浅色" : "☾ 深色"}
        </button>
      </header>

      <main className="main">
        {tab === "settings" ? (
          <SettingsPage agents={agents} onChanged={() => void reload()} />
        ) : !loaded ? (
          <div className="app-loading">
            <div className="spinner" aria-hidden />
            <span>正在加载…</span>
          </div>
        ) : inCat.length === 0 ? (
          /* 无工作区：空态创建页（保留创建入口） */
          <WorkspacePage category={tab} onCreate={() => setWsForm({ kind: "new" })} />
        ) : (
          /* 有工作区：会话视图（左栏 + keep-alive 会话 + 文件面板在会话内） */
          <div className="workspace-layout">
            <SessionSidebar
              workspaces={inCat}
              selectedId={activeWs?.id ?? ""}
              onSelectWorkspace={openWorkspace}
              sessionByWs={sessionByWs}
              onSelectSession={(wsId, sid) =>
                setSessionByWs((prev) => ({ ...prev, [wsId]: sid }))
              }
              onNewSession={newSession}
              onDeleteSession={(wsId, sid) => void removeSession(wsId, sid)}
              running={running}
              onEditWorkspace={(w) => setWsForm({ kind: "edit", initial: w })}
              onDeleteWorkspace={(w) => void removeWorkspace(w)}
              onCreateWorkspace={() => setWsForm({ kind: "new" })}
            />
            {/* keep-alive：所有工作区会话常挂，非当前隐藏（事件持续归流）。 */}
            {workspaces.map((w) => {
              const agent = agents.find((a) => a.id === w.agent_id);
              if (!agent) return null; // 智能体被删除才卸载
              const mode = modes[w.id] ?? "solo";
              const active = activeWs?.id === w.id && w.kind === tab;
              return (
                <div key={w.id} className={`conv-keep ${active ? "" : "hidden"}`}>
                  <Conversation
                    workspace={w}
                    agent={agent}
                    runtimeMode={mode}
                    onToast={toast}
                    onConfirm={confirm}
                    sessionId={sessionByWs[w.id] ?? defaultSession(mode)}
                    onSessionChange={(sid) =>
                      setSessionByWs((prev) => ({ ...prev, [w.id]: sid }))
                    }
                    onModeChange={(m) => {
                      setModes((prev) => ({ ...prev, [w.id]: m }));
                      // 会话 id 带 mode 前缀（main/team-main/<mode>-<id>）：
                      // 切换后前缀不符就跳到该模式的默认会话，避免 solo 的
                      // 历史与运行态混进 team（反之亦然）。
                      const cur = sessionByWs[w.id] ?? defaultSession(m);
                      const belongs =
                        cur === "main" ? m === "solo" : cur.startsWith(m === "team" ? "team-" : "");
                      if (!belongs) {
                        setSessionByWs((prev) => ({ ...prev, [w.id]: defaultSession(m) }));
                      }
                    }}
                  />
                </div>
              );
            })}
          </div>
        )}
      </main>

      {/* 全局 toast 栈（右上角） */}
      {toasts.length > 0 && (
        <div className="toast-stack" role="status" aria-live="polite">
          {toasts.map((t) => (
            <div key={t.id} className={`toast ${t.kind}`}>
              <span className="toast-icon" aria-hidden>
                {t.kind === "error" ? "✕" : "✓"}
              </span>
              <span className="toast-text">{t.text}</span>
              <button className="toast-x" onClick={() => dismissToast(t.id)} aria-label="关闭">
                ×
              </button>
            </div>
          ))}
        </div>
      )}

      {/* 统一确认弹窗（Esc/遮罩关闭 = 取消） */}
      {confirmState && (
        <div
          className="modal-mask"
          onClick={() => {
            confirmState.resolve(false);
            setConfirmState(null);
          }}
        >
          <div
            className="modal confirm-modal"
            role="alertdialog"
            aria-modal="true"
            aria-label={confirmState.title}
            onClick={(e) => e.stopPropagation()}
          >
            <div className="confirm-title">{confirmState.title}</div>
            <div className="confirm-message">{confirmState.message}</div>
            <div className="modal-actions">
              <button
                className="btn ghost"
                onClick={() => {
                  confirmState.resolve(false);
                  setConfirmState(null);
                }}
              >
                取消
              </button>
              <button
                className="btn danger solid"
                autoFocus
                onClick={() => {
                  confirmState.resolve(true);
                  setConfirmState(null);
                }}
              >
                删除
              </button>
            </div>
          </div>
        </div>
      )}

      {wsForm && (
        <WorkspaceFormModal
          agents={agents}
          servers={servers}
          databases={databases}
          category={wsForm.initial?.kind ?? (cat?.key ?? "collab")}
          initial={wsForm.initial ?? null}
          onClose={() => setWsForm(null)}
          onSaved={(msg) => {
            const wasNew = wsForm.kind === "new";
            setWsForm(null);
            flash(msg);
            void reload().then(() => {
              // 新建后直接进入该工作区会话。
              if (wasNew) {
                void api.listWorkspaces().then((ws) => {
                  // 新建的工作区排最后（create 追加）；按 created_at 兜底取最新。
                  const latest = ws.reduce((a, b) => (b.created_at > a.created_at ? b : a), ws[0]);
                  if (latest) openWorkspace(latest.id);
                });
              }
            });
          }}
        />
      )}
    </div>
  );
}
