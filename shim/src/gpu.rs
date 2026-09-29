//! The host side of GPU submission, for `gpu.api`.
//!
//! Each wrapper times the call it forwards and reports how long the calling
//! thread spent inside it, what it returned, and which entry point it was.
//! That is the host's view and nothing more: an asynchronous launch returns
//! long before the device runs anything, and a synchronize is where that time
//! is paid.
//!
//! **Which calls this can see is decided by how the program reached them**,
//! and it is the most important thing to know about this source. `LD_PRELOAD`
//! wins the dynamic linker's global lookup, so it sees a call bound through a
//! program's own relocations: a C program linked with `-lcuda` or `-lvulkan`.
//! It does not see a call through a pointer the program fetched for itself --
//! `dlopen` and `dlsym` on a handle (Python's `ctypes`), `cuGetProcAddress`
//! (which the CUDA *runtime* uses for every driver call it makes), or
//! `vkGetDeviceProcAddr` (which most Vulkan engines use for speed). Those
//! programs report nothing here, and the reader says so rather than reporting
//! an idle GPU.
//!
//! When an entry point is interposed and no library behind the shim provides
//! it, the wrapper answers the API's own "not initialized" error rather than
//! jumping to address zero: a program that probes for the symbol finds it, and
//! must then be told the driver is not there.

use std::ffi::{c_int, c_uint, c_void};
use std::sync::atomic::AtomicUsize;

use crate::{
    guarded, in_hook, monotonic_ns, report, reporting, resolve, thread_id, Record, Timer,
    CLASS_GPU, OP_GPU_CALL,
};

/// `CUDA_ERROR_NOT_INITIALIZED`.
const CUDA_NOT_INITIALIZED: c_int = 3;
/// `VK_ERROR_INITIALIZATION_FAILED`.
const VK_INITIALIZATION_FAILED: i32 = -3;

fn watching() -> bool {
    reporting(CLASS_GPU) && !in_hook()
}

fn emit(index: usize, entered_ns: u64, left_ns: u64, result: i64) {
    guarded(|| {
        report(
            CLASS_GPU,
            Record {
                ts_ns: entered_ns,
                ptr: index as u64,
                size: left_ns.saturating_sub(entered_ns),
                extra: result as u64,
                tid: thread_id(),
                op: OP_GPU_CALL,
                flags: 0,
                padding: [0; 2],
            },
        )
    });
}

/// Declares the interposed entry points, in the order their index is sent.
///
/// The index is the agreement with the reader in `src/collect/gpu/api.rs`,
/// which carries the same list; both sides test it, as they test the record
/// layout.
macro_rules! entry_points {
    ($( $index:literal $name:ident ( $($arg:ident : $ty:ty),* ) -> $ret:ty = $missing:expr; )*) => {
        /// Every interposed entry point, by the index its records carry. Only
        /// the tests read it; the wrappers carry their index as a literal.
        #[cfg(test)]
        const ENTRY_POINTS: &[&str] = &[$(stringify!($name)),*];

        $(
            /// # Safety
            /// Forwards to the next definition of this entry point and obeys
            /// its contract.
            #[no_mangle]
            pub unsafe extern "C" fn $name($($arg: $ty),*) -> $ret {
                static REAL: AtomicUsize = AtomicUsize::new(0);
                let mut timer = Timer::start(CLASS_GPU);
                let address = resolve(&REAL, concat!(stringify!($name), "\0").as_bytes());
                if address == 0 {
                    return $missing;
                }
                let real: unsafe extern "C" fn($($ty),*) -> $ret = std::mem::transmute(address);
                if !watching() {
                    return real($($arg),*);
                }
                let entered = monotonic_ns();
                let result = timer.around(|| real($($arg),*));
                emit($index, entered, monotonic_ns(), i64::from(result));
                result
            }
        )*
    };
}

entry_points! {
    // The CUDA driver API, under the names `cuda.h` binds them to: the `_v2`
    // suffixes are what a program compiled against it actually references.
    0 cuLaunchKernel(
        function: *mut c_void,
        grid_x: c_uint, grid_y: c_uint, grid_z: c_uint,
        block_x: c_uint, block_y: c_uint, block_z: c_uint,
        shared_bytes: c_uint, stream: *mut c_void,
        params: *mut *mut c_void, extra: *mut *mut c_void
    ) -> c_int = CUDA_NOT_INITIALIZED;
    1 cuMemcpyHtoD_v2(device: u64, host: *const c_void, bytes: usize) -> c_int = CUDA_NOT_INITIALIZED;
    2 cuMemcpyDtoH_v2(host: *mut c_void, device: u64, bytes: usize) -> c_int = CUDA_NOT_INITIALIZED;
    3 cuMemcpyHtoDAsync_v2(device: u64, host: *const c_void, bytes: usize, stream: *mut c_void) -> c_int = CUDA_NOT_INITIALIZED;
    4 cuMemcpyDtoHAsync_v2(host: *mut c_void, device: u64, bytes: usize, stream: *mut c_void) -> c_int = CUDA_NOT_INITIALIZED;
    5 cuCtxSynchronize() -> c_int = CUDA_NOT_INITIALIZED;
    6 cuStreamSynchronize(stream: *mut c_void) -> c_int = CUDA_NOT_INITIALIZED;
    7 cuEventSynchronize(event: *mut c_void) -> c_int = CUDA_NOT_INITIALIZED;
    8 cuMemAlloc_v2(device: *mut u64, bytes: usize) -> c_int = CUDA_NOT_INITIALIZED;
    9 cuMemFree_v2(device: u64) -> c_int = CUDA_NOT_INITIALIZED;
    // Vulkan, through the loader's exported trampolines. Dispatchable handles
    // are pointers; fences are 64-bit non-dispatchable handles.
    10 vkQueueSubmit(queue: *mut c_void, count: u32, submits: *const c_void, fence: u64) -> i32 = VK_INITIALIZATION_FAILED;
    11 vkQueueSubmit2(queue: *mut c_void, count: u32, submits: *const c_void, fence: u64) -> i32 = VK_INITIALIZATION_FAILED;
    12 vkQueueWaitIdle(queue: *mut c_void) -> i32 = VK_INITIALIZATION_FAILED;
    13 vkDeviceWaitIdle(device: *mut c_void) -> i32 = VK_INITIALIZATION_FAILED;
    14 vkWaitForFences(device: *mut c_void, count: u32, fences: *const u64, all: u32, timeout: u64) -> i32 = VK_INITIALIZATION_FAILED;
    15 vkQueuePresentKHR(queue: *mut c_void, info: *const c_void) -> i32 = VK_INITIALIZATION_FAILED;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reader in `src/collect/gpu/api.rs` names an entry point by its
    /// index; a reordering here would rename every call silently.
    #[test]
    fn the_entry_points_are_in_the_order_the_reader_names_them() {
        assert_eq!(
            ENTRY_POINTS,
            &[
                "cuLaunchKernel",
                "cuMemcpyHtoD_v2",
                "cuMemcpyDtoH_v2",
                "cuMemcpyHtoDAsync_v2",
                "cuMemcpyDtoHAsync_v2",
                "cuCtxSynchronize",
                "cuStreamSynchronize",
                "cuEventSynchronize",
                "cuMemAlloc_v2",
                "cuMemFree_v2",
                "vkQueueSubmit",
                "vkQueueSubmit2",
                "vkQueueWaitIdle",
                "vkDeviceWaitIdle",
                "vkWaitForFences",
                "vkQueuePresentKHR",
            ]
        );
    }

    /// With no driver behind the shim, which is how its own test binary runs,
    /// an interposed entry point says the driver is not there rather than
    /// jumping to address zero.
    #[test]
    fn an_entry_point_with_nothing_behind_it_answers_not_initialized() {
        // SAFETY: the wrapper forwards nothing when the next definition is
        // missing, which is the case under test.
        let (cuda, vulkan) = unsafe { (cuCtxSynchronize(), vkQueueWaitIdle(std::ptr::null_mut())) };
        assert_eq!(cuda, CUDA_NOT_INITIALIZED);
        assert_eq!(vulkan, VK_INITIALIZATION_FAILED);
    }
}
