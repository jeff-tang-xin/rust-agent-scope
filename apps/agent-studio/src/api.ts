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

/** 工作区类型：协同（solo/team 可切，solo 下 plan/free 可切）/ 运维 / 数据库运维。 */
export type WorkspaceKind = "collab" | "ops" | "db";

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
  created_at: number;
}

export const DEFAULT_AGENT_ID = "default-assistant";

// ---------- 数据库服务器（设置页管理；db 工作区执行层按名字引用） ----------

export interface DatabaseServerConfig {
  name: string;
  db_type: string;
  host: string;
  port: number;
  database: string;
  username: string;
  password: string;
  description: string;
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
}

/** 工作区文件条目（相对工作区根，/ 分隔）。 */
export interface WorkspaceFileEntry {
  path: string;
  is_dir: boolean;
  size: number;
}

export interface TurnStartedPayload {
  workspace_id: string;
  session_id: string;
  turn_id: string;
  reason: "steer";
}

// ---------- 事件（AgentEvent JSON：{type: SCREAMING_SNAKE_CASE, ...}） ----------

export interface AgentEventPayload {
  workspace_id: string;
  session_id: string;
  turn_id: string;
  event: { type?: string; [key: string]: unknown };
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
    invoke<void>("save_database", { database }),
  deleteDatabase: (name: string) => invoke<void>("delete_database", { name }),
  listDatabaseKinds: () => invoke<DatabaseKindInfo[]>("list_database_kinds"),

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
  steerTurn: (workspaceId: string, mode: string, sessionId: string, message: string) =>
    invoke<boolean>("steer_turn", { workspaceId, mode, sessionId, message }),
  interruptTurn: (workspaceId: string, mode: string, sessionId: string) =>
    invoke<boolean>("interrupt_turn", { workspaceId, mode, sessionId }),

  listSessions: (workspaceId: string) =>
    invoke<SessionSummary[]>("list_sessions", { workspaceId }),

  listWorkspaceFiles: (workspaceId: string) =>
    invoke<WorkspaceFileEntry[]>("list_workspace_files", { workspaceId }),
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

export type Unlisten = UnlistenFn;
