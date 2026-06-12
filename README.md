# ATS — Agent Terminal Suite

A Rust TUI for orchestrating many concurrent Claude Code sessions with a calm,
pull-based attention UX. Daemon/client architecture: killing the UI never kills
an agent.

**Full design:** [docs/PLAN.md](docs/PLAN.md)

## Principles

1. **Pull-based attention** — nothing animates, flashes, or notifies by default.
2. **Keyboard-first** — every action in ≤2 keystrokes.
3. **Crash-safe sessions** — the daemon owns all agent processes.
4. **Calm status** — one dim glyph per session: `·` working, `○` idle, `●` finished, `!` needs input.
5. **Cheap workspaces** — spawning a ready-to-work clone takes seconds.

## Workspace layout

| Crate | What |
|---|---|
| `crates/ats-core` | shared types, RPC protocol, config — the contract everything builds against |
| `crates/ats-daemon` | Tokio daemon: PTY sessions, workspace clones, transcript watching, orchestrator |
| `crates/ats-tui` | Ratatui client: left rail + two tab groups of five sessions |
| `crates/ats-cli` | headless RPC client: `ats spawn`, `ats status`, `ats harvest`, ... |

## Building

```
cargo build --workspace
cargo test --workspace
```

## Quick start

```sh
# 1. register a template (a blessed, setup-complete local clone)
ats register api-core ~/repos/api-core

# 2. launch the TUI — it auto-starts the daemon if needed
ats-tui

# 3. inside the TUI: Alt+s → pick template → workspace cloned, session
#    spawned in the next free tab. F1 for all keybindings.
```

Headless: `ats spawn api-core`, `ats status`, `ats scrollback <id>`,
`ats harvest <ws>`, `ats destroy <ws>`.

Config lives in `./ats.toml` or `~/.ats/ats.toml` (see `ats.example.toml`);
`session_cmd` defaults to `claude`. Override the socket with `$ATS_SOCKET`,
the data dir with `$ATS_DATA_DIR`.

## Keybindings (defaults)

| Key | Action |
|---|---|
| `Alt+1..5` / `Alt+6..0` | jump to tab in group A / B |
| `` Alt+` `` | toggle focus group A ↔ B |
| `Alt+r` | focus rail |
| `Alt+s` | spawn: template → workspace → session |
| `Alt+q` | review queue (Enter = jump) |
| `Alt+n` | notes: n new, e edit, f finalize, Enter send to session |
| `Alt+p` | prompt palette: type to filter, Enter paste |
| `Alt+d` | one-line digest of the active session |
| `Alt+o` | orchestrator: ask a question across all sessions |
| `Alt+h` | harvest the active workspace → scrollable diff viewer |
| `Alt+Esc` | raw mode (all keys to the PTY) |
| `Alt+x` | detach UI — agents keep running |
| `F1` | help |

## Orchestrator

Set `ANTHROPIC_API_KEY` to enable the orchestrator. `Alt+o` (or
`ats orch "..."`) opens an **interactive chat with tools**: the orchestrator
can register templates, spawn sessions, type instructions into any or all
sessions, read what they're doing, and harvest results. Setup and
fleet-wide workflows are one instruction away:

```
❯ register ~/repos/api-core as api-core, spawn 3 sessions,
  and have each one pick a different module from docs/TODO.md
❯ tell every working session to commit, push, and post a one-line status
❯ which sessions are blocked, and on what?
```

Tool calls stream live into the panel (`→ spawn_session {...}`); the
conversation persists in the daemon across panel closes and clients
(`Ctrl+r` / `ats orch --reset` clears it). Each instruction is capped at
12 tool rounds.

Also available: one-line digests (`Alt+d` / `ats digest <n>` — heuristic
for short reports, no API call), one-shot questions (`ats ask`), and
re-entry briefings (`ats reentry <n>` → drafted as a note). `auto_digest`
on finish is opt-in in `ats.toml` (calm by default).

## Power features

- **Kickoff presets**: `ats register <name> <path> --kickoff "..."` — every
  new session in that template gets the prompt typed at it once the agent
  has booted. Spawning with a `kickoff_note_id` sends that note instead.
- **Second monitor**: `ats-tui --group b` runs a single-group, full-height
  client against the same daemon. Any number of clients can attach.
- **Per-template colors**: `[ui.template_colors] api-core = "cyan"` tints
  that template's tabs (calm — inactive tabs only).
- **Scripting hooks**: `ats wait <id> --state finished --timeout 600`
  blocks until a session needs you; `ats events` streams state changes as
  JSON lines (PTY noise filtered) — pipe it into anything.

## Status

All four plan phases complete: daemon-owned PTY sessions, template→workspace
clones, two-group TUI with live vt100 rendering, attach/detach with
scrollback, transcript-based status (`!` needs input with the verbatim
question, `●` finished with a summary line), notes, prompt clipboard,
git status in the rail, LLM orchestrator (digests, ask-across-sessions,
re-entry notes), in-TUI harvest diff viewer, kickoff presets, multi-client
solo mode, per-template theming, CLI automation hooks.
See [docs/PLAN.md](docs/PLAN.md) for the original design.
