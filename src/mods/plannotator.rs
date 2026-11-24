//! `gray_plannotator` — annotated plan review, terminal port of
//! `@plannotator/pi-extension` (interactive plan review before
//! implementation).
//!
//! The pi original gates plan approval behind a GUI annotator. Gray has no
//! GUI surface and `command/run` answers inside a 30s TTL, so the review is
//! a phase machine advanced by repeated `/plan-review` calls:
//!
//! ```text
//! (none)                /plan-review  → prompt: agent writes .gray/plans/<ts>.md
//! awaiting_plan         /plan-review  → adopt file as baseline, print annotate how-to
//! awaiting_annotations  (user edits in $EDITOR) /plan-review → diff → prompt: revise
//! awaiting_revision     /plan-review  → "revision ready" once the file changed
//! any                   /plan-accept  → prompt: plan accepted — implement
//! ```
//!
//! Edit attribution is by phase, not by file watching: inside
//! `awaiting_annotations` a changed file means the user annotated; inside
//! `awaiting_revision` it means the agent rewrote. The `prompt/context`
//! hook injects a short "do not implement yet" note for every active phase.
//!
//! Works alongside `gray-plan` (read-only plan mode): plan mode's
//! `tool/before` may deny the agent's `write` of the plan file, so every
//! agent-facing prompt carries a fallback ("reply with the plan text" /
//! "the user can paste it into the file"). Review state lives next to the
//! plans at `<cwd>/.gray/plans/.review.json` and clears on accept/cancel.

use serde::{Deserialize, Serialize};

#[path = "plannotator/sidecar.rs"]
pub mod sidecar;
use serde_json::{Value, json};
pub use sidecar::{handle_plannotator, main_plannotator};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const PLUGIN_NAME: &str = "plannotator";
pub const PROTOCOL: &str = "1.1";
pub const PLAN_REVIEW: &str = "/plan-review";
pub const PLAN_ACCEPT: &str = "/plan-accept";
pub const STATE_FILE: &str = ".review.json";
pub const PLANS_DIR: &str = ".gray/plans";

/// Review phase: which side the next file change is attributed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(clippy::enum_variant_names)]
pub enum Phase {
    /// `/plan-review` issued but the agent hasn't written the plan yet.
    AwaitingPlan,
    /// Plan adopted as baseline; the user annotates it in $EDITOR.
    AwaitingAnnotations,
    /// Annotations fed back; the agent rewrites the plan file.
    AwaitingRevision,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewState {
    /// Absolute path of the plan under review.
    pub plan_path: PathBuf,
    /// Display path relative to the session cwd (`.gray/plans/…`).
    pub rel_path: String,
    pub phase: Phase,
    /// File content at the last phase transition — the version the
    /// "other side" last saw. Diffs always run against this.
    pub baseline: String,
    /// Annotation rounds completed.
    pub round: u32,
    pub created_unix: u64,
}

/// One `<!-- … -->` comment the user added since the baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Annotation {
    /// Comment body, whitespace-joined onto one line.
    pub text: String,
    /// 1-based line where the comment opens.
    pub line: usize,
    /// Nearest preceding content line, for "where in the plan" context.
    pub context: String,
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Days-from-epoch → civil (y, m, d); Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `plan-YYYYMMDD-HHMMSS.md` (UTC) — the `<ts>` in `.gray/plans/<ts>.md`.
pub fn plan_filename(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "plan-{y:04}{m:02}{d:02}-{:02}{:02}{:02}.md",
        tod / 3600,
        tod % 3600 / 60,
        tod % 60
    )
}

pub fn plan_dir(cwd: &Path) -> PathBuf {
    cwd.join(PLANS_DIR)
}

fn state_path(cwd: &Path) -> PathBuf {
    plan_dir(cwd).join(STATE_FILE)
}

pub fn load_state(cwd: &Path) -> Option<ReviewState> {
    let text = std::fs::read_to_string(state_path(cwd)).ok()?;
    serde_json::from_str(&text).ok()
}

fn save_state(cwd: &Path, st: &ReviewState) -> std::io::Result<()> {
    std::fs::create_dir_all(plan_dir(cwd))?;
    std::fs::write(
        state_path(cwd),
        serde_json::to_string(st).unwrap_or_default(),
    )
}

fn clear_state(cwd: &Path) {
    let _ = std::fs::remove_file(state_path(cwd));
}

/// First free `.gray/plans/plan-<ts>.md`; bumps the stamp on collision.
fn fresh_plan_path(cwd: &Path) -> (PathBuf, String) {
    let mut secs = unix_now();
    loop {
        let name = plan_filename(secs);
        let path = plan_dir(cwd).join(&name);
        if !path.exists() {
            return (path, format!("{PLANS_DIR}/{name}"));
        }
        secs += 1;
    }
}

/// `<!-- … -->` comment spans as (start_line, body). Bodies may span lines.
fn comments(text: &str) -> Vec<(usize, String)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut line = 1usize;
    while i + 3 < bytes.len() {
        if &bytes[i..i + 4] == b"<!--" {
            let start_line = line;
            let body_start = i + 4;
            let mut j = body_start;
            while j + 2 < bytes.len() && &bytes[j..j + 3] != b"-->" {
                if bytes[j] == b'\n' {
                    line += 1;
                }
                j += 1;
            }
            let body = String::from_utf8_lossy(&bytes[body_start..j.min(bytes.len())])
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            out.push((start_line, body));
            i = j + 3;
        } else {
            if bytes[i] == b'\n' {
                line += 1;
            }
            i += 1;
        }
    }
    out
}

/// `text` with every `<!-- … -->` region removed (used for the text diff).
fn strip_comments(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut seg = 0usize;
    let mut i = 0;
    while i + 3 < bytes.len() {
        if &bytes[i..i + 4] == b"<!--" {
            out.push_str(&text[seg..i]);
            let mut j = i + 4;
            while j + 2 < bytes.len() && &bytes[j..j + 3] != b"-->" {
                j += 1;
            }
            i = (j + 3).min(bytes.len());
            seg = i;
        } else {
            i += 1;
        }
    }
    out.push_str(&text[seg.min(text.len())..]);
    out
}

/// Nearest non-empty, comment-free line above `line_no` (1-based),
/// truncated for prompt display.
fn context_line(text: &str, line_no: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut i = line_no.min(lines.len());
    while i > 0 {
        i -= 1;
        let t = strip_comments(lines[i]).trim().to_string();
        if !t.is_empty() {
            return t.chars().take(80).collect();
        }
    }
    String::new()
}

/// `<!-- -->` comments present in `current` but not in `baseline`
/// (compared by body text), each with its position and nearest context.
pub fn extract_annotations(current: &str, baseline: &str) -> Vec<Annotation> {
    let base: std::collections::HashSet<String> =
        comments(baseline).into_iter().map(|(_, b)| b).collect();
    comments(current)
        .into_iter()
        .filter(|(_, body)| !body.is_empty() && !base.contains(body))
        .map(|(line, body)| Annotation {
            context: context_line(current, line),
            line,
            text: body,
        })
        .collect()
}

/// Comment-free line edits between baseline and current, as a multiset
/// diff (order-insensitive: paragraph moves don't show as edits).
/// Returns (added_in_current, removed_from_baseline), each capped.
pub fn diff_text_edits(baseline: &str, current: &str) -> (Vec<String>, Vec<String>) {
    let counts = |s: &str| {
        let mut m = HashMap::<String, usize>::new();
        for l in strip_comments(s).lines() {
            let t = l.trim();
            if !t.is_empty() {
                *m.entry(t.to_string()).or_default() += 1;
            }
        }
        m
    };
    let (a, b) = (counts(baseline), counts(current));
    let mut added: Vec<String> = Vec::new();
    let mut removed: Vec<String> = Vec::new();
    for (line, n) in &b {
        let extra = n.saturating_sub(*a.get(line).unwrap_or(&0));
        for _ in 0..extra {
            added.push(line.clone());
        }
    }
    for (line, n) in &a {
        let extra = n.saturating_sub(*b.get(line).unwrap_or(&0));
        for _ in 0..extra {
            removed.push(line.clone());
        }
    }
    added.sort();
    removed.sort();
    added.truncate(40);
    removed.truncate(40);
    (added, removed)
}

/// The revision prompt: what `command/run` hands back as `{"prompt": …}`
/// so the host submits it as a user turn.
pub fn revision_prompt(
    st: &ReviewState,
    annotations: &[Annotation],
    added: &[String],
    removed: &[String],
) -> String {
    let mut p = format!(
        "The user reviewed your plan at `{}` and left feedback (round {}).\n",
        st.rel_path,
        st.round + 1
    );
    if !annotations.is_empty() {
        p.push_str("\nAnnotations (markdown comments):\n");
        for a in annotations.iter().take(25) {
            if a.context.is_empty() {
                p.push_str(&format!("- \"{}\" (line {})\n", a.text, a.line));
            } else {
                p.push_str(&format!(
                    "- \"{}\" (line {}, near \"{}\")\n",
                    a.text, a.line, a.context
                ));
            }
        }
    }
    if !added.is_empty() || !removed.is_empty() {
        p.push_str("\nDirect edits to the plan text:\n");
        for l in added {
            p.push_str(&format!("+ {l}\n"));
        }
        for l in removed {
            p.push_str(&format!("- {l}\n"));
        }
    }
    p.push_str(&format!(
        "\nRewrite the plan at `{}` to address this feedback (you may drop `<!-- -->` comments you resolve). \
         Do NOT implement the plan yet — the user reviews the revision next. \
         If write tools are unavailable, reply with the full revised plan text instead.",
        st.rel_path
    ));
    p
}

const ANNOTATE_HOWTO: &str = "annotate it in $EDITOR: wrap notes as `<!-- comment -->` lines or edit the text directly. \
Then `/plan-review` sends your feedback to the agent · `/plan-accept` approves the plan as-is.";

const WRITE_PLAN_PROMPT: &str = "Write your implementation plan to `{ABS}` as markdown (create the file). \
Then STOP — do not implement anything; the user is annotating the plan before you proceed. \
If write tools are denied (e.g. plan mode), reply with the plan text instead and ask the user to save it to that path.";

/// `prompt/context` text for an active review — keeps the model from
/// implementing (or editing the file) while the user owns it.
pub fn context_text(st: &ReviewState) -> String {
    match st.phase {
        Phase::AwaitingPlan => format!(
            "[plan review] Write your plan to `{}` then stop — the user is reviewing it; do not implement or edit anything else.",
            st.rel_path
        ),
        Phase::AwaitingAnnotations => format!(
            "[plan review] `{}` is under user review — do not implement it and do not edit the file; wait for the user's annotations.",
            st.rel_path
        ),
        Phase::AwaitingRevision => format!(
            "[plan review] The user annotated `{}` — a revision request is coming; do not implement until the plan is accepted.",
            st.rel_path
        ),
    }
}

fn status_text(st: &ReviewState) -> String {
    let phase = match st.phase {
        Phase::AwaitingPlan => "waiting for the agent to write the plan",
        Phase::AwaitingAnnotations => "waiting for your annotations",
        Phase::AwaitingRevision => "waiting for the agent's revision",
    };
    format!(
        "plan review: {} — {} (round {})",
        st.rel_path, phase, st.round
    )
}

fn read_plan(st: &ReviewState) -> Option<String> {
    std::fs::read_to_string(&st.plan_path).ok()
}

/// `/plan-review` with no args: advance the phase machine once.
fn advance(cwd: &Path) -> Value {
    let Some(mut st) = load_state(cwd) else {
        let (abs, rel) = fresh_plan_path(cwd);
        let st = ReviewState {
            plan_path: abs.clone(),
            rel_path: rel.clone(),
            phase: Phase::AwaitingPlan,
            baseline: String::new(),
            round: 0,
            created_unix: unix_now(),
        };
        if let Err(e) = save_state(cwd, &st) {
            return json!({"text": format!("couldn't start review: {e}")});
        }
        return json!({"prompt": WRITE_PLAN_PROMPT.replace("{ABS}", &abs.to_string_lossy())});
    };

    let cur = read_plan(&st);
    match st.phase {
        Phase::AwaitingPlan => match cur {
            None => json!({"text": format!(
                "still waiting for the plan at `{}` — it arrives after the agent answers the /plan-review prompt. `/plan-review cancel` abandons.",
                st.rel_path
            )}),
            Some(c) => {
                st.baseline = c;
                st.phase = Phase::AwaitingAnnotations;
                let _ = save_state(cwd, &st);
                json!({"text": format!("plan ready for review: `{}` — {ANNOTATE_HOWTO}", st.rel_path)})
            }
        },
        Phase::AwaitingAnnotations => match cur {
            None => json!({"text": format!(
                "`{}` is missing — restore it or `/plan-review cancel`.", st.rel_path
            )}),
            Some(c) if c == st.baseline => json!({"text": format!(
                "no annotations yet on `{}` — {ANNOTATE_HOWTO}", st.rel_path
            )}),
            Some(c) => {
                let anns = extract_annotations(&c, &st.baseline);
                let (added, removed) = diff_text_edits(&st.baseline, &c);
                if anns.is_empty() && added.is_empty() && removed.is_empty() {
                    return json!({"text": format!(
                        "the file changed but nothing reads as feedback (whitespace only?) — {ANNOTATE_HOWTO}"
                    )});
                }
                let prompt = revision_prompt(&st, &anns, &added, &removed);
                st.baseline = c;
                st.phase = Phase::AwaitingRevision;
                st.round += 1;
                let _ = save_state(cwd, &st);
                json!({"prompt": prompt})
            }
        },
        Phase::AwaitingRevision => match cur {
            None => json!({"text": format!(
                "waiting for the agent's revision — `{}` is missing though (restore it or `/plan-review cancel`).",
                st.rel_path
            )}),
            Some(c) if c == st.baseline => json!({"text": format!(
                "waiting for the agent's revision of `{}` — it rewrites the file after your annotations.",
                st.rel_path
            )}),
            Some(c) => {
                st.baseline = c;
                st.phase = Phase::AwaitingAnnotations;
                let _ = save_state(cwd, &st);
                json!({"text": format!(
                    "revision {} is in `{}` — {ANNOTATE_HOWTO}", st.round, st.rel_path
                )})
            }
        },
    }
}

/// `command/run` for `/plan-review` → a `{"text"|"prompt"}` outcome.
pub fn plan_review(cwd: &Path, argv: &[String]) -> Value {
    match argv.first().map(String::as_str) {
        None => advance(cwd),
        Some("status") => match load_state(cwd) {
            Some(st) => json!({"text": status_text(&st)}),
            None => json!({"text": "no plan under review — /plan-review starts one"}),
        },
        Some("path") => match load_state(cwd) {
            Some(st) => json!({"text": st.plan_path.to_string_lossy().into_owned()}),
            None => json!({"text": "no plan under review"}),
        },
        Some("cancel") => match load_state(cwd) {
            Some(st) => {
                clear_state(cwd);
                json!({"text": format!("review cancelled — the plan stays at `{}`", st.rel_path)})
            }
            None => json!({"text": "no plan under review"}),
        },
        Some(_) => json!({"text": format!(
            "usage: /plan-review [status|path|cancel] — bare advances the review loop · {ANNOTATE_HOWTO}"
        )}),
    }
}

/// `command/run` for `/plan-accept` → ends the loop.
pub fn plan_accept(cwd: &Path) -> Value {
    match load_state(cwd) {
        None => json!({"text": "no plan under review"}),
        Some(st) => {
            clear_state(cwd);
            if st.plan_path.exists() {
                json!({"prompt": format!(
                    "The plan at `{}` has been reviewed and accepted by the user (they may have made final edits — read the current file). Implement it now, using that plan as the source of truth.",
                    st.rel_path
                )})
            } else {
                json!({"text": format!(
                    "review cleared — no plan was ever written to `{}`.", st.rel_path
                )})
            }
        }
    }
}

/// `prompt/context` result value for the hook.
pub fn prompt_context(cwd: &Path) -> Value {
    match load_state(cwd) {
        Some(st) => json!({"text": context_text(&st)}),
        None => json!({"text": ""}),
    }
}

/// `plugin/manifest` result value — same shape the installed
/// `<name>-manifest.json` carries (discord-style).
pub fn manifest_plannotator() -> Value {
    json!({
        "name": PLUGIN_NAME,
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": PROTOCOL,
        "sidecar_args": ["sidecar"],
        "tools": [],
        "commands": [PLAN_REVIEW, PLAN_ACCEPT],
        "command_descriptions": {"/plan-review": "review a plan before it runs", "/plan-accept": "accept the reviewed plan"},
        "hooks": ["prompt/context"],
        "completion": ["status", "path", "cancel", "help"],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_shape() {
        let m = manifest_plannotator();
        assert_eq!(m["name"], "plannotator");
        assert_eq!(m["protocol"], "1.1");
        assert_eq!(m["sidecar_args"], json!(["sidecar"]));
        assert_eq!(m["commands"], json!(["/plan-review", "/plan-accept"]));
        assert_eq!(m["hooks"], json!(["prompt/context"]));
    }

    #[test]
    fn filename_is_civil() {
        assert_eq!(plan_filename(0), "plan-19700101-000000.md");
        assert_eq!(plan_filename(1_760_011_122), "plan-20251009-115842.md");
    }

    #[test]
    fn comments_parse_multiline() {
        let (l, b) = comments("a\n<!-- one\n two -->\nb <!-- tail -->")[0].clone();
        assert_eq!(l, 2);
        assert_eq!(b, "one two");
        assert_eq!(comments("x")[..].len(), 0);
    }

    #[test]
    fn annotations_are_new_comments_with_context() {
        let base = "# Plan\n\n1. do a\n2. do b\n";
        let cur = "# Plan\n\n1. do a <!-- use the cache here -->\n2. do b\n";
        let anns = extract_annotations(cur, base);
        assert_eq!(anns.len(), 1);
        assert_eq!(anns[0].text, "use the cache here");
        assert_eq!(anns[0].line, 3);
        assert!(anns[0].context.contains("do a") || anns[0].context.contains("Plan"));
        // Comments already in the baseline aren't re-reported.
        assert!(extract_annotations(cur, cur).is_empty());
    }

    #[test]
    fn text_edits_ignore_comments_and_moves() {
        let base = "step a\nstep b\nstep c\n";
        let cur = "step c\nstep a <!-- note -->\nstep b\nnew step\n";
        let (added, removed) = diff_text_edits(base, cur);
        assert_eq!(added, vec!["new step"]);
        assert!(removed.is_empty());
    }

    #[test]
    fn revision_prompt_lists_notes() {
        let st = ReviewState {
            plan_path: PathBuf::from("/x/.gray/plans/p.md"),
            rel_path: ".gray/plans/p.md".into(),
            phase: Phase::AwaitingAnnotations,
            baseline: String::new(),
            round: 1,
            created_unix: 0,
        };
        let p = revision_prompt(
            &st,
            &[Annotation {
                text: "rethink".into(),
                line: 3,
                context: "step".into(),
            }],
            &["added line".into()],
            &["gone".into()],
        );
        assert!(p.contains("\"rethink\" (line 3"));
        assert!(p.contains("+ added line"));
        assert!(p.contains("- gone"));
        assert!(p.contains("Do NOT implement"));
    }
}
