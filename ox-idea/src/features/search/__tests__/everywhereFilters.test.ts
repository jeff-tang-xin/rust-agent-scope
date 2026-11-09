import { describe, expect, it, vi } from "vitest";
import type { SearchQuery, SymbolMatch } from "@/platform/ipc";
import { SeController, type SeDeps, type SeSnapshot } from "../everywhere/controller";
import {
  DEFAULT_FILTERS,
  extensionIndex,
  filterActive,
  languageOf,
  passesFilter,
  resolveFilter,
  sanitizeFilters,
} from "../everywhere/filters";
import { matchActions, matchSettings, mergeActionResults } from "../everywhere/model";

const flush = () => new Promise((r) => setTimeout(r, 0));

function setup(overrides: Partial<SeDeps> = {}) {
  let id = 0;
  const deps: SeDeps = {
    findFiles: vi.fn(async () => []),
    findSymbols: vi.fn(async () => []),
    searchText: vi.fn(async () => {}),
    cancel: vi.fn(),
    nextRequestId: () => ++id,
    actions: () => [],
    isEnabled: () => true,
    recentFiles: async () => [],
    recentActions: () => [],
    root: () => "/p",
    now: () => 0,
    ...overrides,
  };
  const snaps: SeSnapshot[] = [];
  const c = new SeController(deps, (s) => snaps.push(s));
  return { c, deps, last: () => snaps[snaps.length - 1]! };
}

const byExt = extensionIndex([
  { id: "typescript", extensions: ["ts", "d.ts"] },
  { id: "rust", extensions: ["rs"] },
]);
const lang = (p: string) => languageOf(p, byExt);

const fm = (rel: string) => ({ path: `/p/${rel}`, relPath: rel, score: 1, indices: [] });
const sym = (name: string, kind: SymbolMatch["kind"], rel: string): SymbolMatch => ({
  name,
  kind,
  container: null,
  path: `/p/${rel}`,
  relPath: rel,
  range: { start: { line: 0, col: 0 }, end: { line: 0, col: 1 } },
  score: 1,
  indices: [],
});

describe("Search Everywhere filters", () => {
  it("sanitizes stored filters and reports which tabs are filtered", () => {
    expect(sanitizeFilters(null)).toEqual(DEFAULT_FILTERS);
    const f = sanitizeFilters({ files: { languages: ["rust", 3, "rust"] }, text: { mask: "*.ts" }, extra: 1 });
    expect(f.files.languages).toEqual(["rust"]);
    expect(f.symbols).toEqual({ languages: [], kinds: [] });
    expect(filterActive(f, "files")).toBe(true);
    expect(filterActive(f, "text")).toBe(true);
    expect(filterActive(f, "classes")).toBe(false);
    expect(filterActive(f, "all")).toBe(false);
  });

  it("maps paths to languages by their longest known extension", () => {
    expect(lang("/p/a/main.RS")).toBe("rust");
    expect(lang("/p/types.d.ts")).toBe("typescript");
    expect(lang("/p/README")).toBeNull();
  });

  it("keeps unknown file types and buckets unlisted kinds", () => {
    const f = { languageOf: lang, excludedLanguages: ["rust"], excludedKinds: ["module"] };
    expect(passesFilter(f, "/p/a.rs")).toBe(false);
    expect(passesFilter(f, "/p/a.txt")).toBe(true);
    expect(passesFilter(f, "/p/a.ts", "package")).toBe(false);
    expect(passesFilter(f, "/p/a.ts", "function")).toBe(true);
  });

  it("filters Files by file type", async () => {
    const { c, last } = setup({ findFiles: async () => [fm("a.rs"), fm("b.ts"), fm("c.md")] });
    const filters = { ...DEFAULT_FILTERS, files: { languages: ["rust"] } };
    c.run({
      query: "a",
      tab: "files",
      includeNonProject: false,
      filter: resolveFilter(filters, "files", lang),
      extra: {},
    });
    await flush();
    expect(
      last()
        .sections.get("files")!
        .items.map((i) => i.key),
    ).toEqual(["f:/p/b.ts", "f:/p/c.md"]);
  });

  it("filters Symbols by kind and language", async () => {
    const findSymbols = vi.fn(async () => [
      sym("run", "function", "a.rs"),
      sym("Run", "class", "b.ts"),
      sym("runner", "variable", "c.ts"),
    ]);
    const { c, last } = setup({ findSymbols });
    const filters = { ...DEFAULT_FILTERS, symbols: { languages: ["rust"], kinds: ["variable"] } };
    c.run({
      query: "run",
      tab: "symbols",
      includeNonProject: false,
      filter: resolveFilter(filters, "symbols", lang),
      extra: {},
    });
    await flush();
    expect(
      last()
        .sections.get("symbols")!
        .items.map((i) => (i.kind === "symbol" ? i.name : "")),
    ).toEqual(["Run"]);
    expect(findSymbols).toHaveBeenLastCalledWith("run", "all", 4 * 50 + 1);
  });

  it("passes the Text tab's file mask to the search", async () => {
    const searchText = vi.fn(async (_r: number, _q: SearchQuery) => {});
    const { c } = setup({ searchText });
    const filters = { ...DEFAULT_FILTERS, text: { mask: " *.ts, !*.test.* " } };
    c.run({
      query: "needle",
      tab: "text",
      includeNonProject: false,
      filter: resolveFilter(filters, "text", undefined),
      extra: {},
    });
    await flush();
    expect(searchText.mock.calls.at(-1)![1].include).toEqual(["*.ts, !*.test.*"]);
  });

  it("hides disabled actions unless included", async () => {
    const actions = [
      { id: "a.on", title: "Reformat Code", run: () => {} },
      { id: "a.off", title: "Reformat File", run: () => {} },
    ];
    const { c, last } = setup({ actions: () => actions, isEnabled: (a) => a.id === "a.on" });
    c.run({ query: "reformat", tab: "actions", includeNonProject: false, includeDisabled: false, extra: {} });
    await flush();
    expect(
      last()
        .sections.get("actions")!
        .items.map((i) => i.key),
    ).toEqual(["a:a.on"]);
    c.run({ query: "reformat", tab: "actions", includeNonProject: true, includeDisabled: true, extra: {} });
    await flush();
    expect(
      last()
        .sections.get("actions")!
        .items.map((i) => i.key),
    ).toEqual(["a:a.on", "a:a.off"]);
  });
});

describe("Settings in Search Everywhere", () => {
  const entries = [
    { pageId: "editor.general", title: "General", path: ["Editor"] },
    { pageId: "editor.general", title: "Show line numbers", path: ["Editor", "General"] },
    { pageId: "appearance", title: "Appearance", path: [], keywords: ["theme"] },
  ];

  it("matches option labels, page titles and keywords", () => {
    expect(matchSettings(entries, "line num").map((s) => s.title)).toEqual(["Show line numbers"]);
    expect(matchSettings(entries, "theme").map((s) => s.pageId)).toEqual(["appearance"]);
    expect(matchSettings(entries, "")).toEqual([]);
  });

  it("ranks settings below equally good actions", () => {
    const actions = matchActions([{ id: "x", title: "Show Line Numbers", run: () => {} }], "show line", {
      isEnabled: () => true,
      recent: [],
    });
    const merged = mergeActionResults(actions, matchSettings(entries, "show line"));
    expect(merged.map((i) => i.kind)).toEqual(["action", "setting"]);
  });

  it("lists settings with actions once the index answers", async () => {
    const { c, last } = setup({
      actions: () => [{ id: "x", title: "Toggle Line Numbers", run: () => {} }],
      settings: async () => entries,
    });
    c.run({ query: "line numbers", tab: "actions", includeNonProject: false, extra: {} });
    await flush();
    await flush();
    const items = last().sections.get("actions")!.items;
    expect(items.map((i) => i.kind)).toEqual(["action", "setting"]);
    expect(last().sections.get("actions")!.loading).toBe(false);
  });
});
