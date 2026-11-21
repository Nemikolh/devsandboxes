//! The member CA's trust in a container (credential-broker, *The member
//! CA*; docs/architecture/credentials/ca.md, *Trust in containers*): which
//! system bundle the CA variables name, and the root script that puts the
//! CA into that bundle. Pure but for the scripts' text; `run`
//! (`commands::run::claims`) execs them.

use sha2::{Digest, Sha256};

/// The system bundles a probe looks for, in order: Debian, Ubuntu and
/// Alpine; Fedora and RHEL; openSUSE.
pub const BUNDLES: [&str; 3] = ["/etc/ssl/certs/ca-certificates.crt", "/etc/pki/tls/certs/ca-bundle.crt", "/etc/ssl/ca-bundle.pem"];

/// Where the CA variables point when the image has no bundle (or the probe
/// couldn't run): the install script writes one there.
pub const DEFAULT_BUNDLE: &str = BUNDLES[0];

/// Prints the first of [`BUNDLES`] the image has, else `none`. Run from the
/// image before the container exists: the CA variables are container env.
pub const PROBE_SCRIPT: &str = "for f in /etc/ssl/certs/ca-certificates.crt /etc/pki/tls/certs/ca-bundle.crt \
     /etc/ssl/ca-bundle.pem; do if [ -s \"$f\" ]; then echo \"$f\"; exit 0; fi; done; echo none";

/// The bundle [`PROBE_SCRIPT`]'s output names; `None` for `none` or
/// anything else.
pub fn parse_probe(out: &str) -> Option<&'static str> {
    let out = out.trim();
    BUNDLES.into_iter().find(|b| *b == out)
}

/// As root, `$1` the bundle the CA variables name, stdin the CA's PEM then
/// the Mozilla set's ([`install_input`]). The CA goes into the distro's
/// anchors dir and its tool regenerates the bundles, so a later
/// regeneration keeps it; with no tool, it's appended to `$1` between
/// markers (replacing an earlier copy) and still dropped where Debian's and
/// Alpine's tool would read it. A `$1` that still doesn't exist gets the
/// Mozilla set plus the CA (between the markers, so a reissued cert
/// replaces it rather than sitting beside it), never the CA alone. Fails
/// unless `$1` ends up holding the CA.
pub const INSTALL_SCRIPT: &str = r#"set -e
t=/run/devsandbox/ca.$
trap 'rm -f "$t" "$t.all"' EXIT
mkdir -p /run/devsandbox
cat > "$t.all"
sed '/-----END CERTIFICATE-----/q' "$t.all" > "$t"
grep -q -- '-----BEGIN CERTIFICATE-----' "$t" || { echo "no certificate on stdin" >&2; exit 1; }
n=devsandbox-member.crt
anchor() { mkdir -p "$1" && cp "$t" "$1/$n" && chmod 644 "$1/$n"; }
if command -v update-ca-trust >/dev/null 2>&1; then
  anchor /etc/pki/ca-trust/source/anchors
  update-ca-trust extract >/dev/null 2>&1 || { echo "update-ca-trust failed" >&2; exit 1; }
elif command -v update-ca-certificates >/dev/null 2>&1 && [ -d /etc/pki/trust/anchors ]; then
  anchor /etc/pki/trust/anchors
  update-ca-certificates >/dev/null 2>&1 || { echo "update-ca-certificates failed" >&2; exit 1; }
elif command -v update-ca-certificates >/dev/null 2>&1; then
  anchor /usr/local/share/ca-certificates
  update-ca-certificates >/dev/null 2>&1 || { echo "update-ca-certificates failed" >&2; exit 1; }
else
  anchor /usr/local/share/ca-certificates
  if [ -s "$1" ]; then
    sed -i '/^# devsandbox member CA$/,/^# end devsandbox member CA$/d' "$1"
    { echo '# devsandbox member CA'; cat "$t"; echo '# end devsandbox member CA'; } >> "$1"
  fi
fi
if [ ! -s "$1" ]; then
  mkdir -p "$(dirname "$1")"
  { sed '1,/-----END CERTIFICATE-----/d' "$t.all"; echo '# devsandbox member CA'; cat "$t"; echo '# end devsandbox member CA'; } > "$1.tmp"
  chmod 644 "$1.tmp" && mv "$1.tmp" "$1"
fi
k=$(sed -n 2p "$t")
grep -qF -- "$k" "$1" || { echo "$1 doesn't hold the member CA after the install" >&2; exit 1; }
"#;

/// [`INSTALL_SCRIPT`]'s stdin: the CA first, then the Mozilla set.
pub fn install_input(ca_pem: &str) -> String {
    let mut out = ca_pem.trim_end().to_string();
    out.push('\n');
    for cert in webpki_root_certs::TLS_SERVER_ROOT_CERTS {
        out.push_str(&pem(cert));
    }
    out
}

fn pem(der: &[u8]) -> String {
    let b64 = crate::broker::redact::base64(der);
    let mut out = String::from("-----BEGIN CERTIFICATE-----\n");
    for line in b64.chunks(64) {
        out.push_str(std::str::from_utf8(line).expect("base64 is ascii"));
        out.push('\n');
    }
    out.push_str("-----END CERTIFICATE-----\n");
    out
}

/// What the instance records as installed: a reissued cert differs.
pub fn fingerprint(ca_pem: &str) -> String {
    hex::encode(&Sha256::digest(ca_pem.as_bytes())[..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_output_names_a_known_bundle_or_none() {
        assert_eq!(parse_probe("/etc/pki/tls/certs/ca-bundle.crt\n"), Some("/etc/pki/tls/certs/ca-bundle.crt"));
        assert_eq!(parse_probe("/etc/ssl/ca-bundle.pem"), Some("/etc/ssl/ca-bundle.pem"));
        assert_eq!(parse_probe(" /etc/ssl/certs/ca-certificates.crt "), Some(DEFAULT_BUNDLE));
        for other in ["none", "", "/etc/passwd", "sh: not found"] {
            assert_eq!(parse_probe(other), None, "{other}");
        }
        for b in BUNDLES {
            assert!(PROBE_SCRIPT.contains(b), "{b}");
        }
    }

    #[test]
    fn install_input_is_the_ca_then_mozillas_roots() {
        let ca = "-----BEGIN CERTIFICATE-----\nQUJD\n-----END CERTIFICATE-----\n";
        let input = install_input(ca);
        assert!(input.starts_with(ca), "{}", &input[..200]);
        let count = input.matches("-----BEGIN CERTIFICATE-----").count();
        assert_eq!(count, 1 + webpki_root_certs::TLS_SERVER_ROOT_CERTS.len());
        assert!(count > 100, "{count}");
        use rustls::pki_types::{CertificateDer, pem::PemObject};
        let parsed: Vec<_> = CertificateDer::pem_slice_iter(input.as_bytes()).skip(1).collect::<Result<_, _>>().unwrap();
        assert_eq!(parsed[0].as_ref(), webpki_root_certs::TLS_SERVER_ROOT_CERTS[0].as_ref());
        assert!(input.lines().all(|l| l.len() <= 64 || l.starts_with("-----")));
    }

    #[test]
    fn fingerprints_differ_per_cert() {
        assert_eq!(fingerprint("a"), fingerprint("a"));
        assert_ne!(fingerprint("a"), fingerprint("b"));
        assert_eq!(fingerprint("a").len(), 64);
    }
}
