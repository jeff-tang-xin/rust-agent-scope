// Agent 表单模态：新建/编辑工作区（agent）的唯一表单实现。
// SettingsPage 与 WorkspacePage 共用；类别（协同/运维/数据库运维）决定
// 工具面与装配——协同类配置固定存 mode="solo"，运行时再选 solo/team。

import { useState } from "react";
import {
  api,
  type AgentConfig,
  type ModelProviderConfig,
  type TeamMemberConfig,
} from "../api";
import PasteButton from "./PasteButton";

type Category = "collab" | "ops" | "db";

const CATEGORIES: { value: Category; label: string }[] = [
  { value: "collab", label: "协同（solo/team 运行时切换）" },
  { value: "ops", label: "运维（shell + web）" },
  { value: "db", label: "数据库运维（只读 SQL）" },
];

/** 配置 mode → 类别（team 是旧数据的运行方式语义，归入协同）。 */
function categoryOfMode(mode: string): Category {
  if (mode === "ops") return "ops";
  if (mode === "db") return "db";
  return "collab";
}

/** 类别 → 配置 mode（协同类固定 solo，运行方式在进入会话时选择）。 */
function modeOfCategory(c: Category): AgentConfig["mode"] {
  if (c === "ops") return "ops";
  if (c === "db") return "db";
  return "solo";
}

export default function AgentFormModal({
  initial,
  providers,
  category: presetCategory,
  onClose,
  onSaved,
}: {
  /** 编辑时传现有配置；新建传 null */
  initial: AgentConfig | null;
  providers: ModelProviderConfig[];
  /** 新建时的预设类别（来自所在工作区页） */
  category: Category;
  onClose: () => void;
  onSaved: (msg: string) => void;
}) {
  const [category, setCategory] = useState<Category>(
    initial ? categoryOfMode(initial.mode) : presetCategory,
  );
  const [draft, setDraft] = useState<AgentConfig>(() => {
    if (initial) return { ...initial, mode: modeOfCategory(categoryOfMode(initial.mode)) };
    return {
      id: "",
      name: "",
      description: "",
      sys_prompt: "",
      mode: modeOfCategory(presetCategory),
      provider: providers[0]?.name ?? "",
      max_iters: 0,
      members: [],
      workspace: "",
      created_at: 0,
    };
  });
  const [error, setError] = useState("");

  const patchMember = (index: number, patch: Partial<TeamMemberConfig>) => {
    const members = draft.members.map((m, i) => (i === index ? { ...m, ...patch } : m));
    setDraft({ ...draft, members });
  };

  const pickCategory = (c: Category) => {
    setCategory(c);
    setDraft({ ...draft, mode: modeOfCategory(c) });
  };

  const save = async () => {
    try {
      if (draft.id) {
        await api.updateAgent(draft);
        onSaved(`工作区 ${draft.name} 已更新`);
      } else {
        const created = await api.createAgent(draft);
        onSaved(`工作区 ${created.name} 已创建`);
      }
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <div className="modal-mask" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h2>{draft.id ? "编辑工作区" : "新建工作区"}</h2>
        {error && <div className="notice error">{error}</div>}

        <div className="row">
          <div>
            <div className="lbl-row">
              <label className="lbl">Name</label>
              <PasteButton onPaste={(t) => setDraft({ ...draft, name: t })} />
            </div>
            <input
              className="field"
              value={draft.name}
              onChange={(e) => setDraft({ ...draft, name: e.target.value })}
            />
          </div>
          <div>
            <label className="lbl">类别</label>
            <select
              className="field"
              value={category}
              onChange={(e) => pickCategory(e.target.value as Category)}
            >
              {CATEGORIES.map((c) => (
                <option key={c.value} value={c.value}>
                  {c.label}
                </option>
              ))}
            </select>
          </div>
        </div>

        <div className="row">
          <div>
            <label className="lbl">Provider</label>
            <select
              className="field"
              value={draft.provider}
              onChange={(e) => setDraft({ ...draft, provider: e.target.value })}
            >
              {providers.map((p) => (
                <option key={p.name} value={p.name}>
                  {p.name}（{p.model}）
                </option>
              ))}
            </select>
          </div>
          <div style={{ maxWidth: 140 }}>
            <label className="lbl">Max iters（0=默认）</label>
            <input
              className="field"
              type="number"
              min={0}
              value={draft.max_iters}
              onChange={(e) => setDraft({ ...draft, max_iters: Number(e.target.value) || 0 })}
            />
          </div>
        </div>

        <div className="lbl-row">
          <label className="lbl">Description</label>
          <PasteButton onPaste={(t) => setDraft({ ...draft, description: t })} />
        </div>
        <input
          className="field"
          value={draft.description}
          onChange={(e) => setDraft({ ...draft, description: e.target.value })}
        />

        <div className="lbl-row">
          <label className="lbl">System prompt</label>
          <PasteButton onPaste={(t) => setDraft({ ...draft, sys_prompt: t })} />
        </div>
        <textarea
          className="field"
          rows={5}
          value={draft.sys_prompt}
          onChange={(e) => setDraft({ ...draft, sys_prompt: e.target.value })}
        />

        <div className="lbl-row">
          <label className="lbl">Workspace（工作区目录，留空 = 默认 app data）</label>
          <PasteButton onPaste={(t) => setDraft({ ...draft, workspace: t })} />
        </div>
        <input
          className="field"
          placeholder="如 F:\java\my-project —— agent 的文件工具将锚定该目录"
          value={draft.workspace}
          onChange={(e) => setDraft({ ...draft, workspace: e.target.value })}
        />

        {category === "collab" && (
          <>
            <label className="lbl">
              Team members（team 运行方式下的成员；每个成员是独立子代理，lead 之外）
            </label>
            {draft.members.map((m, i) => (
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
                  onClick={() => setDraft({ ...draft, members: draft.members.filter((_, j) => j !== i) })}
                >
                  ×
                </button>
              </div>
            ))}
            <button
              className="btn ghost"
              onClick={() =>
                setDraft({
                  ...draft,
                  members: [...draft.members, { name: "", description: "", prompt: "" }],
                })
              }
            >
              + Add member
            </button>
          </>
        )}

        <div className="modal-actions">
          <button
            className="btn"
            onClick={() => void save()}
            disabled={!draft.name || !draft.provider}
          >
            {draft.id ? "保存修改" : "创建"}
          </button>
          <button className="btn ghost" onClick={onClose}>
            取消
          </button>
        </div>
      </div>
    </div>
  );
}
