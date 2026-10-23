//! 日志式会话存储（对应 Java `SessionLog` / `JournalSessionLog`）。
//!
//! # 机制
//!
//! 会话是一条**不可变的提交链**：每次提交写一个 commit 对象（含一批连续事件），
//! 然后用**一次**对 `head.json` 的 CAS 把链头推进——这就是「一次围栏 CAS 让整条链可见」。
//! 抢到链头之前，新数据对读者完全不可见（上传进来的孤儿 commit 也不会被读到）。
//!
//! 写者按 `(owner, epoch, lease)` 三件套围栏：
//! - `epoch` 每次获得所有权 +1，旧写者的 CAS 必然失败；
//! - `lease` 到期即视为失效（不依赖 GC 或心跳清理）；
//! - `seal` 是**永久**围栏，用于把某个写者身份彻底钉死（例如进程被判定为僵死），
//!   但**不会**吊销其他身份的活动写者。
//!
//! # 与 Java 的差异
//!
//! - `Iterable<SessionEvent>`（迭代时抛异常）→ `Vec<SessionEvent>`：错误在返回值里。
//! - 时钟可注入：Java 用 `Clock` 参数，Rust 用 `Arc<dyn Fn() -> i64>`（毫秒）。
//! - 大负载（> 64 KiB）外置到 `blobs/<sha256>`，commit 里只留 `{}` + `payloadBlobs` 映射，
//!   与 Java 一致；读回时校验 blob 的 hash。

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::events::SessionEvent;
use super::key::SessionHeader;
use super::storage::AtomicSessionStorage;
use super::{now_millis, SessionError};

/// 单个事件负载超过该字节数就外置成 blob（与 Java 的 65536 一致）。
const BLOB_THRESHOLD: usize = 65_536;

/// 链头与写者所有权的 CAS 重试上限（与 Java 一致）。
const CAS_ATTEMPTS: usize = 32;

/// 提交链头（对应 Java `SessionLog.Head`）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Head {
    /// 已提交的最大事件序号。
    pub seq: i64,
    /// 所有权代数：每次易主 +1，使旧写者的 CAS 必然失败。
    pub epoch: i64,
    /// 当前写者身份；`None` 表示无人持有（已释放或已封存）。
    #[serde(default)]
    pub owner: Option<String>,
    /// 租约到期的毫秒时间戳；`0` 表示无租约。
    pub lease_until: i64,
    /// 链头 commit 的 id；`None` 表示还没有提交。
    #[serde(default)]
    pub commit_id: Option<String>,
}

/// 写者句柄（对应 Java `SessionLog.Writer`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Writer {
    pub owner: String,
    pub epoch: i64,
}

/// 一次提交（对应 Java `JournalSessionLog.Commit`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Commit {
    pub batch_id: String,
    pub parent: Option<String>,
    pub first_seq: i64,
    pub last_seq: i64,
    /// 本批事件（已外置的负载在此为 `{}`）序列化字节的 SHA-256。
    pub hash: String,
    pub events: Vec<SessionEvent>,
    #[serde(default)]
    pub payload_blobs: HashMap<String, String>,
}

/// 会话日志的阻塞式 SPI（对应 Java `SessionLog`）。
///
/// 持久化失败**必须**向上传播——绝不能降级成「模型/工具成功」。
pub trait SessionLog: Send + Sync {
    /// 可持久化的输入收件箱视图。默认不支持。
    fn inbox(&self) -> Result<Box<dyn SessionLog>, SessionError> {
        Err(SessionError::unsupported(
            "this session backend does not support a durable inbox",
        ))
    }

    /// 当前链头（只读，不修复、不抢所有权）。
    fn head(&self) -> Result<Head, SessionError>;

    /// 申请写者所有权。
    fn acquire(&self, owner: &str, lease: Duration) -> Result<Writer, SessionError>;

    /// 永久封存某个写者身份（含尚未开始的申请）。默认不支持。
    fn seal_writer(&self, owner: &str) -> Result<Head, SessionError> {
        let _ = owner;
        Err(SessionError::unsupported(
            "this session backend cannot seal a writer",
        ))
    }

    /// 续租。
    fn renew(&self, writer: &Writer, lease: Duration) -> Result<(), SessionError>;

    /// 释放所有权。
    fn release(&self, writer: &Writer) -> Result<(), SessionError>;

    /// 提交一批事件（要求序号连续且与 `expected_seq` 衔接）。
    fn commit(
        &self,
        writer: &Writer,
        batch_id: &str,
        expected_seq: i64,
        events: Vec<SessionEvent>,
    ) -> Result<Head, SessionError>;

    /// 读取 `(after, after + limit]` 区间的事件。
    fn read_after(&self, after: i64, limit: usize) -> Result<Vec<SessionEvent>, SessionError>;

    /// 读取 `(after, through]` 区间的事件。
    ///
    /// Java 返回惰性 `Iterable`；Rust 侧返回 `Vec`（错误只能经返回值传播）。
    fn scan(&self, after: i64, through: i64) -> Result<Vec<SessionEvent>, SessionError> {
        if after < 0 || through < after {
            return Err(SessionError::invalid("invalid committed prefix"));
        }
        let mut collected = Vec::new();
        let mut cursor = after;
        while cursor < through {
            let batch = self.read_after(cursor, (through - cursor).min(256) as usize)?;
            if batch.is_empty() {
                return Err(SessionError::log("missing committed prefix"));
            }
            for event in batch {
                if event.seq <= cursor {
                    continue;
                }
                if event.seq != cursor + 1 {
                    return Err(SessionError::log("non-contiguous event sequence"));
                }
                cursor = event.seq;
                collected.push(event.clone());
            }
        }
        Ok(collected)
    }

    /// 外部读者的导出水位（0 表示从未导出）。
    fn export_cursor(&self, sink: &str) -> Result<i64, SessionError> {
        let _ = sink;
        Ok(0)
    }

    /// 推进导出水位。
    fn advance_export_cursor(
        &self,
        sink: &str,
        expected: i64,
        next: i64,
    ) -> Result<(), SessionError> {
        let _ = (sink, expected, next);
        Err(SessionError::unsupported(
            "export cursor persistence not implemented",
        ))
    }
}

/// 提交链 + 围栏写者（对应 Java `JournalSessionLog`）。
pub struct JournalSessionLog {
    storage: Arc<dyn AtomicSessionStorage>,
    prefix: String,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
}

impl std::fmt::Debug for JournalSessionLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JournalSessionLog")
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

impl JournalSessionLog {
    /// 用系统时钟构造。
    pub fn new(storage: Arc<dyn AtomicSessionStorage>, prefix: impl Into<String>) -> Self {
        Self::with_clock(storage, prefix, Arc::new(now_millis))
    }

    /// 注入时钟（测试围栏/租约用）。
    pub fn with_clock(
        storage: Arc<dyn AtomicSessionStorage>,
        prefix: impl Into<String>,
        clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> Self {
        Self {
            storage,
            prefix: prefix.into(),
            clock,
        }
    }

    /// 存储前缀（例如 `agents/s_xxx/sessions/s_yyy/s_zzz`）。
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// 直接访问底层存储（诊断与测试用）。
    pub fn storage(&self) -> &Arc<dyn AtomicSessionStorage> {
        &self.storage
    }

    fn path(&self, suffix: &str) -> String {
        format!("{}/{}", self.prefix, suffix)
    }

    fn now(&self) -> i64 {
        (self.clock)()
    }

    fn read_head_value(&self) -> Result<Option<super::storage::StorageValue>, SessionError> {
        self.storage.read(&self.path("head.json"))
    }

    fn decode_head(&self, value: Option<&super::storage::StorageValue>) -> Result<Head, SessionError> {
        match value {
            None => Ok(Head::default()),
            Some(value) => Ok(serde_json::from_str(value.as_str()?)?),
        }
    }

    fn seal_path(&self, owner: &str) -> String {
        self.path(&format!("writer-seals/{}.json", sha256_hex(owner.as_bytes())))
    }

    fn sealed(&self, owner: &str) -> Result<bool, SessionError> {
        Ok(self.storage.read(&self.seal_path(owner))?.is_some())
    }

    /// 校验写者仍然拥有链头（epoch 对齐、租约未过期、未被封存）。
    fn owned(&self, head: &Head, writer: &Writer) -> Result<(), SessionError> {
        if self.sealed(&writer.owner)? {
            return Err(SessionError::log("session writer is permanently sealed"));
        }
        if head.epoch != writer.epoch
            || head.owner.as_deref() != Some(writer.owner.as_str())
            || head.lease_until <= self.now()
        {
            return Err(SessionError::log(
                "session writer ownership expired or fenced",
            ));
        }
        Ok(())
    }

    fn update_owner(
        &self,
        writer: &Writer,
        lease: Duration,
        release: bool,
    ) -> Result<(), SessionError> {
        for _ in 0..CAS_ATTEMPTS {
            let value = self.read_head_value()?;
            let head = self.decode_head(value.as_ref())?;
            self.owned(&head, writer)?;
            let next = Head {
                seq: head.seq,
                epoch: head.epoch,
                owner: if release { None } else { head.owner.clone() },
                lease_until: if release {
                    0
                } else {
                    self.now() + lease.as_millis() as i64
                },
                commit_id: head.commit_id.clone(),
            };
            let expected = value.map(|value| value.version).unwrap_or(0);
            if self
                .storage
                .compare_and_set(&self.path("head.json"), expected, &encode(&next)?)?
            {
                return Ok(());
            }
        }
        Err(SessionError::log("writer renewal contention"))
    }
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, SessionError> {
    Ok(serde_json::to_vec(value)?)
}

impl SessionLog for JournalSessionLog {
    fn inbox(&self) -> Result<Box<dyn SessionLog>, SessionError> {
        Ok(Box::new(JournalSessionLog::with_clock(
            self.storage.clone(),
            self.path("inbox"),
            self.clock.clone(),
        )))
    }

    fn head(&self) -> Result<Head, SessionError> {
        // 头部格式标记存在时必须能通过校验，否则整条日志不可信。
        if let Some(header) = self.storage.read(&self.path("session.json"))? {
            let header: SessionHeader = serde_json::from_str(header.as_str()?)?;
            header.validate()?;
        }
        let value = self.read_head_value()?;
        self.decode_head(value.as_ref())
    }

    fn acquire(&self, owner: &str, lease: Duration) -> Result<Writer, SessionError> {
        if owner.trim().is_empty() {
            return Err(SessionError::invalid("empty writer owner"));
        }
        if lease.is_zero() {
            return Err(SessionError::invalid("lease must be positive"));
        }
        // 1) 首次打开时落地格式标记；已存在则校验权威来源。
        let desired = SessionHeader::current();
        self.storage
            .compare_and_set(&self.path("session.json"), 0, &encode(&desired)?)?;
        let stored = self
            .storage
            .read(&self.path("session.json"))?
            .ok_or_else(|| SessionError::log("missing session header"))?;
        let header: SessionHeader = serde_json::from_str(stored.as_str()?)?;
        header.validate()?;
        if header.authority != desired.authority {
            return Err(SessionError::log(
                "session authority mismatch; migrate into a new session instead of changing mode in place",
            ));
        }

        // 2) 抢链头：epoch +1 使旧写者失效。
        for _ in 0..CAS_ATTEMPTS {
            if self.sealed(owner)? {
                return Err(SessionError::log("session writer is permanently sealed"));
            }
            let value = self.read_head_value()?;
            let head = self.decode_head(value.as_ref())?;
            if head.owner.is_some() && head.lease_until > self.now() {
                return Err(SessionError::log("session already has an active writer"));
            }
            let next = Head {
                seq: head.seq,
                epoch: head.epoch + 1,
                owner: Some(owner.to_string()),
                lease_until: self.now() + lease.as_millis() as i64,
                commit_id: head.commit_id.clone(),
            };
            let expected = value.map(|value| value.version).unwrap_or(0);
            if self
                .storage
                .compare_and_set(&self.path("head.json"), expected, &encode(&next)?)?
            {
                // 封存可能落在「初次检查」与「读到链头」之间：绝不把写者交给已被封存的身份，
                // 并把这次抢到的所有权还回去。
                if self.sealed(owner)? {
                    self.seal_writer(owner)?;
                    return Err(SessionError::log("session writer is permanently sealed"));
                }
                return Ok(Writer {
                    owner: owner.to_string(),
                    epoch: next.epoch,
                });
            }
        }
        Err(SessionError::log("writer acquisition contention"))
    }

    fn seal_writer(&self, owner: &str) -> Result<Head, SessionError> {
        if owner.trim().is_empty() {
            return Err(SessionError::invalid("empty writer owner"));
        }
        let mut seal = HashMap::new();
        seal.insert("owner", owner);
        self.storage
            .compare_and_set(&self.seal_path(owner), 0, &encode(&seal)?)?;
        for _ in 0..CAS_ATTEMPTS {
            let value = self.read_head_value()?;
            let head = self.decode_head(value.as_ref())?;
            let next = if head.owner.as_deref() == Some(owner) {
                Head {
                    seq: head.seq,
                    epoch: head.epoch + 1,
                    owner: None,
                    lease_until: 0,
                    commit_id: head.commit_id,
                }
            } else {
                head
            };
            // 即使链头未变也要推进存储版本：这样能围栏掉「暂停中的申请/提交」，
            // 又不会吊销其他身份的活动写者。
            let expected = value.map(|value| value.version).unwrap_or(0);
            if self
                .storage
                .compare_and_set(&self.path("head.json"), expected, &encode(&next)?)?
            {
                return Ok(next);
            }
        }
        Err(SessionError::log("writer sealing contention"))
    }

    fn renew(&self, writer: &Writer, lease: Duration) -> Result<(), SessionError> {
        self.update_owner(writer, lease, false)
    }

    fn release(&self, writer: &Writer) -> Result<(), SessionError> {
        self.update_owner(writer, Duration::ZERO, true)
    }

    fn commit(
        &self,
        writer: &Writer,
        batch_id: &str,
        expected_seq: i64,
        events: Vec<SessionEvent>,
    ) -> Result<Head, SessionError> {
        if events.is_empty() {
            return self.head();
        }
        let frozen = events;
        let mut seq = expected_seq;
        for event in &frozen {
            seq += 1;
            if event.seq != seq {
                return Err(SessionError::log("non-contiguous event batch"));
            }
        }
        let hash = sha256_hex(&serde_json::to_vec(&frozen)?);
        let id = sha256_hex(batch_id.as_bytes());
        let commit_path = self.path(&format!("commits/{id}.json"));

        for _ in 0..CAS_ATTEMPTS {
            let value = self.read_head_value()?;
            let head = self.decode_head(value.as_ref())?;

            // 丢失 ACK 的重放：即使后续批次已提交，也沿链回溯找到自己这一批。
            if head.seq >= seq {
                let mut cursor = head.commit_id.clone();
                while let Some(cursor_id) = cursor {
                    let existing = self.read_commit(&cursor_id)?;
                    if existing.batch_id == batch_id {
                        if existing.hash != hash || existing.first_seq != expected_seq + 1 {
                            return Err(SessionError::log(
                                "batch ID reused with different content",
                            ));
                        }
                        self.owned(&head, writer)?;
                        return Ok(head);
                    }
                    if existing.last_seq < expected_seq + 1 {
                        break;
                    }
                    cursor = existing.parent.clone();
                }
            }

            self.owned(&head, writer)?;
            if head.seq != expected_seq {
                return Err(SessionError::log("stale committed prefix"));
            }

            // 大负载外置成不可变 blob；commit 里只留 `{}` 与映射。
            let mut payload_blobs: HashMap<String, String> = HashMap::new();
            let mut stored = Vec::with_capacity(frozen.len());
            for event in &frozen {
                if event.payload_len() > BLOB_THRESHOLD {
                    let payload = event.payload_json.as_bytes();
                    let blob = sha256_hex(payload);
                    let blob_path = self.path(&format!("blobs/{blob}"));
                    if !self.storage.compare_and_set(&blob_path, 0, payload)? {
                        let existing = self
                            .storage
                            .read(&blob_path)?
                            .ok_or_else(|| SessionError::log("immutable payload blob conflict"))?;
                        if existing.bytes != payload {
                            return Err(SessionError::log("immutable payload blob conflict"));
                        }
                    }
                    payload_blobs.insert(event.event_id.clone(), blob);
                    stored.push(event.with_payload_json("{}"));
                } else {
                    stored.push(event.clone());
                }
            }

            let commit = Commit {
                batch_id: batch_id.to_string(),
                parent: head.commit_id.clone(),
                first_seq: expected_seq + 1,
                last_seq: seq,
                hash: hash.clone(),
                events: stored,
                payload_blobs,
            };
            let data = encode(&commit)?;
            if !self.storage.compare_and_set(&commit_path, 0, &data)? {
                let existing = self
                    .storage
                    .read(&commit_path)?
                    .ok_or_else(|| SessionError::log("immutable commit conflict"))?;
                if existing.bytes != data {
                    return Err(SessionError::log("immutable commit conflict"));
                }
            }

            self.owned(&head, writer)?;
            let next = Head {
                seq,
                epoch: head.epoch,
                owner: head.owner.clone(),
                lease_until: head.lease_until,
                commit_id: Some(id.clone()),
            };
            let expected = value.map(|value| value.version).unwrap_or(0);
            match self.storage.compare_and_set(
                &self.path("head.json"),
                expected,
                &encode(&next)?,
            ) {
                Ok(true) => return Ok(next),
                Ok(false) => continue,
                Err(error) => {
                    // 存储错误也可能只是丢了 ACK：先确认链头是否已经推进到自己这一批。
                    if let Ok(observed) = self.head() {
                        if observed.commit_id.as_deref() == Some(id.as_str())
                            && observed.seq == seq
                        {
                            self.owned(&observed, writer)?;
                            return Ok(observed);
                        }
                    }
                    return Err(error);
                }
            }
        }
        Err(SessionError::log("commit contention"))
    }

    fn read_after(&self, after: i64, limit: usize) -> Result<Vec<SessionEvent>, SessionError> {
        if limit < 1 {
            return Err(SessionError::invalid("invalid limit"));
        }
        let through = self.head()?.seq.min(after + limit as i64);
        if through <= after {
            return Ok(Vec::new());
        }
        self.scan(after, through)
    }

    fn scan(&self, after: i64, through: i64) -> Result<Vec<SessionEvent>, SessionError> {
        let head = self.head()?;
        if after < 0 || through < after || through > head.seq {
            return Err(SessionError::invalid("invalid committed prefix"));
        }

        // 1) 从链头回溯，收集覆盖 (after, through] 的 commit id（结构校验顺带做）。
        let mut ids: VecDeque<String> = VecDeque::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut expected = head.seq;
        let mut cursor = head.commit_id;
        while let Some(id) = cursor {
            if expected <= after {
                break;
            }
            if !seen.insert(id.clone()) {
                return Err(SessionError::log("commit chain cycle"));
            }
            let stored = self
                .storage
                .read(&self.path(&format!("commits/{id}.json")))?
                .ok_or_else(|| SessionError::log("missing committed segment"))?;
            let commit: Commit = serde_json::from_str(stored.as_str()?)?;
            if commit.last_seq != expected
                || commit.first_seq + commit.events.len() as i64 - 1 != expected
                || commit.events.is_empty()
            {
                return Err(SessionError::log("broken committed prefix"));
            }
            if commit.first_seq <= through {
                ids.push_front(id);
            }
            expected = commit.first_seq - 1;
            cursor = commit.parent;
        }
        if expected > after {
            return Err(SessionError::log("incomplete committed chain"));
        }

        // 2) 正序读出并校验连续性（同时解 blob、校验 checksum）。
        let mut collected = Vec::new();
        let mut position = after;
        for id in ids {
            let commit = self.read_commit(&id)?;
            for event in commit.events {
                if event.seq <= position {
                    continue;
                }
                if event.seq != position + 1 {
                    return Err(SessionError::log("non-contiguous event sequence"));
                }
                if event.seq > through {
                    return Ok(collected);
                }
                position = event.seq;
                collected.push(event);
            }
        }
        if position < through {
            return Err(SessionError::log("missing committed events"));
        }
        Ok(collected)
    }

    fn export_cursor(&self, sink: &str) -> Result<i64, SessionError> {
        let path = self.path(&format!("exports/{}.json", sha256_hex(sink.as_bytes())));
        match self.storage.read(&path)? {
            None => Ok(0),
            Some(value) => Ok(serde_json::from_str(value.as_str()?)?),
        }
    }

    fn advance_export_cursor(
        &self,
        sink: &str,
        expected: i64,
        next: i64,
    ) -> Result<(), SessionError> {
        if next <= expected || next > self.head()?.seq {
            return Err(SessionError::invalid("invalid export watermark"));
        }
        let path = self.path(&format!("exports/{}.json", sha256_hex(sink.as_bytes())));
        for _ in 0..CAS_ATTEMPTS {
            let value = self.storage.read(&path)?;
            let current: i64 = match &value {
                None => 0,
                Some(value) => serde_json::from_str(value.as_str()?)?,
            };
            if current >= next {
                return Ok(());
            }
            if current != expected {
                return Err(SessionError::log("export cursor conflict"));
            }
            let version = value.map(|value| value.version).unwrap_or(0);
            if self
                .storage
                .compare_and_set(&path, version, &encode(&next)?)?
            {
                return Ok(());
            }
        }
        Err(SessionError::log("export cursor contention"))
    }
}

impl JournalSessionLog {
    fn read_commit(&self, id: &str) -> Result<Commit, SessionError> {
        let stored = self
            .storage
            .read(&self.path(&format!("commits/{id}.json")))?
            .ok_or_else(|| SessionError::log(format!("missing committed segment: {id}")))?;
        let mut commit: Commit = serde_json::from_str(stored.as_str()?)?;

        if !commit.payload_blobs.is_empty() {
            let mut hydrated = Vec::with_capacity(commit.events.len());
            for event in &commit.events {
                match commit.payload_blobs.get(&event.event_id) {
                    None => hydrated.push(event.clone()),
                    Some(blob) => {
                        let payload = self
                            .storage
                            .read(&self.path(&format!("blobs/{blob}")))?
                            .ok_or_else(|| {
                                SessionError::log("missing or corrupt committed payload blob")
                            })?;
                        if sha256_hex(&payload.bytes) != *blob {
                            return Err(SessionError::log(
                                "missing or corrupt committed payload blob",
                            ));
                        }
                        hydrated.push(event.with_payload_json(payload.as_str()?.to_string()));
                    }
                }
            }
            commit.events = hydrated;
        }

        if sha256_hex(&serde_json::to_vec(&commit.events)?) != commit.hash {
            return Err(SessionError::log("committed segment checksum mismatch"));
        }
        Ok(commit)
    }
}

// ---------------------------------------------------------------------------
// SHA-256（hex 小写）
// ---------------------------------------------------------------------------

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// SHA-256 十六进制摘要（小写）。
///
/// 手写而不引 `sha2`：只在这一处用到，且已有手写 ISO-8601 的先例——少一个依赖，
/// 离线环境也不用担心索引缓存失效（见 `easyclaw-build-env-notes`）。
pub fn sha256_hex(data: &[u8]) -> String {
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];

    // 补位：0x80 + 若干 0 + 64 位大端比特长度。
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity(data.len() + 72);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for block in padded.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (index, chunk) in block.chunks_exact(4).enumerate() {
            w[index] = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        for index in 16..64 {
            let s0 = w[index - 15].rotate_right(7)
                ^ w[index - 15].rotate_right(18)
                ^ (w[index - 15] >> 3);
            let s1 = w[index - 2].rotate_right(17)
                ^ w[index - 2].rotate_right(19)
                ^ (w[index - 2] >> 10);
            w[index] = w[index - 16]
                .wrapping_add(s0)
                .wrapping_add(w[index - 7])
                .wrapping_add(s1);
        }

        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h) = (
            state[0], state[1], state[2], state[3], state[4], state[5], state[6], state[7],
        );
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[index])
                .wrapping_add(w[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }

    state
        .iter()
        .map(|word| format!("{word:08x}"))
        .collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::storage::MemorySessionStorage;
    use crate::session::store::{InMemorySessionLogStore, SessionLogStore};
    use crate::session::SessionKey;
    use std::sync::{Mutex, OnceLock};

    fn key() -> SessionKey {
        SessionKey::new(Some("u"), "a", "s").unwrap()
    }

    fn log() -> Arc<dyn SessionLog> {
        InMemorySessionLogStore::new().open(&key(), None).unwrap()
    }

    fn event(seq: i64, kind: &str) -> SessionEvent {
        SessionEvent::new(format!("e{seq}"), seq, 1, kind, Some("r".into()), Some("t".into()), true, "{}")
            .unwrap()
    }

    const LEASE: Duration = Duration::from_secs(120);

    // ---------- SHA-256 ----------

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // 跨 block 边界（55/56/64 字节）不 panic 且互不相同。
        let hashes: Vec<String> = [55usize, 56, 64, 65]
            .iter()
            .map(|len| sha256_hex(&vec![b'a'; *len]))
            .collect();
        let unique: HashSet<&String> = hashes.iter().collect();
        assert_eq!(unique.len(), hashes.len());
        assert_eq!(hashes[2].len(), 64);
    }

    // ---------- 提交链 ----------

    #[test]
    fn committed_prefix_is_ordered_and_batch_replay_is_idempotent() {
        let log = log();
        let writer = log.acquire("w", LEASE).unwrap();
        let batch = vec![event(1, "run/start"), event(2, "run/end")];
        log.commit(&writer, "batch", 0, batch.clone()).unwrap();
        // 同一 batchId 重放：幂等，不重复写入。
        log.commit(&writer, "batch", 0, batch.clone()).unwrap();
        assert_eq!(log.head().unwrap().seq, 2);
        assert_eq!(log.read_after(0, 10).unwrap(), batch);
        assert_eq!(log.read_after(1, 10).unwrap(), vec![batch[1].clone()]);
        // 同一 batchId 换内容：拒绝。
        let err = log
            .commit(&writer, "batch", 0, vec![event(1, "turn/start")])
            .unwrap_err();
        assert!(matches!(err, SessionError::Log(_)));
    }

    #[test]
    fn commit_rejects_non_contiguous_batch_and_stale_prefix() {
        let log = log();
        let writer = log.acquire("w", LEASE).unwrap();
        // 批次内序号必须连续。
        assert!(log
            .commit(&writer, "b1", 0, vec![event(1, "run/start"), event(3, "run/end")])
            .is_err());
        log.commit(&writer, "b1", 0, vec![event(1, "run/start")]).unwrap();
        // expected_seq 落后 → 陈旧前缀。
        let err = log
            .commit(&writer, "b2", 0, vec![event(2, "turn/start")])
            .unwrap_err();
        assert!(matches!(err, SessionError::Log(_)));
        // 正确的衔接。
        log.commit(&writer, "b2", 1, vec![event(2, "turn/start")]).unwrap();
        assert_eq!(log.head().unwrap().seq, 2);
    }

    #[test]
    fn empty_batch_is_a_noop() {
        let log = log();
        let writer = log.acquire("w", LEASE).unwrap();
        let head = log.commit(&writer, "empty", 0, Vec::new()).unwrap();
        assert_eq!(head.seq, 0);
        assert!(head.commit_id.is_none());
    }

    #[test]
    fn stale_writers_are_fenced_and_lease_cannot_be_stolen() {
        let log = log();
        let first = log.acquire("first", LEASE).unwrap();
        // 别人还在租约内 → 不能抢。
        assert!(log.acquire("second", LEASE).is_err());
        log.release(&first).unwrap();
        let second = log.acquire("second", LEASE).unwrap();
        assert!(second.epoch > first.epoch);
        // 旧写者的提交被围栏。
        let err = log
            .commit(&first, "old", 0, vec![event(1, "run/start")])
            .unwrap_err();
        assert!(matches!(err, SessionError::Log(_)));
        log.commit(&second, "new", 0, vec![event(1, "run/start")]).unwrap();
        assert_eq!(log.head().unwrap().seq, 1);
    }

    #[test]
    fn expired_lease_allows_takeover() {
        // 注入时钟：租约 1 秒，跳过后失效。
        let clock = Arc::new(Mutex::new(1_000i64));
        let storage = Arc::new(MemorySessionStorage::new());
        let erased: Arc<dyn AtomicSessionStorage> = storage;
        let reading = clock.clone();
        let log = JournalSessionLog::with_clock(
            erased,
            "agents/x",
            Arc::new(move || *reading.lock().unwrap()),
        );
        let first = log.acquire("first", Duration::from_secs(1)).unwrap();
        assert!(log.acquire("second", Duration::from_secs(1)).is_err());
        *clock.lock().unwrap() += 2_000;
        let second = log.acquire("second", Duration::from_secs(1)).unwrap();
        assert!(second.epoch > first.epoch);
        assert!(log.commit(&first, "b", 0, vec![event(1, "run/start")]).is_err());
    }

    #[test]
    fn read_only_inspection_does_not_acquire() {
        let log = log();
        let writer = log.acquire("w", LEASE).unwrap();
        log.commit(&writer, "b", 0, vec![event(1, "run/start")]).unwrap();
        let head = log.head().unwrap();
        assert_eq!(head.epoch, 1);
        assert_eq!(head.owner.as_deref(), Some("w"));
        // 读十次也不会改状态。
        for _ in 0..10 {
            assert_eq!(log.head().unwrap(), head);
        }
        assert_eq!(log.read_after(0, 10).unwrap().len(), 1);
        assert_eq!(log.head().unwrap(), head);
    }

    #[test]
    fn missing_committed_data_fails_closed() {
        let store = InMemorySessionLogStore::new();
        let log = store.open(&key(), None).unwrap();
        let writer = log.acquire("w", LEASE).unwrap();
        log.commit(&writer, "b", 0, vec![event(1, "run/start")]).unwrap();
        // 删除已提交的链段：宁可报错，也不能返回「空历史」。
        let storage = store.storage(&key()).unwrap();
        assert_eq!(storage.remove_matching("/commits/"), 1);
        assert!(log.read_after(0, 10).is_err());
        assert!(log.scan(0, 1).is_err());
    }

    #[test]
    fn orphan_commit_is_invisible() {
        // 只写了 commit 对象、没推进链头 → 读者看不到（上传孤儿不污染历史）。
        let storage = Arc::new(MemorySessionStorage::new());
        let erased: Arc<dyn AtomicSessionStorage> = storage.clone();
        let log = JournalSessionLog::new(erased, "p");
        log.acquire("w", LEASE).unwrap();
        let orphan = Commit {
            batch_id: "ghost".into(),
            parent: None,
            first_seq: 1,
            last_seq: 1,
            hash: sha256_hex(&serde_json::to_vec(&vec![event(1, "run/start")]).unwrap()),
            events: vec![event(1, "run/start")],
            payload_blobs: HashMap::new(),
        };
        storage
            .compare_and_set(
                "p/commits/ghost.json",
                0,
                &serde_json::to_vec(&orphan).unwrap(),
            )
            .unwrap();
        assert_eq!(log.head().unwrap().seq, 0);
        assert!(log.read_after(0, 10).unwrap().is_empty());
    }

    #[test]
    fn large_payload_spills_into_content_addressed_blob() {
        let storage = Arc::new(MemorySessionStorage::new());
        let erased: Arc<dyn AtomicSessionStorage> = storage.clone();
        let log = JournalSessionLog::new(erased, "p");
        let writer = log.acquire("w", LEASE).unwrap();
        let big = "x".repeat(BLOB_THRESHOLD + 1);
        let payload = serde_json::json!({ "text": big });
        let event = SessionEvent::new(
            "e1",
            1,
            1,
            "message/assistant",
            None,
            None,
            true,
            serde_json::to_string(&payload).unwrap(),
        )
        .unwrap();
        log.commit(&writer, "b", 0, vec![event.clone()]).unwrap();

        // 存储里 commit 只留 `{}`，大负载在 blobs/<sha256>。
        let paths = storage.list_paths("p/blobs/").unwrap();
        assert_eq!(paths.len(), 1);
        let commit_raw = storage.read("p/commits/").unwrap_or(None);
        assert!(commit_raw.is_none());

        // 读回时完整还原。
        let read = log.read_after(0, 10).unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].payload_json, event.payload_json);
        assert_eq!(read[0].data().unwrap()["text"].as_str().unwrap().len(), BLOB_THRESHOLD + 1);

        // 篡改 blob → 校验失败（不返回被改过的历史）。
        let blob_path = paths[0].clone();
        let corrupted = serde_json::to_vec(&serde_json::json!({ "text": "evil" })).unwrap();
        let current = storage.read(&blob_path).unwrap().unwrap();
        storage
            .compare_and_set(&blob_path, current.version, &corrupted)
            .unwrap();
        assert!(log.read_after(0, 10).is_err());
    }

    #[test]
    fn scan_rejects_out_of_range_requests() {
        let log = log();
        let writer = log.acquire("w", LEASE).unwrap();
        log.commit(&writer, "b", 0, vec![event(1, "run/start"), event(2, "run/end")])
            .unwrap();
        assert_eq!(log.scan(0, 2).unwrap().len(), 2);
        assert_eq!(log.scan(1, 2).unwrap().len(), 1);
        assert!(log.scan(0, 3).is_err());
        assert!(log.scan(2, 1).is_err());
        assert!(log.scan(-1, 1).is_err());
    }

    #[test]
    fn export_cursor_advances_monotonically() {
        let log = log();
        let writer = log.acquire("w", LEASE).unwrap();
        log.commit(&writer, "b", 0, vec![event(1, "run/start"), event(2, "run/end")])
            .unwrap();
        assert_eq!(log.export_cursor("sink").unwrap(), 0);
        log.advance_export_cursor("sink", 0, 1).unwrap();
        assert_eq!(log.export_cursor("sink").unwrap(), 1);
        // 已推进过 → 幂等返回。
        log.advance_export_cursor("sink", 0, 1).unwrap();
        assert_eq!(log.export_cursor("sink").unwrap(), 1);
        // 越过已提交链头 → 拒绝。
        assert!(log.advance_export_cursor("sink", 1, 3).is_err());
        // 期望值与实际不符 → 冲突（当前 1，却声称 0 要推到 2）。
        assert!(log.advance_export_cursor("sink", 0, 2).is_err());
        // 正确的推进。
        log.advance_export_cursor("sink", 1, 2).unwrap();
        assert_eq!(log.export_cursor("sink").unwrap(), 2);
        // 不允许原地踏步或回退。
        assert!(log.advance_export_cursor("sink", 2, 2).is_err());
        assert_eq!(log.export_cursor("sink").unwrap(), 2);
    }

    #[test]
    fn header_is_written_once_and_rejects_foreign_authority() {
        let storage = Arc::new(MemorySessionStorage::new());
        let erased: Arc<dyn AtomicSessionStorage> = storage.clone();
        let log = JournalSessionLog::new(erased, "p");
        log.acquire("w", LEASE).unwrap();
        let header = storage.read("p/session.json").unwrap().unwrap();
        let parsed: SessionHeader = serde_json::from_str(header.as_str().unwrap()).unwrap();
        parsed.validate().unwrap();
        assert_eq!(parsed.authority, "event_log");

        // 权威来源被改成影子模式 → 打开即失败。
        let tampered = serde_json::json!({
            "formatVersion": 1, "authority": "shadow",
            "replayCoverage": "adapter_chunks", "createdAt": 0
        });
        storage
            .compare_and_set("p/session.json", header.version, &serde_json::to_vec(&tampered).unwrap())
            .unwrap();
        assert!(log.acquire("w2", LEASE).is_err());
        assert!(log.head().is_err());
    }

    // ---------- 封存围栏（需要钩子存储） ----------

    #[derive(Default)]
    struct HookStorage {
        inner: MemorySessionStorage,
        before_head_read: Mutex<Vec<Box<dyn Fn() + Send>>>,
        before_head_cas: Mutex<Vec<Box<dyn Fn() + Send>>>,
    }

    impl HookStorage {
        fn pop(queue: &Mutex<Vec<Box<dyn Fn() + Send>>>) {
            let hook = {
                let mut queue = queue.lock().unwrap();
                if queue.is_empty() {
                    None
                } else {
                    Some(queue.remove(0))
                }
            };
            if let Some(hook) = hook {
                hook();
            }
        }
    }

    impl AtomicSessionStorage for HookStorage {
        fn read(&self, path: &str) -> Result<Option<super::super::storage::StorageValue>, SessionError> {
            if path.ends_with("head.json") {
                Self::pop(&self.before_head_read);
            }
            self.inner.read(path)
        }

        fn compare_and_set(
            &self,
            path: &str,
            expected_version: i64,
            bytes: &[u8],
        ) -> Result<bool, SessionError> {
            if path.ends_with("head.json") {
                Self::pop(&self.before_head_cas);
            }
            self.inner.compare_and_set(path, expected_version, bytes)
        }
    }

    #[test]
    fn permanent_seal_fences_paused_acquisition_and_commit_but_allows_new_run() {
        let storage = Arc::new(HookStorage::default());
        let erased: Arc<dyn AtomicSessionStorage> = storage.clone();
        let log: Arc<dyn SessionLog> = Arc::new(JournalSessionLog::new(erased, "sealed"));
        let holder: Arc<OnceLock<Arc<dyn SessionLog>>> = Arc::new(OnceLock::new());
        holder.set(log.clone()).ok().unwrap();

        // 封存落在「初次检查之后、读链头之前」。
        {
            let holder = holder.clone();
            storage
                .before_head_read
                .lock()
                .unwrap()
                .push(Box::new(move || {
                    holder.get().unwrap().seal_writer("paused").unwrap();
                }));
        }
        assert!(log.acquire("paused", LEASE).is_err());

        // 其他身份不受影响。
        let fresh = log.acquire("fresh", LEASE).unwrap();
        log.commit(&fresh, "first", 0, vec![event(1, "run/start")])
            .unwrap();

        // 提交途中的封存：CAS 前落 seal，提交必须失败且链头不动。
        {
            let holder = holder.clone();
            storage
                .before_head_cas
                .lock()
                .unwrap()
                .push(Box::new(move || {
                    holder.get().unwrap().seal_writer("fresh").unwrap();
                }));
        }
        assert!(log
            .commit(&fresh, "late", 1, vec![event(2, "run/end")])
            .is_err());
        assert_eq!(log.head().unwrap().seq, 1);

        // 被封存的身份再也开不了；换身份可以继续这条会话。
        let restarted: Arc<dyn SessionLog> = Arc::new(JournalSessionLog::new(
            storage.clone() as Arc<dyn AtomicSessionStorage>,
            "sealed",
        ));
        assert!(restarted.acquire("fresh", LEASE).is_err());
        let resume = restarted.acquire("resume-with-new-id", LEASE).unwrap();
        // 封存一个无关身份不得吊销当前写者。
        restarted.seal_writer("paused").unwrap();
        restarted
            .commit(&resume, "resumed", 1, vec![event(2, "run/start")])
            .unwrap();
        assert_eq!(restarted.head().unwrap().seq, 2);
    }
}


