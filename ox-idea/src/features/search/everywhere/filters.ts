/**
 * Per-tab result filters of Search Everywhere (IDEA's funnel): file types on
 * Files, languages + symbol kinds on Classes / Symbols, a file mask on Text.
 * Stored as exclusions so newly known languages / kinds show by default.
 */
import type { LanguageInfo } from "@/platform/ipc";
import type { SeTab } from "./model";

export interface SeFilters {
  /** Excluded language ids (Files). */
  files: { languages: string[] };
  /** Excluded language ids and symbol kinds. */
  classes: { languages: string[]; kinds: string[] };
  symbols: { languages: string[]; kinds: string[] };
  /** Comma-separated globs (`*.ts, !*.test.*`); empty = every file. */
  text: { mask: string };
}

export type FilterTab = keyof SeFilters;

export const FILTER_TABS: readonly FilterTab[] = ["files", "classes", "symbols", "text"];

export const isFilterTab = (tab: SeTab): tab is FilterTab => (FILTER_TABS as readonly string[]).includes(tab);

export const DEFAULT_FILTERS: SeFilters = {
  files: { languages: [] },
  classes: { languages: [], kinds: [] },
  symbols: { languages: [], kinds: [] },
  text: { mask: "" },
};

const strings = (v: unknown): string[] =>
  Array.isArray(v) ? [...new Set(v.filter((x): x is string => typeof x === "string"))] : [];

/** Tolerates missing / malformed stored values (older or newer versions). */
export function sanitizeFilters(raw: unknown): SeFilters {
  const o = (raw && typeof raw === "object" ? raw : {}) as Record<string, Record<string, unknown> | undefined>;
  return {
    files: { languages: strings(o.files?.languages) },
    classes: { languages: strings(o.classes?.languages), kinds: strings(o.classes?.kinds) },
    symbols: { languages: strings(o.symbols?.languages), kinds: strings(o.symbols?.kinds) },
    text: { mask: typeof o.text?.mask === "string" ? o.text.mask : "" },
  };
}

export function filterActive(filters: SeFilters, tab: SeTab): boolean {
  switch (tab) {
    case "files":
      return filters.files.languages.length > 0;
    case "classes":
    case "symbols":
      return filters[tab].languages.length > 0 || filters[tab].kinds.length > 0;
    case "text":
      return !!filters.text.mask.trim();
    default:
      return false;
  }
}

/** Symbol kinds offered per tab, in IDEA's order. */
export const CLASS_KINDS = ["class", "interface", "struct", "enum", "trait", "typeAlias"] as const;
export const SYMBOL_KINDS = [
  ...CLASS_KINDS,
  "function",
  "method",
  "constructor",
  "field",
  "property",
  "variable",
  "constant",
  "enumMember",
  "module",
  "namespace",
  "macro",
] as const;

export const KIND_LABELS: Record<string, string> = {
  class: "Classes",
  interface: "Interfaces",
  struct: "Structs",
  enum: "Enums",
  trait: "Traits",
  typeAlias: "Type Aliases",
  function: "Functions",
  method: "Methods",
  constructor: "Constructors",
  field: "Fields",
  property: "Properties",
  variable: "Variables",
  constant: "Constants",
  enumMember: "Enum Members",
  module: "Modules",
  namespace: "Namespaces",
  macro: "Macros",
};

export function kindsFor(tab: "classes" | "symbols"): readonly string[] {
  return tab === "classes" ? CLASS_KINDS : SYMBOL_KINDS;
}

/** Kinds the menu doesn't list fall under the closest listed one. */
function kindBucket(kind: string): string {
  if (kind === "package") return "module";
  if (kind === "typeParameter") return "typeAlias";
  return kind;
}

/** Extension (lower-case, without dot) → language id. */
export function extensionIndex(languages: readonly Pick<LanguageInfo, "id" | "extensions">[]): Map<string, string> {
  const m = new Map<string, string>();
  for (const l of languages) for (const e of l.extensions) if (!m.has(e.toLowerCase())) m.set(e.toLowerCase(), l.id);
  return m;
}

export function languageOf(path: string, byExt: ReadonlyMap<string, string>): string | null {
  const name = path.slice(path.lastIndexOf("/") + 1).toLowerCase();
  // Longest suffix first: `d.ts` before `ts`.
  for (let i = name.indexOf("."); i >= 0; i = name.indexOf(".", i + 1)) {
    const id = byExt.get(name.slice(i + 1));
    if (id) return id;
  }
  return null;
}

/** What the controller needs to apply a tab's filter. */
export interface ResolvedFilter {
  /** Language id of a path (null when unknown: never filtered out). */
  languageOf?: (path: string) => string | null;
  excludedLanguages?: readonly string[];
  excludedKinds?: readonly string[];
  /** Text tab: include globs. */
  mask?: string;
}

export function resolveFilter(
  filters: SeFilters,
  tab: SeTab,
  languageOfPath: ((path: string) => string | null) | undefined,
): ResolvedFilter | null {
  if (!isFilterTab(tab) || !filterActive(filters, tab)) return null;
  if (tab === "text") return { mask: filters.text.mask.trim() };
  if (tab === "files") return { languageOf: languageOfPath, excludedLanguages: filters.files.languages };
  return { languageOf: languageOfPath, excludedLanguages: filters[tab].languages, excludedKinds: filters[tab].kinds };
}

/** Whether a result passes the filter (`kind` for symbols). */
export function passesFilter(f: ResolvedFilter, path: string, kind?: string): boolean {
  if (kind !== undefined && f.excludedKinds?.length && f.excludedKinds.includes(kindBucket(kind))) return false;
  if (f.excludedLanguages?.length && f.languageOf) {
    const lang = f.languageOf(path);
    if (lang && f.excludedLanguages.includes(lang)) return false;
  }
  return true;
}
