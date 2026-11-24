//! The detached orchestrator: `gray-workflows orchestrate <ledger.json>`.
//!
//! Spawned by `run`/`resume`/`deep-research` via setsid; survives the host
//! session. One tick of the loop: reconcile running items against
//! `subagents/runs` records → launch pending items inside the global and
//! per-workflow caps → save → sleep. Exits when every item is terminal,
//! then writes `.gray/workflows/<id>.md`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::workflows::ledger::{self, Item, Ledger, WorktreeInfo};
use crate::workflows::{subagents, worktree};

const POLL: Duration = Duration::from_secs(2);
const MAX_ATTEMPTS: u32 = 5;
/// Per-item result excerpt injected into later-phase prompts.
const CTX_PER_ITEM: usize = 8000;
const CTX_TOTAL: usize = 48000;

fn sync_from_record(item: &mut Item, rec: &serde_json::Value) {
    item.status = rec["status"].as_str().unwrap_or("lost").to_string();
    item.result = rec["result"].as_str().unwrap_or_default().to_string();
    item.error = rec["error"].as_str().unwrap_or_default().to_string();
    item.finished = rec["finished"].as_f64().unwrap_or_else(ledger::now);
    if let Some(wt) = &mut item.wt {
        worktree::finalize(wt);
    }
}

/// Pull each running item's terminal state out of its run record. A
/// missing record means the run was never accepted (or its record was
/// cleaned) — the item goes back to pending for relaunch.
fn reconcile(l: &mut Ledger) {
    for item in &mut l.items {
        if item.status != "running" {
            continue;
        }
        match subagents::read_run(&item.run_id) {
            Some(rec) if subagents::run_terminal(rec["status"].as_str().unwrap_or("")) => {
                sync_from_record(item, &rec);
            }
            Some(_) => {}
            None => {
                item.status = "pending".into();
                item.run_id.clear();
            }
        }
    }
}

/// Prior-phase results block for items behind a phase gate.
fn prior_results(l: &Ledger, phase: u32) -> String {
    if phase == 0 {
        return String::new();
    }
    let mut out = String::from("\n\n## Results from earlier workflow items\n");
    for item in l.items.iter().filter(|i| i.phase < phase) {
        let body: String = item.result.chars().take(CTX_PER_ITEM).collect::<String>()
            + if item.result.len() > CTX_PER_ITEM {
                "\n…(truncated)"
            } else {
                ""
            };
        let line = if item.ok() {
            body
        } else {
            format!("(item {}: {} — {})", item.name, item.status, item.error)
        };
        out.push_str(&format!("\n### {}\n{line}\n", item.name));
        if out.len() > CTX_TOTAL {
            out.push_str("\n…(earlier results truncated)\n");
            break;
        }
    }
    out
}

/// Worktree for this item (create once, reuse on resume), or the project
/// cwd when the item doesn't ask for isolation. Returns the launch cwd.
fn ensure_worktree(repo: Option<&Path>, wf_id: &str, wf_cwd: &str, item: &mut Item) -> PathBuf {
    if !item.worktree {
        return PathBuf::from(wf_cwd);
    }
    if item.wt.is_none() {
        item.wt = repo.and_then(|r| worktree::create(r, wf_id, item.idx));
        if item.wt.is_none() {
            item.wt = Some(WorktreeInfo {
                path: String::new(),
                branch: String::new(),
                repo: String::new(),
                base_commit: String::new(),
                commits: 0,
                dirty: false,
                pruned: false,
                note: "worktree unavailable (not a git repo or unborn HEAD); ran in project cwd"
                    .into(),
            });
        }
    }
    item.wt
        .as_ref()
        .filter(|w| !w.path.is_empty())
        .map(|w| PathBuf::from(&w.path))
        .unwrap_or_else(|| PathBuf::from(wf_cwd))
}

fn launch_one(prior: &str, item: &mut Item, cwd: &Path) {
    item.attempts += 1;
    let mut prompt = item.prompt.clone();
    prompt.push_str(prior);
    if let Some(wt) = item.wt.as_ref().filter(|w| !w.path.is_empty()) {
        prompt.push_str(&worktree::context_note(wt));
    }
    match subagents::launch(item, cwd, &prompt) {
        Ok(run_id) => {
            item.run_id = run_id;
            item.status = "running".into();
            item.started = ledger::now();
            item.last_error.clear();
        }
        Err(e) => {
            let msg = e.to_string();
            item.last_error = msg.clone();
            // A full pool is transient — retry forever; real errors count.
            if !msg.contains("cap") && item.attempts >= MAX_ATTEMPTS {
                item.status = "failed".into();
                item.error = format!("launch failed after {} attempts: {msg}", item.attempts);
                item.finished = ledger::now();
            }
        }
    }
}

/// Aggregate report: `.gray/workflows/<id>.md`.
fn write_report(l: &Ledger) -> String {
    let mut md = format!(
        "# Workflow {} (`{}`)\n\n- status: **{}**\n- items: {} ({} completed, {} failed/stopped)\n- cwd: `{}`\n- started: {}\n",
        l.name,
        l.id,
        l.status,
        l.items.len(),
        l.completed_items(),
        l.items.len() - l.completed_items(),
        l.cwd,
        l.created as u64,
    );
    if l.finished > 0.0 {
        md.push_str(&format!("- duration: {:.0}s\n", l.finished - l.created));
    }
    if !l.spec_path.is_empty() {
        md.push_str(&format!("- spec: `{}`\n", l.spec_path));
    }
    md.push_str("\n## Items\n\n| # | name | agent | status | time | worktree |\n|---|---|---|---|---|---|\n");
    for i in &l.items {
        let dur = if i.finished > 0.0 && i.started > 0.0 {
            format!("{:.0}s", i.finished - i.started)
        } else {
            "-".into()
        };
        let wt =
            i.wt.as_ref()
                .filter(|w| !w.path.is_empty())
                .map(|w| {
                    if w.pruned {
                        "pruned (clean)".to_string()
                    } else {
                        format!(
                            "{} · {} commit{}{}",
                            w.branch,
                            w.commits,
                            if w.commits == 1 { "" } else { "s" },
                            if w.dirty { " · dirty" } else { "" }
                        )
                    }
                })
                .or_else(|| i.wt.as_ref().map(|w| w.note.clone()))
                .unwrap_or_default();
        md.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            i.idx, i.name, i.agent, i.status, dur, wt
        ));
    }
    md.push_str("\n## Results\n");
    for i in &l.items {
        md.push_str(&format!("\n### {} — {}\n\n", i.name, i.status));
        let body = if i.result.is_empty() {
            if i.error.is_empty() {
                "(no result)".to_string()
            } else {
                format!("error: {}", i.error)
            }
        } else {
            i.result.clone()
        };
        let truncated: String = body.chars().take(CTX_PER_ITEM).collect();
        md.push_str(&truncated);
        if body.len() > CTX_PER_ITEM {
            md.push_str("\n\n…(truncated)\n");
        }
        md.push('\n');
    }
    md
}

/// Re-read the ledger's `stop` flag from disk: a `/workflows stop` can land
/// while `launch_one` blocks on the leaf call, and the in-memory copy must
/// not clobber it on the next save.
fn merge_stop(l: &mut crate::workflows::ledger::Ledger, ledger_path: &Path) {
    if let Ok(fresh) = ledger::load(ledger_path) {
        l.stop |= fresh.stop;
    }
}

/// One supervisor pass over the ledger. The only writer of the file.
pub fn orchestrate(ledger_path: &Path) -> anyhow::Result<()> {
    let mut l = ledger::load(ledger_path)?;
    l.pid = std::process::id();
    ledger::save(ledger_path, &l)?;
    loop {
        reconcile(&mut l);
        let repo = worktree::repo_root(Path::new(&l.cwd));
        if !l.stop && !l.done() {
            let phase = l.current_phase();
            loop {
                let free_wf = (l.max_running as usize).saturating_sub(l.running_items());
                let free_global =
                    (subagents::cap() as usize).saturating_sub(subagents::active_count());
                if free_wf.min(free_global) == 0 {
                    break;
                }
                let Some(idx) = l
                    .items
                    .iter()
                    .position(|i| i.status == "pending" && i.phase == phase)
                else {
                    break;
                };
                let prior = prior_results(&l, phase);
                let item = &mut l.items[idx];
                let cwd = ensure_worktree(repo.as_deref(), &l.id, &l.cwd, item);
                item.cwd = cwd.to_string_lossy().into_owned();
                launch_one(&prior, item, &cwd);
                // Persist the run_id immediately — a crash between launch
                // and save would orphan the subagent run.
                merge_stop(&mut l, ledger_path);
                ledger::save(ledger_path, &l)?;
            }
        }
        if l.done() || (l.stop && l.running_items() == 0) {
            break;
        }
        merge_stop(&mut l, ledger_path);
        ledger::save(ledger_path, &l)?;
        std::thread::sleep(POLL);
        l = ledger::load(ledger_path)?;
        l.pid = std::process::id();
    }
    l.finished = ledger::now();
    l.pid = 0;
    l.status = if l.stop {
        "stopped".into()
    } else if l.items.iter().all(|i| i.ok()) {
        "completed".into()
    } else {
        "failed".into()
    };
    let report = write_report(&l);
    let rpath = ledger::report_path(Path::new(&l.cwd), &l.id);
    let _ = std::fs::write(&rpath, &report);
    l.report = rpath.to_string_lossy().into_owned();
    ledger::save(ledger_path, &l)?;
    Ok(())
}

/// `workflows status [id]` text — the same row language the report uses.
pub fn status_text(l: &Ledger) -> String {
    let mut s = format!(
        "workflow {} — {} — {}/{} done",
        l.id,
        l.status,
        l.completed_items(),
        l.items.len()
    );
    if l.name != l.id {
        s.push_str(&format!(" — {}", l.name));
    }
    if l.status == "running" && !l.orchestrator_alive() {
        s.push_str(" — orchestrator not alive; `gray workflows resume ");
        s.push_str(&l.id);
        s.push('`');
    }
    if l.stop {
        s.push_str(" — stop requested");
    }
    s.push('\n');
    for i in &l.items {
        let dur = if i.finished > 0.0 && i.started > 0.0 {
            format!("{:.0}s", i.finished - i.started)
        } else {
            String::new()
        };
        let extra =
            i.wt.as_ref()
                .map(|w| {
                    if !w.path.is_empty() {
                        if w.pruned {
                            " · worktree pruned".to_string()
                        } else {
                            format!(" · {}", w.branch)
                        }
                    } else {
                        format!(" · {}", w.note)
                    }
                })
                .unwrap_or_default();
        let err = if i.error.is_empty() {
            i.last_error.clone()
        } else {
            i.error.clone()
        };
        s.push_str(&format!(
            "  {} {:<10} {:<9} {}{}\n",
            i.name, i.status, dur, i.agent, extra
        ));
        if !err.is_empty() && i.terminal() {
            s.push_str(&format!("      {err}\n"));
        }
    }
    if !l.report.is_empty() {
        s.push_str(&format!("report: {}\n", l.report));
    }
    s
}
