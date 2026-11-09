/** Coverage tool window: one tab per recent suite, a sortable tree of directories and files. */
import { useMemo, useState } from "react";
import clsx from "clsx";
import { executeAction } from "@/platform/actions";
import { useProject } from "@/platform/project";
import { EmptyState, Icon, IconButton, Tabs, Tree, fileIconName, T } from "@/ui";
import { editorService } from "@/workbench/contributions";
import { coverageTree, formatPercent, percent, sortTree, type CoverageNode, type SortKey } from "./model";
import { activeSuite, closeSuite, selectSuite, setCoverageVisible, useCoverage } from "./store";
import styles from "./Coverage.module.css";

export function CoverageTabs() {
  const suites = useCoverage((s) => s.suites);
  const activeId = useCoverage((s) => s.activeId);
  if (!suites.length) return null;
  return (
    <Tabs
      variant="compact"
      aria-label="Coverage suites"
      className={styles.tabs}
      activeId={activeId}
      onSelect={selectSuite}
      onClose={closeSuite}
      items={suites.map((s) => ({
        id: s.id,
        label: s.name,
        closable: true,
        tooltip: `${s.data.runner}, ${new Date(s.createdAt).toLocaleTimeString()}`,
      }))}
    />
  );
}

export function CoverageHeaderActions() {
  const visible = useCoverage((s) => s.visible);
  const hasSuite = useCoverage((s) => !!s.activeId);
  return (
    <>
      <IconButton icon="folder-open" size="small" action="coverage.importReport" />
      <IconButton icon="list" size="small" action="coverage.manageReports" />
      <IconButton
        icon={visible ? "close" : "eye"}
        size="small"
        title={visible ? "Hide Coverage" : "Show Coverage"}
        disabled={!hasSuite}
        onClick={() => setCoverageVisible(!visible)}
      />
    </>
  );
}

function Bar({ covered, total }: { covered: number; total: number }) {
  const p = percent(covered, total);
  return (
    <span className={styles.cell}>
      {p !== null && (
        <span className={styles.bar} aria-hidden="true">
          <span className={styles.barFill} style={{ width: `${p}%` }} />
        </span>
      )}
      <span className={styles.value}>{formatPercent(covered, total)}</span>
    </span>
  );
}

const COLUMNS: Array<{ key: SortKey; label: string }> = [
  { key: "name", label: "Element" },
  { key: "lines", label: "Line, %" },
  { key: "branches", label: "Branch, %" },
];

export function CoverageView() {
  const suite = useCoverage((s) => activeSuite(s));
  const visible = useCoverage((s) => s.visible);
  const root = useProject((s) => s.project?.root ?? "");
  const [sort, setSort] = useState<{ key: SortKey; ascending: boolean }>({ key: "name", ascending: true });
  const [expanded, setExpanded] = useState<Set<string> | null>(null);

  const roots = useMemo(
    () => (suite ? sortTree(coverageTree(suite.data, root), sort.key, sort.ascending) : []),
    [suite, root, sort],
  );
  const effectiveExpanded = useMemo(
    () => expanded ?? new Set(roots.filter((n) => n.dir).map((n) => n.path)),
    [expanded, roots],
  );

  if (!suite) {
    return (
      <EmptyState title="No coverage data">
        <span>
          <T>Run a configuration with coverage to see which lines its tests execute.</T>
        </span>
        <span className={styles.links}>
          <button type="button" className={styles.link} onClick={() => void executeAction("run.coverage")}>
            <T>Run with Coverage</T>
          </button>
          <button type="button" className={styles.link} onClick={() => void executeAction("coverage.importReport")}>
            <T>Import a report collected in CI from disk</T>
          </button>
        </span>
      </EmptyState>
    );
  }
  const branches = suite.data.hasBranches;
  const columns = branches ? COLUMNS : COLUMNS.slice(0, 2);
  const toggleSort = (key: SortKey) =>
    setSort((s) => (s.key === key ? { key, ascending: !s.ascending } : { key, ascending: key === "name" }));

  return (
    <div className={styles.view}>
      {!visible && (
        <div className={styles.banner}>
          <T>Coverage is hidden in editors and the project view.</T>
        </div>
      )}
      <div className={clsx(styles.header, !branches && styles.noBranches)} role="row">
        {columns.map((c) => (
          <button
            key={c.key}
            type="button"
            role="columnheader"
            aria-sort={sort.key === c.key ? (sort.ascending ? "ascending" : "descending") : "none"}
            className={styles.headerCell}
            onClick={() => toggleSort(c.key)}
          >
            {c.label}
            {sort.key === c.key && <Icon name={sort.ascending ? "chevron-up" : "chevron-down"} size={12} />}
          </button>
        ))}
      </div>
      <Tree<CoverageNode>
        aria-label="Coverage"
        className={styles.tree}
        roots={roots}
        getKey={(n) => n.path}
        getLabel={(n) => n.name}
        getChildren={(n) => (n.dir ? n.children : undefined)}
        isExpandable={(n) => n.dir}
        expanded={effectiveExpanded}
        onExpandedChange={(next) => setExpanded(next)}
        onActivate={(n) => {
          if (!n.dir) void editorService().openFile(n.path);
        }}
        renderNode={(n) => (
          <span className={clsx(styles.row, !branches && styles.noBranches)}>
            <span className={styles.name}>
              <Icon name={n.dir ? "folder" : fileIconName(n.name)} size={16} className={styles.icon} />
              <span className={styles.label}>{n.name}</span>
            </span>
            <Bar covered={n.linesCovered} total={n.linesTotal} />
            {branches && <Bar covered={n.branchesCovered} total={n.branchesTotal} />}
          </span>
        )}
        empty={<EmptyState title="The report covers no files" />}
      />
    </div>
  );
}
