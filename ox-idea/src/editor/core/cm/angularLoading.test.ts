import { expect, it } from "vitest";
import { highlightTree, tags as t } from "@lezer/highlight";
import { findLanguageDescription, loadSupport } from "./languages";
import { oxideaHighlightStyle } from "./highlight";

it("lazily loads Angular templates with the editor's HTML and expression colours", async () => {
  const desc = findLanguageDescription("Angular Template", "/angular/src/app/app.component.html");
  expect(desc?.name).toBe("Angular Template");
  const support = await loadSupport(desc!);
  expect(support?.language.name).toBe("angular");
  const doc = '<button [disabled]="busy">{{ name }}</button> @if (ready) { <p>Ready</p> }';
  const tokens: { text: string; cls: string }[] = [];
  highlightTree(support!.language.parser.parse(doc), oxideaHighlightStyle, (from, to, cls) => {
    tokens.push({ text: doc.slice(from, to), cls });
  });
  for (const [text, tag] of [
    ["button", t.tagName],
    ["disabled", t.attributeName],
    ["busy", t.variableName],
    ["{{", t.special(t.brace)],
    ["name", t.variableName],
    ["}}", t.special(t.brace)],
    ["@if", t.controlKeyword],
  ] as const) {
    const cls = oxideaHighlightStyle.style([tag]);
    expect(cls).toBeTruthy();
    expect(tokens).toContainEqual({ text, cls });
  }
  expect(findLanguageDescription("HTML", "/plain/index.html")?.name).toBe("HTML");
  expect(findLanguageDescription("Batch", "/plain/build.bat")?.name).toBe("Batch");
});
