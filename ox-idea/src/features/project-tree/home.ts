import { create } from "zustand";
import { isTauri } from "@/platform/ipc";

/** The user's home directory (for `~` paths); null until known or outside Tauri. */
export const useHomeDir = create<{ home: string | null }>(() => ({ home: null }));

let requested = false;

export function loadHomeDir() {
  if (requested || !isTauri()) return;
  requested = true;
  import("@tauri-apps/api/path")
    .then(({ homeDir }) => homeDir())
    .then((home) => useHomeDir.setState({ home: home.replace(/\/+$/, "") || null }))
    .catch(() => {
      requested = false;
    });
}
