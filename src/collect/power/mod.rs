//! Collectors for the `power` family.
//!
//! Five sources that are worth more than their size, because they are the
//! confounder for every other family. A processor that throttled, or that ran
//! at a lower frequency under one instruction mix than another, makes a cpu, a
//! memory and a gpu measurement all move for a reason that is in none of them.
//! The intent recorded in the roadmap is that these are collected at all times,
//! whether or not anyone asked, and that a throttled run is marked unusable
//! rather than averaged in.
//!
//! Everything here reads sysfs. The `pmu` backends -- RAPL and the aperf/mperf
//! pair through `perf_event_open` -- are the better instruments and wait for
//! counting mode in the shared perf layer; `docs/families/power.md` says what
//! they need.

mod cstate;
mod energy;
mod energy_pmu;
mod freq;
mod sysfs;
mod temp;
mod throttle;

use super::Factory;

/// The collectors this family provides.
pub fn factories() -> Vec<Factory> {
    vec![
        Factory::new("power.energy", "pmu", |resolved| {
            Ok(Box::new(energy_pmu::Prepare::new(resolved.request.mode)?))
        }),
        Factory::new("power.energy", "fs:sys", |resolved| {
            Ok(Box::new(energy::Prepare::new(resolved.request.mode)?))
        }),
        Factory::new("power.freq", "fs:sys", |resolved| {
            Ok(Box::new(freq::Prepare::new(resolved.request.mode)?))
        }),
        Factory::new("power.cstate", "fs:sys", |resolved| {
            Ok(Box::new(cstate::Prepare::new(resolved.request.mode)?))
        }),
        Factory::new("power.temp", "fs:sys", |resolved| {
            Ok(Box::new(temp::Prepare::new(resolved.request.mode)?))
        }),
        Factory::new("power.throttle", "fs:sys", |resolved| {
            Ok(Box::new(throttle::Prepare::new(resolved.request.mode)?))
        }),
    ]
}
