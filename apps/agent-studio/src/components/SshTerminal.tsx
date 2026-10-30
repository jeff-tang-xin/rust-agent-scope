import { useEffect, useRef, useState } from "react";
import { Channel, invoke } from "@tauri-apps/api/core";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import { routeTerminalInput, terminalPlainText, type TerminalAgentLine } from "./terminalRouting";

export default function SshTerminal({ workspaceId, server, agentMode, busy, lines, onAgentInput }: {
  workspaceId: string; server: string; agentMode: boolean; busy: boolean;
  lines: TerminalAgentLine[]; onAgentInput: (text: string) => Promise<void>;
}) {
  const terminal = useRef<Terminal | null>(null);
  const writeInput = useRef<(data: string) => void>(() => {});
  const modeRef = useRef(agentMode);
  modeRef.current = agentMode;
  const seen = useRef(new Map<string, string>());
  const [draft, setDraft] = useState("");
  const [sending, setSending] = useState(false);
  const [live, setLive] = useState(false);
  const submit = async () => {
    const text = draft.trim();
    if (!text || busy || sending || !live) return;
    setSending(true);
    try {
      if (routeTerminalInput(text) === "pty") writeInput.current(`${text}\r`);
      else await onAgentInput(text);
      setDraft("");
    } catch (e) { setStatus(String(e)); }
    finally { setSending(false); }
  };
  useEffect(() => {
    const term = terminal.current;
    if (!term) return;
    for (const line of lines) {
      const previous = seen.current.get(line.id) ?? "";
      if (line.text === previous) continue;
      const delta = line.text.startsWith(previous) ? line.text.slice(previous.length) : line.text;
      term.write(`${previous && line.text.startsWith(previous) ? "" : "\r\n"}${terminalPlainText(delta).replace(/\n/g, "\r\n")}`);
      seen.current.set(line.id, line.text);
    }
  }, [lines, live]);
  const host = useRef<HTMLDivElement>(null);
  const [attempt, setAttempt] = useState(0);
  const [status, setStatus] = useState("连接中…");
  useEffect(() => {
    if (!host.current) return;
    let disposed = false;
    let id = "";
    let ended = false;
    const term = new Terminal({ cursorBlink: true, fontSize: 13, scrollback: 5000, theme: { background: "#111827" } });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(host.current);
    terminal.current = term;
    seen.current.clear();
    setLive(false);
    const resize = () => {
      if (!host.current?.clientWidth || !host.current.clientHeight) return;
      fit.fit();
      if (id && !ended) void invoke("ssh_terminal_resize", { id, cols: term.cols, rows: term.rows }).catch(() => {});
    };
    resize();
    const observer = new ResizeObserver(resize);
    observer.observe(host.current);
    const output = new Channel<{ data: number[]; closed: boolean }>();
    output.onmessage = (event) => {
      if (disposed) return;
      if (event.data.length) term.write(new Uint8Array(event.data));
      if (event.closed) { ended = true; setLive(false); setStatus("已断开，可重新连接"); }
    };
    let queue = Promise.resolve();
    writeInput.current = (data) => {
      if (!id || ended) return;
      // 保序，长粘贴分片，避免 IPC 大包。
      for (let i = 0; i < data.length; i += 4096) {
        const chunk = data.slice(i, i + 4096);
        queue = queue.then(async () => {
          if (!disposed && !ended) await invoke("ssh_terminal_input", { id, data: chunk });
        }).catch((e) => { if (!disposed) setStatus(String(e)); });
      }
    };
    const input = term.onData((data) => {
      if (!modeRef.current || data === "\u0003") writeInput.current(data);
    });
    setStatus("连接中…");
    void invoke<string>("ssh_terminal_open", { workspaceId, name: server, cols: term.cols, rows: term.rows, output })
      .then((value) => {
        id = value;
        if (disposed) void invoke("ssh_terminal_close", { id }).catch(() => {});
        else if (!ended) { setLive(true); setStatus("已连接"); resize(); term.focus(); }
      }).catch((e) => { if (!disposed) setStatus(String(e)); });
    return () => {
      disposed = true;
      observer.disconnect();
      input.dispose();
      term.dispose();
      if (id) void invoke("ssh_terminal_close", { id }).catch(() => {});
    };
  }, [workspaceId, server, attempt]);
  return <div className="ops-terminal">
    <div className="ops-terminal-status"><span>{status}</span><button className="btn ghost" onClick={() => setAttempt((n) => n + 1)}>重开终端</button></div>
    <div className="ops-terminal-host" ref={host} />
    {agentMode && <form className="sh-in" onSubmit={(e) => { e.preventDefault(); void submit(); }}>
      <span>👤</span><input aria-label="AI 终端输入" value={draft} onChange={(e) => setDraft(e.target.value)}
        placeholder="普通命令直通 · 危险命令 / 自然语言交给 Agent" disabled={!live || sending}
        onKeyDown={(e) => { if (e.key === "Enter" && e.nativeEvent.isComposing) e.preventDefault(); }} />
      <button className="btn" disabled={!live || busy || sending || !draft.trim()}>执行</button>
      <button type="button" className="btn ghost" onClick={() => writeInput.current("\u0003")}>Ctrl+C</button>
    </form>}
  </div>;
}
