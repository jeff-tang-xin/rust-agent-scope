/**
 * Lightweight editor observation shared by Structure (caret / edit
 * versions), Recent Files ("edited only") and Recent Locations. One
 * CodeMirror update listener per editor; everything else reads the stores.
 */
import { EditorView, type ViewUpdate } from "@codemirror/view";
import { create } from "zustand";
import {
  activeEditor,
  isExternalChange,
  onEditorEvent,
  registerEditorContribution,
  useEditorRegistry,
} from "@/editor/api";
import { call, commands, onIdeEvent } from "@/platform/ipc";
import {
  MERGE_DISTANCE,
  recordLocation,
  renameLocations,
  type DismissedLocation,
  type RecentLocation,
} from "./locations";

export interface CaretInfo {
  path: string;
  doc: number;
  line: number;
  col: number;
  offset: number;
}

interface TrackingState {
  /** Caret of the active text editor. */
  caret: CaretInfo | null;
  /** Bumped per path on every document change (user edits and backend patches). */
  editVersions: ReadonlyMap<string, number>;
  /** Files edited in this session (Recent Files "Show edited only"). */
  edited: ReadonlySet<string>;
  locations: readonly RecentLocation[];
  /** Places removed from Recent Locations (the editor's own history can't forget them). */
  dismissed: readonly DismissedLocation[];
}

export const useTracking = create<TrackingState>(() => ({
  caret: null,
  editVersions: new Map(),
  edited: new Set(),
  locations: [],
  dismissed: [],
}));

/** Recent Locations › Delete: forgets the place until it is visited again. */
export function dismissLocation(l: RecentLocation) {
  const near = (x: RecentLocation) => x.path === l.path && Math.abs(x.line - l.line) <= MERGE_DISTANCE;
  useTracking.setState((s) => ({
    locations: s.locations.filter((x) => !near(x)),
    dismissed: [...s.dismissed.slice(-99), { path: l.path, line: l.line, time: Date.now() }],
  }));
}

const docPaths = new Map<number, string>();

/** The window switched projects: the previous one's places and edits don't apply. */
export function resetTracking() {
  useTracking.setState({ caret: null, editVersions: new Map(), edited: new Set(), locations: [], dismissed: [] });
}

function bumpVersion(path: string, userEdit: boolean) {
  useTracking.setState((s) => ({
    editVersions: new Map(s.editVersions).set(path, (s.editVersions.get(path) ?? 0) + 1),
    edited: userEdit && !s.edited.has(path) ? new Set(s.edited).add(path) : s.edited,
  }));
}

function caretOf(view: EditorView, path: string, doc: number): CaretInfo {
  const head = view.state.selection.main.head;
  const line = view.state.doc.lineAt(head);
  return { path, doc, line: line.number - 1, col: head - line.from, offset: head };
}

function snippetAround(view: EditorView, line0: number): { snippet: string[]; snippetStart: number } {
  const doc = view.state.doc;
  const start = Math.max(1, line0);
  const end = Math.min(doc.lines, line0 + 2);
  const snippet: string[] = [];
  for (let n = start; n <= end; n++) snippet.push(doc.line(n).text.slice(0, 300));
  return { snippet, snippetStart: start - 1 };
}

let recordTimer: ReturnType<typeof setTimeout> | undefined;
let pendingChanged = false;

function scheduleRecord(view: EditorView, path: string, changed: boolean) {
  pendingChanged ||= changed;
  clearTimeout(recordTimer);
  recordTimer = setTimeout(() => {
    const handle = activeEditor();
    if (!handle || handle.view !== view || !view.dom.isConnected) return;
    const c = caretOf(view, path, handle.ctx.doc);
    const entry: RecentLocation = {
      path,
      line: c.line,
      col: c.col,
      ...snippetAround(view, c.line),
      changed: pendingChanged,
      time: Date.now(),
    };
    pendingChanged = false;
    useTracking.setState((s) => ({ locations: recordLocation(s.locations, entry) }));
  }, 500);
}

function syncActiveCaret() {
  const handle = activeEditor();
  const next = handle ? caretOf(handle.view, handle.ctx.path, handle.ctx.doc) : null;
  const prev = useTracking.getState().caret;
  if (
    prev?.path === next?.path &&
    prev?.offset === next?.offset &&
    prev?.line === next?.line &&
    prev?.doc === next?.doc
  )
    return;
  useTracking.setState({ caret: next });
}

const USER_EDIT = ["input", "delete", "undo", "redo", "move", "paste", "drop"];

export function installTracking(): () => void {
  const disposeContribution = registerEditorContribution({
    id: "search.tracking",
    order: 200,
    extensions: (ctx) => {
      docPaths.set(ctx.doc, ctx.path);
      return EditorView.updateListener.of((u: ViewUpdate) => {
        const isActive = activeEditor()?.view === u.view;
        if (u.docChanged) {
          const user = u.transactions.some((t) => !isExternalChange(t) && USER_EDIT.some((ev) => t.isUserEvent(ev)));
          bumpVersion(ctx.path, user);
          if (user && isActive) scheduleRecord(u.view, ctx.path, true);
        }
        if (isActive && (u.selectionSet || u.docChanged || u.focusChanged)) {
          syncActiveCaret();
          if (u.selectionSet || u.focusChanged) scheduleRecord(u.view, ctx.path, false);
        }
      });
    },
  });

  const unsubRegistry = useEditorRegistry.subscribe((s, prev) => {
    if (s.active === prev.active) return;
    syncActiveCaret();
    if (s.active) scheduleRecord(s.active.view, s.active.ctx.path, false);
  });
  syncActiveCaret();

  // Recent Files is ordered by last activation, not first open (IDEA): switching
  // to an already-open tab must move it to the top of ⌘E / ⌃⇥.
  const unsubActive = onEditorEvent((e) => {
    if (e.type === "activeEditorChanged" && e.path) void call(commands.markRecent(e.path)).catch(() => {});
  });

  const unsubPatched = onIdeEvent("docPatched", (e) => {
    const path =
      docPaths.get(e.doc) ?? [...useEditorRegistry.getState().editors].find((h) => h.ctx.doc === e.doc)?.ctx.path;
    if (path) bumpVersion(path, false);
  });
  const unsubFs = onIdeEvent("fsChanged", (e) => {
    if (!e.removed.length) return;
    useTracking.setState((s) => {
      let locations = s.locations;
      for (const p of e.removed) locations = renameLocations(locations, p, null);
      return { locations };
    });
  });

  return () => {
    clearTimeout(recordTimer);
    disposeContribution();
    unsubRegistry();
    unsubActive();
    unsubPatched();
    unsubFs();
  };
}
