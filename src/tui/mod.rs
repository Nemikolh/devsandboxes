//! Interactive dashboard (`devsandbox` with no subcommand on a TTY).
//!
//! This module owns all terminal I/O: raw mode, the alternate screen, the
//! panic hook that restores them, and the crossterm event loop. The state
//! machine ([`app::App`]) and rendering ([`ui::draw`]) stay I/O-free.

mod app;
mod ui;

use std::io::{self, Stdout};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{self, Event};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::execute;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use app::App;

/// How long each `event::poll` blocks before we redraw. A future step adds a
/// 2s data-refresh tick; the loop is structured so that branch drops in easily.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Data-refresh cadence. Nothing hangs off it yet (step 3 wires in docker).
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

        if last_tick.elapsed() >= TICK_INTERVAL {
            // Data refresh lands in step 3; nothing to do on the tick yet.
            last_tick = Instant::now();
        }
    }
    Ok(())
}
