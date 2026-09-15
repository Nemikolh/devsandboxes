//! Interactive dashboard (`devsandbox` with no subcommand on a TTY).
//!
//! This module owns all terminal I/O: raw mode, the alternate screen, the
//! panic hook that restores them, and the crossterm event loop. The state
//! machine ([`app::App`]) and rendering ([`ui::draw`]) stay I/O-free.

mod app;
mod data;
mod prompt;
mod ui;

use std::io::{self, Stdout, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{self, Event};
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
use prompt::PromptAction;

/// How long each `event::poll` blocks before we redraw.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Data-refresh cadence: how often a background collection is kicked off.
const TICK_INTERVAL: Duration = Duration::from_secs(2);

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
    execute!(stdout, EnterAlternateScreen)?;
    let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    Ok(terminal)
}

/// Best-effort teardown: leave the alternate screen and disable raw mode.
/// Errors are ignored — there is nothing useful to do while cleaning up.
fn restore() {
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
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
    let mut last_tick = Instant::now();

    while !app.should_quit {
        terminal.draw(|frame| ui::draw(frame, &app))?;

        if event::poll(POLL_INTERVAL)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == event::KeyEventKind::Press {
                    app.on_key(key);
                }
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

    // Re-enter: the outer setup already ran once; re-arm the alt screen.
    execute!(io::stdout(), EnterAlternateScreen)?;
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

    // Extensions come from the resolved sandbox; failure to resolve is
    // non-fatal — attach still works, just without registering extensions.
    let extensions: Vec<String> = Config::load(dir)
        .ok()
        .and_then(|cfg| cfg.resolve_sandbox(&row.sandbox).ok())
        .and_then(|sb| sb.properties.vscode_extensions().map(<[String]>::to_vec))
        .unwrap_or_default();
    let _ = commands::run::write_vscode_name_config(&row.container, &extensions);

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
