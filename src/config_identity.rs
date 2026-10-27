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

#![cfg_attr(not(test), allow(dead_code))] // wired by identity steps 13–14

use toml::{Table, Value};

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
}
