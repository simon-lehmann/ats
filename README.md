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
| `Alt+Esc` | raw mode (all keys to the PTY) |
| `Alt+x` | detach UI — agents keep running |
| `F1` | help |

## Orchestrator

Set `ANTHROPIC_API_KEY` to enable digests (`Alt+d` / `ats digest <n>`),
cross-session questions (`Alt+o` / `ats ask "which sessions are blocked?"`),
and re-entry briefings (`ats reentry <n>` → drafted as a note). Short final
reports are digested heuristically without an API call; `auto_digest` on
finish is opt-in in `ats.toml` (calm by default).

## Status

Phases 1–3 complete: daemon-owned PTY sessions, template→workspace clones,
two-group TUI with live vt100 rendering, attach/detach with scrollback,
transcript-based status (`!` needs input with the verbatim question, `●`
finished with a summary line), notes (draft → finalize → send-to-session),
prompt clipboard with fuzzy palette, git status in the rail, headless CLI,
LLM orchestrator (digests, ask-across-sessions, re-entry notes).
Remaining: Phase 4 polish (in-TUI diff viewer, kickoff presets, theming) —
see [docs/PLAN.md §5](docs/PLAN.md).
