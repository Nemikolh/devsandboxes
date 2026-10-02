use std::fmt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};

use super::resolve_instance;
use crate::config::Config;
use crate::runtime::{backend, bounded};
use crate::state::{Instance, State};

/// How long `devsbd vscode-goto` polls for the window to attach: a first
/// attach may install the VS Code server before the extension host appears.
const GOTO_WAIT_SECS: u32 = 30;

/// Host-side bound on the whole goto exec: the helper's wait plus slack for
/// the exec itself and the remote CLI. The container controls how long it
/// runs, so it must not hang `vscode` (or the TUI's worker) forever.
const GOTO_TIMEOUT: Duration = Duration::from_secs(GOTO_WAIT_SECS as u64 + 10);

/// Helper stdout kept: it prints one `socket=… pid=… cli=…` line plus the
/// remote CLI's output, none of which is shown.
const GOTO_OUTPUT_CAP: usize = 64 * 1024;

/// `devsbd vscode-goto`'s exit code for "no VS Code window attached".
const GOTO_NO_WINDOW: i32 = 3;

/// A file position to open in the attached window: `path` is relative to the
/// instance's workspace folder and normalized (no `.`/empty components), with
/// no `..` and not absolute, so joining it can't leave the workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Goto {
    pub path: String,
    pub line: Option<u32>,
    /// Only with a `line` (VS Code's `-g` has no column-only form).
    pub col: Option<u32>,
}

impl Goto {
    /// Validate and normalize the parts (the Inbox `vscode` action carries
    /// them separately).
    pub fn new(path: &str, line: Option<u32>, col: Option<u32>) -> Result<Goto, String> {
        if path.starts_with('/') || path.starts_with('~') {
            return Err(format!("goto path must be relative to the workspace: {path}"));
        }
        let parts: Vec<&str> = path.split('/').filter(|c| !c.is_empty() && *c != ".").collect();
        if parts.contains(&"..") {
            return Err(format!("goto path must stay inside the workspace: {path}"));
        }
        if parts.is_empty() {
            return Err("goto path is empty".into());
        }
        if line == Some(0) || col == Some(0) {
            return Err("goto line and column start at 1".into());
        }
        if col.is_some() && line.is_none() {
            return Err("goto column needs a line".into());
        }
        Ok(Goto { path: parts.join("/"), line, col })
    }

    /// `PATH[:LINE[:COL]]`. Numbers are taken from the right, so a `:` inside
    /// the path (`a:b.rs:3`) survives; what's left must not still end in
    /// `:<digits>` (`x:1:2:3`), which VS Code's `-g` would read differently.
    pub fn parse(spec: &str) -> Result<Goto, String> {
        let mut path = spec;
        let mut nums = Vec::new();
        while nums.len() < 2 {
            let Some((rest, tail)) = path.rsplit_once(':') else { break };
            if tail.is_empty() {
                return Err(format!("empty line or column in `{spec}`"));
            }
            if !tail.bytes().all(|b| b.is_ascii_digit()) {
                break;
            }
            nums.push(tail.parse::<u32>().map_err(|_| format!("line or column too large in `{spec}`"))?);
            path = rest;
        }
        if nums.len() == 2
            && path.rsplit_once(':').is_some_and(|(_, t)| t.bytes().all(|b| b.is_ascii_digit()))
        {
            return Err(format!("ambiguous goto `{spec}`: expected PATH[:LINE[:COL]]"));
        }
        nums.reverse();
        Goto::new(path, nums.first().copied(), nums.get(1).copied())
    }

    /// The in-container absolute path: `workspace` is the instance's
    /// workspace folder as the container sees it (`Instance::workspace`).
    pub fn abs_in(&self, workspace: &str) -> String {
        format!("{}/{}", workspace.trim_end_matches('/'), self.path)
    }

    /// `devsbd vscode-goto`'s argument: `<abs>[:line[:col]]`.
    fn arg_in(&self, workspace: &str) -> String {
        format!("{}{}", self.abs_in(workspace), self.suffix())
    }

    fn suffix(&self) -> String {
        match (self.line, self.col) {
            (Some(l), Some(c)) => format!(":{l}:{c}"),
            (Some(l), None) => format!(":{l}"),
            _ => String::new(),
        }
    }
}

/// The relative form, `path[:line[:col]]`, as the user wrote it.
impl fmt::Display for Goto {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.path, self.suffix())
    }
}

/// How the in-container half of a goto went.
#[derive(Debug, PartialEq, Eq)]
pub enum GotoOutcome {
    Opened,
    /// No window attached within the helper's wait.
    NotReady,
    /// No helper recorded for the instance: nothing to exec.
    NoHelper,
    /// Anything else, with a one-line reason.
    Failed(String),
}

/// The status line for a launch with a goto; `hint` is the runtime caveat
/// `launch` appends either way.
pub fn goto_status(goto: &Goto, outcome: &GotoOutcome, hint: &str) -> String {
    let why = match outcome {
        GotoOutcome::Opened => return format!("opened VS Code at {goto}{hint}"),
        GotoOutcome::NotReady => "window not ready",
        GotoOutcome::NoHelper => "no devsbd helper",
        GotoOutcome::Failed(why) => why,
    };
    format!("opened VS Code; --goto dropped ({why}){hint}")
}

/// Run `devsbd vscode-goto` as root in `container` (root to find the window
/// whoever owns it; the helper drops to the owner for the CLI). Captured and
/// bounded, never inherited: the TUI runs this while it owns the terminal,
/// and the container controls the output and the duration.
pub fn goto_in(container: &str, workspace: &str, goto: &Goto) -> GotoOutcome {
    let arg = goto.arg_in(workspace);
    let wait = GOTO_WAIT_SECS.to_string();
    let mut cmd = Command::new(backend().bin());
    cmd.args(["exec", "-u", "root", container, crate::devsbd::BIN, "vscode-goto", &arg, "--wait", &wait])
        .stdin(Stdio::null());
    match bounded::run(&mut cmd, GOTO_OUTPUT_CAP, GOTO_TIMEOUT) {
        Ok(out) if out.status.success() => GotoOutcome::Opened,
        Ok(out) if out.status.code() == Some(GOTO_NO_WINDOW) => GotoOutcome::NotReady,
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            GotoOutcome::Failed(match stderr.lines().map(str::trim).find(|l| !l.is_empty()) {
                Some(line) => line.to_string(),
                None => match out.status.code() {
                    Some(code) => format!("exit {code}"),
                    None => "killed".into(),
                },
            })
        }
        Err(e) => GotoOutcome::Failed(e.to_string()),
    }
}

/// CLI entry point: open VS Code attached to the instance `name` resolves to
/// (same as `o` in the dashboard), at `goto` when given.
pub fn vscode(dir: &Path, name: &str, goto: Option<&Goto>) -> Result<()> {
    let state = State::load()?;
    let key = resolve_instance(&state, name)?;
    let info = &state.instances[&key];
    println!("{}", launch(dir, &key, info, goto)?);
    Ok(())
}

/// Launch VS Code attached to `info`'s container, detached. Writes the
/// extensions name-config (best-effort), then spawns `code`. With `goto`,
/// then has the helper open the file in that window (docs/inbox-threads.md,
/// *VS Code at a line*): the host `code` always runs first, since it opens or
/// focuses the window the in-container CLI needs. Blocks up to
/// [`GOTO_TIMEOUT`] then. Returns a one-line status. Shared with the TUI's
/// `o` so both open identically.
pub fn launch(dir: &Path, instance: &str, info: &Instance, goto: Option<&Goto>) -> Result<String> {
    // Extensions and remoteUser come from the resolved sandbox, read from the
    // instance's own config root: state is global, so `dir` (the cwd by
    // default) may be another root or none. Failure to resolve is non-fatal —
    // keep the extensions `run` registered and fall back to the remote user
    // recorded in state so the attach still opens with write access.
    let config_dir = info.config_dir.as_deref().unwrap_or(dir);
    let resolved = Config::load(config_dir)
        .and_then(|cfg| cfg.resolve_sandbox(&info.sandbox))
        .ok();
    let extensions: Option<Vec<String>> = resolved.as_ref().map(|sb| {
        sb.properties.vscode_extensions().map(<[String]>::to_vec).unwrap_or_default()
    });
    let remote_user = resolved
        .as_ref()
        .and_then(|sb| sb.properties.remote_user.clone())
        .or_else(|| info.remote_user.clone());
    let _ = super::run::write_vscode_name_config(
        &info.container,
        extensions.as_deref(),
        remote_user.as_deref(),
    );

    // Prefer the generated `.code-workspace` (window named after the instance,
    // carries the extra `folders` roots); instances created before it existed
    // fall back to a plain folder open.
    let (flag, path) = match &info.workspace_file {
        Some(file) => ("--file-uri", file.as_str()),
        None => ("--folder-uri", info.workspace.as_str()),
    };
    // The Remote-Containers extension resolves a different authority per runtime.
    // Docker/podman use `attached-container` (hex of the bare container name).
    // Apple `container` uses `apple-container` (hex of a JSON `{id, image}`
    // payload) and requires the user's opt-in
    // `dev.containers.experimentalAppleContainerSupport` setting.
    let (authority, hint) = if backend().name() == "container" {
        let image = apple_image_reference(&info.container).unwrap_or_default();
        let payload = serde_json::json!({ "id": info.container, "image": image }).to_string();
        (
            format!("apple-container+{}", hex_encode(&payload)),
            " (needs dev.containers.experimentalAppleContainerSupport=true)",
        )
    } else {
        (format!("attached-container+{}", hex_encode(&info.container)), "")
    };
    let uri = format!("vscode-remote://{authority}/{path}");
    // Detached and silent: the dashboard calls this while it owns the
    // terminal, so `code`'s own chatter must not land on the screen.
    std::process::Command::new("code")
        .args([flag, &uri])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("code: failed to launch (`code` on PATH?)")?;
    let Some(goto) = goto else {
        return Ok(format!("opening VS Code → {instance}{hint}"));
    };
    let outcome = match info.devsbd_arch {
        Some(_) => goto_in(&info.container, &info.workspace, goto),
        None => GotoOutcome::NoHelper,
    };
    Ok(goto_status(goto, &outcome, hint))
}

/// Apple `container` image reference (`configuration.image.reference`) for
/// `container`, needed in the `apple-container` attach URI payload. Best-effort:
/// `None` when inspect fails or the field is absent, in which case the caller
/// sends an empty image (the resolver only requires `id`).
fn apple_image_reference(container: &str) -> Option<String> {
    let json = backend().inspect_json(container).ok()?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn goto(path: &str, line: Option<u32>, col: Option<u32>) -> Goto {
        Goto { path: path.into(), line, col }
    }

    #[test]
    fn goto_parses_numbers_from_the_right() {
        assert_eq!(Goto::parse("x.rs"), Ok(goto("x.rs", None, None)));
        assert_eq!(Goto::parse("x.rs:3"), Ok(goto("x.rs", Some(3), None)));
        assert_eq!(Goto::parse("x.rs:3:4"), Ok(goto("x.rs", Some(3), Some(4))));
        // A `:` in the path survives.
        assert_eq!(Goto::parse("a:b.rs:3"), Ok(goto("a:b.rs", Some(3), None)));
        assert_eq!(Goto::parse("a:b.rs"), Ok(goto("a:b.rs", None, None)));
        // Normalized: `.` and empty components dropped.
        assert_eq!(Goto::parse("./src//a.rs/:7"), Ok(goto("src/a.rs", Some(7), None)));
        assert_eq!(Goto::parse("x.rs:3:4").unwrap().to_string(), "x.rs:3:4");
    }

    #[test]
    fn goto_rejects_bad_specs() {
        for bad in [
            "x.rs::3", "x.rs:", "x.rs:3:", "../x", "a/../../x", "/abs", "~/x", "", ".", ":3", "x.rs:0",
            "x.rs:1:0", "x:1:2:3", "x.rs:99999999999",
        ] {
            assert!(Goto::parse(bad).is_err(), "{bad:?} parsed");
        }
        assert_eq!(Goto::new("x.rs", None, Some(2)), Err("goto column needs a line".into()));
    }

    #[test]
    fn goto_joins_the_container_workspace() {
        let g = goto("src/a.rs", Some(3), Some(4));
        assert_eq!(g.abs_in("/workspaces/repo"), "/workspaces/repo/src/a.rs");
        assert_eq!(g.abs_in("/workspaces/repo/"), "/workspaces/repo/src/a.rs");
        assert_eq!(g.abs_in("/"), "/src/a.rs");
        assert_eq!(g.arg_in("/w"), "/w/src/a.rs:3:4");
        assert_eq!(goto("a.rs", Some(3), None).arg_in("/w"), "/w/a.rs:3");
        assert_eq!(goto("a.rs", None, None).arg_in("/w"), "/w/a.rs");
    }

    #[test]
    fn goto_status_per_outcome() {
        let g = goto("src/a.rs", Some(3), None);
        assert_eq!(goto_status(&g, &GotoOutcome::Opened, ""), "opened VS Code at src/a.rs:3");
        assert_eq!(
            goto_status(&g, &GotoOutcome::NotReady, ""),
            "opened VS Code; --goto dropped (window not ready)"
        );
        assert_eq!(
            goto_status(&g, &GotoOutcome::NoHelper, " (hint)"),
            "opened VS Code; --goto dropped (no devsbd helper) (hint)"
        );
        assert_eq!(
            goto_status(&g, &GotoOutcome::Failed("exit 1".into()), ""),
            "opened VS Code; --goto dropped (exit 1)"
        );
    }

    /// `goto_in` against step 8's fake window (`devsbd::fake_vscode_window`):
    /// the helper ends up running the window's CLI with the joined path.
    #[test_utils::docker_test(helper)]
    fn goto_in_opens_the_joined_path_in_the_window_with_docker() -> Result<(), &'static str> {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!("devsandbox-vscode-goto-test-{stamp}");
        let up = Command::new("docker")
            .args(["run", "-d", "--rm", "--name", &name, "python:3.13-alpine", "sleep", "300"])
            .output()
            .unwrap();
        assert!(up.status.success(), "{}", String::from_utf8_lossy(&up.stderr));
        let cleanup = || {
            let _ = Command::new("docker").args(["rm", "-f", &name]).output();
        };
        crate::test_support::with_cleanup(cleanup, || {
            crate::devsbd::install(&name, None).unwrap();
            crate::devsbd::fake_vscode_window(&name);
            let outcome = goto_in(&name, "/workspaces/repo/", &goto("src/a.rs", Some(4), Some(2)));
            assert_eq!(outcome, GotoOutcome::Opened);
            let argv = Command::new("docker").args(["exec", &name, "cat", "/tmp/rec/argv"]).output().unwrap();
            assert_eq!(String::from_utf8_lossy(&argv.stdout), "-g\n/workspaces/repo/src/a.rs:4:2\n");
        });
        Ok(())
    }

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
