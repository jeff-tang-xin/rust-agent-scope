import clsx from "clsx";
import { formatBinding, shortcutLabel } from "@/platform/keymap";
import styles from "./Kbd.module.css";

export interface KbdProps {
  /** Raw binding (`Meta+Shift+O`) — formatted for display. */
  binding?: string;
  /** Show the primary shortcut of an action. */
  action?: string;
  /** Pre-formatted label. */
  children?: React.ReactNode;
  className?: string;
}

/** Keyboard shortcut label in IDEA's compact macOS notation (`⇧⌘O`). */
export function Kbd({ binding, action, children, className }: KbdProps) {
  const label = children ?? (binding ? formatBinding(binding) : action ? shortcutLabel(action) : undefined);
  if (!label) return null;
  return <kbd className={clsx(styles.kbd, className)}>{label}</kbd>;
}
