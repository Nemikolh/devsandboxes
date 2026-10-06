//! Test-only drift guard for the npm package's hand-written typings
//! (`npm/devsandboxes/index.d.ts`): no codegen dependency, instead tests
//! serialize a representative value of each wire struct and check that its
//! keys are exactly the properties of the matching `interface`. A struct
//! gaining, losing or renaming a field without a d.ts update fails the build.
//!
//! The parse is deliberately crude: `interface <Name> [extends A, B] {` up to
//! its matching `}`, one property per line at the top level of the block.

use std::collections::BTreeSet;

use serde_json::Value;

pub const DTS: &str = include_str!("../../npm/devsandboxes/index.d.ts");

/// `interface <name>`'s properties, own first then inherited through
/// `extends`, as `(name, type text)`. Panics when there's no such interface.
pub fn interface(name: &str) -> Vec<(String, String)> {
    let (header, body) = block(name).unwrap_or_else(|| panic!("index.d.ts has no `interface {name}`"));
    let mut out = props(body);
    if let Some((_, bases)) = header.split_once("extends") {
        for base in bases.split(',') {
            let base = base.trim().split('<').next().unwrap_or("").trim();
            if !base.is_empty() {
                out.extend(interface(base));
            }
        }
    }
    out
}

/// The type text of `interface <iface>`'s property `prop`.
pub fn prop_type(iface: &str, prop: &str) -> Option<String> {
    interface(iface).into_iter().find(|(n, _)| n == prop).map(|(_, t)| t)
}

/// `value` (a JSON object) has exactly the properties of `interface <iface>`.
pub fn assert_matches(iface: &str, value: &Value) {
    let keys: BTreeSet<&str> = value
        .as_object()
        .unwrap_or_else(|| panic!("{iface}: not an object: {value}"))
        .keys()
        .map(String::as_str)
        .collect();
    let fields = interface(iface);
    let fields: BTreeSet<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
    let missing: Vec<_> = keys.difference(&fields).collect();
    let stale: Vec<_> = fields.difference(&keys).collect();
    assert!(
        missing.is_empty() && stale.is_empty(),
        "npm/devsandboxes/index.d.ts `interface {iface}` drifted from the wire: \
         add {missing:?}, remove {stale:?} (wire value: {value})"
    );
}

/// One variant of a `type`-tagged union: `value` matches `interface <iface>`
/// exactly, and the interface types `type` as the literal the value carries
/// (`type: 'reply';` for `{"type":"reply",…}`).
pub fn assert_variant(iface: &str, value: &Value) {
    assert_matches(iface, value);
    let tag = value["type"].as_str().unwrap_or_else(|| panic!("{iface}: no string `type` in {value}"));
    assert_eq!(
        prop_type(iface, "type").as_deref(),
        Some(format!("'{tag}'").as_str()),
        "npm/devsandboxes/index.d.ts `interface {iface}`: `type` must be '{tag}'"
    );
}

/// The members of `export type <name> = A | B | …;`, in order. Panics when
/// there's no such alias.
pub fn union_members(name: &str) -> Vec<String> {
    let needle = format!("export type {name} =");
    let at = DTS.find(&needle).unwrap_or_else(|| panic!("index.d.ts has no `export type {name}`")) + needle.len();
    let end = DTS[at..].find(';').map_or(DTS.len(), |e| at + e);
    DTS[at..end].split('|').map(|m| m.trim().to_string()).filter(|m| !m.is_empty()).collect()
}

/// `(header, body)` of `interface <name>`: the text between the name and the
/// `{`, and the block's contents.
fn block(name: &str) -> Option<(&'static str, &'static str)> {
    let needle = format!("interface {name}");
    let mut from = 0;
    let start = loop {
        let at = DTS[from..].find(&needle)? + from;
        let after = DTS[at + needle.len()..].chars().next()?;
        if matches!(after, ' ' | '{' | '<') {
            break at + needle.len();
        }
        from = at + needle.len();
    };
    let open = DTS[start..].find('{')? + start;
    let mut depth = 0usize;
    for (i, c) in DTS[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((&DTS[start..open], &DTS[open + 1..open + i]));
                }
            }
            _ => {}
        }
    }
    None
}

/// The block's top-level properties: `name: T;`, `name?: T;`,
/// `readonly name: T;`, `'quoted.name': T;`. Comment lines are skipped.
fn props(body: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    for line in body.lines() {
        let t = line.trim();
        let comment = t.starts_with('/') || t.starts_with('*');
        if depth == 0 && !comment {
            if let Some(p) = prop(t) {
                out.push(p);
            }
        }
        if !comment {
            depth += t.matches(['{', '(', '[']).count() as i32 - t.matches(['}', ')', ']']).count() as i32;
        }
    }
    out
}

fn prop(line: &str) -> Option<(String, String)> {
    let line = line.strip_prefix("readonly ").unwrap_or(line);
    let (name, rest) = if let Some(q) = line.strip_prefix('\'') {
        let end = q.find('\'')?;
        (q[..end].to_string(), &q[end + 1..])
    } else {
        let end = line.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))?;
        (line[..end].to_string(), &line[end..])
    };
    if name.is_empty() {
        return None;
    }
    let rest = rest.strip_prefix('?').unwrap_or(rest);
    let ty = rest.strip_prefix(':')?;
    Some((name, ty.trim().trim_end_matches(';').trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_props_extends_and_quoted_names() {
        let body = "\n  /** doc { braces } */\n  id: number;\n  readonly name?: string | null;\n  'inbox.changed': InboxChanged;\n  close: (x: {\n    a: number;\n  }) => void;\n  after: boolean;\n";
        let names: Vec<_> = props(body).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["id", "name", "inbox.changed", "close", "after"]);
        assert_eq!(props(body)[1].1, "string | null");
        // Inherited through `extends`.
        assert!(interface("ThreadDetail").iter().any(|(n, _)| n == "owner_name"));
    }
}
