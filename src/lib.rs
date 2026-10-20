//! Omniscient: one vocabulary for system observability.
//!
//! It turns any input surface -- the command line, a session file in YAML,
//! JSON or TOML -- into a single [`Request`](request::Request), checks it
//! against the catalog ([`validate`]), decides how this machine serves it
//! ([`resolve`]), collects it ([`collect`]) and renders what it measured
//! ([`emit`], [`encode`]). The `omn` binary is one user of this library; a
//! program that wants a run's events while it goes uses [`collect::live`], as
//! `examples/live.rs` shows.
//!
//! The ordering is deliberate. The request model and the catalog are what every
//! later phase is written against, and they are the expensive things to change;
//! a collector is a day's work. See `ROADMAP.md`.

/// Implements `Serialize` and `Deserialize` for a type through its `Display`
/// and `FromStr`, so a value reads identically on the command line, in a
/// session file and in a dumped request.
///
/// Defined at the crate root so every module can use it; it must stay above the
/// module declarations below.
macro_rules! serde_as_string {
    ($type:ty) => {
        impl serde::Serialize for $type {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }

        impl<'de> serde::Deserialize<'de> for $type {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let text = <String as serde::Deserialize>::deserialize(deserializer)?;
                text.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

pub mod bpf;
pub mod capabilities;
pub mod catalog;
pub mod cli;
pub mod clock;
pub mod collect;
pub mod ctf;
pub mod emit;
pub mod encode;
pub mod error;
pub mod event;
pub mod filter;
pub mod format;
pub mod join;
pub mod lazy;
pub mod perf;
pub mod render;
pub mod request;
pub mod resolve;
pub mod scalar;
pub mod selftest;
pub mod session;
pub mod stack;
pub mod style;
pub mod triage;
pub mod validate;

pub use catalog::Catalog;
pub use error::ParseError;
pub use request::{Request, Session};
