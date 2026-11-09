//! protocol / rpc / serve 层的单元与集成测试（内存 duplex，不经 stdio）。

use std::time::Duration;

use agentscope_acp::rpc::RpcConn;
use agentscope_acp::{protocol, serve};
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
fn initialize_declares_v1_without_load_session() {
    let result = protocol::initialize_result();
    assert_eq!(result["protocolVersion"], 1);
    assert_eq!(result["agentCapabilities"]["loadSession"], false);
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
            { "type": "image", "data": "..." },
            { "type": "text", "text": "第二段" },
        ],
    });
    let (session_id, text) = protocol::parse_prompt(&params).unwrap();
    assert_eq!(session_id, "s1");
    assert_eq!(text, "第一段\n第二段");
}

#[test]
fn parse_prompt_rejects_missing_fields() {
    assert!(protocol::parse_prompt(&json!({ "prompt": [] })).is_err());
    assert!(protocol::parse_prompt(&json!({ "sessionId": "s" })).is_err());
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

#[tokio::test]
async fn serve_answers_initialize_and_rejects_unknown_method() {
    let (client, server) = duplex(64 * 1024);
    let (client_read, mut client_write) = tokio::io::split(client);
    let mut client_reader = tokio::io::BufReader::new(client_read);
    // serve 从 server 读半边收帧，响应写回 server 写半边（duplex 回程 → client 可读）。
    let (server_read, server_write) = tokio::io::split(server);
    let server_task = tokio::spawn(serve(server_read, server_write));

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

    // session/load 未声明 → 报错
    client_write
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"session/load\",\"params\":{}}\n")
        .await
        .unwrap();
    let frame = read_frame(&mut client_reader).await;
    assert_eq!(frame["error"]["code"], -32601);

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

use tokio::io::AsyncWriteExt;
