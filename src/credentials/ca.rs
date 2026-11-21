//! The member CA (credential-broker, *The member CA*): the CA a container
//! trusts for the hosts the broker intercepts, and the per-host leaves the
//! broker's TLS presents. ECDSA P-256 rather than the member's ed25519
//! identity: browsers and other verifiers refuse Ed25519 certificates.
//!
//! Two files beside `member.key`, both written under `member-ca.lock`
//! (`serve::locked_file`): the key `member-ca.key` (PKCS#8 PEM, created
//! once, temp file created 0600 and renamed; refused if group or others
//! can reach it) and `member-ca.toml`, the current cert with the host set
//! it was issued for. Both are kept out of every container by `key_guard`:
//! the cert is public, but a container that could write it would get its
//! own CA installed in the others.
//!
//! The cert is name-constrained to its host set (permitted DNS subtrees;
//! every IP address excluded, so an IP SAN never slips past). A new host
//! reissues it over the same key, so a leaf minted before still chains to
//! the new cert, which replaces the old one in place. The set only grows
//! while the daemon runs; a host no claim intercepts any more stays until
//! the file is removed. An empty set has no cert at all: a CA without
//! constraints would vouch for every name.

use std::collections::{BTreeSet, HashMap};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use rcgen::{
    BasicConstraints, CertificateParams, CidrSubnet, DistinguishedName, DnType, ExtendedKeyUsagePurpose, GeneralSubtree, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, NameConstraints, PKCS_ECDSA_P256_SHA256, SerialNumber,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde::{Deserialize, Serialize};

use crate::daemon_config::{MEMBER_CA_CERT, MEMBER_CA_KEY};

const DAY: Duration = Duration::from_secs(86_400);
/// How far a container's clock may lag the host's: every cert starts this
/// long before it is made.
pub const SKEW: Duration = DAY;
pub const LEAF_LIFETIME: Duration = Duration::from_secs(7 * 86_400);
/// A cached leaf with less left than this is minted again.
pub const LEAF_RENEW: Duration = DAY;
pub const CA_LIFETIME: Duration = Duration::from_secs(3650 * 86_400);
/// A CA cert with less left than this is reissued at the next load or
/// `ensure_hosts`.
pub const CA_RENEW: Duration = Duration::from_secs(30 * 86_400);

const CA_NAME: &str = "devsandbox member CA";

/// `member-ca.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaFile {
    /// The key the cert was issued over (hex of its raw public key): a
    /// replaced `member-ca.key` reissues the cert.
    public_key: String,
    hosts: BTreeSet<String>,
    /// Unix seconds.
    not_after: u64,
    cert: String,
}

/// A minted leaf: the chain to present (the leaf alone: containers have
/// the CA as an anchor) and its key.
pub struct Leaf {
    pub chain: Vec<CertificateDer<'static>>,
    pub key: PrivateKeyDer<'static>,
}

impl Clone for Leaf {
    fn clone(&self) -> Leaf {
        Leaf { chain: self.chain.clone(), key: self.key.clone_key() }
    }
}

struct Cached {
    leaf: Leaf,
    not_after: SystemTime,
}

pub struct MemberCa {
    dir: PathBuf,
    key: KeyPair,
    issued: Option<CaFile>,
    leaves: Mutex<HashMap<String, Cached>>,
}

impl MemberCa {
    /// The CA in the data dir `data` (`<data>/devsandbox`), its key made on
    /// first use; a cert past `CA_RENEW` or issued over another key is
    /// reissued for the hosts it had.
    pub fn load_or_create(data: &Path) -> Result<MemberCa> {
        MemberCa::load_or_create_at(data, SystemTime::now())
    }

    fn load_or_create_at(data: &Path, now: SystemTime) -> Result<MemberCa> {
        std::fs::create_dir_all(data).with_context(|| format!("cannot create {}", data.display()))?;
        let _lock = lock(data)?;
        let key_path = data.join(MEMBER_CA_KEY);
        let key = match read_private(&key_path)? {
            Some(pem) => {
                let key = KeyPair::from_pem(&pem).map_err(|_| anyhow!("{} doesn't parse as a private key", key_path.display()))?;
                if key.algorithm() != &PKCS_ECDSA_P256_SHA256 {
                    bail!("{} isn't an ECDSA P-256 key", key_path.display());
                }
                key
            }
            None => {
                let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).context("cannot make the member CA key")?;
                crate::serve::locked_file::write_atomic_mode(&key_path, &key.serialize_pem(), 0o600)?;
                key
            }
        };
        let on_disk = read_cert_file(data)?;
        let issued = reconcile(&key, on_disk.clone(), &BTreeSet::new(), now)?;
        if issued != on_disk {
            write_cert_file(data, issued.as_ref())?;
        }
        Ok(MemberCa { dir: data.to_path_buf(), key, issued, leaves: Mutex::default() })
    }

    /// The hosts the current cert covers; empty: no cert.
    pub fn hosts(&self) -> BTreeSet<String> {
        self.issued.as_ref().map(|f| f.hosts.clone()).unwrap_or_default()
    }

    /// What a container installs as its anchor; `None` while no host is
    /// covered.
    // Installed in containers at create (credential-broker step 3, run wiring).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn cert_pem(&self) -> Option<&str> {
        self.issued.as_ref().map(|f| f.cert.as_str())
    }

    /// Covers `hosts` too (in [`check_host`]'s form), reissuing the cert
    /// when the set grows; whether the cert changed. The set on disk counts
    /// as well, so a second daemon's hosts aren't dropped.
    pub fn ensure_hosts(&mut self, hosts: &BTreeSet<String>) -> Result<bool> {
        self.ensure_hosts_at(hosts, SystemTime::now())
    }

    fn ensure_hosts_at(&mut self, hosts: &BTreeSet<String>, now: SystemTime) -> Result<bool> {
        let _lock = lock(&self.dir)?;
        let on_disk = read_cert_file(&self.dir)?;
        let issued = reconcile(&self.key, on_disk.clone(), hosts, now)?;
        if issued != on_disk {
            write_cert_file(&self.dir, issued.as_ref())?;
        }
        let changed = issued != self.issued;
        self.issued = issued;
        Ok(changed)
    }

    /// `host`'s leaf, cached until `LEAF_RENEW` before it expires. Only a
    /// host in the set: the constraints also admit its subdomains, the
    /// broker never mints for them.
    pub fn leaf(&self, host: &str) -> Result<Leaf> {
        self.leaf_at(host, SystemTime::now())
    }

    fn leaf_at(&self, host: &str, now: SystemTime) -> Result<Leaf> {
        if !self.issued.as_ref().is_some_and(|f| f.hosts.contains(host)) {
            bail!("the member CA doesn't cover `{host}`");
        }
        let mut leaves = self.leaves.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = leaves.get(host)
            && c.not_after.duration_since(now).is_ok_and(|left| left > LEAF_RENEW)
        {
            return Ok(c.leaf.clone());
        }
        let leaf = self.mint(host, now)?;
        leaves.insert(host.to_string(), Cached { leaf: leaf.clone(), not_after: now + LEAF_LIFETIME });
        Ok(leaf)
    }

    /// Unchecked against the host set.
    fn mint(&self, host: &str, now: SystemTime) -> Result<Leaf> {
        let mut params = CertificateParams::new(vec![host.to_string()]).with_context(|| format!("`{host}` as a certificate name"))?;
        params.distinguished_name = name(host);
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        params.serial_number = Some(serial()?);
        validity(&mut params, now - SKEW, now + LEAF_LIFETIME)?;
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).context("cannot make a leaf key")?;
        let issuer = Issuer::new(ca_params(&BTreeSet::new()), &self.key);
        let cert = params.signed_by(&key, &issuer).context("cannot sign a leaf")?;
        Ok(Leaf { chain: vec![cert.into()], key: PrivatePkcs8KeyDer::from(key.serialize_der()).into() })
    }
}

/// A host as the CA covers it: lowercase DNS labels, no trailing dot, not
/// an IP address. The broker case-folds and drops the dot before asking.
pub fn check_host(host: &str) -> Result<()> {
    let label_ok = |l: &str| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    let last = host.rsplit('.').next().unwrap_or_default();
    if host.len() > 253 || !host.split('.').all(label_ok) || last.bytes().all(|b| b.is_ascii_digit()) {
        bail!("`{host}` isn't a lowercase DNS name the member CA can cover");
    }
    Ok(())
}

/// The cert `current` should become for `want` on top of its hosts: the
/// same when it covers them, is over `key` and has `CA_RENEW` left.
fn reconcile(key: &KeyPair, current: Option<CaFile>, want: &BTreeSet<String>, now: SystemTime) -> Result<Option<CaFile>> {
    let mut hosts = current.as_ref().map(|f| f.hosts.clone()).unwrap_or_default();
    hosts.extend(want.iter().cloned());
    for host in &hosts {
        check_host(host)?;
    }
    if hosts.is_empty() {
        return Ok(None);
    }
    let public_key = hex::encode(key.public_key_raw());
    if let Some(f) = &current
        && f.hosts == hosts
        && f.public_key == public_key
        && f.not_after > unix(now + CA_RENEW)
    {
        return Ok(current);
    }
    let not_after = now + CA_LIFETIME;
    let mut params = ca_params(&hosts);
    params.serial_number = Some(serial()?);
    validity(&mut params, now - SKEW, not_after)?;
    let cert = params.self_signed(key).context("cannot sign the member CA cert")?;
    Ok(Some(CaFile { public_key, hosts, not_after: unix(not_after), cert: cert.pem() }))
}

/// The CA's subject, usages and constraints; also what leaves are issued
/// under (rcgen's `Issuer` takes the subject and usages from it).
fn ca_params(hosts: &BTreeSet<String>) -> CertificateParams {
    let mut params = CertificateParams::default();
    params.distinguished_name = name(CA_NAME);
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.name_constraints = Some(NameConstraints {
        permitted_subtrees: hosts.iter().map(|h| GeneralSubtree::DnsName(h.clone())).collect(),
        excluded_subtrees: vec![
            GeneralSubtree::IpAddress(CidrSubnet::from_v4_prefix([0; 4], 0)),
            GeneralSubtree::IpAddress(CidrSubnet::from_v6_prefix([0; 16], 0)),
        ],
    });
    params
}

fn name(cn: &str) -> DistinguishedName {
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, cn);
    dn
}

/// rcgen's times are `time::OffsetDateTime`, reached from its epoch
/// rather than through a direct `time` dependency.
fn validity(params: &mut CertificateParams, from: SystemTime, to: SystemTime) -> Result<()> {
    let since = |t: SystemTime| t.duration_since(UNIX_EPOCH).context("a time before 1970");
    params.not_before = rcgen::date_time_ymd(1970, 1, 1) + since(from)?;
    params.not_after = rcgen::date_time_ymd(1970, 1, 1) + since(to)?;
    Ok(())
}

/// 127 random bits, positive as DER wants.
fn serial() -> Result<SerialNumber> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| anyhow!("cannot read the OS random source: {e}"))?;
    bytes[0] &= 0x7f;
    Ok(SerialNumber::from_slice(&bytes))
}

fn unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn lock(data: &Path) -> Result<std::fs::File> {
    crate::serve::locked_file::lock_at(&data.join(MEMBER_CA_KEY).with_extension("lock"))
}

/// A missing file is `None`; one group or others can reach is refused.
fn read_private(path: &Path) -> Result<Option<String>> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    let mode = file.metadata().with_context(|| format!("cannot read {}", path.display()))?.permissions().mode();
    if mode & 0o077 != 0 {
        bail!(
            "{} is reachable by group or others (mode {:04o}): the member CA key must be 0600 (`chmod 600 {}`)",
            path.display(),
            mode & 0o7777,
            path.display()
        );
    }
    let mut text = String::new();
    file.read_to_string(&mut text).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(Some(text))
}

fn read_cert_file(data: &Path) -> Result<Option<CaFile>> {
    let path = data.join(MEMBER_CA_CERT);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    toml::from_str(&text).map(Some).with_context(|| format!("{} doesn't parse", path.display()))
}

fn write_cert_file(data: &Path, file: Option<&CaFile>) -> Result<()> {
    let path = data.join(MEMBER_CA_CERT);
    match file {
        Some(file) => {
            let text = toml::to_string(file).context("cannot serialize the member CA cert")?;
            crate::serve::locked_file::write_atomic_mode(&path, &text, 0o644)
        }
        None => match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e).with_context(|| format!("cannot remove {}", path.display())),
            _ => Ok(()),
        },
    }
}

#[cfg(test)]
mod tests;
