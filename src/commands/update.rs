//! `devsandbox update <name> [args…]`: run a feature's update now
//! (docs/reference/update-cli.md). The command comes from the feature, baked
//! in a sandbox's features image, and runs in a throwaway container from
//! that image with the update's `${sharedVolumes}` dir as its only bind,
//! read-write; sandboxes mount that dir read-only.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::commands::run;
use crate::commands::services;
use crate::config::{Config, MountContext, ResolvedSandbox};
use crate::features::{self, ResolvedFeature};
use crate::runtime::backend;
use crate::state::{Instance, State};
use crate::updates::{self, Update};

/// One sandbox whose features declare the update.
struct Candidate {
    sandbox: ResolvedSandbox,
    feature: ResolvedFeature,
    update: Update,
}

pub fn update(dir: &Path, name: &str, args: &[String]) -> Result<()> {
    updates::check_name(name)?;
    let b = backend();
    // No image dates to pick by, nor the identity plan's flags.
    if b.name() == "container" {
        bail!("`devsandbox update` on Apple's container: {}", crate::NOT_SUPPORTED_YET);
    }
    let config = Config::load(dir)?;
    let project = services::project_id(dir)?;
    let mut candidates = Vec::new();
    for sandbox in config.sandboxes.keys() {
        match candidate(dir, &config, sandbox, name) {
            Ok(Some(c)) => candidates.push(c),
            Ok(None) => {}
            Err(e) => eprintln!("warning: sandbox `{sandbox}`: {e:#}"),
        }
    }
    if candidates.is_empty() {
        bail!("no sandbox of this config has a feature declaring update `{name}`");
    }
    let tags: Vec<String> = candidates.iter().map(|c| run::features_image(&project, &c.sandbox.name)).collect();
    let built: Vec<Option<Option<u64>>> = tags
        .iter()
        .map(|tag| b.image_id(tag).ok().flatten().map(|_| b.image_created(tag).ok().flatten()))
        .collect();
    let Some(i) = newest(&built) else {
        let names: Vec<String> = candidates.iter().map(|c| format!("`{}`", c.sandbox.name)).collect();
        bail!(
            "update `{name}`: no image of sandbox {} is built yet; `devsandbox run <sandbox>` builds it",
            names.join(", ")
        );
    };
    let Candidate { sandbox, feature, update } = &candidates[i];
    let image = &tags[i];
    eprintln!("update `{name}`: from `{image}` (sandbox `{}`)", sandbox.name);

    let bind = host_dir(dir, feature, update)?;
    let props = &sandbox.properties;
    let facts = run::image_facts(image);
    let (plan, seccomp) = run::plan_for(image, run::accounts(props, &facts), props, false, false);
    let (identity_args, warning) =
        run::create_args(&plan, b.name() == "podman", seccomp.as_deref(), &[], std::slice::from_ref(&bind));
    if let Some(warning) = warning {
        eprintln!("warning: {warning}");
    }
    let argv = update_argv(image, identity_args, &update.command, args)?;
    // `run_container`'s guards: the argv is everything the runtime gets.
    // No instance of its own, so every instance's paths are someone else's.
    crate::key_guard::check_argv(&argv)?;
    run::check_bind_argv(dir, &argv, "")?;
    crate::worker_policy::enforce_argv(dir, &argv)?;
    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    let code = b.run_inherit(&refs)?;
    if code != 0 {
        bail!("update `{name}` failed (exit {code})");
    }
    State::update(|state| {
        let record = state.updates.entry(project.clone()).or_default().entry(name.to_string()).or_default();
        record.ok = Some(Instance::now());
        Ok(())
    })
}

/// One update the config root's sandboxes declare.
pub struct Declared {
    /// The first declaring sandbox's (in name order).
    pub update: Update,
    /// Every sandbox whose features declare it, in name order.
    pub sandboxes: Vec<String>,
}

/// Every update the sandboxes of the config root in `dir` declare, by name,
/// without running any. A sandbox that doesn't resolve is in the second
/// list with why.
pub fn declared(dir: &Path, config: &Config) -> (BTreeMap<String, Declared>, Vec<(String, anyhow::Error)>) {
    let mut found: BTreeMap<String, Declared> = BTreeMap::new();
    let mut skipped = Vec::new();
    for sandbox in config.sandboxes.keys() {
        let features = match enabled_features(dir, config, sandbox) {
            Ok((_, features)) => features,
            Err(e) => {
                skipped.push((sandbox.clone(), e));
                continue;
            }
        };
        for update in features.iter().flat_map(|f| &f.metadata.updates) {
            found
                .entry(update.name.clone())
                .or_insert_with(|| Declared { update: update.clone(), sandboxes: Vec::new() })
                .sandboxes
                .push(sandbox.clone());
        }
    }
    (found, skipped)
}

/// Sandbox `sandbox`'s enabled feature declaring update `name`, if any.
fn candidate(dir: &Path, config: &Config, sandbox: &str, name: &str) -> Result<Option<Candidate>> {
    let (resolved, enabled) = enabled_features(dir, config, sandbox)?;
    let found = enabled.into_iter().find_map(|feature| {
        let update = feature.metadata.updates.iter().find(|u| u.name == name)?.clone();
        Some((feature, update))
    });
    Ok(found.map(|(feature, update)| Candidate { sandbox: resolved, feature, update }))
}

/// Sandbox `sandbox` and its enabled features, resolved (fetched into the
/// cache like at `run`). Its features must not declare one update name twice.
fn enabled_features(dir: &Path, config: &Config, sandbox: &str) -> Result<(ResolvedSandbox, Vec<ResolvedFeature>)> {
    let resolved = config.resolve_sandbox(sandbox)?;
    let Some(refs) = &resolved.properties.features else { return Ok((resolved, Vec::new())) };
    let mut enabled = Vec::new();
    for (reference, options) in refs {
        if run::feature_option_values(options)
            .with_context(|| format!("feature `{reference}`: invalid options"))?
            .is_none()
        {
            continue;
        }
        let source = features::FeatureSource::parse(reference)?;
        let feature = features::resolve(&source, dir).with_context(|| format!("fetching feature `{reference}`"))?;
        enabled.push((reference.as_str(), feature));
    }
    let declared: Vec<(&str, &[Update])> = enabled.iter().map(|(r, f)| (*r, f.metadata.updates.as_slice())).collect();
    updates::check_unique(&declared)?;
    let enabled = enabled.into_iter().map(|(_, feature)| feature).collect();
    Ok((resolved, enabled))
}

/// Among the sandboxes' features images (`None`: not built; else its
/// creation time, `None` when unknown), the newest; a tie goes to the first.
fn newest(built: &[Option<Option<u64>>]) -> Option<usize> {
    let mut best: Option<(usize, Option<u64>)> = None;
    for (i, created) in built.iter().enumerate() {
        let Some(created) = created else { continue };
        if best.is_none_or(|(_, b)| *created > b) {
            best = Some((i, *created));
        }
    }
    best.map(|(i, _)| i)
}

/// The `--mount` of the update's dir, read-write: the source of the
/// feature's own read-only mount at `update.mount`, created like a feature
/// mount's (under `shared-volumes/` only, past a worker policy's check).
fn host_dir(dir: &Path, feature: &ResolvedFeature, update: &Update) -> Result<String> {
    let config_dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let config_dir = config_dir.to_string_lossy().into_owned();
    let shared_volumes = Path::new(&config_dir).join("shared-volumes").to_string_lossy().into_owned();
    let ctx = MountContext {
        config_dir: &config_dir,
        workspace_folder: "",
        workspace_folder_basename: "",
        shared_volumes: &shared_volumes,
        instance: "",
        container_env: None,
    };
    let mut mount = None;
    for m in &feature.metadata.mounts {
        let resolved = m.resolve(&ctx)?;
        if resolved.target == update.mount {
            mount = Some(resolved);
            break;
        }
    }
    let mut mount = mount.with_context(|| format!("update `{}`: no mount at `{}`", update.name, update.mount))?;
    if mount.instance_source {
        bail!("update `{}`: its dir names the instance, but every instance shares it", update.name);
    }
    let source = mount.source.clone().unwrap_or_default();
    if !run::creatable_feature_source(&mount.kind, &source, &shared_volumes) {
        bail!("update `{}`: `{source}` isn't a dir under {shared_volumes}", update.name);
    }
    let policy = crate::worker_policy::Policy::for_dir(dir)?;
    run::guard_create(policy.as_ref(), Path::new(&source))?;
    run::provision_bind_source(&source, false)?;
    mount.readonly = false;
    Ok(mount.to_arg())
}

/// The throwaway container: never pulled, default network, the identity
/// plan's args (`identity_args`, the update's bind among them), the command's
/// program as the entrypoint (the image's own would start a sandbox), then
/// its args and the CLI's `extra`.
fn update_argv(image: &str, identity_args: Vec<String>, command: &[String], extra: &[String]) -> Result<Vec<String>> {
    let (program, args) = command.split_first().context("the update's command is empty")?;
    let mut argv: Vec<String> = ["run", "--rm", "--pull=never"].map(String::from).into();
    argv.extend(identity_args);
    argv.extend(["--entrypoint".to_string(), program.clone(), image.to_string()]);
    argv.extend(args.iter().chain(extra).cloned());
    Ok(argv)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn argv_runs_the_command_as_entrypoint() {
        let identity = strings(&["--user", "0:0", "--mount", "type=bind,source=/sv/gh-bin,target=/opt/gh"]);
        let argv = update_argv("img", identity, &strings(&["/usr/lib/gh-update", "/opt/gh"]), &strings(&["2.60.0", "--force"]))
            .unwrap();
        assert_eq!(
            argv,
            strings(&[
                "run", "--rm", "--pull=never", "--user", "0:0", "--mount", "type=bind,source=/sv/gh-bin,target=/opt/gh",
                "--entrypoint", "/usr/lib/gh-update", "img", "/opt/gh", "2.60.0", "--force",
            ])
        );
        assert!(update_argv("img", Vec::new(), &[], &[]).is_err());
    }

    #[test]
    fn newest_built_image_wins() {
        assert_eq!(newest(&[]), None);
        assert_eq!(newest(&[None, None]), None);
        assert_eq!(newest(&[None, Some(Some(5)), Some(Some(9)), Some(Some(7))]), Some(2));
        // A known date beats an unknown one; a tie keeps the first.
        assert_eq!(newest(&[Some(None), Some(Some(1))]), Some(1));
        assert_eq!(newest(&[Some(None), Some(None)]), Some(0));
        assert_eq!(newest(&[Some(Some(3)), Some(Some(3))]), Some(0));
    }

    #[test]
    fn declared_lists_each_update_with_its_sandboxes() {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let cfg = std::env::temp_dir().join(format!("devsandbox-declared-{stamp}"));
        let feature = cfg.join("features/upd");
        std::fs::create_dir_all(&feature).unwrap();
        std::fs::write(
            feature.join("devcontainer-feature.json"),
            r#"{"id": "upd",
                "mounts": [{"type": "bind", "source": "${sharedVolumes}/upd-bin", "target": "/opt/upd", "readonly": true}],
                "customizations": {"devsandbox": {"updates": [
                    {"name": "upd", "mount": "/opt/upd", "command": ["/bin/up"], "every": "1d"}
                ]}}}"#,
        )
        .unwrap();
        let config = Config::parse(
            "[sandbox.a]\nimage = \"x\"\nfeatures = { \"./features/upd\" = true }\n\
             [sandbox.b]\nimage = \"x\"\nfeatures = { \"./features/upd\" = true }\n\
             [sandbox.c]\nimage = \"x\"\n\
             [sandbox.d]\nimage = \"x\"\nfeatures = { \"./features/missing\" = true }\n",
        )
        .unwrap();
        let (found, skipped) = declared(&cfg, &config);
        let _ = std::fs::remove_dir_all(&cfg);
        assert_eq!(found.keys().collect::<Vec<_>>(), ["upd"]);
        assert_eq!(found["upd"].sandboxes, ["a", "b"]);
        assert_eq!(found["upd"].update.every, std::time::Duration::from_secs(86400));
        assert_eq!(skipped.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>(), ["d"]);
    }

    const CHILD_ROOT: &str = "DEVSANDBOX_UPDATE_TEST_ROOT";

    /// The update runs from a sandbox's features image (alpine retagged as
    /// one: no build), writes its dir under `shared-volumes/`, gets the
    /// CLI's args, and its time lands in `State`. In a child with its own
    /// `XDG_DATA_HOME`, so the state written is the test's.
    #[cfg(unix)]
    #[test_utils::docker_test]
    fn update_writes_its_dir_with_docker() -> Result<(), String> {
        let image = "alpine:3.20";
        let rt = backend();
        if rt.image_id(image).ok().flatten().is_none() {
            rt.output_quiet(&["pull", "-q", image]).map_err(|_| "cannot pull alpine:3.20")?;
        }
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("devsandbox-update-{stamp}"));
        crate::test_support::share_runtime_dirs(&root.join("data"), &root.join("runtime"));
        let module = module_path!().split_once("::").map_or(module_path!(), |(_, rest)| rest);
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &format!("{module}::update_child"), "--nocapture", "--test-threads=1"])
            .env(CHILD_ROOT, &root)
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&root);
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success(), "child failed:\n{text}");
        assert!(text.contains("1 passed"), "child did not run:\n{text}");
        Ok(())
    }

    #[test]
    fn update_child() {
        let Some(root) = std::env::var_os(CHILD_ROOT).map(std::path::PathBuf::from) else { return };
        let cfg = root.join("cfg");
        let feature = cfg.join("features/upd");
        std::fs::create_dir_all(&feature).unwrap();
        std::fs::write(
            feature.join("devcontainer-feature.json"),
            r#"{"id": "upd",
                "mounts": [{"type": "bind", "source": "${sharedVolumes}/upd-bin", "target": "/opt/upd", "readonly": true}],
                "customizations": {"devsandbox": {"updates": [
                    {"name": "upd", "mount": "/opt/upd", "command": ["/bin/sh", "-c", "echo \"$@\" > /opt/upd/out", "upd"]}
                ]}}}"#,
        )
        .unwrap();
        std::fs::write(
            cfg.join(crate::config::CONFIG_FILE),
            "[sandbox.a]\nimage = \"alpine:3.20\"\nfeatures = { \"./features/upd\" = true }\n\
             [sandbox.b]\nimage = \"alpine:3.20\"\nfeatures = { \"./features/upd\" = true }\n\
             [sandbox.c]\nimage = \"alpine:3.20\"\n",
        )
        .unwrap();

        let e = format!("{:#}", update(&cfg, "upd", &[]).unwrap_err());
        assert!(e.contains("no image of sandbox `a`, `b` is built yet"), "{e}");
        let e = format!("{:#}", update(&cfg, "nope", &[]).unwrap_err());
        assert!(e.contains("no sandbox of this config has a feature declaring update `nope`"), "{e}");

        let project = services::project_id(&cfg).unwrap();
        let tag = run::features_image(&project, "b");
        let tagged = crate::test_support::runtime_cmd().args(["tag", "alpine:3.20", &tag]).output().unwrap();
        assert!(tagged.status.success(), "tag: {}", String::from_utf8_lossy(&tagged.stderr));
        let cleanup = || {
            let _ = crate::test_support::runtime_cmd().args(["rmi", &tag]).output();
        };
        crate::test_support::with_cleanup(cleanup, || {
            update(&cfg, "upd", &["1.2.3".into(), "--pin".into()]).unwrap();
            let out = std::fs::read_to_string(cfg.join("shared-volumes/upd-bin/out")).unwrap();
            assert_eq!(out.trim(), "1.2.3 --pin");
            let at = State::load().unwrap().updates[&project]["upd"].ok.unwrap();
            assert!(at.abs_diff(Instance::now()) < 600, "{at}");

            // A failing command records nothing new.
            std::fs::remove_dir_all(cfg.join("shared-volumes/upd-bin")).unwrap();
            std::fs::write(cfg.join("shared-volumes/upd-bin"), "").unwrap();
            assert!(update(&cfg, "upd", &[]).is_err());
            assert_eq!(State::load().unwrap().updates[&project]["upd"].ok, Some(at));
        });
    }
}
