/**
 * Browser tool window: the headless Chromium the agent's browser tools drive,
 * streamed live (screencast frames) with input forwarded back. Sharing the
 * process-wide session means the page you see is the page the agent works on.
 */
import { registerToolWindow } from "@/workbench/contributions";
import { lazyComponent } from "@/ui";

export function activate(): () => void {
  const dispose = registerToolWindow({
    id: "browser",
    title: "Browser",
    icon: "web",
    anchor: "right",
    order: 1,
    component: lazyComponent(() => import("./BrowserView").then((m) => m.BrowserView)),
    // The browser can be driven without a project open.
    requiresProject: false,
  });
  return dispose;
}
