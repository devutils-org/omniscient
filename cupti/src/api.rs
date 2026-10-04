//! CUPTI, opened at runtime.
//!
//! The library is not linked: a program the tool launches may have no CUDA
//! toolkit beside its driver, and an injection library that fails to load
//! takes the program's `cuInit` down with it. So CUPTI is looked up the way
//! `vendor:nvml` looks up NVML, and a machine without it is a run that says
//! so rather than a program that does not start.

use std::ffi::{c_char, c_void, CStr, CString};

/// `CUptiResult`.
pub type Result = u32;

pub type RequestFn = extern "C" fn(*mut *mut u8, *mut usize, *mut usize);
pub type CompleteFn = extern "C" fn(*mut c_void, u32, *mut u8, usize, usize);

type RegisterFn = unsafe extern "C" fn(RequestFn, CompleteFn) -> Result;
type KindFn = unsafe extern "C" fn(u32) -> Result;
type NextFn = unsafe extern "C" fn(*mut u8, usize, *mut *mut u8) -> Result;
type DroppedFn = unsafe extern "C" fn(*mut c_void, u32, *mut usize) -> Result;
type U64Fn = unsafe extern "C" fn(*mut u64) -> Result;
type U32Fn = unsafe extern "C" fn(*mut u32) -> Result;
type StringFn = unsafe extern "C" fn(Result, *mut *const c_char) -> Result;

/// The entry points this library calls, resolved once.
pub struct Cupti {
    pub register_callbacks: RegisterFn,
    pub enable: KindFn,
    pub next_record: NextFn,
    pub dropped: DroppedFn,
    pub flush_all: KindFn,
    pub timestamp: U64Fn,
    pub version: U32Fn,
    result_string: StringFn,
}

// SAFETY: function pointers into a library that is never unloaded.
unsafe impl Send for Cupti {}
unsafe impl Sync for Cupti {}

/// Where CUPTI is looked for, in order, after `OMN_CUPTI_LIB`: the
/// unversioned name the loader's cache gives a toolkit install, then the
/// sonames of the releases this was written against.
const NAMES: [&str; 3] = ["libcupti.so", "libcupti.so.13", "libcupti.so.12"];

impl Cupti {
    pub fn open() -> std::result::Result<Self, String> {
        let mut tried = Vec::new();
        let explicit = std::env::var("OMN_CUPTI_LIB").ok();
        let mut handle = std::ptr::null_mut();
        for name in explicit.iter().map(String::as_str).chain(NAMES) {
            let Ok(c_name) = CString::new(name) else {
                continue;
            };
            // SAFETY: a nul-terminated name; the handle is kept for the life
            // of the process.
            handle = unsafe { libc::dlopen(c_name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
            if !handle.is_null() {
                break;
            }
            tried.push(format!("{name}: {}", dl_error()));
        }
        if handle.is_null() {
            return Err(format!("CUPTI is not loadable ({})", tried.join("; ")));
        }
        macro_rules! symbol {
            ($name:literal, $type:ty) => {{
                // SAFETY: looking up a symbol in a handle that stays open.
                let address = unsafe { libc::dlsym(handle, concat!($name, "\0").as_ptr().cast()) };
                if address.is_null() {
                    return Err(format!("CUPTI has no `{}`", $name));
                }
                // SAFETY: the symbol is CUPTI's, with the signature its
                // header declares and the field's type repeats.
                unsafe { std::mem::transmute::<*mut c_void, $type>(address) }
            }};
        }
        Ok(Self {
            register_callbacks: symbol!("cuptiActivityRegisterCallbacks", RegisterFn),
            enable: symbol!("cuptiActivityEnable", KindFn),
            next_record: symbol!("cuptiActivityGetNextRecord", NextFn),
            dropped: symbol!("cuptiActivityGetNumDroppedRecords", DroppedFn),
            flush_all: symbol!("cuptiActivityFlushAll", KindFn),
            timestamp: symbol!("cuptiGetTimestamp", U64Fn),
            version: symbol!("cuptiGetVersion", U32Fn),
            result_string: symbol!("cuptiGetResultString", StringFn),
        })
    }

    /// CUPTI's own name for a result code.
    pub fn describe(&self, code: Result) -> String {
        let mut text: *const c_char = std::ptr::null();
        // SAFETY: CUPTI writes a pointer to a static string.
        let known = unsafe { (self.result_string)(code, &mut text) } == 0 && !text.is_null();
        if known {
            // SAFETY: a nul-terminated static string.
            unsafe { CStr::from_ptr(text) }
                .to_string_lossy()
                .into_owned()
        } else {
            format!("CUPTI error {code}")
        }
    }

    pub fn now(&self) -> Option<u64> {
        let mut value = 0u64;
        // SAFETY: CUPTI writes one u64 we own.
        (unsafe { (self.timestamp)(&mut value) } == 0).then_some(value)
    }
}

fn dl_error() -> String {
    // SAFETY: `dlerror` returns a loader-owned string or null.
    unsafe {
        let text = libc::dlerror();
        if text.is_null() {
            "the dynamic loader gave no reason".to_owned()
        } else {
            CStr::from_ptr(text).to_string_lossy().into_owned()
        }
    }
}
