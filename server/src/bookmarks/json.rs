//! Order-preserving JSON subset for the legacy bookmarks/timeline payloads.
//!
//! The Node reference stores mutation payloads as
//! `JSON.stringify(JSON.parse(text))` and the T03 golden fixtures pin the
//! resulting member order. `serde_json::Value` keeps object members in a
//! `BTreeMap` by default (sorted keys), so it cannot reproduce that order
//! without the `preserve_order` feature (the Cargo manifest is owned by T15).
//!
//! [`Json`] parses into an insertion-ordered tree and renders it compactly:
//! object members keep the input order (a repeated key is replaced in place,
//! like JavaScript), strings are re-escaped with `serde_json`, and numbers keep
//! their original token. That matches `JSON.parse`/`JSON.stringify` for the
//! shapes the legacy API exchanges.
//!
//! This file is the single source of the model. The bookmarks repository
//! declares it as `json`; the timeline repository includes the same file with
//! `#[path = "../bookmarks/json.rs"]` because the crate root (where a normal
//! `mod` declaration would live) is outside the T08 write scope.

#![allow(dead_code)] // small library-like API shared by two repositories/handlers

/// Maximum nesting accepted by [`Json::parse`] (defence against hostile input).
const MAX_DEPTH: usize = 128;

/// A JSON value that remembers object member order.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    /// `null`.
    Null,
    /// `true` / `false`.
    Bool(bool),
    /// A number, kept as its validated source token (no float round-tripping).
    Number(String),
    /// A decoded string.
    String(String),
    /// An ordered array.
    Array(Vec<Json>),
    /// An ordered object; duplicate keys collapse to the first position.
    Object(Vec<(String, Json)>),
}

impl Json {
    /// Parses one complete JSON document; `None` for malformed or trailing input.
    pub fn parse(input: &str) -> Option<Self> {
        Parser::new(input.as_bytes()).parse_document()
    }

    /// Object member lookup (`None` for non-objects and missing keys).
    pub fn get(&self, key: &str) -> Option<&Self> {
        match self {
            Json::Object(entries) => entries
                .iter()
                .find(|(existing, _)| existing == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    /// The string payload, when this value is a JSON string.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(value) => Some(value),
            _ => None,
        }
    }

    /// JavaScript `Boolean(value)` semantics for the shapes the legacy API sees.
    ///
    /// `NaN` does not exist in JSON; every finite number other than `0` is
    /// truthy, every non-empty string is truthy, arrays/objects are truthy.
    pub fn is_truthy(&self) -> bool {
        match self {
            Json::Null => false,
            Json::Bool(value) => *value,
            Json::Number(raw) => raw
                .parse::<f64>()
                .map(|value| value != 0.0)
                .unwrap_or(false),
            Json::String(value) => !value.is_empty(),
            Json::Array(_) | Json::Object(_) => true,
        }
    }

    /// `parseInt(value)` for a JSON number or string.
    ///
    /// Strings are parsed as an optional sign plus a decimal digit prefix (the
    /// `parseInt` contract); a number is truncated toward zero. Anything else
    /// (`null`, booleans, arrays, objects, sign without digits) is `NaN`.
    pub fn js_parse_int(&self) -> Option<i64> {
        match self {
            Json::Number(raw) => {
                if raw.contains('.') || raw.contains('e') || raw.contains('E') {
                    raw.parse::<f64>()
                        .ok()
                        .filter(|value| value.is_finite())
                        .map(|value| value.trunc() as i64)
                } else {
                    raw.parse::<i64>().ok()
                }
            }
            Json::String(value) => parse_int_str(value),
            _ => None,
        }
    }

    /// Renders compact JSON (`JSON.stringify` style, no insignificant spaces).
    pub fn to_compact_string(&self) -> String {
        let mut out = String::new();
        self.write_to(&mut out);
        out
    }

    fn write_to(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Number(raw) => out.push_str(raw),
            Json::String(value) => write_escaped(value, out),
            Json::Array(items) => {
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    item.write_to(out);
                }
                out.push(']');
            }
            Json::Object(entries) => {
                out.push('{');
                for (index, (key, value)) in entries.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    write_escaped(key, out);
                    out.push(':');
                    value.write_to(out);
                }
                out.push('}');
            }
        }
    }
}

/// Inserts/replaces an object member, preserving the position of an existing key
/// (JavaScript assignment semantics; used for `obj.hash = hash`).
pub fn insert_member(entries: &mut Vec<(String, Json)>, key: String, value: Json) {
    if let Some(slot) = entries.iter_mut().find(|(existing, _)| *existing == key) {
        slot.1 = value;
    } else {
        entries.push((key, value));
    }
}

/// `parseInt(text)` after the JavaScript string coercion.
pub fn parse_int_str(text: &str) -> Option<i64> {
    let trimmed = text.trim_start();
    let bytes = trimmed.as_bytes();
    let mut index = 0;
    let mut negative = false;
    if let Some(sign @ (b'+' | b'-')) = bytes.first().copied() {
        negative = sign == b'-';
        index = 1;
    }
    let digits_start = index;
    while matches!(bytes.get(index), Some(b'0'..=b'9')) {
        index += 1;
    }
    if index == digits_start {
        return None;
    }
    let magnitude = trimmed[digits_start..index].parse::<i64>().ok()?;
    Some(if negative { -magnitude } else { magnitude })
}

fn write_escaped(value: &str, out: &mut String) {
    let escaped = serde_json::to_string(value).expect("strings are always serialisable");
    out.push_str(&escaped);
}

struct Parser<'a> {
    input: &'a [u8],
    pos: usize,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self {
            input,
            pos: 0,
            depth: 0,
        }
    }

    fn parse_document(mut self) -> Option<Json> {
        self.skip_whitespace();
        let value = self.value()?;
        self.skip_whitespace();
        (self.pos == self.input.len()).then_some(value)
    }

    fn value(&mut self) -> Option<Json> {
        if self.depth >= MAX_DEPTH {
            return None;
        }
        self.depth += 1;
        let value = self.value_inner();
        self.depth -= 1;
        value
    }

    fn value_inner(&mut self) -> Option<Json> {
        match self.peek()? {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => self.string().map(Json::String),
            b't' => self.literal(b"true", Json::Bool(true)),
            b'f' => self.literal(b"false", Json::Bool(false)),
            b'n' => self.literal(b"null", Json::Null),
            b'-' | b'0'..=b'9' => self.number(),
            _ => None,
        }
    }

    fn literal(&mut self, token: &[u8], value: Json) -> Option<Json> {
        if self.input.get(self.pos..self.pos + token.len())? == token {
            self.pos += token.len();
            Some(value)
        } else {
            None
        }
    }

    fn number(&mut self) -> Option<Json> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek()? {
            b'0' => self.pos += 1,
            b'1'..=b'9' => {
                self.pos += 1;
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return None,
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return None;
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return None;
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let raw = std::str::from_utf8(&self.input[start..self.pos]).ok()?;
        Some(Json::Number(raw.to_owned()))
    }

    fn string(&mut self) -> Option<String> {
        let start = self.pos;
        self.pos += 1;
        loop {
            match self.peek()? {
                b'"' => {
                    self.pos += 1;
                    break;
                }
                b'\\' => self.pos += 2,
                _ => self.pos += 1,
            }
        }
        serde_json::from_slice::<String>(&self.input[start..self.pos]).ok()
    }

    fn object(&mut self) -> Option<Json> {
        self.pos += 1; // '{'
        let mut entries: Vec<(String, Json)> = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Some(Json::Object(entries));
        }
        loop {
            self.skip_whitespace();
            if self.peek() != Some(b'"') {
                return None;
            }
            let key = self.string()?;
            self.skip_whitespace();
            if self.peek() != Some(b':') {
                return None;
            }
            self.pos += 1;
            self.skip_whitespace();
            let value = self.value()?;
            insert_member(&mut entries, key, value);
            self.skip_whitespace();
            match self.peek()? {
                b',' => self.pos += 1,
                b'}' => {
                    self.pos += 1;
                    return Some(Json::Object(entries));
                }
                _ => return None,
            }
        }
    }

    fn array(&mut self) -> Option<Json> {
        self.pos += 1; // '['
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Some(Json::Array(items));
        }
        loop {
            self.skip_whitespace();
            items.push(self.value()?);
            self.skip_whitespace();
            match self.peek()? {
                b',' => self.pos += 1,
                b']' => {
                    self.pos += 1;
                    return Some(Json::Array(items));
                }
                _ => return None,
            }
        }
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_member_order_is_preserved_and_duplicates_replace_in_place() {
        let value = Json::parse(r#"{"z":1,"a":{"b":2,"c":[3,{"d":4}]},"z":5}"#).expect("json");
        assert_eq!(
            value.to_compact_string(),
            r#"{"z":5,"a":{"b":2,"c":[3,{"d":4}]}}"#
        );
        assert_eq!(
            value
                .get("a")
                .and_then(|a| a.get("b"))
                .and_then(Json::as_str),
            None
        );
        assert_eq!(value.get("z").and_then(Json::js_parse_int), Some(5));
    }

    #[test]
    fn strings_round_trip_like_json_stringify() {
        let value = Json::parse(r#"{"title":"Кіно \"quoted\"\n"}"#).expect("json");
        assert_eq!(
            value.to_compact_string(),
            "{\"title\":\"Кіно \\\"quoted\\\"\\n\"}"
        );
    }

    #[test]
    fn numbers_keep_their_source_token() {
        let value = Json::parse("[6000,1.5,-0.25,1e3,10]").expect("json");
        assert_eq!(value.to_compact_string(), "[6000,1.5,-0.25,1e3,10]");
    }

    #[test]
    fn malformed_documents_are_rejected() {
        for input in [
            "",
            " ",
            "{",
            "{\"a\":}",
            "[1,]",
            "{\"a\":1,}",
            "'single'",
            "{\"a\":01}",
            "{\"a\":1} trailing",
            "nul",
        ] {
            assert!(Json::parse(input).is_none(), "{input:?} must be rejected");
        }
    }

    #[test]
    fn parse_int_matches_the_javascript_prefix_contract() {
        assert_eq!(parse_int_str(" 9001"), Some(9001));
        assert_eq!(parse_int_str("9001abc"), Some(9001));
        assert_eq!(parse_int_str("-12.9"), Some(-12));
        assert_eq!(parse_int_str("+7"), Some(7));
        assert_eq!(parse_int_str("abc"), None);
        assert_eq!(parse_int_str(""), None);
        assert_eq!(parse_int_str("-"), None);

        assert_eq!(Json::Number("9001".to_owned()).js_parse_int(), Some(9001));
        assert_eq!(Json::Number("9001.9".to_owned()).js_parse_int(), Some(9001));
        assert_eq!(Json::Number("-0.5".to_owned()).js_parse_int(), Some(0));
        assert_eq!(Json::String("1004".to_owned()).js_parse_int(), Some(1004));
        assert_eq!(Json::Null.js_parse_int(), None);
        assert_eq!(Json::Bool(true).js_parse_int(), None);
    }

    #[test]
    fn truthiness_follows_javascript() {
        assert!(!Json::Null.is_truthy());
        assert!(!Json::Bool(false).is_truthy());
        assert!(!Json::Number("0".to_owned()).is_truthy());
        assert!(!Json::Number("-0.0".to_owned()).is_truthy());
        assert!(!Json::String(String::new()).is_truthy());
        assert!(Json::Number("80".to_owned()).is_truthy());
        assert!(Json::String("0".to_owned()).is_truthy());
        assert!(Json::Array(Vec::new()).is_truthy());
        assert!(Json::Object(Vec::new()).is_truthy());
    }

    #[test]
    fn insert_member_keeps_the_existing_position() {
        let mut entries = vec![
            ("hash".to_owned(), Json::String("old".to_owned())),
            ("percent".to_owned(), Json::Number("10".to_owned())),
        ];
        insert_member(
            &mut entries,
            "hash".to_owned(),
            Json::String("new".to_owned()),
        );
        assert_eq!(
            Json::Object(entries).to_compact_string(),
            r#"{"hash":"new","percent":10}"#
        );
    }

    #[test]
    fn deep_nesting_is_rejected_without_panicking() {
        let deep = format!(
            "{}1{}",
            "[".repeat(MAX_DEPTH + 1),
            "]".repeat(MAX_DEPTH + 1)
        );
        assert!(Json::parse(&deep).is_none());
    }
}
