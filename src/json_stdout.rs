//! Clean stdout for `--json` verbs that run child processes (`run`: image
//! builds, `docker run -d` printing the container id, lifecycle hooks, git,
//! autostart). Threading a "stdout goes to stderr" flag through every spawn
//! site would miss the next one added; instead the process's own stdout is
//! pointed at stderr up front, so every child inheriting stdio (and every
//! stray `println!`) lands on stderr, and the one JSON document is written to
//! a saved handle on the original stdout. A programmatic caller can then parse
//! stdout whole and still show progress from stderr.

use std::fs::File;
use std::io::Write;

use anyhow::{Context, Result};

/// The original stdout, held while fd 1 / `STD_OUTPUT_HANDLE` points at
/// stderr. Not restored: the verb emits and the process exits.
pub struct JsonStdout {
    out: File,
}

impl JsonStdout {
    /// Save the current stdout and redirect it to stderr. Must run before any
    /// child is spawned: children inherit whatever stdout is at spawn time.
    pub fn capture() -> Result<Self> {
        std::io::stdout().flush()?;
        let out = redirect().context("redirect stdout to stderr for --json")?;
        Ok(JsonStdout { out })
    }

    /// Write `doc` (plus a newline) to the original stdout.
    pub fn emit(mut self, doc: &str) -> Result<()> {
        writeln!(self.out, "{doc}")?;
        self.out.flush()?;
        Ok(())
    }
}

#[cfg(unix)]
fn redirect() -> std::io::Result<File> {
    use std::os::fd::AsFd;
    let saved = std::io::stdout().as_fd().try_clone_to_owned()?;
    unsafe extern "C" {
        fn dup2(old: i32, new: i32) -> i32;
    }
    // SAFETY: a plain syscall on integers; fds 1 and 2 are the process's
    // standard streams, and `saved` keeps the original stdout open.
    if unsafe { dup2(2, 1) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(File::from(saved))
}

/// Windows: std resolves `STD_OUTPUT_HANDLE` on every write and at every
/// spawn, so swapping it covers `println!` and inherited child stdio alike.
#[cfg(windows)]
fn redirect() -> std::io::Result<File> {
    use std::os::windows::io::{AsHandle, AsRawHandle};
    let saved = std::io::stdout().as_handle().try_clone_to_owned()?;
    unsafe extern "system" {
        fn SetStdHandle(std_handle: u32, handle: *mut std::ffi::c_void) -> i32;
    }
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    let stderr = std::io::stderr().as_raw_handle();
    // SAFETY: swaps the process's stdout handle slot for the (still open)
    // stderr handle; `saved` is an owned duplicate of the original.
    if unsafe { SetStdHandle(STD_OUTPUT_HANDLE, stderr) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(File::from(saved))
}

#[cfg(not(any(unix, windows)))]
fn redirect() -> std::io::Result<File> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "not supported on this platform"))
}
