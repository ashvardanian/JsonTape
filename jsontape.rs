#![cfg_attr(not(feature = "std"), no_std)]

//! # JsonTape
//!
//! A minimalistic, allocator-aware JSON parser in pure Rust, offered in two
//! complementary flavors that share one strict, RFC 8259-conformant scanner.
//!
//! * [`Json`] is an owned, mutable document and the default. Strings are eagerly
//!   unescaped into the document's allocator at parse time, so [`Json::as_str`]
//!   needs no source and the tree can be freely edited or built from scratch.
//!   Produce one with [`parse`] or [`parse_in`].
//! * [`JsonView`] is an immutable, zero-copy document. String values and object
//!   keys are kept as [`Span`]s into the original source rather than decoded, so
//!   parsing copies nothing, and lookups compare against those still-escaped
//!   spans with an escape-aware, allocation-free comparison. Produce one with
//!   [`view`] or [`view_in`].
//!
//! Both containers allocate through any [`allocator_api2::alloc::Allocator`], so
//! the whole document can live in a bump arena and free in one step.
//!
//! ```rust
//! use jsontape::parse;
//!
//! let document = parse(br#"{ "metric": "ip", "nodes": 20000000, "peers": [1, 2, 3] }"#).unwrap();
//!
//! // Navigate with indexing; a miss anywhere in the chain yields Null, not a panic.
//! assert_eq!(document["metric"].as_str(), Some("ip"));
//! assert_eq!(document["nodes"].as_u64(), Some(20_000_000));
//! assert_eq!(document["peers"][0].as_u64(), Some(1));
//! assert!(document["absent"].is_null());
//! ```

#[cfg(feature = "std")]
extern crate std;

#[cfg(not(feature = "std"))]
extern crate alloc;

use core::cmp::Ordering;
use core::fmt;
use core::hash::{Hash, Hasher};
use core::marker::PhantomData;

use allocator_api2::alloc::{Allocator, Global};
use allocator_api2::vec::Vec;

#[cfg(not(feature = "std"))]
use alloc::string::String;

/// A span into the source bytes: a byte offset and a length. String values and
/// object keys are stored this way, undecoded, so parsing copies nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub len: usize,
}

impl Span {
    /// Resolves against the source the document was parsed from. Returns `None`
    /// if the span is out of bounds or not valid UTF-8. Escape sequences are
    /// returned verbatim, not decoded; use [`unescape_into`] to decode them.
    pub fn resolve(self, source: &[u8]) -> Option<&str> {
        let end = self.start.checked_add(self.len)?;
        core::str::from_utf8(source.get(self.start..end)?).ok()
    }

    /// The raw bytes covered by this span, or `None` if out of bounds.
    fn bytes(self, source: &[u8]) -> Option<&[u8]> {
        let end = self.start.checked_add(self.len)?;
        source.get(self.start..end)
    }
}

/// Why parsing failed. No code path panics on bad input.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum JsonError {
    /// Malformed JSON, with the byte offset of the fault and what went wrong.
    Syntax { offset: usize, kind: SyntaxKind },
    /// A fallible allocation failed.
    Allocation,
}

/// What kind of syntax fault occurred. Non-exhaustive so new categories can be
/// added without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SyntaxKind {
    /// A byte appeared where a different one was required.
    UnexpectedByte,
    /// The input ended while a value or container was still open.
    UnexpectedEnd,
    /// Content followed the top-level value.
    TrailingData,
    /// A number did not match the RFC 8259 grammar.
    InvalidNumber,
    /// A string span was out of bounds or otherwise not a valid string body.
    InvalidString,
    /// A string escape sequence was malformed.
    InvalidEscape,
    /// A `\u` escape formed an unpaired UTF-16 surrogate.
    LoneSurrogate,
    /// A string body was not valid UTF-8.
    InvalidUtf8,
    /// An unescaped control character appeared inside a string.
    ControlCharacter,
    /// Nesting exceeded the configured depth limit.
    DepthExceeded,
}

impl SyntaxKind {
    /// A short, lowercase description without trailing punctuation.
    fn message(self) -> &'static str {
        match self {
            SyntaxKind::UnexpectedByte => "unexpected byte",
            SyntaxKind::UnexpectedEnd => "unexpected end of input",
            SyntaxKind::TrailingData => "trailing data after value",
            SyntaxKind::InvalidNumber => "invalid number",
            SyntaxKind::InvalidString => "invalid string",
            SyntaxKind::InvalidEscape => "invalid string escape",
            SyntaxKind::LoneSurrogate => "unpaired surrogate",
            SyntaxKind::InvalidUtf8 => "invalid UTF-8",
            SyntaxKind::ControlCharacter => "unescaped control character",
            SyntaxKind::DepthExceeded => "nesting too deep",
        }
    }
}

/// A resolved source position: byte offset plus 1-based line and column. The
/// column counts bytes since the last newline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Location {
    pub offset: usize,
    pub line: usize,
    pub column: usize,
}

impl JsonError {
    /// Resolves a [`Syntax`](JsonError::Syntax) fault to a line and column by
    /// scanning `source` up to the fault offset. Returns `None` for
    /// [`Allocation`](JsonError::Allocation).
    pub fn location(&self, source: &[u8]) -> Option<Location> {
        let offset = match self {
            JsonError::Syntax { offset, .. } => *offset,
            _ => return None,
        };
        let end = offset.min(source.len());
        let mut line = 1;
        let mut column = 1;
        for &byte in &source[..end] {
            if byte == b'\n' {
                line += 1;
                column = 1;
            } else {
                column += 1;
            }
        }
        Some(Location { offset, line, column })
    }
}

impl fmt::Display for JsonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JsonError::Syntax { offset, kind } => {
                write!(formatter, "{} at byte offset {offset}", kind.message())
            }
            JsonError::Allocation => write!(formatter, "memory allocation failed"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for JsonError {}

/// Guards against stack exhaustion on adversarially nested input.
const MAX_DEPTH: u32 = 128;

/// Maps a single ASCII hex digit to its value, or `None` if it is not one.
fn hex_value(byte: u8) -> Option<u16> {
    match byte {
        b'0'..=b'9' => Some((byte - b'0') as u16),
        b'a'..=b'f' => Some((byte - b'a' + 10) as u16),
        b'A'..=b'F' => Some((byte - b'A' + 10) as u16),
        _ => None,
    }
}

/// Decodes the bytes of a JSON string span, yielding the resulting UTF-8 bytes
/// one at a time. Every access is bounds-checked: on a malformed span the
/// iterator stops early and sets [`malformed`](UnescapeBytes::malformed), so it
/// never panics even on a span the scanner did not produce.
struct UnescapeBytes<'a> {
    content: &'a [u8],
    index: usize,
    // A decoded `\u` escape can expand to up to four UTF-8 bytes; they are
    // buffered here and drained one at a time before scanning resumes.
    pending: [u8; 4],
    pending_len: u8,
    pending_position: u8,
    // Set when decoding hit an escape the scanner would have rejected.
    malformed: bool,
}

impl<'a> UnescapeBytes<'a> {
    fn new(content: &'a [u8]) -> Self {
        UnescapeBytes {
            content,
            index: 0,
            pending: [0; 4],
            pending_len: 0,
            pending_position: 0,
            malformed: false,
        }
    }

    /// Reads four hex digits starting at `self.index`, advancing past them.
    /// Returns `None` if any are missing or not hexadecimal.
    fn read_hex4(&mut self) -> Option<u16> {
        let mut value = 0u16;
        for _ in 0..4 {
            let digit = hex_value(*self.content.get(self.index)?)?;
            value = value * 16 + digit;
            self.index += 1;
        }
        Some(value)
    }

    /// Encodes `character` into the pending buffer and returns its first byte.
    fn emit_character(&mut self, character: char) -> Option<u8> {
        let mut buffer = [0u8; 4];
        let encoded = character.encode_utf8(&mut buffer).len();
        self.pending[..encoded].copy_from_slice(&buffer[..encoded]);
        self.pending_len = encoded as u8;
        self.pending_position = 1;
        Some(self.pending[0])
    }

    /// Marks the span malformed and stops the iterator.
    fn fail(&mut self) -> Option<u8> {
        self.malformed = true;
        None
    }
}

impl<'a> Iterator for UnescapeBytes<'a> {
    type Item = u8;

    fn next(&mut self) -> Option<u8> {
        if self.pending_position < self.pending_len {
            let byte = self.pending[self.pending_position as usize];
            self.pending_position += 1;
            return Some(byte);
        }
        let byte = *self.content.get(self.index)?;
        if byte != b'\\' {
            self.index += 1;
            return Some(byte);
        }
        // Consume the backslash and its escape selector.
        self.index += 1;
        let selector = match self.content.get(self.index) {
            Some(&selector) => selector,
            None => return self.fail(),
        };
        self.index += 1;
        match selector {
            b'"' => Some(b'"'),
            b'\\' => Some(b'\\'),
            b'/' => Some(b'/'),
            b'b' => Some(0x08),
            b'f' => Some(0x0C),
            b'n' => Some(b'\n'),
            b'r' => Some(b'\r'),
            b't' => Some(b'\t'),
            b'u' => {
                let high = match self.read_hex4() {
                    Some(high) => high,
                    None => return self.fail(),
                };
                let code_point = if (0xD800..=0xDBFF).contains(&high) {
                    // A high surrogate must be followed by `\u` and a low one.
                    if self.content.get(self.index) != Some(&b'\\')
                        || self.content.get(self.index + 1) != Some(&b'u')
                    {
                        return self.fail();
                    }
                    self.index += 2; // skip the "\u" of the low surrogate
                    let low = match self.read_hex4() {
                        Some(low) => low,
                        None => return self.fail(),
                    };
                    if !(0xDC00..=0xDFFF).contains(&low) {
                        return self.fail();
                    }
                    0x10000 + (((high - 0xD800) as u32) << 10) + (low - 0xDC00) as u32
                } else if (0xDC00..=0xDFFF).contains(&high) {
                    return self.fail(); // lone low surrogate
                } else {
                    high as u32
                };
                match char::from_u32(code_point) {
                    Some(character) => self.emit_character(character),
                    None => self.fail(),
                }
            }
            _ => self.fail(),
        }
    }
}

/// Decodes a JSON string `span` from `source`, appending the resulting UTF-8
/// bytes to `output`. Spans produced by this crate's parser are always
/// well-formed; a hand-built or mismatched span is reported, never panicked on.
///
/// # Errors
///
/// Returns [`JsonError::Allocation`] if `output` cannot grow, or
/// [`JsonError::Syntax`] if the span falls outside `source` or is not a
/// well-formed JSON string body.
pub fn unescape_into<A: Allocator>(
    source: &[u8],
    span: Span,
    output: &mut Vec<u8, A>,
) -> Result<(), JsonError> {
    let content = span.bytes(source).ok_or(JsonError::Syntax {
        offset: span.start,
        kind: SyntaxKind::InvalidString,
    })?;
    let mut decoder = UnescapeBytes::new(content);
    for byte in decoder.by_ref() {
        output.try_reserve(1).map_err(|_| JsonError::Allocation)?;
        output.push(byte);
    }
    if decoder.malformed {
        return Err(JsonError::Syntax {
            offset: span.start,
            kind: SyntaxKind::InvalidEscape,
        });
    }
    Ok(())
}

/// Compares a still-escaped JSON string `span` against a decoded `expected`
/// string, decoding the span on the fly. Allocation-free.
fn escaped_equals(source: &[u8], span: Span, expected: &str) -> bool {
    let content = match span.bytes(source) {
        Some(content) => content,
        None => return false,
    };
    // Fast path: an unescaped key compares byte-for-byte with no decoding.
    if !content.contains(&b'\\') {
        return content == expected.as_bytes();
    }
    let mut decoded = UnescapeBytes::new(content);
    let mut wanted = expected.as_bytes().iter();
    loop {
        match (decoded.next(), wanted.next()) {
            (Some(left), Some(&right)) if left == right => continue,
            (None, None) => return true,
            _ => return false,
        }
    }
}

/// Orders a still-escaped JSON string `span` against a decoded `other` string by
/// their decoded byte sequences. Allocation-free; used for view-side sorting.
fn escaped_compare(source: &[u8], span: Span, other: &str) -> Ordering {
    let content = span.bytes(source).unwrap_or(&[]);
    let mut decoded = UnescapeBytes::new(content);
    let mut wanted = other.as_bytes().iter();
    loop {
        match (decoded.next(), wanted.next()) {
            (Some(left), Some(&right)) => match left.cmp(&right) {
                Ordering::Equal => continue,
                unequal => return unequal,
            },
            (Some(_), None) => return Ordering::Greater,
            (None, Some(_)) => return Ordering::Less,
            (None, None) => return Ordering::Equal,
        }
    }
}

/// Orders two still-escaped JSON string spans by their decoded byte sequences,
/// decoding both on the fly. Allocation-free; used to sort object keys.
fn escaped_span_compare(source: &[u8], left: Span, right: Span) -> Ordering {
    let mut left_decoded = UnescapeBytes::new(left.bytes(source).unwrap_or(&[]));
    let mut right_decoded = UnescapeBytes::new(right.bytes(source).unwrap_or(&[]));
    loop {
        match (left_decoded.next(), right_decoded.next()) {
            (Some(left_byte), Some(right_byte)) => match left_byte.cmp(&right_byte) {
                Ordering::Equal => continue,
                unequal => return unequal,
            },
            (Some(_), None) => return Ordering::Greater,
            (None, Some(_)) => return Ordering::Less,
            (None, None) => return Ordering::Equal,
        }
    }
}

/// A scanned numeric token, already classified into the narrowest lane.
enum Scalar {
    Integer(i64),
    Unsigned(u64),
    Float(f64),
}

/// Turns raw JSON tokens into a document. The parser is generic over this trait
/// so a single recursion produces either a [`JsonView`] or a [`Json`].
trait DomBuilder<A: Allocator + Clone> {
    /// A fully parsed value.
    type Value;
    /// An object key.
    type Key;

    fn null() -> Self::Value;
    fn boolean(value: bool) -> Self::Value;
    fn integer(value: i64) -> Self::Value;
    fn unsigned(value: u64) -> Self::Value;
    fn float(value: f64) -> Self::Value;
    fn string(source: &[u8], span: Span, allocator: &A) -> Result<Self::Value, JsonError>;
    fn key(source: &[u8], span: Span, allocator: &A) -> Result<Self::Key, JsonError>;
    fn array(items: Vec<Self::Value, A>) -> Self::Value;
    fn object(entries: Vec<(Self::Key, Self::Value), A>) -> Self::Value;
}

struct Parser<'a, A: Allocator + Clone, B: DomBuilder<A>> {
    source: &'a [u8],
    position: usize,
    depth: u32,
    allocator: A,
    _builder: PhantomData<B>,
}

impl<'a, A: Allocator + Clone, B: DomBuilder<A>> Parser<'a, A, B> {
    fn new(source: &'a [u8], allocator: A) -> Self {
        Parser {
            source,
            position: 0,
            depth: 0,
            allocator,
            _builder: PhantomData,
        }
    }

    fn fault<T>(&self) -> Result<T, JsonError> {
        self.fault_kind(SyntaxKind::UnexpectedByte)
    }

    fn fault_kind<T>(&self, kind: SyntaxKind) -> Result<T, JsonError> {
        Err(JsonError::Syntax {
            offset: self.position,
            kind,
        })
    }

    fn peek(&self) -> Option<u8> {
        self.source.get(self.position).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<u8> {
        self.source.get(self.position + offset).copied()
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.position += 1;
        }
    }

    /// Parses a single top-level document, requiring that nothing but
    /// whitespace follows the value.
    fn parse_document(&mut self) -> Result<B::Value, JsonError> {
        let value = self.value()?;
        self.skip_whitespace();
        if self.position != self.source.len() {
            return self.fault_kind(SyntaxKind::TrailingData);
        }
        Ok(value)
    }

    fn value(&mut self) -> Result<B::Value, JsonError> {
        self.skip_whitespace();
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => {
                let span = self.scan_string()?;
                B::string(self.source, span, &self.allocator)
            }
            Some(b't') => self.expect_literal(b"true").map(|_| B::boolean(true)),
            Some(b'f') => self.expect_literal(b"false").map(|_| B::boolean(false)),
            Some(b'n') => self.expect_literal(b"null").map(|_| B::null()),
            Some(byte) if byte == b'-' || byte.is_ascii_digit() => self.number(),
            None => self.fault_kind(SyntaxKind::UnexpectedEnd),
            _ => self.fault(),
        }
    }

    fn expect_literal(&mut self, literal: &[u8]) -> Result<(), JsonError> {
        let end = self.position + literal.len();
        if self.source.get(self.position..end) == Some(literal) {
            self.position = end;
            Ok(())
        } else {
            self.fault()
        }
    }

    fn number(&mut self) -> Result<B::Value, JsonError> {
        match self.scan_number()? {
            Scalar::Integer(value) => Ok(B::integer(value)),
            Scalar::Unsigned(value) => Ok(B::unsigned(value)),
            Scalar::Float(value) => Ok(B::float(value)),
        }
    }

    /// Scans a number under the strict RFC 8259 grammar: an optional minus, an
    /// integer part of either a lone zero or a non-zero digit followed by more
    /// digits, an optional fraction of a dot and digits, and an optional
    /// exponent. Then classifies it into the narrowest of `i64`, `u64`, or `f64`.
    fn scan_number(&mut self) -> Result<Scalar, JsonError> {
        let start = self.position;

        if self.peek() == Some(b'-') {
            self.position += 1;
        }

        // Integer part: a lone zero, or a non-zero digit run with no leading zeros.
        match self.peek() {
            Some(b'0') => self.position += 1,
            Some(b'1'..=b'9') => {
                self.position += 1;
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.position += 1;
                }
            }
            _ => {
                return Err(JsonError::Syntax {
                    offset: start,
                    kind: SyntaxKind::InvalidNumber,
                })
            }
        }

        let mut is_float = false;

        // Fractional part: a dot followed by at least one digit.
        if self.peek() == Some(b'.') {
            is_float = true;
            self.position += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(JsonError::Syntax {
                    offset: self.position,
                    kind: SyntaxKind::InvalidNumber,
                });
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.position += 1;
            }
        }

        // Exponent part: e/E, an optional sign, then at least one digit.
        if matches!(self.peek(), Some(b'e' | b'E')) {
            is_float = true;
            self.position += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.position += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(JsonError::Syntax {
                    offset: self.position,
                    kind: SyntaxKind::InvalidNumber,
                });
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.position += 1;
            }
        }

        let token = &self.source[start..self.position];
        // The token is all ASCII, so this never fails.
        let text = core::str::from_utf8(token).map_err(|_| JsonError::Syntax { offset: start, kind: SyntaxKind::InvalidNumber })?;

        if is_float {
            return text
                .parse::<f64>()
                .map(Scalar::Float)
                .map_err(|_| JsonError::Syntax { offset: start, kind: SyntaxKind::InvalidNumber });
        }
        if token[0] == b'-' {
            if let Ok(value) = text.parse::<i64>() {
                return Ok(Scalar::Integer(value));
            }
        } else if let Ok(value) = text.parse::<u64>() {
            return Ok(Scalar::Unsigned(value));
        }
        // Integer too wide for its 64-bit lane; fall back to float.
        text.parse::<f64>()
            .map(Scalar::Float)
            .map_err(|_| JsonError::Syntax { offset: start, kind: SyntaxKind::InvalidNumber })
    }

    /// Scans a string, validating escapes and rejecting raw control characters,
    /// and returns the raw span between the quotes, still escaped.
    fn scan_string(&mut self) -> Result<Span, JsonError> {
        self.position += 1; // opening quote
        let start = self.position;
        loop {
            match self.peek() {
                None => return self.fault_kind(SyntaxKind::UnexpectedEnd),
                Some(b'"') => {
                    // RFC 8259 §8.1: the text must be valid UTF-8. Escape bytes
                    // are all ASCII, so validating the whole body catches every
                    // ill-formed literal byte in one pass.
                    let content = &self.source[start..self.position];
                    if let Err(error) = core::str::from_utf8(content) {
                        return Err(JsonError::Syntax {
                            offset: start + error.valid_up_to(),
                            kind: SyntaxKind::InvalidUtf8,
                        });
                    }
                    let span = Span {
                        start,
                        len: self.position - start,
                    };
                    self.position += 1;
                    return Ok(span);
                }
                Some(b'\\') => {
                    self.position += 1;
                    self.scan_escape()?;
                }
                // RFC 8259 forbids unescaped control characters in strings.
                Some(byte) if byte < 0x20 => return self.fault_kind(SyntaxKind::ControlCharacter),
                Some(_) => self.position += 1,
            }
        }
    }

    /// Validates one escape sequence, positioned just past the backslash.
    fn scan_escape(&mut self) -> Result<(), JsonError> {
        match self.peek() {
            Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => {
                self.position += 1;
                Ok(())
            }
            Some(b'u') => {
                let high = self.scan_hex4()?;
                if (0xD800..=0xDBFF).contains(&high) {
                    // A high surrogate must be followed by a low surrogate.
                    if self.peek() == Some(b'\\') && self.peek_at(1) == Some(b'u') {
                        self.position += 1; // the backslash
                        let low = self.scan_hex4()?;
                        if (0xDC00..=0xDFFF).contains(&low) {
                            Ok(())
                        } else {
                            self.fault_kind(SyntaxKind::LoneSurrogate)
                        }
                    } else {
                        self.fault_kind(SyntaxKind::LoneSurrogate)
                    }
                } else if (0xDC00..=0xDFFF).contains(&high) {
                    // A low surrogate without a preceding high surrogate.
                    self.fault_kind(SyntaxKind::LoneSurrogate)
                } else {
                    Ok(())
                }
            }
            _ => self.fault_kind(SyntaxKind::InvalidEscape),
        }
    }

    /// Reads `\u`'s four hex digits, positioned on the `u`, and returns the code
    /// unit. Advances past the `u` and all four digits.
    fn scan_hex4(&mut self) -> Result<u16, JsonError> {
        self.position += 1; // the 'u'
        let mut value = 0u16;
        for _ in 0..4 {
            match self.peek().and_then(hex_value) {
                Some(digit) => {
                    value = value * 16 + digit;
                    self.position += 1;
                }
                None => return self.fault_kind(SyntaxKind::InvalidEscape),
            }
        }
        Ok(value)
    }

    fn array(&mut self) -> Result<B::Value, JsonError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return self.fault_kind(SyntaxKind::DepthExceeded);
        }
        self.position += 1; // opening bracket
        let mut items: Vec<B::Value, A> = Vec::new_in(self.allocator.clone());
        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.position += 1;
            self.depth -= 1;
            return Ok(B::array(items));
        }
        loop {
            let value = self.value()?;
            self.try_push(&mut items, value)?;
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.position += 1,
                Some(b']') => {
                    self.position += 1;
                    break;
                }
                None => return self.fault_kind(SyntaxKind::UnexpectedEnd),
                _ => return self.fault(),
            }
        }
        self.depth -= 1;
        Ok(B::array(items))
    }

    fn object(&mut self) -> Result<B::Value, JsonError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return self.fault_kind(SyntaxKind::DepthExceeded);
        }
        self.position += 1; // opening brace
        let mut entries: Vec<(B::Key, B::Value), A> = Vec::new_in(self.allocator.clone());
        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.position += 1;
            self.depth -= 1;
            return Ok(B::object(entries));
        }
        loop {
            self.skip_whitespace();
            if self.peek() != Some(b'"') {
                return self.fault();
            }
            let key_span = self.scan_string()?;
            let key = B::key(self.source, key_span, &self.allocator)?;
            self.skip_whitespace();
            if self.peek() != Some(b':') {
                return self.fault();
            }
            self.position += 1;
            let value = self.value()?;
            self.try_push(&mut entries, (key, value))?;
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.position += 1,
                Some(b'}') => {
                    self.position += 1;
                    break;
                }
                None => return self.fault_kind(SyntaxKind::UnexpectedEnd),
                _ => return self.fault(),
            }
        }
        self.depth -= 1;
        Ok(B::object(entries))
    }

    fn try_push<T>(&self, items: &mut Vec<T, A>, item: T) -> Result<(), JsonError> {
        items.try_reserve(1).map_err(|_| JsonError::Allocation)?;
        items.push(item);
        Ok(())
    }
}

/// An immutable JSON document that borrows its text from the parsed source.
/// String values and object keys are stored as [`Span`]s, so nothing is copied;
/// resolve them against the same `source` you parsed. Numbers keep their width,
/// so large integers survive past the 2^53 float boundary.
///
/// # Layout and allocators
///
/// Each array and object owns its own [`Vec`], so parsing with the default
/// global allocator scatters those nodes across the heap. Parse with
/// [`view_in`] and a bump or slab allocator (any [`allocator_api2::alloc::Allocator`],
/// for example a `bumpalo::Bump`) to pack the whole document into one contiguous
/// region that frees in O(1) when the arena is dropped — the closest this design
/// gets to a flat tape. Note the caveat: the per-node `Vec`s still grow by
/// doubling, so an arena keeps some slack and dead reallocation remnants; it is
/// arena-flat and cheap to allocate and free, not a compact simdjson-style tape.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum JsonView<A: Allocator = Global> {
    Null,
    Boolean(bool),
    Integer(i64),
    Unsigned(u64),
    Float(f64),
    String(Span),
    Array(Vec<JsonView<A>, A>),
    Object(Vec<(Span, JsonView<A>), A>),
}

/// Zero-sized builder that keeps strings as raw spans.
struct ViewBuilder;

impl<A: Allocator + Clone> DomBuilder<A> for ViewBuilder {
    type Value = JsonView<A>;
    type Key = Span;

    fn null() -> JsonView<A> {
        JsonView::Null
    }
    fn boolean(value: bool) -> JsonView<A> {
        JsonView::Boolean(value)
    }
    fn integer(value: i64) -> JsonView<A> {
        JsonView::Integer(value)
    }
    fn unsigned(value: u64) -> JsonView<A> {
        JsonView::Unsigned(value)
    }
    fn float(value: f64) -> JsonView<A> {
        JsonView::Float(value)
    }
    fn string(_source: &[u8], span: Span, _allocator: &A) -> Result<JsonView<A>, JsonError> {
        Ok(JsonView::String(span))
    }
    fn key(_source: &[u8], span: Span, _allocator: &A) -> Result<Span, JsonError> {
        Ok(span)
    }
    fn array(items: Vec<JsonView<A>, A>) -> JsonView<A> {
        JsonView::Array(items)
    }
    fn object(entries: Vec<(Span, JsonView<A>), A>) -> JsonView<A> {
        JsonView::Object(entries)
    }
}

impl<A: Allocator> JsonView<A> {
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            JsonView::Boolean(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            JsonView::Unsigned(value) => Some(*value),
            JsonView::Integer(value) if *value >= 0 => Some(*value as u64),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            JsonView::Integer(value) => Some(*value),
            JsonView::Unsigned(value) if *value <= i64::MAX as u64 => Some(*value as i64),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            JsonView::Float(value) => Some(*value),
            JsonView::Integer(value) => Some(*value as f64),
            JsonView::Unsigned(value) => Some(*value as f64),
            _ => None,
        }
    }

    /// The raw span of a string value, or `None`. Escapes are not decoded.
    pub fn as_span(&self) -> Option<Span> {
        match self {
            JsonView::String(span) => Some(*span),
            _ => None,
        }
    }

    /// Resolves a string value against its source, without decoding escapes.
    pub fn as_str<'s>(&self, source: &'s [u8]) -> Option<&'s str> {
        match self {
            JsonView::String(span) => span.resolve(source),
            _ => None,
        }
    }

    /// Decodes a string value's escapes into `output`. Returns `None` if this is
    /// not a string, otherwise the result of the decode.
    pub fn unescape_into<B: Allocator>(
        &self,
        source: &[u8],
        output: &mut Vec<u8, B>,
    ) -> Option<Result<(), JsonError>> {
        match self {
            JsonView::String(span) => Some(unescape_into(source, *span, output)),
            _ => None,
        }
    }

    /// The elements of an array value, or `None`.
    pub fn as_array(&self) -> Option<&[JsonView<A>]> {
        match self {
            JsonView::Array(items) => Some(items),
            _ => None,
        }
    }

    /// The key-span and value entries of an object value, or `None`.
    pub fn as_object(&self) -> Option<&[(Span, JsonView<A>)]> {
        match self {
            JsonView::Object(entries) => Some(entries),
            _ => None,
        }
    }

    /// Iterates the elements of an array value; empty for anything else.
    pub fn iter(&self) -> core::slice::Iter<'_, JsonView<A>> {
        match self {
            JsonView::Array(items) => items.iter(),
            _ => [].iter(),
        }
    }

    /// The number of array elements or object entries; `0` for scalars.
    pub fn len(&self) -> usize {
        match self {
            JsonView::Array(items) => items.len(),
            JsonView::Object(entries) => entries.len(),
            _ => 0,
        }
    }

    /// Whether an array or object has no elements. Always `false` for scalars.
    pub fn is_empty(&self) -> bool {
        match self {
            JsonView::Array(items) => items.is_empty(),
            JsonView::Object(entries) => entries.is_empty(),
            _ => false,
        }
    }

    /// Looks up a key in an object, matching against the decoded form of each
    /// stored key. Linear scan; returns the first match.
    pub fn get(&self, source: &[u8], key: &str) -> Option<&JsonView<A>> {
        match self {
            JsonView::Object(entries) => entries
                .iter()
                .find(|(stored, _)| escaped_equals(source, *stored, key))
                .map(|(_, value)| value),
            _ => None,
        }
    }

    /// Recursively sorts every object's entries by their decoded key order,
    /// enabling [`JsonView::get_sorted`] for logarithmic lookups.
    /// Arrays are traversed so nested objects are sorted too; array element
    /// order is preserved. Reorders the document's own storage, not the source.
    pub fn sort_keys(&mut self, source: &[u8]) {
        match self {
            JsonView::Array(items) => {
                for item in items.iter_mut() {
                    item.sort_keys(source);
                }
            }
            JsonView::Object(entries) => {
                entries.sort_unstable_by(|(left, _), (right, _)| {
                    escaped_span_compare(source, *left, *right)
                });
                for (_, value) in entries.iter_mut() {
                    value.sort_keys(source);
                }
            }
            _ => {}
        }
    }

    /// Looks up a key in an object previously ordered by
    /// [`JsonView::sort_keys`], using binary search. Returns `None`
    /// for a non-object; results are unspecified if the object is not sorted.
    pub fn get_sorted(&self, source: &[u8], key: &str) -> Option<&JsonView<A>> {
        match self {
            JsonView::Object(entries) => entries
                .binary_search_by(|(stored, _)| escaped_compare(source, *stored, key))
                .ok()
                .map(|index| &entries[index].1),
            _ => None,
        }
    }
}

/// An owned, decoded UTF-8 string used for keys and string values in [`Json`].
/// Its bytes are always valid UTF-8, so [`JsonString::as_str`] never fails.
pub struct JsonString<A: Allocator = Global>(Vec<u8, A>);

/// The backing storage for a [`Json::Object`]: its key and value pairs.
type ObjectEntries<A> = Vec<(JsonString<A>, Json<A>), A>;

impl<A: Allocator> JsonString<A> {
    /// Decodes a validated JSON string `span` into a fresh owned string.
    fn from_span(source: &[u8], span: Span, allocator: A) -> Result<Self, JsonError> {
        let mut bytes = Vec::new_in(allocator);
        unescape_into(source, span, &mut bytes)?;
        if core::str::from_utf8(&bytes).is_err() {
            return Err(JsonError::Syntax {
                offset: span.start,
                kind: SyntaxKind::InvalidUtf8,
            });
        }
        Ok(JsonString(bytes))
    }

    /// Copies `text` into a fresh owned string. Aborts on allocation failure,
    /// like the standard collections' build-from-code paths.
    fn from_str_in(text: &str, allocator: A) -> Self {
        let mut bytes = Vec::new_in(allocator);
        bytes.extend_from_slice(text.as_bytes());
        JsonString(bytes)
    }

    /// The decoded string.
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.0).unwrap_or("")
    }

    /// The decoded UTF-8 bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// The length in bytes of the decoded string.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the decoded string is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<A: Allocator + Clone> Clone for JsonString<A> {
    fn clone(&self) -> Self {
        JsonString(self.0.clone())
    }
}

impl<A: Allocator> fmt::Debug for JsonString<A> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), formatter)
    }
}

impl<A: Allocator, B: Allocator> PartialEq<JsonString<B>> for JsonString<A> {
    fn eq(&self, other: &JsonString<B>) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

impl<A: Allocator> Eq for JsonString<A> {}

impl<A: Allocator> PartialEq<str> for JsonString<A> {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl<A: Allocator> PartialEq<&str> for JsonString<A> {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

/// An owned, mutable JSON document. Unlike [`JsonView`], strings are decoded at
/// parse time, so [`Json::as_str`] needs no source and the tree can be
/// edited freely. Numbers keep their width past the 2^53 float boundary.
///
/// The parser fills a `Json` with fallible allocation, but the build-from-code
/// mutators — [`Json::push`], [`Json::insert`], and the `*_in` constructors —
/// allocate infallibly and abort on out-of-memory, matching the standard
/// collections.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub enum Json<A: Allocator = Global> {
    #[default]
    Null,
    Boolean(bool),
    Integer(i64),
    Unsigned(u64),
    Float(f64),
    String(JsonString<A>),
    Array(Vec<Json<A>, A>),
    Object(Vec<(JsonString<A>, Json<A>), A>),
}

/// Zero-sized builder that eagerly decodes strings into the allocator.
struct OwnedBuilder;

impl<A: Allocator + Clone> DomBuilder<A> for OwnedBuilder {
    type Value = Json<A>;
    type Key = JsonString<A>;

    fn null() -> Json<A> {
        Json::Null
    }
    fn boolean(value: bool) -> Json<A> {
        Json::Boolean(value)
    }
    fn integer(value: i64) -> Json<A> {
        Json::Integer(value)
    }
    fn unsigned(value: u64) -> Json<A> {
        Json::Unsigned(value)
    }
    fn float(value: f64) -> Json<A> {
        Json::Float(value)
    }
    fn string(source: &[u8], span: Span, allocator: &A) -> Result<Json<A>, JsonError> {
        Ok(Json::String(JsonString::from_span(
            source,
            span,
            allocator.clone(),
        )?))
    }
    fn key(source: &[u8], span: Span, allocator: &A) -> Result<JsonString<A>, JsonError> {
        JsonString::from_span(source, span, allocator.clone())
    }
    fn array(items: Vec<Json<A>, A>) -> Json<A> {
        Json::Array(items)
    }
    fn object(entries: Vec<(JsonString<A>, Json<A>), A>) -> Json<A> {
        Json::Object(entries)
    }
}

impl<A: Allocator, B: Allocator> PartialEq<Json<B>> for Json<A> {
    fn eq(&self, other: &Json<B>) -> bool {
        match (self, other) {
            (Json::Null, Json::Null) => true,
            (Json::Boolean(left), Json::Boolean(right)) => left == right,
            (Json::Integer(left), Json::Integer(right)) => left == right,
            (Json::Unsigned(left), Json::Unsigned(right)) => left == right,
            (Json::Float(left), Json::Float(right)) => left == right,
            (Json::String(left), Json::String(right)) => left == right,
            (Json::Array(left), Json::Array(right)) => {
                left.len() == right.len() && left.iter().zip(right.iter()).all(|(a, b)| a == b)
            }
            (Json::Object(left), Json::Object(right)) => {
                left.len() == right.len()
                    && left
                        .iter()
                        .zip(right.iter())
                        .all(|((key_a, value_a), (key_b, value_b))| {
                            key_a == key_b && value_a == value_b
                        })
            }
            _ => false,
        }
    }
}

impl<A: Allocator> Json<A> {
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Boolean(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Json::Unsigned(value) => Some(*value),
            Json::Integer(value) if *value >= 0 => Some(*value as u64),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Json::Integer(value) => Some(*value),
            Json::Unsigned(value) if *value <= i64::MAX as u64 => Some(*value as i64),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Float(value) => Some(*value),
            Json::Integer(value) => Some(*value as f64),
            Json::Unsigned(value) => Some(*value as f64),
            _ => None,
        }
    }

    /// The decoded string value, or `None`. No source needed.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(string) => Some(string.as_str()),
            _ => None,
        }
    }

    /// The elements of an array value, or `None`.
    pub fn as_array(&self) -> Option<&[Json<A>]> {
        match self {
            Json::Array(items) => Some(items),
            _ => None,
        }
    }

    /// The key and value entries of an object value, or `None`.
    pub fn as_object(&self) -> Option<&[(JsonString<A>, Json<A>)]> {
        match self {
            Json::Object(entries) => Some(entries),
            _ => None,
        }
    }

    /// Mutable access to an array value's elements, or `None`.
    pub fn as_array_mut(&mut self) -> Option<&mut Vec<Json<A>, A>> {
        match self {
            Json::Array(items) => Some(items),
            _ => None,
        }
    }

    /// Mutable access to an object value's entries, or `None`.
    pub fn as_object_mut(&mut self) -> Option<&mut ObjectEntries<A>> {
        match self {
            Json::Object(entries) => Some(entries),
            _ => None,
        }
    }

    /// Iterates the elements of an array value; empty for anything else.
    pub fn iter(&self) -> core::slice::Iter<'_, Json<A>> {
        match self {
            Json::Array(items) => items.iter(),
            _ => [].iter(),
        }
    }

    /// The number of array elements or object entries; `0` for scalars.
    pub fn len(&self) -> usize {
        match self {
            Json::Array(items) => items.len(),
            Json::Object(entries) => entries.len(),
            _ => 0,
        }
    }

    /// Whether an array or object has no elements. Always `false` for scalars.
    pub fn is_empty(&self) -> bool {
        match self {
            Json::Array(items) => items.is_empty(),
            Json::Object(entries) => entries.is_empty(),
            _ => false,
        }
    }

    /// Looks up an object key or an array position, returning `None` if it is
    /// absent or the value is the wrong kind. The index is a `&str` key or a
    /// `usize` position, so `document.get("field")` and `document.get(0)` both
    /// work. For infix syntax, index a `Json` with `document["field"]` or
    /// `document[0]`, which never panics and yields `Null` on a miss.
    pub fn get<I: Lookup<A>>(&self, index: I) -> Option<&Json<A>> {
        index.look_up(self)
    }

    /// The mutable counterpart of [`get`](Json::get).
    pub fn get_mut<I: Lookup<A>>(&mut self, index: I) -> Option<&mut Json<A>> {
        index.look_up_mut(self)
    }

    /// Resolves an RFC 6901 JSON Pointer such as `"/items/0/name"`, walking
    /// nested objects and arrays. An empty pointer refers to the whole document;
    /// a malformed pointer or a missing step yields `None`.
    pub fn pointer(&self, pointer: &str) -> Option<&Json<A>> {
        if pointer.is_empty() {
            return Some(self);
        }
        if !pointer.starts_with('/') {
            return None;
        }
        let mut current = self;
        for raw_token in pointer.split('/').skip(1) {
            // Only allocate to unescape when a `~` is actually present.
            let decoded;
            let token: &str = if raw_token.contains('~') {
                decoded = raw_token.replace("~1", "/").replace("~0", "~");
                &decoded
            } else {
                raw_token
            };
            current = match current {
                Json::Object(_) => current.get(token)?,
                Json::Array(_) => {
                    // RFC 6901: an array index is `0` or `[1-9][0-9]*`; reject
                    // leading zeros, which `usize::from_str` would accept.
                    if token.len() > 1 && token.starts_with('0') {
                        return None;
                    }
                    current.get(token.parse::<usize>().ok()?)?
                }
                _ => return None,
            };
        }
        Some(current)
    }

    /// Appends `value` to an array value. Returns `false`, leaving `value`
    /// dropped, if this is not an array.
    pub fn push(&mut self, value: Json<A>) -> bool {
        match self {
            Json::Array(items) => {
                items.push(value);
                true
            }
            _ => false,
        }
    }

    /// Removes and returns the last element of an array value, or `None`.
    pub fn pop(&mut self) -> Option<Json<A>> {
        match self {
            Json::Array(items) => items.pop(),
            _ => None,
        }
    }

    /// Removes a key from an object value, returning its value if present.
    pub fn remove(&mut self, key: &str) -> Option<Json<A>> {
        match self {
            Json::Object(entries) => {
                let index = entries
                    .iter()
                    .position(|(stored, _)| stored.as_str() == key)?;
                Some(entries.remove(index).1)
            }
            _ => None,
        }
    }

    /// An empty object value allocated in `allocator`.
    pub fn object_in(allocator: A) -> Json<A> {
        Json::Object(Vec::new_in(allocator))
    }

    /// An empty array value allocated in `allocator`.
    pub fn array_in(allocator: A) -> Json<A> {
        Json::Array(Vec::new_in(allocator))
    }

    /// A string value holding a copy of `text`, allocated in `allocator`.
    ///
    /// # Errors
    ///
    /// Returns [`JsonError::Allocation`] if the copy cannot be allocated.
    pub fn string_in(text: &str, allocator: A) -> Result<Json<A>, JsonError> {
        let mut bytes = Vec::new_in(allocator);
        bytes
            .try_reserve(text.len())
            .map_err(|_| JsonError::Allocation)?;
        bytes.extend_from_slice(text.as_bytes());
        Ok(Json::String(JsonString(bytes)))
    }
}

impl<A: Allocator + Clone> Json<A> {
    /// Inserts `key`/`value` into an object value. If the key was already
    /// present its old value is replaced and returned; otherwise the entry is
    /// appended and `None` is returned. A no-op returning `None` on a non-object.
    pub fn insert(&mut self, key: &str, value: Json<A>) -> Option<Json<A>> {
        match self {
            Json::Object(entries) => {
                if let Some((_, existing)) = entries
                    .iter_mut()
                    .find(|(stored, _)| stored.as_str() == key)
                {
                    return Some(core::mem::replace(existing, value));
                }
                let allocator = entries.allocator().clone();
                entries.push((JsonString::from_str_in(key, allocator), value));
                None
            }
            _ => None,
        }
    }
}

impl<A: Allocator> JsonView<A> {
    /// Deep-converts this borrowed view into an owned [`Json`] allocated in
    /// `allocator`, decoding every string's escapes.
    ///
    /// # Errors
    ///
    /// Returns [`JsonError::Allocation`] on allocation failure, or
    /// [`JsonError::Syntax`] if a string decodes to invalid UTF-8.
    pub fn to_json_in<B: Allocator + Clone>(
        &self,
        source: &[u8],
        allocator: B,
    ) -> Result<Json<B>, JsonError> {
        self.to_json_ref(source, &allocator)
    }

    /// Shared recursion for [`to_json_in`](JsonView::to_json_in) that borrows the
    /// allocator, cloning it only where a `Vec` or `JsonString` must own one
    /// rather than once per traversal step.
    fn to_json_ref<B: Allocator + Clone>(
        &self,
        source: &[u8],
        allocator: &B,
    ) -> Result<Json<B>, JsonError> {
        match self {
            JsonView::Null => Ok(Json::Null),
            JsonView::Boolean(value) => Ok(Json::Boolean(*value)),
            JsonView::Integer(value) => Ok(Json::Integer(*value)),
            JsonView::Unsigned(value) => Ok(Json::Unsigned(*value)),
            JsonView::Float(value) => Ok(Json::Float(*value)),
            JsonView::String(span) => Ok(Json::String(JsonString::from_span(
                source,
                *span,
                allocator.clone(),
            )?)),
            JsonView::Array(items) => {
                let mut owned = Vec::new_in(allocator.clone());
                for item in items {
                    let converted = item.to_json_ref(source, allocator)?;
                    owned.try_reserve(1).map_err(|_| JsonError::Allocation)?;
                    owned.push(converted);
                }
                Ok(Json::Array(owned))
            }
            JsonView::Object(entries) => {
                let mut owned = Vec::new_in(allocator.clone());
                for (key_span, value) in entries {
                    let key = JsonString::from_span(source, *key_span, allocator.clone())?;
                    let converted = value.to_json_ref(source, allocator)?;
                    owned.try_reserve(1).map_err(|_| JsonError::Allocation)?;
                    owned.push((key, converted));
                }
                Ok(Json::Object(owned))
            }
        }
    }

    /// Deep-converts this borrowed view into an owned [`Json`] on the global heap.
    pub fn to_json(&self, source: &[u8]) -> Result<Json<Global>, JsonError> {
        self.to_json_in(source, Global)
    }
}

/// Whether to write compact JSON or indent it for readability.
#[derive(Clone, Copy)]
enum Layout {
    Compact,
    Pretty { indent: usize },
}

/// Writes a newline followed by `indent * depth` spaces.
fn write_indent<W: fmt::Write>(writer: &mut W, indent: usize, depth: usize) -> fmt::Result {
    writer.write_char('\n')?;
    for _ in 0..(indent * depth) {
        writer.write_char(' ')?;
    }
    Ok(())
}

/// Writes `text` as a quoted JSON string, escaping what the grammar requires.
fn write_escaped_str<W: fmt::Write>(writer: &mut W, text: &str) -> fmt::Result {
    writer.write_char('"')?;
    let bytes = text.as_bytes();
    let mut run_start = 0;
    // Every byte needing an escape is ASCII, and multi-byte UTF-8 bytes are all
    // >= 0x80, so we can bulk-copy the clean runs between escapes and only break
    // out to write the escape itself.
    for (index, &byte) in bytes.iter().enumerate() {
        if byte >= 0x20 && byte != b'"' && byte != b'\\' {
            continue;
        }
        if run_start < index {
            writer.write_str(&text[run_start..index])?;
        }
        match byte {
            b'"' => writer.write_str("\\\"")?,
            b'\\' => writer.write_str("\\\\")?,
            b'\n' => writer.write_str("\\n")?,
            b'\r' => writer.write_str("\\r")?,
            b'\t' => writer.write_str("\\t")?,
            0x08 => writer.write_str("\\b")?,
            0x0C => writer.write_str("\\f")?,
            other => write!(writer, "\\u{:04x}", other)?,
        }
        run_start = index + 1;
    }
    if run_start < bytes.len() {
        writer.write_str(&text[run_start..])?;
    }
    writer.write_char('"')
}

/// Writes a quoted JSON string straight from a raw source span. The span's bytes
/// are already valid, escaped JSON string content, so no re-escaping is needed.
fn write_raw_string<W: fmt::Write>(writer: &mut W, source: &[u8], span: Span) -> fmt::Result {
    writer.write_char('"')?;
    writer.write_str(span.resolve(source).unwrap_or(""))?;
    writer.write_char('"')
}

/// Whether a finite `f64` has no fractional part, computed without the
/// std-only `f64::fract`/`trunc` so it works under `no_std`.
fn is_integral(value: f64) -> bool {
    // Clear the sign bit for the magnitude; `to_bits`/`from_bits` are `core`.
    let magnitude = f64::from_bits(value.to_bits() & 0x7fff_ffff_ffff_ffff);
    if magnitude >= 4_503_599_627_370_496.0 {
        // At or above 2^52 every representable `f64` is already an integer.
        true
    } else {
        // Otherwise the value fits in `i64`, so a round trip is exact.
        value == (value as i64) as f64
    }
}

/// Writes a finite JSON number straight to `writer`. Non-finite floats become
/// `null`, and integral floats keep a trailing `.0` so their type survives a
/// round trip. `f64`'s formatter never uses exponent notation, so writing
/// directly and appending `.0` when integral needs no intermediate buffer.
fn write_number<W: fmt::Write>(writer: &mut W, value: f64) -> fmt::Result {
    if !value.is_finite() {
        return writer.write_str("null");
    }
    write!(writer, "{value}")?;
    if is_integral(value) {
        writer.write_str(".0")?;
    }
    Ok(())
}

impl<A: Allocator> Json<A> {
    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }
    pub fn is_boolean(&self) -> bool {
        matches!(self, Json::Boolean(_))
    }
    pub fn is_number(&self) -> bool {
        matches!(self, Json::Integer(_) | Json::Unsigned(_) | Json::Float(_))
    }
    pub fn is_string(&self) -> bool {
        matches!(self, Json::String(_))
    }
    pub fn is_array(&self) -> bool {
        matches!(self, Json::Array(_))
    }
    pub fn is_object(&self) -> bool {
        matches!(self, Json::Object(_))
    }

    /// Iterates an array value's elements mutably; empty for anything else.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Json<A>> {
        self.as_array_mut().into_iter().flatten()
    }

    /// Iterates an object value's entries; empty for anything else.
    pub fn entries(&self) -> core::slice::Iter<'_, (JsonString<A>, Json<A>)> {
        match self {
            Json::Object(entries) => entries.iter(),
            _ => [].iter(),
        }
    }

    /// Iterates an object value's keys.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries().map(|(key, _)| key.as_str())
    }

    /// Iterates an object value's values.
    pub fn values(&self) -> impl Iterator<Item = &Json<A>> {
        self.entries().map(|(_, value)| value)
    }

    /// Iterates an object value's values mutably.
    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut Json<A>> {
        self.as_object_mut()
            .into_iter()
            .flatten()
            .map(|(_, value)| value)
    }

    /// Consumes an array value, yielding its element storage; `None` otherwise.
    pub fn into_array(self) -> Option<Vec<Json<A>, A>> {
        match self {
            Json::Array(items) => Some(items),
            _ => None,
        }
    }

    /// Consumes an object value, yielding its entry storage; `None` otherwise.
    pub fn into_object(self) -> Option<ObjectEntries<A>> {
        match self {
            Json::Object(entries) => Some(entries),
            _ => None,
        }
    }

    fn write_layout<W: fmt::Write>(&self, writer: &mut W, layout: Layout, depth: usize) -> fmt::Result {
        match self {
            Json::Null => writer.write_str("null"),
            Json::Boolean(value) => writer.write_str(if *value { "true" } else { "false" }),
            Json::Integer(value) => write!(writer, "{value}"),
            Json::Unsigned(value) => write!(writer, "{value}"),
            Json::Float(value) => write_number(writer, *value),
            Json::String(string) => write_escaped_str(writer, string.as_str()),
            Json::Array(items) => {
                if items.is_empty() {
                    return writer.write_str("[]");
                }
                writer.write_char('[')?;
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        writer.write_char(',')?;
                    }
                    if let Layout::Pretty { indent } = layout {
                        write_indent(writer, indent, depth + 1)?;
                    }
                    item.write_layout(writer, layout, depth + 1)?;
                }
                if let Layout::Pretty { indent } = layout {
                    write_indent(writer, indent, depth)?;
                }
                writer.write_char(']')
            }
            Json::Object(entries) => {
                if entries.is_empty() {
                    return writer.write_str("{}");
                }
                writer.write_char('{')?;
                for (index, (key, value)) in entries.iter().enumerate() {
                    if index > 0 {
                        writer.write_char(',')?;
                    }
                    if let Layout::Pretty { indent } = layout {
                        write_indent(writer, indent, depth + 1)?;
                    }
                    write_escaped_str(writer, key.as_str())?;
                    writer.write_str(match layout {
                        Layout::Compact => ":",
                        Layout::Pretty { .. } => ": ",
                    })?;
                    value.write_layout(writer, layout, depth + 1)?;
                }
                if let Layout::Pretty { indent } = layout {
                    write_indent(writer, indent, depth)?;
                }
                writer.write_char('}')
            }
        }
    }

    /// Writes this value as compact JSON.
    pub fn write_json<W: fmt::Write>(&self, writer: &mut W) -> fmt::Result {
        self.write_layout(writer, Layout::Compact, 0)
    }

    /// Writes this value as JSON indented by `indent` spaces per level.
    pub fn write_json_pretty<W: fmt::Write>(&self, writer: &mut W, indent: usize) -> fmt::Result {
        self.write_layout(writer, Layout::Pretty { indent }, 0)
    }

    /// Renders this value as JSON indented by `indent` spaces per level.
    pub fn to_string_pretty(&self, indent: usize) -> String {
        let mut output = String::new();
        let _ = self.write_json_pretty(&mut output, indent);
        output
    }
}

impl<A: Allocator> fmt::Display for Json<A> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write_layout(formatter, Layout::Compact, 0)
    }
}

/// Types that can index a [`Json`] value: a `&str` object key or a `usize`
/// array position. Powers [`Json::get`], [`Json::get_mut`], and the infix
/// [`core::ops::Index`] operators.
pub trait Lookup<A: Allocator> {
    #[doc(hidden)]
    fn look_up<'j>(&self, value: &'j Json<A>) -> Option<&'j Json<A>>;
    #[doc(hidden)]
    fn look_up_mut<'j>(&self, value: &'j mut Json<A>) -> Option<&'j mut Json<A>>;
}

impl<A: Allocator> Lookup<A> for usize {
    fn look_up<'j>(&self, value: &'j Json<A>) -> Option<&'j Json<A>> {
        match value {
            Json::Array(items) => items.get(*self),
            _ => None,
        }
    }
    fn look_up_mut<'j>(&self, value: &'j mut Json<A>) -> Option<&'j mut Json<A>> {
        match value {
            Json::Array(items) => items.get_mut(*self),
            _ => None,
        }
    }
}

impl<A: Allocator> Lookup<A> for str {
    fn look_up<'j>(&self, value: &'j Json<A>) -> Option<&'j Json<A>> {
        match value {
            Json::Object(entries) => entries
                .iter()
                .find(|(key, _)| key.as_str() == self)
                .map(|(_, value)| value),
            _ => None,
        }
    }
    fn look_up_mut<'j>(&self, value: &'j mut Json<A>) -> Option<&'j mut Json<A>> {
        match value {
            Json::Object(entries) => entries
                .iter_mut()
                .find(|(key, _)| key.as_str() == self)
                .map(|(_, value)| value),
            _ => None,
        }
    }
}

impl<A: Allocator> Lookup<A> for &str {
    fn look_up<'j>(&self, value: &'j Json<A>) -> Option<&'j Json<A>> {
        <str as Lookup<A>>::look_up(self, value)
    }
    fn look_up_mut<'j>(&self, value: &'j mut Json<A>) -> Option<&'j mut Json<A>> {
        <str as Lookup<A>>::look_up_mut(self, value)
    }
}

// The `Null` fallback below is a promoted `&'static Json<A>`. Rvalue static
// promotion applies because `Json` has no explicit `Drop` impl (its drop glue
// comes only from the `Vec` fields), so this works for every allocator `A`, not
// just `Global` — indexing a miss yields `Null` instead of panicking.
impl<A: Allocator> core::ops::Index<usize> for Json<A> {
    type Output = Json<A>;
    fn index(&self, index: usize) -> &Json<A> {
        self.get(index).unwrap_or(&Json::Null)
    }
}

impl<A: Allocator> core::ops::Index<&str> for Json<A> {
    type Output = Json<A>;
    fn index(&self, key: &str) -> &Json<A> {
        self.get(key).unwrap_or(&Json::Null)
    }
}

impl<A: Allocator> JsonView<A> {
    pub fn is_null(&self) -> bool {
        matches!(self, JsonView::Null)
    }
    pub fn is_boolean(&self) -> bool {
        matches!(self, JsonView::Boolean(_))
    }
    pub fn is_number(&self) -> bool {
        matches!(self, JsonView::Integer(_) | JsonView::Unsigned(_) | JsonView::Float(_))
    }
    pub fn is_string(&self) -> bool {
        matches!(self, JsonView::String(_))
    }
    pub fn is_array(&self) -> bool {
        matches!(self, JsonView::Array(_))
    }
    pub fn is_object(&self) -> bool {
        matches!(self, JsonView::Object(_))
    }

    /// Iterates an object value's entries as raw `(key span, value)` pairs.
    pub fn entries(&self) -> core::slice::Iter<'_, (Span, JsonView<A>)> {
        match self {
            JsonView::Object(entries) => entries.iter(),
            _ => [].iter(),
        }
    }

    /// Iterates an object value's keys, resolved against `source` and still
    /// escaped. Keys that are not valid UTF-8 are skipped.
    pub fn keys<'s>(&'s self, source: &'s [u8]) -> impl Iterator<Item = &'s str> + 's {
        self.entries().filter_map(move |(span, _)| span.resolve(source))
    }

    /// Iterates an object value's values.
    pub fn values(&self) -> impl Iterator<Item = &JsonView<A>> {
        self.entries().map(|(_, value)| value)
    }

    fn write_layout<W: fmt::Write>(
        &self,
        source: &[u8],
        writer: &mut W,
        layout: Layout,
        depth: usize,
    ) -> fmt::Result {
        match self {
            JsonView::Null => writer.write_str("null"),
            JsonView::Boolean(value) => writer.write_str(if *value { "true" } else { "false" }),
            JsonView::Integer(value) => write!(writer, "{value}"),
            JsonView::Unsigned(value) => write!(writer, "{value}"),
            JsonView::Float(value) => write_number(writer, *value),
            JsonView::String(span) => write_raw_string(writer, source, *span),
            JsonView::Array(items) => {
                if items.is_empty() {
                    return writer.write_str("[]");
                }
                writer.write_char('[')?;
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        writer.write_char(',')?;
                    }
                    if let Layout::Pretty { indent } = layout {
                        write_indent(writer, indent, depth + 1)?;
                    }
                    item.write_layout(source, writer, layout, depth + 1)?;
                }
                if let Layout::Pretty { indent } = layout {
                    write_indent(writer, indent, depth)?;
                }
                writer.write_char(']')
            }
            JsonView::Object(entries) => {
                if entries.is_empty() {
                    return writer.write_str("{}");
                }
                writer.write_char('{')?;
                for (index, (key, value)) in entries.iter().enumerate() {
                    if index > 0 {
                        writer.write_char(',')?;
                    }
                    if let Layout::Pretty { indent } = layout {
                        write_indent(writer, indent, depth + 1)?;
                    }
                    write_raw_string(writer, source, *key)?;
                    writer.write_str(match layout {
                        Layout::Compact => ":",
                        Layout::Pretty { .. } => ": ",
                    })?;
                    value.write_layout(source, writer, layout, depth + 1)?;
                }
                if let Layout::Pretty { indent } = layout {
                    write_indent(writer, indent, depth)?;
                }
                writer.write_char('}')
            }
        }
    }

    /// Writes this view as compact JSON, resolving spans against `source`.
    pub fn write_json<W: fmt::Write>(&self, source: &[u8], writer: &mut W) -> fmt::Result {
        self.write_layout(source, writer, Layout::Compact, 0)
    }

    /// Writes this view as JSON indented by `indent` spaces per level.
    pub fn write_json_pretty<W: fmt::Write>(
        &self,
        source: &[u8],
        writer: &mut W,
        indent: usize,
    ) -> fmt::Result {
        self.write_layout(source, writer, Layout::Pretty { indent }, 0)
    }

    /// Renders this view as compact JSON.
    pub fn to_json_string(&self, source: &[u8]) -> String {
        let mut output = String::new();
        let _ = self.write_json(source, &mut output);
        output
    }

    /// Renders this view as JSON indented by `indent` spaces per level.
    pub fn to_json_string_pretty(&self, source: &[u8], indent: usize) -> String {
        let mut output = String::new();
        let _ = self.write_json_pretty(source, &mut output, indent);
        output
    }
}

impl<A: Allocator> core::ops::Deref for JsonString<A> {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl<A: Allocator> AsRef<str> for JsonString<A> {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl<A: Allocator> fmt::Display for JsonString<A> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl<A: Allocator> Hash for JsonString<A> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_bytes().hash(state);
    }
}

impl<A: Allocator, B: Allocator> PartialOrd<JsonString<B>> for JsonString<A> {
    fn partial_cmp(&self, other: &JsonString<B>) -> Option<Ordering> {
        Some(self.as_bytes().cmp(other.as_bytes()))
    }
}

impl<A: Allocator> Ord for JsonString<A> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_bytes().cmp(other.as_bytes())
    }
}

impl core::str::FromStr for Json<Global> {
    type Err = JsonError;
    fn from_str(text: &str) -> Result<Self, JsonError> {
        parse(text.as_bytes())
    }
}

impl TryFrom<&[u8]> for Json<Global> {
    type Error = JsonError;
    fn try_from(bytes: &[u8]) -> Result<Self, JsonError> {
        parse(bytes)
    }
}

impl From<bool> for Json<Global> {
    fn from(value: bool) -> Self {
        Json::Boolean(value)
    }
}

impl From<i64> for Json<Global> {
    fn from(value: i64) -> Self {
        Json::Integer(value)
    }
}

impl From<u64> for Json<Global> {
    fn from(value: u64) -> Self {
        Json::Unsigned(value)
    }
}

impl From<f64> for Json<Global> {
    fn from(value: f64) -> Self {
        Json::Float(value)
    }
}

impl From<&str> for Json<Global> {
    fn from(value: &str) -> Self {
        Json::String(JsonString::from_str_in(value, Global))
    }
}

impl From<String> for Json<Global> {
    fn from(value: String) -> Self {
        Json::String(JsonString::from_str_in(&value, Global))
    }
}

impl FromIterator<Json<Global>> for Json<Global> {
    fn from_iter<I: IntoIterator<Item = Json<Global>>>(iter: I) -> Self {
        let mut items = Vec::new_in(Global);
        for value in iter {
            items.push(value);
        }
        Json::Array(items)
    }
}

impl FromIterator<(String, Json<Global>)> for Json<Global> {
    fn from_iter<I: IntoIterator<Item = (String, Json<Global>)>>(iter: I) -> Self {
        let mut entries = Vec::new_in(Global);
        for (key, value) in iter {
            entries.push((JsonString::from_str_in(&key, Global), value));
        }
        Json::Object(entries)
    }
}

/// Parses `source` into an immutable [`JsonView`] allocated in `allocator`.
/// Spans in the result are offsets into `source`, so keep `source` alive.
pub fn view_in<A: Allocator + Clone>(
    source: &[u8],
    allocator: A,
) -> Result<JsonView<A>, JsonError> {
    Parser::<A, ViewBuilder>::new(source, allocator).parse_document()
}

/// Parses `source` into an immutable [`JsonView`] on the global heap.
pub fn view(source: &[u8]) -> Result<JsonView<Global>, JsonError> {
    view_in(source, Global)
}

/// Parses `source` into an owned, mutable [`Json`] allocated in `allocator`,
/// decoding every string's escapes. The result borrows nothing from `source`.
pub fn parse_in<A: Allocator + Clone>(
    source: &[u8],
    allocator: A,
) -> Result<Json<A>, JsonError> {
    Parser::<A, OwnedBuilder>::new(source, allocator).parse_document()
}

/// Parses `source` into an owned, mutable [`Json`] on the global heap.
pub fn parse(source: &[u8]) -> Result<Json<Global>, JsonError> {
    parse_in(source, Global)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(feature = "std"))]
    use alloc::string::{String, ToString};

    fn is_syntax(result: Result<JsonView<Global>, JsonError>) -> bool {
        matches!(result, Err(JsonError::Syntax { .. }))
    }

    #[test]
    fn scalars() {
        assert!(matches!(view(b"null").unwrap(), JsonView::Null));
        assert_eq!(view(b"true").unwrap().as_bool(), Some(true));
        assert_eq!(view(b"false").unwrap().as_bool(), Some(false));
        assert_eq!(view(b"42").unwrap().as_u64(), Some(42));
        assert_eq!(view(b"-7").unwrap().as_i64(), Some(-7));
        assert_eq!(view(br#""hi""#).unwrap().as_str(br#""hi""#), Some("hi"));
    }

    #[test]
    fn number_widths_and_signs() {
        assert_eq!(view(b"0").unwrap().as_u64(), Some(0));
        assert_eq!(view(b"-0").unwrap().as_i64(), Some(0));
        assert_eq!(view(b"-123").unwrap().as_i64(), Some(-123));
        // Beyond the 2^53 float boundary but within u64.
        assert_eq!(
            view(b"20000000000000001").unwrap().as_u64(),
            Some(20_000_000_000_000_001)
        );
        // Wider than u64 falls back to a float.
        let huge = view(b"99999999999999999999999999").unwrap();
        assert!(matches!(huge, JsonView::Float(_)));
        assert!(huge.as_u64().is_none());
    }

    #[test]
    fn floats_and_exponents() {
        assert_eq!(view(b"2.5").unwrap().as_f64(), Some(2.5));
        assert_eq!(view(b"1E5").unwrap().as_f64(), Some(100000.0));
        assert_eq!(view(b"2.5e-3").unwrap().as_f64(), Some(0.0025));
        assert_eq!(view(b"1e+10").unwrap().as_f64(), Some(1e10));
        // Integers widen to floats through as_f64.
        assert_eq!(view(b"7").unwrap().as_f64(), Some(7.0));
    }

    #[test]
    fn nested_and_empty_containers() {
        let source = br#"{ "a": [1, 2, {"b": null}], "c": {} }"#;
        let document = view(source).unwrap();
        let array = document.get(source, "a").unwrap();
        assert_eq!(array.len(), 3);
        assert_eq!(array.iter().next().unwrap().as_u64(), Some(1));
        assert!(document.get(source, "c").unwrap().is_empty());
        assert!(view(b"[]").unwrap().is_empty());
        assert!(view(b"{}").unwrap().is_empty());
    }

    #[test]
    fn whitespace_is_skipped() {
        let source = b"  \n\t [ 1 , 2 ]\r\n ";
        assert_eq!(view(source).unwrap().len(), 2);
    }

    #[test]
    fn unicode_and_surrogate_escapes() {
        // Multi-byte UTF-8 passes through literally.
        let naive = "\"naïve\"".as_bytes();
        assert_eq!(parse(naive).unwrap().as_str(), Some("naïve"));
        // A \u escape decodes to the same code point.
        let accented = br#""na\u00efve""#;
        assert_eq!(parse(accented).unwrap().as_str(), Some("naïve"));
        // A surrogate pair decodes to an astral code point.
        let grinning = br#""\uD83D\uDE00""#;
        assert_eq!(parse(grinning).unwrap().as_str(), Some("😀"));
        // And the literal emoji passes through too.
        let literal = "\"😀\"".as_bytes();
        assert_eq!(parse(literal).unwrap().as_str(), Some("😀"));
    }

    #[test]
    fn simple_escapes_decode() {
        let source = br#""a\n\t\r\"\\\/b""#;
        let mut buffer = Vec::new();
        view(source)
            .unwrap()
            .unescape_into(source, &mut buffer)
            .unwrap()
            .unwrap();
        assert_eq!(buffer.as_slice(), b"a\n\t\r\"\\/b");
        // The view keeps the raw, still-escaped bytes.
        assert_eq!(
            view(source).unwrap().as_str(source),
            Some("a\\n\\t\\r\\\"\\\\\\/b")
        );
    }

    #[test]
    fn rejects_bad_numbers() {
        assert!(is_syntax(view(b"007")));
        assert!(is_syntax(view(b"01")));
        assert!(is_syntax(view(b".5")));
        assert!(is_syntax(view(b"1.")));
        assert!(is_syntax(view(b"1e")));
        assert!(is_syntax(view(b"1e+")));
        assert!(is_syntax(view(b"+5")));
        assert!(is_syntax(view(b"1.2.3")));
        assert!(is_syntax(view(b"-")));
    }

    #[test]
    fn rejects_trailing_content() {
        assert!(is_syntax(view(b"{} x")));
        assert!(is_syntax(view(b"1 2")));
        assert!(is_syntax(view(b"null null")));
        assert!(is_syntax(view(b"[1][2]")));
    }

    #[test]
    fn rejects_bad_strings() {
        assert!(is_syntax(view(b"\"raw\x01control\"")));
        assert!(is_syntax(view(br#""bad\x""#)));
        assert!(is_syntax(view(br#""\u12""#)));
        assert!(is_syntax(view(br#""\uD83D""#))); // lone high surrogate
        assert!(is_syntax(view(br#""\uDE00""#))); // lone low surrogate
        assert!(is_syntax(view(br#""unterminated"#)));
    }

    #[test]
    fn rejects_structural_faults() {
        assert!(is_syntax(view(b"[1, 2")));
        assert!(is_syntax(view(br#"{"a": 1"#)));
        assert!(is_syntax(view(b"[1,]")));
        assert!(is_syntax(view(br#"{"a":1,}"#)));
        assert!(is_syntax(view(b"[,]")));
        assert!(is_syntax(view(b"")));
        assert!(is_syntax(view(b"   ")));
    }

    #[test]
    fn rejects_excessive_depth() {
        let mut source = String::new();
        for _ in 0..(MAX_DEPTH as usize + 5) {
            source.push('[');
        }
        for _ in 0..(MAX_DEPTH as usize + 5) {
            source.push(']');
        }
        assert!(is_syntax(view(source.as_bytes())));
    }

    #[test]
    fn view_get_is_escape_aware() {
        let source = br#"{"na\u00efve": 1, "plain": 2}"#;
        let document = view(source).unwrap();
        assert_eq!(
            document.get(source, "naïve").and_then(|v| v.as_u64()),
            Some(1)
        );
        assert_eq!(
            document.get(source, "plain").and_then(|v| v.as_u64()),
            Some(2)
        );
        assert!(document.get(source, "missing").is_none());
    }

    #[test]
    fn view_sort_then_binary_search() {
        let source = br#"{"c": 3, "a": 1, "b": 2}"#;
        let mut document = view(source).unwrap();
        document.sort_keys(source);
        assert_eq!(
            document.get_sorted(source, "a").and_then(|v| v.as_u64()),
            Some(1)
        );
        assert_eq!(
            document.get_sorted(source, "b").and_then(|v| v.as_u64()),
            Some(2)
        );
        assert_eq!(
            document.get_sorted(source, "c").and_then(|v| v.as_u64()),
            Some(3)
        );
        assert!(document.get_sorted(source, "z").is_none());
    }

    #[test]
    fn strings_must_be_valid_utf8() {
        // A raw 0xFF byte is rejected by both paths, in a value and in a key.
        assert!(matches!(view(b"\"\xff\""), Err(JsonError::Syntax { .. })));
        assert!(matches!(parse(b"\"\xff\""), Err(JsonError::Syntax { .. })));
        assert!(matches!(view(b"{\"\xff\":1}"), Err(JsonError::Syntax { .. })));
        // A truncated multi-byte sequence before the closing quote is rejected.
        assert!(matches!(parse(b"\"a\xc3\""), Err(JsonError::Syntax { .. })));
        // Well-formed multi-byte UTF-8 is accepted.
        assert_eq!(parse("\"café\"".as_bytes()).unwrap().as_str(), Some("café"));
    }

    #[test]
    fn owned_needs_no_source() {
        let source = br#"{"greeting": "a\nb", "count": 5}"#;
        let document = parse(source).unwrap();
        assert_eq!(
            document.get("greeting").and_then(|v| v.as_str()),
            Some("a\nb")
        );
        assert_eq!(document.get("count").and_then(|v| v.as_u64()), Some(5));
    }

    #[test]
    fn owned_mutation() {
        let mut document = Json::object_in(Global);
        assert!(document.insert("x", Json::from(1i64)).is_none());
        assert!(document.insert("y", Json::from(2i64)).is_none());
        // Overwriting returns the old value.
        assert_eq!(
            document.insert("x", Json::from(9i64)),
            Some(Json::from(1i64))
        );
        assert_eq!(document.get("x"), Some(&Json::from(9i64)));
        assert_eq!(document.remove("y"), Some(Json::from(2i64)));
        assert!(document.get("y").is_none());
        assert!(document.remove("y").is_none());

        let mut array = Json::array_in(Global);
        assert!(array.push(Json::from(true)));
        assert!(array.push(Json::from("hi")));
        assert_eq!(array.len(), 2);
        assert_eq!(array.pop(), Some(Json::from("hi")));
        // Mutators are no-ops on the wrong variant.
        assert!(!Json::from(1i64).push(Json::Null));
    }

    #[test]
    fn owned_from_iterators_and_conversions() {
        let array = Json::from_iter([Json::from(1u64), Json::from(2u64), Json::from(3u64)]);
        assert_eq!(array.len(), 3);
        assert_eq!(array.as_array().unwrap()[1], Json::from(2u64));

        let object = Json::from_iter([
            (String::from("name"), Json::from("tape")),
            (String::from("size"), Json::from(3u64)),
        ]);
        assert_eq!(object.get("name").and_then(|v| v.as_str()), Some("tape"));
        assert_eq!(Json::from(2.5f64).as_f64(), Some(2.5));
    }

    #[test]
    fn view_to_owned_round_trip() {
        let source = br#"{"items": [1, "a\tb"], "flag": true}"#;
        let view = view(source).unwrap();
        let owned = view.to_json(source).unwrap();
        assert_eq!(owned.get("flag").and_then(|v| v.as_bool()), Some(true));
        let items = owned.get("items").unwrap();
        assert_eq!(items.as_array().unwrap()[1].as_str(), Some("a\tb"));
    }

    #[test]
    fn error_display_is_readable() {
        #[cfg(feature = "std")]
        {
            let error = JsonError::Syntax {
                offset: 7,
                kind: SyntaxKind::InvalidNumber,
            };
            assert_eq!(format!("{error}"), "invalid number at byte offset 7");
        }
    }

    #[test]
    fn error_kind_and_location() {
        // Kinds classify the fault.
        assert!(matches!(
            parse(b"1."),
            Err(JsonError::Syntax { kind: SyntaxKind::InvalidNumber, .. })
        ));
        assert!(matches!(
            parse(br#""bad\x""#),
            Err(JsonError::Syntax { kind: SyntaxKind::InvalidEscape, .. })
        ));
        assert!(matches!(
            parse(br#""\uDE00""#),
            Err(JsonError::Syntax { kind: SyntaxKind::LoneSurrogate, .. })
        ));
        assert!(matches!(
            parse(b"{} junk"),
            Err(JsonError::Syntax { kind: SyntaxKind::TrailingData, .. })
        ));
        assert!(matches!(
            parse(b"[1"),
            Err(JsonError::Syntax { kind: SyntaxKind::UnexpectedEnd, .. })
        ));
        // Location resolves line and column from the offset.
        let source = b"{\n  \"a\": x\n}";
        let error = parse(source).unwrap_err();
        let location = error.location(source).unwrap();
        assert_eq!(location.line, 2);
        assert_eq!(source[location.offset], b'x');
        assert!(JsonError::Allocation.location(source).is_none());
    }

    #[test]
    fn serialize_owned_round_trip() {
        let source = br#"{"a":[1,2.5,true,null],"b":"x\ny"}"#;
        let document = parse(source).unwrap();
        let rendered = document.to_string();
        let reparsed: Json = rendered.parse().unwrap();
        assert_eq!(document, reparsed);
    }

    #[test]
    fn serialize_escapes_strings() {
        let mut object = Json::object_in(Global);
        object.insert("key", Json::from("a\"b\\c\nd\te"));
        assert_eq!(object.to_string(), r#"{"key":"a\"b\\c\nd\te"}"#);
    }

    #[test]
    fn serialize_view_uses_raw_spans() {
        let source = br#"{ "m" : "a\u00e9b" , "n" : [ 1 , 2 ] }"#;
        let view = view(source).unwrap();
        // The view emits the still-escaped span verbatim, dropping only the
        // insignificant whitespace between tokens.
        assert_eq!(view.to_json_string(source), r#"{"m":"a\u00e9b","n":[1,2]}"#);
    }

    #[test]
    fn serialize_floats() {
        assert_eq!(Json::from(2.0f64).to_string(), "2.0");
        assert_eq!(Json::from(2.5f64).to_string(), "2.5");
        assert_eq!(Json::from(f64::INFINITY).to_string(), "null");
        assert_eq!(Json::from(f64::NAN).to_string(), "null");
        // Large integral floats (above 2^52) still get a `.0`, via bit-math.
        assert_eq!(Json::from(1e16).to_string(), "10000000000000000.0");
        assert_eq!(Json::from(-0.0f64).to_string(), "-0.0");
        assert_eq!(Json::from(0.5f64).to_string(), "0.5");
    }

    #[test]
    fn serialize_escapes_control_chars_in_runs() {
        // The run-based escaper still emits \u00xx for bare control characters
        // while bulk-copying the clean spans around them.
        let mut object = Json::object_in(Global);
        object.insert("k", Json::from("a\u{1}b\u{1f}c"));
        assert_eq!(object.to_string(), r#"{"k":"a\u0001b\u001fc"}"#);
    }

    #[test]
    fn pretty_printing_indents() {
        let document = parse(br#"{"a":[1,2],"b":{}}"#).unwrap();
        let expected = "{\n  \"a\": [\n    1,\n    2\n  ],\n  \"b\": {}\n}";
        assert_eq!(document.to_string_pretty(2), expected);
    }

    #[test]
    fn deserializer_entry_points() {
        let from_str: Json = "[1, 2, 3]".parse().unwrap();
        assert_eq!(from_str.len(), 3);
        let from_bytes = Json::try_from(&b"true"[..]).unwrap();
        assert_eq!(from_bytes.as_bool(), Some(true));
        assert!("nope".parse::<Json>().is_err());
    }

    #[test]
    fn owned_iterators() {
        let mut document = parse(br#"{"a":1,"b":2,"c":3}"#).unwrap();
        {
            let mut keys = document.keys();
            assert_eq!(keys.next(), Some("a"));
            assert_eq!(keys.next(), Some("b"));
            assert_eq!(keys.next(), Some("c"));
            assert_eq!(keys.next(), None);
        }

        let sum: u64 = document.values().filter_map(|v| v.as_u64()).sum();
        assert_eq!(sum, 6);

        for value in document.values_mut() {
            if let Json::Unsigned(number) = value {
                *number += 10;
            }
        }
        assert_eq!(document.get("a").and_then(|v| v.as_u64()), Some(11));

        let mut array = parse(b"[1, 2, 3]").unwrap();
        for value in array.iter_mut() {
            if let Json::Unsigned(number) = value {
                *number *= 2;
            }
        }
        assert_eq!(array.as_array().unwrap()[0].as_u64(), Some(2));
        assert_eq!(array.into_array().unwrap().len(), 3);
    }

    #[test]
    fn view_iterators() {
        let source = br#"{"x":1,"y":2}"#;
        let view = view(source).unwrap();
        let mut keys = view.keys(source);
        assert_eq!(keys.next(), Some("x"));
        assert_eq!(keys.next(), Some("y"));
        assert_eq!(keys.next(), None);
        assert_eq!(view.values().count(), 2);
        assert_eq!(view.entries().len(), 2);
    }

    #[test]
    fn predicates() {
        assert!(view(b"null").unwrap().is_null());
        assert!(view(b"true").unwrap().is_boolean());
        assert!(view(b"1.5").unwrap().is_number());
        assert!(view(br#""s""#).unwrap().is_string());
        assert!(view(b"[]").unwrap().is_array());
        assert!(view(b"{}").unwrap().is_object());
        let owned = parse(b"[1]").unwrap();
        assert!(owned.is_array() && !owned.is_object());
    }

    #[test]
    fn indexing_chains_without_panicking() {
        let document = parse(br#"{"list":[10,20,30],"nested":{"deep":true}}"#).unwrap();
        // Infix indexing, serde_json style.
        assert_eq!(document["list"][1].as_u64(), Some(20));
        assert_eq!(document["nested"]["deep"].as_bool(), Some(true));
        // A miss anywhere in the chain yields Null instead of panicking.
        assert!(document["absent"].is_null());
        assert!(document["list"][99].is_null());
        assert!(document["list"]["not-an-object"].is_null());
        assert!(document["nested"]["missing"]["deeper"].is_null());
    }

    #[test]
    fn get_accepts_key_or_position() {
        let document = parse(br#"{"items":[7,8,9]}"#).unwrap();
        assert_eq!(document.get("items").and_then(|v| v.get(2)).and_then(|v| v.as_u64()), Some(9));
        assert!(document.get("items").unwrap().get(5).is_none());
        assert!(document.get("missing").is_none());
    }

    #[test]
    fn json_pointer() {
        let document = parse(br#"{"a":{"b":[{"c":42}]},"x/y":1}"#).unwrap();
        assert_eq!(document.pointer("/a/b/0/c").and_then(|v| v.as_u64()), Some(42));
        assert_eq!(document.pointer("").map(|v| v.is_object()), Some(true));
        // `~1` decodes to `/` in a reference token per RFC 6901.
        assert_eq!(document.pointer("/x~1y").and_then(|v| v.as_u64()), Some(1));
        assert!(document.pointer("/a/b/9").is_none());
        assert!(document.pointer("no-leading-slash").is_none());
        // RFC 6901: array indices may not have leading zeros.
        assert!(document.pointer("/a/b/01").is_none());
        assert_eq!(document.pointer("/a/b/0/c").and_then(|v| v.as_u64()), Some(42));
    }

    #[test]
    fn unescape_into_never_panics_on_bad_span() {
        // A hand-built span the parser would never produce: a lone trailing
        // backslash. It must be reported, not panicked on.
        let source = b"x\\";
        let span = Span { start: 1, len: 1 };
        let mut output = Vec::new();
        assert!(matches!(
            unescape_into(source, span, &mut output),
            Err(JsonError::Syntax { .. })
        ));
        // A truncated `\u` escape and a lone surrogate likewise report.
        let mut output = Vec::new();
        assert!(unescape_into(b"\\u12", Span { start: 0, len: 4 }, &mut output).is_err());
        let mut output = Vec::new();
        assert!(unescape_into(b"\\uDE00", Span { start: 0, len: 6 }, &mut output).is_err());
        // A well-formed span still decodes cleanly.
        let mut output = Vec::new();
        unescape_into(b"a\\nb", Span { start: 0, len: 4 }, &mut output).unwrap();
        assert_eq!(output.as_slice(), b"a\nb");
    }

    #[test]
    fn json_string_behaves_like_str() {
        let document = parse(br#"{"greeting":"hi"}"#).unwrap();
        let entries = document.as_object().unwrap();
        let key = &entries[0].0;
        assert_eq!(key.len(), 8);
        assert!(key.starts_with("greet"));
        assert_eq!(key.to_string(), "greeting");
    }

    /// A distinct allocator type that just forwards to the global heap, used to
    /// prove the ergonomics work for any allocator, not only `Global`.
    #[derive(Clone, Copy)]
    struct Passthrough;

    unsafe impl Allocator for Passthrough {
        fn allocate(
            &self,
            layout: core::alloc::Layout,
        ) -> Result<core::ptr::NonNull<[u8]>, allocator_api2::alloc::AllocError> {
            Global.allocate(layout)
        }
        unsafe fn deallocate(&self, ptr: core::ptr::NonNull<u8>, layout: core::alloc::Layout) {
            Global.deallocate(ptr, layout)
        }
    }

    #[test]
    fn indexing_works_for_any_allocator() {
        let document = parse_in(br#"{"k":[1,2]}"#, Passthrough).unwrap();
        // The infix `[]` sugar and its Null-on-miss fallback are not limited to
        // `Json<Global>`; the `Null` sentinel promotes for every allocator.
        assert_eq!(document["k"][1].as_u64(), Some(2));
        assert!(document["missing"].is_null());
    }
}
