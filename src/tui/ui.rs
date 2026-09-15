//! Immediate-mode rendering for the dashboard. Layout: tab bar on top, content
//! area in the middle, help bar at the bottom. The Instances tab renders real
//! data from the latest snapshot.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Tabs};

use super::app::{App, ConfigView, Modal, Side, Tab, TextModal};
use super::data::{
    humanize_secs, sandbox_stats, totals_line, ContainerStatus, InstanceRow, Node, SandboxRow,
    ServiceRow, Snapshot,
};
use super::prompt::Prompt;

const HIGHLIGHT: Color = Color::Cyan;

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
    let tabs = Tabs::new(Tab::ALL.iter().map(|t| t.title()))
        .select(selected)
        .style(Style::default())
        .highlight_style(Style::default().fg(HIGHLIGHT).add_modifier(Modifier::BOLD))
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
    }
}

fn draw_services(frame: &mut Frame, app: &App, area: Rect) {
    let snapshot = app.snapshot.as_ref();
    let rows: &[ServiceRow] = snapshot.map_or(&[], |s| s.services.as_slice());

    let error = snapshot.and_then(|s| s.error.as_deref());
    let [err_area, table_area, detail_area] = Layout::vertical([
        Constraint::Length(if error.is_some() { 1 } else { 0 }),
        Constraint::Min(0),
        Constraint::Length(9),
    ])
    .areas(area);

    if let Some(err) = error {
        let line = Line::from(Span::styled(err.to_string(), Style::default().fg(Color::Red)));
        frame.render_widget(Paragraph::new(line), err_area);
    }

    if rows.is_empty() {
        draw_services_empty(frame, table_area);
        draw_service_detail(frame, None, detail_area);
        return;
    }

    draw_services_table(frame, app, rows, table_area);
    draw_service_detail(frame, rows.get(app.selected()), detail_area);
}

fn draw_services_empty(frame: &mut Frame, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(HIGHLIGHT))
        .title(Tab::Services.title());
    let text = Line::from(Span::styled(
        "no services defined in config.toml",
        Style::default().add_modifier(Modifier::DIM),
    ))
    .alignment(Alignment::Center);
    frame.render_widget(Paragraph::new(text).block(block), area);
}

fn draw_services_table(frame: &mut Frame, app: &App, rows: &[ServiceRow], area: Rect) {
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
        .border_style(Style::default().fg(HIGHLIGHT))
        .title(Tab::Services.title());

    let table = Table::new(table_rows, widths)
        .header(header)
        .block(block)
        .column_spacing(1)
        .row_highlight_style(Style::default().fg(HIGHLIGHT).add_modifier(Modifier::BOLD));

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
    Row::new(vec![
        Cell::from(r.name.clone()),
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
/// (green) when any are up, else `N exited` (red) when any exist, else `-` dim.
fn service_status_cell(r: &ServiceRow) -> Cell<'static> {
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

fn draw_service_detail(frame: &mut Frame, row: Option<&ServiceRow>, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(HIGHLIGHT))
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

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_instances(frame: &mut Frame, app: &App, area: Rect) {
    let snapshot = app.snapshot.as_ref();
    let nodes = app.visible_nodes();

    // Optional error line reserved above the table; detail panel below it.
    let error = snapshot.and_then(|s| s.error.as_deref());
    let [err_area, table_area, detail_area] = Layout::vertical([
        Constraint::Length(if error.is_some() { 1 } else { 0 }),
        Constraint::Min(0),
        Constraint::Length(9),
    ])
    .areas(area);

    if let Some(err) = error {
        let line = Line::from(Span::styled(
            err.to_string(),
            Style::default().fg(Color::Red),
        ));
        frame.render_widget(Paragraph::new(line), err_area);
    }

    if nodes.is_empty() {
        draw_empty(frame, table_area);
        draw_detail(frame, snapshot, None, detail_area);
        return;
    }

    let snapshot = snapshot.expect("non-empty nodes imply a snapshot");
    draw_tree(frame, app, snapshot, &nodes, table_area);
    let selected = nodes.get(app.selected()).copied();
    draw_detail(frame, Some(snapshot), selected, detail_area);
}

fn draw_empty(frame: &mut Frame, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(HIGHLIGHT))
        .title(Tab::Instances.title());
    let text = Line::from(Span::styled(
        "no sandboxes defined — check config.toml",
        Style::default().add_modifier(Modifier::DIM),
    ))
    .alignment(Alignment::Center);
    let paragraph = Paragraph::new(text).block(block);
    frame.render_widget(paragraph, area);
}

fn draw_tree(frame: &mut Frame, app: &App, snapshot: &Snapshot, nodes: &[Node], area: Rect) {
    let header = Row::new(
        ["TREE", "STATUS", "UPTIME", "CPU", "MEM", "FOLDER", "SERVICES"]
            .into_iter()
            .map(Cell::from),
    )
    .style(Style::default().add_modifier(Modifier::DIM));

    let table_rows: Vec<Row> = nodes
        .iter()
        .map(|node| tree_row(app, snapshot, *node))
        .collect();

    let widths = [
        Constraint::Min(20),
        Constraint::Length(9),
        Constraint::Length(7),
        Constraint::Length(7),
        Constraint::Length(16),
        Constraint::Min(20),
        Constraint::Min(12),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(HIGHLIGHT))
        .title(Tab::Instances.title());

    let table = Table::new(table_rows, widths)
        .header(header)
        .block(block)
        .column_spacing(1)
        .row_highlight_style(
            Style::default().fg(HIGHLIGHT).add_modifier(Modifier::BOLD),
        );

    let mut state = TableState::default().with_selected(Some(app.selected()));
    frame.render_stateful_widget(table, area, &mut state);
}

/// Render one tree node into a table row. Sandbox / orphan-group rows carry their
/// stats in the STATUS column and leave the instance columns blank; instance
/// rows fill the columns and indent the TREE cell.
fn tree_row<'a>(app: &App, snapshot: &'a Snapshot, node: Node) -> Row<'a> {
    match node {
        Node::Sandbox(i) => match snapshot.sandboxes.get(i) {
            Some(sb) => sandbox_tree_row(app, snapshot, sb),
            None => Row::new(vec![Cell::from("")]),
        },
        Node::Instance(i) => match snapshot.instances.get(i) {
            Some(inst) => instance_tree_row(inst),
            None => Row::new(vec![Cell::from("")]),
        },
        Node::Empty(i) => {
            let name = snapshot.sandboxes.get(i).map_or("", |s| s.name.as_str());
            Row::new(vec![Cell::from(Span::styled(
                format!("    no instances — : run {name}"),
                Style::default().add_modifier(Modifier::DIM),
            ))])
        }
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
            Style::default().fg(HIGHLIGHT).add_modifier(Modifier::BOLD),
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

fn instance_tree_row(r: &InstanceRow) -> Row<'_> {
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
    Row::new(vec![
        Cell::from(format!("  {}", r.name)),
        status,
        Cell::from(humanize_secs(r.uptime_secs)),
        Cell::from(r.cpu.clone().unwrap_or_else(|| "-".to_string())),
        Cell::from(r.mem.clone().unwrap_or_else(|| "-".to_string())),
        Cell::from(folder),
        Cell::from(services),
    ])
}

fn status_style(status: &ContainerStatus) -> Style {
    match status {
        ContainerStatus::Running(_) => Style::default().fg(Color::Green),
        ContainerStatus::Exited(_) => Style::default().fg(Color::Red),
        ContainerStatus::Missing => Style::default().add_modifier(Modifier::DIM),
    }
}

/// Detail panel for the selected tree node: sandbox summary for sandbox / empty
/// nodes, the existing instance detail for instances, and a hint for the orphan
/// group. Empty when nothing is selected.
fn draw_detail(frame: &mut Frame, snapshot: Option<&Snapshot>, node: Option<Node>, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(HIGHLIGHT))
        .title("Detail");

    let lines: Vec<Line> = match (snapshot, node) {
        (Some(s), Some(Node::Sandbox(i))) | (Some(s), Some(Node::Empty(i))) => {
            match s.sandboxes.get(i) {
                Some(sb) => sandbox_detail(s, sb),
                None => Vec::new(),
            }
        }
        (Some(s), Some(Node::Instance(i))) => match s.instances.get(i) {
            Some(r) => instance_detail(r),
            None => Vec::new(),
        },
        (_, Some(Node::Orphans)) => vec![Line::from(Span::styled(
            "instances whose sandbox is no longer in config.toml",
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
    lines
}

fn instance_detail(r: &InstanceRow) -> Vec<Line<'_>> {
    let mut lines: Vec<Line> = vec![
        kv("container", &r.container),
        kv("workspace", &r.workspace),
        kv("remoteUser", r.remote_user.as_deref().unwrap_or("-")),
        kv("remoteEnv", &r.remote_env_len.to_string()),
    ];
    if r.worktree {
        lines.push(kv2("base", &r.base_folder, "worktree", &r.folder));
    } else {
        lines.push(kv("base folder", &r.base_folder));
    }
    if r.drift {
        lines.push(Line::from(Span::styled(
            "config drift: container was created from an older config",
            Style::default().fg(Color::Yellow),
        )));
    }
    lines
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
    let text = match app.modal {
        Modal::Config(_) => "tab original/resolved · ↑↓ scroll · pgup/pgdn · g/G · esc close",
        Modal::Help(_) => "↑↓ scroll · pgup/pgdn · g/G · esc/? close",
        Modal::Logs(_) => "↑↓ scroll · pgup/pgdn · g/G · esc close",
        Modal::None => match app.tab {
            Tab::Instances => "q quit · tab switch · ↑↓ select · ←→ fold · enter config · r run · o vscode · l logs · : cmd · ? help",
            Tab::Services => "q quit · tab switch · ↑↓ select · enter config · : cmd · ? help",
        },
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
        Span::styled(prefix, Style::default().fg(HIGHLIGHT)),
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
            Style::default().fg(HIGHLIGHT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        spans.push(Span::styled(cand.clone(), style));
    }
    Line::from(spans)
}

/// Render the config explorer full-screen over the dashboard.
fn draw_config_modal(frame: &mut Frame, view: &ConfigView) {
    let area = frame.area();

    let (side, other) = match view.showing {
        Side::Original => ("original", "resolved"),
        Side::Resolved => ("resolved", "original"),
    };
    let hash = if view.hash.is_empty() {
        String::new()
    } else {
        format!(" — {}", view.hash)
    };
    let title = format!(" {} — {side} (tab: {other}){hash} ", view.title);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(HIGHLIGHT))
        .title(title);

    let lines: Vec<Line> = view.body().lines().map(highlight_toml_line).collect();
    let paragraph = Paragraph::new(lines).block(block).scroll((view.scroll, 0));

    // Clear whatever is underneath so the modal is opaque.
    frame.render_widget(ratatui::widgets::Clear, area);
    frame.render_widget(paragraph, area);
}

/// Render a plain scrollable text modal (help, logs) full-screen. No per-line
/// highlighting — the body is shown verbatim.
fn draw_text_modal(frame: &mut Frame, view: &TextModal) {
    let area = frame.area();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(HIGHLIGHT))
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
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlight_toml_styles_by_kind() {
        // Section header: cyan bold.
        let header = highlight_toml_line("[sandbox.repo]");
        assert_eq!(header.spans.len(), 1);
        let s = header.spans[0].style;
        assert_eq!(s.fg, Some(Color::Cyan));
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
}
