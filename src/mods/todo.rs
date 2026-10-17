//! gray-todo — per-project todo list for the agent and the user.
//!
//! Port of pi's `todo` extension: a `todo` tool the model calls with
//! `{action: list|add|toggle|clear, text?, id?}` and a `/todos` command that
//! renders the list. Where pi kept state in session entries (so branching
//! rewound it), gray sidecars have no session-entry store — state persists
//! per project directory at `~/.gray/todos/<sha1-of-cwd>.json`, written after
//! every mutation and stable across sessions in the same cwd.
//!
//! `/todos` is interactive when the host grants `host.ask`: it opens a
//! picker ("Toggle which todo?") whose options are every todo plus "just
//! show", and toggles the picked id through the same path as the tool's
//! `toggle` action. With no ask channel it falls back to the plain render.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use serde_json::{Value, json};

pub fn manifest_todo() -> Value {
    json!({
        "name": "todo",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "1.1",
        "tools": [{
            "name": "todo",
            "description": "Manage a todo list. Actions: list, add (text), toggle (id), clear",
            "parameters": {
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["list", "add", "toggle", "clear"],
                        "description": "list | add | toggle | clear"
                    },
                    "text": { "type": "string", "description": "Todo text (for add)" },
                    "id": { "type": "number", "description": "Todo ID (for toggle)" }
                },
                "required": ["action"]
            }
        }],
        "commands": ["/todos"],
        "capabilities": ["host.ask"],
    })
}

/// host/ask round-trip signature — the real one lives in `main` (writes the
/// request to stdout, blocks on a pending channel); tests inject canned
/// answers. Params are the full `host/ask` params object; returns the reply's
/// `result`, or None on timeout.
pub type Ask<'a> = dyn FnMut(&Value) -> Option<Value> + 'a;

type Pending = Arc<Mutex<HashMap<String, mpsc::Sender<Value>>>>;
/// Internal cap on host/ask waits — under the host's outer TTL.
const ASK_TTL: Duration = Duration::from_secs(300);
const Q_TOGGLE: &str = "todo-toggle";
const LABEL_JUST_SHOW: &str = "just show";

/// First picked label out of a host/ask reply — None on absent/timeout/empty.
fn picked_label(result: Option<&Value>, qid: &str) -> Option<String> {
    result?
        .pointer(&format!("/answers/{qid}/answers/0"))?
        .as_str()
        .map(String::from)
        .filter(|s| !s.is_empty())
}

/// One todo item.
#[derive(Clone)]
struct Todo {
    id: i64,
    text: String,
    done: bool,
}

#[derive(Default)]
struct State {
    todos: Vec<Todo>,
    next_id: i64,
}

fn state_dir() -> PathBuf {
    let home = std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".gray")))
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("todos")
}

/// `session.cwd` from a request's params, falling back to the process cwd.
fn session_cwd(params: &Value) -> String {
    params
        .pointer("/session/cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| ".".into())
}

fn state_path(cwd: &str) -> PathBuf {
    state_dir().join(format!("{}.json", sha1_hex(cwd)))
}

fn load(path: &Path) -> State {
    let Ok(text) = std::fs::read_to_string(path) else {
        return State { todos: Vec::new(), next_id: 1 };
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return State { todos: Vec::new(), next_id: 1 };
    };
    let todos: Vec<Todo> = v
        .get("todos")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|t| {
                    Some(Todo {
                        id: t.get("id").and_then(Value::as_i64)?,
                        text: t.get("text").and_then(Value::as_str)?.to_string(),
                        done: t.get("done").and_then(Value::as_bool).unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let next_id = v
        .get("next_id")
        .and_then(Value::as_i64)
        .filter(|n| *n > 0)
        .unwrap_or_else(|| todos.iter().map(|t| t.id).max().unwrap_or(0) + 1);
    State { todos, next_id }
}

fn save(path: &Path, state: &State) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let todos: Vec<Value> = state
        .todos
        .iter()
        .map(|t| json!({"id": t.id, "text": t.text, "done": t.done}))
        .collect();
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json!({"todos": todos, "next_id": state.next_id}).to_string())?;
    std::fs::rename(&tmp, path)
}

/// The `todo` tool: same semantics as pi — ids auto-increment, toggle flips
/// `done`, clear empties and resets the counter.
fn call_tool(name: &str, args: &Value, cwd: &str) -> Result<String, String> {
    if name != "todo" {
        return Err(format!("unknown tool: {name}"));
    }
    let action = args.get("action").and_then(Value::as_str).unwrap_or("");
    let path = state_path(cwd);
    let mut state = load(&path);
    match action {
        "list" => Ok(if state.todos.is_empty() {
            "No todos".into()
        } else {
            state
                .todos
                .iter()
                .map(|t| format!("[{}] #{}: {}", if t.done { "x" } else { " " }, t.id, t.text))
                .collect::<Vec<_>>()
                .join("\n")
        }),
        "add" => {
            let text = args.get("text").and_then(Value::as_str).unwrap_or("");
            if text.is_empty() {
                return Err("Error: text required for add".into());
            }
            let id = state.next_id;
            state.next_id += 1;
            state.todos.push(Todo { id, text: text.to_string(), done: false });
            if let Err(e) = save(&path, &state) {
                return Err(format!("Error: could not save todos: {e}"));
            }
            Ok(format!("Added todo #{id}: {text}"))
        }
        "toggle" => {
            let Some(id) = args.get("id").and_then(Value::as_i64) else {
                return Err("Error: id required for toggle".into());
            };
            let Some(todo) = state.todos.iter_mut().find(|t| t.id == id) else {
                return Err(format!("Todo #{id} not found"));
            };
            todo.done = !todo.done;
            let done = todo.done;
            if let Err(e) = save(&path, &state) {
                return Err(format!("Error: could not save todos: {e}"));
            }
            Ok(format!("Todo #{id} {}", if done { "completed" } else { "uncompleted" }))
        }
        "clear" => {
            let count = state.todos.len();
            state.todos.clear();
            state.next_id = 1;
            if let Err(e) = save(&path, &state) {
                return Err(format!("Error: could not save todos: {e}"));
            }
            Ok(format!("Cleared {count} todos"))
        }
        other => Err(format!("Unknown action: {other}")),
    }
}

/// Render the list: done/total summary plus one checkbox line per todo.
fn render_list(state: &State) -> String {
    if state.todos.is_empty() {
        return "No todos yet. Ask the agent to add some!".into();
    }
    let done = state.todos.iter().filter(|t| t.done).count();
    let mut out = format!("{}/{} completed\n", done, state.todos.len());
    for t in &state.todos {
        out.push_str(&format!(
            "{} #{} {}\n",
            if t.done { "✓" } else { "○" },
            t.id,
            t.text
        ));
    }
    out.trim_end().to_string()
}

fn todo_label(t: &Todo) -> String {
    format!("#{} {}", t.id, t.text)
}

/// `/todos` — interactive picker when host/ask works: toggle the picked todo
/// (via the tool's `toggle` path) or pick "just show". Any ask failure —
/// ungranted capability, timeout, empty answer — falls back to the render.
fn run_command(argv: &[&str], cwd: &str, ask: &mut Ask) -> String {
    if !argv.is_empty() {
        return "usage: /todos".into();
    }
    let state = load(&state_path(cwd));
    if state.todos.is_empty() {
        return render_list(&state);
    }
    let mut options: Vec<Value> = state
        .todos
        .iter()
        .map(|t| {
            json!({
                "label": todo_label(t),
                "description": if t.done { "done" } else { "pending" },
            })
        })
        .collect();
    options.push(json!({
        "label": LABEL_JUST_SHOW,
        "description": "render the list, change nothing",
    }));
    let picked = ask(&json!({
        "questions": [{
            "id": Q_TOGGLE,
            "header": "Todos",
            "question": "Toggle which todo?",
            "options": options,
        }],
        "blocking": true,
    }));
    let label = picked_label(picked.as_ref(), Q_TOGGLE).unwrap_or_default();
    match state.todos.iter().find(|t| todo_label(t) == label) {
        Some(t) => {
            let id = t.id;
            match call_tool("todo", &json!({"action": "toggle", "id": id}), cwd) {
                Ok(text) => format!("{text}\n\n{}", render_list(&load(&state_path(cwd)))),
                Err(e) => e,
            }
        }
        None => render_list(&state),
    }
}

/// SHA-1 (hex) of a string — used only to derive the per-cwd state filename,
/// so a small in-file implementation keeps the crate dependency-free.
fn sha1_hex(data: &str) -> String {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut msg = data.as_bytes().to_vec();
    let bit_len = (msg.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([chunk[4 * i], chunk[4 * i + 1], chunk[4 * i + 2], chunk[4 * i + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

/// One request → `Some(reply)`, or `None` for notifications. The bool asks
/// the loop to exit after writing the reply.
pub fn handle_todo(req: &Value, ask: &mut Ask) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest_todo(),
        "tool/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("args").cloned().unwrap_or(Value::Null);
            let cwd = session_cwd(&params);
            match call_tool(name, &args, &cwd) {
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
            json!({ "text": run_command(&argv, &session_cwd(&params), ask) })
        }
        "plugin/shutdown" => return (Some(json!({ "id": id, "result": {} })), true),
        _ => {
            let error = json!({ "code": -32601, "message": "method not found" });
            return (Some(json!({ "id": id, "error": error })), false);
        }
    };
    (Some(json!({ "id": id, "result": result })), false)
}

fn main_todo() -> std::io::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest_todo());
        return Ok(());
    }
    let stdout = Arc::new(Mutex::new(std::io::stdout()));
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let counter = Mutex::new(0u64);

    // Reader thread: host answers (string id, no method) go to `pending`;
    // host requests (id + method) go to the work channel. Notifications have
    // no id and are dropped here, same as `handle` returning None.
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

    // host/ask: write the request, block on the pending channel. Errors and
    // timeouts surface as None → callers degrade to a text answer.
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
        let (reply, exit) = handle_todo(&req, &mut ask);
        if let Some(reply) = reply {
            let mut o = stdout.lock().expect("stdout");
            let _ = writeln!(o, "{reply}");
            let _ = o.flush();
        }
        if exit {
            break;
        }
    }
    // The reader thread is a detached stdin pump; returning ends the process
    // with it — nothing to join.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(method: &str, params: Value) -> Value {
        call_ask(method, params, &mut |_p: &Value| None)
    }

    fn call_ask(method: &str, params: Value, ask: &mut Ask) -> Value {
        handle_todo(&json!({ "id": 1, "method": method, "params": params }), ask)
            .0
            .unwrap()
    }

    /// Test asker: answers host/ask by picking option `i` of question 0.
    fn picking(i: usize) -> impl FnMut(&Value) -> Option<Value> {
        move |p: &Value| {
            let q = &p["questions"][0];
            let qid = q["id"].as_str().unwrap_or("q").to_string();
            let label = q["options"][i]["label"].as_str()?.to_string();
            let mut answers = serde_json::Map::new();
            answers.insert(qid, json!({"answers": [label]}));
            Some(json!({"answers": answers}))
        }
    }

    /// Run `body` with GRAY_HOME pointed at a fresh temp dir. Serialized:
    /// the env var is process-global and tests run in one process.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static CTR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn with_home(body: impl FnOnce(&Path)) {
        let _g = LOCK.lock().unwrap();
        let n = CTR.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("gray-todo-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("GRAY_HOME", &dir) };
        body(&dir);
        unsafe { std::env::remove_var("GRAY_HOME") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tool(action: &str, extra: Value, cwd: &str) -> Value {
        let mut args = json!({"action": action});
        args.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        call(
            "tool/call",
            json!({"name": "todo", "args": args, "session": {"id": "s", "cwd": cwd}}),
        )
    }

    #[test]
    fn manifest_has_todo_tool_and_todos_command() {
        let m = call("plugin/manifest", Value::Null)["result"].clone();
        assert_eq!(m["name"], "todo");
        assert_eq!(m["commands"], json!(["/todos"]));
        assert_eq!(m["tools"][0]["name"], "todo");
        assert_eq!(m["tools"][0]["parameters"]["required"], json!(["action"]));
        assert_eq!(m["capabilities"], json!(["host.ask"]));
    }

    #[test]
    fn sha1_matches_known_vector() {
        assert_eq!(sha1_hex("abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(sha1_hex(""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    }

    #[test]
    fn add_list_toggle_clear_roundtrip() {
        with_home(|_| {
            let cwd = "/tmp/project-a";
            assert!(tool("list", json!({}), cwd)["result"]["content"].as_str().unwrap().contains("No todos"));

            let r = tool("add", json!({"text": "first"}), cwd);
            assert_eq!(r["result"]["content"], "Added todo #1: first");
            let r = tool("add", json!({"text": "second"}), cwd);
            assert_eq!(r["result"]["content"], "Added todo #2: second");

            let list = tool("list", json!({}), cwd)["result"]["content"].as_str().unwrap().to_string();
            assert!(list.contains("[ ] #1: first") && list.contains("[ ] #2: second"));

            let r = tool("toggle", json!({"id": 1}), cwd);
            assert_eq!(r["result"]["content"], "Todo #1 completed");
            let r = tool("toggle", json!({"id": 1}), cwd);
            assert_eq!(r["result"]["content"], "Todo #1 uncompleted");

            // Errors are is_error results, not panics.
            assert!(tool("add", json!({}), cwd)["result"]["is_error"].as_bool().unwrap());
            assert!(tool("toggle", json!({}), cwd)["result"]["is_error"].as_bool().unwrap());
            assert!(tool("toggle", json!({"id": 99}), cwd)["result"]["is_error"].as_bool().unwrap());

            let r = tool("clear", json!({}), cwd);
            assert_eq!(r["result"]["content"], "Cleared 2 todos");
            // Ids restart after clear.
            let r = tool("add", json!({"text": "again"}), cwd);
            assert_eq!(r["result"]["content"], "Added todo #1: again");
        });
    }

    #[test]
    fn state_is_per_cwd_and_persistent() {
        with_home(|home| {
            tool("add", json!({"text": "a"}), "/tmp/proj-a");
            tool("add", json!({"text": "b"}), "/tmp/proj-b");
            let a = tool("list", json!({}), "/tmp/proj-a")["result"]["content"].as_str().unwrap().to_string();
            let b = tool("list", json!({}), "/tmp/proj-b")["result"]["content"].as_str().unwrap().to_string();
            assert!(a.contains("a") && !a.contains("b"));
            assert!(b.contains("b") && !b.contains("a"));
            // The file exists under ~/.gray/todos/<sha1>.json.
            let files: Vec<_> = std::fs::read_dir(home.join("todos")).unwrap().collect();
            assert_eq!(files.len(), 2);
        });
    }

    #[test]
    fn todos_command_renders_summary_and_lines() {
        with_home(|_| {
            let cwd = "/tmp/proj";
            tool("add", json!({"text": "one"}), cwd);
            tool("add", json!({"text": "two"}), cwd);
            tool("toggle", json!({"id": 2}), cwd);
            let r = call(
                "command/run",
                json!({"name": "/todos", "argv": [], "session": {"id": "s", "cwd": cwd}}),
            );
            let text = r["result"]["text"].as_str().unwrap();
            assert!(text.contains("1/2 completed"));
            assert!(text.contains("○ #1 one") && text.contains("✓ #2 two"));
        });
    }

    #[test]
    fn todos_command_picker_toggles_selected() {
        with_home(|_| {
            let cwd = "/tmp/proj";
            tool("add", json!({"text": "one"}), cwd);
            tool("add", json!({"text": "two"}), cwd);
            let mut ask = picking(1); // second todo
            let r = call_ask(
                "command/run",
                json!({"name": "/todos", "argv": [], "session": {"id": "s", "cwd": cwd}}),
                &mut ask,
            );
            let text = r["result"]["text"].as_str().unwrap();
            assert!(text.contains("Todo #2 completed"), "{text}");
            assert!(text.contains("✓ #2 two") && text.contains("○ #1 one"), "{text}");
        });
    }

    #[test]
    fn todos_command_picker_just_show_changes_nothing() {
        with_home(|_| {
            let cwd = "/tmp/proj";
            tool("add", json!({"text": "one"}), cwd);
            let mut ask = picking(1); // last option = "just show"
            let r = call_ask(
                "command/run",
                json!({"name": "/todos", "argv": [], "session": {"id": "s", "cwd": cwd}}),
                &mut ask,
            );
            let text = r["result"]["text"].as_str().unwrap();
            assert!(text.contains("0/1 completed") && text.contains("○ #1 one"), "{text}");
            let list = tool("list", json!({}), cwd)["result"]["content"].as_str().unwrap().to_string();
            assert!(list.contains("[ ] #1: one"));
        });
    }

    #[test]
    fn shutdown_replies_then_exits() {
        let (reply, exit) = handle_todo(&json!({ "id": 2, "method": "plugin/shutdown" }), &mut |_p: &Value| None);
        assert!(reply.is_some() && exit);
    }
}
