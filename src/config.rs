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
    // Consumed by the runtime layer in a later milestone.
    #[allow(dead_code)]
    #[serde(default)]
    pub services: BTreeMap<String, Table>,
    #[serde(default, rename = "template")]
    pub templates: BTreeMap<String, Table>,
    #[serde(default, rename = "sandbox")]
    pub sandboxes: BTreeMap<String, Table>,
}

/// The exact set of properties a sandbox accepts: the devcontainer.json
/// schema plus the devsandbox extras (`extends`, `folder`, `services`,
/// `cache-folder`). Unknown keys are a hard error (`deny_unknown_fields`);
/// valid-but-unimplemented ones are surfaced by [`Self::ignored`] so `run`
/// can warn before creating the container.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SandboxProperties {
    // --- devsandbox extras ---
    pub folder: Option<String>,
    // Consumed by the runtime layer in a later milestone.
    #[allow(dead_code)]
    pub services: Option<Vec<String>>,
    #[allow(dead_code)]
    #[serde(rename = "cache-folder")]
    pub cache_folder: Option<String>,

    // --- implemented devcontainer properties ---
    pub image: Option<String>,
    pub build: Option<Build>,
    pub docker_compose_file: Option<StringOrList>,
    pub service: Option<String>,
    pub run_services: Option<Vec<String>>,
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

    // --- valid devcontainer properties, not implemented yet ---
    pub name: Option<Value>,
    pub forward_ports: Option<Value>,
    pub app_port: Option<Value>,
    pub ports_attributes: Option<Value>,
    pub other_ports_attributes: Option<Value>,
    pub run_args: Option<Value>,
    pub mounts: Option<Value>,
    pub workspace_mount: Option<Value>,
    pub features: Option<Value>,
    pub override_feature_install_order: Option<Value>,
    pub container_user: Option<Value>,
    pub remote_user: Option<Value>,
    #[serde(rename = "updateRemoteUserUID")]
    pub update_remote_user_uid: Option<Value>,
    pub user_env_probe: Option<Value>,
    pub override_command: Option<Value>,
    pub shutdown_action: Option<Value>,
    pub init: Option<Value>,
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
            mounts => "mounts",
            workspace_mount => "workspaceMount",
            features => "features",
            override_feature_install_order => "overrideFeatureInstallOrder",
            container_user => "containerUser",
            remote_user => "remoteUser",
            update_remote_user_uid => "updateRemoteUserUID",
            user_env_probe => "userEnvProbe",
            override_command => "overrideCommand",
            shutdown_action => "shutdownAction",
            init => "init",
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
    pub fn first(&self) -> &str {
        match self {
            Self::One(s) => s,
            Self::Many(v) => v.first().map(String::as_str).unwrap_or(""),
        }
    }

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
        if let Some(compose) = &self.properties.docker_compose_file {
            return format!("compose {}", compose.first());
        }
        "?".into()
    }
}

impl Config {
    pub fn parse(contents: &str) -> Result<Self> {
        toml::from_str(contents).context("invalid config")
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

        let mut properties = match sandbox.get("extends") {
            None => sandbox.clone(),
            Some(Value::String(template_name)) => {
                let template = self.templates.get(template_name).with_context(|| {
                    format!("sandbox `{name}` extends unknown template `{template_name}`")
                })?;
                deep_merge(template.clone(), sandbox.clone())
            }
            Some(other) => {
                bail!(
                    "sandbox `{name}`: `extends` must be a template name, got {}",
                    other.type_str()
                )
            }
        };
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

    pub fn resolve_all(&self) -> Result<Vec<ResolvedSandbox>> {
        self.sandboxes
            .keys()
            .map(|name| self.resolve_sandbox(name))
            .collect()
    }
}

/// Deep merge: `over` wins; nested tables merge recursively, everything else
/// is replaced wholesale.
/// Stable FNV-1a hash of a merged config table. `Table` is a `BTreeMap`, so its
/// TOML serialization is key-sorted and deterministic across runs.
fn config_hash(table: &Table) -> String {
    let text = toml::to_string(table).unwrap_or_default();
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn deep_merge(base: Table, over: Table) -> Table {
    let mut merged = base;
    for (key, value) in over {
        match (merged.remove(&key), value) {
            (Some(Value::Table(base_table)), Value::Table(over_table)) => {
                merged.insert(key, Value::Table(deep_merge(base_table, over_table)));
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
cache-folder = ".pnpm-store"
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
        assert_eq!(sandbox.properties.cache_folder.as_deref(), Some(".pnpm-store"));
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
    fn config_hash_is_stable_and_sensitive() {
        let config = Config::parse(EXAMPLE).unwrap();
        let a = config.resolve_sandbox("repository-1").unwrap().config_hash;
        let b = config.resolve_sandbox("repository-1").unwrap().config_hash;
        assert_eq!(a, b, "same config must hash identically");
        let c = config.resolve_sandbox("repository-2").unwrap().config_hash;
        assert_ne!(a, c, "different configs must hash differently");
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
remoteUser = "vscode"
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
                "remoteUser",
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

    #[test]
    fn compose_file_string_or_list() {
        let config = Config::parse(
            "[sandbox.a]\ndockerComposeFile = \"docker-compose.yml\"\nservice = \"app\"\n\
             [sandbox.b]\ndockerComposeFile = [\"a.yml\", \"b.yml\"]\nservice = \"app\"",
        )
        .unwrap();
        let a = config.resolve_sandbox("a").unwrap();
        assert_eq!(a.source(), "compose docker-compose.yml");
        let b = config.resolve_sandbox("b").unwrap();
        assert_eq!(
            b.properties.docker_compose_file.unwrap().to_vec(),
            vec!["a.yml", "b.yml"]
        );
    }
}
