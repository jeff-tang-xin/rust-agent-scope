/** `cargo test` (libtest) and `cargo nextest` output. */
import { parseDuration, type TestEvent, type TestOutputParser } from "./types";

const RUNNING_RE = /^\s+Running (?:unittests )?(\S+) \(.+\)$/;
const DOCTESTS_RE = /^\s*Doc-tests (\S+)/;
const TEST_RE = /^test (.+?) \.\.\. (ok|FAILED|ignored|bench)(?:,.*)?(?:\s+<([\d.]+s)>)?$/;
const FAILURE_HEADER_RE = /^---- (.+?) (?:stdout|stderr) ----$/;
// Rust ≥ 1.73: `thread 'x' panicked at src/lib.rs:10:5:`; older: `panicked at 'msg', src/lib.rs:10:5`.
const PANIC_RE = /panicked at (?:'.*', |".*", )?([^\s:'"][^:'"]*):(\d+):(\d+)/;
const SUMMARY_RE =
  /^test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored;.*?(?:finished in ([\d.]+s))?$/;
const NEXTEST_RE = /^\s*(PASS|FAIL|SKIP|SIGSEGV|SIGABRT|TIMEOUT|LEAK)\s+\[\s*([\d.]+s)\]\s+(\S+)\s+(.+)$/;
const NEXTEST_SUMMARY_RE =
  /^\s*Summary \[\s*([\d.]+s)\]\s+(\d+) tests? run: (\d+) passed(?:.*?(\d+) failed)?(?:.*?(\d+) skipped)?/;

export function cargoParser(): TestOutputParser {
  let suite: string | null = null;
  let failure: string[] | null = null;
  const totals = { passed: 0, failed: 0, skipped: 0, durationMs: 0 };

  const pathOf = (name: string) => {
    const parts = name.split("::");
    return suite ? [suite, ...parts] : parts;
  };

  return {
    feed(line) {
      const events: TestEvent[] = [];
      let m: RegExpExecArray | null;
      if ((m = RUNNING_RE.exec(line))) {
        suite = m[1]!;
        failure = null;
      } else if ((m = DOCTESTS_RE.exec(line))) {
        suite = `Doc-tests ${m[1]}`;
        failure = null;
      } else if ((m = TEST_RE.exec(line))) {
        const status = m[2] === "ok" || m[2] === "bench" ? "passed" : m[2] === "FAILED" ? "failed" : "skipped";
        events.push({ type: "result", path: pathOf(m[1]!), status, durationMs: parseDuration(m[3]) });
      } else if ((m = FAILURE_HEADER_RE.exec(line))) {
        failure = pathOf(m[1]!);
      } else if ((m = SUMMARY_RE.exec(line))) {
        failure = null;
        totals.passed += Number(m[1]);
        totals.failed += Number(m[2]);
        totals.skipped += Number(m[3]);
        totals.durationMs += parseDuration(m[4]) ?? 0;
        events.push({ type: "summary", ...totals });
      } else if ((m = NEXTEST_RE.exec(line))) {
        const status = m[1] === "PASS" ? "passed" : m[1] === "SKIP" ? "skipped" : "failed";
        events.push({
          type: "result",
          path: [m[3]!, ...m[4]!.split("::")],
          status,
          durationMs: parseDuration(m[2]),
        });
      } else if ((m = NEXTEST_SUMMARY_RE.exec(line))) {
        events.push({
          type: "summary",
          durationMs: parseDuration(m[1]),
          passed: Number(m[3]),
          failed: m[4] ? Number(m[4]) : 0,
          skipped: m[5] ? Number(m[5]) : 0,
        });
      } else if (failure) {
        if (line === "failures:" || line.startsWith("failures:")) {
          failure = null;
        } else {
          events.push({ type: "output", path: failure, line });
          const p = PANIC_RE.exec(line);
          if (p) {
            events.push({
              type: "location",
              path: failure,
              location: { path: p[1]!, line: Number(p[2]), col: Number(p[3]), resolve: "path" },
            });
          }
        }
      }
      return events;
    },
  };
}
