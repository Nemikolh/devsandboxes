use std::sync::Arc;

use rustls::pki_types::ServerName;
use rustls::pki_types::pem::PemObject;
use rustls::{ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection};

use super::*;

fn temp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dsv-member-ca-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

fn set(hosts: &[&str]) -> BTreeSet<String> {
    hosts.iter().map(|h| h.to_string()).collect()
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// A client trusting only `ca_pem` dials `host`, the server presenting
/// `leaf`, over in-memory buffers; the client's verdict.
fn handshake(ca_pem: &str, host: &str, leaf: &Leaf) -> Result<(), rustls::Error> {
    let mut roots = RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(ca_pem.as_bytes()) {
        roots.add(cert.unwrap()).unwrap();
    }
    let client = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let server = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(leaf.chain.clone(), leaf.key.clone_key())
        .unwrap();
    let mut client = ClientConnection::new(Arc::new(client), ServerName::try_from(host.to_string()).unwrap()).unwrap();
    let mut server = ServerConnection::new(Arc::new(server)).unwrap();
    for _ in 0..10 {
        let mut buf = Vec::new();
        while client.wants_write() {
            client.write_tls(&mut buf).unwrap();
        }
        let mut rd = &buf[..];
        while !rd.is_empty() {
            server.read_tls(&mut rd).unwrap();
            server.process_new_packets().unwrap();
        }
        buf.clear();
        while server.wants_write() {
            server.write_tls(&mut buf).unwrap();
        }
        let mut rd = &buf[..];
        while !rd.is_empty() {
            client.read_tls(&mut rd).unwrap();
            client.process_new_packets()?;
        }
        if !client.is_handshaking() && !server.is_handshaking() {
            return Ok(());
        }
    }
    panic!("the handshake made no progress");
}

fn ca(tag: &str, hosts: &[&str]) -> (PathBuf, MemberCa) {
    let dir = temp(tag);
    let mut ca = MemberCa::load_or_create(&dir).unwrap();
    ca.ensure_hosts(&set(hosts)).unwrap();
    (dir, ca)
}

fn invalid_cert(r: Result<(), rustls::Error>) -> rustls::CertificateError {
    match r {
        Err(rustls::Error::InvalidCertificate(e)) => e,
        other => panic!("expected a refused certificate, got {other:?}"),
    }
}

#[test]
fn a_leaf_for_a_covered_host_passes_a_client_trusting_only_the_ca() {
    let (_, ca) = ca("ok", &["github.com", "api.github.com"]);
    for host in ["github.com", "api.github.com"] {
        handshake(ca.cert_pem().unwrap(), host, &ca.leaf(host).unwrap()).unwrap();
    }
    // The leaf names its own host only.
    let leaf = ca.leaf("api.github.com").unwrap();
    invalid_cert(handshake(ca.cert_pem().unwrap(), "github.com", &leaf));
}

#[test]
fn the_client_enforces_the_name_constraints() {
    let (_, ca) = ca("constraints", &["github.com"]);
    let now = SystemTime::now();
    assert!(ca.leaf("evil.example").is_err());
    let rogue = ca.mint("evil.example", now).unwrap();
    let e = invalid_cert(handshake(ca.cert_pem().unwrap(), "evil.example", &rogue));
    assert!(format!("{e:?}").contains("NameConstraintViolation"), "{e:?}");
    let ip = ca.mint("10.0.0.1", now).unwrap();
    let e = invalid_cert(handshake(ca.cert_pem().unwrap(), "10.0.0.1", &ip));
    assert!(format!("{e:?}").contains("NameConstraintViolation"), "{e:?}");
}

#[test]
fn an_empty_set_has_no_cert_and_no_leaves() {
    let dir = temp("empty");
    let mut ca = MemberCa::load_or_create(&dir).unwrap();
    assert_eq!(ca.cert_pem(), None);
    assert!(ca.hosts().is_empty());
    assert!(!dir.join(MEMBER_CA_CERT).exists());
    assert!(ca.leaf("github.com").is_err());
    assert!(!ca.ensure_hosts(&BTreeSet::new()).unwrap());
    assert!(ca.cert_pem().is_none());
}

#[test]
fn the_key_is_0600_and_survives_a_reload() {
    let (dir, ca) = ca("perms", &["github.com"]);
    assert_eq!(mode(&dir.join(MEMBER_CA_KEY)), 0o600);
    assert_eq!(mode(&dir.join(MEMBER_CA_CERT)), 0o644);
    let again = MemberCa::load_or_create(&dir).unwrap();
    assert_eq!(again.key.public_key_raw(), ca.key.public_key_raw());
    assert_eq!(again.cert_pem(), ca.cert_pem(), "nothing to reissue");
    assert_eq!(again.hosts(), set(&["github.com"]));
}

#[test]
fn a_wide_or_bad_key_is_refused() {
    let (dir, _) = ca("wide", &["github.com"]);
    let key = dir.join(MEMBER_CA_KEY);
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o640)).unwrap();
    let err = MemberCa::load_or_create(&dir).err().unwrap().to_string();
    assert!(err.contains("reachable by group or others"), "{err}");
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let ed25519 = KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
    std::fs::write(&key, ed25519.serialize_pem()).unwrap();
    let err = MemberCa::load_or_create(&dir).err().unwrap().to_string();
    assert!(err.contains("isn't an ECDSA P-256 key"), "{err}");
}

#[test]
fn a_new_host_reissues_over_the_same_key_and_old_leaves_still_chain() {
    let (dir, mut ca) = ca("grow", &["github.com"]);
    let before = ca.cert_pem().unwrap().to_string();
    let old_leaf = ca.leaf("github.com").unwrap();
    let key = std::fs::read_to_string(dir.join(MEMBER_CA_KEY)).unwrap();

    assert!(!ca.ensure_hosts(&set(&["github.com"])).unwrap());
    assert!(ca.ensure_hosts(&set(&["sentry.io"])).unwrap());
    assert_eq!(ca.hosts(), set(&["github.com", "sentry.io"]), "the set grows, never shrinks");
    assert_ne!(ca.cert_pem().unwrap(), before);
    assert_eq!(std::fs::read_to_string(dir.join(MEMBER_CA_KEY)).unwrap(), key);

    handshake(ca.cert_pem().unwrap(), "github.com", &old_leaf).unwrap();
    handshake(ca.cert_pem().unwrap(), "sentry.io", &ca.leaf("sentry.io").unwrap()).unwrap();
    // The old cert doesn't cover the new host.
    invalid_cert(handshake(&before, "sentry.io", &ca.leaf("sentry.io").unwrap()));
}

#[test]
fn another_daemons_hosts_are_kept() {
    let (dir, mut a) = ca("two", &["github.com"]);
    let mut b = MemberCa::load_or_create(&dir).unwrap();
    b.ensure_hosts(&set(&["sentry.io"])).unwrap();
    assert!(a.ensure_hosts(&set(&["gitlab.com"])).unwrap());
    assert_eq!(a.hosts(), set(&["github.com", "gitlab.com", "sentry.io"]));
}

#[test]
fn a_replaced_key_reissues_for_the_same_hosts() {
    let (dir, ca) = ca("rekey", &["github.com"]);
    std::fs::remove_file(dir.join(MEMBER_CA_KEY)).unwrap();
    let again = MemberCa::load_or_create(&dir).unwrap();
    assert_ne!(again.key.public_key_raw(), ca.key.public_key_raw());
    assert_eq!(again.hosts(), set(&["github.com"]));
    handshake(again.cert_pem().unwrap(), "github.com", &again.leaf("github.com").unwrap()).unwrap();
    invalid_cert(handshake(ca.cert_pem().unwrap(), "github.com", &again.leaf("github.com").unwrap()));
}

#[test]
fn an_expiring_ca_cert_is_reissued_at_load() {
    let dir = temp("renew");
    let t0 = SystemTime::now();
    let mut ca = MemberCa::load_or_create_at(&dir, t0).unwrap();
    ca.ensure_hosts_at(&set(&["github.com"]), t0).unwrap();
    let pem = ca.cert_pem().unwrap().to_string();
    let soon = t0 + CA_LIFETIME - CA_RENEW - DAY;
    assert_eq!(MemberCa::load_or_create_at(&dir, soon).unwrap().cert_pem(), Some(pem.as_str()));
    let late = t0 + CA_LIFETIME - CA_RENEW + DAY;
    assert_ne!(MemberCa::load_or_create_at(&dir, late).unwrap().cert_pem(), Some(pem.as_str()));
}

#[test]
fn leaves_are_cached_until_a_day_before_they_expire() {
    let (_, ca) = ca("cache", &["github.com"]);
    let t0 = SystemTime::now();
    let first = ca.leaf_at("github.com", t0).unwrap().chain;
    let same = |t| ca.leaf_at("github.com", t).unwrap().chain == first;
    assert!(same(t0 + LEAF_LIFETIME - LEAF_RENEW - Duration::from_secs(1)));
    assert!(!same(t0 + LEAF_LIFETIME - LEAF_RENEW));
}

#[test]
fn leaves_are_backdated_for_a_lagging_clock() {
    let (_, ca) = ca("skew", &["github.com"]);
    let now = SystemTime::now();
    let pem = ca.cert_pem().unwrap();
    // Minted on a host 12 hours ahead of the client.
    handshake(pem, "github.com", &ca.mint("github.com", now + Duration::from_secs(12 * 3600)).unwrap()).unwrap();
    let ahead = ca.mint("github.com", now + 2 * DAY).unwrap();
    let e = invalid_cert(handshake(pem, "github.com", &ahead));
    assert!(matches!(e, rustls::CertificateError::NotValidYet | rustls::CertificateError::NotValidYetContext { .. }), "{e:?}");
    let old = ca.mint("github.com", now - LEAF_LIFETIME - DAY).unwrap();
    let e = invalid_cert(handshake(pem, "github.com", &old));
    assert!(matches!(e, rustls::CertificateError::Expired | rustls::CertificateError::ExpiredContext { .. }), "{e:?}");
}

#[test]
fn hosts() {
    for good in ["github.com", "api.github.com", "localhost", "a-b.example", "x1.io"] {
        check_host(good).unwrap();
    }
    for bad in ["", "GitHub.com", "github.com.", ".github.com", "a..b", "-a.com", "a-.com", "*.github.com", "10.0.0.1", "::1", "a_b.com", "1.2.3"] {
        assert!(check_host(bad).is_err(), "{bad}");
    }
    let dir = temp("bad-host");
    let mut ca = MemberCa::load_or_create(&dir).unwrap();
    assert!(ca.ensure_hosts(&set(&["Evil.com"])).is_err());
    assert!(ca.cert_pem().is_none());
}
