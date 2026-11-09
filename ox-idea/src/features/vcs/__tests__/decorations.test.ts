import { describe, expect, it } from "vitest";
import type { FileStatus } from "@/bindings";
import { getFileDecoration, setFileDecorations } from "@/workbench/contributions";
import { effectiveKind, stashLabel, statusDecorations } from "../model";

const fs = (
  rel: string,
  staged: FileStatus["staged"],
  unstaged: FileStatus["unstaged"],
  renamedFrom: string | null = null,
): FileStatus => ({
  path: `/repo/${rel}`,
  relPath: rel,
  staged,
  unstaged,
  renamedFrom,
});

describe("effectiveKind", () => {
  it("maps staged / unstaged pairs like IDEA's change lists", () => {
    expect(effectiveKind(fs("a", null, "modified"))).toBe("modified");
    expect(effectiveKind(fs("a", "modified", null))).toBe("modified");
    expect(effectiveKind(fs("a", "added", "modified"))).toBe("added");
    expect(effectiveKind(fs("a", null, "untracked"))).toBe("untracked");
    expect(effectiveKind(fs("a", null, "deleted"))).toBe("deleted");
    expect(effectiveKind(fs("a", "deleted", null))).toBe("deleted");
    expect(effectiveKind(fs("a", "renamed", null))).toBe("renamed");
    expect(effectiveKind(fs("a", null, "conflicted"))).toBe("conflicted");
    expect(effectiveKind(fs("a", null, "ignored"))).toBe("ignored");
    // Added then deleted in the worktree: nothing to show.
    expect(effectiveKind(fs("a", "added", "deleted"))).toBeNull();
    expect(effectiveKind(fs("a", null, null))).toBeNull();
  });
});

describe("statusDecorations", () => {
  it("colours files with the vcs tokens and propagates to folders", () => {
    const d = statusDecorations([
      fs("src/app/Main.kt", null, "modified"),
      fs("src/app/New.kt", null, "untracked"),
      fs("build/", null, "ignored"),
      fs("docs/Moved.md", "renamed", null, "docs/Old.md"),
    ]);
    expect(d.get("/repo/src/app/Main.kt")).toMatchObject({ color: "var(--vcs-modified)", propagate: true });
    expect(d.get("/repo/src/app/New.kt")).toMatchObject({ color: "var(--vcs-untracked)", propagate: true });
    // Directory entries lose the trailing slash; ignored entries are faded and don't propagate.
    expect(d.get("/repo/build")).toMatchObject({ color: "var(--vcs-ignored)", faded: true });
    expect(d.get("/repo/build")?.propagate).toBeUndefined();
    expect(d.get("/repo/docs/Moved.md")?.tooltip).toContain("docs/Old.md");
  });

  it("gives folders the most significant colour (conflict > modified > untracked)", () => {
    setFileDecorations(
      "vcs",
      statusDecorations([
        fs("src/a/New.kt", null, "untracked"),
        fs("src/a/Changed.kt", null, "modified"),
        fs("src/b/Conflict.kt", null, "conflicted"),
        fs("lib/Only.kt", null, "untracked"),
      ]),
    );
    expect(getFileDecoration("/repo/src/a")?.color).toBe("var(--vcs-modified)");
    expect(getFileDecoration("/repo/src")?.color).toBe("var(--vcs-conflict)");
    expect(getFileDecoration("/repo/lib")?.color).toBe("var(--vcs-untracked)");
    expect(getFileDecoration("/repo/src/a/New.kt")?.color).toBe("var(--vcs-untracked)");
    setFileDecorations("vcs", new Map());
    expect(getFileDecoration("/repo/src")).toBeUndefined();
  });
});

describe("stashLabel", () => {
  it("splits git's stash subjects into branch and message", () => {
    expect(stashLabel("On main: wip make")).toEqual({ branch: "main", text: "wip make" });
    expect(stashLabel("WIP on feature/x: 1a2b3c4 Add thing")).toEqual({
      branch: "feature/x",
      text: "1a2b3c4 Add thing",
    });
    expect(stashLabel("custom subject")).toEqual({ branch: null, text: "custom subject" });
  });
});
