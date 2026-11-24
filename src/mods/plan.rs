//! gray-plan — read-only exploration mode ("plan mode").
//!
//! `/plan` toggles; state persists at ~/.gray/plan/enabled (honors
//! $GRAY_HOME). While on, `tool/before` denies `edit`, `write`, and every
//! tool outside a read-only set; `bash` is allowed only when every
//! pipe/chain segment matches a safe pattern and no segment matches a
//! destructive one.
//!
//! `agent/before_start` injects a [PLAN MODE ACTIVE] note so the model
//! knows why writes deny.

use std::path::PathBuf;
use std::sync::OnceLock;

use serde_json::{Value, json};

const DENY_REASON: &str = "plan mode is on — read-only; /plan to exit";

/// Read-only tool allowlist for plan mode. Everything else denies.
/// `bash` is in the set but gated by SAFE/DESTRUCTIVE command patterns.
const READ_ONLY_TOOLS: &[&str] = &[
    "bash",
    "read",
    "grep",
    "find",
    "ls",
    "view",
    "web_search",
    "web_fetch",
    "recall",
];

/// Destructive command patterns for Rust regex.
/// `(^|[^<])>(?!>)` becomes `(^|[^<])>($|[^>])` (no lookahead in `regex`).
const DESTRUCTIVE_PATTERNS: &[&str] = &[
    r"(?i)\brm\b",
    r"(?i)\brmdir\b",
    r"(?i)\bmv\b",
    r"(?i)\bcp\b",
    r"(?i)\bmkdir\b",
    r"(?i)\btouch\b",
    r"(?i)\bchmod\b",
    r"(?i)\bchown\b",
    r"(?i)\bchgrp\b",
    r"(?i)\bln\b",
    r"(?i)\btee\b",
    r"(?i)\btruncate\b",
    r"(?i)\bdd\b",
    r"(?i)\bshred\b",
    r"(^|[^<])>($|[^>])",
    r">>",
    r"(?i)\bnpm\s+(install|uninstall|update|ci|link|publish)",
    r"(?i)\byarn\s+(add|remove|install|publish)",
    r"(?i)\bpnpm\s+(add|remove|install|publish)",
    r"(?i)\bpip\s+(install|uninstall)",
    r"(?i)\bapt(-get)?\s+(install|remove|purge|update|upgrade)",
    r"(?i)\bbrew\s+(install|uninstall|upgrade)",
    r"(?i)\bgit\s+(add|commit|push|pull|merge|rebase|reset|checkout|branch\s+-[dD]|stash|cherry-pick|revert|tag|init|clone)",
    r"(?i)\bsudo\b",
    r"(?i)\bsu\b",
    r"(?i)\bkill\b",
    r"(?i)\bpkill\b",
    r"(?i)\bkillall\b",
    r"(?i)\breboot\b",
    r"(?i)\bshutdown\b",
    r"(?i)\bsystemctl\s+(start|stop|restart|enable|disable)",
    r"(?i)\bservice\s+\S+\s+(start|stop|restart)",
    r"(?i)\b(vim?|nano|emacs|code|subl)\b",
];

/// Safe read-only command patterns.
const SAFE_PATTERNS: &[&str] = &[
    r"^\s*cat\b",
    r"^\s*head\b",
    r"^\s*tail\b",
    r"^\s*less\b",
    r"^\s*more\b",
    r"^\s*grep\b",
    r"^\s*find\b",
    r"^\s*ls\b",
    r"^\s*pwd\b",
    r"^\s*echo\b",
    r"^\s*printf\b",
    r"^\s*wc\b",
    r"^\s*sort\b",
    r"^\s*uniq\b",
    r"^\s*diff\b",
    r"^\s*file\b",
    r"^\s*stat\b",
    r"^\s*du\b",
    r"^\s*df\b",
    r"^\s*tree\b",
    r"^\s*which\b",
    r"^\s*whereis\b",
    r"^\s*type\b",
    r"^\s*env\b",
    r"^\s*printenv\b",
    r"^\s*uname\b",
    r"^\s*whoami\b",
    r"^\s*id\b",
    r"^\s*date\b",
    r"^\s*cal\b",
    r"^\s*uptime\b",
    r"^\s*ps\b",
    r"^\s*top\b",
    r"^\s*htop\b",
    r"^\s*free\b",
    r"(?i)^\s*git\s+(status|log|diff|show|branch|remote|config\s+--get)",
    r"(?i)^\s*git\s+ls-",
    r"(?i)^\s*npm\s+(list|ls|view|info|search|outdated|audit)",
    r"(?i)^\s*yarn\s+(list|info|why|audit)",
    r"(?i)^\s*node\s+--version",
    r"(?i)^\s*python\s+--version",
    r"(?i)^\s*curl\s",
    r"(?i)^\s*wget\s+-O\s*-",
    r"^\s*jq\b",
    r"(?i)^\s*sed\s+-n",
    r"^\s*awk\b",
    r"^\s*rg\b",
    r"^\s*fd\b",
    r"^\s*bat\b",
    r"^\s*eza\b",
];

fn compiled(list: &[&str]) -> &'static [regex::Regex] {
    // Each call site uses its own OnceLock keyed by list identity.
    static CACHE: OnceLock<Vec<regex::Regex>> = OnceLock::new();
    static CACHE2: OnceLock<Vec<regex::Regex>> = OnceLock::new();
    let slot = if std::ptr::eq(list, DESTRUCTIVE_PATTERNS) {
        &CACHE
    } else {
        &CACHE2
    };
    slot.get_or_init(|| {
        list.iter()
            .filter_map(|p| regex::Regex::new(p).ok())
            .collect()
    })
}

fn is_destructive(segment: &str) -> bool {
    compiled(DESTRUCTIVE_PATTERNS)
        .iter()
        .any(|p| p.is_match(segment))
}

fn is_safe_segment(segment: &str) -> bool {
    !is_destructive(segment) && compiled(SAFE_PATTERNS).iter().any(|p| p.is_match(segment))
}

/// Split a command line into pipe/chain segments: `|`, `||`, `&&`, `;`,
/// and newlines. A segment only needs to be safe if it's non-empty.
fn segments(command: &str) -> Vec<&str> {
    command
        .split(['|', '&', ';', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

/// Plan-mode bash verdict: every segment must match a SAFE pattern and no
/// segment may match a DESTRUCTIVE one.
fn is_safe_command(command: &str) -> bool {
    let segs = segments(command);
    !segs.is_empty() && segs.iter().all(|s| is_safe_segment(s))
}

fn gray_home() -> PathBuf {
    std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".gray")))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn flag_path() -> PathBuf {
    gray_home().join("plan").join("enabled")
}

fn plan_mode_on() -> bool {
    flag_path().exists()
}

fn set_plan_mode(on: bool) -> std::io::Result<()> {
    let flag = flag_path();
    if on {
        if let Some(dir) = flag.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&flag, b"")
    } else {
        match std::fs::remove_file(&flag) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            r => r,
        }
    }
}

fn tool_before(params: &Value) -> Value {
    if !plan_mode_on() {
        return json!({"decision": "allow"});
    }
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("args").cloned().unwrap_or(json!({}));
    if name == "bash" {
        let command = args.get("command").and_then(Value::as_str).unwrap_or("");
        if is_safe_command(command) {
            return json!({"decision": "allow"});
        }
        return json!({"decision": "deny", "reason": DENY_REASON});
    }
    if READ_ONLY_TOOLS.contains(&name) {
        json!({"decision": "allow"})
    } else {
        json!({"decision": "deny", "reason": DENY_REASON})
    }
}

/// `/plan …` — `argv` excludes the command name.
fn run_command(argv: &[&str]) -> String {
    match argv.first().copied() {
        Some("status") => {
            format!(
                "plan mode {} — read-only set: {} (+ gated bash) · /plan toggles",
                if plan_mode_on() { "ON" } else { "off" },
                READ_ONLY_TOOLS.join(", ")
            )
        }
        Some("on") | Some("off") => {
            let want = argv[0] == "on";
            match set_plan_mode(want) {
                Ok(()) => format!("plan mode {want}"),
                Err(e) => format!("couldn't flip state: {e}"),
            }
        }
        None => match set_plan_mode(!plan_mode_on()) {
            Ok(()) => format!(
                "plan mode {}",
                if plan_mode_on() {
                    "on — read-only; writes deny"
                } else {
                    "off — full access"
                }
            ),
            Err(e) => format!("couldn't flip state: {e}"),
        },
        Some(_) => "usage: /plan [on|off|status] — bare /plan toggles".into(),
    }
}

pub fn manifest_plan() -> Value {
    json!({
        "name": "plan",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "2.0",
        "tools": [],
        "description": "read-only plan mode before you edit",
        "commands": ["/plan"],
        "command_descriptions": {"/plan": "toggle read-only plan mode"},
        "hooks": ["tool/before", "agent/before_start"],
    })
}

/// One request → `Some(reply)`, or `None` for notifications. The bool asks
/// the loop to exit after writing the reply.
pub fn handle_plan(req: &Value) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest_plan(),
        "tool/before" => tool_before(&params),
        "agent/before_start" => {
            if plan_mode_on() {
                json!({ "text": "[PLAN MODE ACTIVE] You are in plan mode — a read-only exploration mode. edit/write and mutating tools deny; bash is restricted to read-only commands. Do NOT attempt changes — explore and present a plan. /plan exits." })
            } else {
                json!({})
            }
        }
        "command/run" => {
            let argv: Vec<&str> = params
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            json!({ "text": run_command(&argv) })
        }
        "plugin/shutdown" => return (Some(json!({ "id": id, "result": {} })), true),
        _ => {
            let error = json!({ "code": -32601, "message": "method not found" });
            return (Some(json!({ "id": id, "error": error })), false);
        }
    };
    (Some(json!({ "id": id, "result": result })), false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_shape() {
        let m = manifest_plan();
        assert_eq!(m["name"], "plan");
        assert_eq!(m["protocol"], "2.0");
        assert_eq!(m["hooks"], json!(["tool/before", "agent/before_start"]));
        assert_eq!(m["commands"], json!(["/plan"]));
    }

    #[test]
    fn safe_commands_pass() {
        for cmd in [
            "ls -la",
            "cat foo.rs",
            "git status",
            "git log --oneline -5",
            "rg foo src/",
            "cat a | grep b",
            "ls && pwd",
            "echo hi; wc -l f.txt",
            "sed -n '1,5p' f",
            "awk '{print $1}' f",
            "git branch",
        ] {
            assert!(is_safe_command(cmd), "should be safe: {cmd}");
        }
    }

    #[test]
    fn destructive_commands_fail() {
        for cmd in [
            "rm -rf /",
            "cat a > b",
            "cat a >> b",
            "mkdir x",
            "git commit -m x",
            "git checkout main",
            "sudo ls",
            "npm install",
            "echo hi | tee f",
            "ls; rm x",
            "cat a | rm x",
            "vi f",
            "kill 1234",
            "mv a b",
        ] {
            assert!(!is_safe_command(cmd), "should be unsafe: {cmd}");
        }
    }

    #[test]
    fn unknown_and_empty_fail() {
        assert!(!is_safe_command(""));
        assert!(!is_safe_command("   "));
        assert!(!is_safe_command("cargo test"));
        assert!(!is_safe_command("|"));
    }

    #[test]
    fn tool_names_gate() {
        // Not on disk → allow everything.
        let off = tool_before(&json!({"name": "write", "args": {"path": "x", "content": "y"}}));
        assert_eq!(off["decision"], "allow");
    }

    #[test]
    fn deny_reason_wording() {
        assert!(DENY_REASON.contains("plan mode is on"));
        assert!(DENY_REASON.contains("/plan to exit"));
    }
}
