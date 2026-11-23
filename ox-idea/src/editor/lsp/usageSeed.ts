/** One-shot seed for the Show Usages popup. The usages lens already counted
 * these locations (source-only scope), so clicking a lens shows them instantly
 * as provisional results while the full-scope search refreshes and corrects.
 * Types-only dependencies so the editor layer can set it without importing the
 * feature layer (avoids an editor → feature → workbench import cycle). */
import type { Location } from "@/bindings";

let seed: { symbol: string; locations: Location[] } | null = null;

/** Stash the lens's cached locations for the next Show Usages invocation. */
export function setUsageSeed(symbol: string, locations: readonly Location[]): void {
  seed = { symbol, locations: [...locations] };
}

/** Consume the pending seed (clears it); null when none was set. */
export function takeUsageSeed(): { symbol: string; locations: Location[] } | null {
  const s = seed;
  seed = null;
  return s;
}
