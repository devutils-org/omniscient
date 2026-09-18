//! `perf_event_open` and its ring buffer.
//!
//! This is the syscall, not the `perf` tool: a request is an attribute struct
//! and a file descriptor, and the events arrive in a shared memory ring the
//! kernel writes and this process reads. Shelling out to `perf record` would
//! make the tool a scraper of someone else's output format and would put a
//! second clock domain in the middle of the pipeline.
//!
//! Two things here are not incidental. The clock is pinned explicitly with
//! `use_clockid` and `CLOCK_MONOTONIC`, so a sample and an event read from
//! procfs sort into the same stream. And the ring reports its own overflows
//! through `PERF_RECORD_LOST`, which is counted and surfaced: a sampler that
//! silently drops a third of its samples still produces a plausible-looking
//! profile.

use std::io;
use std::os::fd::RawFd;

use perf_event_open_sys as sys;

/// What a sampling event is attached to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// One process, on every cpu.
    Process(i32),
    /// Every process, on one cpu. Needs privileges on most machines.
    Cpu(u32),
}

/// One record read out of the ring.
#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    Sample(Sample),
    /// The kernel had nowhere to put `count` events. They are gone; the only
    /// honest thing left to do is say how many.
    Lost {
        count: u64,
    },
    /// The kernel throttled the event because it was costing too much.
    Throttle {
        throttled: bool,
    },
    /// A record this reader does not decode. Counted, never silently dropped.
    Other {
        kind: u32,
    },
}

/// A sample, with the fields this tool asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    /// Instruction pointer at the time of the event.
    pub ip: u64,
    pub pid: u32,
    pub tid: u32,
    /// `CLOCK_MONOTONIC`, because the attribute asked for it.
    pub time_ns: u64,
    /// The address the event is about. For a page fault, the address that
    /// faulted, which is what makes an interval join possible.
    pub addr: u64,
}

/// The sample fields this reader requests, in the order the kernel writes them.
const SAMPLE_TYPE: u64 = (sys::bindings::PERF_SAMPLE_IP
    | sys::bindings::PERF_SAMPLE_TID
    | sys::bindings::PERF_SAMPLE_TIME
    | sys::bindings::PERF_SAMPLE_ADDR) as u64;

/// The smallest ring worth trying. Below this, a burst of faults would be
/// mostly loss, and a mostly-lost measurement is worse than a refusal.
const MIN_RING_PAGES: usize = 8;

/// Whether an error means "that ring is bigger than you are allowed to map".
fn is_ring_too_large(error: &io::Error) -> bool {
    let text = error.to_string();
    text.contains("mmap")
        && (text.contains("Operation not permitted") || text.contains("Cannot allocate memory"))
}

/// A software event sampled into a ring buffer.
pub struct SampledEvent {
    fd: RawFd,
    base: *mut u8,
    map_len: usize,
    data_size: u64,
    /// Ring overflows, accumulated across drains.
    lost: u64,
    undecoded: u64,
}

// The mapping belongs to this struct alone and is only read through `&mut self`.
unsafe impl Send for SampledEvent {}

impl SampledEvent {
    /// Opens a software event, shrinking the ring until the machine allows it.
    ///
    /// `perf_event_mlock_kb` caps what an unprivileged process may map, and the
    /// cap is small by default. Halving down to a floor gets a working ring on
    /// a stock machine; the size that was accepted is reported, because a
    /// smaller ring means a higher chance of dropping samples under a burst.
    pub fn open_software(
        config: u64,
        period: u64,
        scope: Scope,
        ring_pages: usize,
    ) -> io::Result<Self> {
        let mut pages = ring_pages.next_power_of_two();
        loop {
            match Self::open_with_ring(config, period, scope, pages) {
                Ok(event) => return Ok(event),
                Err(error) if pages > MIN_RING_PAGES && is_ring_too_large(&error) => {
                    pages /= 2;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// The data pages this event's ring actually got.
    pub fn ring_pages(&self) -> usize {
        (self.data_size as usize) / page_size()
    }

    /// What this event does and does not see, in one line, for the record that
    /// travels with the numbers.
    pub fn sampled_threads(&self) -> &'static str {
        "the target task only: threads it starts afterwards are not sampled"
    }

    fn open_with_ring(
        config: u64,
        period: u64,
        scope: Scope,
        ring_pages: usize,
    ) -> io::Result<Self> {
        assert!(
            ring_pages.is_power_of_two(),
            "the ring must be a power of two pages"
        );

        let mut attr: sys::bindings::perf_event_attr = unsafe { std::mem::zeroed() };
        attr.type_ = sys::bindings::PERF_TYPE_SOFTWARE;
        attr.size = std::mem::size_of::<sys::bindings::perf_event_attr>() as u32;
        attr.config = config;
        attr.__bindgen_anon_1.sample_period = period;
        attr.sample_type = SAMPLE_TYPE;
        attr.__bindgen_anon_2.wakeup_events = 1;
        // Start stopped: the caller decides when the window opens, which is how
        // a launched program can be sampled from its first instruction.
        attr.set_disabled(1);
        // One clock domain for the whole tool. Without this the samples would
        // be on the kernel's default clock and every join against them would be
        // off by an amount nobody would notice.
        attr.set_use_clockid(1);
        attr.clockid = libc::CLOCK_MONOTONIC;
        // `inherit` is deliberately left off. The kernel refuses to map a ring
        // buffer for an inherited per-task event (`perf_mmap` returns EINVAL
        // when `cpu == -1 && attr.inherit`), because each inheriting task would
        // need a buffer of its own. Following every thread means per-cpu events,
        // which need privileges most machines do not grant. So this samples the
        // task it was given, and `sampled_threads` below states the limit rather
        // than leaving it to be discovered in a missing number.

        let (pid, cpu) = match scope {
            Scope::Process(pid) => (pid, -1),
            Scope::Cpu(cpu) => (-1, cpu as i32),
        };
        let fd = unsafe {
            sys::perf_event_open(
                &mut attr,
                pid,
                cpu,
                -1,
                sys::bindings::PERF_FLAG_FD_CLOEXEC as u64,
            )
        };
        if fd < 0 {
            return Err(io::Error::other(format!(
                "perf_event_open: {}",
                io::Error::last_os_error()
            )));
        }

        let page = page_size();
        let map_len = (ring_pages + 1) * page;
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                map_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            let error = io::Error::other(format!(
                "mmap of {ring_pages} ring pages: {}",
                io::Error::last_os_error()
            ));
            unsafe { libc::close(fd) };
            return Err(error);
        }

        Ok(Self {
            fd,
            base: base as *mut u8,
            map_len,
            data_size: (ring_pages * page) as u64,
            lost: 0,
            undecoded: 0,
        })
    }

    pub fn enable(&self) -> io::Result<()> {
        self.ioctl(unsafe { sys::ioctls::ENABLE(self.fd, 0) })
    }

    pub fn disable(&self) -> io::Result<()> {
        self.ioctl(unsafe { sys::ioctls::DISABLE(self.fd, 0) })
    }

    fn ioctl(&self, result: i32) -> io::Result<()> {
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Events the ring had to throw away since this event was opened.
    pub fn lost(&self) -> u64 {
        self.lost
    }

    /// Records this reader did not know how to decode.
    pub fn undecoded(&self) -> u64 {
        self.undecoded
    }

    /// Reads everything the kernel has published, calling `handle` per record.
    ///
    /// The tail is only published back to the kernel once the batch has been
    /// copied out, so a record is never overwritten while it is being read.
    pub fn drain(&mut self, mut handle: impl FnMut(Record)) {
        let header = self.base as *mut sys::bindings::perf_event_mmap_page;
        // Acquire: everything the kernel wrote before publishing this head must
        // be visible before it is read.
        let head = unsafe { std::ptr::read_volatile(&(*header).data_head) };
        std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
        let mut tail = unsafe { std::ptr::read_volatile(&(*header).data_tail) };

        while tail < head {
            let (kind, size) = self.read_header(tail);
            if size == 0 {
                // A zero-length record would loop forever; treat the rest of
                // the batch as unreadable rather than spin.
                self.undecoded += 1;
                break;
            }
            let record = self.decode(kind, tail, size);
            match &record {
                Record::Lost { count } => self.lost += count,
                Record::Other { .. } => self.undecoded += 1,
                _ => {}
            }
            handle(record);
            tail += size;
        }

        std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
        unsafe { std::ptr::write_volatile(&mut (*header).data_tail, tail) };
    }

    fn data(&self) -> *const u8 {
        unsafe { self.base.add(page_size()) }
    }

    /// Copies `len` bytes starting at ring offset `offset`, following the wrap.
    fn copy_out(&self, offset: u64, len: usize) -> Vec<u8> {
        // SAFETY: the mapping is `data_size` bytes long and `read_wrapped`
        // never reads past it; the kernel writes there and this process only
        // reads.
        let ring = unsafe { std::slice::from_raw_parts(self.data(), self.data_size as usize) };
        read_wrapped(ring, offset, len)
    }

    fn read_header(&self, offset: u64) -> (u32, u64) {
        let bytes = self.copy_out(offset, 8);
        let kind = u32::from_ne_bytes(bytes[0..4].try_into().expect("4 bytes"));
        let size = u16::from_ne_bytes(bytes[6..8].try_into().expect("2 bytes")) as u64;
        (kind, size)
    }

    fn decode(&self, kind: u32, offset: u64, size: u64) -> Record {
        let body = self.copy_out(offset, size as usize);
        let field = |index: usize| -> u64 {
            let start = 8 + index * 8;
            body.get(start..start + 8)
                .map(|bytes| u64::from_ne_bytes(bytes.try_into().expect("8 bytes")))
                .unwrap_or(0)
        };
        match kind {
            sys::bindings::PERF_RECORD_SAMPLE => {
                let tid_field = field(1);
                Record::Sample(Sample {
                    ip: field(0),
                    pid: (tid_field & 0xffff_ffff) as u32,
                    tid: (tid_field >> 32) as u32,
                    time_ns: field(2),
                    addr: field(3),
                })
            }
            sys::bindings::PERF_RECORD_LOST => Record::Lost { count: field(1) },
            sys::bindings::PERF_RECORD_THROTTLE => Record::Throttle { throttled: true },
            sys::bindings::PERF_RECORD_UNTHROTTLE => Record::Throttle { throttled: false },
            other => Record::Other { kind: other },
        }
    }
}

impl Drop for SampledEvent {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.base as *mut libc::c_void, self.map_len);
            libc::close(self.fd);
        }
    }
}

/// Copies `len` bytes from ring offset `offset`, following the wrap.
///
/// A ring buffer has no end, only a seam, and a record that straddles it is
/// perfectly ordinary. Split out as a free function because that seam is the
/// one piece of this module whose correctness does not need a kernel to check.
fn read_wrapped(ring: &[u8], offset: u64, len: usize) -> Vec<u8> {
    if ring.is_empty() {
        return vec![0; len];
    }
    let size = ring.len();
    let start = (offset % size as u64) as usize;
    let mut bytes = Vec::with_capacity(len);
    let first = len.min(size - start);
    bytes.extend_from_slice(&ring[start..start + first]);
    let mut remaining = len - first;
    // A read longer than the ring itself would wrap more than once. It cannot
    // happen for a record, but going around rather than panicking keeps the
    // seam logic total.
    while remaining > 0 {
        let take = remaining.min(size);
        bytes.extend_from_slice(&ring[..take]);
        remaining -= take;
    }
    bytes
}

fn page_size() -> usize {
    // SAFETY: `sysconf` takes an integer and returns one.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if size > 0 {
        size as usize
    } else {
        4096
    }
}

/// Whether this machine lets an unprivileged process sample its own page
/// faults, and why not when it does not.
pub fn self_sampling_error() -> Option<String> {
    match SampledEvent::open_software(
        sys::bindings::PERF_COUNT_SW_PAGE_FAULTS as u64,
        1,
        Scope::Process(0),
        4,
    ) {
        Ok(_) => None,
        Err(error) => Some(format!(
            "perf_event_open was refused ({error}); check /proc/sys/kernel/perf_event_paranoid"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE_FAULTS: u64 = sys::bindings::PERF_COUNT_SW_PAGE_FAULTS as u64;

    #[test]
    fn a_record_that_straddles_the_seam_is_read_whole() {
        // Sixteen bytes of ring, numbered so any misread is visible.
        let ring: Vec<u8> = (0..16).collect();

        // Wholly before the seam.
        assert_eq!(read_wrapped(&ring, 4, 4), vec![4, 5, 6, 7]);
        // Ending exactly on it.
        assert_eq!(read_wrapped(&ring, 12, 4), vec![12, 13, 14, 15]);
        // Straddling it: the tail of the ring, then the head.
        assert_eq!(read_wrapped(&ring, 14, 4), vec![14, 15, 0, 1]);
        // An offset past the end is the same position, one lap later, which is
        // what the kernel's ever-growing head and tail mean.
        assert_eq!(read_wrapped(&ring, 16 + 14, 4), read_wrapped(&ring, 14, 4));
        assert_eq!(read_wrapped(&ring, 16 * 1000 + 3, 2), vec![3, 4]);
    }

    #[test]
    fn a_read_longer_than_the_ring_goes_around_rather_than_panicking() {
        let ring: Vec<u8> = (0..4).collect();
        assert_eq!(
            read_wrapped(&ring, 2, 10),
            vec![2, 3, 0, 1, 2, 3, 0, 1, 2, 3]
        );
        assert_eq!(read_wrapped(&[], 0, 3), vec![0, 0, 0]);
    }

    #[test]
    fn a_page_fault_sample_carries_the_address_that_faulted() {
        let Ok(mut event) = SampledEvent::open_software(PAGE_FAULTS, 1, Scope::Process(0), 64)
        else {
            eprintln!("skipped: this machine refuses unprivileged perf sampling");
            return;
        };
        event.enable().expect("enable");

        // Mapped, not allocated. `free()` returns nothing to the kernel, so an
        // allocator hands back pages that are already resident and touching
        // them faults nothing: measured here, a third identical `Vec` produced
        // zero faults, which is why this test used to fail only inside a full
        // suite run and pass on its own. A fresh anonymous mapping is the only
        // way to know the pages are not there yet.
        const PAGES: usize = 64;
        let page = page_size();
        let block = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                PAGES * page,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(block, libc::MAP_FAILED, "the test needs a fresh mapping");
        let base = block as u64;
        for index in 0..PAGES {
            unsafe { std::ptr::write_volatile((block as *mut u8).add(index * page), 1) };
        }
        event.disable().expect("disable");

        let (mut samples, mut inside) = (0, 0);
        event.drain(|record| {
            if let Record::Sample(sample) = record {
                samples += 1;
                if (base..base + (PAGES * page) as u64).contains(&sample.addr) {
                    inside += 1;
                }
            }
        });

        // Touching N pages does not produce N faults. On a kernel with
        // multi-size transparent huge pages one fault installs a whole folio,
        // which on this family is up to sixteen base pages, so the count is a
        // fraction of the pages touched and varies run to run. Asserting the
        // page count was a test encoding an expectation the kernel never made.
        //
        // What is worth asserting is that the addresses are real: the block
        // sits at an address the allocator chose, so samples landing inside it
        // cannot be coincidence.
        assert!(samples > 0, "no page-fault sample arrived at all");
        assert!(
            inside >= PAGES / 16,
            "only {inside} of {samples} samples landed in the {PAGES} pages that were touched, which is below even one fault per sixteen-page folio"
        );
        assert!(
            inside <= PAGES,
            "{inside} samples landed in a block of {PAGES} pages, so an address is being reported that was never touched"
        );
        unsafe { libc::munmap(block, PAGES * page) };
    }

    #[test]
    fn sample_timestamps_share_the_tools_clock_domain() {
        let Ok(mut event) = SampledEvent::open_software(PAGE_FAULTS, 1, Scope::Process(0), 16)
        else {
            return;
        };
        event.enable().expect("enable");
        // Enough pages that a folio of any size still yields several faults,
        // and mapped rather than allocated: a block from the allocator's arena
        // can come back on pages another test in this binary already made
        // resident, and a page that is already there does not fault. The test
        // would then be waiting for events that had no reason to happen.
        const PAGES: usize = 256;
        let size = PAGES * page_size();
        // SAFETY: an anonymous mapping of our own, unmapped below.
        let block = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(block, libc::MAP_FAILED, "the mapping must succeed");
        for index in 0..PAGES {
            // SAFETY: writing one byte per page inside that mapping.
            unsafe {
                block
                    .cast::<u8>()
                    .add(index * page_size())
                    .write_volatile(1)
            };
        }
        event.disable().expect("disable");
        // SAFETY: unmapping what was mapped above.
        unsafe { libc::munmap(block, size) };
        let after = crate::clock::monotonic_ns();

        let mut seen = None;
        event.drain(|record| {
            if let Record::Sample(sample) = record {
                seen.get_or_insert(sample.time_ns);
            }
        });

        let time = seen.expect("at least one sample");
        assert!(
            time <= after,
            "a sample cannot be newer than the clock read after it"
        );
        assert!(
            after - time < 5_000_000_000,
            "sample clock and CLOCK_MONOTONIC are {}ns apart, so they are different clocks",
            after - time
        );
    }
}
