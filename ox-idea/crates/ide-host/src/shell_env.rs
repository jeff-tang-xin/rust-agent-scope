//! GUI apps launched from Finder/Dock get a minimal PATH. Language servers,
//! git, package managers and run configurations need the user's real one, so
//! on macOS/Linux we take PATH from the login shell at startup.
//!
//! Spawning an interactive login shell costs ~1 s with typical rc files, so the
//! desktop app caches the value: a warm start uses the cached PATH immediately
//! and refreshes the cache in the background for the next launch.
//!
//! The rest of the shell's environment (API keys, provider switches, SDK homes)
//! goes to child processes through `ide_core::process::shell_env`. It stays in
//! memory, never in the cache: it may hold secrets.
//!
//! The remote server reads the whole environment afresh on every start
//! ([`inherit_login_shell_env`]), and Reload Environment reads it again while
//! running ([`read_login_env_now`], applied by [`crate::environment`]).

#[cfg(unix)]
use std::ffi::{OsStr, OsString};

#[cfg(unix)]
/// Returns a refresh to run once the environment is final (it spawns a
/// thread, and `set_var` must not race with other threads).
pub fn inherit_login_shell_path(identifier: &str) -> Option<impl FnOnce() + use<>> {
    use std::time::Duration;

    if std::env::var_os("OXIDEA_SKIP_SHELL_ENV").is_some() {
        return None;
    }
    record_startup_env();
    let cache = path_cache(identifier);
    let cached = cache.as_ref().and_then(|c| std::fs::read_to_string(c).ok()).filter(|p| !p.trim().is_empty());
    let refresh = cached.is_some();
    let path = match cached {
        Some(path) => path,
        None => {
            // Slow shell rc files must not delay the first start indefinitely.
            let env = read_login_env(&login_shell(), Duration::from_secs(3))?;
            let fresh = login_path(&env)?;
            ide_core::process::set_shell_env(env);
            if let Some(cache) = &cache {
                write_cache(cache, &fresh);
            }
            fresh
        }
    };
    // SAFETY: called at startup before any other thread reads the environment.
    unsafe { std::env::set_var("PATH", path.trim()) };
    let cache = cache.filter(|_| refresh)?;
    Some(move || {
        std::thread::spawn(move || {
            let Some(env) = read_login_env(&login_shell(), Duration::from_secs(10)) else { return };
            if let Some(fresh) = login_path(&env) {
                write_cache(&cache, &fresh);
            }
            ide_core::process::set_shell_env(env);
        });
    })
}

/// This process's environment before it took anything from the login shell: what a
/// shell read later starts from.
#[cfg(unix)]
static STARTUP_ENV: std::sync::OnceLock<Vec<(OsString, OsString)>> = std::sync::OnceLock::new();

#[cfg(unix)]
fn record_startup_env() {
    let _ = STARTUP_ENV.set(std::env::vars_os().collect());
}

/// How long the server waits for the login shell before it falls back to the cached PATH.
#[cfg(unix)]
const SERVER_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The remote server's start: this process takes the login shell's whole environment,
/// read now (an SSH exec channel gets a minimal one and no rc files), so every child
/// inherits it and a changed rc file applies on the next start. Falls back to the PATH
/// cached by the last start when the shell can't be read in time.
///
/// Call at startup before any other thread reads the environment.
#[cfg(unix)]
pub fn inherit_login_shell_env(identifier: &str) {
    if std::env::var_os("OXIDEA_SKIP_SHELL_ENV").is_some() {
        return;
    }
    record_startup_env();
    let cache = path_cache(identifier);
    match read_login_env(&login_shell(), SERVER_READ_TIMEOUT) {
        Some(env) => {
            for (k, v) in &env {
                // SAFETY: documented to run at startup before any other thread reads the environment.
                unsafe { std::env::set_var(k, v) };
            }
            if let (Some(cache), Some(path)) = (&cache, login_path(&env)) {
                write_cache(cache, &path);
            }
            ide_core::process::set_shell_env(env);
        }
        None => {
            let cached = cache.and_then(|c| std::fs::read_to_string(c).ok()).filter(|p| !p.trim().is_empty());
            if let Some(path) = cached {
                // SAFETY: as above.
                unsafe { std::env::set_var("PATH", path.trim()) };
            }
        }
    }
}

/// The login shell's environment, read now (Reload Environment). `Ok(None)` where the
/// IDE doesn't read it (Windows, `OXIDEA_SKIP_SHELL_ENV`).
#[cfg(unix)]
pub fn read_login_env_now() -> Result<Option<Vec<(OsString, OsString)>>, String> {
    if std::env::var_os("OXIDEA_SKIP_SHELL_ENV").is_some() {
        return Ok(None);
    }
    let shell = login_shell();
    read_login_env(&shell, SERVER_READ_TIMEOUT)
        .map(Some)
        .ok_or_else(|| format!("could not read the environment of the login shell ({shell})"))
}

#[cfg(not(unix))]
pub fn read_login_env_now() -> Result<Option<Vec<(std::ffi::OsString, std::ffi::OsString)>>, String> {
    Ok(None)
}

#[cfg(unix)]
fn login_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into())
}

#[cfg(unix)]
fn path_cache(identifier: &str) -> Option<std::path::PathBuf> {
    dirs::cache_dir().map(|d| d.join(identifier).join("login-path"))
}

#[cfg(unix)]
fn write_cache(cache: &std::path::Path, path: &str) {
    if let Some(dir) = cache.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(cache, path);
}

#[cfg(unix)]
fn login_path(env: &[(OsString, OsString)]) -> Option<String> {
    env.iter().find(|(k, _)| k == "PATH").and_then(|(_, v)| v.to_str()).filter(|p| !p.is_empty()).map(str::to_owned)
}

/// Variables the shell sets for itself rather than the user's configuration.
#[cfg(unix)]
const SHELL_OWN: &[&str] = &["PWD", "OLDPWD", "SHLVL", "_"];

/// The environment of `shell` started as an interactive login shell; PATH included, the
/// shell's own bookkeeping left out.
#[cfg(unix)]
pub(crate) fn read_login_env(shell: &str, timeout: std::time::Duration) -> Option<Vec<(OsString, OsString)>> {
    use std::io::Read;
    use std::os::unix::ffi::OsStrExt;
    use std::process::{Command, Stdio};
    use std::time::Instant;

    const MARK: &str = "__OXIDEA_ENV__";
    let mut cmd = Command::new(shell);
    // As at startup: what this process took from the shell (and what a reload added) must
    // not go in again, or rc files that append to PATH would append twice and a variable
    // they no longer export would come back.
    if let Some(startup) = STARTUP_ENV.get() {
        let now: Vec<OsString> = std::env::vars_os().map(|(k, _)| k).collect();
        let overlay = ide_core::process::env_overlay();
        for k in now.iter().chain(overlay.iter().map(|(k, _)| k)) {
            let before = startup.iter().find(|(n, _)| n == k).map(|(_, v)| v);
            if overlay.iter().any(|(o, _)| o == k) || std::env::var_os(k).as_ref() != before {
                match before {
                    Some(v) => cmd.env(k, v),
                    None => cmd.env_remove(k),
                };
            }
        }
    }
    // Nor what the last read found that this process lacks: children get it, the shell must not.
    for (k, _) in ide_core::process::shell_env() {
        cmd.env_remove(k);
    }
    ide_core::process::prepare(&mut cmd);
    let mut child = cmd
        .args(["-ilc", &format!("printf '{MARK}'; /usr/bin/env -0; printf '{MARK}'")])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Read while waiting: a large environment would fill the pipe and stall the shell.
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        stdout.read_to_end(&mut out).map(|_| out)
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                tracing::warn!("timed out reading the environment from {shell}");
                return None;
            }
        }
    }
    let out = reader.join().ok()?.ok()?;
    let mark = MARK.as_bytes();
    let start = out.windows(mark.len()).position(|w| w == mark)? + mark.len();
    let len = out[start..].windows(mark.len()).position(|w| w == mark)?;
    let vars = out[start..start + len]
        .split(|&b| b == 0)
        .filter_map(|entry| {
            let eq = entry.iter().position(|&b| b == b'=')?;
            let (k, v) = (OsStr::from_bytes(&entry[..eq]), OsStr::from_bytes(&entry[eq + 1..]));
            (!k.is_empty() && !SHELL_OWN.iter().any(|s| k == *s)).then(|| (k.to_owned(), v.to_owned()))
        })
        .collect();
    Some(vars)
}

/// Windows GUI 进程继承 explorer 的 PATH 快照（登录时抓的），之后安装/修改的
/// 工具（git、mvn、新 JDK…）不在里面——终端里有、IDE 里没有的经典错位。
/// 从注册表读**最新的**系统 + 用户 PATH 合并进进程环境（启动早期单线程，
/// 对齐 unix 侧 `set_var` 的安全前提）。已是最新则不动（从终端启动的场景）。
#[cfg(windows)]
pub fn inherit_login_shell_path(_identifier: &str) -> Option<fn()> {
    if std::env::var_os("OXIDEA_SKIP_SHELL_ENV").is_some() {
        return None;
    }
    let fresh = registry_path()?;
    let current = std::env::var_os("PATH")?;
    if fresh == current {
        return None;
    }
    // SAFETY: documented to run at startup before any other thread reads the environment.
    unsafe { std::env::set_var("PATH", fresh) };
    Some(|| {})
}

#[cfg(all(not(unix), not(windows)))]
pub fn inherit_login_shell_path(_identifier: &str) -> Option<fn()> {
    None
}

/// 注册表里的最新 PATH：系统（HKLM，在前）+ 用户（HKCU，在后）。
/// `REG_EXPAND_SZ` 的 `%VAR%` 用当前进程环境展开（SystemRoot 等核心变量
/// GUI 进程一定有）；读不到注册表返回 None（保持现状，不破坏启动）。
#[cfg(windows)]
fn registry_path() -> Option<std::ffi::OsString> {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    use winreg::RegKey;

    let machine = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey("SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Environment")
        .ok()?;
    let user = RegKey::predef(HKEY_CURRENT_USER).open_subkey("Environment").ok();
    let read = |key: &RegKey| -> Option<String> {
        let raw = key.get_raw_value("Path").ok()?;
        let text = match raw {
            winreg::RegValue { vtype: winreg::enums::RegType::REG_EXPAND_SZ, bytes } => {
                let wide: Vec<u16> = bytes.chunks(2).map(|c| u16::from_le_bytes([c[0], c.get(1).copied().unwrap_or(0)])).collect();
                expand_env_vars(&decode_utf16(&wide))
            }
            winreg::RegValue { vtype: winreg::enums::RegType::REG_SZ, bytes } => {
                let wide: Vec<u16> = bytes.chunks(2).map(|c| u16::from_le_bytes([c[0], c.get(1).copied().unwrap_or(0)])).collect();
                decode_utf16(&wide)
            }
            // Path 只可能是 SZ / EXPAND_SZ；其余类型（理论上不出现）跳过。
            _ => return None,
        };
        (!text.trim().is_empty()).then_some(text)
    };
    let mut parts = Vec::new();
    if let Some(p) = read(&machine) {
        parts.push(p);
    }
    if let Some(user) = &user {
        if let Some(p) = read(user) {
            parts.push(p);
        }
    }
    if parts.is_empty() {
        return None;
    }
    let joined: String = parts.join(";");
    // 最终防线：任何残留 NUL 都会让 `set_var` panic（WinAPI 不接受 NUL）——
    // 启动崩溃比 PATH 旧严重得多，这里再滤一次兜住 expand 展开值等来源。
    Some(joined.chars().filter(|&c| c != '\0').collect::<String>().into())
}

/// 注册表 REG_SZ 的 UTF-16 解码：去掉全部 NUL——UTF-16 的终止符与损坏值里的
/// 内嵌 NUL 都会出现，`set_var` 遇 NUL 直接 panic（启动路径绝不能崩）。
#[cfg(windows)]
fn decode_utf16(wide: &[u16]) -> String {
    String::from_utf16_lossy(wide).chars().filter(|&c| c != '\0').collect()
}

/// 展开 `%VAR%`（找不到的变量保留原样——与 `ExpandEnvironmentStringsW` 一致）。
#[cfg(windows)]
fn expand_env_vars(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) => {
                let name = &after[..end];
                match std::env::var(name) {
                    Ok(value) => out.push_str(&value),
                    Err(_) => out.push_str(&rest[start..start + end + 2]),
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(not(unix))]
pub fn inherit_login_shell_env(_identifier: &str) {}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn decode_utf16_strips_every_nul() {
        // 尾部终止符 + 内嵌 NUL（损坏值）都必须清掉：set_var 遇 NUL 会 panic。
        let wide: Vec<u16> = "C:\\bin\0;C:\\bin2\\\0;tail\0".encode_utf16().collect();
        assert_eq!(decode_utf16(&wide), "C:\\bin;C:\\bin2\\;tail");
        assert!(!decode_utf16(&wide).contains('\0'));
    }

    #[test]
    fn registry_path_never_contains_nul() {
        // 真实注册表读取（本机必有 PATH）：返回值必须可直接 set_var。
        if let Some(path) = registry_path() {
            let text = path.to_string_lossy();
            assert!(!text.contains('\0'), "registry PATH contains NUL: {text:?}");
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    /// A stand-in login shell: runs the IDE's `-ilc` script after exporting what `rc` holds.
    fn fake_shell(dir: &std::path::Path) -> (String, std::path::PathBuf) {
        let rc = dir.join("rc");
        let shell = dir.join("shell");
        std::fs::write(&shell, format!("#!/bin/sh\n. {}\nshift\nexec /bin/sh -c \"$1\"\n", rc.display())).unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
        (shell.to_string_lossy().into_owned(), rc)
    }

    #[test]
    fn every_read_sees_the_current_rc_file() {
        let tmp = tempfile::tempdir().unwrap();
        let (shell, rc) = fake_shell(tmp.path());
        let get = |env: &[(OsString, OsString)], k: &str| {
            env.iter().find(|(n, _)| n == k).map(|(_, v)| v.to_string_lossy().into_owned())
        };
        std::fs::write(&rc, "export OXIDEA_TEST_RC=one\nexport PATH=/one/bin:$PATH\n").unwrap();
        let first = read_login_env(&shell, Duration::from_secs(10)).unwrap();
        assert_eq!(get(&first, "OXIDEA_TEST_RC").as_deref(), Some("one"));
        assert!(get(&first, "PATH").unwrap().starts_with("/one/bin:"));
        assert!(get(&first, "PWD").is_none() && get(&first, "SHLVL").is_none());

        std::fs::write(&rc, "export OXIDEA_TEST_RC='two words'\n").unwrap();
        let second = read_login_env(&shell, Duration::from_secs(10)).unwrap();
        assert_eq!(get(&second, "OXIDEA_TEST_RC").as_deref(), Some("two words"));

        // What a read found goes to children, not into the next read: an rc file that no
        // longer exports a variable drops it.
        ide_core::process::set_shell_env(second);
        std::fs::write(&rc, "export OXIDEA_TEST_SEEN=\"${OXIDEA_TEST_RC:-unset}\"\n").unwrap();
        let third = read_login_env(&shell, Duration::from_secs(10)).unwrap();
        assert_eq!(get(&third, "OXIDEA_TEST_RC"), None);
        assert_eq!(get(&third, "OXIDEA_TEST_SEEN").as_deref(), Some("unset"));
    }

    #[test]
    fn a_shell_that_hangs_times_out() {
        let tmp = tempfile::tempdir().unwrap();
        let (shell, rc) = fake_shell(tmp.path());
        std::fs::write(&rc, "sleep 5\n").unwrap();
        assert!(read_login_env(&shell, Duration::from_millis(200)).is_none());
    }
}
