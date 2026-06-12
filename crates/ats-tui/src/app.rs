//! Application state: sessions, workspaces, focus, attached terminals.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use ats_core::client::Client;
use ats_core::rpc::{NoteInfo, PromptInfo, Request, Response, SessionInfo, TemplateInfo, WorkspaceInfo};
use ats_core::state::SessionState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Rail,
    GroupA,
    GroupB,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Modal {
    #[default]
    None,
    Help,
    /// template picker for Alt+s
    Spawn { selected: usize },
    /// review queue drain mode for Alt+q
    Queue { selected: usize },
    /// notes panel for Alt+n
    Notes { selected: usize },
    /// minimal note editor; Tab switches title/body, Ctrl+s saves
    NoteEdit {
        id: Option<i64>,
        title: String,
        body: String,
        editing_body: bool,
    },
    /// fuzzy prompt palette for Alt+p
    Palette { query: String, selected: usize },
    /// orchestrator ask panel for Alt+o
    Orchestrator {
        question: String,
        answer: Option<String>,
        busy: bool,
    },
    /// harvest diff viewer for Alt+h
    Diff {
        title: String,
        lines: Vec<String>,
        scroll: usize,
    },
    /// minimal prompt editor
    PromptEdit {
        label: String,
        body: String,
        editing_body: bool,
    },
}

/// One attached terminal: a client-side vt100 screen fed from scrollback +
/// live PtyOutput events.
pub struct Term {
    pub parser: vt100::Parser,
    pub cols: u16,
    pub rows: u16,
}

pub struct App {
    pub client: Arc<Client>,
    pub sessions: Vec<SessionInfo>,
    pub workspaces: Vec<WorkspaceInfo>,
    pub templates: Vec<TemplateInfo>,
    pub notes: Vec<NoteInfo>,
    pub prompts: Vec<PromptInfo>,
    pub focus: Focus,
    /// active slot per group (group A: 1..=a_slots, group B: a_slots+1..=a+b)
    pub active_a: u8,
    pub active_b: u8,
    pub a_slots: u8,
    pub b_slots: u8,
    pub modal: Modal,
    /// single-group client (second monitor): render only the focused group
    pub solo: bool,
    /// calm per-template tab tinting from `[ui.template_colors]`
    pub template_colors: HashMap<String, String>,
    /// everything-through mode: only the toggle key is intercepted
    pub raw_mode: bool,
    pub terms: HashMap<i64, Term>,
    pub status_line: String,
    pub should_quit: bool,
    /// results of background API calls (digest, ask) land here; the
    /// receiver is taken by the run loop (kept out of App so `select!`
    /// can poll it while handlers borrow App mutably)
    pub async_tx: tokio::sync::mpsc::UnboundedSender<AsyncMsg>,
    pub async_rx: Option<tokio::sync::mpsc::UnboundedReceiver<AsyncMsg>>,
}

#[derive(Debug)]
pub enum AsyncMsg {
    /// show in the status line
    Status(String),
    /// answer for the orchestrator panel
    Answer(Result<String, String>),
    /// harvest result for the diff viewer
    Diff { title: String, content: String },
}

impl App {
    pub fn new(client: Arc<Client>, a_slots: u8, b_slots: u8) -> Self {
        let (async_tx, async_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            client,
            sessions: Vec::new(),
            workspaces: Vec::new(),
            templates: Vec::new(),
            notes: Vec::new(),
            prompts: Vec::new(),
            focus: Focus::GroupA,
            active_a: 1,
            active_b: a_slots + 1,
            a_slots,
            b_slots,
            modal: Modal::None,
            solo: false,
            template_colors: HashMap::new(),
            raw_mode: false,
            terms: HashMap::new(),
            status_line: String::new(),
            should_quit: false,
            async_tx,
            async_rx: Some(async_rx),
        }
    }

    pub fn apply_async(&mut self, msg: AsyncMsg) {
        match msg {
            AsyncMsg::Status(s) => self.status_line = s,
            AsyncMsg::Answer(result) => {
                if let Modal::Orchestrator { answer, busy, .. } = &mut self.modal {
                    *busy = false;
                    *answer = Some(match result {
                        Ok(a) => a,
                        Err(e) => format!("error: {e}"),
                    });
                }
            }
            AsyncMsg::Diff { title, content } => {
                self.modal = Modal::Diff {
                    title,
                    lines: content.lines().map(str::to_owned).collect(),
                    scroll: 0,
                };
            }
        }
    }

    /// In solo mode only the focused group's session is attached.
    pub fn visible_slots(&self) -> Vec<u8> {
        if self.solo {
            vec![self.active_slot()]
        } else {
            vec![self.active_a, self.active_b]
        }
    }

    pub fn session_in_slot(&self, slot: u8) -> Option<&SessionInfo> {
        self.sessions
            .iter()
            .filter(|s| s.tab_slot == Some(slot))
            .max_by_key(|s| s.id)
    }

    pub fn active_slot(&self) -> u8 {
        match self.focus {
            Focus::GroupB => self.active_b,
            _ => self.active_a,
        }
    }

    pub fn active_session_id(&self) -> Option<i64> {
        self.session_in_slot(self.active_slot()).map(|s| s.id)
    }

    pub fn review_queue(&self) -> Vec<&SessionInfo> {
        self.sessions
            .iter()
            .filter(|s| {
                matches!(
                    s.state,
                    SessionState::Finished | SessionState::NeedsInput | SessionState::Error
                )
            })
            .collect()
    }

    pub async fn refresh(&mut self) -> Result<()> {
        if let Response::Sessions { sessions } = self.client.request(Request::ListSessions).await? {
            self.sessions = sessions;
        }
        if let Response::Workspaces { workspaces } =
            self.client.request(Request::ListWorkspaces).await?
        {
            self.workspaces = workspaces;
        }
        if let Response::Templates { templates } =
            self.client.request(Request::ListTemplates).await?
        {
            self.templates = templates;
        }
        if let Response::Notes { notes } = self.client.request(Request::ListNotes).await? {
            self.notes = notes;
        }
        if let Response::Prompts { prompts } = self.client.request(Request::ListPrompts).await? {
            self.prompts = prompts;
        }
        Ok(())
    }

    /// Prompts matching the palette query, best first (simple subsequence
    /// scoring — frecency order from the daemon breaks ties).
    pub fn filtered_prompts(&self, query: &str) -> Vec<&PromptInfo> {
        if query.is_empty() {
            return self.prompts.iter().collect();
        }
        let mut scored: Vec<(i64, &PromptInfo)> = self
            .prompts
            .iter()
            .filter_map(|p| fuzzy_score(query, &p.label).map(|s| (s, p)))
            .collect();
        scored.sort_by_key(|(s, _)| -*s);
        scored.into_iter().map(|(_, p)| p).collect()
    }

    /// Make sure the visible sessions (both groups, or just the focused
    /// one in solo mode) are attached, and nothing else is.
    pub async fn sync_attachments(&mut self, pane_a: (u16, u16), pane_b: (u16, u16)) -> Result<()> {
        let mut want: Vec<(i64, (u16, u16))> = Vec::new();
        let visible = self.visible_slots();
        if visible.contains(&self.active_a) {
            if let Some(s) = self.session_in_slot(self.active_a) {
                want.push((s.id, pane_a));
            }
        }
        if visible.contains(&self.active_b) {
            if let Some(s) = self.session_in_slot(self.active_b) {
                want.push((s.id, pane_b));
            }
        }

        let current: Vec<i64> = self.terms.keys().copied().collect();
        for id in current {
            if !want.iter().any(|(w, _)| *w == id) {
                self.terms.remove(&id);
                let _ = self
                    .client
                    .request(Request::DetachSession { session_id: id })
                    .await;
            }
        }

        for (id, (cols, rows)) in want {
            let (cols, rows) = (cols.max(10), rows.max(3));
            if let Some(term) = self.terms.get_mut(&id) {
                if term.cols != cols || term.rows != rows {
                    term.parser.set_size(rows, cols);
                    term.cols = cols;
                    term.rows = rows;
                    let _ = self
                        .client
                        .request(Request::ResizeSession { session_id: id, cols, rows })
                        .await;
                }
                continue;
            }
            let resp = self
                .client
                .request(Request::AttachSession { session_id: id })
                .await?;
            let mut parser = vt100::Parser::new(rows, cols, 2000);
            if let Response::Scrollback { data, .. } = resp {
                parser.process(&data);
            }
            self.terms.insert(id, Term { parser, cols, rows });
            let _ = self
                .client
                .request(Request::ResizeSession { session_id: id, cols, rows })
                .await;
        }
        Ok(())
    }

    pub fn feed_output(&mut self, session_id: i64, bytes: &[u8]) {
        if let Some(term) = self.terms.get_mut(&session_id) {
            term.parser.process(bytes);
        }
    }

    pub fn set_session_state(&mut self, session_id: i64, state: SessionState, detail: Option<String>) {
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            s.state = state;
            s.state_detail = detail;
        }
    }
}

pub fn state_glyph(state: SessionState) -> &'static str {
    match state {
        SessionState::Working => "·",
        SessionState::Idle => "○",
        SessionState::Finished => "●",
        SessionState::NeedsInput | SessionState::Error => "!",
        SessionState::Dead => "✕",
    }
}

/// Tiny subsequence matcher: all query chars must appear in order;
/// consecutive matches score higher. None = no match.
pub fn fuzzy_score(query: &str, text: &str) -> Option<i64> {
    let text: Vec<char> = text.to_lowercase().chars().collect();
    let mut score = 0i64;
    let mut ti = 0usize;
    let mut last_hit: Option<usize> = None;
    for qc in query.to_lowercase().chars() {
        if qc.is_whitespace() {
            continue;
        }
        let pos = text[ti..].iter().position(|&c| c == qc)? + ti;
        score += match last_hit {
            Some(l) if pos == l + 1 => 3,
            _ => 1,
        };
        last_hit = Some(pos);
        ti = pos + 1;
    }
    Some(score)
}

#[cfg(test)]
mod tests {
    use super::fuzzy_score;

    #[test]
    fn fuzzy_matches_subsequences_and_ranks_consecutive_higher() {
        assert!(fuzzy_score("rvw", "review changes").is_some());
        assert!(fuzzy_score("xyz", "review changes").is_none());
        let consecutive = fuzzy_score("rev", "review").unwrap();
        let scattered = fuzzy_score("rew", "review").unwrap();
        assert!(consecutive > scattered);
        // case-insensitive
        assert!(fuzzy_score("RE", "review").is_some());
    }
}
