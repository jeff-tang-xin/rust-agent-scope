//! 消息总线与异步工具登记（对应 Java `harness.agent.bus` 包 6 文件）。
//!
//! [`MessageBus`] 是跨会话/跨进程协调工作的活消息传输，暴露三种消费模式：
//!
//! - **Mode A 排空队列**（单消费者、读即确认）：[`MessageBus::queue_push`] /
//!   [`MessageBus::queue_drain`]，条目最多被返回一次；
//! - **Mode C 重放日志**（多消费者、外部定界）：[`MessageBus::log_append`] /
//!   [`MessageBus::log_read`]，读者各自维护游标，条目保留到裁剪为止；
//! - **Mode D 瞬时广播**（fire-and-forget pub/sub）：[`MessageBus::publish`] /
//!   [`MessageBus::subscribe`]，只有当前订阅者收到，不留历史。
//!
//! 领域助手（inbox / wakeup / session events）把高层概念映射到上述原语，
//! 键名约定与 Java 一致（`agentscope:inbox:<session>` 等）。
//!
//! [`WorkspaceMessageBus`] 用 [`Filesystem`] 后端落地 Mode A/C（文件布局
//! `{busRoot}/queues|logs/{key-hash}/{entryId}.json`）；Mode D 在单进程内用
//! tokio broadcast 真广播。**有意差异**：Java 的 filesystem 后端无法做真
//! pub/sub，`subscribe` 降级为周期性空信号轮询；Rust 侧单进程内直接广播，
//! 跨进程语义（共享 KV 后端）留分布式阶段。
//!
//! [`AsyncToolRegistry`] 跟踪被卸载到后台的异步工具执行生命周期；
//! [`WorkspaceAsyncToolRegistry`] 把每条记录持久化为
//! `{registryRoot}/{sessionId}/{recordId}.json`，进程崩溃后可检测孤儿
//! RUNNING 记录（对齐 Java `InboxMiddleware` 的恢复语义）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::workspace::Filesystem;

/// inbox 队列键前缀（对应 Java `inboxPush` 的固定命名约定）。
pub const INBOX_KEY_PREFIX: &str = "agentscope:inbox:";
/// wakeup 请求队列键。
pub const WAKEUPS_KEY: &str = "agentscope:wakeups";
/// wakeup 信号广播频道。
pub const WAKEUP_SIGNAL_KEY: &str = "agentscope:wakeup_signal";
/// 会话事件重放日志键前缀。
pub const SESSION_EVENTS_KEY_PREFIX: &str = "agentscope:session:events:";
/// 会话事件重放日志上限（对应 Java `SESSION_REPLAY_MAX_LEN`）。
pub const SESSION_REPLAY_MAX_LEN: i64 = 1000;

/// 从总线队列或日志读出的一条条目（对应 Java `BusEntry` record）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BusEntry {
    /// 传输层条目 id（如文件名去 `.json`）。
    #[serde(rename = "entryId")]
    pub entry_id: String,
    /// JSON 可序列化载荷。
    pub payload: Map<String, Value>,
}

/// 后台异步工具执行的生命周期记录（对应 Java `AsyncToolRecord` record）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AsyncToolRecord {
    /// 异步执行的唯一 id。
    pub id: String,
    /// 发起调用的会话。
    #[serde(rename = "sessionId")]
    pub session_id: String,
    /// 工具名。
    #[serde(rename = "toolName")]
    pub tool_name: String,
    /// LLM 原始 tool_call id。
    #[serde(rename = "toolCallId")]
    pub tool_call_id: String,
    /// 当前状态：[`AsyncToolRecord::RUNNING`] 等。
    pub status: String,
    /// 登记时刻（epoch 毫秒；Java 存 `Instant`，Rust 统一用毫秒数）。
    #[serde(rename = "createdAt")]
    pub created_at: i64,
}

impl AsyncToolRecord {
    pub const RUNNING: &'static str = "RUNNING";
    pub const COMPLETED: &'static str = "COMPLETED";
    pub const FAILED: &'static str = "FAILED";
    pub const TIMEOUT: &'static str = "TIMEOUT";
}

/// 活消息传输 trait（对应 Java `MessageBus` 接口；`AutoCloseable` 语义由
/// 实现自行管理，Rust 侧不设 close 方法）。
#[async_trait::async_trait]
pub trait MessageBus: Send + Sync {
    // ---- Mode A：排空队列（单消费者、读即确认） ----

    /// 追加一条载荷到指定键的队列，返回传输层条目 id。
    async fn queue_push(&self, key: &str, payload: &Map<String, Value>) -> Result<String, String>;

    /// 从队列排空至多 `max_count` 条；返回的条目被原子移除，下次调用只返回
    /// 之后到达的条目。按到达顺序返回。
    async fn queue_drain(&self, key: &str, max_count: usize)
        -> Result<Vec<BusEntry>, String>;

    /// 删除指定键的队列及其全部条目。幂等。
    async fn queue_delete(&self, key: &str) -> Result<(), String>;

    /// 不消费地检查队列是否还有条目。
    async fn queue_peek(&self, key: &str) -> Result<bool, String>;

    // ---- Mode C：重放日志（多消费者、外部定界） ----

    /// 追加一条载荷到重放日志；`max_len > 0` 时超出则裁剪最旧条目。
    /// 返回的条目 id 可作为后续读取的游标。
    async fn log_append(
        &self,
        key: &str,
        payload: &Map<String, Value>,
        max_len: i64,
    ) -> Result<String, String>;

    /// 从 `since`（不含）之后读取至多 `max_count` 条；非破坏性，同一批条目
    /// 可被任意多读者读到。`since = None` 从头读。
    async fn log_read(
        &self,
        key: &str,
        since: Option<&str>,
        max_count: usize,
    ) -> Result<Vec<BusEntry>, String>;

    /// 删除重放日志及其全部条目。幂等。
    async fn log_trim(&self, key: &str) -> Result<(), String>;

    // ---- Mode D：瞬时广播（fire-and-forget） ----

    /// 在广播频道上发布载荷；只有当前订阅者收到。
    async fn publish(&self, key: &str, payload: &Map<String, Value>) -> Result<(), String>;

    /// 订阅广播频道，只收到订阅建立之后发布的载荷。
    /// 调用方持有订阅生命周期——drop receiver 即结束。
    fn subscribe(&self, key: &str) -> tokio::sync::broadcast::Receiver<Map<String, Value>>;

    // ---- 领域助手：Inbox ----

    /// 不消费地检查会话 inbox 是否有待处理消息。
    async fn inbox_has_messages(&self, session_id: &str) -> Result<bool, String> {
        self.queue_peek(&format!("{INBOX_KEY_PREFIX}{session_id}")).await
    }

    /// 向会话 inbox 投递一条消息（通常是序列化的 HintBlock）。
    async fn inbox_push(&self, session_id: &str, msg: &Map<String, Value>) -> Result<(), String> {
        self.queue_push(&format!("{INBOX_KEY_PREFIX}{session_id}"), msg)
            .await
            .map(|_| ())
    }

    /// 排空会话 inbox 的待处理消息。
    async fn inbox_drain(&self, session_id: &str, max_count: usize) -> Result<Vec<BusEntry>, String> {
        self.queue_drain(&format!("{INBOX_KEY_PREFIX}{session_id}"), max_count)
            .await
    }

    // ---- 领域助手：Wakeup ----

    /// 入队一个 wakeup 请求并广播信号（多用户隔离由 `user_id` 承担）。
    async fn enqueue_wakeup(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        agent_id: &str,
    ) -> Result<(), String> {
        let mut payload = Map::new();
        payload.insert("userId".into(), Value::from(user_id.unwrap_or("")));
        payload.insert("sessionId".into(), Value::from(session_id));
        payload.insert("agentId".into(), Value::from(agent_id));
        self.queue_push(WAKEUPS_KEY, &payload).await?;
        self.publish(WAKEUP_SIGNAL_KEY, &Map::new()).await
    }

    /// 订阅共享 wakeup 信号频道（每个信号表示「现在去排空 wakeup 队列」）。
    fn subscribe_wakeup(&self) -> tokio::sync::broadcast::Receiver<Map<String, Value>> {
        self.subscribe(WAKEUP_SIGNAL_KEY)
    }

    // ---- 领域助手：Session events ----

    /// 把会话事件写入重放日志并实时扇出：晚加入的订阅者可从日志重放，
    /// 已连接的订阅者经 pub/sub 立即收到。
    async fn session_publish_event(
        &self,
        session_id: &str,
        event: &Map<String, Value>,
    ) -> Result<String, String> {
        let key = format!("{SESSION_EVENTS_KEY_PREFIX}{session_id}");
        let entry_id = self.log_append(&key, event, SESSION_REPLAY_MAX_LEN).await?;
        let mut live = event.clone();
        live.insert("_entry_id".into(), Value::from(entry_id.as_str()));
        self.publish(&key, &live).await?;
        Ok(entry_id)
    }

    /// 从会话事件重放日志读事件（追赶 / 重连场景）。
    async fn session_read_events(
        &self,
        session_id: &str,
        since: Option<&str>,
        max_count: usize,
    ) -> Result<Vec<BusEntry>, String> {
        self.log_read(
            &format!("{SESSION_EVENTS_KEY_PREFIX}{session_id}"),
            since,
            max_count,
        )
        .await
    }

    /// 实时订阅会话发布的事件（只收订阅建立之后的载荷）。
    fn session_subscribe_events(
        &self,
        session_id: &str,
    ) -> tokio::sync::broadcast::Receiver<Map<String, Value>> {
        self.subscribe(&format!("{SESSION_EVENTS_KEY_PREFIX}{session_id}"))
    }

    /// 裁剪会话事件重放日志（会话运行结束后调用，避免晚订阅者重放上一轮的旧事件）。
    async fn session_trim_events(&self, session_id: &str) -> Result<(), String> {
        self.log_trim(&format!("{SESSION_EVENTS_KEY_PREFIX}{session_id}"))
            .await
    }
}

/// 当前时刻（epoch 毫秒）。
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// [`MessageBus`] 的 filesystem 后端实现（对应 Java `WorkspaceMessageBus`）。
///
/// 适用于任意 [`Filesystem`] 后端（本地 / 远端 / 沙箱）。只有当后端真正共享
/// （远端 / KV 存储）时才具备跨进程投递能力；纯本地磁盘后端仍是单进程语义。
///
/// 文件布局（`bus_root` 下）：
/// ```text
/// {bus_root}/
///   queues/{key-hash}/{entryId}.json   — Mode A 条目（drain = ls + read + delete）
///   logs/{key-hash}/{entryId}.json     — Mode C 条目（append-only）
/// ```
pub struct WorkspaceMessageBus {
    fs: std::sync::Arc<dyn Filesystem>,
    bus_root: String,
    seq: AtomicU64,
    /// Mode D 的进程内广播（有意差异：Java 降级为轮询，见模块文档）。
    tx: tokio::sync::broadcast::Sender<Map<String, Value>>,
}

impl std::fmt::Debug for WorkspaceMessageBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceMessageBus")
            .field("bus_root", &self.bus_root)
            .finish_non_exhaustive()
    }
}

impl WorkspaceMessageBus {
    /// 构造；`bus_root` 为总线数据在 filesystem 内的根路径（如 `/bus`）。
    pub fn new(fs: std::sync::Arc<dyn Filesystem>, bus_root: impl Into<String>) -> Self {
        let bus_root = bus_root.into();
        let bus_root = bus_root.trim_end_matches('/').to_string();
        // 初始序号取当前纳秒，降低跨进程同根冲突概率（对齐 Java System.nanoTime）。
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let (tx, _) = tokio::sync::broadcast::channel(256);
        Self {
            fs,
            bus_root,
            seq: AtomicU64::new(nanos),
            tx,
        }
    }

    fn queue_dir(&self, key: &str) -> String {
        format!("{}/queues/{}", self.bus_root, hash_key(key))
    }

    fn log_dir(&self, key: &str) -> String {
        format!("{}/logs/{}", self.bus_root, hash_key(key))
    }

    fn next_entry_id(&self) -> String {
        format!("{:020}", self.seq.fetch_add(1, Ordering::SeqCst) + 1)
    }

    /// 目录不存在时写 `.keep` 占位（对齐 Java `ensureDir`）。
    async fn ensure_dir(&self, dir: &str) -> Result<(), String> {
        if !self.fs.exists(&RuntimeContextOf::empty(), dir).await? {
            self.fs
                .write(&RuntimeContextOf::empty(), &format!("{dir}/.keep"), "")
                .await?;
        }
        Ok(())
    }

    /// 列出目录下的 `.json` 文件，按路径排序；目录不存在返回空。
    async fn list_sorted(&self, dir: &str) -> Result<Vec<crate::workspace::FileInfo>, String> {
        if !self.fs.exists(&RuntimeContextOf::empty(), dir).await? {
            return Ok(Vec::new());
        }
        let entries = self.fs.ls(&RuntimeContextOf::empty(), dir).await?;
        let mut files: Vec<crate::workspace::FileInfo> = entries
            .into_iter()
            .filter(|f| !f.is_directory && f.path.ends_with(".json"))
            .collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(files)
    }

    async fn read_file_content(&self, path: &str) -> Result<Option<String>, String> {
        self.fs.read(&RuntimeContextOf::empty(), path).await
    }
}

/// `RuntimeContext` 的本地别名（bus 层所有 filesystem 调用都用空上下文，
/// 对齐 Java 的 `static final RuntimeContext RC = RuntimeContext.empty()`）。
type RuntimeContextOf = agentscope_core::RuntimeContext;

#[async_trait::async_trait]
impl MessageBus for WorkspaceMessageBus {
    async fn queue_push(&self, key: &str, payload: &Map<String, Value>) -> Result<String, String> {
        let entry_id = self.next_entry_id();
        let dir = self.queue_dir(key);
        self.ensure_dir(&dir).await?;
        let path = format!("{dir}/{entry_id}.json");
        let json = Value::Object(payload.clone()).to_string();
        self.fs.write(&RuntimeContextOf::empty(), &path, &json).await?;
        Ok(entry_id)
    }

    async fn queue_drain(&self, key: &str, max_count: usize) -> Result<Vec<BusEntry>, String> {
        let dir = self.queue_dir(key);
        let files = self.list_sorted(&dir).await?;
        let count = max_count.min(files.len());
        let mut result = Vec::with_capacity(count);
        for fi in files.into_iter().take(count) {
            let content = match self.read_file_content(&fi.path).await {
                Ok(Some(content)) => content,
                // 瞬时读失败（并发 drain 竞争 / I/O 错误）：不删除，保住条目
                // 让下次 drain 重试（对齐 Java 注释语义）。
                Ok(None) => continue,
                Err(_) => continue,
            };
            let entry_id = extract_entry_id(&fi.path);
            let payload: Map<String, Value> = match serde_json::from_str(&content) {
                Ok(Value::Object(map)) => map,
                // 损坏的 JSON 不可恢复：删除以避免同一坏条目造成无限 drain
                // 循环（对齐 Java 注释语义）。
                _ => {
                    let _ = self.fs.delete(&RuntimeContextOf::empty(), &fi.path).await;
                    continue;
                }
            };
            let _ = self.fs.delete(&RuntimeContextOf::empty(), &fi.path).await;
            result.push(BusEntry { entry_id, payload });
        }
        Ok(result)
    }

    async fn queue_delete(&self, key: &str) -> Result<(), String> {
        self.fs.delete(&RuntimeContextOf::empty(), &self.queue_dir(key)).await
    }

    async fn queue_peek(&self, key: &str) -> Result<bool, String> {
        Ok(!self.list_sorted(&self.queue_dir(key)).await?.is_empty())
    }

    async fn log_append(
        &self,
        key: &str,
        payload: &Map<String, Value>,
        max_len: i64,
    ) -> Result<String, String> {
        let entry_id = self.next_entry_id();
        let dir = self.log_dir(key);
        self.ensure_dir(&dir).await?;
        let path = format!("{dir}/{entry_id}.json");
        let json = Value::Object(payload.clone()).to_string();
        self.fs.write(&RuntimeContextOf::empty(), &path, &json).await?;

        if max_len > 0 {
            let files = self.list_sorted(&dir).await?;
            if files.len() as i64 > max_len {
                let excess = files.len() as i64 - max_len;
                for fi in files.into_iter().take(excess as usize) {
                    let _ = self.fs.delete(&RuntimeContextOf::empty(), &fi.path).await;
                }
            }
        }
        Ok(entry_id)
    }

    async fn log_read(
        &self,
        key: &str,
        since: Option<&str>,
        max_count: usize,
    ) -> Result<Vec<BusEntry>, String> {
        let dir = self.log_dir(key);
        let files = self.list_sorted(&dir).await?;
        if files.is_empty() {
            return Ok(Vec::new());
        }

        // since 定位：命中则从下一条开始；未命中从头读（对齐 Java 循环语义）。
        let mut start_idx = 0usize;
        if let Some(since) = since {
            for (i, fi) in files.iter().enumerate() {
                if extract_entry_id(&fi.path) == since {
                    start_idx = i + 1;
                    break;
                }
            }
        }

        let end_idx = (start_idx + max_count).min(files.len());
        let mut result = Vec::with_capacity(end_idx - start_idx);
        for fi in &files[start_idx..end_idx] {
            if let Ok(Some(content)) = self.read_file_content(&fi.path).await {
                if let Ok(Value::Object(payload)) = serde_json::from_str::<Value>(&content) {
                    result.push(BusEntry {
                        entry_id: extract_entry_id(&fi.path),
                        payload,
                    });
                }
            }
        }
        Ok(result)
    }

    async fn log_trim(&self, key: &str) -> Result<(), String> {
        self.fs.delete(&RuntimeContextOf::empty(), &self.log_dir(key)).await
    }

    async fn publish(&self, _key: &str, payload: &Map<String, Value>) -> Result<(), String> {
        // 无订阅者时 send 返回 Err——fire-and-forget 语义下忽略。
        let _ = self.tx.send(payload.clone());
        Ok(())
    }

    fn subscribe(&self, _key: &str) -> tokio::sync::broadcast::Receiver<Map<String, Value>> {
        self.tx.subscribe()
    }
}

/// 键 → 目录名：非法字符替换为 `_`，再拼 Java `String.hashCode` 的无符号
/// 十六进制（对齐 Java `hashKey`；跨实现一致性无要求，仅保证同键同目录）。
pub(crate) fn hash_key(key: &str) -> String {
    let sanitized: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut hash: i32 = 0;
    for c in key.chars() {
        hash = hash.wrapping_mul(31).wrapping_add(c as i32);
    }
    format!("{sanitized}_{:x}", hash & 0x7fff_ffff)
}

/// 从文件路径提取条目 id（去目录与 `.json` 后缀）。
fn extract_entry_id(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.strip_suffix(".json").unwrap_or(name).to_string()
}

/// 异步工具登记 trait（对应 Java `AsyncToolRegistry` 接口）。
///
/// 工具执行超过配置超时被卸载到后台时登记于此，用于：
/// - 正常完成时更新记录并经 inbox 投递结果；
/// - 进程崩溃 / 重启后检测孤儿 RUNNING 记录并通知 LLM。
#[async_trait::async_trait]
pub trait AsyncToolRegistry: Send + Sync {
    /// 登记一条新的异步工具执行（状态为 [`AsyncToolRecord::RUNNING`]）。
    async fn register(&self, record: &AsyncToolRecord) -> Result<(), String>;

    /// 标记异步工具执行完成。`result` 为工具执行结果文本。
    async fn complete(&self, id: &str, result: &str) -> Result<(), String>;

    /// 标记异步工具执行失败。`error` 为错误消息。
    async fn fail(&self, id: &str, error: &str) -> Result<(), String>;

    /// 找出指定会话中 RUNNING 超过 `ttl` 的记录（大概率因进程崩溃成为孤儿）。
    async fn find_stale(&self, session_id: &str, ttl: Duration)
        -> Result<Vec<AsyncToolRecord>, String>;

    /// 把孤儿 RUNNING 记录标记为超时，使其不再被后续 [`Self::find_stale`] 返回。
    async fn mark_timeout(&self, id: &str) -> Result<(), String>;
}

/// [`AsyncToolRegistry`] 的 filesystem 后端实现（对应 Java
/// `WorkspaceAsyncToolRegistry`）。
///
/// 每条记录持久化为 `{registry_root}/{sessionId}/{recordId}.json`，进程崩溃后
/// 记录仍在——会话恢复时可检测孤儿 RUNNING 记录并通知 LLM。
pub struct WorkspaceAsyncToolRegistry {
    fs: std::sync::Arc<dyn Filesystem>,
    registry_root: String,
}

impl std::fmt::Debug for WorkspaceAsyncToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceAsyncToolRegistry")
            .field("registry_root", &self.registry_root)
            .finish_non_exhaustive()
    }
}

impl WorkspaceAsyncToolRegistry {
    /// 构造；`registry_root` 为记录根路径（如 `/bus/async-tools`）。
    pub fn new(fs: std::sync::Arc<dyn Filesystem>, registry_root: impl Into<String>) -> Self {
        let registry_root = registry_root.into();
        let registry_root = registry_root.trim_end_matches('/').to_string();
        Self { fs, registry_root }
    }

    fn session_dir(&self, session_id: &str) -> String {
        format!("{}/{}", self.registry_root, sanitize(session_id))
    }

    fn record_path(&self, session_id: &str, id: &str) -> String {
        format!("{}/{}.json", self.session_dir(session_id), id)
    }

    async fn ensure_dir(&self, dir: &str) -> Result<(), String> {
        if !self.fs.exists(&RuntimeContextOf::empty(), dir).await? {
            self.fs
                .write(&RuntimeContextOf::empty(), &format!("{dir}/.keep"), "")
                .await?;
        }
        Ok(())
    }

    async fn read_record(&self, path: &str) -> Result<Option<AsyncToolRecord>, String> {
        let Some(content) = self.fs.read(&RuntimeContextOf::empty(), path).await? else {
            return Ok(None);
        };
        match serde_json::from_str::<AsyncToolRecord>(&content) {
            Ok(record) => Ok(Some(record)),
            // 解析失败按缺失处理（对齐 Java 记 warn 返回 null）。
            Err(_) => Ok(None),
        }
    }

    /// 全根扫描定位记录文件（id 不带会话信息时唯一确定路径）。
    async fn find_record_path(&self, id: &str) -> Result<Option<String>, String> {
        let entries = match self.fs.ls(&RuntimeContextOf::empty(), &self.registry_root).await {
            Ok(entries) => entries,
            Err(_) => return Ok(None),
        };
        for entry in entries {
            if !entry.is_directory {
                continue;
            }
            let candidate = format!("{}/{}.json", entry.path, id);
            if self.fs.exists(&RuntimeContextOf::empty(), &candidate).await? {
                return Ok(Some(candidate));
            }
        }
        Ok(None)
    }

    async fn update_status(&self, id: &str, new_status: &str) -> Result<(), String> {
        let Some(path) = self.find_record_path(id).await? else {
            return Ok(());
        };
        let Some(record) = self.read_record(&path).await? else {
            return Ok(());
        };
        let updated = AsyncToolRecord {
            status: new_status.to_string(),
            ..record
        };
        let json = serde_json::to_string(&updated).map_err(|e| e.to_string())?;
        self.fs.write(&RuntimeContextOf::empty(), &path, &json).await
    }
}

#[async_trait::async_trait]
impl AsyncToolRegistry for WorkspaceAsyncToolRegistry {
    async fn register(&self, record: &AsyncToolRecord) -> Result<(), String> {
        let dir = self.session_dir(&record.session_id);
        self.ensure_dir(&dir).await?;
        let path = self.record_path(&record.session_id, &record.id);
        let json = serde_json::to_string(record).map_err(|e| e.to_string())?;
        self.fs.write(&RuntimeContextOf::empty(), &path, &json).await
    }

    async fn complete(&self, id: &str, _result: &str) -> Result<(), String> {
        self.update_status(id, AsyncToolRecord::COMPLETED).await
    }

    async fn fail(&self, id: &str, _error: &str) -> Result<(), String> {
        self.update_status(id, AsyncToolRecord::FAILED).await
    }

    async fn find_stale(&self, session_id: &str, ttl: Duration) -> Result<Vec<AsyncToolRecord>, String> {
        let dir = self.session_dir(session_id);
        if !self.fs.exists(&RuntimeContextOf::empty(), &dir).await? {
            return Ok(Vec::new());
        }
        let entries = self.fs.ls(&RuntimeContextOf::empty(), &dir).await?;
        let cutoff = now_millis() - ttl.as_millis() as i64;
        let mut stale = Vec::new();
        for fi in entries {
            if fi.is_directory || !fi.path.ends_with(".json") {
                continue;
            }
            if let Some(record) = self.read_record(&fi.path).await? {
                if record.status == AsyncToolRecord::RUNNING && record.created_at < cutoff {
                    stale.push(record);
                }
            }
        }
        Ok(stale)
    }

    async fn mark_timeout(&self, id: &str) -> Result<(), String> {
        self.update_status(id, AsyncToolRecord::TIMEOUT).await
    }
}

/// 会话目录名安全化（对齐 Java `sanitize`；空串落 `default`）。
fn sanitize(s: &str) -> String {
    if s.is_empty() {
        return "default".to_string();
    }
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::InMemoryFilesystem;
    use std::sync::Arc;

    fn payload(pairs: &[(&str, &str)]) -> Map<String, Value> {
        let mut map = Map::new();
        for (k, v) in pairs {
            map.insert((*k).to_string(), Value::from(*v));
        }
        map
    }

    fn bus() -> WorkspaceMessageBus {
        WorkspaceMessageBus::new(
            Arc::new(InMemoryFilesystem::new()),
            "/bus",
        )
    }

    #[test]
    fn hash_key_is_deterministic_and_sanitizes() {
        let a = hash_key("agentscope:inbox:s1");
        let b = hash_key("agentscope:inbox:s1");
        assert_eq!(a, b);
        assert!(!a.contains(':'));
        assert!(a.starts_with("agentscope_inbox_s1_"));
        // 不同键不同目录。
        assert_ne!(hash_key("k1"), hash_key("k2"));
    }

    #[tokio::test]
    async fn queue_drain_is_ack_on_read_and_ordered() {
        let bus = bus();
        bus.queue_push("q", &payload(&[("n", "1")])).await.unwrap();
        bus.queue_push("q", &payload(&[("n", "2")])).await.unwrap();
        assert!(bus.queue_peek("q").await.unwrap());

        let drained = bus.queue_drain("q", 10).await.unwrap();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].payload.get("n").unwrap(), "1");
        assert_eq!(drained[1].payload.get("n").unwrap(), "2");

        // 读即确认：排空后为空。
        assert!(!bus.queue_peek("q").await.unwrap());
        assert!(bus.queue_drain("q", 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn queue_drain_respects_max_count() {
        let bus = bus();
        for i in 0..3 {
            bus.queue_push("q", &payload(&[("n", &i.to_string())]))
                .await
                .unwrap();
        }
        let first = bus.queue_drain("q", 2).await.unwrap();
        assert_eq!(first.len(), 2);
        let rest = bus.queue_drain("q", 10).await.unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].payload.get("n").unwrap(), "3");
    }

    #[tokio::test]
    async fn queue_delete_is_idempotent() {
        let bus = bus();
        bus.queue_push("q", &payload(&[("a", "b")])).await.unwrap();
        bus.queue_delete("q").await.unwrap();
        bus.queue_delete("q").await.unwrap();
        assert!(!bus.queue_peek("q").await.unwrap());
    }

    #[tokio::test]
    async fn log_read_supports_cursor_and_trim() {
        let bus = bus();
        let e1 = bus.log_append("log", &payload(&[("i", "1")]), 0).await.unwrap();
        let e2 = bus.log_append("log", &payload(&[("i", "2")]), 0).await.unwrap();
        bus.log_append("log", &payload(&[("i", "3")]), 0).await.unwrap();

        // 从头读。
        let all = bus.log_read("log", None, 10).await.unwrap();
        assert_eq!(all.len(), 3);
        // since 游标：只读之后条目。
        let after = bus.log_read("log", Some(&e1), 10).await.unwrap();
        assert_eq!(after.len(), 2);
        assert_eq!(after[0].entry_id, e2);
        // 未知游标从头读（对齐 Java 循环语义）。
        let unknown = bus.log_read("log", Some("missing"), 10).await.unwrap();
        assert_eq!(unknown.len(), 3);
        // max_count 截断。
        let limited = bus.log_read("log", None, 1).await.unwrap();
        assert_eq!(limited.len(), 1);

        // 裁剪。
        bus.log_trim("log").await.unwrap();
        assert!(bus.log_read("log", None, 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn log_append_trims_oldest_beyond_max_len() {
        let bus = bus();
        for i in 0..5 {
            bus.log_append("log", &payload(&[("i", &i.to_string())]), 3)
                .await
                .unwrap();
        }
        let entries = bus.log_read("log", None, 10).await.unwrap();
        assert_eq!(entries.len(), 3);
        // 最旧的 1、2 被裁掉，剩 3、4、5。
        assert_eq!(entries[0].payload.get("i").unwrap(), "3");
        assert_eq!(entries[2].payload.get("i").unwrap(), "5");
    }

    #[tokio::test]
    async fn inbox_and_wakeup_helpers_use_fixed_keys() {
        let bus = bus();
        assert!(!bus.inbox_has_messages("s1").await.unwrap());
        bus.inbox_push("s1", &payload(&[("kind", "hint")])).await.unwrap();
        assert!(bus.inbox_has_messages("s1").await.unwrap());
        let drained = bus.inbox_drain("s1", 10).await.unwrap();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].payload.get("kind").unwrap(), "hint");

        // wakeup：入队 + 广播信号。
        let mut rx = bus.subscribe_wakeup();
        bus.enqueue_wakeup(Some("u"), "s1", "agent-a").await.unwrap();
        let signal = rx.try_recv().unwrap();
        assert!(signal.is_empty()); // 信号载荷为空（对齐 Java Map.of()）。
        // wakeup 队列里有请求记录。
        let wakeups = bus.queue_drain(WAKEUPS_KEY, 10).await.unwrap();
        assert_eq!(wakeups.len(), 1);
        assert_eq!(wakeups[0].payload.get("userId").unwrap(), "u");
        assert_eq!(wakeups[0].payload.get("sessionId").unwrap(), "s1");
        assert_eq!(wakeups[0].payload.get("agentId").unwrap(), "agent-a");
    }

    #[tokio::test]
    async fn session_events_replay_and_trim() {
        let bus = bus();
        let id = bus
            .session_publish_event("s1", &payload(&[("type", "TOKEN")]))
            .await
            .unwrap();
        let events = bus.session_read_events("s1", None, 10).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].entry_id, id);
        bus.session_trim_events("s1").await.unwrap();
        assert!(bus.session_read_events("s1", None, 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn publish_reaches_current_subscribers_only() {
        let bus = bus();
        let mut rx = bus.subscribe("chan");
        bus.publish("chan", &payload(&[("m", "hello")])).await.unwrap();
        let got = rx.try_recv().unwrap();
        assert_eq!(got.get("m").unwrap(), "hello");
    }

    #[tokio::test]
    async fn async_tool_registry_lifecycle() {
        let registry = WorkspaceAsyncToolRegistry::new(
            Arc::new(InMemoryFilesystem::new()),
            "/bus/async-tools",
        );
        let record = AsyncToolRecord {
            id: "r1".into(),
            session_id: "s1".into(),
            tool_name: "long_task".into(),
            tool_call_id: "call-1".into(),
            status: AsyncToolRecord::RUNNING.into(),
            created_at: now_millis() - 10_000,
        };
        registry.register(&record).await.unwrap();

        // 未超 TTL：不判孤儿。
        assert!(registry
            .find_stale("s1", Duration::from_secs(60))
            .await
            .unwrap()
            .is_empty());
        // 超 TTL：判孤儿。
        let stale = registry.find_stale("s1", Duration::from_secs(5)).await.unwrap();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].id, "r1");

        // 标记超时后不再返回。
        registry.mark_timeout("r1").await.unwrap();
        assert!(registry
            .find_stale("s1", Duration::from_secs(5))
            .await
            .unwrap()
            .is_empty());

        // 完成 / 失败状态更新。
        registry.register(&record).await.unwrap();
        registry.complete("r1", "done").await.unwrap();
        let dir_entries = registry
            .fs
            .ls(&RuntimeContextOf::empty(), &registry.session_dir("s1"))
            .await
            .unwrap();
        let path = dir_entries
            .iter()
            .find(|f| !f.is_directory && f.path.ends_with("r1.json"))
            .map(|f| f.path.clone())
            .unwrap();
        let updated = registry.read_record(&path).await.unwrap().unwrap();
        assert_eq!(updated.status, AsyncToolRecord::COMPLETED);

        registry.register(&record).await.unwrap();
        registry.fail("r1", "boom").await.unwrap();
        let updated = registry.read_record(&path).await.unwrap().unwrap();
        assert_eq!(updated.status, AsyncToolRecord::FAILED);
    }

    #[tokio::test]
    async fn async_tool_registry_sanitizes_session_dir() {
        let registry = WorkspaceAsyncToolRegistry::new(
            Arc::new(InMemoryFilesystem::new()),
            "/bus/async-tools",
        );
        let record = AsyncToolRecord {
            id: "r2".into(),
            session_id: "a/b c".into(),
            tool_name: "t".into(),
            tool_call_id: "call".into(),
            status: AsyncToolRecord::RUNNING.into(),
            created_at: now_millis(),
        };
        registry.register(&record).await.unwrap();
        // 非法字符被替换为 _，仍能按会话找到。
        let stale = registry
            .find_stale("a/b c", Duration::from_secs(0))
            .await
            .unwrap();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].id, "r2");
    }
}
