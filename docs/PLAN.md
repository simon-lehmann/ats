# Implementation Plan: Agent Terminal Suite ("ATS")

A Rust-based TUI for orchestrating many concurrent Claude Code sessions with calm, pull-based attention UX.

---

## 0. Product Principles (non-negotiable, read first)

1. **Pull-based attention.** Nothing animates, flashes, or notifies by default. The developer initiates every context switch.
2. **Keyboard-first.** Every action reachable in ≤2 keystrokes. No mouse required, ever.
3. **Crash-safe sessions.** Killing the UI never kills an agent. Daemon owns all processes.
4. **Calm status.** A session's state is at most one dim glyph: `·` working, `○` idle, `●` finished, `!` needs input/error.
5. **Cheap workspaces.** Spawning a ready-to-work clone must take seconds, not minutes.

---

## 1. Architecture Overview

```
┌─────────────────────────────────────────────────────┐
│  ats-tui (Ratatui client)                           │
│  left rail │ tab group A (5) │ tab group B (5)      │
└────────────────────┬────────────────────────────────┘
                     │ JSON-RPC over Unix socket /
                     │ named pipe (Windows)
┌────────────────────┴────────────────────────────────┐
│  ats-daemon (Tokio)                                 │
│  ├─ SessionManager: PTY per session (portable-pty)  │
│  ├─ CloneManager: template clones → workspaces      │
│  ├─ TranscriptWatcher: tails Claude Code JSONL      │
│  ├─ Orchestrator: summaries, insights (API calls)   │
│  └─ Store: SQLite (sessions, notes, prompts, state) │
└─────────────────────────────────────────────────────┘
```

**Why daemon/client:** detach/reattach (tmux-style), crash-safe agents, future multi-client (orchestrator CLI, second window), transcripts survive UI restarts.

### Crate layout (Cargo workspace)

```
ats/
├─ crates/
│  ├─ ats-core      # shared types, RPC protocol (serde), config
│  ├─ ats-daemon    # binary: session/clone/transcript/orchestrator
│  ├─ ats-tui       # binary: Ratatui client
│  └─ ats-cli       # binary: headless commands (ats spawn, ats status)
└─ Cargo.toml
```

### Key dependencies

| Concern | Crate |
|---|---|
| TUI | `ratatui` + `crossterm` |
| Async runtime | `tokio` |
| PTY | `portable-pty` (Windows ConPTY + Unix) |
| Terminal emulation/scrollback | `vt100` or `wezterm-term` (parse PTY output into a grid) |
| RPC | `jsonrpsee` or hand-rolled JSON-lines over `interprocess` (cross-platform local sockets) |
| Storage | `rusqlite` (bundled SQLite) |
| File watching | `notify` (transcript JSONL tailing) |
| Fuzzy search | `nucleo` (prompt clipboard, command palette) |
| Config | `serde` + `toml` |
| Anthropic API | plain `reqwest` against `/v1/messages` |

**Platform note:** primary target Windows (user is on PowerShell 7) — ConPTY via `portable-pty`, named pipes via `interprocess`. Keep everything cross-platform anyway; it's nearly free with these crates.

---

## 2. Data Model (SQLite)

```sql
-- Template clones: blessed, setup-complete local repos
CREATE TABLE templates (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  path TEXT NOT NULL,            -- local template clone
  origin_url TEXT,
  setup_cmd TEXT,                -- optional post-clone hook
  created_at INTEGER, updated_at INTEGER
);

-- Workspaces: per-agent clones spawned from a template
CREATE TABLE workspaces (
  id INTEGER PRIMARY KEY,
  template_id INTEGER REFERENCES templates(id),
  path TEXT NOT NULL,
  branch TEXT,
  status TEXT CHECK(status IN ('spawning','ready','attached','harvesting','destroyed')),
  created_at INTEGER
);

-- Sessions: one Claude Code process in one workspace
CREATE TABLE sessions (
  id INTEGER PRIMARY KEY,
  workspace_id INTEGER REFERENCES workspaces(id),
  tab_slot INTEGER,              -- 1..10 (group A: 1-5, group B: 6-10), NULL = unassigned
  pty_pid INTEGER,
  claude_session_id TEXT,        -- maps to JSONL filename
  transcript_path TEXT,
  state TEXT CHECK(state IN ('working','idle','finished','needs_input','error','dead')),
  state_detail TEXT,             -- e.g. extracted question, summary line
  kickoff_note_id INTEGER,
  created_at INTEGER, last_activity_at INTEGER
);

-- Notes / plans
CREATE TABLE notes (
  id INTEGER PRIMARY KEY,
  title TEXT, body TEXT,
  state TEXT CHECK(state IN ('draft','finalized','claimed','done')),
  pinned INTEGER DEFAULT 0,
  claimed_by_session INTEGER REFERENCES sessions(id),
  created_at INTEGER, updated_at INTEGER
);

-- Prompt clipboard
CREATE TABLE prompts (
  id INTEGER PRIMARY KEY,
  label TEXT, body TEXT,
  kind TEXT CHECK(kind IN ('clipboard','reentry')),  -- top-used vs work-mode history
  use_count INTEGER DEFAULT 0,
  last_used_at INTEGER
);

-- Orchestrator digests
CREATE TABLE digests (
  id INTEGER PRIMARY KEY,
  session_id INTEGER REFERENCES sessions(id),
  summary TEXT,
  source TEXT CHECK(source IN ('heuristic','llm')),
  created_at INTEGER
);
```

---

## 3. RPC Protocol (ats-core)

JSON-lines messages over local socket. Requests, responses, and server-push events.

```rust
// Requests (client → daemon)
enum Request {
  // sessions
  SpawnSession { template_id: i64, tab_slot: Option<u8>, kickoff_note_id: Option<i64> },
  AttachSession { session_id: i64 },        // begin streaming PTY output
  DetachSession { session_id: i64 },
  WriteStdin { session_id: i64, bytes: Vec<u8> },
  ResizeSession { session_id: i64, cols: u16, rows: u16 },
  KillSession { session_id: i64 },
  GetScrollback { session_id: i64, lines: u32 },
  // workspaces
  ListTemplates, RegisterTemplate { .. }, SpawnWorkspace { .. },
  ResetWorkspace { id: i64 }, HarvestWorkspace { id: i64 }, DestroyWorkspace { id: i64 },
  // notes & prompts
  ListNotes, UpsertNote { .. }, FinalizeNote { id: i64 },
  SendNoteToSession { note_id: i64, session_id: i64 },
  ListPrompts, UsePrompt { id: i64, session_id: i64 },
  // orchestrator
  SummarizeSession { session_id: i64, force_llm: bool },
  AskOrchestrator { question: String, session_ids: Vec<i64> },
  ListReviewQueue,
}

// Events (daemon → client, pushed)
enum Event {
  PtyOutput { session_id: i64, bytes: Vec<u8> },     // only for attached sessions
  SessionStateChanged { session_id: i64, state: SessionState, detail: Option<String> },
  DigestReady { session_id: i64, summary: String },
  WorkspaceStatusChanged { .. },
}
```

**Critical detail:** PTY output streams *only* for the currently attached (visible) session. Background sessions write to a ring-buffer scrollback in the daemon (e.g. last 10k lines via `vt100` grid + history). This keeps the UI cheap with 10+ agents running.

---

## 4. Component Specs

### 4.1 SessionManager (daemon)

- Spawns `claude` (Claude Code CLI) inside a PTY (`portable-pty`), cwd = workspace path.
- Capture `claude_session_id`: watch `~/.claude/projects/<hash-of-cwd>/` for the newest `*.jsonl` created after spawn; store path. (Hash = Claude Code's project-dir encoding of the cwd; verify the exact scheme at impl time, fall back to "newest file in dir".)
- Maintains per-session `vt100::Parser` fed from PTY for scrollback + state detection.
- Heartbeat: `last_activity_at` updated on any PTY output. State machine:
  - output flowing → `working`
  - no output for N sec (config, default 8s) → candidate `idle/finished/needs_input` → ask TranscriptWatcher to classify
  - process exit → `dead`

### 4.2 TranscriptWatcher (daemon)

- `notify`-based tail of each session's JSONL.
- Parse last lines (each line = JSON message: `{type, message:{role, content[]}, ...}`).
- **Heuristic classifier (zero-cost, instant):**
  - last assistant message ends with `?` or contains a tool-permission request → `needs_input`, `state_detail` = the question verbatim (truncated 120 chars)
  - last message is assistant text, no pending tool call → `finished`
  - else → `idle`
- On `finished` with a long final message (> ~400 chars) → enqueue LLM digest job.

### 4.3 Orchestrator (daemon)

- **Digest job:** call Anthropic API (`claude-haiku` class model), prompt:
  > "Compress this agent's final report to one line, ≤90 chars: state, blockers, what it needs from the developer. No preamble."
- Digests stored in `digests`, pushed as `DigestReady`.
- **Ask mode (phase 3):** `AskOrchestrator` loads last N messages from selected sessions' JSONL, answers questions ("which sessions are blocked?", "what did tab 5 change?"). Tool-use blocks in JSONL reveal file edits — surface paths.
- Config: API key from env (`ANTHROPIC_API_KEY`), model + auto-digest on/off in `ats.toml`. Default **on-demand only** (calm principle); auto-digest on finish is opt-in.

### 4.4 CloneManager (daemon)

- `RegisterTemplate { path }`: validate it's a git repo, store.
- `SpawnWorkspace`:
  1. Copy template → `<workspaces_root>/<template>-<n>/`
     - Windows: try `robocopy /MIR /MT` ; if on ReFS/dev drive, attempt CoW (`CopyFile` with block cloning) — fall back gracefully
     - Unix: `cp --reflink=auto -r`
  2. `git checkout -b agent/<n>` (configurable branch scheme)
  3. Run `setup_cmd` if present (usually unnecessary — that's the whole point of template clones)
  4. Status `ready`
- `ResetWorkspace`: `git reset --hard && git clean -fd` + re-sync from template (configurable: cheap reset vs full re-copy).
- `HarvestWorkspace`: produce `git diff template/main...HEAD` summary; expose patch file; optional `git push` of branch. (Phase 2: open diff viewer in TUI.)
- `DestroyWorkspace`: kill attached session, delete dir, mark destroyed.

### 4.5 TUI (ats-tui)

**Layout** (Ratatui):

```
┌──── rail (24-30 cols) ────┬──────── tab group A: 1 2 3 4 5 ────────┐
│ ▾ WORKSPACES              │                                         │
│   demo-app    ●  +3 ~12   │         (active session terminal,       │
│   web-app     ·  clean    │          vt100 grid render)             │
│ ▾ REVIEW QUEUE (2)        │                                         │
│   3 ● refactor done, 2…   ├──────── tab group B: 6 7 8 9 0 ────────┤
│   7 ! asks: keep legacy…  │                                         │
│ ▾ PROMPTS                 │         (second active session)         │
│ ▾ NOTES                   │                                         │
└───────────────────────────┴─────────────────────────────────────────┘
```

- Two independently-active tab groups (top/bottom), mirroring the user's current dual-pane pwsh setup. Focus moves between rail / group A / group B.
- Tab bar: number + short name + status glyph only. Dim colors. No bold/blink on state change.
- Terminal pane: render `vt100` grid; scrollback with `PgUp/PgDn`; forwarding of raw input when focused (the pane is a real terminal — Claude Code's own UI must work inside it, including its prompts and colors).

**Keybindings (defaults, all remappable in `ats.toml`):**

| Key | Action |
|---|---|
| `Alt+1..5` | jump to tab in group A |
| `Alt+6..0` | jump to tab in group B |
| `Alt+Tab` / `Alt+`` ` | toggle focus group A ↔ B |
| `Alt+r` | focus rail |
| `Alt+q` | open review queue (drain mode: Enter = jump to session, d = dismiss) |
| `Alt+p` | prompt palette (fuzzy, Enter = paste into active session) |
| `Alt+n` | notes panel; `f` finalize, `s` send to session (pick tab) |
| `Alt+s` | spawn: pick template → new workspace + session in next free slot |
| `Alt+d` | request digest for active session |
| `Alt+o` | orchestrator panel (phase 3) |
| `F1` | help overlay |

**Focus/escape rule:** when a terminal pane is focused, all input goes to the PTY except the `Alt+` namespace. Provide a "raw mode" toggle (`Alt+Esc`) that passes *everything* through, for apps that need Alt keys.

### 4.6 ats-cli (headless)

Thin RPC client: `ats spawn <template>`, `ats status`, `ats digest <n>`, `ats harvest <n>`. Useful for scripting and lets agents themselves interact with the daemon later.

---

## 5. Build Phases

### Phase 1 — Core terminal value (MVP)
> Goal: replaces the two pwsh windows. Daily-drivable.

1. Cargo workspace, ats-core types, config loading
2. Daemon skeleton: socket server, SQLite store, RPC plumbing
3. SessionManager: spawn arbitrary command in PTY, vt100 scrollback, attach/detach streaming
4. TUI shell: layout, two tab groups, terminal rendering, keyboard input forwarding, tab switching
5. CloneManager: templates, spawn/destroy workspaces (plain copy first; reflink/robocopy optimization after)
6. Spawn flow end-to-end: `Alt+s` → template → workspace → `claude` running in tab
7. Heartbeat-based status glyphs (working/idle/dead only)

**Exit criteria:** 10 concurrent Claude Code sessions, smooth switching, daemon survives TUI restart, sessions reattach with scrollback intact.

### Phase 2 — Glue (rail features)
1. Notes CRUD + states + pin; finalize → send-to-session (writes note body to PTY stdin + Enter)
2. Prompt clipboard: frecency sort, fuzzy palette, paste-to-active
3. TranscriptWatcher: JSONL discovery + tail + heuristic classifier → full status glyphs (`finished`, `needs_input` with verbatim question)
4. Review queue in rail
5. Workspace status in rail: branch, dirty count, ahead/behind (shell out to `git status --porcelain=v2`)
6. Harvest: diff summary + patch export

**Exit criteria:** re-entry into any session understandable from the rail alone, without opening the tab.

### Phase 3 — Orchestrator
1. Anthropic API client + digest job queue (on-demand `Alt+d` first)
2. Opt-in auto-digest on `finished`
3. Orchestrator panel: ask questions across sessions (reads JSONL histories)
4. "Draft re-entry context" command: orchestrator writes a catch-up note for a chosen session

### Phase 4 — Polish / power
- Diff viewer in TUI for harvest review
- Session templates (kickoff prompt presets per template repo)
- Multi-window / second monitor client
- Theming, per-template colors (still calm)
- `ats-cli` automation hooks

---

## 6. Testing Strategy

- **ats-core:** unit tests for RPC serde round-trips, state machine transitions
- **Daemon:** integration tests spawning real PTYs with a fake "agent" script (emits output, asks a question, exits) — assert classifier states; transcript watcher tested against captured real Claude Code JSONL fixtures
- **CloneManager:** temp-dir git repos as templates; test spawn/reset/harvest
- **TUI:** Ratatui `TestBackend` snapshot tests for layout + rail rendering
- **E2E smoke (manual checklist):** 10 sessions, kill TUI, reattach, drain review queue

---

## 7. Risks & Mitigations

| Risk | Mitigation |
|---|---|
| Claude Code JSONL location/format changes | Isolate in TranscriptWatcher behind a trait; "newest file in project dir" fallback; heuristics degrade gracefully to heartbeat-only status |
| ConPTY quirks on Windows (resize, colors) | `portable-pty` + early manual testing on Win 10/11; pin terminal to Windows Terminal for v1 |
| Input conflicts (Alt keys eaten by terminal emulator) | Remappable bindings + raw-mode toggle from day one |
| Copy time for huge repos | Reflink/dev-drive CoW path; async spawn with `spawning` status so UI never blocks |
| LLM digest cost/latency | Heuristics first; LLM only for long final reports; on-demand by default |
| vt100 crate gaps for fancy TUI apps (Claude Code uses Ink) | Evaluate `wezterm-term` as alternative early in Phase 1, behind a trait |

---

## 8. Config (`ats.toml` sketch)

```toml
[daemon]
workspaces_root = "D:/ats/workspaces"
scrollback_lines = 10000
idle_threshold_secs = 8

[orchestrator]
model = "claude-haiku-4-5"
auto_digest = false           # calm by default

[ui]
rail_width = 28
group_a_slots = 5
group_b_slots = 5

[keys]
spawn = "alt+s"
review_queue = "alt+q"
# ...
```

---

## 9. Suggested Agent Task Split

Independent workstreams (matches the user's multi-agent workflow):

1. **Agent A:** ats-core + RPC + daemon skeleton + SQLite store
2. **Agent B:** SessionManager + PTY + vt100 scrollback (can stub RPC)
3. **Agent C:** TUI layout + terminal rendering + keybindings (against mock daemon)
4. **Agent D:** CloneManager + git integration
5. **Agent E (phase 2):** TranscriptWatcher + classifier (pure, fixture-driven — fully parallel)

Integration points: A defines the protocol first (it's the contract); B/C/D build against it.
