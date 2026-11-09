/** Interval polling that pauses while the document is hidden. */

export interface PollOptions {
  intervalMs: number;
  /** Defaults to `document`; injectable for tests. */
  doc?: Pick<Document, "visibilityState" | "addEventListener" | "removeEventListener">;
}

/** Runs `tick` now and every `intervalMs` while visible; returns a stopper. */
export function startPolling(tick: () => void, { intervalMs, doc = document }: PollOptions): () => void {
  let timer: ReturnType<typeof setInterval> | undefined;
  const run = () => {
    if (timer === undefined) {
      tick();
      timer = setInterval(tick, intervalMs);
    }
  };
  const pause = () => {
    if (timer !== undefined) clearInterval(timer);
    timer = undefined;
  };
  const onVisibility = () => (doc.visibilityState === "hidden" ? pause() : run());
  doc.addEventListener("visibilitychange", onVisibility);
  if (doc.visibilityState !== "hidden") run();
  return () => {
    pause();
    doc.removeEventListener("visibilitychange", onVisibility);
  };
}
