# ATS — Agent Terminal Suite

**Run a hundred coding agents without losing your mind.**

ATS is a terminal workbench for orchestrating many concurrent [Claude Code](https://claude.com/claude-code)
sessions. A daemon owns the agents; a calm TUI lets you move between them;
an orchestrator (itself an agent) does the busywork of setting up, assigning,
and checking on the fleet.

```
┌──── rail ─────────────┬─────── group A:  1 demo-app ·  2 demo-app ·  3 web !───┐
│ ▾ WORKSPACES          │                                                        │
│   demo-app-1  ~3      │   $ claude                                             │
│   demo-app-2  clean   │   ⏺ Refactoring the session module…                    │
│ ▾ REVIEW QUEUE (2)    │   ⏺ Running cargo test…                                │
│   3 ! keep legacy api?│                                                        │
│   7 ● tests green, PR ├─────── group B:  6 web-app ·  7 docs ●  8 — ──────────┤
│ ▾ NOTES (4)           │                                                        │
│   → split parser work │   (second active session)                              │
└───────────────────────┴────────────────────────────────────────────────────────┘
```

## The idea

Working with one coding agent is a conversation. Working with a hundred is an
attention-management problem: every session wants to interrupt you, every
terminal looks the same, and figuring out *which one actually needs you* means
clicking through all of them.

ATS is built around a few non-negotiable principles:

1. **Pull-based attention.** Nothing animates, flashes, or notifies. A
   session's state is one dim glyph: `·` working, `○` idle, `●` finished,
   `!` needs you. You decide when to context-switch — the review queue
   (`Alt+q`) tells you what's waiting, including the agent's question
   *verbatim*, read from its transcript.
2. **Crash-safe sessions.** A daemon owns every agent process. Kill the UI,
   reboot your terminal, attach from a second window — the agents never
   notice. Scrollback survives.
3. **Cheap workspaces.** Register a template (a blessed, setup-complete
   clone of your repo) once; spawning an agent-ready copy on its own branch
   takes seconds (`cp --reflink` / robocopy, no network).
4. **Keyboard-first.** Every action is ≤2 keystrokes. Your active set sits in
   two tab groups (`Alt+1..0`, `Alt+←/→` to move); the rail, review queue, and
   orchestrator marshal the rest of the fleet — you never watch them all at once.

## The orchestrator

`Alt+o` jumps to the **orchestrator** — itself a Claude Code session that
drives the daemon through ATS's tools (exposed as an MCP server). Instead of
doing setup and coordination by hand, you talk to it:

```
❯ register ~/repos/demo-app as demo-app, spawn a planning session,
  and have it break docs/TODO.md into independent tasks
❯ turn that plan into finalized notes and spawn a session per note
❯ tell every working session to commit and report a one-line status
❯ which sessions are blocked, and on what?
```

It knows the ATS workflow: **notes** are the task backlog (draft → finalized
→ claimed by a session → done), **planning sessions** are bare agents in a
scratch dir for thinking outside any workspace, and **harvest** diffs a
workspace against its spawn-time base into a patch file. Destructive tools
(kill/reset/destroy) need an explicit `confirm` and refuse to touch the
orchestrator itself.

Because it's an ordinary Claude Code session, it uses your existing `claude`
auth — **no `ANTHROPIC_API_KEY` needed**. The tools reach it via a loopback
MCP server registered once at user scope (`ats mcp register`, or done for you
on first launch). On a fresh machine with nothing registered, ATS opens the
orchestrator automatically and it walks you through your first repo — so you
never have to learn `ats register` to get going.

## Quick start

```sh
# one --path per crate (cargo install takes a single --path)
cargo install --path crates/ats-daemon
cargo install --path crates/ats-tui
cargo install --path crates/ats-cli

ats-tui                                  # auto-starts the daemon
# fresh machine: the orchestrator opens and sets up your first repo by chat.
# or do it by hand: Alt+s → pick template → agent in the next free tab. F1 = help.
```

Prefer to register manually? `ats register demo-app ~/repos/demo-app` blesses a
setup-complete local clone as a template. To wire the orchestrator's tools into
Claude Code yourself: `ats mcp register`.

Headless / scripting:

```sh
ats spawn demo-app            # workspace + session without the TUI
ats status                    # all sessions + workspaces at a glance
ats wait 3 --state finished   # block until session 3 reports back
ats events                    # daemon lifecycle as JSON lines
ats harvest 2                 # diffstat + patch file for workspace 2
ats scratch --kickoff "review the open PRs and summarize"
```

## Keybindings

| Key | Action |
|---|---|
| `Alt+1..5` / `Alt+6..0` | jump to tab in group A / B |
| `Alt+←` / `Alt+→` | previous / next tab (cycles occupied tabs) |
| `` Alt+` `` | toggle focus group A ↔ B |
| `Alt+s` | spawn: template → workspace → session (`p` = planning session) |
| `Alt+q` | review queue — what needs you, with the question verbatim |
| `Alt+n` | notes: draft, finalize, send to session |
| `Alt+p` | prompt clipboard (fuzzy, frecency-sorted) |
| `Alt+o` | jump to the orchestrator (a Claude Code session with ATS tools) |
| `Alt+d` | one-line digest of the active session |
| `Alt+h` | harvest active workspace → scrollable diff viewer |
| `Alt+Esc` | raw mode (every key goes to the PTY) |
| `Alt+x` | detach the UI — agents keep running |
| `F1` | help |

All remappable conventions aside, the terminal pane is a *real* terminal:
Claude Code's own UI, colors, and prompts work inside it — and `/exit`-ing the
agent drops you to a shell prompt in that pane, not a dead session. The pane
lives until the shell does.

## Architecture

```
ats-tui ─┐                      ┌─ SessionManager   PTY per agent (portable-pty)
ats-cli ─┼─ JSON-RPC over local ┼─ CloneManager     templates → workspaces (CoW copy)
  ...   ─┘  socket / named pipe └─ TranscriptWatcher tails Claude Code JSONL → status
            (any # of clients)  ├─ MCP server       ATS tools → Claude Code (loopback)
                                ├─ Orchestrator     on-demand digests/ask (Anthropic API)
                ats-daemon ─────┴─ Store            SQLite: sessions, notes, prompts
```

- PTY output streams only for *attached* (visible) sessions; background
  sessions write to a daemon-side ring buffer, so a fleet of hundreds stays cheap.
- Status beyond the heartbeat comes from Claude Code's own transcript
  (`~/.claude/projects/<cwd>/*.jsonl`): a trailing question → `needs_input`
  with the question as the detail line; a final report → `finished` with its
  summary. No transcript → graceful degradation to heartbeat-only.
- Second monitor: `ats-tui --group b` is a single-group client against the
  same daemon.

Workspace layout: `crates/ats-core` (protocol — the contract), `ats-daemon`,
`ats-tui`, `ats-cli`. Design doc: [docs/PLAN.md](docs/PLAN.md).

## Configuration

`./ats.toml` or `~/.ats/ats.toml` — see [ats.example.toml](ats.example.toml).
The essentials:

```toml
[daemon]
workspaces_root = "~/ats/workspaces"
session_cmd = "claude"          # what runs in each session's PTY
idle_threshold_secs = 8

[orchestrator]
model = "claude-haiku-4-5"
auto_digest = false             # calm by default

[ui]
group_a_slots = 5
group_b_slots = 5
[ui.template_colors]            # calm per-template tab tinting
demo-app = "cyan"
```

## Status

Early but complete: all four phases of the original design are implemented
and tested, including cross-platform socket-level end-to-end runs with fake
agents. Built and tested on both Linux and Windows (ConPTY via `portable-pty`,
named pipes via `interprocess`). Scaling the live view past two tab groups to
truly hundreds-on-screen is the next frontier. Expect rough edges; issues welcome.

## License

[MIT](LICENSE)
