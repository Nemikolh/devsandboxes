//! What `run` reads from the built image (docs/architecture/runtimes.md,
//! *Image facts*): its `ENV`, `USER` and `/etc/passwd`, once per image id,
//! cached in `state.toml` (`State::image_facts`). They give the identity
//! plan the adopted account's real ids, `${containerEnv:…}` its values and
//! a root plan's tool env the image's `PATH`.

use crate::config::SandboxProperties;
use crate::runtime::backend;
use crate::runtime::identity::{self, Accounts, Identity, Ids};
use crate::state::{ImageFacts, Instance, State};

/// The facts of `image` (built or pulled, local): the cache's when its id is
/// there, else read now and cached. Never fails: an unreadable fact reads
/// as unset and isn't cached (the next run tries again).
pub(super) fn image_facts(image: &str) -> ImageFacts {
    let b = backend();
    let id = b.image_id(image).ok().flatten();
    let cached = |id: &str| State::load().ok().and_then(|s| s.image_facts.get(id).cloned());
    let read = || {
        let env = b.image_env(image);
        let user = b.image_user(image);
        let passwd = b.image_passwd(image);
        let complete = env.is_ok() && user.is_ok() && passwd.is_ok();
        let facts = ImageFacts {
            env: env.unwrap_or_default(),
            user: user.ok().flatten(),
            passwd: passwd.ok().flatten(),
            at: Instance::now(),
        };
        (facts, complete)
    };
    let (facts, store) = decide(id, cached, read);
    if let Some((id, facts)) = store {
        let _ = State::update(|state| {
            state.remember_image_facts(id, facts);
            Ok(())
        });
    }
    facts
}

/// The cache decision, I/O injected: a cached entry for `id` wins; else
/// `read` (facts, whether every read worked), to be stored (the second
/// value) only when complete and there's an id to key it by.
fn decide(
    id: Option<String>,
    cached: impl FnOnce(&str) -> Option<ImageFacts>,
    read: impl FnOnce() -> (ImageFacts, bool),
) -> (ImageFacts, Option<(String, ImageFacts)>) {
    if let Some(facts) = id.as_deref().and_then(cached) {
        return (facts, None);
    }
    let (facts, complete) = read();
    let store = id.filter(|_| complete).map(|id| (id, facts.clone()));
    (facts, store)
}

/// The user fields the plan starts from: the config's, the image's `USER`,
/// and the adopted account's ids when the image's passwd has it.
/// `image_user` only counts for `identity = "host"` when the config names
/// no user (else the plan never adopts it); [`home`] reads it either way.
pub(super) fn accounts<'a>(props: &'a SandboxProperties, facts: &'a ImageFacts) -> Accounts<'a> {
    let image_user =
        (props.identity() == Identity::Host && props.container_user.is_none() && props.remote_user.is_none())
            .then_some(facts.user.as_deref())
            .flatten();
    let mut accounts = Accounts {
        container_user: props.container_user.as_deref(),
        remote_user: props.remote_user.as_deref(),
        image_user,
        ids: None,
    };
    accounts.ids = account_ids(&accounts, facts.passwd.as_deref());
    accounts
}

/// The adopted account's ids in `passwd`.
fn account_ids(accounts: &Accounts, passwd: Option<&str>) -> Option<Ids> {
    let a = identity::account(passwd?, accounts.adopted()?)?;
    Some(Ids { uid: a.uid, gid: a.gid })
}

/// `${containerEnv:HOME}`'s fallback (`identity::container_home`): the
/// account the container runs as, whatever the plan, so the image's `USER`
/// counts whenever the config names no user.
pub(super) fn home(props: &SandboxProperties, facts: &ImageFacts) -> String {
    let accounts = Accounts {
        container_user: props.container_user.as_deref(),
        remote_user: props.remote_user.as_deref(),
        image_user: facts.user.as_deref(),
        ids: None,
    };
    identity::container_home(&accounts, facts.passwd.as_deref())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn facts(at: u64) -> ImageFacts {
        ImageFacts { at, ..Default::default() }
    }

    #[test]
    fn cache_hit_reads_nothing() {
        let (got, store) = decide(Some("sha256:a".into()), |_| Some(facts(1)), || panic!("read"));
        assert_eq!((got, store), (facts(1), None));
    }

    #[test]
    fn miss_reads_and_stores_only_complete_facts_with_an_id() {
        let (got, store) = decide(Some("sha256:a".into()), |_| None, || (facts(2), true));
        assert_eq!(got, facts(2));
        assert_eq!(store, Some(("sha256:a".into(), facts(2))));
        // A failed read isn't cached: the next run tries again.
        let (_, store) = decide(Some("sha256:a".into()), |_| None, || (facts(2), false));
        assert_eq!(store, None);
        // No id (Apple `container`): read every time, nothing cached.
        let looked = Cell::new(false);
        let (got, store) = decide(None, |_| { looked.set(true); None }, || (facts(3), true));
        assert_eq!((got, store, looked.get()), (facts(3), None, false));
    }

    const PASSWD: &str = "root:x:0:0:root:/root:/bin/sh\nnode:x:1000:1000::/home/node:/bin/sh\nvscode:x:1001:1001::/home/vscode:/bin/zsh\n";

    fn props(toml: &str) -> SandboxProperties {
        let config = crate::config::Config::parse(&format!("[sandbox.s]\nimage = \"x\"\n{toml}")).unwrap();
        config.resolve_sandbox("s").unwrap().properties
    }

    #[test]
    fn accounts_carry_the_real_ids() {
        let f = ImageFacts { user: Some("node".into()), passwd: Some(PASSWD.into()), ..Default::default() };
        let p = props("identity = \"host\"\nremoteUser = \"vscode\"");
        let a = accounts(&p, &f);
        assert_eq!((a.adopted(), a.ids, a.image_user), (Some("vscode"), Some(Ids { uid: 1001, gid: 1001 }), None));
        let p = props("identity = \"host\"");
        let a = accounts(&p, &f);
        assert_eq!((a.adopted(), a.ids), (Some("node"), Some(Ids { uid: 1000, gid: 1000 })));
        // No passwd: unknown ids (the plan assumes).
        assert_eq!(accounts(&p, &ImageFacts { user: Some("node".into()), ..Default::default() }).ids, None);
    }

    #[test]
    fn home_follows_the_account_the_container_runs_as() {
        let f = ImageFacts { user: Some("node".into()), passwd: Some(PASSWD.into()), ..Default::default() };
        assert_eq!(home(&props(""), &f), "/home/node");
        assert_eq!(home(&props("identity = \"image\""), &f), "/home/node");
        assert_eq!(home(&props("remoteUser = \"vscode\""), &f), "/home/vscode");
        assert_eq!(home(&props("remoteUser = \"root\""), &f), "/root");
        assert_eq!(home(&props(""), &ImageFacts::default()), "/root");
    }
}
