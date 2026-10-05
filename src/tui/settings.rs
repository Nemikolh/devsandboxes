//! Dashboard settings (docs/tui-selection.md, step 6): per-user toggles
//! persisted at `<data>/devsandbox/dashboard.toml`, shown and flipped in the
//! `?` Settings & help modal. The modal's rows derive from [`SETTINGS`], so
//! a new setting is a field here plus one table entry.
//!
//! `App` only holds a [`Settings`] and marks it for saving; the event loop
//! does the [`load`] / [`save`] I/O, keeping the state machine I/O-free.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Every field defaults on its own (`#[serde(default)]` at the container
/// takes them from [`Default`]), so a file written by an older version, or
/// edited by hand down to one key, still loads; unknown keys (a newer
/// version's settings) are ignored rather than failing the whole file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Releasing a mouse selection copies it. On by default: most Linux
    /// terminals keep ctrl+shift+c for their own copy, and in the integrated
    /// terminal ctrl+c is the shell's, so the release is the one copy that
    /// always works. Off: only the copy keys do, so a stray drag never
    /// clobbers the clipboard.
    pub copy_on_select: bool,
    /// Relay integrated-terminal apps' OSC 52 copies to the outer clipboard.
    /// On, as real terminals do; an off switch since it lets an app in a
    /// container write the host clipboard.
    pub terminal_clipboard: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { copy_on_select: true, terminal_clipboard: true }
    }
}

/// One row of the settings section: data rather than code so the modal's
/// rows, toggling and hit-testing all come from this table.
pub struct SettingSpec {
    /// The TOML key, for reference next to the label.
    #[allow(dead_code)] // documents the row's file key; not rendered
    pub key: &'static str,
    pub label: &'static str,
    /// One line saying what "on" does, given whether the outer terminal
    /// keeps ctrl+shift+c (`App::copy_key_intercepted`), so a key it names
    /// reaches us.
    pub help: fn(bool) -> &'static str,
    pub get: fn(&Settings) -> bool,
    pub set: fn(&mut Settings, bool),
}

pub const SETTINGS: &[SettingSpec] = &[
    SettingSpec {
        key: "copy_on_select",
        label: "copy on select",
        help: |intercepted| match intercepted {
            false => "releasing a mouse selection copies it (else ctrl-shift-c / cmd-c)",
            true => "releasing a mouse selection copies it (else ctrl-c / cmd-c)",
        },
        get: |s| s.copy_on_select,
        set: |s, v| s.copy_on_select = v,
    },
    SettingSpec {
        key: "terminal_clipboard",
        label: "terminal clipboard",
        help: |_| "apps in the integrated terminal may set the clipboard (OSC 52)",
        get: |s| s.terminal_clipboard,
        set: |s, v| s.terminal_clipboard = v,
    },
];

impl Settings {
    /// Flip setting `i` of [`SETTINGS`]; no-op out of range.
    pub fn toggle(&mut self, i: usize) {
        if let Some(spec) = SETTINGS.get(i) {
            (spec.set)(self, !(spec.get)(self));
        }
    }
}

/// `<data>/devsandbox/dashboard.toml`, next to `state.toml`. Mirrors
/// `State::path`'s base-dir logic, like `prompt::history_path`.
pub fn path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .context("cannot determine user data dir ($XDG_DATA_HOME or $HOME)")?;
    Ok(base.join("devsandbox/dashboard.toml"))
}

/// The saved settings; a missing file is the defaults.
pub fn load() -> Result<Settings> {
    load_from(&path()?)
}

/// Write `settings` to [`path`], creating its directory.
pub fn save(settings: &Settings) -> Result<()> {
    save_to(&path()?, settings)
}

fn load_from(path: &Path) -> Result<Settings> {
    match std::fs::read_to_string(path) {
        Ok(contents) => parse(&contents).with_context(|| format!("invalid settings file {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

fn save_to(path: &Path, settings: &Settings) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    let contents = toml::to_string_pretty(settings).context("cannot serialize settings")?;
    std::fs::write(path, contents).with_context(|| format!("cannot write {}", path.display()))
}

fn parse(contents: &str) -> Result<Settings> {
    Ok(toml::from_str(contents)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let s = Settings::default();
        assert!(s.copy_on_select);
        assert!(s.terminal_clipboard);
        assert_eq!(parse("").unwrap(), s, "an empty file is the defaults");
    }

    #[test]
    fn a_partial_file_keeps_the_other_defaults_and_ignores_unknown_keys() {
        let s = parse("copy_on_select = false\nfrom_the_future = 3\n").unwrap();
        assert_eq!(s, Settings { copy_on_select: false, terminal_clipboard: true });
        let s = parse("terminal_clipboard = false").unwrap();
        assert_eq!(s, Settings { copy_on_select: true, terminal_clipboard: false });
    }

    #[test]
    fn an_invalid_file_is_an_error() {
        assert!(parse("copy_on_select = \"yes\"").is_err());
        assert!(parse("not toml at all [").is_err());
    }

    #[test]
    fn roundtrips_through_a_file() {
        let dir = std::env::temp_dir().join(format!("devsandbox-settings-{}", std::process::id()));
        let path = dir.join("sub/dashboard.toml");
        assert_eq!(load_from(&path).unwrap(), Settings::default(), "missing: defaults");
        let s = Settings { copy_on_select: true, terminal_clipboard: false };
        save_to(&path, &s).unwrap();
        assert_eq!(load_from(&path).unwrap(), s);
        std::fs::write(&path, "copy_on_select = 1").unwrap();
        let err = load_from(&path).unwrap_err();
        assert!(format!("{err:#}").contains("invalid settings file"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_table_toggles_each_field() {
        let mut s = Settings::default();
        for (i, spec) in SETTINGS.iter().enumerate() {
            let before = (spec.get)(&s);
            s.toggle(i);
            assert_eq!((spec.get)(&s), !before, "{}", spec.key);
        }
        assert_eq!(s, Settings { copy_on_select: false, terminal_clipboard: false });
        s.toggle(SETTINGS.len()); // out of range: no-op
        assert_eq!(s, Settings { copy_on_select: false, terminal_clipboard: false });
    }

    #[test]
    fn the_copy_help_names_a_key_that_reaches_us() {
        let help = SETTINGS[0].help;
        assert!(help(false).contains("ctrl-shift-c"));
        assert!(help(true).contains("(else ctrl-c / cmd-c)"), "{}", help(true));
    }
}
