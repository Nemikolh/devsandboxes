//! Interactive dashboard (`devsandbox` with no subcommand on a TTY).
//!
//! This module owns all terminal I/O: raw mode, the alternate screen, the
//! panic hook that restores them, and the crossterm event loop. The state
//! machine ([`app::App`]) and rendering ([`ui::draw`]) stay I/O-free.
//!
//! Unix-only (`main.rs`): the dashboard is a client of devsandbox serve,
//! which is; off unix it only says so.

mod ansi;
mod app;
mod clipboard;
mod daemon;
mod data;
mod kitty;
mod markdown;
mod poll;
mod procs;
mod prompt;
mod remote;
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

use app::{App, ExecCli, VerbEnd};
use poll::{PollUpdate, PollWorker};
use prompt::PromptAction;

/// How long each `event::poll` blocks before we redraw when no terminal is open.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Shorter poll used while any integrated terminal is open. Key events already
/// wake `event::poll`, but child *output* arrives on the reader threads and only
/// becomes visible on the next draw — a 250ms cadence makes shell echo feel
/// laggy, so we redraw ~33×/s to keep echo snappy while terminals exist.
const TERM_POLL_INTERVAL: Duration = Duration::from_millis(30);
/// Process-refresh cadence for expanded instances (the snapshot is pushed).
const PROC_TICK: Duration = Duration::from_secs(5);

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
    // No autostart pass of our own: the daemon runs it (docs/architecture/daemon/host-poll.md, *Autostart*).
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
    // The executable resolves per spawn, not here: an upgrade under a running
    // dashboard must not leave it pointing at the old, deleted file.
    app.exec_cli = Ok(ExecCli { exe: None, root: daemon::root_dir(dir.clone()) });
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
    // The store is shared with every other dashboard and the daemon's sink:
    // reload whenever its mtime/size moved (a cheap stat each tick), or at
    // once when something here poked it.
    let mut inbox_stamp = inbox::store::path().ok().and_then(|p| inbox::ops::stamp(&p));
    let mut reload_inbox = false;
    // The snapshot is the daemon's: its one poll pushes it to the watch the
    // daemon worker holds (`DaemonUpdate::Snapshot`, every ~2 s, its Disk →
    // Listing → Full walk first, so the tree renders straight from disk and
    // statuses, then cpu/mem, fill in). Until the first push the tree says
    // `loading…`. The processes are asked for per request (`poll.rs`), at
    // most one outstanding (`proc_pending`).
    let poller = PollWorker::spawn();
    // Background VS Code launches (`o`, `:code`, a thread's `vscode`): a
    // `--goto` waits up to 30 s for the window. No guard: a second launch
    // just focuses the same window.
    let mut codes: Vec<Receiver<OpDone>> = Vec::new();
    // Verb requests to the daemon, a thread each until answered
    // (`spawn_verb`): `run`/`rebuild`/`start` detached, `stop`/`rm`/
    // `services.rebuild` held, all sent with `detach` (a quit cancels
    // none). Paired with their action so the answer
    // reaches `App` (guards, rm's question) even if the thread dies.
    let mut verbs: Vec<(PromptAction, Receiver<VerbEnd>)> = Vec::new();
    let mut proc_pending = false;
    // Set while a procs failure the daemon answered is shown, so it's one
    // status line per streak. A daemon that can't be reached isn't one:
    // `daemon.rs` already says so.
    let mut procs_failing = false;
    let mut last_proc_tick = Instant::now();
    // Set when something changed what runs (a verb, a rename, a done flag,
    // `instances.changed`): proc targets come from the snapshot, so the proc
    // refresh waits for the next push to land.
    let mut procs_after_snapshot = false;
    // The host daemon (docs/architecture/daemon/role.md) owns the bridges: ssh-agent relays,
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
            // A remote tab whose ssh dropped re-dials when its wait is over.
            app.redial_terminals(std::time::Instant::now());
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
            copy_on_host(sent.as_bytes());
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
            if let Some(text) = clipboard::decode_base64(&b64) {
                copy_on_host(&text);
            }
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
        // `exec` (then force a redraw on return; the processes refetch
        // after the next push).
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
                    // An identity answer that released it leads its line.
                    app.status = Some(match app.take_identity_note() {
                        Some(note) => format!("{note} · {}", verb_pending(&action)),
                        None => verb_pending(&action),
                    });
                    let route = app.snapshot.as_ref().map(|s| verb_route(s, &action)).unwrap_or_default();
                    verbs.push((action.clone(), spawn_verb(&dir, action, verb_tty(size.width, size.height), route)));
                    procs_after_snapshot = true;
                }
                PromptAction::Code { instance, .. } if remote::split(&instance).0.is_some() => {
                    let (machine, name) = remote::split(&instance);
                    app.status = Some(app::not_yet_code(machine.unwrap_or_default(), name));
                }
                PromptAction::Code { instance, goto } => {
                    app.status = Some("opening VS Code…".into());
                    codes.push(spawn_code(dir.clone(), instance, goto, app.code_note.take()));
                }
                // `instances.rename`, no suspend; the answer is a status line
                // (`DaemonUpdate::Renamed`), the renamed row comes with a push.
                PromptAction::Rename { instance, new_name } => daemon.rename(instance, new_name),
                other => {
                    let argv = match &other {
                        PromptAction::Exec { instance, argv } => app.exec_argv(instance, argv),
                        _ => Err("not a command to suspend for".into()),
                    };
                    run_suspended(terminal, argv)?;
                    // Redraw from scratch: the child scribbled over the screen.
                    terminal.clear()?;
                    procs_after_snapshot = true;
                }
            }
        }

        // Process-row SIGTERM/SIGKILL, done flags, modal reads: one-shot
        // requests on the daemon worker's connection, answered below.
        if let Some(sig) = app.take_pending_signal() {
            daemon.signal(sig);
        }
        // The identity offer's read, or an identity modal's answer (the
        // verb it holds comes back with the answer).
        if let Some(req) = app.take_pending_identity() {
            daemon.identity(req);
        }
        for req in app.take_pending_done() {
            daemon.set_done(req);
        }
        for fetch in app.take_pending_fetch() {
            let route = app.snapshot.as_ref().map(|s| fetch_route(s, &fetch)).unwrap_or_default();
            daemon.fetch(fetch, route);
        }
        // `d` on an in-flight row, confirmed: the daemon SIGINTs its build
        // (`instances.cancel`, whoever started it).
        if let Some(instance) = app.take_pending_cancel() {
            daemon.cancel(instance);
            procs_after_snapshot = true;
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
        // Machines tab: `a`'s `machines.set`; the row follows the next push.
        if let Some((name, agent)) = app.take_pending_machine_set() {
            daemon.machine_set(name, agent);
        }
        while let Some(update) = daemon.try_recv() {
            match update {
                daemon::DaemonUpdate::InboxChanged => reload_inbox = true,
                daemon::DaemonUpdate::Ports(rows) => app.set_ports(rows),
                daemon::DaemonUpdate::Snapshot(list, machines) => {
                    app.snapshot_error = None;
                    app.set_machines(machines);
                    app.thread_children = list.thread_children;
                    app.set_snapshot(list.snapshot);
                    if std::mem::take(&mut procs_after_snapshot) {
                        app.needs_proc_fetch = true;
                    }
                }
                // The last snapshot stays, marked stale with why (none yet:
                // the tree says it); the next push clears it.
                daemon::DaemonUpdate::Down(reason) => {
                    app.snapshot_error = Some(data::FetchError { unreachable: true, reason });
                }
                // Containers came or went: the next push shows it, and the
                // processes are refetched after it.
                daemon::DaemonUpdate::InstancesChanged => procs_after_snapshot = true,
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
                // The row renames, dims or brightens with the next push (a
                // `state.toml` write: within ~0.5 s).
                daemon::DaemonUpdate::Renamed(line) => {
                    app.status = Some(line);
                    procs_after_snapshot = true;
                }
                // A thread's child reports only on failure.
                daemon::DaemonUpdate::Done { status, failed, report } => {
                    if report || failed {
                        app.status = Some(status);
                    }
                    procs_after_snapshot = true;
                }
                daemon::DaemonUpdate::IdentityOffer { groups, asked } => app.identity_offer(groups, asked),
                // A released verb is taken on the next pass.
                daemon::DaemonUpdate::Identity { outcome, then } => app.identity_answered(outcome, then),
            }
        }
        // The Inbox lives in one shared file (`crate::inbox::store`), so any
        // dashboard's change must reach this one: push what the keys asked
        // for, then reload when the store moved (ours or someone else's).
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

        // Drain answered verb requests: hand the answer to `App`. The row's
        // phase, stop or removal comes with the pushes; the processes are
        // refetched after the next one.
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
            procs_after_snapshot = true;
            false
        });

        // Drain the daemon's procs answers into the app, freeing the slot.
        while let Some(update) = poller.try_recv() {
            match update {
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

/// Leave the TUI, run a prompt command (`argv`, `:exec`'s, or why there's
/// none) with inherited stdio, wait for one keypress, then re-enter the
/// TUI. `exec` is `devsandbox exec` as a child (`App::exec_argv`: over ssh
/// for a machine's instance), so the dashboard reads no state for it: the
/// CLI prints its own errors above the prompt, and only a child that can't
/// start is ours to print. Never propagated, so a failed `exec` returns to
/// the dashboard. The verbs go to the daemon (`spawn_verb`), `code`/
/// `rename` never suspend, and `port` goes to `pending_port`: only `exec`
/// gets here.
fn run_suspended(terminal: &mut Term, argv: std::result::Result<Vec<String>, String>) -> Result<()> {
    restore();

    let result = argv.map_err(|e| anyhow::anyhow!("{e}")).and_then(|argv| {
        std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .status()
            .map(|_| ())
            .with_context(|| format!("failed to run {}", argv[0]))
    });
    if let Err(e) = result {
        eprintln!("error: {e:#}");
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
/// it, and they write no build log. Sent with `detach` all the same, which
/// for them only means a quit leaves them running.
fn verb_held(action: &PromptAction) -> bool {
    matches!(action, PromptAction::Stop { .. } | PromptAction::Rm { .. } | PromptAction::ServiceRebuild { .. })
}

/// Most rows a dashboard verb's pty gets: the build log modal shows a
/// window of the log, not the whole screen's worth a tall terminal would let
/// buildkit redraw.
const VERB_TTY_ROWS: u16 = 40;

/// The pty size of the dashboard's build verbs: the build log modal's inside
/// (the terminal less its border, so a full-width line isn't clipped), its
/// height up to [`VERB_TTY_ROWS`].
fn verb_tty(width: u16, height: u16) -> crate::serve::verbs::TtySize {
    let inner = ratatui::widgets::Block::bordered().inner(Rect::new(0, 0, width, height));
    crate::serve::verbs::TtySize::sane(inner.width, inner.height.min(VERB_TTY_ROWS))
}

/// The verb request (method, params) for a dashboard verb action in config
/// root `root` (absolute) with the dashboard's `env`; `None` for the other
/// actions. `run`/`rebuild`/`start` always `detach`: the dashboard only
/// waits for the instance to be registered, its row shows the rest. They
/// run on a pty of size `tty` (colour and docker build's progress in their
/// build log).
/// `stop`/`rm`/`services.rebuild` are held (answered when done) and
/// `detach` too, which for them only means a hang-up (a dashboard quit)
/// doesn't cancel them. An unanswered `rm` asks (its branch question comes back as `confirm`); an
/// answered one carries `deleteBranch`. The target always goes in, so
/// nothing can need a pick.
fn verb_call(
    action: &PromptAction,
    root: &Path,
    env: BTreeMap<String, String>,
    tty: crate::serve::verbs::TtySize,
) -> Option<(&'static str, serde_json::Value)> {
    use crate::serve::verbs::{RebuildParams, RmParams, RunParams, ServicesRebuildParams, StartParams, StopParams};
    let root = root.to_path_buf();
    let tty = Some(tty);
    let (method, params) = match action.clone() {
        PromptAction::Run { sandbox, name, branch, base } => (
            "run",
            serde_json::to_value(RunParams {
                root,
                env,
                sandbox,
                name,
                branch,
                base,
                json: false,
                reprobe: false,
                detach: true,
                tty,
                here: false,
            }),
        ),
        PromptAction::Rebuild { instance, force } => (
            "rebuild",
            serde_json::to_value(RebuildParams {
                root,
                env,
                instance: Some(instance),
                all: false,
                force,
                reprobe: false,
                detach: true,
                tty,
            }),
        ),
        PromptAction::Start { instance } => (
            "start",
            serde_json::to_value(StartParams { root, env, instance: Some(instance), all: false, detach: true, tty }),
        ),
        PromptAction::Stop { instance } => {
            ("stop", serde_json::to_value(StopParams { root, env, instance: Some(instance), all: false, detach: true }))
        }
        PromptAction::Rm { instance, force, delete_branch } => (
            "rm",
            serde_json::to_value(RmParams {
                root,
                env,
                instance,
                delete_branch,
                force,
                ask: delete_branch.is_none(),
                detach: true,
            }),
        ),
        PromptAction::ServiceRebuild { name } => {
            ("services.rebuild", serde_json::to_value(ServicesRebuildParams { root, env, name, detach: true }))
        }
        _ => return None,
    };
    Some((method, params.expect("verb params serialize")))
}

/// Where verb `action` goes: its target row's machine and root there
/// (`remote::Route`; the default for this host's rows).
fn verb_route(s: &data::Snapshot, action: &PromptAction) -> remote::Route {
    match action {
        PromptAction::Run { sandbox, .. } => remote::sandbox_route(s, sandbox),
        PromptAction::Rebuild { instance, .. }
        | PromptAction::Start { instance }
        | PromptAction::Stop { instance }
        | PromptAction::Rm { instance, .. } => remote::instance_route(s, instance),
        PromptAction::ServiceRebuild { name } => remote::service_route(s, name),
        _ => remote::Route::default(),
    }
}

/// Where a modal's read goes: a remote config read needs the row's root
/// there (the rest only their machine, from the target's key).
fn fetch_route(s: &data::Snapshot, fetch: &app::Fetch) -> remote::Route {
    match fetch {
        app::Fetch::Config { kind: app::ConfigKind::Sandbox, name } => remote::sandbox_route(s, name),
        app::Fetch::Config { kind: app::ConfigKind::Service, name } => remote::service_route(s, name),
        app::Fetch::Logs { .. } | app::Fetch::Inspect { .. } => remote::Route::default(),
    }
}

/// `action` as its machine takes it: the target's key (`box/web-2`) back
/// to the name there.
fn unqualified(action: &PromptAction) -> PromptAction {
    let bare = |key: &str| remote::split(key).1.to_string();
    match action.clone() {
        PromptAction::Run { sandbox, name, branch, base } => PromptAction::Run { sandbox: bare(&sandbox), name, branch, base },
        PromptAction::Rebuild { instance, force } => PromptAction::Rebuild { instance: bare(&instance), force },
        PromptAction::Start { instance } => PromptAction::Start { instance: bare(&instance) },
        PromptAction::Stop { instance } => PromptAction::Stop { instance: bare(&instance) },
        PromptAction::Rm { instance, force, delete_branch } => {
            PromptAction::Rm { instance: bare(&instance), force, delete_branch }
        }
        PromptAction::ServiceRebuild { name } => PromptAction::ServiceRebuild { name: bare(&name) },
        other => other,
    }
}

/// The verb request for `action` on `route`: [`verb_call`] in this host's
/// `root`, or, for a machine's row, in its root there with `machine` set
/// (the local daemon forwards it, docs/architecture/daemon/machines.md *Forwarded requests*). `Err`: a machine's
/// row with no root known.
fn routed_verb_call(
    action: &PromptAction,
    root: &Path,
    route: &remote::Route,
    env: BTreeMap<String, String>,
    tty: crate::serve::verbs::TtySize,
) -> Result<(&'static str, serde_json::Value)> {
    let Some(machine) = &route.machine else {
        return verb_call(action, root, env, tty).context("not a verb");
    };
    let Some(remote_root) = &route.root else {
        anyhow::bail!("no config root known on machine `{machine}`");
    };
    let (method, mut params) = verb_call(&unqualified(action), Path::new(remote_root), env, tty).context("not a verb")?;
    params["machine"] = machine.as_str().into();
    Ok((method, params))
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
            let status = match (result["instance"].as_str(), result["machine"].as_str()) {
                // Placed on a fleet worker: named where it is.
                (Some(instance), Some(m)) => format!("{label}: {m}/{instance} started"),
                (Some(instance), None) if Some(instance) != named => format!("{label}: {instance} started"),
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
/// dashboard before the answer hangs up, which cancels nothing: every
/// dashboard verb is sent with `detach`, so the daemon leaves it running.
///
/// `route`: a machine's row goes there (`routed_verb_call`); its status
/// lines keep the qualified name (`stopped box/web-2`).
fn spawn_verb(
    dir: &Path,
    action: PromptAction,
    tty: crate::serve::verbs::TtySize,
    route: remote::Route,
) -> Receiver<VerbEnd> {
    let (tx, rx) = mpsc::channel();
    let root = daemon::root_dir(dir.to_path_buf());
    std::thread::spawn(move || {
        use crate::serve::{client, endpoint, verbs_client};
        let env = verbs_client::caller_env(std::env::vars_os());
        let sent = routed_verb_call(&action, &root, &route, env, tty).and_then(|(method, params)| {
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

/// Also put `text` on the host clipboard ([`clipboard::host_helper`]) beside
/// the OSC 52 copy, which the outer terminal may ignore. Best effort, as
/// OSC 52 is: no helper, a failed spawn or write says nothing. Fed and
/// reaped on its own thread so a 1 MiB copy never stalls the loop.
fn copy_on_host(text: &[u8]) {
    use std::process::{Command, Stdio};
    let Some((cmd, args)) = clipboard::host_helper(std::env::consts::OS, |k| std::env::var(k).ok()) else {
        return;
    };
    let Ok(mut child) = Command::new(cmd).args(args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()
    else {
        return;
    };
    let text = text.to_vec();
    std::thread::spawn(move || {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(&text);
        }
        let _ = child.wait();
    });
}

/// Result of a background VS Code launch, drained by the event loop.
struct OpDone {
    /// One-line outcome for the help-bar status.
    status: String,
}


#[cfg(test)]
mod tests {
    use super::*;

    fn run_action(name: Option<&str>, branch: Option<&str>, base: Option<&str>) -> PromptAction {
        PromptAction::Run {
            sandbox: "web".into(),
            name: name.map(String::from),
            branch: branch.map(String::from),
            base: base.map(String::from),
        }
    }

    /// A machine's row: the verb goes to it (`machine`) in the row's root
    /// there, naming the target as the machine knows it; this host's rows
    /// as before; a machine with no root known can't be asked. Modal reads
    /// of a machine's sandbox carry its root too.
    #[test]
    fn verbs_on_a_machines_rows_carry_machine_and_root() {
        use crate::serve::snapshots::tests::{row, snapshot};
        let mut remote = row("web-2", true, None);
        remote.machine = Some("box".into());
        remote.root = Some("/r/two".into());
        let mut orphan = row("old", true, None);
        orphan.machine = Some("far".into());
        let mut s = snapshot(vec![row("web-2", true, None), remote, orphan], false);
        s.sandboxes = vec![serde_json::from_value(serde_json::json!({
            "name": "web", "source": "image x", "folder": null, "services": [], "extends": [],
            "config_hash": "", "build_hash": "", "issues": [], "dispatcher": false,
            "worktree_branch": null, "root": "/r/one", "machine": "box",
        }))
        .unwrap()];
        let root = Path::new("/here");
        let env = BTreeMap::new();
        let tty = verb_tty(100, 40);
        let call = |action: &PromptAction| {
            let route = verb_route(&s, action);
            routed_verb_call(action, root, &route, env.clone(), tty).map(|(m, p)| (m, p["root"].clone(), p["machine"].clone(), p))
        };
        let (method, root_param, machine, params) = call(&PromptAction::Stop { instance: "box/web-2".into() }).unwrap();
        assert_eq!((method, root_param, machine), ("stop", "/r/two".into(), "box".into()));
        assert_eq!(params["instance"], "web-2");
        let (_, root_param, machine, params) = call(&PromptAction::Stop { instance: "web-2".into() }).unwrap();
        assert_eq!((root_param, machine, &params["instance"]), ("/here".into(), serde_json::Value::Null, &"web-2".into()));
        let (method, root_param, machine, params) = call(&run_action(None, None, None)).unwrap();
        assert_eq!((method, root_param, machine, &params["sandbox"]), ("run", "/here".into(), serde_json::Value::Null, &"web".into()));
        let remote_run = PromptAction::Run { sandbox: "box/web".into(), name: None, branch: None, base: None };
        let (_, root_param, machine, params) = call(&remote_run).unwrap();
        assert_eq!((root_param, machine, &params["sandbox"]), ("/r/one".into(), "box".into(), &"web".into()));
        // Minus the routing param, the request the machine would get from
        // its own dashboard.
        let (method, mut params) = routed_verb_call(&remote_run, root, &verb_route(&s, &remote_run), env.clone(), tty).unwrap();
        params.as_object_mut().unwrap().remove("machine");
        assert_eq!((method, params), verb_call(&run_action(None, None, None), Path::new("/r/one"), env.clone(), tty).unwrap());
        let err = call(&PromptAction::Rm { instance: "far/old".into(), force: false, delete_branch: None }).unwrap_err();
        assert_eq!(err.to_string(), "no config root known on machine `far`");
        // Its status lines keep the qualified name.
        assert_eq!(verb_pending(&PromptAction::Stop { instance: "box/web-2".into() }), "stopping box/web-2…");

        let config = app::Fetch::Config { kind: app::ConfigKind::Sandbox, name: "box/web".into() };
        assert_eq!(fetch_route(&s, &config), remote::Route { machine: Some("box".into()), root: Some("/r/one".into()) });
        let logs = app::Fetch::Logs { container: "box/devsandbox-web-2".into() };
        assert_eq!(fetch_route(&s, &logs), remote::Route::default());
    }

    /// Each verb action's request carries the root and env and is what the
    /// daemon's `serve::verbs::parse` accepts; every one is `detach` (the
    /// held ones too: a quit mustn't cancel them).
    #[test]
    fn verb_calls_are_requests_the_daemon_accepts() {
        use crate::serve::verbs::{self, Target, Verb};
        let root = std::env::temp_dir().canonicalize().unwrap();
        let env: BTreeMap<String, String> = [("HOME".to_string(), "/home/me".to_string())].into();
        let tty = verb_tty(100, 300);
        assert_eq!(tty, verbs::TtySize { cols: 98, rows: VERB_TTY_ROWS }, "inside the border, rows capped");
        assert_eq!(verb_tty(160, 30), verbs::TtySize { cols: 158, rows: 28 }, "the modal's inside");
        let parse = |action: &PromptAction, detach: bool| {
            let (method, params) = verb_call(action, &root, env.clone(), tty).unwrap();
            let req = verbs::parse(method, params).unwrap();
            assert_eq!(req.detach, detach, "{action:?}");
            assert_eq!((req.root.as_path(), &req.env), (root.as_path(), &env));
            // A pty for the build verbs only (the held ones log nothing).
            let builds = matches!(action, PromptAction::Run { .. } | PromptAction::Rebuild { .. } | PromptAction::Start { .. });
            assert_eq!(req.tty, builds.then_some(tty), "{action:?}");
            req.verb
        };
        assert_eq!(
            parse(&run_action(Some("w2"), Some("feat/x"), Some("-odd")), true),
            Verb::Run {
                sandbox: "web".into(),
                name: Some("w2".into()),
                branch: Some("feat/x".into()),
                base: Some("-odd".into()),
                json: false,
                reprobe: false
            }
        );
        assert_eq!(
            parse(&PromptAction::Rebuild { instance: "web-2".into(), force: true }, true),
            Verb::Rebuild { target: Target::One("web-2".into()), force: true, reprobe: false }
        );
        assert_eq!(parse(&PromptAction::Start { instance: "web-2".into() }, true), Verb::Start { target: Target::One("web-2".into()) });
        assert_eq!(parse(&PromptAction::Stop { instance: "web-2".into() }, true), Verb::Stop { target: Target::One("web-2".into()) });
        // An unanswered rm asks; the modal's answer goes back as
        // `deleteBranch`, same `force`, no `ask`.
        let rm = |force, delete_branch| PromptAction::Rm { instance: "web-2".into(), force, delete_branch };
        assert_eq!(
            parse(&rm(true, None), true),
            Verb::Rm { instance: "web-2".into(), delete_branch: None, force: true, ask: true }
        );
        assert_eq!(
            parse(&rm(true, Some(false)), true),
            Verb::Rm { instance: "web-2".into(), delete_branch: Some(false), force: true, ask: false }
        );
        assert_eq!(
            parse(&rm(false, Some(true)), true),
            Verb::Rm { instance: "web-2".into(), delete_branch: Some(true), force: false, ask: false }
        );
        assert_eq!(parse(&PromptAction::ServiceRebuild { name: "cache".into() }, true), Verb::ServicesRebuild { name: "cache".into() });
        assert_eq!(verb_call(&PromptAction::Exec { instance: "web-2".into(), argv: Vec::new() }, &root, env, tty), None);
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
        // Placed on a fleet worker: named there.
        assert_eq!(
            status(&run_action(Some("w2"), None, None), ok(json!({"instance": "w2", "detached": true, "machine": "lap"}))),
            "run web --name w2: lap/w2 started (b opens its build log)"
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

        // No daemon (no fallback), or one that went away.
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
}
