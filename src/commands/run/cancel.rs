//! Cancelling a `run`/`rebuild` (docs/instance-phase.md): SIGINT/SIGTERM
//! while one materializes set a flag instead of killing the process, so the
//! command can roll back (`run`) or record `failed` (`rebuild`) before it
//! exits. The terminal's Ctrl-C also reaches the children in the foreground
//! process group, which die, so the step in progress errors out; a SIGTERM to
//! us alone leaves them running, which is why [`check`] runs at every phase
//! boundary too. The first signal restores the default disposition, so a
//! second one kills as before (a rollback that hangs can still be escaped).
//! A SIGTERM still ends the process once the cleanup is done.

use anyhow::{bail, Result};

/// What a cancelled command's error and `failed` record say.
pub(crate) const CANCELLED: &str = "cancelled";

/// Catches SIGINT/SIGTERM while alive and puts back what was there before
/// (the default for the CLI, whatever the dashboard set when it runs a verb
/// in-process) when dropped. Guards may overlap (concurrent `run`s in one
/// process, e.g. tests): the first installs, the last restores.
pub(crate) struct Guard(());

/// Start catching SIGINT/SIGTERM, with a clear cancel flag. No-op off unix.
pub(crate) fn catch() -> Guard {
    #[cfg(unix)]
    imp::acquire();
    Guard(())
}

impl Drop for Guard {
    fn drop(&mut self) {
        #[cfg(unix)]
        imp::release();
    }
}

/// Whether a cancel signal arrived since the outermost live [`Guard`] was made.
pub(crate) fn requested() -> bool {
    #[cfg(unix)]
    {
        imp::requested()
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// The phase-boundary checkpoint: a cancel that arrived while the last step
/// still completed stops the command here.
pub(crate) fn check() -> Result<()> {
    if requested() {
        bail!(CANCELLED);
    }
    Ok(())
}

#[cfg(unix)]
mod imp {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    use libc::c_int;

    /// The signals that cancel.
    const CANCEL: [c_int; 2] = [libc::SIGINT, libc::SIGTERM];

    /// One flag per signal number, so the handler can be exercised on a
    /// signal nothing else uses (the tests) without cancelling a real run.
    static RAISED: [AtomicBool; 65] = [const { AtomicBool::new(false) }; 65];

    /// Live guards, and the dispositions the first one replaced.
    static INSTALLED: Mutex<(usize, Vec<(c_int, libc::sigaction)>)> = Mutex::new((0, Vec::new()));

    pub(super) fn requested() -> bool {
        CANCEL.iter().any(|&sig| raised(sig))
    }

    fn raised(sig: c_int) -> bool {
        RAISED.get(sig as usize).is_some_and(|f| f.load(Ordering::SeqCst))
    }

    fn clear(sig: c_int) {
        if let Some(f) = RAISED.get(sig as usize) {
            f.store(false, Ordering::SeqCst);
        }
    }

    pub(super) fn acquire() {
        let mut installed = INSTALLED.lock().unwrap_or_else(|e| e.into_inner());
        if installed.0 == 0 {
            for sig in CANCEL {
                clear(sig);
                match catch(sig) {
                    Ok(prev) => installed.1.push((sig, prev)),
                    Err(e) => eprintln!("warning: cannot catch signal {sig}; Ctrl-C won't roll back: {e}"),
                }
            }
        }
        installed.0 += 1;
    }

    /// A SIGTERM that found the default disposition is re-raised once it's
    /// back: SIGTERM asks the process to end, and the cleanup was only what
    /// it waited for (`serve` running autostart's `run` must still stop on
    /// `systemctl stop`). SIGINT cancels the command only, so the dashboard
    /// running a verb in-process survives a Ctrl-C.
    pub(super) fn release() {
        let mut installed = INSTALLED.lock().unwrap_or_else(|e| e.into_inner());
        installed.0 = installed.0.saturating_sub(1);
        if installed.0 > 0 {
            return;
        }
        let mut terminate = false;
        for (sig, prev) in installed.1.drain(..) {
            terminate |= sig == libc::SIGTERM && raised(sig) && prev.sa_sigaction == libc::SIG_DFL;
            restore(sig, &prev);
            clear(sig);
        }
        drop(installed);
        if terminate {
            // SAFETY: the default disposition is back: this ends the process.
            unsafe { libc::raise(libc::SIGTERM) };
        }
    }

    /// Only async-signal-safe work: an atomic store and `sigaction`. A cancel
    /// signal puts both back to the default, so the next one of either kills
    /// (`SA_RESETHAND` alone would only reset the one received).
    extern "C" fn on_signal(sig: c_int) {
        if let Some(f) = RAISED.get(sig as usize) {
            f.store(true, Ordering::SeqCst);
        }
        if CANCEL.contains(&sig) {
            for other in CANCEL {
                // SAFETY: `signal` is async-signal-safe; SIG_DFL is valid.
                unsafe { libc::signal(other, libc::SIG_DFL) };
            }
        }
    }

    /// Route `sig` to [`on_signal`], once (`SA_RESETHAND`); returns the
    /// disposition it replaced. `SA_RESTART`: the dashboard's other threads
    /// must not see their syscalls fail with EINTR.
    pub(super) fn catch(sig: c_int) -> std::io::Result<libc::sigaction> {
        // SAFETY: a zeroed `sigaction` is a valid starting point; the handler
        // is an `extern "C" fn(c_int)`, as `sa_sigaction` without SA_SIGINFO
        // expects.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_signal as extern "C" fn(c_int) as libc::sighandler_t;
            action.sa_flags = libc::SA_RESETHAND | libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            let mut prev: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(sig, &action, &mut prev) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(prev)
        }
    }

    pub(super) fn restore(sig: c_int, prev: &libc::sigaction) {
        // SAFETY: `prev` came from `sigaction` itself.
        unsafe { libc::sigaction(sig, prev, std::ptr::null_mut()) };
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        static PREVIOUS_RAN: AtomicBool = AtomicBool::new(false);

        extern "C" fn previous(_: c_int) {
            PREVIOUS_RAN.store(true, Ordering::SeqCst);
        }

        fn current(sig: c_int) -> libc::sighandler_t {
            // SAFETY: a null new action only queries.
            unsafe {
                let mut cur: libc::sigaction = std::mem::zeroed();
                libc::sigaction(sig, std::ptr::null(), &mut cur);
                cur.sa_sigaction
            }
        }

        /// On SIGUSR2, not SIGINT/SIGTERM: the test harness runs tests
        /// concurrently, and a docker test's `run` may hold a real guard
        /// whose flag (and dispositions) this must not touch. Same handler
        /// and install/restore code as the real signals.
        #[test]
        fn handler_flags_once_then_the_previous_disposition_comes_back() {
            let sig = libc::SIGUSR2;
            let previous = previous as extern "C" fn(c_int) as libc::sighandler_t;
            // SAFETY: installing a handler that only stores an atomic.
            let original = unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = previous;
                libc::sigemptyset(&mut action.sa_mask);
                let mut original: libc::sigaction = std::mem::zeroed();
                assert_eq!(libc::sigaction(sig, &action, &mut original), 0);
                original
            };

            let replaced = catch(sig).unwrap();
            assert_eq!(replaced.sa_sigaction, previous);
            assert!(!raised(sig));
            // SAFETY: our handler is installed, so this doesn't kill.
            unsafe { libc::raise(sig) };
            assert!(raised(sig));
            assert!(!PREVIOUS_RAN.load(Ordering::SeqCst));
            // Caught once: the default is back for a second signal.
            assert_eq!(current(sig), libc::SIG_DFL);

            restore(sig, &replaced);
            clear(sig);
            assert_eq!(current(sig), previous);
            // SAFETY: `previous` is installed again.
            unsafe { libc::raise(sig) };
            assert!(PREVIOUS_RAN.load(Ordering::SeqCst));
            assert!(!raised(sig));

            restore(sig, &original);
        }
    }
}
