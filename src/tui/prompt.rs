//! One-line command prompt for the dashboard (`:`). Pure editing/history/
//! completion logic with no terminal I/O, so it is unit-testable; `mod.rs`
//! drives it with decoded key events and executes the returned [`PromptAction`].
//!
//! The prompt is data-agnostic: completion candidates are supplied by the
//! caller through a closure keyed by token position, and history persistence
//! lives in small [`load_history`]/[`append_history`] fns kept apart from the
//! editing logic.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// Command names offered for first-token completion, in display order.
pub const COMMANDS: [&str; 8] =
    ["run", "exec", "code", "rm", "rename", "stop", "start", "rebuild"];

/// Max history entries kept on disk (oldest trimmed first).
const HISTORY_CAP: usize = 200;

/// A parsed prompt line, handed up to the event loop to execute. Parsing lives
/// in [`Prompt::parse`]; the loop owns the terminal suspend + command call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromptAction {
    Run { sandbox: String, name: Option<String>, branch: Option<String> },
    Exec { instance: String, argv: Vec<String> },
    Code { instance: String },
    Rename { instance: String, new_name: String },
    Rm { instance: String },
    Stop { instance: String },
    Start { instance: String },
    Rebuild { instance: String, force: bool },
    /// `rebuild`/`recreate` on the Services tab: recreate the named service's
    /// backing containers. The parser emits [`PromptAction::Rebuild`]
    /// tab-agnostically; `App` rewrites it to this on the Services tab.
    ServiceRebuild { name: String },
}

/// In-flight tab-completion over a single token.
struct Completion {
    /// Candidate replacements for the token, already filtered by its stem.
    candidates: Vec<String>,
    /// Index of the candidate currently applied; advances on repeated tab.
    cycle: usize,
    /// Char span `[start, end)` of the token being completed in `input`.
    start: usize,
    end: usize,
}

/// Editable prompt state. `input`/`cursor` are measured in chars (the line is
/// short and this keeps the arithmetic obvious); conversions to bytes happen
/// only at the String boundary.
pub struct Prompt {
    input: String,
    /// Cursor position as a char index into `input` (`0..=char_len`).
    cursor: usize,
    /// History newest-last. Navigated with Up/Down.
    history: Vec<String>,
    /// `Some(i)` while navigating history (index into `history`); `None` when
    /// editing the live line.
    hist_idx: Option<usize>,
    /// The in-progress line stashed when history navigation began.
    stash: String,
    completion: Option<Completion>,
    /// Inline parse error, shown red under the line.
    pub error: Option<String>,
}

impl Prompt {
    pub fn new(history: Vec<String>) -> Self {
        Self {
            input: String::new(),
            cursor: 0,
            history,
            hist_idx: None,
            stash: String::new(),
            completion: None,
            error: None,
        }
    }

    /// Construct a prompt pre-filled with `text`, cursor at the end. The prefill
    /// is treated as the live in-progress line, so history navigation stashes and
    /// restores it like anything typed by hand.
    pub fn with_input(history: Vec<String>, text: String) -> Self {
        let cursor = text.chars().count();
        Self {
            input: text,
            cursor,
            history,
            hist_idx: None,
            stash: String::new(),
            completion: None,
            error: None,
        }
    }

    pub fn input(&self) -> &str {
        &self.input
    }

    /// Cursor position as a char index, for the renderer to place the caret.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Candidate labels for the active completion cycle, for the hint line.
    /// Empty when no completion is in flight.
    pub fn candidates(&self) -> &[String] {
        self.completion.as_ref().map_or(&[], |c| c.candidates.as_slice())
    }

    /// Index of the applied candidate within [`Self::candidates`].
    pub fn cycle_index(&self) -> usize {
        self.completion.as_ref().map_or(0, |c| c.cycle)
    }

    fn char_len(&self) -> usize {
        self.input.chars().count()
    }

    /// Byte offset of char index `i` (== `input.len()` when `i == char_len`).
    fn byte_at(&self, i: usize) -> usize {
        self.input
            .char_indices()
            .nth(i)
            .map(|(b, _)| b)
            .unwrap_or(self.input.len())
    }

    /// Any edit that changes the text invalidates the completion cycle and any
    /// stale parse error.
    fn edited(&mut self) {
        self.completion = None;
        self.error = None;
    }

    pub fn insert_char(&mut self, c: char) {
        let at = self.byte_at(self.cursor);
        self.input.insert(at, c);
        self.cursor += 1;
        self.edited();
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = self.byte_at(self.cursor - 1);
        let end = self.byte_at(self.cursor);
        self.input.replace_range(start..end, "");
        self.cursor -= 1;
        self.edited();
    }

    pub fn delete(&mut self) {
        if self.cursor >= self.char_len() {
            return;
        }
        let start = self.byte_at(self.cursor);
        let end = self.byte_at(self.cursor + 1);
        self.input.replace_range(start..end, "");
        self.edited();
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
        self.completion = None;
    }

    pub fn right(&mut self) {
        if self.cursor < self.char_len() {
            self.cursor += 1;
        }
        self.completion = None;
    }

    pub fn home(&mut self) {
        self.cursor = 0;
        self.completion = None;
    }

    pub fn end(&mut self) {
        self.cursor = self.char_len();
        self.completion = None;
    }

    /// Ctrl-U: clear the whole line.
    pub fn clear(&mut self) {
        self.input.clear();
        self.cursor = 0;
        self.edited();
    }

    /// Ctrl-W: delete the whitespace-delimited word before the cursor,
    /// including any run of spaces immediately preceding it.
    pub fn delete_word(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let chars: Vec<char> = self.input.chars().collect();
        let mut start = self.cursor;
        // Skip trailing spaces just left of the cursor.
        while start > 0 && chars[start - 1].is_whitespace() {
            start -= 1;
        }
        // Then the word itself.
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        let from = self.byte_at(start);
        let to = self.byte_at(self.cursor);
        self.input.replace_range(from..to, "");
        self.cursor = start;
        self.edited();
    }

    /// Up: step back into history, stashing the live line on the first step.
    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        self.completion = None;
        let next = match self.hist_idx {
            None => {
                self.stash = self.input.clone();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.hist_idx = Some(next);
        self.set_line(self.history[next].clone());
    }

    /// Down: step forward through history; past the newest entry restores the
    /// stashed live line and leaves history navigation.
    pub fn history_next(&mut self) {
        let Some(i) = self.hist_idx else {
            return;
        };
        self.completion = None;
        if i + 1 >= self.history.len() {
            self.hist_idx = None;
            let stash = std::mem::take(&mut self.stash);
            self.set_line(stash);
        } else {
            self.hist_idx = Some(i + 1);
            self.set_line(self.history[i + 1].clone());
        }
    }

    fn set_line(&mut self, line: String) {
        self.input = line;
        self.cursor = self.char_len();
        self.error = None;
    }

    /// Tab: complete the token under the cursor. `candidates(token_index,
    /// tokens)` supplies the raw candidate list for that position given the
    /// line's whitespace-split tokens (so callers can key on the command and
    /// on preceding flags); the prompt filters by the token's stem, applies
    /// the first match, and cycles on repeated calls. Any non-tab edit clears
    /// the cycle (via [`Self::edited`]).
    pub fn complete<F>(&mut self, candidates: F)
    where
        F: Fn(usize, &[String]) -> Vec<String>,
    {
        // Repeated tab: advance within the existing cycle.
        if let Some(comp) = &mut self.completion {
            if comp.candidates.len() > 1 {
                comp.cycle = (comp.cycle + 1) % comp.candidates.len();
                let pick = comp.candidates[comp.cycle].clone();
                let (start, end) = (comp.start, comp.end);
                self.replace_span(start, end, &pick);
            }
            return;
        }

        let (idx, start, end) = self.token_at_cursor();
        let stem: String = self.input.chars().take(end).skip(start).collect();
        let tokens: Vec<String> = self.input.split_whitespace().map(str::to_string).collect();
        let mut cands: Vec<String> = candidates(idx, &tokens)
            .into_iter()
            .filter(|c| c.starts_with(&stem))
            .collect();
        cands.sort();
        cands.dedup();
        if cands.is_empty() {
            return;
        }
        let pick = cands[0].clone();
        self.replace_span(start, end, &pick);
        // Re-anchor the span end to the freshly inserted candidate so a repeat
        // tab replaces the whole candidate, not the original stem.
        let new_end = start + pick.chars().count();
        self.completion = Some(Completion { candidates: cands, cycle: 0, start, end: new_end });
    }

    /// Replace char span `[start, end)` with `text`, leaving the cursor at its
    /// end and refreshing the completion span. Does not clear `completion`.
    fn replace_span(&mut self, start: usize, end: usize, text: &str) {
        let from = self.byte_at(start);
        let to = self.byte_at(end);
        self.input.replace_range(from..to, text);
        let new_end = start + text.chars().count();
        self.cursor = new_end;
        if let Some(comp) = &mut self.completion {
            comp.end = new_end;
        }
        self.error = None;
    }

    /// Char span of the token the cursor sits in (or the empty token at the
    /// cursor when it is on whitespace), plus that token's index. Whitespace-
    /// delimited; an empty trailing token counts as the next index.
    fn token_at_cursor(&self) -> (usize, usize, usize) {
        let chars: Vec<char> = self.input.chars().collect();
        // Walk tokens, tracking index; the cursor belongs to the token whose
        // span contains it, else to a new empty token at the cursor.
        let mut idx = 0;
        let mut i = 0;
        while i < chars.len() {
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            let start = i;
            while i < chars.len() && !chars[i].is_whitespace() {
                i += 1;
            }
            let end = i;
            if self.cursor >= start && self.cursor <= end {
                return (idx, start, end);
            }
            idx += 1;
        }
        // Cursor past the last token (on trailing space) → new empty token.
        (idx, self.cursor, self.cursor)
    }

    /// Parse the current line into a [`PromptAction`]. On error, stash the
    /// message in `self.error` (shown red) and return `None`.
    pub fn parse(&mut self) -> Option<PromptAction> {
        match parse_line(&self.input) {
            Ok(action) => {
                self.error = None;
                Some(action)
            }
            Err(msg) => {
                self.error = Some(msg);
                None
            }
        }
    }
}

/// Pure line parser, split from [`Prompt`] so it is trivially testable.
fn parse_line(line: &str) -> Result<PromptAction, String> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let Some((&cmd, rest)) = tokens.split_first() else {
        return Err("empty command".into());
    };
    match cmd {
        "run" => {
            // `run <sandbox> [--name n] [--branch b]`.
            let mut sandbox: Option<String> = None;
            let mut name: Option<String> = None;
            let mut branch: Option<String> = None;
            let mut i = 0;
            while i < rest.len() {
                match rest[i] {
                    "--name" => {
                        let val = rest.get(i + 1).ok_or("`--name` needs a value")?;
                        name = Some((*val).to_string());
                        i += 2;
                    }
                    "--branch" => {
                        let val = rest.get(i + 1).ok_or("`--branch` needs a value")?;
                        branch = Some((*val).to_string());
                        i += 2;
                    }
                    other if other.starts_with("--") => {
                        return Err(format!("unknown flag `{other}`"));
                    }
                    other => {
                        if sandbox.is_some() {
                            return Err(format!("unexpected argument `{other}`"));
                        }
                        sandbox = Some(other.to_string());
                        i += 1;
                    }
                }
            }
            let sandbox = sandbox.ok_or("usage: run <sandbox> [--name n] [--branch b]")?;
            Ok(PromptAction::Run { sandbox, name, branch })
        }
        "exec" => {
            let (instance, argv) = rest
                .split_first()
                .ok_or("usage: exec <instance> <cmd…>")?;
            if argv.is_empty() {
                return Err("usage: exec <instance> <cmd…>".into());
            }
            Ok(PromptAction::Exec {
                instance: (*instance).to_string(),
                argv: argv.iter().map(|s| (*s).to_string()).collect(),
            })
        }
        "code" => {
            let [instance] = rest else {
                return Err("usage: code <instance>".into());
            };
            Ok(PromptAction::Code { instance: (*instance).to_string() })
        }
        "rm" => {
            let [instance] = rest else {
                return Err("usage: rm <instance>".into());
            };
            Ok(PromptAction::Rm { instance: (*instance).to_string() })
        }
        "rename" => {
            let [instance, new_name] = rest else {
                return Err("usage: rename <instance> <new-name>".into());
            };
            Ok(PromptAction::Rename {
                instance: (*instance).to_string(),
                new_name: (*new_name).to_string(),
            })
        }
        "stop" => {
            let [instance] = rest else {
                return Err("usage: stop <instance>".into());
            };
            Ok(PromptAction::Stop { instance: (*instance).to_string() })
        }
        "start" => {
            let [instance] = rest else {
                return Err("usage: start <instance>".into());
            };
            Ok(PromptAction::Start { instance: (*instance).to_string() })
        }
        // `recreate` is an alias, mirroring the CLI's `rebuild`/`recreate`.
        // `--force` skips the drift check, like the CLI flag.
        "rebuild" | "recreate" => {
            let mut force = false;
            let mut instance: Option<String> = None;
            for tok in rest {
                match *tok {
                    "--force" => force = true,
                    other if other.starts_with("--") => {
                        return Err(format!("unknown flag `{other}`"));
                    }
                    other => {
                        if instance.is_some() {
                            return Err("usage: rebuild [--force] <instance>".into());
                        }
                        instance = Some(other.to_string());
                    }
                }
            }
            let instance = instance.ok_or("usage: rebuild [--force] <instance>")?;
            Ok(PromptAction::Rebuild { instance, force })
        }
        other => Err(format!(
            "unknown command `{other}` (run, exec, code, rm, rename, stop, start, rebuild)"
        )),
    }
}

/// Append `entry` to `history`, skipping a consecutive duplicate (returns
/// `None` then) and trimming the oldest entries past [`HISTORY_CAP`]. Pure, so
/// the dedup/cap policy is testable without touching the filesystem.
fn push_capped(mut history: Vec<String>, entry: &str) -> Option<Vec<String>> {
    if history.last().map(String::as_str) == Some(entry) {
        return None;
    }
    history.push(entry.to_string());
    if history.len() > HISTORY_CAP {
        let overflow = history.len() - HISTORY_CAP;
        history.drain(0..overflow);
    }
    Some(history)
}

/// Path to the persisted history file, alongside `state.toml`
/// (`<data>/devsandbox/prompt_history`). Mirrors `State::path`'s base-dir logic.
pub fn history_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .context("cannot determine user data dir ($XDG_DATA_HOME or $HOME)")?;
    Ok(base.join("devsandbox/prompt_history"))
}

/// Load history newest-last. Missing file → empty; best-effort otherwise.
pub fn load_history() -> Vec<String> {
    let Ok(path) = history_path() else {
        return Vec::new();
    };
    match std::fs::read_to_string(&path) {
        Ok(contents) => contents
            .lines()
            .map(str::trim_end)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Append `entry` to the history file, skipping blanks and consecutive dups,
/// capping the file at [`HISTORY_CAP`] lines. Best-effort: I/O errors are
/// swallowed so a submitted command still runs.
pub fn append_history(entry: &str) {
    let entry = entry.trim();
    if entry.is_empty() {
        return;
    }
    let history = load_history();
    let history = match push_capped(history, entry) {
        Some(h) => h,
        None => return, // consecutive duplicate, nothing to write
    };
    let Ok(path) = history_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut body = history.join("\n");
    body.push('\n');
    let _ = std::fs::write(&path, body);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(input: &str) -> Prompt {
        let mut pr = Prompt::new(Vec::new());
        for c in input.chars() {
            pr.insert_char(c);
        }
        pr
    }

    #[test]
    fn insert_and_cursor_edits() {
        let mut pr = p("abc");
        assert_eq!(pr.input(), "abc");
        assert_eq!(pr.cursor(), 3);
        pr.left();
        pr.left();
        pr.insert_char('X');
        assert_eq!(pr.input(), "aXbc");
        pr.home();
        pr.delete();
        assert_eq!(pr.input(), "Xbc");
        pr.end();
        pr.backspace();
        assert_eq!(pr.input(), "Xb");
    }

    #[test]
    fn ctrl_u_clears() {
        let mut pr = p("run foo");
        pr.clear();
        assert_eq!(pr.input(), "");
        assert_eq!(pr.cursor(), 0);
    }

    #[test]
    fn ctrl_w_deletes_word_and_trailing_spaces() {
        let mut pr = p("run foo bar");
        pr.delete_word();
        assert_eq!(pr.input(), "run foo ");
        pr.delete_word();
        assert_eq!(pr.input(), "run ");
        pr.delete_word();
        assert_eq!(pr.input(), "");
    }

    #[test]
    fn ctrl_w_midline() {
        let mut pr = p("run foobar");
        pr.left();
        pr.left();
        pr.left();
        // Cursor after "foo": delete the word before it.
        pr.delete_word();
        assert_eq!(pr.input(), "run bar");
    }

    /// Candidate source used by completion tests.
    fn cands(idx: usize, tokens: &[String]) -> Vec<String> {
        if idx == 0 {
            return COMMANDS.iter().map(|s| s.to_string()).collect();
        }
        match tokens.first().map(String::as_str) {
            Some("run") => vec!["alpha".into(), "beta".into(), "bacon".into()],
            Some("exec" | "code" | "rm" | "stop") => vec!["inst1".into(), "inst2".into()],
            _ => Vec::new(),
        }
    }

    #[test]
    fn first_token_completion_cycles() {
        let mut pr = p("r");
        pr.complete(cands); // "r" matches rebuild, rename, rm, run → first sorted ("rebuild")
        assert_eq!(pr.input(), "rebuild");
        pr.complete(cands); // cycle to next
        assert_eq!(pr.input(), "rename");
        pr.complete(cands); // then rm
        assert_eq!(pr.input(), "rm");
        pr.complete(cands); // then run
        assert_eq!(pr.input(), "run");
        pr.complete(cands); // wraps back
        assert_eq!(pr.input(), "rebuild");
    }

    #[test]
    fn second_token_completion_filters_by_stem() {
        let mut pr = p("run b");
        pr.complete(cands); // "b" → bacon, beta (sorted)
        assert_eq!(pr.input(), "run bacon");
        pr.complete(cands);
        assert_eq!(pr.input(), "run beta");
        // Typing resets the cycle.
        pr.insert_char('x');
        assert_eq!(pr.input(), "run betax");
        assert!(pr.candidates().is_empty());
    }

    #[test]
    fn completion_empty_token_offers_all() {
        let mut pr = p("exec ");
        pr.complete(cands);
        assert_eq!(pr.input(), "exec inst1");
    }

    #[test]
    fn parse_run_variants() {
        assert_eq!(
            parse_line("run web"),
            Ok(PromptAction::Run { sandbox: "web".into(), name: None, branch: None })
        );
        assert_eq!(
            parse_line("run web --name api"),
            Ok(PromptAction::Run { sandbox: "web".into(), name: Some("api".into()), branch: None })
        );
        assert_eq!(
            parse_line("run web --branch feat/x"),
            Ok(PromptAction::Run {
                sandbox: "web".into(),
                name: None,
                branch: Some("feat/x".into()),
            })
        );
        assert!(parse_line("run").is_err());
        assert!(parse_line("run web --name").is_err());
        assert!(parse_line("run web --branch").is_err());
        assert!(parse_line("run web --bogus").is_err());
        assert!(parse_line("run a b").is_err());
    }

    #[test]
    fn parse_exec_requires_command() {
        assert_eq!(
            parse_line("exec box ls -la"),
            Ok(PromptAction::Exec {
                instance: "box".into(),
                argv: vec!["ls".into(), "-la".into()],
            })
        );
        assert!(parse_line("exec box").is_err());
        assert!(parse_line("exec").is_err());
    }

    #[test]
    fn parse_code_and_rm() {
        assert_eq!(parse_line("code box"), Ok(PromptAction::Code { instance: "box".into() }));
        assert_eq!(parse_line("rm box"), Ok(PromptAction::Rm { instance: "box".into() }));
        assert!(parse_line("code").is_err());
        assert!(parse_line("code a b").is_err());
        assert!(parse_line("rm").is_err());
    }

    #[test]
    fn parse_rename() {
        assert_eq!(
            parse_line("rename old new"),
            Ok(PromptAction::Rename { instance: "old".into(), new_name: "new".into() })
        );
        assert!(parse_line("rename").is_err());
        assert!(parse_line("rename old").is_err());
        assert!(parse_line("rename old new extra").is_err());
    }

    #[test]
    fn parse_stop() {
        assert_eq!(parse_line("stop box"), Ok(PromptAction::Stop { instance: "box".into() }));
        assert!(parse_line("stop").is_err());
        assert!(parse_line("stop a b").is_err());
    }

    #[test]
    fn parse_start() {
        assert_eq!(parse_line("start box"), Ok(PromptAction::Start { instance: "box".into() }));
        assert!(parse_line("start").is_err());
        assert!(parse_line("start a b").is_err());
    }

    #[test]
    fn parse_rebuild_and_recreate_alias() {
        assert_eq!(
            parse_line("rebuild box"),
            Ok(PromptAction::Rebuild { instance: "box".into(), force: false })
        );
        assert_eq!(
            parse_line("recreate box"),
            Ok(PromptAction::Rebuild { instance: "box".into(), force: false })
        );
        // `--force` skips the drift check; position-independent like the CLI.
        assert_eq!(
            parse_line("rebuild --force box"),
            Ok(PromptAction::Rebuild { instance: "box".into(), force: true })
        );
        assert_eq!(
            parse_line("rebuild box --force"),
            Ok(PromptAction::Rebuild { instance: "box".into(), force: true })
        );
        assert!(parse_line("rebuild").is_err());
        assert!(parse_line("rebuild --force").is_err());
        assert!(parse_line("rebuild --bogus box").is_err());
        assert!(parse_line("rebuild a b").is_err());
        assert!(parse_line("recreate").is_err());
    }

    #[test]
    fn parse_unknown_and_empty() {
        assert!(parse_line("").is_err());
        assert!(parse_line("   ").is_err());
        assert!(parse_line("frobnicate x").is_err());
    }

    #[test]
    fn parse_sets_error_on_prompt() {
        let mut pr = p("exec box");
        assert!(pr.parse().is_none());
        assert!(pr.error.is_some());
        // A successful parse clears it.
        let mut ok = p("rm box");
        assert!(ok.parse().is_some());
        assert!(ok.error.is_none());
    }

    #[test]
    fn history_dedup_and_cap() {
        // Consecutive duplicate is rejected.
        let h = vec!["a".to_string()];
        assert_eq!(push_capped(h, "a"), None);

        // Non-consecutive duplicate is kept (only consecutive dedup).
        let h = vec!["a".to_string(), "b".to_string()];
        assert_eq!(
            push_capped(h, "a"),
            Some(vec!["a".into(), "b".into(), "a".into()])
        );

        // Cap trims the oldest entries.
        let full: Vec<String> = (0..HISTORY_CAP).map(|i| i.to_string()).collect();
        let out = push_capped(full, "new").unwrap();
        assert_eq!(out.len(), HISTORY_CAP);
        assert_eq!(out.first().map(String::as_str), Some("1"));
        assert_eq!(out.last().map(String::as_str), Some("new"));
    }

    #[test]
    fn history_navigation_stashes_live_line() {
        let mut pr = Prompt::new(vec!["run a".into(), "rm b".into()]);
        for c in "draft".chars() {
            pr.insert_char(c);
        }
        pr.history_prev();
        assert_eq!(pr.input(), "rm b");
        pr.history_prev();
        assert_eq!(pr.input(), "run a");
        pr.history_prev(); // clamps at oldest
        assert_eq!(pr.input(), "run a");
        pr.history_next();
        assert_eq!(pr.input(), "rm b");
        pr.history_next(); // back to the stashed live line
        assert_eq!(pr.input(), "draft");
    }
}
