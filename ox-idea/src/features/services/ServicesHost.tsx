/**
 * The Services tool window hosts the service views features contribute
 * (Docker here, HTTP requests, database consoles…), one header tab each.
 */
import { useShallow } from "zustand/react/shallow";
import { Tabs } from "@/ui";
import { useActiveServiceView, useContributions, type ServiceViewContribution } from "@/workbench/contributions";
import styles from "./Services.module.css";

function useServiceViews(): ServiceViewContribution[] {
  return useContributions(
    useShallow((s) => [...s.serviceViews.values()].sort((a, b) => (a.order ?? 0) - (b.order ?? 0))),
  );
}

function useVisibleViews(): ServiceViewContribution[] {
  const views = useServiceViews();
  const used = useActiveServiceView((s) => s.used);
  return views.filter((v) => !v.onUse || used.has(v.id));
}

function useCurrentView(): ServiceViewContribution | null {
  const views = useVisibleViews();
  const active = useActiveServiceView((s) => s.active);
  return views.find((v) => v.id === active) ?? views[0] ?? null;
}

export function ServicesTabs() {
  const views = useVisibleViews();
  const current = useCurrentView();
  if (views.length < 2 || !current) return null;
  return (
    <Tabs
      variant="compact"
      className={styles.hostTabs}
      aria-label="Services"
      activeId={current.id}
      onSelect={(active) => useActiveServiceView.setState({ active })}
      items={views.map((v) => ({ id: v.id, label: v.title, text: v.title, icon: v.icon }))}
    />
  );
}

export function ServicesHeaderActions() {
  const Actions = useCurrentView()?.headerActions;
  return Actions ? <Actions /> : null;
}

export function ServicesHost() {
  const current = useCurrentView();
  if (!current) return null;
  const View = current.component;
  return <View key={current.id} />;
}
