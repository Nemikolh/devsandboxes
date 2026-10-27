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
