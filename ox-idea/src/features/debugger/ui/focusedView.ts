/**
 * The debugger view (Frames or Variables) that has keyboard focus, so view-local
 * actions bound to global keys (F2 Set Value, F4 Jump to Source) reach it, like
 * IDEA's per-component shortcuts.
 */
import { setContext } from "@/platform/context";

export interface DebugViewCommands {
  view: "frames" | "variables";
  setValue?: () => void;
  jumpToSource?: () => void;
}

let current: DebugViewCommands | null = null;

export function focusDebugView(commands: DebugViewCommands) {
  current = commands;
  setContext("debugFramesFocus", commands.view === "frames");
  setContext("debugVariablesFocus", commands.view === "variables");
}

export function blurDebugView(view: DebugViewCommands["view"]) {
  if (current?.view !== view) return;
  current = null;
  setContext("debugFramesFocus", false);
  setContext("debugVariablesFocus", false);
}

export function focusedDebugView(): DebugViewCommands | null {
  return current;
}
