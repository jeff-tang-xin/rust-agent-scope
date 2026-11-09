import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { EmptyState } from "@/ui";
import { editorController, useTabErrors } from "../controller";
import type { EditorSlot } from "../model/slot";
import styles from "./EditorArea.module.css";

/** Mounts the CodeMirror view of one tab while it is visible. */
export function TextEditorHost({ tabId }: { tabId: string }) {
  const ctrl = editorController();
  const ref = useRef<HTMLDivElement>(null);
  const [slot, setSlot] = useState<EditorSlot | null>(() => ctrl.slotFor(tabId));
  const error = useTabErrors((s) => s[tabId]);

  useEffect(() => {
    if (slot && !slot.disposed) return;
    let live = true;
    void ctrl.ensureSlot(tabId).then((s) => {
      if (live && s) setSlot(s);
    });
    return () => {
      live = false;
    };
  }, [ctrl, tabId, slot]);

  useLayoutEffect(() => {
    if (!slot || slot.disposed || !ref.current) return;
    slot.mount(ref.current);
    ctrl.onMounted(slot);
    return () => slot.unmount();
  }, [ctrl, slot]);

  if (error)
    return (
      <div className={styles.host}>
        <EmptyState icon="warning" title="Cannot open file">
          {error}
        </EmptyState>
      </div>
    );
  return <div ref={ref} className={styles.host} data-editor-host="" aria-busy={!slot || undefined} />;
}
