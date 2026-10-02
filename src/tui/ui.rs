//! Immediate-mode rendering for the dashboard. Layout: tab bar on top, content
//! area in the middle, help bar at the bottom. The Instances tab renders real
//! data from the latest snapshot.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Paragraph, Row, Table, TableState, Tabs};
use tui_term::widget::{Cursor, PseudoTerminal};

use super::app::{
    pane_lines, title_of, when, App, ConfigView, Focus, InboxFocus, Modal, PaneLine, Pane, PortRow,
    Side, Tab, TextModal, Thread, Tone, View,
};
use super::data::{
    humanize_secs, sandbox_stats, totals_line, ContainerStatus, InstanceRow, Node, SandboxRow,
    ServiceRow, Snapshot,
};
use super::procs::{is_agent, ProcState, MESSAGE_ROW};
use super::prompt::Prompt;
use crate::devsbd::notify::Level;
use crate::inbox::{Kind, State};
use crate::render::JsonLine;

const ACCENT: Color = Color::Rgb(175, 135, 255);
const SELECTION: Color = Color::Rgb(0, 215, 135);

pub fn draw(frame: &mut Frame, app: &App) {
    // The prompt needs a second bottom line for its candidates/error hint.
    let bottom = if app.prompt.is_some() { 2 } else { 1 };
    let [tab_area, content_area, bottom_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(bottom),
    ])
    .areas(frame.area());

    draw_tabs(frame, app, tab_area);
    draw_content(frame, app, content_area);
    if let Some(prompt) = &app.prompt {
        draw_prompt(frame, prompt, bottom_area);
    } else {
        draw_help(frame, app, bottom_area);
    }

    // A modal draws over everything, using the full frame.
    match &app.modal {
        Modal::None => {}
        Modal::Config(view) => draw_config_modal(frame, view),
        Modal::Help(view) => draw_text_modal(frame, view),
        Modal::Logs(view) => draw_text_modal(frame, view),
    }
}

fn draw_tabs(frame: &mut Frame, app: &App, area: Rect) {
    // Right-aligned totals summary shares the tab-bar row; tabs take the rest.
    let totals = app
        .snapshot
        .as_ref()
        .map(|s| totals_line(s, s.collected_at.elapsed()));
    let totals_w = totals.as_deref().map_or(0, |t| t.chars().count() as u16);
    // Leave a gap before the totals; drop them entirely when the row is too narrow.
    let reserve = if totals_w > 0 && area.width > totals_w + 4 {
        totals_w + 2
    } else {
        0
    };
    let [tabs_area, totals_area] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(reserve)]).areas(area);

    let selected = Tab::ALL.iter().position(|t| *t == app.tab).unwrap_or(0);
    let tabs = Tabs::new(Tab::ALL.iter().map(|t| app.tab_title(*t)))
        .select(selected)
        .style(Style::default())
        .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
        .divider(" ");
    frame.render_widget(tabs, tabs_area);

    if reserve > 0 {
        if let Some(totals) = totals {
            let line = Line::from(Span::styled(totals, Style::default().add_modifier(Modifier::DIM)))
                .alignment(Alignment::Right);
            frame.render_widget(Paragraph::new(line), totals_area);
        }
    }
}

fn draw_content(frame: &mut Frame, app: &App, area: Rect) {
    match app.tab {
        Tab::Instances => draw_instances(frame, app, area),
        Tab::Services => draw_services(frame, app, area),
        Tab::Ports => draw_ports(frame, app, area),
        Tab::Inbox => draw_inbox(frame, app, area),
    }
}

/// Split a tab's content `area` into the top part (error line + table) and the
/// bottom section (Detail, plus the terminal panel when terminals are open).
///
/// The split is deliberately independent of snapshot state: the error line is
/// carved *inside* `top` by the caller, never here. That keeps the bottom
/// geometry — and therefore [`term_pane_size`] — a pure function of the frame
/// size and whether any terminal exists, so the event loop's PTY-resize math
/// (step 4) can't drift from what the draw path actually laid out.
///
/// Returns `(top, detail, Some(terms))` when `!app.terms.is_empty()`: the bottom
/// section is 50% of the content area, split `[Detail 30%, terminal Min(0)]`.
/// Returns `(top, detail, None)` otherwise: today's `Length(9)` full-width
/// Detail, no terminal panel.
fn content_areas(app: &App, area: Rect) -> (Rect, Rect, Option<Rect>) {
    if app.terms.is_empty() {
        let [top, detail] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(9)]).areas(area);
        (top, detail, None)
    } else {
        let (top, detail, terms) = open_bottom_split(area);
        (top, detail, Some(terms))
    }
}

/// The terminals-open split of a content area: `(top, detail, terminal panel)`.
/// The single home for the 50% bottom / 30-70 horizontal constants, shared by
/// the draw path ([`content_areas`]) and the PTY-sizing math ([`term_pane_size`])
/// so the two cannot drift.
fn open_bottom_split(area: Rect) -> (Rect, Rect, Rect) {
    let [top, bottom] =
        Layout::vertical([Constraint::Min(0), Constraint::Percentage(50)]).areas(area);
    let [detail, terms] =
        Layout::horizontal([Constraint::Percentage(30), Constraint::Min(0)]).areas(bottom);
    (top, detail, terms)
}

/// `(rows, cols)` of the active terminal's *inner* screen — the terminal panel
/// Rect minus its one-cell block border on each side. Derived from the same
/// layout functions the draw path uses ([`draw`] for the bottom-bar height,
/// [`content_areas`] for the panel), so step 4 can size the PTY to exactly what
/// gets rendered. Returns `None` when no terminal panel is laid out.
pub fn term_pane_size(frame: Rect, prompt_open: bool) -> Option<(u16, u16)> {
    let panel = terminal_panel_rect(frame, prompt_open);
    // Inner screen = panel minus the block border (1 cell each side).
    let rows = panel.height.saturating_sub(2);
    let cols = panel.width.saturating_sub(2);
    if rows == 0 || cols == 0 {
        None
    } else {
        Some((rows, cols))
    }
}

/// The full terminal-panel Rect (border included) for a frame of size `frame`,
/// mirroring `draw()`'s vertical split and the `open_bottom_split` bottom split.
/// Independent of whether terminals are actually open — callers gate on
/// `!app.terms.is_empty()` — so the mouse hit-test and the PTY-size math share
/// exactly one layout definition. `pub(crate)` for the event loop's mouse routing.
pub(crate) fn terminal_panel_rect(frame: Rect, prompt_open: bool) -> Rect {
    // Mirror draw()'s vertical split: tab bar (1), content (Min 0), bottom bar.
    let bottom = if prompt_open { 2 } else { 1 };
    let [_tab_area, content_area, _bottom_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(bottom),
    ])
    .areas(frame);
    let (_top, _detail, panel) = open_bottom_split(content_area);
    panel
}

/// Mouse hit-test for the terminal panel's tab strip. Given the panel Rect, the
/// focus flag (a focused panel prepends the `▶ ` mark, shifting labels right),
/// and a click `(col, row)`, return the index of the tab whose label span the
/// click lands on — or `None` when the click is not on the title row or misses
/// every label. The label spans are derived from the SAME strings the strip
/// renders ([`terminal_tab_labels`]), so a hit can only land where a tab is drawn.
pub(crate) fn terminal_tab_hit(
    app: &App,
    panel: Rect,
    focused: bool,
    col: u16,
    row: u16,
) -> Option<usize> {
    // The tab strip lives in the block's top border row.
    if row != panel.y {
        return None;
    }
    // Title starts one cell in from the left border corner, then the focus mark.
    let mut x = panel.x + 1;
    if focused {
        x += FOCUS_MARK_WIDTH;
    }
    for (i, label) in terminal_tab_labels(app).into_iter().enumerate() {
        if i > 0 {
            x += TAB_SEP_WIDTH; // the two-space separator between tabs
        }
        let width = label.chars().count() as u16;
        if col >= x && col < x + width {
            return Some(i);
        }
        x += width;
    }
    None
}

/// Display width of the `▶ ` focus mark prepended to the tab strip when the
/// terminal is focused (`▶` is one column plus a trailing space).
const FOCUS_MARK_WIDTH: u16 = 2;
/// Width of the two-space separator between tab labels.
const TAB_SEP_WIDTH: u16 = 2;

fn draw_services(frame: &mut Frame, app: &App, area: Rect) {
    let snapshot = app.snapshot.as_ref();
    let rows: &[ServiceRow] = snapshot.map_or(&[], |s| s.services.as_slice());

    let (top, detail_area, terms_area) = content_areas(app, area);
    // Carve the error line out of the top part (never the bottom), so the
    // terminal panel's geometry stays independent of snapshot state.
    let error = snapshot.and_then(|s| s.error.as_deref());
    let [err_area, table_area] = Layout::vertical([
        Constraint::Length(if error.is_some() { 1 } else { 0 }),
        Constraint::Min(0),
    ])
    .areas(top);

    if let Some(err) = error {
        let line = Line::from(Span::styled(err.to_string(), Style::default().fg(Color::Red)));
        frame.render_widget(Paragraph::new(line), err_area);
    }

    // Terminal focused ⇒ the terminal panel is the accented one; dim the rest.
    let panel_focused = app.focus == Focus::Terminal;

    if rows.is_empty() {
        draw_services_empty(frame, table_area, panel_focused);
        draw_service_detail(frame, None, detail_area, panel_focused);
    } else {
        draw_services_table(frame, app, rows, table_area, panel_focused);
        draw_service_detail(frame, rows.get(app.selected()), detail_area, panel_focused);
    }
    if let Some(terms_area) = terms_area {
        draw_terminal_panel(frame, app, terms_area);
    }
}

fn draw_services_empty(frame: &mut Frame, area: Rect, term_focused: bool) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(dash_border_style(term_focused))
        .title(Tab::Services.title());
    let text = Line::from(Span::styled(
        "no services defined in devsandboxes.toml",
        Style::default().add_modifier(Modifier::DIM),
    ))
    .alignment(Alignment::Center);
    frame.render_widget(Paragraph::new(text).block(block), area);
}

fn draw_services_table(
    frame: &mut Frame,
    app: &App,
    rows: &[ServiceRow],
    area: Rect,
    term_focused: bool,
) {
    let header = Row::new(
        ["NAME", "SCOPE", "SOURCE", "PORTS", "STATUS", "USED BY"]
            .into_iter()
            .map(Cell::from),
    )
    .style(Style::default().add_modifier(Modifier::DIM));

    let table_rows: Vec<Row> = rows.iter().map(service_row).collect();

    let widths = [
        Constraint::Length(14),
        Constraint::Length(9),
        Constraint::Min(20),
        Constraint::Length(14),
        Constraint::Length(12),
        Constraint::Min(12),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(dash_border_style(term_focused))
        .title(Tab::Services.title());

    let table = Table::new(table_rows, widths)
        .header(header)
        .block(block)
        .column_spacing(1)
        .row_highlight_style(Style::default().fg(SELECTION).add_modifier(Modifier::BOLD));

    let mut state = TableState::default().with_selected(Some(app.selected()));
    frame.render_stateful_widget(table, area, &mut state);
}

fn service_row(r: &ServiceRow) -> Row<'_> {
    let ports = if r.ports.is_empty() {
        "-".to_string()
    } else {
        r.ports.join(",")
    };
    let used_by = if r.used_by.is_empty() {
        "-".to_string()
    } else {
        r.used_by.join(",")
    };
    // Drifted services get a subtle yellow `!` suffix on the NAME cell (the
    // Instances tab only shows drift in the Detail panel; the Services table is
    // flatter, so a name marker is the least-noisy live signal). The Detail
    // panel spells the drift out.
    let name = if r.drift {
        Cell::from(Line::from(vec![
            Span::raw(r.name.clone()),
            Span::styled(" !", Style::default().fg(Color::Yellow)),
        ]))
    } else {
        Cell::from(r.name.clone())
    };
    Row::new(vec![
        name,
        Cell::from(Span::styled(r.scope.to_string(), scope_style(r.scope))),
        Cell::from(Span::styled(r.source.clone(), source_style(&r.source))),
        Cell::from(ports),
        service_status_cell(r),
        Cell::from(used_by),
    ])
}

fn scope_style(scope: &str) -> Style {
    match scope {
        "global" => Style::default().fg(Color::Blue),
        _ => Style::default().fg(Color::Magenta),
    }
}

/// Color the SOURCE column like `ls`: image green, dockerfile yellow, `?` dim.
fn source_style(source: &str) -> Style {
    if source.starts_with("image ") {
        Style::default().fg(Color::Green)
    } else if source.starts_with("dockerfile ") {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().add_modifier(Modifier::DIM)
    }
}

/// Summarize a service's backing containers into one STATUS cell: `N running`
/// (green) when any are up, else `N exited` (red) when any exist, else `-` dim
/// (`…` dim while the runtime hasn't been listed yet).
fn service_status_cell(r: &ServiceRow) -> Cell<'static> {
    if r.containers.iter().any(|(_, s)| *s == ContainerStatus::Unknown) {
        return Cell::from(Span::styled("…", Style::default().add_modifier(Modifier::DIM)));
    }
    let running = r
        .containers
        .iter()
        .filter(|(_, s)| matches!(s, ContainerStatus::Running(_)))
        .count();
    let present = r
        .containers
        .iter()
        .filter(|(_, s)| !matches!(s, ContainerStatus::Missing))
        .count();
    if running > 0 {
        Cell::from(Span::styled(
            format!("{running} running"),
            Style::default().fg(Color::Green),
        ))
    } else if present > 0 {
        Cell::from(Span::styled(
            format!("{present} exited"),
            Style::default().fg(Color::Red),
        ))
    } else {
        Cell::from(Span::styled("-", Style::default().add_modifier(Modifier::DIM)))
    }
}

fn draw_service_detail(
    frame: &mut Frame,
    row: Option<&ServiceRow>,
    area: Rect,
    term_focused: bool,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(dash_border_style(term_focused))
        .title("Detail");

    let Some(r) = row else {
        frame.render_widget(Paragraph::new("").block(block), area);
        return;
    };

    let mut lines: Vec<Line> = Vec::new();
    if r.containers.is_empty() {
        lines.push(Line::from(Span::styled(
            "no containers",
            Style::default().add_modifier(Modifier::DIM),
        )));
    } else {
        for (name, status) in &r.containers {
            lines.push(Line::from(vec![
                Span::raw(name.clone()),
                Span::raw("  "),
                Span::styled(status.label().to_string(), status_style(status)),
            ]));
        }
    }
    let ports = if r.ports.is_empty() {
        "-".to_string()
    } else {
        r.ports.join(",")
    };
    lines.push(kv("ports", &ports));
    lines.push(kv("env", &r.env_len.to_string()));
    if let Some(cmd) = &r.command {
        lines.push(kv("command", cmd));
    }
    if !r.config_hash.is_empty() {
        lines.push(kv("config hash", &r.config_hash));
    }
    if r.drift {
        lines.push(Line::from(Span::styled(
            "drift: config or dockerfile changed since the container was created (:rebuild)",
            Style::default().fg(Color::Yellow),
        )));
    }

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_ports(frame: &mut Frame, app: &App, area: Rect) {
    let rows: &[PortRow] = &app.ports;

    let (top, detail_area, terms_area) = content_areas(app, area);
    // The Ports tab has no snapshot error line; keep the top part whole.
    let panel_focused = app.focus == Focus::Terminal;

    if rows.is_empty() {
        draw_ports_empty(frame, top, panel_focused);
    } else {
        draw_ports_table(frame, app, rows, top, panel_focused);
    }
    // No per-row detail for forwards; a plain block keeps the layout aligned with
    // the other tabs (and reserves the terminal-panel geometry).
    let detail = Block::default()
        .borders(Borders::ALL)
        .border_style(dash_border_style(panel_focused))
        .title("Detail");
    frame.render_widget(Paragraph::new("").block(detail), detail_area);
    if let Some(terms_area) = terms_area {
        draw_terminal_panel(frame, app, terms_area);
    }
}

fn draw_ports_empty(frame: &mut Frame, area: Rect, term_focused: bool) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(dash_border_style(term_focused))
        .title(Tab::Ports.title());
    let text = Line::from(Span::styled(
        "no forwards — press p on an instance or service, or :port <instance> <port>",
        Style::default().add_modifier(Modifier::DIM),
    ))
    .alignment(Alignment::Center);
    frame.render_widget(Paragraph::new(text).block(block), area);
}

fn draw_ports_table(
    frame: &mut Frame,
    app: &App,
    rows: &[PortRow],
    area: Rect,
    term_focused: bool,
) {
    let header = Row::new(
        ["LOCAL", "TARGET", "PROCESS", "STATE", "CONNS"]
            .into_iter()
            .map(Cell::from),
    )
    .style(Style::default().add_modifier(Modifier::DIM));

    let table_rows: Vec<Row> = rows.iter().map(port_row).collect();

    let widths = [
        Constraint::Length(20),
        Constraint::Min(24),
        Constraint::Length(18),
        Constraint::Length(14),
        Constraint::Length(6),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(dash_border_style(term_focused))
        .title(Tab::Ports.title());

    let table = Table::new(table_rows, widths)
        .header(header)
        .block(block)
        .column_spacing(1)
        .row_highlight_style(Style::default().fg(SELECTION).add_modifier(Modifier::BOLD));

    let mut state = TableState::default().with_selected(Some(app.selected()));
    frame.render_stateful_widget(table, area, &mut state);
}

fn port_row(r: &PortRow) -> Row<'_> {
    let process = r.process.clone().unwrap_or_else(|| "-".to_string());
    let mut target = vec![Span::raw(r.target.clone())];
    if r.configured {
        target.push(Span::styled(" (config)", Style::default().add_modifier(Modifier::DIM)));
    }
    Row::new(vec![
        Cell::from(r.local.clone()),
        Cell::from(Line::from(target)),
        Cell::from(process),
        Cell::from(Span::styled(r.state.clone(), port_state_style(&r.state))),
        Cell::from(r.conns.to_string()),
    ])
}

/// Color the STATE column like the siblings: active green, connecting yellow,
/// error red; anything else uncolored.
fn port_state_style(state: &str) -> Style {
    if state == "active" {
        Style::default().fg(Color::Green)
    } else if state == "connecting" {
        Style::default().fg(Color::Yellow)
    } else if state.starts_with("error") {
        Style::default().fg(Color::Red)
    } else {
        Style::default()
    }
}

fn draw_inbox(frame: &mut Frame, app: &App, area: Rect) {
    let (top, detail_area, terms_area) = content_areas(app, area);
    // The Inbox has no Detail box (the thread pane is its detail): without
    // terminals the split takes the whole content area; with them, the top,
    // the terminal panel staying where it is on every tab.
    let region = if terms_area.is_some() { top } else { top.union(detail_area) };
    let (list_area, thread_area) = inbox_areas(region, app.inbox.split_pct);
    draw_inbox_list(frame, app, list_area);
    draw_inbox_pane(frame, app, thread_area);
    if let Some(terms_area) = terms_area {
        draw_terminal_panel(frame, app, terms_area);
    }
}

/// The Inbox's `(list, thread)` split of `area`, the list taking `split_pct`
/// percent of the width. The one place the split is computed, so a mouse
/// hit-test on the divider can't drift from what was drawn.
fn inbox_areas(area: Rect, split_pct: u16) -> (Rect, Rect) {
    let [list, thread] =
        Layout::horizontal([Constraint::Percentage(split_pct), Constraint::Min(0)]).areas(area);
    (list, thread)
}

/// Border of an Inbox zone: accented while it has the keys, dim otherwise
/// (also while a terminal has them), like the config modal's panes.
fn zone_border_style(app: &App, zone: InboxFocus) -> Style {
    if app.focus == Focus::Dashboard && app.inbox.focus == zone {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::DIM)
    }
}

/// The view switcher, as the list's title: every view with its count, the
/// current one highlighted.
fn inbox_view_header(app: &App) -> Line<'static> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let mut spans = vec![Span::raw(" ")];
    for (i, view) in View::ALL.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", dim));
        }
        let style = if *view == app.inbox.view {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            dim
        };
        spans.push(Span::styled(format!("{} ({})", view.title(), app.inbox.count(*view)), style));
    }
    spans.push(Span::styled("  ←/→ ", dim));
    Line::from(spans)
}

/// Narrowest list (inner width) that still fits the FROM column.
const INBOX_FROM_MIN_WIDTH: u16 = 60;

fn draw_inbox_list(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(zone_border_style(app, InboxFocus::List))
        .title(inbox_view_header(app));
    let from = block.inner(area).width >= INBOX_FROM_MIN_WIDTH;
    let rows = app.inbox.rows();
    if rows.is_empty() {
        let text = if app.inbox.threads().is_empty() {
            "no notifications — containers send them with `devsbd notify \"…\"`".to_string()
        } else {
            format!("nothing in {} — ←/→ switch views", app.inbox.view.title())
        };
        let text = Line::from(Span::styled(text, Style::default().add_modifier(Modifier::DIM)))
            .alignment(Alignment::Center);
        frame.render_widget(Paragraph::new(text).block(block), area);
        return;
    }
    let mut header = vec!["", "FROM", "TITLE", "STATUS", "AGE"];
    // AGE: a week-old date ("Oct 12 14:32") is the widest value (see `when`).
    let mut widths = vec![
        Constraint::Length(1),
        Constraint::Length(12),
        Constraint::Min(16),
        Constraint::Length(12),
        Constraint::Length(12),
    ];
    if !from {
        header.remove(1);
        widths.remove(1);
    }
    let header =
        Row::new(header.into_iter().map(Cell::from)).style(Style::default().add_modifier(Modifier::DIM));
    // Wall clock read per frame, so relative times tick between snapshots.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let threads = app.inbox.threads();
    let rows: Vec<Row> = rows.iter().map(|&i| inbox_row(&threads[i], from, now, app.utc_offset)).collect();
    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .column_spacing(1)
        .row_highlight_style(Style::default().fg(SELECTION).add_modifier(Modifier::BOLD));
    let mut state = TableState::default().with_selected(Some(app.selected()));
    frame.render_stateful_widget(table, area, &mut state);
}

fn level_style(level: Level) -> Style {
    match level {
        Level::Info => Style::default().add_modifier(Modifier::DIM),
        Level::Warn => Style::default().fg(Color::Yellow),
        Level::Error => Style::default().fg(Color::Red),
    }
}

fn state_style(state: State) -> Style {
    match state {
        State::NeedsYou => Style::default().fg(Color::Yellow),
        State::Active => Style::default(),
        State::Done => Style::default().add_modifier(Modifier::DIM),
    }
}

/// The one-cell state marker: a dispatcher thread's state, or a notify
/// record's level.
fn thread_marker(t: &Thread) -> (&'static str, Style) {
    match (t.kind, t.state, t.head().map(|r| r.level)) {
        (Kind::Thread, Some(State::NeedsYou), _) => ("●", state_style(State::NeedsYou)),
        (Kind::Thread, Some(State::Done), _) => ("✓", state_style(State::Done)),
        (Kind::Thread, _, _) => ("○", state_style(State::Active)),
        (Kind::Notify, _, Some(Level::Error)) => ("✖", level_style(Level::Error)),
        (Kind::Notify, _, Some(Level::Warn)) => ("▲", level_style(Level::Warn)),
        (Kind::Notify, _, _) => ("·", level_style(Level::Info)),
    }
}

/// One Inbox row: marker, sender (when `from`), title (`↗` when there's a
/// link), status chip (a notify record's level when it's above info), age.
/// Bold while unread; dim when archived (its instance is gone).
fn inbox_row<'a>(t: &Thread, from: bool, now: u64, utc_offset: i64) -> Row<'a> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let (marker, marker_style) = thread_marker(t);
    let mut title = vec![Span::raw(title_of(t))];
    if t.link.is_some() || t.head().is_some_and(|r| r.link.is_some()) {
        title.push(Span::styled(" ↗", Style::default().fg(Color::Blue)));
    }
    if t.archived {
        title.push(Span::styled(" (archived)", dim));
    }
    let status = match (t.kind, t.head()) {
        (Kind::Thread, _) => Span::styled(t.status.clone().unwrap_or_default(), dim),
        (Kind::Notify, Some(r)) if r.level != Level::Info => {
            Span::styled(r.level.as_str(), level_style(r.level))
        }
        (Kind::Notify, _) => Span::raw(""),
    };
    let mut cells = vec![
        Cell::from(Span::styled(marker, marker_style)),
        Cell::from(Line::from(title)),
        Cell::from(status),
        Cell::from(Span::styled(when(t.changed_at(), now, utc_offset), dim)),
    ];
    if from {
        cells.insert(1, Cell::from(t.owner_name.clone()));
    }
    let row = Row::new(cells);
    if t.archived {
        row.style(dim)
    } else if t.unread {
        row.style(Style::default().add_modifier(Modifier::BOLD))
    } else {
        row
    }
}

fn tone_style(tone: Tone) -> Style {
    match tone {
        Tone::Plain => Style::default(),
        Tone::Dim => Style::default().add_modifier(Modifier::DIM),
        Tone::Bold => Style::default().add_modifier(Modifier::BOLD),
        Tone::Link => Style::default().fg(Color::Blue),
        Tone::State(s) => state_style(s),
        Tone::Level(l) => level_style(l),
    }
}

/// Hard-wrap one pane line at `width` columns, so the pane knows exactly how
/// many rows it renders and can bound its scroll. By char, not display width:
/// the pane is mostly ASCII, and a wide glyph only costs a clipped cell.
fn wrap_pane_line(line: &PaneLine, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for (tone, text) in line {
        let style = tone_style(*tone);
        let mut buf = String::new();
        for ch in text.chars() {
            if used == width {
                if !buf.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut buf), style));
                }
                out.push(Line::from(std::mem::take(&mut spans)));
                used = 0;
            }
            buf.push(ch);
            used += 1;
        }
        if !buf.is_empty() {
            spans.push(Span::styled(buf, style));
        }
    }
    out.push(Line::from(spans));
    out
}

/// The thread pane: the selected thread's content, then its input (a thread
/// taking replies), a one-line hint (one that doesn't) or nothing (a notify
/// thread, which can't be replied to). Records the content's scroll bound for
/// the scroll keys (`InboxView::set_pane_max`), since only here are the size
/// and wrapping known.
fn draw_inbox_pane(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(zone_border_style(app, InboxFocus::Thread))
        .title(" Thread ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let dim = Style::default().add_modifier(Modifier::DIM);
    let Some(t) = app.selected_inbox_thread() else {
        let text = Line::from(Span::styled("no thread selected", dim)).alignment(Alignment::Center);
        frame.render_widget(Paragraph::new(text), inner);
        return;
    };
    let bottom = match (t.kind, &t.reply) {
        (_, Some(_)) => 3,
        (Kind::Thread, None) => 1,
        (Kind::Notify, None) => 0,
    };
    let [content, bottom_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(bottom)]).areas(inner);
    let child = app.thread_child(t);
    let lines: Vec<Line> = pane_lines(t, child.as_ref(), app.utc_offset)
        .iter()
        .flat_map(|l| wrap_pane_line(l, content.width as usize))
        .collect();
    let rows = lines.len().min(u16::MAX as usize) as u16;
    let max = rows.saturating_sub(content.height);
    app.inbox.set_pane_max(max);
    let scroll = app.inbox.scroll.min(max);
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), content);
    match &t.reply {
        Some(reply) => draw_reply_input(frame, app, reply.placeholder.as_deref(), bottom_area),
        None if bottom > 0 => {
            frame.render_widget(Paragraph::new(Span::styled("this thread takes no replies", dim)), bottom_area);
        }
        None => {}
    }
}

/// The thread pane's reply input: a rounded box holding the line being typed,
/// or the thread's placeholder dim while it's empty. The caret is placed (like
/// the `:` prompt's) only while the input has focus, so it doesn't blink in
/// a box the keys don't reach.
fn draw_reply_input(frame: &mut Frame, app: &App, placeholder: Option<&str>, area: Rect) {
    let reply = app.inbox.reply.as_ref().filter(|_| app.inbox.focus == InboxFocus::Input);
    let title = if reply.is_some() { " enter sends · esc back " } else { " r reply " };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(zone_border_style(app, InboxFocus::Input))
        .title(title);
    let inner = block.inner(area);
    let input = reply.map_or("", |r| r.line.input());
    let line = if input.is_empty() {
        let hint = placeholder.unwrap_or("reply to the dispatcher");
        Line::from(Span::styled(hint.to_string(), Style::default().add_modifier(Modifier::DIM)))
    } else {
        Line::from(input.to_string())
    };
    frame.render_widget(Paragraph::new(line).block(block), area);
    if let Some(reply) = reply {
        if app.focus == Focus::Dashboard && inner.width > 0 && inner.height > 0 {
            let col = inner.x + reply.line.cursor() as u16;
            frame.set_cursor_position((col.min(inner.right().saturating_sub(1)), inner.y));
        }
    }
}

/// Pure tab labels for the terminal panel, one per open session: `{i+1}:{title}`
/// with a ` (exited)` suffix on dead tabs. The single source of truth for both
/// the rendered strip ([`terminal_tab_specs`]) and the mouse hit-test
/// ([`terminal_tab_hit`]), so a click can't land on a column the strip doesn't
/// actually draw.
pub(crate) fn terminal_tab_labels(app: &App) -> Vec<String> {
    app.terms
        .sessions()
        .iter()
        .enumerate()
        .map(|(i, s)| {
            if s.exited() {
                format!("{}:{} (exited)", i + 1, s.title)
            } else {
                format!("{}:{}", i + 1, s.title)
            }
        })
        .collect()
}

/// Pure tab-strip model for the terminal panel: one entry per open session as
/// `(label, active, exited)`. Kept free of styling so it's unit-testable; the
/// styling lives in [`terminal_tab_line`].
fn terminal_tab_specs(app: &App) -> Vec<(String, bool, bool)> {
    let active = app.terms.active();
    terminal_tab_labels(app)
        .into_iter()
        .zip(app.terms.sessions())
        .enumerate()
        .map(|(i, (label, s))| (label, i == active, s.exited()))
        .collect()
}

/// Build the terminal panel's block title as a styled tab strip. The active
/// (live) tab is ACCENT+BOLD, inactive live tabs DIM, exited tabs always DIM.
/// When the terminal holds focus, `▶ ` is prepended to match `pane_block`'s
/// focus mark. Tabs are separated by two spaces, matching the plan's example.
fn terminal_tab_line(app: &App) -> Line<'static> {
    let mut spans: Vec<Span> = Vec::new();
    if app.focus == Focus::Terminal {
        // `▶ ` is FOCUS_MARK_WIDTH columns; keep the string and constant in sync.
        spans.push(Span::styled(
            "▶ ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    }
    for (i, (label, active, exited)) in terminal_tab_specs(app).into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        let style = if active && !exited {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        spans.push(Span::styled(label, style));
    }
    Line::from(spans)
}

/// The integrated-terminal panel: a bordered block whose title is the tab strip,
/// showing the active session's vt100 screen. Border is ACCENT+BOLD when focused,
/// DIM otherwise. Assumes at least one terminal is open (only called then).
fn draw_terminal_panel(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Terminal;
    let border_style = if focused {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::DIM)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(terminal_tab_line(app));

    let Some(session) = app.terms.active_session() else {
        frame.render_widget(block, area);
        return;
    };

    // Lock the active screen to render it. A poisoned lock (a reader thread
    // panicked) is unrecoverable here — skip the body but still show the block.
    let Ok(parser) = session.parser().lock() else {
        frame.render_widget(block, area);
        return;
    };
    let screen = parser.screen();

    // Show the cursor only when the terminal is focused; hide it otherwise so a
    // background terminal doesn't compete with the dashboard's own cursor.
    let mut cursor = Cursor::default();
    if !focused {
        cursor.hide();
    }
    let widget = PseudoTerminal::new(screen).block(block).cursor(cursor);
    frame.render_widget(&widget, area);
}

fn draw_instances(frame: &mut Frame, app: &App, area: Rect) {
    let snapshot = app.snapshot.as_ref();
    let nodes = app.visible_nodes();

    let (top, detail_area, terms_area) = content_areas(app, area);
    // Optional error line reserved above the table, carved out of the top part
    // (never the bottom) so the terminal panel geometry is snapshot-independent.
    let error = snapshot.and_then(|s| s.error.as_deref());
    let [err_area, table_area] = Layout::vertical([
        Constraint::Length(if error.is_some() { 1 } else { 0 }),
        Constraint::Min(0),
    ])
    .areas(top);

    if let Some(err) = error {
        let line = Line::from(Span::styled(
            err.to_string(),
            Style::default().fg(Color::Red),
        ));
        frame.render_widget(Paragraph::new(line), err_area);
    }

    // Terminal focused ⇒ the terminal panel is the accented one; dim the rest.
    let panel_focused = app.focus == Focus::Terminal;

    if nodes.is_empty() {
        // No snapshot yet means the first collection is still running, not
        // that the config is empty.
        let message = if snapshot.is_some() {
            "no sandboxes defined — check devsandboxes.toml"
        } else {
            "loading…"
        };
        draw_empty(frame, table_area, panel_focused, message);
        draw_detail(frame, snapshot, None, None, detail_area, panel_focused);
    } else {
        let snapshot = snapshot.expect("non-empty nodes imply a snapshot");
        draw_tree(frame, app, snapshot, &nodes, table_area, panel_focused);
        let selected = nodes.get(app.selected()).copied();
        // Agent count for the selected instance (or a proc row's parent), read
        // from the proc cache; `None` until its forest is fetched.
        let agent_count = match selected {
            Some(Node::Instance(i)) | Some(Node::Proc { instance: i, .. }) => {
                snapshot.instances.get(i).and_then(|r| app.agent_count(&r.name))
            }
            _ => None,
        };
        draw_detail(
            frame,
            Some(snapshot),
            selected,
            agent_count,
            detail_area,
            panel_focused,
        );
    }
    if let Some(terms_area) = terms_area {
        draw_terminal_panel(frame, app, terms_area);
    }
}

fn draw_empty(frame: &mut Frame, area: Rect, term_focused: bool, message: &str) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(dash_border_style(term_focused))
        .title(Tab::Instances.title());
    let text = Line::from(Span::styled(
        message,
        Style::default().add_modifier(Modifier::DIM),
    ))
    .alignment(Alignment::Center);
    let paragraph = Paragraph::new(text).block(block);
    frame.render_widget(paragraph, area);
}

fn draw_tree(
    frame: &mut Frame,
    app: &App,
    snapshot: &Snapshot,
    nodes: &[Node],
    area: Rect,
    term_focused: bool,
) {
    // TYPE only earns its width when some sandbox is a dispatcher.
    let show_type = snapshot.sandboxes.iter().any(|s| s.dispatcher);
    let mut titles = vec!["TREE", "STATUS", "UPTIME", "CPU", "MEM", "FOLDER", "SERVICES"];
    let mut widths = vec![
        Constraint::Min(20),
        Constraint::Length(9),
        Constraint::Length(7),
        Constraint::Length(7),
        Constraint::Length(16),
        Constraint::Min(20),
        Constraint::Min(12),
    ];
    if show_type {
        // SERVICES hugs its longest value and a trailing spacer takes the slack,
        // so TYPE sits next to the services instead of at the far right edge.
        let services = snapshot
            .instances
            .iter()
            .map(|r| r.services.join(",").chars().count())
            .chain([titles[6].len()])
            .max()
            .unwrap_or_default();
        widths[6] = Constraint::Length(services as u16);
        titles.extend(["TYPE", ""]);
        widths.extend([Constraint::Length(10), Constraint::Min(0)]);
    }
    let header = Row::new(titles.into_iter().map(Cell::from))
        .style(Style::default().add_modifier(Modifier::DIM));

    let table_rows: Vec<Row> = nodes
        .iter()
        .map(|node| tree_row(app, snapshot, *node, show_type))
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(dash_border_style(term_focused))
        .title(Tab::Instances.title());

    let table = Table::new(table_rows, widths)
        .header(header)
        .block(block)
        .column_spacing(1)
        .row_highlight_style(
            Style::default().fg(SELECTION).add_modifier(Modifier::BOLD),
        );

    let mut state = TableState::default().with_selected(Some(app.selected()));
    frame.render_stateful_widget(table, area, &mut state);
}

/// Render one tree node into a table row. Sandbox / orphan-group rows carry their
/// stats in the STATUS column and leave the instance columns blank; instance
/// rows fill the columns and indent the TREE cell. `show_type`: the TYPE column
/// is present (some sandbox is a dispatcher).
fn tree_row<'a>(app: &App, snapshot: &'a Snapshot, node: Node, show_type: bool) -> Row<'a> {
    match node {
        Node::Sandbox(i) => match snapshot.sandboxes.get(i) {
            Some(sb) => sandbox_tree_row(app, snapshot, sb),
            None => Row::new(vec![Cell::from("")]),
        },
        Node::Instance(i) => match snapshot.instances.get(i) {
            Some(inst) => instance_tree_row(
                inst,
                app.inbox.needs_you_for(&inst.instance_id),
                super::data::dispatcher_label(inst, &snapshot.instances),
                show_type.then(|| {
                    let dispatcher = snapshot
                        .sandboxes
                        .iter()
                        .any(|s| s.name == inst.sandbox && s.dispatcher);
                    if dispatcher { "dispatcher" } else { "-" }
                }),
            ),
            None => Row::new(vec![Cell::from("")]),
        },
        Node::Empty(i) => {
            let name = snapshot.sandboxes.get(i).map_or("", |s| s.name.as_str());
            Row::new(vec![Cell::from(Span::styled(
                format!("    no instances — : run {name}"),
                Style::default().add_modifier(Modifier::DIM),
            ))])
        }
        Node::Proc { instance, row } => proc_tree_row(app, snapshot, instance, row),
        Node::Orphans => {
            let count = snapshot
                .instances
                .iter()
                .filter(|inst| !snapshot.sandboxes.iter().any(|s| s.name == inst.sandbox))
                .count();
            let marker = if app.is_collapsed_group(super::data::ORPHANS_NAME) {
                '▸'
            } else {
                '▾'
            };
            Row::new(vec![
                Cell::from(Span::styled(
                    format!("{marker} (not in config)"),
                    Style::default().add_modifier(Modifier::DIM),
                )),
                Cell::from(Span::styled(
                    format!("{count} orphan"),
                    Style::default().add_modifier(Modifier::DIM),
                )),
            ])
        }
    }
}

fn sandbox_tree_row<'a>(app: &App, snapshot: &Snapshot, sb: &'a SandboxRow) -> Row<'a> {
    let collapsed = app.is_collapsed_group(&sb.name);
    let marker = if collapsed { '▸' } else { '▾' };
    let total = snapshot.instances.iter().filter(|r| r.sandbox == sb.name).count();
    let running = snapshot
        .instances
        .iter()
        .filter(|r| r.sandbox == sb.name && matches!(r.status, ContainerStatus::Running(_)))
        .count();
    let stats = sandbox_stats(total, running);
    // Green when any running, dim when the sandbox has no instances at all.
    let stats_style = if running > 0 {
        Style::default().fg(Color::Green)
    } else if total == 0 {
        Style::default().add_modifier(Modifier::DIM)
    } else {
        Style::default()
    };
    Row::new(vec![
        Cell::from(Span::styled(
            format!("{marker} {}", sb.name),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Cell::from(Span::styled(stats, stats_style)),
        Cell::from(""),
        Cell::from(""),
        Cell::from(""),
        Cell::from(source_folder_cell(sb)),
        Cell::from(""),
    ])
}

/// Combined source + folder for the sandbox row's FOLDER column: source colored
/// like `ls`, then a dim folder when set.
fn source_folder_cell(sb: &SandboxRow) -> Line<'static> {
    let mut spans = vec![Span::styled(sb.source.clone(), source_style(&sb.source))];
    if let Some(folder) = &sb.folder {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            folder.clone(),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    Line::from(spans)
}

/// `unread`: the instance's Inbox threads waiting on the user (needs-you
/// threads + unread notify records), shown as a yellow `✉N` after the name. `owner`: a dispatcher child's dim
/// `⇠ <dispatcher>` suffix (`data::dispatcher_label`). `kind`: the TYPE cell,
/// `None` when that column is hidden.
fn instance_tree_row(
    r: &InstanceRow,
    unread: usize,
    owner: Option<String>,
    kind: Option<&'static str>,
) -> Row<'static> {
    let status = Cell::from(Span::styled(
        r.status.label().to_string(),
        status_style(&r.status),
    ));
    let folder = if r.worktree {
        format!("⎇ {}", r.folder)
    } else {
        r.folder.clone()
    };
    let services = if r.services.is_empty() {
        "-".to_string()
    } else {
        r.services.join(",")
    };
    let mut name = vec![Span::raw(format!("  {}", r.name))];
    if unread > 0 {
        name.push(Span::styled(format!(" ✉{unread}"), Style::default().fg(Color::Yellow)));
    }
    if let Some(owner) = owner {
        name.push(Span::styled(format!(" {owner}"), Style::default().add_modifier(Modifier::DIM)));
    }
    if r.done {
        name.push(Span::raw(" ✓"));
    }
    let name = Cell::from(Line::from(name));
    let mut cells = vec![
        name,
        status,
        Cell::from(humanize_secs(r.uptime_secs)),
        Cell::from(r.cpu.clone().unwrap_or_else(|| "-".to_string())),
        Cell::from(r.mem.clone().unwrap_or_else(|| "-".to_string())),
        Cell::from(folder),
        Cell::from(services),
    ];
    cells.extend(kind.map(Cell::from));
    // Done: kept as is until removed, so it stays listed but recedes.
    let style = if r.done { Style::default().add_modifier(Modifier::DIM) } else { Style::default() };
    Row::new(cells).style(style)
}

/// Render a process row (or its placeholder) for the instance at `instance`.
/// Dim throughout. Process rows put the PID in the TREE gutter (indented deeper
/// than the instance row) and the `\_`-indented args in the FOLDER column; the
/// status/uptime/cpu/mem columns stay empty. A [`ProcState::Message`] (and the
/// `MESSAGE_ROW` placeholder) renders as one dim message in the TREE column.
fn proc_tree_row<'a>(app: &App, snapshot: &'a Snapshot, instance: usize, row: usize) -> Row<'a> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let name = snapshot.instances.get(instance).map(|r| r.name.as_str());
    let state = name.and_then(|n| app.procs.get(n));

    // A real process row: only when we have a Rows state and a valid index.
    if row != MESSAGE_ROW {
        if let Some(ProcState::Rows { rows, .. }) = state {
            if let Some(p) = rows.get(row) {
                let pid = Cell::from(Span::styled(format!("      {}", p.pid), dim));
                let indent = "\\_ ".repeat(p.depth);
                // Coding-agent processes are highlighted blue+bold; the tree
                // connectors stay dim so the agent name itself is what pops.
                let args = if is_agent(&p.args) {
                    Cell::from(Line::from(vec![
                        Span::styled(indent, dim),
                        Span::styled(
                            p.args.clone(),
                            Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD),
                        ),
                    ]))
                } else {
                    Cell::from(Span::styled(format!("{indent}{}", p.args), dim))
                };
                return Row::new(vec![
                    pid,
                    Cell::from(""),
                    Cell::from(""),
                    Cell::from(""),
                    Cell::from(""),
                    args,
                    Cell::from(""),
                ]);
            }
        }
    }

    // Placeholder / message row.
    let msg = match state {
        Some(ProcState::Message(m)) => m.clone(),
        _ => "(loading…)".to_string(),
    };
    Row::new(vec![Cell::from(Span::styled(format!("      {msg}"), dim))])
}

fn status_style(status: &ContainerStatus) -> Style {
    match status {
        ContainerStatus::Running(_) => Style::default().fg(Color::Green),
        ContainerStatus::Exited(_) => Style::default().fg(Color::Red),
        ContainerStatus::Missing | ContainerStatus::Unknown => {
            Style::default().add_modifier(Modifier::DIM)
        }
    }
}

/// Detail panel for the selected tree node: sandbox summary for sandbox / empty
/// nodes, the existing instance detail for instances, and a hint for the orphan
/// group. Empty when nothing is selected.
fn draw_detail(
    frame: &mut Frame,
    snapshot: Option<&Snapshot>,
    node: Option<Node>,
    agent_count: Option<usize>,
    area: Rect,
    term_focused: bool,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(dash_border_style(term_focused))
        .title("Detail");

    let lines: Vec<Line> = match (snapshot, node) {
        (Some(s), Some(Node::Sandbox(i))) | (Some(s), Some(Node::Empty(i))) => {
            match s.sandboxes.get(i) {
                Some(sb) => sandbox_detail(s, sb),
                None => Vec::new(),
            }
        }
        (Some(s), Some(Node::Instance(i)))
        | (Some(s), Some(Node::Proc { instance: i, .. })) => match s.instances.get(i) {
            Some(r) => instance_detail(r, agent_count),
            None => Vec::new(),
        },
        (_, Some(Node::Orphans)) => vec![Line::from(Span::styled(
            "instances whose sandbox is no longer in devsandboxes.toml",
            Style::default().add_modifier(Modifier::DIM),
        ))],
        _ => Vec::new(),
    };

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Sandbox summary: source, folder, services, extends chain, config hash, and
/// instance counts.
fn sandbox_detail<'a>(snapshot: &Snapshot, sb: &'a SandboxRow) -> Vec<Line<'a>> {
    let total = snapshot.instances.iter().filter(|r| r.sandbox == sb.name).count();
    let running = snapshot
        .instances
        .iter()
        .filter(|r| r.sandbox == sb.name && matches!(r.status, ContainerStatus::Running(_)))
        .count();
    let services = if sb.services.is_empty() {
        "-".to_string()
    } else {
        sb.services.join(", ")
    };
    let extends = if sb.extends.is_empty() {
        "-".to_string()
    } else {
        sb.extends.join(" → ")
    };
    let mut lines = vec![
        kv("source", &sb.source),
        kv("folder", sb.folder.as_deref().unwrap_or("-")),
        kv("services", &services),
        kv("extends", &extends),
    ];
    if !sb.config_hash.is_empty() {
        lines.push(kv("config hash", &sb.config_hash));
    }
    lines.push(kv("instances", &format!("{total} ({running} running)")));
    for issue in &sb.issues {
        lines.push(Line::from(Span::styled(
            format!("⚠ {issue}"),
            Style::default().fg(Color::Red),
        )));
    }
    lines
}

fn instance_detail(r: &InstanceRow, agent_count: Option<usize>) -> Vec<Line<'_>> {
    let mut lines: Vec<Line> = vec![
        kv("container", &r.container),
        kv("workspace", &r.workspace),
        kv("remoteUser", r.remote_user.as_deref().unwrap_or("-")),
        kv("remoteEnv", &r.remote_env_len.to_string()),
        agents_line(r, agent_count),
    ];
    if r.worktree {
        lines.push(kv2("base", &r.base_folder, "worktree", &r.folder));
    } else {
        lines.push(kv("base folder", &r.base_folder));
    }
    if r.drift {
        lines.push(Line::from(Span::styled(
            "drift: config or dockerfile changed since the container was created",
            Style::default().fg(Color::Yellow),
        )));
    }
    lines
}

/// The `agents:` Detail line for an instance. A stopped/missing container shows
/// a dim `-` (nothing to count); a running one shows the count — blue+bold when
/// any agents are present, dim `0`, or a dim `…` while its forest is still being
/// fetched.
fn agents_line(r: &InstanceRow, agent_count: Option<usize>) -> Line<'static> {
    let (text, style) = if !matches!(r.status, ContainerStatus::Running(_)) {
        ("-".to_string(), Style::default().add_modifier(Modifier::DIM))
    } else {
        match agent_count {
            Some(0) => ("0".to_string(), Style::default().add_modifier(Modifier::DIM)),
            Some(n) => (
                n.to_string(),
                Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD),
            ),
            None => ("…".to_string(), Style::default().add_modifier(Modifier::DIM)),
        }
    };
    Line::from(vec![
        Span::styled("agents: ", Style::default().add_modifier(Modifier::DIM)),
        Span::styled(text, style),
    ])
}

/// `key: value` with a dim key.
fn kv<'a>(key: &'a str, value: &str) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{key}: "), Style::default().add_modifier(Modifier::DIM)),
        Span::raw(value.to_string()),
    ])
}

/// Two `key: value` pairs on one line (base folder vs worktree).
fn kv2<'a>(k1: &'a str, v1: &str, k2: &'a str, v2: &str) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{k1}: "), Style::default().add_modifier(Modifier::DIM)),
        Span::raw(v1.to_string()),
        Span::raw("   "),
        Span::styled(format!("{k2}: "), Style::default().add_modifier(Modifier::DIM)),
        Span::raw(v2.to_string()),
    ])
}

fn draw_help(frame: &mut Frame, app: &App, area: Rect) {
    // A status line (e.g. `code` outcome) preempts the help hint until the
    // next key clears it.
    if let Some(status) = &app.status {
        let line = Line::from(Span::styled(
            status.clone(),
            Style::default().fg(Color::Yellow),
        ));
        frame.render_widget(Paragraph::new(line), area);
        return;
    }
    // Terminal focus (no modal) has its own hints, independent of the tab.
    if app.focus == Focus::Terminal && matches!(app.modal, Modal::None) {
        let exited = app.terms.active_session().is_some_and(|s| s.exited());
        let text = if exited {
            "terminal exited — ctrl-]/F12 back · x closes"
        } else {
            "ctrl-] / F12 back to dashboard · all other keys go to the shell"
        };
        let help = Line::from(text).style(Style::default().add_modifier(Modifier::DIM));
        frame.render_widget(Paragraph::new(help), area);
        return;
    }

    let base = match app.modal {
        Modal::Config(_) => "t toggle · tab pane · <> resize · ↑↓ scroll · esc close".to_string(),
        Modal::Help(_) => "↑↓ scroll · pgup/pgdn · g/G · esc/? close".to_string(),
        Modal::Logs(_) => "↑↓ scroll · pgup/pgdn · g/G · esc close".to_string(),
        Modal::None => match app.tab {
            // A process row acts only on itself: signals, nothing forwarded to
            // the parent instance. Signals need container-namespace pids, so a
            // `top` fallback listing (or a placeholder row) drops them.
            Tab::Instances if app.on_proc_row() => {
                if app.proc_row_signalable() {
                    "q quit · tab switch · ↑↓ select · ← parent · t SIGTERM · K SIGKILL · : cmd · ? help"
                        .to_string()
                } else {
                    "q quit · tab switch · ↑↓ select · ← parent · : cmd · ? help".to_string()
                }
            }
            // `r` and `s` mirror what the key would do to the selection:
            // run vs rename, stop vs start.
            Tab::Instances => format!(
                "q quit · tab switch · ↑↓ select · ←→ fold · enter config · r {} · o vscode · s {} · d/u done · l logs · p forward · t term · : cmd · ? help",
                app.run_rename_hint(),
                app.stop_start_hint()
            ),
            Tab::Services => {
                "q quit · tab switch · ↑↓ select · enter config · t term · p forward · : cmd · ? help"
                    .to_string()
            }
            Tab::Ports => {
                "q quit · tab switch · ↑↓ select · d stop forward · : port … · ? help".to_string()
            }
            Tab::Inbox => match app.inbox.focus {
                InboxFocus::List => {
                    "q quit · tab switch · ↑↓ select · ←→ view · enter thread · r reply · d dismiss/done · u reopen · D clear notify · o vscode · t term · l logs · p forward · ? help"
                        .to_string()
                }
                InboxFocus::Thread => {
                    "esc list · ↑↓ scroll · enter open link · 1-9 actions · r reply · d done · u reopen · o vscode · t term · l logs · p forward · q quit · ? help".to_string()
                }
                InboxFocus::Input => {
                    "enter send · esc back to thread · ←→ home end edit · ctrl-u clear · ctrl-w delete word"
                        .to_string()
                }
            },
        },
    };
    // On the dashboard with terminals open, append the terminal-cycle hints.
    // Not over a focused Inbox thread or input: they shadow those keys.
    let shadowed = app.tab == Tab::Inbox && app.inbox.focus != InboxFocus::List;
    let text = if matches!(app.modal, Modal::None) && !app.terms.is_empty() && !shadowed {
        format!("{base} · [/] terms · ctrl-] focus term · x close term")
    } else {
        base
    };
    let help = Line::from(text).style(Style::default().add_modifier(Modifier::DIM));
    frame.render_widget(Paragraph::new(help), area);
}

/// Render the command prompt in the bottom bar: `: <input>` on the first line
/// with the caret placed via `set_cursor_position`, and a second hint line for
/// completion candidates or a red parse error.
fn draw_prompt(frame: &mut Frame, prompt: &Prompt, area: Rect) {
    let [input_area, hint_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    let prefix = ": ";
    let line = Line::from(vec![
        Span::styled(prefix, Style::default().fg(ACCENT)),
        Span::raw(prompt.input().to_string()),
    ]);
    frame.render_widget(Paragraph::new(line), input_area);

    // Caret: prefix width + cursor char offset, clamped to the area.
    let col = input_area.x + prefix.len() as u16 + prompt.cursor() as u16;
    frame.set_cursor_position((col.min(input_area.right().saturating_sub(1)), input_area.y));

    // Hint line: parse error (red) takes precedence over completion candidates.
    let hint = if let Some(err) = &prompt.error {
        Line::from(Span::styled(err.clone(), Style::default().fg(Color::Red)))
    } else {
        prompt_candidates_line(prompt)
    };
    frame.render_widget(Paragraph::new(hint), hint_area);
}

/// Completion-candidate hint: candidates space-joined, the active one bold cyan.
/// Empty line when no cycle is in flight.
fn prompt_candidates_line(prompt: &Prompt) -> Line<'static> {
    let candidates = prompt.candidates();
    if candidates.is_empty() {
        return Line::from(String::new());
    }
    let active = prompt.cycle_index();
    let mut spans: Vec<Span> = Vec::new();
    for (i, cand) in candidates.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        let style = if i == active {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        spans.push(Span::styled(cand.clone(), style));
    }
    Line::from(spans)
}

/// Render the config explorer full-screen over the dashboard: config (TOML) on
/// the left, `docker inspect` (JSON) on the right, split at `split_pct` with the
/// two panes' shared border acting as the divider. The focused pane is marked in
/// its title.
fn draw_config_modal(frame: &mut Frame, view: &ConfigView) {
    let area = frame.area();

    // Clear whatever is underneath so the modal is opaque, then split.
    frame.render_widget(ratatui::widgets::Clear, area);
    let [left_area, right_area] = Layout::horizontal([
        Constraint::Percentage(view.split_pct),
        Constraint::Min(0),
    ])
    .areas(area);

    let (side, other) = match view.showing {
        Side::Original => ("original", "resolved"),
        Side::Resolved => ("resolved", "original"),
    };
    let hash = if view.hash.is_empty() {
        String::new()
    } else {
        format!(" — {}", view.hash)
    };
    let config_focused = matches!(view.focus, Pane::Config);
    let config_mark = if config_focused { "▶ " } else { "" };
    let config_title = format!(
        " {config_mark}config: {} — {side} (t: {other}){hash} ",
        view.title
    );
    let config_lines: Vec<Line> = view.body().lines().map(highlight_toml_line).collect();
    frame.render_widget(
        Paragraph::new(config_lines)
            .block(pane_block(config_title, config_focused))
            .scroll((view.scroll, 0)),
        left_area,
    );

    let inspect_mark = if config_focused { "" } else { "▶ " };
    let inspect_title = if view.inspect_container.is_empty() {
        format!(" {inspect_mark}inspect ")
    } else {
        format!(" {inspect_mark}inspect — {} ", view.inspect_container)
    };
    let inspect_lines: Vec<Line> = view.inspect.lines().map(highlight_json_line).collect();
    frame.render_widget(
        Paragraph::new(inspect_lines)
            .block(pane_block(inspect_title, !config_focused))
            .scroll((view.inspect_scroll, 0)),
        right_area,
    );
}

/// Border style for the dashboard's table / Detail blocks: normally ACCENT, but
/// DIM when the integrated terminal holds focus (the terminal panel becomes the
/// accented one). Titles stay unchanged — only the border color shifts.
fn dash_border_style(term_focused: bool) -> Style {
    if term_focused {
        Style::default().add_modifier(Modifier::DIM)
    } else {
        Style::default().fg(ACCENT)
    }
}

/// A config-modal pane block. The focused pane's border is accented + bold; the
/// unfocused one is dim so the shared column reads as the divider.
fn pane_block(title: String, focused: bool) -> Block<'static> {
    let border_style = if focused {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::DIM)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(title)
}

/// Render a plain scrollable text modal (help, logs) full-screen. No per-line
/// highlighting — the body is shown verbatim.
fn draw_text_modal(frame: &mut Frame, view: &TextModal) {
    let area = frame.area();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .title(view.title.clone());
    let paragraph = Paragraph::new(view.body.clone()).block(block).scroll((view.scroll, 0));
    frame.render_widget(ratatui::widgets::Clear, area);
    frame.render_widget(paragraph, area);
}

/// Light per-line TOML highlighting: `[section]` headers cyan bold, comments
/// dim, the `key =` part of an assignment green, and the rest default.
fn highlight_toml_line(line: &str) -> Line<'static> {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') {
        return Line::from(Span::styled(
            line.to_string(),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    if trimmed.starts_with('[') {
        return Line::from(Span::styled(
            line.to_string(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    }
    // Split a `key = value` assignment at the first `=`, styling the key half
    // (through the `=`) green and leaving the value default.
    if let Some(eq) = line.find('=') {
        let (key, value) = line.split_at(eq + 1);
        return Line::from(vec![
            Span::styled(key.to_string(), Style::default().fg(Color::Green)),
            Span::raw(value.to_string()),
        ]);
    }
    Line::from(Span::raw(line.to_string()))
}

/// Light per-line JSON highlighting mirroring [`highlight_toml_line`]: a
/// `"key":` prefix (through the colon) is green, structural punctuation-only
/// lines (`{`, `}`, `[`, `],`) are dim, and everything else is default. The
/// classification is shared with the CLI `inspect` command
/// ([`crate::render::classify_json_line`]) so both highlight identically.
fn highlight_json_line(line: &str) -> Line<'static> {
    match crate::render::classify_json_line(line) {
        JsonLine::Structural => Line::from(Span::styled(
            line.to_string(),
            Style::default().add_modifier(Modifier::DIM),
        )),
        JsonLine::KeyValue(split) => {
            let (key, value) = line.split_at(split);
            Line::from(vec![
                Span::styled(key.to_string(), Style::default().fg(Color::Green)),
                Span::raw(value.to_string()),
            ])
        }
        JsonLine::Plain => Line::from(Span::raw(line.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::App;
    use crate::tui::term::TermSession;
    use std::path::PathBuf;

    /// An `App` with the given terminal sessions opened, for layout/tab tests.
    fn app_with_terms(sessions: Vec<TermSession>) -> App {
        let mut app = App::new(PathBuf::from("/tmp"));
        for s in sessions {
            app.terms.open(s);
        }
        app
    }

    #[test]
    fn term_pane_size_none_matches_inner_geometry() {
        // 80x40 frame, prompt closed. draw() reserves 1 (tabs) + 1 (bottom) → 38
        // content rows. Bottom section = 50% of 38 = 19 rows; panel width = 70%
        // of 80 = 56 cols. Inner screen = minus 1-cell border each side.
        let frame = Rect::new(0, 0, 80, 40);
        assert_eq!(term_pane_size(frame, false), Some((19 - 2, 56 - 2)));
    }

    #[test]
    fn term_pane_size_prompt_open_shrinks_height() {
        // Prompt open reserves a 2-line bottom bar → 37 content rows; 50% = 18.
        let frame = Rect::new(0, 0, 80, 40);
        assert_eq!(term_pane_size(frame, true), Some((18 - 2, 56 - 2)));
    }

    #[test]
    fn term_pane_size_tiny_frame_is_none() {
        // Too small to carve a usable inner screen.
        assert_eq!(term_pane_size(Rect::new(0, 0, 4, 4), false), None);
    }

    #[test]
    fn inbox_areas_split_the_width_at_split_pct() {
        let area = Rect::new(2, 3, 100, 20);
        let (list, thread) = inbox_areas(area, 40);
        assert_eq!((list.x, list.width), (2, 40));
        assert_eq!((thread.x, thread.width), (42, 60));
        assert_eq!((list.y, list.height, thread.y, thread.height), (3, 20, 3, 20));
        // The default split, on an odd width: the thread pane takes the rest.
        let (list, thread) = inbox_areas(Rect::new(0, 0, 81, 10), crate::tui::app::InboxView::default().split_pct);
        assert_eq!((list.width, thread.width), (32, 49));
    }

    #[test]
    fn tab_specs_number_and_flag_active() {
        let app = app_with_terms(vec![
            TermSession::test_session("web-1", "devsandbox-web-1", 24, 80),
            TermSession::test_session("api-2", "devsandbox-api-2", 24, 80),
        ]);
        // `open` makes the last session active.
        let specs = terminal_tab_specs(&app);
        assert_eq!(
            specs,
            vec![
                ("1:web-1".to_string(), false, false),
                ("2:api-2".to_string(), true, false),
            ]
        );
    }

    #[test]
    fn tab_specs_exited_gets_suffix() {
        let dead = TermSession::test_session("web-1", "devsandbox-web-1", 24, 80);
        dead.set_exited();
        let app = app_with_terms(vec![dead]);
        let specs = terminal_tab_specs(&app);
        assert_eq!(specs, vec![("1:web-1 (exited)".to_string(), true, true)]);
    }

    #[test]
    fn highlight_toml_styles_by_kind() {
        // Section header: cyan bold.
        let header = highlight_toml_line("[sandbox.repo]");
        assert_eq!(header.spans.len(), 1);
        let s = header.spans[0].style;
        assert_eq!(s.fg, Some(ACCENT));
        assert!(s.add_modifier.contains(Modifier::BOLD));

        // Comment: dim.
        let comment = highlight_toml_line("  # a note");
        assert!(comment.spans[0].style.add_modifier.contains(Modifier::DIM));

        // Assignment: key half green, value split off.
        let kv = highlight_toml_line("image = \"node:22\"");
        assert_eq!(kv.spans.len(), 2);
        assert_eq!(kv.spans[0].style.fg, Some(Color::Green));
        assert_eq!(kv.spans[0].content, "image =");
        assert_eq!(kv.spans[1].content, " \"node:22\"");

        // Plain line: single default span, no panic.
        let plain = highlight_toml_line("just text");
        assert_eq!(plain.spans.len(), 1);
    }

    #[test]
    fn highlight_splits_a_full_document() {
        let doc = "[sandbox.s]\nimage = \"a\"\n# c\n\nx";
        let lines: Vec<Line> = doc.lines().map(highlight_toml_line).collect();
        assert_eq!(lines.len(), 5);
    }

    #[test]
    fn highlight_json_styles_by_kind() {
        // Key/value: green key half through the colon, plain value.
        let kv = highlight_json_line("    \"Id\": \"abc\",");
        assert_eq!(kv.spans.len(), 2);
        assert_eq!(kv.spans[0].style.fg, Some(Color::Green));
        assert_eq!(kv.spans[0].content, "    \"Id\":");
        assert_eq!(kv.spans[1].content, " \"abc\",");

        // Structural punctuation line: dim.
        let brace = highlight_json_line("  {");
        assert_eq!(brace.spans.len(), 1);
        assert!(brace.spans[0].style.add_modifier.contains(Modifier::DIM));

        // Plain value line: single default span, no panic.
        let plain = highlight_json_line("    \"abc\"");
        assert_eq!(plain.spans.len(), 1);
        assert_eq!(plain.spans[0].style.fg, None);
    }
}
