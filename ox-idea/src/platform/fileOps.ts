/**
 * Files created / deleted by an explicit IDE action (New File, Copy, Delete,
 * file templates), as opposed to changes seen by the file watcher. VCS uses
 * this for Settings › Version Control › Confirmation.
 */
export interface FileOperation {
  kind: "created" | "deleted";
  paths: string[];
}

type Listener = (op: FileOperation) => void;
const listeners = new Set<Listener>();

export function notifyFileOperation(op: FileOperation) {
  for (const l of listeners) {
    try {
      l(op);
    } catch (e) {
      console.warn("file operation listener failed", e);
    }
  }
}

export function onFileOperation(listener: Listener): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}
