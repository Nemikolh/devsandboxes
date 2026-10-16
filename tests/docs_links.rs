//! Docs stay navigable after files move: every relative link in `docs/`
//! resolves (anchors included), nothing outside the archive links into
//! `docs/plans/archive/`, and every `docs/….md` path cited from code, the
//! READMEs, the skills and the site exists.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use pulldown_cmark::{Event, Parser, Tag, TagEnd};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn walk(dir: &Path, keep: &dyn Fn(&Path) -> bool, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if name == "node_modules" || name == "target" || name == ".git" {
            continue;
        }
        if path.is_dir() {
            walk(&path, keep, out);
        } else if keep(&path) {
            out.push(path);
        }
    }
}

/// `a/b/../c` → `a/c`, without touching the filesystem.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

/// GitHub's heading anchor: lowercase, drop punctuation, spaces → `-`,
/// repeats get `-1`, `-2`, ….
fn slugs(markdown: &str) -> HashSet<String> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut out = HashSet::new();
    let mut heading: Option<String> = None;
    for event in Parser::new(markdown) {
        match event {
            Event::Start(Tag::Heading { .. }) => {
                heading = Some(String::new());
            }
            Event::Text(t) | Event::Code(t) => {
                if let Some(h) = heading.as_mut() {
                    h.push_str(&t);
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                let text = heading.take().unwrap_or_default();
                let base: String = text
                    .to_lowercase()
                    .chars()
                    .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'))
                    .map(|c| if c == ' ' { '-' } else { c })
                    .collect();
                let n = seen.entry(base.clone()).or_insert(0);
                out.insert(if *n == 0 { base.clone() } else { format!("{base}-{n}") });
                *n += 1;
            }
            _ => {}
        }
    }
    out
}

fn links(markdown: &str) -> Vec<String> {
    Parser::new(markdown)
        .filter_map(|e| match e {
            Event::Start(Tag::Link { dest_url, .. }) => Some(dest_url.to_string()),
            _ => None,
        })
        .collect()
}

#[test]
fn docs_relative_links_resolve() {
    let docs = root().join("docs");
    let archive = docs.join("plans/archive");
    let mut files = Vec::new();
    walk(&docs, &|p| p.extension().is_some_and(|e| e == "md"), &mut files);

    let mut bad = Vec::new();
    for file in &files {
        // Archived plans are history: their links may point at docs long gone.
        if file.starts_with(&archive) {
            continue;
        }
        let text = fs::read_to_string(file).unwrap();
        let rel = file.strip_prefix(root()).unwrap().display().to_string();
        for link in links(&text) {
            if link.contains("://") || link.starts_with("mailto:") {
                continue;
            }
            let (path, anchor) = match link.split_once('#') {
                Some((p, a)) => (p, Some(a)),
                None => (link.as_str(), None),
            };
            let target = if path.is_empty() {
                file.clone()
            } else {
                normalize(&file.parent().unwrap().join(path))
            };
            if !target.exists() {
                bad.push(format!("{rel}: {link} (no such file)"));
                continue;
            }
            if target.starts_with(&archive) {
                bad.push(format!("{rel}: {link} (links into plans/archive/)"));
            }
            if let Some(anchor) = anchor
                && target.extension().is_some_and(|e| e == "md")
                && !slugs(&fs::read_to_string(&target).unwrap()).contains(anchor)
            {
                bad.push(format!("{rel}: {link} (no such heading)"));
            }
        }
    }
    assert!(bad.is_empty(), "broken docs links:\n{}", bad.join("\n"));
}

/// `docs/<path>.md` mentions in `text`: at the start of a path (not inside
/// e.g. `site/src/content/docs/x.mdx`), or after a GitHub `/blob/main/`.
fn cited_docs(text: &str) -> Vec<String> {
    let path_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '-' | '_');
    let mut out = Vec::new();
    for (i, _) in text.match_indices("docs/") {
        let before = &text[..i];
        let starts_path = before.chars().next_back().is_none_or(|c| !path_char(c));
        if !starts_path && !before.ends_with("/blob/main/") {
            continue;
        }
        let rest = &text[i..];
        let end = rest.find(|c: char| !path_char(c)).unwrap_or(rest.len());
        let candidate = rest[..end].trim_end_matches('.');
        if let Some(stem) = candidate.strip_suffix(".md") {
            out.push(format!("{stem}.md"));
        } else if let Some(at) = candidate.find(".md")
            && rest[at + 3..].starts_with(|c: char| !c.is_ascii_alphanumeric())
        {
            out.push(candidate[..at + 3].to_string());
        }
    }
    out
}

#[test]
fn docs_cited_from_code_exist() {
    let root = root();
    let sources = |p: &Path| {
        p.extension()
            .is_some_and(|e| matches!(e.to_str(), Some("rs" | "md" | "mdx" | "ts" | "js" | "astro" | "toml")))
    };
    let mut files = Vec::new();
    for dir in ["src", "devsbd", "tests", "test-utils", "skills", "npm/devsandboxes", "site/src"] {
        walk(&root.join(dir), &sources, &mut files);
    }
    for file in ["AGENTS.md", "README.md", "build.rs", "Cargo.toml"] {
        files.push(root.join(file));
    }

    let mut bad = Vec::new();
    for file in &files {
        // This file's own fixtures cite made-up paths.
        if file.ends_with("tests/docs_links.rs") {
            continue;
        }
        let Ok(text) = fs::read_to_string(file) else { continue };
        let rel = file.strip_prefix(&root).unwrap().display().to_string();
        for cited in cited_docs(&text) {
            if cited.starts_with("docs/plans/archive/") {
                bad.push(format!("{rel}: {cited} (cites an archived plan)"));
            } else if !root.join(&cited).exists() {
                bad.push(format!("{rel}: {cited} (no such file)"));
            }
        }
    }
    assert!(bad.is_empty(), "stale docs/ citations:\n{}", bad.join("\n"));
}

#[test]
fn cited_docs_finds_paths_and_skips_lookalikes() {
    let text = "see docs/a/b.md, `docs/c.md` and \
                https://github.com/o/r/blob/main/docs/d.md; not site/content/docs/e.mdx \
                nor docs/f.mdx";
    assert_eq!(cited_docs(text), ["docs/a/b.md", "docs/c.md", "docs/d.md"]);
}

#[test]
fn slugs_follow_github() {
    let s = slugs("# Top\n## `devsbd run` verbs\n## Same\n## Same\n### Holders and idle exit\n");
    for want in ["top", "devsbd-run-verbs", "same", "same-1", "holders-and-idle-exit"] {
        assert!(s.contains(want), "{want} missing from {s:?}");
    }
}
