//! `gray_workflows` — batch/durable multi-agent runs (port of pi's
//! `@quintinshaw/pi-dynamic-workflows`).
//!
//! `/workflow <specfile>` parses a yaml/json task list, writes a ledger at
//! `.gray/workflows/<id>.json`, and hands off to a detached orchestrator
//! (`gray-workflows orchestrate`) that fans items out over
//! `gray subagents run` — respecting the global `max_running` pool cap and
//! an optional per-workflow `max_running`, isolating `worktree: true`
//! items in `git worktree` branches, gating later `phase`s on earlier
//! results, and writing `.gray/workflows/<id>.md` when everything lands.
//! A crashed orchestrator is picked up by `resume`, which reconciles item
//! state against the subagents run ledger.
//!
//! `/deep-research <topic>` is the pi preset: three parallel research
//! passes plus a synthesis item fed their results. TUI and dynamic
//! re-planning are deliberately out of scope — `status`/`report` are
//! plain text.

#[path = "workflows/ledger.rs"]
pub mod ledger;
#[path = "workflows/orchestrate.rs"]
pub mod orchestrate;
#[path = "workflows/sidecar.rs"]
pub mod sidecar;
#[path = "workflows/spec.rs"]
pub mod spec;
#[path = "workflows/subagents.rs"]
pub mod subagents;
#[path = "workflows/worktree.rs"]
pub mod worktree;
pub use sidecar::{handle_workflows, main_workflows};

use anyhow::Context;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const PLUGIN_NAME: &str = "workflows";
pub const PROTOCOL: &str = "1.1";

const USAGE: &str = "gray-workflows — batch subagent runs: /workflow <spec.yaml> · /workflows [status|resume|stop|report] · /deep-research <topic>";

/// `plugin/manifest` result — discord-shaped (protocol 1.1, `sidecar_args`).
pub fn manifest_workflows() -> Value {
    json!({
        "name": PLUGIN_NAME,
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": PROTOCOL,
        "sidecar_args": ["sidecar"],
        "tools": [],
        "commands": ["/workflow", "/workflows", "/deep-research"],
        "command_descriptions": {"/workflow": "run a multi-step workflow", "/workflows": "list saved workflows", "/deep-research": "research a question across the web"},
        "subcommands": ["workflows"],
        "hooks": ["prompt/context"],
        "completion": ["run", "status", "list", "resume", "stop", "report", "deep-research", "help"],
    })
}

/// Spawn `<self> orchestrate <ledger>` detached (setsid, output to the
/// workflow log). Returns the child pid for the ledger.
pub fn spawn_orchestrator(ledger_path: &Path, cwd: &Path) -> anyhow::Result<u32> {
    let exe = std::env::current_exe().context("cannot locate own executable")?;
    let id = ledger_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("wf");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(ledger::log_path(cwd, id))?;
    let err = log.try_clone()?;
    let mut cmd = Command::new(exe);
    cmd.arg("orchestrate")
        .arg(ledger_path)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(err);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                // Own session: survive the sidecar/host exiting.
                libc::setsid();
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP: no console, out of
        // our process group — the same "survive the host exiting" as setsid.
        cmd.creation_flags(0x00000008 | 0x00000200);
    }
    let child = cmd.spawn().context("cannot spawn orchestrator")?;
    Ok(child.id())
}

fn start(
    cwd: &Path,
    s: spec::Spec,
    spec_path: &str,
    max_running: Option<u32>,
) -> anyhow::Result<Value> {
    let mut s = s;
    if let Some(cap) = max_running {
        s.max_running = Some(cap);
    }
    let id = ledger::new_id(cwd)?;
    if s.name.is_none() {
        s.name = Some(if spec_path.is_empty() {
            id.clone()
        } else {
            Path::new(spec_path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(&id)
                .to_string()
        });
    }
    let l = ledger::new_ledger(&id, &s, cwd, spec_path, subagents::cap())?;
    let lp = ledger::path(cwd, &id);
    ledger::save(&lp, &l)?;
    let pid = spawn_orchestrator(&lp, cwd)?;
    let phases = l.items.iter().map(|i| i.phase).max().unwrap_or(0) + 1;
    Ok(json!({"text": format!(
        "workflow {} '{}' started: {} item{} across {} phase{}, ledger {}, report lands at {} (orchestrator pid {pid})",
        id, l.name, l.items.len(), if l.items.len()==1 {""} else {"s"},
        phases, if phases==1 {""} else {"s"},
        lp.display(), ledger::report_path(cwd, &id).display(),
    )}))
}

/// `/workflow <specfile>` / `gray workflows run <specfile>`.
pub fn run_workflows(
    cwd: &Path,
    specfile: &str,
    max_running: Option<u32>,
) -> anyhow::Result<Value> {
    let path = {
        let p = PathBuf::from(specfile);
        if p.is_absolute() { p } else { cwd.join(p) }
    };
    let s = spec::parse_file(&path)?;
    start(cwd, s, &path.to_string_lossy(), max_running)
}

/// `/deep-research <topic>` — the pi preset over the same machinery.
pub fn deep_research(cwd: &Path, topic: &str) -> anyhow::Result<Value> {
    anyhow::ensure!(!topic.trim().is_empty(), "usage: /deep-research <topic>");
    start(cwd, spec::deep_research(topic.trim()), "", None)
}

/// `workflows status [id]` — ledger item table.
pub fn status(cwd: &Path, id: Option<&str>) -> anyhow::Result<Value> {
    let l = ledger::resolve(cwd, id)?;
    Ok(json!({"text": orchestrate::status_text(&l)}))
}

/// `workflows list` — every ledger under this cwd.
pub fn list(cwd: &Path) -> anyhow::Result<Value> {
    let all = ledger::list(cwd);
    if all.is_empty() {
        return Ok(json!({"text": "no workflows in this directory"}));
    }
    let mut s = String::new();
    for l in &all {
        s.push_str(&format!(
            "{}  {:<10} {}/{}  {}\n",
            l.id,
            l.status,
            l.completed_items(),
            l.items.len(),
            l.name
        ));
    }
    Ok(json!({"text": s}))
}

/// `workflows stop <id>` — flag the ledger and stop its running runs.
pub fn stop(cwd: &Path, id: &str) -> anyhow::Result<Value> {
    let mut l = ledger::resolve(cwd, Some(id))?;
    anyhow::ensure!(l.status == "running", "workflow {} already finished", l.id);
    l.stop = true;
    let mut stopped = 0;
    for item in l.items.iter().filter(|i| i.status == "running") {
        if subagents::stop_run(&item.name) {
            stopped += 1;
        }
    }
    ledger::save(&ledger::path(cwd, &l.id), &l)?;
    Ok(json!({"text": format!(
        "workflow {} stop requested ({stopped} running item{} signalled)",
        l.id, if stopped == 1 { "" } else { "s" }
    )}))
}

/// `workflows resume <id>` — reconcile then respawn the orchestrator.
pub fn resume(cwd: &Path, id: &str) -> anyhow::Result<Value> {
    let mut l = ledger::resolve(cwd, Some(id))?;
    anyhow::ensure!(
        !(l.status == "running" && l.orchestrator_alive()),
        "workflow {} is already running",
        l.id
    );
    // Items that never finished — stopped, failed, timed out, lost — go
    // back to pending; completed items are kept. (A still-running item is
    // adopted by the orchestrator's reconcile pass on its first tick.)
    for item in &mut l.items {
        if item.terminal() && !item.ok() {
            item.status = "pending".into();
            item.run_id.clear();
            item.result.clear();
            item.error.clear();
            item.started = 0.0;
            item.finished = 0.0;
        }
    }
    anyhow::ensure!(
        l.items.iter().any(|i| !i.terminal()),
        "workflow {} already finished ({})",
        l.id,
        l.status
    );
    l.stop = false;
    l.status = "running".into();
    l.finished = 0.0;
    let lp = ledger::path(cwd, &l.id);
    ledger::save(&lp, &l)?;
    let pid = spawn_orchestrator(&lp, cwd)?;
    Ok(json!({"text": format!(
        "workflow {} resumed: {} pending / {} running / {} done (orchestrator pid {pid})",
        l.id,
        l.items.iter().filter(|i| i.status == "pending").count(),
        l.items.iter().filter(|i| i.status == "running").count(),
        l.completed_items(),
    )}))
}

/// `workflows report <id>` — print the aggregate report (or its tail so far).
pub fn report(cwd: &Path, id: &str) -> anyhow::Result<Value> {
    let l = ledger::resolve(cwd, Some(id))?;
    let path = if l.report.is_empty() {
        ledger::report_path(cwd, &l.id)
    } else {
        PathBuf::from(&l.report)
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(json!({"text": text})),
        Err(_) => Ok(json!({"text": format!(
            "workflow {} is {} — the report is written when it finishes\n{}",
            l.id, l.status, orchestrate::status_text(&l)
        )})),
    }
}

/// `prompt/context` — surface finished workflows once (`.noticed` marker)
/// and a one-line status for live ones.
pub fn prompt_context(cwd: &Path) -> anyhow::Result<Value> {
    let mut lines = vec![USAGE.to_string()];
    for l in ledger::list(cwd) {
        if l.status == "running" {
            if l.orchestrator_alive() {
                lines.push(format!(
                    "workflow {} '{}' running: {}/{} done — `gray workflows status {}` for detail",
                    l.id,
                    l.name,
                    l.completed_items(),
                    l.items.len(),
                    l.id
                ));
            } else {
                lines.push(format!(
                    "workflow {} '{}' stalled (orchestrator gone): `gray workflows resume {}`",
                    l.id, l.name, l.id
                ));
            }
        } else if !ledger::noticed_path(cwd, &l.id).exists() {
            let failed = l.items.len() - l.completed_items();
            lines.push(format!(
                "workflow {} '{}' {}: {}/{} items completed{} — report at {}",
                l.id,
                l.name,
                l.status,
                l.completed_items(),
                l.items.len(),
                if failed > 0 {
                    format!(", {failed} failed/stopped")
                } else {
                    String::new()
                },
                if l.report.is_empty() {
                    ledger::report_path(cwd, &l.id).display().to_string()
                } else {
                    l.report.clone()
                },
            ));
            let _ = std::fs::write(ledger::noticed_path(cwd, &l.id), b"");
        }
    }
    Ok(json!({"text": lines.join("\n")}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_shape() {
        let m = manifest_workflows();
        assert_eq!(m["name"], "workflows");
        assert_eq!(m["protocol"], "1.1");
        assert_eq!(m["sidecar_args"], json!(["sidecar"]));
        assert_eq!(
            m["commands"],
            json!(["/workflow", "/workflows", "/deep-research"])
        );
        assert_eq!(m["hooks"], json!(["prompt/context"]));
        assert!(m["tools"].as_array().unwrap().is_empty());
    }

    #[test]
    fn ledger_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let s = spec::parse("items:\n  - prompt: one\n  - prompt: two\n").unwrap();
        let l = ledger::new_ledger("abcd1234", &s, dir.path(), "spec.yaml", 4).unwrap();
        let p = ledger::path(dir.path(), "abcd1234");
        ledger::save(&p, &l).unwrap();
        let back = ledger::load(&p).unwrap();
        assert_eq!(back.items.len(), 2);
        assert_eq!(back.items[0].name, "wf-abcd1234-0");
        assert_eq!(back.items[1].status, "pending");
        assert!(back.status == "running");
    }
}
