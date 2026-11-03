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
    void api.listServers().then((all) => {
      if (!alive) return;
      const list = workspace.bound_servers?.length ? all.filter((s) => workspace.bound_servers.includes(s.name)) : all;
      setServers(list);
    }).catch((e) => { if (alive) setError(String(e)); });
    return () => { alive = false; };
  }, [workspace.bound_servers]);
  // 服务器即工作区：会话绑定的服务器自动连接并打开终端，无需手动点连。
  useEffect(() => {
    if (!server) return;
    let alive = true;
    void (async () => {
      try {
        if (!await api.sshStatus(server)) await api.sshConnect(server);
        if (!alive) return;
        setConnected((prev) => prev.includes(server) ? prev : [...prev, server]);
        setOpened((prev) => prev.includes(server) ? prev : [...prev, server]);
      } catch (e) { if (alive) setError(String(e)); }
    })();
    return () => { alive = false; };
  }, [server]);
  // 多服务器绑定的手动工作区：工具栏下拉切换（自动工作区恒为一台，不出现）。
  const select = async (name: string) => {
    if (busy || pending || name === server) return;
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
  const needsApproval = lines.some((line) => line.kind === "confirm");
  return <div className="ops-workbench">
    <section className="ops-center">
      <div className="ops-toolbar"><div className="mode-switch">
        <button className={mode === "agent" ? "active" : ""} onClick={() => setMode("agent")}>Agent</button>
        <button className={mode === "shell" ? "active" : ""} onClick={() => setMode("shell")}>Shell</button>
      </div>
        {servers.length > 1 ? (
          <select className="ops-server-select" value={server} disabled={busy || pending}
            onChange={(e) => void select(e.target.value)}
            title="切换该工作区绑定的服务器">
            {servers.map((s) => <option key={s.name} value={s.name}>{s.name}</option>)}
          </select>
        ) : <span>{server || "未绑定服务器"}</span>}
        {pending && <span>连接中…</span>}
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
        {!ready && <div className="file-empty">{server ? "正在连接服务器…" : "该工作区未绑定服务器，请在设置中绑定。"}</div>}
      </div>
      <div className="ops-terminal-controls" aria-live="polite">
        {needsApproval && <div className="file-panel-head" role="status">有工具等待授权，请在下方批准或拒绝</div>}
        {children}
      </div>
    </section>
    <aside className="file-panel ops-files"><div className="file-panel-head">远程文件 · {server || "未连接"}</div>
      {ready ? <RemoteTree key={server} server={server} onConfirm={onConfirm} /> : <div className="file-empty">连接服务器后浏览、上传和下载文件。</div>}
    </aside>
  </div>;
}
