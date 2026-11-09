/**
 * Code Style › Formatter: "Do not format" globs and formatter on/off markers
 * in code comments (`@formatter:off` … `@formatter:on`). Reformat Code, format
 * on save and the before-commit reformat skip excluded files and leave the
 * text between markers untouched.
 */
import { editorPrefs } from "./editorSettings";

export interface FormatGuardOptions {
  doNotFormat: readonly string[];
  formatterTagsEnabled: boolean;
  formatterOffTag: string;
  formatterOnTag: string;
  formatterTagsRegexp: boolean;
}

export interface TextChangeLike {
  from: number;
  to: number;
  insert: string;
}

/** The Formatter options of the effective settings. */
export function formatGuardOptions(settings?: unknown): FormatGuardOptions {
  const cs = editorPrefs(settings).codeStyle;
  return {
    doNotFormat: cs.doNotFormat,
    formatterTagsEnabled: cs.formatterTagsEnabled,
    formatterOffTag: cs.formatterOffTag,
    formatterOnTag: cs.formatterOnTag,
    formatterTagsRegexp: cs.formatterTagsRegexp,
  };
}

function globRegExp(glob: string): RegExp {
  let re = "";
  for (let i = 0; i < glob.length; i++) {
    const c = glob[i]!;
    if (c === "*" && glob[i + 1] === "*") {
      re += ".*";
      i++;
      if (glob[i + 1] === "/") i++;
    } else if (c === "*") re += "[^/]*";
    else if (c === "?") re += "[^/]";
    else re += c.replace(/[.+^${}()|[\]\\]/g, "\\$&");
  }
  return new RegExp(`^${re}$`);
}

/**
 * Whether "Do not format" excludes `path`. A pattern without `/` matches the
 * file name or any folder name (`*.min.js`, `generated`); one with `/` matches
 * the project-relative path or a folder containing it (`src/gen/**`, `vendor/`).
 */
export function formatExcluded(path: string, root: string | null, globs: readonly string[]): boolean {
  const rel = root && path.startsWith(root + "/") ? path.slice(root.length + 1) : path;
  const segments = rel.split("/").filter(Boolean);
  for (const raw of globs) {
    const glob = raw.trim().replace(/^\.\//, "");
    if (!glob) continue;
    if (!glob.includes("/")) {
      const re = globRegExp(glob);
      if (segments.some((s) => re.test(s))) return true;
      continue;
    }
    const re = globRegExp(glob.replace(/^\/+/, "").replace(/\/+$/, ""));
    for (let i = segments.length; i > 0; i--) if (re.test(segments.slice(0, i).join("/"))) return true;
  }
  return false;
}

/** A comment opener before the marker on its line (`//`, `#`, `/*`, `<!--`, `--`, `;`, `*`, `%`). */
const COMMENT_BEFORE = /(\/\/|#|\/\*|<!--|--|;|\*|%|\{-|\(\*|')/;

function markerMatcher(tag: string, regexp: boolean): RegExp | null {
  if (!tag.trim()) return null;
  try {
    return new RegExp(regexp ? tag : tag.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"), "g");
  } catch {
    return null;
  }
}

/** Marker hits in comments: offsets of their lines (`start`..`end`, end excludes the newline). */
function markerLines(text: string, re: RegExp): Array<{ start: number; end: number }> {
  const out: Array<{ start: number; end: number }> = [];
  re.lastIndex = 0;
  let m: RegExpExecArray | null;
  while ((m = re.exec(text))) {
    if (m[0] === "") {
      re.lastIndex++;
      continue;
    }
    const start = text.lastIndexOf("\n", m.index - 1) + 1;
    const nl = text.indexOf("\n", m.index + m[0].length);
    const end = nl < 0 ? text.length : nl;
    if (COMMENT_BEFORE.test(text.slice(start, m.index)) && out.at(-1)?.start !== start) out.push({ start, end });
    re.lastIndex = Math.max(re.lastIndex, end);
  }
  return out;
}

/**
 * Text the formatter must not touch: from the line of each off marker to the
 * end of the line of the next on marker (or the end of the text).
 */
export function protectedRegions(text: string, opts: FormatGuardOptions): Array<{ from: number; to: number }> {
  if (!opts.formatterTagsEnabled) return [];
  const off = markerMatcher(opts.formatterOffTag, opts.formatterTagsRegexp);
  const on = markerMatcher(opts.formatterOnTag, opts.formatterTagsRegexp);
  if (!off) return [];
  const offs = markerLines(text, off);
  const ons = on ? markerLines(text, on) : [];
  const out: Array<{ from: number; to: number }> = [];
  let pos = 0;
  for (const o of offs) {
    if (o.start < pos) continue;
    const end = ons.find((x) => x.start > o.start);
    out.push({ from: o.start, to: end ? end.end : text.length });
    pos = end ? end.end : text.length;
    if (!end) break;
  }
  return out;
}

const touches = (c: { from: number; to: number }, r: { from: number; to: number }) =>
  c.from === c.to ? c.from > r.from && c.from < r.to : c.from < r.to && c.to > r.from;

/** The change minus its common prefix / suffix with the replaced text. */
function narrow(text: string, c: TextChangeLike): TextChangeLike | null {
  const old = text.slice(c.from, c.to);
  if (old === c.insert) return null;
  const max = Math.min(old.length, c.insert.length);
  let s = 0;
  while (s < max && old.charCodeAt(s) === c.insert.charCodeAt(s)) s++;
  let eo = old.length;
  let ei = c.insert.length;
  while (eo > s && ei > s && old.charCodeAt(eo - 1) === c.insert.charCodeAt(ei - 1)) {
    eo--;
    ei--;
  }
  return { from: c.from + s, to: c.from + eo, insert: c.insert.slice(s, ei) };
}

/**
 * Drops formatting changes inside protected regions. A change replacing whole
 * regions (a formatter rewriting the file) is split around them, provided the
 * formatted text keeps the same markers; other overlapping changes are dropped.
 */
export function guardFormatChanges<T extends TextChangeLike>(
  text: string,
  changes: readonly T[],
  opts: FormatGuardOptions,
): TextChangeLike[] {
  const regions = protectedRegions(text, opts);
  if (!regions.length) return [...changes];
  const out: TextChangeLike[] = [];
  for (const raw of changes) {
    const c = narrow(text, raw);
    if (!c) continue;
    const hit = regions.filter((r) => touches(c, r));
    if (!hit.length) {
      out.push(c);
      continue;
    }
    // Whole regions inside the change: map them onto the formatted text.
    if (hit.some((r) => r.from < c.from || r.to > c.to)) continue;
    const local = protectedRegions(c.insert, opts);
    if (local.length !== hit.length) continue;
    let oldPos = c.from;
    let newPos = 0;
    for (let i = 0; i <= hit.length; i++) {
      const oldEnd = i < hit.length ? hit[i]!.from : c.to;
      const newEnd = i < hit.length ? local[i]!.from : c.insert.length;
      const piece = narrow(text, { from: oldPos, to: oldEnd, insert: c.insert.slice(newPos, newEnd) });
      if (piece) out.push(piece);
      if (i < hit.length) {
        oldPos = hit[i]!.to;
        newPos = local[i]!.to;
      }
    }
  }
  return out;
}

interface PosLike {
  line: number;
  col: number;
}

/** [`guardFormatChanges`] for LSP-style edits (0-based lines, UTF-16 columns) against `text`. */
export function guardTextEdits(
  text: string,
  edits: ReadonlyArray<{ range: { start: PosLike; end: PosLike }; new_text: string }>,
  opts: FormatGuardOptions,
): Array<{ range: { start: PosLike; end: PosLike }; new_text: string }> {
  if (!protectedRegions(text, opts).length) return [...edits];
  const starts = [0];
  for (let i = text.indexOf("\n"); i >= 0; i = text.indexOf("\n", i + 1)) starts.push(i + 1);
  const offset = (p: PosLike) => {
    const line = Math.min(Math.max(0, p.line), starts.length - 1);
    const end = line + 1 < starts.length ? starts[line + 1]! - 1 : text.length;
    return Math.min(starts[line]! + p.col, end);
  };
  const pos = (o: number): PosLike => {
    let lo = 0;
    let hi = starts.length - 1;
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1;
      if (starts[mid]! <= o) lo = mid;
      else hi = mid - 1;
    }
    return { line: lo, col: o - starts[lo]! };
  };
  const changes = edits
    .map((e) => ({ from: offset(e.range.start), to: offset(e.range.end), insert: e.new_text }))
    .sort((a, b) => a.from - b.from || a.to - b.to);
  return guardFormatChanges(text, changes, opts).map((c) => ({
    range: { start: pos(c.from), end: pos(c.to) },
    new_text: c.insert,
  }));
}
