use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use toml::{Table, Value};

pub const CONFIG_FILE: &str = "config.toml";

/// Raw `config.toml` contents. Template and sandbox bodies are kept as
/// free-form tables so `extends` can deep-merge them; the merged result is
/// validated into [`SandboxProperties`] by `resolve_sandbox`.
#[derive(Debug, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub services: BTreeMap<String, Table>,
    #[serde(default, rename = "template")]
    pub templates: BTreeMap<String, Table>,
    #[serde(default, rename = "sandbox")]
    pub sandboxes: BTreeMap<String, Table>,
}

/// The exact set of properties a sandbox accepts: the devcontainer.json
/// schema plus the devsandbox extras (`extends`, `folder`, `services`,
/// `caches`, `persist-shell-history`). Unknown keys are a hard error (`deny_unknown_fields`);
/// valid-but-unimplemented ones are surfaced by [`Self::ignored`] so `run`
/// can warn before creating the container.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SandboxProperties {
    // --- devsandbox extras ---
    pub folder: Option<String>,
    pub services: Option<Vec<String>>,
    /// Package-manager caches to persist and share across the config root (e.g.
    /// `["pnpm", "cargo"]`). Each expands into a shared bind mount plus the env
    /// vars that point the tool at it. See [`known_cache`].
    pub caches: Option<Vec<String>>,
    /// When true, provision a per-instance `.zsh_history` on the host and
    /// bind-mount it, so shell history survives rebuilds without being shared
    /// between concurrent instances.
    #[serde(rename = "persist-shell-history")]
    pub persist_shell_history: Option<bool>,
    /// Branch created for a worktree instance. A pattern supporting the same
    /// `${…}` variables as `mounts` (notably `${instance}`); defaults to
    /// `sandbox/${instance}` when unset. A `run --branch` overrides it.
    #[serde(rename = "worktree-branch")]
    pub worktree_branch: Option<String>,
    /// Start point for a worktree instance's branch (any commit-ish, e.g.
    /// `origin/develop`). Unset: the remote's default branch is detected
    /// after a fetch. A `run --base` overrides it.
    #[serde(rename = "worktree-base")]
    pub worktree_base: Option<String>,
    /// Host shell snippets (relative to the config dir, `${…}` variables as in
    /// `mounts`) sourced by the container's interactive `~/.zshrc` and
    /// `~/.bashrc`. Each file is bind-mounted read-only via its parent dir and
    /// a guarded `. <file>` line is appended to both rc files once at create.
    /// Arrays concatenate under `extends`, so a template's aliases and a
    /// sandbox's additions are all sourced.
    #[serde(rename = "shell-rc")]
    pub shell_rc: Option<Vec<String>>,
    /// Extra VS Code workspace roots: container path -> host folder (relative
    /// to the config dir). Each entry is bind-mounted at its key and listed in
    /// the generated `.code-workspace` file after `workspaceFolder`.
    pub folders: Option<BTreeMap<String, String>>,

    // --- implemented devcontainer properties ---
    pub image: Option<String>,
    pub build: Option<Build>,
    pub workspace_folder: Option<String>,
    pub container_env: Option<BTreeMap<String, String>>,
    pub remote_env: Option<BTreeMap<String, String>>,
    pub initialize_command: Option<LifecycleCommand>,
    pub on_create_command: Option<LifecycleCommand>,
    pub update_content_command: Option<LifecycleCommand>,
    pub post_create_command: Option<LifecycleCommand>,
    pub post_start_command: Option<LifecycleCommand>,
    pub post_attach_command: Option<LifecycleCommand>,
    pub customizations: Option<Customizations>,
    /// devcontainer `features`: a map of feature ref (e.g.
    /// `"ghcr.io/devcontainers/features/node:1"`) to its options.
    pub features: Option<BTreeMap<String, FeatureOptions>>,

    // --- valid devcontainer properties, not implemented yet ---
    pub name: Option<Value>,
    pub forward_ports: Option<Value>,
    pub app_port: Option<Value>,
    pub ports_attributes: Option<Value>,
    pub other_ports_attributes: Option<Value>,
    pub run_args: Option<Value>,
    pub mounts: Option<Vec<Mount>>,
    pub workspace_mount: Option<Value>,
    pub override_feature_install_order: Option<Value>,
    pub container_user: Option<String>,
    pub remote_user: Option<String>,
    #[serde(rename = "updateRemoteUserUID")]
    pub update_remote_user_uid: Option<Value>,
    pub user_env_probe: Option<Value>,
    pub override_command: Option<Value>,
    pub shutdown_action: Option<Value>,
    pub init: Option<bool>,
    pub privileged: Option<Value>,
    pub cap_add: Option<Value>,
    pub security_opt: Option<Value>,
    pub host_requirements: Option<Value>,
    pub wait_for: Option<Value>,
    pub secrets: Option<Value>,
}

impl SandboxProperties {
    /// Names of valid devcontainer properties that are set but not
    /// implemented yet.
    pub fn ignored(&self) -> Vec<String> {
        let mut out = Vec::new();
        macro_rules! flag {
            ($($field:ident => $name:literal),* $(,)?) => {
                $(if self.$field.is_some() { out.push($name.to_string()); })*
            };
        }
        flag!(
            name => "name",
            forward_ports => "forwardPorts",
            app_port => "appPort",
            ports_attributes => "portsAttributes",
            other_ports_attributes => "otherPortsAttributes",
            run_args => "runArgs",
            workspace_mount => "workspaceMount",
            override_feature_install_order => "overrideFeatureInstallOrder",
            update_remote_user_uid => "updateRemoteUserUID",
            user_env_probe => "userEnvProbe",
            override_command => "overrideCommand",
            shutdown_action => "shutdownAction",
            privileged => "privileged",
            cap_add => "capAdd",
            security_opt => "securityOpt",
            host_requirements => "hostRequirements",
            wait_for => "waitFor",
            secrets => "secrets",
        );
        if let Some(customizations) = &self.customizations {
            if let Some(vscode) = &customizations.vscode {
                if vscode.settings.is_some() {
                    out.push("customizations.vscode.settings".into());
                }
                for key in vscode.other.keys() {
                    out.push(format!("customizations.vscode.{key}"));
                }
            }
            for tool in customizations.other.keys() {
                out.push(format!("customizations.{tool}"));
            }
        }
        out
    }

    pub fn vscode_extensions(&self) -> Option<&[String]> {
        self.customizations
            .as_ref()?
            .vscode
            .as_ref()?
            .extensions
            .as_deref()
    }
}

/// Options for a single `features` entry: an options table, a bare string
/// (devcontainer shorthand for the `version` option), or a bool (`true` = no
/// options).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum FeatureOptions {
    Options(BTreeMap<String, Value>),
    Version(String),
    Enabled(bool),
}

/// A `mounts` entry: the docker `--mount` shorthand string
/// (`source=…,target=…,type=bind`) or the devcontainer object form.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Mount {
    Shorthand(String),
    Object(MountObject),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MountObject {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub source: Option<String>,
    pub target: String,
    pub readonly: Option<bool>,
}

/// Values for the devcontainer `${…}` variables devsandbox substitutes in mount
/// sources. `localEnv:*` is read from the process environment separately.
pub struct MountContext<'a> {
    /// Directory holding `config.toml` (`${configDir}`).
    pub config_dir: &'a str,
    /// Host path of the project checkout (`${localWorkspaceFolder}`).
    pub workspace_folder: &'a str,
    /// Its basename (`${localWorkspaceFolderBasename}`).
    pub workspace_folder_basename: &'a str,
    /// `<configDir>/shared-volumes` (`${sharedVolumes}`), the host-backed
    /// persistent-state root.
    pub shared_volumes: &'a str,
    /// Persistent id of the instance being run (`${instance}`), e.g. `web` /
    /// `web-2` — anchors per-instance state under `${sharedVolumes}`. This is
    /// `Instance::instance_id` (the name at creation, unique forever), not the
    /// current display name, so renames never move `${instance}`-anchored paths.
    pub instance: &'a str,
}

/// A mount with variables substituted and defaults applied, ready to hand to
/// `docker run --mount`.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedMount {
    pub kind: String,
    pub source: Option<String>,
    pub target: String,
    pub readonly: bool,
}

impl Mount {
    /// Apply variable substitution and defaults. `bind` mounts require a source.
    pub fn resolve(&self, ctx: &MountContext) -> Result<ResolvedMount> {
        let (kind, source, target, readonly) = match self {
            Mount::Shorthand(s) => parse_shorthand(s)?,
            Mount::Object(o) => (
                o.kind.clone(),
                o.source.clone(),
                o.target.clone(),
                o.readonly.unwrap_or(false),
            ),
        };
        let kind = kind.unwrap_or_else(|| "bind".to_string());
        let source = source.map(|s| substitute(&s, ctx));
        let target = substitute(&target, ctx);
        if kind == "bind" && source.is_none() {
            bail!("bind mount to `{target}` is missing a source");
        }
        Ok(ResolvedMount {
            kind,
            source,
            target,
            readonly,
        })
    }
}

impl ResolvedMount {
    /// The value for a `docker run --mount <value>` flag.
    pub fn to_arg(&self) -> String {
        let mut arg = format!("type={}", self.kind);
        if let Some(source) = &self.source {
            arg.push_str(&format!(",source={source}"));
        }
        arg.push_str(&format!(",target={}", self.target));
        if self.readonly {
            arg.push_str(",readonly");
        }
        arg
    }
}

pub(crate) type MountParts = (Option<String>, Option<String>, String, bool);

/// Parse a docker `--mount` shorthand (`key=value,…`) into
/// (type, source, target, readonly).
pub(crate) fn parse_shorthand(spec: &str) -> Result<MountParts> {
    let (mut kind, mut source, mut target, mut readonly) = (None, None, None, false);
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (key, value) = match part.split_once('=') {
            Some((k, v)) => (k.trim(), Some(v.trim().to_string())),
            None => (part, None),
        };
        match key {
            "type" => kind = value,
            "source" | "src" => source = value,
            "target" | "destination" | "dst" => target = value,
            "readonly" | "ro" => readonly = value.as_deref() != Some("false"),
            "consistency" | "bind-propagation" => {} // docker hints, no-op here
            other => bail!("unknown mount field `{other}` in `{spec}`"),
        }
    }
    let target = target.with_context(|| format!("mount `{spec}` is missing a target"))?;
    Ok((kind, source, target, readonly))
}

/// Package-manager caches supported by the `caches` field, in a stable order.
pub const SUPPORTED_CACHES: &[&str] = &["pnpm", "cargo", "npm", "yarn", "go", "pip"];

/// Resolve a cache name to `(source subdirectory under shared-volumes, mount
/// target in the container, env vars that must point at the target)`. Returns
/// `None` for an unknown name.
pub fn known_cache(name: &str) -> Option<(&'static str, &'static str, &'static [&'static str])> {
    Some(match name {
        "pnpm" => (
            "pnpm-store",
            "/root/.pnpm-store",
            &["npm_config_store_dir", "pnpm_config_store_dir"],
        ),
        "cargo" => ("cargo", "/root/.cargo", &["CARGO_HOME"]),
        "npm" => ("npm", "/root/.npm", &["npm_config_cache"]),
        "yarn" => ("yarn", "/root/.yarn-cache", &["YARN_CACHE_FOLDER"]),
        "go" => ("go-mod", "/root/go/pkg/mod", &["GOMODCACHE"]),
        "pip" => ("pip", "/root/.cache/pip", &["PIP_CACHE_DIR"]),
        _ => return None,
    })
}

/// Replace the devcontainer `${…}` variables devsandbox supports; unknown
/// expressions are left verbatim.
pub fn substitute(input: &str, ctx: &MountContext) -> String {
    let mut out = String::new();
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                let expr = &after[..end];
                match resolve_var(expr, ctx) {
                    Some(value) => out.push_str(&value),
                    None => out.push_str(&rest[start..start + 2 + end + 1]),
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

fn resolve_var(expr: &str, ctx: &MountContext) -> Option<String> {
    if let Some(var) = expr.strip_prefix("localEnv:") {
        // devcontainer semantics: an unset host variable expands to empty.
        return Some(std::env::var(var).unwrap_or_default());
    }
    match expr {
        "configDir" => Some(ctx.config_dir.to_string()),
        "localWorkspaceFolder" => Some(ctx.workspace_folder.to_string()),
        "localWorkspaceFolderBasename" => Some(ctx.workspace_folder_basename.to_string()),
        "sharedVolumes" => Some(ctx.shared_volumes.to_string()),
        "instance" => Some(ctx.instance.to_string()),
        _ => None,
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Build {
    pub dockerfile: Option<String>,
    pub context: Option<String>,
    pub args: Option<BTreeMap<String, String>>,
    pub target: Option<String>,
    pub cache_from: Option<StringOrList>,
    pub options: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct Customizations {
    pub vscode: Option<VscodeCustomizations>,
    // Customizations for other tools; nothing to validate.
    #[serde(flatten)]
    pub other: Table,
}

#[derive(Debug, Deserialize)]
pub struct VscodeCustomizations {
    pub extensions: Option<Vec<String>>,
    pub settings: Option<Value>,
    #[serde(flatten)]
    pub other: Table,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum StringOrList {
    One(String),
    Many(Vec<String>),
}

impl StringOrList {
    pub fn to_vec(&self) -> Vec<&str> {
        match self {
            Self::One(s) => vec![s],
            Self::Many(v) => v.iter().map(String::as_str).collect(),
        }
    }
}

/// devcontainer lifecycle command: a shell string, an argv array, or a named
/// map of either.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum LifecycleCommand {
    Simple(SimpleCommand),
    Parallel(BTreeMap<String, SimpleCommand>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum SimpleCommand {
    Shell(String),
    Args(Vec<String>),
}

impl SimpleCommand {
    fn argv(&self) -> Vec<String> {
        match self {
            Self::Shell(s) => vec!["sh".into(), "-c".into(), s.clone()],
            Self::Args(a) => a.clone(),
        }
    }
}

impl LifecycleCommand {
    /// Argv lists to run, in order. The map form is documented as parallel;
    /// we run its entries sequentially for now.
    pub fn commands(&self) -> Vec<Vec<String>> {
        match self {
            Self::Simple(c) => vec![c.argv()],
            Self::Parallel(map) => map.values().map(SimpleCommand::argv).collect(),
        }
    }
}

/// A sandbox with its `extends` template merged in and validated.
#[derive(Debug)]
pub struct ResolvedSandbox {
    pub name: String,
    pub properties: SandboxProperties,
    /// Stable hash of the merged config, used to detect drift on reuse.
    pub config_hash: String,
}

impl ResolvedSandbox {
    pub fn folder(&self) -> Option<&str> {
        self.properties.folder.as_deref()
    }

    /// Short description of what the sandbox is built from.
    pub fn source(&self) -> String {
        if let Some(image) = &self.properties.image {
            return format!("image {image}");
        }
        if let Some(dockerfile) = self
            .properties
            .build
            .as_ref()
            .and_then(|b| b.dockerfile.as_deref())
        {
            return format!("dockerfile {dockerfile}");
        }
        "?".into()
    }
}

/// How a service is shared. `isolated` (the default) gives every sandbox
/// instance its own container; `global` is a single container shared across the
/// whole config root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceScope {
    #[default]
    Isolated,
    Global,
}

/// A service definition (`[services.<name>]`). Backed by an `image` or a
/// `build`. Free-form beyond the fields devsandbox acts on; unknown keys are
/// rejected so typos surface.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Service {
    #[serde(default)]
    pub scope: ServiceScope,
    pub image: Option<String>,
    pub build: Option<Build>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub ports: Vec<String>,
    #[serde(default)]
    pub command: Option<StringOrList>,
}

/// A validated service with a stable hash of its definition.
#[derive(Debug)]
pub struct ResolvedService {
    pub name: String,
    pub spec: Service,
    pub config_hash: String,
}

impl Config {
    pub fn parse(contents: &str) -> Result<Self> {
        toml::from_str(contents).context("invalid config")
    }

    pub fn resolve_service(&self, name: &str) -> Result<ResolvedService> {
        let table = self
            .services
            .get(name)
            .with_context(|| format!("unknown service `{name}`"))?;
        let config_hash = config_hash(table);
        let spec: Service = table
            .clone()
            .try_into()
            .map_err(|e| anyhow!("service `{name}`: {e}"))?;
        match (&spec.image, &spec.build) {
            (Some(_), Some(_)) => bail!("service `{name}`: set only one of `image` or `build`"),
            (None, None) => bail!("service `{name}`: needs an `image` or a `build`"),
            _ => {}
        }
        Ok(ResolvedService {
            name: name.to_string(),
            spec,
            config_hash,
        })
    }

    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join(CONFIG_FILE);
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        Self::parse(&contents).with_context(|| format!("in {}", path.display()))
    }

    pub fn resolve_sandbox(&self, name: &str) -> Result<ResolvedSandbox> {
        let sandbox = self
            .sandboxes
            .get(name)
            .with_context(|| format!("unknown sandbox `{name}`"))?;

        let mut properties = self.resolve_extends(sandbox, &mut Vec::new())?;
        properties.remove("extends");

        let config_hash = config_hash(&properties);
        let properties: SandboxProperties = properties
            .try_into()
            .map_err(|e| anyhow!("sandbox `{name}`: {e}"))?;

        Ok(ResolvedSandbox {
            name: name.to_string(),
            properties,
            config_hash,
        })
    }

    /// The raw `[sandbox.<name>]` table exactly as written (including `extends`),
    /// for the config explorer's "original" view.
    pub fn sandbox_table(&self, name: &str) -> Result<&Table> {
        self.sandboxes
            .get(name)
            .with_context(|| format!("unknown sandbox `{name}`"))
    }

    /// The merged-but-untyped sandbox table: `extends` resolved and dropped, so
    /// it round-trips to TOML for the config explorer's "resolved" view. Unlike
    /// [`Self::resolve_sandbox`] this skips typed validation. Returns the merged
    /// table plus its config hash.
    pub fn resolved_table(&self, name: &str) -> Result<(Table, String)> {
        let sandbox = self.sandbox_table(name)?;
        let mut properties = self.resolve_extends(sandbox, &mut Vec::new())?;
        properties.remove("extends");
        let hash = config_hash(&properties);
        Ok((properties, hash))
    }

    /// Resolve a table's `extends` into a fully-merged table: referenced
    /// templates first (left-to-right, each with its own `extends` resolved
    /// recursively), then the table's own body on top. `stack` tracks the active
    /// resolution path for cycle detection.
    fn resolve_extends(&self, table: &Table, stack: &mut Vec<String>) -> Result<Table> {
        let names = match table.get("extends") {
            None => Vec::new(),
            Some(Value::String(name)) => vec![name.clone()],
            Some(Value::Array(items)) => items
                .iter()
                .map(|v| {
                    v.as_str().map(str::to_string).ok_or_else(|| {
                        anyhow!("`extends` array must contain template names (strings)")
                    })
                })
                .collect::<Result<Vec<_>>>()?,
            Some(other) => bail!(
                "`extends` must be a template name or an array of names, got {}",
                other.type_str()
            ),
        };

        let mut merged = Table::new();
        for name in names {
            if stack.contains(&name) {
                bail!("cyclic `extends`: {} -> {name}", stack.join(" -> "));
            }
            let template = self
                .templates
                .get(&name)
                .with_context(|| format!("extends unknown template `{name}`"))?;
            stack.push(name);
            let resolved = self.resolve_extends(template, stack)?;
            stack.pop();
            merged = deep_merge(merged, resolved);
        }

        let mut body = table.clone();
        body.remove("extends");
        Ok(deep_merge(merged, body))
    }

    pub fn resolve_all(&self) -> Result<Vec<ResolvedSandbox>> {
        self.sandboxes
            .keys()
            .map(|name| self.resolve_sandbox(name))
            .collect()
    }
}

/// Stable FNV-1a hash of arbitrary text, rendered as 16 hex chars.
pub fn short_hash(text: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// Short hash of a `build`'s dockerfile *contents*, or `""` when there is no
/// build, no `dockerfile` key, or the file can't be read.
///
/// Hashing the bytes (not the path in the merged config, which `config_hash`
/// already covers) is what lets a dockerfile edit register as drift: the path
/// is unchanged, only the contents moved. Limitation: only the dockerfile file
/// itself is hashed — the build context and devcontainer features are not, so
/// edits to those go undetected.
pub fn build_hash(dir: &Path, build: Option<&Build>) -> String {
    let Some(dockerfile) = build.and_then(|b| b.dockerfile.as_deref()) else {
        return String::new();
    };
    match std::fs::read(dir.join(dockerfile)) {
        Ok(bytes) => short_hash(&String::from_utf8_lossy(&bytes)),
        Err(_) => String::new(),
    }
}

/// Stable hash of a merged config table. `Table` is a `BTreeMap`, so its TOML
/// serialization is key-sorted and deterministic across runs.
fn config_hash(table: &Table) -> String {
    short_hash(&toml::to_string(table).unwrap_or_default())
}

/// Deep merge: nested tables merge recursively, arrays concatenate
/// (base first, then `over`), scalars are replaced by `over`. Concatenating
/// arrays lets a sandbox add to a template's `mounts`/`extensions` without
/// restating them (devcontainer merge semantics).
fn deep_merge(base: Table, over: Table) -> Table {
    let mut merged = base;
    for (key, value) in over {
        match (merged.remove(&key), value) {
            (Some(Value::Table(base_table)), Value::Table(over_table)) => {
                merged.insert(key, Value::Table(deep_merge(base_table, over_table)));
            }
            (Some(Value::Array(mut base_arr)), Value::Array(over_arr)) => {
                base_arr.extend(over_arr);
                merged.insert(key, Value::Array(base_arr));
            }
            (_, value) => {
                merged.insert(key, value);
            }
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
[services.database]
image = "postgres"

[template.base-sandbox]
caches = ["pnpm"]
build.args = { A = "1", B = "2" }

[sandbox.repository-1]
extends = "base-sandbox"
folder = "../repository-1"
image = "node-22"

[sandbox.repository-2]
extends = "base-sandbox"
folder = "../repository-2"
services = ["database"]
build.dockerfile = "./Dockerfile"
build.args = { B = "3" }
"#;

    #[test]
    fn parses_example() {
        let config = Config::parse(EXAMPLE).unwrap();
        assert_eq!(config.services.len(), 1);
        assert_eq!(config.templates.len(), 1);
        assert_eq!(config.sandboxes.len(), 2);
    }

    #[test]
    fn resolve_merges_template() {
        let config = Config::parse(EXAMPLE).unwrap();
        let sandbox = config.resolve_sandbox("repository-1").unwrap();
        assert_eq!(sandbox.folder(), Some("../repository-1"));
        assert_eq!(sandbox.source(), "image node-22");
        assert_eq!(sandbox.properties.caches.as_deref(), Some(&["pnpm".to_string()][..]));
    }

    #[test]
    fn resolve_deep_merges_nested_tables() {
        let config = Config::parse(EXAMPLE).unwrap();
        let sandbox = config.resolve_sandbox("repository-2").unwrap();
        assert_eq!(sandbox.source(), "dockerfile ./Dockerfile");
        let args = sandbox.properties.build.as_ref().unwrap().args.as_ref().unwrap();
        // A comes from the template, B is overridden by the sandbox.
        assert_eq!(args["A"], "1");
        assert_eq!(args["B"], "3");
    }

    #[test]
    fn folders_parses_and_merges_through_extends() {
        let config = Config::parse(
            r#"
[template.base]
folders = { "/workspaces/.shared" = "../.shared" }

[sandbox.app]
extends = "base"
folder = "../app"
image = "img"
folders = { "/workspaces/docs" = "../docs" }
"#,
        )
        .unwrap();
        let sandbox = config.resolve_sandbox("app").unwrap();
        let folders = sandbox.properties.folders.as_ref().unwrap();
        assert_eq!(folders["/workspaces/.shared"], "../.shared");
        assert_eq!(folders["/workspaces/docs"], "../docs");
    }

    #[test]
    fn config_hash_is_stable_and_sensitive() {
        let config = Config::parse(EXAMPLE).unwrap();
        let a = config.resolve_sandbox("repository-1").unwrap().config_hash;
        let b = config.resolve_sandbox("repository-1").unwrap().config_hash;
        assert_eq!(a, b, "same config must hash identically");
        let c = config.resolve_sandbox("repository-2").unwrap().config_hash;
        assert_ne!(a, c, "different configs must hash differently");
    }

    #[test]
    fn build_hash_none_and_missing_are_empty() {
        let dir = std::env::temp_dir();
        // No build block at all.
        assert_eq!(build_hash(&dir, None), "");
        // Build with no dockerfile key.
        let no_dockerfile = Build::default();
        assert_eq!(build_hash(&dir, Some(&no_dockerfile)), "");
        // Dockerfile key pointing at a nonexistent file.
        let missing = Build {
            dockerfile: Some("does-not-exist-xyz.Dockerfile".into()),
            ..Build::default()
        };
        assert_eq!(build_hash(&dir, Some(&missing)), "");
    }

    #[test]
    fn build_hash_tracks_dockerfile_contents() {
        let dir = std::env::temp_dir().join(format!("devsandbox-bh-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Dockerfile");
        let build = Build {
            dockerfile: Some("Dockerfile".into()),
            ..Build::default()
        };

        std::fs::write(&path, "FROM alpine\n").unwrap();
        let first = build_hash(&dir, Some(&build));
        assert!(!first.is_empty());
        // Identical contents hash identically.
        assert_eq!(first, build_hash(&dir, Some(&build)));
        // A content change moves the hash even though the path is unchanged.
        std::fs::write(&path, "FROM alpine\nRUN echo hi\n").unwrap();
        assert_ne!(first, build_hash(&dir, Some(&build)));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolved_table_merges_and_drops_extends() {
        let config = Config::parse(EXAMPLE).unwrap();
        let (table, hash) = config.resolved_table("repository-2").unwrap();
        // `extends` is gone but the merged body is present.
        assert!(!table.contains_key("extends"));
        assert_eq!(table["folder"].as_str(), Some("../repository-2"));
        assert_eq!(table["caches"].as_array().unwrap().len(), 1); // from template
        // Nested build.args deep-merged: A from template, B overridden.
        let args = table["build"].as_table().unwrap()["args"].as_table().unwrap();
        assert_eq!(args["A"].as_str(), Some("1"));
        assert_eq!(args["B"].as_str(), Some("3"));
        // Hash matches the typed resolver's for the same sandbox.
        assert_eq!(hash, config.resolve_sandbox("repository-2").unwrap().config_hash);
    }

    #[test]
    fn resolved_table_unknown_sandbox_errors() {
        let config = Config::parse(EXAMPLE).unwrap();
        assert!(config.resolved_table("nope").is_err());
    }

    #[test]
    fn sandbox_without_extends() {
        let config = Config::parse("[sandbox.solo]\nimage = \"alpine\"").unwrap();
        let sandbox = config.resolve_sandbox("solo").unwrap();
        assert_eq!(sandbox.source(), "image alpine");
    }

    #[test]
    fn unknown_template_errors() {
        let config = Config::parse("[sandbox.bad]\nextends = \"nope\"").unwrap();
        let err = config.resolve_sandbox("bad").unwrap_err().to_string();
        assert!(err.contains("unknown template `nope`"), "{err}");
    }

    #[test]
    fn non_string_extends_errors() {
        let config = Config::parse("[sandbox.bad]\nextends = 3").unwrap();
        assert!(config.resolve_sandbox("bad").is_err());
    }

    #[test]
    fn unknown_property_errors() {
        let config = Config::parse("[sandbox.bad]\nimag = \"alpine\"").unwrap();
        let err = config.resolve_sandbox("bad").unwrap_err().to_string();
        assert!(err.contains("imag"), "{err}");
    }

    #[test]
    fn unknown_property_from_template_errors() {
        let config = Config::parse(
            "[template.t]\nfowardPorts = [3000]\n[sandbox.bad]\nextends = \"t\"",
        )
        .unwrap();
        let err = config.resolve_sandbox("bad").unwrap_err().to_string();
        assert!(err.contains("fowardPorts"), "{err}");
    }

    #[test]
    fn unimplemented_properties_are_flagged_ignored() {
        let config = Config::parse(
            r#"
[sandbox.s]
image = "alpine"
forwardPorts = [3000]
runArgs = ["--gpus", "all"]
customizations.vscode.settings = { "editor.formatOnSave" = true }
customizations.jetbrains.plugins = ["x"]
"#,
        )
        .unwrap();
        let sandbox = config.resolve_sandbox("s").unwrap();
        assert_eq!(
            sandbox.properties.ignored(),
            vec![
                "forwardPorts",
                "runArgs",
                "customizations.vscode.settings",
                "customizations.jetbrains",
            ]
        );
    }

    #[test]
    fn implemented_properties_are_not_ignored() {
        let config = Config::parse(
            r#"
[sandbox.s]
image = "alpine"
containerEnv = { FOO = "bar" }
postCreateCommand = "npm install"
customizations.vscode.extensions = ["rust-lang.rust-analyzer"]
"#,
        )
        .unwrap();
        let sandbox = config.resolve_sandbox("s").unwrap();
        assert!(sandbox.properties.ignored().is_empty());
        assert_eq!(
            sandbox.properties.vscode_extensions(),
            Some(&["rust-lang.rust-analyzer".to_string()][..])
        );
    }

    #[test]
    fn feature_option_forms_parse() {
        let config = Config::parse(
            r#"
[sandbox.s]
image = "alpine"

[sandbox.s.features]
"ghcr.io/devcontainers/features/node:1" = { version = "lts" }
"ghcr.io/devcontainers/features/go:1" = "1.22"
"ghcr.io/devcontainers/features/git:1" = true
"#,
        )
        .unwrap();
        let features = config.resolve_sandbox("s").unwrap().properties.features.unwrap();
        match &features["ghcr.io/devcontainers/features/node:1"] {
            FeatureOptions::Options(opts) => {
                assert_eq!(opts["version"].as_str(), Some("lts"));
            }
            other => panic!("expected options table, got {other:?}"),
        }
        match &features["ghcr.io/devcontainers/features/go:1"] {
            FeatureOptions::Version(v) => assert_eq!(v, "1.22"),
            other => panic!("expected version string, got {other:?}"),
        }
        match &features["ghcr.io/devcontainers/features/git:1"] {
            FeatureOptions::Enabled(b) => assert!(b),
            other => panic!("expected bool, got {other:?}"),
        }
    }

    #[test]
    fn features_are_not_ignored() {
        let config = Config::parse(
            r#"
[sandbox.s]
image = "alpine"
features = { "ghcr.io/x/y:1" = true }
"#,
        )
        .unwrap();
        let sandbox = config.resolve_sandbox("s").unwrap();
        assert!(sandbox.properties.ignored().is_empty());
        assert!(!SandboxProperties::default().ignored().contains(&"features".to_string()));
    }

    #[test]
    fn features_deep_merge_across_template() {
        let config = Config::parse(
            r#"
[template.feat-base]
[template.feat-base.features]
"ghcr.io/devcontainers/features/common-utils:2" = true
"ghcr.io/devcontainers/features/node:1" = { version = "18" }

[sandbox.s]
extends = "feat-base"
image = "alpine"
[sandbox.s.features]
"ghcr.io/devcontainers/features/go:1" = "1.22"
"ghcr.io/devcontainers/features/node:1" = { version = "lts" }
"#,
        )
        .unwrap();
        let features = config.resolve_sandbox("s").unwrap().properties.features.unwrap();
        // common-utils comes from the template.
        assert!(matches!(
            features["ghcr.io/devcontainers/features/common-utils:2"],
            FeatureOptions::Enabled(true)
        ));
        // go is added by the sandbox.
        assert!(matches!(
            &features["ghcr.io/devcontainers/features/go:1"],
            FeatureOptions::Version(v) if v == "1.22"
        ));
        // node is overridden by the sandbox.
        match &features["ghcr.io/devcontainers/features/node:1"] {
            FeatureOptions::Options(opts) => assert_eq!(opts["version"].as_str(), Some("lts")),
            other => panic!("expected options table, got {other:?}"),
        }
    }

    #[test]
    fn lifecycle_command_forms() {
        let config = Config::parse(
            r#"
[sandbox.s]
image = "alpine"
postCreateCommand = "npm install"
postStartCommand = ["node", "server.js"]
onCreateCommand = { b = "make", a = ["cargo", "build"] }
"#,
        )
        .unwrap();
        let props = config.resolve_sandbox("s").unwrap().properties;
        assert_eq!(
            props.post_create_command.unwrap().commands(),
            vec![vec!["sh", "-c", "npm install"]]
        );
        assert_eq!(
            props.post_start_command.unwrap().commands(),
            vec![vec!["node", "server.js"]]
        );
        assert_eq!(
            props.on_create_command.unwrap().commands(),
            vec![vec!["cargo", "build"], vec!["sh", "-c", "make"]]
        );
    }

    fn ctx() -> MountContext<'static> {
        MountContext {
            config_dir: "/cfg",
            workspace_folder: "/home/u/repo",
            workspace_folder_basename: "repo",
            shared_volumes: "/cfg/shared-volumes",
            instance: "repo-2",
        }
    }

    #[test]
    fn substitutes_shared_volumes_and_instance() {
        let m = Mount::Shorthand(
            "source=${sharedVolumes}/zidane/${instance},target=/root/.zidane,type=bind".into(),
        );
        let resolved = m.resolve(&ctx()).unwrap();
        assert_eq!(resolved.source.as_deref(), Some("/cfg/shared-volumes/zidane/repo-2"));
        assert_eq!(resolved.target, "/root/.zidane");
    }

    #[test]
    fn mount_shorthand_resolves_and_substitutes() {
        let config = Config::parse(
            r#"
[sandbox.s]
image = "alpine"
mounts = ["source=${configDir}/shared-volumes/cargo,target=/root/.cargo,type=bind"]
"#,
        )
        .unwrap();
        let props = config.resolve_sandbox("s").unwrap().properties;
        assert!(props.ignored().is_empty());
        let resolved = props.mounts.as_ref().unwrap()[0].resolve(&ctx()).unwrap();
        assert_eq!(resolved.source.as_deref(), Some("/cfg/shared-volumes/cargo"));
        assert_eq!(resolved.target, "/root/.cargo");
        assert_eq!(resolved.kind, "bind");
        assert_eq!(
            resolved.to_arg(),
            "type=bind,source=/cfg/shared-volumes/cargo,target=/root/.cargo"
        );
    }

    #[test]
    fn mount_object_form_with_readonly() {
        let config = Config::parse(
            r#"
[sandbox.s]
image = "alpine"
mounts = [{ source = "${localWorkspaceFolder}/x", target = "/x", readonly = true }]
"#,
        )
        .unwrap();
        let props = config.resolve_sandbox("s").unwrap().properties;
        let resolved = props.mounts.as_ref().unwrap()[0].resolve(&ctx()).unwrap();
        assert_eq!(resolved.source.as_deref(), Some("/home/u/repo/x"));
        assert!(resolved.readonly);
        assert_eq!(resolved.to_arg(), "type=bind,source=/home/u/repo/x,target=/x,readonly");
    }

    #[test]
    fn mount_localenv_expands_from_host() {
        // PATH is reliably set; verify substitution reads the host env.
        let expected = format!("type=bind,source={}/c,target=/c", std::env::var("PATH").unwrap());
        let config = Config::parse(
            "[sandbox.s]\nimage = \"alpine\"\nmounts = [\"source=${localEnv:PATH}/c,target=/c\"]",
        )
        .unwrap();
        let props = config.resolve_sandbox("s").unwrap().properties;
        let resolved = props.mounts.as_ref().unwrap()[0].resolve(&ctx()).unwrap();
        assert_eq!(resolved.to_arg(), expected);
    }

    #[test]
    fn mount_bind_without_source_errors() {
        let config =
            Config::parse("[sandbox.s]\nimage = \"alpine\"\nmounts = [\"target=/x,type=bind\"]")
                .unwrap();
        let props = config.resolve_sandbox("s").unwrap().properties;
        assert!(props.mounts.as_ref().unwrap()[0].resolve(&ctx()).is_err());
    }

    #[test]
    fn mount_unknown_variable_left_verbatim() {
        assert_eq!(substitute("${nope}/x", &ctx()), "${nope}/x");
        assert_eq!(substitute("a/${configDir}/b", &ctx()), "a//cfg/b");
    }

    #[test]
    fn arrays_concatenate_on_merge() {
        let config = Config::parse(
            r#"
[template.base]
mounts = ["source=/a,target=/a,type=bind"]
customizations.vscode.extensions = ["a.one"]

[sandbox.s]
extends = "base"
image = "alpine"
mounts = ["source=/b,target=/b,type=bind"]
customizations.vscode.extensions = ["b.two"]
"#,
        )
        .unwrap();
        let props = config.resolve_sandbox("s").unwrap().properties;
        assert_eq!(props.mounts.as_ref().unwrap().len(), 2);
        assert_eq!(
            props.vscode_extensions(),
            Some(&["a.one".to_string(), "b.two".to_string()][..])
        );
    }

    #[test]
    fn persist_shell_history_parses_and_merges() {
        let config = Config::parse(
            r#"
[template.base]
persist-shell-history = true

[sandbox.s]
extends = "base"
image = "alpine"
"#,
        )
        .unwrap();
        let props = config.resolve_sandbox("s").unwrap().properties;
        assert_eq!(props.persist_shell_history, Some(true));
        assert!(props.ignored().is_empty());
    }

    #[test]
    fn worktree_branch_parses_and_is_not_ignored() {
        let config = Config::parse(
            r#"
[sandbox.s]
image = "alpine"
worktree-branch = "feat/${instance}"
"#,
        )
        .unwrap();
        let props = config.resolve_sandbox("s").unwrap().properties;
        assert_eq!(props.worktree_branch.as_deref(), Some("feat/${instance}"));
        assert!(props.ignored().is_empty());
    }

    #[test]
    fn worktree_branch_merges_through_template() {
        let config = Config::parse(
            r#"
[template.base]
worktree-branch = "team/${instance}"

[sandbox.s]
extends = "base"
image = "alpine"
"#,
        )
        .unwrap();
        let props = config.resolve_sandbox("s").unwrap().properties;
        assert_eq!(props.worktree_branch.as_deref(), Some("team/${instance}"));
    }

    #[test]
    fn worktree_base_parses_and_is_not_ignored() {
        let config = Config::parse(
            r#"
[sandbox.s]
image = "alpine"
worktree-base = "origin/develop"
"#,
        )
        .unwrap();
        let props = config.resolve_sandbox("s").unwrap().properties;
        assert_eq!(props.worktree_base.as_deref(), Some("origin/develop"));
        assert!(props.ignored().is_empty());
    }

    #[test]
    fn shell_rc_concatenates_through_template() {
        let config = Config::parse(
            r#"
[template.base]
shell-rc = ["${configDir}/shell/aliases.sh"]

[sandbox.s]
extends = "base"
image = "alpine"
shell-rc = ["./env.sh"]
"#,
        )
        .unwrap();
        let props = config.resolve_sandbox("s").unwrap().properties;
        assert_eq!(
            props.shell_rc.as_deref(),
            Some(&["${configDir}/shell/aliases.sh".to_string(), "./env.sh".to_string()][..])
        );
        assert!(props.ignored().is_empty());
    }

    #[test]
    fn extends_list_merges_left_to_right() {
        let config = Config::parse(
            r#"
[template.node]
image = "node"
containerEnv = { A = "1" }
customizations.vscode.extensions = ["node.ext"]

[template.rust]
containerEnv = { B = "2" }
customizations.vscode.extensions = ["rust.ext"]

[sandbox.s]
extends = ["node", "rust"]
folder = "."
"#,
        )
        .unwrap();
        let props = config.resolve_sandbox("s").unwrap().properties;
        assert_eq!(props.image.as_deref(), Some("node"));
        let env = props.container_env.as_ref().unwrap();
        assert_eq!(env["A"], "1");
        assert_eq!(env["B"], "2");
        assert_eq!(
            props.vscode_extensions(),
            Some(&["node.ext".to_string(), "rust.ext".to_string()][..])
        );
    }

    #[test]
    fn extends_resolves_recursively() {
        let config = Config::parse(
            r#"
[template.base]
image = "alpine"
containerEnv = { BASE = "1" }

[template.mid]
extends = "base"
containerEnv = { MID = "2" }

[sandbox.s]
extends = "mid"
folder = "."
"#,
        )
        .unwrap();
        let props = config.resolve_sandbox("s").unwrap().properties;
        assert_eq!(props.image.as_deref(), Some("alpine"));
        let env = props.container_env.as_ref().unwrap();
        assert_eq!(env["BASE"], "1");
        assert_eq!(env["MID"], "2");
    }

    #[test]
    fn extends_cycle_errors() {
        let config = Config::parse(
            r#"
[template.a]
extends = "b"
[template.b]
extends = "a"
[sandbox.s]
extends = "a"
"#,
        )
        .unwrap();
        let err = config.resolve_sandbox("s").unwrap_err().to_string();
        assert!(err.contains("cyclic"), "{err}");
    }

    #[test]
    fn caches_expand_to_known_specs() {
        let (source, target, env) = known_cache("cargo").unwrap();
        assert_eq!(source, "cargo");
        assert_eq!(target, "/root/.cargo");
        assert_eq!(env, &["CARGO_HOME"]);
        assert!(known_cache("nope").is_none());
    }

    #[test]
    fn service_scope_defaults_to_isolated() {
        let config = Config::parse("[services.db]\nimage = \"postgres:16\"").unwrap();
        let svc = config.resolve_service("db").unwrap();
        assert_eq!(svc.spec.scope, ServiceScope::Isolated);
    }

    #[test]
    fn service_scope_global_parses() {
        let config =
            Config::parse("[services.db]\nimage = \"postgres:16\"\nscope = \"global\"").unwrap();
        let svc = config.resolve_service("db").unwrap();
        assert_eq!(svc.spec.scope, ServiceScope::Global);
    }

    #[test]
    fn service_requires_image_xor_build() {
        let neither = Config::parse("[services.db]\nenv = { X = \"1\" }").unwrap();
        assert!(neither.resolve_service("db").unwrap_err().to_string().contains("image"));
        let both = Config::parse(
            "[services.db]\nimage = \"postgres:16\"\nbuild = { dockerfile = \"Dockerfile\" }",
        )
        .unwrap();
        assert!(both.resolve_service("db").unwrap_err().to_string().contains("only one"));
    }
}
