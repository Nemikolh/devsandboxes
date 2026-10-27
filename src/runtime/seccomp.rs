//! The seccomp profile `run` hands docker (docs/architecture/instances/identity.md,
//! *Tool environment*): moby's default, vendored verbatim and pinned by file
//! name (`seccomp/moby-default-v0.2.4.json`, from `moby/profiles`
//! `seccomp/v0.2.4`, what Docker 29.9.1 vendors), plus [`RULES`], which let a
//! process without `CAP_SYS_ADMIN` create a user namespace, and a pid
//! namespace from inside one: what Chromium's sandbox needs. Regenerated
//! deliberately: a new moby version is a new file.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::config::short_hash;
use crate::state::State;

const MOBY: &str = include_str!("seccomp/moby-default-v0.2.4.json");

/// `clone`/`unshare` with `CLONE_NEWUSER` (0x10000000), and `clone` with
/// `CLONE_NEWPID` (0x20000000), both excluded when `CAP_SYS_ADMIN` is held
/// (moby's own rule allows everything then) and on s390, whose `clone` takes
/// the flags second: the shape of moby's own `clone` rule. The kernel still
/// wants `CAP_SYS_ADMIN` in the owning userns for `CLONE_NEWPID`, so the
/// second only works inside a new userns.
const RULES: [&str; 2] = [
    r#"{"names":["clone","unshare"],"action":"SCMP_ACT_ALLOW","args":[{"index":0,"value":268435456,"valueTwo":268435456,"op":"SCMP_CMP_MASKED_EQ"}],"excludes":{"caps":["CAP_SYS_ADMIN"],"arches":["s390","s390x"]}}"#,
    r#"{"names":["clone"],"action":"SCMP_ACT_ALLOW","args":[{"index":0,"value":536870912,"valueTwo":536870912,"op":"SCMP_CMP_MASKED_EQ"}],"excludes":{"caps":["CAP_SYS_ADMIN"],"arches":["s390","s390x"]}}"#,
];

/// The profile: moby's default with [`RULES`] appended to its `syscalls`.
pub fn profile() -> String {
    let mut profile: Value = serde_json::from_str(MOBY).expect("vendored seccomp profile is valid JSON");
    let syscalls = profile["syscalls"].as_array_mut().expect("vendored seccomp profile has `syscalls`");
    syscalls.extend(RULES.map(|r| serde_json::from_str::<Value>(r).expect("seccomp rule is valid JSON")));
    serde_json::to_string(&profile).expect("a JSON value serializes")
}

/// The profile's file next to `state.toml`, written when missing: the path
/// `--security-opt seccomp=` takes (the docker CLI reads it, so a remote
/// daemon needs nothing).
pub fn ensure_file() -> Result<PathBuf> {
    let state = State::path()?;
    ensure_file_in(state.parent().context("state path has no parent")?)
}

/// [`ensure_file`] in `dir`. Named by the profile's hash, so a changed
/// profile is a new file and an existing one is always current. Written to
/// a pid-named temp file and renamed, so a reader never sees half of one.
pub(crate) fn ensure_file_in(dir: &Path) -> Result<PathBuf> {
    let profile = profile();
    let path = dir.join(format!("seccomp-{}.json", short_hash(&profile)));
    if path.exists() {
        return Ok(path);
    }
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let tmp = dir.join(format!(".seccomp.json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, profile).with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn syscalls(json: &str) -> Vec<Value> {
        serde_json::from_str::<Value>(json).unwrap()["syscalls"].as_array().unwrap().clone()
    }

    #[test]
    fn keeps_every_moby_rule_and_appends_ours() {
        let moby = syscalls(MOBY);
        let ours = syscalls(&profile());
        assert_eq!(ours.len(), moby.len() + 2);
        assert_eq!(ours[..moby.len()], moby[..]);
        let full: Value = serde_json::from_str(&profile()).unwrap();
        let vendored: Value = serde_json::from_str(MOBY).unwrap();
        for key in ["defaultAction", "defaultErrnoRet", "archMap"] {
            assert_eq!(full[key], vendored[key], "{key}");
        }
    }

    #[test]
    fn rules_allow_newuser_and_newpid_without_sys_admin() {
        let ours = syscalls(&profile());
        let excludes = serde_json::json!({"caps": ["CAP_SYS_ADMIN"], "arches": ["s390", "s390x"]});
        let rule = |names: Value, flag: u64| {
            serde_json::json!({
                "names": names,
                "action": "SCMP_ACT_ALLOW",
                "args": [{"index": 0, "value": flag, "valueTwo": flag, "op": "SCMP_CMP_MASKED_EQ"}],
                "excludes": excludes,
            })
        };
        let n = ours.len();
        assert_eq!(ours[n - 2], rule(serde_json::json!(["clone", "unshare"]), 0x1000_0000));
        assert_eq!(ours[n - 1], rule(serde_json::json!(["clone"]), 0x2000_0000));
    }

    #[test]
    fn file_is_named_by_hash_and_written_once() {
        let dir = std::env::temp_dir().join(format!("devsandbox-seccomp-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = ensure_file_in(&dir).unwrap();
        assert_eq!(path, dir.join(format!("seccomp-{}.json", short_hash(&profile()))));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), profile());
        // An existing file is left alone.
        std::fs::write(&path, "kept").unwrap();
        assert_eq!(ensure_file_in(&dir).unwrap(), path);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "kept");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "no temp file left");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The point of the profile: an unprivileged user can create a user
    /// namespace under it, and not under docker's default.
    #[test_utils::docker_test(docker_only)]
    fn allows_unprivileged_userns_with_docker() -> Result<(), &'static str> {
        let image = "alpine:3.20";
        let pulled = crate::test_support::runtime_cmd().args(["pull", "-q", image]).output().map_err(|_| "cannot run docker")?;
        if !pulled.status.success() {
            return Err("cannot pull alpine:3.20");
        }
        let dir = std::env::temp_dir().join(format!("devsandbox-seccomp-docker-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        crate::test_support::with_cleanup(
            || drop(std::fs::remove_dir_all(&dir)),
            || {
                let path = ensure_file_in(&dir).map_err(|_| "cannot write the profile")?;
                let unshare = |opts: &[String]| {
                    let out = crate::test_support::runtime_cmd()
                        .args(["run", "--rm", "--user", "1000:1000"])
                        .args(opts)
                        .args([image, "unshare", "-Ur", "true"])
                        .output()
                        .unwrap();
                    (out.status.success(), String::from_utf8_lossy(&out.stderr).into_owned())
                };
                let (ok, stderr) = unshare(&[]);
                assert!(!ok, "docker's default profile allowed it: {stderr}");
                let (ok, stderr) = unshare(&["--security-opt".into(), format!("seccomp={}", path.display())]);
                assert!(ok, "ours didn't: {stderr}");
                Ok(())
            },
        )
    }
}
