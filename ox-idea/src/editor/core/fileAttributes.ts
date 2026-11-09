/**
 * File › File Properties › Make File Read-only / Writable (IDEA's "Toggle
 * Read-only Attribute", also the status bar lock): asks, then flips the
 * file's writable bit and the open document's editability.
 */
import { call, commands, isTauri } from "@/platform/ipc";
import { basename } from "@/platform/project";
import { notifications } from "@/workbench/contributions";
import { confirm } from "@/workbench/messages";
import type { DocModel } from "./model/document";
import { tx } from "@/platform/i18n";

export function readOnlyQuestion(name: string, readOnly: boolean) {
  return readOnly
    ? { title: "Make File Writable", message: tx`'${name}' is read-only. Make it writable?`, confirm: "Make Writable" }
    : { title: "Make File Read-only", message: tx`Make '${name}' read-only?`, confirm: "Make Read-only" };
}

/** Applies the toggle without asking. Returns the new read-only state, or null on failure. */
export async function setFileWritable(model: DocModel, writable: boolean): Promise<boolean | null> {
  try {
    const readOnly = isTauri() ? await call(commands.fsSetWritable(model.path, writable)) : !writable;
    model.setReadOnly(readOnly);
    return readOnly;
  } catch (e) {
    notifications().notify({
      level: "error",
      title: `Cannot make ${basename(model.path)} ${writable ? "writable" : "read-only"}`,
      body: e instanceof Error ? e.message : String(e),
    });
    return null;
  }
}

export function toggleReadOnly(model: DocModel) {
  if (model.virtual) return;
  const meta = model.getMeta();
  if (meta.banner === "binary" || meta.banner === "tooLarge") return;
  const q = readOnlyQuestion(basename(model.path), meta.readOnly);
  void confirm({ title: q.title, message: q.message, ok: q.confirm }).then(
    (ok) => ok && void setFileWritable(model, meta.readOnly),
  );
}
