//! Member identity (docs/architecture/daemon/fleet.md, *Member identity*;
//! fleet plan step 10a, decisions 44-52): this member's ed25519 key, the
//! text forms of public keys and signatures, domain-separated signing, and
//! signed [`Statement`]s with the rollback check ([`Seen`]).
//!
//! The private key sits behind [`KeyStore`]; [`FileKeyStore`] is
//! `member.key` next to `member.toml` (`member::key_path`), mode 0600,
//! created once under its own lock; a pending rotation's key waits in
//! `member.key.next` until the membership log lists it. `member.get` answers its public half;
//! `membership.*` signs the membership log's entries and introductions with
//! it (`super::membership_api`); a peer link proves it with [`Signer`] and
//! [`PublicKey::verify`] (`super::proof`).

// `Seen` has no caller yet (a link's proof is per-link nonces, not
// versions); its tests are here.
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use ed25519_dalek::Signer as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

/// The domain a [`Statement`]'s signature is made in.
pub const STATEMENT_DOMAIN: &str = "devsandbox/statement/v1";

/// A public key's text form's prefix: `ed25519:<64 lowercase hex>`.
const KEY_PREFIX: &str = "ed25519:";
/// `member.key`'s one line's prefix: `ed25519-seed:<64 lowercase hex>`.
const SEED_PREFIX: &str = "ed25519-seed:";
/// A fingerprint's prefix.
const FINGERPRINT_PREFIX: &str = "SHA256:";

/// The bytes a signature in `domain` covers: `domain || 0x00 || message`.
/// A domain is a constant with no NUL, so the split is unambiguous and a
/// signature made for one use never verifies for another.
fn domain_bytes(domain: &str, message: &[u8]) -> Vec<u8> {
    debug_assert!(!domain.as_bytes().contains(&0), "a domain has no NUL: {domain:?}");
    let mut out = Vec::with_capacity(domain.len() + 1 + message.len());
    out.extend_from_slice(domain.as_bytes());
    out.push(0);
    out.extend_from_slice(message);
    out
}

/// `text` as exactly `N` bytes of hex (either case).
fn hex_array<const N: usize>(text: &str) -> Result<[u8; N]> {
    let mut out = [0u8; N];
    hex::decode_to_slice(text, &mut out).map_err(|e| anyhow!("expected {} hex digits: {e}", 2 * N))?;
    Ok(out)
}

/// An ed25519 public key, a member's identity. Text form (`Display`,
/// `FromStr`, serde): `ed25519:<64 lowercase hex>`; parsing refuses bytes
/// that aren't a point on the curve.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PublicKey([u8; 32]);

impl PublicKey {
    pub fn from_bytes(bytes: [u8; 32]) -> Result<PublicKey> {
        ed25519_dalek::VerifyingKey::from_bytes(&bytes).map_err(|_| anyhow!("not an ed25519 public key"))?;
        Ok(PublicKey(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// What a user compares by eye: `SHA256:` and the first 16 bytes (128
    /// bits) of the sha256 of the raw key, in lowercase hex, in eight groups
    /// of four joined by `-` (`SHA256:3f2a-9b01-…`). Hex since no base64
    /// dependency is direct; truncated since 128 bits is beyond any
    /// second-preimage search and half the length is easier to read aloud.
    pub fn fingerprint(&self) -> String {
        let digest = Sha256::digest(self.0);
        let hex = hex::encode(&digest[..16]);
        let groups: Vec<&str> = (0..8).map(|i| &hex[4 * i..4 * i + 4]).collect();
        format!("{FINGERPRINT_PREFIX}{}", groups.join("-"))
    }

    /// Whether `sig` is this key's over `message` in `domain`. Strict
    /// verification (no small-order keys, canonical signatures).
    pub fn verify(&self, domain: &'static str, message: &[u8], sig: &Signature) -> Result<()> {
        let key = ed25519_dalek::VerifyingKey::from_bytes(&self.0).map_err(|_| anyhow!("not an ed25519 public key"))?;
        let sig = ed25519_dalek::Signature::from_bytes(&sig.0);
        key.verify_strict(&domain_bytes(domain, message), &sig)
            .map_err(|_| anyhow!("bad signature by {self} in `{domain}`"))
    }
}

impl fmt::Display for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{KEY_PREFIX}{}", hex::encode(self.0))
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey({self})")
    }
}

impl FromStr for PublicKey {
    type Err = anyhow::Error;

    /// Only the lowercase form: a key has one spelling, so its text compares
    /// as the key does.
    fn from_str(text: &str) -> Result<PublicKey> {
        let hex = text.strip_prefix(KEY_PREFIX).with_context(|| format!("`{text}`: expected `{KEY_PREFIX}<64 hex digits>`"))?;
        if hex.bytes().any(|b| b.is_ascii_uppercase()) {
            bail!("`{text}`: hex digits are lowercase");
        }
        let bytes = hex_array(hex).with_context(|| format!("`{text}`"))?;
        PublicKey::from_bytes(bytes).with_context(|| format!("`{text}`"))
    }
}

impl Serialize for PublicKey {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for PublicKey {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<PublicKey, D::Error> {
        let text = String::deserialize(d)?;
        text.parse().map_err(|e: anyhow::Error| serde::de::Error::custom(format!("{e:#}")))
    }
}

/// An ed25519 signature. Text form (`Display`, `FromStr`, serde): 128
/// lowercase hex digits.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Signature([u8; 64]);

impl Signature {
    pub fn to_bytes(&self) -> [u8; 64] {
        self.0
    }

    pub fn from_bytes(bytes: [u8; 64]) -> Signature {
        Signature(bytes)
    }
}

impl fmt::Display for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

impl fmt::Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Signature({self})")
    }
}

impl FromStr for Signature {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Signature> {
        if text.bytes().any(|b| b.is_ascii_uppercase()) {
            bail!("a signature's hex digits are lowercase");
        }
        Ok(Signature(hex_array(text).context("a signature")?))
    }
}

impl Serialize for Signature {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Signature {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Signature, D::Error> {
        let text = String::deserialize(d)?;
        text.parse().map_err(|e: anyhow::Error| serde::de::Error::custom(format!("{e:#}")))
    }
}

/// What signs: this member's key, wherever it's kept. Every signature is
/// made in a `domain` (a constant per use, like [`STATEMENT_DOMAIN`]),
/// checked by [`PublicKey::verify`] in the same one.
pub trait Signer {
    fn public_key(&self) -> PublicKey;
    fn sign(&self, domain: &'static str, message: &[u8]) -> Signature;
}

/// A private key in memory. Its `Debug` shows the public key only.
#[derive(Clone)]
pub struct SigningKey(ed25519_dalek::SigningKey);

impl SigningKey {
    /// A new key from 32 bytes of the OS RNG.
    pub fn generate() -> Result<SigningKey> {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|e| anyhow!("cannot read the OS random source: {e}"))?;
        Ok(SigningKey::from_seed(seed))
    }

    pub fn from_seed(seed: [u8; 32]) -> SigningKey {
        SigningKey(ed25519_dalek::SigningKey::from_bytes(&seed))
    }

    /// `member.key`'s line (no newline): `ed25519-seed:<64 lowercase hex>`.
    fn to_line(&self) -> String {
        format!("{SEED_PREFIX}{}", hex::encode(self.0.to_bytes()))
    }

    /// [`SigningKey::to_line`]'s inverse; the file's text, one line with or
    /// without its newline. The error never echoes the text (a secret).
    fn from_text(text: &str) -> Result<SigningKey> {
        let line = text.strip_suffix('\n').unwrap_or(text);
        let seed = line
            .strip_prefix(SEED_PREFIX)
            .filter(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
            .and_then(|hex| hex_array(hex).ok())
            .with_context(|| format!("expected one line `{SEED_PREFIX}<64 lowercase hex digits>`"))?;
        Ok(SigningKey::from_seed(seed))
    }
}

impl Signer for SigningKey {
    fn public_key(&self) -> PublicKey {
        PublicKey(self.0.verifying_key().to_bytes())
    }

    fn sign(&self, domain: &'static str, message: &[u8]) -> Signature {
        Signature(self.0.sign(&domain_bytes(domain, message)).to_bytes())
    }
}

impl fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SigningKey({})", self.public_key())
    }
}

/// Where this member's private key is kept (decision 52: a file now, OS
/// keychains parked), and the next one while a rotation is pending (fleet
/// plan step 10a.6: made by `machine key rotate`, the current one until the
/// membership log lists it).
pub trait KeyStore {
    /// The key, created on first use.
    fn signing_key(&self) -> Result<SigningKey>;

    fn public_key(&self) -> Result<PublicKey> {
        Ok(self.signing_key()?.public_key())
    }

    /// The pending next key; `None` when no rotation is pending.
    fn pending(&self) -> Result<Option<SigningKey>>;

    /// The pending next key, made now when none is: a second rotation
    /// while one is pending reuses it.
    fn next_key(&self) -> Result<SigningKey>;

    /// The pending next key made the key, if it's `expected` (another one,
    /// or none, changes nothing): whether it was. Idempotent.
    fn promote(&self, expected: &PublicKey) -> Result<bool>;
}

/// `member.key` (`member::key_path`): one line, `ed25519-seed:<64
/// lowercase hex>` (the 32-byte seed), mode 0600. Created under the sibling
/// `member.key.lock` (not `member.lock`: a `member.toml` write may load the
/// key), a 0600 temp file renamed into place, so two creators end with one
/// key. Loading refuses a file group or others can reach, and a malformed
/// one: neither is replaced.
///
/// A rotation's next key is `member.key.next` ([`FileKeyStore::next_path`]),
/// the same format, made under the same lock; promoting it is a rename over
/// `member.key` under that lock, so a crash leaves either key whole.
#[derive(Debug, Clone)]
pub struct FileKeyStore {
    path: PathBuf,
}

impl FileKeyStore {
    pub fn new(path: PathBuf) -> FileKeyStore {
        FileKeyStore { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn lock_path(&self) -> PathBuf {
        crate::daemon_config::member_key_sibling(&self.path, ".lock")
    }

    /// `member.key.next`: the pending rotation's key.
    pub fn next_path(&self) -> PathBuf {
        crate::daemon_config::member_key_sibling(&self.path, ".next")
    }

    /// The key in the file; `None` when there's no file.
    pub fn load(&self) -> Result<Option<SigningKey>> {
        load_at(&self.path)
    }

    /// Under the lock, the key at `path`, made there first when there's
    /// none.
    fn load_or_make(&self, path: &Path) -> Result<SigningKey> {
        if let Some(key) = load_at(path)? {
            return Ok(key);
        }
        let dir = path.parent().with_context(|| format!("{} has no parent", path.display()))?;
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let _lock = crate::serve::locked_file::lock_at(&self.lock_path())?;
        // Another creator may have won while this one waited.
        if let Some(key) = load_at(path)? {
            return Ok(key);
        }
        let key = SigningKey::generate()?;
        crate::serve::locked_file::write_atomic_mode(path, &format!("{}\n", key.to_line()), 0o600)?;
        Ok(key)
    }
}

/// The key in the file at `path`; `None` when there's no file.
fn load_at(path: &Path) -> Result<Option<SigningKey>> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    let mode = file.metadata().with_context(|| format!("cannot read {}", path.display()))?.permissions().mode();
    if mode & 0o077 != 0 {
        bail!(
            "{} is reachable by group or others (mode {:04o}): this member's private key must be 0600 (`chmod 600 {}`)",
            path.display(),
            mode & 0o7777,
            path.display()
        );
    }
    let mut text = String::new();
    file.read_to_string(&mut text).with_context(|| format!("cannot read {}", path.display()))?;
    SigningKey::from_text(&text).map(Some).with_context(|| format!("invalid {}", path.display()))
}

impl KeyStore for FileKeyStore {
    fn signing_key(&self) -> Result<SigningKey> {
        self.load_or_make(&self.path)
    }

    fn pending(&self) -> Result<Option<SigningKey>> {
        load_at(&self.next_path())
    }

    fn next_key(&self) -> Result<SigningKey> {
        self.load_or_make(&self.next_path())
    }

    fn promote(&self, expected: &PublicKey) -> Result<bool> {
        let _lock = crate::serve::locked_file::lock_at(&self.lock_path())?;
        match load_at(&self.next_path())? {
            Some(next) if next.public_key() == *expected => {}
            _ => return Ok(false),
        }
        let next = self.next_path();
        std::fs::rename(&next, &self.path)
            .with_context(|| format!("cannot move {} over {}", next.display(), self.path.display()))?;
        Ok(true)
    }
}

/// A signed claim: `signer` (a member name) says, with `key`, that the
/// payload of sha256 `payload_sha256` is its `kind` at `version`. Versions
/// only go up per signer key and kind ([`Seen`]). On the wire (serde) the
/// hash is 64 lowercase hex digits, the key and signature their text forms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Statement {
    pub signer: String,
    pub key: PublicKey,
    pub kind: String,
    #[serde(with = "hex32")]
    pub payload_sha256: [u8; 32],
    pub version: u64,
    pub sig: Signature,
}

impl Statement {
    /// `payload` as `kind` at `version`, signed by `key` for `signer`.
    pub fn sign(signer: &str, key: &impl Signer, kind: &str, payload: &[u8], version: u64) -> Statement {
        let mut s = Statement {
            signer: signer.to_string(),
            key: key.public_key(),
            kind: kind.to_string(),
            payload_sha256: Sha256::digest(payload).into(),
            version,
            sig: Signature([0; 64]),
        };
        s.sig = key.sign(STATEMENT_DOMAIN, &s.signed_bytes());
        s
    }

    /// What the signature covers (in [`STATEMENT_DOMAIN`]): each field in
    /// order, length-prefixed (u32 big-endian) so no two statements encode
    /// alike: `signer`, `key` (32 raw bytes), `kind`, `payload_sha256`,
    /// `version` (u64 big-endian).
    pub fn signed_bytes(&self) -> Vec<u8> {
        let version = self.version.to_be_bytes();
        let fields: [&[u8]; 5] = [self.signer.as_bytes(), self.key.as_bytes(), self.kind.as_bytes(), &self.payload_sha256, &version];
        let mut out = Vec::new();
        for field in fields {
            let len = u32::try_from(field.len()).expect("a statement field under 4 GiB");
            out.extend_from_slice(&len.to_be_bytes());
            out.extend_from_slice(field);
        }
        out
    }

    /// The signature is `key`'s over this statement.
    pub fn verify(&self) -> Result<()> {
        self.key
            .verify(STATEMENT_DOMAIN, &self.signed_bytes(), &self.sig)
            .with_context(|| format!("statement `{}` v{} by `{}`", self.kind, self.version, self.signer))
    }

    /// [`Statement::verify`], and `payload` is the one it hashes.
    pub fn verify_payload(&self, payload: &[u8]) -> Result<()> {
        self.verify()?;
        if <[u8; 32]>::from(Sha256::digest(payload)) != self.payload_sha256 {
            bail!("statement `{}` v{} by `{}`: the payload isn't the one signed", self.kind, self.version, self.signer);
        }
        Ok(())
    }
}

/// `[u8; 32]` as 64 lowercase hex digits (`#[serde(with = "hex32")]`; the
/// membership log's hashes use it too).
pub(crate) mod hex32 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let text = String::deserialize(d)?;
        if text.bytes().any(|b| b.is_ascii_uppercase()) {
            return Err(serde::de::Error::custom("hex digits are lowercase"));
        }
        super::hex_array(&text).map_err(|e| serde::de::Error::custom(format!("{e:#}")))
    }
}

/// The rollback check's memory (pure): per signer key and kind, the
/// highest version accepted and its payload hash. Keyed by the key, not the
/// name: a name is only a claim.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Seen {
    latest: BTreeMap<(PublicKey, String), (u64, [u8; 32])>,
}

/// A statement [`Seen::accept`] took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accepted {
    /// Newer than any seen: now the latest.
    Newer,
    /// The latest already (same version, same payload): nothing changes.
    Again,
}

/// Why [`Seen::accept`] refused a statement. Nothing is recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The signature doesn't verify.
    BadSignature(String),
    /// Older than the latest seen.
    Rollback { kind: String, signer: String, version: u64, latest: u64 },
    /// The latest version, but another payload: the signer said two things.
    Equivocation { kind: String, signer: String, version: u64 },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::BadSignature(why) => f.write_str(why),
            Refusal::Rollback { kind, signer, version, latest } => {
                write!(f, "`{kind}` v{version} by `{signer}` is older than v{latest}, already seen (rollback)")
            }
            Refusal::Equivocation { kind, signer, version } => {
                write!(f, "`{kind}` v{version} by `{signer}` differs from the v{version} already seen (two payloads, one version)")
            }
        }
    }
}

impl std::error::Error for Refusal {}

impl Seen {
    /// `s` verified, then against the latest of its key and kind: a higher
    /// version is recorded, the same one with the same payload is
    /// [`Accepted::Again`], the same one with another payload or a lower
    /// one is refused.
    pub fn accept(&mut self, s: &Statement) -> Result<Accepted, Refusal> {
        s.verify().map_err(|e| Refusal::BadSignature(format!("{e:#}")))?;
        let slot = (s.key, s.kind.clone());
        match self.latest.get(&slot) {
            Some(&(latest, _)) if s.version < latest => {
                Err(Refusal::Rollback { kind: s.kind.clone(), signer: s.signer.clone(), version: s.version, latest })
            }
            Some(&(latest, hash)) if s.version == latest => {
                if hash == s.payload_sha256 {
                    Ok(Accepted::Again)
                } else {
                    Err(Refusal::Equivocation { kind: s.kind.clone(), signer: s.signer.clone(), version: s.version })
                }
            }
            _ => {
                self.latest.insert(slot, (s.version, s.payload_sha256));
                Ok(Accepted::Newer)
            }
        }
    }

    /// The latest version accepted from `key` for `kind`.
    pub fn latest(&self, key: &PublicKey, kind: &str) -> Option<u64> {
        self.latest.get(&(*key, kind.to_string())).map(|&(v, _)| v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dsv-identity-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn key(n: u8) -> SigningKey {
        SigningKey::from_seed([n; 32])
    }

    #[test]
    fn public_keys_roundtrip_as_text() {
        let k = key(1).public_key();
        let text = k.to_string();
        assert!(text.starts_with("ed25519:") && text.len() == 8 + 64, "{text}");
        assert_eq!(text.parse::<PublicKey>().unwrap(), k);
        assert_eq!(serde_json::to_value(k).unwrap(), serde_json::Value::String(text.clone()));
        assert_eq!(serde_json::from_value::<PublicKey>(serde_json::Value::String(text.clone())).unwrap(), k);
        for bad in ["", "ed25519:", "ed25519:abcd", "rsa:00", &text[8..], &text.to_uppercase(), &format!("{text}00")] {
            assert!(bad.parse::<PublicKey>().is_err(), "{bad}");
        }
        assert!(serde_json::from_str::<PublicKey>("\"ed25519:zz\"").is_err());
        assert_ne!(key(2).public_key(), k);
    }

    #[test]
    fn fingerprints_are_short_stable_hex() {
        let k = key(1).public_key();
        let fp = k.fingerprint();
        assert_eq!(fp, k.fingerprint());
        let body = fp.strip_prefix("SHA256:").unwrap();
        let groups: Vec<&str> = body.split('-').collect();
        assert_eq!(groups.len(), 8, "{fp}");
        assert!(groups.iter().all(|g| g.len() == 4 && g.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())), "{fp}");
        let digest = Sha256::digest(k.as_bytes());
        assert_eq!(body.replace('-', ""), hex::encode(&digest[..16]));
        assert_ne!(key(2).public_key().fingerprint(), fp);
    }

    #[test]
    fn signatures_verify_in_their_domain_only() {
        let k = key(3);
        let sig = k.sign("devsandbox/test/a", b"hello");
        let text = sig.to_string();
        assert_eq!(text.len(), 128);
        assert_eq!(text.parse::<Signature>().unwrap(), sig);
        assert_eq!(serde_json::from_value::<Signature>(serde_json::to_value(sig).unwrap()).unwrap(), sig);
        assert!(text[2..].parse::<Signature>().is_err());
        assert!(text.to_uppercase().parse::<Signature>().is_err());

        let pk = k.public_key();
        pk.verify("devsandbox/test/a", b"hello", &sig).unwrap();
        assert!(pk.verify("devsandbox/test/b", b"hello", &sig).is_err(), "another domain");
        assert!(pk.verify("devsandbox/test/a", b"hellO", &sig).is_err(), "another message");
        assert!(key(4).public_key().verify("devsandbox/test/a", b"hello", &sig).is_err(), "another key");
        // The NUL keeps a domain from running into its message.
        let joined = k.sign("devsandbox/test", b"/ahello");
        assert!(pk.verify("devsandbox/test/a", b"hello", &joined).is_err());
        let mut bytes = sig.to_bytes();
        bytes[0] ^= 1;
        assert!(pk.verify("devsandbox/test/a", b"hello", &Signature::from_bytes(bytes)).is_err(), "tampered");
        assert!(!format!("{k:?}").contains(&hex::encode([3u8; 32])), "Debug hides the seed");
    }

    #[test]
    fn statements_verify_and_every_field_is_signed() {
        let k = key(5);
        let s = Statement::sign("lap", &k, "membership", b"payload", 7);
        s.verify().unwrap();
        s.verify_payload(b"payload").unwrap();
        assert!(s.verify_payload(b"other").is_err());
        assert_eq!(s.payload_sha256, <[u8; 32]>::from(Sha256::digest(b"payload")));

        let wire = serde_json::to_value(&s).unwrap();
        assert_eq!(wire["key"], k.public_key().to_string());
        assert_eq!(wire["payload_sha256"], hex::encode(s.payload_sha256));
        assert_eq!(wire["sig"], s.sig.to_string());
        assert_eq!(serde_json::from_value::<Statement>(wire).unwrap(), s);

        let tampered: [(&str, Box<dyn Fn(&mut Statement)>); 6] = [
            ("signer", Box::new(|s| s.signer = "box".into())),
            ("key", Box::new(|s| s.key = key(6).public_key())),
            ("kind", Box::new(|s| s.kind = "hub".into())),
            ("payload_sha256", Box::new(|s| s.payload_sha256[0] ^= 1)),
            ("version", Box::new(|s| s.version += 1)),
            ("sig", Box::new(|s| s.sig = key(5).sign(STATEMENT_DOMAIN, b"else"))),
        ];
        for (field, change) in tampered {
            let mut t = s.clone();
            change(&mut t);
            assert!(t.verify().is_err(), "{field}");
        }
        // Length prefixes: moving bytes between fields changes the encoding.
        let a = Statement::sign("ab", &k, "c", b"", 1);
        let b = Statement { signer: "a".into(), kind: "bc".into(), ..a.clone() };
        assert_ne!(a.signed_bytes(), b.signed_bytes());
        assert!(b.verify().is_err());
        // Another key's statement in this key's name doesn't verify.
        let forged = Statement { key: k.public_key(), ..Statement::sign("lap", &key(6), "membership", b"payload", 7) };
        assert!(forged.verify().is_err());
    }

    #[test]
    fn seen_refuses_rollback_and_equivocation() {
        let (a, b) = (key(7), key(8));
        let mut seen = Seen::default();
        let s = |k: &SigningKey, kind: &str, payload: &[u8], v: u64| Statement::sign("lap", k, kind, payload, v);
        assert_eq!(seen.accept(&s(&a, "membership", b"one", 2)), Ok(Accepted::Newer));
        assert_eq!(seen.accept(&s(&a, "membership", b"one", 2)), Ok(Accepted::Again), "idempotent");
        assert!(matches!(seen.accept(&s(&a, "membership", b"two", 2)), Err(Refusal::Equivocation { version: 2, .. })));
        assert!(matches!(seen.accept(&s(&a, "membership", b"old", 1)), Err(Refusal::Rollback { version: 1, latest: 2, .. })));
        assert_eq!(seen.latest(&a.public_key(), "membership"), Some(2), "a refusal records nothing");
        assert_eq!(seen.accept(&s(&a, "membership", b"three", 5)), Ok(Accepted::Newer), "gaps are fine");
        assert_eq!(seen.latest(&a.public_key(), "membership"), Some(5));
        // Per kind and per key: the same name with another key is another signer.
        assert_eq!(seen.accept(&s(&a, "hub", b"x", 1)), Ok(Accepted::Newer));
        assert_eq!(seen.accept(&s(&b, "membership", b"x", 1)), Ok(Accepted::Newer));
        // A bad signature is refused before the versions.
        let mut bad = s(&a, "membership", b"four", 9);
        bad.version = 10;
        assert!(matches!(seen.accept(&bad), Err(Refusal::BadSignature(_))));
        assert_eq!(seen.latest(&a.public_key(), "membership"), Some(5));
        let msg = seen.accept(&s(&a, "membership", b"old", 3)).unwrap_err().to_string();
        assert!(msg.contains("rollback") && msg.contains("v3") && msg.contains("v5"), "{msg}");
    }

    #[test]
    fn the_file_store_creates_one_0600_key_and_keeps_it() {
        let dir = scratch("file");
        let store = FileKeyStore::new(dir.join("member.key"));
        assert!(store.load().unwrap().is_none());
        let k = store.signing_key().unwrap();
        let mode = std::fs::metadata(store.path()).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o600);
        let text = std::fs::read_to_string(store.path()).unwrap();
        assert!(text.starts_with("ed25519-seed:") && text.ends_with('\n') && text.lines().count() == 1);
        assert_eq!(store.public_key().unwrap(), k.public_key(), "loaded, not made again");
        assert_eq!(FileKeyStore::new(store.path().to_path_buf()).signing_key().unwrap().public_key(), k.public_key());
        assert!(dir.join("member.key.lock").exists());
        let leftovers: Vec<_> =
            std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().ends_with(".tmp")).collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");

        // Reachable by others: refused, naming the fix, and left as it is.
        std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o644)).unwrap();
        let e = format!("{:#}", store.signing_key().unwrap_err());
        assert!(e.contains("chmod 600") && e.contains(&store.path().display().to_string()) && e.contains("0644"), "{e}");
        assert_eq!(std::fs::read_to_string(store.path()).unwrap(), text);
        std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(store.load().is_err(), "group-readable");

        // Malformed: refused, never replaced, the secret never echoed.
        for bad in ["", "ed25519-seed:abcd\n", &text.to_uppercase(), &format!("{text}{text}"), &text.replace("seed", "key")] {
            std::fs::write(store.path(), bad).unwrap();
            std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
            let e = format!("{:#}", store.signing_key().unwrap_err());
            assert!(e.contains("invalid") && !e.contains(&text.trim()[13..]), "{e}");
            assert_eq!(std::fs::read_to_string(store.path()).unwrap(), bad);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `member.key.next`: made once and reused, 0600; promoted over
    /// `member.key` only when it's the expected key, and only once.
    #[test]
    fn a_next_key_is_reused_then_promoted_once() {
        let dir = scratch("next");
        let store = FileKeyStore::new(dir.join("member.key"));
        assert_eq!(store.next_path(), dir.join("member.key.next"));
        let old = store.signing_key().unwrap();
        assert!(store.pending().unwrap().is_none());
        assert!(!store.promote(&old.public_key()).unwrap(), "nothing pending");
        let next = store.next_key().unwrap();
        assert_ne!(next.public_key(), old.public_key());
        assert_eq!(std::fs::metadata(store.next_path()).unwrap().permissions().mode() & 0o7777, 0o600);
        assert_eq!(store.next_key().unwrap().public_key(), next.public_key(), "a pending one is reused");
        assert_eq!(store.pending().unwrap().unwrap().public_key(), next.public_key());
        assert_eq!(store.public_key().unwrap(), old.public_key(), "the key until promoted");

        assert!(!store.promote(&key(9).public_key()).unwrap(), "another key: nothing moves");
        assert!(store.next_path().exists());
        assert!(store.promote(&next.public_key()).unwrap());
        assert_eq!(store.public_key().unwrap(), next.public_key());
        assert!(!store.next_path().exists() && store.pending().unwrap().is_none());
        assert_eq!(std::fs::metadata(store.path()).unwrap().permissions().mode() & 0o7777, 0o600);
        assert!(!store.promote(&next.public_key()).unwrap(), "idempotent");
        assert_eq!(store.public_key().unwrap(), next.public_key());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_creators_end_with_one_key() {
        let dir = scratch("race");
        let path = dir.join("sub/member.key");
        let keys: Vec<PublicKey> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8).map(|_| scope.spawn(|| FileKeyStore::new(path.clone()).public_key().unwrap())).collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(keys.iter().all(|k| *k == keys[0]), "{keys:?}");
        assert_eq!(FileKeyStore::new(path).public_key().unwrap(), keys[0]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
