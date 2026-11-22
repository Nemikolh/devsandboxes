//! `devsandbox access ls|approve|revoke` (docs/reference/credentials-cli.md):
//! this home's credentials as `credentials.toml`, the store and `state.toml`
//! hold them; approving a pending claim version after its diff against the
//! latest approved one (github-and-sentry-access, *The claim contract*);
//! dropping a credential or one of its versions; the folders' GitHub repo
//! fences, and `forget` narrowing one (docs/architecture/credentials/fence.md).
//! Local only.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use super::login::{Stdio, Term, approve_or_refuse, data_dir};
use super::status::Envelope;
use super::run::remotes;
use crate::config::{Access, Config};
use crate::credentials::fence;
use crate::credentials::home::{self, Broken, Entry, Home, Version};
use crate::credentials::review;
use crate::credentials::tokens;
use crate::credentials::store::{Record, Store};
use crate::state::{Instance, State};

/// An instance's config `access`, read now; a seam for tests.
type AccessOf<'a> = dyn FnMut(&Instance) -> Result<BTreeMap<String, Access>> + 'a;

fn config_access(inst: &Instance) -> Result<BTreeMap<String, Access>> {
    let Some(dir) = &inst.config_dir else {
        bail!("the instance was recorded without its config root");
    };
    let resolved = Config::load(dir)?.resolve_sandbox(&inst.sandbox)?;
    Ok(resolved.properties.access.unwrap_or_default())
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

pub fn ls(credential: Option<&str>, json: bool) -> Result<()> {
    let state = State::load()?;
    let list = list_at(&data_dir()?, &state, credential, &mut config_access, now())?;
    // Naming a credential asks about it alone.
    let fences = if credential.is_none() { fences(&state) } else { Vec::new() };
    if json {
        let data = Listing { credentials: list, fences };
        println!("{}", serde_json::to_string_pretty(&Envelope::new(&data)).context("serialize access ls")?);
    } else {
        print!("{}", render(&list, now()));
        print!("{}", render_fences(&fences));
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct Listing {
    credentials: Vec<Credential>,
    fences: Vec<Fence>,
}

/// One folder's GitHub repo fence as recorded (`State::folder_remotes`).
#[derive(Debug, PartialEq, Serialize)]
struct Fence {
    folder: String,
    repos: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unread: Option<String>,
    instances: Vec<FenceUse>,
}

#[derive(Debug, PartialEq, Serialize)]
struct FenceUse {
    instance: String,
    /// Its snapshot (`Instance::github_repos`) differs from its folders'
    /// records: an `access forget` it takes at its next start or rebuild.
    stale: bool,
}

fn fences(state: &State) -> Vec<Fence> {
    state
        .folder_remotes
        .iter()
        .map(|(folder, record)| Fence {
            folder: folder.clone(),
            repos: record.github.clone(),
            unread: record.unread.clone(),
            instances: state
                .instances
                .iter()
                .filter(|(_, i)| remotes::instance_folders(i).any(|f| remotes::folder_key(f) == *folder))
                .map(|(key, i)| FenceUse {
                    instance: key.clone(),
                    stale: i.github_repos != remotes::fence_of(state, remotes::instance_folders(i)),
                })
                .collect(),
        })
        .collect()
}

fn render_fences(fences: &[Fence]) -> String {
    if fences.is_empty() {
        return String::new();
    }
    let mut out = "\nGitHub repo fences (each folder's remotes at its first run):\n".to_string();
    for f in fences {
        let repos = if f.repos.is_empty() { "none".to_string() } else { f.repos.join(", ") };
        out.push_str(&format!("  {}  {repos}", f.folder));
        if let Some(why) = &f.unread {
            out.push_str(&format!("  ({why})"));
        }
        out.push('\n');
        if !f.instances.is_empty() {
            let one = |u: &FenceUse| {
                if u.stale { format!("{} (until its next start)", u.instance) } else { u.instance.clone() }
            };
            out.push_str(&format!("    instances  {}\n", f.instances.iter().map(one).collect::<Vec<_>>().join(", ")));
        }
    }
    out
}

pub fn forget(folder: &Path, repo: &str) -> Result<()> {
    println!("{}", forget_at(&State::path()?, folder, repo)?);
    Ok(())
}

/// Drop `repo` from `folder`'s recorded GitHub fence in the state file
/// `state`. Instances keep their snapshot until their next start or
/// rebuild. Returns the line to print.
pub(crate) fn forget_at(state: &Path, folder: &Path, repo: &str) -> Result<String> {
    let repo = fence::repo_arg(repo)?;
    // A folder since moved or deleted is named as recorded.
    let key = remotes::folder_key(&folder.canonicalize().unwrap_or_else(|_| folder.to_path_buf()));
    State::update_at(state, |s| {
        let Some(record) = s.folder_remotes.get_mut(&key) else {
            bail!("no remotes recorded for `{key}`: no instance has run on it");
        };
        let Some(at) = record.github.iter().position(|r| *r == repo) else {
            bail!("`{repo}` isn't in `{key}`'s GitHub fence (it holds: {})", or_none(&record.github.join(", ")));
        };
        record.github.remove(at);
        let users: Vec<&str> = s
            .instances
            .iter()
            .filter(|(_, i)| remotes::instance_folders(i).any(|f| remotes::folder_key(f) == key))
            .map(|(k, _)| k.as_str())
            .collect();
        let mut done = format!("dropped `{repo}` from `{key}`'s GitHub fence");
        if !users.is_empty() {
            done.push_str(&format!("; these instances take it at their next start or rebuild: {}", users.join(", ")));
        }
        Ok(done)
    })
}

/// One credential as `access ls` shows it: never a secret.
#[derive(Debug, Serialize)]
struct Credential {
    name: String,
    kind: String,
    source: String,
    token: Token,
    /// `None`: every profile.
    profiles: Option<Vec<String>>,
    /// `None`: every scope.
    max: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    members: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    approvers: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    delegate: Vec<String>,
    #[serde(rename = "as", skip_serializing_if = "Vec::is_empty")]
    as_: Vec<String>,
    approved: Vec<VersionRow>,
    pending: Vec<VersionRow>,
    broken: Option<Broken>,
    /// When the secret stops working, if known (`tokens::ends_at`).
    #[serde(skip_serializing_if = "Option::is_none")]
    expires: Option<Expires>,
    instances: Vec<Use>,
}

#[derive(Debug, PartialEq, Serialize)]
struct Expires {
    /// Unix seconds.
    at: u64,
    days: u64,
    expired: bool,
    /// Within `tokens::WARN_DAYS`.
    warn: bool,
}

#[derive(Debug, Serialize)]
struct VersionRow {
    version: String,
    /// The last approved: what a new instance's claim is diffed against.
    latest: bool,
    /// Neither latest nor pinned by an instance here: safe to drop.
    unused: bool,
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum Token {
    Static,
    #[serde(rename = "oauth")]
    OAuth {
        expires_at: Option<u64>,
        refresh_expires_at: Option<u64>,
        refreshable: bool,
        scope: Option<String>,
    },
    Missing,
    Unreadable { error: String },
}

/// An instance on this host whose claim resolves to the credential.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct Use {
    instance: String,
    /// The claim version it pinned at `run`.
    version: String,
    approved: bool,
    /// Why the credential it takes can't be told (its config doesn't load,
    /// `home::resolve` refuses): it may take this one, or another of the kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    unknown: Option<String>,
}

fn short(version: &str) -> &str {
    &version[..version.len().min(12)]
}

/// Every instance claim in `state`, under the credential it takes from
/// `home` as the broker resolves it per call; one that can't be resolved
/// goes under every credential of its kind, marked unknown.
fn uses(home: &Home, state: &State, access_of: &mut AccessOf) -> BTreeMap<String, Vec<Use>> {
    let mut out: BTreeMap<String, Vec<Use>> = BTreeMap::new();
    for (key, inst) in state.instances.iter().filter(|(_, i)| !i.claims.is_empty()) {
        let access = access_of(inst).map_err(|e| format!("cannot read its config: {e:#}"));
        for claim in &inst.claims {
            let kind = claim.kind.as_str();
            let found = access.as_ref().map_err(Clone::clone).and_then(|a| {
                let named = a.get(kind).and_then(|a| a.credential.as_deref());
                home::resolve(home, kind, named, inst.profile.as_deref()).map_err(|e| format!("{e:#}"))
            });
            let entries: Vec<(&str, &Entry, Option<String>)> = match found {
                Ok(None) => Vec::new(),
                Ok(Some((name, entry))) => vec![(name, entry, None)],
                Err(why) => home
                    .iter()
                    .filter(|(_, e)| e.kind == kind)
                    .map(|(n, e)| (n.as_str(), e, Some(why.clone())))
                    .collect(),
            };
            for (name, entry, unknown) in entries {
                out.entry(name.to_string()).or_default().push(Use {
                    instance: key.clone(),
                    version: claim.version.clone(),
                    approved: entry.approved_claim(&claim.version).is_some(),
                    unknown,
                });
            }
        }
    }
    out
}

fn token(record: &Result<Option<Record>>) -> Token {
    match record {
        Err(e) => Token::Unreadable { error: format!("{e:#}") },
        Ok(None) => Token::Missing,
        Ok(Some(Record::Static { .. })) => Token::Static,
        Ok(Some(Record::OAuth(t))) => Token::OAuth {
            expires_at: t.expires_at,
            refresh_expires_at: t.refresh_expires_at,
            refreshable: t.refresh_token.is_some(),
            scope: t.scope.clone(),
        },
    }
}

fn credential(name: &str, entry: &Entry, record: Result<Option<Record>>, uses: Vec<Use>, now: u64) -> Credential {
    let expires = tokens::ends_at(record.as_ref().ok().and_then(Option::as_ref), entry.expires()).map(|at| {
        let l = tokens::left(at, now);
        Expires { at, days: l.days, expired: l.expired, warn: l.warn }
    });
    let pinned = |v: &Version| uses.iter().any(|u| u.version == v.version);
    let latest = entry.latest_approved().map(|v| v.version.as_str());
    let row = |v: &Version| {
        let latest = latest == Some(v.version.as_str());
        VersionRow { version: v.version.clone(), latest, unused: !latest && !pinned(v) }
    };
    Credential {
        name: name.to_string(),
        kind: entry.kind.clone(),
        source: entry.source.into(),
        token: token(&record),
        profiles: entry.profiles.clone(),
        max: entry.max.clone(),
        members: entry.members.clone(),
        approvers: entry.approvers.clone(),
        delegate: entry.delegate.clone(),
        as_: entry.as_.clone(),
        approved: entry.approved.iter().map(row).collect(),
        pending: entry.pending.iter().map(row).collect(),
        broken: entry.broken.clone(),
        expires,
        instances: uses,
    }
}

fn list_at(
    data: &Path,
    state: &State,
    only: Option<&str>,
    access_of: &mut AccessOf,
    now: u64,
) -> Result<Vec<Credential>> {
    let path = home::path(data);
    let home = home::load(&path)?;
    if let Some(name) = only.filter(|n| !home.contains_key(*n)) {
        bail!("no credential `{name}` in {}", path.display());
    }
    let mut uses = uses(&home, state, access_of);
    let store = Store::in_data(data);
    Ok(home
        .iter()
        .filter(|(n, _)| only.is_none_or(|o| o == n.as_str()))
        .map(|(n, e)| credential(n, e, store.get(n), uses.remove(n).unwrap_or_default(), now))
        .collect())
}

fn render_token(token: &Token, name: &str, now: u64) -> String {
    let at = |what: &str, past: &str, t: u64| {
        let verb = if t <= now { past } else { what };
        format!("{verb} {}", crate::inbox::rfc3339(t))
    };
    match token {
        Token::Static => "static".into(),
        Token::Missing => format!("none stored: `devsandbox login {name}`"),
        Token::Unreadable { error } => format!("unreadable: {error}"),
        Token::OAuth { expires_at, refresh_expires_at, refreshable, scope } => {
            let mut out = vec!["oauth".to_string()];
            out.push(expires_at.map_or("no expiry".into(), |t| at("expires", "expired", t)));
            out.push(match (refreshable, refresh_expires_at) {
                (false, _) => "no refresh token".into(),
                (true, None) => "refreshable".into(),
                (true, Some(t)) => at("refreshable until", "refresh token expired", *t),
            });
            out.extend(scope.as_ref().map(|s| format!("scope `{s}`")));
            out.join(", ")
        }
    }
}

fn render_versions(rows: &[VersionRow]) -> String {
    let one = |r: &VersionRow| {
        let marks: Vec<&str> = [(r.latest, "latest"), (r.unused, "unused")]
            .into_iter()
            .filter_map(|(on, m)| on.then_some(m))
            .collect();
        match marks.as_slice() {
            [] => short(&r.version).to_string(),
            m => format!("{} ({})", short(&r.version), m.join(", ")),
        }
    };
    if rows.is_empty() { "none".into() } else { rows.iter().map(one).collect::<Vec<_>>().join(", ") }
}

fn render(list: &[Credential], now: u64) -> String {
    if list.is_empty() {
        return "no credentials on this host: `devsandbox login <credential>` adds one\n".into();
    }
    let mut blocks = Vec::new();
    for c in list {
        let mut out = format!("{}: {}, source {}\n", c.name, c.kind, c.source);
        let line = |out: &mut String, label: &str, value: &str| out.push_str(&format!("  {label:<10}{value}\n"));
        let all = |l: &Option<Vec<String>>, every: &str| l.as_ref().map_or(every.to_string(), |l| l.join(", "));
        line(&mut out, "token", &render_token(&c.token, &c.name, now));
        line(&mut out, "profiles", &all(&c.profiles, "every profile"));
        line(&mut out, "max", &all(&c.max, "every scope"));
        for (label, list) in [("members", &c.members), ("approvers", &c.approvers), ("delegate", &c.delegate), ("as", &c.as_)] {
            if !list.is_empty() {
                line(&mut out, label, &list.join(", "));
            }
        }
        line(&mut out, "approved", &render_versions(&c.approved));
        if !c.pending.is_empty() {
            line(&mut out, "pending", &render_versions(&c.pending));
        }
        if let Some(b) = &c.broken {
            line(&mut out, "broken", &format!("{} (since {}): `devsandbox login {}`", b.reason, b.at, c.name));
        }
        if let Some(e) = &c.expires {
            let left = tokens::Left { days: e.days, expired: e.expired, warn: e.warn };
            let mut text = format!("{} ({})", tokens::in_days(left), crate::inbox::rfc3339(e.at));
            if e.warn || e.expired {
                text = format!("warning: {text}: `devsandbox login {}` replaces it", c.name);
            }
            line(&mut out, "expires", &text);
        }
        if c.instances.is_empty() {
            line(&mut out, "instances", "none here");
        } else {
            out.push_str("  instances\n");
            let width = c.instances.iter().map(|u| u.instance.len()).max().unwrap_or(0);
            for u in &c.instances {
                let approved = if u.approved { "approved" } else { "not approved" };
                out.push_str(&format!("    {:<width$}  {}  {approved}", u.instance, short(&u.version)));
                if let Some(why) = &u.unknown {
                    out.push_str(&format!("  (may take another credential: {why})"));
                }
                out.push('\n');
            }
        }
        blocks.push(out);
    }
    blocks.join("\n")
}

pub fn revoke(credential: &str, version: Option<&str>, yes: bool) -> Result<()> {
    let done = revoke_at(&data_dir()?, &State::load()?, credential, version, yes, &mut Stdio, &mut config_access)?;
    println!("{done}");
    Ok(())
}

/// `--yes` revokes; else ask on a terminal, refuse off one.
fn confirm_revoke(term: &mut dyn Term, yes: bool, question: &str) -> Result<()> {
    if yes {
        return Ok(());
    }
    if !term.tty() {
        bail!("no terminal to confirm on: pass --yes to revoke");
    }
    if !term.confirm(question)? {
        bail!("not revoked: nothing changed");
    }
    Ok(())
}

fn instance_list(uses: &[&Use]) -> String {
    let one = |u: &&Use| match &u.unknown {
        None => u.instance.clone(),
        Some(_) => format!("{} (perhaps: its credential can't be told)", u.instance),
    };
    uses.iter().map(one).collect::<Vec<_>>().join(", ")
}

/// The whole credential, or one claim version of it. Returns the line to
/// print.
pub(crate) fn revoke_at(
    data: &Path,
    state: &State,
    name: &str,
    version: Option<&str>,
    yes: bool,
    term: &mut dyn Term,
    access_of: &mut AccessOf,
) -> Result<String> {
    let path = home::path(data);
    let home = home::load(&path)?;
    let entry = home.get(name).with_context(|| format!("no credential `{name}` in {}", path.display()))?;
    let uses = uses(&home, state, access_of).remove(name).unwrap_or_default();
    match version {
        None => revoke_credential(data, name, entry, &uses, yes, term),
        Some(v) => revoke_version(&path, name, entry, &uses, v, yes, term),
    }
}

fn revoke_credential(data: &Path, name: &str, entry: &Entry, uses: &[Use], yes: bool, term: &mut dyn Term) -> Result<String> {
    let store = Store::in_data(data);
    let oauth = matches!(store.get(name), Ok(Some(Record::OAuth(_))));
    let mut text = format!("credential `{name}` (`{}`): its secret and its entry in credentials.toml go\n", entry.kind);
    let losing: Vec<&Use> = uses.iter().collect();
    if losing.is_empty() {
        text.push_str("no instance here takes it\n");
    } else {
        text.push_str(&format!("instances that lose it: {}\n", instance_list(&losing)));
    }
    term.show(&text);
    confirm_revoke(term, yes, &format!("Revoke `{name}`?"))?;
    // The secret first: an entry left by a failure serves nothing.
    store.remove(name)?;
    home::update(&home::path(data), |h| Ok(h.remove(name).is_some()))?;
    let mut done = format!("revoked `{name}`");
    if oauth {
        // No claim declares a revocation endpoint to call.
        done.push_str(": its OAuth token stays valid at the provider until it expires (revoke the app's access there)");
    }
    Ok(done)
}

fn revoke_version(
    path: &Path,
    name: &str,
    entry: &Entry,
    uses: &[Use],
    prefix: &str,
    yes: bool,
    term: &mut dyn Term,
) -> Result<String> {
    let (list, version) = pick_any(name, entry, prefix)?;
    let short = version.short().to_string();
    let mut text = format!("{list} claim version {short} of `{name}` goes\n");
    let pinning: Vec<&Use> = uses.iter().filter(|u| u.version == version.version).collect();
    if list == "approved" {
        if !pinning.is_empty() {
            text.push_str(&format!(
                "instances pinning it get the not-approved refusal on their next call: {}\n",
                instance_list(&pinning)
            ));
        }
        if entry.latest_approved().is_some_and(|l| l.version == version.version) {
            let before = entry.approved.iter().rev().nth(1);
            text.push_str(&match before {
                Some(v) => format!("the latest approved version becomes {}\n", v.short()),
                None => "no approved version is left\n".into(),
            });
        }
    }
    term.show(&text);
    confirm_revoke(term, yes, &format!("Drop claim version {short} of `{name}`?"))?;
    home::update(path, |h| {
        let e = h.get_mut(name).with_context(|| format!("credential `{name}` is gone"))?;
        let before = e.approved.len() + e.pending.len();
        e.approved.retain(|v| v.version != version.version);
        e.pending.retain(|v| v.version != version.version);
        if e.approved.len() + e.pending.len() == before {
            bail!("claim version {short} of `{name}` is already gone");
        }
        Ok(())
    })?;
    Ok(format!("dropped {list} claim version {short} of `{name}`"))
}

/// The approved or pending version `prefix` starts, with its list's name.
fn pick_any<'a>(name: &str, entry: &'a Entry, prefix: &str) -> Result<(&'static str, &'a Version)> {
    let all = || entry.approved.iter().map(|v| ("approved", v)).chain(entry.pending.iter().map(|v| ("pending", v)));
    let found: Vec<_> = all().filter(|(_, v)| !prefix.is_empty() && v.version.starts_with(prefix)).collect();
    match found.as_slice() {
        [one] => Ok(*one),
        [] => {
            let list = all().map(|(_, v)| v.short()).collect::<Vec<_>>().join(", ");
            bail!("no claim version `{prefix}` of `{name}` (versions: {})", or_none(&list))
        }
        _ => bail!("`{prefix}` names several claim versions of `{name}`: give more of it"),
    }
}

pub fn approve(credential: &str, version: Option<&str>, yes: bool) -> Result<()> {
    let done = approve_at(&data_dir()?, credential, version, yes, &mut Stdio)?;
    println!("{done}");
    Ok(())
}

/// Returns the line to print.
pub(crate) fn approve_at(data: &Path, name: &str, version: Option<&str>, yes: bool, term: &mut dyn Term) -> Result<String> {
    let path = home::path(data);
    let entry = home::load(&path)?
        .remove(name)
        .with_context(|| format!("no credential `{name}` in {}", path.display()))?;
    let pending = pick_pending(name, &entry.pending, version)?;
    let new = pending.view()?;
    let text = match entry.latest_approved() {
        Some(latest) => {
            let old = latest.view()?;
            let diff = review::diff(&old, &new).render(&old, &new);
            format!("claim `{}` version {}, against approved {}:\n{diff}", new.kind, pending.short(), latest.short())
        }
        None => format!("{}  version {}\n", new.summary(), pending.short()),
    };
    term.show(&text);
    approve_or_refuse(term, yes, &format!("Approve this claim version for `{name}`?"))?;
    home::update(&path, |h| {
        let entry = h.get_mut(name).with_context(|| format!("credential `{name}` is gone"))?;
        if !entry.pending.iter().any(|p| p.version == pending.version) {
            bail!("claim version {} of `{name}` is no longer pending", pending.short());
        }
        Ok(entry.approve(pending.clone()))
    })?;
    Ok(format!("approved claim version {} for `{name}`", pending.short()))
}

/// `version` (a prefix of one), else the only one.
fn pick_pending<'a>(name: &str, pending: &'a [Version], version: Option<&str>) -> Result<&'a Version> {
    let list = || pending.iter().map(Version::short).collect::<Vec<_>>().join(", ");
    match version {
        Some(v) => {
            let found: Vec<&Version> = pending.iter().filter(|p| !v.is_empty() && p.version.starts_with(v)).collect();
            match found.as_slice() {
                [one] => Ok(one),
                [] => bail!("no pending claim version `{v}` for `{name}` (pending: {})", or_none(&list())),
                _ => bail!("`{v}` names several pending claim versions of `{name}`: give more of it"),
            }
        }
        None => match pending {
            [] => bail!("nothing pending for `{name}`"),
            [one] => Ok(one),
            _ => bail!("`{name}` has {} pending claim versions ({}): pass --version", pending.len(), list()),
        },
    }
}

fn or_none(list: &str) -> &str {
    if list.is_empty() { "none" } else { list }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::login::tests::{Script, temp};
    use crate::credentials::home::Entry;
    use crate::credentials::home::tests::sentry;
    use crate::credentials::store::{Record, Secret, Store};

    #[test]
    fn approves_the_pending_version_after_its_diff() {
        let data = temp("approve");
        let path = home::path(&data);
        assert!(approve_at(&data, "s", None, true, &mut Script::new(true, &[], None)).is_err());
        Store::in_data(&data).put("s", &Record::token(Secret::new("keep".into()))).unwrap();
        let mut newer = sentry();
        newer.pinned.never.pop();
        home::update(&path, |h| {
            let mut e = Entry::new("sentry", None);
            e.approve(Version::of(&sentry()));
            h.insert("s".into(), e);
            Ok(())
        })
        .unwrap();
        let err = approve_at(&data, "s", None, true, &mut Script::new(true, &[], None)).unwrap_err();
        assert!(err.to_string().contains("nothing pending"), "{err}");
        home::update(&path, |h| home::record_pending(h, "s", &newer.canonical())).unwrap();

        let err = approve_at(&data, "s", None, false, &mut Script::new(false, &[], None)).unwrap_err();
        assert!(err.to_string().contains("pass --yes"), "{err}");
        let mut term = Script::new(true, &[false], None);
        assert!(approve_at(&data, "s", None, false, &mut term).is_err());
        assert!(term.shown.contains("  never:\n    - "), "{}", term.shown);
        assert_eq!(home::load(&path).unwrap()["s"].pending.len(), 1);

        let done = approve_at(&data, "s", Some(&newer.version()[..8]), false, &mut Script::new(true, &[true], None)).unwrap();
        assert!(done.starts_with("approved claim version"), "{done}");
        let e = &home::load(&path).unwrap()["s"];
        assert!(e.pending.is_empty());
        assert_eq!(e.latest_approved().unwrap().version, newer.version());
        assert_eq!(e.approved.len(), 2, "the older version stays for instances pinned to it");
        assert_eq!(Store::in_data(&data).token_now("s").unwrap().unwrap().expose(), "keep");
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn picks_a_pending_version() {
        let v = |c: &str| Version { version: c.into(), claim: String::new() };
        let two = [v("abc1"), v("abd2")];
        assert_eq!(pick_pending("s", &two[..1], None).unwrap().version, "abc1");
        assert!(pick_pending("s", &two, None).unwrap_err().to_string().contains("pass --version"));
        assert_eq!(pick_pending("s", &two, Some("abd")).unwrap().version, "abd2");
        assert!(pick_pending("s", &two, Some("ab")).unwrap_err().to_string().contains("several"));
        assert!(pick_pending("s", &two, Some("")).is_err());
        assert!(pick_pending("s", &[], Some("x")).unwrap_err().to_string().contains("(pending: none)"));
    }

    use crate::broker::policy::tests::{claims, instance};
    use crate::credentials::claim::Claim;
    use crate::credentials::store::OAuthToken;

    /// `sentry` with `n` `never` routes fewer: another claim version.
    fn trimmed(n: usize) -> Claim {
        let mut c = sentry();
        for _ in 0..n {
            c.pinned.never.pop();
        }
        c
    }

    fn with_claim(key: &str, claim: &Claim, profile: Option<&str>) -> Instance {
        let mut inst = instance(key, claims(std::slice::from_ref(claim), &["sntrys_x"]));
        inst.profile = profile.map(String::from);
        inst
    }

    /// `sentry-work` (profile `work`, approved v0 then v1, v2 pending),
    /// `sentry-oss` (profile `oss`), `gh`.
    fn home_fixture() -> Home {
        let mut work = Entry::new("sentry", Some(vec!["work".into()]));
        work.approve(Version::of(&trimmed(0)));
        work.approve(Version::of(&trimmed(1)));
        work.pending.push(Version::of(&trimmed(2)));
        Home::from([
            ("sentry-work".to_string(), work),
            ("sentry-oss".to_string(), Entry::new("sentry", Some(vec!["oss".into()]))),
            ("gh".to_string(), Entry::new("github", None)),
        ])
    }

    /// `w0` pins v0 and `w2` v2 (both `work`); `named` names `sentry-oss`;
    /// `lost` has a config that won't load; `stray` is in no granted profile.
    fn state_fixture() -> State {
        let mut state = State::default();
        for (key, claim, profile) in [
            ("w0", trimmed(0), Some("work")),
            ("w2", trimmed(2), Some("work")),
            ("named", trimmed(1), Some("work")),
            ("lost", trimmed(1), Some("work")),
            ("stray", trimmed(1), Some("home")),
        ] {
            state.instances.insert(key.into(), with_claim(key, &claim, profile));
        }
        state.instances.insert("plain".into(), instance("plain", Vec::new()));
        state
    }

    fn access_fixture(inst: &Instance) -> Result<BTreeMap<String, Access>> {
        match inst.instance_id.as_str() {
            "lost-id" => bail!("no devsandboxes.toml"),
            "named-id" => Ok(BTreeMap::from([(
                "sentry".to_string(),
                toml::from_str::<Access>("credential = \"sentry-oss\"").unwrap(),
            )])),
            _ => Ok(BTreeMap::new()),
        }
    }

    fn data_fixture(tag: &str) -> std::path::PathBuf {
        let data = temp(tag);
        let home = home_fixture();
        home::update(&home::path(&data), |h| {
            *h = home;
            Ok(())
        })
        .unwrap();
        data
    }

    const STATIC: &str = "sntrys_REALSECRET";

    fn oauth_record() -> Record {
        Record::OAuth(OAuthToken {
            access_token: Secret::new("ghu_REALACCESS".into()),
            refresh_token: Some(Secret::new("ghr_REALREFRESH".into())),
            expires_at: Some(1_800_000_000),
            refresh_expires_at: Some(1_900_000_000),
            token_url: "https://github.com/login/oauth/access_token".into(),
            client_id: "Iv1.abc".into(),
            scope: None,
        })
    }

    #[test]
    fn instances_map_to_the_entry_they_take() {
        let uses = uses(&home_fixture(), &state_fixture(), &mut access_fixture);
        let row = |u: &Use| (u.instance.clone(), u.version == trimmed(0).version(), u.approved, u.unknown.is_some());
        let work: Vec<_> = uses["sentry-work"].iter().map(row).collect();
        assert_eq!(work, [("lost".into(), false, true, true), ("w0".into(), true, true, false), ("w2".into(), false, false, false)]);
        let oss: Vec<_> = uses["sentry-oss"].iter().map(row).collect();
        assert_eq!(oss, [("lost".into(), false, false, true), ("named".into(), false, false, false)]);
        assert!(uses["sentry-work"][0].unknown.as_ref().unwrap().contains("cannot read its config: no devsandboxes.toml"));
        assert!(!uses.contains_key("gh"));
    }

    #[test]
    fn lists_with_unused_versions_and_renders() {
        let data = data_fixture("ls");
        Store::in_data(&data).put("sentry-work", &Record::token(Secret::new(STATIC.into()))).unwrap();
        let mut state = state_fixture();
        state.instances.remove("lost");
        let list = list_at(&data, &state, Some("sentry-work"), &mut access_fixture, 0).unwrap();
        let c = &list[0];
        let marks: Vec<(bool, bool)> = c.approved.iter().chain(&c.pending).map(|r| (r.latest, r.unused)).collect();
        assert_eq!(marks, [(false, false), (true, false), (false, false)], "v0 and v2 pinned, v1 latest");

        state.instances.remove("w0");
        state.instances.remove("w2");
        let list = list_at(&data, &state, None, &mut access_fixture, 0).unwrap();
        assert_eq!(list.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["gh", "sentry-oss", "sentry-work"]);
        let c = &list[2];
        let marks: Vec<(bool, bool)> = c.approved.iter().chain(&c.pending).map(|r| (r.latest, r.unused)).collect();
        assert_eq!(marks, [(false, true), (true, false), (false, true)]);

        let (v0, v1, v2) = (trimmed(0).version(), trimmed(1).version(), trimmed(2).version());
        let text = render(&list, 0);
        let want = format!(
            "gh: github, source file\n  token     none stored: `devsandbox login gh`\n  profiles  every profile\n  \
             max       every scope\n  approved  none\n  instances none here\n\n\
             sentry-oss: sentry, source file\n  token     none stored: `devsandbox login sentry-oss`\n  \
             profiles  oss\n  max       every scope\n  approved  none\n  instances\n    named  {}  not approved\n\n\
             sentry-work: sentry, source file\n  token     static\n  profiles  work\n  max       every scope\n  \
             approved  {} (unused), {} (latest)\n  pending   {} (unused)\n  instances none here\n",
            &v1[..12],
            &v0[..12],
            &v1[..12],
            &v2[..12]
        );
        assert_eq!(text, want);
        assert!(!text.contains(STATIC));
        assert_eq!(render(&[], 0), "no credentials on this host: `devsandbox login <credential>` adds one\n");

        let err = list_at(&data, &state, Some("nope"), &mut access_fixture, 0).unwrap_err().to_string();
        assert!(err.contains("no credential `nope`"), "{err}");
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn unknown_instances_render_and_keep_versions_used() {
        let data = data_fixture("ls-unknown");
        let mut state = State::default();
        state.instances.insert("lost".into(), with_claim("lost", &trimmed(0), Some("work")));
        let list = list_at(&data, &state, Some("sentry-work"), &mut access_fixture, 0).unwrap();
        assert!(!list[0].approved[0].unused, "an unknown instance may pin it");
        let text = render(&list, 0);
        assert!(
            text.contains(&format!(
                "  instances\n    lost  {}  approved  (may take another credential: cannot read its config: no devsandboxes.toml)\n",
                &trimmed(0).version()[..12]
            )),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn json_shape_and_oauth_tokens_without_values() {
        let data = data_fixture("ls-json");
        Store::in_data(&data).put("gh", &oauth_record()).unwrap();
        home::update(&home::path(&data), |h| {
            let gh = h.get_mut("gh").unwrap();
            gh.max = Some(vec!["repo.read".into()]);
            gh.approvers = vec!["laptop".into()];
            Ok(())
        })
        .unwrap();
        let list = list_at(&data, &state_fixture(), Some("gh"), &mut access_fixture, 0).unwrap();
        let json = serde_json::to_value(Envelope::new(&list)).unwrap();
        let c = &json["data"][0];
        assert_eq!(
            *c,
            serde_json::json!({
                "name": "gh", "kind": "github", "source": "file",
                "token": {"type": "oauth", "expires_at": 1_800_000_000u64, "refresh_expires_at": 1_900_000_000u64,
                          "refreshable": true, "scope": null},
                "profiles": null, "max": ["repo.read"], "approvers": ["laptop"],
                "approved": [], "pending": [], "broken": null,
                "expires": {"at": 1_900_000_000u64, "days": 21990, "expired": false, "warn": false},
                "instances": [],
            })
        );
        let text = serde_json::to_string(&json).unwrap() + &render(&list, 0);
        for secret in ["ghu_REALACCESS", "ghr_REALREFRESH"] {
            assert!(!text.contains(secret), "{text}");
        }

        let broken = Broken { at: "2026-11-21T10:00:00Z".into(), reason: "its refresh was refused".into() };
        home::mark_broken(&home::path(&data), "sentry-work", broken).unwrap();
        let list = list_at(&data, &state_fixture(), Some("sentry-work"), &mut access_fixture, 0).unwrap();
        let json = serde_json::to_value(&list[0]).unwrap();
        assert_eq!(json["broken"], serde_json::json!({"at": "2026-11-21T10:00:00Z", "reason": "its refresh was refused"}));
        let text = render(&list, 0);
        assert!(
            text.contains(
                "  broken    its refresh was refused (since 2026-11-21T10:00:00Z): `devsandbox login sentry-work`\n"
            ),
            "{text}"
        );
        assert_eq!(json["token"], serde_json::json!({"type": "missing"}));
        assert_eq!(json["approved"][1], serde_json::json!({"version": trimmed(1).version(), "latest": true, "unused": false}));
        assert_eq!(json["instances"][1], serde_json::json!({"instance": "w0", "version": trimmed(0).version(), "approved": true}));
        assert!(json["instances"][0]["unknown"].as_str().unwrap().contains("cannot read its config"));
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn expiry_is_shown_and_warned_of_within_fourteen_days() {
        let data = data_fixture("ls-expiry");
        let now = 1_800_000_000;
        let day = 86_400;
        Store::in_data(&data).put("sentry-work", &Record::token(Secret::new(STATIC.into()))).unwrap();
        let show = |data: &Path, now: u64| {
            let list = list_at(data, &State::default(), Some("sentry-work"), &mut access_fixture, now).unwrap();
            let text = render(&list, now);
            (serde_json::to_value(&list[0]).unwrap()["expires"].clone(), text)
        };
        let (json, text) = show(&data, now);
        assert_eq!(json, serde_json::Value::Null, "a pasted token with no reported expiry");
        assert!(!text.contains("expires"), "{text}");

        home::set_expires_at(&home::path(&data), "sentry-work", now + 30 * day).unwrap();
        let (json, text) = show(&data, now);
        assert_eq!(json, serde_json::json!({"at": now + 30 * day, "days": 30, "expired": false, "warn": false}));
        assert!(text.contains("  expires   in 30 days (2027-02-14T08:00:00Z)\n"), "{text}");
        let (json, text) = show(&data, now + 20 * day);
        assert_eq!(json["warn"], true);
        assert!(
            text.contains(
                "  expires   warning: in 10 days (2027-02-14T08:00:00Z): `devsandbox login sentry-work` replaces it\n"
            ),
            "{text}"
        );
        let (json, text) = show(&data, now + 31 * day);
        assert_eq!((json["expired"].clone(), json["warn"].clone()), (true.into(), false.into()));
        assert!(text.contains("  expires   warning: expired (2027-02-14T08:00:00Z)"), "{text}");

        // An OAuth record's end is its refresh token's, whatever was recorded.
        Store::in_data(&data).put("sentry-work", &oauth_record()).unwrap();
        let (json, _) = show(&data, 1_900_000_000 - 3 * day);
        assert_eq!(json, serde_json::json!({"at": 1_900_000_000u64, "days": 3, "expired": false, "warn": true}));
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn oauth_token_lines() {
        let t = |expires_at, refresh_expires_at, refreshable, scope: Option<&str>| Token::OAuth {
            expires_at,
            refresh_expires_at,
            refreshable,
            scope: scope.map(String::from),
        };
        let now = 1_800_000_000;
        assert_eq!(
            render_token(&t(Some(now + 3600), Some(now + 86_400), true, Some("repo")), "gh", now),
            "oauth, expires 2027-01-15T09:00:00Z, refreshable until 2027-01-16T08:00:00Z, scope `repo`"
        );
        assert_eq!(
            render_token(&t(Some(now), Some(now - 1), true, None), "gh", now),
            "oauth, expired 2027-01-15T08:00:00Z, refresh token expired 2027-01-15T07:59:59Z"
        );
        assert_eq!(render_token(&t(None, None, false, None), "gh", now), "oauth, no expiry, no refresh token");
        assert_eq!(render_token(&t(None, None, true, None), "gh", now), "oauth, no expiry, refreshable");
        assert_eq!(render_token(&Token::Unreadable { error: "bad".into() }, "gh", now), "unreadable: bad");
    }

    #[test]
    fn revokes_a_credential() {
        let data = data_fixture("revoke");
        let store = Store::in_data(&data);
        store.put("sentry-work", &Record::token(Secret::new(STATIC.into()))).unwrap();
        store.put("gh", &oauth_record()).unwrap();
        let state = state_fixture();
        let revoke = |name, yes, term: &mut Script| revoke_at(&data, &state, name, None, yes, term, &mut access_fixture);

        let err = revoke("sentry-work", false, &mut Script::new(false, &[], None)).unwrap_err().to_string();
        assert!(err.contains("pass --yes to revoke"), "{err}");
        let mut term = Script::new(true, &[false], None);
        let err = revoke("sentry-work", false, &mut term).unwrap_err().to_string();
        assert!(err.contains("not revoked"), "{err}");
        assert!(
            term.shown.contains("instances that lose it: lost (perhaps: its credential can't be told), w0, w2\n"),
            "{}",
            term.shown
        );
        assert!(store.get("sentry-work").unwrap().is_some());
        assert!(home::load(&home::path(&data)).unwrap().contains_key("sentry-work"));

        let mut term = Script::new(true, &[true], None);
        let done = revoke("sentry-work", false, &mut term).unwrap();
        assert_eq!(done, "revoked `sentry-work`");
        assert!(!term.shown.contains(STATIC) && !done.contains(STATIC));
        assert_eq!(store.get("sentry-work").unwrap(), None);
        assert!(!home::load(&home::path(&data)).unwrap().contains_key("sentry-work"));

        let mut term = Script::new(false, &[], None);
        let done = revoke("gh", true, &mut term).unwrap();
        assert!(term.shown.contains("no instance here takes it"), "{}", term.shown);
        assert!(done.contains("stays valid at the provider"), "{done}");
        assert!(!done.contains("ghu_") && !term.shown.contains("ghu_"));
        assert_eq!(store.get("gh").unwrap(), None);
        assert!(revoke("gh", true, &mut Script::new(false, &[], None)).unwrap_err().to_string().contains("no credential `gh`"));
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn revokes_a_version() {
        let data = data_fixture("revoke-version");
        let state = state_fixture();
        let path = home::path(&data);
        let (v0, v1, v2) = (trimmed(0).version(), trimmed(1).version(), trimmed(2).version());
        let revoke = |v: &str, yes, term: &mut Script| {
            revoke_at(&data, &state, "sentry-work", Some(v), yes, term, &mut access_fixture)
        };

        assert!(revoke("", true, &mut Script::new(false, &[], None)).unwrap_err().to_string().contains("no claim version"));
        let err = revoke(&v0[..8], false, &mut Script::new(false, &[], None)).unwrap_err().to_string();
        assert!(err.contains("pass --yes to revoke"), "{err}");

        let mut term = Script::new(true, &[true], None);
        let done = revoke(&v0[..8], false, &mut term).unwrap();
        assert_eq!(done, format!("dropped approved claim version {} of `sentry-work`", &v0[..12]));
        assert!(
            term.shown.contains("instances pinning it get the not-approved refusal on their next call: w0\n"),
            "{}",
            term.shown
        );
        assert!(!term.shown.contains("latest"), "{}", term.shown);

        let mut term = Script::new(false, &[], None);
        revoke(&v1, true, &mut term).unwrap();
        assert!(term.shown.contains("no approved version is left"), "{}", term.shown);
        assert!(term.shown.contains("lost (perhaps"), "{}", term.shown);

        let mut term = Script::new(false, &[], None);
        let done = revoke(&v2[..10], true, &mut term).unwrap();
        assert!(done.starts_with("dropped pending claim version"), "{done}");
        assert!(!term.shown.contains("refusal"), "{}", term.shown);
        let e = &home::load(&path).unwrap()["sentry-work"];
        assert!(e.approved.is_empty() && e.pending.is_empty());
        let err = revoke(&v2[..10], true, &mut Script::new(false, &[], None)).unwrap_err().to_string();
        assert!(err.contains("(versions: none)"), "{err}");
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn forget_narrows_a_folder_fence_and_ls_shows_it() {
        let root = temp("forget");
        let (folder, state) = (root.join("app"), root.join("state.toml"));
        std::fs::create_dir_all(&folder).unwrap();
        let key = remotes::folder_key(&folder.canonicalize().unwrap());
        assert!(forget_at(&state, &folder, "o/r").unwrap_err().to_string().contains("no remotes recorded"));
        State::update_at(&state, |s| {
            let record = crate::state::FolderRemotes { github: vec!["o/app".into(), "v/x".into()], ..Default::default() };
            s.folder_remotes.insert(key.clone(), record);
            let mut inst = instance("app", Vec::new());
            inst.base_folder = key.clone().into();
            inst.github_repos = vec!["o/app".into(), "v/x".into()];
            s.instances.insert("app".into(), inst);
            Ok(())
        })
        .unwrap();
        assert!(forget_at(&state, &folder, "o/nope").unwrap_err().to_string().contains("(it holds: o/app, v/x)"));
        assert!(forget_at(&state, &folder, "not a repo").is_err());

        let done = forget_at(&state, &folder, "https://github.com/V/X.git").unwrap();
        assert_eq!(done, format!("dropped `v/x` from `{key}`'s GitHub fence; these instances take it at their next start or rebuild: app"));
        let s = State::load_at(&state).unwrap();
        assert_eq!(s.folder_remotes[&key].github, ["o/app"]);
        assert_eq!(s.instances["app"].github_repos, ["o/app", "v/x"], "until its next start");
        let listed = fences(&s);
        assert_eq!(listed[0].instances, [FenceUse { instance: "app".into(), stale: true }]);
        assert_eq!(
            render_fences(&listed),
            format!("\nGitHub repo fences (each folder's remotes at its first run):\n  {key}  o/app\n    instances  app (until its next start)\n")
        );
        remotes::refresh_at(&state, "app").unwrap();
        assert!(!fences(&State::load_at(&state).unwrap())[0].instances[0].stale);
        assert!(forget_at(&state, &folder, "v/x").is_err(), "never re-widened");
        assert_eq!(render_fences(&[]), "");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn picks_any_version_and_names_the_latest_left() {
        let e = &home_fixture()["sentry-work"];
        let (v0, v1) = (trimmed(0).version(), trimmed(1).version());
        assert_eq!(pick_any("s", e, &v1[..12]).unwrap(), ("approved", &e.approved[1]));
        assert_eq!(pick_any("s", e, &trimmed(2).version()).unwrap().0, "pending");
        let common = v0.chars().zip(v1.chars()).take_while(|(a, b)| a == b).count();
        if common > 0 {
            assert!(pick_any("s", e, &v0[..common]).unwrap_err().to_string().contains("several"));
        }

        let data = data_fixture("revoke-latest");
        let mut term = Script::new(false, &[], None);
        revoke_at(&data, &State::default(), "sentry-work", Some(&v1), true, &mut term, &mut access_fixture).unwrap();
        assert!(term.shown.contains(&format!("the latest approved version becomes {}\n", &v0[..12])), "{}", term.shown);
        let _ = std::fs::remove_dir_all(&data);
    }
}
