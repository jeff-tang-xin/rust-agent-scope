/**
 * View › Expression Type Info (⌃⇧P): the type of the symbol / expression at
 * the caret, taken from the language server's hover (its signature code
 * block), shown in a small popup.
 */
import type { EditorView } from "@codemirror/view";
import type { Markup } from "@/bindings";
import { docOf, reportLspError } from "./lsp";
import { requestHover } from "./hover";
import { rangeToOffsets } from "./positions";
import { showHint } from "./hint";
import { caretAnchor, openPopup } from "./popupStore";

export interface TypeInfo {
  /** The type, or the whole signature when no type can be split off. */
  text: string;
  /** Fence language of the hover's code block, if any. */
  language: string | null;
}

const FENCE = /```([\w+#-]*)[^\n]*\n([\s\S]*?)```/g;
/** A lone module / package path (rust-analyzer's first hover block). */
const PATH_ONLY = /^[\w$]+(?:(?:::|\.)[\w$]+)*$/;
const MODIFIERS =
  "(?:let|const|var|val|mut|static|readonly|export|declare|public|private|protected|internal|final|ref)";
/** `(local var) x: T`, `let mut x: T`, `(property) a.b?: T`, `x: T`. */
const COLON_TYPED = new RegExp(`^(?:\\([\\w ]+\\)\\s*)?(?:${MODIFIERS}\\s+)*[\\w$.]+\\??\\s*:\\s*([\\s\\S]+)$`);
/** Go: `var x int`, `field x string`, `const x untyped int`. */
const GO_TYPED = /^(?:var|field|const)\s+[\w$]+\s+([\s\S]+?)(?:\s*=.*)?$/;

/** Signature → type for variables / properties / parameters; other signatures as they are. */
export function typeFromSignature(signature: string): string {
  const sig = signature.replace(/\s+/g, " ").trim();
  const colon = COLON_TYPED.exec(sig);
  if (colon) return colon[1]!.replace(/\s=(?!>)\s[\s\S]*$/, "").trim();
  const go = GO_TYPED.exec(sig);
  if (go) return go[1]!.trim();
  return sig;
}

/** Extracts the type shown by Expression Type Info from hover markup, or null. */
export function extractTypeInfo(m: Markup | null | undefined): TypeInfo | null {
  if (!m || !m.value.trim()) return null;
  if (m.kind !== "markdown") {
    const first = m.value.split(/\r?\n/).find((l) => l.trim());
    return first ? { text: typeFromSignature(first), language: null } : null;
  }
  const blocks = [...m.value.matchAll(FENCE)]
    .map((b) => ({ language: b[1] || null, code: b[2]!.trim() }))
    .filter((b) => b.code);
  const block = blocks.find((b) => !PATH_ONLY.test(b.code)) ?? blocks[0];
  if (block) return { text: typeFromSignature(block.code), language: block.language };
  const inline = /`([^`]+)`/.exec(m.value);
  return inline ? { text: typeFromSignature(inline[1]!), language: null } : null;
}

export async function expressionTypeInfo(view: EditorView) {
  const pos = view.state.selection.main.head;
  try {
    const hover = await requestHover(view, pos);
    const info = extractTypeInfo(hover?.contents);
    if (!hover || !info) {
      showHint(view, "No type information found");
      return;
    }
    const language = info.language ?? docOf(view)?.uiMode ?? "";
    openPopup({
      kind: "doc",
      anchor: caretAnchor(view, hover.range ? rangeToOffsets(view.state.doc, hover.range).from : pos),
      entries: [
        {
          markup: { kind: "markdown", value: `\`\`\`${language}\n${info.text}\n\`\`\`` },
          language: docOf(view)?.uiMode ?? null,
        },
      ],
      index: 0,
      themeClasses: view.themeClasses,
    });
  } catch (e) {
    reportLspError(view, e, true);
  }
}
