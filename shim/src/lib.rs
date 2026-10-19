//! The preload shim: what `LD_PRELOAD` sees.
//!
//! This library replaces entry points in the observed program and reports each
//! call to `omn` through one pipe. It started with `malloc` and friends; since
//! the ninth wave it also interposes `pthread_mutex_lock` and its neighbours
//! (`mutex.rs`, for `sched.mutex`) and the host side of the CUDA driver and
//! Vulkan submission paths (`gpu.rs`, for `gpu.api`). It is preferred over a
//! uprobe for one reason: a uprobe costs one to two microseconds per call,
//! which on an allocation- or lock-heavy program is not an observation but a
//! different program. A shim costs tens of nanoseconds.
//!
//! **One pipe, several classes.** A launch carries one shim stream however
//! many preload sources read it, so what the program is asked to report is a
//! set of classes, named in `OMN_SHIM_REPORT`: a hook whose class was not
//! asked for calls straight through and writes nothing. A heap-only run does
//! not pay for mutex records, and a mutex-only run does not fill the pipe with
//! allocations.
//!
//! Three rules govern everything here, and breaking any one of them corrupts
//! the program being measured rather than merely the measurement.
//!
//! **Never allocate in a hook.** The reporting path uses a stack buffer and a
//! raw `write`, and every entry point is guarded against re-entry, because the
//! moment this code allocates it calls itself.
//!
//! **Never block the program.** The pipe is non-blocking. When it is full the
//! record is dropped and counted, and the count is reported as soon as there is
//! room again, so the loss appears in the output instead of appearing as a
//! stall in the program under measurement.
//!
//! **Never fail the program.** If the pipe is missing, closed, or was never
//! configured, the hooks still allocate and free correctly. Losing the
//! measurement is acceptable; changing the program's behaviour is not.
//!
//! **No system call per record, when `omn` hands over a ring.** A record used
//! to cost a `gettid` and a `write(2)`: measured on a million allocations, 7 ms
//! bare became 1.13 s, about 560 ns a record, and every thread of the program
//! queued on the one pipe (finding 47). Since then `omn` maps a shared ring
//! (`OMN_SHIM_RING_FD`), the thread id is read once per thread, and a record
//! is a reservation, a copy and a release store. The pipe stays as the
//! fallback for a ring that could not be mapped, and the three rules above
//! hold for both: a full ring drops and counts, in the ring's own header.
//!
//! **What the shim costs the program is published beside what it reports.**
//! One hooked call in [`SAMPLE_EVERY`] of each thread is timed whole, from
//! the hook's entry to its return, less the real function it wraps -- the
//! allocation, the lock, the driver call are the program's own work -- and
//! the time is added to its class's account in the ring's header
//! (`ring::SPENT`, `ring::TIMED`). `omn` scales it up to every call and
//! reports it as what the collector spent inside the target (finding 47): the
//! tool's own cpu is the reader's, and for a preloaded library that is not
//! where the cost is. Timing one call in seventeen costs a counter increment
//! on the other sixteen, where timing each would add four clock reads to a
//! call whose record costs about three. Seventeen rather than sixteen because
//! a thread that alternates two hooks -- lock, unlock -- would otherwise time
//! only one of them.

use std::cell::Cell;
use std::ffi::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicU8, AtomicUsize, Ordering};

mod gpu;
mod mutex;

/// Environment variable naming the file descriptor to report on.
const FD_VARIABLE: &[u8] = b"OMN_SHIM_FD\0";

/// Environment variables naming the shared ring and its size in slots, when
/// `omn` provides one. See [`ring`].
const RING_FD_VARIABLE: &[u8] = b"OMN_SHIM_RING_FD\0";
const RING_SLOTS_VARIABLE: &[u8] = b"OMN_SHIM_RING_SLOTS\0";

/// Environment variable naming the classes to report, as a decimal bit set of
/// the `CLASS_*` values. Absent means the heap alone, which is what the shim
/// reported before it knew anything else.
const CLASSES_VARIABLE: &[u8] = b"OMN_SHIM_REPORT\0";

/// What a hook belongs to, and therefore whether it reports at all.
const CLASS_HEAP: u8 = 1;
const CLASS_MUTEX: u8 = 2;
const CLASS_GPU: u8 = 4;
const CLASSES: [u8; 3] = [CLASS_HEAP, CLASS_MUTEX, CLASS_GPU];

const OP_ALLOC: u8 = 1;
const OP_FREE: u8 = 2;
/// Records the shim had to throw away, so the loss is in the data. `ptr`
/// carries the class they belonged to and `size` how many there were: a mutex
/// record lost is not an allocation lost, and the reader of each class counts
/// only its own.
const OP_DROPPED: u8 = 3;
/// One acquisition of a pthread mutex, reported when it is released: `ptr` is
/// the mutex, `size` the wait in nanoseconds and `extra` the hold.
const OP_MUTEX: u8 = 4;
/// One call into a GPU API entry point: `ptr` is the entry point's index in
/// `gpu::ENTRY_POINTS`, `size` the time spent inside it and `extra` what it
/// returned, sign-extended.
const OP_GPU_CALL: u8 = 5;

/// One reported event. Fixed size and native endian: both sides are the same
/// build on the same machine, and a length prefix would cost a branch in the
/// hot path for nothing.
#[repr(C)]
#[derive(Clone, Copy)]
struct Record {
    ts_ns: u64,
    ptr: u64,
    size: u64,
    extra: u64,
    tid: u32,
    op: u8,
    /// Per operation; `mutex::FLAG_*` for a mutex record, zero otherwise.
    flags: u8,
    padding: [u8; 2],
}

const RECORD_SIZE: usize = std::mem::size_of::<Record>();

static REPORT_FD: AtomicI32 = AtomicI32::new(-1);
static REPORTED_CLASSES: AtomicU8 = AtomicU8::new(0);
static CONFIGURED: AtomicBool = AtomicBool::new(false);
/// Records dropped and not yet reported, one counter per class.
static DROPPED: [AtomicU64; 3] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];

fn dropped_slot(class: u8) -> &'static AtomicU64 {
    &DROPPED[class.trailing_zeros() as usize]
}

static REAL_MALLOC: AtomicUsize = AtomicUsize::new(0);
static REAL_CALLOC: AtomicUsize = AtomicUsize::new(0);
static REAL_REALLOC: AtomicUsize = AtomicUsize::new(0);
static REAL_FREE: AtomicUsize = AtomicUsize::new(0);
static REAL_POSIX_MEMALIGN: AtomicUsize = AtomicUsize::new(0);
static REAL_ALIGNED_ALLOC: AtomicUsize = AtomicUsize::new(0);
static REAL_MEMALIGN: AtomicUsize = AtomicUsize::new(0);
static REAL_VALLOC: AtomicUsize = AtomicUsize::new(0);
static REAL_PVALLOC: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// Set while a hook is running, so anything the hook itself allocates goes
    /// straight to the real allocator and is not reported. Const-initialized
    /// and `Copy`, so touching it never allocates and never registers a
    /// destructor.
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
    /// Set while this thread is inside `dlsym`, for the allocation `dlsym`
    /// itself makes. Const-initialized, like `IN_HOOK`.
    static RESOLVING: Cell<bool> = const { Cell::new(false) };
    /// This thread's id, read once: `gettid` is a system call, and it was half
    /// the price of a record. Zero until the first read, and zeroed again in a
    /// forked child, whose one thread has a new id.
    static TID: Cell<u32> = const { Cell::new(0) };
}

/// Allocations served before `dlsym` has produced the real allocator.
///
/// `dlsym` itself may allocate, so the first call into this shim cannot rely on
/// the real `malloc` existing yet. A bump region breaks the cycle; it is small
/// because only the dynamic loader's own bootstrap comes through it.
const BOOTSTRAP_CAPACITY: usize = 64 * 1024;

/// The region is aligned, not merely its block sizes. A plain `static mut
/// [u8; N]` is byte-aligned, so every block cut from it inherits whatever
/// address the linker chose -- and `malloc` promises sixteen. The loader stores
/// pointers and doubles in what it gets back, and on some targets a misaligned
/// store is a fault rather than a slowdown.
#[repr(C, align(16))]
struct Bootstrap([u8; BOOTSTRAP_CAPACITY]);

static mut BOOTSTRAP: Bootstrap = Bootstrap([0; BOOTSTRAP_CAPACITY]);
static BOOTSTRAP_USED: AtomicUsize = AtomicUsize::new(0);

fn bootstrap_alloc(size: usize) -> *mut c_void {
    // Rounded up to malloc's alignment promise. A `malloc` replacement may not
    // panic, so the rounding of an absurd size is checked rather than wrapped:
    // in a build with overflow checks this is an abort inside the observed
    // program, and in one without it is a tiny allocation for a huge request.
    let Some(size) = size.checked_add(15).map(|rounded| rounded & !15) else {
        return std::ptr::null_mut();
    };

    // The reservation has to be all-or-nothing. Adding first and checking after
    // would consume the space even when the request does not fit, so one
    // oversized request during loader bootstrap would permanently exhaust the
    // region and every small allocation after it would fail too.
    let mut used = BOOTSTRAP_USED.load(Ordering::Relaxed);
    loop {
        let Some(end) = used.checked_add(size) else {
            return std::ptr::null_mut();
        };
        if end > BOOTSTRAP_CAPACITY {
            return std::ptr::null_mut();
        }
        match BOOTSTRAP_USED.compare_exchange_weak(used, end, Ordering::Relaxed, Ordering::Relaxed)
        {
            // SAFETY: this thread reserved `used..end` and nothing else can
            // hand it out.
            Ok(_) => return unsafe { (&raw mut BOOTSTRAP).cast::<u8>().add(used).cast() },
            Err(actual) => used = actual,
        }
    }
}

fn is_bootstrap(ptr: *mut c_void) -> bool {
    let base = (&raw const BOOTSTRAP).cast::<u8>() as usize;
    let address = ptr as usize;
    address >= base && address < base + BOOTSTRAP_CAPACITY
}

/// Resolves one symbol from the next object in the link order.
fn resolve(slot: &AtomicUsize, name: &[u8]) -> usize {
    let cached = slot.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }
    // Before glibc 2.34, `dlsym` calls `calloc` (in `_dlerror_run`), which is
    // this shim's: resolving it again recursed until the stack ran out, and
    // every program that allocated died before `main` (Ubuntu 18.04's 2.27,
    // distroproof, 2026-10-19). A nested call is answered "not yet", and its
    // hook serves it from the bootstrap region.
    if RESOLVING.with(|resolving| resolving.replace(true)) {
        return 0;
    }
    // SAFETY: `name` is a NUL-terminated byte string.
    let address = unsafe { libc::dlsym(libc::RTLD_NEXT, name.as_ptr().cast::<c_char>()) } as usize;
    RESOLVING.with(|resolving| resolving.set(false));
    slot.store(address, Ordering::Relaxed);
    address
}

/// Reads a decimal number from the environment without allocating.
///
/// `getenv` does not allocate, which `std::env::var` does not promise.
fn decimal_variable(name: &[u8]) -> Option<i32> {
    // SAFETY: `name` is NUL-terminated.
    let value = unsafe { libc::getenv(name.as_ptr().cast::<c_char>()) };
    if value.is_null() {
        return None;
    }
    let mut number: i32 = 0;
    let mut index = 0isize;
    loop {
        // SAFETY: walking a NUL-terminated string from the environment.
        let byte = unsafe { *value.offset(index) } as u8;
        if !byte.is_ascii_digit() {
            break;
        }
        number = number
            .saturating_mul(10)
            .saturating_add((byte - b'0') as i32);
        index += 1;
    }
    (index > 0).then_some(number)
}

fn configure() {
    // A load first: this runs on every hooked call, and a read-modify-write on
    // one shared line from every thread of a lock-heavy program is a
    // contention of its own.
    if CONFIGURED.load(Ordering::Relaxed) || CONFIGURED.swap(true, Ordering::Relaxed) {
        return;
    }
    let classes = decimal_variable(CLASSES_VARIABLE).map_or(CLASS_HEAP, |bits| bits as u8);
    REPORTED_CLASSES.store(classes, Ordering::Relaxed);
    if let Some(fd) = decimal_variable(FD_VARIABLE) {
        REPORT_FD.store(fd, Ordering::Relaxed);
    }
    if let (Some(fd), Some(slots)) = (
        decimal_variable(RING_FD_VARIABLE),
        decimal_variable(RING_SLOTS_VARIABLE),
    ) {
        calibrate_clock();
        ring::map(fd, slots as u64);
    }
}

/// Configured before `main`, while the program has one thread.
///
/// Configuring lazily on the first hooked call is still there, for the calls
/// other constructors make before this one runs; but a lazy configuration
/// raced by two threads lets the loser see an unset descriptor and skip a
/// record, and running it here is what makes that window empty in practice.
extern "C" fn configure_at_load() {
    configure();
    // A forked child keeps the parent thread's cached id in its one thread,
    // whose id is the child's pid. Registered inside a guard: the registration
    // may allocate, and that allocation is the loader's, not the program's.
    guarded(|| {
        // SAFETY: registering a handler with no preparation and no parent half.
        unsafe { libc::pthread_atfork(None, None, Some(forget_thread_id)) };
    });
}

extern "C" fn forget_thread_id() {
    TID.with(|tid| tid.set(0));
}

#[used]
#[link_section = ".init_array"]
static CONFIGURE_AT_LOAD: extern "C" fn() = configure_at_load;

/// Whether a hook of `class` should report, which is also whether it should
/// spend anything at all on timing.
fn reporting(class: u8) -> bool {
    configure();
    REPORTED_CLASSES.load(Ordering::Relaxed) & class != 0 && REPORT_FD.load(Ordering::Relaxed) >= 0
}

/// Whether this thread is already inside a hook, whose own calls are not the
/// program's and must not be timed or reported.
fn in_hook() -> bool {
    IN_HOOK.with(Cell::get)
}

fn monotonic_ns() -> u64 {
    #[cfg(test)]
    tests::CLOCK_READS.with(|reads| reads.set(reads.get() + 1));
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `clock_gettime` fills a struct we own.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) };
    (time.tv_sec as u64) * 1_000_000_000 + (time.tv_nsec as u64)
}

fn thread_id() -> u32 {
    TID.with(|cached| {
        let known = cached.get();
        if known != 0 {
            return known;
        }
        // SAFETY: `gettid` takes no argument and cannot fail.
        let tid = unsafe { libc::syscall(libc::SYS_gettid) as u32 };
        cached.set(tid);
        tid
    })
}

/// One hooked call in this many, per thread, is timed for the shim's own
/// account. Odd, so that a thread alternating two hooks times both.
const SAMPLE_EVERY: u32 = 17;

thread_local! {
    /// Reporting calls this thread still has to make before the next one is
    /// timed. Counted down, and decided *before* the hook does anything, so
    /// that a timed call's clock starts ahead of its own checks.
    static UNTIL_TIMED: Cell<u32> = const { Cell::new(0) };
}

/// What one `CLOCK_MONOTONIC` read costs here, measured once at load and
/// published in the ring's header beside the account it is part of.
static CLOCK_READ_NS: AtomicU64 = AtomicU64::new(0);

fn calibrate_clock() {
    // The least of several short batches, not one long one: a batch the
    // program's thread was preempted in measures the preemption, and a price
    // inflated that way is subtracted from every timed call -- which on a
    // loaded machine zeroed nearly all of them (one run's account came to
    // 5 ms for 400 ms of shim).
    const BATCHES: u32 = 16;
    const READS: u64 = 8;
    let mut least = u64::MAX;
    for _ in 0..BATCHES {
        let first = monotonic_ns();
        let mut last = first;
        for _ in 0..READS {
            last = monotonic_ns();
        }
        least = least.min(last.saturating_sub(first) / READS);
    }
    CLOCK_READ_NS.store(least, Ordering::Relaxed);
}

/// Times one hooked call in [`SAMPLE_EVERY`], less the real function it
/// wraps, into the ring's account of what the shim spent inside the program.
///
/// A timer that did not draw this call, or whose class is not reported, or
/// that runs inside another hook (whose time already holds it), reads no
/// clock at all. Only the ring carries the account: through the pipe the shim
/// times nothing, and `omn` says the in-target cost is unknown.
///
/// The account is added when the timer is dropped, so a hook that returns
/// early -- a null `free`, a pointer from the bootstrap region -- still
/// accounts for the turn it drew: the reader scales by the sampling rate, and a
/// drawn turn that never reported would bias the account down.
pub(crate) struct Timer {
    /// Zero when this call is not timed.
    started_ns: u64,
    /// Time spent in the real function, which is the program's.
    excluded_ns: u64,
    class: u8,
}

impl Timer {
    /// The first thing a hook does. Whether the call reports is decided
    /// before any clock is read: a call of a class nobody asked for -- every
    /// `malloc` under a mutex-only session -- must cost the program a few
    /// loads, not a `CLOCK_MONOTONIC` read (20 M `malloc`/`free` pairs went
    /// from 0.12 s bare to 1.07 s when the clock came first). The price is
    /// that those few loads fall outside a timed call's span, which
    /// understates the account by a handful of nanoseconds per timed call.
    pub(crate) fn start(class: u8) -> Timer {
        if !ring::mapped() || in_hook() || !reporting(class) {
            // Not a reporting call: it neither draws nor spends a turn, and
            // reads no clock.
            return Timer {
                started_ns: 0,
                excluded_ns: 0,
                class,
            };
        }
        let until = UNTIL_TIMED.with(Cell::get);
        let started_ns = if until == 0 { monotonic_ns() } else { 0 };
        UNTIL_TIMED.with(|slot| {
            slot.set(if until == 0 {
                SAMPLE_EVERY - 1
            } else {
                until - 1
            })
        });
        Timer {
            started_ns,
            excluded_ns: 0,
            class,
        }
    }

    /// Runs the real function, keeping its time out of the account.
    pub(crate) fn around<T>(&mut self, real: impl FnOnce() -> T) -> T {
        if self.started_ns == 0 {
            return real();
        }
        let entered = monotonic_ns();
        let result = real();
        self.excluded_ns += monotonic_ns().saturating_sub(entered);
        result
    }
}

impl Drop for Timer {
    /// Adds this call's time to its class's account, less what timing it
    /// added: the span holds about two clock reads' worth of the four a timed
    /// call makes -- the end of the first, the start of the one before the
    /// real function, the end of the one after it and the start of the last
    /// -- which an untimed call does not pay.
    fn drop(&mut self) {
        if self.started_ns == 0 {
            return;
        }
        match own_time(
            monotonic_ns().saturating_sub(self.started_ns),
            self.excluded_ns,
            CLOCK_READ_NS.load(Ordering::Relaxed),
        ) {
            Some(spent) => ring::spent(self.class, spent),
            None => ring::left_out(self.class),
        }
    }
}

/// A hook's own time over a span longer than this is not the hook's: nothing
/// the shim does between its clock reads -- a few checks, a record copied
/// into the ring -- comes within three orders of magnitude of it, and a
/// preemption of the program's thread inside the span does. One such span
/// on a two-cpu CI runner, scaled by [`SAMPLE_EVERY`], put a scheduler slice
/// seventeen times into the account, which came to 610 ms for 396 ms the
/// program lost.
const PREEMPTED_NS: u64 = 200_000;

/// What a timed call spent of the shim's own: its span, less the real function
/// and the two clock reads timing added. `None` for a span the thread was
/// preempted in: it is none of the hook's, and the ring counts it apart
/// ([`ring::left_out`]) rather than adding a time slice to the account or
/// dropping it unsaid. `omn` prices such a turn at the mean of the others and
/// says how many there were.
fn own_time(span_ns: u64, excluded_ns: u64, clock_read_ns: u64) -> Option<u64> {
    let own = span_ns
        .saturating_sub(excluded_ns)
        .saturating_sub(2 * clock_read_ns);
    (own <= PREEMPTED_NS).then_some(own)
}

/// Writes one allocator record, when the heap was asked for.
fn report_heap(op: u8, ptr: *mut c_void, size: u64) {
    if !reporting(CLASS_HEAP) {
        return;
    }
    report(
        CLASS_HEAP,
        Record {
            ts_ns: monotonic_ns(),
            ptr: ptr as u64,
            size,
            extra: 0,
            tid: thread_id(),
            op,
            flags: 0,
            padding: [0; 2],
        },
    );
}

fn dropped_record(class: u8, count: u64) -> Record {
    Record {
        ts_ns: monotonic_ns(),
        ptr: u64::from(class),
        size: count,
        extra: 0,
        tid: thread_id(),
        op: OP_DROPPED,
        flags: 0,
        padding: [0; 2],
    }
}

/// Writes one record, dropping it rather than blocking the program.
fn report(class: u8, record: Record) {
    configure();
    if ring::write(class, &record) {
        return;
    }
    let fd = REPORT_FD.load(Ordering::Relaxed);
    if fd < 0 {
        return;
    }
    let slot = dropped_slot(class);

    // A drop that is never reported is a drop nobody can account for, so the
    // backlog rides along with the next record that gets through.
    let dropped = slot.swap(0, Ordering::Relaxed);
    if dropped > 0 && !write_record(fd, &dropped_record(class, dropped)) {
        // The pipe is full, so the record this call was for is not going to
        // fit either: it goes back with the backlog rather than leaving
        // through the `return` uncounted. Counting the backlog alone is how
        // the counter came to report seven drops for thirty-one thousand lost
        // records -- one per burst, because the first record of a burst
        // incremented it and every record behind it fell out here.
        slot.fetch_add(dropped + 1, Ordering::Relaxed);
        return;
    }

    if !write_record(fd, &record) {
        slot.fetch_add(1, Ordering::Relaxed);
    }
}

/// How many times the exit flush retries a full pipe, a millisecond apart.
///
/// The reader drains until the program is reaped, and this runs before that, so
/// a full pipe here empties within a tick or two. The bound is what keeps a
/// reader that has already gone from turning the end of a program into a hang.
const FLUSH_ATTEMPTS: u32 = 200;

/// Reports the backlog the program ends holding.
///
/// The backlog rides out on the next record that gets through, and at the end
/// of a run there is no next record: whatever was dropped in the last burst is
/// counted in `DROPPED` and never surfaces. On the two-hundred-thousand
/// allocation storm that was up to six thousand records -- counted, and lost
/// between the counter and the reader, which is the same invariant broken one
/// step further along.
///
/// This is the one place the shim waits for the pipe, and it is allowed to:
/// the program's last instruction has already run, so what is being delayed is
/// the teardown of a process that has finished rather than the work being
/// measured.
extern "C" fn flush_at_exit() {
    let fd = REPORT_FD.load(Ordering::Relaxed);
    if fd < 0 {
        return;
    }
    let mut attempts = 0;
    for class in CLASSES {
        let slot = dropped_slot(class);
        let dropped = slot.swap(0, Ordering::Relaxed);
        if dropped == 0 {
            continue;
        }
        loop {
            if write_record(fd, &dropped_record(class, dropped)) {
                break;
            }
            attempts += 1;
            if attempts >= FLUSH_ATTEMPTS {
                // Put it back rather than pretend: nothing else will read it,
                // but a counter that silently zeroes itself is how this
                // started.
                slot.fetch_add(dropped, Ordering::Relaxed);
                return;
            }
            // SAFETY: sleeping for a millisecond takes no memory and cannot
            // fail in a way that matters here.
            unsafe { libc::usleep(1_000) };
        }
    }
}

#[used]
#[link_section = ".fini_array"]
static FLUSH_AT_EXIT: extern "C" fn() = flush_at_exit;

/// The shared ring `omn` maps and hands over, so that a record costs no
/// system call.
///
/// A multi-producer, single-consumer ring of fixed slots in a `memfd` both
/// sides map. The layout is fixed and mirrored in `src/collect/mem/heap/`:
///
/// ```text
///   0    magic                 written by omn, checked here
///   64   head   (u64)          the next slot to reserve, advanced by writers
///   128  tail   (u64)          the next slot omn will read, advanced by omn
///   192  drops  (3 x u64)      records a full ring refused, per class
///   256  spent  (3 x u64)      nanoseconds the timed calls took, per class
///   288  timed  (3 x u64)      how many calls were timed, per class
///   312  clock  (u64)          what one clock read costs, as calibrated
///   320  left   (3 x u64)      timed calls a preemption fell in, per class:
///                              counted in `timed`, their spans in no `spent`
///   384  slots  (SLOT_BYTES each): a sequence word, then the 40-byte record
/// ```
///
/// The magic changed with the account: a shim and an `omn` that disagree
/// about where the slots start refuse the ring and fall back to the pipe
/// rather than read each other's records from the wrong offset.
///
/// A writer reserves slot `h` by compare-and-swap on `head`, only while
/// `h - tail` is under the slot count -- so a slot is never reused before omn
/// has read it -- copies the record in, and publishes it by storing `h + 1`
/// into the slot's sequence word with release ordering. omn reads slots in
/// order while their sequence word says they are published, and then advances
/// `tail`. A full ring is a drop, counted in the header, never a wait.
mod ring {
    use super::{Record, RECORD_SIZE};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    pub const MAGIC: u64 = 0x3367_6e72_5f6e_6d6f; // "omn_rng3", little endian
    pub const HEAD: usize = 64;
    pub const TAIL: usize = 128;
    pub const DROPS: usize = 192;
    pub const SPENT: usize = 256;
    pub const TIMED: usize = 288;
    pub const CLOCK: usize = 312;
    pub const LEFT_OUT: usize = 320;
    pub const SLOTS: usize = 384;
    pub const SLOT_BYTES: usize = 48;

    static BASE: AtomicUsize = AtomicUsize::new(0);
    static COUNT: AtomicU64 = AtomicU64::new(0);

    /// Maps the ring omn created, or leaves the pipe in charge if it cannot.
    pub fn map(fd: i32, slots: u64) {
        if slots == 0 || !slots.is_power_of_two() {
            return;
        }
        let length = SLOTS + slots as usize * SLOT_BYTES;
        // SAFETY: mapping a descriptor omn passed for this purpose; a failure
        // is checked and leaves the ring unset.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return;
        }
        // SAFETY: the mapping is at least `SLOTS` bytes long.
        let magic = unsafe { (base as *const u64).read_volatile() };
        if magic != MAGIC {
            // SAFETY: unmapping what was just mapped.
            unsafe { libc::munmap(base, length) };
            return;
        }
        COUNT.store(slots, Ordering::Relaxed);
        word(base as usize, CLOCK).store(
            super::CLOCK_READ_NS.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        BASE.store(base as usize, Ordering::Release);
    }

    /// Whether the ring is mapped, and so whether there is an account to add
    /// the shim's time to.
    pub fn mapped() -> bool {
        BASE.load(Ordering::Relaxed) != 0
    }

    /// Adds one timed record's cost to its class's account.
    pub fn spent(class: u8, ns: u64) {
        let base = BASE.load(Ordering::Acquire);
        if base == 0 {
            return;
        }
        let at = 8 * class.trailing_zeros() as usize;
        word(base, SPENT + at).fetch_add(ns, Ordering::Relaxed);
        word(base, TIMED + at).fetch_add(1, Ordering::Relaxed);
    }

    /// Counts one timed call whose span a preemption fell in: a turn that was
    /// timed, with no time of the shim's that can be told from the
    /// scheduler's.
    pub fn left_out(class: u8) {
        let base = BASE.load(Ordering::Acquire);
        if base == 0 {
            return;
        }
        let at = 8 * class.trailing_zeros() as usize;
        word(base, LEFT_OUT + at).fetch_add(1, Ordering::Relaxed);
        word(base, TIMED + at).fetch_add(1, Ordering::Relaxed);
    }

    fn word(base: usize, offset: usize) -> &'static AtomicU64 {
        // SAFETY: every offset used is inside the mapping and 8-byte aligned,
        // and the mapping lives as long as the process.
        unsafe { &*((base + offset) as *const AtomicU64) }
    }

    /// Writes `record` into the ring. `false` when there is no ring, so the
    /// caller falls back to the pipe; a full ring is `true` -- the record was
    /// handled, by being counted as dropped.
    pub fn write(class: u8, record: &Record) -> bool {
        let base = BASE.load(Ordering::Acquire);
        if base == 0 {
            return false;
        }
        let slots = COUNT.load(Ordering::Relaxed);
        let head = word(base, HEAD);
        let tail = word(base, TAIL);
        let mut reserved = head.load(Ordering::Relaxed);
        loop {
            if reserved.wrapping_sub(tail.load(Ordering::Acquire)) >= slots {
                word(base, DROPS + 8 * class.trailing_zeros() as usize)
                    .fetch_add(1, Ordering::Relaxed);
                return true;
            }
            match head.compare_exchange_weak(
                reserved,
                reserved + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => reserved = actual,
            }
        }
        let slot = base + SLOTS + (reserved & (slots - 1)) as usize * SLOT_BYTES;
        // SAFETY: the slot is this writer's alone until it is published: it was
        // reserved above, and omn read its previous occupant before `tail`
        // moved past it.
        unsafe {
            std::ptr::copy_nonoverlapping(
                (record as *const Record).cast::<u8>(),
                (slot + 8) as *mut u8,
                RECORD_SIZE,
            );
        }
        word(slot, 0).store(reserved + 1, Ordering::Release);
        true
    }
}

fn write_record(fd: i32, record: &Record) -> bool {
    // SAFETY: writing `RECORD_SIZE` bytes from a struct of exactly that size.
    // A write this small is atomic on a pipe, so records never interleave.
    let written =
        unsafe { libc::write(fd, (record as *const Record).cast::<c_void>(), RECORD_SIZE) };
    written == RECORD_SIZE as isize
}

/// Runs `body` with reporting suppressed, returning `None` if already inside a
/// hook.
fn guarded<T>(body: impl FnOnce() -> T) -> Option<T> {
    IN_HOOK.with(|flag| {
        if flag.get() {
            return None;
        }
        flag.set(true);
        let result = body();
        flag.set(false);
        Some(result)
    })
}

/// # Safety
/// Replaces libc's `malloc` and obeys its contract.
#[no_mangle]
pub unsafe extern "C" fn malloc(size: usize) -> *mut c_void {
    let mut timer = Timer::start(CLASS_HEAP);
    let real = resolve(&REAL_MALLOC, b"malloc\0");
    if real == 0 {
        return bootstrap_alloc(size);
    }
    let real: extern "C" fn(usize) -> *mut c_void = std::mem::transmute(real);
    let pointer = timer.around(|| real(size));
    if !pointer.is_null() {
        guarded(|| report_heap(OP_ALLOC, pointer, size as u64));
    }
    pointer
}

/// # Safety
/// Replaces libc's `calloc` and obeys its contract.
#[no_mangle]
pub unsafe extern "C" fn calloc(count: usize, size: usize) -> *mut c_void {
    let mut timer = Timer::start(CLASS_HEAP);
    let real = resolve(&REAL_CALLOC, b"calloc\0");
    if real == 0 {
        // The bump region is zeroed and never reused, so it already satisfies
        // calloc's contract.
        return bootstrap_alloc(count.saturating_mul(size));
    }
    let real: extern "C" fn(usize, usize) -> *mut c_void = std::mem::transmute(real);
    let pointer = timer.around(|| real(count, size));
    if !pointer.is_null() {
        guarded(|| report_heap(OP_ALLOC, pointer, count.saturating_mul(size) as u64));
    }
    pointer
}

/// # Safety
/// Replaces libc's `realloc` and obeys its contract.
#[no_mangle]
pub unsafe extern "C" fn realloc(pointer: *mut c_void, size: usize) -> *mut c_void {
    let mut timer = Timer::start(CLASS_HEAP);
    let real = resolve(&REAL_REALLOC, b"realloc\0");
    if real == 0 || is_bootstrap(pointer) {
        let fresh = bootstrap_alloc(size);
        if !pointer.is_null() && !fresh.is_null() {
            std::ptr::copy_nonoverlapping(pointer.cast::<u8>(), fresh.cast::<u8>(), size);
        }
        return fresh;
    }
    let real: extern "C" fn(*mut c_void, usize) -> *mut c_void = std::mem::transmute(real);
    let fresh = timer.around(|| real(pointer, size));
    // A realloc is a free and an allocation, and reporting it as both is what
    // keeps the live set correct without the reader needing a special case. A
    // realloc that failed left the block where it was, and only one to zero
    // bytes gives it back without returning another.
    guarded(|| {
        if !pointer.is_null() && (!fresh.is_null() || size == 0) {
            report_heap(OP_FREE, pointer, 0);
        }
        if !fresh.is_null() {
            report_heap(OP_ALLOC, fresh, size as u64);
        }
    });
    fresh
}

/// # Safety
/// Replaces libc's `free` and obeys its contract.
#[no_mangle]
pub unsafe extern "C" fn free(pointer: *mut c_void) {
    let mut timer = Timer::start(CLASS_HEAP);
    if pointer.is_null() {
        return;
    }
    if is_bootstrap(pointer) {
        // The bump region is never given back; it is a few tens of kilobytes
        // that live as long as the process.
        return;
    }
    let real = resolve(&REAL_FREE, b"free\0");
    if real == 0 {
        return;
    }
    guarded(|| report_heap(OP_FREE, pointer, 0));
    let real: extern "C" fn(*mut c_void) = std::mem::transmute(real);
    timer.around(|| real(pointer));
}

/// # Safety
/// Replaces libc's `posix_memalign` and obeys its contract.
#[no_mangle]
pub unsafe extern "C" fn posix_memalign(
    out: *mut *mut c_void,
    alignment: usize,
    size: usize,
) -> c_int {
    let mut timer = Timer::start(CLASS_HEAP);
    let real = resolve(&REAL_POSIX_MEMALIGN, b"posix_memalign\0");
    if real == 0 {
        return libc::ENOMEM;
    }
    let real: extern "C" fn(*mut *mut c_void, usize, usize) -> c_int = std::mem::transmute(real);
    let result = timer.around(|| real(out, alignment, size));
    if result == 0 && !out.is_null() {
        let pointer = *out;
        guarded(|| report_heap(OP_ALLOC, pointer, size as u64));
    }
    result
}

// The aligned entry points below are why a hook on `malloc` is not a hook on
// the heap. glibc implements each of them on its internal allocator, not
// through `malloc`'s PLT slot (`reallocarray` does call `realloc`'s, and is
// seen through it; `tests/aggregation.rs` holds both), so an allocation made through one of them never
// reaches the `malloc` above: `dd bs=64M` allocates its 64 MiB buffer with
// `aligned_alloc`, and before these hooks a run of it reported a largest
// allocation of 4 KiB and no loss. An allocation nobody saw, with nothing
// counted, is the silent kind of loss. Each one reports the size the caller
// asked for, as `malloc` does, and `pvalloc`'s rounding up to a page is the
// allocator's, not the program's.
//
// Before `dlsym` has answered there is no bootstrap for them: the bump region
// is 16-byte aligned and nothing larger is promised, so an aligned request
// that early is refused the way the allocator refuses one it cannot serve.

/// Calls the aligned allocator `slot` names with `(alignment, size)`, and
/// reports what it returned.
///
/// # Safety
/// `name` must name a libc function of type `fn(usize, usize) -> *mut c_void`.
unsafe fn aligned(slot: &AtomicUsize, name: &[u8], alignment: usize, size: usize) -> *mut c_void {
    let mut timer = Timer::start(CLASS_HEAP);
    let real = resolve(slot, name);
    if real == 0 {
        *libc::__errno_location() = libc::ENOMEM;
        return std::ptr::null_mut();
    }
    let real: extern "C" fn(usize, usize) -> *mut c_void = std::mem::transmute(real);
    let pointer = timer.around(|| real(alignment, size));
    if !pointer.is_null() {
        guarded(|| report_heap(OP_ALLOC, pointer, size as u64));
    }
    pointer
}

/// Calls the page-aligned allocator `slot` names with `size`, and reports what
/// it returned.
///
/// # Safety
/// `name` must name a libc function of type `fn(usize) -> *mut c_void`.
unsafe fn page_aligned(slot: &AtomicUsize, name: &[u8], size: usize) -> *mut c_void {
    let mut timer = Timer::start(CLASS_HEAP);
    let real = resolve(slot, name);
    if real == 0 {
        *libc::__errno_location() = libc::ENOMEM;
        return std::ptr::null_mut();
    }
    let real: extern "C" fn(usize) -> *mut c_void = std::mem::transmute(real);
    let pointer = timer.around(|| real(size));
    if !pointer.is_null() {
        guarded(|| report_heap(OP_ALLOC, pointer, size as u64));
    }
    pointer
}

/// # Safety
/// Replaces libc's `aligned_alloc` and obeys its contract.
#[no_mangle]
pub unsafe extern "C" fn aligned_alloc(alignment: usize, size: usize) -> *mut c_void {
    aligned(&REAL_ALIGNED_ALLOC, b"aligned_alloc\0", alignment, size)
}

/// # Safety
/// Replaces libc's `memalign` and obeys its contract.
#[no_mangle]
pub unsafe extern "C" fn memalign(alignment: usize, size: usize) -> *mut c_void {
    aligned(&REAL_MEMALIGN, b"memalign\0", alignment, size)
}

/// # Safety
/// Replaces libc's `valloc` and obeys its contract.
#[no_mangle]
pub unsafe extern "C" fn valloc(size: usize) -> *mut c_void {
    page_aligned(&REAL_VALLOC, b"valloc\0", size)
}

/// # Safety
/// Replaces libc's `pvalloc` and obeys its contract.
#[no_mangle]
pub unsafe extern "C" fn pvalloc(size: usize) -> *mut c_void {
    page_aligned(&REAL_PVALLOC, b"pvalloc\0", size)
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        /// `CLOCK_MONOTONIC` reads this thread made, counted by `monotonic_ns`.
        pub(super) static CLOCK_READS: Cell<u64> = const { Cell::new(0) };
    }

    /// A call of a class nobody reports must not read the clock, even on the
    /// turn that would be timed: the clock read was what a mutex-only session
    /// cost every `malloc` (`docs/open-findings.md`, the seventeenth wave,
    /// stream build, `shim/src/lib.rs:384`). No ring is mapped in this test
    /// binary, so nothing reports, and the turn counter is at the timed turn.
    #[test]
    fn a_call_that_does_not_report_reads_no_clock() {
        UNTIL_TIMED.with(|slot| slot.set(0));
        let before = CLOCK_READS.with(Cell::get);
        for class in [CLASS_HEAP, CLASS_MUTEX, CLASS_GPU] {
            let mut timer = Timer::start(class);
            assert_eq!(timer.started_ns, 0, "an unreported call is not timed");
            assert_eq!(timer.around(|| 7), 7);
            drop(timer);
        }
        assert_eq!(
            CLOCK_READS.with(Cell::get),
            before,
            "a call that does not report read the clock"
        );
        assert_eq!(
            UNTIL_TIMED.with(Cell::get),
            0,
            "an unreported call drew a turn"
        );
    }

    /// The record layout is an agreement between this library and the reader in
    /// `src/collect/mem/heap/mod.rs`, and nothing in the type system holds the
    /// two together: the reader decodes by offset from a byte stream. If they
    /// ever disagree, every record is misparsed and the numbers stay plausible,
    /// which is the worst way for it to break. So both sides assert the layout.
    #[test]
    fn the_record_layout_matches_what_the_reader_decodes() {
        assert_eq!(RECORD_SIZE, 40, "the reader hardcodes 40 bytes per record");
        assert_eq!(std::mem::offset_of!(Record, ts_ns), 0);
        assert_eq!(std::mem::offset_of!(Record, ptr), 8);
        assert_eq!(std::mem::offset_of!(Record, size), 16);
        assert_eq!(std::mem::offset_of!(Record, extra), 24);
        assert_eq!(std::mem::offset_of!(Record, tid), 32);
        assert_eq!(std::mem::offset_of!(Record, op), 36);
        assert_eq!(std::mem::offset_of!(Record, flags), 37);
    }

    #[test]
    fn the_operation_codes_are_the_ones_the_reader_expects() {
        // Named here and in the reader; a silent renumbering would turn every
        // allocation into a free.
        assert_eq!(
            (OP_ALLOC, OP_FREE, OP_DROPPED, OP_MUTEX, OP_GPU_CALL),
            (1, 2, 3, 4, 5)
        );
        assert_eq!((CLASS_HEAP, CLASS_MUTEX, CLASS_GPU), (1, 2, 4));
    }

    /// Each class counts its own drops, so a burst of lock records the pipe
    /// could not hold is not charged to the heap sources reading beside it.
    #[test]
    fn every_class_has_a_drop_counter_of_its_own() {
        let slots: Vec<*const AtomicU64> = CLASSES
            .iter()
            .map(|&class| dropped_slot(class) as *const AtomicU64)
            .collect();
        assert_ne!(slots[0], slots[1]);
        assert_ne!(slots[1], slots[2]);
        assert_ne!(slots[0], slots[2]);
    }

    /// The bump region exists to serve the dynamic loader before `dlsym` has
    /// produced the real allocator. It must hand back distinct, aligned,
    /// in-bounds blocks, and refuse rather than overrun when it is exhausted.
    #[test]
    fn the_bootstrap_region_hands_out_distinct_aligned_blocks() {
        let first = bootstrap_alloc(24);
        let second = bootstrap_alloc(24);
        assert!(!first.is_null() && !second.is_null());
        assert_ne!(first, second, "two allocations must not share an address");
        assert_eq!(first as usize % 16, 0, "malloc promises 16-byte alignment");
        assert_eq!(second as usize % 16, 0);
        assert!(
            (second as usize) - (first as usize) >= 24,
            "the blocks overlap"
        );
        assert!(is_bootstrap(first) && is_bootstrap(second));
    }

    /// What `dlsym` of glibc 2.27 does: allocate through this shim while it is
    /// resolving. The nested resolution must answer "not yet" rather than
    /// call `dlsym` again, and must leave the slot to the outer one.
    #[test]
    fn a_resolution_inside_dlsym_is_not_attempted_again() {
        let slot = AtomicUsize::new(0);
        RESOLVING.with(|resolving| resolving.set(true));
        assert_eq!(resolve(&slot, b"malloc\0"), 0);
        assert_eq!(slot.load(Ordering::Relaxed), 0);
        RESOLVING.with(|resolving| resolving.set(false));
        assert_ne!(resolve(&slot, b"malloc\0"), 0);
    }

    #[test]
    fn a_refused_bootstrap_request_does_not_consume_the_region() {
        // Ask for more than the whole region, twice, then ask for something
        // that plainly fits. A refusal that still advanced the cursor would
        // have exhausted the region for everyone after it -- which is what a
        // failing loader bootstrap would have done in the real thing.
        assert!(
            bootstrap_alloc(BOOTSTRAP_CAPACITY * 2).is_null(),
            "a request larger than the region must fail"
        );
        assert!(
            bootstrap_alloc(usize::MAX).is_null(),
            "and must not overflow"
        );
        assert!(
            !bootstrap_alloc(16).is_null(),
            "a refused request must leave the region usable"
        );
    }

    #[test]
    fn a_pointer_from_elsewhere_is_not_mistaken_for_a_bootstrap_block() {
        let mut owned = 0u64;
        let outside: *mut c_void = (&raw mut owned).cast();
        assert!(!is_bootstrap(outside));
        assert!(!is_bootstrap(std::ptr::null_mut()));
    }

    /// The ring's numbers, pinned here and in `src/collect/mem/heap/`: a
    /// renumbering on one side would have omn read every record from the
    /// wrong offset.
    #[test]
    fn the_ring_layout_is_the_one_omn_reads() {
        assert_eq!(ring::MAGIC, 0x3367_6e72_5f6e_6d6f);
        assert_eq!(
            (
                ring::HEAD,
                ring::TAIL,
                ring::DROPS,
                ring::SPENT,
                ring::TIMED,
                ring::CLOCK,
                ring::LEFT_OUT,
                ring::SLOTS,
                ring::SLOT_BYTES
            ),
            (64, 128, 192, 256, 288, 312, 320, 384, 48)
        );
        assert_eq!(
            SAMPLE_EVERY, 17,
            "the reader scales the account by seventeen"
        );
        assert_eq!(ring::SLOT_BYTES, 8 + RECORD_SIZE);
    }

    /// A timed call's own time is its span less the real function and the
    /// clock reads, and a span a preemption fell in is none of the hook's: it
    /// is left out, to be counted apart, not zeroed.
    #[test]
    fn a_preempted_span_is_left_out_of_the_account() {
        assert_eq!(own_time(1_000, 600, 20), Some(360));
        assert_eq!(own_time(500, 600, 20), Some(0), "never below zero");
        assert_eq!(own_time(PREEMPTED_NS + 640, 600, 20), Some(PREEMPTED_NS));
        assert_eq!(own_time(PREEMPTED_NS + 641, 600, 20), None);
        assert_eq!(
            own_time(4_000_000 + 1_000, 600, 20),
            None,
            "a scheduler slice inside the span is the machine's, not the shim's"
        );
    }
}
