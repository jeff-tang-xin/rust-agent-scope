// 应用骨架：左侧导航（协同 / 运维 / 数据库运维 + 设置）+ 主区。
// 工作区三大类对齐产品概念：协同（solo/team 可切换，plan/free 可切换）、
// 运维（shell/web）、数据库运维（只读 SQL）——都是工作区，针对不同的项目。
// 每类是工作区卡片列表页，点「进入」进入对话视图。

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
  // 协同工作区的运行方式（solo/team）：进入对话时选择，切换即重开会话。
  const [runtimeMode, setRuntimeMode] = useState<"solo" | "team">("solo");
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
  // 会话用的智能体 = 工作区选定的智能体。
  const selectedAgent = selected
    ? agents.find((a) => a.id === selected.agent_id)
    : undefined;

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
        ) : showConversation && selected && selectedAgent ? (
          <Conversation
            key={`${selected.id}:${runtimeMode}`}
            workspace={selected}
            agent={selectedAgent}
            runtimeMode={runtimeMode}
            onModeChange={setRuntimeMode}
            onBack={() => setSelectedId("")}
          />
        ) : (
          <WorkspacePage
            category={tab}
            agents={loaded ? agents : []}
            workspaces={loaded ? workspaces : []}
            onOpen={(w) => {
              setRuntimeMode("solo");
              setSelectedId(w.id);
            }}
            onChanged={() => void reload()}
            onNotice={flash}
            onError={fail}
          />
        )}
      </main>
    </div>
  );
}
