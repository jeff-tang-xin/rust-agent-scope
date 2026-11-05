// 测试工作区右侧工作台面板：需求 / 用例 / 任务 / 报告与 Bug。
//
// 分工（对齐 src-tauri/src/testing.rs 头注）：数据 CRUD、Excel/XMind 导入
// 解析、报告生成都在后端命令；本组件负责编排——
// - 用例生成：把需求发给会话里的测试智能体（sendMessage，需求附图随消息
//   以 Image 块提供），从回复末尾的 ```json {"cases":[...]} 解析草稿，预览
//   确认后批量导入；智能体判定信息不足时回复 {"clarifications":[...]}，
//   弹澄清窗收集用户回答（存回需求）后重新生成；
// - 任务执行：创建任务后逐用例发执行提示词，等 turn 结束取最后一条助手
//   回复解析 {"result":...}，回写结果；fail/blocked 直接提交 Bug（不重试，
//   对齐 TEST_PROMPT 的执行纪律）。
// 视图附加：用例流程图、任务甘特图（SVG 内联，无第三方依赖）、报告/Bug
// 导出（系统保存对话框 + write_export_file，仅 .md）。

import { useCallback, useEffect, useRef, useState } from "react";
import { save } from "@tauri-apps/plugin-dialog";
import {
  api,
  onTurnFinished,
  type AttachmentInput,
  type TestCase,
  type TestBug,
  type TestConfig,
  type TestRequirement,
  type TestTask,
  type TestingData,
  type WorkspaceConfig,
} from "../api";
import { usePanelWidth } from "./usePanelWidth";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import remarkBreaks from "remark-breaks";

/** 测试工作台内 LLM 文本（报告 / Bug 字段 / 评审条目）的统一 markdown 渲染。
 *  remark-breaks：模型常用单换行分段，按聊天惯例渲染成 <br>，避免挤成一段。 */
function TestMarkdown({ text }: { text: string }) {
  return <Markdown remarkPlugins={[remarkGfm, remarkBreaks]}>{text}</Markdown>;
}

type TestTab = "cases" | "tasks" | "reports" | "configs";

const EMPTY_DATA: TestingData = {
  requirements: [],
  cases: [],
  tasks: [],
  bugs: [],
  configs: [],
};

/** File → base64（无 padding 依赖，直接二进制串 btoa；分块防栈溢出）。 */
function fileToBase64(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => {
      const bytes = new Uint8Array(reader.result as ArrayBuffer);
      let bin = "";
      const chunk = 0x8000;
      for (let i = 0; i < bytes.length; i += chunk) {
        bin += String.fromCharCode(...bytes.subarray(i, i + chunk));
      }
      resolve(btoa(bin));
    };
    reader.onerror = () => reject(reader.error);
    reader.readAsArrayBuffer(file);
  });
}

/** 取回复里最后一个 ```json 代码块（解析失败返回 null，不猜）。 */
function extractJsonBlock(text: string): Record<string, unknown> | null {
  const matches = [...text.matchAll(/```json\s*([\s\S]*?)```/g)];
  for (let i = matches.length - 1; i >= 0; i--) {
    try {
      const v = JSON.parse(matches[i][1].trim());
      if (v && typeof v === "object") return v as Record<string, unknown>;
    } catch {
      // 最后一个块坏了就往前找（模型偶尔在块后追加说明）。
    }
  }
  return null;
}

/** 用例生成结果：用例草稿，或智能体判定信息不足时的澄清问题（需求澄清协议）。 */
type DraftResult =
  | { kind: "cases"; cases: TestCase[] }
  | { kind: "clarify"; questions: string[] };

/** 从 AI 回复解析用例草稿（{"cases":[...]} 协议）。 */
function parseGeneratedCases(obj: Record<string, unknown> | null): TestCase[] {
  const raw = obj?.cases;
  if (!Array.isArray(raw)) return [];
  const out: TestCase[] = [];
  for (const c of raw) {
    if (!c || typeof c !== "object") continue;
    const o = c as Record<string, unknown>;
    const title = String(o.title ?? "").trim();
    if (!title) continue;
    out.push({
      id: "",
      title,
      precondition: String(o.precondition ?? ""),
      steps: Array.isArray(o.steps) ? o.steps.map(String).filter((s) => s.trim()) : [],
      expected: String(o.expected ?? ""),
      priority: String(o.priority ?? "P2"),
      tags: Array.isArray(o.tags) ? o.tags.map(String) : [],
      requirement_id: "",
      source: "ai",
      created_at: 0,
    });
  }
  return out;
}

/** 用例审查结论（TEST_PROMPT 的审查协议输出）。 */
interface CaseReview {
  coverage: {
    score: number;
    covered: string[];
    missing: string[];
  };
  duplicates: string[];
  conflicts: string[];
  risk: "high" | "medium" | "low";
  suggestions: {
    title: string;
    steps: string[];
    expected: string;
    priority: string;
  }[];
  verdict: "pass" | "revise";
}

/** 从 AI 回复解析审查结论（{"coverage":...,"verdict":...} 协议）。 */
function parseCaseReview(obj: Record<string, unknown> | null): CaseReview | null {
  if (!obj) return null;
  const cov = (obj.coverage ?? {}) as Record<string, unknown>;
  const rawCovered = Array.isArray(cov.covered) ? cov.covered.map(String) : [];
  const rawMissing = Array.isArray(cov.missing) ? cov.missing.map(String) : [];
  const rawDup = Array.isArray(obj.duplicates) ? obj.duplicates.map(String) : [];
  const rawConflicts = Array.isArray(obj.conflicts) ? obj.conflicts.map(String) : [];
  const riskRaw = String(obj.risk ?? "medium");
  const risk =
    riskRaw === "high" || riskRaw === "medium" || riskRaw === "low" ? riskRaw : "medium";
  const rawSuggestions = Array.isArray(obj.suggestions) ? obj.suggestions : [];
  const suggestions: CaseReview["suggestions"] = [];
  for (const s of rawSuggestions) {
    if (!s || typeof s !== "object") continue;
    const o = s as Record<string, unknown>;
    const title = String(o.title ?? "").trim();
    if (!title) continue;
    suggestions.push({
      title,
      steps: Array.isArray(o.steps) ? o.steps.map(String).filter((x) => x.trim()) : [],
      expected: String(o.expected ?? ""),
      priority: String(o.priority ?? "P2"),
    });
  }
  const verdictRaw = String(obj.verdict ?? "revise");
  const verdict = verdictRaw === "pass" ? "pass" : "revise";
  const scoreRaw = Number(cov.score);
  const score = Number.isFinite(scoreRaw) ? Math.max(0, Math.min(100, scoreRaw)) : 0;
  return {
    coverage: { score, covered: rawCovered, missing: rawMissing },
    duplicates: rawDup,
    conflicts: rawConflicts,
    risk,
    suggestions,
    verdict,
  };
}

/** 执行结果协议（TEST_PROMPT 的输出契约）。 */
interface ExecOutcome {
  result: "pass" | "fail" | "blocked";
  actual: string;
  steps: Array<{ no: number; status: string; note: string }>;
  screenshots: string[];
  bug: {
    title?: string;
    severity?: string;
    steps?: string;
    expected?: string;
    actual?: string;
  } | null;
}

function parseExecOutcome(obj: Record<string, unknown> | null): ExecOutcome | null {
  if (!obj) return null;
  const result = String(obj.result ?? "");
  if (result !== "pass" && result !== "fail" && result !== "blocked") return null;
  const rawSteps = Array.isArray(obj.steps) ? obj.steps : [];
  return {
    result,
    actual: String(obj.actual ?? ""),
    steps: rawSteps
      .filter((s): s is Record<string, unknown> => !!s && typeof s === "object")
      .map((s) => ({
        no: Number(s.no ?? 0),
        status: String(s.status ?? ""),
        note: String(s.note ?? ""),
      })),
    screenshots: Array.isArray(obj.screenshots) ? obj.screenshots.map(String) : [],
    bug:
      obj.bug && typeof obj.bug === "object"
        ? (obj.bug as ExecOutcome["bug"])
        : null,
  };
}

const nowMs = () => Date.now();

/** AI 审查时一次性送进提示词的用例条数上限（超出按抽样审查并注明总数）。 */
const REVIEW_MAX_CASES = 60;

/** 单条用例的最长等待（超时即中断该轮并记为失败，继续下一条）。
 *
 * 不给上限的话，一旦某轮卡住（模型流停摆、浏览器 CDP 无响应），会话锁会被
 * 那一轮一直占着：前端干等，后端后续每条消息都返回 "a turn is already
 * running"，整批用例报废。 */
const CASE_TIMEOUT_MS = 10 * 60_000;

/** 一轮结束的等待结果。`timedOut` 让调用方能区分「跑完了但失败」与「根本没回来」。 */
type TurnWait = { ok: boolean; timedOut: boolean };

const fmtTime = (ms: number) =>
  ms > 0 ? new Date(ms).toLocaleString("zh-CN", { hour12: false }) : "—";

const STATUS_LABEL: Record<string, string> = {
  pending: "待执行",
  running: "执行中",
  pass: "通过",
  fail: "失败",
  blocked: "阻塞",
  error: "错误",
  done: "已完成",
  open: "待处理",
  fixed: "已修复",
  closed: "已关闭",
};

const STATUS_CLASS: Record<string, string> = {
  pending: "st-pending",
  running: "st-running",
  pass: "st-pass",
  fail: "st-fail",
  blocked: "st-blocked",
  error: "st-fail",
  done: "st-pass",
  open: "st-running",
  fixed: "st-pass",
  closed: "st-pending",
};

/** 执行提示词（与 TEST_PROMPT 的结果输出协议呼应；纪律部分提示词里已带）。
 *  任务引用了测试配置时，地址与账号从配置注入——账号密码只配一次，多任务复用。
 *  `dataProfile` 是对应用例所属需求登记的数据现状：执行时取值必须据实（精准查询
 *  用真实存在的数据、分页页码与日期断言落在真实范围内），否则会把「数据不够」
 *  误报成功能 Bug。 */
function execPrompt(
  task: TestTask,
  tc: TestCase,
  config?: TestConfig | null,
  dataProfile = "",
): string {
  const steps = tc.steps.map((s, i) => `${i + 1}. ${s}`).join("\n");
  const baseUrl = config?.base_url || task.base_url;
  const accounts = config?.accounts ?? [];
  const accountLines =
    accounts.length > 0
      ? [
          "测试账号（如需登录，按顺序尝试；全部失败记 blocked 并说明）：",
          ...accounts.map(
            (a, i) =>
              `${i + 1}. 用户名：${a.username}  密码：${a.password}${a.note ? `（${a.note}）` : ""}`,
          ),
        ]
      : [];
  return [
    "执行以下测试用例（被测系统以本工作区为准）：",
    "",
    `被测系统地址：${baseUrl || "（未填写，按用例步骤中的地址）"}`,
    ...accountLines,
    `用例标题：${tc.title}`,
    `前置条件：${tc.precondition || "无"}`,
    "测试步骤：",
    steps || "（无步骤，按标题理解执行）",
    `预期结果：${tc.expected}`,
    ...(config?.notes ? [`环境约定：${config.notes}`] : []),
    ...(dataProfile
      ? ["被测系统当前真实情况（用例里的具体值都必须以此为准，不要臆造）：", dataProfile]
      : []),
    "",
    "按测试工作区纪律执行：逐步执行并截图留证；某步失败或被阻塞时立即停止，不重试、不绕过。",
    "完成后在回复末尾按结果输出协议输出一个 ```json 代码块。",
  ].join("\n");
}

export default function TestWorkbench({
  workspace,
  sessionId,
  runtimeMode,
  convBusy,
  onToast,
  onCollapse,
}: {
  workspace: WorkspaceConfig;
  sessionId: string;
  /** 会话运行方式（与左侧会话一致，保证消息进同一个 session）。 */
  runtimeMode: "solo" | "team";
  /** 左侧会话是否正在运行（编排器发消息前检查，避免交叉对话）。 */
  convBusy: boolean;
  onToast: (kind: "info" | "error", text: string) => void;
  onCollapse: () => void;
}) {
  const { w: panelWidth, startResize } = usePanelWidth("ox-test-width", 340, 260, 620);
  const [data, setData] = useState<TestingData>(EMPTY_DATA);
  const [tab, setTab] = useState<TestTab>("cases");
  const [error, setError] = useState("");

  // 弹窗态（同一时刻至多一个；用可空对象而非布尔，省一遍 id 传递）。
  const [reqForm, setReqForm] = useState<TestRequirement | null>(null);
  const [caseForm, setCaseForm] = useState<TestCase | null>(null);
  const [taskForm, setTaskForm] = useState<{
    name: string;
    base_url: string;
    config_id: string;
    requirement_ids: string[];
    case_ids: string[];
  } | null>(null);
  const [importPreview, setImportPreview] = useState<{
    source: string;
    cases: TestCase[];
  } | null>(null);
  const [flowCase, setFlowCase] = useState<TestCase | null>(null);
  const [ganttTask, setGanttTask] = useState<TestTask | null>(null);
  const [reportView, setReportView] = useState<{ title: string; md: string } | null>(null);
  const [bugView, setBugView] = useState<TestBug | null>(null);
  // 测试配置表单（可复用的环境 + 账号资产）。
  const [configForm, setConfigForm] = useState<TestConfig | null>(null);

  // 编排器状态（同一时刻只跑一个任务）。
  const [runningTaskId, setRunningTaskId] = useState("");
  const [runProgress, setRunProgress] = useState("");
  const runningRef = useRef(false);
  // 需求澄清：智能体判定信息不足时弹窗收集用户回答，答完带答案重新生成。
  const [clarify, setClarify] = useState<{ req: TestRequirement; questions: string[] } | null>(
    null,
  );

  const reload = useCallback(async () => {
    try {
      setData(await api.testListData(workspace.id));
      setError("");
    } catch (e) {
      setError(String(e));
    }
  }, [workspace.id]);

  useEffect(() => {
    void reload();
  }, [reload]);

  const toast = (kind: "info" | "error", text: string) => {
    if (kind === "error") setError(text);
    onToast(kind, text);
  };

  // ---------- 会话编排原语 ----------

  /** 等指定 turn 结束（事件驱动 + 兜底超时）。 */
  const waitTurnFinished = (turnId: string, timeoutMs = CASE_TIMEOUT_MS) =>
    new Promise<TurnWait>((resolve) => {
      let settled = false;
      let timer = 0;
      // listen 注册是异步的（返回 Promise<unlisten>）：先占位、拿到后再替换，
      // 避免「已 settle 才拿到 unlisten」导致监听泄漏。
      let off: () => void = () => {};
      const finish = (ok: boolean, timedOut: boolean) => {
        if (settled) return;
        settled = true;
        window.clearTimeout(timer);
        off();
        resolve({ ok, timedOut });
      };
      const un = onTurnFinished((p) => {
        if (p.workspace_id === workspace.id && p.turn_id === turnId) {
          finish(p.outcome === "completed", false);
        }
      });
      void un
        .then((f) => {
          if (settled) f();
          else off = f;
        })
        .catch(() => {});
      timer = window.setTimeout(() => finish(false, true), timeoutMs);
    });

  /** 等一轮结束并取回复；超时/失败返回空串，同时带回失败原因（写进用例结果）。 */
  const awaitTurn = async (turnId: string): Promise<{ text: string; failure: string }> => {
    const wait = await waitTurnFinished(turnId);
    if (wait.timedOut) {
      // 必须中断：否则后端那一轮的会话锁一直不释放，后续用例全部发不出去。
      try {
        await api.interruptTurn(workspace.id, runtimeMode, sessionId);
      } catch {
        /* 已自然结束，忽略 */
      }
      return {
        text: "",
        failure: `执行超时（${CASE_TIMEOUT_MS / 60_000} 分钟没有返回结果），已中断该轮`,
      };
    }
    if (!wait.ok) return { text: "", failure: "本轮未正常完成（模型/工具报错或被中断）" };
    return { text: await lastAssistantText(), failure: "" };
  };

  /** 发送一条消息。会话锁在「中断 → 立刻重发」时可能还没释放（后端 turn_chain
   *  收尾是异步的），遇到占用短暂重试，而不是把这一条判死。 */
  const sendWithRetry = async (text: string, attachments?: AttachmentInput[]) => {
    for (let attempt = 0; ; attempt++) {
      try {
        return await api.sendMessage(workspace.id, runtimeMode, sessionId, text, attachments);
      } catch (e) {
        if (!String(e).includes("already running") || attempt >= 10) throw e;
        await new Promise((r) => setTimeout(r, 300));
      }
    }
  };

  /** 需求附图 → 附件块（图片走 Image 块）：读回 base64，按扩展名给 media_type。 */
  const loadRequirementImages = async (names: string[]): Promise<AttachmentInput[]> => {
    const out: AttachmentInput[] = [];
    for (const name of names) {
      try {
        const b64 = await api.testReadScreenshot(workspace.id, name);
        const ext = name.split(".").pop()?.toLowerCase() ?? "png";
        const mime =
          ext === "jpg" || ext === "jpeg"
            ? "image/jpeg"
            : ext === "webp"
              ? "image/webp"
              : ext === "gif"
                ? "image/gif"
                : "image/png";
        out.push({ name, media_type: mime, data_base64: b64 });
      } catch {
        // 单张图读不到（被手动清理）不阻断生成，提示词里已要求以文字为准。
      }
    }
    return out;
  };

  /** 取会话最后一条助手回复文本。 */
  const lastAssistantText = async () => {
    const hist = await api.getHistory(workspace.id, runtimeMode, sessionId);
    for (let i = hist.length - 1; i >= 0; i--) {
      if (hist[i].role === "assistant") return hist[i].text;
    }
    return "";
  };

  // ---------- 用例生成 / 导入 ----------

  /** 让会话里的测试智能体按需求生成用例草稿（不落库，返回给调用方决定）。
   *  需求附图随消息发给智能体（Image 块）；智能体判定信息不足时回复
   *  {"clarifications":[...]}，由调用方弹澄清窗，不报错。 */
  const requestCaseDrafts = async (
    req: TestRequirement,
    extraAnswers: string[] = [],
  ): Promise<DraftResult> => {
    if (convBusy || runningRef.current) {
      toast("error", "会话正在运行，请等待当前任务完成再生成用例");
      return { kind: "cases", cases: [] };
    }
    try {
      onToast("info", `已把需求「${req.title}」发给测试智能体，等待生成…`);
      const dataProfile = req.data_profile.trim();
      const clarifications = req.clarifications
        .map((c) => c.trim())
        .filter(Boolean)
        .concat(extraAnswers.map((a) => a.trim()).filter(Boolean));
      const prompt = [
        `请根据以下需求设计测试用例，并按用例生成协议在回复末尾输出一个 \`\`\`json 代码块（{"cases":[...]}）。`,
        "",
        `需求标题：${req.title}`,
        `需求内容：`,
        req.content || "（无补充描述，按标题理解）",
        ...(req.images.length > 0
          ? [`需求附图（${req.images.length} 张，随本消息提供）：流程/页面操作以此为准。`]
          : []),
        "",
        "被测系统数据现状：",
        dataProfile || "（未提供）",
        ...(clarifications.length > 0
          ? ["", "此前澄清（用户已确认，直接作为事实使用）：", ...clarifications.map((c, i) => `${i + 1}. ${c}`)]
          : []),
        "",
        "信息不足时先澄清：如果需求文字、附图、数据现状都不足以说清「流程每一步在哪个页面、点什么、填什么」，不要臆造操作步骤——按需求澄清协议输出 {\"clarifications\":[...]}，一次只走一个协议。",
        "",
        "用例必须与真实情况一致（违反即视为无效用例）：",
        "1. 用例里出现的任何具体值——页码、条数、日期、编号、名称、金额、状态——都要能从上面的数据现状或需求描述里找到出处。",
        "2. 找不到出处的，不要臆造一个「看起来合理」的值：改写成前置条件（先准备成什么状态），或参数化表述（取列表第一条、取当前时间、用上一步得到的值）。",
        "3. 客观条件不足以支撑某个场景时（数据量、时间范围、权限、状态等），把「需要先具备什么」写进前置条件，或如实说明该场景当前不可测，不要假设条件成立。",
        "4. 只有专门验证某个反向场景（如查询不存在的数据、超出真实范围的取值）时，才使用超出真实范围的值，并在标题里写明该场景。",
      ].join("\n");
      const attachments = await loadRequirementImages(req.images);
      const turn = await sendWithRetry(prompt, attachments.length > 0 ? attachments : undefined);
      const { text, failure } = await awaitTurn(turn.turn_id);
      if (failure) toast("error", `生成用例失败：${failure}`);
      const obj = extractJsonBlock(text);
      // 澄清回复优先：智能体按协议回了问题，转交调用方弹窗，不算失败。
      const rawClar = obj?.clarifications;
      if (Array.isArray(rawClar)) {
        const questions = rawClar.map(String).map((s) => s.trim()).filter(Boolean);
        if (questions.length > 0) return { kind: "clarify", questions };
      }
      const cases = parseGeneratedCases(obj).map((c) => ({
        ...c,
        requirement_id: req.id,
      }));
      if (!cases.length && !failure) {
        // 带上回复摘要：用户能直接看到智能体到底回了什么（而不是干巴巴的解析失败）。
        const brief = text.trim().replace(/\s+/g, " ").slice(0, 160);
        toast(
          "error",
          `未能从回复解析出用例（缺少 {"cases":[...]} 或 {"clarifications":[...]} 代码块）。回复摘要：${brief || "（空）"}——可在会话里让智能体重试`,
        );
      }
      return { kind: "cases", cases };
    } catch (e) {
      toast("error", `生成用例失败：${String(e)}`);
      return { kind: "cases", cases: [] };
    }
  };

  /** 让会话里的测试智能体按需求生成用例 → 解析 → 预览确认；
   *  智能体要求澄清时弹澄清窗，用户回答后带着答案重新生成。 */
  const generateCases = async (req: TestRequirement, extraAnswers: string[] = []) => {
    const result = await requestCaseDrafts(req, extraAnswers);
    if (result.kind === "clarify") {
      setClarify({ req, questions: result.questions });
      return;
    }
    if (!result.cases.length) return;
    setImportPreview({ source: "ai", cases: result.cases });
  };

  /** AI 审查用例覆盖度与全面性（上传导入前的关卡）：返回结构化审查结论。 */
  const reviewCases = async (
    cases: TestCase[],
    req: TestRequirement | null,
  ): Promise<CaseReview | null> => {
    if (convBusy || runningRef.current) {
      toast("error", "会话正在运行，请等待当前任务完成再审查");
      return null;
    }
    if (!cases.length) return null;
    try {
      onToast("info", `已把 ${cases.length} 条用例发给测试智能体，等待审查…`);
      // 用例过多时按条数截断（抽样审查），并把总数与抽样范围写进提示词——
      // 让模型在结论里如实注明，避免「静默只审了一部分」。
      const shown = cases.slice(0, REVIEW_MAX_CASES);
      const truncated = cases.length - shown.length;
      const lines = shown.map((c, i) => {
        const steps = c.steps.map((s) => `    ${s}`).join("\n");
        return `${i + 1}. [${c.priority}] ${c.title}\n  前置：${c.precondition || "无"}\n  步骤：\n${steps || "    （无）"}\n  预期：${c.expected || "（无）"}`;
      });
      const prompt = [
        `请审查以下测试用例的覆盖度与全面性，并按用例审查协议在回复末尾输出一个 \`\`\`json 代码块。`,
        "",
        req
          ? `关联需求：${req.title}\n需求内容：${req.content || "（无补充描述）"}`
          : "未关联具体需求：只能基于用例本身判断结构完整性与场景全面性，缺需求依据的部分请在结论里如实说明缺口。",
        "",
        `用例共 ${cases.length} 条${truncated > 0 ? `，本次只提供前 ${shown.length} 条（另有 ${truncated} 条未提供）` : ""}：`,
        ...lines,
      ].join("\n");
      const turn = await api.sendMessage(workspace.id, runtimeMode, sessionId, prompt);
      const { text, failure } = await awaitTurn(turn.turn_id);
      if (failure) toast("error", `审查失败：${failure}`);
      const review = parseCaseReview(extractJsonBlock(text));
      if (!review && !failure) {
        toast("error", "未能从回复解析出审查结论（缺少审查协议代码块），可在会话里让智能体重试");
      }
      return review;
    } catch (e) {
      toast("error", `审查失败：${String(e)}`);
      return null;
    }
  };

  const importFile = async (file: File, kind: "excel" | "xmind") => {
    try {
      const b64 = await fileToBase64(file);
      const cases =
        kind === "excel"
          ? await api.testParseExcel(workspace.id, b64)
          : await api.testParseXmind(workspace.id, b64);
      if (!cases.length) {
        toast("error", "文件里没有识别到用例（Excel 需含「用例标题/标题」表头行）");
        return;
      }
      setImportPreview({ source: kind, cases });
    } catch (e) {
      toast("error", `解析失败：${String(e)}`);
    }
  };

  /** 确认导入（只导入用户勾选的用例）。 */
  const confirmImport = async (picked: TestCase[]) => {
    if (!importPreview) return;
    if (!picked.length) {
      toast("error", "没有勾选任何用例");
      return;
    }
    try {
      const saved = await api.testImportCases(workspace.id, picked);
      onToast("info", `已导入 ${saved.length} 条用例`);
      setImportPreview(null);
      await reload();
    } catch (e) {
      toast("error", `导入失败：${String(e)}`);
    }
  };

  // ---------- 用例跨工作区复用（JSON / CSV 导出、JSON 导入） ----------

  const exportCasesJson = async (cases: TestCase[]) => {
    try {
      const path = await save({
        defaultPath: "test-cases.json",
        filters: [{ name: "JSON", extensions: ["json"] }],
      });
      if (!path) return;
      await api.writeExportFile(path, JSON.stringify(cases, null, 2));
      onToast("info", `已导出 ${cases.length} 条用例：${path}`);
    } catch (e) {
      toast("error", `导出失败：${String(e)}`);
    }
  };

  /** 导出 CSV（Excel 可直接打开；步骤多行合并为 `; `）。 */
  const exportCasesCsv = async (cases: TestCase[]) => {
    try {
      const path = await save({
        defaultPath: "test-cases.csv",
        filters: [{ name: "CSV", extensions: ["csv"] }],
      });
      if (!path) return;
      const cell = (v: string) => `"${v.replace(/"/g, '""').replace(/\r?\n/g, "; ")}"`;
      const header = ["用例标题", "前置条件", "步骤", "预期结果", "优先级", "标签", "需求"].map(
        cell,
      );
      const rows = cases.map((c) =>
        [
          c.title,
          c.precondition,
          c.steps.join("; "),
          c.expected,
          c.priority,
          c.tags.join("; "),
          data.requirements.find((r) => r.id === c.requirement_id)?.title ?? "",
        ]
          .map(cell)
          .join(","),
      );
      // BOM：Excel 打开中文 CSV 需要 UTF-8 BOM，否则中文乱码。
      await api.writeExportFile(path, `\uFEFF${[header.join(","), ...rows].join("\r\n")}`);
      onToast("info", `已导出 ${cases.length} 条用例（CSV）：${path}`);
    } catch (e) {
      toast("error", `导出失败：${String(e)}`);
    }
  };

  const importCasesJson = async (file: File) => {
    try {
      const arr = JSON.parse(await file.text());
      if (!Array.isArray(arr)) throw new Error("JSON 顶层必须是数组");
      const cases: TestCase[] = arr
        .filter((c): c is Record<string, unknown> => !!c && typeof c === "object")
        .map((c) => ({
          id: "",
          title: String(c.title ?? "").trim(),
          precondition: String(c.precondition ?? ""),
          steps: Array.isArray(c.steps) ? c.steps.map(String).filter((s) => s.trim()) : [],
          expected: String(c.expected ?? ""),
          priority: String(c.priority ?? "P2"),
          tags: Array.isArray(c.tags) ? c.tags.map(String) : [],
          requirement_id: "",
          source: "json",
          created_at: 0,
        }))
        .filter((c) => c.title);
      if (!cases.length) {
        toast("error", "文件里没有可导入的用例（每条需有 title 字段）");
        return;
      }
      setImportPreview({ source: "json", cases });
    } catch (e) {
      toast("error", `解析失败：${String(e)}`);
    }
  };

  // ---------- 导出 ----------

  const exportMd = async (title: string, md: string) => {
    try {
      const path = await save({
        defaultPath: `${title.replace(/[\\/:*?"<>|]/g, "_")}.md`,
        filters: [{ name: "Markdown", extensions: ["md"] }],
      });
      if (!path) return;
      await api.writeExportFile(path, md);
      onToast("info", `已导出：${path}`);
    } catch (e) {
      toast("error", `导出失败：${String(e)}`);
    }
  };

  return (
    <>
      <div className="fp-resize" onMouseDown={startResize} aria-hidden />
      <aside className="file-panel" style={{ width: panelWidth }}>
        <div className="file-panel-head">
          <span className="file-panel-title">🧪 测试工作台</span>
          <span className="spacer" />
          <button className="icon-btn" title="刷新" onClick={() => void reload()}>
            ⟳
          </button>
          <button className="icon-btn" title="收起面板" onClick={onCollapse}>
            ×
          </button>
        </div>

        <div className="test-tabs">
          <button
            className={`test-tab ${tab === "cases" ? "active" : ""}`}
            onClick={() => setTab("cases")}
          >
            需求与用例
          </button>
          <button
            className={`test-tab ${tab === "tasks" ? "active" : ""}`}
            onClick={() => setTab("tasks")}
          >
            测试任务
          </button>
          <button
            className={`test-tab ${tab === "reports" ? "active" : ""}`}
            onClick={() => setTab("reports")}
          >
            报告与 Bug
          </button>
          <button
            className={`test-tab ${tab === "configs" ? "active" : ""}`}
            onClick={() => setTab("configs")}
          >
            配置
          </button>
        </div>

        {error && <div className="file-panel-error">{error}</div>}

        {tab === "cases" && (
          <CasesTab
            data={data}
            busy={convBusy || runningRef.current}
            onNewReq={() =>
              setReqForm({ id: "", title: "", content: "", data_profile: "", images: [], clarifications: [], created_at: 0 })
            }
            onEditReq={setReqForm}
            onDeleteReq={async (r) => {
              try {
                await api.testDeleteRequirement(workspace.id, r.id);
                onToast("info", `需求「${r.title}」已删除（关联用例保留，仅解除关联）`);
                await reload();
              } catch (e) {
                toast("error", String(e));
              }
            }}
            onGenerate={generateCases}
            onNewCase={() =>
              setCaseForm({
                id: "",
                title: "",
                precondition: "",
                steps: [],
                expected: "",
                priority: "P2",
                tags: [],
                requirement_id: data.requirements[0]?.id ?? "",
                source: "manual",
                created_at: 0,
              })
            }
            onEditCase={setCaseForm}
            onDeleteCase={async (c) => {
              try {
                await api.testDeleteCase(workspace.id, c.id);
                onToast("info", `用例「${c.title}」已删除`);
                await reload();
              } catch (e) {
                toast("error", String(e));
              }
            }}
            onFlow={setFlowCase}
            onImportExcel={(f) => void importFile(f, "excel")}
            onImportXmind={(f) => void importFile(f, "xmind")}
            onImportJson={(f) => void importCasesJson(f)}
            onExportCases={(cases) => void exportCasesJson(cases)}
            onExportCasesCsv={(cases) => void exportCasesCsv(cases)}
          />
        )}

        {tab === "tasks" && (
          <TasksTab
            data={data}
            runningTaskId={runningTaskId}
            runProgress={runProgress}
            onNewTask={() =>
              setTaskForm({
                name: "",
                base_url: "",
                config_id: "",
                requirement_ids: [],
                case_ids: [],
              })
            }
            onEditTask={(t) =>
              setTaskForm({
                name: t.name,
                base_url: t.base_url,
                config_id: t.config_id,
                requirement_ids: t.requirement_ids,
                case_ids: t.case_ids,
              })
            }
            onDeleteTask={async (t) => {
              try {
                await api.testDeleteTask(workspace.id, t.id);
                onToast("info", `任务「${t.name}」已删除（Bug 保留）`);
                await reload();
              } catch (e) {
                toast("error", String(e));
              }
            }}
            onRun={(t, force) => void runTask(t, force)}
            onGantt={setGanttTask}
            onCaseFlow={(cid) => setFlowCase(data.cases.find((c) => c.id === cid) ?? null)}
            onBugView={(bid) => {
              const b = data.bugs.find((x) => x.id === bid);
              if (b) setBugView(b);
            }}
          />
        )}

        {tab === "reports" && (
          <ReportsTab
            data={data}
            onTaskReport={async (t) => {
              try {
                const md = await api.testBuildReport(workspace.id, t.id);
                setReportView({ title: `测试报告-${t.name}`, md });
              } catch (e) {
                toast("error", String(e));
              }
            }}
            onBugView={setBugView}
            onDeleteBug={async (b) => {
              try {
                await api.testDeleteBug(workspace.id, b.id);
                onToast("info", `Bug「${b.title}」已删除`);
                await reload();
              } catch (e) {
                toast("error", String(e));
              }
            }}
          />
        )}

        {tab === "configs" && (
          <ConfigsTab
            data={data}
            onNewConfig={() =>
              setConfigForm({ id: "", name: "", base_url: "", accounts: [], notes: "", created_at: 0 })
            }
            onEditConfig={setConfigForm}
            onDeleteConfig={async (c) => {
              try {
                await api.testDeleteConfig(workspace.id, c.id);
                onToast("info", `配置「${c.name}」已删除（引用它的任务回退用手填地址）`);
                await reload();
              } catch (e) {
                toast("error", String(e));
              }
            }}
          />
        )}
      </aside>

      {/* ---------- 弹窗层 ---------- */}

      {reqForm && (
        <ReqFormModal
          workspaceId={workspace.id}
          initial={reqForm}
          onClose={() => setReqForm(null)}
          onSave={async (r) => {
            try {
              await api.testSaveRequirement(workspace.id, r);
              setReqForm(null);
              await reload();
            } catch (e) {
              toast("error", String(e));
            }
          }}
        />
      )}

      {clarify && (
        <ClarifyModal
          req={clarify.req}
          questions={clarify.questions}
          onClose={() => setClarify(null)}
          onSubmit={async (answers) => {
            const { req, questions: qs } = clarify;
            setClarify(null);
            try {
              // 问答存回需求：下次生成作为「已确认事实」带进提示词，不再重复问。
              const merged = [
                ...req.clarifications,
                ...qs.map((q, i) => `问：${q}／答：${answers[i]?.trim() || "（未回答）"}`),
              ];
              const saved = await api.testSaveRequirement(workspace.id, {
                ...req,
                clarifications: merged,
              });
              await reload();
              void generateCases(saved);
            } catch (e) {
              toast("error", String(e));
            }
          }}
        />
      )}

      {caseForm && (
        <CaseFormModal
          initial={caseForm}
          requirements={data.requirements}
          onClose={() => setCaseForm(null)}
          onSave={async (c) => {
            try {
              await api.testSaveCase(workspace.id, c);
              setCaseForm(null);
              await reload();
            } catch (e) {
              toast("error", String(e));
            }
          }}
        />
      )}

      {taskForm && (
        <TaskFormModal
          initial={taskForm}
          data={data}
          onClose={() => setTaskForm(null)}
          onGenerateCases={requestCaseDrafts}
          onClarify={(req, questions) => setClarify({ req, questions })}
          onSave={async (t, autoRun, newCases) => {
            try {
              // AI 生成的用例先落库拿到 id，再与已有用例一起关联到任务。
              let caseIds = t.case_ids;
              if (newCases.length) {
                const savedCases = await api.testImportCases(workspace.id, newCases);
                caseIds = [...caseIds, ...savedCases.map((c) => c.id)];
                onToast("info", `已保存 ${savedCases.length} 条 AI 生成的用例`);
              }
              const saved = await api.testSaveTask(workspace.id, {
                ...t,
                case_ids: caseIds,
                id: "",
                status: "pending",
                results: [],
                created_at: 0,
                finished_at: 0,
              });
              setTaskForm(null);
              await reload();
              onToast("info", `任务「${saved.name}」已创建`);
              // 需求：创建后自动跑测试任务（逐用例执行）。
              if (autoRun) void runTask(saved);
            } catch (e) {
              toast("error", String(e));
            }
          }}
        />
      )}

      {importPreview && (
        <ImportPreviewModal
          source={importPreview.source}
          cases={importPreview.cases}
          requirements={data.requirements}
          onClose={() => setImportPreview(null)}
          onConfirm={(picked) => void confirmImport(picked)}
          onReview={reviewCases}
        />
      )}

      {flowCase && <FlowchartModal testCase={flowCase} onClose={() => setFlowCase(null)} />}

      {ganttTask && (
        <GanttModal
          task={data.tasks.find((t) => t.id === ganttTask.id) ?? ganttTask}
          cases={data.cases}
          onClose={() => setGanttTask(null)}
        />
      )}

      {reportView && (
        <ReportModal
          title={reportView.title}
          md={reportView.md}
          onClose={() => setReportView(null)}
          onExport={() => void exportMd(reportView.title, reportView.md)}
        />
      )}

      {configForm && (
        <ConfigFormModal
          initial={configForm}
          onClose={() => setConfigForm(null)}
          onSave={async (c) => {
            try {
              const saved = await api.testSaveConfig(workspace.id, c);
              setConfigForm(null);
              await reload();
              onToast("info", `配置「${saved.name}」已保存`);
            } catch (e) {
              toast("error", String(e));
            }
          }}
        />
      )}

      {bugView && (
        <BugModal
          workspaceId={workspace.id}
          bug={bugView}
          cases={data.cases}
          tasks={data.tasks}
          onClose={() => setBugView(null)}
          onChanged={async () => {
            await reload();
            const fresh = data.bugs.find((b) => b.id === bugView.id);
            if (fresh) setBugView(fresh);
          }}
          onExport={async (b) => {
            try {
              const md = await api.testBuildBugReport(workspace.id, b.id);
              await exportMd(`Bug报告-${b.title}`, md);
            } catch (e) {
              toast("error", String(e));
            }
          }}
        />
      )}
    </>
  );

  // ---------- 任务执行编排（逐用例；fail/blocked 直接提交 Bug，不重试） ----------
  //
  // 编排器跑在前端 async 循环里：刷新页面/关窗口会让循环凭空消失，任务就永远
  // 停在 running。所以「中断 → 刷新 → 续跑」是必须支持的路径：任务状态是
  // running 时按钮显示「继续执行」，点它按用例结果续跑（已通过的跳过）。

  async function runTask(task: TestTask, force = false) {
    if (runningRef.current) {
      toast("error", "已有任务在执行中");
      return;
    }
    if (convBusy) {
      toast("error", "会话正在运行，请等待或停止当前对话再执行任务");
      return;
    }
    // 全量重跑会覆盖全部已有结果，属不可逆操作，先确认再动。
    if (
      force &&
      !window.confirm(
        `强制全量重跑「${task.name}」？\n\n将逐条重跑全部用例，已有执行结果会被覆盖（Bug 记录保留）。`,
      )
    ) {
      return;
    }
    runningRef.current = true;
    setRunningTaskId(task.id);
    try {
      // 上次执行被刷新/崩溃打断时，后端那个 turn 可能还在跑并占着会话锁，
      // 后续每条消息都会被拒（"a turn is already running"）——这就是「刷新后
      // 续不上」。先读最新任务，确认是中断残留再清一次：interrupt_turn 幂等，
      // 没有运行中的 turn 时后端返回 false，无副作用。
      const fresh = await api.testListData(workspace.id);
      const t = fresh.tasks.find((x) => x.id === task.id);
      if (!t) {
        toast("error", "任务不存在（可能已被删除）");
        return;
      }
      if (t.status === "running") {
        try {
          await api.interruptTurn(workspace.id, runtimeMode, sessionId);
        } catch {
          /* 没有可中断的 turn，忽略 */
        }
      }
      const casesById = new Map(fresh.cases.map((c) => [c.id, c]));
      // 用例所属需求的数据现状（执行时注入，避免模型自己去编查询值/页码/日期）。
      const dataProfileOf = (c: TestCase) =>
        fresh.requirements.find((r) => r.id === c.requirement_id)?.data_profile?.trim() ?? "";
      // 任务引用的测试配置（环境 + 账号）；配置已删则回退任务自带 base_url。
      const config = fresh.configs.find((c) => c.id === t.config_id) ?? null;
      const results = t.results.map((r) => ({ ...r }));

      // 每条用例最近一次 Bug（一条用例可能有多条历史 Bug，取创建时间最新的）。
      // 重跑筛选与「不重复建 Bug」都依赖它。
      const latestBugByCase = new Map<string, TestBug>();
      for (const b of fresh.bugs) {
        if (!b.case_id) continue;
        const prev = latestBugByCase.get(b.case_id);
        if (!prev || b.created_at >= prev.created_at) latestBugByCase.set(b.case_id, b);
      }
      /** 该用例是否还有「待处理」的 Bug（未修复 → 重跑没有意义，等修）。 */
      const openBugOf = (caseId: string) => {
        const b = latestBugByCase.get(caseId);
        return b && b.status === "open" ? b : null;
      };

      // 重跑筛选用例：只跑「Bug 已修复（含已关闭）」和「还没跑过」的。
      // - 无结果 / pending / running（上次中断残留）→ 跑；
      // - pass → 跳过（已通过，重跑只会白烧一轮模型）；
      // - fail / blocked / error → 看该用例最近一条 Bug：仍 open 就跳过，
      //   已 fixed/closed（或无 Bug，如协议解析失败）才重跑。
      const targets: string[] = [];
      let skippedPassed = 0;
      let skippedOpenBug = 0;
      if (force) {
        // 强制全量重跑：无视「已通过 / 待修 Bug」筛选，逐条重跑（已有结果被覆盖）。
        targets.push(...t.case_ids);
      } else {
        for (const cid of t.case_ids) {
          const st = results.find((x) => x.case_id === cid)?.status ?? "pending";
          if (st === "pending" || st === "running") {
            targets.push(cid);
            continue;
          }
          if (st === "pass") {
            skippedPassed += 1;
            continue;
          }
          if (openBugOf(cid)) {
            skippedOpenBug += 1;
            continue;
          }
          targets.push(cid);
        }
      }
      if (!targets.length) {
        // 全部跳过仍要给任务收尾：上次被打断的任务若剩的都在等 Bug 修，
        // 不置 done 就会永远显示「继续执行」。
        if (t.status === "running") {
          await api.testUpdateTask(workspace.id, {
            ...t,
            status: "done",
            results,
            finished_at: nowMs(),
          });
          await reload();
        }
        onToast(
          "info",
          t.case_ids.length
            ? `没有需要执行的用例：已通过 ${skippedPassed} 条、待修 Bug 未处理 ${skippedOpenBug} 条（重跑只跑「已修复」与「未跑过」的用例；需要全跑请用「全量重跑」）`
            : "任务没有关联用例，无法执行",
        );
        return;
      }
      const skipNote = [
        skippedPassed > 0 ? `已通过 ${skippedPassed} 条` : "",
        skippedOpenBug > 0 ? `待修 Bug 未处理 ${skippedOpenBug} 条` : "",
      ]
        .filter(Boolean)
        .join("、");
      if (skipNote) {
        onToast("info", `本次跳过 ${skipNote}；将执行 ${targets.length} 条`);
      }

      let done = 0;
      for (let i = 0; i < targets.length; i++) {
        const cid = targets[i];
        const ri = results.findIndex((r) => r.case_id === cid);
        if (ri < 0) continue;
        const tc = casesById.get(cid);
        if (!tc) {
          results[ri] = {
            ...results[ri],
            status: "error",
            actual: "用例已被删除，无法执行",
            finished_at: nowMs(),
          };
          done++;
          continue;
        }
        setRunProgress(`${i + 1}/${targets.length}：${tc.title}`);
        // 新一轮执行先清掉上一次的 Bug 关联与原始回复：本次还没结论，
        // 留着旧 bug_id 会让「通过」的用例仍显示挂着 Bug。
        results[ri] = {
          ...results[ri],
          status: "running",
          started_at: nowMs(),
          bug_id: "",
          raw_reply: "",
        };
        await api.testUpdateTask(workspace.id, { ...t, status: "running", results });
        await reload();

        // 卡住是「看不出在等还是死了」的根源：每 5 秒把已等秒数写进进度，
        // 用户才知道该继续等还是该中断。
        const caseStart = Date.now();
        const ticker = window.setInterval(() => {
          const sec = Math.round((Date.now() - caseStart) / 1000);
          setRunProgress(`${i + 1}/${targets.length}：${tc.title}（已等 ${sec}s）`);
        }, 5000);

        let reply = "";
        // 本轮失败的真实原因（超时/发送失败/非正常结束）。以前一律吞成空串，
        // 面板只能显示笼统的「解析失败」，看不出到底卡在哪一步。
        let failure = "";
        try {
          // 用例所属需求的附图随执行消息发给智能体：执行时页面长什么样、
          // 流程怎么走，图比文字可靠（与生成侧同一份图源）。
          const reqImages = (() => {
            const rid = tc.requirement_id;
            if (!rid) return [];
            return data.requirements.find((x) => x.id === rid)?.images ?? [];
          })();
          const attachments = await loadRequirementImages(reqImages);
          const turn = await sendWithRetry(
            execPrompt(t, tc, config, dataProfileOf(tc)),
            attachments.length > 0 ? attachments : undefined,
          );
          const r = await awaitTurn(turn.turn_id);
          reply = r.text;
          failure = r.failure;
        } catch (e) {
          failure = `发送失败：${String(e)}`;
        } finally {
          window.clearInterval(ticker);
        }
        const outcome = parseExecOutcome(extractJsonBlock(reply));
        if (outcome) {
          results[ri] = {
            ...results[ri],
            status: outcome.result,
            actual: outcome.actual,
            steps: outcome.steps,
            screenshots: outcome.screenshots,
            raw_reply: reply,
            finished_at: nowMs(),
          };
          // fail/blocked → 直接提交 Bug（需求 6：阻塞不重试）。
          if (outcome.result !== "pass") {
            // 同一用例已有「待处理」Bug 时不再新建：重跑会把同一问题刷成一堆
            // 重复报告。Bug 已修复/已关闭后再次失败，说明是回归或新问题，才另建一条。
            const existing = openBugOf(tc.id);
            if (existing) {
              results[ri].bug_id = existing.id;
              onToast("info", `已存在待处理 Bug「${existing.title}」，本次不重复创建`);
            } else {
              const b = outcome.bug ?? {};
              const bug: TestBug = {
                id: "",
                title: b.title?.trim() || `${tc.title}（${outcome.result === "blocked" ? "执行阻塞" : "用例失败"}）`,
                severity: b.severity?.trim() || "P2",
                steps:
                  b.steps?.trim() ||
                  tc.steps.map((s, si) => `${si + 1}. ${s}`).join("\n") ||
                  tc.title,
                expected: b.expected?.trim() || tc.expected,
                actual: b.actual?.trim() || outcome.actual,
                screenshots: outcome.screenshots,
                case_id: tc.id,
                task_id: t.id,
                status: "open",
                created_at: 0,
              };
              try {
                const saved = await api.testSaveBug(workspace.id, bug);
                results[ri].bug_id = saved.id;
                // 新 Bug 立刻进索引：同一次任务里后面的用例不会重复建，
                // 也保证本次后续重跑筛选看到的是最新状态。
                latestBugByCase.set(tc.id, saved);
                onToast("info", `已提交 Bug：${saved.title}`);
              } catch (e) {
                toast("error", `Bug 提交失败：${String(e)}`);
              }
            }
          }
        } else {
          results[ri] = {
            ...results[ri],
            status: "error",
            // 优先展示真实失败原因（超时/发送失败），否则才是协议解析问题。
            actual: failure || "未能从回复解析出结果 JSON（协议见测试智能体提示词）",
            raw_reply: reply,
            finished_at: nowMs(),
          };
        }
        done++;
        // 跳过的不计入 total，否则任务永远收不了尾。
        const isLast = done === targets.length;
        await api.testUpdateTask(workspace.id, {
          ...t,
          status: isLast ? "done" : "running",
          results,
          finished_at: isLast ? nowMs() : 0,
        });
        await reload();
      }
      onToast(
        "info",
        `任务「${t.name}」执行完成：本次执行 ${targets.length} 条${
          skippedPassed + skippedOpenBug > 0 ? `，跳过 ${skippedPassed + skippedOpenBug} 条` : ""
        }`,
      );
    } catch (e) {
      toast("error", `任务执行中断：${String(e)}`);
    } finally {
      runningRef.current = false;
      setRunningTaskId("");
      setRunProgress("");
      await reload();
    }
  }
}

// ==================== 页签：需求与用例 ====================

function CasesTab({
  data,
  busy,
  onNewReq,
  onEditReq,
  onDeleteReq,
  onGenerate,
  onNewCase,
  onEditCase,
  onDeleteCase,
  onFlow,
  onImportExcel,
  onImportXmind,
  onImportJson,
  onExportCases,
  onExportCasesCsv,
}: {
  data: TestingData;
  busy: boolean;
  onNewReq: () => void;
  onEditReq: (r: TestRequirement) => void;
  onDeleteReq: (r: TestRequirement) => void;
  onGenerate: (r: TestRequirement) => void;
  onNewCase: () => void;
  onEditCase: (c: TestCase) => void;
  onDeleteCase: (c: TestCase) => void;
  onFlow: (c: TestCase) => void;
  onImportExcel: (f: File) => void;
  onImportXmind: (f: File) => void;
  onImportJson: (f: File) => void;
  /** 导出当前筛选下的用例（跨工作区复用：导出 → 另一个工作区导入）。 */
  onExportCases: (cases: TestCase[]) => void;
  /** 导出当前筛选下的用例为 CSV（Excel 可直接打开）。 */
  onExportCasesCsv: (cases: TestCase[]) => void;
}) {
  const excelRef = useRef<HTMLInputElement>(null);
  const xmindRef = useRef<HTMLInputElement>(null);
  const jsonRef = useRef<HTMLInputElement>(null);
  const [reqFilter, setReqFilter] = useState("");

  const cases = reqFilter
    ? data.cases.filter((c) => c.requirement_id === reqFilter)
    : data.cases;

  return (
    <div className="test-body">
      <div className="test-section">
        <div className="test-section-head">
          <span className="test-section-title">需求（{data.requirements.length}）</span>
          <span className="spacer" />
          <button className="btn" onClick={onNewReq}>
            + 需求
          </button>
        </div>
        {data.requirements.length === 0 ? (
          <div className="file-empty">
            还没有需求。先录入需求，再让智能体按需求生成用例。
          </div>
        ) : (
          <div className="test-list">
            {data.requirements.map((r) => (
              <div key={r.id} className="test-item">
                <div className="test-item-main">
                  <div className="test-item-title" title={r.title}>
                    {r.title}
                  </div>
                  <div className="test-item-sub">
                    {casesOfReqCount(data, r.id)} 条用例 · {fmtTime(r.created_at)}
                  </div>
                </div>
                <div className="test-item-actions">
                  <button
                    className="btn"
                    disabled={busy}
                    title="把需求发给会话里的测试智能体生成用例"
                    onClick={() => onGenerate(r)}
                  >
                    AI 生成用例
                  </button>
                  <button className="icon-btn" title="编辑" onClick={() => onEditReq(r)}>
                    ✎
                  </button>
                  <button
                    className="icon-btn"
                    title="删除"
                    onClick={() => onDeleteReq(r)}
                  >
                    🗑
                  </button>
                </div>
              </div>
            ))}
          </div>
        )}
      </div>

      <div className="test-section">
        <div className="test-section-head">
          <span className="test-section-title">
            用例（{cases.length}
            {reqFilter ? `/${data.cases.length}` : ""}）
          </span>
          <span className="spacer" />
          <button className="btn" onClick={onNewCase}>
            + 用例
          </button>
          <button className="btn" onClick={() => excelRef.current?.click()}>
            导入 Excel
          </button>
          <button className="btn" onClick={() => xmindRef.current?.click()}>
            导入 XMind
          </button>
          <button className="btn" onClick={() => jsonRef.current?.click()} title="导入本工具导出的用例 JSON">
            导入 JSON
          </button>
          <button
            className="btn"
            disabled={cases.length === 0}
            title="把当前筛选下的用例导出为 JSON，可在其他测试工作区导入复用"
            onClick={() => onExportCases(cases)}
          >
            导出 JSON
          </button>
          <button
            className="btn"
            disabled={cases.length === 0}
            title="把当前筛选下的用例导出为 CSV，Excel 可直接打开"
            onClick={() => onExportCasesCsv(cases)}
          >
            导出 CSV
          </button>
        </div>
        <input
          ref={jsonRef}
          type="file"
          accept=".json"
          style={{ display: "none" }}
          onChange={(e) => {
            const f = e.target.files?.[0];
            if (f) onImportJson(f);
            e.target.value = "";
          }}
        />
        <input
          ref={excelRef}
          type="file"
          accept=".xlsx,.xls"
          style={{ display: "none" }}
          onChange={(e) => {
            const f = e.target.files?.[0];
            if (f) onImportExcel(f);
            e.target.value = "";
          }}
        />
        <input
          ref={xmindRef}
          type="file"
          accept=".xmind"
          style={{ display: "none" }}
          onChange={(e) => {
            const f = e.target.files?.[0];
            if (f) onImportXmind(f);
            e.target.value = "";
          }}
        />
        {data.requirements.length > 0 && (
          <select
            className="field test-filter"
            value={reqFilter}
            onChange={(e) => setReqFilter(e.target.value)}
          >
            <option value="">全部需求</option>
            {data.requirements.map((r) => (
              <option key={r.id} value={r.id}>
                {r.title}
              </option>
            ))}
          </select>
        )}
        {cases.length === 0 ? (
          <div className="file-empty">
            暂无用例：可让智能体按需求生成、导入 Excel/XMind，或手工新建。
          </div>
        ) : (
          <div className="test-list">
            {cases.map((c) => (
              <div key={c.id} className="test-item">
                <div className="test-item-main">
                  <div className="test-item-title" title={c.title}>
                    <span className={`badge ${prioClass(c.priority)}`}>{c.priority}</span>{" "}
                    {c.title}
                  </div>
                  <div className="test-item-sub">
                    {c.steps.length} 步 · {sourceLabel(c.source)}
                    {c.requirement_id
                      ? ` · ${data.requirements.find((r) => r.id === c.requirement_id)?.title ?? "已关联需求"}`
                      : " · 未关联需求"}
                  </div>
                </div>
                <div className="test-item-actions">
                  <button className="icon-btn" title="流程图" onClick={() => onFlow(c)}>
                    ⛭
                  </button>
                  <button className="icon-btn" title="编辑" onClick={() => onEditCase(c)}>
                    ✎
                  </button>
                  <button className="icon-btn" title="删除" onClick={() => onDeleteCase(c)}>
                    🗑
                  </button>
                </div>
              </div>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

const casesOfReqCount = (data: TestingData, reqId: string) =>
  data.cases.filter((c) => c.requirement_id === reqId).length;

const prioClass = (p: string) =>
  p === "P1" ? "prio-p1" : p === "P2" ? "prio-p2" : "prio-p3";

const sourceLabel = (s: string) =>
  s === "ai"
    ? "AI 生成"
    : s === "excel"
      ? "Excel 导入"
      : s === "xmind"
        ? "XMind 导入"
        : s === "json"
          ? "JSON 导入"
          : "手工";

// ==================== 页签：测试任务 ====================

function TasksTab({
  data,
  runningTaskId,
  runProgress,
  onNewTask,
  onEditTask,
  onDeleteTask,
  onRun,
  onGantt,
  onCaseFlow,
  onBugView,
}: {
  data: TestingData;
  runningTaskId: string;
  runProgress: string;
  onNewTask: () => void;
  onEditTask: (t: TestTask) => void;
  onDeleteTask: (t: TestTask) => void;
  onRun: (t: TestTask, force?: boolean) => void;
  onGantt: (t: TestTask) => void;
  onCaseFlow: (caseId: string) => void;
  onBugView: (bugId: string) => void;
}) {
  if (data.tasks.length === 0) {
    return (
      <div className="test-body">
        <div className="test-section">
          <div className="test-section-head">
            <span className="test-section-title">测试任务</span>
            <span className="spacer" />
            <button className="btn" onClick={onNewTask}>
              + 任务
            </button>
          </div>
          <div className="file-empty">
            还没有测试任务。创建任务时关联需求与用例，创建后自动逐用例执行。
          </div>
        </div>
      </div>
    );
  }
  return (
    <div className="test-body">
      <div className="test-section">
        <div className="test-section-head">
          <span className="test-section-title">测试任务（{data.tasks.length}）</span>
          <span className="spacer" />
          <button className="btn" onClick={onNewTask}>
            + 任务
          </button>
        </div>
        <div className="test-list">
          {data.tasks.map((t) => {
            const running = t.id === runningTaskId;
            const counts = countResults(t);
            return (
              <div key={t.id} className="test-item test-task">
                <div className="test-item-main">
                  <div className="test-item-title" title={t.name}>
                    <span className={`badge ${STATUS_CLASS[t.status] ?? ""}`}>
                      {STATUS_LABEL[t.status] ?? t.status}
                    </span>{" "}
                    {t.name}
                  </div>
                  <div className="test-item-sub">
                    {t.case_ids.length} 用例 · ✅{counts.pass} ❌{counts.fail} ⛔
                    {counts.blocked}
                    {t.config_id
                      ? ` · ${data.configs.find((c) => c.id === t.config_id)?.name ?? "配置已删除"}`
                      : ""}
                    · {fmtTime(t.created_at)}
                  </div>
                  {running && runProgress && (
                    <div className="test-progress">▶ {runProgress}</div>
                  )}
                  {t.results.length > 0 && (
                    <div className="test-results">
                      {t.results.map((r) => {
                        const c = data.cases.find((x) => x.id === r.case_id);
                        return (
                          <div key={r.case_id} className="test-result-row">
                            <span
                              className={`badge ${STATUS_CLASS[r.status] ?? ""}`}
                              title={STATUS_LABEL[r.status] ?? r.status}
                            >
                              {STATUS_LABEL[r.status] ?? r.status}
                            </span>
                            <span
                              className="test-result-title"
                              title={r.actual || c?.title || r.case_id}
                              onClick={() => c && onCaseFlow(c.id)}
                            >
                              {c?.title ?? r.case_id}
                            </span>
                            {r.bug_id && (
                              <button
                                className="link-btn"
                                onClick={() => onBugView(r.bug_id)}
                              >
                                Bug
                              </button>
                            )}
                          </div>
                        );
                      })}
                    </div>
                  )}
                </div>
                <div className="test-item-actions">
                  <button
                    className="btn"
                    disabled={runningTaskId !== ""}
                    onClick={() => onRun(t)}
                    title={
                      running
                        ? "执行中"
                        : t.status === "running"
                          ? "上次执行被中断（刷新/关闭页面会中断编排）——点此从未完成的用例继续"
                          : t.status === "done"
                            ? "重跑：只跑「Bug 已修复」和「还没跑过」的用例"
                            : "执行（逐用例跑）"
                    }
                  >
                    {running
                      ? "执行中…"
                      : t.status === "running"
                        ? "继续执行"
                        : t.status === "done"
                          ? "重跑"
                          : "执行"}
                  </button>
                  <button
                    className="icon-btn"
                    disabled={runningTaskId !== ""}
                    title="强制全量重跑（忽略「已通过」与「待修 Bug」筛选，覆盖已有结果）"
                    onClick={() => onRun(t, true)}
                  >
                    ⟳
                  </button>
                  <button className="icon-btn" title="甘特图" onClick={() => onGantt(t)}>
                    ▤
                  </button>
                  <button className="icon-btn" title="编辑" onClick={() => onEditTask(t)}>
                    ✎
                  </button>
                  <button className="icon-btn" title="删除" onClick={() => onDeleteTask(t)}>
                    🗑
                  </button>
                </div>
              </div>
            );
          })}
        </div>
      </div>
    </div>
  );
}

const countResults = (t: TestTask) => {
  const counts = { pass: 0, fail: 0, blocked: 0 };
  for (const r of t.results) {
    if (r.status === "pass") counts.pass++;
    else if (r.status === "fail") counts.fail++;
    else if (r.status === "blocked") counts.blocked++;
  }
  return counts;
};

// ==================== 页签：报告与 Bug ====================

function ReportsTab({
  data,
  onTaskReport,
  onBugView,
  onDeleteBug,
}: {
  data: TestingData;
  onTaskReport: (t: TestTask) => void;
  onBugView: (b: TestBug) => void;
  onDeleteBug: (b: TestBug) => void;
}) {
  return (
    <div className="test-body">
      <div className="test-section">
        <div className="test-section-head">
          <span className="test-section-title">任务报告</span>
        </div>
        {data.tasks.length === 0 ? (
          <div className="file-empty">还没有任务，先在「测试任务」页创建。</div>
        ) : (
          <div className="test-list">
            {data.tasks.map((t) => (
              <div key={t.id} className="test-item">
                <div className="test-item-main">
                  <div className="test-item-title">{t.name}</div>
                  <div className="test-item-sub">
                    {STATUS_LABEL[t.status] ?? t.status} · {t.results.length} 结果
                  </div>
                </div>
                <div className="test-item-actions">
                  <button className="btn" onClick={() => onTaskReport(t)}>
                    生成报告
                  </button>
                </div>
              </div>
            ))}
          </div>
        )}
      </div>

      <div className="test-section">
        <div className="test-section-head">
          <span className="test-section-title">Bug（{data.bugs.length}）</span>
        </div>
        {data.bugs.length === 0 ? (
          <div className="file-empty">
            暂无 Bug。任务执行中用例失败/阻塞时会自动提交，也可在会话里让智能体补充。
          </div>
        ) : (
          <div className="test-list">
            {[...data.bugs]
              .sort((a, b) => b.created_at - a.created_at)
              .map((b) => (
                <div key={b.id} className="test-item">
                  <div className="test-item-main">
                    <div className="test-item-title" title={b.title}>
                      <span className={`badge ${sevClass(b.severity)}`}>{b.severity}</span>{" "}
                      {b.title}
                    </div>
                    <div className="test-item-sub">
                      <span className={`badge ${STATUS_CLASS[b.status] ?? ""}`}>
                        {STATUS_LABEL[b.status] ?? b.status}
                      </span>{" "}
                      · {fmtTime(b.created_at)}
                    </div>
                  </div>
                  <div className="test-item-actions">
                    <button className="btn" onClick={() => onBugView(b)}>
                      查看
                    </button>
                    <button className="icon-btn" title="删除" onClick={() => onDeleteBug(b)}>
                      🗑
                    </button>
                  </div>
                </div>
              ))}
          </div>
        )}
      </div>
    </div>
  );
}

const sevClass = (s: string) =>
  s === "P1" ? "prio-p1" : s === "P2" ? "prio-p2" : "prio-p3";

// ==================== 页签：测试配置（可复用资产） ====================

function ConfigsTab({
  data,
  onNewConfig,
  onEditConfig,
  onDeleteConfig,
}: {
  data: TestingData;
  onNewConfig: () => void;
  onEditConfig: (c: TestConfig) => void;
  onDeleteConfig: (c: TestConfig) => void;
}) {
  const usedBy = (configId: string) =>
    data.tasks.filter((t) => t.config_id === configId).length;
  return (
    <div className="test-body">
      <div className="test-section">
        <div className="test-section-head">
          <span className="test-section-title">测试配置（{data.configs.length}）</span>
          <span className="spacer" />
          <button className="btn" onClick={onNewConfig}>
            + 配置
          </button>
        </div>
        <div className="hint" style={{ marginBottom: 6 }}>
          被测环境与测试账号只配一次，创建任务时引用，执行时自动注入提示词——
          不必每个任务重填账号密码。
        </div>
        {data.configs.length === 0 ? (
          <div className="file-empty">
            还没有配置。建议先建一条（如「测试环境-管理员」），填入被测地址与测试账号。
          </div>
        ) : (
          <div className="test-list">
            {data.configs.map((c) => (
              <div key={c.id} className="test-item">
                <div className="test-item-main">
                  <div className="test-item-title" title={c.name}>
                    {c.name}
                  </div>
                  <div className="test-item-sub">
                    {c.base_url || "（未填地址）"} · {c.accounts.length} 个账号 · 被{" "}
                    {usedBy(c.id)} 个任务引用
                  </div>
                  {c.accounts.length > 0 && (
                    <div className="test-accounts">
                      {c.accounts.map((a, i) => (
                        <div key={i} className="test-account-row">
                          <span className="mono">{a.username}</span>
                          <span className="mono">••••••</span>
                          {a.note && <span className="hint">{a.note}</span>}
                        </div>
                      ))}
                    </div>
                  )}
                </div>
                <div className="test-item-actions">
                  <button className="icon-btn" title="编辑" onClick={() => onEditConfig(c)}>
                    ✎
                  </button>
                  <button className="icon-btn" title="删除" onClick={() => onDeleteConfig(c)}>
                    🗑
                  </button>
                </div>
              </div>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

// ==================== 弹窗：测试配置表单 ====================

function ConfigFormModal({
  initial,
  onClose,
  onSave,
}: {
  initial: TestConfig;
  onClose: () => void;
  onSave: (c: TestConfig) => void;
}) {
  const [c, setC] = useState<TestConfig>(initial);
  const setAccount = (i: number, patch: Partial<TestConfig["accounts"][number]>) => {
    const accounts = c.accounts.map((a, ai) => (ai === i ? { ...a, ...patch } : a));
    setC({ ...c, accounts });
  };
  return (
    <div className="modal-mask" onClick={onClose}>
      <div className="modal test-modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-title">{c.id ? "编辑配置" : "新增测试配置"}</div>
        <label className="lbl">配置名</label>
        <input
          className="field"
          value={c.name}
          autoFocus
          onChange={(e) => setC({ ...c, name: e.target.value })}
          placeholder="如：测试环境-管理员"
        />
        <label className="lbl">被测系统地址</label>
        <input
          className="field"
          value={c.base_url}
          onChange={(e) => setC({ ...c, base_url: e.target.value })}
          placeholder="如：http://localhost:3000"
        />
        <label className="lbl">测试账号（保存时自动忽略未填完整的行）</label>
        {c.accounts.map((a, i) => (
          <div key={i} className="test-account-edit">
            <input
              className="field"
              placeholder="用户名"
              value={a.username}
              onChange={(e) => setAccount(i, { username: e.target.value })}
            />
            <input
              className="field"
              type="password"
              placeholder="密码"
              autoComplete="off"
              value={a.password}
              onChange={(e) => setAccount(i, { password: e.target.value })}
            />
            <input
              className="field"
              placeholder="备注（可选）"
              value={a.note}
              onChange={(e) => setAccount(i, { note: e.target.value })}
            />
            <button
              className="icon-btn"
              title="删除该账号"
              onClick={() =>
                setC({ ...c, accounts: c.accounts.filter((_, ai) => ai !== i) })
              }
            >
              🗑
            </button>
          </div>
        ))}
        <button
          className="btn ghost"
          onClick={() => setC({ ...c, accounts: [...c.accounts, { username: "", password: "", note: "" }] })}
        >
          + 添加账号
        </button>
        <label className="lbl">环境说明 / 约定（执行时随提示词带给智能体）</label>
        <textarea
          className="field test-area"
          rows={3}
          value={c.notes}
          onChange={(e) => setC({ ...c, notes: e.target.value })}
          placeholder="如：测试数据随便造；勿动系统设置页"
        />
        <div className="modal-actions">
          <button className="btn ghost" onClick={onClose}>
            取消
          </button>
          <button
            className="btn"
            disabled={!c.name.trim()}
            onClick={() => onSave({ ...c, name: c.name.trim() })}
          >
            保存
          </button>
        </div>
      </div>
    </div>
  );
}

// ==================== 弹窗：需求表单 ====================

function ReqFormModal({
  workspaceId,
  initial,
  onClose,
  onSave,
}: {
  workspaceId: string;
  initial: TestRequirement;
  onClose: () => void;
  onSave: (r: TestRequirement) => void;
}) {
  const [r, setR] = useState(initial);
  // 附图预览（文件名 → data URL）：新图上传后与已有图统一走 testReadScreenshot 读回。
  const [shots, setShots] = useState<Record<string, string>>({});
  const [uploading, setUploading] = useState(false);
  const [uploadErr, setUploadErr] = useState("");
  useEffect(() => {
    let alive = true;
    void (async () => {
      const next: Record<string, string> = {};
      for (const name of r.images) {
        try {
          const b64 = await api.testReadScreenshot(workspaceId, name);
          next[name] = `data:image/${name.split(".").pop()?.toLowerCase() || "png"};base64,${b64}`;
        } catch {
          // 图被手动清理：预览缺一张不阻断编辑。
        }
      }
      if (alive) setShots(next);
    })();
    return () => {
      alive = false;
    };
  }, [workspaceId, r.images]);

  /** 选图/贴图：立即上传落盘拿文件名，push 进需求 images。 */
  const addImages = async (files: File[]) => {
    if (!files.length) return;
    setUploading(true);
    setUploadErr("");
    try {
      const names: string[] = [];
      for (const f of files) {
        const ext = (f.name.split(".").pop() || "png").toLowerCase();
        const b64 = await fileToBase64(f);
        names.push(await api.testSaveRequirementImage(workspaceId, b64, ext));
      }
      setR((prev) => ({ ...prev, images: [...prev.images, ...names] }));
    } catch (e) {
      setUploadErr(`图片上传失败：${String(e)}`);
    } finally {
      setUploading(false);
    }
  };

  return (
    <div className="modal-mask" onClick={onClose}>
      <div className="modal test-modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-title">{r.id ? "编辑需求" : "新增需求"}</div>
        <label className="lbl">标题</label>
        <input
          className="field"
          value={r.title}
          autoFocus
          onChange={(e) => setR({ ...r, title: e.target.value })}
          placeholder="如：用户登录功能"
        />
        <label className="lbl">需求描述</label>
        <textarea
          className="field test-area"
          rows={8}
          value={r.content}
          onChange={(e) => setR({ ...r, content: e.target.value })}
          placeholder="粘贴需求文档或功能说明，智能体按此生成用例"
        />
        <label className="lbl">需求附图（流程图 / 页面截图，生成用例时随消息发给智能体）</label>
        {uploadErr && <div className="test-clarify-hint" style={{ color: "#c5221f" }}>{uploadErr}</div>}
        <div className="test-req-images">
          {r.images.map((name) => (
            <div key={name} className="test-req-image">
              {shots[name] ? (
                <img src={shots[name]} alt={name} />
              ) : (
                <div className="test-req-image-loading">加载中…</div>
              )}
              <button
                className="test-req-image-del"
                title="移除附图"
                onClick={() =>
                  setR((prev) => ({ ...prev, images: prev.images.filter((x) => x !== name) }))
                }
              >
                ×
              </button>
            </div>
          ))}
          <label className={`test-req-image-add${uploading ? " disabled" : ""}`}>
            {uploading ? "上传中…" : "+ 贴图 / 选图"}
            <input
              type="file"
              accept="image/png,image/jpeg,image/webp,image/gif"
              multiple
              style={{ display: "none" }}
              disabled={uploading}
              onChange={(e) => {
                void addImages(Array.from(e.target.files ?? []));
                e.target.value = "";
              }}
            />
          </label>
        </div>
        <label className="lbl">当前真实情况（生成用例的依据）</label>
        <textarea
          className="field test-area"
          rows={4}
          value={r.data_profile}
          onChange={(e) => setR({ ...r, data_profile: e.target.value })}
          placeholder={
            "被测系统当前的客观情况，例如：\n" +
            "- 数据规模：商品共 10 条，每页 20 条\n" +
            "- 时间范围：创建时间集中在 2025-03 ~ 2025-06\n" +
            "- 真实样例：ID=1001「春季衬衫」，最新一条 2025-06-18\n" +
            "- 其他：权限模型（管理员/普通用户）、可写范围、已知限制\n" +
            "用例里的具体值都会以此为准；留空则退化成「取列表第一条」这类相对写法。"
          }
        />
        {r.clarifications.length > 0 && (
          <>
            <label className="lbl">澄清记录（智能体提问与你的回答，生成时作为已确认事实）</label>
            <textarea
              className="field test-area"
              rows={3}
              value={r.clarifications.join("\n")}
              onChange={(e) =>
                setR({
                  ...r,
                  clarifications: e.target.value.split("\n").filter((s) => s.trim()),
                })
              }
              placeholder="问：…／答：…"
            />
          </>
        )}
        <div className="modal-actions">
          <button className="btn ghost" onClick={onClose}>
            取消
          </button>
          <button
            className="btn"
            disabled={!r.title.trim()}
            onClick={() => onSave({ ...r, title: r.title.trim() })}
          >
            保存
          </button>
        </div>
      </div>
    </div>
  );
}

// ==================== 弹窗：需求澄清 ====================

/** 需求澄清窗：智能体判定信息不足（说不清页面怎么操作）时收集用户回答，
 *  回答存回需求后带着答案重新生成——而不是让智能体臆造操作步骤。 */
function ClarifyModal({
  req,
  questions,
  onClose,
  onSubmit,
}: {
  req: TestRequirement;
  questions: string[];
  onClose: () => void;
  onSubmit: (answers: string[]) => void;
}) {
  const [answers, setAnswers] = useState<string[]>(() => questions.map(() => ""));
  return (
    <div className="modal-mask" onClick={onClose}>
      <div className="modal test-modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-title">需求澄清 · {req.title}</div>
        <div className="test-clarify-hint">
          智能体判定现有信息（需求文字、附图、数据现状）不足以写出可执行的用例——说不清流程每一步在哪个页面、点什么、填什么。请回答以下问题；回答会保存进需求，下次生成不再重复问。
        </div>
        {questions.map((q, i) => (
          <div key={i}>
            <label className="lbl">
              {i + 1}. {q}
            </label>
            <textarea
              className="field test-area"
              rows={2}
              value={answers[i]}
              onChange={(e) =>
                setAnswers((prev) => prev.map((a, j) => (j === i ? e.target.value : a)))
              }
              placeholder="回答（可留空）"
            />
          </div>
        ))}
        <div className="modal-actions">
          <button className="btn ghost" onClick={onClose}>
            取消
          </button>
          <button className="btn" onClick={() => onSubmit(answers)}>
            提交并重新生成
          </button>
        </div>
      </div>
    </div>
  );
}

// ==================== 弹窗：用例表单 ====================

function CaseFormModal({
  initial,
  requirements,
  onClose,
  onSave,
}: {
  initial: TestCase;
  requirements: TestRequirement[];
  onClose: () => void;
  onSave: (c: TestCase) => void;
}) {
  const [c, setC] = useState({
    ...initial,
    stepsText: initial.steps.join("\n"),
    tagsText: initial.tags.join("，"),
  });
  return (
    <div className="modal-mask" onClick={onClose}>
      <div className="modal test-modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-title">{c.id ? "编辑用例" : "新增用例"}</div>
        <label className="lbl">标题</label>
        <input
          className="field"
          value={c.title}
          autoFocus
          onChange={(e) => setC({ ...c, title: e.target.value })}
        />
        <label className="lbl">前置条件</label>
        <input
          className="field"
          value={c.precondition}
          onChange={(e) => setC({ ...c, precondition: e.target.value })}
        />
        <label className="lbl">步骤（每行一步）</label>
        <textarea
          className="field test-area"
          rows={5}
          value={c.stepsText}
          onChange={(e) => setC({ ...c, stepsText: e.target.value })}
        />
        <label className="lbl">预期结果</label>
        <textarea
          className="field test-area"
          rows={2}
          value={c.expected}
          onChange={(e) => setC({ ...c, expected: e.target.value })}
        />
        <div className="lbl-row">
          <div className="lbl-col">
            <label className="lbl">优先级</label>
            <select
              className="field"
              value={c.priority}
              onChange={(e) => setC({ ...c, priority: e.target.value })}
            >
              <option>P1</option>
              <option>P2</option>
              <option>P3</option>
            </select>
          </div>
          <div className="lbl-col">
            <label className="lbl">关联需求</label>
            <select
              className="field"
              value={c.requirement_id}
              onChange={(e) => setC({ ...c, requirement_id: e.target.value })}
            >
              <option value="">（不关联）</option>
              {requirements.map((r) => (
                <option key={r.id} value={r.id}>
                  {r.title}
                </option>
              ))}
            </select>
          </div>
        </div>
        <label className="lbl">标签（逗号分隔）</label>
        <input
          className="field"
          value={c.tagsText}
          onChange={(e) => setC({ ...c, tagsText: e.target.value })}
        />
        <div className="modal-actions">
          <button className="btn ghost" onClick={onClose}>
            取消
          </button>
          <button
            className="btn"
            disabled={!c.title.trim()}
            onClick={() =>
              onSave({
                ...c,
                title: c.title.trim(),
                steps: c.stepsText
                  .split("\n")
                  .map((s) => s.trim())
                  .filter(Boolean),
                tags: c.tagsText
                  .split(/[,，]/)
                  .map((s) => s.trim())
                  .filter(Boolean),
              })
            }
          >
            保存
          </button>
        </div>
      </div>
    </div>
  );
}

// ==================== 弹窗：任务表单 ====================

function TaskFormModal({
  initial,
  data,
  onClose,
  onSave,
  onGenerateCases,
  onClarify,
}: {
  initial: {
    name: string;
    base_url: string;
    config_id: string;
    requirement_ids: string[];
    case_ids: string[];
  };
  data: TestingData;
  onClose: () => void;
  onSave: (
    t: {
      name: string;
      base_url: string;
      config_id: string;
      requirement_ids: string[];
      case_ids: string[];
    },
    autoRun: boolean,
    /** 本次由 AI 生成、待持久化并关联到任务的用例（用户已勾选确认）。 */
    newCases: TestCase[],
  ) => void;
  /** 让会话里的测试智能体按需求生成用例草稿（未落库）；信息不足时返回澄清问题。 */
  onGenerateCases: (req: TestRequirement) => Promise<DraftResult>;
  /** 智能体要求澄清时上抛（澄清窗由工作台顶层渲染，避免弹窗嵌套）。 */
  onClarify: (req: TestRequirement, questions: string[]) => void;
}) {
  const [t, setT] = useState(initial);
  const [autoRun, setAutoRun] = useState(true);
  // AI 生成的用例草稿：先预览，用户勾选确认后随任务一起落库。
  const [drafts, setDrafts] = useState<TestCase[]>([]);
  const [pickedDrafts, setPickedDrafts] = useState<Set<number>>(new Set());
  const [generating, setGenerating] = useState(false);
  const toggle = (arr: string[], id: string) =>
    arr.includes(id) ? arr.filter((x) => x !== id) : [...arr, id];
  /** 勾/取消需求时联动其名下用例：一个需求动辄几十条用例，逐条点不现实。
   *  只在点需求这一刻联动，之后手动改个别用例不会被覆盖回去。 */
  const toggleRequirement = (reqId: string) => {
    const on = !t.requirement_ids.includes(reqId);
    const own = data.cases.filter((c) => c.requirement_id === reqId).map((c) => c.id);
    setT({
      ...t,
      requirement_ids: on
        ? [...t.requirement_ids, reqId]
        : t.requirement_ids.filter((x) => x !== reqId),
      case_ids: on
        ? Array.from(new Set([...t.case_ids, ...own]))
        : t.case_ids.filter((id) => !own.includes(id)),
    });
  };
  // 选中配置后地址取自配置（配置是唯一事实源），手填输入框禁用。
  const pickedConfig = data.configs.find((c) => c.id === t.config_id) ?? null;

  /** 按选中的需求生成用例草稿（已存在的结果会被替换）；
   *  智能体要求澄清时上抛给工作台顶层弹澄清窗。 */
  const generate = async (req: TestRequirement) => {
    setGenerating(true);
    try {
      const result = await onGenerateCases(req);
      if (result.kind === "clarify") {
        onClarify(req, result.questions);
        return;
      }
      if (!result.cases.length) return;
      setDrafts(result.cases);
      setPickedDrafts(new Set(result.cases.map((_, i) => i)));
    } finally {
      setGenerating(false);
    }
  };

  return (
    <div className="modal-mask" onClick={onClose}>
      <div className="modal test-modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-title">新建测试任务</div>
        <label className="lbl">任务名称</label>
        <input
          className="field"
          value={t.name}
          autoFocus
          onChange={(e) => setT({ ...t, name: e.target.value })}
          placeholder="如：登录模块回归测试"
        />
        <label className="lbl">测试配置（环境 + 账号，可复用；在「配置」页维护）</label>
        <select
          className="field"
          value={t.config_id}
          onChange={(e) => {
            const cfg = data.configs.find((c) => c.id === e.target.value);
            setT({
              ...t,
              config_id: e.target.value,
              base_url: cfg?.base_url ?? t.base_url,
            });
          }}
        >
          <option value="">（不使用配置，手填地址）</option>
          {data.configs.map((c) => (
            <option key={c.id} value={c.id}>
              {c.name}（{c.accounts.length} 账号）
            </option>
          ))}
        </select>
        <label className="lbl">被测系统地址{pickedConfig ? "（来自配置）" : ""}</label>
        <input
          className="field"
          value={pickedConfig ? pickedConfig.base_url : t.base_url}
          disabled={!!pickedConfig}
          onChange={(e) => setT({ ...t, base_url: e.target.value })}
          placeholder="如：http://localhost:3000"
        />
        <label className="lbl">关联需求</label>
        <div className="check-group test-checks">
          {data.requirements.length === 0 && (
            <span className="hint">暂无需求（可只关联用例）</span>
          )}
          {data.requirements.map((r) => (
            <div key={r.id} className="test-req-row">
              <label className="member-check">
                <input
                  type="checkbox"
                  checked={t.requirement_ids.includes(r.id)}
                  onChange={() => toggleRequirement(r.id)}
                />
                {r.title}
              </label>
              <button
                className="btn"
                disabled={generating}
                title="按该需求让测试智能体生成用例（生成后可勾选确认，保存任务时一并落库）"
                onClick={() => void generate(r)}
              >
                {generating ? "生成中…" : "AI 生成用例"}
              </button>
            </div>
          ))}
        </div>
        {drafts.length > 0 && (
          <>
            <label className="lbl">
              AI 生成的用例（{pickedDrafts.size}/{drafts.length}，保存任务时落库并关联）
            </label>
            <div className="check-group test-checks">
              {drafts.map((c, i) => (
                <label key={i} className="member-check">
                  <input
                    type="checkbox"
                    checked={pickedDrafts.has(i)}
                    onChange={() => {
                      const next = new Set(pickedDrafts);
                      if (next.has(i)) next.delete(i);
                      else next.add(i);
                      setPickedDrafts(next);
                    }}
                  />
                  <span className={`badge ${prioClass(c.priority)}`}>{c.priority}</span>{" "}
                  {c.title}
                  <span className="hint">（{c.steps.length} 步）</span>
                </label>
              ))}
            </div>
          </>
        )}
        <div className="test-req-row">
          <label className="lbl">
            关联用例（{t.case_ids.length}/{data.cases.length}）
          </label>
          {data.cases.length > 0 && (
            <>
              <button
                className="btn ghost"
                title="选中全部用例"
                onClick={() => setT({ ...t, case_ids: data.cases.map((c) => c.id) })}
              >
                全选
              </button>
              <button
                className="btn ghost"
                title="清空已选用例"
                onClick={() => setT({ ...t, case_ids: [] })}
              >
                清空
              </button>
            </>
          )}
        </div>
        <div className="check-group test-checks">
          {data.cases.length === 0 && drafts.length === 0 && (
            <span className="hint">
              暂无用例：可在上方按需求「AI 生成用例」，或先在「需求与用例」页准备
            </span>
          )}
          {data.cases.map((c) => (
            <label key={c.id} className="member-check">
              <input
                type="checkbox"
                checked={t.case_ids.includes(c.id)}
                onChange={() => {
                  const on = !t.case_ids.includes(c.id);
                  // 勾选用例时顺带勾上它所属的需求（避免「用例选了、需求没选」的矛盾）；
                  // 取消时不反向联动——用户往往只想摘掉这一条。
                  const needReq =
                    on && c.requirement_id && !t.requirement_ids.includes(c.requirement_id);
                  setT({
                    ...t,
                    case_ids: toggle(t.case_ids, c.id),
                    requirement_ids: needReq
                      ? [...t.requirement_ids, c.requirement_id]
                      : t.requirement_ids,
                  });
                }}
              />
              <span className={`badge ${prioClass(c.priority)}`}>{c.priority}</span>{" "}
              {c.title}
            </label>
          ))}
        </div>
        <label className="member-check">
          <input type="checkbox" checked={autoRun} onChange={(e) => setAutoRun(e.target.checked)} />
          保存后立即自动执行
        </label>
        <div className="modal-actions">
          <button className="btn ghost" onClick={onClose}>
            取消
          </button>
          <button
            className="btn"
            disabled={
              !t.name.trim() || (t.case_ids.length === 0 && pickedDrafts.size === 0)
            }
            onClick={() =>
              onSave(
                { ...t, name: t.name.trim() },
                autoRun,
                drafts.filter((_, i) => pickedDrafts.has(i)),
              )
            }
            title={
              t.case_ids.length === 0 && pickedDrafts.size === 0 ? "至少关联一条用例" : ""
            }
          >
            保存
          </button>
        </div>
      </div>
    </div>
  );
}

// ==================== 弹窗：导入预览（含 AI 审查关卡） ====================

function ImportPreviewModal({
  source,
  cases,
  requirements,
  onClose,
  onConfirm,
  onReview,
}: {
  source: string;
  cases: TestCase[];
  /** 需求列表（审查时选一项作为覆盖度依据）。 */
  requirements: TestRequirement[];
  onClose: () => void;
  /** 导入用户勾选的用例。 */
  onConfirm: (picked: TestCase[]) => void;
  /** 发起 AI 审查（覆盖度与全面性）。 */
  onReview: (cases: TestCase[], req: TestRequirement | null) => Promise<CaseReview | null>;
}) {
  const [picked, setPicked] = useState<Set<number>>(new Set(cases.map((_, i) => i)));
  const [reviewing, setReviewing] = useState(false);
  const [review, setReview] = useState<CaseReview | null>(null);
  // 审查依据的需求（可选）：给了需求才能判「覆盖度」，否则只判结构完整性。
  const [reqId, setReqId] = useState("");
  const toggle = (i: number) => {
    const next = new Set(picked);
    if (next.has(i)) next.delete(i);
    else next.add(i);
    setPicked(next);
    // 改动勾选后旧审查结论不再对应当前批次：作废，要求重新审查。
    if (source !== "ai") setReview(null);
  };
  const pickedCases = cases.filter((_, i) => picked.has(i));
  // 上传路径必须先过 AI 审查才能落库（AI 生成路径本身已是 AI 产出，可直接确认）。
  const needReview = source !== "ai";
  const canConfirm = picked.size > 0 && (!needReview || !!review);
  const runReview = async () => {
    setReviewing(true);
    try {
      const req = requirements.find((r) => r.id === reqId) ?? null;
      // 审查当前勾选的用例（没勾就审全部）。
      const target = picked.size ? pickedCases : cases;
      const result = await onReview(target, req);
      if (result) setReview(result);
    } finally {
      setReviewing(false);
    }
  };
  return (
    <div className="modal-mask" onClick={onClose}>
      <div className="modal test-modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-title">
          导入预览（
          {source === "ai"
            ? "AI 生成"
            : source === "excel"
              ? "Excel"
              : source === "json"
                ? "JSON"
                : "XMind"}{" "}
          · {cases.length} 条）
        </div>

        {/* AI 审查关卡：用户上传的用例先审覆盖度与全面性，确认后才导入。 */}
        <div className="test-review-box">
          <div className="test-review-head">
            <span className="test-section-title">AI 审查（覆盖度与全面性）</span>
            <span className="spacer" />
            {requirements.length > 0 && (
              <select
                className="field test-filter"
                value={reqId}
                onChange={(e) => setReqId(e.target.value)}
                title="选择关联需求，审查时以其为覆盖度依据"
              >
                <option value="">（不关联需求）</option>
                {requirements.map((r) => (
                  <option key={r.id} value={r.id}>
                    {r.title}
                  </option>
                ))}
              </select>
            )}
            <button className="btn" disabled={reviewing} onClick={() => void runReview()}>
              {reviewing ? "审查中…" : review ? "重新审查" : "开始审查"}
            </button>
          </div>
          {!review && !reviewing && (
            <div className="hint">
              {needReview
                ? "导入前必须先让测试智能体审查这批用例的覆盖度与全面性；审查结论会展示在下方。"
                : "可让测试智能体审查这批用例的覆盖度与全面性（选填）。"}
            </div>
          )}
          {reviewing && <div className="hint">正在审查，请稍候…</div>}
          {review && (
            <div className="test-review-report">
              <div className="test-review-score">
                覆盖度 <strong>{review.coverage.score}</strong> / 100 · 风险{" "}
                <span
                  className={`badge ${
                    review.risk === "high"
                      ? "st-fail"
                      : review.risk === "low"
                        ? "st-pass"
                        : "st-running"
                  }`}
                >
                  {review.risk === "high" ? "高" : review.risk === "low" ? "低" : "中"}
                </span>{" "}
                · 结论{" "}
                <strong>{review.verdict === "pass" ? "覆盖充分" : "建议补充后再执行"}</strong>
              </div>
              <ReviewList title="已覆盖" items={review.coverage.covered} />
              <ReviewList title="未覆盖 / 遗漏场景" items={review.coverage.missing} />
              <ReviewList title="重复用例" items={review.duplicates} />
              <ReviewList title="冲突用例" items={review.conflicts} />
              {review.suggestions.length > 0 && (
                <div className="test-review-sugg">
                  <div className="test-review-sub">建议补充用例（{review.suggestions.length}）</div>
                  {review.suggestions.map((s, i) => (
                    <div key={i} className="test-review-sugg-item">
                      <span className={`badge ${prioClass(s.priority)}`}>{s.priority}</span>{" "}
                      {s.title}
                      {s.expected && <span className="hint"> · 预期：{s.expected}</span>}
                    </div>
                  ))}
                </div>
              )}
            </div>
          )}
        </div>

        <div className="test-import-list">
          {cases.map((c, i) => (
            <label key={i} className="member-check">
              <input type="checkbox" checked={picked.has(i)} onChange={() => toggle(i)} />
              <span className={`badge ${prioClass(c.priority)}`}>{c.priority}</span>{" "}
              <span className="test-import-title">
                {c.title}
                <span className="hint">（{c.steps.length} 步）</span>
              </span>
            </label>
          ))}
        </div>
        <div className="modal-actions">
          <button className="btn ghost" onClick={onClose}>
            取消
          </button>
          <button
            className="btn"
            disabled={!canConfirm}
            onClick={() => onConfirm(pickedCases)}
            title={
              !canConfirm
                ? needReview && !review
                  ? "请先完成 AI 审查再导入"
                  : "没有勾选任何用例"
                : ""
            }
          >
            导入选中（{picked.size}）
          </button>
        </div>
      </div>
    </div>
  );
}

/** 审查报告里的一组条目（空则不渲染）。 */
function ReviewList({ title, items }: { title: string; items: string[] }) {
  if (!items.length) return null;
  return (
    <div className="test-review-sub">
      {title}（{items.length}）
      <ul className="test-review-items">
        {items.map((t, i) => (
          <li key={i}>
            <TestMarkdown text={t} />
          </li>
        ))}
      </ul>
    </div>
  );
}

// ==================== 弹窗：用例流程图（SVG，竖排步骤） ====================

function FlowchartModal({ testCase, onClose }: { testCase: TestCase; onClose: () => void }) {
  const W = 420;
  const nodeH = 46;
  const gap = 26;
  const nodes: { text: string; kind: "start" | "step" | "end" }[] = [
    { text: "开始（前置条件：" + (testCase.precondition || "无") + "）", kind: "start" },
    ...testCase.steps.map((s) => ({ text: s, kind: "step" as const })),
    { text: "预期：" + (testCase.expected || "—"), kind: "end" },
  ];
  const H = nodes.length * (nodeH + gap) + gap;
  const trunc = (s: string, n: number) => (s.length > n ? s.slice(0, n - 1) + "…" : s);

  return (
    <div className="modal-mask" onClick={onClose}>
      <div className="modal test-modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-title">流程图 · {testCase.title}</div>
        <div className="test-flow-wrap">
          <svg width={W} height={H} className="test-flow">
            {nodes.map((n, i) => {
              const y = gap + i * (nodeH + gap);
              const fill =
                n.kind === "start" ? "#e6f4ea" : n.kind === "end" ? "#e8f0fe" : "#f1f3f4";
              const stroke =
                n.kind === "start" ? "#34a853" : n.kind === "end" ? "#4285f4" : "#9aa0a6";
              return (
                <g key={i}>
                  {i > 0 && (
                    <line
                      x1={W / 2}
                      y1={y - gap}
                      x2={W / 2}
                      y2={y}
                      stroke="#9aa0a6"
                      strokeWidth={1.5}
                      markerEnd="url(#arrow)"
                    />
                  )}
                  <rect
                    x={16}
                    y={y}
                    width={W - 32}
                    height={nodeH}
                    rx={n.kind === "start" || n.kind === "end" ? nodeH / 2 : 8}
                    fill={fill}
                    stroke={stroke}
                  />
                  <text x={W / 2} y={y + nodeH / 2 + 4} textAnchor="middle" fontSize={12}>
                    {trunc(n.text, 34)}
                  </text>
                </g>
              );
            })}
            <defs>
              <marker
                id="arrow"
                markerWidth="8"
                markerHeight="8"
                refX="6"
                refY="3"
                orient="auto"
              >
                <path d="M0,0 L6,3 L0,6 Z" fill="#9aa0a6" />
              </marker>
            </defs>
          </svg>
        </div>
        <div className="modal-actions">
          <button className="btn ghost" onClick={onClose}>
            关闭
          </button>
        </div>
      </div>
    </div>
  );
}

// ==================== 弹窗：任务甘特图（SVG，按执行时间轴） ====================

function GanttModal({
  task,
  cases,
  onClose,
}: {
  task: TestTask;
  cases: TestCase[];
  onClose: () => void;
}) {
  const timed = task.results.filter((r) => r.started_at > 0 && r.finished_at >= r.started_at);
  const t0 = timed.length ? Math.min(...timed.map((r) => r.started_at)) : 0;
  const t1 = timed.length ? Math.max(...timed.map((r) => r.finished_at)) : 0;
  const span = Math.max(t1 - t0, 1);
  const W = 560;
  const rowH = 30;
  const labelW = 150;
  const H = timed.length * rowH + 50;
  const color = (s: string) =>
    s === "pass" ? "#34a853" : s === "fail" ? "#ea4335" : s === "blocked" ? "#f9ab00" : "#9aa0a6";

  return (
    <div className="modal-mask" onClick={onClose}>
      <div className="modal test-modal test-modal-wide" onClick={(e) => e.stopPropagation()}>
        <div className="modal-title">甘特图 · {task.name}</div>
        {timed.length === 0 ? (
          <div className="file-empty">任务还没有带时间的执行记录（执行后生成）。</div>
        ) : (
          <div className="test-flow-wrap">
            <svg width={W} height={H} className="test-flow">
              {/* 时间刻度：5 格 */}
              {[0, 1, 2, 3, 4].map((i) => {
                const x = labelW + (i * (W - labelW - 16)) / 4;
                const ms = t0 + (span * i) / 4;
                return (
                  <g key={i}>
                    <line
                      x1={x}
                      y1={26}
                      x2={x}
                      y2={H - 10}
                      stroke="#e8eaed"
                      strokeWidth={1}
                    />
                    <text x={x} y={16} fontSize={10} textAnchor="middle" fill="#5f6368">
                      {new Date(ms).toLocaleTimeString("zh-CN", { hour12: false })}
                    </text>
                  </g>
                );
              })}
              {timed.map((r, i) => {
                const y = 30 + i * rowH;
                const x1 = labelW + ((r.started_at - t0) / span) * (W - labelW - 16);
                const x2 = labelW + ((r.finished_at - t0) / span) * (W - labelW - 16);
                const title = cases.find((c) => c.id === r.case_id)?.title ?? r.case_id;
                return (
                  <g key={r.case_id}>
                    <text x={4} y={y + 14} fontSize={11} fill="#202124">
                      {title.length > 18 ? title.slice(0, 17) + "…" : title}
                    </text>
                    <rect
                      x={x1}
                      y={y + 4}
                      width={Math.max(x2 - x1, 3)}
                      height={rowH - 12}
                      rx={4}
                      fill={color(r.status)}
                      opacity={0.85}
                    >
                      <title>{`${title}：${STATUS_LABEL[r.status] ?? r.status}（${((r.finished_at - r.started_at) / 1000).toFixed(1)}s）`}</title>
                    </rect>
                  </g>
                );
              })}
            </svg>
          </div>
        )}
        <div className="modal-actions">
          <button className="btn ghost" onClick={onClose}>
            关闭
          </button>
        </div>
      </div>
    </div>
  );
}

// ==================== 弹窗：报告查看 / 导出 ====================

function ReportModal({
  title,
  md,
  onClose,
  onExport,
}: {
  title: string;
  md: string;
  onClose: () => void;
  onExport: () => void;
}) {
  return (
    <div className="modal-mask" onClick={onClose}>
      <div className="modal test-modal test-modal-wide" onClick={(e) => e.stopPropagation()}>
        <div className="modal-title">{title}</div>
        <div className="test-report md">
          <TestMarkdown text={md} />
        </div>
        <div className="modal-actions">
          <button
            className="btn ghost"
            onClick={() => void navigator.clipboard.writeText(md)}
          >
            复制
          </button>
          <button className="btn" onClick={onExport}>
            导出 .md
          </button>
          <button className="btn ghost" onClick={onClose}>
            关闭
          </button>
        </div>
      </div>
    </div>
  );
}

// ==================== 弹窗：Bug 详情（截图 + 状态流转） ====================

function BugModal({
  workspaceId,
  bug,
  cases,
  tasks,
  onClose,
  onChanged,
  onExport,
}: {
  workspaceId: string;
  bug: TestBug;
  cases: TestCase[];
  tasks: TestTask[];
  onClose: () => void;
  onChanged: () => void;
  onExport: (b: TestBug) => void;
}) {
  const [shots, setShots] = useState<Record<string, string>>({});
  useEffect(() => {
    let alive = true;
    for (const name of bug.screenshots) {
      if (shots[name]) continue;
      api
        .testReadScreenshot(workspaceId, name)
        .then((b64) => {
          if (alive) setShots((prev) => ({ ...prev, [name]: b64 }));
        })
        .catch(() => {});
    }
    return () => {
      alive = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [bug.id, bug.screenshots.join(",")]);

  const caseTitle = cases.find((c) => c.id === bug.case_id)?.title ?? "—";
  const taskName = tasks.find((t) => t.id === bug.task_id)?.name ?? "—";

  return (
    <div className="modal-mask" onClick={onClose}>
      <div className="modal test-modal test-modal-wide" onClick={(e) => e.stopPropagation()}>
        <div className="modal-title">
          <span className={`badge ${sevClass(bug.severity)}`}>{bug.severity}</span> {bug.title}
        </div>
        <div className="test-bug-meta">
          来源用例：{caseTitle} · 来源任务：{taskName} · {fmtTime(bug.created_at)}
        </div>
        <label className="lbl">复现步骤</label>
        <div className="test-bug-block md">
          <TestMarkdown text={bug.steps} />
        </div>
        <div className="lbl-row">
          <div className="lbl-col">
            <label className="lbl">预期结果</label>
            <div className="test-bug-block md">
              <TestMarkdown text={bug.expected || "—"} />
            </div>
          </div>
          <div className="lbl-col">
            <label className="lbl">实际结果</label>
            <div className="test-bug-block md">
              <TestMarkdown text={bug.actual || "—"} />
            </div>
          </div>
        </div>
        {bug.screenshots.length > 0 && (
          <>
            <label className="lbl">截图（{bug.screenshots.length}）</label>
            <div className="test-shots">
              {bug.screenshots.map((name) => (
                <div key={name} className="test-shot">
                  {shots[name] ? (
                    <img src={`data:image/png;base64,${shots[name]}`} alt={name} />
                  ) : (
                    <div className="file-empty">加载中…</div>
                  )}
                  <div className="test-shot-name">{name}</div>
                </div>
              ))}
            </div>
          </>
        )}
        <label className="lbl">状态</label>
        <select
          className="field"
          value={bug.status}
          onChange={async (e) => {
            await api.testSaveBug(workspaceId, { ...bug, status: e.target.value });
            onChanged();
          }}
        >
          <option value="open">待处理</option>
          <option value="fixed">已修复</option>
          <option value="closed">已关闭</option>
        </select>
        <div className="modal-actions">
          <button className="btn" onClick={() => onExport(bug)}>
            导出 Bug 报告
          </button>
          <button className="btn ghost" onClick={onClose}>
            关闭
          </button>
        </div>
      </div>
    </div>
  );
}


