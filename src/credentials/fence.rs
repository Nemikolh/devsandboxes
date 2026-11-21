//! The folder's remotes as the GitHub repo fence sees them
//! (docs/architecture/credentials/fence.md): which remote URLs a folder's
//! record keeps, and the `owner/repo` each `github.com` one names. Pure but
//! for [`ssh_hostname`], the one host call, which callers inject.

use std::process::{Command, Stdio};

use anyhow::{Result, bail};

/// A remote URL that names a network host: `<scheme>://` for a scheme in
/// [`SCHEMES`], or scp form `[user@]host:path`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Remote<'a> {
    pub scheme: Scheme,
    /// Lowercase.
    pub host: String,
    pub port: Option<&'a str>,
    /// After the host: `/o/r.git` (URL form), `o/r.git` (scp form).
    pub path: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scheme {
    Https,
    Http,
    Git,
    /// `ssh://`, `git+ssh://`, `ssh+git://` and scp form.
    Ssh,
}

const SCHEMES: [(&str, Scheme); 6] = [
    ("https", Scheme::Https),
    ("http", Scheme::Http),
    ("git", Scheme::Git),
    ("ssh", Scheme::Ssh),
    ("git+ssh", Scheme::Ssh),
    ("ssh+git", Scheme::Ssh),
];

/// The hosts GitHub serves git over ssh on (`ssh.github.com`: port 443).
const GITHUB_SSH: [&str; 2] = ["github.com", "ssh.github.com"];

/// `url` as a [`Remote`], or `None` for what a folder's record drops: a
/// host path, `file://`, `ext::` and every other `<transport>::` remote
/// helper, a scheme outside [`SCHEMES`], and any host that isn't
/// [`plain_host`] (an IPv6 literal, a leading `-` ssh would read as an
/// option).
pub(crate) fn parse(url: &str) -> Option<Remote<'_>> {
    if url.contains("::") || url.chars().any(|c| c.is_control() || c == ' ') {
        return None;
    }
    if let Some((scheme, rest)) = url.split_once("://") {
        let scheme = SCHEMES.iter().find(|(s, _)| s.eq_ignore_ascii_case(scheme))?.1;
        let (authority, path) = rest.find('/').map_or((rest, ""), |i| rest.split_at(i));
        let hostport = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        let (host, port) = match hostport.split_once(':') {
            Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => (h, Some(p)),
            Some(_) => return None,
            None => (hostport, None),
        };
        return plain_host(host).then(|| Remote { scheme, host: host.to_ascii_lowercase(), port, path });
    }
    // git's rule: a `:` before any `/` makes it scp form, else a path.
    let colon = url.find(':')?;
    if url[..colon].contains('/') {
        return None;
    }
    let (userhost, path) = (&url[..colon], &url[colon + 1..]);
    let host = userhost.rsplit_once('@').map_or(userhost, |(_, h)| h);
    plain_host(host).then(|| Remote { scheme: Scheme::Ssh, host: host.to_ascii_lowercase(), port: None, path })
}

/// A name `ssh -G` may be handed: letters, digits, `.`, `-`, `_`, starting
/// with a letter or digit, at most 253 bytes. `.git/config` wrote it, and
/// ssh substitutes it (`%h`) into a user's `Match exec` commands.
pub(crate) fn plain_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host.starts_with(|c: char| c.is_ascii_alphanumeric())
        && host.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

/// The `owner/repo` (normalized: [`normalize`]) a recorded remote URL names
/// on GitHub: `https://github.com/…`, `ssh://[user@]github.com/…` or scp
/// `[user@]github.com:…`, an ssh host resolved through `resolve` (an alias
/// such as `github-work`, `ssh -G`'s `hostname`) when it isn't GitHub's
/// own. `None`: not a GitHub repo URL.
pub(crate) fn github_repo(url: &str, resolve: &mut dyn FnMut(&str) -> Option<String>) -> Option<String> {
    let remote = parse(url)?;
    let github = match remote.scheme {
        Scheme::Https => remote.host == "github.com" && remote.port.is_none_or(|p| p == "443"),
        Scheme::Ssh if remote.port.is_none_or(|p| p == "22") => {
            GITHUB_SSH.contains(&remote.host.as_str())
                || resolve(&remote.host).is_some_and(|h| GITHUB_SSH.iter().any(|g| h.eq_ignore_ascii_case(g)))
        }
        _ => false,
    };
    if !github {
        return None;
    }
    let path = match remote.scheme {
        Scheme::Https => {
            let path = remote.path.strip_prefix('/')?;
            path.strip_suffix('/').unwrap_or(path)
        }
        _ => remote.path.strip_prefix('/').unwrap_or(remote.path),
    };
    let (owner, repo) = path.split_once('/')?;
    normalize(owner, repo)
}

/// `owner/repo` lowercased, `.wiki.git` or `.git` dropped from `repo`;
/// `None` for anything GitHub wouldn't name a repo by: an empty or `.`/`..`
/// part, another segment, an encoded or other character outside GitHub's
/// sets (owners `[A-Za-z0-9-]`, repos `[A-Za-z0-9._-]`). Strict, never
/// decoded (github-and-sentry-access, *Matching*).
pub(crate) fn normalize(owner: &str, repo: &str) -> Option<String> {
    let repo = repo.strip_suffix(".wiki.git").or_else(|| repo.strip_suffix(".git")).unwrap_or(repo);
    let owner_ok = !owner.is_empty() && owner.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    let repo_ok = !matches!(repo, "" | "." | "..")
        && repo.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));
    (owner_ok && repo_ok).then(|| format!("{}/{}", owner.to_ascii_lowercase(), repo.to_ascii_lowercase()))
}

/// `access forget`'s `<repo>`: `owner/repo`, or a GitHub URL of it (no
/// alias resolved).
pub(crate) fn repo_arg(arg: &str) -> Result<String> {
    let direct = arg.split_once('/').and_then(|(o, r)| normalize(o, r));
    match direct.or_else(|| github_repo(arg, &mut |_| None)) {
        Some(repo) => Ok(repo),
        None => bail!("`{arg}` names no GitHub repo: give `owner/repo` or its https://github.com/ URL"),
    }
}

/// The `hostname` line of `ssh -G` output (`hostname github.com`).
pub(crate) fn ssh_g_hostname(out: &str) -> Option<String> {
    out.lines()
        .filter_map(|l| l.split_once(' '))
        .find(|(k, _)| k.eq_ignore_ascii_case("hostname"))
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The host this user's ssh config resolves `alias` to (`ssh -G -- alias`,
/// no connection made); `None` for an alias that isn't [`plain_host`], or
/// no ssh.
pub(crate) fn ssh_hostname(alias: &str) -> Option<String> {
    if !plain_host(alias) {
        return None;
    }
    let out = Command::new("ssh")
        .args(["-G", "--", alias])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    ssh_g_hostname(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aliases(alias: &str) -> Option<String> {
        match alias {
            "github-work" => Some("github.com".into()),
            "gh443" => Some("SSH.GitHub.com".into()),
            other => Some(other.into()),
        }
    }

    #[test]
    fn github_urls_in_every_form() {
        let table: &[(&str, Option<&str>)] = &[
            ("https://github.com/Owner/Repo", Some("owner/repo")),
            ("https://github.com/o/r.git", Some("o/r")),
            ("https://github.com/o/r/", Some("o/r")),
            ("https://user:tok@github.com/o/r.git", Some("o/r")),
            ("HTTPS://GitHub.COM/o/r", Some("o/r")),
            ("https://github.com:443/o/r", Some("o/r")),
            ("https://github.com/o/r.wiki.git", Some("o/r")),
            ("https://github.com/o/my.repo_x-1", Some("o/my.repo_x-1")),
            ("ssh://git@github.com/o/r.git", Some("o/r")),
            ("ssh://git@github.com:22/o/r", Some("o/r")),
            ("git+ssh://git@github.com/o/r", Some("o/r")),
            ("git@github.com:o/r.git", Some("o/r")),
            ("github.com:o/r", Some("o/r")),
            ("git@github.com:/o/r.git", Some("o/r")),
            ("git@ssh.github.com:o/r", Some("o/r")),
            ("git@github-work:Work/Svc.git", Some("work/svc")),
            ("ssh://git@gh443/o/r", Some("o/r")),
            // Not GitHub repos.
            ("https://gitlab.com/o/r", None),
            ("http://github.com/o/r", None),
            ("git://github.com/o/r", None),
            ("https://github.com:8443/o/r", None),
            ("ssh://git@github.com:2222/o/r", None),
            ("https://github.com/o", None),
            ("https://github.com/o/r/tree/main", None),
            ("https://github.com/o/r.git/info/refs", None),
            ("https://github.com//o/r", None),
            ("https://github.com/o/../x/r", None),
            ("https://github.com/o/..", None),
            ("https://github.com/o/%72", None),
            ("https://github.com/o%2Fx/r", None),
            ("https://github.com/o/r?x=1", None),
            ("https://github.com/o/r#x", None),
            ("https://github.com/o/.git", None),
            ("https://github.com/o_x/r", None),
            ("git@evil.example:o/r", None),
            // Dropped outright.
            ("ext::ssh -o ProxyCommand=x github.com %S o/r", None),
            ("file:///home/u/o/r", None),
            ("/home/u/github.com:o/r", None),
            ("./github.com:o/r", None),
            ("fd::3", None),
            ("https://[::1]/o/r", None),
            ("git@-oProxyCommand=x:o/r", None),
        ];
        for (url, want) in table {
            assert_eq!(github_repo(url, &mut aliases).as_deref(), *want, "{url}");
        }
    }

    #[test]
    fn records_network_remotes_only() {
        for kept in ["https://gitlab.com/o/r", "git@gitlab.com:o/r", "ssh://h:2222/r", "git://h/r", "http://h/r"] {
            assert!(parse(kept).is_some(), "{kept}");
        }
        for dropped in [
            "/home/u/r",
            "../r",
            "r",
            "file:///r",
            "FILE:///r",
            "ext::sh -c x",
            "persistent-https://h/r",
            "https://h:x/r",
            "https://-h/r",
            "git@h r:o/r",
            "https://h/r\nx",
        ] {
            assert!(parse(dropped).is_none(), "{dropped}");
        }
    }

    #[test]
    fn resolves_only_plain_host_aliases() {
        let mut asked = Vec::new();
        let mut resolve = |a: &str| {
            asked.push(a.to_string());
            Some("github.com".to_string())
        };
        assert_eq!(github_repo("git@work_gh:o/r", &mut resolve).as_deref(), Some("o/r"));
        assert_eq!(github_repo("git@$(touch x):o/r", &mut resolve), None);
        assert_eq!(github_repo("https://gh-alias/o/r", &mut resolve), None, "https never resolves aliases");
        assert_eq!(asked, ["work_gh"]);
        for bad in ["", "-oProxyCommand", ".x", "a b", "a;b", "a%h", &"a".repeat(254)] {
            assert!(!plain_host(bad), "{bad}");
        }
        assert!(plain_host("github-work.example_1"));
        assert_eq!(ssh_hostname("$(x)"), None);
    }

    #[test]
    fn reads_ssh_g_output() {
        let out = "user git\nhostname github.com\nport 22\nhostname other\n";
        assert_eq!(ssh_g_hostname(out).as_deref(), Some("github.com"));
        assert_eq!(ssh_g_hostname("user git\n"), None);
        assert_eq!(ssh_g_hostname("hostname \n"), None);
    }

    #[test]
    fn forget_takes_owner_repo_or_a_url() {
        assert_eq!(repo_arg("O/R.git").unwrap(), "o/r");
        assert_eq!(repo_arg("git@github.com:o/r").unwrap(), "o/r");
        assert_eq!(repo_arg("https://github.com/o/r").unwrap(), "o/r");
        assert!(repo_arg("o").is_err());
        assert!(repo_arg("o/r/x").is_err());
        assert!(repo_arg("git@github-work:o/r").is_err());
    }
}
