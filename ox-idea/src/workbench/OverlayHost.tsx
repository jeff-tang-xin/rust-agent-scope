import { ContextMenuHost, TooltipHost } from "@/ui";
import { useContributions } from "./contributions";
import { ErrorBoundary } from "./ErrorBoundary";
import { InputDialogsHost } from "./inputDialogs";
import { MessagesHost } from "./messages";
import { Balloons } from "./Notifications";

/** Everything floating above the workbench: feature overlays, menus, tooltips, balloons. */
export function OverlayHost() {
  const overlays = useContributions((s) => s.overlays);
  return (
    <>
      {[...overlays.values()].map(({ id, component: C }) => (
        <ErrorBoundary key={id} name={`overlay ${id}`}>
          <C />
        </ErrorBoundary>
      ))}
      <MessagesHost />
      <InputDialogsHost />
      <Balloons />
      <ContextMenuHost />
      <TooltipHost />
    </>
  );
}
