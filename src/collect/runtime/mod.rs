//! Collectors for the `runtime` family.
//!
//! The layer above the kernel: the compiler, the collector and the allocator
//! that a managed runtime carries with it. Most of what this family can see
//! comes from probes the runtime was built to publish, which are Wave 2 work.
//! What is here now is the one signal a runtime writes to a file rather than
//! through a probe: the JIT symbol map.

mod jit_map;

use super::Factory;

/// The collectors this family provides.
pub fn factories() -> Vec<Factory> {
    vec![Factory::new("runtime.jit", "fs:jit-map", |_| {
        Ok(Box::new(jit_map::Prepare))
    })]
}
