/**
 * Insert / Apply actions for fenced code blocks in agent replies (see Markdown.tsx).
 * Insert replaces the active editor's selection in one undoable transaction. Apply
 * infers a target file (fence info path → active editor's file → native picker) and
 * writes only after the user confirms a diff preview, through `writeFileText` — the
 * same path merge results use (an open file gets one undoable editor transaction).
 */
import { useState } from "react";
import { activeEditor } from "@/editor/api";
import { tx } from "@/platform/i18n";
import { Dialog } from "@/ui";
import { showStatusInfo, type EditorInput } from "@/workbench/contributions";
import { notify } from "@/workbench/notificationStore";
import type { DiffData } from "@/features/vcs/diffInput";
import { pathLink } from "./conversation";

/** Inserts `text` at the active editor's caret (replacing its selection); false when there is no editable file. */
export function insertCodeAtCaret(text: string): boolean {
  const editor = activeEditor();
  if (!editor || editor.ctx.virtual || editor.ctx.readOnly) {
    showStatusInfo(tx("Open a file to insert the code into"));
    return false;
  }
  const { from, to } = editor.view.state.selection.main;
  editor.view.dispatch({
    changes: { from, to, insert: text },
    selection: { anchor: from + text.length },
    scrollIntoView: true,
    userEvent: "input.paste",
  });
  editor.view.focus();
  return true;
}

/** The file a fence's info string names (```ts src/a.ts), else the active editor's file. */
function fenceTarget(info: string, cwd: string): string | null {
  for (const token of info.trim().split(/\s+/)) {
    const link = pathLink(token, cwd);
    if (link) return link.path;
  }
  const editor = activeEditor();
  return editor && !editor.ctx.virtual ? editor.ctx.path : null;
}

/** Previews `text` against the target file and writes it only when the user confirms. */
export async function applyCodeBlock(text: string, info: string, cwd: string): Promise<void> {
  try {
    let path = fenceTarget(info, cwd);
    if (!path) {
      const { open } = await import("@tauri-apps/plugin-dialog");
      const picked = await open({
        directory: false,
        multiple: false,
        title: tx("Apply Code to File"),
        defaultPath: cwd,
      });
      path = Array.isArray(picked) ? (picked[0] ?? null) : picked;
      if (!path) return;
    }
    const target = path;
    // Loaded on demand: the diff machinery stays out of the conversation's module graph.
    const [{ diffInput }, { openDialog }, { readWorkingText, writeFileText }, { DiffEditor }] = await Promise.all([
      import("@/features/vcs/diffInput"),
      import("@/features/vcs/dialogs"),
      import("@/features/vcs/ops"),
      import("@/features/vcs/DiffEditor"),
    ]);
    const current = await readWorkingText(target);
    const after = text.endsWith("\n") ? text : `${text}\n`;
    const data: DiffData = {
      path: target,
      left: { type: "text", text: current ?? "", label: current === null ? "(does not exist)" : "Current" },
      right: { type: "text", text: after, label: "Code block" },
      embedded: true,
    };
    const input = diffInput(data);
    openDialog((close) => (
      <ApplyDialog
        input={input}
        path={target}
        DiffEditor={DiffEditor}
        onApply={() => writeFileText(target, after, { save: true })}
        onClose={close}
      />
    ));
  } catch (e) {
    notify({ level: "error", title: "Could not apply code block", body: e instanceof Error ? e.message : String(e) });
  }
}

function ApplyDialog({
  input,
  path,
  DiffEditor,
  onApply,
  onClose,
}: {
  input: EditorInput;
  path: string;
  DiffEditor: typeof import("@/features/vcs/DiffEditor").DiffEditor;
  onApply: () => Promise<void>;
  onClose: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const apply = () => {
    setBusy(true);
    setError(null);
    onApply().then(onClose, (e: unknown) => {
      setBusy(false);
      setError(e instanceof Error ? e.message : String(e));
    });
  };
  return (
    <Dialog
      open
      title={tx`Apply to ${path}`}
      onClose={onClose}
      width="calc(100vw - 64px)"
      resizable="both"
      minWidth={480}
      minHeight={280}
      initialSize={{ w: Math.min(1000, globalThis.innerWidth - 64), h: Math.min(680, globalThis.innerHeight - 64) }}
      buttons={[
        { label: "Apply", role: "default", disabled: busy, onClick: apply },
        { label: "Cancel", role: "cancel" },
      ]}
    >
      <div style={{ display: "flex", flexDirection: "column", flex: 1, minHeight: 0 }}>
        {error && (
          <div style={{ padding: "4px 8px", color: "var(--danger)", fontSize: "var(--fs-small)" }}>{error}</div>
        )}
        <DiffEditor key={input.key} input={input} />
      </div>
    </Dialog>
  );
}
