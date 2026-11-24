//! Semantic styling for prompts on stderr. `src/render.rs` is the stdout
//! side (tables); a prompt's stdout may be piped while its stderr is a
//! terminal, so the gate is stderr's.

use std::io::IsTerminal;

/// Whether prompts on stderr get color: stderr is a TTY, `NO_COLOR` is
/// unset and `TERM` isn't `dumb`.
pub fn enabled() -> bool {
    enabled_from(
        std::io::stderr().is_terminal(),
        std::env::var_os("NO_COLOR").is_some(),
        std::env::var("TERM").ok().as_deref(),
    )
}

fn enabled_from(tty: bool, no_color: bool, term: Option<&str>) -> bool {
    tty && !no_color && term != Some("dumb")
}

/// What a piece of text is, rather than how it looks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Role {
    #[default]
    Plain,
    Heading,
    Name,
    Danger,
    Warn,
    Note,
    Ok,
    Dim,
    Added,
    Removed,
    /// The cursor and the picked answer.
    Accent,
}

impl Role {
    fn sgr(self) -> Option<&'static str> {
        Some(match self {
            Role::Plain => return None,
            Role::Heading | Role::Name => "1",
            Role::Danger => "1;31",
            Role::Warn => "33",
            Role::Note | Role::Accent => "36",
            Role::Ok | Role::Added => "32",
            Role::Removed => "31",
            Role::Dim => "2",
        })
    }
}

/// The color decision taken once, so pure renders can be tested with it off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Style {
    pub on: bool,
}

impl Style {
    pub fn detect() -> Style {
        Style { on: enabled() }
    }

    pub fn plain() -> Style {
        Style { on: false }
    }

    pub fn paint(self, role: Role, text: &str) -> String {
        match role.sgr() {
            Some(code) if self.on => format!("\x1b[{code}m{text}\x1b[0m"),
            _ => text.to_string(),
        }
    }

    pub fn heading(self, text: &str) -> String {
        self.paint(Role::Heading, text)
    }

    pub fn name(self, text: &str) -> String {
        self.paint(Role::Name, text)
    }

    pub fn danger(self, text: &str) -> String {
        self.paint(Role::Danger, text)
    }

    pub fn warn(self, text: &str) -> String {
        self.paint(Role::Warn, text)
    }

    pub fn note(self, text: &str) -> String {
        self.paint(Role::Note, text)
    }

    pub fn ok(self, text: &str) -> String {
        self.paint(Role::Ok, text)
    }

    pub fn dim(self, text: &str) -> String {
        self.paint(Role::Dim, text)
    }

    pub fn added(self, text: &str) -> String {
        self.paint(Role::Added, text)
    }

    pub fn removed(self, text: &str) -> String {
        self.paint(Role::Removed, text)
    }

    pub fn accent(self, text: &str) -> String {
        self.paint(Role::Accent, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_needs_a_tty_no_no_color_and_a_capable_term() {
        assert!(enabled_from(true, false, Some("xterm-256color")));
        assert!(enabled_from(true, false, None));
        assert!(!enabled_from(false, false, Some("xterm")));
        assert!(!enabled_from(true, true, Some("xterm")));
        assert!(!enabled_from(true, false, Some("dumb")));
    }

    #[test]
    fn on_wraps_in_sgr_off_returns_the_text() {
        let on = Style { on: true };
        assert_eq!(on.danger("never"), "\x1b[1;31mnever\x1b[0m");
        assert_eq!(on.heading("gh"), "\x1b[1mgh\x1b[0m");
        assert_eq!(on.added("+ a"), "\x1b[32m+ a\x1b[0m");
        assert_eq!(on.dim("…"), "\x1b[2m…\x1b[0m");
        assert_eq!(on.paint(Role::Plain, "x"), "x");
        assert_eq!(Style::plain().danger("never"), "never");
    }
}
