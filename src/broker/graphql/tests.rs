use std::io::Write;

use serde_json::json;

use super::*;

fn body(v: serde_json::Value) -> Vec<u8> {
    v.to_string().into_bytes()
}

fn op(query: &str) -> Result<Operation, Bad> {
    parse(&body(json!({ "query": query })))
}

fn with_vars(query: &str, vars: serde_json::Value) -> Result<Operation, Bad> {
    parse(&body(json!({ "query": query, "variables": vars })))
}

fn fields(r: Result<Operation, Bad>) -> Vec<Field> {
    match r {
        Ok(Operation::Mutation(f)) => f,
        other => panic!("{other:?}"),
    }
}

fn names(r: Result<Operation, Bad>) -> Vec<String> {
    fields(r).into_iter().map(|f| f.name).collect()
}

fn refused(r: Result<Operation, Bad>, want: &str) {
    match r {
        Err(b) => {
            assert_eq!(b.status, 400, "{b:?}");
            assert!(b.why.contains(want), "{want:?} not in {:?}", b.why);
        }
        Ok(o) => panic!("{want:?}: parsed {o:?}"),
    }
}

#[test]
fn queries_parse_as_queries() {
    assert_eq!(op("query { viewer { login } }"), Ok(Operation::Query));
    assert_eq!(op("{ viewer { login } }"), Ok(Operation::Query), "shorthand");
    assert_eq!(
        parse(&body(json!({ "query": "query RepositoryInfo($owner: String!, $name: String!) { repository(owner: $owner, name: $name) { id } }", "variables": { "owner": "o", "name": "r" }, "operationName": "RepositoryInfo" }))),
        Ok(Operation::Query)
    );
    assert_eq!(parse(&body(json!({ "query": "{ a }", "variables": null, "operationName": null }))), Ok(Operation::Query));
    assert_eq!(op("# comment\n\u{feff}query Q { a(x: [1, 2.5e3, \"s\\u00e9\", \"\"\"block\"\"\", ENUM, {k: null}]) @include(if: true) { ... on X { b } } }"), Ok(Operation::Query));
}

#[test]
fn the_body_is_strict_json_with_three_fields() {
    let raw = |s: &str| parse(s.as_bytes());
    refused(raw(r#"[{"query":"{ a }"}]"#), "a batch");
    refused(raw(r#"{"query":"{ a }","query":"mutation { b }"}"#), "the key `query` twice");
    refused(raw(r#"{"query":"mutation { b(input: $i) }","variables":{"i":{"x":1,"x":2}}}"#), "the key `x` twice");
    refused(raw(r#"{"query":"{ a }","extensions":{}}"#), "field `extensions`");
    refused(raw(r#"{"query":"{ a }"} {}"#), "isn't strict JSON");
    refused(raw(r#""{ a }""#), "isn't a JSON object");
    refused(raw(r#"{"variables":{}}"#), "without a `query` string");
    refused(raw(r#"{"query":1}"#), "without a `query` string");
    refused(raw(r#"{"query":"{ a }","variables":[]}"#), "aren't an object");
    refused(raw(r#"{"query":"{ a }","operationName":1}"#), "isn't a string");
    refused(raw(""), "isn't strict JSON");
}

#[test]
fn one_operation_and_never_a_subscription() {
    refused(op("query A { a } query B { b }"), "2 operations");
    refused(op("query A { a } mutation B { b }"), "2 operations");
    refused(op("{ a } { b }"), "2 operations");
    refused(op("subscription { a }"), "a GraphQL subscription");
    refused(parse(&body(json!({ "query": "query A { a }", "operationName": "B" }))), "`operationName` `B`");
    refused(parse(&body(json!({ "query": "{ a }", "operationName": "A" }))), "`operationName` `A`");
    assert_eq!(parse(&body(json!({ "query": "mutation A { b }", "operationName": "A" }))).map(|_| ()), Ok(()));
    refused(op("fragment F on Query { a }"), "no operation");
    refused(op("type Query { a: Int }"), "isn't an executable definition");
}

#[test]
fn documents_that_dont_parse_are_refused() {
    for bad in [
        "{",
        "{ }",
        "{ a(x: ) }",
        "{ a(x: 1, x: 2) }",
        "{ a(x: {k: 1, k: 2}) }",
        "query($a: Int, $a: Int) { b }",
        "query($a: Int = $b) { c }",
        "{ a(x: 01) }",
        "{ a(x: 1.) }",
        "{ a(x: 1x) }",
        "{ a(x: \"unterminated) }",
        "{ a(x: \"line\nbreak\") }",
        "{ a(x: \"\\u{1F600}\") }",
        "{ a(x: \"\\ud800\") }",
        "{ a(x: \"\\q\") }",
        "{ a(x: \"\"\"open) }",
        "{ a } ; ",
        "{ ...on }",
        "fragment on on X { a } { ...on }",
        "{ a ..b }",
    ] {
        assert!(op(bad).is_err(), "{bad:?}");
    }
    let deep = format!("{}{}", "{ a ".repeat(100), "}".repeat(100));
    refused(op(&deep), "nesting past");
    let deep = format!("{{ a(x: {}1{}) }}", "[".repeat(100), "]".repeat(100));
    refused(op(&deep), "nesting past");
}

#[test]
fn fragments_expand_into_the_mutation_and_cycles_are_refused() {
    let q = "mutation { ...A ... on Mutation { addComment(input: {subjectId: \"I_1\"}) { clientMutationId } } } \
             fragment A on Mutation { ...B } fragment B on Mutation { createPullRequest(input: {}) { clientMutationId } }";
    assert_eq!(names(op(q)), ["createPullRequest", "addComment"]);
    // Inline fragments without a type condition, and with directives.
    assert_eq!(names(op("mutation { ... @include(if: true) { deleteRef(input: {}) { a } } }")), ["deleteRef"]);
    // A field below the top level isn't a mutation field.
    assert_eq!(names(op("mutation { a { createPullRequest(input: {}) { b } } }")), ["a"]);

    refused(op("mutation { ...A } fragment A on Mutation { ...A }"), "fragment `A` spreads itself");
    refused(op("{ x } fragment A on Q { y { ...B } } fragment B on Q { ...A }"), "spreads itself");
    refused(op("mutation { ...Missing }"), "undefined fragment `Missing`");
    refused(op("mutation { ...A } fragment A on M { a } fragment A on M { b }"), "fragment `A` defined twice");
}

#[test]
fn aliases_dont_hide_a_field() {
    let f = fields(op("mutation { harmless: createPullRequest(input: {repositoryId: \"R_1\"}) { a } x: addComment(input: {}) { a } }"));
    assert_eq!(f.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["createPullRequest", "addComment"]);
}

#[test]
fn arguments_resolve_from_literals_variables_and_defaults() {
    let q = "mutation($input: CreatePullRequestInput!, $head: ID = \"R_head\") { \
             createPullRequest(input: $input) { a } \
             b: createPullRequest(input: {repositoryId: \"R_lit\", headRepositoryId: $head}) { a } }";
    let f = fields(with_vars(q, json!({ "input": { "repositoryId": "R_var", "title": "t" } })));
    let ids = |f: &Field, p: &str| inputs(f, p).unwrap();
    assert_eq!(ids(&f[0], "input.repositoryId"), [Leaf::Str("R_var".into())]);
    assert_eq!(ids(&f[0], "input.headRepositoryId"), [], "absent holds none");
    assert_eq!(ids(&f[1], "input.repositoryId"), [Leaf::Str("R_lit".into())]);
    assert_eq!(ids(&f[1], "input.headRepositoryId"), [Leaf::Str("R_head".into())], "the default");

    // A variable sent as null is null, not its default.
    let f = fields(with_vars(q, json!({ "input": {}, "head": null })));
    assert_eq!(ids(&f[1], "input.headRepositoryId"), []);

    refused(op("mutation { a(input: $undefined) { b } }"), "variable `$undefined` isn't defined");
}

#[test]
fn input_paths_walk_objects_and_lists() {
    let q = "mutation($ids: [ID!]) { m(input: {branch: {repositoryNameWithOwner: \"o/r\", id: null}, many: $ids, one: \"R_1\", \
             nested: [{id: \"R_a\"}, {id: \"R_b\"}, {other: 1}], n: 42, big: 99999999999999999999, block: \"\"\"R_x\"\"\", e: R_enum}) { a } }";
    let f = &fields(with_vars(q, json!({ "ids": ["R_2", "R_3"] })))[0];
    let s = |v: &str| Leaf::Str(v.into());
    assert_eq!(inputs(f, "input.branch.repositoryNameWithOwner").unwrap(), [s("o/r")]);
    assert_eq!(inputs(f, "input.branch.id").unwrap(), []);
    assert_eq!(inputs(f, "input.many[]").unwrap(), [s("R_2"), s("R_3")]);
    assert_eq!(inputs(f, "input.one[]").unwrap(), [s("R_1")], "a single value as a list of one");
    assert_eq!(inputs(f, "input.nested[].id").unwrap(), [s("R_a"), s("R_b")]);
    assert_eq!(inputs(f, "input.n").unwrap(), [Leaf::Int(42)]);
    assert_eq!(inputs(f, "input.missing.deeper").unwrap(), []);
    for bad in ["input.many", "input.big", "input.block", "input.e", "input.branch", "input.one.x", "input.nested.id"] {
        assert!(inputs(f, bad).is_err(), "{bad}");
    }
}

#[test]
fn bodies_decode_within_the_bound() {
    let plain = body(json!({ "query": "{ a }" }));
    let gz = |b: &[u8]| {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(b).unwrap();
        e.finish().unwrap()
    };
    let zlib = |b: &[u8]| {
        let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(b).unwrap();
        e.finish().unwrap()
    };
    assert_eq!(decode(&plain, None).unwrap(), plain);
    assert_eq!(decode(&gz(&plain), Some(b"gzip")).unwrap(), plain);
    assert_eq!(decode(&zlib(&plain), Some(b" Deflate ")).unwrap(), plain);
    assert_eq!(decode(&plain, Some(b"identity")).unwrap(), plain);
    assert_eq!(decode(&plain, Some(b"gzip")).unwrap_err().status, 400, "doesn't decode");
    assert_eq!(decode(&plain, Some(b"br")).unwrap_err().status, 400);
    assert_eq!(decode(&plain, Some(b"gzip, gzip")).unwrap_err().status, 400);
    // A bomb: small as sent, past the bound once decoded.
    let bomb = gz(&vec![b' '; MAX_BODY as usize + 1]);
    assert!(bomb.len() < 64 * 1024);
    assert_eq!(decode(&bomb, Some(b"gzip")).unwrap_err().status, 413);
    let fits = gz(&vec![b' '; MAX_BODY as usize]);
    assert_eq!(decode(&fits, Some(b"gzip")).unwrap().len(), MAX_BODY as usize);
}
