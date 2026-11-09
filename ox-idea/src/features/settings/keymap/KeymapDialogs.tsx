/** Small modal dialogs of the Keymap page: keymap name (Duplicate / Rename) and confirmations. */
import { useId, useState } from "react";
import { Dialog, Input, T } from "@/ui";
import { validateKeymapName, type CustomKeymaps } from "./model";
import styles from "./Keymap.module.css";

export function KeymapNameDialog({
  title,
  initial,
  customs,
  ownId,
  onCancel,
  onSubmit,
}: {
  title: string;
  initial: string;
  customs: CustomKeymaps;
  ownId?: string;
  onCancel: () => void;
  onSubmit: (name: string) => void;
}) {
  const [name, setName] = useState(initial);
  const id = useId();
  const error = validateKeymapName(name, customs, ownId);
  const submit = () => !error && onSubmit(name.trim());
  return (
    <Dialog
      open
      onClose={onCancel}
      title={title}
      width={380}
      buttons={[
        { label: "OK", role: "default", disabled: !!error, onClick: submit },
        { label: "Cancel", role: "cancel" },
      ]}
    >
      <div className={styles.recordBody}>
        <div className={styles.recordRow}>
          <label className={styles.recordLabel} htmlFor={id}>
            <T>Name:</T>
          </label>
          <Input
            id={id}
            autoFocus
            value={name}
            invalid={!!error}
            onChange={(e) => setName(e.target.value)}
            onFocus={(e) => e.currentTarget.select()}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                submit();
              }
            }}
          />
        </div>
        {error && name.trim() !== "" && <div className={styles.nameError}>{error}</div>}
      </div>
    </Dialog>
  );
}
