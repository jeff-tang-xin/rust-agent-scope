// 协同侧「需求与 Bug」面板：展示绑定测试工作区沉淀的需求与 Bug（同一
// 工作目录下的 .ox/testing/ 数据），并把 open Bug 一键发给会话发起修复。
//
// 数据源与测试工作台完全同源（testListData 按工作区目录解析）——测试绑定
// 本协同后两边读写同一份；未绑定时这里自然是空，空态文案引导去绑定。
// 「发起修复」把 open Bug 组装成修复任务消息发进当前会话：智能体用
// bug_list/req_detail 取证据 → 修复自测 → bug_set_status 置 fixed →
// 测试侧「继续执行」自动回归。

import { useCallback, useEffect, useState } from "react";
import { api, type TestBug, type TestingData } from "../api";

const REFRESH_MS = 10_000;

const SEV_CLASS: Record<string, string> = { P1: "danger", P2: "warn", P3: "" };
const STATUS_LABEL: Record<string, string> = {
  open: "待修复",
  fixed: "已修复",
  closed: "已关闭",
  pass: "通过",
  fail: "失败",
  blocked: "阻塞",
  error: "异常",
  pending: "待执行",
  running: "执行中",
};
const STATUS_CLASS: Record<string, string> = {
  open: "warn",
  fixed: "info",
  closed: "success",
  pass: "success",
  fail: "danger",
  blocked: "warn",
  error: "danger",
  pending: "",
  running: "info",
};

/** 组装「统一发起修复」消息：智能体按 bug_list/req_detail 取证据逐个修。 */
function fixMessage(bugs: TestBug[], reqTitle: (caseId: string) => string): string {
  const lines = bugs.map(
    (b, i) => `${i + 1}. [${b.severity}] ${b.title}（需求：${reqTitle(b.case_id)}，bug_id=${b.id}）`,
  );
  return [
    `请修复以下 ${bugs.length} 个 Bug：`,
    ...lines,
    "",
    "用 bug_list（status=open）看复现步骤与失败证据，逐个定位根因、修改代码并自测（编译/相关测试通过），",
    "然后 bug_set_status 置 fixed 并写清 fix_note——测试侧会自动回归。全部处理完后汇报每个 Bug 的根因与修法。",
  ].join("\n");
}

export default function CollabTestPanel({
  workspaceId,
  busy,
  onSend,
  onCollapse,
}: {
  workspaceId: string;
  /** 会话运行中：发起修复禁用（避免与当前 turn 交叉）。 */
  busy: boolean;
  onSend: (text: string) => void;
  onCollapse: () => void;
}) {
  const [data, setData] = useState<TestingData | null>(null);
  const [error, setError] = useState("");

  const reload = useCallback(async () => {
    try {
      setData(await api.testListData(workspaceId));
      setError("");
    } catch (e) {
      setError(String(e));
    }
  }, [workspaceId]);

  useEffect(() => {
    void reload();
    const timer = window.setInterval(() => void reload(), REFRESH_MS);
    return () => window.clearInterval(timer);
  }, [reload]);

  const bugs = data?.bugs ?? [];
  const openBugs = bugs.filter((b) => b.status === "open");
  const reqTitle = (caseId: string) => {
    const c = data?.cases.find((x) => x.id === caseId);
    return data?.requirements.find((r) => r.id === c?.requirement_id)?.title ?? "未关联";
  };

  return (
    <>
      <aside className="file-panel">
        <div className="file-panel-head">
          <span className="browser-preview-title">🐛 需求与 Bug（来自绑定的测试工作区）</span>
          <span className="spacer" />
          <button className="icon-btn" title="刷新" onClick={() => void reload()}>
            ⟳
          </button>
          <button className="icon-btn" title="收起面板" onClick={onCollapse}>
            »
          </button>
        </div>
        <div className="file-body">
          {error && <div className="file-empty">{error}</div>}
          {!error && data && (
            <>
              {openBugs.length > 0 && (
                <div className="test-section">
                  <div className="test-section-head">
                    <span className="test-section-title">
                      待修复 Bug（{openBugs.length}）
                    </span>
                    <span className="spacer" />
                    <button
                      className="btn primary"
                      disabled={busy}
                      title={
                        busy
                          ? "会话运行中，等当前回合结束再发起"
                          : "把全部待修复 Bug 发给智能体，逐个修复并回归"
                      }
                      onClick={() => onSend(fixMessage(openBugs, reqTitle))}
                    >
                      ⚒ 发起修复
                    </button>
                  </div>
                  <div className="test-list">
                    {openBugs.map((b) => (
                      <div key={b.id} className="test-item">
                        <div className="test-item-main">
                          <div className="test-item-title" title={b.steps}>
                            <span className={`badge ${SEV_CLASS[b.severity] ?? ""}`}>
                              {b.severity}
                            </span>{" "}
                            {b.title}
                          </div>
                          <div className="test-item-sub">需求：{reqTitle(b.case_id)}</div>
                        </div>
                      </div>
                    ))}
                  </div>
                </div>
              )}

              <div className="test-section">
                <div className="test-section-head">
                  <span className="test-section-title">全部 Bug（{bugs.length}）</span>
                </div>
                {bugs.length === 0 ? (
                  <div className="file-empty">
                    暂无需求与 Bug。测试工作区绑定本协同后，其需求、用例与 Bug
                    会出现在这里（测试侧执行产出 Bug → 这里发起修复 → 置 fixed →
                    测试自动回归）。
                  </div>
                ) : (
                  <div className="test-list">
                    {bugs.map((b) => (
                      <div key={b.id} className="test-item">
                        <div className="test-item-main">
                          <div className="test-item-title" title={`${b.steps}\n——\n${b.fix_note}`}>
                            <span className={`badge ${SEV_CLASS[b.severity] ?? ""}`}>
                              {b.severity}
                            </span>{" "}
                            <span className={`badge ${STATUS_CLASS[b.status] ?? ""}`}>
                              {STATUS_LABEL[b.status] ?? b.status}
                            </span>{" "}
                            {b.title}
                          </div>
                          <div className="test-item-sub">
                            需求：{reqTitle(b.case_id)}
                            {b.fix_note ? ` · 修复：${b.fix_note.slice(0, 40)}` : ""}
                          </div>
                        </div>
                      </div>
                    ))}
                  </div>
                )}
              </div>

              <div className="test-section">
                <div className="test-section-head">
                  <span className="test-section-title">
                    需求（{data.requirements.length}）
                  </span>
                </div>
                {data.requirements.length === 0 ? (
                  <div className="file-empty">
                    暂无需求。会话里讨论收敛后让智能体用 req_create 沉淀。
                  </div>
                ) : (
                  <div className="test-list">
                    {data.requirements.map((r) => {
                      const cases = data.cases.filter((c) => c.requirement_id === r.id);
                      const caseIds = new Set(cases.map((c) => c.id));
                      const open = bugs.filter(
                        (b) => caseIds.has(b.case_id) && b.status === "open",
                      ).length;
                      return (
                        <div key={r.id} className="test-item">
                          <div className="test-item-main">
                            <div className="test-item-title" title={r.title}>
                              {r.origin === "collab" && (
                                <span className="test-origin-tag">协同</span>
                              )}{" "}
                              {r.title}
                            </div>
                            <div className="test-item-sub">
                              {cases.length} 条用例 · open Bug {open}
                            </div>
                          </div>
                        </div>
                      );
                    })}
                  </div>
                )}
              </div>
            </>
          )}
        </div>
      </aside>
    </>
  );
}
