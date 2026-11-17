/** Window requests from the built-in MCP server, answered through window-scoped IPC. */
import { create } from "zustand";
import { allActions, executeActionOrThrow, getAction } from "@/platform/actions";
import { call, commands, type McpUiRequest } from "@/platform/ipc";
import { editorService } from "@/workbench/contributions";

type Confirm = Extract<McpUiRequest, { type: "confirm" }> & { id: number; error?: string };
export const useMcpConfirms = create<{ queue: Confirm[] }>(() => ({ queue: [] }));
export type Respond = (id: number, allow: boolean, error?: string) => Promise<void>;
const respond: Respond = async (id, allow, error) => {
  await call(commands.mcpRespond(id, allow, error ?? null));
};
const sending = new Set<number>();
export function dropConfirm(id: number) {
  sending.delete(id);
  useMcpConfirms.setState((s) => ({ queue: s.queue.filter((c) => c.id !== id) }));
}
export function answerConfirm(c: Confirm, allow: boolean, send: Respond = respond) {
  dropConfirm(c.id);
  sending.add(c.id);
  void send(c.id, allow).then(
    () => sending.delete(c.id),
    (error: unknown) => {
      if (!sending.delete(c.id)) return;
      useMcpConfirms.setState((s) => ({ queue: [{ ...c, error: String(error) }, ...s.queue] }));
    },
  );
}
export function denyAll(send: Respond = respond) {
  const queue = useMcpConfirms.getState().queue;
  useMcpConfirms.setState({ queue: [] });
  sending.clear();
  for (const c of queue) void send(c.id, false).catch(() => {});
}
export async function handleMcpRequest(
  id: number,
  request: McpUiRequest,
  send: Respond = respond,
  editor = editorService,
) {
  if (request.type === "confirm") {
    useMcpConfirms.setState((s) =>
      s.queue.some((c) => c.id === id) ? s : { queue: [...s.queue, { ...request, id }] },
    );
    return;
  }
  if (request.type === "executeAction") {
    try {
      if (!getAction(request.actionId)) {
        const near = allActions()
          .filter((a) => !a.hidden && a.id.toLowerCase().includes(request.actionId.toLowerCase()))
          .slice(0, 10)
          .map((a) => a.id);
        throw new Error(
          `Unknown action: ${request.actionId}` +
            (near.length ? `; near matches: ${near.join(", ")}` : "; no near matches"),
        );
      }
      let arg: unknown;
      if (request.arg != null) {
        try {
          arg = JSON.parse(request.arg);
        } catch {
          throw new Error("arg must be valid JSON text");
        }
      }
      // Throws when the action's `when` clause disables it in the current context.
      await executeActionOrThrow(request.actionId, arg);
      await send(id, true);
    } catch (error) {
      await send(id, false, String(error)).catch(() => {});
    }
    return;
  }
  try {
    await editor().openFile(request.path, {
      selection: request.line != null ? { line: request.line - 1, col: (request.column ?? 1) - 1 } : undefined,
      focus: true,
    });
    await send(id, true);
  } catch (error) {
    await send(id, false, String(error)).catch(() => {});
  }
}
