//! A config root's bundle (docs/architecture/daemon/fleet.md, *Config
//! versions*): what the authority signs as a config version and keeps to
//! serve it. The bundle is the config file plus each file under the root
//! the config references (sandboxes' and services' `build` dockerfile and
//! context, `${configDir}` bind sources, local features); its hash is over
//! the config's structure and those files, not the root's text: comments,
//! formatting, notes and logs in the root change nothing.
//!
//! [`collect`] runs over any dir, so a receiver recomputes the hash over
//! the copy it unpacked from [`Bundle::tar`] and gets the same one.

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};
use toml::Table;

use crate::config::{self, Build, Config, FeatureOptions, Mount, MountContext};
use crate::features::FeatureSource;
use crate::serve::fleet::code::{ROOT_SKIPS, ROOT_TAR_LIMIT, tar};

/// The domain the hash starts with.
pub const HASH_DOMAIN: &str = "devsandbox/mesh-config/v1";

/// One referenced file: its contents and exec bit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    pub exec: bool,
    pub data: Vec<u8>,
}

/// A config root's bundle: the config file (its name in the root and its
/// bytes as written) and the files it references, by relative path.
#[derive(Debug, Clone)]
pub struct Bundle {
    pub config_name: String,
    pub config: Vec<u8>,
    /// The parsed config file, what the hash covers of it.
    table: Table,
    pub files: BTreeMap<String, File>,
}

/// `field` length-prefixed (u64 big-endian), so no two field lists encode
/// alike.
fn put(out: &mut Sha256, field: &[u8]) {
    out.update((field.len() as u64).to_be_bytes());
    out.update(field);
}

impl Bundle {
    /// sha256 over [`HASH_DOMAIN`], the config table's canonical encoding
    /// (`toml::Table` is a `BTreeMap`: its TOML is key-sorted, as
    /// `config::config_hash` relies on), then per referenced file in path
    /// order its path, exec bit and contents' sha256.
    pub fn hash(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        put(&mut h, HASH_DOMAIN.as_bytes());
        put(&mut h, toml::to_string(&self.table).expect("a parsed table serializes").as_bytes());
        for (path, f) in &self.files {
            put(&mut h, path.as_bytes());
            put(&mut h, &[u8::from(f.exec)]);
            put(&mut h, &Sha256::digest(&f.data));
        }
        h.finalize().into()
    }

    /// The bundle as a [`tar`]: the config file then the referenced files,
    /// at their relative paths. Deterministic: the same bundle, the same
    /// bytes.
    pub fn tar(&self) -> Result<Vec<u8>> {
        let mut w = tar::Writer::default();
        w.file(&self.config_name, false, &self.config)?;
        for (path, f) in self.files.iter().filter(|(p, _)| **p != self.config_name) {
            w.file(path, f.exec, &f.data)?;
        }
        Ok(w.finish())
    }
}

/// Where the authority keeps the bundle hashing to `hash`, under data dir
/// `data` (`<data>/devsandbox/`, `member.toml`'s): outside `fleet/`, which
/// the worker policy covers.
pub fn bundle_path(data: &Path, hash: &[u8; 32]) -> PathBuf {
    data.join("mesh").join("bundles").join(format!("{}.tar", hex::encode(hash)))
}

/// `bundle` stored at [`bundle_path`] (temp file + rename, 0600: a
/// referenced file may hold a token). Already there: left as is.
pub fn store(data: &Path, bundle: &Bundle) -> Result<PathBuf> {
    let path = bundle_path(data, &bundle.hash());
    if path.is_file() {
        return Ok(path);
    }
    let dir = path.parent().expect("a bundle path has a parent");
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let tmp = dir.join(format!(".{}.{}.tmp", path.file_name().and_then(|n| n.to_str()).unwrap_or_default(), std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::File::options()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("cannot create {}", tmp.display()))?;
    f.write_all(&bundle.tar()?).with_context(|| format!("cannot write {}", tmp.display()))?;
    drop(f);
    std::fs::rename(&tmp, &path).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(path)
}

/// The authority's name → root bindings: `<data>/devsandbox/mesh/roots.toml`,
/// `name = "/canonical/path"`. A config root is named in the log by its
/// directory's name, so two roots with one name would bump each other's
/// version: the first path a name is signed from keeps it ([`bind`]).
/// Baby step (mesh-config plan, decision 3): the name may become a full
/// path or another identity later.
pub fn roots_path(data: &Path) -> PathBuf {
    data.join("mesh").join("roots.toml")
}

/// The bindings at `path`; a missing file is none.
pub fn load_roots(path: &Path) -> Result<BTreeMap<String, PathBuf>> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("invalid {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// The path `name` is bound to under data dir `data`, if any.
pub fn bound_root(data: &Path, name: &str) -> Result<Option<PathBuf>> {
    Ok(load_roots(&roots_path(data))?.remove(name))
}

/// `name` bound to `path` in `roots` when unbound (pure). `Err`: the path
/// it is bound to, another one.
pub fn bind_in(roots: &mut BTreeMap<String, PathBuf>, name: &str, path: &Path) -> Result<(), PathBuf> {
    match roots.get(name) {
        Some(bound) if bound != path => Err(bound.clone()),
        Some(_) => Ok(()),
        None => {
            roots.insert(name.to_string(), path.to_path_buf());
            Ok(())
        }
    }
}

/// [`bind_in`] over [`roots_path`] under data dir `data`, under its lock
/// (`serve::locked_file`: two verbs can sign at once); written only when
/// `name` is newly bound.
pub fn bind(data: &Path, name: &str, path: &Path) -> Result<Result<(), PathBuf>> {
    crate::serve::locked_file::update(
        &roots_path(data),
        load_roots,
        |r| toml::to_string(r).context("cannot serialize the config roots"),
        |r| bind_in(r, name, path),
    )
}

/// The bundle of config root `root`. The config must load
/// (`Config::load`), and every sandbox's merged table and every service's
/// `build` must read, else there is no bundle: what it references isn't
/// known.
///
/// A reference is bundled only when written relative to the config dir (a
/// mount source starting `${configDir}` with no other variable; a relative
/// `build` path or local feature): an absolute path or another variable is
/// a host path, resolved per host like `folder`, and a receiver's copy
/// sits at another path. It must also resolve, canonicalized, under the
/// root, and not be the root itself (a default `build.context` of `.`), in
/// [`ROOT_SKIPS`] or in a `.git`. A missing one is skipped (bind sources
/// are created per host). A directory contributes its files, recursively;
/// a symlink is followed when it resolves to a file under the root, else
/// left out.
pub fn collect(root: &Path) -> Result<Bundle> {
    let config = Config::load(root)?;
    let path = config::find(root)?.with_context(|| format!("no config file in {}", root.display()))?;
    let config_name = path.file_name().and_then(|n| n.to_str()).context("the config file's name isn't UTF-8")?.to_string();
    let text = std::fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
    let table = raw_table(&path, &text)?;
    let canon = root.canonicalize().with_context(|| format!("cannot resolve {}", root.display()))?;

    let mut refs: Vec<PathBuf> = Vec::new();
    let build_refs = |build: &Build, refs: &mut Vec<PathBuf>| {
        refs.extend(build.dockerfile.as_deref().map(PathBuf::from));
        refs.push(PathBuf::from(build.context.as_deref().unwrap_or(".")));
    };
    for name in config.sandboxes.keys() {
        let (merged, _) = config.resolved_table(name).with_context(|| format!("sandbox `{name}`"))?;
        let field = |key: &str| merged.get(key).cloned();
        if let Some(build) = field("build") {
            let build: Build = build.try_into().map_err(|e| anyhow!("sandbox `{name}`: build: {e}"))?;
            build_refs(&build, &mut refs);
        }
        if let Some(mounts) = field("mounts") {
            let mounts: Vec<Mount> = mounts.try_into().map_err(|e| anyhow!("sandbox `{name}`: mounts: {e}"))?;
            refs.extend(mounts.iter().filter_map(|m| mount_ref(m, &canon)));
        }
        if let Some(features) = field("features") {
            let features: BTreeMap<String, FeatureOptions> =
                features.try_into().map_err(|e| anyhow!("sandbox `{name}`: features: {e}"))?;
            for (reference, options) in &features {
                if matches!(options, FeatureOptions::Enabled(false)) {
                    continue;
                }
                if let Ok(FeatureSource::Local(p)) = FeatureSource::parse(reference) {
                    refs.push(p);
                }
            }
        }
    }
    for (name, service) in &config.services {
        if let Some(build) = service.get("build").cloned() {
            let build: Build = build.try_into().map_err(|e| anyhow!("service `{name}`: build: {e}"))?;
            build_refs(&build, &mut refs);
        }
    }

    let mut files = BTreeMap::new();
    let mut size = text.len() as u64;
    for r in refs {
        let Some(rel) = relative_under(&canon, &r) else { continue };
        add(&canon, &rel, &mut files, &mut size)?;
    }
    Ok(Bundle { config_name, config: text, table, files })
}

/// The config file as a table, as written (not merged per sandbox: an
/// unused template still counts). YAML lands in the same `toml::Table`, as
/// `Config::parse_yaml` has it.
fn raw_table(path: &Path, text: &[u8]) -> Result<Table> {
    let text = std::str::from_utf8(text).with_context(|| format!("{} isn't UTF-8", path.display()))?;
    match path.extension().and_then(|e| e.to_str()) {
        Some("yaml" | "yml") => {
            let t: Option<Table> = serde_saphyr::from_str(text).map_err(|e| anyhow!("{e}")).with_context(|| format!("in {}", path.display()))?;
            Ok(t.unwrap_or_default())
        }
        _ => toml::from_str(text).with_context(|| format!("in {}", path.display())),
    }
}

/// A bind mount's source as a path relative to the config dir, when it is
/// written `${configDir}/…` with no other variable.
fn mount_ref(mount: &Mount, canon: &Path) -> Option<PathBuf> {
    let (kind, source) = match mount {
        Mount::Shorthand(s) => {
            let (kind, source, _, _) = config::parse_shorthand(s).ok()?;
            (kind, source)
        }
        Mount::Object(o) => (o.kind.clone(), o.source.clone()),
    };
    if kind.as_deref().unwrap_or("bind") != "bind" {
        return None;
    }
    let source = source?;
    let rest = source.strip_prefix("${configDir}")?;
    if rest.contains("${") {
        return None;
    }
    let dir = canon.to_string_lossy();
    let ctx = MountContext {
        config_dir: &dir,
        workspace_folder: "",
        workspace_folder_basename: "",
        shared_volumes: "",
        instance: "",
        container_env: None,
    };
    let resolved = PathBuf::from(config::substitute(&source, &ctx));
    resolved.strip_prefix(canon).ok().map(Path::to_path_buf)
}

/// `reference` (relative to the config dir) as a `/`-separated path under
/// the root, lexically normalized, when it is relative, stays under the
/// root, isn't the root and is in no skipped place. `None` otherwise.
fn relative_under(canon: &Path, reference: &Path) -> Option<String> {
    if reference.is_absolute() {
        return None;
    }
    let mut at = canon.to_path_buf();
    for c in reference.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                at.pop();
            }
            Component::Normal(n) => at.push(n),
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    let rel = at.strip_prefix(canon).ok()?;
    let rel = rel.to_str()?.to_string();
    (!skipped(&rel)).then_some(rel)
}

/// The root itself, [`ROOT_SKIPS`] at its top, or a `.git` anywhere.
fn skipped(rel: &str) -> bool {
    rel.is_empty() || ROOT_SKIPS.iter().any(|s| rel.split('/').next() == Some(*s)) || rel.split('/').any(|c| c == ".git")
}

/// `rel` (under the root `canon`) into `files` when it resolves under the
/// root: a file, or a directory's files recursively. Missing: nothing.
fn add(canon: &Path, rel: &str, files: &mut BTreeMap<String, File>, size: &mut u64) -> Result<()> {
    let path = canon.join(rel);
    let Ok(real) = path.canonicalize() else { return Ok(()) };
    match real.strip_prefix(canon).ok().and_then(Path::to_str) {
        Some(r) if !skipped(r) => {}
        _ => return Ok(()),
    }
    let meta = std::fs::metadata(&real).with_context(|| format!("cannot read {}", path.display()))?;
    if meta.is_file() {
        *size += meta.len();
        if *size > ROOT_TAR_LIMIT {
            bail!("the config's bundle is over {ROOT_TAR_LIMIT} bytes");
        }
        let data = std::fs::read(&real).with_context(|| format!("cannot read {}", path.display()))?;
        files.insert(rel.to_string(), File { exec: meta.permissions().mode() & 0o100 != 0, data });
    } else if meta.is_dir() && !path.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) {
        let mut names: Vec<String> = Vec::new();
        for e in std::fs::read_dir(&path).with_context(|| format!("cannot list {}", path.display()))? {
            let e = e.with_context(|| format!("cannot list {}", path.display()))?;
            let name = e.file_name();
            names.push(name.to_str().with_context(|| format!("{} has a non-UTF-8 name", e.path().display()))?.to_string());
        }
        for name in names {
            let child = format!("{rel}/{name}");
            if !skipped(&child) {
                add(canon, &child, files, size)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("devsandbox-mesh-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    const CONFIG: &str = r#"
[template.base]
mounts = [
  "source=${configDir}/mcps/common.json,target=/m.json,type=bind,readonly",
  "source=${configDir}/missing.json,target=/x.json,type=bind",
  "source=${sharedVolumes}/state,target=/s,type=bind",
  "source=${configDir}/shared-volumes/other,target=/o,type=bind",
  "source=${configDir},target=/self,type=bind",
]

[sandbox.web]
extends = "base"
folder = "../proj"
folders = { "/workspaces/root" = "../root" }
build = { dockerfile = "dockerfiles/web/Dockerfile", context = "dockerfiles/web" }

[sandbox.web.features]
"./features/hello" = {}

[sandbox.far]
folder = "../proj"
build = { dockerfile = "dockerfiles/far/Dockerfile", context = "../outside" }
mounts = [{ source = "${configDir}/mcps/far.json", target = "/far.json", type = "bind" }]

[services.db]
build = { dockerfile = "dockerfiles/db/Dockerfile" }
"#;

    /// A root named `root` (so its self-mount via `folders` is `../root`)
    /// with every kind of reference, junk next to them, and a build
    /// context outside it.
    fn fixture(tag: &str) -> (PathBuf, PathBuf) {
        let base = scratch(tag);
        let root = base.join("root");
        write(&root, "devsandboxes.toml", CONFIG);
        write(&root, "mcps/common.json", "{}");
        write(&root, "mcps/far.json", "{\"far\":1}");
        write(&root, "dockerfiles/web/Dockerfile", "FROM alpine\n");
        write(&root, "dockerfiles/web/files/a.txt", "a");
        write(&root, "dockerfiles/far/Dockerfile", "FROM debian\n");
        write(&root, "dockerfiles/db/Dockerfile", "FROM postgres\n");
        write(&root, "features/hello/devcontainer-feature.json", "{\"id\":\"hello\"}");
        write(&root, "features/hello/install.sh", "#!/bin/sh\n");
        std::fs::set_permissions(root.join("features/hello/install.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        write(&root, "notes.md", "notes");
        write(&root, "dispatcher/dispatch.log", "log");
        write(&root, "shared-volumes/other/x", "state");
        write(&root, "mcps/.git/HEAD", "ref");
        write(&base, "outside/ctx.txt", "outside");
        (base, root)
    }

    fn hash(root: &Path) -> [u8; 32] {
        collect(root).unwrap().hash()
    }

    #[test]
    fn the_bundle_is_the_config_and_what_it_references() {
        let (base, root) = fixture("refs");
        let b = collect(&root).unwrap();
        let paths: Vec<&str> = b.files.keys().map(String::as_str).collect();
        assert_eq!(
            paths,
            [
                "dockerfiles/db/Dockerfile",
                "dockerfiles/far/Dockerfile",
                "dockerfiles/web/Dockerfile",
                "dockerfiles/web/files/a.txt",
                "features/hello/devcontainer-feature.json",
                "features/hello/install.sh",
                "mcps/common.json",
                "mcps/far.json",
            ],
            "no context outside the root, no root, no shared-volumes, no junk"
        );
        assert!(b.files["features/hello/install.sh"].exec);
        assert_eq!(b.config_name, "devsandboxes.toml");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_hash_ignores_formatting_and_unreferenced_files() {
        let (base, root) = fixture("same");
        let h = hash(&root);
        // Comments, whitespace and key order.
        let reordered = CONFIG.replace("[services.db]\nbuild = { dockerfile = \"dockerfiles/db/Dockerfile\" }\n", "");
        write(&root, "devsandboxes.toml", &format!("# a comment\n[services.db]\n  build = {{ dockerfile = \"dockerfiles/db/Dockerfile\" }}\n\n{reordered}"));
        assert_eq!(hash(&root), h, "formatting");
        write(&root, "notes.md", "other notes");
        write(&root, "dispatcher/dispatch.log", "more log");
        write(&root, "__pycache__/x.pyc", "junk");
        write(&root, "shared-volumes/other/x", "changed");
        write(&root, "shared-volumes/state/y", "new");
        write(&root, "mcps/.git/HEAD", "other");
        assert_eq!(hash(&root), h, "unreferenced files");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_hash_follows_structure_and_referenced_files() {
        let (base, root) = fixture("changes");
        let mut seen = vec![hash(&root)];
        let mut changed = |root: &Path, what: &str| {
            let h = hash(root);
            assert!(!seen.contains(&h), "{what}");
            seen.push(h);
        };
        write(&root, "devsandboxes.toml", &format!("{CONFIG}\n[sandbox.more]\nimage = \"alpine\"\n"));
        changed(&root, "a structural edit");
        write(&root, "mcps/common.json", "{\"a\":1}");
        changed(&root, "a ${configDir} mount");
        write(&root, "dockerfiles/web/Dockerfile", "FROM alpine:3\n");
        changed(&root, "a dockerfile");
        write(&root, "dockerfiles/web/files/a.txt", "b");
        changed(&root, "a file in a build context");
        write(&root, "features/hello/install.sh", "#!/bin/sh\necho hi\n");
        changed(&root, "a local feature");
        std::fs::set_permissions(root.join("mcps/common.json"), std::fs::Permissions::from_mode(0o755)).unwrap();
        changed(&root, "an exec bit");
        write(&root, "outside/ignored", "x");
        write(&base, "outside/ctx.txt", "changed");
        assert_eq!(hash(&root), *seen.last().unwrap(), "a context outside the root");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_symlink_out_of_the_root_is_left_out() {
        let (base, root) = fixture("links");
        std::os::unix::fs::symlink(base.join("outside/ctx.txt"), root.join("dockerfiles/web/files/out.txt")).unwrap();
        std::os::unix::fs::symlink("../../../notes.md", root.join("dockerfiles/web/files/notes.md")).unwrap();
        let b = collect(&root).unwrap();
        assert!(!b.files.contains_key("dockerfiles/web/files/out.txt"));
        assert_eq!(b.files["dockerfiles/web/files/notes.md"].data, b"notes", "a link inside the root, followed");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_stored_tar_unpacks_to_the_same_hash() {
        let (base, root) = fixture("tar");
        let b = collect(&root).unwrap();
        let data = base.join("data");
        let path = store(&data, &b).unwrap();
        assert_eq!(path, data.join(format!("mesh/bundles/{}.tar", hex::encode(b.hash()))));
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let tar = std::fs::read(&path).unwrap();
        assert_eq!(tar, b.tar().unwrap(), "deterministic");
        // Unpacked where the root's self-mount still names it.
        let copy = base.join("copy/root");
        std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
        tar::extract(&tar, &copy).unwrap();
        assert_eq!(hash(&copy), b.hash());
        assert_eq!(std::fs::read_to_string(copy.join("devsandboxes.toml")).unwrap(), CONFIG, "the config as written");
        assert!(!copy.join("notes.md").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_name_binds_to_its_first_path() {
        let mut roots = BTreeMap::new();
        assert_eq!(bind_in(&mut roots, "cfg", Path::new("/a/cfg")), Ok(()));
        assert_eq!(bind_in(&mut roots, "cfg", Path::new("/a/cfg")), Ok(()));
        assert_eq!(bind_in(&mut roots, "cfg", Path::new("/b/cfg")), Err(PathBuf::from("/a/cfg")));
        assert_eq!(bind_in(&mut roots, "other", Path::new("/b/other")), Ok(()));
        assert_eq!(roots.len(), 2);
    }

    #[test]
    fn the_bindings_round_trip() {
        let data = scratch("roots");
        assert!(load_roots(&roots_path(&data)).unwrap().is_empty(), "a missing file is empty");
        assert_eq!(bound_root(&data, "cfg").unwrap(), None);
        assert_eq!(bind(&data, "cfg", Path::new("/a/cfg")).unwrap(), Ok(()));
        assert_eq!(bind(&data, "x.y", Path::new("/a/x.y")).unwrap(), Ok(()));
        let text = std::fs::read_to_string(roots_path(&data)).unwrap();
        assert!(text.contains("cfg = \"/a/cfg\""), "{text}");
        assert_eq!(bind(&data, "cfg", Path::new("/b/cfg")).unwrap(), Err(PathBuf::from("/a/cfg")));
        assert_eq!(bound_root(&data, "cfg").unwrap(), Some(PathBuf::from("/a/cfg")));
        assert_eq!(bound_root(&data, "x.y").unwrap(), Some(PathBuf::from("/a/x.y")));
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn a_config_that_wont_load_has_no_bundle() {
        let (base, root) = fixture("bad");
        write(&root, "devsandboxes.toml", "[sandbox.a\n");
        assert!(collect(&root).is_err());
        write(&root, "devsandboxes.toml", "[sandbox.a]\nextends = \"nope\"\n");
        assert!(format!("{:#}", collect(&root).unwrap_err()).contains("nope"));
        let _ = std::fs::remove_dir_all(&base);
    }
}
