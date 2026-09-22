//! Interactive dashboard (`devsandbox` with no subcommand on a TTY).
//!
//! This module owns all terminal I/O: raw mode, the alternate screen, the
//! panic hook that restores them, and the crossterm event loop. The state
//! machine ([`app::App`]) and rendering ([`ui::draw`]) stay I/O-free.

mod app;
mod data;
mod procs;
mod prompt;
mod term;
mod ui;

use std::collections::BTreeMap;
use std::io::{self, Stdout, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event};
use ratatui::layout::Rect;
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

/// How long each `event::poll` blocks before we redraw when no terminal is open.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Shorter poll used while any integrated terminal is open. Key events already
/// wake `event::poll`, but child *output* arrives on the reader threads and only
/// becomes visible on the next draw — a 250ms cadence makes shell echo feel
/// laggy, so we redraw ~33×/s to keep echo snappy while terminals exist.
const TERM_POLL_INTERVAL: Duration = Duration::from_millis(30);
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
    // Background `s` stops/starts, each reporting completion over its own
    // channel; paired with the instance name so the guard clears even if the
    // thread dies.
    let mut stops: Vec<(String, Receiver<OpDone>)> = Vec::new();
    let mut starts: Vec<(String, Receiver<OpDone>)> = Vec::new();
    let mut last_tick = Instant::now();
    // At most one process fetch in flight; `Some` while one is running.
    let mut proc_pending: Option<Receiver<BTreeMap<String, ProcState>>> = None;
    let mut last_proc_tick = Instant::now();

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
                    app.status = Some(launch_code(&dir, &app, &instance));
                }
                // Rename is pure state I/O: run it in place (no suspend) and
                // force an immediate resnapshot so the renamed row shows now.
                PromptAction::Rename { instance, new_name } => {
                    app.status = Some(match commands::rename::rename(&instance, &new_name) {
                        Ok(()) => format!("renamed {instance} -> {new_name}"),
                        Err(e) => format!("rename failed: {e:#}"),
                    });
                    last_tick = Instant::now();
                    pending = Some(spawn_collect(&dir));
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

        // A background stop/start was requested by `s`: spawn it without
        // suspending.
        if let Some(instance) = app.take_pending_stop() {
            stops.push((instance.clone(), spawn_stop(&instance)));
        }
        if let Some(instance) = app.take_pending_start() {
            starts.push((instance.clone(), spawn_start(&instance)));
        }

        // Drain any finished background stops/starts: update the status line,
        // clear the in-flight guard, and force an immediate snapshot refresh so
        // the row's new status shows without waiting for the next tick.
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
        starts.retain_mut(|(instance, rx)| match rx.try_recv() {
            Ok(done) => {
                app.starting.remove(instance);
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
                app.starting.remove(instance);
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
        PromptAction::Run { sandbox, name, branch } => {
            commands::run::run(dir, Some(sandbox.clone()), name.clone(), branch.clone())
        }
        PromptAction::Exec { instance, argv } => {
            commands::exec::exec_status(instance, true, true, argv).map(|_| ())
        }
        PromptAction::Rm { instance } => commands::rm::rm(instance),
        PromptAction::Stop { instance } => commands::stop::stop(Some(instance.clone()), false),
        PromptAction::Start { instance } => {
            commands::start::start(dir, Some(instance.clone()), false)
        }
        PromptAction::Rebuild { instance } => {
            commands::rebuild::rebuild(dir, Some(instance.clone()), false)
        }
        PromptAction::ServiceRebuild { name } => commands::services::rebuild(dir, name),
        // `code` and `rename` never suspend; handled by the caller.
        PromptAction::Code { .. } | PromptAction::Rename { .. } => Ok(()),
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

    // Re-enter: the outer setup already ran once; re-arm the alt screen + mouse.
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
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

    // Prefer the generated `.code-workspace` (window named after the instance,
    // carries the extra `folders` roots); instances created before it existed
    // fall back to a plain folder open. Row names equal state instance keys.
    let workspace_file = crate::state::State::load()
        .ok()
        .and_then(|s| s.instances.get(instance).and_then(|i| i.workspace_file.clone()));
    let (flag, path) = match &workspace_file {
        Some(file) => ("--file-uri", file.as_str()),
        None => ("--folder-uri", row.workspace.as_str()),
    };
    // The Remote-Containers extension resolves a different authority per runtime.
    // Docker/podman use `attached-container` (hex of the bare container name).
    // Apple `container` uses `apple-container` (hex of a JSON `{id, image}`
    // payload) and requires the user's opt-in
    // `dev.containers.experimentalAppleContainerSupport` setting.
    let backend = crate::runtime::backend();
    let (authority, hint) = if backend.name() == "container" {
        let image = apple_image_reference(&row.container).unwrap_or_default();
        let payload = serde_json::json!({ "id": row.container, "image": image }).to_string();
        (
            format!("apple-container+{}", hex_encode(&payload)),
            " (needs dev.containers.experimentalAppleContainerSupport=true)",
        )
    } else {
        (format!("attached-container+{}", hex_encode(&row.container)), "")
    };
    let uri = format!("vscode-remote://{authority}/{path}");
    match std::process::Command::new("code").args([flag, &uri]).spawn() {
        Ok(_) => format!("opening VS Code → {instance}{hint}"),
        Err(e) => format!("code: failed to launch (`code` on PATH?): {e}"),
    }
}

/// Apple `container` image reference (`configuration.image.reference`) for
/// `container`, needed in the `apple-container` attach URI payload. Best-effort:
/// `None` when inspect fails or the field is absent, in which case the caller
/// sends an empty image (the resolver only requires `id`).
fn apple_image_reference(container: &str) -> Option<String> {
    let json = crate::runtime::backend().inspect_json(container).ok()?;
    let v: serde_json::Value = serde_json::from_str(&json).ok()?;
    let obj = v.get(0).unwrap_or(&v);
    obj.get("configuration")?
        .get("image")?
        .get("reference")?
        .as_str()
        .map(str::to_string)
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

/// Result of a background `s` stop/start, drained by the event loop.
struct OpDone {
    /// One-line outcome for the help-bar status.
    status: String,
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
                        commands::stop::service_containers(&info.project, &instance);
                    commands::stop::stop_containers(&info.container, &services, true);
                    format!("stopped {instance}")
                }
                None => format!("stop: unknown instance `{instance}`"),
            },
            Err(e) => format!("stop: {e:#}"),
        };
        let _ = tx.send(OpDone { status });
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
                    let services =
                        commands::stop::service_containers(&info.project, &instance);
                    commands::start::start_containers(&info.container, &services, true);
                    format!("started {instance}")
                }
                None => format!("start: unknown instance `{instance}`"),
            },
            Err(e) => format!("start: {e:#}"),
        };
        let _ = tx.send(OpDone { status });
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

    #[test]
    fn apple_authority_payload_roundtrips() {
        // The `apple-container` authority carries hex of a JSON `{id, image}`
        // payload; VS Code hex-decodes and `JSON.parse`s it. Verify the encoding
        // devsandbox emits decodes back to that object.
        let payload = serde_json::json!({ "id": "devsandbox-web", "image": "img:latest" })
            .to_string();
        let hex = hex_encode(&payload);
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        let decoded: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded["id"], "devsandbox-web");
        assert_eq!(decoded["image"], "img:latest");
    }
}
