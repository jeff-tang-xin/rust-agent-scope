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

/** Provider 协议类型注册表：值与后端 build_model 的分发键一致。
 * models：常用模型建议（datalist，仍可自由输入）；keyHint：Key 获取地址；
 * desc：协议适用场景一行说明。 */
const PROVIDER_TYPES: {
  value: string;
  label: string;
  baseUrl: string;
  keyless?: boolean;
  models: string[];
  keyHint?: string;
  desc: string;
}[] = [
  {
    value: "openai",
    label: "OpenAI 兼容（DeepSeek / vLLM / DashScope 兼容模式…）",
    baseUrl: "https://api.openai.com/v1",
    models: ["deepseek-chat", "deepseek-reasoner", "qwen-plus", "glm-4-plus", "kimi-k2-instruct"],
    keyHint: "platform.openai.com/api-keys（第三方网关在其各自控制台获取）",
    desc: "任何实现 /chat/completions 的服务：DeepSeek、vLLM、OneAPI/NewAPI 网关、DashScope 兼容模式等。",
  },
  {
    value: "openai-official",
    label: "OpenAI 官方（Responses API）",
    baseUrl: "https://api.openai.com",
    models: ["gpt-4o", "gpt-4o-mini", "gpt-4.1", "o4-mini"],
    keyHint: "platform.openai.com/api-keys",
    desc: "OpenAI 官方 Responses API（/v1/responses），与「OpenAI 兼容」的区别是走官方新协议。",
  },
  {
    value: "anthropic",
    label: "Anthropic（Claude）",
    baseUrl: "https://api.anthropic.com",
    models: ["claude-sonnet-4-5", "claude-opus-4-1", "claude-haiku-4-5"],
    keyHint: "console.anthropic.com/settings/keys",
    desc: "Claude 官方 Messages API；需要能直连 api.anthropic.com 的网络。",
  },
  {
    value: "gemini",
    label: "Google Gemini",
    baseUrl: "https://generativelanguage.googleapis.com",
    models: ["gemini-2.5-pro", "gemini-2.5-flash", "gemini-2.0-flash"],
    keyHint: "aistudio.google.com/apikey",
    desc: "Google AI Studio 的 generateContent 协议；需要能直连 Google 服务的网络。",
  },
  {
    value: "dashscope",
    label: "DashScope 原生（通义千问）",
    baseUrl: "https://dashscope.aliyuncs.com",
    models: ["qwen3-coder-plus", "qwen-max", "qwen-plus", "qwen-turbo", "qwen3-vl-plus"],
    keyHint: "bailian.console.aliyun.com → API-KEY 管理",
    desc: "阿里云百炼原生协议（通义千问全系；国内直连）。兼容模式请选「OpenAI 兼容」。",
  },
  {
    value: "ollama",
    label: "Ollama（本地，无需 Key）",
    baseUrl: "http://localhost:11434",
    keyless: true,
    models: ["qwen2.5:7b", "llama3.1:8b", "deepseek-r1:7b"],
    desc: "本地推理服务（默认 http://localhost:11434），无需 API Key；可用「拉取本地模型」列出已装模型。",
  },
];

function emptyProvider(): ModelProviderConfig {
  return {
    name: "",
    provider_type: "openai",
    base_url: "https://api.openai.com/v1",
    api_key: "",
    model: "",
    context_window: 0,
  };
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
    cli_path: "",
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
  // Ollama 本地已装模型（协议=ollama 时点「拉取本地模型」填充，并入模型建议）。
  const [ollamaModels, setOllamaModels] = useState<string[]>([]);
  const [ollamaStatus, setOllamaStatus] = useState("");
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
      // 保存后立即验证 CLI 可用性（后端自动探索或用指定路径）——只提示
      // 不阻断：档案先存着，CLI 可以稍后装/再指定。
      const outcome = await api.saveDatabase(dbDraft);
      setDbDraft(null);
      await load();
      if (outcome.cli_resolved) {
        flash(`数据库 ${dbDraft.name} 已保存，CLI 就绪：${outcome.cli_resolved}`);
      } else if (outcome.cli_error) {
        fail(outcome.cli_error);
      } else {
        flash(`数据库 ${dbDraft.name} 已保存`);
      }
    } catch (e) {
      fail(e);
    }
  };

  // 连接表单「探测」按钮：即时验证 CLI 自动探索/指定路径的结果。
  const probeDbCli = async () => {
    if (!dbDraft) return;
    try {
      const path = await api.probeDatabaseCli(dbDraft.db_type, dbDraft.cli_path);
      flash(`CLI 已找到：${path}`);
    } catch (e) {
      fail(String(e));
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

  // 导航显隐开关的重渲染触发器（值本身在 localStorage，UI 只需刷新）。
  const [navTick, setNavTick] = useState(0);
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

        {/* ---------- Providers ---------- */}
        <div className="card">
          <h2>Model Providers</h2>
          <div className="desc">模型服务（OpenAI 兼容 / OpenAI Responses / Anthropic / Gemini / DashScope / Ollama）</div>
          {providers.map((p) => (
            <div key={p.name} className="agent-list-item" style={{ cursor: "default" }}>
              <span className="name">{p.name}</span>
              <span className="desc">
                [{p.provider_type || "openai"}] {p.base_url} · {p.model}
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
              <div className="lbl-row">
                <label className="lbl">协议类型</label>
              </div>
              <select
                className="field"
                value={providerDraft.provider_type || "openai"}
                onChange={(e) => {
                  const next = e.target.value;
                  const meta = PROVIDER_TYPES.find((t) => t.value === next);
                  // base_url 未自定义（仍是某协议默认值）时跟随切换，否则保留
                  const isPreset = PROVIDER_TYPES.some(
                    (t) => providerDraft.base_url === t.baseUrl,
                  );
                  setProviderDraft({
                    ...providerDraft,
                    provider_type: next,
                    base_url: isPreset && meta ? meta.baseUrl : providerDraft.base_url,
                  });
                  setOllamaModels([]);
                  setOllamaStatus("");
                }}
              >
                {PROVIDER_TYPES.map((t) => (
                  <option key={t.value} value={t.value}>
                    {t.label}
                  </option>
                ))}
              </select>
              <div className="desc" style={{ marginTop: 4 }}>
                {PROVIDER_TYPES.find((t) => t.value === (providerDraft.provider_type || "openai"))
                  ?.desc}
              </div>
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
                    {(providerDraft.provider_type || "openai") === "ollama" && (
                      <button
                        className="btn ghost"
                        style={{ padding: "0 8px" }}
                        onClick={() => {
                          setOllamaStatus("拉取中…");
                          void api
                            .ollamaListModels(providerDraft.base_url)
                            .then((models) => {
                              setOllamaModels(models);
                              setOllamaStatus(
                                models.length
                                  ? `已拉取 ${models.length} 个本地模型`
                                  : "本地未安装任何模型（先 ollama pull）",
                              );
                            })
                            .catch((e) => setOllamaStatus(`拉取失败：${String(e)}（可手输模型名）`));
                        }}
                      >
                        拉取本地模型
                      </button>
                    )}
                  </div>
                  <input
                    className="field"
                    value={providerDraft.model}
                    list="provider-model-suggestions"
                    onChange={(e) => setProviderDraft({ ...providerDraft, model: e.target.value })}
                  />
                  {ollamaStatus && (
                    <div className="desc" style={{ marginTop: 2 }}>
                      {ollamaStatus}
                    </div>
                  )}
                  <datalist id="provider-model-suggestions">
                    {[
                      ...(PROVIDER_TYPES.find(
                        (t) => t.value === (providerDraft.provider_type || "openai"),
                      )?.models ?? []),
                      ...ollamaModels,
                    ].map((m) => (
                      <option key={m} value={m} />
                    ))}
                  </datalist>
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
                <label className="lbl">上下文窗口（token，留空 = 未知；压缩预算 = 窗口 − 20000）</label>
              </div>
              <input
                className="field"
                type="number"
                min={0}
                placeholder="如 128000（deepseek 128k / qwen 131072）——不填则压缩只按消息条数兜底"
                value={providerDraft.context_window || ""}
                onChange={(e) =>
                  setProviderDraft({
                    ...providerDraft,
                    context_window: Number(e.target.value) || 0,
                  })
                }
              />
              <div className="lbl-row">
                <label className="lbl">
                  {PROVIDER_TYPES.find((t) => t.value === (providerDraft.provider_type || "openai"))
                    ?.keyless
                    ? "API Key（Ollama 本地服务无需填写）"
                    : "API Key（仅存本地配置文件）"}
                </label>
                {!PROVIDER_TYPES.find((t) => t.value === (providerDraft.provider_type || "openai"))
                  ?.keyless && (
                  <span className="desc">
                    获取：{
                      PROVIDER_TYPES.find(
                        (t) => t.value === (providerDraft.provider_type || "openai"),
                      )?.keyHint
                    }
                  </span>
                )}
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
                {k?.driver.mode === "Cli" && d.cli_path.trim() && (
                  <span className="badge" title={d.cli_path}>
                    CLI 已指定
                  </span>
                )}
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
                          : `驱动：外部客户端 ${kind.driver.program}（自动探索 PATH 与常见安装目录，也可在下方指定路径）`}
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
              {dbKind?.driver.mode === "Cli" && (
                <>
                  <div className="lbl-row">
                    <label className="lbl">
                      CLI 路径（可选，留空自动探索 PATH 与常见安装目录）
                    </label>
                    <PasteButton onPaste={(t) => setDbDraft({ ...dbDraft, cli_path: t })} />
                  </div>
                  <div style={{ display: "flex", gap: 8 }}>
                    <input
                      className="field"
                      style={{ flex: 1 }}
                      value={dbDraft.cli_path}
                      placeholder={`如 D:\\mysql\\bin\\mysql.exe 或 D:\\mysql\\bin`}
                      onChange={(e) => setDbDraft({ ...dbDraft, cli_path: e.target.value })}
                    />
                    <button className="btn ghost" onClick={() => void probeDbCli()}>
                      探测
                    </button>
                  </div>
                </>
              )}
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
              <label className="member-check">
                <input type="checkbox" checked={serverDraft.allow_legacy_rsa ?? false}
                  onChange={(e) => setServerDraft({ ...serverDraft, allow_legacy_rsa: e.target.checked })} />
                兼容旧服务器（允许 RSA/SHA-1）
              </label>
              <div className="desc">默认关闭，仅此服务器生效。只追加 ssh-rsa，不启用 ssh-dss；建议升级服务端 SSH。保存后断开并重新连接生效。</div>
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
