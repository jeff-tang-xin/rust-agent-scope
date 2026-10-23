// 设置页：模型 Provider（≈ Environment）+ Agent 定义（≈ Managed Agent）的
// 创建与配置。配置持久化到 app config dir（studio-config.json）。

import { useEffect, useState } from "react";
import { readText } from "@tauri-apps/plugin-clipboard-manager";
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
  const [agentDraft, setAgentDraft] = useState<AgentConfig | null>(null);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");

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

  const saveAgent = async () => {
    if (!agentDraft) return;
    try {
      if (agentDraft.id) {
        await api.updateAgent(agentDraft);
        flash(`Agent ${agentDraft.name} updated`);
      } else {
        const created = await api.createAgent(agentDraft);
        flash(`Agent ${created.name} created`);
      }
      setAgentDraft(null);
      onChanged();
    } catch (e) {
      fail(e);
    }
  };

  const deleteAgent = async (id: string) => {
    try {
      await api.deleteAgent(id);
      if (agentDraft?.id === id) setAgentDraft(null);
      onChanged();
      flash("Agent deleted (workspace data kept)");
    } catch (e) {
      fail(e);
    }
  };

  const patchMember = (index: number, patch: Partial<TeamMemberConfig>) => {
    if (!agentDraft) return;
    const members = agentDraft.members.map((m, i) =>
      i === index ? { ...m, ...patch } : m,
    );
    setAgentDraft({ ...agentDraft, members });
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
          <div className="desc">每个 Agent 独立 workspace 与会话状态；工具面由模式决定</div>
          {agents.map((a) => (
            <div key={a.id} className="agent-list-item" style={{ cursor: "default" }}>
              <span className="name">{a.name}</span>
              <span className="badge">{a.mode}</span>
              <span className="desc">{a.description || a.sys_prompt.slice(0, 60)}</span>
              <span className="badge">{a.provider}</span>
              <button className="btn ghost" onClick={() => setAgentDraft(a)}>
                Edit
              </button>
              <button className="btn danger" onClick={() => void deleteAgent(a.id)}>
                Delete
              </button>
            </div>
          ))}
          {agentDraft ? (
            <div style={{ marginTop: 12 }}>
              <div className="row">
                <div>
                  <div className="lbl-row">
                    <label className="lbl">Name</label>
                    <PasteButton onPaste={(t) => setAgentDraft({ ...agentDraft, name: t })} />
                  </div>
                  <input
                    className="field"
                    value={agentDraft.name}
                    onChange={(e) => setAgentDraft({ ...agentDraft, name: e.target.value })}
                  />
                </div>
                <div>
                  <label className="lbl">Mode</label>
                  <select
                    className="field"
                    value={agentDraft.mode}
                    onChange={(e) =>
                      setAgentDraft({ ...agentDraft, mode: e.target.value as AgentConfig["mode"] })
                    }
                  >
                    {MODES.map((m) => (
                      <option key={m.value} value={m.value}>
                        {m.label}
                      </option>
                    ))}
                  </select>
                </div>
                <div>
                  <label className="lbl">Provider</label>
                  <select
                    className="field"
                    value={agentDraft.provider}
                    onChange={(e) => setAgentDraft({ ...agentDraft, provider: e.target.value })}
                  >
                    {providers.map((p) => (
                      <option key={p.name} value={p.name}>
                        {p.name}（{p.model}）
                      </option>
                    ))}
                  </select>
                </div>
                <div style={{ maxWidth: 120 }}>
                  <label className="lbl">Max iters（0=默认）</label>
                  <input
                    className="field"
                    type="number"
                    min={0}
                    value={agentDraft.max_iters}
                    onChange={(e) =>
                      setAgentDraft({ ...agentDraft, max_iters: Number(e.target.value) || 0 })
                    }
                  />
                </div>
              </div>
              <div className="lbl-row">
                <label className="lbl">Description</label>
                <PasteButton
                  onPaste={(t) => setAgentDraft({ ...agentDraft, description: t })}
                />
              </div>
              <input
                className="field"
                value={agentDraft.description}
                onChange={(e) => setAgentDraft({ ...agentDraft, description: e.target.value })}
              />
              <div className="lbl-row">
                <label className="lbl">System prompt</label>
                <PasteButton
                  onPaste={(t) => setAgentDraft({ ...agentDraft, sys_prompt: t })}
                />
              </div>
              <textarea
                className="field"
                rows={5}
                value={agentDraft.sys_prompt}
                onChange={(e) => setAgentDraft({ ...agentDraft, sys_prompt: e.target.value })}
              />

              <div className="lbl-row">
                <label className="lbl">Workspace（工作区目录，留空 = 默认 app data）</label>
                <PasteButton
                  onPaste={(t) => setAgentDraft({ ...agentDraft, workspace: t })}
                />
              </div>
              <input
                className="field"
                placeholder="如 F:\java\my-project —— agent 的文件工具将锚定该目录"
                value={agentDraft.workspace}
                onChange={(e) => setAgentDraft({ ...agentDraft, workspace: e.target.value })}
              />

              {agentDraft.mode === "team" && (
                <>
                  <label className="lbl">Team members（lead 之外的成员；每个成员是独立子代理）</label>
                  {agentDraft.members.map((m, i) => (
                    <div key={i} className="member-row">
                      <input
                        className="field"
                        placeholder="name"
                        value={m.name}
                        onChange={(e) => patchMember(i, { name: e.target.value })}
                      />
                      <input
                        className="field"
                        placeholder="description（何时委派给它）"
                        value={m.description}
                        onChange={(e) => patchMember(i, { description: e.target.value })}
                      />
                      <input
                        className="field"
                        placeholder="prompt（成员系统提示）"
                        value={m.prompt}
                        onChange={(e) => patchMember(i, { prompt: e.target.value })}
                      />
                      <button
                        className="btn danger"
                        onClick={() =>
                          setAgentDraft({
                            ...agentDraft,
                            members: agentDraft.members.filter((_, j) => j !== i),
                          })
                        }
                      >
                        ×
                      </button>
                    </div>
                  ))}
                  <button
                    className="btn ghost"
                    onClick={() =>
                      setAgentDraft({
                        ...agentDraft,
                        members: [...agentDraft.members, { name: "", description: "", prompt: "" }],
                      })
                    }
                  >
                    + Add member
                  </button>
                </>
              )}

              <div style={{ marginTop: 14, display: "flex", gap: 8 }}>
                <button className="btn" onClick={() => void saveAgent()} disabled={!agentDraft.name || !agentDraft.provider}>
                  {agentDraft.id ? "Save changes" : "Create agent"}
                </button>
                <button className="btn ghost" onClick={() => setAgentDraft(null)}>
                  Cancel
                </button>
              </div>
            </div>
          ) : (
            <button className="btn ghost" style={{ marginTop: 8 }} onClick={() => setAgentDraft(emptyAgent(providers))}>
              + Create agent
            </button>
          )}
        </div>
      </div>
    </>
  );
}
