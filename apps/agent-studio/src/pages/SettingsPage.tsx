// 设置页：模型 Provider（≈ Environment）+ Agent 定义（≈ Managed Agent）的
// 创建与配置。配置持久化到 app config dir（studio-config.json）。

import { useEffect, useState } from "react";
import {
  api,
  type AgentConfig,
  type ModelProviderConfig,
} from "../api";
import AgentFormModal from "../components/AgentFormModal";
import PasteButton from "../components/PasteButton";

function emptyProvider(): ModelProviderConfig {
  return { name: "", base_url: "https://api.openai.com/v1", api_key: "", model: "" };
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
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  // agent 表单模态：null = 关闭；initial 为 null 表示新建。
  const [agentForm, setAgentForm] = useState<{ initial: AgentConfig | null } | null>(null);

  const load = async () => setProviders(await api.listProviders());
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

  // ---------- agents ----------

  const deleteAgent = async (id: string) => {
    try {
      await api.deleteAgent(id);
      onChanged();
      flash("工作区已删除（数据保留）");
    } catch (e) {
      fail(e);
    }
  };

  return (
    <>
      <header className="page-header">
        <h1>Settings</h1>
        <span className="sub">模型 Provider 与 Agent 定义（本地持久化）</span>
      </header>
      <div className="page-body">
        {error && <div className="notice error">{error}</div>}
        {notice && <div className="notice info">{notice}</div>}

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

        {/* ---------- Agents ---------- */}
        <div className="card">
          <h2>Agents</h2>
          <div className="desc">
            每个 Agent 独立 workspace 与会话状态；类别决定工具面，协同类的运行方式
            （solo/team）在进入会话时选择
          </div>
          {agents.map((a) => (
            <div key={a.id} className="agent-list-item" style={{ cursor: "default" }}>
              <span className="name">{a.name}</span>
              <span className="badge">
                {a.mode === "ops" ? "运维" : a.mode === "db" ? "数据库" : "协同"}
              </span>
              <span className="desc">{a.description || a.sys_prompt.slice(0, 60)}</span>
              <span className="badge">{a.provider}</span>
              <button className="btn ghost" onClick={() => setAgentForm({ initial: a })}>
                Edit
              </button>
              <button className="btn danger" onClick={() => void deleteAgent(a.id)}>
                Delete
              </button>
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
          category={agentForm.initial ? "collab" : "collab"}
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
