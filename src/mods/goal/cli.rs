//! The `/goal …` and `gray goal …` surface — one clap parser, one dispatch.
//! `run_at` returns a [`Reply`] so the wire can distinguish "say this" from
//! "run this prompt" (the `/goal <desc>` creation path feeds the session a
//! planning prompt — the checklist is drafted conversationally, not by us).

use clap::{Args, Parser, Subcommand};
use std::path::Path;

use anyhow::{Context, bail, ensure};

use crate::goal::audit;
use crate::goal::{Audit, Status, Store};

#[derive(Args, Debug, Clone)]
pub struct GoalArgs {
    #[command(subcommand)]
    pub command: Option<GoalCommand>,
}

/// Words that start a real subcommand; anything else after `/goal` is the
/// objective shorthand (`/goal add CSV export` → `new "add CSV export"`).
pub const VERBS: &[&str] = &[
    "new",
    "list",
    "show",
    "done",
    "check",
    "drop",
    "audit",
    "audit-run",
    "help",
];

/// Route argv to a parse: bare → status; verb-first → clap; else the
/// creation shorthand. Shared by the wire (`command/run`) and the CLI.
pub fn args_for(argv: Vec<String>) -> Result<GoalArgs, clap::Error> {
    let verb = argv
        .first()
        .map(|a| VERBS.contains(&a.to_ascii_lowercase().as_str()))
        .unwrap_or(false);
    if !argv.is_empty() && !verb {
        return Ok(GoalArgs {
            command: Some(GoalCommand::New { desc: argv }),
        });
    }
    #[derive(Parser)]
    struct Argv {
        #[command(flatten)]
        args: GoalArgs,
    }
    Argv::try_parse_from(std::iter::once("goal".to_string()).chain(argv)).map(|a| a.args)
}

#[derive(Subcommand, Debug, Clone)]
pub enum GoalCommand {
    /// Create a goal and hand the session a planning prompt
    New { desc: Vec<String> },
    /// List every goal
    List,
    /// Show one goal doc (default: the active goal)
    Show { id: Option<String> },
    /// `done` — mark the goal done; `done <n>` — check item n;
    /// `done <id>` — mark that goal done
    Done { arg: Option<String> },
    /// Check item n explicitly (alias of `done <n>`)
    Check { n: usize, id: Option<String> },
    /// Drop a goal (kept on disk, status=dropped)
    Drop { id: Option<String> },
    /// Spawn the independent completion auditor
    Audit { id: Option<String> },
    /// `audit-run` — internal: the detached audit child
    #[command(hide = true)]
    AuditRun {
        id: String,
        #[arg(long)]
        cwd: Option<String>,
    },
}

/// What the wire returns: `text` renders inline; `prompt` becomes a session
/// prompt (creation path — the agent drafts the checklist via the tool).
pub struct Reply {
    pub text: String,
    pub prompt: Option<String>,
}

impl Reply {
    fn text(s: impl Into<String>) -> Reply {
        Reply {
            text: s.into(),
            prompt: None,
        }
    }
    pub fn to_wire(&self) -> serde_json::Value {
        if let Some(p) = &self.prompt {
            serde_json::json!({ "prompt": p, "text": self.text })
        } else {
            serde_json::json!({ "text": self.text })
        }
    }
}

pub fn run(args: &GoalArgs) -> anyhow::Result<Reply> {
    let home = crate::goal::gray_home()?;
    let cwd = std::env::current_dir()?;
    run_at(args, &home, &cwd)
}

/// Testable seam: dispatch against explicit home + session cwd.
pub fn run_at(args: &GoalArgs, home: &Path, cwd: &Path) -> anyhow::Result<Reply> {
    let store = Store::new(home)?;
    match &args.command {
        // `/goal` bare — status of the active goal.
        None => Ok(Reply::text(status_line(&store))),
        Some(GoalCommand::New { desc }) => {
            ensure!(!desc.is_empty(), "usage: /goal new <description>");
            create(&store, &desc.join(" "), cwd)
        }
        Some(GoalCommand::List) => Ok(Reply::text(list(&store))),
        Some(GoalCommand::Show { id }) => show(&store, id.as_deref()),
        Some(GoalCommand::Done { arg }) => done(&store, arg.as_deref()),
        Some(GoalCommand::Check { n, id }) => check_item(&store, *n, id.as_deref()),
        Some(GoalCommand::Drop { id }) => drop_goal(&store, id.as_deref()),
        Some(GoalCommand::Audit { id }) => kickoff(&store, id.as_deref(), cwd),
        Some(GoalCommand::AuditRun { id, cwd: c }) => {
            let dir = c
                .as_deref()
                .map(Path::new)
                .map(Path::to_path_buf)
                .unwrap_or_else(|| cwd.to_path_buf());
            Ok(Reply::text(audit::audit_run(home, id, &dir)?))
        }
    }
}

fn status_line(store: &Store) -> String {
    match store.active() {
        Some(g) => {
            let mut s = format!("{}\n", g.summary());
            if let Some((n, item)) = g.next_open() {
                s.push_str(&format!("next: #{n} {item}\n"));
            }
            s.push_str("verbs: /goal show · done [n] · check <n> · drop · audit · list\n");
            s
        }
        None => "no active goal — /goal <description> starts one\n".to_string(),
    }
}

fn list(store: &Store) -> String {
    let goals = store.list();
    if goals.is_empty() {
        return "no goals — /goal <description> starts one\n".to_string();
    }
    let mut out = String::new();
    for g in &goals {
        let mark = if g.status == Status::Active { "*" } else { " " };
        out.push_str(&format!("{mark} {}\n", g.summary()));
    }
    out
}

fn show(store: &Store, id: Option<&str>) -> anyhow::Result<Reply> {
    let sel = id.unwrap_or("");
    let Some(g) = store.resolve(sel) else {
        bail!("no goal matching '{sel}'");
    };
    let path = g.path(&store.home);
    let mut out = format!("{}\n", g.summary());
    if !g.desc.is_empty() {
        out.push_str(&format!("{}\n", g.desc));
    }
    for (n, item) in g.items.iter().enumerate() {
        out.push_str(&format!(
            "  {}. [{}] {}\n",
            n + 1,
            if item.checked { "x" } else { " " },
            item.text
        ));
    }
    if !g.audit_log.is_empty() {
        out.push_str("audit log:\n");
        for line in g.audit_log.iter().rev().take(5).rev() {
            out.push_str(&format!("  - {line}\n"));
        }
    }
    out.push_str(&format!("doc: {}\n", path.display()));
    Ok(Reply::text(out))
}

fn create(store: &Store, desc: &str, cwd: &Path) -> anyhow::Result<Reply> {
    let goal = store.create(desc, cwd)?;
    let id = goal.id.clone();
    Ok(Reply {
        text: format!("goal {id} created — {}\n", goal.title),
        prompt: Some(format!(
            "A persistent goal was just created (doc ~/.gray/goals/{id}.md):\n\
            \"{desc}\"\n\n\
            Turn it into an ordered checklist of concrete, verifiable steps with the `goal` tool \
            (action=set_items). Discuss the plan with the user only if the objective is genuinely \
            ambiguous; otherwise draft it and start work on the first item. \
            Check items off as you finish them (action=check_item). Completion is claimed with \
            action=complete and verified by an independent auditor via /goal audit — never self-grade."
        )),
    })
}

/// `/goal done` (mark whole goal), `/goal done <n>` (item n of the active
/// goal), `/goal done <id>` (that goal).
fn done(store: &Store, arg: Option<&str>) -> anyhow::Result<Reply> {
    done_inner(store, arg, None)
}

fn check_item(store: &Store, n: usize, id: Option<&str>) -> anyhow::Result<Reply> {
    done_inner(store, id, Some(n))
}

fn done_inner(store: &Store, sel: Option<&str>, n: Option<usize>) -> anyhow::Result<Reply> {
    // A bare `done N` reads as an item number; anything else as a goal id.
    let (sel, n) = match (sel, n) {
        (Some(a), None) => match a.parse::<usize>() {
            Ok(n) => (None, Some(n)),
            Err(_) => (Some(a), None),
        },
        (s, n) => (s, n),
    };
    let sel = sel.unwrap_or("");
    let Some(mut g) = store.resolve(sel) else {
        bail!("no goal matching '{sel}'");
    };
    if let Some(n) = n {
        ensure!(
            n >= 1 && n <= g.items.len(),
            "item {n} out of range (goal has {} items)",
            g.items.len()
        );
        let was = g.items[n - 1].checked;
        store.update(&mut g, |g| {
            g.items[n - 1].checked = true;
            g.claim_blocked = false;
        })?;
        return Ok(Reply::text(if was {
            format!("item {n} was already checked\n")
        } else {
            format!("checked item {n}: {}\n", g.items[n - 1].text)
        }));
    }
    // Whole-goal done: the user's authority — warn on open items but allow.
    let open = g.open();
    store.update(&mut g, |g| {
        g.status = Status::Done;
        g.claim_blocked = false;
    })?;
    Ok(Reply::text(if open.is_empty() {
        format!(
            "goal {} marked done{}.\n",
            g.id,
            match g.audit {
                Audit::Pass => " (audit: pass)",
                Audit::Fail => " (audit: FAIL — rerun /goal audit after fixes)",
                _ => " — /goal audit verifies it independently",
            }
        )
    } else {
        format!(
            "goal {} marked done with {} item(s) still open: {}\n",
            g.id,
            open.len(),
            open.iter()
                .map(|n| format!("#{n}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }))
}

fn drop_goal(store: &Store, id: Option<&str>) -> anyhow::Result<Reply> {
    let sel = id.unwrap_or("");
    let Some(mut g) = store.resolve(sel) else {
        bail!("no goal matching '{sel}'");
    };
    store.update(&mut g, |g| g.status = Status::Dropped)?;
    Ok(Reply::text(format!(
        "goal {} dropped (kept at {} — delete the file to forget it)\n",
        g.id,
        g.path(&store.home).display()
    )))
}

fn kickoff(store: &Store, id: Option<&str>, cwd: &Path) -> anyhow::Result<Reply> {
    let sel = id.unwrap_or("");
    let goal = store
        .resolve(sel)
        .or_else(|| {
            store
                .list()
                .into_iter()
                .find(|g| g.status != Status::Dropped)
        })
        .context("no goal to audit")?;
    Ok(Reply::text(audit::kickoff(store, &goal, cwd)?))
}

#[allow(dead_code)]
pub fn usage() -> String {
    "/goal — persistent goals + independent completion audit\n\
     /goal <desc>   create a goal (the agent drafts the ordered checklist)\n\
     /goal list     all goals (* = active)\n\
     /goal show [id] goal doc summary + path\n\
     /goal done [n] check item n, or mark the goal done\n\
     /goal check <n> check item n\n\
     /goal drop [id] drop a goal (file kept, status=dropped)\n\
     /goal audit [id] independent audit of checked items → pass/fail verdict\n"
        .to_string()
}

/// Dispatch for the model-facing `goal` tool. Returns `(content, is_error)`.
pub fn tool_call(store: &Store, args: &serde_json::Value) -> (String, bool) {
    let action = args
        .get("action")
        .and_then(|a| a.as_str())
        .unwrap_or("status");
    let sel = args.get("goal_id").and_then(|g| g.as_str()).unwrap_or("");
    let goal = match store.resolve(sel) {
        Some(g) => g,
        None => {
            return (
                "no active goal — the user starts one with /goal <description>".to_string(),
                true,
            );
        }
    };
    // Mutating items on a done goal reopens it — work resumed.
    let reopen = |g: &mut crate::goal::Goal| {
        if g.status == Status::Done {
            g.status = Status::Active;
        }
    };
    match action {
        "status" => (status_line(store), false),
        "set_items" => {
            let items: Vec<String> = args
                .get("items")
                .and_then(|i| i.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|e| e.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            if items.is_empty() {
                return ("set_items needs a non-empty `items` array".into(), true);
            }
            let mut g = goal;
            let r = store.update(&mut g, |g| {
                g.items = items
                    .iter()
                    .map(|t| crate::goal::Item {
                        checked: false,
                        text: t.clone(),
                    })
                    .collect();
                g.claim_blocked = false;
                reopen(g);
            });
            match r {
                Ok(()) => (
                    format!("goal {}: checklist set ({} items)", g.id, g.items.len()),
                    false,
                ),
                Err(e) => (format!("goal: {e:#}"), true),
            }
        }
        "add_item" => {
            let item = args.get("item").and_then(|i| i.as_str()).unwrap_or("");
            if item.trim().is_empty() {
                return ("add_item needs `item` text".into(), true);
            }
            let mut g = goal;
            let r = store.update(&mut g, |g| {
                g.items.push(crate::goal::Item {
                    checked: false,
                    text: item.trim().to_string(),
                });
                g.claim_blocked = false;
                reopen(g);
            });
            match r {
                Ok(()) => (
                    format!("goal {}: added item {}", g.id, g.items.len()),
                    false,
                ),
                Err(e) => (format!("goal: {e:#}"), true),
            }
        }
        "check_item" | "uncheck_item" => {
            let n = args.get("n").and_then(|n| n.as_u64()).unwrap_or(0) as usize;
            if n == 0 || n > goal.items.len() {
                return (
                    format!("n must be 1..={} for goal {}", goal.items.len(), goal.id),
                    true,
                );
            }
            let check = action == "check_item";
            let mut g = goal;
            let r = store.update(&mut g, |g| {
                g.items[n - 1].checked = check;
                if check {
                    g.claim_blocked = false;
                }
                reopen(g);
            });
            match r {
                Ok(()) => (
                    format!(
                        "goal {}: item {n} {} ({})",
                        g.id,
                        if check { "checked" } else { "unchecked" },
                        g.items[n - 1].text
                    ),
                    false,
                ),
                Err(e) => (format!("goal: {e:#}"), true),
            }
        }
        // The completion claim: refused while items stay open — this is the
        // nudge; `claim_blocked` arms `agent/before_start` for one reminder.
        "complete" => {
            let open = goal.open();
            if !open.is_empty() {
                let mut g = goal;
                let _ = store.update(&mut g, |g| g.claim_blocked = true);
                let lines: Vec<String> = open
                    .iter()
                    .map(|&n| format!("  {n}. {}", g.items[n - 1].text))
                    .collect();
                return (
                    format!(
                        "goal {} not complete — {} item(s) still open:\n{}\n\
                        Finish them and check them off (action=check_item), or revise the plan. \
                        Completion is independently audited, not self-graded.",
                        g.id,
                        open.len(),
                        lines.join("\n")
                    ),
                    true,
                );
            }
            let mut g = goal;
            let r = store.update(&mut g, |g| {
                g.status = Status::Done;
                g.claim_blocked = false;
            });
            match r {
                Ok(()) => (
                    format!(
                        "goal {} marked complete. An independent audit (`/goal audit`) is the \
                        real grade — ask the user to run it, or expect it to run at review time.",
                        g.id
                    ),
                    false,
                ),
                Err(e) => (format!("goal: {e:#}"), true),
            }
        }
        other => (
            format!(
                "unknown goal action '{other}' — use status|set_items|add_item|check_item|uncheck_item|complete"
            ),
            true,
        ),
    }
}
