//! Interactive dashboard (`devsandbox` with no subcommand on a TTY).
//!
//! This module owns all terminal I/O: raw mode, the alternate screen, the
//! panic hook that restores them, and the crossterm event loop. The state
//! machine ([`app::App`]) and rendering ([`ui::draw`]) stay I/O-free.
//!
//! Unix-only (`main.rs`): the dashboard is a client of devsandbox serve,
//! which is; off unix it only says so.

mod app;
mod clipboard;
mod daemon;
mod data;
mod kitty;
mod markdown;
mod poll;
mod procs;
mod prompt;
mod select;
mod settings;
mod spec;
mod term;
mod textarea;
mod ui;

use std::collections::BTreeMap;
use std::io::{self, Stdout, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::layout::Rect;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use crossterm::execute;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::commands;
use crate::commands::vscode::Goto;
use crate::inbox;

use app::{App, VerbEnd};
use poll::{PollUpdate, PollWorker};
use prompt::PromptAction;

/// How long each `event::poll` blocks before we redraw when no terminal is open.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Shorter poll used while any integrated terminal is open. Key events already
/// wake `event::poll`, but child *output* arrives on the reader threads and only
/// becomes visible on the next draw — a 250ms cadence makes shell echo feel
/// laggy, so we redraw ~33×/s to keep echo snappy while terminals exist.
const TERM_POLL_INTERVAL: Duration = Duration::from_millis(30);
/// Data-refresh cadence: how often a background collection is kicked off.
const TICK_INTERVAL: Duration = Duration::from_secs(2);
/// Cpu/mem cadence: at most one stats collection per interval, the ticks in
/// between are listing-only (`StatsClock`). `stats --no-stream` samples every
/// running container twice, the bulk of the TUI's dockerd load.
const STATS_INTERVAL: Duration = Duration::from_secs(7);
/// Process-refresh cadence for expanded instances (separate from the snapshot).
const PROC_TICK: Duration = Duration::from_secs(5);
/// Delay of the second refresh after a start/stop finishes. The immediate one
/// can be skipped (a stale collection already in flight) or land before the
/// containers settle (`health: starting`), so re-poll shortly after.
const FOLLOWUP_DELAY: Duration = Duration::from_millis(500);

/// One-shot delayed refresh armed when a start/stop finishes. Kept separate
/// from the event loop so the timing is testable without a runtime.
#[derive(Default)]
struct FollowUp {
    due: Option<Instant>,
}

impl FollowUp {
    /// (Re)arm for `now + FOLLOWUP_DELAY`; a later op pushes the deadline out so
    /// back-to-back ops collapse into one follow-up after the last.
    fn schedule(&mut self, now: Instant) {
        self.due = Some(now + FOLLOWUP_DELAY);
    }

    /// True once due and no collection is in flight (`busy`), disarming it. A
    /// due follow-up stays armed while busy, so it runs as soon as the slot
    /// frees instead of being dropped like a plain tick.
    fn take_due(&mut self, now: Instant, busy: bool) -> bool {
        match self.due {
            Some(due) if now >= due && !busy => {
                self.due = None;
                true
            }
            _ => false,
        }
    }
}

/// Ask for a refresh snapshot, with stats only when `clock` says they're due,
/// unless one is outstanding (`busy`): docker calls can't pile up.
fn refresh(poll: &PollWorker, busy: &mut bool, clock: &mut StatsClock) {
    if !*busy {
        poll.request_snapshot(clock.depth(Instant::now()));
        *busy = true;
    }
}

/// [`refresh`] for something that just changed what the snapshot shows
/// (a rename, a suspended command): one already outstanding may predate the
/// change, so a follow-up asks again once it's in.
fn refresh_after_change(poll: &PollWorker, busy: &mut bool, clock: &mut StatsClock, followup: &mut FollowUp) {
    if *busy {
        followup.schedule(Instant::now());
    } else {
        refresh(poll, busy, clock);
    }
}

/// Picks each collection's depth so stats run at most every `STATS_INTERVAL`.
/// Kept separate from the event loop so the timing is testable without a
/// runtime.
#[derive(Default)]
struct StatsClock {
    last: Option<Instant>,
}

impl StatsClock {
    /// `Full` (recording `now`) when stats are due, else `Listing`.
    fn depth(&mut self, now: Instant) -> data::Depth {
        if self.last.is_some_and(|last| now < last + STATS_INTERVAL) {
            return data::Depth::Listing;
        }
        self.last = Some(now);
        data::Depth::Full
    }
}

type Term = Terminal<CrosstermBackend<Stdout>>;

/// Whether the outer terminal speaks the kitty keyboard protocol, probed once
/// in [`setup`]. Without it the integrated terminal can't tell e.g. ctrl+m from
/// Enter (both are `\r` in legacy encoding), so inner apps that bind such
/// chords never see them. Statics rather than `App` fields because
/// [`restore`] also runs from the panic hook.
static KITTY: AtomicBool = AtomicBool::new(false);
/// Whether our flags are currently pushed, so [`restore`] pops exactly once:
/// it runs on suspend, on exit and possibly again from the panic hook, and an
/// extra pop would clobber flags the user's shell pushed on the main screen.
static KITTY_PUSHED: AtomicBool = AtomicBool::new(false);

/// Launch the dashboard, guaranteeing the terminal is restored on every exit
/// path (normal, error, or panic).
pub fn dashboard(dir: &Path) -> Result<()> {
    install_panic_hook();
    // No autostart pass of our own: the daemon runs it (docs/dashboard-client.md, F16).
    let mut terminal = setup().context("failed to set up terminal")?;

    let result = run(&mut terminal, App::new(dir.to_path_buf()));

    // Restore on both the Ok and Err paths before returning.
    restore();
    result
}

/// Enable raw mode, enter the alternate screen, build the ratatui terminal.
fn setup() -> Result<Term> {
    enable_raw_mode()?;
    // crossterm sends `CSI ? u` followed by DA1, which every terminal answers,
    // so an unsupporting terminal (gnome-terminal, tmux) replies at once
    // instead of hitting the 2s timeout.
    KITTY.store(matches!(supports_keyboard_enhancement(), Ok(true)), Ordering::Relaxed);
    enter()?;
    let terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    Ok(terminal)
}

/// Enter the alternate screen with mouse capture and, when supported, kitty
/// keyboard flags. Shared by [`setup`] and the re-entry after a suspended
/// command. Only `DISAMBIGUATE_ESCAPE_CODES`: event types would add
/// release/repeat events the dashboard doesn't want, and it is the one flag the
/// integrated terminal emulates for inner apps. Pushed after entering the
/// alternate screen because kitty keeps one flag stack per screen.
fn enter() -> io::Result<()> {
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    if KITTY.load(Ordering::Relaxed) {
        execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
        KITTY_PUSHED.store(true, Ordering::Relaxed);
    }
    Ok(())
}

/// Best-effort teardown: pop our kitty flags, leave the alternate screen and
/// disable raw mode. Errors are ignored — there is nothing useful to do while
/// cleaning up.
fn restore() {
    if KITTY_PUSHED.swap(false, Ordering::Relaxed) {
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
    let _ = disable_raw_mode();
}

/// Restore the terminal before the default panic hook prints, so a panic never
/// leaves the shell in raw mode / the alternate screen.
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        default(info);
    }));
}

fn run(terminal: &mut Term, mut app: App) -> Result<()> {
    let dir = app.dir.clone();
    app.utc_offset = inbox::clock::local_utc_offset();
    app.kitty = KITTY.load(Ordering::Relaxed);
    app.copy_key_intercepted = app::copy_key_intercepted(|var| std::env::var(var).ok());
    // Before the inbox, whose error would otherwise hide this one: an
    // invalid file runs on the defaults, and the first toggle overwrites it.
    match settings::load() {
        Ok(s) => app.settings = s,
        Err(e) => app.status = Some(format!("settings not loaded, using defaults: {e:#}")),
    }
    match inbox::store::path().and_then(|p| inbox::ops::load(&p)) {
        Ok(inbox) => app.set_inbox(inbox),
        Err(e) => app.status = Some(format!("inbox not loaded: {e:#}")),
    }
    // The store is shared with every other dashboard and the bridge worker:
    // reload whenever its mtime/size moved (a cheap stat each tick), or at
    // once when something here poked it.
    let mut inbox_stamp = inbox::store::path().ok().and_then(|p| inbox::ops::stamp(&p));
    let mut reload_inbox = false;
    // The daemon collects the snapshot and the processes (`poll.rs`), at
    // most one request of each outstanding (`pending` / `proc_pending`).
    // Startup walks the `Depth`s cheapest first, so the tree renders straight
    // from disk and statuses, then cpu/mem, fill in as the runtime answers;
    // `deeper` holds the depths still to chain (popped from the back).
    let poller = PollWorker::spawn(dir.clone());
    poller.request_snapshot(data::Depth::Disk);
    let mut pending = true;
    let mut deeper = vec![data::Depth::Full, data::Depth::Listing];
    let mut stats_clock = StatsClock::default();
    // Background VS Code launches (`o`, `:code`, a thread's `vscode`): a
    // `--goto` waits up to 30 s for the window. No guard: a second launch
    // just focuses the same window.
    let mut codes: Vec<Receiver<OpDone>> = Vec::new();
    // Verb requests to the daemon, a thread each until answered
    // (`spawn_verb`): `run`/`rebuild`/`start` detached, `stop`/`rm`/
    // `services.rebuild` held. Paired with their action so the answer
    // reaches `App` (guards, rm's question) even if the thread dies.
    let mut verbs: Vec<(PromptAction, Receiver<VerbEnd>)> = Vec::new();
    let mut last_tick = Instant::now();
    let mut proc_pending = false;
    // Set while a procs failure the daemon answered is shown, so it's one
    // status line per streak. A daemon that can't be reached isn't one:
    // `daemon.rs` already says so.
    let mut procs_failing = false;
    let mut last_proc_tick = Instant::now();
    let mut followup = FollowUp::default();
    // Set while the follow-up's collection runs: proc targets come from the
    // snapshot, so the proc refresh must wait for it to land.
    let mut procs_after_snapshot = false;
    // The host daemon (docs/serve.md) owns the bridges: ssh-agent relays,
    // the notify drain, popups and control ops. Its worker thread
    // (`daemon.rs`) holds the connection for the dashboard's life (so the
    // dashboard is a holder), reconnecting after a handoff or a death, and
    // relays its notifications. Without one the dashboard works on the
    // store alone, minus the live features. The daemon also owns the
    // Ports tab's forwards (they outlive the dashboard); the worker relays
    // adds/stops and this root's rows.
    let daemon = daemon::DaemonWorker::spawn(dir.clone());

    while !app.should_quit {
        // Full-frame area, shared by the pre-draw PTY resize and mouse routing.
        let size = terminal.size()?;
        let area = Rect::new(0, 0, size.width, size.height);

        // Pre-draw: size every PTY to the panel the draw path is about to lay
        // out, so the shell's winsize matches what gets rendered. Resize is a
        // cheap no-op when unchanged; every tab (not just the active one) is
        // sized so a background tab is already correct when switched to.
        if !app.terms.is_empty() {
            if let Some((rows, cols)) = ui::term_pane_size(area, app.prompt.is_some()) {
                app.terms.resize_all(rows, cols);
            }
        }

        terminal.draw(|frame| ui::draw(frame, &app))?;
        // A released selection was read from the frame just drawn; send it
        // to the outer terminal's clipboard. OSC 52 has no reply, hence the
        // status line.
        if let Some(text) = app.take_clipboard() {
            let sent = clipboard::cap(&text);
            let mut out = io::stdout();
            out.write_all(&clipboard::osc52(sent, std::env::var_os("TMUX").is_some()))?;
            out.flush()?;
            let n = sent.chars().count();
            app.status = Some(if sent.len() < text.len() {
                format!("copied {n} chars (cut at 1 MiB)")
            } else {
                format!("copied {n} chars")
            });
        }
        // A release with copy on select off: say what a copy key would copy.
        if let Some(hint) = app.take_selected_hint() {
            app.status = Some(hint);
        }
        // Apps in the integrated terminal copy with OSC 52 to *their*
        // terminal, our vt100 emulator, whose callbacks queue it; relay it to
        // the outer one (unless the `terminal_clipboard` setting is off).
        if let Some((title, b64)) = app.take_terminal_clipboard() {
            let mut out = io::stdout();
            out.write_all(&clipboard::osc52_base64(&b64, std::env::var_os("TMUX").is_some()))?;
            out.flush()?;
            app.status = Some(format!("copied from {title}"));
        }

        // Shorter cadence while terminals are open so shell echo stays snappy.
        let poll = if app.terms.is_empty() {
            POLL_INTERVAL
        } else {
            TERM_POLL_INTERVAL
        };
        if event::poll(poll)? {
            match event::read()? {
                Event::Key(key) if key.kind == event::KeyEventKind::Press => app.on_key(key),
                // Feed the full frame area so the modal divider math and the
                // terminal-panel hit-testing stay I/O-free.
                Event::Mouse(ev) => app.on_mouse(&ev, area),
                _ => {}
            }
        }
        // A toggle in the `?` modal: persist it now, best-effort (the toggle
        // holds for this session either way).
        if let Some(s) = app.take_settings_save() {
            if let Err(e) = settings::save(&s) {
                app.status = Some(format!("settings not saved: {e:#}"));
            }
        }

        // A prompt command or key action is ready: send a verb to the
        // daemon, launch `code` in the background, or suspend the TUI for
        // `exec` (then force a redraw + immediate refresh on return).
        if let Some(action) = app.take_pending_action() {
            match action {
                // devsandbox serve runs the verb (a build outlives the
                // dashboard and logs itself); a build's row shows the phase
                // as soon as it's recorded.
                action @ (PromptAction::Run { .. }
                | PromptAction::Rebuild { .. }
                | PromptAction::Start { .. }
                | PromptAction::Stop { .. }
                | PromptAction::Rm { .. }
                | PromptAction::ServiceRebuild { .. }) => {
                    app.status = Some(verb_pending(&action));
                    verbs.push((action.clone(), spawn_verb(&dir, action)));
                    last_tick = Instant::now();
                    followup.schedule(last_tick);
                    refresh(&poller, &mut pending, &mut stats_clock);
                }
                PromptAction::Code { instance, goto } => {
                    app.status = Some("opening VS Code…".into());
                    codes.push(spawn_code(dir.clone(), instance, goto, app.code_note.take()));
                }
                // `instances.rename`, no suspend; the answer (a status line,
                // `DaemonUpdate::Renamed`) resnapshots so the renamed row
                // shows then.
                PromptAction::Rename { instance, new_name } => daemon.rename(instance, new_name),
                other => {
                    run_suspended(terminal, other)?;
                    // Redraw from scratch: the child scribbled over the screen.
                    terminal.clear()?;
                    // Refresh data now rather than waiting for the next tick.
                    last_tick = Instant::now();
                    refresh_after_change(&poller, &mut pending, &mut stats_clock, &mut followup);
                }
            }
        }

        // Process-row SIGTERM/SIGKILL, done flags, modal reads: one-shot
        // requests on the daemon worker's connection, answered below.
        if let Some(sig) = app.take_pending_signal() {
            daemon.signal(sig);
        }
        for req in app.take_pending_done() {
            daemon.set_done(req);
        }
        for fetch in app.take_pending_fetch() {
            daemon.fetch(fetch);
        }
        // `d` on an in-flight row, confirmed: the daemon SIGINTs its build
        // (`instances.cancel`, whoever started it).
        if let Some(instance) = app.take_pending_cancel() {
            daemon.cancel(instance);
            followup.schedule(Instant::now());
        }
        // An open build log wants (re)reading (one read outstanding at a
        // time: `take_build_log_read`).
        if let Some(id) = app.take_build_log_read() {
            daemon.build_log(id);
        }
        // An Inbox link: hand it to the desktop opener.
        if let Some(link) = app.take_pending_open() {
            if let Err(status) = open_link(&link) {
                app.status = Some(status);
            }
        }

        // Ports tab: hand add/remove requests to the daemon worker (the daemon
        // owns the forwards) and drain its updates into the app.
        if let Some(req) = app.take_pending_port() {
            daemon.forward_add(req);
        }
        if let Some(id) = app.take_pending_unport() {
            daemon.forward_rm(id);
        }
        while let Some(update) = daemon.try_recv() {
            match update {
                daemon::DaemonUpdate::InboxChanged => reload_inbox = true,
                daemon::DaemonUpdate::Ports(rows) => app.set_ports(rows),
                // The daemon's 5 s poll saw containers come or go:
                // resnapshot soon rather than on the next tick.
                daemon::DaemonUpdate::InstancesChanged => followup.schedule(Instant::now()),
                daemon::DaemonUpdate::Shown(line) | daemon::DaemonUpdate::Status(line) => app.status = Some(line),
                daemon::DaemonUpdate::Logs { container, body } => app.set_logs(&container, body),
                daemon::DaemonUpdate::Config { kind, name, text } => app.set_config(kind, &name, text),
                daemon::DaemonUpdate::Inspect { container, body } => app.set_inspect(&container, body),
                daemon::DaemonUpdate::BuildLog { instance, read } => app.set_build_log(&instance, read),
                // Refresh the processes now so a killed one drops off the
                // forest without waiting for the next tick.
                daemon::DaemonUpdate::Signalled(line) => {
                    app.status = Some(line);
                    app.needs_proc_fetch = true;
                }
                daemon::DaemonUpdate::Renamed(line) => {
                    app.status = Some(line);
                    last_tick = Instant::now();
                    refresh_after_change(&poller, &mut pending, &mut stats_clock, &mut followup);
                }
                // A thread's child reports only on failure; resnapshot so the
                // row dims or brightens now.
                daemon::DaemonUpdate::Done { status, failed, report } => {
                    if report || failed {
                        app.status = Some(status);
                    }
                    last_tick = Instant::now();
                    followup.schedule(last_tick);
                    refresh(&poller, &mut pending, &mut stats_clock);
                }
            }
        }
        // The Inbox lives in one shared file (`crate::inbox::store`), so any
        // dashboard's change must reach this one: push what the keys asked
        // for, then reload when the store moved (ours or someone else's).
        // Terminals just opened on relay-mode instances: the daemon's bridge
        // relays this dashboard's agent. Disconnected: skipped (no fallback).
        let bridges = app.take_pending_bridges();
        if daemon.connected() {
            for instance in bridges {
                daemon.ensure_bridge(instance, crate::devsbd::bridge::own_agent());
            }
        }
        let ops = app.take_pending_inbox();
        let ops = if !ops.is_empty() && daemon.connected() {
            // Through the API; the worker says when they landed. No reload
            // before then: it would undo `App`'s optimistic copy.
            daemon.apply(ops);
            Vec::new()
        } else {
            ops
        };
        if !ops.is_empty() {
            // Stamped by `ops::apply`, under the store lock, not in `App`:
            // event ids and times are minted where every dashboard's writes
            // serialize.
            if let Err(e) = inbox::store::path().and_then(|p| inbox::ops::apply(&p, &ops, "tui")) {
                app.status = Some(format!("inbox not saved: {e:#}"));
            }
            reload_inbox = true;
        }
        let stamp = inbox::store::path().ok().and_then(|p| inbox::ops::stamp(&p));
        if reload_inbox || stamp != inbox_stamp {
            // Stamp before loading: a write in between only costs one more
            // reload, while stamping after could hide it until the next one.
            inbox_stamp = stamp;
            reload_inbox = false;
            match inbox::store::path().and_then(|p| inbox::ops::load(&p)) {
                Ok(loaded) => app.set_inbox(loaded),
                Err(e) => app.status = Some(format!("inbox not loaded: {e:#}")),
            }
        }

        codes.retain_mut(|rx| match rx.try_recv() {
            Ok(done) => {
                app.status = Some(done.status);
                false
            }
            Err(TryRecvError::Empty) => true,
            Err(TryRecvError::Disconnected) => false,
        });

        // Drain answered verb requests: hand the answer to `App`, and
        // resnapshot so the row shows the build's phase, the stop, the
        // removal.
        verbs.retain_mut(|(action, rx)| {
            let end = match rx.try_recv() {
                Ok(end) => end,
                Err(TryRecvError::Empty) => return true,
                // Thread died without sending: clear the guards all the same.
                Err(TryRecvError::Disconnected) => {
                    VerbEnd::Done { status: format!("{}: no answer", verb_label(action)), failed: true }
                }
            };
            app.verb_answered(action, end);
            last_tick = Instant::now();
            followup.schedule(last_tick);
            refresh(&poller, &mut pending, &mut stats_clock);
            false
        });

        // Drain the daemon's answers into the app, freeing their slots.
        while let Some(update) = poller.try_recv() {
            match update {
                PollUpdate::Snapshot(answer) => {
                    pending = false;
                    match answer {
                        Ok(list) => {
                            app.snapshot_error = None;
                            app.thread_children = list.thread_children;
                            app.set_snapshot(list.snapshot);
                            if let Some(depth) = deeper.pop() {
                                if depth == data::Depth::Full {
                                    stats_clock.last = Some(Instant::now());
                                }
                                poller.request_snapshot(depth);
                                pending = true;
                            }
                            if procs_after_snapshot {
                                procs_after_snapshot = false;
                                app.needs_proc_fetch = true;
                            }
                        }
                        // The last snapshot stays, marked stale with why
                        // (none yet: the tree says it); the next tick retries.
                        Err(e) => app.snapshot_error = Some(e),
                    }
                }
                PollUpdate::Procs(answer) => {
                    proc_pending = false;
                    match answer {
                        Ok(procs) => {
                            procs_failing = false;
                            app.apply_proc_fetch(procs);
                        }
                        // The last rows stay.
                        Err(e) => {
                            if !e.unreachable && !procs_failing {
                                app.status = Some(format!("processes: {}", e.reason));
                            }
                            procs_failing = !e.unreachable;
                        }
                    }
                }
            }
        }

        if followup.take_due(Instant::now(), pending) {
            last_tick = Instant::now();
            refresh(&poller, &mut pending, &mut stats_clock);
            procs_after_snapshot = true;
        }

        if last_tick.elapsed() >= TICK_INTERVAL {
            last_tick = Instant::now();
            refresh(&poller, &mut pending, &mut stats_clock);
        }

        // Process refresh: fetch now on expand / cursor move, else every
        // PROC_TICK, whenever no fetch is already running. Targets are the
        // expanded instances plus the selected running one (for the Detail agent
        // count); an empty set means there is nothing to fetch.
        let want_now = app.take_needs_proc_fetch();
        let tick_due = last_proc_tick.elapsed() >= PROC_TICK;
        if (want_now || tick_due) && !proc_pending {
            last_proc_tick = Instant::now();
            // Non-running expanded instances get their `(not running)` row here;
            // running ones (and the selection) come back as fetch targets.
            let targets = app.proc_fetch_targets();
            if !targets.is_empty() {
                poller.request_procs(targets);
                proc_pending = true;
            }
        }
    }
    Ok(())
}

/// Leave the TUI, run a prompt command with inherited stdio, wait for one
/// keypress, then re-enter the TUI. Command errors are printed (not propagated)
/// so a failed `exec` returns the user to the dashboard.
fn run_suspended(terminal: &mut Term, action: PromptAction) -> Result<()> {
    restore();

    let result = match &action {
        PromptAction::Exec { instance, argv } => {
            commands::exec::exec_status(instance, true, true, argv).map(|_| ())
        }
        // The verbs go to the daemon (`spawn_verb`), `code`/`rename` never
        // suspend: the caller handles them. `port` never reaches here —
        // `App` routes it to `pending_port`, not `pending_action`.
        PromptAction::Run { .. }
        | PromptAction::Rebuild { .. }
        | PromptAction::Start { .. }
        | PromptAction::Stop { .. }
        | PromptAction::Rm { .. }
        | PromptAction::ServiceRebuild { .. }
        | PromptAction::Code { .. }
        | PromptAction::Rename { .. }
        | PromptAction::Port { .. } => Ok(()),
    };
    if let Err(e) = result {
        match log_error(&action, &e) {
            Some(path) => eprintln!("error: {e:#}\nlogged to {}", path.display()),
            None => eprintln!("error: {e:#}"),
        }
    }

    print!("\r\n\x1b[2mpress any key to return\x1b[0m");
    let _ = io::stdout().flush();
    // Raw mode first: cooked mode is line-buffered, so "any key" would
    // otherwise need an Enter before the event arrives.
    enable_raw_mode()?;
    wait_for_key();

    // Re-enter: the outer setup already ran once; re-arm the alt screen, mouse
    // and kitty flags.
    enter()?;
    terminal.hide_cursor()?;
    Ok(())
}

/// Write a failed suspended command's error to a timestamped log file under the
/// data dir (`<data>/devsandbox/logs/<verb>-<unix>.log`, alongside `state.toml`)
/// and return its path so the caller can point the user at it. Best-effort:
/// `None` on any I/O failure, so the caller falls back to a plain stderr print.
fn log_error(action: &PromptAction, err: &anyhow::Error) -> Option<PathBuf> {
    let verb = match action {
        PromptAction::Exec { .. } => "exec",
        // Never suspended (`run_suspended`): the daemon runs the verbs and
        // their status line (or build log) has the error.
        _ => return None,
    };
    let dir = crate::state::State::path().ok()?.parent()?.join("logs");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("{verb}-{}.log", crate::state::Instance::now()));
    std::fs::write(&path, format!("{verb} failed: {err:#}\n")).ok()?;
    Some(path)
}

/// Block until the next key press (consuming it), ignoring release/repeat.
fn wait_for_key() {
    loop {
        match event::read() {
            Ok(Event::Key(key)) if key.kind == event::KeyEventKind::Press => break,
            Ok(_) => continue,
            Err(_) => break,
        }
    }
}

/// Launch VS Code attached to `instance` (a snapshot row name, which equals
/// the state instance key), at `goto` when given. Returns a one-line status
/// for the help-bar.
fn launch_code(dir: &Path, instance: &str, goto: Option<&Goto>) -> String {
    let state = match crate::state::State::load() {
        Ok(s) => s,
        Err(e) => return format!("code: {e:#}"),
    };
    let Some(info) = state.instances.get(instance) else {
        return format!("code: unknown instance `{instance}`");
    };
    commands::vscode::launch(dir, instance, info, goto).unwrap_or_else(|e| format!("{e:#}"))
}

/// Spawn a detached thread running [`launch_code`] (a goto blocks while the
/// helper waits for the window) and reporting its status, with `note` (the
/// Inbox action's caveats, [`App::code_note`]) appended.
fn spawn_code(dir: PathBuf, instance: String, goto: Option<Goto>, note: Option<String>) -> Receiver<OpDone> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let outcome = launch_code(&dir, &instance, goto.as_ref());
        let status = match note {
            Some(note) => format!("{outcome} · {note}"),
            None => outcome,
        };
        let _ = tx.send(OpDone { status });
    });
    rx
}

/// `run web` / `rebuild web-2` / `start web-2` / `rm web-2` / `service
/// rebuild cache`: how the status line names a dashboard verb request.
fn verb_label(action: &PromptAction) -> String {
    match action {
        PromptAction::Run { sandbox, name: Some(name), .. } => format!("run {sandbox} --name {name}"),
        PromptAction::Run { sandbox, .. } => format!("run {sandbox}"),
        PromptAction::Rebuild { instance, .. } => format!("rebuild {instance}"),
        PromptAction::Start { instance } => format!("start {instance}"),
        PromptAction::Stop { instance } => format!("stop {instance}"),
        PromptAction::Rm { instance, .. } => format!("rm {instance}"),
        PromptAction::ServiceRebuild { name } => format!("service rebuild {name}"),
        _ => String::new(),
    }
}

/// The status line while a verb request is out: `s`'s "stopping x…" /
/// "starting x…" (as `App` words them), else the label.
fn verb_pending(action: &PromptAction) -> String {
    match action {
        PromptAction::Stop { instance } => format!("stopping {instance}…"),
        PromptAction::Start { instance } => format!("starting {instance}…"),
        PromptAction::Rm { instance, .. } => format!("removing {instance}…"),
        _ => format!("{}…", verb_label(action)),
    }
}

/// The instance a verb action names before it runs: `rebuild`/`start`/
/// `stop`/`rm`'s, `run --name`'s (a plain `run`'s is picked by the verb).
fn verb_instance(action: &PromptAction) -> Option<&str> {
    match action {
        PromptAction::Run { name, .. } => name.as_deref(),
        PromptAction::Rebuild { instance, .. }
        | PromptAction::Start { instance }
        | PromptAction::Stop { instance }
        | PromptAction::Rm { instance, .. } => Some(instance),
        _ => None,
    }
}

/// The held verbs: they end with the work (seconds), the request waits for
/// it, and they write no build log.
fn verb_held(action: &PromptAction) -> bool {
    matches!(action, PromptAction::Stop { .. } | PromptAction::Rm { .. } | PromptAction::ServiceRebuild { .. })
}

/// The verb request (method, params) for a dashboard verb action in config
/// root `root` (absolute) with the dashboard's `env`; `None` for the other
/// actions. `run`/`rebuild`/`start` always `detach`: the dashboard only
/// waits for the instance to be registered, its row shows the rest.
/// `stop`/`rm`/`services.rebuild` are held (they can't detach). An
/// unanswered `rm` asks (its branch question comes back as `confirm`); an
/// answered one carries `deleteBranch`. The target always goes in, so
/// nothing can need a pick.
fn verb_call(
    action: &PromptAction,
    root: &Path,
    env: BTreeMap<String, String>,
) -> Option<(&'static str, serde_json::Value)> {
    use crate::serve::verbs::{RebuildParams, RmParams, RunParams, ServicesRebuildParams, StartParams, StopParams};
    let root = root.to_path_buf();
    let (method, params) = match action.clone() {
        PromptAction::Run { sandbox, name, branch, base } => {
            ("run", serde_json::to_value(RunParams { root, env, sandbox, name, branch, base, json: false, detach: true }))
        }
        PromptAction::Rebuild { instance, force } => (
            "rebuild",
            serde_json::to_value(RebuildParams { root, env, instance: Some(instance), all: false, force, detach: true }),
        ),
        PromptAction::Start { instance } => {
            ("start", serde_json::to_value(StartParams { root, env, instance: Some(instance), all: false, detach: true }))
        }
        PromptAction::Stop { instance } => {
            ("stop", serde_json::to_value(StopParams { root, env, instance: Some(instance), all: false }))
        }
        PromptAction::Rm { instance, force, delete_branch } => (
            "rm",
            serde_json::to_value(RmParams { root, env, instance, delete_branch, force, ask: delete_branch.is_none() }),
        ),
        PromptAction::ServiceRebuild { name } => {
            ("services.rebuild", serde_json::to_value(ServicesRebuildParams { root, env, name }))
        }
        _ => return None,
    };
    Some((method, params.expect("verb params serialize")))
}

/// How a verb request ended, for [`App::verb_answered`]: rm's `confirm` (its
/// branch question, to a modal) for an rm that asked, else
/// [`verb_status`]'s line.
fn verb_end(action: &PromptAction, sent: Result<crate::serve::client::Streamed>) -> VerbEnd {
    use crate::serve::client::Streamed;
    if let (PromptAction::Rm { delete_branch: None, .. }, Ok(Streamed::Answer(r))) = (action, &sent)
        && let Some(e) = &r.error
        && e.code == "confirm"
    {
        return VerbEnd::Confirm(e.message.clone());
    }
    let (status, failed) = verb_status(action, sent);
    VerbEnd::Done { status, failed }
}

/// The status line (and whether it's a failure) for a verb request's end:
/// the detached answer names the instance and where its build log is; a
/// held verb's says what it did. A `failed` verb quotes its error; a build
/// verb's points at the build log (`b`) of its instance, which a plain
/// `run` only knows from the answer (else "its row": the failed row shows
/// up under the sandbox). Other errors (bad params, no such instance) ran
/// nothing, so no build log. `Err` is the daemon unreachable: no fallback
/// (the dashboard never builds in its own process, which would die with
/// it).
fn verb_status(action: &PromptAction, sent: Result<crate::serve::client::Streamed>) -> (String, bool) {
    use crate::serve::client::Streamed;
    let label = verb_label(action);
    let r = match sent {
        Ok(Streamed::Answer(r)) => r,
        // The daemon exited before answering (a handoff, a crash): the
        // verb runs on, its row shows it.
        Ok(Streamed::Lost) => return (format!("{label}: devsandbox serve went away; it keeps running"), false),
        // Never asked for (`spawn_verb` doesn't hang up); a hang-up cancels.
        Ok(Streamed::HungUp) => return (format!("{label}: cancelled"), true),
        Err(e) => return (format!("{label}: needs devsandbox serve ({e:#})"), true),
    };
    let named = verb_instance(action);
    match r.error {
        Some(e) if e.code == "failed" && !verb_held(action) => {
            let instance = e.data.as_ref().and_then(|d| d["instance"].as_str()).or(named);
            let hint = match instance {
                Some(instance) => format!("b on {instance} opens its build log"),
                None => "b on its row opens the build log".to_string(),
            };
            (format!("{label} failed: {} ({hint})", e.message), true)
        }
        Some(e) => (format!("{label} failed: {}", e.message), true),
        None => {
            let result = r.result.unwrap_or_default();
            if result["detached"] != true {
                // Held verbs end with the work; `start` with nothing to
                // build ends before it's registered: the normal result.
                let done = match action {
                    PromptAction::Stop { instance } => format!("stopped {instance}"),
                    PromptAction::Rm { instance, .. } => format!("removed {instance}"),
                    PromptAction::ServiceRebuild { name } => format!("rebuilt service {name}"),
                    _ => format!("{label}: done"),
                };
                return (done, false);
            }
            let status = match result["instance"].as_str() {
                Some(instance) if Some(instance) != named => format!("{label}: {instance} started"),
                _ => format!("{label}: started"),
            };
            (format!("{status} (b opens its build log)"), false)
        }
    }
}

/// Send verb `action` to devsandbox serve on a thread of its own: a
/// connection of its own too (lazy start, as the daemon worker's), since a
/// verb request holds its connection until answered. `output`/`progress`
/// are dropped (the build log has a build's, and nothing may reach the
/// terminal). A detached verb answers once the instance is registered, and
/// dropping the connection then leaves the build running; a held one
/// (`stop`, `rm`, `services.rebuild`) answers when done. Quitting the
/// dashboard before the answer hangs up, which cancels it (SIGINT).
fn spawn_verb(dir: &Path, action: PromptAction) -> Receiver<VerbEnd> {
    let (tx, rx) = mpsc::channel();
    let root = daemon::root_dir(dir.to_path_buf());
    std::thread::spawn(move || {
        use crate::serve::{client, endpoint, verbs_client};
        let env = verbs_client::caller_env(std::env::vars_os());
        let sent = verb_call(&action, &root, env).context("not a verb").and_then(|(method, params)| {
            let mut conn = endpoint::socket_dir().and_then(|socket| client::connect(&socket, "tui"))?;
            conn.call_streaming(method, params, Duration::from_secs(1), &mut |_| true)
        });
        let _ = tx.send(verb_end(&action, sent));
    });
    rx
}

/// Hand `link` to the desktop opener (`open` on macOS, `xdg-open` elsewhere),
/// stdio null so it can't scribble on the TUI, reaped on its own thread so
/// the loop never waits. `Err` carries a status line when it can't spawn.
fn open_link(link: &str) -> std::result::Result<(), String> {
    use std::process::{Command, Stdio};
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let mut child = Command::new(opener)
        .arg(link)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("open: {opener}: {e}"))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Result of a background VS Code launch, drained by the event loop.
struct OpDone {
    /// One-line outcome for the help-bar status.
    status: String,
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_clock_runs_stats_at_most_once_per_interval() {
        let t0 = Instant::now();
        let mut c = StatsClock::default();
        assert_eq!(c.depth(t0), data::Depth::Full, "first refresh has stats");
        assert_eq!(c.depth(t0 + TICK_INTERVAL), data::Depth::Listing);
        assert_eq!(c.depth(t0 + STATS_INTERVAL - Duration::from_millis(1)), data::Depth::Listing);
        assert_eq!(c.depth(t0 + STATS_INTERVAL), data::Depth::Full);
        assert_eq!(c.depth(t0 + STATS_INTERVAL + TICK_INTERVAL), data::Depth::Listing);
    }

    fn run_action(name: Option<&str>, branch: Option<&str>, base: Option<&str>) -> PromptAction {
        PromptAction::Run {
            sandbox: "web".into(),
            name: name.map(String::from),
            branch: branch.map(String::from),
            base: base.map(String::from),
        }
    }

    /// Each verb action's request carries the root and env and is what the
    /// daemon's `serve::verbs::parse` accepts; only `run`/`rebuild`/`start`
    /// detach.
    #[test]
    fn verb_calls_are_requests_the_daemon_accepts() {
        use crate::serve::verbs::{self, Target, Verb};
        let root = std::env::temp_dir().canonicalize().unwrap();
        let env: BTreeMap<String, String> = [("HOME".to_string(), "/home/me".to_string())].into();
        let parse = |action: &PromptAction, detach: bool| {
            let (method, params) = verb_call(action, &root, env.clone()).unwrap();
            let req = verbs::parse(method, params).unwrap();
            assert_eq!(req.detach, detach, "{action:?}");
            assert_eq!((req.root.as_path(), &req.env), (root.as_path(), &env));
            req.verb
        };
        assert_eq!(
            parse(&run_action(Some("w2"), Some("feat/x"), Some("-odd")), true),
            Verb::Run {
                sandbox: "web".into(),
                name: Some("w2".into()),
                branch: Some("feat/x".into()),
                base: Some("-odd".into()),
                json: false
            }
        );
        assert_eq!(
            parse(&PromptAction::Rebuild { instance: "web-2".into(), force: true }, true),
            Verb::Rebuild { target: Target::One("web-2".into()), force: true }
        );
        assert_eq!(parse(&PromptAction::Start { instance: "web-2".into() }, true), Verb::Start { target: Target::One("web-2".into()) });
        assert_eq!(parse(&PromptAction::Stop { instance: "web-2".into() }, false), Verb::Stop { target: Target::One("web-2".into()) });
        // An unanswered rm asks; the modal's answer goes back as
        // `deleteBranch`, same `force`, no `ask`.
        let rm = |force, delete_branch| PromptAction::Rm { instance: "web-2".into(), force, delete_branch };
        assert_eq!(
            parse(&rm(true, None), false),
            Verb::Rm { instance: "web-2".into(), delete_branch: None, force: true, ask: true }
        );
        assert_eq!(
            parse(&rm(true, Some(false)), false),
            Verb::Rm { instance: "web-2".into(), delete_branch: Some(false), force: true, ask: false }
        );
        assert_eq!(
            parse(&rm(false, Some(true)), false),
            Verb::Rm { instance: "web-2".into(), delete_branch: Some(true), force: false, ask: false }
        );
        assert_eq!(parse(&PromptAction::ServiceRebuild { name: "cache".into() }, false), Verb::ServicesRebuild { name: "cache".into() });
        assert_eq!(verb_call(&PromptAction::Exec { instance: "web-2".into(), argv: Vec::new() }, &root, env), None);
    }

    #[test]
    fn verb_status_reads_each_answer() {
        use crate::serve::client::Streamed;
        use crate::serve::proto::{ErrorBody, Response};
        use serde_json::{Value, json};
        let ok = |v: Value| Ok(Streamed::Answer(Response { id: Some(1), result: Some(v), error: None }));
        let err = |code: &str, message: &str, data: Option<Value>| {
            let error = ErrorBody { code: code.into(), message: message.into(), data };
            Ok(Streamed::Answer(Response { id: Some(1), result: None, error: Some(error) }))
        };
        let rebuild = PromptAction::Rebuild { instance: "web-2".into(), force: false };
        let status = |action: &PromptAction, sent| verb_status(action, sent).0;

        // Detached: a plain run learns its instance from the answer.
        assert_eq!(
            status(&run_action(None, None, None), ok(json!({"instance": "web-2", "detached": true}))),
            "run web: web-2 started (b opens its build log)"
        );
        assert_eq!(
            status(&run_action(Some("w2"), None, None), ok(json!({"instance": "w2", "detached": true}))),
            "run web --name w2: started (b opens its build log)"
        );
        assert_eq!(
            status(&rebuild, ok(json!({"instance": "web-2", "detached": true}))),
            "rebuild web-2: started (b opens its build log)"
        );
        // Ended before it was registered.
        let start = PromptAction::Start { instance: "web-2".into() };
        assert_eq!(verb_status(&start, ok(json!({"instance": "web-2", "exit": 0}))), ("start web-2: done".into(), false));

        // `failed`: the verb's error, pointing at the build log.
        assert_eq!(
            verb_status(&rebuild, err("failed", "boom", Some(json!({"exit": 1, "instance": null})))),
            ("rebuild web-2 failed: boom (b on web-2 opens its build log)".into(), true)
        );
        assert_eq!(
            status(&run_action(None, None, None), err("failed", "no sandbox `web`", Some(json!({"exit": 1, "instance": null})))),
            "run web failed: no sandbox `web` (b on its row opens the build log)"
        );
        assert_eq!(
            status(&run_action(None, None, None), err("failed", "boom", Some(json!({"exit": 1, "instance": "web-3"})))),
            "run web failed: boom (b on web-3 opens its build log)"
        );
        // Other errors ran nothing: no build log to point at.
        assert_eq!(status(&rebuild, err("not-found", "no instance `web-2`", None)), "rebuild web-2 failed: no instance `web-2`");

        // No daemon (F8, no fallback), or one that went away.
        assert_eq!(
            verb_status(&run_action(None, None, None), Err(anyhow::anyhow!("cannot start devsandbox serve"))),
            ("run web: needs devsandbox serve (cannot start devsandbox serve)".into(), true)
        );
        assert_eq!(status(&rebuild, Ok(Streamed::Lost)), "rebuild web-2: devsandbox serve went away; it keeps running");

        // Held verbs: what they did; a failure quotes the verb, no build log.
        let stop = PromptAction::Stop { instance: "web-2".into() };
        let rm = PromptAction::Rm { instance: "web-2".into(), force: false, delete_branch: None };
        let service = PromptAction::ServiceRebuild { name: "cache".into() };
        let done = json!({"instance": null, "exit": 0});
        assert_eq!(verb_status(&stop, ok(json!({"instance": "web-2", "exit": 0}))), ("stopped web-2".into(), false));
        assert_eq!(verb_status(&rm, ok(json!({"instance": "web-2", "exit": 0}))), ("removed web-2".into(), false));
        assert_eq!(verb_status(&service, ok(done.clone())), ("rebuilt service cache".into(), false));
        let refused = "`web-2` has uncommitted changes; use --force";
        assert_eq!(
            verb_status(&rm, err("failed", refused, Some(json!({"exit": 1, "instance": "web-2"})))),
            (format!("rm web-2 failed: {refused}"), true)
        );
        assert_eq!(
            status(&service, err("failed", "no service `cache`", Some(json!({"exit": 1, "instance": null})))),
            "service rebuild cache failed: no service `cache`"
        );
        assert_eq!(status(&stop, err("failed", "boom", None)), "stop web-2 failed: boom");
    }

    /// rm's `confirm` is its question for the modal, as rm words it; only an
    /// rm that asked gets one, the rest are status lines.
    #[test]
    fn verb_end_turns_rms_confirm_into_its_question() {
        use crate::serve::client::Streamed;
        use crate::serve::proto::{ErrorBody, Response};
        use serde_json::json;
        let answer = |code: &str, message: &str| {
            let error = ErrorBody { code: code.into(), message: message.into(), data: None };
            Ok(Streamed::Answer(Response { id: Some(1), result: None, error: Some(error) }))
        };
        let q = "delete branch `b`? it has commits not on its remote";
        let rm = |delete_branch| PromptAction::Rm { instance: "web-2".into(), force: false, delete_branch };
        assert_eq!(verb_end(&rm(None), answer("confirm", q)), VerbEnd::Confirm(q.into()));
        // Asked again after an answer: a failure, never a second modal.
        assert_eq!(
            verb_end(&rm(Some(true)), answer("confirm", q)),
            VerbEnd::Done { status: format!("rm web-2 failed: {q}"), failed: true }
        );
        let ok = Ok(Streamed::Answer(Response { id: Some(1), result: Some(json!({"instance": "web-2", "exit": 0})), error: None }));
        assert_eq!(verb_end(&rm(None), ok), VerbEnd::Done { status: "removed web-2".into(), failed: false });
    }

    #[test]
    fn followup_fires_once_after_delay() {
        let t0 = Instant::now();
        let mut f = FollowUp::default();
        assert!(!f.take_due(t0, false), "unarmed never fires");
        f.schedule(t0);
        assert!(!f.take_due(t0 + Duration::from_millis(499), false));
        assert!(f.take_due(t0 + FOLLOWUP_DELAY, false));
        assert!(!f.take_due(t0 + Duration::from_secs(5), false), "one-shot");
    }

    #[test]
    fn followup_waits_for_in_flight_collection() {
        let t0 = Instant::now();
        let mut f = FollowUp::default();
        f.schedule(t0);
        let late = t0 + Duration::from_secs(1);
        assert!(!f.take_due(late, true), "held while busy");
        assert!(f.take_due(late, false), "runs once the slot frees");
    }

    #[test]
    fn followup_reschedule_pushes_deadline() {
        let t0 = Instant::now();
        let mut f = FollowUp::default();
        f.schedule(t0);
        f.schedule(t0 + Duration::from_millis(300));
        assert!(!f.take_due(t0 + FOLLOWUP_DELAY, false));
        assert!(f.take_due(t0 + Duration::from_millis(800), false));
    }
}
