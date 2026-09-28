//! Embedded devsbd helper binaries (see docs/sandbox-helper.md and build.rs).
//! Both Linux arches are always embedded: the non-host one is the fallback
//! for emulated images. An empty blob means the helper wasn't built, and
//! callers fall back to the bind-mount path.
//!
//! Also installs the helper into containers (`ensure`): written through
//! `exec -i` stdin, which works on every runtime (no `docker cp`, no file
//! binds on Apple `container`).

#[cfg(unix)]
pub mod bridge;
// Shared with the helper; the parser is helper-only (the host only writes).
#[allow(dead_code)]
pub mod bootfile;
// The forwarder engine (step 5); the CLI (`devsandbox port`) drives its full
// public surface, the TUI Ports tab reuses it (docs/port-forwarding.md).
#[cfg(unix)]
pub mod forward;
#[cfg(unix)]
mod mux;
// Partly helper-only (e.g. `Frame::encode` callers on the daemon side).
#[allow(dead_code)]
pub mod proto;

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::Read;

use serde::{Deserialize, Serialize};

use crate::config::LifecycleCommand;
use crate::runtime::backend;
use crate::state::{Instance, State};

/// Serialized as `x86_64` / `aarch64` in state.toml.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Arch {
    X86_64,
    Aarch64,
}

impl Arch {
    /// Arch a local runtime runs natively (Windows x86_64 → linux x86_64,
    /// macOS arm64 → linux aarch64).
    pub fn host() -> Arch {
        if cfg!(target_arch = "aarch64") { Arch::Aarch64 } else { Arch::X86_64 }
    }

    pub fn other(self) -> Arch {
        match self {
            Arch::X86_64 => Arch::Aarch64,
            Arch::Aarch64 => Arch::X86_64,
        }
    }
}

const X86_64_ZST: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/devsbd-x86_64.zst"));
const AARCH64_ZST: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/devsbd-aarch64.zst"));
const X86_64_HASH: &str = include_str!(concat!(env!("OUT_DIR"), "/devsbd-x86_64.sha256"));
const AARCH64_HASH: &str = include_str!(concat!(env!("OUT_DIR"), "/devsbd-aarch64.sha256"));

fn compressed(arch: Arch) -> &'static [u8] {
    match arch {
        Arch::X86_64 => X86_64_ZST,
        Arch::Aarch64 => AARCH64_ZST,
    }
}

/// Decompressed helper for `arch`, or `None` when it wasn't embedded.
pub fn blob(arch: Arch) -> Option<Cow<'static, [u8]>> {
    decompress(compressed(arch)).map(Cow::Owned)
}

/// sha256 (hex) of the uncompressed helper, or `None` when not embedded.
pub fn hash(arch: Arch) -> Option<&'static str> {
    let h = match arch {
        Arch::X86_64 => X86_64_HASH,
        Arch::Aarch64 => AARCH64_HASH,
    };
    (!h.is_empty()).then_some(h)
}

/// Whether this build ships a helper for any arch. False for `cargo install`
/// builds (no `DEVSANDBOX_DEVSBD_DIR`): they keep the ssh-agent bind mount.
pub fn embedded() -> bool {
    hash(Arch::X86_64).is_some() || hash(Arch::Aarch64).is_some()
}

/// Whether `info`'s ssh-agent uses the relay (not the bind mount): the helper
/// runs in its container and no mount occupies the daemon's socket path. Not
/// cfg-gated so `exec_argv` can call it on every platform; the relay is
/// unix-only, so `false` off unix (`bridge` compiles only there). An instance
/// created with a mount keeps mount mode until recreated (`ssh_auth_sock` is
/// `Some`).
pub fn relay_mode(info: &Instance) -> bool {
    cfg!(unix) && info.devsbd_arch.is_some() && info.ssh_auth_sock.is_none()
}

fn decompress(zst: &[u8]) -> Option<Vec<u8>> {
    if zst.is_empty() {
        return None;
    }
    let mut src = zst;
    let mut dec = ruzstd::decoding::StreamingDecoder::new(&mut src)
        .expect("embedded devsbd blob is valid zstd (written by build.rs)");
    let mut out = Vec::new();
    dec.read_to_end(&mut out).expect("decompress embedded devsbd");
    Some(out)
}

/// Install path inside the container. On docker `/run` is in the writable
/// layer, so the binary survives a restart (the daemon doesn't — the bridge
/// self-heals that); the reinstall on every `start` is for CLI upgrades and
/// images that mount a tmpfs at `/run`.
pub const BIN: &str = "/run/devsandbox/bin/devsbd";

/// Write stdin to a temp file and rename, so a concurrent `devsbd version`
/// never sees a half-written binary. The `/usr/local/bin` symlink puts
/// `devsbd` on `PATH` for scripts; best-effort (the dir may be missing or
/// read-only), so it can't turn a good install into a failure.
const INSTALL_SCRIPT: &str = "mkdir -p /run/devsandbox/bin \
    && cat > /run/devsandbox/bin/devsbd.tmp \
    && chmod 755 /run/devsandbox/bin/devsbd.tmp \
    && mv /run/devsandbox/bin/devsbd.tmp /run/devsandbox/bin/devsbd \
    && { ln -sf /run/devsandbox/bin/devsbd /usr/local/bin/devsbd 2>/dev/null || true; }";

/// Boot file write, atomic for the same reason as `INSTALL_SCRIPT`: a hook
/// firing mid-write must see the old file or the new one, never half.
const BOOT_WRITE_SCRIPT: &str = "mkdir -p /run/devsandbox \
    && cat > /run/devsandbox/boot.tmp \
    && mv /run/devsandbox/boot.tmp /run/devsandbox/boot";

/// What `devsbd boot` should run on the container's next start: `cmd` (a
/// `postStartCommand`) with the context the host's lifecycle exec gives it
/// (`run::exec_lifecycle`: `-w workspace`, `-u remote_user`, `remote_env`,
/// then `SSH_AUTH_SOCK`). Empty argvs are dropped, as the exec path skips them.
pub fn boot_spec(
    cmd: &LifecycleCommand,
    workspace: &str,
    remote_env: Option<&BTreeMap<String, String>>,
    remote_user: Option<&str>,
    ssh_auth_sock: Option<&str>,
) -> bootfile::BootSpec {
    let mut env: Vec<(String, String)> =
        remote_env.into_iter().flatten().map(|(k, v)| (k.clone(), v.clone())).collect();
    if let Some(sock) = ssh_auth_sock {
        env.push(("SSH_AUTH_SOCK".into(), sock.into()));
    }
    bootfile::BootSpec {
        user: remote_user.map(str::to_string),
        cwd: Some(workspace.to_string()),
        env,
        cmds: cmd.commands().into_iter().filter(|argv| !argv.is_empty()).collect(),
    }
}

/// Write `spec` as `container`'s boot file, or remove the file when `None`.
/// Best-effort: removal is silent (no file is the common case); a failed write
/// gets a note, since the hook then won't rerun `postStartCommand` on restart.
///
/// Known gap: on `start` the runtime fires the hook before the host rewrites
/// the file, so that one start runs the previously recorded command. Changing
/// `postStartCommand` is config drift (rebuild) anyway.
pub fn sync_boot(container: &str, spec: Option<&bootfile::BootSpec>, quiet: bool) {
    match spec {
        Some(spec) => {
            let args = ["exec", "-i", "-u", "root", container, "sh", "-c", BOOT_WRITE_SCRIPT];
            let text = bootfile::serialize(spec);
            if let (Err(e), false) = (backend().run_with_stdin(&args, text.as_bytes()), quiet) {
                eprintln!("note: couldn't write the boot hook's postStartCommand in `{container}`: {e:#}");
            }
        }
        None => {
            let _ = backend()
                .output_quiet(&["exec", "-u", "root", container, "rm", "-f", bootfile::PATH]);
        }
    }
}

/// Build hash from `devsbd version` output (`devsbd <protocol> <hash>`). The
/// hash pins the exact build, protocol version included.
fn parse_version(out: &str) -> Option<&str> {
    let mut words = out.split_whitespace();
    match (words.next(), words.next(), words.next(), words.next()) {
        (Some("devsbd"), Some(_), Some(hash), None) => Some(hash),
        _ => None,
    }
}

/// Arch try order: the one recorded as working (so emulated images go
/// straight to the right blob), else the host's, then the other. No
/// `uname`/inspect probe: local runtimes run host-arch containers except
/// emulated images, which the blind second try covers. Real probing is
/// deferred to remote-container support (docs/sandbox-helper.md).
fn candidates(recorded: Option<Arch>) -> [Arch; 2] {
    let first = recorded.unwrap_or_else(Arch::host);
    [first, first.other()]
}

/// Hash reported by the helper installed in `container`; `None` when it is
/// missing or can't run (wrong arch, no binary, container down).
fn installed_hash(container: &str) -> Option<String> {
    let out = backend().output_quiet(&["exec", "-u", "root", container, BIN, "version"]).ok()?;
    parse_version(&out).map(str::to_string)
}

/// Make sure `container` runs this build's helper; returns the arch that works.
/// Skips the write when the installed helper already reports an embedded hash
/// (so a CLI upgrade rewrites it). Otherwise writes each candidate arch and
/// keeps the first whose `version` reports its hash — one blind retry rather
/// than matching `exec format error`, whose code/text differ per runtime. `Err`
/// is a one-line note: the helper is unavailable for this container.
pub fn install(container: &str, recorded: Option<Arch>) -> Result<Arch, String> {
    let order = candidates(recorded);
    if order.iter().all(|a| hash(*a).is_none()) {
        return Err("devsbd not embedded in this build (see scripts/build-devsbd.sh)".into());
    }
    if let Some(current) = installed_hash(container) {
        if let Some(arch) = order.into_iter().find(|a| hash(*a) == Some(current.as_str())) {
            return Ok(arch);
        }
    }
    for arch in order {
        let (Some(want), Some(bytes)) = (hash(arch), blob(arch)) else {
            continue;
        };
        let args = ["exec", "-i", "-u", "root", container, "sh", "-c", INSTALL_SCRIPT];
        if backend().run_with_stdin(&args, &bytes).is_ok()
            && installed_hash(container).as_deref() == Some(want)
        {
            return Ok(arch);
        }
    }
    Err(format!("devsbd couldn't run in `{container}`"))
}

/// Start `devsbd daemon` detached. Idempotent: a second daemon sees the
/// pidfile lock and exits 0. Failures are silent; bridges then fail their
/// handshake and the relay is simply off.
fn start_daemon(container: &str) {
    let _ = backend().output_quiet(&["exec", "-d", "-u", "root", container, BIN, "daemon"]);
}

/// `install` + `start_daemon`; `Err` carries the reason so the caller can word
/// its own note (relay-first `run` says "ssh-agent forwarding off"). Never
/// fails the command: the helper is optional.
pub fn ensure_or_reason(container: &str, recorded: Option<Arch>) -> Result<Arch, String> {
    let arch = install(container, recorded)?;
    start_daemon(container);
    Ok(arch)
}

/// `ensure_or_reason`, printing the plain note on failure unless `quiet` (the
/// TUI owns the screen). The common path; relay-first `run` calls
/// `ensure_or_reason` directly to reword the note.
pub fn ensure(container: &str, recorded: Option<Arch>, quiet: bool) -> Option<Arch> {
    match ensure_or_reason(container, recorded) {
        Ok(arch) => Some(arch),
        Err(note) => {
            if !quiet {
                eprintln!("note: {note}");
            }
            None
        }
    }
}

/// `ensure` for an existing instance, persisting `devsbd_arch` under `key`
/// when it changed. State is reloaded so the save can't clobber writes made
/// since the caller loaded it. Returns the resulting arch so callers can act
/// on the fresh value rather than the pre-ensure `info` (e.g. `start`'s relay
/// decision, whose `info` snapshot may carry a stale `devsbd_arch`).
pub fn ensure_recorded(key: &str, info: &Instance, quiet: bool) -> Option<Arch> {
    let arch = ensure(&info.container, info.devsbd_arch, quiet);
    if arch == info.devsbd_arch {
        return arch;
    }
    let saved = State::load().and_then(|mut state| match state.instances.get_mut(key) {
        Some(entry) => {
            entry.devsbd_arch = arch;
            state.save()
        }
        None => Ok(()),
    });
    if let (Err(e), false) = (saved, quiet) {
        eprintln!("note: couldn't record devsbd arch: {e:#}");
    }
    arch
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_blob_means_unavailable() {
        assert_eq!(decompress(&[]), None);
    }

    #[test]
    fn decompress_roundtrips_zstd() {
        let data = b"devsbd payload".repeat(100);
        let zst = zstd::encode_all(&data[..], 19).unwrap();
        assert_eq!(decompress(&zst).as_deref(), Some(&data[..]));
    }

    #[test]
    fn blob_and_hash_agree_on_availability() {
        for arch in [Arch::X86_64, Arch::Aarch64] {
            assert_eq!(blob(arch).is_some(), hash(arch).is_some(), "{arch:?}");
        }
    }

    #[test]
    fn embedded_blob_carries_its_hash() {
        // build.rs patches the hash into the helper's BUILD_HASH slot, which is
        // what `devsbd version` reports.
        for arch in [Arch::X86_64, Arch::Aarch64] {
            let (Some(b), Some(h)) = (blob(arch), hash(arch)) else { continue };
            assert_eq!(h.len(), 64);
            let slot = [&b"DEVSBD_BUILD_HASH:"[..], h.as_bytes()].concat();
            assert!(b.windows(slot.len()).any(|w| w == slot), "{arch:?}");
        }
    }

    #[test]
    fn parse_version_takes_the_hash() {
        assert_eq!(parse_version("devsbd 1 abc123\n"), Some("abc123"));
        assert_eq!(parse_version("devsbd 1"), None);
        assert_eq!(parse_version("devsbd 1 abc extra"), None);
        assert_eq!(parse_version("sh: devsbd: not found"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn candidates_prefer_recorded_then_host() {
        assert_eq!(candidates(None), [Arch::host(), Arch::host().other()]);
        assert_eq!(candidates(Some(Arch::Aarch64)), [Arch::Aarch64, Arch::X86_64]);
        assert_eq!(candidates(Some(Arch::X86_64)), [Arch::X86_64, Arch::Aarch64]);
    }

    #[test]
    fn arch_serializes_lowercase() {
        #[derive(Serialize, Deserialize)]
        struct W {
            a: Arch,
        }
        assert_eq!(toml::to_string(&W { a: Arch::X86_64 }).unwrap().trim(), "a = \"x86_64\"");
        let w: W = toml::from_str("a = \"aarch64\"").unwrap();
        assert_eq!(w.a, Arch::Aarch64);
    }

    /// Docker-gated, and skipped when no helper is embedded: installs into a
    /// throwaway alpine container, checks the hash-matching no-op, then the
    /// wrong-arch-first retry.
    #[test_utils::docker_test(helper)]
    fn installs_into_container_with_docker() -> Result<(), &'static str> {
        use std::process::Command;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!("devsandbox-devsbd-test-{stamp}");
        let up = Command::new("docker")
            .args(["run", "-d", "--rm", "--name", &name, "alpine:3.20", "sleep", "300"])
            .output()
            .unwrap();
        assert!(up.status.success(), "{}", String::from_utf8_lossy(&up.stderr));
        let inode = || {
            let out = Command::new("docker").args(["exec", &name, "stat", "-c", "%i", BIN]).output().unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        let cleanup = || {
            let _ = Command::new("docker").args(["rm", "-f", &name]).output();
        };
        crate::test_support::with_cleanup(cleanup, || {
            let arch = install(&name, None).unwrap();
            assert_eq!(installed_hash(&name).as_deref(), hash(arch));
            // Current helper: no rewrite (mv would change the inode).
            let before = inode();
            assert_eq!(install(&name, Some(arch.other())), Ok(arch));
            assert_eq!(inode(), before);
            // Missing helper, wrong arch recorded: blind retry lands on one that
            // runs (the other arch too, where qemu binfmt is registered).
            Command::new("docker").args(["exec", &name, "rm", BIN]).output().unwrap();
            let retried = install(&name, Some(Arch::host().other())).unwrap();
            assert_eq!(installed_hash(&name).as_deref(), hash(retried));
        });
        Ok(())
    }

    #[test]
    fn boot_spec_mirrors_the_lifecycle_exec() {
        #[derive(Deserialize)]
        struct W {
            c: LifecycleCommand,
        }
        let w: W = toml::from_str("c = { b = [\"make\", \"up\"], a = \"echo hi\", z = [] }").unwrap();
        let env = BTreeMap::from([("FOO".to_string(), "bar".to_string())]);
        let spec = boot_spec(&w.c, "/workspaces/repo", Some(&env), Some("vscode"), Some("/run/sock"));
        assert_eq!(
            spec,
            bootfile::BootSpec {
                user: Some("vscode".into()),
                cwd: Some("/workspaces/repo".into()),
                // remote_env first, then SSH_AUTH_SOCK (`lifecycle_argv`'s order).
                env: vec![("FOO".into(), "bar".into()), ("SSH_AUTH_SOCK".into(), "/run/sock".into())],
                // Map form in key order, the empty argv dropped.
                cmds: vec![
                    vec!["sh".into(), "-c".into(), "echo hi".into()],
                    vec!["make".into(), "up".into()],
                ],
            }
        );
        let bare = boot_spec(&w.c, "/w", None, None, None);
        assert_eq!((bare.user, bare.env), (None, vec![]));
        // What the host writes is what the helper reads.
        assert_eq!(bootfile::parse(&bootfile::serialize(&spec)), Ok(spec));
    }

    #[test]
    fn install_links_devsbd_onto_path_best_effort() {
        assert!(INSTALL_SCRIPT.ends_with(
            "&& { ln -sf /run/devsandbox/bin/devsbd /usr/local/bin/devsbd 2>/dev/null || true; }"
        ));
    }

    #[test]
    fn other_arch_flips() {
        assert_eq!(Arch::host().other().other(), Arch::host());
        assert_ne!(Arch::host().other(), Arch::host());
    }

    /// Runs the embedded host-arch helper directly and checks `version` reports
    /// `devsbd <proto::VERSION> <hash(host)>`. Catches a stale `target/devsbd`
    /// after a protocol change (the blob's built-in `VERSION` wouldn't match
    /// this crate's). Skipped when no helper is embedded (`cargo install`).
    #[cfg(target_os = "linux")]
    #[test_utils::helper_test]
    fn embedded_host_helper_reports_version() -> Result<(), &'static str> {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        let want_hash = hash(Arch::host()).expect("gated on Need::Helper");
        let bytes = blob(Arch::host()).expect("blob present when hash is");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("devsandbox-devsbd-test-{stamp}"));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&bytes).unwrap();
        f.flush().unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        drop(f);

        // Exec of a just-written file races other test threads' `fork()`: a
        // child forked while our write fd was open inherits it until its own
        // exec closes it (O_CLOEXEC), and meanwhile our exec fails ETXTBSY.
        // That window is brief, so a bounded retry is the standard fix.
        let mut tries = 0;
        let out = loop {
            match Command::new(&path).arg("version").output() {
                Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && tries < 100 => {
                    tries += 1;
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                r => break r.unwrap(),
            }
        };
        let _ = std::fs::remove_file(&path);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            format!("devsbd {} {want_hash}", proto::VERSION),
            "stale target/devsbd? rerun scripts/build-devsbd.sh",
        );
        Ok(())
    }
}
