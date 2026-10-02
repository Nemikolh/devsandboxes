//! The `:` command line as seen by the app: prefilled prompts, key handling,
//! and tab-completion candidates (grammar lives in `tui::spec`).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::commands::run::DEFAULT_WORKTREE_BRANCH;
use crate::config::Config;
use crate::tui::data::Node;
use crate::tui::prompt::{commands, Prompt, PromptAction};
use crate::tui::spec;

use super::{App, PortRequest, Tab};

impl App {
    /// Open the command prompt, loading persisted history. Clears any status.
    pub(super) fn open_prompt(&mut self) {
        self.status = None;
        self.prompt = Some(Prompt::new(crate::tui::prompt::load_history()));
    }

    /// `r` (Instances tab): open the prompt pre-filled `run <sandbox> ` for the
    /// sandbox under the cursor (a sandbox / empty node's sandbox, or the sandbox
    /// behind an instance). The orphan group, its children, and no selection fall
    /// back to a plain empty prompt (identical to `:`).
    fn open_run_prompt(&mut self) {
        self.status = None;
        let sandbox = self.snapshot.as_ref().and_then(|snapshot| {
            match self.selected_node() {
                Some(Node::Sandbox(i)) | Some(Node::Empty(i)) => {
                    snapshot.sandboxes.get(i).map(|s| s.name.clone())
                }
                Some(Node::Instance(i)) => snapshot.instances.get(i).map(|r| r.sandbox.clone()),
                Some(Node::Proc { instance, .. }) => {
                    snapshot.instances.get(instance).map(|r| r.sandbox.clone())
                }
                Some(Node::Orphans) | None => None,
            }
        });
        let history = crate::tui::prompt::load_history();
        self.prompt = Some(match sandbox {
            Some(name) => Prompt::with_input(history, format!("run {name} ")),
            None => Prompt::new(history),
        });
    }

    /// `r` (Instances tab): rename the instance under the cursor (or its process
    /// row's parent) via the command prompt pre-filled `rename <instance> `.
    /// Sandbox / empty / orphan-group selections have no instance, so they fall
    /// back to [`Self::open_run_prompt`] — `r` keeps its run behavior there.
    pub(super) fn open_rename_or_run_prompt(&mut self) {
        let Some(name) = self.selected_instance_name() else {
            self.open_run_prompt();
            return;
        };
        self.status = None;
        let history = crate::tui::prompt::load_history();
        self.prompt = Some(Prompt::with_input(history, format!("rename {name} ")));
    }

    /// `p` (Instances tab): open the prompt prefilled `port <instance> ` for the
    /// instance under the cursor (or a process row's parent), cursor at the end.
    /// Sandbox / empty / orphan-group selections have no instance, so `p` is a
    /// no-op there (nothing to forward from).
    pub(super) fn open_port_prompt_instance(&mut self) {
        let Some(name) = self.selected_instance_name() else {
            return;
        };
        self.open_port_prompt_for(&name);
    }

    /// The prompt prefilled `port <name> `, whatever is selected (the Inbox
    /// thread pane's `p` targets the thread's child).
    pub(super) fn open_port_prompt_for(&mut self, name: &str) {
        self.status = None;
        let history = crate::tui::prompt::load_history();
        self.prompt = Some(Prompt::with_input(history, format!("port {name} ")));
    }

    /// `p` (Services tab): open the prompt prefilled `port <instance> --service
    /// <svc> ` for the selected service, using its first `used_by` instance as the
    /// instance slot (a global service reached via a named instance works because
    /// the resolver falls back to another referencing instance). With no user, the
    /// instance slot is left blank for the user to fill.
    pub(super) fn open_port_prompt_service(&mut self) {
        let Some(row) = self
            .snapshot
            .as_ref()
            .and_then(|s| s.services.get(self.selected()))
        else {
            return;
        };
        let instance = row.used_by.first().map(String::as_str).unwrap_or("");
        let svc = row.name.clone();
        self.status = None;
        let history = crate::tui::prompt::load_history();
        self.prompt =
            Some(Prompt::with_input(history, format!("port {instance} --service {svc} ")));
    }

    /// Hand a forward to the event loop and show the Ports tab, where it
    /// appears. Shared by the `port` prompt and the Inbox `forward` host
    /// action, so both start forwards the same way. The tab is set directly,
    /// not via `set_tab`: a focused Inbox thread stays focused for the way back.
    pub(super) fn request_port(&mut self, req: PortRequest) {
        self.status = Some(format!("forwarding {} …", req.spec));
        self.pending_port = Some(req);
        self.tab = Tab::Ports;
    }

    /// Key handling while the prompt is open. `esc` cancels, `enter` parses
    /// (a parse error stays inline and keeps the prompt open), `tab` completes,
    /// everything else edits the line. Assumes `self.prompt` is `Some`.
    pub(super) fn on_key_prompt(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Candidate lists are read here (config/snapshot) so the prompt stays
        // data-agnostic; computed only on tab.
        match key.code {
            KeyCode::Esc => {
                self.prompt = None;
            }
            KeyCode::Char('c') if ctrl => {
                self.prompt = None;
            }
            KeyCode::Enter => {
                let Some(prompt) = &mut self.prompt else {
                    return;
                };
                if let Some(action) = prompt.parse() {
                    let line = prompt.input().to_string();
                    crate::tui::prompt::append_history(&line);
                    // `port` is not a suspending action: it goes to the forwarder
                    // worker (step 10) via `pending_port`, and the TUI stays up on
                    // the Ports tab. Handled before the generic `pending_action`
                    // path so the loop never suspends the screen for it.
                    if let PromptAction::Port { instance, service, address, spec } = action {
                        self.request_port(PortRequest { instance, service, address, spec });
                        self.prompt = None;
                        return;
                    }
                    // `rebuild`/`recreate` parses tab-agnostically as an instance
                    // action; on the Services tab it targets the named service
                    // instead. Rewrite here where the tab is known, keeping the
                    // parser pure.
                    let action = match action {
                        // `force` is dropped: service rebuild always recreates.
                        PromptAction::Rebuild { instance, .. } if self.tab == Tab::Services => {
                            PromptAction::ServiceRebuild { name: instance }
                        }
                        other => other,
                    };
                    self.pending_action = Some(action);
                    self.prompt = None;
                }
            }
            KeyCode::Tab => {
                let instances = self.instance_names();
                let services = self.service_names();
                let tab = self.tab;
                // One config load per tab: sandbox names for the positional
                // argument plus `worktree-branch` lookups for `--branch` values.
                let config = Config::load(&self.dir).ok();
                let sandboxes: Vec<String> = config
                    .as_ref()
                    .map(|c| c.sandboxes.keys().cloned().collect())
                    .unwrap_or_default();
                if let Some(prompt) = &mut self.prompt {
                    prompt.complete(|idx, tokens| {
                        Self::candidates_for(
                            tab,
                            idx,
                            tokens,
                            &sandboxes,
                            &instances,
                            &services,
                            config.as_ref(),
                        )
                    });
                }
            }
            KeyCode::Up => {
                if let Some(prompt) = &mut self.prompt {
                    prompt.history_prev();
                }
            }
            KeyCode::Down => {
                if let Some(prompt) = &mut self.prompt {
                    prompt.history_next();
                }
            }
            KeyCode::Left => self.with_prompt(Prompt::left),
            KeyCode::Right => self.with_prompt(Prompt::right),
            KeyCode::Home => self.with_prompt(Prompt::home),
            KeyCode::End => self.with_prompt(Prompt::end),
            KeyCode::Backspace => self.with_prompt(Prompt::backspace),
            KeyCode::Delete => self.with_prompt(Prompt::delete),
            KeyCode::Char('u') if ctrl => self.with_prompt(Prompt::clear),
            KeyCode::Char('w') if ctrl => self.with_prompt(Prompt::delete_word),
            KeyCode::Char(c) if !ctrl => {
                if let Some(prompt) = &mut self.prompt {
                    prompt.insert_char(c);
                }
            }
            _ => {}
        }
    }

    fn with_prompt(&mut self, f: impl FnOnce(&mut Prompt)) {
        if let Some(prompt) = &mut self.prompt {
            f(prompt);
        }
    }

    /// Candidate list for the token at `idx` of the whitespace-split `tokens`
    /// (the token being completed is absent when empty). Command names for the
    /// first token; everything past it walks the command's [`spec::CommandSpec`]:
    /// the value of the preceding flag, the spec's unused flags on a `-` stem,
    /// the next unconsumed positional, and — once the positionals are filled —
    /// the unused flags again. Trailing argv (`exec`) gets no candidates.
    ///
    /// [`spec::ArgValue::Branch`] offers the branch the run would use anyway
    /// (the sandbox's `worktree-branch`, else the default pattern) so the user
    /// edits a base instead of typing from scratch; `${…}` variables stay
    /// unsubstituted, exactly as `run` would receive them.
    fn candidates_for(
        tab: Tab,
        idx: usize,
        tokens: &[String],
        sandboxes: &[String],
        instances: &[String],
        services: &[String],
        config: Option<&Config>,
    ) -> Vec<String> {
        if idx == 0 {
            return commands().map(str::to_string).collect();
        }
        let Some(cmd_spec) = tokens.first().and_then(|t| spec::find(t)) else {
            return Vec::new();
        };
        let value_candidates = |value: spec::ArgValue| -> Vec<String> {
            match value {
                spec::ArgValue::Instance => instances.to_vec(),
                spec::ArgValue::InstanceOrService if tab == Tab::Services => services.to_vec(),
                spec::ArgValue::InstanceOrService => instances.to_vec(),
                spec::ArgValue::Service => services.to_vec(),
                spec::ArgValue::Sandbox => sandboxes.to_vec(),
                spec::ArgValue::Branch => {
                    let sandbox = cmd_spec
                        .positionals
                        .iter()
                        .zip(Self::consumed_positionals(cmd_spec, tokens, idx))
                        .find(|(v, _)| **v == spec::ArgValue::Sandbox)
                        .map(|(_, t)| t);
                    let branch = sandbox
                        .and_then(|s| {
                            config?.resolve_sandbox(s).ok()?.properties.worktree_branch
                        })
                        .unwrap_or_else(|| DEFAULT_WORKTREE_BRANCH.to_string());
                    vec![branch]
                }
                spec::ArgValue::Free => Vec::new(),
            }
        };
        // Value position: the previous token is a value-taking flag.
        if let Some(value) = tokens
            .get(idx - 1)
            .and_then(|p| cmd_spec.flags.iter().find(|f| f.name == p.as_str()))
            .and_then(|f| f.value)
        {
            return value_candidates(value);
        }
        // Flags in spec order (the prompt sorts later); one already on the
        // line — anywhere but at `idx` itself — is not re-offered.
        let unused_flags = || -> Vec<String> {
            cmd_spec
                .flags
                .iter()
                .filter(|f| {
                    !tokens.iter().enumerate().any(|(i, t)| i != idx && t == f.name)
                })
                .map(|f| f.name.to_string())
                .collect()
        };
        let stem = tokens.get(idx).map(String::as_str).unwrap_or("");
        if stem.starts_with('-') {
            return unused_flags();
        }
        let consumed = Self::consumed_positionals(cmd_spec, tokens, idx);
        if let Some(&value) = cmd_spec.positionals.get(consumed.len()) {
            return value_candidates(value);
        }
        if cmd_spec.trailing {
            return Vec::new(); // verbatim argv: no candidates
        }
        unused_flags()
    }

    /// Positional tokens already on the line: tokens after the command that
    /// are neither a known flag, a value-taking flag's value, nor the token
    /// currently being completed (at index `skip`). For `trailing` specs the
    /// argv after the positionals is not scanned.
    fn consumed_positionals<'a>(
        cmd_spec: &spec::CommandSpec,
        tokens: &'a [String],
        skip: usize,
    ) -> Vec<&'a str> {
        let mut out = Vec::new();
        let mut i = 1;
        while i < tokens.len() {
            if cmd_spec.trailing && out.len() == cmd_spec.positionals.len() {
                break;
            }
            let tok = tokens[i].as_str();
            if let Some(flag) = cmd_spec.flags.iter().find(|f| f.name == tok) {
                i += if flag.value.is_some() { 2 } else { 1 };
                continue;
            }
            if i != skip {
                out.push(tok);
            }
            i += 1;
        }
        out
    }

    /// Instance names from the latest snapshot (empty until one lands).
    fn instance_names(&self) -> Vec<String> {
        self.snapshot
            .as_ref()
            .map(|s| s.instances.iter().map(|r| r.name.clone()).collect())
            .unwrap_or_default()
    }

    fn service_names(&self) -> Vec<String> {
        self.snapshot
            .as_ref()
            .map(|s| s.services.iter().map(|r| r.name.clone()).collect())
            .unwrap_or_default()
    }

    /// Take the pending action for the event loop to execute, if any.
    pub fn take_pending_action(&mut self) -> Option<PromptAction> {
        self.pending_action.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::test_support::*;
    use crate::tui::app::*;

    #[test]
    fn port_prompt_sets_pending_port_not_action() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1));
        app.on_key(key(KeyCode::Char(':')));
        for c in "port api 3000".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));
        // Routed to pending_port (worker), never pending_action (suspend path).
        assert_eq!(app.take_pending_action(), None);
        assert_eq!(
            app.take_pending_port(),
            Some(PortRequest {
                instance: "api".into(),
                service: None,
                address: None,
                spec: "3000".into(),
            })
        );
        // Submitting a forward switches to the Ports tab with a status line.
        assert_eq!(app.tab, Tab::Ports);
        assert_eq!(app.status.as_deref(), Some("forwarding 3000 …"));
    }

    #[test]
    fn port_prompt_carries_flags() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1));
        app.on_key(key(KeyCode::Char(':')));
        for c in "port api --service pg --address 0.0.0.0 8080:5432".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));
        assert_eq!(
            app.take_pending_port(),
            Some(PortRequest {
                instance: "api".into(),
                service: Some("pg".into()),
                address: Some("0.0.0.0".into()),
                spec: "8080:5432".into(),
            })
        );
    }

    #[test]
    fn port_prompt_bad_spec_errors_without_request() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1));
        app.on_key(key(KeyCode::Char(':')));
        for c in "port api 0".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));
        // Parse error keeps the prompt open, sets no request, and stays put.
        assert!(app.prompt.is_some());
        assert!(prompt(&app).error.is_some());
        assert_eq!(app.take_pending_port(), None);
        assert_eq!(app.tab, Tab::Instances);
    }

    #[test]
    fn port_completion_service_and_instance() {
        let instances = vec!["api".to_string()];
        let services = vec!["pg".to_string(), "redis".to_string()];
        // First positional completes instances.
        assert_eq!(
            App::candidates_for(Tab::Instances, 1, &toks("port"), &[], &instances, &services, None),
            instances
        );
        // `--service` value completes service names, on any tab.
        assert_eq!(
            App::candidates_for(
                Tab::Instances,
                3,
                &toks("port api --service"),
                &[],
                &instances,
                &services,
                None,
            ),
            services
        );
    }

    #[test]
    fn p_on_instance_prefills_port_prompt() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1)); // [Sandbox(0), inst0]
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Char('p')));
        let p = prompt(&app);
        assert_eq!(p.input(), "port inst0 ");
        assert_eq!(p.cursor(), "port inst0 ".chars().count());
    }

    #[test]
    fn p_on_sandbox_is_noop() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1)); // cursor on Sandbox(0)
        app.on_key(key(KeyCode::Char('p')));
        assert!(app.prompt.is_none());
    }

    #[test]
    fn p_on_service_prefills_with_used_by_instance() {
        let mut app = new_app();
        let mut snap = service_snapshot(Vec::new());
        snap.services[0].used_by = vec!["api".into(), "web".into()];
        app.set_snapshot(snap);
        app.tab = Tab::Services;
        app.on_key(key(KeyCode::Char('p')));
        // First used_by instance fills the instance slot; service via --service.
        assert_eq!(prompt(&app).input(), "port api --service svc ");
    }

    #[test]
    fn p_on_service_without_used_by_leaves_instance_blank() {
        let mut app = new_app();
        app.set_snapshot(service_snapshot(Vec::new())); // used_by empty
        app.tab = Tab::Services;
        app.on_key(key(KeyCode::Char('p')));
        assert_eq!(prompt(&app).input(), "port  --service svc ");
    }

    #[test]
    fn r_on_sandbox_prefills_run() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1)); // [Sandbox(0), inst0], cursor on sandbox
        app.on_key(key(KeyCode::Char('r')));
        let p = prompt(&app);
        assert_eq!(p.input(), "run s ");
        // Cursor at the end (6 chars).
        assert_eq!(p.cursor(), "run s ".chars().count());
    }

    #[test]
    fn r_on_empty_marker_prefills_run() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(0)); // [Sandbox(0), Empty(0)]
        app.on_key(key(KeyCode::Down)); // onto Empty(0)
        assert_eq!(app.selected_node(), Some(Node::Empty(0)));
        app.on_key(key(KeyCode::Char('r')));
        assert_eq!(prompt(&app).input(), "run s ");
    }

    #[test]
    fn r_on_instance_prefills_rename() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(2)); // [Sandbox(0), inst0, inst1]
        app.on_key(key(KeyCode::Down)); // onto inst0
        assert_eq!(app.selected_node(), Some(Node::Instance(0)));
        app.on_key(key(KeyCode::Char('r')));
        assert_eq!(prompt(&app).input(), "rename inst0 ");
    }

    #[test]
    fn r_on_orphans_gives_empty_prompt() {
        let mut app = new_app();
        app.set_snapshot(orphan_snapshot()); // [Orphans, inst0], cursor on Orphans
        assert_eq!(app.selected_node(), Some(Node::Orphans));
        app.on_key(key(KeyCode::Char('r')));
        assert_eq!(prompt(&app).input(), "");
    }

    #[test]
    fn r_with_no_snapshot_gives_empty_prompt() {
        let mut app = new_app();
        app.on_key(key(KeyCode::Char('r')));
        assert_eq!(prompt(&app).input(), "");
    }

    #[test]
    fn prefilled_prompt_history_up_stashes_prefill() {
        // Seed one history entry, prefill, then Up shows history and Down restores
        // the prefill as the stashed live line.
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1));
        app.prompt = Some(Prompt::with_input(vec!["run other".into()], "run s ".into()));
        app.on_key(key(KeyCode::Up));
        assert_eq!(prompt(&app).input(), "run other");
        app.on_key(key(KeyCode::Down));
        assert_eq!(prompt(&app).input(), "run s ");
    }

    #[test]
    fn prompt_rebuild_on_services_tab_queues_service_rebuild() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1));
        app.tab = Tab::Services;
        app.on_key(key(KeyCode::Char(':')));
        for c in "rebuild db".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));
        // On the Services tab, `rebuild <name>` targets the service, not an
        // instance.
        assert_eq!(
            app.take_pending_action(),
            Some(PromptAction::ServiceRebuild { name: "db".into() }),
        );
    }

    #[test]
    fn prompt_rebuild_on_instances_tab_stays_instance_rebuild() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1));
        app.tab = Tab::Instances;
        app.on_key(key(KeyCode::Char(':')));
        for c in "rebuild inst0".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));
        assert_eq!(
            app.take_pending_action(),
            Some(PromptAction::Rebuild { instance: "inst0".into(), force: false }),
        );
    }

    #[test]
    fn r_and_o_ignored_on_services_tab() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1));
        app.tab = Tab::Services;
        app.on_key(key(KeyCode::Char('r')));
        assert!(app.prompt.is_none());
        app.on_key(key(KeyCode::Char('o')));
        assert_eq!(app.take_pending_action(), None);
    }

    #[test]
    fn run_candidates_sandbox_position() {
        let sandboxes = vec!["api".to_string(), "web".to_string()];
        assert_eq!(
            App::candidates_for(Tab::Instances, 1, &toks("run"), &sandboxes, &[], &[], None),
            sandboxes
        );
        // Sandbox not on the line yet (only a flag pair) → still sandbox names.
        assert_eq!(
            App::candidates_for(Tab::Instances, 3, &toks("run --name x"), &sandboxes, &[], &[], None),
            sandboxes
        );
    }

    #[test]
    fn run_candidates_flags_after_sandbox() {
        let sandboxes = vec!["web".to_string()];
        // Empty token after the sandbox → the flags.
        assert_eq!(
            App::candidates_for(Tab::Instances, 2, &toks("run web"), &sandboxes, &[], &[], None),
            vec!["--name".to_string(), "--branch".to_string(), "--base".to_string()]
        );
        // A `--` stem too (the prompt then filters by the stem).
        assert_eq!(
            App::candidates_for(Tab::Instances, 2, &toks("run web --"), &sandboxes, &[], &[], None),
            vec!["--name".to_string(), "--branch".to_string(), "--base".to_string()]
        );
        // A flag already used is not offered again.
        assert_eq!(
            App::candidates_for(Tab::Instances, 4, &toks("run web --name x"), &sandboxes, &[], &[], None),
            vec!["--branch".to_string(), "--base".to_string()]
        );
    }

    #[test]
    fn run_candidates_branch_value_offers_default() {
        let config = cfg();
        let sandboxes: Vec<String> = config.sandboxes.keys().cloned().collect();
        // Sandbox with `worktree-branch` → its pattern as the editable base.
        assert_eq!(
            App::candidates_for(Tab::Instances, 3, &toks("run web --branch"), &sandboxes, &[], &[], Some(&config)),
            vec!["wt/${instance}".to_string()]
        );
        // Without one → the built-in default pattern.
        assert_eq!(
            App::candidates_for(Tab::Instances, 3, &toks("run api --branch"), &sandboxes, &[], &[], Some(&config)),
            vec![DEFAULT_WORKTREE_BRANCH.to_string()]
        );
        // `--name` values are free-form: no candidates.
        assert!(App::candidates_for(Tab::Instances, 3, &toks("run web --name"), &sandboxes, &[], &[], Some(&config))
            .is_empty());
    }

    #[test]
    fn rebuild_completes_instance_names() {
        let instances = vec!["a".to_string(), "b".to_string()];
        assert_eq!(
            App::candidates_for(Tab::Instances, 1, &toks("rebuild"), &[], &instances, &[], None),
            instances
        );
        assert_eq!(
            App::candidates_for(Tab::Instances, 1, &toks("recreate"), &[], &instances, &[], None),
            instances
        );
    }

    #[test]
    fn rebuild_completes_service_names_on_services_tab() {
        let instances = vec!["a".to_string(), "b".to_string()];
        let services = vec!["cache".to_string(), "db".to_string()];
        // Services tab: `rebuild`/`recreate` offer service names, not instances.
        assert_eq!(
            App::candidates_for(
                Tab::Services,
                1,
                &toks("rebuild"),
                &[],
                &instances,
                &services,
                None,
            ),
            services
        );
        assert_eq!(
            App::candidates_for(
                Tab::Services,
                1,
                &toks("recreate"),
                &[],
                &instances,
                &services,
                None,
            ),
            services
        );
    }

    #[test]
    fn rebuild_offers_force_flag() {
        // `--` stem before or after the positional → the spec's flags.
        assert_eq!(
            App::candidates_for(Tab::Instances, 1, &toks("rebuild --"), &[], &[], &[], None),
            vec!["--force".to_string()]
        );
        assert_eq!(
            App::candidates_for(Tab::Instances, 2, &toks("rebuild box --"), &[], &[], &[], None),
            vec!["--force".to_string()]
        );
        // Positional filled, empty stem → unused flags too.
        assert_eq!(
            App::candidates_for(Tab::Instances, 2, &toks("rebuild box"), &[], &[], &[], None),
            vec!["--force".to_string()]
        );
    }

    #[test]
    fn rebuild_force_not_reoffered() {
        assert!(App::candidates_for(
            Tab::Instances,
            2,
            &toks("rebuild --force --"),
            &[],
            &[],
            &[],
            None,
        )
        .is_empty());
    }

    #[test]
    fn rebuild_completes_instances_after_force() {
        let instances = vec!["a".to_string(), "b".to_string()];
        let services = vec!["cache".to_string()];
        // Boolean flag before the positional: still the positional's names.
        assert_eq!(
            App::candidates_for(
                Tab::Instances,
                2,
                &toks("rebuild --force"),
                &[],
                &instances,
                &services,
                None,
            ),
            instances
        );
        // Services tab: same walk, service names.
        assert_eq!(
            App::candidates_for(
                Tab::Services,
                2,
                &toks("rebuild --force"),
                &[],
                &instances,
                &services,
                None,
            ),
            services
        );
    }

    #[test]
    fn exec_argv_offers_nothing() {
        let instances = vec!["box".to_string()];
        // Positional consumed → the rest of the line is verbatim argv.
        assert!(App::candidates_for(Tab::Instances, 2, &toks("exec box"), &[], &instances, &[], None)
            .is_empty());
        assert!(
            App::candidates_for(Tab::Instances, 3, &toks("exec box ls"), &[], &instances, &[], None)
                .is_empty()
        );
    }

    // --- integrated-terminal state (step 2) -----------------------------------
}
