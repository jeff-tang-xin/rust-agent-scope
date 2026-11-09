/** Metrics state shared by the status item, its popup and the dev overlay. */
import { create } from "zustand";
import { call, commands, isTauri, type AppMetrics, type ClearCachesResult } from "@/platform/ipc";

interface PerfState {
  metrics: AppMetrics | null;
  /** Last metrics fetch failed (shown in the popup). */
  error: string | null;
  popupOpen: boolean;
  overlayOpen: boolean;
}

export const usePerf = create<PerfState>(() => ({ metrics: null, error: null, popupOpen: false, overlayOpen: false }));

/** Poll interval of the memory indicator. */
export const METRICS_INTERVAL_MS = 5000;

/** Fetches `app_metrics` into the store; returns the round-trip time in ms. */
export async function refreshMetrics(): Promise<number | null> {
  if (!isTauri()) return null;
  const start = performance.now();
  try {
    const metrics = await call(commands.appMetrics());
    const rtt = performance.now() - start;
    usePerf.setState({ metrics, error: null });
    return rtt;
  } catch (e) {
    usePerf.setState({ error: e instanceof Error ? e.message : String(e) });
    return null;
  }
}

export async function clearCaches(): Promise<ClearCachesResult | null> {
  if (!isTauri()) return null;
  const result = await call(commands.clearCaches());
  void refreshMetrics();
  return result;
}
