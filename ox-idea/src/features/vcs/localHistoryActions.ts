/** Local History actions beyond Show History: Put Label… */
import { registerActions } from "@/platform/actions";
import { call, commands } from "@/platform/ipc";
import { notifications } from "@/workbench/contributions";
import { promptText } from "./dialogs";
import { errorMessage } from "./ops";
import { showRecentChanges } from "./recentChanges";

/** Local History › Put Label…: names the current state of every file in the project. */
export async function putLabel(): Promise<boolean> {
  const r = await promptText({
    title: "Put Label",
    label: "Label name:",
    okLabel: "OK",
    validate: (v) => (v.trim() ? null : "Enter a label name"),
  });
  if (!r) return false;
  try {
    await call(commands.historyPutLabel(r.value));
    notifications().notify({
      level: "info",
      title: `Label '${r.value}' added to Local History`,
      key: "localHistory.label",
    });
    return true;
  } catch (e) {
    notifications().notify({ level: "error", title: "Cannot put label", body: errorMessage(e) });
    return false;
  }
}

export function registerLocalHistoryActions(): () => void {
  return registerActions([
    {
      id: "localHistory.putLabel",
      title: "Local History: Put Label…",
      category: "Git",
      icon: "bookmark",
      when: "projectOpen",
      keywords: ["local history", "label", "checkpoint"],
      run: () => void putLabel(),
    },
    {
      id: "localHistory.recentChanges",
      title: "Recent Changes",
      category: "View",
      icon: "history",
      when: "projectOpen",
      keywords: ["local history", "recent changes", "activity"],
      run: () => void showRecentChanges(),
    },
  ]);
}
