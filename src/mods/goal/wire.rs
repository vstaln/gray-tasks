//! The goal part's wire surface inside gray-tasks.
//!
//! `prompt/context` serves the active goal + next unchecked item,
//! `command/run` carries `/goal …`, `tool/call` carries the `goal` tool, and
//! `agent/before_start` delivers the completion nudge. `handle_goal` is one
//! request in, one reply out; gray-tasks' dispatcher fans the hooks out.
//!
//! Blocking serde only, same shape as the standalone `gray-goal` sidecar.

use std::path::{Path, PathBuf};

use crate::goal::Store;
use crate::goal::cli;

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

/// Merged-plugin entry point: one request → reply, dispatched the way the
/// standalone `gray-goal` sidecar did. Notifications (no id) get no reply;
/// unknown methods get JSON-RPC -32601.
pub fn handle_goal(req: &serde_json::Value) -> (Option<serde_json::Value>, bool) {
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let Some(id) = req.get("id").cloned() else {
        return (None, method == "plugin/shutdown");
    };
    let params = req
        .get("params")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let home = match crate::goal::gray_home() {
        Ok(h) => h,
        Err(e) => {
            let error =
                serde_json::json!({ "code": "unavailable", "message": format!("goal: {e:#}") });
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
    (
        Some(serde_json::json!({ "id": id, "result": result })),
        false,
    )
}
