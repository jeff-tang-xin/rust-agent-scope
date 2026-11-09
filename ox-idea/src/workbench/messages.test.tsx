import { afterEach, describe, expect, it } from "vitest";
import { act, fireEvent, render, screen } from "@testing-library/react";
import { usePlatform } from "@/platform/platformInfo";
import { ask, confirmChecked, MessagesHost, type MessageResult } from "./messages";

function askUsagesDetected() {
  let result: MessageResult | undefined;
  act(() => {
    void ask({
      title: "Usages Detected",
      body: "Function 'unused' has 1 usage that is not safe to delete.",
      buttons: [
        { id: "view", label: "View Usages", primary: true },
        { id: "delete", label: "Delete Anyway" },
      ],
    }).then((r) => (result = r));
  });
  return () => result;
}

const footerLabels = () =>
  [...screen.getByRole("dialog").querySelectorAll("[data-dialog-footer] button")].map((b) => b.textContent);

describe("message dialogs", () => {
  const os = usePlatform.getState().os;
  afterEach(() => usePlatform.setState({ os }));

  it("macOS: Cancel · Delete Anyway · View Usages (default rightmost, focused); Enter picks the default", async () => {
    usePlatform.setState({ os: "macos" });
    render(<MessagesHost />);
    const result = askUsagesDetected();
    expect(footerLabels()).toEqual(["Cancel", "Delete Anyway", "View Usages"]);
    expect(document.activeElement).toBe(screen.getByRole("button", { name: "View Usages" }));
    await act(async () => void fireEvent.keyDown(screen.getByRole("dialog"), { key: "Enter" }));
    expect(result()).toEqual({ button: "view", checked: false });
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("Windows: View Usages · Delete Anyway · Cancel; Escape and Cancel cancel", async () => {
    usePlatform.setState({ os: "windows" });
    render(<MessagesHost />);
    let result = askUsagesDetected();
    expect(footerLabels()).toEqual(["View Usages", "Delete Anyway", "Cancel"]);
    await act(async () => void fireEvent.keyDown(document.body, { key: "Escape" }));
    expect(result()?.button).toBe("cancel");
    result = askUsagesDetected();
    await act(async () => void fireEvent.click(screen.getByRole("button", { name: "Cancel" })));
    expect(result()?.button).toBe("cancel");
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("reports the checkbox with the chosen button", async () => {
    render(<MessagesHost />);
    let result: { checked: boolean } | null | undefined;
    act(() => {
      void confirmChecked({
        title: "Delete Image",
        message: "Delete image 'x'?",
        ok: "Delete",
        checkbox: "Force",
      }).then((r) => (result = r));
    });
    fireEvent.click(screen.getByRole("checkbox", { name: "Force" }));
    await act(async () => void fireEvent.click(screen.getByRole("button", { name: "Delete" })));
    expect(result).toEqual({ checked: true });
  });

  it("starts with the given checkbox state and can be dismissed from outside", async () => {
    render(<MessagesHost />);
    const abort = new AbortController();
    let result: MessageResult | undefined;
    act(() => {
      void ask({
        title: "Delete",
        body: "Delete 'a'?",
        checkbox: "Safe delete",
        checked: true,
        signal: abort.signal,
        buttons: [{ id: "ok", label: "Delete", primary: true }],
      }).then((r) => (result = r));
    });
    expect(screen.getByRole("checkbox", { name: "Safe delete" })).toBeChecked();
    await act(async () => abort.abort());
    expect(result).toEqual({ button: "dismissed", checked: true });
    expect(screen.queryByRole("dialog")).toBeNull();
  });
});
