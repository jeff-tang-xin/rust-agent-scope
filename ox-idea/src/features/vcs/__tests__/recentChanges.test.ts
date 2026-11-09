import { describe, expect, it } from "vitest";
import type { HistoryEntry } from "@/bindings";
import { recentChangeLabel, recentChanges } from "../recentChanges";

const e = (path: string, t: number, label = "Saved", kind: HistoryEntry["kind"] = "snapshot"): HistoryEntry => ({
  id: `${path}@${t}`,
  path,
  timestampMs: t,
  label,
  size: 1,
  kind,
});

describe("View › Recent Changes", () => {
  it("lists newest first and collapses runs of saves of one file", () => {
    const rows = recentChanges([
      e("/p/a.ts", 1),
      e("/p/a.ts", 3),
      e("/p/a.ts", 2),
      e("/p", 4, "Before Git checkout", "label"),
      e("/p/b.ts", 5),
    ]);
    expect(rows.map((r) => r.id)).toEqual(["/p/b.ts@5", "/p@4", "/p/a.ts@3"]);
    expect(recentChanges([e("/p/a", 1), e("/p/b", 2), e("/p/c", 3)], 2)).toHaveLength(2);
  });

  it("labels revisions by project-relative path", () => {
    expect(recentChangeLabel(e("/p/src/a.ts", 1), "/p")).toBe("src/a.ts — Saved");
    expect(recentChangeLabel(e("/p", 1, "Release", "label"), "/p")).toBe("Release");
  });
});
