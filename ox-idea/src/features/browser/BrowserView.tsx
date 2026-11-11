/**
 * Browser tool window: the headless browser the agent drives, streamed as JPEG
 * frames over a Channel and drawn on a canvas that forwards mouse, keyboard,
 * wheel and paste back through CDP — what the agent sees is what you control.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { Channel } from "@tauri-apps/api/core";
import { call, commands } from "@/platform/ipc";
import type { BrowserFrame } from "@/bindings";
import { IconButton } from "@/ui";
import styles from "./Browser.module.css";

/** CDP modifier bitmask (see ide dispatch_key): Alt=1, Ctrl=2, Meta=4, Shift=8. */
const MOD_ALT = 1;
const MOD_CTRL = 2;
const MOD_META = 4;
const MOD_SHIFT = 8;

/** Windows virtual key codes of the keys pages handle specially; printable
 * characters go through `char` events instead and pass 0. */
const VK: Record<string, number> = {
  Enter: 13,
  Backspace: 8,
  Tab: 9,
  Escape: 27,
  Space: 32,
  PageUp: 33,
  PageDown: 34,
  End: 35,
  Home: 36,
  ArrowLeft: 37,
  ArrowUp: 38,
  ArrowRight: 39,
  ArrowDown: 40,
  Delete: 46,
  ...Object.fromEntries(Array.from({ length: 12 }, (_, i) => [`F${i + 1}`, 112 + i])),
};

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

export function BrowserView() {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  /** The latest frame's device size: input coordinates are scaled by it. */
  const device = useRef({ width: 0, height: 0 });
  const [url, setUrl] = useState("");
  const [error, setError] = useState<string | null>(null);
  /** Attached once the frame channel is live; frames flow until unmount. */
  const [live, setLive] = useState(false);

  useEffect(() => {
    const channel = new Channel<BrowserFrame>();
    channel.onmessage = (frame) => {
      const canvas = canvasRef.current;
      if (!canvas) return;
      const width = frame.device_width ?? 0;
      const height = frame.device_height ?? 0;
      if (width > 0 && height > 0 && (canvas.width !== width || canvas.height !== height)) {
        canvas.width = width;
        canvas.height = height;
        device.current = { width, height };
      }
      const image = new Image();
      image.onload = () => canvas.getContext("2d")?.drawImage(image, 0, 0);
      image.src = `data:image/jpeg;base64,${frame.data}`;
      if (!live) setLive(true);
    };
    call(commands.browserPanelAttach(channel))
      .then((state) => {
        setUrl(state.url);
        setError(null);
      })
      .catch((e) => setError(message(e)));
    // Pages navigate themselves (links, the agent): keep the address bar honest.
    const poll = setInterval(() => {
      call(commands.browserPanelUrl())
        .then(setUrl)
        .catch(() => {});
    }, 2000);
    return () => {
      clearInterval(poll);
      void commands.browserPanelDetach().catch(() => {});
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  /** Page CSS pixels → device pixels of the current frame. */
  const toPage = useCallback((clientX: number, clientY: number) => {
    const canvas = canvasRef.current!;
    const rect = canvas.getBoundingClientRect();
    const { width, height } = device.current;
    const scaleX = rect.width > 0 ? width / rect.width : 1;
    const scaleY = rect.height > 0 ? height / rect.height : 1;
    return { x: (clientX - rect.left) * scaleX, y: (clientY - rect.top) * scaleY };
  }, []);

  const mouse = useCallback(
    (eventType: string, e: React.MouseEvent, button: string | null = null, clickCount = 0) => {
      const { x, y } = toPage(e.clientX, e.clientY);
      void call(commands.browserPanelMouse(eventType, x, y, button, clickCount, 0, 0)).catch(() => {});
    },
    [toPage],
  );

  const wheel = useCallback(
    (e: React.WheelEvent) => {
      const { x, y } = toPage(e.clientX, e.clientY);
      void call(commands.browserPanelMouse("wheel", x, y, null, 0, e.deltaX, e.deltaY)).catch(() => {});
    },
    [toPage],
  );

  const key = useCallback((eventType: "keydown" | "keyup", e: React.KeyboardEvent) => {
    const modifiers =
      (e.altKey ? MOD_ALT : 0) | (e.ctrlKey ? MOD_CTRL : 0) | (e.metaKey ? MOD_META : 0) | (e.shiftKey ? MOD_SHIFT : 0);
    void call(
      commands.browserPanelKey(eventType, e.key, e.code, null, modifiers, VK[e.key] ?? 0),
    ).catch(() => {});
  }, []);

  const onKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      // The canvas keeps focus while typing; don't let the workbench act on it.
      e.preventDefault();
      e.stopPropagation();
      key("keydown", e);
      // A printable character (no shortcuts): CDP pages want a `char` with text.
      if (e.key.length === 1 && !e.ctrlKey && !e.altKey && !e.metaKey) {
        void call(commands.browserPanelKey("char", e.key, e.code, e.key, e.shiftKey ? MOD_SHIFT : 0, 0)).catch(() => {});
      }
    },
    [key],
  );

  const onPaste = useCallback((e: React.ClipboardEvent) => {
    const text = e.clipboardData.getData("text");
    if (text) void call(commands.browserPanelInsertText(text)).catch(() => {});
  }, []);

  const navigate = useCallback((target: string) => {
    const trimmed = target.trim();
    if (!trimmed) return;
    // A bare host or phrase is a search/query URL, like browsers treat the box.
    const withScheme = /^[a-z][\w+.-]*:/i.test(trimmed) || trimmed.startsWith("localhost")
      ? trimmed
      : /[\s]/.test(trimmed) || !trimmed.includes(".")
        ? `https://www.bing.com/search?q=${encodeURIComponent(trimmed)}`
        : `https://${trimmed}`;
    call(commands.browserPanelNavigate(withScheme))
      .then(setUrl)
      .catch((e) => setError(message(e)));
  }, []);

  const history = useCallback((action: string) => {
    void call(commands.browserPanelHistory(action)).catch(() => {});
  }, []);

  return (
    <div className={styles.view}>
      <div className={styles.toolbar}>
        <IconButton icon="arrow-left" title="Back" size="small" onClick={() => history("back")} />
        <IconButton icon="arrow-right" title="Forward" size="small" onClick={() => history("forward")} />
        <IconButton icon="refresh" title="Reload" size="small" onClick={() => history("reload")} />
        <input
          className={styles.url}
          value={url}
          placeholder={error ?? (live ? undefined : "Starting the browser…")}
          onChange={(e) => setUrl(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") navigate(e.currentTarget.value);
          }}
          spellCheck={false}
        />
      </div>
      <div className={styles.stage}>
        <canvas
          ref={canvasRef}
          className={styles.page}
          tabIndex={0}
          width={1280}
          height={720}
          onMouseDown={(e) => mouse("pressed", e, e.button === 1 ? "middle" : e.button === 2 ? "right" : "left", 1)}
          onMouseUp={(e) => mouse("released", e, e.button === 1 ? "middle" : e.button === 2 ? "right" : "left", 1)}
          onMouseMove={(e) => mouse("moved", e)}
          onWheel={wheel}
          onKeyDown={onKeyDown}
          onKeyUp={(e) => {
            e.preventDefault();
            key("keyup", e);
          }}
          onPaste={onPaste}
          onContextMenu={(e) => e.preventDefault()}
        />
      </div>
    </div>
  );
}
