//! OXidea 鸿蒙（OpenHarmony）native 桥接 crate —— P3 起点。
//!
//! 编成 cdylib（`.so`）供鸿蒙 ArkTS 经 NAPI 调用，替代 Tauri 的 JS↔Rust IPC。
//! 当前是最小链接验证 stub：暴露一个 `extern "C"` 探针，真实链接 `ide-core`，
//! 用以验证 OHOS 完整编译+链接链（NDK clang + sysroot）。后续替换为真正的 NAPI 入口。

/// 链接探针：返回 `ide-core` 报告的 Windows build number（非 Windows 恒为 -1）。
/// 仅用于证明 `ide-core` 被链入 `.so`；鸿蒙上恒返回 -1。
#[unsafe(no_mangle)]
pub extern "C" fn oxidea_ohos_probe() -> i32 {
    match ide_core::platform::windows_build_number() {
        Some(build) => build as i32,
        None => -1,
    }
}
