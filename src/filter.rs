//! The filter grammar.
//!
//! A filter is parsed into an expression tree rather than kept as a string,
//! because the resolver has to decide, per predicate, whether it can be pushed
//! into the kernel or has to run in userspace. That decision needs the fields
//! and the operators, not the text; and the placement of every predicate is
//! reported back to the user, so a filter that did not make it into the kernel
//! is visible rather than merely slow.
//!
//! ```text
//! expression := disjunction
//! disjunction := conjunction ("||" conjunction)*
//! conjunction := unary ("&&" unary)*
//! unary       := "!" unary | "(" expression ")" | comparison
//! comparison  := field operator value
//! operator    := "==" | "!=" | "<" | "<=" | ">" | ">=" | "=~" | "in"
//! value       := integer-with-optional-suffix | quoted-string | bare-word | "(" value ("," value)* ")"
//! ```

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::ParseError;
use crate::scalar::parse_scalar;

/// A filter predicate tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterExpr {
    And(Box<FilterExpr>, Box<FilterExpr>),
    Or(Box<FilterExpr>, Box<FilterExpr>),
    Not(Box<FilterExpr>),
    Compare {
        field: String,
        operator: CompareOp,
        value: Value,
    },
}

impl FilterExpr {
    /// Every field the expression reads, sorted and deduplicated.
    ///
    /// The validator checks these against the source's declared
    /// `filter_fields`: filtering on a field a source does not carry is a
    /// mistake worth catching before anything is attached.
    pub fn fields(&self) -> BTreeSet<&str> {
        let mut fields = BTreeSet::new();
        self.collect_fields(&mut fields);
        fields
    }

    fn collect_fields<'a>(&'a self, into: &mut BTreeSet<&'a str>) {
        match self {
            FilterExpr::And(left, right) | FilterExpr::Or(left, right) => {
                left.collect_fields(into);
                right.collect_fields(into);
            }
            FilterExpr::Not(inner) => inner.collect_fields(into),
            FilterExpr::Compare { field, .. } => {
                into.insert(field.as_str());
            }
        }
    }

    /// The parts of the top-level conjunction, in order: `a && (b || c) && d`
    /// is `a`, `b || c` and `d`. A predicate that is not a conjunction is one
    /// part.
    pub fn conjuncts(&self) -> Vec<&FilterExpr> {
        match self {
            FilterExpr::And(left, right) => {
                let mut parts = left.conjuncts();
                parts.extend(right.conjuncts());
                parts
            }
            other => vec![other],
        }
    }

    /// The field of a comparison a program can carry as a constant: `field ==
    /// value` with one value. `None` for anything else -- an inequality, a
    /// range, a pattern, a list, a negation, a disjunction -- because a
    /// program that compares one register against one immediate is what
    /// "pushed down" means for every program this build writes.
    pub fn constant_equality(&self) -> Option<&str> {
        match self {
            FilterExpr::Compare {
                field,
                operator: CompareOp::Eq,
                value: Value::Int { .. } | Value::Str(_),
            } => Some(field.as_str()),
            _ => None,
        }
    }

    fn precedence(&self) -> u8 {
        match self {
            FilterExpr::Or(..) => 1,
            FilterExpr::And(..) => 2,
            FilterExpr::Not(..) => 3,
            FilterExpr::Compare { .. } => 4,
        }
    }
}

/// How a field is compared to a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    /// Substring match, for string fields such as `comm`: `comm =~ 'python'`
    /// keeps `python3`.
    ///
    /// It is not a regular expression, although every other tool that spells a
    /// match operator this way means one. A pattern written as a regex parses,
    /// validates, is reported as applied and keeps nothing, which looks exactly
    /// like a workload that did nothing -- so the ones that can only have been
    /// meant as a regex are refused by [`literal_only`] rather than evaluated
    /// literally.
    Matches,
    /// Membership in a literal set.
    In,
}

impl CompareOp {
    fn as_str(self) -> &'static str {
        match self {
            CompareOp::Eq => "==",
            CompareOp::Ne => "!=",
            CompareOp::Lt => "<",
            CompareOp::Le => "<=",
            CompareOp::Gt => ">",
            CompareOp::Ge => ">=",
            CompareOp::Matches => "=~",
            CompareOp::In => "in",
        }
    }
}

impl fmt::Display for CompareOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A literal on the right-hand side of a comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// Integers keep their literal form so `4KiB` still reads as `4KiB` after a
    /// round trip, while comparisons use the normalized value.
    Int {
        value: i64,
        literal: String,
    },
    Str(String),
    List(Vec<Value>),
}

impl Value {
    pub fn int(value: i64) -> Self {
        Value::Int {
            value,
            literal: value.to_string(),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int { literal, .. } => f.write_str(literal),
            Value::Str(text) => write!(f, "'{text}'"),
            Value::List(values) => {
                let rendered: Vec<String> = values.iter().map(Value::to_string).collect();
                write!(f, "({})", rendered.join(", "))
            }
        }
    }
}

impl fmt::Display for FilterExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FilterExpr::Compare {
                field,
                operator,
                value,
            } => write!(f, "{field} {operator} {value}"),
            FilterExpr::Not(inner) => {
                write!(f, "!")?;
                write_nested(f, inner, self.precedence())
            }
            FilterExpr::And(left, right) => {
                write_nested(f, left, self.precedence())?;
                write!(f, " && ")?;
                write_nested(f, right, self.precedence())
            }
            FilterExpr::Or(left, right) => {
                write_nested(f, left, self.precedence())?;
                write!(f, " || ")?;
                write_nested(f, right, self.precedence())
            }
        }
    }
}

/// Parenthesizes a child only when its precedence would otherwise change the
/// meaning of the re-parsed text.
fn write_nested(f: &mut fmt::Formatter<'_>, child: &FilterExpr, parent: u8) -> fmt::Result {
    if child.precedence() < parent {
        write!(f, "({child})")
    } else {
        write!(f, "{child}")
    }
}

impl FromStr for FilterExpr {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let tokens = lex(s)?;
        let mut parser = Parser {
            tokens: &tokens,
            position: 0,
        };
        let expression = parser.parse_disjunction()?;
        match parser.peek() {
            None => Ok(expression),
            Some(token) => Err(ParseError::new(format!(
                "unexpected `{token}` at the end of filter `{s}`"
            ))),
        }
    }
}

impl Serialize for FilterExpr {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for FilterExpr {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(String),
    Str(String),
    Operator(CompareOp),
    And,
    Or,
    Not,
    OpenParen,
    CloseParen,
    Comma,
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Token::Word(text) => f.write_str(text),
            Token::Str(text) => write!(f, "'{text}'"),
            Token::Operator(operator) => write!(f, "{operator}"),
            Token::And => f.write_str("&&"),
            Token::Or => f.write_str("||"),
            Token::Not => f.write_str("!"),
            Token::OpenParen => f.write_str("("),
            Token::CloseParen => f.write_str(")"),
            Token::Comma => f.write_str(","),
        }
    }
}

fn lex(input: &str) -> Result<Vec<Token>, ParseError> {
    let chars: Vec<char> = input.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            c if c.is_whitespace() => i += 1,
            '(' => {
                tokens.push(Token::OpenParen);
                i += 1;
            }
            ')' => {
                tokens.push(Token::CloseParen);
                i += 1;
            }
            ',' => {
                tokens.push(Token::Comma);
                i += 1;
            }
            '\'' | '"' => {
                let quote = c;
                let start = i + 1;
                let mut end = start;
                while end < chars.len() && chars[end] != quote {
                    end += 1;
                }
                if end >= chars.len() {
                    return Err(ParseError::new(format!(
                        "unterminated string in filter `{input}`"
                    )));
                }
                tokens.push(Token::Str(chars[start..end].iter().collect()));
                i = end + 1;
            }
            '&' | '|' => {
                let expected = c;
                if chars.get(i + 1) != Some(&expected) {
                    return Err(ParseError::new(format!(
                        "`{c}` alone is not an operator in a filter (write `{c}{c}`)"
                    )));
                }
                tokens.push(if c == '&' { Token::And } else { Token::Or });
                i += 2;
            }
            '=' | '!' | '<' | '>' => {
                let next = chars.get(i + 1).copied();
                let (token, width) = match (c, next) {
                    ('=', Some('=')) => (Token::Operator(CompareOp::Eq), 2),
                    ('=', Some('~')) => (Token::Operator(CompareOp::Matches), 2),
                    ('!', Some('=')) => (Token::Operator(CompareOp::Ne), 2),
                    ('<', Some('=')) => (Token::Operator(CompareOp::Le), 2),
                    ('>', Some('=')) => (Token::Operator(CompareOp::Ge), 2),
                    ('<', _) => (Token::Operator(CompareOp::Lt), 1),
                    ('>', _) => (Token::Operator(CompareOp::Gt), 1),
                    ('!', _) => (Token::Not, 1),
                    ('=', _) => {
                        return Err(ParseError::new(
                            "`=` is assignment, not comparison: write `==` in a filter",
                        ))
                    }
                    _ => unreachable!("outer match restricts the first character"),
                };
                tokens.push(token);
                i += width;
            }
            _ => {
                let start = i;
                while i < chars.len()
                    && !chars[i].is_whitespace()
                    && !matches!(
                        chars[i],
                        '(' | ')' | ',' | '=' | '!' | '<' | '>' | '&' | '|' | '\'' | '"'
                    )
                {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                if word == "in" {
                    tokens.push(Token::Operator(CompareOp::In));
                } else {
                    tokens.push(Token::Word(word));
                }
            }
        }
    }
    if tokens.is_empty() {
        return Err(ParseError::new("empty filter"));
    }
    Ok(tokens)
}

struct Parser<'a> {
    tokens: &'a [Token],
    position: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&'a Token> {
        self.tokens.get(self.position)
    }

    fn next(&mut self) -> Option<&'a Token> {
        let token = self.tokens.get(self.position);
        if token.is_some() {
            self.position += 1;
        }
        token
    }

    fn parse_disjunction(&mut self) -> Result<FilterExpr, ParseError> {
        let mut left = self.parse_conjunction()?;
        while self.peek() == Some(&Token::Or) {
            self.next();
            let right = self.parse_conjunction()?;
            left = FilterExpr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_conjunction(&mut self) -> Result<FilterExpr, ParseError> {
        let mut left = self.parse_unary()?;
        while self.peek() == Some(&Token::And) {
            self.next();
            let right = self.parse_unary()?;
            left = FilterExpr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<FilterExpr, ParseError> {
        match self.peek() {
            Some(Token::Not) => {
                self.next();
                Ok(FilterExpr::Not(Box::new(self.parse_unary()?)))
            }
            Some(Token::OpenParen) => {
                self.next();
                let inner = self.parse_disjunction()?;
                match self.next() {
                    Some(Token::CloseParen) => Ok(inner),
                    _ => Err(ParseError::new("unbalanced parenthesis in filter")),
                }
            }
            _ => self.parse_comparison(),
        }
    }

    fn parse_comparison(&mut self) -> Result<FilterExpr, ParseError> {
        let field = match self.next() {
            Some(Token::Word(word)) => word.clone(),
            Some(token) => {
                return Err(ParseError::new(format!(
                    "expected a field name in filter, found `{token}`"
                )))
            }
            None => {
                return Err(ParseError::new(
                    "filter ends where a field name was expected",
                ))
            }
        };
        let operator = match self.next() {
            Some(Token::Operator(operator)) => *operator,
            Some(token) => {
                return Err(ParseError::new(format!(
                    "expected a comparison operator after `{field}`, found `{token}`"
                )))
            }
            None => {
                return Err(ParseError::new(format!(
                    "`{field}` is not a predicate: a filter compares a field to a value, as in `{field} > 0`"
                )))
            }
        };
        let value = self.parse_value(&field)?;
        if operator == CompareOp::Matches {
            if let Value::Str(pattern) = &value {
                literal_only(&field, pattern)?;
            }
        }
        if operator == CompareOp::In && !matches!(value, Value::List(_)) {
            return Err(ParseError::new(format!(
                "`in` expects a list, as in `{field} in (1, 2)`"
            )));
        }
        if operator != CompareOp::In && matches!(value, Value::List(_)) {
            return Err(ParseError::new(format!(
                "a list value only works with `in`, not with `{operator}`"
            )));
        }
        Ok(FilterExpr::Compare {
            field,
            operator,
            value,
        })
    }

    fn parse_value(&mut self, field: &str) -> Result<Value, ParseError> {
        match self.next() {
            Some(Token::Str(text)) => Ok(Value::Str(text.clone())),
            Some(Token::Word(word)) => Ok(word_value(word)),
            Some(Token::OpenParen) => {
                let mut values = Vec::new();
                loop {
                    match self.next() {
                        Some(Token::Str(text)) => values.push(Value::Str(text.clone())),
                        Some(Token::Word(word)) => values.push(word_value(word)),
                        Some(token) => {
                            return Err(ParseError::new(format!(
                                "expected a value in list, found `{token}`"
                            )))
                        }
                        None => return Err(ParseError::new("unterminated list in filter")),
                    }
                    match self.next() {
                        Some(Token::Comma) => continue,
                        Some(Token::CloseParen) => break,
                        Some(token) => {
                            return Err(ParseError::new(format!(
                                "expected `,` or `)` in list, found `{token}`"
                            )))
                        }
                        None => return Err(ParseError::new("unterminated list in filter")),
                    }
                }
                Ok(Value::List(values))
            }
            Some(token) => Err(ParseError::new(format!(
                "expected a value after `{field}`, found `{token}`"
            ))),
            None => Err(ParseError::new(format!(
                "filter ends where `{field}`'s value was expected"
            ))),
        }
    }
}

/// Refuses a `=~` pattern that could only have been meant as a regular
/// expression.
///
/// `=~` is `str::contains`. Perl, Ruby, awk and bash's `[[ =~ ]]` all spell a
/// regular-expression match this way, so a reader has every reason to write
/// one -- and a regex written here is parsed, validated, reported as applied,
/// and keeps nothing. An empty series is also what a correct filter over a
/// quiet workload produces, so the mistake is invisible at exactly the moment
/// it matters.
///
/// Refusing is the only place the tool can say what the operator does, since
/// nothing else in a run has a reason to mention it. It happens at parse time,
/// before anything is attached: the request cannot mean what it says, which is
/// not the same as a machine that cannot serve it.
///
/// The set is deliberately narrow -- what an attribute value almost never
/// contains and a misread predicate almost always does. `.`, `+`, `*` and `?`
/// are absent on purpose: `a.js:3:1`, `libc.so.6+0x27781` and `operator*` are
/// ordinary method and frame text, and refusing them would break substring
/// matches that work today.
fn literal_only(field: &str, pattern: &str) -> Result<(), ParseError> {
    let refuse = |what: &str, instead: String| {
        Err(ParseError::new(format!(
            "`=~` is a substring test, not a regular expression, so `{pattern}` would only keep \
             values containing {what} literally. {instead}"
        )))
    };

    if let Some(inner) = pattern
        .strip_prefix('^')
        .or_else(|| pattern.strip_suffix('$'))
    {
        let anchor = if pattern.starts_with('^') {
            "`^`"
        } else {
            "`$`"
        };
        let inner = inner.trim_start_matches('^').trim_end_matches('$');
        return refuse(
            anchor,
            format!(
                "It has no anchors: write `{field} == '{inner}'` for an exact match, or \
                 `{field} =~ '{inner}'` to find it anywhere in the value."
            ),
        );
    }

    for (character, means) in [
        ('[', "a character class"),
        ('(', "a group"),
        ('\\', "an escape"),
    ] {
        if pattern.contains(character) {
            return refuse(
                &format!("`{character}`"),
                format!(
                    "`{character}` is {means} in a regular expression and nothing in a \
                     substring: write the text to look for, as in `{field} =~ 'text'`."
                ),
            );
        }
    }
    Ok(())
}

/// A bare word is an integer when it parses as one, a string otherwise, so
/// `rwbs == R` and `bytes > 4KiB` both work without quoting rules.
fn word_value(word: &str) -> Value {
    match parse_scalar(word) {
        Ok(value) => Value::Int {
            value,
            literal: word.to_owned(),
        },
        Err(_) => Value::Str(word.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two questions the resolver asks of a predicate to place it: its
    /// top-level parts, and which of them compare one field with one value.
    #[test]
    fn a_conjunction_is_read_part_by_part_and_only_one_value_equality_is_constant() {
        let predicate: FilterExpr = "syscall == 'read' && (comm == 'a' || comm == 'b') && pid == 7"
            .parse()
            .unwrap();
        let parts = predicate.conjuncts();
        assert_eq!(parts.len(), 3);
        let constant: Vec<Option<&str>> =
            parts.iter().map(|part| part.constant_equality()).collect();
        assert_eq!(constant, vec![Some("syscall"), None, Some("pid")]);
        for shape in [
            "syscall != 'read'",
            "syscall in (read, write)",
            "bytes > 4",
            "!(pid == 1)",
        ] {
            let expression: FilterExpr = shape.parse().unwrap();
            assert_eq!(expression.conjuncts().len(), 1, "{shape}");
            assert_eq!(expression.constant_equality(), None, "{shape}");
        }
    }

    fn parse(text: &str) -> FilterExpr {
        text.parse().expect("filter should parse")
    }

    #[test]
    fn conjunction_binds_tighter_than_disjunction() {
        let expression = parse("a == 1 || b == 2 && c == 3");
        assert!(matches!(expression, FilterExpr::Or(..)), "{expression:?}");
    }

    #[test]
    fn parentheses_override_precedence_and_survive_the_round_trip() {
        let expression = parse("(a == 1 || b == 2) && c == 3");
        assert_eq!(expression.to_string(), "(a == 1 || b == 2) && c == 3");
        assert_eq!(parse(&expression.to_string()), expression);
    }

    #[test]
    fn suffixed_literals_keep_their_written_form_and_their_value() {
        let expression = parse("bytes > 4KiB");
        assert_eq!(expression.to_string(), "bytes > 4KiB");
        let FilterExpr::Compare {
            value: Value::Int { value, .. },
            ..
        } = &expression
        else {
            panic!("expected a comparison against an integer");
        };
        assert_eq!(*value, 4096);
    }

    #[test]
    fn membership_needs_a_list_and_a_list_needs_membership() {
        assert!(parse("syscall in (read, write)")
            .to_string()
            .contains("in ("));
        assert!("syscall in read".parse::<FilterExpr>().is_err());
        assert!("syscall == (read, write)".parse::<FilterExpr>().is_err());
    }

    #[test]
    fn fields_are_collected_for_validation() {
        let expression = parse("bytes > 4096 && (comm =~ 'nginx' || dev == 259)");
        assert_eq!(
            expression.fields().into_iter().collect::<Vec<_>>(),
            ["bytes", "comm", "dev"]
        );
    }

    #[test]
    fn a_single_equals_is_refused_with_an_explanation() {
        let error = "bytes = 4".parse::<FilterExpr>().unwrap_err();
        assert!(error.message().contains("`==`"), "{error}");
    }

    #[test]
    fn a_bare_field_is_not_a_predicate() {
        let error = "bytes".parse::<FilterExpr>().unwrap_err();
        assert!(error.message().contains("is not a predicate"), "{error}");
    }
}

/// Evaluates a predicate against an event.
///
/// This is the userspace half of filter placement. When the resolver cannot
/// push a predicate into the kernel it says so, and this is what then runs --
/// after the event has already been paid for, which is exactly why the
/// placement is worth reporting.
///
/// A field resolves against the event's attributes, plus `kind`, which every
/// event has: an attribute named `kind` comes first, since a source that
/// declares one (`sched.irq`'s `hard`/`soft`) filters on it, and the event's
/// own kind is the source's name, which `--filter` has no reason to ask. A field the event does not carry makes the comparison false
/// rather than an error: an event that lacks the field simply is not one of the
/// events being asked for.
pub fn matches(expression: &FilterExpr, event: &crate::event::Event) -> bool {
    match expression {
        FilterExpr::And(left, right) => matches(left, event) && matches(right, event),
        FilterExpr::Or(left, right) => matches(left, event) || matches(right, event),
        FilterExpr::Not(inner) => !matches(inner, event),
        FilterExpr::Compare {
            field,
            operator,
            value,
        } => compare(field, *operator, value, event),
    }
}

fn compare(field: &str, operator: CompareOp, value: &Value, event: &crate::event::Event) -> bool {
    use crate::event::AttrValue;

    let attribute = match event.attr(field) {
        Some(attribute) => Some(attribute.clone()),
        None if field == "kind" => Some(AttrValue::Text(event.kind.clone())),
        None => None,
    };
    let Some(attribute) = attribute else {
        return false;
    };

    match operator {
        CompareOp::In => match value {
            Value::List(values) => values
                .iter()
                .any(|candidate| compare_one(&attribute, CompareOp::Eq, candidate)),
            _ => false,
        },
        other => compare_one(&attribute, other, value),
    }
}

fn compare_one(attribute: &crate::event::AttrValue, operator: CompareOp, value: &Value) -> bool {
    use crate::event::AttrValue;

    match value {
        Value::Int { value, .. } => {
            let Some(left) = attribute.as_f64() else {
                return false;
            };
            let right = *value as f64;
            match operator {
                CompareOp::Eq => left == right,
                CompareOp::Ne => left != right,
                CompareOp::Lt => left < right,
                CompareOp::Le => left <= right,
                CompareOp::Gt => left > right,
                CompareOp::Ge => left >= right,
                // A numeric field has no substring to match against.
                CompareOp::Matches | CompareOp::In => false,
            }
        }
        Value::Str(text) => {
            let left = match attribute {
                AttrValue::Text(left) => left.clone(),
                other => other.to_string(),
            };
            match operator {
                CompareOp::Eq => left == *text,
                CompareOp::Ne => left != *text,
                CompareOp::Matches => left.contains(text.as_str()),
                CompareOp::Lt => left.as_str() < text.as_str(),
                CompareOp::Le => left.as_str() <= text.as_str(),
                CompareOp::Gt => left.as_str() > text.as_str(),
                CompareOp::Ge => left.as_str() >= text.as_str(),
                CompareOp::In => false,
            }
        }
        Value::List(_) => false,
    }
}

#[cfg(test)]
mod evaluation_tests {
    use super::*;
    use crate::event::{Attr, CollectorId, Event};

    fn event() -> Event {
        Event {
            ts_ns: 1,
            ts_err_ns: 0,
            kind: "mem.heap.alloc".to_owned(),
            keys: Vec::new(),
            attrs: vec![
                Attr::uint("bytes", 8192),
                Attr::text("op", "alloc"),
                Attr::text("comm", "nginx-worker"),
            ],
            collector_id: CollectorId::new("test"),
            seq: 0,
        }
    }

    fn check(text: &str) -> bool {
        matches(&text.parse::<FilterExpr>().unwrap(), &event())
    }

    #[test]
    fn numeric_and_textual_comparisons_both_work() {
        assert!(check("bytes > 4KiB"));
        assert!(!check("bytes > 16KiB"));
        assert!(check("op == 'alloc'"));
        assert!(check("comm =~ 'nginx'"));
        assert!(!check("comm == 'nginx'"));
    }

    #[test]
    fn the_event_kind_is_addressable_like_a_field() {
        assert!(check("kind == 'mem.heap.alloc'"));
        assert!(check("kind =~ 'heap'"));
    }

    /// A source that declares an attribute named `kind` -- `sched.irq`'s
    /// `hard` or `soft` -- is filtered on it: `--filter 'kind == "soft"'`
    /// used to compare the event's own kind, `sched.irq`, and keep nothing.
    #[test]
    fn an_attribute_named_kind_comes_before_the_event_s_kind() {
        let mut irq = event();
        irq.kind = "sched.irq".to_owned();
        irq.attrs.push(Attr::text("kind", "soft"));
        let check = |text: &str| matches(&text.parse::<FilterExpr>().unwrap(), &irq);
        assert!(check("kind == 'soft'"));
        assert!(!check("kind == 'sched.irq'"));
    }

    #[test]
    fn a_field_the_event_does_not_carry_is_false_rather_than_an_error() {
        assert!(!check("dev == 259"));
        assert!(check("!(dev == 259)"));
    }

    #[test]
    fn membership_and_boolean_composition_hold() {
        assert!(check("op in (alloc, free)"));
        assert!(!check("op in (free, realloc)"));
        assert!(check("bytes > 1024 && op == 'alloc'"));
        assert!(check("bytes > 1000000 || op == 'alloc'"));
    }
}
