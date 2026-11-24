//! A tree whose collapsed nodes show a short preview of their children,
//! opened in place to read the rest.

use super::Key;
use super::style::{Role, Style};

/// Children a collapsed node previews before `… N more`.
pub const PREVIEW: usize = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub label: String,
    pub role: Role,
    pub hint: Option<String>,
    pub children: Vec<Node>,
    pub open: bool,
}

impl Node {
    pub fn new(label: impl Into<String>) -> Node {
        Node { label: label.into(), role: Role::Plain, hint: None, children: Vec::new(), open: false }
    }

    pub fn role(mut self, role: Role) -> Node {
        self.role = role;
        self
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Node {
        self.hint = Some(hint.into());
        self
    }

    pub fn child(mut self, child: Node) -> Node {
        self.children.push(child);
        self
    }

    pub fn children(mut self, children: impl IntoIterator<Item = Node>) -> Node {
        self.children.extend(children);
        self
    }

    pub fn opened(mut self) -> Node {
        self.open = true;
        self
    }

    fn is_open(&self) -> bool {
        self.open && !self.children.is_empty()
    }
}

/// The cursor indexes the navigable nodes: roots and the children of open
/// nodes, in display order. A collapsed node's preview lines aren't
/// navigable. Opening or closing only changes nodes after the cursor's,
/// so the index keeps pointing at the same node.
#[derive(Clone, Debug)]
pub struct Fold {
    roots: Vec<Node>,
    cursor: usize,
}

impl Fold {
    pub fn new(roots: Vec<Node>) -> Fold {
        Fold { roots, cursor: 0 }
    }

    pub fn roots(&self) -> &[Node] {
        &self.roots
    }

    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    fn visible(&self) -> Vec<Vec<usize>> {
        fn walk(nodes: &[Node], path: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
            for (i, node) in nodes.iter().enumerate() {
                path.push(i);
                out.push(path.clone());
                if node.is_open() {
                    walk(&node.children, path, out);
                }
                path.pop();
            }
        }
        let mut out = Vec::new();
        walk(&self.roots, &mut Vec::new(), &mut out);
        out
    }

    fn node_mut(&mut self, path: &[usize]) -> &mut Node {
        let mut node = &mut self.roots[path[0]];
        for &i in &path[1..] {
            node = &mut node.children[i];
        }
        node
    }

    pub fn at_last(&self) -> bool {
        self.cursor + 1 >= self.visible().len()
    }

    pub fn focus_last(&mut self) {
        self.cursor = self.visible().len().saturating_sub(1);
    }

    pub fn key(&mut self, key: Key) {
        let visible = self.visible();
        let Some(path) = visible.get(self.cursor).cloned() else { return };
        match key {
            Key::Up | Key::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            Key::Down | Key::Char('j') => {
                if self.cursor + 1 < visible.len() {
                    self.cursor += 1;
                }
            }
            Key::Right => {
                let node = self.node_mut(&path);
                node.open = !node.children.is_empty();
            }
            Key::Space | Key::Enter => {
                let node = self.node_mut(&path);
                node.open = !node.is_open() && !node.children.is_empty();
            }
            Key::Left => {
                let node = self.node_mut(&path);
                if node.is_open() {
                    node.open = false;
                } else if let Some(parent) = visible.iter().position(|p| p[..] == path[..path.len() - 1]) {
                    self.cursor = parent;
                }
            }
            _ => {}
        }
    }

    /// The widget's lines, each behind a two-column cursor gutter, and the
    /// line the cursor is on (`0` when `cursor` is off).
    pub fn lines(&self, style: Style, cursor: bool) -> (Vec<String>, usize) {
        let mut out = Render { style, cursor: cursor.then_some(self.cursor), gutter: true, seen: 0, at: 0, lines: Vec::new() };
        out.walk(&self.roots, 0);
        (out.lines, out.at)
    }

    /// The tree as it stands, no cursor or gutter: the plain fallback's.
    pub fn plain_lines(&self, style: Style) -> Vec<String> {
        let mut out = Render { style, cursor: None, gutter: false, seen: 0, at: 0, lines: Vec::new() };
        out.walk(&self.roots, 0);
        out.lines
    }

    /// One collapsed line per root: what scrollback keeps once answered.
    pub fn summary_lines(&self, style: Style) -> Vec<String> {
        self.roots.iter().map(|root| head(root, false, style)).collect()
    }
}

struct Render {
    style: Style,
    cursor: Option<usize>,
    gutter: bool,
    seen: usize,
    at: usize,
    lines: Vec<String>,
}

impl Render {
    fn walk(&mut self, nodes: &[Node], depth: usize) {
        let pad = if self.gutter { "  " } else { "" };
        for node in nodes {
            let here = self.cursor == Some(self.seen);
            self.seen += 1;
            if here {
                self.at = self.lines.len();
            }
            let mark = if here { format!("{} ", self.style.accent("❯")) } else { pad.to_string() };
            let indent = "  ".repeat(depth);
            self.lines.push(format!("{mark}{indent}{}", head(node, node.is_open(), self.style)));
            if node.children.is_empty() {
                continue;
            }
            if node.is_open() {
                self.walk(&node.children, depth + 1);
                continue;
            }
            let indent = "  ".repeat(depth + 1);
            for child in node.children.iter().take(PREVIEW) {
                let text = head(child, false, Style::plain());
                self.lines.push(format!("{pad}{indent}{}", self.style.dim(&text)));
            }
            if node.children.len() > PREVIEW {
                let more = format!("… {} more", node.children.len() - PREVIEW);
                self.lines.push(format!("{pad}{indent}{}", self.style.dim(&more)));
            }
        }
    }
}

/// `▸ label (hint)` for a parent (`▾` open), `label (hint)` for a leaf.
fn head(node: &Node, open: bool, style: Style) -> String {
    let glyph = match (node.children.is_empty(), open) {
        (true, _) => "",
        (false, true) => "▾ ",
        (false, false) => "▸ ",
    };
    let mut text = format!("{glyph}{}", style.paint(node.role, &node.label));
    if let Some(hint) = &node.hint {
        text.push(' ');
        text.push_str(&style.dim(&format!("({hint})")));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> Fold {
        Fold::new(vec![
            Node::new("credential gh").role(Role::Name),
            Node::new("scope repo").hint("3 routes").children(["GET /a", "GET /b", "POST /c"].map(Node::new)),
            Node::new("never").role(Role::Danger).child(Node::new("DELETE /repo")),
        ])
    }

    fn plain(fold: &Fold) -> Vec<String> {
        fold.lines(Style::plain(), true).0
    }

    #[test]
    fn collapsed_nodes_preview_their_first_children() {
        assert_eq!(
            plain(&tree()),
            [
                "❯ credential gh",
                "  ▸ scope repo (3 routes)",
                "    GET /a",
                "    GET /b",
                "    … 1 more",
                "  ▸ never",
                "    DELETE /repo",
            ]
        );
    }

    #[test]
    fn opening_shows_every_child_navigable() {
        let mut fold = tree();
        fold.key(Key::Down);
        fold.key(Key::Right);
        fold.key(Key::Down);
        fold.key(Key::Down);
        let (lines, at) = fold.lines(Style::plain(), true);
        assert_eq!(
            lines,
            ["  credential gh", "  ▾ scope repo (3 routes)", "    GET /a", "❯   GET /b", "    POST /c", "  ▸ never", "    DELETE /repo"]
        );
        assert_eq!(at, 3);
    }

    #[test]
    fn left_closes_or_moves_to_the_parent() {
        let mut fold = tree();
        fold.key(Key::Down);
        fold.key(Key::Enter);
        fold.key(Key::Down);
        fold.key(Key::Left);
        assert_eq!(fold.lines(Style::plain(), true).1, 1);
        fold.key(Key::Left);
        assert_eq!(plain(&fold)[2], "    GET /a");
        assert_eq!(plain(&fold)[4], "    … 1 more");
    }

    #[test]
    fn space_toggles_and_leaves_stay_leaves() {
        let mut fold = tree();
        fold.key(Key::Space);
        assert_eq!(plain(&fold)[0], "❯ credential gh");
        fold.key(Key::Down);
        fold.key(Key::Space);
        assert_eq!(plain(&fold)[1], "❯ ▾ scope repo (3 routes)");
        fold.key(Key::Space);
        assert_eq!(plain(&fold)[1], "❯ ▸ scope repo (3 routes)");
    }

    #[test]
    fn cursor_clamps_and_reports_the_ends() {
        let mut fold = tree();
        fold.key(Key::Up);
        assert_eq!(fold.lines(Style::plain(), true).1, 0);
        fold.focus_last();
        assert!(fold.at_last());
        fold.key(Key::Down);
        assert_eq!(fold.lines(Style::plain(), true).1, 5);
    }

    #[test]
    fn plain_and_summary_lines_have_no_gutter() {
        let mut fold = tree();
        fold.key(Key::Down);
        fold.key(Key::Right);
        assert_eq!(fold.plain_lines(Style::plain())[..3], ["credential gh", "▾ scope repo (3 routes)", "  GET /a"]);
        assert_eq!(fold.summary_lines(Style::plain()), ["credential gh", "▸ scope repo (3 routes)", "▸ never"]);
    }

    #[test]
    fn preview_lines_are_dimmed_and_roles_styled() {
        let lines = tree().lines(Style { on: true }, false).0;
        assert_eq!(lines[5], "  ▸ \x1b[1;31mnever\x1b[0m");
        assert_eq!(lines[6], "    \x1b[2mDELETE /repo\x1b[0m");
    }
}
