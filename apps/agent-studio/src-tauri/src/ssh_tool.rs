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
    // 新版默认列表含旧 RSA；保持产品的显式 opt-in 约束。
    config.preferred.key.to_mut().retain(|key| *key != russh::keys::Algorithm::Rsa { hash: None });
    config.preferred.kex.to_mut().retain(|kex| *kex != russh::kex::DH_G1_SHA1 && *kex != russh::kex::DH_G14_SHA1 && *kex != russh::kex::DH_GEX_SHA1);
    if allow_legacy_rsa {
        let keys = config.preferred.key.to_mut();
        if !keys.contains(&russh::keys::Algorithm::Rsa { hash: None }) {
            // 现代算法优先，旧签名仅作为此连接的末位备选。
            keys.push(russh::keys::Algorithm::Rsa { hash: None });
        }
    }
    // OpenSSH 5.3 支持 GEX-SHA256；不为兼容旧主机降级到 SHA-1 KEX。
    if !config.preferred.kex.contains(&russh::kex::DH_GEX_SHA256) {
        config.preferred.kex.to_mut().push(russh::kex::DH_GEX_SHA256);
    }
    config
}

/// 现有 handler 接受任意主机密钥，尚无 known_hosts 校验。
/// 不等同于 accept-new：后者还会拒绝已知主机的密钥变化。
#[derive(Clone)]
struct ClientHandler;

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::PublicKeyOrCertificate,
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
        if !ok.success() {
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
    Command(String, crate::terminal_capture::Reply),
    Resize(u32, u32),
    Close,
}

/// SSH 连接池（AppState 持有；按服务器名缓存已认证连接）。
#[derive(Default)]
pub struct SshPool {
    sessions: Mutex<HashMap<String, Arc<SshConn>>>,
    terminals: Arc<Mutex<HashMap<String, tokio::sync::mpsc::Sender<TerminalInput>>>>,
    // (workspace, server, 创建序号)：序号用于多终端时选最新（页面刷新后
    // 旧 PTY 成孤儿，新开的终端序号更大、更接近用户当前上下文）。
    terminal_bindings: Arc<Mutex<HashMap<String, (String, String, u64)>>>,
    seq: std::sync::atomic::AtomicU64,
}

impl SshPool {
    pub fn new() -> Self {
        Self::default()
    }

    /// 独立 PTY 通道；输入和窗口变化通过有界队列送入同一个任务。
    pub async fn open_terminal(
        &self, workspace: &str, server: &str, cols: u32, rows: u32,
        output: tauri::ipc::Channel<TerminalEvent>,
    ) -> std::result::Result<String, String> {
        let conn = self.get(server).await?;
        let mut channel = conn.handle.channel_open_session().await.map_err(|e| e.to_string())?;
        channel.request_pty(true, "xterm-256color", cols.clamp(1, 500), rows.clamp(1, 300), 0, 0, &[])
            .await.map_err(|e| e.to_string())?;
        channel.request_shell(true).await.map_err(|e| e.to_string())?;
        let id = uuid::Uuid::new_v4().to_string();
        // 同工作区同服务器的旧终端先关（页面刷新后旧 PTY 无人认领成孤儿，
        // 每刷新一次泄一个；不变式：每工作区每服务器至多一个活跃 PTY，
        // 由后端保证）。旧任务收到 Close 自行退出并清理绑定。
        let stale: Vec<String> = self.terminal_bindings.lock().await.iter()
            .filter(|(_, (ws, name, _))| ws == workspace && name == server)
            .map(|(bound_id, _)| bound_id.clone())
            .collect();
        for bound_id in &stale {
            if let Some(tx) = self.terminals.lock().await.get(bound_id) {
                let _ = tx.try_send(TerminalInput::Close);
            }
        }
        let seq = self.seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, mut rx) = tokio::sync::mpsc::channel(128);
        self.terminals.lock().await.insert(id.clone(), tx);
        self.terminal_bindings.lock().await.insert(id.clone(), (workspace.to_string(), server.to_string(), seq));
        let bindings = self.terminal_bindings.clone();
        let terminals = self.terminals.clone();
        let task_id = id.clone();
        tokio::spawn(async move {
            let token = uuid::Uuid::new_v4().simple().to_string();
            let mut capture = crate::terminal_capture::Capture::new(&token);
            // Install a completion marker while preserving any existing Bash
            // PROMPT_COMMAND hooks. The OSC marker is consumed before display.
            let setup = crate::terminal_capture::bash_prompt_setup(&token);
            let setup_ok = channel.data(setup.as_bytes()).await.is_ok();
            let mut cancel_tick = tokio::time::interval(std::time::Duration::from_millis(100));
            let mut deadline: Option<tokio::time::Instant> = None;
            loop {
                if !setup_ok { break; }
                tokio::select! {
                    _ = cancel_tick.tick() => {
                        if capture.cancel_if_abandoned() {
                            deadline = None;
                            // Best-effort foreground interruption, not a kill guarantee.
                            if channel.data(&[3][..]).await.is_err() { break; }
                        }
                    }
                    input = rx.recv() => {
                        let result = match input {
                            Some(TerminalInput::Command(command, reply)) => {
                                if !capture.begin(reply) { continue; }
                                deadline = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(120));
                                let mut bytes = command.into_bytes();
                                bytes.push(b'\r');
                                channel.data(&bytes[..]).await
                            }
                            Some(TerminalInput::Data(data)) => {
                                if !capture.input(&data) { continue; }
                                channel.data(&data[..]).await
                            },
                            Some(TerminalInput::Resize(cols, rows)) => channel.window_change(cols.clamp(1, 500), rows.clamp(1, 300), 0, 0).await,
                            Some(TerminalInput::Close) | None => break,
                        };
                        if result.is_err() { break; }
                    }
                    _ = async {
                        match deadline {
                            Some(at) => tokio::time::sleep_until(at).await,
                            None => std::future::pending::<()>().await,
                        }
                    } => {
                        capture.fail("等待命令完成超时；远端可能仍在执行，请检查终端，未自动杀进程");
                        deadline = None;
                    }
                    msg = channel.wait() => {
                        match msg {
                            Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
                                let visible = capture.feed(&data);
                                if capture.ready { deadline = None; }
                                if !visible.is_empty() && output.send(TerminalEvent { data: visible, closed: false }).is_err() { break; }
                            }
                            Some(ChannelMsg::Close) | Some(ChannelMsg::Eof) | None => break,
                            _ => {}
                        }
                    }
                }
            }
            capture.fail("终端连接已关闭，命令结果未知");
            let _ = channel.close().await;
            terminals.lock().await.remove(&task_id);
            bindings.lock().await.remove(&task_id);
            let _ = output.send(TerminalEvent { data: vec![], closed: true });
        });
        Ok(id)
    }

    pub async fn terminal_input(&self, id: &str, input: TerminalInput) -> std::result::Result<(), String> {
        let tx = self.terminals.lock().await.get(id).cloned().ok_or("终端已关闭")?;
        tx.send(input).await.map_err(|_| "终端已关闭".into())
    }

    async fn send_command(&self, workspace: &str, server: &str, command: &str) -> std::result::Result<(String, i32), String> {
        // 终端绑定选创建序号最大的（最新窗口；孤儿/旧窗口已被 open_terminal
        // 关闭，此处兜底处理瞬态并存）。没有可用终端时回退独立 exec 通道：
        // 干净环境（cwd=home、不共享终端 cd 状态），但保证 Agent 不因终端
        // 状态卡死——此前 0/多终端一律报错，页面刷新即触发，实测卡死。
        let bound: Vec<(String, u64)> = self.terminal_bindings.lock().await.iter()
            .filter(|(_, (ws, name, _))| ws == workspace && name == server)
            .map(|(id, (_, _, seq))| (id.clone(), *seq))
            .collect();
        // A literal newline could produce several prompts; do not mistake the first
        // for completion of the entire request. Interactive input belongs in SH.
        if command.contains(['\r', '\n', '\0', '\x1b', '\x03']) {
            return Err("Agent 命令须为单行且不含终端控制字符；多行/交互输入请切换 SH".into());
        }
        if let Some((id, _)) = bound.iter().max_by_key(|(_, seq)| *seq) {
            let (tx, rx) = tokio::sync::oneshot::channel();
            match self.terminal_input(id, TerminalInput::Command(command.to_string(), tx)).await {
                Ok(()) => {
                    // 命令已送入终端：结果未知时不回退 exec（可能已在远端
                    // 执行，重发会重复执行命令）。
                    return rx.await.map_err(|_| "终端执行任务已关闭，结果未知".to_string())?;
                }
                Err(_) => {} // 绑定已死（终端刚关闭）→ 落到 exec 回退
            }
        }
        let conn = self.get(server).await?;
        let out = conn.exec(command).await?;
        Ok((out.text, out.exit_code))
    }

    /// 取已连接会话；未连接报错（前端先 connect）。
    pub async fn get(&self, server_name: &str) -> std::result::Result<Arc<SshConn>, String> {
        let map = self.sessions.lock().await;
        map.get(server_name)
            .cloned()
            .ok_or_else(|| format!("server `{server_name}` 未连接，请先在服务器面板连接"))
    }

    /// 建连并缓存；已存在存活连接时直接复用（幂等）。此前重复 connect
    /// 会覆盖旧连接，导致开在旧连接上的 PTY 静默死亡、终端绑定被清——
    /// 前端却仍显示「已连接」，Agent 执行时报「必须恰好有一个终端」。
    pub async fn connect(
        &self,
        server: ServerConfig,
    ) -> std::result::Result<Arc<SshConn>, String> {
        {
            let map = self.sessions.lock().await;
            if let Some(existing) = map.get(&server.name) {
                if !existing.handle.is_closed() {
                    return Ok(existing.clone());
                }
            }
        }
        let conn = Arc::new(SshConn::connect(server.clone()).await?);
        let mut map = self.sessions.lock().await;
        // 双检：并发 connect 时后完成者复用先完成者，避免覆盖存活连接。
        if let Some(existing) = map.get(&server.name) {
            if !existing.handle.is_closed() {
                return Ok(existing.clone());
            }
        }
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
        let modern_keys: Vec<_> = defaults.preferred.key.iter().filter(|key| **key != russh::keys::Algorithm::Rsa { hash: None }).cloned().collect();
        assert_eq!(normal.preferred.key.as_ref(), modern_keys.as_slice());
        let legacy = ssh_client_config(true);
        assert!(legacy.preferred.key.contains(&russh::keys::Algorithm::Rsa { hash: None }));
        assert!(legacy.preferred.key.starts_with(&modern_keys));
        assert_eq!(legacy.preferred.kex, normal.preferred.kex);
        assert!(legacy.preferred.kex.contains(&russh::kex::DH_GEX_SHA256));
        assert!(!normal.preferred.key.contains(&russh::keys::Algorithm::Rsa { hash: None }));
        assert!(!legacy.preferred.kex.contains(&russh::kex::DH_G1_SHA1));
        assert!(!legacy.preferred.kex.contains(&russh::kex::DH_G14_SHA1));
        assert_eq!(legacy.preferred.cipher, defaults.preferred.cipher);
        assert_eq!(legacy.preferred.mac, defaults.preferred.mac);
        assert_eq!(ssh_client_config(false).preferred.key, normal.preferred.key);
    }

    #[tokio::test]
    async fn command_uses_bound_terminal_unchanged() {
        let pool = SshPool::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        pool.terminals.lock().await.insert("pty".into(), tx.clone());
        pool.terminal_bindings.lock().await.insert("pty".into(), ("ws".into(), "srv".into(), 0));
        // 其他工作区的绑定不匹配 → 无可用终端 → 回退独立 exec → 未连接报错。
        assert!(pool.send_command("other", "srv", "pwd").await.is_err());
        let execute = pool.send_command("ws", "srv", "cd /tmp");
        let receive = async {
            match rx.recv().await.unwrap() {
                TerminalInput::Command(command, reply) => {
                    assert_eq!(command, "cd /tmp");
                    reply.send(Ok(("result".into(), 0))).unwrap();
                }
                _ => panic!("expected terminal command"),
            }
        };
        let (result, _) = tokio::join!(execute, receive);
        assert_eq!(result.unwrap(), ("result".into(), 0));
        assert!(pool.send_command("ws", "srv", "pwd\nls").await.is_err());
        // 重复窗口（如页面刷新后孤儿未及清理）：选创建序号最大的最新终端，
        // 不再报错卡死 Agent。
        pool.terminals.lock().await.insert("duplicate".into(), tx);
        pool.terminal_bindings.lock().await.insert("duplicate".into(), ("ws".into(), "srv".into(), 1));
        let execute = pool.send_command("ws", "srv", "pwd");
        let receive = async {
            match rx.recv().await.unwrap() {
                TerminalInput::Command(command, reply) => {
                    assert_eq!(command, "pwd");
                    reply.send(Ok(("latest".into(), 0))).unwrap();
                }
                _ => panic!("expected terminal command"),
            }
        };
        let (result, _) = tokio::join!(execute, receive);
        assert_eq!(result.unwrap(), ("latest".into(), 0));
    }

    #[tokio::test]
    async fn session_binding_supplies_omitted_server_and_rejects_mismatch() {
        // ops 会话已绑定服务器：模型省略 server 时按绑定补齐（否则模型会因
        // 不知道服务器名而反问用户、不发工具调用）。
        let dir = std::env::temp_dir().join(format!("ox-binding-{}", uuid::Uuid::new_v4().simple()));
        let session_dir = dir.join("s1");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join("binding.json"), json!({"server":"platform"}).to_string()).unwrap();
        let tool = RemoteShellTool::new(Arc::new(SshPool::new()), None)
            .with_terminal_workspace(Some("ws".into()))
            .with_session_dir(dir.clone());
        let ctx = agentscope_core::tool::ToolContext {
            session_id: "s1".into(),
            ..agentscope_core::tool::ToolContext::new("call-1", "remote_shell")
        };
        let input = json!({"command":"free -m"});
        // 未连接服务器 → 报错信息里的 server 名应为绑定补齐后的 platform
        // （证明省略 server 时确实按会话绑定解析，而非空名）。
        let resolved = tool
            .call_with_context(input.clone(), &ctx)
            .await
            .expect("工具以 Ok(ToolOutput::error) 形式返回失败");
        let text = resolved.content;
        assert!(text.contains("platform"), "server 应由绑定补齐: {text}");
        assert!(text.contains("未连接"), "{text}");

        // 显式给出与绑定不符的服务器 → 拒绝。
        let mismatch = tool
            .call_with_context(json!({"server":"other","command":"free -m"}), &ctx)
            .await
            .expect("工具以 Ok(ToolOutput::error) 形式返回失败");
        assert!(
            mismatch.content.contains("仅可操作服务器 `platform`"),
            "应提示当前会话仅可操作 platform: {}",
            mismatch.content
        );

        // 没有绑定文件 → 不补齐，报空名/未连接。
        let ctx2 = agentscope_core::tool::ToolContext {
            session_id: "s2".into(),
            ..agentscope_core::tool::ToolContext::new("call-2", "remote_shell")
        };
        let unbound = tool
            .call_with_context(json!({"command":"free -m"}), &ctx2)
            .await
            .expect("工具以 Ok(ToolOutput::error) 形式返回失败");
        assert!(unbound.content.contains("required"), "{}", unbound.content);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn binding_host_mismatch_rejects_execution() {
        // 绑定记录的 host 与当前档案不一致（删服务器后新增同名但指向别机）
        // → 拒绝执行，防旧会话静默连到新机器。
        let dir = std::env::temp_dir().join(format!("ox-host-{}", uuid::Uuid::new_v4().simple()));
        let session_dir = dir.join("s1");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join("binding.json"),
            json!({"server":"platform","server_host":"10.0.0.1"}).to_string(),
        )
        .unwrap();
        let ctx = agentscope_core::tool::ToolContext {
            session_id: "s1".into(),
            ..agentscope_core::tool::ToolContext::new("call-1", "remote_shell")
        };
        let mk_config = |host: &str| {
            Arc::new(std::sync::Mutex::new(crate::config::StudioConfig {
                servers: vec![serde_json::from_value(json!({"name":"platform","host":host})).unwrap()],
                ..Default::default()
            }))
        };
        let mk_tool = |config: Arc<std::sync::Mutex<crate::config::StudioConfig>>| {
            RemoteShellTool::new(Arc::new(SshPool::new()), None)
                .with_terminal_workspace(Some("ws".into()))
                .with_session_dir(dir.clone())
                .with_config(config)
        };

        // host 不一致 → 拒绝并提示重新确认绑定。
        let rejected = mk_tool(mk_config("10.0.0.2"))
            .call_with_context(json!({"command":"free -m"}), &ctx)
            .await
            .expect("工具以 Ok(ToolOutput::error) 形式返回失败");
        assert!(rejected.content.contains("地址已变更"), "{}", rejected.content);

        // host 一致 → 通过校验，走到「未连接」错误（证明没被 host 校验拦住）。
        let passed = mk_tool(mk_config("10.0.0.1"))
            .call_with_context(json!({"command":"free -m"}), &ctx)
            .await
            .expect("工具以 Ok(ToolOutput::error) 形式返回失败");
        assert!(passed.content.contains("未连接"), "{}", passed.content);

        // 档案已删除 → 拒绝。
        let gone = mk_tool(Arc::new(std::sync::Mutex::new(crate::config::StudioConfig::default())))
            .call_with_context(json!({"command":"free -m"}), &ctx)
            .await
            .expect("工具以 Ok(ToolOutput::error) 形式返回失败");
        assert!(gone.content.contains("档案已不存在"), "{}", gone.content);

        // 旧数据（无 server_host 字段）→ 跳过校验，行为与之前一致。
        std::fs::write(session_dir.join("binding.json"), json!({"server":"platform"}).to_string()).unwrap();
        let legacy = mk_tool(mk_config("10.0.0.2"))
            .call_with_context(json!({"command":"free -m"}), &ctx)
            .await
            .expect("工具以 Ok(ToolOutput::error) 形式返回失败");
        assert!(legacy.content.contains("未连接"), "{}", legacy.content);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ops_tool_schema_makes_server_optional() {
        let ops = RemoteShellTool::new(Arc::new(SshPool::new()), None).with_terminal_workspace(Some("ws".into()));
        let params = ops.parameters();
        assert_eq!(params["required"], json!(["command"]), "ops 模式 server 应可选");
        assert!(ops.description().contains("OPTIONAL"));
        let plain = RemoteShellTool::new(Arc::new(SshPool::new()), None);
        assert_eq!(plain.parameters()["required"], json!(["server","command"]), "非 ops 仍要求 server");
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
    terminal_workspace: Option<String>,
    /// 内存配置副本：校验会话绑定记录的 host 与当前档案 host 一致
    /// （防「删服务器后新增同名但指向别机」被旧会话静默沿用）。
    config: Option<Arc<std::sync::Mutex<crate::config::StudioConfig>>>,
}

impl RemoteShellTool {
    pub fn new(pool: Arc<SshPool>, allowed_servers: Option<Vec<String>>) -> Self {
        Self {
            pool,
            allowed_servers,
            workspace_state_dir: None,
            terminal_workspace: None,
            config: None,
        }
    }

    pub fn with_terminal_workspace(mut self, workspace: Option<String>) -> Self {
        self.terminal_workspace = workspace;
        self
    }

    pub fn with_session_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.workspace_state_dir = Some(dir);
        self
    }

    pub fn with_config(
        mut self,
        config: Arc<std::sync::Mutex<crate::config::StudioConfig>>,
    ) -> Self {
        self.config = Some(config);
        self
    }

    /// 绑定 host 一致性校验：binding.json 记录了绑定时档案 host 且配置
    /// 可达时，当前档案 host 不一致（或档案已删除）即拒绝执行。
    /// 旧会话数据无 server_host 字段时跳过（重新绑定后即受保护）。
    fn binding_host_matches(&self, server: &str, recorded_host: &str) -> std::result::Result<(), String> {
        let Some(config) = &self.config else {
            return Ok(());
        };
        let guard = config.lock().unwrap_or_else(|e| e.into_inner());
        match guard.servers.iter().find(|s| s.name == server) {
            Some(current) if current.host == recorded_host => Ok(()),
            Some(current) => Err(format!(
                "服务器 `{server}` 的地址已变更（绑定时 {recorded_host}，当前 {}），请重新确认绑定后再执行",
                current.host
            )),
            None => Err(format!(
                "服务器 `{server}` 的档案已不存在（可能已删除），请重新绑定后再执行"
            )),
        }
    }
}

#[async_trait::async_trait]
impl Tool for RemoteShellTool {
    fn name(&self) -> &str {
        "remote_shell"
    }

    fn description(&self) -> &str {
        if self.terminal_workspace.is_some() {
            return "Execute an approved single-line command unchanged in the existing workspace Bash terminal. The `server` argument is OPTIONAL: omit it to use the server bound to the current session (shown in the servers panel). Do not ask the user which server to use - the session already has one bound. Requires user confirmation. Returns captured terminal output (including command echo) and shell exit code when the prompt hook fires. Uses current shell directory and environment. MANDATORY: commands must be non-interactive, bounded, produce a finite result, and terminate by themselves. Never use continuous monitoring/following commands (tail -f/-F, journalctl -f, watch), interactive programs, infinite loops, background jobs, or unbounded streams. Use finite snapshots instead: tail -n 100 FILE, journalctl -n 100 --no-pager, top -b -n 1, vmstat 1 5, ping -c 4 HOST. Specify count/time limits where applicable; if bounded execution cannot be ensured, do not execute and ask the user. Do not change PROMPT_COMMAND. Timeout/disconnection means unknown completion, not success; inspect partial output and ask the user before retrying.";
        }
        "Execute a single shell command on a connected remote server over SSH and \
         return its output and exit code. The server must be connected from the \
         servers panel first. Every execution requires user confirmation. Commands must be \
         non-interactive, bounded, return finite output and terminate by themselves. Never use \
         tail -f/-F, journalctl -f, watch, infinite loops, background jobs or unbounded streams. \
         Use finite snapshots and explicit count/time limits instead. Each exec has independent \
         shell state: do not assume a previous command's cwd or environment persists. Use explicit \
         paths or combine dependent operations in one approved command. If bounded execution \
         cannot be ensured, do not execute and ask the user."
    }

    fn parameters(&self) -> Value {
        // ops（共享 PTY）会话已绑定唯一服务器：server 省略即可，避免模型因
        // 不知道服务器名而反问用户、从而不发工具调用（实测：必填时模型只回
        // 计划性文字；可选时立即发出命令）。非 ops 仍按显式服务器名调用。
        if self.terminal_workspace.is_some() {
            return json!({
                "type": "object",
                "properties": {
                    "server": {"type": "string", "description": "Optional. Omit to use the server bound to the current session."},
                    "command": {"type": "string", "description": "A single shell command to run."}
                },
                "required": ["command"]
            });
        }
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
        let mut input = input;
        // 会话绑定是服务器名的真相：模型省略 server 时按绑定补齐（ops 会话
        // 已绑定唯一服务器）；显式给了且与绑定不符则拒绝，防止跨服务器误操作。
        if let Some(dir) = &self.workspace_state_dir {
            let path = dir.join(&ctx.session_id).join("binding.json");
            match std::fs::read(path) {
                Ok(bytes) => {
                    let binding: Value = match serde_json::from_slice(&bytes) {
                        Ok(value) => value,
                        Err(_) => return Ok(ToolOutput::error("无法读取服务器绑定，拒绝执行")),
                    };
                    if let Some(bound) = binding.get("server").and_then(Value::as_str) {
                        let bound = bound.trim();
                        if !bound.is_empty() {
                            let requested = input.get("server").and_then(Value::as_str).map(str::trim).unwrap_or("");
                            if requested.is_empty() && self.terminal_workspace.is_some() {
                                if let Some(map) = input.as_object_mut() {
                                    map.insert("server".to_string(), Value::String(bound.to_string()));
                                }
                            } else if requested != bound {
                                return Ok(ToolOutput::error(format!("当前会话仅可操作服务器 `{bound}`")));
                            }
                            // 绑定 host 一致性：档案地址变更（同名不同机）或已删除时拒绝。
                            if let Some(recorded) = binding.get("server_host").and_then(Value::as_str) {
                                if let Err(e) = self.binding_host_matches(bound, recorded) {
                                    return Ok(ToolOutput::error(e));
                                }
                            }
                        }
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
        if let Some(workspace) = &self.terminal_workspace {
            return match self.pool.send_command(workspace, &server, &command).await {
                Ok((output, exit_code)) => Ok(ToolOutput::ok(json!({ "server": server, "exit_code": exit_code, "output": output }).to_string())),
                Err(e) => Ok(ToolOutput::error(e)),
            };
        }
        match conn.exec(&command).await {
            Ok(out) => Ok(ToolOutput::ok(json!({ "server": server, "exit_code": out.exit_code, "output": out.text }).to_string())),
            Err(e) => Ok(ToolOutput::error(e)),
        }
    }
}
