/** Summaries of latency / frame samples. */

/** Nearest-rank percentile; NaN when empty. */
export function percentile(values: readonly number[], p: number): number {
  if (values.length === 0) return NaN;
  const sorted = [...values].sort((a, b) => a - b);
  const rank = Math.ceil((p / 100) * sorted.length);
  return sorted[Math.min(sorted.length - 1, Math.max(0, rank - 1))]!;
}

export const median = (values: readonly number[]) => percentile(values, 50);

/** `p50Ms` / `p95Ms` / `maxMs`, or `<prefix>P50Ms` … with a prefix. */
export function latencySummary(samples: readonly number[], prefix = "") {
  const key = (k: string) => (prefix ? `${prefix}${k[0]!.toUpperCase()}${k.slice(1)}` : k);
  return {
    [key("p50Ms")]: percentile(samples, 50),
    [key("p95Ms")]: percentile(samples, 95),
    [key("maxMs")]: samples.length ? Math.max(...samples) : NaN,
  };
}

/**
 * Frame pacing while something animates or scrolls. `refreshMs` is the display
 * interval measured at idle; a frame that took k intervals dropped k − 1 frames.
 */
export function frameSummary(frames: readonly number[], refreshMs: number) {
  let dropped = 0;
  let expected = 0;
  for (const f of frames) {
    const n = Math.max(1, Math.round(f / refreshMs));
    expected += n;
    dropped += n - 1;
  }
  return {
    frameP95Ms: percentile(frames, 95),
    frameMaxMs: frames.length ? Math.max(...frames) : NaN,
    droppedFramesPct: expected ? (100 * dropped) / expected : 0,
  };
}

/**
 * Keystroke summary: keydown → next frame percentiles and the share of keys
 * whose frame came more than one refresh interval late (a missed frame).
 */
export function keySummary(samples: readonly number[], refreshMs: number, prefix = "") {
  const key = (k: string) => (prefix ? `${prefix}${k[0]!.toUpperCase()}${k.slice(1)}` : k);
  const missed = samples.filter((s) => s > refreshMs + 2).length;
  return {
    ...latencySummary(samples, prefix),
    [key("missedFramePct")]: samples.length ? (100 * missed) / samples.length : NaN,
  };
}

/** Total blocking time: the part of each long task beyond 50 ms. */
export function totalBlockingTime(tasks: ReadonlyArray<{ duration: number }>): number {
  return tasks.reduce((sum, t) => sum + Math.max(0, t.duration - 50), 0);
}
