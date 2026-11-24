//! `gray-workflows` — CLI + JSONL sidecar (gray wire v1.1).
//!
//! Wire (plannotator/subagents shape):
//! - `{"id":N,"method":"plugin/manifest"}` → `{"id":N,"result":{…}}`
//! - `{"id":N,"method":"prompt/context","params":{"cwd",…}}` → `{"text"}`
//! - `{"id":N,"method":"command/run","params":{"name","argv","session"}}`
//!   → `{"text"}`; `/workflow` maps to `run`, `/deep-research` to
//!   `deep-research`, `/workflows` parses its own argv
//! - `{"method":"plugin/shutdown"}` (no id): exit cleanly
//!
//! Bare invocation is the sidecar (what `gray plugin check` spawns);
//! `sidecar` is the installed `sidecar_args` form; `manifest` prints the
//! manifest for installers; `gray workflows <verb>` is the CLI surface.

use crate::workflows as wf;
use crate::workflows::orchestrate;
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "gray-workflows",
    about = "Batch/durable multi-agent workflows for gray",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Action>,
}

#[derive(Subcommand)]
enum Action {
    /// Start a workflow from a yaml/json spec file.
    Run {
        specfile: String,
        #[arg(long)]
        max_running: Option<u32>,
    },
    /// Show one workflow's ledger (default: newest in this directory).
    Status { id: Option<String> },
    /// List workflows under this directory.
    List,
    /// Re-drive an interrupted workflow (adopts live runs, relaunches lost).
    Resume { id: String },
    /// Stop launching and signal the workflow's running subagents.
    Stop { id: String },
    /// Print the aggregate report (or live status while running).
    Report { id: String },
    /// Preset: parallel research passes + a synthesis item on the results.
    DeepResearch {
        #[arg(required = true, num_args = 1..)]
        topic: Vec<String>,
    },
    /// Detached supervisor loop — spawned by run/resume, not for humans.
    #[command(hide = true)]
    Orchestrate { ledger: PathBuf },
    /// Print the plugin manifest (installers probe this).
    Manifest,
}

/// Session cwd from `params.session.cwd` / `params.cwd`, else our own.
fn session_cwd(params: &Value) -> PathBuf {
    params
        .get("session")
        .and_then(|s| s.get("cwd"))
        .and_then(Value::as_str)
        .or_else(|| params.get("cwd").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn execute(action: Action, cwd: &Path) -> anyhow::Result<Value> {
    match action {
        Action::Run {
            specfile,
            max_running,
        } => wf::run_workflows(cwd, &specfile, max_running),
        Action::Status { id } => wf::status(cwd, id.as_deref()),
        Action::List => wf::list(cwd),
        Action::Resume { id } => wf::resume(cwd, &id),
        Action::Stop { id } => wf::stop(cwd, &id),
        Action::Report { id } => wf::report(cwd, &id),
        Action::DeepResearch { topic } => wf::deep_research(cwd, &topic.join(" ")),
        Action::Orchestrate { ledger } => {
            orchestrate::orchestrate(&ledger).map(|_| json!({"text": ""}))
        }
        Action::Manifest => Ok(wf::manifest_workflows()),
    }
}

pub fn handle_workflows(req: &Value) -> Option<Value> {
    let id = req.get("id").cloned()?;
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "plugin/manifest" => Ok(wf::manifest_workflows()),
        "prompt/context" => wf::prompt_context(&session_cwd(&params)),
        "command/run" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let argv: Vec<String> = params
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            let mapped: Vec<String> = match name {
                "/workflow" => {
                    if argv.is_empty() {
                        return Some(json!({
                            "id": id,
                            "result": {"text": "usage: /workflow <spec.yaml|json> — items: prompt, model?, effort?, worktree?, phase?"},
                        }));
                    }
                    std::iter::once("run".to_string()).chain(argv).collect()
                }
                "/deep-research" => std::iter::once("deep-research".to_string())
                    .chain(argv)
                    .collect(),
                "/workflows" | "workflows" => {
                    if argv.is_empty() {
                        vec!["status".to_string()]
                    } else {
                        argv
                    }
                }
                _ => {
                    return Some(json!({
                        "id": id,
                        "error": format!("unknown command: {name}"),
                    }));
                }
            };
            let full: Vec<String> = std::iter::once("gray-workflows".to_string())
                .chain(mapped)
                .collect();
            Cli::try_parse_from(full)
                .map_err(anyhow::Error::from)
                .and_then(|cli| {
                    let cwd = session_cwd(&params);
                    execute(cli.command.unwrap_or(Action::List), &cwd)
                })
        }
        _ => {
            return Some(json!({
                "id": id,
                "error": {"code": -32601, "message": "method not found"},
            }));
        }
    };
    Some(match result {
        Ok(v) => json!({"id": id, "result": v}),
        Err(e) => json!({"id": id, "error": e.to_string()}),
    })
}

fn sidecar() -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        // `plugin/shutdown` is a notification (no id): stop cleanly.
        if req.get("method").and_then(Value::as_str) == Some("plugin/shutdown") {
            break;
        }
        if let Some(reply) = handle_workflows(&req) {
            writeln!(stdout, "{reply}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

pub fn main_workflows(argv: Vec<String>) -> anyhow::Result<()> {
    match argv.get(1).map(String::as_str) {
        None | Some("sidecar") => sidecar().map_err(anyhow::Error::from),
        _ => {
            let cli = match Cli::try_parse_from(argv) {
                Ok(c) => c,
                Err(e) => {
                    let _ = e.print();
                    std::process::exit(if e.use_stderr() { 1 } else { 0 });
                }
            };
            match cli.command {
                Some(action) => {
                    let cwd = std::env::current_dir()?;
                    let v = execute(action, &cwd)?;
                    if let Some(text) = v["text"].as_str() {
                        println!("{text}");
                    } else {
                        println!("{}", serde_json::to_string_pretty(&v)?);
                    }
                    Ok(())
                }
                None => sidecar().map_err(anyhow::Error::from),
            }
        }
    }
}
