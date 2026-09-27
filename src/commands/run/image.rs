//! Feature image build: the derived image with devcontainer features installed,
//! the generated Dockerfile and entrypoint chain, and what features ask of the
//! container (mounts, `--privileged`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::commands::services;
use crate::config::{
    parse_shorthand, FeatureOptions, MountContext, ResolvedSandbox, SandboxProperties,
};
use crate::features::{self, FeatureMetadata};
use crate::runtime::{backend, NAME_PREFIX};

use super::mounts::{feature_mount_decision, MountDecision};

/// What the sandbox's enabled features ask of the container itself (the image
/// side lives in [`build_features_image`]).
pub(super) struct FeatureContainerOpts {
    /// `--mount` args.
    pub(super) mounts: Vec<String>,
    /// Per-instance volume names among `mounts` (for `rm`).
    pub(super) volumes: Vec<String>,
    /// Refs of the enabled features declaring `privileged: true`; the backend
    /// check happens in [`privileged_decision`].
    pub(super) privileged_by: Vec<String>,
}

/// Whether to pass `--privileged`, plus the warnings to print. Requested by the
/// sandbox's `privileged = true` or any enabled feature's `privileged: true`;
/// on a backend without the flag every request is skipped with a warning, like
/// an unsatisfiable feature mount.
pub(super) fn privileged_decision(sandbox: bool, features: &[String], supported: bool) -> (bool, Vec<String>) {
    let requested = sandbox || !features.is_empty();
    if supported || !requested {
        return (requested, Vec::new());
    }
    let skip = "this runtime cannot run privileged containers; skipping `privileged`";
    let mut warnings: Vec<String> =
        features.iter().map(|r| format!("feature `{r}`: {skip}")).collect();
    if sandbox {
        warnings.insert(0, skip.to_string());
    }
    (false, warnings)
}

/// Resolve the container-side settings declared by the sandbox's enabled
/// features (fetches hit the per-user cache also used by the image build).
///
/// Mounts are turned into `--mount` args. The
/// sandbox always wins: a feature mount whose target is already claimed is
/// dropped, so the config can replace e.g. docker-outside-of-docker's hardcoded
/// `/var/run/docker.sock` source with a rootless socket path. Mounts the
/// backend cannot apply, or whose bind source is missing on the host, are
/// skipped with a warning — auto-creating the source (the config-mount
/// behavior) would hand the container an empty stub instead of a clear signal.
pub(super) fn feature_container_opts(
    props: &SandboxProperties,
    ctx: &MountContext,
    existing: &[String],
    workspace: &str,
) -> Result<FeatureContainerOpts> {
    let mut opts = FeatureContainerOpts {
        mounts: Vec::new(),
        volumes: Vec::new(),
        privileged_by: Vec::new(),
    };
    let Some(features) = &props.features else {
        return Ok(opts);
    };
    let mut taken: BTreeSet<String> = existing
        .iter()
        .filter_map(|arg| parse_shorthand(arg).ok().map(|(_, _, target, _)| target))
        .collect();
    taken.insert(workspace.to_string());
    for (reference, options) in features {
        if feature_option_values(options)
            .with_context(|| format!("feature `{reference}`: invalid options"))?
            .is_none()
        {
            continue; // `= false`: disabled.
        }
        let parsed = features::FeatureRef::parse(reference)?;
        let feature =
            features::fetch(&parsed).with_context(|| format!("fetching feature `{reference}`"))?;
        if feature.metadata.privileged == Some(true) {
            opts.privileged_by.push(reference.clone());
        }
        for mount in &feature.metadata.mounts {
            let resolved = mount.resolve(ctx)?;
            let probe = |source: &str| std::fs::metadata(source).ok().map(|m| !m.is_dir());
            match feature_mount_decision(
                &resolved,
                &taken,
                backend().supports_file_binds(),
                probe,
            ) {
                MountDecision::Apply => {
                    taken.insert(resolved.target.clone());
                    opts.mounts.push(resolved.to_arg());
                    if resolved.per_instance {
                        opts.volumes.extend(resolved.source.clone());
                    }
                }
                MountDecision::Overridden => {}
                MountDecision::Skip(reason) => {
                    eprintln!("warning: feature `{reference}`: {reason}");
                }
            }
        }
    }
    Ok(opts)
}

/// Image to run: the `image` property as-is, or a local build for
/// `build.dockerfile` sandboxes. When the sandbox declares `features`, the base
/// (image or build) is extended into a derived image tagged
/// `devsandbox-img-<sandbox>-feat`.
pub(super) fn image_for(dir: &Path, sandbox: &ResolvedSandbox) -> Result<String> {
    let base = base_image_for(dir, sandbox)?;
    let props = &sandbox.properties;
    match &props.features {
        Some(features) if !features.is_empty() => build_features_image(sandbox, &base, features),
        _ => Ok(base),
    }
}

/// The base image: the `image` property as-is, or a local build for
/// `build.dockerfile` sandboxes.
fn base_image_for(dir: &Path, sandbox: &ResolvedSandbox) -> Result<String> {
    let props = &sandbox.properties;
    if let Some(image) = &props.image {
        return Ok(image.clone());
    }
    let build = props
        .build
        .as_ref()
        .with_context(|| format!("sandbox `{}` has neither `image` nor `build`", sandbox.name))?;
    let dockerfile = build
        .dockerfile
        .as_deref()
        .with_context(|| format!("sandbox `{}`: `build.dockerfile` is required", sandbox.name))?;
    let tag = format!("{NAME_PREFIX}img-{}", sandbox.name);
    let args = services::build_args(
        backend(),
        dir,
        &tag,
        dockerfile,
        build,
        &format!("sandbox `{}`", sandbox.name),
    );
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    backend().run_checked(&arg_refs)?;
    Ok(tag)
}

// ------------------------------------------------------------------------
// Derived-image build from devcontainer `features`.
//
// The impure orchestration (`build_features_image`) fetches + orders the
// features, assembles a build context, and shells out to the runtime. The
// generation of the Dockerfile, env-files, and install wrapper is factored into
// pure functions (below) so they're unit-testable without docker or network.
// The layout mirrors the official devcontainers CLI
// (`containerFeaturesConfiguration.ts`, `containerFeatures.ts`).
// ------------------------------------------------------------------------

/// A feature ready to be baked in: its build-context directory name, its id,
/// metadata, and the resolved option map (defaults merged under user values).
struct FeatureBuild {
    /// `<idx>-<id>`, the per-feature directory name in the build context.
    dir_name: String,
    metadata: FeatureMetadata,
}

/// Fetch + order the sandbox's features, assemble a build context under the
/// features cache, and build the derived image. Returns its tag.
fn build_features_image(
    sandbox: &ResolvedSandbox,
    base: &str,
    features: &BTreeMap<String, FeatureOptions>,
) -> Result<String> {
    let props = &sandbox.properties;

    // Resolve each config entry to a fetched feature + its option map, skipping
    // features explicitly disabled with `= false`.
    let mut resolved = Vec::new();
    let mut options_by_key: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for (reference, opts) in features {
        let Some(values) = feature_option_values(opts)
            .with_context(|| format!("feature `{reference}`: invalid options"))?
        else {
            continue; // `= false`: skip entirely.
        };
        let parsed = features::FeatureRef::parse(reference)?;
        let feature =
            features::fetch(&parsed).with_context(|| format!("fetching feature `{reference}`"))?;
        options_by_key.insert(feature.reference.full(), values);
        resolved.push(feature);
    }
    if resolved.is_empty() {
        return Ok(base.to_string()); // all features disabled.
    }

    let ordered = features::install_order(resolved)?;

    let container_user = props.container_user.as_deref().unwrap_or("root");
    let remote_user = props.remote_user.as_deref().unwrap_or(container_user);

    // Fresh build context.
    let context = features::build_context_dir(&sandbox.name)?;
    std::fs::write(
        context.join("devcontainer-features.builtin.env"),
        builtin_env(container_user, remote_user),
    )
    .with_context(|| format!("cannot write builtin env in {}", context.display()))?;

    // Per feature: copy its cached dir in under `<idx>-<id>`, write the env-file
    // and install wrapper, and collect what the Dockerfile generator needs.
    let mut builds = Vec::with_capacity(ordered.len());
    for (idx, feature) in ordered.iter().enumerate() {
        let id = feature
            .metadata
            .id
            .clone()
            .unwrap_or_else(|| feature.reference.id.clone());
        let dir_name = format!("{idx}-{id}");
        let dest = context.join(&dir_name);
        copy_dir(&feature.dir, &dest).with_context(|| {
            format!("copying feature `{}` into build context", feature.reference.full())
        })?;

        let values = options_by_key.remove(&feature.reference.full()).unwrap_or_default();
        let env_file = feature_env_file(&feature.metadata, &values);
        std::fs::write(dest.join("devcontainer-features.env"), &env_file)
            .with_context(|| format!("cannot write env file for `{}`", feature.reference.full()))?;
        let wrapper =
            install_wrapper(&feature.metadata, &feature.reference.without_tag(), &env_file);
        std::fs::write(dest.join("devcontainer-features-install.sh"), wrapper).with_context(
            || format!("cannot write install wrapper for `{}`", feature.reference.full()),
        )?;

        builds.push(FeatureBuild {
            dir_name,
            metadata: feature.metadata.clone(),
        });
    }

    let dockerfile =
        generate_dockerfile(base, &builds, props.container_user.as_deref(), remote_user);
    let dockerfile_path = context.join("Dockerfile.devsandbox-features");
    std::fs::write(&dockerfile_path, dockerfile)
        .with_context(|| format!("cannot write {}", dockerfile_path.display()))?;

    let tag = format!("{NAME_PREFIX}img-{}-feat", sandbox.name);
    backend().run_checked(&[
        "build",
        "-t",
        &tag,
        "-f",
        &dockerfile_path.to_string_lossy(),
        &context.to_string_lossy(),
    ])?;

    // Feature entrypoints: a second, metadata-only build on the same tag. The
    // image's own ENTRYPOINT is read from the first build (which inherits the
    // base's untouched) because a Dockerfile cannot reference its parent's.
    let entrypoints: Vec<(String, String)> = ordered
        .iter()
        .filter_map(|f| {
            let entrypoint = f.metadata.entrypoint.as_deref()?.trim();
            (!entrypoint.is_empty()).then(|| (f.reference.without_tag(), entrypoint.to_string()))
        })
        .collect();
    if !entrypoints.is_empty() {
        let image_entrypoint = backend()
            .image_entrypoint(&tag)
            .with_context(|| format!("cannot read the ENTRYPOINT of `{tag}`"))?;
        std::fs::write(context.join(ENTRYPOINT_WRAPPER_FILE), entrypoint_wrapper(&entrypoints))
            .with_context(|| format!("cannot write entrypoint wrapper in {}", context.display()))?;
        let dockerfile_path = context.join("Dockerfile.devsandbox-entrypoint");
        std::fs::write(&dockerfile_path, entrypoint_dockerfile(&tag, &image_entrypoint))
            .with_context(|| format!("cannot write {}", dockerfile_path.display()))?;
        backend().run_checked(&[
            "build",
            "-t",
            &tag,
            "-f",
            &dockerfile_path.to_string_lossy(),
            &context.to_string_lossy(),
        ])?;
    }
    Ok(tag)
}

/// Build-context name of the generated entrypoint chain.
const ENTRYPOINT_WRAPPER_FILE: &str = "devsandbox-entrypoint.sh";

/// Where the entrypoint chain lives in the derived image.
const ENTRYPOINT_WRAPPER_PATH: &str = "/usr/local/share/devsandbox-entrypoint.sh";

/// The entrypoint chain: each feature's `entrypoint` (`(id, command)` pairs,
/// in install order), then `exec "$@"`, which is the image's own ENTRYPOINT
/// followed by the container command. Each feature entrypoint runs with no
/// arguments, so its trailing `exec "$@"` is a no-op and it returns. The
/// command is inserted verbatim, as the official CLI does. A failing one warns
/// and the chain continues (no `set -e`, same as the CLI), so a broken feature
/// can't keep the sandbox from starting.
fn entrypoint_wrapper(entrypoints: &[(String, String)]) -> String {
    let mut out = String::from(
        "#!/bin/sh\n\
# Generated by devsandbox: feature entrypoints (install order), then the\n\
# image's own ENTRYPOINT + the container command.\n",
    );
    for (id, entrypoint) in entrypoints {
        out.push_str(&format!(
            "{entrypoint} || echo \"devsandbox: entrypoint of feature '{id}' exited with status $?; continuing\" >&2\n"
        ));
    }
    out.push_str("exec \"$@\"\n");
    out
}

/// The second-stage Dockerfile that installs the entrypoint chain on top of
/// `image`, keeping `image_entrypoint` (the image's own ENTRYPOINT) as the
/// chain's arguments. The chain runs through `/bin/sh` so it needs no exec bit
/// (COPY mode handling differs between builders).
fn entrypoint_dockerfile(image: &str, image_entrypoint: &[String]) -> String {
    let mut entrypoint = vec!["/bin/sh".to_string(), ENTRYPOINT_WRAPPER_PATH.to_string()];
    entrypoint.extend(image_entrypoint.iter().cloned());
    format!(
        "FROM {image}\n\
COPY {ENTRYPOINT_WRAPPER_FILE} {ENTRYPOINT_WRAPPER_PATH}\n\
ENTRYPOINT {}\n",
        serde_json::to_string(&entrypoint).expect("strings serialize"),
    )
}

/// Turn a config `FeatureOptions` into a stringified option map, or `None` when
/// the feature is disabled (`= false`). `Options` tables reject array/table
/// values with a clear error; scalars stringify (strings as-is, bools
/// `true`/`false`, ints/floats via their `Display`).
fn feature_option_values(opts: &FeatureOptions) -> Result<Option<BTreeMap<String, String>>> {
    match opts {
        FeatureOptions::Enabled(false) => Ok(None),
        FeatureOptions::Enabled(true) => Ok(Some(BTreeMap::new())),
        FeatureOptions::Version(v) => {
            Ok(Some(BTreeMap::from([("version".to_string(), v.clone())])))
        }
        FeatureOptions::Options(table) => {
            let mut out = BTreeMap::new();
            for (key, value) in table {
                out.insert(key.clone(), toml_option_value(key, value)?);
            }
            Ok(Some(out))
        }
    }
}

/// Stringify a scalar TOML option value. Arrays and tables are rejected: feature
/// options are flat scalars.
fn toml_option_value(key: &str, value: &toml::Value) -> Result<String> {
    match value {
        toml::Value::String(s) => Ok(s.clone()),
        toml::Value::Boolean(b) => Ok(b.to_string()),
        toml::Value::Integer(i) => Ok(i.to_string()),
        toml::Value::Float(f) => Ok(f.to_string()),
        toml::Value::Datetime(d) => Ok(d.to_string()),
        toml::Value::Array(_) | toml::Value::Table(_) => {
            bail!("option `{key}` must be a string, bool, or number, not an array or table")
        }
    }
}

/// The `devcontainer-features.builtin.env` body: `_CONTAINER_USER` and
/// `_REMOTE_USER`. The `_*_HOME` lines are appended in-container by the Dockerfile.
fn builtin_env(container_user: &str, remote_user: &str) -> String {
    format!("_CONTAINER_USER={container_user}\n_REMOTE_USER={remote_user}\n")
}

/// A feature's option env-name, mirroring the CLI's `getSafeId`: non-word chars
/// (outside `[A-Za-z0-9_]`) become `_`, then any leading run of digits and
/// underscores collapses to a single `_`, then the whole thing is uppercased.
fn safe_id(name: &str) -> String {
    let mapped: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    let trimmed = mapped.trim_start_matches(|c: char| c.is_ascii_digit() || c == '_');
    let out = if trimmed.len() == mapped.len() {
        mapped
    } else {
        format!("_{trimmed}")
    };
    out.to_uppercase()
}

/// Escape a value for a double-quoted context: backslash and double-quote only.
/// A literal dollar sign is left intact so a value like /usr/local/go/bin:${PATH}
/// still expands at build time when emitted as an ENV instruction.
fn escape_dq(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The `devcontainer-features.env` body: one `NAME="value"` per option, names via
/// [`safe_id`], values = metadata defaults overridden by user-provided values.
fn feature_env_file(metadata: &FeatureMetadata, values: &BTreeMap<String, String>) -> String {
    // Start from metadata defaults, override with user values. BTreeMap keeps a
    // stable (alphabetical) order.
    let mut merged: BTreeMap<String, String> = BTreeMap::new();
    for (name, option) in &metadata.options {
        if let Some(default) = &option.default {
            merged.insert(name.clone(), json_scalar(default));
        }
    }
    for (name, value) in values {
        merged.insert(name.clone(), value.clone());
    }
    merged
        .iter()
        .map(|(name, value)| format!("{}=\"{}\"", safe_id(name), escape_dq(value)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Stringify a JSON scalar option default: strings as-is, everything else via its
/// JSON rendering (bool becomes `true`/`false`, numbers become their text).
fn json_scalar(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Single-quote escape for a value printed inside `'...'` in the wrapper banner,
/// mirroring the CLI's `escapeQuotesForShell`: each `'` becomes `'\''`.
fn escape_sq(value: &str) -> String {
    value.replace('\'', "'\\''")
}

/// The per-feature `devcontainer-features-install.sh` wrapper: a banner echoing
/// the feature identity + options, then `set -a` sourcing of the builtin and
/// feature env-files, then `./install.sh`. `env_file` is the already-generated
/// `devcontainer-features.env` body, echoed (indented) in the banner.
fn install_wrapper(metadata: &FeatureMetadata, id: &str, env_file: &str) -> String {
    let name = metadata.name.as_deref().unwrap_or("Unknown");
    let version = metadata.version.as_deref().unwrap_or("");
    let documentation = metadata.documentation_url.as_deref().unwrap_or("");
    let options_indented = env_file
        .lines()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "#!/bin/sh\n\
set -e\n\
\n\
echo ===========================================================================\n\
echo 'Feature       : {name}'\n\
echo 'Id            : {id}'\n\
echo 'Version       : {version}'\n\
echo 'Documentation : {documentation}'\n\
echo 'Options       :'\n\
echo '{options}'\n\
echo ===========================================================================\n\
\n\
set -a\n\
. ../devcontainer-features.builtin.env\n\
. ./devcontainer-features.env\n\
set +a\n\
\n\
chmod +x ./install.sh\n\
./install.sh\n",
        name = escape_sq(name),
        id = escape_sq(id),
        version = escape_sq(version),
        documentation = escape_sq(documentation),
        options = escape_sq(&options_indented),
    )
}

/// Generate the derived-image Dockerfile. `container_user` is `Some` only when
/// the sandbox set `containerUser`; when `None` the final stage stays `root`.
/// `remote_user` is the effective remote user (used for the `_*_HOME` probe).
fn generate_dockerfile(
    base: &str,
    features: &[FeatureBuild],
    container_user: Option<&str>,
    remote_user: &str,
) -> String {
    let effective_container_user = container_user.unwrap_or("root");
    let mut out = String::new();
    out.push_str(&format!("FROM {base}\n"));
    out.push_str("USER root\n");
    out.push_str("RUN mkdir -p /tmp/dev-container-features\n");
    out.push_str("COPY devcontainer-features.builtin.env /tmp/dev-container-features/\n");
    // Append the user home dirs to the builtin env, resolved in-container.
    out.push_str(&format!(
        "RUN echo \"_CONTAINER_USER_HOME=$(getent passwd {cu} | cut -d: -f6)\" \
>> /tmp/dev-container-features/devcontainer-features.builtin.env && \
echo \"_REMOTE_USER_HOME=$(getent passwd {ru} | cut -d: -f6)\" \
>> /tmp/dev-container-features/devcontainer-features.builtin.env\n",
        cu = effective_container_user,
        ru = remote_user,
    ));
    for feature in features {
        for (key, value) in &feature.metadata.container_env {
            out.push_str(&format!("ENV {key}=\"{}\"\n", escape_dq(value)));
        }
        let dir = &feature.dir_name;
        out.push_str(&format!("COPY {dir} /tmp/dev-container-features/{dir}\n"));
        out.push_str(&format!(
            "RUN chmod -R 0755 /tmp/dev-container-features/{dir} && \
cd /tmp/dev-container-features/{dir} && \
chmod +x ./devcontainer-features-install.sh && \
./devcontainer-features-install.sh\n",
        ));
    }
    if let Some(user) = container_user {
        out.push_str(&format!("USER {user}\n"));
    }
    out
}

/// Recursively copy `src` into `dst` (created if missing), using plain `std::fs`
/// (no external deps). Files are copied; nested dirs recursed.
fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst).with_context(|| format!("cannot create {}", dst.display()))?;
    for entry in std::fs::read_dir(src).with_context(|| format!("cannot read {}", src.display()))? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)
                .with_context(|| format!("cannot copy {} to {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::FeatureOption;

    #[test]
    fn privileged_decision_ors_sandbox_and_features() {
        let dind = ["ghcr.io/devcontainers/features/docker-in-docker:2".to_string()];
        assert_eq!(privileged_decision(false, &[], true), (false, vec![]));
        assert_eq!(privileged_decision(true, &[], true), (true, vec![]));
        assert_eq!(privileged_decision(false, &dind, true), (true, vec![]));
        assert_eq!(privileged_decision(true, &dind, true), (true, vec![]));
    }

    #[test]
    fn privileged_decision_unsupported_warns_per_request() {
        // Apple `container` has no `--privileged`.
        let dind = ["ghcr.io/devcontainers/features/docker-in-docker:2".to_string()];
        assert_eq!(privileged_decision(false, &[], false), (false, vec![]));
        let (on, warnings) = privileged_decision(true, &dind, false);
        assert!(!on);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].starts_with("this runtime cannot"), "{warnings:?}");
        assert!(warnings[1].contains("docker-in-docker"), "{warnings:?}");
    }

    fn md() -> FeatureMetadata {
        FeatureMetadata::default()
    }

    fn opt_default(v: serde_json::Value) -> FeatureOption {
        FeatureOption { default: Some(v) }
    }

    #[test]
    fn safe_id_maps_like_get_safe_id() {
        // Dashes and dots become underscores; uppercased.
        assert_eq!(safe_id("install-jq"), "INSTALL_JQ");
        assert_eq!(safe_id("version"), "VERSION");
        // A leading digit run collapses to a single underscore.
        assert_eq!(safe_id("2fa"), "_FA");
        assert_eq!(safe_id("123abc"), "_ABC");
        // Leading underscores/digits collapse together to one underscore.
        assert_eq!(safe_id("_1_x"), "_X");
        // Already-safe names just uppercase.
        assert_eq!(safe_id("NODE_gyp"), "NODE_GYP");
    }

    #[test]
    fn escape_dq_escapes_quote_and_backslash_not_dollar() {
        assert_eq!(escape_dq(r#"a"b"#), r#"a\"b"#);
        assert_eq!(escape_dq(r"a\b"), r"a\\b");
        // Dollar stays literal so ENV expands ${PATH} at build time.
        assert_eq!(escape_dq("/usr/local/go/bin:${PATH}"), "/usr/local/go/bin:${PATH}");
    }

    #[test]
    fn feature_option_values_scalar_forms() {
        assert_eq!(
            feature_option_values(&FeatureOptions::Version("1.22".into())).unwrap(),
            Some(BTreeMap::from([("version".to_string(), "1.22".to_string())]))
        );
        assert_eq!(
            feature_option_values(&FeatureOptions::Enabled(true)).unwrap(),
            Some(BTreeMap::new())
        );
        // Disabled => skip the feature entirely.
        assert_eq!(feature_option_values(&FeatureOptions::Enabled(false)).unwrap(), None);

        let table = BTreeMap::from([
            ("version".to_string(), toml::Value::String("lts".into())),
            ("installTools".to_string(), toml::Value::Boolean(true)),
            ("uid".to_string(), toml::Value::Integer(1000)),
        ]);
        let values = feature_option_values(&FeatureOptions::Options(table)).unwrap().unwrap();
        assert_eq!(values["version"], "lts");
        assert_eq!(values["installTools"], "true");
        assert_eq!(values["uid"], "1000");
    }

    #[test]
    fn feature_option_values_rejects_arrays_and_tables() {
        let table = BTreeMap::from([(
            "list".to_string(),
            toml::Value::Array(vec![toml::Value::String("a".into())]),
        )]);
        let err = feature_option_values(&FeatureOptions::Options(table)).unwrap_err().to_string();
        assert!(err.contains("array or table"), "{err}");
    }

    #[test]
    fn feature_env_file_merges_defaults_then_user_override() {
        let mut m = md();
        m.options.insert("version".into(), opt_default(serde_json::json!("latest")));
        m.options.insert("installTools".into(), opt_default(serde_json::json!(true)));
        m.options.insert("uid".into(), opt_default(serde_json::json!(1000)));
        // User overrides `version`; leaves the others at their default.
        let values = BTreeMap::from([("version".to_string(), "1.22".to_string())]);
        let env = feature_env_file(&m, &values);
        // BTreeMap => alphabetical option order (installTools, uid, version).
        assert_eq!(
            env,
            "INSTALLTOOLS=\"true\"\nUID=\"1000\"\nVERSION=\"1.22\""
        );
    }

    #[test]
    fn feature_env_file_escapes_values() {
        let values = BTreeMap::from([("flag".to_string(), r#"a"b\c"#.to_string())]);
        let env = feature_env_file(&md(), &values);
        assert_eq!(env, r#"FLAG="a\"b\\c""#);
    }

    #[test]
    fn install_wrapper_has_banner_sourcing_and_exec() {
        let mut m = md();
        m.name = Some("O'Brien's Feature".into());
        m.version = Some("1.0.0".into());
        let env_file = "VERSION=\"1.22\"";
        let w = install_wrapper(&m, "ghcr.io/x/y", env_file);
        assert!(w.starts_with("#!/bin/sh\nset -e\n"), "{w}");
        // Single-quote escaping in the banner name.
        assert!(w.contains("echo 'Feature       : O'\\''Brien'\\''s Feature'"), "{w}");
        assert!(w.contains("echo 'Id            : ghcr.io/x/y'"), "{w}");
        // Options echoed, indented.
        assert!(w.contains("echo '    VERSION=\"1.22\"'"), "{w}");
        // set -a sourcing block, in order.
        assert!(w.contains("set -a\n. ../devcontainer-features.builtin.env\n. ./devcontainer-features.env\nset +a"), "{w}");
        // Runs install.sh at the end.
        assert!(w.trim_end().ends_with("chmod +x ./install.sh\n./install.sh"), "{w}");
    }

    #[test]
    fn entrypoint_dockerfile_chains_image_entrypoint() {
        let df = entrypoint_dockerfile("devsandbox-img-s-feat", &["/usr/bin/tini".into(), "--".into()]);
        assert_eq!(
            df,
            "FROM devsandbox-img-s-feat\n\
COPY devsandbox-entrypoint.sh /usr/local/share/devsandbox-entrypoint.sh\n\
ENTRYPOINT [\"/bin/sh\",\"/usr/local/share/devsandbox-entrypoint.sh\",\"/usr/bin/tini\",\"--\"]\n"
        );
        // No image ENTRYPOINT: the chain execs the container command directly.
        assert!(entrypoint_dockerfile("i", &[])
            .contains("ENTRYPOINT [\"/bin/sh\",\"/usr/local/share/devsandbox-entrypoint.sh\"]\n"));
    }

    /// Runs the generated chain under the real `/bin/sh`: feature entrypoints
    /// in order with no arguments, a failing one warns without stopping the
    /// chain, then `exec "$@"` hands over to the image entrypoint + command.
    #[test]
    fn entrypoint_wrapper_runs_in_order_and_execs_args() {
        let dir = std::env::temp_dir().join(format!(
            "devsandbox-ep-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("log");
        let script = |name: &str, body: &str| {
            let path = dir.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            path.display().to_string()
        };
        // Same shape as docker-in-docker's docker-init.sh: work, then exec "$@".
        let first = script("a.sh", &format!("echo \"a:$#\" >> {}\nexec \"$@\"", log.display()));
        let broken = script("b.sh", "exit 3");
        let wrapper = entrypoint_wrapper(&[
            ("ghcr.io/x/a".into(), format!("sh {first}")),
            ("ghcr.io/x/b".into(), format!("sh {broken}")),
        ]);
        let out = std::process::Command::new("/bin/sh")
            .args(["-c", &wrapper, "sh", "sh", "-c"])
            .arg(format!("echo \"cmd:$0\" >> {}", log.display()))
            .arg("sleep")
            .output()
            .unwrap();
        let logged = std::fs::read_to_string(&log).unwrap_or_default();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(out.status.success(), "{stderr}");
        assert_eq!(logged, "a:0\ncmd:sleep\n");
        assert!(stderr.contains("entrypoint of feature 'ghcr.io/x/b' exited with status 3; continuing"), "{stderr}");
    }

    #[test]
    fn builtin_env_lines() {
        assert_eq!(builtin_env("root", "vscode"), "_CONTAINER_USER=root\n_REMOTE_USER=vscode\n");
    }

    fn fb(dir_name: &str, container_env: &[(&str, &str)]) -> FeatureBuild {
        let mut m = md();
        m.container_env = container_env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        FeatureBuild { dir_name: dir_name.into(), metadata: m }
    }

    #[test]
    fn dockerfile_base_env_escaping_and_copy_run() {
        let features = vec![fb(
            "0-go",
            &[
                ("PATH", "/usr/local/go/bin:${PATH}"),
                ("QUOTED", r#"a"b\c"#),
            ],
        )];
        let df = generate_dockerfile("alpine:3.20", &features, None, "root");
        assert!(df.starts_with("FROM alpine:3.20\nUSER root\n"), "{df}");
        assert!(df.contains("RUN mkdir -p /tmp/dev-container-features\n"), "{df}");
        assert!(
            df.contains("COPY devcontainer-features.builtin.env /tmp/dev-container-features/\n"),
            "{df}"
        );
        // Home probe uses getent for both users (root here).
        assert!(
            df.contains("_CONTAINER_USER_HOME=$(getent passwd root | cut -d: -f6)")
                && df.contains("_REMOTE_USER_HOME=$(getent passwd root | cut -d: -f6)"),
            "{df}"
        );
        // ENV: $ preserved, " and \ escaped.
        assert!(df.contains("ENV PATH=\"/usr/local/go/bin:${PATH}\"\n"), "{df}");
        assert!(df.contains("ENV QUOTED=\"a\\\"b\\\\c\"\n"), "{df}");
        // Per-feature COPY + RUN.
        assert!(df.contains("COPY 0-go /tmp/dev-container-features/0-go\n"), "{df}");
        assert!(
            df.contains(
                "RUN chmod -R 0755 /tmp/dev-container-features/0-go && \
cd /tmp/dev-container-features/0-go && \
chmod +x ./devcontainer-features-install.sh && \
./devcontainer-features-install.sh\n"
            ),
            "{df}"
        );
        // No containerUser => stays root, no trailing USER line.
        assert!(!df.contains("\nUSER root\nUSER"), "{df}");
        assert_eq!(df.matches("USER ").count(), 1, "only the initial USER root: {df}");
    }

    #[test]
    fn dockerfile_restores_container_user_when_set() {
        let features = vec![fb("0-common-utils", &[])];
        let df = generate_dockerfile("debian:bookworm", &features, Some("vscode"), "vscode");
        // Final USER line restores the configured container user.
        assert!(df.trim_end().ends_with("USER vscode"), "{df}");
        // Home probe uses the configured users.
        assert!(df.contains("_CONTAINER_USER_HOME=$(getent passwd vscode | cut -d: -f6)"), "{df}");
        assert!(df.contains("_REMOTE_USER_HOME=$(getent passwd vscode | cut -d: -f6)"), "{df}");
    }

    /// Docker-gated: builds a derived image from a synthetic pre-extracted feature
    /// (no network) using the pure generators, then runs it and checks the option
    /// value and remote user landed. Skips (with a message) when `docker info`
    /// fails — so it's a no-op in a sandbox without docker, but exercises the real
    /// build on CI (where it fails instead of skipping).
    #[test_utils::docker_test]
    fn derived_image_builds_with_docker() -> Result<(), &'static str> {
        use std::process::Command;

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let ctx = std::env::temp_dir().join(format!("devsandbox-feat-build-{stamp}"));
        std::fs::create_dir_all(&ctx).unwrap();
        let tag = format!("devsandbox-feat-test-{stamp}");

        let cleanup = |ctx: &Path, tag: &str| {
            let _ = std::fs::remove_dir_all(ctx);
            let _ = Command::new("docker").args(["rmi", "-f", tag]).output();
        };

        // Synthetic feature: one option `myopt` (default "fallback"), a
        // containerEnv, and an install.sh writing a marker with the option and
        // remote user. Uses /bin/sh (alpine busybox).
        let mut m = md();
        m.id = Some("marker".into());
        m.name = Some("Marker".into());
        m.version = Some("1.0.0".into());
        m.options.insert("myopt".into(), opt_default(serde_json::json!("fallback")));
        m.container_env = BTreeMap::from([("MARKER_ENV".to_string(), "from-env".to_string())]);

        let feat_dir = ctx.join("0-marker");
        std::fs::create_dir_all(&feat_dir).unwrap();
        std::fs::write(
            feat_dir.join("install.sh"),
            "#!/bin/sh\nset -e\nmkdir -p /opt\n\
             echo \"myopt=$MYOPT remote=$_REMOTE_USER env=$MARKER_ENV\" > /opt/marker\n",
        )
        .unwrap();

        // Env-file: user overrides myopt to "chosen".
        let values = BTreeMap::from([("myopt".to_string(), "chosen".to_string())]);
        let env_file = feature_env_file(&m, &values);
        std::fs::write(feat_dir.join("devcontainer-features.env"), &env_file).unwrap();
        std::fs::write(
            feat_dir.join("devcontainer-features-install.sh"),
            install_wrapper(&m, "local/marker", &env_file),
        )
        .unwrap();
        std::fs::write(
            ctx.join("devcontainer-features.builtin.env"),
            builtin_env("root", "root"),
        )
        .unwrap();

        let builds = vec![FeatureBuild { dir_name: "0-marker".into(), metadata: m }];
        let dockerfile = generate_dockerfile("alpine:3.20", &builds, None, "root");
        let dockerfile_path = ctx.join("Dockerfile.devsandbox-features");
        std::fs::write(&dockerfile_path, dockerfile).unwrap();

        let build = Command::new("docker")
            .args([
                "build",
                "-t",
                &tag,
                "-f",
                &dockerfile_path.to_string_lossy(),
                &ctx.to_string_lossy(),
            ])
            .output()
            .unwrap();
        if !build.status.success() {
            let stderr = String::from_utf8_lossy(&build.stderr).into_owned();
            cleanup(&ctx, &tag);
            panic!("docker build failed: {stderr}");
        }

        let run = Command::new("docker")
            .args(["run", "--rm", &tag, "cat", "/opt/marker"])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&run.stdout).into_owned();
        let ok = run.status.success();
        cleanup(&ctx, &tag);
        assert!(ok, "docker run failed: {}", String::from_utf8_lossy(&run.stderr));
        assert_eq!(stdout.trim(), "myopt=chosen remote=root env=from-env", "marker: {stdout}");
        Ok(())
    }

    /// Docker-gated: an image with its own ENTRYPOINT gets the feature
    /// entrypoint chain via the real inspect + same-tag rebuild, and a run
    /// executes feature entrypoint, then image entrypoint, then the command.
    #[test_utils::docker_test]
    fn entrypoint_chain_builds_with_docker() -> Result<(), &'static str> {
        use std::process::Command;

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let ctx = std::env::temp_dir().join(format!("devsandbox-ep-build-{stamp}"));
        std::fs::create_dir_all(&ctx).unwrap();
        let tag = format!("devsandbox-ep-test-{stamp}");
        let build = |dockerfile: &str| {
            let path = ctx.join("Dockerfile");
            std::fs::write(&path, dockerfile).unwrap();
            Command::new("docker")
                .args(["build", "-q", "-t", &tag, "-f", &path.to_string_lossy(), &ctx.to_string_lossy()])
                .output()
                .unwrap()
        };
        let cleanup = || {
            let _ = std::fs::remove_dir_all(&ctx);
            let _ = Command::new("docker").args(["rmi", "-f", &tag]).output();
        };

        // Stand-in for the features build: a feature entrypoint baked in, and
        // an image ENTRYPOINT that must keep running (and pass the command on).
        let base = build(
            "FROM alpine:3.20\n\
RUN printf '#!/bin/sh\\necho feature >> /tmp/log\\nexec \"$@\"\\n' > /usr/local/share/f-init.sh && chmod +x /usr/local/share/f-init.sh\n\
ENTRYPOINT [\"/bin/sh\", \"-c\", \"echo image >> /tmp/log; exec \\\"$@\\\"\", \"image-ep\"]\n",
        );
        if !base.status.success() {
            let stderr = String::from_utf8_lossy(&base.stderr).into_owned();
            cleanup();
            panic!("base build failed: {stderr}");
        }

        let result = (|| -> Result<String, String> {
            use crate::runtime::Backend;
            let image_entrypoint = crate::runtime::Dockerlike::DOCKER
                .image_entrypoint(&tag)
                .map_err(|e| format!("{e:#}"))?;
            std::fs::write(
                ctx.join(ENTRYPOINT_WRAPPER_FILE),
                entrypoint_wrapper(&[("local/f".into(), "/usr/local/share/f-init.sh".into())]),
            )
            .unwrap();
            let chained = build(&entrypoint_dockerfile(&tag, &image_entrypoint));
            if !chained.status.success() {
                return Err(String::from_utf8_lossy(&chained.stderr).into_owned());
            }
            let run = Command::new("docker")
                .args(["run", "--rm", &tag, "sh", "-c", "echo cmd >> /tmp/log; cat /tmp/log"])
                .output()
                .unwrap();
            if !run.status.success() {
                return Err(String::from_utf8_lossy(&run.stderr).into_owned());
            }
            Ok(String::from_utf8_lossy(&run.stdout).trim().to_string())
        })();
        cleanup();
        let log = result.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(log, "feature\nimage\ncmd", "log: {log}");
        Ok(())
    }
}
