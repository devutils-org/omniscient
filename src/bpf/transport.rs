// SPDX-License-Identifier: MIT OR GPL-2.0
//! How a program's records reach userspace: the BPF ring buffer where the
//! kernel creates one, a perf event array where it does not.
//!
//! The ring buffer is Linux 5.8's (RHEL backports it to its 4.18), and every
//! program here was written for it: one ring for every cpu, records in the
//! order they were reserved. The kernels before it -- Debian 10's 4.19,
//! Ubuntu 20.04's 5.4 -- have the older transport only: a
//! `BPF_MAP_TYPE_PERF_EVENT_ARRAY` whose slot for each cpu holds a perf event
//! of the software type `PERF_COUNT_SW_BPF_OUTPUT`, written by
//! `bpf_perf_event_output` into that event's ring. One ring per cpu, so a
//! drain reads every cpu's and puts the records back in time order by the
//! stamp the caller names.
//!
//! **Losses are the program's to count, on both.** A full ring makes the
//! output helper return an error, and every program in this layer counts that
//! in its drop counter whichever transport it writes into. The perf ring also
//! writes a `PERF_RECORD_LOST` for what it threw away; those are the same
//! records counted a second time, so the drain skips them.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use perf_event_open_sys as sys;

use super::insn::{helper, Insn, Program, R10};
use super::map::map_failure;
use super::ring::{ring_bytes, RingBuffer};
use super::syscall::{self, BPF_MAP_TYPE_RINGBUF};

/// `BPF_MAP_TYPE_PERF_EVENT_ARRAY`.
const BPF_MAP_TYPE_PERF_EVENT_ARRAY: u32 = 4;
/// `bpf_perf_event_output(ctx, map, flags, data, size)`.
const PERF_EVENT_OUTPUT: i32 = 25;
/// `BPF_F_CURRENT_CPU`: the slot of the cpu the program runs on. The flags'
/// upper bits are a context length, so the constant is loaded zero-extended.
const BPF_F_CURRENT_CPU: i64 = 0xffff_ffff;
/// The smallest per-cpu ring worth having: a burst on one cpu of a few
/// hundred records.
const MIN_PERF_PAGES: usize = 8;
/// A cpu's ring is never larger than this, whatever the caller asked for the
/// whole machine.
const MAX_PERF_PAGES: usize = 256;

/// Where a program publishes, as the instructions that publish name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    Ring(RawFd),
    PerfArray(RawFd),
}

impl Output {
    /// Publishes the `size` bytes at `at` on the stack, the context in
    /// `ctx_reg`; `r0` is then zero or a negative errno, as both helpers
    /// return. Clobbers `r1` to `r5`.
    ///
    /// The context is what `bpf_perf_event_output` takes first and the ring
    /// buffer ignores: a program that may write into either keeps it in a
    /// callee-saved register.
    pub fn push(self, program: &mut Program, ctx_reg: u8, at: i16, size: i32) {
        match self {
            Output::Ring(fd) => {
                program.extend(Insn::load_map_fd(1, fd));
                program.push(Insn::mov64_reg(2, R10));
                program.push(Insn::add64_imm(2, at as i32));
                program.push(Insn::mov64_imm(3, size));
                program.push(Insn::mov64_imm(4, 0));
                program.push(Insn::call(helper::RINGBUF_OUTPUT));
            }
            Output::PerfArray(fd) => {
                program.push(Insn::mov64_reg(1, ctx_reg));
                program.extend(Insn::load_map_fd(2, fd));
                program.extend(Insn::ld_imm64(3, BPF_F_CURRENT_CPU));
                program.push(Insn::mov64_reg(4, R10));
                program.push(Insn::add64_imm(4, at as i32));
                program.push(Insn::mov64_imm(5, size));
                program.push(Insn::call(PERF_EVENT_OUTPUT));
            }
        }
    }

    /// Whether `insn` is the call of an output helper, of either transport:
    /// what the instruction tests look for.
    #[cfg(test)]
    pub fn is_output_call(insn: &Insn) -> bool {
        insn.code == 0x85 && (insn.imm == helper::RINGBUF_OUTPUT || insn.imm == PERF_EVENT_OUTPUT)
    }
}

/// The transport a set of programs writes into, and its reader.
pub struct Transport {
    kind: Kind,
}

enum Kind {
    Ring(RingBuffer),
    PerfArray(PerfArray),
}

impl Transport {
    /// A ring buffer of about `requested_bytes` where this kernel creates one;
    /// otherwise a perf event array with a ring per online cpu, which a drain
    /// merges by the eight-byte `CLOCK_MONOTONIC` stamp at `stamp_at` in each
    /// record (`None`: the records of one cpu stay together, in its order).
    pub fn create(
        requested_bytes: usize,
        stamp_at: Option<usize>,
        name: &str,
    ) -> Result<Self, String> {
        if *super::map_type_support(crate::need::BpfMap::Ringbuf) == super::Support::Yes {
            Self::ring(requested_bytes, name)
        } else {
            Self::perf_array(requested_bytes, stamp_at, name)
        }
    }

    /// The BPF ring buffer, which this kernel must create.
    pub fn ring(requested_bytes: usize, name: &str) -> Result<Self, String> {
        let bytes = ring_bytes(requested_bytes);
        let map = syscall::map_create(BPF_MAP_TYPE_RINGBUF, 0, 0, bytes as u32, name)
            .map_err(|error| map_failure(&format!("{name} ring buffer"), error))?;
        let ring = RingBuffer::map(map, bytes)
            .map_err(|error| format!("cannot map the {name} ring buffer: {error}"))?;
        Ok(Self {
            kind: Kind::Ring(ring),
        })
    }

    /// The perf event array, whichever transport this kernel has: what the
    /// tests choose to reach it on a kernel that has both.
    pub fn perf_array(
        requested_bytes: usize,
        stamp_at: Option<usize>,
        name: &str,
    ) -> Result<Self, String> {
        Ok(Self {
            kind: Kind::PerfArray(PerfArray::open(requested_bytes, stamp_at, name)?),
        })
    }

    /// What the programs' instructions publish into.
    pub fn output(&self) -> Output {
        match &self.kind {
            Kind::Ring(ring) => Output::Ring(ring.as_raw_fd()),
            Kind::PerfArray(array) => Output::PerfArray(array.map.as_raw_fd()),
        }
    }

    /// Hands every complete record to `handle`, and returns how many.
    ///
    /// A perf ring pads a record to eight bytes, so a record may be up to
    /// seven bytes longer than what the program wrote, zeroes at its end.
    pub fn drain(&mut self, handle: impl FnMut(&[u8])) -> usize {
        match &mut self.kind {
            Kind::Ring(ring) => ring.drain(handle),
            Kind::PerfArray(array) => array.drain(handle),
        }
    }
}

/// A perf event array and the rings of the events in its slots.
struct PerfArray {
    map: OwnedFd,
    rings: Vec<CpuRing>,
    stamp_at: Option<usize>,
}

/// One cpu's `PERF_COUNT_SW_BPF_OUTPUT` event and its mapped ring.
struct CpuRing {
    event: OwnedFd,
    base: *mut u8,
    map_len: usize,
    data_size: usize,
}

// The mappings belong to this struct alone and are only read through it.
unsafe impl Send for PerfArray {}

impl PerfArray {
    fn open(requested_bytes: usize, stamp_at: Option<usize>, name: &str) -> Result<Self, String> {
        let slots = syscall::possible_cpus() as u32;
        let map = syscall::map_create(BPF_MAP_TYPE_PERF_EVENT_ARRAY, 4, 4, slots, name)
            .map_err(|error| map_failure(&format!("{name} perf event array"), error))?;
        let cpus = crate::capabilities::online_cpus().map_err(|error| {
            format!(
                "{} cannot be read, so the cpus to open a ring on are unknown: {error}",
                crate::capabilities::ONLINE_CPUS
            )
        })?;
        let page = page_size();
        let per_cpu = requested_bytes / cpus.len().max(1);
        let mut pages = (per_cpu / page)
            .next_power_of_two()
            .clamp(MIN_PERF_PAGES, MAX_PERF_PAGES);
        let mut rings = Vec::with_capacity(cpus.len());
        for cpu in cpus {
            let ring = loop {
                match CpuRing::open(cpu, pages) {
                    Ok(ring) => break ring,
                    // `perf_event_mlock_kb` caps what a user maps beyond its
                    // memlock limit; a smaller ring loses more under a burst,
                    // and the programs count what it loses.
                    Err(error) if pages > 1 && error.raw_os_error() == Some(libc::EPERM) => {
                        pages /= 2;
                    }
                    Err(error) => {
                        return Err(match error.raw_os_error() {
                            Some(libc::EACCES | libc::EPERM) => super::permission_advice(
                                "opening the perf event a BPF program writes its records into",
                            ),
                            _ => format!(
                                "cannot open the {name} perf ring on cpu {cpu}: {error}; this \
                                 kernel has no BPF ring buffer, so a perf event array is the \
                                 only transport left"
                            ),
                        })
                    }
                }
            };
            syscall::map_update(
                map.as_raw_fd(),
                &cpu.to_ne_bytes(),
                &(ring.event.as_raw_fd() as u32).to_ne_bytes(),
            )
            .map_err(|error| {
                format!("cannot put cpu {cpu}'s ring in the {name} perf event array: {error}")
            })?;
            rings.push(ring);
        }
        for ring in &rings {
            // Enabled only once every slot holds its event: a program may
            // already run, and a slot without one is a record lost.
            if unsafe { sys::ioctls::ENABLE(ring.event.as_raw_fd(), 0) } < 0 {
                return Err(format!(
                    "the {name} perf ring will not start: {}",
                    io::Error::last_os_error()
                ));
            }
        }
        Ok(Self {
            map,
            rings,
            stamp_at,
        })
    }

    fn drain(&mut self, mut handle: impl FnMut(&[u8])) -> usize {
        let mut records: Vec<Vec<u8>> = Vec::new();
        for ring in &mut self.rings {
            ring.drain(&mut records);
        }
        if let Some(at) = self.stamp_at {
            // Stable: two records of one stamp keep their cpu's order.
            records.sort_by_key(|record| {
                record
                    .get(at..at + 8)
                    .map_or(0, |bytes| u64::from_ne_bytes(bytes.try_into().expect("8")))
            });
        }
        for record in &records {
            handle(record);
        }
        records.len()
    }
}

impl CpuRing {
    fn open(cpu: u32, pages: usize) -> io::Result<Self> {
        let mut attr = sys::bindings::perf_event_attr {
            type_: sys::bindings::PERF_TYPE_SOFTWARE,
            size: std::mem::size_of::<sys::bindings::perf_event_attr>() as u32,
            config: u64::from(sys::bindings::PERF_COUNT_SW_BPF_OUTPUT),
            sample_type: u64::from(sys::bindings::PERF_SAMPLE_RAW),
            ..Default::default()
        };
        attr.__bindgen_anon_1.sample_period = 1;
        attr.__bindgen_anon_2.wakeup_events = 1;
        attr.set_disabled(1);
        let flags = sys::bindings::PERF_FLAG_FD_CLOEXEC as libc::c_ulong;
        let fd = unsafe { sys::perf_event_open(&mut attr, -1, cpu as i32, -1, flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let event = unsafe { OwnedFd::from_raw_fd(fd) };
        let page = page_size();
        let map_len = (pages + 1) * page;
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                map_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                event.as_raw_fd(),
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            event,
            base: base as *mut u8,
            map_len,
            data_size: pages * page,
        })
    }

    /// Copies every record out, then publishes the tail once.
    fn drain(&mut self, records: &mut Vec<Vec<u8>>) {
        let header = self.base as *mut sys::bindings::perf_event_mmap_page;
        let head = unsafe { std::ptr::read_volatile(&(*header).data_head) };
        std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
        let tail = unsafe { std::ptr::read_volatile(&(*header).data_tail) };
        // SAFETY: the data area is the `data_size` bytes after the first page.
        let data = unsafe {
            std::slice::from_raw_parts(self.base.add(page_size()) as *const u8, self.data_size)
        };
        let tail = walk(data, tail, head, records, &mut 0);
        std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
        unsafe { std::ptr::write_volatile(&mut (*header).data_tail, tail) };
    }
}

impl Drop for CpuRing {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.base as *mut libc::c_void, self.map_len) };
    }
}

/// Walks a perf ring's records between `tail` and `head`, keeping each
/// sample's raw payload and adding each lost record's count, and returns the
/// new tail. Separated from the mapping so that a ring built by hand tests it.
fn walk(data: &[u8], mut tail: u64, head: u64, records: &mut Vec<Vec<u8>>, lost: &mut u64) -> u64 {
    while tail < head {
        let header = read_wrapped(data, tail, 8);
        let kind = u32::from_ne_bytes(header[0..4].try_into().expect("4"));
        let size = u16::from_ne_bytes(header[6..8].try_into().expect("2")) as u64;
        if size < 8 {
            // A record shorter than its header would loop forever; the rest
            // of the batch is unreadable, and skipping to the head loses it
            // where a spin would hang the run.
            return head;
        }
        let body = read_wrapped(data, tail, size as usize);
        match kind {
            sys::bindings::PERF_RECORD_SAMPLE => {
                let length = body
                    .get(8..12)
                    .map_or(0, |bytes| u32::from_ne_bytes(bytes.try_into().expect("4")))
                    as usize;
                if let Some(payload) = body.get(12..12 + length) {
                    records.push(payload.to_vec());
                }
            }
            sys::bindings::PERF_RECORD_LOST => {
                if let Some(bytes) = body.get(16..24) {
                    *lost += u64::from_ne_bytes(bytes.try_into().expect("8"));
                }
            }
            _ => {}
        }
        tail += size;
    }
    tail
}

/// `len` bytes at ring position `at`, across the end of the ring.
fn read_wrapped(data: &[u8], at: u64, len: usize) -> Vec<u8> {
    let size = data.len();
    let start = (at % size as u64) as usize;
    let mut out = Vec::with_capacity(len);
    let first = len.min(size - start);
    out.extend_from_slice(&data[start..start + first]);
    if first < len {
        out.extend_from_slice(&data[..len - first]);
    }
    out
}

fn page_size() -> usize {
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if size > 0 {
        size as usize
    } else {
        4096
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sample record as the kernel writes one for a program's output: the
    /// header, the raw size, the payload, padded to eight bytes.
    fn sample(payload: &[u8]) -> Vec<u8> {
        let size = (8 + 4 + payload.len()).div_ceil(8) * 8;
        let mut record = vec![0u8; size];
        record[0..4].copy_from_slice(&sys::bindings::PERF_RECORD_SAMPLE.to_ne_bytes());
        record[6..8].copy_from_slice(&(size as u16).to_ne_bytes());
        let raw = (size - 12) as u32;
        record[8..12].copy_from_slice(&raw.to_ne_bytes());
        record[12..12 + payload.len()].copy_from_slice(payload);
        record
    }

    fn lost(count: u64) -> Vec<u8> {
        let mut record = vec![0u8; 24];
        record[0..4].copy_from_slice(&sys::bindings::PERF_RECORD_LOST.to_ne_bytes());
        record[6..8].copy_from_slice(&24u16.to_ne_bytes());
        record[16..24].copy_from_slice(&count.to_ne_bytes());
        record
    }

    /// Lays `records` into a ring of `size` bytes from position `from`,
    /// wrapping, and returns the ring and the head.
    fn ring_of(size: usize, from: u64, records: &[Vec<u8>]) -> (Vec<u8>, u64) {
        let mut data = vec![0u8; size];
        let mut at = from;
        for record in records {
            for byte in record {
                data[(at % size as u64) as usize] = *byte;
                at += 1;
            }
        }
        (data, at)
    }

    #[test]
    fn samples_come_back_padded_and_lost_records_are_counted_apart() {
        let (data, head) = ring_of(256, 0, &[sample(b"first"), lost(3), sample(b"second!")]);
        let (mut records, mut dropped) = (Vec::new(), 0);
        let tail = walk(&data, 0, head, &mut records, &mut dropped);
        assert_eq!(tail, head);
        assert_eq!(dropped, 3);
        assert_eq!(records.len(), 2);
        // Padded to eight bytes with the raw size word: the payload's own
        // bytes come first and zeroes after them.
        assert!(records[0].starts_with(b"first"));
        assert!(records[0][5..].iter().all(|byte| *byte == 0));
        assert!(records[1].starts_with(b"second!"));
    }

    /// A record that runs past the end of the ring continues at its start.
    #[test]
    fn a_record_across_the_end_of_the_ring_is_read_whole() {
        let from = 256 - 12;
        let (data, head) = ring_of(256, from, &[sample(b"straddling the end")]);
        let (mut records, mut dropped) = (Vec::new(), 0);
        walk(&data, from, head, &mut records, &mut dropped);
        assert_eq!(records.len(), 1);
        assert!(records[0].starts_with(b"straddling the end"));
    }

    #[test]
    fn a_record_shorter_than_its_header_ends_the_walk_rather_than_spinning() {
        let mut broken = sample(b"x");
        broken[6..8].copy_from_slice(&0u16.to_ne_bytes());
        let (data, head) = ring_of(64, 0, &[broken]);
        let (mut records, mut dropped) = (Vec::new(), 0);
        assert_eq!(walk(&data, 0, head, &mut records, &mut dropped), head);
        assert!(records.is_empty());
    }

    /// The perf-array instructions put the context first and load the flag
    /// zero-extended: `-1` as an immediate is sign-extended, which sets the
    /// context-length bits and makes every output fail with `EINVAL`.
    #[test]
    fn the_perf_output_takes_the_context_and_the_current_cpu() {
        let mut program = Program::new();
        Output::PerfArray(7).push(&mut program, 6, -64, 48);
        program.push(Insn::mov64_imm(0, 0));
        program.push(Insn::exit());
        let instructions = program.build();
        assert_eq!(instructions[0], Insn::mov64_reg(1, 6));
        let flags = &instructions[3..5];
        assert_eq!(flags, &Insn::ld_imm64(3, 0xffff_ffff));
        assert_eq!(flags[1].imm, 0, "the flags' upper half is zero");
        assert!(instructions.iter().any(Output::is_output_call));
    }

    /// The perf array, end to end, on this kernel: a program on a syscall's
    /// tracepoint writes its stamp and the caller's tid, and the drain hands
    /// back this thread's record.
    #[test]
    fn a_program_publishes_through_a_perf_event_array() {
        if let super::super::Support::No(reason) | super::super::Support::Unchecked(reason) =
            super::super::load_support()
        {
            eprintln!("skipped: {reason}");
            return;
        }
        let mut transport = match Transport::perf_array(1 << 16, Some(0), "omn_test_pa") {
            Ok(transport) => transport,
            Err(reason) => {
                eprintln!("skipped: {reason}");
                return;
            }
        };
        let mut program = Program::new();
        program.push(Insn::mov64_reg(6, 1));
        program.push(Insn::call(helper::KTIME_GET_NS));
        program.push(Insn::stx(super::super::insn::BPF_DW, R10, -16, 0));
        program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
        program.push(Insn::stx(super::super::insn::BPF_DW, R10, -8, 0));
        transport.output().push(&mut program, 6, -16, 16);
        program.push(Insn::mov64_imm(0, 0));
        program.push(Insn::exit());
        let instructions = program.build();
        let attached = match super::super::attach::tracepoint(
            "syscalls",
            "sys_enter_getppid",
            &instructions,
            "omn_test_pa",
            super::super::attach::Scope::System,
        ) {
            Ok(attached) => attached,
            Err(reason) => {
                eprintln!("skipped: {reason}");
                return;
            }
        };
        let tid = unsafe { libc::gettid() } as u64;
        for _ in 0..5 {
            unsafe { libc::getppid() };
        }
        let mut mine = 0;
        let mut stamps = Vec::new();
        transport.drain(|record| {
            let word = |at: usize| u64::from_ne_bytes(record[at..at + 8].try_into().unwrap());
            stamps.push(word(0));
            if word(8) & 0xffff_ffff == tid {
                mine += 1;
            }
        });
        drop(attached);
        assert_eq!(mine, 5, "every call of this thread is one record");
        assert!(
            stamps.windows(2).all(|pair| pair[0] <= pair[1]),
            "{stamps:?}"
        );
    }
}
