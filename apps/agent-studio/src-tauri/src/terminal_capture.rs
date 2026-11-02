//! Bash prompt framing for the shared PTY. Silence is never a completion signal.
use tokio::sync::oneshot;

pub type Reply = oneshot::Sender<Result<(String, i32), String>>;
const LIMIT: usize = 16 * 1024;

/// Prepend the marker without discarding scalar or array-valued Bash prompt hooks.
/// Returning the original status also preserves `$?` for the existing first hook.
pub fn bash_prompt_setup(token: &str) -> String {
    // Tokens become shell identifiers as well as OSC payloads.
    assert!(!token.is_empty() && token.bytes().all(|b| b.is_ascii_alphanumeric()));
    let hook = format!("__ox_prompt_{token}");
    format!(" if [ -n \"$BASH_VERSION\" ]; then {hook}() {{ local status=$?; printf '\\033]777;{token};%s\\007' \"$status\"; return \"$status\"; }}; PROMPT_COMMAND=({hook} \"${{PROMPT_COMMAND[@]}}\"); fi\r")
}

pub struct Capture {
    prefix: Vec<u8>,
    pending: Vec<u8>,
    output: Vec<u8>,
    reply: Option<Reply>,
    pub ready: bool,
    truncated: bool,
}

impl Capture {
    pub fn new(token: &str) -> Self {
        Self { prefix: format!("\x1b]777;{token};").into_bytes(), pending: vec![], output: vec![], reply: None, ready: false, truncated: false }
    }

    pub fn begin(&mut self, reply: Reply) -> bool {
        if reply.is_closed() { return false; }
        if !self.ready || self.reply.is_some() {
            let _ = reply.send(Err("终端未就绪或仍有命令运行；请等待提示符。仅支持已安装完成钩子的 Bash。".into()));
            return false;
        }
        self.ready = false;
        self.output.clear();
        self.truncated = false;
        self.reply = Some(reply);
        true
    }

    pub fn cancel_if_abandoned(&mut self) -> bool {
        if self.reply.as_ref().is_some_and(|reply| reply.is_closed()) {
            self.fail("调用已取消");
            return true;
        }
        false
    }

    pub fn input(&mut self, data: &[u8]) -> bool {
        // During an Agent command only Ctrl-C may be sent, never interleave commands.
        if self.reply.is_some() && data != [3] { return false; }
        self.ready = false;
        true
    }

    fn collect(&mut self, bytes: &[u8]) {
        if self.reply.is_some() {
            let count = bytes.len().min(LIMIT.saturating_sub(self.output.len()));
            self.output.extend_from_slice(&bytes[..count]);
            self.truncated |= count < bytes.len();
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(bytes);
        let mut visible = Vec::new();
        loop {
            if let Some(start) = self.pending.windows(self.prefix.len()).position(|w| w == self.prefix) {
                let before: Vec<_> = self.pending.drain(..start).collect();
                self.collect(&before);
                visible.extend(before);
                let Some(end) = self.pending.iter().position(|b| *b == 7) else {
                    if self.pending.len() > self.prefix.len() + 16 {
                        let plain = std::mem::take(&mut self.pending);
                        self.collect(&plain);
                        visible.extend(plain);
                    }
                    break;
                };
                let code = std::str::from_utf8(&self.pending[self.prefix.len()..end]).ok().and_then(|s| s.parse::<i32>().ok());
                let marker: Vec<_> = self.pending.drain(..=end).collect();
                if let Some(code) = code {
                    self.ready = true;
                    if let Some(reply) = self.reply.take() {
                        let mut text = String::from_utf8_lossy(&self.output).into_owned();
                        if self.truncated { text.push_str("\n[输出已截断]"); }
                        let _ = reply.send(Ok((text, code)));
                    }
                } else {
                    self.collect(&marker);
                    visible.extend(marker);
                }
            } else {
                let keep = (1..self.prefix.len()).rev().find(|n| self.pending.ends_with(&self.prefix[..*n])).unwrap_or(0);
                let count = self.pending.len() - keep;
                let plain: Vec<_> = self.pending.drain(..count).collect();
                self.collect(&plain);
                visible.extend(plain);
                break;
            }
        }
        visible
    }

    pub fn fail(&mut self, reason: &str) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(Err(format!("{reason}\n已采集的部分输出：\n{}", String::from_utf8_lossy(&self.output))));
        }
        self.ready = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_setup_installs_hook_without_replacing_existing_entries() {
        let setup = bash_prompt_setup("test");
        assert!(setup.contains("PROMPT_COMMAND=(__ox_prompt_test \"${PROMPT_COMMAND[@]}\")"));
        assert!(setup.contains("local status=$?;"));
        assert!(setup.contains("return \"$status\";"));
        assert!(!setup.contains("-z \"$PROMPT_COMMAND\""));
        assert!(setup.ends_with('\r'));
    }

    // Remote shells are Bash; Windows development hosts need not have Bash installed.
    #[cfg(unix)]
    #[test]
    fn prompt_setup_preserves_existing_hooks_and_status() {
        let setup = bash_prompt_setup("test");
        for (existing, expected) in [
            ("unset PROMPT_COMMAND", ""),
            ("PROMPT_COMMAND='printf \"old:%s\" \"$?\"'", "old:7"),
            ("PROMPT_COMMAND=('printf \"old:%s\" \"$?\"' 'printf second')", "old:7second"),
        ] {
            // Join before setting the status so command substitution cannot change it.
            let script = format!("{existing}\n{}\nhooks=$(IFS=';'; echo \"${{PROMPT_COMMAND[*]}}\")\n(exit 7)\neval \"$hooks\"\nexit 0", setup.trim());
            let output = std::process::Command::new("bash")
                .args(["--noprofile", "--norc", "-c", &script])
                .output().expect("Bash is required for prompt hook regression tests");
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            assert_eq!(String::from_utf8_lossy(&output.stdout), format!("\x1b]777;test;7\x07{expected}"));
        }
    }

    #[tokio::test]
    async fn fragmented_prompt_returns_output_and_exit_code() {
        let mut c = Capture::new("test");
        c.feed(b"\x1b]777;test;0\x07");
        let (tx, rx) = oneshot::channel();
        assert!(c.begin(tx));
        assert!(!c.input(b"pwd\r"));
        assert_eq!(c.feed(b"hello\r\n\x1b]777;te"), b"hello\r\n");
        assert_eq!(c.feed(b"st;7\x07$ "), b"$ ");
        assert_eq!(rx.await.unwrap().unwrap(), ("hello\r\n".into(), 7));
    }
    #[tokio::test]
    async fn cancellation_and_truncation_are_explicit() {
        let mut c = Capture::new("test");
        c.feed(b"\x1b]777;test;0\x07");
        let (tx, rx) = oneshot::channel();
        drop(rx);
        assert!(!c.begin(tx));
        let (tx, rx) = oneshot::channel();
        assert!(c.begin(tx));
        drop(rx);
        assert!(c.cancel_if_abandoned());
        assert!(!c.ready);
        c.feed(b"\x1b]777;test;130\x07");
        let (tx, rx) = oneshot::channel();
        assert!(c.begin(tx));
        c.feed(&vec![b'a'; LIMIT + 100]);
        c.feed(b"\x1b]777;test;0\x07");
        assert!(rx.await.unwrap().unwrap().0.ends_with("[输出已截断]"));
    }

    #[tokio::test]
    async fn silence_is_not_success_and_failure_has_partial_output() {
        let mut c = Capture::new("test");
        let (tx, rx) = oneshot::channel();
        assert!(!c.begin(tx));
        assert!(rx.await.unwrap().is_err());
        c.feed(b"\x1b]777;test;0\x07");
        let (tx, mut rx) = oneshot::channel();
        assert!(c.begin(tx));
        c.feed(b"partial");
        assert!(rx.try_recv().is_err());
        c.fail("timeout");
        assert!(rx.await.unwrap().unwrap_err().contains("partial"));
        assert!(!c.ready);
    }
}
