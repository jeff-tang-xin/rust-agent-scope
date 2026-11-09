/**
 * The Run console of a configuration with "Emulate terminal in output console":
 * the terminal widget (provided by the terminal feature) shows the raw output
 * and sends keystrokes and its size to the process.
 */
import { useEffect, useRef } from "react";
import { T } from "@/ui";
import { embeddedTerminal } from "@/workbench/contributions";
import type { TerminalOutput } from "./terminalOutput";
import styles from "./Run.module.css";

export function TerminalConsole({
  output,
  cwd,
  onData,
  onResize,
  "aria-label": ariaLabel,
}: {
  output: TerminalOutput;
  cwd: string;
  onData: (data: string) => void;
  onResize: (cols: number, rows: number) => void;
  "aria-label": string;
}) {
  const host = useRef<HTMLDivElement>(null);
  const handlers = useRef({ onData, onResize });
  useEffect(() => {
    handlers.current = { onData, onResize };
  });
  const factory = embeddedTerminal();

  useEffect(() => {
    const el = host.current;
    if (!el || !factory) return;
    let disposed = false;
    let cleanup = () => {};
    void factory(el, {
      cwd,
      "aria-label": ariaLabel,
      onData: (d) => handlers.current.onData(d),
      onResize: (c, r) => handlers.current.onResize(c, r),
    }).then((term) => {
      if (disposed) return term.dispose();
      const detach = output.attach(
        (d) => term.write(d),
        () => term.clear(),
      );
      cleanup = () => {
        detach();
        term.dispose();
      };
    });
    return () => {
      disposed = true;
      cleanup();
    };
  }, [factory, output, cwd, ariaLabel]);

  if (!factory)
    return (
      <div className={styles.notice}>
        <T>The terminal is not available; turn off "Emulate terminal in output console" to see the output.</T>
      </div>
    );
  return <div ref={host} className={styles.terminalConsole} />;
}
