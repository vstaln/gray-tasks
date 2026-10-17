//! gray-kanban — a tiny per-project kanban for the agent and the user.
//!
//! One `kanban` tool
//! ({action: boards|create|add|move|done|show|rm}) plus `/kanban [board]`
//! which renders the board as text columns. Boards live under
//! `~/.gray/kanban/<cwd-hash>/<board>.json` — keyed to the session's working
//! directory so different projects get different boards.
//!
//! Columns: backlog | todo | doing | review | done. Card ids: k1, k2, …
//!
//! `/kanban move [board]` is interactive when the host grants `host.ask`:
//! step 1 picks a card, step 2 picks the target column, then the move runs
//! through the same path as the tool's `move` action. With no ask channel it
//! falls back to the board render.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use serde_json::{Value, json};

const COLS: [&str; 5] = ["backlog", "todo", "doing", "review", "done"];
const COL_WIDTH: usize = 24;

/// host/ask round-trip signature — the real one lives in `main` (writes the
/// request to stdout, blocks on a pending channel); tests inject canned
/// answers. Params are the full `host/ask` params object; returns the reply's
/// `result`, or None on timeout.
pub type Ask<'a> = dyn FnMut(&Value) -> Option<Value> + 'a;

type Pending = Arc<Mutex<HashMap<String, mpsc::Sender<Value>>>>;
/// Internal cap on host/ask waits — under the host's outer TTL.
const ASK_TTL: Duration = Duration::from_secs(300);
const Q_CARD: &str = "kanban-card";
const Q_COL: &str = "kanban-col";
const LABEL_CANCEL: &str = "cancel";

/// First picked label out of a host/ask reply — None on absent/timeout/empty.
fn picked_label(result: Option<&Value>, qid: &str) -> Option<String> {
    result?
        .pointer(&format!("/answers/{qid}/answers/0"))?
        .as_str()
        .map(String::from)
        .filter(|s| !s.is_empty())
}

pub fn manifest_kanban() -> Value {
    json!({
        "name": "kanban",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "1.1",
        "tools": [{
            "name": "kanban",
            "description": "Manage a per-project kanban board. Actions: boards (list), create, add (needs title, optional col+note), move (id+col), done (id), show, rm (id removes a card, board removes a board). Columns: backlog|todo|doing|review|done. Card ids look like k1.",
            "parameters": {
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["boards", "create", "add", "move", "done", "show", "rm"]},
                    "board":  {"type": "string", "description": "Board name (default: main)"},
                    "title":  {"type": "string", "description": "Card title for add"},
                    "col":    {"type": "string", "description": "Column name: backlog|todo|doing|review|done"},
                    "id":     {"type": "string", "description": "Card id like k3"},
                    "note":   {"type": "string", "description": "Optional note on the card"}
                },
                "required": ["action"]
            }
        }],
        "commands": ["/kanban"],
        "capabilities": ["host.ask"],
    })
}

// ---------- storage ---------------------------------------------------------

fn state_dir() -> PathBuf {
    let home = std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".gray")))
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("kanban")
}

/// FNV-1a over the session cwd — stable, dependency-free project key.
fn cwd_hash(cwd: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in cwd.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn boards_dir(cwd: &str) -> PathBuf {
    state_dir().join(cwd_hash(cwd))
}

fn valid_board_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.starts_with('.')
}

fn board_path(cwd: &str, board: &str) -> Result<PathBuf, String> {
    if !valid_board_name(board) {
        return Err(format!("invalid board name: {board:?}"));
    }
    Ok(boards_dir(cwd).join(format!("{board}.json")))
}

fn empty_board(name: &str) -> Value {
    let mut cols = serde_json::Map::new();
    for c in COLS {
        cols.insert(c.into(), json!([]));
    }
    json!({"name": name, "next": 1, "cols": cols})
}

fn load_board(cwd: &str, board: &str) -> Result<Value, String> {
    let path = board_path(cwd, board)?;
    let text = std::fs::read_to_string(&path)
        .map_err(|_| format!("board '{board}' not found — create it with kanban {{action:\"create\",board:\"{board}\"}}"))?;
    serde_json::from_str(&text).map_err(|e| format!("board '{board}' is corrupt: {e}"))
}

fn save_board(cwd: &str, board: &str, data: &Value) -> Result<(), String> {
    let path = board_path(cwd, board)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(data).unwrap())
        .map_err(|e| format!("couldn't write board: {e}"))
}

fn valid_col(col: &str) -> bool {
    COLS.contains(&col)
}

// ---------- rendering -------------------------------------------------------

fn truncate(s: &str, width: usize) -> String {
    let mut out: String = s.chars().take(width).collect();
    if s.chars().count() > width {
        out = s.chars().take(width.saturating_sub(1)).collect();
        out.push('…');
    }
    out
}

fn render(board: &Value) -> String {
    let name = board.get("name").and_then(Value::as_str).unwrap_or("?");
    let cols: Vec<Vec<String>> = COLS
        .iter()
        .map(|c| {
            board
                .pointer(&format!("/cols/{c}"))
                .and_then(Value::as_array)
                .map(|cards| {
                    cards
                        .iter()
                        .map(|card| {
                            let id = card.get("id").and_then(Value::as_str).unwrap_or("?");
                            let title = card.get("title").and_then(Value::as_str).unwrap_or("");
                            let note = card.get("note").and_then(Value::as_str).unwrap_or("");
                            let mut line = format!("{id} {title}");
                            if !note.is_empty() {
                                line.push_str(&format!(" — {note}"));
                            }
                            truncate(&line, COL_WIDTH - 1)
                        })
                        .collect()
                })
                .unwrap_or_default()
        })
        .collect();

    let mut out = format!("kanban: {name}\n");
    let header: Vec<String> = COLS
        .iter()
        .zip(&cols)
        .map(|(c, cards)| {
            format!(
                "{:width$}",
                format!("{} ({})", c.to_uppercase(), cards.len()),
                width = COL_WIDTH
            )
        })
        .collect();
    out.push_str(&header.join(""));
    out.push('\n');
    let rows = cols.iter().map(Vec::len).max().unwrap_or(0);
    if rows == 0 {
        out.push_str("(empty — kanban {action:\"add\",title:\"…\"} to add a card)\n");
        return out;
    }
    for row in 0..rows {
        let line: Vec<String> = cols
            .iter()
            .map(|cards| {
                let cell = cards.get(row).cloned().unwrap_or_default();
                format!("{cell:width$}", width = COL_WIDTH)
            })
            .collect();
        out.push_str(line.join("").trim_end());
        out.push('\n');
    }
    out
}

// ---------- actions ---------------------------------------------------------

fn act_boards(cwd: &str) -> Result<String, String> {
    let dir = boards_dir(cwd);
    let mut names: Vec<String> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            if let Some(name) = e.file_name().to_str() {
                if let Some(stem) = name.strip_suffix(".json") {
                    names.push(stem.to_string());
                }
            }
        }
    }
    names.sort();
    if names.is_empty() {
        Ok("no boards yet — kanban {action:\"create\"} makes 'main'".into())
    } else {
        Ok(format!("boards: {}", names.join(", ")))
    }
}

fn act_create(cwd: &str, board: &str) -> Result<String, String> {
    let path = board_path(cwd, board)?;
    if path.exists() {
        return Err(format!("board '{board}' already exists"));
    }
    save_board(cwd, board, &empty_board(board))?;
    Ok(format!("created board '{board}'"))
}

fn act_add(cwd: &str, board: &str, args: &Value) -> Result<String, String> {
    let title = args
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if title.is_empty() {
        return Err("add needs a title".into());
    }
    let col = args.get("col").and_then(Value::as_str).unwrap_or("backlog");
    if !valid_col(col) {
        return Err(format!("unknown column '{col}' — use {}", COLS.join("|")));
    }
    let mut b = if board_path(cwd, board)?.exists() {
        load_board(cwd, board)?
    } else {
        empty_board(board)
    };
    let next = b.get("next").and_then(Value::as_u64).unwrap_or(1);
    let id = format!("k{next}");
    let mut card = json!({"id": id, "title": title});
    if let Some(note) = args.get("note").and_then(Value::as_str).filter(|n| !n.is_empty()) {
        card["note"] = json!(note);
    }
    b["next"] = json!(next + 1);
    b["cols"][col].as_array_mut().unwrap().push(card);
    save_board(cwd, board, &b)?;
    Ok(format!("added {id} to {col}: {title}"))
}

fn locate_card(b: &Value, id: &str) -> Result<(&'static str, usize), String> {
    for c in COLS {
        if let Some(cards) = b["cols"][c].as_array() {
            if let Some(pos) = cards.iter().position(|k| k["id"].as_str() == Some(id)) {
                return Ok((c, pos));
            }
        }
    }
    Err(format!("no card '{id}' on this board"))
}

fn act_move(cwd: &str, board: &str, args: &Value, target: Option<&str>) -> Result<String, String> {
    let id = args.get("id").and_then(Value::as_str).unwrap_or("").trim();
    if id.is_empty() {
        return Err("move/done needs a card id".into());
    }
    let col = target
        .map(str::to_string)
        .or_else(|| args.get("col").and_then(Value::as_str).map(str::to_string))
        .ok_or_else(|| "move needs a target col".to_string())?;
    if !valid_col(&col) {
        return Err(format!("unknown column '{col}' — use {}", COLS.join("|")));
    }
    let mut b = load_board(cwd, board)?;
    let (src, pos) = locate_card(&b, id)?;
    if src == col {
        return Ok(format!("{id} is already in {col}"));
    }
    let card = b["cols"][src][pos].clone();
    b["cols"][src].as_array_mut().unwrap().remove(pos);
    b["cols"][&col].as_array_mut().unwrap().push(card.clone());
    save_board(cwd, board, &b)?;
    let title = card.get("title").and_then(Value::as_str).unwrap_or("");
    Ok(format!("moved {id} {src} → {col}: {title}"))
}

fn act_rm(cwd: &str, board: &str, args: &Value) -> Result<String, String> {
    if let Some(id) = args.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        let mut b = load_board(cwd, board)?;
        let (src, pos) = locate_card(&b, id)?;
        let card = b["cols"][src][pos].clone();
        b["cols"][src].as_array_mut().unwrap().remove(pos);
        save_board(cwd, board, &b)?;
        let title = card.get("title").and_then(Value::as_str).unwrap_or("");
        return Ok(format!("removed {id}: {title}"));
    }
    let path = board_path(cwd, board)?;
    if !path.exists() {
        return Err(format!("board '{board}' not found"));
    }
    std::fs::remove_file(&path).map_err(|e| format!("couldn't remove board: {e}"))?;
    Ok(format!("removed board '{board}'"))
}

fn session_cwd(params: &Value) -> String {
    params
        .pointer("/session/cwd")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| std::env::var("PWD").ok())
        .unwrap_or_else(|| ".".into())
}

fn call_tool(name: &str, args: &Value, session: &Value) -> Result<String, String> {
    if name != "kanban" {
        return Err(format!("unknown tool: {name}"));
    }
    let cwd = session_cwd(&json!({ "session": session }));
    let action = args.get("action").and_then(Value::as_str).unwrap_or("show");
    let board = args.get("board").and_then(Value::as_str).unwrap_or("main");
    match action {
        "boards" => act_boards(&cwd),
        "create" => act_create(&cwd, board),
        "add" => act_add(&cwd, board, args),
        "move" => act_move(&cwd, board, args, None),
        "done" => act_move(&cwd, board, args, Some("done")),
        "show" => Ok(render(&load_board(&cwd, board)?)),
        "rm" => act_rm(&cwd, board, args),
        other => Err(format!("unknown action '{other}' — boards|create|add|move|done|show|rm")),
    }
}

/// `/kanban move [board]` — two-step picker: choose a card, then a column,
/// and apply through `act_move`. Any ask failure (ungranted capability,
/// timeout, cancel) falls back to the board render.
fn move_interactive(cwd: &str, board: &str, ask: &mut Ask) -> String {
    let b = match load_board(cwd, board) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let mut card_opts: Vec<Value> = Vec::new();
    for c in COLS {
        if let Some(cards) = b["cols"][c].as_array() {
            for card in cards {
                let id = card.get("id").and_then(Value::as_str).unwrap_or("?");
                let title = card.get("title").and_then(Value::as_str).unwrap_or("");
                card_opts.push(json!({
                    "label": format!("{id} {title}"),
                    "description": format!("in {c}"),
                }));
            }
        }
    }
    if card_opts.is_empty() {
        return format!("board '{board}' has no cards — nothing to move");
    }
    card_opts.push(json!({"label": LABEL_CANCEL, "description": "don't move anything"}));
    let picked = ask(&json!({
        "questions": [{
            "id": Q_CARD,
            "header": "Move card",
            "question": format!("Move which card on '{board}'?"),
            "options": card_opts,
        }],
        "blocking": true,
    }));
    let label = picked_label(picked.as_ref(), Q_CARD).unwrap_or_default();
    if label.is_empty() || label == LABEL_CANCEL {
        return render(&b);
    }
    // Card labels are "<id> <title>"; the id is the first token.
    let id = label.split_whitespace().next().unwrap_or("");
    let mut col_opts: Vec<Value> = COLS
        .iter()
        .map(|c| json!({"label": c, "description": format!("move {id} here")}))
        .collect();
    col_opts.push(json!({"label": LABEL_CANCEL, "description": "don't move anything"}));
    let picked = ask(&json!({
        "questions": [{
            "id": Q_COL,
            "header": "Move card",
            "question": format!("Move {id} to which column?"),
            "options": col_opts,
        }],
        "blocking": true,
    }));
    let col = picked_label(picked.as_ref(), Q_COL).unwrap_or_default();
    if col.is_empty() || col == LABEL_CANCEL || !valid_col(&col) {
        return render(&b);
    }
    match act_move(cwd, board, &json!({"id": id, "col": col}), None) {
        Ok(text) => format!(
            "{text}\n\n{}",
            render(&load_board(cwd, board).unwrap_or(b))
        ),
        Err(e) => e,
    }
}

/// `/kanban [board]` — render the board as text columns for the user.
fn run_command(argv: &[&str], session: &Value, ask: &mut Ask) -> String {
    let cwd = session_cwd(&json!({ "session": session }));
    match argv.first().copied() {
        Some("boards") => act_boards(&cwd).unwrap_or_else(|e| e),
        Some("move") => move_interactive(&cwd, argv.get(1).copied().unwrap_or("main"), ask),
        Some(board) => match load_board(&cwd, board) {
            Ok(b) => render(&b),
            Err(e) => e,
        },
        None => match load_board(&cwd, "main") {
            Ok(b) => render(&b),
            Err(e) => format!(
                "{e}\n\n/kanban [board] renders a board · kanban tool actions: boards|create|add|move|done|show|rm"
            ),
        },
    }
}

/// One request → `Some(reply)`, or `None` for notifications. The bool asks
/// the loop to exit after writing the reply.
pub fn handle_kanban(req: &Value, ask: &mut Ask) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest_kanban(),
        "tool/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("args").cloned().unwrap_or(Value::Null);
            let session = params.get("session").cloned().unwrap_or(Value::Null);
            match call_tool(name, &args, &session) {
                Ok(text) => json!({ "content": text }),
                Err(e) => json!({ "content": e, "is_error": true }),
            }
        }
        "command/run" => {
            let argv: Vec<&str> = params
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let session = params.get("session").cloned().unwrap_or(Value::Null);
            json!({ "text": run_command(&argv, &session, ask) })
        }
        "plugin/shutdown" => return (Some(json!({ "id": id, "result": {} })), true),
        _ => {
            let error = json!({ "code": -32601, "message": "method not found" });
            return (Some(json!({ "id": id, "error": error })), false);
        }
    };
    (Some(json!({ "id": id, "result": result })), false)
}

fn main_kanban() -> std::io::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest_kanban());
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
        let (reply, exit) = handle_kanban(&req, &mut ask);
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
    use std::sync::Mutex;

    static LOCK: Mutex<()> = Mutex::new(());

    struct TempHome;
    impl TempHome {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("gray-kanban-test-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            unsafe { std::env::set_var("GRAY_HOME", &dir) };
            Self
        }
    }
    impl Drop for TempHome {
        fn drop(&mut self) {
            let dir = std::env::temp_dir().join(format!("gray-kanban-test-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    fn call(method: &str, params: Value) -> Value {
        call_ask(method, params, &mut |_p: &Value| None)
    }

    fn call_ask(method: &str, params: Value, ask: &mut Ask) -> Value {
        handle_kanban(&json!({ "id": 1, "method": method, "params": params }), ask)
            .0
            .unwrap()
    }

    /// Test asker: answers each successive host/ask by picking the option at
    /// the next index in `idxs` (then keeps picking index 0).
    fn picking_seq(idxs: Vec<usize>) -> impl FnMut(&Value) -> Option<Value> {
        let mut it = idxs.into_iter();
        move |p: &Value| {
            let i = it.next().unwrap_or(0);
            let q = &p["questions"][0];
            let qid = q["id"].as_str().unwrap_or("q").to_string();
            let label = q["options"][i]["label"].as_str()?.to_string();
            let mut answers = serde_json::Map::new();
            answers.insert(qid, json!({"answers": [label]}));
            Some(json!({"answers": answers}))
        }
    }

    fn tool(args: Value) -> Value {
        call(
            "tool/call",
            json!({ "name": "kanban", "args": args, "session": {"cwd": "/tmp/proj"} }),
        )["result"]
            .clone()
    }

    #[test]
    fn manifest_shape() {
        let m = call("plugin/manifest", Value::Null)["result"].clone();
        assert_eq!(m["name"], "kanban");
        assert_eq!(m["commands"], json!(["/kanban"]));
        assert_eq!(m["tools"][0]["name"], "kanban");
        assert_eq!(m["capabilities"], json!(["host.ask"]));
    }

    #[test]
    fn add_move_done_show() {
        let _g = LOCK.lock().unwrap();
        let _h = TempHome::new();
        let r = tool(json!({"action": "add", "title": "write tests"}));
        assert!(r["content"].as_str().unwrap().contains("k1"), "{r}");
        let r = tool(json!({"action": "move", "id": "k1", "col": "doing"}));
        assert!(r["content"].as_str().unwrap().contains("backlog → doing"));
        let r = tool(json!({"action": "done", "id": "k1"}));
        assert!(r["content"].as_str().unwrap().contains("→ done"));
        let r = tool(json!({"action": "show"}));
        let text = r["content"].as_str().unwrap();
        assert!(text.contains("DONE (1)") && text.contains("write tests"), "{text}");
    }

    #[test]
    fn errors_are_tool_errors() {
        let _g = LOCK.lock().unwrap();
        let _h = TempHome::new();
        assert_eq!(tool(json!({"action": "add"}))["is_error"], true);
        assert_eq!(tool(json!({"action": "show", "board": "nope"}))["is_error"], true);
        assert_eq!(
            tool(json!({"action": "move", "id": "k9", "col": "mars"}))["is_error"],
            true
        );
        assert_eq!(tool(json!({"action": "bogus"}))["is_error"], true);
    }

    #[test]
    fn boards_and_rm() {
        let _g = LOCK.lock().unwrap();
        let _h = TempHome::new();
        tool(json!({"action": "add", "title": "x", "board": "bugs"}));
        let r = tool(json!({"action": "boards"}));
        assert!(r["content"].as_str().unwrap().contains("bugs"), "{r}");
        let r = tool(json!({"action": "rm", "board": "bugs"}));
        assert!(r["content"].as_str().unwrap().contains("removed board"));
    }

    #[test]
    fn board_name_traversal_rejected() {
        assert!(board_path("/tmp/proj", "../evil").is_err());
        assert!(board_path("/tmp/proj", "a/b").is_err());
        assert!(board_path("/tmp/proj", "main").is_ok());
    }

    #[test]
    fn command_renders_board() {
        let _g = LOCK.lock().unwrap();
        let _h = TempHome::new();
        tool(json!({"action": "add", "title": "ship it"}));
        let r = call(
            "command/run",
            json!({ "name": "/kanban", "argv": [], "session": {"cwd": "/tmp/proj"} }),
        );
        let text = r["result"]["text"].as_str().unwrap();
        assert!(text.contains("kanban: main") && text.contains("ship it"), "{text}");
    }

    #[test]
    fn different_cwds_get_different_boards() {
        let _g = LOCK.lock().unwrap();
        let _h = TempHome::new();
        call(
            "tool/call",
            json!({ "name": "kanban", "args": {"action": "add", "title": "a"},
                    "session": {"cwd": "/tmp/one"} }),
        );
        let r = call(
            "tool/call",
            json!({ "name": "kanban", "args": {"action": "show"},
                    "session": {"cwd": "/tmp/two"} }),
        );
        assert_eq!(r["result"]["is_error"], true);
    }

    #[test]
    fn move_picker_moves_card() {
        let _g = LOCK.lock().unwrap();
        let _h = TempHome::new();
        tool(json!({"action": "add", "title": "write tests"}));
        // Options: [k1 card, cancel] then [backlog, todo, doing, review, done, cancel].
        let mut ask = picking_seq(vec![0, 2]);
        let r = call_ask(
            "command/run",
            json!({ "name": "/kanban", "argv": ["move"], "session": {"cwd": "/tmp/proj"} }),
            &mut ask,
        );
        let text = r["result"]["text"].as_str().unwrap();
        assert!(text.contains("moved k1 backlog → doing"), "{text}");
        assert!(text.contains("DOING (1)"), "{text}");
    }

    #[test]
    fn move_picker_no_ask_falls_back_to_render() {
        let _g = LOCK.lock().unwrap();
        let _h = TempHome::new();
        tool(json!({"action": "add", "title": "x"}));
        let r = call(
            "command/run",
            json!({ "name": "/kanban", "argv": ["move"], "session": {"cwd": "/tmp/proj"} }),
        );
        let text = r["result"]["text"].as_str().unwrap();
        assert!(text.contains("kanban: main") && text.contains("k1 x"), "{text}");
        // Card unmoved: still in backlog.
        let r = tool(json!({"action": "show"}));
        assert!(r["content"].as_str().unwrap().contains("BACKLOG (1)"), "{r}");
    }

    #[test]
    fn move_picker_cancel_at_second_step() {
        let _g = LOCK.lock().unwrap();
        let _h = TempHome::new();
        tool(json!({"action": "add", "title": "x"}));
        // Pick the card, then "cancel" (last column option = index 5).
        let mut ask = picking_seq(vec![0, 5]);
        let r = call_ask(
            "command/run",
            json!({ "name": "/kanban", "argv": ["move"], "session": {"cwd": "/tmp/proj"} }),
            &mut ask,
        );
        let text = r["result"]["text"].as_str().unwrap();
        assert!(text.contains("kanban: main") && !text.contains("moved"), "{text}");
    }

    #[test]
    fn shutdown_replies_then_exits() {
        let (reply, exit) = handle_kanban(&json!({ "id": 2, "method": "plugin/shutdown" }), &mut |_p: &Value| None);
        assert!(reply.is_some() && exit);
    }
}
