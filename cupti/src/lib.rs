//! The CUDA injection library: what `CUDA_INJECTION64_PATH` loads.
//!
//! The CUDA driver loads the library this path names during `cuInit` and calls
//! its `InitializeInjection`, inside the program, before the program's first
//! context exists. That is the only place CUPTI's activity API works: it
//! records what the contexts of its own process do, so `gpu.kernel` serves a
//! program `omn` launches and refuses a running one with that reason.
//!
//! What it does, once armed (`OMN_CUPTI_REPORT`):
//!
//! - enables CUPTI's concurrent-kernel, memcpy and memset activity, and hands
//!   CUPTI buffers to fill -- at most [`wire::DEFAULT_BUFFERS`] of
//!   [`wire::DEFAULT_BUFFER_BYTES`] at once, so what the library holds is
//!   bounded. Past that bound CUPTI drops records, and counts them;
//! - drains them on a flush thread every [`wire::DEFAULT_FLUSH_MS`] ms, so a
//!   full buffer is handed back long before the bound is reached, and takes a
//!   calibration pair at each flush;
//! - turns every record into a frame on the channel to `omn`
//!   (`channel.rs`), with what CUPTI dropped and what could not be sent;
//! - at exit, forces a last flush and says so ([`wire::OP_END`]), so a run
//!   that ended without that frame knows its tail may be missing.
//!
//! **Never fail the program.** Every CUPTI call that fails is reported as an
//! [`wire::OP_ERROR`] frame and the library disarms; `InitializeInjection`
//! answers success whatever happened, because a failure there fails the
//! program's `cuInit`.
//!
//! **A forked child reports nothing, and exits as it would have.** A program
//! that forks after `cuInit` gives its child a copy of this library's state:
//! the `atexit` handler, the channel's batch and its lock, CUPTI's own state
//! as the parent's threads left it -- but not those threads. The child cannot
//! use CUDA (the driver does not survive a fork), so it has nothing of its own
//! to report, and running the closing flush there waits on locks no thread
//! will ever release: measured, a child forked after 500 kernels hung in
//! `exit` for good. So everything that runs at exit or sends asks first
//! whether this is still the process that initialised, and a child returns
//! at once ([`owner`]). A child that execs a CUDA program is a new process
//! the driver loads this library into afresh, under its own pid.
//!
//! **Disarmed costs one `dlopen`.** With `OMN_CUPTI_REPORT=0` the library
//! loads and does nothing else: the negative control a tracer's cost is
//! measured against, so the loader's own work is not billed to CUPTI.

mod api;
mod channel;
#[allow(dead_code)]
mod wire;

use std::ffi::{c_int, c_void, CStr};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use api::Cupti;
use channel::Channel;
use wire::cupti as c;

static CUPTI: OnceLock<Cupti> = OnceLock::new();
static CHANNEL: Mutex<Option<Channel>> = Mutex::new(None);
static BUFFER_BYTES: AtomicUsize = AtomicUsize::new(wire::DEFAULT_BUFFER_BYTES);
static BUFFERS: AtomicUsize = AtomicUsize::new(wire::DEFAULT_BUFFERS);
/// Buffers handed to CUPTI and not yet returned.
static OUTSTANDING: AtomicUsize = AtomicUsize::new(0);
static STOPPING: AtomicBool = AtomicBool::new(false);
static INITIALISED: AtomicBool = AtomicBool::new(false);
/// The process `InitializeInjection` ran in; zero before it has.
static OWNER: AtomicU32 = AtomicU32::new(0);
/// Records of a kind this library does not decode.
static UNKNOWN: AtomicUsize = AtomicUsize::new(0);
/// Records handed back before their activity finished: no time to report.
static INCOMPLETE: AtomicUsize = AtomicUsize::new(0);

/// Called by the CUDA driver inside `cuInit`. Answers 1, success, always.
#[no_mangle]
pub extern "C" fn InitializeInjection() -> c_int {
    if INITIALISED.swap(true, Ordering::SeqCst) {
        return 1;
    }
    OWNER.store(std::process::id(), Ordering::SeqCst);
    let Some(channel) = Channel::from_environment() else {
        return 1;
    };
    *lock() = Some(channel);
    let report: u32 = std::env::var(wire::REPORT_VARIABLE)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(wire::REPORT_ACTIVITY);
    if report & wire::REPORT_ACTIVITY == 0 {
        return 1;
    }
    if let Err(reason) = arm(report) {
        error(&reason);
        send_now();
    }
    1
}

/// Whether this is the process that initialised, rather than a child forked
/// from it, which holds a copy of the parent's state and none of its threads.
fn owner() -> bool {
    OWNER.load(Ordering::SeqCst) == std::process::id()
}

fn lock() -> std::sync::MutexGuard<'static, Option<Channel>> {
    CHANNEL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn with_channel(f: impl FnOnce(&mut Channel)) {
    if let Some(channel) = lock().as_mut() {
        f(channel);
    }
}

fn error(reason: &str) {
    with_channel(|channel| channel.frame(wire::OP_ERROR, reason.as_bytes()));
}

fn send_now() {
    with_channel(Channel::send);
}

fn setting(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn arm(report: u32) -> Result<(), String> {
    let cupti = Cupti::open()?;
    let cupti = CUPTI.get_or_init(|| cupti);
    BUFFER_BYTES.store(
        setting(wire::BUFFER_BYTES_VARIABLE, wire::DEFAULT_BUFFER_BYTES).max(4096),
        Ordering::Relaxed,
    );
    BUFFERS.store(
        setting(wire::BUFFERS_VARIABLE, wire::DEFAULT_BUFFERS),
        Ordering::Relaxed,
    );
    let flush_ms = setting(wire::FLUSH_MS_VARIABLE, wire::DEFAULT_FLUSH_MS as usize) as u64;

    let mut version = 0u32;
    // SAFETY: CUPTI writes one u32 we own.
    unsafe { (cupti.version)(&mut version) };
    let mut hello = wire::Fields::<24>::default();
    hello
        .u32(version)
        .u32(report)
        .u64(BUFFER_BYTES.load(Ordering::Relaxed) as u64)
        .u64(BUFFERS.load(Ordering::Relaxed) as u64);
    with_channel(|channel| channel.frame(wire::OP_HELLO, hello.as_slice()));
    calibrate(cupti, 32);

    let check = |what: &str, code: api::Result| {
        if code == c::SUCCESS {
            Ok(())
        } else {
            Err(format!("{what} failed: {}", cupti.describe(code)))
        }
    };
    // SAFETY: registering two `extern "C"` functions with the signatures
    // CUPTI's header declares.
    check("cuptiActivityRegisterCallbacks", unsafe {
        (cupti.register_callbacks)(request_buffer, buffer_complete)
    })?;
    for (kind, name) in [
        (c::KIND_CONCURRENT_KERNEL, "CONCURRENT_KERNEL"),
        (c::KIND_MEMCPY, "MEMCPY"),
        (c::KIND_MEMSET, "MEMSET"),
    ] {
        // SAFETY: enabling an activity kind by its documented number.
        check(&format!("cuptiActivityEnable({name})"), unsafe {
            (cupti.enable)(kind)
        })?;
    }
    send_now();

    std::thread::Builder::new()
        .name("omn-cupti-flush".to_owned())
        .spawn(move || flush_loop(flush_ms))
        .map_err(|error| format!("cannot start the flush thread: {error}"))?;
    // SAFETY: registering a function with C linkage that takes nothing.
    unsafe { libc::atexit(at_exit) };
    Ok(())
}

/// Takes the tightest of `tries` (monotonic, CUPTI, monotonic) brackets and
/// sends it.
fn calibrate(cupti: &Cupti, tries: usize) {
    let mut best: Option<(u64, u64, u64)> = None;
    for _ in 0..tries {
        let before = monotonic_ns();
        let Some(cupti_ns) = cupti.now() else {
            return;
        };
        let after = monotonic_ns();
        if best.is_none_or(|(b, _, a)| after - before < a - b) {
            best = Some((before, cupti_ns, after));
        }
    }
    if let Some((before, cupti_ns, after)) = best {
        let mut pair = wire::Fields::<24>::default();
        pair.u64(before).u64(cupti_ns).u64(after);
        with_channel(|channel| channel.frame(wire::OP_CALIB, pair.as_slice()));
    }
}

fn monotonic_ns() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: writing a timespec we own.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) };
    time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64
}

fn flush_loop(period_ms: u64) {
    let Some(cupti) = CUPTI.get() else {
        return;
    };
    while !STOPPING.load(Ordering::Acquire) {
        std::thread::sleep(std::time::Duration::from_millis(period_ms));
        if STOPPING.load(Ordering::Acquire) {
            break;
        }
        calibrate(cupti, 4);
        // Never forced here. A forced flush hands back the buffer CUPTI is
        // still filling, records of kernels that have not finished included,
        // with their timestamps zero -- and those records are never
        // delivered again: measured, a forced flush every 50 ms under 500
        // launches of a 2 ms kernel returned 482 of them without a time. So
        // a buffer comes back when it is full, and the rest at exit, when
        // the program's own synchronisation has completed them.
        // SAFETY: CUPTI's flush; buffers come back through `buffer_complete`.
        unsafe { (cupti.flush_all)(0) };
        send_now();
    }
}

extern "C" fn at_exit() {
    // Before any lock or CUPTI call: in a forked child both may be held by
    // threads that were not copied, and waiting on them never returns.
    if !owner() {
        return;
    }
    STOPPING.store(true, Ordering::Release);
    let Some(cupti) = CUPTI.get() else {
        return;
    };
    // The bracket first, so that it reaches `omn` before the records the
    // flush hands back, which all precede it: they are converted between
    // brackets rather than extrapolated past the last one.
    calibrate(cupti, 32);
    // SAFETY: a forced flush hands back every buffer, complete or not.
    unsafe { (cupti.flush_all)(c::FLUSH_FORCED) };
    with_channel(|channel| {
        let mut end = wire::Fields::<24>::default();
        end.u64(channel.seen)
            .u64(channel.lost)
            .u64(UNKNOWN.load(Ordering::Relaxed) as u64);
        channel.frame(wire::OP_END, end.as_slice());
        let incomplete = INCOMPLETE.swap(0, Ordering::Relaxed) as u64;
        if incomplete > 0 {
            channel.frame(wire::OP_INCOMPLETE, &incomplete.to_ne_bytes());
        }
        channel.send();
    });
}

extern "C" fn request_buffer(buffer: *mut *mut u8, size: *mut usize, max_records: *mut usize) {
    let bytes = BUFFER_BYTES.load(Ordering::Relaxed);
    let mut granted: *mut u8 = std::ptr::null_mut();
    if OUTSTANDING.fetch_add(1, Ordering::AcqRel) < BUFFERS.load(Ordering::Relaxed) {
        let mut memory: *mut c_void = std::ptr::null_mut();
        // SAFETY: an 8-byte aligned allocation, as CUPTI requires, freed in
        // `buffer_complete`.
        if unsafe { libc::posix_memalign(&mut memory, 8, bytes) } == 0 {
            granted = memory.cast();
        }
    }
    if granted.is_null() {
        // No buffer: CUPTI drops what it cannot store, and counts it.
        OUTSTANDING.fetch_sub(1, Ordering::AcqRel);
    }
    // SAFETY: CUPTI passes three pointers to write through.
    unsafe {
        *buffer = granted;
        *size = if granted.is_null() { 0 } else { bytes };
        *max_records = 0;
    }
}

extern "C" fn buffer_complete(
    context: *mut c_void,
    stream: u32,
    buffer: *mut u8,
    _size: usize,
    valid: usize,
) {
    let Some(cupti) = CUPTI.get() else {
        return;
    };
    let mut guard = lock();
    if let Some(channel) = guard.as_mut() {
        if !buffer.is_null() && valid > 0 {
            let mut record: *mut u8 = std::ptr::null_mut();
            // SAFETY: walking the records of a buffer CUPTI returned; each
            // one is valid until the buffer is freed below.
            while unsafe { (cupti.next_record)(buffer, valid, &mut record) } == c::SUCCESS {
                // SAFETY: as above; `decode` reads within the record's
                // common prefix.
                unsafe { decode(channel, record) };
            }
        }
        let incomplete = INCOMPLETE.swap(0, Ordering::Relaxed) as u64;
        if incomplete > 0 {
            channel.frame(wire::OP_INCOMPLETE, &incomplete.to_ne_bytes());
        }
        let mut dropped = 0usize;
        // SAFETY: CUPTI writes one size_t we own.
        let asked = unsafe { (cupti.dropped)(context, stream, &mut dropped) };
        if asked == c::SUCCESS && dropped > 0 {
            let mut fields = wire::Fields::<16>::default();
            fields.u64(dropped as u64).u32(stream).u32(0);
            channel.frame(wire::OP_DROPPED, fields.as_slice());
        }
    }
    drop(guard);
    if !buffer.is_null() {
        // SAFETY: the buffer came from `posix_memalign` in `request_buffer`.
        unsafe { libc::free(buffer.cast()) };
        OUTSTANDING.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Whether a record carries both of its times: one handed back before its
/// activity finished carries zeros, and is counted rather than reported.
fn complete(start: u64, end: u64) -> bool {
    start != 0 && end >= start
}

/// Reads a field of a record at a byte offset.
///
/// # Safety
/// `record` points at a record at least `offset + size_of::<T>()` long.
unsafe fn field<T: Copy>(record: *const u8, offset: usize) -> T {
    std::ptr::read_unaligned(record.add(offset).cast::<T>())
}

/// # Safety
/// `record` is a record CUPTI returned, valid for its kind's common prefix.
unsafe fn decode(channel: &mut Channel, record: *const u8) {
    let kind: u32 = field(record, 0);
    match kind {
        c::KIND_CONCURRENT_KERNEL | c::KIND_KERNEL => {
            use c::kernel as k;
            if !complete(field(record, k::START), field(record, k::END)) {
                INCOMPLETE.fetch_add(1, Ordering::Relaxed);
                return;
            }
            let name: *const std::ffi::c_char = field(record, k::NAME);
            let name_id = if name.is_null() {
                0
            } else {
                channel.name(CStr::from_ptr(name).to_bytes())
            };
            let static_shared: i32 = field(record, k::STATIC_SHARED);
            let dynamic_shared: i32 = field(record, k::DYNAMIC_SHARED);
            let kernel = wire::Kernel {
                start: field(record, k::START),
                end: field(record, k::END),
                device: field(record, k::DEVICE),
                context: field(record, k::CONTEXT),
                stream: field(record, k::STREAM),
                correlation: field(record, k::CORRELATION),
                grid: [
                    field(record, k::GRID),
                    field(record, k::GRID + 4),
                    field(record, k::GRID + 8),
                ],
                block: [
                    field(record, k::BLOCK),
                    field(record, k::BLOCK + 4),
                    field(record, k::BLOCK + 8),
                ],
                name: name_id,
                registers: u32::from(field::<u16>(record, k::REGISTERS)),
                shared: (static_shared.max(0) + dynamic_shared.max(0)) as u32,
            };
            channel.frame(wire::OP_KERNEL, kernel.encode().as_slice());
        }
        c::KIND_MEMCPY | c::KIND_MEMSET => {
            use c::copy as m;
            if !complete(field(record, m::START), field(record, m::END)) {
                INCOMPLETE.fetch_add(1, Ordering::Relaxed);
                return;
            }
            let detail = if kind == c::KIND_MEMCPY {
                u32::from(field::<u8>(record, m::COPY_KIND))
            } else {
                field(record, m::MEMSET_VALUE)
            };
            let copy = wire::Copy {
                start: field(record, m::START),
                end: field(record, m::END),
                bytes: field(record, m::BYTES),
                device: field(record, m::DEVICE),
                context: field(record, m::CONTEXT),
                stream: field(record, m::STREAM),
                correlation: field(record, m::CORRELATION),
                detail,
            };
            let op = if kind == c::KIND_MEMCPY {
                wire::OP_MEMCPY
            } else {
                wire::OP_MEMSET
            };
            channel.frame(op, copy.encode().as_slice());
        }
        other => {
            UNKNOWN.fetch_add(1, Ordering::Relaxed);
            channel.frame(wire::OP_UNKNOWN, &other.to_ne_bytes());
        }
    }
}
