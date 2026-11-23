//! `devsandbox update <name> [args…]`: run a feature's update now
//! (docs/reference/update-cli.md). The command comes from the features image
//! it was baked in (its `devsandbox.updates` label), and runs in a throwaway
//! container from that image with the update's `${sharedVolumes}` dir as
//! its only bind, read-write; sandboxes mount that dir read-only.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::commands::run;
use crate::commands::services;
use crate::config::{Config, Mount, MountContext, MountObject};
use crate::features;
use crate::runtime::{backend, Backend};
use crate::state::{Instance, State};
use crate::updates::{self, Baked, Update};

pub fn update(dir: &Path, name: &str, args: &[String]) -> Result<()> {
    updates::check_name(name)?;
    let b = backend();
    // No image dates or labels to pick by, nor the identity plan's flags.
    if b.name() == "container" {
        bail!("`devsandbox update` on Apple's container: {}", crate::NOT_SUPPORTED_YET);
    }
    let config = Config::load(dir)?;
    let project = services::project_id(dir)?;
    let images = images(config.sandboxes.keys(), |sandbox| facts(b, &run::features_image(&project, sandbox)));
    for image in &images {
        if let Err(e) = &image.baked {
            eprintln!("warning: sandbox `{}`: {e:#}", image.sandbox);
        }
    }
    let Some((i, baked)) = choose(&images, name) else {
        let mut declaring = Vec::new();
        for sandbox in config.sandboxes.keys() {
            match declares(dir, &config, sandbox, name) {
                Ok(true) => declaring.push(sandbox.clone()),
                Ok(false) => {}
                Err(e) => eprintln!("warning: sandbox `{sandbox}`: {e:#}"),
            }
        }
        bail!(refusal(name, &images, &declaring));
    };
    let sandbox = config.resolve_sandbox(&images[i].sandbox)?;
    let image = run::features_image(&project, &sandbox.name);
    eprintln!("update `{name}`: from `{image}` (sandbox `{}`)", sandbox.name);

    let bind = host_dir(dir, baked)?;
    let props = &sandbox.properties;
    let facts = run::image_facts(&image);
    let (plan, seccomp) = run::plan_for(&image, run::accounts(props, &facts), props, false, false);
    let (identity_args, warning) =
        run::create_args(&plan, b.name() == "podman", seccomp.as_deref(), &[], std::slice::from_ref(&bind));
    if let Some(warning) = warning {
        eprintln!("warning: {warning}");
    }
    let argv = update_argv(&image, identity_args, &baked.update.command, args)?;
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

struct Facts {
    created: Option<u64>,
    labels: BTreeMap<String, String>,
}

/// `None`: the image isn't built (or can't be inspected).
fn facts(b: &dyn Backend, tag: &str) -> Option<Facts> {
    let labels = b.image_labels(tag).ok()?.unwrap_or_default();
    Some(Facts { created: b.image_created(tag).ok().flatten(), labels })
}

struct Image {
    sandbox: String,
    /// Unix seconds; `None` when unknown.
    created: Option<u64>,
    /// The updates its label lists; `Ok(None)` without the label.
    baked: Result<Option<Vec<Baked>>>,
}

/// The built features images of `sandboxes`, in their order.
fn images<'a>(sandboxes: impl Iterator<Item = &'a String>, mut facts: impl FnMut(&str) -> Option<Facts>) -> Vec<Image> {
    sandboxes
        .filter_map(|sandbox| {
            let Facts { created, labels } = facts(sandbox)?;
            let baked = labels.get(updates::LABEL).map(|label| updates::from_label(label)).transpose();
            Some(Image { sandbox: sandbox.clone(), created, baked })
        })
        .collect()
}

fn baked<'a>(image: &'a Image, name: &str) -> Option<&'a Baked> {
    image.baked.as_ref().ok()?.as_ref()?.iter().find(|b| b.update.name == name)
}

/// The newest image whose label lists update `name`, and its entry.
fn choose<'a>(images: &'a [Image], name: &str) -> Option<(usize, &'a Baked)> {
    let built: Vec<Option<Option<u64>>> = images.iter().map(|i| baked(i, name).map(|_| i.created)).collect();
    let i = newest(&built)?;
    Some((i, baked(&images[i], name)?))
}

/// Why no image runs update `name`, `declaring` the sandboxes whose
/// features declare it now.
fn refusal(name: &str, images: &[Image], declaring: &[String]) -> String {
    let quoted = |names: &[&String]| names.iter().map(|s| format!("`{s}`")).collect::<Vec<_>>().join(", ");
    if declaring.is_empty() {
        return format!("no sandbox of this config has a feature declaring update `{name}`");
    }
    let stale: Vec<&String> = declaring.iter().filter(|s| images.iter().any(|i| &i.sandbox == *s)).collect();
    match stale.as_slice() {
        [] => format!(
            "update `{name}`: no image of sandbox {} is built yet; `devsandbox run <sandbox>` builds it",
            quoted(&declaring.iter().collect::<Vec<_>>())
        ),
        [one] => format!("update `{name}`: the image of sandbox `{one}` was built without it; `devsandbox rebuild {one}` adds it"),
        _ => format!(
            "update `{name}`: the images of sandbox {} were built without it; `devsandbox rebuild <sandbox>` adds it",
            quoted(&stale)
        ),
    }
}

/// One update the config root's built images carry.
pub struct Declared {
    /// The first carrying sandbox's (in name order).
    pub update: Update,
    /// Every sandbox whose features image carries it, in name order.
    pub sandboxes: Vec<String>,
}

/// Every update the features images of the config's sandboxes carry, by
/// name, without running any. A sandbox whose image isn't built or has no
/// label carries none; one whose label doesn't validate is in the second
/// list with why.
pub fn declared(project: &str, config: &Config) -> (BTreeMap<String, Declared>, Vec<(String, anyhow::Error)>) {
    let b = backend();
    declared_in(images(config.sandboxes.keys(), |sandbox| facts(b, &run::features_image(project, sandbox))))
}

fn declared_in(images: Vec<Image>) -> (BTreeMap<String, Declared>, Vec<(String, anyhow::Error)>) {
    let mut found: BTreeMap<String, Declared> = BTreeMap::new();
    let mut skipped = Vec::new();
    for image in images {
        let baked = match image.baked {
            Ok(baked) => baked.unwrap_or_default(),
            Err(e) => {
                skipped.push((image.sandbox, e));
                continue;
            }
        };
        for Baked { update, .. } in baked {
            found
                .entry(update.name.clone())
                .or_insert_with(|| Declared { update, sandboxes: Vec::new() })
                .sandboxes
                .push(image.sandbox.clone());
        }
    }
    (found, skipped)
}

/// Whether sandbox `sandbox`'s enabled features declare update `name` now
/// (resolved and fetched into the cache like at `run`): its image predates it.
fn declares(dir: &Path, config: &Config, sandbox: &str, name: &str) -> Result<bool> {
    let resolved = config.resolve_sandbox(sandbox)?;
    let Some(refs) = &resolved.properties.features else { return Ok(false) };
    for (reference, options) in refs {
        if run::feature_option_values(options)
            .with_context(|| format!("feature `{reference}`: invalid options"))?
            .is_none()
        {
            continue;
        }
        let source = features::FeatureSource::parse(reference)?;
        let feature = features::resolve(&source, dir).with_context(|| format!("fetching feature `{reference}`"))?;
        if feature.metadata.updates.iter().any(|u| u.name == name) {
            return Ok(true);
        }
    }
    Ok(false)
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

/// The `--mount` of the update's dir, read-write: its source as the image's
/// label names it, created like a feature mount's (under `shared-volumes/`
/// only, past a worker policy's check).
fn host_dir(dir: &Path, baked: &Baked) -> Result<String> {
    let update = &baked.update;
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
    let mount = Mount::Object(MountObject {
        kind: Some("bind".into()),
        source: Some(baked.source.clone()),
        target: update.mount.clone(),
        readonly: Some(false),
    });
    let mount = mount.resolve(&ctx)?;
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

    fn entry(name: &str, every: &str) -> serde_json::Value {
        serde_json::json!({"feature": "f", "name": name, "mount": "/opt/x", "source": "${sharedVolumes}/x",
            "command": ["/bin/up"], "every": every})
    }

    /// `label`: `None` for an image without one.
    fn image(sandbox: &str, created: Option<u64>, label: Option<serde_json::Value>) -> (String, Facts) {
        let labels = label.map(|l| (updates::LABEL.to_string(), l.to_string())).into_iter().collect();
        (sandbox.to_string(), Facts { created, labels })
    }

    /// `built`: the sandboxes with an image; the rest of `all` have none.
    fn seen(all: &[&str], built: Vec<(String, Facts)>) -> Vec<Image> {
        let mut built: BTreeMap<String, Facts> = built.into_iter().collect();
        let all: Vec<String> = all.iter().map(|s| s.to_string()).collect();
        images(all.iter(), |s| built.remove(s))
    }

    #[test]
    fn choice_goes_to_the_newest_image_labelled_with_it() {
        let images = seen(
            &["a", "b", "c", "d", "e", "f"],
            vec![
                image("a", Some(5), Some(serde_json::json!([entry("upd", "1d")]))),
                image("b", Some(9), None),
                image("c", Some(7), Some(serde_json::json!([entry("other", "1d"), entry("upd", "2d")]))),
                image("d", Some(8), Some(serde_json::json!([entry("upd", "0d")]))),
                image("e", Some(10), Some(serde_json::json!([]))),
            ],
        );
        assert_eq!(images.iter().map(|i| i.sandbox.as_str()).collect::<Vec<_>>(), ["a", "b", "c", "d", "e"]);
        assert!(images[3].baked.is_err());
        let (i, baked) = choose(&images, "upd").unwrap();
        assert_eq!((images[i].sandbox.as_str(), baked.update.every.as_secs()), ("c", 2 * 86400));
        assert_eq!(baked.source, "${sharedVolumes}/x");
        assert!(choose(&images, "nope").is_none());
    }

    #[test]
    fn refusal_names_a_rebuild_when_the_images_predate_the_update() {
        let images = seen(&["a", "b", "c"], vec![image("a", None, None), image("b", None, Some(serde_json::json!([])))]);
        let declaring = |s: &[&str]| s.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            refusal("upd", &images, &[]),
            "no sandbox of this config has a feature declaring update `upd`"
        );
        assert_eq!(
            refusal("upd", &images, &declaring(&["c"])),
            "update `upd`: no image of sandbox `c` is built yet; `devsandbox run <sandbox>` builds it"
        );
        assert_eq!(
            refusal("upd", &images, &declaring(&["a", "c"])),
            "update `upd`: the image of sandbox `a` was built without it; `devsandbox rebuild a` adds it"
        );
        assert_eq!(
            refusal("upd", &images, &declaring(&["a", "b"])),
            "update `upd`: the images of sandbox `a`, `b` were built without it; `devsandbox rebuild <sandbox>` adds it"
        );
    }

    #[test]
    fn declared_lists_each_labelled_update_with_its_sandboxes() {
        let images = seen(
            &["a", "b", "c", "d", "e"],
            vec![
                image("a", None, Some(serde_json::json!([entry("upd", "1d")]))),
                image("b", None, Some(serde_json::json!([entry("upd", "3d"), entry("tool", "1h")]))),
                image("c", None, None),
                image("d", None, Some(serde_json::json!("not a list"))),
            ],
        );
        let (found, skipped) = declared_in(images);
        assert_eq!(found.keys().collect::<Vec<_>>(), ["tool", "upd"]);
        assert_eq!(found["upd"].sandboxes, ["a", "b"]);
        assert_eq!(found["upd"].update.every, std::time::Duration::from_secs(86400));
        assert_eq!(found["tool"].sandboxes, ["b"]);
        // Unlabelled `c` and unbuilt `e` say nothing; only a bad label is reported.
        assert_eq!(skipped.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>(), ["d"]);
    }

    const CHILD_ROOT: &str = "DEVSANDBOX_UPDATE_TEST_ROOT";

    /// The update runs what a sandbox's features image's label says (alpine
    /// plus the label, not the feature's current command), writes its dir
    /// under `shared-volumes/`, gets the CLI's args, and its time lands in
    /// `State`; an image without the label is refused. In a child with its own
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
                "mounts": [{"type": "bind", "source": "${sharedVolumes}/upd-bin", "target": "/opt/upd"}],
                "customizations": {"devsandbox": {"updates": [
                    {"name": "upd", "mount": "/opt/upd", "command": ["/bin/false"]}
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
            let e = format!("{:#}", update(&cfg, "upd", &[]).unwrap_err());
            assert!(e.contains("the image of sandbox `b` was built without it; `devsandbox rebuild b`"), "{e}");

            let label = updates::to_label(&[Baked {
                feature: "./features/upd".into(),
                update: Update {
                    name: "upd".into(),
                    mount: "/opt/upd".into(),
                    command: ["/bin/sh", "-c", "echo \"$@\" > /opt/upd/out", "upd"].map(String::from).into(),
                    every: updates::DEFAULT_EVERY,
                },
                source: "${sharedVolumes}/upd-bin".into(),
            }]);
            let context = root.join("image");
            std::fs::create_dir_all(&context).unwrap();
            std::fs::write(context.join("Dockerfile"), "FROM alpine:3.20\n").unwrap();
            let built = crate::test_support::runtime_cmd()
                .args(["build", "-q", "-t", &tag, "--label", &format!("{}={label}", updates::LABEL)])
                .arg(&context)
                .output()
                .unwrap();
            assert!(built.status.success(), "build: {}", String::from_utf8_lossy(&built.stderr));

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
