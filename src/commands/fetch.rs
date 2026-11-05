//! `devsandbox fetch <machine>/<instance>`: fetch a placed instance's branch
//! and WIP back into the local checkout it was placed from, as hidden refs
//! (docs/architecture/daemon/fleet.md, *Fetch-back*). A client of this
//! host's daemon's `placements.fetch` only: the links to the worker are the
//! daemon's.

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::serve::fleet::code::fetch::{CLI_WAIT, FetchedBack, PLACEMENTS_FETCH};
use crate::serve::machines::{Where, parse_name};
use crate::serve::{client, endpoint};

pub fn fetch(name: &str) -> Result<()> {
    let (machine, instance) = target(name)?;
    let mut conn = client::connect(&endpoint::socket_dir()?, "cli").context("`devsandbox fetch` needs devsandbox serve")?;
    let params = json!({ "machine": machine, "instance": instance });
    let r = conn
        .call_within(PLACEMENTS_FETCH, params, CLI_WAIT)?
        .with_context(|| format!("no answer from devsandbox serve within {} min", CLI_WAIT.as_secs() / 60))?;
    if let Some(e) = r.error {
        bail!("{}", e.message);
    }
    let got: FetchedBack = serde_json::from_value(r.result.unwrap_or_default()).context("unexpected placements.fetch answer")?;
    for line in lines(&got) {
        println!("{line}");
    }
    Ok(())
}

/// `<machine>/<instance>`, or a bare `<instance>` the daemon resolves among
/// this host's placements; `@/<name>` is this host's own, never placed.
fn target(name: &str) -> Result<(Option<String>, String)> {
    match parse_name(name)? {
        (Where::Machine(m), i) => Ok((Some(m), i)),
        (Where::Any, i) => Ok((None, i)),
        (Where::Local, i) => bail!("`@/{i}` is this host's: fetch takes an instance this host placed on a machine (`<machine>/{i}`)"),
    }
}

/// What `fetch` prints: the refs set, then how to look at them.
fn lines(got: &FetchedBack) -> Vec<String> {
    let who = format!("{}/{}", got.machine, got.instance);
    let mut out = vec![if got.fetched == 0 {
        format!("{who}: nothing new; refs in {}", got.folder.display())
    } else {
        format!("fetched {who} into {} ({} bytes)", got.folder.display(), got.fetched)
    }];
    for (name, id) in &got.refs {
        out.push(format!("  {name} → {}", &id[..id.len().min(12)]));
    }
    if let Some(wip) = got.refs.keys().find(|n| n.ends_with("/wip")) {
        out.push(format!("look: git -C {} log {wip}", got.folder.display()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_target_is_qualified_or_bare() {
        assert_eq!(target("lap/web-2").unwrap(), (Some("lap".into()), "web-2".into()));
        assert_eq!(target("web-2").unwrap(), (None, "web-2".into()));
        assert!(target("@/web-2").unwrap_err().to_string().contains("this host's"));
        assert!(target("lap/").is_err());
    }

    #[test]
    fn the_refs_print_with_how_to_look() {
        let mut got = FetchedBack {
            machine: "lap".into(),
            instance: "web-2".into(),
            folder: "/w/web".into(),
            refs: [
                ("refs/devsandbox/lap/web-2/heads/joan/web-2".to_string(), "a".repeat(40)),
                ("refs/devsandbox/lap/web-2/wip".to_string(), "b".repeat(40)),
            ]
            .into(),
            fetched: 1234,
        };
        assert_eq!(
            lines(&got),
            [
                "fetched lap/web-2 into /w/web (1234 bytes)",
                "  refs/devsandbox/lap/web-2/heads/joan/web-2 → aaaaaaaaaaaa",
                "  refs/devsandbox/lap/web-2/wip → bbbbbbbbbbbb",
                "look: git -C /w/web log refs/devsandbox/lap/web-2/wip",
            ]
        );
        got.fetched = 0;
        assert_eq!(lines(&got)[0], "lap/web-2: nothing new; refs in /w/web");
    }
}
