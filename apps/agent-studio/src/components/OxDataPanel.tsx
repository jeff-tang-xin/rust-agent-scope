// .ox 数据面板：共享黑板报 + 记忆列表（用户管理入口）。
//
// 黑板：工作区级、跨会话共享（模型经 blackboard_append/read 工具读写，
// 只增不改）；本面板是用户侧删改的唯一入口——编辑（类型/正文）与删除。
// 记忆：MEMORY.md 与 memory/*.md 是 markdown 台账（无条目结构），做文件级
// 管理（查看/编辑/删除），不做条目级拆分。
//
// 样式复用 file-panel / modal 体系；与文件/服务器/数据库面板互斥展开
// （挂载方控制）。

import { useCallback, useEffect, useState } from "react";
import { api, type BlackboardEntry, type MemoryFileEntry } from "../api";
import DataViewerModal from "./DataViewerModal";

const KINDS = ["note", "finding", "risk", "conclusion"] as const;

const KIND_LABEL: Record<string, string> = {
  note: "note",
  finding: "finding",
  risk: "risk",
  conclusion: "conclusion",
};

function fmtTime(ms: number): string {
  if (!ms) return "";
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

function fmtSize(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}

export default function OxDataPanel({
  workspaceId,
  onCollapse,
  onToast,
}: {
  workspaceId: string;
  onCollapse: () => void;
  onToast?: (kind: "error" | "info", text: string) => void;
}) {
  const [tab, setTab] = useState<"board" | "memory">("board");
  return (
    <aside className="file-panel">
      <div className="file-panel-head">
        <div className="ox-tabs">
          <button
            className={`ox-tab ${tab === "board" ? "active" : ""}`}
            onClick={() => setTab("board")}
          >
            黑板报
          </button>
          <button
            className={`ox-tab ${tab === "memory" ? "active" : ""}`}
            onClick={() => setTab("memory")}
          >
            记忆
          </button>
        </div>
        <span className="spacer" />
        <button className="icon-btn" title="收起面板" onClick={onCollapse}>
          »
        </button>
      </div>
      {tab === "board" ? (
        <BlackboardTab workspaceId={workspaceId} onToast={onToast} />
      ) : (
        <MemoryTab workspaceId={workspaceId} onToast={onToast} />
      )}
    </aside>
  );
}

// ---------- 黑板报 ----------

function BlackboardTab({
  workspaceId,
  onToast,
}: {
  workspaceId: string;
  onToast?: (kind: "error" | "info", text: string) => void;
}) {
  const [entries, setEntries] = useState<BlackboardEntry[] | null>(null);
  const [error, setError] = useState("");
  const [editing, setEditing] = useState<BlackboardEntry | null>(null);
  const [viewing, setViewing] = useState<BlackboardEntry | null>(null);

  const load = useCallback(async () => {
    try {
      setError("");
      setEntries(await api.blackboardList(workspaceId));
    } catch (e) {
      setError(String(e));
    }
  }, [workspaceId]);

  useEffect(() => {
    setEntries(null);
    setEditing(null);
    void load();
  }, [load]);

  const remove = async (e: BlackboardEntry) => {
    if (!window.confirm(`删除黑板条目 #${e.id}？\n\n${e.content.slice(0, 80)}`)) return;
    try {
      await api.blackboardDelete(workspaceId, e.id);
      onToast?.("info", `已删除 #${e.id}`);
      void load();
    } catch (err) {
      setError(String(err));
    }
  };

  const saveEdit = async (kind: string, content: string) => {
    if (!editing) return;
    try {
      await api.blackboardUpdate(workspaceId, editing.id, kind, content);
      onToast?.("info", `已更新 #${editing.id}`);
      setEditing(null);
      void load();
    } catch (err) {
      setError(String(err));
    }
  };

  return (
    <div className="file-tree ox-board">
      {error && <div className="file-panel-error">{error}</div>}
      {entries === null && <div className="file-empty">加载中…</div>}
      {entries !== null && entries.length === 0 && (
        <div className="file-empty">黑板为空：智能体登记结论/风险后会出现在这里</div>
      )}
      {entries?.map((e) => (
        <div key={e.id} className={`ox-entry ox-kind-${e.type}`}>
          <div className="ox-entry-meta">
            <span className={`ox-badge ox-badge-${e.type}`}>{KIND_LABEL[e.type] ?? e.type}</span>
            <span className="ox-entry-id">#{e.id}</span>
            {e.author && <span className="ox-entry-author">{e.author}</span>}
            <span className="spacer" />
            <span className="ox-entry-time">{fmtTime(e.ts)}</span>
            <button className="icon-btn" title="编辑" onClick={() => setEditing({ ...e })}>
              ✎
            </button>
            <button className="icon-btn" title="删除" onClick={() => void remove(e)}>
              ✕
            </button>
          </div>
          <div
            className="ox-entry-content"
            title="点击查看全文"
            onClick={() => setViewing(e)}
          >
            {e.content}
          </div>
        </div>
      ))}
      {viewing && (
        <DataViewerModal
          title={`黑板条目 #${viewing.id}${viewing.author ? ` · ${viewing.author}` : ""}`}
          content={viewing.content}
          mode="markdown"
          editable
          onSave={async (content) => {
            try {
              await api.blackboardUpdate(workspaceId, viewing.id, viewing.type, content);
              onToast?.("info", `已更新 #${viewing.id}`);
              setViewing(null);
              void load();
            } catch (err) {
              setError(String(err));
            }
          }}
          onClose={() => setViewing(null)}
        />
      )}
      {editing && (
        <EditEntryModal
          entry={editing}
          onCancel={() => setEditing(null)}
          onSave={(kind, content) => void saveEdit(kind, content)}
        />
      )}
    </div>
  );
}

function EditEntryModal({
  entry,
  onCancel,
  onSave,
}: {
  entry: BlackboardEntry;
  onCancel: () => void;
  onSave: (kind: string, content: string) => void;
}) {
  const [kind, setKind] = useState(entry.type);
  const [content, setContent] = useState(entry.content);
  return (
    <div className="modal-mask" onClick={onCancel}>
      <div className="modal file-preview ox-edit" onClick={(e) => e.stopPropagation()}>
        <h2>
          编辑黑板条目 #{entry.id}
        </h2>
        <div className="ox-edit-kind">
          <span>类型：</span>
          <select value={kind} onChange={(e) => setKind(e.target.value)}>
            {KINDS.map((k) => (
              <option key={k} value={k}>
                {KIND_LABEL[k]}
              </option>
            ))}
          </select>
        </div>
        <textarea
          className="ox-edit-content"
          value={content}
          rows={6}
          onChange={(e) => setContent(e.target.value)}
        />
        <div className="modal-actions">
          <button className="btn ghost" onClick={onCancel}>
            取消
          </button>
          <button
            className="btn"
            disabled={!content.trim()}
            onClick={() => onSave(kind, content)}
          >
            保存
          </button>
        </div>
      </div>
    </div>
  );
}

// ---------- 记忆 ----------

function MemoryTab({
  workspaceId,
  onToast,
}: {
  workspaceId: string;
  onToast?: (kind: "error" | "info", text: string) => void;
}) {
  const [files, setFiles] = useState<MemoryFileEntry[] | null>(null);
  const [error, setError] = useState("");
  const [viewing, setViewing] = useState<{ path: string; content: string } | null>(null);

  const load = useCallback(async () => {
    try {
      setError("");
      setFiles(await api.memoryListFiles(workspaceId));
    } catch (e) {
      setError(String(e));
    }
  }, [workspaceId]);

  useEffect(() => {
    setFiles(null);
    setViewing(null);
    void load();
  }, [load]);

  const open = async (path: string) => {
    try {
      setError("");
      setViewing({ path, content: await api.memoryReadFile(workspaceId, path) });
    } catch (e) {
      setError(String(e));
    }
  };

  const save = async (path: string, content: string) => {
    try {
      await api.memorySaveFile(workspaceId, path, content);
      onToast?.("info", `已保存 ${path}`);
      setViewing({ path, content });
      void load();
    } catch (e) {
      setError(String(e));
    }
  };

  const remove = async (path: string) => {
    if (!window.confirm(`删除记忆文件 ${path}？\n\n删除后不可恢复（MEMORY.md 会在下次记忆写入时重建）。`)) return;
    try {
      await api.memoryDeleteFile(workspaceId, path);
      onToast?.("info", `已删除 ${path}`);
      setViewing((prev) => (prev?.path === path ? null : prev));
      void load();
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <div className="file-tree ox-memory">
      {error && <div className="file-panel-error">{error}</div>}
      {files === null && <div className="file-empty">加载中…</div>}
      {files !== null && files.length === 0 && (
        <div className="file-empty">暂无记忆文件（智能体沉淀记忆后出现在这里）</div>
      )}
      {files?.map((f) => (
        <div
          key={f.path}
          className={`ox-mem-file ${viewing?.path === f.path ? "active" : ""}`}
          onClick={() => void open(f.path)}
        >
          <span className="ox-mem-path">{f.path}</span>
          <span className="spacer" />
          <span className="ox-entry-time" title={`修改于 ${fmtTime(f.modified_ms)}`}>
            {fmtSize(f.size)}
          </span>
          <button
            className="icon-btn"
            title="删除文件"
            onClick={(ev) => {
              ev.stopPropagation();
              void remove(f.path);
            }}
          >
            ✕
          </button>
        </div>
      ))}
      {viewing && (
        <DataViewerModal
          title={viewing.path}
          content={viewing.content}
          editable
          onSave={(content) => save(viewing.path, content)}
          onClose={() => setViewing(null)}
        />
      )}
    </div>
  );
}
