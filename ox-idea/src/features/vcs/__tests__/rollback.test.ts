import { describe, expect, it } from "vitest";
import type { FileStatus } from "@/bindings";
import { isNestedRepoEntry, rollbackCandidates } from "../changeOps";

const fs = (rel: string, unstaged: FileStatus["unstaged"]): FileStatus => ({
  path: `/r/${rel}`,
  relPath: rel,
  staged: null,
  unstaged,
  renamedFrom: null,
});

describe("rollback candidates with a submodule", () => {
  const roots = ["/r", "/r/libs/core"];
  const status = [
    fs("app.txt", "modified"),
    fs("libs/core", "modified"),
    fs("libs/core/core.txt", "modified"),
    fs("libs/core/wip.txt", "untracked"),
  ];
  const paths = (files: FileStatus[]) => files.map((f) => f.path);

  it("never offers the submodule's own entry", () => {
    expect(isNestedRepoEntry("/r/libs/core", roots)).toBe(true);
    expect(isNestedRepoEntry("/r", roots)).toBe(false);
    expect(paths(rollbackCandidates(undefined, status, roots))).toEqual(["/r/app.txt", "/r/libs/core/core.txt"]);
  });

  it("takes a selected change alone, not the submodule's changes below its path", () => {
    expect(rollbackCandidates(["/r/libs/core"], status, roots)).toEqual([]);
    expect(paths(rollbackCandidates(["/r/app.txt", "/r/libs/core"], status, roots))).toEqual(["/r/app.txt"]);
  });

  it("takes the changes below a selected folder", () => {
    expect(paths(rollbackCandidates(["/r/libs"], status, roots))).toEqual(["/r/libs/core/core.txt"]);
  });
});
