//! Collectors for the `cpu` family.
//!
//! Thirteen of this family's fourteen sources are `perf_event_open` counters,
//! so one mechanism serves most of them: a group of counters opened over the
//! target and read once, when the window closes. `counters.rs` holds it, and
//! the table there is what a new counting source is added to.

/// Visible to the rest of the collector layer for `mem.faults`, which samples a
/// cgroup the way `cpu.profile` does.
pub(in crate::collect) mod cgroup;
/// Visible to the rest of the collector layer for `mem.faults`, whose `count`
/// mode is one software counter read through this group rather than a copy of it.
pub(in crate::collect) mod counters;
mod oncpu;
mod oncpu_bpf;
mod percpu;
mod pressure;
mod profile;
mod throttle;
mod topdown;
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
                // A counting source's `sample` mode is a profile on its own
                // counter (`profile::SAMPLED`): `perf record -e <event>`.
                if let (crate::request::Mode::Sample { .. }, Some(sampled)) = (
                    resolved.request.mode,
                    profile::sampled_for(
                        resolved.request.source.as_str(),
                        resolved.backend.id.as_str(),
                    ),
                ) {
                    return profiled(resolved, sampled);
                }
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
            Factory::new("cpu.oncpu_time", "bpf:raw_tp", |resolved| {
                Ok(Box::new(oncpu_bpf::Prepare::new(
                    resolved.request.mode,
                    resolved.request.filter.clone(),
                )?))
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
            // the target is launched. The resolver has made that choice
            // already, and refused the request where it refuses; it is taken
            // as made, so the run cannot unwind otherwise than the resolved
            // file says. A request resolved without one (a file written before
            // the field) is chosen here.
            Factory::new("cpu.profile", "pmu:sw", |resolved| {
                let sampled = profile::sampled_for("cpu.profile", "pmu:sw")
                    .ok_or("no sampled event is declared for `cpu.profile` on `pmu:sw`")?;
                profiled(resolved, sampled)
            }),
        ])
        .collect()
}

/// A profile of `sampled`, unwound the way the resolved file says.
fn profiled(
    resolved: &crate::resolve::ResolvedRequest,
    sampled: profile::Sampled,
) -> Result<Box<dyn super::Prepared>, String> {
    let choice = match &resolved.stack {
        Some(choice) => choice.clone(),
        None => crate::stack::choose(&resolved.request, &resolved.environment)?,
    };
    Ok(Box::new(profile::Prepare {
        mode: resolved.request.mode,
        sampled,
        choice,
        debuginfod: None,
    }))
}
