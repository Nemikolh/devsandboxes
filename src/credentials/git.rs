//! git in the container under a git-host claim
//! (docs/architecture/credentials/git.md): the git settings that send
//! `https://<host>/` through git's own proxy port with the member CA and
//! devsbd's credential helper, as command-scope env (`GIT_CONFIG_COUNT`/
//! `_KEY_<n>`/`_VALUE_<n>`, read last) merged behind a config's own, and
//! as a system gitconfig copy for tools that clear the env. Pure but for
//! the copy's root script, which `run` (`commands::run::claims`) execs.

use crate::state::FolderRemotes;

/// The only host the repo fence knows: owners get identity rewrites there.
pub const GITHUB: &str = "github.com";

/// Where the system copy lives, included from `/etc/gitconfig`.
pub const SYSTEM_FILE: &str = "/etc/gitconfig.d/devsandbox";

/// The credential helper git runs (`!` = through the shell, `get` /
/// `store` / `erase` appended).
pub fn helper() -> String {
    format!("!{} credential", crate::devsbd::BIN)
}

/// The settings, in the order git must read them (each helper list reset
/// before devsbd's is set): per host `H` of `hosts`, the ssh forms
/// rewritten to `https://H/`, then per owner in `owners` (as the recorded
/// URLs spell them; `github.com` only) `https://H/<owner>/` mapped to
/// itself for fetch and push, so a dotfile's shorter rewrite of
/// `https://H/` loses to the longer match; then `H`'s proxy, CA bundle and
/// credential helper.
pub fn entries(hosts: &[String], owners: &[String], port: u16, bundle: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut set = |k: String, v: String| out.push((k, v));
    for host in hosts {
        let base = format!("https://{host}/");
        set(format!("url.{base}.insteadOf"), format!("git@{host}:"));
        set(format!("url.{base}.insteadOf"), format!("ssh://git@{host}/"));
        for owner in owners.iter().filter(|_| host == GITHUB) {
            let mine = format!("{base}{owner}/");
            set(format!("url.{mine}.insteadOf"), mine.clone());
            set(format!("url.{mine}.pushInsteadOf"), mine);
        }
        set(format!("http.{base}.proxy"), format!("http://127.0.0.1:{port}"));
        set(format!("http.{base}.sslCAInfo"), bundle.to_string());
        set(format!("credential.https://{host}.helper"), String::new());
        set(format!("credential.https://{host}.helper"), helper());
    }
    out
}

/// The owners of `records`' GitHub URLs that are in `fence` (normalized
/// `owner/repo`s), each spelling once, sorted. Aliased ssh hosts aren't
/// resolved here: those URLs keep their alias, which no rewrite matches.
pub fn owners<'a>(records: impl IntoIterator<Item = &'a FolderRemotes>, fence: &[String]) -> Vec<String> {
    let mut out: Vec<String> = records
        .into_iter()
        .flat_map(|r| r.remotes.iter())
        .flat_map(|r| r.urls.iter().chain(&r.pushurls))
        .filter_map(|url| crate::credentials::fence::github_parts(url, &mut |_| None))
        .filter(|(_, repo)| fence.contains(repo))
        .map(|(owner, _)| owner.to_string())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Whether `key` is one of git's command-scope variables.
pub fn is_config_var(key: &str) -> bool {
    let indexed = |prefix: &str| {
        key.strip_prefix(prefix).is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    };
    key == "GIT_CONFIG_COUNT" || indexed("GIT_CONFIG_KEY_") || indexed("GIT_CONFIG_VALUE_")
}

/// The command-scope env for `ours` behind `user` (env layers in the order
/// they apply, last wins): the user's entries first, renumbered from 0
/// (one without its key or value is dropped: git would refuse the whole
/// env), then `ours`, then the count. An unparsable user count reads as no
/// entries, as git would refuse them all.
pub fn config_env<'a>(
    user: impl IntoIterator<Item = (&'a String, &'a String)>,
    ours: &[(String, String)],
) -> Vec<(String, String)> {
    let mut vars: std::collections::BTreeMap<&str, &str> = Default::default();
    for (k, v) in user {
        if is_config_var(k) {
            vars.insert(k, v);
        }
    }
    let count = vars.get("GIT_CONFIG_COUNT").and_then(|c| c.trim().parse::<usize>().ok()).unwrap_or(0);
    let theirs = (0..count).filter_map(|i| {
        let key = vars.get(format!("GIT_CONFIG_KEY_{i}").as_str())?;
        let value = vars.get(format!("GIT_CONFIG_VALUE_{i}").as_str())?;
        Some((key.to_string(), value.to_string()))
    });
    let all: Vec<(String, String)> = theirs.chain(ours.iter().cloned()).collect();
    let mut out = Vec::with_capacity(all.len() * 2 + 1);
    for (n, (k, v)) in all.iter().enumerate() {
        out.push((format!("GIT_CONFIG_KEY_{n}"), k.clone()));
        out.push((format!("GIT_CONFIG_VALUE_{n}"), v.clone()));
    }
    out.push(("GIT_CONFIG_COUNT".into(), all.len().to_string()));
    out
}

/// [`config_env`] for every exec, over `layers` (the image's, containerEnv,
/// remoteEnv, `--env`), when `exec_keys` (remoteEnv's and `--env`'s, which
/// each exec sets over the container's env) hold some of git's variables;
/// else empty, the container's env being enough.
pub fn exec_config_env<'a>(
    layers: impl IntoIterator<Item = (&'a String, &'a String)>,
    mut exec_keys: impl Iterator<Item = &'a String>,
    ours: &[(String, String)],
) -> Vec<(String, String)> {
    if ours.is_empty() || !exec_keys.any(|k| is_config_var(k)) {
        return Vec::new();
    }
    config_env(layers, ours)
}

/// `entries` as a gitconfig file, one section per entry so the order holds.
pub fn system_config(entries: &[(String, String)]) -> String {
    let mut out = String::from("# Written by devsandbox for this sandbox's git-host claim; rewritten at run.\n");
    for (key, value) in entries {
        let Some((section, rest)) = key.split_once('.') else { continue };
        let Some((sub, var)) = rest.rsplit_once('.') else { continue };
        out.push_str(&format!("[{section} {}]\n\t{var} = {}\n", quote(sub), quote(value)));
    }
    out
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// As root, stdin [`system_config`]'s text: written to [`SYSTEM_FILE`]
/// atomically and included from `/etc/gitconfig` (git honors `include` in
/// every scope), whose rewrite is atomic too. Exit 3: `/etc` isn't writable.
pub const SYSTEM_SCRIPT: &str = r#"set -e
f=/etc/gitconfig.d/devsandbox
[ -w /etc ] || { echo "/etc isn't writable" >&2; exit 3; }
mkdir -p /etc/gitconfig.d
cat > "$f.tmp.$$"
chmod 644 "$f.tmp.$$"
mv "$f.tmp.$$" "$f"
if ! grep -qxF "	path = $f" /etc/gitconfig 2>/dev/null; then
  t=/etc/gitconfig.tmp.$$
  { cat /etc/gitconfig 2>/dev/null || true; printf '[include]\n\tpath = %s\n' "$f"; } > "$t"
  chmod 644 "$t"
  mv "$t" /etc/gitconfig
fi
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::FolderRemote;

    fn pairs(p: &[(&str, &str)]) -> Vec<(String, String)> {
        p.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn entries_rewrite_ssh_and_keep_each_owners_spelling() {
        let got = entries(&["github.com".into()], &["Org".into(), "org".into()], 29004, "/b.crt");
        let helper = helper();
        let want = pairs(&[
            ("url.https://github.com/.insteadOf", "git@github.com:"),
            ("url.https://github.com/.insteadOf", "ssh://git@github.com/"),
            ("url.https://github.com/Org/.insteadOf", "https://github.com/Org/"),
            ("url.https://github.com/Org/.pushInsteadOf", "https://github.com/Org/"),
            ("url.https://github.com/org/.insteadOf", "https://github.com/org/"),
            ("url.https://github.com/org/.pushInsteadOf", "https://github.com/org/"),
            ("http.https://github.com/.proxy", "http://127.0.0.1:29004"),
            ("http.https://github.com/.sslCAInfo", "/b.crt"),
            ("credential.https://github.com.helper", ""),
            ("credential.https://github.com.helper", &helper),
        ]);
        assert_eq!(got, want);
        assert_eq!(helper, "!/run/devsandbox/bin/devsbd credential");
        // Owners are GitHub's: another git host gets none.
        let other = entries(&["git.example".into()], &["Org".into()], 1, "/b");
        assert!(other.iter().all(|(k, _)| !k.contains("Org")), "{other:?}");
        assert_eq!(other.len(), 6);
    }

    #[test]
    fn owners_come_from_fenced_recorded_urls_as_spelled() {
        let rec = |urls: &[&str], push: &[&str]| FolderRemotes {
            remotes: vec![FolderRemote {
                name: "origin".into(),
                urls: urls.iter().map(|u| u.to_string()).collect(),
                pushurls: push.iter().map(|u| u.to_string()).collect(),
            }],
            ..Default::default()
        };
        let a = rec(&["git@github.com:Org/App.git", "https://gitlab.com/x/y"], &["https://github.com/org/app"]);
        let b = rec(&["https://github.com/Forgotten/r", "git@github-work:Work/svc", "https://github.com/up/lib"], &[]);
        let fence = vec!["org/app".to_string(), "up/lib".into(), "work/svc".into()];
        assert_eq!(owners([&a, &b], &fence), ["Org", "org", "up"]);
        assert!(owners([&a], &[]).is_empty());
    }

    #[test]
    fn config_env_merges_a_users_entries_ahead() {
        let ours = pairs(&[("http.https://github.com/.proxy", "http://127.0.0.1:1"), ("credential.https://github.com.helper", "")]);
        assert_eq!(
            config_env(std::iter::empty(), &ours),
            pairs(&[
                ("GIT_CONFIG_KEY_0", "http.https://github.com/.proxy"),
                ("GIT_CONFIG_VALUE_0", "http://127.0.0.1:1"),
                ("GIT_CONFIG_KEY_1", "credential.https://github.com.helper"),
                ("GIT_CONFIG_VALUE_1", ""),
                ("GIT_CONFIG_COUNT", "2"),
            ])
        );
        // Image layer, then containerEnv overriding the count and one entry;
        // entry 1 has no value and is dropped, entry 3 is past the count.
        let layers = pairs(&[
            ("GIT_CONFIG_COUNT", "1"),
            ("GIT_CONFIG_KEY_0", "core.editor"),
            ("GIT_CONFIG_VALUE_0", "vi"),
            ("PATH", "/bin"),
            ("GIT_CONFIG_COUNT", "3"),
            ("GIT_CONFIG_VALUE_0", "nano"),
            ("GIT_CONFIG_KEY_1", "x.y"),
            ("GIT_CONFIG_KEY_2", "user.name"),
            ("GIT_CONFIG_VALUE_2", "Me"),
            ("GIT_CONFIG_KEY_3", "stale.key"),
            ("GIT_CONFIG_VALUE_3", "s"),
            ("GIT_CONFIG_GLOBAL", "/g"),
        ]);
        let got = config_env(layers.iter().map(|(k, v)| (k, v)), &ours);
        assert_eq!(
            got,
            pairs(&[
                ("GIT_CONFIG_KEY_0", "core.editor"),
                ("GIT_CONFIG_VALUE_0", "nano"),
                ("GIT_CONFIG_KEY_1", "user.name"),
                ("GIT_CONFIG_VALUE_1", "Me"),
                ("GIT_CONFIG_KEY_2", "http.https://github.com/.proxy"),
                ("GIT_CONFIG_VALUE_2", "http://127.0.0.1:1"),
                ("GIT_CONFIG_KEY_3", "credential.https://github.com.helper"),
                ("GIT_CONFIG_VALUE_3", ""),
                ("GIT_CONFIG_COUNT", "4"),
            ])
        );
        let bogus = pairs(&[("GIT_CONFIG_COUNT", "x"), ("GIT_CONFIG_KEY_0", "a.b"), ("GIT_CONFIG_VALUE_0", "c")]);
        let got = config_env(bogus.iter().map(|(k, v)| (k, v)), &ours);
        assert_eq!(got.last(), Some(&("GIT_CONFIG_COUNT".to_string(), "2".to_string())));
        assert_eq!(got[1], ("GIT_CONFIG_VALUE_0".to_string(), "http://127.0.0.1:1".to_string()));

        let remote = pairs(&[("GIT_CONFIG_COUNT", "1"), ("GIT_CONFIG_KEY_0", "a.b"), ("GIT_CONFIG_VALUE_0", "c")]);
        let plain = pairs(&[("FOO", "1")]);
        let layers = || remote.iter().map(|(k, v)| (k, v));
        assert!(exec_config_env(layers(), plain.iter().map(|(k, _)| k), &ours).is_empty());
        assert!(exec_config_env(layers(), remote.iter().map(|(k, _)| k), &[]).is_empty());
        let got = exec_config_env(layers(), remote.iter().map(|(k, _)| k), &ours);
        assert_eq!((got[0].1.as_str(), got.last().unwrap().1.as_str()), ("a.b", "3"));

        for (key, yes) in [("GIT_CONFIG_COUNT", true), ("GIT_CONFIG_KEY_12", true), ("GIT_CONFIG_VALUE_0", true),
            ("GIT_CONFIG_KEY_", false), ("GIT_CONFIG_GLOBAL", false), ("GIT_CONFIG_PARAMETERS", false), ("GIT_CONFIG_KEY_x", false)]
        {
            assert_eq!(is_config_var(key), yes, "{key}");
        }
    }

    #[test]
    fn system_config_keeps_the_order_and_quotes() {
        let text = system_config(&pairs(&[
            ("url.https://github.com/.insteadOf", "git@github.com:"),
            ("credential.https://github.com.helper", ""),
            ("credential.https://github.com.helper", "!/run/devsandbox/bin/devsbd credential"),
            ("http.https://x/\"q\\.proxy", "a\"b"),
        ]));
        assert_eq!(
            text.lines().skip(1).collect::<Vec<_>>(),
            [
                "[url \"https://github.com/\"]",
                "\tinsteadOf = \"git@github.com:\"",
                "[credential \"https://github.com\"]",
                "\thelper = \"\"",
                "[credential \"https://github.com\"]",
                "\thelper = \"!/run/devsandbox/bin/devsbd credential\"",
                "[http \"https://x/\\\"q\\\\\"]",
                "\tproxy = \"a\\\"b\"",
            ]
        );
    }

    /// git itself reads the file as the env form means it.
    #[test]
    fn git_reads_the_system_copy_like_the_env() {
        let dir = std::env::temp_dir().join(format!("devsandbox-gitcfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ours = entries(&["github.com".into()], &["Org".into()], 29004, "/b.crt");
        let file = dir.join("config");
        std::fs::write(&file, system_config(&ours)).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(["config", "--file"])
                .arg(&file)
                .args(args)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap()
        };
        assert_eq!(git(&["--get-all", "url.https://github.com/.insteadof"]), "git@github.com:\nssh://git@github.com/\n");
        assert_eq!(git(&["--get-all", "credential.https://github.com.helper"]), "\n!/run/devsandbox/bin/devsbd credential\n");
        assert_eq!(git(&["--get-urlmatch", "http", "https://github.com/Org/r"]), "http.proxy http://127.0.0.1:29004\nhttp.sslcainfo /b.crt\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
