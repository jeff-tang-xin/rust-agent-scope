/** Chooser state and the recently used scratch languages (per user, browser storage). */
import { create } from "zustand";
import { pushRecent, type ScratchLanguage } from "./model";

const RECENT_KEY = "openidea.scratch.recentLanguages";

function readRecent(): string[] {
  try {
    const raw = localStorage.getItem(RECENT_KEY);
    const v: unknown = raw ? JSON.parse(raw) : [];
    return Array.isArray(v) ? v.filter((x): x is string => typeof x === "string") : [];
  } catch {
    return [];
  }
}

interface ScratchState {
  /** Languages of the open chooser; null when closed. */
  languages: ScratchLanguage[] | null;
  seq: number;
  recent: string[];
}

export const useScratch = create<ScratchState>(() => ({ languages: null, seq: 0, recent: readRecent() }));

export function openChooser(languages: ScratchLanguage[]) {
  useScratch.setState((s) => ({ languages, seq: s.seq + 1 }));
}

export function closeChooser() {
  useScratch.setState({ languages: null });
}

export function rememberLanguage(id: string) {
  const recent = pushRecent(useScratch.getState().recent, id);
  useScratch.setState({ recent });
  try {
    localStorage.setItem(RECENT_KEY, JSON.stringify(recent));
  } catch {
    /* not remembered */
  }
}
