// 测试工作区右侧工作台面板：需求 / 用例 / 任务 / 报告与 Bug。
//
// 分工（对齐 src-tauri/src/testing.rs 头注）：数据 CRUD、Excel/XMind 导入
// 解析、报告生成都在后端命令；本组件负责编排——
// - 用例生成：把需求发给会话里的测试智能体（sendMessage），从回复末尾的
//   ```json {"cases":[...]} 解析草稿，预览确认后批量导入；
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
  type TestCase,
  type TestBug,
  type TestConfig,
  type TestRequirement,
  type TestTask,
  type TestingData,
  type WorkspaceConfig,
} from "../api";
import { usePanelWidth } from "./usePanelWidth";

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
 *  任务引用了测试配置时，地址与账号从配置注入——账号密码只配一次，多任务复用。 */
function execPrompt(task: TestTask, tc: TestCase, config?: TestConfig | null): string {
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

  /** 等指定 turn 结束（事件驱动 + 兜底超时；返回是否正常完成）。 */
  const waitTurnFinished = (turnId: string, timeoutMs = 15 * 60_000) =>
    new Promise<boolean>((resolve) => {
      let settled = false;
      const finish = (ok: boolean, un: Promise<() => void>) => {
        if (settled) return;
        settled = true;
        un.then((f) => f()).catch(() => {});
        resolve(ok);
      };
      const un = onTurnFinished((p) => {
        if (p.workspace_id === workspace.id && p.turn_id === turnId) {
          finish(p.outcome === "completed", un);
        }
      });
      setTimeout(() => finish(false, un), timeoutMs);
    });

  /** 取会话最后一条助手回复文本。 */
  const lastAssistantText = async () => {
    const hist = await api.getHistory(workspace.id, runtimeMode, sessionId);
    for (let i = hist.length - 1; i >= 0; i--) {
      if (hist[i].role === "assistant") return hist[i].text;
    }
    return "";
  };

  // ---------- 用例生成 / 导入 ----------

  /** 让会话里的测试智能体按需求生成用例 → 解析 → 预览确认。 */
  const generateCases = async (req: TestRequirement) => {
    if (convBusy || runningRef.current) {
      toast("error", "会话正在运行，请等待当前任务完成再生成用例");
      return;
    }
    try {
      onToast("info", `已把需求「${req.title}」发给测试智能体，等待生成…`);
      const prompt = [
        `请根据以下需求设计测试用例，并按用例生成协议在回复末尾输出一个 \`\`\`json 代码块（{"cases":[...]}）。`,
        "",
        `需求标题：${req.title}`,
        `需求内容：`,
        req.content || "（无补充描述，按标题理解）",
      ].join("\n");
      const turn = await api.sendMessage(workspace.id, runtimeMode, sessionId, prompt);
      const ok = await waitTurnFinished(turn.turn_id);
      const text = ok ? await lastAssistantText() : "";
      const cases = parseGeneratedCases(extractJsonBlock(text)).map((c) => ({
        ...c,
        requirement_id: req.id,
      }));
      if (!cases.length) {
        toast("error", "未能从回复解析出用例（缺少 {\"cases\":[...]} 代码块），可在会话里让智能体重试");
        return;
      }
      setImportPreview({ source: "ai", cases });
    } catch (e) {
      toast("error", `生成用例失败：${String(e)}`);
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

  const confirmImport = async () => {
    if (!importPreview) return;
    try {
      const saved = await api.testImportCases(workspace.id, importPreview.cases);
      onToast("info", `已导入 ${saved.length} 条用例`);
      setImportPreview(null);
      await reload();
    } catch (e) {
      toast("error", `导入失败：${String(e)}`);
    }
  };

  // ---------- 用例跨工作区复用（JSON 导出 / 导入） ----------

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
              setReqForm({ id: "", title: "", content: "", created_at: 0 })
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
            onRun={(t) => void runTask(t)}
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
          onSave={async (t, autoRun) => {
            try {
              const saved = await api.testSaveTask(workspace.id, {
                ...t,
                id: "",
                status: "pending",
                results: [],
                created_at: 0,
                finished_at: 0,
              });              setTaskForm(null);
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
          onClose={() => setImportPreview(null)}
          onConfirm={confirmImport}
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

  async function runTask(task: TestTask) {
    if (runningRef.current) {
      toast("error", "已有任务在执行中");
      return;
    }
    if (convBusy) {
      toast("error", "会话正在运行，请等待或停止当前对话再执行任务");
      return;
    }
    runningRef.current = true;
    setRunningTaskId(task.id);
    try {
      // 以磁盘上的最新任务为基准（面板状态可能落后）。
      const fresh = await api.testListData(workspace.id);
      const t = fresh.tasks.find((x) => x.id === task.id);
      if (!t) {
        toast("error", "任务不存在（可能已被删除）");
        return;
      }
      const casesById = new Map(fresh.cases.map((c) => [c.id, c]));
      // 任务引用的测试配置（环境 + 账号）；配置已删则回退任务自带 base_url。
      const config = fresh.configs.find((c) => c.id === t.config_id) ?? null;
      const results = t.results.map((r) => ({ ...r }));
      let done = 0;
      for (let i = 0; i < t.case_ids.length; i++) {
        const cid = t.case_ids[i];
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
        setRunProgress(`${i + 1}/${t.case_ids.length}：${tc.title}`);
        results[ri] = { ...results[ri], status: "running", started_at: nowMs() };
        await api.testUpdateTask(workspace.id, { ...t, status: "running", results });
        await reload();

        let reply = "";
        try {
          const turn = await api.sendMessage(workspace.id, runtimeMode, sessionId, execPrompt(t, tc, config));
          const ok = await waitTurnFinished(turn.turn_id);
          if (ok) reply = await lastAssistantText();
        } catch {
          reply = ""; // 发送失败按 error 处理，不中断整个任务
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
              onToast("info", `已提交 Bug：${saved.title}`);
            } catch (e) {
              toast("error", `Bug 提交失败：${String(e)}`);
            }
          }
        } else {
          results[ri] = {
            ...results[ri],
            status: "error",
            actual: "未能从回复解析出结果 JSON（协议见测试智能体提示词）",
            raw_reply: reply,
            finished_at: nowMs(),
          };
        }
        done++;
        const isLast = done === t.case_ids.length;
        await api.testUpdateTask(workspace.id, {
          ...t,
          status: isLast ? "done" : "running",
          results,
          finished_at: isLast ? nowMs() : 0,
        });
        await reload();
      }
      onToast("info", `任务「${t.name}」执行完成`);
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
  onRun: (t: TestTask) => void;
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
                    title={running ? "执行中" : "执行（逐用例跑，已有结果将被覆盖）"}
                  >
                    {running ? "执行中…" : t.status === "pending" ? "执行" : "重跑"}
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
  initial,
  onClose,
  onSave,
}: {
  initial: TestRequirement;
  onClose: () => void;
  onSave: (r: TestRequirement) => void;
}) {
  const [r, setR] = useState(initial);
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
  ) => void;
}) {
  const [t, setT] = useState(initial);
  const [autoRun, setAutoRun] = useState(true);
  const toggle = (arr: string[], id: string) =>
    arr.includes(id) ? arr.filter((x) => x !== id) : [...arr, id];
  // 选中配置后地址取自配置（配置是唯一事实源），手填输入框禁用。
  const pickedConfig = data.configs.find((c) => c.id === t.config_id) ?? null;

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
            <label key={r.id} className="member-check">
              <input
                type="checkbox"
                checked={t.requirement_ids.includes(r.id)}
                onChange={() =>
                  setT({ ...t, requirement_ids: toggle(t.requirement_ids, r.id) })
                }
              />
              {r.title}
            </label>
          ))}
        </div>
        <label className="lbl">
          关联用例（{t.case_ids.length}/{data.cases.length}）
        </label>
        <div className="check-group test-checks">
          {data.cases.length === 0 && (
            <span className="hint">暂无用例，请先在「需求与用例」页准备</span>
          )}
          {data.cases.map((c) => (
            <label key={c.id} className="member-check">
              <input
                type="checkbox"
                checked={t.case_ids.includes(c.id)}
                onChange={() => setT({ ...t, case_ids: toggle(t.case_ids, c.id) })}
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
            disabled={!t.name.trim() || t.case_ids.length === 0}
            onClick={() => onSave({ ...t, name: t.name.trim() }, autoRun)}
            title={!t.case_ids.length ? "至少关联一条用例" : ""}
          >
            保存
          </button>
        </div>
      </div>
    </div>
  );
}

// ==================== 弹窗：导入预览 ====================

function ImportPreviewModal({
  source,
  cases,
  onClose,
  onConfirm,
}: {
  source: string;
  cases: TestCase[];
  onClose: () => void;
  onConfirm: () => void;
}) {
  const [picked, setPicked] = useState<Set<number>>(new Set(cases.map((_, i) => i)));
  const toggle = (i: number) => {
    const next = new Set(picked);
    if (next.has(i)) next.delete(i);
    else next.add(i);
    setPicked(next);
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
            disabled={picked.size === 0}
            onClick={() => onConfirm()}
          >
            导入选中（{picked.size}）
          </button>
        </div>
      </div>
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
        <pre className="test-report mono">{md}</pre>
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
        <pre className="test-bug-block">{bug.steps}</pre>
        <div className="lbl-row">
          <div className="lbl-col">
            <label className="lbl">预期结果</label>
            <pre className="test-bug-block">{bug.expected || "—"}</pre>
          </div>
          <div className="lbl-col">
            <label className="lbl">实际结果</label>
            <pre className="test-bug-block">{bug.actual || "—"}</pre>
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


