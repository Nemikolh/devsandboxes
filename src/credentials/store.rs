//! The file store (credential-broker, *`credentials.toml`*: `source =
//! "file"`): every secret of the home's credentials, by credential name, in
//! one 0600 file beside `member.key` (`daemon_config::CREDENTIAL_STORE`).
//! The default on Linux, where a headless box or a user unit started before
//! a desktop session has no unlocked keyring.
//!
//! Each credential's [`Record`] is a pasted token or the token set an OAuth
//! login got, with what refreshing it takes (its token URL and the client
//! id it was issued to, which may be the home's own rather than the
//! claim's: `login --client-id`).
//!
//! Written as `member.key` is: under the sibling lock, to a temp file
//! created 0600 and renamed (`serve::locked_file::write_atomic_mode`), so
//! the secrets are never on disk with wider permissions. A store readable
//! by group or others is refused, not read. Kept out of every container by
//! `key_guard`.

use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// A secret value: no `Display`, and `Debug` shows nothing of it, so it
/// can't reach a log or an error by accident.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: String) -> Secret {
        Secret(value)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(…)")
    }
}

/// One credential's secret: `[secret.<name>]` with its `type`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Record {
    /// Pasted: used as is until the next login.
    Static { token: Secret },
    #[serde(rename = "oauth")]
    OAuth(OAuthToken),
}

/// What an OAuth login (device flow or browser) got. Times are unix
/// seconds; unset: the provider gave no lifetime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OAuthToken {
    pub access_token: Secret,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<Secret>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_expires_at: Option<u64>,
    pub token_url: String,
    pub client_id: String,
    /// As the token response granted it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

impl Record {
    pub fn token(token: Secret) -> Record {
        Record::Static { token }
    }

    pub fn access_token(&self) -> &Secret {
        match self {
            Record::Static { token } => token,
            Record::OAuth(t) => &t.access_token,
        }
    }
}

#[derive(Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Doc {
    #[serde(default)]
    secret: BTreeMap<String, Record>,
}

pub struct Store {
    path: PathBuf,
}

impl Store {
    /// The store in the data dir `data` (`<data>/devsandbox`).
    pub fn in_data(data: &Path) -> Store {
        Store { path: data.join(crate::daemon_config::CREDENTIAL_STORE) }
    }

    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn get(&self, name: &str) -> Result<Option<Record>> {
        Ok(self.load()?.secret.remove(name))
    }

    /// The token as stored, an OAuth access token expired or not; the
    /// broker renews through `oauth::refresh::Renewer`.
    #[cfg(test)]
    pub fn token_now(&self, name: &str) -> Result<Option<Secret>> {
        Ok(self.get(name)?.map(|r| r.access_token().clone()))
    }

    /// Replaces the record whole.
    pub fn put(&self, name: &str, record: &Record) -> Result<()> {
        self.update(|doc| {
            doc.secret.insert(name.to_string(), record.clone());
        })
    }

    /// `f` runs under the store's lock, which it may hold for a refresh
    /// call, so a login or a second daemon can't race a rotating refresh
    /// token. It sees `name`'s record as stored now and returns its
    /// replacement, if any, written atomically with the rest.
    pub fn renew<T>(&self, name: &str, f: impl FnOnce(Option<&Record>) -> (Option<Record>, T)) -> Result<T> {
        let mut out = None;
        self.update(|doc| {
            let (new, t) = f(doc.secret.get(name));
            if let Some(r) = new {
                doc.secret.insert(name.to_string(), r);
            }
            out = Some(t);
        })?;
        Ok(out.expect("update runs f once loaded"))
    }

    /// Whether there was one.
    pub fn remove(&self, name: &str) -> Result<bool> {
        let mut removed = false;
        self.update(|doc| removed = doc.secret.remove(name).is_some())?;
        Ok(removed)
    }

    fn update(&self, f: impl FnOnce(&mut Doc)) -> Result<()> {
        let dir = self.path.parent().with_context(|| format!("{} has no parent", self.path.display()))?;
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let _lock = crate::serve::locked_file::lock_at(&self.path.with_extension("lock"))?;
        let mut doc = self.load()?;
        let before = doc.secret.clone();
        f(&mut doc);
        if doc.secret == before {
            return Ok(());
        }
        let text = toml::to_string(&doc).context("cannot serialize the credential store")?;
        crate::serve::locked_file::write_atomic_mode(&self.path, &text, 0o600)
    }

    /// A missing file is an empty store. Parse errors name the file, never
    /// its content.
    fn load(&self) -> Result<Doc> {
        let path = &self.path;
        let mut file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Doc::default()),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        };
        let mode = file.metadata().with_context(|| format!("cannot read {}", path.display()))?.permissions().mode();
        if mode & 0o077 != 0 {
            bail!(
                "{} is reachable by group or others (mode {:04o}): the credential store must be 0600 (`chmod 600 {}`)",
                path.display(),
                mode & 0o7777,
                path.display()
            );
        }
        let mut text = String::new();
        file.read_to_string(&mut text).with_context(|| format!("cannot read {}", path.display()))?;
        match toml::from_str(&text) {
            Ok(doc) => Ok(doc),
            Err(_) => bail!("{} doesn't parse as a credential store", path.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dsv-cred-store-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn put_get_remove_at_0600() {
        let dir = temp("rw");
        let store = Store::in_data(&dir);
        assert_eq!(store.get("sentry-work").unwrap(), None);
        assert!(!store.remove("sentry-work").unwrap());
        assert!(!store.path().exists(), "no change, no file");

        let s = Record::token(Secret::new("tok\"en\n=1".into()));
        store.put("sentry-work", &s).unwrap();
        assert_eq!(mode(store.path()), 0o600);
        store.put("github", &Record::token(Secret::new("g".into()))).unwrap();
        assert_eq!(store.get("sentry-work").unwrap(), Some(s));
        assert_eq!(store.token_now("sentry-work").unwrap().unwrap().expose(), "tok\"en\n=1");
        assert!(store.remove("github").unwrap());
        assert_eq!(store.get("github").unwrap(), None);
        assert_eq!(mode(store.path()), 0o600);
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The temp file is created 0600 whatever the umask, and a stale temp
    /// with a wider mode is replaced rather than written into.
    #[test]
    fn created_0600_even_over_a_stale_wide_temp() {
        let dir = temp("umask");
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::in_data(&dir);
        let stale = dir.join(format!(".credentials-store.toml.{}.tmp", std::process::id()));
        std::fs::write(&stale, "old").unwrap();
        std::fs::set_permissions(&stale, std::fs::Permissions::from_mode(0o666)).unwrap();
        store.put("x", &Record::token(Secret::new("v".into()))).unwrap();
        assert_eq!(mode(store.path()), 0o600);
        assert!(!stale.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_wide_or_bad_store_is_refused_without_its_content() {
        let dir = temp("wide");
        let store = Store::in_data(&dir);
        store.put("x", &Record::token(Secret::new("hunter2".into()))).unwrap();
        std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = format!("{:#}", store.get("x").unwrap_err());
        assert!(err.contains("must be 0600"), "{err}");
        assert!(store.put("y", &Record::token(Secret::new("z".into()))).is_err(), "nor rewritten");

        std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(store.path(), "secret = \"hunter2").unwrap();
        let err = format!("{:#}", store.get("x").unwrap_err());
        assert!(err.contains("doesn't parse") && !err.contains("hunter2"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn oauth() -> OAuthToken {
        OAuthToken {
            access_token: Secret::new("ghu_access".into()),
            refresh_token: Some(Secret::new("ghr_refresh".into())),
            expires_at: Some(1_800_000_000),
            refresh_expires_at: Some(1_815_000_000),
            token_url: "https://github.com/login/oauth/access_token".into(),
            client_id: "Iv1.abc".into(),
            scope: None,
        }
    }

    #[test]
    fn records_round_trip_in_their_toml_form() {
        let dir = temp("record");
        let store = Store::in_data(&dir);
        store.put("pat", &Record::token(Secret::new("p".into()))).unwrap();
        store.put("gh", &Record::OAuth(oauth())).unwrap();
        let text = std::fs::read_to_string(store.path()).unwrap();
        assert!(text.contains("[secret.pat]\ntype = \"static\"\ntoken = \"p\"\n"), "{text}");
        assert!(text.contains("[secret.gh]\ntype = \"oauth\"\naccess_token = \"ghu_access\"\n"), "{text}");
        assert!(text.contains("expires_at = 1800000000\n") && !text.contains("scope"), "{text}");
        assert_eq!(store.get("gh").unwrap(), Some(Record::OAuth(oauth())));
        assert_eq!(store.token_now("gh").unwrap().unwrap().expose(), "ghu_access");
        assert_eq!(mode(store.path()), 0o600);

        std::fs::write(store.path(), text.replace("type = \"oauth\"", "type = \"oauth\"\nbogus = 1")).unwrap();
        assert!(store.get("gh").is_err(), "unknown fields refused");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn debug_hides_the_value() {
        assert_eq!(format!("{:?}", Secret::new("hunter2".into())), "Secret(…)");
        let shown = format!("{:?}", Record::OAuth(oauth()));
        assert!(!shown.contains("ghu_") && !shown.contains("ghr_"), "{shown}");
    }
}
