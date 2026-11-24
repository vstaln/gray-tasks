//! gray-gsd — the Get Shit Done spec → plan → tasks loop as gray commands.
//!
//! The loop is condensed to one `/gsd` command and one `gsd_task` tool,
//! with state as plain markdown under `.gsd/` in the project (spec.md,
//! plan.md, tasks.md checkboxes).
//!
//! `/gsd <goal>` returns `{prompt}` so the agent runs the loop itself;
//! `/gsd status`, `/gsd reset` return `{text}`.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

pub fn manifest_gsd() -> Value {
    json!({
        "name": "gsd",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "1.1",
        "tools": [{
            "name": "gsd_task",
            "description": "Manage the .gsd/tasks.md checklist for the current project. \
                Actions: \"list\" (default) renders the checklist; \"done\" marks task \
                `index` (1-based, in file order) as `- [x]`; \"add\" appends `- [ ] <text>`; \
                \"undo\" flips `index` back to `- [ ]`. Only mark done after verification.",
            "parameters": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "done", "undo", "add"],
                        "description": "Default: list." },
                    "index": { "type": "integer", "description": "1-based task number for done/undo." },
                    "text": { "type": "string", "description": "Task text for add." },
                    "dir": { "type": "string", "description": "Project dir override; default: sidecar cwd." }
                }
            }
        }],
        "commands": ["/gsd"],
        "command_descriptions": {"/gsd": "run the spec, plan, tasks loop on a goal"},
    })
}

fn gsd_dir(base: &Path) -> PathBuf {
    base.join(".gsd")
}

fn session_dir(session: Option<&Value>) -> PathBuf {
    session
        .and_then(|s| s.get("cwd"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Parse tasks.md into (line_index_in_file, checked, text) per checkbox line.
fn parse_tasks(content: &str) -> Vec<(usize, bool, String)> {
    let mut out = Vec::new();
    for (i, line) in content.lines().enumerate() {
        let t = line.trim_start();
        for (prefix, checked) in [
            ("- [ ]", false),
            ("- [x]", true),
            ("- [X]", true),
            ("* [ ]", false),
            ("* [x]", true),
            ("* [X]", true),
        ] {
            if let Some(rest) = t.strip_prefix(prefix) {
                out.push((i, checked, rest.trim().to_string()));
                break;
            }
        }
    }
    out
}

fn render_tasks(content: &str) -> String {
    let tasks = parse_tasks(content);
    if tasks.is_empty() {
        return "tasks.md has no checkbox lines (`- [ ] task`)".into();
    }
    let done = tasks.iter().filter(|t| t.1).count();
    let mut s = format!("{done}/{} tasks done\n", tasks.len());
    for (n, (_, checked, text)) in tasks.iter().enumerate() {
        s.push_str(&format!(
            "{}. [{}] {text}\n",
            n + 1,
            if *checked { "x" } else { " " }
        ));
    }
    s.trim_end().to_string()
}

fn call_tool(name: &str, args: &Value) -> Result<String, String> {
    match name {
        "gsd_task" => gsd_task(args),
        other => Err(format!("unknown tool: {other}")),
    }
}

fn gsd_task(args: &Value) -> Result<String, String> {
    let base = args
        .get("dir")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let file = gsd_dir(&base).join("tasks.md");
    let action = args.get("action").and_then(Value::as_str).unwrap_or("list");
    match action {
        "add" => {
            let text = args
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            if text.is_empty() {
                return Err("add needs `text`".into());
            }
            std::fs::create_dir_all(file.parent().unwrap())
                .map_err(|e| format!("couldn't create .gsd: {e}"))?;
            let mut content = std::fs::read_to_string(&file).unwrap_or_else(|_| "# Tasks\n".into());
            if !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str(&format!("- [ ] {text}\n"));
            std::fs::write(&file, &content).map_err(|e| format!("couldn't write tasks.md: {e}"))?;
            Ok(render_tasks(&content))
        }
        "list" => match std::fs::read_to_string(&file) {
            Ok(content) => Ok(render_tasks(&content)),
            Err(_) => Err("no .gsd/tasks.md here — run /gsd <goal> first".into()),
        },
        "done" | "undo" => {
            let index = args.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            if index == 0 {
                return Err(format!("{action} needs a 1-based `index`"));
            }
            let content = std::fs::read_to_string(&file)
                .map_err(|_| "no .gsd/tasks.md here — run /gsd <goal> first".to_string())?;
            let tasks = parse_tasks(&content);
            let Some(&(line_idx, _, _)) = tasks.get(index - 1) else {
                return Err(format!("no task #{index} ({} tasks)", tasks.len()));
            };
            let mut lines: Vec<String> = content.lines().map(str::to_string).collect();
            let target = if action == "done" { "[x]" } else { "[ ]" };
            let l = &mut lines[line_idx];
            for pat in ["[ ]", "[x]", "[X]"] {
                if let Some(pos) = l.find(pat) {
                    l.replace_range(pos..pos + 3, target);
                    break;
                }
            }
            let new = lines.join("\n") + "\n";
            std::fs::write(&file, &new).map_err(|e| format!("couldn't write tasks.md: {e}"))?;
            Ok(render_tasks(&new))
        }
        other => Err(format!("unknown action: {other} (list|done|undo|add)")),
    }
}

const LOOP_PROMPT_TAIL: &str = "\
State lives in `.gsd/` as plain markdown files committed with the code.

1. SPEC — write `.gsd/spec.md`. Lock down WHAT and WHY before HOW: falsifiable \
requirements, explicit non-goals/boundaries, and acceptance criteria a verifier \
could check. If the goal is ambiguous, ask the user at most a few sharp \
questions first, then write the spec.
2. PLAN — write `.gsd/plan.md`: ordered steps, each with its verification \
(test command, build check, or manual check).
3. TASKS — write `.gsd/tasks.md`: a flat `- [ ]` checkbox list, each task \
small enough to finish and verify in one pass. It is the source of truth for \
progress — keep it in sync.
4. EXECUTE — work tasks top to bottom. For each: implement it, run its \
verification from plan.md, and only then flip it to `- [x]` via the \
`gsd_task` tool. If verification fails, fix or split the task — never mark \
unverified work done. If scope shifts, update spec.md/plan.md/tasks.md \
first, then continue.
5. DONE — when every box is checked, run the acceptance criteria from \
spec.md end to end and report the result.";

fn gsd_prompt(goal: &str, spec_only: bool) -> String {
    if spec_only {
        format!(
            "You are running the GSD spec loop for this repository.\n\nGoal: {goal}\n\n\
            Write `.gsd/spec.md` (falsifiable requirements, explicit non-goals, acceptance \
            criteria — ask the user a few sharp questions first if the goal is ambiguous), \
            `.gsd/plan.md` (ordered steps, each with its verification), and `.gsd/tasks.md` \
            (`- [ ]` checkbox list). Then STOP — do not execute. Report what you wrote."
        )
    } else {
        format!(
            "You are running the GSD loop for this repository.\n\nGoal: {goal}\n\n{LOOP_PROMPT_TAIL}"
        )
    }
}

fn execute_prompt(dir: &Path) -> Result<String, String> {
    let tasks = gsd_dir(dir).join("tasks.md");
    if !tasks.exists() {
        return Err("no .gsd/tasks.md — run /gsd <goal> first".into());
    }
    Ok(
        "Resume the GSD loop in this repository. Read `.gsd/spec.md`, `.gsd/plan.md`, and \
        `.gsd/tasks.md` first. Then execute each unchecked `- [ ]` task in order: implement, \
        run its verification from plan.md, mark it `- [x]` via the `gsd_task` tool only after \
        it passes. Never mark unverified work done. When every box is checked, run the \
        acceptance criteria from spec.md and report."
            .to_string(),
    )
}

fn status_text(dir: &Path) -> String {
    let g = gsd_dir(dir);
    let mut s = String::new();
    for f in ["spec.md", "plan.md"] {
        s.push_str(&format!(
            ".gsd/{f}: {}\n",
            if g.join(f).exists() {
                "present"
            } else {
                "missing"
            }
        ));
    }
    match std::fs::read_to_string(g.join("tasks.md")) {
        Ok(content) => s.push_str(&render_tasks(&content)),
        Err(_) => s.push_str(".gsd/tasks.md: missing — run /gsd <goal>"),
    }
    s
}

fn run_command(argv: &[&str], session: Option<&Value>) -> Value {
    let dir = session_dir(session);
    match argv.first().copied() {
        None => json!({ "text": format!(
            "gray-gsd {} — spec→plan→tasks loop, state in .gsd/\n\
            /gsd <goal>  spec+plan+tasks, then execute with verification\n\
            /gsd spec-only <goal>  write .gsd/*.md, stop before executing\n\
            /gsd execute  resume unchecked tasks\n\
            /gsd status · /gsd reset",
            env!("CARGO_PKG_VERSION")) }),
        Some("status") => json!({ "text": status_text(&dir) }),
        Some("reset") => {
            let g = gsd_dir(&dir);
            let mut removed = 0;
            for f in ["spec.md", "plan.md", "tasks.md"] {
                if std::fs::remove_file(g.join(f)).is_ok() {
                    removed += 1;
                }
            }
            let _ = std::fs::remove_dir(&g);
            json!({ "text": format!("reset .gsd/ ({removed} files removed)") })
        }
        Some("execute") => match execute_prompt(&dir) {
            Ok(p) => json!({ "prompt": p }),
            Err(t) => json!({ "text": t }),
        },
        Some("spec-only") => {
            let goal = argv[1..].join(" ");
            if goal.trim().is_empty() {
                return json!({ "text": "usage: /gsd spec-only <goal>" });
            }
            json!({ "prompt": gsd_prompt(&goal, true) })
        }
        Some(_) => json!({ "prompt": gsd_prompt(&argv.join(" "), false) }),
    }
}

/// One request → `Some(reply)`, or `None` for notifications. The bool asks
/// the loop to exit after writing the reply.
pub fn handle_gsd(req: &Value) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest_gsd(),
        "tool/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("args").cloned().unwrap_or(Value::Null);
            match call_tool(name, &args) {
                Ok(text) => json!({ "content": text }),
                Err(text) => json!({ "content": text, "is_error": true }),
            }
        }
        "command/run" => {
            let argv: Vec<&str> = params
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            run_command(&argv, params.get("session"))
        }
        "plugin/shutdown" => return (Some(json!({ "id": id, "result": {} })), true),
        _ => {
            let error = json!({ "code": -32601, "message": "method not found" });
            return (Some(json!({ "id": id, "error": error })), false);
        }
    };
    (Some(json!({ "id": id, "result": result })), false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(method: &str, params: Value) -> Value {
        handle_gsd(&json!({ "id": 1, "method": method, "params": params }))
            .0
            .unwrap()
    }

    #[test]
    fn manifest_has_task_tool_and_command() {
        let m = call("plugin/manifest", Value::Null)["result"].clone();
        assert_eq!(m["name"], "gsd");
        assert_eq!(m["commands"], json!(["/gsd"]));
        assert_eq!(m["tools"][0]["name"], "gsd_task");
    }

    #[test]
    fn goal_returns_prompt_mentioning_gsd_files() {
        let r = call(
            "command/run",
            json!({
                "name": "/gsd", "argv": ["add", "oauth", "login"],
                "session": {"id": "s", "cwd": "/tmp/x"}
            }),
        );
        let p = r["result"]["prompt"].as_str().unwrap();
        assert!(p.contains("add oauth login"));
        assert!(p.contains(".gsd/spec.md") && p.contains(".gsd/tasks.md"));
    }

    #[test]
    fn spec_only_stops_before_execute() {
        let r = call(
            "command/run",
            json!({
                "name": "/gsd", "argv": ["spec-only", "x"], "session": {"cwd": "/tmp"}
            }),
        );
        let p = r["result"]["prompt"].as_str().unwrap();
        assert!(p.contains("STOP"));
    }

    #[test]
    fn execute_without_state_is_text_not_prompt() {
        let r = call(
            "command/run",
            json!({
                "name": "/gsd", "argv": ["execute"], "session": {"cwd": "/nonexistent-gsd-dir"}
            }),
        );
        assert!(
            r["result"]["text"]
                .as_str()
                .unwrap()
                .contains("no .gsd/tasks.md")
        );
        assert!(r["result"].get("prompt").is_none());
    }

    #[test]
    fn task_tool_roundtrip() {
        let dir = std::env::temp_dir().join(format!("gsd-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".gsd")).unwrap();
        std::fs::write(
            dir.join(".gsd/tasks.md"),
            "# Tasks\n- [ ] first\n- [ ] second\n",
        )
        .unwrap();
        let d = dir.to_str().unwrap();
        let r = call(
            "tool/call",
            json!({"name":"gsd_task","args":{"action":"done","index":2,"dir":d}}),
        );
        assert!(
            r["result"]["content"]
                .as_str()
                .unwrap()
                .contains("2. [x] second")
        );
        let r = call(
            "tool/call",
            json!({"name":"gsd_task","args":{"action":"undo","index":2,"dir":d}}),
        );
        assert!(
            r["result"]["content"]
                .as_str()
                .unwrap()
                .contains("0/2 tasks done")
        );
        let r = call(
            "tool/call",
            json!({"name":"gsd_task","args":{"action":"add","text":"third","dir":d}}),
        );
        assert!(
            r["result"]["content"]
                .as_str()
                .unwrap()
                .contains("3. [ ] third")
        );
        let r = call(
            "tool/call",
            json!({"name":"gsd_task","args":{"action":"done","index":9,"dir":d}}),
        );
        assert_eq!(r["result"]["is_error"], true);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn notifications_are_silent_and_dont_exit() {
        let (reply, exit) = handle_gsd(&json!({
            "method": "event/notify", "params": {"type": "turn_end"}
        }));
        assert!(reply.is_none() && !exit);
    }
}
