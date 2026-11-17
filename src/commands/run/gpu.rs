//! The run's GPU flags from `hostRequirements.gpu`
//! (docs/architecture/instances/lifecycle.md, *GPUs*).

use anyhow::{bail, Result};

use crate::config::GpuNeed;
use crate::runtime::gpu::GpuSupport;
use crate::worker_policy::Refusal;

const OPTIONAL_HINT: &str = "set `hostRequirements.gpu = \"optional\"` to run without one";

/// The `run` flags for the sandbox's GPU `need`, plus the warnings to
/// print; an error when a required GPU can't be had. `support` is the
/// runtime's answer (`Backend::gpu_support`), asked only when a GPU is
/// wanted and the worker policy (`allowed`: none applies, or it has
/// `gpus = true`) lends one. Optional: no GPU is a warning, the container
/// runs without.
pub(super) fn gpu_decision(
    need: Option<GpuNeed>,
    support: impl FnOnce() -> Result<GpuSupport>,
    allowed: bool,
) -> Result<(Vec<String>, Vec<String>)> {
    let Some(need) = need else { return Ok((Vec::new(), Vec::new())) };
    let why = if !allowed {
        Refusal::new("GPUs", "gpus = true").to_string()
    } else {
        match support() {
            Ok(GpuSupport::None(reason)) => reason,
            Ok(usable) => return Ok((usable.run_args(), Vec::new())),
            Err(e) => format!("cannot tell whether the runtime has GPUs: {e:#}"),
        }
    };
    match need {
        GpuNeed::Required => bail!("`hostRequirements.gpu` asks for a GPU and none is usable: {why}; {OPTIONAL_HINT}"),
        GpuNeed::Optional => {
            let warning = format!("no GPU, running without one (`hostRequirements.gpu` is optional): {why}");
            Ok((Vec::new(), vec![warning]))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none() -> Result<GpuSupport> {
        Ok(GpuSupport::None("docker has no `nvidia` runtime".into()))
    }

    fn failed() -> Result<GpuSupport> {
        anyhow::bail!("docker info: connection refused")
    }

    #[test]
    fn no_need_asks_nothing() {
        let unasked = || -> Result<GpuSupport> { panic!("detection ran with no GPU asked for") };
        for allowed in [true, false] {
            assert_eq!(gpu_decision(None, unasked, allowed).unwrap(), (vec![], vec![]));
        }
    }

    #[test]
    fn usable_gives_the_runtimes_flags() {
        for need in [GpuNeed::Required, GpuNeed::Optional] {
            let (flags, warnings) = gpu_decision(Some(need), || Ok(GpuSupport::Gpus), true).unwrap();
            assert_eq!((flags, warnings), (vec!["--gpus".to_string(), "all".into()], vec![]));
            let (flags, _) = gpu_decision(Some(need), || Ok(GpuSupport::Cdi), true).unwrap();
            assert_eq!(flags, ["--device", "nvidia.com/gpu=all"]);
        }
    }

    #[test]
    fn required_refuses_naming_why() {
        let err = |support: fn() -> Result<GpuSupport>, allowed| {
            format!("{:#}", gpu_decision(Some(GpuNeed::Required), support, allowed).unwrap_err())
        };
        let e = err(none, true);
        assert!(e.contains("docker has no `nvidia` runtime") && e.contains("\"optional\""), "{e}");
        let e = err(failed, true);
        assert!(e.contains("connection refused") && e.contains("\"optional\""), "{e}");
        let e = err(|| panic!("detection ran under a refusing policy"), false);
        assert!(e.contains("worker policy") && e.contains("gpus = true") && e.contains("\"optional\""), "{e}");
    }

    #[test]
    fn optional_warns_once_and_runs_without() {
        for (support, allowed, want) in [
            (none as fn() -> Result<GpuSupport>, true, "docker has no `nvidia` runtime"),
            (failed, true, "connection refused"),
            (|| Ok(GpuSupport::Gpus), false, "gpus = true"),
        ] {
            let (flags, warnings) = gpu_decision(Some(GpuNeed::Optional), support, allowed).unwrap();
            assert!(flags.is_empty(), "{want}");
            assert_eq!(warnings.len(), 1, "{warnings:?}");
            assert!(warnings[0].contains(want), "{warnings:?}");
        }
    }
}
