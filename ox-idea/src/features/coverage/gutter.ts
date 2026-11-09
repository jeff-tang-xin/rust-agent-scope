/**
 * Coverage stripes in the editor's change lane (as in IDEA, on top of VCS
 * markers): green covered, yellow partly covered (branches / blocks), red
 * not covered. Lines are mapped through edits until the next run.
 */
import { RangeSet, StateEffect, StateField, type Extension, type Text } from "@codemirror/state";
import { GutterMarker, ViewPlugin, type EditorView } from "@codemirror/view";
import { LaneMarker, gutterLaneMarkers, type EditorDocContext } from "@/editor/api";
import type { FileCoverage } from "@/platform/ipc";
import { lineState, type LineState } from "./model";
import { shownFileCoverage, useCoverage } from "./store";

const COLOR: Record<LineState, string> = {
  covered: "var(--coverage-covered)",
  partial: "var(--coverage-partial)",
  uncovered: "var(--coverage-uncovered)",
};

function title(l: FileCoverage["lines"][number]): string {
  const state = lineState(l);
  const hits = state === "uncovered" ? "Not covered" : `Hits: ${l.hits}`;
  return l.branchesTotal > 0 ? `${hits}, ${l.branchesCovered}/${l.branchesTotal} branches covered` : hits;
}

const setCoverage = StateEffect.define<FileCoverage | null>();

export function coverageMarkers(file: FileCoverage | null, doc: Text): RangeSet<GutterMarker> {
  if (!file) return RangeSet.empty;
  const ranges = [];
  for (const l of file.lines) {
    if (l.line >= doc.lines) continue;
    ranges.push(new LaneMarker(COLOR[lineState(l)], "bar", title(l)).range(doc.line(l.line + 1).from));
  }
  return RangeSet.of(ranges, true);
}

const coverageField = StateField.define<RangeSet<GutterMarker>>({
  create: () => RangeSet.empty,
  update(value, tr) {
    for (const e of tr.effects) if (e.is(setCoverage)) return coverageMarkers(e.value, tr.state.doc);
    return tr.docChanged ? value.map(tr.changes) : value;
  },
});

class CoverageSync {
  private readonly unsub: () => void;
  private shown: FileCoverage | null = null;

  constructor(
    private readonly view: EditorView,
    private readonly path: string,
  ) {
    this.unsub = useCoverage.subscribe(() => this.sync());
    // Not inside the constructor's update cycle.
    queueMicrotask(() => this.sync());
  }

  private destroyed = false;

  sync() {
    if (this.destroyed) return;
    const file = shownFileCoverage(this.path);
    if (file === this.shown) return;
    this.shown = file;
    this.view.dispatch({ effects: setCoverage.of(file) });
  }

  destroy() {
    this.destroyed = true;
    this.unsub();
  }
}

/** Extensions for one text editor. */
export function coverageGutter(ctx: EditorDocContext): Extension {
  if (ctx.virtual) return [];
  return [
    coverageField,
    gutterLaneMarkers.from(coverageField),
    ViewPlugin.define((view) => new CoverageSync(view, ctx.path)),
  ];
}
