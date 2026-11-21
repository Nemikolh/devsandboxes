//! Masking the injected value wherever it comes back (an echoing endpoint,
//! an error quoting the request): an exact-match pass over response
//! headers ([`Secrets::mask`]) and the body as it streams ([`Redactor`]),
//! for every value the caller lists (the credential and its basic-auth
//! forms, [`basic_auth`]). The mask is the same length as the match, so a
//! passed-through `Content-Length` or chunk size still holds.
//!
//! A stream holds back only a tail that is a proper prefix of some value,
//! never a whole chunk: an SSE event goes out as it arrives unless it ends
//! in what could be the start of a secret.

use std::sync::Arc;

pub const MASK: u8 = b'*';

/// Cheap to clone, one per response stream.
#[derive(Debug, Clone)]
pub struct Secrets {
    values: Arc<[Vec<u8>]>,
    longest: usize,
}

impl Secrets {
    /// Empty values are dropped (they would match everywhere).
    pub fn new(values: impl IntoIterator<Item = Vec<u8>>) -> Secrets {
        let mut values: Vec<Vec<u8>> = values.into_iter().filter(|v| !v.is_empty()).collect();
        values.sort();
        values.dedup();
        let longest = values.iter().map(Vec::len).max().unwrap_or(0);
        Secrets {
            values: values.into(),
            longest,
        }
    }

    /// In place; whether there was one.
    pub fn mask(&self, buf: &mut [u8]) -> bool {
        let mut marks = vec![false; buf.len()];
        self.mark(buf, &mut marks);
        apply(buf, &marks)
    }

    pub fn stream(&self) -> Redactor {
        Redactor {
            secrets: self.clone(),
            held: Vec::new(),
            held_marks: Vec::new(),
        }
    }

    fn mark(&self, buf: &[u8], marks: &mut [bool]) {
        for v in self.values.iter() {
            for (i, w) in buf.windows(v.len()).enumerate() {
                if w == v.as_slice() {
                    marks[i..i + v.len()].fill(true);
                }
            }
        }
    }

    /// All a later chunk could complete into a match.
    fn pending(&self, buf: &[u8]) -> usize {
        let max = self.longest.saturating_sub(1).min(buf.len());
        (1..=max)
            .rev()
            .find(|&l| {
                let tail = &buf[buf.len() - l..];
                self.values.iter().any(|v| v.len() > l && v.starts_with(tail))
            })
            .unwrap_or(0)
    }
}

fn apply(buf: &mut [u8], marks: &[bool]) -> bool {
    let mut any = false;
    for (b, &m) in buf.iter_mut().zip(marks) {
        if m {
            *b = MASK;
            any = true;
        }
    }
    any
}

/// [`Redactor::feed`] each chunk as it arrives and send what it returns;
/// [`Redactor::finish`] at the end of the body.
pub struct Redactor {
    secrets: Secrets,
    /// With the marks already decided for them (a match that ended inside them).
    held: Vec<u8>,
    held_marks: Vec<bool>,
}

impl Redactor {
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut buf = std::mem::take(&mut self.held);
        buf.extend_from_slice(chunk);
        let mut marks = std::mem::take(&mut self.held_marks);
        marks.resize(buf.len(), false);
        self.secrets.mark(&buf, &mut marks);
        let cut = buf.len() - self.secrets.pending(&buf);
        self.held = buf.split_off(cut);
        self.held_marks = marks.split_off(cut);
        apply(&mut buf, &marks);
        buf
    }

    pub fn finish(mut self) -> Vec<u8> {
        apply(&mut self.held, &self.held_marks);
        self.held
    }
}

/// The form a basic-auth header carries the secret in.
pub fn basic_auth(user: &[u8], secret: &[u8]) -> Vec<u8> {
    let mut plain = user.to_vec();
    plain.push(b':');
    plain.extend_from_slice(secret);
    base64(&plain)
}

/// Standard alphabet, padded; `None` for anything else (whitespace
/// included).
pub fn base64_decode(text: &[u8]) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(4) {
        return None;
    }
    let value = |b: u8| match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let groups = text.len() / 4;
    for (i, group) in text.chunks(4).enumerate() {
        let pad = group.iter().rev().take_while(|&&b| b == b'=').count();
        if pad > 2 || (pad > 0 && i + 1 != groups) {
            return None;
        }
        let mut n = 0u32;
        for (j, &b) in group.iter().enumerate() {
            let v = if j >= 4 - pad { 0 } else { value(b)? };
            n |= u32::from(v) << (18 - 6 * j);
        }
        out.extend_from_slice(&n.to_be_bytes()[1..4 - pad]);
    }
    Some(out)
}

fn base64(data: &[u8]) -> Vec<u8> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(data.len().div_ceil(3) * 4);
    for group in data.chunks(3) {
        let n = group.iter().enumerate().fold(0u32, |n, (i, &b)| n | (u32::from(b) << (16 - 8 * i)));
        for i in 0..4 {
            out.push(if i <= group.len() {
                ALPHABET[((n >> (18 - 6 * i)) & 0x3f) as usize]
            } else {
                b'='
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secrets(values: &[&str]) -> Secrets {
        Secrets::new(values.iter().map(|v| v.as_bytes().to_vec()))
    }

    fn masked(s: &Secrets, input: &[u8]) -> Vec<u8> {
        let mut out = input.to_vec();
        s.mask(&mut out);
        out
    }

    fn streamed(s: &Secrets, chunks: &[&[u8]]) -> Vec<u8> {
        let mut r = s.stream();
        let mut out: Vec<u8> = chunks.iter().flat_map(|c| r.feed(c)).collect();
        out.extend(r.finish());
        out
    }

    #[test]
    fn base64_vectors() {
        for (plain, enc) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(plain.as_bytes()), enc.as_bytes(), "{plain}");
            assert_eq!(base64_decode(enc.as_bytes()).as_deref(), Some(plain.as_bytes()), "{enc}");
        }
        for bad in ["Zg", "Zg=", "Z===", "Zg==Zg==", "Zm9 ", "Zm9-"] {
            assert_eq!(base64_decode(bad.as_bytes()), None, "{bad}");
        }
        assert_eq!(base64(&[0xff, 0xfe, 0x00]), b"//4A");
        assert_eq!(basic_auth(b"user", b"pass"), b"dXNlcjpwYXNz");
    }

    #[test]
    fn header_value_masked() {
        let s = secrets(&["ghp_SECRET"]);
        let mut v = b"token ghp_SECRET, again ghp_SECRET".to_vec();
        assert!(s.mask(&mut v));
        assert_eq!(v, b"token **********, again **********");
        let mut v = b"ghp_SECRE".to_vec();
        assert!(!s.mask(&mut v));
    }

    #[test]
    fn empty_and_no_values() {
        let s = Secrets::new([Vec::new()]);
        let mut r = s.stream();
        assert_eq!(r.feed(b"abc"), b"abc");
        assert!(r.finish().is_empty());
    }

    #[test]
    fn holds_only_a_possible_prefix() {
        let s = secrets(&["sk-12345"]);
        let mut r = s.stream();
        // An SSE event with nothing secret-like at its end goes out whole.
        assert_eq!(r.feed(b"data: {\"x\":1}\n\n"), b"data: {\"x\":1}\n\n");
        assert_eq!(r.feed(b"data: sk-1"), b"data: ");
        assert_eq!(r.feed(b"23"), b"");
        assert_eq!(r.feed(b"45 ok"), b"******** ok");
        assert_eq!(r.feed(b"data: sk-9"), b"data: sk-9");
        assert_eq!(r.feed(b" s"), b" ");
        assert_eq!(r.finish(), b"s");
    }

    #[test]
    fn value_split_at_every_offset() {
        let s = secrets(&["secretvalue"]);
        let input = b"before secretvalue mid secretvalue after";
        let want = b"before *********** mid *********** after";
        assert_eq!(masked(&s, input), want);
        for i in 0..=input.len() {
            for j in i..=input.len() {
                let got = streamed(&s, &[&input[..i], &input[i..j], &input[j..]]);
                assert_eq!(got, want, "split at {i},{j}");
            }
        }
        let bytes: Vec<&[u8]> = input.chunks(1).collect();
        assert_eq!(streamed(&s, &bytes), want);
    }

    #[test]
    fn overlapping_occurrences_and_prefixes() {
        // Self-overlapping value: both occurrences masked.
        let s = secrets(&["abab"]);
        assert_eq!(masked(&s, b"xababab"), b"x******");
        // A false start that is itself a prefix: "aab" inside "aaab".
        let s = secrets(&["aab"]);
        let input = b"aaaab aab";
        let want = b"aa*** ***";
        for i in 0..=input.len() {
            assert_eq!(streamed(&s, &[&input[..i], &input[i..]]), want, "split at {i}");
        }
    }

    #[test]
    fn multiple_values() {
        let token = b"ghs_abcdef";
        let basic = basic_auth(b"x-access-token", token);
        let s = Secrets::new([token.to_vec(), basic.clone(), b"ghs_abc".to_vec()]);
        let mut input = b"t=ghs_abcdef b=Basic ".to_vec();
        input.extend_from_slice(&basic);
        input.extend_from_slice(b" short=ghs_abc. end");
        let mut want = b"t=********** b=Basic ".to_vec();
        want.extend(std::iter::repeat_n(MASK, basic.len()));
        want.extend_from_slice(b" short=*******. end");
        assert_eq!(masked(&s, &input), want);
        for i in 0..=input.len() {
            for j in (i..=input.len()).step_by(3) {
                assert_eq!(streamed(&s, &[&input[..i], &input[i..j], &input[j..]]), want, "split at {i},{j}");
            }
        }
    }

    #[test]
    fn streaming_matches_one_shot() {
        // A tiny alphabet makes overlaps and false starts frequent.
        let s = secrets(&["abca", "bcab", "aa", "cabcabc"]);
        let mut seed = 0x2545_f491_u32;
        let mut next = |m: u32| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed % m
        };
        for _ in 0..500 {
            let len = next(40) as usize;
            let input: Vec<u8> = (0..len).map(|_| b"abc"[next(3) as usize]).collect();
            let mut chunks = Vec::new();
            let mut at = 0;
            while at < input.len() {
                let n = (1 + next(6) as usize).min(input.len() - at);
                chunks.push(&input[at..at + n]);
                at += n;
            }
            assert_eq!(streamed(&s, &chunks), masked(&s, &input), "{:?}", String::from_utf8_lossy(&input));
        }
    }
}
