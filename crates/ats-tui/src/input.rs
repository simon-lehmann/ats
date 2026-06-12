//! Key handling. Rule (plan §4.5): with a terminal pane focused, everything
//! goes to the PTY except the Alt+ namespace; raw mode forwards even that,
//! keeping only Alt+Esc to leave raw mode.

use anyhow::Result;
use ats_core::rpc::Request;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::{App, Focus, Modal};

/// Encode a key event as the byte sequence a terminal would send.
pub fn key_to_bytes(key: &KeyEvent) -> Vec<u8> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let mut out: Vec<u8> = Vec::new();
    if alt {
        out.push(0x1b); // ESC prefix for Alt-modified keys
    }
    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                let c = c.to_ascii_lowercase();
                if c.is_ascii_lowercase() {
                    out.push((c as u8) - b'a' + 1);
                } else if c == ' ' {
                    out.push(0);
                }
            } else {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
        KeyCode::Enter => out.push(b'\r'),
        KeyCode::Tab => out.push(b'\t'),
        KeyCode::BackTab => out.extend_from_slice(b"\x1b[Z"),
        KeyCode::Backspace => out.push(0x7f),
        KeyCode::Esc => out.push(0x1b),
        KeyCode::Up => out.extend_from_slice(b"\x1b[A"),
        KeyCode::Down => out.extend_from_slice(b"\x1b[B"),
        KeyCode::Right => out.extend_from_slice(b"\x1b[C"),
        KeyCode::Left => out.extend_from_slice(b"\x1b[D"),
        KeyCode::Home => out.extend_from_slice(b"\x1b[H"),
        KeyCode::End => out.extend_from_slice(b"\x1b[F"),
        KeyCode::PageUp => out.extend_from_slice(b"\x1b[5~"),
        KeyCode::PageDown => out.extend_from_slice(b"\x1b[6~"),
        KeyCode::Delete => out.extend_from_slice(b"\x1b[3~"),
        KeyCode::Insert => out.extend_from_slice(b"\x1b[2~"),
        KeyCode::F(n) => {
            let seq: &[u8] = match n {
                1 => b"\x1bOP",
                2 => b"\x1bOQ",
                3 => b"\x1bOR",
                4 => b"\x1bOS",
                5 => b"\x1b[15~",
                6 => b"\x1b[17~",
                7 => b"\x1b[18~",
                8 => b"\x1b[19~",
                9 => b"\x1b[20~",
                10 => b"\x1b[21~",
                11 => b"\x1b[23~",
                12 => b"\x1b[24~",
                _ => b"",
            };
            out.extend_from_slice(seq);
        }
        _ => {}
    }
    out
}

fn digit_to_slot(c: char) -> Option<u8> {
    match c {
        '1'..='9' => Some(c as u8 - b'0'),
        '0' => Some(10),
        _ => None,
    }
}

pub async fn handle_key(app: &mut App, key: KeyEvent) -> Result<()> {
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    // raw mode: everything through, Alt+Esc leaves
    if app.raw_mode {
        if alt && key.code == KeyCode::Esc {
            app.raw_mode = false;
            return Ok(());
        }
        return forward(app, &key).await;
    }

    // modal handling first
    if app.modal != Modal::None {
        return handle_modal_key(app, key).await;
    }

    if key.code == KeyCode::F(1) {
        app.modal = Modal::Help;
        return Ok(());
    }

    if alt {
        match key.code {
            KeyCode::Char(c) if digit_to_slot(c).is_some() => {
                jump_to_slot(app, digit_to_slot(c).unwrap());
                return Ok(());
            }
            KeyCode::Char('`') => {
                app.focus = match app.focus {
                    Focus::GroupA => Focus::GroupB,
                    _ => Focus::GroupA,
                };
                return Ok(());
            }
            KeyCode::Char('r') => {
                app.focus = Focus::Rail;
                return Ok(());
            }
            KeyCode::Char('s') => {
                app.modal = Modal::Spawn { selected: 0 };
                return Ok(());
            }
            KeyCode::Char('q') => {
                app.modal = Modal::Queue { selected: 0 };
                return Ok(());
            }
            KeyCode::Char('n') => {
                app.modal = Modal::Notes { selected: 0 };
                return Ok(());
            }
            KeyCode::Char('p') => {
                app.modal = Modal::Palette { query: String::new(), selected: 0 };
                return Ok(());
            }
            // harvest the active session's workspace → diff viewer
            KeyCode::Char('h') => {
                if let Some(s) = app
                    .sessions
                    .iter()
                    .find(|s| Some(s.id) == app.active_session_id())
                {
                    let workspace_id = s.workspace_id;
                    let title = s.title.clone();
                    let client = app.client.clone();
                    let tx = app.async_tx.clone();
                    app.status_line = format!("harvesting {title}…");
                    tokio::spawn(async move {
                        let msg = match client
                            .request(Request::HarvestWorkspace { id: workspace_id })
                            .await
                        {
                            Ok(ats_core::rpc::Response::Harvest { patch_path, .. }) => {
                                match tokio::fs::read_to_string(&patch_path).await {
                                    Ok(content) => crate::app::AsyncMsg::Diff {
                                        title: format!("{title} ({patch_path})"),
                                        content,
                                    },
                                    Err(e) => crate::app::AsyncMsg::Status(format!(
                                        "harvest: cannot read {patch_path}: {e}"
                                    )),
                                }
                            }
                            Ok(_) => crate::app::AsyncMsg::Status("harvest: unexpected response".into()),
                            Err(e) => crate::app::AsyncMsg::Status(format!("harvest: {e:#}")),
                        };
                        let _ = tx.send(msg);
                    });
                }
                return Ok(());
            }
            KeyCode::Char('o') => {
                app.modal = Modal::Orchestrator {
                    input: String::new(),
                    log: Vec::new(),
                    busy: false,
                };
                return Ok(());
            }
            // digest of the active session, in the background
            KeyCode::Char('d') => {
                if let Some(session_id) = app.active_session_id() {
                    let client = app.client.clone();
                    let tx = app.async_tx.clone();
                    app.status_line = format!("digesting session {session_id}…");
                    tokio::spawn(async move {
                        let msg = match client
                            .request(Request::SummarizeSession { session_id, force_llm: false })
                            .await
                        {
                            Ok(ats_core::rpc::Response::Digest { summary, .. }) => {
                                format!("digest [{session_id}]: {summary}")
                            }
                            Ok(_) => "digest: unexpected response".into(),
                            Err(e) => format!("digest: {e:#}"),
                        };
                        let _ = tx.send(crate::app::AsyncMsg::Status(msg));
                    });
                }
                return Ok(());
            }
            KeyCode::Char('x') => {
                app.should_quit = true;
                return Ok(());
            }
            KeyCode::Esc => {
                if matches!(app.focus, Focus::GroupA | Focus::GroupB) {
                    app.raw_mode = true;
                }
                return Ok(());
            }
            _ => {
                // unbound Alt combo over a terminal: forward it
                if matches!(app.focus, Focus::GroupA | Focus::GroupB) {
                    return forward(app, &key).await;
                }
                return Ok(());
            }
        }
    }

    match app.focus {
        Focus::Rail => {
            // rail is read-only in phase 1; Esc returns to group A
            if matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
                app.focus = Focus::GroupA;
            }
            Ok(())
        }
        Focus::GroupA | Focus::GroupB => forward(app, &key).await,
    }
}

/// Edit a string field from key input (the minimal editors).
fn edit_text(s: &mut String, key: &KeyEvent) {
    match key.code {
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => s.push(c),
        KeyCode::Backspace => {
            s.pop();
        }
        _ => {}
    }
}

fn is_ctrl(key: &KeyEvent, c: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char(c)
}

async fn handle_modal_key(app: &mut App, key: KeyEvent) -> Result<()> {
    let modal = std::mem::take(&mut app.modal);
    match modal {
        Modal::None => {}
        Modal::Help => {
            if !matches!(key.code, KeyCode::Esc | KeyCode::F(1) | KeyCode::Char('q')) {
                app.modal = Modal::Help;
            }
        }
        Modal::Spawn { selected } => match key.code {
            KeyCode::Esc => {}
            KeyCode::Up => app.modal = Modal::Spawn { selected: selected.saturating_sub(1) },
            KeyCode::Down => {
                let max = app.templates.len().saturating_sub(1);
                app.modal = Modal::Spawn { selected: (selected + 1).min(max) };
            }
            KeyCode::Enter => {
                if let Some(t) = app.templates.get(selected) {
                    let client = app.client.clone();
                    let template_id = t.id;
                    app.status_line = format!("spawning workspace from {}…", t.name);
                    tokio::spawn(async move {
                        let _ = client
                            .request(Request::SpawnSession {
                                template_id,
                                tab_slot: None,
                                kickoff_note_id: None,
                            })
                            .await;
                    });
                }
            }
            _ => app.modal = Modal::Spawn { selected },
        },
        Modal::Queue { selected } => match key.code {
            KeyCode::Esc => {}
            KeyCode::Up => app.modal = Modal::Queue { selected: selected.saturating_sub(1) },
            KeyCode::Down => {
                let max = app.review_queue().len().saturating_sub(1);
                app.modal = Modal::Queue { selected: (selected + 1).min(max) };
            }
            KeyCode::Enter => {
                if let Some(s) = app.review_queue().get(selected) {
                    if let Some(slot) = s.tab_slot {
                        jump_to_slot(app, slot);
                    }
                }
            }
            _ => app.modal = Modal::Queue { selected },
        },
        Modal::Notes { selected } => match key.code {
            KeyCode::Esc => {}
            KeyCode::Up => app.modal = Modal::Notes { selected: selected.saturating_sub(1) },
            KeyCode::Down => {
                let max = app.notes.len().saturating_sub(1);
                app.modal = Modal::Notes { selected: (selected + 1).min(max) };
            }
            KeyCode::Char('n') => {
                app.modal = Modal::NoteEdit {
                    id: None,
                    title: String::new(),
                    body: String::new(),
                    editing_body: false,
                };
            }
            KeyCode::Char('e') => {
                if let Some(n) = app.notes.get(selected) {
                    app.modal = Modal::NoteEdit {
                        id: Some(n.id),
                        title: n.title.clone(),
                        body: n.body.clone(),
                        editing_body: true,
                    };
                } else {
                    app.modal = Modal::Notes { selected };
                }
            }
            KeyCode::Char('f') => {
                if let Some(n) = app.notes.get(selected) {
                    app.client.request(Request::FinalizeNote { id: n.id }).await?;
                    app.refresh().await?;
                }
                app.modal = Modal::Notes { selected };
            }
            // Enter / 's': send note body to the active session
            KeyCode::Enter | KeyCode::Char('s') => {
                if let Some(n) = app.notes.get(selected) {
                    if let Some(session_id) = app.active_session_id() {
                        let note_id = n.id;
                        app.client
                            .request(Request::SendNoteToSession { note_id, session_id })
                            .await?;
                        app.status_line = format!("note {note_id} → session {session_id}");
                        app.refresh().await?;
                    } else {
                        app.status_line = "no active session to send to".into();
                        app.modal = Modal::Notes { selected };
                    }
                }
            }
            _ => app.modal = Modal::Notes { selected },
        },
        Modal::NoteEdit { id, mut title, mut body, editing_body } => {
            if key.code == KeyCode::Esc {
                app.modal = Modal::Notes { selected: 0 };
            } else if is_ctrl(&key, 's') {
                let resp = app
                    .client
                    .request(Request::UpsertNote { id, title, body })
                    .await?;
                if let ats_core::rpc::Response::Note { .. } = resp {
                    app.refresh().await?;
                }
                app.modal = Modal::Notes { selected: 0 };
            } else if key.code == KeyCode::Tab {
                app.modal = Modal::NoteEdit { id, title, body, editing_body: !editing_body };
            } else if key.code == KeyCode::Enter {
                if editing_body {
                    body.push('\n');
                }
                app.modal = Modal::NoteEdit { id, title, body, editing_body: true };
            } else {
                if editing_body {
                    edit_text(&mut body, &key);
                } else {
                    edit_text(&mut title, &key);
                }
                app.modal = Modal::NoteEdit { id, title, body, editing_body };
            }
        }
        Modal::Palette { mut query, selected } => {
            if key.code == KeyCode::Esc {
            } else if is_ctrl(&key, 'n') {
                app.modal = Modal::PromptEdit {
                    label: String::new(),
                    body: String::new(),
                    editing_body: false,
                };
            } else if key.code == KeyCode::Up {
                app.modal = Modal::Palette { query, selected: selected.saturating_sub(1) };
            } else if key.code == KeyCode::Down {
                let max = app.filtered_prompts(&query).len().saturating_sub(1);
                app.modal = Modal::Palette { query, selected: (selected + 1).min(max) };
            } else if key.code == KeyCode::Enter {
                let prompt_id = app.filtered_prompts(&query).get(selected).map(|p| p.id);
                if let (Some(id), Some(session_id)) = (prompt_id, app.active_session_id()) {
                    app.client.request(Request::UsePrompt { id, session_id }).await?;
                    app.status_line = format!("prompt → session {session_id}");
                    app.refresh().await?;
                }
            } else {
                edit_text(&mut query, &key);
                app.modal = Modal::Palette { query, selected: 0 };
            }
        }
        Modal::Diff { title, lines, scroll } => {
            let page = 20usize;
            let max = lines.len().saturating_sub(1);
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {}
                KeyCode::Up => {
                    app.modal = Modal::Diff { title, lines, scroll: scroll.saturating_sub(1) }
                }
                KeyCode::Down => {
                    app.modal = Modal::Diff { title, lines, scroll: (scroll + 1).min(max) }
                }
                KeyCode::PageUp => {
                    app.modal = Modal::Diff { title, lines, scroll: scroll.saturating_sub(page) }
                }
                KeyCode::PageDown => {
                    app.modal = Modal::Diff { title, lines, scroll: (scroll + page).min(max) }
                }
                KeyCode::Home => app.modal = Modal::Diff { title, lines, scroll: 0 },
                KeyCode::End => app.modal = Modal::Diff { title, lines, scroll: max },
                _ => app.modal = Modal::Diff { title, lines, scroll },
            }
        }
        Modal::Orchestrator { mut input, mut log, busy } => {
            if key.code == KeyCode::Esc {
                // close; daemon keeps the conversation — Alt+o resumes it
            } else if is_ctrl(&key, 'r') && !busy {
                app.client.request(Request::OrchestratorReset).await?;
                log.push("— conversation reset —".into());
                app.modal = Modal::Orchestrator { input, log, busy };
            } else if key.code == KeyCode::Enter && !busy && !input.trim().is_empty() {
                let message = std::mem::take(&mut input);
                log.push(format!("you: {message}"));
                let client = app.client.clone();
                let tx = app.async_tx.clone();
                tokio::spawn(async move {
                    let result = match client
                        .request(Request::OrchestratorChat { message })
                        .await
                    {
                        Ok(ats_core::rpc::Response::Answer { text }) => Ok(text),
                        Ok(_) => Err("unexpected response".to_string()),
                        Err(e) => Err(format!("{e:#}")),
                    };
                    let _ = tx.send(crate::app::AsyncMsg::Answer(result));
                });
                app.modal = Modal::Orchestrator { input, log, busy: true };
            } else {
                if !busy {
                    edit_text(&mut input, &key);
                }
                app.modal = Modal::Orchestrator { input, log, busy };
            }
        }
        Modal::PromptEdit { mut label, mut body, editing_body } => {
            if key.code == KeyCode::Esc {
                app.modal = Modal::Palette { query: String::new(), selected: 0 };
            } else if is_ctrl(&key, 's') {
                app.client
                    .request(Request::UpsertPrompt {
                        id: None,
                        label,
                        body,
                        kind: "clipboard".into(),
                    })
                    .await?;
                app.refresh().await?;
                app.modal = Modal::Palette { query: String::new(), selected: 0 };
            } else if key.code == KeyCode::Tab {
                app.modal = Modal::PromptEdit { label, body, editing_body: !editing_body };
            } else if key.code == KeyCode::Enter {
                if editing_body {
                    body.push('\n');
                }
                app.modal = Modal::PromptEdit { label, body, editing_body: true };
            } else {
                if editing_body {
                    edit_text(&mut body, &key);
                } else {
                    edit_text(&mut label, &key);
                }
                app.modal = Modal::PromptEdit { label, body, editing_body };
            }
        }
    }
    Ok(())
}

fn jump_to_slot(app: &mut App, slot: u8) {
    if slot <= app.a_slots {
        app.active_a = slot;
        app.focus = Focus::GroupA;
    } else if slot <= app.a_slots + app.b_slots {
        app.active_b = slot;
        app.focus = Focus::GroupB;
    }
}

async fn forward(app: &mut App, key: &KeyEvent) -> Result<()> {
    let Some(session_id) = app.active_session_id() else {
        return Ok(());
    };
    let bytes = key_to_bytes(key);
    if bytes.is_empty() {
        return Ok(());
    }
    app.client
        .request(Request::WriteStdin { session_id, bytes })
        .await?;
    Ok(())
}
