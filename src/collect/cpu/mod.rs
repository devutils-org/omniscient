//! Collectors for the `cpu` family.
//!
//! Thirteen of this family's fourteen sources are `perf_event_open` counters,
//! so one mechanism serves most of them: a group of counters opened over the
//! target and read once, when the window closes. `counters.rs` holds it, and
//! the table there is what a new counting source is added to.

/// Visible to the rest of the collector layer for `mem.faults`, whose `count`
/// mode is one software counter read through this group rather than a copy of it.
pub(in crate::collect) mod counters;
mod oncpu;
mod percpu;
mod pressure;
mod profile;
mod throttle;
mod topdown;
mod util;

use super::Factory;
use crate::request::StackMode;

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
            Factory::new("cpu.oncpu_time", "fs:proc", |resolved| {
                Ok(Box::new(oncpu::Prepare::new(resolved.request.mode)?))
            }),
            Factory::new("cpu.pressure", "fs:proc", |resolved| {
                Ok(Box::new(pressure::Prepare(resolved.request.mode)))
            }),
            Factory::new("cpu.topdown", "pmu", |resolved| {
                Ok(Box::new(topdown::Prepare(resolved.request.mode)))
            }),
            Factory::new("cpu.throttle", "fs:cgroup", |resolved| {
                Ok(Box::new(throttle::Prepare(resolved.request.mode)))
            }),
            // The one source of this family that is not a counter: it samples,
            // and what it reports is where the samples landed. The unwind mode
            // is decided here rather than in the collector, because `--stack
            // dwarf` is a refusal with a reason and a refusal belongs before
            // the target is launched.
            Factory::new("cpu.profile", "pmu:sw", |resolved| {
                let mode = resolved.request.stack.unwrap_or(StackMode::Fp);
                Ok(Box::new(profile::Prepare {
                    mode: resolved.request.mode,
                    callchain: crate::stack::callchain_for(mode)?,
                }))
            }),
        ])
        .collect()
}
