// 会话组件：消息流 + composer + 事件归流渲染（按工作区归流——会话状态
// 属于工作区，同一智能体在不同工作区各有独立会话）。
// 事件处理对齐 Java AgentSessionView.apply 的思路：增量 apply 事件维护视图，
// AgentResult 为最终消息的权威来源，流式 delta 仅作过程展示。

import { formatTerminalTool } from "./terminalRouting";
import {
  Fragment,
  memo,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { invoke } from "@tauri-apps/api/core";
import { save } from "@tauri-apps/plugin-dialog";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { api, onAgentEvent, onTurnFinished, onTurnStarted, type AgentConfig, type AgentEventPayload, type ConfirmResultPayload, type SessionBinding, type WorkspaceConfig, type WorkspaceFileEntry } from "../api";
import OpsWorkbench from "./OpsWorkbench";
import TestWorkbench from "./TestWorkbench";
import DatabasesPanel from "./DatabasesPanel";
import OxDataPanel from "./OxDataPanel";
import BrowserPreviewPanel from "./BrowserPreviewPanel";
import { usePanelWidth } from "./usePanelWidth";
import DataViewerModal from "./DataViewerModal";

type Entry =
  | { kind: "msg"; id: string; role: "user" | "assistant" | "system"; text: string; ts?: string; streaming?: boolean; actor?: string }
  | { kind: "tool"; id: string; callId: string; name: string; args: string; result?: string; ok?: boolean; open?: boolean; ts?: string; actor?: string }
  | { kind: "thinking"; id: string; text: string; ts?: string; streaming?: boolean; open?: boolean; actor?: string }
  | { kind: "image"; id: string; dataUrl: string; ts?: string }
  | { kind: "file"; id: string; name: string; ts?: string }
  | { kind: "indicator"; id: string; text: string; ts?: string; since?: number }
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

/** 判重归一化：折叠空白差异——流式 delta 拼接文本与权威结果文本可能
 *  差换行/空格（逐字精确比对会漏判，导致同一份回复渲染两条）。 */
const normText = (t: string) => t.replace(/\s+/g, " ").trim();

/** 打字机：流式期间按帧渐进显示（自适应步长追平积压），结束后整段显示。 */function Typewriter({ text, active }: { text: string; active: boolean }) {
  const [shown, setShown] = useState(() => (active ? 0 : text.length));
  // 性能取舍：逐字 reveal 需要每帧对全文 slice 后重新走 remarkGfm 解析，
  // 长回答时 CPU 随文本长度线性上涨（流式 delta 本身已是渐进到达，
  // 视觉上仍是流式）。这里去掉 raf 逐字循环，直接渲染当前文本。
  void shown;
  void setShown;
  return (
    <>
      <Markdown remarkPlugins={[remarkGfm]}>{text}</Markdown>
      {active && <span className="type-cursor" aria-hidden />}
    </>
  );
}

/** memo 化的 Markdown 渲染：text 不变时跳过 remarkGfm 重解析。
 *  父级（entries 数组）任何 setState 都会重渲染消息列表，历史条目
 *  文本稳定——memo 掉后长会话的滚动/流式渲染成本大幅下降。
 *  pre 覆盖：代码块右上角加复制按钮（从子节点提取纯文本）。 */
const markdownComponents = {
  pre: ({ children }: { children?: ReactNode }) => (
    <div className="code-wrap">
      <button
        className="code-copy"
        title="复制代码"
        onClick={(ev) => {
          const wrap = ev.currentTarget.parentElement;
          const text = wrap?.querySelector("pre")?.textContent ?? "";
          void copyText(text);
          ev.currentTarget.textContent = "已复制";
          setTimeout(() => {
            const btn = wrap?.querySelector<HTMLButtonElement>(".code-copy");
            if (btn) btn.textContent = "复制";
          }, 1200);
        }}
      >
        复制
      </button>
      <pre>{children}</pre>
    </div>
  ),
};
const MemoMarkdown = memo(function MemoMarkdown({ text }: { text: string }) {
  return (
    <Markdown remarkPlugins={[remarkGfm]} components={markdownComponents}>
      {text}
    </Markdown>
  );
});

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
  // 与消息主体同策略：用户上滚离底 >40px 即暂停跟随，滚回底部恢复。
  const nearBottomRef = useRef(true);
  useEffect(() => {
    if (!active) return;
    const outer = outerRef.current;
    const inner = innerRef.current;
    if (!outer || !inner) return;
    nearBottomRef.current = true;
    outer.scrollTop = outer.scrollHeight;
    const ro = new ResizeObserver(() => {
      if (nearBottomRef.current) outer.scrollTop = outer.scrollHeight;
    });
    ro.observe(inner);
    const onScroll = () => {
      nearBottomRef.current =
        outer.scrollHeight - outer.scrollTop - outer.clientHeight < 40;
    };
    outer.addEventListener("scroll", onScroll, { passive: true });
    return () => {
      ro.disconnect();
      outer.removeEventListener("scroll", onScroll);
    };
  }, [active]);
  return (
    <div ref={outerRef} className={className} onScroll={undefined}>
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
  // 面板宽度（左缘拖拽调宽，localStorage 记忆；180–480px，对话区保底 420）。
  const { w: fpWidth, startResize: startFpResize } = usePanelWidth(
    "ox-fp-width",
    250,
    180,
    480,
  );
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
    <>
      <div className="fp-resize" onMouseDown={startFpResize} aria-hidden />
      <aside className="file-panel" style={{ width: fpWidth }}>
      <div className="file-panel-head">
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
    </>
  );
}

/** 原始错误串 → 可读文案（匹配不到的保持原样，不丢信息）。 */
function humanizeError(msg: string): string {
  const m = msg.toLowerCase();
  if (m.includes("program not found"))
    return "CLI 程序未找到：请确认已安装并加入 PATH";
  if (m.includes("connection reset") || m.includes("connection refused") || m.includes("timed out") || m.includes("timeout"))
    return "网络异常（连接被重置或超时）——请检查网络/代理后重试";
  if (m.includes("unauthorized") || m.includes("401") || m.includes("api key"))
    return "鉴权失败：API Key 缺失或无效，请在「智能体与模型」里检查配置";
  if (m.includes("rate limit") || m.includes("429"))
    return "触发模型限流（429）——稍等片刻后重试";
  return msg;
}

/** 复制文本到剪贴板（Tauri 剪贴板插件优先，降级 web API）。 */
async function copyText(text: string): Promise<void> {
  try {
    const mod = await import("@tauri-apps/plugin-clipboard-manager");
    await mod.writeText(text);
  } catch {
    await navigator.clipboard.writeText(text);
  }
}

// 「思考中…」计时：模型首 token 延迟（大上下文 prefill / 推理模型服务端
// 思考）可达数十秒——秒数实时跳动让等待可感知，避免误判卡死。
function Elapsed({ since }: { since: number }) {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, []);
  const s = Math.max(0, Math.round((now - since) / 1000));
  return <span className="elapsed"> · {s}s</span>;
}

export default function Conversation({
  workspace,
  agent,
  runtimeMode,
  sessionId,
  refreshKey,
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
  /** 清空信号：App 层清空当前会话后 +1，重拉历史（会话 id 不变）。 */
  refreshKey?: number;
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
  // 文件面板显隐（可收起成细条）；test 工作区默认收起，让位给测试工作台。
  const [filesOpen, setFilesOpen] = useState(workspace.kind !== "test");
  // .ox 数据面板（黑板报 + 记忆）：与文件/服务器/数据库面板互斥展开。
  const [oxOpen, setOxOpen] = useState(false);
  const [browserOpen, setBrowserOpen] = useState(false);
  // 测试工作台（test 工作区默认展开：需求/用例/任务/报告是这类工作区的主操作面）。
  const [testOpen, setTestOpen] = useState(workspace.kind === "test");
  const [planActive, setPlanActive] = useState(false);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  // 图片灯箱（点击消息图片放大查看；Esc/点击遮罩关闭）。
  const [lightbox, setLightbox] = useState<string | null>(null);
  // 「回到底部」悬浮钮：用户上滚离底 >80px 时出现（流式输出不打扰查看历史）。
  const [showJump, setShowJump] = useState(false);
  // 子 agent 条目折叠：已展开的连续 actor 段（key = 段首条目 id）。
  const [expandedActorGroups, setExpandedActorGroups] = useState<Set<string>>(
    new Set(),
  );
  // 输入历史（↑ 召回）：仅记录成功发出的文本，会话内存态。
  const inputHistoryRef = useRef<string[]>([]);
  const historyIdxRef = useRef(-1);
  // 草稿持久化：按 工作区+会话 隔离，切换会话不丢未发送内容。
  const draftKey = `ox-draft:${workspace.id}:${runtimeMode}:${sessionId}`;
  useEffect(() => {
    setInput(localStorage.getItem(draftKey) ?? "");
    historyIdxRef.current = -1;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [draftKey]);
  useEffect(() => {
    localStorage.setItem(draftKey, input);
  }, [draftKey, input]);

  // 子 agent 条目折叠预处理：连续 actor 段默认只显示前 3 条，其余收进
  // 「展开更多」——子 agent 一次任务动辄几十个工具卡，把主时间线撑得没法看。
  // 流式中的条目始终显示（折叠会藏掉正在发生的动作）。
  const actorFold = useMemo(() => {
    const hiddenByGroup = new Map<string, string>();
    const moreByAfterId = new Map<string, { key: string; count: number }>();
    let key = "";
    let count = 0;
    let thirdId = "";
    const flush = () => {
      if (key && count > 3) moreByAfterId.set(thirdId, { key, count: count - 3 });
      key = "";
      count = 0;
      thirdId = "";
    };
    for (const e of entries) {
      // image/file 等条目没有 actor/streaming 字段（union 分支差异）。
      const actor = "actor" in e ? e.actor : undefined;
      const streaming = "streaming" in e ? e.streaming : false;
      if (actor) {
        if (!key) key = e.id;
        count += 1;
        if (count === 3) thirdId = e.id;
        if (count > 3 && !streaming) hiddenByGroup.set(e.id, key);
      } else {
        flush();
      }
    }
    flush();
    return { hiddenByGroup, moreByAfterId };
  }, [entries]);

  // 会话与连接的绑定（db 工作区绑数据库、ops 工作区绑服务器）：面板跟随
  // 会话切换显示对应连接。绑定存后端会话目录，跨重启保留。
  const [binding, setBinding] = useState<SessionBinding>({});
  useEffect(() => {
    let alive = true;
    setBinding({});
    api
      .getSessionBinding(workspace.id, sessionId)
      .then((b) => {
        if (!alive) return;
        let next = b ?? {};
        // ops 自动工作区（绑定唯一服务器）：会话无绑定时自动绑上——
        // 服务器即工作区，选中即用，无需手选。绑定失败静默（档案可能
        // 已删，remote_shell 执行时会给出明确报错）。
        if (workspace.kind === "ops" && !next.server && (workspace.bound_servers?.length ?? 0) === 1) {
          const autoServer = workspace.bound_servers[0];
          next = { ...next, server: autoServer };
          void api.setSessionBinding(workspace.id, sessionId, "server", autoServer).catch(() => {});
        }
        setBinding(next);
      })
      .catch(() => alive && setBinding({}));
    return () => {
      alive = false;
    };
  }, [workspace.id, sessionId, workspace.kind, workspace.bound_servers]);
  const bindConnection = async (kind: "database" | "server", name: string | null) => {
    setBinding((prev) => ({ ...prev, [kind]: name ?? undefined }));
    try {
      await api.setSessionBinding(workspace.id, sessionId, kind, name);
    } catch (e) {
      onToast?.("error", String(e));
    }
  };

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

  // 流式 delta 微批：同一帧内的多条 delta 合并成一次 setEntries（rAF 对齐
    // 显示刷新）。每条 delta 单独 setState 会让 React 每秒重渲染消息列表
    // 几十次（长输出时 CPU 明显），合并后每帧至多一次。
    const pendingDeltaRef = useRef<Map<string, string>>(new Map());
    const deltaRafRef = useRef(0);
    const flushDeltas = useCallback(() => {
      if (deltaRafRef.current) {
        cancelAnimationFrame(deltaRafRef.current);
        deltaRafRef.current = 0;
      }
      const pending = pendingDeltaRef.current;
      if (pending.size === 0) return;
      pendingDeltaRef.current = new Map();
      setEntries((prev) =>
        prev.map((e) => {
          const add = pending.get(e.id);
          if (add === undefined) return e;
          if (e.kind === "msg") return { ...e, text: e.text + add };
          if (e.kind === "thinking") return { ...e, text: e.text + add };
          return e;
        }),
      );
    }, []);
    const queueDelta = useCallback((entryId: string, delta: string) => {
      pendingDeltaRef.current.set(entryId, (pendingDeltaRef.current.get(entryId) ?? "") + delta);
      if (!deltaRafRef.current) {
        deltaRafRef.current = requestAnimationFrame(() => {
          deltaRafRef.current = 0;
          flushDeltas();
        });
      }
    }, [flushDeltas]);

  // 卸载时取消未落帧的 delta 合并（防泄漏与卸后 setState）。
  useEffect(
    () => () => {
      if (deltaRafRef.current) cancelAnimationFrame(deltaRafRef.current);
    },
    [],
  );

  const finalizeStreams = useCallback(() => {
    flushDeltas(); // 块结束前把未落帧的 delta 一并写入（否则尾部文本丢失）
    const cur = streamRef.current;
    streamRef.current = null;
    if (cur) {
      upsert(cur.entryId, (e) => {
        if (e.kind === "msg") {
          if (e.role === "assistant" && e.text.trim()) {
            turnTextsRef.current.add(normText(e.text));
          }
          return { ...e, streaming: false };
        }
        if (e.kind === "thinking") return { ...e, streaming: false, open: false }; // 思考结束默认收起
        return e;
      });
    }
  }, [upsert, flushDeltas]);

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
  }, [workspace.id, sessionId, runtimeMode, refreshKey]);

  // 事件归流：只处理 (agent_id, session_id) 匹配本组件的事件。
  useEffect(() => {
    const applies = (p: { workspace_id: string; session_id: string }) =>
      p.workspace_id === workspace.id && p.session_id === sessionId;

    const un1 = onAgentEvent((payload: AgentEventPayload) => {
      if (!applies(payload)) return;
      // 事件在流 = turn 在跑：busy 失真自愈。send/steer/sendConfirm 的失败
      // 路径会盲目置 false，但后端 turn 并未停（如 steer 撞锁降级、确认调用
      // 失败）——这里统一拉回真值；turn 真正结束时 TURN_FINISHED 会再置 false。
      setBusySafe(true);
      const ev = payload.event;
      switch (ev?.type) {
        case "CONTEXT_COMPACTION_START":
          // 压缩要调一次辅助 LLM，可能耗时数分钟：indicator 提示「压缩中」
          // 并给出预期，避免长时间无输出被误判为卡死。
          setEntries((prev) => [
            ...prev,
            {
              kind: "indicator",
              text: "正在压缩上下文，预计需要几分钟，请稍等…",
              ts: nowTime(),
              id: nextId(),
              since: Date.now(),
            },
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
            {
              kind: "indicator",
              text: "思考中…",
              ts: nowTime(),
              id: nextId(),
              since: Date.now(),
            },
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
            queueDelta(cur.entryId, delta);
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
            queueDelta(cur.entryId, delta);
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
          // 恢复路径的工具开始执行没有新的 TOOL_CALL_START：结果增量到达
          // 即从确认卡移除对应项（显式批准/同类自动放行的调用不再留在待
          // 确认列表，全部清完卡片消失）。无变化时返回原引用避免重渲染。
          setEntries((prev) => {
            let changed = false;
            const next = prev
              .map((e) => {
                if (e.kind !== "confirm") return e;
                const rest = e.calls.filter((c) => String(c.id ?? "") !== callKey);
                if (rest.length === e.calls.length) return e;
                changed = true;
                return rest.length > 0 ? { ...e, calls: rest } : null;
              })
              .filter((x): x is Entry => x !== null);
            return changed ? next : prev;
          });
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
              turnTextsRef.current.add(normText(text));
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
                .filter((t) => t.trim() && !seen.has(normText(t)));
              if (fresh.length > 0) {
                fresh.forEach((t) => seen.add(normText(t)));
                setEntries((prev) => {
                  let next = prev;
                  for (const t of fresh) {
                    const nt = normText(t);
                    // 尾部最近的 assistant 条目 = 本 turn 刚定格的流式块。
                    // 若与本条互为前缀（流式截断/空白差异），补全替换而非
                    // 追加——否则同一份回复出现两条（截断的流式版 + 完整的
                    // 权威版）。只看尾部条目：跨 turn 的相似回复是模型真实
                    // 输出，不能吞。长度阈值防短文本误判。
                    let idx = -1;
                    for (let i = next.length - 1; i >= 0; i--) {
                      const e = next[i];
                      if (e.kind === "msg" && e.role === "assistant") {
                        idx = i;
                        break;
                      }
                    }
                    const dup = (() => {
                      if (idx < 0) return false;
                      const e = next[idx];
                      if (e.kind !== "msg") return false;
                      const et = normText(e.text);
                      if (et === nt) return true;
                      const short = Math.min(et.length, nt.length);
                      return short >= 20 && (et.startsWith(nt) || nt.startsWith(et));
                    })();
                    if (dup && idx >= 0) {
                      next = next.map((x, i) =>
                        i === idx && x.kind === "msg" && normText(x.text).length < nt.length
                          ? { ...x, text: t, streaming: false }
                          : x,
                      );
                      continue;
                    }
                    next = [
                      ...next,
                      { kind: "msg", role: "assistant", text: t, ts: nowTime(), id: nextId() },
                    ];
                  }
                  return next;
                });
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
        case "EXCEED_MAX_ITERS":
          // 达到最大轮次上限：明确告知（此前静默停止，用户误以为卡死/坏了）。
          finalizeStreams();
          setEntries((prev) => [
            ...prev.filter((e) => e.kind !== "indicator"),
            {
              kind: "indicator",
              text: `已达最大轮次上限（${ev.current_iter ?? "?"}/${ev.max_iters ?? "?"}）——直接回复可继续，或在「智能体与模型」里调高轮次上限`,
              ts: nowTime(),
              id: nextId(),
            },
          ]);
          break;
        case "ERROR": {
          // 核心层失败事件（模型调用报错等）：立即呈现错误，不让失败静默。
          // turn-finished(failed) 会再兜一次，这里保证错误在事件流上就可见。
          finalizeStreams();
          const message = typeof ev.message === "string" ? ev.message : "未知错误";
          setError(message);
          setEntries((prev) => [
            ...prev.filter((e) => e.kind !== "indicator"),
            {
              kind: "indicator",
              text: `本轮失败：${message}`,
              ts: nowTime(),
              id: nextId(),
            },
          ]);
          break;
        }
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
      if (workspace.kind === "ops") {
        setEntries((prev) => [...prev, {
          kind: "indicator", id: nextId(),
          text: payload.outcome === "interrupted" ? "本轮已中断（远端进程是否停止需另行确认）"
            : payload.outcome === "failed" ? `本轮失败：${payload.error ?? "未知错误"}`
            : prev.some((entry) => entry.kind === "confirm") ? "等待工具授权，并非执行中断" : "本轮已结束",
        }]);
      }
    });

    // steer 续轮：后端自动开启的新 turn（前端保持 busy、提示用户）。
    const un3 = onTurnStarted((payload) => {
      if (!applies(payload)) return;
      setBusySafe(true);
      turnTextsRef.current = new Set(); // 新 turn：判重集合清零
      turnToolSeenRef.current = false;
      setNotice("已插入对话——智能体继续处理。");
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

  // 全局快捷键：Esc 关灯箱（其次停止运行）；Ctrl/Cmd+K 聚焦会话搜索。
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      // Shell 的 Esc / Ctrl+K 属于终端，不能触发 Agent 的全局快捷键。
      if (e.target instanceof Element && e.target.closest(".ops-terminal")) return;
      if (e.key === "Escape") {
        if (lightbox) {
          setLightbox(null);
          return;
        }
        if (busyRef.current) void interrupt();
        return;
      }
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        document.querySelector<HTMLInputElement>("input[data-ss-search]")?.focus();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [lightbox]);

  // 发送失败恢复：把文本与附件放回输入区（否则用户重打一遍）。
  const restoreDraft = (
    text: string,
    atts: Array<{ name: string; media_type: string; data_base64: string; preview?: string }>,
  ) => {
    setInput(text);
    setAttachments(atts);
  };

  const send = async () => {
    const text = input.trim();
    // busy 检查走同步 ref：state 在同一渲染周期内不更新，双击/回车+点击
    // 会连发两条（见 busyRef 注释）。
    if ((!text && attachments.length === 0) || busyRef.current) return;
    setError("");
    setInput("");
    inputHistoryRef.current.push(text);
    historyIdxRef.current = -1;
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
            restoreDraft(text, attachments);
            setError(String(e2));
          }
        }
        return;
      }
      setBusySafe(false);
      // 发送失败：恢复输入与附件（否则用户重打一遍），错误给可读文案。
      restoreDraft(text, attachments);
      setError(humanizeError(msg));
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
    // db_dml 永不授权升级：数据变更每条语句影响面不同，必须逐次确认——
    // 「本会话允许/永久允许」对它一律降级为本次批准（后端写入口同样拒绝）。
    const effectiveScope = toolName === "db_dml" ? "once" : scope;
    const result: ConfirmResultPayload = {
      confirmed,
      tool_call: call,
      ...(effectiveScope !== "once"
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
    if (effectiveScope === "always" && confirmed && toolName) {
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
      setEntries((prev) => [...prev, { kind: "confirm", id: nextId(), calls: [call] }]);
      setError(String(err));
    }
  };

  // 批量裁决：一次处理确认卡里的全部挂起调用（全部批准/全部拒绝）。
  // 并行同批工具（如多个 edit_file）逐个点太繁琐；后端另有「批准一个、
  // 同批同名自动放行」兜底，这里提供显式的一次性批量入口。
  const resolveConfirmBulk = async (
    entryId: string,
    calls: Record<string, unknown>[],
    confirmed: boolean,
  ) => {
    setEntries((prev) => prev.filter((e) => e.id !== entryId));
    const results: ConfirmResultPayload[] = calls.map((call) => ({
      confirmed,
      tool_call: call,
    }));
    setBusySafe(true);
    try {
      await api.sendConfirm(workspace.id, runtimeMode, sessionId, results);
    } catch (err) {
      setBusySafe(false);
      setEntries((prev) => [...prev, { kind: "confirm", id: nextId(), calls }]);
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

  const newSession = async () => {
    // 有内容的会话先归档再开新的（数据保留、可从归档箱还原；后端会先停
    // 运行中的 turn）——开新会话即收尾当前会话，避免废弃/卡死会话堆积在
    // 常规列表里。空会话直接切，不产生垃圾归档；归档失败不阻断新建
    // （降级为只新建，错误提示但不拦路）。
    if (entries.length > 0) {
      const ok = onConfirm
        ? await onConfirm(
            "归档当前会话",
            "将归档当前会话并开启新会话。\n会话数据保留，可在左栏「归档箱」中还原。",
          )
        : true;
      if (!ok) return;
      try {
        await api.archiveSession(workspace.id, runtimeMode, sessionId);
      } catch (err) {
        setError(String(err));
      }
    }
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

  // 导出会话为 Markdown（用户经系统保存对话框选路径；写盘走 Rust command）。
  const exportMarkdown = async () => {
    const lines: string[] = [
      `# 会话导出 — ${agent.name}`,
      "",
      `- 工作区：${workspace.name}`,
      `- 会话 ID：${sessionId}`,
      `- 导出时间：${new Date().toLocaleString("zh-CN")}`,
      "",
      "---",
      "",
    ];
    for (const e of entries) {
      if (e.kind === "msg") {
        lines.push(`## ${e.role === "user" ? "👤 用户" : e.role === "system" ? "⚙ 系统" : `✦ ${e.actor ?? agent.name}`}`, "", e.text, "");
      } else if (e.kind === "thinking") {
        lines.push(`<details><summary>💭 思考过程</summary>`, "", e.text, "", `</details>`, "");
      } else if (e.kind === "tool") {
        lines.push(
          `> 🛠 **${e.name}** — ${e.ok === undefined ? "运行中" : e.ok ? "完成" : "出错"}${e.actor ? `（${e.actor}）` : ""}`,
          "",
        );
      } else if (e.kind === "image") {
        lines.push("> 🖼 [图片附件]", "");
      } else if (e.kind === "file") {
        lines.push(`> 📎 ${e.name}`, "");
      } else if (e.kind === "indicator") {
        lines.push(`> ℹ ${e.text}`, "");
      } else if (e.kind === "confirm") {
        lines.push(`> 🔐 工具授权请求（${e.calls.length} 项）`, "");
      }
    }
    try {
      const path = await save({
        defaultPath: `session-${sessionId.slice(0, 8)}.md`,
        filters: [{ name: "Markdown", extensions: ["md"] }],
      });
      if (!path) return;
      await invoke("write_export_file", { path, contents: lines.join("\n") });
      onToast?.("info", "会话已导出");
    } catch (e) {
      setError(humanizeError(String(e)));
    }
  };

  const clear = async () => {
    // 清空不可恢复：先确认（防手滑丢掉整个会话上下文）。
    const ok = await onConfirm?.("清空会话", "将清空当前会话的全部上下文（不可恢复）。确定继续？");
    if (!ok) return;
    try {
      await api.clearSession(workspace.id, runtimeMode, sessionId);
      setEntries([]);
    } catch (e) {
      setError(humanizeError(String(e)));
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
                {e.streaming ? (
                  <Typewriter text={e.text} active />
                ) : (
                  <MemoMarkdown text={e.text} />
                )}
              </div>
              {!e.streaming && e.text.trim() && (
                <div className="msg-actions">
                  <button title="复制全文" onClick={() => void copyText(e.text)}>
                    复制
                  </button>
                </div>
              )}
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
            {e.streaming && <span className="status running">思考中</span>}
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
          <img
            className="msg-img"
            src={e.dataUrl}
            alt="attachment"
            onClick={() => setLightbox(e.dataUrl)}
            title="点击查看大图"
          />
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
            {e.calls.length > 1 && (
              <div className="confirm-actions confirm-bulk">
                <button
                  className="btn primary"
                  onClick={() => void resolveConfirmBulk(e.id, e.calls, true)}
                >
                  全部批准（{e.calls.length} 项）
                </button>
                <button
                  className="btn danger"
                  onClick={() => void resolveConfirmBulk(e.id, e.calls, false)}
                >
                  全部拒绝
                </button>
              </div>
            )}
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
                    {name !== "db_dml" && (
                      <button
                        className="btn ghost"
                        title="批准并在本会话内始终允许该工具"
                        onClick={() => void resolveConfirm(e.id, call, true, "session")}
                      >
                        本会话允许
                      </button>
                    )}
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
      return (
        <div className="turn-indicator">
          {e.text}
          {e.since && <Elapsed since={e.since} />}
        </div>
      );
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
            {e.ok === undefined ? "运行中" : e.ok ? "完成" : "出错"}
          </span>
          <span className="entry-caret">{open ? "▾" : "▸"}</span>
        </button>
        {open && (
          <AutoScrollBody className="entry-body tool-body" active={e.ok === undefined}>
            {e.args && (
              <div className="tool-io-wrap">
                <button
                  className="tool-copy"
                  title="复制参数"
                  onClick={() => void copyText(prettyArgs(e.args))}
                >
                  复制
                </button>
                <pre className="tool-io">{prettyArgs(e.args)}</pre>
              </div>
            )}
            {e.result !== undefined && (
              <div className="tool-io-wrap">
                <button
                  className="tool-copy"
                  title="复制输出"
                  onClick={() => void copyText(e.result ?? "")}
                >
                  复制
                </button>
                <pre className="tool-io out">{e.result}</pre>
              </div>
            )}
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

  const content = (
    <div className="conversation">
      {lightbox && (
        <div className="lightbox" onClick={() => setLightbox(null)}>
          <img src={lightbox} alt="预览" />
        </div>
      )}
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
        <button
          className="btn ghost"
          onClick={() => void newSession()}
          disabled={busy}
          title="归档当前会话并开启新会话（空会话直接切换）"
        >
          + 新会话
        </button>
        <span className="spacer" />
        <button
          className="btn ghost"
          onClick={() => void exportMarkdown()}
          disabled={entries.length === 0}
          title="导出当前会话为 Markdown 文件"
        >
          导出
        </button>
        <span className="badge" title={`会话 ID：${sessionId}`}>
          会话 {sessionId.slice(0, 8)}
        </span>
      </div>
      
      
      <div className="messages-wrap">
      <div
        className="messages"
        ref={scrollRef}
        onScroll={() => {
          const el = scrollRef.current;
          if (!el) return;
          const gap = el.scrollHeight - el.scrollTop - el.clientHeight;
          nearBottomRef.current = gap < 80;
          setShowJump(gap > 120);
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
          // 子 agent 折叠：段内第 4 条起隐藏，段尾挂「展开更多」。
          const groupKey = actorFold.hiddenByGroup.get(e.id);
          if (groupKey && !expandedActorGroups.has(groupKey)) return null;
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
          const row = (
            <div className={`tl-row tl-${rowKind}`}>
              <span className="tl-dot" aria-hidden />
              <div className="tl-body">
                {showTime ? <div className="tl-time">{e.ts}</div> : null}
                {renderEntry(e)}
              </div>
            </div>
          );
          const more = actorFold.moreByAfterId.get(e.id);
          if (more && !expandedActorGroups.has(more.key)) {
            return (
              <Fragment key={e.id}>
                {row}
                <button
                  className="actor-more"
                  onClick={() =>
                    setExpandedActorGroups(
                      (prev) => new Set(prev).add(more.key),
                    )
                  }
                >
                  ↓ 展开子智能体的另外 {more.count} 条
                </button>
              </Fragment>
            );
          }
          return <Fragment key={e.id}>{row}</Fragment>;
        })}
        </div>
        {showJump && (
          <button
            className="jump-bottom"
            onClick={() => {
              const el = scrollRef.current;
              if (el) el.scrollTop = el.scrollHeight;
            }}
            title="回到底部"
          >
            ↓ 回到底部
          </button>
        )}
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
      {entries.some((e) => e.kind === "confirm") && (
        <button
          className="confirm-banner"
          onClick={() => {
            const el = document.querySelector(".entry-card.confirm-card");
            el?.scrollIntoView({ behavior: "smooth", block: "center" });
          }}
          title="点击定位到待确认的操作"
        >
          ⚠ 有待确认的操作（工具执行需要你的批准）——点击定位
        </button>
      )}
      <div className="composer">
        {supportsPlan && (
          <button
            className={`btn ghost icon-only${planActive ? " plan-on" : ""}`}
            onClick={togglePlan}
            disabled={busy}
            title={planActive ? "退出计划模式" : "计划模式"}
          >
            📋
          </button>
        )}
        {planActive && (
          <span className="badge plan" style={{ alignSelf: "center" }}>
            计划模式
          </span>
        )}
        <label className="btn ghost attach-btn" title="添加附件（图片直接看；Excel/PDF/CSV 自动解析成文本；其他文件原样附带，≤8MB）">
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
            // ↑/↓ 召回输入历史：仅在输入框为空（↑）或正在浏览历史时生效，
            // 不干扰光标移动与多行编辑。
            if (e.key === "ArrowUp" && !e.shiftKey) {
              const hist = inputHistoryRef.current;
              if ((e.currentTarget.value === "" || historyIdxRef.current >= 0) && hist.length > 0) {
                e.preventDefault();
                const idx = historyIdxRef.current < 0 ? hist.length - 1 : Math.max(0, historyIdxRef.current - 1);
                historyIdxRef.current = idx;
                setInput(hist[idx]);
              }
              return;
            }
            if (e.key === "ArrowDown" && !e.shiftKey && historyIdxRef.current >= 0) {
              e.preventDefault();
              const hist = inputHistoryRef.current;
              const idx = historyIdxRef.current + 1;
              if (idx >= hist.length) {
                historyIdxRef.current = -1;
                setInput("");
              } else {
                historyIdxRef.current = idx;
                setInput(hist[idx]);
              }
              return;
            }
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
            className="btn warn icon-only"
            onClick={() => void steer()}
            disabled={!input.trim() && attachments.length === 0}
            title="插入对话（运行中排队，下一步生效）"
          >
            ⤵
          </button>
        ) : (
          <button
            className="btn icon-only"
            onClick={() => void send()}
            disabled={!input.trim()}
            title="发送（Enter）"
          >
            ➤
          </button>
        )}
        {busy && (
          <button className="btn danger icon-only" onClick={interrupt} title="停止运行">
            ⏹
          </button>
        )}
        <button
          className="btn ghost icon-only"
          onClick={clear}
          title="清空会话上下文（不可恢复）"
        >
          🗑
        </button>
      </div>
      {dragActive && <div className="drop-hint">松开以添加附件</div>}
      </div>
      {workspace.kind === "ops" ? null : filesOpen ? (
        workspace.kind === "db" ? (
          <DatabasesPanel
            onCollapse={() => setFilesOpen(false)}
            boundDatabases={workspace.bound_databases}
            sessionBinding={binding.database ?? null}
            onBindDatabase={(name) => void bindConnection("database", name)}
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
      ) : browserOpen ? (
        <BrowserPreviewPanel onCollapse={() => setBrowserOpen(false)} />
      ) : testOpen ? (
        <TestWorkbench
          workspace={workspace}
          sessionId={sessionId}
          runtimeMode={runtimeMode}
          convBusy={busy}
          onToast={onToast ?? (() => {})}
          onCollapse={() => setTestOpen(false)}
        />
      ) : (
        <div className="file-panel-tabs">
          <button
            className="file-panel-tab"
            title={workspace.kind === "db" ? "展开数据库面板" : "展开文件面板"}
            onClick={() => {
              setFilesOpen(true);
              setOxOpen(false);
            }}
          >
            {workspace.kind === "db" ? "🗄" : "📁"}
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
          <button
            className="file-panel-tab"
            title="展开浏览器直播面板（Agent 正在操作的页面）"
            onClick={() => {
              setBrowserOpen(true);
              setFilesOpen(false);
              setOxOpen(false);
            }}
          >
            🌐
          </button>
          {workspace.kind === "test" && (
            <button
              className="file-panel-tab"
              title="展开测试工作台（需求 / 用例 / 任务 / 报告）"
              onClick={() => {
                setTestOpen(true);
                setFilesOpen(false);
                setOxOpen(false);
                setBrowserOpen(false);
              }}
            >
              🧪
            </button>
          )}
        </div>
      )}
    </div>
  );
  if (workspace.kind !== "ops") return content;
  return <OpsWorkbench workspace={workspace} server={binding.server ?? ""} busy={busy}
    onConfirm={onConfirm}
    onSelect={async (name) => {
      await api.setSessionBinding(workspace.id, sessionId, "server", name);
      setBinding((prev) => ({ ...prev, server: name }));
    }}
    lines={entries.flatMap<import("./terminalRouting").TerminalAgentLine>((entry) => {
      if (entry.kind === "msg") return [{ id: `${sessionId}:${entry.id}`, kind: entry.role === "user" ? "input" as const : "reply" as const, text: entry.text }];
      if (entry.kind === "indicator") return [{ id: `${sessionId}:${entry.id}`, kind: "reply" as const, text: entry.text }];
      if (entry.kind === "thinking") return [{ id: `${sessionId}:${entry.id}`, kind: "thinking" as const, text: entry.text }];
      if (entry.kind === "tool") return [{ id: `${sessionId}:${entry.id}`, kind: "tool" as const, text: formatTerminalTool(entry.name, entry.args, entry.result) }];
      if (entry.kind === "confirm") return [{ id: `${sessionId}:${entry.id}`, kind: "confirm" as const, text: "操作需要确认，请检查下方的工具参数并批准或拒绝。" }];
      return [];
    })}
    onAgentInput={async (text) => {
      if (busyRef.current) throw new Error("Agent 正在运行，请等待或停止当前任务");
      setBusySafe(true);
      setEntries((prev) => [...prev, { kind: "msg", role: "user", text, id: nextId() }]);
      try {
        await api.sendMessage(workspace.id, runtimeMode, sessionId, text);
      } catch (e) { setBusySafe(false); throw e; }
    }}>
      {error && <div className="file-panel-error" role="alert">{error}</div>}
      {busy && <button className="btn danger" onClick={() => void interrupt()}>停止 Agent</button>}
      {entries.filter((entry) => entry.kind === "confirm").map((entry) => <div key={entry.id}>{renderEntry(entry)}</div>)}
    </OpsWorkbench>;
}
