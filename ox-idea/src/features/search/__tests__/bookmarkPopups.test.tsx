import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen } from "@testing-library/react";
import { EditorState } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { BookmarkPopupsOverlay, bookmarkRows } from "../bookmarks/BookmarkPopups";
import { bookmarkGutter } from "../bookmarks/gutter";
import {
  loadBookmarks,
  updateBookmarkData,
  updateBookmarks,
  useBookmarkPopups,
  useBookmarks,
  type Bookmark,
} from "../bookmarks/store";

beforeEach(() => {
  localStorage.clear();
  loadBookmarks("/p");
});

afterEach(() => {
  act(() => useBookmarkPopups.setState({ chooser: null, list: false, editing: null }));
});

const set = (items: Bookmark[]) => act(() => updateBookmarks(() => items));

describe("mnemonic chooser", () => {
  it("a typed letter assigns that mnemonic (with the description) and closes", () => {
    set([{ path: "/p/b", line: 0, text: "b", created: 1, mnemonic: "K" }]);
    render(<BookmarkPopupsOverlay />);
    act(() => useBookmarkPopups.setState({ chooser: { path: "/p/a", line: 2, text: "fun a()" } }));
    expect(screen.getByText("Add Mnemonic Bookmark")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "K" }).className).toMatch(/keyAssigned/);
    fireEvent.change(screen.getByLabelText("Bookmark description"), { target: { value: "entry" } });
    fireEvent.keyDown(screen.getByRole("button", { name: "1" }), { key: "k" });
    expect(useBookmarkPopups.getState().chooser).toBeNull();
    const items = useBookmarks.getState().items;
    expect(items.find((b) => b.path === "/p/a")).toMatchObject({ line: 2, mnemonic: "K", description: "entry" });
    expect(items.find((b) => b.path === "/p/b")?.mnemonic).toBeUndefined();
  });
});

describe("Bookmarks popup", () => {
  it("groups by file; Delete removes and F2 edits the description", () => {
    set([
      { path: "/p/a", line: 1, text: "one", created: 1 },
      { path: "/p/a", line: 5, text: "five", created: 2, mnemonic: "3" },
      { path: "/p/b", line: 0, text: "zero", created: 3 },
    ]);
    expect(bookmarkRows(useBookmarks.getState().items).map((r) => r.kind)).toEqual([
      "file",
      "mark",
      "mark",
      "file",
      "mark",
    ]);
    const { items, lists } = useBookmarks.getState();
    expect(bookmarkRows(items, [], lists).map((r) => r.kind)).toEqual(["list", "file", "mark", "mark", "file", "mark"]);
    render(<BookmarkPopupsOverlay />);
    act(() => useBookmarkPopups.setState({ list: true }));
    const list = screen.getByRole("listbox", { name: "Bookmarks" });
    expect(screen.getByText("five")).toBeInTheDocument();
    fireEvent.keyDown(list, { key: "Delete" });
    expect(useBookmarks.getState().items.map((b) => b.line)).toEqual([5, 0]);
    fireEvent.keyDown(list, { key: "F2" });
    expect(useBookmarkPopups.getState().editing).toEqual({ path: "/p/a", line: 5 });
    const input = screen.getByLabelText("Description");
    fireEvent.change(input, { target: { value: "main" } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(useBookmarks.getState().items[0]!.description).toBe("main");
  });
});

describe("Bookmarks popup with file and folder bookmarks", () => {
  it("groups by list: folders, then files with their line bookmarks; folders select in the project view", async () => {
    const actions = await import("@/platform/actions");
    const run = vi.spyOn(actions, "executeAction").mockResolvedValue(undefined as never);
    act(() =>
      updateBookmarkData((d) => ({
        ...d,
        lists: [...d.lists, { id: "list2", name: "Review" }],
        items: [
          { path: "/p/a.ts", line: 3, text: "three", created: 1, list: "default" },
          { path: "/p/c.ts", line: 0, text: "zero", created: 1, list: "list2" },
        ],
        paths: [
          { path: "/p/src", dir: true, list: "default", created: 1 },
          { path: "/p/a.ts", dir: false, list: "default", created: 1 },
          { path: "/p/b.ts", dir: false, list: "list2", created: 1 },
        ],
      })),
    );
    const { items, paths, lists } = useBookmarks.getState();
    expect(bookmarkRows(items, paths, lists).map((r) => r.key)).toEqual([
      "l:default",
      "d:default:/p/src",
      "p:default:/p/a.ts",
      "b:/p/a.ts:3",
      "l:list2",
      "p:list2:/p/b.ts",
      "f:list2:/p/c.ts",
      "b:/p/c.ts:0",
    ]);
    render(<BookmarkPopupsOverlay />);
    act(() => useBookmarkPopups.setState({ list: true }));
    expect(screen.getByText("Review")).toBeInTheDocument();
    // The folder is the first selectable row: Enter selects it in the project view.
    vi.useFakeTimers();
    fireEvent.keyDown(screen.getByRole("listbox", { name: "Bookmarks" }), { key: "Enter" });
    vi.runAllTimers();
    vi.useRealTimers();
    expect(run).toHaveBeenCalledWith("navigate.selectInProjectView", "/p/src");
    run.mockRestore();
  });
});

describe("bookmark gutter", () => {
  it("⌘/Ctrl-click in the gutter toggles a bookmark before other gutter handlers see it", () => {
    const toggle = vi.fn();
    const other = vi.fn();
    const view = new EditorView({
      parent: document.body,
      state: EditorState.create({ doc: "a\nb\n", extensions: bookmarkGutter("/p/a", toggle) }),
    });
    view.dom.addEventListener("mousedown", other);
    const gutter = view.dom.querySelector(".cm-gutters")!;
    gutter.dispatchEvent(new MouseEvent("mousedown", { bubbles: true, button: 0, metaKey: true, ctrlKey: true }));
    expect(toggle).toHaveBeenCalledWith(0);
    expect(other).not.toHaveBeenCalled();
    gutter.dispatchEvent(new MouseEvent("mousedown", { bubbles: true, button: 0 }));
    expect(toggle).toHaveBeenCalledTimes(1);
    expect(other).toHaveBeenCalledTimes(1);
    view.destroy();
  });

  it("shows the mnemonic instead of the bookmark icon", async () => {
    set([{ path: "/p/a", line: 1, text: "b", created: 1, mnemonic: "Q" }]);
    const view = new EditorView({
      parent: document.body,
      state: EditorState.create({ doc: "a\nb\n", extensions: bookmarkGutter("/p/a", () => {}) }),
    });
    await act(() => new Promise((r) => setTimeout(r, 0)));
    expect(view.dom.querySelector(".cm-openidea-bookmarkMnemonic")?.textContent).toBe("Q");
    view.destroy();
  });
});
