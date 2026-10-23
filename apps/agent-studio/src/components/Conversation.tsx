// 会话组件：消息流 + composer + 事件归流渲染。
// 事件处理对齐 Java AgentSessionView.apply 的思路：增量 apply 事件维护视图，
// AgentResult 为最终消息的权威来源，流式 delta 仅作过程展示。

import { useCallback, useEffect, useRef, useState } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import {
  api,
  onAgentEvent,
  onTurnFinished,
  onTurnStarted,
  type AgentConfig,
  type AgentEventPayload,
  type SessionSummary,
} from "../api";

type Entry =
  | { kind: "msg"; id: string; role: "user" | "assistant" | "system"; text: string; ts?: string }
  | { kind: "tool"; id: string; callId: string; name: string; args: string; result?: string; ok?: boolean }
  | { kind: "thinking"; id: string; text: string }
  | { kind: "image"; id: string; dataUrl: string }
  | { kind: "file"; id: string; name: string }
  | { kind: "indicator"; id: string; text: string };

let entrySeq = 0;
const nextId = () => `e${++entrySeq}`;

function extractText(content: unknown): string {
  if (!Array.isArray(content)) return "";
  return content
    .filter((b) => (b as { type?: string })?.type === "text")
    .map((b) => (b as { text?: string }).text ?? "")
    .join("");
}

function shortJson(value: unknown, max = 300): string {
  try {
    const s = typeof value === "string" ? value : JSON.stringify(value);
    return s.length > max ? s.slice(0, max) + " …" : s;
  } catch {
    return String(value);
  }
}

/** 把历史消息的 ContentBlock 列表转成渲染条目（块级 transcript 回放）。 */
function entriesFromBlocks(
  role: "user" | "assistant" | "system",
  blocks: Array<Record<string, unknown>>,
): Entry[] {
  const out: Entry[] = [];
  for (const b of blocks) {
    const type = String(b.type ?? "");
    if (type === "text") {
      const text = String(b.text ?? "");
      if (text.trim()) out.push({ kind: "msg", role, text, id: nextId() });
    } else if (type === "thinking") {
      // 注意字段名：ContentBlock::Thinking 的 serde 键是 thinking（非 text）。
      const text = String(b.thinking ?? "");
      if (text.trim()) out.push({ kind: "thinking", text, id: nextId() });
    } else if (type === "image") {
      const src = b.source as { media_type?: string; data?: string } | undefined;
      if (src?.data) {
        out.push({
          kind: "image",
          dataUrl: `data:${src.media_type ?? "image/png"};base64,${src.data}`,
          id: nextId(),
        });
      }
    } else if (type === "data") {
      out.push({ kind: "file", name: String(b.name ?? "attachment"), id: nextId() });
    } else if (type === "tool_use") {
      out.push({
        kind: "tool",
        callId: String(b.id ?? ""),
        name: String(b.name ?? "tool"),
        args: shortJson(b.input ?? {}, 600),
        id: nextId(),
      });
    } else if (type === "tool_result") {
      const output = Array.isArray(b.output) ? b.output : [];
      const text = output
        .filter((o) => (o as { type?: string })?.type === "text")
        .map((o) => (o as { text?: string }).text ?? "")
        .join("\n");
      out.push({
        kind: "tool",
        callId: String(b.id ?? ""),
        name: String(b.name ?? "tool"),
        args: "",
        result: text || shortJson(output, 600),
        ok: String(b.state ?? "success") === "success",
        id: nextId(),
      });
    }
    // 其余块类型（hint/audio/video/…）历史回放暂不渲染
  }
  return out;
}

export default function Conversation({
  agent,
  runtimeMode,
  onModeChange,
  onBack,
}: {
  agent: AgentConfig;
  /** 运行方式（solo/team）：协同类可切换，切换即重开会话（新实例）。 */
  runtimeMode: "solo" | "team";
  onModeChange: (mode: "solo" | "team") => void;
  onBack: () => void;
}) {
  // 协同类 agent 的配置 mode 固定 "solo"（类别语义）；ops/db 无运行方式切换。
  const isCollab = agent.mode === "solo";
  // 会话管理：默认 main（team 运行用独立默认会话，与 solo 空间分开）；
  // 可切换历史会话或新建（对齐 service 的多会话语义）。
  const [sessionId, setSessionId] = useState(runtimeMode === "team" ? "team-main" : "main");
  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  const [entries, setEntries] = useState<Entry[]>([]);
  const [input, setInput] = useState("");
  // 待发送附件（base64 内联；preview 仅图片用于本地预览）。
  const [attachments, setAttachments] = useState<
    Array<{ name: string; media_type: string; data_base64: string; preview?: string }>
  >([]);
  const [busy, setBusy] = useState(false);
  const [planActive, setPlanActive] = useState(false);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const streamRef = useRef<string | null>(null); // 当前流式 assistant entry id
  const thinkRef = useRef<string | null>(null);
  const toolRef = useRef<Map<string, string>>(new Map()); // callKey -> entry id
  const scrollRef = useRef<HTMLDivElement>(null);

  const upsert = useCallback((id: string, patch: (e: Entry) => Entry) => {
    setEntries((prev) => prev.map((e) => (e.id === id ? patch(e) : e)));
  }, []);

  const reloadSessions = useCallback(async () => {
    try {
      setSessions(await api.listSessions(agent.id));
    } catch {
      /* 会话发现失败不阻塞主流程 */
    }
  }, [agent.id]);

  // 下拉选项：已落盘会话 + 当前会话（新建未发消息时尚未落盘，补临时项）。
  const sessionOptions = sessions.some((s) => s.session_id === sessionId)
    ? sessions
    : [{ session_id: sessionId, messages: 0, updated_at: 0 }, ...sessions];

  // 初始加载：历史（块级回放）+ plan 状态 + 会话列表（重启后从 AgentState 恢复）。
  useEffect(() => {
    let alive = true;
    setEntries([]);
    streamRef.current = null;
    thinkRef.current = null;
    toolRef.current.clear();
    setBusy(false);
    api.getHistory(agent.id, runtimeMode, sessionId).then((history) => {
      if (!alive) return;
      const restored: Entry[] = [];
      for (const h of history) {
        const role = (h.role === "user" || h.role === "assistant" ? h.role : "system") as
          | "user"
          | "assistant"
          | "system";
        if (h.blocks?.length) {
          restored.push(...entriesFromBlocks(role, h.blocks));
        } else if (h.text) {
          restored.push({ kind: "msg", role, text: h.text, ts: h.timestamp, id: nextId() });
        }
      }
      setEntries(restored);
    });
    api.getPlanStatus(agent.id, runtimeMode, sessionId).then((s) => alive && setPlanActive(s.active));
    void reloadSessions();
    return () => {
      alive = false;
    };
  }, [agent.id, sessionId, runtimeMode, reloadSessions]);

  // 事件归流：只处理 (agent_id, session_id) 匹配本组件的事件。
  useEffect(() => {
    const applies = (p: { agent_id: string; session_id: string }) =>
      p.agent_id === agent.id && p.session_id === sessionId;

    const un1 = onAgentEvent((payload: AgentEventPayload) => {
      if (!applies(payload)) return;
      const ev = payload.event;
      switch (ev?.type) {
        case "MODEL_CALL_START":
          setEntries((prev) => [...prev, { kind: "indicator", text: "thinking…", id: nextId() }]);
          break;
        case "MODEL_CALL_END":
          setEntries((prev) => prev.filter((e) => e.kind !== "indicator"));
          break;
        case "TEXT_BLOCK_DELTA": {
          const delta = String(ev.delta ?? "");
          if (!delta) break;
          if (!streamRef.current) {
            const id = nextId();
            streamRef.current = id;
            setEntries((prev) => [...prev, { kind: "msg", role: "assistant", text: delta, id }]);
          } else {
            const id = streamRef.current;
            upsert(id, (e) => (e.kind === "msg" ? { ...e, text: e.text + delta } : e));
          }
          break;
        }
        case "THINKING_BLOCK_DELTA": {
          const delta = String(ev.delta ?? "");
          if (!delta) break;
          if (!thinkRef.current) {
            const id = nextId();
            thinkRef.current = id;
            setEntries((prev) => [...prev, { kind: "thinking", text: delta, id }]);
          } else {
            const id = thinkRef.current;
            upsert(id, (e) => (e.kind === "thinking" ? { ...e, text: e.text + delta } : e));
          }
          break;
        }
        case "TOOL_CALL_START": {
          const name = String(ev.name ?? ev.tool_name ?? "tool");
          const args = shortJson(ev.arguments ?? ev.input ?? ev.args ?? "");
          const id = nextId();
          const callKey = String(ev.call_id ?? ev.id ?? `${name}-${id}`);
          toolRef.current.set(callKey, id);
          setEntries((prev) => [
            ...prev,
            { kind: "tool", callId: callKey, name, args, id },
          ]);
          break;
        }
        case "TOOL_RESULT_END": {
          const callKey = String(ev.call_id ?? ev.id ?? "");
          const entryId = toolRef.current.get(callKey);
          const output = ev.output ?? ev.result ?? ev.content;
          const text =
            typeof output === "string" ? output : shortJson(output, 1200);
          if (entryId) {
            upsert(entryId, (e) =>
              e.kind === "tool" ? { ...e, result: text, ok: ev.is_error !== true } : e,
            );
          }
          break;
        }
        case "AGENT_RESULT": {
          // 最终消息是权威来源：替换流式缓冲。
          const result = ev.result as { content?: unknown } | undefined;
          const text = extractText(result?.content);
          const streamingId = streamRef.current;
          streamRef.current = null;
          thinkRef.current = null;
          if (text) {
            if (streamingId) {
              upsert(streamingId, (e) => (e.kind === "msg" ? { ...e, text } : e));
            } else {
              setEntries((prev) => [...prev, { kind: "msg", role: "assistant", text, id: nextId() }]);
            }
          }
          break;
        }
        case "AGENT_END":
          streamRef.current = null;
          thinkRef.current = null;
          break;
        default:
          break;
      }
    });

    const un2 = onTurnFinished((payload) => {
      if (!applies(payload)) return;
      setBusy(false);
      if (payload.outcome === "failed") {
        setError(payload.error ?? "turn failed");
      }
      void reloadSessions(); // 消息已落盘，刷新会话摘要
    });

    // steer 续轮：后端自动开启的新 turn（前端保持 busy、提示用户）。
    const un3 = onTurnStarted((payload) => {
      if (!applies(payload)) return;
      setBusy(true);
      setNotice("Continuing with your steering input…");
      setTimeout(() => setNotice(""), 3000);
    });

    return () => {
      un1.then((f) => f());
      un2.then((f) => f());
      un3.then((f) => f());
    };
  }, [agent.id, sessionId, upsert, reloadSessions]);

  // 自动滚底。
  useEffect(() => {
    scrollRef.current?.scrollTo({ top: scrollRef.current.scrollHeight });
  }, [entries]);

  // 选附件：FileReader 读 data URL → 拆出 base64（单文件上限 8MB，防 IPC 卡顿）。
  const pickFiles = async (files: FileList | null) => {
    if (!files?.length) return;
    const next: Array<{ name: string; media_type: string; data_base64: string; preview?: string }> = [];
    for (const f of Array.from(files)) {
      if (f.size > 8 * 1024 * 1024) {
        setError(`附件 ${f.name} 超过 8MB，已跳过`);
        continue;
      }
      const dataUrl = await new Promise<string>((resolve, reject) => {
        const r = new FileReader();
        r.onload = () => resolve(String(r.result));
        r.onerror = () => reject(r.error);
        r.readAsDataURL(f);
      });
      const b64 = dataUrl.slice(dataUrl.indexOf(",") + 1);
      next.push({
        name: f.name,
        media_type: f.type || "application/octet-stream",
        data_base64: b64,
        preview: f.type.startsWith("image/") ? dataUrl : undefined,
      });
    }
    setAttachments((prev) => [...prev, ...next]);
  };

  const send = async () => {
    const text = input.trim();
    if ((!text && attachments.length === 0) || busy) return;
    setError("");
    setInput("");
    // 本地立即显示：文本 + 附件（图片缩略 / 文件 chip）。
    const local: Entry[] = [];
    if (text) local.push({ kind: "msg", role: "user", text, id: nextId() });
    for (const a of attachments) {
      local.push(
        a.preview
          ? { kind: "image", dataUrl: a.preview, id: nextId() }
          : { kind: "file", name: a.name, id: nextId() },
      );
    }
    setEntries((prev) => [...prev, ...local]);
    const payload = attachments.map(({ name, media_type, data_base64 }) => ({
      name,
      media_type,
      data_base64,
    }));
    setAttachments([]);
    setBusy(true);
    try {
      await api.sendMessage(agent.id, runtimeMode, sessionId, text, payload);
    } catch (e) {
      setBusy(false);
      setError(String(e));
    }
  };

  // steer：turn 运行中补充指令（排队 → 当前 turn 结算后自动续轮消费）。
  const steer = async () => {
    const text = input.trim();
    if (!text || !busy) return;
    setError("");
    setInput("");
    setEntries((prev) => [...prev, { kind: "msg", role: "user", text, id: nextId() }]);
    try {
      await api.steerTurn(agent.id, runtimeMode, sessionId, text);
      setNotice("Steering queued — the agent will pick it up after the current step.");
      setTimeout(() => setNotice(""), 3000);
    } catch (e) {
      setError(String(e));
    }
  };

  // Enter 发送；busy 时 Enter 转为 steer（运行中也能打字补充指令）。
  const submit = () => (busy ? void steer() : void send());

  const newSession = () => {
    const id = `${runtimeMode}-${crypto.randomUUID().replace(/-/g, "").slice(0, 12)}`;
    setSessionId(id);
    setSessions((prev) => [{ session_id: id, messages: 0, updated_at: 0 }, ...prev]);
  };

  const togglePlan = async () => {
    try {
      const status = await api.setPlanMode(agent.id, runtimeMode, sessionId, !planActive);
      setPlanActive(status.active);
    } catch (e) {
      setError(String(e));
    }
  };

  const interrupt = async () => {
    try {
      await api.interruptTurn(agent.id, runtimeMode, sessionId);
    } catch (e) {
      setError(String(e));
    }
  };

  const clear = async () => {
    try {
      await api.clearSession(agent.id, runtimeMode, sessionId);
      setEntries([]);
      void reloadSessions();
    } catch (e) {
      setError(String(e));
    }
  };

  // plan/free 仅 solo 运行方式下可用（产品语义）。
  const supportsPlan = runtimeMode === "solo";

  // 切换运行方式（solo ↔ team）：以当前 agent 重新进入（App 层换实例重开会话）。
  const switchMode = (next: "solo" | "team") => {
    if (next === runtimeMode) return;
    if (next === "team" && agent.members.length === 0) {
      if (!window.confirm("Team 模式当前未配置成员（lead 将单干）。可在编辑里添加成员。继续切换？")) {
        return;
      }
    }
    onModeChange(next);
  };

  return (
    <div className="conversation">
      <div className="session-bar">
        <button className="btn ghost" onClick={onBack} title="返回工作区列表" disabled={busy}>
          ←
        </button>
        {isCollab && (
          <div className="mode-switch">
            <button
              className={runtimeMode === "solo" ? "active" : ""}
              onClick={() => switchMode("solo")}
              disabled={busy}
              title="单 Agent 独立完成任务（plan/free 可切换）"
            >
              Solo
            </button>
            <button
              className={runtimeMode === "team" ? "active" : ""}
              onClick={() => switchMode("team")}
              disabled={busy}
              title="以该智能体为 lead，与配置的成员协作"
            >
              Team
            </button>
          </div>
        )}
        <select
          className="field"
          style={{ width: 260 }}
          value={sessionId}
          onChange={(e) => setSessionId(e.target.value)}
        >
          {/* 当前会话可能尚未落盘（新建未发消息），补一个临时选项保证选中生效 */}
          {sessionOptions.map((s) => (
            <option key={s.session_id} value={s.session_id}>
              {s.session_id === sessionId
                ? `● ${s.session_id.slice(0, 8)}`
                : `${s.session_id.slice(0, 8)} · ${s.messages} msgs`}
            </option>
          ))}
        </select>
        <button className="btn ghost" onClick={newSession} disabled={busy}>
          + New session
        </button>
        <span className="spacer" />
        <span className="badge">{sessionId.slice(0, 8)}</span>
      </div>
      {error && <div className="notice error" style={{ margin: "10px 20px 0" }}>{error}</div>}
      {notice && <div className="notice info" style={{ margin: "10px 20px 0" }}>{notice}</div>}
      <div className="messages" ref={scrollRef}>
        {entries.length === 0 && (
          <div className="empty">
            <div>Start the conversation with {agent.name}</div>
            <div style={{ fontSize: 12 }}>
              mode: {isCollab ? `collab · ${runtimeMode}` : agent.mode}
              {supportsPlan ? " · plan/free switchable" : ""}
            </div>
          </div>
        )}
        {entries.map((e) => {
          if (e.kind === "msg") {
            return (
              <div key={e.id} className={`msg ${e.role}`}>
                {/* assistant 输出走 markdown 渲染（GFM 表格/任务列表）；
                    user 输入保持纯文本（避免用户打的 * _ 被意外格式化）。 */}
                {e.role === "assistant" ? (
                  <div className="bubble md">
                    <Markdown remarkPlugins={[remarkGfm]}>{e.text}</Markdown>
                  </div>
                ) : (
                  <div className="bubble">{e.text}</div>
                )}
              </div>
            );
          }
          if (e.kind === "thinking") {
            return (
              <div key={e.id} className="thinking-block">
                <div className="thinking-label">💭 思考过程</div>
                <div className="thinking-text md">
                  <Markdown remarkPlugins={[remarkGfm]}>{e.text}</Markdown>
                </div>
              </div>
            );
          }
          if (e.kind === "image") {
            return (
              <div key={e.id} className="msg user">
                <img className="msg-img" src={e.dataUrl} alt="attachment" />
              </div>
            );
          }
          if (e.kind === "file") {
            return (
              <div key={e.id} className="msg user">
                <div className="bubble file-chip">📎 {e.name}</div>
              </div>
            );
          }
          if (e.kind === "indicator") {
            return (
              <div key={e.id} className="turn-indicator">
                {e.text}
              </div>
            );
          }
          return (
            <div key={e.id} className="tool-block">
              <div className="tool-head">
                <span>🛠</span>
                <span className="tool-name">{e.name}</span>
                <span className={`status ${e.ok === undefined ? "" : e.ok ? "ok" : "err"}`}>
                  {e.ok === undefined ? "running…" : e.ok ? "done" : "error"}
                </span>
              </div>
              <div className="tool-body">
                {e.args && `in: ${e.args}\n`}
                {e.result !== undefined && `out: ${e.result}`}
              </div>
            </div>
          );
        })}
      </div>
      {attachments.length > 0 && (
        <div className="attach-chips">
          {attachments.map((a, i) => (
            <span key={`${a.name}-${i}`} className="chip">
              {a.preview ? (
                <img src={a.preview} alt={a.name} />
              ) : (
                <span className="chip-icon">📎</span>
              )}
              <span className="chip-name" title={a.name}>
                {a.name}
              </span>
              <button
                className="chip-x"
                title="移除"
                onClick={() => setAttachments((prev) => prev.filter((_, j) => j !== i))}
              >
                ×
              </button>
            </span>
          ))}
        </div>
      )}
      <div className="composer">
        {supportsPlan && (
          <button className="btn ghost" onClick={togglePlan} disabled={busy}>
            {planActive ? "Switch to free" : "Switch to plan"}
          </button>
        )}
        {planActive && <span className="badge plan" style={{ alignSelf: "center" }}>PLAN MODE</span>}
        <label className="btn ghost attach-btn" title="添加附件（图片/文件，≤8MB）">
          📎
          <input
            type="file"
            multiple
            hidden
            onChange={(e) => {
              void pickFiles(e.target.files);
              e.currentTarget.value = "";
            }}
          />
        </label>
        <textarea
          value={input}
          onChange={(e) => setInput(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              submit();
            }
          }}
          placeholder={
            busy
              ? "Type to steer the running turn… (Enter to steer)"
              : "Type a message… (Enter to send, Shift+Enter for newline)"
          }
        />
        {busy ? (
          <button className="btn warn" onClick={() => void steer()} disabled={!input.trim()}>
            Steer
          </button>
        ) : (
          <button className="btn" onClick={() => void send()} disabled={!input.trim()}>
            Send
          </button>
        )}
        {busy && (
          <button className="btn danger" onClick={interrupt}>
            Stop
          </button>
        )}
        <button className="btn ghost" onClick={clear} title="Clear session context">
          Clear
        </button>
      </div>
    </div>
  );
}
