// 工作区表单模态：新建 / 编辑两用。
// 工作区 = 针对一个项目的工作环境：选定智能体 + 起名 + 定目录；
// 协同类还可配置 team 运行方式的成员（引用已有智能体，lead 之外）。
// 智能体定义（人设 / Provider）在设置页管理，这里只做「选谁 + 叫什么 + 在哪干活」。

import { useState } from "react";
import { api, type AgentConfig, type WorkspaceConfig, type WorkspaceKind } from "../api";
import PasteButton from "./PasteButton";

const KIND_LABEL: Record<WorkspaceKind, string> = {
  collab: "协同",
  ops: "运维",
  db: "数据库运维",
};

export default function WorkspaceFormModal({
  agents,
  category,
  initial,
  onClose,
  onSaved,
}: {
  agents: AgentConfig[];
  /** 工作区类型（由所在页决定；编辑时取 initial.kind）。 */
  category: WorkspaceKind;
  /** 编辑时传现有工作区；新建传 null。 */
  initial: WorkspaceConfig | null;
  onClose: () => void;
  onSaved: (msg: string) => void;
}) {
  const kind = initial?.kind ?? category;
  const [agentId, setAgentId] = useState(initial?.agent_id ?? agents[0]?.id ?? "");
  const [name, setName] = useState(initial?.name ?? "");
  const [dir, setDir] = useState(initial?.dir ?? "");
  // team 成员（仅协同工作区）：引用已有智能体 id（lead 之外）。
  const [members, setMembers] = useState<string[]>(initial?.members ?? []);
  const [error, setError] = useState("");

  const editing = Boolean(initial?.id);
  const agent = agents.find((a) => a.id === agentId);

  const toggleMember = (id: string) => {
    setMembers((prev) =>
      prev.includes(id) ? prev.filter((m) => m !== id) : [...prev, id],
    );
  };

  const save = async () => {
    if (!agent) return;
    const payload: WorkspaceConfig = {
      id: initial?.id ?? "",
      name,
      kind,
      agent_id: agentId,
      dir,
      members: kind === "collab" ? members.filter((m) => m !== agentId) : [],
      created_at: initial?.created_at ?? 0,
    };
    try {
      if (editing) {
        await api.updateWorkspace(payload);
        onSaved(`工作区 ${name} 已更新`);
      } else {
        const created = await api.createWorkspace(payload);
        onSaved(`工作区 ${created.name} 已创建（智能体：${agent.name}）`);
      }
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <div className="modal-mask" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h2>{editing ? `编辑${KIND_LABEL[kind]}工作区` : `新建${KIND_LABEL[kind]}工作区`}</h2>
        {error && <div className="notice error">{error}</div>}
        {agents.length === 0 && (
          <div className="notice info">
            还没有可用的智能体——请先到 Settings 创建，或使用内置的「全能助手」。
          </div>
        )}

        <label className="lbl">智能体（本工作区用它干活）</label>
        <select
          className="field"
          value={agentId}
          onChange={(e) => setAgentId(e.target.value)}
        >
          {agents.map((a) => (
            <option key={a.id} value={a.id}>
              {a.name}
              {a.provider ? "" : "（未绑定 Provider）"}
            </option>
          ))}
        </select>
        {agent && (
          <div className="desc" style={{ margin: "4px 0 0" }}>
            {agent.description || "（无描述）"}
          </div>
        )}

        <div className="lbl-row">
          <label className="lbl">Name（工作区名）</label>
          <PasteButton onPaste={setName} />
        </div>
        <input
          className="field"
          value={name}
          placeholder="如 my-project"
          onChange={(e) => setName(e.target.value)}
        />

        <div className="lbl-row">
          <label className="lbl">Workspace（工作区目录，留空 = 默认 app data）</label>
          <PasteButton onPaste={setDir} />
        </div>
        <input
          className="field"
          placeholder="如 F:\java\my-project —— 智能体的文件工具将锚定该目录"
          value={dir}
          onChange={(e) => setDir(e.target.value)}
        />

        {kind === "collab" && (
          <>
            <label className="lbl">
              Team members（team 运行方式下的成员；每个成员是独立子代理，lead 之外）
            </label>
            {agents.filter((a) => a.id !== agentId).length === 0 ? (
              <div className="desc">暂无其他智能体可作成员；可先到 Settings 创建。</div>
            ) : (
              agents
                .filter((a) => a.id !== agentId)
                .map((a) => (
                  <label key={a.id} className="member-check">
                    <input
                      type="checkbox"
                      checked={members.includes(a.id)}
                      onChange={() => toggleMember(a.id)}
                    />
                    <span className="name">{a.name}</span>
                    <span className="desc">{a.description || a.sys_prompt.slice(0, 40)}</span>
                  </label>
                ))
            )}
          </>
        )}

        <div className="modal-actions">
          <button
            className="btn"
            onClick={() => void save()}
            disabled={!name || !agent}
          >
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
