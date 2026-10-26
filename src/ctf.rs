//! Reading a CTF 1.8 trace, the format LTTng writes, into this tool's events.
//!
//! An import, not a source (`docs/DECISIONS.md`, boundary 6): LTTng-UST is fast
//! because it writes in-line into its own buffer without a trap, which is also
//! why its events do not exist for an eBPF program. What it recorded is read
//! after the fact and merged into a stored run, where `omn join` meets it
//! with this tool's own events on the thread and the clock.
//!
//! The reader covers what a TSDL metadata file declares for LTTng and
//! barectf: integers of any width and byte order, enumerations, strings,
//! floating point, structures, variants, fixed arrays and sequences, the
//! packetized and the plain metadata, and the compact timestamps an event
//! header carries as the low bits of its clock. Nothing is guessed: a
//! declaration it does not know is an error naming it, and a packet it cannot
//! decode is counted, never skipped silently.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use crate::event::{Attr, AttrValue, CollectorId, Event, KeyValue};

/// The magic number a packetized metadata stream's packets start with.
const METADATA_MAGIC: u32 = 0x75d1_1d57;
/// Bytes of a metadata packet's header: magic, uuid, checksum, content and
/// packet sizes, then five one-byte fields.
const METADATA_HEADER_BYTES: usize = 37;
/// The magic number a data packet's header carries, when it declares one.
const PACKET_MAGIC: u64 = 0xc1fc_1fc1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CtfError(String);

impl CtfError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for CtfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CtfError {}

type Result<T> = std::result::Result<T, CtfError>;

// ---------------------------------------------------------------------------
// The metadata's types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Type {
    Int(Int),
    Float {
        bits: u32,
        align: u32,
        little: Option<bool>,
    },
    Str,
    Enum {
        int: Int,
        labels: Vec<(String, i128, i128)>,
    },
    Struct {
        fields: Vec<(String, Type)>,
        align: u32,
    },
    Variant {
        tag: String,
        options: Vec<(String, Type)>,
    },
    Array {
        element: Box<Type>,
        len: usize,
    },
    Sequence {
        element: Box<Type>,
        len: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
struct Int {
    bits: u32,
    align: u32,
    signed: bool,
    /// `None` is the trace's byte order, which a type declared before the
    /// `trace` block cannot know yet.
    little: Option<bool>,
    /// An integer that holds text: `char` arrays and sequences are text.
    text: bool,
    /// The clock this integer's value is a reading of.
    clock: Option<String>,
}

impl Type {
    /// The integer, when this is a byte that holds text.
    fn filter_text(&self) -> Option<&Int> {
        match self {
            Type::Int(int) if int.text && int.bits == 8 => Some(int),
            _ => None,
        }
    }

    fn align(&self) -> u32 {
        match self {
            Type::Int(int) | Type::Enum { int, .. } => int.align,
            Type::Float { align, .. } => *align,
            Type::Str => 8,
            Type::Struct { fields, align } => fields
                .iter()
                .map(|(_, field)| field.align())
                .fold(*align, u32::max),
            // Aligned as the option chosen, which is only known at the read.
            Type::Variant { .. } => 1,
            Type::Array { element, .. } | Type::Sequence { element, .. } => element.align(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Clock {
    name: String,
    freq: u64,
    precision: u64,
}

#[derive(Debug, Clone, PartialEq)]
struct EventClass {
    name: String,
    stream_id: u64,
    context: Option<Type>,
    fields: Option<Type>,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct StreamClass {
    packet_context: Option<Type>,
    event_header: Option<Type>,
    event_context: Option<Type>,
}

/// What a trace's metadata declares.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Metadata {
    little: bool,
    packet_header: Option<Type>,
    clocks: Vec<Clock>,
    streams: BTreeMap<u64, StreamClass>,
    /// By stream class, then by id.
    events: BTreeMap<(u64, u64), EventClass>,
    /// The `env` block: what the tracer said about itself and the traced
    /// program (`tracer_name`, `procname`, `vpid`, `hostname`).
    pub env: BTreeMap<String, String>,
}

// ---------------------------------------------------------------------------
// The metadata's text
// ---------------------------------------------------------------------------

/// The metadata's text, whether it was written plain or in packets.
fn metadata_text(bytes: &[u8]) -> Result<String> {
    let magic = |at: usize, little: bool| {
        bytes.get(at..at + 4).map(|word| {
            let word = [word[0], word[1], word[2], word[3]];
            if little {
                u32::from_le_bytes(word)
            } else {
                u32::from_be_bytes(word)
            }
        })
    };
    let little = match (magic(0, true), magic(0, false)) {
        (Some(METADATA_MAGIC), _) => true,
        (_, Some(METADATA_MAGIC)) => false,
        _ => {
            return String::from_utf8(bytes.to_vec())
                .map_err(|_| CtfError::new("the metadata is neither packetized nor text"));
        }
    };
    let mut text = Vec::new();
    let mut at = 0;
    while at + METADATA_HEADER_BYTES <= bytes.len() {
        if magic(at, little) != Some(METADATA_MAGIC) {
            return Err(CtfError::new(format!(
                "the metadata packet at byte {at} does not start with its magic number"
            )));
        }
        let content_bits = magic(at + 24, little).unwrap_or(0) as usize;
        let packet_bits = magic(at + 28, little).unwrap_or(0) as usize;
        if bytes[at + 32] != 0 || bytes[at + 33] != 0 {
            return Err(CtfError::new(
                "the metadata is compressed or encrypted, which this reader does not undo",
            ));
        }
        let content = content_bits / 8;
        let packet = packet_bits / 8;
        if content < METADATA_HEADER_BYTES || packet < content || at + content > bytes.len() {
            return Err(CtfError::new(format!(
                "the metadata packet at byte {at} declares sizes its file does not hold"
            )));
        }
        text.extend_from_slice(&bytes[at + METADATA_HEADER_BYTES..at + content]);
        at += packet;
    }
    String::from_utf8(text).map_err(|_| CtfError::new("the metadata's text is not UTF-8"))
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Ident(String),
    Number(i128),
    Str(String),
    Punct(&'static str),
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Token::Ident(ident) => f.write_str(ident),
            Token::Number(number) => write!(f, "{number}"),
            Token::Str(text) => write!(f, "\"{text}\""),
            Token::Punct(punct) => f.write_str(punct),
        }
    }
}

const PUNCTUATION: [&str; 15] = [
    "...", ":=", "{", "}", "(", ")", "[", "]", ";", "=", ",", "<", ">", ".", ":",
];

fn tokens(text: &str) -> Result<Vec<Token>> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let rest = &text[at..];
        let byte = bytes[at];
        if byte.is_ascii_whitespace() || byte == 0 {
            at += 1;
        } else if rest.starts_with("/*") {
            at += rest
                .find("*/")
                .map(|end| end + 2)
                .ok_or_else(|| CtfError::new("the metadata ends inside a comment"))?;
        } else if rest.starts_with("//") {
            at += rest.find('\n').unwrap_or(rest.len());
        } else if byte == b'"' {
            let mut value = String::new();
            let mut chars = rest[1..].char_indices();
            let end = loop {
                match chars.next() {
                    Some((index, '"')) => break index + 2,
                    Some((_, '\\')) => match chars.next() {
                        Some((_, 'n')) => value.push('\n'),
                        Some((_, 't')) => value.push('\t'),
                        Some((_, other)) => value.push(other),
                        None => return Err(CtfError::new("the metadata ends inside a string")),
                    },
                    Some((_, other)) => value.push(other),
                    None => return Err(CtfError::new("the metadata ends inside a string")),
                }
            };
            out.push(Token::Str(value));
            at += end;
        } else if byte == b'\'' {
            // A character literal is its code: `'a'`, `'\n'`.
            let (value, length) = match bytes.get(at + 1..at + 4) {
                Some([b'\\', escaped, b'\'']) => (
                    match escaped {
                        b'n' => b'\n',
                        b't' => b'\t',
                        b'0' => 0,
                        other => *other,
                    },
                    4,
                ),
                Some([character, b'\'', _]) => (*character, 3),
                _ => return Err(CtfError::new("the metadata holds a malformed character")),
            };
            out.push(Token::Number(i128::from(value)));
            at += length;
        } else if byte.is_ascii_digit()
            || (matches!(byte, b'-' | b'+') && bytes.get(at + 1).is_some_and(u8::is_ascii_digit))
        {
            let negative = byte == b'-';
            let start = at + usize::from(matches!(byte, b'-' | b'+'));
            let mut end = start;
            while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
                end += 1;
            }
            let literal = text[start..end].trim_end_matches(['u', 'U', 'l', 'L']);
            let value = if let Some(hex) = literal
                .strip_prefix("0x")
                .or_else(|| literal.strip_prefix("0X"))
            {
                i128::from_str_radix(hex, 16)
            } else if literal.len() > 1 && literal.starts_with('0') {
                i128::from_str_radix(&literal[1..], 8)
            } else {
                literal.parse()
            }
            .map_err(|_| CtfError::new(format!("`{}` is not a number", &text[at..end])))?;
            out.push(Token::Number(if negative { -value } else { value }));
            at = end;
        } else if byte.is_ascii_alphabetic() || byte == b'_' {
            let mut end = at;
            while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
                end += 1;
            }
            out.push(Token::Ident(text[at..end].to_owned()));
            at = end;
        } else if let Some(punct) = PUNCTUATION.iter().find(|punct| rest.starts_with(**punct)) {
            out.push(Token::Punct(punct));
            at += punct.len();
        } else {
            return Err(CtfError::new(format!(
                "the metadata holds `{}`, which TSDL does not",
                rest.chars().next().unwrap_or_default()
            )));
        }
    }
    Ok(out)
}

/// A value on the right of `=`: a number, a string, or a dotted name
/// (`clock.monotonic.value`, `le`).
#[derive(Debug, Clone, PartialEq)]
enum Value {
    Number(i128),
    Text(String),
}

impl Value {
    fn number(&self, what: &str) -> Result<i128> {
        match self {
            Value::Number(number) => Ok(*number),
            Value::Text(text) => match text.as_str() {
                "true" | "TRUE" => Ok(1),
                "false" | "FALSE" => Ok(0),
                _ => Err(CtfError::new(format!("`{what}` is `{text}`, not a number"))),
            },
        }
    }

    fn text(&self) -> String {
        match self {
            Value::Number(number) => number.to_string(),
            Value::Text(text) => text.clone(),
        }
    }
}

struct Parser {
    tokens: Vec<Token>,
    at: usize,
    aliases: BTreeMap<String, Type>,
    structs: BTreeMap<String, Type>,
    variants: BTreeMap<String, Type>,
    enums: BTreeMap<String, Type>,
    metadata: Metadata,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn peek_is(&self, punct: &str) -> bool {
        matches!(self.peek(), Some(Token::Punct(p)) if *p == punct)
    }

    fn next(&mut self) -> Result<Token> {
        let token = self
            .tokens
            .get(self.at)
            .cloned()
            .ok_or_else(|| CtfError::new("the metadata ends in the middle of a declaration"))?;
        self.at += 1;
        Ok(token)
    }

    fn expect(&mut self, punct: &str) -> Result<()> {
        match self.next()? {
            Token::Punct(p) if p == punct => Ok(()),
            other => Err(CtfError::new(format!(
                "the metadata has `{other}` where `{punct}` belongs"
            ))),
        }
    }

    fn eat(&mut self, punct: &str) -> bool {
        let found = self.peek_is(punct);
        if found {
            self.at += 1;
        }
        found
    }

    fn ident(&mut self) -> Result<String> {
        match self.next()? {
            Token::Ident(ident) => Ok(ident),
            other => Err(CtfError::new(format!(
                "the metadata has `{other}` where a name belongs"
            ))),
        }
    }

    /// `a.b.c`, joined.
    fn dotted(&mut self) -> Result<String> {
        let mut name = self.ident()?;
        while self.eat(".") {
            name.push('.');
            name.push_str(&self.ident()?);
        }
        Ok(name)
    }

    fn value(&mut self) -> Result<Value> {
        match self.next()? {
            Token::Number(number) => Ok(Value::Number(number)),
            Token::Str(text) => Ok(Value::Text(text)),
            Token::Ident(mut name) => {
                while self.eat(".") {
                    name.push('.');
                    name.push_str(&self.ident()?);
                }
                Ok(Value::Text(name))
            }
            other => Err(CtfError::new(format!(
                "the metadata has `{other}` where a value belongs"
            ))),
        }
    }

    fn parse(mut self) -> Result<Metadata> {
        while self.peek().is_some() {
            let keyword = self.ident()?;
            match keyword.as_str() {
                "typealias" => self.typealias()?,
                "typedef" => {
                    let declared = self.type_specifier()?;
                    let (name, declared) = self.declarator(declared)?;
                    self.aliases.insert(name, declared);
                }
                "trace" => self.block(|parser, key| parser.trace_entry(key))?,
                "env" => {
                    let mut env = BTreeMap::new();
                    self.block(|parser, key| {
                        parser.expect("=")?;
                        env.insert(key, parser.value()?.text());
                        Ok(())
                    })?;
                    self.metadata.env = env;
                }
                "clock" => {
                    let mut clock = Clock {
                        name: String::new(),
                        freq: 1_000_000_000,
                        precision: 0,
                    };
                    self.block(|parser, key| {
                        parser.expect("=")?;
                        let value = parser.value()?;
                        match key.as_str() {
                            "name" => clock.name = value.text(),
                            "freq" => clock.freq = value.number("freq")? as u64,
                            "precision" => clock.precision = value.number("precision")? as u64,
                            _ => {}
                        }
                        Ok(())
                    })?;
                    self.metadata.clocks.push(clock);
                }
                "stream" => {
                    let mut id = 0;
                    let mut class = StreamClass::default();
                    self.block(|parser, key| {
                        if parser.eat(":=") {
                            let declared = parser.type_specifier()?;
                            match key.as_str() {
                                "packet.context" => class.packet_context = Some(declared),
                                "event.header" => class.event_header = Some(declared),
                                "event.context" => class.event_context = Some(declared),
                                _ => {}
                            }
                        } else {
                            parser.expect("=")?;
                            let value = parser.value()?;
                            if key == "id" {
                                id = value.number("id")? as u64;
                            }
                        }
                        Ok(())
                    })?;
                    self.metadata.streams.insert(id, class);
                }
                "event" => {
                    let mut class = EventClass {
                        name: String::new(),
                        stream_id: 0,
                        context: None,
                        fields: None,
                    };
                    let mut id = 0;
                    self.block(|parser, key| {
                        if parser.eat(":=") {
                            let declared = parser.type_specifier()?;
                            match key.as_str() {
                                "context" => class.context = Some(declared),
                                "fields" => class.fields = Some(declared),
                                _ => {}
                            }
                        } else {
                            parser.expect("=")?;
                            let value = parser.value()?;
                            match key.as_str() {
                                "name" => class.name = value.text(),
                                "id" => id = value.number("id")? as u64,
                                "stream_id" => class.stream_id = value.number("stream_id")? as u64,
                                _ => {}
                            }
                        }
                        Ok(())
                    })?;
                    self.metadata.events.insert((class.stream_id, id), class);
                }
                "callsite" => self.block(|parser, _| {
                    parser.expect("=")?;
                    parser.value().map(drop)
                })?,
                // A named type declared on its own: `struct name { ... };`.
                "struct" | "variant" | "enum" | "integer" | "floating_point" | "string" => {
                    self.at -= 1;
                    self.type_specifier()?;
                    self.expect(";")?;
                }
                other => {
                    return Err(CtfError::new(format!(
                        "the metadata declares `{other}`, which this reader does not know"
                    )));
                }
            }
        }
        if self.metadata.clocks.len() > 1 {
            return Err(CtfError::new(format!(
                "the trace declares {} clocks, and which one an event is stamped with is not \
                 something this reader works out",
                self.metadata.clocks.len()
            )));
        }
        Ok(self.metadata)
    }

    /// `{ key = value; key := type; ... };`, each entry handed to `entry` with
    /// its dotted key.
    fn block(&mut self, mut entry: impl FnMut(&mut Self, String) -> Result<()>) -> Result<()> {
        self.expect("{")?;
        while !self.eat("}") {
            let key = self.dotted()?;
            entry(self, key)?;
            self.expect(";")?;
        }
        self.expect(";")
    }

    fn trace_entry(&mut self, key: String) -> Result<()> {
        if self.eat(":=") {
            let declared = self.type_specifier()?;
            if key == "packet.header" {
                self.metadata.packet_header = Some(declared);
            }
            return Ok(());
        }
        self.expect("=")?;
        let value = self.value()?;
        match key.as_str() {
            "byte_order" => {
                self.metadata.little = match value.text().as_str() {
                    "le" | "little" => true,
                    "be" | "big" | "network" => false,
                    other => {
                        return Err(CtfError::new(format!(
                            "the trace's byte order is `{other}`"
                        )));
                    }
                };
            }
            "major" if value.number("major")? != 1 => {
                return Err(CtfError::new(format!(
                    "the trace is CTF {}, and this reader reads CTF 1.8",
                    value.text()
                )));
            }
            _ => {}
        }
        Ok(())
    }

    fn typealias(&mut self) -> Result<()> {
        let declared = self.type_specifier()?;
        self.expect(":=")?;
        let mut name = Vec::new();
        while !self.peek_is(";") {
            name.push(self.ident()?);
        }
        self.expect(";")?;
        self.aliases.insert(name.join(" "), declared);
        Ok(())
    }

    fn type_specifier(&mut self) -> Result<Type> {
        let keyword = match self.peek() {
            Some(Token::Ident(ident)) => ident.clone(),
            other => {
                return Err(CtfError::new(format!(
                    "the metadata has `{}` where a type belongs",
                    other.map(ToString::to_string).unwrap_or_default()
                )));
            }
        };
        match keyword.as_str() {
            "integer" => {
                self.at += 1;
                self.integer().map(Type::Int)
            }
            "floating_point" => {
                self.at += 1;
                let (mut exp, mut mant, mut align, mut little) = (0, 0, 8, None);
                self.attributes(|key, value| {
                    match key {
                        "exp_dig" => exp = value.number(key)? as u32,
                        "mant_dig" => mant = value.number(key)? as u32,
                        "align" => align = value.number(key)? as u32,
                        "byte_order" => little = byte_order(&value)?,
                        _ => {}
                    }
                    Ok(())
                })?;
                Ok(Type::Float {
                    bits: exp + mant,
                    align,
                    little,
                })
            }
            "string" => {
                self.at += 1;
                if self.peek_is("{") {
                    self.attributes(|_, _| Ok(()))?;
                }
                Ok(Type::Str)
            }
            "enum" => {
                self.at += 1;
                self.enumeration()
            }
            "struct" => {
                self.at += 1;
                self.structure()
            }
            "variant" => {
                self.at += 1;
                self.variant()
            }
            _ => {
                // An alias's name, which may be several words (`unsigned
                // long`): the longest one declared.
                let mut words = Vec::new();
                let mut found = None;
                let mut at = self.at;
                while let Some(Token::Ident(word)) = self.tokens.get(at) {
                    words.push(word.clone());
                    at += 1;
                    if let Some(declared) = self.aliases.get(&words.join(" ")) {
                        found = Some((at, declared.clone()));
                    }
                }
                let (end, declared) = found.ok_or_else(|| {
                    CtfError::new(format!(
                        "the metadata uses the type `{keyword}`, never declared"
                    ))
                })?;
                self.at = end;
                Ok(declared)
            }
        }
    }

    fn attributes(&mut self, mut each: impl FnMut(&str, Value) -> Result<()>) -> Result<()> {
        self.expect("{")?;
        while !self.eat("}") {
            let key = self.ident()?;
            self.expect("=")?;
            let value = self.value()?;
            each(&key, value)?;
            self.expect(";")?;
        }
        Ok(())
    }

    fn integer(&mut self) -> Result<Int> {
        let mut int = Int {
            bits: 0,
            align: 0,
            signed: false,
            little: None,
            text: false,
            clock: None,
        };
        self.attributes(|key, value| {
            match key {
                "size" => int.bits = value.number(key)? as u32,
                "align" => int.align = value.number(key)? as u32,
                "signed" => int.signed = value.number(key)? != 0,
                "byte_order" => int.little = byte_order(&value)?,
                "encoding" => int.text = !matches!(value.text().as_str(), "none" | "NONE"),
                "map" => {
                    let text = value.text();
                    int.clock = text
                        .strip_prefix("clock.")
                        .and_then(|rest| rest.strip_suffix(".value"))
                        .map(str::to_owned);
                }
                _ => {}
            }
            Ok(())
        })?;
        if int.bits == 0 || int.bits > 64 {
            return Err(CtfError::new(format!(
                "an integer of {} bits, where 1 to 64 are read",
                int.bits
            )));
        }
        if int.align == 0 {
            int.align = if int.bits % 8 == 0 { 8 } else { 1 };
        }
        Ok(int)
    }

    fn enumeration(&mut self) -> Result<Type> {
        let name = match self.peek() {
            Some(Token::Ident(name)) => {
                let name = name.clone();
                self.at += 1;
                Some(name)
            }
            _ => None,
        };
        if !self.peek_is(":") && !self.peek_is("{") {
            let name = name.unwrap_or_default();
            return self.enums.get(&name).cloned().ok_or_else(|| {
                CtfError::new(format!("the enumeration `{name}` is never declared"))
            });
        }
        let int = if self.eat(":") {
            match self.type_specifier()? {
                Type::Int(int) => int,
                other => {
                    return Err(CtfError::new(format!(
                        "an enumeration over {other:?}, where an integer belongs"
                    )));
                }
            }
        } else {
            match self.aliases.get("int") {
                Some(Type::Int(int)) => int.clone(),
                _ => {
                    return Err(CtfError::new(
                        "an enumeration names no integer, and `int` is never declared",
                    ));
                }
            }
        };
        self.expect("{")?;
        let mut labels = Vec::new();
        let mut next = 0i128;
        while !self.eat("}") {
            let label = match self.next()? {
                Token::Ident(label) | Token::Str(label) => label,
                other => {
                    return Err(CtfError::new(format!(
                        "the metadata has `{other}` where an enumeration's label belongs"
                    )));
                }
            };
            let (low, high) = if self.eat("=") {
                let low = self.value()?.number(&label)?;
                let high = if self.eat("...") {
                    self.value()?.number(&label)?
                } else {
                    low
                };
                (low, high)
            } else {
                (next, next)
            };
            next = high + 1;
            labels.push((label, low, high));
            if !self.eat(",") {
                self.expect("}")?;
                break;
            }
        }
        let declared = Type::Enum { int, labels };
        if let Some(name) = name {
            self.enums.insert(name, declared.clone());
        }
        Ok(declared)
    }

    fn structure(&mut self) -> Result<Type> {
        let name = match self.peek() {
            Some(Token::Ident(name)) => {
                let name = name.clone();
                self.at += 1;
                Some(name)
            }
            _ => None,
        };
        if !self.peek_is("{") {
            let name = name.unwrap_or_default();
            return self
                .structs
                .get(&name)
                .cloned()
                .ok_or_else(|| CtfError::new(format!("the structure `{name}` is never declared")));
        }
        let fields = self.fields()?;
        let mut align = 1;
        if matches!(self.peek(), Some(Token::Ident(word)) if word == "align") {
            self.at += 1;
            self.expect("(")?;
            align = self.value()?.number("align")? as u32;
            self.expect(")")?;
        }
        let declared = Type::Struct { fields, align };
        if let Some(name) = name {
            self.structs.insert(name, declared.clone());
        }
        Ok(declared)
    }

    fn variant(&mut self) -> Result<Type> {
        let name = match self.peek() {
            Some(Token::Ident(name)) => {
                let name = name.clone();
                self.at += 1;
                Some(name)
            }
            _ => None,
        };
        let tag = if self.eat("<") {
            let tag = self.dotted()?;
            self.expect(">")?;
            Some(tag)
        } else {
            None
        };
        if !self.peek_is("{") {
            let name = name.unwrap_or_default();
            let mut declared =
                self.variants.get(&name).cloned().ok_or_else(|| {
                    CtfError::new(format!("the variant `{name}` is never declared"))
                })?;
            if let (Type::Variant { tag: held, .. }, Some(tag)) = (&mut declared, tag) {
                *held = tag;
            }
            return Ok(declared);
        }
        let options = self.fields()?;
        let declared = Type::Variant {
            tag: tag.unwrap_or_default(),
            options,
        };
        if let Some(name) = name {
            self.variants.insert(name, declared.clone());
        }
        Ok(declared)
    }

    /// `{ type name; type name[4]; type name[len]; ... }`.
    fn fields(&mut self) -> Result<Vec<(String, Type)>> {
        self.expect("{")?;
        let mut fields = Vec::new();
        while !self.eat("}") {
            if matches!(self.peek(), Some(Token::Ident(word)) if word == "typealias") {
                self.at += 1;
                self.typealias()?;
                continue;
            }
            let declared = self.type_specifier()?;
            loop {
                fields.push(self.declarator(declared.clone())?);
                if !self.eat(",") {
                    break;
                }
            }
            self.expect(";")?;
        }
        Ok(fields)
    }

    /// A field's name and its dimensions, innermost last.
    fn declarator(&mut self, declared: Type) -> Result<(String, Type)> {
        let name = self.ident()?;
        let mut dimensions = Vec::new();
        while self.eat("[") {
            dimensions.push(self.value()?);
            self.expect("]")?;
        }
        let mut declared = declared;
        for dimension in dimensions.into_iter().rev() {
            declared = match dimension {
                Value::Number(len) => Type::Array {
                    element: Box::new(declared),
                    len: len as usize,
                },
                Value::Text(len) => Type::Sequence {
                    element: Box::new(declared),
                    len,
                },
            };
        }
        Ok((name, declared))
    }
}

fn byte_order(value: &Value) -> Result<Option<bool>> {
    match value.text().as_str() {
        "le" | "little" => Ok(Some(true)),
        "be" | "big" | "network" => Ok(Some(false)),
        "native" => Ok(None),
        other => Err(CtfError::new(format!("a byte order of `{other}`"))),
    }
}

impl Metadata {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let text = metadata_text(bytes)?;
        if !text.trim_start().starts_with("/* CTF 1.8") && !text.contains("trace") {
            return Err(CtfError::new("the metadata is not CTF 1.8's TSDL"));
        }
        Parser {
            tokens: tokens(&text)?,
            at: 0,
            aliases: BTreeMap::new(),
            structs: BTreeMap::new(),
            variants: BTreeMap::new(),
            enums: BTreeMap::new(),
            metadata: Metadata {
                little: true,
                ..Metadata::default()
            },
        }
        .parse()
    }
}

// ---------------------------------------------------------------------------
// The data
// ---------------------------------------------------------------------------

/// A decoded field.
#[derive(Debug, Clone, PartialEq)]
enum Field {
    Uint(u64),
    Int(i64),
    Float(f64),
    Text(String),
    /// An enumeration's label, beside its number.
    Label(String, i128),
    Struct(Vec<(String, Field)>),
    List(Vec<Field>),
}

impl Field {
    fn number(&self) -> Option<i128> {
        match self {
            Field::Uint(value) => Some(i128::from(*value)),
            Field::Int(value) => Some(i128::from(*value)),
            Field::Label(_, value) => Some(*value),
            _ => None,
        }
    }

    fn get(&self, name: &str) -> Option<&Field> {
        match self {
            Field::Struct(fields) => fields
                .iter()
                .find(|(field, _)| field == name)
                .map(|(_, value)| value),
            _ => None,
        }
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    /// In bits, from the packet's start.
    at: usize,
    /// In bits: where this packet's content ends.
    end: usize,
    little: bool,
    /// The clock, as the last integer mapped to it left it.
    clock: u64,
    /// Fields decoded so far, innermost scope last: what a variant's tag and a
    /// sequence's length are looked up in.
    scopes: Vec<Vec<(String, Field)>>,
}

impl Reader<'_> {
    fn align(&mut self, bits: u32) {
        let bits = bits.max(1) as usize;
        self.at = self.at.div_ceil(bits) * bits;
    }

    fn bits(&mut self, bits: u32, little: bool) -> Result<u64> {
        let bits = bits as usize;
        if self.at + bits > self.end {
            return Err(CtfError::new("a field runs past its packet's content"));
        }
        let first = self.at / 8;
        let last = (self.at + bits).div_ceil(8);
        let shift = self.at % 8;
        let mut word: u128 = 0;
        if little {
            for (index, byte) in self.bytes[first..last].iter().enumerate() {
                word |= u128::from(*byte) << (8 * index);
            }
            word >>= shift;
        } else {
            for byte in &self.bytes[first..last] {
                word = (word << 8) | u128::from(*byte);
            }
            word >>= (last - first) * 8 - shift - bits;
        }
        self.at += bits;
        let mask = if bits == 64 {
            u128::from(u64::MAX)
        } else {
            (1u128 << bits) - 1
        };
        Ok((word & mask) as u64)
    }

    fn int(&mut self, int: &Int) -> Result<Field> {
        self.align(int.align);
        let raw = self.bits(int.bits, int.little.unwrap_or(self.little))?;
        if int.clock.is_some() {
            self.clock = advance(self.clock, raw, int.bits);
        }
        Ok(if int.signed {
            let shift = 64 - int.bits;
            Field::Int(((raw << shift) as i64) >> shift)
        } else {
            Field::Uint(raw)
        })
    }

    fn lookup(&self, path: &str) -> Option<&Field> {
        let name = path.rsplit('.').next().unwrap_or(path);
        self.scopes.iter().rev().find_map(|scope| {
            scope
                .iter()
                .rev()
                .find(|(field, _)| field == name)
                .map(|(_, value)| value)
        })
    }

    fn read(&mut self, declared: &Type) -> Result<Field> {
        match declared {
            Type::Int(int) => self.int(int),
            Type::Float {
                bits,
                align,
                little,
            } => {
                self.align(*align);
                let raw = self.bits(*bits, little.unwrap_or(self.little))?;
                Ok(Field::Float(match bits {
                    32 => f64::from(f32::from_bits(raw as u32)),
                    64 => f64::from_bits(raw),
                    other => {
                        return Err(CtfError::new(format!(
                            "a floating point number of {other} bits"
                        )));
                    }
                }))
            }
            Type::Str => {
                self.align(8);
                let start = self.at / 8;
                let content = self.end / 8;
                let length = self.bytes[start..content.min(self.bytes.len())]
                    .iter()
                    .position(|byte| *byte == 0)
                    .ok_or_else(|| CtfError::new("a string runs past its packet's content"))?;
                self.at += (length + 1) * 8;
                Ok(Field::Text(
                    String::from_utf8_lossy(&self.bytes[start..start + length]).into_owned(),
                ))
            }
            Type::Enum { int, labels } => {
                let value = self.int(int)?.number().unwrap_or_default();
                let label = labels
                    .iter()
                    .find(|(_, low, high)| (*low..=*high).contains(&value))
                    .map(|(label, _, _)| label.clone())
                    .unwrap_or_default();
                Ok(Field::Label(label, value))
            }
            Type::Struct { .. } => {
                self.align(declared.align());
                let Type::Struct { fields, .. } = declared else {
                    unreachable!()
                };
                self.scopes.push(Vec::new());
                for (name, field) in fields {
                    let value = self.read(field);
                    let value = match value {
                        Ok(value) => value,
                        Err(error) => {
                            self.scopes.pop();
                            return Err(error);
                        }
                    };
                    if let Some(scope) = self.scopes.last_mut() {
                        scope.push((name.clone(), value));
                    }
                }
                Ok(Field::Struct(self.scopes.pop().unwrap_or_default()))
            }
            Type::Variant { tag, options } => {
                let chosen = match self.lookup(tag) {
                    Some(Field::Label(label, _)) => label.clone(),
                    _ => {
                        return Err(CtfError::new(format!(
                            "a variant's tag `{tag}` is not an enumeration read before it"
                        )));
                    }
                };
                let (_, option) = options
                    .iter()
                    .find(|(name, _)| *name == chosen)
                    .ok_or_else(|| {
                        CtfError::new(format!("a variant has no option for its tag `{chosen}`"))
                    })?;
                let option = option.clone();
                self.align(option.align());
                self.read(&option)
            }
            Type::Array { element, len } => self.list(element, *len),
            Type::Sequence { element, len } => {
                let count = self.lookup(len).and_then(Field::number).ok_or_else(|| {
                    CtfError::new(format!(
                        "a sequence's length `{len}` was not read before it"
                    ))
                })?;
                self.list(element, usize::try_from(count).unwrap_or(0))
            }
        }
    }

    fn list(&mut self, element: &Type, len: usize) -> Result<Field> {
        if let Some(int) = element.filter_text() {
            self.align(int.align);
            let start = self.at / 8;
            if self.at + len * 8 > self.end {
                return Err(CtfError::new("a text array runs past its packet's content"));
            }
            self.at += len * 8;
            let bytes = &self.bytes[start..start + len];
            let text = bytes.split(|byte| *byte == 0).next().unwrap_or_default();
            return Ok(Field::Text(String::from_utf8_lossy(text).into_owned()));
        }
        self.align(element.align());
        let mut items = Vec::with_capacity(len.min(4096));
        for _ in 0..len {
            items.push(self.read(element)?);
        }
        Ok(Field::List(items))
    }
}

/// A clock left at `clock`, and a reading of its low `bits`: the clock is
/// later, so a reading lower than the clock's own low bits wrapped.
fn advance(clock: u64, reading: u64, bits: u32) -> u64 {
    if bits >= 64 {
        return reading;
    }
    let mask = (1u64 << bits) - 1;
    let mut value = (clock & !mask) | reading;
    if reading < clock & mask {
        value = value.wrapping_add(1u64 << bits);
    }
    value
}

/// One event as the trace holds it.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    /// The clock's reading, in its own cycles.
    cycles: u64,
    name: String,
    cpu: Option<u64>,
    /// The stream context's, the event's own context's and the payload's
    /// fields, in that order.
    fields: Vec<(String, Field)>,
}

/// What one data stream file held.
#[derive(Debug, Default, PartialEq)]
struct StreamRead {
    records: Vec<Record>,
    /// The tracer's own count of events it could not write: cumulative in
    /// each packet, so the last packet's is the stream's.
    discarded: u64,
    /// Why the rest of the file could not be read, when it could not.
    undecodable: Option<String>,
}

fn read_stream(metadata: &Metadata, bytes: &[u8]) -> StreamRead {
    let mut read = StreamRead::default();
    let mut offset = 0;
    let mut clock = 0;
    while offset < bytes.len() {
        match read_packet(metadata, &bytes[offset..], clock, &mut read) {
            Ok((packet_bytes, left_at)) => {
                clock = left_at;
                offset += packet_bytes;
            }
            Err(error) => {
                read.undecodable = Some(format!("at byte {offset}: {error}"));
                break;
            }
        }
    }
    read
}

/// Reads one packet, and returns its size in bytes and the clock it ended on.
fn read_packet(
    metadata: &Metadata,
    bytes: &[u8],
    clock: u64,
    read: &mut StreamRead,
) -> Result<(usize, u64)> {
    let mut reader = Reader {
        bytes,
        at: 0,
        end: bytes.len() * 8,
        little: metadata.little,
        clock,
        scopes: Vec::new(),
    };
    let header = match &metadata.packet_header {
        Some(declared) => Some(reader.read(declared)?),
        None => None,
    };
    let magic = header.as_ref().and_then(|header| header.get("magic"));
    if magic.is_some_and(|magic| magic.number() != Some(i128::from(PACKET_MAGIC))) {
        return Err(CtfError::new(
            "a packet does not start with CTF's magic number",
        ));
    }
    let stream_id = header
        .as_ref()
        .and_then(|header| header.get("stream_id"))
        .and_then(Field::number)
        .unwrap_or(0) as u64;
    // A trace that declares no stream has one, of no context and no header.
    let implicit = StreamClass::default();
    let stream = match metadata.streams.get(&stream_id) {
        None if stream_id == 0 && metadata.streams.is_empty() => Some(&implicit),
        stream => stream,
    }
    .ok_or_else(|| {
        CtfError::new(format!(
            "a packet names stream class {stream_id}, never declared"
        ))
    })?;
    // The packet's `timestamp_end` is read like any field mapped to the
    // clock, and must not move the clock the events start from.
    let context = match &stream.packet_context {
        Some(declared) => {
            let before = reader.clock;
            let context = reader.read(declared)?;
            reader.clock = match context.get("timestamp_begin").and_then(Field::number) {
                Some(begin) => begin as u64,
                None => before,
            };
            Some(context)
        }
        None => None,
    };
    let context_number = |name: &str| {
        context
            .as_ref()
            .and_then(|context| context.get(name))
            .and_then(Field::number)
    };
    let packet_bits = context_number("packet_size").map_or(bytes.len() * 8, |bits| bits as usize);
    let content_bits = context_number("content_size").map_or(packet_bits, |bits| bits as usize);
    if packet_bits == 0 || packet_bits > bytes.len() * 8 || content_bits > packet_bits {
        return Err(CtfError::new(format!(
            "a packet declares {packet_bits} bits, {content_bits} of content, in {} bytes left",
            bytes.len()
        )));
    }
    reader.end = content_bits;
    if let Some(discarded) = context_number("events_discarded") {
        read.discarded = discarded as u64;
    }
    let cpu = context_number("cpu_id").map(|cpu| cpu as u64);

    while reader.at < reader.end {
        let header = match &stream.event_header {
            Some(declared) => Some(reader.read(declared)?),
            None => None,
        };
        // LTTng's compact and large headers say the id twice: the enumeration
        // says `extended`, and the variant's option holds the real one.
        let id = header
            .as_ref()
            .and_then(|header| {
                header
                    .get("v")
                    .and_then(|variant| variant.get("id"))
                    .or_else(|| header.get("id"))
            })
            .and_then(Field::number)
            .unwrap_or(0) as u64;
        let class = metadata.events.get(&(stream_id, id)).ok_or_else(|| {
            CtfError::new(format!(
                "an event names id {id} of stream class {stream_id}, never declared"
            ))
        })?;
        let mut fields = Vec::new();
        for declared in [&stream.event_context, &class.context, &class.fields]
            .into_iter()
            .flatten()
        {
            if let Field::Struct(read) = reader.read(declared)? {
                fields.extend(read);
            }
        }
        read.records.push(Record {
            cycles: reader.clock,
            name: class.name.clone(),
            cpu,
            fields,
        });
    }
    Ok((packet_bits / 8, reader.clock))
}

// ---------------------------------------------------------------------------
// Into this tool's events
// ---------------------------------------------------------------------------

/// What a trace directory held, as this tool's events.
#[derive(Debug, Default, PartialEq)]
pub struct Trace {
    /// In time order.
    pub events: Vec<Event>,
    /// Events the tracer counted as discarded: its buffer was full.
    pub discarded: u64,
    /// Data files the reader stopped in, with why.
    pub undecodable: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// The clock's name, as the metadata declares it.
    pub clock: String,
}

/// The source name the imported events carry.
pub const SOURCE: &str = "ctf.event";

/// Reads a CTF trace: a directory holding `metadata` and its data streams.
/// LTTng writes one such directory per tracing domain and per user or process
/// (`ust/uid/1000/64-bit/`); a directory above them is walked to the first
/// one that holds a `metadata`.
pub fn read(directory: &Path) -> std::result::Result<Trace, CtfError> {
    let traces = trace_directories(directory);
    if traces.is_empty() {
        return Err(CtfError::new(format!(
            "{} holds no CTF trace: no `metadata` file in it or below it",
            directory.display()
        )));
    }
    let mut out = Trace::default();
    for trace in traces {
        let metadata_path = trace.join("metadata");
        let bytes = std::fs::read(&metadata_path).map_err(|error| {
            CtfError::new(format!("cannot read {}: {error}", metadata_path.display()))
        })?;
        let metadata = Metadata::parse(&bytes)
            .map_err(|error| CtfError::new(format!("{}: {error}", metadata_path.display())))?;
        let clock = metadata.clocks.first().cloned().ok_or_else(|| {
            CtfError::new(format!(
                "{} declares no clock, and an event without a time cannot be placed in a run",
                metadata_path.display()
            ))
        })?;
        // LTTng's `monotonic` is the kernel's CLOCK_MONOTONIC, which is this
        // tool's one domain: its cycles are the same nanoseconds on the same
        // boot. Any other clock maps to it by nothing the trace says.
        if clock.name != "monotonic" {
            return Err(CtfError::new(format!(
                "{} stamps its events with the clock `{}`, and only LTTng's `monotonic` is \
                 CLOCK_MONOTONIC, the one clock this tool's events are on",
                metadata_path.display(),
                clock.name
            )));
        }
        let to_ns = |cycles: u64| -> u64 {
            if clock.freq == 1_000_000_000 {
                cycles
            } else {
                (u128::from(cycles) * 1_000_000_000 / u128::from(clock.freq.max(1))) as u64
            }
        };
        let ts_err_ns = u32::try_from(to_ns(clock.precision)).unwrap_or(u32::MAX);
        let mut files: Vec<_> = std::fs::read_dir(&trace)
            .map_err(|error| CtfError::new(format!("cannot list {}: {error}", trace.display())))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.is_file()
                    && path.file_name().is_some_and(|name| {
                        let name = name.to_string_lossy();
                        name != "metadata" && !name.starts_with('.')
                    })
            })
            .collect();
        files.sort();
        for file in files {
            let bytes = std::fs::read(&file).map_err(|error| {
                CtfError::new(format!("cannot read {}: {error}", file.display()))
            })?;
            let stream = read_stream(&metadata, &bytes);
            out.discarded += stream.discarded;
            if let Some(reason) = stream.undecodable {
                out.undecodable
                    .push(format!("{}: {reason}", file.display()));
            }
            for record in stream.records {
                out.events
                    .push(event(record, to_ns, ts_err_ns, &metadata.env));
            }
        }
        out.clock = clock.name;
        out.env.extend(metadata.env);
    }
    out.events.sort_by_key(|event| event.ts_ns);
    for (seq, event) in out.events.iter_mut().enumerate() {
        event.seq = seq as u64;
    }
    Ok(out)
}

fn trace_directories(directory: &Path) -> Vec<std::path::PathBuf> {
    if directory.join("metadata").is_file() {
        return vec![directory.to_path_buf()];
    }
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut below: Vec<_> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_dir())
        .collect();
    below.sort();
    below
        .iter()
        .flat_map(|path| trace_directories(path))
        .collect()
}

/// TSDL escapes a field's name with a leading underscore, so a field may be
/// called `_int`; the name meant is the one without it.
fn field_name(name: &str) -> &str {
    name.strip_prefix('_').unwrap_or(name)
}

fn event(
    record: Record,
    to_ns: impl Fn(u64) -> u64,
    ts_err_ns: u32,
    env: &BTreeMap<String, String>,
) -> Event {
    let mut keys = Vec::new();
    let mut attrs = vec![Attr {
        name: "event".to_owned(),
        value: AttrValue::Text(record.name),
    }];
    let mut tid = None;
    let mut pid = None;
    for (name, value) in &record.fields {
        let name = field_name(name);
        match (name, value.number()) {
            ("vtid" | "tid", Some(number)) => tid = Some(number as i32),
            ("vpid" | "pid", Some(number)) => pid = Some(number as i32),
            _ => {}
        }
        flatten(name, value, &mut attrs);
    }
    // A trace of one process names it in its environment, not per event.
    let pid = pid.or_else(|| env.get("vpid").and_then(|vpid| vpid.parse().ok()));
    if let Some(tid) = tid {
        keys.push(KeyValue::Task { tid });
    }
    if let Some(pid) = pid {
        keys.push(KeyValue::Proc { pidns: 0, pid });
    }
    if let Some(cpu) = record.cpu {
        keys.push(KeyValue::Cpu { cpu: cpu as u32 });
    }
    Event {
        ts_ns: to_ns(record.cycles),
        ts_err_ns,
        kind: SOURCE.to_owned(),
        keys,
        attrs,
        collector_id: CollectorId::new("ctf"),
        seq: 0,
    }
}

fn flatten(name: &str, value: &Field, attrs: &mut Vec<Attr>) {
    let value = match value {
        Field::Uint(value) => AttrValue::Uint(*value),
        Field::Int(value) => AttrValue::Int(*value),
        Field::Float(value) => AttrValue::Float(*value),
        Field::Text(text) | Field::Label(text, _) => AttrValue::Text(text.clone()),
        Field::Struct(fields) => {
            for (field, value) in fields {
                flatten(&format!("{name}.{}", field_name(field)), value, attrs);
            }
            return;
        }
        Field::List(items) => {
            for (index, item) in items.iter().enumerate() {
                flatten(&format!("{name}[{index}]"), item, attrs);
            }
            return;
        }
    };
    attrs.push(Attr {
        name: name.to_owned(),
        value,
    });
}

/// What an import kept and set aside, said to the person who ran it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Account {
    pub kept: usize,
    pub before: usize,
    pub after: usize,
}

/// The trace's events as one more result of a stored run: those inside the
/// run's window, on the run's clock, stored `raw` so `omn join` reads them.
///
/// An event outside the window is not this run's, and is counted rather than
/// kept. A trace none of whose events falls inside it was not recorded during
/// the run -- or not on this boot, where the same `monotonic` numbers mean
/// other instants -- and is refused: kept, its events would join with a
/// stranger's.
pub fn merge(
    trace: Trace,
    results: &[crate::emit::Results],
) -> std::result::Result<(crate::emit::Results, Account), CtfError> {
    if results.iter().any(|result| result.source == SOURCE) {
        return Err(CtfError::new(format!(
            "the run already holds an imported `{SOURCE}` result; import into the run as it was \
             stored"
        )));
    }
    let mut window: Option<crate::collect::Watched> = None;
    for result in results.iter().filter(|result| result.window.attached) {
        let joined = window.get_or_insert(result.window);
        joined.opened_ns = joined.opened_ns.min(result.window.opened_ns);
        joined.closed_ns = joined.closed_ns.max(result.window.closed_ns);
    }
    let window = window.ok_or_else(|| {
        CtfError::new(
            "no result of the run was ever attached, so the run has no window to place \
                       the trace in",
        )
    })?;
    let total = trace.events.len();
    let mut account = Account::default();
    let mut events = Vec::with_capacity(total);
    for event in trace.events {
        if event.ts_ns < window.opened_ns {
            account.before += 1;
        } else if event.ts_ns > window.closed_ns {
            account.after += 1;
        } else {
            events.push(event);
        }
    }
    account.kept = events.len();
    if total > 0 && events.is_empty() {
        return Err(CtfError::new(format!(
            "none of the trace's {total} events falls inside the run's window ({} to {} ns of \
             CLOCK_MONOTONIC): it was recorded at another time, or on another boot",
            window.opened_ns, window.closed_ns
        )));
    }
    for (seq, event) in events.iter_mut().enumerate() {
        event.seq = seq as u64;
    }

    let mut losses = crate::collect::Losses::default();
    if trace.discarded > 0 {
        losses.dropped = trace.discarded;
        losses.reasons.insert(
            "the tracer's ring buffer was full: it discarded these before writing them, over the \
             whole trace and not only the run's window"
                .to_owned(),
            trace.discarded,
        );
    }
    let mut degradations = Vec::new();
    for stopped in &trace.undecodable {
        losses.undecoded += 1;
        degradations.push(format!(
            "a data file stopped being decodable, and the rest of it was not read: {stopped}"
        ));
    }
    let mut notes = vec![format!(
        "imported from a CTF trace after the run, on its `{}` clock (CLOCK_MONOTONIC)",
        trace.clock
    )];
    if account.before + account.after > 0 {
        notes.push(format!(
            "{} of the trace's {total} events fell outside the run's window and were left out: \
             {} before it, {} after",
            account.before + account.after,
            account.before,
            account.after
        ));
    }
    // LTTng-UST records the ids as the traced process sees them.
    notes.push(
        "`task` and `proc` are the traced process's `vtid` and `vpid`: inside a container they \
         are its namespace's ids, not the host's"
            .to_owned(),
    );
    let mut fields = crate::emit::Fields::new();
    fields.insert("before_window".to_owned(), account.before as u64);
    fields.insert("after_window".to_owned(), account.after as u64);
    let label = match (trace.env.get("tracer_name"), trace.env.get("procname")) {
        (Some(tracer), Some(procname)) => format!("{tracer}:{procname}"),
        (Some(tracer), None) => tracer.clone(),
        _ => SOURCE.to_owned(),
    };
    let result = crate::emit::Results {
        label,
        source: SOURCE.to_owned(),
        unit: crate::catalog::Unit::Count,
        backend: "import:ctf".to_owned(),
        events_seen: events.len(),
        outputs: vec![crate::emit::Output::Raw { events }],
        observed_ns: window.closed_ns.saturating_sub(window.opened_ns),
        window,
        stopped_because: "imported after the run".to_owned(),
        losses,
        filtered: None,
        lifetime: None,
        unverified: Vec::new(),
        degradations,
        stacks: BTreeMap::new(),
        notes,
        overhead: crate::collect::Overhead::default(),
        served_by: Vec::new(),
        period_ns: None,
        by_ring: Vec::new(),
        value_attr: String::new(),
        row_stamp: None,
        drains: Vec::new(),
        fields,
        provenance: crate::stack::Provenance::default(),
    };
    Ok((result, account))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/ctf")
            .join(name)
    }

    fn text_of(event: &Event, name: &str) -> Option<String> {
        match event.attr(name) {
            Some(AttrValue::Text(text)) => Some(text.clone()),
            _ => None,
        }
    }

    #[test]
    fn a_compact_timestamp_wraps_into_the_next_period() {
        // 27 bits: the clock at 0x7ff_fff0 low bits, a reading of 0x10 is
        // after the wrap.
        let clock = (5u64 << 27) | 0x7ff_fff0;
        assert_eq!(advance(clock, 0x10, 27), (6u64 << 27) | 0x10);
        assert_eq!(advance(clock, 0x7ff_fff8, 27), (5u64 << 27) | 0x7ff_fff8);
        assert_eq!(advance(clock, 42, 64), 42);
    }

    #[test]
    fn bits_are_read_from_the_low_end_in_little_endian_and_the_high_in_big() {
        let bytes = [0b1010_1101u8, 0xff];
        let mut little = Reader {
            bytes: &bytes,
            at: 0,
            end: 16,
            little: true,
            clock: 0,
            scopes: Vec::new(),
        };
        assert_eq!(little.bits(5, true).unwrap(), 0b01101);
        assert_eq!(little.bits(3, true).unwrap(), 0b101);
        let mut big = Reader {
            bytes: &bytes,
            at: 0,
            end: 16,
            little: false,
            clock: 0,
            scopes: Vec::new(),
        };
        assert_eq!(big.bits(5, false).unwrap(), 0b10101);
        assert_eq!(big.bits(3, false).unwrap(), 0b101);
    }

    #[test]
    fn a_declaration_this_reader_does_not_know_is_named() {
        let error =
            Metadata::parse(b"/* CTF 1.8 */ trace { major = 1; }; frobnicate { };").unwrap_err();
        assert!(error.to_string().contains("`frobnicate`"), "{error}");
        let error = Metadata::parse(b"/* CTF 1.8 */ trace { major = 2; minor = 0; };").unwrap_err();
        assert!(error.to_string().contains("CTF 2"), "{error}");
    }

    /// `wk-heartbeat-u`, an LTTng-UST 2.0 trace of eight per-cpu streams, as
    /// babeltrace 2's own test suite reads it
    /// (`tests/plugins/sink.text.details/default-ctf1-mip0.expect`).
    #[test]
    fn an_lttng_ust_trace_reads_as_babeltrace_reads_it() {
        let trace = read(&fixture("wk-heartbeat-u")).expect("the trace reads");
        assert!(trace.undecodable.is_empty(), "{:?}", trace.undecodable);
        assert_eq!(trace.clock, "monotonic");
        assert_eq!(
            trace.env.get("tracer_name").map(String::as_str),
            Some("lttng-ust")
        );
        assert_eq!(trace.discarded, 0);
        // babeltrace prints 20 events of `heartbeat:msg`.
        assert_eq!(trace.events.len(), 20);
        let first = &trace.events[0];
        // `[1,967,640,734,196 cycles ...]`, at 1 GHz.
        assert_eq!(first.ts_ns, 1_967_640_734_196);
        assert_eq!(first.ts_err_ns, 0);
        assert_eq!(text_of(first, "event").as_deref(), Some("heartbeat:msg"));
        assert_eq!(text_of(first, "msg").as_deref(), Some("heartbeat"));
        assert_eq!(first.keys[0], KeyValue::Task { tid: 3214 });
        assert_eq!(
            first.keys[1],
            KeyValue::Proc {
                pidns: 0,
                pid: 3208
            }
        );
        assert_eq!(first.keys[2], KeyValue::Cpu { cpu: 2 });
        let second = &trace.events[1];
        assert_eq!(second.ts_ns, 1_967_640_810_463);
        assert_eq!(second.keys[0], KeyValue::Task { tid: 3215 });
        assert_eq!(second.keys[2], KeyValue::Cpu { cpu: 4 });
        assert!(
            trace
                .events
                .windows(2)
                .all(|pair| pair[0].ts_ns <= pair[1].ts_ns),
            "in time order"
        );
    }

    /// Every record of a trace's data files, its clock refused or not: what
    /// the decoder reads, apart from what an import accepts.
    fn decoded(name: &str) -> Vec<Record> {
        let directory = fixture(name);
        let metadata = Metadata::parse(&std::fs::read(directory.join("metadata")).unwrap())
            .expect("the metadata parses");
        let bytes = std::fs::read(directory.join("stream")).unwrap();
        let read = read_stream(&metadata, &bytes);
        assert_eq!(read.undecodable, None);
        read.records
    }

    fn payload(record: &Record) -> Vec<(String, Field)> {
        record.fields.clone()
    }

    /// babeltrace's `trace-array-align-elem.expect` and
    /// `trace-struct-array-align-elem.expect`: an array whose element is
    /// aligned, empty, between two bytes -- the empty array's alignment
    /// moves nothing, and a reader that aligns it reads `c` from the wrong
    /// byte.
    #[test]
    fn an_empty_aligned_array_moves_nothing() {
        let records = decoded("array-align-elem");
        assert_eq!(records.len(), 1);
        let fields = payload(&records[0]);
        assert_eq!(fields[0], ("a".to_owned(), Field::Uint(1)));
        assert_eq!(fields[2], ("c".to_owned(), Field::Uint(3)));

        let records = decoded("struct-array-align-elem");
        assert_eq!(records.len(), 1);
        let fields = payload(&records[0]);
        assert_eq!(fields[0], ("x".to_owned(), Field::Uint(1)));
        assert_eq!(fields[1].1.get("a"), Some(&Field::Uint(5)));
        assert_eq!(fields[2], ("z".to_owned(), Field::Uint(9)));
    }

    /// `multi-domains`, a kernel and a user-space trace of one LTTng
    /// session side by side, as babeltrace reads it over the live protocol
    /// (`tests/plugins/src.ctf.lttng-live/cli-multi-domains.expect`): 4272
    /// events, and the kernel's tracer discarded 728.
    #[test]
    fn a_session_s_domains_are_read_together_and_their_discards_counted() {
        let trace = read(&fixture("multi-domains")).expect("the trace reads");
        assert!(trace.undecodable.is_empty(), "{:?}", trace.undecodable);
        assert_eq!(trace.events.len(), 4272);
        assert_eq!(trace.discarded, 728);
        assert!(trace
            .events
            .windows(2)
            .all(|pair| pair[0].ts_ns <= pair[1].ts_ns));
        assert!(
            trace
                .events
                .iter()
                .enumerate()
                .all(|(seq, event)| event.seq == seq as u64),
            "one sequence over the merged streams"
        );
    }

    #[test]
    fn a_clock_other_than_lttng_s_monotonic_is_refused() {
        let directory = std::env::temp_dir().join(format!("omn-ctf-clock-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("metadata"),
            "/* CTF 1.8 */ trace { major = 1; minor = 8; byte_order = le; }; \
             clock { name = cycle_counter; freq = 1000; };",
        )
        .unwrap();
        let error = read(&directory).unwrap_err().to_string();
        std::fs::remove_dir_all(&directory).ok();
        assert!(error.contains("`cycle_counter`"), "{error}");
        assert!(error.contains("CLOCK_MONOTONIC"), "{error}");
    }
}
