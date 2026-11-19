/**
 * Closing the loop on a worktree task: the agent works on its own `agent/<slug>` branch
 * but does not commit, so its results sit in the worktree as uncommitted changes. Once the
 * task is idle this offers to commit those changes to the agent branch and merge it back
 * into the base branch (or open a pull request), reusing the VCS branch operations rather
 * than reinventing merge/checkout/conflict handling.
 */
import { useState } from "react";
import type { AgentTask } from "@/bindings";
import { call, commands } from "@/platform/ipc";
import { useTx } from "@/platform/i18n";
import { notifications } from "@/workbench/contributions";
import { branchRef, checkoutBranch, mergeIntoCurrent } from "@/features/vcs/branchOps";
import { confirmDialog } from "@/features/vcs/dialogs";
import { errorMessage, saveAll } from "@/features/vcs/ops";
import { currentRepo, refreshNow } from "@/features/vcs/store";
import { openCreatePullRequest } from "@/features/github/toolwindow/open";
import { isBusy } from "./model";
import { refreshChanges } from "./store";
import styles from "./Session.module.css";

/** The main checkout the agent branch should land in: the task's project root. */
function mainRepoOf(task: AgentTask): string | null {
  return task.projectRoot ?? currentRepo();
}

/** True once `repo` is checked out on `base`; switches it there (with confirmation) if not. */
async function ensureOnBase(repo: string, base: string): Promise<boolean> {
  const head = (await call(commands.vcsBranchInfo(repo)).catch(() => null))?.head ?? null;
  if (head === base) return true;
  const ok = await confirmDialog({
    title: "Merge Agent Branch",
    message: `The main checkout is on '${head ?? "a detached HEAD"}', not '${base}'.\nCheck out '${base}' before merging?`,
    okLabel: "Check Out",
  });
  if (!ok) return false;
  if (repo === currentRepo()) {
    // The proven Smart Checkout path (stashes local changes that are in the way).
    await checkoutBranch(base);
  } else {
    await call(commands.vcsCheckout(repo, base, false, { mode: "smart", diverged: null }));
  }
  const after = (await call(commands.vcsBranchInfo(repo)).catch(() => null))?.head ?? null;
  return after === base;
}

/**
 * Commits the worktree's changed files to the agent branch with their working-tree content.
 * A branch that is already committed reports "nothing to commit", which is not an error here:
 * the merge below still brings those commits across.
 */
async function commitWorktree(task: AgentTask, paths: string[]): Promise<boolean> {
  try {
    await call(
      commands.vcsCommit(task.cwd, `Agent task: ${task.title}`, {
        amend: false,
        signOff: false,
        author: null,
        runHooks: false,
        stage: paths,
      }),
    );
    return true;
  } catch (e) {
    const msg = errorMessage(e);
    if (/nothing to commit|no changes|nothing added/i.test(msg)) return true;
    notifications().notify({ level: "error", title: "Couldn't commit the worktree changes", body: msg });
    return false;
  }
}

/** Merge-to-base and Create-PR actions for a finished worktree task (rendered in the status bar). */
export function WorktreeMerge({ task }: { task: AgentTask }) {
  const tx = useTx();
  const [busy, setBusy] = useState(false);
  const branch = task.branch;
  const base = task.base;
  if (task.isolation !== "worktree" || !branch || !base || isBusy(task.status)) return null;

  const merge = async () => {
    const repo = mainRepoOf(task);
    if (!repo) {
      notifications().notify({ level: "warning", title: "No main checkout to merge into" });
      return;
    }
    const changes = await call(commands.agentChanges(task.id)).catch(() => []);
    if (!changes.length) {
      notifications().notify({ level: "info", title: tx`No changes to merge from '${branch}'` });
      return;
    }
    const ok = await confirmDialog({
      title: "Merge Agent Branch",
      message: tx`Commit ${changes.length} changed file(s) to '${branch}' and merge it into '${base}'?`,
      okLabel: "Merge",
    });
    if (!ok) return;
    setBusy(true);
    try {
      await saveAll();
      if (
        !(await commitWorktree(
          task,
          changes.map((c) => c.path),
        ))
      )
        return;
      if (!(await ensureOnBase(repo, base))) {
        notifications().notify({ level: "warning", title: tx`Main checkout is not on '${base}'; merge cancelled` });
        return;
      }
      await mergeIntoCurrent(branch, repo, branchRef(branch, false));
      await refreshNow();
      void refreshChanges(task.id);
    } catch (e) {
      notifications().notify({ level: "error", title: "Couldn't merge the agent branch", body: errorMessage(e) });
    } finally {
      setBusy(false);
    }
  };

  return (
    <span className={styles.worktreeMerge}>
      <button
        type="button"
        className={styles.worktreeMergeBtn}
        disabled={busy}
        title={tx`Commit the worktree changes and merge '${branch}' into '${base}'`}
        onClick={() => void merge()}
      >
        {tx`Merge to ${base}`}
      </button>
      <button
        type="button"
        className={styles.worktreeMergeBtn}
        disabled={busy}
        title="Create a pull request for the agent branch"
        onClick={() => void openCreatePullRequest()}
      >
        {tx("Create PR")}
      </button>
    </span>
  );
}
