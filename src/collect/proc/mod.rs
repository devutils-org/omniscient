//! Collectors for the `proc` family.
//!
//! Six sources, over two mechanisms that answer two different kinds of
//! question. `proc.fd` and `proc.threads` are counts a process keeps about
//! itself, read from procfs on a clock. The other four are events the kernel
//! publishes as they happen -- an exec, an exit, a signal, a syscall -- and go
//! through the eBPF layer, because there is no file to poll for any of them and
//! a poll would answer with whatever was under the clock.

mod events;
mod lifecycle;
mod status;
mod syscall;
mod syscall_names;

use super::Factory;

/// The collectors this family provides.
pub fn factories() -> Vec<Factory> {
    vec![
        Factory::new("proc.fd", "fs:proc", |_| {
            Ok(Box::new(status::Prepare(status::Counted::Descriptors)))
        }),
        Factory::new("proc.threads", "fs:proc", |_| {
            Ok(Box::new(status::Prepare(status::Counted::Threads)))
        }),
        Factory::new("proc.exec", "bpf:raw_tp", |resolved| {
            Ok(Box::new(events::PrepareExec(resolved.request.mode)))
        }),
        Factory::new("proc.exit", "bpf:raw_tp", |resolved| {
            Ok(Box::new(events::PrepareExit(resolved.request.mode)))
        }),
        Factory::new("proc.signals", "bpf:raw_tp", |resolved| {
            Ok(Box::new(events::PrepareSignals(resolved.request.mode)))
        }),
        Factory::new("proc.syscall", "bpf:raw_tp", |resolved| {
            Ok(Box::new(syscall::Prepare {
                mode: resolved.request.mode,
                // Carried into the collector because the resolver credits every
                // `bpf` backend with filtering in the kernel and then leaves
                // its events alone; see `syscall::Prepare`.
                filter: resolved.request.filter.clone(),
            }))
        }),
    ]
}
