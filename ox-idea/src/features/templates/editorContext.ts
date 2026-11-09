/**
 * Facts about the caret position used by template variables: enclosing
 * class / function names, comment tokens, whether the caret is inside a
 * string or comment. Syntax tree first, text scanning as a fallback.
 */
import { syntaxTree } from "@codemirror/language";
import type { EditorState } from "@codemirror/state";
import { camelCase, decapitalize, snakeCase, type TemplateFunctions } from "./live/model";

type SyntaxNode = ReturnType<ReturnType<typeof syntaxTree>["resolveInner"]>;

const CLASS_NODE =
  /^(Class|Interface|Struct|Enum|Trait|Impl|Object|Record|Type)(Declaration|Definition|Item|Spec|Decl)?$/;
const FUNC_NODE = /^(Function|Method|Constructor|Fun)(Declaration|Definition|Item|Decl)$/;
const NAME_NODE =
  /^(VariableDefinition|VariableName|TypeIdentifier|TypeDefinition|Definition|DefName|BoundIdentifier|PropertyDefinition|PropertyName|Identifier|Name|TypeName|FieldName)$/;

function nodeName(state: EditorState, node: SyntaxNode, kind: "class" | "func"): string | null {
  const names: string[] = [];
  for (let c = node.firstChild; c; c = c.nextSibling) {
    if (/Body|Block|ParamList|Parameters|ArgList/.test(c.name)) break;
    if (NAME_NODE.test(c.name)) names.push(state.sliceDoc(c.from, c.to));
  }
  if (!names.length) return null;
  // `impl Trait for Type` names the type.
  return kind === "class" && /^Impl/.test(node.name) ? names[names.length - 1]! : names[0]!;
}

function scanBack(text: string, re: RegExp): string | null {
  let last: string | null = null;
  for (const m of text.matchAll(re)) last = m[m.length - 1] ?? null;
  return last;
}

export function enclosingName(state: EditorState, pos: number, kind: "class" | "func"): string {
  const re = kind === "class" ? CLASS_NODE : FUNC_NODE;
  for (let n: SyntaxNode | null = syntaxTree(state).resolveInner(pos, -1); n; n = n.parent) {
    if (re.test(n.name)) {
      const name = nodeName(state, n, kind);
      if (name) return name;
    }
  }
  const before = state.sliceDoc(Math.max(0, pos - 20000), pos);
  const found =
    kind === "class"
      ? scanBack(
          before,
          /\b(?:class|interface|struct|enum|trait|object|record|impl(?:<[^>\n]*>)?(?:\s+[\w:<>]+\s+for)?)\s+([A-Za-z_$][\w$]*)/g,
        )
      : scanBack(before, /\b(?:fn|def|func|function|fun)\s+(?:\([^)\n]*\)\s*)?([A-Za-z_$][\w$]*)\s*[(<]/g);
  return found ?? "";
}

const COMMENT_OR_STRING = /Comment|String|Docstring|Template|Char(acter)?Literal|RawString/i;

export function inCommentOrString(state: EditorState, pos: number): boolean {
  for (let n: SyntaxNode | null = syntaxTree(state).resolveInner(pos, -1); n; n = n.parent) {
    if (COMMENT_OR_STRING.test(n.name) && n.from < pos && (n.to > pos || !/String|Literal/.test(n.name))) {
      // Directly after a closed string the caret is outside it.
      if (/String|Literal/i.test(n.name) && n.to === pos) return false;
      return true;
    }
  }
  return false;
}

export function inComment(state: EditorState, pos: number): boolean {
  for (let n: SyntaxNode | null = syntaxTree(state).resolveInner(pos, -1); n; n = n.parent)
    if (/Comment/i.test(n.name) && n.from < pos) return true;
  return false;
}

export function commentTokens(
  state: EditorState,
  pos: number,
): { line?: string; block?: { open: string; close: string } } {
  const data = state.languageDataAt<{ line?: string; block?: { open: string; close: string } }>("commentTokens", pos);
  return data[0] ?? {};
}

const STOP_WORDS = new Set([
  "get",
  "new",
  "iter",
  "into_iter",
  "iter_mut",
  "clone",
  "unwrap",
  "expect",
  "to_string",
  "to_owned",
  "as_ref",
  "collect",
  "await",
  "toString",
  "valueOf",
  "build",
  "of",
]);

/**
 * Variable name for an expression, IDEA-style: `getUserName()` → `userName`,
 * `items.iter()` → `iter`, `new ArrayList<>()` → `arrayList`.
 */
export function suggestVariableName(expr: string, language: string, fallback = "value"): string {
  const snake = language === "rust" || language === "python";
  const cleaned = expr
    .replace(/<[^<>()]*>/g, "")
    .replace(/\((?:[^()]|\([^()]*\))*\)|\[(?:[^[\]])*\]/g, "")
    .replace(/[?!&*]/g, "");
  const parts = cleaned.split(/\.|::|->|\s+/).filter((p) => /^[A-Za-z_$][\w$]*$/.test(p));
  let word = parts[parts.length - 1] ?? "";
  if (word === "new" || !word) word = parts[parts.length - 2] ?? "";
  const m = /^(get|is|has|to|as)([A-Z_].*)$/.exec(word);
  if (m) word = m[2]!.replace(/^_/, "");
  if (STOP_WORDS.has(word)) word = parts[parts.length - 2] ?? "";
  if (!word || /^\d/.test(word)) return fallback;
  const name = snake ? snakeCase(word) || fallback : decapitalize(/^[A-Z]/.test(word) ? word : camelCase(word) || word);
  return KEYWORDS.has(name) ? `${name}Value` : name;
}

const KEYWORDS = new Set([
  "if",
  "else",
  "for",
  "while",
  "do",
  "return",
  "class",
  "fn",
  "let",
  "const",
  "var",
  "val",
  "def",
  "func",
  "type",
  "match",
  "in",
  "is",
  "as",
  "new",
  "this",
  "self",
  "super",
  "import",
  "package",
  "struct",
  "enum",
  "impl",
  "trait",
  "loop",
  "break",
  "continue",
  "true",
  "false",
  "null",
  "nil",
  "None",
  "default",
  "switch",
  "case",
  "try",
  "catch",
]);

export interface TemplateEnvInput {
  state: EditorState;
  pos: number;
  path: string;
  language: string;
  clipboard?: string;
  user?: string;
  /** Postfix expression, for `suggestVariableName()` / `expressionText()`. */
  expression?: string;
}

/** Variable functions bound to an editor position. */
export function templateFunctions(input: TemplateEnvInput): TemplateFunctions {
  const { state, pos, path, language } = input;
  const file = path.slice(path.lastIndexOf("/") + 1);
  const dot = file.lastIndexOf(".");
  const tokens = () => commentTokens(state, pos);
  return {
    className: () => enclosingName(state, pos, "class"),
    methodName: () => enclosingName(state, pos, "func"),
    functionName: () => enclosingName(state, pos, "func"),
    fileName: () => file,
    fileNameWithoutExtension: () => (dot > 0 ? file.slice(0, dot) : file),
    filePath: () => path,
    user: () => input.user ?? "",
    clipboard: () => input.clipboard ?? "",
    suggestVariableName: () => suggestVariableName(input.expression ?? "", language, ""),
    expressionText: () => input.expression ?? "",
    commentStart: () => {
      if (inComment(state, pos)) return "";
      const t = tokens();
      return t.line ? `${t.line} ` : t.block ? `${t.block.open} ` : "// ";
    },
    commentEnd: () => {
      if (inComment(state, pos)) return "";
      const t = tokens();
      return !t.line && t.block ? ` ${t.block.close}` : "";
    },
    lineCommentStart: () => tokens().line ?? "//",
  };
}
