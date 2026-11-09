import { beforeEach, describe, expect, it } from "vitest";
import { buildBookmarkTree } from "../bookmarks/BookmarksView";
import {
  DEFAULT_LIST_ID,
  createList,
  deleteList,
  loadBookmarks,
  moveBookmarks,
  renameList,
  renamePathBookmarks,
  setDefaultList,
  toggleBookmark,
  togglePathBookmarks,
  updateBookmarkData,
  updateBookmarks,
  useBookmarks,
  validateListName,
  type BookmarkData,
} from "../bookmarks/store";

const data = (): BookmarkData => ({
  items: [{ path: "/p/a.ts", line: 1, text: "a", created: 1, list: DEFAULT_LIST_ID }],
  paths: [],
  lists: [{ id: DEFAULT_LIST_ID, name: "p" }],
  defaultList: DEFAULT_LIST_ID,
});

describe("bookmark lists", () => {
  it("creates, renames and validates lists", () => {
    const { data: d, id } = createList(data(), " Review ");
    expect(d.lists).toEqual([
      { id: DEFAULT_LIST_ID, name: "p" },
      { id, name: "Review" },
    ]);
    expect(validateListName(d.lists, "Review")).toMatch(/already exists/);
    expect(validateListName(d.lists, "Review", id)).toBeNull();
    expect(validateListName(d.lists, " ")).toMatch(/empty/);
    expect(renameList(d, id, "Later").lists[1]!.name).toBe("Later");
  });

  it("moves bookmarks between lists and changes the default", () => {
    const { data: d0, id } = createList(data(), "Two");
    let d = togglePathBookmarks(d0, [{ path: "/p/src", dir: true }], 5);
    d = moveBookmarks(d, [{ path: "/p/a.ts", line: 1 }, { path: "/p/src" }], id);
    expect(d.items[0]!.list).toBe(id);
    expect(d.paths[0]!.list).toBe(id);
    // Unknown target lists are ignored.
    expect(moveBookmarks(d, [{ path: "/p/src" }], "nope")).toBe(d);
    d = setDefaultList(d, id);
    expect(d.defaultList).toBe(id);
    d = togglePathBookmarks(d, [{ path: "/p/b.ts", dir: false }], 6);
    expect(d.paths.find((p) => p.path === "/p/b.ts")!.list).toBe(id);
  });

  it("deletes a list with its bookmarks, keeping a default", () => {
    const { data: d0, id } = createList(data(), "Two");
    const d = deleteList(setDefaultList(moveBookmarks(d0, [{ path: "/p/a.ts", line: 1 }], id), id), id, "p");
    expect(d.items).toEqual([]);
    expect(d.lists.map((l) => l.id)).toEqual([DEFAULT_LIST_ID]);
    expect(d.defaultList).toBe(DEFAULT_LIST_ID);
    const last = deleteList(d, DEFAULT_LIST_ID, "fresh");
    expect(last.lists).toEqual([{ id: DEFAULT_LIST_ID, name: "fresh" }]);
  });

  it("toggles file / folder bookmarks together and follows removals", () => {
    let d = togglePathBookmarks(data(), [{ path: "/p/x.ts", dir: false }], 1);
    d = togglePathBookmarks(
      d,
      [
        { path: "/p/x.ts", dir: false },
        { path: "/p/dir", dir: true },
      ],
      2,
    );
    expect(d.paths.map((p) => p.path)).toEqual(["/p/dir", "/p/x.ts"]);
    d = togglePathBookmarks(d, [
      { path: "/p/x.ts", dir: false },
      { path: "/p/dir", dir: true },
    ]);
    expect(d.paths).toEqual([]);
    const paths = togglePathBookmarks(data(), [{ path: "/p/dir/f.ts", dir: false }]).paths;
    expect(renamePathBookmarks(paths, "/p/dir", "/p/new").map((p) => p.path)).toEqual(["/p/new/f.ts"]);
    expect(renamePathBookmarks(paths, "/p/dir", null)).toEqual([]);
  });

  it("groups a list's folders, bookmarked files and line bookmarks", () => {
    const d = togglePathBookmarks(
      data(),
      [
        { path: "/p/a.ts", dir: false },
        { path: "/p/lib", dir: true },
      ],
      1,
    );
    const [list] = buildBookmarkTree(d);
    expect(list).toMatchObject({ kind: "list", isDefault: true, count: 3 });
    const children = list!.kind === "list" ? list!.children : [];
    expect(children.map((n) => n.kind)).toEqual(["folder", "file"]);
    expect(children[1]).toMatchObject({ kind: "file", bookmarked: true });
  });
});

describe("bookmark list persistence", () => {
  beforeEach(() => localStorage.clear());

  it("reads bookmarks saved before lists into the default list", () => {
    localStorage.setItem(
      "openidea.bookmarks.v1:/proj",
      JSON.stringify([{ path: "/proj/a", line: 2, text: "", created: 1 }]),
    );
    loadBookmarks("/proj");
    const s = useBookmarks.getState();
    expect(s.lists).toEqual([{ id: DEFAULT_LIST_ID, name: "proj" }]);
    expect(s.items[0]!.list).toBe(DEFAULT_LIST_ID);
  });

  it("persists lists, the default and file bookmarks; new line bookmarks join the default list", () => {
    loadBookmarks("/q");
    let id = "";
    updateBookmarkData((d) => {
      const made = createList(d, "Two");
      id = made.id;
      return togglePathBookmarks(setDefaultList(made.data, made.id), [{ path: "/q/f", dir: false }]);
    });
    updateBookmarks((l) => toggleBookmark(l, "/q/a", 0, "x"));
    loadBookmarks("/other");
    loadBookmarks("/q");
    const s = useBookmarks.getState();
    expect(s.lists.map((l) => l.name)).toEqual(["q", "Two"]);
    expect(s.defaultList).toBe(id);
    expect(s.paths).toMatchObject([{ path: "/q/f", list: id }]);
    expect(s.items).toMatchObject([{ path: "/q/a", list: id }]);
    // Line bookmarks stay readable by older versions (same key, plain array).
    expect(JSON.parse(localStorage.getItem("openidea.bookmarks.v1:/q")!)).toHaveLength(1);
  });

  it("ignores corrupt list storage", () => {
    localStorage.setItem(
      "openidea.bookmarkLists.v1:/c",
      JSON.stringify({ lists: [{ id: 3 }], defaultList: "x", paths: [1] }),
    );
    loadBookmarks("/c");
    const s = useBookmarks.getState();
    expect(s.lists).toEqual([{ id: DEFAULT_LIST_ID, name: "c" }]);
    expect(s.defaultList).toBe(DEFAULT_LIST_ID);
    expect(s.paths).toEqual([]);
  });
});
