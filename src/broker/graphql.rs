//! `POST /graphql` read strictly before policy (github-and-sentry-access,
//! *Routes the broker can attribute*; credential-broker, *Shape*, "The
//! broker"): what the operation is, and for a mutation each top-level
//! field with its arguments resolved, so policy maps the field to its scope
//! and reads the `fence` inputs from it.
//!
//! Strict where the upstream might read the request differently from us:
//! one JSON object (an array is a batch), no duplicate keys anywhere, only
//! `query`, `variables` and `operationName`; a document of exactly one
//! operation, no subscription, every fragment defined once, no fragment
//! cycle, no duplicate argument, input field or variable. Fragments,
//! spread or inline, are expanded into the mutation's top-level selection
//! whatever their type condition (an over-read only denies more); aliases
//! only rename a response key, so a field is mapped by its name.
//!
//! Hand-written rather than a parser crate: the executable subset is small,
//! and a reader that refuses what it doesn't need to understand (`\u{…}`
//! escapes, a block string where an input is read) is the point.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::Read;

use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use serde_json::{Map, Number, Value as Json};

/// The bound on the body as sent and once decoded: gh's largest documents
/// are a few KiB.
pub const MAX_BODY: u64 = 1024 * 1024;

/// Nesting past this (selections, values, types) is refused, so a body
/// within the bound can't exhaust the stack.
const MAX_DEPTH: usize = 64;

/// Why a request is refused, with its status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bad {
    pub status: u16,
    pub why: String,
}

impl Bad {
    fn new(status: u16, why: impl Into<String>) -> Bad {
        Bad { status, why: why.into() }
    }
}

/// The body decoded per its `Content-Encoding` (`gzip`, `deflate`'s zlib
/// stream, `identity`), within [`MAX_BODY`].
pub fn decode(raw: &[u8], encoding: Option<&[u8]>) -> Result<Vec<u8>, Bad> {
    let enc = match encoding.map(|e| std::str::from_utf8(e).map(|s| s.trim().to_ascii_lowercase())) {
        None => return Ok(raw.to_vec()),
        Some(Ok(e)) => e,
        Some(Err(_)) => return Err(Bad::new(400, "a GraphQL body with a `Content-Encoding` that isn't text")),
    };
    let reader: Box<dyn Read + '_> = match enc.as_str() {
        "identity" => return Ok(raw.to_vec()),
        "gzip" | "x-gzip" => Box::new(flate2::read::GzDecoder::new(raw)),
        "deflate" => Box::new(flate2::read::ZlibDecoder::new(raw)),
        _ => return Err(Bad::new(400, format!("a GraphQL body in `Content-Encoding: {enc}`: only gzip or deflate"))),
    };
    let mut out = Vec::new();
    reader
        .take(MAX_BODY + 1)
        .read_to_end(&mut out)
        .map_err(|_| Bad::new(400, format!("a GraphQL body that doesn't decode as `{enc}`")))?;
    if out.len() as u64 > MAX_BODY {
        return Err(Bad::new(413, format!("a GraphQL body over {MAX_BODY} bytes once decoded")));
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq)]
pub enum Operation {
    Query,
    /// Its top-level fields, fragments expanded, in document order.
    Mutation(Vec<Field>),
}

/// A top-level mutation field, its arguments resolved: variables replaced
/// by their JSON value or the definition's default.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: String,
    pub args: Vec<(String, Value)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Var(String),
    Null,
    Bool(bool),
    Int(String),
    Float(String),
    Str(String),
    /// A block string: never read as an input.
    Block,
    Enum(String),
    List(Vec<Value>),
    Object(Vec<(String, Value)>),
}

/// The decoded body as an [`Operation`], or why it's refused (`400`).
pub fn parse(body: &[u8]) -> Result<Operation, Bad> {
    let bad = |why: String| Bad::new(400, why);
    let req = request(body).map_err(bad)?;
    let doc = document(&req.query).map_err(|e| bad(format!("a GraphQL document that doesn't parse: {e}")))?;
    operation(&doc, req.operation_name.as_deref(), &req.variables).map_err(bad)
}

struct Request {
    query: String,
    variables: Map<String, Json>,
    operation_name: Option<String>,
}

fn request(body: &[u8]) -> Result<Request, String> {
    let mut de = serde_json::Deserializer::from_slice(body);
    let Strict(v) = Strict::deserialize(&mut de).map_err(|e| format!("a GraphQL body that isn't strict JSON: {e}"))?;
    de.end().map_err(|e| format!("a GraphQL body that isn't strict JSON: {e}"))?;
    let mut obj = match v {
        Json::Object(o) => o,
        Json::Array(_) => return Err("a batch of GraphQL requests".into()),
        _ => return Err("a GraphQL body that isn't a JSON object".into()),
    };
    if let Some(k) = obj.keys().find(|k| !matches!(k.as_str(), "query" | "variables" | "operationName")) {
        return Err(format!("a GraphQL body with field `{k}`: only `query`, `variables` and `operationName`"));
    }
    let query = match obj.remove("query") {
        Some(Json::String(q)) => q,
        _ => return Err("a GraphQL body without a `query` string".into()),
    };
    let variables = match obj.remove("variables") {
        None | Some(Json::Null) => Map::new(),
        Some(Json::Object(o)) => o,
        Some(_) => return Err("GraphQL `variables` that aren't an object".into()),
    };
    let operation_name = match obj.remove("operationName") {
        None | Some(Json::Null) => None,
        Some(Json::String(s)) => Some(s),
        Some(_) => return Err("a GraphQL `operationName` that isn't a string".into()),
    };
    Ok(Request { query, variables, operation_name })
}

/// JSON as `serde_json` reads it, but an object naming a key twice is an
/// error: the upstream may take the other one.
struct Strict(Json);

impl<'de> Deserialize<'de> for Strict {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Strict, D::Error> {
        d.deserialize_any(StrictVisitor)
    }
}

struct StrictVisitor;

impl<'de> Visitor<'de> for StrictVisitor {
    type Value = Strict;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }
    fn visit_bool<E>(self, v: bool) -> Result<Strict, E> {
        Ok(Strict(Json::Bool(v)))
    }
    fn visit_i64<E>(self, v: i64) -> Result<Strict, E> {
        Ok(Strict(Json::Number(v.into())))
    }
    fn visit_u64<E>(self, v: u64) -> Result<Strict, E> {
        Ok(Strict(Json::Number(v.into())))
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Strict, E> {
        Number::from_f64(v).map(|n| Strict(Json::Number(n))).ok_or_else(|| E::custom("a number JSON can't hold"))
    }
    fn visit_str<E>(self, v: &str) -> Result<Strict, E> {
        Ok(Strict(Json::String(v.to_string())))
    }
    fn visit_string<E>(self, v: String) -> Result<Strict, E> {
        Ok(Strict(Json::String(v)))
    }
    fn visit_unit<E>(self) -> Result<Strict, E> {
        Ok(Strict(Json::Null))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Strict, A::Error> {
        let mut out = Vec::new();
        while let Some(Strict(v)) = seq.next_element()? {
            out.push(v);
        }
        Ok(Strict(Json::Array(out)))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Strict, A::Error> {
        let mut out = Map::new();
        while let Some(k) = map.next_key::<String>()? {
            if out.contains_key(&k) {
                return Err(de::Error::custom(format!("the key `{k}` twice in one object")));
            }
            let Strict(v) = map.next_value()?;
            out.insert(k, v);
        }
        Ok(Strict(Json::Object(out)))
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Punct(u8),
    Spread,
    Name(String),
    Int(String),
    Float(String),
    Str(String),
    Block,
}

fn lex(src: &str) -> Result<Vec<Tok>, String> {
    let b = src.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    let name_start = |c: u8| c == b'_' || c.is_ascii_alphabetic();
    let name_part = |c: u8| c == b'_' || c.is_ascii_alphanumeric();
    while i < b.len() {
        let c = b[i];
        match c {
            b' ' | b'\t' | b'\n' | b'\r' | b',' => i += 1,
            0xEF if b[i..].starts_with(&[0xEF, 0xBB, 0xBF]) => i += 3,
            b'#' => {
                while i < b.len() && !matches!(b[i], b'\n' | b'\r') {
                    i += 1;
                }
            }
            b'.' if b[i..].starts_with(b"...") => {
                out.push(Tok::Spread);
                i += 3;
            }
            b'!' | b'$' | b'&' | b'(' | b')' | b':' | b'=' | b'@' | b'[' | b']' | b'{' | b'|' | b'}' => {
                out.push(Tok::Punct(c));
                i += 1;
            }
            _ if name_start(c) => {
                let at = i;
                while i < b.len() && name_part(b[i]) {
                    i += 1;
                }
                out.push(Tok::Name(src[at..i].to_string()));
            }
            b'-' | b'0'..=b'9' => {
                let at = i;
                if b[i] == b'-' {
                    i += 1;
                }
                let digits = |i: &mut usize| {
                    let s = *i;
                    while *i < b.len() && b[*i].is_ascii_digit() {
                        *i += 1;
                    }
                    *i - s
                };
                let int_at = i;
                if digits(&mut i) == 0 {
                    return Err("a number without digits".into());
                }
                if b[int_at] == b'0' && i - int_at > 1 {
                    return Err("a number with a leading zero".into());
                }
                let mut float = false;
                if i < b.len() && b[i] == b'.' {
                    i += 1;
                    if digits(&mut i) == 0 {
                        return Err("a number with no digits after `.`".into());
                    }
                    float = true;
                }
                if i < b.len() && matches!(b[i], b'e' | b'E') {
                    i += 1;
                    if i < b.len() && matches!(b[i], b'+' | b'-') {
                        i += 1;
                    }
                    if digits(&mut i) == 0 {
                        return Err("a number with no exponent digits".into());
                    }
                    float = true;
                }
                if i < b.len() && (b[i] == b'.' || name_start(b[i])) {
                    return Err("a number run into a name or `.`".into());
                }
                let text = src[at..i].to_string();
                out.push(if float { Tok::Float(text) } else { Tok::Int(text) });
            }
            b'"' if b[i..].starts_with(b"\"\"\"") => {
                i += 3;
                loop {
                    if i >= b.len() {
                        return Err("an unterminated block string".into());
                    }
                    if b[i..].starts_with(b"\\\"\"\"") {
                        i += 4;
                    } else if b[i..].starts_with(b"\"\"\"") {
                        i += 3;
                        break;
                    } else {
                        i += 1;
                    }
                }
                out.push(Tok::Block);
            }
            b'"' => {
                let (s, next) = string(src, i + 1)?;
                out.push(Tok::Str(s));
                i = next;
            }
            _ => return Err(format!("an unexpected character at byte {i}")),
        }
    }
    Ok(out)
}

/// A string's value from just past its opening quote, and where it ends.
fn string(src: &str, mut i: usize) -> Result<(String, usize), String> {
    let b = src.as_bytes();
    let mut out = String::new();
    loop {
        let Some(&c) = b.get(i) else { return Err("an unterminated string".into()) };
        match c {
            b'"' => return Ok((out, i + 1)),
            b'\n' | b'\r' => return Err("a line break in a string".into()),
            b'\\' => {
                let e = *b.get(i + 1).ok_or("an unterminated string")?;
                i += 2;
                match e {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\u{8}'),
                    b'f' => out.push('\u{c}'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' => {
                        let hex = |at: usize| {
                            src.get(at..at + 4)
                                .filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()))
                                .and_then(|h| u32::from_str_radix(h, 16).ok())
                                .ok_or_else(|| "a `\\u` escape that isn't four hex digits".to_string())
                        };
                        let hi = hex(i)?;
                        i += 4;
                        let code = match hi {
                            0xD800..=0xDBFF => {
                                if !src[i..].starts_with("\\u") {
                                    return Err("a lone surrogate escape".into());
                                }
                                let lo = hex(i + 2)?;
                                if !(0xDC00..=0xDFFF).contains(&lo) {
                                    return Err("a lone surrogate escape".into());
                                }
                                i += 6;
                                0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                            }
                            0xDC00..=0xDFFF => return Err("a lone surrogate escape".into()),
                            c => c,
                        };
                        out.push(char::from_u32(code).ok_or("an escape naming no character")?);
                    }
                    _ => return Err("an unknown string escape".into()),
                }
            }
            c if c < 0x20 && c != b'\t' => return Err("a control character in a string".into()),
            _ => {
                let ch = src[i..].chars().next().expect("inside the string");
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Query,
    Mutation,
    Subscription,
}

#[derive(Debug)]
struct OpDef {
    kind: Kind,
    name: Option<String>,
    vars: BTreeMap<String, Option<Value>>,
    selection: Vec<Selection>,
}

#[derive(Debug)]
struct Fragment {
    selection: Vec<Selection>,
}

#[derive(Debug)]
enum Selection {
    Field { name: String, args: Vec<(String, Value)>, selection: Vec<Selection> },
    Spread(String),
    Inline(Vec<Selection>),
}

#[derive(Debug, Default)]
struct Document {
    ops: Vec<OpDef>,
    fragments: BTreeMap<String, Fragment>,
}

struct Parser {
    toks: Vec<Tok>,
    at: usize,
    depth: usize,
}

type Parsed<T> = Result<T, String>;

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.at)
    }

    fn next(&mut self) -> Parsed<Tok> {
        let t = self.toks.get(self.at).cloned().ok_or("an unexpected end")?;
        self.at += 1;
        Ok(t)
    }

    fn is(&self, c: u8) -> bool {
        self.peek() == Some(&Tok::Punct(c))
    }

    fn is_name(&self, n: &str) -> bool {
        matches!(self.peek(), Some(Tok::Name(x)) if x == n)
    }

    fn expect(&mut self, c: u8) -> Parsed<()> {
        match self.next()? {
            Tok::Punct(p) if p == c => Ok(()),
            _ => Err(format!("`{}` expected", c as char)),
        }
    }

    fn name(&mut self) -> Parsed<String> {
        match self.next()? {
            Tok::Name(n) => Ok(n),
            _ => Err("a name expected".into()),
        }
    }

    fn deeper(&mut self) -> Parsed<()> {
        self.depth += 1;
        match self.depth > MAX_DEPTH {
            true => Err(format!("nesting past {MAX_DEPTH} levels")),
            false => Ok(()),
        }
    }

    fn document(&mut self) -> Parsed<Document> {
        let mut doc = Document::default();
        while self.peek().is_some() {
            if self.is(b'{') {
                let selection = self.selection_set()?;
                doc.ops.push(OpDef { kind: Kind::Query, name: None, vars: BTreeMap::new(), selection });
                continue;
            }
            let word = self.name()?;
            let kind = match word.as_str() {
                "query" => Kind::Query,
                "mutation" => Kind::Mutation,
                "subscription" => Kind::Subscription,
                "fragment" => {
                    let name = self.name()?;
                    if name == "on" {
                        return Err("a fragment named `on`".into());
                    }
                    if !self.is_name("on") {
                        return Err("a fragment without its type condition".into());
                    }
                    self.at += 1;
                    self.name()?;
                    self.directives()?;
                    let selection = self.selection_set()?;
                    if doc.fragments.insert(name.clone(), Fragment { selection }).is_some() {
                        return Err(format!("fragment `{name}` defined twice"));
                    }
                    continue;
                }
                other => return Err(format!("`{other}` isn't an executable definition")),
            };
            let name = match self.peek() {
                Some(Tok::Name(_)) => Some(self.name()?),
                _ => None,
            };
            let vars = match self.is(b'(') {
                true => self.variable_definitions()?,
                false => BTreeMap::new(),
            };
            self.directives()?;
            let selection = self.selection_set()?;
            doc.ops.push(OpDef { kind, name, vars, selection });
        }
        if doc.ops.is_empty() {
            return Err("no operation".into());
        }
        Ok(doc)
    }

    fn variable_definitions(&mut self) -> Parsed<BTreeMap<String, Option<Value>>> {
        self.expect(b'(')?;
        let mut vars = BTreeMap::new();
        loop {
            self.expect(b'$')?;
            let name = self.name()?;
            self.expect(b':')?;
            self.type_ref()?;
            let default = match self.is(b'=') {
                true => {
                    self.at += 1;
                    Some(self.value(true)?)
                }
                false => None,
            };
            self.directives()?;
            if vars.insert(name.clone(), default).is_some() {
                return Err(format!("variable `${name}` defined twice"));
            }
            if self.is(b')') {
                self.at += 1;
                return Ok(vars);
            }
        }
    }

    fn type_ref(&mut self) -> Parsed<()> {
        self.deeper()?;
        if self.is(b'[') {
            self.at += 1;
            self.type_ref()?;
            self.expect(b']')?;
        } else {
            self.name()?;
        }
        if self.is(b'!') {
            self.at += 1;
        }
        self.depth -= 1;
        Ok(())
    }

    fn directives(&mut self) -> Parsed<()> {
        while self.is(b'@') {
            self.at += 1;
            self.name()?;
            if self.is(b'(') {
                self.arguments()?;
            }
        }
        Ok(())
    }

    fn arguments(&mut self) -> Parsed<Vec<(String, Value)>> {
        self.expect(b'(')?;
        let mut args: Vec<(String, Value)> = Vec::new();
        loop {
            let name = self.name()?;
            self.expect(b':')?;
            let v = self.value(false)?;
            if args.iter().any(|(n, _)| *n == name) {
                return Err(format!("argument `{name}` given twice"));
            }
            args.push((name, v));
            if self.is(b')') {
                self.at += 1;
                return Ok(args);
            }
        }
    }

    fn selection_set(&mut self) -> Parsed<Vec<Selection>> {
        self.deeper()?;
        self.expect(b'{')?;
        let mut out = Vec::new();
        loop {
            out.push(self.selection()?);
            if self.is(b'}') {
                self.at += 1;
                self.depth -= 1;
                return Ok(out);
            }
        }
    }

    fn selection(&mut self) -> Parsed<Selection> {
        if self.peek() == Some(&Tok::Spread) {
            self.at += 1;
            if self.is_name("on") {
                self.at += 1;
                self.name()?;
            } else if let Some(Tok::Name(_)) = self.peek() {
                let name = self.name()?;
                self.directives()?;
                return Ok(Selection::Spread(name));
            }
            self.directives()?;
            return Ok(Selection::Inline(self.selection_set()?));
        }
        let mut name = self.name()?;
        if self.is(b':') {
            self.at += 1;
            name = self.name()?;
        }
        let args = match self.is(b'(') {
            true => self.arguments()?,
            false => Vec::new(),
        };
        self.directives()?;
        let selection = match self.is(b'{') {
            true => self.selection_set()?,
            false => Vec::new(),
        };
        Ok(Selection::Field { name, args, selection })
    }

    /// `konst`: a variable's default, where no variable may appear.
    fn value(&mut self, konst: bool) -> Parsed<Value> {
        self.deeper()?;
        let v = match self.next()? {
            Tok::Punct(b'$') if !konst => Value::Var(self.name()?),
            Tok::Int(s) => Value::Int(s),
            Tok::Float(s) => Value::Float(s),
            Tok::Str(s) => Value::Str(s),
            Tok::Block => Value::Block,
            Tok::Name(n) => match n.as_str() {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                "null" => Value::Null,
                _ => Value::Enum(n),
            },
            Tok::Punct(b'[') => {
                let mut items = Vec::new();
                while !self.is(b']') {
                    items.push(self.value(konst)?);
                }
                self.at += 1;
                Value::List(items)
            }
            Tok::Punct(b'{') => {
                let mut fields: Vec<(String, Value)> = Vec::new();
                while !self.is(b'}') {
                    let name = self.name()?;
                    self.expect(b':')?;
                    let v = self.value(konst)?;
                    if fields.iter().any(|(n, _)| *n == name) {
                        return Err(format!("input field `{name}` given twice"));
                    }
                    fields.push((name, v));
                }
                self.at += 1;
                Value::Object(fields)
            }
            _ => return Err("a value expected".into()),
        };
        self.depth -= 1;
        Ok(v)
    }
}

fn document(src: &str) -> Parsed<Document> {
    Parser { toks: lex(src)?, at: 0, depth: 0 }.document()
}

fn spreads<'a>(sel: &'a [Selection], out: &mut Vec<&'a str>) {
    for s in sel {
        match s {
            Selection::Field { selection, .. } | Selection::Inline(selection) => spreads(selection, out),
            Selection::Spread(n) => out.push(n),
        }
    }
}

/// Every spread names a defined fragment, and no fragment reaches itself.
fn check_fragments(doc: &Document) -> Parsed<()> {
    let mut all = Vec::new();
    for op in &doc.ops {
        spreads(&op.selection, &mut all);
    }
    for f in doc.fragments.values() {
        spreads(&f.selection, &mut all);
    }
    if let Some(n) = all.iter().find(|n| !doc.fragments.contains_key(**n)) {
        return Err(format!("a spread of undefined fragment `{n}`"));
    }
    fn visit<'a>(doc: &'a Document, name: &'a str, path: &mut Vec<&'a str>, done: &mut BTreeSet<&'a str>) -> Parsed<()> {
        if path.contains(&name) {
            return Err(format!("fragment `{name}` spreads itself"));
        }
        if !done.insert(name) {
            return Ok(());
        }
        path.push(name);
        let mut next = Vec::new();
        spreads(&doc.fragments[name].selection, &mut next);
        for n in next {
            visit(doc, n, path, done)?;
        }
        path.pop();
        Ok(())
    }
    let mut done = BTreeSet::new();
    for name in doc.fragments.keys() {
        visit(doc, name, &mut Vec::new(), &mut done)?;
    }
    Ok(())
}

fn top_fields<'a>(doc: &'a Document, sel: &'a [Selection], out: &mut Vec<(&'a str, &'a [(String, Value)])>) {
    for s in sel {
        match s {
            Selection::Field { name, args, .. } => out.push((name, args)),
            Selection::Inline(inner) => top_fields(doc, inner, out),
            Selection::Spread(n) => top_fields(doc, &doc.fragments[n.as_str()].selection, out),
        }
    }
}

fn operation(doc: &Document, name: Option<&str>, vars: &Map<String, Json>) -> Parsed<Operation> {
    let [op] = doc.ops.as_slice() else {
        return Err(format!("a GraphQL document of {} operations: one per request", doc.ops.len()));
    };
    if let Some(n) = name
        && op.name.as_deref() != Some(n)
    {
        return Err(format!("`operationName` `{n}` names no operation of the document"));
    }
    check_fragments(doc).map_err(|e| format!("a GraphQL document that doesn't parse: {e}"))?;
    match op.kind {
        Kind::Query => Ok(Operation::Query),
        Kind::Subscription => Err("a GraphQL subscription".into()),
        Kind::Mutation => {
            let mut top = Vec::new();
            top_fields(doc, &op.selection, &mut top);
            let fields = top
                .into_iter()
                .map(|(name, args)| {
                    let args = args.iter().map(|(n, v)| Ok((n.clone(), resolve(v, op, vars)?))).collect::<Parsed<_>>()?;
                    Ok(Field { name: name.to_string(), args })
                })
                .collect::<Parsed<_>>()?;
            Ok(Operation::Mutation(fields))
        }
    }
}

fn resolve(v: &Value, op: &OpDef, vars: &Map<String, Json>) -> Parsed<Value> {
    Ok(match v {
        Value::Var(n) => {
            let Some(default) = op.vars.get(n) else {
                return Err(format!("variable `${n}` isn't defined by the operation"));
            };
            match vars.get(n) {
                Some(j) => from_json(j),
                None => default.clone().unwrap_or(Value::Null),
            }
        }
        Value::List(items) => Value::List(items.iter().map(|i| resolve(i, op, vars)).collect::<Parsed<_>>()?),
        Value::Object(fields) => {
            Value::Object(fields.iter().map(|(k, v)| Ok((k.clone(), resolve(v, op, vars)?))).collect::<Parsed<_>>()?)
        }
        other => other.clone(),
    })
}

fn from_json(j: &Json) -> Value {
    match j {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) if n.is_i64() || n.is_u64() => Value::Int(n.to_string()),
        Json::Number(n) => Value::Float(n.to_string()),
        Json::String(s) => Value::Str(s.clone()),
        Json::Array(a) => Value::List(a.iter().map(from_json).collect()),
        Json::Object(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), from_json(v))).collect()),
    }
}

/// A value a `fence` input holds: an id or name as a string, or an integer
/// id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Leaf {
    Str(String),
    Int(u64),
}

/// Every value at `path` (a claim `fence` input: dotted, `[]` for each
/// element of a list) in `field`'s arguments; an absent or `null` step
/// holds none. A `[]` step over a single value takes it as a list of one,
/// as GraphQL coerces it.
pub fn inputs(field: &Field, path: &str) -> Result<Vec<Leaf>, String> {
    let root = Value::Object(field.args.clone());
    let mut at = vec![&root];
    let segments: Vec<&str> = path.split('.').collect();
    for seg in &segments {
        let (name, list) = seg.strip_suffix("[]").map_or((*seg, false), |n| (n, true));
        let mut next = Vec::new();
        for v in at {
            match v {
                Value::Null => {}
                Value::Object(fields) => next.extend(fields.iter().find(|(k, _)| k == name).map(|(_, v)| v)),
                _ => return Err(format!("`{}`'s `{path}` crosses a value that isn't an input object", field.name)),
            }
        }
        at = match list {
            true => next
                .into_iter()
                .flat_map(|v| match v {
                    Value::List(items) => items.iter().collect(),
                    Value::Null => Vec::new(),
                    one => vec![one],
                })
                .collect(),
            false => next,
        };
    }
    let mut out = Vec::new();
    for v in at {
        match v {
            Value::Null => {}
            Value::Str(s) => out.push(Leaf::Str(s.clone())),
            Value::Int(i) => out.push(Leaf::Int(
                i.parse().map_err(|_| format!("`{}`'s `{path}` holds `{i}`, not an id", field.name))?,
            )),
            _ => return Err(format!("`{}`'s `{path}` holds something other than an id or a name", field.name)),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
