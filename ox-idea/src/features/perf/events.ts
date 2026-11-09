/**
 * Keystroke → paint latency from the Event Timing API: an `event` entry's
 * `duration` spans input timestamp to the next frame presented after its
 * handlers ran, which is exactly the "typed char visible" latency.
 */

/** Input events that make up typing in CodeMirror. */
export const TYPING_EVENTS: ReadonlySet<string> = new Set(["keydown", "keypress", "beforeinput", "input"]);

/** Frame budget (60 Hz) used for the "over budget" count. */
export const FRAME_BUDGET_MS = 16;

/** The subset of `PerformanceEventTiming` we read (structural, for tests). */
export interface EventEntryLike {
  name: string;
  duration: number;
  target?: unknown;
}

interface ClosestCapable {
  closest(selector: string): unknown;
}

function elementOf(target: unknown): ClosestCapable | null {
  if (!target || typeof target !== "object") return null;
  const t = target as { closest?: unknown; parentElement?: unknown };
  if (typeof t.closest === "function") return t as ClosestCapable;
  // Text nodes: use their parent element.
  const parent = t.parentElement as { closest?: unknown } | null | undefined;
  if (parent && typeof parent.closest === "function") return parent as ClosestCapable;
  return null;
}

/** Whether an event-timing entry is a keystroke in a CodeMirror editor. */
export function isEditorTypingEntry(entry: EventEntryLike): boolean {
  if (!TYPING_EVENTS.has(entry.name)) return false;
  const el = elementOf(entry.target);
  return !!el && !!el.closest(".cm-editor");
}

/**
 * Collapses the entries of one keystroke (keydown, beforeinput, input share an
 * interaction) into one sample: the longest duration per `interactionId`.
 * Entries without an id (0) are kept individually.
 */
export function keystrokeLatencies(entries: ReadonlyArray<EventEntryLike & { interactionId?: number }>): number[] {
  const byInteraction = new Map<number, number>();
  const loose: number[] = [];
  for (const e of entries) {
    if (!isEditorTypingEntry(e)) continue;
    const id = e.interactionId ?? 0;
    if (id === 0) loose.push(e.duration);
    else byInteraction.set(id, Math.max(byInteraction.get(id) ?? 0, e.duration));
  }
  return [...byInteraction.values(), ...loose];
}

/** Whether the browser's PerformanceObserver supports `type`. */
export function supportsEntryType(type: string): boolean {
  const PO = (globalThis as { PerformanceObserver?: { supportedEntryTypes?: readonly string[] } }).PerformanceObserver;
  return !!PO && Array.isArray(PO.supportedEntryTypes) && PO.supportedEntryTypes.includes(type);
}
