//! `pthread_mutex_lock` and its neighbours, for `sched.mutex`.
//!
//! One record per acquisition, written when the mutex is **released**: that is
//! the first moment both halves are known -- how long the thread waited to get
//! the mutex, and how long it then held it. Reporting at acquisition and again
//! at release would double the records for a hook that fires far more often
//! than an allocation, and the reader would have to pair them again.
//!
//! The record is written *after* the real unlock returns. Writing it inside
//! the critical section would lengthen every hold by one `write(2)` and make
//! every waiter wait for the measurement.
//!
//! What the thread holds lives in a small per-thread stack, not in a map: it
//! is touched on every lock and unlock, must not allocate, and a thread holds
//! a handful of mutexes at once. A thread that nests deeper than the stack has
//! its extra acquisitions reported at once with the hold marked unseen, rather
//! than forgotten.
//!
//! **What is not an acquisition here.** Glibc's own internal locks (stdio,
//! the allocator, the loader) do not go through the exported symbol and are
//! invisible, which is what the caller wants. A condition variable reacquires
//! its mutex inside `pthread_cond_wait`, where the time spent waiting for the
//! condition and the time spent waiting for the mutex cannot be told apart:
//! the hold that led into the wait is reported, ending there, and the
//! reacquisition is not reported at all. A failed `pthread_mutex_trylock` is a
//! sign of contention but not a wait, and is not reported either.

use std::cell::Cell;
use std::ffi::{c_int, c_void};
use std::sync::atomic::AtomicUsize;

use crate::{
    guarded, in_hook, monotonic_ns, report, reporting, resolve, thread_id, Record, Timer,
    CLASS_MUTEX, OP_MUTEX,
};

/// The mutex was taken by `pthread_mutex_trylock`, so its wait is zero by
/// construction rather than measured.
pub const FLAG_TRY: u8 = 1;
/// The hold ended in `pthread_cond_wait` or `pthread_cond_timedwait` rather
/// than at an unlock.
pub const FLAG_COND: u8 = 2;
/// The thread held more mutexes than the stack keeps, so the hold was never
/// timed: `extra` is zero and means nothing.
pub const FLAG_HOLD_UNSEEN: u8 = 4;

/// Mutexes one thread can hold at once and still have their holds timed.
const DEPTH: usize = 8;

#[derive(Clone, Copy)]
struct Held {
    mutex: usize,
    asked_ns: u64,
    acquired_ns: u64,
    flags: u8,
}

const EMPTY: Held = Held {
    mutex: 0,
    asked_ns: 0,
    acquired_ns: 0,
    flags: 0,
};

thread_local! {
    /// Const-initialized and `Copy`, like `IN_HOOK`: touching it never
    /// allocates and never registers a destructor.
    static HELD: [Cell<Held>; DEPTH] = const { [const { Cell::new(EMPTY) }; DEPTH] };
    static HELD_COUNT: Cell<usize> = const { Cell::new(0) };
}

static REAL_LOCK: AtomicUsize = AtomicUsize::new(0);
static REAL_TRYLOCK: AtomicUsize = AtomicUsize::new(0);
static REAL_UNLOCK: AtomicUsize = AtomicUsize::new(0);
static REAL_COND_WAIT: AtomicUsize = AtomicUsize::new(0);
static REAL_COND_TIMEDWAIT: AtomicUsize = AtomicUsize::new(0);

fn watching() -> bool {
    reporting(CLASS_MUTEX) && !in_hook()
}

fn emit(mutex: usize, held: Held, released_ns: u64) {
    let hold = if held.flags & FLAG_HOLD_UNSEEN != 0 {
        0
    } else {
        released_ns.saturating_sub(held.acquired_ns)
    };
    guarded(|| {
        report(
            CLASS_MUTEX,
            Record {
                ts_ns: held.asked_ns,
                ptr: mutex as u64,
                size: held.acquired_ns.saturating_sub(held.asked_ns),
                extra: hold,
                tid: thread_id(),
                op: OP_MUTEX,
                flags: held.flags,
                padding: [0; 2],
            },
        )
    });
}

/// Remembers an acquisition until its release, or reports it now when the
/// stack is full.
fn push(mutex: usize, asked_ns: u64, acquired_ns: u64, flags: u8) {
    let held = Held {
        mutex,
        asked_ns,
        acquired_ns,
        flags,
    };
    let count = HELD_COUNT.with(Cell::get);
    if count >= DEPTH {
        emit(
            mutex,
            Held {
                flags: flags | FLAG_HOLD_UNSEEN,
                ..held
            },
            acquired_ns,
        );
        return;
    }
    HELD.with(|stack| stack[count].set(held));
    HELD_COUNT.with(|slot| slot.set(count + 1));
}

/// Takes the most recent acquisition of `mutex` off the stack. Mutexes are
/// not always released in the order they were taken, so the entries above it
/// move down.
fn take(mutex: usize) -> Option<Held> {
    let count = HELD_COUNT.with(Cell::get);
    HELD.with(|stack| {
        let index = (0..count)
            .rev()
            .find(|&index| stack[index].get().mutex == mutex)?;
        let held = stack[index].get();
        for above in index + 1..count {
            stack[above - 1].set(stack[above].get());
        }
        HELD_COUNT.with(|slot| slot.set(count - 1));
        Some(held)
    })
}

type Lock = unsafe extern "C" fn(*mut c_void) -> c_int;

/// Resolves a real entry point, or answers `EINVAL` as the one error every one
/// of these may return: a program whose libc has no `pthread_mutex_lock` did
/// not link against one, and cannot be calling this.
fn real(slot: &AtomicUsize, name: &[u8]) -> Option<usize> {
    match resolve(slot, name) {
        0 => None,
        address => Some(address),
    }
}

/// # Safety
/// Replaces libc's `pthread_mutex_lock` and obeys its contract.
#[no_mangle]
pub unsafe extern "C" fn pthread_mutex_lock(mutex: *mut c_void) -> c_int {
    let mut timer = Timer::start(CLASS_MUTEX);
    let Some(address) = real(&REAL_LOCK, b"pthread_mutex_lock\0") else {
        return libc::EINVAL;
    };
    let lock: Lock = std::mem::transmute(address);
    if !watching() {
        return lock(mutex);
    }
    let asked = monotonic_ns();
    let result = timer.around(|| lock(mutex));
    if result == 0 {
        push(mutex as usize, asked, monotonic_ns(), 0);
    }
    result
}

/// # Safety
/// Replaces libc's `pthread_mutex_trylock` and obeys its contract.
#[no_mangle]
pub unsafe extern "C" fn pthread_mutex_trylock(mutex: *mut c_void) -> c_int {
    let mut timer = Timer::start(CLASS_MUTEX);
    let Some(address) = real(&REAL_TRYLOCK, b"pthread_mutex_trylock\0") else {
        return libc::EINVAL;
    };
    let trylock: Lock = std::mem::transmute(address);
    if !watching() {
        return trylock(mutex);
    }
    let result = timer.around(|| trylock(mutex));
    if result == 0 {
        let now = monotonic_ns();
        push(mutex as usize, now, now, FLAG_TRY);
    }
    result
}

/// # Safety
/// Replaces libc's `pthread_mutex_unlock` and obeys its contract.
#[no_mangle]
pub unsafe extern "C" fn pthread_mutex_unlock(mutex: *mut c_void) -> c_int {
    let mut timer = Timer::start(CLASS_MUTEX);
    let Some(address) = real(&REAL_UNLOCK, b"pthread_mutex_unlock\0") else {
        return libc::EINVAL;
    };
    let unlock: Lock = std::mem::transmute(address);
    if !watching() {
        return unlock(mutex);
    }
    let held = take(mutex as usize);
    let released = monotonic_ns();
    let result = timer.around(|| unlock(mutex));
    if let Some(held) = held {
        emit(mutex as usize, held, released);
    }
    result
}

/// The hold that led into a condition wait ends where the wait begins.
fn entering_condition_wait(mutex: *mut c_void) {
    if !watching() {
        return;
    }
    if let Some(held) = take(mutex as usize) {
        emit(
            mutex as usize,
            Held {
                flags: held.flags | FLAG_COND,
                ..held
            },
            monotonic_ns(),
        );
    }
}

/// # Safety
/// Replaces libc's `pthread_cond_wait` and obeys its contract.
#[no_mangle]
pub unsafe extern "C" fn pthread_cond_wait(cond: *mut c_void, mutex: *mut c_void) -> c_int {
    let Some(address) = real(&REAL_COND_WAIT, b"pthread_cond_wait\0") else {
        return libc::EINVAL;
    };
    let wait: unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int =
        std::mem::transmute(address);
    entering_condition_wait(mutex);
    wait(cond, mutex)
}

/// # Safety
/// Replaces libc's `pthread_cond_timedwait` and obeys its contract.
#[no_mangle]
pub unsafe extern "C" fn pthread_cond_timedwait(
    cond: *mut c_void,
    mutex: *mut c_void,
    deadline: *const c_void,
) -> c_int {
    let Some(address) = real(&REAL_COND_TIMEDWAIT, b"pthread_cond_timedwait\0") else {
        return libc::EINVAL;
    };
    let wait: unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_void) -> c_int =
        std::mem::transmute(address);
    entering_condition_wait(mutex);
    wait(cond, mutex, deadline)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clear() {
        while HELD_COUNT.with(Cell::get) > 0 {
            let top = HELD.with(|stack| stack[HELD_COUNT.with(Cell::get) - 1].get());
            take(top.mutex);
        }
    }

    #[test]
    fn a_release_takes_the_acquisition_of_that_mutex_and_leaves_the_others() {
        clear();
        push(0xa, 1, 2, 0);
        push(0xb, 3, 4, 0);
        push(0xc, 5, 6, 0);
        // Released out of order: the middle one.
        let held = take(0xb).expect("0xb is held");
        assert_eq!((held.asked_ns, held.acquired_ns), (3, 4));
        assert_eq!(take(0xc).map(|held| held.asked_ns), Some(5));
        assert_eq!(take(0xa).map(|held| held.asked_ns), Some(1));
        assert!(take(0xa).is_none(), "released twice");
    }

    /// A recursive mutex is taken twice by one thread; the inner release is
    /// the inner acquisition's.
    #[test]
    fn a_recursive_acquisition_is_released_innermost_first() {
        clear();
        push(0xa, 1, 2, 0);
        push(0xa, 10, 11, 0);
        assert_eq!(take(0xa).map(|held| held.asked_ns), Some(10));
        assert_eq!(take(0xa).map(|held| held.asked_ns), Some(1));
    }

    #[test]
    fn an_unlock_of_what_this_thread_never_locked_finds_nothing() {
        clear();
        assert!(take(0xdead).is_none());
    }

    #[test]
    fn the_flags_are_the_ones_the_reader_expects() {
        assert_eq!((FLAG_TRY, FLAG_COND, FLAG_HOLD_UNSEEN), (1, 2, 4));
    }
}
