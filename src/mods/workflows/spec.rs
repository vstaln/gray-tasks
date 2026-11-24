//! Workflow spec parsing: a yaml/json task list.
//!
//! Shape (both files and the `/deep-research` preset land here):
//!
//! ```yaml
//! name: my-batch            # optional; defaults to the file stem
//! max_running: 3            # optional cap inside the global subagents cap
//! items:                    # required, 1-64 entries (alias: tasks)
//!   - prompt: "what to do"  # required (alias: task)
//!     name: label           # optional, [a-zA-Z0-9][a-zA-Z0-9_-]{0,63}
//!     agent: worker         # optional, default "worker"
//!     model: provider/model # optional
//!     effort: low           # optional
//!     worktree: true        # optional per-item git worktree
//!     phase: 0              # optional; later phases start after earlier ones
//! ```
//!
//! A bare top-level array is accepted as `items`.

use anyhow::{Context, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;

pub const MAX_ITEMS: usize = 64;
pub const MAX_PROMPT: usize = 32000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecItem {
    pub prompt: String,
    pub name: Option<String>,
    pub agent: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub worktree: bool,
    pub phase: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct Spec {
    pub name: Option<String>,
    pub max_running: Option<u32>,
    pub items: Vec<SpecItem>,
}

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .enumerate()
            .all(|(i, b)| b.is_ascii_alphanumeric() || (i > 0 && (b == b'-' || b == b'_')))
}

/// yaml-rust2 value → serde_json (pure-Rust yaml; no libyaml).
fn yaml_to_json(y: &yaml_rust2::Yaml) -> Value {
    use yaml_rust2::Yaml;
    match y {
        Yaml::Real(s) => s
            .parse::<f64>()
            .map(|f| json!(f))
            .unwrap_or_else(|_| json!(s)),
        Yaml::Integer(i) => json!(i),
        Yaml::String(s) => json!(s),
        Yaml::Boolean(b) => json!(b),
        Yaml::Array(a) => Value::Array(a.iter().map(yaml_to_json).collect()),
        Yaml::Hash(h) => {
            let mut map = serde_json::Map::new();
            for (k, v) in h {
                let key = match k {
                    Yaml::String(s) => s.clone(),
                    other => yaml_to_json(other).to_string(),
                };
                map.insert(key, yaml_to_json(v));
            }
            Value::Object(map)
        }
        _ => Value::Null,
    }
}

/// Try JSON first (strict errors point at the real problem), then yaml.
pub fn parse(text: &str) -> anyhow::Result<Spec> {
    let value: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(json_err) => {
            let docs = yaml_rust2::YamlLoader::load_from_str(text).map_err(|e| {
                anyhow!("spec is neither JSON nor YAML: {e}; JSON error was: {json_err}")
            })?;
            docs.into_iter()
                .next()
                .map(|d| yaml_to_json(&d))
                .ok_or_else(|| anyhow!("empty spec file"))?
        }
    };
    let items_value = if value.is_array() {
        value.clone()
    } else {
        value
            .get("items")
            .or_else(|| value.get("tasks"))
            .cloned()
            .ok_or_else(|| anyhow!("spec needs an 'items' list"))?
    };
    let arr = items_value
        .as_array()
        .ok_or_else(|| anyhow!("'items' must be a list"))?;
    anyhow::ensure!(
        !arr.is_empty() && arr.len() <= MAX_ITEMS,
        "items must contain 1–{MAX_ITEMS} entries"
    );
    let mut items = Vec::with_capacity(arr.len());
    for (i, raw) in arr.iter().enumerate() {
        let obj = raw
            .as_object()
            .ok_or_else(|| anyhow!("item {i} must be an object"))?;
        let str_field = |keys: &[&str]| -> Option<String> {
            keys.iter().find_map(|k| {
                obj.get(*k)
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .filter(|s| !s.is_empty())
            })
        };
        let prompt = str_field(&["prompt", "task"])
            .ok_or_else(|| anyhow!("item {i} needs a nonempty 'prompt'"))?;
        anyhow::ensure!(
            prompt.len() <= MAX_PROMPT && !prompt.contains('\0'),
            "item {i} prompt must be 1–{MAX_PROMPT} chars, no NUL"
        );
        let name = str_field(&["name"]);
        if let Some(n) = &name {
            anyhow::ensure!(
                valid_name(n),
                "item {i} name must match [a-zA-Z0-9][a-zA-Z0-9_-]{{0,63}}"
            );
        }
        let worktree = obj
            .get("worktree")
            .map(|v| v.as_bool().unwrap_or(false))
            .unwrap_or(false);
        let phase = obj
            .get("phase")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(1000) as u32;
        items.push(SpecItem {
            prompt,
            name,
            agent: str_field(&["agent"]).unwrap_or_else(|| "worker".into()),
            model: str_field(&["model"]),
            effort: str_field(&["effort"]),
            worktree,
            phase,
        });
    }
    let max_running = if value.is_array() {
        None
    } else {
        value
            .get("max_running")
            .or_else(|| value.get("max-running"))
            .or_else(|| value.get("concurrency"))
            .and_then(Value::as_u64)
            .map(|n| n.clamp(1, 32) as u32)
    };
    let name = if value.is_array() {
        None
    } else {
        value
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .filter(|s| !s.is_empty())
    };
    Ok(Spec {
        name,
        max_running,
        items,
    })
}

pub fn parse_file(path: &Path) -> anyhow::Result<Spec> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read spec {}", path.display()))?;
    parse(&text)
}

/// `/deep-research <topic>` preset: three parallel research passes, then a
/// synthesis item that inherits their results via phase gating.
pub fn deep_research(topic: &str) -> Spec {
    let angle = |focus: &str| SpecItem {
        prompt: format!(
            "Research '{topic}': {focus}. Return structured findings with specifics \
             (names, versions, numbers, citations where possible)."
        ),
        name: None,
        agent: "scout".into(),
        model: None,
        effort: Some("low".into()),
        worktree: false,
        phase: 0,
    };
    Spec {
        name: Some(format!("deep-research: {topic}")),
        max_running: None,
        items: vec![
            angle(
                "survey the landscape — key concepts, history, major approaches, \
                   important projects/papers/tools",
            ),
            angle(
                "dig into the technical details — how the leading approaches actually \
                   work, their trade-offs and comparisons",
            ),
            angle(
                "find criticisms, limitations, alternatives, open questions and \
                   recent developments",
            ),
            SpecItem {
                prompt: format!(
                    "Synthesize the research findings below into a well-structured report \
                     on '{topic}': an overview, key findings, trade-offs, and open \
                     questions. Return the report text as your final answer."
                ),
                name: Some("synthesis".into()),
                agent: "oracle".into(),
                model: None,
                effort: None,
                worktree: false,
                phase: 1,
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_items_parse() {
        let s = parse(
            r#"{"items":[{"prompt":"a","worktree":true},{"task":"b","agent":"scout","phase":1}]}"#,
        )
        .unwrap();
        assert_eq!(s.items.len(), 2);
        assert!(s.items[0].worktree);
        assert_eq!(s.items[1].agent, "scout");
        assert_eq!(s.items[1].phase, 1);
    }

    #[test]
    fn yaml_items_parse() {
        let s = parse("name: demo\nmax_running: 2\nitems:\n  - prompt: one\n    worktree: true\n  - task: two\n    effort: low\n").unwrap();
        assert_eq!(s.name.as_deref(), Some("demo"));
        assert_eq!(s.max_running, Some(2));
        assert_eq!(s.items.len(), 2);
        assert!(s.items[0].worktree);
        assert_eq!(s.items[1].effort.as_deref(), Some("low"));
    }

    #[test]
    fn bare_array_parse() {
        let s = parse("- prompt: x\n- prompt: y\n").unwrap();
        assert_eq!(s.items.len(), 2);
    }

    #[test]
    fn rejects_bad_specs() {
        assert!(parse("").is_err());
        assert!(parse("items: []").is_err());
        assert!(parse("items:\n  - {}").is_err());
        assert!(parse("items:\n  - prompt: x\n    name: '-bad'").is_err());
    }

    #[test]
    fn preset_has_four_items() {
        let s = deep_research("rust async");
        assert_eq!(s.items.len(), 4);
        assert_eq!(s.items[3].phase, 1);
        assert_eq!(s.items[3].agent, "oracle");
    }
}
