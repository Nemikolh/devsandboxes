//! Immediate-mode rendering for the dashboard. Layout: tab bar on top, content
//! area in the middle, help bar at the bottom. The Instances tab renders real
//! data from the latest snapshot; the Services tab is placeholder (step 4).

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Tabs};

use super::app::{App, Tab};
use super::data::{humanize_secs, ContainerStatus, InstanceRow, ServiceRow};

const HIGHLIGHT: Color = Color::Cyan;

pub fn draw(frame: &mut Frame, app: &App) {
    let [tab_area, content_area, help_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_tabs(frame, app, tab_area);
    draw_content(frame, app, content_area);
    draw_help(frame, help_area);
}

fn draw_tabs(frame: &mut Frame, app: &App, area: Rect) {
    let selected = Tab::ALL.iter().position(|t| *t == app.tab).unwrap_or(0);
    let tabs = Tabs::new(Tab::ALL.iter().map(|t| t.title()))
        .select(selected)
        .style(Style::default())
        .highlight_style(Style::default().fg(HIGHLIGHT).add_modifier(Modifier::BOLD))
        .divider(" ");
    frame.render_widget(tabs, area);
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
    let rows: &[InstanceRow] = snapshot.map_or(&[], |s| s.instances.as_slice());

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

    if rows.is_empty() {
        draw_empty(frame, table_area);
        draw_detail(frame, None, detail_area);
        return;
    }

    draw_table(frame, app, rows, table_area);
    draw_detail(frame, rows.get(app.selected()), detail_area);
}

fn draw_empty(frame: &mut Frame, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(HIGHLIGHT))
        .title(Tab::Instances.title());
    let text = Line::from(Span::styled(
        "no instances — press : to run one (step 6)",
        Style::default().add_modifier(Modifier::DIM),
    ))
    .alignment(Alignment::Center);
    let paragraph = Paragraph::new(text).block(block);
    frame.render_widget(paragraph, area);
}

fn draw_table(frame: &mut Frame, app: &App, rows: &[InstanceRow], area: Rect) {
    let header = Row::new(
        ["NAME", "SANDBOX", "STATUS", "UPTIME", "CPU", "MEM", "FOLDER", "SERVICES"]
            .into_iter()
            .map(Cell::from),
    )
    .style(Style::default().add_modifier(Modifier::DIM));

    let table_rows: Vec<Row> = rows.iter().map(instance_row).collect();

    let widths = [
        Constraint::Length(14),
        Constraint::Length(14),
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

fn instance_row(r: &InstanceRow) -> Row<'_> {
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
        Cell::from(r.name.clone()),
        Cell::from(r.sandbox.clone()),
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

fn draw_detail(frame: &mut Frame, row: Option<&InstanceRow>, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(HIGHLIGHT))
        .title("Detail");

    let Some(r) = row else {
        frame.render_widget(Paragraph::new("").block(block), area);
        return;
    };

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

    frame.render_widget(Paragraph::new(lines).block(block), area);
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

fn draw_help(frame: &mut Frame, area: Rect) {
    let help = Line::from("q quit · tab switch tab · ↑↓ select")
        .style(Style::default().add_modifier(Modifier::DIM));
    frame.render_widget(Paragraph::new(help), area);
}
