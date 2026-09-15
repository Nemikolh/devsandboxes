//! devcontainer `features` fetch + metadata.
//!
//! A feature is an OCI artifact (e.g. `ghcr.io/devcontainers/features/node:1`):
//! a tar layer holding `devcontainer-feature.json` + `install.sh`. This module
//! parses a feature ref, pulls the artifact from its registry by shelling out to
//! `curl` (mirroring the project's shell-out-to-docker/git philosophy; no new
//! crates), caches the extraction per user, reads the metadata, and computes an
//! install order from `installsAfter`. Building the derived image lives in the
//! `run` command (step 3); this module stops at a [`ResolvedFeature`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

/// The layer mediaType carrying a feature's tarball.
const FEATURE_LAYER_MEDIA_TYPE: &str = "application/vnd.devcontainers.layer.v1+tar";
/// The manifest mediaType we request.
const MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

/// A parsed OCI feature reference.
///
/// `ghcr.io/devcontainers/features/node:1` becomes `registry = "ghcr.io"`,
/// `path = "devcontainers/features/node"`, `id = "node"`, `tag = "1"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureRef {
    /// Registry host (with optional port), e.g. `ghcr.io`.
    pub registry: String,
    /// Repository path under the registry, e.g. `devcontainers/features/node`.
    pub path: String,
    /// Last path segment, e.g. `node`.
    pub id: String,
    /// Tag, e.g. `1`. Defaults to `latest` when absent.
    pub tag: String,
}

impl FeatureRef {
    /// Parse an OCI feature ref. Rejects local paths, refs without a registry
    /// host, and https tarball URLs with a clear error.
    pub fn parse(input: &str) -> Result<Self> {
        let input = input.trim();
        if input.is_empty() {
            bail!("empty feature reference");
        }
        if input.starts_with("http://") || input.starts_with("https://") {
            bail!(
                "feature `{input}`: tarball/URL features are not supported; use an OCI ref like `ghcr.io/devcontainers/features/node:1`"
            );
        }
        if input.starts_with('.') || input.starts_with('/') {
            bail!(
                "feature `{input}`: local-path features are not supported; use an OCI ref like `ghcr.io/devcontainers/features/node:1`"
            );
        }

        // The tag is after the last `:` only when that `:` sits after the last
        // `/` — otherwise the colon is a registry port (`localhost:5000/x`).
        let (name, tag) = match input.rfind(':') {
            Some(colon) if colon > input.rfind('/').unwrap_or(0) => {
                (&input[..colon], input[colon + 1..].to_string())
            }
            _ => (input, "latest".to_string()),
        };
        if tag.is_empty() {
            bail!("feature `{input}`: empty tag");
        }

        let mut segments = name.splitn(2, '/');
        let registry = segments
            .next()
            .filter(|s| !s.is_empty())
            .with_context(|| format!("feature `{input}`: missing registry host"))?;
        let path = segments.next().unwrap_or("");

        // A registry host must look like one: it has a `.` (domain) or `:`
        // (host:port) or is `localhost`, and the ref must carry a path after it.
        let looks_like_host =
            registry.contains('.') || registry.contains(':') || registry == "localhost";
        if path.is_empty() || !looks_like_host {
            bail!(
                "feature `{input}`: not an OCI reference (expected `<registry>/<path>[:<tag>]`, e.g. `ghcr.io/devcontainers/features/node:1`)"
            );
        }

        let id = path
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .with_context(|| format!("feature `{input}`: empty id"))?
            .to_string();

        Ok(FeatureRef {
            registry: registry.to_string(),
            path: path.to_string(),
            id,
            tag,
        })
    }

    /// The ref with its tag stripped, e.g. `ghcr.io/devcontainers/features/node`.
    /// Used to match `installsAfter` entries, which are written without a tag.
    pub fn without_tag(&self) -> String {
        format!("{}/{}", self.registry, self.path)
    }

    /// The full ref including tag, e.g. `ghcr.io/devcontainers/features/node:1`.
    pub fn full(&self) -> String {
        format!("{}/{}:{}", self.registry, self.path, self.tag)
    }

    /// Cache directory for this ref:
    /// `$XDG_CACHE_HOME/devsandbox/features/<registry>/<path>/<tag>/`,
    /// falling back to `$HOME/.cache/...`. Mirrors `State::path`.
    pub fn cache_dir(&self) -> Result<PathBuf> {
        Ok(cache_root()?.join(&self.registry).join(&self.path).join(&self.tag))
    }
}

/// Base cache dir (`$XDG_CACHE_HOME/devsandbox/features`, falling back to
/// `$HOME/.cache/devsandbox/features`). Mirrors `State::path`'s style.
fn cache_root() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .context("cannot determine user cache dir ($XDG_CACHE_HOME or $HOME)")?;
    Ok(base.join("devsandbox/features"))
}

/// A fresh build-context dir for a sandbox's derived image build, under the
/// features cache root: `<cache_root>/build-<sandbox>`. Recreated each call
/// (any prior contents are removed) so a rebuild starts clean.
pub fn build_context_dir(sandbox: &str) -> Result<PathBuf> {
    let dir = cache_root()?.join(format!("build-{sandbox}"));
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("cannot clear {}", dir.display()))?;
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    Ok(dir)
}

/// A feature's `devcontainer-feature.json`. Tolerant of unknown fields and
/// missing values so new schema keys don't break parsing.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeatureMetadata {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub options: BTreeMap<String, FeatureOption>,
    #[serde(default)]
    pub container_env: BTreeMap<String, String>,
    #[serde(default)]
    pub installs_after: Vec<String>,
    #[serde(default)]
    pub entrypoint: Option<String>,
    #[serde(default, rename = "documentationURL")]
    pub documentation_url: Option<String>,
}

/// One entry in a feature's `options` map. Only `default` is acted on for now;
/// other keys (`type`, `description`, `proposals`, `enum`) are tolerated.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeatureOption {
    #[serde(default)]
    pub default: Option<serde_json::Value>,
}

impl FeatureMetadata {
    fn parse(json: &str) -> Result<Self> {
        serde_json::from_str(json).context("invalid devcontainer-feature.json")
    }

    fn read(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        Self::parse(&contents).with_context(|| format!("in {}", path.display()))
    }
}

/// A fetched feature: its ref, the cache dir holding the extracted contents, and
/// the parsed metadata.
#[derive(Debug, Clone)]
pub struct ResolvedFeature {
    pub reference: FeatureRef,
    pub dir: PathBuf,
    pub metadata: FeatureMetadata,
}

/// Fetch a feature into the default per-user cache and read its metadata.
pub fn fetch(reference: &FeatureRef) -> Result<ResolvedFeature> {
    fetch_into(reference, &cache_root()?)
}

/// Like [`fetch`] but with an explicit cache root (for tests). The feature is
/// extracted into `<cache_root>/<registry>/<path>/<tag>/`.
pub fn fetch_into(reference: &FeatureRef, cache_root: &Path) -> Result<ResolvedFeature> {
    let dir = cache_root
        .join(&reference.registry)
        .join(&reference.path)
        .join(&reference.tag);
    let metadata_path = dir.join("devcontainer-feature.json");

    // Offline reuse: a populated cache is taken as-is (mutable tags won't
    // auto-refresh; clear the cache dir to force a re-pull).
    if !metadata_path.exists() {
        download_and_extract(reference, &dir)
            .with_context(|| format!("fetching feature `{}`", reference.full()))?;
    }

    let metadata = FeatureMetadata::read(&metadata_path)?;
    Ok(ResolvedFeature {
        reference: reference.clone(),
        dir,
        metadata,
    })
}

/// Pull the manifest + layer blob and extract the tarball into `dir`. Extraction
/// goes to a sibling temp dir that is renamed into place, so a killed fetch never
/// leaves a half-populated cache.
fn download_and_extract(reference: &FeatureRef, dir: &Path) -> Result<()> {
    let token = fetch_token(reference)?;
    let manifest = fetch_manifest(reference, token.as_deref())?;
    let digest = feature_layer_digest(&manifest)
        .with_context(|| format!("feature `{}`: no feature layer in manifest", reference.full()))?;

    let parent = dir
        .parent()
        .ok_or_else(|| anyhow!("invalid cache dir {}", dir.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("cannot create {}", parent.display()))?;

    // Temp dir + temp tarball as siblings of the target, for an atomic rename.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp_dir = parent.join(format!(".{}-{}.tmp", reference.tag, stamp));
    let tmp_tar = parent.join(format!(".{}-{}.tar", reference.tag, stamp));
    // Best-effort cleanup of leftovers from a previous crash.
    let _ = std::fs::remove_dir_all(&tmp_dir);
    let _ = std::fs::remove_file(&tmp_tar);
    std::fs::create_dir_all(&tmp_dir)
        .with_context(|| format!("cannot create {}", tmp_dir.display()))?;

    let result = (|| -> Result<()> {
        download_blob(reference, &digest, token.as_deref(), &tmp_tar)?;
        // `tar` auto-detects compression (feature layers are gzip tarballs).
        run_checked(
            Command::new("tar").arg("-xf").arg(&tmp_tar).arg("-C").arg(&tmp_dir),
            "tar",
        )?;
        Ok(())
    })();

    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        let _ = std::fs::remove_file(&tmp_tar);
        return Err(e);
    }
    let _ = std::fs::remove_file(&tmp_tar);

    // Atomic publish. If another concurrent fetch won the race, keep theirs.
    if dir.exists() {
        let _ = std::fs::remove_dir_all(&tmp_dir);
    } else if let Err(e) = std::fs::rename(&tmp_dir, dir) {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return Err(e).with_context(|| format!("cannot publish cache dir {}", dir.display()));
    }
    Ok(())
}

/// Obtain a pull token if the registry demands one. Requests the manifest
/// anonymously first; on a 401 it parses the `WWW-Authenticate: Bearer …`
/// challenge and fetches a token. Returns `None` when anonymous access works.
fn fetch_token(reference: &FeatureRef) -> Result<Option<String>> {
    let manifest_url = format!(
        "https://{}/v2/{}/manifests/{}",
        reference.registry, reference.path, reference.tag
    );
    let headers = curl_headers(&[
        "-H",
        &format!("Accept: {MANIFEST_MEDIA_TYPE}"),
        &manifest_url,
    ])?;
    let (status, header_lines) = split_status(&headers);
    if status != 401 {
        return Ok(None);
    }
    let challenge = header_lines
        .iter()
        .find_map(|l| l.strip_prefix_ci("www-authenticate:"))
        .map(str::trim)
        .with_context(|| {
            format!("feature `{}`: 401 without a WWW-Authenticate challenge", reference.full())
        })?;
    let challenge = parse_bearer_challenge(challenge, &reference.path)?;
    let token_url = format!(
        "{}?service={}&scope={}",
        challenge.realm,
        urlencode(&challenge.service),
        urlencode(&challenge.scope),
    );
    let body = curl_body(&[&token_url])?;
    let json: serde_json::Value =
        serde_json::from_str(&body).context("token endpoint returned invalid JSON")?;
    let token = json
        .get("token")
        .or_else(|| json.get("access_token"))
        .and_then(|v| v.as_str())
        .with_context(|| format!("feature `{}`: token endpoint had no token", reference.full()))?;
    Ok(Some(token.to_string()))
}

/// GET the manifest JSON, sending the bearer token when present.
fn fetch_manifest(reference: &FeatureRef, token: Option<&str>) -> Result<serde_json::Value> {
    let url = format!(
        "https://{}/v2/{}/manifests/{}",
        reference.registry, reference.path, reference.tag
    );
    let mut args: Vec<String> = vec!["-H".into(), format!("Accept: {MANIFEST_MEDIA_TYPE}")];
    if let Some(token) = token {
        args.push("-H".into());
        args.push(format!("Authorization: Bearer {token}"));
    }
    args.push(url);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let body = curl_body(&refs)?;
    serde_json::from_str(&body).context("registry returned an invalid manifest")
}

/// GET a blob by digest, following redirects, writing the bytes to `out`.
fn download_blob(
    reference: &FeatureRef,
    digest: &str,
    token: Option<&str>,
    out: &Path,
) -> Result<()> {
    let url = format!(
        "https://{}/v2/{}/blobs/{}",
        reference.registry, reference.path, digest
    );
    let mut cmd = Command::new("curl");
    cmd.arg("-fsSL"); // -L: blob GETs redirect to storage.
    if let Some(token) = token {
        cmd.arg("-H").arg(format!("Authorization: Bearer {token}"));
    }
    cmd.arg("-o").arg(out).arg(&url);
    run_checked(&mut cmd, "curl")?;
    Ok(())
}

/// Find the feature layer's digest in a manifest.
fn feature_layer_digest(manifest: &serde_json::Value) -> Option<String> {
    manifest
        .get("layers")?
        .as_array()?
        .iter()
        .find(|layer| {
            layer.get("mediaType").and_then(|v| v.as_str()) == Some(FEATURE_LAYER_MEDIA_TYPE)
        })
        .and_then(|layer| layer.get("digest"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// A parsed `WWW-Authenticate: Bearer …` challenge.
#[derive(Debug, PartialEq, Eq)]
struct BearerChallenge {
    realm: String,
    service: String,
    scope: String,
}

/// Parse a `Bearer realm="…",service="…",scope="…"` challenge. `scope` defaults
/// to `repository:<path>:pull` when the challenge omits it.
fn parse_bearer_challenge(value: &str, path: &str) -> Result<BearerChallenge> {
    let params = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))
        .with_context(|| format!("unsupported auth challenge: {value}"))?;

    let mut map: BTreeMap<String, String> = BTreeMap::new();
    for part in split_challenge_params(params) {
        if let Some((k, v)) = part.split_once('=') {
            let v = v.trim().trim_matches('"');
            map.insert(k.trim().to_ascii_lowercase(), v.to_string());
        }
    }

    let realm = map
        .get("realm")
        .cloned()
        .with_context(|| format!("auth challenge missing realm: {value}"))?;
    let service = map.get("service").cloned().unwrap_or_default();
    let scope = map
        .get("scope")
        .cloned()
        .unwrap_or_else(|| format!("repository:{path}:pull"));
    Ok(BearerChallenge {
        realm,
        service,
        scope,
    })
}

/// Split a challenge parameter list on commas that sit outside quotes, so a
/// `scope="repository:x:pull,push"` value stays intact.
fn split_challenge_params(params: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for c in params.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                cur.push(c);
            }
            ',' if !in_quotes => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// Minimal percent-encoding for query values (token scope/service).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Run `curl -sS -D - -o /dev/null <args>` and return the captured response
/// headers (status line included), for inspecting a 401 challenge.
fn curl_headers(args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("curl");
    cmd.arg("-sS").arg("-D").arg("-").arg("-o").arg("/dev/null");
    cmd.args(args);
    capture_stdout(&mut cmd, "curl")
}

/// Run `curl -fsSL <args>` and return the response body.
fn curl_body(args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("curl");
    cmd.arg("-fsSL");
    cmd.args(args);
    capture_stdout(&mut cmd, "curl")
}

/// Capture stdout; non-zero exit is an error with the first stderr line folded
/// in (mirrors `runtime::output_quiet`).
fn capture_stdout(cmd: &mut Command, name: &str) -> Result<String> {
    let out = cmd
        .output()
        .with_context(|| format!("failed to run {name} (is it installed?)"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!(
            "{name} exited with status {}: {}",
            out.status.code().unwrap_or(1),
            stderr.lines().next().unwrap_or("non-zero exit").trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Run a command, treating a non-zero exit as an error (stderr inherited).
fn run_checked(cmd: &mut Command, name: &str) -> Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("failed to run {name} (is it installed?)"))?;
    if !status.success() {
        bail!("{name} exited with status {}", status.code().unwrap_or(1));
    }
    Ok(())
}

/// Split a raw HTTP header dump into `(status_code, header_lines)`. `curl -D -`
/// may print multiple blocks on redirects; the last block's status wins.
fn split_status(headers: &str) -> (u16, Vec<&str>) {
    let mut status = 0u16;
    let mut lines = Vec::new();
    for line in headers.lines() {
        let line = line.trim_end();
        if let Some(rest) = line.strip_prefix("HTTP/") {
            // e.g. "HTTP/2 401" or "HTTP/1.1 401 Unauthorized"
            if let Some(code) = rest.split_whitespace().nth(1).and_then(|c| c.parse().ok()) {
                status = code;
                lines.clear();
                continue;
            }
        }
        if !line.is_empty() {
            lines.push(line);
        }
    }
    (status, lines)
}

/// Case-insensitive header-prefix helper.
trait StripPrefixCi {
    fn strip_prefix_ci(&self, prefix: &str) -> Option<&str>;
}

impl StripPrefixCi for &str {
    fn strip_prefix_ci(&self, prefix: &str) -> Option<&str> {
        // Compare bytes before slicing: a match implies the prefix region is
        // ASCII, so the subsequent str slice cannot split a UTF-8 char.
        let head = self.as_bytes().get(..prefix.len())?;
        if head.eq_ignore_ascii_case(prefix.as_bytes()) {
            Some(&self[prefix.len()..])
        } else {
            None
        }
    }
}

/// Order features so every feature installs after the ones it declares in
/// `installsAfter`. Topological sort with an alphabetical (by full ref)
/// tie-break among ready nodes each round, for determinism. `installsAfter`
/// entries not present in the requested set are ignored (soft deps). A cycle is
/// an error naming its members.
///
/// `installsAfter` entries are written without a tag; each is matched against a
/// candidate's ref-sans-tag first, then its bare id (the official CLI matches
/// loosely).
pub fn install_order(features: Vec<ResolvedFeature>) -> Result<Vec<ResolvedFeature>> {
    let keys: Vec<String> = features.iter().map(|f| f.reference.full()).collect();

    // Map each dependency selector (ref-sans-tag or bare id) to the feature
    // key(s) it resolves to, so `installsAfter` can be matched loosely.
    let mut by_sans_tag: BTreeMap<String, String> = BTreeMap::new();
    let mut by_id: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for f in &features {
        by_sans_tag.insert(f.reference.without_tag(), f.reference.full());
        by_id.entry(f.reference.id.clone()).or_default().push(f.reference.full());
    }

    // Edges: `dep -> feature` (dep installs before feature).
    let mut deps: BTreeMap<String, BTreeSet<String>> = keys
        .iter()
        .map(|k| (k.clone(), BTreeSet::new()))
        .collect();
    for f in &features {
        let key = f.reference.full();
        for entry in &f.metadata.installs_after {
            let entry = entry.trim();
            // Match sans-tag first; fall back to bare id.
            if let Some(dep) = by_sans_tag.get(entry) {
                if *dep != key {
                    deps.get_mut(&key).unwrap().insert(dep.clone());
                }
            } else if let Some(dep_keys) = by_id.get(entry) {
                for dep in dep_keys {
                    if *dep != key {
                        deps.get_mut(&key).unwrap().insert(dep.clone());
                    }
                }
            }
            // else: soft dep not in the requested set — ignored.
        }
    }

    let mut features_by_key: BTreeMap<String, ResolvedFeature> =
        features.into_iter().map(|f| (f.reference.full(), f)).collect();

    let mut ordered = Vec::with_capacity(keys.len());
    let mut placed: BTreeSet<String> = BTreeSet::new();
    while placed.len() < keys.len() {
        // Ready = all deps already placed. `deps` keys are sorted (BTreeMap),
        // giving an alphabetical tie-break among ready nodes.
        let ready: Vec<String> = deps
            .iter()
            .filter(|(k, _)| !placed.contains(*k))
            .filter(|(_, d)| d.iter().all(|dep| placed.contains(dep)))
            .map(|(k, _)| k.clone())
            .collect();
        if ready.is_empty() {
            let cycle: Vec<String> = keys.iter().filter(|k| !placed.contains(*k)).cloned().collect();
            bail!("cyclic feature `installsAfter`: {}", cycle.join(", "));
        }
        for key in ready {
            ordered.push(features_by_key.remove(&key).unwrap());
            placed.insert(key);
        }
    }
    Ok(ordered)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feature(reference: &str, installs_after: &[&str]) -> ResolvedFeature {
        let reference = FeatureRef::parse(reference).unwrap();
        let mut metadata = FeatureMetadata::default();
        metadata.id = Some(reference.id.clone());
        metadata.installs_after = installs_after.iter().map(|s| s.to_string()).collect();
        ResolvedFeature {
            reference,
            dir: PathBuf::new(),
            metadata,
        }
    }

    #[test]
    fn parse_full_ref_with_tag() {
        let r = FeatureRef::parse("ghcr.io/devcontainers/features/node:1").unwrap();
        assert_eq!(r.registry, "ghcr.io");
        assert_eq!(r.path, "devcontainers/features/node");
        assert_eq!(r.id, "node");
        assert_eq!(r.tag, "1");
        assert_eq!(r.without_tag(), "ghcr.io/devcontainers/features/node");
        assert_eq!(r.full(), "ghcr.io/devcontainers/features/node:1");
    }

    #[test]
    fn parse_defaults_tag_to_latest() {
        let r = FeatureRef::parse("ghcr.io/devcontainers/features/go").unwrap();
        assert_eq!(r.tag, "latest");
        assert_eq!(r.id, "go");
    }

    #[test]
    fn parse_registry_with_port() {
        let r = FeatureRef::parse("localhost:5000/team/feat:2").unwrap();
        assert_eq!(r.registry, "localhost:5000");
        assert_eq!(r.path, "team/feat");
        assert_eq!(r.id, "feat");
        assert_eq!(r.tag, "2");
    }

    #[test]
    fn parse_registry_with_port_no_tag() {
        // The colon is a port, not a tag: tag must default to latest.
        let r = FeatureRef::parse("localhost:5000/team/feat").unwrap();
        assert_eq!(r.registry, "localhost:5000");
        assert_eq!(r.path, "team/feat");
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn parse_rejects_local_paths() {
        for input in ["./x", "../x", "/abs/x"] {
            let err = FeatureRef::parse(input).unwrap_err().to_string();
            assert!(err.contains("local-path"), "{input}: {err}");
        }
    }

    #[test]
    fn parse_rejects_single_segment() {
        for input in ["node", "just-a-name"] {
            assert!(FeatureRef::parse(input).is_err(), "{input} should be rejected");
        }
    }

    #[test]
    fn parse_rejects_url_forms() {
        for input in [
            "https://example.com/feature.tgz",
            "http://example.com/feature.tar",
        ] {
            let err = FeatureRef::parse(input).unwrap_err().to_string();
            assert!(err.contains("tarball") || err.contains("URL"), "{input}: {err}");
        }
    }

    #[test]
    fn parse_rejects_hostless_ref() {
        // First segment has no `.`/`:` and isn't localhost -> not a host.
        let err = FeatureRef::parse("myorg/feature:1").unwrap_err().to_string();
        assert!(err.contains("not an OCI reference"), "{err}");
    }

    #[test]
    fn install_order_soft_dep() {
        // common-utils installs first; node declares installsAfter common-utils.
        let feats = vec![
            feature("ghcr.io/devcontainers/features/node:1", &[
                "ghcr.io/devcontainers/features/common-utils",
            ]),
            feature("ghcr.io/devcontainers/features/common-utils:2", &[]),
        ];
        let ordered = install_order(feats).unwrap();
        let ids: Vec<_> = ordered.iter().map(|f| f.reference.id.as_str()).collect();
        assert_eq!(ids, ["common-utils", "node"]);
    }

    #[test]
    fn install_order_matches_installs_after_by_bare_id() {
        // installsAfter uses a bare id ("common-utils"), not the full path.
        let feats = vec![
            feature("ghcr.io/devcontainers/features/node:1", &["common-utils"]),
            feature("ghcr.io/devcontainers/features/common-utils:2", &[]),
        ];
        let ordered = install_order(feats).unwrap();
        let ids: Vec<_> = ordered.iter().map(|f| f.reference.id.as_str()).collect();
        assert_eq!(ids, ["common-utils", "node"]);
    }

    #[test]
    fn install_order_alphabetical_tie_break() {
        // No deps between them: alphabetical by full ref (git < node < zsh).
        let feats = vec![
            feature("ghcr.io/devcontainers/features/node:1", &[]),
            feature("ghcr.io/devcontainers/features/zsh:1", &[]),
            feature("ghcr.io/devcontainers/features/git:1", &[]),
        ];
        let ordered = install_order(feats).unwrap();
        let ids: Vec<_> = ordered.iter().map(|f| f.reference.id.as_str()).collect();
        assert_eq!(ids, ["git", "node", "zsh"]);
    }

    #[test]
    fn install_order_absent_dep_is_ignored() {
        // installsAfter references a feature not in the set -> ignored, no error.
        let feats = vec![feature(
            "ghcr.io/devcontainers/features/node:1",
            &["ghcr.io/devcontainers/features/common-utils"],
        )];
        let ordered = install_order(feats).unwrap();
        assert_eq!(ordered.len(), 1);
        assert_eq!(ordered[0].reference.id, "node");
    }

    #[test]
    fn install_order_cycle_errors() {
        let feats = vec![
            feature("ghcr.io/x/a:1", &["ghcr.io/x/b"]),
            feature("ghcr.io/x/b:1", &["ghcr.io/x/a"]),
        ];
        let err = install_order(feats).unwrap_err().to_string();
        assert!(err.contains("cyclic"), "{err}");
        assert!(err.contains("ghcr.io/x/a:1") && err.contains("ghcr.io/x/b:1"), "{err}");
    }

    #[test]
    fn metadata_deserializes_from_fixture() {
        // Realistic go-feature snippet with unknown fields, camelCase renames,
        // an option default, containerEnv with ${PATH}, and installsAfter.
        let json = r#"{
            "id": "go",
            "version": "1.3.2",
            "name": "Go",
            "documentationURL": "https://github.com/devcontainers/features/tree/main/src/go",
            "options": {
                "version": {
                    "type": "string",
                    "proposals": ["latest", "none", "1.22"],
                    "default": "latest",
                    "description": "Select or enter a Go version to install"
                }
            },
            "containerEnv": {
                "GOROOT": "/usr/local/go",
                "PATH": "/usr/local/go/bin:${PATH}"
            },
            "installsAfter": [
                "ghcr.io/devcontainers/features/common-utils"
            ],
            "entrypoint": "/usr/local/share/go-init.sh",
            "someFutureUnknownField": {"nested": true}
        }"#;
        let md = FeatureMetadata::parse(json).unwrap();
        assert_eq!(md.id.as_deref(), Some("go"));
        assert_eq!(md.version.as_deref(), Some("1.3.2"));
        assert_eq!(md.name.as_deref(), Some("Go"));
        assert_eq!(
            md.documentation_url.as_deref(),
            Some("https://github.com/devcontainers/features/tree/main/src/go")
        );
        assert_eq!(
            md.options["version"].default.as_ref().and_then(|v| v.as_str()),
            Some("latest")
        );
        assert_eq!(md.container_env["GOROOT"], "/usr/local/go");
        assert_eq!(md.container_env["PATH"], "/usr/local/go/bin:${PATH}");
        assert_eq!(
            md.installs_after,
            vec!["ghcr.io/devcontainers/features/common-utils".to_string()]
        );
        assert_eq!(md.entrypoint.as_deref(), Some("/usr/local/share/go-init.sh"));
    }

    #[test]
    fn bearer_challenge_parses_realm_service_scope() {
        let ch = parse_bearer_challenge(
            r#"Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:devcontainers/features/node:pull""#,
            "devcontainers/features/node",
        )
        .unwrap();
        assert_eq!(ch.realm, "https://ghcr.io/token");
        assert_eq!(ch.service, "ghcr.io");
        assert_eq!(ch.scope, "repository:devcontainers/features/node:pull");
    }

    #[test]
    fn bearer_challenge_defaults_scope() {
        let ch = parse_bearer_challenge(
            r#"Bearer realm="https://auth.example.com/token",service="registry.example.com""#,
            "team/feat",
        )
        .unwrap();
        assert_eq!(ch.realm, "https://auth.example.com/token");
        assert_eq!(ch.service, "registry.example.com");
        assert_eq!(ch.scope, "repository:team/feat:pull");
    }

    #[test]
    fn bearer_challenge_rejects_non_bearer() {
        assert!(parse_bearer_challenge(r#"Basic realm="x""#, "team/feat").is_err());
    }

    #[test]
    fn split_status_takes_last_block() {
        let headers = "HTTP/1.1 401 Unauthorized\r\nwww-authenticate: Bearer realm=\"x\"\r\n\r\nHTTP/2 200\r\ncontent-type: application/json\r\n";
        let (status, lines) = split_status(headers);
        assert_eq!(status, 200);
        assert!(lines.iter().any(|l| l.starts_with("content-type")));
        // The 401 block's headers were cleared when the 200 status arrived.
        assert!(!lines.iter().any(|l| l.to_ascii_lowercase().starts_with("www-authenticate")));
    }

    /// Network-gated: pulls common-utils from ghcr.io into a temp cache. Skips
    /// (with a message) when the registry is unreachable.
    #[test]
    fn fetches_common_utils_from_ghcr() {
        // Probe reachability: a 401 counts as reachable (registry is up).
        let probe = Command::new("curl")
            .args(["-fsS", "-o", "/dev/null", "--max-time", "10", "https://ghcr.io/v2/"])
            .status();
        let reachable = match probe {
            // -f makes a 401 exit non-zero (22); treat exit 22 as reachable too.
            Ok(s) => s.success() || s.code() == Some(22),
            Err(_) => false,
        };
        if !reachable {
            eprintln!("skipping fetches_common_utils_from_ghcr: ghcr.io unreachable");
            return;
        }

        let tmp = std::env::temp_dir().join(format!(
            "devsandbox-feat-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let reference = FeatureRef::parse("ghcr.io/devcontainers/features/common-utils:2").unwrap();
        let resolved = fetch_into(&reference, &tmp);

        // Clean up regardless of outcome.
        let cleanup = |p: &Path| {
            let _ = std::fs::remove_dir_all(p);
        };

        let resolved = match resolved {
            Ok(r) => r,
            Err(e) => {
                cleanup(&tmp);
                panic!("fetch failed: {e:#}");
            }
        };
        assert!(
            resolved.dir.join("devcontainer-feature.json").exists(),
            "devcontainer-feature.json missing in {}",
            resolved.dir.display()
        );
        assert_eq!(resolved.metadata.id.as_deref(), Some("common-utils"));
        assert!(resolved.dir.join("install.sh").exists(), "install.sh missing");

        cleanup(&tmp);
    }
}
