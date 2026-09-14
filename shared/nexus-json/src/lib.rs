//! JSON, read and written.
//!
//! This tree has no third-party dependencies and this file is the reason there
//! is not one now. Every parser here was written the same way — the settings
//! file, the DNS message, the HTML page, the shell's words — and a JSON reader
//! is a smaller job than any of them.
//!
//! # Objects keep their order
//!
//! An object is a list of pairs and not a map. That is a deliberate choice with
//! one reason behind it: these files live in a repository and are read as
//! diffs. A map would write its keys in whatever order it happened to store
//! them, and a state file that shuffles its own keys between two writes is a
//! file whose every diff is noise.
//!
//! Lookup is therefore linear. The objects this reads have tens of keys, not
//! thousands, and a linear scan over thirty pairs is faster than hashing one.
//!
//! # Numbers
//!
//! Kept as `f64`, which is what JSON says a number is, with `as_i64` for the
//! ones that are counts and timestamps — which is all of them here. A number
//! that is not exactly an integer is reported as not being one rather than
//! quietly truncated.
//!
//! # What it refuses
//!
//! Depth beyond [`MAX_DEPTH`], because a thousand open brackets is a stack
//! overflow and not a document. Trailing content after the value, because
//! `{} garbage` is two things and one of them was not asked for. Everything
//! else it refuses, it refuses with the position, since a state file that will
//! not parse is a thing somebody has to find in an editor.

#![deny(unsafe_op_in_unsafe_fn)]

use std::fmt::Write as _;

/// How deeply values may nest.
///
/// The parser recurses, so this is what stands between a malformed file and a
/// stack overflow. These documents nest four or five deep.
pub const MAX_DEPTH: usize = 64;

/// A JSON value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Value>),
    /// Pairs in the order they were written, and written back in that order.
    Object(Vec<(String, Value)>),
}

impl Value {
    /// An empty object.
    #[must_use]
    pub fn object() -> Self {
        Self::Object(Vec::new())
    }

    /// An empty array.
    #[must_use]
    pub fn array() -> Self {
        Self::Array(Vec::new())
    }

    /// A string value, from anything that can become one.
    #[must_use]
    pub fn string(text: impl Into<String>) -> Self {
        Self::String(text.into())
    }

    /// A number, from an integer.
    #[must_use]
    pub fn number(value: i64) -> Self {
        Self::Number(value as f64)
    }

    // -- reading -------------------------------------------------------------

    /// The value at a key, if this is an object that has one.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Self::Object(pairs) => pairs.iter().find(|(name, _)| name == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// The same, to change.
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        match self {
            Self::Object(pairs) => pairs
                .iter_mut()
                .find(|(name, _)| name == key)
                .map(|(_, v)| v),
            _ => None,
        }
    }

    /// Walk a path of keys: `value.at(["build", "status"])`.
    ///
    /// Here because reading five levels into a state file is what this crate is
    /// for, and `get().and_then(get).and_then(get)` is four lines saying one
    /// thing.
    #[must_use]
    pub fn at<'a>(&self, path: impl IntoIterator<Item = &'a str>) -> Option<&Value> {
        let mut here = self;
        for key in path {
            here = here.get(key)?;
        }
        Some(here)
    }

    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(text) => Some(text),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }

    /// The number, if it is one and it is a whole one.
    #[must_use]
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Number(value) if value.fract() == 0.0 && value.is_finite() => Some(*value as i64),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Number(value) => Some(*value),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Self::Array(items) => Some(items),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_object(&self) -> Option<&[(String, Value)]> {
        match self {
            Self::Object(pairs) => Some(pairs),
            _ => None,
        }
    }

    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// What kind of thing this is, for a message about it not being the kind
    /// that was wanted.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Bool(_) => "a boolean",
            Self::Number(_) => "a number",
            Self::String(_) => "a string",
            Self::Array(_) => "an array",
            Self::Object(_) => "an object",
        }
    }

    // -- writing -------------------------------------------------------------

    /// Set a key, keeping its position if it is already there.
    ///
    /// Keeping the position matters for the same reason the order does: a
    /// change to one field should be a one-line diff and not a reordering.
    pub fn set(&mut self, key: &str, value: Value) {
        if let Self::Object(pairs) = self {
            if let Some(slot) = pairs.iter_mut().find(|(name, _)| name == key) {
                slot.1 = value;
            } else {
                pairs.push((key.to_string(), value));
            }
        }
    }

    /// Remove a key, and say whether there was one.
    pub fn remove(&mut self, key: &str) -> bool {
        if let Self::Object(pairs) = self {
            let before = pairs.len();
            pairs.retain(|(name, _)| name != key);
            return pairs.len() != before;
        }
        false
    }

    /// Append to an array.
    pub fn push(&mut self, value: Value) {
        if let Self::Array(items) = self {
            items.push(value);
        }
    }

    /// The compact form: no spaces, no newlines.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, None, 0);
        out
    }

    /// The readable form, two spaces per level.
    ///
    /// What the files on disk are written in. They are read by people as often
    /// as by programs, and a state file on one line is a state file nobody
    /// reviews.
    #[must_use]
    pub fn to_pretty(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, Some(2), 0);
        out
    }

    fn write(&self, out: &mut String, indent: Option<usize>, depth: usize) {
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(true) => out.push_str("true"),
            Self::Bool(false) => out.push_str("false"),
            Self::Number(value) => write_number(out, *value),
            Self::String(text) => write_string(out, text),
            Self::Array(items) => {
                if items.is_empty() {
                    out.push_str("[]");
                    return;
                }
                out.push('[');
                for (at, item) in items.iter().enumerate() {
                    if at > 0 {
                        out.push(',');
                    }
                    newline(out, indent, depth + 1);
                    item.write(out, indent, depth + 1);
                }
                newline(out, indent, depth);
                out.push(']');
            }
            Self::Object(pairs) => {
                if pairs.is_empty() {
                    out.push_str("{}");
                    return;
                }
                out.push('{');
                for (at, (key, value)) in pairs.iter().enumerate() {
                    if at > 0 {
                        out.push(',');
                    }
                    newline(out, indent, depth + 1);
                    write_string(out, key);
                    out.push(':');
                    if indent.is_some() {
                        out.push(' ');
                    }
                    value.write(out, indent, depth + 1);
                }
                newline(out, indent, depth);
                out.push('}');
            }
        }
    }
}

fn newline(out: &mut String, indent: Option<usize>, depth: usize) {
    if let Some(width) = indent {
        out.push('\n');
        for _ in 0..width * depth {
            out.push(' ');
        }
    }
}

/// Whole numbers without a decimal point, because `3` is what a count is.
fn write_number(out: &mut String, value: f64) {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 9.0e15 {
        let _ = write!(out, "{}", value as i64);
    } else if value.is_finite() {
        let _ = write!(out, "{value}");
    } else {
        // Neither infinity nor NaN is JSON. Null is, and it is what every other
        // implementation writes here.
        out.push_str("null");
    }
}

fn write_string(out: &mut String, text: &str) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            // Everything below a space has to be escaped; everything else goes
            // through as itself, so Japanese in a state file stays readable
            // rather than becoming six characters of hex.
            control if (control as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", control as u32);
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

// -- reading -----------------------------------------------------------------

/// Why a document would not read, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trouble {
    pub what: String,
    /// Bytes from the start.
    pub at: usize,
}

impl std::fmt::Display for Trouble {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(out, "{} at byte {}", self.what, self.at)
    }
}

impl std::error::Error for Trouble {}

/// Read a document.
pub fn parse(text: &str) -> Result<Value, Trouble> {
    let mut reader = Reader {
        bytes: text.as_bytes(),
        at: 0,
    };
    reader.space();
    let value = reader.value(0)?;
    reader.space();
    if reader.at < reader.bytes.len() {
        return Err(reader.trouble("there is more after the value"));
    }
    Ok(value)
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn trouble(&self, what: &str) -> Trouble {
        Trouble {
            what: what.to_string(),
            at: self.at,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn expect(&mut self, byte: u8, what: &str) -> Result<(), Trouble> {
        if self.peek() == Some(byte) {
            self.at += 1;
            return Ok(());
        }
        Err(self.trouble(what))
    }

    fn literal(&mut self, word: &str) -> bool {
        if self.bytes[self.at..].starts_with(word.as_bytes()) {
            self.at += word.len();
            return true;
        }
        false
    }

    fn value(&mut self, depth: usize) -> Result<Value, Trouble> {
        if depth > MAX_DEPTH {
            return Err(self.trouble("nested too deeply"));
        }
        match self.peek() {
            None => Err(self.trouble("the document ended where a value was expected")),
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => self.string().map(Value::String),
            Some(b't') if self.literal("true") => Ok(Value::Bool(true)),
            Some(b'f') if self.literal("false") => Ok(Value::Bool(false)),
            Some(b'n') if self.literal("null") => Ok(Value::Null),
            Some(byte) if byte == b'-' || byte.is_ascii_digit() => self.number(),
            Some(_) => Err(self.trouble("this is not the start of a value")),
        }
    }

    fn object(&mut self, depth: usize) -> Result<Value, Trouble> {
        self.at += 1; // the brace
        let mut pairs: Vec<(String, Value)> = Vec::new();
        self.space();
        if self.peek() == Some(b'}') {
            self.at += 1;
            return Ok(Value::Object(pairs));
        }
        loop {
            self.space();
            let key = self.string()?;
            // A key twice is a document saying two things. Refused rather than
            // resolved: whichever one this crate kept would be the one the
            // writer did not mean half the time.
            if pairs.iter().any(|(name, _)| name == &key) {
                return Err(self.trouble("this key appears twice"));
            }
            self.space();
            self.expect(b':', "a colon was expected after the key")?;
            self.space();
            let value = self.value(depth + 1)?;
            pairs.push((key, value));
            self.space();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b'}') => {
                    self.at += 1;
                    return Ok(Value::Object(pairs));
                }
                _ => return Err(self.trouble("a comma or a closing brace was expected")),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Value, Trouble> {
        self.at += 1; // the bracket
        let mut items = Vec::new();
        self.space();
        if self.peek() == Some(b']') {
            self.at += 1;
            return Ok(Value::Array(items));
        }
        loop {
            self.space();
            items.push(self.value(depth + 1)?);
            self.space();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b']') => {
                    self.at += 1;
                    return Ok(Value::Array(items));
                }
                _ => return Err(self.trouble("a comma or a closing bracket was expected")),
            }
        }
    }

    fn string(&mut self) -> Result<String, Trouble> {
        self.expect(b'"', "a string was expected")?;
        let mut text = String::new();
        loop {
            let Some(byte) = self.peek() else {
                return Err(self.trouble("the document ended inside a string"));
            };
            match byte {
                b'"' => {
                    self.at += 1;
                    return Ok(text);
                }
                b'\\' => {
                    self.at += 1;
                    let Some(escape) = self.peek() else {
                        return Err(self.trouble("the document ended inside an escape"));
                    };
                    self.at += 1;
                    match escape {
                        b'"' => text.push('"'),
                        b'\\' => text.push('\\'),
                        b'/' => text.push('/'),
                        b'b' => text.push('\u{8}'),
                        b'f' => text.push('\u{c}'),
                        b'n' => text.push('\n'),
                        b'r' => text.push('\r'),
                        b't' => text.push('\t'),
                        b'u' => text.push(self.escaped_character()?),
                        _ => return Err(self.trouble("this is not an escape")),
                    }
                }
                control if control < 0x20 => {
                    return Err(self.trouble("a control character has to be escaped"));
                }
                _ => {
                    // Copied as UTF-8 rather than byte by byte: the input is a
                    // `&str`, so whatever is here is already valid, and finding
                    // the end of the character is what `char_indices` is for.
                    let rest = &self.bytes[self.at..];
                    let width = utf8_width(rest[0]);
                    let Some(piece) = rest.get(..width).and_then(|b| std::str::from_utf8(b).ok())
                    else {
                        return Err(self.trouble("this is not valid text"));
                    };
                    text.push_str(piece);
                    self.at += width;
                }
            }
        }
    }

    /// `\uXXXX`, and the surrogate pair that follows it when there is one.
    fn escaped_character(&mut self) -> Result<char, Trouble> {
        let first = self.four_hex()?;
        // A high surrogate on its own is half a character. JSON writes anything
        // outside the basic plane as a pair, so the second half is expected to
        // be right here.
        if (0xD800..0xDC00).contains(&first) {
            if !(self.peek() == Some(b'\\') && self.bytes.get(self.at + 1) == Some(&b'u')) {
                return Err(self.trouble("a surrogate escape has no pair"));
            }
            self.at += 2;
            let second = self.four_hex()?;
            if !(0xDC00..0xE000).contains(&second) {
                return Err(self.trouble("a surrogate escape has the wrong kind of pair"));
            }
            let combined = 0x1_0000 + ((first - 0xD800) << 10) + (second - 0xDC00);
            return char::from_u32(combined).ok_or_else(|| self.trouble("not a character"));
        }
        char::from_u32(first).ok_or_else(|| self.trouble("not a character"))
    }

    fn four_hex(&mut self) -> Result<u32, Trouble> {
        let Some(digits) = self.bytes.get(self.at..self.at + 4) else {
            return Err(self.trouble("an escape needs four hex digits"));
        };
        let mut value = 0u32;
        for digit in digits {
            let Some(part) = (*digit as char).to_digit(16) else {
                return Err(self.trouble("an escape needs four hex digits"));
            };
            value = value * 16 + part;
        }
        self.at += 4;
        Ok(value)
    }

    fn number(&mut self) -> Result<Value, Trouble> {
        let start = self.at;
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        while matches!(self.peek(), Some(byte) if byte.is_ascii_digit()) {
            self.at += 1;
        }
        if self.peek() == Some(b'.') {
            self.at += 1;
            while matches!(self.peek(), Some(byte) if byte.is_ascii_digit()) {
                self.at += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.at += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.at += 1;
            }
            while matches!(self.peek(), Some(byte) if byte.is_ascii_digit()) {
                self.at += 1;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.at])
            .map_err(|_| self.trouble("this is not a number"))?;
        text.parse::<f64>().map(Value::Number).map_err(|_| Trouble {
            what: "this is not a number".to_string(),
            at: start,
        })
    }
}

/// How many bytes a UTF-8 character starting with this byte takes.
const fn utf8_width(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_simple_values_read() {
        assert_eq!(parse("null"), Ok(Value::Null));
        assert_eq!(parse("true"), Ok(Value::Bool(true)));
        assert_eq!(parse(" false "), Ok(Value::Bool(false)));
        assert_eq!(parse("42"), Ok(Value::Number(42.0)));
        assert_eq!(parse("-1.5"), Ok(Value::Number(-1.5)));
        assert_eq!(parse("1e3"), Ok(Value::Number(1000.0)));
        assert_eq!(parse("\"hi\""), Ok(Value::string("hi")));
    }

    #[test]
    fn an_object_keeps_the_order_it_was_given() {
        let value = parse(r#"{"z":1,"a":2,"m":3}"#).unwrap();
        let keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .iter()
            .map(|(key, _)| key.as_str())
            .collect();
        assert_eq!(keys, ["z", "a", "m"]);
        assert_eq!(value.to_text(), r#"{"z":1,"a":2,"m":3}"#);
    }

    #[test]
    fn setting_a_key_that_is_there_keeps_its_place() {
        let mut value = parse(r#"{"a":1,"b":2,"c":3}"#).unwrap();
        value.set("b", Value::number(9));
        assert_eq!(value.to_text(), r#"{"a":1,"b":9,"c":3}"#);
        value.set("d", Value::number(4));
        assert_eq!(value.to_text(), r#"{"a":1,"b":9,"c":3,"d":4}"#);
    }

    #[test]
    fn a_document_round_trips() {
        let text = r#"{"a":[1,2,{"b":null}],"c":"x"}"#;
        let value = parse(text).unwrap();
        assert_eq!(value.to_text(), text);
        assert_eq!(parse(&value.to_pretty()).unwrap(), value);
    }

    #[test]
    fn escapes_read_and_write() {
        let value = parse(r#""a\"b\\c\ndA日""#).unwrap();
        assert_eq!(value.as_str(), Some("a\"b\\c\ndA日"));
        assert_eq!(value.to_text(), "\"a\\\"b\\\\c\\ndA日\"");
    }

    #[test]
    fn a_surrogate_pair_becomes_one_character() {
        let value = parse(r#""😀""#).unwrap();
        assert_eq!(value.as_str(), Some("😀"));
        // And half of one is refused rather than guessed at.
        assert!(parse(r#""\ud83d""#).is_err());
    }

    #[test]
    fn japanese_goes_through_as_itself() {
        let value = Value::string("設定を変えました");
        assert_eq!(value.to_text(), "\"設定を変えました\"");
        assert_eq!(parse(&value.to_text()).unwrap(), value);
    }

    #[test]
    fn whole_numbers_are_written_without_a_point() {
        assert_eq!(Value::Number(3.0).to_text(), "3");
        assert_eq!(Value::Number(-17.0).to_text(), "-17");
        assert_eq!(Value::Number(0.5).to_text(), "0.5");
    }

    #[test]
    fn a_number_that_is_not_whole_is_not_an_integer() {
        assert_eq!(Value::Number(3.0).as_i64(), Some(3));
        assert_eq!(Value::Number(3.5).as_i64(), None);
    }

    #[test]
    fn a_key_twice_is_refused() {
        assert!(parse(r#"{"a":1,"a":2}"#).is_err());
    }

    #[test]
    fn rubbish_is_refused_with_a_position() {
        assert!(parse("").is_err());
        assert!(parse("{").is_err());
        assert!(parse("{\"a\"}").is_err());
        assert!(parse("[1,]").is_err());
        assert!(parse("tru").is_err());
        let trouble = parse("{} and more").unwrap_err();
        assert_eq!(trouble.at, 3);
    }

    #[test]
    fn a_deep_document_is_refused_rather_than_overflowing() {
        let deep = "[".repeat(MAX_DEPTH + 5) + &"]".repeat(MAX_DEPTH + 5);
        assert!(parse(&deep).is_err());
        let shallow = "[".repeat(8) + &"]".repeat(8);
        assert!(parse(&shallow).is_ok());
    }

    #[test]
    fn a_raw_control_character_is_refused() {
        assert!(parse("\"a\nb\"").is_err());
    }

    #[test]
    fn a_path_reads_through_several_levels() {
        let value = parse(r#"{"build":{"status":"ok","errors":0}}"#).unwrap();
        assert_eq!(
            value.at(["build", "status"]).and_then(Value::as_str),
            Some("ok")
        );
        assert_eq!(
            value.at(["build", "errors"]).and_then(Value::as_i64),
            Some(0)
        );
        assert_eq!(value.at(["build", "missing"]), None);
        assert_eq!(value.at(["nothing", "here"]), None);
    }

    #[test]
    fn pretty_and_compact_say_the_same_thing() {
        let value = parse(r#"{"a":[1,2],"b":{},"c":[]}"#).unwrap();
        assert_eq!(parse(&value.to_pretty()).unwrap(), value);
        assert!(value.to_pretty().contains('\n'));
        assert!(!value.to_text().contains('\n'));
    }
}
