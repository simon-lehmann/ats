//! Application state: sessions, workspaces, focus, attached terminals.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use ratatui::layout::Rect;
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
    /// the orchestrator's live Claude Code session, hosted as a centered
    /// overlay (Alt+o). Keys are forwarded to its PTY; Esc/Alt+o closes.
    Orchestrator,
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

/// Lines of scrollback each attached terminal retains and can scroll through.
pub const SCROLLBACK_LINES: usize = 2000;

/// One attached terminal: a client-side vt100 screen fed from scrollback +
/// live PtyOutput events.
pub struct Term {
    pub parser: vt100::Parser,
    pub cols: u16,
    pub rows: u16,
    /// rows scrolled up into history; 0 = live (bottom). Mirrors the value
    /// last handed to `parser.set_scrollback`.
    pub scrollback: usize,
}

impl Term {
    /// Move the view by `delta` rows (positive = back into history) and push
    /// the new offset into the parser. Clamped to [0, SCROLLBACK_LINES].
    pub fn scroll_by(&mut self, delta: isize) {
        let next = (self.scrollback as isize + delta).clamp(0, SCROLLBACK_LINES as isize) as usize;
        self.set_scroll(next);
    }

    pub fn set_scroll(&mut self, offset: usize) {
        let offset = offset.min(SCROLLBACK_LINES);
        self.scrollback = offset;
        self.parser.set_scrollback(offset);
    }
}

/// A clickable tab in a group's title bar (recorded during draw for mouse hits).
#[derive(Clone, Copy)]
pub struct TabHit {
    pub rect: Rect,
    pub slot: u8,
}

/// A clickable row in the left rail and what clicking it does.
#[derive(Clone, Copy)]
pub struct RailHit {
    pub y: u16,
    pub action: RailAction,
}

#[derive(Clone, Copy)]
pub enum RailAction {
    /// jump to the tab in this slot
    Tab(u8),
    /// open a rail tool's modal
    Queue,
    Notes,
    Palette,
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
    /// rolling actions-per-minute per session, pushed by the daemon's
    /// `SessionApm` event; rendered as a dim suffix in the tab label.
    pub apm: HashMap<i64, f32>,
    /// hit-test regions recorded on the last draw, for mouse routing
    pub pane_a_rect: Rect,
    pub pane_b_rect: Rect,
    pub orch_rect: Rect,
    pub tab_hits: Vec<TabHit>,
    pub rail_hits: Vec<RailHit>,
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
            apm: HashMap::new(),
            pane_a_rect: Rect::default(),
            pane_b_rect: Rect::default(),
            orch_rect: Rect::default(),
            tab_hits: Vec::new(),
            rail_hits: Vec::new(),
            status_line: String::new(),
            should_quit: false,
            async_tx,
            async_rx: Some(async_rx),
        }
    }

    pub fn apply_async(&mut self, msg: AsyncMsg) {
        match msg {
            AsyncMsg::Status(s) => self.status_line = s,
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

    /// The live orchestrator session (no tab slot; shown in the Alt+o overlay).
    pub fn orchestrator_session_id(&self) -> Option<i64> {
        newest_live_orchestrator(&self.sessions)
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
    pub async fn sync_attachments(
        &mut self,
        pane_a: (u16, u16),
        pane_b: (u16, u16),
        orch: Option<(i64, (u16, u16))>,
    ) -> Result<()> {
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
        // the orchestrator overlay, when open, hosts its session live
        if let Some(orch) = orch {
            if !want.iter().any(|(id, _)| *id == orch.0) {
                want.push(orch);
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
            let mut parser = vt100::Parser::new(rows, cols, SCROLLBACK_LINES);
            if let Response::Scrollback { data, .. } = resp {
                parser.process(&data);
            }
            self.terms.insert(id, Term { parser, cols, rows, scrollback: 0 });
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
            // a vt100 process() resets the view to the live screen; restore the
            // reader's scroll position so output doesn't yank them to the bottom
            if term.scrollback != 0 {
                term.parser.set_scrollback(term.scrollback);
            }
        }
    }

    /// The terminal the keyboard/scroll currently targets: the orchestrator's
    /// session when its overlay is open, otherwise the focused group's tab.
    fn scroll_target_id(&self) -> Option<i64> {
        if self.modal == Modal::Orchestrator {
            self.orchestrator_session_id()
        } else {
            self.active_session_id()
        }
    }

    /// Scroll the active terminal by `delta` rows (positive = back in history).
    pub fn scroll_active(&mut self, delta: isize) {
        if let Some(id) = self.scroll_target_id() {
            if let Some(term) = self.terms.get_mut(&id) {
                term.scroll_by(delta);
            }
        }
    }

    /// Jump the active terminal back to the live screen.
    pub fn scroll_active_to_live(&mut self) {
        if let Some(id) = self.scroll_target_id() {
            if let Some(term) = self.terms.get_mut(&id) {
                term.set_scroll(0);
            }
        }
    }

    /// Jump the active terminal to the oldest retained scrollback.
    pub fn scroll_active_to_top(&mut self) {
        if let Some(id) = self.scroll_target_id() {
            if let Some(term) = self.terms.get_mut(&id) {
                term.set_scroll(SCROLLBACK_LINES);
            }
        }
    }

    /// Page the active terminal up/down by ~one screen.
    pub fn scroll_active_page(&mut self, up: bool) {
        let page = self
            .scroll_target_id()
            .and_then(|id| self.terms.get(&id))
            .map(|t| t.rows.saturating_sub(1).max(1) as isize)
            .unwrap_or(10);
        self.scroll_active(if up { page } else { -page });
    }

    pub fn scroll_term(&mut self, id: i64, delta: isize) {
        if let Some(term) = self.terms.get_mut(&id) {
            term.scroll_by(delta);
        }
    }

    /// Which terminal session sits under terminal cell (`col`, `row`), for
    /// routing mouse-wheel scroll. Prefers the orchestrator overlay when open.
    pub fn term_id_at(&self, col: u16, row: u16) -> Option<i64> {
        if self.modal == Modal::Orchestrator {
            return rect_hit(self.orch_rect, col, row)
                .then(|| self.orchestrator_session_id())
                .flatten();
        }
        if self.solo {
            return rect_hit(self.pane_a_rect, col, row)
                .then(|| self.session_in_slot(self.active_slot()).map(|s| s.id))
                .flatten();
        }
        if rect_hit(self.pane_a_rect, col, row) {
            if let Some(s) = self.session_in_slot(self.active_a) {
                return Some(s.id);
            }
        }
        if rect_hit(self.pane_b_rect, col, row) {
            if let Some(s) = self.session_in_slot(self.active_b) {
                return Some(s.id);
            }
        }
        None
    }

    /// Handle a left-click at (`col`, `row`): select a clicked tab, act on a
    /// clicked rail row, or focus the group whose pane was clicked. Returns the
    /// modal to open (if any) so the async caller can act on it.
    pub fn click_at(&mut self, col: u16, row: u16) -> Option<Modal> {
        // a tab in either group's title bar
        if let Some(hit) = self.tab_hits.iter().find(|h| rect_hit(h.rect, col, row)).copied() {
            self.select_slot(hit.slot);
            return None;
        }
        // a row in the left rail
        if let Some(hit) = self.rail_hits.iter().find(|h| h.y == row).copied() {
            match hit.action {
                RailAction::Tab(slot) => self.select_slot(slot),
                RailAction::Queue => return Some(Modal::Queue { selected: 0 }),
                RailAction::Notes => return Some(Modal::Notes { selected: 0 }),
                RailAction::Palette => {
                    return Some(Modal::Palette { query: String::new(), selected: 0 })
                }
            }
            return None;
        }
        // clicking a pane just focuses its group
        if rect_hit(self.pane_a_rect, col, row) {
            self.focus = Focus::GroupA;
        } else if !self.solo && rect_hit(self.pane_b_rect, col, row) {
            self.focus = Focus::GroupB;
        }
        None
    }

    /// Focus a tab slot, switching to the group that owns it (any group).
    pub fn select_slot(&mut self, slot: u8) {
        if slot >= 1 && slot <= self.a_slots {
            self.active_a = slot;
            self.focus = Focus::GroupA;
        } else if slot > self.a_slots && slot <= self.a_slots + self.b_slots {
            self.active_b = slot;
            self.focus = Focus::GroupB;
        }
    }

    pub fn set_session_state(&mut self, session_id: i64, state: SessionState, detail: Option<String>) {
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            s.state = state;
            s.state_detail = detail;
        }
    }
}

/// Newest non-dead orchestrator session id. `sessions` may include dead rows,
/// and the orchestrator flag repeats across respawns, so filter by liveness and
/// take the most recent — a bare `find` on the flag could pick a dead session.
pub fn newest_live_orchestrator(sessions: &[SessionInfo]) -> Option<i64> {
    sessions
        .iter()
        .filter(|s| s.is_orchestrator && s.state != SessionState::Dead)
        .max_by_key(|s| s.id)
        .map(|s| s.id)
}

/// Is terminal cell (`col`, `row`) inside `r`?
fn rect_hit(r: Rect, col: u16, row: u16) -> bool {
    r.width > 0 && r.height > 0 && col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height
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
    use super::{fuzzy_score, newest_live_orchestrator, Term, SCROLLBACK_LINES};
    use ats_core::rpc::SessionInfo;
    use ats_core::state::SessionState;

    #[test]
    fn term_scroll_clamps_to_history_bounds() {
        let mut t = Term {
            parser: vt100::Parser::new(24, 80, SCROLLBACK_LINES),
            cols: 80,
            rows: 24,
            scrollback: 0,
        };
        t.scroll_by(-5); // can't go below the live screen
        assert_eq!(t.scrollback, 0);
        t.scroll_by(10);
        assert_eq!(t.scrollback, 10);
        t.scroll_by(isize::MAX / 2); // can't pass the retained history
        assert_eq!(t.scrollback, SCROLLBACK_LINES);
        t.set_scroll(0);
        assert_eq!(t.scrollback, 0);
    }

    fn session(id: i64, is_orchestrator: bool, state: SessionState) -> SessionInfo {
        SessionInfo {
            id,
            workspace_id: 1,
            tab_slot: None,
            pid: None,
            title: "orchestrator".into(),
            template_name: "scratch".into(),
            state,
            state_detail: None,
            workspace_path: "/x".into(),
            created_at: 0,
            last_activity_at: 0,
            is_orchestrator,
        }
    }

    #[test]
    fn newest_live_orchestrator_skips_dead_respawns() {
        // dead orchestrators 2 & 3 from earlier respawns, live one is 4
        let sessions = vec![
            session(2, true, SessionState::Dead),
            session(3, true, SessionState::Dead),
            session(4, true, SessionState::Idle),
            session(5, false, SessionState::Working),
        ];
        assert_eq!(newest_live_orchestrator(&sessions), Some(4));

        // no live orchestrator → None (so EnsureOrchestrator can respawn)
        assert_eq!(
            newest_live_orchestrator(&[session(2, true, SessionState::Dead)]),
            None
        );
        // never an orchestrator → None
        assert_eq!(
            newest_live_orchestrator(&[session(1, false, SessionState::Working)]),
            None
        );
    }

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
