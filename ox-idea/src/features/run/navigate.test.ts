import { afterEach, describe, expect, it, vi } from "vitest";
import { jvmFrameTarget } from "./links";

const files: Record<string, string> = {
  "/p/src/main/java/com/acme/Util.java": "package com.acme;\nclass Util {}",
  "/p/other/Util.java": "package org.other;\nclass Util {}",
  "/p/src/main/kotlin/Orders.kt": "package com.acme.orders\nfun main() {}",
};
const symbols = vi.hoisted(() => ({ list: [] as unknown[] }));

vi.mock("@/platform/ipc", () => ({
  isTauri: () => true,
  nextRequestId: () => 1,
  call: async (v: unknown) => v,
  commands: {
    fsReadDir: async () => [],
    fsReadText: async (path: string) => files[path] ?? "",
    findFiles: async (name: string) =>
      Object.keys(files)
        .filter((p) => p.endsWith(`/${name}`))
        .map((path) => ({ path, relPath: path.slice(3) })),
    lspWorkspaceSymbols: async () => symbols.list,
  },
}));

const { resolveTarget } = await import("./navigate");

describe("JVM stack frame resolution", () => {
  afterEach(() => {
    symbols.list = [];
  });

  it("picks the file of the frame's package, not any file of that name", async () => {
    expect(await resolveTarget(jvmFrameTarget("com.acme.Util", "Util.java", 3), "/p")).toBe(
      "/p/src/main/java/com/acme/Util.java",
    );
    expect(await resolveTarget(jvmFrameTarget("org.other.Util", "Util.java", 3), "/p")).toBe("/p/other/Util.java");
  });

  it("finds Kotlin files outside their package directory by their package declaration", async () => {
    expect(await resolveTarget(jvmFrameTarget("com.acme.orders.OrdersKt", "Orders.kt", 1), "/p")).toBe(
      "/p/src/main/kotlin/Orders.kt",
    );
  });

  it("library classes open the language server's source, else nothing", async () => {
    expect(await resolveTarget(jvmFrameTarget("java.util.ArrayList", "ArrayList.java", 427), "/p")).toBeNull();
    symbols.list = [
      {
        name: "ArrayList",
        kind: "class",
        container: "java.util",
        location: { path: "jdt://contents/ArrayList.class", uri: "jdt://contents/ArrayList.class" },
      },
    ];
    expect(await resolveTarget(jvmFrameTarget("java.util.ArrayList", "ArrayList.java", 427), "/p")).toBe(
      "jdt://contents/ArrayList.class",
    );
    // Same simple name in another package isn't it.
    expect(await resolveTarget(jvmFrameTarget("java.awt.List$X", "List.java", 1), "/p")).toBeNull();
  });
});
