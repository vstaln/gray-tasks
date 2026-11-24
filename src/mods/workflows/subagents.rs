//! Driving `gray subagents` programmatically — we spawn its runs and read
//! its run ledger; supervision stays inside gray-subagents.
//!
//! Resolution order for the entry point:
//!   1. `GRAY_WORKFLOWS_SUBAGENTS_CMD` (explicit override; test hooks)
//!   2. `$GRAY_BIN subagents …` — the `gray <name>` plugin forwarder
//!   3. `gray subagents …` on PATH
//!   4. the `subagents` entry's `cli_argv` in the plugin lock (direct bin)
//!
//! Run records live at `$GRAY_HOME/subagents/runs/<run_id>.json`; the global
//! concurrency cap is `$GRAY_HOME/subagents/settings.json` `max_running`
//! (default 4) — the supervisor itself rejects over-cap launches, so the
//! orchestrator counts active records before spawning.

use anyhow::{Context, anyhow};
use serde_json::Value;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::workflows::ledger::Item;

pub const ACTIVE: &[&str] = &["running", "stopping"];
pub const TERMINAL_OK: &[&str] = &["completed", "ok"];
pub const TERMINAL_ALL: &[&str] = &[
    "completed",
    "ok",
    "failed",
    "stopped",
    "timeout",
    "lost",
    "exited",
];

pub fn gray_home() -> anyhow::Result<PathBuf> {
    std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".gray")))
        .context("cannot resolve GRAY_HOME")
}

fn runs_dir() -> anyhow::Result<PathBuf> {
    Ok(gray_home()?.join("subagents/runs"))
}

/// Global `max_running` from `$GRAY_HOME/subagents/settings.json` (default 4).
pub fn cap() -> u32 {
    let path = gray_home()
        .map(|h| h.join("subagents/settings.json"))
        .unwrap_or_default();
    std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v["max_running"].as_u64())
        .map(|n| n.clamp(1, 32) as u32)
        .unwrap_or(4)
}

/// Every run record under `subagents/runs`, parse failures skipped.
pub fn all_runs() -> Vec<Value> {
    let mut out = Vec::new();
    if let Ok(dir) = runs_dir()
        && let Ok(entries) = std::fs::read_dir(dir)
    {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) == Some("json")
                && let Ok(v) =
                    serde_json::from_slice::<Value>(&std::fs::read(&p).unwrap_or_default())
            {
                out.push(v);
            }
        }
    }
    out
}

/// Globally active runs — every workflow shares the same pool.
pub fn active_count() -> usize {
    all_runs()
        .iter()
        .filter(|j| {
            j["status"]
                .as_str()
                .map(|s| ACTIVE.contains(&s))
                .unwrap_or(false)
        })
        .count()
}

/// One run record by run_id; `None` when the file is gone or invalid.
pub fn read_run(run_id: &str) -> Option<Value> {
    let path = runs_dir().ok()?.join(format!("{run_id}.json"));
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

pub fn run_terminal(status: &str) -> bool {
    !ACTIVE.contains(&status)
}

/// The argv prefix that reaches `gray subagents`: env override, else
/// `$GRAY_BIN`/`gray` + `subagents`, else the lock's installed binary.
fn subagents_argv() -> anyhow::Result<Vec<String>> {
    if let Some(cmd) = std::env::var_os("GRAY_WORKFLOWS_SUBAGENTS_CMD") {
        let s = cmd.to_string_lossy();
        let parts: Vec<String> = s.split_whitespace().map(str::to_owned).collect();
        anyhow::ensure!(!parts.is_empty(), "GRAY_WORKFLOWS_SUBAGENTS_CMD is empty");
        return Ok(parts);
    }
    let gray = std::env::var_os("GRAY_BIN").map(PathBuf::from).or_else(|| {
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|d| d.join("gray"))
                .find(|p| p.is_file())
        })
    });
    if let Some(gray) = gray {
        return Ok(vec![
            gray.to_string_lossy().into_owned(),
            "subagents".into(),
        ]);
    }
    // Last resort: the installed plugin binary from the user lock.
    let lock = gray_home()?.join("plugins/lock.json");
    let v: Value = serde_json::from_slice(&std::fs::read(&lock)?)?;
    let argv = v["plugins"]["subagents"]["cli_argv"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .map(|s| vec![s.to_string()])
        .context("no subagents entry point: install gray-subagents")?;
    Ok(argv)
}

/// `subagents run` result: the new run_id. Spawning is synchronous — the
/// supervisor detaches and returns immediately.
pub fn launch(item: &Item, cwd: &std::path::Path, prompt: &str) -> anyhow::Result<String> {
    let mut argv = subagents_argv()?;
    argv.extend(["run".to_string(), "--name".to_string(), item.name.clone()]);
    argv.extend(["--agent".to_string(), item.agent.clone()]);
    if !item.model.is_empty() {
        argv.extend(["--model".to_string(), item.model.clone()]);
    }
    if !item.effort.is_empty() {
        argv.extend(["--effort".to_string(), item.effort.clone()]);
    }
    argv.push("--".to_string());
    argv.push(prompt.to_string());
    let (prog, args) = argv.split_first().context("empty subagents argv")?;
    let out = Command::new(prog)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("cannot spawn {prog}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    anyhow::ensure!(
        out.status.success(),
        "subagents run failed: {}",
        stderr.trim().lines().last().unwrap_or("unknown error")
    );
    let v: Value = serde_json::from_str(stdout.trim())
        .with_context(|| format!("subagents run did not return JSON: {stdout:.200}"))?;
    v["run_ids"]
        .as_array()
        .and_then(|a| a.first())
        .or_else(|| v.get("run_id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("subagents run returned no run_id"))
}

/// `subagents stop <name>` — best effort; returns true when accepted.
pub fn stop_run(name: &str) -> bool {
    let Ok(mut argv) = subagents_argv() else {
        return false;
    };
    argv.extend(["stop".to_string(), name.to_string()]);
    let Some((prog, args)) = argv.split_first() else {
        return false;
    };
    Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_sets_cover_terminals() {
        for s in [
            "completed",
            "failed",
            "stopped",
            "timeout",
            "lost",
            "exited",
        ] {
            assert!(run_terminal(s), "{s}");
        }
        for s in ["running", "stopping"] {
            assert!(!run_terminal(s), "{s}");
        }
    }
}
