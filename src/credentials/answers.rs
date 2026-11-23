//! `access.json`, next to `state.toml`: the permission requests' answers
//! kept per instance (credential-broker, *Permission requests*): *this
//! instance* and deny, each for one [`AskKey`] until the instance's `rm`
//! or an `access revoke <request>`. *Always* answers are rules on the
//! credential in `credentials.toml` instead ([`super::home::Rule`]).
//!
//! Written by the daemon's [`Registry`](crate::broker::asks::Registry),
//! `rm` and `access revoke` under the sibling `access.lock`, temp file and
//! rename (`serve::locked_file`). Kept out of every container by
//! `key_guard`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::broker::asks::AskKey;

pub const VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// The instance's matching calls go through.
    Instance,
    /// Its matching calls are refused without asking.
    Deny,
}

/// One kept answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Remembered {
    /// The request it answered: what `access revoke` names.
    pub id: String,
    pub instance_id: String,
    /// The instance's name when answered, for display only.
    pub instance: String,
    pub credential: String,
    pub scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    pub answer: Kind,
    /// RFC 3339, UTC.
    pub at: String,
}

impl Remembered {
    pub fn key(&self) -> AskKey {
        AskKey {
            instance_id: self.instance_id.clone(),
            credential: self.credential.clone(),
            scope: self.scope.clone(),
            repo: self.repo.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Answers {
    pub answers: Vec<Remembered>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Doc {
    version: u32,
    #[serde(default)]
    answers: Vec<Remembered>,
}

/// The answer kept for `key`, if any.
pub fn lookup<'a>(answers: &'a Answers, key: &AskKey) -> Option<&'a Remembered> {
    answers.answers.iter().find(|a| a.key() == *key)
}

/// Keeps `answer`, replacing the one kept for its key.
pub fn record(answers: &mut Answers, answer: Remembered) {
    let key = answer.key();
    answers.answers.retain(|a| a.key() != key);
    answers.answers.push(answer);
}

/// Drops the answer kept from request `id`; whether there was one.
pub fn revoke(answers: &mut Answers, id: &str) -> bool {
    let before = answers.answers.len();
    answers.answers.retain(|a| a.id != id);
    answers.answers.len() != before
}

/// `<data>/access.json`.
pub fn path(data: &Path) -> PathBuf {
    data.join(crate::daemon_config::ACCESS)
}

pub fn parse(text: &str) -> Result<Answers> {
    let doc: Doc = serde_json::from_str(text)?;
    if doc.version != VERSION {
        bail!("unsupported version {} (expected {VERSION})", doc.version);
    }
    Ok(Answers { answers: doc.answers })
}

pub fn serialize(answers: &Answers) -> Result<String> {
    let doc = Doc { version: VERSION, answers: answers.answers.clone() };
    Ok(serde_json::to_string_pretty(&doc).context("cannot serialize access answers")? + "\n")
}

/// A missing file is no answers.
pub fn load(path: &Path) -> Result<Answers> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text).with_context(|| format!("invalid {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Answers::default()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// Read-modify-write under the lock, saved only when `f` changed it.
pub fn update<T>(path: &Path, f: impl FnOnce(&mut Answers) -> T) -> Result<T> {
    crate::serve::locked_file::update(path, load, serialize, f)
}

/// Drops every answer kept for the instance: its `rm`.
pub fn forget_instance(data: &Path, instance_id: &str) -> Result<()> {
    update(&path(data), |a| a.answers.retain(|r| r.instance_id != instance_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kept(id: &str, instance_id: &str, scope: &str, repo: Option<&str>, answer: Kind) -> Remembered {
        Remembered {
            id: id.into(),
            instance_id: instance_id.into(),
            instance: "web-1".into(),
            credential: "github-work".into(),
            scope: scope.into(),
            repo: repo.map(String::from),
            answer,
            at: "2026-11-23T10:00:00Z".into(),
        }
    }

    fn dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dsv-answers-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn round_trips_versioned() {
        let answers = Answers {
            answers: vec![
                kept("R-AAAAAAAAAAAA", "i1", "pr.write", Some("o/r"), Kind::Instance),
                kept("R-BBBBBBBBBBBB", "i1", "other", None, Kind::Deny),
            ],
        };
        let text = serialize(&answers).unwrap();
        assert!(text.starts_with("{\n  \"version\": 1,\n  \"answers\": ["), "{text}");
        assert!(text.contains("\"answer\": \"instance\"") && text.contains("\"answer\": \"deny\""), "{text}");
        assert_eq!(text.matches("\"repo\"").count(), 1, "an unset repo is left out: {text}");
        assert_eq!(parse(&text).unwrap(), answers);
        let err = format!("{:#}", parse("{\"version\": 2, \"answers\": []}").unwrap_err());
        assert!(err.contains("unsupported version 2"), "{err}");
        assert!(parse("{\"version\": 1, \"bogus\": 1}").is_err());
    }

    #[test]
    fn lookup_matches_the_whole_key_and_record_replaces() {
        let mut answers = Answers::default();
        record(&mut answers, kept("R-AAAAAAAAAAAA", "i1", "pr.write", Some("o/r"), Kind::Instance));
        let key = kept("x", "i1", "pr.write", Some("o/r"), Kind::Instance).key();
        assert_eq!(lookup(&answers, &key).unwrap().id, "R-AAAAAAAAAAAA");
        for other in [
            kept("x", "i2", "pr.write", Some("o/r"), Kind::Instance).key(),
            kept("x", "i1", "pr.read", Some("o/r"), Kind::Instance).key(),
            kept("x", "i1", "pr.write", Some("o/s"), Kind::Instance).key(),
            kept("x", "i1", "pr.write", None, Kind::Instance).key(),
        ] {
            assert!(lookup(&answers, &other).is_none(), "{other:?}");
        }
        record(&mut answers, kept("R-BBBBBBBBBBBB", "i1", "pr.write", Some("o/r"), Kind::Deny));
        assert_eq!(answers.answers.len(), 1);
        assert_eq!(lookup(&answers, &key).unwrap().answer, Kind::Deny);
        assert!(!revoke(&mut answers, "R-AAAAAAAAAAAA"), "replaced already");
        assert!(revoke(&mut answers, "R-BBBBBBBBBBBB"));
        assert!(answers.answers.is_empty());
    }

    #[test]
    fn forget_instance_drops_its_answers_only() {
        let data = dir("forget");
        let p = path(&data);
        update(&p, |a| {
            record(a, kept("R-AAAAAAAAAAAA", "i1", "pr.write", None, Kind::Instance));
            record(a, kept("R-BBBBBBBBBBBB", "i2", "pr.write", None, Kind::Deny));
        })
        .unwrap();
        assert!(data.join("access.lock").exists());
        forget_instance(&data, "i1").unwrap();
        let left = load(&p).unwrap();
        assert_eq!(left.answers.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["R-BBBBBBBBBBBB"]);
        forget_instance(&dir("forget-none"), "i1").unwrap();
        let _ = std::fs::remove_dir_all(&data);
    }
}
