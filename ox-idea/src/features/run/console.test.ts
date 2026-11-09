import { describe, expect, it } from "vitest";
import { ConsoleModel, MAX_LINE_LENGTH, paletteColor } from "./console";
import { segmentLine } from "./segments";
import { findOutputLinks } from "./links";

const texts = (m: ConsoleModel) => m.lines.map((l) => l.text);

describe("ConsoleModel", () => {
  it("splits chunks into lines and joins partial lines across chunks", () => {
    const m = new ConsoleModel();
    m.append({ kind: "stdout", text: "hello wo" });
    m.append({ kind: "stdout", text: "rld\nsecond\n" });
    m.append({ kind: "stdout", text: "third" });
    expect(texts(m)).toEqual(["hello world", "second", "third"]);
  });

  it("keeps streams and system messages on their own lines", () => {
    const m = new ConsoleModel();
    m.append({ kind: "system", text: "cargo run\n" });
    m.append({ kind: "stdout", text: "out" });
    m.append({ kind: "stderr", text: "err\n" });
    m.append({ kind: "system", text: "\nProcess finished with exit code 0\n" });
    expect(m.lines.map((l) => [l.kind, l.text])).toEqual([
      ["system", "cargo run"],
      ["stdout", "out"],
      ["stderr", "err"],
      ["system", ""],
      ["system", "Process finished with exit code 0"],
    ]);
  });

  it("handles CRLF and carriage-return overwrites (progress bars)", () => {
    const m = new ConsoleModel();
    m.append({ kind: "stdout", text: "a\r\nb\r\n" });
    m.append({ kind: "stdout", text: "progress 10%\rprogress 50%" });
    m.append({ kind: "stdout", text: "\rprogress 100%\n" });
    expect(texts(m)).toEqual(["a", "b", "progress 100%"]);
  });

  it("treats a CR split from its LF across chunks as a line ending", () => {
    const m = new ConsoleModel();
    m.append({ kind: "stdout", text: "line\r" });
    m.append({ kind: "stdout", text: "\nnext\n" });
    expect(texts(m)).toEqual(["line", "next"]);
  });

  it("parses SGR colours and keeps state across chunks", () => {
    const m = new ConsoleModel();
    m.append({ kind: "stdout", text: "\x1b[31mred " });
    m.append({ kind: "stdout", text: "still\x1b[0m plain\n" });
    m.append({ kind: "stdout", text: "\x1b[1;92mbold green\x1b[22;39m\n" });
    const [first, second] = m.lines;
    expect(first!.text).toBe("red still plain");
    expect(first!.spans).toEqual([{ text: "red still", fg: "var(--ansi-red)" }, { text: " plain" }]);
    expect(second!.spans).toEqual([{ text: "bold green", fg: "var(--ansi-bright-green)", bold: true }]);
  });

  it("supports 256-colour and truecolor", () => {
    const m = new ConsoleModel();
    m.append({ kind: "stdout", text: "\x1b[38;5;196mA\x1b[38;2;10;20;30mB\x1b[48;5;244mC\x1b[0m\n" });
    const spans = m.lines[0]!.spans;
    expect(spans[0]).toMatchObject({ text: "A", fg: "rgb(255, 0, 0)" });
    expect(spans[1]).toMatchObject({ text: "B", fg: "rgb(10, 20, 30)" });
    expect(spans[2]).toMatchObject({ text: "C", bg: "rgb(128, 128, 128)" });
  });

  it("reassembles escape sequences split across chunks", () => {
    const m = new ConsoleModel();
    m.append({ kind: "stdout", text: "x\x1b[3" });
    m.append({ kind: "stdout", text: "2mgreen\x1b[0m\n" });
    expect(m.lines[0]!.spans).toEqual([{ text: "x" }, { text: "green", fg: "var(--ansi-green)" }]);
  });

  it("strips OSC hyperlinks, cursor movement and other control characters", () => {
    const m = new ConsoleModel();
    m.append({ kind: "stdout", text: "\x1b]8;;https://x.dev\x07link\x1b]8;;\x07 \x1b[?25lok\x1b[?25h\x07\n" });
    m.append({ kind: "stdout", text: "50%\x1b[2K\x1b[1Gdone\n" });
    expect(texts(m)).toEqual(["link ok", "done"]);
  });

  it("reports completed lines to onLine, plain text only", () => {
    const seen: Array<[string, string]> = [];
    const m = new ConsoleModel({ onLine: (t, k) => seen.push([t, k]) });
    m.append({ kind: "stdout", text: "\x1b[32mtest a ... ok\x1b[0m\npart" });
    expect(seen).toEqual([["test a ... ok", "stdout"]]);
    m.flush();
    expect(seen[1]).toEqual(["part", "stdout"]);
  });

  it("caps the buffer and tracks dropped lines", () => {
    const m = new ConsoleModel({ maxLines: 100 });
    for (let i = 0; i < 250; i++) m.append({ kind: "stdout", text: `line ${i}\n` });
    expect(m.size).toBeLessThanOrEqual(100);
    expect(m.dropped + m.size).toBe(250);
    expect(m.lines[m.size - 1]!.text).toBe("line 249");
    // Ids stay monotonic after trimming.
    expect(m.lines[0]!.id).toBe(m.dropped + 1);
  });

  it("breaks up very long lines", () => {
    const m = new ConsoleModel();
    m.append({ kind: "stdout", text: "x".repeat(MAX_LINE_LENGTH * 2 + 5) + "\n" });
    expect(m.lines.map((l) => l.text.length)).toEqual([MAX_LINE_LENGTH, MAX_LINE_LENGTH, 5]);
  });

  it("clear() empties the buffer", () => {
    const m = new ConsoleModel();
    m.append({ kind: "stdout", text: "a\nb" });
    m.clear();
    m.append({ kind: "stdout", text: "c\n" });
    expect(texts(m)).toEqual(["c"]);
  });
});

describe("paletteColor", () => {
  it("maps the 256-colour cube and grey ramp", () => {
    expect(paletteColor(1)).toBe("var(--ansi-red)");
    expect(paletteColor(9)).toBe("var(--ansi-bright-red)");
    expect(paletteColor(16)).toBe("rgb(0, 0, 0)");
    expect(paletteColor(231)).toBe("rgb(255, 255, 255)");
    expect(paletteColor(232)).toBe("rgb(8, 8, 8)");
  });
});

describe("segmentLine", () => {
  it("splits styled spans at link boundaries", () => {
    const m = new ConsoleModel();
    m.append({ kind: "stderr", text: "error at \x1b[1msrc/ma\x1b[0min.rs:3:1 here\n" });
    const line = m.lines[0]!;
    const segs = segmentLine(line.spans, findOutputLinks(line.text));
    expect(segs.map((s) => [s.text, !!s.link, !!s.style.bold])).toEqual([
      ["error at ", false, false],
      ["src/ma", true, true],
      ["in.rs:3:1", true, false],
      [" here", false, false],
    ]);
    expect(segs.map((s) => s.text).join("")).toBe(line.text);
  });

  it("returns spans unchanged without links", () => {
    expect(segmentLine([{ text: "a", fg: "x" }, { text: "b" }], [])).toEqual([
      { text: "a", style: { fg: "x" } },
      { text: "b", style: {} },
    ]);
  });
});
