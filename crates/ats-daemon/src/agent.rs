//! Interactive orchestrator: a tool-using agent loop over the daemon's own
//! capabilities. The developer talks to it (Alt+o / `ats orch`); it can do
//! setup (register templates), spawn sessions, instruct or broadcast to
//! sessions, read what they're doing, and harvest results.
//!
//! Conversation history persists in the daemon until `OrchestratorReset`.
//! Progress (each tool call) is pushed as `OrchestratorProgress` events so
//! clients can show it live.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use ats_core::rpc::Event;
use serde_json::{json, Value};

use crate::server::Daemon;
use crate::transcript;

/// Hard cap on tool-call rounds per instruction — a runaway-loop backstop.
const MAX_ROUNDS: usize = 12;
/// Tool results are truncated to keep the context bounded.
const RESULT_MAX: usize = 4000;
/// History cap (messages); oldest turns are dropped beyond this.
const HISTORY_MAX: usize = 60;

const SYSTEM: &str = "You are the orchestrator of ATS (Agent Terminal Suite). You manage \
coding-agent sessions (Claude Code CLIs running in PTYs) on the developer's machine. \
Use your tools to carry out the developer's instructions: register template repos, \
spawn sessions, send instructions to sessions, check on progress, harvest results. \
When instructing a session, write the message exactly as it should reach the agent — \
complete, self-contained prompts. Sessions are independent; to run a workflow across \
all of them, instruct each one (or broadcast). Prefer acting over asking; only ask \
the developer when an instruction is genuinely ambiguous. Be brief in your replies: \
state what you did and anything that needs the developer's attention.";

fn tools() -> Value {
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
            "description": "Terminate a session's agent process. The workspace and its changes remain.",
            "input_schema": {"type": "object", "properties": {
                "session_id": {"type": "integer"}
            }, "required": ["session_id"]}
        }
    ])
}

fn truncate(s: String) -> String {
    if s.chars().count() <= RESULT_MAX {
        return s;
    }
    let cut: String = s.chars().take(RESULT_MAX).collect();
    format!("{cut}\n[truncated]")
}

async fn execute_tool(daemon: &Arc<Daemon>, name: &str, input: &Value) -> Result<String> {
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
        other => Err(anyhow!("unknown tool '{other}'")),
    }
}

fn progress(daemon: &Daemon, text: String) {
    let _ = daemon.events.send(Event::OrchestratorProgress { text });
}

/// One developer message → agent loop until the model stops calling tools.
/// Returns the model's final text. History lives in the daemon.
pub async fn chat(daemon: &Arc<Daemon>, message: String) -> Result<String> {
    // one conversation at a time; concurrent requests queue here
    let mut history = daemon.orchestrator_history.lock().await;
    history.push(json!({"role": "user", "content": message}));

    let mut final_text = String::new();
    for round in 0..MAX_ROUNDS {
        let body = daemon
            .orchestrator
            .messages_payload(&history, &tools(), SYSTEM)
            .await;
        let body = match body {
            Ok(b) => b,
            Err(e) => {
                // don't poison the history with a half-finished turn
                history.pop();
                return Err(e);
            }
        };
        let content = body
            .get("content")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let stop = body.get("stop_reason").and_then(Value::as_str).unwrap_or("");

        history.push(json!({"role": "assistant", "content": content}));

        let mut tool_results = Vec::new();
        for block in &content {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(t) = block.get("text").and_then(Value::as_str) {
                        if !t.trim().is_empty() {
                            final_text = t.trim().to_string();
                            progress(daemon, final_text.clone());
                        }
                    }
                }
                Some("tool_use") => {
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("?");
                    let id = block.get("id").and_then(Value::as_str).unwrap_or("");
                    let input = block.get("input").cloned().unwrap_or(json!({}));
                    progress(daemon, format!("→ {name} {input}"));
                    let (result, is_error) = match execute_tool(daemon, name, &input).await {
                        Ok(r) => (truncate(r), false),
                        Err(e) => (format!("error: {e:#}"), true),
                    };
                    progress(daemon, format!("← {}", result.lines().next().unwrap_or("")));
                    tool_results.push(json!({
                        "type": "tool_result",
                        "tool_use_id": id,
                        "content": result,
                        "is_error": is_error,
                    }));
                }
                _ => {}
            }
        }

        if stop != "tool_use" || tool_results.is_empty() {
            break;
        }
        history.push(json!({"role": "user", "content": tool_results}));
        if round == MAX_ROUNDS - 1 {
            final_text.push_str("\n[stopped: tool-call limit reached]");
        }
    }

    // keep the conversation bounded
    let len = history.len();
    if len > HISTORY_MAX {
        history.drain(..len - HISTORY_MAX);
        // history must start with a user message for the API
        while history
            .first()
            .map(|m| m["role"] != "user" || m["content"].is_array())
            .unwrap_or(false)
        {
            history.remove(0);
        }
    }

    if final_text.is_empty() {
        final_text = "(no reply)".into();
    }
    Ok(final_text)
}
