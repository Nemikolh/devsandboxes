//! Interactive dashboard (`devsandbox` with no subcommand on a TTY).
//!
//! This module owns all terminal I/O: raw mode, the alternate screen, the
//! panic hook that restores them, and the crossterm event loop. The state
//! machine ([`app::App`]) and rendering ([`ui::draw`]) stay I/O-free.

mod app;
mod data;
#[cfg(unix)]
mod forwards;
mod kitty;
mod procs;
mod prompt;
mod spec;
mod term;
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
use crate::inbox;

use app::{App, PendingSignal};
use data::Snapshot;
use procs::{parse_top, build_forest, ProcState};
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
    // Before the alternate screen, so `run`/`start` output stays readable.
    crate::commands::autostart::autostart(dir);
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
    app.utc_offset = local_utc_offset();
    app.kitty = KITTY.load(Ordering::Relaxed);
    match inbox::store::load() {
        Ok(inbox) => app.set_inbox(inbox),
        Err(e) => app.status = Some(format!("inbox not loaded: {e:#}")),
    }
    // The store is shared with every other dashboard and the bridge worker:
    // reload whenever its mtime/size moved (a cheap stat each tick), or at
    // once when something here poked it.
    let mut inbox_stamp = inbox::store::stamp();
    let mut reload_inbox = false;
    // At most one collection thread in flight; `Some` while one is running.
    // Startup walks the `Depth`s cheapest first, so the tree renders straight
    // from disk and statuses, then cpu/mem, fill in as the runtime answers;
    // `deeper` holds the collections still to chain (popped from the back).
    let mut pending: Option<Receiver<Collected>> = Some(spawn_collect_with(&dir, data::Depth::Disk));
    let mut deeper = vec![data::Depth::Full, data::Depth::Listing];
    let mut stats_clock = StatsClock::default();
    // Background `s` stops/starts, each reporting completion over its own
    // channel; paired with the instance name so the guard clears even if the
    // thread dies.
    let mut stops: Vec<(String, Receiver<OpDone>)> = Vec::new();
    let mut starts: Vec<(String, Receiver<OpDone>)> = Vec::new();
    // Background process signals (SIGTERM/SIGKILL from a process row); no guard
    // needed since each keypress targets a concrete pid.
    let mut signals: Vec<Receiver<OpDone>> = Vec::new();
    // Background done-flag writes (`d`/`u`, a thread's child); the `bool` is
    // `PendingDone::report`.
    let mut dones: Vec<(bool, Receiver<OpDone>)> = Vec::new();
    let mut last_tick = Instant::now();
    // At most one process fetch in flight; `Some` while one is running.
    let mut proc_pending: Option<Receiver<BTreeMap<String, ProcState>>> = None;
    let mut last_proc_tick = Instant::now();
    let mut followup = FollowUp::default();
    // Set while the follow-up's collection runs: proc targets come from the
    // snapshot, so the proc refresh must wait for it to land.
    let mut procs_after_snapshot = false;
    // ssh-agent relays, one per running instance while the dashboard is open
    // (docs/sandbox-helper.md). Owned by a worker thread so `State::load` and
    // the per-instance `exec` spawns never block the UI; the snapshot arm just
    // sends it the running set. Its `Drop` (any exit path, including `?` early
    // returns) closes the channel and joins the thread, killing every bridge
    // before the terminal is restored.
    // With a notify sink, bridges also drain each instance's `devsbd notify`
    // outbox (docs/automations.md); the worker shows desktop notifications,
    // this loop just drains the channel.
    #[cfg(unix)]
    let (notify_tx, notifications) = std::sync::mpsc::channel();
    #[cfg(unix)]
    let bridges = crate::devsbd::bridge::Bridges::spawn_worker(Some(notify_tx));
    // Ports-tab forwards (docs/port-forwarding.md, step 10). Same ownership as
    // `bridges`: a worker thread owns every `Forward`, doing all config/state/
    // docker work (route resolution, `ensure`, the `lsof` probe) off the UI
    // thread; its `Drop` (any exit path, including `?`) closes the channel and
    // joins, dropping every forward — killing its bridge/`exec` — before the
    // terminal is restored. Declared after `bridges` so it drops first (LIFO),
    // though the two are independent.
    #[cfg(unix)]
    let forwards = forwards::ForwardWorker::spawn(dir.clone());

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

        // A prompt command is ready: suspend the TUI (or, for `code`, just
        // launch it), then force a redraw + immediate refresh on return.
        if let Some(action) = app.take_pending_action() {
            match action {
                PromptAction::Code { instance } => {
                    let outcome = launch_code(&dir, &instance);
                    app.status = Some(match app.code_note.take() {
                        Some(note) => format!("{outcome} · {note}"),
                        None => outcome,
                    });
                }
                // Rename is pure state I/O (`rename_exact`: no stdout, no
                // prompts — plain `rename` would scribble on the alternate
                // screen): run it in place (no suspend) and force an immediate
                // resnapshot so the renamed row shows now.
                PromptAction::Rename { instance, new_name } => {
                    app.status = Some(match commands::rename::rename_exact(&instance, &new_name) {
                        Ok(new_name) => format!("renamed {instance} -> {new_name}"),
                        Err(e) => format!("rename failed: {e:#}"),
                    });
                    last_tick = Instant::now();
                    pending = Some(spawn_collect(&dir, &mut stats_clock));
                }
                other => {
                    run_suspended(terminal, &dir, other)?;
                    // Redraw from scratch: the child scribbled over the screen.
                    terminal.clear()?;
                    // Refresh data now rather than waiting for the next tick.
                    last_tick = Instant::now();
                    pending = Some(spawn_collect(&dir, &mut stats_clock));
                }
            }
        }

        // A background stop/start was requested by `s`: spawn it without
        // suspending.
        if let Some(instance) = app.take_pending_stop() {
            stops.push((instance.clone(), spawn_stop(&instance)));
        }
        if let Some(instance) = app.take_pending_start() {
            starts.push((instance.clone(), spawn_start(&instance)));
        }
        // A process-row SIGTERM/SIGKILL was requested: send it off-thread.
        if let Some(sig) = app.take_pending_signal() {
            signals.push(spawn_signal(sig));
        }
        for req in app.take_pending_done() {
            dones.push((req.report, spawn_done(req.instance, req.done)));
        }
        // An Inbox link: hand it to the desktop opener.
        if let Some(link) = app.take_pending_open() {
            if let Err(status) = open_link(&link) {
                app.status = Some(status);
            }
        }

        // Ports tab: hand add/remove requests to the forwarder worker and drain
        // its updates into the app. All docker/config work happens on the worker,
        // never here. Off unix the mux (and thus forwarding) doesn't exist.
        #[cfg(unix)]
        {
            if let Some(req) = app.take_pending_port() {
                forwards.add(req);
            }
            if let Some(id) = app.take_pending_unport() {
                forwards.remove(id);
            }
            while let Some(update) = forwards.try_recv() {
                match update {
                    forwards::ForwardUpdate::Rows(rows) => app.set_ports(rows),
                    forwards::ForwardUpdate::Status(status) => app.status = Some(status),
                }
            }
            // Container messages: the bridge worker already wrote them to the
            // store and rendered the status line, so this is a "reload now"
            // poke plus the latest line, so it's noticed from any tab. A put
            // that changed nothing never arrives here.
            while let Ok(line) = notifications.try_recv() {
                app.status = Some(line);
                reload_inbox = true;
            }
        }
        #[cfg(not(unix))]
        {
            if app.take_pending_port().is_some() {
                app.status = Some("port forwarding is unix-only for now".into());
            }
            // No forwards exist off unix, so a stale unport is just dropped.
            let _ = app.take_pending_unport();
        }
        // The Inbox lives in one shared file (`crate::inbox::store`), so any
        // dashboard's change must reach this one: push what the keys asked
        // for, then reload when the store moved (ours or someone else's).
        let ops = app.take_pending_inbox();
        if !ops.is_empty() {
            // Stamped here, under the store lock, not in `App`: event ids
            // and times are minted where every dashboard's writes serialize.
            let now = crate::state::Instance::now();
            if let Err(e) = inbox::store::update(|i| ops.iter().for_each(|op| i.apply(op, now))) {
                app.status = Some(format!("inbox not saved: {e:#}"));
            }
            reload_inbox = true;
        }
        let stamp = inbox::store::stamp();
        if reload_inbox || stamp != inbox_stamp {
            // Stamp before loading: a write in between only costs one more
            // reload, while stamping after could hide it until the next one.
            inbox_stamp = stamp;
            reload_inbox = false;
            match inbox::store::load() {
                Ok(loaded) => app.set_inbox(loaded),
                Err(e) => app.status = Some(format!("inbox not loaded: {e:#}")),
            }
        }

        // Drain any finished background stops/starts: update the status line,
        // clear the in-flight guard, and force an immediate snapshot refresh so
        // the row's new status shows without waiting for the next tick.
        stops.retain_mut(|(instance, rx)| match rx.try_recv() {
            Ok(done) => {
                app.stopping.remove(instance);
                app.status = Some(done.status);
                last_tick = Instant::now();
                followup.schedule(last_tick);
                if pending.is_none() {
                    pending = Some(spawn_collect(&dir, &mut stats_clock));
                }
                false
            }
            Err(TryRecvError::Empty) => true,
            // Thread died without sending; clear the guard so `s` works again.
            Err(TryRecvError::Disconnected) => {
                app.stopping.remove(instance);
                false
            }
        });
        starts.retain_mut(|(instance, rx)| match rx.try_recv() {
            Ok(done) => {
                app.starting.remove(instance);
                app.status = Some(done.status);
                last_tick = Instant::now();
                followup.schedule(last_tick);
                if pending.is_none() {
                    pending = Some(spawn_collect(&dir, &mut stats_clock));
                }
                false
            }
            Err(TryRecvError::Empty) => true,
            // Thread died without sending; clear the guard so `s` works again.
            Err(TryRecvError::Disconnected) => {
                app.starting.remove(instance);
                false
            }
        });
        // Drain finished signals: show the outcome and force a proc refresh so a
        // killed process drops off the forest without waiting for the next tick.
        signals.retain_mut(|rx| match rx.try_recv() {
            Ok(done) => {
                app.status = Some(done.status);
                app.needs_proc_fetch = true;
                false
            }
            Err(TryRecvError::Empty) => true,
            Err(TryRecvError::Disconnected) => false,
        });

        // Drain finished done-flag writes: report (a thread's child only on
        // failure) and resnapshot so the row dims or brightens now.
        dones.retain_mut(|(report, rx)| match rx.try_recv() {
            Ok(done) => {
                if *report || done.failed {
                    app.status = Some(done.status);
                }
                last_tick = Instant::now();
                followup.schedule(last_tick);
                if pending.is_none() {
                    pending = Some(spawn_collect(&dir, &mut stats_clock));
                }
                false
            }
            Err(TryRecvError::Empty) => true,
            Err(TryRecvError::Disconnected) => false,
        });

        // Drain a finished collection into the app, freeing the in-flight slot.
        if let Some(rx) = &pending {
            match rx.try_recv() {
                Ok((snapshot, children)) => {
                    app.thread_children = children;
                    #[cfg(unix)]
                    {
                        // Owned list handed to the worker; never blocks the UI.
                        let running: Vec<String> = snapshot
                            .instances
                            .iter()
                            .filter(|r| matches!(r.status, data::ContainerStatus::Running(_)))
                            .map(|r| r.container.clone())
                            .collect();
                        bridges.send(running);
                        // Configured `forwardPorts` follow the running set.
                        forwards.sync(
                            snapshot
                                .instances
                                .iter()
                                .filter(|r| matches!(r.status, data::ContainerStatus::Running(_)))
                                .map(|r| r.name.clone())
                                .collect(),
                        );
                    }
                    app.set_snapshot(snapshot);
                    pending = deeper.pop().map(|depth| {
                        if depth == data::Depth::Full {
                            stats_clock.last = Some(Instant::now());
                        }
                        spawn_collect_with(&dir, depth)
                    });
                    if procs_after_snapshot {
                        procs_after_snapshot = false;
                        app.needs_proc_fetch = true;
                    }
                }
                Err(TryRecvError::Empty) => {}
                // Thread died without sending; drop the slot so the next tick retries.
                Err(TryRecvError::Disconnected) => pending = None,
            }
        }

        if followup.take_due(Instant::now(), pending.is_some()) {
            last_tick = Instant::now();
            pending = Some(spawn_collect(&dir, &mut stats_clock));
            procs_after_snapshot = true;
        }

        if last_tick.elapsed() >= TICK_INTERVAL {
            last_tick = Instant::now();
            // Skip if a collection is still running so docker calls can't pile up.
            if pending.is_none() {
                pending = Some(spawn_collect(&dir, &mut stats_clock));
            }
        }

        // Drain a finished process fetch into the app, freeing the in-flight slot.
        if let Some(rx) = &proc_pending {
            match rx.try_recv() {
                Ok(procs) => {
                    app.apply_proc_fetch(procs);
                    proc_pending = None;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => proc_pending = None,
            }
        }

        // Process refresh: fetch now on expand / cursor move, else every
        // PROC_TICK, whenever no fetch is already running. Targets are the
        // expanded instances plus the selected running one (for the Detail agent
        // count); an empty set means there is nothing to fetch.
        let want_now = app.take_needs_proc_fetch();
        let tick_due = last_proc_tick.elapsed() >= PROC_TICK;
        if (want_now || tick_due) && proc_pending.is_none() {
            last_proc_tick = Instant::now();
            // Non-running expanded instances get their `(not running)` row here;
            // running ones (and the selection) come back as fetch targets.
            let targets = app.proc_fetch_targets();
            if !targets.is_empty() {
                proc_pending = Some(spawn_proc_fetch(targets));
            }
        }
    }
    Ok(())
}

/// Leave the TUI, run a prompt command with inherited stdio, wait for one
/// keypress, then re-enter the TUI. Command errors are printed (not propagated)
/// so a failed `run`/`exec`/`rm` returns the user to the dashboard.
fn run_suspended(terminal: &mut Term, dir: &Path, action: PromptAction) -> Result<()> {
    restore();

    let result = match &action {
        PromptAction::Run { sandbox, name, branch, base } => {
            commands::run::run(
                dir,
                Some(sandbox.clone()),
                name.clone(),
                branch.clone(),
                base.clone(),
                Default::default(),
            )
            .map(|key| key.into_iter().for_each(|key| println!("{key}")))
        }
        PromptAction::Exec { instance, argv } => {
            commands::exec::exec_status(instance, true, true, argv).map(|_| ())
        }
        PromptAction::Rm { instance, force } => commands::rm::rm(instance, None, *force),
        PromptAction::Stop { instance } => commands::stop::stop(Some(instance.clone()), false),
        PromptAction::Start { instance } => {
            commands::start::start(dir, Some(instance.clone()), false)
        }
        PromptAction::Rebuild { instance, force } => {
            commands::rebuild::rebuild(dir, Some(instance.clone()), false, *force)
        }
        PromptAction::ServiceRebuild { name } => commands::services::rebuild(dir, name),
        // `code`/`rename` never suspend (handled by the caller); `port` never
        // reaches here — `App` routes it to `pending_port` (step 10), not
        // `pending_action`.
        PromptAction::Code { .. } | PromptAction::Rename { .. } | PromptAction::Port { .. } => {
            Ok(())
        }
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
        PromptAction::Run { .. } => "run",
        PromptAction::Exec { .. } => "exec",
        PromptAction::Rm { .. } => "rm",
        PromptAction::Stop { .. } => "stop",
        PromptAction::Start { .. } => "start",
        PromptAction::Rebuild { .. } => "rebuild",
        PromptAction::ServiceRebuild { .. } => "service rebuild",
        PromptAction::Code { .. } => "code",
        PromptAction::Rename { .. } => "rename",
        PromptAction::Port { .. } => "port",
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
/// the state instance key). Returns a one-line status for the help-bar.
fn launch_code(dir: &Path, instance: &str) -> String {
    let state = match crate::state::State::load() {
        Ok(s) => s,
        Err(e) => return format!("code: {e:#}"),
    };
    let Some(info) = state.instances.get(instance) else {
        return format!("code: unknown instance `{instance}`");
    };
    commands::vscode::launch(dir, instance, info).unwrap_or_else(|e| format!("{e:#}"))
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

/// Seconds east of UTC for the Inbox clock, from `date +%z` (std has no
/// timezone support and no tz crate is in the tree). 0 (UTC) when `date` is
/// missing or odd; read once, so a DST switch mid-session isn't picked up.
fn local_utc_offset() -> i64 {
    std::process::Command::new("date")
        .arg("+%z")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .and_then(|o| app::parse_utc_offset(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or(0)
}

/// Result of a background `s` stop/start, drained by the event loop.
struct OpDone {
    /// One-line outcome for the help-bar status.
    status: String,
    /// The op failed (only `spawn_done` reads it: a thread's child write
    /// reports failures alone).
    failed: bool,
}

/// Spawn a detached thread that stops `instance` (docker stop can block ~10s on
/// the SIGTERM timeout) and reports a one-line status back. Docker calls go
/// through the screen-safe quiet path so stderr can't corrupt the TUI. `instance`
/// is a snapshot row name, which equals the state instance key.
fn spawn_stop(instance: &str) -> Receiver<OpDone> {
    let (tx, rx) = mpsc::channel();
    let instance = instance.to_string();
    std::thread::spawn(move || {
        let status = match crate::state::State::load() {
            Ok(state) => match state.instances.get(&instance) {
                Some(info) => {
                    let services =
                        commands::stop::service_containers(&info.project, &info.instance_id);
                    commands::stop::stop_containers(&info.container, &services, true);
                    format!("stopped {instance}")
                }
                None => format!("stop: unknown instance `{instance}`"),
            },
            Err(e) => format!("stop: {e:#}"),
        };
        let _ = tx.send(OpDone { status, failed: false });
    });
    rx
}

/// Spawn a detached thread that starts `instance`'s containers (isolated
/// services first, then the instance) and reports a one-line status back. The
/// bare-start path is used — no service recreation or lifecycle commands — so
/// nothing can write to the alternate screen; docker calls go through the
/// screen-safe quiet path. `instance` is a snapshot row name, which equals the
/// state instance key.
fn spawn_start(instance: &str) -> Receiver<OpDone> {
    let (tx, rx) = mpsc::channel();
    let instance = instance.to_string();
    std::thread::spawn(move || {
        let status = match crate::state::State::load() {
            Ok(state) => match state.instances.get(&instance) {
                Some(info) => {
                    // Bare start bypasses `start_instance`, so re-point the agent
                    // symlink here too, so a restart after host agent rotation
                    // re-captures the live socket (docs/ssh-agent.md).
                    commands::run::ssh_agent_refresh(info);
                    let services =
                        commands::stop::service_containers(&info.project, &info.instance_id);
                    commands::start::start_containers(&info.container, &services, true);
                    // Reinstall the helper (no-op when current): covers CLI
                    // upgrades and images that mount a tmpfs at `/run`.
                    crate::devsbd::ensure_recorded(&instance, info, true);
                    format!("started {instance}")
                }
                None => format!("start: unknown instance `{instance}`"),
            },
            Err(e) => format!("start: {e:#}"),
        };
        let _ = tx.send(OpDone { status, failed: false });
    });
    rx
}

/// Spawn a detached thread that sets or clears `instance`'s done flag in
/// state (`commands::done::set_saved`: no stdout/stderr, the TUI owns the
/// terminal) and reports a one-line status back. `instance` is a state key.
fn spawn_done(instance: String, done: bool) -> Receiver<OpDone> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let verb = if done { "done" } else { "undone" };
        let (status, failed) = match commands::done::set_saved(&instance, done) {
            Ok(status) => (status, false),
            Err(e) => (format!("{verb}: {e:#}"), true),
        };
        let _ = tx.send(OpDone { status, failed });
    });
    rx
}

/// Spawn a detached thread that sends `sig.signal` to a pid inside a container
/// (`kill -<n>` via the runtime's screen-safe exec) and reports a one-line
/// status back.
fn spawn_signal(sig: PendingSignal) -> Receiver<OpDone> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let status = match crate::runtime::backend().signal_proc(
            &sig.container,
            &sig.pid,
            sig.signal.num(),
        ) {
            Ok(()) => format!("sent {} to pid {}", sig.signal.name(), sig.pid),
            Err(e) => format!("{}: {e:#}", sig.signal.name()),
        };
        let _ = tx.send(OpDone { status, failed: false });
    });
    rx
}

/// Stdout kept from a child's `devsbd run ls` before it's given up on.
const RUN_LS_CAP: usize = 64 * 1024;

/// How long a child's `devsbd run ls` may take before it's killed.
const RUN_LS_TIMEOUT: Duration = Duration::from_secs(5);

/// `devsbd run ls` in `container`, bounded in size and time (its output is
/// container-controlled). `None` on any failure — no helper, one predating
/// runs, a timeout, or output past [`RUN_LS_CAP`] (dropped whole rather than
/// parsed cut off): the Procs view then just shows no runs.
fn run_ls(container: &str) -> Option<String> {
    let backend = crate::runtime::backend();
    let mut cmd = std::process::Command::new(backend.bin());
    cmd.args(["exec", container, crate::devsbd::BIN, "run", "ls"]).stdin(std::process::Stdio::null());
    let out = crate::runtime::bounded::run(&mut cmd, RUN_LS_CAP, RUN_LS_TIMEOUT).ok()?;
    (out.status.success() && !out.truncated).then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Spawn one detached thread that runs the runtime's `proc_list` for each target
/// `(instance name, container)` and sends back a name→[`ProcState`] map. Docker
/// calls go through the screen-safe quiet path; a per-container error becomes a
/// `(processes unavailable: …)` message row rather than failing the batch.
fn spawn_proc_fetch(targets: Vec<(String, String)>) -> Receiver<BTreeMap<String, ProcState>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut out: BTreeMap<String, ProcState> = BTreeMap::new();
        for (instance, container) in targets {
            let state = match crate::runtime::backend().proc_list(&container) {
                Ok(list) => {
                    let mut rows = build_forest(parse_top(&list.text));
                    // A child's runs next to its processes (docs/automations.md,
                    // "Runs"). No helper, or one predating runs: no rows.
                    if let Some(text) = run_ls(&container) {
                        rows.extend(procs::run_rows(&text));
                    }
                    ProcState::Rows { rows, signalable: list.container_pids }
                }
                Err(e) => ProcState::Message(format!("(processes unavailable: {e:#})")),
            };
            out.insert(instance, state);
        }
        // Receiver may be gone if the UI quit mid-fetch; ignore send errors.
        let _ = tx.send(out);
    });
    rx
}

/// One collection: the snapshot, plus every dispatcher's children by key
/// (`dispatch::thread_children`) for the Inbox thread pane. Read from
/// `state.toml` on the collector thread, with the snapshot, so the pane never
/// touches the file per frame; an unreadable state resolves no child.
type Collected = (Snapshot, BTreeMap<String, BTreeMap<String, String>>);

/// Spawn a detached thread that collects one [`Snapshot`] and sends it back.
/// The receiver is polled from the event loop, keeping [`App`] I/O-free.
/// A refresh collection, with stats only when `clock` says they're due.
fn spawn_collect(dir: &Path, clock: &mut StatsClock) -> Receiver<Collected> {
    spawn_collect_with(dir, clock.depth(Instant::now()))
}

/// A collection down to `depth` (`data::collect_with`).
fn spawn_collect_with(dir: &Path, depth: data::Depth) -> Receiver<Collected> {
    let (tx, rx) = mpsc::channel();
    let dir: PathBuf = dir.to_path_buf();
    std::thread::spawn(move || {
        let snapshot = data::collect_with(&dir, depth);
        let children = crate::state::State::load()
            .map(|s| crate::commands::dispatch::thread_children(&s))
            .unwrap_or_default();
        // Receiver may be gone if the UI quit mid-collection; ignore send errors.
        let _ = tx.send((snapshot, children));
    });
    rx
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
