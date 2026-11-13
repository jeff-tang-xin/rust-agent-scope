//! protocol / rpc / serve 层的单元与集成测试（内存 duplex，不经 stdio）。

use std::time::Duration;

use agentscope_acp::rpc::RpcConn;
use agentscope_acp::{protocol, serve, set_ox_tool_provider, OxHostContext};
use agentscope_core::message::Source;
use agentscope_core::ContentBlock;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::duplex;

/// 读一行 JSON 帧（带超时，防测试挂死）。
/// 注意：BufReader 必须跨调用复用（调用方持有），否则预读缓冲丢失导致超时。
async fn read_frame<R>(reader: &mut tokio::io::BufReader<R>) -> Value
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;
    let mut buf = String::new();
    let n = tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut buf))
        .await
        .expect("读帧超时")
        .expect("行读取失败");
    assert!(n > 0, "对端流已关闭");
    serde_json::from_str(buf.trim()).expect("帧不是合法 JSON")
}

#[test]
fn initialize_declares_v1_with_load_session() {
    let result = protocol::initialize_result();
    assert_eq!(result["protocolVersion"], 1);
    assert_eq!(result["agentCapabilities"]["loadSession"], true);
    assert_eq!(result["agentInfo"]["name"], "agentscope");
}

#[test]
fn tool_kind_covers_major_families() {
    assert_eq!(protocol::tool_kind("read_file"), "read");
    assert_eq!(protocol::tool_kind("write_file"), "edit");
    assert_eq!(protocol::tool_kind("shell_execute"), "execute");
    assert_eq!(protocol::tool_kind("web_fetch"), "fetch");
    assert_eq!(protocol::tool_kind("mystery"), "other");
}

#[test]
fn parse_prompt_concatenates_text_blocks() {
    let params = json!({
        "sessionId": "s1",
        "prompt": [
            { "type": "text", "text": "第一段" },
            { "type": "image", "data": "...", "mimeType": "image/png" },
            { "type": "text", "text": "第二段" },
        ],
    });
    let (session_id, content) = protocol::parse_prompt(&params).unwrap();
    assert_eq!(session_id, "s1");
    // 文本在前（拼接），图片块按出现顺序保留。
    assert_eq!(content.len(), 2);
    assert_eq!(content[0], ContentBlock::Text { text: "第一段\n第二段".into() });
    match &content[1] {
        ContentBlock::Image { source: Source::Base64 { media_type, data }, .. } => {
            assert_eq!(media_type, "image/png");
            assert_eq!(data, "...");
        }
        other => panic!("expected image block, got {other:?}"),
    }
}

#[test]
fn parse_prompt_accepts_image_only() {
    // 只贴图不写字：合法（多模态模型直接看图）。
    let params = json!({
        "sessionId": "s1",
        "prompt": [{ "type": "image", "data": "aGk=" }],
    });
    let (_, content) = protocol::parse_prompt(&params).unwrap();
    assert_eq!(content.len(), 1);
    assert!(matches!(&content[0], ContentBlock::Image { .. }));
}

#[test]
fn parse_prompt_rejects_missing_fields() {
    assert!(protocol::parse_prompt(&json!({ "prompt": [] })).is_err());
    assert!(protocol::parse_prompt(&json!({ "sessionId": "s" })).is_err());
    // image 块缺 data：明确报错（优于静默丢图）。
    let empty = json!({ "sessionId": "s", "prompt": [{ "type": "image" }] });
    assert!(protocol::parse_prompt(&empty).is_err());
}

#[test]
fn parse_new_session_requires_cwd() {
    assert_eq!(
        protocol::parse_new_session(&json!({ "cwd": "/tmp/x" })).unwrap(),
        "/tmp/x"
    );
    assert!(protocol::parse_new_session(&json!({})).is_err());
    assert!(protocol::parse_new_session(&json!({ "cwd": "  " })).is_err());
}

/// session/new 走 assemble_agent 全路径：宿主注入记忆锚点时，记忆管线
/// （memory 工具 + flush/maintenance 中间件）装配成功，会话正常建立。
/// 注意：set_ox_tool_provider 是进程级 OnceLock——本测试注册的上下文会被
/// 同进程其他走 assemble_agent 的测试（mode/compact 端到端）读到，因此
/// 收尾【不能】删除 root：并行测试的 agent 可能仍持有 memory_anchor 下的
/// state store，删除会让它们的保存路径 IO 失败（表现为偶发错误帧）。
/// 目录留给系统 temp 清理。
#[tokio::test]
async fn session_new_assembles_memory_pipeline_from_host_context() {
    let root = std::env::temp_dir().join(format!("ox-acp-mem-{}", std::process::id()));
    let cwd = root.join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    let memory_root = root.join("memory-anchor");
    set_ox_tool_provider(Arc::new(move |_| OxHostContext {
        tools: Vec::new(),
        memory_root: Some(memory_root.clone()),
        project_brief: None,
    }));

    let (client, server) = duplex(64 * 1024);
    let (client_read, mut client_write) = tokio::io::split(client);
    let mut client_reader = tokio::io::BufReader::new(client_read);
    let (server_read, server_write) = tokio::io::split(server);
    let env = vec![("AGENTSCOPE_MODEL".to_owned(), "test-model".to_owned())];
    let server_task = tokio::spawn(serve(server_read, server_write, env));

    client_write
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
        .await
        .unwrap();
    let frame = read_frame(&mut client_reader).await;
    assert_eq!(frame["id"], 1);

    let params = json!({ "cwd": cwd.display().to_string() });
    client_write
        .write_all(
            format!("{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/new\",\"params\":{params}}}\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let frame = read_frame(&mut client_reader).await;
    assert_eq!(frame["id"], 2);
    assert!(frame["result"]["sessionId"].is_string(), "session/new failed: {frame}");
    // `/` 面板的 skill 声明：宿主专属内置（oxidea-usage）+ 框架内置
    // （spec-workflow）随 session/new 声明（磁盘无 .claude/skills 也有内置可调）。
    let listed = frame["result"]["availableCommands"].as_array().expect("availableCommands 缺失");
    let names: Vec<&str> = listed.iter().filter_map(|c| c["name"].as_str()).collect();
    assert!(names.contains(&"oxidea-usage"), "内置 oxidea-usage 应声明: {names:?}");
    assert!(names.contains(&"spec-workflow"), "框架内置 spec-workflow 应声明: {names:?}");
    assert!(
        listed.iter().all(|c| c["source"] == "skill"),
        "skill 声明的 source 应为 skill: {listed:?}"
    );

    server_task.abort();
    // 收尾【不】删除 root（与文件头注释一致）：并行测试（mode/compact 端到端）
    // 可能仍持有 memory_anchor 下的 state store，删除会让它们的保存路径 IO 失败。目录留给系统 temp 清理。
}

#[tokio::test]
async fn serve_answers_initialize_and_rejects_unknown_method() {    let (client, server) = duplex(64 * 1024);
    let (client_read, mut client_write) = tokio::io::split(client);
    let mut client_reader = tokio::io::BufReader::new(client_read);
    // serve 从 server 读半边收帧，响应写回 server 写半边（duplex 回程 → client 可读）。
    let (server_read, server_write) = tokio::io::split(server);
    let server_task = tokio::spawn(serve(server_read, server_write, Vec::new()));

    // initialize
    client_write
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
        .await
        .unwrap();
    let frame = read_frame(&mut client_reader).await;
    assert_eq!(frame["id"], 1);
    assert_eq!(frame["result"]["protocolVersion"], 1);

    // 未知方法 → method not found
    client_write
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"nope\",\"params\":{}}\n")
        .await
        .unwrap();
    let frame = read_frame(&mut client_reader).await;
    assert_eq!(frame["id"], 2);
    assert_eq!(frame["error"]["code"], -32601);

    // session/load 缺参数 → invalid params（方法已支持，参数缺失报 -32602）
    client_write
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"session/load\",\"params\":{}}\n")
        .await
        .unwrap();
    let frame = read_frame(&mut client_reader).await;
    assert_eq!(frame["error"]["code"], -32602);

    // 非法 JSON 行被丢弃，不影响后续请求
    client_write.write_all(b"not-json\n").await.unwrap();
    client_write
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"initialize\",\"params\":{}}\n")
        .await
        .unwrap();
    let frame = read_frame(&mut client_reader).await;
    assert_eq!(frame["id"], 4);

    server_task.abort();
}

#[tokio::test]
async fn rpc_conn_pairs_outbound_responses_by_id() {
    let (client, server) = duplex(64 * 1024);
    let (client_read, mut client_write) = tokio::io::split(client);
    let mut client_reader = tokio::io::BufReader::new(client_read);
    let (server_read, server_write) = tokio::io::split(server);
    let conn = Arc::new(RpcConn::new(server_write));

    // server 端读循环：消费 client 回写的响应帧，配对给等待者。
    let reader_conn = Arc::clone(&conn);
    tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut reader = tokio::io::BufReader::new(server_read);
        let mut buf = String::new();
        loop {
            buf.clear();
            if reader.read_line(&mut buf).await.unwrap_or(0) == 0 {
                break;
            }
            let frame: Value = match serde_json::from_str(buf.trim()) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if frame.get("id").is_some() && frame.get("method").is_none() {
                reader_conn
                    .resolve_pending(frame["id"].clone(), frame)
                    .await;
            }
        }
    });

    // 发出站请求（后台等待）
    let request_task = tokio::spawn({
        let conn = Arc::clone(&conn);
        async move {
            conn.request("session/request_permission", json!({ "q": 1 }))
                .await
        }
    });
    // 给写端一点时间落帧
    tokio::time::sleep(Duration::from_millis(50)).await;

    let frame = read_frame(&mut client_reader).await;
    assert_eq!(frame["method"], "session/request_permission");
    let id = frame["id"].as_i64().unwrap();

    // 回错误帧 → request 返回 Err
    client_write
        .write_all(
            format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"error\":{{\"code\":-1,\"message\":\"denied\"}}}}\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), request_task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result, Err((-1, "denied".to_string())));
}

#[tokio::test]
async fn rpc_conn_delivers_notifications_in_order() {
    let (client, server) = duplex(64 * 1024);
    let (client_read, _client_write) = tokio::io::split(client);
    let mut client_reader = tokio::io::BufReader::new(client_read);
    let conn = std::sync::Arc::new(RpcConn::new(server));

    conn.notify("session/update", json!({ "n": 1 })).await;
    conn.notify("session/update", json!({ "n": 2 })).await;

    let first = read_frame(&mut client_reader).await;
    let second = read_frame(&mut client_reader).await;
    assert_eq!(first["params"]["n"], 1);
    assert_eq!(second["params"]["n"], 2);
}

/// `/compact` prompt 端到端：不进模型，直接压缩并回 compaction_update 帧。
/// 空对话场景（session/new 后立即 /compact）——不依赖真实模型可用性。
#[tokio::test]
async fn compact_prompt_emits_compaction_updates_and_ends_turn() {
    let root = std::env::temp_dir().join(format!("ox-acp-compact-{}", std::process::id()));
    let cwd = root.join("proj");
    std::fs::create_dir_all(&cwd).unwrap();

    let (client, server) = duplex(64 * 1024);
    let (client_read, mut client_write) = tokio::io::split(client);
    let mut client_reader = tokio::io::BufReader::new(client_read);
    let (server_read, server_write) = tokio::io::split(server);
    let env = vec![("AGENTSCOPE_MODEL".to_owned(), "test-model".to_owned())];
    let server_task = tokio::spawn(serve(server_read, server_write, env));

    // initialize + session/new
    client_write
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
        .await
        .unwrap();
    let _ = read_frame(&mut client_reader).await;
    let params = json!({ "cwd": cwd.display().to_string() });
    client_write
        .write_all(
            format!("{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/new\",\"params\":{params}}}\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let frame = read_frame(&mut client_reader).await;
    let session_id = frame["result"]["sessionId"]
        .as_str()
        .expect("session/new failed")
        .to_owned();

    // /compact prompt（空对话 → completed「对话为空」）
    let prompt = json!({
        "sessionId": session_id,
        "prompt": [{ "type": "text", "text": "/compact" }],
    });
    client_write
        .write_all(
            format!("{{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"session/prompt\",\"params\":{prompt}}}\n")
                .as_bytes(),
        )
        .await
        .unwrap();

    // 收集 compaction_update 通知，直到 prompt 响应到达
    let mut updates = Vec::new();
    let mut prompt_result = None;
    for _ in 0..10 {
        let frame = read_frame(&mut client_reader).await;
        if frame.get("method").and_then(|m| m.as_str()) == Some("session/update") {
            updates.push(frame["params"].clone());
        } else if frame.get("id").is_some() {
            prompt_result = Some(frame);
            break;
        }
    }
    assert_eq!(updates.len(), 2, "expected in_progress + completed, got {updates:?}");
    assert_eq!(updates[0]["update"]["sessionUpdate"], "compaction_update");
    assert_eq!(updates[0]["update"]["status"], "in_progress");
    assert_eq!(updates[1]["update"]["status"], "completed");
    // 同一 compactionId（Start/End 配对）
    assert_eq!(
        updates[0]["update"]["compactionId"],
        updates[1]["update"]["compactionId"]
    );
    // 空对话：completed 带说明文本（v2 content 包裹形态）
    assert_eq!(
        updates[1]["update"]["summary"][0]["content"]["text"],
        "对话为空，无需压缩"
    );
    // prompt 正常结束（命令不进模型，无 LLM 依赖）
    let result = prompt_result.expect("prompt response missing");
    assert_eq!(result["id"], 3);
    assert_eq!(result["result"]["stopReason"], "end_turn");

    server_task.abort();
    // 不删 root：进程级 provider 的 memory_anchor 被同进程并行测试共享（见上注释）。
}

/// mode configOption 声明与切换往返：session/new 带 Code/Plan 下拉；
/// set_config_option 切 plan → config_option_update 通知 + 响应 currentValue=plan；
/// 未知配置项/值报 invalid params。
#[tokio::test]
async fn mode_config_option_declared_and_switchable() {
    let root = std::env::temp_dir().join(format!("ox-acp-mode-{}", std::process::id()));
    let cwd = root.join("proj");
    std::fs::create_dir_all(&cwd).unwrap();

    let (client, server) = duplex(64 * 1024);
    let (client_read, mut client_write) = tokio::io::split(client);
    let mut client_reader = tokio::io::BufReader::new(client_read);
    let (server_read, server_write) = tokio::io::split(server);
    let env = vec![("AGENTSCOPE_MODEL".to_owned(), "test-model".to_owned())];
    let server_task = tokio::spawn(serve(server_read, server_write, env));

    // initialize + session/new：响应声明 mode configOption（默认 code）
    client_write
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
        .await
        .unwrap();
    let _ = read_frame(&mut client_reader).await;
    let params = json!({ "cwd": cwd.display().to_string() });
    client_write
        .write_all(
            format!("{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/new\",\"params\":{params}}}\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let frame = read_frame(&mut client_reader).await;
    let session_id = frame["result"]["sessionId"].as_str().unwrap().to_owned();
    let options = &frame["result"]["configOptions"];
    assert_eq!(options[0]["id"], "mode");
    assert_eq!(options[0]["category"], "mode");
    assert_eq!(options[0]["currentValue"], "code");
    let values: Vec<&str> = options[0]["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["value"].as_str().unwrap())
        .collect();
    assert_eq!(values, ["code", "plan"]);

    // 切到 plan：先收到 config_option_update 通知，再收到响应（currentValue=plan）
    let set = json!({ "sessionId": session_id, "configId": "mode", "value": "plan", "type": "id" });
    client_write
        .write_all(
            format!("{{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"session/set_config_option\",\"params\":{set}}}\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut update_seen = None;
    let response = loop {
        let frame = read_frame(&mut client_reader).await;
        if frame.get("method").and_then(|m| m.as_str()) == Some("session/update") {
            if frame["params"]["update"]["sessionUpdate"] == "config_option_update" {
                update_seen = Some(frame["params"]["update"]["configOptions"].clone());
            }
        } else {
            break frame;
        }
    };
    assert_eq!(response["id"], 3, "实际响应: {response}");
    assert_eq!(
        response["result"]["configOptions"][0]["currentValue"],
        "plan",
        "实际响应: {response}"
    );
    let update = update_seen.expect("config_option_update notification missing");
    assert_eq!(update[0]["currentValue"], "plan");

    // 未知配置项 → invalid params
    let bad = json!({ "sessionId": session_id, "configId": "model", "value": "m1" });
    client_write
        .write_all(
            format!("{{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"session/set_config_option\",\"params\":{bad}}}\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let frame = read_frame(&mut client_reader).await;
    assert_eq!(frame["error"]["code"], -32602);

    server_task.abort();
    let _ = std::fs::remove_dir_all(&root);
}

use tokio::io::AsyncWriteExt;
