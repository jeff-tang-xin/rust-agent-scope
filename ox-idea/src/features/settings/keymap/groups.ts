/**
 * Where an action sits in the Keymap tree, mirroring IDEA's top-level groups:
 * Main Menu › File / Edit / View / Navigate / Code / Refactor / Run / Tools /
 * Git / Window / Help, Editor Actions, Tool Windows, Other, Plugins. Derived
 * from the action's category (and the `plugin.` id prefix of plugin commands).
 */
import type { Action } from "@/platform/actions";

export const MAIN_MENU = "Main Menu";
export const EDITOR_ACTIONS = "Editor Actions";
export const TOOL_WINDOWS = "Tool Windows";
export const OTHER = "Other";
export const PLUGINS = "Plugins";

/** Top-level groups in display order. */
export const ROOT_ORDER = [MAIN_MENU, EDITOR_ACTIONS, TOOL_WINDOWS, OTHER, PLUGINS] as const;

/** Main menu submenus in menu-bar order. */
export const MAIN_MENU_ORDER = [
  "File",
  "Edit",
  "View",
  "Navigate",
  "Code",
  "Refactor",
  "Run",
  "Tools",
  "Git",
  "Window",
  "Help",
] as const;

/** Category → path below the root group. */
const CATEGORY_PATHS: Record<string, string[]> = {
  File: [MAIN_MENU, "File"],
  Edit: [MAIN_MENU, "Edit"],
  Find: [MAIN_MENU, "Edit", "Find"],
  View: [MAIN_MENU, "View"],
  Navigate: [MAIN_MENU, "Navigate"],
  Bookmarks: [MAIN_MENU, "Navigate", "Bookmarks"],
  Code: [MAIN_MENU, "Code"],
  Folding: [MAIN_MENU, "Code", "Folding"],
  "Code Completion": [MAIN_MENU, "Code", "Code Completion"],
  Refactor: [MAIN_MENU, "Refactor"],
  Run: [MAIN_MENU, "Run"],
  Debug: [MAIN_MENU, "Run", "Debugging Actions"],
  Test: [MAIN_MENU, "Run", "Testing"],
  Tools: [MAIN_MENU, "Tools"],
  Git: [MAIN_MENU, "Git"],
  VCS: [MAIN_MENU, "Git"],
  Window: [MAIN_MENU, "Window"],
  Help: [MAIN_MENU, "Help"],
  Editor: [EDITOR_ACTIONS],
  "Editor Actions": [EDITOR_ACTIONS],
  "Tool Windows": [TOOL_WINDOWS],
  // IDEA lists the bundled Terminal plugin's actions under Plugins.
  Terminal: [PLUGINS, "Terminal"],
  Plugins: [PLUGINS],
  "Project View": [OTHER, "Project View Popup Menu"],
};

/** Group path of an action, e.g. `["Main Menu", "Edit", "Find"]`. */
export function groupPath(action: Pick<Action, "id" | "category">): string[] {
  if (action.id.startsWith("plugin.")) return [PLUGINS, action.category || OTHER];
  const cat = action.category?.trim();
  if (!cat) return action.id.startsWith("editor.") ? [EDITOR_ACTIONS] : [OTHER];
  return CATEGORY_PATHS[cat] ?? [OTHER, cat];
}

/** Sort rank of a group among its siblings (lower first; unknown groups alphabetically last). */
export function groupRank(parentPath: readonly string[], name: string): number {
  const order: readonly string[] =
    parentPath.length === 0
      ? ROOT_ORDER
      : parentPath.length === 1 && parentPath[0] === MAIN_MENU
        ? MAIN_MENU_ORDER
        : [];
  const i = order.indexOf(name);
  return i < 0 ? order.length : i;
}

/** Icon of a group row, like IDEA's keymap tree. */
export function groupIcon(path: readonly string[], expanded: boolean): string {
  if (path.length === 1) {
    if (path[0] === MAIN_MENU) return "list";
    if (path[0] === EDITOR_ACTIONS) return "edit";
    if (path[0] === TOOL_WINDOWS) return "layout";
    if (path[0] === PLUGINS) return "plugin";
  }
  return expanded ? "folder-open" : "folder";
}
