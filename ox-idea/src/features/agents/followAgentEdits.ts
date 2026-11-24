/**
 * Follow Agent edits (Autoscroll from Source): when an agent edit tool changes
 * a file (apply_patch / create_new_file), open it in a preview tab and reveal
 * the first changed line. Batches bursts into one reveal (the last event wins)
 * and never steals focus — the user keeps typing while the agent works.
 */
import { onIdeEvent } from "@/platform/ipc";
import { editorService } from "@/workbench/contributions";
import { useAgents } from "./store";

const REVEAL_DELAY_MS = 150;

let pending: { path: string; line: number | null } | null = null;
let timer: ReturnType<typeof setTimeout> | null = null;

function reveal() {
  timer = null;
  const edit = pending;
  pending = null;
  if (!edit || !useAgents.getState().followAgentEdits) return;
  const options = { preview: true, focus: false };
  const opening =
    edit.line == null
      ? editorService().openFile(edit.path, options)
      : editorService().openFile(edit.path, { ...options, selection: { line: edit.line, col: 0 } });
  opening.catch((e: unknown) => console.warn(`could not follow agent edit of ${edit.path}`, e));
}

/** Subscribes this window to agent edit events; returns the unsubscriber. */
export function followAgentEdits(): () => void {
  return onIdeEvent("agentEdit", (e) => {
    pending = { path: e.path, line: e.line };
    if (timer) clearTimeout(timer);
    timer = setTimeout(reveal, REVEAL_DELAY_MS);
  });
}
