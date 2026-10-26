// 会话组件：消息流 + composer + 事件归流渲染（按工作区归流——会话状态
// 属于工作区，同一智能体在不同工作区各有独立会话）。
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
  type WorkspaceConfig,
  type WorkspaceFileEntry,
} from "../api";

type Entry =
  | { kind: "msg"; id: string; role: "user" | "assistant" | "system"; text: string; ts?: string; streaming?: boolean }
  | { kind: "tool"; id: string; callId: string; name: string; args: string; result?: string; ok?: boolean; ts?: string }
  | { kind: "thinking"; id: string; text: string; ts?: string; streaming?: boolean }
  | { kind: "image"; id: string; dataUrl: string; ts?: string }
  | { kind: "file"; id: string; name: string; ts?: string }
  | { kind: "indicator"; id: string; text: string; ts?: string };

let entrySeq = 0;
const nextId = () => `e${++entrySeq}`;

/** 本地时间 HH:MM:SS（时间线节点标签）。 */
const nowTime = () => new Date().toLocaleTimeString("zh-CN", { hour12: false });

/** 历史时间戳（ISO-8601）→ HH:MM:SS；非 ISO 原样返回。 */
function fmtTime(ts?: string): string | undefined {
  if (!ts) return undefined;
  return /^\d{4}-\d{2}-\d{2}T/.test(ts) ? ts.slice(11, 19) : ts;
}

/** 打字机：流式期间按帧渐进显示（自适应步长追平积压），结束后整段显示。 */
function Typewriter({ text, active }: { text: string; active: boolean }) {
  const [shown, setShown] = useState(() => (active ? 0 : text.length));
  useEffect(() => {
    if (!active) {
      setShown(text.length);
      return;
    }
    let raf = 0;
    const tick = () => {
      setShown((prev) => {
        if (prev >= text.length) return prev;
        // 积压越多 reveal 越快：保持打字节奏又不掉队
        const step = Math.max(1, Math.ceil((text.length - prev) / 15));
        return Math.min(text.length, prev + step);
      });
      raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
  }, [text, active]);
  return (
    <>
      <Markdown remarkPlugins={[remarkGfm]}>{text.slice(0, shown)}</Markdown>
      {active && <span className="type-cursor" aria-hidden />}
    </>
  );
}

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

/** 把历史消息的 ContentBlock 列表转成渲染条目（块级 transcript 回放）。
 *  ts 为消息级时间戳（块级无独立时间，统一用所属消息的）。 */
function entriesFromBlocks(
  role: "user" | "assistant" | "system",
  blocks: Array<Record<string, unknown>>,
  ts?: string,
): Entry[] {
  const out: Entry[] = [];
  for (const b of blocks) {
    const type = String(b.type ?? "");
    if (type === "text") {
      const text = String(b.text ?? "");
      if (text.trim()) out.push({ kind: "msg", role, text, ts, id: nextId() });
    } else if (type === "thinking") {
      // 注意字段名：ContentBlock::Thinking 的 serde 键是 thinking（非 text）。
      const text = String(b.thinking ?? "");
      if (text.trim()) out.push({ kind: "thinking", text, ts, id: nextId() });
    } else if (type === "image") {
      const src = b.source as { media_type?: string; data?: string } | undefined;
      if (src?.data) {
        out.push({
          kind: "image",
          dataUrl: `data:${src.media_type ?? "image/png"};base64,${src.data}`,
          ts,
          id: nextId(),
        });
      }
    } else if (type === "data") {
      out.push({ kind: "file", name: String(b.name ?? "attachment"), ts, id: nextId() });
    } else if (type === "tool_use") {
      out.push({
        kind: "tool",
        callId: String(b.id ?? ""),
        name: String(b.name ?? "tool"),
        args: shortJson(b.input ?? {}, 600),
        ts,
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
        ts,
        id: nextId(),
      });
    }
    // 其余块类型（hint/audio/video/…）历史回放暂不渲染
  }
  return out;
}

/** 字节数 → 可读大小。 */
function fmtSize(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}

/** 右侧文件面板：工作区根目录的文件列表（与 agent 文件工具同一锚定目录）。
 *  点击文件预览文本；turn 结束自动刷新（agent 产出文件后列表跟随）。 */
function FilePanel({ workspaceId }: { workspaceId: string }) {
  const [files, setFiles] = useState<WorkspaceFileEntry[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");
  const [preview, setPreview] = useState<{ path: string; content: string } | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError("");
    try {
      setFiles(await api.listWorkspaceFiles(workspaceId));
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, [workspaceId]);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    const un = onTurnFinished((p) => {
      if (p.workspace_id === workspaceId) void load();
    });
    return () => {
      un.then((f) => f());
    };
  }, [workspaceId, load]);

  const open = async (path: string) => {
    try {
      setError("");
      setPreview({ path, content: await api.readWorkspaceFile(workspaceId, path) });
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <aside className="file-panel">
      <div className="file-panel-head">
        <span>文件</span>
        <span className="spacer" />
        <button className="icon-btn" title="刷新" onClick={() => void load()}>
          ⟳
        </button>
      </div>
      {error && <div className="file-panel-error">{error}</div>}
      <div className="file-tree">
        {loading && files.length === 0 && <div className="file-empty">加载中…</div>}
        {!loading && files.length === 0 && <div className="file-empty">（空目录）</div>}
        {files.map((f) => (
          <button
            key={f.path}
            className={`file-row ${f.is_dir ? "is-dir" : ""}`}
            style={{ paddingLeft: 8 + (f.path.split("/").length - 1) * 12 }}
            title={f.path}
            onClick={() => !f.is_dir && void open(f.path)}
          >
            <span className="file-icon">{f.is_dir ? "📁" : "📄"}</span>
            <span className="file-name">{f.path.split("/").pop()}</span>
            {!f.is_dir && f.size > 0 && <span className="file-size">{fmtSize(f.size)}</span>}
          </button>
        ))}
      </div>
      {preview && (
        <div className="modal-mask" onClick={() => setPreview(null)}>
          <div className="modal file-preview" onClick={(e) => e.stopPropagation()}>
            <h2>{preview.path}</h2>
            <pre>{preview.content}</pre>
            <div className="modal-actions">
              <button className="btn ghost" onClick={() => setPreview(null)}>
                关闭
              </button>
            </div>
          </div>
        </div>
      )}
    </aside>
  );
}

export default function Conversation({
  workspace,
  agent,
  runtimeMode,
  onModeChange,
  onBack,
}: {
  /** 会话所属工作区（命令与事件都按工作区归流）。 */
  workspace: WorkspaceConfig;
  /** 工作区选定的智能体（显示名与空态文案用）。 */
  agent: AgentConfig;
  /** 运行方式（solo/team）：协同工作区可切换，切换即重开会话（新实例）。 */
  runtimeMode: "solo" | "team";
  onModeChange: (mode: "solo" | "team") => void;
  onBack: () => void;
}) {
  // 仅协同工作区有 solo/team 运行方式切换；ops/db 工具面由类型决定。
  const isCollab = workspace.kind === "collab";
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
  // 拖拽附件进行中（容器高亮提示）。
  const [dragActive, setDragActive] = useState(false);
  const [planActive, setPlanActive] = useState(false);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  // 当前流式块（按 block_id 分组：同一块追加，块切换定格旧条目开新条目，
  // 保证 思考→工具→思考→输出 按真实时间交错显示）。
  const streamRef = useRef<{ kind: "msg" | "thinking"; blockId: string; entryId: string } | null>(null);
  const toolRef = useRef<Map<string, string>>(new Map()); // callKey -> entry id
  const scrollRef = useRef<HTMLDivElement>(null);

  const upsert = useCallback((id: string, patch: (e: Entry) => Entry) => {
    setEntries((prev) => prev.map((e) => (e.id === id ? patch(e) : e)));
  }, []);

  // 收流：把流式中的 msg/thinking 条目置为完成（打字机停走、光标消失）。
  // 块切换 / 块结束 / 工具开始 / AGENT_RESULT / AGENT_END / turn 终态都会调用。
  const finalizeStreams = useCallback(() => {
    const cur = streamRef.current;
    streamRef.current = null;
    if (cur) {
      upsert(cur.entryId, (e) =>
        e.kind === "msg" || e.kind === "thinking" ? { ...e, streaming: false } : e,
      );
    }
  }, [upsert]);

  const reloadSessions = useCallback(async () => {
    try {
      setSessions(await api.listSessions(workspace.id));
    } catch {
      /* 会话发现失败不阻塞主流程 */
    }
  }, [workspace.id]);

  // 下拉选项：已落盘会话 + 当前会话（新建未发消息时尚未落盘，补临时项）。
  const sessionOptions = sessions.some((s) => s.session_id === sessionId)
    ? sessions
    : [{ session_id: sessionId, messages: 0, updated_at: 0 }, ...sessions];

  // 初始加载：历史（块级回放）+ plan 状态 + 会话列表（重启后从 AgentState 恢复）。
  useEffect(() => {
    let alive = true;
    setEntries([]);
    streamRef.current = null;
    toolRef.current.clear();
    setBusy(false);
    api.getHistory(workspace.id, runtimeMode, sessionId).then((history) => {
      if (!alive) return;
      const restored: Entry[] = [];
      for (const h of history) {
        const role = (h.role === "user" || h.role === "assistant" ? h.role : "system") as
          | "user"
          | "assistant"
          | "system";
        const ts = fmtTime(h.timestamp);
        if (h.blocks?.length) {
          restored.push(...entriesFromBlocks(role, h.blocks, ts));
        } else if (h.text) {
          restored.push({ kind: "msg", role, text: h.text, ts, id: nextId() });
        }
      }
      setEntries(restored);
    });
    api.getPlanStatus(workspace.id, runtimeMode, sessionId).then((s) => alive && setPlanActive(s.active));
    void reloadSessions();
    return () => {
      alive = false;
    };
  }, [workspace.id, sessionId, runtimeMode, reloadSessions]);

  // 事件归流：只处理 (agent_id, session_id) 匹配本组件的事件。
  useEffect(() => {
    const applies = (p: { workspace_id: string; session_id: string }) =>
      p.workspace_id === workspace.id && p.session_id === sessionId;

    const un1 = onAgentEvent((payload: AgentEventPayload) => {
      if (!applies(payload)) return;
      const ev = payload.event;
      switch (ev?.type) {
        case "MODEL_CALL_START":
          setEntries((prev) => [
            ...prev,
            { kind: "indicator", text: "thinking…", ts: nowTime(), id: nextId() },
          ]);
          break;
        case "MODEL_CALL_END":
          setEntries((prev) => prev.filter((e) => e.kind !== "indicator"));
          break;
        case "TEXT_BLOCK_DELTA": {
          const delta = String(ev.delta ?? "");
          if (!delta) break;
          const blockId = String(ev.block_id ?? "");
          const cur = streamRef.current;
          if (!cur || cur.kind !== "msg" || cur.blockId !== blockId) {
            // 新文本块：先定格上一个流式块（保持时间交错），再开新条目。
            finalizeStreams();
            const id = nextId();
            streamRef.current = { kind: "msg", blockId, entryId: id };
            setEntries((prev) => [
              ...prev,
              { kind: "msg", role: "assistant", text: delta, ts: nowTime(), streaming: true, id },
            ]);
          } else {
            upsert(cur.entryId, (e) => (e.kind === "msg" ? { ...e, text: e.text + delta } : e));
          }
          break;
        }
        case "THINKING_BLOCK_DELTA": {
          const delta = String(ev.delta ?? "");
          if (!delta) break;
          const blockId = String(ev.block_id ?? "");
          const cur = streamRef.current;
          if (!cur || cur.kind !== "thinking" || cur.blockId !== blockId) {
            // 新思考块：先定格上一个流式块，再开新条目（工具后的思考不再并入前面）。
            finalizeStreams();
            const id = nextId();
            streamRef.current = { kind: "thinking", blockId, entryId: id };
            setEntries((prev) => [
              ...prev,
              { kind: "thinking", text: delta, ts: nowTime(), streaming: true, id },
            ]);
          } else {
            upsert(cur.entryId, (e) => (e.kind === "thinking" ? { ...e, text: e.text + delta } : e));
          }
          break;
        }
        case "TEXT_BLOCK_END":
        case "THINKING_BLOCK_END":
          // 块结束：光标立刻消失（后续同类型新块会开新条目）。
          finalizeStreams();
          break;
        case "TOOL_CALL_START": {
          finalizeStreams(); // 工具前的文本/思考块定格（去掉打字光标）
          // 后端事件字段：tool_call_name / tool_call_id（event.rs ToolCallStart）。
          const name = String(ev.tool_call_name ?? ev.name ?? ev.tool_name ?? "tool");
          const id = nextId();
          const callKey = String(ev.tool_call_id ?? ev.call_id ?? ev.id ?? `${name}-${id}`);
          toolRef.current.set(callKey, id);
          setEntries((prev) => [
            ...prev,
            { kind: "tool", callId: callKey, name, args: "", ts: nowTime(), id },
          ]);
          break;
        }
        case "TOOL_CALL_DELTA": {
          // 参数 JSON 分片累积（非流式执行时为一次完整序列化）。
          const callKey = String(ev.tool_call_id ?? ev.call_id ?? "");
          const chunk = String(ev.delta ?? "");
          if (!callKey || !chunk) break;
          const entryId = toolRef.current.get(callKey);
          if (entryId) {
            upsert(entryId, (e) => (e.kind === "tool" ? { ...e, args: e.args + chunk } : e));
          }
          break;
        }
        case "TOOL_CALL_END": {
          // 参数齐了：尝试 pretty 一次（失败保持原样）。
          const callKey = String(ev.tool_call_id ?? ev.call_id ?? "");
          const entryId = toolRef.current.get(callKey);
          if (entryId) {
            upsert(entryId, (e) => {
              if (e.kind !== "tool") return e;
              let args = e.args;
              try {
                args = JSON.stringify(JSON.parse(e.args), null, 2);
              } catch {
                /* 非 JSON 保持原样 */
              }
              return { ...e, args };
            });
          }
          break;
        }
        case "TOOL_RESULT_TEXT_DELTA": {
          // 结果文本增量累积（ToolResultEnd 只带 state，内容走这里）。
          const callKey = String(ev.tool_call_id ?? ev.call_id ?? "");
          const chunk = String(ev.delta ?? "");
          if (!callKey || !chunk) break;
          const entryId = toolRef.current.get(callKey);
          if (entryId) {
            upsert(entryId, (e) =>
              e.kind === "tool" ? { ...e, result: (e.result ?? "") + chunk } : e,
            );
          }
          break;
        }
        case "TOOL_RESULT_END": {
          // 后端 ToolResultEnd 只带 state（Success/Error/…），内容已由
          // TOOL_RESULT_TEXT_DELTA 累积；这里只定格成功/失败状态。
          const callKey = String(ev.tool_call_id ?? ev.call_id ?? ev.id ?? "");
          const entryId = toolRef.current.get(callKey);
          const ok = /^success$/i.test(String(ev.state ?? "success"));
          if (entryId) {
            upsert(entryId, (e) => (e.kind === "tool" ? { ...e, ok } : e));
          }
          break;
        }
        case "AGENT_RESULT": {
          // 最终消息是权威来源：替换最后一个文本流块并结束流式态。
          const result = ev.result as { content?: unknown } | undefined;
          const text = extractText(result?.content);
          const cur = streamRef.current;
          streamRef.current = null;
          if (text) {
            if (cur && cur.kind === "msg") {
              upsert(cur.entryId, (e) => (e.kind === "msg" ? { ...e, text, streaming: false } : e));
            } else {
              if (cur) {
                upsert(cur.entryId, (e) =>
                  e.kind === "msg" || e.kind === "thinking" ? { ...e, streaming: false } : e,
                );
              }
              setEntries((prev) => [
                ...prev,
                { kind: "msg", role: "assistant", text, ts: nowTime(), id: nextId() },
              ]);
            }
          } else if (cur) {
            // 权威结果无文本（纯工具轮）：仅结束流式态
            upsert(cur.entryId, (e) =>
              e.kind === "msg" || e.kind === "thinking" ? { ...e, streaming: false } : e,
            );
          }
          break;
        }
        case "AGENT_END":
          finalizeStreams();
          break;
        default:
          break;
      }
    });

    const un2 = onTurnFinished((payload) => {
      if (!applies(payload)) return;
      setBusy(false);
      finalizeStreams(); // 中断/失败兜底：复位流式态，打字机停走
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
  }, [workspace.id, sessionId, upsert, finalizeStreams, reloadSessions]);

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
      await api.sendMessage(workspace.id, runtimeMode, sessionId, text, payload);
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
      await api.steerTurn(workspace.id, runtimeMode, sessionId, text);
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
      const status = await api.setPlanMode(workspace.id, runtimeMode, sessionId, !planActive);
      setPlanActive(status.active);
    } catch (e) {
      setError(String(e));
    }
  };

  const interrupt = async () => {
    try {
      await api.interruptTurn(workspace.id, runtimeMode, sessionId);
    } catch (e) {
      setError(String(e));
    }
  };

  const clear = async () => {
    try {
      await api.clearSession(workspace.id, runtimeMode, sessionId);
      setEntries([]);
      void reloadSessions();
    } catch (e) {
      setError(String(e));
    }
  };

  // plan/free 仅 solo 运行方式下可用（产品语义）。
  const supportsPlan = runtimeMode === "solo";

  // 单条消息渲染（时间线行内）。assistant 流式输出接打字机；
  // user 输入保持纯文本（避免用户打的 * _ 被意外格式化）。
  const renderEntry = (e: Entry) => {
    if (e.kind === "msg") {
      return (
        <div className={`msg ${e.role}`}>
          {e.role === "assistant" ? (
            <div className="bubble md">
              <Typewriter text={e.text} active={e.streaming === true} />
            </div>
          ) : (
            <div className="bubble">{e.text}</div>
          )}
        </div>
      );
    }
    if (e.kind === "thinking") {
      return (
        <div className="thinking-block">
          <div className="thinking-label">💭 思考过程</div>
          <div className="thinking-text md">
            <Typewriter text={e.text} active={e.streaming === true} />
          </div>
        </div>
      );
    }
    if (e.kind === "image") {
      return (
        <div className="msg user">
          <img className="msg-img" src={e.dataUrl} alt="attachment" />
        </div>
      );
    }
    if (e.kind === "file") {
      return (
        <div className="msg user">
          <div className="bubble file-chip">📎 {e.name}</div>
        </div>
      );
    }
    if (e.kind === "indicator") {
      return <div className="turn-indicator">{e.text}</div>;
    }
    return (
      <div className="tool-block">
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
  };

  // 切换运行方式（solo ↔ team）：以当前 agent 重新进入（App 层换实例重开会话）。
  const switchMode = (next: "solo" | "team") => {
    if (next === runtimeMode) return;
    if (next === "team" && workspace.members.length === 0) {
      if (!window.confirm("Team 模式当前未配置成员（lead 将单干）。可在编辑工作区里添加成员。继续切换？")) {
        return;
      }
    }
    onModeChange(next);
  };

  return (
    <div className="conversation">
      <div
        className={`conv-main ${dragActive ? "drag-active" : ""}`}
        onDragOver={(e) => {
          e.preventDefault();
          setDragActive(true);
        }}
        onDragLeave={(e) => {
          if (e.currentTarget === e.target) setDragActive(false);
        }}
        onDrop={(e) => {
          e.preventDefault();
          setDragActive(false);
          if (e.dataTransfer?.files?.length) void pickFiles(e.dataTransfer.files);
        }}
      >
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
              {workspace.kind === "collab"
                ? `collab · ${runtimeMode}`
                : workspace.kind === "ops"
                  ? "ops · shell + web"
                  : "db · read-only SQL"}
              {supportsPlan ? " · plan/free switchable" : ""}
            </div>
          </div>
        )}
        {entries.map((e) => {
          // 时间线行：左侧节点圆点 + 时间标签，右侧原条目渲染。
          const rowKind =
            e.kind === "msg"
              ? e.role
              : e.kind === "image" || e.kind === "file"
                ? "user"
                : e.kind;
          return (
            <div key={e.id} className={`tl-row tl-${rowKind}`}>
              <span className="tl-dot" aria-hidden />
              <div className="tl-body">
                {e.ts ? <div className="tl-time">{e.ts}</div> : null}
                {renderEntry(e)}
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
          onPaste={(e) => {
            // 粘贴图片/文件 → 附件（文本粘贴不受影响，files 为空时不拦截）。
            const files = e.clipboardData?.files;
            if (files && files.length > 0) {
              e.preventDefault();
              void pickFiles(files);
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
      {dragActive && <div className="drop-hint">松开以添加附件</div>}
      </div>
      <FilePanel workspaceId={workspace.id} />
    </div>
  );
}
