// 通用数据查看/编辑弹层：黑板条目、记忆文件、文件面板预览共用。
//
// 按内容类型渲染（解决「序列化问题」——原始文本一坨不可读）：
// - markdown：react-markdown + GFM 渲染（记忆台账、黑板条目）
// - json：parse 后缩进格式化（parse 失败回退原文）
// - code：等宽 pre（html/代码/其它）
// editable + onSave 时提供编辑切换（textarea 草稿，保存回调由挂载方实现）。

import { useEffect, useState } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";

export type ViewMode = "markdown" | "json" | "code";

/** 按文件名/标题后缀推断渲染方式；无后缀按 code。 */
export function detectMode(title: string): ViewMode {
  const p = title.toLowerCase();
  if (p.endsWith(".md") || p.endsWith(".markdown")) return "markdown";
  if (p.endsWith(".json") || p.endsWith(".jsonc")) return "json";
  if (p.endsWith(".html") || p.endsWith(".htm") || p.endsWith(".xml")) return "code";
  return "code";
}

/** JSON 尽量格式化；非法 JSON 原样返回。 */
export function prettyJson(content: string): string {
  try {
    return JSON.stringify(JSON.parse(content), null, 2);
  } catch {
    return content;
  }
}

export default function DataViewerModal({
  title,
  content,
  mode,
  editable = false,
  onSave,
  onClose,
}: {
  title: string;
  content: string;
  /** 缺省按 title 后缀推断。 */
  mode?: ViewMode;
  /** 提供保存回调时显示「编辑」切换。 */
  editable?: boolean;
  onSave?: (content: string) => Promise<void> | void;
  onClose: () => void;
}) {
  const m = mode ?? detectMode(title);
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(content);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    setDraft(content);
    setEditing(false);
    setSaving(false);
  }, [content, title]);

  const save = async () => {
    if (!onSave || saving) return;
    setSaving(true);
    try {
      await onSave(draft);
      setEditing(false);
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="drawer-mask" onClick={onClose}>
      <div className="drawer dvm" onClick={(e) => e.stopPropagation()}>
        <div className="dvm-head">
          <h2 title={title}>{title}</h2>
          <span className="spacer" />
          {editable && onSave && !editing && (
            <button className="btn ghost" onClick={() => setEditing(true)}>
              编辑
            </button>
          )}
          {editing && (
            <>
              <button className="btn ghost" onClick={() => { setEditing(false); setDraft(content); }}>
                取消
              </button>
              <button className="btn" disabled={saving || draft === content} onClick={() => void save()}>
                {saving ? "保存中…" : "保存"}
              </button>
            </>
          )}
          <button className="btn ghost" onClick={onClose}>
            关闭
          </button>
        </div>

        {editing ? (
          <textarea
            className="dvm-edit"
            value={draft}
            autoFocus
            spellCheck={false}
            onChange={(e) => setDraft(e.target.value)}
          />
        ) : m === "markdown" ? (
          <div className="dvm-md">
            <Markdown remarkPlugins={[remarkGfm]}>{content}</Markdown>
          </div>
        ) : (
          <pre className="dvm-code">{m === "json" ? prettyJson(content) : content}</pre>
        )}
      </div>
    </div>
  );
}
