//! A list of answers, one picked with the arrows, a hotkey or Enter.

use super::Key;
use super::style::Style;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub label: String,
    pub hint: Option<String>,
    pub key: Option<char>,
}

impl Choice {
    pub fn new(label: impl Into<String>) -> Choice {
        Choice { label: label.into(), hint: None, key: None }
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Choice {
        self.hint = Some(hint.into());
        self
    }

    pub fn hotkey(mut self, key: char) -> Choice {
        self.key = Some(key);
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Pending,
    Picked(usize),
    Cancelled,
}

/// The cursor clamps at both ends rather than wrapping: a held arrow
/// never lands on the answer at the other end, and `Review` needs "up on
/// the first choice" to mean leaving the list.
#[derive(Clone, Debug)]
pub struct Select {
    question: String,
    choices: Vec<Choice>,
    cursor: usize,
}

impl Select {
    pub fn new(question: impl Into<String>, choices: Vec<Choice>, default: usize) -> Select {
        let cursor = default.min(choices.len().saturating_sub(1));
        Select { question: question.into(), choices, cursor }
    }

    pub fn question(&self) -> &str {
        &self.question
    }

    pub fn choices(&self) -> &[Choice] {
        &self.choices
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub(super) fn set_cursor(&mut self, cursor: usize) {
        self.cursor = cursor.min(self.choices.len().saturating_sub(1));
    }

    fn hotkey_index(&self, c: char) -> Option<usize> {
        let c = c.to_ascii_lowercase();
        self.choices.iter().position(|choice| choice.key.map(|k| k.to_ascii_lowercase()) == Some(c))
    }

    /// `k`/`j` move unless a choice claims them as its hotkey.
    pub(super) fn is_up(&self, key: Key) -> bool {
        key == Key::Up || (key == Key::Char('k') && self.hotkey_index('k').is_none())
    }

    fn is_down(&self, key: Key) -> bool {
        key == Key::Down || (key == Key::Char('j') && self.hotkey_index('j').is_none())
    }

    pub fn key(&mut self, key: Key) -> Outcome {
        if self.is_up(key) {
            self.cursor = self.cursor.saturating_sub(1);
            return Outcome::Pending;
        }
        if self.is_down(key) {
            self.set_cursor(self.cursor + 1);
            return Outcome::Pending;
        }
        match key {
            Key::Esc | Key::CtrlC => Outcome::Cancelled,
            Key::Enter if self.choices.is_empty() => Outcome::Cancelled,
            Key::Enter => Outcome::Picked(self.cursor),
            Key::Char(c) => self.hotkey_index(c).map_or(Outcome::Pending, Outcome::Picked),
            _ => Outcome::Pending,
        }
    }

    /// The question, then one line per choice; the cursor's line is
    /// `1 + cursor()`. `focused` off hides the cursor (focus is elsewhere
    /// in a `Review`).
    pub fn lines(&self, style: Style, focused: bool) -> Vec<String> {
        let mut lines = vec![question_line(&self.question, style)];
        for (i, choice) in self.choices.iter().enumerate() {
            lines.push(if focused && i == self.cursor {
                format!("{} {}", style.accent("❯"), choice_text(choice, style, true))
            } else {
                format!("  {}", choice_text(choice, style, false))
            });
        }
        lines
    }
}

pub(super) fn question_line(question: &str, style: Style) -> String {
    format!("{} {}", style.accent("?"), style.heading(question))
}

/// `label [k]  hint`, shared by the widget and the plain fallback.
pub(super) fn choice_text(choice: &Choice, style: Style, bold: bool) -> String {
    let mut text = if bold { style.heading(&choice.label) } else { choice.label.clone() };
    if let Some(k) = choice.key {
        text.push_str(&style.dim(&format!(" [{k}]")));
    }
    if let Some(hint) = &choice.hint {
        text.push_str(&style.dim(&format!("  {hint}")));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approve() -> Select {
        Select::new(
            "Approve?",
            vec![Choice::new("Approve").hotkey('a').hint("use it"), Choice::new("Refuse").hotkey('r')],
            1,
        )
    }

    #[test]
    fn default_is_preselected_and_clamped() {
        assert_eq!(approve().cursor(), 1);
        assert_eq!(Select::new("q", vec![Choice::new("x")], 5).cursor(), 0);
    }

    #[test]
    fn arrows_and_jk_move_clamped() {
        let mut s = approve();
        assert_eq!(s.key(Key::Down), Outcome::Pending);
        assert_eq!(s.cursor(), 1);
        s.key(Key::Up);
        assert_eq!(s.cursor(), 0);
        s.key(Key::Char('k'));
        assert_eq!(s.cursor(), 0);
        s.key(Key::Char('j'));
        assert_eq!(s.cursor(), 1);
    }

    #[test]
    fn enter_picks_the_cursor_hotkeys_pick_directly() {
        let mut s = approve();
        assert_eq!(s.key(Key::Enter), Outcome::Picked(1));
        assert_eq!(s.key(Key::Char('a')), Outcome::Picked(0));
        assert_eq!(s.key(Key::Char('A')), Outcome::Picked(0));
        assert_eq!(s.key(Key::Char('x')), Outcome::Pending);
    }

    #[test]
    fn a_hotkey_beats_jk_navigation() {
        let mut s = Select::new("q", vec![Choice::new("keep").hotkey('k'), Choice::new("drop")], 1);
        assert_eq!(s.key(Key::Char('k')), Outcome::Picked(0));
        assert_eq!(s.cursor(), 1);
    }

    #[test]
    fn esc_and_ctrl_c_cancel() {
        assert_eq!(approve().key(Key::Esc), Outcome::Cancelled);
        assert_eq!(approve().key(Key::CtrlC), Outcome::Cancelled);
        assert_eq!(Select::new("q", vec![], 0).key(Key::Enter), Outcome::Cancelled);
    }

    #[test]
    fn lines_mark_the_cursor() {
        let s = approve();
        assert_eq!(s.lines(Style::plain(), true), ["? Approve?", "  Approve [a]  use it", "❯ Refuse [r]"]);
        assert_eq!(s.lines(Style::plain(), false), ["? Approve?", "  Approve [a]  use it", "  Refuse [r]"]);
    }

    #[test]
    fn lines_style_the_cursor_label() {
        let lines = approve().lines(Style { on: true }, true);
        assert_eq!(lines[2], "\x1b[36m❯\x1b[0m \x1b[1mRefuse\x1b[0m\x1b[2m [r]\x1b[0m");
    }
}
