//! The idle exit (docs/inbox-redesign.md "Idle exit"): the daemon exits after
//! [`IDLE_TIMEOUT`] with no holder; a new holder cancels the countdown. Pure,
//! so the daemon loop only snapshots [`Holders`] and acts on the [`Decision`].

use std::time::{Duration, Instant};

pub const IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// What keeps the daemon alive, one field per row of the holder table. Only
/// `clients` is counted so far; the rest stay 0 until their step fills them.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Holders {
    /// Connected clients: TUI, `api --stdio`, an `inbox` command, an `exec`
    /// session using the ssh-agent relay.
    pub clients: usize,
    /// Active port forwards (step 8).
    pub forwards: usize,
    /// Running instances declaring `dispatcher` or `inbox = true` (step 5).
    pub live_instances: usize,
    /// In-flight control requests (step 5).
    pub control: usize,
    /// `--follow` subscribers (step 18).
    pub followers: usize,
}

impl Holders {
    pub fn any(&self) -> bool {
        let Holders { clients, forwards, live_instances, control, followers } = *self;
        clients + forwards + live_instances + control + followers > 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Held (or keep-alive): no countdown running.
    Hold,
    /// Idle, exiting in this long unless a holder shows up.
    Wait(Duration),
    Exit,
}

/// `idle_since` is the last moment a holder was seen (the daemon's start,
/// or when the last holder left). `keep_alive` disables the exit; `timeout`
/// is [`IDLE_TIMEOUT`] outside tests.
pub fn decide(holders: &Holders, idle_since: Instant, now: Instant, keep_alive: bool, timeout: Duration) -> Decision {
    if keep_alive || holders.any() {
        return Decision::Hold;
    }
    let idle = now.saturating_duration_since(idle_since);
    match timeout.checked_sub(idle) {
        Some(left) if !left.is_zero() => Decision::Wait(left),
        _ => Decision::Exit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Duration = IDLE_TIMEOUT;

    #[test]
    fn counts_down_then_exits_with_no_holder() {
        let start = Instant::now();
        let none = Holders::default();
        assert_eq!(decide(&none, start, start, false, T), Decision::Wait(T));
        let at = |s| start + Duration::from_secs(s);
        assert_eq!(decide(&none, start, at(60), false, T), Decision::Wait(T - Duration::from_secs(60)));
        assert_eq!(decide(&none, start, at(599), false, T), Decision::Wait(Duration::from_secs(1)));
        assert_eq!(decide(&none, start, at(600), false, T), Decision::Exit);
        assert_eq!(decide(&none, start, at(3600), false, T), Decision::Exit);
    }

    #[test]
    fn any_holder_row_holds() {
        let start = Instant::now();
        let late = start + T * 2;
        let rows = [
            Holders { clients: 1, ..Default::default() },
            Holders { forwards: 1, ..Default::default() },
            Holders { live_instances: 1, ..Default::default() },
            Holders { control: 1, ..Default::default() },
            Holders { followers: 1, ..Default::default() },
        ];
        for h in rows {
            assert_eq!(decide(&h, start, late, false, T), Decision::Hold, "{h:?}");
        }
    }

    #[test]
    fn keep_alive_never_exits() {
        let start = Instant::now();
        assert_eq!(decide(&Holders::default(), start, start + T * 10, true, T), Decision::Hold);
    }

    #[test]
    fn a_new_idle_since_restarts_the_countdown() {
        let start = Instant::now();
        let left = start + Duration::from_secs(590);
        assert_eq!(decide(&Holders::default(), left, left, false, T), Decision::Wait(T));
        // A clock reading before idle_since (racing threads) is not negative idle.
        assert_eq!(decide(&Holders::default(), left, start, false, T), Decision::Wait(T));
    }
}
