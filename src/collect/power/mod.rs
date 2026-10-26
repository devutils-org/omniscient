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
//! Most of it reads sysfs. The `pmu` backends -- RAPL, and APERF, MPERF and
//! the TSC through the kernel's `msr` unit -- open the same counters as perf
//! events, which an ordinary user may where the files and device nodes are
//! root's.

mod cstate;
mod energy;
mod energy_pmu;
mod freq;
mod freq_pmu;
mod idle_bpf;
mod process_energy;
mod sysfs;
mod temp;
mod throttle;
mod wakeups;

use super::Factory;

/// The collectors this family provides.
pub fn factories() -> Vec<Factory> {
    vec![
        Factory::new("power.energy", "pmu", |resolved| {
            let by_domain = resolved
                .request
                .keys
                .contains(&crate::request::KeyKind::Domain);
            Ok(Box::new(energy_pmu::Prepare::new(
                resolved.request.mode,
                by_domain,
            )?))
        }),
        // The package's counted window, shared out by cpu time: an estimate.
        Factory::new("power.process_energy", "pmu", |resolved| {
            Ok(Box::new(process_energy::Prepare {
                inner: Box::new(energy_pmu::Prepare::new(resolved.request.mode, false)?),
                mode: resolved.request.mode,
            }))
        }),
        Factory::new("power.energy", "fs:sys", |resolved| {
            Ok(Box::new(energy::Prepare::new(resolved.request.mode)?))
        }),
        Factory::new("power.freq", "fs:sys", |resolved| {
            Ok(Box::new(freq::Prepare::new(resolved.request.mode)?))
        }),
        Factory::new("power.freq", "pmu", |resolved| {
            Ok(Box::new(freq_pmu::Prepare::new(resolved.request.mode)?))
        }),
        Factory::new("power.cstate", "bpf:raw_tp", |resolved| {
            Ok(Box::new(idle_bpf::Prepare::new(
                resolved.request.mode,
                false,
            )?))
        }),
        Factory::new("power.cstate", "bpf:tp", |resolved| {
            Ok(Box::new(idle_bpf::Prepare::new(
                resolved.request.mode,
                true,
            )?))
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
        Factory::new("power.wakeups", "fs:proc", |resolved| {
            Ok(Box::new(wakeups::Prepare::new(resolved.request.mode)?))
        }),
    ]
}
