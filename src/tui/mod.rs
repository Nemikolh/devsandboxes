//! Interactive dashboard (`devsandbox` with no subcommand on a TTY).
//!
//! This module owns all terminal I/O: raw mode, the alternate screen, the
//! panic hook that restores them, and the crossterm event loop. The state
//! machine ([`app::App`]) and rendering ([`ui::draw`]) stay I/O-free.

mod app;
mod data;
mod procs;
mod prompt;
mod ui;

use std::collections::BTreeMap;
use std::io::{self, Stdout, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::execute;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::commands;
use crate::config::Config;

use app::App;
use data::Snapshot;
use procs::{parse_top, build_forest, ProcState};
use prompt::PromptAction;

/// How long each `event::poll` blocks before we redraw.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Data-refresh cadence: how often a background collection is kicked off.
const TICK_INTERVAL: Duration = Duration::from_secs(2);
/// Process-refresh cadence for expanded instances (separate from the snapshot).
const PROC_TICK: Duration = Duration::from_secs(5);

type Term = Terminal<CrosstermBackend<Stdout>>;

/// Launch the dashboard, guaranteeing the terminal is restored on every exit
/// path (normal, error, or panic).
pub fn dashboard(dir: &Path) -> Result<()> {
    install_panic_hook();
    let mut terminal = setup().context("failed to set up terminal")?;

    let result = run(&mut terminal, App::new(dir.to_path_buf()));

    // Restore on both the Ok and Err paths before returning.
    restore();
    result
}

/// Enable raw mode, enter the alternate screen, build the ratatui terminal.
fn setup() -> Result<Term> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    Ok(terminal)
}

/// Best-effort teardown: leave the alternate screen and disable raw mode.
/// Errors are ignored — there is nothing useful to do while cleaning up.
fn restore() {
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
    // At most one collection thread in flight; `Some` while one is running.
    let mut pending: Option<Receiver<Snapshot>> = Some(spawn_collect(&dir));
    // Background `s` stops, each reporting completion over its own channel;
    // paired with the instance name so the guard clears even if the thread dies.
    let mut stops: Vec<(String, Receiver<StopDone>)> = Vec::new();
    let mut last_tick = Instant::now();
    // At most one process fetch in flight; `Some` while one is running.
    let mut proc_pending: Option<Receiver<BTreeMap<String, ProcState>>> = None;
    let mut last_proc_tick = Instant::now();

    while !app.should_quit {
        terminal.draw(|frame| ui::draw(frame, &app))?;

        if event::poll(POLL_INTERVAL)? {
            match event::read()? {
                Event::Key(key) if key.kind == event::KeyEventKind::Press => app.on_key(key),
                // The modal is full-screen, so the frame width equals the
                // terminal width; feed it in so the divider math stays I/O-free.
                Event::Mouse(ev) => app.on_mouse(&ev, terminal.size()?.width),
                _ => {}
            }
        }

        // A prompt command is ready: suspend the TUI (or, for `code`, just
        // launch it), then force a redraw + immediate refresh on return.
        if let Some(action) = app.take_pending_action() {
            match action {
                PromptAction::Code { instance } => {
                    app.status = Some(launch_code(&dir, &app, &instance));
                }
                other => {
                    run_suspended(terminal, &dir, other)?;
                    // Redraw from scratch: the child scribbled over the screen.
                    terminal.clear()?;
                    // Refresh data now rather than waiting for the next tick.
                    last_tick = Instant::now();
                    pending = Some(spawn_collect(&dir));
                }
            }
        }

        // A background stop was requested by `s`: spawn it without suspending.
        if let Some(instance) = app.take_pending_stop() {
            stops.push((instance.clone(), spawn_stop(&instance)));
        }

        // Drain any finished background stops: update the status line, clear the
        // in-flight guard, and force an immediate snapshot refresh so the row's
        // new (stopped) status shows without waiting for the next tick.
        stops.retain_mut(|(instance, rx)| match rx.try_recv() {
            Ok(done) => {
                app.stopping.remove(instance);
                app.status = Some(done.status);
                last_tick = Instant::now();
                if pending.is_none() {
                    pending = Some(spawn_collect(&dir));
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

        // Drain a finished collection into the app, freeing the in-flight slot.
        if let Some(rx) = &pending {
            match rx.try_recv() {
                Ok(snapshot) => {
                    app.set_snapshot(snapshot);
                    pending = None;
                }
                Err(TryRecvError::Empty) => {}
                // Thread died without sending; drop the slot so the next tick retries.
                Err(TryRecvError::Disconnected) => pending = None,
            }
        }

        if last_tick.elapsed() >= TICK_INTERVAL {
            last_tick = Instant::now();
            // Skip if a collection is still running so docker calls can't pile up.
            if pending.is_none() {
                pending = Some(spawn_collect(&dir));
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

        // Process refresh: fetch now on expand, else every PROC_TICK, but only
        // while something is expanded and no fetch is already running.
        let want_now = app.take_needs_proc_fetch();
        let tick_due = last_proc_tick.elapsed() >= PROC_TICK;
        if (want_now || tick_due) && app.has_expanded_procs() && proc_pending.is_none() {
            last_proc_tick = Instant::now();
            // Non-running expanded instances get their `(not running)` row here;
            // running ones come back as fetch targets.
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
        PromptAction::Run { sandbox, name } => {
            commands::run::run(dir, Some(sandbox.clone()), name.clone())
        }
        PromptAction::Exec { instance, argv } => {
            commands::exec::exec_status(instance, true, true, argv).map(|_| ())
        }
        PromptAction::Rm { instance } => commands::rm::rm(instance),
        PromptAction::Stop { instance } => commands::stop::stop(instance),
        // `code` never suspends; handled by the caller.
        PromptAction::Code { .. } => Ok(()),
    };
    if let Err(e) = result {
        eprintln!("error: {e:#}");
    }

    print!("\r\n\x1b[2mpress any key to return\x1b[0m");
    let _ = io::stdout().flush();
    // Raw mode first: cooked mode is line-buffered, so "any key" would
    // otherwise need an Enter before the event arrives.
    enable_raw_mode()?;
    wait_for_key();

    // Re-enter: the outer setup already ran once; re-arm the alt screen + mouse.
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
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

/// Launch VS Code attached to the selected instance's container, detached.
/// Best-effort: writes the extensions name-config, then spawns `code`. Returns
/// a one-line status (error text on failure) for the help-bar.
fn launch_code(dir: &Path, app: &App, instance: &str) -> String {
    let Some(snapshot) = &app.snapshot else {
        return format!("code: no data yet for `{instance}`");
    };
    let Some(row) = snapshot.instances.iter().find(|r| r.name == instance) else {
        return format!("code: unknown instance `{instance}`");
    };

    // Extensions and remoteUser come from the resolved sandbox; failure to
    // resolve is non-fatal — fall back to the remote user recorded in state at
    // run time so the attach still opens with write access.
    let resolved = Config::load(dir)
        .ok()
        .and_then(|cfg| cfg.resolve_sandbox(&row.sandbox).ok());
    let extensions: Vec<String> = resolved
        .as_ref()
        .and_then(|sb| sb.properties.vscode_extensions().map(<[String]>::to_vec))
        .unwrap_or_default();
    let remote_user = resolved
        .as_ref()
        .and_then(|sb| sb.properties.remote_user.clone())
        .or_else(|| row.remote_user.clone());
    let _ = commands::run::write_vscode_name_config(
        &row.container,
        &extensions,
        remote_user.as_deref(),
    );

    let uri = format!(
        "vscode-remote://attached-container+{}/{}",
        hex_encode(&row.container),
        row.workspace
    );
    match std::process::Command::new("code")
        .args(["--folder-uri", &uri])
        .spawn()
    {
        Ok(_) => format!("opening VS Code → {instance}"),
        Err(e) => format!("code: failed to launch (`code` on PATH?): {e}"),
    }
}

/// Lowercase hex of a string's UTF-8 bytes, as the Remote-Containers URI wants.
fn hex_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        out.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    out
}

/// Result of a background `s` stop, drained by the event loop.
struct StopDone {
    /// One-line outcome for the help-bar status.
    status: String,
}

/// Spawn a detached thread that stops `instance` (docker stop can block ~10s on
/// the SIGTERM timeout) and reports a one-line status back. Docker calls go
/// through the screen-safe quiet path so stderr can't corrupt the TUI. `instance`
/// is a snapshot row name, which equals the state instance key.
fn spawn_stop(instance: &str) -> Receiver<StopDone> {
    let (tx, rx) = mpsc::channel();
    let instance = instance.to_string();
    std::thread::spawn(move || {
        let status = match crate::state::State::load() {
            Ok(state) => match state.instances.get(&instance) {
                Some(info) => {
                    let services =
                        commands::stop::service_containers(&info.project, &instance);
                    commands::stop::stop_containers(&info.container, &services, true);
                    format!("stopped {instance}")
                }
                None => format!("stop: unknown instance `{instance}`"),
            },
            Err(e) => format!("stop: {e:#}"),
        };
        let _ = tx.send(StopDone { status });
    });
    rx
}

/// Spawn one detached thread that runs `docker top` for each target
/// `(instance name, container)` and sends back a name→[`ProcState`] map. Docker
/// calls go through the screen-safe quiet path; a per-container error becomes a
/// `(processes unavailable: …)` message row rather than failing the batch.
fn spawn_proc_fetch(targets: Vec<(String, String)>) -> Receiver<BTreeMap<String, ProcState>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut out: BTreeMap<String, ProcState> = BTreeMap::new();
        for (instance, container) in targets {
            let state = match crate::runtime::backend().proc_list(&container) {
                Ok(text) => ProcState::Rows(build_forest(parse_top(&text))),
                Err(e) => ProcState::Message(format!("(processes unavailable: {e:#})")),
            };
            out.insert(instance, state);
        }
        // Receiver may be gone if the UI quit mid-fetch; ignore send errors.
        let _ = tx.send(out);
    });
    rx
}

/// Spawn a detached thread that collects one [`Snapshot`] and sends it back.
/// The receiver is polled from the event loop, keeping [`App`] I/O-free.
fn spawn_collect(dir: &Path) -> Receiver<Snapshot> {
    let (tx, rx) = mpsc::channel();
    let dir: PathBuf = dir.to_path_buf();
    std::thread::spawn(move || {
        // Receiver may be gone if the UI quit mid-collection; ignore send errors.
        let _ = tx.send(data::collect(&dir));
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::hex_encode;

    #[test]
    fn hex_encodes_container_name() {
        // Lowercase hex of the UTF-8 bytes, matching the attach URI format.
        assert_eq!(hex_encode("devsandbox-web"), "64657673616e64626f782d776562");
        assert_eq!(hex_encode(""), "");
        assert_eq!(hex_encode("A/z"), "412f7a");
    }
}
