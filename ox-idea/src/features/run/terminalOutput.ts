/**
 * Output of a run that emulates a terminal: raw terminal data kept (bounded) so
 * a console re-mounted later replays it, and forwarded to the mounted terminal.
 */
import type { OutputChunk } from "@/platform/ipc";

/** Replay buffer bound, in characters (the terminal keeps its own scrollback). */
const MAX_CHARS = 2_000_000;

const DIM = "\x1b[2m";
const RED = "\x1b[31m";
const RESET = "\x1b[0m";

/** What the terminal shows for a chunk: IDE messages dimmed, on their own lines. */
export function terminalText(chunk: OutputChunk): string {
  if (chunk.kind === "stdout") return chunk.text;
  const text = chunk.text.replace(/\r?\n/g, "\r\n");
  return chunk.kind === "system" ? `${DIM}${text}${RESET}` : `${RED}${text}${RESET}`;
}

export class TerminalOutput {
  private chunks: string[] = [];
  private size = 0;
  private sink: ((data: string) => void) | null = null;
  private onClear: (() => void) | null = null;

  append(chunk: OutputChunk) {
    const data = terminalText(chunk);
    if (!data) return;
    this.chunks.push(data);
    this.size += data.length;
    while (this.size > MAX_CHARS && this.chunks.length > 1) this.size -= this.chunks.shift()!.length;
    this.sink?.(data);
  }

  /** Attaches a terminal: it gets the output so far, then everything new. Returns the detach function. */
  attach(write: (data: string) => void, clear: () => void): () => void {
    for (const c of this.chunks) write(c);
    this.sink = write;
    this.onClear = clear;
    return () => {
      if (this.sink === write) {
        this.sink = null;
        this.onClear = null;
      }
    };
  }

  clear() {
    this.chunks = [];
    this.size = 0;
    this.onClear?.();
  }

  get text(): string {
    return this.chunks.join("");
  }
}
