//! SSH 远程运维（ops 工作区）：连接池 + `remote_shell` 工具 + SFTP 文件操作。
//!
//! - 连接：russh（纯 Rust，无系统依赖），密码认证（档案密码留空则尝试无密码）；
//! - `remote_shell`：agent 工具面——在指定服务器上执行命令并回传输出；
//!   注册进权限引擎 ask 规则，**每次执行都需要用户确认**（HITL）；
//! - SFTP（列目录/上传/下载/删除）：供前端服务器面板直接调用（用户手动
//!   操作，无需 agent 参与），与工具共享连接池。

use std::collections::HashMap;
use std::sync::Arc;

use agentscope_core::error::Result;
use agentscope_core::tool::{Tool, ToolOutput};
use russh::client::{self, Handle};
use russh::ChannelMsg;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::config::ServerConfig;

/// 单命令输出上限（防上下文打爆；超出截断并注明）。
const MAX_OUTPUT: usize = 16 * 1024;

/// russh 客户端 handler：接受任意主机密钥（运维工具面向用户自配的服务器，
/// 与 `ssh -o StrictHostKeyChecking=accept-new` 的首次连接语义一致）。
#[derive(Clone)]
struct ClientHandler;

#[async_trait::async_trait]
impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::key::PublicKey,
    ) -> std::result::Result<bool, Self::Error> {
        Ok(true)
    }
}

/// 一条已认证的 SSH 连接（按服务器名缓存；Handle 可并发开多个 channel）。
pub struct SshConn {
    handle: Handle<ClientHandler>,
}

impl SshConn {
    /// 建连 + 认证（密码；空密码直接尝试，失败报错提示补档案）。
    pub async fn connect(server: ServerConfig) -> std::result::Result<Self, String> {
        let config = Arc::new(client::Config::default());
        let addr = (server.host.as_str(), server.port);
        let mut handle = client::connect(config, addr, ClientHandler)
            .await
            .map_err(|e| format!("connect {}:{} failed: {e}", server.host, server.port))?;
        let ok = handle
            .authenticate_password(&server.username, &server.password)
            .await
            .map_err(|e| format!("auth {}@{} failed: {e}", server.username, server.host))?;
        if !ok {
            return Err(format!(
                "auth failed for {}@{}: 密码错误或服务器不允许密码登录",
                server.username, server.host
            ));
        }
        Ok(Self { handle })
    }

    /// 执行一条命令，收集 stdout/stderr 与退出码。
    pub async fn exec(&self, command: &str) -> std::result::Result<ExecOutput, String> {
        let mut channel = self
            .handle
            .channel_open_session()
            .await
            .map_err(|e| format!("open channel failed: {e}"))?;
        channel
            .exec(false, command)
            .await
            .map_err(|e| format!("exec failed: {e}"))?;

        let mut out = Vec::new();
        let mut code: i32 = 0;
        while let Some(msg) = channel.wait().await {
            match msg {
                ChannelMsg::Data { ref data } => append_truncated(&mut out, data),
                ChannelMsg::ExtendedData { ref data, .. } => append_truncated(&mut out, data),
                ChannelMsg::ExitStatus { exit_status } => code = exit_status as i32,
                ChannelMsg::Eof | ChannelMsg::Close => break,
                _ => {}
            }
        }
        Ok(ExecOutput {
            text: String::from_utf8_lossy(&out).into_owned(),
            exit_code: code,
        })
    }

    /// SFTP 会话（每次操作开一个 subsystem channel，用完即弃——简单可靠）。
    async fn sftp(
        &self,
    ) -> std::result::Result<russh_sftp::client::SftpSession, String> {
        let channel = self
            .handle
            .channel_open_session()
            .await
            .map_err(|e| format!("open sftp channel failed: {e}"))?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|e| format!("request sftp subsystem failed: {e}"))?;
        russh_sftp::client::SftpSession::new(channel.into_stream())
            .await
            .map_err(|e| format!("sftp init failed: {e}"))
    }

    /// 列目录（名称 + 类型 + 大小）。
    pub async fn list_dir(&self, path: &str) -> std::result::Result<Vec<DirEntry>, String> {
        let sftp = self.sftp().await?;
        let entries = sftp
            .read_dir(path)
            .await
            .map_err(|e| format!("read_dir {path} failed: {e}"))?;
        let mut out: Vec<DirEntry> = entries
            .into_iter()
            .map(|e| {
                let name = e.file_name();
                let meta = e.metadata();
                let is_dir = meta.is_dir();
                let size = meta.size.unwrap_or(0);
                DirEntry {
                    name,
                    is_dir,
                    size,
                }
            })
            .collect();
        out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));
        Ok(out)
    }

    /// 上传：本地文件 → 远程路径。
    pub async fn upload(
        &self,
        local_path: &std::path::Path,
        remote_path: &str,
    ) -> std::result::Result<u64, String> {
        let data = std::fs::read(local_path).map_err(|e| format!("read local file: {e}"))?;
        let sftp = self.sftp().await?;
        let mut file = sftp
            .create(remote_path)
            .await
            .map_err(|e| format!("create {remote_path} failed: {e}"))?;
        use tokio::io::AsyncWriteExt;
        file.write_all(&data)
            .await
            .map_err(|e| format!("write {remote_path} failed: {e}"))?;
        file.shutdown()
            .await
            .map_err(|e| format!("close {remote_path} failed: {e}"))?;
        Ok(data.len() as u64)
    }

    /// 下载：远程文件 → 本地路径。
    pub async fn download(
        &self,
        remote_path: &str,
        local_path: &std::path::Path,
    ) -> std::result::Result<u64, String> {
        let sftp = self.sftp().await?;
        let mut file = sftp
            .open(remote_path)
            .await
            .map_err(|e| format!("open {remote_path} failed: {e}"))?;
        let mut data = Vec::new();
        use tokio::io::AsyncReadExt;
        file.read_to_end(&mut data)
            .await
            .map_err(|e| format!("read {remote_path} failed: {e}"))?;
        std::fs::write(local_path, &data).map_err(|e| format!("write local file: {e}"))?;
        Ok(data.len() as u64)
    }

    /// 删除远程文件。
    pub async fn delete_file(&self, remote_path: &str) -> std::result::Result<(), String> {
        let sftp = self.sftp().await?;
        sftp.remove_file(remote_path)
            .await
            .map_err(|e| format!("remove {remote_path} failed: {e}"))
    }
}

/// 命令执行输出（文本 + 退出码）。
pub struct ExecOutput {
    pub text: String,
    pub exit_code: i32,
}

/// 命令执行结果（前端 sh 模式投影；serde 序列化）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ExecResult {
    pub exit_code: i32,
    pub output: String,
}

/// 目录项（前端面板渲染）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

/// 输出累积（超限截断，尾部注明）。
fn append_truncated(buf: &mut Vec<u8>, data: &[u8]) {
    if buf.len() >= MAX_OUTPUT {
        return;
    }
    let room = MAX_OUTPUT - buf.len();
    let take = data.len().min(room);
    buf.extend_from_slice(&data[..take]);
    if take < data.len() {
        buf.extend_from_slice(b"\n...(output truncated)");
    }
}

/// SSH 连接池（AppState 持有；按服务器名缓存已认证连接）。
#[derive(Default)]
pub struct SshPool {
    sessions: Mutex<HashMap<String, Arc<SshConn>>>,
}

impl SshPool {
    pub fn new() -> Self {
        Self::default()
    }

    /// 取已连接会话；未连接报错（前端先 connect）。
    pub async fn get(&self, server_name: &str) -> std::result::Result<Arc<SshConn>, String> {
        let map = self.sessions.lock().await;
        map.get(server_name)
            .cloned()
            .ok_or_else(|| format!("server `{server_name}` 未连接，请先在服务器面板连接"))
    }

    /// 建连并缓存（重复连接先断旧再建新）。
    pub async fn connect(
        &self,
        server: ServerConfig,
    ) -> std::result::Result<Arc<SshConn>, String> {
        let conn = Arc::new(SshConn::connect(server.clone()).await?);
        let mut map = self.sessions.lock().await;
        map.insert(server.name.clone(), conn.clone());
        Ok(conn)
    }

    /// 断开并移除。
    pub async fn disconnect(&self, server_name: &str) {
        let mut map = self.sessions.lock().await;
        if let Some(conn) = map.remove(server_name) {
            let _ = conn
                .handle
                .disconnect(russh::Disconnect::ByApplication, "user disconnect", "")
                .await;
        }
    }

    pub async fn is_connected(&self, server_name: &str) -> bool {
        self.sessions.lock().await.contains_key(server_name)
    }
}

/// `remote_shell` 工具：在指定远程服务器上执行一条命令（ask 规则 → 每次确认）。
pub struct RemoteShellTool {
    pool: Arc<SshPool>,
}

impl RemoteShellTool {
    pub fn new(pool: Arc<SshPool>) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl Tool for RemoteShellTool {
    fn name(&self) -> &str {
        "remote_shell"
    }

    fn description(&self) -> &str {
        "Execute a single shell command on a connected remote server over SSH and \
         return its output and exit code. The server must be connected from the \
         servers panel first. Every execution requires user confirmation."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "server": {"type": "string", "description": "Server name (from the servers panel)."},
                "command": {"type": "string", "description": "A single shell command to run."}
            },
            "required": ["server", "command"]
        })
    }

    async fn call(&self, input: Value) -> Result<ToolOutput> {
        let server = input
            .get("server")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let command = input
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if server.is_empty() || command.is_empty() {
            return Ok(ToolOutput::error("server and command are required"));
        }
        let conn = match self.pool.get(&server).await {
            Ok(c) => c,
            Err(e) => return Ok(ToolOutput::error(e)),
        };
        match conn.exec(&command).await {
            Ok(out) => Ok(ToolOutput::ok(
                json!({
                    "server": server,
                    "exit_code": out.exit_code,
                    "output": out.text,
                })
                .to_string(),
            )),
            Err(e) => Ok(ToolOutput::error(e)),
        }
    }
}
