import type { CSSProperties, ReactNode } from "react";
import clsx from "clsx";
import { indexRuns } from "./match";
import styles from "./HighlightedText.module.css";

export interface HighlightedTextProps {
  text: string;
  /** Matched UTF-16 indices (sorted). */
  indices?: readonly number[] | null;
  className?: string;
  style?: CSSProperties;
  /** `accent` colours matches with the accent (selected rows); `strong` brightens them. */
  tone?: "accent" | "strong";
}

/** Renders `text` with matched characters emphasised, as in IDEA's choosers. */
export function HighlightedText({ text, indices, className, style, tone = "strong" }: HighlightedTextProps) {
  if (!indices || indices.length === 0)
    return (
      <span className={className} style={style}>
        {text}
      </span>
    );
  const parts: ReactNode[] = [];
  let pos = 0;
  for (const [s, e] of indexRuns(indices)) {
    if (s >= text.length) break;
    if (s > pos) parts.push(text.slice(pos, s));
    parts.push(
      <mark key={s} className={clsx(styles.match, tone === "accent" && styles.accent)}>
        {text.slice(s, e)}
      </mark>,
    );
    pos = e;
  }
  if (pos < text.length) parts.push(text.slice(pos));
  return (
    <span className={className} style={style}>
      {parts}
    </span>
  );
}
