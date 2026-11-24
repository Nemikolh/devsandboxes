//! A checklist: any number of answers checked with Space, confirmed with
//! Enter.

use super::Key;
use super::select::{Choice, choice_text, question_line};
use super::style::Style;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Pending,
    /// The checked indexes, ascending; empty is an answer, not a cancel.
    Done(Vec<usize>),
    Cancelled,
}

/// Moves like `Select` (clamped, `k`/`j` unless claimed as hotkeys), but
/// a hotkey toggles its choice instead of answering.
#[derive(Clone, Debug)]
pub struct Multi {
    question: String,
    choices: Vec<Choice>,
    checked: Vec<bool>,
    cursor: usize,
}

impl Multi {
    /// `checked` shorter than `choices` leaves the rest unchecked.
    pub fn new(question: impl Into<String>, choices: Vec<Choice>, checked: &[bool]) -> Multi {
        let checked = (0..choices.len()).map(|i| checked.get(i).copied().unwrap_or(false)).collect();
        Multi { question: question.into(), choices, checked, cursor: 0 }
    }

    pub fn question(&self) -> &str {
        &self.question
    }

    pub fn choices(&self) -> &[Choice] {
        &self.choices
    }

    pub fn checked(&self) -> &[bool] {
        &self.checked
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    fn hotkey_index(&self, c: char) -> Option<usize> {
        let c = c.to_ascii_lowercase();
        self.choices.iter().position(|choice| choice.key.map(|k| k.to_ascii_lowercase()) == Some(c))
    }

    fn toggle(&mut self, i: usize) {
        if let Some(on) = self.checked.get_mut(i) {
            *on = !*on;
        }
    }

    pub fn key(&mut self, key: Key) -> Outcome {
        let claimed = |c| matches!(key, Key::Char(k) if k == c) && self.hotkey_index(c).is_none();
        if key == Key::Up || claimed('k') {
            self.cursor = self.cursor.saturating_sub(1);
            return Outcome::Pending;
        }
        if key == Key::Down || claimed('j') {
            self.cursor = (self.cursor + 1).min(self.choices.len().saturating_sub(1));
            return Outcome::Pending;
        }
        match key {
            Key::Esc | Key::CtrlC => return Outcome::Cancelled,
            Key::Enter => return Outcome::Done(checked_indexes(&self.checked)),
            Key::Space => self.toggle(self.cursor),
            Key::Char(c) => {
                if let Some(i) = self.hotkey_index(c) {
                    self.toggle(i);
                }
            }
            _ => {}
        }
        Outcome::Pending
    }

    /// The question, then one line per choice; the cursor's line is
    /// `1 + cursor()`.
    pub fn lines(&self, style: Style) -> Vec<String> {
        let mut lines = vec![question_line(&self.question, style)];
        for (i, choice) in self.choices.iter().enumerate() {
            let at = i == self.cursor;
            let gutter = if at { style.accent("❯") } else { " ".to_string() };
            lines.push(format!("{gutter} {} {}", mark(self.checked[i], style), choice_text(choice, style, at)));
        }
        lines
    }
}

pub(super) fn checked_indexes(checked: &[bool]) -> Vec<usize> {
    checked.iter().enumerate().filter(|(_, on)| **on).map(|(i, _)| i).collect()
}

fn mark(on: bool, style: Style) -> String {
    if on { style.accent("◉") } else { "◯".to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn multi() -> Multi {
        Multi::new(
            "Lend?",
            vec![Choice::new("docker").hint("runs containers"), Choice::new("ssh").hotkey('s'), Choice::new("gpu")],
            &[false, true],
        )
    }

    #[test]
    fn checked_pads_to_the_choices() {
        assert_eq!(multi().checked(), [false, true, false]);
        assert_eq!(Multi::new("q", vec![Choice::new("x")], &[true, true]).checked(), [true]);
    }

    #[test]
    fn arrows_and_jk_move_clamped() {
        let mut m = multi();
        assert_eq!(m.key(Key::Up), Outcome::Pending);
        assert_eq!(m.cursor(), 0);
        m.key(Key::Char('j'));
        m.key(Key::Down);
        m.key(Key::Down);
        assert_eq!(m.cursor(), 2);
        m.key(Key::Char('k'));
        assert_eq!(m.cursor(), 1);
    }

    #[test]
    fn space_toggles_the_cursor_and_enter_confirms_the_checked() {
        let mut m = multi();
        assert_eq!(m.key(Key::Space), Outcome::Pending);
        m.key(Key::Down);
        m.key(Key::Space);
        assert_eq!(m.key(Key::Enter), Outcome::Done(vec![0]));
        m.key(Key::Space);
        assert_eq!(m.key(Key::Enter), Outcome::Done(vec![0, 1]));
    }

    #[test]
    fn nothing_checked_is_still_an_answer() {
        let mut m = Multi::new("q", vec![Choice::new("x")], &[]);
        assert_eq!(m.key(Key::Enter), Outcome::Done(vec![]));
        assert_eq!(Multi::new("q", vec![], &[]).key(Key::Enter), Outcome::Done(vec![]));
    }

    #[test]
    fn a_hotkey_toggles_without_answering() {
        let mut m = multi();
        assert_eq!(m.key(Key::Char('S')), Outcome::Pending);
        assert_eq!(m.checked(), [false, false, false]);
        assert_eq!(m.cursor(), 0);
        assert_eq!(m.key(Key::Char('x')), Outcome::Pending);
    }

    #[test]
    fn a_hotkey_beats_jk_navigation() {
        let mut m = Multi::new("q", vec![Choice::new("a"), Choice::new("keep").hotkey('k')], &[]);
        m.key(Key::Down);
        m.key(Key::Char('k'));
        assert_eq!(m.cursor(), 1);
        assert_eq!(m.checked(), [false, true]);
    }

    #[test]
    fn esc_and_ctrl_c_cancel() {
        assert_eq!(multi().key(Key::Esc), Outcome::Cancelled);
        assert_eq!(multi().key(Key::CtrlC), Outcome::Cancelled);
    }

    #[test]
    fn lines_mark_the_cursor_and_the_checked() {
        let mut m = multi();
        assert_eq!(m.lines(Style::plain()), ["? Lend?", "❯ ◯ docker  runs containers", "  ◉ ssh [s]", "  ◯ gpu"]);
        m.key(Key::Down);
        assert_eq!(m.lines(Style::plain())[1..], ["  ◯ docker  runs containers", "❯ ◉ ssh [s]", "  ◯ gpu"]);
    }

    #[test]
    fn lines_style_the_cursor_and_checked_marks() {
        let mut m = multi();
        m.key(Key::Down);
        let lines = m.lines(Style { on: true });
        assert_eq!(lines[2], "\x1b[36m❯\x1b[0m \x1b[36m◉\x1b[0m \x1b[1mssh\x1b[0m\x1b[2m [s]\x1b[0m");
        assert_eq!(lines[3], "  ◯ gpu");
    }
}
