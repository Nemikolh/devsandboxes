//! Kitty keyboard protocol emulation for the integrated terminal.
//!
//! vt100 ignores the protocol's sequences, so an inner app (zidane, helix, …)
//! asking for it would never get an answer and would fall back to legacy
//! encoding, where e.g. ctrl+m and Enter are the same `\r`. [`KittyState`]
//! plugs into the parser as its [`vt100::Callbacks`]: vt100 hands every CSI it
//! doesn't implement to `unhandled_csi` together with the live screen, which is
//! exactly the hook needed to track the app's flag stacks (one per screen, as
//! the spec requires) and to queue the replies to its `CSI ? u` queries.
//!
//! Only honored when the outer terminal speaks the protocol too (probed by the
//! dashboard at startup): otherwise devsandbox itself receives legacy bytes and
//! could not produce the disambiguated encodings it would advertise.
//!
//! Spec: <https://sw.kovidgoyal.net/kitty/keyboard-protocol/>.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// "Disambiguate escape codes": the only enhancement we implement. The others
/// need input the dashboard doesn't receive (release/repeat events, the
/// shifted and base-layout keys, associated text), so they are masked off and
/// queries report what is really in effect, as the spec asks.
pub const DISAMBIGUATE: u8 = 0b1;

/// Stack depth cap. The spec asks terminals to bound the stack and evict the
/// oldest entry on overflow so a misbehaving app can't grow it forever.
const MAX_DEPTH: usize = 16;

/// One screen's flag stack. The top is the flags in effect; empty means none.
#[derive(Debug, Default)]
struct FlagStack(Vec<u8>);

impl FlagStack {
    fn current(&self) -> u8 {
        self.0.last().copied().unwrap_or(0)
    }

    /// `CSI > flags u`.
    fn push(&mut self, flags: u8) {
        if self.0.len() == MAX_DEPTH {
            self.0.remove(0);
        }
        self.0.push(flags & DISAMBIGUATE);
    }

    /// `CSI < n u`: popping past the bottom just empties it (all flags reset).
    fn pop(&mut self, n: usize) {
        self.0.truncate(self.0.len().saturating_sub(n));
    }

    /// `CSI = flags ; mode u`: 1 replaces, 2 sets bits, 3 clears bits of the
    /// current entry. With nothing pushed it creates the entry, so the change
    /// sticks the way it does on a real terminal's base entry.
    fn set(&mut self, flags: u8, mode: u16) {
        let flags = flags & DISAMBIGUATE;
        if self.0.is_empty() {
            self.0.push(0);
        }
        let top = self.0.last_mut().expect("just ensured non-empty");
        match mode {
            2 => *top |= flags,
            3 => *top &= !flags,
            _ => *top = flags,
        }
    }
}

/// Per-session protocol state, owned by the session's `vt100::Parser`.
#[derive(Debug, Default)]
pub struct KittyState {
    /// Whether to emulate at all (the outer terminal speaks the protocol).
    /// When false every sequence is ignored and queries go unanswered, which
    /// is what apps see from a terminal without support.
    enabled: bool,
    main: FlagStack,
    alt: FlagStack,
    /// Replies to the child's queries, queued here because the callback runs
    /// inside `Parser::process` under the parser lock; the reader thread takes
    /// them afterwards and writes them to the PTY.
    replies: Vec<u8>,
}

impl KittyState {
    pub fn new(enabled: bool) -> KittyState {
        KittyState { enabled, ..KittyState::default() }
    }

    /// Flags in effect for the main or the alternate screen.
    pub fn flags(&self, alternate_screen: bool) -> u8 {
        if alternate_screen {
            self.alt.current()
        } else {
            self.main.current()
        }
    }

    /// Drain the queued query replies.
    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.replies)
    }

    /// Forget the alternate screen's stack. Called by the reader whenever the
    /// child is back on the main screen: apps usually leave the alternate
    /// screen without popping (a real terminal drops that stack with the
    /// screen), and a stale entry must not leak into the next app to enter it.
    pub fn reset_alt(&mut self) {
        self.alt = FlagStack::default();
    }
}

/// Encode `key` for a child that enabled `flags`, or `None` when the legacy
/// encoding ([`super::term::encode_key`]) is what the protocol mandates.
/// Disambiguation only changes the keys legacy encoding conflates: Esc, and
/// ctrl/alt/super chords of text keys (ctrl+m vs Enter, ctrl+[ vs Esc, ctrl+i
/// vs Tab) become `CSI code;mods u`. Enter/Tab/Backspace stay legacy unless
/// modified, per spec, so typing `reset` still works after a crashed app left
/// the mode on; shift alone on a text key is just the shifted text.
pub fn encode_key(key: KeyEvent, flags: u8) -> Option<Vec<u8>> {
    if flags & DISAMBIGUATE == 0 {
        return None;
    }
    let mut mods = modifier_bits(key.modifiers);
    let code = match key.code {
        KeyCode::Esc => 27,
        KeyCode::Enter | KeyCode::Tab | KeyCode::Backspace if mods == 0 => return None,
        KeyCode::Enter => 13,
        KeyCode::Tab => 9,
        KeyCode::Backspace => 127,
        // crossterm folds shift+tab into BackTab; kitty reports it as tab+shift.
        KeyCode::BackTab => {
            mods |= SHIFT;
            9
        }
        KeyCode::Char(c) => {
            if mods & !SHIFT == 0 {
                return None;
            }
            // The protocol reports the unshifted key plus the shift bit.
            if mods & SHIFT != 0 {
                c.to_ascii_lowercase() as u32
            } else {
                c as u32
            }
        }
        _ => return None,
    };
    Some(if mods == 0 {
        format!("\x1b[{code}u")
    } else {
        format!("\x1b[{code};{}u", mods + 1)
    }
    .into_bytes())
}

const SHIFT: u8 = 1;

/// The spec's modifier bitfield (sent as value + 1).
fn modifier_bits(m: KeyModifiers) -> u8 {
    [
        (KeyModifiers::SHIFT, SHIFT),
        (KeyModifiers::ALT, 2),
        (KeyModifiers::CONTROL, 4),
        (KeyModifiers::SUPER, 8),
        (KeyModifiers::HYPER, 16),
        (KeyModifiers::META, 32),
    ]
    .into_iter()
    .filter(|(m2, _)| m.contains(*m2))
    .fold(0, |acc, (_, bit)| acc | bit)
}

impl vt100::Callbacks for KittyState {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        if !self.enabled || c != 'u' || i2.is_some() {
            return;
        }
        let param = |i: usize| params.get(i).and_then(|p| p.first()).copied();
        let stack = if screen.alternate_screen() {
            &mut self.alt
        } else {
            &mut self.main
        };
        match i1 {
            Some(b'>') => stack.push(param(0).unwrap_or(0) as u8),
            // `CSI < u` pops one; a 0 count means the default too.
            Some(b'<') => stack.pop(param(0).filter(|&n| n > 0).unwrap_or(1) as usize),
            Some(b'=') => stack.set(param(0).unwrap_or(0) as u8, param(1).unwrap_or(1)),
            Some(b'?') => {
                let reply = format!("\x1b[?{}u", stack.current());
                self.replies.extend_from_slice(reply.as_bytes());
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parser(enabled: bool) -> vt100::Parser<KittyState> {
        vt100::Parser::new_with_callbacks(24, 80, 0, KittyState::new(enabled))
    }

    fn flags(p: &vt100::Parser<KittyState>) -> u8 {
        p.callbacks().flags(p.screen().alternate_screen())
    }

    #[test]
    fn push_pop_and_query_track_the_stack() {
        let mut p = parser(true);
        p.process(b"\x1b[?u");
        assert_eq!(p.callbacks_mut().take_replies(), b"\x1b[?0u");
        p.process(b"\x1b[>1u\x1b[?u");
        assert_eq!(flags(&p), DISAMBIGUATE);
        assert_eq!(p.callbacks_mut().take_replies(), b"\x1b[?1u");
        p.process(b"\x1b[<u");
        assert_eq!(flags(&p), 0);
        // Popping past the bottom is harmless.
        p.process(b"\x1b[<5u");
        assert_eq!(flags(&p), 0);
    }

    #[test]
    fn unsupported_flags_are_masked_off() {
        let mut p = parser(true);
        // disambiguate | event types | alternate keys | all keys as escapes
        p.process(b"\x1b[>15u\x1b[?u");
        assert_eq!(flags(&p), DISAMBIGUATE);
        assert_eq!(p.callbacks_mut().take_replies(), b"\x1b[?1u");
        p.process(b"\x1b[>2u");
        assert_eq!(flags(&p), 0);
    }

    #[test]
    fn set_modes_replace_or_and_clear() {
        let mut p = parser(true);
        p.process(b"\x1b[=1u");
        assert_eq!(flags(&p), DISAMBIGUATE);
        p.process(b"\x1b[=1;3u");
        assert_eq!(flags(&p), 0);
        p.process(b"\x1b[=1;2u");
        assert_eq!(flags(&p), DISAMBIGUATE);
        p.process(b"\x1b[=0;1u");
        assert_eq!(flags(&p), 0);
    }

    #[test]
    fn main_and_alternate_screens_have_their_own_stacks() {
        let mut p = parser(true);
        p.process(b"\x1b[?1049h\x1b[>1u");
        assert_eq!(flags(&p), DISAMBIGUATE);
        // Leaving without popping: the main screen never saw the push.
        p.process(b"\x1b[?1049l");
        assert_eq!(flags(&p), 0);
        p.callbacks_mut().reset_alt();
        p.process(b"\x1b[?1049h");
        assert_eq!(flags(&p), 0, "a stale alt stack must not leak into the next app");
    }

    #[test]
    fn stack_depth_is_bounded() {
        let mut p = parser(true);
        for _ in 0..(MAX_DEPTH + 4) {
            p.process(b"\x1b[>1u");
        }
        assert_eq!(p.callbacks().main.0.len(), MAX_DEPTH);
    }

    #[test]
    fn disabled_ignores_everything() {
        let mut p = parser(false);
        p.process(b"\x1b[>1u\x1b[?u");
        assert_eq!(flags(&p), 0);
        assert!(p.callbacks_mut().take_replies().is_empty());
    }

    fn enc(code: KeyCode, mods: KeyModifiers) -> Option<Vec<u8>> {
        encode_key(KeyEvent::new(code, mods), DISAMBIGUATE)
    }

    #[test]
    fn disambiguated_keys_use_csi_u() {
        let ctrl = KeyModifiers::CONTROL;
        let shift = KeyModifiers::SHIFT;
        let alt = KeyModifiers::ALT;
        for (code, mods, want) in [
            (KeyCode::Esc, KeyModifiers::NONE, "\x1b[27u"),
            (KeyCode::Char('m'), ctrl, "\x1b[109;5u"),
            (KeyCode::Char('o'), ctrl, "\x1b[111;5u"),
            (KeyCode::Char('['), ctrl, "\x1b[91;5u"),
            (KeyCode::Char(' '), ctrl, "\x1b[32;5u"),
            (KeyCode::Char('X'), ctrl | shift, "\x1b[120;6u"),
            (KeyCode::Char('b'), alt, "\x1b[98;3u"),
            (KeyCode::Char('c'), ctrl | alt, "\x1b[99;7u"),
            (KeyCode::Char('s'), KeyModifiers::SUPER, "\x1b[115;9u"),
            (KeyCode::Enter, shift, "\x1b[13;2u"),
            (KeyCode::Enter, ctrl, "\x1b[13;5u"),
            (KeyCode::Tab, ctrl, "\x1b[9;5u"),
            (KeyCode::BackTab, shift, "\x1b[9;2u"),
            (KeyCode::BackTab, KeyModifiers::NONE, "\x1b[9;2u"),
            (KeyCode::Backspace, alt, "\x1b[127;3u"),
        ] {
            assert_eq!(enc(code, mods), Some(want.as_bytes().to_vec()), "{code:?} {mods:?}");
        }
    }

    #[test]
    fn keys_legacy_already_encodes_unambiguously_fall_back() {
        for (code, mods) in [
            (KeyCode::Char('a'), KeyModifiers::NONE),
            (KeyCode::Char('A'), KeyModifiers::SHIFT),
            (KeyCode::Enter, KeyModifiers::NONE),
            (KeyCode::Tab, KeyModifiers::NONE),
            (KeyCode::Backspace, KeyModifiers::NONE),
            (KeyCode::Up, KeyModifiers::NONE),
            (KeyCode::F(5), KeyModifiers::NONE),
        ] {
            assert_eq!(enc(code, mods), None, "{code:?} {mods:?}");
        }
    }

    #[test]
    fn no_flags_means_legacy() {
        let key = KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL);
        assert_eq!(encode_key(key, 0), None);
    }

    #[test]
    fn a_query_split_across_reads_is_still_answered() {
        let mut p = parser(true);
        p.process(b"\x1b[");
        p.process(b"?u");
        assert_eq!(p.callbacks_mut().take_replies(), b"\x1b[?0u");
    }
}
