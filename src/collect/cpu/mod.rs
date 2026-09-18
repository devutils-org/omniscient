//! Collectors for the `cpu` family.
//!
//! Thirteen of this family's fourteen sources are `perf_event_open` counters,
//! so one mechanism serves most of them: a group of counters opened over the
//! target and read once, when the window closes. `counters.rs` holds it, and
//! the table there is what a new counting source is added to.

mod counters;
mod pressure;
mod throttle;
mod util;

use super::Factory;

/// The collectors this family provides.
///
/// Adding a counting source means adding a line to `counters::SPECS`. No file
/// outside this directory changes, which is what lets the families be worked on
/// independently.
pub fn factories() -> Vec<Factory> {
    counters::SPECS
        .iter()
        .map(|spec| {
            // The builder is a plain function pointer rather than a closure over
            // `spec`, so it finds its own entry again. The pair (source,
            // backend) is unique by the catalog's own rule, which is what makes
            // that lookup exact.
            Factory::new(spec.source, spec.backend, |resolved| {
                let spec = counters::spec_for(
                    resolved.request.source.as_str(),
                    resolved.backend.id.as_str(),
                )
                .ok_or_else(|| {
                    format!(
                        "no counter is declared for `{}` on `{}`",
                        resolved.request.source, resolved.backend.id
                    )
                })?;
                Ok(Box::new(counters::Prepare::new(
                    spec,
                    resolved.request.mode,
                )?))
            })
        })
        .chain([
            Factory::new("cpu.util", "fs:proc", |resolved| {
                Ok(Box::new(util::Prepare(resolved.request.mode)))
            }),
            Factory::new("cpu.pressure", "fs:proc", |resolved| {
                Ok(Box::new(pressure::Prepare(resolved.request.mode)))
            }),
            Factory::new("cpu.throttle", "fs:cgroup", |resolved| {
                Ok(Box::new(throttle::Prepare(resolved.request.mode)))
            }),
        ])
        .collect()
}
