import { beforeEach, describe, expect, it, vi } from "vitest";
import type { DocModel } from "./model/document";
import { readOnlyQuestion, toggleReadOnly } from "./fileAttributes";

function model(readOnly: boolean, extra: Partial<{ virtual: boolean; banner: string | null }> = {}) {
  const meta = { readOnly, banner: extra.banner ?? (readOnly ? "readOnly" : null) };
  return {
    path: "/p/a.txt",
    virtual: extra.virtual ?? false,
    getMeta: () => meta,
    setReadOnly: vi.fn((ro: boolean) => void (meta.readOnly = ro)),
  } as unknown as DocModel & { setReadOnly: ReturnType<typeof vi.fn> };
}

let answer = true;
const { confirm } = vi.hoisted(() => ({ confirm: vi.fn(async () => answer) }));
vi.mock("@/workbench/messages", () => ({ confirm }));

beforeEach(() => confirm.mockClear());

describe("Toggle Read-only Attribute", () => {
  it("asks before making a read-only file writable, then toggles", async () => {
    const m = model(true);
    answer = true;
    toggleReadOnly(m);
    expect(confirm).toHaveBeenCalledWith(expect.objectContaining({ title: "Make File Writable", ok: "Make Writable" }));
    await vi.waitFor(() => expect(m.setReadOnly).toHaveBeenCalledWith(false));
  });

  it("Cancel leaves the file alone", async () => {
    const m = model(false);
    answer = false;
    toggleReadOnly(m);
    expect(confirm).toHaveBeenCalledWith(expect.objectContaining({ title: "Make File Read-only" }));
    await Promise.resolve();
    expect(m.setReadOnly).not.toHaveBeenCalled();
  });

  it("does nothing for library and binary documents", () => {
    toggleReadOnly(model(true, { virtual: true }));
    toggleReadOnly(model(true, { banner: "binary" }));
    expect(confirm).not.toHaveBeenCalled();
    expect(readOnlyQuestion("a.txt", false).confirm).toBe("Make Read-only");
  });
});
