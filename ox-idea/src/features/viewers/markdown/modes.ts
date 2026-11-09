/** Markdown editor layout per file: Editor only / Editor and Preview / Preview only (IDEA's split editor). */
import { create } from "zustand";

export type MarkdownMode = "editor" | "split" | "preview";

const DEFAULT_KEY = "openidea.markdown.defaultMode";
const MODES: readonly MarkdownMode[] = ["editor", "split", "preview"];

function storedDefault(): MarkdownMode {
  try {
    const v = localStorage.getItem(DEFAULT_KEY);
    return MODES.includes(v as MarkdownMode) ? (v as MarkdownMode) : "split";
  } catch {
    return "split";
  }
}

interface ModesState {
  byPath: Record<string, MarkdownMode>;
  /** Mode of files not switched yet: the last one chosen. */
  fallback: MarkdownMode;
}

export const useMarkdownModes = create<ModesState>(() => ({ byPath: {}, fallback: storedDefault() }));

export const modeOf = (s: ModesState, path: string): MarkdownMode => s.byPath[path] ?? s.fallback;

export function setMarkdownMode(path: string, mode: MarkdownMode) {
  useMarkdownModes.setState((s) => ({ byPath: { ...s.byPath, [path]: mode }, fallback: mode }));
  try {
    localStorage.setItem(DEFAULT_KEY, mode);
  } catch {
    // Per-viewer convenience only.
  }
}
