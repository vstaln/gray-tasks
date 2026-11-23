# gray-tasks

One sidecar for every task-tracking and multi-step driver in the gray agent
harness. It replaces eight standalone plugins, each kept as a **part** that can
be switched on or off on its own.

| Part | Was | Tools | Commands | State |
|---|---|---|---|---|
| `todo` | `gray-todo` | `todo` | `/todos` | `~/.gray/todos/` |
| `kanban` | `gray-kanban` | — | `/kanban` | `~/.gray/kanban/` |
| `goal` | `gray-goal` | `goal` | `/goal` | `~/.gray/goals/` |
| `loop` | `gray-loop` | `loop_done`, `loop_wakeup`, `loop_task`, `loop_learn` | `/loop` | `~/.gray/loop/` |
| `gsd` | `gray-gsd` | `gsd_task` | `/gsd` | `<cwd>/.gsd/` |
| `workflows` | `gray-workflows` | — | `/workflow`, `/workflows`, `/deep-research` | `<cwd>/.gray/workflows/` |
| `plan` | `gray-plan` | — | `/plan` | `~/.gray/plan/` |
| `plannotator` | `gray-plannotator` | — | `/plan-review`, `/plan-accept` | `<cwd>/.gray/plans/` |

`/tasks` is the merged overview. Every part is **on by default**.

## Turning parts on and off

Parts are toggled through gray core, which is the only writer of the state file
`$GRAY_HOME/plugins/parts.json` (overrides only):

```sh
gray plugin parts tasks              # table: part, on/off, default, description
gray plugin disable tasks/loop       # also: gray plugin disable loop
gray plugin enable tasks/loop        # also: gray plugin enable loop
```

At install time the picker offers the parts, `--parts todo,goal,loop` turns on
exactly those, and `--yes` (or no TTY) takes the defaults without writing
anything. Toggles apply from the next session.

The file itself holds only overrides, so anything absent keeps its default:

```json
{ "tasks": { "loop": false, "plannotator": false } }
```

A disabled part contributes nothing to the manifest — no tools, no slash
commands, no completion words, no hooks — receives no events, and a tool or
command naming one of its tools answers the normal unknown-tool or
unknown-command error. Enabled parts keep the names, state directories and
behaviour they had as standalone plugins.

## CLI

`gray-tasks <part> <args…>` runs that part's former CLI. Bare `gray tasks` on a
terminal lists the parts and their state.

```sh
gray tasks goal list
gray tasks loop status
gray tasks workflows list
gray tasks plannotator status
gray tasks manifest          # the merged manifest, enabled parts only
```

`todo`, `kanban` and `gsd` have no CLI of their own; they answer the sidecar
wire only, so their tools and slash commands are the whole surface. The `loop`
part also keeps gray's status widget (`gray tasks widget`).

## Hooks

Hook results are merged the way the merged plugins always did: text fields from
every enabled owner are joined with a blank line.

| Hook | Parts |
|---|---|
| `prompt/context` | goal, workflows, plannotator |
| `agent/before_start` | goal, loop, plan |
| `tool/before` | plan (read-only gate) |
| `event/notify` | loop (`turn_end` paces self-scheduled loops) |

## Layout

`src/main.rs` is the shared dispatcher: it reads the parts state once at start,
builds the manifest from the enabled parts, and routes `tool/call`,
`command/run`, the hooks and the part CLIs. `src/parts.rs` owns the on/off
contract. Each part's source lives under `src/mods/`, nearly verbatim from its
standalone repo, with only the entry points renamed (`manifest_<part>`,
`handle_<part>`).

## Development

```sh
cargo fmt
cargo build --release
gray plugin check .          # conformance probe: the plugin-dev plugin
```

The tests are the ported ones from each part; `todo`, `kanban` and `loop` touch
`GRAY_HOME`, so `RUST_TEST_THREADS=1` in `.cargo/config.toml` keeps them serial.
