//! gray-tasks — one sidecar for task tracking.
//!
//! Merges gray-todo, gray-kanban and gray-goal. Each source lives nearly
//! verbatim in `mods/` with its own state files (`~/.gray/todos/`,
//! `~/.gray/kanban/`, `~/.gray/goals/`) and slash commands — only the wire
//! plumbing is shared.
//!
//! Routing:
//! - `tool/call`          → `todo` / `kanban` / `goal` by `params.name`.
//! - `command/run`        → `/todos` / `/kanban` / `/goal` by `params.name`;
//!   `/tasks` is the merged overview.
//! - `prompt/context`     → goal (active goal + next item).
//! - `agent/before_start` → goal (completion nudge).
//! - `host/ask`           → one shared round-trip channel for todo + kanban
//!   pickers (both declare `host.ask`).
//!
//! argv: `manifest` prints the merged manifest; `sidecar` / bare-on-a-pipe
//! runs the wire loop; anything else is gray-goal's CLI surface (`gray tasks
//! …`, and the detached `audit-run` child that `/goal audit` spawns via
//! `current_exe`).

use std::collections::HashMap;
use std::io::{BufRead, IsTerminal, Write};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use serde_json::{Value, json};

#[path = "mods/goal/mod.rs"]
mod goal;
#[path = "mods/kanban.rs"]
mod kanban;
#[path = "mods/todo.rs"]
mod todo;

/// Same type as each module's `Ask` alias: the host/ask round-trip.
type Ask<'a> = dyn FnMut(&Value) -> Option<Value> + 'a;
type Pending = Arc<Mutex<HashMap<String, mpsc::Sender<Value>>>>;
/// Internal cap on host/ask waits — under the host's outer TTL (as in todo/kanban).
const ASK_TTL: Duration = Duration::from_secs(300);

fn module_tools(m: Value) -> Vec<Value> {
    m.get("tools").and_then(Value::as_array).cloned().unwrap_or_default()
}

fn manifest() -> Value {
    let mut tools = Vec::new();
    for m in [todo::manifest_todo(), kanban::manifest_kanban(), goal::manifest_goal()] {
        tools.extend(module_tools(m));
    }
    json!({
        "name": "tasks",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "1.1",
        "sidecar_args": ["sidecar"],
        "tools": tools,
        "commands": ["/tasks", "/todos", "/kanban", "/goal"],
        "hooks": ["prompt/context", "agent/before_start"],
        "capabilities": ["host.ask"],
        "completion": goal::COMPLETION,
    })
}

#[derive(Clone, Copy)]
enum Owner {
    Todo,
    Kanban,
    Goal,
}

fn tool_owner(name: &str) -> Option<Owner> {
    match name {
        "todo" => Some(Owner::Todo),
        "kanban" => Some(Owner::Kanban),
        goal::TOOL_NAME => Some(Owner::Goal),
        _ => None,
    }
}

fn command_owner(name: &str) -> Option<Owner> {
    match name {
        "/todos" => Some(Owner::Todo),
        "/kanban" => Some(Owner::Kanban),
        "/goal" => Some(Owner::Goal),
        _ => None,
    }
}

fn dispatch(owner: Owner, req: &Value, ask: &mut Ask) -> (Option<Value>, bool) {
    match owner {
        Owner::Todo => todo::handle_todo(req, ask),
        Owner::Kanban => kanban::handle_kanban(req, ask),
        Owner::Goal => goal::wire::handle_goal(req),
    }
}

/// `/tasks` — merged-plugin overview.
fn tasks_command() -> String {
    format!(
        "gray-tasks {} — one sidecar, three trackers:\n\
         \x20 /todos    per-project todo list (tool: todo)       ~/.gray/todos/\n\
         \x20 /kanban   per-project kanban board (tool: kanban)  ~/.gray/kanban/\n\
         \x20 /goal     persistent goals + independent auditor (tool: goal)  ~/.gray/goals/",
        env!("CARGO_PKG_VERSION")
    )
}

fn handle(req: &Value, ask: &mut Ask) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest(),
        "prompt/context" | "agent/before_start" => {
            let (reply, _) = goal::wire::handle_goal(req);
            return (reply, false);
        }
        "tool/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            match tool_owner(name) {
                Some(owner) => {
                    let (reply, _) = dispatch(owner, req, ask);
                    return (reply, false);
                }
                None => json!({ "content": format!("unknown tool: {name}"), "is_error": true }),
            }
        }
        "command/run" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            if name == "/tasks" {
                json!({ "text": tasks_command() })
            } else if let Some(owner) = command_owner(name) {
                let (reply, _) = dispatch(owner, req, ask);
                return (reply, false);
            } else {
                json!({ "text": format!("unknown command {name} — try /tasks") })
            }
        }
        "plugin/shutdown" => return (Some(json!({ "id": id, "result": {} })), true),
        _ => {
            let error = json!({ "code": -32601, "message": format!("method not found: {method}") });
            return (Some(json!({ "id": id, "error": error })), false);
        }
    };
    (Some(json!({ "id": id, "result": result })), false)
}

/// Wire loop — the todo/kanban shape: a stdin reader thread routes host
/// answers (string id, no method) to `pending` and requests to the work
/// channel, so a `host/ask` can block mid-handler.
fn sidecar() -> std::io::Result<()> {
    let stdout = Arc::new(Mutex::new(std::io::stdout()));
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let counter = Mutex::new(0u64);

    let (work_tx, work_rx) = mpsc::channel::<Value>();
    let reader_pending = pending.clone();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            if let Some(id) = v.get("id").and_then(Value::as_str)
                && v.get("method").is_none()
            {
                if let Some(tx) = reader_pending.lock().expect("pending").remove(id) {
                    let _ = tx.send(v.get("result").cloned().unwrap_or(Value::Null));
                }
                continue;
            }
            let shutdown = v.get("method").and_then(Value::as_str) == Some("plugin/shutdown");
            if (v.get("id").is_some() || shutdown) && work_tx.send(v).is_err() {
                break;
            }
        }
    });

    let mut ask = |params: &Value| -> Option<Value> {
        let id = {
            let mut n = counter.lock().expect("id counter");
            *n += 1;
            format!("q{n}")
        };
        let (tx, rx) = mpsc::channel();
        pending.lock().expect("pending").insert(id.clone(), tx);
        {
            let req = json!({"id": id, "method": "host/ask", "params": params});
            let mut o = stdout.lock().expect("stdout");
            let _ = writeln!(o, "{req}");
            let _ = o.flush();
        }
        let reply = rx.recv_timeout(ASK_TTL).ok();
        pending.lock().expect("pending").remove(&id);
        reply
    };

    for req in work_rx {
        let (reply, exit) = handle(&req, &mut ask);
        if let Some(reply) = reply {
            let mut o = stdout.lock().expect("stdout");
            let _ = writeln!(o, "{reply}");
            let _ = o.flush();
        }
        if exit {
            break;
        }
    }
    Ok(())
}

fn main() -> std::io::Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("manifest") => {
            println!("{}", manifest());
            Ok(())
        }
        Some("sidecar") => sidecar(),
        None => {
            if std::io::stdin().is_terminal() {
                eprintln!(
                    "tasks is a plugin sidecar (todo + kanban + goal) — install with `gray plugin install tasks`; `tasks <goal verb>` drives the goal CLI"
                );
                return Ok(());
            }
            sidecar()
        }
        // Everything else is gray-goal's CLI (incl. the detached `audit-run`).
        Some(_) => {
            goal::wire::main_goal();
            Ok(())
        }
    }
}
