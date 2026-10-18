// SPDX-License-Identifier: MIT OR GPL-2.0
//! The page cache programs: what reads looked up, and what the cache had to
//! fill for them.
//!
//! `io.pagecache` and `io.readahead` are two questions about one pair of
//! counts, so they are one program pair here and two readings of it in
//! `src/collect/io/pagecache.rs`:
//!
//! ```text
//!   read(2) ── filemap_read ── filemap_get_pages ─┬─ readahead ── filemap_add_folio ──> mm_filemap_add_to_page_cache(folio)
//!                                                 └──────────────────────────────────> mm_filemap_get_pages(mapping, start, end)
//! ```
//!
//! **`mm_filemap_get_pages` is what a read asked for.** It fires at the end of
//! `filemap_get_pages`, once the pages of `[start, end]` are in the cache --
//! found there, or brought in by the readahead the call triggered. It is the
//! denominator: pages a read looked up, whether they were there or not.
//!
//! **`mm_filemap_add_to_page_cache` is what the cache had to fill.** It fires
//! in `__filemap_add_folio`, once per folio that enters a file's cache, in the
//! context of the task whose read triggered it -- synchronous and asynchronous
//! readahead both run in the reader. A folio may be larger than a page, so the
//! page count is read out of the folio: one page unless the head flag says it
//! is large, and `_nr_pages` (`_folio_nr_pages` before it was renamed) then.
//!
//! **A page a write creates is not a miss, and it is told apart.** An append
//! adds a folio to the cache too, and nothing was looked up or read for it: the
//! write fills it. The difference is visible at the moment it is added, because
//! a write extends `i_size` only after the folio is in the cache and filled,
//! while readahead never reads past the end of the file. So a folio whose index
//! is at or past the page the file currently ends in is counted as *created*,
//! and one inside the file as *filled*. What this cannot tell apart is a
//! whole-page overwrite of a page that was not cached: that is a fill with no
//! read behind it, and the collector says so.
//!
//! Both fire on every page on the machine, so both **count in the kernel**: one
//! row per `(task, file)` per drain interval, holding the pages looked up, the
//! lookups, the pages filled, the folios they came in, and the pages created.
//! The row records when it was created, which is the window's first event
//! rather than the drain that read it (finding 44).

use std::os::fd::{AsRawFd, OwnedFd};

use super::btf::Btf;
use super::insn::{self, helper, read_field, read_pointer, Insn, Program, BPF_DW, BPF_W, R10};
use super::map::{map_failure, row_map, select_rows, Counters, Rows, TaskStorage};
use super::mem_program::{check_signature, comm, count_drop, load, long, read_comm, word};
use super::syscall;
use super::DrainLosses;

/// The two tracepoints, and the prototypes the programs read their slots by.
const GET_PAGES: &str = "mm_filemap_get_pages";
const GET_PAGES_SIGNATURE: [&str; 3] =
    ["address_space *", "long unsigned int", "long unsigned int"];
const ADD: &str = "mm_filemap_add_to_page_cache";
const ADD_SIGNATURE: [&str; 1] = ["folio *"];

/// How many distinct `(task, file)` rows one drain interval may hold.
///
/// A task reading a file is one row however many pages it reads, so this is a
/// bound on how many tasks touched how many files in a tenth of a second. A
/// `find | xargs cat` over a source tree is the workload that approaches it,
/// and what does not fit is counted as a loss.
pub const MAX_ROWS: u32 = 16384;

/// The drop counter's slots.
const DROP_ROWS: u32 = 0;
/// A lookup or a fill whose file could not be named: the mapping had no host
/// inode, or the inode no superblock. Not a loss of a measurement anyone asked
/// for -- a file with no inode is not a file -- but counted, because a number
/// here that is not zero means the chain read something other than a file.
const UNNAMED: u32 = 1;
/// A lookup whose thread the kernel could not give a slot of the `last`
/// storage, which it allocates on the thread's first lookup. Not a lost event:
/// it is counted, but the next continuation of the same read cannot be
/// recognised and is counted again.
const UNTRACKED: u32 = 2;
/// Calls recognised as the continuation of a read already counted.
const CONTINUED: u32 = 3;
const DROP_SLOTS: u32 = 4;

/// Where each region lives on the program's stack.
const ROW_VALUE: i16 = -64;
const ROW_KEY: i16 = -88;
const PID_TGID: i16 = -96;
const DROP_KEY: i16 = -100;
/// Where a pointer read out of a structure lands before it is loaded.
const SCRATCH: i16 = -112;
/// The number of pages the file currently spans, for the created-or-filled
/// test and for clamping a lookup to the file.
const FILE_PAGES: i16 = -120;
/// The row map of this interval, as the generation slot named it, kept in
/// this slot once per run; see `map::select_rows`.
const ROWS_SLOT: i16 = -128;
/// A thread's previous lookup, as byte offsets within its slot of the `last`
/// task storage.
///
/// `filemap_read` calls `filemap_get_pages` once per batch of folios, and each
/// call reports the whole *remaining* range: a 1 MiB read reports
/// `[0, 255]`, then `[31, 255]`, then `[62, 255]`, and so on. Summed, that
/// counted 3168 pages for a 1024-page file. A call from the same thread, on
/// the same mapping, with the same end and a start that has advanced, is the
/// read it continues, and adds nothing.
///
/// The slot lives in the thread itself (`BPF_MAP_TYPE_TASK_STORAGE`), so it
/// dies with the thread. It used to be a hash keyed by tid, which kept a
/// thread's entry after it exited until a userspace sweep removed it, and lost
/// any entry the kernel refreshed between the sweep's look and its removal.
/// Neither can happen to a slot the kernel frees with its task, and a recycled
/// tid starts with an empty one.
const LAST_MAPPING: i16 = 0;
const LAST_START: i16 = 8;
const LAST_END: i16 = 16;
const LAST_VALUE_BYTES: usize = 24;

/// `BPF_LOCAL_STORAGE_GET_F_CREATE`: a thread's first lookup makes its slot,
/// zeroed, and a zero mapping matches no file.
const F_CREATE: i32 = 1;

/// A row key, as byte offsets within it.
const KEY_TID: i16 = 0;
const KEY_TGID: i16 = 4;
/// `inode.i_ino`.
const KEY_INO: i16 = 8;
/// `super_block.s_dev`, the kernel's packed `dev_t`.
const KEY_DEV: i16 = 16;
/// Written as zero: the kernel hashes every byte of a key.
const KEY_PAD: i16 = 20;
const ROW_KEY_BYTES: usize = 24;

/// A row value, as byte offsets within it.
///
/// When the row was created: the window's first event of this key, which is
/// what the event is stamped with.
const VALUE_FIRST_NS: i16 = 0;
/// Pages `mm_filemap_get_pages` reported: `end - start + 1`, summed.
const VALUE_ASKED: i16 = 8;
/// Calls of `mm_filemap_get_pages`.
const VALUE_LOOKUPS: i16 = 16;
/// Pages that entered the cache inside the file's current length.
const VALUE_FILLED: i16 = 24;
/// The folios those pages came in.
const VALUE_FOLIOS: i16 = 32;
/// Pages that entered the cache at or past the file's current end.
const VALUE_CREATED: i16 = 40;
const VALUE_COMM: i16 = 48;
const ROW_VALUE_BYTES: usize = 64;

/// The page size the created-or-filled test divides `i_size` by. The kernel's
/// `PAGE_SHIFT` is not in BTF, and it is 12 on every architecture this build's
/// raw tracepoints are exercised on; `load` refuses a machine whose page size
/// says otherwise rather than dividing by the wrong one.
const PAGE_SHIFT: i32 = 12;

/// One `(task, file)` row over one drain interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageCacheRow {
    pub tid: u32,
    pub tgid: u32,
    pub ino: u64,
    /// The superblock's device, as the kernel packs a `dev_t`.
    pub dev: u32,
    /// When the row's first event happened, `CLOCK_MONOTONIC`.
    pub first_ns: u64,
    /// Pages reads looked up, whether the cache had them or not.
    pub asked: u64,
    /// How many lookups those pages came in.
    pub lookups: u64,
    /// Pages the cache had to fill inside the file: demand misses and readahead.
    pub filled: u64,
    pub folios: u64,
    /// Pages added at or past the file's end, which a write creates.
    pub created: u64,
    pub comm: String,
}

impl PageCacheRow {
    fn decode(key: &[u8], value: &[u8]) -> Option<Self> {
        if key.len() < ROW_KEY_BYTES || value.len() < ROW_VALUE_BYTES {
            return None;
        }
        Some(Self {
            tid: word(key, KEY_TID as usize),
            tgid: word(key, KEY_TGID as usize),
            ino: long(key, KEY_INO as usize),
            dev: word(key, KEY_DEV as usize),
            first_ns: long(value, VALUE_FIRST_NS as usize),
            asked: long(value, VALUE_ASKED as usize),
            lookups: long(value, VALUE_LOOKUPS as usize),
            filled: long(value, VALUE_FILLED as usize),
            folios: long(value, VALUE_FOLIOS as usize),
            created: long(value, VALUE_CREATED as usize),
            comm: comm(value, VALUE_COMM as usize),
        })
    }

    /// The same row with another's counts added in, for a `count` run.
    pub fn absorb(&mut self, other: &PageCacheRow) {
        self.first_ns = self.first_ns.min(other.first_ns);
        self.asked += other.asked;
        self.lookups += other.lookups;
        self.filled += other.filled;
        self.folios += other.folios;
        self.created += other.created;
        self.comm.clone_from(&other.comm);
    }

    /// A row built by hand, for the collector's tests.
    #[cfg(test)]
    pub fn for_tests(tid: u32, ino: u64, asked: u64, filled: u64, created: u64) -> Self {
        Self {
            tid,
            tgid: tid,
            ino,
            dev: 41,
            first_ns: 1,
            asked,
            lookups: 1,
            filled,
            folios: filled,
            created,
            comm: "test".to_owned(),
        }
    }
}

/// Whose events the programs keep. Neither set is the whole machine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Narrowing {
    pub tgid: Option<u32>,
    pub tid: Option<u32>,
    /// The map of the processes `tgid` started, which [`PageCacheProgram::load`]
    /// creates and fills in: the caller leaves it `None`.
    pub descendants: Option<i32>,
}

/// Where the fields of the chain from a folio to its file sit on this kernel.
struct Offsets {
    folio_flags: i32,
    folio_mapping: i32,
    folio_index: i32,
    folio_nr_pages: i32,
    /// The bit of `folio.flags` that says the folio is larger than a page.
    pg_head: i32,
    mapping_host: i32,
    i_ino: i32,
    i_size: i32,
    i_sb: i32,
    s_dev: i32,
}

impl Offsets {
    fn read(btf: &Btf) -> Result<Self, String> {
        Ok(Self {
            folio_flags: btf.member_offset("folio", "flags")?,
            folio_mapping: btf.member_offset("folio", "mapping")?,
            folio_index: btf.member_offset("folio", "index")?,
            // `_folio_nr_pages` until the kernels after 6.12 renamed it
            // `_nr_pages` (6.8 and 6.12 have the first, 6.17 and later the
            // second, as distroproof's images showed).
            folio_nr_pages: btf
                .member_offset("folio", "_nr_pages")
                .or_else(|_| btf.member_offset("folio", "_folio_nr_pages"))
                .map_err(|reason| {
                    format!(
                        "{reason} (nor `_folio_nr_pages`): this kernel does not keep a large \
                         folio's page count where this program reads it, and counting every \
                         folio as one page would report a two-megabyte fill as four kilobytes"
                    )
                })?,
            pg_head: pg_head(btf)?,
            mapping_host: btf.member_offset("address_space", "host")?,
            i_ino: btf.member_offset("inode", "i_ino")?,
            i_size: btf.member_offset("inode", "i_size")?,
            i_sb: btf.member_offset("inode", "i_sb")?,
            s_dev: btf.member_offset("super_block", "s_dev")?,
        })
    }
}

/// `PG_head`'s bit number, as this kernel's `enum pageflags` declares it.
///
/// The enum has gained and lost members between releases, so the number is
/// looked up rather than written down. It has to fit a jump's immediate, which
/// every flag below the thirty-first does.
fn pg_head(btf: &Btf) -> Result<i32, String> {
    (0..31)
        .find(|bit| btf.enum_variant("pageflags", *bit) == Some("PG_head"))
        .map(|bit| bit as i32)
        .ok_or_else(|| {
            "this kernel's `enum pageflags` has no `PG_head` below bit 31, so a large folio \
             cannot be told from a small one and its pages cannot be counted"
                .to_owned()
        })
}

/// Skips an event of a task the request did not name.
fn narrow(program: &mut Program, narrowing: Narrowing) {
    if let Some(tgid) = narrowing.tgid {
        program.push(Insn::ldx(BPF_W, 1, R10, PID_TGID + 4));
        match narrowing.descendants {
            None => {
                program.jump_to("done", Insn::jne_imm(1, tgid as i32, 0));
            }
            // The target compared as a constant, any other thread group looked
            // up in the processes it started: a launched shell's `cat` is the
            // run's.
            Some(map) => {
                program.jump_to("narrow_kept", Insn::jeq_imm(1, tgid as i32, 0));
                program.extend(Insn::load_map_fd(1, map));
                program.push(Insn::mov64_reg(2, R10));
                program.push(Insn::add64_imm(2, (PID_TGID + 4) as i32));
                program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
                program.jump_to("done", Insn::jeq_imm(0, 0, 0));
                program.label("narrow_kept");
            }
        }
    }
    if let Some(tid) = narrowing.tid {
        program.push(Insn::ldx(BPF_W, 1, R10, PID_TGID));
        program.jump_to("done", Insn::jne_imm(1, tid as i32, 0));
    }
}

/// Reads the current task into the key, and leaves if it is not one the
/// request named. Clobbers `r0` through `r5`.
fn task(program: &mut Program, narrowing: Narrowing) {
    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::stx(BPF_DW, R10, PID_TGID, 0));
    narrow(program, narrowing);
    program.push(Insn::ldx(BPF_W, 1, R10, PID_TGID));
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_TID, 1));
    program.push(Insn::ldx(BPF_W, 1, R10, PID_TGID + 4));
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_TGID, 1));
}

/// Writes the file's inode number and superblock device into the key, from the
/// inode in `host`, or leaves through `unnamed` when the inode has no
/// superblock. Clobbers `host`.
fn file_identity(program: &mut Program, at: &Offsets, host: u8) {
    read_field(program, ROW_KEY + KEY_INO, 8, host, at.i_ino);
    read_pointer(program, SCRATCH, host, host, at.i_sb);
    program.jump_to("unnamed", Insn::jeq_imm(host, 0, 0));
    program.push(Insn::st_imm(BPF_DW, R10, ROW_KEY + KEY_DEV, 0));
    read_field(program, ROW_KEY + KEY_DEV, 4, host, at.s_dev);
    program.push(Insn::st_imm(BPF_W, R10, ROW_KEY + KEY_PAD, 0));
}

/// The tail every program shares: find the row, add to it, or create it.
///
/// `add` names the value fields this event adds to and the register or
/// constant each adds; a field not named starts at zero in a new row.
fn upsert(program: &mut Program, rows_fd: i32, drops_fd: i32, add: &[(i16, Addend)]) {
    // The row map of this interval, taken once: a drain may swap it at any
    // instruction, and a lookup and an insert that named two maps would put
    // one occurrence in each. See `map::Rows`. The addends are in r6 to r9,
    // which the lookup leaves alone.
    select_rows(program, rows_fd, ROWS_SLOT, "no_rows");
    program.push(row_map(1, ROWS_SLOT));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, ROW_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to("first", Insn::jeq_imm(0, 0, 0));

    // One row shared by every cpu, so every increment is atomic: a load, an
    // add and a store loses whichever increment lands between the first two.
    for (field, addend) in add {
        match addend {
            Addend::Register(register) => {
                program.push(Insn::atomic_add(BPF_DW, 0, *field, *register));
            }
            Addend::One => {
                program.push(Insn::mov64_imm(1, 1));
                program.push(Insn::atomic_add(BPF_DW, 0, *field, 1));
            }
        };
    }
    program.jump_to("done", Insn::ja());

    program.label("first");
    for field in [
        VALUE_ASKED,
        VALUE_LOOKUPS,
        VALUE_FILLED,
        VALUE_FOLIOS,
        VALUE_CREATED,
    ] {
        match add.iter().find(|(named, _)| *named == field) {
            Some((_, Addend::Register(register))) => {
                program.push(Insn::stx(BPF_DW, R10, ROW_VALUE + field, *register));
            }
            Some((_, Addend::One)) => {
                program.push(Insn::st_imm(BPF_DW, R10, ROW_VALUE + field, 1));
            }
            None => {
                program.push(Insn::st_imm(BPF_DW, R10, ROW_VALUE + field, 0));
            }
        }
    }
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, ROW_VALUE + VALUE_FIRST_NS, 0));
    // Once per row, not per page: the name does not change, and this is the
    // hot path.
    read_comm(program, ROW_VALUE + VALUE_COMM);
    program.push(row_map(1, ROWS_SLOT));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, ROW_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, ROW_VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    count_drop(program, drops_fd, DROP_KEY, DROP_ROWS, "done");
    program.jump_to("done", Insn::ja());

    // The slot held no map, which it always does; counted rather than assumed.
    program.label("no_rows");
    count_drop(program, drops_fd, DROP_KEY, DROP_ROWS, "done");
    program.jump_to("done", Insn::ja());

    program.label("unnamed");
    count_drop(program, drops_fd, DROP_KEY, UNNAMED, "done");

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
}

/// What an event adds to one field of its row.
#[derive(Debug, Clone, Copy)]
enum Addend {
    Register(u8),
    One,
}

/// `mm_filemap_get_pages(mapping, start, end)`: a read looked up the pages of
/// `[start, end]` that the file has, unless it is the continuation of a read
/// already counted.
fn get_pages_instructions(
    btf: &Btf,
    narrowing: Narrowing,
    rows_fd: i32,
    last_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    check_signature(
        btf,
        GET_PAGES,
        &[&GET_PAGES_SIGNATURE],
        "the mapping and the page range",
    )?;
    let at = Offsets::read(btf)?;

    let mut program = Program::new();
    // The three arguments, before any helper can clobber the context pointer.
    program.push(Insn::ldx(BPF_DW, 9, 1, 0));
    program.push(Insn::ldx(BPF_DW, 7, 1, 8));
    program.push(Insn::ldx(BPF_DW, 8, 1, 16));

    task(&mut program, narrowing);
    program.jump_to("unnamed", Insn::jeq_imm(9, 0, 0));
    read_pointer(&mut program, SCRATCH, 6, 9, at.mapping_host);
    program.jump_to("unnamed", Insn::jeq_imm(6, 0, 0));

    // The range, clamped to the pages the file has: the end is where the
    // read's buffer ends, and a 1 MiB read of a 100 KiB file reports 256
    // pages of which 25 exist. r1 is the file's last page.
    read_pointer(&mut program, SCRATCH, 1, 6, at.i_size);
    program.push(Insn::add64_imm(1, (1 << PAGE_SHIFT) - 1));
    program.push(Insn::rsh64_imm(1, PAGE_SHIFT));
    program.push(Insn::add64_imm(1, -1));
    program.push(Insn::mov64_reg(2, 8));
    program.push(Insn::sub64_reg(2, 1));
    program.jump_to("clamp", Insn::jsge_imm(2, 1, 0));
    program.jump_to("clamped", Insn::ja());
    program.label("clamp");
    program.push(Insn::mov64_reg(8, 1));
    program.label("clamped");
    // A read that starts at or past the end asked for nothing the file has.
    program.push(Insn::mov64_reg(2, 8));
    program.push(Insn::sub64_reg(2, 7));
    program.jump_to("in_file", Insn::jsge_imm(2, 0, 0));
    program.jump_to("done", Insn::ja());
    program.label("in_file");

    // The thread's previous lookup: the same mapping, the same end and a start
    // that advanced is the next batch of a read already counted. r6 to r9 --
    // the inode, the range and the mapping -- survive the two helper calls.
    program.push(Insn::call(helper::GET_CURRENT_TASK_BTF));
    program.push(Insn::mov64_reg(2, 0));
    program.extend(Insn::load_map_fd(1, last_fd));
    program.push(Insn::mov64_imm(3, 0));
    program.push(Insn::mov64_imm(4, F_CREATE));
    program.push(Insn::call(helper::TASK_STORAGE_GET));
    program.jump_to("untracked", Insn::jeq_imm(0, 0, 0));
    program.push(Insn::ldx(BPF_DW, 1, 0, LAST_MAPPING));
    program.push(Insn::sub64_reg(1, 9));
    program.jump_to("fresh", Insn::jne_imm(1, 0, 0));
    program.push(Insn::ldx(BPF_DW, 1, 0, LAST_END));
    program.push(Insn::sub64_reg(1, 8));
    program.jump_to("fresh", Insn::jne_imm(1, 0, 0));
    program.push(Insn::mov64_reg(1, 7));
    program.push(Insn::ldx(BPF_DW, 2, 0, LAST_START));
    program.push(Insn::sub64_reg(1, 2));
    program.jump_to("continued", Insn::jsge_imm(1, 1, 0));
    program.jump_to("fresh", Insn::ja());

    program.label("continued");
    // Moved forward in place, so a third batch continues the second. The
    // slot belongs to this thread alone, so a plain store is enough.
    program.push(Insn::stx(BPF_DW, 0, LAST_START, 7));
    count_drop(&mut program, drops_fd, DROP_KEY, CONTINUED, "done");
    program.jump_to("done", Insn::ja());

    program.label("fresh");
    program.push(Insn::stx(BPF_DW, 0, LAST_MAPPING, 9));
    program.push(Insn::stx(BPF_DW, 0, LAST_START, 7));
    program.push(Insn::stx(BPF_DW, 0, LAST_END, 8));
    program.jump_to("tracked", Insn::ja());

    program.label("untracked");
    count_drop(&mut program, drops_fd, DROP_KEY, UNTRACKED, "tracked");
    program.label("tracked");

    file_identity(&mut program, &at, 6);
    // The page count, inclusive of both ends.
    program.push(Insn::sub64_reg(8, 7));
    program.push(Insn::add64_imm(8, 1));
    upsert(
        &mut program,
        rows_fd,
        drops_fd,
        &[
            (VALUE_ASKED, Addend::Register(8)),
            (VALUE_LOOKUPS, Addend::One),
        ],
    );
    let built = program.build();
    insn::audit(&built)?;
    Ok(built)
}

/// `mm_filemap_add_to_page_cache(folio)`: a folio entered a file's cache.
fn add_instructions(
    btf: &Btf,
    narrowing: Narrowing,
    rows_fd: i32,
    drops_fd: i32,
) -> Result<Vec<Insn>, String> {
    check_signature(btf, ADD, &[&ADD_SIGNATURE], "the folio")?;
    let at = Offsets::read(btf)?;

    let mut program = Program::new();
    program.push(Insn::ldx(BPF_DW, 7, 1, 0));
    task(&mut program, narrowing);

    // The folio's page count, into r8: one, unless the head flag says the
    // folio is large, and then the count kept in its second page.
    read_pointer(&mut program, SCRATCH, 1, 7, at.folio_flags);
    program.push(Insn::mov64_imm(8, 1));
    program.jump_to("large", Insn::jset_imm(1, 1 << at.pg_head, 0));
    program.jump_to("counted", Insn::ja());
    program.label("large");
    program.push(Insn::st_imm(BPF_DW, R10, SCRATCH, 0));
    read_field(&mut program, SCRATCH, 4, 7, at.folio_nr_pages);
    program.push(Insn::ldx(BPF_W, 8, R10, SCRATCH));
    program.label("counted");

    // The file, through the folio's mapping.
    read_pointer(&mut program, SCRATCH, 9, 7, at.folio_mapping);
    program.jump_to("unnamed", Insn::jeq_imm(9, 0, 0));
    read_pointer(&mut program, SCRATCH, 6, 9, at.mapping_host);
    program.jump_to("unnamed", Insn::jeq_imm(6, 0, 0));

    // The pages the file spans now, rounded up. A folio whose index is at or
    // past that is one no read could have asked for: a write is creating it.
    read_pointer(&mut program, SCRATCH, 1, 6, at.i_size);
    program.push(Insn::add64_imm(1, (1 << PAGE_SHIFT) - 1));
    program.push(Insn::rsh64_imm(1, PAGE_SHIFT));
    program.push(Insn::stx(BPF_DW, R10, FILE_PAGES, 1));
    read_pointer(&mut program, SCRATCH, 1, 7, at.folio_index);
    program.push(Insn::ldx(BPF_DW, 2, R10, FILE_PAGES));
    program.push(Insn::sub64_reg(1, 2));
    // r7 is the folio no longer, and becomes the verdict: non-zero for a page
    // created at the end of the file.
    program.push(Insn::mov64_imm(7, 0));
    program.jump_to("past_end", Insn::jsge_imm(1, 0, 0));
    program.jump_to("judged", Insn::ja());
    program.label("past_end");
    program.push(Insn::mov64_imm(7, 1));
    program.label("judged");

    file_identity(&mut program, &at, 6);

    // Three addends in the three registers a helper call leaves alone, so one
    // tail serves both verdicts: r9 the pages filled, r6 the pages created, r7
    // the folios a fill came in. The other two are zero, and adding zero to a
    // row is one instruction on a path that has already paid for a lookup.
    program.jump_to("creating", Insn::jne_imm(7, 0, 0));
    program.push(Insn::mov64_reg(9, 8));
    program.push(Insn::mov64_imm(6, 0));
    program.push(Insn::mov64_imm(7, 1));
    program.jump_to("addends", Insn::ja());
    program.label("creating");
    program.push(Insn::mov64_imm(9, 0));
    program.push(Insn::mov64_reg(6, 8));
    program.push(Insn::mov64_imm(7, 0));
    program.label("addends");

    upsert(
        &mut program,
        rows_fd,
        drops_fd,
        &[
            (VALUE_FILLED, Addend::Register(9)),
            (VALUE_FOLIOS, Addend::Register(7)),
            (VALUE_CREATED, Addend::Register(6)),
        ],
    );
    let built = program.build();
    insn::audit(&built)?;
    Ok(built)
}

/// The two loaded, attached programs and the rows they share.
///
/// Dropping it detaches both: the links, the programs and the maps are file
/// descriptors, and the kernel unloads a program when the last one closes.
pub struct PageCacheProgram {
    rows: Rows,
    /// Held so the storage outlives the programs that write into it.
    _last: TaskStorage,
    drops: Counters,
    programs: Vec<OwnedFd>,
    _links: Vec<OwnedFd>,
    /// The processes a process target started, followed from the fork.
    descendants: Option<super::sched_common::Descendants>,
}

impl PageCacheProgram {
    pub fn load(mut narrowing: Narrowing) -> Result<Self, String> {
        let btf = super::kernel_btf()?;
        let descendants = match narrowing.tgid {
            Some(root) => Some(super::sched_common::Descendants::follow(root)?),
            None => None,
        };
        narrowing.descendants = descendants
            .as_ref()
            .map(super::sched_common::Descendants::map_fd);
        // SAFETY: `sysconf` takes an integer and returns one.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if page != 1 << PAGE_SHIFT {
            return Err(format!(
                "this machine's pages are {page} bytes, and the page cache programs divide a \
                 file's length by 4096 to tell a page a read filled from one a write created"
            ));
        }
        let rows = Rows::create(ROW_KEY_BYTES, ROW_VALUE_BYTES, MAX_ROWS, "omn_pc_rows")
            .map_err(|error| map_failure("page cache row map", error))?;
        let last = TaskStorage::create(LAST_VALUE_BYTES, "omn_pc_last")
            .map_err(|error| map_failure("page cache per-thread storage", error))?;
        let drops = Counters::create(DROP_SLOTS, "omn_pc_drop")
            .map_err(|error| map_failure("page cache drop counter", error))?;

        // The fill is attached first. A lookup attached alone for an instant
        // reports pages found with none filled, which reads as a hit; a fill
        // alone reads as a miss nobody asked for, which the collector already
        // reports as what it is.
        let sites = [
            (
                ADD,
                add_instructions(btf, narrowing, rows.as_raw_fd(), drops.as_raw_fd())?,
                "omn_pc_add",
            ),
            (
                GET_PAGES,
                get_pages_instructions(
                    btf,
                    narrowing,
                    rows.as_raw_fd(),
                    last.as_raw_fd(),
                    drops.as_raw_fd(),
                )?,
                "omn_pc_get",
            ),
        ];
        let mut programs = Vec::new();
        let mut links = Vec::new();
        for (tracepoint, built, name) in sites {
            let program = load(&built, name)?;
            let link =
                syscall::raw_tracepoint_open(tracepoint, program.as_raw_fd()).map_err(|error| {
                    format!("cannot attach to the `{tracepoint}` tracepoint: {error}")
                })?;
            programs.push(program);
            links.push(link);
        }
        Ok(Self {
            rows,
            _last: last,
            drops,
            programs,
            _links: links,
            descendants,
        })
    }

    /// Takes every row the kernel has counted since the last drain.
    pub fn drain(&self) -> Result<(Vec<PageCacheRow>, DrainLosses), String> {
        let drained = self
            .rows
            .drain()
            .map_err(|error| format!("cannot read the page cache rows: {error}"))?;
        let mut losses = DrainLosses {
            vanished: drained.vanished,
            truncated: drained.truncated,
        };
        let mut rows = Vec::with_capacity(drained.entries.len());
        for (key, value) in &drained.entries {
            match PageCacheRow::decode(key, value) {
                Some(row) => rows.push(row),
                None => losses.vanished += 1,
            }
        }
        Ok((rows, losses))
    }

    /// Lookups whose thread the kernel could not give a slot, so that a
    /// continuation of theirs may have been counted as a lookup of its own.
    pub fn untracked(&self) -> u64 {
        self.drops.read(UNTRACKED)
    }

    /// Lookups recognised as the next batch of a read already counted.
    pub fn continued(&self) -> u64 {
        self.drops.read(CONTINUED)
    }

    /// Events whose row could not be created because the map was full.
    pub fn dropped(&self) -> u64 {
        self.drops.read(DROP_ROWS)
    }

    /// Events whose mapping named no inode or superblock.
    pub fn unnamed(&self) -> u64 {
        self.drops.read(UNNAMED)
    }

    /// Runs the kernel skipped because one of these was already running on
    /// that cpu; see `BlockProgram::skipped`.
    pub fn skipped(&self) -> u64 {
        crate::bpf::attach::fentry::skipped(self.programs.iter())
            + self
                .descendants
                .as_ref()
                .map_or(0, super::sched_common::Descendants::skipped)
    }

    /// Processes the target started that the map of its descendants had no
    /// room for: their page-cache traffic is in no row.
    pub fn unfollowed(&self) -> u64 {
        self.descendants
            .as_ref()
            .map_or(0, super::sched_common::Descendants::missed)
    }

    pub fn capacity(&self) -> u32 {
        MAX_ROWS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bpf::mem_program::COMM_BYTES;

    fn kernel_btf() -> Option<&'static Btf> {
        match super::super::kernel_btf() {
            Ok(btf) => Some(btf),
            Err(reason) => {
                eprintln!("skipped: {reason}");
                None
            }
        }
    }

    /// A key or a value with a byte nobody writes is a record with stack
    /// garbage in it: the kernel hashes every byte of a key, so one stray byte
    /// splits one task's reads of one file across two rows.
    #[test]
    fn every_byte_of_a_key_and_a_value_is_accounted_for() {
        let key = [
            (KEY_TID, 4),
            (KEY_TGID, 4),
            (KEY_INO, 8),
            (KEY_DEV, 4),
            (KEY_PAD, 4),
        ];
        let value = [
            (VALUE_FIRST_NS, 8),
            (VALUE_ASKED, 8),
            (VALUE_LOOKUPS, 8),
            (VALUE_FILLED, 8),
            (VALUE_FOLIOS, 8),
            (VALUE_CREATED, 8),
            (VALUE_COMM, COMM_BYTES as i16),
        ];
        let last = [(LAST_MAPPING, 8), (LAST_START, 8), (LAST_END, 8)];
        for (fields, bytes, what) in [
            (&key[..], ROW_KEY_BYTES, "the row key"),
            (&value[..], ROW_VALUE_BYTES, "the row value"),
            (&last[..], LAST_VALUE_BYTES, "a thread's last lookup"),
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

    /// The regions must not overlap and must stay inside the frame. Finding 46
    /// is a row value that grew into the key beside it and wrote a timestamp
    /// over a vector number.
    #[test]
    fn the_stack_regions_do_not_overlap_and_stay_inside_the_frame() {
        let regions = [
            (ROW_VALUE, ROW_VALUE_BYTES as i16, "row value"),
            (ROW_KEY, ROW_KEY_BYTES as i16, "row key"),
            (PID_TGID, 8, "pid and tgid"),
            (DROP_KEY, 4, "drop key"),
            (SCRATCH, 8, "scratch"),
            (FILE_PAGES, 8, "file pages"),
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

    /// Every store the programs make to the stack lands inside a region the
    /// test above knows about: a region list that forgot one would pass while
    /// the program wrote somewhere else.
    #[test]
    fn every_stack_store_of_both_programs_lands_in_a_declared_region() {
        let Some(btf) = kernel_btf() else { return };
        let regions = [
            (ROW_VALUE, ROW_VALUE_BYTES as i16),
            (ROW_KEY, ROW_KEY_BYTES as i16),
            (PID_TGID, 8),
            (DROP_KEY, 4),
            (SCRATCH, 8),
            (FILE_PAGES, 8),
            (ROWS_SLOT, 8),
        ];
        for built in [
            add_instructions(btf, Narrowing::default(), 3, 4).expect("the fill program"),
            get_pages_instructions(btf, Narrowing::default(), 3, 5, 4).expect("the lookup program"),
        ] {
            for insn in &built {
                let class = insn.code & 0x07;
                // BPF_ST and BPF_STX, through the frame pointer.
                if !(class == 0x02 || class == 0x03) || insn.regs & 0x0f != R10 {
                    continue;
                }
                let width = match insn.code & 0x18 {
                    0x00 => 4,
                    0x08 => 2,
                    0x10 => 1,
                    _ => 8,
                };
                assert!(
                    regions
                        .iter()
                        .any(|(at, size)| insn.off >= *at && insn.off + width <= at + size),
                    "a store of {width} bytes at {} is in no declared region",
                    insn.off
                );
            }
        }
    }

    /// Both programs find the row map through the generation slot once per
    /// run, and the lookup and the insert both use what they found -- the
    /// property that makes the drain's swap safe; see `map::Rows`.
    #[test]
    fn the_row_map_is_taken_from_the_slot_once_per_run() {
        let Some(btf) = kernel_btf() else { return };
        for built in [
            add_instructions(btf, Narrowing::default(), 3, 4).expect("the fill program"),
            get_pages_instructions(btf, Narrowing::default(), 3, 5, 4).expect("the lookup program"),
        ] {
            crate::bpf::map::assert_rows_taken_once(&built, 3);
        }
    }

    #[test]
    fn the_running_kernel_declares_the_prototypes_these_programs_read() {
        let Some(btf) = kernel_btf() else { return };
        check_signature(btf, GET_PAGES, &[&GET_PAGES_SIGNATURE], "the range").expect(GET_PAGES);
        check_signature(btf, ADD, &[&ADD_SIGNATURE], "the folio").expect(ADD);
        Offsets::read(btf).expect("every field of the chain from a folio to its file");
    }

    /// The flag bit is looked up, and on the kernels this has run on it is 6.
    /// A different number is not wrong -- the enum moves -- but it must be one
    /// the kernel names `PG_head`.
    #[test]
    fn the_head_flag_is_the_one_the_kernel_names() {
        let Some(btf) = kernel_btf() else { return };
        let bit = pg_head(btf).expect("a head flag");
        assert_eq!(
            btf.enum_variant("pageflags", i64::from(bit)),
            Some("PG_head")
        );
    }

    /// One row, shared by every cpu: each increment must be atomic.
    #[test]
    fn every_accumulation_into_a_shared_row_is_atomic() {
        let Some(btf) = kernel_btf() else { return };
        let atomics = |built: Vec<Insn>| built.iter().filter(|insn| insn.code == 0xdb).count();
        assert_eq!(
            atomics(get_pages_instructions(btf, Narrowing::default(), 3, 5, 4).expect("a program")),
            2,
            "pages asked and lookups"
        );
        assert_eq!(
            atomics(add_instructions(btf, Narrowing::default(), 3, 4).expect("a program")),
            3,
            "pages filled, folios, pages created"
        );
    }

    /// Narrowing is two compares on the thread group and the thread, and a
    /// program for the whole machine has neither.
    #[test]
    fn a_narrowed_program_compares_the_task_and_a_system_one_does_not() {
        let Some(btf) = kernel_btf() else { return };
        let compares = |narrowing| {
            get_pages_instructions(btf, narrowing, 3, 5, 4)
                .expect("a program")
                .iter()
                .filter(|insn| insn.code == 0x55 && insn.imm == 4242)
                .count()
        };
        assert_eq!(compares(Narrowing::default()), 0);
        assert_eq!(
            compares(Narrowing {
                tgid: Some(4242),
                tid: None,
                descendants: None,
            }),
            1
        );
    }

    #[test]
    fn a_record_the_wrong_size_is_refused_rather_than_padded() {
        assert!(PageCacheRow::decode(&[0u8; 8], &[0u8; ROW_VALUE_BYTES]).is_none());
        assert!(PageCacheRow::decode(&[0u8; ROW_KEY_BYTES], &[0u8; 8]).is_none());
    }

    #[test]
    fn a_row_survives_the_round_trip_field_by_field() {
        let mut key = [0u8; ROW_KEY_BYTES];
        let mut value = [0u8; ROW_VALUE_BYTES];
        let put = |bytes: &mut [u8], at: i16, data: &[u8]| {
            bytes[at as usize..at as usize + data.len()].copy_from_slice(data)
        };
        put(&mut key, KEY_TID, &7u32.to_ne_bytes());
        put(&mut key, KEY_TGID, &6u32.to_ne_bytes());
        put(&mut key, KEY_INO, &123_456u64.to_ne_bytes());
        put(&mut key, KEY_DEV, &41u32.to_ne_bytes());
        for (at, number) in [
            (VALUE_FIRST_NS, 11u64),
            (VALUE_ASKED, 12),
            (VALUE_LOOKUPS, 13),
            (VALUE_FILLED, 14),
            (VALUE_FOLIOS, 15),
            (VALUE_CREATED, 16),
        ] {
            put(&mut value, at, &number.to_ne_bytes());
        }
        put(&mut value, VALUE_COMM, b"cat");
        let row = PageCacheRow::decode(&key, &value).expect("a full record");
        assert_eq!((row.tid, row.tgid, row.ino, row.dev), (7, 6, 123_456, 41));
        assert_eq!(
            (
                row.first_ns,
                row.asked,
                row.lookups,
                row.filled,
                row.folios,
                row.created
            ),
            (11, 12, 13, 14, 15, 16)
        );
        assert_eq!(row.comm, "cat");
    }
}
