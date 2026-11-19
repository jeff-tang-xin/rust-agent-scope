/**
 * Agent messages: Markdown → sanitized HTML (marked + DOMPurify). Fenced code is
 * highlighted and gets Copy / Insert / Apply buttons (see codeActions.tsx); inline
 * code naming a file of the task opens it.
 * Mermaid fences become `[data-mermaid]` placeholders drawn by the Markdown viewer's
 * shared renderer after each render; the block keeps a Copy button of its own, since
 * the drawn diagram hides the code block, and `onDiagramDrawn` lets the conversation
 * follow the end when a diagram draws after the last streamed chunk.
 */
import { useLayoutEffect, useMemo, useRef } from "react";
import { Marked } from "marked";
import DOMPurify from "dompurify";
import { highlightInto } from "@/editor/lsp/markdown";
import { iconNode } from "@/editor/lsp/iconMarkup";
import { call, commands } from "@/platform/ipc";
import { tx } from "@/platform/i18n";
import { openExternalUrl } from "@/platform/openUrl";
import { copyText } from "@/ui";
import { editorService } from "@/workbench/contributions";
import { hasMermaid, renderMermaidIn } from "@/features/viewers/markdown/mermaid";
import { applyCodeBlock, insertCodeAtCaret } from "./codeActions";
import { fileHref, pathLink, type PathLink } from "./conversation";
import { loadImages } from "./markdownImages";
import styles from "./Session.module.css";

const escapeHtml = (s: string) =>
  s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");

const marked = new Marked({
  gfm: true,
  breaks: true,
  renderer: {
    code({ text, lang }) {
      if (lang?.trim().split(/\s+/)[0]?.toLowerCase() === "mermaid")
        return `<div data-mermaid=""><pre><code class="language-mermaid">${escapeHtml(text)}</code></pre></div>\n`;
      // Like marked's default, but keeping the fence's full info string (`ts src/a.ts`)
      // for the Apply action, which always previews a diff before writing.
      const info = lang?.trim() ?? "";
      const first = info.split(/\s+/)[0] ?? "";
      const cls = first ? ` class="language-${escapeHtml(first)}"` : "";
      const attr = info ? ` data-info="${escapeHtml(info)}"` : "";
      return `<pre><code${cls}${attr}>${escapeHtml(text.replace(/\n$/, ""))}\n</code></pre>\n`;
    },
  },
});

/** Rendered messages by text, least recently used first: reopening a conversation renders it again. */
const rendered = new Map<string, string>();
const RENDERED_MAX = 500;

let purifier: ReturnType<typeof DOMPurify> | null = null;

function purify(): ReturnType<typeof DOMPurify> {
  if (purifier) return purifier;
  const p = DOMPurify(window);
  const sources = new WeakMap<Node, string>();
  // Preserve image-only file URLs without relaxing URI sanitizing for links or other tags.
  p.addHook("beforeSanitizeAttributes", (node) => {
    if (node.tagName !== "IMG") return;
    sources.set(node, node.getAttribute("src") ?? "");
    node.removeAttribute("src");
  });
  p.addHook("afterSanitizeAttributes", (node) => {
    const src = sources.get(node);
    if (src) node.setAttribute("data-image-src", src);
  });
  purifier = p;
  return p;
}

export function markdownHtml(text: string, cache = true): string {
  if (!cache) return renderHtml(text);
  let html = rendered.get(text);
  if (html === undefined) {
    html = renderHtml(text);
    if (rendered.size >= RENDERED_MAX) rendered.delete(rendered.keys().next().value!);
  } else rendered.delete(text);
  rendered.set(text, html);
  return html;
}

function renderHtml(text: string): string {
  const html = marked.parse(text, { async: false }) as string;
  // Replies can quote repository content. Styles would reach the whole IDE, and the click
  // handlers below trust `data-copy` / `data-path`, which only `enhance` may set.
  const root = purify().sanitize(html, {
    USE_PROFILES: { html: true },
    RETURN_DOM: true,
    FORBID_TAGS: ["style", "form", "source"],
    FORBID_ATTR: ["style", "srcset"],
    ALLOW_DATA_ATTR: false,
    // The mermaid placeholder and the fence info string written by the renderer above;
    // other data attributes quoted in a reply stay out.
    ADD_ATTR: ["data-mermaid", "data-info"],
  }) as HTMLElement;
  root.querySelectorAll("li").forEach(wrapLooseText);
  // WebKit cuts controls inside <pre> out of a text selection, even when transparent.
  root.querySelectorAll("pre").forEach((pre) => {
    if (pre.parentElement?.hasAttribute("data-mermaid")) return;
    const block = document.createElement("div");
    block.className = styles.codeBlock!;
    pre.replaceWith(block);
    block.appendChild(pre);
  });
  return root.innerHTML;
}

const BLOCK = new Set(["UL", "OL", "P", "PRE", "BLOCKQUOTE", "TABLE", "HR", "DIV", "H1", "H2", "H3", "H4", "H5", "H6"]);

/**
 * Puts the text of an item that also holds a nested list (or other blocks) in its own
 * block. Otherwise WebKit paints a stray sliver of selection from the end of that text
 * to the right edge when a selection ends there.
 */
function wrapLooseText(li: Element) {
  const nodes = [...li.childNodes];
  if (!nodes.some((n) => BLOCK.has(n.nodeName))) return;
  let run: ChildNode[] = [];
  const flush = () => {
    if (run.some((n) => n.nodeType !== Node.TEXT_NODE || n.textContent!.trim())) {
      const div = li.ownerDocument.createElement("div");
      run[0]!.before(div);
      div.append(...run);
    }
    run = [];
  };
  for (const n of nodes) {
    if (BLOCK.has(n.nodeName)) flush();
    else run.push(n);
  }
  flush();
}

/** Whether a path names an existing file, per path (agents name the same files again and again). */
const existing = new Map<string, Promise<boolean>>();
function fileExists(path: string): Promise<boolean> {
  let known = existing.get(path);
  if (!known) {
    known = call(commands.fsProbe(path)).then(
      () => true,
      () => false,
    );
    existing.set(path, known);
  }
  return known;
}

export function openPathLink(link: PathLink) {
  const selection = link.line === undefined ? undefined : { line: link.line, col: link.col ?? 0 };
  void editorService().openFile(link.path, { selection, focus: true });
}

/** Highlighting, Copy buttons and file links over freshly rendered `root`. */
function enhance(root: HTMLElement, cwd: string, images: Map<string, Promise<string>>): () => void {
  let alive = true;
  const disposeImages = loadImages(root, cwd, images);
  const copies: HTMLButtonElement[] = [];
  root.querySelectorAll<HTMLElement>("pre > code").forEach((code) => {
    const pre = code.parentElement!;
    const lang = [...code.classList].find((c) => c.startsWith("language-"))?.slice("language-".length) ?? null;
    const text = (code.textContent ?? "").replace(/\n+$/, "");
    code.textContent = text;
    const copy = document.createElement("button");
    copy.type = "button";
    copy.className = styles.codeCopy!;
    const label = tx("Copy Code");
    copy.title = label;
    copy.setAttribute("aria-label", label);
    copy.appendChild(iconNode("copy", 14));
    copy.dataset.copy = text;
    const block = pre.parentElement!;
    block.appendChild(copy);
    copies.push(copy);
    if (!block.hasAttribute("data-mermaid")) {
      // Insert / Apply carry the source text (highlighting rewrites the code's DOM).
      const action = (icon: string, label: string, extra: string) => {
        const button = document.createElement("button");
        button.type = "button";
        button.className = `${styles.codeCopy!} ${extra}`;
        button.title = label;
        button.setAttribute("aria-label", label);
        button.appendChild(iconNode(icon, 14));
        block.appendChild(button);
        copies.push(button);
        return button;
      };
      const insert = action("paste", tx("Insert at Caret"), styles.codeInsert!);
      insert.dataset.insert = text;
      const apply = action("edit", tx("Apply to File"), styles.codeApply!);
      apply.dataset.apply = text;
      apply.dataset.info = code.getAttribute("data-info") ?? "";
      void highlightInto(code, text, lang);
    }
  });
  root.querySelectorAll<HTMLElement>("code").forEach((code) => {
    if (code.parentElement?.tagName === "PRE" || code.closest("a")) return;
    const link = pathLink(code.textContent ?? "", cwd);
    if (!link) return;
    void fileExists(link.path).then((exists) => {
      if (!alive || !exists) return;
      code.classList.add(styles.fileLink!);
      code.dataset.path = link.path;
      if (link.line !== undefined) code.dataset.line = String(link.line);
      if (link.col !== undefined) code.dataset.col = String(link.col);
      code.title = tx`Open ${link.path}`;
    });
  });
  return () => {
    alive = false;
    copies.forEach((copy) => copy.remove());
    disposeImages();
  };
}

export function Markdown({
  text,
  cwd,
  onDiagramDrawn,
}: {
  text: string;
  cwd: string;
  /** A diagram drew after the message rendered: the conversation may follow the end. */
  onDiagramDrawn?: () => void;
}) {
  const html = useMemo(() => markdownHtml(text, true), [text]);
  const ref = useRef<HTMLDivElement>(null);
  const images = useRef(new Map<string, Promise<string>>());
  /** The last drawing of each mermaid block, stood in while a changed one renders. */
  const diagrams = useRef<Node[]>([]);
  useLayoutEffect(() => (ref.current ? enhance(ref.current, cwd, images.current) : undefined), [html, cwd]);
  // Mermaid diagrams: before paint, so cached ones don't flash their source. Only a
  // draw that lands after the render needs the follow — cached ones are in place by
  // the time the conversation's own render pass scrolls.
  useLayoutEffect(() => {
    const root = ref.current;
    if (!root || !hasMermaid(root)) return;
    const pass = renderMermaidIn(root, diagrams.current);
    let live = true;
    if (pass.async) void pass.done.then(() => live && onDiagramDrawn?.());
    return () => {
      live = false;
      pass.cancel();
    };
  }, [html, onDiagramDrawn]);
  return (
    <div
      ref={ref}
      className={styles.markdown}
      onClick={(e) => {
        const target = e.target as HTMLElement;
        const insert = target.closest<HTMLElement>("button[data-insert]");
        if (insert) {
          insertCodeAtCaret(insert.dataset.insert ?? "");
          return;
        }
        const apply = target.closest<HTMLElement>("button[data-apply]");
        if (apply) {
          void applyCodeBlock(apply.dataset.apply ?? "", apply.dataset.info ?? "", cwd);
          return;
        }
        const copy = target.closest<HTMLElement>("button[data-copy]");
        if (copy) {
          void copyText(copy.dataset.copy ?? "");
          return;
        }
        const file = target.closest<HTMLElement>("code[data-path]");
        if (file && !window.getSelection()?.toString()) {
          const line = file.dataset.line === undefined ? undefined : Number(file.dataset.line);
          const col = file.dataset.col === undefined ? undefined : Number(file.dataset.col);
          openPathLink({ path: file.dataset.path!, line, col });
          return;
        }
        const a = target.closest("a");
        if (!a) return;
        e.preventDefault();
        const href = a.getAttribute("href") ?? "";
        if (/^https?:/i.test(href)) void openExternalUrl(href);
        else {
          const link = fileHref(href, cwd);
          if (link) openPathLink(link);
        }
      }}
      dangerouslySetInnerHTML={{ __html: html }}
    />
  );
}
