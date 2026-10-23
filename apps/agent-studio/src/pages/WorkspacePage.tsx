// 工作区列表页：卡片网格（参照工作台产品形态）——
// 头像 + 名称 + 工作区路径 + 归属项目 + 模式徽标（协同类可切 solo/team）
// + 进入/编辑/删除。新建与编辑走 AgentFormModal。

import { useEffect, useState } from "react";
import { api, type AgentConfig, type ModelProviderConfig } from "../api";
import AgentFormModal from "../components/AgentFormModal";

export type Category = "collab" | "ops" | "db";

const CATEGORY_META: Record<
  Category,
  { title: string; sub: string; modes: AgentConfig["mode"][] }
> = {
  collab: {
    title: "协同工作区",
    sub: "单 Agent 独立完成任务，或 lead + 成员协作；solo/team 可切换，plan/free 可切换",
    modes: ["solo", "team"],
  },
  ops: {
    title: "运维工作区",
    sub: "运维工作台 · shell + web 工具面",
    modes: ["ops"],
  },
  db: {
    title: "数据库运维工作区",
    sub: "数据库工作台 · 只读 SQL",
    modes: ["db"],
  },
};

/** 归属项目名：自定义工作区取路径末段，默认工作区显示「默认」。 */
function projectOf(a: AgentConfig): string {
  const ws = a.workspace.trim();
  if (!ws) return "默认";
  const seg = ws.split(/[\\/]/).filter(Boolean).pop();
  return seg || "默认";
}

export default function WorkspacePage({
  category,
  agents,
  onOpen,
  onChanged,
  onNotice,
  onError,
}: {
  category: Category;
  agents: AgentConfig[];
  onOpen: (agent: AgentConfig) => void;
  onChanged: () => void;
  onNotice: (msg: string) => void;
  onError: (msg: string) => void;
}) {
  const meta = CATEGORY_META[category];
  const [providers, setProviders] = useState<ModelProviderConfig[]>([]);
  const [formOpen, setFormOpen] = useState(false);
  const [editing, setEditing] = useState<AgentConfig | null>(null);

  useEffect(() => {
    api
      .listProviders()
      .then(setProviders)
      .catch((e) => onError(String(e)));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const inCategory = agents.filter((a) => meta.modes.includes(a.mode));

  const remove = async (a: AgentConfig) => {
    if (!window.confirm(`删除工作区「${a.name}」？工作区数据保留，仅移除配置。`)) return;
    try {
      await api.deleteAgent(a.id);
      onNotice("工作区已删除（数据保留）");
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
        <button
          className="btn"
          onClick={() => {
            setEditing(null);
            setFormOpen(true);
          }}
        >
          + 新建{meta.title.replace("工作区", "")}工作区
        </button>
      </header>
      <div className="page-body">
        {inCategory.length === 0 ? (
          <div className="empty">
            <div>还没有{meta.title}。</div>
            <button
              className="btn"
              onClick={() => {
                setEditing(null);
                setFormOpen(true);
              }}
            >
              + 新建{meta.title.replace("工作区", "")}工作区
            </button>
          </div>
        ) : (
          <div className="ws-grid">
            {inCategory.map((a) => (
              <div key={a.id} className="ws-card">
                <div className="ws-card-head">
                  <div className="avatar">{a.name.slice(0, 1).toUpperCase()}</div>
                  <div className="ws-card-title">
                    <div className="name">{a.name}</div>
                    <div className="path" title={a.workspace || "默认工作区"}>
                      {a.workspace.trim() || "默认工作区"}
                    </div>
                  </div>
                  <div className="ws-card-actions">
                    <button
                      className="icon-btn"
                      title="编辑"
                      onClick={() => {
                        setEditing(a);
                        setFormOpen(true);
                      }}
                    >
                      ✎
                    </button>
                    <button className="icon-btn danger" title="删除" onClick={() => void remove(a)}>
                      🗑
                    </button>
                  </div>
                </div>
                <div className="ws-card-foot">
                  <span className="proj" title={projectOf(a)}>
                    📁 归属项目：{projectOf(a)}
                  </span>
                  <span className="spacer" />
                  {category === "collab" && (
                    <span className="badge" title="运行方式在进入后可切换（solo / team）">
                      协同
                    </span>
                  )}
                  <button className="btn ghost sm" onClick={() => onOpen(a)}>
                    进入 →
                  </button>
                </div>
              </div>
            ))}
          </div>
        )}
      </div>

      {formOpen && (
        <AgentFormModal
          initial={editing}
          providers={providers}
          category={category}
          onClose={() => setFormOpen(false)}
          onSaved={(msg) => {
            setFormOpen(false);
            onNotice(msg);
            onChanged();
          }}
        />
      )}
    </>
  );
}
