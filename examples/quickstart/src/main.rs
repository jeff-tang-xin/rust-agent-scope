//! Hello AgentScope (Rust 版) —— 对应 Java 版 Quickstart 的最小示例。
//!
//! 运行前设置环境变量：
//! - `OPENAI_API_KEY`  ：API 密钥（必填）
//! - `OPENAI_BASE_URL` ：服务地址（可选，默认 <https://api.openai.com/v1>；
//!   DashScope 兼容模式可设为 <https://dashscope.aliyuncs.com/compatible-mode/v1>）
//! - `OPENAI_MODEL`    ：模型名（可选，默认 gpt-4o-mini）
//!
//! 运行：`cargo run -p quickstart`

use std::io::Write;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use agentscope_core::error::Result;
use agentscope_core::event::AgentEventKind;
use agentscope_core::message::Msg;
use agentscope_core::tool::{Tool, ToolOutput, Toolkit};
use agentscope_core::{Agent, JsonFileAgentStateStore, ReActAgent};
use agentscope_model_openai::OpenAIChatModel;

/// 示例工具：获取当前 Unix 时间戳（秒）。
struct CurrentTimeTool;

#[async_trait]
impl Tool for CurrentTimeTool {
    fn name(&self) -> &str {
        "get_current_time"
    }

    fn description(&self) -> &str {
        "获取当前时间，返回 Unix 时间戳（秒）"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn call(&self, _input: Value) -> Result<ToolOutput> {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Ok(ToolOutput::ok(format!("当前 Unix 时间戳：{secs} 秒")))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let api_key = std::env::var("OPENAI_API_KEY").unwrap_or_default();
    if api_key.is_empty() {
        eprintln!("请先设置环境变量 OPENAI_API_KEY（可选：OPENAI_BASE_URL、OPENAI_MODEL）");
        std::process::exit(1);
    }

    let base_url =
        std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".into());
    let model_name = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into());

    // 1. 模型提供商（OpenAI 兼容协议）
    let model = OpenAIChatModel::new(api_key, model_name).with_base_url(base_url);

    // 2. 工具集
    let mut toolkit = Toolkit::new();
    toolkit.register(Arc::new(CurrentTimeTool));

    // 3. 状态存储：对话真身在 AgentState，落盘后重跑进程历史仍在
    //    （对应 Java 版 `stateStore(...)` + `defaultSessionId(...)`）
    let state_store = JsonFileAgentStateStore::new(".quickstart-state")?;

    // 4. 组装 ReAct agent
    let agent = ReActAgent::builder()
        .name("assistant")
        .sys_prompt("你是一个乐于助人的助手，回答请使用中文。需要时可以调用工具。")
        .model(Arc::new(model))
        .toolkit(toolkit)
        .state_store(Arc::new(state_store))
        .default_session_id("quickstart")
        .build()?;

    // 5. 流式执行：token 级增量打印，同时展示 ReAct 的工具调用过程
    //    （对应 Java 版 agent.stream(...) / TextBlockDeltaEvent）
    let question = "现在几点了？请告诉我时间戳并换算成可读时间。";
    println!("user: {question}\n");

    let mut events = agent.stream_events(Msg::user("user", question)).await?;
    while let Some(event) = events.recv().await {
        match event.kind {
            AgentEventKind::TextBlockDelta { delta, .. } => {
                print!("{delta}");
                let _ = std::io::stdout().flush();
            }
            AgentEventKind::ToolCallStart {
                tool_call_name, ..
            } => {
                println!("\n[tool call] {tool_call_name}");
            }
            AgentEventKind::ToolResultEnd {
                tool_call_name,
                state,
                ..
            } => {
                println!("[tool result] {tool_call_name} -> {state:?}");
            }
            AgentEventKind::AgentResult { result } => {
                println!("\n\nassistant(final): {}", result.text());
            }
            _ => {}
        }
    }

    // 6. 读取对话真相（AgentState.context）：这里应能看到本次与历次运行的完整历史
    if let Some(state) = agent.state()? {
        println!(
            "\n[session] session_id={} 历史消息 {} 条（已落盘 .quickstart-state）",
            state.session_id,
            state.context.len()
        );
    }
    Ok(())
}
