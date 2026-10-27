// 会话组件：消息流 + composer + 事件归流渲染（按工作区归流——会话状态
// 属于工作区，同一智能体在不同工作区各有独立会话）。
// 事件处理对齐 Java AgentSessionView.apply 的思路：增量 apply 事件维护视图，
// AgentResult 为最终消息的权威来源，流式 delta 仅作过程展示。

import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { api, onAgentEvent, onTurnFinished, onTurnStarted, type AgentConfig, type AgentEventPayload, type ConfirmResultPayload, type WorkspaceConfig, type WorkspaceFileEntry } from "../api";
import ServersPanel from "./ServersPanel";
import DatabasesPanel from "./DatabasesPanel";
import OxDataPanel from "./OxDataPanel";
import DataViewerModal from "./DataViewerModal";

type Entry =
  | { kind: "msg"; id: string; role: "user" | "assistant" | "system"; text: string; ts?: string; streaming?: boolean; actor?: string }
  | { kind: "tool"; id: string; callId: string; name: string; args: string; result?: string; ok?: boolean; open?: boolean; ts?: string; actor?: string }
  | { kind: "thinking"; id: string; text: string; ts?: string; streaming?: boolean; open?: boolean; actor?: string }
  | { kind: "image"; id: string; dataUrl: string; ts?: string }
  | { kind: "file"; id: string; name: string; ts?: string }
  | { kind: "indicator"; id: string; text: string; ts?: string }
  | { kind: "confirm"; id: string; calls: Array<Record<string, unknown>>; ts?: string };

let entrySeq = 0;
const nextId = () => `e${++entrySeq}`;

/** 本地时间 HH:MM:SS（时间线节点标签）。 */
const nowTime = () => new Date().toLocaleTimeString("zh-CN", { hour12: false });

/** 历史时间戳（ISO-8601）→ HH:MM:SS；非 ISO 原样返回。 */
function fmtTime(ts?: string): string | undefined {
  if (!ts) return undefined;
  if (!/^\d{4}-\d{2}-\d{2}T/.test(ts)) return ts;
  // 消息时间戳是 UTC ISO 串：转本地时区显示（与流式 nowTime 同源），
  // 否则比本地时间慢 8 小时（UTC+8），用户看到的是「对不上钟」的时间。
  const d = new Date(ts);
  return isNaN(d.getTime()) ? ts.slice(11, 19) : d.toLocaleTimeString("zh-CN", { hour12: false });
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

/** 流式内容自动滚底容器：内框（max-height + overflow）里内容长高时
 *  （打字机 reveal / delta 追加）保持滚动到底部；非流式（历史展开）
 *  不干预——滚动控制权留给用户。 */
function AutoScrollBody({
  className,
  active,
  children,
}: {
  className: string;
  active: boolean;
  children: ReactNode;
}) {
  const outerRef = useRef<HTMLDivElement>(null);
  const innerRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!active) return;
    const outer = outerRef.current;
    const inner = innerRef.current;
    if (!outer || !inner) return;
    outer.scrollTop = outer.scrollHeight;
    const ro = new ResizeObserver(() => {
      outer.scrollTop = outer.scrollHeight;
    });
    ro.observe(inner);
    return () => ro.disconnect();
  }, [active]);
  return (
    <div ref={outerRef} className={className}>
      <div ref={innerRef}>{children}</div>
    </div>
  );
}

function extractText(content: unknown): string {
  if (!Array.isArray(content)) return "";
  return content
    .filter((b) => (b as { type?: string })?.type === "text")
    .map((b) => (b as { text?: string }).text ?? "")
    .join("");
}

/** 工具参数展示：单 string 字段的 JSON 直接显示字段值（memory_save 的
 *  content 等 markdown 正文可读，\n 不再是字面转义）；其余 pretty 缩进。 */
function prettyArgs(args: string): string {
  try {
    const v = JSON.parse(args);
    if (typeof v === "string") return v;
    if (v && typeof v === "object" && !Array.isArray(v)) {
      const keys = Object.keys(v);
      if (keys.length === 1 && typeof v[keys[0]] === "string") return v[keys[0]];
      return JSON.stringify(v, null, 2);
    }
    return JSON.stringify(v, null, 2);
  } catch {
    return args;
  }
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
  /** callId → tool_use 条目（跨消息合并：tool_result 回填到对应调用条目，
   *  不再各建一条导致「running + done」双卡）。 */
  byCall?: Map<string, Entry>,
): Entry[] {
  const out: Entry[] = [];
  for (const b of blocks) {
    const type = String(b.type ?? "");
    if (type === "text") {
      const text = String(b.text ?? "");
      // 运行时注入的 system-reminder（如 plan 模式约束）是给模型的指令，
      // 不作为对话内容渲染。
      if (text.trim() && !text.startsWith("<system-reminder>")) {
        out.push({ kind: "msg", role, text, ts, id: nextId() });
      }
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
      const entry: Entry = {
        kind: "tool",
        callId: String(b.id ?? ""),
        name: String(b.name ?? "tool"),
        args: shortJson(b.input ?? {}, 600),
        ts,
        id: nextId(),
      };
      out.push(entry);
      if (entry.callId) byCall?.set(entry.callId, entry);
    } else if (type === "tool_result") {
      const output = Array.isArray(b.output) ? b.output : [];
      const text = output
        .filter((o) => (o as { type?: string })?.type === "text")
        .map((o) => (o as { text?: string }).text ?? "")
        .join("\n");
      const callId = String(b.id ?? "");
      const ok = String(b.state ?? "success") === "success";
      const hit = callId ? byCall?.get(callId) : undefined;
      if (hit && hit.kind === "tool") {
        // 同一调用的结果：回填到 tool_use 条目（合并为一张卡）。
        hit.result = text || shortJson(output, 600);
        hit.ok = ok;
      // 回填后继续处理后续块（原 return 会丢弃同消息剩余 tool_result，
      // 对应 tool_use 条目被误标「未完成」）。
      }
      out.push({
        kind: "tool",
        callId,
        name: String(b.name ?? "tool"),
        args: "",
        result: text || shortJson(output, 600),
        ok,
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

/** 目录懒加载状态：dirPath → 该层直接子项（点开哪层加载哪层）。 */

function DirList({
  dirPath,
  depth,
  dirs,
  expanded,
  onToggle,
  onOpen,
}: {
  dirPath: string;
  depth: number;
  dirs: Map<string, WorkspaceFileEntry[]>;
  expanded: Set<string>;
  onToggle: (path: string, isDir: boolean) => void;
  onOpen: (path: string) => void;
}) {
  const items = dirs.get(dirPath);
  if (!items) {
    return <div className="file-empty" style={{ paddingLeft: 8 + depth * 12 }}>加载中…</div>;
  }
  return (
    <>
      {items.map((it) => (
        <div key={it.path}>
          <button
            className={`file-row ${it.is_dir ? "is-dir" : ""}`}
            style={{ paddingLeft: 8 + depth * 12 }}
            title={it.path}
            onClick={() => (it.is_dir ? onToggle(it.path, true) : onOpen(it.path))}
          >
            <span className="file-icon">{it.is_dir ? (expanded.has(it.path) ? "📂" : "📁") : "📄"}</span>
            <span className="file-name">{it.path.split("/").pop()}</span>
            {!it.is_dir && it.size > 0 && <span className="file-size">{fmtSize(it.size)}</span>}
          </button>
          {it.is_dir && expanded.has(it.path) && (
            <DirList
              dirPath={it.path}
              depth={depth + 1}
              dirs={dirs}
              expanded={expanded}
              onToggle={onToggle}
              onOpen={onOpen}
            />
          )}
        </div>
      ))}
      {items.length === 0 && (
        <div className="file-empty" style={{ paddingLeft: 8 + depth * 12 }}>（空目录）</div>
      )}
    </>
  );
}

/** 右侧文件面板：工作区根目录的文件树（与 agent 文件工具同一锚定目录）。
 *  懒加载：点开哪层加载哪层（后端单层列目录，无深度/总量限制）；
 *  目录默认折叠；点击文件预览文本；turn 结束自动刷新已展开层；可整体收起。 */
function FilePanel({
  workspaceId,
  onCollapse,
}: {
  workspaceId: string;
  onCollapse: () => void;
}) {
  const [dirs, setDirs] = useState<Map<string, WorkspaceFileEntry[]>>(new Map());
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");
  const [preview, setPreview] = useState<{ path: string; content: string } | null>(null);
  const expandedRef = useRef<Set<string>>(expanded);
  expandedRef.current = expanded;

  const loadDir = useCallback(
    async (dirPath: string) => {
      try {
        const list = await api.listWorkspaceFiles(workspaceId, dirPath || undefined);
        setDirs((prev) => {
          const next = new Map(prev);
          next.set(dirPath, list);
          return next;
        });
      } catch (e) {
        setError(String(e));
      }
    },
    [workspaceId],
  );

  // 初始/切换工作区：重置到根层（目录默认折叠）。
  useEffect(() => {
    setDirs(new Map());
    setExpanded(new Set());
    setPreview(null);
    setError("");
    setLoading(true);
    void loadDir("").finally(() => setLoading(false));
  }, [loadDir]);

  // turn 结束刷新：根层 + 已展开的目录层（agent 可能改了文件）。
  useEffect(() => {
    const un = onTurnFinished((p) => {
      if (p.workspace_id !== workspaceId) return;
      void loadDir("");
      for (const d of expandedRef.current) void loadDir(d);
    });
    return () => {
      un.then((f) => f());
    };
  }, [workspaceId, loadDir]);

  const toggle = (path: string, isDir: boolean) => {
    if (!isDir) return;
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });
    if (!dirs.has(path)) void loadDir(path);
  };

  const open = async (path: string) => {
    try {
      setError("");
      setPreview({ path, content: await api.readWorkspaceFile(workspaceId, path) });
    } catch (e) {
      setError(String(e));
    }
  };

  const refresh = () => {
    setLoading(true);
    void loadDir("")
      .then(() => {
        for (const d of expandedRef.current) void loadDir(d);
      })
      .finally(() => setLoading(false));
  };

  return (
    <aside className="file-panel">
      <div className="file-panel-head">
        <span>文件</span>
        <span className="spacer" />
        <button className="icon-btn" title="刷新" onClick={refresh}>
          ⟳
        </button>
        <button className="icon-btn" title="收起面板" onClick={onCollapse}>
          »
        </button>
      </div>
      {error && <div className="file-panel-error">{error}</div>}
      <div className="file-tree">
        {loading && dirs.get("") === undefined && <div className="file-empty">加载中…</div>}
        <DirList
          dirPath=""
          depth={0}
          dirs={dirs}
          expanded={expanded}
          onToggle={toggle}
          onOpen={(p) => void open(p)}
        />
      </div>
      {preview && (
        <DataViewerModal
          title={preview.path}
          content={preview.content}
          onClose={() => setPreview(null)}
        />
      )}
    </aside>
  );
}

export default function Conversation({
  workspace,
  agent,
  runtimeMode,
  sessionId,
  onSessionChange,
  onModeChange,
  onToast,
  onConfirm,
}: {
  /** 会话所属工作区（命令与事件都按工作区归流）。 */
  workspace: WorkspaceConfig;
  /** 工作区选定的智能体（显示名与空态文案用）。 */
  agent: AgentConfig;
  /** 运行方式（solo/team）：协同工作区可切换，切换即重开会话（新实例）。 */
  runtimeMode: "solo" | "team";
  /** 当前 session（受控：App 层持有，左栏可切换）。 */
  sessionId: string;
  onSessionChange: (sessionId: string) => void;
  onModeChange: (mode: "solo" | "team") => void;
  /** 全局 toast（App 层渲染）：会话内错误/提示统一走右上角通知。 */
  onToast?: (kind: "info" | "error", text: string) => void;
  /** 全局确认弹窗（App 层渲染，替代 window.confirm）。 */
  onConfirm?: (title: string, message: string) => Promise<boolean>;
}) {
  // 仅协同工作区有 solo/team 运行方式切换；ops/db 工具面由类型决定。
  const isCollab = workspace.kind === "collab";

  // 会话切换/新建时聚焦输入框（桌面 IM 惯例，直接可打字）。
  useEffect(() => {
    inputRef.current?.focus();
  }, [sessionId]);

  // 会话由 App 层受控（左栏切换/新建）；本组件只消费 sessionId。
  const [entries, setEntries] = useState<Entry[]>([]);
  const [input, setInput] = useState("");
  // 待发送附件（base64 内联；preview 仅图片用于本地预览）。
  const [attachments, setAttachments] = useState<
    Array<{ name: string; media_type: string; data_base64: string; preview?: string }>
  >([]);
  const [busy, setBusy] = useState(false);
  // busy 的同步镜像：React setState 是异步批处理，同一渲染周期内连续两次
  // send()/steer() 读到的 busy 都是旧值（false），防抖失效——快速双击发送
  // 会把同一条消息发两遍（第二遍被后端 turn 互斥拒绝后降级 steer 入队，
  // 结算续轮再跑一遍）。入口检查走 ref（同步），写入走 setBusySafe 保持
  // ref 与 state 一致。
  const busyRef = useRef(false);
  const setBusySafe = (v: boolean) => {
    busyRef.current = v;
    setBusy(v);
  };
  // 拖拽附件进行中（容器高亮提示）。
  const [dragActive, setDragActive] = useState(false);
  // 文件面板显隐（可收起成细条）。
  const [filesOpen, setFilesOpen] = useState(true);
  // .ox 数据面板（黑板报 + 记忆）：与文件/服务器/数据库面板互斥展开。
  const [oxOpen, setOxOpen] = useState(false);
  const [planActive, setPlanActive] = useState(false);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");

  // 会话内错误/提示统一走全局 toast（右上角）：state 只作事件通道，
  // 变化即转发并清空——所有既有 setError/setNotice 调用点无需改动。
  useEffect(() => {
    if (error) {
      onToast?.("error", error);
      setError("");
    }
  }, [error, onToast]);
  useEffect(() => {
    if (notice) {
      onToast?.("info", notice);
      setNotice("");
    }
  }, [notice, onToast]);

  // 当前流式块（按 block_id 分组：同一块追加，块切换定格旧条目开新条目，
  // 保证 思考→工具→思考→输出 按真实时间交错显示）。
  const streamRef = useRef<{ kind: "msg" | "thinking"; blockId: string; entryId: string } | null>(null);
  const toolRef = useRef<Map<string, string>>(new Map()); // callKey -> entry id
  // 本 turn 已流式渲染过的 assistant 文本块（TEXT 块定格时登记）：
  // AGENT_RESULT 的权威文本与之比对，只 append 没渲染过的块——
  // 「模型先说话再调工具」的 turn 里，权威结果到达时末尾是工具卡，
  // 按末尾比对会落空导致同一句话渲染两条。
  const turnTextsRef = useRef<Set<string>>(new Set());
  // 本 turn 是否出现过工具卡（TOOL_CALL_START 登记）：区分「纯工具轮的
  // 空权威结果」（正常）与「空响应」（模型只产出 thinking，无正文无工具）。
  const turnToolSeenRef = useRef(false);
  // 提交在途标记：steer/send 的 IPC 往返期间忽略再次提交。IME 组词确认的
  // Enter 与发送的 Enter 连击（间隔可短于 IPC 往返）会把同一条消息提交
  // 两次——steer 双条入队会被模型当两轮指令各处理一遍。
  const submittingRef = useRef(false);
  const scrollRef = useRef<HTMLDivElement>(null);
  const innerScrollRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLTextAreaElement>(null);
  // 用户是否停在底部附近（上滚查看历史时不强拉到底——可控性）。
  const nearBottomRef = useRef(true);

  const upsert = useCallback((id: string, patch: (e: Entry) => Entry) => {
    setEntries((prev) => prev.map((e) => (e.id === id ? patch(e) : e)));
  }, []);

  // 收流：把流式中的 msg/thinking 条目置为完成（打字机停走、光标消失）。
  // 块切换 / 块结束 / 工具开始 / AGENT_RESULT / AGENT_END / turn 终态都会调用。
  /** 用户点击卡片头部：手动展开/收起（覆盖 running 展开 / done 收起的默认行为）。
   *  注意按条目类型取默认值——历史回放的 thinking/tool 条目没有 open 字段，
   *  不能用 `"open" in e` 判断（那会让历史条目点击无反应）。 */
  const toggleEntry = useCallback(
    (id: string) => {
      upsert(id, (e) => {
        if (e.kind === "thinking") return { ...e, open: !(e.open ?? e.streaming === true) };
        if (e.kind === "tool") return { ...e, open: !(e.open ?? e.ok === undefined) };
        return e;
      });
    },
    [upsert],
  );

  const finalizeStreams = useCallback(() => {
    const cur = streamRef.current;
    streamRef.current = null;
    if (cur) {
      upsert(cur.entryId, (e) => {
        if (e.kind === "msg") {
          if (e.role === "assistant" && e.text.trim()) {
            turnTextsRef.current.add(e.text);
          }
          return { ...e, streaming: false };
        }
        if (e.kind === "thinking") return { ...e, streaming: false, open: false }; // 思考结束默认收起
        return e;
      });
    }
  }, [upsert]);

  // 初始加载：历史（块级回放）+ plan 状态 + 会话列表（重启后从 AgentState 恢复）。
  useEffect(() => {
    let alive = true;
    setEntries([]);
    streamRef.current = null;
    toolRef.current.clear();
    turnTextsRef.current = new Set();
    turnToolSeenRef.current = false;
    setBusySafe(false);
    api.getHistory(workspace.id, runtimeMode, sessionId).then((history) => {
      if (!alive) return;
      const restored: Entry[] = [];
      const byCall = new Map<string, Entry>(); // callId → tool_use 条目（结果回填合并）
      for (const h of history) {
        const role = (h.role === "user" || h.role === "assistant" ? h.role : "system") as
          | "user"
          | "assistant"
          | "system";
        const ts = fmtTime(h.timestamp);
        if (h.blocks?.length) {
          restored.push(...entriesFromBlocks(role, h.blocks, ts, byCall));
        } else if (h.text) {
          // 同上：system-reminder 注入消息不渲染。
          if (!h.text.startsWith("<system-reminder>")) {
            restored.push({ kind: "msg", role, text: h.text, ts, id: nextId() });
          }
        }
      }
      // 历史里没有结果的调用（中断/挂起）不再是「运行中」：标记未完成（红色）。
      for (const e of restored) {
        if (e.kind === "tool" && e.ok === undefined) e.ok = false;
      }
      setEntries(restored);
    });
    api.getPlanStatus(workspace.id, runtimeMode, sessionId).then((s) => alive && setPlanActive(s.active));
    // 刷新恢复：该会话的 turn 若仍在后端运行，恢复 busy——事件监听是全局
    // 的，后续事件照常到达本组件，无需重启 turn。
    api
      .listRunningTurns()
      .then((runs) => {
        if (!alive) return;
        const still = runs.some(
          (r) => r.workspace_id === workspace.id && r.session_id === sessionId,
        );
        if (still) setBusySafe(true);
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [workspace.id, sessionId, runtimeMode]);

  // 事件归流：只处理 (agent_id, session_id) 匹配本组件的事件。
  useEffect(() => {
    const applies = (p: { workspace_id: string; session_id: string }) =>
      p.workspace_id === workspace.id && p.session_id === sessionId;

    const un1 = onAgentEvent((payload: AgentEventPayload) => {
      if (!applies(payload)) return;
      const ev = payload.event;
      switch (ev?.type) {
        case "CONTEXT_COMPACTION_START":
          // 压缩要调一次辅助 LLM，可能耗时数秒：indicator 提示「压缩中」，
          // 避免长时间无输出被误判为卡死。
          setEntries((prev) => [
            ...prev,
            { kind: "indicator", text: "compacting context…", ts: nowTime(), id: nextId() },
          ]);
          break;
        case "CONTEXT_COMPACTION_END":
          setEntries((prev) => [
            ...prev.filter((e) => e.kind !== "indicator"),
            {
              kind: "msg",
              role: "system",
              text: `上下文已压缩${ev.detail ? `：${String(ev.detail)}` : ""}`,
              ts: nowTime(),
              id: nextId(),
            },
          ]);
          break;
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
              {
                kind: "msg",
                role: "assistant",
                text: delta,
                ts: nowTime(),
                streaming: true,
                id,
                actor: ev.source ? String(ev.source) : undefined,
              },
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
              {
                kind: "thinking",
                text: delta,
                ts: nowTime(),
                streaming: true,
                open: true,
                id,
                actor: ev.source ? String(ev.source) : undefined,
              },
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
          turnToolSeenRef.current = true;
          setEntries((prev) => [
            ...prev,
            {
              kind: "tool",
              callId: callKey,
              name,
              args: "",
              ts: nowTime(),
              open: true,
              id,
              actor: ev.source ? String(ev.source) : undefined,
            },
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
            upsert(entryId, (e) => (e.kind === "tool" ? { ...e, ok, open: false } : e)); // 结束默认收起
          }
          break;
        }
        case "REQUIRE_USER_CONFIRM": {
          // 权限 HITL：引擎请求用户授权（工具调用已挂起，未执行）。
          finalizeStreams();
          const calls = Array.isArray(ev.tool_calls)
            ? (ev.tool_calls as Array<Record<string, unknown>>)
            : [];
          if (calls.length > 0) {
            setEntries((prev) => [
              ...prev,
              { kind: "confirm", calls, ts: nowTime(), id: nextId() },
            ]);
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
              turnTextsRef.current.add(text);
              upsert(cur.entryId, (e) => (e.kind === "msg" ? { ...e, text, streaming: false } : e));
            } else {
              if (cur) {
                upsert(cur.entryId, (e) =>
                  e.kind === "msg" || e.kind === "thinking" ? { ...e, streaming: false } : e,
                );
              }
              // 流式块已由 *_BLOCK_END / TOOL_CALL_START 定格并登记进
              // turnTextsRef：权威结果只是同一内容的回执。按块比对，只把
              // 没渲染过的文本块 append——「先说话再调工具」的 turn 里
              // 权威结果到达时末尾是工具卡，按末尾比对会落空导致重复。
              const blocks = Array.isArray(result?.content)
                ? (result?.content as Array<Record<string, unknown>>)
                : [];
              const seen = turnTextsRef.current;
              const fresh = blocks
                .filter((b) => String(b?.type ?? "") === "text")
                .map((b) => String(b?.text ?? ""))
                .filter((t) => t.trim() && !seen.has(t));
              if (fresh.length > 0) {
                fresh.forEach((t) => seen.add(t));
                setEntries((prev) => [
                  ...prev,
                  {
                    kind: "msg",
                    role: "assistant",
                    text: fresh.join("\n\n"),
                    ts: nowTime(),
                    id: nextId(),
                  },
                ]);
              }
            }
          } else {
            // 权威结果无文本：仅结束流式态。若本 turn 既没有渲染过正文、
            // 也没有任何工具卡（模型只产出 thinking 或完全空响应），
            // 显示占位提示——否则回合「无疾而终」，用户不知道发生了什么。
            if (cur) {
              upsert(cur.entryId, (e) =>
                e.kind === "msg" || e.kind === "thinking" ? { ...e, streaming: false } : e,
              );
            }
            if (!turnTextsRef.current.size && !turnToolSeenRef.current) {
              setEntries((prev) => [
                ...prev,
                {
                  kind: "msg",
                  role: "assistant",
                  text: "（模型本轮没有产出可见回答，请重试或换个问法）",
                  ts: nowTime(),
                  id: nextId(),
                },
              ]);
            }
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
      setBusySafe(false);
      finalizeStreams(); // 中断/失败兜底：复位流式态，打字机停走
      turnTextsRef.current = new Set(); // turn 边界：判重集合清零
      turnToolSeenRef.current = false;
      if (payload.outcome === "failed") {
        setError(payload.error ?? "turn failed");
      }
    });

    // steer 续轮：后端自动开启的新 turn（前端保持 busy、提示用户）。
    const un3 = onTurnStarted((payload) => {
      if (!applies(payload)) return;
      setBusySafe(true);
      turnTextsRef.current = new Set(); // 新 turn：判重集合清零
      turnToolSeenRef.current = false;
      setNotice("Continuing with your steering input…");
      setTimeout(() => setNotice(""), 3000);
    });

    return () => {
      un1.then((f) => f());
      un2.then((f) => f());
      un3.then((f) => f());
    };
  }, [workspace.id, sessionId, upsert, finalizeStreams]);

  // 自动滚底：观察内容高度（打字机 reveal / 新条目都会长高），
  // 仅当用户停在底部附近时跟随；上滚查看历史时不打扰。
  useEffect(() => {
    const outer = scrollRef.current;
    const inner = innerScrollRef.current;
    if (!outer || !inner) return;
    const ro = new ResizeObserver(() => {
      if (nearBottomRef.current) outer.scrollTop = outer.scrollHeight;
    });
    ro.observe(inner);
    return () => ro.disconnect();
  }, []);

  // 切会话：重置为跟随底部。
  useEffect(() => {
    nearBottomRef.current = true;
    scrollRef.current?.scrollTo({ top: 0 });
  }, [sessionId]);

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
    // busy 检查走同步 ref：state 在同一渲染周期内不更新，双击/回车+点击
    // 会连发两条（见 busyRef 注释）。
    if ((!text && attachments.length === 0) || busyRef.current) return;
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
    setBusySafe(true);
    try {
      await api.sendMessage(workspace.id, runtimeMode, sessionId, text, payload);
    } catch (e) {
      const msg = String(e);
      // turn 互斥撞锁（steer 续轮的 turn-finished→turn-started 间隙前端 busy
      // 短暂失真等场景）：后端确实在跑 → 消息（含附件）降级为插入排队，
      // turn 结算时自动消费。
      if (msg.includes("already running")) {
        try {
          await api.steerTurn(workspace.id, runtimeMode, sessionId, text, payload);
          setNotice("智能体仍在运行——消息已插入对话，下一步生效。");
          setTimeout(() => setNotice(""), 4000);
        } catch {
          // steer 也被拒（后端守卫：turn 恰好已结束）→ 重试正常发送，
          // 此时拿锁应成功；消息不丢、不残留。
          try {
            await api.sendMessage(workspace.id, runtimeMode, sessionId, text, payload);
            return;
          } catch (e2) {
            setBusySafe(false);
            setError(String(e2));
          }
        }
        return;
      }
      setBusySafe(false);
      setError(msg);
    }
  };

  // 权限裁决：对挂起的工具调用答复（批准/始终允许/拒绝）→ 确认消息恢复回合。
  // tool_call 原样回传事件里的 ToolUse 块（core 据此定位 ASKING 块）；
  // 「始终允许」附带 allow 规则，core 写进会话权限上下文后对后续调用生效。
  const resolveConfirm = async (
    entryId: string,
    call: Record<string, unknown>,
    confirmed: boolean,
    scope: "once" | "session" | "always",
  ) => {
    // 从卡片移除该调用（全部裁决完卡片自然消失）。
    setEntries((prev) =>
      prev
        .map((e) => {
          if (e.id !== entryId || e.kind !== "confirm") return e;
          const rest = e.calls.filter((x) => x !== call);
          return rest.length > 0 ? { ...e, calls: rest } : null;
        })
        .filter((x): x is Entry => x !== null),
    );
    const toolName = String(call.name ?? "");
    const result: ConfirmResultPayload = {
      confirmed,
      tool_call: call,
      ...(scope !== "once"
        ? {
            rules: [
              {
                tool_name: toolName,
                behavior: "allow",
                source: "user",
              },
            ],
          }
        : {}),
    };
    // 永久允许：先落盘（跨会话生效）；失败不阻断本次批准（会话内规则仍随
    // confirm 结果写入）。
    if (scope === "always" && confirmed && toolName) {
      try {
        await api.savePermanentPermission(workspace.id, toolName);
      } catch (err) {
        console.error("save permanent permission failed:", err);
      }
    }
    setBusySafe(true);
    try {
      await api.sendConfirm(workspace.id, runtimeMode, sessionId, [result]);
    } catch (err) {
      setBusySafe(false);
      setError(String(err));
    }
  };

  // steer：turn 运行中补充指令（agent 在下一个 step 边界注入上下文，当步生效）。
  // 后端对「无运行中 turn」的 steer 会拒绝（防残留消息混入后续输入或触发
  // 自动续轮）——因此先调后端、成功才落本地条目；被拒且 turn 已结束时
  // 自动转正常发送，消息不丢。
  const steer = async () => {
    const text = input.trim();
    if ((!text && attachments.length === 0) || !busyRef.current) return;
    setError("");
    // 本地立即显示：文本 + 附件（图片缩略 / 文件 chip），与 send 同构。
    const local: Entry[] = [];
    if (text) local.push({ kind: "msg", role: "user", text, id: nextId() });
    for (const a of attachments) {
      local.push(
        a.preview
          ? { kind: "image", dataUrl: a.preview, id: nextId() }
          : { kind: "file", name: a.name, id: nextId() },
      );
    }
    const payload = attachments.map(({ name, media_type, data_base64 }) => ({
      name,
      media_type,
      data_base64,
    }));
    try {
      await api.steerTurn(workspace.id, runtimeMode, sessionId, text, payload);
      setInput("");
      setAttachments([]);
      setEntries((prev) => [...prev, ...local]);
      setNotice("已插入对话——智能体下一步会看到。");
      setTimeout(() => setNotice(""), 3000);
    } catch (e) {
      const msg = String(e);
      if (msg.includes("no turn is running")) {
        // busy 失真（turn 恰好结束）：改走正常发送。
        setInput("");
        setAttachments([]);
        setEntries((prev) => [...prev, ...local]);
        setBusySafe(true);
        try {
          await api.sendMessage(workspace.id, runtimeMode, sessionId, text, payload);
        } catch (e2) {
          setBusySafe(false);
          setError(String(e2));
        }
        return;
      }
      setError(msg);
    }
  };

  // Enter 发送；busy 时 Enter 转为插入（运行中也能打字，消息插入对话下一步生效）。
  // 在途防重：IPC 往返未完成前忽略再次提交（见 submittingRef 注释）。
  const submit = () => {
    if (submittingRef.current) return;
    const text = input.trim();
    if (!text) return;
    submittingRef.current = true;
    const op = busy ? steer() : send();
    void op.finally(() => {
      submittingRef.current = false;
    });
  };

  const newSession = () => {
    const id = `${runtimeMode}-${crypto.randomUUID().replace(/-/g, "").slice(0, 12)}`;
    onSessionChange(id);
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
            <>
              <div className="msg-role">{e.actor ? `✦ ${e.actor}` : "✦ 回答"}</div>
              <div className="bubble md">
                <Typewriter text={e.text} active={e.streaming === true} />
              </div>
            </>
          ) : (
            <div className="bubble">{e.text}</div>
          )}
        </div>
      );
    }
    if (e.kind === "thinking") {
      // 思考卡片：思考中默认展开，块结束默认收起；点击头部切换。
      const open = e.open ?? e.streaming === true;
      return (
        <div className={`entry-card think-card${e.streaming ? " streaming" : ""}`}>
          <button className="entry-head" onClick={() => toggleEntry(e.id)}>
            <span className="entry-icon">💭</span>
            <span className="entry-title">思考过程</span>
            {e.actor && <span className="actor-badge">{e.actor}</span>}
            {e.streaming && <span className="status running">thinking…</span>}
            <span className="entry-caret">{open ? "▾" : "▸"}</span>
          </button>
          {open && (
            <AutoScrollBody className="entry-body thinking-text md" active={e.streaming === true}>
              <Typewriter text={e.text} active={e.streaming === true} />
            </AutoScrollBody>
          )}
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
    if (e.kind === "confirm") {
      // 权限审批卡：每个挂起调用独立裁决；全部裁决后卡片消失。
      return (
        <div className="entry-card confirm-card">
          <div className="entry-head confirm-head">
            <span className="entry-icon">🔐</span>
            <span className="entry-title">工具授权请求</span>
            <span className="status ask">待确认</span>
          </div>
          <div className="entry-body confirm-body">
            {e.calls.map((call, idx) => {
              const name = String(call.name ?? "tool");
              const input = JSON.stringify(call.input ?? {}, null, 2);
              return (
                <div key={String(call.id ?? idx)} className="confirm-call">
                  <div className="confirm-tool">{name}</div>
                  <pre className="tool-io">{input}</pre>
                  <div className="confirm-actions">
                    <button
                      className="btn primary"
                      onClick={() => void resolveConfirm(e.id, call, true, "once")}
                    >
                      批准
                    </button>
                    <button
                      className="btn ghost"
                      title="批准并在本会话内始终允许该工具"
                      onClick={() => void resolveConfirm(e.id, call, true, "session")}
                    >
                      本会话允许
                    </button>
                    <button
                      className="btn ghost"
                      title="批准并永久允许该工具（跨会话生效）"
                      onClick={() => void resolveConfirm(e.id, call, true, "always")}
                    >
                      永久允许
                    </button>
                    <button
                      className="btn danger"
                      onClick={() => void resolveConfirm(e.id, call, false, "once")}
                    >
                      拒绝
                    </button>
                  </div>
                </div>
              );
            })}
          </div>
        </div>
      );
    }
    if (e.kind === "indicator") {
      return <div className="turn-indicator">{e.text}</div>;
    }
    // 工具卡片：运行中默认展开（参数/输出实时可见），结束默认收起；点击头部切换。
    const open = e.open ?? e.ok === undefined;
    return (
      <div className={`entry-card tool-card${e.ok === undefined ? " streaming" : ""}`}>
        <button className="entry-head" onClick={() => toggleEntry(e.id)}>
          <span className="entry-icon">🛠</span>
          <span className="entry-title tool-name">{e.name}</span>
          {e.actor && <span className="actor-badge">{e.actor}</span>}
          <span className={`status ${e.ok === undefined ? "" : e.ok ? "ok" : "err"}`}>
            {e.ok === undefined ? "running…" : e.ok ? "done" : "error"}
          </span>
          <span className="entry-caret">{open ? "▾" : "▸"}</span>
        </button>
        {open && (
          <AutoScrollBody className="entry-body tool-body" active={e.ok === undefined}>
            {e.args && <pre className="tool-io">{prettyArgs(e.args)}</pre>}
            {e.result !== undefined && <pre className="tool-io out">{e.result}</pre>}
          </AutoScrollBody>
        )}
      </div>
    );
  };
  // 切换运行方式（solo ↔ team）：以当前 agent 重新进入（App 层换实例重开会话）。
  const switchMode = (next: "solo" | "team") => {
    if (next === runtimeMode) return;
    if (next === "team" && workspace.members.length === 0) {
      void onConfirm?.(
        "切换到协同模式",
        "当前工作区尚未配置成员（lead 将单干）。\n可稍后在编辑工作区里添加成员。继续切换？",
      ).then((ok) => {
        if (ok) onModeChange(next);
      });
      return;
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
        <button className="btn ghost" onClick={newSession} disabled={busy}>
          + New session
        </button>
        <span className="spacer" />
        <span className="badge">{sessionId.slice(0, 8)}</span>
      </div>
      
      
      <div
        className="messages"
        ref={scrollRef}
        onScroll={() => {
          const el = scrollRef.current;
          if (!el) return;
          nearBottomRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
        }}
      >
        <div className="messages-inner" ref={innerScrollRef}>
        {entries.length === 0 && (
          <div className="empty-hero">
            <div className="hero-avatar" aria-hidden>
              {agent.name.slice(0, 1)}
            </div>
            <div className="hero-title">与 {agent.name} 开始对话</div>
            <div className="hero-sub">
              {workspace.kind === "collab"
                ? runtimeMode === "team"
                  ? "协同模式：可派发子代理并行干活"
                  : "单兵模式：编码、文件、检索、规划"
                : workspace.kind === "ops"
                  ? "远程运维：shell 执行（需确认）、文件管理、资料检索"
                  : "数据库：只读查询免确认，变更需确认"}
            </div>
            <div className="hero-tags">
              <span className="badge">Enter 发送</span>
              <span className="badge">Shift+Enter 换行</span>
              <span className="badge">Ctrl+V 粘贴图片</span>
              {supportsPlan && <span className="badge">计划模式先行</span>}
            </div>
          </div>
        )}
        {entries.map((e, idx) => {
          // 时间线行：左侧节点圆点 + 时间标签，右侧原条目渲染。
          const rowKind =
            e.kind === "msg"
              ? e.role
              : e.kind === "image" || e.kind === "file"
                ? "user"
                : e.kind;
          // 历史回放中同一条消息展开的多个条目共享消息时间戳：
          // 连续相同时间只在首条显示，避免「同一秒」逐行堆叠。
          const showTime = e.ts && e.ts !== entries[idx - 1]?.ts;
          return (
            <div key={e.id} className={`tl-row tl-${rowKind}`}>
              <span className="tl-dot" aria-hidden />
              <div className="tl-body">
                {showTime ? <div className="tl-time">{e.ts}</div> : null}
                {renderEntry(e)}
              </div>
            </div>
          );
        })}
        </div>
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
            {planActive ? "退出计划模式" : "计划模式"}
          </button>
        )}
        {planActive && (
          <span className="badge plan" style={{ alignSelf: "center" }}>
            计划模式
          </span>
        )}
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
            // IME 组词期间（中文输入法确认候选词）的 Enter 不触发发送/插话：
            // 否则「确认组词的 Enter + 发送的 Enter」会把同一条消息提交两次。
            if (e.nativeEvent.isComposing || e.keyCode === 229) return;
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
              ? "运行中输入将插入对话，下一步生效（Enter 插入）"
              : "输入消息…（Enter 发送，Shift+Enter 换行）"
          }
        />
        {busy ? (
          <button
            className="btn warn"
            onClick={() => void steer()}
            disabled={!input.trim() && attachments.length === 0}
          >
            插入
          </button>
        ) : (
          <button className="btn" onClick={() => void send()} disabled={!input.trim()}>
            发送
          </button>
        )}
        {busy && (
          <button className="btn danger" onClick={interrupt}>
            停止
          </button>
        )}
        <button className="btn ghost" onClick={clear} title="清空会话上下文（不可恢复）">
          清空
        </button>
      </div>
      {dragActive && <div className="drop-hint">松开以添加附件</div>}
      </div>
      {filesOpen ? (
        workspace.kind === "ops" ? (
          <ServersPanel
            onCollapse={() => setFilesOpen(false)}
            boundServers={workspace.bound_servers}
            onConfirm={onConfirm}
          />
        ) : workspace.kind === "db" ? (
          <DatabasesPanel
            onCollapse={() => setFilesOpen(false)}
            boundDatabases={workspace.bound_databases}
            onConfirm={onConfirm}
          />
        ) : (
          <FilePanel workspaceId={workspace.id} onCollapse={() => setFilesOpen(false)} />
        )
      ) : oxOpen ? (
        <OxDataPanel
          workspaceId={workspace.id}
          onCollapse={() => setOxOpen(false)}
          onToast={onToast}
        />
      ) : (
        <div className="file-panel-tabs">
          <button
            className="file-panel-tab"
            title={workspace.kind === "ops" ? "展开服务器面板" : workspace.kind === "db" ? "展开数据库面板" : "展开文件面板"}
            onClick={() => {
              setFilesOpen(true);
              setOxOpen(false);
            }}
          >
            {workspace.kind === "ops" ? "🖥" : workspace.kind === "db" ? "🗄" : "📁"}
          </button>
          <button
            className="file-panel-tab"
            title="展开黑板报与记忆面板"
            onClick={() => {
              setOxOpen(true);
              setFilesOpen(false);
            }}
          >
            📋
          </button>
        </div>
      )}
    </div>
  );
}
