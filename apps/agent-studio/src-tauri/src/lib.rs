// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
// 浏览器工具集已下沉为框架级 crate（studio 与 ox-idea 共用）；别名保持
// `crate::browser_tool::X` 路径不变，factory.rs 无需改动。
use agentscope_browser as browser_tool;
// testing 的 #[tauri::command] 生成的隐藏宏（__cmd__test_* 等）经 #[macro_use]
// 进入后续模块作用域——必须声明在 mod commands 之前（macro_rules 按源码顺序可见）。
#[macro_use]
mod testing;
mod commands;
mod config;
mod db_exec;
mod db_kind;
mod default_prompt;
mod factory;
mod api_tool;
mod req_tool;mod session;
mod ssh_tool;
mod team;
mod testing_tools;
mod terminal_capture;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    commands::run();
}
