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
  // 已打过标题的条目：工具结果到达时整段文本被替换，若按「文本是否变化」
  // 判定标题会重复打出，旧内容也会重复渲染。
  const titled = useRef(new Set<string>());
  const outputQueue = useRef<string[]>([]);
  const [sending, setSending] = useState(false);
  const inputState = useRef({ busy: busy || sending, onAgentInput });
  inputState.current = { busy: busy || sending, onAgentInput };
  // Agent 模式下的当前输入行：终端里不再逐字回显（回显会与真实 shell 输出
  // 打架，且与下方「输入」分区重复显示），改由终端下方的输入栏显示。
  const [agentLine, setAgentLine] = useState("");
  const [terminalGeneration, setTerminalGeneration] = useState(0);
  useEffect(() => {
    const term = terminal.current;
    if (!term) return;
    for (const line of lines) {
      const previous = seen.current.get(line.id) ?? "";
      if (line.text === previous) continue;
      const growing = line.text.startsWith(previous);
      const delta = growing ? line.text.slice(previous.length) : line.text;
      const styles = { input: ["36", "输入"], thinking: ["90", "思考"], reply: ["32", "回复"], tool: ["34", "工具执行"], confirm: ["33", "待授权"] };
      const [color, label] = styles[line.kind ?? "reply"];
      if (!titled.current.has(line.id)) {
        titled.current.add(line.id);
        outputQueue.current.push(`\r\n\x1b[${color}m── ${label} ──\x1b[0m\r\n`);
      }
      const text = terminalPlainText(delta).replace(/\n/g, "\r\n");
      for (const char of text.match(/\r\n|[^]/gu) ?? []) outputQueue.current.push(`\x1b[${color}m${char}\x1b[0m`);
      seen.current.set(line.id, line.text);
    }
  }, [lines, terminalGeneration]);
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
    titled.current.clear();
    outputQueue.current = [];
    setAgentLine("");
    setTerminalGeneration((generation) => generation + 1);
    const typing = window.setInterval(() => {
      const queue = outputQueue.current;
      if (queue.length) term.write(queue.splice(0, Math.max(4, Math.ceil(queue.length / 30))).join(""));
    }, 16);
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
      // 先冲刷 Agent 文本队列再写 PTY 字节：否则先到的 Agent 文本可能被后到的
      // shell 输出抢先显示；此处仅保证写入顺序，不隔离两种输出的光标状态。
      if (event.data.length) {
        const pending = outputQueue.current;
        if (pending.length) term.write(pending.splice(0).join(""));
        term.write(new Uint8Array(event.data));
      }
      if (event.closed) { ended = true; setStatus("已断开，可重新连接"); }
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
    let inputLine = "";
    const input = term.onData((data) => {
      if (!modeRef.current) { writeInput.current(data); return; }
      if (data === "\u0003") {
        inputLine = "";
        setAgentLine("");
        term.write("^C\r\n");
        writeInput.current(data);
        return;
      }
      // 草稿独立显示，可在输出期间编辑；忙碌时只阻止提交，不丢弃按键。
      if (data === "\r") {
        const text = inputLine.trim();
        if (!text || inputState.current.busy) return;
        inputLine = "";
        setAgentLine("");
        term.write("\r\n");
        if (routeTerminalInput(text) === "pty") writeInput.current(`${text}\r`);
        else {
          setSending(true);
          inputState.current.busy = true;
          void inputState.current.onAgentInput(text)
            .catch((e) => setStatus(String(e)))
            .finally(() => { setSending(false); });
        }
        return;
      }
      if (data === "\u007f" || data === "\b") {
        const chars = Array.from(inputLine);
        if (chars.length) {
          chars.pop();
          inputLine = chars.join("");
          setAgentLine(inputLine);
        }
        return;
      }
      // 粘贴不执行换行；箭头等控制序列不转发给远端 shell。
      if (data.startsWith("\u001b")) return;
      const text = terminalPlainText(data).replace(/[\r\n\t]/g, " ");
      inputLine += text;
      // 回显到终端下方的输入栏，而非终端缓冲区：终端里回显会与真实 shell
      // 输出交错，且同一条输入还会作为「输入」分区再渲染一次。
      setAgentLine(inputLine);
    });
    setStatus("连接中…");
    void invoke<string>("ssh_terminal_open", { workspaceId, name: server, cols: term.cols, rows: term.rows, output })
      .then((value) => {
        id = value;
        if (disposed) void invoke("ssh_terminal_close", { id }).catch(() => {});
        else if (!ended) { setStatus("已连接"); resize(); term.focus(); }
      }).catch((e) => { if (!disposed) setStatus(String(e)); });
    return () => {
      disposed = true;
      observer.disconnect();
      window.clearInterval(typing);
      outputQueue.current = [];
      input.dispose();
      terminal.current = null;
      writeInput.current = () => {};
      term.dispose();
      if (id) void invoke("ssh_terminal_close", { id }).catch(() => {});
    };
  }, [workspaceId, server, attempt]);
  return <div className="ops-terminal">
    <div className="ops-terminal-status"><span>{status}</span><button className="btn ghost" onClick={() => setAttempt((n) => n + 1)}>重开终端</button></div>
    <div className="ops-terminal-host" ref={host} />
    {agentMode && <div className="sh-in">
      <span className="sh-in-label">AI 幕布</span>
      <span className="sh-in-echo">{agentLine}</span>
      <span className="sh-in-hint">点击终端直接输入，Enter 提交 · 思考 / 回复 / 工具执行分区标记</span>
    </div>}
  </div>;
}
