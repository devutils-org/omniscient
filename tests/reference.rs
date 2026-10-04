//! omn against the tools it replaces: every pair in `catalog/references/`
//! run on the same workload and held to its declared tolerance. The runner
//! is stream reference's (BRIEF.md); each family fills its own module.

#[path = "reference/common.rs"]
mod common;

#[path = "reference/bpftrace.rs"]
mod bpftrace;
#[path = "reference/gpu.rs"]
mod gpu;
#[path = "reference/heap.rs"]
mod heap;
#[path = "reference/io.rs"]
mod io;
#[path = "reference/net.rs"]
mod net;
#[path = "reference/perfrecord.rs"]
mod perfrecord;
#[path = "reference/perfsched.rs"]
mod perfsched;
#[path = "reference/perfstat.rs"]
mod perfstat;
#[path = "reference/power.rs"]
mod power;
#[path = "reference/sysstat.rs"]
mod sysstat;
#[path = "reference/top.rs"]
mod top;

#[allow(dead_code)]
fn all_parsers() -> Vec<common::Parser> {
    let mut all = Vec::new();
    all.extend(perfstat::parsers());
    all.extend(perfrecord::parsers());
    all.extend(perfsched::parsers());
    all.extend(bpftrace::parsers());
    all.extend(sysstat::parsers());
    all.extend(top::parsers());
    all.extend(heap::parsers());
    all.extend(net::parsers());
    all.extend(power::parsers());
    all.extend(gpu::parsers());
    all.extend(io::parsers());
    all
}
