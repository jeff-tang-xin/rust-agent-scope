// 工作区空态页：该类别还没有工作区时显示（创建入口保留在这里；
// 有工作区后直接进入会话视图，不再经过卡片列表页）。

import type { WorkspaceKind } from "../api";

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
  test: {
    title: "测试工作区",
    sub: "测试工作台 · 需求 → 用例 → 自动执行 → 报告与 Bug",
  },
};

export default function WorkspacePage({
  category,
  onCreate,
}: {
  category: Category;
  onCreate: () => void;
}) {
  const meta = CATEGORY_META[category];
  return (
    <>
      <header className="page-header">
        <h1>{meta.title}</h1>
        <span className="sub">{meta.sub}</span>
      </header>
      <div className="page-body">
        <div className="empty">
          <div>还没有{meta.title}。</div>
          <button className="btn" onClick={onCreate}>
            + 新建{meta.title}
          </button>
        </div>
      </div>
    </>
  );
}
