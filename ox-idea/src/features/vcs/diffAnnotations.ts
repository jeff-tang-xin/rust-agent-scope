/** Notes (pull request review comments) rendered as blocks under diff lines. */
import { StateField, type EditorState, type Extension } from "@codemirror/state";
import { Decoration, EditorView, WidgetType, type DecorationSet } from "@codemirror/view";
import type { DiffAnnotation } from "./diffInput";

type Classes = Readonly<Record<string, string | undefined>>;

class NotesWidget extends WidgetType {
  constructor(
    readonly notes: DiffAnnotation[],
    readonly cls: Classes,
  ) {
    super();
  }
  override eq(other: WidgetType): boolean {
    return (
      other instanceof NotesWidget &&
      other.notes.length === this.notes.length &&
      other.notes.every((n, i) => n.body === this.notes[i]!.body && n.author === this.notes[i]!.author)
    );
  }
  override toDOM(): HTMLElement {
    const box = document.createElement("div");
    box.className = this.cls.notes ?? "";
    for (const n of this.notes) {
      const item = document.createElement("div");
      item.className = this.cls.note ?? "";
      const head = document.createElement("div");
      head.className = this.cls.noteHead ?? "";
      const who = document.createElement("span");
      who.className = this.cls.noteAuthor ?? "";
      who.textContent = n.author;
      head.append(who);
      if (n.date) {
        const when = document.createElement("span");
        when.textContent = new Date(n.date).toLocaleString();
        head.append(when);
      }
      const body = document.createElement("div");
      body.className = this.cls.noteBody ?? "";
      body.textContent = n.body;
      item.append(head, body);
      box.append(item);
    }
    return box;
  }
  override ignoreEvent(): boolean {
    return true;
  }
}

function build(state: EditorState, notes: readonly DiffAnnotation[], cls: Classes): DecorationSet {
  const byLine = new Map<number, DiffAnnotation[]>();
  for (const n of notes) {
    if (n.line < 0 || n.line >= state.doc.lines) continue;
    byLine.set(n.line, [...(byLine.get(n.line) ?? []), n]);
  }
  const ranges = [...byLine.entries()]
    .sort((a, b) => a[0] - b[0])
    .map(([line, list]) =>
      Decoration.widget({ widget: new NotesWidget(list, cls), block: true, side: 1 }).range(
        state.doc.line(line + 1).to,
      ),
    );
  return Decoration.set(ranges);
}

/** Block widgets with `notes` under their lines; `cls` supplies the CSS module classes. */
export function diffAnnotations(notes: readonly DiffAnnotation[], cls: Classes): Extension {
  if (!notes.length) return [];
  return StateField.define<DecorationSet>({
    create: (state) => build(state, notes, cls),
    update: (value, tr) => (tr.docChanged ? value.map(tr.changes) : value),
    provide: (f) => EditorView.decorations.from(f),
  });
}
