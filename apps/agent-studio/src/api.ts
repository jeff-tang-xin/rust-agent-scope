// Tauri 后端 API 封装：类型 + invoke/listen。
// 事件信封与命令面一一对应 src-tauri/src/commands.rs。

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

// ---------- 资源类型（≈ service 的 Environment / Managed Agent 定义） ----------

export interface ModelProviderConfig {
  name: string;
  base_url: string;
  api_key: string;
  model: string;
}

export interface TeamMemberConfig {
  name: string;
  description: string;
  prompt: string;
}

export interface AgentConfig {
  id: string;
  name: string;
  description: string;
  sys_prompt: string;
  mode: "solo" | "team" | "ops" | "db";
  provider: string;
  max_iters: number;
  members: TeamMemberConfig[];
  /** 自定义工作区绝对路径；空串 = 默认 app data 目录 */
  workspace: string;
  created_at: number;
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

export interface TurnStartedPayload {
  agent_id: string;
  session_id: string;
  turn_id: string;
  reason: "steer";
}

// ---------- 事件（AgentEvent JSON：{type: SCREAMING_SNAKE_CASE, ...}） ----------

export interface AgentEventPayload {
  agent_id: string;
  session_id: string;
  turn_id: string;
  event: { type?: string; [key: string]: unknown };
}

export interface TurnFinishedPayload {
  agent_id: string;
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

  listAgents: () => invoke<AgentConfig[]>("list_agents"),
  createAgent: (agent: AgentConfig) => invoke<AgentConfig>("create_agent", { agent }),
  updateAgent: (agent: AgentConfig) => invoke<void>("update_agent", { agent }),
  deleteAgent: (agentId: string) => invoke<void>("delete_agent", { agentId }),

  sendMessage: (agentId: string, mode: string, sessionId: string, message: string) =>
    invoke<TurnInfo>("send_message", { agentId, mode, sessionId, message }),
  steerTurn: (agentId: string, mode: string, sessionId: string, message: string) =>
    invoke<boolean>("steer_turn", { agentId, mode, sessionId, message }),
  interruptTurn: (agentId: string, mode: string, sessionId: string) =>
    invoke<boolean>("interrupt_turn", { agentId, mode, sessionId }),

  listSessions: (agentId: string) =>
    invoke<SessionSummary[]>("list_sessions", { agentId }),

  setPlanMode: (agentId: string, mode: string, sessionId: string, active: boolean) =>
    invoke<PlanStatus>("set_plan_mode", { agentId, mode, sessionId, active }),
  getPlanStatus: (agentId: string, mode: string, sessionId: string) =>
    invoke<PlanStatus>("get_plan_status", { agentId, mode, sessionId }),

  getHistory: (agentId: string, mode: string, sessionId: string) =>
    invoke<HistoryItem[]>("get_history", { agentId, mode, sessionId }),
  clearSession: (agentId: string, mode: string, sessionId: string) =>
    invoke<void>("clear_session", { agentId, mode, sessionId }),
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
