//! `gray-goal` sidecar + CLI, one binary:
//!
//! - `gray-goal manifest` → manifest JSON on stdout (install probe;
//!   answering registers `cli_argv`, so `gray goal …` `exec`s back here)
//! - `gray-goal sidecar` / bare binary on a pipe → wire v1.1 NDJSON loop:
//!   `prompt/context` serves the active goal + next unchecked item,
//!   `command/run` carries `/goal …`, `tool/call` carries the `goal` tool,
//!   `agent/before_start` delivers the completion nudge
//! - `gray-goal <args>` → the CLI surface (also `audit-run`, the detached
//!   auditor child `/goal audit` spawns)
//!
//! Blocking std I/O, same shape as `gray-memory`/`gray-questions`; no async.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::goal::cli;
use crate::goal::{PLUGIN_NAME, Store};

fn session_cwd(params: &serde_json::Value) -> PathBuf {
    params["session"]["cwd"]
        .as_str()
        .or_else(|| params["cwd"].as_str())
        .filter(|s| !s.is_empty())
        .map(Path::new)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

/// `command/run`: `/goal` argv → the shared clap surface. The reply is
/// `{"text"}` or `{"prompt","text"}` (creation hands the session a
/// checklist-drafting prompt).
fn command_run(home: &Path, params: &serde_json::Value) -> serde_json::Value {
    let argv: Vec<String> = params
        .get("argv")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|e| e.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let cwd = session_cwd(params);
    match cli::args_for(argv) {
        Ok(args) => match cli::run_at(&args, home, &cwd) {
            Ok(reply) => reply.to_wire(),
            Err(e) => serde_json::json!({ "text": format!("goal: {e:#}") }),
        },
        Err(e) => serde_json::json!({ "text": e.to_string() }),
    }
}

/// `tool/call`: the `goal` tool. `{}` args → `status` (the checker probes
/// with empty args).
fn tool_call(home: &Path, params: &serde_json::Value) -> serde_json::Value {
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    if name != crate::goal::TOOL_NAME && !name.is_empty() {
        return serde_json::json!({
            "content": format!("unknown tool: {name}"),
            "is_error": true,
        });
    }
    let cwd = session_cwd(params);
    let args = params
        .get("args")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let store = match Store::new(home) {
        Ok(s) => s,
        Err(e) => {
            return serde_json::json!({ "content": format!("goal: {e:#}"), "is_error": true });
        }
    };
    let _ = cwd;
    let (content, is_error) = cli::tool_call(&store, &args);
    serde_json::json!({ "content": content, "is_error": is_error })
}

/// Merged-plugin entry point (added for gray-tasks): one request → reply,
/// same dispatch as `sidecar_loop` below. Notifications (no id) get no reply;
/// unknown methods get JSON-RPC -32601 instead of being dropped.
pub fn handle_goal(req: &serde_json::Value) -> (Option<serde_json::Value>, bool) {
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let Some(id) = req.get("id").cloned() else {
        return (None, method == "plugin/shutdown");
    };
    let params = req.get("params").cloned().unwrap_or(serde_json::Value::Null);
    let home = match crate::goal::gray_home() {
        Ok(h) => h,
        Err(e) => {
            let error = serde_json::json!({ "code": "unavailable", "message": format!("goal: {e:#}") });
            return (Some(serde_json::json!({ "id": id, "error": error })), false);
        }
    };
    let result = match method {
        "plugin/manifest" => crate::goal::manifest_goal(),
        "prompt/context" => match Store::new(&home) {
            Ok(store) => serde_json::json!({ "text": crate::goal::served_text(&store) }),
            Err(_) => serde_json::json!({}),
        },
        "agent/before_start" => match Store::new(&home) {
            Ok(store) => match crate::goal::completion_nudge(&store) {
                Some(text) => serde_json::json!({ "text": text }),
                None => serde_json::json!({}),
            },
            Err(_) => serde_json::json!({}),
        },
        "command/run" => command_run(&home, &params),
        "tool/call" => tool_call(&home, &params),
        "plugin/shutdown" => return (Some(serde_json::json!({ "id": id, "result": {} })), true),
        _ => {
            let error = serde_json::json!({ "code": -32601, "message": format!("method not found: {method}") });
            return (Some(serde_json::json!({ "id": id, "error": error })), false);
        }
    };
    (Some(serde_json::json!({ "id": id, "result": result })), false)
}

fn sidecar_loop() -> anyhow::Result<()> {
    let home = crate::goal::gray_home()?;
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if v.get("method").and_then(|m| m.as_str()) == Some("plugin/shutdown") {
            break;
        }
        // Notifications (no id) need no reply: event/notify lands here.
        let Some(id) = v.get("id").and_then(|i| i.as_u64()) else {
            continue;
        };
        let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = v.get("params").cloned().unwrap_or(serde_json::Value::Null);
        let result = match method {
            "plugin/manifest" => crate::goal::manifest_goal(),
            "prompt/context" => match Store::new(&home) {
                Ok(store) => serde_json::json!({ "text": crate::goal::served_text(&store) }),
                Err(_) => serde_json::json!({}),
            },
            "agent/before_start" => match Store::new(&home) {
                Ok(store) => match crate::goal::completion_nudge(&store) {
                    Some(text) => serde_json::json!({ "text": text }),
                    None => serde_json::json!({}),
                },
                Err(_) => serde_json::json!({}),
            },
            "command/run" => command_run(&home, &params),
            "tool/call" => tool_call(&home, &params),
            _ => continue, // unknown methods/lines are ignored
        };
        let reply = serde_json::json!({ "id": id, "result": result });
        let mut o = stdout.lock();
        writeln!(o, "{reply}")?;
        o.flush()?;
    }
    Ok(())
}

pub fn main_goal() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("manifest") => {
            println!("{}", crate::goal::manifest_goal());
            return;
        }
        Some("sidecar") => {
            if let Err(e) = sidecar_loop() {
                eprintln!("{PLUGIN_NAME}: {e:#}");
                std::process::exit(1);
            }
            return;
        }
        None => {
            // A TTY human gets the hint; a pipe/host gets the wire loop.
            if std::io::stdin().is_terminal() {
                eprintln!(
                    "gray-goal is a plugin sidecar — drive it via `gray goal …` or install with `gray plugin install`"
                );
                return;
            }
            if let Err(e) = sidecar_loop() {
                eprintln!("{PLUGIN_NAME}: {e:#}");
                std::process::exit(1);
            }
            return;
        }
        _ => {}
    }
    match cli::args_for(args) {
        Ok(args) => match cli::run(&args) {
            Ok(reply) => print!("{}", reply.text),
            Err(e) => {
                eprintln!("{e:#}");
                std::process::exit(1);
            }
        },
        Err(e) => {
            // clap prints its own usage for flag errors; exit code is its contract.
            std::process::exit(if e.use_stderr() { 1 } else { 0 });
        }
    }
}
