//! Shared helpers for tests that need a real container runtime or the embedded
//! devsbd helper. Tests opt in with `#[test_utils::docker_test]` /
//! `#[test_utils::helper_test]` / `#[test_utils::host_test]`, which expand to
//! a call to [`gated`].
//!
//! "Docker" tests run against the runtime `DEVSANDBOX_RUNTIME` selects
//! (`runtime::backend()`), so CI runs the same tests under podman too; they
//! drive it through [`runtime_cmd`], never a hardcoded `docker`. A test about
//! a docker-only mechanism says so (`#[docker_test(docker_only)]`) and is not
//! applicable elsewhere.

use std::fmt::Display;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::process::Command;

/// An environment requirement a gated test declares.
pub enum Need {
    /// `<runtime> info` succeeds for the selected runtime.
    Runtime,
    /// The selected runtime is docker. Checked before the other needs: under
    /// another runtime the test is out of scope, not skipped, so it passes on
    /// CI's podman job instead of failing it.
    DockerOnly,
    /// A devsbd helper for the host arch is embedded (`scripts/build-devsbd.sh`).
    Helper,
}

impl Need {
    fn missing(&self) -> Option<&'static str> {
        match self {
            Need::Runtime => {
                let up = matches!(runtime_cmd().arg("info").output(), Ok(o) if o.status.success());
                (!up).then_some("container runtime unavailable")
            }
            // Handled by `gated` before anything else.
            Need::DockerOnly => None,
            Need::Helper => {
                use crate::devsbd::{Arch, hash};
                hash(Arch::host()).is_none().then_some("no devsbd helper embedded")
            }
        }
    }
}

/// Run a gated test: skip (see [`skip`]) on the first unmet need, else run
/// `body` and treat its `Err(why)` the same way.
pub fn gated<E: Display>(test: &str, needs: &[Need], body: impl FnOnce() -> Result<(), E>) {
    let runtime = crate::runtime::backend().name();
    if needs.iter().any(|n| matches!(n, Need::DockerOnly)) && runtime != "docker" {
        return eprintln!("{test}: docker only, not applicable under {runtime}");
    }
    if let Some(why) = needs.iter().find_map(Need::missing) {
        return skip(test, why);
    }
    if let Err(why) = body() {
        skip(test, &why.to_string());
    }
}

/// A command for the selected runtime's CLI (`docker`, `podman`, …), what
/// gated tests drive containers with.
pub fn runtime_cmd() -> Command {
    Command::new(crate::runtime::backend().bin())
}

/// Give a test child that runs with its own `XDG_DATA_HOME` (`data`) and
/// `XDG_RUNTIME_DIR` (`runtime`) the runtime's own state back. Rootless
/// podman keeps its storage and its pause process under those dirs, so a
/// fresh pair would mean an empty store owned by subuids (that the test's
/// cleanup can't delete) and a second user namespace. Under podman this
/// links `data/containers` and `runtime/{containers,libpod}` to the
/// parent's; docker needs nothing. Removing the child's dirs removes only
/// the links.
#[cfg(unix)]
pub fn share_runtime_dirs(data: &std::path::Path, runtime: &std::path::Path) {
    use std::os::unix::fs::{DirBuilderExt, symlink};
    std::fs::create_dir_all(data).unwrap();
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(runtime).unwrap();
    if crate::runtime::backend().name() != "podman" {
        return;
    }
    let out = runtime_cmd().args(["info", "--format", "{{.Store.GraphRoot}}\n{{.Store.RunRoot}}"]).output().unwrap();
    assert!(out.status.success(), "podman info: {}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut lines = text.lines().map(std::path::Path::new);
    let (Some(graph), Some(run)) = (lines.next(), lines.next()) else { panic!("podman info: {text}") };
    // `<data>/containers/storage` and `<run dir>/containers` by default.
    let (Some(store), Some(run_dir)) = (graph.parent(), run.parent()) else { panic!("podman info: {text}") };
    symlink(store, data.join("containers")).unwrap();
    for name in ["containers", "libpod"] {
        symlink(run_dir.join(name), runtime.join(name)).unwrap();
    }
}

/// Run `body`, then `cleanup` even if `body` panicked, then re-raise the panic
/// or return `body`'s value. Container tests use it to always remove what they
/// started. `body` comes last so it reads as a trailing block.
pub fn with_cleanup<T>(cleanup: impl FnOnce(), body: impl FnOnce() -> T) -> T {
    let result = catch_unwind(AssertUnwindSafe(body));
    cleanup();
    result.unwrap_or_else(|panic| resume_unwind(panic))
}

/// Skip an environment-gated test locally, but fail it on CI.
///
/// Docker/helper-gated tests skip on dev machines that lack docker, network or
/// a built helper. On CI a skip would silently hide the only real end-to-end
/// coverage, so there it panics instead. Keyed on the `CI` env var, which
/// GitHub Actions (and most other CI systems) sets on every runner by itself,
/// so no edit to the workflow file can quietly turn these tests into no-ops.
fn skip(test: &str, why: &str) {
    if on_ci() {
        panic!("{test}: {why}; environment-gated tests must never skip on CI (`CI` is set)");
    }
    eprintln!("skipping {test}: {why}");
}

fn on_ci() -> bool {
    std::env::var("CI").is_ok_and(|v| !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"))
}
