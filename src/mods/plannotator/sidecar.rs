//! `gray-plannotator` sidecar: JSON lines on stdio (gray wire v1).
//!
//! Wire summary (same shape as `gray-questions`/`gray-subagents`):
//! - host→sidecar `{"id":N,"method":"plugin/manifest"}` → `{"id":N,"result":{…}}`
//! - host→sidecar `{"id":N,"method":"prompt/context","params":{"cwd",…}}`
//!   → `{"id":N,"result":{"text": …}}` — empty when no review is open
//! - host→sidecar `{"id":N,"method":"command/run","params":{"name","argv",
//!   "session":{"id","cwd"}}}` → `{"text"}` (say) or `{"prompt"}` (the
//!   host submits it as a user turn — this is how annotations reach the
//!   agent as a revision request)
//! - host→sidecar `{"method":"plugin/shutdown"}` (no id: exit cleanly)
//!
//! Bare invocation is the sidecar (that's what `gray plugin check`
//! spawns); `sidecar` is the installed `sidecar_args` form. `manifest`
//! prints the manifest for installers; `status` is a debug helper.

use crate::plannotator as p;
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::path::PathBuf;

/// Session cwd out of `params.session.cwd` (command/run) or `params.cwd`
/// (prompt/context); falls back to our own cwd so the commands still
/// behave sensibly under `gray plugin check`.
fn session_cwd(params: &Value) -> PathBuf {
    params
        .get("session")
        .and_then(|s| s.get("cwd"))
        .and_then(Value::as_str)
        .or_else(|| params.get("cwd").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn handle_plannotator(req: &Value) -> Option<Value> {
    let id = req.get("id").cloned()?;
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "plugin/manifest" => p::manifest_plannotator(),
        "prompt/context" => p::prompt_context(&session_cwd(&params)),
        "command/run" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let argv: Vec<String> = params
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            let cwd = session_cwd(&params);
            match name {
                n if n == p::PLAN_REVIEW => p::plan_review(&cwd, &argv),
                n if n == p::PLAN_ACCEPT => p::plan_accept(&cwd),
                _ => json!({"text": format!("unknown command: {name}")}),
            }
        }
        _ => {
            return Some(json!({
                "id": id,
                "error": {"code": -32601, "message": "method not found"},
            }));
        }
    };
    Some(json!({"id": id, "result": result}))
}

fn sidecar() -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        // `plugin/shutdown` is a notification (no id): stop cleanly.
        // Anything else id-less (event/notify, …) needs no reply.
        if req.get("method").and_then(Value::as_str) == Some("plugin/shutdown") {
            break;
        }
        if let Some(reply) = handle_plannotator(&req) {
            writeln!(stdout, "{reply}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

const USAGE: &str = "\
gray-plannotator — annotated plan review for gray

  gray-plannotator [sidecar]   run the JSONL sidecar (default)
  gray-plannotator manifest    print the plugin manifest
  gray-plannotator status      show the review open in this directory

In gray: /plan-review starts/advances the loop, /plan-accept approves.";

pub fn main_plannotator(args: &[String]) -> std::io::Result<()> {
    match args.first().map(String::as_str) {
        Some("manifest") => {
            println!("{}", p::manifest_plannotator());
            Ok(())
        }
        Some("status") => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            match p::load_state(&cwd) {
                Some(st) => println!(
                    "review open: {} · phase {:?} · round {}",
                    st.plan_path.display(),
                    st.phase,
                    st.round
                ),
                None => println!("no plan under review here"),
            }
            Ok(())
        }
        Some("--help") | Some("-h") | Some("help") => {
            println!("{USAGE}");
            Ok(())
        }
        // Bare (plugin check) and `sidecar` (installed sidecar_args) alike.
        _ => sidecar(),
    }
}
