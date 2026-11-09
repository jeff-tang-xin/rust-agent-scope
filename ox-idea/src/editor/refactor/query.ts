/** Cancellable LSP queries shared by Refactor This, the Refactor menu actions and Safe Delete. */
import type { EditorView } from "@codemirror/view";
import type { CodeActionQuery, IdeError } from "@/bindings";
import { IpcError, call, commands, isTauri, nextRequestId } from "@/platform/ipc";
import { whenSynced } from "@/editor/api";
import { docOf } from "@/editor/lsp/lsp";
import { offsetsToRange } from "@/editor/lsp/positions";
import { fallbackLanguageName } from "./catalog";

type Result<T> = { status: "ok"; data: T } | { status: "error"; error: IdeError };

export const cancelled = () => new IpcError({ kind: "cancelled", message: "cancelled" });

/** Runs a backend request under a UI-allocated id; aborting `signal` cancels it there. */
export async function cancellable<T>(run: (request: number) => Promise<Result<T>>, signal?: AbortSignal): Promise<T> {
  if (!isTauri()) throw new IpcError({ kind: "unavailable", message: "No language server available" });
  if (signal?.aborted) throw cancelled();
  const id = nextRequestId();
  const onAbort = () => void commands.cancelRequest(id);
  signal?.addEventListener("abort", onAbort, { once: true });
  try {
    return await call(run(id));
  } finally {
    signal?.removeEventListener("abort", onAbort);
  }
}

/** `refactor*` code actions for the selection, with the kinds the servers advertise. */
export async function queryRefactorActions(
  view: EditorView,
  signal?: AbortSignal,
  only: string[] = ["refactor"],
): Promise<CodeActionQuery> {
  const ctx = docOf(view);
  if (!ctx) throw new IpcError({ kind: "unavailable", message: "No document" });
  await whenSynced(view);
  const sel = view.state.selection.main;
  const range = offsetsToRange(view.state.doc, sel.from, sel.to);
  const q = await cancellable((r) => commands.lspCodeActionQuery(r, ctx.doc, range, only), signal);
  // Servers may ignore `only`.
  return { ...q, actions: q.actions.filter((a) => a.kind?.startsWith("refactor") ?? false) };
}

let names: Promise<Map<string, string>> | null = null;

/** Display name of a language id ("rust" → "Rust"). */
export async function languageName(id: string): Promise<string> {
  if (isTauri()) {
    names ??= commands
      .languages()
      .then((list) => new Map(list.map((l) => [l.id as string, l.name])))
      .catch(() => new Map<string, string>());
    const name = (await names).get(id);
    if (name) return name;
  }
  return fallbackLanguageName(id);
}
