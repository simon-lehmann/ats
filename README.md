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

## Status

Phase 1 (MVP) in progress — see [docs/PLAN.md §5](docs/PLAN.md) for the phase plan.
