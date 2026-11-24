//! Interactive questions on stderr: a select list and a review (a fold
//! tree above a select), redrawn in place in raw mode, with a plain
//! line-read fallback off a terminal. See docs/architecture/cli-prompts.md.

// Callers land with the login/access step.
#![allow(dead_code)]

pub mod fold;
pub mod review;
pub mod select;
pub mod style;

use std::io::{BufRead, IsTerminal, Write};

use anyhow::{Context, Result, bail};
use crossterm::cursor::{Hide, MoveToColumn, MoveUp, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::queue;
use crossterm::terminal::{self, Clear, ClearType, DisableLineWrap, EnableLineWrap};

pub use fold::Node;
pub use review::Review;
pub use select::{Choice, Outcome, Select};
pub use style::Style;

/// The widgets' input, so their state machines are tested without
/// crossterm events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Enter,
    Space,
    Tab,
    Esc,
    CtrlC,
    Char(char),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Picked(usize),
    Cancelled,
}

/// What a widget draws: `body` scrolls to keep `focus` in view when the
/// terminal is short, `foot` (the question and choices) always shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub body: Vec<String>,
    pub foot: Vec<String>,
    pub focus: Option<usize>,
}

trait Widget {
    fn key(&mut self, key: Key) -> Outcome;
    fn frame(&self, style: Style) -> Frame;
}

impl Widget for Select {
    fn key(&mut self, key: Key) -> Outcome {
        Select::key(self, key)
    }

    fn frame(&self, style: Style) -> Frame {
        let mut body = self.lines(style, true);
        body.push(style.dim("↑↓ move · enter pick · esc cancel"));
        Frame { body, foot: Vec::new(), focus: Some(1 + self.cursor()) }
    }
}

impl Widget for Review {
    fn key(&mut self, key: Key) -> Outcome {
        Review::key(self, key)
    }

    fn frame(&self, style: Style) -> Frame {
        Review::frame(self, style)
    }
}

/// Ask `question` with `choices`, `default` preselected. Leaves one
/// summary line on stderr.
pub fn select(question: &str, choices: &[Choice], default: usize) -> Result<Answer> {
    let style = Style::detect();
    let mut widget = Select::new(question, choices.to_vec(), default);
    match interactive(&mut widget, style)? {
        Some(outcome) => {
            eprintln!("{}", summary(question, picked_label(choices, outcome), style));
            Ok(answer(outcome))
        }
        None => plain_select(&widget, style),
    }
}

/// Show `tree` above the question. Once answered, scrollback keeps the
/// tree's roots collapsed (one line each) and the summary line.
pub fn review(tree: Vec<Node>, question: &str, choices: &[Choice], default: usize) -> Result<Answer> {
    let style = Style::detect();
    let mut widget = Review::new(tree, Select::new(question, choices.to_vec(), default));
    match interactive(&mut widget, style)? {
        Some(outcome) => {
            for line in widget.fold().summary_lines(style) {
                eprintln!("{line}");
            }
            eprintln!("{}", summary(question, picked_label(choices, outcome), style));
            Ok(answer(outcome))
        }
        None => {
            for line in widget.fold().plain_lines(style) {
                eprintln!("{line}");
            }
            plain_select(widget.select(), style)
        }
    }
}

fn answer(outcome: Outcome) -> Answer {
    match outcome {
        Outcome::Picked(i) => Answer::Picked(i),
        _ => Answer::Cancelled,
    }
}

fn picked_label(choices: &[Choice], outcome: Outcome) -> Option<&str> {
    match outcome {
        Outcome::Picked(i) => Some(&choices[i].label),
        _ => None,
    }
}

/// `? question › answer`; a cancel is the whole line dimmed.
fn summary(question: &str, picked: Option<&str>, style: Style) -> String {
    match picked {
        Some(label) => format!("{} {} › {}", style.accent("?"), style.heading(question), style.accent(label)),
        None => style.dim(&format!("? {question} › cancelled")),
    }
}

/// Raw mode, hidden cursor and no autowrap (so a long line is clipped and
/// the redraw's line count stays true) until dropped, `?` and panics
/// included.
struct Raw;

impl Raw {
    fn enter() -> std::io::Result<Raw> {
        terminal::enable_raw_mode()?;
        let raw = Raw;
        let mut err = std::io::stderr();
        queue!(err, Hide, DisableLineWrap)?;
        err.flush()?;
        Ok(raw)
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        let mut err = std::io::stderr();
        let _ = queue!(err, EnableLineWrap, Show);
        let _ = err.flush();
        let _ = terminal::disable_raw_mode();
    }
}

/// Run `widget` in place; `None` when this isn't an interactive terminal
/// (the caller falls back to the plain prompt). The widget is erased
/// before returning.
fn interactive(widget: &mut impl Widget, style: Style) -> Result<Option<Outcome>> {
    let dumb = std::env::var("TERM").is_ok_and(|t| t == "dumb");
    if dumb || !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Ok(None);
    }
    let Ok(raw) = Raw::enter() else { return Ok(None) };
    let mut err = std::io::stderr();
    let mut drawn = 0;
    let mut top = 0;
    let outcome = loop {
        // A pty nobody sized reports 0 rows: draw unclamped.
        let height = match terminal::size() {
            Ok((_, rows)) if rows > 0 => usize::from(rows),
            _ => usize::MAX,
        };
        let frame = widget.frame(style);
        let (lines, next_top) = visible(&frame, height, top);
        top = next_top;
        erase(&mut err, drawn)?;
        write!(err, "{}", lines.join("\r\n"))?;
        err.flush()?;
        drawn = lines.len();
        let key = loop {
            match event::read().context("cannot read the terminal")? {
                Event::Key(ev) if ev.kind != KeyEventKind::Release => {
                    if let Some(key) = key_of(&ev) {
                        break Some(key);
                    }
                }
                Event::Resize(..) => break None,
                _ => {}
            }
        };
        if let Some(key) = key {
            match widget.key(key) {
                Outcome::Pending => {}
                done => break done,
            }
        }
    };
    erase(&mut err, drawn)?;
    err.flush()?;
    drop(raw);
    Ok(Some(outcome))
}

/// Back to the first drawn line and clear down. Guarded: `MoveUp(0)` is
/// `CSI 0 A`, which terminals read as one line up.
fn erase(err: &mut impl Write, drawn: usize) -> Result<()> {
    if drawn == 0 {
        return Ok(());
    }
    queue!(err, MoveToColumn(0))?;
    if drawn > 1 {
        queue!(err, MoveUp(u16::try_from(drawn - 1).unwrap_or(u16::MAX)))?;
    }
    queue!(err, Clear(ClearType::FromCursorDown))?;
    Ok(())
}

/// The lines that fit in `height`: the whole foot, and a window of the
/// body kept around its focus; returns the window's new top.
fn visible(frame: &Frame, height: usize, top: usize) -> (Vec<String>, usize) {
    let room = height.saturating_sub(frame.foot.len());
    let top = window(frame.body.len(), room, frame.focus, top);
    let end = (top + room).min(frame.body.len());
    let lines = frame.body[top..end].iter().chain(&frame.foot).cloned().collect();
    (lines, top)
}

/// Scrolls only as far as needed to show `focus`; with no focus the
/// window stays put.
fn window(len: usize, room: usize, focus: Option<usize>, top: usize) -> usize {
    if len <= room {
        return 0;
    }
    let mut top = top.min(len - room);
    if let Some(focus) = focus {
        if focus < top {
            top = focus;
        } else if focus >= top + room {
            top = focus + 1 - room;
        }
    }
    top
}

fn key_of(ev: &KeyEvent) -> Option<Key> {
    if ev.modifiers.contains(KeyModifiers::CONTROL) {
        return matches!(ev.code, KeyCode::Char('c' | 'C')).then_some(Key::CtrlC);
    }
    Some(match ev.code {
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Enter => Key::Enter,
        KeyCode::Tab | KeyCode::BackTab => Key::Tab,
        KeyCode::Esc => Key::Esc,
        KeyCode::Char(' ') => Key::Space,
        KeyCode::Char(c) => Key::Char(c),
        _ => return None,
    })
}

fn plain_select(select: &Select, style: Style) -> Result<Answer> {
    for line in plain_lines(select, style) {
        eprintln!("{line}");
    }
    let choices = select.choices();
    eprint!("[1-{}, default {}] > ", choices.len(), select.cursor() + 1);
    std::io::stderr().flush()?;
    let mut line = String::new();
    let read = std::io::stdin().lock().read_line(&mut line)?;
    // A terminal echoed the newline; piped input didn't.
    if read == 0 || !std::io::stdin().is_terminal() {
        eprintln!();
    }
    if read == 0 {
        return Ok(Answer::Cancelled);
    }
    parse_plain(&line, choices, select.cursor()).map(Answer::Picked)
}

fn plain_lines(select: &Select, style: Style) -> Vec<String> {
    let mut lines = vec![select::question_line(select.question(), style)];
    for (i, choice) in select.choices().iter().enumerate() {
        lines.push(format!("  {}) {}", i + 1, select::choice_text(choice, style, false)));
    }
    lines
}

/// Empty is the default; else a 1-based number or a hotkey.
fn parse_plain(input: &str, choices: &[Choice], default: usize) -> Result<usize> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(default);
    }
    if let Ok(n) = input.parse::<usize>() {
        if n == 0 || n > choices.len() {
            bail!("selection out of range");
        }
        return Ok(n - 1);
    }
    let mut chars = input.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        let c = c.to_ascii_lowercase();
        if let Some(i) = choices.iter().position(|choice| choice.key.map(|k| k.to_ascii_lowercase()) == Some(c)) {
            return Ok(i);
        }
    }
    bail!("invalid selection: {input}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choices() -> Vec<Choice> {
        vec![Choice::new("Approve").hotkey('a'), Choice::new("Refuse").hotkey('r').hint("safe")]
    }

    #[test]
    fn plain_answers_take_numbers_hotkeys_and_empty_for_default() {
        let c = choices();
        assert_eq!(parse_plain("\n", &c, 1).unwrap(), 1);
        assert_eq!(parse_plain("1\n", &c, 1).unwrap(), 0);
        assert_eq!(parse_plain(" R \n", &c, 0).unwrap(), 1);
        assert!(parse_plain("3", &c, 0).is_err());
        assert!(parse_plain("0", &c, 0).is_err());
        assert!(parse_plain("yes", &c, 0).is_err());
    }

    #[test]
    fn plain_lines_number_the_choices() {
        let select = Select::new("Approve?", choices(), 1);
        assert_eq!(plain_lines(&select, Style::plain()), ["? Approve?", "  1) Approve [a]", "  2) Refuse [r]  safe"]);
    }

    #[test]
    fn summary_line() {
        assert_eq!(summary("Approve?", Some("Refuse"), Style::plain()), "? Approve? › Refuse");
        assert_eq!(summary("Approve?", None, Style::plain()), "? Approve? › cancelled");
        assert_eq!(summary("Q", None, Style { on: true }), "\x1b[2m? Q › cancelled\x1b[0m");
    }

    #[test]
    fn window_scrolls_just_enough_to_show_the_focus() {
        assert_eq!(window(5, 10, Some(4), 3), 0);
        assert_eq!(window(20, 5, Some(7), 0), 3);
        assert_eq!(window(20, 5, Some(2), 3), 2);
        assert_eq!(window(20, 5, Some(4), 2), 2);
        assert_eq!(window(20, 5, None, 30), 15);
    }

    #[test]
    fn visible_keeps_the_foot_and_scrolls_the_body() {
        let frame = Frame { body: (0..6).map(|i| i.to_string()).collect(), foot: vec!["q".into(), "c".into()], focus: Some(5) };
        assert_eq!(visible(&frame, 5, 0), (vec!["3".into(), "4".into(), "5".into(), "q".into(), "c".into()], 3));
        let frame = Frame { focus: None, ..frame };
        assert_eq!(visible(&frame, 5, 1).0[0], "1");
    }

    #[test]
    fn keys_map_from_crossterm() {
        let key = |code, modifiers| key_of(&KeyEvent::new(code, modifiers));
        assert_eq!(key(KeyCode::Char('c'), KeyModifiers::CONTROL), Some(Key::CtrlC));
        assert_eq!(key(KeyCode::Char('x'), KeyModifiers::CONTROL), None);
        assert_eq!(key(KeyCode::Char(' '), KeyModifiers::NONE), Some(Key::Space));
        assert_eq!(key(KeyCode::Char('A'), KeyModifiers::SHIFT), Some(Key::Char('A')));
        assert_eq!(key(KeyCode::BackTab, KeyModifiers::SHIFT), Some(Key::Tab));
        assert_eq!(key(KeyCode::Home, KeyModifiers::NONE), None);
    }
}
