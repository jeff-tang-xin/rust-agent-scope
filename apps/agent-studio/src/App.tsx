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
  const [loaded, setLoaded] = useState(false);
  const [notice, setNotice] = useState("");
  const [error, setError] = useState("");
  // 当前选中的工作区（切类别时自动落到该类第一个）。
  const [selectedId, setSelectedId] = useState("");
  // 每个工作区独立的运行方式（solo/team）与当前 session——多项目互不干扰。
  const [modes, setModes] = useState<Record<string, "solo" | "team">>({});
  const [sessionByWs, setSessionByWs] = useState<Record<string, string>>({});
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

  // 界面缩放（设置页可调，localStorage 记忆；默认「标准」110%——
  // 14px 基准在高分屏上偏小，取中档）。
  useEffect(() => {
    document.body.style.zoom = localStorage.getItem("studio-zoom") ?? "1.1";
  }, []);

  const reload = useCallback(async () => {
    const [a, ws] = await Promise.all([api.listAgents(), api.listWorkspaces()]);
    setAgents(a);
    setWorkspaces(ws);
    setLoaded(true);
  }, []);

  useEffect(() => {
    void reload();
  }, [reload]);

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

  const flash = (msg: string) => {
    setNotice(msg);
    setError("");
    setTimeout(() => setNotice(""), 2500);
  };
  const fail = (msg: string) => setError(msg);

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
    if (!window.confirm(`删除工作区「${w.name}」？会话数据保留，仅移除工作区配置。`)) return;
    try {
      await api.deleteWorkspace(w.id);
      flash("工作区已删除（会话数据保留）");
      await reload();
    } catch (e) {
      fail(String(e));
    }
  };

  return (
    <div className="app">
      {/* 顶部导航条：品牌 + 类别导航 + 主题切换（原左侧竖栏上移，四栏变三栏） */}
      <header className="topbar">
        <span className="brand">AgentScope Studio</span>
        <nav className="topbar-nav">
          {CATS.map((c) => (
            <button
              key={c.key}
              className={`nav-item ${tab === c.key ? "active" : ""}`}
              onClick={() => setTab(c.key)}
            >
              {c.label}
              <span className="mode-tag">
                {workspaces.filter((w) => w.kind === c.key).length}
              </span>
            </button>
          ))}
          <button
            className={`nav-item ${tab === "settings" ? "active" : ""}`}
            onClick={() => setTab("settings")}
          >
            Agents &amp; Providers
          </button>
        </nav>
        <span className="spacer" />
        <button
          className="nav-item"
          onClick={() => setTheme(theme === "dark" ? "light" : "dark")}
          title="Toggle color theme"
        >
          {theme === "dark" ? "☀  Light mode" : "☾  Dark mode"}
        </button>
      </header>

      <main className="main">
        {error && (
          <div className="notice error" style={{ margin: "10px 20px 0" }}>
            {error}
          </div>
        )}
        {notice && (
          <div className="notice info" style={{ margin: "10px 20px 0" }}>
            {notice}
          </div>
        )}
        {tab === "settings" ? (
          <SettingsPage agents={agents} onChanged={() => void reload()} />
        ) : !loaded ? null : inCat.length === 0 ? (
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
                    sessionId={sessionByWs[w.id] ?? defaultSession(mode)}
                    onSessionChange={(sid) =>
                      setSessionByWs((prev) => ({ ...prev, [w.id]: sid }))
                    }
                    onModeChange={(m) => setModes((prev) => ({ ...prev, [w.id]: m }))}
                  />
                </div>
              );
            })}
          </div>
        )}
      </main>

      {wsForm && (
        <WorkspaceFormModal
          agents={agents}
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
