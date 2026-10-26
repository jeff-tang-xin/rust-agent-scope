// 工作区表单模态：新建 / 编辑两用。
// 工作区 = 针对一个项目的工作环境：选定智能体 + 起名 + 定目录；
// 协同类还可配置 team 运行方式的成员（引用已有智能体，lead 之外）。
// 智能体定义（人设 / Provider）在设置页管理，这里只做「选谁 + 叫什么 + 在哪干活」。

import { useEffect, useState, type Dispatch, type SetStateAction } from "react";
import {
  api,
  type AgentConfig,
  type DatabaseServerConfig,
  type ServerConfig,
  type WorkspaceConfig,
  type WorkspaceKind,
} from "../api";
import PasteButton from "./PasteButton";

const KIND_LABEL: Record<WorkspaceKind, string> = {
  collab: "协同",
  ops: "运维",
  db: "数据库运维",
};

export default function WorkspaceFormModal({
  agents,
  servers,
  databases,
  category,
  initial,
  onClose,
  onSaved,
}: {
  agents: AgentConfig[];
  /** 服务器档案（设置页管理；勾选绑定 → 工作区开放 remote_shell 能力）。 */
  servers: ServerConfig[];
  /** 数据库档案（设置页管理；勾选绑定 → 工作区开放 db_query/db_dml 能力）。 */
  databases: DatabaseServerConfig[];
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
  // 绑定的服务器/数据库档案名（能力白名单：绑定 = 开放对应工具且只能操作
  // 列表内的资源；协同工作区未绑定则无远程能力，ops/db 类别默认开放）。
  const [boundServers, setBoundServers] = useState<string[]>(initial?.bound_servers ?? []);
  const [boundDatabases, setBoundDatabases] = useState<string[]>(initial?.bound_databases ?? []);
  const [error, setError] = useState("");
  // 保存中：按钮禁用防重复提交（后端写配置文件非原子瞬间）。
  const [saving, setSaving] = useState(false);

  const editing = Boolean(initial?.id);
  const agent = agents.find((a) => a.id === agentId);

  const toggleMember = (id: string) => {
    setMembers((prev) =>
      prev.includes(id) ? prev.filter((m) => m !== id) : [...prev, id],
    );
  };

  const toggleBound = (
    setter: Dispatch<SetStateAction<string[]>>,
    name: string,
  ) => {
    setter((prev) =>
      prev.includes(name) ? prev.filter((n) => n !== name) : [...prev, name],
    );
  };

  const save = async () => {
    if (!agent || saving) return;
    setSaving(true);
    const payload: WorkspaceConfig = {
      id: initial?.id ?? "",
      name,
      kind,
      agent_id: agentId,
      dir,
      members: kind === "collab" ? members.filter((m) => m !== agentId) : [],
      bound_servers: boundServers,
      bound_databases: boundDatabases,
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
      setSaving(false);
    }
  };

  // Esc 关闭弹窗（桌面惯例）；保存中不响应，防误触丢填写。
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
            <label className="lbl">团队成员</label>
            <div className="desc" style={{ marginBottom: 4 }}>
              team 运行方式下的成员，每个成员是独立子代理（lead 之外）
            </div>
            {agents.filter((a) => a.id !== agentId).length === 0 ? (
              <div className="desc">暂无其他智能体可作成员；可先到 Settings 创建。</div>
            ) : (
              <div className="check-group">
                {agents
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
                  ))}
              </div>
            )}
          </>
        )}

        <label className="lbl">绑定服务器</label>
        <div className="desc" style={{ marginBottom: 4 }}>
          勾选后开放 remote_shell 且仅限勾选的服务器；不勾选 = 可操作全部
        </div>
        {servers.length === 0 ? (
          <div className="desc">暂无服务器档案——可先到 Settings 添加。</div>
        ) : (
          <div className="check-group">
            {servers.map((s) => (
              <label key={s.name} className="member-check">
                <input
                  type="checkbox"
                  checked={boundServers.includes(s.name)}
                  onChange={() => toggleBound(setBoundServers, s.name)}
                />
                <span className="name">{s.name}</span>
                <span className="desc">
                  {s.host}:{s.port}
                </span>
              </label>
            ))}
          </div>
        )}

        <label className="lbl">绑定数据库</label>
        <div className="desc" style={{ marginBottom: 4 }}>
          勾选后开放 db_query / db_dml 且仅限勾选的库；不勾选 = 可访问全部
        </div>
        {databases.length === 0 ? (
          <div className="desc">暂无数据库档案——可先到 Settings 添加。</div>
        ) : (
          <div className="check-group">
            {databases.map((d) => (
              <label key={d.name} className="member-check">
                <input
                  type="checkbox"
                  checked={boundDatabases.includes(d.name)}
                  onChange={() => toggleBound(setBoundDatabases, d.name)}
                />
                <span className="name">{d.name}</span>
                <span className="desc">
                  {d.db_type}
                  {d.database ? ` · ${d.database}` : ""}
                </span>
              </label>
            ))}
          </div>
        )}

        <div className="modal-actions">
          <button
            className="btn"
            onClick={() => void save()}
            disabled={!name || !agent || saving}
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
