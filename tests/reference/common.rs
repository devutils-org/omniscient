//! What every reference family shares: a parser turns a tool's stdout and
//! stderr into keyed values (`""` for a single scalar), so a pair can
//! compare a scalar, a histogram's buckets or a per-thread table alike.

pub type Values = Vec<(String, f64)>;

#[allow(dead_code)]
pub struct Parser {
    /// The name a pair's `reference.parse` field gives.
    pub name: &'static str,
    pub parse: fn(stdout: &str, stderr: &str) -> Result<Values, String>,
}
