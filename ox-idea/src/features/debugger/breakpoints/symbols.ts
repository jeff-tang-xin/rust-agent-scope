/**
 * Method / field declarations on a source line, named the way debug adapters expect
 * them in function breakpoints and data breakpoint ids. Text-based (no PSI): good for
 * ordinary declarations, which is what IDEA's gutter toggles act on.
 */

export interface MethodTarget {
  /** Function breakpoint name for the language's adapter. */
  name: string;
  /** Short label (`Order.total()`). */
  label: string;
}

export interface FieldTarget {
  /** java-debug data breakpoint id (`pkg.Class#field`). */
  dataId: string;
  label: string;
}

const CONTROL = new Set([
  "if",
  "for",
  "while",
  "switch",
  "catch",
  "return",
  "new",
  "throw",
  "else",
  "do",
  "try",
  "synchronized",
  "assert",
  "super",
  "this",
]);

/** Strips string/char literals and comments so braces and keywords inside them don't count. */
function code(line: string, state: { block: boolean }): string {
  let out = "";
  for (let i = 0; i < line.length; i++) {
    const c = line[i]!;
    if (state.block) {
      if (c === "*" && line[i + 1] === "/") {
        state.block = false;
        i++;
      }
      continue;
    }
    if (c === "/" && line[i + 1] === "/") break;
    if (c === "/" && line[i + 1] === "*") {
      state.block = true;
      i++;
      continue;
    }
    if (c === '"' || c === "'") {
      const q = c;
      i++;
      while (i < line.length && line[i] !== q) i += line[i] === "\\" ? 2 : 1;
      out += q + q;
      continue;
    }
    out += c;
  }
  return out;
}

/** Enclosing types of `line` (outermost first) in a brace language, plus the package. */
function javaContext(lines: readonly string[], line: number): { pkg: string; types: string[] } {
  let pkg = "";
  const stack: Array<{ name: string | null; depth: number }> = [];
  let depth = 0;
  let pendingType: string | null = null;
  const state = { block: false };
  for (let i = 0; i < line; i++) {
    const text = code(lines[i] ?? "", state);
    const p = /^\s*package\s+([\w.]+)/.exec(text);
    if (p) pkg = p[1]!;
    const t = /\b(?:class|interface|enum|record|object)\s+([A-Za-z_$][\w$]*)/.exec(text);
    if (t) pendingType = t[1]!;
    for (const c of text) {
      if (c === "{") {
        depth++;
        stack.push({ name: pendingType, depth });
        pendingType = null;
      } else if (c === "}") {
        while (stack.length && stack[stack.length - 1]!.depth >= depth) stack.pop();
        depth--;
      }
    }
  }
  // A type declared on the line itself (one-line `class A { void f() {} }`) is ignored.
  return { pkg, types: stack.filter((s) => s.name).map((s) => s.name!) };
}

function qualified(pkg: string, types: string[]): string | null {
  if (!types.length) return null;
  return (pkg ? `${pkg}.` : "") + types.join("$");
}

const JAVA_METHOD = /^\s*(?:@\w+(?:\([^)]*\))?\s+)*(?:[\w<>,.?[\]\s]+?\s+)?([A-Za-z_$][\w$]*)\s*\(/;

function javaMethod(lines: readonly string[], line: number): MethodTarget | null {
  const text = code(lines[line] ?? "", { block: false });
  const m = JAVA_METHOD.exec(text);
  if (!m) return null;
  const name = m[1]!;
  const before = text.slice(0, m.index + m[0].length - 1 - name.length).trim();
  if (CONTROL.has(name) || /[=.]|\breturn\b|\bnew\b/.test(before)) return null;
  // `foo(x);` is a call; a declaration has a type or modifiers in front, or is a constructor.
  const { pkg, types } = javaContext(lines, line);
  const cls = qualified(pkg, types);
  if (!cls) return null;
  const isConstructor = name === types[types.length - 1];
  if (!before && !isConstructor) return null;
  const method = isConstructor ? "<init>" : name;
  return { name: `${cls}#${method}`, label: `${types[types.length - 1]}.${isConstructor ? name : method}()` };
}

const JAVA_FIELD =
  /^\s*(?:@\w+(?:\([^)]*\))?\s+)*((?:(?:public|protected|private|static|final|volatile|transient)\s+)*)[\w<>,.?[\]\s]+?\s+([A-Za-z_$][\w$]*)\s*(?:=|;)/;

function javaField(lines: readonly string[], line: number): FieldTarget | null {
  const text = code(lines[line] ?? "", { block: false });
  if (text.includes("(") && !/=/.test(text.slice(0, text.indexOf("(")))) return null;
  const m = JAVA_FIELD.exec(text);
  if (!m) return null;
  const name = m[2]!;
  if (CONTROL.has(name) || /^\s*return\b/.test(text)) return null;
  const { pkg, types } = javaContext(lines, line);
  const cls = qualified(pkg, types);
  // Only directly inside a type body (a local variable sits inside a method's braces).
  if (!cls || !insideTypeBody(lines, line, types.length)) return null;
  return { dataId: `${cls}#${name}`, label: `${types[types.length - 1]}.${name}` };
}

/** Whether every open brace before `line` belongs to a type declaration. */
function insideTypeBody(lines: readonly string[], line: number, typeDepth: number): boolean {
  let depth = 0;
  const state = { block: false };
  for (let i = 0; i < line; i++) {
    for (const c of code(lines[i] ?? "", state)) {
      if (c === "{") depth++;
      else if (c === "}") depth--;
    }
  }
  return depth === typeDepth;
}

function goMethod(lines: readonly string[], line: number): MethodTarget | null {
  const m = /^\s*func\s+(?:\(\s*\w*\s*(\*?)\s*([A-Za-z_]\w*)(?:\[[^\]]*\])?\s*\)\s*)?([A-Za-z_]\w*)\s*[[(]/.exec(
    lines[line] ?? "",
  );
  if (!m) return null;
  const pkg = lines.map((l) => /^\s*package\s+(\w+)/.exec(l)?.[1]).find(Boolean) ?? "main";
  const [, star, recv, fn] = m;
  const name = recv ? (star ? `${pkg}.(*${recv}).${fn}` : `${pkg}.${recv}.${fn}`) : `${pkg}.${fn}`;
  return { name, label: recv ? `${recv}.${fn}()` : `${fn}()` };
}

function simple(re: RegExp, lines: readonly string[], line: number): MethodTarget | null {
  const m = re.exec(lines[line] ?? "");
  return m ? { name: m[1]!, label: `${m[1]}()` } : null;
}

const C_FUNCTION =
  /^\s*(?:(?:static|inline|extern|virtual|constexpr|const|unsigned|signed|struct|enum)\s+)*[A-Za-z_][\w:<>,*&\s]*?[\s*&]([A-Za-z_~][\w:~]*)\s*\([^;]*$/;

/** The method declared on `line` (0-based), named for `language`'s debug adapter. */
export function methodAt(language: string, text: string | readonly string[], line: number): MethodTarget | null {
  const lines = typeof text === "string" ? text.split("\n") : text;
  switch (language) {
    case "java":
    case "kotlin":
      return language === "java"
        ? javaMethod(lines, line)
        : simple(/^\s*(?:[\w@]+\s+)*fun\s+(?:<[^>]*>\s*)?(?:[\w.]+\.)?([A-Za-z_]\w*)\s*\(/, lines, line);
    case "python":
      return simple(/^\s*(?:async\s+)?def\s+([A-Za-z_]\w*)\s*\(/, lines, line);
    case "go":
      return goMethod(lines, line);
    case "rust":
      return simple(
        /^\s*(?:pub(?:\([^)]*\))?\s+)?(?:(?:const|async|unsafe|extern\s+"[^"]*")\s+)*fn\s+([A-Za-z_]\w*)/,
        lines,
        line,
      );
    case "javascript":
    case "typescript":
    case "javascriptreact":
    case "typescriptreact":
      return (
        simple(/^\s*(?:export\s+)?(?:default\s+)?(?:async\s+)?function\*?\s+([A-Za-z_$][\w$]*)/, lines, line) ??
        simple(
          /^\s*(?:(?:public|private|protected|static|async|get|set)\s+)*([A-Za-z_$][\w$]*)\s*\([^)]*\)\s*(?::[^{]*)?\{/,
          lines,
          line,
        )
      );
    case "c":
    case "cpp":
    case "objective-c":
    case "objective-cpp": {
      const t = simple(C_FUNCTION, lines, line);
      return t && !CONTROL.has(t.name) ? t : null;
    }
    case "swift":
      return simple(/^\s*(?:[\w@]+\s+)*func\s+([A-Za-z_]\w*)/, lines, line);
    default:
      return null;
  }
}

/** The field declared on `line` (0-based) as a persistent watchpoint id (Java only). */
export function fieldAt(language: string, text: string, line: number): FieldTarget | null {
  if (language !== "java") return null;
  return javaField(text.split("\n"), line);
}
