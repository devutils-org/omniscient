//! Collectors for the `runtime` family.
//!
//! The layer above the kernel: the compiler, the collector and the allocator
//! that a managed runtime carries with it. Two mechanisms serve it. The JIT
//! symbol map is the one signal a runtime writes to a file rather than through
//! a probe. Everything else is a probe the runtime was built to publish, found
//! in its ELF notes and reached with a uprobe -- `src/bpf/usdt.rs` for the
//! mechanism, `usdt.rs` here for which probes each source means.

mod jit_map;
/// Also read by `sched.mutex`, whose USDT backend reads glibc's probes.
pub(crate) mod objects;
mod usdt;

use super::Factory;
use usdt::Flavour;

/// The collectors this family provides.
pub fn factories() -> Vec<Factory> {
    vec![
        Factory::new("runtime.jit", "fs:jit-map", |_| {
            Ok(Box::new(jit_map::Prepare))
        }),
        Factory::new("runtime.custom", "bpf:usdt", |resolved| {
            Ok(Box::new(prepare(Flavour::Custom, resolved)))
        }),
        Factory::new("runtime.gc", "bpf:usdt", |resolved| {
            Ok(Box::new(prepare(Flavour::Gc, resolved)))
        }),
        Factory::new("runtime.alloc", "bpf:usdt", |resolved| {
            Ok(Box::new(prepare(Flavour::Alloc, resolved)))
        }),
    ]
}

fn prepare(flavour: Flavour, resolved: &crate::resolve::ResolvedRequest) -> usdt::Prepare {
    usdt::Prepare {
        flavour,
        mode: resolved.request.mode,
        target: resolved.request.target.clone(),
        filter: resolved.request.filter.clone(),
    }
}
