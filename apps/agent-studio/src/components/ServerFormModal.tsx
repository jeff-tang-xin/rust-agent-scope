// SSH 服务器表单模态（仅设置页使用）：连接档案（ops 工作区的 remote_shell /
// 文件操作按名字引用）。allow_legacy_rsa 默认关闭，仅此服务器生效。

import { useEffect, useState } from "react";
import { api, type ServerConfig } from "../api";
import PasteButton from "./PasteButton";

export default function ServerFormModal({
  initial,
  existingNames,
  onClose,
  onSaved,
}: {
  /** 编辑时传现有配置；新建传 null。 */
  initial: ServerConfig | null;
  existingNames: string[];
  onClose: () => void;
  onSaved: (msg: string) => void;
}) {
  const editing = Boolean(initial?.name);
  const [draft, setDraft] = useState<ServerConfig>(
    initial ?? { name: "", host: "", port: 22, username: "root", password: "", description: "" },
  );
  const [error, setError] = useState("");
  const [saving, setSaving] = useState(false);

  const set = (patch: Partial<ServerConfig>) => setDraft({ ...draft, ...patch });

  const save = async () => {
    if (saving) return;
    setSaving(true);
    try {
      await api.saveServer(draft);
      onSaved(`服务器 ${draft.name} 已${editing ? "更新" : "创建"}`);
    } catch (e) {
      setError(String(e));
      setSaving(false);
    }
  };

  // Esc 关闭（保存中不响应）；遮罩点击关闭由 mask 的 onClick 承担。
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !saving) onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose, saving]);

  return (
    <div className="modal-mask" onClick={() => !saving && onClose()}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h2>{editing ? `编辑服务器 · ${draft.name}` : "新建服务器连接"}</h2>
        {error && <div className="notice error">{error}</div>}

        <div className="row">
          <div>
            <div className="lbl-row">
              <label className="lbl">名称</label>
              <PasteButton onPaste={(t) => set({ name: t })} />
            </div>
            <input
              className="field"
              value={draft.name}
              disabled={editing || existingNames.includes(draft.name)}
              placeholder="工作区按名字引用"
              onChange={(e) => set({ name: e.target.value })}
            />
            {(editing || existingNames.includes(draft.name)) && (
              <div className="desc" style={{ marginTop: 2 }}>
                {editing ? "名称创建后不可修改" : "该名称已存在"}
              </div>
            )}
          </div>
          <div>
            <div className="lbl-row">
              <label className="lbl">主机</label>
              <PasteButton onPaste={(t) => set({ host: t })} />
            </div>
            <input
              className="field"
              value={draft.host}
              placeholder="10.0.0.8"
              onChange={(e) => set({ host: e.target.value })}
            />
          </div>
          <div>
            <label className="lbl">端口</label>
            <input
              className="field"
              type="number"
              min={0}
              max={65535}
              value={draft.port}
              onChange={(e) => set({ port: Number(e.target.value) || 22 })}
            />
          </div>
        </div>

        <div className="row">
          <div>
            <div className="lbl-row">
              <label className="lbl">用户名</label>
              <PasteButton onPaste={(t) => set({ username: t })} />
            </div>
            <input
              className="field"
              value={draft.username}
              onChange={(e) => set({ username: e.target.value })}
            />
          </div>
          <div>
            <div className="lbl-row">
              <label className="lbl">密码（留空走本机 ssh agent / 默认密钥）</label>
              <PasteButton onPaste={(t) => set({ password: t })} />
            </div>
            <input
              className="field"
              type="password"
              value={draft.password}
              onChange={(e) => set({ password: e.target.value })}
            />
          </div>
        </div>

        <div className="lbl-row">
          <label className="lbl">描述（可选）</label>
          <PasteButton onPaste={(t) => set({ description: t })} />
        </div>
        <input
          className="field"
          value={draft.description}
          onChange={(e) => set({ description: e.target.value })}
        />

        <label className="member-check">
          <input
            type="checkbox"
            checked={draft.allow_legacy_rsa ?? false}
            onChange={(e) => set({ allow_legacy_rsa: e.target.checked })}
          />
          兼容旧服务器（允许 RSA/SHA-1）
        </label>
        <div className="desc">
          默认关闭，仅此服务器生效。只追加 ssh-rsa，不启用 ssh-dss；建议升级服务端 SSH。保存后断开并重新连接生效。
        </div>

        <div className="modal-actions">
          <button
            className="btn"
            onClick={() => void save()}
            disabled={!draft.name || !draft.host.trim() || saving}
          >
            {saving ? "保存中…" : editing ? "保存修改" : "创建"}
          </button>
          <button className="btn ghost" onClick={onClose} disabled={saving}>
            取消
          </button>
        </div>
      </div>
    </div>
  );
}
