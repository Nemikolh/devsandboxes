//! The run's GPU flags from `hostRequirements.gpu`, a sandbox's or a
//! service's (docs/architecture/instances/lifecycle.md, *GPUs*;
//! docs/architecture/instances/services.md).

use anyhow::{bail, Result};

use crate::config::GpuNeed;
use crate::runtime::gpu::GpuSupport;
use crate::worker_policy::Refusal;

const OPTIONAL_HINT: &str = "set `hostRequirements.gpu = \"optional\"` to run without one";

/// The `run` flags for a sandbox's GPU `need` (or service `service`'s,
/// named in the messages), plus the warnings to print; an error when a
/// required GPU can't be had. `support` is the runtime's answer
/// (`Backend::gpu_support`), asked only when a GPU is wanted and the
/// worker policy (`allowed`: none applies, or it has `gpus = true`) lends
/// one. Optional: no GPU is a warning, the container runs without.
pub(crate) fn gpu_decision(
    need: Option<GpuNeed>,
    support: impl FnOnce() -> Result<GpuSupport>,
    allowed: bool,
    service: Option<&str>,
) -> Result<(Vec<String>, Vec<String>)> {
    let Some(need) = need else { return Ok((Vec::new(), Vec::new())) };
    let owner = service.map(|s| format!("service `{s}`: ")).unwrap_or_default();
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
        GpuNeed::Required => {
            bail!("{owner}`hostRequirements.gpu` asks for a GPU and none is usable: {why}; {OPTIONAL_HINT}")
        }
        GpuNeed::Optional => {
            let warning = format!("{owner}no GPU, running without one (`hostRequirements.gpu` is optional): {why}");
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
            assert_eq!(gpu_decision(None, unasked, allowed, None).unwrap(), (vec![], vec![]));
        }
    }

    #[test]
    fn usable_gives_the_runtimes_flags() {
        for need in [GpuNeed::Required, GpuNeed::Optional] {
            let (flags, warnings) = gpu_decision(Some(need), || Ok(GpuSupport::Gpus), true, None).unwrap();
            assert_eq!((flags, warnings), (vec!["--gpus".to_string(), "all".into()], vec![]));
            let (flags, _) = gpu_decision(Some(need), || Ok(GpuSupport::Cdi), true, Some("db")).unwrap();
            assert_eq!(flags, ["--device", "nvidia.com/gpu=all"]);
        }
    }

    #[test]
    fn required_refuses_naming_why() {
        let err = |support: fn() -> Result<GpuSupport>, allowed| {
            format!("{:#}", gpu_decision(Some(GpuNeed::Required), support, allowed, None).unwrap_err())
        };
        let e = err(none, true);
        assert!(e.contains("docker has no `nvidia` runtime") && e.contains("\"optional\""), "{e}");
        let e = err(failed, true);
        assert!(e.contains("connection refused") && e.contains("\"optional\""), "{e}");
        let e = err(|| panic!("detection ran under a refusing policy"), false);
        assert!(e.contains("worker policy") && e.contains("gpus = true") && e.contains("\"optional\""), "{e}");
        assert!(!e.contains("service"), "{e}");
    }

    #[test]
    fn a_service_is_named() {
        let e = gpu_decision(Some(GpuNeed::Required), none, true, Some("db")).unwrap_err().to_string();
        assert!(e.starts_with("service `db`: `hostRequirements.gpu`") && e.contains("\"optional\""), "{e}");
        let (flags, warnings) = gpu_decision(Some(GpuNeed::Optional), none, true, Some("db")).unwrap();
        assert!(flags.is_empty());
        assert!(warnings.len() == 1 && warnings[0].starts_with("service `db`: no GPU"), "{warnings:?}");
    }

    #[test]
    fn optional_warns_once_and_runs_without() {
        for (support, allowed, want) in [
            (none as fn() -> Result<GpuSupport>, true, "docker has no `nvidia` runtime"),
            (failed, true, "connection refused"),
            (|| Ok(GpuSupport::Gpus), false, "gpus = true"),
        ] {
            let (flags, warnings) = gpu_decision(Some(GpuNeed::Optional), support, allowed, None).unwrap();
            assert!(flags.is_empty(), "{want}");
            assert_eq!(warnings.len(), 1, "{warnings:?}");
            assert!(warnings[0].contains(want), "{warnings:?}");
        }
    }
}
