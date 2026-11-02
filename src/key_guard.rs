//! The member key at rest (docs/architecture/daemon/fleet.md, *Member
//! identity*): no container devsandbox creates or image it builds may reach
//! `member.key` (nor its pending `member.key.next` or its `member.key.lock`).
//! Unlike the worker policy (`worker_policy`), this holds on every host,
//! for every config and trust level, and no allowance lifts it: a bind whose
//! source is one of those files or a dir above them (`$HOME`, the data dir,
//! `/`) is refused, and so is a build whose context, dockerfile,
//! `--build-context`, `--secret` or `--ssh` path covers them (the runtime
//! reads all of it).
//!
//! Checked over the final runtime argv right before it runs ([`check_argv`]),
//! like the worker policy's authority, and over a config's `mounts` before
//! their missing sources are created on the host ([`check_mount`]).
//! Cross-platform: the key's path is `daemon_config::member_key`'s, not
//! `serve`'s.

use std::fmt;
use std::path::{Component, Path, PathBuf};

use anyhow::Result;

use crate::worker_policy::{self, Arg};

/// A host path the runtime would hand a container or a build that covers a
/// member key file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exposure {
    /// What the path is (`bind mount of`, `build context`, …).
    pub what: &'static str,
    /// The path as the argv names it.
    pub source: String,
    /// The key file it covers.
    pub key: PathBuf,
}

impl fmt::Display for Exposure {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "refused: {} `{}` covers `{}`; this member's private key (fleet identity) must never be \
             reachable from a container (use a narrower path that doesn't contain it)",
            self.what,
            self.source,
            self.key.display()
        )
    }
}

impl std::error::Error for Exposure {}

/// The key files to keep out, each as written and as resolved (a symlinked
/// data dir is caught either way), and `$HOME` for a `~` source.
#[derive(Debug, Clone, Default)]
pub struct Guard {
    keys: Vec<(PathBuf, PathBuf)>,
    home: Option<PathBuf>,
}

impl Guard {
    /// The guard over the process env (`$XDG_DATA_HOME` / `$HOME`). With
    /// neither set there's no data dir, so no key there to guard.
    pub fn from_env() -> Guard {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        match crate::daemon_config::data_dir_from(std::env::var_os("XDG_DATA_HOME"), home.clone().map(Into::into)) {
            Ok(data) => Guard::for_data(&data, home),
            Err(_) => Guard { keys: Vec::new(), home },
        }
    }

    /// The guard over the data dir `data` (`<data>/devsandbox`).
    pub fn for_data(data: &Path, home: Option<PathBuf>) -> Guard {
        let key = crate::daemon_config::member_key(&lexical(data));
        let keys = crate::daemon_config::member_key_files(&key)
            .into_iter()
            .map(|k| {
                let resolved = worker_policy::resolve(&k).unwrap_or_else(|| k.clone());
                (k, resolved)
            })
            .collect();
        Guard { keys, home }
    }

    /// A host path `source` the runtime gets as `what`: refused when it is a
    /// key file or a dir above one, as written (made absolute, `..` dropped
    /// lexically, as the runtime cleans it) or resolved (symlinks).
    pub fn check_path(&self, source: &str, what: &'static str) -> Result<(), Exposure> {
        let expanded = match (source.strip_prefix('~'), &self.home) {
            (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => {
                home.join(rest.trim_start_matches('/'))
            }
            _ => PathBuf::from(source),
        };
        let written = lexical(&expanded);
        let forms = [Some(written.clone()), worker_policy::resolve(&written), worker_policy::resolve(&expanded)];
        for form in forms.iter().flatten() {
            for (key, resolved) in &self.keys {
                if key.starts_with(form) || resolved.starts_with(form) {
                    return Err(Exposure { what, source: source.to_string(), key: key.clone() });
                }
            }
        }
        Ok(())
    }

    /// A `--mount` value: a bind's source passes [`Self::check_path`]
    /// (volumes and tmpfs aren't host paths).
    pub fn check_mount(&self, value: &str) -> Result<(), Exposure> {
        let fields = worker_policy::mount_fields(value);
        match (fields.kind, fields.source) {
            ("bind", Some(source)) => self.check_path(source, "bind mount of"),
            _ => Ok(()),
        }
    }

    /// The runtime argv `argv` (`run …` or `build …`).
    pub fn check_argv(&self, argv: &[String]) -> Result<(), Exposure> {
        match argv.first().map(String::as_str) {
            Some("build") => self.check_build(argv),
            _ => self.check_run(argv),
        }
    }

    /// A `run` argv's binds (`-v`, `--volume`, `--mount`), up to the image.
    /// devsandbox writes every flag of it, so `worker_policy::args` knows
    /// which take a value; a malformed tail is left to the runtime.
    fn check_run(&self, argv: &[String]) -> Result<(), Exposure> {
        for arg in worker_policy::args(argv, worker_policy::RUN_SWITCHES, "runtime") {
            match arg {
                Ok(Arg::Flag("-v" | "--volume", Some(value))) => {
                    if let Some(source) = worker_policy::volume_source(value) {
                        self.check_path(source, "bind mount of")?;
                    }
                }
                Ok(Arg::Flag("--mount", Some(value))) => self.check_mount(value)?,
                Ok(Arg::Flag(..)) => {}
                Ok(Arg::Positional(_)) | Err(_) => break,
            }
        }
        Ok(())
    }

    /// A `build` argv's paths: the context (last: `services::build_args`
    /// puts it there), `-f`, `--build-context`, `--secret`'s `src`, `--ssh`'s
    /// files. `build.options` are free-form, so a flag's arity isn't known:
    /// each path flag is found by itself, not by walking the argv.
    fn check_build(&self, argv: &[String]) -> Result<(), Exposure> {
        let rest = argv.get(1..).unwrap_or_default();
        for (i, arg) in rest.iter().enumerate() {
            let (flag, inline) = worker_policy::split_flag(arg);
            let value = || inline.or_else(|| rest.get(i + 1).map(String::as_str));
            match flag {
                "-f" | "--file" => {
                    if let Some(file) = value().filter(|f| *f != "-") {
                        self.check_path(file, "build file")?;
                    }
                }
                "--build-context" => {
                    let source = value().and_then(|v| v.split_once('=')).map(|(_, s)| s).unwrap_or_default();
                    let source = source.strip_prefix("oci-layout://").unwrap_or(source);
                    if !source.is_empty() && !source.contains("://") {
                        self.check_path(source, "build context")?;
                    }
                }
                "--secret" => {
                    for part in value().unwrap_or_default().split(',') {
                        if let Some(("src" | "source", src)) = part.split_once('=') {
                            self.check_path(src, "build secret")?;
                        }
                    }
                }
                "--ssh" => {
                    if let Some((_, paths)) = value().and_then(|v| v.split_once('=')) {
                        for path in paths.split(',') {
                            self.check_path(path, "build ssh key")?;
                        }
                    }
                }
                _ => {}
            }
        }
        match rest.last().map(String::as_str) {
            Some(context) if context != "-" && !context.starts_with('-') && !context.contains("://") => {
                self.check_path(context, "build context")
            }
            _ => Ok(()),
        }
    }
}

/// [`Guard::check_argv`] over the process env, before `argv` runs.
pub fn check_argv(argv: &[String]) -> Result<()> {
    Guard::from_env().check_argv(argv)?;
    Ok(())
}

/// [`Guard::check_mount`] over the process env.
pub fn check_mount(value: &str) -> Result<()> {
    Guard::from_env().check_mount(value)?;
    Ok(())
}

/// `path` made absolute with `.` and `..` dropped lexically (as a runtime
/// cleans a bind source), no filesystem access.
fn lexical(path: &Path) -> PathBuf {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temp base with `home/.local/share/devsandbox` (the data dir) and a
    /// sibling `home/work`. Returns (base, home, data).
    fn fixture(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("dsv-keyguard-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let home = base.join("home");
        let data = home.join(".local/share/devsandbox");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(home.join("work/repo")).unwrap();
        std::fs::write(data.join("member.key"), "x").unwrap();
        let base = base.canonicalize().unwrap();
        (base.clone(), base.join("home"), base.join("home/.local/share/devsandbox"))
    }

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    fn run(extra: &[&str]) -> Vec<String> {
        let mut v = argv(&["run", "-d", "--name", "devsandbox-x", "--init", "-w", "/w"]);
        v.extend(extra.iter().map(|s| s.to_string()));
        v.extend(argv(&["alpine", "sh", "-c", "-v", "/:/r"]));
        v
    }

    #[test]
    fn binds_covering_the_key_are_refused() {
        let (base, home, data) = fixture("binds");
        let g = Guard::for_data(&data, Some(home.clone()));
        let s = |p: &Path| p.display().to_string();
        let key = data.join("member.key");
        let covering = [
            s(&key),
            s(&data.join("member.key.next")),
            s(&data.join("member.key.lock")),
            s(&data),
            s(&home.join(".local/share")),
            s(&home),
            "/".to_string(),
            "~".to_string(),
            "~/.local".to_string(),
            format!("{}/work/../.local", s(&home)),
            format!("{}/gone/..", s(&data)),
        ];
        for source in &covering {
            for extra in [
                vec!["-v".to_string(), format!("{source}:/h")],
                vec!["--volume".to_string(), format!("{source}:/h:ro")],
                vec![format!("--volume={source}:/h")],
                vec!["--mount".to_string(), format!("type=bind,source={source},target=/h,readonly")],
                vec!["--mount".to_string(), format!("src={source},dst=/h")],
            ] {
                let extra: Vec<&str> = extra.iter().map(String::as_str).collect();
                let err = g.check_argv(&run(&extra)).unwrap_err();
                assert_eq!(err.source, *source, "{extra:?}");
                assert!(err.to_string().contains("private key (fleet identity)"), "{err}");
            }
        }
        // A symlink to the data dir (or above it) is caught where it points.
        let link = base.join("link");
        std::os::unix::fs::symlink(&data, &link).unwrap();
        let up = base.join("up");
        std::os::unix::fs::symlink(&home, &up).unwrap();
        for source in [s(&link), s(&up), format!("{}/.local", s(&up))] {
            let err = g.check_argv(&run(&["-v", &format!("{source}:/d")])).unwrap_err();
            assert!(err.key.ends_with("member.key"), "{source}: {err}");
        }
        // The data dir itself a symlink: the bind of its target is caught too.
        let real = base.join("real-data");
        std::fs::create_dir_all(&real).unwrap();
        let linked = base.join("linked/devsandbox");
        std::fs::create_dir_all(linked.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&real, &linked).unwrap();
        let g2 = Guard::for_data(&linked, None);
        assert!(g2.check_argv(&run(&["-v", &format!("{}:/d", s(&real))])).is_err());
        assert!(g2.check_argv(&run(&["-v", &format!("{}:/d", s(&base.join("linked")))])).is_err());
        // Narrower or unrelated paths, volumes, tmpfs, the command's args.
        for extra in [
            vec!["-v".to_string(), format!("{}:/w", s(&home.join("work/repo")))],
            vec!["-v".to_string(), format!("{}:/w", s(&home.join("work")))],
            vec!["-v".to_string(), format!("{}:/f", s(&data.join("fleet/peer")))],
            vec!["-v".to_string(), format!("{}:/s", s(&data.join("seccomp-1.json")))],
            vec!["-v".to_string(), format!("{}:/k", s(&data.join("member.key.bak")))],
            vec!["-v".to_string(), "named:/data".to_string()],
            vec!["--mount".to_string(), "type=volume,source=v,target=/v".to_string()],
            vec!["--mount".to_string(), "type=tmpfs,target=/t".to_string()],
            vec!["-e".to_string(), "-v=/:/x".to_string(), "--label".to_string(), "/".to_string()],
        ] {
            let extra: Vec<&str> = extra.iter().map(String::as_str).collect();
            assert_eq!(g.check_argv(&run(&extra)), Ok(()), "{extra:?}");
        }
    }

    #[test]
    fn builds_covering_the_key_are_refused() {
        let (_, home, data) = fixture("build");
        let g = Guard::for_data(&data, Some(home.clone()));
        let h = home.display().to_string();
        let repo = home.join("work/repo").display().to_string();
        let df = format!("{repo}/Dockerfile");
        let ok = argv(&["build", "-t", "t", "-f", &df, "--build-arg", "A=1", "--squash", &repo]);
        assert_eq!(g.check_argv(&ok), Ok(()));
        let ctx_ok = argv(&["build", "-t", "t", "--build-context", "img=docker-image://alpine", "-f", &df, &repo]);
        assert_eq!(g.check_argv(&ctx_ok), Ok(()));
        let key = data.join("member.key").display().to_string();
        for (bad, what) in [
            (argv(&["build", "-t", "t", "-f", &df, &h]), "build context"),
            (argv(&["build", "-t", "t", "-f", &df, "--no-cache", "/"]), "build context"),
            (argv(&["build", "-t", "t", "-f", &key, &repo]), "build file"),
            (argv(&["build", "-t", "t", "--squash", "--build-context", &format!("x={h}"), "-f", &df, &repo]), "build context"),
            (argv(&["build", "-t", "t", &format!("--build-context=x=oci-layout://{h}"), &repo]), "build context"),
            (argv(&["build", "-t", "t", "--secret", &format!("id=k,src={key}"), &repo]), "build secret"),
            (argv(&["build", "-t", "t", "--ssh", &format!("default={key}"), &repo]), "build ssh key"),
        ] {
            let err = g.check_argv(&bad).unwrap_err();
            assert_eq!(err.what, what, "{bad:?}");
        }
    }

    /// The guard reads no fleet root and no `member.toml`: a plain local
    /// config's dir and `trust = "full"` change nothing.
    #[test]
    fn it_holds_without_a_fleet_root_and_under_full() {
        let (_, home, data) = fixture("full");
        std::fs::write(data.join("member.toml"), "trust = \"full\"\n[allow]\nbinds = [\"/\"]\nruntime-socket = true\n").unwrap();
        let cfg = home.join("work");
        assert!(worker_policy::Policy::detect(&data, &cfg, Vec::new()).unwrap().is_none());
        let g = Guard::for_data(&data, Some(home.clone()));
        assert!(g.check_argv(&run(&["-v", &format!("{}:/h", home.display())])).is_err());
        assert!(g.check_mount(&format!("type=bind,source={},target=/h", data.display())).is_err());
        // No data dir (neither env var): nothing to guard.
        assert_eq!(Guard::default().check_argv(&run(&["-v", "/:/r"])), Ok(()));
    }
}
