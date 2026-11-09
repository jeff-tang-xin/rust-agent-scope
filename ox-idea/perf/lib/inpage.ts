/**
 * Instrumentation injected before the app loads (`page.addInitScript`). It must
 * be self-contained: Playwright serialises the function source.
 *
 * Latency = input event timestamp → start of the first frame that shows its
 * effect (the requestAnimationFrame callback of that frame). Rendering that
 * frame follows within it; its main-thread cost is reported separately from
 * CDP counters. (A "message after rAF" end point was tried and dropped: Chrome
 * sometimes defers that task by a whole frame, which shows up as fake jank.)
 */

export interface InPage {
  /** keydown → next frame, per key while `recordKeys` is on. */
  keyLatencies: number[];
  recordKeys: boolean;
  /** Timestamp of the latest keydown / pointerdown / wheel / input event. */
  lastInput: number;
  frames: number[] | null;
  longTasks: Array<{ start: number; duration: number }>;
  afterNextPaint(): Promise<number>;
  startFrames(): void;
  stopFrames(): number[];
  /** Resolves with (frame start − first input after `arm`) once `selector` (and `text`) is rendered. */
  arm(): void;
  waitShown(selector: string, text: string | null, timeoutMs: number): Promise<number>;
}

export function installInPage() {
  const w = window as unknown as { __perf: InPage };
  if (w.__perf) return;
  let armedAt = -1;
  let firstInputAfterArm = -1;

  const afterNextPaint = () =>
    new Promise<number>((resolve) =>
      requestAnimationFrame(() => {
        const ch = new MessageChannel();
        ch.port1.onmessage = () => resolve(performance.now());
        ch.port2.postMessage(0);
      }),
    );

  const onInput = (e: Event) => {
    const t = e.timeStamp;
    h.lastInput = t;
    if (armedAt >= 0 && firstInputAfterArm < 0) firstInputAfterArm = t;
    if (e.type === "keydown" && h.recordKeys) requestAnimationFrame(() => h.keyLatencies.push(performance.now() - t));
  };
  // `input` covers `keyboard.insertText`, which dispatches no keydown.
  for (const type of ["keydown", "pointerdown", "wheel", "input"]) {
    window.addEventListener(type, onInput, { capture: true, passive: true });
  }

  const h: InPage = {
    keyLatencies: [],
    recordKeys: false,
    lastInput: 0,
    frames: null,
    longTasks: [],
    afterNextPaint,
    startFrames() {
      const frames: number[] = [];
      h.frames = frames;
      let prev = -1;
      const tick = (t: number) => {
        if (h.frames !== frames) return;
        if (prev >= 0) frames.push(t - prev);
        prev = t;
        requestAnimationFrame(tick);
      };
      requestAnimationFrame(tick);
    },
    stopFrames() {
      const f = h.frames ?? [];
      h.frames = null;
      return f;
    },
    arm() {
      armedAt = performance.now();
      firstInputAfterArm = -1;
    },
    waitShown(selector, text, timeoutMs) {
      const deadline = performance.now() + timeoutMs;
      const matches = () => {
        const el = document.querySelector(selector) as HTMLElement | null;
        if (!el || el.getClientRects().length === 0) return false;
        return text === null || (el.textContent ?? "").includes(text);
      };
      return new Promise<number>((resolve, reject) => {
        const tick = () => {
          // Checked at the start of a frame: a match here is painted by this frame.
          if (matches()) {
            const start = firstInputAfterArm >= 0 ? firstInputAfterArm : armedAt;
            armedAt = -1;
            resolve(performance.now() - start);
          } else if (performance.now() > deadline) {
            armedAt = -1;
            reject(new Error(`timed out waiting for ${selector}${text ? ` containing "${text}"` : ""}`));
          } else requestAnimationFrame(tick);
        };
        requestAnimationFrame(tick);
      });
    },
  };
  w.__perf = h;

  try {
    new PerformanceObserver((list) => {
      for (const e of list.getEntries()) h.longTasks.push({ start: e.startTime, duration: e.duration });
    }).observe({ type: "longtask", buffered: true });
  } catch {
    // Long Tasks API unavailable: TBT is reported as 0.
  }
}
