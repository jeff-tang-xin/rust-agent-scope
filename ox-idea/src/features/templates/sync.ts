/** Keeps template stores in step with edits saved by another window (the Settings window). */
import { FILE_TEMPLATES_KEY, reloadFileTemplates } from "./files/store";
import { LIVE_TEMPLATE_OPTIONS_KEY, LIVE_TEMPLATES_KEY, reloadLiveTemplates } from "./live/store";
import { POSTFIX_KEY, reloadPostfixSettings } from "./postfix/store";

const reloaders: Record<string, () => void> = {
  [LIVE_TEMPLATES_KEY]: reloadLiveTemplates,
  [LIVE_TEMPLATE_OPTIONS_KEY]: reloadLiveTemplates,
  [POSTFIX_KEY]: reloadPostfixSettings,
};

export function syncTemplatesFromStorage(): () => void {
  const onStorage = (e: StorageEvent) => {
    if (!e.key) return;
    // File templates also keep per-project schemes under keys extending FILE_TEMPLATES_KEY.
    if (e.key.startsWith(FILE_TEMPLATES_KEY)) reloadFileTemplates();
    else reloaders[e.key]?.();
  };
  window.addEventListener("storage", onStorage);
  return () => window.removeEventListener("storage", onStorage);
}
