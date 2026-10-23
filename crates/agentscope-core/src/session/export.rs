//! 导出层（对应 Java `SessionExportSink` / `SessionLogExporter` / `SessionTranscriptExport`）。
//!
//! 两条通道职责不同：
//!
//! - **事件导出**（[`SessionLogExporter`]）：持久化 outbox 游标，把已提交事件逐条
//!   交给外部 sink。丢 ACK 只会重投同一条不可变事件，不会重执行——游标推进与
//!   sink 受理必须一一对应。
//! - **转录导出**（[`transcript_entries`]）：读时把消息列表拆成扁平条目，给仍在
//!   消费「一行一条对话」的旧客户端。纯函数，从不写日志。

use std::sync::Arc;

use serde_json::Value;

use super::events::SessionEvent;
use super::journal::SessionLog;
use super::SessionError;
use crate::message::{ContentBlock, Msg};

/// 已确认提交事件的导出口（对应 Java `SessionExportSink`）。
///
/// 实现必须按原生 `event_id` 去重：游标推进与受理之间崩溃时，同一条事件会被重投。
pub trait SessionExportSink: Send + Sync {
    /// sink 的稳定标识（游标按它存取）。
    fn name(&self) -> &str;

    /// 受理一条事件。失败时游标不推进，下次 `drain` 会重投。
    fn accept(&self, event: &SessionEvent) -> Result<(), SessionError>;
}

/// 持久化 outbox 游标（对应 Java `SessionLogExporter`）。
///
/// `drain` 把游标之后的已提交事件逐条交给 sink：每条 ACK 后立即推进游标，
/// 因此重复 `drain` 是安全的（最多重投最后一条未 ACK 的事件）。
pub struct SessionLogExporter {
    log: Arc<dyn SessionLog>,
    sink: Arc<dyn SessionExportSink>,
}

impl SessionLogExporter {
    /// 绑定日志与导出口。
    pub fn new(log: Arc<dyn SessionLog>, sink: Arc<dyn SessionExportSink>) -> Self {
        Self { log, sink }
    }

    /// 把游标之后的事件逐条交给 sink，返回推进到的序号（无新事件时等于当前游标）。
    pub fn drain(&self) -> Result<i64, SessionError> {
        let mut cursor = self.log.export_cursor(self.sink.name())?;
        let head = self.log.head()?.seq;
        if head > cursor {
            for event in self.log.scan(cursor, head)? {
                self.sink.accept(&event)?;
                self.log
                    .advance_export_cursor(self.sink.name(), cursor, event.seq)?;
                cursor = event.seq;
            }
        }
        Ok(cursor)
    }
}

/// 扁平转录条目（对应 Java `SessionTranscriptExport.Entry`）。
///
/// 一条消息可能拆出多条条目：正文一条，每个工具调用/结果块各一条（id 加 `:序号` 后缀）。
#[derive(Debug, Clone)]
pub struct TranscriptEntry {
    pub id: String,
    pub role: String,
    pub content: Option<String>,
    pub timestamp: Option<String>,
    pub tool_name: Option<String>,
    pub tool_input: Option<Value>,
    pub tool_result: Option<String>,
}

/// 把消息列表拆成扁平转录条目（读时导出，从不写日志）。
///
/// 与 Java 一致的拆分规则：正文非空**或**不含工具块的消息出一条正文条目；
/// 工具块无论正文是否为空都各自出条目（`Msg::text()` 只取顶层 Text 块，
/// 工具结果正文必须从块里取——见已知坑）。
pub fn transcript_entries(messages: &[Msg]) -> Vec<TranscriptEntry> {
    let mut entries = Vec::new();
    for message in messages {
        let text = message.text();
        let has_tools = message.content.iter().any(|block| {
            matches!(
                block,
                ContentBlock::ToolUse { .. } | ContentBlock::ToolResult { .. }
            )
        });
        if !text.trim().is_empty() || !has_tools {
            entries.push(TranscriptEntry {
                id: message.id.clone(),
                role: format!("{:?}", message.role).to_uppercase(),
                content: (!text.is_empty()).then_some(text),
                timestamp: message.timestamp.clone(),
                tool_name: None,
                tool_input: None,
                tool_result: None,
            });
        }
        for (index, block) in message.content.iter().enumerate() {
            let id = format!("{}:{}", message.id, index);
            match block {
                ContentBlock::ToolUse { name, input, .. } => {
                    entries.push(TranscriptEntry {
                        id,
                        role: "TOOL".into(),
                        content: None,
                        timestamp: message.timestamp.clone(),
                        tool_name: Some(name.clone()),
                        tool_input: Some(input.clone()),
                        tool_result: None,
                    });
                }
                ContentBlock::ToolResult { name, output, .. } => {
                    let output = output
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    entries.push(TranscriptEntry {
                        id,
                        role: "TOOL".into(),
                        content: None,
                        timestamp: message.timestamp.clone(),
                        tool_name: Some(name.clone()),
                        tool_input: None,
                        tool_result: (!output.is_empty()).then_some(output),
                    });
                }
                _ => {}
            }
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ToolResultState;
    use crate::session::{SessionKey, SessionLogStore};
    use crate::session::store::InMemorySessionLogStore;
    use serde_json::json;
    use std::sync::Mutex;

    fn log() -> Arc<dyn SessionLog> {
        InMemorySessionLogStore::new()
            .open(&SessionKey::new(Some("u"), "a", "s").unwrap(), None)
            .unwrap()
    }

    struct CollectingSink {
        name: &'static str,
        seen: Mutex<Vec<String>>,
        fail_first: std::sync::atomic::AtomicBool,
    }

    impl CollectingSink {
        fn new(name: &'static str) -> Arc<Self> {
            Arc::new(Self {
                name,
                seen: Mutex::new(Vec::new()),
                fail_first: std::sync::atomic::AtomicBool::new(false),
            })
        }

        fn seen(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl SessionExportSink for CollectingSink {
        fn name(&self) -> &str {
            self.name
        }

        fn accept(&self, event: &SessionEvent) -> Result<(), SessionError> {
            // 模拟 ACK 丢失：第一次受理报错，游标不推进，下次 drain 重投同一条
            if self
                .fail_first
                .compare_exchange(
                    true,
                    false,
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                )
                .is_ok()
            {
                return Err(SessionError::log("sink temporarily unavailable"));
            }
            self.seen.lock().unwrap().push(event.event_id.clone());
            Ok(())
        }
    }

    fn event(seq: i64, kind: &str) -> SessionEvent {
        SessionEvent::new(format!("e{seq}"), seq, seq, kind, None, None, false, "{}").unwrap()
    }

    #[test]
    fn drain_advances_cursor_and_survives_lost_ack() {
        let log = log();
        let writer = log.acquire("w1", std::time::Duration::from_secs(60)).unwrap();
        log.commit(
            &writer,
            "b1",
            0,
            vec![event(1, "run/start"), event(2, "run/end")],
        )
        .unwrap();
        log.release(&writer).unwrap();

        let sink = CollectingSink::new("audit");
        sink.fail_first
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let exporter = SessionLogExporter::new(log.clone(), sink.clone());

        // 第一次 drain：第一条受理失败 → 错误传播，游标不动
        let error = exporter.drain().unwrap_err();
        assert!(error.to_string().contains("temporarily"), "{error}");
        assert_eq!(log.export_cursor("audit").unwrap(), 0);
        assert!(sink.seen().is_empty());

        // 第二次 drain：两条都受理，游标推进到链头
        assert_eq!(exporter.drain().unwrap(), 2);
        assert_eq!(sink.seen(), vec!["e1".to_string(), "e2".to_string()]);

        // 重复 drain：无新事件，不重投
        assert_eq!(exporter.drain().unwrap(), 2);
        assert_eq!(sink.seen().len(), 2);
    }

    #[test]
    fn transcript_splits_tool_blocks_into_own_entries() {
        let use_block = ContentBlock::ToolUse {
            id: "call1".into(),
            name: "search".into(),
            input: json!({"q": "rust"}),
            state: None,
            content: None,
        };
        let result_block = ContentBlock::ToolResult {
            id: "call1".into(),
            name: "search".into(),
            output: vec![ContentBlock::Text {
                text: "找到 3 条".into(),
            }],
            state: ToolResultState::Success,
            execution_details: None,
        };
        let assistant = Msg::new(
            crate::message::Role::Assistant,
            "agent",
            vec![
                ContentBlock::Text {
                    text: "我来查一下".into(),
                },
                use_block,
            ],
        );
        let tool = Msg::new(
            crate::message::Role::Tool,
            "",
            vec![result_block],
        );
        let entries = transcript_entries(&[assistant, tool]);

        // assistant：正文一条 + 工具调用一条；tool：工具结果一条
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].role, "ASSISTANT");
        assert_eq!(entries[0].content.as_deref(), Some("我来查一下"));
        assert_eq!(entries[1].role, "TOOL");
        assert_eq!(entries[1].tool_name.as_deref(), Some("search"));
        assert_eq!(entries[1].tool_input.as_ref(), Some(&json!({"q": "rust"})));
        assert!(entries[1].id.ends_with(":1"));
        assert_eq!(entries[2].tool_result.as_deref(), Some("找到 3 条"));
    }

    #[test]
    fn empty_text_message_without_tools_still_gets_an_entry() {
        let message = Msg::new(crate::message::Role::User, "user", vec![]);
        let entries = transcript_entries(&[message]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].role, "USER");
        assert!(entries[0].content.is_none());
    }
}
