//! `lend.toml`, next to `state.toml`: what this host lends to a fleet hub
//! (docs/architecture/daemon/fleet.md, *Worker side*), and the fleet root
//! that lending reaches (`<data>/devsandbox/fleet/<hub>/`).
//!
//! The worker owns it: the hub never writes it. Only the daemon does, through
//! `lend.set` (`fleet::lender`; `devsandbox lend` / `fleet` are its
//! clients), with [`update`]: under the sibling `lend.lock`, a temp file and
//! a rename (`serve::locked_file`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::serve::machines::{check_name, ssh};
use crate::worker_policy::Allow;

/// Whether this host lends.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Offer to the hub and take new work.
    On,
    /// No link.
    #[default]
    Off,
    /// Offer to the hub, take no new work (recorded and offered; placement,
    /// fleet plan step 3, will honor it).
    Drain,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::On => "on",
            State::Off => "off",
            State::Drain => "drain",
        }
    }
}

/// The file. Unknown keys are ignored, as in `daemon.config.toml`: an
/// allowance a newer binary knows is one an older one doesn't grant.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Lend {
    pub state: State,
    /// The hub's ssh target (`me@hub`, `ssh://me@hub:2200`, an alias).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub hub: String,
    /// The name this host offers itself under; empty is [`default_name`]
    /// of the hostname.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// The worker policy's allowances (`worker_policy`, which reads them
    /// itself from this file).
    pub allow: Allow,
    /// Soft caps, reported in this host's capacity
    /// (`serve::capacity::Capacity::caps`); never enforced.
    #[serde(skip_serializing_if = "SoftCaps::is_empty")]
    pub caps: SoftCaps,
}

/// `[caps]`: what this host means to lend (fleet plan, *Placement*). All
/// optional; sizes as `16G`, `512M`, `1.5T` (binary units) or bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SoftCaps {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instances: Option<u64>,
}

impl SoftCaps {
    pub fn is_empty(&self) -> bool {
        *self == SoftCaps::default()
    }

    /// `mem` and `disk` must be sizes.
    pub fn check(&self) -> Result<()> {
        for (key, size) in [("mem", &self.mem), ("disk", &self.disk)] {
            if let Some(size) = size {
                crate::serve::capacity::parse_size(size).with_context(|| format!("`caps.{key}`"))?;
            }
        }
        Ok(())
    }
}

impl Lend {
    /// Whether a link is held: `on` and `drain`.
    pub fn lends(&self) -> bool {
        self.state != State::Off
    }
}

/// [`path_from`] over the process env.
pub fn path() -> Result<PathBuf> {
    path_from(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))
}

/// `<data>/devsandbox/lend.toml`.
pub fn path_from(xdg_data_home: Option<OsString>, home: Option<OsString>) -> Result<PathBuf> {
    Ok(crate::daemon_config::data_dir_from(xdg_data_home, home)?.join("lend.toml"))
}

/// `text` as a [`Lend`], [`check`]ed.
pub fn parse(text: &str) -> Result<Lend> {
    let lend: Lend = toml::from_str(text)?;
    check(&lend)?;
    Ok(lend)
}

/// Lending (`on`/`drain`) needs a valid `hub` target; a `hub` that's set
/// must be one anyway, and a `name` that's set a valid machine name.
pub fn check(lend: &Lend) -> Result<()> {
    if lend.lends() && lend.hub.is_empty() {
        bail!("`hub`: no hub to lend to");
    }
    if !lend.hub.is_empty() {
        ssh::parse_target(&lend.hub).context("`hub`")?;
        fleet_dir_name(&lend.hub)?;
    }
    if !lend.name.is_empty() {
        check_name(&lend.name).context("`name`")?;
    }
    lend.caps.check()?;
    lend.allow.check()
}

pub fn serialize(lend: &Lend) -> Result<String> {
    toml::to_string_pretty(lend).context("cannot serialize lend.toml")
}

/// Read-modify-write of the file at `path` under its lock (a missing file is
/// the default, `off`); saved only when `f` changed it. A file that doesn't
/// parse fails the update rather than being overwritten.
pub fn update<T>(path: &Path, f: impl FnOnce(&mut Lend) -> T) -> Result<T> {
    crate::serve::locked_file::update(path, |p| Ok(load(p)?.unwrap_or_default()), serialize, f)
}

/// The file at `path`; a missing file is `None` (not lending).
pub fn load(path: &Path) -> Result<Option<Lend>> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text).map(Some).with_context(|| format!("invalid {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// The name a worker offers itself under without `name`: the hostname's
/// first label (`lap.local` → `lap`), if it's a valid machine name.
pub fn default_name(hostname: &str) -> Result<String> {
    let label = hostname.split('.').next().unwrap_or_default();
    match check_name(label) {
        Ok(()) => Ok(label.to_string()),
        Err(e) => bail!("cannot name this worker after hostname `{hostname}` ({e:#}); set `name` in lend.toml"),
    }
}

/// The longest fleet root dir name: well under a file name's 255 bytes.
const MAX_DIR_NAME: usize = 128;

/// The fleet root's dir name for hub target `hub`: its host (`user@host`)
/// with `_<port>` for a URL's port; every byte other than ASCII
/// alphanumerics, `.`, `-`, `_` and `@` becomes `_`, and a leading `.` too,
/// so the name is one plain path component (never `.`, `..` or hidden).
pub fn fleet_dir_name(hub: &str) -> Result<String> {
    let target = ssh::parse_target(hub)?;
    let mut raw = target.host;
    if let Some(port) = target.port {
        raw.push_str(&format!("_{port}"));
    }
    let mut name: String =
        raw.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '@') { c } else { '_' }).collect();
    if name.starts_with('.') {
        name.replace_range(..1, "_");
    }
    if name.len() > MAX_DIR_NAME {
        bail!("hub target `{hub}` is too long for a directory name");
    }
    Ok(name)
}

/// `<data>/fleet/<fleet_dir_name(hub)>`, `data` being `lend.toml`'s dir.
pub fn fleet_root(data: &Path, hub: &str) -> Result<PathBuf> {
    Ok(data.join("fleet").join(fleet_dir_name(hub)?))
}

/// Write `lend` at `path` (tests: what an earlier `lend.set` left).
#[cfg(test)]
pub fn write_for_test(path: &Path, lend: &Lend) {
    update(path, |l| *l = lend.clone()).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_path_is_next_to_state_toml() {
        assert_eq!(path_from(Some("/xdg".into()), None).unwrap(), PathBuf::from("/xdg/devsandbox/lend.toml"));
        assert_eq!(
            path_from(None, Some("/home/u".into())).unwrap(),
            PathBuf::from("/home/u/.local/share/devsandbox/lend.toml")
        );
    }

    #[test]
    fn parses() {
        let l = parse("state = \"on\"\nhub = \"me@hub\"\nname = \"lap\"\n[allow]\n").unwrap();
        assert_eq!(l, Lend { state: State::On, hub: "me@hub".into(), name: "lap".into(), ..Default::default() });
        let a = parse("[allow]\nprivileged = true\nbinds = [\"/srv/x\"]\nlater = true\n").unwrap().allow;
        assert_eq!(a, Allow { privileged: true, binds: vec!["/srv/x".into()], ..Default::default() });
        assert!(parse("[allow]\nbinds = [\"x\"]\n").is_err(), "binds are absolute");
        // Unset allowances stay out of the file.
        assert_eq!(serialize(&Lend { allow: a, ..Default::default() }).unwrap(), "state = \"off\"\n\n[allow]\nprivileged = true\nbinds = [\"/srv/x\"]\n");
        assert!(l.lends());
        let drain = parse("state = \"drain\"\nhub = \"hub\"\n").unwrap();
        assert!(drain.lends() && drain.name.is_empty());
        assert_eq!(parse("").unwrap(), Lend::default(), "empty: off");
        assert!(!parse("").unwrap().lends());
        assert!(parse("state = \"off\"\nlater = 1\n").is_ok(), "unknown keys are ignored");
        // Off needs no hub; lending does, and a good one.
        assert!(parse("state = \"on\"\n").is_err());
        assert!(parse("state = \"on\"\nhub = \"-oProxyCommand=x\"\n").is_err());
        assert!(parse("state = \"on\"\nhub = \"h\"\nname = \"a/b\"\n").is_err());
        assert!(parse("state = \"maybe\"\n").is_err());
        // Soft caps: sizes checked, unset ones left out of the file.
        let caps = parse("[caps]\ncpus = 4\nmem = \"16G\"\ninstances = 3\n").unwrap().caps;
        assert_eq!(caps, SoftCaps { cpus: Some(4), mem: Some("16G".into()), disk: None, instances: Some(3) });
        assert_eq!(serialize(&Lend { caps, ..Default::default() }).unwrap(), "state = \"off\"\n\n[allow]\n\n[caps]\ncpus = 4\nmem = \"16G\"\ninstances = 3\n");
        assert!(!serialize(&Lend::default()).unwrap().contains("caps"));
        let bad = parse("[caps]\ndisk = \"lots\"\n").unwrap_err();
        assert!(format!("{bad:#}").contains("caps.disk"), "{bad:#}");
        assert!(parse("[caps]\ncpus = -1\n").is_err());
        // A hub kept while off must still be a target.
        assert!(parse("state = \"off\"\nhub = \"hub\"\n").is_ok());
        assert!(parse("state = \"off\"\nhub = \"-oProxyCommand=x\"\n").is_err());
    }

    #[test]
    fn writes_and_loads() {
        let dir = std::env::temp_dir().join(format!("dsv-lend-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("lend.toml");
        assert_eq!(load(&path).unwrap(), None, "missing: not lending");
        let lend = Lend { state: State::Drain, hub: "ssh://me@hub:2200".into(), ..Default::default() };
        write_for_test(&path, &lend);
        assert_eq!(load(&path).unwrap(), Some(lend));
        assert!(dir.join("lend.lock").exists(), "written through locked_file");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("name"), "an unset name is left out: {text}");
        // A no-op doesn't write.
        let off = dir.join("sub/lend.toml");
        update(&off, |l| l.state = State::Off).unwrap();
        assert!(!off.exists());
        std::fs::write(&path, "state = 1\n").unwrap();
        assert!(format!("{:#}", load(&path).unwrap_err()).contains("invalid"));
        assert!(update(&path, |l| l.state = State::Off).is_err(), "a bad file isn't overwritten");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "state = 1\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_names_take_the_first_label() {
        assert_eq!(default_name("lap.local").unwrap(), "lap");
        assert_eq!(default_name("box").unwrap(), "box");
        assert!(default_name("").is_err());
        assert!(default_name("a b").is_err());
    }

    #[test]
    fn fleet_dir_names_are_one_plain_component() {
        let cases = [
            ("me@hub", "me@hub"),
            ("hub.example.com", "hub.example.com"),
            ("ssh://me@hub:2200", "me@hub_2200"),
            ("ssh://[::1]:22", "__1_22"),
            ("..", "_."),
            (".hidden", "_hidden"),
            ("a/b", "a_b"),
            ("ü@x", "_@x"),
        ];
        for (hub, want) in cases {
            assert_eq!(fleet_dir_name(hub).unwrap(), want, "{hub}");
        }
        assert!(fleet_dir_name("").is_err());
        assert!(fleet_dir_name(&"h".repeat(MAX_DIR_NAME + 1)).is_err());
        assert_eq!(fleet_root(Path::new("/d/devsandbox"), "me@hub").unwrap(), PathBuf::from("/d/devsandbox/fleet/me@hub"));
    }
}
