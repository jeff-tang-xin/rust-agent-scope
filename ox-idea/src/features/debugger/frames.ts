/** Frame list presentation: library frames collapse into "… N library frames hidden". */
import type { StackFrame } from "@/platform/ipc";

const LIBRARY_PATH =
  /\/(node_modules|site-packages|dist-packages|\.cargo\/registry|\.rustup|rustc\/[0-9a-f]+|go\/pkg\/mod|\.m2\/repository|\.gradle\/caches)\/|^\/(usr|opt|Library|System)\/|<node_internals>/;

export function isLibraryFrame(frame: StackFrame, root: string | null): boolean {
  if (frame.presentationHint === "subtle" || frame.presentationHint === "deemphasize") return true;
  if (!frame.path) return true;
  if (LIBRARY_PATH.test(frame.path)) return true;
  return !!root && !frame.path.startsWith(root + "/");
}

export type FrameItem =
  | { kind: "frame"; frame: StackFrame; library: boolean }
  | { kind: "hidden"; count: number; key: string };

/**
 * Groups runs of library frames. The selected frame and the first frame are
 * always shown; `showAll` disables collapsing.
 */
export function groupFrames(
  frames: readonly StackFrame[],
  root: string | null,
  selected: number | null,
  showAll: boolean,
): FrameItem[] {
  const out: FrameItem[] = [];
  let run: StackFrame[] = [];
  const flush = () => {
    if (!run.length) return;
    if (run.length === 1) out.push({ kind: "frame", frame: run[0]!, library: true });
    else out.push({ kind: "hidden", count: run.length, key: `hidden:${run[0]!.id}` });
    run = [];
  };
  frames.forEach((f, i) => {
    const library = isLibraryFrame(f, root);
    if (!library || showAll || i === 0 || f.id === selected) {
      flush();
      out.push({ kind: "frame", frame: f, library });
    } else {
      run.push(f);
    }
  });
  flush();
  return out;
}

/** `create:22` — frame name (last segment) and 1-based line. */
export function frameLabel(f: StackFrame): string {
  return f.line >= 0 && f.path ? `${f.name}:${f.line + 1}` : f.name;
}
