/** Editor side of Emmet: Tab expansion and Update Tag with Emmet. */
import { snippet } from "@codemirror/autocomplete";
import type { EditorView } from "@codemirror/view";
import { enclosingTag, expandAt, syntaxAt, updateTagEdits } from "./model";

/** Tab: expands the Emmet abbreviation before the caret in a `language` document. */
export function expandEmmetAtCaret(view: EditorView, language: string): boolean {
  const { state } = view;
  const sel = state.selection.main;
  if (!sel.empty || state.selection.ranges.length > 1) return false;
  const syntax = syntaxAt(language, state.sliceDoc(0, sel.head));
  if (!syntax) return false;
  const line = state.doc.lineAt(sel.head);
  const found = expandAt(line.text, sel.head - line.from, syntax);
  if (!found) return false;
  snippet(found.snippet)(view, null, line.from + found.from, line.from + found.to);
  return true;
}

/** The enclosing tag's name at the caret (for the prompt). */
export function tagAtCaret(view: EditorView): string | null {
  return enclosingTag(view.state.doc.toString(), view.state.selection.main.head)?.name ?? null;
}

/** Applies `abbreviation` to the tag enclosing the caret; false when nothing changed. */
export function updateTag(view: EditorView, abbreviation: string): boolean {
  const tag = enclosingTag(view.state.doc.toString(), view.state.selection.main.head);
  const edits = tag && updateTagEdits(tag, abbreviation);
  if (!edits) return false;
  view.dispatch({ changes: edits, userEvent: "input.emmet", scrollIntoView: true });
  return true;
}
