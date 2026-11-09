/**
 * File › Invalidate Caches…: IDEA's dialog. The symbol index is always
 * rebuilt; Local History, language server workspaces and build tool caches
 * optionally. Caches in use are deleted at the next start, so the choice is
 * scheduled and the IDE restarts.
 */
import { useState } from "react";
import { create } from "zustand";
import type { InvalidateCachesOptions } from "@/bindings";
import { registerActions } from "@/platform/actions";
import { call, commands, isTauri } from "@/platform/ipc";
import { Checkbox, Dialog, T } from "@/ui";
import { notifications } from "@/workbench/contributions";
import styles from "./app.module.css";

const useOpen = create<{ open: boolean }>(() => ({ open: false }));

export function openInvalidateCaches() {
  useOpen.setState({ open: true });
}

const close = () => useOpen.setState({ open: false });

export const DEFAULT_INVALIDATE_OPTIONS: Required<InvalidateCachesOptions> = {
  localHistory: false,
  languageServers: false,
  buildCaches: false,
};

/** Schedules the invalidation (or none, for Just Restart) and restarts. */
export async function invalidateAndRestart(options: InvalidateCachesOptions | null): Promise<void> {
  if (!isTauri()) {
    notifications().notify({
      level: "info",
      title: "Restart is only available in the desktop app",
      key: "app.invalidateCaches",
    });
    return;
  }
  try {
    await call(commands.cachesScheduleInvalidation(options));
    await commands.appRestart();
  } catch (e) {
    notifications().notify({
      level: "error",
      title: "Could not invalidate caches",
      body: e instanceof Error ? e.message : String(e),
      key: "app.invalidateCaches",
    });
  }
}

export function InvalidateCachesDialog({ onClose = close }: { onClose?: () => void }) {
  const [options, setOptions] = useState(DEFAULT_INVALIDATE_OPTIONS);
  const set = (key: keyof InvalidateCachesOptions) => (on: boolean) => setOptions((o) => ({ ...o, [key]: on }));
  return (
    <Dialog
      open
      title="Invalidate Caches"
      onClose={onClose}
      width={480}
      buttons={[
        {
          label: "Invalidate and Restart",
          role: "default",
          autoFocus: true,
          onClick: () => {
            onClose();
            void invalidateAndRestart(options);
          },
        },
        {
          label: "Just Restart",
          role: "extra",
          onClick: () => {
            onClose();
            void invalidateAndRestart(null);
          },
        },
        { label: "Cancel", role: "cancel" },
      ]}
    >
      <div className={styles.invalidateBody}>
        <p className={styles.invalidateText}>
          <T>
            {
              "The caches will be invalidated and rebuilt on the next startup: the symbol index of every project and the syntax caches."
            }
          </T>
        </p>
        <div className={styles.invalidateHeading}>
          <T>Optional:</T>
        </div>
        <Checkbox
          label="Clear file system cache and Local History"
          checked={options.localHistory}
          onChange={set("localHistory")}
        />
        <Checkbox
          label="Clear language server workspaces (Java import data and indexes)"
          checked={options.languageServers}
          onChange={set("languageServers")}
        />
        <Checkbox
          label="Clear build tool caches (Maven / Gradle sync results)"
          checked={options.buildCaches}
          onChange={set("buildCaches")}
        />
        {options.localHistory && (
          <p className={styles.invalidateWarning} role="note">
            <T>Local History of all projects will be deleted.</T>
          </p>
        )}
      </div>
    </Dialog>
  );
}

export function InvalidateCachesOverlay() {
  const open = useOpen((s) => s.open);
  return open ? <InvalidateCachesDialog /> : null;
}

export function registerInvalidateCachesActions(): () => void {
  return registerActions([
    {
      id: "file.invalidateCaches",
      title: "Invalidate Caches…",
      category: "File",
      icon: "refresh",
      keywords: ["clear caches", "repair", "index", "rebuild", "restart"],
      run: openInvalidateCaches,
    },
    {
      id: "app.restart",
      title: "Restart IDE",
      category: "File",
      keywords: ["relaunch"],
      run: () => invalidateAndRestart(null),
    },
  ]);
}
