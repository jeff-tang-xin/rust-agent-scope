/** "problems" file decorations: files with errors (and their folders) get IDEA's red underline. */
import { setFileDecorations, type FileDecoration } from "@/workbench/contributions";
import { countSeverities, fileDiagnostics, useProblems, type ProblemsMap } from "./problemsStore";
import { tx } from "@/platform/i18n";

export function problemDecorations(map: ProblemsMap): Map<string, FileDecoration> {
  const out = new Map<string, FileDecoration>();
  for (const path of map.keys()) {
    const c = countSeverities(fileDiagnostics(map, path));
    if (!c.error) continue;
    const errors = c.error === 1 ? tx`${c.error} error` : tx`${c.error} errors`;
    const warnings = !c.warning ? "" : c.warning === 1 ? tx`${c.warning} warning` : tx`${c.warning} warnings`;
    out.set(path, { error: true, propagate: true, tooltip: warnings ? `${errors}, ${warnings}` : errors });
  }
  return out;
}

export function installProblemDecorations(): () => void {
  let scheduled = false;
  const push = () => {
    if (scheduled) return;
    scheduled = true;
    // Diagnostics often arrive in bursts (one event per file); coalesce.
    setTimeout(() => {
      scheduled = false;
      setFileDecorations("problems", problemDecorations(useProblems.getState().map));
    }, 50);
  };
  const unsub = useProblems.subscribe((s, prev) => {
    if (s.map !== prev.map) push();
  });
  return () => {
    unsub();
    setFileDecorations("problems", new Map());
  };
}
