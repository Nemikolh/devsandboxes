//! Immediate-mode rendering for the dashboard. Layout: tab bar on top, content
//! area in the middle, help bar at the bottom. Content is placeholder text this
//! step; real views land in steps 3–4.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph, Tabs};

use super::app::{App, Tab};

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

fn draw_tabs(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let selected = Tab::ALL.iter().position(|t| *t == app.tab).unwrap_or(0);
    let tabs = Tabs::new(Tab::ALL.iter().map(|t| t.title()))
        .select(selected)
        .style(Style::default())
        .highlight_style(Style::default().fg(HIGHLIGHT).add_modifier(Modifier::BOLD))
        .divider(" ");
    frame.render_widget(tabs, area);
}

fn draw_content(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let body = match app.tab {
        Tab::Instances => "instances view — step 3",
        Tab::Services => "services view — step 4",
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(HIGHLIGHT))
        .title(app.tab.title());
    let paragraph = Paragraph::new(body).block(block);
    frame.render_widget(paragraph, area);
}

fn draw_help(frame: &mut Frame, area: ratatui::layout::Rect) {
    let help = Line::from("q quit · tab switch tab · ↑↓ select")
        .style(Style::default().add_modifier(Modifier::DIM));
    frame.render_widget(Paragraph::new(help), area);
}
