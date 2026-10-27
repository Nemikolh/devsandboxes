//! The hub side of an offer (docs/architecture/daemon/fleet.md, *Hub
//! side*): `fleet.offer {worker, version, root, draining?}`, sent by a worker's `fleet-link`
//! once its link's `Hello`s are through, writes (or keeps) the worker's
//! offered entry in `machines.toml`, `root` (the worker's fleet root) its
//! one root; and [`dial`], how this host reaches an
//! offered machine: its `fleet-<worker>.sock`.
//!
//! Internal: devsandbox's own `fleet-link` is the only caller (not in the
//! npm typings). Connection-free like `machines::api`: a function of the
//! params and a [`Ctx`].

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::serve::api::{ApiError, parse};
use crate::serve::endpoint;
use crate::serve::machines::service::Handle;
use crate::serve::machines::ssh::{Dial, Duplex};
use crate::serve::machines::{self, Entry, check_name};

type Answer = Result<Value, ApiError>;

/// What `fleet.offer` needs from the daemon.
pub struct Ctx<'a> {
    /// `machines.toml`.
    pub registry: &'a Path,
    /// This daemon is a hub (`hub = true` under `[serve]`,
    /// `daemon_config::ServeConfig::hub`): only a hub accepts offers.
    pub hub: bool,
    /// The links, reloaded when an entry is added.
    pub links: Option<&'a Handle>,
    pub log: fn(&str),
}

/// Run `method` if it's a `fleet.*` one; `None` otherwise.
pub fn call(method: &str, params: Value, ctx: &Ctx) -> Option<Answer> {
    match method {
        "fleet.offer" => Some(parse(params).and_then(|p| offer(p, ctx))),
        _ => None,
    }
}

#[derive(Deserialize)]
struct OfferParams {
    worker: String,
    /// The worker's build, its link `Hello` (`<semver> <git>
    /// <published|dev> <os>/<arch>`, `fleet::update::HELLO`), for the log.
    version: String,
    /// The worker's canonical fleet root (a path there): the entry's one
    /// root, which this hub's link watches.
    root: PathBuf,
    /// The worker's `lend` was `drain` when it linked: it takes no new work
    /// (logged only: placement reads `drain` from the capacity report).
    #[serde(default)]
    draining: bool,
}

/// What an offer does to the registry.
#[derive(Debug, PartialEq, Eq)]
pub enum Offered {
    /// A new offered entry.
    Added,
    /// The worker's entry is there already, with this root.
    Kept,
    /// The worker's entry is there; its root was another (or none).
    Updated,
}

/// Accept `worker`'s offer of fleet root `root` into `entries` (pure): a
/// new name becomes an offered entry with `root` its one root, an offered
/// one keeps its name and gets `root` if it had another; a name an ssh
/// machine has is refused (decision 2 of the fleet plan: a worker never
/// takes over a machine this host sshes to).
pub fn accept(entries: &mut Vec<Entry>, worker: &str, root: &Path) -> Result<Offered, ApiError> {
    let roots = vec![root.to_path_buf()];
    match entries.iter_mut().find(|e| e.name == worker) {
        Some(e) if e.is_offered() && e.roots == roots => Ok(Offered::Kept),
        Some(e) if e.is_offered() => {
            e.roots = roots;
            Ok(Offered::Updated)
        }
        Some(e) => Err(ApiError::denied(format!(
            "`{worker}` is an ssh machine here ({}): a fleet worker needs another name",
            e.label()
        ))),
        None => {
            entries.push(Entry { roots, ..Entry::offered(worker) });
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(Offered::Added)
        }
    }
}

fn offer(p: OfferParams, ctx: &Ctx) -> Answer {
    check_name(&p.worker).map_err(|e| ApiError::invalid(format!("{e:#}")))?;
    if !p.root.is_absolute() {
        return Err(ApiError::invalid(format!("`root` {} isn't an absolute path", p.root.display())));
    }
    if !ctx.hub {
        return Err(ApiError::denied(
            "this devsandbox isn't a fleet hub (`hub = true` under `[serve]` in daemon.config.toml)",
        ));
    }
    let offered = machines::update(ctx.registry, |entries| accept(entries, &p.worker, &p.root)).map_err(ApiError::internal)??;
    (ctx.log)(&format!(
        "fleet: worker {} offered (devsandbox {}{}, root {}){}",
        p.worker,
        p.version,
        if p.draining { ", draining" } else { "" },
        p.root.display(),
        match offered {
            Offered::Added => ", added",
            Offered::Updated => ", root updated",
            Offered::Kept => "",
        }
    ));
    // A changed entry restarts its link, which then watches the new root.
    if offered != Offered::Kept
        && let Some(links) = ctx.links
    {
        links.reload();
    }
    Ok(json!({ "ok": true }))
}

/// The daemon's [`Dial`]: an offered machine is a connection to its
/// worker's `fleet-link` socket in `socket_dir` (one `API` stream on the
/// held link, which starts with the API relay's sentinel line, so
/// `machines::link::handshake` reads it as it reads an ssh relay's);
/// anything else goes to `ssh`.
pub fn dial(socket_dir: PathBuf, ssh: Dial) -> Dial {
    Arc::new(move |entry: &Entry| {
        if !entry.is_offered() {
            return ssh(entry);
        }
        let path = endpoint::fleet_socket(&socket_dir, &entry.name);
        let stream = endpoint::Stream::connect(&path)
            .map_err(|e| anyhow::anyhow!("its fleet link isn't up ({}: {e})", path.display()))?;
        Ok(Duplex::socket(stream))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(registry: &Path, hub: bool) -> Ctx<'_> {
        Ctx { registry, hub, links: None, log: |_| {} }
    }

    fn lap(root: &str) -> Entry {
        Entry { roots: vec![root.into()], ..Entry::offered("lap") }
    }

    #[test]
    fn accept_adds_keeps_updates_and_refuses_ssh_names() {
        let mut entries = vec![Entry::ssh("srv", "me@srv")];
        let root = Path::new("/d/fleet/hub");
        assert_eq!(accept(&mut entries, "lap", root).unwrap(), Offered::Added);
        assert_eq!(entries, vec![lap("/d/fleet/hub"), Entry::ssh("srv", "me@srv")]);
        assert_eq!(accept(&mut entries, "lap", root).unwrap(), Offered::Kept);
        assert_eq!(entries.len(), 2);
        // Another fleet root (the worker's data dir moved): the one root now.
        assert_eq!(accept(&mut entries, "lap", Path::new("/e/fleet/hub")).unwrap(), Offered::Updated);
        assert_eq!(entries[0], lap("/e/fleet/hub"));
        // An entry from before roots were offered gets its root.
        let mut old = vec![Entry::offered("lap")];
        assert_eq!(accept(&mut old, "lap", root).unwrap(), Offered::Updated);
        assert_eq!(old, [lap("/d/fleet/hub")]);
        let e = accept(&mut entries, "srv", root).unwrap_err();
        assert_eq!(e.code, "denied");
        assert!(e.message.contains("ssh machine here (me@srv)"), "{}", e.message);
    }

    #[test]
    fn offers_are_accepted_by_a_hub_only() {
        let dir = std::env::temp_dir().join(format!("dsv-offer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let registry = dir.join("machines.toml");
        let params = |w: &str| json!({"worker": w, "version": "0.6.0", "root": "/d/fleet/hub"});

        let e = call("fleet.offer", params("lap"), &ctx(&registry, false)).unwrap().unwrap_err();
        assert!(e.code == "denied" && e.message.contains("isn't a fleet hub"), "{e:?}");
        assert!(!registry.exists(), "nothing written");

        let hub = ctx(&registry, true);
        assert_eq!(call("fleet.offer", params("lap"), &hub).unwrap().unwrap(), json!({"ok": true}));
        assert_eq!(machines::load(&registry).unwrap(), vec![lap("/d/fleet/hub")]);
        assert!(std::fs::read_to_string(&registry).unwrap().contains("offered = true"));
        // Again (draining this time): kept, not duplicated.
        let draining = json!({"worker": "lap", "version": "0.6.0", "root": "/d/fleet/hub", "draining": true});
        call("fleet.offer", draining, &hub).unwrap().unwrap();
        assert_eq!(machines::load(&registry).unwrap(), vec![lap("/d/fleet/hub")]);
        // Another root: written.
        let moved = json!({"worker": "lap", "version": "0.6.0", "root": "/e/fleet/hub"});
        call("fleet.offer", moved, &hub).unwrap().unwrap();
        assert_eq!(machines::load(&registry).unwrap(), vec![lap("/e/fleet/hub")]);
        let relative = json!({"worker": "lap", "version": "0.6.0", "root": "fleet/hub"});
        assert_eq!(call("fleet.offer", relative, &hub).unwrap().unwrap_err().code, "invalid");
        assert_eq!(machines::load(&registry).unwrap(), vec![lap("/e/fleet/hub")]);

        machines::update(&registry, |e| e.push(Entry::ssh("srv", "me@srv"))).unwrap();
        assert_eq!(call("fleet.offer", params("srv"), &hub).unwrap().unwrap_err().code, "denied");
        assert_eq!(call("fleet.offer", params("a/b"), &hub).unwrap().unwrap_err().code, "invalid");
        assert_eq!(call("fleet.offer", json!({"worker": "x"}), &hub).unwrap().unwrap_err().code, "invalid");
        assert!(call("fleet.nope", Value::Null, &hub).is_none());
        // Internal: not in the npm typings.
        assert!(crate::serve::dts::prop_type("ApiMethods", "fleet.offer").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_dial_picks_the_fleet_socket_for_offered_machines_only() {
        let dir = std::env::temp_dir().join(format!("dsv-odial-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ssh: Dial = Arc::new(|e: &Entry| anyhow::bail!("ssh to {}", e.label()));
        let d = dial(dir.clone(), ssh);
        assert_eq!(format!("{:#}", d(&Entry::ssh("srv", "me@srv")).err().unwrap()), "ssh to me@srv");
        let down = format!("{:#}", d(&Entry::offered("lap")).err().unwrap());
        assert!(down.starts_with("its fleet link isn't up"), "{down}");
        let _listener = std::os::unix::net::UnixListener::bind(endpoint::fleet_socket(&dir, "lap")).unwrap();
        assert!(d(&Entry::offered("lap")).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
