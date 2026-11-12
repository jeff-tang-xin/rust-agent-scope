//! Debug sessions: start/stop, breakpoints, control, state polling, evaluation.
//!
//! Agent 侧的调试闭环（方案 docs/agent-ide-awareness-plan.md 改动 6）：`ide-dap`
//! 是完整 DAP 客户端、`ops::debug` 已含全部环境准备（debugpy 解释器、delve/kotlin/
//! rdbg SDK、coalescing sink），这里只做 MCP 工具化映射。DAP 事件仍推给前端 UI
//! （同一会话双消费）；agent 用 `get_debug_state` 拉模式轮询，不接事件订阅。
//!
//! 行号约定：工具参数与返回一律 1-based（对 agent 与编辑器一致），转 DAP 的
//! 0-based 只发生在本文件边界。

use crate::mcp::tool::{Call, Output, Param, ParamType, Tool};
use ide_core::{DebugSessionId, IdeError, IdeResult};
use ide_dap::{Breakpoint, LaunchConfig, StackFrame, StepKind};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub fn tools() -> Vec<Tool> {
    vec![
        start_debug_session(),
        set_breakpoint(),
        remove_breakpoint(),
        debug_control(),
        get_debug_state(),
        evaluate_expression(),
        stop_debug_session(),
    ]
}

// ─── shared helpers ──────────────────────────────────────────────────────────

/// The session to act on: an explicit `sessionId`, or the only live session of
/// this window. With several live sessions the agent must name one — guessing
/// would step the wrong program.
fn resolve_session(call: &Call, id: Option<i64>) -> IdeResult<DebugSessionId> {
    let sessions = crate::ops::debug::debug_sessions(&call.host(), call.window())?;
    match id {
        Some(n) if n > 0 => {
            let id = DebugSessionId(n as u32);
            if sessions.iter().any(|s| s.id == id) {
                Ok(id)
            } else {
                Err(IdeError::not_found(format!(
                    "debug session #{n} not found in this project; call get_debug_state without `sessionId` to list live sessions"
                )))
            }
        }
        Some(_) => Err(IdeError::invalid("`sessionId` must be a positive integer")),
        None => match sessions.as_slice() {
            [one] => Ok(one.id),
            [] => Err(IdeError::not_found(
                "no live debug session; start one with start_debug_session",
            )),
            many => Err(IdeError::invalid(format!(
                "{} debug sessions are live ({}); pass `sessionId`",
                many.len(),
                many.iter().map(|s| format!("#{}", s.id.0)).collect::<Vec<_>>().join(", ")
            ))),
        },
    }
}

fn positive_or_invalid(name: &str, value: Option<i64>) -> IdeResult<Option<i64>> {
    match value {
        None => Ok(None),
        Some(n) if n > 0 => Ok(Some(n)),
        Some(_) => Err(IdeError::invalid(format!("`{name}` must be a positive integer"))),
    }
}

/// Frames of a thread; empty when it is running (adapters return an empty trace
/// or an error for a thread that is not stopped — both mean "running" here).
async fn thread_frames(
    host: &crate::Host,
    window: &str,
    session: DebugSessionId,
    thread_id: i64,
) -> Vec<StackFrame> {
    match crate::ops::debug::debug_stack_trace(host, window, session, thread_id).await {
        Ok(frames) if !frames.is_empty() => frames,
        _ => Vec::new(),
    }
}

/// The first stopped thread's top frame, for tools that default to "where the
/// program stopped" (`evaluate_expression` without `frameId`).
async fn first_stopped_frame(
    host: &crate::Host,
    window: &str,
    session: DebugSessionId,
) -> Option<StackFrame> {
    let threads = crate::ops::debug::debug_threads(host, window, session).await.ok()?;
    for t in threads {
        let frames = thread_frames(host, window, session, t.id).await;
        if let Some(top) = frames.into_iter().next() {
            return Some(top);
        }
    }
    None
}

fn variable_json(v: &ide_dap::Variable) -> Value {
    json!({
        "name": v.name,
        "value": v.value,
        "type": v.type_name,
        // > 0: pass as `variablesReference` to get_debug_state to expand children.
        "variablesReference": v.variables_reference,
    })
}

fn breakpoint_json(path_display: &str, states: &[ide_dap::BreakpointState]) -> Value {
    let points: Vec<_> = states
        .iter()
        .map(|s| {
            json!({
                "line": s.line + 1, // 1-based for the agent
                "verified": s.verified,
                "message": s.message,
            })
        })
        .collect();
    json!({ "file": path_display, "breakpoints": points })
}

// ─── start / stop ────────────────────────────────────────────────────────────

fn start_debug_session() -> Tool {
    Tool::new(
        "start_debug_session",
        "Starts a debug session (DAP) for this project and waits until the adapter is \
initialized and configuration is delivered. The user is asked for confirmation first.\n\n\
Pass the DAP `adapter` name (java-debug for Java, kotlin-debug-adapter, codelldb for Rust/C++, \
delve for Go, js-debug for Node, debugpy for Python, netcoredbg for .NET, lldb-dap, rdbg for Ruby) \
and what to debug: `program` plus optional `args`/`cwd`/`env`, or adapter-specific options via \
`extraJson` (a JSON object as text, e.g. `{\"cargo\": {\"args\": [\"build\"]}}` builds with cargo \
and launches the produced executable, `{\"debugServer\": 5005}` attaches to an adapter already \
listening on that port, `{\"vmArgs\": \"...\"}` for java-debug).\n\n\
Typical flow: set_breakpoint first (or after starting), start_debug_session, then poll \
get_debug_state until a thread is stopped at a breakpoint, inspect variables, \
evaluate_expression, debug_control to step, stop_debug_session when done.\n\n\
`request` is `launch` (default) or `attach` (attach to an already-running process; for Java \
remote attach use adapter `java-debug` with `extraJson` `{\"debugServer\": \"host:port\"}`).",
        |call| async move {
            let adapter = call.str("adapter")?;
            let request = call.opt_str("request")?.unwrap_or_else(|| "launch".into());
            if request != "launch" && request != "attach" {
                return Err(IdeError::invalid("`request` must be `launch` or `attach`"));
            }
            let program = call.opt_str("program")?;
            let args = call.str_list("args")?;
            let cwd = match call.opt_str("cwd")? {
                Some(p) => Some(call.resolve_inside(&p)?.into()),
                None => None,
            };
            let env: BTreeMap<_, _> = call
                .args
                .get("env")
                .filter(|v| !v.is_null())
                .map(|_| call.str_map("env"))
                .transpose()?
                .unwrap_or_default()
                .into_iter()
                .collect();
            let extra_json = call.opt_str("extraJson")?;
            if let Some(text) = &extra_json
                && serde_json::from_str::<Value>(text).is_err()
            {
                return Err(IdeError::invalid("`extraJson` must be a JSON object as text"));
            }
            let name = call
                .opt_str("name")?
                .unwrap_or_else(|| program.clone().unwrap_or_else(|| adapter.clone()));
            let config =
                LaunchConfig { name, adapter: adapter.clone(), request: request.clone(), program: program.clone(), args, cwd, env, extra_json };
            let mut detail = format!("Start debug session: adapter `{adapter}` ({request})");
            if let Some(program) = &program {
                detail.push_str(&format!(", program `{program}`"));
            }
            call.confirm("start_debug_session", Some(detail)).await?;
            let id = crate::ops::debug::debug_start(&call.host(), call.window(), config).await?;
            Output::json(json!({
                "sessionId": id.0,
                "hint": "Poll get_debug_state to see threads; a thread with frames is stopped. \
stop_debug_session terminates the session and kills the debuggee.",
            }))
        },
    )
    .write()
    .param(Param::required("adapter", ParamType::String, "DAP adapter name: java-debug, kotlin-debug-adapter, codelldb (Rust/C++), delve (Go), js-debug (Node), debugpy (Python), netcoredbg (.NET), lldb-dap, rdbg (Ruby)"))
    .param(Param::optional("request", ParamType::String, "`launch` (default) or `attach`"))
    .param(Param::optional("program", ParamType::String, "Program to debug (executable, script or module path), relative to the project root"))
    .param(Param::optional("args", ParamType::StringList, "Program arguments"))
    .param(Param::optional("cwd", ParamType::String, "Working directory, relative to the project root (default: project root)"))
    .param(Param::optional("env", ParamType::StringMap, "Extra environment variables for the debuggee"))
    .param(Param::optional("extraJson", ParamType::String, "Adapter-specific options as JSON text, merged into the launch request (e.g. `{\"cargo\": {\"args\": [\"build\"]}}`, `{\"debugServer\": 5005}`)"))
    .param(Param::optional("name", ParamType::String, "Session name shown in the UI (default: program or adapter)"))
}

fn stop_debug_session() -> Tool {
    Tool::new(
        "stop_debug_session",
        "Terminates a debug session and kills the debuggee process tree. The user is asked \
for confirmation first. Pass the `sessionId` from start_debug_session (or get_debug_state); \
omit it when exactly one session is live.",
        |call| async move {
            let session = resolve_session(&call, positive_or_invalid("sessionId", call.opt_i64("sessionId")?)?)?;
            call.confirm("stop_debug_session", Some(format!("Stop debug session #{0}", session.0)))
                .await?;
            crate::ops::debug::debug_stop(&call.host(), call.window(), session).await?;
            Output::json(json!({ "stopped": session.0 }))
        },
    )
    .write()
    .param(Param::optional("sessionId", ParamType::Integer, "Session id from start_debug_session; required when several sessions are live"))
}

// ─── breakpoints ─────────────────────────────────────────────────────────────

fn set_breakpoint() -> Tool {
    Tool::new(
        "set_breakpoint",
        "Sets (or updates) a line breakpoint in a project file and reports whether the \
installed adapters verified it. The user is asked for confirmation first. Works before or \
during a debug session; the breakpoint store is per file and shared with the editor UI.\n\n\
`line` is 1-based. An existing breakpoint on the same line is updated (e.g. to change \
`condition`). Use remove_breakpoint to delete. Note: setting a breakpoint does not start a \
session — call start_debug_session to debug.",
        |call| async move {
            let file = call.str("filePath")?;
            let path = call.resolve_writable(&file).await?;
            let line = positive_or_invalid("line", call.opt_i64("line")?)?;
            let condition = call.opt_str("condition")?;
            let mut breakpoints = call.host().debugger.stored_breakpoints(&path);
            let mut detail = format!("Set breakpoint in {}", call.display(&path));
            if let Some(line) = line {
                let line0 = (line - 1) as u32;
                detail.push_str(&format!(" at line {line}"));
                if let Some(slot) = breakpoints.iter_mut().find(|b| b.line == line0) {
                    slot.condition = condition.clone();
                    slot.enabled = true;
                } else {
                    breakpoints.push(Breakpoint {
                        line: line0,
                        condition,
                        hit_condition: None,
                        log_message: None,
                        enabled: true,
                    });
                }
            }
            call.confirm("set_breakpoint", Some(detail)).await?;
            let states =
                crate::ops::debug::debug_set_breakpoints(&call.host(), call.window(), path.clone(), breakpoints).await?;
            Output::json(breakpoint_json(&call.display(&path), &states))
        },
    )
    .write()
    .param(Param::required("filePath", ParamType::String, "File to set the breakpoint in, relative to the project root"))
    .param(Param::optional("line", ParamType::Integer, "1-based line; omit to re-send the file's stored breakpoints to the adapters"))
    .param(Param::optional("condition", ParamType::String, "Break only when this expression is true (adapter syntax)"))
}

fn remove_breakpoint() -> Tool {
    Tool::new(
        "remove_breakpoint",
        "Removes a line breakpoint (one line, or all breakpoints of a file when `line` is \
omitted) and reports the file's remaining breakpoints. The user is asked for confirmation \
first.",
        |call| async move {
            let file = call.str("filePath")?;
            let path = call.resolve_writable(&file).await?;
            let line = positive_or_invalid("line", call.opt_i64("line")?)?;
            let mut breakpoints = call.host().debugger.stored_breakpoints(&path);
            let mut detail = format!("Remove breakpoints in {}", call.display(&path));
            match line {
                Some(line) => {
                    let line0 = (line - 1) as u32;
                    detail.push_str(&format!(" at line {line}"));
                    let before = breakpoints.len();
                    breakpoints.retain(|b| b.line != line0);
                    if breakpoints.len() == before {
                        return Output::json(breakpoint_json(&call.display(&path), &[]));
                    }
                }
                None => breakpoints.clear(),
            }
            call.confirm("remove_breakpoint", Some(detail)).await?;
            let states =
                crate::ops::debug::debug_set_breakpoints(&call.host(), call.window(), path.clone(), breakpoints).await?;
            Output::json(breakpoint_json(&call.display(&path), &states))
        },
    )
    .write()
    .param(Param::required("filePath", ParamType::String, "File to remove the breakpoint from, relative to the project root"))
    .param(Param::optional("line", ParamType::Integer, "1-based line; omit to clear all breakpoints of the file"))
}

// ─── control ─────────────────────────────────────────────────────────────────

fn debug_control() -> Tool {
    Tool::new(
        "debug_control",
        "Resumes or steps a stopped debug session: `action` is one of `continue` \
(run until the next breakpoint), `step_over`, `step_into`, `step_out`, or `pause` \
(suspend a running program). The user is asked for confirmation first.\n\n\
`threadId` defaults to the session's first thread — fine for single-threaded programs; \
for a specific thread take it from get_debug_state. After the action returns, poll \
get_debug_state to see where the program stopped next.",
        |call| async move {
            let action = call.str("action")?;
            if !matches!(action.as_str(), "continue" | "step_over" | "step_into" | "step_out" | "pause") {
                return Err(IdeError::invalid(format!(
                    "`action` must be one of continue, step_over, step_into, step_out, pause; got `{action}`"
                )));
            }
            let session = resolve_session(&call, positive_or_invalid("sessionId", call.opt_i64("sessionId")?)?)?;
            let host = call.host();
            let window = call.window().to_owned();
            let thread_id = match positive_or_invalid("threadId", call.opt_i64("threadId")?)? {
                Some(n) => n,
                None => {
                    let threads = crate::ops::debug::debug_threads(&host, &window, session).await?;
                    threads
                        .first()
                        .map(|t| t.id)
                        .ok_or_else(|| IdeError::unavailable("the session has no threads yet; poll get_debug_state"))?
                }
            };
            call.confirm("debug_control", Some(format!("{action} debug session #{0}", session.0)))
                .await?;
            match action.as_str() {
                "continue" => crate::ops::debug::debug_resume(&host, &window, session, thread_id).await?,
                "pause" => crate::ops::debug::debug_pause(&host, &window, session, thread_id).await?,
                "step_over" => crate::ops::debug::debug_step(&host, &window, session, thread_id, StepKind::Over).await?,
                "step_into" => crate::ops::debug::debug_step(&host, &window, session, thread_id, StepKind::Into).await?,
                "step_out" => crate::ops::debug::debug_step(&host, &window, session, thread_id, StepKind::Out).await?,
                _ => unreachable!("action validated above"),
            }
            Output::json(json!({
                "action": action,
                "sessionId": session.0,
                "threadId": thread_id,
                "hint": "Poll get_debug_state to see where the program is now.",
            }))
        },
    )
    .write()
    .param(Param::required("action", ParamType::String, "One of: continue, step_over, step_into, step_out, pause"))
    .param(Param::optional("sessionId", ParamType::Integer, "Session id; required when several sessions are live"))
    .param(Param::optional("threadId", ParamType::Integer, "Thread to act on (from get_debug_state); default: the session's first thread"))
}

// ─── state polling / evaluation ──────────────────────────────────────────────

fn get_debug_state() -> Tool {
    Tool::new(
        "get_debug_state",
        "Read-only poll of the current debug state. Without `variablesReference` it lists \
the live sessions (when `sessionId` is omitted) and, for the session, every thread with \
`stopped` true/false; for the first stopped thread it adds the top stack frame (file + \
1-based line), the scopes and their top-level variables.\n\n\
Pass a `variablesReference` (> 0, from a previous result) to expand that variable's children \
instead — one level per call. Poll this tool after start_debug_session and after every \
debug_control action; DAP events keep flowing to the UI regardless.",
        |call| async move {
            let host = call.host();
            let window = call.window().to_owned();
            let session_arg = positive_or_invalid("sessionId", call.opt_i64("sessionId")?)?;
            if let Some(reference) = positive_or_invalid("variablesReference", call.opt_i64("variablesReference")?)? {
                let session = resolve_session(&call, session_arg)?;
                let variables = crate::ops::debug::debug_variables(&host, &window, session, reference as i64).await?;
                let max = call.opt_i64("maxVariables")?.unwrap_or(20).clamp(1, 200) as usize;
                let truncated = variables.len() > max;
                let items: Vec<_> = variables.iter().take(max).map(variable_json).collect();
                return Output::json(json!({
                    "sessionId": session.0,
                    "variablesReference": reference,
                    "variables": items,
                    "truncated": truncated,
                }));
            }
            let sessions = crate::ops::debug::debug_sessions(&host, &window)?;
            if session_arg.is_none() {
                let list: Vec<_> = sessions
                    .iter()
                    .map(|s| json!({ "sessionId": s.id.0, "name": s.name, "adapter": s.adapter }))
                    .collect();
                if list.is_empty() {
                    return Output::json(json!({
                        "sessions": [],
                        "hint": "No live debug session. Set breakpoints with set_breakpoint and start one with start_debug_session.",
                    }));
                }
                if sessions.len() > 1 {
                    return Output::json(json!({
                        "sessions": list,
                        "hint": "Several sessions are live; pass `sessionId` to inspect one.",
                    }));
                }
            }
            let session = resolve_session(&call, session_arg)?;
            let threads = crate::ops::debug::debug_threads(&host, &window, session).await?;
            let mut threads_out = Vec::with_capacity(threads.len());
            let mut top: Option<(i64, StackFrame)> = None;
            for t in &threads {
                let frames = thread_frames(&host, &window, session, t.id).await;
                let stopped = !frames.is_empty();
                if stopped && top.is_none() {
                    if let Some(frame) = frames.first() {
                        top = Some((t.id, frame.clone()));
                    }
                }
                threads_out.push(json!({ "threadId": t.id, "name": t.name, "stopped": stopped }));
            }
            let mut result = json!({
                "sessionId": session.0,
                "threads": threads_out,
            });
            if let Some((thread_id, frame)) = top {
                let location = frame.path.as_ref().map(|p| {
                    json!({
                        "file": call.display(p.as_ref()),
                        "line": frame.line + 1, // 1-based for the agent
                        "column": frame.column + 1,
                    })
                });
                let scopes = crate::ops::debug::debug_scopes(&host, &window, session, frame.id).await?;
                let max = call.opt_i64("maxVariables")?.unwrap_or(20).clamp(1, 200) as usize;
                let mut scopes_out = Vec::with_capacity(scopes.len());
                for scope in &scopes {
                    let variables = crate::ops::debug::debug_variables(&host, &window, session, scope.variables_reference).await?;
                    let truncated = variables.len() > max;
                    let items: Vec<_> = variables.iter().take(max).map(variable_json).collect();
                    scopes_out.push(json!({
                        "name": scope.name,
                        "variables": items,
                        "truncated": truncated,
                    }));
                }
                result["stoppedAt"] = json!({
                    "threadId": thread_id,
                    "frameId": frame.id,
                    "function": frame.name,
                    "location": location,
                    "scopes": scopes_out,
                });
            }
            Output::json(result)
        },
    )
    .param(Param::optional("sessionId", ParamType::Integer, "Session id; required when several sessions are live"))
    .param(Param::optional("variablesReference", ParamType::Integer, "Expand this variable (> 0 from a previous result) one level instead of listing threads"))
    .param(Param::optional("maxVariables", ParamType::Integer, "Max variables per scope (1-200, default 20)"))
}

fn evaluate_expression() -> Tool {
    Tool::new(
        "evaluate_expression",
        "Read-only evaluation of an expression in a stopped debug session's frame context \
(watch/repl semantics of the adapter). `frameId` defaults to the first stopped thread's top \
frame; pass one from get_debug_state to evaluate in another frame. The session must be \
stopped — poll get_debug_state first.",
        |call| async move {
            let expression = call.str("expression")?;
            let session = resolve_session(&call, positive_or_invalid("sessionId", call.opt_i64("sessionId")?)?)?;
            let host = call.host();
            let window = call.window().to_owned();
            let frame_id = match positive_or_invalid("frameId", call.opt_i64("frameId")?)? {
                Some(n) => Some(n),
                None => first_stopped_frame(&host, &window, session).await.map(|f| f.id),
            };
            let variable = crate::ops::debug::debug_evaluate(&host, &window, session, frame_id, expression.clone()).await?;
            let mut result = variable_json(&variable);
            result["expression"] = json!(expression);
            result["sessionId"] = json!(session.0);
            Output::json(result)
        },
    )
    .param(Param::required("expression", ParamType::String, "Expression to evaluate in the frame context (adapter syntax)"))
    .param(Param::optional("sessionId", ParamType::Integer, "Session id; required when several sessions are live"))
    .param(Param::optional("frameId", ParamType::Integer, "Frame to evaluate in (from get_debug_state); default: first stopped thread's top frame"))
}

