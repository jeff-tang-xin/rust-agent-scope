/** Installed browsers, the built-in server's URLs, and per-file preview state. */
import { create } from "zustand";
import { executeAction } from "@/platform/actions";
import { call, commands, isTauri, type Browser } from "@/platform/ipc";
import { hasIcon } from "@/ui";
import { notifications } from "@/workbench/contributions";

/** Files a browser can show (Open In › Browser). */
export const isWebPath = (path: string) => /\.(html?|xhtml|shtml|svg)$/i.test(path);
/** Files that get the browser bar and the built-in preview. */
export const isHtmlPath = (path: string) => /\.(html?|xhtml|shtml)$/i.test(path);

export const useBrowsers = create<{ list: Browser[] }>(() => ({ list: [] }));

export async function loadBrowsers(): Promise<void> {
  if (!isTauri()) return;
  try {
    useBrowsers.setState({ list: await call(commands.webBrowsers()) });
  } catch (e) {
    console.warn("web browsers", e);
  }
}

export const browserIcon = (id: string) => (hasIcon(`browser-${id}`) ? `browser-${id}` : "web");

/** Paths shown with the built-in preview beside the editor. */
export const useWebPreview = create<Record<string, boolean>>(() => ({}));

export function togglePreview(path: string, on = !useWebPreview.getState()[path]) {
  useWebPreview.setState({ [path]: on });
}

/** The built-in server's URL for `path`. */
export function webUrl(path: string): Promise<string> {
  return call(commands.webUrl(path));
}

/** Saves all documents (the server reads the disk), then opens `path` in `browser` or the default browser. */
export async function openInBrowser(path: string, browser: string | null = null): Promise<void> {
  try {
    await executeAction("file.save");
    await call(commands.webOpen(await webUrl(path), browser));
  } catch (e) {
    notifications().notify({
      level: "error",
      title: "Cannot open the browser",
      body: e instanceof Error ? e.message : String(e),
      key: "web-open",
    });
  }
}
