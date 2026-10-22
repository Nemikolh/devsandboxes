//! Who a sandbox runs as, per runtime mode (docs/plans/active/identity.md,
//! sections 2–3). The same devcontainer user fields mean opposite things on
//! rootful docker, rootless docker, rootless podman and VM-backed runtimes, so
//! the user model is decided here from the probed mode instead of taken
//! literally: [`classify`] turns the canary's two host-side `stat`s into a
//! [`Mode`], and [`plan`] maps (mode, [`Identity`], adopted account) to a
//! [`Plan`]. Pure: no I/O and no `backend()`, so every row is table-tested;
//! running the canary and applying the plan belong to the callers.

use serde::{Deserialize, Serialize};

/// The user-namespace situation of a runtime context, as the probe found it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// uid 0 inside is host root, uid 1000 inside is host 1000.
    Rootful,
    /// uid 0 inside is the host user; every other uid is a subuid.
    RootlessDocker,
    /// Same mapping as rootless docker, but podman's `keep-id` can map the
    /// host user onto a non-root uid.
    RootlessPodman,
    /// A VM's file share translates ownership (Docker Desktop, OrbStack,
    /// colima, podman machine, Apple `container`).
    VmBacked,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Rootful => "rootful",
            Mode::RootlessDocker => "rootless-docker",
            Mode::RootlessPodman => "rootless-podman",
            Mode::VmBacked => "vm",
        }
    }

    fn parse(s: &str) -> Option<Mode> {
        [Mode::Rootful, Mode::RootlessDocker, Mode::RootlessPodman, Mode::VmBacked]
            .into_iter()
            .find(|m| m.as_str() == s)
    }

    /// How [`Plan::summary`] names the mode.
    fn describe(self) -> &'static str {
        match self {
            Mode::Rootful => "rootful",
            Mode::RootlessDocker => "rootless docker",
            Mode::RootlessPodman => "rootless podman",
            Mode::VmBacked => "VM-backed",
        }
    }
}

/// A probe's result for one runtime context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Probe {
    pub mode: Mode,
    /// SELinux is enforcing: binds need relabeling or every access is EACCES.
    pub selinux: bool,
}

/// The sandbox's `identity` field: what the user wants, as opposed to the
/// devcontainer fields' mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Identity {
    /// Inside you are the adopted account, with the host's uid/gid.
    #[default]
    Host,
    /// Users taken literally, no remapping (the pre-identity behavior).
    Image,
}

impl Identity {
    fn as_str(self) -> &'static str {
        match self {
            Identity::Host => "host",
            Identity::Image => "image",
        }
    }

    fn parse(s: &str) -> Option<Identity> {
        match s {
            "host" => Some(Identity::Host),
            "image" => Some(Identity::Image),
            _ => None,
        }
    }
}

/// A uid/gid pair: the host user's, or a file's owner as `stat` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ids {
    pub uid: u32,
    pub gid: u32,
}

/// The ids the adopted account is assumed to have (*Assumed ids*): what
/// devcontainer base images and common-utils' `automatic` user use. The
/// canary writes its non-root file as this uid too.
pub const ASSUMED: Ids = Ids { uid: 1000, gid: 1000 };

/// Which rootless runtime ran the canary: the stats alone can't tell rootless
/// docker from rootless podman, the backend's name can.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Docker,
    Podman,
}

/// Classify the canary: `as_root` and `as_1000` are the host-side owners of
/// the files written inside as uid 0 and uid 1000. Only uids are compared;
/// gids follow them and a squashing share may not translate groups the same
/// way. `None` when the outcome matches no known mode.
pub fn classify(host: Ids, as_root: Ids, as_1000: Ids, engine: Engine) -> Option<Mode> {
    if as_root.uid == 0 && as_1000.uid == ASSUMED.uid {
        Some(Mode::Rootful)
    } else if as_root.uid == host.uid && as_1000.uid == host.uid {
        // Everything squashed onto the host user: a VM's file share.
        Some(Mode::VmBacked)
    } else if as_root.uid == host.uid && as_1000.uid != 0 {
        // Root is you, 1000 a subuid.
        Some(match engine {
            Engine::Docker => Mode::RootlessDocker,
            Engine::Podman => Mode::RootlessPodman,
        })
    } else {
        None
    }
}

/// The devcontainer user fields a plan starts from. The image's `USER` is the
/// caller's to read (from the image config); `None` there means unknown or
/// unset.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Accounts<'a> {
    pub container_user: Option<&'a str>,
    pub remote_user: Option<&'a str>,
    pub image_user: Option<&'a str>,
}

impl<'a> Accounts<'a> {
    /// The account `identity = "host"` adopts: `remoteUser`, else
    /// `containerUser`, else the image's `USER`, as an account name (a
    /// `user:group` spec loses its group). `None` when that is root or
    /// empty: there is no account to adopt.
    pub fn adopted(&self) -> Option<&'a str> {
        let spec = self.remote_user.or(self.container_user).or(self.image_user)?;
        let user = spec.split(':').next().unwrap_or_default().trim();
        (!user.is_empty() && user != "root" && user != "0").then_some(user)
    }
}

/// The account `devsbd identity apply` moves to the host's ids (rootful).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remap {
    pub user: String,
    pub uid: u32,
    pub gid: u32,
}

/// How a sandbox is created and exec'd into, decided once at create and
/// recorded on the container ([`Plan::label`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub mode: Mode,
    pub identity: Identity,
    /// The adopted account (`identity = "host"` with one to adopt).
    pub account: Option<String>,
    /// `--user` at create; `None` leaves the image's (or keep-id's) user.
    pub create_user: Option<String>,
    /// `--userns` at create.
    pub userns: Option<String>,
    /// Move the account to these ids after create.
    pub remap: Option<Remap>,
    /// `-u` for every exec; `None` leaves the container's user.
    pub exec_user: Option<String>,
    /// Exec as root with `HOME` = the adopted account's home, chowned to 0
    /// (rootless docker, where root is you).
    pub root_home: bool,
    /// Relabel binds (SELinux enforcing).
    pub relabel: bool,
    /// Said once by `run`, never recorded.
    pub warnings: Vec<String>,
}

const ROOT: &str = "root";

/// The warning for an image with nothing to adopt (*No account to adopt*).
pub const NO_ACCOUNT_WARNING: &str = "no non-root account to adopt (remoteUser, containerUser and the image's USER are unset or root): running as root; \
     add the `ghcr.io/devcontainers/features/common-utils` feature to the config for one";

/// The strategy table (section 3) plus *No account to adopt*.
/// `Identity::Image` is the pre-identity behavior: create as
/// `containerUser`, exec as `remoteUser` (else `containerUser`), nothing
/// remapped. Relabeling applies to every plan.
pub fn plan(probe: Probe, identity: Identity, accounts: Accounts, host: Ids) -> Plan {
    let mut p = Plan {
        mode: probe.mode,
        identity,
        account: None,
        create_user: None,
        userns: None,
        remap: None,
        exec_user: None,
        root_home: false,
        relabel: probe.selinux,
        warnings: Vec::new(),
    };
    if identity == Identity::Image {
        p.create_user = accounts.container_user.map(str::to_string);
        p.exec_user = accounts.remote_user.or(accounts.container_user).map(str::to_string);
        return p;
    }
    let Some(account) = accounts.adopted() else {
        p.create_user = Some(ROOT.into());
        p.exec_user = Some(ROOT.into());
        // Under rootless docker root is the plan anyway.
        if probe.mode != Mode::RootlessDocker {
            p.warnings.push(NO_ACCOUNT_WARNING.into());
        }
        return p;
    };
    p.account = Some(account.into());
    match probe.mode {
        Mode::Rootful => {
            p.create_user = Some(ROOT.into());
            p.remap = Some(Remap { user: account.into(), uid: host.uid, gid: host.gid });
            p.exec_user = Some(account.into());
        }
        Mode::RootlessDocker => {
            p.create_user = Some(ROOT.into());
            p.exec_user = Some(ROOT.into());
            p.root_home = true;
        }
        Mode::RootlessPodman => {
            // keep-id runs the container as the mapped uid unless `--user`
            // says otherwise.
            p.userns = Some(format!("keep-id:uid={},gid={}", ASSUMED.uid, ASSUMED.gid));
            p.exec_user = Some(account.into());
        }
        Mode::VmBacked => {
            p.create_user = accounts.container_user.map(str::to_string);
            p.exec_user = Some(account.into());
        }
    }
    p
}

impl Plan {
    /// One line for `run` and `inspect` (section 6).
    pub fn summary(&self) -> String {
        let mode = self.mode.describe();
        let mut s = match (self.identity, self.mode, self.account.as_deref()) {
            (Identity::Image, ..) => {
                let user = self.exec_user.as_deref().unwrap_or("the image's user");
                format!("{mode}: identity = \"image\", exec as {user}, no remap")
            }
            (Identity::Host, _, None) => format!("{mode}: no account to adopt, exec as root"),
            (Identity::Host, Mode::Rootful, Some(a)) => {
                let (uid, gid) = self.remap.as_ref().map_or((0, 0), |r| (r.uid, r.gid));
                format!("{mode}: exec as {a}, remapped to {uid}:{gid}")
            }
            (Identity::Host, Mode::RootlessDocker, Some(a)) => {
                format!("{mode}: exec as root, root maps to you (HOME of {a})")
            }
            (Identity::Host, Mode::RootlessPodman, Some(a)) => {
                format!("{mode}: exec as {a}, keep-id maps it to you")
            }
            (Identity::Host, Mode::VmBacked, Some(a)) => {
                format!("{mode}: exec as {a}, the file share maps ownership")
            }
        };
        if self.relabel {
            s.push_str(", binds relabeled (SELinux)");
        }
        s
    }

    /// The container label recording the plan: `;`-separated `key=value`
    /// fields (bare keys for flags), unset ones omitted. Warnings aren't
    /// recorded.
    pub fn label(&self) -> String {
        let mut fields = vec![format!("mode={}", self.mode.as_str()), format!("identity={}", self.identity.as_str())];
        let mut opt = |key: &str, v: &Option<String>| {
            if let Some(v) = v {
                fields.push(format!("{key}={}", escape(v)));
            }
        };
        opt("account", &self.account);
        opt("create", &self.create_user);
        opt("userns", &self.userns);
        opt("exec", &self.exec_user);
        if let Some(r) = &self.remap {
            fields.push(format!("remap={}:{}:{}", escape(&r.user), r.uid, r.gid));
        }
        if self.root_home {
            fields.push("root_home".into());
        }
        if self.relabel {
            fields.push("relabel".into());
        }
        fields.join(";")
    }

    /// The inverse of [`Plan::label`]; `None` on anything it wouldn't write.
    pub fn parse_label(s: &str) -> Option<Plan> {
        let (mut mode, mut identity) = (None, None);
        let mut p = Plan {
            mode: Mode::Rootful,
            identity: Identity::Host,
            account: None,
            create_user: None,
            userns: None,
            remap: None,
            exec_user: None,
            root_home: false,
            relabel: false,
            warnings: Vec::new(),
        };
        for field in s.split(';') {
            match field.split_once('=') {
                None => match field {
                    "root_home" => p.root_home = true,
                    "relabel" => p.relabel = true,
                    _ => return None,
                },
                Some((key, v)) => match key {
                    "mode" => mode = Some(Mode::parse(v)?),
                    "identity" => identity = Some(Identity::parse(v)?),
                    "account" => p.account = Some(unescape(v)?),
                    "create" => p.create_user = Some(unescape(v)?),
                    "userns" => p.userns = Some(unescape(v)?),
                    "exec" => p.exec_user = Some(unescape(v)?),
                    "remap" => {
                        let mut it = v.rsplitn(3, ':');
                        let gid = it.next()?.parse().ok()?;
                        let uid = it.next()?.parse().ok()?;
                        let user = unescape(it.next()?)?;
                        p.remap = Some(Remap { user, uid, gid });
                    }
                    _ => return None,
                },
            }
        }
        p.mode = mode?;
        p.identity = identity?;
        Some(p)
    }
}

/// Keep `;` (the field separator) out of values; `%` escapes itself.
fn escape(v: &str) -> String {
    v.replace('%', "%25").replace(';', "%3B")
}

fn unescape(v: &str) -> Option<String> {
    let mut out = String::with_capacity(v.len());
    let mut rest = v;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        match rest.get(i + 1..i + 3)? {
            "25" => out.push('%'),
            "3B" => out.push(';'),
            _ => return None,
        }
        rest = &rest[i + 3..];
    }
    out.push_str(rest);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: Ids = Ids { uid: 501, gid: 20 };
    const MODES: [Mode; 4] = [Mode::Rootful, Mode::RootlessDocker, Mode::RootlessPodman, Mode::VmBacked];

    fn ids(uid: u32, gid: u32) -> Ids {
        Ids { uid, gid }
    }

    fn probe(mode: Mode) -> Probe {
        Probe { mode, selinux: false }
    }

    fn vscode() -> Accounts<'static> {
        Accounts { container_user: Some("vscode"), remote_user: Some("vscode"), image_user: None }
    }

    #[test]
    fn classify_outcomes() {
        // host-uid / root / subuid / squashed, and unknown.
        let rootful = classify(HOST, ids(0, 0), ids(1000, 1000), Engine::Docker);
        assert_eq!(rootful, Some(Mode::Rootful));
        let sub = ids(100_999, 100_999);
        assert_eq!(classify(HOST, HOST, sub, Engine::Docker), Some(Mode::RootlessDocker));
        assert_eq!(classify(HOST, HOST, sub, Engine::Podman), Some(Mode::RootlessPodman));
        assert_eq!(classify(HOST, HOST, HOST, Engine::Docker), Some(Mode::VmBacked));
        // A host user that is itself 1000 on a VM share still reads as VM.
        let h1000 = ids(1000, 1000);
        assert_eq!(classify(h1000, h1000, h1000, Engine::Podman), Some(Mode::VmBacked));
        // Root lands on a subuid: no known mode.
        assert_eq!(classify(HOST, sub, sub, Engine::Docker), None);
        // Root is root but 1000 lands elsewhere (e.g. userns-remap daemon).
        assert_eq!(classify(HOST, ids(0, 0), ids(231_072, 231_072), Engine::Docker), None);
        // Root is you but 1000 squashed to root.
        assert_eq!(classify(HOST, HOST, ids(0, 0), Engine::Docker), None);
    }

    #[test]
    fn adopted_account_precedence() {
        let a = |c, r, i| Accounts { container_user: c, remote_user: r, image_user: i };
        assert_eq!(a(Some("c"), Some("r"), Some("i")).adopted(), Some("r"));
        assert_eq!(a(Some("c"), None, Some("i")).adopted(), Some("c"));
        assert_eq!(a(None, None, Some("node:node")).adopted(), Some("node"));
        assert_eq!(a(None, None, None).adopted(), None);
        assert_eq!(a(None, None, Some("")).adopted(), None);
        assert_eq!(a(None, None, Some("0:0")).adopted(), None);
        // remoteUser root wins over a non-root containerUser.
        assert_eq!(a(Some("vscode"), Some("root"), None).adopted(), None);
    }

    #[test]
    fn host_plan_per_mode() {
        let p = plan(probe(Mode::Rootful), Identity::Host, vscode(), HOST);
        assert_eq!(p.create_user.as_deref(), Some("root"));
        assert_eq!(p.remap, Some(Remap { user: "vscode".into(), uid: 501, gid: 20 }));
        assert_eq!(p.exec_user.as_deref(), Some("vscode"));
        assert_eq!((p.userns, p.root_home), (None, false));

        let p = plan(probe(Mode::RootlessDocker), Identity::Host, vscode(), HOST);
        assert_eq!(p.create_user.as_deref(), Some("root"));
        assert_eq!(p.exec_user.as_deref(), Some("root"));
        assert_eq!(p.account.as_deref(), Some("vscode"));
        assert!(p.root_home);
        assert_eq!((p.userns, p.remap), (None, None));

        let p = plan(probe(Mode::RootlessPodman), Identity::Host, vscode(), HOST);
        assert_eq!(p.userns.as_deref(), Some("keep-id:uid=1000,gid=1000"));
        assert_eq!(p.exec_user.as_deref(), Some("vscode"));
        assert_eq!((p.create_user, p.remap, p.root_home), (None, None, false));

        let p = plan(probe(Mode::VmBacked), Identity::Host, vscode(), HOST);
        assert_eq!(p.create_user.as_deref(), Some("vscode"));
        assert_eq!(p.exec_user.as_deref(), Some("vscode"));
        assert_eq!((p.userns, p.remap, p.root_home), (None, None, false));

        for mode in MODES {
            let p = plan(probe(mode), Identity::Host, vscode(), HOST);
            assert!(p.warnings.is_empty(), "{mode:?}");
            assert!(!p.relabel, "{mode:?}");
        }
    }

    #[test]
    fn host_plan_adopts_image_user() {
        let accounts = Accounts { image_user: Some("node"), ..Default::default() };
        let p = plan(probe(Mode::Rootful), Identity::Host, accounts, HOST);
        assert_eq!(p.remap.map(|r| r.user).as_deref(), Some("node"));
        assert_eq!(p.exec_user.as_deref(), Some("node"));
    }

    #[test]
    fn no_account_runs_as_root_in_every_mode() {
        for mode in MODES {
            let p = plan(probe(mode), Identity::Host, Accounts::default(), HOST);
            assert_eq!(p.create_user.as_deref(), Some("root"), "{mode:?}");
            assert_eq!(p.exec_user.as_deref(), Some("root"), "{mode:?}");
            assert_eq!((&p.account, &p.userns, &p.remap, p.root_home), (&None, &None, &None, false), "{mode:?}");
            let want: &[&str] = if mode == Mode::RootlessDocker { &[] } else { &[NO_ACCOUNT_WARNING] };
            assert_eq!(p.warnings, want, "{mode:?}");
        }
    }

    #[test]
    fn image_identity_is_todays_behavior_in_every_mode() {
        let accounts = Accounts { container_user: Some("app:app"), remote_user: Some("dev"), image_user: Some("x") };
        let only_container = Accounts { container_user: Some("app"), ..Default::default() };
        for mode in MODES {
            let p = plan(probe(mode), Identity::Image, accounts, HOST);
            assert_eq!(p.create_user.as_deref(), Some("app:app"), "{mode:?}");
            assert_eq!(p.exec_user.as_deref(), Some("dev"), "{mode:?}");
            assert_eq!((&p.account, &p.userns, &p.remap, p.root_home), (&None, &None, &None, false), "{mode:?}");
            assert!(p.warnings.is_empty());

            let p = plan(probe(mode), Identity::Image, only_container, HOST);
            assert_eq!(p.exec_user.as_deref(), Some("app"), "{mode:?}");

            let p = plan(probe(mode), Identity::Image, Accounts::default(), HOST);
            assert_eq!((p.create_user, p.exec_user), (None, None), "{mode:?}");
            assert!(p.warnings.is_empty());
        }
    }

    #[test]
    fn selinux_relabels_every_plan() {
        for mode in MODES {
            for identity in [Identity::Host, Identity::Image] {
                let p = plan(Probe { mode, selinux: true }, identity, vscode(), HOST);
                assert!(p.relabel, "{mode:?} {identity:?}");
            }
        }
    }

    #[test]
    fn summaries() {
        let s = |mode, identity, accounts| plan(probe(mode), identity, accounts, HOST).summary();
        assert_eq!(
            s(Mode::RootlessDocker, Identity::Host, vscode()),
            "rootless docker: exec as root, root maps to you (HOME of vscode)"
        );
        assert_eq!(s(Mode::Rootful, Identity::Host, vscode()), "rootful: exec as vscode, remapped to 501:20");
        assert_eq!(s(Mode::RootlessPodman, Identity::Host, vscode()), "rootless podman: exec as vscode, keep-id maps it to you");
        assert_eq!(s(Mode::VmBacked, Identity::Host, Accounts::default()), "VM-backed: no account to adopt, exec as root");
        assert_eq!(s(Mode::Rootful, Identity::Image, vscode()), "rootful: identity = \"image\", exec as vscode, no remap");
        let p = plan(Probe { mode: Mode::Rootful, selinux: true }, Identity::Host, vscode(), HOST);
        assert!(p.summary().ends_with(", binds relabeled (SELinux)"));
    }

    #[test]
    fn label_round_trips() {
        let odd = Accounts { container_user: Some("a;b%c:grp"), remote_user: None, image_user: None };
        for mode in MODES {
            for selinux in [false, true] {
                for identity in [Identity::Host, Identity::Image] {
                    for accounts in [vscode(), Accounts::default(), odd] {
                        let mut p = plan(Probe { mode, selinux }, identity, accounts, HOST);
                        let label = p.label();
                        p.warnings.clear();
                        assert_eq!(Plan::parse_label(&label), Some(p), "{label}");
                    }
                }
            }
        }
        let label = plan(probe(Mode::RootlessPodman), Identity::Host, vscode(), HOST).label();
        assert_eq!(label, "mode=rootless-podman;identity=host;account=vscode;userns=keep-id:uid=1000,gid=1000;exec=vscode");
    }

    #[test]
    fn parse_label_rejects_garbage() {
        for bad in ["", "identity=host", "mode=rootful", "mode=nope;identity=host", "mode=rootful;identity=host;x=1"] {
            assert_eq!(Plan::parse_label(bad), None, "{bad}");
        }
        assert_eq!(Plan::parse_label("mode=rootful;identity=host;exec=a%zz"), None);
        assert_eq!(Plan::parse_label("mode=rootful;identity=host;remap=u:x:1"), None);
    }

    #[test]
    fn identity_serde_is_lowercase() {
        #[derive(Deserialize, Serialize)]
        struct T {
            identity: Identity,
        }
        let t: T = toml::from_str("identity = \"image\"").unwrap();
        assert_eq!(t.identity, Identity::Image);
        assert_eq!(toml::to_string(&T { identity: Identity::Host }).unwrap().trim(), "identity = \"host\"");
        assert!(toml::from_str::<T>("identity = \"Host\"").is_err());
    }
}
