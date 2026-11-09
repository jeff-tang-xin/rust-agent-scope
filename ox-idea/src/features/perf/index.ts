/** Memory indicator, "Clear caches" and the dev performance overlay. */
import { registerActions } from "@/platform/actions";
import { registerOverlay, registerStatusItem } from "@/workbench/contributions";
import { perfActions } from "./actions";
import { MEMORY_WIDGET, MemoryStatus } from "./MemoryStatus";
import { PerfOverlay } from "./PerfOverlay";

export function activate(): () => void {
  const disposers = [
    registerActions(perfActions()),
    // Far right and hidden by default, like IDEA's memory indicator.
    registerStatusItem({ ...MEMORY_WIDGET, align: "right", priority: 100, component: MemoryStatus }),
    registerOverlay({ id: "perf.overlay", component: PerfOverlay }),
  ];
  return () => disposers.reverse().forEach((d) => d());
}
