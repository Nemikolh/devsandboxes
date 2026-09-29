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

    #[test]
    fn a_query_split_across_reads_is_still_answered() {
        let mut p = parser(true);
        p.process(b"\x1b[");
        p.process(b"?u");
        assert_eq!(p.callbacks_mut().take_replies(), b"\x1b[?0u");
    }
}
