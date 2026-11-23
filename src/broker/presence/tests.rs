use serde_json::json;

use super::*;

fn claim() -> Pinned {
    crate::credentials::home::tests::github().pinned
}

fn targets(method: &str, path: &str, body: Option<Json>) -> Result<Vec<Named>, Unnamed> {
    http(&claim(), method, path, body.as_ref()).map(|f| f.into_iter().map(|f| f.named).collect())
}

fn named(s: &str) -> Vec<Named> {
    vec![Named::Target(s.into())]
}

#[test]
fn ref_ops_are_asked_as_the_claims_ref_entries_name_them() {
    let c = claim();
    for op in [RefOp::Create, RefOp::Update, RefOp::Delete] {
        assert!(ref_asked(&c, "refs/tags/v1", op), "{op:?}");
        assert!(ref_asked(&c, "refs/tags/a/b", op), "{op:?}");
    }
    assert!(ref_asked(&c, "refs/heads/a/x", RefOp::Delete));
    assert!(!ref_asked(&c, "refs/heads/x", RefOp::Create));
    assert!(!ref_asked(&c, "refs/heads/x", RefOp::Update));
    assert!(!ref_asked(&c, "refs/notes/x", RefOp::Delete));
    assert!(!ref_asked(&c, "refs/tags", RefOp::Delete), "`*` takes a segment");
    let mut any = c.clone();
    any.presence = vec![Presence::Ref { pattern: "refs/heads/main".into(), on: vec![] }];
    assert!(ref_asked(&any, "refs/heads/main", RefOp::Create), "empty `on`: every op");
    assert!(!ref_asked(&any, "refs/heads/mainx", RefOp::Create));
    // Release routes aren't ref ops.
    let f = http(&c, "POST", "/repos/o/r/releases", Some(&json!({ "tag_name": "v1" }))).unwrap();
    assert_eq!(f[0].ref_op, None);
}

#[test]
fn path_variable_targets() {
    let c = claim();
    for (m, path, want) in [
        ("DELETE", "/repos/o/r/git/refs/tags/v1", "refs/tags/v1"),
        ("PATCH", "/repos/o/r/git/refs/tags/v1", "refs/tags/v1"),
        // gh's encoded form, as matching decodes it.
        ("DELETE", "/repos/o/r/git/refs/tags%2Fv2", "refs/tags/v2"),
        ("DELETE", "/repos/o/r/git/refs/heads/main", "refs/heads/main"),
        ("DELETE", "/repos/o/r/git/refs/heads/a/x", "refs/heads/a/x"),
    ] {
        assert!(!reads_body(&c, m, path), "{m} {path}");
        assert_eq!(targets(m, path, None).unwrap(), named(want), "{m} {path}");
    }
    // Not presence: a branch update, a read, another route.
    for (m, path) in [
        ("PATCH", "/repos/o/r/git/refs/heads/main"),
        ("GET", "/repos/o/r/git/refs/tags/v1"),
        ("POST", "/repos/o/r/pulls"),
    ] {
        assert!(!reads_body(&c, m, path), "{m} {path}");
        assert_eq!(targets(m, path, None).unwrap(), [], "{m} {path}");
    }
}

#[test]
fn body_field_targets() {
    let c = claim();
    assert!(reads_body(&c, "POST", "/repos/o/r/git/refs"));
    let refs = |body: Json| targets("POST", "/repos/o/r/git/refs", Some(body));
    assert_eq!(refs(json!({ "ref": "refs/tags/v1", "sha": "0" })).unwrap(), named("refs/tags/v1"));
    for (body, why) in [
        (json!({ "ref": "tags/v1" }), "`tags/v1` isn't a full ref"),
        (json!({ "sha": "0" }), "the call holds none"),
        (json!({ "ref": null }), "the call holds none"),
        (json!({ "ref": "" }), "it's empty"),
        (json!({ "ref": 7 }), "it holds 7, not a name"),
        (json!({ "ref": { "x": 1 } }), "holds something other than an id or a name"),
        (json!(["refs/tags/v1"]), "a body that isn't a JSON object"),
    ] {
        let u = refs(body.clone()).unwrap_err();
        assert_eq!(u.field, "ref");
        assert!(u.why.contains(why), "{body}: {}", u.why);
    }
    assert_eq!(refs_unread(), "its body wasn't read");
}

fn refs_unread() -> String {
    targets("POST", "/repos/o/r/git/refs", None).unwrap_err().why
}

#[test]
fn a_release_is_presence_when_it_publishes() {
    let c = claim();
    let create = |body: Json| targets("POST", "/repos/o/r/releases", Some(body));
    assert!(reads_body(&c, "POST", "/repos/o/r/releases"));
    assert_eq!(create(json!({ "tag_name": "v1" })).unwrap(), named("v1"), "published at once");
    assert_eq!(create(json!({ "tag_name": "v1", "draft": false })).unwrap(), named("v1"));
    assert_eq!(create(json!({ "tag_name": "v1", "draft": "true" })).unwrap(), named("v1"), "only JSON true is a draft");
    assert_eq!(create(json!({ "tag_name": "v1", "draft": true })).unwrap(), [], "a draft isn't published");
    assert_eq!(create(json!({ "draft": false })).unwrap_err().field, "tag_name");

    let edit = |body: Json| targets("PATCH", "/repos/o/r/releases/5", Some(body));
    assert!(reads_body(&c, "PATCH", "/repos/o/r/releases/5"));
    assert_eq!(edit(json!({ "draft": false })).unwrap(), [Named::Release], "publishing: the release's tag, looked up");
    for body in [json!({}), json!({ "draft": true }), json!({ "name": "n", "tag_name": "v9" }), json!({ "draft": null })] {
        assert_eq!(edit(body.clone()).unwrap(), [], "{body}: an edit that doesn't publish");
    }
    // Other release routes aren't presence.
    assert!(!reads_body(&c, "DELETE", "/repos/o/r/releases/5"));
    assert!(!reads_body(&c, "POST", "/repos/o/r/releases/generate-notes"));
}

fn field(query: &str, variables: Json) -> graphql::Field {
    let body = json!({ "query": query, "variables": variables }).to_string();
    match graphql::parse(body.as_bytes()).unwrap() {
        graphql::Operation::Mutation(mut fields) => fields.remove(0),
        other => panic!("{other:?}"),
    }
}

#[test]
fn mutation_targets() {
    let c = claim();
    let create = field(
        "mutation($i: CreateRefInput!) { createRef(input: $i) { ref { id } } }",
        json!({ "i": { "repositoryId": "R_42", "name": "refs/tags/v1", "oid": "0" } }),
    );
    let found = mutation(&c, &create).unwrap();
    let create_op = Some(RefOp::Create);
    assert_eq!(found, [Found { field: "input.name".into(), named: Named::Target("refs/tags/v1".into()), ref_op: create_op }]);
    for (name, op) in [("deleteRef", RefOp::Delete), ("updateRef", RefOp::Update)] {
        let f = field(&format!("mutation {{ {name}(input: {{refId: \"REF_1\"}}) {{ clientMutationId }} }}"), Json::Null);
        let want = Found { field: "input.refId".into(), named: Named::RefNode("REF_1".into()), ref_op: Some(op) };
        assert_eq!(mutation(&c, &f).unwrap(), [want]);
    }
    let missing = field("mutation { deleteRef(input: {}) { clientMutationId } }", Json::Null);
    assert_eq!(mutation(&c, &missing).unwrap_err(), Unnamed { field: "input.refId".into(), why: "the call holds none".into() });
    let number = field("mutation { createRef(input: {name: 7}) { clientMutationId } }", Json::Null);
    assert!(mutation(&c, &number).unwrap_err().why.contains("not a name"));
    let other = field("mutation { addComment(input: {subjectId: \"I_1\"}) { clientMutationId } }", Json::Null);
    assert_eq!(mutation(&c, &other).unwrap(), []);
}
