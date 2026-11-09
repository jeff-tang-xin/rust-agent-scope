import { beforeEach, describe, expect, it, vi } from "vitest";

const vcsLog = vi.fn();
const vcsConfigGet = vi.fn();

vi.mock("@/platform/ipc", async (importOriginal) => {
  const real = await importOriginal<typeof import("@/platform/ipc")>();
  return {
    ...real,
    commands: {
      ...real.commands,
      vcsLog: (repo: unknown, query: unknown) => vcsLog(repo, query),
      vcsConfigGet: (repo: unknown, key: string) => vcsConfigGet(repo, key),
    },
  };
});

import { parseDay, splitList } from "../dialogs";
import { unstashTitles } from "../changeOps";
import {
  ME,
  dateRangeLabel,
  emptyFilters,
  hasFilters,
  inDateRange,
  reloadLog,
  resolveAuthors,
  useGitLog,
} from "../logStore";

describe("Git Log filters", () => {
  it("resolves the 'me' user to the current Git user and drops duplicates", () => {
    expect(resolveAuthors([ME, "alice", " alice "], "me@corp.test")).toEqual(["me@corp.test", "alice"]);
    expect(resolveAuthors([ME], null)).toEqual([]);
  });

  it("matches a custom date range with an exclusive end", () => {
    const d = { after: 100, before: 200, label: "" };
    expect([99, 100, 199, 200].map((t) => inDateRange(t, d))).toEqual([false, true, true, false]);
    expect(inDateRange(5, { after: null, before: 10, label: "" })).toBe(true);
    expect(inDateRange(5, null)).toBe(true);
  });

  it("labels a date range by its first and last day", () => {
    const jan1 = parseDay("2024-01-01")!;
    const feb1 = parseDay("2024-02-01")!;
    expect(dateRangeLabel(jan1, feb1)).toBe("2024-01-01 – 2024-01-31");
    expect(dateRangeLabel(jan1, null)).toBe("after 2024-01-01");
    expect(dateRangeLabel(null, feb1)).toBe("until 2024-01-31");
    expect(parseDay("nope")).toBeNull();
  });

  it("counts multi-value filters as filters", () => {
    expect(hasFilters(emptyFilters())).toBe(false);
    expect(hasFilters({ ...emptyFilters(), branches: ["main", "dev"] })).toBe(true);
    expect(hasFilters({ ...emptyFilters(), authors: [ME] })).toBe(true);
  });

  it("splits comma-separated custom values", () => {
    expect(splitList(" a, ,b ,")).toEqual(["a", "b"]);
  });
});

describe("loading the log", () => {
  beforeEach(() => {
    vcsLog.mockReset().mockResolvedValue({ status: "ok", data: [] });
    vcsConfigGet.mockReset().mockImplementation(async (_repo: unknown, key: string) => ({
      status: "ok",
      data: key === "user.email" ? "me@corp.test" : null,
    }));
  });

  it("sends every selected branch and user, with 'me' resolved", async () => {
    useGitLog.setState({ filters: { ...emptyFilters(), branches: ["main", "dev"], authors: [ME, "bob"] } });
    await reloadLog();
    const queries = vcsLog.mock.calls.map((c) => c[1] as { branches?: string[]; authors?: string[]; range: unknown });
    const page = queries.find((q) => q.range === null)!;
    expect(page.branches).toEqual(["main", "dev"]);
    expect(page.authors).toEqual(["me@corp.test", "bob"]);
  });
});

describe("unstash", () => {
  it("names the operation after what it does", () => {
    const base = { index: 2, pop: false, reinstateIndex: true, branch: null };
    expect(unstashTitles(base).title).toBe("Apply stash@{2}");
    expect(unstashTitles({ ...base, pop: true }).success).toBe("Unstashed stash@{2}");
    expect(unstashTitles({ ...base, branch: "fix" }).title).toBe("Unstash stash@{2} as branch fix");
  });
});
