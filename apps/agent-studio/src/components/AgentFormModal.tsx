// 智能体表单模态（仅设置页使用）：智能体 = 纯定义——人设 + Provider 绑定。
// 类别 / 工作目录 / team 成员都不在这里：那些是工作区的属性，在工作区表单里选。
// 内置智能体（default-assistant）：全字段可改，但不可删除（后端同样拒绝）。

import { useState } from "react";
import { api, DEFAULT_AGENT_ID, type AgentConfig } from "../api";
import PasteButton from "./PasteButton";

function emptyAgent(): AgentConfig {
  return {
    id: "",
    name: "",
    description: "",
    sys_prompt: "",
    provider: "",
    max_iters: 30,
    created_at: 0,
  };
}

export default function AgentFormModal({
  initial,
  providers,
  onClose,
  onSaved,
}: {
  /** 编辑时传现有智能体；新建传 null。 */
  initial: AgentConfig | null;
  providers: { name: string }[];
  onClose: () => void;
  onSaved: (msg: string) => void;
}) {
  const editing = Boolean(initial?.id);
  const builtin = initial?.id === DEFAULT_AGENT_ID;
  const [draft, setDraft] = useState<AgentConfig>(initial ?? emptyAgent());
  const [error, setError] = useState("");

  const set = (patch: Partial<AgentConfig>) => setDraft({ ...draft, ...patch });

  const save = async () => {
    try {
      if (editing) {
        await api.updateAgent(draft);
        onSaved(`智能体 ${draft.name} 已更新`);
      } else {
        const created = await api.createAgent(draft);
        onSaved(`智能体 ${created.name} 已创建`);
      }
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <div className="modal-mask" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h2>{editing ? "编辑智能体" : "新建智能体"}</h2>
        {error && <div className="notice error">{error}</div>}
        {builtin && (
          <div className="notice info">
            内置智能体：可修改任意字段；不可删除（始终保留）。
          </div>
        )}

        <div className="lbl-row">
          <label className="lbl">Name</label>
          <PasteButton onPaste={(t) => set({ name: t })} />
        </div>
        <input
          className="field"
          value={draft.name}
          onChange={(e) => set({ name: e.target.value })}
        />

        <label className="lbl">Provider</label>
        <select
          className="field"
          value={draft.provider}
          onChange={(e) => set({ provider: e.target.value })}
        >
          <option value="">（未绑定）</option>
          {providers.map((p) => (
            <option key={p.name} value={p.name}>
              {p.name}
            </option>
          ))}
        </select>

        <label className="lbl">Max iterations</label>
        <input
          className="field"
          type="number"
          min={1}
          max={200}
          value={draft.max_iters}
          onChange={(e) => set({ max_iters: Number(e.target.value) || 30 })}
        />

        <div className="lbl-row">
          <label className="lbl">Description</label>
          <PasteButton onPaste={(t) => set({ description: t })} />
        </div>
        <input
          className="field"
          value={draft.description}
          onChange={(e) => set({ description: e.target.value })}
        />

        <div className="lbl-row">
          <label className="lbl">System prompt</label>
          <PasteButton onPaste={(t) => set({ sys_prompt: t })} />
        </div>
        <textarea
          className="field"
          rows={6}
          value={draft.sys_prompt}
          onChange={(e) => set({ sys_prompt: e.target.value })}
        />

        <div className="modal-actions">
          <button className="btn" onClick={() => void save()} disabled={!draft.name}>
            {editing ? "保存修改" : "创建"}
          </button>
          <button className="btn ghost" onClick={onClose}>
            取消
          </button>
        </div>
      </div>
    </div>
  );
}
