/** Actions of the perf feature. No default shortcuts (keymaps live in platform). */
import type { Action } from "@/platform/actions";
import { notifications } from "@/workbench/contributions";
import { isStatusItemVisible, setStatusWidgetVisible } from "@/workbench/statusBarWidgets";
import { useProject } from "@/platform/project";
import { formatSize } from "./format";
import { MEMORY_WIDGET } from "./MemoryStatus";
import { clearCaches, refreshMetrics, usePerf } from "./store";
import { tx } from "@/platform/i18n";

function memoryIndicatorShown(): boolean {
  const overrides = useProject.getState().settings?.appearance?.statusBarWidgets;
  return isStatusItemVisible(MEMORY_WIDGET, overrides);
}

export function perfActions(): Action[] {
  return [
    {
      id: "perf.clearCaches",
      title: "Clear Caches",
      category: "Help",
      description: "Drop syntax trees and EditorConfig caches, compact the index",
      icon: "trash",
      keywords: ["memory", "invalidate", "index"],
      run: async () => {
        try {
          const r = await clearCaches();
          if (!r) {
            notifications().notify({ level: "info", title: "No backend caches to clear", key: "perf.clearCaches" });
            return;
          }
          const memory =
            r.rssBeforeBytes != null && r.rssAfterBytes != null
              ? tx` Memory ${formatSize(r.rssBeforeBytes)} → ${formatSize(r.rssAfterBytes)}.`
              : "";
          notifications().notify({
            level: "info",
            title: "Caches cleared",
            body:
              r.syntaxTrees === 1
                ? tx`${r.syntaxTrees} syntax tree dropped, index compacted.${memory}`
                : tx`${r.syntaxTrees} syntax trees dropped, index compacted.${memory}`,
            key: "perf.clearCaches",
          });
        } catch (e) {
          notifications().notify({
            level: "error",
            title: "Could not clear caches",
            body: e instanceof Error ? e.message : String(e),
            key: "perf.clearCaches",
          });
        }
      },
    },
    {
      id: "perf.showMemory",
      title: "Show Memory Indicator Details",
      category: "Help",
      icon: "info",
      keywords: ["memory", "diagnostics", "metrics"],
      run: async () => {
        if (!memoryIndicatorShown()) await setStatusWidgetVisible(MEMORY_WIDGET.id, true);
        await refreshMetrics();
        usePerf.setState({ popupOpen: true });
      },
    },
    {
      id: "perf.toggleMemoryIndicator",
      title: "Memory Indicator",
      category: "View",
      description: "Show or hide the memory indicator in the status bar",
      keywords: ["memory", "status bar", "widget", "heap"],
      toggled: () => memoryIndicatorShown(),
      run: () => setStatusWidgetVisible(MEMORY_WIDGET.id, !memoryIndicatorShown()),
    },
    {
      id: "perf.toggleOverlay",
      title: "Toggle Performance Overlay (Dev)",
      category: "Help",
      description: "Keystroke-to-paint latency, IPC round trips and long tasks",
      keywords: ["latency", "fps", "diagnostics", "typing"],
      run: () => usePerf.setState((s) => ({ overlayOpen: !s.overlayOpen })),
    },
  ];
}
