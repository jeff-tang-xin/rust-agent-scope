/** Dev diagnostics: keystroke → paint latency, IPC round trips, long tasks. */
import { useEffect, useRef, useState } from "react";
import { IconButton, T } from "@/ui";
import { isTauri } from "@/platform/ipc";
import { formatMs } from "./format";
import { FRAME_BUDGET_MS, keystrokeLatencies, supportsEntryType, type EventEntryLike } from "./events";
import { RollingWindow, type Summary } from "./stats";
import { refreshMetrics, usePerf } from "./store";
import styles from "./perf.module.css";
import { tx } from "@/platform/i18n";

const IPC_PING_MS = 2000;
const REPAINT_MS = 500;

export function PerfOverlay() {
  const open = usePerf((s) => s.overlayOpen);
  if (!open) return null;
  return <PerfPanel onClose={() => usePerf.setState({ overlayOpen: false })} />;
}

interface Samples {
  keys: Summary;
  ipc: Summary;
  longTasks: Summary;
}

function PerfPanel({ onClose }: { onClose: () => void }) {
  const windows = useRef({
    keys: new RollingWindow(200),
    ipc: new RollingWindow(100),
    longTasks: new RollingWindow(100),
  });
  const [samples, setSamples] = useState<Samples | null>(null);
  const eventTiming = supportsEntryType("event");
  const longTasks = supportsEntryType("longtask");

  useEffect(() => {
    const w = windows.current;
    const observers: PerformanceObserver[] = [];
    if (eventTiming) {
      const po = new PerformanceObserver((list) => {
        const entries = list.getEntries() as unknown as Array<EventEntryLike & { interactionId?: number }>;
        for (const v of keystrokeLatencies(entries)) w.keys.push(v);
      });
      // `durationThreshold` (min 16 ms) keeps the observer cheap while typing.
      po.observe({ type: "event", buffered: true, durationThreshold: 16 } as PerformanceObserverInit);
      observers.push(po);
    }
    if (longTasks) {
      const po = new PerformanceObserver((list) => list.getEntries().forEach((e) => w.longTasks.push(e.duration)));
      po.observe({ type: "longtask", buffered: true });
      observers.push(po);
    }
    const ping = isTauri()
      ? setInterval(() => void refreshMetrics().then((rtt) => rtt != null && w.ipc.push(rtt)), IPC_PING_MS)
      : undefined;
    const repaint = setInterval(
      () =>
        setSamples({
          keys: w.keys.summary(FRAME_BUDGET_MS),
          ipc: w.ipc.summary(FRAME_BUDGET_MS),
          longTasks: w.longTasks.summary(50),
        }),
      REPAINT_MS,
    );
    return () => {
      observers.forEach((o) => o.disconnect());
      if (ping) clearInterval(ping);
      clearInterval(repaint);
    };
  }, [eventTiming, longTasks]);

  return (
    <section className={styles.overlay} aria-label={tx("Performance overlay (dev diagnostics)")}>
      <div className={styles.overlayHeader}>
        <span className={styles.overlayTitle}>
          <T>Performance (dev diagnostics)</T>
        </span>
        <IconButton icon="close" size="small" title="Close performance overlay" onClick={onClose} />
      </div>
      <Block
        title="Keystroke → paint"
        summary={samples?.keys}
        budget={`>${FRAME_BUDGET_MS} ms`}
        note={eventTiming ? "Only events ≥ 16 ms are reported by the browser." : "Event Timing API not supported here."}
      />
      <Block
        title="IPC round trip"
        summary={samples?.ipc}
        budget={`>${FRAME_BUDGET_MS} ms`}
        note={isTauri() ? "Sampled app_metrics ping every 2 s." : "No backend (browser mode)."}
      />
      {longTasks && <Block title="Long tasks" summary={samples?.longTasks} budget=">50 ms" />}
    </section>
  );
}

function Block({ title, summary, budget, note }: { title: string; summary?: Summary; budget: string; note?: string }) {
  const s = summary;
  return (
    <div className={styles.section}>
      <div className={styles.row}>
        <span className={styles.label}>{title}</span>
        <span className={styles.value}>{s ? `${s.count} samples` : "–"}</span>
      </div>
      <div className={styles.row}>
        <span className={styles.label}>
          <T>last · p50 · p95 · max</T>
        </span>
        <span className={styles.value}>{s ? [s.last, s.p50, s.p95, s.max].map(formatMs).join(" · ") : "–"}</span>
      </div>
      <div className={styles.row}>
        <span className={styles.label}>{tx`Over budget (${budget})`}</span>
        <span className={s && s.overBudget > 0 ? `${styles.value} ${styles.warn}` : styles.value}>
          {s ? s.overBudget : "–"}
        </span>
      </div>
      {note && <div className={styles.note}>{note}</div>}
    </div>
  );
}
