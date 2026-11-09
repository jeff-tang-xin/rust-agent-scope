import { useState } from "react";
import { executeAction } from "@/platform/actions";
import { shortcutLabel } from "@/platform/keymap";
import { useProject } from "@/platform/project";
import { Button, Spinner, T } from "@/ui";
import { useWorkbenchSlots } from "./contributions";
import { ErrorBoundary, Slot } from "./ErrorBoundary";
import { usePlatform } from "@/platform/platformInfo";
import { TitleBar } from "./TitleBar";
import styles from "./Welcome.module.css";

function WelcomeFallback() {
  const opening = useProject((s) => s.opening);
  const version = usePlatform((s) => s.version);
  const [busy, setBusy] = useState(false);
  return (
    <div className={styles.fallback}>
      <div className={styles.brand}>
        <span className={styles.name}>
          <T>OpenIDEA</T>
        </span>
        {version && <span className={styles.version}>{version}</span>}
      </div>
      <div className={styles.actions}>
        <Button
          variant="primary"
          icon="folder-open"
          disabled={busy || opening}
          onClick={async () => {
            setBusy(true);
            try {
              await executeAction("file.openProject");
            } finally {
              setBusy(false);
            }
          }}
        >
          <T>Open…</T>
        </Button>
        {(busy || opening) && <Spinner size={14} className={styles.spinner} />}
      </div>
      <div className={styles.hint}>
        <span>
          <T>Open a folder</T>
        </span>
        <span className={styles.keys}>{shortcutLabel("file.openProject")}</span>
      </div>
    </div>
  );
}

/** Shown while no project is open; a feature can replace the content via `provideWelcomeView`. */
export function Welcome() {
  const Provided = useWorkbenchSlots((s) => s.welcome);
  return (
    <div className={styles.root}>
      <TitleBar minimal title="Welcome to OpenIDEA" />
      <main className={styles.main}>
        {Provided ? (
          <ErrorBoundary name="welcome view" fallback={<WelcomeFallback />}>
            <Slot component={Provided} />
          </ErrorBoundary>
        ) : (
          <WelcomeFallback />
        )}
      </main>
    </div>
  );
}
