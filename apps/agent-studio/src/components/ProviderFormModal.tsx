// 模型服务表单模态（仅设置页使用）：协议类型 → 连接参数。
// 协议注册表 PROVIDER_TYPES 与后端 build_model 的分发键一致；
// 切协议时 base_url 未自定义（仍是某协议默认值）才跟随切换。

import { useEffect, useState } from "react";
import { api, type ModelProviderConfig } from "../api";
import PasteButton from "./PasteButton";

/** Provider 协议类型注册表：models 为常用模型建议（datalist，仍可自由输入）；
 * keyHint：Key 获取地址；desc：协议适用场景一行说明。 */
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

export default function ProviderFormModal({
  initial,
  existingNames,
  onClose,
  onSaved,
}: {
  /** 编辑时传现有配置；新建传 null。 */
  initial: ModelProviderConfig | null;
  /** 已占用的名称（编辑时名称不可改）。 */
  existingNames: string[];
  onClose: () => void;
  onSaved: (msg: string) => void;
}) {
  const editing = Boolean(initial?.name);
  const [draft, setDraft] = useState<ModelProviderConfig>(
    initial ?? {
      name: "",
      provider_type: "openai",
      base_url: "https://api.openai.com/v1",
      api_key: "",
      model: "",
      context_window: 0,
    },
  );
  // Ollama 本地已装模型（点「拉取本地模型」填充，并入模型建议）。
  const [ollamaModels, setOllamaModels] = useState<string[]>([]);
  const [ollamaStatus, setOllamaStatus] = useState("");
  const [error, setError] = useState("");
  const [saving, setSaving] = useState(false);

  const set = (patch: Partial<ModelProviderConfig>) => setDraft({ ...draft, ...patch });
  const meta = PROVIDER_TYPES.find((t) => t.value === (draft.provider_type || "openai"));

  const save = async () => {
    if (saving) return;
    setSaving(true);
    try {
      await api.saveProvider(draft);
      onSaved(`模型服务 ${draft.name} 已${editing ? "更新" : "创建"}`);
    } catch (e) {
      setError(String(e));
      setSaving(false);
    }
  };

  // Esc 关闭（保存中不响应）；遮罩点击关闭由 mask 的 onClick 承担。
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !saving) onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose, saving]);

  return (
    <div className="modal-mask" onClick={() => !saving && onClose()}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h2>{editing ? `编辑模型服务 · ${draft.name}` : "新建模型服务"}</h2>
        {error && <div className="notice error">{error}</div>}

        <label className="lbl">协议类型</label>
        <select
          className="field"
          value={draft.provider_type || "openai"}
          onChange={(e) => {
            const next = e.target.value;
            const m = PROVIDER_TYPES.find((t) => t.value === next);
            // base_url 未自定义（仍是某协议默认值）时跟随切换，否则保留
            const isPreset = PROVIDER_TYPES.some((t) => draft.base_url === t.baseUrl);
            setDraft({
              ...draft,
              provider_type: next,
              base_url: isPreset && m ? m.baseUrl : draft.base_url,
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
        {meta && <div className="desc" style={{ marginTop: 4 }}>{meta.desc}</div>}

        <div className="row" style={{ marginTop: 8 }}>
          <div>
            <div className="lbl-row">
              <label className="lbl">名称</label>
              <PasteButton onPaste={(t) => set({ name: t })} />
            </div>
            <input
              className="field"
              value={draft.name}
              disabled={editing || existingNames.includes(draft.name)}
              placeholder="如 deepseek / 本地 qwen"
              onChange={(e) => set({ name: e.target.value })}
            />
            {(editing || existingNames.includes(draft.name)) && (
              <div className="desc" style={{ marginTop: 2 }}>
                {editing ? "名称创建后不可修改" : "该名称已存在"}
              </div>
            )}
          </div>
          <div>
            <div className="lbl-row">
              <label className="lbl">模型</label>
              <PasteButton onPaste={(t) => set({ model: t })} />
              {meta?.keyless && meta.value === "ollama" && (
                <button
                  className="btn ghost"
                  style={{ padding: "0 8px" }}
                  onClick={() => {
                    setOllamaStatus("拉取中…");
                    void api
                      .ollamaListModels(draft.base_url)
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
              value={draft.model}
              list="provider-model-suggestions"
              placeholder="选择建议或直接输入"
              onChange={(e) => set({ model: e.target.value })}
            />
            {ollamaStatus && <div className="desc" style={{ marginTop: 2 }}>{ollamaStatus}</div>}
            <datalist id="provider-model-suggestions">
              {[...(meta?.models ?? []), ...ollamaModels].map((m) => (
                <option key={m} value={m} />
              ))}
            </datalist>
          </div>
        </div>

        <div className="lbl-row">
          <label className="lbl">Base URL</label>
          <PasteButton onPaste={(t) => set({ base_url: t })} />
        </div>
        <input
          className="field"
          value={draft.base_url}
          onChange={(e) => set({ base_url: e.target.value })}
        />

        <div className="lbl-row">
          <label className="lbl">上下文窗口（token，留空 = 未知）</label>
        </div>
        <input
          className="field"
          type="number"
          min={0}
          placeholder="如 128000（deepseek 128k / qwen 131072）；压缩预算 = 窗口 − 20000，不填则只按消息条数兜底"
          value={draft.context_window || ""}
          onChange={(e) => set({ context_window: Number(e.target.value) || 0 })}
        />

        {meta?.keyless ? (
          <>
            <div className="lbl-row">
              <label className="lbl">API Key</label>
            </div>
            <div className="desc">Ollama 本地服务无需填写，留空即可。</div>
          </>
        ) : (
          <>
            <div className="lbl-row">
              <label className="lbl">API Key（仅存本地配置文件）</label>
              <span className="desc">获取：{meta?.keyHint}</span>
              <PasteButton onPaste={(t) => set({ api_key: t })} />
            </div>
            <input
              className="field"
              type="password"
              value={draft.api_key}
              onChange={(e) => set({ api_key: e.target.value })}
            />
          </>
        )}

        <div className="modal-actions">
          <button
            className="btn"
            onClick={() => void save()}
            disabled={!draft.name || !draft.model || saving}
          >
            {saving ? "保存中…" : editing ? "保存修改" : "创建"}
          </button>
          <button className="btn ghost" onClick={onClose} disabled={saving}>
            取消
          </button>
        </div>
      </div>
    </div>
  );
}
