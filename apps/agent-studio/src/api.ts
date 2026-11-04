// Tauri 后端 API 封装：类型 + invoke/listen。
// 事件信封与命令面一一对应 src-tauri/src/commands.rs。
//
// 资源模型：智能体（Agent）= 纯定义（人设 + Provider）；工作区（Workspace）
// = 针对一个项目的工作环境（类型 collab/ops/db + 选定智能体 + 工作目录 +
// team 成员）。会话按工作区隔离。

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

// ---------- 资源类型（≈ service 的 Environment / Managed Agent / Workspace） ----------

export interface ModelProviderConfig {
  name: string;
  base_url: string;
  api_key: string;
  model: string;
  /** 上下文窗口（token；0/缺省 = 未知）——压缩触发预算的依据。 */
  context_window?: number;
}

/** 智能体定义：纯人设 + Provider 绑定（无类别/目录/成员——那些归工作区）。 */
export interface AgentConfig {
  id: string;
  name: string;
  description: string;
  sys_prompt: string;
  provider: string;
  max_iters: number;
  created_at: number;
}

/** 工作区类型：协同（solo/team 可切，solo 下 plan/free 可切）/ 运维 / 数据库运维 / 测试。 */
export type WorkspaceKind = "collab" | "ops" | "db" | "test";

/** 工作区：针对一个项目的工作环境。 */
export interface WorkspaceConfig {
  id: string;
  name: string;
  kind: WorkspaceKind;
  /** 选定的智能体（AgentConfig.id）。 */
  agent_id: string;
  /** 工作目录（绝对路径；空串 = 默认 app data）。 */
  dir: string;
  /** 协同工作区 team 运行方式下的成员（智能体 id 列表，lead 之外）。 */
  members: string[];
  /** 绑定的服务器档案名（能力白名单；空 = ops 类别默认开放全部）。 */
  bound_servers: string[];
  /** 绑定的数据库档案名（能力白名单；空 = db 类别默认开放全部）。 */
  bound_databases: string[];
  created_at: number;
}

export const DEFAULT_AGENT_ID = "default-assistant";

// ---------- .ox 数据（共享黑板 + 记忆；用户面板管理） ----------

/** 黑板条目：工作区级、跨会话共享；模型侧只增不改，用户可删改。 */
export interface BlackboardEntry {
  id: number;
  /** 登记时间（Unix 毫秒）。 */
  ts: number;
  /** note / finding / risk / conclusion。 */
  type: string;
  author: string;
  content: string;
}

/** 记忆文件（MEMORY.md + memory/YYYY-MM-DD.md；markdown 台账，文件级管理）。 */
export interface MemoryFileEntry {
  path: string;
  size: number;
  modified_ms: number;
}

// ---------- 数据库服务器（设置页管理；db 工作区执行层按名字引用） ----------

export interface DatabaseServerConfig {
  name: string;
  db_type: string;
  host: string;
  port: number;
  database: string;
  username: string;
  password: string;
  /** 外部 CLI 路径（可选）：填可执行文件或其目录；留空自动探索（PATH → 常见安装目录）。 */
  cli_path: string;
  description: string;
}

/** 保存数据库档案的验证结果（CLI 驱动类型才有内容；不阻断保存）。 */
export interface SaveDatabaseOutcome {
  cli_resolved?: string;
  cli_error?: string;
}

/** 会话与连接的绑定（存会话目录 binding.json；面板跟随会话切换）。 */
export interface SessionBinding {
  database?: string;
  server?: string;
}

/** 驱动形态（后端 adjacently tagged：{"mode":"Cli","program":"psql"} | {"mode":"Bundled"}）。 */
export interface DatabaseDriverInfo {
  mode: "Cli" | "Bundled";
  program?: string;
}

/** 数据库类型注册表条目（后端 DATABASE_KINDS 投影）。 */
export interface DatabaseKindInfo {
  id: string;
  label: string;
  default_port: number;
  needs_host: boolean;
  needs_port: boolean;
  needs_database: boolean;
  driver: DatabaseDriverInfo;
}

/** SSH 服务器档案（设置页管理；ops 工作区 remote_shell/SFTP 按名字引用）。 */
export interface ServerConfig {
  allow_legacy_rsa?: boolean;
  name: string;
  host: string;
  port: number;
  username: string;
  password: string;
  description: string;
}

/** 远程命令执行结果（sh 模式投影）。 */
export interface ExecResult {
  exit_code: number;
  output: string;
}

/** 远程目录条目（SFTP read_dir 投影）。 */
export interface DirEntry {
  name: string;
  is_dir: boolean;
  size: number;
}

export interface PlanStatus {
  active: boolean;
  plan_file?: string;
}

export interface TurnInfo {
  turn_id: string;
  session_id: string;
  status: string;
}

/** 上传附件（base64 内联；图片走 Image 块，其他文件走 Data 块）。 */
export interface AttachmentInput {
  name: string;
  media_type: string;
  data_base64: string;
}

export interface HistoryItem {
  role: string;
  text: string;
  timestamp?: string;
  /** ContentBlock 序列化值（type: text/thinking/tool_use/tool_result/…）。 */
  blocks: Array<Record<string, unknown>>;
}

export interface SessionSummary {
  session_id: string;
  messages: number;
  updated_at: number;
  /** 派生本会话的父会话（spawn 子代理会话才有）。 */
  parent_session_id?: string;
  /** 会话名：自定义名优先，否则首条用户消息摘要；缺省回退 session id。 */
  title?: string;
  /** ops 工作区：会话绑定的服务器档案名（未绑定为空）。 */
  server?: string;
}

/** 已归档会话（归档箱列表项）：数据仍在磁盘，只是从常规列表里隐藏。 */
export interface ArchivedSession {
  session_id: string;
  /** 会话名：自定义名优先，否则首条用户消息摘要。 */
  title?: string;
  /** 消息条数。 */
  messages: number;
  /** 最后修改时间（unix 秒）。 */
  updated_at: number;
  /** 归档时间（unix 秒）。 */
  archived_at: number;
}

/** 工作区文件条目（相对工作区根，/ 分隔）。 */
export interface WorkspaceFileEntry {
  path: string;
  is_dir: boolean;
  size: number;
}

// ---------- 测试工作区（需求 / 用例 / 任务 / Bug；数据存 .ox/testing/） ----------

/** 需求（测试的对象；用例生成与任务关联的锚点）。 */
export interface TestRequirement {
  id: string;
  title: string;
  content: string;
  created_at: number;
}

/** 测试用例（AI 生成 / Excel、XMind 导入 / 手工录入共用一份结构）。 */
export interface TestCase {
  id: string;
  title: string;
  precondition: string;
  steps: string[];
  expected: string;
  priority: string;
  tags: string[];
  /** 关联需求 id（空 = 未关联）。 */
  requirement_id: string;
  /** manual / ai / excel / xmind。 */
  source: string;
  created_at: number;
}

/** 单步执行结果。 */
export interface TestStepResult {
  no: number;
  status: string;
  note: string;
}

/** 单用例执行结果（任务创建时初始化为 pending，编排器逐个回写）。 */
export interface TestCaseResult {
  case_id: string;
  /** pending / running / pass / fail / blocked / error。 */
  status: string;
  actual: string;
  steps: TestStepResult[];
  /** 截图文件名（.ox/screenshots/ 下，经 test_read_screenshot 读取）。 */
  screenshots: string[];
  /** 关联 Bug id（fail/blocked 时创建）。 */
  bug_id: string;
  raw_reply: string;
  started_at: number;
  finished_at: number;
}

/** 测试账号（可复用配置里的一组登录凭据；明文存本地，UI 遮蔽显示）。 */
export interface TestAccount {
  username: string;
  password: string;
  note: string;
}

/** 测试配置：被测环境 + 账号等可复用资产，任务按 config_id 引用。 */
export interface TestConfig {
  id: string;
  name: string;
  base_url: string;
  accounts: TestAccount[];
  notes: string;
  created_at: number;
}

/** 测试任务：关联需求与用例，创建后由前端编排器逐用例自动执行。 */
export interface TestTask {
  id: string;
  name: string;
  /** 被测系统地址（执行提示词里带给 agent）。 */
  base_url: string;
  /** 引用的测试配置 id（空 = 未引用，用 base_url 手填值）。 */
  config_id: string;
  requirement_ids: string[];
  case_ids: string[];
  /** pending / running / done。 */
  status: string;
  results: TestCaseResult[];
  created_at: number;
  finished_at: number;
}

/** Bug 报告（fail/blocked 自动提交 + 手工补充）。 */
export interface TestBug {
  id: string;
  title: string;
  severity: string;
  steps: string;
  expected: string;
  actual: string;
  screenshots: string[];
  case_id: string;
  task_id: string;
  /** open / fixed / closed。 */
  status: string;
  created_at: number;
}

/** 测试工作区全量数据（test_list_data 一次拉齐）。 */
export interface TestingData {
  requirements: TestRequirement[];
  cases: TestCase[];
  tasks: TestTask[];
  bugs: TestBug[];
  configs: TestConfig[];
}

export interface TurnStartedPayload {
  workspace_id: string;
  session_id: string;
  turn_id: string;
  reason: "steer";
}

/** 运行中 turn 的描述（list_running_turns；刷新后恢复 busy/running 用）。 */
export interface RunningTurnInfo {
  workspace_id: string;
  mode: string;
  session_id: string;
}

// ---------- 事件（AgentEvent JSON：{type: SCREAMING_SNAKE_CASE, ...}） ----------

export interface AgentEventPayload {
  workspace_id: string;
  session_id: string;
  turn_id: string;
  event: { type?: string; [key: string]: unknown };
}

/** 权限确认结果（对齐 core ConfirmResult；tool_call 原样回传事件里的 ToolUse 块）。 */
export interface ConfirmResultPayload {
  confirmed: boolean;
  tool_call: Record<string, unknown>;
  /** 「始终允许」时附带的建议规则（allow 行为，写进会话权限上下文）。 */
  rules?: Array<{ tool_name: string; rule_content?: string | null; behavior: string; source: string }>;
  reason?: string;
}

export interface TurnFinishedPayload {
  workspace_id: string;
  session_id: string;
  turn_id: string;
  outcome: "completed" | "interrupted" | "failed";
  error?: string;
}

// ---------- 命令封装 ----------

export const api = {
  listProviders: () => invoke<ModelProviderConfig[]>("list_providers"),
  saveProvider: (provider: ModelProviderConfig) =>
    invoke<void>("save_provider", { provider }),
  deleteProvider: (name: string) => invoke<void>("delete_provider", { name }),

  listDatabases: () => invoke<DatabaseServerConfig[]>("list_databases"),
  saveDatabase: (database: DatabaseServerConfig) =>
    invoke<SaveDatabaseOutcome>("save_database", { database }),
  deleteDatabase: (name: string) => invoke<void>("delete_database", { name }),
  listDatabaseKinds: () => invoke<DatabaseKindInfo[]>("list_database_kinds"),
  // CLI 探测（连接表单「探测」按钮）：Ok=解析到的完整路径，Err=提示。
  probeDatabaseCli: (dbType: string, cliPath: string) =>
    invoke<string>("probe_database_cli", { dbType, cliPath }),

  // 会话与连接的绑定（db 工作区绑数据库、ops 工作区绑服务器；面板跟随会话）。
  getSessionBinding: (workspaceId: string, sessionId: string) =>
    invoke<SessionBinding>("get_session_binding", { workspaceId, sessionId }),
  setSessionBinding: (
    workspaceId: string,
    sessionId: string,
    kind: "database" | "server",
    name: string | null,
  ) =>
    invoke<void>("set_session_binding", { workspaceId, sessionId, kind, name }),

  // SSH 服务器档案 + 连接 + SFTP 文件操作（ops 工作区服务器面板）。
  listServers: () => invoke<ServerConfig[]>("list_servers"),
  saveServer: (server: ServerConfig) => invoke<void>("save_server", { server }),
  deleteServer: (name: string) => invoke<void>("delete_server", { name }),
  sshConnect: (name: string) => invoke<void>("ssh_connect", { name }),
  sshDisconnect: (name: string) => invoke<void>("ssh_disconnect", { name }),
  sshExec: (name: string, command: string) =>
    invoke<{ exit_code: number; output: string }>("ssh_exec", { name, command }),
  sshStatus: (name: string) => invoke<boolean>("ssh_status", { name }),
  sshListDir: (name: string, path: string) =>
    invoke<DirEntry[]>("ssh_list_dir", { name, path }),
  sshUpload: (name: string, localPath: string, remotePath: string) =>
    invoke<number>("ssh_upload", { name, localPath, remotePath }),
  sshDownload: (name: string, remotePath: string, localPath: string) =>
    invoke<number>("ssh_download", { name, remotePath, localPath }),
  sshDeleteFile: (name: string, remotePath: string) =>
    invoke<void>("ssh_delete_file", { name, remotePath }),

  // 查询模式直接执行 SQL（DML 的确认由前端弹窗承担；AI 模式走权限引擎）。
  dbExecute: (database: string, sql: string) =>
    invoke<string>("db_execute", { database, sql }),

  listAgents: () => invoke<AgentConfig[]>("list_agents"),
  createAgent: (agent: AgentConfig) => invoke<AgentConfig>("create_agent", { agent }),
  updateAgent: (agent: AgentConfig) => invoke<void>("update_agent", { agent }),
  deleteAgent: (agentId: string) => invoke<void>("delete_agent", { agentId }),

  listWorkspaces: () => invoke<WorkspaceConfig[]>("list_workspaces"),
  createWorkspace: (workspace: WorkspaceConfig) =>
    invoke<WorkspaceConfig>("create_workspace", { workspace }),
  updateWorkspace: (workspace: WorkspaceConfig) =>
    invoke<void>("update_workspace", { workspace }),
  deleteWorkspace: (workspaceId: string) =>
    invoke<void>("delete_workspace", { workspaceId }),

  sendMessage: (
    workspaceId: string,
    mode: string,
    sessionId: string,
    message: string,
    attachments?: AttachmentInput[],
  ) =>
    invoke<TurnInfo>("send_message", { workspaceId, mode, sessionId, message, attachments }),
  sendConfirm: (
    workspaceId: string,
    mode: string,
    sessionId: string,
    confirmResults: ConfirmResultPayload[],
  ) =>
    invoke<TurnInfo>("send_message", {
      workspaceId,
      mode,
      sessionId,
      message: "",
      confirmResults,
    }),
  // 永久授权（「永久允许」）：跨会话工具 allow 名单，装配时合并进权限上下文。
  savePermanentPermission: (workspaceId: string, toolName: string) =>
    invoke<void>("save_permanent_permission", { workspaceId, toolName }),
  deletePermanentPermission: (workspaceId: string, toolName: string) =>
    invoke<void>("delete_permanent_permission", { workspaceId, toolName }),
  listPermanentPermissions: (workspaceId: string) =>
    invoke<string[]>("list_permanent_permissions", { workspaceId }),

  steerTurn: (
    workspaceId: string,
    mode: string,
    sessionId: string,
    message: string,
    attachments?: AttachmentInput[],
  ) =>
    invoke<boolean>("steer_turn", { workspaceId, mode, sessionId, message, attachments }),
  interruptTurn: (workspaceId: string, mode: string, sessionId: string) =>
    invoke<boolean>("interrupt_turn", { workspaceId, mode, sessionId }),

  listSessions: (workspaceId: string) =>
    invoke<SessionSummary[]>("list_sessions", { workspaceId }),

  // 会话归档：归档 = 从常规列表隐藏（数据留在原地，可还原）。
  archiveSession: (workspaceId: string, mode: string, sessionId: string) =>
    invoke<void>("archive_session", { workspaceId, mode, sessionId }),
  restoreSession: (workspaceId: string, sessionId: string) =>
    invoke<void>("restore_session", { workspaceId, sessionId }),
  listArchivedSessions: (workspaceId: string) =>
    invoke<ArchivedSession[]>("list_archived_sessions", { workspaceId }),

  listWorkspaceFiles: (workspaceId: string, path?: string) =>
    invoke<WorkspaceFileEntry[]>("list_workspace_files", { workspaceId, path: path ?? null }),
  readWorkspaceFile: (workspaceId: string, path: string) =>
    invoke<string>("read_workspace_file", { workspaceId, path }),

  setPlanMode: (workspaceId: string, mode: string, sessionId: string, active: boolean) =>
    invoke<PlanStatus>("set_plan_mode", { workspaceId, mode, sessionId, active }),
  getPlanStatus: (workspaceId: string, mode: string, sessionId: string) =>
    invoke<PlanStatus>("get_plan_status", { workspaceId, mode, sessionId }),

  getHistory: (workspaceId: string, mode: string, sessionId: string) =>
    invoke<HistoryItem[]>("get_history", { workspaceId, mode, sessionId }),
  clearSession: (workspaceId: string, mode: string, sessionId: string) =>
    invoke<void>("clear_session", { workspaceId, mode, sessionId }),
  renameSession: (workspaceId: string, sessionId: string, title: string) =>
    invoke<void>("rename_session", { workspaceId, sessionId, title }),
  deleteSession: (workspaceId: string, mode: string, sessionId: string) =>
    invoke<void>("delete_session", { workspaceId, mode, sessionId }),

  // 运行中 turn 查询（页面刷新后恢复 busy/running：事件监听是全局的，
  // 丢的只是内存状态）。
  listRunningTurns: () => invoke<RunningTurnInfo[]>("list_running_turns"),

  // ---------- 运行日志 ----------

  openLogDir: () => invoke<void>("open_log_dir"),

  // ---------- 会话导出（系统保存对话框拿路径后写盘；仅 .md） ----------

  writeExportFile: (path: string, contents: string) =>
    invoke<void>("write_export_file", { path, contents }),

  // ---------- 测试工作区（数据 CRUD / 导入解析 / 报告生成） ----------

  testListData: (workspaceId: string) =>
    invoke<TestingData>("test_list_data", { workspaceId }),
  testSaveRequirement: (workspaceId: string, requirement: TestRequirement) =>
    invoke<TestRequirement>("test_save_requirement", { workspaceId, requirement }),
  testDeleteRequirement: (workspaceId: string, id: string) =>
    invoke<boolean>("test_delete_requirement", { workspaceId, id }),
  testSaveCase: (workspaceId: string, testCase: TestCase) =>
    invoke<TestCase>("test_save_case", { workspaceId, case: testCase }),
  testDeleteCase: (workspaceId: string, id: string) =>
    invoke<boolean>("test_delete_case", { workspaceId, id }),
  testImportCases: (workspaceId: string, cases: TestCase[]) =>
    invoke<TestCase[]>("test_import_cases", { workspaceId, cases }),
  testSaveTask: (workspaceId: string, task: TestTask) =>
    invoke<TestTask>("test_save_task", { workspaceId, task }),
  testUpdateTask: (workspaceId: string, task: TestTask) =>
    invoke<void>("test_update_task", { workspaceId, task }),
  testDeleteTask: (workspaceId: string, id: string) =>
    invoke<boolean>("test_delete_task", { workspaceId, id }),
  testSaveBug: (workspaceId: string, bug: TestBug) =>
    invoke<TestBug>("test_save_bug", { workspaceId, bug }),
  testDeleteBug: (workspaceId: string, id: string) =>
    invoke<boolean>("test_delete_bug", { workspaceId, id }),
  testSaveConfig: (workspaceId: string, config: TestConfig) =>
    invoke<TestConfig>("test_save_config", { workspaceId, config }),
  testDeleteConfig: (workspaceId: string, id: string) =>
    invoke<boolean>("test_delete_config", { workspaceId, id }),
  testReadScreenshot: (workspaceId: string, fileName: string) =>
    invoke<string>("test_read_screenshot", { workspaceId, fileName }),
  testParseExcel: (workspaceId: string, fileBase64: string) =>
    invoke<TestCase[]>("test_parse_excel", { workspaceId, fileBase64 }),
  testParseXmind: (workspaceId: string, fileBase64: string) =>
    invoke<TestCase[]>("test_parse_xmind", { workspaceId, fileBase64 }),
  testBuildReport: (workspaceId: string, taskId: string) =>
    invoke<string>("test_build_report", { workspaceId, taskId }),
  testBuildBugReport: (workspaceId: string, bugId: string) =>
    invoke<string>("test_build_bug_report", { workspaceId, bugId }),

  // ---------- .ox 数据管理（黑板报 + 记忆，用户面板） ----------

  blackboardList: (workspaceId: string, limit?: number) =>
    invoke<BlackboardEntry[]>("blackboard_list", { workspaceId, limit: limit ?? null }),
  blackboardDelete: (workspaceId: string, id: number) =>
    invoke<boolean>("blackboard_delete", { workspaceId, id }),
  blackboardUpdate: (workspaceId: string, id: number, kind?: string, content?: string) =>
    invoke<BlackboardEntry>("blackboard_update", {
      workspaceId,
      id,
      kind: kind ?? null,
      content: content ?? null,
    }),
  memoryListFiles: (workspaceId: string) =>
    invoke<MemoryFileEntry[]>("memory_list_files", { workspaceId }),
  memoryReadFile: (workspaceId: string, path: string) =>
    invoke<string>("memory_read_file", { workspaceId, path }),
  memorySaveFile: (workspaceId: string, path: string, content: string) =>
    invoke<void>("memory_save_file", { workspaceId, path, content }),
  memoryDeleteFile: (workspaceId: string, path: string) =>
    invoke<void>("memory_delete_file", { workspaceId, path }),
};

// ---------- 事件订阅 ----------

export function onAgentEvent(handler: (payload: AgentEventPayload) => void) {
  return listen<AgentEventPayload>("agent-event", (e) => handler(e.payload));
}

export function onTurnFinished(handler: (payload: TurnFinishedPayload) => void) {
  return listen<TurnFinishedPayload>("turn-finished", (e) => handler(e.payload));
}

export function onTurnStarted(handler: (payload: TurnStartedPayload) => void) {
  return listen<TurnStartedPayload>("turn-started", (e) => handler(e.payload));
}

/** 会话归档广播（archive_session 成功后发）：左栏据此刷新常规列表。 */
export function onSessionArchived(
  handler: (payload: { workspace_id: string; session_id: string }) => void,
) {
  return listen<{ workspace_id: string; session_id: string }>("session-archived", (e) =>
    handler(e.payload),
  );
}

export type Unlisten = UnlistenFn;
