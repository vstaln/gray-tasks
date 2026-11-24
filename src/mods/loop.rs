//! gray-loop — self-driving iteration loops on plain wire primitives.
//!
//! `/loop <goal>` seeds
//! `~/.gray/loop/<session>/state.json` and returns a `{prompt}` that puts
//! the agent to work. On every `agent/before_start` while the state reads
//! `running` and `iter < max`, the hook bumps `iter` and appends a
//! "continue" user message — so the loop terminates at `max` even if the
//! model never touches the file.
//!
//! Completion is deterministic when tasks exist: `loop_task pass` flips
//! `passes` flags in state, and the plugin ends the loop itself once every
//! task passes. A `--gate <cmd>` verify command runs detached (tool calls
//! answer in milliseconds; the gate resolves on the next iteration or the
//! next loop_done poll) — a failing gate rejects the "done" and the loop
//! keeps going. With no tasks the `loop_done` tool ends the loop early,
//! still gated.
//!
//! `/loop stop` forces `failed`, `/loop max N` retunes the cap,
//! `/loop status` reports, `/loop tasks` renders the checklist,
//! `/loop steer <msg>` queues steering for the next iteration,
//! `/loop list` shows every session's loop.
//!
//! `--review` adds a second-agent stage between the task oracle and the
//! gate: a detached `gray -p` child reviews the work and must end its reply
//! with `VERDICT: CLEAR` / `VERDICT: RESOLVABLE — <fix>` / `VERDICT: NOVEL —
//! <issue>`. CLEAR proceeds to the gate; RESOLVABLE queues the finding as
//! steering and rejects the "done"; NOVEL asks the human (`host/ask`,
//! capability `host.ask`) whether to send it back, ship anyway, or stop —
//! and degrades to a steering note when no ask channel answers. A failing
//! gate clears `review_passed`, so post-gate fixes get reviewed again.
//!
//! Fail open everywhere: malformed state, missing files, bad args — all
//! produce `{}` or a text note, never a crash.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use serde_json::{Value, json};

#[path = "loop/sched.rs"]
mod sched;

const DEFAULT_MAX: u64 = 10;
const MAX_LEARNINGS: usize = 50;
const GATE_TIMEOUT_SECS: u64 = 180;
const GATE_OUT_TAIL: usize = 3000;
const REVIEW_TIMEOUT_SECS: u64 = 600;
/// Under the host's ~330s outer ask TTL.
const ASK_TTL: Duration = Duration::from_secs(290);

/// Replies to sidecar→host requests (`host/ask`) carry the string id sent.
pub type Pending = Arc<Mutex<HashMap<String, mpsc::Sender<Value>>>>;

/// Shared handles for calling back into the host. `None` in tests exercises
/// the degraded paths.
pub struct HostLink {
    pub out: Arc<Mutex<std::io::Stdout>>,
    pub pending: Pending,
    pub counter: Arc<Mutex<u64>>,
}

fn next_id(counter: &Arc<Mutex<u64>>) -> String {
    let mut n = counter.lock().expect("id counter");
    *n += 1;
    format!("q{n}")
}

pub fn manifest_loop() -> Value {
    json!({
        "name": "loop",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "2.0",
        "tools": [
            {
                "name": "loop_done", "label": "Loop done", "preview": "{status} · {reason|firstline}",
                "description": "Stop the active /loop when the goal is verifiably complete (status \"done\") or unachievable (status \"failed\"). If the loop has a gate command, \"done\" arms it: it runs in the background and is re-checked on every iteration and every loop_done call — only exit 0 ends the loop. If the loop was started with --review, a detached second-agent review must return VERDICT CLEAR (or a human must override a NOVEL finding) before the gate or \"done\" is accepted. Only call when a loop is running.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "status": { "type": "string", "enum": ["done", "failed"], "description": "Final loop status." },
                        "reason": { "type": "string", "description": "Why the loop is ending." }
                    },
                    "required": ["status"]
                }
            },
            {
                "name": "loop_wakeup", "label": "Loop wakeup", "preview": "{delay_seconds}s · {reason|firstline}",
                "description": "Pace a self-paced /loop: after this tick's work, schedule the next tick (delay_seconds 0 = continue right away, up to 3600) or end the loop (stop: true). If you don't re-arm during a tick, the loop ends after it. Same as running `gray loop wakeup <secs>` / `gray loop stop` in bash.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "delay_seconds": { "type": "integer", "minimum": 0, "maximum": 3600, "description": "Seconds after this turn ends to run the next tick. 0 for active work; longer when polling something external." },
                        "reason": { "type": "string", "description": "One sentence: why this delay (shown to the user)." },
                        "noop": { "type": "boolean", "description": "true if this tick changed nothing." },
                        "stop": { "type": "boolean", "description": "true ends the loop (finished, blocked on the user, or asked to stop); other fields ignored." }
                    }
                }
            },
            {
                "name": "loop_task", "label": "Loop task", "preview": "{op} · {text|firstline}",
                "description": "Manage the active /loop's task checklist — the deterministic exit oracle. op \"add\" appends a task (needs text). op \"pass\"/\"fail\" set a task's passes flag (needs task: 1-based id, exact text, unique prefix, or unique substring). op \"list\" shows the checklist. When every task passes the loop ends itself.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "op": { "type": "string", "enum": ["add", "pass", "fail", "list"], "description": "Checklist operation." },
                        "task": { "anyOf": [{ "type": "integer", "minimum": 1 }, { "type": "string" }], "description": "For pass/fail: 1-based task id, exact text, unique prefix, or unique substring." },
                        "text": { "type": "string", "description": "For add: the task description (one checkable unit of work)." }
                    },
                    "required": ["op"]
                }
            },
            {
                "name": "loop_learn", "label": "Loop learned", "preview": "{text|firstline}",
                "description": "Append one durable learning to the active /loop's log — something a future iteration must know (a decision made, a trap hit, a path that worked). Appended, never overwritten; last entries ride every continue prompt.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "text": { "type": "string", "description": "The learning to append." }
                    },
                    "required": ["text"]
                }
            }
        ],
        "commands": ["/loop"],
        "command_descriptions": {"/loop": "loop on a goal until its tasks pass"},
        "hooks": ["agent/before_start"],
        "capabilities": ["host.ask", "host.say", "widget.override"],
        "widget": true,
    })
}

// ── state ─────────────────────────────────────────────────────

fn gray_home() -> PathBuf {
    std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".gray")))
        .or_else(|| std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join(".gray")))
        .unwrap_or_else(|| PathBuf::from(".gray"))
}

fn state_path(session_id: &str) -> PathBuf {
    // session ids are uuids, but sanitize anyway — never trust a filename
    let safe: String = session_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    gray_home().join("loop").join(safe).join("state.json")
}

fn read_state(session_id: &str) -> Option<Value> {
    let text = std::fs::read_to_string(state_path(session_id)).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_state(session_id: &str, state: &Value) -> std::io::Result<()> {
    let path = state_path(session_id);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(state)?)?;
    std::fs::rename(&tmp, &path)
        .or_else(|_| std::fs::write(&path, serde_json::to_string_pretty(state)?))
}

fn set_status(session_id: &str, status: &str, note: &str) -> bool {
    let Some(mut st) = read_state(session_id) else {
        return false;
    };
    st["status"] = json!(status);
    if !note.is_empty() {
        st["notes"] = json!(note);
    }
    let ok = write_state(session_id, &st).is_ok();
    if ok && status != "running" {
        purge_wakes(session_id);
    }
    ok
}

// ── self-drive: session wake entries ─────────────────────────
// The host drains `cron/inbox/<sid>/*.json` into prompts (card+prompt
// fields) and a 2s poller wakes the REPL when files appear. Queuing one
// entry per turn is what makes the loop actually loop: the queued prompt
// starts the next turn, whose before_start queues the one after. Files
// named `loop-*` are ours — purge them on every terminal transition.

fn inbox_dir(sid: &str) -> PathBuf {
    gray_home().join("cron").join("inbox").join(sid)
}

fn queue_wake(sid: &str, card: &str, prompt: &str) -> Option<String> {
    if sid.is_empty() {
        return None;
    }
    let dir = inbox_dir(sid);
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let name = format!("loop-{nonce}-{}.json", std::process::id());
    let tmp = dir.join(format!(".{name}.tmp"));
    let body = json!({ "card": card, "prompt": prompt });
    if std::fs::write(&tmp, body.to_string()).is_ok()
        && std::fs::rename(&tmp, dir.join(&name)).is_ok()
    {
        return Some(name);
    }
    let _ = std::fs::remove_file(&tmp);
    None
}

fn purge_wakes(sid: &str) {
    if sid.is_empty() {
        return;
    }
    if let Ok(rd) = std::fs::read_dir(inbox_dir(sid)) {
        for ent in rd.flatten() {
            if ent.file_name().to_string_lossy().starts_with("loop-") {
                let _ = std::fs::remove_file(ent.path());
            }
        }
    }
}

fn session_id(params: &Value) -> String {
    params
        .get("session")
        .and_then(|s| s.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn session_cwd(params: &Value) -> String {
    params
        .get("session")
        .and_then(|s| s.get("cwd"))
        .and_then(Value::as_str)
        .unwrap_or(".")
        .to_string()
}

fn tasks(st: &Value) -> Vec<Value> {
    st.get("tasks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn render_tasks(ts: &[Value]) -> String {
    if ts.is_empty() {
        return "no tasks yet — add checkable units of work with loop_task add".into();
    }
    let done = ts
        .iter()
        .filter(|t| t["passes"].as_bool() == Some(true))
        .count();
    let mut lines = vec![format!("{done}/{} done:", ts.len())];
    let next_unmarked = ts.iter().position(|t| t["passes"].as_bool() != Some(true));
    for (i, t) in ts.iter().enumerate().take(15) {
        let mark = if t["passes"].as_bool() == Some(true) {
            "x"
        } else {
            " "
        };
        let arrow = if Some(i) == next_unmarked {
            "  <- next"
        } else {
            ""
        };
        lines.push(format!(
            "  [{}] {}. {}{}",
            mark,
            i + 1,
            t["text"].as_str().unwrap_or("?"),
            arrow
        ));
    }
    if ts.len() > 15 {
        lines.push(format!("  … {} more", ts.len() - 15));
    }
    lines.join("\n")
}

// ── gate ──────────────────────────────────────────────────────

fn gate_files(sid: &str) -> (PathBuf, PathBuf) {
    let dir = state_path(sid)
        .parent()
        .map(PathBuf::from)
        .unwrap_or_default();
    (dir.join("gate.out"), dir.join("gate.code"))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn gate_tail(sid: &str) -> String {
    let (out_p, _) = gate_files(sid);
    let bytes = std::fs::read(&out_p).unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    let t = text.trim();
    if t.is_empty() {
        return "(no output)".into();
    }
    t.chars()
        .rev()
        .take(GATE_OUT_TAIL)
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}

fn kill_tree(pid: u64) {
    #[cfg(unix)]
    {
        // Setsid children share `pid` as their process-group id, so a group
        // kill reaps the whole tree even if the leader already exited.
        let _ = std::process::Command::new("kill")
            .args(["-9", &format!("-{pid}")])
            .status();
        // Fallback: walk the descendant set for non-setsid spawns.
        let mut pids = vec![pid];
        let mut i = 0;
        while i < pids.len() && pids.len() < 128 {
            let cur = pids[i].to_string();
            if let Ok(out) = std::process::Command::new("pgrep")
                .args(["-P", &cur])
                .output()
            {
                for line in String::from_utf8_lossy(&out.stdout).lines() {
                    if let Ok(c) = line.trim().parse::<u64>()
                        && !pids.contains(&c)
                    {
                        pids.push(c);
                    }
                }
            }
            i += 1;
        }
        for p in pids.iter().skip(1) {
            let _ = std::process::Command::new("kill")
                .args(["-9", &p.to_string()])
                .status();
        }
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status();
    }
    #[cfg(windows)]
    let _ = std::process::Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .status();
    #[cfg(not(any(unix, windows)))]
    let _ = pid;
}

fn gate_clear(st: &mut Value) {
    st.as_object_mut().map(|o| o.remove("gate_run"));
}

/// A Command that runs `script` under a POSIX shell. On unix it goes through
/// `setsid` (Linux only — it doesn't exist on macOS/Windows) so the child is
/// its own process-group leader and `kill_tree` can reap the whole tree.
/// Elsewhere a bare `sh` (Git Bash on Windows) still runs the script.
fn sh_cmd(script: &str) -> std::process::Command {
    // setsid is a Linux (util-linux) tool — it does not exist on macOS or
    // Windows. There a bare `sh` (Git Bash on Windows) still runs the script;
    // kill_tree falls back to the descendant walk / taskkill /T.
    #[cfg(target_os = "linux")]
    {
        let mut c = std::process::Command::new("setsid");
        c.arg("sh").arg("-c").arg(script);
        c
    }
    #[cfg(not(target_os = "linux"))]
    {
        let mut c = std::process::Command::new("sh");
        c.arg("-c").arg(script);
        c
    }
}

fn gate_arm(sid: &str, st: &mut Value, cmd: &str, cwd: &str) -> Result<(), String> {
    let (out_p, code_p) = gate_files(sid);
    let out = std::fs::File::create(&out_p).map_err(|e| format!("gate: {e}"))?;
    let err = out.try_clone().map_err(|e| format!("gate: {e}"))?;
    let _ = std::fs::remove_file(&code_p);
    let mut child = sh_cmd("( eval \"$GATE_CMD\" ); printf %s $? > \"$GATE_CODE\"")
        .env("GATE_CMD", cmd)
        .env("GATE_CODE", &code_p)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(out))
        .stderr(std::process::Stdio::from(err))
        .spawn()
        .map_err(|e| format!("couldn't start gate command (needs `sh` on PATH): {e}"))?;
    let pid = child.id();
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    st["gate_run"] = json!({ "pid": pid, "at": now_secs(), "cmd": cmd });
    write_state(sid, st).map_err(|e| format!("gate: state write failed: {e}"))
}

enum GatePoll {
    Running(u64),
    Finished(i64),
    TimedOut(u64),
    Absent,
}

#[cfg(unix)]
fn pid_alive(pid: u64) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn pid_alive(_pid: u64) -> bool {
    true
}

fn gate_poll(sid: &str, st: &mut Value) -> GatePoll {
    let Some(run) = st.get("gate_run").cloned() else {
        return GatePoll::Absent;
    };
    let pid = run.get("pid").and_then(Value::as_u64).unwrap_or(0);
    let at = run.get("at").and_then(Value::as_u64).unwrap_or(0);
    let (_, code_p) = gate_files(sid);
    if let Ok(code) = std::fs::read_to_string(&code_p) {
        gate_clear(st);
        let _ = write_state(sid, st);
        return GatePoll::Finished(code.trim().parse().unwrap_or(-1));
    }
    let elapsed = now_secs().saturating_sub(at);
    if elapsed >= GATE_TIMEOUT_SECS {
        kill_tree(pid);
        gate_clear(st);
        let _ = write_state(sid, st);
        return GatePoll::TimedOut(elapsed);
    }
    if !pid_alive(pid) {
        gate_clear(st);
        let _ = write_state(sid, st);
        return GatePoll::Finished(-1);
    }
    GatePoll::Running(elapsed)
}

// ── second-agent review ─────────────────────────────────────

fn review_files(sid: &str) -> (PathBuf, PathBuf) {
    let dir = state_path(sid)
        .parent()
        .map(PathBuf::from)
        .unwrap_or_default();
    (dir.join("review.out"), dir.join("review.code"))
}

fn review_enabled(st: &Value) -> bool {
    st.get("review").and_then(Value::as_bool) == Some(true) && st.get("review_disabled").is_none()
}

fn review_passed(st: &Value) -> bool {
    st.get("review_passed").and_then(Value::as_bool) == Some(true)
}

fn push_steer(st: &mut Value, msg: String) {
    let mut q = st["steer"].as_array().cloned().unwrap_or_default();
    q.push(json!(msg));
    st["steer"] = json!(q);
}

/// Parse the reviewer's last `VERDICT:` line → (kind, note).
fn parse_verdict(text: &str) -> Option<(&'static str, String)> {
    for line in text.lines().rev() {
        let Some(rest) = line.trim().strip_prefix("VERDICT:") else {
            continue;
        };
        let rest = rest.trim();
        for kind in ["CLEAR", "RESOLVABLE", "NOVEL"] {
            if let Some(tail) = rest.strip_prefix(kind) {
                let note = tail
                    .trim_start_matches([' ', '—', '–', '-', ':'])
                    .trim()
                    .to_string();
                return Some((kind, note));
            }
        }
    }
    None
}

/// The detached reviewer's prompt — verdict contract rides at the end.
fn review_prompt(st: &Value) -> String {
    let goal = st.get("goal").and_then(Value::as_str).unwrap_or("?");
    let ts = tasks(st);
    let checklist = if ts.is_empty() {
        "  (the working agent reported done with no checklist)".to_string()
    } else {
        ts.iter()
            .map(|t| {
                format!(
                    "  [{}] {}",
                    if t["passes"].as_bool() == Some(true) {
                        "x"
                    } else {
                        " "
                    },
                    t["text"].as_str().unwrap_or("?")
                )
            })
            .collect::<Vec<_>>()
            .join(
                "
",
            )
    };
    let notes = st.get("notes").and_then(Value::as_str).unwrap_or("");
    format!(
        "You are a second agent performing an independent code review of another agent's work,          in the same working directory. Inspect the real state — files, `git status`/`diff`/`log`,          test results — do not trust the claims below.

         Goal: {goal}

         Checklist the working agent marked:
{checklist}

         Its progress notes: {notes}

         Judge whether the goal is truly and completely met at reasonable quality.          Read-only: do not modify any files.

         End your reply with exactly one verdict line:
         VERDICT: CLEAR — the goal is met and the work is sound
         VERDICT: RESOLVABLE — <a concrete problem the working agent can fix itself>
         VERDICT: NOVEL — <an issue needing human judgment: ambiguous spec, design fork, out-of-scope discovery>
         Put the actionable substance inside the verdict line itself."
    )
}

enum ReviewPoll {
    Absent,
    Running(u64),
    Clear,
    Resolvable(String),
    Novel(String),
    Failed(String),
}

/// Spawn the detached reviewer: `gray -p <prompt>` via the same
/// spawn-to-files pattern as the gate. Binary: `$GRAY_LOOP_REVIEW_BIN`,
/// then `$GRAY_BIN`, else `gray` on PATH.
fn review_arm(sid: &str, st: &mut Value, cwd: &str) -> Result<(), String> {
    let (out_p, code_p) = review_files(sid);
    let out = std::fs::File::create(&out_p).map_err(|e| format!("review: {e}"))?;
    let err = out.try_clone().map_err(|e| format!("review: {e}"))?;
    let _ = std::fs::remove_file(&code_p);
    let bin = std::env::var("GRAY_LOOP_REVIEW_BIN")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("GRAY_BIN").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "gray".into());
    let name = format!("loop-review-{}", &sid[..sid.len().min(8)]);
    let mut child = sh_cmd(
        r#""$LOOP_REVIEW_BIN" -p "$LOOP_REVIEW_PROMPT"; printf %s $? > "$LOOP_REVIEW_CODE""#,
    )
    .env("LOOP_REVIEW_BIN", bin)
    .env("LOOP_REVIEW_PROMPT", review_prompt(st))
    .env("LOOP_REVIEW_CODE", &code_p)
    .env("GRAY_AGENT_NAME", name)
    .current_dir(cwd)
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::from(out))
    .stderr(std::process::Stdio::from(err))
    .spawn()
    .map_err(|e| format!("couldn't start reviewer (needs `sh` + `gray` on PATH): {e}"))?;
    let pid = child.id();
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    st["review_run"] = json!({ "pid": pid, "at": now_secs() });
    write_state(sid, st).map_err(|e| format!("review: state write failed: {e}"))
}

fn review_poll(sid: &str, st: &mut Value) -> ReviewPoll {
    let Some(run) = st.get("review_run").cloned() else {
        return ReviewPoll::Absent;
    };
    let pid = run.get("pid").and_then(Value::as_u64).unwrap_or(0);
    let at = run.get("at").and_then(Value::as_u64).unwrap_or(0);
    let (out_p, code_p) = review_files(sid);
    let clear = |st: &mut Value| {
        st.as_object_mut().map(|o| o.remove("review_run"));
        let _ = write_state(sid, st);
    };
    if let Ok(code) = std::fs::read_to_string(&code_p) {
        clear(st);
        let text = std::fs::read_to_string(&out_p).unwrap_or_default();
        return match parse_verdict(&text) {
            Some(("CLEAR", _)) => ReviewPoll::Clear,
            Some(("RESOLVABLE", n)) => ReviewPoll::Resolvable(n),
            Some(("NOVEL", n)) => ReviewPoll::Novel(n),
            _ => ReviewPoll::Failed(format!("exited {} with no VERDICT line", code.trim())),
        };
    }
    let elapsed = now_secs().saturating_sub(at);
    if elapsed >= REVIEW_TIMEOUT_SECS {
        kill_tree(pid);
        clear(st);
        return ReviewPoll::Failed(format!("timed out after {elapsed}s"));
    }
    if !pid_alive(pid) {
        clear(st);
        return ReviewPoll::Failed("died without recording an exit code".into());
    }
    ReviewPoll::Running(elapsed)
}

enum HumanVerdict {
    Retry(String),
    Ship,
    Stop,
    Unavailable,
}

/// The human-review node: a blocking `host/ask` built on the shared
/// `gray_questions` wire shapes. Any channel failure → Unavailable.
fn human_review(link: Option<&HostLink>, issue: &str) -> HumanVerdict {
    let Some(link) = link else {
        return HumanVerdict::Unavailable;
    };
    let Ok(qs) = gray_questions::normalize_questions(vec![gray_questions::UserQuestion {
        id: "loop_review".into(),
        header: "Loop review".into(),
        question: format!(
            "A second-agent review flagged something it can't resolve:

{issue}

How should the loop proceed?"
        ),
        options: vec![
            gray_questions::UserOption {
                label: "Send back to work (Recommended)".into(),
                description:
                    "The loop keeps iterating on it; pick Other/add a note to give guidance.".into(),
            },
            gray_questions::UserOption {
                label: "Ship anyway".into(),
                description: "Accept the work as-is and run the remaining checks.".into(),
            },
            gray_questions::UserOption {
                label: "Stop the loop".into(),
                description: "End the loop as failed.".into(),
            },
        ],
        is_other: false,
    }]) else {
        return HumanVerdict::Unavailable;
    };
    let id = next_id(&link.counter);
    let (tx, rx) = mpsc::channel();
    link.pending.lock().expect("pending").insert(id.clone(), tx);
    let req = json!({
        "id": id,
        "method": gray_questions::HOST_ASK,
        "params": { "questions": qs, "blocking": true },
    });
    {
        let mut o = link.out.lock().expect("stdout");
        if writeln!(o, "{req}").and_then(|_| o.flush()).is_err() {
            link.pending.lock().expect("pending").remove(&id);
            return HumanVerdict::Unavailable;
        }
    }
    let reply = rx.recv_timeout(ASK_TTL).ok();
    link.pending.lock().expect("pending").remove(&id);
    let Some(reply) = reply else {
        return HumanVerdict::Unavailable;
    };
    if reply.get("error").is_some() {
        return HumanVerdict::Unavailable;
    }
    let answers = gray_questions::parse_ask_result(&reply);
    let Some(a) = answers.iter().find(|a| a.id == "loop_review") else {
        return HumanVerdict::Unavailable;
    };
    let label = a.answers.first().map(String::as_str).unwrap_or("");
    let note: Vec<&str> = a
        .answers
        .iter()
        .filter_map(|s| s.strip_prefix("user_note:").map(str::trim))
        .collect();
    if label.starts_with("Ship") {
        HumanVerdict::Ship
    } else if label.starts_with("Stop") {
        HumanVerdict::Stop
    } else {
        let note = note.join(" ");
        HumanVerdict::Retry(if note.is_empty() {
            issue.to_string()
        } else {
            format!("{issue} — human note: {note}")
        })
    }
}

/// One step of the review stage. `Some` = a rendered verdict for the
/// caller; `None` = proceed to the next stage (gate/done).
fn review_stage(
    sid: &str,
    st: &mut Value,
    cwd: &str,
    link: Option<&HostLink>,
) -> Option<DoneCheck> {
    match review_poll(sid, st) {
        ReviewPoll::Absent => match review_arm(sid, st, cwd) {
            Ok(()) => Some(DoneCheck::Verifying(
                "reviewing — a second agent (`gray -p`) is checking the work in the background;                  it resolves on the next iteration or your next loop_done call"
                    .into(),
            )),
            Err(e) => {
                st["review_disabled"] = json!(e.clone());
                push_steer(st, format!("second-agent review unavailable: {e}"));
                let _ = write_state(sid, st);
                None
            }
        },
        ReviewPoll::Running(secs) => Some(DoneCheck::Verifying(format!(
            "reviewer still running ({secs}s of {REVIEW_TIMEOUT_SECS}s) — resolves on the next iteration or your next loop_done call"
        ))),
        ReviewPoll::Clear => {
            st["review_passed"] = json!(true);
            st["review_fails"] = json!(0);
            let _ = write_state(sid, st);
            None
        }
        ReviewPoll::Resolvable(note) => {
            push_steer(st, format!("reviewer sent it back (fixable): {note}"));
            let _ = write_state(sid, st);
            Some(DoneCheck::Rejected(format!(
                "second-agent review found a resolvable issue — fix it, then loop_done again:
{note}
(full review: {})",
                review_files(sid).0.display()
            )))
        }
        ReviewPoll::Novel(issue) => match human_review(link, &issue) {
            HumanVerdict::Ship => {
                st["review_passed"] = json!(true);
                let _ = write_state(sid, st);
                None
            }
            HumanVerdict::Stop => {
                st["status"] = json!("failed");
                st["notes"] = json!(format!("stopped at human review — {issue}"));
                let _ = write_state(sid, st);
                Some(DoneCheck::Complete(format!(
                    "loop ended: stopped at human review — {issue}"
                )))
            }
            HumanVerdict::Retry(guidance) => {
                push_steer(st, format!("human review: {guidance}"));
                let _ = write_state(sid, st);
                Some(DoneCheck::Rejected(format!("human sent it back — {guidance}")))
            }
            HumanVerdict::Unavailable => {
                push_steer(
                    st,
                    format!(
                        "reviewer flagged a novel issue but no human answered: {issue} — resolve it yourself or explain why it can't be"
                    ),
                );
                let _ = write_state(sid, st);
                Some(DoneCheck::Rejected(format!(
                    "novel issue needs human judgment but no ask channel answered: {issue}"
                )))
            }
        },
        ReviewPoll::Failed(why) => {
            let fails = st
                .get("review_fails")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                + 1;
            st["review_fails"] = json!(fails);
            if fails >= 3 {
                st["review_disabled"] =
                    json!(format!("reviewer failed {fails} times in a row ({why})"));
                push_steer(
                    st,
                    format!("second-agent review disabled after {fails} failures: {why}"),
                );
                let _ = write_state(sid, st);
                Some(DoneCheck::Rejected(format!(
                    "reviewer failed {fails}× in a row ({why}) — review disabled; \"done\" re-arms nothing, call loop_done again to proceed without it"
                )))
            } else {
                let _ = write_state(sid, st);
                Some(DoneCheck::Rejected(format!(
                    "reviewer failed ({why}) — \"done\" rejected; a fresh review arms on the next poll"
                )))
            }
        }
    }
}

enum DoneCheck {
    Open,
    Verifying(String),
    Rejected(String),
    Complete(String),
}

fn resolve_done(sid: &str, cwd: &str, basis: &str, link: Option<&HostLink>) -> DoneCheck {
    let Some(mut st) = read_state(sid) else {
        return DoneCheck::Rejected("couldn't read loop state".into());
    };
    if st.get("status").and_then(Value::as_str) != Some("running") {
        return DoneCheck::Rejected("loop is not running".into());
    }
    // Review stage runs before the gate: a second agent judges the work,
    // then the mechanical checks decide.
    if review_enabled(&st)
        && !review_passed(&st)
        && let Some(verdict) = review_stage(sid, &mut st, cwd, link)
    {
        return verdict;
    }
    let Some(gate) = st
        .get("gate")
        .and_then(Value::as_str)
        .filter(|g| !g.is_empty())
        .map(str::to_string)
    else {
        set_status(sid, "done", basis);
        return DoneCheck::Complete(format!("loop complete: {basis}"));
    };
    let verdict = match gate_poll(sid, &mut st) {
        GatePoll::Absent => match gate_arm(sid, &mut st, &gate, cwd) {
            Ok(()) => DoneCheck::Verifying(format!(
                "verifying — gate `{gate}` running in background; it resolves on the next iteration or your next loop_done call"
            )),
            Err(e) => DoneCheck::Rejected(format!("{e} — \"done\" rejected, loop continues")),
        },
        GatePoll::Running(secs) => DoneCheck::Verifying(format!(
            "gate `{gate}` still verifying ({secs}s of {GATE_TIMEOUT_SECS}s) — resolves on the next iteration or your next loop_done call"
        )),
        GatePoll::TimedOut(secs) => DoneCheck::Rejected(format!(
            "gate `{gate}` timed out after {secs}s — \"done\" rejected, loop continues. Output tail:\n{}",
            gate_tail(sid)
        )),
        GatePoll::Finished(0) => {
            set_status(sid, "done", &format!("{basis}; gate `{gate}` green"));
            DoneCheck::Complete(format!("loop complete: {basis}, gate green"))
        }
        GatePoll::Finished(code) if code < 0 => DoneCheck::Rejected(format!(
            "gate `{gate}` died without recording an exit code — \"done\" rejected, loop continues. Output tail:\n{}",
            gate_tail(sid)
        )),
        GatePoll::Finished(code) => DoneCheck::Rejected(format!(
            "gate `{gate}` exited {code} — \"done\" rejected, loop continues. Output tail:\n{}",
            gate_tail(sid)
        )),
    };
    if matches!(verdict, DoneCheck::Rejected(_)) {
        unpass_review(&mut st);
        let _ = write_state(sid, &st);
    }
    verdict
}

/// A failed/timed-out gate means the work changed — a passed review no
/// longer covers it, so the next completion attempt re-reviews.
fn unpass_review(st: &mut Value) {
    if st.get("review_passed").and_then(Value::as_bool) == Some(true) {
        st["review_passed"] = json!(false);
    }
}

fn check_done(sid: &str, cwd: &str, link: Option<&HostLink>) -> DoneCheck {
    let Some(st) = read_state(sid) else {
        return DoneCheck::Open;
    };
    let ts = tasks(&st);
    if ts.is_empty() || ts.iter().any(|t| t["passes"].as_bool() != Some(true)) {
        return DoneCheck::Open;
    }
    resolve_done(sid, cwd, &format!("all {} tasks passed", ts.len()), link)
}

// ── commands ──────────────────────────────────────────────────

/// `/loop <goal>` — seed state, return the kickoff prompt.
fn start_goal(session_id: &str, goal: &str, gate: Option<&str>, review: bool) -> Value {
    let path = state_path(session_id);
    // Replacing a running loop: kill its detached gate/reviewer so they
    // can't go orphaned or write a stale result file into the new run.
    let mut replaced = false;
    if let Some(old_st) = read_state(session_id)
        && old_st.get("status").and_then(Value::as_str) == Some("running")
    {
        replaced = true;
        for key in ["gate_run", "review_run"] {
            if let Some(pid) = old_st
                .get(key)
                .and_then(|g| g.get("pid"))
                .and_then(Value::as_u64)
            {
                kill_tree(pid);
            }
        }
    }
    purge_wakes(session_id);
    let existing_max = read_state(session_id)
        .filter(|st| !sched::is_sched(st))
        .and_then(|st| st.get("max").and_then(Value::as_u64))
        .unwrap_or(DEFAULT_MAX);
    let mut st = json!({
        "goal": goal,
        "iter": 0,
        "max": existing_max,
        "status": "running",
        "notes": "",
        "tasks": [],
        "learnings": [],
        "steer": [],
    });
    if let Some(g) = gate {
        st["gate"] = json!(g);
    }
    if review {
        st["review"] = json!(true);
    }
    if let Err(e) = write_state(session_id, &st) {
        return json!({ "text": format!("loop: couldn't write {}: {e}", path.display()) });
    }
    let gate_line = gate
        .map(|g| format!("\nA verification gate is set: `{g}` runs in the background (from the session directory) once all tasks pass — it must exit 0 before \"done\" is accepted; it resolves on a later iteration."))
        .unwrap_or_default();
    let review_line = if review {
        "\nA second-agent review is set: once your tasks pass, a detached `gray -p` reviewer inspects the \
         work and answers VERDICT CLEAR / RESOLVABLE / NOVEL — only CLEAR (or a human override on NOVEL) \
         reaches the final checks. RESOLVABLE findings come back to you as steering."
    } else {
        ""
    };
    let replaced_line = if replaced {
        "\n(Note: this replaced a previous loop that was still running — its task list, learnings and counters were reset.)\n"
    } else {
        ""
    };
    json!({ "prompt": format!(
        "{replaced_line}Work on this goal: {goal}\n\n\
         You are inside a /loop — after this turn you will be re-prompted to continue until the goal is\n\
         done or the iteration cap ({existing_max}) is hit.\n\n\
         How this loop works:\n\
         1. Break the goal into checkable units and register them: call loop_task with op \"add\"\n\
            once per task. The loop ends itself when every task passes — that is the exit oracle,\n\
            not your say-so.\n\
         2. Each iteration: work the next unchecked task, verify it for real, then mark it with\n\
            loop_task op \"pass\". If it breaks later, mark op \"fail\" and fix it.\n\
         3. Anything a future iteration must know (decisions, traps, what worked), append with\n\
            loop_learn — the log rides every continue prompt.\n\
         4. Small task-free goals may skip the checklist and call loop_done directly.\n\
         5. Your durable journal is {} — \"notes\" is a short progress summary\n\
            for your next iteration (what's done, what's next, what not to redo). Update it before\n\
            ending every turn. In a git repo, prefer committing completed units so progress\n\
            survives context loss.{gate_line}{review_line}\n\n\
         When the goal is verifiably complete call loop_done with status \"done\"; if it turns out\n\
         to be unachievable call it with \"failed\" and a reason.\n\n\
         If the loop_* tools aren't in your tool list, use bash instead (same effect):\n\
         `{cli} task add \"<text>\"` · `{cli} task pass <N>` · `{cli} learn \"<text>\"` ·\n\
         `{cli} done \"<reason>\"` · `{cli} fail \"<reason>\"`.",
        path.display(),
        cli = sched::cli(),
    ) })
}

fn run_command(argv: &[&str], params: &Value) -> Value {
    let sid = session_id(params);
    if sid.is_empty() {
        return json!({ "text": "loop: no session id" });
    }
    let cwd = session_cwd(params);
    // `/loop goal <goal>` (or any --gate/--review flag) → the checklist
    // goal loop. `/loop stop|status|…` → control. Everything else is
    // Claude Code's `/loop [interval] <prompt>`.
    if argv.first() == Some(&"goal") {
        return goal_run(&argv[1..], &sid, params);
    }
    if argv.iter().any(|w| *w == "--gate" || *w == "--review") {
        return goal_run(argv, &sid, params);
    }
    let control = matches!(
        argv.first().copied(),
        Some("stop" | "status" | "tasks" | "list" | "steer" | "max" | "help")
    );
    let is_control = match argv.first().copied() {
        Some("steer") => true,
        Some("max") => argv.len() == 2 && argv[1].parse::<u64>().is_ok_and(|n| n > 0),
        Some(_) if control => argv.len() == 1,
        _ => false,
    };
    if !is_control {
        if let Some(reply) = sched::start(argv, &sid, &cwd) {
            return reply;
        }
        // bare /loop, no loop.md: show what's running, else help
        if let Some(st) = read_state(&sid)
            && st.get("status").and_then(Value::as_str) == Some("running")
        {
            return run_command(&["status"], params);
        }
        return json!({ "text": help_text() });
    }
    if argv.first() == Some(&"help") {
        return json!({ "text": help_text() });
    }
    // Bare-word subcommands only fire alone — "/loop stop the rollout" is
    // a goal, not a stop. `max` needs its numeric arg to be one.
    match argv.first().copied() {
        Some(word) if argv.len() > 1 && matches!(word, "stop" | "status" | "tasks" | "list") => {
            goal_run(argv, &sid, params)
        }
        Some("max")
            if argv
                .get(1)
                .and_then(|s| s.parse::<u64>().ok())
                .filter(|n| *n > 0)
                .is_none()
                && argv.len() > 1 =>
        {
            goal_run(argv, &sid, params)
        }
        Some("stop") => {
            if let Some(mut st) = read_state(&sid) {
                if let Some(pid) = st
                    .get("gate_run")
                    .and_then(|g| g.get("pid"))
                    .and_then(Value::as_u64)
                {
                    kill_tree(pid);
                }
                if let Some(pid) = st
                    .get("review_run")
                    .and_then(|g| g.get("pid"))
                    .and_then(Value::as_u64)
                {
                    kill_tree(pid);
                }
                st.as_object_mut().map(|o| o.remove("review_run"));
                gate_clear(&mut st);
                let was = st.get("status").and_then(Value::as_str) == Some("running");
                st["status"] = json!(if sched::is_sched(&st) {
                    "stopped"
                } else {
                    "failed"
                });
                st["notes"] = json!("stopped by user via /loop stop");
                if !was {
                    purge_wakes(&sid);
                    return json!({ "text": "no running loop for this session" });
                }
                let ok = write_state(&sid, &st).is_ok();
                purge_wakes(&sid);
                if ok {
                    return json!({ "text": "loop stopped" });
                }
            }
            json!({ "text": "no active loop for this session" })
        }
        Some("status") => match read_state(&sid) {
            Some(st) if sched::is_sched(&st) => json!({ "text": sched::status_line(&st) }),
            Some(st) => {
                let ts = tasks(&st);
                let task_line = if ts.is_empty() {
                    String::new()
                } else {
                    let done = ts
                        .iter()
                        .filter(|t| t["passes"].as_bool() == Some(true))
                        .count();
                    format!("\ntasks: {done}/{} done", ts.len())
                };
                let gate_line = st
                    .get("gate")
                    .and_then(Value::as_str)
                    .map(|g| format!("\ngate: {g}"))
                    .unwrap_or_default();
                let review_line = if st.get("review").and_then(Value::as_bool) == Some(true) {
                    let s = if review_passed(&st) {
                        "cleared".to_string()
                    } else if st.get("review_run").is_some() {
                        "running".to_string()
                    } else if let Some(why) = st.get("review_disabled").and_then(Value::as_str) {
                        format!("skipped ({why})")
                    } else {
                        "pending".to_string()
                    };
                    format!("\nreview: {s}")
                } else {
                    String::new()
                };
                let run_line = st
                    .get("gate_run")
                    .and_then(|r| {
                        let at = r.get("at").and_then(Value::as_u64)?;
                        Some(format!(
                            "\nverifying: gate running ({}s of {GATE_TIMEOUT_SECS}s)",
                            now_secs().saturating_sub(at)
                        ))
                    })
                    .unwrap_or_default();
                json!({ "text": format!(
                    "loop {} — iter {}/{} — goal: {}{}{}{}{}\nnotes: {}",
                    st.get("status").and_then(Value::as_str).unwrap_or("?"),
                    st.get("iter").and_then(Value::as_u64).unwrap_or(0),
                    st.get("max").and_then(Value::as_u64).unwrap_or(DEFAULT_MAX),
                    st.get("goal").and_then(Value::as_str).unwrap_or("?"),
                    task_line,
                    gate_line,
                    review_line,
                    run_line,
                    st.get("notes").and_then(Value::as_str).unwrap_or(""),
                ) })
            }
            None => json!({ "text": "no loop state for this session" }),
        },
        Some("tasks") => match read_state(&sid) {
            Some(st) => json!({ "text": format!("loop tasks — {}", render_tasks(&tasks(&st))) }),
            None => json!({ "text": "no loop state for this session" }),
        },
        Some("steer") => {
            let msg = argv[1..].join(" ");
            if msg.is_empty() {
                return json!({ "text": "usage: /loop steer <message for the next tick>" });
            }
            match read_state(&sid) {
                Some(mut st) if st["status"].as_str() == Some("running") => {
                    let mut q = st["steer"].as_array().cloned().unwrap_or_default();
                    q.push(json!(msg));
                    st["steer"] = json!(q);
                    if write_state(&sid, &st).is_ok() {
                        json!({ "text": "steering queued for the next tick" })
                    } else {
                        json!({ "text": "loop: couldn't write state" })
                    }
                }
                _ => json!({ "text": "no running loop for this session" }),
            }
        }
        Some("max") => {
            let Some(n) = argv
                .get(1)
                .and_then(|s| s.parse::<u64>().ok())
                .filter(|n| *n > 0)
            else {
                return json!({ "text": "usage: /loop max <positive N>" });
            };
            match read_state(&sid) {
                Some(mut st) => {
                    st["max"] = json!(n);
                    if write_state(&sid, &st).is_ok() {
                        json!({ "text": format!("loop max → {n}") })
                    } else {
                        json!({ "text": "loop: couldn't write state" })
                    }
                }
                None => json!({ "text": "no active loop — start one with /loop <goal> first" }),
            }
        }
        Some("list") => {
            let root = gray_home().join("loop");
            let mut lines = vec![];
            if let Ok(rd) = std::fs::read_dir(&root) {
                for ent in rd.flatten() {
                    let Ok(text) = std::fs::read_to_string(ent.path().join("state.json")) else {
                        continue;
                    };
                    let Ok(st) = serde_json::from_str::<Value>(&text) else {
                        continue;
                    };
                    let sess = ent.file_name().to_string_lossy().to_string();
                    lines.push(format!(
                        "{} — {} — iter {}/{} — {}",
                        &sess[..sess.len().min(8)],
                        st.get("status").and_then(Value::as_str).unwrap_or("?"),
                        st.get("iter").and_then(Value::as_u64).unwrap_or(0),
                        st.get("max").and_then(Value::as_u64).unwrap_or(DEFAULT_MAX),
                        st.get("goal")
                            .or_else(|| st.get("prompt"))
                            .and_then(Value::as_str)
                            .unwrap_or("?")
                            .lines()
                            .next()
                            .unwrap_or("?"),
                    ));
                }
            }
            lines.sort();
            if lines.is_empty() {
                json!({ "text": "no loops found" })
            } else {
                json!({ "text": format!("loops ({}):\n{}", lines.len(), lines.join("\n")) })
            }
        }
        _ => json!({ "text": help_text() }),
    }
}

fn help_text() -> String {
    format!(
        "gray-loop {}\n\
         /loop 5m <prompt>        re-run the prompt every 5m (also: <prompt> every 2 hours)\n\
         /loop <prompt>           self-paced — the agent picks when the next tick runs\n\
         /loop                    self-paced over .gray/loop.md (or ~/.gray/loop.md)\n\
         /loop goal <goal> [--gate \"cmd\"] [--review]   checklist loop until the goal verifiably passes\n\
         /loop status · stop · steer <msg> · max N · tasks · list",
        env!("CARGO_PKG_VERSION")
    )
}

/// `/loop <goal> [--review] [--gate "<cmd>"]` — everything that isn't a
/// bare subcommand lands here, flags split out of the goal text.
fn goal_run(argv: &[&str], sid: &str, params: &Value) -> Value {
    let _ = params;
    let words: Vec<&str> = argv.to_vec();
    let split = words
        .iter()
        .position(|w| *w == "--gate")
        .unwrap_or(words.len());
    let gate = if split < words.len() {
        match words[split + 1..].join(" ") {
            g if g.is_empty() => {
                return json!({ "text": "usage: /loop goal <goal> [--review] [--gate \"<verify command>\"]" });
            }
            g => Some(g),
        }
    } else {
        None
    };
    let mut review = false;
    let goal = words[..split]
        .iter()
        .filter(|w| {
            if **w == "--review" {
                review = true;
                false
            } else {
                true
            }
        })
        .copied()
        .collect::<Vec<_>>()
        .join(" ");
    if goal.is_empty() {
        return json!({ "text": "usage: /loop goal <goal> [--review] [--gate \"<verify command>\"]" });
    }
    start_goal(sid, &goal, gate.as_deref(), review)
}

// ── hook + tools ──────────────────────────────────────────────

/// `agent/before_start` — while a loop is running under its cap, bump iter
/// and append the continue message. Anything odd → `{}` (fail open).
fn before_start(params: &Value, link: Option<&HostLink>) -> Value {
    let sid = session_id(params);
    if sid.is_empty() {
        return json!({});
    }
    if read_state(&sid).is_some_and(|st| sched::is_sched(&st)) {
        return json!({});
    }
    let cwd = session_cwd(params);
    match iterate(&sid, &cwd, link, true) {
        Some(text) => json!({ "text": text }),
        None => json!({}),
    }
}

/// `event/notify` turn_end — fallback drive for hosts that never dispatch
/// `agent/before_start` (lean prompt mode skips the hook entirely). Skipped
/// when before_start already advanced this turn (`bs_iter` tag) so the two
/// channels never double-count an iteration.
fn on_turn_end(params: &Value, link: Option<&HostLink>) {
    let sid = session_id(params);
    if sid.is_empty() {
        return;
    }
    let Some(st) = read_state(&sid) else {
        return;
    };
    if sched::is_sched(&st) {
        sched::on_turn_end(&sid, link);
        return;
    }
    if st.get("status").and_then(Value::as_str) != Some("running") {
        return;
    }
    if st.get("iter") == st.get("bs_iter") {
        return;
    }
    let cwd = session_cwd(params);
    let _ = iterate(&sid, &cwd, link, false);
}

/// Shared iteration advance: poll review + gate, enforce the cap, bump iter,
/// queue the wake entry for the next turn. `from_bs` tags the bump so
/// `on_turn_end` can tell this turn was already driven. Returns the continue
/// text — before_start injects it; turn_end discards it.
fn iterate(sid: &str, cwd: &str, link: Option<&HostLink>, from_bs: bool) -> Option<String> {
    let mut st = read_state(sid)?;
    if st.get("status").and_then(Value::as_str) != Some("running") {
        purge_wakes(sid);
        return None;
    }
    let mut gate_note: Option<String> = None;
    // Review stage: poll the detached second agent; a NOVEL verdict can
    // block here on `host/ask` for the human-review step.
    if st.get("review_run").is_some() {
        match review_stage(sid, &mut st, cwd, link) {
            Some(DoneCheck::Verifying(m)) | Some(DoneCheck::Rejected(m)) => gate_note = Some(m),
            Some(DoneCheck::Complete(_)) => return None,
            Some(DoneCheck::Open) | None => {
                if review_passed(&st) {
                    gate_note = Some(
                        "second-agent review cleared — call loop_done to run the final checks"
                            .into(),
                    );
                }
            }
        }
        if st.get("status").and_then(Value::as_str) != Some("running") {
            purge_wakes(sid);
            return None;
        }
    }
    if st.get("gate_run").is_some() {
        match gate_poll(sid, &mut st) {
            GatePoll::Finished(0) => {
                let ts = tasks(&st);
                let basis = if ts.is_empty() {
                    "goal reported done".to_string()
                } else {
                    format!("all {} tasks passed", ts.len())
                };
                let gate = st
                    .get("gate")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_string();
                st["status"] = json!("done");
                st["notes"] = json!(format!("{basis}; gate `{gate}` green"));
                let _ = write_state(sid, &st);
                purge_wakes(sid);
                return None;
            }
            GatePoll::Finished(code) => {
                unpass_review(&mut st);
                let why = if code < 0 {
                    "ended without recording an exit code".to_string()
                } else {
                    format!("exit {code}")
                };
                gate_note = Some(format!(
                    "Gate FAILED ({why}) — \"done\" was rejected, keep working. Output tail:\n{}",
                    gate_tail(sid)
                ));
            }
            GatePoll::TimedOut(secs) => {
                unpass_review(&mut st);
                gate_note = Some(format!(
                    "Gate TIMED OUT after {secs}s — \"done\" was rejected, keep working. Output tail:\n{}",
                    gate_tail(sid)
                ));
            }
            GatePoll::Running(secs) => {
                gate_note = Some(format!(
                    "Gate still verifying ({secs}s of {GATE_TIMEOUT_SECS}s)."
                ));
            }
            GatePoll::Absent => {}
        }
    }
    if st.get("status").and_then(Value::as_str) != Some("running") {
        return None;
    }
    let iter = st.get("iter").and_then(Value::as_u64).unwrap_or(0);
    let max = st.get("max").and_then(Value::as_u64).unwrap_or(DEFAULT_MAX);
    if iter >= max {
        if let Some(pid) = st
            .get("gate_run")
            .and_then(|g| g.get("pid"))
            .and_then(Value::as_u64)
        {
            kill_tree(pid);
        }
        if let Some(pid) = st
            .get("review_run")
            .and_then(|g| g.get("pid"))
            .and_then(Value::as_u64)
        {
            kill_tree(pid);
        }
        st["status"] = json!("failed");
        st["notes"] = json!(format!("iteration cap ({max}) reached"));
        let _ = write_state(sid, &st);
        purge_wakes(sid);
        return None;
    }
    st["iter"] = json!(iter + 1);
    if from_bs {
        st["bs_iter"] = json!(iter + 1);
    }
    let steer: Vec<String> = st
        .get("steer")
        .and_then(Value::as_array)
        .map(|q| {
            q.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if !steer.is_empty() {
        st["steer"] = json!([]);
    }
    if write_state(sid, &st).is_err() {
        return None;
    }
    let goal = st.get("goal").and_then(Value::as_str).unwrap_or("?");
    let mut parts = vec![format!(
        "Loop iteration {} of {max} — goal: \"{goal}\"",
        iter + 1
    )];
    let ts = tasks(&st);
    if !ts.is_empty() {
        parts.push(format!("Tasks — {}", render_tasks(&ts)));
    }
    if let Some(l) = st
        .get("learnings")
        .and_then(Value::as_array)
        .filter(|l| !l.is_empty())
    {
        let shown: Vec<String> = l
            .iter()
            .rev()
            .take(3)
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        parts.push(format!(
            "Learnings (latest first):\n{}",
            shown
                .iter()
                .map(|s| format!("  - {s}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    if !steer.is_empty() {
        parts.push(format!("Steering: {}", steer.join(" | ")));
    }
    if let Some(note) = gate_note {
        parts.push(note);
    }
    if review_enabled(&st) && !review_passed(&st) {
        parts.push("Review: a detached second agent (`gray -p`) reviews the work once tasks pass — VERDICT CLEAR proceeds to the final checks; RESOLVABLE/NOVEL findings come back as steering.".to_string());
    }
    if let Some(gate) = st
        .get("gate")
        .and_then(Value::as_str)
        .filter(|g| !g.is_empty())
    {
        parts.push(format!(
            "Gate: `{gate}` must exit 0 before \"done\" is accepted."
        ));
    }
    parts.push(if ts.is_empty() {
        format!("Continue. Call loop_done when the goal is verifiably complete (no loop tools? run `{} done \"<reason>\"` in bash).", sched::cli())
    } else {
        "Continue with the next unchecked task; mark it with loop_task when it truly passes."
            .to_string()
    });
    let text = parts.join("\n");
    // Queue the next turn: the host drains cron/inbox/<sid> into a fresh
    // prompt, whose before_start queues the one after — the chain that
    // makes /loop self-driving. The prompt self-neutralizes if the loop
    // ended between queue and delivery.
    let mut wake_parts = parts.clone();
    let next_label = if from_bs { iter + 2 } else { iter + 1 };
    wake_parts[0] = format!("Loop iteration {} of {max} — goal: \"{goal}\"", next_label);
    let _ = queue_wake(
        sid,
        &format!("loop iteration {} of {max} — {goal}", next_label),
        &format!(
            "{}\n\n(This is a scheduled /loop nudge — if the loop has ended, reply briefly and take no further action.)",
            wake_parts.join("\n")
        ),
    );
    Some(text)
}

fn call_tool(
    name: &str,
    args: &Value,
    params: &Value,
    link: Option<&HostLink>,
) -> Result<String, String> {
    let sid = session_id(params);
    if sid.is_empty() {
        return Err("no session id".into());
    }
    match name {
        "loop_wakeup" => {
            if args.get("stop").and_then(Value::as_bool) == Some(true) {
                return cli_stop(
                    &sid,
                    args.get("reason").and_then(Value::as_str).unwrap_or(""),
                );
            }
            sched::wakeup(
                &sid,
                args.get("delay_seconds").and_then(Value::as_u64),
                args.get("reason").and_then(Value::as_str).unwrap_or(""),
                args.get("noop").and_then(Value::as_bool).unwrap_or(false),
            )
        }
        "loop_done" => {
            let status = args.get("status").and_then(Value::as_str).unwrap_or("");
            if !matches!(status, "done" | "failed") {
                return Err("status must be \"done\" or \"failed\"".into());
            }
            let reason = args.get("reason").and_then(Value::as_str).unwrap_or("");
            match read_state(&sid) {
                Some(mut st) if st.get("status").and_then(Value::as_str) == Some("running") => {
                    if status == "done" {
                        let ts = tasks(&st);
                        let unpassed = ts
                            .iter()
                            .filter(|t| t["passes"].as_bool() != Some(true))
                            .count();
                        if unpassed > 0 {
                            return Err(format!(
                                "{}/{} tasks still unchecked — pass them with loop_task first, or call loop_done \"failed\"",
                                ts.len() - unpassed,
                                ts.len()
                            ));
                        }
                        let basis = if ts.is_empty() {
                            "goal reported done".to_string()
                        } else {
                            format!("all {} tasks passed", ts.len())
                        };
                        return match resolve_done(&sid, &session_cwd(params), &basis, link) {
                            DoneCheck::Complete(msg) | DoneCheck::Verifying(msg) => Ok(msg),
                            DoneCheck::Rejected(msg) => Err(msg),
                            DoneCheck::Open => Err("couldn't finalize loop".into()),
                        };
                    }
                    if let Some(pid) = st
                        .get("gate_run")
                        .and_then(|g| g.get("pid"))
                        .and_then(Value::as_u64)
                    {
                        kill_tree(pid);
                    }
                    if let Some(pid) = st
                        .get("review_run")
                        .and_then(|g| g.get("pid"))
                        .and_then(Value::as_u64)
                    {
                        kill_tree(pid);
                    }
                    st.as_object_mut().map(|o| o.remove("review_run"));
                    gate_clear(&mut st);
                    let _ = write_state(&sid, &st);
                    if set_status(&sid, status, reason) {
                        Ok(format!(
                            "loop ended: {status}{}",
                            if reason.is_empty() {
                                String::new()
                            } else {
                                format!(" — {reason}")
                            }
                        ))
                    } else {
                        Err("couldn't write loop state".into())
                    }
                }
                Some(_) => Ok("loop is not running".into()),
                None => Ok("no active loop is running".into()),
            }
        }
        "loop_task" => {
            let op = args.get("op").and_then(Value::as_str).unwrap_or("");
            let Some(mut st) = read_state(&sid) else {
                return Ok("no active loop is running".into());
            };
            if st.get("status").and_then(Value::as_str) != Some("running") {
                return Ok("loop is not running".into());
            }
            let mut ts = tasks(&st);
            match op {
                "add" => {
                    let text = args
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim();
                    if text.is_empty() {
                        return Err("loop_task add needs \"text\"".into());
                    }
                    ts.push(json!({ "id": ts.len() + 1, "text": text, "passes": false }));
                    st["tasks"] = json!(ts);
                    let _ = write_state(&sid, &st);
                    Ok(format!(
                        "task {} added\n{}",
                        tasks(&st).len(),
                        render_tasks(&tasks(&st))
                    ))
                }
                "pass" | "fail" => {
                    let want_pass = op == "pass";
                    let idx = match args.get("task") {
                        Some(Value::Number(n)) => n
                            .as_u64()
                            .map(|u| u as usize)
                            .and_then(|u| u.checked_sub(1)),
                        Some(Value::String(s)) => {
                            let s = s.to_lowercase();
                            let text_of =
                                |t: &Value| t["text"].as_str().unwrap_or("").to_lowercase();
                            if let Some(i) = ts.iter().position(|t| text_of(t) == s) {
                                Some(i)
                            } else {
                                let unique = |pred: &dyn Fn(&Value) -> bool| -> Option<usize> {
                                    let hits: Vec<usize> = ts
                                        .iter()
                                        .enumerate()
                                        .filter(|(_, t)| pred(t))
                                        .map(|(i, _)| i)
                                        .collect();
                                    (hits.len() == 1).then_some(hits[0])
                                };
                                unique(&|t| text_of(t).starts_with(&s))
                                    .or_else(|| unique(&|t| text_of(t).contains(&s)))
                            }
                        }
                        _ => None,
                    };
                    let Some(i) = idx.filter(|i| *i < ts.len()) else {
                        return Err(format!(
                            "no such task — use a 1-based id, exact text, unique prefix, or unique substring\n{}",
                            render_tasks(&ts)
                        ));
                    };
                    ts[i]["passes"] = json!(want_pass);
                    let label = ts[i]["text"].as_str().unwrap_or("?").to_string();
                    st["tasks"] = json!(ts);
                    let _ = write_state(&sid, &st);
                    let marked = format!(
                        "task {} {}: {}",
                        i + 1,
                        if want_pass { "passed" } else { "reopened" },
                        label
                    );
                    if want_pass {
                        match check_done(&sid, &session_cwd(params), link) {
                            DoneCheck::Open => {}
                            DoneCheck::Verifying(msg)
                            | DoneCheck::Complete(msg)
                            | DoneCheck::Rejected(msg) => {
                                return Ok(format!("{marked}\n{msg}"));
                            }
                        }
                    }
                    Ok(format!("{marked}\n{}", render_tasks(&tasks(&st))))
                }
                "list" => Ok(render_tasks(&ts)),
                _ => Err("op must be add|pass|fail|list".into()),
            }
        }
        "loop_learn" => {
            let text = args
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            if text.is_empty() {
                return Err("loop_learn needs \"text\"".into());
            }
            let Some(mut st) = read_state(&sid) else {
                return Ok("no active loop is running".into());
            };
            let mut l = st["learnings"].as_array().cloned().unwrap_or_default();
            l.push(json!(text));
            let keep = l.split_off(l.len().saturating_sub(MAX_LEARNINGS));
            st["learnings"] = json!(keep);
            let _ = write_state(&sid, &st);
            Ok(format!("learning {} logged", keep.len()))
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

/// Sessions this sidecar served — shutdown only reaps their children and
/// wakes, never another live window's loop.
static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn note_session(params: &Value) {
    let sid = session_id(params);
    if sid.is_empty() {
        return;
    }
    if let Ok(mut v) = SEEN.lock()
        && !v.contains(&sid)
    {
        v.push(sid);
    }
}

fn shutdown_cleanup() {
    let seen = SEEN.lock().map(|v| v.clone()).unwrap_or_default();
    if let Ok(rd) = std::fs::read_dir(gray_home().join("loop")) {
        for ent in rd.flatten() {
            if !seen.iter().any(|s| *s == ent.file_name().to_string_lossy()) {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(ent.path().join("state.json"))
                && let Ok(st) = serde_json::from_str::<Value>(&text)
            {
                for key in ["gate_run", "review_run"] {
                    if let Some(pid) = st
                        .get(key)
                        .and_then(|g| g.get("pid"))
                        .and_then(Value::as_u64)
                    {
                        kill_tree(pid);
                    }
                }
            }
        }
    }
    if let Ok(rd) = std::fs::read_dir(gray_home().join("cron").join("inbox")) {
        for ent in rd.flatten() {
            if !seen.iter().any(|s| *s == ent.file_name().to_string_lossy()) {
                continue;
            }
            if let Ok(rd2) = std::fs::read_dir(ent.path()) {
                for f in rd2.flatten() {
                    if f.file_name().to_string_lossy().starts_with("loop-") {
                        let _ = std::fs::remove_file(f.path());
                    }
                }
            }
        }
    }
}

// ── wire ──────────────────────────────────────────────────────

pub fn handle_loop(req: &Value, link: Option<&HostLink>) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    note_session(&params);
    let Some(id) = id else {
        // Notifications (no id): turn_end is the lean-mode loop drive;
        // plugin/shutdown cleans up detached children before exit.
        match method {
            "event/notify" if params.get("type").and_then(Value::as_str) == Some("turn_end") => {
                on_turn_end(&params, link);
            }
            "plugin/shutdown" => {
                shutdown_cleanup();
                return (None, true);
            }
            _ => {}
        }
        return (None, false);
    };
    let result = match method {
        "plugin/manifest" => manifest_loop(),
        "agent/before_start" => before_start(&params, link),
        "command/run" => {
            // The host's argv is shlex-parsed (quotes stripped, and once
            // whitespace-split wholesale when quoting is unbalanced). `raw`
            // is the verbatim text after the command name — prefer it so a
            // goal/prompt/gate keeps apostrophes and quotes intact.
            let owned: Vec<String>;
            let argv: Vec<&str> = match params.get("raw").and_then(Value::as_str) {
                Some(raw) => {
                    owned = raw.split_whitespace().map(str::to_string).collect();
                    owned.iter().map(String::as_str).collect()
                }
                None => params
                    .get("argv")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default(),
            };
            run_command(&argv, &params)
        }
        "tool/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("args").cloned().unwrap_or(Value::Null);
            match call_tool(name, &args, &params, link) {
                Ok(text) => json!({ "content": text }),
                Err(text) => json!({ "content": text, "is_error": true }),
            }
        }
        "plugin/shutdown" => return (Some(json!({ "id": id, "result": {} })), true),
        _ => {
            let error = json!({ "code": -32601, "message": "method not found" });
            return (Some(json!({ "id": id, "error": error })), false);
        }
    };
    (Some(json!({ "id": id, "result": result })), false)
}

// ── CLI: `gray loop <verb>` — the bash path every model has ───

/// Stop whatever loop `sid` runs (either mode), killing gate/review kids.
fn cli_stop(sid: &str, reason: &str) -> Result<String, String> {
    let Some(mut st) = read_state(sid) else {
        return Err(format!("no /loop in session {sid}"));
    };
    if st.get("status").and_then(Value::as_str) != Some("running") {
        purge_wakes(sid);
        return Ok("the loop had already ended".into());
    }
    for key in ["gate_run", "review_run"] {
        if let Some(pid) = st
            .get(key)
            .and_then(|g| g.get("pid"))
            .and_then(Value::as_u64)
        {
            kill_tree(pid);
        }
    }
    st.as_object_mut().map(|o| o.remove("review_run"));
    gate_clear(&mut st);
    let sched = sched::is_sched(&st);
    st["status"] = json!(if sched { "done" } else { "failed" });
    st["notes"] = json!(if reason.is_empty() {
        "stopped by the agent".to_string()
    } else {
        reason.to_string()
    });
    write_state(sid, &st).map_err(|e| format!("couldn't write state: {e}"))?;
    purge_wakes(sid);
    Ok("loop ended — no more ticks will run".into())
}

fn cli_usage() -> String {
    let c = sched::cli();
    format!(
        "usage (session from $GRAY_SESSION_ID or --session ID):\n\
         \x20 {c} wakeup <seconds> [--reason \"why\"] [--noop]   re-arm a self-paced /loop (0..{})\n\
         \x20 {c} stop [\"reason\"]                            end the loop\n\
         \x20 {c} status\n\
         \x20 {c} done [\"reason\"] | fail [\"reason\"]          goal loop: same as loop_done\n\
         \x20 {c} task add \"text\" | pass N | fail N | list     goal loop: same as loop_task\n\
         \x20 {c} learn \"text\"                               goal loop: same as loop_learn",
        sched::MAX_WAKE
    )
}

fn cli(args: &[String]) -> i32 {
    let mut sid = std::env::var("GRAY_SESSION_ID").unwrap_or_default();
    let mut rest: Vec<String> = vec![];
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--session" {
            if let Some(v) = it.next() {
                sid = v.clone();
            }
        } else {
            rest.push(a.clone());
        }
    }
    let verb = rest.first().cloned().unwrap_or_default();
    if matches!(verb.as_str(), "" | "help" | "--help" | "-h") {
        println!("{}", cli_usage());
        return 0;
    }
    if sid.is_empty() {
        eprintln!(
            "gray loop: no session — run this from a gray bash tool call, or pass --session ID"
        );
        return 2;
    }
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let params = json!({ "session": { "id": sid, "cwd": cwd } });
    let tail = || rest[1..].join(" ");
    let res: Result<String, String> = match verb.as_str() {
        "wakeup" | "wake" | "next" => {
            let mut delay = None;
            let mut reason = String::new();
            let mut noop = false;
            let mut i = 1;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--noop" => noop = true,
                    "--reason" => {
                        reason = rest.get(i + 1).cloned().unwrap_or_default();
                        i += 1;
                    }
                    w => {
                        if delay.is_none()
                            && let Ok(n) = w.trim_end_matches('s').parse::<u64>()
                        {
                            delay = Some(n);
                        } else if reason.is_empty() {
                            reason = w.to_string();
                        } else {
                            reason = format!("{reason} {w}");
                        }
                    }
                }
                i += 1;
            }
            sched::wakeup(&sid, delay, &reason, noop)
        }
        "stop" | "end" => cli_stop(&sid, &tail()),
        "status" => read_state(&sid)
            .map(|st| {
                if sched::is_sched(&st) {
                    sched::status_line(&st)
                } else {
                    run_command(&["status"], &params)["text"]
                        .as_str()
                        .unwrap_or("")
                        .to_string()
                }
            })
            .ok_or_else(|| format!("no /loop in session {sid}")),
        "done" | "fail" | "failed" => call_tool(
            "loop_done",
            &json!({ "status": if verb == "done" { "done" } else { "failed" }, "reason": tail() }),
            &params,
            None,
        ),
        "task" => {
            let op = rest.get(1).cloned().unwrap_or_else(|| "list".into());
            let arg = rest[2.min(rest.len())..].join(" ");
            let mut a = json!({ "op": op });
            if op == "add" {
                a["text"] = json!(arg);
            } else if !arg.is_empty() {
                a["task"] = arg.parse::<u64>().map(|n| json!(n)).unwrap_or(json!(arg));
            }
            call_tool("loop_task", &a, &params, None)
        }
        "learn" => call_tool("loop_learn", &json!({ "text": tail() }), &params, None),
        other => Err(format!("unknown verb `{other}`\n{}", cli_usage())),
    };
    match res {
        Ok(t) => {
            println!("{t}");
            0
        }
        Err(t) => {
            eprintln!("{t}");
            1
        }
    }
}

const CLI_VERBS: &[&str] = &[
    "wakeup",
    "wake",
    "next",
    "stop",
    "end",
    "status",
    "done",
    "fail",
    "failed",
    "task",
    "learn",
    "help",
    "--help",
    "-h",
    "--session",
];

pub fn loop_cli(args: &[String]) -> i32 {
    if matches!(
        args.first().map(String::as_str),
        Some("help") | Some("--help") | Some("-h")
    ) {
        println!("{}", cli_usage());
        return 0;
    }
    cli(args)
}

pub fn widget() {
    let sid = std::env::var("GRAY_SESSION_ID").unwrap_or_default();
    println!("{}", sched::widget_snapshot(&sid));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(method: &str, params: Value) -> Value {
        handle_loop(
            &json!({ "id": 1, "method": method, "params": params }),
            None,
        )
        .0
        .unwrap()
    }

    /// Point state_path at a scratch GRAY_HOME for the duration of `f`.
    /// Env vars are process-global; tests touching them must serialize.
    fn with_home<T>(tag: &str, f: impl FnOnce(&std::path::Path) -> T) -> T {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("grayloop-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        unsafe { std::env::set_var("GRAY_HOME", &home) };
        let out = f(&home);
        unsafe { std::env::remove_var("GRAY_HOME") };
        let _ = std::fs::remove_dir_all(&home);
        out
    }

    #[test]
    fn manifest_claims_hook_tool_command() {
        let m = call("plugin/manifest", Value::Null)["result"].clone();
        assert_eq!(m["name"], "loop");
        assert_eq!(m["protocol"], "2.0");
        assert_eq!(m["hooks"], json!(["agent/before_start"]));
        assert_eq!(m["tools"][0]["name"], "loop_done");
    }

    #[test]
    fn loop_lifecycle_start_nudge_done() {
        with_home("life", |home| {
            let sess = json!({ "id": "s-1", "cwd": "/x" });
            // /loop <goal> → prompt + state file
            let r = call(
                "command/run",
                json!({ "name": "/loop", "argv": ["goal", "fix", "tests"], "session": sess }),
            );
            assert!(
                r["result"]["prompt"]
                    .as_str()
                    .unwrap()
                    .contains("fix tests")
            );
            let st: Value = serde_json::from_str(
                &std::fs::read_to_string(home.join("loop/s-1/state.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(st["status"], "running");
            // before_start nudges and counts
            let r = call("agent/before_start", json!({ "session": sess }));
            assert!(
                r["result"]["text"]
                    .as_str()
                    .unwrap()
                    .contains("Loop iteration 1")
            );
            // loop_done stops it
            let r = call(
                "tool/call",
                json!({
                    "name": "loop_done",
                    "args": { "status": "done", "reason": "all green" },
                    "session": sess,
                }),
            );
            assert!(r["result"]["content"].as_str().unwrap().contains("done"));
            // no more nudges
            let r = call("agent/before_start", json!({ "session": sess }));
            assert_eq!(r["result"], json!({}));
        });
    }

    #[test]
    fn iteration_cap_forces_failed() {
        with_home("cap", |_| {
            let sess = json!({ "id": "s-2" });
            call(
                "command/run",
                json!({ "name": "/loop", "argv": ["goal", "g"], "session": sess }),
            );
            call(
                "command/run",
                json!({ "name": "/loop", "argv": ["max", "2"], "session": sess }),
            );
            for expect_nudge in [true, true, false] {
                let r = call("agent/before_start", json!({ "session": sess }));
                assert_eq!(r["result"].get("text").is_some(), expect_nudge);
            }
            let st = read_state("s-2").unwrap();
            assert_eq!(st["status"], "failed");
        });
    }

    #[test]
    fn stop_and_status_and_fail_open() {
        with_home("stop", |_| {
            let sess = json!({ "id": "s-3" });
            // no loop: everything degrades to text/{}
            let r = call("agent/before_start", json!({ "session": sess }));
            assert_eq!(r["result"], json!({}));
            let r = call(
                "command/run",
                json!({ "name": "/loop", "argv": ["status"], "session": sess }),
            );
            assert!(r["result"]["text"].as_str().unwrap().contains("no loop"));
            // start then stop
            call(
                "command/run",
                json!({ "name": "/loop", "argv": ["goal", "x"], "session": sess }),
            );
            let r = call(
                "command/run",
                json!({ "name": "/loop", "argv": ["stop"], "session": sess }),
            );
            assert!(r["result"]["text"].as_str().unwrap().contains("stopped"));
            let st = read_state("s-3").unwrap();
            assert_eq!(st["status"], "failed");
        });
    }

    #[test]
    fn loop_done_rejects_bad_status() {
        let r = call(
            "tool/call",
            json!({
                "name": "loop_done", "args": { "status": "maybe" }, "session": { "id": "s-9" }
            }),
        );
        assert_eq!(r["result"]["is_error"], true);
    }

    #[test]
    fn shutdown_replies_then_exits() {
        let (reply, exit) = handle_loop(&json!({ "id": 2, "method": "plugin/shutdown" }), None);
        assert!(reply.is_some() && exit);
    }

    #[test]
    fn manifest_claims_ask_capability() {
        let m = call("plugin/manifest", Value::Null)["result"].clone();
        assert_eq!(
            m["capabilities"],
            json!(["host.ask", "host.say", "widget.override"])
        );
        assert_eq!(m["widget"], json!(true));
    }

    #[test]
    fn review_flag_parses() {
        with_home("reviewflag", |home| {
            let sess = json!({ "id": "s-r1", "cwd": "/x" });
            call(
                "command/run",
                json!({ "name": "/loop", "argv": ["fix", "--review"], "session": sess }),
            );
            let st: Value = serde_json::from_str(
                &std::fs::read_to_string(home.join("loop/s-r1/state.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(st["review"], json!(true));
            call(
                "command/run",
                json!({ "name": "/loop", "argv": ["fix", "--gate", "cargo", "test", "--review"], "session": sess }),
            );
            let st: Value = serde_json::from_str(
                &std::fs::read_to_string(home.join("loop/s-r1/state.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(st["gate"], json!("cargo test --review"));
            assert!(st.get("review").and_then(Value::as_bool) != Some(true));
            call(
                "command/run",
                json!({ "name": "/loop", "argv": ["fix", "--review", "--gate", "cargo", "test"], "session": sess }),
            );
            let st: Value = serde_json::from_str(
                &std::fs::read_to_string(home.join("loop/s-r1/state.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(st["review"], json!(true));
            assert_eq!(st["gate"], json!("cargo test"));
        });
    }

    #[test]
    fn verdict_parses() {
        assert_eq!(
            parse_verdict(
                "looks fine
VERDICT: CLEAR"
            ),
            Some(("CLEAR", String::new()))
        );
        assert_eq!(
            parse_verdict("VERDICT: RESOLVABLE — fix the null deref"),
            Some(("RESOLVABLE", "fix the null deref".to_string()))
        );
        assert_eq!(
            parse_verdict(
                "noise
VERDICT: NOVEL: spec unclear
trailing"
            ),
            Some(("NOVEL", "spec unclear".to_string()))
        );
        assert_eq!(parse_verdict("no verdict here"), None);
        assert_eq!(
            parse_verdict(
                "VERDICT: NOVEL — x
more
VERDICT: CLEAR"
            ),
            Some(("CLEAR", String::new()))
        );
    }

    /// A stub `gray` binary that prints a fixed verdict line.
    #[cfg(unix)]
    fn stub_reviewer(home: &std::path::Path, verdict_line: &str) {
        std::fs::create_dir_all(home).unwrap();
        let sh = home.join("reviewer.sh");
        std::fs::write(
            &sh,
            format!(
                "#!/bin/sh
echo 'reviewing'
echo '{verdict_line}'
"
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&sh, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        unsafe { std::env::set_var("GRAY_LOOP_REVIEW_BIN", &sh) };
    }

    #[cfg(unix)]
    fn wait_for(path: &std::path::Path) -> bool {
        for _ in 0..100 {
            if path.exists() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        false
    }

    #[cfg(unix)]
    #[test]
    fn review_clear_then_done() {
        with_home("rclear", |home| {
            let sess = json!({ "id": "s-r2", "cwd": home.to_string_lossy() });
            stub_reviewer(home, "VERDICT: CLEAR");
            call(
                "command/run",
                json!({ "name": "/loop", "argv": ["g", "--review"], "session": sess }),
            );
            let r = call(
                "tool/call",
                json!({ "name": "loop_done", "args": {"status": "done"}, "session": sess }),
            );
            assert!(r["result"]["content"].as_str().unwrap().contains("review"));
            assert!(wait_for(&home.join("loop/s-r2/review.code")));
            let r = call(
                "tool/call",
                json!({ "name": "loop_done", "args": {"status": "done"}, "session": sess }),
            );
            assert!(
                r["result"]["content"]
                    .as_str()
                    .unwrap()
                    .contains("complete")
            );
            assert_eq!(read_state("s-r2").unwrap()["status"], "done");
            unsafe { std::env::remove_var("GRAY_LOOP_REVIEW_BIN") };
        });
    }

    #[test]
    fn raw_tail_keeps_quotes_and_apostrophes() {
        with_home("rawtail", |home| {
            let sess = json!({ "id": "s-raw", "cwd": home.to_string_lossy() });
            // argv is what shlex produced; raw is the verbatim tail — raw wins.
            call(
                "command/run",
                json!({ "name": "/loop", "argv": ["goal", "fix", "it"],
                        "raw": "goal fix it's \"quoted\" thing", "session": sess }),
            );
            let st = read_state("s-raw").unwrap();
            assert_eq!(st["goal"], "fix it's \"quoted\" thing");
            call(
                "command/run",
                json!({ "name": "/loop", "argv": ["stop"], "session": sess }),
            );

            call(
                "command/run",
                json!({ "name": "/loop", "argv": ["goal", "x", "--gate", "grep", "a", "b", "f"],
                        "raw": "goal x --gate \"grep 'a b' f\"", "session": sess }),
            );
            let st = read_state("s-raw").unwrap();
            assert_eq!(st["gate"], "\"grep 'a b' f\"");
        });
    }

    #[cfg(unix)]
    #[test]
    fn resolvable_rejects_and_steers() {
        with_home("rres", |home| {
            let sess = json!({ "id": "s-r3", "cwd": home.to_string_lossy() });
            stub_reviewer(home, "VERDICT: RESOLVABLE — fix the null deref");
            call(
                "command/run",
                json!({ "name": "/loop", "argv": ["g", "--review"], "session": sess }),
            );
            call(
                "tool/call",
                json!({ "name": "loop_done", "args": {"status": "done"}, "session": sess }),
            );
            assert!(wait_for(&home.join("loop/s-r3/review.code")));
            let r = call(
                "tool/call",
                json!({ "name": "loop_done", "args": {"status": "done"}, "session": sess }),
            );
            assert_eq!(r["result"]["is_error"], true);
            assert!(
                r["result"]["content"]
                    .as_str()
                    .unwrap()
                    .contains("resolvable")
            );
            let r = call("agent/before_start", json!({ "session": sess }));
            assert!(
                r["result"]["text"]
                    .as_str()
                    .unwrap()
                    .contains("fix the null deref")
            );
            unsafe { std::env::remove_var("GRAY_LOOP_REVIEW_BIN") };
        });
    }

    #[cfg(unix)]
    #[test]
    fn novel_without_ask_channel_rejects() {
        with_home("rnov", |home| {
            let sess = json!({ "id": "s-r4", "cwd": home.to_string_lossy() });
            stub_reviewer(home, "VERDICT: NOVEL — spec is ambiguous");
            call(
                "command/run",
                json!({ "name": "/loop", "argv": ["g", "--review"], "session": sess }),
            );
            call(
                "tool/call",
                json!({ "name": "loop_done", "args": {"status": "done"}, "session": sess }),
            );
            assert!(wait_for(&home.join("loop/s-r4/review.code")));
            let r = call(
                "tool/call",
                json!({ "name": "loop_done", "args": {"status": "done"}, "session": sess }),
            );
            assert_eq!(r["result"]["is_error"], true);
            assert!(r["result"]["content"].as_str().unwrap().contains("human"));
            assert_eq!(read_state("s-r4").unwrap()["status"], "running");
            unsafe { std::env::remove_var("GRAY_LOOP_REVIEW_BIN") };
        });
    }

    #[test]
    fn interval_parsing_matches_claude_code() {
        let p = |s: &str| sched::parse_interval(&s.split_whitespace().collect::<Vec<_>>());
        assert_eq!(
            p("5m check the deploy"),
            (Some(300), "check the deploy".into())
        );
        assert_eq!(
            p("check the deploy every 20m"),
            (Some(1200), "check the deploy".into())
        );
        assert_eq!(
            p("check the deploy every 2 hours"),
            (Some(7200), "check the deploy".into())
        );
        assert_eq!(p("check every PR"), (None, "check every PR".into()));
        assert_eq!(p("fix the tests"), (None, "fix the tests".into()));
        assert_eq!(
            p("5x is not an interval"),
            (None, "5x is not an interval".into())
        );
    }

    #[test]
    fn plain_loop_prompt_is_schedule_mode_and_control_words_still_work() {
        with_home("schedroute", |_| {
            let sess = json!({ "id": "s-sr", "cwd": "/x" });
            let r = call(
                "command/run",
                json!({ "name": "/loop", "argv": ["stop", "the", "rollout"], "session": sess }),
            );
            assert!(
                r["result"]["prompt"]
                    .as_str()
                    .unwrap()
                    .contains("stop the rollout")
            );
            let st = read_state("s-sr").unwrap();
            assert_eq!(st["mode"], "schedule");
            assert_eq!(st["interval"], Value::Null);
            let r = call(
                "command/run",
                json!({ "name": "/loop", "argv": ["status"], "session": sess }),
            );
            assert!(r["result"]["text"].as_str().unwrap().contains("self-paced"));
            let r = call(
                "command/run",
                json!({ "name": "/loop", "argv": ["stop"], "session": sess }),
            );
            assert_eq!(r["result"]["text"], "loop stopped");
            assert_eq!(read_state("s-sr").unwrap()["status"], "stopped");
        });
    }

    #[test]
    fn self_paced_ends_without_rearm_and_continues_with_one() {
        with_home("selfpace", |home| {
            let sid = "s-sp";
            let sess = json!({ "id": sid, "cwd": "/x" });
            let notify = |_: ()| {
                handle_loop(
                    &json!({ "method": "event/notify", "params": { "type": "turn_end", "session": sess } }),
                    None,
                );
            };
            call(
                "command/run",
                json!({ "name": "/loop", "argv": ["poll", "ci"], "session": sess }),
            );
            // tick 1 re-arms with 0 → tick 2 queued in the inbox at once
            assert!(
                sched::wakeup(sid, Some(0), "active", false)
                    .unwrap()
                    .contains("right after")
            );
            notify(());
            std::thread::sleep(Duration::from_millis(300));
            let inbox = home.join("cron").join("inbox").join(sid);
            let files: Vec<_> = std::fs::read_dir(&inbox).unwrap().flatten().collect();
            assert_eq!(files.len(), 1, "tick 2 queued");
            let body: Value =
                serde_json::from_str(&std::fs::read_to_string(files[0].path()).unwrap()).unwrap();
            assert!(body["prompt"].as_str().unwrap().contains("tick 2"));
            let st = read_state(sid).unwrap();
            assert_eq!(st["iter"], 2);
            assert_eq!(st["in_flight"], true);
            // a user turn ends before the tick is delivered → not a tick close
            notify(());
            assert_eq!(read_state(sid).unwrap()["in_flight"], true);
            // REPL drains it; the tick's turn ends with no re-arm → loop over
            std::fs::remove_file(files[0].path()).unwrap();
            notify(());
            let st = read_state(sid).unwrap();
            assert_eq!(st["status"], "done");
            assert!(st["notes"].as_str().unwrap().contains("didn't re-arm"));
            assert!(
                sched::wakeup(sid, Some(5), "", false).is_err(),
                "can't re-arm a dead loop"
            );
        });
    }

    #[test]
    fn fixed_interval_arms_without_model_help_and_caps() {
        with_home("fixed", |_| {
            let sid = "s-fx";
            let sess = json!({ "id": sid, "cwd": "/x" });
            call(
                "command/run",
                json!({ "name": "/loop", "argv": ["10m", "check", "deploy", "--max", "2"], "session": sess }),
            );
            let st = read_state(sid).unwrap();
            assert_eq!(st["interval"], 600);
            assert_eq!(st["max"], 2);
            handle_loop(
                &json!({ "method": "event/notify", "params": { "type": "turn_end", "session": sess } }),
                None,
            );
            let st = read_state(sid).unwrap();
            assert_eq!(st["status"], "running");
            assert!(st["next_due"].as_u64().unwrap() >= now_secs() + 590);
            // fire tick 2 by hand (the timer would in 10m), then end it: cap
            sched::fire(sid, st["gen"].as_u64().unwrap());
            purge_wakes(sid);
            handle_loop(
                &json!({ "method": "event/notify", "params": { "type": "turn_end", "session": sess } }),
                None,
            );
            let st = read_state(sid).unwrap();
            assert_eq!(st["status"], "done");
            assert!(st["notes"].as_str().unwrap().contains("cap"));
        });
    }

    #[test]
    fn bare_loop_uses_loop_md() {
        with_home("loopmd", |home| {
            std::fs::create_dir_all(home).unwrap();
            std::fs::write(home.join("loop.md"), "- triage new issues\n").unwrap();
            let r = call(
                "command/run",
                json!({ "name": "/loop", "argv": [], "session": { "id": "s-md", "cwd": "" } }),
            );
            assert!(
                r["result"]["prompt"]
                    .as_str()
                    .unwrap()
                    .contains("triage new issues")
            );
        });
    }

    #[test]
    fn widget_counts_down_then_shows_the_running_tick() {
        with_home("widget", |_| {
            let sid = "s-w";
            let sess = json!({ "id": sid, "cwd": "/x" });
            call(
                "command/run",
                json!({ "name": "/loop", "argv": ["5m", "check", "stuff"], "session": sess }),
            );
            // armed wait: turn_end sets next_due + reason
            let mut st = read_state(sid).unwrap();
            st["in_flight"] = json!(false);
            st["next_due"] = json!(now_secs() + 80);
            st["next_reason"] = json!("review fixes");
            write_state(sid, &st).unwrap();
            let snap = sched::widget_snapshot(sid);
            let text = snap["text"].as_str().unwrap();
            assert!(text.contains("\u{27f3} tick 2 · wakes in 1:20"), "{text}");
            assert!(text.contains("waiting on review fixes"), "{text}");
            assert!(!text.contains('/'), "no tick cap in the area: {text}");
            assert_eq!(snap["shimmer_ranges"], json!([[1, 7, 15]]));
            // mid-tick: fire() clears next_due and stamps tick_started_at
            let mut st = read_state(sid).unwrap();
            st["in_flight"] = json!(true);
            st["tick_started_at"] = json!(now_secs() - 75);
            st.as_object_mut().unwrap().remove("next_due");
            write_state(sid, &st).unwrap();
            let snap = sched::widget_snapshot(sid);
            assert!(
                snap["text"]
                    .as_str()
                    .unwrap()
                    .contains("\u{25cf} tick 1 running  1m15s"),
                "{}",
                snap["text"]
            );
            // stop/finish clears the area
            let mut st = read_state(sid).unwrap();
            st["status"] = json!("done");
            write_state(sid, &st).unwrap();
            assert_eq!(sched::widget_snapshot(sid)["text"], "");
        });
    }

    #[test]
    fn widget_stamp_gates_the_say_fallback() {
        with_home("seen", |_| {
            assert!(!sched::widget_seen_recently("s-seen"));
            let _ = sched::widget_snapshot("s-seen");
            assert!(sched::widget_seen_recently("s-seen"));
        });
    }

    #[test]
    fn countdown_formats_as_clock() {
        assert_eq!(sched::fmt_clock(0), "0:00");
        assert_eq!(sched::fmt_clock(80), "1:20");
        assert_eq!(sched::fmt_clock(3729), "1:02:09");
    }

    #[test]
    fn shutdown_only_purges_its_own_sessions() {
        with_home("scoped", |home| {
            let other = home.join("cron").join("inbox").join("someone-else");
            std::fs::create_dir_all(&other).unwrap();
            std::fs::write(other.join("loop-1.json"), "{}").unwrap();
            call(
                "command/run",
                json!({ "name": "/loop", "argv": ["x"], "session": { "id": "mine", "cwd": "/x" } }),
            );
            shutdown_cleanup();
            assert!(other.join("loop-1.json").exists());
        });
    }
}
