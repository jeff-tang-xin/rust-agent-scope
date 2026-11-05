// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
mod browser_tool;
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
mod req_tool;
mod session;
mod ssh_tool;
mod terminal_capture;
mod team;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    commands::run();
}
