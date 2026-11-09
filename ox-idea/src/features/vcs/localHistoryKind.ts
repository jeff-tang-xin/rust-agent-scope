/** Local History tab input; kept apart from `LocalHistory` so opening one does not load it. */
import { editorService, type EditorInput } from "@/workbench/contributions";

export const LOCAL_HISTORY_KIND = "localHistory";

export function localHistoryInput(path: string): EditorInput {
  return { kind: LOCAL_HISTORY_KIND, key: `localHistory:${path}`, path, data: { path } };
}

export function openLocalHistory(path: string) {
  void editorService().openInput(localHistoryInput(path), { focus: true });
}
