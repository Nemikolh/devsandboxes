//! A build's notes (docs/architecture/instances/lifecycle.md, *Build
//! notes*): the stderr lines of a `run`/`rebuild`/`start` worth keeping on
//! the instance (the identity summary, the browser sandbox, the GitHub
//! fence, `warning:`/`note:` lines), so the dashboard shows them after the
//! build log scrolled by.
//!
//! Collected per thread, not process-wide: `devsandbox serve` runs
//! autostart's commands in-process, side by side. So a line printed from
//! another thread than the one that started the [`Capture`] is printed but
//! not collected.

use std::cell::RefCell;
use std::fmt::Display;

thread_local! {
    static COLLECTED: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

/// Collecting on this thread until [`Capture::finish`] (or drop). A capture
/// started inside another hands its lines on to the outer one when it ends.
pub(crate) struct Capture {
    outer: Option<Vec<String>>,
    done: bool,
}

impl Capture {
    pub(crate) fn start() -> Self {
        let outer = COLLECTED.with(|c| c.replace(Some(Vec::new())));
        Capture { outer, done: false }
    }

    /// Stop collecting; what was collected since [`Capture::start`].
    pub(crate) fn finish(mut self) -> Vec<String> {
        self.done = true;
        self.end()
    }

    fn end(&mut self) -> Vec<String> {
        let lines = COLLECTED.with(|c| c.replace(self.outer.take())).unwrap_or_default();
        COLLECTED.with(|c| {
            if let Some(outer) = c.borrow_mut().as_mut() {
                outer.extend(lines.iter().cloned());
            }
        });
        lines
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if !self.done {
            self.end();
        }
    }
}

/// [`Capture`]'s closure form: `f`'s result and the lines it collected.
#[cfg(test)]
pub(crate) fn capture<T>(f: impl FnOnce() -> T) -> (T, Vec<String>) {
    let capture = Capture::start();
    let out = f();
    (out, capture.finish())
}

/// What the innermost capture on this thread collected so far (none when
/// nothing collects).
pub(crate) fn collected() -> Vec<String> {
    COLLECTED.with(|c| c.borrow().clone().unwrap_or_default())
}

/// Print `line` on stderr as is, and collect it.
pub(crate) fn line(line: impl Display) {
    let line = line.to_string();
    eprintln!("{line}");
    COLLECTED.with(|c| {
        if let Some(lines) = c.borrow_mut().as_mut() {
            lines.push(line);
        }
    });
}

/// `eprintln!` that collects the line too ([`line`]).
macro_rules! noted {
    ($($arg:tt)*) => {
        $crate::commands::run::notes::line(format_args!($($arg)*))
    };
}
pub(crate) use noted;

/// [`line`] as `warning: <msg>`.
pub(crate) fn warn(msg: impl Display) {
    line(format!("warning: {msg}"));
}

/// [`line`] as `note: <msg>`.
pub(crate) fn note(msg: impl Display) {
    line(format!("note: {msg}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_with_prefixes_only_while_capturing() {
        warn("before");
        let ((), lines) = capture(|| {
            warn("w");
            note("n");
            line("identity: x");
        });
        assert_eq!(lines, ["warning: w", "note: n", "identity: x"]);
        warn("after");
        assert!(collected().is_empty());
    }

    #[test]
    fn nested_capture_hands_its_lines_to_the_outer() {
        let ((), outer) = capture(|| {
            note("a");
            let ((), inner) = capture(|| warn("b"));
            assert_eq!(inner, ["warning: b"]);
            assert_eq!(collected(), ["note: a", "warning: b"]);
        });
        assert_eq!(outer, ["note: a", "warning: b"]);
    }

    #[test]
    fn a_dropped_capture_stops_collecting() {
        {
            let _capture = Capture::start();
            warn("x");
            assert_eq!(collected(), ["warning: x"]);
        }
        assert!(collected().is_empty());
    }

    #[test]
    fn other_threads_are_not_collected() {
        let ((), lines) = capture(|| {
            std::thread::spawn(|| warn("elsewhere")).join().unwrap();
            warn("here");
        });
        assert_eq!(lines, ["warning: here"]);
    }
}
