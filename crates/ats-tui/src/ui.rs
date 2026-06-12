//! Rendering: left rail, two tab groups, vt100 grid panes, modals.
//! Calm by design — dim colors, one glyph per state, nothing blinks.

use ats_core::state::SessionState;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::{state_glyph, App, Focus, Modal};

const DIM: Style = Style::new().fg(Color::DarkGray);
const NORMAL: Style = Style::new().fg(Color::Gray);
const ACTIVE: Style = Style::new().fg(Color::White);
const ALERT: Style = Style::new().fg(Color::Yellow);

pub struct PaneAreas {
    pub a_inner: Rect,
    pub b_inner: Rect,
    /// inner rect of the orchestrator overlay when it's open (for PTY sizing)
    pub orch_inner: Option<Rect>,
}

pub fn draw(frame: &mut Frame, app: &App, rail_width: u16) -> PaneAreas {
    let vsplit = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(1)])
        .split(frame.area());
    draw_footer(frame, app, vsplit[1]);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(rail_width), Constraint::Min(20)])
        .split(vsplit[0]);

    draw_rail(frame, app, cols[0]);

    let (a_inner, b_inner) = if app.solo {
        // second-monitor mode: one group, full height
        let group = if app.focus == Focus::GroupB { Focus::GroupB } else { Focus::GroupA };
        let inner = draw_group(frame, app, cols[1], group);
        (inner, inner)
    } else {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(cols[1]);
        (
            draw_group(frame, app, rows[0], Focus::GroupA),
            draw_group(frame, app, rows[1], Focus::GroupB),
        )
    };

    let mut orch_inner = None;
    match &app.modal {
        Modal::Help => draw_help(frame),
        Modal::Spawn { selected } => draw_spawn(frame, app, *selected),
        Modal::Queue { selected } => draw_queue(frame, app, *selected),
        Modal::Notes { selected } => draw_notes(frame, app, *selected),
        Modal::NoteEdit { title, body, editing_body, .. } => {
            draw_editor(frame, "note — Tab title/body, Ctrl+s save", title, body, *editing_body)
        }
        Modal::Palette { query, selected } => draw_palette(frame, app, query, *selected),
        Modal::Orchestrator => orch_inner = Some(draw_orchestrator_overlay(frame, app)),
        Modal::Diff { title, lines, scroll } => draw_diff(frame, title, lines, *scroll),
        Modal::PromptEdit { label, body, editing_body } => {
            draw_editor(frame, "prompt — Tab label/body, Ctrl+s save", label, body, *editing_body)
        }
        Modal::None => {}
    }

    PaneAreas { a_inner, b_inner, orch_inner }
}

/// Centered overlay hosting the orchestrator's live Claude Code session.
/// Returns the inner rect so the session's PTY can be sized to match.
fn draw_orchestrator_overlay(frame: &mut Frame, app: &App) -> Rect {
    let area = frame.area();
    let w = area.width.saturating_sub(6).min(120);
    let h = area.height.saturating_sub(3);
    let view = centered(frame, w, h);
    let block = modal_block("orchestrator — Esc to close · keys go to the agent");
    let inner = block.inner(view);
    frame.render_widget(Clear, view);
    frame.render_widget(block, view);
    match app.orchestrator_session_id().and_then(|id| app.terms.get(&id)) {
        Some(term) => render_screen(frame.buffer_mut(), term.parser.screen(), inner),
        None => frame.render_widget(Paragraph::new(Line::styled("  attaching…", DIM)), inner),
    }
    inner
}

/// Bottom strip: a transient status message when one is set, otherwise a
/// vim-style context-sensitive key hint bar (keys bright-ish, labels dim).
fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    if !app.status_line.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled(format!(" {}", app.status_line), NORMAL)),
            area,
        );
        return;
    }
    let mut spans: Vec<Span> = vec![Span::raw(" ")];
    for (i, (key, label)) in footer_hints(app).into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", DIM));
        }
        if !key.is_empty() {
            spans.push(Span::styled(key, NORMAL));
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(label, DIM));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The most relevant bindings for the current context. First entry of each
/// pair is the key (omit for a plain note), second is what it does.
fn footer_hints(app: &App) -> Vec<(&'static str, &'static str)> {
    if app.raw_mode {
        return vec![("Alt+Esc", "exit raw"), ("", "every key goes to the terminal")];
    }
    match &app.modal {
        Modal::None => {
            // cold start: no templates and nothing in a tab → steer to setup
            let cold = app.templates.is_empty()
                && app.sessions.iter().all(|s| s.tab_slot.is_none());
            if cold {
                return vec![
                    ("Alt+o", "set up with the orchestrator"),
                    ("Alt+s", "spawn"),
                    ("F1", "help"),
                ];
            }
            match app.focus {
                Focus::Rail => vec![
                    ("Alt+1-0", "tab"),
                    ("Alt+s", "spawn"),
                    ("Alt+q", "queue"),
                    ("Enter", "back to panes"),
                    ("F1", "help"),
                ],
                _ => vec![
                    ("Alt+←/→", "tab"),
                    ("Alt+s", "spawn"),
                    ("Alt+o", "orchestrator"),
                    ("Alt+q", "queue"),
                    ("Alt+n", "notes"),
                    ("Alt+x", "detach"),
                    ("F1", "help"),
                ],
            }
        }
        Modal::Help => vec![("Esc", "close")],
        Modal::Spawn { .. } => {
            vec![("↑↓", "select"), ("Enter", "spawn"), ("p", "planning"), ("Esc", "close")]
        }
        Modal::Queue { .. } => vec![("↑↓", "select"), ("Enter", "jump"), ("Esc", "close")],
        Modal::Notes { .. } => vec![
            ("n", "new"),
            ("e", "edit"),
            ("f", "finalize"),
            ("Enter", "send"),
            ("Esc", "close"),
        ],
        Modal::NoteEdit { .. } | Modal::PromptEdit { .. } => {
            vec![("Tab", "title/body"), ("Ctrl+s", "save"), ("Esc", "cancel")]
        }
        Modal::Palette { .. } => {
            vec![("type", "filter"), ("Enter", "paste"), ("Ctrl+n", "add"), ("Esc", "close")]
        }
        Modal::Orchestrator => vec![("Esc", "close"), ("", "keys go to the orchestrator")],
        Modal::Diff { .. } => vec![("↑↓", "scroll"), ("PgUp/Dn", "page"), ("Esc", "close")],
    }
}

fn glyph_style(state: SessionState) -> Style {
    match state {
        SessionState::NeedsInput | SessionState::Error => ALERT,
        SessionState::Finished => NORMAL,
        _ => DIM,
    }
}

fn draw_rail(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::styled("▾ WORKSPACES", DIM));
    if app.workspaces.is_empty() {
        lines.push(Line::styled("  (none)", DIM));
    }
    for w in &app.workspaces {
        let name = std::path::Path::new(&w.path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| w.template_name.clone());
        // git figures: "clean" / "~3" dirty, "+2/-1" ahead/behind
        let git = match (w.dirty, w.ahead, w.behind) {
            (Some(0), Some(0), Some(0)) | (Some(0), None, None) => "clean".to_string(),
            (dirty, ahead, behind) => {
                let mut parts = Vec::new();
                if let Some(d) = dirty.filter(|d| *d > 0) {
                    parts.push(format!("~{d}"));
                }
                if let Some(a) = ahead.filter(|a| *a > 0) {
                    parts.push(format!("+{a}"));
                }
                if let Some(b) = behind.filter(|b| *b > 0) {
                    parts.push(format!("-{b}"));
                }
                if parts.is_empty() { "clean".into() } else { parts.join(" ") }
            }
        };
        let dirty_style = if git == "clean" { DIM } else { NORMAL };
        lines.push(Line::from(vec![
            Span::styled(format!("  {name:<14}"), NORMAL),
            Span::styled(format!("{git:<8}"), dirty_style),
            Span::styled(
                w.branch.clone().unwrap_or_default(),
                DIM,
            ),
        ]));
    }

    let queue = app.review_queue();
    lines.push(Line::raw(""));
    lines.push(Line::styled(format!("▾ REVIEW QUEUE ({})", queue.len()), DIM));
    for s in queue.iter().take(8) {
        let detail = s.state_detail.as_deref().unwrap_or("");
        let slot = s.tab_slot.map(|n| n.to_string()).unwrap_or_else(|| "-".into());
        let mut text = format!("  {slot} {} {detail}", state_glyph(s.state));
        text.truncate(area.width.saturating_sub(2) as usize);
        lines.push(Line::styled(text, glyph_style(s.state)));
    }

    lines.push(Line::raw(""));
    lines.push(Line::styled("▾ SESSIONS", DIM));
    for s in &app.sessions {
        if s.tab_slot.is_none() {
            continue;
        }
        let slot = s.tab_slot.unwrap();
        let mut text = format!("  {slot} {} {}", state_glyph(s.state), s.title);
        text.truncate(area.width.saturating_sub(2) as usize);
        lines.push(Line::styled(text, glyph_style(s.state)));
    }

    lines.push(Line::raw(""));
    lines.push(Line::styled(format!("▾ PROMPTS ({})", app.prompts.len()), DIM));
    for p in app.prompts.iter().take(3) {
        let mut text = format!("  {}", p.label);
        text.truncate(area.width.saturating_sub(2) as usize);
        lines.push(Line::styled(text, DIM));
    }

    lines.push(Line::raw(""));
    lines.push(Line::styled(format!("▾ NOTES ({})", app.notes.len()), DIM));
    for n in app.notes.iter().take(4) {
        let marker = match n.state.as_str() {
            "finalized" => "▪",
            "claimed" => "→",
            _ => "·",
        };
        let mut text = format!("  {marker} {}", n.title);
        text.truncate(area.width.saturating_sub(2) as usize);
        lines.push(Line::styled(text, DIM));
    }

    let border_style = if app.focus == Focus::Rail { ACTIVE } else { DIM };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Span::styled(" ats ", DIM));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_group(frame: &mut Frame, app: &App, area: Rect, group: Focus) -> Rect {
    let (first, count, active) = if group == Focus::GroupA {
        (1u8, app.a_slots, app.active_a)
    } else {
        (app.a_slots + 1, app.b_slots, app.active_b)
    };

    // tab bar: number + short name + glyph, dim; per-template tint stays calm
    let mut spans: Vec<Span> = vec![Span::raw(" ")];
    for slot in first..first + count {
        let session = app.session_in_slot(slot);
        let label = match session {
            Some(s) => {
                let mut name = s.title.clone();
                name.truncate(10);
                format!("{} {name} {}", slot_key_label(slot), state_glyph(s.state))
            }
            None => format!("{} —", slot_key_label(slot)),
        };
        let tint = session
            .and_then(|s| app.template_colors.get(&s.template_name))
            .and_then(|name| parse_color(name));
        let style = if slot == active {
            if app.focus == group { ACTIVE } else { NORMAL }
        } else {
            tint.map(|c| Style::new().fg(c)).unwrap_or(DIM)
        };
        spans.push(Span::styled(format!(" {label} "), style));
        spans.push(Span::styled("│", DIM));
    }
    if app.raw_mode && app.focus == group {
        spans.push(Span::styled(" RAW ", ALERT));
    }

    let border_style = if app.focus == group { NORMAL } else { DIM };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Line::from(spans));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    match app.session_in_slot(active).map(|s| s.id) {
        Some(id) => {
            if let Some(term) = app.terms.get(&id) {
                render_screen(frame.buffer_mut(), term.parser.screen(), inner);
            } else {
                frame.render_widget(
                    Paragraph::new(Line::styled("attaching…", DIM)),
                    inner,
                );
            }
        }
        None => {
            frame.render_widget(
                Paragraph::new(vec![
                    Line::raw(""),
                    Line::styled("  empty slot — Alt+s to spawn a session", DIM),
                ]),
                inner,
            );
        }
    }
    inner
}

/// Map a tab slot to the key that reaches it (slot 10 = key 0).
fn slot_key_label(slot: u8) -> String {
    if slot == 10 { "0".into() } else { slot.to_string() }
}

/// Named or `#rrggbb` colors for `[ui.template_colors]`.
fn parse_color(name: &str) -> Option<Color> {
    let n = name.trim().to_lowercase();
    if let Some(hex) = n.strip_prefix('#') {
        if hex.len() == 6 {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            return Some(Color::Rgb(r, g, b));
        }
        return None;
    }
    Some(match n.as_str() {
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "gray" | "grey" => Color::Gray,
        "darkgray" | "darkgrey" => Color::DarkGray,
        "white" => Color::White,
        _ => return None,
    })
}

fn draw_diff(frame: &mut Frame, title: &str, lines: &[String], scroll: usize) {
    let area = frame.area();
    let w = area.width.saturating_sub(6).min(110);
    let h = area.height.saturating_sub(4);
    let view = centered(frame, w, h);
    let visible = (h as usize).saturating_sub(2);

    let mut rendered: Vec<Line> = Vec::new();
    for l in lines.iter().skip(scroll).take(visible) {
        let style = if l.starts_with('+') && !l.starts_with("+++") {
            Style::new().fg(Color::Green)
        } else if l.starts_with('-') && !l.starts_with("---") {
            Style::new().fg(Color::Red)
        } else if l.starts_with("@@") {
            Style::new().fg(Color::Cyan)
        } else if l.starts_with("diff ") || l.starts_with("index ") {
            ACTIVE
        } else {
            NORMAL
        };
        let mut text = l.clone();
        text.truncate(w.saturating_sub(2) as usize);
        rendered.push(Line::styled(text, style));
    }
    if rendered.is_empty() {
        rendered.push(Line::styled("  (no changes against base)", DIM));
    }
    let pos = format!(" {title} — {}/{} (↑↓ PgUp/PgDn, Esc) ", scroll, lines.len());
    frame.render_widget(Clear, view);
    frame.render_widget(Paragraph::new(rendered).block(modal_block(&pos)), view);
}

fn vt_color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// Blit a vt100 screen into the ratatui buffer at `area`.
fn render_screen(buf: &mut Buffer, screen: &vt100::Screen, area: Rect) {
    let (rows, cols) = screen.size();
    for row in 0..rows.min(area.height) {
        for col in 0..cols.min(area.width) {
            let Some(cell) = screen.cell(row, col) else { continue };
            let x = area.x + col;
            let y = area.y + row;
            let target = &mut buf[(x, y)];
            let contents = cell.contents();
            if contents.is_empty() {
                target.set_symbol(" ");
            } else {
                target.set_symbol(&contents);
            }
            let mut style = Style::new()
                .fg(vt_color(cell.fgcolor()))
                .bg(vt_color(cell.bgcolor()));
            if cell.bold() {
                style = style.add_modifier(Modifier::BOLD);
            }
            if cell.italic() {
                style = style.add_modifier(Modifier::ITALIC);
            }
            if cell.underline() {
                style = style.add_modifier(Modifier::UNDERLINED);
            }
            if cell.inverse() {
                style = style.add_modifier(Modifier::REVERSED);
            }
            target.set_style(style);
        }
    }
    // hardware-style cursor: reverse the cell under the cursor
    if !screen.hide_cursor() {
        let (crow, ccol) = screen.cursor_position();
        if crow < area.height && ccol < area.width {
            buf[(area.x + ccol, area.y + crow)]
                .set_style(Style::new().add_modifier(Modifier::REVERSED));
        }
    }
}

fn centered(frame: &Frame, width: u16, height: u16) -> Rect {
    let area = frame.area();
    let w = width.min(area.width.saturating_sub(2));
    let h = height.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

fn modal_block(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(NORMAL)
        .title(Span::styled(format!(" {title} "), ACTIVE))
}

fn draw_help(frame: &mut Frame) {
    let lines: Vec<Line> = [
        ("Alt+1..5 / Alt+6..0", "jump to tab in group A / B"),
        ("Alt+←/→", "previous / next tab"),
        ("Alt+` ", "toggle focus group A ↔ B"),
        ("Alt+r", "focus rail"),
        ("Alt+s", "spawn: template → workspace → session"),
        ("Alt+q", "review queue (Enter jump, Esc close)"),
        ("Alt+n", "notes: n new, e edit, f finalize, Enter send"),
        ("Alt+p", "prompt palette (type to filter, Enter paste)"),
        ("Alt+d", "digest the active session (one line)"),
        ("Alt+o", "orchestrator chat: it sets up, spawns, instructs"),
        ("Alt+h", "harvest active workspace → diff viewer"),
        ("Alt+Esc", "raw mode: forward all keys to the terminal"),
        ("Alt+x", "detach UI (daemon and agents keep running)"),
        ("F1 / Esc", "this help / close"),
    ]
    .iter()
    .map(|(k, v)| {
        Line::from(vec![
            Span::styled(format!("  {k:<22}"), ACTIVE),
            Span::styled(*v, NORMAL),
        ])
    })
    .collect();
    let area = centered(frame, 64, lines.len() as u16 + 2);
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines).block(modal_block("help")), area);
}

fn draw_spawn(frame: &mut Frame, app: &App, selected: usize) {
    let mut lines: Vec<Line> = Vec::new();
    if app.templates.is_empty() {
        lines.push(Line::styled(
            "  no templates — register one: ats register <name> <path>",
            NORMAL,
        ));
    }
    for (i, t) in app.templates.iter().enumerate() {
        let style = if i == selected { ACTIVE } else { NORMAL };
        let marker = if i == selected { "▸" } else { " " };
        lines.push(Line::styled(format!(" {marker} {:<16} {}", t.name, t.path), style));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled("  p — planning session (scratch dir, no clone)", DIM));
    let area = centered(frame, 64, lines.len() as u16 + 2);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(modal_block("spawn session — Enter to confirm")),
        area,
    );
}

fn draw_notes(frame: &mut Frame, app: &App, selected: usize) {
    let mut lines: Vec<Line> = Vec::new();
    if app.notes.is_empty() {
        lines.push(Line::styled("  no notes — n to draft one", DIM));
    }
    for (i, n) in app.notes.iter().enumerate() {
        let style = if i == selected { ACTIVE } else { NORMAL };
        let marker = if i == selected { "▸" } else { " " };
        let pin = if n.pinned { "*" } else { " " };
        let mut text = format!(" {marker}{pin}[{:<9}] {}", n.state, n.title);
        text.truncate(70);
        lines.push(Line::styled(text, style));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "  n new · e edit · f finalize · Enter send to active session",
        DIM,
    ));
    let area = centered(frame, 74, (lines.len() as u16 + 2).max(6));
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines).block(modal_block("notes")), area);
}

fn draw_editor(frame: &mut Frame, title: &str, first: &str, body: &str, editing_body: bool) {
    let (first_style, body_style) = if editing_body { (NORMAL, ACTIVE) } else { (ACTIVE, NORMAL) };
    let mut lines = vec![
        Line::styled(format!("{first}{}", if editing_body { "" } else { "▎" }), first_style),
        Line::styled("─".repeat(66), DIM),
    ];
    for l in body.split('\n') {
        lines.push(Line::styled(l.to_string(), body_style));
    }
    if editing_body {
        if let Some(last) = lines.last_mut() {
            last.spans.push(Span::styled("▎", ACTIVE));
        }
    }
    let area = centered(frame, 70, (lines.len() as u16 + 2).clamp(8, 24));
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines).block(modal_block(title)), area);
}

fn draw_palette(frame: &mut Frame, app: &App, query: &str, selected: usize) {
    let filtered = app.filtered_prompts(query);
    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled("  ❯ ", DIM),
            Span::styled(query.to_string(), ACTIVE),
            Span::styled("▎", ACTIVE),
        ]),
        Line::styled("─".repeat(66), DIM),
    ];
    if filtered.is_empty() {
        lines.push(Line::styled("  no matches — Ctrl+n to add a prompt", DIM));
    }
    for (i, p) in filtered.iter().take(12).enumerate() {
        let style = if i == selected { ACTIVE } else { NORMAL };
        let marker = if i == selected { "▸" } else { " " };
        let preview: String = p.body.split('\n').next().unwrap_or("").chars().take(34).collect();
        let mut text = format!(" {marker} {:<18} {preview}", p.label);
        text.truncate(68);
        lines.push(Line::styled(text, style));
    }
    let area = centered(frame, 70, (lines.len() as u16 + 2).max(7));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(modal_block("prompts — Enter pastes into active session")),
        area,
    );
}

fn draw_queue(frame: &mut Frame, app: &App, selected: usize) {
    let queue = app.review_queue();
    let mut lines: Vec<Line> = Vec::new();
    if queue.is_empty() {
        lines.push(Line::styled("  queue is empty — nothing needs you", DIM));
    }
    for (i, s) in queue.iter().enumerate() {
        let style = if i == selected { ACTIVE } else { glyph_style(s.state) };
        let marker = if i == selected { "▸" } else { " " };
        let slot = s.tab_slot.map(|n| n.to_string()).unwrap_or_else(|| "-".into());
        let detail = s.state_detail.as_deref().unwrap_or("");
        let mut text = format!(" {marker} [{slot}] {} {} {detail}", state_glyph(s.state), s.title);
        text.truncate(70);
        lines.push(Line::styled(text, style));
    }
    let area = centered(frame, 74, lines.len() as u16 + 2);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(modal_block("review queue — Enter to jump")),
        area,
    );
}
