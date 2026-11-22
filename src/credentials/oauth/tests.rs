use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustls::RootCertStore;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;

use super::browser::{self, Callback, callback};
use super::device::{self, Authorization, Poller};
use super::*;
use crate::broker::upstream::Timeouts;

/// No sleeping: `sleep` moves `now`.
pub(crate) struct FakeClock {
    pub now: Duration,
    pub slept: Vec<Duration>,
}

impl FakeClock {
    pub(crate) fn new() -> FakeClock {
        FakeClock { now: Duration::ZERO, slept: Vec::new() }
    }
}

pub(crate) const UNIX: u64 = 1_700_000_000;

impl Clock for FakeClock {
    fn now(&self) -> Duration {
        self.now
    }
    fn sleep(&mut self, d: Duration) {
        self.slept.push(d);
        self.now += d;
    }
    fn unix(&self) -> u64 {
        UNIX
    }
}

/// The upstream client trusting the test CA (`tests/fixtures/broker/`),
/// loopback let through.
pub(crate) fn http() -> Http {
    let mut roots = RootCertStore::empty();
    let ca = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/broker/ca.pem");
    roots.add(CertificateDer::from_pem_file(ca).unwrap()).unwrap();
    let timeouts = Timeouts { connect: Duration::from_secs(5), idle: Duration::from_secs(10) };
    Http::loopback(Upstream::new(roots, None, timeouts).unwrap())
}

pub(crate) fn json(status: u16, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// The request line's target.
pub(crate) fn path(head: &str) -> &str {
    head.split(' ').nth(1).unwrap_or_default()
}

pub(crate) fn form(body: &[u8]) -> Vec<(String, String)> {
    form_decode(std::str::from_utf8(body).unwrap())
}

pub(crate) fn field<'a>(form: &'a [(String, String)], key: &str) -> Option<&'a str> {
    form.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

pub(crate) const GITHUB_TOKEN: &str = r#"{"access_token":"ghu_live","expires_in":28800,"refresh_token":"ghr_live","refresh_token_expires_in":15897600,"token_type":"bearer","scope":""}"#;

/// gh's OAuth app's token: no expiry, no refresh token.
pub(crate) const GH_OAUTH_TOKEN: &str = r#"{"access_token":"gho_x","token_type":"bearer","scope":"repo,read:org"}"#;

/// A device-flow provider: `/device` answers a code, `/token` is pending
/// once, asks to slow down once, then issues GitHub's app user token.
pub(crate) fn device_provider() -> (u16, crate::broker::serve::tests::Seen) {
    device_provider_issuing(GITHUB_TOKEN)
}

/// [`device_provider`] issuing `token` (a JSON reply).
pub(crate) fn device_provider_issuing(token: &'static str) -> (u16, crate::broker::serve::tests::Seen) {
    let polls = AtomicUsize::new(0);
    crate::broker::serve::tests::upstream_server(move |head, body| {
        let f = form(body);
        assert!(head.contains("\r\nAccept: application/json\r\n"), "{head}");
        match path(head) {
            "/device" => json(
                200,
                r#"{"device_code":"dev-123","user_code":"WDJB-MJHT","verification_uri":"https://github.com/login/device","expires_in":899,"interval":5}"#,
            ),
            "/token" if field(&f, "device_code") != Some("dev-123") => json(200, r#"{"error":"incorrect_device_code"}"#),
            "/token" => match polls.fetch_add(1, Ordering::SeqCst) {
                0 => json(200, r#"{"error":"authorization_pending","error_description":"wait"}"#),
                1 => json(200, r#"{"error":"slow_down","interval":10}"#),
                _ => json(200, token),
            },
            _ => json(404, "{}"),
        }
    })
}

fn reply(r: Result<Reply>) -> std::result::Result<TokenSet, OAuthError> {
    match r.unwrap() {
        Reply::Token(t) => Ok(t),
        Reply::Error(e) => Err(e),
    }
}

const JSON: Option<&str> = Some("application/json");

#[test]
fn github_token_replies() {
    let t = reply(parse_token_reply(200, JSON, GITHUB_TOKEN.as_bytes())).ok().unwrap();
    assert_eq!(t.access_token.expose(), "ghu_live");
    assert_eq!(t.refresh_token.as_ref().unwrap().expose(), "ghr_live");
    assert_eq!((t.expires_in, t.refresh_token_expires_in, t.scope.as_deref()), (Some(28800), Some(15897600), None));
    let r = t.into_record("https://x/t", "Iv1.c", UNIX);
    assert_eq!((r.expires_at, r.refresh_expires_at), (Some(UNIX + 28800), Some(UNIX + 15897600)));
    assert_eq!((r.token_url.as_str(), r.client_id.as_str()), ("https://x/t", "Iv1.c"));

    // GitHub's errors come with a 200.
    let e = reply(parse_token_reply(200, JSON, br#"{"error":"slow_down","error_description":"Too many","interval":10}"#));
    let e = e.err().unwrap();
    assert_eq!((e.code.as_str(), e.description.as_deref(), e.interval), ("slow_down", Some("Too many"), Some(10)));
    // And a form when not asked for JSON.
    let form = Some("application/x-www-form-urlencoded; charset=utf-8");
    let t = reply(parse_token_reply(200, form, b"access_token=gho_x&scope=repo%2Cgist&token_type=bearer")).ok().unwrap();
    assert_eq!((t.access_token.expose(), t.scope.as_deref(), t.expires_in), ("gho_x", Some("repo,gist"), None));
    let e = reply(parse_token_reply(200, form, b"error=bad_verification_code&error_description=The+code+is+wrong"));
    assert_eq!(e.err().unwrap().to_string(), "bad_verification_code (The code is wrong)");
}

#[test]
fn rfc6749_replies_and_refusals() {
    let e = reply(parse_token_reply(400, JSON, br#"{"error":"invalid_grant","error_description":"bad\ncode"}"#));
    assert_eq!(e.err().unwrap().to_string(), "invalid_grant (bad\\ncode)", "control characters escaped");
    // A string `expires_in`, as some providers send.
    let t = reply(parse_token_reply(200, None, br#"{"access_token":"a","expires_in":"3600","token_type":"Bearer"}"#));
    assert_eq!(t.ok().unwrap().expires_in, Some(3600));

    for (status, body, wants) in [
        (500, &br#"{"access_token":"sekrit"}"#[..], "answered 500 with no access token"),
        (200, br#"{"token_type":"bearer"}"#, "answered 200 with no access token"),
        (200, br#"{"access_token":"sekrit","token_type":"mac"}"#, "a `mac` token, not a bearer one"),
        (200, b"<html>sekrit</html>", "neither a JSON object nor a form"),
        (200, br#"["sekrit"]"#, "neither a JSON object nor a form"),
    ] {
        let err = format!("{:#}", parse_token_reply(status, JSON, body).err().unwrap());
        assert!(err.contains(wants) && !err.contains("sekrit"), "{err}");
    }
}

#[test]
fn forms_encode_and_decode() {
    let enc = form_encode(&[("redirect_uri", "http://127.0.0.1:5/callback"), ("a b", "x+y&z=é~")]);
    assert_eq!(enc, "redirect_uri=http%3A%2F%2F127.0.0.1%3A5%2Fcallback&a%20b=x%2By%26z%3D%C3%A9~");
    let back = form_decode(&enc);
    assert_eq!(back, [("redirect_uri".into(), "http://127.0.0.1:5/callback".into()), ("a b".into(), "x+y&z=é~".into())]);
    assert_eq!(form_decode("a=%zz&b&&c=%4"), [("a".into(), "%zz".into()), ("b".into(), String::new()), ("c".into(), "%4".into())]);
}

#[test]
fn base64url_and_pkce() {
    for (raw, want) in [("", ""), ("f", "Zg"), ("fo", "Zm8"), ("foo", "Zm9v"), ("foob", "Zm9vYg"), ("fooba", "Zm9vYmE")] {
        assert_eq!(base64url(raw.as_bytes()), want);
    }
    assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
    // RFC 7636 appendix B.
    let octets = [
        116, 24, 223, 180, 151, 153, 224, 37, 79, 250, 96, 125, 216, 173, 187, 186, 22, 212, 37, 77, 105, 214, 191, 240,
        91, 88, 5, 88, 83, 132, 141, 121,
    ];
    let verifier = base64url(&octets);
    assert_eq!(verifier, "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
    assert_eq!(pkce_challenge(&verifier), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");

    let (a, b) = (random(32).unwrap(), random(32).unwrap());
    assert_eq!(a.len(), 43, "RFC 7636's 43-character minimum");
    assert_ne!(a, b);
}

#[test]
fn device_authorization_replies() {
    let form = Some("application/x-www-form-urlencoded");
    let a = device::parse_authorization(
        200,
        form,
        b"device_code=d&expires_in=900&interval=5&user_code=AB-CD&verification_uri=https%3A%2F%2Fgithub.com%2Flogin%2Fdevice",
    )
    .unwrap();
    assert_eq!((a.device_code.expose(), a.user_code.as_str(), a.expires_in, a.interval), ("d", "AB-CD", 900, Some(5)));
    assert_eq!(a.verification_uri, "https://github.com/login/device");
    let google = br#"{"device_code":"d","user_code":"u","verification_url":"https://g/device","expires_in":60}"#;
    assert_eq!(device::parse_authorization(200, JSON, google).unwrap().verification_uri, "https://g/device");

    for (status, body, wants) in [
        (200, &br#"{"error":"device_flow_disabled"}"#[..], "refused: device_flow_disabled"),
        (400, br#"{"error":"invalid_client","error_description":"no"}"#, "refused: invalid_client (no)"),
        (500, br#"{"device_code":"sekrit"}"#, "answered 500"),
        (200, br#"{"device_code":"sekrit","verification_uri":"https://x","expires_in":5}"#, "no `user_code`"),
    ] {
        let err = format!("{:#}", device::parse_authorization(status, JSON, body).err().unwrap());
        assert!(err.contains(wants) && !err.contains("sekrit"), "{err}");
    }
}

fn auth(expires_in: u64, interval: Option<u64>) -> Authorization {
    Authorization {
        device_code: Secret::new("d".into()),
        user_code: "u".into(),
        verification_uri: "https://x".into(),
        verification_uri_complete: None,
        expires_in,
        interval,
    }
}

fn err(code: &str, interval: Option<u64>) -> Reply {
    Reply::Error(OAuthError { code: code.into(), description: None, interval })
}

fn token() -> Reply {
    parse_token_reply(200, JSON, GITHUB_TOKEN.as_bytes()).unwrap()
}

/// `device::poll` over `replies`, in order: what it ended in, and the
/// sleeps between.
fn run_poll(a: &Authorization, replies: Vec<Reply>) -> (Result<TokenSet>, Vec<u64>) {
    let mut clock = FakeClock::new();
    let mut replies = replies.into_iter();
    let out = device::poll(a, &mut clock, || Ok(replies.next().expect("polled past the script")));
    (out, clock.slept.iter().map(Duration::as_secs).collect())
}

#[test]
fn the_device_poll_waits_slows_down_and_ends() {
    let (out, slept) = run_poll(&auth(900, Some(5)), vec![err("authorization_pending", None), token()]);
    assert_eq!(out.unwrap().access_token.expose(), "ghu_live");
    assert_eq!(slept, [5, 5]);

    // `slow_down` adds 5 s, or takes the provider's interval when longer,
    // and it sticks.
    let replies = vec![err("slow_down", None), err("slow_down", Some(30)), err("authorization_pending", None), token()];
    let (out, slept) = run_poll(&auth(900, None), replies);
    assert!(out.is_ok());
    assert_eq!(slept, [5, 10, 30, 30], "5 s by default");

    for (code, wants) in [
        ("access_denied", "denied at the provider"),
        ("expired_token", "the code expired"),
        ("unsupported_grant_type", "refused: unsupported_grant_type"),
    ] {
        let (out, _) = run_poll(&auth(900, Some(1)), vec![err("authorization_pending", None), err(code, None)]);
        let e = out.err().unwrap().to_string();
        assert!(e.contains(wants), "{code}: {e}");
    }

    // Past `expires_in` it stops without asking again.
    let (out, slept) = run_poll(&auth(12, Some(5)), vec![err("authorization_pending", None), err("authorization_pending", None)]);
    assert!(out.err().unwrap().to_string().contains("the code expired"));
    assert_eq!(slept, [5, 5, 5]);

    let mut p = Poller::new(&auth(10, Some(0)), Duration::from_secs(7));
    assert_eq!(p, Poller { interval: Duration::from_secs(1), deadline: Duration::from_secs(17) }, "at least 1 s");
    assert!(matches!(p.next(err("authorization_pending", None)).unwrap(), device::Step::Again));
}

struct Seen {
    shown: String,
    opened: Vec<String>,
}

impl Ui for Seen {
    fn show(&mut self, text: &str) {
        self.shown.push_str(text);
    }
    fn open(&mut self, url: &str) {
        self.opened.push(url.to_string());
    }
}

#[test]
fn device_login_end_to_end() {
    let (port, seen) = device_provider();
    let at = device::Endpoints {
        client_id: "Iv1.mine",
        device_url: &format!("https://127.0.0.1:{port}/device"),
        token_url: &format!("https://127.0.0.1:{port}/token"),
        scope: None,
    };
    let mut ui = Seen { shown: String::new(), opened: Vec::new() };
    let mut clock = FakeClock::new();
    let rec = device::login(&http(), &at, &mut clock, &mut ui).unwrap();
    assert_eq!(rec.access_token.expose(), "ghu_live");
    assert_eq!(rec.refresh_token.unwrap().expose(), "ghr_live");
    assert_eq!((rec.expires_at, rec.client_id.as_str()), (Some(UNIX + 28800), "Iv1.mine"));
    assert_eq!(rec.token_url, at.token_url);
    assert!(ui.shown.contains("Open https://github.com/login/device and enter the code WDJB-MJHT"), "{}", ui.shown);
    assert_eq!(ui.opened, ["https://github.com/login/device"]);
    assert_eq!(clock.slept.iter().map(Duration::as_secs).collect::<Vec<_>>(), [5, 5, 10]);

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 4);
    assert_eq!(form(&seen[0].1), [("client_id".into(), "Iv1.mine".into())]);
    let poll = form(&seen[1].1);
    assert_eq!(field(&poll, "grant_type"), Some("urn:ietf:params:oauth:grant-type:device_code"));
    assert_eq!((field(&poll, "client_id"), field(&poll, "device_code")), (Some("Iv1.mine"), Some("dev-123")));
}

/// gh's own app: the code request carries the scope; the token has no
/// expiry and no refresh token, which the renewer leaves alone.
#[test]
fn device_login_with_a_scope_and_a_token_that_never_expires() {
    let (port, seen) = device_provider_issuing(GH_OAUTH_TOKEN);
    let at = device::Endpoints {
        client_id: "178c6fc778ccc68e1d6a",
        device_url: &format!("https://127.0.0.1:{port}/device"),
        token_url: &format!("https://127.0.0.1:{port}/token"),
        scope: Some("repo read:org"),
    };
    let mut ui = Seen { shown: String::new(), opened: Vec::new() };
    let rec = device::login(&http(), &at, &mut FakeClock::new(), &mut ui).unwrap();
    assert_eq!(rec.access_token.expose(), "gho_x");
    assert_eq!((rec.refresh_token.is_none(), rec.expires_at, rec.refresh_expires_at), (true, None, None));
    assert_eq!(rec.scope.as_deref(), Some("repo,read:org"));
    assert!(!refresh::due(&rec, u64::MAX), "never due");
    let code = form(&seen.lock().unwrap()[0].1);
    assert_eq!(code, [("client_id".into(), "178c6fc778ccc68e1d6a".into()), ("scope".into(), "repo read:org".into())]);
}

#[test]
fn the_oauth_endpoints_take_the_address_check() {
    let (port, seen) = device_provider();
    let public = Http { upstream: http().upstream, private: false };
    let at = device::Endpoints {
        client_id: "c",
        device_url: &format!("https://127.0.0.1:{port}/device"),
        token_url: &format!("https://127.0.0.1:{port}/token"),
        scope: None,
    };
    let mut ui = Seen { shown: String::new(), opened: Vec::new() };
    let err = format!("{:#}", device::login(&public, &at, &mut FakeClock::new(), &mut ui).err().unwrap());
    assert!(err.contains("loopback, link-local, private or reserved"), "{err}");
    assert!(seen.lock().unwrap().is_empty());
}

#[test]
fn callbacks_count_only_with_this_state() {
    let s = "st4te";
    assert_eq!(callback("GET", "/callback?code=abc&state=st4te", s), Callback::Code("abc".into()));
    assert_eq!(callback("GET", "/callback?state=st4te&code=a%2Fb", s), Callback::Code("a/b".into()));
    for (target, status) in [
        ("/callback?code=abc&state=other", 400),
        ("/callback?code=abc&state=st4t", 400),
        ("/callback?code=abc", 400),
        ("/callback?code=abc&state=st4te&state=st4te", 400),
        ("/callback?code=a&code=b&state=st4te", 400),
        ("/callback?state=st4te", 400),
        ("/callback?code=&state=st4te", 400),
        ("/callback?error=access_denied&state=other", 400),
        ("/favicon.ico", 404),
        ("/callbackx?code=abc&state=st4te", 404),
    ] {
        assert!(matches!(callback("GET", target, s), Callback::Refused(st, _) if st == status), "{target}");
    }
    assert!(matches!(callback("POST", "/callback?code=abc&state=st4te", s), Callback::Refused(405, _)));
    assert_eq!(
        callback("GET", "/callback?error=access_denied&error_description=The+user+said%0Ano&state=st4te", s),
        Callback::Denied("access_denied (The user said\\nno)".into())
    );
}

#[test]
fn authorize_urls() {
    let u = browser::authorize_url("https://github.com/login/oauth/authorize", "Iv1.c", None, "http://127.0.0.1:9/callback", "s", "ch");
    assert_eq!(
        u,
        "https://github.com/login/oauth/authorize?response_type=code&client_id=Iv1.c&\
         redirect_uri=http%3A%2F%2F127.0.0.1%3A9%2Fcallback&state=s&code_challenge=ch&code_challenge_method=S256"
    );
    assert!(browser::authorize_url("https://x/a?prompt=consent", "c", None, "r", "s", "ch").starts_with("https://x/a?prompt=consent&response_type=code&"));
    let u = browser::authorize_url("https://x/a", "c", Some("repo read:org"), "r", "s", "ch");
    assert!(u.ends_with("&code_challenge_method=S256&scope=repo%20read%3Aorg"), "{u}");
}

/// Plays the browser: a stray request, one with a forged state, then the
/// provider's redirect with the code.
pub(crate) fn browser_opener(challenge: Arc<Mutex<Option<String>>>) -> impl FnMut(&str) + Send + 'static {
    move |url: &str| {
        let query = form_decode(url.split_once('?').unwrap().1);
        let get = |k: &str| query.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap();
        assert_eq!(get("code_challenge_method"), "S256");
        *challenge.lock().unwrap() = Some(get("code_challenge"));
        let (redirect, state) = (get("redirect_uri"), get("state"));
        std::thread::spawn(move || {
            let rest = redirect.strip_prefix("http://").unwrap();
            let (authority, path) = rest.split_once('/').unwrap();
            let call = |target: String| {
                let mut tcp = TcpStream::connect(authority).unwrap();
                write!(tcp, "GET {target} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n").unwrap();
                let mut out = String::new();
                tcp.read_to_string(&mut out).unwrap();
                out
            };
            assert!(call("/favicon.ico".into()).starts_with("HTTP/1.1 404 "));
            assert!(call(format!("/{path}?code=forged&state=nope")).starts_with("HTTP/1.1 400 "));
            let ok = call(format!("/{path}?code=the-code&state={state}"));
            assert!(ok.starts_with("HTTP/1.1 200 ") && ok.contains("Logged in"), "{ok}");
        });
    }
}

/// A token endpoint for the authorization code: `the-code` with the
/// verifier matching the challenge the browser carried.
pub(crate) fn code_provider(challenge: Arc<Mutex<Option<String>>>) -> (u16, crate::broker::serve::tests::Seen) {
    crate::broker::serve::tests::upstream_server(move |head, body| {
        let f = form(body);
        let verified = field(&f, "code_verifier").map(pkce_challenge) == *challenge.lock().unwrap();
        match (path(head), field(&f, "grant_type"), field(&f, "code")) {
            ("/token", Some("authorization_code"), Some("the-code")) if verified => json(200, GITHUB_TOKEN),
            ("/token", ..) => json(400, r#"{"error":"invalid_grant"}"#),
            _ => json(404, "{}"),
        }
    })
}

#[test]
fn browser_login_end_to_end() {
    let challenge = Arc::new(Mutex::new(None));
    let (port, seen) = code_provider(challenge.clone());
    let at = browser::Endpoints {
        client_id: "Iv1.c",
        authorize_url: "https://github.example/login/oauth/authorize",
        token_url: &format!("https://127.0.0.1:{port}/token"),
        scope: None,
    };
    struct Browser<F>(F, String);
    impl<F: FnMut(&str)> Ui for Browser<F> {
        fn show(&mut self, text: &str) {
            self.1.push_str(text);
        }
        fn open(&mut self, url: &str) {
            (self.0)(url)
        }
    }
    let mut ui = Browser(browser_opener(challenge), String::new());
    let rec = browser::login(&http(), &at, Duration::from_secs(20), &mut SystemClock::new(), &mut ui).unwrap();
    assert_eq!((rec.access_token.expose(), rec.client_id.as_str()), ("ghu_live", "Iv1.c"));
    assert!(ui.1.contains("visit:\n  https://github.example/login/oauth/authorize?response_type=code&"), "{}", ui.1);

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let f = form(&seen[0].1);
    assert!(field(&f, "redirect_uri").unwrap().starts_with("http://127.0.0.1:"));
    assert!(field(&f, "redirect_uri").unwrap().ends_with("/callback"));
    assert_eq!(field(&f, "client_id"), Some("Iv1.c"));
}

#[test]
fn browser_login_times_out_and_takes_a_denial() {
    let at = browser::Endpoints { client_id: "c", authorize_url: "https://x/a", token_url: "https://x/t", scope: None };
    let mut ui = Seen { shown: String::new(), opened: Vec::new() };
    let err = browser::login(&http(), &at, Duration::from_millis(200), &mut SystemClock::new(), &mut ui).err().unwrap();
    assert!(err.to_string().contains("no login callback within"), "{err}");

    struct Deny;
    impl Ui for Deny {
        fn show(&mut self, _: &str) {}
        fn open(&mut self, url: &str) {
            let query = form_decode(url.split_once('?').unwrap().1);
            let get = |k: &str| query.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap();
            let (redirect, state) = (get("redirect_uri"), get("state"));
            std::thread::spawn(move || {
                let authority = redirect.strip_prefix("http://").unwrap().split_once('/').unwrap().0.to_string();
                let mut tcp = TcpStream::connect(&authority).unwrap();
                write!(tcp, "GET /callback?error=access_denied&state={state} HTTP/1.1\r\nHost: {authority}\r\n\r\n").unwrap();
                let _ = tcp.read_to_end(&mut Vec::new());
            });
        }
    }
    let err = browser::login(&http(), &at, Duration::from_secs(20), &mut SystemClock::new(), &mut Deny).err().unwrap();
    assert!(err.to_string().contains("denied at the provider: access_denied"), "{err}");
}
