//! `devsandbox machine add|ls|set|upgrade|unlink|rm|trust|roots|allow|key`
//! (docs/architecture/daemon/machines.md, *`devsandbox machine
//! add|ls|set|upgrade|unlink|rm|…`*): a client of the daemon's `machines.*`,
//! never in-process, since the registry and the links are the daemon's.
//! `machine trust`, `machine roots`, `machine allow` and `machine key` are likewise clients of `member.*`
//! (this host's `member.toml`, docs/architecture/daemon/fleet.md, *Trust*).
//!
//! `add` always probes the machine first (`machines.probe`): its member key's
//! fingerprint is shown and confirmed on a TTY (`--yes` takes it without
//! asking; off a TTY `--yes` is required), and the confirmed key goes to
//! `machines.add`, which refuses a machine whose key differs. `add` picks
//! no config roots: a machine's are what its own daemon lists
//! (`machine roots add` there names more).
//!
//! Before probing, `add` checks the machine's devsandbox (`install`) and,
//! when it's missing or another version, copies this binary there: asked on
//! a TTY, without asking under `--auto-install`, refused otherwise.
//!
//! `upgrade` copies this binary over a registered machine's devsandbox when
//! its build differs (`install::decide_upgrade`; asked on a TTY, `--yes`
//! required off one), then has its daemon hand off to it
//! (`install::handoff`) and redials this host's link to it
//! (`machines.cycle`), so the far end's `fleet-link` runs it too.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::render::table;
use crate::serve::capacity;
use crate::serve::client::{self, Conn};
use crate::serve::endpoint;
use crate::serve::fleet::member::{AllowInfo, AllowPatch, MemberInfo, Trust};
use crate::serve::fleet::update::{self, Build};
use crate::serve::machines::{Entry, MachineKind};
use crate::serve::machines::api::{Added, MachineInfo, MachineRow, Probed};
use crate::serve::machines::join::{Fleet, Joined};
use crate::serve::fleet::proposals::Proposal;
use crate::serve::machines::changes::{Approved, ControlPlaneMove, Proposed, Rotation, Signed};
use crate::serve::machines::leave::{Heard, Removed, RmAnswer};
use crate::serve::machines::install::{self, Dest, Plan, Remote, Upgrade};

const HEADERS: [&str; 7] = ["NAME", "TARGET", "ROOTS", "AGENT", "STATE", "VERSION", "CAPACITY"];

pub fn add(target: &str, name: Option<&str>, agent: bool, control_plane: bool, auto_install: bool, yes: bool, allow: &AddAllow) -> Result<()> {
    // Checked before anything reaches the machine.
    let flagged = add_patch(target, allow)?;
    let command = ensure_devsandbox(target, auto_install)?;
    let mut conn = connect()?;
    let probed: Probed = call(&mut conn, "machines.probe", json!({ "target": target, "command": command }))?;
    let tty = std::io::stdin().is_terminal();
    confirm_key(target, &probed.fingerprint, tty, yes)?;
    let patch = match allow_step(flagged.is_some(), tty, probed.trust) {
        AllowStep::Flags => flagged,
        AllowStep::Ask => {
            eprintln!("{}", lends_line(target, &probed.allow));
            let question = allow_question(target);
            loop {
                match parse_allow_answer(&prompt(&question)?) {
                    Ok(patch) => break patch,
                    Err(e) => eprintln!("{e}"),
                }
            }
        }
        AllowStep::Full => {
            eprintln!("note: {target}'s trust is full, so no allowance applies there: not asking which to add");
            None
        }
        AllowStep::Nothing => None,
    };
    let params = json!({ "target": target, "name": name, "control_plane": control_plane, "agent": agent, "command": command, "key": probed.key, "allow": patch });
    let added: Added = call(&mut conn, "machines.add", params)?;
    println!("added {} ({target})", added.name);
    if let Some(allow) = &added.allow {
        println!("{}", lends_line(&added.name, allow));
    }
    println!("{}", fleet_line(&added.name, &added.fleet));
    if let Some(p) = &added.proposed {
        println!("{}", added_proposal_line(&added.name, p));
    }
    if newly_in_a_fleet(added.fleet.joined) {
        let own: MemberInfo = call(&mut conn, "member.get", Value::Null)?;
        if let Some(hint) = own_allow_hint(&own) {
            eprintln!("{hint}");
        }
    }
    for note in &added.configs {
        eprintln!("{note}");
    }
    if let Some(warning) = added.warning {
        eprintln!("warning: {warning}");
    }
    Ok(())
}

/// `machine add`'s `--allow <allowance>` (wire names, [`parse_allowance`]),
/// `--allow-bind <path>` and `--allow-local-env <NAME>`.
#[derive(Debug, Default)]
pub struct AddAllow {
    pub flags: Vec<&'static str>,
    pub binds: Vec<String>,
    pub local_env: Vec<String>,
}

/// `machines.add`'s `allow` from the flags (`None`: none given). Each
/// allowance is added, nothing removed. A bind is a path on the machine,
/// so it must be absolute: neither this host's `~` nor its current
/// directory means anything there.
fn add_patch(target: &str, allow: &AddAllow) -> Result<Option<AllowPatch>> {
    let mut patch = AllowPatch::default();
    for key in &allow.flags {
        set_allowance(&mut patch, key);
    }
    for bind in &allow.binds {
        let path = Path::new(bind);
        if !path.is_absolute() {
            bail!("--allow-bind `{bind}`: give the path on {target} as an absolute one");
        }
        // `components` drops `.`s and trailing `/`s.
        patch.binds_add.push(path.components().collect());
    }
    for name in &allow.local_env {
        crate::serve::fleet::member::check_env_name(name).map_err(|e| anyhow::anyhow!("--allow-local-env: {e:#}"))?;
        patch.local_env_add.push(name.clone());
    }
    Ok((patch != AllowPatch::default()).then_some(patch))
}

/// Turn on allowance `key` (a wire name from [`AllowInfo::flags`]) in `patch`.
fn set_allowance(patch: &mut AllowPatch, key: &str) {
    let slot = match key {
        "privileged" => &mut patch.privileged,
        "runtime_socket" => &mut patch.runtime_socket,
        "host_network" => &mut patch.host_network,
        "global_services" => &mut patch.global_services,
        "dispatcher" => &mut patch.dispatcher,
        "inbox" => &mut patch.inbox,
        "initialize_command" => &mut patch.initialize_command,
        "publish" => &mut patch.publish,
        "gpus" => &mut patch.gpus,
        _ => unreachable!("`{key}` isn't an allowance"),
    };
    *slot = Some(true);
}

/// The on/off allowances as the CLI spells them (`machine allow`'s flags).
fn allowance_names() -> Vec<String> {
    AllowInfo::default().flags().iter().map(|(key, _)| key.replace('_', "-")).collect()
}

/// `--allow`'s value, and each item of the add's question: an allowance
/// as the CLI spells it (`host-network`), answered as its wire name.
pub fn parse_allowance(s: &str) -> Result<&'static str, String> {
    AllowInfo::default()
        .flags()
        .into_iter()
        .map(|(key, _)| key)
        .find(|key| key.replace('_', "-") == s)
        .ok_or_else(|| format!("`{s}` isn't an allowance (one of: {})", allowance_names().join(", ")))
}

/// What `machine add` does about the machine's allowances.
#[derive(Debug, PartialEq, Eq)]
enum AllowStep {
    /// `--allow*` given: those, nothing asked.
    Flags,
    /// On a TTY: show the machine's and ask which to add.
    Ask,
    /// On a TTY, the machine at `trust = full`: no allowance applies, a note.
    Full,
    /// Off a TTY: nothing asked, nothing changed (an unset allowance is
    /// the safe default, unlike an unconfirmed key).
    Nothing,
}

fn allow_step(flagged: bool, tty: bool, trust: Trust) -> AllowStep {
    match (flagged, tty, trust) {
        (true, _, _) => AllowStep::Flags,
        (false, false, _) => AllowStep::Nothing,
        (false, true, Trust::Full) => AllowStep::Full,
        (false, true, Trust::Worker) => AllowStep::Ask,
    }
}

fn allow_question(target: &str) -> String {
    format!(
        "Allow on {target}, comma-separated ({}; paths and env vars: --allow-bind, --allow-local-env)? Empty adds none: ",
        allowance_names().join(", ")
    )
}

/// The question's answer as `machines.add`'s `allow` (`None`: empty, none
/// added); `Err` names an unknown allowance, and the question is asked again.
fn parse_allow_answer(answer: &str) -> Result<Option<AllowPatch>, String> {
    let mut patch = AllowPatch::default();
    for item in answer.split([',', ' ', '\t']).filter(|s| !s.is_empty()) {
        set_allowance(&mut patch, parse_allowance(item)?);
    }
    Ok((patch != AllowPatch::default()).then_some(patch))
}

/// `<who> lends placed work: <what>` in one line, `nothing` when no
/// allowance is set.
fn lends_line(who: &str, allow: &AllowInfo) -> String {
    let mut groups = Vec::new();
    let on: Vec<String> = allow.flags().iter().filter(|(_, on)| *on).map(|(key, _)| key.replace('_', "-")).collect();
    if !on.is_empty() {
        groups.push(on.join(", "));
    }
    if !allow.binds.is_empty() {
        groups.push(format!("binds: {}", allow.binds.iter().map(|b| b.display().to_string()).collect::<Vec<_>>().join(", ")));
    }
    if !allow.local_env.is_empty() {
        groups.push(format!("local-env: {}", allow.local_env.join(", ")));
    }
    let what = if groups.is_empty() { "nothing".to_string() } else { groups.join("; ") };
    format!("{who} lends placed work: {what}")
}

/// The add put this host in a fleet it wasn't in, so the fleet can place
/// work here from now on.
fn newly_in_a_fleet(joined: Joined) -> bool {
    matches!(joined, Joined::This | Joined::New)
}

/// A hint when this host lends placed work nothing (`worker` trust, no
/// allowance): `machine add` asks the machine's allowances, never this
/// host's, which only `machine allow` here sets.
fn own_allow_hint(own: &MemberInfo) -> Option<String> {
    (own.trust == Trust::Worker && own.allow == AllowInfo::default())
        .then(|| "note: this host lends nothing to placed work; set it with `devsandbox machine allow`".to_string())
}

/// Show `target`'s key fingerprint and have it confirmed: asked on a TTY
/// unless `yes`; off one, `yes` is required.
fn confirm_key(target: &str, fingerprint: &str, tty: bool, yes: bool) -> Result<()> {
    let shown = key_line(target, fingerprint);
    match consent(tty, yes) {
        Consent::Install => {
            eprintln!("{shown}");
            Ok(())
        }
        Consent::Refuse => bail!("{}", key_refusal(target, fingerprint)),
        Consent::Ask => {
            eprintln!("{shown}");
            if is_yes(&prompt("Is that the machine's key (`devsandbox machine key` there)? [y/N] ")?) {
                Ok(())
            } else {
                bail!("{target}'s key not confirmed: nothing added")
            }
        }
    }
}

fn key_line(target: &str, fingerprint: &str) -> String {
    format!("{target}'s key fingerprint is {fingerprint}")
}

/// Off a TTY without `--yes`.
fn key_refusal(target: &str, fingerprint: &str) -> String {
    format!(
        "{}: pass --yes to take it without a TTY to confirm it on (check it against `devsandbox machine key` run there)",
        key_line(target, fingerprint)
    )
}

/// What `add`'s join did, in one line.
fn fleet_line(name: &str, fleet: &Fleet) -> String {
    let control_plane = &fleet.control_plane;
    match fleet.joined {
        Joined::Target => format!("{name} joined {control_plane}'s fleet"),
        Joined::This => format!("this host joined {control_plane}'s fleet"),
        // `--control-plane` made the machine the control plane: the control plane is its name.
        Joined::New if *control_plane == name => format!("a fleet of two: {name} is the control plane"),
        Joined::New => format!("a fleet of two: this host ({control_plane}) is the control plane"),
        Joined::Same => format!("an extra link in {control_plane}'s fleet"),
    }
}

/// [`install::DEFAULT_DEST`] as the registry's `command`: shell text, the
/// remote login shell expands the `~`.
const LOCAL_COMMAND: &str = "~/.local/bin/devsandbox";

/// Check `target`'s devsandbox and copy this one there when it's missing or
/// another version (docs/architecture/daemon/machines.md, *Install*). The
/// machine's `command`: `None` is `devsandbox` on its login `PATH`.
fn ensure_devsandbox(target: &str, auto_install: bool) -> Result<Option<String>> {
    let ours = env!("CARGO_PKG_VERSION");
    let entry = Entry::ssh(target, target);
    let remote = install::check(&entry, None)?;
    let found_command = remote.off_path.then(|| LOCAL_COMMAND.to_string());
    if found_command.is_some() {
        eprintln!("note: `devsandbox` isn't on {target}'s login PATH, using {LOCAL_COMMAND}");
    }
    let Plan::Install { dest, from } = install::decide(&remote, ours) else {
        return Ok(found_command);
    };
    let found = match &dest {
        Dest::Found(path) => Some(path.as_str()),
        Dest::Default => None,
    };
    if !install::platform_matches(&remote.os, &remote.arch) {
        let why = platform_mismatch(target, &remote);
        if found.is_none() {
            eprintln!("warning: found no devsandbox on {target}, and {why}: install it there yourself");
            return Ok(None);
        }
        // Today's behaviour: the daemon warns about the other version.
        eprintln!("note: {why}: leaving its devsandbox as it is");
        return Ok(found_command);
    }
    match consent(std::io::stdin().is_terminal(), auto_install) {
        Consent::Install => {}
        Consent::Refuse => bail!("{}", refusal(target, found, from.as_deref(), ours)),
        Consent::Ask => {
            if !is_yes(&prompt(&question(target, found, from.as_deref(), ours))?) {
                if found.is_none() {
                    bail!("no devsandbox on {target}, so it isn't added");
                }
                return Ok(found_command);
            }
        }
    }
    let shown = found.unwrap_or(LOCAL_COMMAND);
    eprintln!("copying devsandbox {ours} to {target}:{shown}…");
    let installed = install::install(&entry, dest.path(), &crate::exe::devsandbox()?)?;
    let after = install::check(&entry, None)?;
    let command = match rechecked(&after, &dest, ours, target, &installed)? {
        Rechecked::OnPath => after.off_path.then(|| LOCAL_COMMAND.to_string()),
        Rechecked::OffPath => {
            let direct = install::check(&entry, Some(&installed))?;
            if direct.version.as_deref() != Some(ours) {
                bail!("{installed} on {target} doesn't run as devsandbox {ours} after the copy");
            }
            Some(LOCAL_COMMAND.to_string())
        }
    };
    eprintln!("installed devsandbox {ours} at {installed}");
    if command.is_some() {
        eprintln!("note: `devsandbox` on {target}'s login PATH isn't that one, so the machine is registered with command = \"{LOCAL_COMMAND}\"");
    }
    Ok(command)
}

/// Whether to install without asking, ask, or refuse.
#[derive(Debug, PartialEq, Eq)]
enum Consent {
    Install,
    Ask,
    Refuse,
}

fn consent(tty: bool, auto_install: bool) -> Consent {
    match (auto_install, tty) {
        (true, _) => Consent::Install,
        (false, true) => Consent::Ask,
        (false, false) => Consent::Refuse,
    }
}

/// `from` as a sentence's object: `0.7.0`, or that it's unknown.
fn version_text(from: Option<&str>) -> &str {
    from.unwrap_or("of an unknown (older) version")
}

/// The error off a TTY without `--auto-install`. `found`: the machine's
/// devsandbox, `None` when missing.
fn refusal(target: &str, found: Option<&str>, from: Option<&str>, ours: &str) -> String {
    match found {
        None => format!("{target} has no devsandbox: pass --auto-install to copy this one ({ours}) there"),
        Some(path) => format!(
            "{target} runs devsandbox {} at {path}, this is {ours}: pass --auto-install to replace it",
            version_text(from)
        ),
    }
}

/// The `[y/N]` question on a TTY.
fn question(target: &str, found: Option<&str>, from: Option<&str>, ours: &str) -> String {
    match found {
        None => format!("devsandbox isn't on {target}'s PATH. Copy this one ({ours}) to {LOCAL_COMMAND} there? [y/N] "),
        Some(path) => format!(
            "{target} runs devsandbox {} at {path}. Replace it with this one ({ours})? [y/N] ",
            from.unwrap_or("(unknown version)")
        ),
    }
}

fn is_yes(answer: &str) -> bool {
    answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes")
}

fn platform_mismatch(target: &str, remote: &Remote) -> String {
    format!(
        "{target} is {} {}; this devsandbox is built for {} {}, so it can't be copied there",
        remote.os,
        remote.arch,
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// What the check after the copy says about the login `PATH`.
#[derive(Debug, PartialEq, Eq)]
enum Rechecked {
    /// `devsandbox` there is ours now.
    OnPath,
    /// Installed to [`LOCAL_COMMAND`], which the login `PATH` doesn't reach
    /// first: the machine's `command` is that path.
    OffPath,
}

/// `after`: the login shell's devsandbox after copying ours to `dest`,
/// `installed` there. An update in place that `PATH` still doesn't resolve
/// to is an error: no `command` can fix that.
fn rechecked(after: &Remote, dest: &Dest, ours: &str, target: &str, installed: &str) -> Result<Rechecked> {
    if after.version.as_deref() == Some(ours) {
        return Ok(Rechecked::OnPath);
    }
    if *dest == Dest::Default {
        return Ok(Rechecked::OffPath);
    }
    match &after.path {
        None => bail!("updated {installed}, but {target}'s login PATH no longer finds a devsandbox"),
        Some(path) => bail!(
            "updated {installed}, but `devsandbox` on {target}'s login PATH is {path} (devsandbox {})",
            version_text(after.version.as_deref())
        ),
    }
}

/// `machine upgrade <name>`: copy this binary over registered machine
/// `name`'s devsandbox (the one its `command` names) when its build
/// differs (docs/architecture/daemon/machines.md, *Install*), then have the
/// daemon there hand off to it and this host's link to it redial.
pub fn upgrade(name: &str, yes: bool) -> Result<()> {
    let ours = update::HELLO;
    let info: MachineInfo = call(&mut connect()?, "machines.get", json!({ "name": name }))?;
    let Some(target) = info.target.as_deref() else {
        bail!("{}", no_ssh(name, info.kind));
    };
    let entry = Entry { command: info.command.clone(), ..Entry::ssh(&info.name, target) };
    // Named, so the check never falls back to `~/.local/bin`: that isn't
    // what the link runs.
    let program = entry.command().to_string();
    let remote = install::check(&entry, Some(&program))?;
    // What is copied, so what the machine's bytes must be.
    let binary = crate::exe::devsandbox()?;
    let sha = hex::encode(update::sha256_of(&binary)?);
    let (path, from) = match install::decide_upgrade(&remote, ours, &sha) {
        Upgrade::Nothing => {
            println!("{name} already runs devsandbox {}", label(Some(ours)));
            return Ok(());
        }
        Upgrade::Refuse(why) => bail!("cannot upgrade {name} ({target}): {why}"),
        Upgrade::Install { path, from } => (path, from),
    };
    let what = upgrade_what(name, &path, from.as_deref(), ours);
    match consent(std::io::stdin().is_terminal(), yes) {
        Consent::Install => {}
        Consent::Refuse => bail!("{what}: pass --yes to replace it without a TTY to confirm on"),
        Consent::Ask => {
            if !is_yes(&prompt(&format!("{what}. Replace it? [y/N] "))?) {
                bail!("nothing changed on {name}");
            }
        }
    }
    eprintln!("copying devsandbox {} to {target}:{path}…", label(Some(ours)));
    let installed = install::install(&entry, Some(&path), &binary)?;
    let after = install::check(&entry, Some(&program))?;
    if install::decide_upgrade(&after, ours, &sha) != Upgrade::Nothing {
        bail!("{installed} on {target} doesn't run as devsandbox {} after the copy (it says {})", label(Some(ours)), label(after.hello.as_deref()));
    }
    install::handoff(&entry, &installed)?;
    let _: Value = call(&mut connect()?, "machines.cycle", json!({ "name": info.name }))?;
    println!("upgraded {name}: devsandbox {} → {}", label(from.as_deref()), label(Some(ours)));
    if let Some(note) = from.as_deref().and_then(|from| older_note(name, from, ours)) {
        eprintln!("note: {note}");
    }
    Ok(())
}

/// A `fleet-hello` as a build label (`0.7.0 (dev, 1a2b)`); unknown: an
/// older build, from before `fleet-hello`.
fn label(hello: Option<&str>) -> String {
    hello.map_or_else(|| "of an unknown (older) build".into(), |h| Build::parse(h).label())
}

/// What `upgrade` is about to do, in a clause.
fn upgrade_what(name: &str, path: &str, from: Option<&str>, ours: &str) -> String {
    format!("{name} runs devsandbox {} at {path}; this is {}", label(from), label(Some(ours)))
}

/// Why `upgrade` can't reach `name`: no ssh target here.
fn no_ssh(name: &str, kind: MachineKind) -> String {
    let how = match kind {
        MachineKind::Routed => "is reached over a route",
        MachineKind::Inbound | MachineKind::Ssh => "links to this host from its end (inbound)",
    };
    format!(
        "{name} {how}: `machine upgrade` copies over ssh, and this host has none to it; run it on a host that registered {name} (`machine add`)"
    )
}

/// A downgrade by semver: the daemon there doesn't hand off to an older
/// semver (ADR 0015), so it keeps its build until restarted.
fn older_note(name: &str, from: &str, ours: &str) -> Option<String> {
    let semver = |hello: &str| crate::serve::proto::semver_triple(hello.split_whitespace().next().unwrap_or(""));
    (semver(ours) < semver(from)).then(|| {
        format!("this is an older semver than {name} ran: its daemon doesn't hand off to an older one, so it runs the copy once restarted there")
    })
}

pub fn ls(json: bool) -> Result<()> {
    let mut conn = connect()?;
    if json {
        let rows: Value = call(&mut conn, "machines.list", Value::Null)?;
        println!("{}", serde_json::to_string_pretty(&rows).context("serialize machines")?);
        return Ok(());
    }
    let rows: Vec<MachineRow> = call(&mut conn, "machines.list", Value::Null)?;
    if rows.is_empty() {
        println!("no machines; `devsandbox machine add <target>` adds one");
        return Ok(());
    }
    // Best effort: a daemon without a member.toml has no skip to say.
    let member: Option<MemberInfo> = call(&mut conn, "member.get", Value::Null).ok();
    let notes = ls_notes(&rows, member.as_ref().and_then(|m| m.pin_skipped.as_deref()));
    let cells: Vec<[String; 7]> = rows.into_iter().map(cells).collect();
    print!("{}", table(&HEADERS, &cells));
    for note in notes {
        eprintln!("note: {note}");
    }
    Ok(())
}

/// `machine ls`'s lines under the table (pure): this host behind the
/// fleet's pinned build, as its links decided (each distinct why once),
/// and, on the authority, why its build isn't pinned (`member.get`'s
/// `pin_skipped`).
fn ls_notes(rows: &[MachineRow], pin_skipped: Option<&str>) -> Vec<String> {
    let mut notes: Vec<String> = Vec::new();
    for why in rows.iter().filter_map(|r| r.behind_here.as_deref()) {
        let note = format!("this host is behind: {why}");
        if !notes.contains(&note) {
            notes.push(note);
        }
    }
    notes.extend(pin_skipped.map(str::to_string));
    notes
}

pub fn set(name: &str, agent: bool) -> Result<()> {
    let mut conn = connect()?;
    let _: Value = call(&mut conn, "machines.set", json!({ "name": name, "agent": agent }))?;
    println!("{name}: agent {}", on_off(agent));
    Ok(())
}

/// `machine trust [worker|full]`: set this host's trust (`member.set`), or
/// show it (`member.get`).
pub fn trust(to: Option<Trust>) -> Result<()> {
    let mut conn = connect()?;
    let info: MemberInfo = match to {
        Some(trust) => call(&mut conn, "member.set", json!({ "trust": trust }))?,
        None => call(&mut conn, "member.get", Value::Null)?,
    };
    println!("{}", trust_line(info.trust));
    if let Some(warning) = info.exposed_by.as_deref().and_then(exposure_warning) {
        eprintln!("warning: {warning}");
    }
    Ok(())
}

/// What `machine trust` warns of while this host is `worker` and exposed
/// (`member.get`'s `exposed_by`, the chain from the control plane): the machine that
/// dials this host has a shell here, which `worker` doesn't stop.
fn exposure_warning(chain: &[String]) -> Option<String> {
    let dialer = crate::serve::fleet::graph::dialer(chain)?;
    Some(format!(
        "{dialer} dials this host over ssh, so it has a shell here and could rewrite member.toml: `worker` doesn't protect this host from {dialer} (exposed through {})",
        chain.join(" > ")
    ))
}

/// `machine key`: this host's member key (`member.get`'s `key`, created
/// there on first ask) and its fingerprint.
pub fn key() -> Result<()> {
    let mut conn = connect()?;
    let info: MemberInfo = call(&mut conn, "member.get", Value::Null)?;
    print!("{}", key_text(&info));
    Ok(())
}

/// `<name> <key>`, then the fingerprint; `-` for a host with no valid name.
/// A pending rotation's next key after.
fn key_text(info: &MemberInfo) -> String {
    let mut text = format!("{} {}\n{}\n", info.name.as_deref().unwrap_or("-"), info.key, info.fingerprint);
    if let Some(next) = &info.next {
        text.push_str(&format!("rotating to {} ({}): this host's key once the authority's signed change reaches it\n", next.fingerprint, next.key));
    }
    text
}

/// `machine key rotate`: a new key for this host (`membership.rotate`),
/// signed here on the authority or proposed to it.
pub fn rotate_key() -> Result<()> {
    let mut conn = connect()?;
    let r: Rotation = call(&mut conn, "membership.rotate", Value::Null)?;
    for line in rotation_lines(&r) {
        println!("{line}");
    }
    Ok(())
}

fn rotation_lines(r: &Rotation) -> Vec<String> {
    match r {
        Rotation::Rotated(r) => {
            let mut lines = vec![format!("this host's key is now {} ({})", r.fingerprint, r.key)];
            lines.extend(signed_lines(&r.signed));
            lines
        }
        Rotation::Proposed(p) => vec![
            proposed_line(&format!("rotating this host's key to {}", p.fingerprint), &p.proposed),
            "this host keeps its key until the signed change reaches it".into(),
        ],
    }
}

/// `machine control-plane <name>`: the fleet's control-plane role moved to member `name`
/// (`membership.controlPlane`), signed here on the authority or proposed to it.
pub fn control_plane(name: &str) -> Result<()> {
    let mut conn = connect()?;
    let r: ControlPlaneMove = call(&mut conn, "membership.controlPlane", json!({ "name": name }))?;
    for line in control_plane_lines(&r) {
        println!("{line}");
    }
    Ok(())
}

fn control_plane_lines(r: &ControlPlaneMove) -> Vec<String> {
    match r {
        ControlPlaneMove::Signed(s) => {
            let mut lines = vec![format!("{} is the fleet's control plane", s.name)];
            lines.extend(signed_lines(s));
            lines
        }
        ControlPlaneMove::Proposed(p) => vec![proposed_line(&format!("making {} the fleet's control plane", p.name), p)],
    }
}

pub enum RootsOp {
    Show,
    Add(Vec<String>),
    Rm(Vec<String>),
}

/// `machine roots [add|rm] [<path>…]`: this host's picked roots
/// (`member.toml`'s `roots`): shown (`member.get`), added or removed
/// (`member.set {roots_add|roots_rm}`). The paths are resolved here, where
/// they are: `machine add` runs `roots add` on the machine over ssh.
pub fn roots(op: RootsOp) -> Result<()> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let cwd = std::env::current_dir().context("cannot read the current directory")?;
    let params = match op {
        RootsOp::Show => None,
        RootsOp::Add(paths) => {
            let roots = paths.iter().map(|p| existing_dir(&resolve_root(home.as_deref(), &cwd, p)?)).collect::<Result<Vec<_>>>()?;
            Some(json!({ "roots_add": roots }))
        }
        RootsOp::Rm(paths) => {
            let mut roots = Vec::new();
            for p in &paths {
                let path = resolve_root(home.as_deref(), &cwd, p)?;
                // The canonical form when it still exists, and the literal
                // one, so a removed dir can be dropped too.
                if let Ok(canonical) = path.canonicalize() {
                    roots.push(canonical);
                }
                roots.push(path);
            }
            Some(json!({ "roots_rm": roots }))
        }
    };
    let mut conn = connect()?;
    let info: MemberInfo = match params {
        Some(params) => call(&mut conn, "member.set", params)?,
        None => call(&mut conn, "member.get", Value::Null)?,
    };
    print!("{}", roots_text(&info.roots));
    Ok(())
}

fn roots_text(roots: &[PathBuf]) -> String {
    if roots.is_empty() {
        return "no picked roots (only those with instances); `devsandbox machine roots add <path>` adds one\n".into();
    }
    roots.iter().map(|r| format!("{}\n", r.display())).collect()
}

/// `machine allow`'s flags: `(wire key, on)` per `--<allowance> on|off`
/// given, and the list changes as typed.
#[derive(Debug, Default)]
pub struct AllowChange {
    pub flags: Vec<(&'static str, bool)>,
    pub bind_add: Vec<String>,
    pub bind_rm: Vec<String>,
    pub local_env_add: Vec<String>,
    pub local_env_rm: Vec<String>,
}

/// `machine allow [--<allowance> on|off]… [--bind-add|--bind-rm <path>]…
/// [--local-env-add|--local-env-rm <NAME>]…`: this host's worker policy
/// allowances (`member.toml`'s `[allow]`), shown (`member.get`) or changed
/// (`member.set {allow}`). This host's only: `member.*` is never answered
/// over a link, and `--machine` refuses `devsandbox machine` (`machines::cli::plan`).
pub fn allow(change: AllowChange) -> Result<()> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let cwd = std::env::current_dir().context("cannot read the current directory")?;
    let params = allow_params(&change, home.as_deref(), &cwd)?;
    let mut conn = connect()?;
    let info: MemberInfo = match params {
        Some(params) => call(&mut conn, "member.set", params)?,
        None => call(&mut conn, "member.get", Value::Null)?,
    };
    print!("{}", allow_text(&info.allow, info.trust));
    Ok(())
}

/// `member.set`'s params for `change` (`None`: nothing to change, a show).
/// Bind paths are resolved as `roots` are ([`resolve_root`]), not
/// canonicalized: the policy resolves them at each check.
fn allow_params(change: &AllowChange, home: Option<&Path>, cwd: &Path) -> Result<Option<Value>> {
    // `components` drops `.`s, so `./x` is saved as `/cwd/x`.
    let resolve = |paths: &[String]| {
        paths.iter().map(|p| resolve_root(home, cwd, p).map(|p| p.components().collect::<PathBuf>())).collect::<Result<Vec<_>>>()
    };
    let mut allow = serde_json::Map::new();
    for (key, on) in &change.flags {
        allow.insert((*key).into(), json!(on));
    }
    let lists = [
        ("binds_add", json!(resolve(&change.bind_add)?)),
        ("binds_rm", json!(resolve(&change.bind_rm)?)),
        ("local_env_add", json!(change.local_env_add)),
        ("local_env_rm", json!(change.local_env_rm)),
    ];
    for (key, list) in lists {
        if list.as_array().is_some_and(|l| !l.is_empty()) {
            allow.insert(key.into(), list);
        }
    }
    Ok((!allow.is_empty()).then(|| json!({ "allow": allow })))
}

/// One `<key>: on|off` line per allowance (the file's kebab names, as the
/// flags), then `binds` and `local-env`; under `full`, that none applies.
fn allow_text(allow: &AllowInfo, trust: Trust) -> String {
    let mut text: String = allow.flags().iter().map(|(key, on)| format!("{}: {}\n", key.replace('_', "-"), on_off(*on))).collect();
    let list = |items: Vec<String>| if items.is_empty() { "none".to_string() } else { items.join(", ") };
    text.push_str(&format!("binds: {}\n", list(allow.binds.iter().map(|b| b.display().to_string()).collect())));
    text.push_str(&format!("local-env: {}\n", list(allow.local_env.clone())));
    if trust == Trust::Full {
        text.push_str("trust is full: no worker policy applies here, so these limit nothing (devsandbox machine trust worker)\n");
    }
    text
}

/// `arg` as an absolute path: a leading `~` or `~/` is `home`, a relative
/// path is against `cwd`. Not canonicalized (the caller does, where the
/// path must exist).
fn resolve_root(home: Option<&Path>, cwd: &Path, arg: &str) -> Result<PathBuf> {
    if arg.is_empty() {
        bail!("empty path");
    }
    let rest = match arg.strip_prefix('~') {
        Some("") => Some(""),
        Some(rest) => rest.strip_prefix('/'),
        None => None,
    };
    Ok(match rest {
        Some(rest) => {
            let home = home.with_context(|| format!("`{arg}`: no $HOME to expand `~` with"))?;
            if rest.is_empty() { home.to_path_buf() } else { home.join(rest) }
        }
        None => cwd.join(arg),
    })
}

/// `path` canonicalized, which must be an existing directory.
fn existing_dir(path: &Path) -> Result<PathBuf> {
    let canonical = path.canonicalize().with_context(|| format!("no directory {}", path.display()))?;
    if !canonical.is_dir() {
        bail!("{} is not a directory", path.display());
    }
    Ok(canonical)
}

fn trust_line(trust: Trust) -> String {
    match trust {
        Trust::Worker => "trust: worker".into(),
        Trust::Full => "trust: full (remote callers get this host's whole API: any root, any path, no worker policy)".into(),
    }
}

/// `machine trust`'s value: `worker` or `full`.
pub fn parse_trust(s: &str) -> Result<Trust, String> {
    match s {
        "worker" => Ok(Trust::Worker),
        "full" => Ok(Trust::Full),
        _ => Err(format!("`{s}` isn't `worker` or `full`")),
    }
}

/// `--agent`'s value: `on` or `off`.
pub fn parse_on_off(s: &str) -> Result<bool, String> {
    match s {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err(format!("`{s}` isn't `on` or `off`")),
    }
}

fn on_off(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

pub fn unlink(name: &str) -> Result<()> {
    let mut conn = connect()?;
    let _: Value = call(&mut conn, "machines.unlink", json!({ "name": name }))?;
    println!("unlinked {name}");
    Ok(())
}

/// `machine rm`: `name` out of the fleet (or this host leaving it), one
/// line per outcome.
pub fn rm(name: &str) -> Result<()> {
    let mut conn = connect()?;
    let answer: RmAnswer = call(&mut conn, "machines.rm", json!({ "name": name }))?;
    for line in rm_lines(&answer) {
        println!("{line}");
    }
    Ok(())
}

/// `machine rm`'s lines (pure).
fn rm_lines(answer: &RmAnswer) -> Vec<String> {
    match answer {
        RmAnswer::Revoked(s) => signed_lines(s),
        RmAnswer::Proposed(p) => vec![proposed_line(&format!("removing {}", p.name), p)],
        RmAnswer::Left(l) => {
            let mut lines = Vec::new();
            if let Some(p) = &l.proposed {
                lines.push(proposed_line("its own removal", p));
            }
            if l.control_plane == l.name {
                lines.push(format!("this host ({}) was the control plane, alone: no fleet now", l.name));
            } else {
                match (&l.removed, &l.error) {
                    (Some(r), _) => lines.extend(removed_lines(r).into_iter().skip(1)),
                    (None, Some(why)) => {
                        lines.push(format!("the control plane {} wasn't reached ({why}): it still lists this host until `machine rm {}` there", l.control_plane, l.name))
                    }
                    (None, None) => {}
                }
                lines.push(format!("this host ({}) left {}'s fleet", l.name, l.control_plane));
            }
            if !l.dropped.is_empty() {
                lines.push(format!("unlinked {}", l.dropped.join(", ")));
            }
            lines
        }
    }
}

/// `machine add`'s proposal: of the machine's add when this host
/// introduced it, else of this host's join (the machine introduced this
/// host and proposed it).
fn added_proposal_line(machine: &str, p: &Proposed) -> String {
    proposed_line(if p.name == machine { "its add" } else { "this host's join" }, p)
}

/// A proposal's outcome: where to approve it, or why it wasn't sent.
fn proposed_line(what: &str, p: &Proposed) -> String {
    match (p.id, &p.error) {
        (Some(id), _) => format!("proposed {what} to {}: approve there with `devsandbox machine approve {id}`", p.authority),
        (None, Some(why)) => format!("{what} not proposed to the authority {}: {why}", p.authority),
        (None, None) => format!("{what} not proposed to the authority {}", p.authority),
    }
}

/// A change signed here (the authority): the change, whom its push
/// reached, then the removal's lines.
fn signed_lines(s: &Signed) -> Vec<String> {
    let mut lines = vec![format!("signed {}", s.change)];
    if !s.pushed.is_empty() {
        lines.push(format!("sent to {}", s.pushed.join(", ")));
    }
    for m in &s.unpushed {
        lines.push(format!("not sent to {} ({}): it takes it on its next link", m.name, m.error));
    }
    if let Some(r) = &s.removed {
        lines.extend(removed_lines(r));
    }
    if let Some(why) = &s.error {
        lines.push(why.clone());
    }
    lines
}

/// `machine pending`: the proposals waiting for approval here.
pub fn pending() -> Result<()> {
    let mut conn = connect()?;
    let list: Vec<Proposal> = call(&mut conn, "membership.pending", Value::Null)?;
    print!("{}", pending_text(&list, now_unix()));
    Ok(())
}

fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// `machine pending`'s table, or a note when there's none (pure).
fn pending_text(list: &[Proposal], now: u64) -> String {
    if list.is_empty() {
        return "no pending proposals\n".into();
    }
    let offset = crate::inbox::clock::local_utc_offset();
    let cells: Vec<[String; 4]> = list
        .iter()
        .map(|p| [p.id.to_string(), p.from.clone(), p.op.to_string(), crate::inbox::clock::short_age(p.at, now, offset)])
        .collect();
    table(&["ID", "FROM", "WHAT", "WHEN"], &cells)
}

/// `machine approve <id>`: sign the proposal here and run it.
pub fn approve(id: u64) -> Result<()> {
    let mut conn = connect()?;
    let approved: Approved = call(&mut conn, "membership.approve", json!({ "id": id }))?;
    for line in signed_lines(&approved.signed) {
        println!("{line}");
    }
    Ok(())
}

/// `machine deny <id>`: drop the proposal.
pub fn deny(id: u64) -> Result<()> {
    let mut conn = connect()?;
    let _: Value = call(&mut conn, "membership.deny", json!({ "id": id }))?;
    println!("denied proposal {id}");
    Ok(())
}

fn removed_lines(r: &Removed) -> Vec<String> {
    let mut lines = vec![format!("removed {} from {}'s fleet", r.name, r.control_plane)];
    match r.leave {
        Heard::Told => lines.push(format!("{} left the fleet", r.name)),
        Heard::Leaving => {}
        Heard::Unreachable => lines.push(format!(
            "{} wasn't reached ({}): it keeps its links until `machine rm {}` reaches it, or `machine rm {}` runs on it",
            r.name,
            r.error.as_deref().unwrap_or("unknown"),
            r.name,
            r.name
        )),
    }
    for m in &r.forgot {
        lines.push(format!("{m} forgot {}", r.name));
    }
    for m in &r.missed {
        lines.push(format!("{} wasn't reached ({}): it may still link to {}", m.name, m.error, r.name));
    }
    lines
}

fn connect() -> Result<Conn> {
    client::connect(&endpoint::socket_dir()?, "cli").context("`devsandbox machine` needs devsandbox serve")
}

/// One call; an error answer is the command's error, with the daemon's
/// message.
pub(crate) fn call<T: DeserializeOwned>(conn: &mut Conn, method: &str, params: Value) -> Result<T> {
    let r = conn.call(method, params)?;
    if let Some(e) = r.error {
        bail!("{}", e.message);
    }
    serde_json::from_value(r.result.unwrap_or(Value::Null)).with_context(|| format!("unexpected {method} answer"))
}

/// `ls`'s cells: the first root and how many more; an inbound neighbor's
/// TARGET is `inbound` (it has none); a member held as an introduction
/// (capped, fleet plan decision 51) named with its introducer, one exposed
/// while `worker` with the machine that dials it.
fn cells(r: MachineRow) -> [String; 7] {
    let notes: Vec<String> = r
        .introduced_by
        .as_ref()
        .map(|by| format!("introduced by {by}"))
        .into_iter()
        .chain(crate::serve::fleet::graph::exposed_note(r.exposed_by.as_deref()))
        .collect();
    let name = if notes.is_empty() { r.name.clone() } else { format!("{} ({})", r.name, notes.join(", ")) };
    let roots = match r.roots.split_first() {
        None => "-".to_string(),
        Some((first, [])) => first.display().to_string(),
        Some((first, rest)) => format!("{} +{}", first.display(), rest.len()),
    };
    // An inbound machine has no target: its kind; a routed one, its first hop.
    let target = match (r.target, r.kind, r.via) {
        (Some(target), ..) => target,
        (None, crate::serve::machines::MachineKind::Routed, via) => format!("routed via {}", via.as_deref().unwrap_or("-")),
        (None, ..) => "inbound".into(),
    };
    let agent = on_off(r.agent);
    let capacity = r.capacity.as_ref();
    let mut room = capacity::summary(capacity);
    if let Some(note) = capacity.and_then(capacity::over_note) {
        room = format!("{room} ({note})");
    }
    [
        name,
        target,
        roots,
        agent.into(),
        match &r.behind {
            Some(why) => format!("{} (behind: {why})", r.state.as_str()),
            None => r.state.as_str().into(),
        },
        r.version.unwrap_or_else(|| "-".into()),
        room,
    ]
}

fn prompt(text: &str) -> Result<String> {
    eprint!("{text}");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(answer.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn the_join_is_one_line() {
        let line = |joined| fleet_line("box", &Fleet { control_plane: "k".into(), joined });
        assert_eq!(line(Joined::Target), "box joined k's fleet");
        assert_eq!(line(Joined::This), "this host joined k's fleet");
        assert_eq!(line(Joined::New), "a fleet of two: this host (k) is the control plane");
        assert_eq!(fleet_line("box", &Fleet { control_plane: "box".into(), joined: Joined::New }), "a fleet of two: box is the control plane");
        assert_eq!(line(Joined::Same), "an extra link in k's fleet");
    }

    #[test]
    fn rm_says_what_reached_whom() {
        use crate::serve::machines::leave::{Left, Missed};
        let removed = |leave, error: Option<&str>| Removed {
            name: "box".into(),
            control_plane: "k".into(),
            leave,
            error: error.map(String::from),
            forgot: vec!["y".into()],
            missed: vec![Missed { name: "z".into(), error: "no link to it".into() }],
        };
        let signed = |removed, error: Option<&str>| Signed {
            name: "box".into(),
            change: "revoke `box`".into(),
            pushed: vec!["k".into()],
            unpushed: vec![Missed { name: "w".into(), error: "its link isn't up".into() }],
            removed,
            error: error.map(String::from),
        };
        assert_eq!(
            rm_lines(&RmAnswer::Revoked(signed(Some(removed(Heard::Told, None)), None))),
            [
                "signed revoke `box`",
                "sent to k",
                "not sent to w (its link isn't up): it takes it on its next link",
                "removed box from k's fleet",
                "box left the fleet",
                "y forgot box",
                "z wasn't reached (no link to it): it may still link to box"
            ]
        );
        let lines = rm_lines(&RmAnswer::Revoked(signed(Some(removed(Heard::Unreachable, Some("down"))), None)));
        assert!(lines[4].starts_with("box wasn't reached (down): "), "{lines:?}");
        let lines = rm_lines(&RmAnswer::Revoked(signed(None, Some("the control plane `k` didn't remove it"))));
        assert_eq!(lines.last().unwrap(), "the control plane `k` didn't remove it");
        let proposed = Proposed { name: "box".into(), authority: "lap".into(), id: Some(3), error: None };
        assert_eq!(
            rm_lines(&RmAnswer::Proposed(proposed)),
            ["proposed removing box to lap: approve there with `devsandbox machine approve 3`"]
        );
        let mut by_control_plane = removed(Heard::Leaving, None);
        (by_control_plane.name, by_control_plane.forgot, by_control_plane.missed) = ("lap".into(), vec![], vec![]);
        let left = |removed, error: Option<&str>| Left {
            name: "lap".into(),
            control_plane: "k".into(),
            removed,
            error: error.map(String::from),
            dropped: vec!["k".into()],
            proposed: None,
        };
        assert_eq!(rm_lines(&RmAnswer::Left(left(Some(by_control_plane), None))), ["this host (lap) left k's fleet", "unlinked k"]);
        let lines = rm_lines(&RmAnswer::Left(left(None, Some("no link to it"))));
        assert_eq!(lines[0], "the control plane k wasn't reached (no link to it): it still lists this host until `machine rm lap` there");
        let alone = Left { name: "k".into(), control_plane: "k".into(), removed: None, error: None, dropped: vec![], proposed: None };
        assert_eq!(rm_lines(&RmAnswer::Left(alone)), ["this host (k) was the control plane, alone: no fleet now"]);
        let not_linked = Proposed { name: "lap".into(), authority: "a".into(), id: None, error: Some("not linked".into()) };
        let lines = rm_lines(&RmAnswer::Left(Left { proposed: Some(not_linked), ..left(None, None) }));
        assert_eq!(lines[0], "its own removal not proposed to the authority a: not linked");
    }

    #[test]
    fn pending_lists_id_from_what_and_when() {
        use crate::serve::fleet::proposals::Change;
        assert_eq!(pending_text(&[], 0), "no pending proposals\n");
        let p = Proposal { id: 3, from: "box".into(), op: Change::Revoke { name: "pi".into() }, at: 1_000 };
        let text = pending_text(&[p], 1_000 + 120);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("ID") && lines[0].contains("FROM") && lines[0].contains("WHAT") && lines[0].contains("WHEN"), "{text}");
        assert!(lines[1].contains("box") && lines[1].contains("remove pi from the fleet") && lines[1].trim_end().ends_with("2m"), "{text}");
    }

    fn remote(path: Option<&str>, version: Option<&str>) -> Remote {
        Remote { os: "Linux".into(), arch: "x86_64".into(), path: path.map(Into::into), off_path: false, version: version.map(Into::into), hello: None, sha256: None }
    }

    #[test]
    fn the_key_is_confirmed_on_a_tty_or_by_yes() {
        let fp = "SHA256:aaaa";
        // `--yes`: taken whatever the TTY (no prompt is read in a test).
        for tty in [true, false] {
            confirm_key("me@box", fp, tty, true).unwrap();
        }
        let e = confirm_key("me@box", fp, false, false).unwrap_err().to_string();
        assert_eq!(e, key_refusal("me@box", fp));
        assert!(e.starts_with("me@box's key fingerprint is SHA256:aaaa: pass --yes"), "{e}");
    }

    #[test]
    fn install_asks_on_a_tty_and_needs_the_flag_off_one() {
        assert_eq!(consent(true, false), Consent::Ask);
        assert_eq!(consent(false, false), Consent::Refuse);
        assert_eq!(consent(true, true), Consent::Install);
        assert_eq!(consent(false, true), Consent::Install);
        for yes in ["y", "Y", "yes", "YES"] {
            assert!(is_yes(yes), "{yes}");
        }
        for no in ["", "n", "no", "yep", "y es"] {
            assert!(!is_yes(no), "{no}");
        }
    }

    #[test]
    fn install_messages_name_what_is_there() {
        assert_eq!(
            refusal("box", None, None, "1.0.0"),
            "box has no devsandbox: pass --auto-install to copy this one (1.0.0) there"
        );
        assert_eq!(
            refusal("box", Some("/b/devsandbox"), Some("0.9.0"), "1.0.0"),
            "box runs devsandbox 0.9.0 at /b/devsandbox, this is 1.0.0: pass --auto-install to replace it"
        );
        assert_eq!(
            refusal("box", Some("/b/devsandbox"), None, "1.0.0"),
            "box runs devsandbox of an unknown (older) version at /b/devsandbox, this is 1.0.0: pass --auto-install to replace it"
        );
        assert_eq!(
            question("box", None, None, "1.0.0"),
            "devsandbox isn't on box's PATH. Copy this one (1.0.0) to ~/.local/bin/devsandbox there? [y/N] "
        );
        assert_eq!(
            question("box", Some("/b/devsandbox"), Some("0.9.0"), "1.0.0"),
            "box runs devsandbox 0.9.0 at /b/devsandbox. Replace it with this one (1.0.0)? [y/N] "
        );
        assert_eq!(
            question("box", Some("/b/devsandbox"), None, "1.0.0"),
            "box runs devsandbox (unknown version) at /b/devsandbox. Replace it with this one (1.0.0)? [y/N] "
        );
        let mismatch = Remote { os: "FreeBSD".into(), arch: "riscv64".into(), path: None, off_path: false, version: None, hello: None, sha256: None };
        let why = platform_mismatch("box", &mismatch);
        assert!(why.starts_with("box is FreeBSD riscv64; this devsandbox is built for "), "{why}");
        assert!(!install::platform_matches(&mismatch.os, &mismatch.arch));
    }

    #[test]
    fn upgrade_messages_name_both_builds() {
        let ours = "1.0.0 abc dev linux/x86_64";
        assert_eq!(
            upgrade_what("box", "/b/devsandbox", Some("0.9.0 def published linux/x86_64"), ours),
            "box runs devsandbox 0.9.0 (published, def) at /b/devsandbox; this is 1.0.0 (dev, abc)"
        );
        assert_eq!(
            upgrade_what("box", "/b/devsandbox", None, ours),
            "box runs devsandbox of an unknown (older) build at /b/devsandbox; this is 1.0.0 (dev, abc)"
        );
        let inbound = no_ssh("box", MachineKind::Inbound);
        assert!(inbound.starts_with("box links to this host from its end (inbound): `machine upgrade` copies over ssh"), "{inbound}");
        assert!(no_ssh("pi", MachineKind::Routed).starts_with("pi is reached over a route: "));
        assert!(older_note("box", "1.1.0 x dev linux/x86_64", ours).is_some());
        assert_eq!(older_note("box", "0.9.0 x dev linux/x86_64", ours), None);
        assert_eq!(older_note("box", "1.0.0 x dev linux/x86_64", ours), None);
    }

    #[test]
    fn local_command_is_the_default_dest() {
        assert_eq!(LOCAL_COMMAND.strip_prefix('~'), install::DEFAULT_DEST.strip_prefix("$HOME"));
    }

    #[test]
    fn the_recheck_decides_the_command() {
        let found = Dest::Found("/b/devsandbox".into());
        let local = "/home/me/.local/bin/devsandbox";
        let check = |after: &Remote, dest: &Dest, installed: &str| rechecked(after, dest, "1.0.0", "box", installed);
        // On the login PATH now, wherever it went.
        assert_eq!(check(&remote(Some(local), Some("1.0.0")), &Dest::Default, local).unwrap(), Rechecked::OnPath);
        assert_eq!(check(&remote(Some("/b/devsandbox"), Some("1.0.0")), &found, "/b/devsandbox").unwrap(), Rechecked::OnPath);
        // `~/.local/bin` off the login PATH, or behind another devsandbox.
        assert_eq!(check(&remote(None, None), &Dest::Default, local).unwrap(), Rechecked::OffPath);
        assert_eq!(check(&remote(Some("/opt/devsandbox"), Some("0.1.0")), &Dest::Default, local).unwrap(), Rechecked::OffPath);
        // Updated in place, yet PATH resolves elsewhere.
        let err = check(&remote(Some("/opt/devsandbox"), Some("0.1.0")), &found, "/b/devsandbox").unwrap_err().to_string();
        assert_eq!(err, "updated /b/devsandbox, but `devsandbox` on box's login PATH is /opt/devsandbox (devsandbox 0.1.0)");
        let err = check(&remote(Some("/opt/devsandbox"), None), &found, "/b/devsandbox").unwrap_err().to_string();
        assert!(err.ends_with("(devsandbox of an unknown (older) version)"), "{err}");
        let err = check(&remote(None, None), &found, "/b/devsandbox").unwrap_err().to_string();
        assert!(err.contains("no longer finds a devsandbox"), "{err}");
    }

    #[test]
    fn agent_takes_on_or_off() {
        assert_eq!(parse_on_off("on"), Ok(true));
        assert_eq!(parse_on_off("off"), Ok(false));
        for bad in ["", "yes", "true", "ON"] {
            assert!(parse_on_off(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn trust_takes_worker_or_full_and_full_says_what_it_gives() {
        assert_eq!(parse_trust("worker"), Ok(Trust::Worker));
        assert_eq!(parse_trust("full"), Ok(Trust::Full));
        for bad in ["", "Full", "all"] {
            assert!(parse_trust(bad).is_err(), "{bad:?}");
        }
        assert_eq!(trust_line(Trust::Worker), "trust: worker");
        assert!(trust_line(Trust::Full).contains("whole API"));
    }

    #[test]
    fn key_prints_name_key_and_fingerprint() {
        use crate::serve::fleet::identity::{Signer as _, SigningKey};
        let key = SigningKey::from_seed([2; 32]).public_key();
        let info = MemberInfo {
            name: Some("lap".into()),
            control_plane: None,
            trust: Trust::Worker,
            dev_builds: false,
            roots: Vec::new(),
            allow: AllowInfo::default(),
            fingerprint: key.fingerprint(),
            key,
            next: None,
            exposed_by: None,
            pin_skipped: None,
        };
        assert_eq!(key_text(&info), format!("lap {key}\n{}\n", key.fingerprint()));
        assert!(key_text(&MemberInfo { name: None, ..info.clone() }).starts_with("- ed25519:"));
        let next = SigningKey::from_seed([3; 32]).public_key();
        let pending = MemberInfo { next: Some(crate::serve::fleet::membership_api::KeyInfo::of(&next)), ..info };
        assert!(key_text(&pending).ends_with(&format!("\nrotating to {} ({next}): this host's key once the authority's signed change reaches it\n", next.fingerprint())));
    }

    #[test]
    fn rotation_says_the_new_key_and_where_it_went() {
        use crate::serve::fleet::identity::{Signer as _, SigningKey};
        use crate::serve::machines::changes::{RotateProposed, Rotated};
        let key = SigningKey::from_seed([2; 32]).public_key();
        let signed = Signed { name: "lap".into(), change: "rotate `lap`'s key".into(), pushed: vec!["box".into()], unpushed: vec![], removed: None, error: None };
        let r = Rotation::Rotated(Rotated { key, fingerprint: key.fingerprint(), signed });
        assert_eq!(rotation_lines(&r), [format!("this host's key is now {} ({key})", key.fingerprint()), "signed rotate `lap`'s key".into(), "sent to box".into()]);
        let proposed = Proposed { name: "pi".into(), authority: "lap".into(), id: Some(4), error: None };
        let r = Rotation::Proposed(RotateProposed { key, fingerprint: key.fingerprint(), proposed });
        assert_eq!(
            rotation_lines(&r)[0],
            format!("proposed rotating this host's key to {} to lap: approve there with `devsandbox machine approve 4`", key.fingerprint())
        );
    }

    #[test]
    fn an_adds_proposal_names_who_joins() {
        let theirs = Proposed { name: "box".into(), authority: "lap".into(), id: Some(2), error: None };
        assert_eq!(added_proposal_line("box", &theirs), "proposed its add to lap: approve there with `devsandbox machine approve 2`");
        let ours = Proposed { name: "pi".into(), authority: "lap".into(), id: Some(3), error: None };
        assert_eq!(added_proposal_line("box", &ours), "proposed this host's join to lap: approve there with `devsandbox machine approve 3`");
        let failed = Proposed { error: Some("box: membership.propose: down (failed)".into()), id: None, ..ours };
        assert_eq!(added_proposal_line("box", &failed), "this host's join not proposed to the authority lap: box: membership.propose: down (failed)");
    }

    #[test]
    fn a_control_plane_move_says_where_it_went() {
        let signed = Signed { name: "lap".into(), change: "control plane `lap`".into(), pushed: vec!["box".into()], unpushed: vec![], removed: None, error: None };
        assert_eq!(control_plane_lines(&ControlPlaneMove::Signed(signed)), ["lap is the fleet's control plane", "signed control plane `lap`", "sent to box"]);
        let proposed = Proposed { name: "box".into(), authority: "lap".into(), id: Some(3), error: None };
        assert_eq!(
            control_plane_lines(&ControlPlaneMove::Proposed(proposed)),
            ["proposed making box the fleet's control plane to lap: approve there with `devsandbox machine approve 3`"]
        );
    }

    #[test]
    fn roots_resolve_home_and_cwd() {
        let home = Some(Path::new("/home/me"));
        let cwd = Path::new("/work");
        let r = |arg: &str| resolve_root(home, cwd, arg).unwrap();
        assert_eq!(r("~"), PathBuf::from("/home/me"));
        assert_eq!(r("~/"), PathBuf::from("/home/me"));
        assert_eq!(r("~/cfg"), PathBuf::from("/home/me/cfg"));
        assert_eq!(r("/srv/cfg"), PathBuf::from("/srv/cfg"));
        assert_eq!(r("cfg/x"), PathBuf::from("/work/cfg/x"));
        // `~user` and `a~` aren't expanded.
        assert_eq!(r("~bob/cfg"), PathBuf::from("/work/~bob/cfg"));
        assert_eq!(r("a~"), PathBuf::from("/work/a~"));
        assert!(resolve_root(home, cwd, "").is_err());
        let err = resolve_root(None, cwd, "~/cfg").unwrap_err().to_string();
        assert!(err.contains("$HOME"), "{err}");
        assert_eq!(resolve_root(None, cwd, "cfg").unwrap(), PathBuf::from("/work/cfg"));
    }

    #[test]
    fn allow_flags_become_a_patch() {
        let home = Some(Path::new("/home/me"));
        let cwd = Path::new("/work");
        let params = |c: &AllowChange| allow_params(c, home, cwd).unwrap();
        assert_eq!(params(&AllowChange::default()), None, "no flag: a show");
        let change = AllowChange {
            flags: vec![("gpus", true), ("runtime_socket", false)],
            bind_add: vec!["data".into(), "~/x".into(), "/srv".into()],
            local_env_rm: vec!["TOKEN".into()],
            ..Default::default()
        };
        assert_eq!(
            params(&change),
            Some(json!({"allow": {"gpus": true, "runtime_socket": false, "binds_add": ["/work/data", "/home/me/x", "/srv"], "local_env_rm": ["TOKEN"]}}))
        );
        let rm = AllowChange { bind_rm: vec!["./data".into()], local_env_add: vec!["A".into()], ..Default::default() };
        assert_eq!(params(&rm), Some(json!({"allow": {"binds_rm": ["/work/data"], "local_env_add": ["A"]}})));
        let empty = AllowChange { bind_add: vec!["".into()], ..Default::default() };
        assert!(allow_params(&empty, home, cwd).is_err());
    }

    #[test]
    fn add_allow_flags_become_a_patch() {
        assert_eq!(add_patch("box", &AddAllow::default()).unwrap(), None, "no flag: none");
        let flags = AddAllow { flags: vec!["gpus", "host_network"], binds: vec!["/srv/./data/".into()], local_env: vec!["TOKEN".into()] };
        let want = AllowPatch {
            gpus: Some(true),
            host_network: Some(true),
            binds_add: vec!["/srv/data".into()],
            local_env_add: vec!["TOKEN".into()],
            ..Default::default()
        };
        assert_eq!(add_patch("box", &flags).unwrap(), Some(want));
        for relative in ["data", "~/data", "./data"] {
            let e = add_patch("box", &AddAllow { binds: vec![relative.into()], ..Default::default() }).unwrap_err().to_string();
            assert!(e.contains("absolute") && e.contains("box"), "{e}");
        }
        assert!(add_patch("box", &AddAllow { local_env: vec!["A=B".into()], ..Default::default() }).is_err());
        // What goes on the wire: what `member.set` takes.
        let wire = serde_json::to_value(add_patch("box", &AddAllow { flags: vec!["inbox"], ..Default::default() }).unwrap()).unwrap();
        assert_eq!(serde_json::from_value::<AllowPatch>(wire).unwrap(), AllowPatch { inbox: Some(true), ..Default::default() });
    }

    #[test]
    fn allowances_parse_as_the_cli_spells_them() {
        assert_eq!(parse_allowance("host-network"), Ok("host_network"));
        assert_eq!(parse_allowance("gpus"), Ok("gpus"));
        let e = parse_allowance("host_network").unwrap_err();
        assert!(e.starts_with("`host_network` isn't an allowance (one of: privileged, runtime-socket, "), "{e}");
        assert!(parse_allowance("").is_err());
        // Every allowance can be set.
        for (key, _) in AllowInfo::default().flags() {
            let mut patch = AllowPatch::default();
            set_allowance(&mut patch, key);
            assert_ne!(patch, AllowPatch::default(), "{key}");
        }
    }

    #[test]
    fn add_asks_on_a_tty_without_flags_unless_trust_is_full() {
        assert_eq!(allow_step(true, false, Trust::Worker), AllowStep::Flags);
        assert_eq!(allow_step(true, true, Trust::Full), AllowStep::Flags, "flags are applied whatever the trust");
        assert_eq!(allow_step(false, true, Trust::Worker), AllowStep::Ask);
        assert_eq!(allow_step(false, true, Trust::Full), AllowStep::Full);
        assert_eq!(allow_step(false, false, Trust::Worker), AllowStep::Nothing);
        assert_eq!(allow_step(false, false, Trust::Full), AllowStep::Nothing);
        assert_eq!(
            allow_question("me@box"),
            "Allow on me@box, comma-separated (privileged, runtime-socket, host-network, global-services, dispatcher, inbox, \
             initialize-command, publish, gpus; paths and env vars: --allow-bind, --allow-local-env)? Empty adds none: "
        );
    }

    #[test]
    fn the_answer_adds_the_allowances_it_names() {
        assert_eq!(parse_allow_answer(""), Ok(None));
        assert_eq!(parse_allow_answer(" , "), Ok(None));
        let want = AllowPatch { gpus: Some(true), host_network: Some(true), ..Default::default() };
        assert_eq!(parse_allow_answer("gpus, host-network"), Ok(Some(want.clone())));
        assert_eq!(parse_allow_answer("gpus host-network,gpus"), Ok(Some(want)));
        assert!(parse_allow_answer("gpus, root").unwrap_err().starts_with("`root` isn't an allowance"));
    }

    #[test]
    fn lends_is_one_line() {
        assert_eq!(lends_line("box", &AllowInfo::default()), "box lends placed work: nothing");
        let allow = AllowInfo { gpus: true, publish: true, binds: vec!["/a".into(), "/b".into()], local_env: vec!["T".into()], ..Default::default() };
        assert_eq!(lends_line("box", &allow), "box lends placed work: publish, gpus; binds: /a, /b; local-env: T");
        assert_eq!(lends_line("box", &AllowInfo { binds: vec!["/a".into()], ..Default::default() }), "box lends placed work: binds: /a");
    }

    #[test]
    fn joining_a_fleet_lending_nothing_hints_machine_allow() {
        assert!(newly_in_a_fleet(Joined::This) && newly_in_a_fleet(Joined::New));
        assert!(!newly_in_a_fleet(Joined::Target) && !newly_in_a_fleet(Joined::Same));
        use crate::serve::fleet::identity::{Signer as _, SigningKey};
        let key = SigningKey::from_seed([1; 32]).public_key();
        let own = MemberInfo {
            name: Some("lap".into()),
            control_plane: Some("k".into()),
            trust: Trust::Worker,
            dev_builds: false,
            roots: vec![],
            allow: AllowInfo::default(),
            fingerprint: key.fingerprint(),
            key,
            next: None,
            exposed_by: None,
            pin_skipped: None,
        };
        assert_eq!(own_allow_hint(&own).unwrap(), "note: this host lends nothing to placed work; set it with `devsandbox machine allow`");
        assert_eq!(own_allow_hint(&MemberInfo { trust: Trust::Full, ..own.clone() }), None);
        assert_eq!(own_allow_hint(&MemberInfo { allow: AllowInfo { inbox: true, ..Default::default() }, ..own }), None);
    }

    #[test]
    fn allow_lists_every_allowance() {
        let allow = AllowInfo { gpus: true, runtime_socket: true, binds: vec!["/a".into(), "/b".into()], ..Default::default() };
        assert_eq!(
            allow_text(&allow, Trust::Worker),
            "privileged: off\nruntime-socket: on\nhost-network: off\nglobal-services: off\ndispatcher: off\ninbox: off\n\
             initialize-command: off\npublish: off\ngpus: on\nbinds: /a, /b\nlocal-env: none\n"
        );
        assert!(allow_text(&AllowInfo::default(), Trust::Full).ends_with("limit nothing (devsandbox machine trust worker)\n"));
    }

    #[test]
    fn added_roots_must_be_directories() {
        let dir = std::env::temp_dir().join(format!("dsv-roots-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("f");
        std::fs::write(&file, "").unwrap();
        assert_eq!(existing_dir(&dir.join(".")).unwrap(), dir.canonicalize().unwrap());
        let err = existing_dir(&file).unwrap_err().to_string();
        assert_eq!(err, format!("{} is not a directory", file.display()));
        let err = existing_dir(&dir.join("gone")).unwrap_err().to_string();
        assert_eq!(err, format!("no directory {}", dir.join("gone").display()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn roots_print_one_per_line_or_a_note() {
        assert_eq!(roots_text(&["/a".into(), "/b c".into()]), "/a\n/b c\n");
        assert!(roots_text(&[]).contains("machine roots add"));
    }

    #[test]
    fn ls_cells_show_the_first_root() {
        let row = |roots: &[&str], version: Option<&str>| MachineRow {
            name: "m".into(),
            kind: crate::serve::machines::MachineKind::Ssh,
            target: Some("me@m".into()),
            roots: roots.iter().map(Into::into).collect(),
            command: None,
            agent: false,
            state: crate::serve::snapshots::LinkState::Idle,
            error: None,
            version: version.map(String::from),
            capacity: None,
            introduced_by: None,
            via: None,
            exposed_by: None,
            behind: None,
            behind_here: None,
        };
        assert_eq!(cells(row(&["/a"], None)), ["m", "me@m", "/a", "off", "idle", "-", "-"].map(String::from));
        assert_eq!(cells(row(&["/a", "/b", "/c"], Some("0.7.0")))[2..6], ["/a +2", "off", "idle", "0.7.0"].map(String::from));
        // Capacity from the machine's last snapshot, and what's over a soft
        // cap.
        let g = 1u64 << 30;
        let capacity = crate::serve::capacity::Capacity {
            cpus: Some(8),
            mem_total: Some(32 * g),
            mem_available: Some(14 * g),
            running: 3,
            caps: Some(crate::serve::capacity::Caps { mem: Some(16 * g), ..Default::default() }),
            ..Default::default()
        };
        let full = cells(MachineRow { capacity: Some(capacity), ..row(&[], None) });
        assert_eq!(full[6], "8 cpu · 18/32G · 3 run (over cap: mem 18G/16G)");
        assert_eq!(cells(MachineRow { agent: true, ..row(&[], None) })[3], "on");
        assert_eq!(cells(row(&[], None))[2], "-");
        let inbound = MachineRow { kind: crate::serve::machines::MachineKind::Inbound, target: None, ..row(&[], None) };
        assert_eq!(cells(inbound)[1], "inbound");
        let routed = MachineRow { kind: crate::serve::machines::MachineKind::Routed, target: None, via: Some("box".into()), ..row(&[], None) };
        assert_eq!(cells(routed)[1..4], ["routed via box", "-", "off"].map(String::from), "a routed member's grant shows too");
        // A capped member: named with its introducer.
        let capped = MachineRow { introduced_by: Some("lap".into()), ..row(&[], None) };
        assert_eq!(cells(capped)[0], "m (introduced by lap)");
        // Exposed while `worker`: the machine that dials it.
        let chain = Some(vec!["box".into(), "k".into(), "m".into()]);
        assert_eq!(cells(MachineRow { exposed_by: chain.clone(), ..row(&[], None) })[0], "m (exposed via k)");
        let both = MachineRow { exposed_by: chain, introduced_by: Some("lap".into()), ..row(&[], None) };
        assert_eq!(cells(both)[0], "m (introduced by lap, exposed via k)");
        // Behind: still connected, with why; this host behind is a note.
        let connected = crate::serve::snapshots::LinkState::Connected;
        let behind = MachineRow { state: connected, behind: Some("dev build: not taken".into()), ..row(&[], None) };
        assert_eq!(cells(behind)[4], "connected (behind: dev build: not taken)");
        let here = |why: &str| MachineRow { state: connected, behind_here: Some(why.into()), ..row(&[], None) };
        let rows = [here("dev build: can't run on macos/aarch64"), row(&[], None), here("dev build: can't run on macos/aarch64")];
        assert_eq!(cells(here("x"))[4], "connected");
        assert_eq!(ls_notes(&rows, None), ["this host is behind: dev build: can't run on macos/aarch64"], "once");
        let skipped = "dev build 0.9.0 (dev, 1a2b) not pinned: it runs on linux/x86_64, the control plane on macos/aarch64";
        assert_eq!(ls_notes(&rows[1..2], Some(skipped)), [skipped]);
        assert!(ls_notes(&rows[1..2], None).is_empty());
    }

    #[test]
    fn trust_warns_naming_the_machine_with_a_shell_here() {
        let chain = |c: &[&str]| c.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            exposure_warning(&chain(&["box", "lap"])).unwrap(),
            "box dials this host over ssh, so it has a shell here and could rewrite member.toml: `worker` doesn't protect this host from box (exposed through box > lap)"
        );
        let w = exposure_warning(&chain(&["box", "k", "lap"])).unwrap();
        assert!(w.starts_with("k dials this host") && w.ends_with("(exposed through box > k > lap)"), "{w}");
        assert_eq!(exposure_warning(&chain(&["box"])), None, "the control plane itself");
    }
}
