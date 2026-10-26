//! Collectors for the `proc` family.
//!
//! Eight sources, over two mechanisms that answer two different kinds of
//! question. `proc.fd`, `proc.threads` and `proc.tasks` are what a process
//! keeps about itself, read from procfs on a clock -- `proc.tasks` for every
//! process of the machine, or every thread of one. The other four are events the kernel
//! publishes as they happen -- an exec, an exit, a signal, a syscall -- and go
//! through the eBPF layer, because there is no file to poll for any of them and
//! a poll would answer with whatever was under the clock.

mod calls;
mod events;
mod lifecycle;
mod status;
mod syscall;
mod syscall_names;
mod tasks;

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
        Factory::new("proc.tasks", "fs:proc", |_| Ok(Box::new(tasks::Prepare))),
        Factory::new("proc.exec", "bpf:raw_tp", |resolved| {
            Ok(Box::new(events::PrepareExec {
                mode: resolved.request.mode,
                // Applied by the collector, for the reason `syscall` below
                // gives: nothing else would.
                filter: resolved.request.filter.clone(),
                classic: false,
            }))
        }),
        Factory::new("proc.exec", "bpf:tp", |resolved| {
            Ok(Box::new(events::PrepareExec {
                mode: resolved.request.mode,
                filter: resolved.request.filter.clone(),
                classic: true,
            }))
        }),
        Factory::new("proc.exit", "bpf:raw_tp", |resolved| {
            Ok(Box::new(events::PrepareExit {
                mode: resolved.request.mode,
                // Applied by the collector, for the reason `syscall` below
                // gives: nothing else would.
                filter: resolved.request.filter.clone(),
                classic: false,
            }))
        }),
        Factory::new("proc.exit", "bpf:tp", |resolved| {
            Ok(Box::new(events::PrepareExit {
                mode: resolved.request.mode,
                filter: resolved.request.filter.clone(),
                classic: true,
            }))
        }),
        Factory::new("proc.signals", "bpf:raw_tp", |resolved| {
            Ok(Box::new(events::PrepareSignals {
                mode: resolved.request.mode,
                // Applied by the collector, for the reason `syscall` below
                // gives: nothing else would.
                filter: resolved.request.filter.clone(),
                classic: false,
            }))
        }),
        Factory::new("proc.signals", "bpf:tp", |resolved| {
            Ok(Box::new(events::PrepareSignals {
                mode: resolved.request.mode,
                filter: resolved.request.filter.clone(),
                classic: true,
            }))
        }),
        Factory::new("proc.calls", "bpf:raw_tp", |resolved| {
            Ok(Box::new(calls::Prepare {
                mode: resolved.request.mode,
                filter: resolved.request.filter.clone(),
                classic: false,
            }))
        }),
        Factory::new("proc.calls", "bpf:tp", |resolved| {
            Ok(Box::new(calls::Prepare {
                mode: resolved.request.mode,
                filter: resolved.request.filter.clone(),
                classic: true,
            }))
        }),
        Factory::new("proc.syscall", "bpf:raw_tp", |resolved| {
            Ok(Box::new(syscall::Prepare {
                mode: resolved.request.mode,
                // Carried into the collector because the resolver credits every
                // `bpf` backend with filtering in the kernel and then leaves
                // its events alone; see `syscall::Prepare`.
                filter: resolved.request.filter.clone(),
                classic: false,
            }))
        }),
        Factory::new("proc.syscall", "bpf:tp", |resolved| {
            Ok(Box::new(syscall::Prepare {
                mode: resolved.request.mode,
                filter: resolved.request.filter.clone(),
                classic: true,
            }))
        }),
    ]
}
