// ===========================================================================
// 共享黑板（工作区级、跨会话共享的公共记录本）
// ===========================================================================
//
// 语义对齐 Easy-Claw 平台的 blackboard：所有会话与子代理共用一份「公共记录
// 本」，得出结论/发现/风险时立即登记，动手前先读——并行协作的共享载体，
// 也是子代理被截断时唯一幸存的持久化通道。
//
// 作用域：**工作区级、跨 session 共享**（用户拍板）——不做会话隔离过滤，
// 条目全工作区可见；溯源靠条目的 author/时间戳。
//
// 写入方有两类，并发语义不同：
// - 模型侧（BlackboardAppendTool）：只增不改（append-only），保持记录本
//   语义简单；
// - 用户侧（studio 管理面板）：可删除/修改任意条目（commands 直接调
//   [`BlackboardStore`] 的 delete/update）。
//
// 存储：`<工作区根>/.ox/blackboard.json`（JSON 数组 + 原子重写）。不用
// 追加式 JSONL——用户侧要随机删改，整文件重写最简单；条目量级（几十上百）
// 下重写成本可忽略。并发用进程级全局锁串行化（studio 单进程，模型工具与
// 管理命令可能持有不同 store 实例，实例内锁不够）。

use agentscope_core::message::ToolResultState;
use agentscope_core::tool::{Tool, ToolOutput};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// 黑板条目类型（对齐 Easy-Claw：note/finding/risk/conclusion；未知值按 note）。
pub const BLACKBOARD_TYPES: [&str; 4] = ["note", "finding", "risk", "conclusion"];

/// 单条黑板记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlackboardEntry {
    /// 自增 id（全文件最大值 +1，删除后不复用）。
    pub id: u64,
    /// 登记时间（Unix 毫秒）。
    pub ts: i64,
    /// 条目类型：note / finding / risk / conclusion。
    pub kind: String,
    /// 登记者（模型自报的 agent 名/label；用户面板操作为 "user"；可为空）。
    #[serde(default)]
    pub author: String,
    /// 正文：一句话说清「是什么 + 对别人意味着什么」。
    pub content: String,
}

/// 归一化条目类型：不在四值内的按 note 处理。
fn normalize_kind(kind: &str) -> String {
    let k = kind.trim().to_ascii_lowercase();
    if BLACKBOARD_TYPES.contains(&k.as_str()) {
        k
    } else {
        "note".to_string()
    }
}

/// 进程级全局锁：跨 store 实例串行化「读-改-写」临界区。
fn blackboard_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// 文件结构：条目数组 + id 水位。id 用持久化水位自增（删除不复用）——
/// 只看现存条目 max+1 会在删除最大 id 条目后复用其 id。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct BlackboardFile {
    next_id: u64,
    entries: Vec<BlackboardEntry>,
}

impl Default for BlackboardFile {
    fn default() -> Self {
        Self { next_id: 1, entries: Vec::new() }
    }
}

/// 黑板存储：锚定一个 JSON 文件（约定 `<工作区根>/.ox/blackboard.json`）。
#[derive(Clone)]
pub struct BlackboardStore {
    path: PathBuf,
}

impl BlackboardStore {
    /// 锚定到指定 JSON 文件路径。
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// 追加一条记录，返回带 id/时间戳的完整条目。
    pub fn append(&self, kind: &str, author: &str, content: &str) -> Result<BlackboardEntry, String> {
        let content = content.trim();
        if content.is_empty() {
            return Err("content is required".to_string());
        }
        let _guard = blackboard_lock().lock().map_err(|_| "blackboard lock poisoned")?;
        let mut file = self.load_locked()?;
        let entry = BlackboardEntry {
            id: file.next_id,
            ts: now_millis(),
            kind: normalize_kind(kind),
            author: author.trim().to_string(),
            content: content.to_string(),
        };
        file.next_id += 1;
        file.entries.push(entry.clone());
        self.save_locked(&file)?;
        Ok(entry)
    }

    /// 列出条目，最新在前；`limit` 截取最近 N 条（None = 全量）。
    pub fn list(&self, limit: Option<usize>) -> Result<Vec<BlackboardEntry>, String> {
        let _guard = blackboard_lock().lock().map_err(|_| "blackboard lock poisoned")?;
        let mut entries = self.load_locked()?.entries;
        entries.reverse();
        if let Some(n) = limit {
            entries.truncate(n);
        }
        Ok(entries)
    }

    /// 删除指定条目；返回是否真的删了（id 不存在 = false）。
    pub fn delete(&self, id: u64) -> Result<bool, String> {
        let _guard = blackboard_lock().lock().map_err(|_| "blackboard lock poisoned")?;
        let mut file = self.load_locked()?;
        let before = file.entries.len();
        file.entries.retain(|e| e.id != id);
        if file.entries.len() == before {
            return Ok(false);
        }
        self.save_locked(&file)?;
        Ok(true)
    }

    /// 修改指定条目的类型与正文（None = 保持不变）；返回更新后的条目。
    pub fn update(
        &self,
        id: u64,
        kind: Option<&str>,
        content: Option<&str>,
    ) -> Result<BlackboardEntry, String> {
        let _guard = blackboard_lock().lock().map_err(|_| "blackboard lock poisoned")?;
        let mut file = self.load_locked()?;
        let entry = file
            .entries
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or_else(|| format!("entry #{id} not found"))?;
        if let Some(k) = kind {
            entry.kind = normalize_kind(k);
        }
        if let Some(c) = content {
            let c = c.trim();
            if c.is_empty() {
                return Err("content cannot be empty".to_string());
            }
            entry.content = c.to_string();
        }
        let updated = entry.clone();
        self.save_locked(&file)?;
        Ok(updated)
    }

    /// 读文件（调用方持锁）。文件不存在 = 空黑板（首次使用）。
    fn load_locked(&self) -> Result<BlackboardFile, String> {
        let bytes = match std::fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(BlackboardFile::default())
            }
            Err(e) => return Err(format!("read blackboard: {e}")),
        };
        serde_json::from_slice(&bytes).map_err(|e| format!("parse blackboard: {e}"))
    }

    /// 原子写：临时文件 + 删除旧文件 + rename（Windows rename 不能覆盖）。
    /// 崩溃窗口：rename 前旧文件已删——最坏丢本次写入，不损坏既有数据。
    fn save_locked(&self, file: &BlackboardFile) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create blackboard dir: {e}"))?;
        }
        let bytes = serde_json::to_vec_pretty(file).map_err(|e| format!("serialize blackboard: {e}"))?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, bytes).map_err(|e| format!("write blackboard tmp: {e}"))?;
        if self.path.exists() {
            std::fs::remove_file(&self.path).map_err(|e| format!("remove old blackboard: {e}"))?;
        }
        std::fs::rename(&tmp, &self.path).map_err(|e| format!("rename blackboard: {e}"))
    }
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `blackboard_append` 工具：登记一条结论/发现/风险/补充说明。
pub struct BlackboardAppendTool {
    store: BlackboardStore,
}

impl BlackboardAppendTool {
    pub fn new(store: BlackboardStore) -> Self {
        Self { store }
    }

    pub const NAME: &'static str = "blackboard_append";
}

#[async_trait::async_trait]
impl Tool for BlackboardAppendTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Append a conclusion/finding/risk/note to the workspace-shared blackboard (visible to \
all sessions and subagents, across sessions). Use for durable intermediate results: register \
conclusions as soon as you reach them instead of hoarding until the end; other agents may be \
waiting on them. Entries are append-only for agents — ask the user to edit/remove via the UI."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "type": {
                    "type": "string",
                    "enum": ["note", "finding", "risk", "conclusion"],
                    "description": "Entry kind: conclusion/decision, discovered fact, risk/pitfall, or supplementary note. Unknown values are treated as note."
                },
                "author": {
                    "type": "string",
                    "description": "Who registers this (agent name or label, e.g. coder-s2). Optional."
                },
                "content": {
                    "type": "string",
                    "description": "One sentence: what it is + what it means for others. No process logs or drafts."
                }
            },
            "required": ["content"]
        })
    }

    async fn call(&self, input: serde_json::Value) -> agentscope_core::Result<ToolOutput> {
        let content = match input.get("content").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => {
                return Ok(ToolOutput::error("content is required"));
            }
        };
        let kind = input
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("note");
        let author = input
            .get("author")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        match self.store.append(kind, author, content) {
            Ok(entry) => Ok(ToolOutput {
                content: format!(
                    "✅ #{} 已登记（{}, by {}）",
                    entry.id,
                    entry.kind,
                    if entry.author.is_empty() { "anonymous" } else { &entry.author }
                ),
                state: ToolResultState::Success,
                execution_details: None,
            }),
            Err(e) => Ok(ToolOutput::error(&e)),
        }
    }
}

/// `blackboard_read` 工具：读黑板最近条目（最新在前）。
pub struct BlackboardReadTool {
    store: BlackboardStore,
}

impl BlackboardReadTool {
    pub fn new(store: BlackboardStore) -> Self {
        Self { store }
    }

    pub const NAME: &'static str = "blackboard_read";
}

#[async_trait::async_trait]
impl Tool for BlackboardReadTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Read recent entries from the workspace-shared blackboard (newest first). Call before \
starting work to see what other agents or earlier sessions already concluded, avoiding \
duplicated effort and conflicting decisions. The blackboard is shared across sessions."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "Max entries to return (default 30, max 100)."
                }
            }
        })
    }

    async fn call(&self, input: serde_json::Value) -> agentscope_core::Result<ToolOutput> {
        let limit = input
            .get("limit")
            .and_then(|v| v.as_i64())
            .filter(|l| *l > 0)
            .map(|l| l.min(100) as usize);
        match self.store.list(limit) {
            Ok(entries) => {
                if entries.is_empty() {
                    return Ok(ToolOutput {
                        content: "（黑板为空：还没有任何登记条目）".to_string(),
                        state: ToolResultState::Success,
                        execution_details: None,
                    });
                }
                let mut sb = String::new();
                for e in &entries {
                    sb.push_str(&format!(
                        "#{} [{}] ({}) {}\n",
                        e.id,
                        e.kind,
                        format_ts(e.ts),
                        e.content
                    ));
                }
                Ok(ToolOutput {
                    content: sb,
                    state: ToolResultState::Success,
                    execution_details: None,
                })
            }
            Err(e) => Ok(ToolOutput::error(&e)),
        }
    }
}

/// 时间戳 → `YYYY-MM-DD HH:MM`（本地无关的 UTC 展示即可，避免引 chrono）。
fn format_ts(millis: i64) -> String {
    // 简易 UTC 换算：天数 + 当日秒数（不处理闰秒；展示用途足够）。
    let secs = millis.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (h, m) = (tod / 3600, (tod % 3600) / 60);
    // civil-from-days（Howard Hinnant 算法）。
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };
    format!("{y:04}-{mth:02}-{d:02} {h:02}:{m:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_store(tag: &str) -> (std::path::PathBuf, BlackboardStore) {
        let dir = std::env::temp_dir().join(format!("agentscope-bb-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("blackboard.json");
        let _ = std::fs::remove_file(&path);
        (dir, BlackboardStore::new(&path))
    }

    /// append → list 往返；类型归一化（未知值 → note）。
    #[test]
    fn append_then_list_roundtrip_normalizes_kind() {
        let (dir, store) = tmp_store("roundtrip");
        let e1 = store.append("conclusion", "lead", "方案 A 已确认").unwrap();
        assert_eq!(e1.id, 1);
        let e2 = store.append("bogus-kind", "", "未知类型兜底").unwrap();
        assert_eq!(e2.kind, "note");
        // 空内容拒绝。
        assert!(store.append("note", "", "   ").is_err());
        let all = store.list(None).unwrap();
        assert_eq!(all.len(), 2);
        // 最新在前。
        assert_eq!(all[0].id, 2);
        assert_eq!(all[1].content, "方案 A 已确认");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 跨 store 实例共享（同一路径）：模拟「模型会话 append、用户面板读取」
    /// 与「跨 session 可见」——两个实例指向同一文件即可互见。
    #[test]
    fn cross_instance_shared_visibility() {
        let (dir, store_a) = tmp_store("shared");
        let store_b = BlackboardStore::new(dir.join("blackboard.json"));
        store_a.append("finding", "coder-s2", "A 会话的发现").unwrap();
        let seen = store_b.list(None).unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].content, "A 会话的发现");
        assert_eq!(seen[0].author, "coder-s2");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// id 自增不复用：删除 #2 后新条目仍是 #3。
    #[test]
    fn delete_then_append_does_not_reuse_id() {
        let (dir, store) = tmp_store("ids");
        store.append("note", "", "one").unwrap();
        store.append("note", "", "two").unwrap();
        assert!(store.delete(2).unwrap());
        assert!(!store.delete(2).unwrap()); // 幂等：再删返回 false
        let e = store.append("note", "", "three").unwrap();
        assert_eq!(e.id, 3);
        assert_eq!(store.list(None).unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// update：改类型/正文；不存在报错；正文清空拒绝。
    #[test]
    fn update_edits_kind_and_content() {
        let (dir, store) = tmp_store("update");
        let e = store.append("note", "coder", "初稿结论").unwrap();
        let u = store
            .update(e.id, Some("risk"), Some(" 修正后的风险描述 "))
            .unwrap();
        assert_eq!(u.kind, "risk");
        assert_eq!(u.content, "修正后的风险描述");
        assert!(store.update(999, None, Some("x")).is_err());
        assert!(store.update(e.id, None, Some("   ")).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// limit 截取最近 N 条。
    #[test]
    fn list_limit_returns_newest_n() {
        let (dir, store) = tmp_store("limit");
        for i in 1..=5 {
            store.append("note", "", &format!("item-{i}")).unwrap();
        }
        let top3 = store.list(Some(3)).unwrap();
        assert_eq!(top3.len(), 3);
        assert_eq!(top3[0].content, "item-5");
        assert_eq!(top3[2].content, "item-3");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 并发 append：10 个任务同时登记，全部落盘且 id 唯一。
    #[tokio::test]
    async fn concurrent_appends_all_persisted() {
        let (dir, store) = tmp_store("conc");
        let store = std::sync::Arc::new(store);
        let mut tasks = Vec::new();
        for i in 0..10 {
            let s = store.clone();
            tasks.push(tokio::spawn(async move {
                s.append("finding", &format!("agent-{i}"), &format!("并发条目-{i}"))
                    .unwrap()
            }));
        }
        let mut ids = Vec::new();
        for t in tasks {
            ids.push(t.await.unwrap().id);
        }
        ids.sort();
        assert_eq!(ids, (1..=10).collect::<Vec<_>>());
        assert_eq!(store.list(None).unwrap().len(), 10);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 模型工具：append 返回确认文案；read 空黑板与有条目两种输出。
    #[tokio::test]
    async fn tools_append_and_read_output_format() {
        let (dir, _guard) = tmp_store("tools");
        let append = BlackboardAppendTool::new(BlackboardStore::new(dir.join("blackboard.json")));
        let read = BlackboardReadTool::new(BlackboardStore::new(dir.join("blackboard.json")));

        // 空黑板。
        let out = read.call(json!({})).await.unwrap();
        assert!(out.content.contains("黑板为空"));

        // 缺 content 报错。
        let out = append.call(json!({ "type": "risk" })).await.unwrap();
        assert_eq!(out.state, ToolResultState::Error);

        // 正常登记。
        let out = append
            .call(json!({"type": "conclusion", "author": "lead", "content": "口径已确认"}))
            .await
            .unwrap();
        assert!(out.content.contains("#1 已登记（conclusion, by lead）"), "{}", out.content);

        // 读回。
        let out = read.call(json!({"limit": 10})).await.unwrap();
        assert!(out.content.contains("#1 [conclusion]"), "{}", out.content);
        assert!(out.content.contains("口径已确认"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
