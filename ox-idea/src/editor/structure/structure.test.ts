import { describe, expect, it } from "vitest";
import { IpcError } from "@/platform/ipc";
import { withFallback } from "./index";

const unavailable = () => new IpcError({ kind: "unavailable", message: "no server" });
const protocol = () => new IpcError({ kind: "protocol", message: "boom" });
const cancelled = () => new IpcError({ kind: "cancelled", message: "cancelled" });
const fail = (e: Error) => () => Promise.reject(e);
const ok =
  <T>(v: T) =>
  () =>
    Promise.resolve(v);

describe("withFallback", () => {
  it("prefers the language server", async () => {
    expect(await withFallback(ok([1]), ok([2]))).toEqual({ value: [1], source: "lsp" });
  });

  it("falls back when the server is not ready, fails or answers nothing", async () => {
    expect(await withFallback(fail(unavailable()), ok([2]))).toEqual({ value: [2], source: "syntax" });
    expect(await withFallback(fail(protocol()), ok([2]))).toEqual({ value: [2], source: "syntax" });
    expect(await withFallback(ok<number[]>([]), ok([2]))).toEqual({ value: [2], source: "syntax" });
    expect(await withFallback(ok(null), ok("x"))).toEqual({ value: "x", source: "syntax" });
  });

  it("honours the `when` conditions", async () => {
    await expect(withFallback(fail(protocol()), ok([2]), { when: ["notReady"] })).rejects.toThrow("boom");
    expect(await withFallback(ok<number[]>([]), ok([2]), { when: ["notReady", "error"] })).toEqual({
      value: [],
      source: "lsp",
    });
    expect(await withFallback(ok({ items: [] }), ok({ items: [1] }), { isEmpty: (v) => v.items.length === 0 })).toEqual(
      {
        value: { items: [1] },
        source: "syntax",
      },
    );
  });

  it("never falls back on cancellation and keeps the server outcome when the fallback fails", async () => {
    let called = false;
    const fb = () => {
      called = true;
      return Promise.resolve([2]);
    };
    await expect(withFallback(fail(cancelled()), fb)).rejects.toMatchObject({ kind: "cancelled" });
    expect(called).toBe(false);
    await expect(withFallback(fail(unavailable()), fail(unavailable()))).rejects.toThrow("no server");
    expect(await withFallback(ok<number[]>([]), ok<number[]>([]))).toEqual({ value: [], source: "lsp" });
    expect(await withFallback(ok<number[]>([]), fail(protocol()))).toEqual({ value: [], source: "lsp" });
  });

  it("answers from the fallback when the server is slow", async () => {
    const slowLsp = () => new Promise<number[]>((resolve) => setTimeout(() => resolve([1]), 200));
    expect(await withFallback(slowLsp, ok([2]), { when: ["slow"], slowMs: 10 })).toEqual({
      value: [2],
      source: "syntax",
    });
    // An empty fallback waits for the server.
    expect(await withFallback(slowLsp, ok<number[]>([]), { when: ["slow"], slowMs: 10 })).toEqual({
      value: [1],
      source: "lsp",
    });
    expect(await withFallback(ok([1]), ok([2]), { when: ["slow"], slowMs: 50 })).toEqual({ value: [1], source: "lsp" });
  });

  it("can skip the server", async () => {
    let lspCalled = false;
    const lsp = () => {
      lspCalled = true;
      return Promise.resolve([1]);
    };
    expect(await withFallback(lsp, ok([2]), { skipLsp: true })).toEqual({ value: [2], source: "syntax" });
    expect(lspCalled).toBe(false);
  });
});
