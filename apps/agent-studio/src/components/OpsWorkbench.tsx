import { lazy, Suspense, useEffect, useState, type ReactNode } from "react";
import { api, type ServerConfig, type WorkspaceConfig } from "../api";
import { RemoteTree } from "./ServersPanel";
const SshTerminal = lazy(() => import("./SshTerminal"));

export default function OpsWorkbench({ workspace, server, busy, onSelect, onConfirm, children, lines, onAgentInput }: {
  workspace: WorkspaceConfig;
  server: string;
  busy: boolean;
  onSelect: (name: string) => Promise<void>;
  onConfirm?: (title: string, message: string) => Promise<boolean>;
  children: ReactNode;
  lines: import("./terminalRouting").TerminalAgentLine[];
  onAgentInput: (text: string) => Promise<void>;
}) {
  const [servers, setServers] = useState<ServerConfig[]>([]);
  const [connected, setConnected] = useState<string[]>([]);
  const [opened, setOpened] = useState<string[]>([]);
  const [mode, setMode] = useState<"agent" | "shell">("shell");
  const [pending, setPending] = useState(false);
  const [error, setError] = useState("");
  useEffect(() => {
    let alive = true;
    void api.listServers().then(async (all) => {
      const list = workspace.bound_servers?.length ? all.filter((s) => workspace.bound_servers.includes(s.name)) : all;
      const states = await Promise.all(list.map((s) => api.sshStatus(s.name)));
      if (alive) { setServers(list); setConnected(list.filter((_, i) => states[i]).map((s) => s.name)); }
    }).catch((e) => { if (alive) setError(String(e)); });
    return () => { alive = false; };
  }, [workspace.bound_servers]);
  const select = async (name: string) => {
    if (busy || pending) return;
    setPending(true); setError("");
    try {
      if (!await api.sshStatus(name)) await api.sshConnect(name);
      await onSelect(name);
      setConnected((prev) => prev.includes(name) ? prev : [...prev, name]);
    } catch (e) { setError(String(e)); }
    finally { setPending(false); }
  };
  const ready = !!server && connected.includes(server) && servers.some((s) => s.name === server);
  useEffect(() => {
    if (ready) setOpened((prev) => prev.includes(server) ? prev : [...prev, server]);
  }, [ready, mode, server]);
  return <div className="ops-workbench">
    <aside className="ops-servers">
      <div className="file-panel-head">服务器</div>
      {servers.map((s) => <button key={s.name} className={`ops-server ${server === s.name ? "active" : ""}`} disabled={busy || pending} onClick={() => void select(s.name)}>
        <strong>{connected.includes(s.name) ? "● " : "○ "}{s.name}</strong><small>{s.username}@{s.host}:{s.port}</small>
      </button>)}
      {!servers.length && <div className="file-empty">暂无服务器，请在设置中添加并绑定。</div>}
      {busy && <div className="file-empty">Agent 运行中，暂不可切换服务器</div>}
    </aside>
    <section className="ops-center">
      <div className="ops-toolbar"><div className="mode-switch">
        <button className={mode === "agent" ? "active" : ""} onClick={() => setMode("agent")}>Agent</button>
        <button className={mode === "shell" ? "active" : ""} onClick={() => setMode("shell")}>Shell</button>
      </div><span>{server || "请选择服务器"}</span>{pending && <span>连接中…</span>}
        {ready && <button className="btn ghost" disabled={busy || pending} onClick={async () => {
          if (!await onConfirm?.("断开服务器", `断开 ${server} 的 SSH 连接？该服务器的其他终端也会断开。`)) return;
          setPending(true);
          try { await api.sshDisconnect(server); setConnected((prev) => prev.filter((name) => name !== server)); setOpened((prev) => prev.filter((name) => name !== server)); }
          catch (e) { setError(String(e)); }
          finally { setPending(false); }
        }}>断开</button>}
      </div>
      {error && <div className="file-panel-error">{error}</div>}
      <div className="ops-mode">
        {opened.filter((name) => connected.includes(name)).map((name) => <div key={name} className={`ops-terminal-slot ${name === server ? "" : "hidden"}`}>
          <Suspense fallback={<div className="file-empty">加载终端…</div>}><SshTerminal workspaceId={workspace.id} server={name} agentMode={mode === "agent"} busy={busy || pending} lines={name === server ? lines : []} onAgentInput={onAgentInput} /></Suspense>
        </div>)}
        {!ready && <div className="file-empty">选择服务器后打开交互式 Shell。</div>}
      </div>
      <div className="ops-terminal-controls">{children}</div>
    </section>
    <aside className="file-panel ops-files"><div className="file-panel-head">远程文件 · {server || "未连接"}</div>
      {ready ? <RemoteTree key={server} server={server} onConfirm={onConfirm} /> : <div className="file-empty">连接服务器后浏览、上传和下载文件。</div>}
    </aside>
  </div>;
}
