/**
 * The settings search index for Search Everywhere: every page plus the
 * option labels of the built-in pages, with the page's breadcrumb.
 */
import { useSettingsPages, type SettingsSearchEntry } from "@/workbench/contributions";
import { loadBuiltinPages } from "./dialog/LazySettingsDialog";
import { breadcrumb, buildPageTree, flatten } from "./model/pages";

let cached: { pages: unknown; entries: SettingsSearchEntry[] } | null = null;

export async function settingsSearchEntries(): Promise<SettingsSearchEntry[]> {
  await loadBuiltinPages();
  const { optionLabels } = await import("./pages/registry");
  const pages = useSettingsPages.getState().pages;
  if (cached?.pages === pages) return cached.entries;
  const tree = buildPageTree(pages.values());
  const entries: SettingsSearchEntry[] = [];
  for (const node of flatten(tree)) {
    const page = node.page;
    if (!page) continue;
    const path = breadcrumb(tree, node.key);
    entries.push({ pageId: page.id, title: page.title, path: path.slice(0, -1), keywords: page.keywords });
    const seen = new Set<string>();
    for (const label of optionLabels(page.id)) {
      if (seen.has(label)) continue;
      seen.add(label);
      entries.push({ pageId: page.id, title: label, path });
    }
  }
  cached = { pages, entries };
  return entries;
}
