import { toolWindowService, type ToolWindowService } from "@/workbench/contributions";

/** The tool window service, or undefined before the workbench provided it (tests). */
export function maybeToolWindowService(): ToolWindowService | undefined {
  try {
    return toolWindowService();
  } catch {
    return undefined;
  }
}
