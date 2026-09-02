use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use toml::{Table, Value};

pub const CONFIG_FILE: &str = "config.toml";

/// Raw `config.toml` contents. Service, template and sandbox bodies are kept
/// as free-form tables: sandboxes accept any devcontainer.json property plus
/// the devsandbox extras (`extends`, `folder`, `services`, `cache-folder`).
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

/// A sandbox with its `extends` template merged in.
#[derive(Debug)]
pub struct ResolvedSandbox {
    pub name: String,
    pub properties: Table,
}

impl ResolvedSandbox {
    fn str_property(&self, key: &str) -> Option<&str> {
        self.properties.get(key).and_then(Value::as_str)
    }

    pub fn folder(&self) -> Option<&str> {
        self.str_property("folder")
    }

    /// Short description of what the sandbox is built from.
    pub fn source(&self) -> String {
        if let Some(image) = self.str_property("image") {
            return format!("image {image}");
        }
        if let Some(dockerfile) = self
            .properties
            .get("build")
            .and_then(Value::as_table)
            .and_then(|b| b.get("dockerfile"))
            .and_then(Value::as_str)
        {
            return format!("dockerfile {dockerfile}");
        }
        if let Some(compose) = self.str_property("dockerComposeFile") {
            return format!("compose {compose}");
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

        Ok(ResolvedSandbox {
            name: name.to_string(),
            properties,
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
        assert_eq!(
            sandbox.properties.get("cache-folder").and_then(Value::as_str),
            Some(".pnpm-store")
        );
        assert!(sandbox.properties.get("extends").is_none());
    }

    #[test]
    fn resolve_deep_merges_nested_tables() {
        let config = Config::parse(EXAMPLE).unwrap();
        let sandbox = config.resolve_sandbox("repository-2").unwrap();
        assert_eq!(sandbox.source(), "dockerfile ./Dockerfile");
        let args = sandbox.properties["build"]["args"].as_table().unwrap();
        // A comes from the template, B is overridden by the sandbox.
        assert_eq!(args["A"].as_str(), Some("1"));
        assert_eq!(args["B"].as_str(), Some("3"));
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
}
