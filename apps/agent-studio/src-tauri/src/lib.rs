// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
mod commands;
mod config;
mod factory;
mod session;
mod sql_tool;
mod team;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    commands::run();
}
