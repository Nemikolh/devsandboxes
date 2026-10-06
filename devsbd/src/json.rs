//! A JSON syntax checker (RFC 8259) that also pulls top-level string fields
//! out: `key` for `devsbd thread put`, `thread` + `id` for `devsbd thread
//! send` (docs/inbox-threads.md, *Decisions*: "Helper: syntax check only";
//! docs/inbox-redesign.md, *Threads and messages*). The helper has no
//! dependencies on purpose, and the schema belongs to the host, so this
//! validates the document and extracts the fields the outbox and the wire
//! record need — nothing else is decoded or kept.
//!
//! Rejecting malformed JSON here, rather than letting the host do it, keeps a
//! typo from becoming an error record minutes later with no exit status for
//! the dispatcher to notice.

/// Nesting cap for objects and arrays. A thread body is a flat record with one
/// array of small objects; anything deeper is a mistake or an attempt to blow
/// the (recursive) parser's stack.
const MAX_DEPTH: usize = 32;

/// Validate `text` as exactly one JSON document and return its top-level
/// object's `key` field. The error is one line, fit for stderr.
pub fn parse_key(text: &str) -> Result<String, String> {
    let [key] = parse_fields(text, ["key"])?;
    Ok(key)
}

/// Validate `text` as exactly one JSON document and return the top-level
/// object's string fields `names`, in that order; each must be present and a
/// string. The error is one line, fit for stderr.
pub fn parse_fields<const N: usize>(text: &str, names: [&str; N]) -> Result<[String; N], String> {
    let mut p = Parser { b: text.as_bytes(), i: 0 };
    p.ws();
    if p.peek() != Some(b'{') {
        return Err(p.err("expected a JSON object"));
    }
    // Outer: the member was seen; inner: its value, `None` when not a string.
    let mut found: [Option<Option<String>>; N] = std::array::from_fn(|_| None);
    p.i += 1;
    p.ws();
    if p.peek() == Some(b'}') {
        p.i += 1;
    } else {
        loop {
            p.ws();
            let name = p.string()?;
            p.ws();
            p.expect(b':')?;
            p.ws();
            // Only `names` are decoded; every other value is validated and dropped.
            if let Some(slot) = names.iter().position(|n| *n == name) {
                found[slot] = Some(match p.peek() {
                    Some(b'"') => Some(p.string()?),
                    _ => {
                        p.value(1)?;
                        None
                    }
                });
            } else {
                p.value(1)?;
            }
            p.ws();
            match p.next() {
                Some(b',') => continue,
                Some(b'}') => break,
                _ => return Err(p.err("expected `,` or `}`")),
            }
        }
    }
    p.ws();
    if p.i != p.b.len() {
        return Err(p.err("trailing data after the JSON value"));
    }
    let mut out: [String; N] = std::array::from_fn(|_| String::new());
    for ((name, value), slot) in names.iter().zip(found).zip(out.iter_mut()) {
        *slot = match value {
            Some(Some(v)) => v,
            Some(None) => return Err(format!("`{name}` is not a string")),
            None => return Err(format!("no `{name}` field")),
        };
    }
    Ok(out)
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    /// `<what> at byte <offset>`: enough to find the problem in a one-liner.
    fn err(&self, what: &str) -> String {
        format!("{what} at byte {}", self.i)
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let c = self.peek()?;
        self.i += 1;
        Some(c)
    }

    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn expect(&mut self, c: u8) -> Result<(), String> {
        if self.peek() == Some(c) {
            self.i += 1;
            return Ok(());
        }
        Err(self.err(&format!("expected `{}`", c as char)))
    }

    /// One value at `depth` levels of nesting, validated and discarded.
    fn value(&mut self, depth: usize) -> Result<(), String> {
        if depth > MAX_DEPTH {
            return Err(self.err(&format!("nested deeper than {MAX_DEPTH}")));
        }
        match self.peek() {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => self.string().map(drop),
            Some(b't') => self.literal("true"),
            Some(b'f') => self.literal("false"),
            Some(b'n') => self.literal("null"),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            _ => Err(self.err("expected a JSON value")),
        }
    }

    fn object(&mut self, depth: usize) -> Result<(), String> {
        self.i += 1;
        self.ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(());
        }
        loop {
            self.ws();
            self.string()?;
            self.ws();
            self.expect(b':')?;
            self.ws();
            self.value(depth + 1)?;
            self.ws();
            match self.next() {
                Some(b',') => continue,
                Some(b'}') => return Ok(()),
                _ => return Err(self.err("expected `,` or `}`")),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<(), String> {
        self.i += 1;
        self.ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(());
        }
        loop {
            self.ws();
            self.value(depth + 1)?;
            self.ws();
            match self.next() {
                Some(b',') => continue,
                Some(b']') => return Ok(()),
                _ => return Err(self.err("expected `,` or `]`")),
            }
        }
    }

    fn literal(&mut self, word: &str) -> Result<(), String> {
        if self.b[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            return Ok(());
        }
        Err(self.err("expected a JSON value"))
    }

    /// `-? int frac? exp?`, RFC 8259 §6: no leading `+`, no leading zeros, no
    /// bare `.5`, no `1.` and no hex.
    fn number(&mut self) -> Result<(), String> {
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        match self.next() {
            Some(b'0') => {}
            Some(c) if c.is_ascii_digit() => self.digits(),
            _ => return Err(self.err("expected a digit")),
        }
        if self.peek() == Some(b'.') {
            self.i += 1;
            if !self.peek().is_some_and(|c| c.is_ascii_digit()) {
                return Err(self.err("expected a digit"));
            }
            self.digits();
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if !self.peek().is_some_and(|c| c.is_ascii_digit()) {
                return Err(self.err("expected a digit"));
            }
            self.digits();
        }
        Ok(())
    }

    fn digits(&mut self) {
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
    }

    /// One string, decoded. The input is a `&str`, so non-escaped bytes are
    /// already valid UTF-8 and are copied through; only escapes are rebuilt.
    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = String::new();
        let mut plain = self.i;
        loop {
            let start = self.i;
            let c = self.next().ok_or_else(|| self.err("unterminated string"))?;
            match c {
                b'"' => {
                    out.push_str(self.slice(plain, start));
                    return Ok(out);
                }
                b'\\' => {
                    out.push_str(self.slice(plain, start));
                    out.push(self.escape()?);
                    plain = self.i;
                }
                // RFC 8259: unescaped control characters are not allowed.
                0x00..=0x1f => return Err(self.err("raw control character in a string")),
                _ => {}
            }
        }
    }

    /// `self.b[from..to]` as a `&str`. Both ends are character boundaries: the
    /// scanner only stops on ASCII bytes, which never occur inside a multi-byte
    /// UTF-8 sequence.
    fn slice(&self, from: usize, to: usize) -> &str {
        std::str::from_utf8(&self.b[from..to]).unwrap_or("")
    }

    /// The character after a `\`.
    fn escape(&mut self) -> Result<char, String> {
        let c = self.next().ok_or_else(|| self.err("unterminated escape"))?;
        Ok(match c {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => return self.unicode_escape(),
            _ => return Err(self.err("bad escape")),
        })
    }

    /// `\uXXXX`, with a surrogate pair for anything past the BMP. A lone
    /// surrogate has no character to decode to, so it's refused rather than
    /// turned into U+FFFD.
    fn unicode_escape(&mut self) -> Result<char, String> {
        let hi = self.hex4()?;
        if !(0xd800..0xdc00).contains(&hi) {
            return char::from_u32(hi).ok_or_else(|| self.err("lone surrogate in a `\\u` escape"));
        }
        if self.next() != Some(b'\\') || self.next() != Some(b'u') {
            return Err(self.err("`\\u` high surrogate without its pair"));
        }
        let lo = self.hex4()?;
        if !(0xdc00..0xe000).contains(&lo) {
            return Err(self.err("`\\u` high surrogate without its pair"));
        }
        let c = 0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00);
        char::from_u32(c).ok_or_else(|| self.err("bad `\\u` escape"))
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let end = self.i + 4;
        let digits = self.b.get(self.i..end).ok_or_else(|| self.err("short `\\u` escape"))?;
        let mut n = 0u32;
        for d in digits {
            let v = match d {
                b'0'..=b'9' => d - b'0',
                b'a'..=b'f' => d - b'a' + 10,
                b'A'..=b'F' => d - b'A' + 10,
                _ => return Err(self.err("bad `\\u` escape")),
            };
            n = n * 16 + u32::from(v);
        }
        self.i = end;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_the_key_past_every_value_shape() {
        let json = r#" {
            "title": "a \"quoted\" \u00e9 \ud83d\ude00 \\ \/ \b\f\n\r\t",
            "nested": {"a": [1, -2.5e+10, 0.5, true, false, null, {}, []]},
            "key": "pr-123",
            "empty": {}
        } "#;
        assert_eq!(parse_key(json), Ok("pr-123".into()));
        // Minimal, and a duplicate member (JSON allows it; last wins).
        assert_eq!(parse_key("{\"key\":\"a\"}"), Ok("a".into()));
        assert_eq!(parse_key("{\"key\":\"a\",\"key\":\"b\"}"), Ok("b".into()));
        // An empty key is syntactically fine here; the key rules reject it later.
        assert_eq!(parse_key("{\"key\":\"\"}"), Ok(String::new()));
    }

    #[test]
    fn decodes_escapes_in_the_key() {
        assert_eq!(parse_key(r#"{"key":"a\u002db"}"#), Ok("a-b".into()));
        assert_eq!(parse_key(r#"{"key":"\ud83d\ude00"}"#), Ok("😀".into()));
        assert_eq!(parse_key(r#"{"key":"a\\\nb"}"#), Ok("a\\\nb".into()));
    }

    #[test]
    fn rejects_a_missing_or_non_string_key() {
        assert_eq!(parse_key("{}").unwrap_err(), "no `key` field");
        assert_eq!(parse_key(r#"{"title":"x"}"#).unwrap_err(), "no `key` field");
        assert_eq!(parse_key(r#"{"key":1}"#).unwrap_err(), "`key` is not a string");
        assert_eq!(parse_key(r#"{"key":null}"#).unwrap_err(), "`key` is not a string");
        assert_eq!(parse_key(r#"{"key":["a"]}"#).unwrap_err(), "`key` is not a string");
    }

    #[test]
    fn extracts_several_fields_in_the_asked_order() {
        let json = r#"{"blocks":[{"type":"markdown","text":"hi","id":"inner"}],"id":"run-1","thread":"pr-1"}"#;
        assert_eq!(parse_fields(json, ["thread", "id"]), Ok(["pr-1".to_string(), "run-1".to_string()]));
        // Nested members with the same name are not top-level fields.
        assert_eq!(parse_fields(r#"{"thread":"t","x":{"id":"a"}}"#, ["thread", "id"]).unwrap_err(), "no `id` field");
        assert_eq!(parse_fields(r#"{"id":"a"}"#, ["thread", "id"]).unwrap_err(), "no `thread` field");
        assert_eq!(parse_fields(r#"{"thread":"t","id":7}"#, ["thread", "id"]).unwrap_err(), "`id` is not a string");
        assert!(parse_fields(r#"{"thread":"t","id":"a""#, ["thread", "id"]).is_err());
    }

    #[test]
    fn rejects_a_non_object_document() {
        for bad in ["[]", "\"x\"", "1", "true", "null", "", "   "] {
            assert!(parse_key(bad).unwrap_err().contains("expected a JSON object"), "{bad:?}");
        }
    }

    #[test]
    fn rejects_bad_syntax() {
        for bad in [
            r#"{"key":"a""#,            // unterminated object
            r#"{"key":"a}"#,            // unterminated string
            r#"{key:"a"}"#,             // unquoted member name
            r#"{"key":'a'}"#,           // single quotes
            r#"{"key":"a",}"#,          // trailing comma
            r#"{"a":01,"key":"k"}"#,    // leading zero
            r#"{"a":+1,"key":"k"}"#,    // leading plus
            r#"{"a":.5,"key":"k"}"#,    // bare fraction
            r#"{"a":1.,"key":"k"}"#,    // trailing point
            r#"{"a":1e,"key":"k"}"#,    // empty exponent
            r#"{"a":0x10,"key":"k"}"#,  // hex
            r#"{"a":tru,"key":"k"}"#,   // truncated literal
            r#"{"a":[1,,2],"key":"k"}"#,
            r#"{"a":[1,2,],"key":"k"}"#,
            r#"{"key":"a\q"}"#,         // bad escape
            r#"{"key":"a\u00"}"#,       // short \u
            r#"{"key":"a\uZZZZ"}"#,     // non-hex \u
            r#"{"key":"\ud83d"}"#,      // lone high surrogate
            r#"{"key":"\udc00"}"#,      // lone low surrogate
            "{\"key\":\"a\nb\"}",       // raw control character
        ] {
            assert!(parse_key(bad).is_err(), "{bad:?} parsed");
        }
    }

    #[test]
    fn rejects_trailing_garbage() {
        assert!(parse_key(r#"{"key":"a"} {}"#).unwrap_err().contains("trailing data"));
        assert!(parse_key(r#"{"key":"a"}x"#).unwrap_err().contains("trailing data"));
        assert!(parse_key(r#"{"key":"a"}null"#).unwrap_err().contains("trailing data"));
        // Whitespace around the document is fine.
        assert_eq!(parse_key("\n\t {\"key\":\"a\"} \r\n"), Ok("a".into()));
    }

    #[test]
    fn caps_nesting_depth() {
        let nest = |n: usize| format!("{{\"key\":\"k\",\"a\":{}{}}}", "[".repeat(n), "]".repeat(n));
        assert!(parse_key(&nest(MAX_DEPTH - 1)).is_ok());
        let err = parse_key(&nest(MAX_DEPTH + 1)).unwrap_err();
        assert!(err.contains("nested deeper than"), "{err}");
        // Deep enough to overflow an unguarded recursive parser.
        assert!(parse_key(&nest(100_000)).is_err());
    }
}
