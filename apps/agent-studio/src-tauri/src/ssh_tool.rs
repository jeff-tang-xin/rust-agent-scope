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

fn ssh_client_config(allow_legacy_rsa: bool) -> client::Config {
    let mut config = client::Config::default();
    if allow_legacy_rsa {
        let keys = config.preferred.key.to_mut();
        if !keys.contains(&russh::keys::key::SSH_RSA) {
            // 现代算法优先，旧签名仅作为此连接的末位备选。
            keys.push(russh::keys::key::SSH_RSA);
        }
    }
    config
}

/// 现有 handler 接受任意主机密钥，尚无 known_hosts 校验。
/// 不等同于 accept-new：后者还会拒绝已知主机的密钥变化。
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
        let config = Arc::new(ssh_client_config(server.allow_legacy_rsa));
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

#[derive(Clone, serde::Serialize)]
pub struct TerminalEvent {
    pub data: Vec<u8>,
    pub closed: bool,
}

pub enum TerminalInput {
    Data(Vec<u8>),
    Resize(u32, u32),
    Close,
}

/// SSH 连接池（AppState 持有；按服务器名缓存已认证连接）。
#[derive(Default)]
pub struct SshPool {
    sessions: Mutex<HashMap<String, Arc<SshConn>>>,
    terminals: Arc<Mutex<HashMap<String, tokio::sync::mpsc::Sender<TerminalInput>>>>,
}

impl SshPool {
    pub fn new() -> Self {
        Self::default()
    }

    /// 独立 PTY 通道；输入和窗口变化通过有界队列送入同一个任务。
    pub async fn open_terminal(
        &self, server: &str, cols: u32, rows: u32,
        output: tauri::ipc::Channel<TerminalEvent>,
    ) -> std::result::Result<String, String> {
        let conn = self.get(server).await?;
        let mut channel = conn.handle.channel_open_session().await.map_err(|e| e.to_string())?;
        channel.request_pty(true, "xterm-256color", cols.clamp(1, 500), rows.clamp(1, 300), 0, 0, &[])
            .await.map_err(|e| e.to_string())?;
        channel.request_shell(true).await.map_err(|e| e.to_string())?;
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, mut rx) = tokio::sync::mpsc::channel(128);
        self.terminals.lock().await.insert(id.clone(), tx);
        let terminals = self.terminals.clone();
        let task_id = id.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    input = rx.recv() => {
                        let result = match input {
                            Some(TerminalInput::Data(data)) => channel.data(&data[..]).await,
                            Some(TerminalInput::Resize(cols, rows)) => channel.window_change(cols.clamp(1, 500), rows.clamp(1, 300), 0, 0).await,
                            Some(TerminalInput::Close) | None => break,
                        };
                        if result.is_err() { break; }
                    }
                    msg = channel.wait() => {
                        match msg {
                            Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
                                if output.send(TerminalEvent { data: data.to_vec(), closed: false }).is_err() { break; }
                            }
                            Some(ChannelMsg::Close) | Some(ChannelMsg::Eof) | None => break,
                            _ => {}
                        }
                    }
                }
            }
            let _ = channel.close().await;
            terminals.lock().await.remove(&task_id);
            let _ = output.send(TerminalEvent { data: vec![], closed: true });
        });
        Ok(id)
    }

    pub async fn terminal_input(&self, id: &str, input: TerminalInput) -> std::result::Result<(), String> {
        let tx = self.terminals.lock().await.get(id).cloned().ok_or("终端已关闭")?;
        tx.send(input).await.map_err(|_| "终端已关闭".into())
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
        self.sessions.lock().await.get(server_name).is_some_and(|conn| !conn.handle.is_closed())
    }
}

#[cfg(test)]
mod terminal_tests {
    use super::*;

    #[test]
    fn legacy_rsa_is_opt_in_and_preserves_modern_algorithms() {
        let defaults = client::Config::default();
        let normal = ssh_client_config(false);
        assert_eq!(normal.preferred.key, defaults.preferred.key);
        let legacy = ssh_client_config(true);
        assert!(legacy.preferred.key.contains(&russh::keys::key::SSH_RSA));
        assert!(legacy.preferred.key.starts_with(&defaults.preferred.key));
        assert_eq!(legacy.preferred.kex, defaults.preferred.kex);
        assert_eq!(legacy.preferred.cipher, defaults.preferred.cipher);
        assert_eq!(legacy.preferred.mac, defaults.preferred.mac);
        assert_eq!(ssh_client_config(false).preferred.key, defaults.preferred.key);
    }

    #[test]
    fn legacy_server_config_defaults_off_and_roundtrips() {
        let mut server: ServerConfig = serde_json::from_value(json!({"name":"test", "host":"localhost"})).unwrap();
        assert!(!server.allow_legacy_rsa);
        server.allow_legacy_rsa = true;
        let decoded: ServerConfig = serde_json::from_value(serde_json::to_value(server).unwrap()).unwrap();
        assert!(decoded.allow_legacy_rsa);
    }

    #[tokio::test]
    async fn missing_terminal_rejects_input() {
        let pool = SshPool::new();
        assert!(pool.terminal_input("missing", TerminalInput::Data(vec![3])).await.is_err());
        assert!(!pool.is_connected("missing").await);
    }

    #[test]
    fn terminal_event_preserves_binary_utf8_chunks() {
        let event = TerminalEvent { data: vec![0x1b, 0x5b, 0xe4, 0xb8], closed: false };
        let value = serde_json::to_value(event).unwrap();
        assert_eq!(value["data"], json!([27, 91, 228, 184]));
        assert_eq!(value["closed"], false);
    }
}

/// `remote_shell` 工具：在指定远程服务器上执行一条命令（ask 规则 → 每次确认）。
pub struct RemoteShellTool {
    pool: Arc<SshPool>,
    /// 允许操作的服务器档案名白名单；`None` = 不限（工作区未绑定列表时
    /// 按类别默认开放）。`Some` 时调用参数 `server` 不在名单内即拒绝。
    allowed_servers: Option<Vec<String>>,
    workspace_state_dir: Option<std::path::PathBuf>,
}

impl RemoteShellTool {
    pub fn new(pool: Arc<SshPool>, allowed_servers: Option<Vec<String>>) -> Self {
        Self {
            pool,
            allowed_servers,
            workspace_state_dir: None,
        }
    }

    pub fn with_session_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.workspace_state_dir = Some(dir);
        self
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

    async fn call_with_context(&self, input: Value, ctx: &agentscope_core::tool::ToolContext) -> Result<ToolOutput> {
        if let Some(dir) = &self.workspace_state_dir {
            let path = dir.join(&ctx.session_id).join("binding.json");
            match std::fs::read(path) {
                Ok(bytes) => {
                    let binding: Value = match serde_json::from_slice(&bytes) {
                        Ok(value) => value,
                        Err(_) => return Ok(ToolOutput::error("无法读取服务器绑定，拒绝执行")),
                    };
                    if let Some(bound) = binding.get("server").and_then(Value::as_str)
                        && !bound.is_empty() && input.get("server").and_then(Value::as_str) != Some(bound) {
                        return Ok(ToolOutput::error(format!("当前会话仅可操作服务器 `{bound}`")));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Ok(ToolOutput::error("无法读取服务器绑定，拒绝执行")),
            }
        }
        self.call(input).await
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
        if let Some(allowed) = &self.allowed_servers
            && !allowed.iter().any(|name| name == &server) {
                return Ok(ToolOutput::error(format!(
                    "server `{server}` is not bound to this workspace (bound: {allowed:?})"
                )));
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
