//! 持久化命令收件箱（对应 Java `SessionInbox`）。
//!
//! 收件箱解决的是「谁来接收用户输入」：执行中的会话已经被写者占住，外部投递
//! 不能去抢那把租约，否则两个写者会互相踩。所以收件箱是**独立的子日志**
//! （`<session>/inbox`），只做一件事：把命令（submit / resume / respond /
//! cancel …）原子地记下来，由执行方在回合边界上按序消费。
//!
//! 五类事件：`inbox/accepted` `inbox/opened` `inbox/closed` `inbox/rejected`
//! `inbox/handled`。**其他任何事件都是日志损坏**，直接失败而不猜。
//!
//! 收件箱的短租约（2 分钟）永远不与执行写者的租约重叠。

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::events::SessionEvent;
use super::journal::{SessionLog, Writer};
use super::SessionError;
use crate::message::Msg;

/// 收件箱租约时长（对应 Java `Duration.ofMinutes(2)`）。
pub const INBOX_LEASE: Duration = Duration::from_secs(120);
/// 抢租约的最大重试次数（对应 Java `attempt >= 99`）。
const MAX_ACQUIRE_ATTEMPTS: u32 = 100;
/// 重试间隔（对应 Java `Thread.sleep(10)`）。
const ACQUIRE_BACKOFF: Duration = Duration::from_millis(10);

/// 一条待消费的会话命令。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Command {
    /// 幂等键：同 id 同内容只记一次。
    pub id: String,
    /// 命令类型（`submit` / `resume` / `respond` / `cancel` …）。
    pub kind: String,
    /// 属于哪个逻辑回合（`cancel` 之类可能没有）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// 随命令携带的用户输入。
    #[serde(default)]
    pub messages: Vec<Msg>,
    /// 续跑要回填的未决请求 id（`respond` 用）。
    #[serde(default)]
    pub request_ids: Vec<String>,
}

impl Command {
    /// 新命令。
    pub fn new(id: impl Into<String>, kind: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind: kind.into(),
            turn_id: None,
            messages: Vec::new(),
            request_ids: Vec::new(),
        }
    }

    /// 归属逻辑回合。
    pub fn with_turn(mut self, turn_id: impl Into<String>) -> Self {
        self.turn_id = Some(turn_id.into());
        self
    }

    /// 携带输入消息。
    pub fn with_messages(mut self, messages: Vec<Msg>) -> Self {
        self.messages = messages;
        self
    }

    /// 携带未决请求 id。
    pub fn with_request_ids(mut self, request_ids: Vec<String>) -> Self {
        self.request_ids = request_ids;
        self
    }

    /// 是否会驱动一次执行（`cancel` 之类只做标记）。
    pub fn execution(&self) -> bool {
        matches!(self.kind.as_str(), "submit" | "resume" | "respond")
    }

    /// 去掉时间戳后的可比较形态（幂等判据与 Java 一致：只有 timestamp 不算内容）。
    fn comparable(&self) -> Self {
        let mut clone = self.clone();
        for message in &mut clone.messages {
            message.timestamp = None;
        }
        clone
    }
}

/// 收件箱快照。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    /// 已接收的命令（插入序，含已被拒绝/消费的）。
    pub commands: Vec<Command>,
    /// 当前打开的回合（`inbox/opened` 之后、匹配的 `inbox/closed` 之前）。
    pub turn_id: Option<String>,
    /// 当前打开回合的执行标识。
    pub run_id: Option<String>,
    /// 被拒绝的命令 → 原因（插入序）。
    pub rejected: Vec<(String, String)>,
    /// 已消费的命令 id（插入序）。
    pub handled: Vec<String>,
}

impl Snapshot {
    /// 某个命令被拒绝的原因。
    pub fn rejected_reason(&self, command_id: &str) -> Option<&str> {
        self.rejected
            .iter()
            .find(|(id, _)| id == command_id)
            .map(|(_, reason)| reason.as_str())
    }

    /// 某个命令是否已被消费。
    pub fn is_handled(&self, command_id: &str) -> bool {
        self.handled.iter().any(|id| id == command_id)
    }

    /// 还有没有待执行的命令（未拒绝、未消费且会驱动执行）。
    pub fn pending(&self) -> impl Iterator<Item = &Command> {
        self.commands.iter().filter(|command| {
            command.execution()
                && !self.is_handled(&command.id)
                && self.rejected_reason(&command.id).is_none()
        })
    }
}

/// 收件箱（对应 Java `SessionInbox`，内部持有独立的子日志）。
pub struct SessionInbox {
    journal: Box<dyn SessionLog>,
}

impl SessionInbox {
    /// 从会话日志派生出收件箱子日志。
    ///
    /// 后端不支持持久收件箱时返回 [`SessionError::Unsupported`]。
    pub fn new(log: &dyn SessionLog) -> Result<Self, SessionError> {
        Ok(Self {
            journal: log.inbox()?,
        })
    }

    /// 当前快照（只读，不抢租约）。
    pub fn snapshot(&self) -> Result<Snapshot, SessionError> {
        let upper = self.journal.head()?.seq;
        let mut snapshot = Snapshot::default();
        for event in self.journal.scan(0, upper)? {
            let data = event.data()?;
            match event.kind.as_str() {
                "inbox/accepted" => {
                    let command: Command = serde_json::from_value(data.clone())?;
                    snapshot.commands.push(command);
                }
                "inbox/opened" => {
                    snapshot.turn_id = event.turn_id.clone();
                    snapshot.run_id = event.execution_run_id.clone();
                }
                "inbox/closed" => {
                    // 只有「关闭的是当前那个运行」才真正关掉，否则是过期的关闭事件。
                    if snapshot.run_id == event.execution_run_id {
                        snapshot.turn_id = None;
                        snapshot.run_id = None;
                    }
                }
                "inbox/rejected" => {
                    if let Some(id) = data.get("commandId").and_then(|value| value.as_str()) {
                        let reason = data
                            .get("reason")
                            .and_then(|value| value.as_str())
                            .unwrap_or_default();
                        snapshot.rejected.push((id.to_string(), reason.to_string()));
                    }
                }
                "inbox/handled" => {
                    if let Some(id) = data.get("commandId").and_then(|value| value.as_str()) {
                        snapshot.handled.push(id.to_string());
                    }
                }
                other => {
                    // 损坏的收件箱日志：宁可拒绝服务，也不要静默丢命令。
                    return Err(SessionError::log(format!("unknown inbox event: {other}")));
                }
            }
        }
        Ok(snapshot)
    }

    /// 在收件箱租约内执行一段操作（对应 Java `locked`）。
    ///
    /// 这个调用是**阻塞**的：抢不到租约会短暂重试。请在阻塞线程里调用
    /// （如 `tokio::task::spawn_blocking`），不要直接放异步任务里。
    pub fn locked<T>(
        &self,
        operation: impl FnOnce(&Transaction<'_>) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        let writer = self.acquire()?;
        let outcome = operation(&Transaction {
            inbox: self,
            writer: &writer,
        });
        match outcome {
            Ok(value) => {
                self.journal.release(&writer)?;
                Ok(value)
            }
            Err(error) => {
                // 操作失败优先返回操作错误；释放失败只会在日志里留下痕迹。
                let _ = self.journal.release(&writer);
                Err(error)
            }
        }
    }

    fn acquire(&self) -> Result<Writer, SessionError> {
        let owner = uuid::Uuid::new_v4().to_string();
        let mut attempt = 0;
        loop {
            match self.journal.acquire(&owner, INBOX_LEASE) {
                Ok(writer) => return Ok(writer),
                Err(error) => {
                    let message = error.to_string();
                    let retryable =
                        message.contains("active writer") || message.contains("contention");
                    if !retryable || attempt + 1 >= MAX_ACQUIRE_ATTEMPTS {
                        return Err(error);
                    }
                    attempt += 1;
                    std::thread::sleep(ACQUIRE_BACKOFF);
                }
            }
        }
    }
}

/// 收件箱写事务（对应 Java `SessionInbox.Transaction`）。
pub struct Transaction<'a> {
    inbox: &'a SessionInbox,
    writer: &'a Writer,
}

impl Transaction<'_> {
    /// 事务内的最新快照。
    pub fn snapshot(&self) -> Result<Snapshot, SessionError> {
        self.inbox.snapshot()
    }

    /// 接收命令。同 id 同内容幂等返回既有命令；同 id 不同内容直接报错。
    pub fn accept(&self, command: Command) -> Result<Command, SessionError> {
        for existing in self.snapshot()?.commands {
            if existing.id != command.id {
                continue;
            }
            if existing.comparable() != command.comparable() {
                return Err(SessionError::invalid(
                    "idempotency key already used for different input",
                ));
            }
            return Ok(existing);
        }
        self.append(
            "inbox/accepted",
            None,
            command.turn_id.clone(),
            serde_json::to_value(&command)?,
        )?;
        Ok(command)
    }

    /// 标记某个回合在某个运行里开始执行。
    pub fn open(&self, turn_id: &str, run_id: &str) -> Result<(), SessionError> {
        self.append(
            "inbox/opened",
            Some(run_id),
            Some(turn_id.to_string()),
            serde_json::json!({}),
        )
    }

    /// 关闭回合。
    pub fn close(&self, turn_id: &str, run_id: &str) -> Result<(), SessionError> {
        self.append(
            "inbox/closed",
            Some(run_id),
            Some(turn_id.to_string()),
            serde_json::json!({}),
        )
    }

    /// 拒绝一条命令并记下原因。
    pub fn reject(&self, command: &Command, reason: &str) -> Result<(), SessionError> {
        self.append(
            "inbox/rejected",
            None,
            command.turn_id.clone(),
            serde_json::json!({ "commandId": command.id, "reason": reason }),
        )
    }

    /// 标记命令已消费。
    pub fn handled(&self, command: &Command) -> Result<(), SessionError> {
        self.append(
            "inbox/handled",
            None,
            command.turn_id.clone(),
            serde_json::json!({ "commandId": command.id }),
        )
    }

    fn append(
        &self,
        kind: &str,
        run_id: Option<&str>,
        turn_id: Option<String>,
        payload: serde_json::Value,
    ) -> Result<(), SessionError> {
        let seq = self.inbox.journal.head()?.seq;
        let json = serde_json::to_string(&payload)?;
        let event = SessionEvent::new(
            uuid::Uuid::new_v4().to_string(),
            seq + 1,
            super::now_millis(),
            kind,
            run_id.map(str::to_string),
            turn_id,
            true,
            json,
        )?;
        self.inbox.journal.commit(
            self.writer,
            &uuid::Uuid::new_v4().to_string(),
            seq,
            vec![event],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::store::{InMemorySessionLogStore, SessionLogStore};
    use crate::session::SessionKey;
    use std::sync::Arc;

    fn log() -> Arc<dyn SessionLog> {
        InMemorySessionLogStore::new()
            .open(&SessionKey::new(Some("u"), "a", "s").unwrap(), None)
            .unwrap()
    }

    #[test]
    fn accept_is_idempotent_and_conflicts_fail() {
        let inbox = SessionInbox::new(log().as_ref()).unwrap();
        let command = Command::new("cmd-1", "submit")
            .with_turn("turn-1")
            .with_messages(vec![Msg::user("user", "你好")]);

        let first = inbox.locked(|tx| tx.accept(command.clone())).unwrap();
        assert_eq!(first.id, "cmd-1");

        // 同 id 同内容：幂等，不追加事件。
        let second = inbox.locked(|tx| tx.accept(command.clone())).unwrap();
        assert_eq!(second, first);
        assert_eq!(inbox.snapshot().unwrap().commands.len(), 1);

        // 时间戳不参与判等。
        let mut restamped = command.clone();
        restamped.messages[0].timestamp = Some("2026-10-22T00:00:00Z".into());
        assert!(inbox.locked(|tx| tx.accept(restamped)).is_ok());
        assert_eq!(inbox.snapshot().unwrap().commands.len(), 1);

        // 同 id 不同内容：必须报错，不能静默覆盖。
        let conflicting = Command::new("cmd-1", "submit").with_messages(vec![Msg::user("user", "别的")]);
        let error = inbox.locked(|tx| tx.accept(conflicting)).unwrap_err();
        assert!(matches!(error, SessionError::Invalid(_)));
    }

    #[test]
    fn open_and_close_track_current_turn() {
        let inbox = SessionInbox::new(log().as_ref()).unwrap();
        inbox.locked(|tx| tx.open("turn-1", "run-1")).unwrap();
        let snapshot = inbox.snapshot().unwrap();
        assert_eq!(snapshot.turn_id.as_deref(), Some("turn-1"));
        assert_eq!(snapshot.run_id.as_deref(), Some("run-1"));

        // 别的运行来关闭：不生效（过期事件）。
        inbox.locked(|tx| tx.close("turn-1", "run-2")).unwrap();
        assert_eq!(inbox.snapshot().unwrap().turn_id.as_deref(), Some("turn-1"));

        inbox.locked(|tx| tx.close("turn-1", "run-1")).unwrap();
        let snapshot = inbox.snapshot().unwrap();
        assert!(snapshot.turn_id.is_none());
        assert!(snapshot.run_id.is_none());
    }

    #[test]
    fn rejected_and_handled_commands_leave_the_pending_set() {
        let inbox = SessionInbox::new(log().as_ref()).unwrap();
        let cancelled = Command::new("cmd-cancel", "cancel");
        let submit = Command::new("cmd-submit", "submit");
        inbox.locked(|tx| {
            tx.accept(cancelled.clone())?;
            tx.accept(submit.clone())
        })
        .unwrap();

        {
            let snapshot = inbox.snapshot().unwrap();
            let pending: Vec<&str> = snapshot.pending().map(|c| c.id.as_str()).collect();
            assert_eq!(pending, ["cmd-submit"], "cancel 不驱动执行");
        }

        inbox
            .locked(|tx| tx.reject(&cancelled, "nothing to cancel"))
            .unwrap();
        inbox.locked(|tx| tx.handled(&submit)).unwrap();

        let snapshot = inbox.snapshot().unwrap();
        assert_eq!(
            snapshot.rejected_reason("cmd-cancel"),
            Some("nothing to cancel")
        );
        assert!(snapshot.is_handled("cmd-submit"));
        assert_eq!(snapshot.pending().count(), 0);
        // 已消费的命令仍在列表里（审计用）。
        assert_eq!(snapshot.commands.len(), 2);
    }

    #[test]
    fn unknown_inbox_event_fails_closed() {
        let log = log();
        let inbox = SessionInbox::new(log.as_ref()).unwrap();
        let writer = inbox
            .journal
            .acquire("w", Duration::from_secs(60))
            .unwrap();
        let event = SessionEvent::new(
            "e1",
            1,
            super::super::now_millis(),
            "inbox/mystery",
            None,
            None,
            true,
            "{}".to_string(),
        )
        .unwrap();
        inbox
            .journal
            .commit(&writer, "b1", 0, vec![event])
            .unwrap();
        let error = inbox.snapshot().unwrap_err();
        assert!(matches!(error, SessionError::Log(_)));
    }

    #[test]
    fn inbox_lease_is_independent_from_execution_writer() {
        let log = log();
        // 执行方占住主日志写者。
        let execution = log.acquire("exec", Duration::from_secs(600)).unwrap();
        let inbox = SessionInbox::new(log.as_ref()).unwrap();
        // 收件箱仍可写入：这是「执行中接收输入」的前提。
        inbox
            .locked(|tx| tx.accept(Command::new("cmd-1", "submit")))
            .unwrap();
        assert_eq!(inbox.snapshot().unwrap().commands.len(), 1);
        log.release(&execution).unwrap();
    }
}
