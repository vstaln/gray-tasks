//! The independent completion auditor: `/goal audit` spawns a detached
//! `gray-goal audit-run` child so a `command/run` reply never waits on the
//! check. `audit-run` invokes the auditor — `GRAY_GOAL_AUDITOR` (a `sh -c`
//! command, test seam) or `gray --bare -p` (default: a fresh agent with
//! blocking bash, no plugins — the grader is not the executor) — captures
//! its output, parses the `VERDICT: PASS|FAIL` line, and writes the result
//! into the goal doc's frontmatter + `## Audit log`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};

use crate::goal::{Audit, Goal, Status, Store, now_iso};

/// Kill the auditor after this; a wedged child must not pin the verdict.
pub const AUDIT_TIMEOUT: Duration = Duration::from_secs(600);
/// Auditor output kept in `<id>.audit.log` (tail, not head: the verdict
/// and its evidence sit at the end).
const LOG_KEEP: usize = 16 * 1024;

/// Reviewer prompt handed to the fresh auditor process. States the verdict
/// contract the parser below relies on.
pub fn audit_prompt(goal: &Goal, cwd: &Path) -> String {
    let mut items = String::new();
    for (n, item) in goal.items.iter().enumerate() {
        items.push_str(&format!(
            "{}. [{}] {}\n",
            n + 1,
            if item.checked { "x" } else { " " },
            item.text
        ));
    }
    if items.is_empty() {
        items.push_str("(checklist is empty)\n");
    }
    format!(
        "You are an independent completion auditor. You did NOT do the work under review; grade only what the repository state proves. Never trust the checklist itself.\n\
        \n\
        Goal {id}: {title}\n\
        Objective:\n{desc}\n\
        \n\
        Executor's checklist (claims):\n{items}\
        \n\
        Working directory: {cwd}\n\
        \n\
        For every CHECKED item, verify the claim against the actual files and behavior in the working directory — read the files, grep, run the tests or builds the item implies. Still-unchecked items are not claims; list them as OPEN instead.\n\
        \n\
        Report exactly one line per item:\n\
        ITEM <n>: PASS|FAIL|OPEN — <evidence you found or what is missing>\n\
        \n\
        Then finish with exactly one final line:\n\
        VERDICT: PASS\n\
        or\n\
        VERDICT: FAIL\n\
        Verdict is FAIL when any checked item lacks real evidence or any claimed behavior does not actually work; PASS only when every checked item verifies.",
        id = goal.id,
        title = goal.title,
        desc = goal.desc,
        items = items,
        cwd = cwd.display(),
    )
}

/// Last `VERDICT: PASS|FAIL` in the output (case-insensitive). None when the
/// auditor never reached one — that maps to `Audit::Error`, not a guess.
pub fn parse_verdict(output: &str) -> Option<bool> {
    let mut verdict = None;
    for line in output.lines() {
        let t = line.trim();
        let upper = t.to_ascii_uppercase();
        if let Some(rest) = upper.strip_prefix("VERDICT:") {
            let word = rest.trim();
            if word.starts_with("PASS") {
                verdict = Some(true);
            } else if word.starts_with("FAIL") {
                verdict = Some(false);
            }
        }
    }
    verdict
}

/// One-line audit summary for the goal's log: per-item tallies when present,
/// else a clipped tail line of the auditor output.
fn verdict_summary(output: &str) -> String {
    let pass = output.matches("ITEM ").count(); // total graded lines
    let fails = output
        .lines()
        .filter(|l| {
            let u = l.trim().to_ascii_uppercase();
            u.starts_with("ITEM") && u.contains(": FAIL")
        })
        .count();
    if pass > 0 {
        return format!("{}/{pass} item lines passed", pass - fails);
    }
    output
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("(no output)")
        .chars()
        .take(120)
        .collect()
}

/// `gray-goal audit-run <id> --cwd <dir>`: blocking child of `/goal audit`.
/// Pipes the auditor, then commits the verdict into the goal doc.
pub fn audit_run(home: &Path, id: &str, cwd: &Path) -> anyhow::Result<String> {
    let store = Store::new(home)?;
    let Some(goal) = store.find(id) else {
        bail!("no goal matching '{id}'");
    };
    let prompt = audit_prompt(&goal, cwd);
    let log_path = goal.path(home).with_extension("audit.log");

    let mut cmd = auditor_command(&prompt);
    cmd.current_dir(cwd)
        .env("GRAY_GOAL_ID", &goal.id)
        .env("GRAY_GOAL_FILE", goal.path(home))
        .env("GRAY_GOAL_CWD", cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("spawn auditor")?;

    // Drain pipes on threads so a chatty auditor can't deadlock on a full
    // stderr buffer while we wait on a timeout.
    let mut out_pipe = child.stdout.take().expect("piped");
    let mut err_pipe = child.stderr.take().expect("piped");
    let out_t = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = out_pipe.read_to_end(&mut b);
        b
    });
    let err_t = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = err_pipe.read_to_end(&mut b);
        b
    });

    let start = Instant::now();
    let status = loop {
        match child.try_wait()? {
            Some(s) => break Some(s),
            None if start.elapsed() > AUDIT_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => std::thread::sleep(Duration::from_millis(200)),
        }
    };
    let mut output = String::from_utf8_lossy(&out_t.join().unwrap_or_default()).into_owned();
    let err = String::from_utf8_lossy(&err_t.join().unwrap_or_default()).into_owned();
    if !err.trim().is_empty() {
        output.push_str("\n--- stderr ---\n");
        output.push_str(&err);
    }
    let tail: String = output
        .chars()
        .rev()
        .take(LOG_KEEP)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let _ = std::fs::write(&log_path, &tail);

    let mut g = goal;
    let verdict = match (status, parse_verdict(&output)) {
        (None, _) => ("timeout", Audit::Error),
        (Some(s), _) if !s.success() => ("error", Audit::Error),
        (Some(_), Some(true)) => ("pass", Audit::Pass),
        (Some(_), Some(false)) => ("fail", Audit::Fail),
        (Some(_), None) => ("error", Audit::Error),
    };
    let note = match status {
        None => "auditor exceeded 600s".to_string(),
        Some(s) if !s.success() => format!("auditor exited {s}"),
        _ if verdict.1 == Audit::Error => "auditor produced no VERDICT line".to_string(),
        _ => verdict_summary(&output),
    };
    g.audit = verdict.1;
    g.claim_blocked = false;
    g.audit_log
        .push(format!("{} verdict={} — {note}", now_iso(), verdict.0));
    store.update(&mut g, |_| {})?;
    Ok(format!(
        "goal {} audit: {} ({note}) — log {}",
        g.id,
        verdict.1.as_str(),
        log_path.display()
    ))
}

/// The auditor invocation. `GRAY_GOAL_AUDITOR` wins (tests, custom runners);
/// otherwise a bare `gray -p` — no plugins, so the plugin can't grade itself.
fn auditor_command(prompt: &str) -> Command {
    if let Ok(spec) = std::env::var("GRAY_GOAL_AUDITOR") {
        let (shell, flag) = if cfg!(windows) {
            ("cmd", "/C")
        } else {
            ("sh", "-c")
        };
        let mut c = Command::new(shell);
        c.args([flag, &spec]);
        return c;
    }
    let gray = std::env::var("GRAY_BIN").unwrap_or_else(|_| "gray".to_string());
    let mut c = Command::new(gray);
    c.args(["--bare", "-p", prompt]);
    c
}

/// `/goal audit` (wire path): mark the goal `audit: running`, spawn a
/// detached `audit-run`, return immediately — `command/run` has a 30s host
/// TTL and a real audit never fits it.
pub fn kickoff(store: &Store, goal: &Goal, cwd: &Path) -> anyhow::Result<String> {
    if goal.status == Status::Dropped {
        bail!("goal {} is dropped", goal.id);
    }
    if goal.audit == Audit::Running {
        bail!("goal {} already has an audit running", goal.id);
    }
    let mut g = goal.clone();
    store.update(&mut g, |g| {
        g.audit = Audit::Running;
        g.claim_blocked = false;
    })?;
    let exe = std::env::current_exe().context("current_exe")?;
    let log = goals_log(store, &g.id);
    let out = std::fs::File::create(&log).unwrap_or_else(|_| {
        std::fs::File::create(std::env::temp_dir().join("gray-goal-audit.log")).expect("audit log")
    });
    let err = out.try_clone().context("clone log fd")?;
    Command::new(exe)
        .args(["audit-run", &g.id, "--cwd"])
        .arg(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .spawn()
        .context("spawn audit-run")?;
    Ok(format!(
        "audit started for {} — an independent reviewer is checking every checked item against {} . \
         `/goal show` and next turn's context carry the verdict; full output lands in {}.",
        g.id,
        cwd.display(),
        log.display()
    ))
}

fn goals_log(store: &Store, id: &str) -> PathBuf {
    crate::goal::goals_dir(&store.home).join(format!("{id}.audit-run.log"))
}
