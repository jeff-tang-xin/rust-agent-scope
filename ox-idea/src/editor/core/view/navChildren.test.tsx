import { describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen } from "@testing-library/react";
import type { OutlineItem } from "@/platform/ipc";

const openFile = vi.fn(async () => {});
vi.mock("../controller", () => ({
  editorController: () => ({ openFile }),
  useEditorStatus: () => null,
}));

const { useOutline } = await import("../model/document");
const { NavChildrenPopup } = await import("./NavBarPopup");

const range = (line: number) => ({ start: { line, col: 2 }, end: { line, col: 5 } });
const item = (name: string, line: number, children: OutlineItem[] = []): OutlineItem =>
  ({ name, detail: null, kind: "function", range: range(line), selection_range: range(line), children }) as OutlineItem;

describe("status bar navigation bar popup", () => {
  it("lists a file's symbols and navigates to the chosen one", async () => {
    useOutline.setState({ "/p/a.ts": [item("first", 1), item("second", 7)] });
    const anchor = document.createElement("button");
    document.body.appendChild(anchor);
    const onClose = vi.fn();
    render(
      <NavChildrenPopup segment={{ kind: "file", path: "/p/a.ts", label: "a.ts" }} anchor={anchor} onClose={onClose} />,
    );
    await act(async () => {});
    const list = screen.getByRole("listbox", { name: "a.ts" });
    expect(screen.getAllByRole("option").map((o) => o.textContent)).toEqual(["first", "second"]);
    fireEvent.keyDown(list, { key: "ArrowDown" });
    fireEvent.keyDown(list, { key: "Enter" });
    expect(onClose).toHaveBeenCalled();
    expect(openFile).toHaveBeenCalledWith("/p/a.ts", { selection: { line: 7, col: 2 }, focus: true });
  });
});
