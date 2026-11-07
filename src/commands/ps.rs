use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::commands::status::Envelope;
use crate::render::table;
use crate::runtime::{backend, ContainerRow, NAME_PREFIX};
use crate::snapshot::{allowed_now, profile_disallowed, PhaseRow};
use crate::state::{Instance, PhaseView, State};

const HEADERS: [&str; 3] = ["NAME", "IMAGE", "STATUS"];
/// With a PROFILE column: only when some row has a profile.
const PROFILE_HEADERS: [&str; 4] = ["NAME", "IMAGE", "STATUS", "PROFILE"];

/// One `ps --json` row: the runtime's listing plus the state facts `ps`
/// shows: whether the container's instance is done, its phase while it
/// is being materialized or failed (`snapshot::PhaseRow`, absent when ready),
/// and its profile, flagged when its sandbox no longer allows it.
/// Also the API's `containers.list` rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PsRow {
    #[serde(flatten)]
    pub row: ContainerRow,
    pub done: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<PhaseRow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// As `snapshot::InstanceRow::profile_disallowed`.
    #[serde(default)]
    pub profile_disallowed: bool,
}

pub fn ps(all: bool, json: bool) -> Result<()> {
    // Best-effort: `ps` is a runtime listing, so an unreadable state file
    // only loses the done marks.
    let rows = rows(backend().list(all, NAME_PREFIX)?, &State::load().unwrap_or_default());
    print!("{}", render(rows, json)?);
    Ok(())
}

/// `ps`'s rows from the runtime's `listed` containers: sorted by name,
/// joined with `state` ([`join_state`]). Shared with `containers.list`.
/// The config roots of instances with a profile are read (each once,
/// best-effort) to flag one no longer allowed.
pub(crate) fn rows(mut listed: Vec<ContainerRow>, state: &State) -> Vec<PsRow> {
    listed.sort_by(|a, b| a.name.cmp(&b.name));
    let mut configs = BTreeMap::new();
    join_state(listed, state, Instance::phase_view, |i| allowed_now(&mut configs, i))
}

/// What `ps` prints for `rows`: the `--json` envelope (one line ending), or
/// the table. Shared with the run on a machine
/// (`machines::cli::calls`).
pub(crate) fn render(rows: Vec<PsRow>, json: bool) -> Result<String> {
    if json {
        let out = serde_json::to_string_pretty(&Envelope::new(rows)).context("serialize ps")?;
        return Ok(format!("{out}\n"));
    }
    let cells: Vec<[String; 4]> = rows.into_iter().map(cells).collect();
    if cells.iter().any(|[.., profile]| !profile.is_empty()) {
        // A row without a profile would end in STATUS's padding.
        let out = table(&PROFILE_HEADERS, &cells);
        return Ok(out.lines().map(|l| format!("{}\n", l.trim_end())).collect());
    }
    let cells: Vec<[String; 3]> = cells.into_iter().map(|[name, image, status, _]| [name, image, status]).collect();
    Ok(table(&HEADERS, &cells))
}

/// Join the rows with their instance by container name: done marks, the
/// phase (`phase` is `Instance::phase_view`, injected so tests don't probe
/// pids) and the profile (`allowed` is `snapshot::allowed_now`, injected so
/// tests read no config; asked only for an instance with a profile).
/// Service containers are never in state, so none applies to them.
/// A listing, so an instance without a container (yet) has no row.
fn join_state(
    rows: Vec<ContainerRow>,
    state: &State,
    phase: impl Fn(&Instance) -> Option<PhaseView>,
    mut allowed: impl FnMut(&Instance) -> Option<Vec<String>>,
) -> Vec<PsRow> {
    let done: BTreeSet<&str> =
        state.instances.values().filter(|i| i.done.is_some()).map(|i| i.container.as_str()).collect();
    rows.into_iter()
        .map(|row| {
            let inst = state.instances.values().find(|i| i.container == row.name);
            let phase = inst.and_then(&phase).map(PhaseRow::from);
            let profile = inst.and_then(|i| i.profile.clone());
            let profile_disallowed = match (inst, &profile) {
                (Some(i), Some(p)) => profile_disallowed(Some(p), allowed(i).as_deref()),
                _ => false,
            };
            PsRow { done: done.contains(row.name.as_str()), phase, profile, profile_disallowed, row }
        })
        .collect()
}

/// Table cells. STATUS is the phase while set (the step in flight, or
/// `failed (<step>)`) instead of the runtime's text; a done instance's gets
/// a `(done)` suffix. PROFILE is empty without one, `<p> (not allowed)` when
/// its sandbox no longer allows it.
fn cells(r: PsRow) -> [String; 4] {
    let status = match r.phase {
        Some(PhaseRow::InFlight { step }) => step,
        Some(PhaseRow::Failed { step, .. }) => format!("failed ({step})"),
        None => r.row.status,
    };
    let status = if r.done { format!("{status} (done)") } else { status };
    let profile = match (r.profile, r.profile_disallowed) {
        (Some(p), true) => format!("{p} (not allowed)"),
        (Some(p), false) => p,
        (None, _) => String::new(),
    };
    [r.row.name, r.row.image, status, profile]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str) -> ContainerRow {
        ContainerRow { name: name.into(), image: "img".into(), status: "Up 1 hour".into(), ..Default::default() }
    }

    #[test]
    fn done_instances_are_marked_by_container_name() {
        let mut state: State = toml::from_str(
            "[instance.a]\nsandbox = \"web\"\ncontainer = \"devsandbox-a\"\nfolder = \"/f\"\nworkspace = \"/w\"\ncreated_unix = 0\n\
             [instance.b]\nsandbox = \"web\"\ncontainer = \"devsandbox-b\"\nfolder = \"/f\"\nworkspace = \"/w\"\ncreated_unix = 0\n",
        )
        .unwrap();
        state.instances.get_mut("a").unwrap().done = Some(1);
        let rows = join_state(vec![row("devsandbox-a"), row("devsandbox-b"), row("devsandbox-svc-redis")], &state, |_| None, |_| None);
        let done: Vec<bool> = rows.iter().map(|r| r.done).collect();
        assert_eq!(done, [true, false, false]);
        let json = serde_json::to_value(&rows[0]).unwrap();
        assert_eq!(json["done"], true);
        assert_eq!(json["name"], "devsandbox-a", "runtime fields stay top-level: {json}");
        let table: Vec<[String; 4]> = rows.into_iter().map(cells).collect();
        assert_eq!(table[0][2], "Up 1 hour (done)");
        assert_eq!(table[1][2], "Up 1 hour");
    }

    #[test]
    fn the_phase_replaces_the_runtime_status() {
        let state: State = toml::from_str(
            "[instance.a]\nsandbox = \"web\"\ncontainer = \"devsandbox-a\"\nfolder = \"/f\"\nworkspace = \"/w\"\ncreated_unix = 0\n\
             [instance.b]\nsandbox = \"web\"\ncontainer = \"devsandbox-b\"\nfolder = \"/f\"\nworkspace = \"/w\"\ncreated_unix = 0\n",
        )
        .unwrap();
        let phase = |i: &Instance| match i.container.as_str() {
            "devsandbox-a" => Some(PhaseView::InFlight("postCreateCommand".into())),
            _ => Some(PhaseView::Failed { at: "postStartCommand".into(), error: "exit status 1".into() }),
        };
        let rows = join_state(vec![row("devsandbox-a"), row("devsandbox-b"), row("devsandbox-svc-redis")], &state, phase, |_| None);
        let json = serde_json::to_value(&rows[1]).unwrap();
        assert_eq!(json["phase"], serde_json::json!({"state": "failed", "step": "postStartCommand", "error": "exit status 1"}));
        assert!(serde_json::to_value(&rows[2]).unwrap().get("phase").is_none(), "service: no phase");
        let table: Vec<[String; 4]> = rows.into_iter().map(cells).collect();
        assert_eq!(table[0][2], "postCreateCommand");
        assert_eq!(table[1][2], "failed (postStartCommand)");
        assert_eq!(table[2][2], "Up 1 hour");
    }

    /// A profile rides along from the instance, flagged when its sandbox no
    /// longer allows it; `allowed` is asked only for instances with one.
    /// The table grows a PROFILE column only when some row has a profile.
    #[test]
    fn profiles_show_and_a_disallowed_one_is_flagged() {
        let mut state: State = toml::from_str(
            "[instance.a]\nsandbox = \"web\"\ncontainer = \"devsandbox-a\"\nfolder = \"/f\"\nworkspace = \"/w\"\ncreated_unix = 0\nprofile = \"work\"\n\
             [instance.b]\nsandbox = \"api\"\ncontainer = \"devsandbox-b\"\nfolder = \"/f\"\nworkspace = \"/w\"\ncreated_unix = 0\nprofile = \"work\"\n\
             [instance.c]\nsandbox = \"web\"\ncontainer = \"devsandbox-c\"\nfolder = \"/f\"\nworkspace = \"/w\"\ncreated_unix = 0\n",
        )
        .unwrap();
        let mut asked = Vec::new();
        let allowed = |i: &Instance| {
            asked.push(i.sandbox.clone());
            match i.sandbox.as_str() {
                "web" => Some(vec!["work".to_string()]),
                _ => Some(vec!["personal".to_string()]),
            }
        };
        let listed = vec![row("devsandbox-a"), row("devsandbox-b"), row("devsandbox-c"), row("devsandbox-svc-redis")];
        let rows = join_state(listed, &state, |_| None, allowed);
        assert_eq!(asked, ["web", "api"]);
        let got: Vec<_> = rows.iter().map(|r| (r.profile.as_deref(), r.profile_disallowed)).collect();
        assert_eq!(got, [(Some("work"), false), (Some("work"), true), (None, false), (None, false)]);
        let json = serde_json::to_value(&rows[1]).unwrap();
        assert_eq!((&json["profile"], &json["profile_disallowed"]), (&serde_json::json!("work"), &serde_json::json!(true)));
        assert!(serde_json::to_value(&rows[2]).unwrap().get("profile").is_none());

        let out = render(rows, false).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].ends_with("STATUS     PROFILE"), "{out}");
        assert!(lines[1].ends_with("  work"), "{out}");
        assert!(lines[2].ends_with("  work (not allowed)"), "{out}");
        assert!(lines[3].ends_with("Up 1 hour"), "{out}");

        // No profile anywhere: the three columns as before.
        state.instances.values_mut().for_each(|i| i.profile = None);
        let rows = join_state(vec![row("devsandbox-a")], &state, |_| None, |_| unreachable!());
        assert!(render(rows, false).unwrap().starts_with("NAME          IMAGE  STATUS\n"));
    }
}
