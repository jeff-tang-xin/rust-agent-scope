// ops 工作区右侧服务器面板：服务器列表 → 连接 → 目录浏览 + 文件操作
// （上传/下载/删除）+ sh 模式（手动执行命令）。AI 模式 = 左侧会话
// （remote_shell 工具，每次执行走审批卡）。
import { useCallback, useEffect, useState } from "react";
import { open as openFileDialog, save as saveFileDialog } from "@tauri-apps/plugin-dialog";
import { api, type DirEntry, type ServerConfig } from "../api";

/** 单层目录条目上限（与文件面板同策略：懒加载，点开哪层列哪层）。 */
const PAGE_LIMIT = 2000;

/** 格式化文件大小。 */
function fmtSize(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
  return `${(n / 1024 / 1024 / 1024).toFixed(1)} GB`;
}

export default function ServersPanel({ onCollapse }: { onCollapse: () => void }) {
  const [servers, setServers] = useState<ServerConfig[]>([]);
  const [connected, setConnected] = useState<Set<string>>(new Set());
  const [active, setActive] = useState(""); // 当前操作的服务器
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  const reload = useCallback(async () => {
    try {
      const list = await api.listServers();
      setServers(list);
      // 恢复连接状态显示（后端连接池仍在的保持绿点）。
      const statuses = await Promise.all(list.map((s) => api.sshStatus(s.name)));
      setConnected(new Set(list.filter((_, i) => statuses[i]).map((s) => s.name)));
    } catch (e) {
      setError(String(e));
    }
  }, []);

  useEffect(() => {
    void reload();
  }, [reload]);

  const connect = async (name: string) => {
    setBusy(true);
    setError("");
    try {
      await api.sshConnect(name);
      setConnected((prev) => new Set(prev).add(name));
      setActive(name);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const disconnect = async (name: string) => {
    try {
      await api.sshDisconnect(name);
    } finally {
      setConnected((prev) => {
        const next = new Set(prev);
        next.delete(name);
        return next;
      });
      if (active === name) setActive("");
    }
  };

  return (
    <aside className="file-panel">
      <div className="file-panel-head">
        <span>服务器</span>
        <span className="spacer" />
        <button className="icon-btn" title="刷新" onClick={() => void reload()}>
          ⟳
        </button>
        <button className="icon-btn" title="收起面板（AI 模式）" onClick={onCollapse}>
          »
        </button>
      </div>
      {error && <div className="file-panel-error">{error}</div>}
      {servers.length === 0 ? (
        <div className="file-empty">暂无服务器，请在设置页添加 SSH 服务器。</div>
      ) : (
        <div className="server-list">
          {servers.map((s) => (
            <ServerItem
              key={s.name}
              server={s}
              connected={connected.has(s.name)}
              expanded={active === s.name && connected.has(s.name)}
              busy={busy}
              onConnect={() => void connect(s.name)}
              onDisconnect={() => void disconnect(s.name)}
              onToggle={() =>
                connected.has(s.name)
                  ? setActive(active === s.name ? "" : s.name)
                  : void connect(s.name)
              }
            />
          ))}
        </div>
      )}
    </aside>
  );
}

/** 单台服务器：头部（状态点 + 名称 + 连接/断开）+ 展开后的目录树 + sh 输入。 */
function ServerItem({
  server,
  connected,
  expanded,
  busy,
  onConnect,
  onDisconnect,
  onToggle,
}: {
  server: ServerConfig;
  connected: boolean;
  expanded: boolean;
  busy: boolean;
  onConnect: () => void;
  onDisconnect: () => void;
  onToggle: () => void;
}) {
  return (
    <div className="server-item">
      <div className="server-head" onClick={onToggle}>
        <span className={`dot ${connected ? "on" : ""}`} />
        <span className="server-name" title={server.description || server.name}>
          {server.name}
        </span>
        <span className="server-host">
          {server.username}@{server.host}
        </span>
        <span className="spacer" />
        {connected ? (
          <button
            className="icon-btn"
            title="断开"
            onClick={(e) => {
              e.stopPropagation();
              onDisconnect();
            }}
          >
            ⏏
          </button>
        ) : (
          <button
            className="icon-btn"
            title="连接"
            disabled={busy}
            onClick={(e) => {
              e.stopPropagation();
              onConnect();
            }}
          >
            ⚡
          </button>
        )}
      </div>
      {expanded && (
        <>
          <RemoteTree server={server.name} />
          <ShConsole server={server.name} />
        </>
      )}
    </div>
  );
}

/** 远程目录树（懒加载；文件行内操作：下载/删除；目录头部：上传到此目录）。 */
function RemoteTree({ server }: { server: string }) {
  const [dirs, setDirs] = useState<Map<string, DirEntry[]>>(new Map());
  const [expanded, setExpanded] = useState<Set<string>>(new Set(["/"]));
  const [cwd, setCwd] = useState("/");
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);

  const loadDir = useCallback(
    async (path: string) => {
      try {
        const list = await api.sshListDir(server, path || "/");
        setDirs((prev) => new Map(prev).set(path, list));
      } catch (e) {
        setError(String(e));
      }
    },
    [server],
  );

  useEffect(() => {
    setDirs(new Map());
    setExpanded(new Set(["/"]));
    setCwd("/");
    setError("");
    void loadDir("/");
  }, [loadDir]);

  const toggle = (path: string, isDir: boolean) => {
    if (!isDir) return;
    setCwd(path);
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });
    if (!dirs.has(path)) void loadDir(path);
  };

  const upload = async (dir: string) => {
    const picked = await pickFile();
    if (!picked) return;
    setError("");
    try {
      const name = picked.split(/[\\/]/).pop() ?? "upload.bin";
      const remote = `${dir.replace(/\/$/, "")}/${name}`;
      await api.sshUpload(server, picked, remote);
      void loadDir(dir);
    } catch (e) {
      setError(String(e));
    }
  };

  const download = async (dir: string, name: string) => {
    const picked = await pickSavePath(name);
    if (!picked) return;
    setError("");
    try {
      const remote = `${dir.replace(/\/$/, "")}/${name}`;
      await api.sshDownload(server, remote, picked);
    } catch (e) {
      setError(String(e));
    }
  };

  const remove = async (dir: string, entry: DirEntry) => {
    if (!window.confirm(`删除远程${entry.is_dir ? "目录" : "文件"} ${dir}/${entry.name}？`)) return;
    setError("");
    try {
      // 目录删除走 sh rm -rf（SFTP 只支持单文件）；文件走 SFTP。
      const remote = `${dir.replace(/\/$/, "")}/${entry.name}`;
      if (entry.is_dir) {
        await api.sshExec(server, `rm -rf ${JSON.stringify(remote)}`);
      } else {
        await api.sshDeleteFile(server, remote);
      }
      void loadDir(dir);
    } catch (e) {
      setError(String(e));
    }
  };

  const renderLevel = (path: string, depth: number): React.ReactNode => {
    const list = dirs.get(path);
    return (
      <>
        {(list ?? []).map((e) => {
          const child = path === "/" ? `/${e.name}` : `${path}/${e.name}`;
          const open = expanded.has(child);
          return (
            <div key={child}>
              <div
                className="file-row"
                style={{ paddingLeft: 8 + depth * 12 }}
                onClick={() => toggle(child, e.is_dir)}
              >
                <span className="file-icon">{e.is_dir ? (open ? "📂" : "📁") : "📄"}</span>
                <span className="file-name" title={e.name}>
                  {e.name}
                </span>
                <span className="file-size">{e.is_dir ? "" : fmtSize(e.size)}</span>
                <span className="file-actions">
                  {!e.is_dir && (
                    <>
                      <button
                        className="icon-btn"
                        title="下载"
                        onClick={(ev) => {
                          ev.stopPropagation();
                          void download(path, e.name);
                        }}
                      >
                        ⬇
                      </button>
                      <button
                        className="icon-btn"
                        title="删除"
                        onClick={(ev) => {
                          ev.stopPropagation();
                          void remove(path, e);
                        }}
                      >
                        🗑
                      </button>
                    </>
                  )}
                </span>
              </div>
              {e.is_dir && open && renderLevel(child, depth + 1)}
            </div>
          );
        })}
        {list && list.length === 0 && (
          <div className="file-empty" style={{ paddingLeft: 8 + depth * 12 }}>
            （空目录）
          </div>
        )}
        {list === undefined && (
          <div className="file-empty" style={{ paddingLeft: 8 + depth * 12 }}>
            加载中…
          </div>
        )}
      </>
    );
  };

  return (
    <div className="remote-tree">
      <div className="remote-cwd">
        <span className="mono">{cwd}</span>
        <span className="spacer" />
        <button className="icon-btn" title={`上传文件到 ${cwd}`} onClick={() => void upload(cwd)}>
          ⬆
        </button>
        <button
          className="icon-btn"
          title="刷新"
          onClick={() => {
            setLoading(true);
            void loadDir(cwd).finally(() => setLoading(false));
          }}
        >
          ⟳
        </button>
      </div>
      {error && <div className="file-panel-error">{error}</div>}
      <div className="file-tree">{loading ? "加载中…" : renderLevel("/", 0)}</div>
    </div>
  );
}

/** sh 模式：单行命令输入 + 输出回显（用户手动执行，无需确认）。 */
function ShConsole({ server }: { server: string }) {
  const [cmd, setCmd] = useState("");
  const [history, setHistory] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);

  const run = async () => {
    if (!cmd.trim() || busy) return;
    setBusy(true);
    try {
      const r = await api.sshExec(server, cmd);
      setHistory((prev) =>
        [...prev, `$ ${cmd}`, r.output || `(exit ${r.exit_code})`, ""].slice(-200),
      );
      setCmd("");
    } catch (e) {
      setHistory((prev) => [...prev, `$ ${cmd}`, String(e), ""].slice(-200));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="sh-console">
      <div className="sh-out mono">
        {history.length === 0 ? "sh 模式：输入命令直接在服务器执行。\n" : history.join("\n")}
      </div>
      <div className="sh-in">
        <span className="mono">$</span>
        <input
          className="mono"
          value={cmd}
          placeholder="如：df -h / free -m / tail -100 /var/log/syslog"
          onChange={(e) => setCmd(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") void run();
          }}
          disabled={busy}
        />
        <button className="btn" onClick={() => void run()} disabled={busy || !cmd.trim()}>
          Run
        </button>
      </div>
    </div>
  );
}

/** 本地文件选择（Tauri dialog 插件；返回绝对路径）。 */
async function pickFile(): Promise<string | null> {
  const picked = await openFileDialog({ multiple: false, directory: false });
  return typeof picked === "string" ? picked : null;
}

/** 本地保存路径（下载用；默认文件名）。 */
async function pickSavePath(defaultName: string): Promise<string | null> {
  const picked = await saveFileDialog({ defaultPath: defaultName });
  return typeof picked === "string" ? picked : null;
}

export { PAGE_LIMIT };
