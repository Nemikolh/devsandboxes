//! Everything `run` mounts into a container: config `mounts`, extra folders,
//! the workspace file, caches, shell history/rc, and feature-declared mounts.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::config::{
    parse_shorthand, substitute, FolderWorktree, LifecycleCommand, MountContext, ResolvedMount,
    ResolvedSandbox, SandboxProperties, SimpleCommand,
};
use crate::runtime::backend;

/// Resolve the sandbox's `mounts` into `docker run --mount` values, substituting
/// `${…}` variables and creating any missing bind sources so a first run doesn't
/// fail on a non-existent host path. Entries sharing a target collapse to the
/// last one (see [`last_wins_by_target`]) before any source is created, so an
/// overridden template mount leaves no stub behind.
pub(super) fn resolve_mounts(
    dir: &Path,
    folder: &Path,
    basename: &str,
    instance_id: &str,
    sandbox: &ResolvedSandbox,
) -> Result<ResolvedMounts> {
    let Some(mounts) = &sandbox.properties.mounts else {
        return Ok(ResolvedMounts::default());
    };
    // `${configDir}` anchors host-backed volumes; make it absolute.
    let config_dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let shared_volumes = config_dir.join("shared-volumes");
    let ctx = MountContext {
        config_dir: &config_dir.to_string_lossy(),
        workspace_folder: &folder.to_string_lossy(),
        workspace_folder_basename: basename,
        shared_volumes: &shared_volumes.to_string_lossy(),
        instance: instance_id,
    };
    let resolved = mounts
        .iter()
        .map(|mount| mount.resolve(&ctx))
        .collect::<Result<Vec<_>>>()
        .with_context(|| format!("sandbox `{}`: invalid mount", sandbox.name))?;
    let resolved = last_wins_by_target(resolved);
    let mut out = ResolvedMounts::default();
    for resolved in resolved {
        if resolved.kind == "bind" {
            if let Some(source) = &resolved.source {
                ensure_bind_source(Path::new(source))?;
            }
        }
        if resolved.per_instance {
            out.volumes.extend(resolved.source.clone());
        }
        out.args.push(resolved.to_arg());
    }
    Ok(out)
}

/// The sandbox's own `mounts` as `--mount` args, plus the per-instance volume
/// names among them (for `rm`).
#[derive(Default)]
pub(super) struct ResolvedMounts {
    pub(super) args: Vec<String>,
    pub(super) volumes: Vec<String>,
}

/// Drop every mount whose target a later entry claims again, keeping the order
/// of the survivors. `extends` concatenates `mounts` base-first, so "later" is
/// the more downstream table: a sandbox (or a template further down the chain)
/// replaces an inherited mount by re-mounting its target, instead of the
/// runtime rejecting the pair as a duplicate mount point.
fn last_wins_by_target(mounts: Vec<ResolvedMount>) -> Vec<ResolvedMount> {
    let mut seen = BTreeSet::new();
    let mut kept: Vec<ResolvedMount> = mounts
        .into_iter()
        .rev()
        .filter(|m| seen.insert(m.target.clone()))
        .collect();
    kept.reverse();
    kept
}

/// Order `--mount` args parent-first (by target depth, then path), so a mount
/// nested inside another is applied after it and stays visible. docker sorts
/// this way itself (moby `container.SortMounts`), but that is undocumented and
/// not something to rely on for other runtimes (Apple `container`). Stable, so
/// equal targets keep their order; unparsable args sort as depth 0.
pub(super) fn sort_parents_first(mounts: &mut [String]) {
    let key = |arg: &String| {
        let target = parse_shorthand(arg).map(|(_, _, t, _)| t).unwrap_or_default();
        let depth = target.split('/').filter(|c| !c.is_empty()).count();
        (depth, target)
    };
    mounts.sort_by_cached_key(key);
}

/// One resolved `folders` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ResolvedFolder {
    /// Container path.
    pub target: String,
    /// Canonical host folder.
    pub host: PathBuf,
    /// The entry's `worktree` mode.
    pub mode: FolderWorktree,
}

/// Resolve `folders` (container path -> host folder) into [`ResolvedFolder`]s.
/// Host paths behave like `folder`: relative to the config dir, must exist.
/// Container paths must be absolute and distinct from `workspaceFolder`, which
/// is always the first root.
pub(super) fn resolve_folders(
    dir: &Path,
    ctx: &MountContext,
    workspace: &str,
    props: &SandboxProperties,
) -> Result<Vec<ResolvedFolder>> {
    let Some(folders) = &props.folders else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(folders.len());
    for (target, entry) in folders {
        let target = substitute(target, ctx);
        if !target.starts_with('/') {
            bail!("`folders` key `{target}` must be an absolute container path");
        }
        if target == workspace {
            bail!(
                "`folders` key `{target}` duplicates `workspaceFolder`; \
                 the primary folder is added automatically"
            );
        }
        let source = substitute(entry.path(), ctx);
        let host = dir
            .join(&source)
            .canonicalize()
            .with_context(|| format!("`folders` entry `{source}` does not exist"))?;
        out.push(ResolvedFolder { target, host, mode: entry.worktree_mode() });
    }
    Ok(out)
}

/// JSON body of the generated `.code-workspace`: the primary workspace folder
/// first, then the extra `folders` roots. `recommendations`, when non-empty, is
/// written as `extensions.recommendations` so VS Code offers to install them —
/// the fallback for backends where auto-install via `nameConfigs` is unavailable
/// (see [`write_workspace_file`]).
///
/// The primary folder gets an explicit name (see [`primary_folder_name`]) and
/// `terminal.integrated.cwd` is pinned to it via `${workspaceFolder:<name>}`:
/// in a multi-root workspace VS Code otherwise picks the terminal cwd from
/// whichever root holds the active editor, which lands new terminals in the
/// extra roots.
fn workspace_file_json(
    instance: &str,
    workspace: &str,
    extra_folders: &[ResolvedFolder],
    recommendations: &[String],
) -> String {
    let primary = primary_folder_name(instance, workspace, extra_folders);
    let folders: Vec<serde_json::Value> =
        std::iter::once(serde_json::json!({ "name": primary, "path": workspace }))
            .chain(
                extra_folders
                    .iter()
                    .map(|folder| serde_json::json!({ "path": folder.target })),
            )
            .collect();
    let mut root = serde_json::json!({
        "folders": folders,
        "settings": {
            "terminal.integrated.cwd": format!("${{workspaceFolder:{primary}}}"),
        },
    });
    if !recommendations.is_empty() {
        root["extensions"] = serde_json::json!({ "recommendations": recommendations });
    }
    serde_json::to_string_pretty(&root).expect("workspace json serializes")
}

/// Explorer name of the primary root: its path's basename, so it reads like
/// the extra roots (VS Code shows those by basename) instead of the instance
/// name. Falls back to the instance when the basename is empty or shared with
/// an extra root, where `${workspaceFolder:<name>}` would be ambiguous.
fn primary_folder_name<'a>(
    instance: &'a str,
    workspace: &'a str,
    extra_folders: &[ResolvedFolder],
) -> &'a str {
    fn basename(path: &str) -> Option<&str> {
        path.rsplit('/').find(|s| !s.is_empty())
    }
    match basename(workspace) {
        Some(name) if !extra_folders.iter().any(|f| basename(&f.target) == Some(name)) => name,
        _ => instance,
    }
}

/// Write `/workspaces/<instance>.code-workspace` inside the container so VS
/// Code opens the instance as a workspace named after it (the window title is
/// the file name; there is no separate name property). Best-effort: a container
/// without `sh` still runs, and `code` falls back to a folder open when no file
/// was recorded. Returns the container path on success.
pub(super) fn write_workspace_file(
    container: &str,
    instance: &str,
    workspace: &str,
    extra_folders: &[ResolvedFolder],
    extensions: &[String],
) -> Option<String> {
    let path = format!("/workspaces/{instance}.code-workspace");
    // The Remote-Containers `nameConfigs` file (see `write_vscode_name_config`)
    // installs extensions automatically only for Docker containers; on Apple
    // `container` it is ignored, so fall back to workspace recommendations.
    let recommendations: &[String] = if backend().name() == "container" {
        extensions
    } else {
        &[]
    };
    let json = workspace_file_json(instance, workspace, extra_folders, recommendations);
    // Runs as the container's default user, often root: shell and tools by
    // fixed path, not the container's env (`runtime::fixed_path`).
    let script = crate::runtime::fixed_path(r#"mkdir -p "${2%/*}" && printf '%s\n' "$1" > "$2""#);
    let sh = crate::runtime::SH;
    match backend().run_checked(&["exec", container, sh, "-c", &script, "sh", &json, &path]) {
        Ok(()) => Some(path),
        Err(e) => {
            eprintln!("warning: cannot write {path} in `{container}`: {e:#}");
            None
        }
    }
}

/// Expand `caches` into (extra `--mount` args, extra `(key, value)` env vars).
/// Each cache is a shared bind mount under `${configDir}/shared-volumes/<name>`
/// (auto-created) plus the env vars that point the tool at its mount target.
pub(super) fn resolve_caches(
    config_dir: &Path,
    props: &SandboxProperties,
) -> Result<(Vec<String>, Vec<(String, String)>)> {
    let mut mounts = Vec::new();
    let mut env = Vec::new();
    for name in props.caches.iter().flatten() {
        let (source_sub, target, vars) = crate::config::known_cache(name).with_context(|| {
            format!(
                "unknown cache `{name}`; supported: {}",
                crate::config::SUPPORTED_CACHES.join(", ")
            )
        })?;
        let source = config_dir.join("shared-volumes").join(source_sub);
        std::fs::create_dir_all(&source)
            .with_context(|| format!("cannot create {}", source.display()))?;
        mounts.push(format!("type=bind,source={},target={target}", source.display()));
        for var in vars {
            env.push((var.to_string(), target.to_string()));
        }
    }
    Ok((mounts, env))
}

/// Container mount point of the managed shell-history directory.
const HISTORY_TARGET: &str = "/commandhistory";

/// Provision this instance's managed shell history under
/// `${configDir}/shared-volumes/history/<instance_id>/.zsh_history` and return
/// (host file path, `--mount` arg, `HISTFILE` env pair). The *directory* is
/// bind-mounted (Apple's `container` cannot bind a single file) and zsh is
/// pointed at the file inside it via `HISTFILE`. An existing file is reused
/// untouched, so history survives rm + run rebuilds; keying by the persistent
/// id keeps it attached across renames too.
pub(super) fn provision_shell_history(
    dir: &Path,
    instance_id: &str,
) -> Result<(PathBuf, String, (String, String))> {
    let config_dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let history_dir = config_dir.join("shared-volumes").join("history").join(instance_id);
    std::fs::create_dir_all(&history_dir)
        .with_context(|| format!("cannot create {}", history_dir.display()))?;
    let path = history_dir.join(".zsh_history");
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("cannot create {}", path.display()))?;
    let mount = format!("type=bind,source={},target={HISTORY_TARGET}", history_dir.display());
    let env = ("HISTFILE".to_string(), format!("{HISTORY_TARGET}/.zsh_history"));
    Ok((path, mount, env))
}

const SHELL_RC_TARGET: &str = "/devsandbox/rc";

/// Resolve `shell-rc` entries into `(--mount arg, container file path)` pairs.
/// Entry `i`'s *parent directory* is bind-mounted read-only at
/// `/devsandbox/rc/<i>` (Apple's `container` cannot bind a single file) and the
/// file is addressed by basename inside it. A missing host file is an error:
/// unlike `mounts`, auto-creating an empty snippet would only hide a typo.
pub(super) fn resolve_shell_rc(
    dir: &Path,
    ctx: &MountContext,
    props: &SandboxProperties,
) -> Result<Vec<(String, String)>> {
    let Some(entries) = &props.shell_rc else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(entries.len());
    for (i, entry) in entries.iter().enumerate() {
        let source = substitute(entry, ctx);
        let host = dir
            .join(&source)
            .canonicalize()
            .with_context(|| format!("`shell-rc` entry `{source}` does not exist"))?;
        if !host.is_file() {
            bail!("`shell-rc` entry `{source}` is not a file");
        }
        let parent = host.parent().context("shell-rc entry has no parent dir")?;
        let name = host
            .file_name()
            .context("shell-rc entry has no file name")?
            .to_string_lossy();
        let target = format!("{SHELL_RC_TARGET}/{i}");
        out.push((
            format!("type=bind,source={},target={target},readonly", parent.display()),
            format!("{target}/{name}"),
        ));
    }
    Ok(out)
}

/// Shell snippet that makes the remote user's interactive rc files source each
/// `shell-rc` path and, with `persist_history`, pin `HISTFILE` to the managed
/// location. The rc-level export exists because the container-env `HISTFILE`
/// alone is not enough: VS Code's injected shell integration resets `HISTFILE`
/// to `$HOME/.zsh_history` before sourcing `~/.zshrc`, so only a line inside
/// the rc file survives every shell startup path. Runs as `remoteUser`, so
/// `$HOME` is theirs rather than a hard-coded `/root`. Idempotent: each line is
/// appended only when the rc file doesn't mention it yet, and a missing rc file
/// is created. The source line is guarded with `[ -r … ]` so a shell still
/// starts if the mount is gone.
pub(super) fn shell_rc_wiring(paths: &[&str], persist_history: bool) -> LifecycleCommand {
    let mut script = String::from("set -eu\nfor rc in \"$HOME/.zshrc\" \"$HOME/.bashrc\"; do\n");
    if persist_history {
        let histfile = format!("{HISTORY_TARGET}/.zsh_history");
        script.push_str(&format!(
            "  grep -qsF 'HISTFILE={histfile}' \"$rc\" || printf '\\nexport HISTFILE={histfile}\\n' >> \"$rc\"\n"
        ));
    }
    for path in paths {
        script.push_str(&format!(
            "  grep -qsF '{path}' \"$rc\" || printf '\\n[ -r {path} ] && . {path}\\n' >> \"$rc\"\n"
        ));
    }
    script.push_str("done\n");
    LifecycleCommand::Simple(SimpleCommand::Shell(script))
}

/// The first `${…}` expression left in an already-substituted value.
fn unresolved_variable(value: &str) -> Option<&str> {
    let start = value.find("${")?;
    let end = value[start..].find('}').map_or(value.len(), |e| start + e + 1);
    Some(&value[start..end])
}

/// `previous` followed by the new names not already in it.
pub(super) fn merge_volumes(previous: &[String], current: Vec<String>) -> Vec<String> {
    let mut out = previous.to_vec();
    for volume in current {
        if !out.contains(&volume) {
            out.push(volume);
        }
    }
    out
}

/// Outcome for one feature-declared mount.
#[derive(Debug, PartialEq)]
pub(super) enum MountDecision {
    Apply,
    /// Target already claimed by the sandbox (config wins, silently).
    Overridden,
    /// Not applicable here; skipped with a printed warning.
    Skip(String),
}

/// Pure applicability check for a feature mount: `taken` holds the targets
/// already mounted, `file_binds` whether the backend can bind single files, and
/// `source_probe` reports a bind source as `Some(is_file_like)` (`!is_dir`, so
/// sockets count as files) or `None` when it does not exist on the host.
pub(super) fn feature_mount_decision(
    mount: &ResolvedMount,
    taken: &BTreeSet<String>,
    file_binds: bool,
    source_probe: impl Fn(&str) -> Option<bool>,
) -> MountDecision {
    if taken.contains(&mount.target) {
        return MountDecision::Overridden;
    }
    // `substitute` leaves unknown `${…}` verbatim; passing one on only earns an
    // opaque runtime error (e.g. an invalid volume name).
    for value in mount.source.iter().chain([&mount.target]) {
        if let Some(var) = unresolved_variable(value) {
            return MountDecision::Skip(format!(
                "mount to `{}` uses unsupported variable `{var}`; skipping",
                mount.target
            ));
        }
    }
    if mount.kind != "bind" {
        return MountDecision::Apply;
    }
    // `Mount::resolve` already rejects sourceless bind mounts.
    let Some(source) = &mount.source else {
        return MountDecision::Skip(format!("bind mount to `{}` has no source", mount.target));
    };
    match source_probe(source) {
        None => MountDecision::Skip(format!(
            "mount source `{source}` does not exist on the host; skipping"
        )),
        Some(true) if !file_binds => MountDecision::Skip(format!(
            "cannot bind file `{source}` on this runtime; skipping"
        )),
        _ => MountDecision::Apply,
    }
}

/// Create a missing bind-mount source. A final path component containing a dot
/// (other than a leading one) is treated as a file (touched); anything else as a
/// directory — so `credentials.json` becomes a file and `pnpm-store` a dir.
fn ensure_bind_source(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    let is_file = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.trim_start_matches('.').contains('.'))
        .unwrap_or(false);
    if is_file {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("cannot create {}", path.display()))?;
    } else {
        std::fs::create_dir_all(path).with_context(|| format!("cannot create {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FolderEntry, FolderTable};

    fn ctx() -> MountContext<'static> {
        MountContext {
            config_dir: "/cfg",
            workspace_folder: "/host/app",
            workspace_folder_basename: "app",
            shared_volumes: "/cfg/shared-volumes",
            instance: "app",
        }
    }

    fn props_with_folders(entries: &[(&str, &str)]) -> SandboxProperties {
        SandboxProperties {
            folders: Some(
                entries
                    .iter()
                    .map(|(k, v)| (k.to_string(), FolderEntry::Path(v.to_string())))
                    .collect(),
            ),
            ..Default::default()
        }
    }

    #[test]
    fn folders_resolve_relative_to_config_dir() {
        let dir = std::env::temp_dir();
        let props = props_with_folders(&[("/workspaces/.shared", ".")]);
        let resolved = resolve_folders(&dir, &ctx(), "/workspaces/app", &props).unwrap();
        assert_eq!(
            resolved,
            vec![ResolvedFolder {
                target: "/workspaces/.shared".into(),
                host: dir.canonicalize().unwrap(),
                mode: FolderWorktree::Auto,
            }]
        );
    }

    #[test]
    fn folders_table_form_substitutes_path_and_keeps_mode() {
        let dir = std::env::temp_dir();
        let props = SandboxProperties {
            folders: Some(
                [(
                    "/workspaces/${instance}-lib".to_string(),
                    FolderEntry::Table(FolderTable {
                        path: "${configDir}/.".into(),
                        worktree: Some(FolderWorktree::Never),
                    }),
                )]
                .into(),
            ),
            ..Default::default()
        };
        let ctx = MountContext { config_dir: dir.to_str().unwrap(), ..ctx() };
        let resolved = resolve_folders(&dir, &ctx, "/workspaces/app", &props).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].target, "/workspaces/app-lib");
        assert_eq!(resolved[0].host, dir.canonicalize().unwrap());
        assert_eq!(resolved[0].mode, FolderWorktree::Never);
    }

    // --- shell-rc ---

    #[test]
    fn shell_rc_mounts_parent_dir_readonly_and_addresses_file() {
        let dir = std::env::temp_dir().join(format!("devsandbox-rc-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("shell")).unwrap();
        std::fs::write(dir.join("shell/aliases.sh"), "alias ll='ls -l'\n").unwrap();
        let props = SandboxProperties {
            shell_rc: Some(vec!["./shell/aliases.sh".into()]),
            ..Default::default()
        };
        let resolved = resolve_shell_rc(&dir, &ctx(), &props).unwrap();
        let parent = dir.join("shell").canonicalize().unwrap();
        assert_eq!(
            resolved,
            vec![(
                format!("type=bind,source={},target=/devsandbox/rc/0,readonly", parent.display()),
                "/devsandbox/rc/0/aliases.sh".to_string(),
            )]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn shell_rc_missing_file_is_an_error() {
        let props = SandboxProperties {
            shell_rc: Some(vec!["./nope.sh".into()]),
            ..Default::default()
        };
        let err = resolve_shell_rc(&std::env::temp_dir(), &ctx(), &props).unwrap_err();
        assert!(err.to_string().contains("`shell-rc` entry `./nope.sh` does not exist"));
    }

    #[test]
    fn shell_rc_directory_is_rejected() {
        let props = SandboxProperties {
            shell_rc: Some(vec![".".into()]),
            ..Default::default()
        };
        let err = resolve_shell_rc(&std::env::temp_dir(), &ctx(), &props).unwrap_err();
        assert!(err.to_string().contains("is not a file"));
    }

    #[test]
    fn shell_rc_wiring_is_guarded_and_idempotent_per_path() {
        let cmd = shell_rc_wiring(&["/devsandbox/rc/0/a.sh", "/devsandbox/rc/1/b.sh"], false);
        let argv = &cmd.commands()[0];
        assert_eq!(&argv[..2], &["sh".to_string(), "-c".to_string()]);
        let script = &argv[2];
        assert!(script.contains(r#"for rc in "$HOME/.zshrc" "$HOME/.bashrc""#));
        assert!(script.contains(
            r#"grep -qsF '/devsandbox/rc/0/a.sh' "$rc" || printf '\n[ -r /devsandbox/rc/0/a.sh ] && . /devsandbox/rc/0/a.sh\n' >> "$rc""#
        ));
        assert!(script.contains("grep -qsF '/devsandbox/rc/1/b.sh'"));
        assert!(!script.contains("HISTFILE"));
    }

    fn feature_mount(kind: &str, source: Option<&str>, target: &str) -> ResolvedMount {
        ResolvedMount {
            kind: kind.into(),
            source: source.map(Into::into),
            target: target.into(),
            readonly: false,
            per_instance: false,
        }
    }

    #[test]
    fn feature_mount_decision_skips_unresolved_variables() {
        // An unsupported variable would otherwise reach the runtime verbatim.
        let m = feature_mount("volume", Some("dind-${someFutureVar}"), "/var/lib/docker");
        match feature_mount_decision(&m, &BTreeSet::new(), true, |_| None) {
            MountDecision::Skip(reason) => {
                assert!(reason.contains("unsupported variable `${someFutureVar}`"), "{reason}")
            }
            other => panic!("expected Skip, got {other:?}"),
        }
        let m = feature_mount("volume", Some("v"), "/x/${nope");
        assert!(matches!(
            feature_mount_decision(&m, &BTreeSet::new(), true, |_| None),
            MountDecision::Skip(_)
        ));
        // Overridden by the sandbox wins before the check: silent, as before.
        let taken = BTreeSet::from(["/var/lib/docker".to_string()]);
        let m = feature_mount("volume", Some("dind-${someFutureVar}"), "/var/lib/docker");
        assert_eq!(
            feature_mount_decision(&m, &taken, true, |_| None),
            MountDecision::Overridden
        );
    }

    #[test]
    fn merge_volumes_keeps_previous_and_dedupes() {
        let previous = vec!["dind-web".to_string(), "old-web".to_string()];
        assert_eq!(
            merge_volumes(&previous, vec!["dind-web".into(), "new-web".into()]),
            ["dind-web", "old-web", "new-web"]
        );
        assert!(merge_volumes(&[], vec![]).is_empty());
    }

    #[test]
    fn last_wins_by_target_keeps_downstream_mount() {
        let mounts = vec![
            feature_mount("bind", Some("/tpl/gh"), "/root/.config/gh"),
            feature_mount("bind", Some("/tpl/bin"), "/usr/local/bin/tool"),
            feature_mount("bind", Some("/sandbox/gh"), "/root/.config/gh"),
        ];
        let kept = last_wins_by_target(mounts);
        assert_eq!(
            kept,
            vec![
                feature_mount("bind", Some("/tpl/bin"), "/usr/local/bin/tool"),
                feature_mount("bind", Some("/sandbox/gh"), "/root/.config/gh"),
            ]
        );
    }

    #[test]
    fn sort_parents_first_orders_by_depth_then_path() {
        let mut mounts = vec![
            "type=bind,source=/h/creds.json,target=/root/.zidane/credentials.json".to_string(),
            "type=bind,source=/s/zidane,target=/root/.zidane".to_string(),
            "type=bind,source=/s/b,target=/b".to_string(),
            "type=bind,source=/s/a,target=/a/x".to_string(),
        ];
        sort_parents_first(&mut mounts);
        let targets: Vec<_> = mounts
            .iter()
            .map(|m| parse_shorthand(m).unwrap().2)
            .collect();
        assert_eq!(
            targets,
            ["/b", "/a/x", "/root/.zidane", "/root/.zidane/credentials.json"]
        );
    }

    #[test]
    fn last_wins_by_target_keeps_nested_targets() {
        // Layering inside an inherited mount is not an override.
        let mounts = vec![
            feature_mount("bind", Some("/s/zidane"), "/root/.zidane"),
            feature_mount("bind", Some("/h/creds.json"), "/root/.zidane/credentials.json"),
        ];
        assert_eq!(last_wins_by_target(mounts.clone()), mounts);
    }

    #[test]
    fn feature_mount_decision_config_target_wins_silently() {
        let taken = BTreeSet::from(["/var/run/docker-host.sock".to_string()]);
        let m = feature_mount("bind", Some("/var/run/docker.sock"), "/var/run/docker-host.sock");
        assert_eq!(
            feature_mount_decision(&m, &taken, true, |_| Some(true)),
            MountDecision::Overridden
        );
    }

    #[test]
    fn feature_mount_decision_missing_source_warns() {
        let m = feature_mount("bind", Some("/var/run/docker.sock"), "/var/run/docker-host.sock");
        match feature_mount_decision(&m, &BTreeSet::new(), true, |_| None) {
            MountDecision::Skip(reason) => assert!(reason.contains("does not exist"), "{reason}"),
            other => panic!("expected Skip, got {other:?}"),
        }
    }

    #[test]
    fn feature_mount_decision_file_bind_unsupported_warns() {
        // Apple `container` cannot bind single files (sockets included).
        let m = feature_mount("bind", Some("/var/run/docker.sock"), "/var/run/docker-host.sock");
        match feature_mount_decision(&m, &BTreeSet::new(), false, |_| Some(true)) {
            MountDecision::Skip(reason) => assert!(reason.contains("cannot bind file"), "{reason}"),
            other => panic!("expected Skip, got {other:?}"),
        }
    }

    #[test]
    fn feature_mount_decision_applies_dirs_and_volumes() {
        let dir = feature_mount("bind", Some("/opt/data"), "/data");
        assert_eq!(
            feature_mount_decision(&dir, &BTreeSet::new(), false, |_| Some(false)),
            MountDecision::Apply
        );
        // Non-bind mounts have no host source to probe.
        let vol = feature_mount("volume", Some("dind-var-lib-docker"), "/var/lib/docker");
        assert_eq!(
            feature_mount_decision(&vol, &BTreeSet::new(), false, |_| None),
            MountDecision::Apply
        );
    }

    #[test]
    fn shell_rc_wiring_pins_histfile_when_history_persisted() {
        let cmd = shell_rc_wiring(&[], true);
        let script = &cmd.commands()[0][2];
        assert!(script.contains(
            r#"grep -qsF 'HISTFILE=/commandhistory/.zsh_history' "$rc" || printf '\nexport HISTFILE=/commandhistory/.zsh_history\n' >> "$rc""#
        ), "{script}");
    }

    #[test]
    fn folders_reject_relative_container_path() {
        let props = props_with_folders(&[("workspaces/x", ".")]);
        let err = resolve_folders(Path::new("/tmp"), &ctx(), "/workspaces/app", &props)
            .unwrap_err()
            .to_string();
        assert!(err.contains("absolute container path"), "{err}");
    }

    #[test]
    fn folders_reject_workspace_folder_duplicate() {
        let props = props_with_folders(&[("/workspaces/app", ".")]);
        let err = resolve_folders(Path::new("/tmp"), &ctx(), "/workspaces/app", &props)
            .unwrap_err()
            .to_string();
        assert!(err.contains("duplicates `workspaceFolder`"), "{err}");
    }

    #[test]
    fn folders_reject_missing_host_folder() {
        let props = props_with_folders(&[("/workspaces/x", "does-not-exist-9f3a")]);
        let err = resolve_folders(Path::new("/tmp"), &ctx(), "/workspaces/app", &props)
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not exist"), "{err}");
    }

    #[test]
    fn workspace_json_lists_primary_first() {
        let extras = vec![ResolvedFolder {
            target: "/workspaces/.shared".into(),
            host: PathBuf::from("/x"),
            mode: FolderWorktree::Auto,
        }];
        let json = workspace_file_json("app-2", "/workspaces/app", &extras, &[]);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed["folders"],
            serde_json::json!([
                { "name": "app", "path": "/workspaces/app" },
                { "path": "/workspaces/.shared" }
            ])
        );
        assert_eq!(
            parsed["settings"]["terminal.integrated.cwd"],
            "${workspaceFolder:app}"
        );
        // No recommendations passed → no `extensions` key at all.
        assert!(parsed.get("extensions").is_none());
    }

    #[test]
    fn primary_folder_named_after_path_basename() {
        assert_eq!(primary_folder_name("app-2", "/workspaces/app", &[]), "app");
        assert_eq!(primary_folder_name("app-2", "/workspaces/app/", &[]), "app");
    }

    #[test]
    fn primary_folder_falls_back_to_instance() {
        assert_eq!(primary_folder_name("app-2", "/", &[]), "app-2");
        let clash = vec![ResolvedFolder {
            target: "/src/app".to_string(),
            host: PathBuf::from("/x"),
            mode: FolderWorktree::Auto,
        }];
        assert_eq!(primary_folder_name("app-2", "/workspaces/app", &clash), "app-2");
    }

    #[test]
    fn workspace_json_includes_extension_recommendations() {
        let recs = vec!["dbaeumer.vscode-eslint".to_string()];
        let json = workspace_file_json("app", "/workspaces/app", &[], &recs);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed["extensions"]["recommendations"],
            serde_json::json!(["dbaeumer.vscode-eslint"])
        );
    }
}
