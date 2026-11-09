/** The expanded value hint: an object under the mouse as a variables tree (as IDEA). */
import { createRoot } from "react-dom/client";
import type { DebugSessionId, Variable } from "@/platform/ipc";
import { VariablesView } from "./VariablesView";
import styles from "./VariablesView.module.css";

export function mountValueTree(host: HTMLElement, expression: string, variable: Variable, session: DebugSessionId) {
  const root = createRoot(host);
  root.render(
    <div className={styles.hint}>
      <VariablesView
        roots={[{ key: "hint", kind: "eval", name: expression, variable, error: null }]}
        session={session}
        defaultExpanded={["hint"]}
        autoFocus
        aria-label={expression}
      />
    </div>,
  );
  return { unmount: () => queueMicrotask(() => root.unmount()) };
}
