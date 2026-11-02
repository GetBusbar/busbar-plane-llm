// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Usage recovery from a response whose transport ended early. The relay retains the original
//! prefix while it is within its cap, so root-member custody is provable even if the root never
//! closes. A headless tail cannot prove whether a `usage` key belongs to the response or a tool's
//! payload and is never a billing source. Over-cap relays already report no usage.

/// Skip only JSON whitespace.
fn whitespace(buf: &[u8], mut at: usize) -> usize {
    while buf
        .get(at)
        .is_some_and(|b| matches!(*b, b' ' | b'\t' | b'\r' | b'\n'))
    {
        at += 1;
    }
    at
}

/// Locate one complete value lexically. The caller validates that exact span as JSON before
/// advancing, so matching delimiters alone never authenticate a malformed prefix.
fn complete_value_end(buf: &[u8], start: usize) -> Option<usize> {
    let first = *buf.get(start)?;
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escape = false;
    let container = matches!(first, b'{' | b'[');
    if !container && first != b'"' {
        let end = buf[start..]
            .iter()
            .position(|b| matches!(*b, b',' | b'}' | b' ' | b'\t' | b'\r' | b'\n'))
            .map_or(buf.len(), |n| start + n);
        return (end > start).then_some(end);
    }
    for i in start..buf.len() {
        let c = buf[i];
        if in_str {
            if escape {
                escape = false;
            } else if c == b'\\' {
                escape = true;
            } else if c == b'"' {
                in_str = false;
                if !container {
                    return Some(i + 1);
                }
            }
        } else {
            match c {
                b'"' => in_str = true,
                b'{' | b'[' => depth += 1,
                b'}' | b']' => {
                    depth = depth.checked_sub(1)?;
                    if depth == 0 {
                        return Some(i + 1);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Recover a complete object-valued root `key` from the original response prefix. Every earlier
/// root value is structurally located and validated before advancing, so nested tool objects,
/// strings and a headless tail cannot impersonate usage. An incomplete later member does not
/// erase a report already received; a complete duplicate root member follows JSON's last-wins rule.
/// `key` includes JSON quotes, as existing dialect callers spell it.
pub fn isolate_tail_usage_object(tail: &[u8], key: &[u8]) -> Option<serde_json::Value> {
    let expected: String = serde_json::from_slice(key).ok()?;
    let mut at = whitespace(tail, 0);
    if tail.get(at) != Some(&b'{') {
        return None;
    }
    at += 1;
    let mut found = None;
    loop {
        at = whitespace(tail, at);
        if tail.get(at) != Some(&b'"') {
            return found;
        }
        let Some(end) = complete_value_end(tail, at) else {
            return found;
        };
        let Ok(name) = serde_json::from_slice::<String>(&tail[at..end]) else {
            return found;
        };
        at = whitespace(tail, end);
        if tail.get(at) != Some(&b':') {
            return found;
        }
        at = whitespace(tail, at + 1);
        let Some(end) = complete_value_end(tail, at) else {
            return if name == expected { None } else { found };
        };
        if serde_json::from_slice::<serde::de::IgnoredAny>(&tail[at..end]).is_err() {
            return if name == expected { None } else { found };
        }
        if name == expected {
            found = crate::codec::json::parse::<serde_json::Value>(&tail[at..end])
                .ok()
                .filter(serde_json::Value::is_object);
        }
        at = whitespace(tail, end);
        if tail.get(at) != Some(&b',') {
            return found;
        }
        at += 1;
    }
}

#[cfg(test)]
#[path = "tests/usage_tail_root_tests.rs"]
mod root_tests;

/// The longest string value [`StopKeyScanner`] reads. A stop-reason token is a short enum member
/// (`ERROR`, `MALFORMED_FUNCTION_CALL`); a longer value is not one, and reading it would cost work
/// proportional to the body.
const MAX_TOKEN_BYTES: usize = 64;

/// The longest stop-reason key (quotes included) [`StopKeyScanner`] looks for; the bound lets it
/// carry its state inline.
pub const MAX_STOP_KEY_BYTES: usize = 32;

/// The deepest container nesting [`StopKeyScanner`] tracks kinds of. A stop field sits a few levels
/// deep; a deeper body is still scanned but its deeper containers are never the stop field's.
const MAX_TRACKED_DEPTH: u16 = 64;

/// A dialect's non-stream stop-reason field, located structurally: member `key` (quotes included,
/// e.g. `b"\"finish_reason\""`) of an object at container nesting `depth`, counting every `{` and
/// `[` from the root (`1` is top-level; a Gemini candidate's member `{"candidates":[{…}]}` is `3`).
/// The same name deeper, e.g. inside a tool call's `args`, is content, never the stop reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StopKey {
    pub key: &'static [u8],
    pub depth: u16,
}

/// Where [`StopKeyScanner`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScanState {
    /// Walking the document for the first member named `key` at `depth`.
    Seeking,
    /// Just past the `:`: whitespace, then the opening `"`.
    BeforeQuote,
    /// Inside the value, collecting the token.
    InToken,
    /// The token is complete.
    Found,
    /// The first such member's value is not a plain token: no answer, and none will come.
    Gone,
}

/// The string value of the first member [`StopKey`] names, read incrementally as a body streams
/// through a relay: it walks the JSON lexically (strings, escapes, nesting) without parsing or
/// copying the body. State is bounded and inline (no heap): depth, a bit per tracked container, the
/// key match position and at most [`MAX_TOKEN_BYTES`] of token. A same-named member at another depth
/// (a tool's `args` holding `"finishReason":"MALFORMED_FUNCTION_CALL"`) or the name inside a string
/// value never matches.
#[derive(Clone, Debug)]
pub struct StopKeyScanner {
    /// The key's name, quotes stripped.
    name: &'static [u8],
    depth: u16,
    state: ScanState,
    /// The current container nesting.
    level: u16,
    /// Bit `n` set: the container at nesting `n + 1` is an object.
    objects: u64,
    in_string: bool,
    escaped: bool,
    /// The string being walked is a candidate member name at the key's depth.
    candidate: bool,
    /// How many bytes of `name` the candidate has matched.
    matched: usize,
    /// A candidate that matched `name` whole just closed: the next non-whitespace byte says whether
    /// it was a member name (`:`) or a value.
    closed_match: bool,
    token: [u8; MAX_TOKEN_BYTES],
    token_len: usize,
}

impl StopKeyScanner {
    /// A scanner for `key`. `None` for an empty or unquoted key, one longer than
    /// [`MAX_STOP_KEY_BYTES`], or a depth of `0`.
    pub fn new(key: StopKey) -> Option<Self> {
        let k = key.key;
        let well_formed = k.len() > 2
            && k.len() <= MAX_STOP_KEY_BYTES
            && k[0] == b'"'
            && k[k.len() - 1] == b'"'
            && key.depth > 0;
        well_formed.then(|| Self {
            name: &k[1..k.len() - 1],
            depth: key.depth,
            state: ScanState::Seeking,
            level: 0,
            objects: 0,
            in_string: false,
            escaped: false,
            candidate: false,
            matched: 0,
            closed_match: false,
            token: [0; MAX_TOKEN_BYTES],
            token_len: 0,
        })
    }

    /// Whether the innermost open container is an object.
    fn in_object(&self) -> bool {
        self.level > 0
            && self.level <= MAX_TRACKED_DEPTH
            && self.objects & (1u64 << (self.level - 1)) != 0
    }

    /// Feed the next chunk of the body.
    pub fn feed(&mut self, chunk: &[u8]) {
        for &c in chunk {
            match self.state {
                ScanState::Found | ScanState::Gone => return,
                ScanState::Seeking => self.seek(c),
                ScanState::BeforeQuote if c.is_ascii_whitespace() => {}
                ScanState::BeforeQuote if c == b'"' => self.state = ScanState::InToken,
                ScanState::BeforeQuote => self.state = ScanState::Gone,
                ScanState::InToken if c == b'"' => self.state = ScanState::Found,
                ScanState::InToken if c == b'\\' || self.token_len == MAX_TOKEN_BYTES => {
                    self.state = ScanState::Gone;
                }
                ScanState::InToken => {
                    self.token[self.token_len] = c;
                    self.token_len += 1;
                }
            }
        }
    }

    /// One byte of the document walk while seeking.
    fn seek(&mut self, c: u8) {
        if self.in_string {
            if self.escaped {
                self.escaped = false;
                self.candidate = false;
            } else if c == b'\\' {
                self.escaped = true;
                self.candidate = false;
            } else if c == b'"' {
                self.in_string = false;
                self.closed_match = self.candidate && self.matched == self.name.len();
                self.candidate = false;
            } else if self.candidate {
                if self.name.get(self.matched) == Some(&c) {
                    self.matched += 1;
                } else {
                    self.candidate = false;
                }
            }
            return;
        }
        if self.closed_match {
            if c.is_ascii_whitespace() {
                return;
            }
            self.closed_match = false;
            if c == b':' {
                // The member this scanner looks for: its value decides the answer.
                self.state = ScanState::BeforeQuote;
                return;
            }
        }
        match c {
            b'"' => {
                self.in_string = true;
                self.candidate = self.level == self.depth && self.in_object();
                self.matched = 0;
            }
            b'{' | b'[' => {
                self.level = self.level.saturating_add(1);
                if self.level <= MAX_TRACKED_DEPTH {
                    let bit = 1u64 << (self.level - 1);
                    if c == b'{' {
                        self.objects |= bit;
                    } else {
                        self.objects &= !bit;
                    }
                }
            }
            b'}' | b']' => self.level = self.level.saturating_sub(1),
            _ => {}
        }
    }

    /// The token, once the body has supplied all of it; `None` otherwise.
    pub fn token(&self) -> Option<&str> {
        (self.state == ScanState::Found)
            .then(|| std::str::from_utf8(&self.token[..self.token_len]).ok())
            .flatten()
    }
}
