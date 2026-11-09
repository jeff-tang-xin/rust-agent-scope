/** Merge editor tab input; kept apart from `MergeEditor` so opening one does not load it. */
import type { EditorInput } from "@/workbench/contributions";

export const MERGE_KIND = "merge";

export interface MergeData {
  path: string;
}

export function mergeInput(path: string): EditorInput {
  return { kind: MERGE_KIND, key: `merge:${path}`, path, data: { path } satisfies MergeData };
}
