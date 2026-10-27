//! A config root as a tar (docs/architecture/daemon/fleet.md, *Moving
//! code*): written and read here, never by a `tar` program, so the bytes
//! are the same on every host (members sorted, mtime 0, owner 0, modes
//! 0644 / 0755 by the exec bit) and what a receiver accepts is decided
//! member by member in this file.
//!
//! The format is ustar with pax `path` / `linkpath` records for names over
//! 100 bytes; members are directories, regular files and symlinks only.
//! [`read`] refuses anything else: another type (hard links, devices,
//! global pax headers), a pax key other than those two, a base-256 number,
//! a bad checksum, an absolute path, a `.` / `..` / empty component, and a
//! symlink whose target isn't [`check_link`]'s shape. [`extract`] then
//! never writes through a symlink (every parent must be a real directory
//! it made) and never over an existing member.

use std::fs::File;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result, bail};

const BLOCK: usize = 512;

/// At most this many members in a tar [`read`] takes.
pub const MEMBERS_MAX: usize = 200_000;

/// One member: its path (relative, `/`-separated, no trailing `/`) and what
/// it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member<'a> {
    pub path: String,
    pub kind: Kind<'a>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind<'a> {
    Dir,
    File { exec: bool, data: &'a [u8] },
    Link(String),
}

/// A tar being written, in memory.
#[derive(Default)]
pub struct Writer {
    out: Vec<u8>,
}

impl Writer {
    pub fn dir(&mut self, path: &str) -> Result<()> {
        check_path(path)?;
        self.header(&format!("{path}/"), b'5', 0o755, 0, "");
        Ok(())
    }

    pub fn file(&mut self, path: &str, exec: bool, data: &[u8]) -> Result<()> {
        check_path(path)?;
        self.header(path, b'0', if exec { 0o755 } else { 0o644 }, data.len() as u64, "");
        self.out.extend_from_slice(data);
        self.pad();
        Ok(())
    }

    pub fn link(&mut self, path: &str, target: &str) -> Result<()> {
        check_path(path)?;
        check_link(path, target)?;
        self.header(path, b'2', 0o777, 0, target);
        Ok(())
    }

    /// The tar's bytes: two zero blocks end it.
    pub fn finish(mut self) -> Vec<u8> {
        self.out.extend_from_slice(&[0; 2 * BLOCK]);
        self.out
    }

    pub fn len(&self) -> usize {
        self.out.len()
    }

    fn pad(&mut self) {
        let rem = self.out.len() % BLOCK;
        if rem != 0 {
            self.out.resize(self.out.len() + BLOCK - rem, 0);
        }
    }

    fn header(&mut self, name: &str, typeflag: u8, mode: u32, size: u64, link: &str) {
        let mut pax = String::new();
        if name.len() > 100 {
            pax.push_str(&pax_record("path", name));
        }
        if link.len() > 100 {
            pax.push_str(&pax_record("linkpath", link));
        }
        if !pax.is_empty() {
            self.out.extend_from_slice(&raw_header("././@PaxHeader", b'x', 0o644, pax.len() as u64, ""));
            self.out.extend_from_slice(pax.as_bytes());
            self.pad();
        }
        self.out.extend_from_slice(&raw_header(name, typeflag, mode, size, link));
    }
}

/// `<len> key=value\n`, `len` counting itself.
fn pax_record(key: &str, value: &str) -> String {
    let rest = key.len() + value.len() + 3;
    let mut len = rest + 1;
    while len != rest + len.to_string().len() {
        len = rest + len.to_string().len();
    }
    format!("{len} {key}={value}\n")
}

/// One ustar header block (long names cut: the pax record before it says
/// the real one).
fn raw_header(name: &str, typeflag: u8, mode: u32, size: u64, link: &str) -> [u8; BLOCK] {
    let mut h = [0u8; BLOCK];
    let put = |h: &mut [u8; BLOCK], at: usize, len: usize, bytes: &[u8]| {
        let n = bytes.len().min(len);
        h[at..at + n].copy_from_slice(&bytes[..n]);
    };
    put(&mut h, 0, 100, name.as_bytes());
    put(&mut h, 100, 8, format!("{mode:07o}\0").as_bytes());
    put(&mut h, 108, 8, b"0000000\0");
    put(&mut h, 116, 8, b"0000000\0");
    put(&mut h, 124, 12, format!("{size:011o}\0").as_bytes());
    put(&mut h, 136, 12, b"00000000000\0");
    h[156] = typeflag;
    put(&mut h, 157, 100, link.as_bytes());
    put(&mut h, 257, 6, b"ustar\0");
    put(&mut h, 263, 2, b"00");
    h[148..156].copy_from_slice(b"        ");
    let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
    put(&mut h, 148, 8, format!("{sum:06o}\0 ").as_bytes());
    h
}

/// A member path: relative, `/`-separated, every component plain (not
/// empty, `.` or `..`), no NUL.
pub fn check_path(path: &str) -> Result<()> {
    let plain = |c: &str| !c.is_empty() && c != "." && c != ".." && !c.contains('\0');
    if path.is_empty() || path.starts_with('/') || !path.split('/').all(plain) {
        bail!("unsafe path {path:?} in the config root's tar");
    }
    Ok(())
}

/// A symlink at `path` may point at `target` when `target` is relative, its
/// `..` components all come first, no more of them than `path` has parents
/// (so it never leaves the tree), and the rest are plain. Then, as the
/// kernel resolves `..` from the link's real directory and every link is
/// checked so, following links never leaves the tree either.
pub fn check_link(path: &str, target: &str) -> Result<()> {
    let refuse = || anyhow::anyhow!("symlink {path} points outside the config root ({target})");
    if target.is_empty() || target.starts_with('/') || target.contains('\0') {
        return Err(refuse());
    }
    let depth = path.split('/').count() - 1;
    let mut ups = 0;
    let mut down = false;
    for c in target.split('/') {
        match c {
            ".." if !down => ups += 1,
            "" | "." | ".." => return Err(refuse()),
            _ => down = true,
        }
    }
    if ups > depth {
        return Err(refuse());
    }
    Ok(())
}

/// The members of `tar` (as [`Writer`] writes it), each checked (see the
/// module docs).
pub fn read(tar: &[u8]) -> Result<Vec<Member<'_>>> {
    let mut members = Vec::new();
    let mut at = 0;
    let (mut pax_path, mut pax_link) = (None, None);
    loop {
        let Some(h) = tar.get(at..at + BLOCK) else { bail!("the tar ends without its end blocks") };
        at += BLOCK;
        if h.iter().all(|&b| b == 0) {
            return Ok(members);
        }
        let stored = octal(&h[148..156]).context("bad tar checksum field")?;
        let sum: u64 = h.iter().enumerate().map(|(i, &b)| if (148..156).contains(&i) { 32 } else { u64::from(b) }).sum();
        if stored != sum {
            bail!("bad tar checksum");
        }
        let size = octal(&h[124..136]).context("bad tar size field")? as usize;
        let data = tar.get(at..at.checked_add(size).context("bad tar size")?).context("the tar ends inside a member")?;
        at += size.div_ceil(BLOCK) * BLOCK;
        let typeflag = h[156];
        if typeflag == b'x' {
            for (key, value) in pax_records(data)? {
                match key {
                    "path" => pax_path = Some(value.to_string()),
                    "linkpath" => pax_link = Some(value.to_string()),
                    _ => bail!("tar pax key `{key}` isn't taken"),
                }
            }
            continue;
        }
        let name = match pax_path.take() {
            Some(p) => p,
            None => {
                let (prefix, name) = (field(&h[345..500])?, field(&h[0..100])?);
                if prefix.is_empty() { name.to_string() } else { format!("{prefix}/{name}") }
            }
        };
        let link = match pax_link.take() {
            Some(l) => l,
            None => field(&h[157..257])?.to_string(),
        };
        let path = match typeflag {
            b'5' => name.strip_suffix('/').unwrap_or(&name).to_string(),
            _ => name,
        };
        check_path(&path)?;
        let mode = octal(&h[100..108]).context("bad tar mode field")?;
        let kind = match typeflag {
            b'5' => Kind::Dir,
            b'0' | 0 => Kind::File { exec: mode & 0o100 != 0, data },
            b'2' => {
                check_link(&path, &link)?;
                Kind::Link(link)
            }
            t => bail!("tar member {path} is of type `{}`, which isn't taken", t as char),
        };
        if members.len() >= MEMBERS_MAX {
            bail!("the tar has more than {MEMBERS_MAX} members");
        }
        members.push(Member { path, kind });
    }
}

/// A NUL-terminated (or full) text field.
fn field(bytes: &[u8]) -> Result<&str> {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end]).context("a tar name isn't UTF-8")
}

/// An octal number field (NULs and spaces around it); base-256 refused.
fn octal(bytes: &[u8]) -> Result<u64> {
    if bytes.first().is_some_and(|b| b & 0x80 != 0) {
        bail!("base-256 numbers aren't taken");
    }
    let text = std::str::from_utf8(bytes)?.trim_matches(|c| c == '\0' || c == ' ');
    if text.is_empty() {
        return Ok(0);
    }
    Ok(u64::from_str_radix(text, 8)?)
}

fn pax_records(data: &[u8]) -> Result<Vec<(&str, &str)>> {
    let mut text = std::str::from_utf8(data).context("a pax header isn't UTF-8")?;
    let mut out = Vec::new();
    while !text.is_empty() {
        let (len, _) = text.split_once(' ').context("bad pax record")?;
        let len: usize = len.parse().context("bad pax record length")?;
        let record = text.get(..len).context("bad pax record length")?;
        text = &text[len..];
        let body = record.strip_suffix('\n').context("bad pax record")?;
        let (_, kv) = body.split_once(' ').context("bad pax record")?;
        out.push(kv.split_once('=').context("bad pax record")?);
    }
    Ok(out)
}

/// Write `tar`'s members into `into`, a directory this creates (it must not
/// exist yet). Parents are made as real directories (0755); a parent that
/// is anything else (a symlink a member made) or a member that exists
/// already is refused. Files are 0644 or 0755 by their exec bit.
pub fn extract(tar: &[u8], into: &Path) -> Result<()> {
    let members = read(tar)?;
    std::fs::create_dir(into).with_context(|| format!("cannot create {}", into.display()))?;
    for m in members {
        let mut parts: Vec<&str> = m.path.split('/').collect();
        let last = parts.pop().expect("a checked path has a component");
        let mut dir = into.to_path_buf();
        for p in parts {
            dir.push(p);
            real_dir(&dir, &m.path)?;
        }
        let at = dir.join(last);
        match &m.kind {
            Kind::Dir => real_dir(&at, &m.path)?,
            Kind::File { exec, data } => {
                let mut f = File::options()
                    .write(true)
                    .create_new(true)
                    .mode(if *exec { 0o755 } else { 0o644 })
                    .open(&at)
                    .with_context(|| format!("cannot create {} (a duplicate member?)", m.path))?;
                f.write_all(data).with_context(|| format!("cannot write {}", at.display()))?;
            }
            Kind::Link(target) => std::os::unix::fs::symlink(target, &at)
                .with_context(|| format!("cannot create symlink {} (a duplicate member?)", m.path))?,
        }
    }
    Ok(())
}

/// `dir` is a real directory: made if missing, refused if it's anything
/// else.
fn real_dir(dir: &Path, member: &str) -> Result<()> {
    match dir.symlink_metadata() {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => bail!("tar member {member} goes through {}, which isn't a directory", dir.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(dir).with_context(|| format!("cannot create {}", dir.display()))
        }
        Err(e) => Err(e).with_context(|| format!("cannot read {}", dir.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("devsandbox-tar-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn members_round_trip_with_long_names() {
        let long = format!("cfg/{}/file", "d".repeat(150));
        let target = format!("../{}", "t".repeat(120));
        let mut w = Writer::default();
        w.dir("cfg").unwrap();
        w.file("cfg/devsandboxes.toml", false, b"[sandbox.web]\n").unwrap();
        w.file("cfg/run.sh", true, b"#!/bin/sh\n").unwrap();
        w.file(&long, false, &[7; 1000]).unwrap();
        w.link("cfg/l", "run.sh").unwrap();
        w.link("cfg/sub/far", &target).unwrap();
        let tar = w.finish();
        assert_eq!(tar.len() % BLOCK, 0);
        let got = read(&tar).unwrap();
        let paths: Vec<_> = got.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(paths, ["cfg", "cfg/devsandboxes.toml", "cfg/run.sh", long.as_str(), "cfg/l", "cfg/sub/far"]);
        assert_eq!(got[1].kind, Kind::File { exec: false, data: b"[sandbox.web]\n" });
        assert_eq!(got[2].kind, Kind::File { exec: true, data: b"#!/bin/sh\n" });
        assert_eq!(got[4].kind, Kind::Link("run.sh".into()));
        assert_eq!(got[5].kind, Kind::Link(target));
        // GNU tar reads it too (where installed): the format is plain ustar + pax.
        let dir = scratch("gnu");
        std::fs::write(dir.join("t.tar"), &tar).unwrap();
        if let Ok(out) = std::process::Command::new("tar").arg("-tf").arg(dir.join("t.tar")).output() {
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            assert!(String::from_utf8_lossy(&out.stdout).contains(&long));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pax_record_lengths_count_themselves() {
        assert_eq!(pax_record("path", "a"), "9 path=a\n");
        // The length's own digits roll over to two.
        assert_eq!(pax_record("path", "ab"), "11 path=ab\n");
        let r = pax_record("path", &"x".repeat(91));
        assert_eq!(r.len(), r.split(' ').next().unwrap().parse::<usize>().unwrap());
    }

    #[test]
    fn unsafe_paths_and_links_are_refused() {
        for bad in ["", "/etc/passwd", "../x", "a/../../x", "a//b", "./a", "a/.", "a/"] {
            assert!(check_path(bad).is_err(), "{bad:?}");
        }
        check_path("a/b.c/.hidden").unwrap();
        for (path, target) in [("l", "../x"), ("a/l", "../../x"), ("a/l", "/etc"), ("a/l", "b/../.."), ("a/l", "./b"), ("a/l", "")] {
            assert!(check_link(path, target).is_err(), "{path} -> {target}");
        }
        for (path, target) in [("l", "x"), ("a/l", "../x"), ("a/b/l", "../../c/d"), ("a/l", "..")] {
            check_link(path, target).unwrap();
        }
        let mut w = Writer::default();
        assert!(w.link("l", "../out").is_err());
        assert!(w.file("../x", false, b"").is_err());
    }

    /// A tar written by hand, members [`Writer`] won't write.
    fn hand(name: &str, typeflag: u8, link: &str) -> Vec<u8> {
        let mut t = raw_header(name, typeflag, 0o644, 0, link).to_vec();
        t.extend_from_slice(&[0; 2 * BLOCK]);
        t
    }

    #[test]
    fn read_refuses_what_writer_never_writes() {
        let err = |t: &[u8]| format!("{:#}", read(t).unwrap_err());
        assert!(err(&hand("/etc/passwd", b'0', "")).contains("unsafe path"));
        assert!(err(&hand("a/../../x", b'0', "")).contains("unsafe path"));
        assert!(err(&hand("..", b'5', "")).contains("unsafe path"));
        assert!(err(&hand("l", b'2', "/etc")).contains("points outside"));
        assert!(err(&hand("a/l", b'2', "../../x")).contains("points outside"));
        assert!(err(&hand("h", b'1', "x")).contains("type `1`"));
        assert!(err(&hand("d", b'3', "")).contains("type `3`"));
        assert!(err(&hand("g", b'g', "")).contains("type `g`"));
        let mut bad_sum = hand("x", b'0', "");
        bad_sum[0] = b'y';
        assert!(err(&bad_sum).contains("checksum"));
        assert!(err(&[0; 10]).contains("ends without"));
        // A pax `size` (or any key but the two) is refused.
        let mut t = Vec::new();
        let rec = pax_record("size", "1");
        t.extend_from_slice(&raw_header("p", b'x', 0o644, rec.len() as u64, ""));
        t.extend_from_slice(rec.as_bytes());
        t.resize(2 * BLOCK, 0);
        t.extend_from_slice(&hand("f", b'0', ""));
        assert!(err(&t).contains("pax key `size`"));
    }

    #[test]
    fn extract_writes_members_and_never_through_a_link() {
        let dir = scratch("extract");
        let mut w = Writer::default();
        w.dir("cfg").unwrap();
        w.file("cfg/a", false, b"a").unwrap();
        w.file("cfg/x/run", true, b"#!").unwrap();
        w.link("cfg/l", "x").unwrap();
        extract(&w.finish(), &dir.join("out")).unwrap();
        let out = dir.join("out/cfg");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::read(out.join("a")).unwrap(), b"a");
        assert!(std::fs::metadata(out.join("x/run")).unwrap().permissions().mode() & 0o111 != 0);
        assert_eq!(std::fs::read_link(out.join("l")).unwrap(), Path::new("x"));
        assert!(extract(&Writer::default().finish(), &dir.join("out")).is_err(), "into an existing dir");

        // A link then a member through it: refused, nothing written past it.
        let mut w = Writer::default();
        w.dir("d").unwrap();
        w.link("l", "d").unwrap();
        w.file("l/f", false, b"x").unwrap();
        let err = format!("{:#}", extract(&w.finish(), &dir.join("through")).unwrap_err());
        assert!(err.contains("isn't a directory"), "{err}");
        assert!(!dir.join("through/d/f").exists());
        // A duplicate file: refused.
        let mut w = Writer::default();
        w.file("f", false, b"1").unwrap();
        w.file("f", false, b"2").unwrap();
        assert!(format!("{:#}", extract(&w.finish(), &dir.join("dup")).unwrap_err()).contains("duplicate"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
