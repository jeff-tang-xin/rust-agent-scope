// 工作区列表页：卡片网格——工作区名 + 工作目录 + 选定的智能体 + 进入/
// 编辑/删除。工作区 = 针对一个项目的工作环境（协同/运维/数据库运维三类）；
// 新建与编辑都走 WorkspaceFormModal（编辑的是工作区，不是智能体定义）。

import { useState } from "react";
import { api, type AgentConfig, type WorkspaceConfig, type WorkspaceKind } from "../api";
import WorkspaceFormModal from "../components/WorkspaceFormModal";

export type Category = WorkspaceKind;

const CATEGORY_META: Record<WorkspaceKind, { title: string; sub: string }> = {
  collab: {
    title: "协同工作区",
    sub: "单 Agent 独立完成任务，或 lead + 成员协作；solo/team 可切换，plan/free 可切换",
  },
  ops: {
    title: "运维工作区",
    sub: "运维工作台 · shell + web 工具面",
  },
  db: {
    title: "数据库运维工作区",
    sub: "数据库工作台 · 只读 SQL",
  },
};

/** 归属项目名：自定义目录取路径末段，默认工作区显示「默认」。 */
function projectOf(w: WorkspaceConfig): string {
  const dir = w.dir.trim();
  if (!dir) return "默认";
  const seg = dir.split(/[\\/]/).filter(Boolean).pop();
  return seg || "默认";
}

export default function WorkspacePage({
  category,
  agents,
  workspaces,
  onOpen,
  onChanged,
  onNotice,
  onError,
}: {
  category: Category;
  agents: AgentConfig[];
  workspaces: WorkspaceConfig[];
  onOpen: (workspace: WorkspaceConfig) => void;
  onChanged: () => void;
  onNotice: (msg: string) => void;
  onError: (msg: string) => void;
}) {
  const meta = CATEGORY_META[category];
  // 表单状态：新建 / 编辑都走 WorkspaceFormModal（编辑的是工作区配置）。
  const [form, setForm] = useState<{
    kind: "new" | "edit";
    initial?: WorkspaceConfig;
  } | null>(null);

  const inCategory = workspaces.filter((w) => w.kind === category);
  const agentName = (id: string) => agents.find((a) => a.id === id)?.name ?? "（智能体已删除）";

  const remove = async (w: WorkspaceConfig) => {
    if (!window.confirm(`删除工作区「${w.name}」？会话数据保留，仅移除工作区配置。`)) return;
    try {
      await api.deleteWorkspace(w.id);
      onNotice("工作区已删除（会话数据保留）");
      onChanged();
    } catch (e) {
      onError(String(e));
    }
  };

  return (
    <>
      <header className="page-header">
        <h1>{meta.title}</h1>
        <span className="sub">{meta.sub}</span>
        <span className="spacer" />
        <button className="btn" onClick={() => setForm({ kind: "new" })}>
          + 新建{meta.title.replace("工作区", "")}工作区
        </button>
      </header>
      <div className="page-body">
        {inCategory.length === 0 ? (
          <div className="empty">
            <div>还没有{meta.title}。</div>
            <button className="btn" onClick={() => setForm({ kind: "new" })}>
              + 新建{meta.title.replace("工作区", "")}工作区
            </button>
          </div>
        ) : (
          <div className="ws-grid">
            {inCategory.map((w) => (
              <div key={w.id} className="ws-card">
                <div className="ws-card-head">
                  <div className="avatar">{w.name.slice(0, 1).toUpperCase()}</div>
                  <div className="ws-card-title">
                    <div className="name">{w.name}</div>
                    <div className="path" title={w.dir || "默认工作区"}>
                      {w.dir.trim() || "默认工作区"}
                    </div>
                  </div>
                  <div className="ws-card-actions">
                    <button
                      className="icon-btn"
                      title="编辑工作区"
                      onClick={() => setForm({ kind: "edit", initial: w })}
                    >
                      ✎
                    </button>
                    <button className="icon-btn danger" title="删除工作区" onClick={() => void remove(w)}>
                      🗑
                    </button>
                  </div>
                </div>
                <div className="ws-card-foot">
                  <span className="proj" title={projectOf(w)}>
                    📁 归属项目：{projectOf(w)}
                  </span>
                  <span className="spacer" />
                  <span className="badge" title="本工作区选定的智能体">
                    🤖 {agentName(w.agent_id)}
                  </span>
                  <button className="btn ghost sm" onClick={() => onOpen(w)}>
                    进入 →
                  </button>
                </div>
              </div>
            ))}
          </div>
        )}
      </div>

      {form && (
        <WorkspaceFormModal
          agents={agents}
          category={category}
          initial={form.initial ?? null}
          onClose={() => setForm(null)}
          onSaved={(msg) => {
            setForm(null);
            onNotice(msg);
            onChanged();
          }}
        />
      )}
    </>
  );
}
