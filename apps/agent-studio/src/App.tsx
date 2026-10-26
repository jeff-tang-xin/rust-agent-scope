// 应用骨架：左侧导航（协同 / 运维 / 数据库运维 + 设置）+ 主区。
// 工作区三大类对齐产品概念：协同（solo/team 可切换，plan/free 可切换）、
// 运维（shell/web）、数据库运维（只读 SQL）——都是工作区，针对不同的项目。
// 每类是工作区卡片列表页，点「进入」进入对话视图。
//
// 会话 keep-alive：打开过的工作区会话不随切换卸载（仅隐藏）——事件监听
// 持续归流，多项目并行时切走再切回，流式内容/工具进度/busy 状态无缝续接。

import { useCallback, useEffect, useState } from "react";
import { api, type AgentConfig, type WorkspaceConfig, type WorkspaceKind } from "./api";
import Conversation from "./components/Conversation";
import WorkspacePage, { type Category } from "./pages/WorkspacePage";
import SettingsPage from "./pages/SettingsPage";
import "./styles.css";

type Tab = Category | "settings";

const CATS: { key: WorkspaceKind; label: string }[] = [
  { key: "collab", label: "协同" },
  { key: "ops", label: "运维" },
  { key: "db", label: "数据库运维" },
];

export default function App() {
  const [tab, setTab] = useState<Tab>("collab");
  const [agents, setAgents] = useState<AgentConfig[]>([]);
  const [workspaces, setWorkspaces] = useState<WorkspaceConfig[]>([]);
  const [selectedId, setSelectedId] = useState("");
  const [loaded, setLoaded] = useState(false);
  const [notice, setNotice] = useState("");
  const [error, setError] = useState("");
  // 已打开会话的工作区（keep-alive：切走仅隐藏，不卸载）。
  const [opened, setOpened] = useState<string[]>([]);
  // 每个工作区独立的运行方式（solo/team）——多项目互不干扰。
  const [modes, setModes] = useState<Record<string, "solo" | "team">>({});
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

  const reload = useCallback(async () => {
    const [a, ws] = await Promise.all([api.listAgents(), api.listWorkspaces()]);
    setAgents(a);
    setWorkspaces(ws);
    // 已删除的工作区同步移出 keep-alive 列表。
    setOpened((prev) => prev.filter((id) => ws.some((w) => w.id === id)));
    setLoaded(true);
  }, []);

  useEffect(() => {
    void reload();
  }, [reload]);

  const flash = (msg: string) => {
    setNotice(msg);
    setError("");
    setTimeout(() => setNotice(""), 2500);
  };
  const fail = (msg: string) => setError(msg);

  const cat = CATS.find((c) => c.key === tab);
  const selected = workspaces.find((w) => w.id === selectedId);
  // 对话视图仅在选中工作区属于当前类时展示；否则显示列表页。
  const inCat = cat ? workspaces.filter((w) => w.kind === cat.key) : [];
  const showConversation = Boolean(cat && selected && inCat.some((w) => w.id === selected.id));

  const openWorkspace = (w: WorkspaceConfig) => {
    setOpened((prev) => (prev.includes(w.id) ? prev : [...prev, w.id]));
    setModes((prev) => (prev[w.id] ? prev : { ...prev, [w.id]: "solo" }));
    setSelectedId(w.id);
  };

  return (
    <div className="app">
      <aside className="sidebar">
        <div className="brand">AgentScope Studio</div>
        <div className="nav-section">工作区</div>
        {CATS.map((c) => (
          <button
            key={c.key}
            className={`nav-item ${tab === c.key ? "active" : ""}`}
            onClick={() => {
              setTab(c.key);
              setSelectedId("");
            }}
          >
            {c.label}
            <span className="mode-tag">
              {workspaces.filter((w) => w.kind === c.key).length}
            </span>
          </button>
        ))}
        <div className="nav-section">Settings</div>
        <button
          className={`nav-item ${tab === "settings" ? "active" : ""}`}
          onClick={() => setTab("settings")}
        >
          Agents & Providers
        </button>
        <div className="sidebar-footer">
          <button
            className="nav-item"
            onClick={() => setTheme(theme === "dark" ? "light" : "dark")}
            title="Toggle color theme"
          >
            {theme === "dark" ? "☀  Light mode" : "☾  Dark mode"}
          </button>
        </div>
      </aside>

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
        ) : (
          <>
            {/* keep-alive 会话：打开过的都挂着，非当前的隐藏（事件持续归流）。 */}
            {opened.map((id) => {
              const w = workspaces.find((x) => x.id === id);
              const agent = w ? agents.find((a) => a.id === w.agent_id) : undefined;
              if (!w || !agent) return null; // 工作区或智能体被删除才真正卸载
              const active = showConversation && selected?.id === id;
              const mode = modes[id] ?? "solo";
              return (
                <div key={`${id}:${mode}`} className={`conv-keep ${active ? "" : "hidden"}`}>
                  <Conversation
                    workspace={w}
                    agent={agent}
                    runtimeMode={mode}
                    onModeChange={(m) => setModes((prev) => ({ ...prev, [id]: m }))}
                    onBack={() => setSelectedId("")}
                  />
                </div>
              );
            })}
            {!showConversation && (
              <WorkspacePage
                category={tab}
                agents={loaded ? agents : []}
                workspaces={loaded ? workspaces : []}
                onOpen={openWorkspace}
                onChanged={() => void reload()}
                onNotice={flash}
                onError={fail}
              />
            )}
          </>
        )}
      </main>
    </div>
  );
}
