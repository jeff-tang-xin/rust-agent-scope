/**
 * Toggle Line Breakpoint as IDEA does it: in Java, a method declaration line gets a
 * method breakpoint and a field declaration a field watchpoint; any other line (and
 * every other language) a line breakpoint. An existing breakpoint of any kind is removed.
 */
import { removeSpecial, specialAt, toggleFieldAt, toggleMethodAt } from "./special";
import { breakpointsOf, toggleBreakpoint } from "./store";
import { fieldAt, methodAt } from "./symbols";

export function toggleBreakpointSmart(path: string, line: number, language: string, text: string) {
  const special = specialAt(path).find((b) => b.line === line);
  if (special) {
    removeSpecial(special.id);
    return;
  }
  if (language === "java" && !breakpointsOf(path).some((b) => b.line === line)) {
    const field = fieldAt(language, text, line);
    if (field) {
      toggleFieldAt(path, line, field);
      return;
    }
    const method = methodAt(language, text, line);
    if (method) {
      toggleMethodAt(path, line, method.name);
      return;
    }
  }
  toggleBreakpoint(path, line);
}
