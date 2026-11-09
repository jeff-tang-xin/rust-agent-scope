/**
 * Dragging over the line numbers (or the empty space of the other gutters)
 * selects whole lines, as in IDEA. A press without a drag is left to the
 * gutter's own click handling (e.g. toggling a breakpoint).
 */
import { EditorSelection, type EditorState } from "@codemirror/state";
import { EditorView, ViewPlugin } from "@codemirror/view";

/** The lines from `start` to `end` (1-based, either order), the head at the far line boundary. */
export function lineRangeSelection(state: EditorState, start: number, end: number): EditorSelection {
  const doc = state.doc;
  const after = (n: number) => (n < doc.lines ? doc.line(n + 1).from : doc.length);
  return end >= start
    ? EditorSelection.single(doc.line(start).from, after(end))
    : EditorSelection.single(after(start), doc.line(end).from);
}

/** Line-number column or empty gutter space; not the fold chevrons, VCS annotations or a gutter's own markers. */
export function inLineSelectArea(target: EventTarget | null): boolean {
  const t = target as HTMLElement | null;
  if (!t?.closest?.(".cm-gutters") || t.closest(".cm-foldGutter, .cm-vcs-blame")) return false;
  if (t.closest(".cm-lineNumbers")) return true;
  if (t.classList.contains("cm-gutter")) return true;
  return t.classList.contains("cm-gutterElement") && t.childElementCount === 0;
}

export const gutterLineDrag = ViewPlugin.fromClass(
  class {
    private stop: (() => void) | null = null;

    constructor(readonly view: EditorView) {
      view.dom.addEventListener("mousedown", this.onMouseDown);
    }

    private lineAt(clientY: number): number {
      const block = this.view.lineBlockAtHeight(clientY - this.view.documentTop);
      return this.view.state.doc.lineAt(block.from).number;
    }

    onMouseDown = (e: MouseEvent) => {
      if (e.button !== 0 || e.altKey || e.metaKey || e.ctrlKey || e.shiftKey || !inLineSelectArea(e.target)) return;
      this.stop?.();
      const start = this.lineAt(e.clientY);
      let dragging = false;
      const move = (m: MouseEvent) => {
        if (!(m.buttons & 1)) return finish();
        const line = this.lineAt(m.clientY);
        if (!dragging && line === start) return;
        dragging = true;
        this.view.dispatch({
          selection: lineRangeSelection(this.view.state, start, line),
          scrollIntoView: true,
          userEvent: "select.pointer",
        });
      };
      const finish = () => {
        window.removeEventListener("mousemove", move, true);
        window.removeEventListener("mouseup", finish, true);
        this.stop = null;
        if (dragging) this.view.focus();
      };
      window.addEventListener("mousemove", move, true);
      window.addEventListener("mouseup", finish, true);
      this.stop = finish;
    };

    destroy() {
      this.stop?.();
      this.view.dom.removeEventListener("mousedown", this.onMouseDown);
    }
  },
);
