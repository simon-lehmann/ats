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
}

pub fn draw(frame: &mut Frame, app: &App, rail_width: u16) -> PaneAreas {
    let vsplit = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(1)])
        .split(frame.area());
    frame.render_widget(
        Paragraph::new(Line::styled(format!(" {}", app.status_line), DIM)),
        vsplit[1],
    );

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(rail_width), Constraint::Min(20)])
        .split(vsplit[0]);

    draw_rail(frame, app, cols[0]);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(cols[1]);

    let a_inner = draw_group(frame, app, rows[0], Focus::GroupA);
    let b_inner = draw_group(frame, app, rows[1], Focus::GroupB);

    match &app.modal {
        Modal::Help => draw_help(frame),
        Modal::Spawn { selected } => draw_spawn(frame, app, *selected),
        Modal::Queue { selected } => draw_queue(frame, app, *selected),
        Modal::Notes { selected } => draw_notes(frame, app, *selected),
        Modal::NoteEdit { title, body, editing_body, .. } => {
            draw_editor(frame, "note — Tab title/body, Ctrl+s save", title, body, *editing_body)
        }
        Modal::Palette { query, selected } => draw_palette(frame, app, query, *selected),
        Modal::Orchestrator { question, answer, busy } => {
            draw_orchestrator(frame, question, answer.as_deref(), *busy)
        }
        Modal::PromptEdit { label, body, editing_body } => {
            draw_editor(frame, "prompt — Tab label/body, Ctrl+s save", label, body, *editing_body)
        }
        Modal::None => {}
    }

    PaneAreas { a_inner, b_inner }
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

    // tab bar: number + short name + glyph, dim
    let mut spans: Vec<Span> = vec![Span::raw(" ")];
    for slot in first..first + count {
        let label = match app.session_in_slot(slot) {
            Some(s) => {
                let mut name = s.title.clone();
                name.truncate(10);
                format!("{} {name} {}", slot_key_label(slot), state_glyph(s.state))
            }
            None => format!("{} —", slot_key_label(slot)),
        };
        let style = if slot == active {
            if app.focus == group { ACTIVE } else { NORMAL }
        } else {
            DIM
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
        ("Alt+` ", "toggle focus group A ↔ B"),
        ("Alt+r", "focus rail"),
        ("Alt+s", "spawn: template → workspace → session"),
        ("Alt+q", "review queue (Enter jump, Esc close)"),
        ("Alt+n", "notes: n new, e edit, f finalize, Enter send"),
        ("Alt+p", "prompt palette (type to filter, Enter paste)"),
        ("Alt+d", "digest the active session (one line)"),
        ("Alt+o", "orchestrator: ask across all sessions"),
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

fn draw_orchestrator(frame: &mut Frame, question: &str, answer: Option<&str>, busy: bool) {
    let mut lines = vec![
        Line::from(vec![
            Span::styled("  ? ", DIM),
            Span::styled(question.to_string(), ACTIVE),
            Span::styled(if busy { "" } else { "▎" }, ACTIVE),
        ]),
        Line::styled("─".repeat(70), DIM),
    ];
    if busy {
        lines.push(Line::styled("  thinking…", DIM));
    } else if let Some(a) = answer {
        for l in a.lines() {
            // crude wrap at panel width
            let mut rest = l;
            loop {
                let take = rest.chars().take(70).collect::<String>();
                lines.push(Line::styled(format!("  {take}"), NORMAL));
                if rest.chars().count() <= 70 {
                    break;
                }
                rest = &rest[take.len()..];
            }
        }
    } else {
        lines.push(Line::styled(
            "  ask across all sessions, e.g. \"which sessions are blocked?\"",
            DIM,
        ));
    }
    let area = centered(frame, 76, (lines.len() as u16 + 2).clamp(7, 28));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(modal_block("orchestrator — Enter to ask")),
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
