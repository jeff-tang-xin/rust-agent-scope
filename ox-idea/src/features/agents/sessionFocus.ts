/** The agent session tab holding focus, for actions that act on it (Find in Conversation). */
import { setContext } from "@/platform/context";

export interface FocusedSession {
  openFind(): void;
}

let focused: FocusedSession | null = null;

export function setFocusedSession(session: FocusedSession | null) {
  focused = session;
  setContext("agentSessionFocus", !!session);
}

export const focusedSession = () => focused;
