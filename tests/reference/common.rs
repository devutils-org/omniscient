//! What every reference family shares: a parser turns a tool's stdout and
//! stderr into keyed values (`""` for a single scalar), so a pair can
//! compare a scalar, a histogram's buckets or a per-thread table alike.
//! And the small helpers every parser ends up writing.

use std::path::{Path, PathBuf};

pub type Values = Vec<(String, f64)>;

#[allow(dead_code)]
pub struct Parser {
    /// The name a pair's `reference.parse` field gives.
    pub name: &'static str,
    pub parse: fn(stdout: &str, stderr: &str) -> Result<Values, String>,
}

/// The repository's root, where every pair's relative path resolves.
#[allow(dead_code)]
pub fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// A tool's output captured for a family's parser tests:
/// `tests/reference/captures/<family>/<name>`.
#[allow(dead_code)]
pub fn capture(family: &str, name: &str) -> String {
    let path = root()
        .join("tests/reference/captures")
        .join(family)
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("capture {}: {e}", path.display()))
}

/// A number as tools print it: thousands separators (`1,234,567`,
/// `1 234 567`, `1_234`), a trailing `%`, surrounding blanks. `<not counted>`
/// and the like are an error, never zero.
#[allow(dead_code)]
pub fn number(text: &str) -> Result<f64, String> {
    let cleaned: String = text
        .trim()
        .trim_end_matches('%')
        .chars()
        .filter(|c| !matches!(c, ',' | '_' | ' ' | '\u{a0}' | '\u{202f}'))
        .collect();
    cleaned
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
        .ok_or_else(|| format!("`{}` is not a number", text.trim()))
}

/// The value under `key` in a parser's output.
#[allow(dead_code)]
pub fn value(values: &Values, key: &str) -> Option<f64> {
    values.iter().find(|(k, _)| k == key).map(|(_, v)| *v)
}

/// Whether a binary is on `PATH` (or, given as a path, exists).
#[allow(dead_code)]
pub fn installed(binary: &str) -> bool {
    if binary.contains('/') {
        let path = Path::new(binary);
        return if path.is_absolute() {
            path.exists()
        } else {
            root().join(path).exists()
        };
    }
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(binary).is_file()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_as_tools_print_them() {
        assert_eq!(number(" 1,234,567 ").unwrap(), 1_234_567.0);
        assert_eq!(number("1 234").unwrap(), 1234.0);
        assert_eq!(number("12.5%").unwrap(), 12.5);
        assert!(number("<not counted>").is_err());
        assert!(number("").is_err());
        assert!(number("nan").is_err());
    }
}
