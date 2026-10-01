//! Fixtures shared by the `app` submodules' tests.

use crate::config::Config;
use crate::tui::data::{ContainerStatus, ServiceRow};
use crate::tui::term::TermSession;

use super::*;

pub(super) fn new_app() -> App {
    App::new(PathBuf::from("."))
}

pub(super) fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// `n` synthetic forward rows with ids `0..n`.
pub(super) fn port_rows(n: usize) -> Vec<PortRow> {
    (0..n)
        .map(|i| PortRow {
            id: i as u64,
            local: format!("127.0.0.1:{}", 3000 + i),
            target: format!("api:{}", 3000 + i),
            process: None,
            state: "active".into(),
            conns: 0,
            configured: false,
        })
        .collect()
}

/// A snapshot with one sandbox `s` holding `n` instances. Its Instances tree
/// is `[Sandbox(0), Instance(0)..Instance(n-1)]` when expanded, i.e. node
/// position of instance `i` is `i + 1`.
pub(super) fn snapshot_with(n: usize) -> Snapshot {
    use crate::tui::data::{ContainerStatus, InstanceRow, SandboxRow};
    let instances = (0..n)
        .map(|i| InstanceRow {
            name: format!("inst{i}"),
            sandbox: "s".into(),
            container: format!("devsandbox-inst{i}"),
            status: ContainerStatus::Missing,
            uptime_secs: 0,
            cpu: None,
            mem: None,
            folder: "/f".into(),
            worktree: false,
            services: Vec::new(),
            workspace: "/w".into(),
            remote_user: None,
            remote_env_len: 0,
            base_folder: "/f".into(),
            drift: false,
            instance_id: String::new(),
            dispatcher: None,
        })
        .collect();
    let sandboxes = vec![SandboxRow {
        name: "s".into(),
        source: "image x".into(),
        folder: None,
        services: Vec::new(),
        extends: Vec::new(),
        config_hash: "hash".into(),
        build_hash: String::new(),
        issues: Vec::new(),
        dispatcher: false,
    }];
    Snapshot {
        instances,
        sandboxes,
        services: Vec::new(),
        sandbox_count: 1,
        runtime_name: "docker",
        runtime_version: None,
        collected_at: std::time::Instant::now(),
        error: None,
    }
}

/// Install a small config modal directly, bypassing fs/config reads.
pub(super) fn open_modal(app: &mut App) {
    app.modal = Modal::Config(ConfigView {
        title: "s".into(),
        original: "[sandbox.s]\nimage = \"a\"\n".into(),
        // Ten lines so scroll has room to move.
        resolved: (0..10).map(|i| format!("line{i}\n")).collect(),
        showing: Side::Original,
        scroll: 0,
        hash: "deadbeef".into(),
        // Twenty inspect lines so the inspect pane scrolls independently.
        inspect: (0..20).map(|i| format!("insp{i}\n")).collect(),
        inspect_scroll: 0,
        inspect_container: "devsandbox-s".into(),
        focus: Pane::Config,
        split_pct: 50,
    });
}

pub(super) fn view(app: &App) -> &ConfigView {
    match &app.modal {
        Modal::Config(v) => v,
        _ => panic!("config modal not open"),
    }
}

/// A snapshot with no configured sandboxes and one instance whose sandbox is
/// not in config, so it groups under the orphan node:
/// tree = [Orphans, Instance(0)].
pub(super) fn orphan_snapshot() -> Snapshot {
    use crate::tui::data::{ContainerStatus, InstanceRow};
    let instances = vec![InstanceRow {
        name: "orphan0".into(),
        sandbox: "gone".into(),
        container: "devsandbox-orphan0".into(),
        status: ContainerStatus::Missing,
        uptime_secs: 0,
        cpu: None,
        mem: None,
        folder: "/f".into(),
        worktree: false,
        services: Vec::new(),
        workspace: "/w".into(),
        remote_user: None,
        remote_env_len: 0,
        base_folder: "/f".into(),
        drift: false,
        instance_id: String::new(),
        dispatcher: None,
    }];
    Snapshot {
        instances,
        sandboxes: Vec::new(),
        services: Vec::new(),
        sandbox_count: 0,
        runtime_name: "docker",
        runtime_version: None,
        collected_at: std::time::Instant::now(),
        error: None,
    }
}

pub(super) fn prompt(app: &App) -> &Prompt {
    app.prompt.as_ref().expect("prompt open")
}

/// `snapshot_with(n)` with every instance's container reporting `status`.
pub(super) fn snapshot_with_status(n: usize, status: ContainerStatus) -> Snapshot {
    let mut snap = snapshot_with(n);
    for row in &mut snap.instances {
        row.status = status.clone();
    }
    snap
}

pub(super) fn running() -> ContainerStatus {
    ContainerStatus::Running("Up".into())
}

/// Seed the proc cache + expansion for an instance so proc rows are visible
/// without a fetch.
pub(super) fn expand_with_rows(app: &mut App, instance: &str, pids: &[&str]) {
    use crate::tui::procs::{ProcRow, ProcState};
    let rows = pids
        .iter()
        .map(|p| ProcRow { pid: p.to_string(), depth: 0, args: "x".into() })
        .collect();
    app.expanded_procs.insert(instance.to_string());
    app.procs
        .insert(instance.to_string(), ProcState::Rows { rows, signalable: true });
}

/// Select the single process row of `inst0`, returning the app ready to act.
pub(super) fn app_on_proc_row(pids: &[&str]) -> App {
    let mut app = new_app();
    app.set_snapshot(snapshot_with_status(1, running()));
    expand_with_rows(&mut app, "inst0", pids);
    app.on_key(key(KeyCode::Down)); // inst0
    app.on_key(key(KeyCode::Down)); // proc row
    assert_eq!(app.selected_node(), Some(Node::Proc { instance: 0, row: 0 }));
    app
}

pub(super) fn mouse(kind: MouseEventKind, column: u16) -> MouseEvent {
    MouseEvent { kind, column, row: 0, modifiers: KeyModifiers::NONE }
}

pub(super) fn mouse_at(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE }
}

pub(super) fn term_sess(title: &str, container: &str) -> TermSession {
    TermSession::test_session(title, container, 24, 80)
}

// A frame large enough that the terminal panel is laid out; the panel Rect
// is derived from the same helper the real code uses so tests can't drift.
pub(super) const FRAME: Rect = Rect { x: 0, y: 0, width: 100, height: 40 };

// The config modal is full-screen, so its divider math only reads the width;
// a 100-wide area keeps the pre-Rect tests' column→percent mapping intact.
pub(super) const MODAL_AREA: Rect = Rect { x: 0, y: 0, width: 100, height: 40 };

pub(super) fn toks(line: &str) -> Vec<String> {
    line.split_whitespace().map(str::to_string).collect()
}

pub(super) fn cfg() -> Config {
    Config::parse(
        "[sandbox.web]\nfolder = \"code\"\nworktree-branch = \"wt/${instance}\"\n\n[sandbox.api]\nfolder = \"code\"\n",
    )
    .unwrap()
}

/// A snapshot whose single service `svc` has `containers`.
pub(super) fn service_snapshot(containers: Vec<(String, ContainerStatus)>) -> Snapshot {
    Snapshot {
        instances: Vec::new(),
        sandboxes: Vec::new(),
        services: vec![ServiceRow {
            name: "svc".into(),
            scope: "isolated",
            source: "image x".into(),
            ports: Vec::new(),
            containers,
            used_by: Vec::new(),
            env_len: 0,
            command: None,
            config_hash: "hash".into(),
            drift: false,
        }],
        sandbox_count: 0,
        runtime_name: "docker",
        runtime_version: None,
        collected_at: std::time::Instant::now(),
        error: None,
    }
}

/// Open a test terminal directly (no PTY) and focus it, bypassing the
/// I/O-bound `open_terminal`.
pub(super) fn open_test_term(app: &mut App, title: &str, container: &str) {
    app.terms
        .open(TermSession::test_session(title, container, 24, 80));
    app.focus = Focus::Terminal;
}
