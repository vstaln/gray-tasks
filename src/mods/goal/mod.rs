//! `gray_goal` shared core: the goal-document model (`~/.gray/goals/<id>.md`),
//! the wire manifest + `goal` tool schema, and the auditor prompt/verdict
//! shapes. The sidecar binary (`src/main.rs`) and the `gray goal` CLI both
//! speak through these functions so wire and CLI never diverge.
//!
//! A goal doc is a human-editable markdown file with a tiny `key: value`
//! frontmatter block; the ordered checklist is `- [ ]` / `- [x]` bullets
//! under `## Checklist`. No YAML dependency — the frontmatter is ours.

pub mod audit;
pub mod cli;
pub mod wire;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;

/// Plugin name claimed in `plugin/manifest` (`/goal`, `gray goal`).
pub const PLUGIN_NAME: &str = "goal";
/// Wire protocol version (`plugin/shutdown` + `session` params).
pub const PROTOCOL: &str = "1.1";
/// Slash command claimed in the manifest.
pub const COMMANDS: &[&str] = &["/goal"];
/// Tool name the model calls to record progress and claim completion.
pub const TOOL_NAME: &str = "goal";
/// Host completer vocabulary for `/goal <tab>` and `gray goal <tab>`.
pub const COMPLETION: &[&str] = &[
    "new", "list", "show", "done", "check", "drop", "audit", "help",
];

/// `~/.gray`, overridable with `GRAY_HOME` (same rule as the host).
pub fn gray_home() -> anyhow::Result<PathBuf> {
    std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".gray")))
        .or_else(|| std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join(".gray")))
        .ok_or_else(|| {
            anyhow::anyhow!("cannot resolve home: set GRAY_HOME or the platform user profile")
        })
}

/// `<home>/goals` — one markdown doc per goal.
pub fn goals_dir(home: &Path) -> PathBuf {
    home.join("goals")
}

pub fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

// ---------------------------------------------------------------------------
// Goal document
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Active,
    Done,
    Dropped,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Active => "active",
            Status::Done => "done",
            Status::Dropped => "dropped",
        }
    }
    fn parse(s: &str) -> Self {
        match s.trim() {
            "done" | "complete" => Status::Done,
            "dropped" | "archived" => Status::Dropped,
            _ => Status::Active,
        }
    }
}

/// Audit verdict field: `none`, `running`, `pass`, `fail`, `error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audit {
    None,
    Running,
    Pass,
    Fail,
    Error,
}

impl Audit {
    pub fn as_str(&self) -> &'static str {
        match self {
            Audit::None => "none",
            Audit::Running => "running",
            Audit::Pass => "pass",
            Audit::Fail => "fail",
            Audit::Error => "error",
        }
    }
    fn parse(s: &str) -> Self {
        match s.trim() {
            "running" => Audit::Running,
            "pass" => Audit::Pass,
            "fail" => Audit::Fail,
            "error" => Audit::Error,
            _ => Audit::None,
        }
    }
}

/// One ordered checklist item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub checked: bool,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct Goal {
    pub id: String,
    pub title: String,
    pub status: Status,
    pub audit: Audit,
    /// Set when a `goal complete` claim was refused for open items; the
    /// `agent/before_start` nudge reads and clears it.
    pub claim_blocked: bool,
    /// Session cwd captured at creation (fallback audit cwd).
    pub cwd: String,
    pub created: String,
    pub updated: String,
    /// Objective paragraphs between `# Goal` and `## Checklist`.
    pub desc: String,
    /// Ordered checklist.
    pub items: Vec<Item>,
    /// `- <iso> verdict=… — …` lines under `## Audit log`.
    pub audit_log: Vec<String>,
}

impl Goal {
    pub fn path(&self, home: &Path) -> PathBuf {
        goals_dir(home).join(format!("{}.md", self.id))
    }

    pub fn open(&self) -> Vec<usize> {
        self.items
            .iter()
            .enumerate()
            .filter(|(_, i)| !i.checked)
            .map(|(n, _)| n + 1)
            .collect()
    }

    pub fn checked(&self) -> Vec<usize> {
        self.items
            .iter()
            .enumerate()
            .filter(|(_, i)| i.checked)
            .map(|(n, _)| n + 1)
            .collect()
    }

    pub fn next_open(&self) -> Option<(usize, &str)> {
        self.items
            .iter()
            .enumerate()
            .find(|(_, i)| !i.checked)
            .map(|(n, i)| (n + 1, i.text.as_str()))
    }

    /// `3/5` style progress pair.
    pub fn progress(&self) -> (usize, usize) {
        (self.checked().len(), self.items.len())
    }

    /// One-line summary for lists and context injection.
    pub fn summary(&self) -> String {
        let (done, total) = self.progress();
        let mut s = format!(
            "{} [{}] {}/{} items",
            self.id,
            self.status.as_str(),
            done,
            total
        );
        if !self.title.is_empty() {
            s.push_str(&format!(" — {}", self.title));
        }
        if self.audit != Audit::None {
            s.push_str(&format!(" · audit: {}", self.audit.as_str()));
        }
        s
    }

    /// Serialize the canonical markdown doc (full rewrite on every save).
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("---\n");
        out.push_str(&format!("id: {}\n", self.id));
        out.push_str(&format!("title: {}\n", self.title));
        out.push_str(&format!("status: {}\n", self.status.as_str()));
        out.push_str(&format!("audit: {}\n", self.audit.as_str()));
        out.push_str(&format!("claim_blocked: {}\n", self.claim_blocked));
        out.push_str(&format!("cwd: {}\n", self.cwd));
        out.push_str(&format!("created: {}\n", self.created));
        out.push_str(&format!("updated: {}\n", self.updated));
        out.push_str("---\n\n# Goal\n\n");
        out.push_str(self.desc.trim());
        out.push_str("\n\n## Checklist\n\n");
        for (n, item) in self.items.iter().enumerate() {
            out.push_str(&format!(
                "{}. [{}] {}\n",
                n + 1,
                if item.checked { "x" } else { " " },
                item.text
            ));
        }
        out.push_str("\n## Audit log\n\n");
        for line in &self.audit_log {
            out.push_str(&format!("- {line}\n"));
        }
        out
    }

    /// Parse a goal doc. Tolerant: missing frontmatter keys default, unknown
    /// keys are ignored, a missing Checklist/Audit section is empty.
    pub fn parse(id_hint: &str, text: &str) -> Goal {
        let mut fm: BTreeMap<String, String> = BTreeMap::new();
        let mut body = text;
        if let Some(end) = text
            .strip_prefix("---\n")
            .and_then(|rest| rest.find("\n---"))
        {
            let rest = &text[4..];
            for line in rest[..end].lines() {
                if let Some((k, v)) = line.split_once(':') {
                    fm.insert(k.trim().to_string(), v.trim().to_string());
                }
            }
            body = &rest[end + 4..];
        }
        let get = |k: &str| fm.get(k).cloned().unwrap_or_default();

        // Body sections: `# Goal` → desc, `## Checklist` → items,
        // `## Audit log` → log lines.
        let mut desc = String::new();
        let mut items = Vec::new();
        let mut audit_log = Vec::new();
        let mut section = "";
        for line in body.lines() {
            let t = line.trim_end();
            if t.starts_with("# ") || t.starts_with("## ") {
                section = if t.starts_with("## Checklist") {
                    "checklist"
                } else if t.starts_with("## Audit") {
                    "audit"
                } else if t.starts_with("# ") {
                    "goal"
                } else {
                    "other"
                };
                continue;
            }
            match section {
                "checklist" => {
                    if let Some(item) = parse_item(t) {
                        items.push(item);
                    }
                }
                "audit" => {
                    if let Some(entry) = t.strip_prefix("- ") {
                        audit_log.push(entry.trim_end().to_string());
                    }
                }
                "goal" => {
                    if !desc.is_empty() {
                        desc.push('\n');
                    }
                    desc.push_str(line);
                }
                _ => {}
            }
        }

        Goal {
            id: {
                let id = get("id");
                if id.is_empty() {
                    id_hint.to_string()
                } else {
                    id
                }
            },
            title: get("title"),
            status: Status::parse(&get("status")),
            audit: Audit::parse(&get("audit")),
            claim_blocked: get("claim_blocked").trim() == "true",
            cwd: get("cwd"),
            created: get("created"),
            updated: get("updated"),
            desc: desc.trim().to_string(),
            items,
            audit_log,
        }
    }
}

/// `1. [x] text` or `- [ ] text` → item. Numbered form keeps the checklist
/// visibly ordered; `-` bullets still parse (hand edits).
fn parse_item(line: &str) -> Option<Item> {
    let rest = line.strip_prefix("- ").or_else(|| {
        let (num, rest) = line.split_once(". ")?;
        num.trim().parse::<usize>().ok()?;
        Some(rest)
    })?;
    let (mark, text) = rest.split_once(']')?;
    let checked = match mark.trim() {
        "[x" | "[X" => true,
        "[ " | "[" => false,
        _ => return None,
    };
    let text = text.trim().to_string();
    if text.is_empty() {
        return None;
    }
    Some(Item { checked, text })
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

pub struct Store {
    pub(crate) home: PathBuf,
}

impl Store {
    pub fn new(home: &Path) -> anyhow::Result<Store> {
        let dir = goals_dir(home);
        if !dir.exists() {
            std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        }
        Ok(Store {
            home: home.to_path_buf(),
        })
    }

    /// Every `*.md` goal, ordered by `updated` descending (falls back to id).
    pub fn list(&self) -> Vec<Goal> {
        let mut goals: Vec<Goal> = std::fs::read_dir(goals_dir(&self.home))
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter_map(|e| {
                        let p = e.path();
                        if p.extension().and_then(|x| x.to_str()) != Some("md") {
                            return None;
                        }
                        let id = p.file_stem()?.to_str()?.to_string();
                        let text = std::fs::read_to_string(&p).ok()?;
                        Some(Goal::parse(&id, &text))
                    })
                    .collect()
            })
            .unwrap_or_default();
        goals.sort_by(|a, b| b.updated.cmp(&a.updated).then(a.id.cmp(&b.id)));
        goals
    }

    /// The "current" goal: most recently updated still-active one.
    pub fn active(&self) -> Option<Goal> {
        self.list().into_iter().find(|g| g.status == Status::Active)
    }

    /// Resolve a user-supplied id or unambiguous id prefix.
    pub fn find(&self, prefix: &str) -> Option<Goal> {
        let all = self.list();
        if let Some(g) = all.iter().find(|g| g.id == prefix) {
            return Some(g.clone());
        }
        let mut hits = all.iter().filter(|g| g.id.starts_with(prefix));
        match (hits.next(), hits.next()) {
            (Some(g), None) => Some(g.clone()),
            _ => None,
        }
    }

    /// `selector` empty → the current goal (active when one exists, else the
    /// most recently updated non-dropped goal — `/goal drop` after `done`
    /// still finds it); else id/prefix lookup.
    pub fn resolve(&self, selector: &str) -> Option<Goal> {
        if selector.is_empty() {
            self.list()
                .into_iter()
                .find(|g| g.status != Status::Dropped)
        } else {
            self.find(selector)
        }
    }

    fn fresh_id(&self) -> String {
        let stamp = chrono::Utc::now().format("%Y%m%d").to_string();
        let rand = format!(
            "{:04x}",
            (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
                .unwrap_or(0)
                ^ std::process::id() as u64)
                & 0xffff
        );
        let mut id = format!("g-{stamp}-{rand}");
        let mut n = 0u32;
        while goals_dir(&self.home).join(format!("{id}.md")).exists() {
            n += 1;
            id = format!("g-{stamp}-{rand}-{n}");
        }
        id
    }

    /// Create an active goal doc. `title` = first ~60 chars of the objective.
    pub fn create(&self, desc: &str, cwd: &Path) -> anyhow::Result<Goal> {
        let now = now_iso();
        let goal = Goal {
            id: self.fresh_id(),
            title: desc
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(60)
                .collect(),
            status: Status::Active,
            audit: Audit::None,
            claim_blocked: false,
            cwd: cwd.to_string_lossy().into_owned(),
            created: now.clone(),
            updated: now,
            desc: desc.trim().to_string(),
            items: Vec::new(),
            audit_log: Vec::new(),
        };
        self.save(&goal)?;
        Ok(goal)
    }

    /// Atomic-ish persist: temp file + rename inside the goals dir.
    pub fn save(&self, goal: &Goal) -> anyhow::Result<()> {
        let dir = goals_dir(&self.home);
        let path = goal.path(&self.home);
        let tmp = dir.join(format!(".{}.tmp", goal.id));
        std::fs::write(&tmp, goal.render()).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("rename to {}", path.display()))?;
        Ok(())
    }

    /// Load, mutate, bump `updated`, save.
    pub fn update(&self, goal: &mut Goal, f: impl FnOnce(&mut Goal)) -> anyhow::Result<()> {
        f(goal);
        goal.updated = now_iso();
        self.save(goal)
    }
}

// ---------------------------------------------------------------------------
// Wire surface
// ---------------------------------------------------------------------------

/// Model-facing `goal` tool: progress recording + the completion claim that
/// the nudge gates.
pub fn tool_definition() -> serde_json::Value {
    serde_json::json!({
        "name": TOOL_NAME,
        "label": "Goal",
        "description": "Read and update the persistent goal's ordered checklist, and claim completion. \
            Actions: `status` (active goal summary), `set_items` (replace the whole checklist — pass `items`), \
            `add_item` (append — pass `item`), `check_item` / `uncheck_item` (toggle — pass `n`, the 1-based item number), \
            `complete` (claim the goal done; refused while items stay open — an independent auditor, /goal audit, does the grading).",
        "parameters": {
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["status", "set_items", "add_item", "check_item", "uncheck_item", "complete"],
                    "description": "Default: status."
                },
                "goal_id": {
                    "type": "string",
                    "description": "Goal id or unambiguous prefix; default is the active goal."
                },
                "items": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "set_items: the full ordered checklist."
                },
                "item": {"type": "string", "description": "add_item: text of the item to append."},
                "n": {"type": "integer", "description": "check_item/uncheck_item: 1-based item number."}
            }
        }
    })
}

/// The `plugin/manifest` result value (discord-shaped: sidecar_args + tools
/// + hooks + completion).
pub fn manifest_goal() -> serde_json::Value {
    serde_json::json!({
        "name": PLUGIN_NAME,
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": PROTOCOL,
        "sidecar_args": ["sidecar"],
        "tools": [tool_definition()],
        "commands": COMMANDS,
        "hooks": ["prompt/context", "agent/before_start"],
        "completion": COMPLETION,
    })
}

/// What the model can't know on its own: that persistent goals exist and
/// which verbs drive them. Kept to two lines — state rides below it.
pub const GOAL_POLICY: &str = "Goals: persistent task goals live in ~/.gray/goals and survive sessions. \
    Record checklist progress with the `goal` tool and claim completion through it — \
    completion is never self-graded: `/goal audit` runs an independent check. \
    `/goal <desc>` starts a goal; also `list`, `show`, `done`, `drop`.";

/// The `prompt/context` block: policy + the active goal and its next
/// unchecked item + last audit verdict.
pub fn served_text(store: &Store) -> String {
    let mut text = GOAL_POLICY.to_string();
    let Some(goal) = store.active() else {
        return text;
    };
    let (done, total) = goal.progress();
    text.push_str(&format!(
        "\nActive goal {} — \"{}\" ({}/{} items done).",
        goal.id, goal.title, done, total
    ));
    if let Some((n, item)) = goal.next_open() {
        text.push_str(&format!(" Next: #{n} {item}."));
    }
    match goal.audit {
        Audit::Fail => text.push_str(" Last independent audit FAILED — see the goal's audit log before claiming completion again."),
        Audit::Pass => text.push_str(" Last independent audit passed."),
        Audit::Running => text.push_str(" An independent audit is running."),
        Audit::Error => text.push_str(" The last audit errored; rerun /goal audit."),
        Audit::None => {}
    }
    text
}

/// The `agent/before_start` nudge: fires once after a refused completion
/// claim while open items remain. Clears `claim_blocked` so it nags once
/// per claim, not every turn.
pub fn completion_nudge(store: &Store) -> Option<String> {
    let mut goal = store.active()?;
    if !goal.claim_blocked {
        return None;
    }
    let open = goal.open();
    let _ = store.update(&mut goal, |g| g.claim_blocked = false);
    if open.is_empty() {
        return None;
    }
    let next = goal
        .next_open()
        .map(|(n, t)| format!("#{n} {t}"))
        .unwrap_or_default();
    Some(format!(
        "Goal \"{}\" ({}) was claimed complete but {} item(s) are still open — next: {next}. \
         Verify and check them off with the goal tool, adjust the plan, or ask for `/goal drop`. \
         Completion is audited independently (`/goal audit`), not self-graded.",
        goal.title,
        goal.id,
        open.len()
    ))
}
