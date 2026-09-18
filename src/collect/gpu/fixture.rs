//! A known amount of video memory, allocated on purpose.
//!
//! A test that checks a collector against its own arithmetic proves nothing, so
//! the VRAM collector is checked against a quantity this fixture chose: it
//! creates a CUDA context, allocates N bytes, and the collector has to report
//! N. The allocation goes through the driver API loaded at runtime, so no
//! toolkit, no compiler and no linked SDK are needed -- and a machine without
//! the driver skips the test with a reason instead of failing it.

use std::ffi::{c_int, c_uint, c_void};

use super::nvml::{cast, dl_error, lookup};

type FnInit = unsafe extern "C" fn(c_uint) -> c_int;
type FnDeviceGet = unsafe extern "C" fn(*mut c_int, c_int) -> c_int;
type FnCtxCreate = unsafe extern "C" fn(*mut *mut c_void, c_uint, c_int) -> c_int;
type FnMemAlloc = unsafe extern "C" fn(*mut u64, usize) -> c_int;
type FnMemFree = unsafe extern "C" fn(u64) -> c_int;
type FnCtxDestroy = unsafe extern "C" fn(*mut c_void) -> c_int;

/// A CUDA context with one allocation in it.
pub struct CudaArena {
    library: *mut c_void,
    context: *mut c_void,
    pointer: Option<u64>,
    mem_alloc: FnMemAlloc,
    mem_free: FnMemFree,
    ctx_destroy: FnCtxDestroy,
}

impl CudaArena {
    /// Opens a context on device 0, or says why it could not.
    pub fn open() -> Result<Self, String> {
        let name = std::ffi::CString::new("libcuda.so.1").unwrap();
        // SAFETY: the name is a valid nul-terminated string; the handle is kept
        // by the returned value and closed in `Drop`.
        let library = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if library.is_null() {
            return Err(format!("libcuda.so.1 is not loadable here: {}", dl_error()));
        }

        // SAFETY: every symbol is looked up in this library under the name of
        // the driver API entry point whose signature the type mirrors.
        unsafe {
            let init: FnInit = cast(lookup(library, "cuInit"));
            let device_get: FnDeviceGet = cast(lookup(library, "cuDeviceGet"));
            let ctx_create: FnCtxCreate = cast(lookup(library, "cuCtxCreate_v2"));
            let mem_alloc: FnMemAlloc = cast(lookup(library, "cuMemAlloc_v2"));
            let mem_free: FnMemFree = cast(lookup(library, "cuMemFree_v2"));
            let ctx_destroy: FnCtxDestroy = cast(lookup(library, "cuCtxDestroy_v2"));

            let code = init(0);
            if code != 0 {
                libc::dlclose(library);
                return Err(format!("cuInit refused with code {code}"));
            }
            let mut device: c_int = 0;
            let code = device_get(&mut device, 0);
            if code != 0 {
                libc::dlclose(library);
                return Err(format!("cuDeviceGet refused with code {code}"));
            }
            let mut context: *mut c_void = std::ptr::null_mut();
            let code = ctx_create(&mut context, 0, device);
            if code != 0 {
                libc::dlclose(library);
                return Err(format!("cuCtxCreate refused with code {code}"));
            }
            Ok(Self {
                library,
                context,
                pointer: None,
                mem_alloc,
                mem_free,
                ctx_destroy,
            })
        }
    }

    /// Takes exactly `bytes` of device memory, and keeps it until this value is
    /// dropped.
    pub fn allocate(&mut self, bytes: usize) -> Result<(), String> {
        let mut pointer: u64 = 0;
        // SAFETY: the context is current on this thread and `pointer` is a live
        // local.
        let code = unsafe { (self.mem_alloc)(&mut pointer, bytes) };
        if code != 0 {
            return Err(format!("cuMemAlloc({bytes}) refused with code {code}"));
        }
        self.pointer = Some(pointer);
        Ok(())
    }
}

impl Drop for CudaArena {
    fn drop(&mut self) {
        // SAFETY: the pointer and the context are this value's own, and it is
        // being destroyed.
        unsafe {
            if let Some(pointer) = self.pointer.take() {
                (self.mem_free)(pointer);
            }
            (self.ctx_destroy)(self.context);
            libc::dlclose(self.library);
        }
    }
}
