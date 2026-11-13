// 设置页：模型 Provider（≈ Environment）+ 智能体定义（≈ Managed Agent）的
// 创建与配置。智能体是纯定义（人设 + Provider）；工作区在左侧导航各分类页管理。
// 配置持久化到 app config dir（studio-config.json）。
//
// 布局：顶部 tab 分区（通用 / 模型服务 / 数据库 / SSH 服务器 / 智能体），
// 一次只看一类；编辑一律走弹窗（对齐 AgentFormModal），列表页只做浏览与增删入口。

import { useEffect, useState } from "react";
import {
  api,
  DEFAULT_AGENT_ID,
  type AgentConfig,
  type DatabaseKindInfo,
  type DatabaseServerConfig,
  type ModelProviderConfig,
  type ServerConfig,
} from "../api";
import AgentFormModal from "../components/AgentFormModal";
import DatabaseFormModal from "../components/DatabaseFormModal";
import ProviderFormModal from "../components/ProviderFormModal";
import ServerFormModal from "../components/ServerFormModal";

type SettingsTab = "general" | "providers" | "databases" | "servers" | "agents";

const TABS: { id: SettingsTab; label: string }[] = [
  { id: "general", label: "通用" },
  { id: "providers", label: "模型服务" },
  { id: "databases", label: "数据库" },
  { id: "servers", label: "SSH 服务器" },
  { id: "agents", label: "智能体" },
];

export default function SettingsPage({
  agents,
  onChanged,
}: {
  agents: AgentConfig[];
  onChanged: () => void;
}) {
  const [tab, setTab] = useState<SettingsTab>("providers");
  const [providers, setProviders] = useState<ModelProviderConfig[]>([]);
  // 编辑中的表单：null = 关闭；{ initial: null } = 新建（对齐 agentForm 形态）。
  const [providerForm, setProviderForm] = useState<{ initial: ModelProviderConfig | null } | null>(
    null,
  );
  const [databases, setDatabases] = useState<DatabaseServerConfig[]>([]);
  const [dbForm, setDbForm] = useState<{ initial: DatabaseServerConfig | null } | null>(null);
  const [servers, setServers] = useState<ServerConfig[]>([]);
  const [serverForm, setServerForm] = useState<{ initial: ServerConfig | null } | null>(null);
  // 数据库类型注册表（后端 DATABASE_KINDS 投影；新增类型自动出现在下拉里）。
  const [dbKinds, setDbKinds] = useState<DatabaseKindInfo[]>([]);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  // agent 表单模态：null = 关闭；initial 为 null 表示新建。
  const [agentForm, setAgentForm] = useState<{ initial: AgentConfig | null } | null>(null);

  const load = async () => {
    setProviders(await api.listProviders());
    setDatabases(await api.listDatabases());
    setDbKinds(await api.listDatabaseKinds());
    setServers(await api.listServers());
  };
  useEffect(() => {
    void load();
  }, []);

  const flash = (msg: string) => {
    setNotice(msg);
    setError("");
    setTimeout(() => setNotice(""), 2500);
  };

  const fail = (e: unknown) => setError(String(e));

  const deleteProvider = async (name: string) => {
    try {
      await api.deleteProvider(name);
      await load();
      flash(`模型服务 ${name} 已删除`);
    } catch (e) {
      fail(e);
    }
  };

  const deleteDatabase = async (name: string) => {
    try {
      await api.deleteDatabase(name);
      await load();
      flash(`数据库 ${name} 已删除`);
    } catch (e) {
      fail(e);
    }
  };

  const deleteServer = async (name: string) => {
    try {
      await api.deleteServer(name);
      await load();
      flash(`服务器 ${name} 已删除`);
    } catch (e) {
      fail(e);
    }
  };

  const deleteAgent = async (id: string) => {
    try {
      await api.deleteAgent(id);
      onChanged();
      flash("智能体已删除");
    } catch (e) {
      fail(e);
    }
  };

  // 导航显隐开关的重渲染触发器（值本身在 localStorage，UI 只需刷新）。
  const [navTick, setNavTick] = useState(0);
  // 桌面悬浮球开关（后端配置持久化；启动时后端按它决定是否显示）。
  const [ballDisabled, setBallDisabled] = useState<boolean>(false);
  useEffect(() => {
    void api
      .ballGetDisabled()
      .then(setBallDisabled)
      .catch(() => {});
  }, []);

  return (
    <>
      <header className="page-header">
        <h1>Settings</h1>
        <span className="sub">模型服务、数据库服务器与智能体定义（本地持久化）</span>
      </header>
      <div className="page-body">
        <div className="mode-switch" style={{ marginBottom: 14 }}>
          {TABS.map((t) => (
            <button
              key={t.id}
              className={tab === t.id ? "active" : ""}
              onClick={() => {
                setTab(t.id);
                setError("");
              }}
            >
              {t.label}
            </button>
          ))}
        </div>
        {error && <div className="notice error">{error}</div>}
        {notice && <div className="notice info">{notice}</div>}

        {tab === "general" && (
          <div className="card">
            <h2>界面</h2>
            <div className="desc">界面缩放——立即生效，本地记忆（默认「标准」）</div>
            <div style={{ display: "flex", gap: 8 }}>
              {(
                [
                  ["1", "紧凑 100%"],
                  ["1.1", "标准 110%"],
                  ["1.25", "宽松 125%"],
                ] as const
              ).map(([z, label]) => {
                const current = localStorage.getItem("studio-zoom") ?? "1.1";
                return (
                  <button
                    key={z}
                    className={`btn ${current === z ? "" : "ghost"}`}
                    onClick={() => {
                      localStorage.setItem("studio-zoom", z);
                      document.body.style.zoom = z;
                      document.documentElement.style.setProperty("--app-zoom", z);
                    }}
                  >
                    {label}
                  </button>
                );
              })}
            </div>
            <div className="desc" style={{ marginTop: 12 }}>
              导航显示——默认关闭，勾选后顶部导航才显示对应菜单（本地记忆，立即生效）
            </div>
            <div key={navTick} style={{ display: "flex", gap: 16 }}>
              {(
                [
                  ["ox-nav:show-ops", "运维菜单"],
                  ["ox-nav:show-db", "数据库运维菜单"],
                  ["ox-nav:show-test", "测试菜单"],
                ] as const
              ).map(([key, label]) => (
                <label
                  key={key}
                  style={{ display: "flex", alignItems: "center", gap: 6, cursor: "pointer" }}
                >
                  <input
                    type="checkbox"
                    checked={localStorage.getItem(key) === "1"}
                    onChange={(e) => {
                      localStorage.setItem(key, e.target.checked ? "1" : "0");
                      setNavTick((t) => t + 1);
                      window.dispatchEvent(new CustomEvent("ox-nav-change"));
                    }}
                  />
                  {label}
                </label>
              ))}
            </div>
            <div className="desc" style={{ marginTop: 12 }}>
              桌面悬浮球——启动时在屏幕右上角显示可拖拽的小机器人（右键菜单可
              打开主窗/切换外观/停用；停用跨启动持久）
            </div>
            <div style={{ display: "flex", gap: 16 }}>
              <label style={{ display: "flex", alignItems: "center", gap: 6, cursor: "pointer" }}>
                <input
                  type="checkbox"
                  checked={!ballDisabled}
                  onChange={(e) => {
                    const disabled = !e.target.checked;
                    setBallDisabled(disabled);
                    void api.ballSetDisabled(disabled).catch(() => setBallDisabled(!disabled));
                  }}
                />
                显示悬浮球
              </label>
            </div>
            <div className="desc" style={{ marginTop: 12 }}>
              运行日志——后端运行记录（turn 启停/中断/清空/panic），排查问题用；
              文件在 ~/.ox/logs/studio.log，超 5MB 自动轮转
            </div>
            <div style={{ display: "flex", gap: 8 }}>
              <button
                className="btn ghost"
                onClick={() => void api.openLogDir().catch((e) => alert(String(e)))}
              >
                打开日志目录
              </button>
            </div>
          </div>
        )}

        {tab === "providers" && (
          <div className="card">
            <h2>模型服务</h2>
            <div className="desc">
              模型服务连接档案（OpenAI 兼容 / OpenAI Responses / Anthropic / Gemini / DashScope /
              Ollama），智能体在此绑定
            </div>
            {providers.map((p) => (
              <div key={p.name} className="agent-list-item" style={{ cursor: "default" }}>
                <span className="name">{p.name}</span>
                <span className="badge">{p.provider_type || "openai"}</span>
                <span className="desc">
                  {p.base_url} · {p.model}
                </span>
                <span className="badge">{p.api_key ? "key 已设" : "无 key"}</span>
                <button className="btn ghost" onClick={() => setProviderForm({ initial: p })}>
                  编辑
                </button>
                <button className="btn danger" onClick={() => void deleteProvider(p.name)}>
                  删除
                </button>
              </div>
            ))}
            <button
              className="btn ghost"
              style={{ marginTop: 8 }}
              onClick={() => setProviderForm({ initial: null })}
            >
              + 新建模型服务
            </button>
          </div>
        )}

        {tab === "databases" && (
          <div className="card">
            <h2>数据库服务器</h2>
            <div className="desc">
              数据库连接档案（MySQL / PostgreSQL / SQLite / SQL Server），仅存本地配置文件
            </div>
            {databases.map((d) => {
              const k = dbKinds.find((x) => x.id === d.db_type);
              const endpoint =
                (k?.needs_port ? `${d.host}:${d.port}` : d.host) +
                (k?.needs_database ? `/${d.database}` : "");
              return (
                <div key={d.name} className="agent-list-item" style={{ cursor: "default" }}>
                  <span className="name">{d.name}</span>
                  <span className="badge">{k?.label ?? d.db_type}</span>
                  <span className="desc">
                    {endpoint}
                    {d.username ? ` · ${d.username}` : ""}
                    {d.description ? ` · ${d.description}` : ""}
                  </span>
                  <span className="badge">{d.password ? "密码已设" : "无密码"}</span>
                  {k?.driver.mode === "Cli" && d.cli_path.trim() && (
                    <span className="badge" title={d.cli_path}>
                      CLI 已指定
                    </span>
                  )}
                  <button className="btn ghost" onClick={() => setDbForm({ initial: d })}>
                    编辑
                  </button>
                  <button className="btn danger" onClick={() => void deleteDatabase(d.name)}>
                    删除
                  </button>
                </div>
              );
            })}
            <button
              className="btn ghost"
              style={{ marginTop: 8 }}
              onClick={() => setDbForm({ initial: null })}
            >
              + 新建数据库连接
            </button>
          </div>
        )}

        {tab === "servers" && (
          <div className="card">
            <h2>SSH 服务器</h2>
            <div className="desc">
              服务器连接档案（ops 工作区的 remote_shell / 文件操作按名字引用），仅存本地配置文件
            </div>
            {servers.map((s) => (
              <div key={s.name} className="agent-list-item" style={{ cursor: "default" }}>
                <span className="name">{s.name}</span>
                <span className="badge">
                  {s.username}@{s.host}:{s.port}
                </span>
                <span className="desc">{s.description}</span>
                <span className="badge">{s.password ? "密码已设" : "密钥登录"}</span>
                <button className="btn ghost" onClick={() => setServerForm({ initial: s })}>
                  编辑
                </button>
                <button className="btn danger" onClick={() => void deleteServer(s.name)}>
                  删除
                </button>
              </div>
            ))}
            <button
              className="btn ghost"
              style={{ marginTop: 8 }}
              onClick={() => setServerForm({ initial: null })}
            >
              + 新建服务器连接
            </button>
          </div>
        )}

        {tab === "agents" && (
          <div className="card">
            <h2>智能体</h2>
            <div className="desc">
              智能体 = 纯定义（人设 + Provider），在此创建与维护；
              工作区（协同 / 运维 / 数据库运维）选用智能体并决定目录、成员与工具面
            </div>
            {agents.map((a) => (
              <div key={a.id} className="agent-list-item" style={{ cursor: "default" }}>
                <span className="name">{a.name}</span>
                {a.id === DEFAULT_AGENT_ID && <span className="badge ok">内置</span>}
                <span className="desc">{a.description || a.sys_prompt.slice(0, 60)}</span>
                <span className="badge">{a.provider || "未绑定 Provider"}</span>
                <button className="btn ghost" onClick={() => setAgentForm({ initial: a })}>
                  编辑
                </button>
                {a.id !== DEFAULT_AGENT_ID && (
                  <button className="btn danger" onClick={() => void deleteAgent(a.id)}>
                    删除
                  </button>
                )}
              </div>
            ))}
            <button
              className="btn ghost"
              style={{ marginTop: 8 }}
              onClick={() => setAgentForm({ initial: null })}
            >
              + 新建智能体
            </button>
          </div>
        )}
      </div>

      {providerForm && (
        <ProviderFormModal
          initial={providerForm.initial}
          existingNames={providers.map((p) => p.name)}
          onClose={() => setProviderForm(null)}
          onSaved={(msg) => {
            setProviderForm(null);
            flash(msg);
            void load();
          }}
        />
      )}
      {dbForm && (
        <DatabaseFormModal
          initial={dbForm.initial}
          dbKinds={dbKinds}
          existingNames={databases.map((d) => d.name)}
          onClose={() => setDbForm(null)}
          onSaved={(msg) => {
            setDbForm(null);
            flash(msg);
            void load();
          }}
        />
      )}
      {serverForm && (
        <ServerFormModal
          initial={serverForm.initial}
          existingNames={servers.map((s) => s.name)}
          onClose={() => setServerForm(null)}
          onSaved={(msg) => {
            setServerForm(null);
            flash(msg);
            void load();
          }}
        />
      )}
      {agentForm && (
        <AgentFormModal
          initial={agentForm.initial}
          providers={providers}
          onClose={() => setAgentForm(null)}
          onSaved={(msg) => {
            setAgentForm(null);
            flash(msg);
            onChanged();
          }}
        />
      )}
    </>
  );
}
