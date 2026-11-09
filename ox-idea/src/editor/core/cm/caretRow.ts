import type { Extension, Range } from "@codemirror/state";
import { Decoration, ViewPlugin, type DecorationSet, type EditorView, type ViewUpdate } from "@codemirror/view";

const caretRow = Decoration.line({ class: "cm-activeLine" });

function rows(view: EditorView): DecorationSet {
  const out: Range<Decoration>[] = [];
  let last = -1;
  for (const r of view.state.selection.ranges) {
    // IntelliJ drops the caret-row highlight under a selection; the opaque row would hide it.
    if (!r.empty) continue;
    const from = view.lineBlockAt(r.head).from;
    if (from > last) out.push(caretRow.range(from));
    last = from;
  }
  return Decoration.set(out, true);
}

/** `highlightActiveLine` that only marks lines holding an empty cursor. */
export const caretRowHighlight: Extension = ViewPlugin.fromClass(
  class {
    decorations: DecorationSet;
    constructor(view: EditorView) {
      this.decorations = rows(view);
    }
    update(u: ViewUpdate) {
      if (u.docChanged || u.selectionSet) this.decorations = rows(u.view);
    }
  },
  { decorations: (v) => v.decorations },
);
