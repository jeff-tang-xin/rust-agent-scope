// 设置页：模型 Provider（≈ Environment）+ 智能体定义（≈ Managed Agent）的
// 创建与配置。智能体是纯定义（人设 + Provider）；工作区在左侧导航各分类页管理。
// 配置持久化到 app config dir（studio-config.json）。

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
import PasteButton from "../components/PasteButton";

function emptyProvider(): ModelProviderConfig {
  return { name: "", base_url: "https://api.openai.com/v1", api_key: "", model: "" };
}

function emptyDatabase(): DatabaseServerConfig {
  return {
    name: "",
    db_type: "mysql",
    host: "",
    port: 3306,
    database: "",
    username: "",
    password: "",
    description: "",
  };
}

function emptyServer(): ServerConfig {
  return { name: "", host: "", port: 22, username: "root", password: "", description: "" };
}

export default function SettingsPage({
  agents,
  onChanged,
}: {
  agents: AgentConfig[];
  onChanged: () => void;
}) {
  const [providers, setProviders] = useState<ModelProviderConfig[]>([]);
  const [providerDraft, setProviderDraft] = useState<ModelProviderConfig | null>(null);
  const [databases, setDatabases] = useState<DatabaseServerConfig[]>([]);
  const [dbDraft, setDbDraft] = useState<DatabaseServerConfig | null>(null);
  const [servers, setServers] = useState<ServerConfig[]>([]);
  const [serverDraft, setServerDraft] = useState<ServerConfig | null>(null);
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

  // ---------- providers ----------

  const saveProvider = async () => {
    if (!providerDraft) return;
    try {
      await api.saveProvider(providerDraft);
      setProviderDraft(null);
      await load();
      flash(`Provider ${providerDraft.name} saved`);
    } catch (e) {
      fail(e);
    }
  };

  const deleteProvider = async (name: string) => {
    try {
      await api.deleteProvider(name);
      await load();
      flash(`Provider ${name} deleted`);
    } catch (e) {
      fail(e);
    }
  };

  // ---------- databases ----------

  const saveDatabase = async () => {
    if (!dbDraft) return;
    try {
      await api.saveDatabase(dbDraft);
      setDbDraft(null);
      await load();
      flash(`数据库 ${dbDraft.name} 已保存`);
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

  const saveServer = async () => {
    if (!serverDraft) return;
    try {
      await api.saveServer(serverDraft);
      setServerDraft(null);
      await load();
      flash(`服务器 ${serverDraft.name} 已保存`);
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

  // ---------- agents ----------

  const deleteAgent = async (id: string) => {
    try {
      await api.deleteAgent(id);
      onChanged();
      flash("智能体已删除");
    } catch (e) {
      fail(e);
    }
  };

  // 当前编辑中的数据库类型描述符（字段启停/标签/驱动提示都按注册表元数据走）。
  const dbKind = dbDraft ? dbKinds.find((k) => k.id === dbDraft.db_type) : undefined;

  return (
    <>
      <header className="page-header">
        <h1>Settings</h1>
        <span className="sub">模型 Provider、数据库服务器与 Agent 定义（本地持久化）</span>
      </header>
      <div className="page-body">
        {error && <div className="notice error">{error}</div>}
        {notice && <div className="notice info">{notice}</div>}

        {/* ---------- 界面 ---------- */}
        <div className="card">
          <h2>界面</h2>
          <div className="desc">界面缩放——立即生效，本地记忆（默认「标准」）</div>
          <div style={{ display: "flex", gap: 8 }}>
            {([
              ["1", "紧凑 100%"],
              ["1.1", "标准 110%"],
              ["1.25", "宽松 125%"],
            ] as const).map(([z, label]) => {
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
        </div>

        {/* ---------- Providers ---------- */}
        <div className="card">
          <h2>Model Providers</h2>
          <div className="desc">OpenAI 兼容服务（OpenAI / DashScope 兼容模式 / DeepSeek / Ollama / vLLM…）</div>
          {providers.map((p) => (
            <div key={p.name} className="agent-list-item" style={{ cursor: "default" }}>
              <span className="name">{p.name}</span>
              <span className="desc">
                {p.base_url} · {p.model}
              </span>
              <span className="badge">{p.api_key ? "key set" : "no key"}</span>
              <button className="btn ghost" onClick={() => setProviderDraft(p)}>
                Edit
              </button>
              <button className="btn danger" onClick={() => void deleteProvider(p.name)}>
                Delete
              </button>
            </div>
          ))}
          {providerDraft ? (
            <div style={{ marginTop: 12 }}>
              <div className="row">
                <div>
                  <div className="lbl-row">
                    <label className="lbl">Name</label>
                    <PasteButton
                      onPaste={(t) => setProviderDraft({ ...providerDraft, name: t })}
                    />
                  </div>
                  <input
                    className="field"
                    value={providerDraft.name}
                    disabled={providers.some((p) => p.name === providerDraft.name)}
                    onChange={(e) => setProviderDraft({ ...providerDraft, name: e.target.value })}
                  />
                </div>
                <div>
                  <div className="lbl-row">
                    <label className="lbl">Model</label>
                    <PasteButton
                      onPaste={(t) => setProviderDraft({ ...providerDraft, model: t })}
                    />
                  </div>
                  <input
                    className="field"
                    value={providerDraft.model}
                    onChange={(e) => setProviderDraft({ ...providerDraft, model: e.target.value })}
                  />
                </div>
              </div>
              <div className="lbl-row">
                <label className="lbl">Base URL</label>
                <PasteButton
                  onPaste={(t) => setProviderDraft({ ...providerDraft, base_url: t })}
                />
              </div>
              <input
                className="field"
                value={providerDraft.base_url}
                onChange={(e) => setProviderDraft({ ...providerDraft, base_url: e.target.value })}
              />
              <div className="lbl-row">
                <label className="lbl">API Key（仅存本地配置文件）</label>
                <PasteButton
                  onPaste={(t) => setProviderDraft({ ...providerDraft, api_key: t })}
                />
              </div>
              <input
                className="field"
                type="password"
                value={providerDraft.api_key}
                onChange={(e) => setProviderDraft({ ...providerDraft, api_key: e.target.value })}
              />
              <div style={{ marginTop: 12, display: "flex", gap: 8 }}>
                <button className="btn" onClick={() => void saveProvider()} disabled={!providerDraft.name || !providerDraft.model}>
                  Save provider
                </button>
                <button className="btn ghost" onClick={() => setProviderDraft(null)}>
                  Cancel
                </button>
              </div>
            </div>
          ) : (
            <button className="btn ghost" style={{ marginTop: 8 }} onClick={() => setProviderDraft(emptyProvider())}>
              + Add provider
            </button>
          )}
        </div>

        {/* ---------- Databases ---------- */}
        <div className="card">
          <h2>Database Servers</h2>
          <div className="desc">
            数据库服务器连接档案（MySQL / PostgreSQL / SQLite / SQL Server），仅存本地配置文件
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
                <button className="btn ghost" onClick={() => setDbDraft(d)}>
                  Edit
                </button>
                <button className="btn danger" onClick={() => void deleteDatabase(d.name)}>
                  Delete
                </button>
              </div>
            );
          })}
          {dbDraft ? (
            <div style={{ marginTop: 12 }}>
              <div className="row">
                <div>
                  <div className="lbl-row">
                    <label className="lbl">Name</label>
                    <PasteButton onPaste={(t) => setDbDraft({ ...dbDraft, name: t })} />
                  </div>
                  <input
                    className="field"
                    value={dbDraft.name}
                    disabled={databases.some((d) => d.name === dbDraft.name)}
                    onChange={(e) => setDbDraft({ ...dbDraft, name: e.target.value })}
                  />
                </div>
                <div>
                  <label className="lbl">Type</label>
                  <select
                    className="field"
                    value={dbDraft.db_type}
                    onChange={(e) => {
                      const kind = dbKinds.find((k) => k.id === e.target.value);
                      setDbDraft({
                        ...dbDraft,
                        db_type: e.target.value,
                        // 切类型自动带出默认端口（无端口类型归 0）。
                        port: kind ? kind.default_port : dbDraft.port,
                      });
                    }}
                  >
                    {dbKinds.map((k) => (
                      <option key={k.id} value={k.id}>
                        {k.label}
                      </option>
                    ))}
                  </select>
                  {(() => {
                    const kind = dbKinds.find((k) => k.id === dbDraft.db_type);
                    if (!kind) return null;
                    return (
                      <div className="desc" style={{ margin: "4px 0 0" }}>
                        {kind.driver.mode === "Bundled"
                          ? "驱动：内置（rusqlite，无外部依赖）"
                          : `驱动：外部客户端 ${kind.driver.program}（需已在 PATH，应用不内置驱动）`}
                      </div>
                    );
                  })()}
                </div>
              </div>
              <div className="row">
                <div>
                  <div className="lbl-row">
                    <label className="lbl">
                      {dbKind && !dbKind.needs_port ? "数据库文件路径" : "Host"}
                    </label>
                    <PasteButton onPaste={(t) => setDbDraft({ ...dbDraft, host: t })} />
                  </div>
                  <input
                    className="field"
                    value={dbDraft.host}
                    placeholder={dbKind && !dbKind.needs_port ? "C:\\data\\app.db" : "10.0.0.8"}
                    onChange={(e) => setDbDraft({ ...dbDraft, host: e.target.value })}
                  />
                </div>
                <div>
                  <label className="lbl">Port</label>
                  <input
                    className="field"
                    type="number"
                    min={0}
                    max={65535}
                    value={dbDraft.port}
                    disabled={!dbKind?.needs_port}
                    onChange={(e) =>
                      setDbDraft({ ...dbDraft, port: Number(e.target.value) || 0 })
                    }
                  />
                </div>
              </div>
              <div className="row">
                <div>
                  <div className="lbl-row">
                    <label className="lbl">Database</label>
                    <PasteButton onPaste={(t) => setDbDraft({ ...dbDraft, database: t })} />
                  </div>
                  <input
                    className="field"
                    value={dbDraft.database}
                    disabled={!dbKind?.needs_database}
                    onChange={(e) => setDbDraft({ ...dbDraft, database: e.target.value })}
                  />
                </div>
                <div>
                  <div className="lbl-row">
                    <label className="lbl">Username</label>
                    <PasteButton onPaste={(t) => setDbDraft({ ...dbDraft, username: t })} />
                  </div>
                  <input
                    className="field"
                    value={dbDraft.username}
                    onChange={(e) => setDbDraft({ ...dbDraft, username: e.target.value })}
                  />
                </div>
              </div>
              <div className="lbl-row">
                <label className="lbl">Password（仅存本地配置文件）</label>
                <PasteButton onPaste={(t) => setDbDraft({ ...dbDraft, password: t })} />
              </div>
              <input
                className="field"
                type="password"
                value={dbDraft.password}
                onChange={(e) => setDbDraft({ ...dbDraft, password: e.target.value })}
              />
              <div className="lbl-row">
                <label className="lbl">Description</label>
                <PasteButton onPaste={(t) => setDbDraft({ ...dbDraft, description: t })} />
              </div>
              <input
                className="field"
                value={dbDraft.description}
                onChange={(e) => setDbDraft({ ...dbDraft, description: e.target.value })}
              />
              <div style={{ marginTop: 12, display: "flex", gap: 8 }}>
                <button
                  className="btn"
                  onClick={() => void saveDatabase()}
                  disabled={
                    !dbDraft.name ||
                    (dbDraft.db_type !== "sqlite" && !dbDraft.host.trim())
                  }
                >
                  Save database
                </button>
                <button className="btn ghost" onClick={() => setDbDraft(null)}>
                  Cancel
                </button>
              </div>
            </div>
          ) : (
            <button
              className="btn ghost"
              style={{ marginTop: 8 }}
              onClick={() => setDbDraft(emptyDatabase())}
            >
              + Add database
            </button>
          )}
        </div>

        {/* ---------- SSH Servers ---------- */}
        <div className="card">
          <h2>SSH Servers</h2>
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
              <span className="badge">{s.password ? "密码已设" : "无密码"}</span>
              <button className="btn ghost" onClick={() => setServerDraft(s)}>
                Edit
              </button>
              <button className="btn danger" onClick={() => void deleteServer(s.name)}>
                Delete
              </button>
            </div>
          ))}
          {serverDraft ? (
            <div style={{ marginTop: 12 }}>
              <div className="row">
                <div>
                  <div className="lbl-row">
                    <label className="lbl">Name</label>
                    <PasteButton onPaste={(t) => setServerDraft({ ...serverDraft, name: t })} />
                  </div>
                  <input
                    className="field"
                    value={serverDraft.name}
                    disabled={servers.some((s) => s.name === serverDraft.name)}
                    onChange={(e) => setServerDraft({ ...serverDraft, name: e.target.value })}
                  />
                </div>
                <div>
                  <div className="lbl-row">
                    <label className="lbl">Host</label>
                    <PasteButton onPaste={(t) => setServerDraft({ ...serverDraft, host: t })} />
                  </div>
                  <input
                    className="field"
                    value={serverDraft.host}
                    placeholder="10.0.0.8"
                    onChange={(e) => setServerDraft({ ...serverDraft, host: e.target.value })}
                  />
                </div>
                <div>
                  <label className="lbl">Port</label>
                  <input
                    className="field"
                    type="number"
                    min={0}
                    max={65535}
                    value={serverDraft.port}
                    onChange={(e) =>
                      setServerDraft({ ...serverDraft, port: Number(e.target.value) || 22 })
                    }
                  />
                </div>
              </div>
              <div className="row">
                <div>
                  <div className="lbl-row">
                    <label className="lbl">Username</label>
                    <PasteButton onPaste={(t) => setServerDraft({ ...serverDraft, username: t })} />
                  </div>
                  <input
                    className="field"
                    value={serverDraft.username}
                    onChange={(e) => setServerDraft({ ...serverDraft, username: e.target.value })}
                  />
                </div>
                <div>
                  <div className="lbl-row">
                    <label className="lbl">Password（留空走本机 ssh agent/默认密钥）</label>
                    <PasteButton onPaste={(t) => setServerDraft({ ...serverDraft, password: t })} />
                  </div>
                  <input
                    className="field"
                    type="password"
                    value={serverDraft.password}
                    onChange={(e) => setServerDraft({ ...serverDraft, password: e.target.value })}
                  />
                </div>
              </div>
              <div className="lbl-row">
                <label className="lbl">Description</label>
                <PasteButton onPaste={(t) => setServerDraft({ ...serverDraft, description: t })} />
              </div>
              <input
                className="field"
                value={serverDraft.description}
                onChange={(e) => setServerDraft({ ...serverDraft, description: e.target.value })}
              />
              <div style={{ marginTop: 12, display: "flex", gap: 8 }}>
                <button
                  className="btn"
                  onClick={() => void saveServer()}
                  disabled={!serverDraft.name || !serverDraft.host.trim()}
                >
                  Save server
                </button>
                <button className="btn ghost" onClick={() => setServerDraft(null)}>
                  Cancel
                </button>
              </div>
            </div>
          ) : (
            <button
              className="btn ghost"
              style={{ marginTop: 8 }}
              onClick={() => setServerDraft(emptyServer())}
            >
              + Add server
            </button>
          )}
        </div>

        {/* ---------- Agents ---------- */}
        <div className="card">
          <h2>Agents</h2>
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
                Edit
              </button>
              {a.id !== DEFAULT_AGENT_ID && (
                <button className="btn danger" onClick={() => void deleteAgent(a.id)}>
                  Delete
                </button>
              )}
            </div>
          ))}
          <button
            className="btn ghost"
            style={{ marginTop: 8 }}
            onClick={() => setAgentForm({ initial: null })}
          >
            + Create agent
          </button>
        </div>
      </div>

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
