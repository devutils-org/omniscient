//! The allocator programs: what the page and slab allocators handed out.
//!
//! Two sources, one shape. `mem.page_alloc` watches `mm_page_alloc` and
//! `mm_page_free`, `mem.kmalloc` watches `kmalloc` and `kfree`, and both
//! answer the same question in different units: how much did the kernel
//! allocate, in what sizes, on whose behalf.
//!
//! **These fire at rates no other source in this repository approaches.**
//! `mm_page_alloc` is every page the kernel hands to anyone -- a fault, a page
//! table, a slab's backing pages, a network buffer -- and `kmalloc` is every
//! kernel object small enough not to be a page. A program that wrote one ring
//! record per event would make the cost of the measurement grow with the thing
//! being measured, and the numbers would describe a machine running `omn`. So
//! the histogram is built in the kernel: a count per
//! `(task, cgroup, size bucket, direction)`, drained on a tick, which turns
//! hundreds of thousands of events into a few hundred map entries.
//!
//! **The bucket is the size, and for pages it is free.** A page allocation of
//! order *k* is exactly `2^k` pages, and `emit::BucketLayout::Log2` puts `2^k`
//! in bucket `k + 1` -- so the order *is* the bucket, plus one, with no search
//! to run. A slab allocation's byte count is not a power of two, so that one
//! goes through the unrolled `log2` in `mem_program.rs`.
//!
//! **Only allocations are published as events, and frees are counted.** Both
//! tracepoints are attached -- the catalog's chain requires both, and the
//! totals are the first thing anyone asks for -- but an event stream that mixes
//! the two cannot be told apart by anything in the key or the filter
//! vocabulary: series are grouped by key, `direction` is not a key and not a
//! declared filter field, and a histogram of allocations and frees together is
//! a histogram of neither. So the frees are added up and reported as what they
//! are, in the run's own notes, and `docs/families/mem.md` records the catalog
//! change that would make them events.

use std::os::fd::{AsRawFd, OwnedFd};

use super::btf::Btf;
use super::insn::{helper, Insn, Program, BPF_DW, BPF_W, R10};
use super::map::{map_failure, row_map, select_rows, Counters, Rows};
use super::mem_program::{
    bucket_low, check_signature, comm, count_drop, load, long, push_log2_bucket, read_cgroup_id,
    read_comm, read_pid_tgid, word,
};
use super::syscall;
use super::DrainLosses;

/// How many distinct `(task, cgroup, bucket, direction)` rows one drain
/// interval may hold.
///
/// Four thousand rows is a quarter of a megabyte, and it is deliberately not
/// the largest number that would fit. A task allocating pages produces at most
/// a couple of dozen rows however hard it works -- the dimensions are its own
/// identity and the size, and both are bounded -- so the capacity is a bound on
/// *how many tasks allocated at once*, which on this machine's heaviest
/// observed interval was a few hundred.
///
/// A bound nothing has ever reached is a loss path nobody has exercised, and
/// this family's scenarios drive it on purpose. What happens when it is reached
/// is counted per cpu and surfaced as a loss, because a row the kernel could
/// not create is a number of allocations that nothing anywhere records.
const MAX_ROWS: u32 = 4096;

/// The single slot of the drop counter: rows the map had no room for.
const DROP_ROWS: u32 = 0;

/// Where each map's key and value live on the program's stack.
const ROW_VALUE: i16 = -48;
const ROW_KEY: i16 = -80;
const PID_TGID: i16 = -88;
const DROP_KEY: i16 = -92;
/// Where the program keeps the row map it looked up through the generation
/// slot, once per run; see `map::select_rows`.
const ROWS_SLOT: i16 = -104;

/// The layout of a row key, as byte offsets within it.
const KEY_TID: i16 = 0;
const KEY_TGID: i16 = 4;
/// The cgroup the allocating task is in, as `bpf_get_current_cgroup_id` reports
/// it, or zero for a source whose entry declares no cgroup key.
const KEY_CGROUP: i16 = 8;
/// The log2 bucket of the allocation's size. Part of the key is what makes this
/// a histogram rather than a mean: a task that mostly allocates single pages
/// and occasionally allocates two megabytes is two rows here and one
/// indistinguishable average anywhere else.
const KEY_BUCKET: i16 = 16;
/// Zero for an allocation, one for a free.
const KEY_DIRECTION: i16 = 20;
const ROW_KEY_BYTES: usize = 24;

/// The layout of a row value.
const VALUE_COUNT: i16 = 0;
/// The exact sum of the sizes in this row, for the sources whose rows can hold
/// two different sizes. A log2 bucket is a factor of two wide, so a total built
/// from bucket bounds inherits that error and this does not.
const VALUE_TOTAL: i16 = 8;
/// For a slab allocation, what the caller asked for, beside what it was given.
/// The difference between the two is the allocator's rounding, which is the one
/// number a reader cannot get anywhere else.
const VALUE_REQUESTED: i16 = 16;
const VALUE_COMM: i16 = 24;
const ROW_VALUE_BYTES: usize = 40;

/// Which allocator a program is watching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Allocator {
    /// `mm_page_alloc` and `mm_page_free`: the page allocator, in pages.
    Pages,
    /// `kmalloc` and `kfree`: the slab allocator, in bytes.
    Slab,
}

/// Where a tracepoint keeps the size of what it allocated.
#[derive(Debug, Clone, Copy)]
enum Size {
    /// The slot holds a page allocation order, so the size is `1 << order`
    /// pages and the bucket is the order plus one. Nothing is accumulated: every
    /// allocation in such a row is the same size by construction, so the exact
    /// total is `count << order` and userspace computes it without needing a
    /// shift instruction this encoding does not have.
    Order(i16),
    /// The slot holds a byte count, and the slot beside it what was asked for.
    Bytes { given: i16, asked: i16 },
    /// The tracepoint carries no size at all. `kfree` is this: it has the
    /// pointer and the call site and nothing that says how large the object
    /// was, so a freed byte count is not a number this source can report.
    Silent,
}

/// One tracepoint, and what a program attached to it reads out of it.
struct Site {
    tracepoint: &'static str,
    /// Every prototype the program reads the same slots out of, newest first,
    /// in the order the catalog's requirement lists them.
    signatures: &'static [&'static [&'static str]],
    reads: &'static str,
    size: Size,
    direction: u32,
}

impl Allocator {
    /// The two tracepoints, allocation first.
    fn sites(self) -> [Site; 2] {
        match self {
            Allocator::Pages => [
                Site {
                    tracepoint: "mm_page_alloc",
                    signatures: &[&["page *", "unsigned int", "gfp_t", "int"]],
                    reads: "the allocation order",
                    size: Size::Order(8),
                    direction: 0,
                },
                Site {
                    tracepoint: "mm_page_free",
                    signatures: &[&["page *", "unsigned int"]],
                    reads: "the allocation order",
                    size: Size::Order(8),
                    direction: 1,
                },
            ],
            Allocator::Slab => [
                Site {
                    tracepoint: "kmalloc",
                    // The NUMA node arrived after 5.15, as a sixth argument: the
                    // two sizes this reads are in the same slots in both.
                    signatures: &[
                        &[
                            "long unsigned int",
                            "void *",
                            "size_t",
                            "size_t",
                            "gfp_t",
                            "int",
                        ],
                        &["long unsigned int", "void *", "size_t", "size_t", "gfp_t"],
                    ],
                    reads: "the bytes asked for and the bytes given",
                    size: Size::Bytes {
                        given: 24,
                        asked: 16,
                    },
                    direction: 0,
                },
                Site {
                    tracepoint: "kfree",
                    signatures: &[&["long unsigned int", "void *"]],
                    reads: "the freed pointer",
                    size: Size::Silent,
                    direction: 1,
                },
            ],
        }
    }

    /// Allocation tracepoints a kernel splits the first site into, attached
    /// with the same program wherever this kernel publishes them.
    ///
    /// 5.15 fires `kmalloc` for `__kmalloc` and `kmalloc_node` for
    /// `__kmalloc_node` and `__kmalloc_node_track_caller` -- which is where a
    /// socket buffer's data comes from -- and 6.1 folded the second into the
    /// first. Attached to `kmalloc` alone, 5.15 reported 77 KiB for 2000
    /// datagrams of 2 KiB (`tests/scenario_mem.rs`, in the matrix's 5.15
    /// guest). A kernel that has no `kmalloc_node` has folded it, and there is
    /// nothing to add.
    fn companions(self) -> &'static [Site] {
        match self {
            Allocator::Pages => &[],
            Allocator::Slab => &[Site {
                tracepoint: "kmalloc_node",
                signatures: &[&[
                    "long unsigned int",
                    "void *",
                    "size_t",
                    "size_t",
                    "gfp_t",
                    "int",
                ]],
                reads: "the bytes asked for and the bytes given",
                size: Size::Bytes {
                    given: 24,
                    asked: 16,
                },
                direction: 0,
            }],
        }
    }

    /// Whether the entry declares a cgroup key, and therefore whether the
    /// programs pay for `bpf_get_current_cgroup_id` on their hot path.
    fn with_cgroup(self) -> bool {
        match self {
            Allocator::Pages => true,
            Allocator::Slab => false,
        }
    }

    /// What the maps are called, which is what `bpftool map list` shows.
    fn map_prefix(self) -> (&'static str, &'static str, [&'static str; 2]) {
        match self {
            Allocator::Pages => (
                "omn_page_rows",
                "omn_page_drop",
                ["omn_page_alloc", "omn_page_free"],
            ),
            Allocator::Slab => (
                "omn_slab_rows",
                "omn_slab_drop",
                ["omn_slab_alloc", "omn_slab_free"],
            ),
        }
    }
}

/// One `(task, cgroup, size bucket, direction)` row over one drain interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllocRow {
    pub tid: u32,
    pub tgid: u32,
    /// The cgroup id, which is the cgroup directory's inode number, or zero
    /// where the source declares no cgroup key or the task is in none.
    pub cgroup_id: u64,
    /// The log2 bucket, in `emit::BucketLayout::Log2`'s numbering.
    pub bucket: u32,
    /// Whether this row counts frees rather than allocations.
    pub freeing: bool,
    pub count: u64,
    /// The exact sum, where the kernel accumulated one. Zero for a page row,
    /// where [`AllocRow::total_size`] is exact without it.
    total: u64,
    /// The exact sum of what the callers asked for, for slab allocations.
    pub requested: u64,
    pub comm: String,
}

impl AllocRow {
    /// The low bound of this row's bucket: one page count or one byte count.
    ///
    /// This is the value that travels on the event, because it is the one the
    /// emit layer's own log2 bucketing maps back to exactly this bucket.
    pub fn size(&self) -> u64 {
        bucket_low(self.bucket)
    }

    /// The exact total of this row.
    ///
    /// For a slab row it is what the kernel added up. For a page row every
    /// allocation behind the row is `1 << order` pages and the order is part of
    /// the key, so the total is exact without the kernel having accumulated
    /// anything -- which is why the program does not, on the hottest path in
    /// this family.
    pub fn total_size(&self, allocator: Allocator) -> u64 {
        match allocator {
            Allocator::Pages => self.count.saturating_mul(self.size()),
            Allocator::Slab => self.total,
        }
    }

    /// The allocation order, for a page row: the bucket is the order plus one.
    pub fn order(&self) -> u32 {
        self.bucket.saturating_sub(1)
    }

    /// A row built by hand, for the collector's tests.
    ///
    /// `total` is private because what it means depends on which allocator the
    /// row came from -- [`AllocRow::total_size`] is the accessor that knows --
    /// and a test that sets it directly is the only thing outside this file
    /// that has a reason to name it.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub fn for_tests(
        tid: u32,
        tgid: u32,
        cgroup_id: u64,
        bucket: u32,
        freeing: bool,
        count: u64,
        total: u64,
        requested: u64,
        comm: &str,
    ) -> Self {
        Self {
            tid,
            tgid,
            cgroup_id,
            bucket,
            freeing,
            count,
            total,
            requested,
            comm: comm.to_owned(),
        }
    }

    fn decode(key: &[u8], value: &[u8]) -> Option<Self> {
        if key.len() < ROW_KEY_BYTES || value.len() < ROW_VALUE_BYTES {
            return None;
        }
        Some(Self {
            tid: word(key, KEY_TID as usize),
            tgid: word(key, KEY_TGID as usize),
            cgroup_id: long(key, KEY_CGROUP as usize),
            bucket: word(key, KEY_BUCKET as usize),
            freeing: word(key, KEY_DIRECTION as usize) != 0,
            count: long(value, VALUE_COUNT as usize),
            total: long(value, VALUE_TOTAL as usize),
            requested: long(value, VALUE_REQUESTED as usize),
            comm: comm(value, VALUE_COMM as usize),
        })
    }
}

/// Builds the program for one tracepoint.
fn instructions(
    btf: &Btf,
    site: &Site,
    with_cgroup: bool,
    rows_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    check_signature(btf, site.tracepoint, site.signatures, site.reads)?;

    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));

    // The two sizes go in callee-saved registers, because everything after this
    // is helper calls and they clobber r1 through r5.
    match site.size {
        Size::Bytes { given, asked } => {
            program.push(Insn::ldx(BPF_DW, 7, 6, given));
            program.push(Insn::ldx(BPF_DW, 8, 6, asked));
        }
        Size::Order(_) | Size::Silent => {}
    }

    // The bucket, in r3, before anything can call a helper: `push_log2_bucket`
    // computes it through r1 and r2.
    match site.size {
        // A word load rather than a doubleword: the argument is an `unsigned
        // int` in a 64-bit slot, and what the kernel left in the upper half is
        // not part of the order.
        Size::Order(at) => {
            program.push(Insn::ldx(BPF_W, 3, 6, at));
            program.push(Insn::add64_imm(3, 1));
        }
        Size::Bytes { .. } => push_log2_bucket(&mut program, 7),
        Size::Silent => {
            program.push(Insn::mov64_imm(3, 0));
        }
    }
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_BUCKET, 3));
    program.push(Insn::st_imm(
        BPF_W,
        R10,
        ROW_KEY + KEY_DIRECTION,
        site.direction as i32,
    ));

    read_pid_tgid(&mut program, PID_TGID);
    program.push(Insn::ldx(BPF_W, 1, R10, PID_TGID));
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_TID, 1));
    program.push(Insn::ldx(BPF_W, 1, R10, PID_TGID + 4));
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_TGID, 1));
    read_cgroup_id(&mut program, ROW_KEY + KEY_CGROUP, with_cgroup);

    // The row map of this interval, taken once: a drain may swap it at any
    // instruction, and a lookup and an insert that named two maps would put
    // one occurrence in each. See `map::Rows`.
    select_rows(&mut program, rows_fd, ROWS_SLOT, "no_rows");
    program.push(row_map(1, ROWS_SLOT));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, ROW_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to("first", Insn::jeq_imm(0, 0, 0));

    // One allocation shared by every cpu, so every increment has to be one the
    // other cpus cannot interleave with. A load, an add and a store loses
    // whichever increment lands between the first two, and loses most of them
    // on exactly the busy machines these numbers are read on.
    program.push(Insn::mov64_imm(1, 1));
    program.push(Insn::atomic_add(BPF_DW, 0, VALUE_COUNT, 1));
    if let Size::Bytes { .. } = site.size {
        program.push(Insn::atomic_add(BPF_DW, 0, VALUE_TOTAL, 7));
        program.push(Insn::atomic_add(BPF_DW, 0, VALUE_REQUESTED, 8));
    }
    program.jump_to("done", Insn::ja());

    program.label("first");
    program.push(Insn::st_imm(BPF_DW, R10, ROW_VALUE + VALUE_COUNT, 1));
    match site.size {
        Size::Bytes { .. } => {
            program.push(Insn::stx(BPF_DW, R10, ROW_VALUE + VALUE_TOTAL, 7));
            program.push(Insn::stx(BPF_DW, R10, ROW_VALUE + VALUE_REQUESTED, 8));
        }
        // Written rather than left: the value is handed to the kernel whole,
        // and a byte nobody wrote is whatever was on the stack.
        Size::Order(_) | Size::Silent => {
            program.push(Insn::st_imm(BPF_DW, R10, ROW_VALUE + VALUE_TOTAL, 0));
            program.push(Insn::st_imm(BPF_DW, R10, ROW_VALUE + VALUE_REQUESTED, 0));
        }
    }
    // The name is read once, when the row is created, rather than on every
    // allocation: it does not change, and this is the hot path.
    read_comm(&mut program, ROW_VALUE + VALUE_COMM);
    program.push(row_map(1, ROWS_SLOT));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, ROW_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, ROW_VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_ROWS, "done");

    program.jump_to("done", Insn::ja());
    // The slot held no map, which it always does; counted rather than assumed.
    program.label("no_rows");
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_ROWS, "done");

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let built = program.build();
    super::insn::audit(&built)?;
    Ok(built)
}

/// Two loaded, attached programs and the map they share.
///
/// Dropping it detaches both: the links, the programs and the maps are file
/// descriptors, and the kernel unloads a program when the last one closes.
pub struct AllocProgram {
    allocator: Allocator,
    rows: Rows,
    drops: Counters,
    programs: Vec<OwnedFd>,
    _links: Vec<OwnedFd>,
}

impl AllocProgram {
    /// Runs the kernel skipped because one of these programs was already
    /// running on that cpu -- an interrupt re-entering it, since a raw
    /// tracepoint runs with preemption off. The kernel counts them in
    /// `bpf_prog_info.recursion_misses` and nowhere else, so nothing in this
    /// program's own counters can say an occurrence was never seen.
    pub fn skipped(&self) -> u64 {
        crate::bpf::attach::fentry::skipped(self.programs.iter())
    }

    pub fn load(allocator: Allocator) -> Result<Self, String> {
        let btf = super::kernel_btf()?;
        let (rows_name, drops_name, program_names) = allocator.map_prefix();

        let rows = Rows::create(ROW_KEY_BYTES, ROW_VALUE_BYTES, MAX_ROWS, rows_name)
            .map_err(|error| map_failure("allocation histogram map", error))?;
        let drops = Counters::create(1, drops_name)
            .map_err(|error| map_failure("allocation drop counter", error))?;

        let mut programs = Vec::new();
        let mut links = Vec::new();
        for (site, name) in allocator.sites().iter().zip(program_names) {
            let built = instructions(
                btf,
                site,
                allocator.with_cgroup(),
                rows.as_raw_fd(),
                drops.as_raw_fd(),
            )?;
            let program = load(&built, name)?;
            let link = syscall::raw_tracepoint_open(site.tracepoint, program.as_raw_fd()).map_err(
                |error| {
                    format!(
                        "cannot attach to the `{}` tracepoint: {error}",
                        site.tracepoint
                    )
                },
            )?;
            programs.push(program);
            links.push(link);
        }

        for site in allocator.companions() {
            // Absent is the kernel that folded it into the first site; present
            // in another shape is one this program cannot read, and serving
            // the source without it would be a total short by everything it
            // carries.
            if btf.tracepoint_signature(site.tracepoint).is_err() {
                continue;
            }
            let built = instructions(
                btf,
                site,
                allocator.with_cgroup(),
                rows.as_raw_fd(),
                drops.as_raw_fd(),
            )?;
            let program = load(&built, "omn_slab_node")?;
            let link = syscall::raw_tracepoint_open(site.tracepoint, program.as_raw_fd()).map_err(
                |error| {
                    format!(
                        "cannot attach to the `{}` tracepoint: {error}",
                        site.tracepoint
                    )
                },
            )?;
            programs.push(program);
            links.push(link);
        }

        Ok(Self {
            allocator,
            rows,
            drops,
            programs,
            _links: links,
        })
    }

    pub fn allocator(&self) -> Allocator {
        self.allocator
    }

    /// Takes every row the kernel has accumulated since the last drain.
    ///
    /// Each row is a delta over the interval between two drains: the rows are
    /// removed as they are read, so what comes back next time is what happened
    /// in between.
    pub fn drain(&self) -> Result<(Vec<AllocRow>, DrainLosses), String> {
        let drained = self
            .rows
            .drain()
            .map_err(|error| format!("cannot read the allocation histogram: {error}"))?;
        let mut losses = DrainLosses {
            vanished: drained.vanished,
            truncated: drained.truncated,
        };
        let mut rows = Vec::with_capacity(drained.entries.len());
        for (key, value) in &drained.entries {
            match AllocRow::decode(key, value) {
                Some(row) => rows.push(row),
                // A record the wrong size means this reader and the program
                // disagree about the layout, which is a bug rather than a
                // machine condition -- but counting it beats reinterpreting it.
                None => losses.vanished += 1,
            }
        }
        Ok((rows, losses))
    }

    /// Allocations the programs could not count because the map was full.
    pub fn dropped(&self) -> u64 {
        self.drops.read(DROP_ROWS)
    }

    /// How many rows the map can hold, which is what a drop count is against.
    pub fn capacity(&self) -> u32 {
        MAX_ROWS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bpf::mem_program::COMM_BYTES;

    fn kernel_btf() -> Option<Btf> {
        match Btf::from_kernel() {
            Ok(btf) => Some(btf),
            Err(reason) => {
                eprintln!("skipped: {reason}");
                None
            }
        }
    }

    fn built(allocator: Allocator, which: usize) -> Option<Vec<Insn>> {
        let btf = kernel_btf()?;
        Some(
            instructions(
                &btf,
                &allocator.sites()[which],
                allocator.with_cgroup(),
                3,
                4,
            )
            .expect("a program"),
        )
    }

    /// A kernel whose prototypes are not the ones these programs read must be
    /// refused by name, on all four tracepoints.
    ///
    /// This is ground truth from the kernel that is running rather than from
    /// this file: the four constants below are what the programs index by, and
    /// reading `bytes_alloc` out of a slot that holds a pointer produces a
    /// four-hundred-billion-byte allocation rather than an error.
    #[test]
    fn the_running_kernel_declares_the_tracepoints_these_programs_attach_to() {
        let Some(btf) = kernel_btf() else { return };
        for allocator in [Allocator::Pages, Allocator::Slab] {
            for site in allocator.sites() {
                let found = btf
                    .tracepoint_signature(site.tracepoint)
                    .unwrap_or_else(|error| panic!("{}: {error}", site.tracepoint));
                assert!(
                    site.signatures.iter().any(|shape| found == *shape),
                    "this kernel's `{}({})` is none of the prototypes these programs read",
                    site.tracepoint,
                    found.join(", ")
                );
            }
        }
    }

    /// The probe answers from the catalog's line and the attach from the
    /// program; a shape only one of them knows is a source that reads missing
    /// where it could be served, or usable where it refuses.
    #[test]
    fn the_catalog_lists_every_shape_the_programs_read() {
        for (allocator, source) in [
            (Allocator::Pages, "mem.page_alloc"),
            (Allocator::Slab, "mem.kmalloc"),
        ] {
            for site in allocator.sites() {
                crate::bpf::mem_program::assert_catalog_lists(
                    source,
                    site.tracepoint,
                    site.signatures,
                );
            }
        }
    }

    /// A companion is the first site under another name: same slots, same
    /// direction, the same program.
    #[test]
    fn a_companion_site_reads_what_the_site_it_completes_reads() {
        for allocator in [Allocator::Pages, Allocator::Slab] {
            let [first, _] = allocator.sites();
            for companion in allocator.companions() {
                assert_eq!(companion.direction, first.direction);
                assert_eq!(format!("{:?}", companion.size), format!("{:?}", first.size));
                assert!(
                    first.signatures.contains(&companion.signatures[0]),
                    "{} is read in a shape {} is",
                    companion.tracepoint,
                    first.tracepoint
                );
            }
        }
    }

    /// 5.15's `kmalloc` has five arguments where later kernels have six, and
    /// the program reads the two sizes out of the same slots in both: the
    /// shape it accepts differs only in the argument it never reads.
    #[test]
    fn both_kmalloc_shapes_keep_the_sizes_in_the_slots_the_program_reads() {
        let [kmalloc, _] = Allocator::Slab.sites();
        let Size::Bytes { given, asked } = kmalloc.size else {
            panic!("kmalloc carries sizes");
        };
        for shape in kmalloc.signatures {
            assert_eq!(shape[(asked / 8) as usize], "size_t", "{shape:?}");
            assert_eq!(shape[(given / 8) as usize], "size_t", "{shape:?}");
            assert_eq!(
                shape[..5],
                ["long unsigned int", "void *", "size_t", "size_t", "gfp_t"]
            );
        }
    }

    /// A tracepoint whose shape has moved is refused with both shapes in the
    /// message, rather than read wrong.
    #[test]
    fn a_prototype_that_does_not_match_is_refused_with_both_shapes() {
        let Some(btf) = kernel_btf() else { return };
        let wrong = Site {
            tracepoint: "mm_page_alloc",
            signatures: &[&["page *", "unsigned int"]],
            reads: "the allocation order",
            size: Size::Order(8),
            direction: 0,
        };
        let error = instructions(&btf, &wrong, true, 3, 4).expect_err("a refusal");
        assert!(error.contains("this kernel declares"), "{error}");
        assert!(error.contains("gfp_t"), "the kernel's own shape: {error}");
        assert!(
            error.contains("this program is written for"),
            "and the program's: {error}"
        );
    }

    /// The count and both byte totals are accumulated into one allocation
    /// shared by every cpu, so all three have to be atomic. A plain
    /// load-add-store loses whichever increment lands between the two halves of
    /// another cpu's.
    #[test]
    fn every_accumulation_into_a_shared_row_is_atomic() {
        let counted =
            |instructions: Vec<Insn>| instructions.iter().filter(|insn| insn.code == 0xdb).count();
        // Pages: the count, and nothing else -- the total is exact from the
        // bucket and the program does not pay for it.
        assert_eq!(counted(built(Allocator::Pages, 0).unwrap_or_default()), 1);
        // Slab: the count, the bytes given and the bytes asked for.
        assert_eq!(counted(built(Allocator::Slab, 0).unwrap_or_default()), 3);
    }

    /// `kfree` carries no size, and a program that invented one would report a
    /// freed byte count that came from nowhere. The free side of the slab
    /// allocator counts frees and says nothing about their size.
    #[test]
    fn a_free_with_no_size_computes_no_bucket_and_no_total() {
        let Some(instructions) = built(Allocator::Slab, 1) else {
            return;
        };
        assert!(
            !instructions
                .iter()
                .any(|insn| insn.code == 0x77 && insn.imm == 32),
            "the log2 search starts with a 32-bit shift, and there is nothing here to bucket"
        );
        assert_eq!(
            instructions.iter().filter(|insn| insn.code == 0xdb).count(),
            1,
            "a free is counted and nothing about it is summed"
        );
    }

    /// The cgroup helper is on the page programs, whose entry declares a cgroup
    /// key, and not on the slab ones, which are the hottest in the family and
    /// declare none.
    #[test]
    fn only_the_source_that_declares_a_cgroup_key_pays_for_the_cgroup_helper() {
        let Some(pages) = built(Allocator::Pages, 0) else {
            return;
        };
        let slab = built(Allocator::Slab, 0).expect("a program");
        let calls = |instructions: &[Insn], helper: i32| {
            instructions
                .iter()
                .any(|insn| insn.code == 0x85 && insn.imm == helper)
        };
        let cgroup_id = crate::bpf::mem_program::GET_CURRENT_CGROUP_ID;
        assert!(calls(&pages, cgroup_id));
        assert!(!calls(&slab, cgroup_id));
    }

    /// A key or a value the program leaves a hole in is not a smaller record,
    /// it is a record with stack garbage in it: the kernel hashes every byte of
    /// a key, so one byte nobody wrote puts two identical allocations in two
    /// rows.
    #[test]
    fn every_byte_of_a_key_and_a_value_is_accounted_for() {
        let key = [
            (KEY_TID, 4),
            (KEY_TGID, 4),
            (KEY_CGROUP, 8),
            (KEY_BUCKET, 4),
            (KEY_DIRECTION, 4),
        ];
        let value = [
            (VALUE_COUNT, 8),
            (VALUE_TOTAL, 8),
            (VALUE_REQUESTED, 8),
            (VALUE_COMM, COMM_BYTES as i16),
        ];
        for (fields, bytes, what) in [
            (&key[..], ROW_KEY_BYTES, "the row key"),
            (&value[..], ROW_VALUE_BYTES, "the row value"),
        ] {
            let mut covered = vec![false; bytes];
            for (at, size) in fields {
                for byte in *at..*at + *size {
                    let byte = byte as usize;
                    assert!(byte < bytes, "{what} is written past its own end");
                    assert!(!covered[byte], "{what} writes byte {byte} twice");
                    covered[byte] = true;
                }
            }
            assert!(
                covered.iter().all(|byte| *byte),
                "{what} has a byte the program never writes"
            );
        }
    }

    /// The stack regions must not overlap and must stay inside the frame. Two
    /// that overlap build a key partly out of a value, which hashes to a row
    /// nobody looks in -- so the numbers are simply short.
    #[test]
    fn the_stack_regions_do_not_overlap_and_stay_inside_the_frame() {
        let regions = [
            (ROW_VALUE, ROW_VALUE_BYTES as i16, "row value"),
            (ROW_KEY, ROW_KEY_BYTES as i16, "row key"),
            (PID_TGID, 8, "pid and tgid"),
            (DROP_KEY, 4, "drop key"),
            (ROWS_SLOT, 8, "row map slot"),
        ];
        for (at, size, name) in regions {
            assert!(at >= -512 && at + size <= 0, "`{name}` is off the frame");
            for (other_at, other_size, other) in regions {
                if name == other {
                    continue;
                }
                assert!(
                    at + size <= other_at || other_at + other_size <= at,
                    "`{name}` overlaps `{other}`"
                );
            }
        }
    }

    #[test]
    fn a_record_the_wrong_size_is_refused_rather_than_padded() {
        assert!(AllocRow::decode(&[0u8; 8], &[0u8; ROW_VALUE_BYTES]).is_none());
        assert!(AllocRow::decode(&[0u8; ROW_KEY_BYTES], &[0u8; 8]).is_none());
    }

    /// A page row's total is exact without the kernel having added anything up,
    /// because every allocation behind it is the same size. A slab row's is not,
    /// and must come from the accumulator.
    #[test]
    fn a_page_rows_total_is_its_count_times_its_size_and_a_slabs_is_not() {
        let mut key = [0u8; ROW_KEY_BYTES];
        // Bucket 10 is order 9: two megabytes, a transparent huge page.
        key[KEY_BUCKET as usize..KEY_BUCKET as usize + 4].copy_from_slice(&10u32.to_ne_bytes());
        let mut value = [0u8; ROW_VALUE_BYTES];
        value[VALUE_COUNT as usize..VALUE_COUNT as usize + 8].copy_from_slice(&7u64.to_ne_bytes());
        value[VALUE_TOTAL as usize..VALUE_TOTAL as usize + 8].copy_from_slice(&99u64.to_ne_bytes());
        let row = AllocRow::decode(&key, &value).expect("a full record");

        assert_eq!(row.order(), 9);
        assert_eq!(row.size(), 512, "order 9 is 512 pages");
        assert_eq!(row.total_size(Allocator::Pages), 7 * 512);
        assert_eq!(
            row.total_size(Allocator::Slab),
            99,
            "a slab row reports what the kernel summed, not what its bucket implies"
        );
    }

    #[test]
    fn a_rows_direction_and_name_survive_the_round_trip() {
        let mut key = [0u8; ROW_KEY_BYTES];
        key[KEY_DIRECTION as usize..KEY_DIRECTION as usize + 4]
            .copy_from_slice(&1u32.to_ne_bytes());
        key[KEY_CGROUP as usize..KEY_CGROUP as usize + 8].copy_from_slice(&4242u64.to_ne_bytes());
        let mut value = [0u8; ROW_VALUE_BYTES];
        value[VALUE_COMM as usize..VALUE_COMM as usize + 6].copy_from_slice(b"python");
        let row = AllocRow::decode(&key, &value).expect("a full record");
        assert!(row.freeing);
        assert_eq!(row.cgroup_id, 4242);
        assert_eq!(row.comm, "python");
    }

    /// The row map is found through the generation slot once per run, and the
    /// lookup and the insert both use what it found -- the property that makes
    /// the drain's swap safe; see `map::Rows`.
    #[test]
    fn the_row_map_is_taken_from_the_slot_once_per_run() {
        let Some(btf) = kernel_btf() else { return };
        let program =
            instructions(&btf, &Allocator::Pages.sites()[0], true, 3, 4).expect("a program");
        crate::bpf::map::assert_rows_taken_once(&program, 3);
    }
}
