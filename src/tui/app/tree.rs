//! Instances-tree selection and navigation: clamping, collapse/expand, and
//! parent jumps.

use crate::tui::data::{Node, ORPHANS_NAME};

use super::{App, Tab};

impl App {
    pub(super) fn select_up(&mut self) {
        let slot = self.tab.index();
        self.selected[slot] = self.selected[slot].saturating_sub(1);
        // Moving the cursor may land on a new instance; fetch its procs now so
        // the Detail agent count appears without waiting for the next proc tick.
        self.needs_proc_fetch = true;
    }

    pub(super) fn select_down(&mut self) {
        let rows = self.row_count();
        if rows == 0 {
            return;
        }
        let slot = self.tab.index();
        self.selected[slot] = (self.selected[slot] + 1).min(rows - 1);
        self.needs_proc_fetch = true;
    }

    /// The collapse-set key for a collapsible node (sandbox name / orphan group),
    /// or `None` for leaf nodes (instances, empty markers).
    fn collapse_key(&self, node: Node) -> Option<String> {
        let snapshot = self.snapshot.as_ref()?;
        match node {
            Node::Sandbox(i) => snapshot.sandboxes.get(i).map(|s| s.name.clone()),
            Node::Orphans => Some(ORPHANS_NAME.to_string()),
            Node::Instance(_) | Node::Empty(_) | Node::Proc { .. } => None,
        }
    }

    /// `→`: expand the node under the cursor. On a sandbox / orphan group this
    /// unfolds its children; on an instance it expands the process layer (and
    /// signals the event loop to fetch now). No-op on other leaves.
    pub(super) fn tree_expand(&mut self) {
        match self.selected_node() {
            Some(Node::Instance(_)) => self.expand_procs(),
            Some(node) => {
                if let Some(key) = self.collapse_key(node) {
                    self.collapsed.remove(&key);
                    self.clamp_selection();
                }
            }
            None => {}
        }
    }

    /// `space`: toggle the node under the cursor. Collapsible groups fold/unfold;
    /// an instance toggles its process layer.
    pub(super) fn tree_toggle(&mut self) {
        match self.selected_node() {
            Some(Node::Instance(_)) => {
                if let Some(name) = self.selected_instance_name() {
                    if !self.expanded_procs.remove(&name) {
                        self.expanded_procs.insert(name);
                        self.needs_proc_fetch = true;
                    }
                    self.clamp_selection();
                }
            }
            Some(node) => {
                if let Some(key) = self.collapse_key(node) {
                    if !self.collapsed.remove(&key) {
                        self.collapsed.insert(key);
                    }
                    self.clamp_selection();
                }
            }
            None => {}
        }
    }

    /// Mark the selected instance's process layer expanded and request a fetch.
    fn expand_procs(&mut self) {
        if let Some(name) = self.selected_instance_name() {
            if self.expanded_procs.insert(name) {
                self.needs_proc_fetch = true;
            }
            self.clamp_selection();
        }
    }

    /// `←`: fold one level. On a collapsible node collapse it. On a process row
    /// jump to its instance. On an instance with procs expanded collapse the
    /// procs (staying on the instance); otherwise jump to the parent sandbox.
    pub(super) fn tree_collapse(&mut self) {
        let Some(node) = self.selected_node() else {
            return;
        };
        match node {
            Node::Sandbox(_) | Node::Orphans => {
                if let Some(key) = self.collapse_key(node) {
                    self.collapsed.insert(key);
                    self.clamp_selection();
                }
            }
            Node::Proc { instance, .. } => {
                if let Some(pos) = self.instance_node_position(instance) {
                    self.selected[Tab::Instances.index()] = pos;
                }
            }
            Node::Instance(_) => {
                // First press collapses an expanded process layer (staying put);
                // otherwise fall through to the parent-sandbox jump.
                if let Some(name) = self.selected_instance_name() {
                    if self.expanded_procs.remove(&name) {
                        self.clamp_selection();
                        return;
                    }
                }
                if let Some(parent) = self.parent_index(self.selected[Tab::Instances.index()]) {
                    self.selected[Tab::Instances.index()] = parent;
                }
            }
            Node::Empty(_) => {
                if let Some(parent) = self.parent_index(self.selected[Tab::Instances.index()]) {
                    self.selected[Tab::Instances.index()] = parent;
                }
            }
        }
    }

    /// Instance index under the cursor: the instance row itself or the parent of
    /// a selected process row. `None` on sandbox / empty / orphan-group nodes.
    pub(super) fn selected_instance_index(&self) -> Option<usize> {
        match self.selected_node()? {
            Node::Instance(i) => Some(i),
            Node::Proc { instance, .. } => Some(instance),
            _ => None,
        }
    }

    /// Name of the instance under the cursor, whether the selected node is the
    /// instance row itself or one of its process rows. `None` otherwise.
    pub(super) fn selected_instance_name(&self) -> Option<String> {
        let snapshot = self.snapshot.as_ref()?;
        let idx = self.selected_instance_index()?;
        snapshot.instances.get(idx).map(|r| r.name.clone())
    }

    /// Visible-node position of `Node::Instance(instance)`, for jumping a process
    /// row's selection back onto its instance row.
    fn instance_node_position(&self, instance: usize) -> Option<usize> {
        self.visible_nodes()
            .iter()
            .position(|n| matches!(n, Node::Instance(i) if *i == instance))
    }

    /// Index of the enclosing group node (sandbox / orphan header) for the child
    /// at visible-node position `pos`: the nearest preceding `Sandbox`/`Orphans`.
    fn parent_index(&self, pos: usize) -> Option<usize> {
        let nodes = self.visible_nodes();
        nodes[..pos.min(nodes.len())]
            .iter()
            .rposition(|n| matches!(n, Node::Sandbox(_) | Node::Orphans))
    }
}

#[cfg(test)]
mod tests {
    use crate::tui::app::test_support::*;
    use crate::tui::app::*;

    #[test]
    fn selection_clamps() {
        let mut app = new_app();
        // Up at the top stays at 0.
        app.on_key(key(KeyCode::Up));
        assert_eq!(app.selected(), 0);

        // With zero placeholder rows, down never moves past 0.
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Char('j')));
        assert_eq!(app.selected(), 0);
    }

    #[test]
    fn selection_clamps_when_snapshot_shrinks() {
        let mut app = new_app();
        // Tree: [Sandbox(0), inst0, inst1, inst2] → 4 nodes.
        app.set_snapshot(snapshot_with(3));
        // Move down to the last row.
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.selected(), 3);

        // Shrinking to one instance → [Sandbox(0), inst0] re-clamps to last row.
        app.set_snapshot(snapshot_with(1));
        assert_eq!(app.selected(), 1);

        // Empty sandbox → [Sandbox(0), Empty(0)] still has 2 rows; clamp to 1.
        app.set_snapshot(snapshot_with(0));
        assert_eq!(app.selected(), 1);
    }

    #[test]
    fn left_on_instance_jumps_to_parent_sandbox() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(2)); // [Sandbox(0), inst0, inst1]
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Down)); // on inst1
        assert_eq!(app.selected(), 2);
        assert_eq!(app.selected_node(), Some(Node::Instance(1)));

        app.on_key(key(KeyCode::Left)); // jumps to the sandbox node
        assert_eq!(app.selected(), 0);
        assert_eq!(app.selected_node(), Some(Node::Sandbox(0)));
    }

    #[test]
    fn collapse_hides_children_and_reclamps_selection() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(2)); // [Sandbox(0), inst0, inst1]
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Down)); // on inst1 (pos 2)
        assert_eq!(app.selected(), 2);

        // Collapse the sandbox from the child: jumps to the parent first.
        app.on_key(key(KeyCode::Left));
        assert_eq!(app.selected(), 0);
        // Now on the sandbox node: Left collapses it, hiding both instances.
        app.on_key(key(KeyCode::Left));
        assert_eq!(app.visible_nodes(), vec![Node::Sandbox(0)]);
        assert_eq!(app.selected(), 0);

        // Space toggles it back open.
        app.on_key(key(KeyCode::Char(' ')));
        assert_eq!(
            app.visible_nodes(),
            vec![Node::Sandbox(0), Node::Instance(0), Node::Instance(1)],
        );
    }

    #[test]
    fn collapse_survives_snapshot_refresh_and_clamps() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(3)); // 4 nodes
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Down)); // last instance, pos 3
        assert_eq!(app.selected(), 3);
        // Collapse via parent jump then Left.
        app.on_key(key(KeyCode::Left)); // to sandbox, pos 0
        app.on_key(key(KeyCode::Left)); // collapse
        assert_eq!(app.visible_nodes().len(), 1);

        // A refresh keeps the collapse and re-clamps (still 1 node).
        app.set_snapshot(snapshot_with(3));
        assert_eq!(app.visible_nodes().len(), 1);
        assert_eq!(app.selected(), 0);
    }

    #[test]
    fn right_on_instance_expands_procs_and_signals_fetch() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1)); // [Sandbox(0), inst0]
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Right));
        assert!(app.expanded_procs.contains("inst0"));
        assert!(app.take_needs_proc_fetch());
        // Consumed once.
        assert!(!app.take_needs_proc_fetch());
    }

    #[test]
    fn space_toggles_procs_on_instance() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1));
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Char(' ')));
        assert!(app.expanded_procs.contains("inst0"));
        app.on_key(key(KeyCode::Char(' ')));
        assert!(!app.expanded_procs.contains("inst0"));
    }

    #[test]
    fn left_ladder_proc_to_instance_to_sandbox() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1)); // [Sandbox(0), inst0]
        expand_with_rows(&mut app, "inst0", &["10", "20"]);
        // Tree: [Sandbox(0), Instance(0), Proc r0, Proc r1].
        app.on_key(key(KeyCode::Down)); // inst0 (pos 1)
        app.on_key(key(KeyCode::Down)); // proc r0 (pos 2)
        app.on_key(key(KeyCode::Down)); // proc r1 (pos 3)
        assert_eq!(app.selected_node(), Some(Node::Proc { instance: 0, row: 1 }));

        // First Left: proc → its instance row.
        app.on_key(key(KeyCode::Left));
        assert_eq!(app.selected_node(), Some(Node::Instance(0)));
        // Procs still expanded (jump didn't collapse them).
        assert!(app.expanded_procs.contains("inst0"));

        // Second Left: instance with procs expanded → collapse procs, stay put.
        app.on_key(key(KeyCode::Left));
        assert_eq!(app.selected_node(), Some(Node::Instance(0)));
        assert!(!app.expanded_procs.contains("inst0"));

        // Third Left: instance → parent sandbox.
        app.on_key(key(KeyCode::Left));
        assert_eq!(app.selected_node(), Some(Node::Sandbox(0)));
    }
}
