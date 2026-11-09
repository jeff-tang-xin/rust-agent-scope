import { afterEach, beforeAll, describe, expect, it } from "vitest";
import { EditorSelection, EditorState } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { indentService, type LanguageSupport } from "@codemirror/language";
import { languages } from "@codemirror/language-data";
import { addCursorAbove, insertBlankLine, selectLine, toggleTabFocusMode } from "@codemirror/commands";
import { codeBlockEnd, codeBlockStart, splitLine } from "./caretCommands";
import { editorBaseKeymap } from "./baseKeymap";

const views: EditorView[] = [];
afterEach(() => views.splice(0).forEach((v) => v.destroy()));

/** One unit deeper after a line ending in `{`. */
const braceIndent = indentService.of((cx, pos) => {
  const line = cx.lineAt(pos, -1);
  const base = /^ */.exec(line.text)![0].length;
  return line.text
    .slice(0, pos - line.from)
    .trimEnd()
    .endsWith("{")
    ? base + 2
    : base;
});

/** `|` marks carets. */
function make(text: string) {
  const carets: number[] = [];
  let doc = "";
  for (const ch of text) {
    if (ch === "|") carets.push(doc.length);
    else doc += ch;
  }
  const view = new EditorView({
    state: EditorState.create({
      doc,
      selection: EditorSelection.create(carets.map((c) => EditorSelection.cursor(c))),
      extensions: [EditorState.allowMultipleSelections.of(true), braceIndent],
    }),
  });
  views.push(view);
  return view;
}

function show(view: EditorView) {
  const doc = view.state.doc.toString();
  const heads = view.state.selection.ranges.map((r) => r.head).sort((a, b) => b - a);
  return heads.reduce((s, h) => s.slice(0, h) + "|" + s.slice(h), doc);
}

describe("Split Line", () => {
  it("breaks the line at each caret and leaves the caret before the break", () => {
    const view = make("if (a) {|return 1; }");
    splitLine(view);
    expect(show(view)).toBe("if (a) {|\n  return 1; }");
    const multi = make("a|b\nc|d");
    splitLine(multi);
    expect(show(multi)).toBe("a|\nb\nc|\nd");
  });
});

describe("code block navigation", () => {
  let js: LanguageSupport;
  beforeAll(async () => {
    js = await languages.find((l) => l.name === "JavaScript")!.load();
  });
  const at = (text: string) => {
    const pos = text.indexOf("|");
    return { state: EditorState.create({ doc: text.replace("|", ""), extensions: js }), pos };
  };

  it("goes to just inside the enclosing braces", () => {
    const { state, pos } = at("f() { if (x) { a|(); } b(); }");
    expect(codeBlockStart(state, pos)).toBe("f() { if (x) {".length);
    expect(codeBlockEnd(state, pos)).toBe("f() { if (x) { a(); ".length);
  });

  it("jumps across a brace at the caret to its match, and outward from inside one", () => {
    const text = "f() { if (x) { a(); } b(); }";
    const { state } = at(text);
    const open = text.indexOf("{", 6);
    const close = text.indexOf("}");
    expect(codeBlockEnd(state, open)).toBe(close + 1); // |{ … } → { … }|
    expect(codeBlockStart(state, close + 1)).toBe(open); // { … }| → |{ … }
    expect(codeBlockStart(state, open + 1)).toBe(text.indexOf("{") + 1); // {| → outer block
    expect(codeBlockEnd(state, close)).toBe(text.lastIndexOf("}")); // |} → outer block
  });

  it("skips braces in strings and comments", () => {
    const { state, pos } = at('f() { const s = "{"; /* } */ a|(); }');
    expect(codeBlockStart(state, pos)).toBe("f() {".length);
    expect(codeBlockEnd(state, pos)).toBe('f() { const s = "{"; /* } */ a(); '.length);
  });

  it("has nowhere to go outside any block", () => {
    const { state, pos } = at("a|();");
    expect(codeBlockStart(state, pos)).toBeNull();
    expect(codeBlockEnd(state, pos)).toBeNull();
  });
});

describe("editor base keymap", () => {
  it("drops CodeMirror defaults that IDEA uses for other actions", () => {
    const runs = editorBaseKeymap.map((b) => b.run);
    for (const cmd of [insertBlankLine, toggleTabFocusMode, selectLine, addCursorAbove])
      expect(runs).not.toContain(cmd);
    expect(editorBaseKeymap.some((b) => b.key === "Mod-Enter" || b.key === "Ctrl-m")).toBe(false);
    expect(editorBaseKeymap.some((b) => b.win === "Ctrl-Shift-z")).toBe(true);
  });
});
