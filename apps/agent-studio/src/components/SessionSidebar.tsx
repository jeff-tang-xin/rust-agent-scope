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
  onDeleteSession,
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
  /** 删除会话（App 层补 runtime mode；删除后本组件刷新列表）。 */
  onDeleteSession: (wsId: string, sessionId: string) => void;
}) {
  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  // 内联重命名：正在编辑的 session id + 输入框值（Tauri WebView 无 prompt）。
  const [renaming, setRenaming] = useState("");
  const [renameValue, setRenameValue] = useState("");
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

  const commitRename = async (wsId: string, sessionId: string) => {
    const title = renameValue.trim();
    setRenaming("");
    try {
      await api.renameSession(wsId, sessionId, title);
      await load();
    } catch {
      /* 重命名失败静默（下次刷新可见原状） */
    }
  };

  const removeSession = async (wsId: string, sessionId: string) => {
    // 确认弹窗在 App 层（统一 confirm + toast 反馈）；这里只负责触发与刷新。
    await onDeleteSession(wsId, sessionId);
    await load();
  };

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

  // 树形归属：spawn 子代理会话（parent_session_id 指向父）缩进挂在父下；
  // 父不在列表（已清上下文）的子会话按顶层显示，不丢。
  const topSessions = sessionItems.filter(
    (s) => !s.parent_session_id || !sessionItems.some((p) => p.session_id === s.parent_session_id),
  );
  const childrenOf = (parentId: string) =>
    sessionItems.filter((s) => s.parent_session_id === parentId);

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
              {(() => {
                const renderRow = (s: SessionSummary, child: boolean) => {
                  const isRunning = running.has(`${w.id}:${s.session_id}`);
                  const isCurrent = currentSession === s.session_id;
                  if (renaming === s.session_id) {
                    return (
                      <div key={s.session_id} className="ss-session renaming">
                        <input
                          className="ss-rename"
                          autoFocus
                          value={renameValue}
                          placeholder="会话名（留空 = 自动摘要）"
                          onChange={(e) => setRenameValue(e.target.value)}
                          onKeyDown={(e) => {
                            if (e.key === "Enter") void commitRename(w.id, s.session_id);
                            if (e.key === "Escape") setRenaming("");
                          }}
                          onBlur={() => void commitRename(w.id, s.session_id)}
                        />
                      </div>
                    );
                  }
                  return (
                    <div
                      key={s.session_id}
                      className={`ss-session-row ${isCurrent ? "active" : ""} ${child ? "is-child" : ""}`}
                    >
                      <button
                        className={`ss-session ${isCurrent ? "active" : ""}`}
                        onClick={() => onSelectSession(w.id, s.session_id)}
                        title={s.title ? `${s.title}\n（${s.session_id}）` : s.session_id}
                      >
                        <span className={`ss-dot ${isRunning ? "run" : ""}`} aria-hidden />
                        <span className="ss-session-name">
                          {s.title || s.session_id.slice(0, 12)}
                        </span>
                        <span className="ss-session-state">
                          {isRunning ? "运行中" : child ? "子代理" : `${s.messages} 条`}
                        </span>
                      </button>
                      <span className="ss-session-actions">
                        <button
                          className="icon-btn"
                          title="重命名会话"
                          onClick={() => {
                            setRenaming(s.session_id);
                            setRenameValue(s.title ?? "");
                          }}
                        >
                          ✎
                        </button>
                        <button
                          className="icon-btn danger"
                          title="删除会话"
                          onClick={() => void removeSession(w.id, s.session_id)}
                        >
                          🗑
                        </button>
                      </span>
                    </div>
                  );
                };
                return topSessions.map((s) => (
                  <div key={s.session_id}>
                    {renderRow(s, false)}
                    {childrenOf(s.session_id).map((c) => renderRow(c, true))}
                  </div>
                ));
              })()}
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
