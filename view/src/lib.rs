//! `omn top`: an interactive terminal view of what `omn` measures.
//!
//! A **projection, never a collector**: every view draws from a [`model::Data`]
//! -- a run as `omn` wrote it, or a live run as its runner hands it over -- and
//! nothing here opens a source, polls a file or starts a program. Unused, it
//! costs nothing: `omn` links it, and only `omn top` calls it.
//!
//! - [`model`]: the run, mirrored from the JSON `omn` writes.
//! - [`trust`]: the one style every figure is drawn in -- dimmed, badged,
//!   error-barred, or refused in place.
//! - [`views`]: the [`views::View`] contract and the registry of the nine
//!   views.
//! - [`app`]: the frame -- header, tabs, help line, the view's own cost.
//! - [`feed`]: a live run's updates.
//! - [`term`]: the interactive loop.
//! - [`snapshot`]: a view drawn into text, for its test and its judge.
//!
//! `docs/view/README.md` is the guide for a view's author.

pub mod app;
pub mod cost;
pub mod feed;
pub mod model;
pub mod snapshot;
pub mod term;
pub mod trust;
pub mod units;
pub mod views;

/// The terminal library the views draw with, at the version this crate
/// links: a test outside this crate draws with the same one.
pub use ratatui;
