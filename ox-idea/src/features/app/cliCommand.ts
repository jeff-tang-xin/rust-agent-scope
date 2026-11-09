/** Install / Uninstall the `oxidea` shell command. */
import type { Action } from "@/platform/actions";
import { tx } from "@/platform/i18n";
import { call, commands, isCancelled } from "@/platform/ipc";
import { notifications } from "@/workbench/contributions";

const KEY = "app.cli";

function errorText(e: unknown) {
  return e instanceof Error ? e.message : String(e);
}

async function installCli(): Promise<void> {
  try {
    const result = await call(commands.appCliInstall());
    const hint = result.pathDirectory
      ? tx`Add ${result.pathDirectory} to the beginning of PATH, then open a new terminal.`
      : tx("Open a new terminal to use the oxidea command.");
    notifications().notify({
      level: "info",
      title: tx("The oxidea command is installed"),
      body: `${result.path}\n${hint}`,
      key: KEY,
    });
  } catch (e) {
    if (!isCancelled(e))
      notifications().notify({
        level: "error",
        title: tx("Cannot install the oxidea command"),
        body: errorText(e),
        key: KEY,
      });
  }
}

async function uninstallCli(): Promise<void> {
  try {
    const links = await call(commands.appCliUninstall());
    notifications().notify({
      level: "info",
      title: tx(links.length ? "The oxidea command is uninstalled" : "The oxidea command is not installed"),
      body: links.join("\n") || undefined,
      key: KEY,
    });
  } catch (e) {
    if (!isCancelled(e))
      notifications().notify({
        level: "error",
        title: tx("Cannot uninstall the oxidea command"),
        body: errorText(e),
        key: KEY,
      });
  }
}

export function cliActions(): Action[] {
  return [
    {
      id: "app.installCli",
      title: tx("Install 'oxidea' Command"),
      category: tx("Tools"),
      keywords: ["cli", "shell", "terminal", "path", "command line", "launcher"],
      run: installCli,
    },
    {
      id: "app.uninstallCli",
      title: tx("Uninstall 'oxidea' Command"),
      category: tx("Tools"),
      keywords: ["cli", "shell", "terminal", "path", "command line", "launcher"],
      run: uninstallCli,
    },
  ];
}
