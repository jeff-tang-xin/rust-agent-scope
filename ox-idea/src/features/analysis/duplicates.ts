/** Locate Duplicates: runs the backend clone detector over a scope. */
import { create } from "zustand";
import {
  call,
  commands,
  isCancelled,
  isTauri,
  nextRequestId,
  type DuplicateOptions,
  type DuplicatesReport,
  type RequestId,
} from "@/platform/ipc";
import { notifications, toolWindowService } from "@/workbench/contributions";
import { describeScope, resolveScope, type ScopeChoice } from "./scope";

const OPTIONS_KEY = "openidea.analysis.duplicates.v1";

export const DEFAULT_DUPLICATE_OPTIONS: DuplicateOptions = {
  minTokens: 60,
  anonymizeIdentifiers: true,
  anonymizeLiterals: true,
};

export function loadDuplicateOptions(): DuplicateOptions {
  try {
    const raw = globalThis.localStorage?.getItem(OPTIONS_KEY);
    if (raw) return { ...DEFAULT_DUPLICATE_OPTIONS, ...(JSON.parse(raw) as Partial<DuplicateOptions>) };
  } catch {
    // storage unavailable
  }
  return DEFAULT_DUPLICATE_OPTIONS;
}

function saveDuplicateOptions(o: DuplicateOptions) {
  try {
    globalThis.localStorage?.setItem(OPTIONS_KEY, JSON.stringify(o));
  } catch {
    // storage unavailable
  }
}

interface DuplicatesState {
  running: boolean;
  report: DuplicatesReport | null;
  scopeLabel: string | null;
  error: string | null;
  /** Index of the group shown in the preview. */
  selected: number | null;
  last: { choice: ScopeChoice; options: DuplicateOptions } | null;
}

const initialDuplicates = (): DuplicatesState => ({
  running: false,
  report: null,
  scopeLabel: null,
  error: null,
  selected: null,
  last: null,
});

export const useDuplicates = create<DuplicatesState>(initialDuplicates);

let inflight: RequestId | null = null;

export function cancelDuplicates() {
  if (inflight !== null) void commands.cancelRequest(inflight).catch(() => undefined);
}

/** The window switched projects: stops the run and forgets its report. */
export function resetDuplicates() {
  cancelDuplicates();
  inflight = null;
  useDuplicates.setState(initialDuplicates());
}

export async function locateDuplicates(choice: ScopeChoice, options: DuplicateOptions): Promise<void> {
  const scope = await resolveScope(choice);
  if (typeof scope === "string") {
    notifications().notify({ group: "analysis", level: "warning", title: "Locate Duplicates", body: scope });
    return;
  }
  saveDuplicateOptions(options);
  cancelDuplicates();
  const id = nextRequestId();
  inflight = id;
  useDuplicates.setState({
    running: true,
    report: null,
    error: null,
    selected: null,
    scopeLabel: describeScope(choice, scope),
    last: { choice, options },
  });
  toolWindowService().show("duplicates");
  if (!isTauri()) {
    useDuplicates.setState({ running: false, error: "Locate Duplicates needs the desktop app" });
    return;
  }
  try {
    const report = await call(commands.analysisLocateDuplicates(id, scope, options));
    if (inflight !== id) return;
    useDuplicates.setState({ running: false, report, selected: report.groups.length ? 0 : null });
  } catch (e) {
    if (inflight !== id) return;
    useDuplicates.setState({
      running: false,
      error: isCancelled(e) ? null : e instanceof Error ? e.message : String(e),
    });
  } finally {
    if (inflight === id) inflight = null;
  }
}

export function rerunDuplicates() {
  const last = useDuplicates.getState().last;
  if (last) void locateDuplicates(last.choice, last.options);
}

/** Source lines of a fragment with their 1-based numbers (the text starts at its first line). */
export function fragmentLines(text: string, firstLine: number): Array<{ no: number; text: string }> {
  return text.split(/\r\n?|\n/).map((t, i) => ({ no: firstLine + i + 1, text: t }));
}
