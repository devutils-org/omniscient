//! The heap shim: what `LD_PRELOAD` sees.
//!
//! This library replaces `malloc` and friends in the observed program and
//! reports every allocation and free to `omn` through a pipe. It is preferred
//! over a uprobe on `malloc` for one reason: a uprobe costs one to two
//! microseconds per call, which on an allocation-heavy program is not an
//! observation but a different program. A shim costs tens of nanoseconds.
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

use std::cell::Cell;
use std::ffi::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};

/// Environment variable naming the file descriptor to report on.
const FD_VARIABLE: &[u8] = b"OMN_HEAP_FD\0";

const OP_ALLOC: u8 = 1;
const OP_FREE: u8 = 2;
/// Records the shim had to throw away, so the loss is in the data.
const OP_DROPPED: u8 = 3;

/// One reported event. Fixed size and native endian: both sides are the same
/// build on the same machine, and a length prefix would cost a branch in the
/// hot path for nothing.
#[repr(C)]
#[derive(Clone, Copy)]
struct Record {
    ts_ns: u64,
    ptr: u64,
    size: u64,
    tid: u32,
    op: u8,
    padding: [u8; 3],
}

const RECORD_SIZE: usize = std::mem::size_of::<Record>();

static REPORT_FD: AtomicI32 = AtomicI32::new(-1);
static CONFIGURED: AtomicBool = AtomicBool::new(false);
static DROPPED: AtomicU64 = AtomicU64::new(0);

static REAL_MALLOC: AtomicUsize = AtomicUsize::new(0);
static REAL_CALLOC: AtomicUsize = AtomicUsize::new(0);
static REAL_REALLOC: AtomicUsize = AtomicUsize::new(0);
static REAL_FREE: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// Set while a hook is running, so anything the hook itself allocates goes
    /// straight to the real allocator and is not reported. Const-initialized
    /// and `Copy`, so touching it never allocates and never registers a
    /// destructor.
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
}

/// Allocations served before `dlsym` has produced the real allocator.
///
/// `dlsym` itself may allocate, so the first call into this shim cannot rely on
/// the real `malloc` existing yet. A bump region breaks the cycle; it is small
/// because only the dynamic loader's own bootstrap comes through it.
const BOOTSTRAP_CAPACITY: usize = 64 * 1024;
static mut BOOTSTRAP: [u8; BOOTSTRAP_CAPACITY] = [0; BOOTSTRAP_CAPACITY];
static BOOTSTRAP_USED: AtomicUsize = AtomicUsize::new(0);

fn bootstrap_alloc(size: usize) -> *mut c_void {
    // Sixteen-byte alignment is what malloc promises on every target here.
    let size = (size + 15) & !15;
    let offset = BOOTSTRAP_USED.fetch_add(size, Ordering::Relaxed);
    if offset + size > BOOTSTRAP_CAPACITY {
        return std::ptr::null_mut();
    }
    // SAFETY: the offset was reserved by the atomic above and never reused.
    unsafe { (&raw mut BOOTSTRAP).cast::<u8>().add(offset).cast() }
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
    // SAFETY: `name` is a NUL-terminated byte string.
    let address = unsafe { libc::dlsym(libc::RTLD_NEXT, name.as_ptr().cast::<c_char>()) } as usize;
    slot.store(address, Ordering::Relaxed);
    address
}

fn configure() {
    if CONFIGURED.swap(true, Ordering::Relaxed) {
        return;
    }
    // `getenv` does not allocate, which `std::env::var` does not promise.
    // SAFETY: `FD_VARIABLE` is NUL-terminated.
    let value = unsafe { libc::getenv(FD_VARIABLE.as_ptr().cast::<c_char>()) };
    if value.is_null() {
        return;
    }
    let mut fd: i32 = 0;
    let mut index = 0isize;
    loop {
        // SAFETY: walking a NUL-terminated string from the environment.
        let byte = unsafe { *value.offset(index) } as u8;
        if !byte.is_ascii_digit() {
            break;
        }
        fd = fd.saturating_mul(10).saturating_add((byte - b'0') as i32);
        index += 1;
    }
    if index > 0 {
        REPORT_FD.store(fd, Ordering::Relaxed);
    }
}

fn monotonic_ns() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `clock_gettime` fills a struct we own.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) };
    (time.tv_sec as u64) * 1_000_000_000 + (time.tv_nsec as u64)
}

fn thread_id() -> u32 {
    // SAFETY: `gettid` takes no argument and cannot fail.
    unsafe { libc::syscall(libc::SYS_gettid) as u32 }
}

/// Writes one record, dropping it rather than blocking the program.
fn report(op: u8, ptr: *mut c_void, size: u64) {
    configure();
    let fd = REPORT_FD.load(Ordering::Relaxed);
    if fd < 0 {
        return;
    }

    // A drop that is never reported is a drop nobody can account for, so the
    // backlog rides along with the next record that gets through.
    let dropped = DROPPED.swap(0, Ordering::Relaxed);
    if dropped > 0 && !write_record(fd, OP_DROPPED, std::ptr::null_mut(), dropped) {
        DROPPED.fetch_add(dropped, Ordering::Relaxed);
        return;
    }

    if !write_record(fd, op, ptr, size) {
        DROPPED.fetch_add(1, Ordering::Relaxed);
    }
}

fn write_record(fd: i32, op: u8, ptr: *mut c_void, size: u64) -> bool {
    let record = Record {
        ts_ns: monotonic_ns(),
        ptr: ptr as u64,
        size,
        tid: thread_id(),
        op,
        padding: [0; 3],
    };
    // SAFETY: writing `RECORD_SIZE` bytes from a struct of exactly that size.
    // A write this small is atomic on a pipe, so records never interleave.
    let written = unsafe { libc::write(fd, (&raw const record).cast::<c_void>(), RECORD_SIZE) };
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
    let real = resolve(&REAL_MALLOC, b"malloc\0");
    if real == 0 {
        return bootstrap_alloc(size);
    }
    let real: extern "C" fn(usize) -> *mut c_void = std::mem::transmute(real);
    let pointer = real(size);
    if !pointer.is_null() {
        guarded(|| report(OP_ALLOC, pointer, size as u64));
    }
    pointer
}

/// # Safety
/// Replaces libc's `calloc` and obeys its contract.
#[no_mangle]
pub unsafe extern "C" fn calloc(count: usize, size: usize) -> *mut c_void {
    let real = resolve(&REAL_CALLOC, b"calloc\0");
    if real == 0 {
        // The bump region is zeroed and never reused, so it already satisfies
        // calloc's contract.
        return bootstrap_alloc(count.saturating_mul(size));
    }
    let real: extern "C" fn(usize, usize) -> *mut c_void = std::mem::transmute(real);
    let pointer = real(count, size);
    if !pointer.is_null() {
        guarded(|| report(OP_ALLOC, pointer, count.saturating_mul(size) as u64));
    }
    pointer
}

/// # Safety
/// Replaces libc's `realloc` and obeys its contract.
#[no_mangle]
pub unsafe extern "C" fn realloc(pointer: *mut c_void, size: usize) -> *mut c_void {
    let real = resolve(&REAL_REALLOC, b"realloc\0");
    if real == 0 || is_bootstrap(pointer) {
        let fresh = bootstrap_alloc(size);
        if !pointer.is_null() && !fresh.is_null() {
            std::ptr::copy_nonoverlapping(pointer.cast::<u8>(), fresh.cast::<u8>(), size);
        }
        return fresh;
    }
    let real: extern "C" fn(*mut c_void, usize) -> *mut c_void = std::mem::transmute(real);
    let fresh = real(pointer, size);
    // A realloc is a free and an allocation, and reporting it as both is what
    // keeps the live set correct without the reader needing a special case.
    guarded(|| {
        if !pointer.is_null() {
            report(OP_FREE, pointer, 0);
        }
        if !fresh.is_null() {
            report(OP_ALLOC, fresh, size as u64);
        }
    });
    fresh
}

/// # Safety
/// Replaces libc's `free` and obeys its contract.
#[no_mangle]
pub unsafe extern "C" fn free(pointer: *mut c_void) {
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
    guarded(|| report(OP_FREE, pointer, 0));
    let real: extern "C" fn(*mut c_void) = std::mem::transmute(real);
    real(pointer);
}

/// # Safety
/// Replaces libc's `posix_memalign` and obeys its contract.
#[no_mangle]
pub unsafe extern "C" fn posix_memalign(
    out: *mut *mut c_void,
    alignment: usize,
    size: usize,
) -> c_int {
    let real = libc::dlsym(
        libc::RTLD_NEXT,
        b"posix_memalign\0".as_ptr().cast::<c_char>(),
    );
    if real.is_null() {
        return libc::ENOMEM;
    }
    let real: extern "C" fn(*mut *mut c_void, usize, usize) -> c_int = std::mem::transmute(real);
    let result = real(out, alignment, size);
    if result == 0 && !out.is_null() {
        let pointer = *out;
        guarded(|| report(OP_ALLOC, pointer, size as u64));
    }
    result
}
