/** Rolling window of latency samples with percentile summaries. */

export interface Summary {
  count: number;
  last: number | null;
  p50: number | null;
  p95: number | null;
  max: number | null;
  /** Samples above the budget passed to `summary`. */
  overBudget: number;
}

/** Nearest-rank percentile of `sorted` (ascending); `null` when empty. */
export function percentile(sorted: readonly number[], p: number): number | null {
  if (sorted.length === 0) return null;
  const rank = Math.ceil((Math.min(100, Math.max(0, p)) / 100) * sorted.length);
  return sorted[Math.max(0, rank - 1)] ?? null;
}

export class RollingWindow {
  private buf: number[] = [];
  private next = 0;
  private total = 0;
  private lastValue: number | null = null;

  constructor(readonly capacity = 200) {}

  push(value: number) {
    if (!Number.isFinite(value)) return;
    this.total++;
    this.lastValue = value;
    if (this.buf.length < this.capacity) {
      this.buf.push(value);
    } else {
      this.buf[this.next] = value;
    }
    this.next = (this.next + 1) % this.capacity;
  }

  /** Samples ever pushed (not just those retained). */
  get pushed(): number {
    return this.total;
  }

  get size(): number {
    return this.buf.length;
  }

  last(): number | null {
    return this.lastValue;
  }

  values(): number[] {
    return [...this.buf];
  }

  clear() {
    this.buf = [];
    this.next = 0;
    this.total = 0;
    this.lastValue = null;
  }

  summary(budget = Infinity): Summary {
    const sorted = [...this.buf].sort((a, b) => a - b);
    return {
      count: sorted.length,
      last: this.last(),
      p50: percentile(sorted, 50),
      p95: percentile(sorted, 95),
      max: sorted[sorted.length - 1] ?? null,
      overBudget: sorted.filter((v) => v > budget).length,
    };
  }
}
