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
    match app.modal {
        Modal::Help => {
            if matches!(key.code, KeyCode::Esc | KeyCode::F(1) | KeyCode::Char('q')) {
                app.modal = Modal::None;
            }
            return Ok(());
        }
        Modal::Spawn { selected } => {
            match key.code {
                KeyCode::Esc => app.modal = Modal::None,
                KeyCode::Up => {
                    app.modal = Modal::Spawn { selected: selected.saturating_sub(1) };
                }
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
                    app.modal = Modal::None;
                }
                _ => {}
            }
            return Ok(());
        }
        Modal::Queue { selected } => {
            match key.code {
                KeyCode::Esc => app.modal = Modal::None,
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
                    app.modal = Modal::None;
                }
                _ => {}
            }
            return Ok(());
        }
        Modal::None => {}
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
