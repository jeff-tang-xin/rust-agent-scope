// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
mod commands;
mod config;
mod db_exec;
mod db_kind;
mod default_prompt;
mod factory;
mod session;
mod ssh_tool;
mod terminal_capture;
mod team;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    commands::run();
}
