// 会话左栏：当前类别下的工作区列表 + 选中工作区的 session 列表 +
// 每个 session 的运行状态（运行中 / 空闲）。多项目并行时在这里切换。

import { useCallback, useEffect, useState } from "react";
import {
  api,
  onTurnFinished,
  type SessionSummary,
  type WorkspaceConfig,
} from "../api";

export default function SessionSidebar({
  workspaces,
  selectedId,
  onSelectWorkspace,
  sessionByWs,
  onSelectSession,
  onNewSession,
  running,
  onEditWorkspace,
  onDeleteWorkspace,
  onCreateWorkspace,
}: {
  workspaces: WorkspaceConfig[];
  selectedId: string;
  onSelectWorkspace: (id: string) => void;
  /** 各工作区当前选中的 session（App 层持有，与 Conversation 受控同步）。 */
  sessionByWs: Record<string, string>;
  onSelectSession: (wsId: string, sessionId: string) => void;
  onNewSession: (wsId: string) => void;
  /** 运行中的 "workspaceId:sessionId" 集合（App 层全局事件维护）。 */
  running: Set<string>;
  onEditWorkspace: (w: WorkspaceConfig) => void;
  onDeleteWorkspace: (w: WorkspaceConfig) => void;
  onCreateWorkspace: () => void;
}) {
  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  const selected = workspaces.find((w) => w.id === selectedId);

  const load = useCallback(async () => {
    if (!selected) {
      setSessions([]);
      return;
    }
    try {
      setSessions(await api.listSessions(selected.id));
    } catch {
      /* 会话发现失败不阻塞左栏 */
    }
  }, [selected]);

  useEffect(() => {
    void load();
  }, [load]);

  // turn 结束刷新 session 摘要（新会话落盘 / 消息数变化）。
  useEffect(() => {
    const un = onTurnFinished((p) => {
      if (selected && p.workspace_id === selected.id) void load();
    });
    return () => {
      un.then((f) => f());
    };
  }, [selected, load]);

  // 当前选中的 session 可能尚未落盘（新建未发消息），补临时项保证显示。
  const currentSession = selected ? sessionByWs[selected.id] : undefined;
  const sessionItems: SessionSummary[] =
    selected && currentSession && !sessions.some((s) => s.session_id === currentSession)
      ? [{ session_id: currentSession, messages: 0, updated_at: 0 }, ...sessions]
      : sessions;

  return (
    <aside className="session-sidebar">
      <div className="ss-head">工作区</div>
      {workspaces.map((w) => (
        <div key={w.id}>
          <div
            className={`ss-ws ${w.id === selectedId ? "active" : ""}`}
            onClick={() => onSelectWorkspace(w.id)}
            title={w.dir.trim() || "默认工作区"}
          >
            <span className="ss-ws-name">{w.name}</span>
            <span className="ss-ws-actions">
              <button
                className="icon-btn"
                title="编辑工作区"
                onClick={(e) => {
                  e.stopPropagation();
                  onEditWorkspace(w);
                }}
              >
                ✎
              </button>
              <button
                className="icon-btn danger"
                title="删除工作区"
                onClick={(e) => {
                  e.stopPropagation();
                  onDeleteWorkspace(w);
                }}
              >
                🗑
              </button>
            </span>
          </div>
          {w.id === selectedId && (
            <>
              <div className="ss-ws-path" title={w.dir.trim() || "默认工作区（~/.ox）"}>
                {w.dir.trim() || "默认工作区（~/.ox）"}
              </div>
              <div className="ss-sessions">
              {sessionItems.map((s) => {
                const isRunning = running.has(`${w.id}:${s.session_id}`);
                const isCurrent = currentSession === s.session_id;
                return (
                  <button
                    key={s.session_id}
                    className={`ss-session ${isCurrent ? "active" : ""}`}
                    onClick={() => onSelectSession(w.id, s.session_id)}
                    title={s.session_id}
                  >
                    <span className={`ss-dot ${isRunning ? "run" : ""}`} aria-hidden />
                    <span className="ss-session-name">{s.session_id.slice(0, 12)}</span>
                    <span className="ss-session-state">
                      {isRunning ? "运行中" : `${s.messages} 条`}
                    </span>
                  </button>
                );
              })}
              {sessionItems.length === 0 && <div className="ss-empty">（暂无会话）</div>}
              <button className="ss-new" onClick={() => onNewSession(w.id)}>
                + 新会话
              </button>
              </div>
            </>
          )}
        </div>
      ))}
      <button className="ss-create" onClick={onCreateWorkspace}>
        + 新建工作区
      </button>
    </aside>
  );
}
