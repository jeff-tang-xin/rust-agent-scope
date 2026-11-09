import { useEffect, type RefObject } from "react";

/**
 * The editor area focuses an editor kind's host element (`[data-editor-focus]`);
 * hand that focus on to `target` so the viewer's keyboard handling and context
 * keys apply right away.
 */
export function useForwardFocus(target: RefObject<HTMLElement | null>) {
  useEffect(() => {
    const el = target.current;
    const host = el?.closest<HTMLElement>("[data-editor-focus]");
    if (!el || !host) return;
    const onFocus = (e: FocusEvent) => {
      if (e.target === host) el.focus({ preventScroll: true });
    };
    host.addEventListener("focus", onFocus);
    if (document.activeElement === host) el.focus({ preventScroll: true });
    return () => host.removeEventListener("focus", onFocus);
  }, [target]);
}
