//! The durable ledger: `<cwd>/.gray/workflows/<id>.json`.
//!
//! One JSON document per workflow, written atomically (tempfile + rename).
//! The detached orchestrator owns writes while a run is live; `stop`,
//! `resume` and `prompt/context` touch single fields through the same
//! atomic path. A crashed orchestrator leaves a ledger that `resume`
//! reconciles item-by-item against `subagents/runs` records.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use crate::workflows::spec::Spec;

pub const LEDGER_DIR: &str = ".gray/workflows";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeInfo {
    pub path: String,
    pub branch: String,
    /// Owning repo root — needed for `branch -D` after the worktree is gone.
    pub repo: String,
    pub base_commit: String,
    #[serde(default)]
    pub commits: i64,
    #[serde(default)]
    pub dirty: bool,
    #[serde(default)]
    pub pruned: bool,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub idx: usize,
    /// Subagent run name: `wf-<id>-<idx>` (plus item name when given).
    pub name: String,
    pub prompt: String,
    pub agent: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub effort: String,
    #[serde(default)]
    pub worktree: bool,
    #[serde(default)]
    pub phase: u32,
    /// pending | running | completed | failed | stopped | timeout | lost
    pub status: String,
    #[serde(default)]
    pub run_id: String,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub last_error: String,
    #[serde(default)]
    pub started: f64,
    #[serde(default)]
    pub finished: f64,
    #[serde(default)]
    pub result: String,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub wt: Option<WorktreeInfo>,
    /// Directory the item actually launched in (worktree path or project cwd).
    #[serde(default)]
    pub cwd: String,
}

impl Item {
    pub fn terminal(&self) -> bool {
        !matches!(self.status.as_str(), "pending" | "running")
    }
    pub fn ok(&self) -> bool {
        matches!(self.status.as_str(), "completed" | "ok")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ledger {
    /// 8-hex id; the ledger file name is `<id>.json`.
    pub id: String,
    pub name: String,
    /// Project root the workflow was launched from.
    pub cwd: String,
    #[serde(default)]
    pub spec_path: String,
    /// running | completed | failed | stopped
    pub status: String,
    pub max_running: u32,
    #[serde(default)]
    pub stop: bool,
    /// Orchestrator pid; `status` checks liveness to suggest `resume`.
    #[serde(default)]
    pub pid: u32,
    pub created: f64,
    #[serde(default)]
    pub finished: f64,
    pub items: Vec<Item>,
    #[serde(default)]
    pub report: String,
}

impl Ledger {
    pub fn done(&self) -> bool {
        self.items.iter().all(|i| i.terminal())
    }
    pub fn running_items(&self) -> usize {
        self.items.iter().filter(|i| i.status == "running").count()
    }
    pub fn completed_items(&self) -> usize {
        self.items.iter().filter(|i| i.ok()).count()
    }
    /// Lowest phase with unfinished work; later phases wait for it.
    pub fn current_phase(&self) -> u32 {
        self.items
            .iter()
            .filter(|i| !i.terminal())
            .map(|i| i.phase)
            .min()
            .unwrap_or(0)
    }
    /// True while a detached orchestrator owns this ledger.
    pub fn orchestrator_alive(&self) -> bool {
        self.pid != 0 && pid_alive(self.pid)
    }
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    let r = unsafe { libc::kill(pid as i32, 0) };
    r == 0
}

#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code = 0u32;
        let alive = GetExitCodeProcess(h, &mut code) != 0 && code == STILL_ACTIVE as u32;
        CloseHandle(h);
        return alive;
    }
}

pub fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn dir(cwd: &Path) -> PathBuf {
    cwd.join(LEDGER_DIR)
}

pub fn path(cwd: &Path, id: &str) -> PathBuf {
    dir(cwd).join(format!("{id}.json"))
}

pub fn report_path(cwd: &Path, id: &str) -> PathBuf {
    dir(cwd).join(format!("{id}.md"))
}

pub fn log_path(cwd: &Path, id: &str) -> PathBuf {
    dir(cwd).join(format!("{id}.log"))
}

/// Marker written once a finished workflow has been surfaced by
/// `prompt/context` — kept out of the ledger so it never races the
/// orchestrator's own writes.
pub fn noticed_path(cwd: &Path, id: &str) -> PathBuf {
    dir(cwd).join(format!("{id}.noticed"))
}

pub fn load(path: &Path) -> anyhow::Result<Ledger> {
    serde_json::from_slice(&std::fs::read(path)?)
        .with_context(|| format!("invalid ledger {}", path.display()))
}

/// Atomic write via tempfile+persist — readers never see a torn ledger.
pub fn save(path: &Path, ledger: &Ledger) -> anyhow::Result<()> {
    let dir = path.parent().context("ledger path has no parent")?;
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    use std::io::Write;
    writeln!(tmp, "{}", serde_json::to_string_pretty(ledger)?)?;
    tmp.persist(path)?;
    Ok(())
}

/// All ledgers under `<cwd>/.gray/workflows`, newest first.
pub fn list(cwd: &Path) -> Vec<Ledger> {
    let mut out = Vec::new();
    let dir = dir(cwd);
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) == Some("json")
                && let Ok(l) = load(&p)
            {
                out.push(l);
            }
        }
    }
    out.sort_by(|a, b| b.created.total_cmp(&a.created));
    out
}

/// Resolve an id argument: exact ledger name, unique prefix, or `last`.
pub fn resolve(cwd: &Path, id: Option<&str>) -> anyhow::Result<Ledger> {
    let all = list(cwd);
    match id.unwrap_or("last") {
        "last" => all
            .into_iter()
            .next()
            .context("no workflows in this directory"),
        id => {
            let mut hits: Vec<Ledger> = all
                .into_iter()
                .filter(|l| l.id == id || l.id.starts_with(id))
                .collect();
            anyhow::ensure!(!hits.is_empty(), "no workflow matching '{id}'");
            anyhow::ensure!(
                hits.len() == 1 || hits[0].id == id,
                "'{id}' matches {} workflows; give more digits",
                hits.len()
            );
            Ok(hits.remove(0))
        }
    }
}

/// Ledger built from a parsed spec; item names default to `wf-<id>-<idx>`.
pub fn new_ledger(
    id: &str,
    spec: &Spec,
    cwd: &Path,
    spec_path: &str,
    default_cap: u32,
) -> anyhow::Result<Ledger> {
    let items = spec
        .items
        .iter()
        .enumerate()
        .map(|(idx, it)| Item {
            idx,
            name: it
                .name
                .clone()
                .map(|n| format!("wf-{id}-{n}"))
                .unwrap_or_else(|| format!("wf-{id}-{idx}")),
            prompt: it.prompt.clone(),
            agent: it.agent.clone(),
            model: it.model.clone().unwrap_or_default(),
            effort: it.effort.clone().unwrap_or_default(),
            worktree: it.worktree,
            phase: it.phase,
            status: "pending".into(),
            run_id: String::new(),
            attempts: 0,
            last_error: String::new(),
            started: 0.0,
            finished: 0.0,
            result: String::new(),
            error: String::new(),
            wt: None,
            cwd: String::new(),
        })
        .collect();
    Ok(Ledger {
        id: id.to_string(),
        name: spec.name.clone().unwrap_or_else(|| id.to_string()),
        cwd: cwd.to_string_lossy().into_owned(),
        spec_path: spec_path.to_string(),
        status: "running".into(),
        max_running: spec.max_running.unwrap_or(default_cap).clamp(1, 32),
        stop: false,
        pid: 0,
        created: now(),
        finished: 0.0,
        items,
        report: String::new(),
    })
}

/// 8-hex workflow id, retried on collision. Entropy is std-only: a fresh
/// RandomState's OS-seeded keys mixed with time and pid (no /dev/urandom
/// on Windows).
pub fn new_id(cwd: &Path) -> anyhow::Result<String> {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    for _ in 0..16 {
        let mut h = RandomState::new().build_hasher();
        h.write_u64(N.fetch_add(1, Ordering::Relaxed));
        h.write_u32(std::process::id());
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        );
        let id = format!("{:08x}", h.finish() as u32);
        if !path(cwd, &id).exists() {
            return Ok(id);
        }
    }
    anyhow::bail!("could not allocate a fresh workflow id")
}

/// Item row as the aggregate report sees it.
pub fn summary(l: &Ledger) -> Value {
    json!({
        "id": l.id, "name": l.name, "status": l.status,
        "total": l.items.len(), "done": l.completed_items(),
        "running": l.running_items(),
    })
}
