import { create } from "zustand";

export type RecentPopupKind = "files" | "locations";

interface RecentPopupState {
  open: RecentPopupKind | null;
  /** Recent Files: only files edited this session (⌘E again). */
  editedOnly: boolean;
  /** Recent Locations: only locations with changes (⇧⌘E again). */
  changedOnly: boolean;
  session: number;
  /** Opened by ⌃⇥ (IDEA Switcher): releasing Ctrl opens the selection. */
  switcher: boolean;
  /** Net ⌃⇥ / ⌃⇧⇥ presses since the switcher opened. */
  step: number;
}

export const useRecentPopup = create<RecentPopupState>(() => ({
  open: null,
  editedOnly: false,
  changedOnly: false,
  session: 0,
  switcher: false,
  step: 0,
}));

export function showRecentPopup(kind: RecentPopupKind) {
  const s = useRecentPopup.getState();
  if (s.open === kind) {
    if (kind === "files") useRecentPopup.setState({ editedOnly: !s.editedOnly });
    else useRecentPopup.setState({ changedOnly: !s.changedOnly });
    return;
  }
  useRecentPopup.setState({
    open: kind,
    editedOnly: false,
    changedOnly: false,
    session: s.session + 1,
    switcher: false,
    step: 0,
  });
}

/** ⌃⇥ / ⌃⇧⇥: opens the switcher, or moves its selection while Ctrl is held. */
export function cycleSwitcher(delta: 1 | -1) {
  const s = useRecentPopup.getState();
  if (s.open === "files" && s.switcher) {
    useRecentPopup.setState({ step: s.step + delta });
    return;
  }
  // The first press selects the previous file (the initial index), ⌃⇧⇥ the oldest.
  useRecentPopup.setState({
    open: "files",
    editedOnly: false,
    changedOnly: false,
    session: s.session + 1,
    switcher: true,
    step: delta < 0 ? -1 : 0,
  });
}

export function hideRecentPopup() {
  useRecentPopup.setState({ open: null });
}
