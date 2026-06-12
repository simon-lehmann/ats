//! The ATS tool surface: declarations (`tools`) and execution (`execute_tool`)
//! for the daemon's own capabilities — register templates, spawn sessions,
//! instruct/broadcast, read progress, harvest, manage notes & prompts.
//!
//! These are served to the orchestrator (and any Claude Code session) over the
//! MCP server (`crate::mcp`); `guardrail_block` gates destructive tools.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::server::Daemon;
use crate::transcript;

/// Tool results are truncated to keep the context bounded.
const RESULT_MAX: usize = 4000;

pub(crate) fn tools() -> Value {
    json!([
        {
            "name": "list_templates",
            "description": "List registered template repos (id, name, path, kickoff prompt).",
            "input_schema": {"type": "object", "properties": {}}
        },
        {
            "name": "register_template",
            "description": "Register a local git repo as a template for spawning workspaces.",
            "input_schema": {"type": "object", "properties": {
                "name": {"type": "string"},
                "path": {"type": "string", "description": "absolute path to a local git repo"},
                "kickoff_prompt": {"type": "string", "description": "optional prompt typed at every new session in this template"}
            }, "required": ["name", "path"]}
        },
        {
            "name": "list_sessions",
            "description": "List all sessions: id, tab slot, title, state (working/idle/finished/needs_input/dead), state detail (question or summary), workspace path.",
            "input_schema": {"type": "object", "properties": {}}
        },
        {
            "name": "spawn_session",
            "description": "Clone a workspace from a template and start an agent session in it. Returns the new session id and tab slot.",
            "input_schema": {"type": "object", "properties": {
                "template": {"type": "string", "description": "template name or id"},
                "instruction": {"type": "string", "description": "optional kickoff instruction typed at the agent once it has booted (overrides the template preset)"}
            }, "required": ["template"]}
        },
        {
            "name": "spawn_planning_session",
            "description": "Start a bare agent session WITHOUT cloning a workspace — for planning, triage, or research in parallel to project sessions. Runs in the daemon's scratch directory unless cwd is given (an existing path, e.g. a workspace to inspect). Takes a normal tab.",
            "input_schema": {"type": "object", "properties": {
                "cwd": {"type": "string", "description": "optional working directory; default: scratch dir"},
                "instruction": {"type": "string", "description": "optional kickoff typed at the agent once booted"}
            }}
        },
        {
            "name": "send_to_session",
            "description": "Type a message into a session's terminal (an Enter keypress is appended). Use for instructing the agent or answering its question.",
            "input_schema": {"type": "object", "properties": {
                "session_id": {"type": "integer"},
                "text": {"type": "string"}
            }, "required": ["session_id", "text"]}
        },
        {
            "name": "broadcast",
            "description": "Send the same message to every live session (optionally only those in given states).",
            "input_schema": {"type": "object", "properties": {
                "text": {"type": "string"},
                "states": {"type": "array", "items": {"type": "string"}, "description": "optional filter, e.g. [\"working\",\"idle\"]"}
            }, "required": ["text"]}
        },
        {
            "name": "read_session",
            "description": "Read a session's recent activity: transcript dialogue if available, else raw terminal scrollback tail.",
            "input_schema": {"type": "object", "properties": {
                "session_id": {"type": "integer"}
            }, "required": ["session_id"]}
        },
        {
            "name": "harvest",
            "description": "Diff a workspace against its spawn-time base; returns the diffstat and the patch file path.",
            "input_schema": {"type": "object", "properties": {
                "workspace_id": {"type": "integer"}
            }, "required": ["workspace_id"]}
        },
        {
            "name": "kill_session",
            "description": "Terminate a session's agent process. The workspace and its changes remain. DESTRUCTIVE: only when the developer asked for it; requires confirm=true.",
            "input_schema": {"type": "object", "properties": {
                "session_id": {"type": "integer"},
                "confirm": {"type": "boolean", "description": "must be true; set only after the developer explicitly asked to kill this session"}
            }, "required": ["session_id"]}
        },
        {
            "name": "list_notes",
            "description": "List the notes backlog: id, state (draft/finalized/claimed/done), title, claimed_by session, body.",
            "input_schema": {"type": "object", "properties": {}}
        },
        {
            "name": "add_note",
            "description": "Create or update a note (the task backlog). Body must be a complete, self-contained brief an agent can act on. Pass id to update an existing note.",
            "input_schema": {"type": "object", "properties": {
                "id": {"type": "integer", "description": "omit to create"},
                "title": {"type": "string"},
                "body": {"type": "string"}
            }, "required": ["title", "body"]}
        },
        {
            "name": "finalize_note",
            "description": "Mark a draft note as finalized (ready to assign to a session).",
            "input_schema": {"type": "object", "properties": {
                "note_id": {"type": "integer"}
            }, "required": ["note_id"]}
        },
        {
            "name": "send_note",
            "description": "Assign a note to a session: types the note body into the agent's terminal and marks the note claimed by that session.",
            "input_schema": {"type": "object", "properties": {
                "note_id": {"type": "integer"},
                "session_id": {"type": "integer"}
            }, "required": ["note_id", "session_id"]}
        },
        {
            "name": "list_prompts",
            "description": "List the reusable prompt clipboard (id, label, use count, body).",
            "input_schema": {"type": "object", "properties": {}}
        },
        {
            "name": "save_prompt",
            "description": "Save a reusable prompt to the clipboard for the developer's palette.",
            "input_schema": {"type": "object", "properties": {
                "label": {"type": "string"},
                "body": {"type": "string"}
            }, "required": ["label", "body"]}
        },
        {
            "name": "digest_session",
            "description": "One-line digest of a session's final report (state, blockers, what it needs).",
            "input_schema": {"type": "object", "properties": {
                "session_id": {"type": "integer"}
            }, "required": ["session_id"]}
        },
        {
            "name": "reset_workspace",
            "description": "Discard ALL changes in a workspace (git reset --hard + clean). DESTRUCTIVE: only when the developer asked for it; requires confirm=true.",
            "input_schema": {"type": "object", "properties": {
                "workspace_id": {"type": "integer"},
                "confirm": {"type": "boolean", "description": "must be true; set only after the developer explicitly asked to reset this workspace"}
            }, "required": ["workspace_id"]}
        },
        {
            "name": "destroy_workspace",
            "description": "Kill the workspace's sessions and delete its directory. DESTRUCTIVE and irreversible: only when the developer asked for it; requires confirm=true.",
            "input_schema": {"type": "object", "properties": {
                "workspace_id": {"type": "integer"},
                "confirm": {"type": "boolean", "description": "must be true; set only after the developer explicitly asked to destroy this workspace"}
            }, "required": ["workspace_id"]}
        }
    ])
}

/// Tools that mutate fleet state irreversibly.
pub(crate) fn is_destructive(name: &str) -> bool {
    matches!(name, "kill_session" | "reset_workspace" | "destroy_workspace")
}

/// Guardrail for destructive tools (option 1). Returns `Some(error)` when the
/// call must be blocked, `None` when it may proceed: destructive tools require
/// an explicit `confirm: true`, and may never target the orchestrator's own
/// session/workspace (so an agent can't kill the thing coordinating it).
pub(crate) fn guardrail_block(daemon: &Arc<Daemon>, name: &str, input: &Value) -> Option<String> {
    if !is_destructive(name) {
        return None;
    }
    if input.get("confirm").and_then(Value::as_bool) != Some(true) {
        return Some(format!(
            "'{name}' is destructive and was not confirmed. Confirm with the developer \
             first, then call again with \"confirm\": true."
        ));
    }
    if let Ok(Some(orch)) = daemon.store.orchestrator_session() {
        let targets_orch = match name {
            "kill_session" => input.get("session_id").and_then(Value::as_i64) == Some(orch.id),
            "reset_workspace" | "destroy_workspace" => {
                input.get("workspace_id").and_then(Value::as_i64) == Some(orch.workspace_id)
            }
            _ => false,
        };
        if targets_orch {
            return Some(format!(
                "'{name}' refused: that targets the orchestrator's own session/workspace."
            ));
        }
    }
    None
}

fn truncate(s: String) -> String {
    if s.chars().count() <= RESULT_MAX {
        return s;
    }
    let cut: String = s.chars().take(RESULT_MAX).collect();
    format!("{cut}\n[truncated]")
}

pub(crate) async fn execute_tool(daemon: &Arc<Daemon>, name: &str, input: &Value) -> Result<String> {
    let int = |k: &str| -> Result<i64> {
        input
            .get(k)
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow!("missing integer '{k}'"))
    };
    let text = |k: &str| -> Result<String> {
        input
            .get(k)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("missing string '{k}'"))
    };

    match name {
        "list_templates" => {
            let ts = daemon.store.list_templates()?;
            if ts.is_empty() {
                return Ok("no templates registered".into());
            }
            Ok(ts
                .iter()
                .map(|t| {
                    format!(
                        "id={} name={} path={} kickoff={:?}",
                        t.id, t.name, t.path, t.kickoff_prompt
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "register_template" => {
            let t = daemon
                .clones
                .register_template(
                    &text("name")?,
                    &text("path")?,
                    None,
                    input.get("kickoff_prompt").and_then(Value::as_str),
                )
                .await?;
            Ok(format!("registered template id={} name={}", t.id, t.name))
        }
        "list_sessions" => {
            let ss = daemon.store.list_sessions()?;
            if ss.is_empty() {
                return Ok("no sessions".into());
            }
            Ok(ss
                .iter()
                .map(|s| {
                    format!(
                        "id={} slot={:?} title={} state={:?} detail={:?} workspace_id={} path={}",
                        s.id, s.tab_slot, s.title, s.state, s.state_detail, s.workspace_id,
                        s.workspace_path
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "spawn_session" => {
            let tpl = text("template")?;
            let template_id = match tpl.parse::<i64>() {
                Ok(id) => id,
                Err(_) => daemon
                    .store
                    .list_templates()?
                    .iter()
                    .find(|t| t.name == tpl)
                    .map(|t| t.id)
                    .ok_or_else(|| anyhow!("no template named '{tpl}'"))?,
            };
            let resp = daemon
                .spawn_session_with_kickoff(
                    template_id,
                    None,
                    None,
                    input.get("instruction").and_then(Value::as_str).map(str::to_owned),
                )
                .await?;
            if let ats_core::rpc::Response::Session { session } = resp {
                Ok(format!(
                    "spawned session id={} slot={:?} workspace={}",
                    session.id, session.tab_slot, session.workspace_path
                ))
            } else {
                Ok("spawned".into())
            }
        }
        "spawn_planning_session" => {
            let resp = daemon
                .spawn_scratch_session(
                    input.get("cwd").and_then(Value::as_str).map(str::to_owned),
                    None,
                    input.get("instruction").and_then(Value::as_str).map(str::to_owned),
                )
                .await?;
            if let ats_core::rpc::Response::Session { session } = resp {
                Ok(format!(
                    "planning session id={} slot={:?} cwd={}",
                    session.id, session.tab_slot, session.workspace_path
                ))
            } else {
                Ok("spawned".into())
            }
        }
        "send_to_session" => {
            let id = int("session_id")?;
            let mut bytes = text("text")?.into_bytes();
            bytes.push(b'\r');
            daemon.sessions.write_stdin(id, &bytes)?;
            Ok(format!("sent to session {id}"))
        }
        "broadcast" => {
            let msg = text("text")?;
            let states: Option<Vec<String>> = input.get("states").and_then(|v| {
                v.as_array()
                    .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
            });
            let mut sent = Vec::new();
            for s in daemon.store.list_sessions()? {
                if !daemon.sessions.is_live(s.id) {
                    continue;
                }
                if let Some(states) = &states {
                    let st = format!("{:?}", s.state).to_lowercase();
                    if !states.iter().any(|w| w.to_lowercase() == st) {
                        continue;
                    }
                }
                let mut bytes = msg.clone().into_bytes();
                bytes.push(b'\r');
                if daemon.sessions.write_stdin(s.id, &bytes).is_ok() {
                    sent.push(s.id.to_string());
                }
            }
            Ok(format!("broadcast to sessions [{}]", sent.join(", ")))
        }
        "read_session" => {
            let id = int("session_id")?;
            if let Ok(Some(p)) = daemon.store.session_transcript(id) {
                let path = std::path::PathBuf::from(p);
                let dialogue =
                    tokio::task::spawn_blocking(move || transcript::recent_dialogue(&path, RESULT_MAX))
                        .await?;
                if !dialogue.is_empty() {
                    return Ok(truncate(dialogue));
                }
            }
            let sb = daemon.sessions.scrollback(id)?;
            let text = String::from_utf8_lossy(&sb);
            let tail: String = text
                .chars()
                .skip(text.chars().count().saturating_sub(RESULT_MAX))
                .collect();
            Ok(format!("[terminal scrollback]\n{tail}"))
        }
        "harvest" => {
            let id = int("workspace_id")?;
            let (stat, path) = daemon.clones.harvest_workspace(id).await?;
            Ok(format!("{stat}\npatch: {}", path.display()))
        }
        "kill_session" => {
            let id = int("session_id")?;
            daemon.sessions.kill(id)?;
            Ok(format!("killed session {id}"))
        }
        "list_notes" => {
            let notes = daemon.store.list_notes()?;
            if notes.is_empty() {
                return Ok("no notes".into());
            }
            Ok(notes
                .iter()
                .map(|n| {
                    format!(
                        "id={} state={} claimed_by={:?} title={}\n  {}",
                        n.id,
                        n.state,
                        n.claimed_by_session,
                        n.title,
                        n.body.replace('\n', "\n  ")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "add_note" => {
            let note = daemon.store.upsert_note(
                input.get("id").and_then(Value::as_i64),
                &text("title")?,
                &text("body")?,
            )?;
            Ok(format!("note id={} state={} title={}", note.id, note.state, note.title))
        }
        "finalize_note" => {
            let id = int("note_id")?;
            daemon.store.set_note_state(id, "finalized", None)?;
            Ok(format!("note {id} finalized"))
        }
        "send_note" => {
            let note_id = int("note_id")?;
            let session_id = int("session_id")?;
            let note = daemon
                .store
                .get_note(note_id)?
                .ok_or_else(|| anyhow!("no note {note_id}"))?;
            let mut payload = note.body.into_bytes();
            payload.push(b'\r');
            daemon.sessions.write_stdin(session_id, &payload)?;
            daemon.store.set_note_state(note_id, "claimed", Some(session_id))?;
            Ok(format!("note {note_id} sent to session {session_id} and claimed"))
        }
        "list_prompts" => {
            let prompts = daemon.store.list_prompts()?;
            if prompts.is_empty() {
                return Ok("no prompts saved".into());
            }
            Ok(prompts
                .iter()
                .map(|p| format!("id={} ({}x) {}: {}", p.id, p.use_count, p.label, p.body))
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "save_prompt" => {
            let p = daemon
                .store
                .upsert_prompt(None, &text("label")?, &text("body")?, "clipboard")?;
            Ok(format!("prompt id={} saved: {}", p.id, p.label))
        }
        "digest_session" => {
            let id = int("session_id")?;
            let (summary, source) = daemon.orchestrator.digest(&daemon.store, id, false).await?;
            Ok(format!("[{source}] {summary}"))
        }
        "reset_workspace" => {
            let id = int("workspace_id")?;
            daemon.clones.reset_workspace(id).await?;
            Ok(format!("workspace {id} reset to clean state"))
        }
        "destroy_workspace" => {
            let id = int("workspace_id")?;
            for s in daemon.store.list_sessions()? {
                if s.workspace_id == id && daemon.sessions.is_live(s.id) {
                    let _ = daemon.sessions.kill(s.id);
                }
            }
            daemon.clones.destroy_workspace(id).await?;
            Ok(format!("workspace {id} destroyed"))
        }
        other => Err(anyhow!("unknown tool '{other}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn daemon() -> Arc<Daemon> {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut config = ats_core::config::Config::default();
        config.daemon.workspaces_root = "/tmp/ats-agent-test-ws".into();
        Arc::new(Daemon::new(config, store, std::path::PathBuf::from("/tmp/ats-agent-test")))
    }

    #[tokio::test]
    async fn note_tools_cover_the_backlog_lifecycle() {
        let d = daemon();
        let out = execute_tool(
            &d,
            "add_note",
            &json!({"title": "parser", "body": "Build the parser per docs/PLAN.md §3."}),
        )
        .await
        .unwrap();
        assert!(out.contains("state=draft"), "{out}");

        let out = execute_tool(&d, "finalize_note", &json!({"note_id": 1})).await.unwrap();
        assert!(out.contains("finalized"));

        // send_note to a live cat session: body lands in the PTY, note claimed
        let t = d.store.insert_template("t", "/tmp", None, None, None).unwrap();
        let ws = d
            .store
            .insert_workspace(t.id, "/tmp", ats_core::state::WorkspaceStatus::Ready)
            .unwrap();
        let sid = d.store.insert_session(ws, None, None, None).unwrap();
        d.sessions.spawn(sid, "cat", "/tmp", d.store.clone()).unwrap();

        let out = execute_tool(&d, "send_note", &json!({"note_id": 1, "session_id": sid}))
            .await
            .unwrap();
        assert!(out.contains("claimed"), "{out}");
        let out = execute_tool(&d, "list_notes", &json!({})).await.unwrap();
        assert!(out.contains("state=claimed"), "{out}");

        // the note body reached the agent's terminal (cat echoes it)
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let sb = d.sessions.scrollback(sid).unwrap();
            if String::from_utf8_lossy(&sb).contains("docs/PLAN.md") {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "note never reached the PTY");
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let _ = d.sessions.kill(sid);
    }

    #[tokio::test]
    async fn prompt_tools_round_trip() {
        let d = daemon();
        let out = execute_tool(
            &d,
            "save_prompt",
            &json!({"label": "status", "body": "Summarize where you are."}),
        )
        .await
        .unwrap();
        assert!(out.contains("saved"));
        let out = execute_tool(&d, "list_prompts", &json!({})).await.unwrap();
        assert!(out.contains("status"), "{out}");
    }

    #[tokio::test]
    async fn unknown_tool_is_an_error_result() {
        let d = daemon();
        assert!(execute_tool(&d, "rm_rf_slash", &json!({})).await.is_err());
    }

    #[tokio::test]
    async fn guardrail_requires_confirm_on_destructive_tools() {
        let d = daemon();
        // read-only tools are never gated
        assert!(guardrail_block(&d, "list_sessions", &json!({})).is_none());
        // destructive without confirm → blocked
        assert!(guardrail_block(&d, "destroy_workspace", &json!({"workspace_id": 1})).is_some());
        // destructive with confirm → guardrail passes (execution may still error)
        assert!(guardrail_block(&d, "destroy_workspace", &json!({"workspace_id": 1, "confirm": true})).is_none());
    }

    #[tokio::test]
    async fn guardrail_protects_the_orchestrator_session() {
        let d = daemon();
        let t = d.store.insert_template("scratch", "/tmp", None, None, None).unwrap();
        let ws = d
            .store
            .insert_workspace(t.id, "/tmp/orch", ats_core::state::WorkspaceStatus::Ready)
            .unwrap();
        let sid = d.store.insert_session(ws, None, None, None).unwrap();
        d.store.mark_orchestrator(sid).unwrap();
        // confirmed, but targeting the orchestrator's own session/workspace → refused
        assert!(guardrail_block(&d, "kill_session", &json!({"session_id": sid, "confirm": true})).is_some());
        assert!(guardrail_block(&d, "destroy_workspace", &json!({"workspace_id": ws, "confirm": true})).is_some());
        // a different workspace is allowed
        assert!(guardrail_block(&d, "destroy_workspace", &json!({"workspace_id": ws + 999, "confirm": true})).is_none());
    }

    #[test]
    fn every_declared_tool_has_a_handler_arm() {
        // keep tools() and execute_tool in sync: names declared to the model
        // must all dispatch (the reverse is caught by the compiler)
        let declared: Vec<String> = tools()
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        const HANDLED: &[&str] = &[
            "list_templates", "register_template", "list_sessions", "spawn_session",
            "spawn_planning_session",
            "send_to_session", "broadcast", "read_session", "harvest", "kill_session",
            "list_notes", "add_note", "finalize_note", "send_note",
            "list_prompts", "save_prompt", "digest_session",
            "reset_workspace", "destroy_workspace",
        ];
        for name in &declared {
            assert!(HANDLED.contains(&name.as_str()), "tool '{name}' declared but unhandled");
        }
        assert_eq!(declared.len(), HANDLED.len());
    }
}
