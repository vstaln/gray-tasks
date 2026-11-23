//! Per-item git worktree isolation (hermes `subagent_worktree.py` shape):
//! `git worktree add <repo>/.worktrees/wf-<id>-<idx> -b workflows/<id>-<idx>`,
//! `.worktrees/` auto-added to `.gitignore`, and a finalize step that only
//! prunes on affirmative proof (0 commits AND clean tree) — anything
//! unmeasured keeps the worktree for manual review.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::workflows::ledger::WorktreeInfo;

fn git(args: &[&str], cwd: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    }
}

/// `git rev-parse --show-toplevel`; `None` outside a work tree.
pub fn repo_root(cwd: &Path) -> Option<PathBuf> {
    git(&["rev-parse", "--show-toplevel"], cwd).map(PathBuf::from)
}

/// Keep `.worktrees/` out of status noise — hermes' `_ensure_gitignore_entry`.
fn ensure_gitignore(repo: &Path) {
    let path = repo.join(".gitignore");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == ".worktrees/") {
        return;
    }
    let mut body = existing;
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str(".worktrees/\n");
    let _ = std::fs::write(&path, body);
}

/// Create `<repo>/.worktrees/wf-<wf_id>-<idx>` on branch
/// `workflows/<wf_id>-<idx>`; `None` silently downgrades to the repo cwd
/// (unborn HEAD, non-git dir, git failure — same contract as hermes).
pub fn create(repo: &Path, wf_id: &str, idx: usize) -> Option<WorktreeInfo> {
    let name = format!("wf-{wf_id}-{idx}");
    let branch = format!("workflows/{wf_id}-{idx}");
    let wt_path = repo.join(".worktrees").join(&name);
    ensure_gitignore(repo);
    let base = git(&["rev-parse", "HEAD"], repo).unwrap_or_default();
    let ok = Command::new("git")
        .args([
            "worktree",
            "add",
            &wt_path.to_string_lossy(),
            "-b",
            &branch,
            "HEAD",
        ])
        .current_dir(repo)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        return None;
    }
    Some(WorktreeInfo {
        path: wt_path.to_string_lossy().into_owned(),
        branch,
        repo: repo.to_string_lossy().into_owned(),
        base_commit: base,
        commits: 0,
        dirty: false,
        pruned: false,
        note: String::new(),
    })
}

/// The prompt block telling the child it owns an isolated worktree.
pub fn context_note(wt: &WorktreeInfo) -> String {
    format!(
        "\n\n[WORKTREE ISOLATION] You are working in an isolated git worktree \
         at: {}\nYour dedicated branch is: {}\nAll file edits and shell \
         commands must happen inside this worktree directory (your terminal \
         already starts there). Do NOT cd to the main repository checkout. \
         Commit your changes to your branch when done; the workflow report \
         will reference your branch. If you make no commits and leave the \
         tree clean, the worktree is discarded automatically.",
        wt.path, wt.branch
    )
}

/// Inspect a finished item's worktree; prune only when the tree is clean
/// AND zero commits ahead of base — defaults prove nothing (hermes #88113).
pub fn finalize(wt: &mut WorktreeInfo) {
    let path = Path::new(&wt.path);
    if wt.path.is_empty() {
        return;
    }
    if wt.pruned || !path.is_dir() {
        wt.pruned = true;
        return;
    }
    if wt.base_commit.is_empty() {
        wt.note = "no base commit recorded; worktree kept for review".into();
        return;
    }
    let Some(commits) = git(
        &["rev-list", "--count", &format!("{}..HEAD", wt.base_commit)],
        path,
    )
    .and_then(|s| s.parse::<i64>().ok()) else {
        wt.note = "git rev-list probe failed; worktree kept for review".into();
        return;
    };
    wt.commits = commits;
    let Some(porcelain) = git(&["status", "--porcelain"], path) else {
        wt.note = "git status probe failed; worktree kept for review".into();
        return;
    };
    wt.dirty = !porcelain.is_empty();
    if commits == 0 && !wt.dirty {
        let repo = PathBuf::from(&wt.repo);
        let removed = Command::new("git")
            .args(["worktree", "remove", "--force", &wt.path])
            .current_dir(&repo)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if removed {
            let _ = git(&["branch", "-D", &wt.branch], &repo);
            wt.pruned = true;
        }
    }
}
