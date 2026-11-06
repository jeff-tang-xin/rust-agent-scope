// 数据库服务器表单模态（仅设置页使用）：连接档案 + CLI 驱动配置。
// 字段启停/标签/驱动提示按后端 DATABASE_KINDS 注册表元数据走（dbKinds 由页面传入）。

import { useEffect, useState } from "react";
import { api, type DatabaseKindInfo, type DatabaseServerConfig } from "../api";
import PasteButton from "./PasteButton";

export default function DatabaseFormModal({
  initial,
  dbKinds,
  existingNames,
  onClose,
  onSaved,
}: {
  /** 编辑时传现有配置；新建传 null。 */
  initial: DatabaseServerConfig | null;
  /** 数据库类型注册表（后端 DATABASE_KINDS 投影）。 */
  dbKinds: DatabaseKindInfo[];
  existingNames: string[];
  onClose: () => void;
  onSaved: (msg: string) => void;
}) {
  const editing = Boolean(initial?.name);
  const [draft, setDraft] = useState<DatabaseServerConfig>(
    initial ?? {
      name: "",
      db_type: "mysql",
      host: "",
      port: 3306,
      database: "",
      username: "",
      password: "",
      cli_path: "",
      description: "",
    },
  );
  const [error, setError] = useState("");
  const [saving, setSaving] = useState(false);

  const set = (patch: Partial<DatabaseServerConfig>) => setDraft({ ...draft, ...patch });
  const kind = dbKinds.find((k) => k.id === draft.db_type);

  const save = async () => {
    if (saving) return;
    setSaving(true);
    try {
      // 保存后立即验证 CLI 可用性（后端自动探索或用指定路径）——只提示
      // 不阻断：档案先存着，CLI 可以稍后装/再指定。
      const outcome = await api.saveDatabase(draft);
      if (outcome.cli_resolved) {
        onSaved(`数据库 ${draft.name} 已保存，CLI 就绪：${outcome.cli_resolved}`);
      } else if (outcome.cli_error) {
        onSaved(`数据库 ${draft.name} 已保存；注意：${outcome.cli_error}`);
      } else {
        onSaved(`数据库 ${draft.name} 已保存`);
      }
    } catch (e) {
      setError(String(e));
      setSaving(false);
    }
  };

  // 连接表单「探测」按钮：即时验证 CLI 自动探索/指定路径的结果。
  const probeCli = async () => {
    try {
      const path = await api.probeDatabaseCli(draft.db_type, draft.cli_path);
      setError("");
      setDraft({ ...draft });
      onSavedProbe(`CLI 已找到：${path}`);
    } catch (e) {
      setError(String(e));
    }
  };
  // 探测结果只做即时提示，不关闭弹窗——用局部状态而非 onSaved。
  const [probeMsg, setProbeMsg] = useState("");
  const onSavedProbe = (msg: string) => setProbeMsg(msg);

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
        <h2>{editing ? `编辑数据库 · ${draft.name}` : "新建数据库连接"}</h2>
        {error && <div className="notice error">{error}</div>}
        {probeMsg && <div className="notice info">{probeMsg}</div>}

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
            <label className="lbl">类型</label>
            <select
              className="field"
              value={draft.db_type}
              onChange={(e) => {
                const k = dbKinds.find((x) => x.id === e.target.value);
                setDraft({
                  ...draft,
                  db_type: e.target.value,
                  // 切类型自动带出默认端口（无端口类型归 0）。
                  port: k ? k.default_port : draft.port,
                });
              }}
            >
              {dbKinds.map((k) => (
                <option key={k.id} value={k.id}>
                  {k.label}
                </option>
              ))}
            </select>
            {kind && (
              <div className="desc" style={{ margin: "4px 0 0" }}>
                {kind.driver.mode === "Bundled"
                  ? "驱动：内置（无外部依赖）"
                  : `驱动：外部客户端 ${kind.driver.program}（自动探索 PATH 与常见安装目录，也可在下方指定路径）`}
              </div>
            )}
          </div>
        </div>

        <div className="row">
          <div>
            <div className="lbl-row">
              <label className="lbl">{kind && !kind.needs_port ? "数据库文件路径" : "主机"}</label>
              <PasteButton onPaste={(t) => set({ host: t })} />
            </div>
            <input
              className="field"
              value={draft.host}
              placeholder={kind && !kind.needs_port ? "C:\\data\\app.db" : "10.0.0.8"}
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
              disabled={!kind?.needs_port}
              onChange={(e) => set({ port: Number(e.target.value) || 0 })}
            />
          </div>
        </div>

        <div className="row">
          <div>
            <div className="lbl-row">
              <label className="lbl">数据库</label>
              <PasteButton onPaste={(t) => set({ database: t })} />
            </div>
            <input
              className="field"
              value={draft.database}
              disabled={!kind?.needs_database}
              onChange={(e) => set({ database: e.target.value })}
            />
          </div>
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
        </div>

        <div className="lbl-row">
          <label className="lbl">密码（仅存本地配置文件）</label>
          <PasteButton onPaste={(t) => set({ password: t })} />
        </div>
        <input
          className="field"
          type="password"
          value={draft.password}
          onChange={(e) => set({ password: e.target.value })}
        />

        {kind?.driver.mode === "Cli" && (
          <>
            <div className="lbl-row">
              <label className="lbl">
                CLI 路径（可选，留空自动探索 PATH 与常见安装目录）
              </label>
              <PasteButton onPaste={(t) => set({ cli_path: t })} />
            </div>
            <div style={{ display: "flex", gap: 8 }}>
              <input
                className="field"
                style={{ flex: 1 }}
                value={draft.cli_path}
                placeholder="如 D:\\mysql\\bin\\mysql.exe 或 D:\\mysql\\bin"
                onChange={(e) => set({ cli_path: e.target.value })}
              />
              <button className="btn ghost" onClick={() => void probeCli()}>
                探测
              </button>
            </div>
          </>
        )}

        <div className="lbl-row">
          <label className="lbl">描述（可选）</label>
          <PasteButton onPaste={(t) => set({ description: t })} />
        </div>
        <input
          className="field"
          value={draft.description}
          onChange={(e) => set({ description: e.target.value })}
        />

        <div className="modal-actions">
          <button
            className="btn"
            onClick={() => void save()}
            disabled={!draft.name || (draft.db_type !== "sqlite" && !draft.host.trim()) || saving}
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
