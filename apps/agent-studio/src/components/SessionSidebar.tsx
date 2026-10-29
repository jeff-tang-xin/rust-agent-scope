// 会话左栏：当前类别下的工作区列表 + 选中工作区的 session 列表 +
// 每个 session 的运行状态（运行中 / 空闲）。多项目并行时在这里切换。

import { useCallback, useEffect, useState } from "react";
import {
  api,
  onTurnFinished,
  type ArchivedSession,
  type SessionSummary,
  type WorkspaceConfig,
} from "../api";

/** 会话更新时间（unix 秒）→ 相对时间；非法值返回空串。 */
function relTime(unixSec: number): string {
  if (!unixSec) return "";
  const diff = Date.now() - unixSec * 1000;
  if (diff < 60_000) return "刚刚";
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)} 分钟前`;
  if (diff < 86_400_000) return `${Math.floor(diff / 3_600_000)} 小时前`;
  if (diff < 172_800_000) return "昨天";
  const d = new Date(unixSec * 1000);
  return `${d.getMonth() + 1}-${d.getDate()}`;
}

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
  onArchiveSession,
  onRestoreSession,
  /** 顶部分类名（协同/运维/数据库运维）——替换无信息量的「工作区」标题。 */
  categoryLabel,
  /** 栏宽（App 层拖拽调宽持有）；collapsed 态忽略。 */
  width,
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
  /** 归档会话（App 层补 runtime mode；归档后本组件刷新列表）。 */
  onArchiveSession: (wsId: string, sessionId: string) => Promise<void>;
  /** 还原会话（从归档箱恢复）。 */
  onRestoreSession: (wsId: string, sessionId: string) => Promise<void>;
  categoryLabel: string;
  width?: number;
}) {
  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  // 内联重命名：正在编辑的 session id + 输入框值（Tauri WebView 无 prompt）。
  const [renaming, setRenaming] = useState("");
  // 会话搜索（Ctrl+K 聚焦）：按标题/会话 ID 子串过滤当前列表。
  const [query, setQuery] = useState("");
  const [renameValue, setRenameValue] = useState("");
  // 归档箱：已归档会话列表 + 展开开关（归档信息查询菜单）。
  const [archived, setArchived] = useState<ArchivedSession[]>([]);
  const [showArchive, setShowArchive] = useState(false);
  // 收窄：折叠成窄条只留展开按钮（localStorage 记忆，跨会话保持）。
  const [collapsed, setCollapsed] = useState(
    () => localStorage.getItem("ss-collapsed") === "1",
  );
  const toggleCollapsed = () =>
    setCollapsed((prev) => {
      localStorage.setItem("ss-collapsed", prev ? "0" : "1");
      return !prev;
    });
  const selected = workspaces.find((w) => w.id === selectedId);

  const load = useCallback(async () => {
    if (!selected) {
      setSessions([]);
      setArchived([]);
      return;
    }
    try {
      setSessions(await api.listSessions(selected.id));
    } catch {
      /* 会话发现失败不阻塞左栏 */
    }
    try {
      setArchived(await api.listArchivedSessions(selected.id));
    } catch {
      /* 归档列表拉取失败不阻塞左栏 */
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

  // 归档会话：确认弹窗在 App 层；归档后从常规列表消失（数据留在原地）。
  const archiveSession = async (wsId: string, sessionId: string) => {
    await onArchiveSession(wsId, sessionId);
    await load();
  };

  // 还原会话：从归档箱恢复到常规列表。
  const restoreSession = async (wsId: string, sessionId: string) => {
    await onRestoreSession(wsId, sessionId);
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

  // 搜索过滤：标题或会话 ID 含关键字（大小写不敏感）；空串不过滤。
  const q = query.trim().toLowerCase();
  const visible: SessionSummary[] = q
    ? sessionItems.filter(
        (s) =>
          (s.title ?? "").toLowerCase().includes(q) || s.session_id.toLowerCase().includes(q),
      )
    : sessionItems;

  // 树形归属：spawn 子代理会话（parent_session_id 指向父）缩进挂在父下；
  // 父不在列表（已清上下文）的子会话按顶层显示，不丢。
  const topSessions = visible.filter(
    (s) => !s.parent_session_id || !visible.some((p) => p.session_id === s.parent_session_id),
  );
  const childrenOf = (parentId: string) =>
    visible.filter((s) => s.parent_session_id === parentId);

  if (collapsed) {
    return (
      <aside className="session-sidebar collapsed">
        <button
          className="icon-btn ss-collapse-btn"
          title="展开工作区栏"
          onClick={toggleCollapsed}
        >
          »
        </button>
        {/* 收起态：当前工作区的会话小图标（首字符方块），点击直达切换会话；
            运行中带绿点角标，当前会话高亮。悬停看完整会话名。 */}
        {selected && sessionItems.length > 0 && (
          <div className="ss-rail">
            {sessionItems.map((s) => {
              const isRunning = running.has(`${selected.id}:${s.session_id}`);
              const isCurrent = currentSession === s.session_id;
              const label = (s.title || s.session_id).trim();
              const meta = `${s.title || s.session_id} · ${isRunning ? "运行中" : relTime(s.updated_at) || `${s.messages} 条消息`}`;
              return (
                <button
                  key={s.session_id}
                  className={`ss-rail-item ${isCurrent ? "active" : ""}`}
                  title={meta}
                  onClick={() => onSelectSession(selected.id, s.session_id)}
                >
                  {label.slice(0, 2) || "·"}
                  {isRunning && <span className="ss-rail-run" aria-hidden />}
                </button>
              );
            })}
          </div>
        )}
      </aside>
    );
  }

  return (
    <aside className="session-sidebar" style={width ? { width } : undefined}>
      <div className="ss-body">
      <div className="ss-main">
      <div className="ss-head">
        <span>{categoryLabel}</span>
      </div>
      <div className="ss-search">
        <input
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          placeholder="搜索会话…（Ctrl+K）"
          data-ss-search
        />
        {query && (
          <button className="icon-btn" title="清除" onClick={() => setQuery("")}>
            ×
          </button>
        )}
      </div>
      <button className="ss-create" onClick={onCreateWorkspace}>
        + 新建工作区
      </button>
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
                        <span
                          className="ss-session-state"
                          title={`${new Date(s.updated_at * 1000).toLocaleString("zh-CN", { hour12: false })} · ${s.messages} 条消息`}
                        >
                          {isRunning ? "运行中" : child ? "子代理" : relTime(s.updated_at) || `${s.messages} 条`}
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
                          className="icon-btn"
                          title="归档会话（从列表隐藏，可随时还原）"
                          onClick={() => void archiveSession(w.id, s.session_id)}
                        >
                          🗄
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
              {/* 归档信息查询菜单：展开已归档会话（标题/条数/归档时间）+ 还原 */}
              <button
                className={`ss-archive-toggle ${showArchive ? "active" : ""}`}
                onClick={() => setShowArchive((v) => !v)}
                title="归档箱：查看已归档会话并还原"
              >
                🗄 归档箱{archived.length > 0 ? ` (${archived.length})` : ""}
              </button>
              {showArchive && (
                <div className="ss-archive-list">
                  {archived.length === 0 && <div className="ss-empty">（暂无归档会话）</div>}
                  {archived.map((a) => (
                    <div key={a.session_id} className="ss-archive-row">
                      <span
                        className="ss-archive-name"
                        title={`${a.title ?? a.session_id}\n（${a.session_id}）\n归档于 ${new Date(a.archived_at * 1000).toLocaleString("zh-CN", { hour12: false })}`}
                      >
                        {a.title || a.session_id.slice(0, 12)}
                      </span>
                      <span className="ss-archive-meta">
                        {a.messages} 条 · {relTime(a.archived_at) || "已归档"}
                      </span>
                      <button
                        className="icon-btn"
                        title="还原会话"
                        onClick={() => void restoreSession(w.id, a.session_id)}
                      >
                        ↩
                      </button>
                    </div>
                  ))}
                </div>
              )}
              </div>
            </>
          )}
        </div>
      ))}
      </div>
      {/* 版权信息（常驻左栏底部） */}
      <div className="ss-copyright">power by xinl.tang</div>
      </div>
      {/* 右缘竖条：整条可点收窄（把手常驻，好点好找） */}
      <button
        className="ss-collapse-strip"
        title="收窄工作区栏"
        onClick={toggleCollapsed}
      >
        ‹
      </button>
    </aside>
  );
}
