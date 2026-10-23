// 粘贴按钮：走 Tauri 剪贴板 API（不依赖 WebView 原生编辑行为——
// WebView2 环境下 Ctrl+V/右键粘贴可能不可用，这是确定性兜底路径）。

import { readText } from "@tauri-apps/plugin-clipboard-manager";

export default function PasteButton({ onPaste }: { onPaste: (text: string) => void }) {
  const paste = async () => {
    try {
      const text = await readText();
      if (text) onPaste(text.trim());
    } catch {
      /* 剪贴板为空或读取失败：静默（按钮本就是兜底路径） */
    }
  };
  return (
    <button
      type="button"
      className="btn ghost paste-btn"
      title="Paste from clipboard"
      onClick={() => void paste()}
    >
      Paste
    </button>
  );
}
