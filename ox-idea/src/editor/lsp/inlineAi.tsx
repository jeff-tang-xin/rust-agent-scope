/**
 * Cmd+K inline AI: a floating prompt anchored at the caret/selection. Simple
 * tasks without leaving the editor — generate at the caret, edit the selection,
 * ask about the code, or fix reported problems — using the built-in OX agent
 * model. The sidebar Agent handles complex multi-file work. Code results show a
 * diff preview (Apply/Cancel); answers show as text. Mirrors VCS HunkPopup.
 */
import { useRef, useState } from "react";
import { createRoot, type Root } from "react-dom/client";
import { EditorSelection } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { registerActions } from "@/platform/actions";
import { activeEditor } from "@/editor/api";
import { call, commands, isCancelled, isTauri, nextRequestId, type RequestId } from "@/platform/ipc";
import { tx } from "@/platform/i18n";
import { Button, IconButton, Popup, Spinner } from "@/ui";
import type { InlineAiResult } from "@/bindings";
import { docOf } from "./lsp";
import { offsetToPos } from "./positions";
import { diagnosticsFor } from "./diagnostics";
import styles from "./inlineAi.module.css";

type Mode = "edit" | "generate" | "ask" | "fix";

interface AiState {
  view: EditorView;
  doc: number;
  path: string;
  from: number;
  to: number;
  x: number;
  y: number;
  id: number;
  hasSelection: boolean;
  problems: number;
}

let root: Root | null = null;
let host: HTMLElement | null = null;
let openFor: EditorView | null = null;
let seq = 0;
let inflight: RequestId | null = null;

function cancelInflight() {
  if (inflight !== null && isTauri()) void commands.cancelRequest(inflight);
  inflight = null;
}

function render(state: AiState | null) {
  if (!state && !root) return;
  if (!root) {
    host = document.createElement("div");
    host.dataset.inlineAi = "";
    document.body.appendChild(host);
    root = createRoot(host);
  }
  root.render(state ? <InlineAiView key={state.id} state={state} /> : null);
}

export function closeInlineAi() {
  cancelInflight();
  openFor = null;
  render(null);
}

export function openInlineAi(view: EditorView) {
  const doc = docOf(view);
  if (!doc || doc.readOnly || !isTauri()) return;
  const sel = view.state.selection.main;
  const from = Math.min(sel.from, sel.to);
  const to = Math.max(sel.from, sel.to);
  const coords = view.coordsAtPos(to > from ? to : from);
  openFor = view;
  render({
    view,
    doc: doc.doc,
    path: doc.path,
    from,
    to,
    x: coords?.left ?? 0,
    y: coords ? coords.bottom : 0,
    id: ++seq,
    hasSelection: to > from,
    problems: diagnosticsFor(doc.path).length,
  });
}

/** Close the popup when its editor's document changes (like HunkPopup). */
export function inlineAiExtension() {
  return EditorView.updateListener.of((u) => {
    if (u.docChanged && openFor === u.view) closeInlineAi();
  });
}

export function installInlineAi(): () => void {
  return () => {
    cancelInflight();
    root?.unmount();
    host?.remove();
    root = null;
    host = null;
    openFor = null;
  };
}

function splitLines(text: string): string[] {
  if (!text) return [];
  const lines = text.split("\n");
  if (lines[lines.length - 1] === "") lines.pop();
  return lines;
}

const MODES: { mode: Mode; label: string }[] = [
  { mode: "edit", label: "Edit" },
  { mode: "generate", label: "Generate" },
  { mode: "ask", label: "Ask" },
  { mode: "fix", label: "Fix" },
];

function InlineAiView({ state }: { state: AiState }) {
  const { view, from, to, hasSelection } = state;
  const [instruction, setInstruction] = useState("");
  const [mode, setMode] = useState<Mode>(hasSelection ? "edit" : "generate");
  const [phase, setPhase] = useState<"input" | "pending" | "result">("input");
  const [result, setResult] = useState<InlineAiResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const reqRef = useRef<RequestId | null>(null);

  const submit = async () => {
    const text = instruction.trim();
    if (!text || phase === "pending") return;
    setPhase("pending");
    setError(null);
    const id = nextRequestId();
    reqRef.current = id;
    inflight = id;
    const range = { start: offsetToPos(view.state.doc, from), end: offsetToPos(view.state.doc, to) };
    const diagnostics = mode === "fix" || mode === "ask" ? diagnosticsFor(state.path) : [];
    try {
      const r = await call(commands.aiInline(id, state.doc, range, text, mode, diagnostics));
      if (reqRef.current !== id) return;
      setResult(r);
      setPhase("result");
    } catch (e) {
      if (reqRef.current !== id) return;
      if (isCancelled(e)) {
        closeInlineAi();
        return;
      }
      setError(e instanceof Error ? e.message : String(e));
      setPhase("input");
    } finally {
      if (reqRef.current === id) reqRef.current = null;
      if (inflight === id) inflight = null;
    }
  };

  const apply = () => {
    const code = result?.code;
    if (code == null) return;
    view.dispatch({
      changes: { from, to, insert: code },
      selection: EditorSelection.cursor(from + code.length),
      userEvent: "input.complete",
      scrollIntoView: true,
    });
    closeInlineAi();
    view.focus();
  };

  const oldLines = splitLines(view.state.sliceDoc(from, to));
  const newLines = splitLines(result?.code ?? "");

  return (
    <Popup
      open
      anchor={{ x: state.x, y: state.y }}
      placement="bottom-start"
      offset={4}
      onClose={closeInlineAi}
      trapFocus
      autoFocus
      aria-label={tx("Inline AI")}
      className={styles.popup}
    >
      <div className={styles.header}>
        <span className={styles.title}>{tx("Inline AI")}</span>
        <IconButton icon="close" title="Close" size="small" onClick={closeInlineAi} />
      </div>

      {phase !== "result" && (
        <textarea
          className={styles.input}
          data-autofocus
          rows={2}
          value={instruction}
          disabled={phase === "pending"}
          placeholder={tx("Tell AI what to do…")}
          onChange={(e) => setInstruction(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              void submit();
            }
          }}
        />
      )}

      {phase === "input" && (
        <>
          <div className={styles.modes} role="radiogroup" aria-label={tx("Mode")}>
            {MODES.map(({ mode: m, label }) => (
              <button
                key={m}
                type="button"
                role="radio"
                aria-checked={mode === m}
                className={mode === m ? styles.modeOn : styles.mode}
                disabled={(m === "edit" && !hasSelection) || (m === "fix" && state.problems === 0)}
                onClick={() => setMode(m)}
              >
                {tx(label)}
              </button>
            ))}
          </div>
          {error && (
            <div className={styles.error} role="alert">
              {error}
            </div>
          )}
          <div className={styles.footer}>
            <Button variant="primary" size="small" disabled={!instruction.trim()} onClick={() => void submit()}>
              {mode === "ask" ? "Ask" : "Generate"}
            </Button>
          </div>
        </>
      )}

      {phase === "pending" && (
        <div className={styles.pending}>
          <Spinner size={16} label={tx("Thinking…")} />
          <span>{tx("Thinking…")}</span>
          <Button size="small" onClick={closeInlineAi}>
            Cancel
          </Button>
        </div>
      )}

      {phase === "result" && result?.kind === "code" && (
        <>
          <div className={styles.body}>
            {oldLines.map((l, i) => (
              <div key={`o${i}`} className={styles.removed}>
                <span className={styles.sign} aria-hidden>
                  -
                </span>
                {l || " "}
              </div>
            ))}
            {newLines.map((l, i) => (
              <div key={`n${i}`} className={styles.added}>
                <span className={styles.sign} aria-hidden>
                  +
                </span>
                {l || " "}
              </div>
            ))}
          </div>
          <div className={styles.footer}>
            <Button variant="primary" size="small" onClick={apply}>
              Apply
            </Button>
            <Button size="small" onClick={closeInlineAi}>
              Cancel
            </Button>
          </div>
        </>
      )}

      {phase === "result" && result?.kind === "answer" && (
        <>
          <div className={styles.answer}>{result.text}</div>
          <div className={styles.footer}>
            <Button size="small" onClick={closeInlineAi}>
              Close
            </Button>
          </div>
        </>
      )}
    </Popup>
  );
}

export function registerInlineAiActions(): () => void {
  return registerActions([
    {
      id: "editor.inlineAi.trigger",
      title: "Inline AI",
      category: "Code",
      when: "editorTextFocus",
      keywords: ["ai", "inline chat", "cmd k", "edit with ai"],
      run: () => {
        const view = activeEditor()?.view;
        if (view) openInlineAi(view);
      },
    },
  ]);
}
