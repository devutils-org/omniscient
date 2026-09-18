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
    /// Substring or glob match, for string fields such as `comm`.
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
