use anyhow::Result;

use super::resolve_container;
use crate::render::{classify_json_line, dim, green, JsonLine};
use crate::runtime::backend;
use crate::state::{Instance, State};

/// Pretty-print the runtime's `inspect` JSON for an instance or service
/// container, with the same light key highlighting as the TUI inspect pane
/// (color only on a TTY; see `render::paint`). For an instance, its browser
/// sandbox line goes first, on stderr, from the record ([`browser_line`]),
/// then its profile's when it has one ([`profile_line`]), then why it has no
/// helper ([`helper_line`]) or, when it has one, the exec cgroup warning if its execs land outside its cgroup
/// (`run::cgroup`, checked live: no answer from a stopped container).
pub fn inspect(name: &str, json: bool) -> Result<()> {
    let state = State::load()?;
    let container = resolve_container(&state, name)?;
    let raw = backend().inspect_json(&container)?;
    if let Some(info) = state.instances.values().find(|i| i.container == container) {
        eprintln!("{}", browser_line(info));
        if info.profile.is_some() {
            let allowed = crate::snapshot::allowed_now(&mut Default::default(), info);
            eprintln!("{}", profile_line(info, allowed.as_deref()));
        }
        match helper_line(info) {
            Some(line) => eprintln!("{line}"),
            None => crate::commands::run::cgroup::warn_if_outside(&container),
        }
    }
    println!("{}", render(&raw, json));
    Ok(())
}

/// What `inspect` prints of raw inspect output (one `println` of it), shared
/// with the re-run on a machine (`machines::cli::calls`). JSON
/// mode is the runtime's own inspect document verbatim (no color, no
/// envelope): the payload is the backend's schema, not ours.
pub(crate) fn render(raw: &str, json: bool) -> String {
    if json {
        return pretty_inspect(raw);
    }
    pretty_inspect(raw).lines().map(paint_json_line).collect::<Vec<_>>().join("\n")
}

/// `run`'s browser sandbox line for a recorded instance
/// (`run::tool_env::summary`); an instance from before the check says so.
fn browser_line(info: &Instance) -> String {
    if info.browser_sandbox.is_none() && info.tool_env.is_empty() {
        return "browser sandbox: not recorded (created before the userns check; `devsandbox rebuild` checks it)".into();
    }
    use crate::commands::run::tool_env;
    tool_env::summary(tool_env::root_plan(info.remote_user.as_deref()), info.browser_sandbox.as_ref())
}

/// `profile: <p>`, with ` (no longer allowed by the config)` when `allowed`
/// (its sandbox's allowed profiles now, `None` when unknown) lacks it.
fn profile_line(info: &Instance, allowed: Option<&[String]>) -> String {
    let profile = info.profile.as_deref().unwrap_or("-");
    match crate::snapshot::profile_disallowed(info.profile.as_deref(), allowed) {
        true => format!("profile: {profile} (no longer allowed by the config)"),
        false => format!("profile: {profile}"),
    }
}

/// `helper: unavailable: <why>` for an instance recorded without the helper
/// (no notifications, opener or agent relay), `None` with one.
fn helper_line(info: &Instance) -> Option<String> {
    if info.devsbd_arch.is_some() {
        return None;
    }
    let why = info.devsbd_error.as_deref().unwrap_or("no reason recorded");
    Some(format!("helper: unavailable: {why} (`devsandbox stop` + `start` retries the install)"))
}

/// Pretty-print raw `inspect` output. On JSON parse failure the raw output is
/// kept with the parse error as the first line. Shared with the TUI's inspect
/// pane so both render identically.
pub fn pretty_inspect(raw: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(v) => match serde_json::to_string_pretty(&v) {
            Ok(pretty) => pretty,
            Err(e) => format!("cannot format inspect JSON: {e}\n{raw}"),
        },
        Err(e) => format!("could not parse inspect JSON: {e}\n{raw}"),
    }
}

/// ANSI rendering of one pretty-printed JSON line: structural lines dim,
/// `"key":` prefixes green, everything else verbatim.
fn paint_json_line(line: &str) -> String {
    match classify_json_line(line) {
        JsonLine::Structural => dim(line),
        JsonLine::KeyValue(split) => {
            let (key, value) = line.split_at(split);
            format!("{}{value}", green(key))
        }
        JsonLine::Plain => line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pretty_inspect_formats_valid_json() {
        assert_eq!(
            pretty_inspect(r#"[{"Id":"abc"}]"#),
            "[\n  {\n    \"Id\": \"abc\"\n  }\n]"
        );
    }

    #[test]
    fn profile_line_flags_a_disallowed_profile() {
        let state: State = toml::from_str(
            "[instance.a]\nsandbox = \"web\"\ncontainer = \"c\"\nfolder = \"/f\"\nworkspace = \"/w\"\ncreated_unix = 0\nprofile = \"work\"\n",
        )
        .unwrap();
        let info = &state.instances["a"];
        assert_eq!(profile_line(info, Some(&["work".into()])), "profile: work");
        assert_eq!(profile_line(info, None), "profile: work", "unknown: not flagged");
        assert_eq!(profile_line(info, Some(&["personal".into()])), "profile: work (no longer allowed by the config)");
    }

    #[test]
    fn helper_line_says_why_there_is_no_helper() {
        let state: State = toml::from_str(
            "[instance.a]\nsandbox = \"web\"\ncontainer = \"c\"\nfolder = \"/f\"\nworkspace = \"/w\"\ncreated_unix = 0\n",
        )
        .unwrap();
        let mut info = state.instances["a"].clone();
        let retry = " (`devsandbox stop` + `start` retries the install)";
        assert_eq!(helper_line(&info), Some(format!("helper: unavailable: no reason recorded{retry}")));
        info.devsbd_error = Some("devsbd couldn't run in `c`".into());
        assert_eq!(helper_line(&info), Some(format!("helper: unavailable: devsbd couldn't run in `c`{retry}")));
        info.devsbd_arch = Some(crate::devsbd::Arch::X86_64);
        assert_eq!(helper_line(&info), None);
    }

    #[test]
    fn pretty_inspect_keeps_raw_on_parse_failure() {
        let out = pretty_inspect("not json");
        assert!(out.starts_with("could not parse inspect JSON:"), "{out}");
        assert!(out.ends_with("\nnot json"), "{out}");
    }
}
