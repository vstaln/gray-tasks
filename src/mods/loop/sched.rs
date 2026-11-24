//! Schedule mode — Claude Code's `/loop [interval] <prompt>` ("Kairos"),
//! rebuilt on gray's wire primitives.
//!
//! * `/loop 5m <prompt>` / `/loop <prompt> every 5 minutes` — **fixed**:
//!   the prompt re-fires every interval until stopped or the tick cap.
//! * `/loop <prompt>` — **self-paced**: the model re-arms each tick with
//!   `gray loop wakeup <secs>` (or the `loop_wakeup` tool); 0 continues
//!   at once, up to 3600 waits. No re-arm → the loop ends after that tick
//!   (pull, not push — exactly Claude Code's ScheduleWakeup contract).
//! * bare `/loop` with a `loop.md` (`<cwd>/.gray/loop.md`, then
//!   `<gray home>/loop.md`) — self-paced loop over that file.
//!
//! Plumbing: the first tick is the command's `{prompt}`. Every later tick
//! is a `cron/inbox/<sid>/loop-*.json` entry the REPL drains into a turn.
//! `event/notify turn_end` (dispatched even in lean mode) closes a tick and
//! arms the next one on a timer thread inside this sidecar. `epoch` stamps
//! each arming so a stop/restart silently cancels stale timers, and
//! `pending_file` tells a tick's own turn from a user turn that ended
//! before the queued tick was delivered.
//!
//! Loop control goes through bash (`gray loop …`, `$GRAY_SESSION_ID`
//! picks the session) because providers that bridge tools over MCP
//! (claude-sub) only advertise an allowlist — bash is the one tool every
//! model is guaranteed to have.

use super::*;
use std::collections::HashMap as Map;

pub(crate) const SCHED_MAX: u64 = 50;
const MIN_INTERVAL: u64 = 10;
const MAX_INTERVAL: u64 = 7 * 86_400;
pub(crate) const MAX_WAKE: u64 = 3600;
const LOOP_MD_CAP: usize = 25_000;

/// Serializes state read-modify-write between the wire thread and timers.
static STATE_LOCK: Mutex<()> = Mutex::new(());
/// sid → epoch of the timer armed in *this* process.
static ARMED: Mutex<Option<Map<String, u64>>> = Mutex::new(None);

pub(crate) fn is_sched(st: &Value) -> bool {
    st.get("mode").and_then(Value::as_str) == Some("schedule")
}

/// The shell command the model runs. `gray loop …` resolves through the
/// host's plugin CLI; override with `$GRAY_LOOP_CLI` (tests, odd installs).
pub(crate) fn cli() -> String {
    std::env::var("GRAY_LOOP_CLI").unwrap_or_else(|_| "gray loop".into())
}

// ── interval parsing ──────────────────────────────────────────

fn unit_secs(u: &str) -> Option<u64> {
    match u.to_ascii_lowercase().as_str() {
        "s" | "sec" | "secs" | "second" | "seconds" => Some(1),
        "m" | "min" | "mins" | "minute" | "minutes" => Some(60),
        "h" | "hr" | "hrs" | "hour" | "hours" => Some(3600),
        "d" | "day" | "days" => Some(86_400),
        _ => None,
    }
}

/// `90s` / `5m` / `2h` / `1d` — the `^\d+[smhd]$` token.
fn compact(tok: &str) -> Option<u64> {
    let i = tok.find(|c: char| !c.is_ascii_digit())?;
    let (n, u) = tok.split_at(i);
    if n.is_empty() || !matches!(u, "s" | "m" | "h" | "d") {
        return None;
    }
    n.parse::<u64>().ok()?.checked_mul(unit_secs(u)?)
}

/// Split `[interval] <prompt>` in Claude Code's priority order: a leading
/// `5m` token, else a trailing `every 5m` / `every 5 minutes` clause
/// (`check every PR` has no interval), else no interval at all.
pub(crate) fn parse_interval(words: &[&str]) -> (Option<u64>, String) {
    let join = |ws: &[&str]| ws.join(" ");
    if let Some(first) = words.first()
        && let Some(s) = compact(first)
    {
        return (Some(s), join(&words[1..]));
    }
    let n = words.len();
    if n >= 2
        && words[n - 2].eq_ignore_ascii_case("every")
        && let Some(s) = compact(words[n - 1])
    {
        return (Some(s), join(&words[..n - 2]));
    }
    if n >= 3
        && words[n - 3].eq_ignore_ascii_case("every")
        && let Ok(k) = words[n - 2].parse::<u64>()
        && let Some(u) = unit_secs(words[n - 1])
    {
        return (Some(k.saturating_mul(u)), join(&words[..n - 3]));
    }
    (None, join(words))
}

pub(crate) fn fmt_dur(secs: u64) -> String {
    match secs {
        0 => "now".into(),
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => {
            if s % 60 == 0 {
                format!("{}m", s / 60)
            } else {
                format!("{}m{}s", s / 60, s % 60)
            }
        }
        s if s < 86_400 => {
            let m = (s % 3600) / 60;
            if m == 0 {
                format!("{}h", s / 3600)
            } else {
                format!("{}h{m}m", s / 3600)
            }
        }
        s => format!("{}d", s / 86_400),
    }
}

fn loop_md(cwd: &str) -> Option<(String, PathBuf)> {
    let mut cands = vec![];
    if !cwd.is_empty() {
        cands.push(PathBuf::from(cwd).join(".gray").join("loop.md"));
    }
    cands.push(gray_home().join("loop.md"));
    for p in cands {
        if let Ok(t) = std::fs::read_to_string(&p)
            && !t.trim().is_empty()
        {
            let t = if t.len() > LOOP_MD_CAP {
                let mut cut = LOOP_MD_CAP;
                while !t.is_char_boundary(cut) {
                    cut -= 1;
                }
                let cut = t[..cut].rfind('\n').unwrap_or(cut);
                format!(
                    "{}\n> WARNING: loop.md was truncated to {LOOP_MD_CAP} bytes.",
                    &t[..cut]
                )
            } else {
                t
            };
            return Some((t, p));
        }
    }
    None
}

// ── start ─────────────────────────────────────────────────────

/// `/loop [interval] <prompt> [--max N]`. `None` = nothing to start (bare
/// `/loop` without a loop.md) — the caller shows help/status instead.
pub(crate) fn start(argv: &[&str], sid: &str, cwd: &str) -> Option<Value> {
    let mut max = SCHED_MAX;
    let mut words: Vec<&str> = vec![];
    let mut i = 0;
    while i < argv.len() {
        if argv[i] == "--max"
            && let Some(n) = argv
                .get(i + 1)
                .and_then(|s| s.parse::<u64>().ok())
                .filter(|n| *n > 0)
        {
            max = n;
            i += 2;
            continue;
        }
        words.push(argv[i]);
        i += 1;
    }
    let (interval, mut prompt) = parse_interval(&words);
    let mut source = Value::Null;
    if prompt.trim().is_empty() {
        let (text, path) = loop_md(cwd)?;
        source = json!(path.display().to_string());
        prompt = text;
    }
    let interval = interval.map(|s| s.clamp(MIN_INTERVAL, MAX_INTERVAL));
    let replaced = retire(sid);
    let _g = STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let st = json!({
        "mode": "schedule",
        "prompt": prompt,
        "interval": interval,
        "source": source,
        "status": "running",
        "iter": 1,
        "max": max,
        "in_flight": true,
        "gen": 0,
        "noop_streak": 0,
        "steer": [],
        "notes": "",
        "started_at": now_secs(),
        "tick_started_at": now_secs(),
    });
    if let Err(e) = write_state(sid, &st) {
        return Some(json!({ "text": format!("loop: couldn't write state: {e}") }));
    }
    let mut p = tick_prompt(&st, &[]);
    if replaced {
        p = format!("(This /loop replaced one that was still running.)\n\n{p}");
    }
    Some(json!({ "prompt": p }))
}

/// End whatever loop is running for `sid` (any mode) before a new start.
fn retire(sid: &str) -> bool {
    let Some(old) = read_state(sid) else {
        return false;
    };
    if old.get("status").and_then(Value::as_str) != Some("running") {
        return false;
    }
    for key in ["gate_run", "review_run"] {
        if let Some(pid) = old
            .get(key)
            .and_then(|g| g.get("pid"))
            .and_then(Value::as_u64)
        {
            kill_tree(pid);
        }
    }
    purge_wakes(sid);
    true
}

// ── tick prompt ───────────────────────────────────────────────

pub(crate) fn tick_prompt(st: &Value, steer: &[String]) -> String {
    let n = st.get("iter").and_then(Value::as_u64).unwrap_or(1);
    let max = st.get("max").and_then(Value::as_u64).unwrap_or(SCHED_MAX);
    let prompt = st.get("prompt").and_then(Value::as_str).unwrap_or("");
    let cli = cli();
    let from_md = st.get("source").and_then(Value::as_str);
    let mut out = match st.get("interval").and_then(Value::as_u64) {
        Some(i) => format!("[/loop tick {n} of {max} · every {}]", fmt_dur(i)),
        None => format!("[/loop tick {n} of {max} · self-paced]"),
    };
    out.push('\n');
    if let Some(p) = from_md {
        out.push_str(&format!("Tasks from {p}:\n"));
    }
    out.push_str(prompt.trim());
    if !steer.is_empty() {
        out.push_str(&format!(
            "\n\nSteering from the user for this tick: {}",
            steer.join(" | ")
        ));
    }
    out.push_str("\n\n---\n");
    match st.get("interval").and_then(Value::as_u64) {
        Some(i) => out.push_str(&format!(
            "This is a recurring /loop: the prompt above re-runs automatically every {} — you don't schedule it. \
             Do this tick's work, then end your turn with a one-line status. \
             If the job is finished, can't make progress, or the user asked to stop, end the loop by running \
             `{cli} stop \"<reason>\"` in bash.",
            fmt_dur(i)
        )),
        None => out.push_str(&format!(
            "This is a self-paced /loop — you decide when it runs again. Do this tick's work, then before ending your turn run ONE of these in bash:\n\
             - keep going: `{cli} wakeup <seconds> --reason \"<why this delay>\"` — 0 continues right away (active work); \
             60–{MAX_WAKE} waits (polling something external — match how fast it changes). Add `--noop` if this tick changed nothing.\n\
             - finished, blocked on the user, or asked to stop: `{cli} stop \"<reason>\"`.\n\
             If you don't re-arm this turn, the loop ends after it. (A `loop_wakeup` tool does the same if you have it.)"
        )),
    }
    out
}

// ── turn_end: close the tick, arm the next ───────────────────

pub(crate) fn on_turn_end(sid: &str, link: Option<&HostLink>) {
    let guard = STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(mut st) = read_state(sid) else {
        return;
    };
    if !is_sched(&st) || st.get("status").and_then(Value::as_str) != Some("running") {
        return;
    }
    let pending_undelivered = st
        .get("pending_file")
        .and_then(Value::as_str)
        .is_some_and(|f| inbox_dir(sid).join(f).exists());
    if st.get("in_flight").and_then(Value::as_bool) != Some(true) || pending_undelivered {
        // A user turn, not a loop tick — just make sure a timer is alive
        // in this process (the sidecar may have restarted).
        drop(guard);
        ensure_armed(sid, &st);
        return;
    }
    let obj = st.as_object_mut().expect("state object");
    obj.insert("in_flight".into(), json!(false));
    obj.remove("pending_file");
    let wake = obj.remove("wake");
    let iter = st.get("iter").and_then(Value::as_u64).unwrap_or(1);
    let max = st.get("max").and_then(Value::as_u64).unwrap_or(SCHED_MAX);
    let interval = st.get("interval").and_then(Value::as_u64);
    let (delay, reason) = match interval {
        Some(i) => (i, String::new()),
        None => match &wake {
            Some(w) => (
                w.get("delay")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .min(MAX_WAKE),
                w.get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            ),
            None => {
                finish(
                    sid,
                    &mut st,
                    &format!("tick {iter} didn't re-arm — loop ended"),
                );
                say(
                    link,
                    &format!(
                        "/loop ended after tick {iter}: the agent didn't re-arm it (`{} wakeup`). Start again with /loop.",
                        cli()
                    ),
                );
                return;
            }
        },
    };
    if iter >= max {
        finish(sid, &mut st, &format!("tick cap ({max}) reached"));
        say(
            link,
            &format!(
                "/loop ended: tick cap {max} reached (raise it with /loop max N and start again)."
            ),
        );
        return;
    }
    let noop = wake
        .as_ref()
        .and_then(|w| w.get("noop"))
        .and_then(Value::as_bool)
        == Some(true);
    let streak = if noop {
        st.get("noop_streak").and_then(Value::as_u64).unwrap_or(0) + 1
    } else {
        0
    };
    let epoch = st.get("gen").and_then(Value::as_u64).unwrap_or(0) + 1;
    let due = now_secs() + delay;
    st["noop_streak"] = json!(streak);
    st["gen"] = json!(epoch);
    st["next_due"] = json!(due);
    st["next_reason"] = json!(reason);
    if write_state(sid, &st).is_err() {
        return;
    }
    drop(guard);
    arm(sid.to_string(), epoch, due);
    if delay > 0 && streak < 2 && !widget_seen_recently(sid) {
        let why = if reason.is_empty() {
            String::new()
        } else {
            format!(" — {reason}")
        };
        say(
            link,
            &format!(
                "/loop: tick {} in {}{why}  (/loop stop to end)",
                iter + 1,
                fmt_dur(delay)
            ),
        );
    }
}

fn finish(sid: &str, st: &mut Value, note: &str) {
    st["status"] = json!("done");
    st["notes"] = json!(note);
    if let Some(o) = st.as_object_mut() {
        o.remove("next_due");
        o.remove("pending_file");
        o.remove("in_flight");
    }
    let _ = write_state(sid, st);
    purge_wakes(sid);
}

fn ensure_armed(sid: &str, st: &Value) {
    let (Some(epoch), Some(due)) = (
        st.get("gen").and_then(Value::as_u64),
        st.get("next_due").and_then(Value::as_u64),
    ) else {
        return;
    };
    if st.get("in_flight").and_then(Value::as_bool) == Some(true) {
        return;
    }
    let armed = ARMED
        .lock()
        .ok()
        .and_then(|m| m.as_ref().and_then(|m| m.get(sid).copied()));
    if armed != Some(epoch) {
        arm(sid.to_string(), epoch, due);
    }
}

/// Timer thread: sleep to `due`, then fire if `epoch` still owns the loop.
fn arm(sid: String, epoch: u64, due: u64) {
    if let Ok(mut m) = ARMED.lock() {
        m.get_or_insert_with(Map::new).insert(sid.clone(), epoch);
    }
    std::thread::spawn(move || {
        loop {
            let now = now_secs();
            if now >= due {
                break;
            }
            std::thread::sleep(Duration::from_secs((due - now).min(2)));
            // Cheap liveness check so /loop stop cancels promptly.
            match read_state(&sid) {
                Some(st)
                    if st.get("status").and_then(Value::as_str) == Some("running")
                        && st.get("gen").and_then(Value::as_u64) == Some(epoch) => {}
                _ => return,
            }
        }
        fire(&sid, epoch);
    });
}

pub(crate) fn fire(sid: &str, epoch: u64) {
    let _g = STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(mut st) = read_state(sid) else {
        return;
    };
    if !is_sched(&st)
        || st.get("status").and_then(Value::as_str) != Some("running")
        || st.get("gen").and_then(Value::as_u64) != Some(epoch)
        || st.get("in_flight").and_then(Value::as_bool) == Some(true)
    {
        return;
    }
    let iter = st.get("iter").and_then(Value::as_u64).unwrap_or(1) + 1;
    st["iter"] = json!(iter);
    let steer: Vec<String> = st
        .get("steer")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    st["steer"] = json!([]);
    let prompt = tick_prompt(&st, &steer);
    let label: String = st
        .get("prompt")
        .and_then(Value::as_str)
        .unwrap_or("")
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(80)
        .collect();
    let max = st.get("max").and_then(Value::as_u64).unwrap_or(SCHED_MAX);
    let Some(file) = queue_wake(
        sid,
        &format!("/loop tick {iter} of {max} — {label}"),
        &prompt,
    ) else {
        return;
    };
    st["in_flight"] = json!(true);
    st["pending_file"] = json!(file);
    st["tick_started_at"] = json!(now_secs());
    if let Some(o) = st.as_object_mut() {
        o.remove("next_due");
    }
    let _ = write_state(sid, &st);
}

// ── control: wakeup / stop (CLI + tool) ──────────────────────

pub(crate) fn wakeup(
    sid: &str,
    delay: Option<u64>,
    reason: &str,
    noop: bool,
) -> Result<String, String> {
    let _g = STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(mut st) = read_state(sid) else {
        return Err(format!("no /loop in session {sid}"));
    };
    if st.get("status").and_then(Value::as_str) != Some("running") {
        return Err("the loop isn't running (it ended or was stopped) — nothing to re-arm".into());
    }
    if !is_sched(&st) {
        return Err("this is a goal loop (/loop goal) — it re-prompts on its own; use `done`/`task` instead".into());
    }
    if let Some(i) = st.get("interval").and_then(Value::as_u64) {
        return Ok(format!(
            "this loop is recurring every {} — the next tick is automatic, nothing to arm",
            fmt_dur(i)
        ));
    }
    let Some(delay) = delay else {
        return Err("usage: wakeup <seconds> [--reason \"why\"] [--noop]".into());
    };
    let clamped = delay.min(MAX_WAKE);
    st["wake"] = json!({ "delay": clamped, "reason": reason, "noop": noop, "at": now_secs() });
    write_state(sid, &st).map_err(|e| format!("couldn't write state: {e}"))?;
    let note = if clamped != delay {
        format!(" (clamped from {delay}s to the {MAX_WAKE}s max)")
    } else {
        String::new()
    };
    Ok(if clamped == 0 {
        format!("armed: the next tick starts right after this turn ends{note}")
    } else {
        format!(
            "armed: next tick {} after this turn ends{note}",
            fmt_dur(clamped)
        )
    })
}

pub(crate) fn status_line(st: &Value) -> String {
    let status = st.get("status").and_then(Value::as_str).unwrap_or("?");
    let pace = match st.get("interval").and_then(Value::as_u64) {
        Some(i) => format!("every {}", fmt_dur(i)),
        None => "self-paced".into(),
    };
    let next = match (status, st.get("next_due").and_then(Value::as_u64)) {
        ("running", Some(d)) => {
            let why = st
                .get("next_reason")
                .and_then(Value::as_str)
                .filter(|r| !r.is_empty())
                .map(|r| format!(" ({r})"))
                .unwrap_or_default();
            format!(
                "\nnext tick in {}{why}",
                fmt_dur(d.saturating_sub(now_secs()))
            )
        }
        ("running", None) => "\ntick in progress".into(),
        _ => String::new(),
    };
    format!(
        "loop {status} — {pace} — tick {}/{}{next}\nprompt: {}\nnotes: {}",
        st.get("iter").and_then(Value::as_u64).unwrap_or(0),
        st.get("max").and_then(Value::as_u64).unwrap_or(SCHED_MAX),
        st.get("prompt")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .lines()
            .next()
            .unwrap_or(""),
        st.get("notes").and_then(Value::as_str).unwrap_or(""),
    )
}

/// `m:ss` (or `h:mm:ss`) countdown for the widget area — `18:42`, `1:02:09`.
pub(crate) fn fmt_clock(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

fn widget_seen_path(sid: &str) -> PathBuf {
    state_path(sid).with_file_name("widget.seen")
}

/// The host polls `<binary> widget` on a tick; the stamp tells [`say`]
/// callers a widget-capable gray is already showing the area, so the
/// transcript line stays a fallback for hosts that never poll.
fn touch_widget_seen(sid: &str) {
    let path = widget_seen_path(sid);
    let stale = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .map(|m| m.elapsed().unwrap_or_default() > Duration::from_secs(30))
        .unwrap_or(true);
    if stale {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&path, b"");
    }
}

pub(crate) fn widget_seen_recently(sid: &str) -> bool {
    std::fs::metadata(widget_seen_path(sid))
        .and_then(|m| m.modified())
        .ok()
        .is_some_and(|m| m.elapsed().unwrap_or_default() < Duration::from_secs(75))
}

/// The above-composer status area (same mechanism gray-subagents uses):
/// `loop   ⟳ tick 9 · wakes in 18:42` while armed, with the wake reason
/// on a dim second line; `loop   ● tick 9 running  1m12s` mid-tick.
/// Empty text clears the area — stop/finish needs no explicit teardown.
pub(crate) fn widget_snapshot(sid: &str) -> Value {
    touch_widget_seen(sid);
    let mut text = String::new();
    let mut ranges: Vec<(usize, usize, usize)> = Vec::new();
    if let Some(st) = read_state(sid)
        && is_sched(&st)
        && st.get("status").and_then(Value::as_str) == Some("running")
    {
        let iter = st.get("iter").and_then(Value::as_u64).unwrap_or(1);
        let now = now_secs();
        text.push('\n');
        let (label, line) = match st.get("next_due").and_then(Value::as_u64) {
            Some(due) => {
                let label = format!("\u{27f3} tick {}", iter + 1);
                (
                    label.clone(),
                    format!(
                        "loop   {label} · wakes in {}",
                        fmt_clock(due.saturating_sub(now))
                    ),
                )
            }
            None => {
                let started = st
                    .get("tick_started_at")
                    .and_then(Value::as_u64)
                    .unwrap_or(now);
                let label = format!("\u{25cf} tick {iter} running");
                (
                    label.clone(),
                    format!("loop   {label}  {}", fmt_dur(now.saturating_sub(started))),
                )
            }
        };
        ranges.push((1, 7, 7 + label.chars().count()));
        text.push_str(&line);
        text.push('\n');
        if let Some(reason) = st
            .get("next_reason")
            .and_then(Value::as_str)
            .filter(|r| !r.is_empty())
        {
            text.push_str(&format!("     waiting on {reason}\n"));
        }
        text.push('\n');
    }
    json!({"version":1,"text":text,"shimmer_lines":[],"shimmer_ranges":ranges})
}

/// host/say — fire and forget (capability `host.say`; ungranted just errors).
pub(crate) fn say(link: Option<&HostLink>, text: &str) {
    let Some(link) = link else { return };
    let id = next_id(&link.counter);
    let (tx, _rx) = mpsc::channel();
    link.pending.lock().expect("pending").insert(id.clone(), tx);
    let req = json!({ "id": id, "method": "host/say", "params": { "text": text } });
    let mut o = link.out.lock().expect("stdout");
    let _ = writeln!(o, "{req}").and_then(|_| o.flush());
}
