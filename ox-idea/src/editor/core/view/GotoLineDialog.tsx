/** ⌘L: "Go to Line:Column" (1-based `line[:col]`). */
import { useState, type FormEvent } from "react";
import { Dialog, Input } from "@/ui";
import { editorController, useEditorStatus, useGotoLine } from "../controller";
import styles from "./EditorArea.module.css";
import { tx } from "@/platform/i18n";

export function parseLineCol(text: string): { line: number; col: number } | null {
  const m = /^\s*(\d+)\s*(?:[:,]\s*(\d+))?\s*$/.exec(text);
  if (!m) return null;
  const line = Number(m[1]);
  const col = m[2] ? Number(m[2]) : 1;
  if (line < 1 || col < 1) return null;
  return { line: line - 1, col: col - 1 };
}

export function GotoLineDialog() {
  const open = useGotoLine((s) => s.open);
  if (!open) return null;
  return <GotoLineInner />;
}

function GotoLineInner() {
  const status = useEditorStatus((s) => s.status);
  const [value, setValue] = useState(() => (status ? `${status.line + 1}:${status.col + 1}` : ""));
  const parsed = parseLineCol(value);
  const close = () => useGotoLine.setState({ open: false });
  const submit = (e?: FormEvent) => {
    e?.preventDefault();
    if (!parsed || !status) return;
    close();
    void editorController().openFile(status.path, { selection: parsed, focus: true });
  };
  return (
    <Dialog
      open
      onClose={close}
      title="Go to Line:Column"
      width={340}
      buttons={[
        { label: "OK", role: "default", disabled: !parsed || !status, onClick: () => submit() },
        { label: "Cancel", role: "cancel" },
      ]}
    >
      <form onSubmit={submit} className={styles.gotoBody}>
        <Input
          data-autofocus=""
          mono
          value={value}
          invalid={!!value && !parsed}
          onChange={(e) => setValue(e.target.value)}
          onFocus={(e) => e.currentTarget.select()}
          aria-label="Line and column"
          placeholder="line[:column]"
        />
        <span className={styles.gotoHint}>
          {status ? tx`Current position ${status.line + 1}:${status.col + 1}` : tx("No active editor")}
        </span>
      </form>
    </Dialog>
  );
}
