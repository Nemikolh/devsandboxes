//! What in a config breaks under `identity = "host"`: the *findings* a
//! migration offers to fix (docs/plans/active/identity.md, section 7
//! *Migrating existing configs*). Pure over [`Config`]; no I/O.
//!
//! Each template's and sandbox's *own* table is scanned, so a finding names
//! the table that defines it ([`Origin`]) and one edit there fixes every
//! sandbox it reaches. The kinds ([`Kind`]):
//! - `RootTarget`: a `mounts` target equal to or under `/root`, shorthand
//!   (`target=`/`dst=`/`destination=`) or object (`target`);
//! - `RootEnv`: a `containerEnv`/`remoteEnv` value equal to or under `/root`;
//! - `RootPath`: `workspaceFolder`, or a `folders` key (its container path),
//!   under `/root`. No automatic edit: fixed by hand;
//! - `RootUser`: `remoteUser`/`containerUser = "root"`, for the sandboxes
//!   where this table's value wins the merge (the last table of
//!   [`Config::extends_chain`] + the sandbox to set the key, as
//!   `Config::resolved_table` merges it). Its edit removes the key.
//!
//! "Under `/root`" is `/root` itself or `/root/…` (not `/rootfs`). The edit
//! for `RootTarget`/`RootEnv` replaces that leading `/root` with
//! `${containerEnv:HOME}`. Not findings: caches (`/var/cache/devsandbox/`),
//! shell history (`/commandhistory`), features' mounts (not in the user's
//! tables) and `updateRemoteUserUID`.
//!
//! `sandboxes` lists every sandbox whose `extends` chain reaches the table
//! and whose effective identity (the merged `identity` /
//! `updateRemoteUserUID`, as `SandboxProperties::identity`) is `host`:
//! under `image`, users are literal and `/root` works as before. A finding
//! reaching sandboxes, none of them counting, isn't one. A template no
//! sandbox reaches keeps its findings with an empty `sandboxes`, unless its
//! own chain makes it `image` (so writing `identity = "image"` next to a
//! root user, the "continue as is", clears it for good). A sandbox whose
//! `extends` doesn't resolve counts as reaching only its own table.
//!
//! A finding's [`Finding::id`] is `short_hash("<table>|<key>|<value>")`, e.g.
//! `sandbox.web|mounts|source=/x,target=/root/.x`: no array index, so it
//! survives reordering, and a new or edited value is a new finding. `table`
//! is [`Origin::table`]; `key` and `value` are [`Finding`]'s.
//!
//! **Rewrite** ([`rewrite`], [`keep_root`]): pure over the file's text, on a
//! `toml_edit` document so comments, blank lines and order survive. Each
//! finding is located by its table, key and current value (shorthand mount
//! strings and env values replaced in place, an object mount's `target`
//! replaced by key, in an inline table or a `[[….mounts]]` entry; a root
//! user's key removed), and the output is reparsed to check the findings
//! are gone. [`migrate_file`] applies it to a config file: YAML is refused
//! with the edits as text, a file whose [`file_hash`] changed is a
//! conflict, and the write goes through a `.bak` copy and tmp + rename.
//!
//! [`ops`] is all of it as the API's `config.*` methods, with the acks in
//! `state.toml`.

// Only the daemon (unix) calls it so far; the CLI's no-daemon path comes in
// identity step 16.
#![cfg_attr(not(unix), allow(dead_code))]

pub mod ops;

use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use toml::{Table, Value};
use toml_edit::{DocumentMut, Item, TableLike};

use crate::config::{self, Config, SandboxProperties};
use crate::runtime::identity::Identity;

/// The path the findings are about.
const ROOT: &str = "/root";
/// What a `/root` prefix becomes.
const HOME: &str = "${containerEnv:HOME}";

/// The table a finding is in.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Origin {
    Template(String),
    Sandbox(String),
}

impl Origin {
    /// `template.<name>` / `sandbox.<name>`: the table's TOML header, and the
    /// first part of a finding's id.
    pub fn table(&self) -> String {
        match self {
            Origin::Template(name) => format!("template.{name}"),
            Origin::Sandbox(name) => format!("sandbox.{name}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    RootTarget,
    RootEnv,
    RootPath,
    RootUser,
}

/// The automatic fix for a finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    /// Replace [`Finding::value`] with this (for a mount, the whole mount
    /// rendered as [`mount_text`] renders it).
    Set(String),
    /// Remove [`Finding::key`] from the table.
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// `short_hash("<table>|<key>|<value>")` (module doc).
    pub id: String,
    pub origin: Origin,
    pub kind: Kind,
    /// Where in the table: `mounts`, `containerEnv.<NAME>`,
    /// `remoteEnv.<NAME>`, `workspaceFolder`, `folders`, `remoteUser`,
    /// `containerUser`.
    pub key: String,
    /// The current value: the mount's [`mount_text`], the env value, the
    /// path (for `folders`, the entry's key), or `root`.
    pub value: String,
    /// `None`: fixed by hand (`RootPath`).
    pub edit: Option<Edit>,
    /// The sandboxes it applies to, sorted (module doc); empty for a
    /// template no sandbox reaches.
    pub sandboxes: Vec<String>,
}

/// Every finding in `config`: templates' first, then sandboxes', each table
/// in its keys' order (`mounts`, env, paths, users).
pub fn identity_findings(config: &Config) -> Vec<Finding> {
    let chains: Vec<(&String, Vec<Origin>)> = config
        .sandboxes
        .iter()
        .map(|(name, table)| (name, order(config, table, Origin::Sandbox(name.clone()))))
        .collect();
    let origins = config
        .templates
        .iter()
        .map(|(name, table)| (Origin::Template(name.clone()), table))
        .chain(config.sandboxes.iter().map(|(name, table)| (Origin::Sandbox(name.clone()), table)));

    let mut findings: Vec<Finding> = Vec::new();
    for (origin, table) in origins {
        let reached: Vec<&(&String, Vec<Origin>)> =
            chains.iter().filter(|(_, chain)| chain.contains(&origin)).collect();
        for (kind, key, value, edit) in scan(table) {
            let sandboxes: Vec<String> = reached
                .iter()
                .filter(|(_, chain)| identity(config, chain) == Identity::Host)
                .filter(|(_, chain)| kind != Kind::RootUser || winner(config, chain, &key) == Some(&origin))
                .map(|(name, _)| name.to_string())
                .collect();
            if sandboxes.is_empty() {
                if !reached.is_empty() {
                    continue;
                }
                if identity(config, &order(config, table, origin.clone())) == Identity::Image {
                    continue;
                }
            }
            let id = config::short_hash(&format!("{}|{key}|{value}", origin.table()));
            if findings.iter().any(|f| f.id == id) {
                continue; // the same mount listed twice in one table
            }
            findings.push(Finding { id, origin: origin.clone(), kind, key, value, edit, sandboxes });
        }
    }
    findings
}

/// The tables merged for `table` (`me`), in merge order, `me` last.
fn order(config: &Config, table: &Table, me: Origin) -> Vec<Origin> {
    let mut order: Vec<Origin> = config
        .extends_chain(table)
        .unwrap_or_default()
        .into_iter()
        .map(Origin::Template)
        .collect();
    order.push(me);
    order
}

fn table_of<'a>(config: &'a Config, origin: &Origin) -> Option<&'a Table> {
    match origin {
        Origin::Template(name) => config.templates.get(name),
        Origin::Sandbox(name) => config.sandboxes.get(name),
    }
}

/// The table in `order` whose top-level `key` the merge keeps (the last to
/// set it), with its value.
fn last_set<'a>(config: &'a Config, order: &'a [Origin], key: &str) -> Option<(&'a Origin, &'a Value)> {
    order
        .iter()
        .rev()
        .find_map(|origin| table_of(config, origin)?.get(key).map(|v| (origin, v)))
}

fn winner<'a>(config: &'a Config, order: &'a [Origin], key: &str) -> Option<&'a Origin> {
    last_set(config, order, key).map(|(origin, _)| origin)
}

/// The merged identity of `order`, read like `SandboxProperties::identity`
/// (a malformed value counts as unset; validation reports it elsewhere).
fn identity(config: &Config, order: &[Origin]) -> Identity {
    let props = SandboxProperties {
        identity: last_set(config, order, "identity").and_then(|(_, v)| v.clone().try_into().ok()),
        update_remote_user_uid: last_set(config, order, "updateRemoteUserUID").and_then(|(_, v)| v.as_bool()),
        ..Default::default()
    };
    props.identity()
}

/// One table's own findings, without ids or sandboxes.
fn scan(table: &Table) -> Vec<(Kind, String, String, Option<Edit>)> {
    let mut out = Vec::new();
    if let Some(Value::Array(mounts)) = table.get("mounts") {
        for mount in mounts {
            if let (Some(text), Some(edit)) = (mount_text(mount), mount_edit(mount)) {
                out.push((Kind::RootTarget, "mounts".to_string(), text, Some(Edit::Set(edit))));
            }
        }
    }
    for field in ["containerEnv", "remoteEnv"] {
        let Some(Value::Table(env)) = table.get(field) else { continue };
        for (name, value) in env {
            if let Some(value) = value.as_str()
                && let Some(edit) = home_path(value)
            {
                out.push((Kind::RootEnv, format!("{field}.{name}"), value.to_string(), Some(Edit::Set(edit))));
            }
        }
    }
    if let Some(path) = table.get("workspaceFolder").and_then(Value::as_str)
        && under_root(path)
    {
        out.push((Kind::RootPath, "workspaceFolder".to_string(), path.to_string(), None));
    }
    if let Some(Value::Table(folders)) = table.get("folders") {
        for path in folders.keys().filter(|p| under_root(p)) {
            out.push((Kind::RootPath, "folders".to_string(), path.clone(), None));
        }
    }
    for key in ["containerUser", "remoteUser"] {
        if table.get(key).and_then(Value::as_str) == Some("root") {
            out.push((Kind::RootUser, key.to_string(), "root".to_string(), Some(Edit::Remove)));
        }
    }
    out
}

/// `/root` itself or a path under it.
pub fn under_root(path: &str) -> bool {
    path.strip_prefix(ROOT).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// `path` with its leading `/root` replaced by `${containerEnv:HOME}`;
/// `None` unless it's [`under_root`].
pub fn home_path(path: &str) -> Option<String> {
    under_root(path).then(|| format!("{HOME}{}", &path[ROOT.len()..]))
}

/// A `mounts` entry as a finding's value: a shorthand string as written; an
/// object as `key=value` pairs in key order, joined by `,` (strings bare,
/// other values as TOML), e.g. `readonly=true,source=/x,target=/root/.x`.
/// `None` for anything else.
pub fn mount_text(mount: &Value) -> Option<String> {
    match mount {
        Value::String(s) => Some(s.clone()),
        Value::Table(t) => Some(
            t.iter()
                .map(|(k, v)| match v {
                    Value::String(s) => format!("{k}={s}"),
                    other => format!("{k}={other}"),
                })
                .collect::<Vec<_>>()
                .join(","),
        ),
        _ => None,
    }
}

/// The mount with its target moved to [`home_path`], as [`mount_text`];
/// `None` when the target isn't under `/root` (or the mount doesn't parse:
/// validation reports it elsewhere). A shorthand keeps every other part as
/// written.
fn mount_edit(mount: &Value) -> Option<String> {
    match mount {
        Value::String(spec) => {
            let (_, _, target, _) = config::parse_shorthand(spec).ok()?;
            home_path(&target)?;
            let parts: Vec<String> = spec
                .split(',')
                .map(|part| match part.split_once('=') {
                    Some((key, value)) if matches!(key.trim(), "target" | "destination" | "dst") => {
                        home_path(value.trim()).map_or_else(|| part.to_string(), |new| format!("{key}={new}"))
                    }
                    _ => part.to_string(),
                })
                .collect();
            Some(parts.join(","))
        }
        Value::Table(t) => {
            let new = home_path(t.get("target")?.as_str()?)?;
            let mut t = t.clone();
            t.insert("target".to_string(), Value::String(new));
            mount_text(&Value::Table(t))
        }
        _ => None,
    }
}

/// Apply `findings`' edits to the config text `text`; returns the new text
/// and the number of findings applied. `RootPath` findings (no edit) are
/// skipped and not counted. A finding that isn't where it says (its table,
/// key and current value) is an error, as is output that doesn't parse or
/// still has an applied finding.
pub fn rewrite(text: &str, findings: &[Finding]) -> anyhow::Result<(String, usize)> {
    let mut doc: DocumentMut = text.parse().context("invalid config")?;
    let mut applied: Vec<&str> = Vec::new();
    for finding in findings {
        let Some(edit) = &finding.edit else { continue };
        let table = table_mut(&mut doc, &finding.origin).with_context(|| not_found(finding))?;
        if !apply(table, finding, edit) {
            bail!("{}", not_found(finding));
        }
        applied.push(&finding.id);
    }
    let out = doc.to_string();
    verify(&out, &applied)?;
    Ok((out, applied.len()))
}

/// "Continue as is" for a `RootUser` finding: `identity = "image"` in its
/// table (an existing `identity` is overwritten), so keeping root is explicit
/// and the finding is gone for good. Checked like [`rewrite`].
pub fn keep_root(text: &str, finding: &Finding) -> anyhow::Result<String> {
    if finding.kind != Kind::RootUser {
        bail!("only a root user can be kept, not {} in [{}]", finding.key, finding.origin.table());
    }
    let mut doc: DocumentMut = text.parse().context("invalid config")?;
    let table = table_mut(&mut doc, &finding.origin).with_context(|| not_found(finding))?;
    match table.get_mut("identity").and_then(Item::as_value_mut) {
        Some(value) => set_str(value, "image"),
        None => {
            table.insert("identity", toml_edit::value("image"));
        }
    }
    let out = doc.to_string();
    verify(&out, &[&finding.id])?;
    Ok(out)
}

fn not_found(finding: &Finding) -> String {
    format!("{} = {:?} not found in [{}]", finding.key, finding.value, finding.origin.table())
}

/// `[template.<name>]` / `[sandbox.<name>]` in whatever form it's written:
/// a standard, dotted, implicit or inline table.
fn table_mut<'a>(doc: &'a mut DocumentMut, origin: &Origin) -> Option<&'a mut dyn TableLike> {
    let (top, name) = match origin {
        Origin::Template(name) => ("template", name),
        Origin::Sandbox(name) => ("sandbox", name),
    };
    doc.as_table_mut().get_mut(top)?.as_table_like_mut()?.get_mut(name)?.as_table_like_mut()
}

/// One finding's edit on its table; `false` when it isn't there.
fn apply(table: &mut dyn TableLike, finding: &Finding, edit: &Edit) -> bool {
    match (finding.kind, edit) {
        (Kind::RootTarget, Edit::Set(new)) => set_mount(table, &finding.value, new),
        (Kind::RootEnv, Edit::Set(new)) => {
            let Some((field, name)) = finding.key.split_once('.') else { return false };
            let value = table
                .get_mut(field)
                .and_then(Item::as_table_like_mut)
                .and_then(|env| env.get_mut(name))
                .and_then(Item::as_value_mut);
            match value {
                Some(value) if value.as_str() == Some(&finding.value) => {
                    set_str(value, new);
                    true
                }
                _ => false,
            }
        }
        (Kind::RootUser, Edit::Remove) => {
            if table.get(&finding.key).and_then(Item::as_str) != Some(finding.value.as_str()) {
                return false;
            }
            table.remove(&finding.key);
            true
        }
        _ => false,
    }
}

/// Every `mounts` entry whose [`mount_text`] is `old` (a table may list one
/// twice: one finding) gets the edit: a shorthand string becomes `new`, an
/// object's `target` its [`home_path`]. Entries are in an array (strings,
/// inline tables) or an array of tables (`[[sandbox.x.mounts]]`).
fn set_mount(table: &mut dyn TableLike, old: &str, new: &str) -> bool {
    let mut hit = false;
    match table.get_mut("mounts") {
        Some(Item::Value(toml_edit::Value::Array(mounts))) => {
            for mount in mounts.iter_mut() {
                if let toml_edit::Value::String(s) = &*mount {
                    if s.value() == old {
                        set_str(mount, new);
                        hit = true;
                    }
                } else if let toml_edit::Value::InlineTable(t) = mount
                    && plain(&toml_edit::Value::InlineTable(t.clone())).and_then(|v| mount_text(&v)).as_deref() == Some(old)
                {
                    hit |= set_target(t);
                }
            }
        }
        Some(Item::ArrayOfTables(mounts)) => {
            for t in mounts.iter_mut() {
                let text = plain(&toml_edit::Value::InlineTable(t.clone().into_inline_table())).and_then(|v| mount_text(&v));
                if text.as_deref() == Some(old) {
                    hit |= set_target(t);
                }
            }
        }
        _ => {}
    }
    hit
}

/// An object mount's `target` moved to its [`home_path`].
fn set_target(mount: &mut dyn TableLike) -> bool {
    let Some(target) = mount.get_mut("target").and_then(Item::as_value_mut) else { return false };
    let Some(new) = target.as_str().and_then(home_path) else { return false };
    set_str(target, &new);
    true
}

/// `toml_edit`'s value as `toml`'s, for [`mount_text`]: its text, without
/// the decor (comments around it), reparsed.
fn plain(value: &toml_edit::Value) -> Option<Value> {
    let mut value = value.clone();
    value.decor_mut().clear();
    toml::from_str::<Table>(&format!("v = {value}\n")).ok()?.remove("v")
}

/// Replace a value with the string `new`, keeping its decor (the spacing and
/// comments around it).
fn set_str(value: &mut toml_edit::Value, new: &str) {
    let decor = value.decor().clone();
    *value = new.into();
    *value.decor_mut() = decor;
}

/// `out` parses, and none of `ids` is among its findings.
fn verify(out: &str, ids: &[&str]) -> anyhow::Result<()> {
    let config = Config::parse(out).context("the rewritten config doesn't parse")?;
    let left: Vec<String> = identity_findings(&config)
        .into_iter()
        .filter(|f| ids.contains(&f.id.as_str()))
        .map(|f| format!("{} in [{}]", f.key, f.origin.table()))
        .collect();
    if !left.is_empty() {
        bail!("the rewrite didn't fix {}", left.join(", "));
    }
    Ok(())
}

/// A config file's hash, as read for its findings and checked again before
/// [`migrate_file`] writes: [`config::short_hash`] of its text (UTF-8, so the
/// same as of its bytes).
pub fn file_hash(text: &str) -> String {
    config::short_hash(text)
}

/// One finding's edit as a line for a person (YAML configs get these instead
/// of a rewrite); `None` for a finding fixed by hand. `keep` is "continue as
/// is" for a `RootUser`.
pub fn edit_line(finding: &Finding, keep: bool) -> Option<String> {
    let table = finding.origin.table();
    if keep {
        return Some(format!("{table}: set identity = \"image\" (keeps {} = \"root\")", finding.key));
    }
    match finding.edit.as_ref()? {
        Edit::Set(new) if finding.kind == Kind::RootTarget => {
            Some(format!("{table}: mounts entry \"{}\" -> \"{new}\"", finding.value))
        }
        Edit::Set(new) => Some(format!("{table}: {} = \"{}\" -> \"{new}\"", finding.key, finding.value)),
        Edit::Remove => Some(format!("{table}: remove {} = \"{}\"", finding.key, finding.value)),
    }
}

/// What [`migrate_file`] did.
#[derive(Debug, PartialEq, Eq)]
pub struct Migrated {
    /// Findings fixed (edits + keeps); 0 wrote nothing.
    pub edits: usize,
    /// The file as given (a symlink's target was written).
    pub file: PathBuf,
}

/// Why [`migrate_file`] wrote nothing (or failed writing). Shaped for the
/// API's error codes: `conflict`, `unsupported`, `invalid`; `Io` is an
/// internal error.
#[derive(Debug)]
pub enum MigrateError {
    /// The file's [`file_hash`] isn't the one its findings were read at.
    Conflict,
    /// A YAML config: not rewritten. `edits_text` is the edits, one
    /// [`edit_line`] per line, for the user to make by hand.
    Unsupported { edits_text: String },
    /// The config doesn't parse, an id isn't a current finding (or a kept
    /// one isn't a root user), or the rewrite didn't come out right.
    Invalid(String),
    /// Reading or writing failed; the message names a `.bak` left behind.
    Io(anyhow::Error),
}

impl std::fmt::Display for MigrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MigrateError::Conflict => write!(f, "the config file changed since its findings were read"),
            MigrateError::Unsupported { edits_text } => {
                write!(f, "YAML configs aren't rewritten; make these edits by hand:\n{edits_text}")
            }
            MigrateError::Invalid(why) => write!(f, "{why}"),
            MigrateError::Io(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for MigrateError {}

/// Fix the config file at `path` (as [`config::find`] picks it): the findings
/// `ids` (`None`: every one with an edit, but `keep`'s), and "continue as
/// is" for the `RootUser` findings `keep`. `expected_hash` is the
/// [`file_hash`] the ids were read at.
///
/// The write: a `<file>.bak` copy, the new text to a tmp file in the same
/// dir with the old mode, renamed over the file (a symlink's target: the
/// link stays), then reread and checked. The `.bak` is removed once that
/// passes, and left (named in the error) when anything fails.
pub fn migrate_file(
    path: &Path,
    ids: Option<&[String]>,
    keep: &[String],
    expected_hash: &str,
) -> Result<Migrated, MigrateError> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read {}", path.display()))
        .map_err(MigrateError::Io)?;
    if file_hash(&text) != expected_hash {
        return Err(MigrateError::Conflict);
    }
    let yaml = matches!(path.extension().and_then(|e| e.to_str()), Some("yaml" | "yml"));
    let config = if yaml { Config::parse_yaml(&text) } else { Config::parse(&text) }
        .map_err(|e| MigrateError::Invalid(format!("{e:#}")))?;
    let (edits, keeps) = select(identity_findings(&config), ids, keep).map_err(MigrateError::Invalid)?;
    if yaml {
        let lines = edits.iter().map(|f| edit_line(f, false)).chain(keeps.iter().map(|f| edit_line(f, true)));
        let edits_text = lines.flatten().collect::<Vec<_>>().join("\n");
        return Err(MigrateError::Unsupported { edits_text });
    }

    let invalid = |e: anyhow::Error| MigrateError::Invalid(format!("{e:#}"));
    let (mut new, mut count) = rewrite(&text, &edits).map_err(invalid)?;
    for finding in &keeps {
        new = keep_root(&new, finding).map_err(invalid)?;
        count += 1;
    }
    let file = path.to_path_buf();
    if count == 0 {
        return Ok(Migrated { edits: 0, file });
    }
    let ids: Vec<&str> = edits.iter().filter(|f| f.edit.is_some()).chain(&keeps).map(|f| f.id.as_str()).collect();
    write(path, &new, &ids).map_err(MigrateError::Io)?;
    Ok(Migrated { edits: count, file })
}

/// `ids`/`keep` as findings: `(edits, keeps)`. Every id must be a current
/// finding, a kept one a `RootUser`, and none both.
fn select(findings: Vec<Finding>, ids: Option<&[String]>, keep: &[String]) -> Result<(Vec<Finding>, Vec<Finding>), String> {
    let known = |id: &String| findings.iter().any(|f| &f.id == id);
    if let Some(id) = ids.into_iter().flatten().chain(keep).find(|id| !known(id)) {
        return Err(format!("no finding {id} in the config (reread its findings)"));
    }
    if let Some(id) = ids.into_iter().flatten().find(|id| keep.contains(id)) {
        return Err(format!("finding {id} is both updated and kept"));
    }
    let (keeps, rest): (Vec<Finding>, Vec<Finding>) = findings.into_iter().partition(|f| keep.contains(&f.id));
    if let Some(f) = keeps.iter().find(|f| f.kind != Kind::RootUser) {
        return Err(format!("only a root user can be kept, not {} in [{}]", f.key, f.origin.table()));
    }
    let edits = match ids {
        Some(ids) => rest.into_iter().filter(|f| ids.contains(&f.id)).collect(),
        None => rest.into_iter().filter(|f| f.edit.is_some()).collect(),
    };
    Ok((edits, keeps))
}

/// [`migrate_file`]'s write of `new` over `path`, checked for `ids` gone.
fn write(path: &Path, new: &str, ids: &[&str]) -> anyhow::Result<()> {
    let target = std::fs::canonicalize(path).with_context(|| format!("cannot resolve {}", path.display()))?;
    let name = target.file_name().context("config path has no file name")?.to_string_lossy().into_owned();
    let dir = target.parent().context("config path has no dir")?;
    let bak = dir.join(format!("{name}.bak"));
    std::fs::copy(&target, &bak).with_context(|| format!("cannot back up {} to {}", target.display(), bak.display()))?;
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let replaced = (|| -> anyhow::Result<()> {
        let mode = std::fs::metadata(&target)?.permissions();
        std::fs::write(&tmp, new).with_context(|| format!("cannot write {}", tmp.display()))?;
        std::fs::set_permissions(&tmp, mode).with_context(|| format!("cannot set the mode of {}", tmp.display()))?;
        std::fs::rename(&tmp, &target).with_context(|| format!("cannot replace {}", target.display()))?;
        let back = std::fs::read_to_string(&target).with_context(|| format!("cannot reread {}", target.display()))?;
        verify(&back, ids).with_context(|| format!("{} after the write", target.display()))
    })();
    if let Err(e) = replaced {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.context(format!("backup left at {}", bak.display())));
    }
    std::fs::remove_file(&bak).with_context(|| format!("updated {}, but cannot remove {}", target.display(), bak.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn findings(toml: &str) -> Vec<Finding> {
        identity_findings(&Config::parse(toml).unwrap())
    }

    /// `(table, kind, key, value, sandboxes)` per finding, for compact asserts.
    fn summary(toml: &str) -> Vec<(String, Kind, String, String, Vec<String>)> {
        findings(toml)
            .into_iter()
            .map(|f| (f.origin.table(), f.kind, f.key, f.value, f.sandboxes))
            .collect()
    }

    fn row(table: &str, kind: Kind, key: &str, value: &str, sandboxes: &[&str]) -> (String, Kind, String, String, Vec<String>) {
        let sandboxes = sandboxes.iter().map(|s| s.to_string()).collect();
        (table.to_string(), kind, key.to_string(), value.to_string(), sandboxes)
    }

    #[test]
    fn a_sandbox_every_kind() {
        let got = findings(
            r#"
[sandbox.web]
image = "img"
mounts = [
  "source=${localEnv:HOME}/.config/gh,target=/root/.config/gh,type=bind",
  "type=bind, src=/x, dst=/root/.x",
  { source = "/y", target = "/root/.y", readonly = true },
]
containerEnv = { CLAUDE_CONFIG_DIR = "/root/.claude", EDITOR = "vim" }
remoteEnv = { CACHE = "/root" }
workspaceFolder = "/root/app"
folders = { "/root/lib" = "../lib", "/workspaces/ok" = "../ok" }
containerUser = "root"
remoteUser = "root"
"#,
        );
        let got: Vec<_> = got.into_iter().map(|f| (f.kind, f.key, f.value, f.edit, f.sandboxes)).collect();
        let set = |s: &str| Some(Edit::Set(s.to_string()));
        let web = vec!["web".to_string()];
        let want = vec![
            (
                Kind::RootTarget,
                "mounts",
                "source=${localEnv:HOME}/.config/gh,target=/root/.config/gh,type=bind",
                set("source=${localEnv:HOME}/.config/gh,target=${containerEnv:HOME}/.config/gh,type=bind"),
            ),
            (Kind::RootTarget, "mounts", "type=bind, src=/x, dst=/root/.x", set("type=bind, src=/x, dst=${containerEnv:HOME}/.x")),
            (
                Kind::RootTarget,
                "mounts",
                "readonly=true,source=/y,target=/root/.y",
                set("readonly=true,source=/y,target=${containerEnv:HOME}/.y"),
            ),
            (Kind::RootEnv, "containerEnv.CLAUDE_CONFIG_DIR", "/root/.claude", set("${containerEnv:HOME}/.claude")),
            (Kind::RootEnv, "remoteEnv.CACHE", "/root", set("${containerEnv:HOME}")),
            (Kind::RootPath, "workspaceFolder", "/root/app", None),
            (Kind::RootPath, "folders", "/root/lib", None),
            (Kind::RootUser, "containerUser", "root", Some(Edit::Remove)),
            (Kind::RootUser, "remoteUser", "root", Some(Edit::Remove)),
        ];
        let want: Vec<_> = want
            .into_iter()
            .map(|(k, key, value, edit)| (k, key.to_string(), value.to_string(), edit, web.clone()))
            .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn template_shared_by_two_sandboxes_is_one_finding() {
        let got = summary(
            r#"
[template.base]
mounts = ["source=/a,target=/root/.a"]
[sandbox.web]
extends = "base"
[sandbox.api]
extends = ["base"]
"#,
        );
        assert_eq!(got, vec![row("template.base", Kind::RootTarget, "mounts", "source=/a,target=/root/.a", &["api", "web"])]);
    }

    #[test]
    fn unused_template_has_no_sandboxes() {
        let got = summary("[template.old]\nremoteUser = \"root\"\ncontainerEnv = { X = \"/root/x\" }\n");
        assert_eq!(
            got,
            vec![
                row("template.old", Kind::RootEnv, "containerEnv.X", "/root/x", &[]),
                row("template.old", Kind::RootUser, "remoteUser", "root", &[]),
            ]
        );
    }

    #[test]
    fn extends_chain_reaches_through_templates() {
        let got = summary(
            r#"
[template.base]
workspaceFolder = "/root/src"
[template.mid]
extends = "base"
[sandbox.web]
extends = "mid"
"#,
        );
        assert_eq!(got, vec![row("template.base", Kind::RootPath, "workspaceFolder", "/root/src", &["web"])]);
    }

    #[test]
    fn root_user_counts_only_where_it_wins() {
        let got = summary(
            r#"
[template.base]
remoteUser = "root"
[template.dev]
extends = "base"
remoteUser = "dev"
containerUser = "dev"
[sandbox.plain]
extends = "base"
[sandbox.overridden]
extends = "base"
remoteUser = "dev"
[sandbox.via_dev]
extends = "dev"
[sandbox.own]
extends = "dev"
containerUser = "root"
"#,
        );
        assert_eq!(
            got,
            vec![
                row("template.base", Kind::RootUser, "remoteUser", "root", &["plain"]),
                row("sandbox.own", Kind::RootUser, "containerUser", "root", &["own"]),
            ]
        );
        // Overridden in every sandbox reaching it: not a finding at all.
        let got = summary("[template.base]\nremoteUser = \"root\"\n[sandbox.web]\nextends = \"base\"\nremoteUser = \"dev\"\n");
        assert_eq!(got, vec![]);
    }

    #[test]
    fn root_exactly_and_under_but_not_lookalikes() {
        let got = summary(
            r#"
[sandbox.web]
mounts = ["source=/a,target=/root", "source=/b,target=/rootfs/x", "source=/c,target=/root/", "source=/d,target=/home/root"]
containerEnv = { A = "/rootfs", B = "x/root" }
remoteUser = "rooter"
"#,
        );
        let values: Vec<_> = got.into_iter().map(|r| r.3).collect();
        assert_eq!(values, vec!["source=/a,target=/root", "source=/c,target=/root/"]);
        assert!(under_root("/root") && under_root("/root/.x"));
        assert!(!under_root("/rootfs") && !under_root("/roo") && !under_root("root"));
        assert_eq!(home_path("/root/"), Some("${containerEnv:HOME}/".to_string()));
    }

    #[test]
    fn caches_history_and_hints_are_not_findings() {
        let got = summary(
            r#"
[sandbox.web]
caches = ["cargo", "pnpm"]
persist-shell-history = true
updateRemoteUserUID = true
mounts = ["source=/h,target=/commandhistory", "source=/c,target=/var/cache/devsandbox/cargo"]
"#,
        );
        assert_eq!(got, vec![]);
    }

    #[test]
    fn image_identity_is_not_affected() {
        // Explicit, through the hint, and inherited from a template.
        for extra in ["identity = \"image\"", "updateRemoteUserUID = false"] {
            let got = summary(&format!("[sandbox.web]\nremoteUser = \"root\"\nmounts = [\"source=/a,target=/root/.a\"]\n{extra}\n"));
            assert_eq!(got, vec![], "{extra}");
        }
        let toml = r#"
[template.base]
mounts = ["source=/a,target=/root/.a"]
[template.keep]
identity = "image"
[sandbox.legacy]
extends = ["base", "keep"]
[sandbox.web]
extends = ["base", "keep"]
identity = "host"
"#;
        assert_eq!(summary(toml), vec![row("template.base", Kind::RootTarget, "mounts", "source=/a,target=/root/.a", &["web"])]);
        // "Continue as is" on an unused template's root user clears it.
        assert_eq!(summary("[template.old]\nremoteUser = \"root\"\nidentity = \"image\"\n"), vec![]);
    }

    #[test]
    fn ids_name_the_table_key_and_value_not_the_index() {
        let a = findings("[sandbox.web]\nmounts = [\"source=/a,target=/root/.a\", \"source=/b,target=/root/.b\"]\n");
        let b = findings("[sandbox.web]\nmounts = [\"source=/b,target=/root/.b\", \"source=/a,target=/root/.a\"]\n");
        let ids = |f: &[Finding]| f.iter().map(|f| f.id.clone()).collect::<std::collections::BTreeSet<_>>();
        assert_eq!(ids(&a), ids(&b));
        assert_eq!(a[0].id, config::short_hash("sandbox.web|mounts|source=/a,target=/root/.a"));
        // The same text in another table, or an edited value, is another id.
        let t = findings("[template.web]\nmounts = [\"source=/a,target=/root/.a\"]\n");
        assert_ne!(t[0].id, a[0].id);
        let edited = findings("[sandbox.web]\nmounts = [\"source=/a,target=/root/.c\"]\n");
        assert!(!ids(&a).contains(&edited[0].id));
        // A duplicate entry in one table is one finding.
        let dup = findings("[sandbox.web]\nmounts = [\"source=/a,target=/root/.a\", \"source=/a,target=/root/.a\"]\n");
        assert_eq!(dup.len(), 1);
    }

    #[test]
    fn broken_extends_still_scans_own_tables() {
        let got = summary(
            r#"
[template.a]
extends = "b"
[template.b]
extends = "a"
remoteUser = "root"
[sandbox.web]
extends = "missing"
remoteUser = "root"
"#,
        );
        assert_eq!(
            got,
            vec![
                row("template.b", Kind::RootUser, "remoteUser", "root", &[]),
                row("sandbox.web", Kind::RootUser, "remoteUser", "root", &["web"]),
            ]
        );
    }

    /// [`rewrite`] with every finding of `toml`.
    fn rewrite_all(toml: &str) -> (String, usize) {
        rewrite(toml, &findings(toml)).unwrap()
    }

    #[test]
    fn rewrite_keeps_comments_blank_lines_and_order() {
        let toml = r#"# my config

[template.base]
# the gh login
mounts = [
  "source=${localEnv:HOME}/.config/gh,target=/root/.config/gh,type=bind", # keep me
  'source=/x,target=/opt/x',
]

[sandbox.web]   # web
extends = "base"
image = "img"
containerEnv = { CLAUDE_CONFIG_DIR = "/root/.claude", EDITOR = "vim" } # env

# remote env below
[sandbox.web.remoteEnv]
CACHE = "/root/.cache"  # cache
OTHER = "/opt"
"#;
        let (out, n) = rewrite_all(toml);
        assert_eq!(n, 3);
        let want = r#"# my config

[template.base]
# the gh login
mounts = [
  "source=${localEnv:HOME}/.config/gh,target=${containerEnv:HOME}/.config/gh,type=bind", # keep me
  'source=/x,target=/opt/x',
]

[sandbox.web]   # web
extends = "base"
image = "img"
containerEnv = { CLAUDE_CONFIG_DIR = "${containerEnv:HOME}/.claude", EDITOR = "vim" } # env

# remote env below
[sandbox.web.remoteEnv]
CACHE = "${containerEnv:HOME}/.cache"  # cache
OTHER = "/opt"
"#;
        assert_eq!(out, want);
        assert_eq!(findings(&out), vec![]);
    }

    #[test]
    fn rewrite_object_mounts_in_both_forms() {
        let toml = r#"[sandbox.web]
image = "img"
mounts = [{ source = "/a", target = "/root/.a", readonly = true }, "source=/s,dst=/root/.s"]

[sandbox.api]
image = "img"

# first
[[sandbox.api.mounts]]
source = "/b"
target = "/root/.b" # b
type = "bind"

[[sandbox.api.mounts]]
source = "/c"
target = "/opt/c"
"#;
        let (out, n) = rewrite_all(toml);
        assert_eq!(n, 3);
        let want = r#"[sandbox.web]
image = "img"
mounts = [{ source = "/a", target = "${containerEnv:HOME}/.a", readonly = true }, "source=/s,dst=${containerEnv:HOME}/.s"]

[sandbox.api]
image = "img"

# first
[[sandbox.api.mounts]]
source = "/b"
target = "${containerEnv:HOME}/.b" # b
type = "bind"

[[sandbox.api.mounts]]
source = "/c"
target = "/opt/c"
"#;
        assert_eq!(out, want);
    }

    #[test]
    fn rewrite_a_template_once_for_two_sandboxes() {
        let toml = "[template.base]\nmounts = [\"source=/a,target=/root/.a\"]\n[sandbox.web]\nextends = \"base\"\n[sandbox.api]\nextends = \"base\"\n";
        let found = findings(toml);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].sandboxes, vec!["api", "web"]);
        let (out, n) = rewrite(toml, &found).unwrap();
        assert_eq!(n, 1);
        assert!(out.contains("target=${containerEnv:HOME}/.a"), "{out}");
        // A duplicate entry (one finding) is fixed everywhere it's listed.
        let (out, n) = rewrite_all("[sandbox.web]\nmounts = [\"source=/a,target=/root/.a\", \"source=/a,target=/root/.a\"]\n");
        assert_eq!(n, 1);
        assert!(!out.contains("/root"), "{out}");
    }

    #[test]
    fn rewrite_removes_root_users_and_skips_hand_fixes() {
        let toml = "[sandbox.web]\nimage = \"img\"\n# who\nremoteUser = \"root\"\ncontainerUser = \"root\" # c\nworkspaceFolder = \"/root/app\"\n";
        let (out, n) = rewrite_all(toml);
        assert_eq!(n, 2, "RootPath isn't counted");
        assert_eq!(out, "[sandbox.web]\nimage = \"img\"\nworkspaceFolder = \"/root/app\"\n");
        let left = findings(&out);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].kind, Kind::RootPath);
    }

    #[test]
    fn rewrite_refuses_a_stale_finding() {
        let found = findings("[sandbox.web]\nremoteUser = \"root\"\n");
        let err = rewrite("[sandbox.web]\nremoteUser = \"dev\"\n", &found).unwrap_err().to_string();
        assert_eq!(err, "remoteUser = \"root\" not found in [sandbox.web]");
        let err = rewrite("[sandbox.api]\n", &found).unwrap_err().to_string();
        assert!(err.contains("[sandbox.web]"), "{err}");
    }

    #[test]
    fn keep_root_writes_identity_image_in_each_table_form() {
        let cases = [
            ("[sandbox.web]\nremoteUser = \"root\" # r\n\n[sandbox.api]\n", "[sandbox.web]\nremoteUser = \"root\" # r\nidentity = \"image\"\n\n[sandbox.api]\n"),
            ("[sandbox.web]\nidentity = \"host\" # h\nremoteUser = \"root\"\n", "[sandbox.web]\nidentity = \"image\" # h\nremoteUser = \"root\"\n"),
            ("[sandbox]\nweb = { remoteUser = \"root\" }\n", "[sandbox]\nweb = { remoteUser = \"root\" , identity = \"image\" }\n"),
            ("sandbox.web.remoteUser = \"root\"\n", "sandbox.web.remoteUser = \"root\"\nsandbox.web.identity = \"image\"\n"),
            ("[template.base]\ncontainerUser = \"root\"\n[sandbox.web]\nextends = \"base\"\n", "[template.base]\ncontainerUser = \"root\"\nidentity = \"image\"\n[sandbox.web]\nextends = \"base\"\n"),
        ];
        for (toml, want) in cases {
            let found = findings(toml);
            assert_eq!(found.len(), 1, "{toml}");
            assert_eq!(keep_root(toml, &found[0]).unwrap(), want, "{toml}");
        }
        let env = findings("[sandbox.web]\ncontainerEnv = { X = \"/root\" }\n");
        assert!(keep_root("[sandbox.web]\ncontainerEnv = { X = \"/root\" }\n", &env[0]).is_err());
    }

    #[test]
    fn keep_root_errors_when_a_sandbox_still_says_host() {
        let toml = "[template.base]\nremoteUser = \"root\"\n[sandbox.web]\nextends = \"base\"\nidentity = \"host\"\n";
        let found = findings(toml);
        let err = keep_root(toml, &found[0]).unwrap_err().to_string();
        assert_eq!(err, "the rewrite didn't fix remoteUser in [template.base]");
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("devsandbox-migrate-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const AFFECTED: &str = "# top\n[sandbox.web]\nimage = \"img\"\nremoteUser = \"root\"\nmounts = [\"source=/a,target=/root/.a\"]\n";

    #[test]
    fn migrate_file_writes_and_removes_the_backup() {
        let dir = scratch("ok");
        let path = dir.join("devsandboxes.toml");
        std::fs::write(&path, AFFECTED).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        }
        let got = migrate_file(&path, None, &[], &file_hash(AFFECTED)).unwrap();
        assert_eq!(got, Migrated { edits: 2, file: path.clone() });
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "# top\n[sandbox.web]\nimage = \"img\"\nmounts = [\"source=/a,target=${containerEnv:HOME}/.a\"]\n");
        let mut left: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        left.sort();
        assert_eq!(left, vec!["devsandboxes.toml"], "no .bak, no tmp");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o640);
        }
        // Nothing left to fix: nothing written.
        let got = migrate_file(&path, None, &[], &file_hash(&text)).unwrap();
        assert_eq!(got.edits, 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn migrate_file_picks_ids_and_keeps() {
        let dir = scratch("pick");
        let path = dir.join("devsandboxes.toml");
        std::fs::write(&path, AFFECTED).unwrap();
        let found = findings(AFFECTED);
        let (user, mount) = (found.iter().find(|f| f.kind == Kind::RootUser).unwrap(), found.iter().find(|f| f.kind == Kind::RootTarget).unwrap());
        let hash = file_hash(AFFECTED);
        // Unknown ids, a kept non-user and an id both ways are invalid.
        for (ids, keep) in [
            (Some(vec!["nope".to_string()]), vec![]),
            (None, vec![mount.id.clone()]),
            (Some(vec![user.id.clone()]), vec![user.id.clone()]),
        ] {
            let err = migrate_file(&path, ids.as_deref(), &keep, &hash).unwrap_err();
            assert!(matches!(err, MigrateError::Invalid(_)), "{err}");
        }
        let got = migrate_file(&path, Some(&[]), std::slice::from_ref(&user.id), &hash).unwrap();
        assert_eq!(got.edits, 1);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("remoteUser = \"root\"\nmounts = [\"source=/a,target=/root/.a\"]\nidentity = \"image\"\n"), "{text}");
        assert_eq!(findings(&text), vec![]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn migrate_file_conflict_when_the_file_changed() {
        let dir = scratch("conflict");
        let path = dir.join("devsandboxes.toml");
        std::fs::write(&path, AFFECTED).unwrap();
        let hash = file_hash(AFFECTED);
        std::fs::write(&path, format!("{AFFECTED}# edited\n")).unwrap();
        assert!(matches!(migrate_file(&path, None, &[], &hash), Err(MigrateError::Conflict)));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), format!("{AFFECTED}# edited\n"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn migrate_file_refuses_yaml_with_the_edits() {
        let dir = scratch("yaml");
        let path = dir.join("devsandboxes.yaml");
        let yaml = "sandbox:\n  web:\n    image: img\n    remoteUser: root\n    containerUser: root\n    containerEnv: {X: /root/x}\n    workspaceFolder: /root/app\n";
        std::fs::write(&path, yaml).unwrap();
        let found = identity_findings(&Config::parse_yaml(yaml).unwrap());
        let keep = vec![found.iter().find(|f| f.key == "containerUser").unwrap().id.clone()];
        let Err(MigrateError::Unsupported { edits_text }) = migrate_file(&path, None, &keep, &file_hash(yaml)) else { panic!() };
        assert_eq!(
            edits_text,
            "sandbox.web: containerEnv.X = \"/root/x\" -> \"${containerEnv:HOME}/x\"\n\
             sandbox.web: remove remoteUser = \"root\"\n\
             sandbox.web: set identity = \"image\" (keeps containerUser = \"root\")"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), yaml);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn migrate_file_writes_through_a_symlink() {
        let dir = scratch("link");
        std::fs::create_dir_all(dir.join("real")).unwrap();
        let target = dir.join("real/devsandboxes.toml");
        std::fs::write(&target, AFFECTED).unwrap();
        let link = dir.join("devsandboxes.toml");
        std::os::unix::fs::symlink("real/devsandboxes.toml", &link).unwrap();
        let got = migrate_file(&link, None, &[], &file_hash(AFFECTED)).unwrap();
        assert_eq!(got, Migrated { edits: 2, file: link.clone() });
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(findings(&std::fs::read_to_string(&target).unwrap()), vec![]);
        assert_eq!(std::fs::read_dir(dir.join("real")).unwrap().count(), 1, "no .bak");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn migrate_file_leaves_the_backup_when_the_write_fails() {
        let dir = scratch("fail");
        let path = dir.join("devsandboxes.toml");
        std::fs::write(&path, AFFECTED).unwrap();
        // A dir where the tmp file goes: the write fails after the copy.
        let squat = dir.join(format!(".devsandboxes.toml.{}.tmp", std::process::id()));
        std::fs::create_dir_all(squat.join("x")).unwrap();
        let err = migrate_file(&path, None, &[], &file_hash(AFFECTED)).unwrap_err();
        assert!(matches!(err, MigrateError::Io(_)), "{err}");
        let bak = dir.join("devsandboxes.toml.bak");
        assert!(err.to_string().starts_with(&format!("backup left at {}", bak.display())), "{err}");
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), AFFECTED);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), AFFECTED);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
