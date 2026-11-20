//! `devsandbox trust-origin <folder>`: re-pin the origin URL host git
//! fetches for a repo (`commands::run::fetch_trusted_origin`) to the one its
//! config holds now. Local-only: the pin and the checkout are this host's.

use std::path::Path;

use anyhow::{bail, Result};

use crate::commands::run::{read_origin, trust_key};
use crate::state::State;

pub fn trust_origin(folder: &Path) -> Result<()> {
    let (old, new) = trust_origin_at(&State::path()?, folder)?;
    println!("origin of {}: {new}", folder.display());
    match old {
        Some(old) if old == new => println!("already trusted"),
        Some(old) => println!("trusted (was {old})"),
        None => println!("trusted (nothing was pinned)"),
    }
    Ok(())
}

/// Pin `folder`'s current origin URL in the state file `state`: `(the pin it
/// replaced, the URL)`.
fn trust_origin_at(state: &Path, folder: &Path) -> Result<(Option<String>, String)> {
    let origin = read_origin(folder)?;
    let Some(url) = origin.url else { bail!("{} has no `origin` remote", folder.display()) };
    let key = trust_key(&origin.common);
    let old = State::update_at(state, |s| Ok(s.trusted_origins.insert(key, url.clone())))?;
    Ok((old, url))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git").arg("-C").arg(dir).args(args).status().unwrap().success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn re_pins_to_the_url_the_config_holds() {
        let root = std::env::temp_dir().join(format!("devsandbox-trust-origin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (repo, state) = (root.join("repo"), root.join("state.toml"));
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        assert!(trust_origin_at(&state, &repo).unwrap_err().to_string().contains("has no `origin` remote"));

        git(&repo, &["remote", "add", "origin", "/srv/a.git"]);
        let key = trust_key(&repo.join(".git").canonicalize().unwrap());
        State::update_at(&state, |s| Ok(s.trusted_origins.insert(key.clone(), "/srv/old.git".into()))).unwrap();
        assert_eq!(trust_origin_at(&state, &repo).unwrap(), (Some("/srv/old.git".into()), "/srv/a.git".into()));
        assert_eq!(State::load_at(&state).unwrap().trusted_origins[&key], "/srv/a.git");
        assert_eq!(trust_origin_at(&state, &repo).unwrap(), (Some("/srv/a.git".into()), "/srv/a.git".into()));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
