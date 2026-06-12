//! Local-socket RPC server: JSON-lines requests in, responses + filtered
//! events out. Each client tracks which sessions it has attached; `PtyOutput`
//! is forwarded only for those (plan §3, "critical detail").

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use ats_core::config::Config;
use ats_core::rpc::{Event, Request, Response, RpcRequest, RpcResponse, ServerMessage};
use ats_core::state::SessionState;
use interprocess::local_socket::{
    tokio::{prelude::*, Listener, Stream},
    GenericFilePath, ListenerOptions,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, Mutex};

use crate::clone::{git_status, CloneManager};
use crate::session::SessionManager;
use crate::store::Store;

pub struct Daemon {
    pub store: Arc<Store>,
    pub sessions: SessionManager,
    pub clones: CloneManager,
    pub data_dir: PathBuf,
    pub orchestrator: crate::orchestrator::Orchestrator,
    /// interactive-orchestrator conversation (crate::agent); the lock also
    /// serializes chat turns
    pub orchestrator_history: Mutex<Vec<serde_json::Value>>,
    pub events: broadcast::Sender<Event>,
    pub config: Config,
}

impl Daemon {
    pub fn new(config: Config, store: Arc<Store>, data_dir: PathBuf) -> Self {
        let (events, _) = broadcast::channel(4096);
        let sessions = SessionManager::new(events.clone(), config.daemon.scrollback_lines);
        let clones = CloneManager::new(
            store.clone(),
            events.clone(),
            PathBuf::from(&config.daemon.workspaces_root),
            data_dir.clone(),
        );
        let orchestrator = crate::orchestrator::Orchestrator::new(&config.orchestrator);
        Self {
            store,
            sessions,
            clones,
            data_dir,
            orchestrator,
            orchestrator_history: Mutex::new(Vec::new()),
            events,
            config,
        }
    }

    /// Spawn workspace + session from a template; the full `Alt+s` flow.
    /// Kickoff precedence: explicit override (orchestrator) > note body >
    /// template preset — written to the agent's stdin once it has booted.
    pub async fn spawn_session_with_kickoff(
        self: &Arc<Self>,
        template_id: i64,
        tab_slot: Option<u8>,
        kickoff_note_id: Option<i64>,
        kickoff_override: Option<String>,
    ) -> Result<Response> {
        let template = self
            .store
            .get_template(template_id)?
            .ok_or_else(|| anyhow!("no template {template_id}"))?;
        let ws = self.clones.spawn_workspace(template_id).await?;
        let max_slots = self.config.ui.group_a_slots + self.config.ui.group_b_slots;
        let slot = match tab_slot {
            Some(s) => Some(s),
            None => self.store.next_free_tab_slot(max_slots)?,
        };
        let session_id = self.store.insert_session(ws.id, slot, None, kickoff_note_id)?;
        let pid = self.sessions.spawn(
            session_id,
            &self.config.daemon.session_cmd,
            &ws.path,
            self.store.clone(),
        )?;
        if let Some(pid) = pid {
            let _ = self.store.set_session_pid(session_id, pid);
            tracing::info!(session_id, pid, path = %ws.path, "session spawned");
        }
        self.store
            .update_workspace(ws.id, None, None, ats_core::state::WorkspaceStatus::Attached)?;

        let kickoff = match (kickoff_override, kickoff_note_id) {
            (Some(text), _) => Some((None, text)),
            (None, Some(note_id)) => {
                self.store.get_note(note_id)?.map(|n| (Some(note_id), n.body))
            }
            (None, None) => template.kickoff_prompt.map(|p| (None, p)),
        };
        if let Some((note_id, text)) = kickoff {
            self.schedule_kickoff(session_id, note_id, text);
        }

        let session = self
            .store
            .get_session(session_id)?
            .ok_or_else(|| anyhow!("session vanished"))?;
        Ok(Response::Session { session })
    }

    /// Type `text` into the session once the agent CLI has had a moment to
    /// boot; mark the note claimed if one was the source.
    fn schedule_kickoff(self: &Arc<Self>, session_id: i64, note_id: Option<i64>, text: String) {
        let daemon = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            let mut bytes = text.into_bytes();
            bytes.push(b'\r');
            match daemon.sessions.write_stdin(session_id, &bytes) {
                Ok(()) => {
                    if let Some(note_id) = note_id {
                        let _ = daemon.store.set_note_state(note_id, "claimed", Some(session_id));
                    }
                }
                Err(e) => tracing::warn!(session_id, "kickoff send failed: {e:#}"),
            }
        });
    }

    /// Bare agent session in `cwd` (default: `<data_dir>/scratch`) — no
    /// workspace clone. Backed by a synthetic "scratch" template row so the
    /// schema's session→workspace→template chain holds; destroy stays
    /// guarded because scratch cwds live outside the workspaces root.
    pub async fn spawn_scratch_session(
        self: &Arc<Self>,
        cwd: Option<String>,
        tab_slot: Option<u8>,
        kickoff: Option<String>,
    ) -> Result<Response> {
        let cwd = match cwd {
            Some(c) => PathBuf::from(c),
            None => self.data_dir.join("scratch"),
        };
        std::fs::create_dir_all(&cwd)
            .with_context(|| format!("creating scratch cwd {}", cwd.display()))?;
        let cwd_str = cwd.to_string_lossy().into_owned();

        let template_id = match self.store.list_templates()?.iter().find(|t| t.name == "scratch") {
            Some(t) => t.id,
            None => {
                self.store
                    .insert_template(
                        "scratch",
                        &self.data_dir.join("scratch").to_string_lossy(),
                        None,
                        None,
                        None,
                    )?
                    .id
            }
        };
        let ws_id = self.store.insert_workspace(
            template_id,
            &cwd_str,
            ats_core::state::WorkspaceStatus::Attached,
        )?;

        let max_slots = self.config.ui.group_a_slots + self.config.ui.group_b_slots;
        let slot = match tab_slot {
            Some(s) => Some(s),
            None => self.store.next_free_tab_slot(max_slots)?,
        };
        let session_id = self.store.insert_session(ws_id, slot, None, None)?;
        let pid = self.sessions.spawn(
            session_id,
            &self.config.daemon.session_cmd,
            &cwd_str,
            self.store.clone(),
        )?;
        if let Some(pid) = pid {
            let _ = self.store.set_session_pid(session_id, pid);
            tracing::info!(session_id, pid, path = %cwd_str, "scratch session spawned");
        }
        if let Some(text) = kickoff {
            self.schedule_kickoff(session_id, None, text);
        }
        let session = self
            .store
            .get_session(session_id)?
            .ok_or_else(|| anyhow!("session vanished"))?;
        Ok(Response::Session { session })
    }

    async fn dispatch(self: &Arc<Self>, req: Request, attached: &Mutex<HashSet<i64>>) -> Result<Response> {
        match req {
            Request::ListSessions => Ok(Response::Sessions { sessions: self.store.list_sessions()? }),
            Request::SpawnSession { template_id, tab_slot, kickoff_note_id } => {
                self.spawn_session_with_kickoff(template_id, tab_slot, kickoff_note_id, None)
                    .await
            }
            Request::SpawnScratchSession { cwd, tab_slot, kickoff } => {
                self.spawn_scratch_session(cwd, tab_slot, kickoff).await
            }
            Request::AttachSession { session_id } => {
                let data = self.sessions.attach(session_id)?;
                attached.lock().await.insert(session_id);
                Ok(Response::Scrollback { session_id, data })
            }
            Request::DetachSession { session_id } => {
                if attached.lock().await.remove(&session_id) {
                    self.sessions.detach(session_id);
                }
                Ok(Response::Ok)
            }
            Request::WriteStdin { session_id, bytes } => {
                self.sessions.write_stdin(session_id, &bytes)?;
                Ok(Response::Ok)
            }
            Request::ResizeSession { session_id, cols, rows } => {
                self.sessions.resize(session_id, cols, rows)?;
                Ok(Response::Ok)
            }
            Request::KillSession { session_id } => {
                self.sessions.kill(session_id)?;
                Ok(Response::Ok)
            }
            Request::GetScrollback { session_id } => Ok(Response::Scrollback {
                session_id,
                data: self.sessions.scrollback(session_id)?,
            }),
            Request::ListTemplates => Ok(Response::Templates { templates: self.store.list_templates()? }),
            Request::RegisterTemplate { name, path, setup_cmd, kickoff_prompt } => {
                let template = self
                    .clones
                    .register_template(
                        &name,
                        &path,
                        setup_cmd.as_deref(),
                        kickoff_prompt.as_deref(),
                    )
                    .await?;
                Ok(Response::Template { template })
            }
            Request::ListWorkspaces => {
                let mut workspaces = self.store.list_workspaces()?;
                for w in &mut workspaces {
                    if let Some(git) = git_status(&w.path).await {
                        w.dirty = Some(git.dirty);
                        w.ahead = git.ahead;
                        w.behind = git.behind;
                    }
                }
                Ok(Response::Workspaces { workspaces })
            }
            Request::SpawnWorkspace { template_id } => {
                let workspace = self.clones.spawn_workspace(template_id).await?;
                Ok(Response::Workspace { workspace })
            }
            Request::ResetWorkspace { id } => {
                self.clones.reset_workspace(id).await?;
                Ok(Response::Ok)
            }
            Request::HarvestWorkspace { id } => {
                let (diff_stat, patch_path) = self.clones.harvest_workspace(id).await?;
                Ok(Response::Harvest {
                    workspace_id: id,
                    diff_stat,
                    patch_path: patch_path.to_string_lossy().into_owned(),
                })
            }
            Request::DestroyWorkspace { id } => {
                // kill any live session in this workspace first
                for s in self.store.list_sessions()? {
                    if s.workspace_id == id && self.sessions.is_live(s.id) {
                        let _ = self.sessions.kill(s.id);
                    }
                }
                self.clones.destroy_workspace(id).await?;
                Ok(Response::Ok)
            }
            Request::ListNotes => Ok(Response::Notes { notes: self.store.list_notes()? }),
            Request::UpsertNote { id, title, body } => {
                let note = self.store.upsert_note(id, &title, &body)?;
                Ok(Response::Note { note })
            }
            Request::FinalizeNote { id } => {
                self.store.set_note_state(id, "finalized", None)?;
                Ok(Response::Ok)
            }
            Request::SendNoteToSession { note_id, session_id } => {
                let note = self
                    .store
                    .get_note(note_id)?
                    .ok_or_else(|| anyhow!("no note {note_id}"))?;
                let mut payload = note.body.into_bytes();
                payload.push(b'\r');
                self.sessions.write_stdin(session_id, &payload)?;
                self.store.set_note_state(note_id, "claimed", Some(session_id))?;
                Ok(Response::Ok)
            }
            Request::ListPrompts => Ok(Response::Prompts { prompts: self.store.list_prompts()? }),
            Request::UpsertPrompt { id, label, body, kind } => {
                self.store.upsert_prompt(id, &label, &body, &kind)?;
                Ok(Response::Prompts { prompts: self.store.list_prompts()? })
            }
            Request::UsePrompt { id, session_id } => {
                let prompt = self
                    .store
                    .get_prompt(id)?
                    .ok_or_else(|| anyhow!("no prompt {id}"))?;
                self.sessions.write_stdin(session_id, prompt.body.as_bytes())?;
                self.store.bump_prompt(id)?;
                Ok(Response::Ok)
            }
            Request::ListReviewQueue => {
                let sessions = self
                    .store
                    .list_sessions()?
                    .into_iter()
                    .filter(|s| {
                        matches!(s.state, SessionState::Finished | SessionState::NeedsInput | SessionState::Error)
                    })
                    .collect();
                Ok(Response::Sessions { sessions })
            }
            Request::SummarizeSession { session_id, force_llm } => {
                let (summary, _source) = self
                    .orchestrator
                    .digest(&self.store, session_id, force_llm)
                    .await?;
                let _ = self.events.send(Event::DigestReady {
                    session_id,
                    summary: summary.clone(),
                });
                Ok(Response::Digest { session_id, summary })
            }
            Request::AskOrchestrator { question, session_ids } => {
                let ids = if session_ids.is_empty() {
                    self.store.list_sessions()?.iter().map(|s| s.id).collect()
                } else {
                    session_ids
                };
                let text = self.orchestrator.ask(&self.store, &question, &ids).await?;
                Ok(Response::Answer { text })
            }
            Request::DraftReentry { session_id } => {
                let note = self.orchestrator.draft_reentry(&self.store, session_id).await?;
                Ok(Response::Note { note })
            }
            Request::OrchestratorChat { message } => {
                let text = crate::agent::chat(self, message).await?;
                Ok(Response::Answer { text })
            }
            Request::OrchestratorReset => {
                self.orchestrator_history.lock().await.clear();
                Ok(Response::Ok)
            }
        }
    }
}

/// Find (and cache) the session's transcript, then classify its tail.
async fn classify_session(
    daemon: &Daemon,
    claude_home: &std::path::Path,
    session_id: i64,
) -> crate::transcript::Classification {
    use crate::transcript;
    let idle = || transcript::Classification { state: SessionState::Idle, detail: None };

    let path = match daemon.store.session_transcript(session_id) {
        Ok(Some(p)) => Some(std::path::PathBuf::from(p)),
        Ok(None) => {
            let Ok(Some(info)) = daemon.store.get_session(session_id) else {
                return idle();
            };
            let found = transcript::discover_transcript(
                claude_home,
                &info.workspace_path,
                info.created_at,
            );
            if let Some(p) = &found {
                let _ = daemon
                    .store
                    .set_session_transcript(session_id, &p.to_string_lossy());
            }
            found
        }
        Err(_) => None,
    };
    match path {
        Some(p) => tokio::task::spawn_blocking(move || transcript::classify_file(&p))
            .await
            .unwrap_or_else(|_| idle()),
        None => idle(),
    }
}

async fn handle_client(daemon: Arc<Daemon>, conn: Stream) {
    let (recv, mut send) = conn.split();
    let mut lines = BufReader::new(recv).lines();
    let mut events = daemon.events.subscribe();
    let attached: Mutex<HashSet<i64>> = Mutex::new(HashSet::new());

    loop {
        tokio::select! {
            line = lines.next_line() => {
                let line = match line {
                    Ok(Some(l)) if !l.trim().is_empty() => l,
                    Ok(Some(_)) => continue,
                    Ok(None) | Err(_) => break, // client gone
                };
                let msg = match serde_json::from_str::<RpcRequest>(&line) {
                    Ok(req) => {
                        let id = req.id;
                        match daemon.dispatch(req.request, &attached).await {
                            Ok(resp) => RpcResponse::ok(id, resp),
                            Err(e) => RpcResponse::err(id, format!("{e:#}")),
                        }
                    }
                    Err(e) => RpcResponse::err(0, format!("bad request: {e}")),
                };
                if write_msg(&mut send, &ServerMessage::Response(msg)).await.is_err() {
                    break;
                }
            }
            ev = events.recv() => {
                match ev {
                    Ok(ev) => {
                        if let Event::PtyOutput { session_id, .. } = &ev {
                            if !attached.lock().await.contains(session_id) {
                                continue;
                            }
                        }
                        if write_msg(&mut send, &ServerMessage::Event(ev)).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "client lagged behind event bus");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    // connection closed: release this client's attachments
    for sid in attached.lock().await.drain() {
        daemon.sessions.detach(sid);
    }
}

async fn write_msg(
    send: &mut (impl tokio::io::AsyncWrite + Unpin),
    msg: &ServerMessage,
) -> Result<()> {
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    send.write_all(&line).await?;
    Ok(())
}

pub async fn serve(daemon: Arc<Daemon>, socket_path: &str) -> Result<()> {
    #[cfg(unix)]
    if std::path::Path::new(socket_path).exists() {
        std::fs::remove_file(socket_path)
            .with_context(|| format!("removing stale socket {socket_path}"))?;
    }
    let name = socket_path
        .to_fs_name::<GenericFilePath>()
        .context("building socket name")?;
    let listener: Listener = ListenerOptions::new()
        .name(name)
        .create_tokio()
        .with_context(|| format!("binding local socket {socket_path}"))?;
    tracing::info!(socket = socket_path, "ats-daemon listening");

    // heartbeat sweep (plan §4.1) + transcript classification (plan §4.2):
    // quiet sessions get classified from their Claude Code JSONL tail;
    // no transcript → plain idle (graceful degradation).
    {
        let daemon = daemon.clone();
        let period = daemon.config.daemon.idle_threshold_secs.max(1);
        tokio::spawn(async move {
            let claude_home = crate::transcript::default_claude_home();
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(period.min(2)));
            loop {
                tick.tick().await;
                let threshold = daemon.config.daemon.idle_threshold_secs as i64;
                for id in daemon.sessions.quiet_working(threshold) {
                    let class = classify_session(&daemon, &claude_home, id).await;
                    let finished = class.state == SessionState::Finished;
                    let long_report = class
                        .detail
                        .as_deref()
                        .map(|d| d.len() >= 100)
                        .unwrap_or(false);
                    daemon.sessions.set_state(id, class.state, class.detail, &daemon.store);
                    // opt-in auto-digest on long finished reports (plan §4.3)
                    if finished && long_report && daemon.config.orchestrator.auto_digest {
                        let daemon = daemon.clone();
                        tokio::spawn(async move {
                            match daemon.orchestrator.digest(&daemon.store, id, false).await {
                                Ok((summary, _)) => {
                                    let _ = daemon
                                        .events
                                        .send(Event::DigestReady { session_id: id, summary });
                                }
                                Err(e) => tracing::warn!(session = id, "auto-digest: {e:#}"),
                            }
                        });
                    }
                }
            }
        });
    }

    loop {
        let conn = listener.accept().await.context("accept")?;
        let daemon = daemon.clone();
        tokio::spawn(async move { handle_client(daemon, conn).await });
    }
}
