//! A `Fold` of details above a `Select`, so the details can be opened
//! before answering.

use super::fold::{Fold, Node};
use super::select::{Outcome, Select};
use super::style::Style;
use super::{Frame, Key};

#[derive(Clone, Debug)]
pub struct Review {
    fold: Fold,
    select: Select,
    tree_focus: bool,
}

impl Review {
    /// Focus starts on the choices, at `select`'s default.
    pub fn new(tree: Vec<Node>, select: Select) -> Review {
        Review { fold: Fold::new(tree), select, tree_focus: false }
    }

    pub fn fold(&self) -> &Fold {
        &self.fold
    }

    pub fn select(&self) -> &Select {
        &self.select
    }

    /// Hotkeys only count with the choices focused, so keys meant for the
    /// tree never answer.
    pub fn key(&mut self, key: Key) -> Outcome {
        match key {
            Key::Esc | Key::CtrlC => return Outcome::Cancelled,
            Key::Tab => {
                self.tree_focus = !self.tree_focus && !self.fold.is_empty();
                return Outcome::Pending;
            }
            _ => {}
        }
        if self.tree_focus {
            if matches!(key, Key::Down | Key::Char('j')) && self.fold.at_last() {
                self.tree_focus = false;
                self.select.set_cursor(0);
            } else {
                self.fold.key(key);
            }
            return Outcome::Pending;
        }
        if self.select.cursor() == 0 && self.select.is_up(key) && !self.fold.is_empty() {
            self.tree_focus = true;
            self.fold.focus_last();
            return Outcome::Pending;
        }
        self.select.key(key)
    }

    pub fn frame(&self, style: Style) -> Frame {
        let (body, at) = self.fold.lines(style, self.tree_focus);
        let mut foot = vec![String::new()];
        foot.extend(self.select.lines(style, !self.tree_focus));
        let help = match (self.tree_focus, self.fold.is_empty()) {
            (true, _) => "↑↓ move · → expand · ← collapse · tab choices · esc cancel",
            (false, false) => "↑↓ move · enter pick · tab details · esc cancel",
            (false, true) => "↑↓ move · enter pick · esc cancel",
        };
        foot.push(style.dim(help));
        Frame { body, foot, focus: self.tree_focus.then_some(at) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ask::select::Choice;

    fn review() -> Review {
        Review::new(
            vec![Node::new("scope").children(["a", "b", "c"].map(Node::new)), Node::new("never")],
            Select::new("Approve?", vec![Choice::new("Approve").hotkey('a'), Choice::new("Refuse").hotkey('r')], 1),
        )
    }

    fn lines(r: &Review) -> Vec<String> {
        let frame = r.frame(Style::plain());
        frame.body.into_iter().chain(frame.foot).collect()
    }

    #[test]
    fn starts_on_the_default_choice_with_no_tree_cursor() {
        let r = review();
        assert_eq!(
            lines(&r),
            [
                "  ▸ scope",
                "    a",
                "    b",
                "    … 1 more",
                "  never",
                "",
                "? Approve?",
                "  Approve [a]",
                "❯ Refuse [r]",
                "↑↓ move · enter pick · tab details · esc cancel",
            ]
        );
        assert_eq!(r.frame(Style::plain()).focus, None);
    }

    #[test]
    fn up_past_the_first_choice_enters_the_tree_at_its_end() {
        let mut r = review();
        r.key(Key::Up);
        assert_eq!(r.select().cursor(), 0);
        assert_eq!(r.key(Key::Up), Outcome::Pending);
        let frame = r.frame(Style::plain());
        assert_eq!(frame.body[4], "❯ never");
        assert_eq!(frame.focus, Some(4));
        assert_eq!(frame.foot[2], "  Approve [a]");
    }

    #[test]
    fn tree_keys_never_answer_and_down_returns_to_the_choices() {
        let mut r = review();
        r.key(Key::Tab);
        assert_eq!(r.key(Key::Char('a')), Outcome::Pending);
        assert_eq!(r.key(Key::Enter), Outcome::Pending);
        assert_eq!(r.frame(Style::plain()).body[0], "❯ ▾ scope");
        r.focus_down_to_choices();
        assert_eq!(r.frame(Style::plain()).focus, None);
        assert_eq!(r.select().cursor(), 0);
        assert_eq!(r.key(Key::Enter), Outcome::Picked(0));
    }

    #[test]
    fn tab_toggles_focus_and_keeps_the_choice() {
        let mut r = review();
        r.key(Key::Tab);
        assert_eq!(r.frame(Style::plain()).focus, Some(0));
        r.key(Key::Tab);
        assert_eq!(r.key(Key::Enter), Outcome::Picked(1));
    }

    #[test]
    fn esc_cancels_from_either_focus() {
        let mut r = review();
        assert_eq!(r.key(Key::Esc), Outcome::Cancelled);
        r.key(Key::Tab);
        assert_eq!(r.key(Key::CtrlC), Outcome::Cancelled);
    }

    #[test]
    fn an_empty_tree_keeps_focus_on_the_choices() {
        let mut r = Review::new(vec![], Select::new("q", vec![Choice::new("x"), Choice::new("y")], 0));
        r.key(Key::Tab);
        r.key(Key::Up);
        assert_eq!(r.frame(Style::plain()).focus, None);
        assert_eq!(r.key(Key::Enter), Outcome::Picked(0));
    }

    impl Review {
        fn focus_down_to_choices(&mut self) {
            for _ in 0..10 {
                if !self.tree_focus {
                    return;
                }
                self.key(Key::Down);
            }
        }
    }
}
