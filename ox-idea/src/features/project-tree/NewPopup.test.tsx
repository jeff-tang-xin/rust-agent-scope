import { afterEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen } from "@testing-library/react";
import { NewPopupHost } from "./NewPopup";
import { closeNewPopup, openNewPopup, useNewPopup } from "./newPopupModel";

afterEach(() => act(() => closeNewPopup()));

describe("New popup", () => {
  it("filters file kinds as you type and runs the selection with Enter", () => {
    const python = vi.fn();
    const css = vi.fn();
    render(<NewPopupHost />);
    act(() =>
      openNewPopup({ x: 10, y: 10 }, [
        { id: "file", label: "File", icon: "file", run: () => undefined },
        { kind: "separator" },
        { id: "py", label: "Python File", icon: "file-python", run: python },
        { id: "css", label: "Stylesheet", icon: "file-css", run: css },
      ]),
    );
    const search = screen.getByRole("textbox", { name: "Search for file type" });
    expect(search).toHaveAttribute("placeholder", "Search for file type");
    expect(screen.getAllByRole("option").map((o) => o.textContent)).toEqual(["File", "Python File", "Stylesheet"]);

    fireEvent.change(search, { target: { value: "sty" } });
    expect(screen.getAllByRole("option").map((o) => o.textContent)).toEqual(["Stylesheet"]);
    fireEvent.keyDown(search, { key: "Enter" });
    expect(css).toHaveBeenCalledOnce();
    expect(useNewPopup.getState().request).toBeNull();
  });

  it("moves the selection with the arrow keys, skipping separators", () => {
    const python = vi.fn();
    render(<NewPopupHost />);
    act(() =>
      openNewPopup({ x: 10, y: 10 }, [
        { id: "file", label: "File", run: () => undefined },
        { kind: "separator" },
        { id: "py", label: "Python File", run: python },
      ]),
    );
    const search = screen.getByRole("textbox", { name: "Search for file type" });
    fireEvent.keyDown(search, { key: "ArrowDown" });
    fireEvent.keyDown(search, { key: "Enter" });
    expect(python).toHaveBeenCalledOnce();
  });
});
