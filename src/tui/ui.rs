//! Immediate-mode rendering for the dashboard. Layout: tab bar on top, content
//! area in the middle, help bar at the bottom. The Instances tab renders real
//! data from the latest snapshot.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Paragraph, Row, Table, TableState};
use tui_term::widget::{Cursor, PseudoTerminal};

use super::app::{
    chip, form_lines, form_title, forms_offset, pane_feed, pane_header, pinned_forms, short_age, title_of, App,
    ConfigView, Extract, Focus, FormSpot, HeaderRow, HelpModal, InboxFocus, Modal, PaneLine, Pane, PortRow, Side, Tab,
    TextModal, Thread, Tone, View,
};
use super::settings::SETTINGS;
use super::data::{
    humanize_secs, sandbox_stats, totals_line, ContainerStatus, InstanceRow, Node, SandboxRow,
    ServiceRow, Snapshot,
};
use super::procs::{is_agent, ProcState, MESSAGE_ROW};
use super::markdown;
use super::prompt::Prompt;
use super::textarea::{self, TextArea};
use super::select::{self, RegionId};
use crate::devsbd::notify::Level;
use crate::inbox::{Kind, State};
use crate::render::JsonLine;

pub(super) const ACCENT: Color = Color::Rgb(175, 135, 255);
const SELECTION: Color = Color::Rgb(0, 215, 135);

pub fn draw(frame: &mut Frame, app: &App) {
    // Every pane drawn below registers its selectable text area anew.
    app.clear_regions();
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

    // A modal draws over everything, using the full frame: only its own
    // regions may be hit, not the panes it hides.
    if !matches!(app.modal, Modal::None) {
        app.clear_regions();
    }
    match &app.modal {
        Modal::None => {}
        Modal::Config(view) => draw_config_modal(frame, app, view),
        Modal::Help(view) => draw_help_modal(frame, app, view),
        Modal::Logs(view) => draw_text_modal(frame, app, view),
    }

    // Last, so nothing drawn hides it; and the copy (or the count a release
    // without copy on select shows) reads the very cells this frame shows.
    let extract = app.take_extract_request();
    if let Some((region, sel)) = app.selection_in_frame() {
        let buf = frame.buffer_mut();
        if let Some(extract) = extract {
            let text = match region.source {
                // Off the vt100 screen, scrollback included; the panel's
                // lock is long released.
                select::Source::Terminal { .. } => terminal_selection_text(app, &sel),
                _ => select::extract(buf, &region, &sel),
            };
            match extract {
                Extract::Copy => app.set_clipboard(text),
                Extract::Measure => app.set_measured(&text),
            }
        }
        select::highlight(buf, &region, &sel);
    }
}

/// The text of a terminal selection, read from the active session's screen.
fn terminal_selection_text(app: &App, sel: &select::Selection) -> String {
    let Some(Ok(mut parser)) = app.terms.active_session().map(|s| s.parser().lock()) else {
        return String::new();
    };
    let (from, to) = sel.range();
    select::extract_terminal(parser.screen_mut(), from, to)
}

/// Register the inner text area of a bordered pane drawn at `area` as
/// selectable. Every dashboard pane is a `Borders::ALL` block with its title
/// on the top border, so the inner rect is the same as the drawn block's.
fn selectable(app: &App, id: RegionId, area: Rect) {
    app.add_region(id, Block::bordered().inner(area));
}

/// The tab bar row's `(tabs, totals)` split, plus the totals text. The
/// right-aligned totals share the row; the tabs take the rest. Shared by the
/// draw path and [`tab_hit`], so a click can't land on a clipped title.
fn tab_bar_areas(app: &App, area: Rect) -> (Rect, Rect, Option<String>) {
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
    (tabs_area, totals_area, (reserve > 0).then_some(totals).flatten())
}

/// Padding on each side of a tab title, and the divider between tabs: what
/// ratatui's `Tabs` drew (its default padding, our `" "` divider) before the
/// bar was rendered by hand for click hit-testing.
const TAB_PAD: u16 = 1;
const TAB_DIVIDER: u16 = 1;

/// Every tab with its title and the columns it covers (title plus its
/// padding), relative to the tab area's left edge and unclipped. The one
/// layout both [`draw_tabs`] and [`tab_hit`] read, so a click lands on the
/// tab drawn there; `Tabs`' internal layout isn't exposed to mirror.
pub(crate) fn tab_spans(app: &App) -> Vec<(Tab, String, std::ops::Range<u16>)> {
    let mut x = 0;
    Tab::ALL
        .iter()
        .map(|&tab| {
            let title = app.tab_title(tab);
            let end = x + TAB_PAD + cols(&title) as u16 + TAB_PAD;
            let span = (tab, title, x..end);
            x = end + TAB_DIVIDER;
            span
        })
        .collect()
}

/// The tab under a click at `(col, row)` on a frame of size `frame`: the tab
/// bar is the frame's first row (see [`draw`]).
pub(crate) fn tab_hit(app: &App, frame: Rect, col: u16, row: u16) -> Option<Tab> {
    if row != frame.y || frame.height == 0 {
        return None;
    }
    let (tabs_area, _, _) = tab_bar_areas(app, Rect { height: 1, ..frame });
    if col < tabs_area.x || col >= tabs_area.right() {
        return None;
    }
    let rel = col - tabs_area.x;
    tab_spans(app).into_iter().find(|(_, _, r)| r.contains(&rel)).map(|(t, _, _)| t)
}

fn draw_tabs(frame: &mut Frame, app: &App, area: Rect) {
    let (tabs_area, totals_area, totals) = tab_bar_areas(app, area);

    let highlight = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
    let pad = " ".repeat(TAB_PAD as usize);
    let mut spans = Vec::new();
    for (i, (tab, title, _)) in tab_spans(app).into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" ".repeat(TAB_DIVIDER as usize)));
        }
        let style = if tab == app.tab { highlight } else { Style::default() };
        spans.push(Span::raw(pad.clone()));
        spans.push(Span::styled(title, style));
        spans.push(Span::raw(pad.clone()));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), tabs_area);

    if let Some(totals) = totals {
        let line = Line::from(Span::styled(totals, Style::default().add_modifier(Modifier::DIM)))
            .alignment(Alignment::Right);
        frame.render_widget(Paragraph::new(line), totals_area);
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
    let (_top, _detail, panel) = open_bottom_split(frame_content_area(frame, prompt_open));
    panel
}

/// The content area of a frame of size `frame`, mirroring `draw()`'s vertical
/// split: tab bar (1), content (Min 0), bottom bar (2 rows with the prompt
/// open, else 1). Shared by the mouse hit-tests that only know the frame size.
fn frame_content_area(frame: Rect, prompt_open: bool) -> Rect {
    let bottom = if prompt_open { 2 } else { 1 };
    let [_tab_area, content_area, _bottom_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(bottom),
    ])
    .areas(frame);
    content_area
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
    selectable(app, RegionId::ServicesTable, table_area);
    selectable(app, RegionId::ServiceDetail, detail_area);
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
    selectable(app, RegionId::PortsTable, top);
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
    let (_top, _detail, terms_area) = content_areas(app, area);
    let region = inbox_region(area, terms_area.is_some());
    let (list_area, thread_area) = inbox_areas(region, app.inbox.split_pct);
    draw_inbox_list(frame, app, list_area);
    draw_inbox_pane(frame, app, thread_area);
    if let Some(terms_area) = terms_area {
        draw_terminal_panel(frame, app, terms_area);
    }
}

/// The part of a tab's content `area` the Inbox's list/thread split fills.
/// The Inbox has no Detail box (the thread pane is its detail): without
/// terminals that's the whole content area; with them, the top, the terminal
/// panel staying where it is on every tab. Derived from [`content_areas`]'s
/// own splits so it can't drift from them.
fn inbox_region(area: Rect, terms_open: bool) -> Rect {
    if terms_open {
        open_bottom_split(area).0
    } else {
        area
    }
}

/// The Rect [`inbox_areas`] splits for a frame of size `frame`: what
/// [`draw_inbox`] lays out, computed from the frame size alone so the
/// divider's mouse hit-test (in the I/O-free `App`) matches the drawing.
pub(crate) fn inbox_split_rect(frame: Rect, prompt_open: bool, terms_open: bool) -> Rect {
    inbox_region(frame_content_area(frame, prompt_open), terms_open)
}

/// Column of the list/thread divider in `region` at `split_pct`: the thread
/// pane's left border, right next to the list's right one; a press on
/// either border grabs it.
pub(crate) fn inbox_divider_col(region: Rect, split_pct: u16) -> u16 {
    inbox_areas(region, split_pct).1.x
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

/// Display width of `s` in terminal columns. Through ratatui's `Span::width`
/// (unicode-width underneath), so a wide glyph in a title counts as the two
/// cells it takes, without a direct dependency.
fn cols(s: &str) -> usize {
    Span::raw(s).width()
}

/// `s` cut to at most `width` columns, ending in `…` when anything was cut.
pub(crate) fn truncate(s: &str, width: usize) -> String {
    if cols(s) <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    let mut buf = [0u8; 4];
    for ch in s.chars() {
        let w = cols(ch.encode_utf8(&mut buf));
        if used + w > width - 1 {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

/// Fewest columns the left text keeps before the right text is dropped: a
/// title cut to a couple of letters says less than no age at all.
const FIT_MIN_LEFT: usize = 8;

/// `left` and `right` laid out in `width` columns with at least one space
/// between them: the left text is truncated with `…` to make room, and when
/// even that leaves it fewer than [`FIT_MIN_LEFT`] columns (or its whole
/// self, if shorter) the right text is dropped (returned empty) instead.
fn fit(left: &str, right: &str, width: usize) -> (String, String) {
    let (lw, rw) = (cols(left), cols(right));
    if rw == 0 {
        return (truncate(left, width), String::new());
    }
    if lw + 1 + rw <= width {
        return (left.to_string(), right.to_string());
    }
    if width >= rw + 1 + lw.min(FIT_MIN_LEFT) {
        return (truncate(left, width - rw - 1), right.to_string());
    }
    (truncate(left, width), String::new())
}

/// Re-applies `parts`' styles to `fitted`, a [`fit`] of their concatenation:
/// truncation only cuts a tail and adds `…`, so the fitted text's chars line
/// up with the parts' in order (the `…` takes the style of the part it lands
/// in).
fn restyle(parts: &[(String, Style)], fitted: &str) -> Vec<Span<'static>> {
    let mut chars = fitted.chars();
    let mut out = Vec::new();
    for (text, style) in parts {
        let piece: String = chars.by_ref().take(text.chars().count()).collect();
        if piece.is_empty() {
            break;
        }
        out.push(Span::styled(piece, *style));
    }
    out
}

/// A card's state chip ([`chip`], shared with the pane head) as styled
/// parts: the tones' styles, a status as inline markdown.
fn card_chip(t: &Thread) -> Vec<(String, Style)> {
    chip(t)
        .into_iter()
        .flat_map(|(tone, text)| match tone {
            Tone::Text => styled_parts(markdown::inline_spans(&text), tone_style(tone)),
            _ => vec![(text, tone_style(tone))],
        })
        .collect()
}

/// Inline-markdown `spans` as [`card_line`] parts, each on `base`.
fn styled_parts(spans: Vec<Span<'static>>, base: Style) -> Vec<(String, Style)> {
    spans.into_iter().map(|s| (s.content.into_owned(), base.patch(s.style))).collect()
}

/// Background of the selected card. A mid-dark gray, paired with an explicit
/// foreground ([`CARD_SELECTED_FG`]) so the text reads the same on dark and
/// light terminal themes (a bare tint under the theme's own dark text would
/// vanish on a light one); `REVERSED` would be anything but subtle.
const CARD_TINT: Color = Color::Indexed(236);
const CARD_SELECTED_FG: Color = Color::Indexed(252);
/// The selection bar while the list has no keys: still there, quieter.
const ACCENT_DIM: Color = Color::Rgb(110, 85, 160);

/// Rows a card takes: two content lines and a spacer (dropped after the last
/// card when it doesn't fit).
const CARD_ROWS: usize = 3;

/// One line of a card, exactly `width` columns: column 0 holds the selection
/// bar (or a space, so text aligns), then the left parts, padding, the right
/// text, and one trailing space off the border. Padded explicitly so the
/// selected card's tint spans the whole width.
fn card_line(
    left: &[(String, Style)],
    right: (String, Style),
    width: usize,
    bar: Option<Color>,
    base: Style,
) -> Line<'static> {
    let mut spans = vec![match bar {
        Some(color) => Span::styled("▌", Style::default().fg(color)),
        None => Span::raw(" "),
    }];
    let avail = width.saturating_sub(2);
    let plain: String = left.iter().map(|(s, _)| s.as_str()).collect();
    let (l, r) = fit(&plain, &right.0, avail);
    let used = cols(&l) + cols(&r);
    spans.extend(restyle(left, &l));
    spans.push(Span::raw(" ".repeat(avail.saturating_sub(used))));
    if !r.is_empty() {
        spans.push(Span::styled(r, right.1));
    }
    if width >= 2 {
        spans.push(Span::raw(" "));
    }
    Line::from(spans).style(base)
}

/// A card's two content lines: title (bold while unread, `↗` with a link,
/// ` · archived` when its instance is gone) with the age, then the state chip
/// with the sender. Archived cards are dimmed throughout; the selected one
/// gets the bar ([`ACCENT`] while the list has the keys) and the tint.
fn card_lines(t: &Thread, width: usize, selected: bool, focused: bool, now: u64, utc_offset: i64) -> [Line<'static>; 2] {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let fade = |s: Style| if t.archived { s.add_modifier(Modifier::DIM) } else { s };
    let title_style = if t.unread { Style::default().add_modifier(Modifier::BOLD) } else { Style::default() };
    let mut title = styled_parts(markdown::inline_spans(&title_of(t)), fade(title_style));
    if t.link.is_some() || t.head().is_some_and(|r| r.link.is_some()) {
        title.push((" ↗".into(), fade(Style::default().fg(Color::Blue))));
    }
    if t.archived {
        title.push((" · archived".into(), dim));
    }
    let chip: Vec<(String, Style)> = card_chip(t).into_iter().map(|(s, style)| (s, fade(style))).collect();
    let age = (short_age(t.changed_at(), now, utc_offset), dim);
    let from = (t.owner_name.clone(), dim);
    let (bar, base) = if selected {
        let bar = if focused { ACCENT } else { ACCENT_DIM };
        (Some(bar), Style::default().bg(CARD_TINT).fg(CARD_SELECTED_FG))
    } else {
        (None, Style::default())
    };
    [
        card_line(&title, age, width, bar, base),
        card_line(&chip, from, width, bar, base),
    ]
}

/// First card to show so `selected` is fully visible in `height` rows,
/// moving `prev` (last frame's) as little as possible: the list holds still
/// while the cursor moves inside it. Also pulled back so a shrunk list or a
/// taller pane doesn't leave blank rows under the last card.
fn card_offset(prev: usize, selected: usize, len: usize, height: usize) -> usize {
    // n cards need 3n - 1 rows: the last one's spacer can go.
    let fit = ((height + 1) / CARD_ROWS).max(1);
    let mut off = prev.min(len.saturating_sub(fit));
    if selected < off {
        off = selected;
    } else if selected >= off + fit {
        off = selected + 1 - fit;
    }
    off
}

/// Short view names, for a strip too narrow for the full ones.
fn view_short(view: View) -> &'static str {
    match view {
        View::NeedsYou => "Needs",
        other => other.title(),
    }
}

/// The view switcher above the list: `‹ Needs you 3 │ Active 5 │ Done │ All ›`,
/// the current view accented, the arrows dim at the ends (the views clamp,
/// see `View::step`), zero counts left out. Narrower widths drop the counts,
/// then shorten the names.
fn view_strip(current: View, counts: [usize; 4], width: usize) -> Line<'static> {
    Line::from(view_strip_parts(current, counts, width).into_iter().map(|(s, _)| s).collect::<Vec<_>>())
}

/// What a click on a view-strip span does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StripHit {
    /// `‹`: one view left.
    Prev,
    /// `›`: one view right.
    Next,
    /// A view's name (or its count).
    View(View),
}

/// [`view_strip`]'s spans, each tagged with what a click on it does, so
/// drawing and [`view_strip_hit`] can't disagree on where a name sits.
fn view_strip_parts(current: View, counts: [usize; 4], width: usize) -> Vec<(Span<'static>, Option<StripHit>)> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let build = |counts_on: bool, short: bool| {
        let first = current == View::ALL[0];
        let last = current == View::ALL[View::ALL.len() - 1];
        let mut spans = vec![
            (Span::raw(" "), None),
            (Span::styled("‹ ", if first { dim } else { Style::default() }), Some(StripHit::Prev)),
        ];
        for (i, view) in View::ALL.iter().enumerate() {
            if i > 0 {
                spans.push((Span::styled(" │ ", dim), None));
            }
            let style = if *view == current {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                dim
            };
            let hit = Some(StripHit::View(*view));
            spans.push((Span::styled(if short { view_short(*view) } else { view.title() }, style), hit));
            if counts_on && counts[i] > 0 {
                spans.push((Span::styled(format!(" {}", counts[i]), dim), hit));
            }
        }
        spans.push((Span::styled(" ›", if last { dim } else { Style::default() }), Some(StripHit::Next)));
        spans
    };
    let width_of = |parts: &[(Span, Option<StripHit>)]| parts.iter().map(|(s, _)| s.width()).sum::<usize>();
    [(true, false), (false, false)]
        .into_iter()
        .map(|(c, s)| build(c, s))
        .find(|p| width_of(p) <= width)
        .unwrap_or_else(|| build(false, true))
}

/// What a click `x` columns into a `width`-wide view strip hits.
pub(crate) fn view_strip_hit(current: View, counts: [usize; 4], width: usize, x: u16) -> Option<StripHit> {
    let mut start = 0;
    for (span, hit) in view_strip_parts(current, counts, width) {
        let end = start + span.width();
        if (start..end).contains(&(x as usize)) {
            return hit;
        }
        start = end;
    }
    None
}

/// The list column's `(view strip, list block)` split.
fn inbox_list_areas(area: Rect) -> (Rect, Rect) {
    let [strip, list] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    (strip, list)
}

/// Position (into `InboxView::rows`) of the card drawn at `row` in the list
/// block's `inner` area, cards starting at `offset`: three rows per card, the
/// third a spacer that hits nothing (see [`draw_inbox_list`]).
fn card_at(inner: Rect, offset: usize, len: usize, row: u16) -> Option<usize> {
    if row < inner.y || row >= inner.bottom() {
        return None;
    }
    let rel = (row - inner.y) as usize;
    let pos = offset + rel / CARD_ROWS;
    (rel % CARD_ROWS != CARD_ROWS - 1 && pos < len).then_some(pos)
}

/// The reply text the input box shows: only while it has focus (unfocused,
/// it shows the placeholder). Shared by drawing and hit-testing so both size
/// the box alike.
fn focused_reply(app: &App) -> Option<&TextArea> {
    app.inbox.reply.as_ref().filter(|_| app.inbox.focus == InboxFocus::Input).map(|r| &r.line)
}

/// What the composer box holds: a form's text question being edited (the
/// box is shared with it), else the focused reply.
fn composer_text(app: &App) -> Option<&TextArea> {
    app.form_text_edit().or_else(|| focused_reply(app))
}

/// A thread's `compose.hint`, when it says anything: one dim row under the
/// input box.
fn compose_hint(t: &Thread) -> Option<&str> {
    t.compose.as_ref()?.hint.as_deref().filter(|h| !h.trim().is_empty())
}

/// Rows the thread pane keeps under its feed, `width` the pane's inner
/// width: the composer (a thread taking replies), i.e. the input box's border
/// plus 1 to [`textarea::MAX_ROWS`] rows of wrapped `reply`, and a row for
/// `compose.hint` when there is one; a one-line hint (a dispatcher thread that
/// takes no replies); or none (a notify thread, which can't be replied to).
fn pane_bottom_rows(t: &Thread, reply: Option<&TextArea>, form_edit: bool, width: u16) -> u16 {
    if let (true, Some(edit)) = (form_edit, reply) {
        // A form's text edit: the box alone, on any thread.
        return 2 + edit.height(width.saturating_sub(2));
    }
    match (t.kind, &t.compose) {
        (_, Some(_)) => {
            2 + reply.map_or(1, |r| r.height(width.saturating_sub(2))) + u16::from(compose_hint(t).is_some())
        }
        (Kind::Thread, None) => 1,
        (Kind::Notify, None) => 0,
    }
}

/// The thread pane's areas inside its block, top to bottom: the pinned
/// header, the separator row under it, the pinned open forms, the scrolling
/// feed and the composer (or the no-replies line). Room goes to the composer
/// first, then the header, the separator, then the forms (at most half the
/// pane: past that their box scrolls), and the feed gets the rest, so a
/// short pane keeps what the keys act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PaneAreas {
    header: Rect,
    sep: Rect,
    forms: Rect,
    feed: Rect,
    bottom: Rect,
}

fn pane_areas(inner: Rect, header: u16, forms: u16, bottom: u16) -> PaneAreas {
    let bottom = bottom.min(inner.height);
    let header = header.min(inner.height - bottom);
    let sep = 1.min(inner.height - bottom - header);
    let rest = inner.height - bottom - header - sep;
    let forms = forms.min(inner.height / 2).min(rest);
    let feed = rest - forms;
    let row = |y: u16, height: u16| Rect { y, height, ..inner };
    let top = inner.y + header + sep;
    PaneAreas {
        header: row(inner.y, header),
        sep: row(inner.y + header, sep),
        forms: row(top, forms),
        feed: row(top + forms, feed),
        bottom: row(top + forms + feed, bottom),
    }
}

/// The pinned forms as drawn: every box's rows (borders included), which
/// form and question each row belongs to, and the focused question's rows.
struct FormRows {
    lines: Vec<Line<'static>>,
    spots: Vec<FormSpot>,
    focus: Option<std::ops::Range<usize>>,
}

/// The thread pane's layout for `t`: the pinned header as the rows drawn at
/// the pane's inner width, the pinned forms' rows and the areas. Shared by
/// drawing and hit-testing, so a click lands where things are.
struct PaneLayout {
    header: Vec<Line<'static>>,
    forms: FormRows,
    areas: PaneAreas,
}

fn thread_pane_layout(app: &App, t: &Thread, inner: Rect) -> PaneLayout {
    let child = app.thread_child(t);
    let header = header_lines(&pane_header(t, child.as_ref(), app.thread_target_stopped(t), app.utc_offset), inner.width, app.inbox.raw);
    let rows = header.len().min(u16::MAX as usize) as u16;
    let forms = form_rows(app, t, inner.width);
    let want = forms.lines.len().min(u16::MAX as usize) as u16;
    let bottom = pane_bottom_rows(t, composer_text(app), app.form_text_edit().is_some(), inner.width);
    let areas = pane_areas(inner, rows, want, bottom);
    PaneLayout { header, forms, areas }
}

/// `t`'s open forms (newest first) as rounded boxes `width` wide: the
/// form's title on the top border with ` open form ` on the right, the
/// content ([`form_lines`]) wrapped inside a one-column margin. The box
/// with the form cursor is accented while the dashboard has the keys.
fn form_rows(app: &App, t: &Thread, width: u16) -> FormRows {
    let mut out = FormRows { lines: Vec::new(), spots: Vec::new(), focus: None };
    let width = width as usize;
    if width < 8 {
        return out;
    }
    let inner = width - 4;
    for (fi, p) in pinned_forms(t).iter().enumerate() {
        let cursor = app.form_cursor(t.id, p.message);
        let style = if cursor.is_some() && app.focus == Focus::Dashboard {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        let spot = |question| FormSpot { form: fi, question };
        let title = truncate(&crate::inbox::sanitize(form_title(p.form)), width.saturating_sub(6));
        let right = if cols(&title) + 17 <= width { " open form " } else { "" };
        let fill = width.saturating_sub(cols(&title) + cols(right) + 4);
        out.lines.push(Line::from(vec![
            Span::styled("╭ ", style),
            Span::styled(title, style.remove_modifier(Modifier::DIM).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" {}{right}╮", "─".repeat(fill)), style),
        ]));
        out.spots.push(spot(None));
        let content = form_lines(p.form, p.record, cursor.as_ref(), inner);
        for (li, line) in content.lines.iter().enumerate() {
            let question = content.questions.iter().position(|r| r.contains(&li));
            let start = out.lines.len();
            for (row, _) in pane_rows(line, inner as u16, app.inbox.raw) {
                let pad = inner.saturating_sub(row.width());
                let mut spans = vec![Span::styled("│ ", style)];
                spans.extend(row.spans);
                spans.push(Span::raw(" ".repeat(pad)));
                spans.push(Span::styled(" │", style));
                out.lines.push(Line::from(spans));
                out.spots.push(spot(question));
            }
            if let (Some(c), Some(q)) = (&cursor, question) {
                if c.question == q {
                    let r = out.focus.get_or_insert(start..start);
                    r.end = out.lines.len();
                }
            }
        }
        out.lines.push(Line::from(Span::styled(format!("╰{}╯", "─".repeat(width - 2)), style)));
        out.spots.push(spot(None));
    }
    out
}

/// `spans` cut to `width` columns, ending in `…` when cut, styles kept.
fn cut_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let parts: Vec<(String, Style)> = spans.into_iter().map(|s| (s.content.into_owned(), s.style)).collect();
    let plain: String = parts.iter().map(|(s, _)| s.as_str()).collect();
    restyle(&parts, &truncate(&plain, width))
}

/// The header's rows at `width` columns ([`HeaderRow`]): a line is one row,
/// cut with `…`, its right text right-aligned when both fit ([`fit`]); the
/// buttons are packed two spaces apart and wrap between buttons, a button
/// wider than the pane cut on its own row.
fn header_lines(rows: &[HeaderRow], width: u16, raw: bool) -> Vec<Line<'static>> {
    let width = width as usize;
    let dim = Style::default().add_modifier(Modifier::DIM);
    let mut out = Vec::new();
    for row in rows {
        match row {
            HeaderRow::Line { left, right } => {
                let parts: Vec<(String, Style)> =
                    pane_spans(left, raw).into_iter().map(|s| (s.content.into_owned(), s.style)).collect();
                let plain: String = parts.iter().map(|(s, _)| s.as_str()).collect();
                let right = right.as_deref().map(crate::inbox::sanitize).unwrap_or_default();
                let (l, r) = fit(&plain, &right, width);
                let mut spans = restyle(&parts, &l);
                if !r.is_empty() {
                    spans.push(Span::raw(" ".repeat(width.saturating_sub(cols(&l) + cols(&r)))));
                    spans.push(Span::styled(r, dim));
                }
                out.push(Line::from(spans));
            }
            HeaderRow::Buttons(buttons) => {
                let mut cur: Vec<Span<'static>> = Vec::new();
                let mut used = 0;
                for b in buttons {
                    let spans = pane_spans(b, raw);
                    let w: usize = spans.iter().map(Span::width).sum();
                    if used > 0 && used + 2 + w > width {
                        out.push(Line::from(std::mem::take(&mut cur)));
                        used = 0;
                    }
                    if used > 0 {
                        cur.push(Span::raw("  "));
                        used += 2;
                    }
                    used += w.min(width);
                    cur.extend(cut_spans(spans, width));
                }
                if !cur.is_empty() {
                    out.push(Line::from(cur));
                }
            }
        }
    }
    out
}

/// What a click on the Inbox tab lands on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InboxHit {
    Strip(StripHit),
    /// A card, by position in `InboxView::rows`.
    Card(usize),
    /// The list block off any card: a spacer, a border, blank rows.
    ListBlank,
    /// The thread pane above its composer or hint: header, separator,
    /// feed (borders included).
    Thread,
    /// The pinned forms: the form and question under the row, if any; or
    /// (`None`) the composer box while it holds a form's text edit.
    Form(Option<FormSpot>),
    /// The composer: the reply input box and its `compose.hint` row.
    Input,
    /// The "takes no replies" hint row.
    Hint,
}

/// What's under `(col, row)` on the Inbox tab for a frame of size `frame`,
/// from the same layout functions [`draw_inbox`] uses and the list offset it
/// last drew with, so a click hits what is on screen.
pub(crate) fn inbox_hit(app: &App, frame: Rect, col: u16, row: u16) -> Option<InboxHit> {
    let at = Position::new(col, row);
    let region = inbox_split_rect(frame, app.prompt.is_some(), !app.terms.is_empty());
    if !region.contains(at) {
        return None;
    }
    let (list, thread) = inbox_areas(region, app.inbox.split_pct);
    if list.contains(at) {
        let (strip, block) = inbox_list_areas(list);
        if strip.contains(at) {
            let counts = View::ALL.map(|v| app.inbox.count(v));
            return view_strip_hit(app.inbox.view, counts, strip.width as usize, col - strip.x).map(InboxHit::Strip);
        }
        let inner = Block::bordered().inner(block);
        let card = card_at(inner, app.inbox.list_offset(), app.inbox.rows().len(), row);
        return Some(match card {
            Some(pos) if inner.contains(at) => InboxHit::Card(pos),
            _ => InboxHit::ListBlank,
        });
    }
    let Some(t) = app.selected_inbox_thread() else {
        return Some(InboxHit::Thread);
    };
    let inner = Block::bordered().inner(thread);
    let PaneLayout { forms, areas, .. } = thread_pane_layout(app, t, inner);
    if areas.forms.contains(at) {
        let row = (row - areas.forms.y) as usize + app.inbox.forms_scroll();
        return Some(InboxHit::Form(forms.spots.get(row).copied()));
    }
    // The header, separator and feed are all the thread; the composer's
    // hint row is part of it, so a click there doesn't drop a typed reply.
    if !areas.bottom.contains(at) {
        Some(InboxHit::Thread)
    } else if app.form_text_edit().is_some() {
        Some(InboxHit::Form(None))
    } else if t.compose.is_some() {
        Some(InboxHit::Input)
    } else {
        Some(InboxHit::Hint)
    }
}

fn draw_inbox_list(frame: &mut Frame, app: &App, area: Rect) {
    let (strip_area, list_area) = inbox_list_areas(area);
    let counts = View::ALL.map(|v| app.inbox.count(v));
    frame.render_widget(Paragraph::new(view_strip(app.inbox.view, counts, strip_area.width as usize)), strip_area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(zone_border_style(app, InboxFocus::List))
        .title(" Inbox ");
    let inner = block.inner(list_area);
    frame.render_widget(block, list_area);
    app.add_region(RegionId::InboxList, inner);
    let dim = Style::default().add_modifier(Modifier::DIM);
    let rows = app.inbox.rows();
    if rows.is_empty() {
        let text = if app.inbox.threads().is_empty() {
            "no notifications — containers send them with `devsbd notify \"…\"`".to_string()
        } else {
            format!("nothing in {} — ←/→ switch views", app.inbox.view.title())
        };
        let mut lines = vec![Line::default(); (inner.height as usize).saturating_sub(1) / 2];
        lines.push(Line::from(Span::styled(text, dim)).alignment(Alignment::Center));
        frame.render_widget(Paragraph::new(lines), inner);
        return;
    }
    // Wall clock read per frame, so relative times tick between snapshots.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let selected = app.selected().min(rows.len() - 1);
    let height = inner.height as usize;
    let off = card_offset(app.inbox.list_offset(), selected, rows.len(), height);
    app.inbox.set_list_offset(off);
    let focused = app.focus == Focus::Dashboard && app.inbox.focus == InboxFocus::List;
    let threads = app.inbox.threads();
    let width = inner.width as usize;
    let mut lines: Vec<Line> = Vec::with_capacity(height + CARD_ROWS);
    for (pos, &i) in rows.iter().enumerate().skip(off) {
        if lines.len() >= height {
            break;
        }
        if pos > off {
            lines.push(Line::default());
        }
        lines.extend(card_lines(&threads[i], width, pos == selected, focused, now, app.utc_offset));
    }
    frame.render_widget(Paragraph::new(lines), inner);
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

fn tone_style(tone: Tone) -> Style {
    match tone {
        Tone::Plain | Tone::Text | Tone::Markdown => Style::default(),
        Tone::Cursor => Style::default().add_modifier(Modifier::REVERSED),
        Tone::Dim => Style::default().add_modifier(Modifier::DIM),
        Tone::Bold | Tone::Title => Style::default().add_modifier(Modifier::BOLD),
        Tone::Link => Style::default().fg(Color::Blue),
        Tone::State(s) => state_style(s),
        Tone::Level(l) => level_style(l),
    }
}

/// One pane line as the rows it takes at `width` columns. The pane draws
/// exactly these rows (no `Paragraph` wrap), so their count is what bounds
/// its scroll. A [`Tone::Markdown`] line is a whole document, rendered as
/// blocks; elsewhere [`Tone::Text`]/[`Tone::Title`] segments are inline
/// markdown on their tone's style, the rest plain; then the line is word
/// wrapped by display width. `raw` (`m`) shows the markdown as its source.
/// Every segment is sanitized: it may be stored text from before the
/// apply-boundary pass. Each row comes with how a copy joins it
/// ([`select::RowMeta`]).
fn pane_rows(line: &PaneLine, width: u16, raw: bool) -> Vec<(Line<'static>, select::RowMeta)> {
    if let [(Tone::Markdown, md)] = line.as_slice() {
        return if raw { markdown::raw_rows(md, width) } else { markdown::render_rows(md, width) };
    }
    markdown::wrap_rows(&pane_spans(line, raw), width as usize, &[], &[], false)
}

/// One non-document pane line as styled spans, unwrapped (see [`pane_rows`]).
fn pane_spans(line: &PaneLine, raw: bool) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (tone, text) in line {
        let base = tone_style(*tone);
        match tone {
            Tone::Text | Tone::Title | Tone::Markdown if !raw => spans.extend(
                markdown::inline_spans(text).into_iter().map(|s| {
                    let style = base.patch(s.style);
                    s.style(style)
                }),
            ),
            Tone::Text | Tone::Title | Tone::Markdown => spans.push(Span::styled(markdown::fold(text), base)),
            _ => spans.push(Span::styled(crate::inbox::sanitize(text), base)),
        }
    }
    spans
}

/// The thread pane (layout v3, docs/inbox-redesign.md *TUI Inbox*): the
/// pinned header ([`pane_header`]: title, chip, child, action row), a
/// separator joined to the border, the feed newest first ([`pane_feed`]),
/// scrolling on its own, then the composer (the input box and its
/// `compose.hint`), a one-line no-replies hint, or nothing (a notify thread).
/// Records the feed's scroll bound for the scroll keys
/// (`InboxView::set_pane_max`), since only here are the size and wrapping
/// known. The thread's open forms are pinned between the separator and the
/// feed ([`form_rows`]), their box scrolled to the form cursor
/// ([`forms_offset`]); a form's text edit takes the composer box.
fn draw_inbox_pane(frame: &mut Frame, app: &App, area: Rect) {
    let border = zone_border_style(app, InboxFocus::Thread);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(if app.inbox.raw { " Thread · raw " } else { " Thread " });
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let dim = Style::default().add_modifier(Modifier::DIM);
    let Some(t) = app.selected_inbox_thread() else {
        let text = Line::from(Span::styled("no thread selected", dim)).alignment(Alignment::Center);
        frame.render_widget(Paragraph::new(text), inner);
        return;
    };
    let PaneLayout { header, forms, areas } = thread_pane_layout(app, t, inner);
    // Screen cells: the header never scrolls, so what's drawn is the text.
    app.add_region(RegionId::InboxHeader, areas.header);
    frame.render_widget(Paragraph::new(header), areas.header);
    if areas.sep.height > 0 && area.width >= 2 {
        let rule = format!("├{}┤", "─".repeat(area.width as usize - 2));
        let row = Rect { x: area.x, width: area.width, ..areas.sep };
        frame.render_widget(Paragraph::new(Span::styled(rule, border)), row);
    }
    let (total, height) = (forms.lines.len(), areas.forms.height as usize);
    let follow = app.inbox.take_forms_follow();
    let off = forms_offset(app.inbox.forms_scroll(), forms.focus, total, height, follow);
    app.inbox.set_forms_scroll(off, total.saturating_sub(height));
    if height > 0 {
        let off = off.min(u16::MAX as usize) as u16;
        frame.render_widget(Paragraph::new(forms.lines).scroll((off, 0)), areas.forms);
    }
    let feed = areas.feed;
    let (lines, texts): (Vec<Line>, Vec<select::RowText>) = pane_feed(t, app.utc_offset)
        .iter()
        .flat_map(|l| pane_rows(l, feed.width, app.inbox.raw))
        .map(|(line, meta)| {
            let text = select::RowText::new(&line, meta);
            (line, text)
        })
        .unzip();
    let rows = lines.len().min(u16::MAX as usize) as u16;
    let max = rows.saturating_sub(feed.height);
    app.inbox.set_pane_max(max);
    let scroll = app.inbox.scroll.min(max);
    // The feed only: the header and the composer are not message text.
    app.add_rows_region(RegionId::InboxThread, feed, scroll as usize, texts);
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), feed);
    let bottom = areas.bottom;
    if let (Some(edit), Some(q)) = (app.form_text_edit(), app.editing_question()) {
        let multiline = matches!(q.kind, crate::inbox::form::QuestionKind::Text { multiline: true, .. });
        let keys = if multiline { "enter/esc keep · alt-enter newline" } else { "enter/esc keep" };
        let label = markdown::fold(&q.label);
        let title = format!(" {} · {keys} ", truncate(&label, (bottom.width as usize).saturating_sub(cols(keys) + 8)));
        let placeholder = match &q.kind {
            crate::inbox::form::QuestionKind::Text { placeholder: Some(p), .. } => p.as_str(),
            _ => "",
        };
        draw_text_box(frame, app, Some(edit), &title, placeholder, InboxFocus::Form, bottom);
        return;
    }
    match &t.compose {
        Some(compose) => {
            let hint = compose_hint(t).filter(|_| bottom.height > 0);
            let input = Rect { height: bottom.height - u16::from(hint.is_some()), ..bottom };
            draw_reply_input(frame, app, compose.placeholder.as_deref(), input);
            if let Some(hint) = hint {
                let text = truncate(&crate::inbox::sanitize(hint), bottom.width as usize);
                let row = Rect { y: bottom.bottom() - 1, height: 1, ..bottom };
                frame.render_widget(Paragraph::new(Span::styled(text, dim)), row);
            }
        }
        None if bottom.height > 0 => {
            frame.render_widget(Paragraph::new(Span::styled("this thread takes no replies", dim)), bottom);
        }
        None => {}
    }
}

/// The thread pane's reply input: a rounded box holding the text being typed,
/// wrapped and scrolled to the cursor ([`TextArea::view`]), or the thread's
/// placeholder dim while it's empty. The caret is placed (like the `:`
/// prompt's) only while the input has focus, so it doesn't blink in a box the
/// keys don't reach.
fn draw_reply_input(frame: &mut Frame, app: &App, placeholder: Option<&str>, area: Rect) {
    let reply = focused_reply(app);
    let title = if reply.is_some() { " enter sends · esc back " } else { " r reply " };
    let placeholder = placeholder.unwrap_or("reply to the owner");
    draw_text_box(frame, app, reply, title, placeholder, InboxFocus::Input, area);
}

/// The composer box: `text` (the focused reply, or a form's text edit)
/// wrapped and scrolled to the cursor, else `placeholder` dim; the caret
/// placed while the dashboard has the keys. Its border follows `zone`.
fn draw_text_box(
    frame: &mut Frame,
    app: &App,
    text: Option<&TextArea>,
    title: &str,
    placeholder: &str,
    zone: InboxFocus,
    area: Rect,
) {
    let reply = text;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(zone_border_style(app, zone))
        .title(title.to_string());
    let inner = block.inner(area);
    let view = reply.map(|r| r.view(inner.width, inner.height));
    let lines: Vec<Line> = match &view {
        Some(v) if reply.is_some_and(|r| !r.input().is_empty()) => {
            v.lines.iter().map(|l| Line::from(l.clone())).collect()
        }
        _ => {
            vec![Line::from(Span::styled(placeholder.to_string(), Style::default().add_modifier(Modifier::DIM)))]
        }
    };
    frame.render_widget(Paragraph::new(lines).block(block), area);
    if let Some(v) = view {
        if app.focus == Focus::Dashboard && inner.width > 0 && inner.height > 0 {
            let (col, row) = v.caret;
            frame.set_cursor_position((inner.x + col.min(inner.width - 1), inner.y + row.min(inner.height - 1)));
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
    let Ok(mut parser) = session.parser().lock() else {
        frame.render_widget(block, area);
        return;
    };
    // The body is selectable unless the child tracks the mouse (it gets the
    // buttons then, `App::terminal_mouse`); an exited one tracks nothing.
    let screen = parser.screen_mut();
    if screen.mouse_protocol_mode() == vt100::MouseProtocolMode::None || session.exited() {
        app.add_terminal_region(block.inner(area), select::terminal_top(screen));
    }
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
        selectable(app, RegionId::InstancesTree, table_area);
        selectable(app, RegionId::Detail, detail_area);
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
        selectable(app, RegionId::InstancesTree, table_area);
        selectable(app, RegionId::Detail, detail_area);
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
        Modal::Help(_) => {
            "tab/S-tab setting · space/enter/click toggle · ↑↓ scroll · pgup/pgdn · g/G · esc/? close"
                .to_string()
        }
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
                "q quit · tab switch · ↑↓ select · ←→ fold · enter config · r {} · o vscode · s {} · d rm · l logs · p forward · t term · : cmd · ? help",
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
                    "esc list · ↑↓ scroll · tab form/reply · enter open link · 1-9 actions · r reply · d done · u reopen · o vscode · t term · l logs · p forward · m raw · q quit · ? help".to_string()
                }
                InboxFocus::Input => {
                    "enter send · alt-enter newline · esc back to thread · ←→↑↓ home end edit · ctrl-u clear · ctrl-w delete word"
                        .to_string()
                }
                InboxFocus::Form => match &app.inbox.form {
                    Some(e) if e.editing.is_some() => {
                        "enter/esc keep the edit · alt-enter newline · ←→↑↓ home end edit · ctrl-u clear · ctrl-w delete word"
                            .to_string()
                    }
                    Some(e) if e.confirming => "enter submit · esc cancel".to_string(),
                    _ => "tab/S-tab question · ↑↓ option · space pick · e edit text · enter confirm · r reply · esc thread · q quit · ? help"
                        .to_string(),
                },
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
    // Scrolled sideways so the caret stays in view past the right edge.
    let room = input_area.width.saturating_sub(prefix.len() as u16);
    let (_, shown, caret) = textarea::hscroll(prompt.input(), prompt.cursor(), room as usize);
    let line = Line::from(vec![Span::styled(prefix, Style::default().fg(ACCENT)), Span::raw(shown)]);
    frame.render_widget(Paragraph::new(line), input_area);

    let col = input_area.x + prefix.len() as u16 + caret;
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
fn draw_config_modal(frame: &mut Frame, app: &App, view: &ConfigView) {
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
    selectable_lines(app, RegionId::ConfigLeft, left_area, view.scroll, view.body());

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
    selectable_lines(app, RegionId::ConfigRight, right_area, view.inspect_scroll, &view.inspect);
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
fn draw_text_modal(frame: &mut Frame, app: &App, view: &TextModal) {
    let area = frame.area();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .title(view.title.clone());
    let paragraph = Paragraph::new(view.body.clone()).block(block).scroll((view.scroll, 0));
    frame.render_widget(ratatui::widgets::Clear, area);
    frame.render_widget(paragraph, area);
    selectable_lines(app, RegionId::TextModal, area, view.scroll, &view.body);
}

/// The `?` modal's `(settings, help body)` areas inside its border over the
/// full frame `area`: one row per [`SETTINGS`] entry, a blank row, then the
/// help text. Shared by the draw and [`help_setting_hit`].
fn help_layout(area: Rect) -> (Rect, Rect) {
    let inner = Block::bordered().inner(area);
    let n = (SETTINGS.len() as u16).min(inner.height);
    let settings = Rect { height: n, ..inner };
    let gap = (n + 1).min(inner.height);
    let body = Rect { y: inner.y + gap, height: inner.height - gap, ..inner };
    (settings, body)
}

/// The [`SETTINGS`] row under `(col, row)` of the `?` modal drawn over the
/// full frame `area`, if any.
pub fn help_setting_hit(area: Rect, col: u16, row: u16) -> Option<usize> {
    let (settings, _) = help_layout(area);
    settings.contains(Position::new(col, row)).then(|| (row - settings.y) as usize)
}

/// The `?` Settings & help modal: the settings rows on top (`[x]` per
/// value, the cursor row accented), the key reference below, scrolling and
/// selectable like any [`TextModal`] (the settings rows are no region: a
/// press there is a toggle, not a selection).
fn draw_help_modal(frame: &mut Frame, app: &App, view: &HelpModal) {
    let area = frame.area();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .title(view.text.title.clone());
    frame.render_widget(ratatui::widgets::Clear, area);
    frame.render_widget(block, area);
    let (settings_area, body_area) = help_layout(area);
    let width = SETTINGS.iter().map(|s| s.label.chars().count()).max().unwrap_or(0);
    let rows: Vec<Line> = SETTINGS
        .iter()
        .enumerate()
        .map(|(i, spec)| {
            let on = if (spec.get)(&app.settings) { 'x' } else { ' ' };
            let cursor = i == view.cursor;
            let style = if cursor {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::from(vec![
                Span::styled(format!("{} [{on}] {:<width$}  ", if cursor { '›' } else { ' ' }, spec.label), style),
                Span::styled((spec.help)(app.copy_key_intercepted), Style::default().add_modifier(Modifier::DIM)),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(rows), settings_area);
    let body = &view.text.body;
    frame.render_widget(Paragraph::new(body.clone()).scroll((view.text.scroll, 0)), body_area);
    let rows = body.lines().map(select::RowText::plain).collect();
    app.add_rows_region(RegionId::TextModal, body_area, view.text.scroll as usize, rows);
}

/// [`selectable`] for a bordered `Paragraph` of `body` scrolled to `scroll`
/// without wrap: one row per line, copied whole past the pane's right edge
/// ([`select::extract_rows`]).
fn selectable_lines(app: &App, id: RegionId, area: Rect, scroll: u16, body: &str) {
    let rows = body.lines().map(select::RowText::plain).collect();
    app.add_rows_region(id, Block::bordered().inner(area), scroll as usize, rows);
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
    fn inbox_split_rect_matches_draw_inbox() {
        let frame = Rect::new(0, 0, 100, 40);
        for prompt_open in [false, true] {
            // Content = frame minus the tab line and the 1-2 row bottom bar.
            let bottom = if prompt_open { 2 } else { 1 };
            let content = Rect::new(0, 1, 100, 39 - bottom);
            // No terminals: the whole content area, as draw_inbox lays it out.
            let closed = App::new(PathBuf::from("/tmp"));
            let (top, detail, terms) = content_areas(&closed, content);
            assert!(terms.is_none());
            let rect = inbox_split_rect(frame, prompt_open, false);
            assert_eq!(rect, top.union(detail));
            assert_eq!(rect, content);
            // Terminals open: the top only, ending where the panel starts.
            let open = app_with_terms(vec![TermSession::test_session("web-1", "devsandbox-web-1", 24, 80)]);
            let (top, _detail, terms) = content_areas(&open, content);
            let rect = inbox_split_rect(frame, prompt_open, true);
            assert_eq!(rect, top);
            assert_eq!(rect.bottom(), terminal_panel_rect(frame, prompt_open).y);
            assert_eq!(terms, Some(terminal_panel_rect(frame, prompt_open)));
            // The divider is the thread pane's left edge.
            let (_list, thread) = inbox_areas(rect, 40);
            assert_eq!(inbox_divider_col(rect, 40), thread.x);
            assert_eq!(thread.x, 40);
        }
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

    use crate::devsbd::notify::Record;
    use crate::inbox::{Inbox, Note};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn dthread(title: &str, state: State, status: Option<&str>) -> Thread {
        Thread {
            kind: Kind::Thread,
            title: title.into(),
            owner_name: "bab-disp".into(),
            state: Some(state),
            status: status.map(str::to_string),
            ..Thread::default()
        }
    }

    fn note(msg: &str, level: Level, at: u64) -> Thread {
        Thread {
            kind: Kind::Notify,
            owner_name: "builder".into(),
            notes: vec![Note { id: 1, record: Record { level, key: None, link: None, msg: msg.into(), at } }],
            ..Thread::default()
        }
    }

    #[test]
    fn fit_keeps_both_with_a_gap_and_truncates_left_first() {
        assert_eq!(fit("title", "2m", 20), ("title".into(), "2m".into()));
        // Exactly fits with one space.
        assert_eq!(fit("title", "2m", 8), ("title".into(), "2m".into()));
        // Left cut with `…` to keep the gap.
        assert_eq!(fit("a long title here", "2m", 14), ("a long tit…".into(), "2m".into()));
        // Too narrow to keep FIT_MIN_LEFT columns of title: the right goes.
        assert_eq!(fit("a long title here", "12 min ago", 15), ("a long title h…".into(), String::new()));
        // A short left needs only its own width.
        assert_eq!(fit("ok", "12 min ago", 13), ("ok".into(), "12 min ago".into()));
        assert_eq!(fit("anything", "", 4), ("any…".into(), String::new()));
        assert_eq!(fit("x", "y", 0), (String::new(), String::new()));
    }

    #[test]
    fn fit_counts_display_width() {
        // `界` is two columns: 4 glyphs = 8 columns.
        assert_eq!(fit("世界世界", "1h", 11), ("世界世界".into(), "1h".into()));
        // 8 columns for the left: three wide glyphs (6) + `…`, as a fourth
        // wouldn't leave room for it.
        let (l, r) = fit("世界世界世界", "1h", 11);
        assert_eq!((l.as_str(), r.as_str()), ("世界世…", "1h"));
        assert!(cols(&l) <= 8);
        // A wide glyph that would straddle the limit is dropped whole.
        assert_eq!(truncate("a世界", 3), "a…");
    }

    #[test]
    fn chip_per_kind_state_and_level() {
        let c = |t: &Thread| card_chip(t).into_iter().map(|(s, _)| s).collect::<String>();
        // The status shows next to every state, not just active.
        assert_eq!(c(&dthread("t", State::NeedsYou, Some("x"))), "● x");
        assert_eq!(c(&dthread("t", State::NeedsYou, None)), "● needs you");
        assert_eq!(c(&dthread("t", State::Active, Some("running ci"))), "○ running ci");
        assert_eq!(c(&dthread("t", State::Active, None)), "○ active");
        assert_eq!(c(&dthread("t", State::Active, Some("  "))), "○ active");
        assert_eq!(c(&dthread("t", State::Done, None)), "✓ done");
        assert_eq!(c(&dthread("t", State::Done, Some("merged"))), "✓ merged");
        assert_eq!(c(&note("m", Level::Warn, 0)), "▲ warn");
        assert_eq!(c(&note("m", Level::Error, 0)), "✖ error");
        assert_eq!(c(&note("m", Level::Info, 0)), "· info");
        assert_eq!(card_chip(&dthread("t", State::NeedsYou, None))[0].1.fg, Some(Color::Yellow));
        assert_eq!(card_chip(&dthread("t", State::NeedsYou, Some("x")))[0].1.fg, Some(Color::Yellow));
        assert_eq!(card_chip(&note("m", Level::Error, 0))[0].1.fg, Some(Color::Red));
        // A status is inline markdown.
        let parts = card_chip(&dthread("t", State::Active, Some("run `ci`\nnext")));
        assert_eq!(parts.iter().map(|(s, _)| s.as_str()).collect::<String>(), "○ run ci next");
        assert_eq!(parts.iter().find(|(s, _)| s == "ci").unwrap().1.bg, markdown::CODE.bg);
    }

    #[test]
    fn card_chip_and_pane_head_agree() {
        let row_text = |l: &Line| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>();
        for t in [
            dthread("t", State::NeedsYou, Some("review `drafts`")),
            dthread("t", State::Active, None),
            dthread("t", State::Done, None),
            note("m", Level::Warn, 0),
        ] {
            let card: String = card_chip(&t).into_iter().map(|(s, _)| s).collect();
            let pane = &header_lines(&pane_header(&t, None, false, 0), 200, false)[1];
            assert!(row_text(pane).starts_with(&format!("{card}  ·  ")), "{card:?} vs {:?}", row_text(pane));
        }
    }

    #[test]
    fn card_lines_layout_and_selection() {
        let mut t = dthread("#6900 feat/agent-run-cost", State::NeedsYou, None);
        t.unread = true;
        t.updated_at = 1000 - 120;
        let [l1, l2] = card_lines(&t, 30, false, true, 1000, 0);
        assert_eq!(text(&l1), " #6900 feat/agent-run-cost 2m ");
        assert_eq!(text(&l2), " ● needs you         bab-disp ");
        assert_eq!((l1.width(), l2.width()), (30, 30));
        assert!(l1.spans[1].style.add_modifier.contains(Modifier::BOLD), "unread title is bold");
        assert_eq!(l1.style.bg, None);

        let [s1, s2] = card_lines(&t, 30, true, true, 1000, 0);
        for (line, bar) in [(&s1, ACCENT), (&s2, ACCENT)] {
            assert_eq!(line.spans[0].content, "▌");
            assert_eq!(line.spans[0].style.fg, Some(bar));
            assert_eq!(line.style.bg, Some(CARD_TINT));
        }
        let [u1, _] = card_lines(&t, 30, true, false, 1000, 0);
        assert_eq!(u1.spans[0].style.fg, Some(ACCENT_DIM));

        // Link + archived suffixes; archived dims the title.
        t.link = Some("https://x".into());
        t.archived = true;
        t.title = "fix".into();
        let [a1, _] = card_lines(&t, 40, false, true, 1000, 0);
        assert!(text(&a1).starts_with(" fix ↗ · archived "));
        assert!(a1.spans[1].style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn card_offset_scrolls_minimally() {
        // 11 rows hold 4 cards (the last spacer dropped).
        assert_eq!(card_offset(0, 3, 10, 11), 0);
        assert_eq!(card_offset(0, 4, 10, 11), 1);
        // Moving up inside the window keeps it still.
        assert_eq!(card_offset(3, 4, 10, 11), 3);
        assert_eq!(card_offset(3, 2, 10, 11), 2);
        // A shrunk list pulls the window back.
        assert_eq!(card_offset(8, 5, 6, 11), 2);
        // Too short for even one card: the selected one leads.
        assert_eq!(card_offset(0, 5, 10, 1), 5);
    }

    #[test]
    fn view_strip_shortens_to_fit() {
        let counts = [3, 5, 0, 9];
        let full = view_strip(View::NeedsYou, counts, 80);
        assert_eq!(text(&full), " ‹ Needs you 3 │ Active 5 │ Done │ All 9 ›");
        let current = full.spans.iter().find(|s| s.content == "Needs you").unwrap();
        assert_eq!(current.style.fg, Some(ACCENT));
        assert!(full.spans[1].style.add_modifier.contains(Modifier::DIM), "‹ dim at the left end");
        assert_eq!(text(&view_strip(View::Active, counts, 36)), " ‹ Needs you │ Active │ Done │ All ›");
        assert_eq!(text(&view_strip(View::All, counts, 20)), " ‹ Needs │ Active │ Done │ All ›");
    }

    fn render(app: &App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn inbox_app() -> App {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let mut a = dthread("#6900 feat/agent-run-cost", State::NeedsYou, Some("review draft replies"));
        a.updated_at = now - 120;
        a.unread = true;
        a.link = Some("https://example.com/pr/6900".into());
        let mut b = dthread("#7414 fix/login", State::Active, Some("running ci"));
        b.updated_at = now - 3600;
        let c = note("ci failed on main", Level::Warn, now - 3 * 86_400);
        let mut d = dthread("#7001 docs/config", State::Done, None);
        d.updated_at = now - 5 * 86_400;
        let e = note("image rebuilt", Level::Info, now - 6 * 86_400);
        let mut threads = vec![a, b, c, d, e];
        for (i, t) in threads.iter_mut().enumerate() {
            t.id = i as u64 + 1;
            t.owner = format!("{}-id", t.owner_name);
        }
        let mut inbox = Inbox::default();
        inbox.threads = threads;
        let mut app = App::new(PathBuf::from("/tmp"));
        app.set_inbox(inbox);
        app.on_key(KeyEvent::from(KeyCode::Char('4')));
        for _ in 0..3 {
            app.on_key(KeyEvent::from(KeyCode::Right));
        }
        app
    }

    #[test]
    fn inbox_renders_cards() {
        let app = inbox_app();
        let screen = render(&app, 120, 30);
        if std::env::var_os("SHOW_INBOX").is_some() {
            println!("{screen}");
        }
        assert!(screen.contains("‹ Needs you 1 │ Active 1 │ Done 1 │ All 5 ›"), "{screen}");
        assert!(screen.contains("╭ Inbox "), "{screen}");
        // Needs you → Active → Done → All: each view without the selected
        // thread starts at its top, and All keeps Done's `#7001` by id.
        assert!(screen.contains("▌#7001 docs/config"), "{screen}");
        assert!(screen.contains(" #6900 feat/agent-run-cost ↗"), "{screen}");
        assert!(screen.contains("▲ warn"), "{screen}");
    }

    #[test]
    fn tab_spans_match_the_rendered_tab_line() {
        let app = inbox_app();
        assert_eq!(app.tab_title(Tab::Inbox), "Inbox (1)");
        let screen = render(&app, 120, 30);
        let first: Vec<char> = screen.lines().next().unwrap().chars().collect();
        let frame = Rect::new(0, 0, 120, 30);
        let spans = tab_spans(&app);
        for (tab, title, r) in &spans {
            let drawn: String = first[r.start as usize..r.end as usize].iter().collect();
            assert_eq!(drawn, format!(" {title} "));
            assert_eq!(tab_hit(&app, frame, r.start, 0), Some(*tab));
            assert_eq!(tab_hit(&app, frame, r.end - 1, 0), Some(*tab));
            // Not on the content row below.
            assert_eq!(tab_hit(&app, frame, r.start, 1), None);
        }
        // The dividers between tabs and the rest of the row hit nothing.
        for w in spans.windows(2) {
            assert_eq!(w[0].2.end + 1, w[1].2.start);
            assert_eq!(tab_hit(&app, frame, w[0].2.end, 0), None);
        }
        assert_eq!(tab_hit(&app, frame, spans.last().unwrap().2.end + 5, 0), None);
    }

    #[test]
    fn view_strip_hits_follow_the_drawn_spans() {
        let counts = [3, 5, 0, 9];
        let line = text(&view_strip(View::NeedsYou, counts, 80));
        let col = |needle: &str| line[..line.find(needle).unwrap()].chars().count() as u16;
        let hit = |x| view_strip_hit(View::NeedsYou, counts, 80, x);
        assert_eq!(hit(0), None, "leading space");
        assert_eq!(hit(col("‹")), Some(StripHit::Prev));
        assert_eq!(hit(col("Needs you")), Some(StripHit::View(View::NeedsYou)));
        assert_eq!(hit(col("Active 5") + 7), Some(StripHit::View(View::Active)), "the count is the view's too");
        assert_eq!(hit(col("Active") - 2), None, "separator");
        assert_eq!(hit(col("Done")), Some(StripHit::View(View::Done)));
        assert_eq!(hit(col("All")), Some(StripHit::View(View::All)));
        assert_eq!(hit(col("›")), Some(StripHit::Next));
        assert_eq!(hit(col("›") + 1), None, "past the end");
        // Narrow: the short names move everything left.
        let narrow = text(&view_strip(View::All, counts, 20));
        let at = narrow.find("Active").map(|b| narrow[..b].chars().count() as u16).unwrap();
        assert_eq!(view_strip_hit(View::All, counts, 20, at), Some(StripHit::View(View::Active)));
    }

    #[test]
    fn card_at_maps_rows_to_cards_past_the_offset() {
        let inner = Rect::new(1, 3, 30, 10);
        assert_eq!(card_at(inner, 0, 5, 2), None, "above");
        assert_eq!(card_at(inner, 0, 5, 3), Some(0));
        assert_eq!(card_at(inner, 0, 5, 4), Some(0));
        assert_eq!(card_at(inner, 0, 5, 5), None, "spacer");
        assert_eq!(card_at(inner, 0, 5, 6), Some(1));
        assert_eq!(card_at(inner, 2, 5, 3), Some(2), "offset");
        assert_eq!(card_at(inner, 2, 5, 9), Some(4));
        assert_eq!(card_at(inner, 2, 5, 12), None, "past the last card");
        assert_eq!(card_at(inner, 0, 5, 13), None, "below");
    }

    #[test]
    fn inbox_hit_lands_on_what_is_drawn() {
        let mut app = inbox_app();
        let frame = Rect::new(0, 0, 120, 30);
        let screen: Vec<String> = render(&app, 120, 30).lines().map(str::to_string).collect();
        // Every card's title row hits that card.
        let threads = app.inbox.threads().to_vec();
        for (pos, &i) in app.inbox.rows().iter().enumerate() {
            let title = title_of(&threads[i]);
            // Within the list's 48 columns (40% of 120): the pane repeats the
            // selected title.
            let in_list = |l: &String| l.chars().take(48).collect::<String>().contains(title.as_str());
            let row = screen.iter().position(in_list).unwrap() as u16;
            assert_eq!(inbox_hit(&app, frame, 5, row), Some(InboxHit::Card(pos)), "{title}");
            assert_eq!(inbox_hit(&app, frame, 5, row + 2), Some(InboxHit::ListBlank), "spacer under {title}");
        }
        // The strip's row: `All` is current; `‹` steps back.
        let strip = &screen[1];
        let col = strip[..strip.find('‹').unwrap()].chars().count() as u16;
        assert_eq!(inbox_hit(&app, frame, col, 1), Some(InboxHit::Strip(StripHit::Prev)));
        // Thread pane (list 40% of 120 = 48 cols): content, then the
        // selected `#7001` takes no replies, so its last inner row is the hint.
        let hint = screen.iter().position(|l| l.contains("this thread takes no replies")).unwrap() as u16;
        assert_eq!(inbox_hit(&app, frame, 60, hint), Some(InboxHit::Hint));
        assert_eq!(inbox_hit(&app, frame, 60, hint - 1), Some(InboxHit::Thread));
        assert_eq!(inbox_hit(&app, frame, 60, 1), Some(InboxHit::Thread), "border");
        assert_eq!(inbox_hit(&app, frame, 60, 0), None, "tab line");
        assert_eq!(inbox_hit(&app, frame, 60, 29), None, "help bar");
        // A thread taking replies: the input box's three rows.
        let sel = app.inbox.rows()[app.selected()];
        let mut inbox = Inbox::default();
        inbox.threads = threads;
        inbox.threads[sel].compose = Some(Default::default());
        app.set_inbox(inbox);
        let screen: Vec<String> = render(&app, 120, 30).lines().map(str::to_string).collect();
        let top = screen.iter().position(|l| l.contains("╭ r reply")).unwrap() as u16;
        for row in top..top + 3 {
            assert_eq!(inbox_hit(&app, frame, 60, row), Some(InboxHit::Input), "row {row}");
        }
        assert_eq!(inbox_hit(&app, frame, 60, top - 1), Some(InboxHit::Thread));
    }

    /// The reply box wraps what's typed, grows with it up to
    /// `textarea::MAX_ROWS` rows then scrolls to the cursor, and hit-testing
    /// sees the same box.
    #[test]
    fn reply_box_wraps_grows_and_follows_the_cursor() {
        let mut app = inbox_app();
        let sel = app.inbox.rows()[app.selected()];
        let mut inbox = Inbox::default();
        inbox.threads = app.inbox.threads().to_vec();
        inbox.threads[sel].compose = Some(Default::default());
        app.set_inbox(inbox);
        app.on_key(KeyEvent::from(KeyCode::Char('r')));
        assert_eq!(app.inbox.focus, InboxFocus::Input);
        let frame = Rect::new(0, 0, 120, 30);
        // The box's rows (borders included), its inner columns, the caret.
        let draw = |app: &App| {
            let mut term = Terminal::new(TestBackend::new(120, 30)).unwrap();
            term.draw(|f| draw(f, app)).unwrap();
            let buf = term.backend().buffer().clone();
            let screen: Vec<String> =
                (0..30).map(|y| (0..120).map(|x| buf[(x, y)].symbol().to_string()).collect()).collect();
            let top = screen.iter().position(|l| l.contains("╭ enter sends")).unwrap();
            let bottom = top + screen[top..].iter().position(|l| l.contains('╰')).unwrap();
            let border = &screen[top];
            let left = border.chars().position(|c| c == '╭').unwrap();
            let right = border.chars().position(|c| c == '╮').unwrap();
            let rows: Vec<String> =
                screen[top + 1..bottom].iter().map(|l| l.chars().skip(left + 1).take(right - left - 1).collect()).collect();
            let caret = term.get_cursor_position().unwrap();
            (top as u16, bottom as u16, left as u16 + 1, rows, (caret.x, caret.y))
        };
        let typed = |app: &mut App, s: &str| s.chars().for_each(|c| app.on_key(KeyEvent::from(KeyCode::Char(c))));

        // Empty: one row, the placeholder.
        let (_, _, _, rows, _) = draw(&app);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].starts_with("reply to the owner"), "{rows:?}");

        // Longer than a row: wrapped, nothing lost, caret after the last char.
        let (_, _, x0, rows, _) = draw(&app);
        let width = rows[0].chars().count();
        let text = format!("start{}END", "x".repeat(width * 2));
        typed(&mut app, &text);
        let (top, bottom, _, rows, caret) = draw(&app);
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert!(rows[0].starts_with("start"));
        assert_eq!(rows.concat().trim_end(), text);
        let end = rows[2].trim_end().chars().count() as u16;
        assert_eq!(caret, (x0 + end, top + 3));
        for row in top..=bottom {
            assert_eq!(inbox_hit(&app, frame, x0 + 1, row), Some(InboxHit::Input), "row {row}");
        }
        assert_eq!(inbox_hit(&app, frame, x0 + 1, top - 1), Some(InboxHit::Thread));

        // A newline (alt-enter) starts a row; plain enter would send.
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        typed(&mut app, "next");
        let (_, _, _, rows, _) = draw(&app);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[3].trim_end(), "next");

        // Past the cap: six rows, scrolled so the caret's row is the last.
        for _ in 0..5 {
            app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        }
        typed(&mut app, "tail");
        let (top, bottom, _, rows, caret) = draw(&app);
        assert_eq!(rows.len(), textarea::MAX_ROWS as usize);
        assert_eq!(rows[5].trim_end(), "tail");
        assert!(!rows.iter().any(|r| r.starts_with("start")), "scrolled past the top");
        assert_eq!(caret, (x0 + 4, top + 6));
        assert_eq!(inbox_hit(&app, frame, x0 + 1, bottom), Some(InboxHit::Input));
        assert_eq!(inbox_hit(&app, frame, x0 + 1, top), Some(InboxHit::Input));

        // Enter sends and the box shrinks back to one row.
        app.on_key(KeyEvent::from(KeyCode::Enter));
        let (_, _, _, rows, _) = draw(&app);
        assert_eq!(rows.len(), 1);
    }

    /// The `:` prompt scrolls sideways so the caret stays on screen.
    #[test]
    fn prompt_scrolls_to_the_caret() {
        let mut app = App::new(PathBuf::from("/tmp"));
        app.on_key(KeyEvent::from(KeyCode::Char(':')));
        let text = format!("exec {}Z", "a".repeat(60));
        text.chars().for_each(|c| app.on_key(KeyEvent::from(KeyCode::Char(c))));
        let mut term = Terminal::new(TestBackend::new(40, 10)).unwrap();
        term.draw(|f| draw(f, &app)).unwrap();
        let buf = term.backend().buffer().clone();
        let line: String = (0..40).map(|x| buf[(x, 8)].symbol().to_string()).collect();
        assert!(line.starts_with(": "), "{line:?}");
        assert!(line.trim_end().ends_with('Z'), "{line:?}");
        let caret = term.get_cursor_position().unwrap();
        assert_eq!((caret.x, caret.y), (39, 8));
    }

    const LLM_MESSAGE: &str = "## Review summary

I checked **4 comments** from Greptile:

- `parse_port` overflows on `0`
- the retry loop never backs off
  - nested: also in `connect`

```rust
fn parse(s: &str) -> u16 {
\ts.parse().unwrap()
}
```

> CI is green on the branch.

See [PR 6900](https://github.com/o/r/pull/6900).";

    /// The Inbox showing one thread with `message`, its pane focused.
    fn message_app(message: &str) -> App {
        let mut t = dthread("#6900 fix `parse_port`", State::NeedsYou, Some("review **drafts**"));
        t.id = 1;
        t.owner = "bab-disp-id".into();
        t.feed = vec![crate::inbox::FeedItem::markdown(1, 0, "m", message)];
        let mut inbox = Inbox::default();
        inbox.threads = vec![t];
        let mut app = App::new(PathBuf::from("/tmp"));
        app.set_inbox(inbox);
        app.on_key(KeyEvent::from(KeyCode::Char('4')));
        app.on_key(KeyEvent::from(KeyCode::Enter));
        app
    }

    fn draw_buffer(app: &App, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        term.backend().buffer().clone()
    }

    fn rows_of(buf: &ratatui::buffer::Buffer) -> Vec<String> {
        let a = buf.area;
        (0..a.height).map(|y| (0..a.width).map(|x| buf[(x, y)].symbol().to_string()).collect()).collect()
    }

    /// Where `needle` starts on screen, as a cell position (wide-glyph-free
    /// rows, so a char index is a column).
    fn find_cell(rows: &[String], needle: &str) -> (u16, u16) {
        let y = rows.iter().position(|r| r.contains(needle)).unwrap_or_else(|| panic!("{needle}:\n{}", rows.join("\n")));
        let x = rows[y][..rows[y].find(needle).unwrap()].chars().count();
        (x as u16, y as u16)
    }

    #[test]
    fn thread_pane_renders_a_typical_llm_message() {
        let app = message_app(LLM_MESSAGE);
        let buf = draw_buffer(&app, 100, 34);
        let rows = rows_of(&buf);
        if std::env::var_os("SHOW_INBOX").is_some() {
            println!("{}", rows.join("\n"));
        }
        let screen = rows.join("\n");
        for gone in ["##", "**", "```", "`parse_port`", "](https"] {
            assert!(!screen.contains(gone), "{gone} in\n{screen}");
        }
        let style = |needle: &str| buf[find_cell(&rows, needle)].style();
        let heading = style("Review summary");
        assert_eq!((heading.fg, heading.add_modifier.contains(Modifier::BOLD)), (Some(ACCENT), true));
        assert!(style("4 comments").add_modifier.contains(Modifier::BOLD));
        assert_eq!(style("parse_port overflows").bg, markdown::CODE.bg);
        assert!(screen.contains("• parse_port overflows on 0"), "{screen}");
        assert!(screen.contains("  • nested: also in connect"), "{screen}");
        // The code block: tinted, its tab expanded, padded across the pane.
        let (x, y) = find_cell(&rows, "fn parse(s: &str)");
        assert_eq!(buf[(x, y)].style().bg, markdown::CODE.bg);
        assert!(rows[y as usize + 1].contains("    s.parse().unwrap()"), "{screen}");
        assert_eq!(buf[(98 - 1, y)].style().bg, markdown::CODE.bg, "padded to the pane's inner edge");
        assert!(screen.contains("│ CI is green on the branch."), "{screen}");
        assert!(style("PR 6900").add_modifier.contains(Modifier::UNDERLINED));
        assert!(screen.contains("See PR 6900 (https://github.com/o/r/pull/6900)."), "{screen}");
        // Title and status are inline markdown too (the status only shows in
        // the pane; the title also on the card, left of column 40).
        assert!(style("drafts").add_modifier.contains(Modifier::BOLD));
        let y = rows.iter().position(|r| r.chars().skip(40).collect::<String>().contains("#6900 fix parse_port")).unwrap();
        let x = 40 + rows[y].chars().skip(40).collect::<String>().find("parse_port").unwrap() as u16;
        assert_eq!(buf[(x, y as u16)].style().bg, markdown::CODE.bg);
    }

    #[test]
    fn thread_pane_lays_out_a_table() {
        let mut app = message_app("Checks:\n\n| check | took | result |\n|:--|--:|:-:|\n| `cargo test` | 41s | ok |\n| e2e | 3m | failing |");
        let buf = draw_buffer(&app, 100, 20);
        let rows = rows_of(&buf);
        if std::env::var_os("SHOW_INBOX").is_some() {
            println!("{}", rows.join("\n"));
        }
        let screen = rows.join("\n");
        let (x, y) = find_cell(&rows, "check       took  result");
        assert!(buf[(x, y)].style().add_modifier.contains(Modifier::BOLD), "bold header");
        let rule = &rows[y as usize + 1];
        assert_eq!(rule.chars().skip(x as usize).take_while(|c| *c == '─').count(), 25, "{screen}");
        assert!(buf[(x, y + 1)].style().add_modifier.contains(Modifier::DIM), "dim rule");
        let (cx, cy) = find_cell(&rows, "cargo test   41s    ok");
        assert_eq!((cx, cy), (x, y + 2), "{screen}");
        assert_eq!(buf[(cx, cy)].style().bg, markdown::CODE.bg, "inline code keeps its tint");
        assert_eq!(find_cell(&rows, "e2e           3m  failing"), (x, y + 3), "{screen}");
        // Raw: the source.
        app.on_key(KeyEvent::from(KeyCode::Char('m')));
        assert!(render(&app, 100, 20).contains("| `cargo test` | 41s | ok |"));
    }

    #[test]
    fn m_toggles_the_raw_source() {
        let mut app = message_app(LLM_MESSAGE);
        app.on_key(KeyEvent::from(KeyCode::Char('m')));
        assert!(app.inbox.raw);
        let screen = render(&app, 100, 34);
        assert!(screen.contains("╭ Thread · raw "), "{screen}");
        assert!(screen.contains("## Review summary"), "{screen}");
        assert!(screen.contains("I checked **4 comments** from Greptile:"), "{screen}");
        assert!(screen.contains("```rust"), "{screen}");
        assert!(screen.contains("review **drafts**"), "{screen}");
        app.on_key(KeyEvent::from(KeyCode::Char('m')));
        assert!(!render(&app, 100, 34).contains("```rust"));
    }

    /// The scroll bound is the rows drawn: scrolled to the bottom, the last
    /// line sits on the pane's last content row, right above the hint.
    #[test]
    fn the_pane_scroll_bound_is_exact() {
        let long: String = (1..=40).map(|i| format!("- item {i} with a few words to wrap\n")).collect();
        let table = "| a | b |\n|---|--:|\n| 1 | 2 |\n| `x` | 3 |\n";
        let mut app = message_app(&format!("{table}\n{long}\n{table}\nTHE END"));
        // Draw once so the bound is known, then jump to the bottom.
        render(&app, 80, 24);
        app.on_key(KeyEvent::from(KeyCode::Char('G')));
        let rows: Vec<String> = render(&app, 80, 24).lines().map(str::to_string).collect();
        let (_, end) = find_cell(&rows, "THE END");
        let (_, hint) = find_cell(&rows, "this thread takes no replies");
        assert_eq!(end + 1, hint, "{}", rows.join("\n"));
        // One more line down is not possible.
        let scroll = app.inbox.scroll;
        app.on_key(KeyEvent::from(KeyCode::Down));
        assert_eq!(app.inbox.scroll, scroll);
    }

    /// One thread of `n` messages (`message 1` oldest), a link, a key, two
    /// actions and `compose`, its pane focused.
    fn feed_app(n: u64, compose: Option<crate::inbox::Compose>) -> App {
        let mut t = dthread("#6900 feat/agent-run-cost-event", State::NeedsYou, Some("review drafts"));
        t.id = 1;
        t.owner = "bab-disp-id".into();
        t.key = Some("pr-6900".into());
        t.link = Some("https://x/pr/6900".into());
        t.actions = vec![
            crate::inbox::Action { id: "retry".into(), label: "Retry".into(), ..Default::default() },
            crate::inbox::Action { id: "done".into(), label: "Done".into(), done: true, ..Default::default() },
        ];
        t.feed = (1..=n)
            .map(|i| crate::inbox::FeedItem::markdown(i, i * 60, &format!("m{i}"), &format!("message {i}")))
            .collect();
        t.compose = compose;
        let mut inbox = Inbox::default();
        inbox.threads = vec![t];
        let mut app = App::new(PathBuf::from("/tmp"));
        app.set_inbox(inbox);
        app.on_key(KeyEvent::from(KeyCode::Char('4')));
        app.on_key(KeyEvent::from(KeyCode::Enter));
        app
    }

    /// The thread pane's columns of a 100-wide frame's row (list 40%).
    fn pane_part(row: &str) -> String {
        row.chars().skip(40).collect()
    }

    // At 100x24: the pane's inner area is columns 41-98, rows 2-21.
    #[test]
    fn the_header_stays_put_while_the_feed_scrolls_newest_first() {
        let mut app = feed_app(20, None);
        let rows = rows_of(&draw_buffer(&app, 100, 24));
        if std::env::var_os("SHOW_INBOX").is_some() {
            println!("{}", rows.join("\n"));
        }
        let pane: Vec<String> = rows.iter().map(|r| pane_part(r)).collect();
        assert!(pane[2].starts_with("│#6900 feat/agent-run-cost-event ↗ "), "{}", pane.join("\n"));
        assert!(pane[3].starts_with("│● review drafts  ·  ") && pane[3].ends_with(" bab-disp · pr-6900│"), "{:?}", pane[3]);
        // The action row wraps between buttons: 58 columns hold five.
        assert_eq!(pane[4], format!("│{:58}│", "[1] Retry  [2] Done ✓  [o] VS Code  [t] Terminal  [l] Logs"));
        assert_eq!(pane[5], format!("│{:58}│", "[p] Port"));
        assert_eq!(pane[6], format!("├{}┤", "─".repeat(58)), "separator joined to the border");
        // Newest first: scroll 0 shows the latest message right under it.
        assert!(pane[7].trim_end_matches('│').trim_end().ends_with("  bab-disp"), "{:?}", pane[7]);
        assert!(pane[8].contains("message 20"), "{:?}", pane[8]);
        let header: Vec<String> = pane[1..=6].to_vec();

        // Scrolled to the end: the oldest message is in view, the header the same.
        app.on_key(KeyEvent::from(KeyCode::Char('G')));
        assert!(app.inbox.scroll > 0);
        let pane: Vec<String> = rows_of(&draw_buffer(&app, 100, 24)).iter().map(|r| pane_part(r)).collect();
        assert_eq!(pane[1..=6], header[..], "pinned");
        assert!(!pane.iter().any(|r| r.contains("message 20")));
        // The last feed row sits right above the no-replies hint.
        assert!(pane[20].contains("message 1 "), "{}", pane.join("\n"));
        assert!(pane[21].contains("this thread takes no replies"));

        // Header rows and the separator hit the thread, not the input.
        let frame = Rect::new(0, 0, 100, 24);
        for row in 2..=7 {
            assert_eq!(inbox_hit(&app, frame, 60, row), Some(InboxHit::Thread), "row {row}");
        }
        assert_eq!(inbox_hit(&app, frame, 60, 21), Some(InboxHit::Hint));
        // The wheel over the header scrolls the feed like over the feed.
        let max = app.inbox.scroll;
        let wheel = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::ScrollUp,
            column: 60,
            row: 3,
            modifiers: KeyModifiers::NONE,
        };
        app.on_mouse(&wheel, frame);
        assert_eq!(app.inbox.scroll, max - 3);
    }

    #[test]
    fn the_composer_draws_its_hint_under_the_box_and_hits_agree() {
        let hint = "Starts an agent with your message, then waits for it to finish and posts the result";
        let compose = crate::inbox::Compose {
            placeholder: Some("Instructions for this PR…".into()),
            hint: Some(hint.into()),
        };
        let mut app = feed_app(3, Some(compose));
        let frame = Rect::new(0, 0, 100, 24);
        let check = |app: &App, box_rows: u16| {
            let pane: Vec<String> = rows_of(&draw_buffer(app, 100, 24)).iter().map(|r| pane_part(r)).collect();
            // The hint is the last inner row, cut to the width with `…`.
            assert_eq!(pane[21], format!("│{}│", truncate(hint, 58)), "{}", pane.join("\n"));
            assert!(pane[21].contains('…'));
            // The box right above it: border + rows + border.
            let top = 21 - box_rows - 2;
            assert!(pane[top as usize].starts_with("│╭"), "{}", pane.join("\n"));
            assert!(pane[20].starts_with("│╰"), "{}", pane.join("\n"));
            for row in top..=21 {
                assert_eq!(inbox_hit(app, frame, 60, row), Some(InboxHit::Input), "row {row}");
            }
            assert_eq!(inbox_hit(app, frame, 60, top - 1), Some(InboxHit::Thread));
            assert_eq!(inbox_hit(app, frame, 60, 3), Some(InboxHit::Thread), "header");
            pane
        };
        let pane = check(&app, 1);
        assert!(pane[19].contains("Instructions for this PR…"), "{}", pane.join("\n"));

        // Typing grows the box; the hint stays under it, hits follow.
        app.on_key(KeyEvent::from(KeyCode::Char('r')));
        let typed = "y".repeat(56 * 2 + 5);
        typed.chars().for_each(|c| app.on_key(KeyEvent::from(KeyCode::Char(c))));
        check(&app, 3);
        // A click on the hint row keeps the typed line (it's the composer).
        app.on_mouse(
            &crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: 60,
                row: 21,
                modifiers: KeyModifiers::NONE,
            },
            frame,
        );
        assert_eq!(app.inbox.focus, InboxFocus::Input);
        assert_eq!(app.inbox.reply.as_ref().unwrap().line.input(), typed);
    }

    /// [`feed_app`] whose newest message carries an open form of `n`
    /// questions: the first a choice with context, the rest confirms.
    fn form_feed_app(n: usize) -> App {
        use crate::inbox::form::{ChoiceOption, Form, Question, QuestionKind};
        let mut app = feed_app(3, None);
        let mut questions = vec![Question {
            id: "c-1".into(),
            label: "greptile on `src/cost.ts:42`".into(),
            context: Some("> Consider batching these writes.".into()),
            required: true,
            kind: QuestionKind::Choice {
                options: vec![
                    ChoiceOption { id: "post".into(), label: "Post the reply".into(), description: None },
                    ChoiceOption { id: "skip".into(), label: "Don't reply".into(), description: None },
                ],
                multiple: false,
                default: Some(crate::inbox::form::ChoiceDefault::One("post".into())),
            },
        }];
        questions.extend((2..=n).map(|i| Question {
            id: format!("q-{i}"),
            label: format!("Question {i}?"),
            context: None,
            required: false,
            kind: QuestionKind::Confirm { yes: None, no: None, default: None },
        }));
        let form = Form { id: "drafts".into(), title: Some("Replies to post".into()), submit: "Post".into(), questions };
        let mut t = app.selected_inbox_thread().unwrap().clone();
        let blocks = vec![crate::inbox::Block::Form(form)];
        let record = crate::inbox::FormRecord::open("drafts");
        let kind = crate::inbox::ItemKind::Message { id: "m4".into(), blocks, edited: false, withdrawn: false, form: Some(record) };
        t.feed.push(crate::inbox::FeedItem { seq: 4, at: 240, kind });
        let mut inbox = Inbox::default();
        inbox.threads = vec![t];
        app.set_inbox(inbox);
        app
    }

    fn press(app: &mut App, frame: Rect, kind: crossterm::event::MouseEventKind, row: u16) {
        app.on_mouse(&crossterm::event::MouseEvent { kind, column: 60, row, modifiers: KeyModifiers::NONE }, frame);
    }

    // At 100x24: the pane's inner area is columns 41-98, rows 2-21; the
    // header takes rows 2-5, the separator row 6.
    #[test]
    fn open_forms_are_pinned_between_the_header_and_the_feed_and_hits_agree() {
        let mut app = form_feed_app(2);
        let frame = Rect::new(0, 0, 100, 24);
        let pane: Vec<String> = rows_of(&draw_buffer(&app, 100, 24)).iter().map(|r| pane_part(r)).collect();
        if std::env::var_os("SHOW_INBOX").is_some() {
            println!("{}", pane.join("\n"));
        }
        assert_eq!(pane[6], format!("├{}┤", "─".repeat(58)));
        assert!(pane[7].starts_with("│╭ Replies to post ─") && pane[7].ends_with("─ open form ╮│"), "{:?}", pane[7]);
        assert_eq!(pane[8], format!("││ {:54} ││", "  1/2 greptile on src/cost.ts:42"));
        assert_eq!(pane[9], format!("││ {:54} ││", "    (•) Post the reply   ( ) Don't reply"));
        assert_eq!(pane[10], format!("││ {:54} ││", "  2/2 Question 2?"));
        assert_eq!(pane[12], format!("││ {:54} ││", "[Tab] next  [Space] pick  [e] edit  [Enter] Confirm"));
        assert_eq!(pane[13], format!("│╰{}╯│", "─".repeat(56)));
        // The feed right under it, newest first: the form's message.
        assert!(pane[14].trim_end_matches('│').trim_end().ends_with("  bab-disp"), "{}", pane.join("\n"));
        assert!(pane[15].contains("form: Replies to post · open, pinned above"), "{}", pane.join("\n"));

        // Hits: the box is the form zone, by question; around it the thread.
        let spot = |question| Some(InboxHit::Form(Some(FormSpot { form: 0, question })));
        assert_eq!(inbox_hit(&app, frame, 60, 6), Some(InboxHit::Thread));
        assert_eq!(inbox_hit(&app, frame, 60, 7), spot(None));
        assert_eq!(inbox_hit(&app, frame, 60, 8), spot(Some(0)));
        assert_eq!(inbox_hit(&app, frame, 60, 11), spot(Some(1)));
        assert_eq!(inbox_hit(&app, frame, 60, 12), spot(None));
        assert_eq!(inbox_hit(&app, frame, 60, 14), Some(InboxHit::Thread));

        // A click on question 2 focuses it; the box is accented, the focused
        // question marked; question 1's context stays folded.
        press(&mut app, frame, crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left), 11);
        assert_eq!(app.inbox.focus, InboxFocus::Form);
        assert_eq!(app.inbox.form.as_ref().map(|e| e.question), Some(1));
        let pane: Vec<String> = rows_of(&draw_buffer(&app, 100, 24)).iter().map(|r| pane_part(r)).collect();
        assert_eq!(pane[10], format!("││ {:54} ││", "› 2/2 Question 2?"));
        // Back to question 1: its context shows, the box grows a row.
        app.on_key(KeyEvent::from(KeyCode::Tab));
        let pane: Vec<String> = rows_of(&draw_buffer(&app, 100, 24)).iter().map(|r| pane_part(r)).collect();
        assert!(pane[9].contains("Consider batching these writes."), "{}", pane.join("\n"));
        assert!(pane[14].starts_with("│╰"), "{}", pane.join("\n"));
    }

    #[test]
    fn a_tall_form_takes_half_the_pane_and_scrolls_to_the_cursor() {
        let mut app = form_feed_app(12);
        let frame = Rect::new(0, 0, 100, 24);
        let pane: Vec<String> = rows_of(&draw_buffer(&app, 100, 24)).iter().map(|r| pane_part(r)).collect();
        // Half of the 20 inner rows: 7-16, then the feed.
        assert!(pane[7].starts_with("│╭ Replies to post"), "{}", pane.join("\n"));
        assert!(pane[17].trim_end_matches('│').trim_end().ends_with("  bab-disp"), "{}", pane.join("\n"));
        assert_eq!(inbox_hit(&app, frame, 60, 16), Some(InboxHit::Form(Some(FormSpot { form: 0, question: Some(4) }))));
        assert_eq!(inbox_hit(&app, frame, 60, 17), Some(InboxHit::Thread));
        // Focus the last question: the box scrolls to it.
        app.on_key(KeyEvent::from(KeyCode::Tab));
        app.on_key(KeyEvent::from(KeyCode::BackTab));
        let pane: Vec<String> = rows_of(&draw_buffer(&app, 100, 24)).iter().map(|r| pane_part(r)).collect();
        assert!(pane[7..=16].iter().any(|r| r.contains("› 12/12 Question 12?")), "{}", pane.join("\n"));
        assert!(!pane[7..=16].iter().any(|r| r.contains("Replies to post")), "scrolled past the top border");
        let scrolled = app.inbox.forms_scroll();
        assert!(scrolled > 0);
        // The wheel over the box scrolls it, and the cursor doesn't pull it back.
        press(&mut app, frame, crossterm::event::MouseEventKind::ScrollUp, 10);
        assert_eq!(app.inbox.forms_scroll(), scrolled - 3);
        draw_buffer(&app, 100, 24);
        assert_eq!(app.inbox.forms_scroll(), scrolled - 3);
        // A text-free form: `e` does nothing to the composer.
        app.on_key(KeyEvent::from(KeyCode::Char('e')));
        assert_eq!(inbox_hit(&app, frame, 60, 21), Some(InboxHit::Hint));
    }

    #[test]
    fn header_lines_cut_align_and_wrap() {
        let line = |left: &str, right: Option<&str>| HeaderRow::Line {
            left: vec![(Tone::Title, left.to_string())],
            right: right.map(str::to_string),
        };
        let texts = |rows: &[HeaderRow], w: u16| header_lines(rows, w, false).iter().map(text).collect::<Vec<_>>();
        // Right-aligned when both fit; the left cut first, then the right dropped.
        assert_eq!(texts(&[line("chip", Some("owner · key"))], 20), ["chip     owner · key"]);
        assert_eq!(texts(&[line("a long chip line", Some("owner"))], 20), ["a long chip l… owner"]);
        assert_eq!(texts(&[line("a long chip line", Some("owner"))], 10), ["a long ch…"]);
        // Titles are one row, inline markdown rendered before cutting.
        assert_eq!(texts(&[line("fix `parse_port` everywhere", None)], 16), ["fix parse_port …"]);
        let b = |s: &str| vec![(Tone::Plain, s.to_string())];
        let buttons = HeaderRow::Buttons(vec![b("[1] Retry"), b("[2] Done"), b("[o] VS Code")]);
        assert_eq!(texts(std::slice::from_ref(&buttons), 40), ["[1] Retry  [2] Done  [o] VS Code"]);
        assert_eq!(texts(std::slice::from_ref(&buttons), 20), ["[1] Retry  [2] Done", "[o] VS Code"]);
        // A button wider than the pane: cut on its own row.
        assert_eq!(texts(std::slice::from_ref(&buttons), 6), ["[1] R…", "[2] D…", "[o] V…"]);
    }

    #[test]
    fn a_card_title_renders_inline_code_without_backticks() {
        let mut t = dthread("fix `parse_port` *now*", State::NeedsYou, None);
        t.updated_at = 1000;
        let [l1, _] = card_lines(&t, 40, false, true, 1000, 0);
        assert!(text(&l1).starts_with(" fix parse_port now  ") && text(&l1).ends_with(" now "), "{l1:?}");
        assert_eq!(l1.width(), 40);
        let code = l1.spans.iter().find(|s| s.content == "parse_port").unwrap();
        assert_eq!(code.style.bg, markdown::CODE.bg);
        assert!(l1.spans.iter().find(|s| s.content == "now").unwrap().style.add_modifier.contains(Modifier::ITALIC));
        // Truncated, the code keeps its tint up to the `…`.
        let [cut, _] = card_lines(&t, 12, false, true, 1000, 0);
        assert_eq!(text(&cut), " fix parse… ");
        assert_eq!(cut.spans.iter().find(|s| s.content == "parse…").unwrap().style.bg, markdown::CODE.bg);
    }

    #[test]
    fn tiny_inbox_keeps_the_selected_card_visible() {
        let mut app = inbox_app();
        for _ in 0..4 {
            app.on_key(KeyEvent::from(KeyCode::Down));
        }
        let screen = render(&app, 30, 8);
        assert!(screen.contains("▌image"), "{screen}");
        // Back up to the top: the window follows.
        for _ in 0..4 {
            app.on_key(KeyEvent::from(KeyCode::Up));
        }
        let screen = render(&app, 30, 8);
        assert!(screen.contains("▌#6"), "{screen}");
    }
}
