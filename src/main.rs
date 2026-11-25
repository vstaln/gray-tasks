//!
//! - `tool/before`        → plan (read-only gate).
//!
//! argv: `manifest` prints the merged manifest; `sidecar` / bare-on-a-pipe

use std::collections::HashMap;
use std::io::{BufRead, IsTerminal, Write};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use serde_json::{Value, json};

mod parts;

#[allow(dead_code)]
#[path = "mods/goal/mod.rs"]
mod goal;
#[allow(dead_code)]
#[path = "mods/gsd.rs"]
mod gsd;
#[allow(dead_code)]
#[path = "mods/kanban.rs"]
mod kanban;
#[allow(dead_code)]
#[path = "mods/loop.rs"]
mod r#loop;
#[allow(dead_code)]
#[path = "mods/plan.rs"]
mod plan;
#[allow(dead_code)]
#[path = "mods/plannotator.rs"]
mod plannotator;
#[allow(dead_code)]
#[path = "mods/todo.rs"]
mod todo;
#[allow(dead_code)]
#[path = "mods/workflows.rs"]
mod workflows;

const PARTS: &[parts::Part] = &[
    parts::Part {
        name: "todo",
        default: true,
        description: "per-project todo list: the todo tool and /todos",
    },
    parts::Part {
        name: "kanban",
        default: true,
        description: "per-project kanban board: /kanban",
    },
    parts::Part {
        name: "goal",
        default: true,
        description: "persistent goal checklists with an independent auditor",
    },
    parts::Part {
        name: "loop",
        default: true,
        description: "scheduled and self-paced /loop iterations until a goal is done",
    },
    parts::Part {
        name: "gsd",
        default: true,
        description: "spec, plan, tasks loop in .gsd/: /gsd and the gsd_task tool",
    },
    parts::Part {
        name: "workflows",
        default: true,
        description: "batch multi-agent workflow specs and /deep-research",
    },
    parts::Part {
        name: "plan",
        default: true,
        description: "read-only plan mode: /plan",
    },
    parts::Part {
        name: "plannotator",
        default: true,
        description: "annotated plan review: /plan-review and /plan-accept",
    },
];

/// Same type as each module's `Ask` alias: the host/ask round-trip.
type Ask<'a> = dyn FnMut(&Value) -> Option<Value> + 'a;
type Pending = Arc<Mutex<HashMap<String, mpsc::Sender<Value>>>>;
/// Internal cap on host/ask waits — under the host's outer TTL (as in todo/kanban).
const ASK_TTL: Duration = Duration::from_secs(290);

const META: &str = "/tasks";

fn union_strs(acc: &mut Vec<Value>, m: &Value, key: &str) {
    for v in m.get(key).and_then(Value::as_array).into_iter().flatten() {
        if !acc.contains(v) {
            acc.push(v.clone());
        }
    }
}

fn union_completion(acc: &mut Vec<Value>, m: &Value) {
    let word = |v: &Value| {
        v.as_str()
            .or_else(|| v.get("arg").and_then(Value::as_str))
            .map(str::to_string)
    };
    for v in m
        .get("completion")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(w) = word(v)
            && !acc
                .iter()
                .any(|seen| word(seen).as_deref() == Some(w.as_str()))
        {
            acc.push(v.clone());
        }
    }
}

fn union_tools(acc: &mut Vec<Value>, m: &Value) {
    for t in m
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let name = t.get("name");
        if !acc.iter().any(|x| x.get("name") == name) {
            acc.push(t.clone());
        }
    }
}

fn union_map(acc: &mut serde_json::Map<String, Value>, m: &Value, key: &str) {
    if let Some(map) = m.get(key).and_then(Value::as_object) {
        for (k, v) in map {
            acc.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
}

fn manifest(parts: &parts::Parts) -> Value {
    let mut tools: Vec<Value> = Vec::new();
    let mut commands: Vec<Value> = vec![json!(META)];
    let mut hooks: Vec<Value> = Vec::new();
    let mut subcommands: Vec<Value> = Vec::new();
    let mut completion: Vec<Value> = Vec::new();
    let mut capabilities: Vec<Value> = Vec::new();
    let mut descriptions = serde_json::Map::new();
    let enabled: [(&str, Value); 8] = [
        ("todo", todo::manifest_todo()),
        ("kanban", kanban::manifest_kanban()),
        ("goal", goal::manifest_goal()),
        ("loop", r#loop::manifest_loop()),
        ("gsd", gsd::manifest_gsd()),
        ("workflows", workflows::manifest_workflows()),
        ("plan", plan::manifest_plan()),
        ("plannotator", plannotator::manifest_plannotator()),
    ];
    for (name, m) in &enabled {
        if !parts.on(name) {
            continue;
        }
        union_tools(&mut tools, m);
        union_strs(&mut commands, m, "commands");
        union_strs(&mut hooks, m, "hooks");
        union_strs(&mut subcommands, m, "subcommands");
        union_completion(&mut completion, m);
        union_strs(&mut capabilities, m, "capabilities");
        union_map(&mut descriptions, m, "command_descriptions");
    }
    descriptions.insert(META.into(), json!("list the task trackers"));
    json!({
        "name": "tasks",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "2.0",
        "description": "todos, kanban, goals, loops and multi-step drivers",
        "sidecar_args": ["sidecar"],
        "tools": tools,
        "commands": commands,
        "command_descriptions": descriptions,
        "subcommands": subcommands,
        "completion": completion,
        "hooks": hooks,
        "capabilities": capabilities,
        "widget": parts.on("loop"),
        "parts": parts.manifest(PARTS),
    })
}

#[derive(Clone, Copy)]
enum Owner {
    Todo,
    Kanban,
    Goal,
    Loop,
    Gsd,
    Workflows,
    Plan,
    Plannotator,
}

impl Owner {
    fn part(self) -> &'static str {
        match self {
            Owner::Todo => "todo",
            Owner::Kanban => "kanban",
            Owner::Goal => "goal",
            Owner::Loop => "loop",
            Owner::Gsd => "gsd",
            Owner::Workflows => "workflows",
            Owner::Plan => "plan",
            Owner::Plannotator => "plannotator",
        }
    }
}

fn tool_owner(name: &str) -> Option<Owner> {
    match name {
        "todo" => Some(Owner::Todo),
        "kanban" => Some(Owner::Kanban),
        goal::TOOL_NAME => Some(Owner::Goal),
        "loop_done" | "loop_wakeup" | "loop_task" | "loop_learn" => Some(Owner::Loop),
        "gsd_task" => Some(Owner::Gsd),
        _ => None,
    }
}

fn command_owner(name: &str) -> Option<Owner> {
    match name {
        "/todos" => Some(Owner::Todo),
        "/kanban" => Some(Owner::Kanban),
        "/goal" => Some(Owner::Goal),
        "/loop" => Some(Owner::Loop),
        "/gsd" => Some(Owner::Gsd),
        "/workflow" | "/workflows" | "workflows" | "/deep-research" => Some(Owner::Workflows),
        "/plan" => Some(Owner::Plan),
        "/plan-review" | "/plan-accept" => Some(Owner::Plannotator),
        _ => None,
    }
}

fn collect_text(acc: &mut Vec<String>, reply: Option<Value>) {
    let Some(reply) = reply else { return };
    let Some(result) = reply.get("result") else {
        return;
    };
    for key in ["text", "message", "append"] {
        if let Some(t) = result.get(key).and_then(Value::as_str) {
            let t = t.trim_end();
            if !t.is_empty() && !acc.iter().any(|seen| seen == t) {
                acc.push(t.to_string());
            }
        }
    }
}

fn joined(texts: Vec<String>) -> Value {
    if texts.is_empty() {
        json!({})
    } else {
        json!({ "text": texts.join("\n\n") })
    }
}

fn dispatch(owner: Owner, req: &Value, ask: &mut Ask, link: &r#loop::HostLink) -> Option<Value> {
    match owner {
        Owner::Todo => todo::handle_todo(req, ask).0,
        Owner::Kanban => kanban::handle_kanban(req, ask).0,
        Owner::Goal => goal::wire::handle_goal(req).0,
        Owner::Loop => r#loop::handle_loop(req, Some(link)).0,
        Owner::Gsd => gsd::handle_gsd(req).0,
        Owner::Plan => plan::handle_plan(req).0,
        Owner::Workflows => workflows::handle_workflows(req),
        Owner::Plannotator => plannotator::handle_plannotator(req),
    }
}

fn tasks_command(parts: &parts::Parts) -> String {
    let rows: [(&str, &str); 8] = [
        ("todo", "/todos    per-project todo list (tool: todo)"),
        (
            "kanban",
            "/kanban   per-project kanban board (tool: kanban)",
        ),
        (
            "goal",
            "/goal     persistent goals + independent auditor (tool: goal)",
        ),
        (
            "loop",
            "/loop     scheduled and self-paced loops (loop_* tools)",
        ),
        ("gsd", "/gsd      spec, plan, tasks loop (tool: gsd_task)"),
        (
            "workflows",
            "/workflow batch multi-agent runs; /workflows, /deep-research",
        ),
        ("plan", "/plan     read-only planning mode"),
        (
            "plannotator",
            "/plan-review, /plan-accept  annotated plan review",
        ),
    ];
    let mut out = format!(
        "gray-tasks {} — one sidecar, eight parts:",
        env!("CARGO_PKG_VERSION")
    );
    for (name, row) in rows {
        if parts.on(name) {
            out.push_str("\n  ");
            out.push_str(row);
        }
    }
    out
}

fn handle(
    req: &Value,
    ask: &mut Ask,
    link: &r#loop::HostLink,
    parts: &parts::Parts,
) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        if (method == "event/notify" || method == "plugin/shutdown") && parts.on("loop") {
            let _ = r#loop::handle_loop(req, Some(link));
        }
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest(parts),
        "prompt/context" => {
            let mut texts = Vec::new();
            if parts.on("goal") {
                collect_text(&mut texts, goal::wire::handle_goal(req).0);
            }
            if parts.on("workflows") {
                collect_text(&mut texts, workflows::handle_workflows(req));
            }
            if parts.on("plannotator") {
                collect_text(&mut texts, plannotator::handle_plannotator(req));
            }
            joined(texts)
        }
        "agent/before_start" => {
            let mut texts = Vec::new();
            if parts.on("goal") {
                collect_text(&mut texts, goal::wire::handle_goal(req).0);
            }
            if parts.on("loop") {
                collect_text(&mut texts, r#loop::handle_loop(req, Some(link)).0);
            }
            if parts.on("plan") {
                collect_text(&mut texts, plan::handle_plan(req).0);
            }
            joined(texts)
        }
        "tool/before" => {
            if parts.on("plan") {
                let (reply, _) = plan::handle_plan(req);
                return (reply, false);
            }
            json!({ "decision": "allow" })
        }
        "tool/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            match tool_owner(name) {
                Some(owner) if parts.on(owner.part()) => {
                    let (reply, _) = match dispatch(owner, req, ask, link) {
                        Some(reply) => (Some(reply), false),
                        None => (None, false),
                    };
                    return (reply, false);
                }
                _ => json!({ "content": format!("unknown tool: {name}"), "is_error": true }),
            }
        }
        "command/run" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            if name == META {
                json!({ "text": tasks_command(parts) })
            } else if let Some(owner) = command_owner(name)
                && parts.on(owner.part())
            {
                let (reply, _) =
                    dispatch(owner, req, ask, link).map_or((None, false), |r| (Some(r), false));
                return (reply, false);
            } else {
                json!({ "text": format!("unknown command {name} — try {META}") })
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

fn sidecar(parts: &parts::Parts) -> std::io::Result<()> {
    let stdout = Arc::new(Mutex::new(std::io::stdout()));
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let counter = Arc::new(Mutex::new(0u64));
    let link = r#loop::HostLink {
        out: stdout.clone(),
        pending: pending.clone(),
        counter: counter.clone(),
    };

    let (work_tx, work_rx) = mpsc::channel::<Value>();
    let reader_pending = pending.clone();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
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
        let (reply, exit) = handle(&req, &mut ask, &link, parts);
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

fn part_cli(part: &str, args: &[String], parts: &parts::Parts) -> std::io::Result<()> {
    match part {
        "goal" => {
            match goal::cli::args_for(args.to_vec()) {
                Ok(parsed) => match goal::cli::run(&parsed) {
                    Ok(reply) => print!("{}", reply.text),
                    Err(e) => {
                        eprintln!("{e:#}");
                        std::process::exit(1);
                    }
                },
                Err(e) => std::process::exit(if e.use_stderr() { 1 } else { 0 }),
            }
            Ok(())
        }
        "loop" => {
            std::process::exit(r#loop::loop_cli(args));
        }
        "workflows" => {
            let mut argv = vec!["gray-workflows".to_string()];
            argv.extend(args.iter().cloned());
            match workflows::main_workflows(argv) {
                Ok(()) => Ok(()),
                Err(e) => {
                    eprintln!("{e:#}");
                    std::process::exit(1);
                }
            }
        }
        "plannotator" => plannotator::main_plannotator(args),
        other => {
            eprintln!(
                "gray-tasks {other} has no CLI — it answers the sidecar wire only (tools and slash commands stay as in gray-{other})"
            );
            let _ = parts;
            Ok(())
        }
    }
}

fn usage(parts: &parts::Parts) -> String {
    let mut out = format!(
        "gray-tasks {} — one sidecar for todo, kanban, goal, loop, gsd, workflows, plan and plannotator.\n\
         Install with `gray plugin install tasks`; parts toggle in ~/.gray/plugins/parts.json.\n\n\
         Parts:",
        env!("CARGO_PKG_VERSION")
    );
    for p in PARTS {
        out.push_str(&format!(
            "\n  {:<12} {}  {}",
            p.name,
            if parts.on(p.name) { "on " } else { "off" },
            p.description
        ));
    }
    out.push_str(
        "\n\nUsage: gray tasks <part> <args…>   (goal, loop, workflows, plannotator)\n\
         \x20      gray tasks manifest        print the merged manifest\n\
         \x20      gray tasks sidecar         run the sidecar (default on a pipe)",
    );
    out
}

fn main() -> std::io::Result<()> {
    let parts = parts::Parts::load("tasks", PARTS);
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("orchestrate") => args.insert(0, "workflows".to_string()),
        Some("audit-run") => args.insert(0, "goal".to_string()),
        _ => {}
    }
    match args.first().map(String::as_str) {
        Some("manifest") => {
            println!("{}", manifest(&parts));
            Ok(())
        }
        Some("sidecar") => sidecar(&parts),
        Some("widget") => {
            if parts.on("loop") {
                r#loop::widget();
            } else {
                eprintln!("the loop part is off — no widget to draw");
            }
            Ok(())
        }
        Some(part) if PARTS.iter().any(|p| p.name == part) => {
            if parts.on(part) {
                part_cli(part, &args[1..], &parts)
            } else {
                eprintln!(
                    "the {part} part is off — enable it with `gray plugin enable tasks/{part}`"
                );
                Ok(())
            }
        }
        None if std::io::stdin().is_terminal() => {
            println!("{}", usage(&parts));
            Ok(())
        }
        None => sidecar(&parts),
        Some("-h" | "--help" | "help") => {
            println!("{}", usage(&parts));
            Ok(())
        }
        Some(other) => {
            eprintln!("unknown part '{other}'\n\n{}", usage(&parts));
            std::process::exit(2);
        }
    }
}
