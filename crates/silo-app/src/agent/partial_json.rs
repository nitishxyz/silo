//! Tolerant reading of a tool call's JSON input while the model still streams it: what an incomplete object
//! prefix holds so far. Strings cut off mid-way read up to the cut (never half an escape or half a surrogate
//! pair); numbers and literals count only once complete; a key without its value yet is left out. So every value
//! read grows monotonically with the prefix: a later read only extends an earlier one. Pure.

use serde_json::{Map, Value};

/// What an input prefix holds so far.
#[derive(Clone, Debug, PartialEq)]
pub struct Partial {
    pub value: Value,
    /// The top-level key whose value is still being written (its string cut off, or its array/object open).
    pub open: Option<String>,
    /// The whole object arrived (its closing `}`).
    pub complete: bool,
}

// Field access for tests; the transcript reads `value` as a tool input.
#[cfg(test)]
impl Partial {
    /// Top-level string field `key` and whether it is still being written.
    #[must_use]
    pub fn field(&self, key: &str) -> Option<(&str, bool)> {
        self.value.get(key)?.as_str().map(|s| (s, self.open.as_deref() == Some(key)))
    }

    /// The `edits` array's `(old_string, new_string)` pairs as far as they arrived.
    #[must_use]
    pub fn edits(&self) -> Vec<(Option<&str>, Option<&str>)> {
        let Some(edits) = self.value.get("edits").and_then(Value::as_array) else { return Vec::new() };
        edits.iter().map(|e| (e.get("old_string").and_then(Value::as_str), e.get("new_string").and_then(Value::as_str))).collect()
    }
}

/// Read the JSON object `text` begins (`None` unless it starts with `{`, maybe after whitespace).
#[must_use]
pub fn parse(text: &str) -> Option<Partial> {
    let mut p = Parser { s: text.as_bytes(), text, i: 0, open: None, depth: 0 };
    p.ws();
    if p.peek()? != b'{' {
        return None;
    }
    let (value, complete) = p.value()?;
    Some(Partial { value, open: if complete { None } else { p.open }, complete })
}

struct Parser<'a> {
    s: &'a [u8],
    text: &'a str,
    i: usize,
    /// The top-level key being read.
    open: Option<String>,
    depth: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn ws(&mut self) {
        while self.peek().is_some_and(|b| b.is_ascii_whitespace()) {
            self.i += 1;
        }
    }

    /// A value and whether it ended; `None`: nothing of it is usable yet (or the input is malformed).
    fn value(&mut self) -> Option<(Value, bool)> {
        self.ws();
        match self.peek()? {
            b'{' => Some(self.object()),
            b'[' => Some(self.array()),
            b'"' => self.string().map(|(s, done)| (Value::String(s), done)),
            _ => self.scalar().map(|v| (v, true)),
        }
    }

    fn object(&mut self) -> (Value, bool) {
        self.i += 1;
        self.depth += 1;
        let mut map = Map::new();
        // A member may start here (first, or after a comma).
        let mut ready = true;
        let done = loop {
            self.ws();
            match self.peek() {
                Some(b'}') => {
                    self.i += 1;
                    break true;
                }
                Some(b',') if !ready => {
                    self.i += 1;
                    ready = true;
                }
                Some(b'"') if ready => {
                    ready = false;
                    let Some((key, true)) = self.string() else { break false };
                    self.ws();
                    if self.peek() != Some(b':') {
                        break false;
                    }
                    self.i += 1;
                    if self.depth == 1 {
                        self.open = Some(key.clone());
                    }
                    let Some((value, ended)) = self.value() else { break false };
                    map.insert(key, value);
                    if !ended {
                        break false;
                    }
                    if self.depth == 1 {
                        self.open = None;
                    }
                }
                _ => break false,
            }
        };
        self.depth -= 1;
        (Value::Object(map), done)
    }

    fn array(&mut self) -> (Value, bool) {
        self.i += 1;
        self.depth += 1;
        let mut items = Vec::new();
        let mut ready = true;
        let done = loop {
            self.ws();
            match self.peek() {
                Some(b']') => {
                    self.i += 1;
                    break true;
                }
                Some(b',') if !ready => {
                    self.i += 1;
                    ready = true;
                }
                Some(_) if ready => {
                    ready = false;
                    let Some((value, ended)) = self.value() else { break false };
                    items.push(value);
                    if !ended {
                        break false;
                    }
                }
                _ => break false,
            }
        };
        self.depth -= 1;
        (Value::Array(items), done)
    }

    /// A number or literal, once a delimiter shows it is complete.
    fn scalar(&mut self) -> Option<Value> {
        let start = self.i;
        while self.peek().is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'+' | b'.')) {
            self.i += 1;
        }
        self.peek()?;
        serde_json::from_str(&self.text[start..self.i]).ok()
    }
}

impl Parser<'_> {
    /// A string (at its `"`) and whether its closing quote arrived. A cut-off escape, `\u` or surrogate pair is
    /// left out until the rest of it arrives.
    fn string(&mut self) -> Option<(String, bool)> {
        self.i += 1;
        let mut out = String::new();
        loop {
            let run = self.s[self.i..].iter().position(|b| matches!(b, b'"' | b'\\')).unwrap_or(self.s.len() - self.i);
            // Both stops are ASCII, so the run ends on a char boundary.
            out.push_str(&self.text[self.i..self.i + run]);
            self.i += run;
            match self.peek() {
                None => return Some((out, false)),
                Some(b'"') => {
                    self.i += 1;
                    return Some((out, true));
                }
                _ => {}
            }
            let Some(escape) = self.s.get(self.i + 1).copied() else { return Some((out, false)) };
            let simple = match escape {
                b'n' => Some('\n'),
                b't' => Some('\t'),
                b'r' => Some('\r'),
                b'b' => Some('\u{8}'),
                b'f' => Some('\u{c}'),
                b'"' => Some('"'),
                b'\\' => Some('\\'),
                b'/' => Some('/'),
                b'u' => None,
                _ => return None,
            };
            if let Some(c) = simple {
                out.push(c);
                self.i += 2;
                continue;
            }
            let Some(unit) = self.hex(self.i + 2) else { return self.cut(self.i + 2, out) };
            if (0xD800..0xDC00).contains(&unit) {
                // A high surrogate: its low half follows as `\uXXXX`.
                let next = self.i + 6;
                match (self.s.get(next), self.s.get(next + 1)) {
                    (None, _) | (Some(b'\\'), None) => return Some((out, false)),
                    (Some(b'\\'), Some(b'u')) => match self.hex(next + 2) {
                        None => return self.cut(next + 2, out),
                        Some(low) if (0xDC00..0xE000).contains(&low) => {
                            out.push(char::from_u32(0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00)).unwrap_or('\u{fffd}'));
                            self.i = next + 6;
                            continue;
                        }
                        Some(_) => {}
                    },
                    _ => {}
                }
            }
            out.push(char::from_u32(unit).unwrap_or('\u{fffd}'));
            self.i += 6;
        }
    }

    /// No four hex digits at `at`: the text ran out inside a `\u` escape (keep what came before it), or the
    /// escape is malformed (stop).
    fn cut(&self, at: usize, out: String) -> Option<(String, bool)> {
        let rest = &self.s[at.min(self.s.len())..];
        (rest.len() < 4 && rest.iter().all(u8::is_ascii_hexdigit)).then_some((out, false))
    }

    /// Four hex digits at `at`; `None` if they did not all arrive (or are not hex).
    fn hex(&self, at: usize) -> Option<u32> {
        let digits = self.text.get(at..at + 4)?;
        u32::from_str_radix(digits, 16).ok().filter(|_| digits.bytes().all(|b| b.is_ascii_hexdigit()))
    }
}

#[cfg(test)]
mod tests;
