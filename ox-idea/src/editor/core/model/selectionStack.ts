/**
 * Extend / Shrink Selection (⌥↑ / ⌥↓). Extending pushes the current range on
 * a stack and picks the innermost candidate strictly containing it; shrinking
 * pops back. The stack is only valid while the selection is the one the last
 * extend produced.
 */
export interface SimpleRange {
  from: number;
  to: number;
}

export interface SelectionStack {
  /** Ranges to restore on shrink, innermost last. */
  stack: SimpleRange[];
  /** The selection the last extend produced. */
  top: SimpleRange | null;
}

export const emptyStack = (): SelectionStack => ({ stack: [], top: null });

const same = (a: SimpleRange | null, b: SimpleRange) => !!a && a.from === b.from && a.to === b.to;

export function extendSelection(
  st: SelectionStack,
  current: SimpleRange,
  candidates: readonly SimpleRange[],
): { state: SelectionStack; next: SimpleRange | null } {
  const base = same(st.top, current) ? st : emptyStack();
  const next = [...candidates]
    .filter((r) => r.from <= current.from && r.to >= current.to && (r.from < current.from || r.to > current.to))
    .sort((a, b) => a.to - a.from - (b.to - b.from))[0];
  if (!next) return { state: base, next: null };
  return { state: { stack: [...base.stack, current], top: next }, next };
}

export function shrinkSelection(
  st: SelectionStack,
  current: SimpleRange,
): { state: SelectionStack; next: SimpleRange | null } {
  if (!same(st.top, current) || !st.stack.length) return { state: emptyStack(), next: null };
  const stack = st.stack.slice(0, -1);
  const next = st.stack[st.stack.length - 1]!;
  return { state: { stack, top: next }, next };
}
