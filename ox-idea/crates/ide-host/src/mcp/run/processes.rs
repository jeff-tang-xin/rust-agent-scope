//! `get_running_processes` and `stop_process`.
//!
//! Agent 侧的进程簿记闭环：`execute_run_configuration` 能启动但此前停不掉、
//! 看不到运行清单——长驻服务（dev server / Spring Boot）启动后 agent 失联，
//! 只能留给用户手动停。这两个工具包装 `ProcessManager`（ide-run，API 现成）：
//! list 给「现在跑着什么」，stop 给「优雅终止」。日志不在此列——execute 的
//! 结果已带 `fullOutputPath`（持续增长的完整输出文件），用 read_file 轮询即可，
//! 不为包装而改 ProcessManager。

use crate::mcp::tool::{Output, Param, ParamType, Tool};
use serde_json::json;
use std::time::Duration;

pub fn get_running_processes() -> Tool {
    Tool::new(
        "get_running_processes",
        "Lists processes started through run configurations in this project (running first, then recently \
exited). Each entry has `runId`, `name`, `kind`, `command`, `pid` (when known) and `running`.\n\n\
Use this tool to check what is already running before starting a new process, and to find the \
`runId` for `stop_process`. To read a process's output, read the `fullOutputPath` file returned \
by `execute_run_configuration` - it keeps growing while the process is alive.",
        |call| async move {
            let host = call.host();
            let mut running = Vec::new();
            let mut exited = Vec::new();
            for info in host.processes.list() {
                let entry = json!({
                    "runId": info.id.0,
                    "name": info.name,
                    "pid": info.pid,
                    "running": info.running,
                    "pty": info.pty,
                });
                if info.running {
                    running.push(entry);
                } else {
                    exited.push(entry);
                }
            }
            let exited: Vec<_> = exited.into_iter().take(10).collect();
            let hint = if running.is_empty() {
                "No processes are running."
            } else {
                "Pass `runId` to stop_process to terminate one."
            };
            Output::json(json!({ "running": running, "recentlyExited": exited, "hint": hint }))
        },
    )
}

pub fn stop_process() -> Tool {
    Tool::new(
        "stop_process",
        "Gracefully terminates a process previously started with `execute_run_configuration`, by its \
`runId` (from `get_running_processes`). Waits up to `graceSeconds` (default 5) for a graceful \
exit, then force-kills. The user is asked for confirmation first.",
        |call| async move {
            let run_id = match call.opt_i64("runId")? {
                Some(id) if id > 0 => id as u32,
                _ => {
                    return Err(ide_core::IdeError::invalid(
                        "`runId` must be a positive integer from get_running_processes",
                    ));
                }
            };
            let grace = call.opt_i64("graceSeconds")?.unwrap_or(5).clamp(1, 30) as u64;
            call.confirm("stop_process", Some(format!("Terminate run #{run_id}"))).await?;
            call.host().processes.stop_with_grace(ide_core::RunId(run_id), Duration::from_secs(grace))?;
            Output::json(json!({ "stopped": run_id }))
        },
    )
    .write()
    .param(Param::required("runId", ParamType::Integer, "runId from get_running_processes"))
    .param(Param::optional(
        "graceSeconds",
        ParamType::Integer,
        "Seconds to wait for graceful exit before force-kill (1-30, default 5)",
    ))
}
