use std::collections::HashMap;

use ed25519_dalek::Signer as _;

use super::*;
use crate::config::KeyRef;

fn string(out: &mut Vec<u8>, s: &[u8]) {
    put_string(out, s);
}

fn strings(parts: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for p in parts {
        string(&mut out, p);
    }
    out
}

/// A client key blob; its signatures are the agent's business, never ours.
fn client_key(n: u8) -> Vec<u8> {
    strings(&[b"ssh-ed25519", &[n; 32]])
}

/// A server's ed25519 host key and its blob.
fn host(n: u8) -> (ed25519_dalek::SigningKey, Vec<u8>) {
    let sk = ed25519_dalek::SigningKey::from_bytes(&[n; 32]);
    let blob = strings(&[b"ssh-ed25519", sk.verifying_key().as_bytes()]);
    (sk, blob)
}

fn ed_sig(sk: &ed25519_dalek::SigningKey, data: &[u8]) -> Vec<u8> {
    strings(&[b"ssh-ed25519", &sk.sign(data).to_bytes()])
}

fn bind(hostkey: &[u8], session_id: &[u8], signature: &[u8], forwarding: bool) -> Vec<u8> {
    let mut out = vec![EXTENSION];
    out.extend(strings(&[SESSION_BIND, hostkey, session_id, signature]));
    out.push(forwarding as u8);
    out
}

fn sign(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut out = vec![SIGN_REQUEST];
    out.extend(strings(&[key, data]));
    out.extend(0u32.to_be_bytes());
    out
}

fn userauth(session_id: &[u8], key: &[u8]) -> Vec<u8> {
    let mut out = strings(&[session_id]);
    out.push(USERAUTH_REQUEST);
    out.extend(strings(&[b"git", b"ssh-connection", b"publickey"]));
    out.push(1);
    out.extend(strings(&[b"ssh-ed25519", key]));
    out
}

fn sshsig(namespace: &[u8]) -> Vec<u8> {
    let mut out = SSHSIG_MAGIC.to_vec();
    out.extend(strings(&[namespace, b"", b"sha512", &[0; 64]]));
    out
}

fn answer(keys: &[(&[u8], &str)]) -> Vec<u8> {
    let mut out = vec![IDENTITIES_ANSWER];
    out.extend((keys.len() as u32).to_be_bytes());
    for (blob, comment) in keys {
        out.extend(strings(&[blob, comment.as_bytes()]));
    }
    out
}

struct Known(HashMap<String, Vec<Vec<u8>>>);

impl KnownHosts for Known {
    fn keys(&self, host: &str) -> Result<Vec<Vec<u8>>, String> {
        if host == "broken" {
            return Err("`ssh-keygen -F` didn't run: gone".into());
        }
        Ok(self.0.get(host).cloned().unwrap_or_default())
    }
}

fn named(key: &[u8], destinations: Option<&[&str]>, signing_only: bool) -> SshKey {
    SshKey {
        key: KeyRef::Fingerprint(fingerprint_b64(key)),
        destinations: destinations.map(|d| d.iter().map(|h| h.to_string()).collect()),
        signing_only,
    }
}

/// gitlab.example.com's host key is host(1)'s.
fn filter(keys: Vec<SshKey>) -> Filter<Known> {
    let known = HashMap::from([("gitlab.example.com".to_string(), vec![host(1).1])]);
    Filter::new(keys, vec!["github.com".into()], Known(known))
}

fn refusal(step: Step) -> String {
    match step {
        Step::Refuse(why) => why,
        other => panic!("not refused: {other:?}"),
    }
}

/// A connection bound to `hostkey`'s server by a proper session-bind.
fn bound(f: &mut Filter<Known>, host_n: u8, session_id: &[u8]) {
    let (sk, blob) = host(host_n);
    assert_eq!(f.request(&bind(&blob, session_id, &ed_sig(&sk, session_id), false)), Step::Forward);
}

#[test]
fn unnamed_keys_are_hidden_and_never_sign() {
    let (mine, other) = (client_key(1), client_key(2));
    let f = filter(vec![named(&mine, Some(&["gitlab.example.com"]), false)]);
    let listed = f.identities(&answer(&[(&other, "work"), (&mine, "me@laptop")]));
    assert_eq!(listed, answer(&[(&mine, "me@laptop")]));
    assert_eq!(filter(Vec::new()).identities(&answer(&[(&mine, "x")])), answer(&[]));
    // A failure passes as is; a malformed answer becomes one.
    assert_eq!(f.identities(&[FAILURE]), [FAILURE]);
    assert_eq!(f.identities(&[IDENTITIES_ANSWER, 0, 0, 0, 1]), [FAILURE]);

    let mut f = filter(Vec::new());
    assert_eq!(f.request(&[REQUEST_IDENTITIES]), Step::Identities);
    let why = refusal(f.request(&sign(&other, &userauth(b"sid", &other))));
    assert!(why.contains("isn't in the sandbox's `ssh-keys`") && why.contains("github.com"), "{why}");
    assert!(why.starts_with(&format!("SHA256:{}", fingerprint_b64(&other))), "{why}");
}

#[test]
fn a_key_without_destinations_signs_anything() {
    let mine = client_key(1);
    let mut f = filter(vec![named(&mine, None, false)]);
    assert_eq!(f.request(&sign(&mine, &userauth(b"sid", &mine))), Step::Forward);
    assert_eq!(f.request(&sign(&mine, &sshsig(b"file"))), Step::Forward);
    // Named by its public key line's blob, the same.
    let mut f = filter(vec![SshKey { key: KeyRef::Blob(mine.clone()), destinations: None, signing_only: false }]);
    assert_eq!(f.request(&sign(&mine, b"anything")), Step::Forward);
}

#[test]
fn a_destination_key_signs_only_logins_bound_to_a_known_host_key() {
    let mine = client_key(1);
    let keys = || vec![named(&mine, Some(&["gitlab.example.com"]), false)];
    let login = sign(&mine, &userauth(b"sid-1", &mine));

    let why = refusal(filter(keys()).request(&login));
    assert!(why.contains("session-bind@openssh.com") && why.contains("this one isn't"), "{why}");

    // Bound to a host key known_hosts doesn't give gitlab (GitHub's, say).
    let mut f = filter(keys());
    bound(&mut f, 2, b"sid-1");
    let why = refusal(f.request(&login));
    assert!(why.contains("signs only for `gitlab.example.com`") && why.contains("another host's key"), "{why}");

    let mut f = filter(keys());
    bound(&mut f, 1, b"sid-1");
    assert_eq!(f.request(&login), Step::Forward);
    let why = refusal(f.request(&sign(&mine, &userauth(b"sid-2", &mine))));
    assert!(why.contains("isn't the one the connection bound"), "{why}");
    let why = refusal(f.request(&sign(&mine, &userauth(b"sid-1", &client_key(9)))));
    assert!(why.contains("names another key"), "{why}");
    let why = refusal(f.request(&sign(&mine, &sshsig(b"git"))));
    assert!(why.contains("has `destinations`"), "{why}");
    let why = refusal(f.request(&sign(&mine, b"not a login")));
    assert!(why.contains("isn't one"), "{why}");
    // A login-bound connection takes no second bind.
    let (sk, blob) = host(1);
    let why = refusal(f.request(&bind(&blob, b"sid-3", &ed_sig(&sk, b"sid-3"), false)));
    assert!(why.contains("after one for a login"), "{why}");
}

#[test]
fn a_forged_session_bind_is_refused_and_binds_nothing() {
    let mine = client_key(1);
    let mut f = filter(vec![named(&mine, Some(&["gitlab.example.com"]), false)]);
    // gitlab's key, but signed by another: what a forger without it can do.
    let (_, gitlab) = host(1);
    let (forger, _) = host(3);
    let why = refusal(f.request(&bind(&gitlab, b"sid-1", &ed_sig(&forger, b"sid-1"), false)));
    assert!(why.contains("doesn't check"), "{why}");
    let why = refusal(f.request(&sign(&mine, &userauth(b"sid-1", &mine))));
    assert!(why.contains("this one isn't"), "{why}");
}

#[test]
fn a_forwarding_bind_last_is_no_login() {
    let mine = client_key(1);
    let mut f = filter(vec![named(&mine, Some(&["gitlab.example.com"]), false)]);
    let (sk, blob) = host(1);
    assert_eq!(f.request(&bind(&blob, b"hop", &ed_sig(&sk, b"hop"), true)), Step::Forward);
    let why = refusal(f.request(&sign(&mine, &userauth(b"hop", &mine))));
    assert!(why.contains("for forwarding"), "{why}");
    // The next hop's bind is the login's.
    bound(&mut f, 1, b"sid-2");
    assert_eq!(f.request(&sign(&mine, &userauth(b"sid-2", &mine))), Step::Forward);
}

#[test]
fn a_destination_missing_from_known_hosts_is_named() {
    let mine = client_key(1);
    let mut f = filter(vec![named(&mine, Some(&["git.corp", "[git.lab]:2222"]), false)]);
    bound(&mut f, 1, b"sid");
    let why = refusal(f.request(&sign(&mine, &userauth(b"sid", &mine))));
    assert!(why.contains("`git.corp`, `[git.lab]:2222` not in this host's known_hosts"), "{why}");

    let mut f = filter(vec![named(&mine, Some(&["broken"]), false)]);
    bound(&mut f, 1, b"sid");
    let why = refusal(f.request(&sign(&mine, &userauth(b"sid", &mine))));
    assert!(why.contains("didn't run"), "{why}");
}

#[test]
fn a_signing_only_key_signs_git_signatures_only() {
    let mine = client_key(1);
    let mut f = filter(vec![named(&mine, None, true)]);
    assert_eq!(f.request(&sign(&mine, &sshsig(b"git"))), Step::Forward);
    let why = refusal(f.request(&sign(&mine, &sshsig(b"file"))));
    assert!(why.contains("not `file`"), "{why}");
    bound(&mut f, 1, b"sid");
    let why = refusal(f.request(&sign(&mine, &userauth(b"sid", &mine))));
    assert!(why.contains("never logins"), "{why}");
    // A truncated SSHSIG blob is no signature.
    let mut cut = sshsig(b"git");
    cut.truncate(cut.len() - 3);
    assert!(matches!(f.request(&sign(&mine, &cut)), Step::Refuse(_)));

    // Named twice, once per use: each use takes its entry.
    let mut f = filter(vec![named(&mine, Some(&["gitlab.example.com"]), false), named(&mine, None, true)]);
    assert_eq!(f.request(&sign(&mine, &sshsig(b"git"))), Step::Forward);
    bound(&mut f, 1, b"sid");
    assert_eq!(f.request(&sign(&mine, &userauth(b"sid", &mine))), Step::Forward);
}

#[test]
fn other_requests_are_refused() {
    let mut f = filter(Vec::new());
    for msg in [vec![17u8], vec![19], vec![22], vec![]] {
        assert!(matches!(f.request(&msg), Step::Refuse(_)), "{msg:?}");
    }
    let mut query = vec![EXTENSION];
    query.extend(strings(&[b"query"]));
    let why = refusal(f.request(&query));
    assert!(why.contains("`query`"), "{why}");
}

#[test]
fn ecdsa_and_rsa_host_keys_verify() {
    use ring::rand::SystemRandom;
    use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
    let rng = SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
    let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap();
    let hostkey = strings(&[b"ecdsa-sha2-nistp256", b"nistp256", pair.public_key().as_ref()]);
    let fixed = pair.sign(&rng, b"sid").unwrap();
    let mpint = |v: &[u8]| {
        let mut m: Vec<u8> = v.iter().skip_while(|&&b| b == 0).copied().collect();
        if m.first().is_some_and(|b| b & 0x80 != 0) {
            m.insert(0, 0);
        }
        m
    };
    let (r, s) = fixed.as_ref().split_at(32);
    let sig = strings(&[b"ecdsa-sha2-nistp256", &strings(&[&mpint(r), &mpint(s)])]);
    assert_eq!(verify(&hostkey, b"sid", &sig), Ok(()));
    assert!(verify(&hostkey, b"other", &sig).is_err());

    let n = hex::decode(RSA_N).unwrap();
    let mut n_mpint = vec![0];
    n_mpint.extend(&n);
    let hostkey = strings(&[b"ssh-rsa", &[1, 0, 1], &n_mpint]);
    let sig = hex::decode(RSA_SIG).unwrap();
    assert_eq!(verify(&hostkey, RSA_MSG, &strings(&[b"rsa-sha2-256", &sig])), Ok(()));
    assert!(verify(&hostkey, b"other", &strings(&[b"rsa-sha2-256", &sig])).is_err());
    let why = verify(&hostkey, RSA_MSG, &strings(&[b"ssh-rsa", &sig])).unwrap_err();
    assert!(why.contains("`ssh-rsa` signatures aren't checked"), "{why}");

    let p521_sig = strings(&[b"ecdsa-sha2-nistp521", b"rs"]);
    let why = verify(&strings(&[b"ecdsa-sha2-nistp521", b"nistp521", b"q"]), b"sid", &p521_sig).unwrap_err();
    assert!(why.contains("`ecdsa-sha2-nistp521` host keys aren't checked"), "{why}");
}

/// `openssl dgst -sha256 -sign` of [`RSA_MSG`] by a throwaway 2048-bit key
/// (e = 65537): ring verifies rsa but can't sign without a key file.
const RSA_MSG: &[u8] = b"devsandbox-rsa-session-id";
const RSA_N: &str = "9f1c8f8d75b53feb18f2b1b85b881636882c1c37ad204724ec51c773f2ed62a03350ea70552c55817de2b5ab35cb02c24d45bf1f5647c3f2724fa2f1376d03a91bd8cdf0840bd1fad23979e6952015efad5aef2e3629712b6edf6c7cec8c2f6609aee122ef2208c905bf5473f20d2f7ac6eff41db1e7331a548be64b19b51224e57c50a390f42419fea75966495a1d1921ad20c080eb6374fcfabca47d524f53b6101a27206be744d475f4885bc20629fc96cbcc5d1e22042751e27c5243fadf916006016823b855d6078195c5ab047cb572e986fc4926594866c275fd20a992f26aba669d2b27a29c73a5d75c026308c0ef0ff0c12d807e21f2fa164e3afdfb";
const RSA_SIG: &str = "39b16efea054ff2a064a043031c4b620b06b6549ca55dd3999d089a6ac85dda4c62650b6a5cf5e57e98b61f7e61cf50b6dd7d20c2265b233102f6a39392270feda06905e469a2f474648eddc66ecb4f32862508f9bf8cec6d8e083ca58931cd094102dfb0e7a07c66bcbea0fa4169efb2380a61c8bffb623ade81e658cbe91e0810f303f4a38278e7dc7986040b03837d226601bf5c1c7e9d9bc64a149b3fd3bdb945ffd56621e3a99f31f5e1a718622403e1c456f538ede7b047450fe3cd6450fcc89f183bdcddc1d76ecfe1bf0ab74ebdfab8208f82e0e9b39f570795cb1d7faaa5339092da24c5ae7ac4c199b07f38f516851d1b548e7d2e5f4cdec2ea79e";

#[test]
fn known_hosts_output_drops_revoked_and_ca_lines() {
    let (_, a) = host(1);
    let (_, b) = host(2);
    let (_, c) = host(3);
    let b64 = |k: &[u8]| String::from_utf8(crate::broker::redact::base64(k)).unwrap();
    let out = format!(
        "# Host gitlab.example.com found: line 3\ngitlab.example.com ssh-ed25519 {} comment\n|1|abc=|def= ssh-ed25519 {}\n@revoked gitlab.example.com ssh-ed25519 {}\n@cert-authority *.example.com ssh-ed25519 {}\nbroken line\n\n",
        b64(&a),
        b64(&b),
        b64(&b),
        b64(&c)
    );
    assert_eq!(parse_known_hosts(&out), vec![a]);
}

#[test]
fn ssh_keygen_runs_per_existing_file() {
    let dir = std::env::temp_dir().join(format!("dsv-known-hosts-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (_, a) = host(1);
    let line = format!("gitlab.example.com ssh-ed25519 {}", String::from_utf8(crate::broker::redact::base64(&a)).unwrap());
    // Stands in for ssh-keygen: echoes its argv's host and file, then a hit.
    let program = dir.join("fake-keygen");
    std::fs::write(&program, format!("#!/bin/sh\necho \"# $2 in $4\"\necho '{line}'\n")).unwrap();
    let mut perms = std::fs::metadata(&program).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&program, perms).unwrap();
    let file = dir.join("known_hosts");
    std::fs::write(&file, "").unwrap();
    let keygen = SshKeygen { program: program.clone(), files: vec![file, dir.join("missing")] };
    assert_eq!(keygen.keys("gitlab.example.com"), Ok(vec![a]));
    let none = SshKeygen { program, files: vec![dir.join("missing")] };
    assert_eq!(none.keys("gitlab.example.com"), Ok(Vec::new()));
    let gone = SshKeygen { program: dir.join("no-such-program"), files: vec![dir.join("known_hosts")] };
    assert!(gone.keys("x").unwrap_err().contains("didn't run"));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn relay_filters_a_live_connection() {
    let mine = client_key(1);
    let other = client_key(2);
    let (mut client, mut client_end) = UnixStream::pair().unwrap();
    let (mut agent_end, mut agent) = UnixStream::pair().unwrap();
    let listing = answer(&[(&other, "work"), (&mine, "me")]);
    let fake_agent = std::thread::spawn(move || {
        // Answers the listing, then a forwarded sign; never sees the refused one.
        let req = read_message(&mut agent).unwrap().unwrap();
        assert_eq!(req, [REQUEST_IDENTITIES]);
        write_message(&mut agent, &listing).unwrap();
        let req = read_message(&mut agent).unwrap().unwrap();
        assert_eq!(req[0], SIGN_REQUEST);
        write_message(&mut agent, &[14, 0, 0, 0, 0]).unwrap();
        assert_eq!(read_message(&mut agent).unwrap(), None);
    });
    let relay_thread = std::thread::spawn(move || {
        let mut f = filter(vec![named(&mine, None, true)]);
        let reports = std::sync::Mutex::new(Vec::new());
        relay(&mut client_end, &mut agent_end, &mut f, &|why| reports.lock().unwrap().push(why)).unwrap();
        reports.into_inner().unwrap()
    });
    write_message(&mut client, &[REQUEST_IDENTITIES]).unwrap();
    assert_eq!(read_message(&mut client).unwrap().unwrap(), answer(&[(&client_key(1), "me")]));
    write_message(&mut client, &sign(&other, b"x")).unwrap();
    assert_eq!(read_message(&mut client).unwrap().unwrap(), [FAILURE]);
    write_message(&mut client, &sign(&client_key(1), &sshsig(b"git"))).unwrap();
    assert_eq!(read_message(&mut client).unwrap().unwrap(), [14, 0, 0, 0, 0]);
    drop(client);
    let reports = relay_thread.join().unwrap();
    fake_agent.join().unwrap();
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert!(reports[0].contains("isn't in the sandbox's `ssh-keys`"), "{reports:?}");
}

#[test]
fn a_gate_applies_only_under_a_git_host_claim_and_reads_ssh_keys_per_connection() {
    let dir = std::env::temp_dir().join(format!("dsv-agent-gate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mine = client_key(1);
    let toml = |keys: &str| format!("[sandbox.web]\nfolder = \".\"\nssh-keys = [{keys}]\n");
    std::fs::write(dir.join(crate::config::CONFIG_FILE), toml("")).unwrap();
    let mut info: Instance = toml::from_str(&format!(
        "sandbox = \"web\"\ncontainer = \"devsandbox-web\"\nfolder = \"/w\"\nworkspace = \"/workspaces/w\"\ncreated_unix = 0\nconfig_dir = \"{}\"\n",
        dir.display()
    ))
    .unwrap();
    let reports: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let report: Report = {
        let reports = Arc::clone(&reports);
        Arc::new(move |why| reports.lock().unwrap().push(why))
    };
    assert!(Gate::for_instance(&info, Arc::clone(&report)).is_none(), "no claim: the agent passes as is");
    info.claims = vec![crate::credentials::instance::InstanceClaim {
        kind: "github".into(),
        feature: "./gh".into(),
        version: "v".into(),
        claim: "{}".into(),
        phantom: "p".into(),
        listeners: Vec::new(),
        files: Vec::new(),
        git_hosts: vec!["github.com".into()],
    }];
    let gate = Gate::for_instance(&info, report).unwrap();
    let list = |gate: &Gate| {
        let (mut client, client_end) = UnixStream::pair().unwrap();
        let (agent_end, mut agent) = UnixStream::pair().unwrap();
        let listing = answer(&[(&mine, "me")]);
        let fake = std::thread::spawn(move || {
            read_message(&mut agent).unwrap().unwrap();
            write_message(&mut agent, &listing).unwrap();
        });
        write_message(&mut client, &[REQUEST_IDENTITIES]).unwrap();
        let got = std::thread::scope(|s| {
            s.spawn(|| gate.serve(client_end, agent_end));
            let got = read_message(&mut client).unwrap().unwrap();
            drop(client);
            got
        });
        fake.join().unwrap();
        got
    };
    assert_eq!(list(&gate), answer(&[]));
    std::fs::write(dir.join(crate::config::CONFIG_FILE), toml(&format!("{{ key = \"SHA256:{}\" }}", fingerprint_b64(&mine))))
        .unwrap();
    assert_eq!(list(&gate), answer(&[(&mine, "me")]));
    std::fs::write(dir.join(crate::config::CONFIG_FILE), "not toml [").unwrap();
    assert_eq!(list(&gate), answer(&[]));
    assert!(reports.lock().unwrap().iter().any(|r| r.contains("didn't load")), "{:?}", reports.lock().unwrap());
    std::fs::remove_dir_all(&dir).unwrap();
}
