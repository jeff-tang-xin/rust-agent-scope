/**
 * Go to Test / Go to Test Subject (⇧⌘T) naming conventions, per language:
 * JVM / .NET `FooTest`, `FooTests`, `TestFoo`; Go `foo_test.go`; Python
 * `test_foo.py`, `foo_test.py`; JS/TS `foo.test.ts`, `foo.spec.ts`; Ruby
 * `foo_spec.rb`, `foo_test.rb`; C/C++ `foo_test.cc`, `test_foo.c`; Rust
 * integration tests `tests/foo.rs` (unit tests live in the file's
 * `#[cfg(test)] mod`). Pure: no IPC.
 */

export interface SplitName {
  stem: string;
  /** Extension(s) after the stem, with the dot: `.java`, `.test.ts`. */
  ext: string;
}

const JS_EXT = /\.(?:[cm]?[jt]sx?)$/;
const JVM_EXT = /\.(java|kt|kts|scala|groovy|cs|fs|vb|swift|dart|php)$/;
const C_EXT = /\.(c|cc|cpp|cxx|h|hpp|m|mm)$/;

export function splitName(name: string): SplitName {
  const dot = name.lastIndexOf(".");
  return dot > 0 ? { stem: name.slice(0, dot), ext: name.slice(dot) } : { stem: name, ext: "" };
}

/** Whether `name` looks like a test by name alone (the test roots are checked separately). */
export function isTestName(name: string): boolean {
  const { stem, ext } = splitName(name);
  if (/^\.(test|spec)$/.test(splitName(stem).ext) && JS_EXT.test(ext)) return true;
  if (ext === ".go") return stem.endsWith("_test");
  if (ext === ".py") return stem.startsWith("test_") || stem.endsWith("_test") || stem === "conftest";
  if (ext === ".rb") return stem.endsWith("_spec") || stem.endsWith("_test");
  if (JVM_EXT.test(name))
    return /^Test[A-Z0-9_]/.test(stem) || (/Tests?$/.test(stem) && stem !== "Test" && stem !== "Tests");
  if (C_EXT.test(name)) return stem.startsWith("test_") || stem.endsWith("_test") || stem.endsWith("_unittest");
  return false;
}

/** File names a test of `name` may have, most conventional first. */
export function testNamesFor(name: string): string[] {
  const { stem, ext } = splitName(name);
  if (JS_EXT.test(name)) return [`${stem}.test${ext}`, `${stem}.spec${ext}`];
  if (ext === ".go") return [`${stem}_test.go`];
  if (ext === ".py") return [`test_${stem}.py`, `${stem}_test.py`];
  if (ext === ".rb") return [`${stem}_spec.rb`, `${stem}_test.rb`];
  if (ext === ".rs") return [`${stem}.rs`, `${stem}_test.rs`, `${stem}_tests.rs`];
  if (JVM_EXT.test(name))
    return [`${stem}Test${ext}`, `${stem}Tests${ext}`, `Test${stem}${ext}`, `${stem}IT${ext}`, `${stem}Spec${ext}`];
  if (C_EXT.test(name)) return [`${stem}_test${ext}`, `test_${stem}${ext}`, `${stem}_unittest${ext}`];
  return [`${stem}_test${ext}`, `test_${stem}${ext}`];
}

/** File names the subject of test `name` may have. */
export function subjectNamesFor(name: string): string[] {
  const { stem, ext } = splitName(name);
  const inner = splitName(stem);
  if (JS_EXT.test(ext) && /^\.(test|spec)$/.test(inner.ext)) {
    const base = inner.stem;
    // `foo.test.ts` may test `foo.tsx` / `foo.js` too.
    return [...new Set([`${base}${ext}`, ...[".ts", ".tsx", ".js", ".jsx", ".mts", ".mjs"].map((e) => `${base}${e}`)])];
  }
  const out: string[] = [];
  const strip = (s: string) => out.push(`${s}${ext}`);
  if (ext === ".go" || ext === ".py" || ext === ".rb" || ext === ".rs" || C_EXT.test(name)) {
    for (const suffix of ["_test", "_tests", "_spec", "_unittest"])
      if (stem.endsWith(suffix) && stem.length > suffix.length) strip(stem.slice(0, -suffix.length));
    if (stem.startsWith("test_") && stem.length > 5) strip(stem.slice(5));
    // Rust integration tests (`tests/foo.rs`) are named like their subject.
    if (ext === ".rs") strip(stem);
    return [...new Set(out)];
  }
  for (const suffix of ["Tests", "Test", "IT", "Spec"])
    if (stem.endsWith(suffix) && stem.length > suffix.length) strip(stem.slice(0, -suffix.length));
  if (/^Test[A-Z0-9_]/.test(stem)) strip(stem.slice(4));
  return [...new Set(out)];
}

/** Common leading path segments of two `/`-separated paths. */
function sharedPrefix(a: string, b: string): number {
  const x = a.split("/");
  const y = b.split("/");
  let n = 0;
  while (n < x.length && n < y.length && x[n] === y[n]) n++;
  return n;
}

/**
 * Orders candidate paths for `from`: the mirrored package (same path below
 * its root) first, then the closest in the tree.
 */
export function rankCandidates(
  from: string,
  candidates: readonly string[],
  relBelowRoot: (path: string) => string | null,
): string[] {
  const own = relBelowRoot(from);
  const ownDir = own === null ? null : own.slice(0, Math.max(own.lastIndexOf("/"), 0));
  const score = (p: string) => {
    const rel = relBelowRoot(p);
    const dir = rel === null ? null : rel.slice(0, Math.max(rel.lastIndexOf("/"), 0));
    return (ownDir !== null && dir === ownDir ? 1000 : 0) + sharedPrefix(from, p);
  };
  return [...candidates].sort((a, b) => score(b) - score(a) || (a < b ? -1 : a > b ? 1 : 0));
}

/** Offset of the `#[cfg(test)]` module of a Rust file, if any. */
export function rustTestModuleOffset(text: string): number | null {
  const m = /#\[cfg\(test\)\]\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+/.exec(text);
  return m ? m.index : null;
}

/** The name a new test of `name` gets (without directory), by convention. */
export function newTestName(name: string): string {
  return testNamesFor(name)[0]!;
}
