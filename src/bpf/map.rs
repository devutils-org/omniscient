// SPDX-License-Identifier: MIT OR GPL-2.0
//! Maps a program and this process share, and how a map is drained.
//!
//! The ring buffer in `ring.rs` carries one record per event, which is the
//! right shape when the events are rare and each one matters. It is the wrong
//! shape for a tracepoint that fires two million times in a run: the cost of
//! the measurement then scales with the workload, and a switch-heavy program
//! measures a load test rather than itself.
//!
//! The answer is to count in the kernel. A hash map keyed by whatever the
//! series is keyed by -- a task, a stack, a device -- turns those two million
//! events into a few hundred entries, and userspace reads the entries instead
//! of the events. That is what makes `sched.offcpu` affordable, and it is the
//! mechanism `--budget` will degrade a trace onto.
//!
//! Two things about a map are not obvious and both change what the numbers
//! mean:
//!
//! **A drain is not a snapshot.** There is no cursor and no atomic iteration.
//! The walk asks the kernel for the key after this one until there are no
//! more, and the program keeps writing throughout. So a drain takes what it
//! finds and leaves the rest for the next pass, and nothing here pretends to
//! have seen the map at an instant.
//!
//! **Reading and removing must be one operation.** A lookup followed by a
//! delete loses whatever the program added in between, silently and in
//! proportion to how busy the workload is. `BPF_MAP_LOOKUP_AND_DELETE_ELEM`
//! is one operation and is what `drain` uses.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use super::insn::{helper, Insn, Program, BPF_DW, R10};

use super::syscall::{
    self, TypedMap, BPF_F_NO_PREALLOC, BPF_MAP_TYPE_HASH, BPF_MAP_TYPE_PERCPU_ARRAY,
    BPF_MAP_TYPE_STACK_TRACE, BPF_MAP_TYPE_TASK_STORAGE,
};

/// A map this layer could not create, said in a way a reader can act on.
///
/// Every program here attaches `permission_advice` to its `prog_load`, and a
/// program load is not the first privileged thing a collector does -- **map
/// creation is**, and it fails first. So what somebody without `CAP_BPF`
/// actually saw, from every collector written before the seventh wave, was
///
/// ```text
/// cannot create the `sched_process_exec` ring buffer: Operation not permitted (os error 1)
/// ```
///
/// which names neither the capability nor the way to get it. Nobody noticed for
/// four waves because the message only appears on a machine without the
/// capabilities, which is not the machine any of this was written on.
///
/// `what` is the map, without its article: "off-cpu in-flight map".
pub fn map_failure(what: &str, error: io::Error) -> String {
    if error.kind() == io::ErrorKind::PermissionDenied {
        // EPERM is also the answer of a kernel that charges maps to
        // RLIMIT_MEMLOCK, which no capability this tool asks for lifts.
        if let Some(reason) = super::maps_charged_to_memlock() {
            return format!("cannot create the {what}: {reason}");
        }
        super::permission_advice(&format!("creating the {what}"))
    } else {
        format!("cannot create the {what}: {error}")
    }
}

/// A `BPF_MAP_TYPE_HASH` shared with a program, addressed as bytes.
///
/// Key and value are byte strings here rather than types, because the layer
/// does not know what any particular program counts. The program that creates
/// one owns the `#[repr(C)]` structs on both sides and the test that says they
/// agree.
pub struct Hash {
    fd: OwnedFd,
    key_bytes: usize,
    value_bytes: usize,
    max_entries: u32,
}

/// What one drain found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Drained {
    pub entries: Vec<(Vec<u8>, Vec<u8>)>,
    /// Keys the walk returned and the read no longer found.
    ///
    /// Nothing but a drain removes from these maps, so this should be zero. It
    /// is counted rather than ignored because if it is ever not zero, an entry
    /// was dropped between being seen and being read, and a total short by one
    /// entry looks exactly like a total.
    pub vanished: u64,
    /// Set when the walk stopped at the map's own capacity rather than at its
    /// end, which means the kernel kept handing back keys.
    pub truncated: bool,
}

impl Hash {
    pub fn create(
        key_bytes: usize,
        value_bytes: usize,
        max_entries: u32,
        name: &str,
    ) -> io::Result<Self> {
        let fd = syscall::map_create(
            BPF_MAP_TYPE_HASH,
            key_bytes as u32,
            value_bytes as u32,
            max_entries,
            name,
        )?;
        Ok(Self {
            fd,
            key_bytes,
            value_bytes,
            max_entries,
        })
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Every key the map holds right now, in whatever order the kernel walks
    /// its buckets.
    pub fn keys(&self) -> io::Result<(Vec<Vec<u8>>, bool)> {
        let fd = self.fd.as_raw_fd();
        let key_bytes = self.key_bytes;
        walk(self.max_entries as usize, |from| {
            syscall::map_next_key(fd, from, key_bytes)
        })
    }

    /// Reads every entry and removes it, so the next drain sees only what
    /// happened since this one.
    ///
    /// Removing is what keeps the map bounded on a long run and what makes
    /// each drain a delta rather than a running total; the caller adds them up
    /// if it wants the total, and can say which window each one covers.
    pub fn drain(&self) -> io::Result<Drained> {
        let (keys, truncated) = self.keys()?;
        let mut drained = Drained {
            truncated,
            ..Default::default()
        };
        for key in keys {
            match syscall::map_take(self.fd.as_raw_fd(), &key, self.value_bytes)? {
                Some(value) => drained.entries.push((key, value)),
                None => drained.vanished += 1,
            }
        }
        Ok(drained)
    }

    /// How many entries the map holds, without reading or removing any.
    ///
    /// For a map of in-flight state -- tasks that went off cpu and have not
    /// come back -- this is the count still open when the run ended, which is
    /// a fact about the measurement rather than a loss.
    pub fn len(&self) -> io::Result<usize> {
        Ok(self.keys()?.0.len())
    }
}

/// `BPF_MAP_TYPE_ARRAY_OF_MAPS`, and the two commands a swap needs, which
/// `syscall.rs` does not offer.
const BPF_MAP_TYPE_ARRAY_OF_MAPS: u32 = 12;
const BPF_MAP_CREATE: i32 = 0;
const BPF_MAP_UPDATE_ELEM: i32 = 2;

/// The prefix of `union bpf_attr` that `BPF_MAP_CREATE` reads, as far as
/// `inner_map_fd` and the name. The kernel requires every byte past what it
/// is given to be zero, and a shorter attribute is how that is guaranteed.
#[repr(C)]
#[derive(Default)]
struct OuterCreateAttr {
    map_type: u32,
    key_size: u32,
    value_size: u32,
    max_entries: u32,
    map_flags: u32,
    inner_map_fd: u32,
    numa_node: u32,
    map_name: [u8; 16],
}

#[repr(C)]
struct ElemAttr {
    map_fd: u32,
    pad: u32,
    key: u64,
    value: u64,
    flags: u64,
}

/// A row map a program writes into while userspace drains it, made safe to
/// drain.
///
/// **The race this closes.** A program that counts into a hash looks its row
/// up and adds to it a few instructions later. A drain that takes the row with
/// lookup-and-delete between the two has already copied it out, and the
/// addition lands in a freed element -- or, the map being preallocated, in
/// whatever key the element was handed to next. Nothing counts it. Measured by
/// the eighth wave on the fentry histograms: a hundred thousand reads, 55192
/// counted, 44807 gone.
///
/// **Two row maps, and the drain only ever empties the one nobody can be
/// writing.** `fentry::HistogramProgram` found that shape: a generation word
/// says which map is written, a drain flips it and empties the other. Its
/// grace period is an interval -- the map drained is the one retired a tick
/// ago -- which costs every row a tick of lateness and makes the last drain a
/// special case with a sleep in it.
///
/// Here the word is a one-slot `BPF_MAP_TYPE_ARRAY_OF_MAPS`, which the
/// programs look up once per run to find the row map they write, and a drain
/// **swaps the slot and then waits for a grace period** before it reads the
/// map it retired. Every program this layer loads runs inside an RCU read-side
/// section -- `__bpf_trace_run` and the trampolines take `rcu_read_lock()`
/// around the program -- so once `synchronize_rcu()` has returned, no program
/// that could have read the old slot is still running, and the retired map is
/// written by no one. It is emptied whole: nothing arrives a tick late, and
/// nothing is left behind at the end of the run.
///
/// **The kernel's own swap is not the grace period.** Its comment on
/// `maybe_wait_bpf_programs` promises one on a map-of-maps update, and on this
/// machine (7.2) the update returns in 20 to 30 us, where a real
/// `synchronize_rcu()` takes 5 to 18 ms. Whatever the update waits for, it is
/// not what this needs, so the wait is asked for explicitly:
/// `membarrier(MEMBARRIER_CMD_GLOBAL)` is `synchronize_rcu()` from userspace.
/// And the programs here *can* be preempted mid-run -- `sys_exit`, which no
/// interrupt enters, shows thousands of the kernel's recursion misses under
/// eight busy threads -- so "a program runs for microseconds" is not a bound.
///
/// The price is that wait, paid once per drain in the drain's thread and never
/// by the program: 5 to 18 ms of sleeping on this machine, against a 100 ms
/// tick.
pub struct Rows {
    outer: OwnedFd,
    maps: [Hash; 2],
    /// Which of the two the slot names now.
    writing: std::cell::Cell<usize>,
}

impl Rows {
    pub fn create(
        key_bytes: usize,
        value_bytes: usize,
        max_entries: u32,
        name: &str,
    ) -> io::Result<Self> {
        if !grace_periods_offered() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "this kernel offers no way to wait for a grace period from userspace \
                 (`membarrier(MEMBARRIER_CMD_GLOBAL)` is withdrawn under `nohz_full`), and \
                 without one a drain would race the programs writing the map it reads",
            ));
        }
        let maps = [
            Hash::create(key_bytes, value_bytes, max_entries, name)?,
            Hash::create(key_bytes, value_bytes, max_entries, name)?,
        ];
        let mut attr = OuterCreateAttr {
            map_type: BPF_MAP_TYPE_ARRAY_OF_MAPS,
            key_size: 4,
            value_size: 4,
            max_entries: 1,
            inner_map_fd: maps[0].as_raw_fd() as u32,
            map_name: kernel_name(&format!("{name}_gen")),
            ..Default::default()
        };
        let fd = unsafe {
            libc::syscall(
                libc::SYS_bpf,
                BPF_MAP_CREATE,
                &mut attr as *mut _ as *mut libc::c_void,
                std::mem::size_of::<OuterCreateAttr>() as u32,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let outer = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd as RawFd) };
        let rows = Self {
            outer,
            maps,
            writing: std::cell::Cell::new(0),
        };
        rows.point_at(0)?;
        Ok(rows)
    }

    /// The map the programs load, which is the slot and not either row map.
    pub fn as_raw_fd(&self) -> RawFd {
        self.outer.as_raw_fd()
    }

    /// Points the slot at one row map. Programs that begin after this read the
    /// new one; programs already running may still hold the old one, which is
    /// what [`grace_period`] is for.
    fn point_at(&self, which: usize) -> io::Result<()> {
        let key = 0u32;
        let value = self.maps[which].as_raw_fd() as u32;
        let mut attr = ElemAttr {
            map_fd: self.outer.as_raw_fd() as u32,
            pad: 0,
            key: &key as *const u32 as u64,
            value: &value as *const u32 as u64,
            flags: 0,
        };
        let result = unsafe {
            libc::syscall(
                libc::SYS_bpf,
                BPF_MAP_UPDATE_ELEM,
                &mut attr as *mut _ as *mut libc::c_void,
                std::mem::size_of::<ElemAttr>() as u32,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        self.writing.set(which);
        Ok(())
    }

    /// Every row counted since the last drain, removed.
    ///
    /// The slot is pointed at the empty map first, a grace period is waited
    /// for, and only then is the other map read -- so a row is either in this
    /// drain whole or in the next one whole.
    pub fn drain(&self) -> io::Result<Drained> {
        let retired = self.writing.get();
        self.point_at(1 - retired)?;
        grace_period()?;
        self.maps[retired].drain()
    }

    /// How many rows the map being written holds now, without taking any.
    #[cfg(test)]
    pub fn len(&self) -> io::Result<usize> {
        self.maps[self.writing.get()].len()
    }
}

/// `MEMBARRIER_CMD_QUERY` and `MEMBARRIER_CMD_GLOBAL`, from
/// `include/uapi/linux/membarrier.h`.
const MEMBARRIER_CMD_QUERY: libc::c_long = 0;
const MEMBARRIER_CMD_GLOBAL: libc::c_long = 1;

/// Whether this kernel will wait for a grace period on request. A query, so
/// that a machine that cannot is refused when the program loads rather than
/// failing on its first drain.
pub(crate) fn grace_periods_offered() -> bool {
    let offered = unsafe { libc::syscall(libc::SYS_membarrier, MEMBARRIER_CMD_QUERY, 0, 0) };
    offered >= 0 && offered & MEMBARRIER_CMD_GLOBAL != 0
}

/// Returns once every RCU read-side section that had begun has ended -- which
/// is every eBPF program that was running on any cpu when it was called.
///
/// Public for the two-map drains that keep their own word rather than a
/// [`Rows`] slot (`fentry::HistogramProgram`), and for the two-phase drain
/// stream K proposed, where a round retires every drained source first and
/// waits once.
pub fn grace_period() -> io::Result<()> {
    let result = unsafe { libc::syscall(libc::SYS_membarrier, MEMBARRIER_CMD_GLOBAL, 0, 0) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A name, truncated to the 15 characters plus NUL the kernel stores.
fn kernel_name(name: &str) -> [u8; 16] {
    let mut buffer = [0u8; 16];
    let bytes = name.as_bytes();
    let len = bytes.len().min(15);
    buffer[..len].copy_from_slice(&bytes[..len]);
    buffer
}

/// Looks the row map up through a [`Rows`] slot and keeps it on the stack at
/// `spill`, an eight-byte slot of the program's own; `missing` is where a run
/// goes if the lookup finds nothing.
///
/// **Once per run, and before the first row lookup.** Two lookups of the slot
/// in one run could straddle a swap and name two different maps -- a row found
/// in one and created in the other -- so the pointer is taken once and every
/// row access of the run reads it back with [`row_map`]. `missing` cannot be
/// reached while the slot holds a map, which it does from creation; the
/// verifier insists on the branch, and the caller counts it as a row it could
/// not record, so that if it ever is reached it is not silent.
///
/// Clobbers `r0` through `r5`.
pub fn select_rows(program: &mut Program, rows_fd: RawFd, spill: i16, missing: &'static str) {
    program.push(Insn::st_imm(BPF_DW, R10, spill, 0));
    program.extend(Insn::load_map_fd(1, rows_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, spill as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to(missing, Insn::jeq_imm(0, 0, 0));
    program.push(Insn::stx(BPF_DW, R10, spill, 0));
}

/// Loads the row map [`select_rows`] kept at `spill` into `dst`: what every
/// `load_map_fd(dst, rows)` of a drained program becomes.
pub fn row_map(dst: u8, spill: i16) -> Insn {
    Insn::ldx(BPF_DW, dst, R10, spill)
}

/// The check every program drained through [`Rows`] runs on itself: the slot
/// is read once, kept on the stack, and every row access reads it back from
/// there.
///
/// A second read of the slot is the defect this layer exists to prevent, and
/// it is invisible in every test that does not look at the instructions: two
/// reads can straddle a drain's swap, find a row in one map and create it in
/// the other, and the occurrence is counted twice or not at all depending on
/// which way the drain went.
#[cfg(test)]
pub fn assert_rows_taken_once(instructions: &[Insn], rows_fd: RawFd) {
    const LD_IMM64: u8 = 0x18;
    const STX_DW: u8 = 0x7b;
    const LDX_DW: u8 = 0x79;
    let loads: Vec<usize> = instructions
        .iter()
        .enumerate()
        .filter(|(_, insn)| insn.code == LD_IMM64 && insn.imm == rows_fd)
        .map(|(at, _)| at)
        .collect();
    assert_eq!(
        loads.len(),
        1,
        "the generation slot is read once per run, and this program reads it at {loads:?}"
    );
    let spill = instructions[loads[0]..]
        .iter()
        .find(|insn| insn.code == STX_DW && insn.regs & 0x0f == R10 && insn.regs >> 4 == 0)
        .expect("the map the slot named is kept on the stack")
        .off;
    let refills = instructions
        .iter()
        .filter(|insn| insn.code == LDX_DW && insn.regs == (R10 << 4) | 1 && insn.off == spill)
        .count();
    assert!(
        refills >= 2,
        "the row lookup and the row insert both use the map the slot named; {refills} read it"
    );
}

/// Walks a map's keys, stopping at the end or at `capacity`.
///
/// Separated from the syscall so the termination can be tested. A walk that
/// the kernel never ends -- keys reappearing because the program keeps
/// inserting into buckets the walk has already passed -- would otherwise be an
/// unbounded loop inside a drain that runs on a tick, which is a hang rather
/// than a wrong number. The capacity is the map's own, so reaching it means
/// something is wrong and the caller is told rather than left to notice.
fn walk(
    capacity: usize,
    mut next: impl FnMut(Option<&[u8]>) -> io::Result<Option<Vec<u8>>>,
) -> io::Result<(Vec<Vec<u8>>, bool)> {
    let mut keys: Vec<Vec<u8>> = Vec::new();
    let mut from: Option<Vec<u8>> = None;
    loop {
        let Some(key) = next(from.as_deref())? else {
            return Ok((keys, false));
        };
        if keys.len() >= capacity {
            return Ok((keys, true));
        }
        keys.push(key.clone());
        from = Some(key);
    }
}

/// A one-per-cpu counter array, which is how this layer counts what a program
/// could not record.
///
/// Per-cpu so that incrementing it costs no atomic on a path that runs tens of
/// thousands of times a second, and summed on the way out. Every program in
/// the layer carries one of these, because neither the ring buffer nor a full
/// hash map reports its own overflow: the helper returns an error, the event
/// is gone, and this is the only place that records it happened.
pub struct Counters {
    fd: OwnedFd,
    cpus: usize,
}

impl Counters {
    pub fn create(slots: u32, name: &str) -> io::Result<Self> {
        let fd = syscall::map_create(BPF_MAP_TYPE_PERCPU_ARRAY, 4, 8, slots, name)?;
        Ok(Self {
            fd,
            cpus: syscall::possible_cpus(),
        })
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// One slot, summed over the cpus that incremented it.
    ///
    /// A map that cannot be read reads as zero, which is the only answer that
    /// does not invent a loss; the counter is read while the program is still
    /// loaded, so the case does not arise in practice.
    pub fn read(&self, slot: u32) -> u64 {
        syscall::map_lookup_percpu_u64(self.fd.as_raw_fd(), slot, self.cpus)
            .map(|values| values.iter().sum())
            .unwrap_or(0)
    }

    /// One slot, each cpu's own count, indexed by cpu: for a program whose
    /// per-cpu slots are the measurement rather than a loss to add up.
    pub fn read_per_cpu(&self, slot: u32) -> Vec<u64> {
        syscall::map_lookup_percpu_u64(self.fd.as_raw_fd(), slot, self.cpus)
            .unwrap_or_else(|_| vec![0; self.cpus])
    }
}

/// The stacks a program interned with `bpf_get_stackid`.
///
/// A `BPF_MAP_TYPE_STACK_TRACE` is the kernel's own side table: the program
/// asks for an id, the kernel walks the stack, hashes it and hands back a
/// number, and the same stack always gets the same number. That is what makes
/// a stack affordable on a path that runs tens of thousands of times a second
/// -- the walk happens once per distinct stack, and what travels with each
/// measurement is four bytes.
///
/// Entries are read and left in place. Many measurements resolve to the same
/// stack, and taking one out on the first sighting would leave every later one
/// unresolvable.
pub struct StackTraces {
    fd: OwnedFd,
    depth: u32,
}

impl StackTraces {
    /// `depth` is how many frames the kernel will walk before it gives up. The
    /// value size is fixed at creation, so a stack deeper than this comes back
    /// truncated rather than failing, and the truncation is invisible in the
    /// map: what says so is the last slot being full.
    pub fn create(slots: u32, depth: u32, name: &str) -> io::Result<Self> {
        let fd = syscall::map_create(
            BPF_MAP_TYPE_STACK_TRACE,
            4,
            depth * std::mem::size_of::<u64>() as u32,
            slots,
            name,
        )?;
        Ok(Self { fd, depth })
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    pub fn depth(&self) -> u32 {
        self.depth
    }

    /// The addresses behind one id, leaf first, with the zero padding removed.
    ///
    /// `None` when the id is not in the map: the kernel evicts under pressure,
    /// and an id whose stack is gone is a measurement that keeps its number and
    /// loses its explanation, which is worth reporting as such rather than as
    /// an empty stack.
    pub fn get(&self, id: u32) -> io::Result<Option<Vec<u64>>> {
        let bytes = self.depth as usize * std::mem::size_of::<u64>();
        let Some(value) = syscall::map_lookup(self.fd.as_raw_fd(), &id.to_ne_bytes(), bytes)?
        else {
            return Ok(None);
        };
        let addresses: Vec<u64> = value
            .chunks_exact(std::mem::size_of::<u64>())
            .map(|chunk| u64::from_ne_bytes(chunk.try_into().expect("eight bytes")))
            // The kernel zero-fills the slots it did not walk into. A zero is
            // not an address, so the first one ends the stack.
            .take_while(|address| *address != 0)
            .collect();
        Ok(Some(addresses))
    }
}

/// A `BPF_MAP_TYPE_TASK_STORAGE`: one value per task, kept in the task itself.
///
/// This is the one map in the layer that is not keyed by anything a program
/// computes. A program asks for *the current task's* slot, and the kernel hands
/// back the value that lives in that `task_struct` -- so whatever one program
/// wrote there while a thread was doing something is readable by any other
/// program that later runs in the same thread, on any hook, with no key to
/// agree on. That is what carries a request's identity from where it enters to
/// where it touches the disk: a read is submitted in the reader's context, so
/// the block layer's issue finds the id the request's birth wrote.
///
/// Two things the kernel insists on and nothing else in this layer needs:
///
/// - **A BTF description of the key and the value.** The value lives inside
///   the task, and `bpf_local_storage_map_alloc_check` refuses a map it is not
///   told the type of. The key must be a 32-bit int -- from userspace it is a
///   pidfd -- and the value is described as an array of `int` of the right
///   size, which is all the check reads. [`btf_blob`] builds exactly that.
/// - **No preallocation, and no capacity.** An entry exists when a program
///   creates one for a task and goes when the task does, so `max_entries` is
///   zero and `BPF_F_NO_PREALLOC` is required.
///
/// An entry dies with its task. A thread that exits mid-request leaves no
/// stale identity for a recycled tid to inherit, which a hash keyed by tid
/// would.
pub struct TaskStorage {
    fd: OwnedFd,
    /// The BTF object the map was described with. The map holds its own
    /// reference; this one is kept so the description outlives nothing it
    /// describes.
    _btf: OwnedFd,
}

/// The BTF type ids [`btf_blob`] assigns.
const BTF_INT_ID: u32 = 1;
const BTF_VALUE_ID: u32 = 2;

/// The smallest BTF that describes a local-storage map: `int` for the key, and
/// an array of `int` as long as the value for the value.
///
/// Laid out as `include/uapi/linux/btf.h` gives it: a 24-byte header, the type
/// section, then the string section, whose first byte must be the empty
/// string. The value's element type does not matter to the kernel -- it checks
/// the size and looks for special fields, which an array of `int` has none of
/// -- so no struct is described and no member name is owed.
fn btf_blob(value_bytes: usize) -> Vec<u8> {
    const MAGIC: u16 = 0xeb9f;
    const KIND_INT: u32 = 1;
    const KIND_ARRAY: u32 = 3;
    const INT_SIGNED: u32 = 1;

    let strings: &[u8] = b"\0int\0";
    let int_name = 1u32;

    let mut types: Vec<u8> = Vec::new();
    let word = |types: &mut Vec<u8>, value: u32| types.extend_from_slice(&value.to_le_bytes());
    // [1] int: name, info (kind in bits 24..28), size, then the encoding word:
    // signed, at bit offset zero, thirty-two bits wide.
    word(&mut types, int_name);
    word(&mut types, KIND_INT << 24);
    word(&mut types, 4);
    word(&mut types, (INT_SIGNED << 24) | 32);
    // [2] int[value_bytes / 4]: no name, the array kind, a zero size field,
    // then element type, index type, element count.
    word(&mut types, 0);
    word(&mut types, KIND_ARRAY << 24);
    word(&mut types, 0);
    word(&mut types, BTF_INT_ID);
    word(&mut types, BTF_INT_ID);
    word(&mut types, (value_bytes / 4) as u32);

    let mut blob = Vec::with_capacity(24 + types.len() + strings.len());
    blob.extend_from_slice(&MAGIC.to_le_bytes());
    blob.push(1); // version
    blob.push(0); // flags
    blob.extend_from_slice(&24u32.to_le_bytes()); // hdr_len
    blob.extend_from_slice(&0u32.to_le_bytes()); // type_off
    blob.extend_from_slice(&(types.len() as u32).to_le_bytes()); // type_len
    blob.extend_from_slice(&(types.len() as u32).to_le_bytes()); // str_off
    blob.extend_from_slice(&(strings.len() as u32).to_le_bytes()); // str_len
    blob.extend_from_slice(&types);
    blob.extend_from_slice(strings);
    blob
}

impl TaskStorage {
    /// `value_bytes` must be a multiple of four, which every value this layer
    /// lays out is: the kernel is told the value is an array of `int`.
    pub fn create(value_bytes: usize, name: &str) -> io::Result<Self> {
        assert!(
            value_bytes > 0 && value_bytes % 4 == 0,
            "a task-storage value is described as an array of int"
        );
        let btf = syscall::btf_load(&btf_blob(value_bytes)).map_err(|refused| {
            if refused.verifier_log.is_empty() {
                refused.error
            } else {
                io::Error::new(
                    refused.error.kind(),
                    format!(
                        "{}; the kernel said: {}",
                        refused.error, refused.verifier_log
                    ),
                )
            }
        })?;
        let fd = syscall::map_create_typed(&TypedMap {
            map_type: BPF_MAP_TYPE_TASK_STORAGE,
            key_size: 4,
            value_size: value_bytes as u32,
            max_entries: 0,
            map_flags: BPF_F_NO_PREALLOC,
            btf_fd: btf.as_raw_fd(),
            btf_key_type_id: BTF_INT_ID,
            btf_value_type_id: BTF_VALUE_ID,
            name,
        })?;
        Ok(Self { fd, _btf: btf })
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// One task's value, read from userspace: the key is a pidfd for the
    /// task, which is how the kernel lets a process name a task it may not
    /// share an address space with. `None` when the task holds no entry.
    ///
    /// Nothing in the build reads a task's value from userspace -- the
    /// programs publish what they account -- so this is here for the test
    /// that the map answers the way the kernel documents.
    #[cfg(test)]
    pub fn lookup(&self, pidfd: RawFd, value_bytes: usize) -> io::Result<Option<Vec<u8>>> {
        syscall::map_lookup(self.fd.as_raw_fd(), &pidfd.to_ne_bytes(), value_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A walk over a map the kernel serves normally: every key once, in the
    /// order it hands them back, and an end that is an end.
    #[test]
    fn a_walk_yields_every_key_once_and_stops_at_the_end() {
        let map = [b"a".to_vec(), b"b".to_vec(), b"c".to_vec()];
        let (keys, truncated) = walk(16, |from| {
            Ok(match from {
                None => Some(map[0].clone()),
                Some(key) => map
                    .iter()
                    .position(|candidate| candidate == key)
                    .and_then(|at| map.get(at + 1))
                    .cloned(),
            })
        })
        .expect("a walk over a map that answers");
        assert_eq!(keys, map);
        assert!(!truncated);
    }

    #[test]
    fn an_empty_map_walks_to_no_keys() {
        let (keys, truncated) = walk(16, |_| Ok(None)).expect("an empty walk");
        assert!(keys.is_empty() && !truncated);
    }

    /// A program inserting into buckets the walk has already passed can make
    /// the kernel hand back keys forever. Inside a drain on a tick loop that
    /// is a hang, which is worse than a wrong number because nothing is ever
    /// reported at all -- so the walk stops at the map's own capacity and says
    /// it did.
    #[test]
    fn a_walk_that_never_ends_stops_at_the_maps_capacity() {
        let mut issued = 0u32;
        let (keys, truncated) = walk(8, |_| {
            issued += 1;
            Ok(Some(issued.to_ne_bytes().to_vec()))
        })
        .expect("a walk over a map that keeps answering");
        assert_eq!(keys.len(), 8, "no more than the map can hold");
        assert!(truncated, "and the caller is told the walk was cut short");
    }

    /// An error from the kernel is not an empty map. Returning the keys found
    /// so far would report a partial drain as a complete one.
    #[test]
    fn a_walk_that_fails_is_an_error_not_a_short_answer() {
        let mut calls = 0;
        let result = walk(16, |_| {
            calls += 1;
            if calls == 1 {
                Ok(Some(b"first".to_vec()))
            } else {
                Err(io::Error::from_raw_os_error(libc::EPERM))
            }
        });
        assert!(result.is_err());
    }

    /// Without the capabilities there is no map to create, and that is the
    /// answer rather than a failure -- but the refusal must be the kernel's.
    #[test]
    fn a_hash_map_is_created_or_the_kernel_says_why() {
        match Hash::create(4, 8, 16, "omn_test_hash") {
            Ok(map) => {
                let (keys, truncated) = map.keys().expect("an empty map walks");
                assert!(keys.is_empty() && !truncated, "a new map holds nothing");
                assert_eq!(map.len().expect("a length"), 0);

                syscall::map_update(map.as_raw_fd(), &7u32.to_ne_bytes(), &42u64.to_ne_bytes())
                    .expect("a write into a map this process owns");
                assert_eq!(map.len().expect("a length"), 1);

                // The drain must both read and remove: a second one finding
                // the same entry would double every total built from it.
                let drained = map.drain().expect("a drain");
                assert_eq!(drained.entries.len(), 1);
                assert_eq!(drained.entries[0].0, 7u32.to_ne_bytes());
                assert_eq!(
                    u64::from_ne_bytes(drained.entries[0].1[..8].try_into().unwrap()),
                    42
                );
                assert_eq!((drained.vanished, drained.truncated), (0, false));
                assert!(
                    map.drain().expect("a second drain").entries.is_empty(),
                    "a drained entry must not come back"
                );
            }
            Err(error) => {
                assert!(
                    error.raw_os_error().is_some(),
                    "a refusal must carry the kernel's errno, not a message invented here"
                );
                eprintln!("skipped: this machine does not let this process create a map: {error}");
            }
        }
    }

    /// The check refuses what it exists to refuse: a program that reads the
    /// generation slot twice in one run.
    #[test]
    #[should_panic(expected = "read once per run")]
    fn a_program_that_reads_the_slot_twice_is_refused() {
        let mut program = Program::new();
        select_rows(&mut program, 7, -8, "missing");
        select_rows(&mut program, 7, -16, "missing");
        program.label("missing");
        program.push(Insn::exit());
        assert_rows_taken_once(&program.build(), 7);
    }

    /// Rows written into the map being written come back from one drain, and
    /// rows written after it come back from the next -- neither lost at the
    /// swap nor returned twice.
    #[test]
    fn a_drain_takes_the_map_written_until_now_and_leaves_the_next_one_empty() {
        let rows = match Rows::create(4, 8, 16, "omn_test_rows") {
            Ok(rows) => rows,
            Err(error) => {
                assert!(
                    error.raw_os_error().is_some() || error.kind() == io::ErrorKind::Unsupported
                );
                eprintln!(
                    "skipped: this machine does not let this process create the row maps: {error}"
                );
                return;
            }
        };
        let write = |key: u32, value: u64| {
            let map = &rows.maps[rows.writing.get()];
            syscall::map_update(map.as_raw_fd(), &key.to_ne_bytes(), &value.to_ne_bytes())
                .expect("a write into a map this process owns");
        };
        write(1, 10);
        write(2, 20);
        assert_eq!(rows.len().expect("a length"), 2);
        let first = rows.drain().expect("a drain");
        assert_eq!(first.entries.len(), 2, "both rows of the first interval");
        assert_eq!(
            rows.len().expect("a length"),
            0,
            "the map now written is empty"
        );
        write(3, 30);
        let second = rows.drain().expect("a second drain");
        assert_eq!(second.entries.len(), 1, "only what was written since");
        assert_eq!(second.entries[0].0, 3u32.to_ne_bytes());
        assert!(rows.drain().expect("a third drain").entries.is_empty());
    }

    /// The BTF handed to the kernel is checked byte by byte on the other side,
    /// and a field one word out of place is a refused map with a log line as
    /// its only diagnosis. The layout is `include/uapi/linux/btf.h`'s.
    #[test]
    fn the_task_storage_btf_is_laid_out_as_the_kernel_reads_it() {
        let blob = btf_blob(64);
        let word = |at: usize| u32::from_le_bytes(blob[at..at + 4].try_into().unwrap());
        assert_eq!(u16::from_le_bytes([blob[0], blob[1]]), 0xeb9f);
        assert_eq!((blob[2], blob[3]), (1, 0), "version one, no flags");
        assert_eq!(word(4), 24, "the header is 24 bytes");
        let (type_off, type_len, str_off, str_len) = (word(8), word(12), word(16), word(20));
        assert_eq!(type_off, 0);
        assert_eq!(type_len, 40, "an int of four words and an array of six");
        assert_eq!(str_off, type_len, "the strings follow the types");
        let strings = &blob[24 + str_off as usize..][..str_len as usize];
        assert_eq!(
            strings[0], 0,
            "the string section starts with the empty string"
        );
        let types = 24 + type_off as usize;
        // The key: `int`, four bytes, signed, thirty-two bits.
        assert_eq!(&strings[word(types) as usize..][..4], b"int\0");
        assert_eq!(word(types + 4) >> 24, 1, "an INT");
        assert_eq!(word(types + 8), 4);
        assert_eq!(word(types + 12) & 0xff, 32);
        // The value: an array of that int, as long as the value.
        assert_eq!(word(types + 20) >> 24, 3, "an ARRAY");
        assert_eq!(word(types + 28), BTF_INT_ID, "of int");
        assert_eq!(word(types + 36), 16, "sixty-four bytes of int");
        assert_eq!(blob.len(), 24 + 40 + str_len as usize);
    }

    /// Created, or refused with the kernel's own errno. Not created by an
    /// in-process test on a normal machine: the test binary carries no
    /// capabilities, and `tests/vertical.rs` is where the map is exercised.
    #[test]
    fn a_task_storage_map_is_created_or_the_kernel_says_why() {
        match TaskStorage::create(64, "omn_test_task") {
            Ok(storage) => {
                let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0) };
                assert!(pidfd >= 0, "pidfd_open: {}", io::Error::last_os_error());
                assert_eq!(
                    storage
                        .lookup(pidfd as RawFd, 64)
                        .expect("a lookup by pidfd"),
                    None,
                    "no program has written into this task"
                );
                unsafe { libc::close(pidfd as RawFd) };
            }
            Err(error) => {
                assert!(error.raw_os_error().is_some(), "{error}");
                eprintln!("skipped: this process may not create a task-storage map: {error}");
            }
        }
    }

    /// The advice has to arrive on the first privileged call a collector makes,
    /// which is this one and not the program load. A reader whose kernel
    /// refuses gets the capability and the command, not `os error 1`.
    #[test]
    fn a_map_refused_on_privilege_grounds_names_the_capability() {
        let refused = io::Error::from(io::ErrorKind::PermissionDenied);
        let message = map_failure("off-cpu in-flight map", refused);
        assert!(message.contains("CAP_BPF"), "{message}");
        assert!(message.contains("setcap"), "{message}");
        assert!(message.contains("off-cpu in-flight map"), "{message}");
    }

    /// And a failure that is not about privilege says what it was, rather than
    /// sending a reader to grant a capability they already have. A full map
    /// table and a refused one are different problems.
    #[test]
    fn a_map_refused_for_any_other_reason_is_reported_as_itself() {
        let full = io::Error::from(io::ErrorKind::OutOfMemory);
        let message = map_failure("syscall histogram map", full);
        assert!(!message.contains("CAP_BPF"), "{message}");
        assert!(
            message.starts_with("cannot create the syscall histogram map"),
            "{message}"
        );
    }
}
