/** The abbreviation prompt of Update Tag with Emmet. */
import { useState } from "react";
import { create } from "zustand";
import { Dialog, Input } from "@/ui";
import styles from "./Emmet.module.css";
import { tx } from "@/platform/i18n";

interface Pending {
  tag: string;
  resolve: (abbreviation: string | null) => void;
}

const usePrompt = create<{ pending: Pending | null }>(() => ({ pending: null }));

/** Asks for the abbreviation to apply to `tag`; null when cancelled. */
export function promptAbbreviation(tag: string): Promise<string | null> {
  usePrompt.getState().pending?.resolve(null);
  return new Promise((resolve) => usePrompt.setState({ pending: { tag, resolve } }));
}

function PromptDialog({ tag, resolve }: Pending) {
  const [value, setValue] = useState("");
  const done = (v: string | null) => {
    usePrompt.setState({ pending: null });
    resolve(v);
  };
  return (
    <Dialog
      open
      title="Update Tag with Emmet"
      onClose={() => done(null)}
      width={420}
      buttons={[
        { label: "OK", role: "default", disabled: !value.trim(), onClick: () => done(value) },
        { label: "Cancel", role: "cancel" },
      ]}
    >
      <div className={styles.body}>
        <label className={styles.label} htmlFor="emmet-update-tag">
          {tx`Abbreviation for <${tag}>, e.g.`} <code>section.card#main[data-x=1]</code>
        </label>
        <Input
          id="emmet-update-tag"
          autoFocus
          spellCheck={false}
          value={value}
          onChange={(e) => setValue(e.target.value)}
        />
      </div>
    </Dialog>
  );
}

export function UpdateTagDialogHost() {
  const pending = usePrompt((s) => s.pending);
  return pending ? <PromptDialog {...pending} /> : null;
}
