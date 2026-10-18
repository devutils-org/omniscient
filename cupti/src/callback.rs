//! `gpu.api` through CUPTI's callback API: every call into the CUDA driver
//! API, timed in the program.
//!
//! CUPTI calls [`on_call`] at the entry and the exit of each driver API call
//! the program makes, on the calling thread, with a slot for that call's own
//! data (`correlationData`): the entry writes the moment there, the exit reads
//! it back and sends one [`wire::OP_API`] frame -- when, how long, which call
//! and what it returned. Both ends are `CLOCK_MONOTONIC`, read inside the
//! program, so the frame needs no calibration.
//!
//! What the preload shim cannot see, this does: a call reached through
//! `cuGetProcAddress` (the CUDA runtime fetches every driver entry point that
//! way) is a call into the driver all the same. What it does not see is what
//! is not the driver API: Vulkan and NVTX stay the shim's.

use std::ffi::{c_char, c_void, CStr};

use crate::api::{Cupti, Result};
use crate::wire;

/// `CUpti_CallbackDomain`'s driver API domain.
const DOMAIN_DRIVER_API: u32 = 1;
/// `CUpti_ApiCallbackSite`.
const SITE_ENTER: u32 = 0;
const SITE_EXIT: u32 = 1;

/// The prefix of `CUpti_CallbackData` this reads; the fields' order and
/// types are the header's, unchanged since CUPTI 4.
#[repr(C)]
struct CallbackData {
    site: u32,
    function_name: *const c_char,
    function_params: *const c_void,
    function_return_value: *mut c_void,
    symbol_name: *const c_char,
    context: *mut c_void,
    context_uid: u32,
    correlation_data: *mut u64,
    correlation_id: u32,
}

type CallbackFn = extern "C" fn(*mut c_void, u32, u32, *const c_void);
type SubscribeFn = unsafe extern "C" fn(*mut *mut c_void, CallbackFn, *mut c_void) -> Result;
type EnableDomainFn = unsafe extern "C" fn(u32, *mut c_void, u32) -> Result;

/// Subscribes to the driver API domain. A failure is the caller's to report.
pub fn arm(cupti: &Cupti) -> std::result::Result<(), String> {
    // SAFETY: CUPTI's own entry points, with the signatures its header gives.
    let subscribe: SubscribeFn =
        unsafe { std::mem::transmute::<*mut c_void, SubscribeFn>(cupti.symbol("cuptiSubscribe")?) };
    let enable_domain: EnableDomainFn = unsafe {
        std::mem::transmute::<*mut c_void, EnableDomainFn>(cupti.symbol("cuptiEnableDomain")?)
    };
    let mut subscriber: *mut c_void = std::ptr::null_mut();
    // SAFETY: CUPTI writes the subscriber handle into memory we own.
    let code = unsafe { subscribe(&mut subscriber, on_call, std::ptr::null_mut()) };
    if code != wire::cupti::SUCCESS {
        return Err(format!("cuptiSubscribe failed: {}", cupti.describe(code)));
    }
    // SAFETY: enabling a documented domain on the subscriber just made.
    let code = unsafe { enable_domain(1, subscriber, DOMAIN_DRIVER_API) };
    if code != wire::cupti::SUCCESS {
        return Err(format!(
            "cuptiEnableDomain(DRIVER_API) failed: {}",
            cupti.describe(code)
        ));
    }
    Ok(())
}

/// The thread's id, as the kernel numbers it.
fn tid() -> u32 {
    // SAFETY: a system call that takes nothing.
    unsafe { libc::syscall(libc::SYS_gettid) as u32 }
}

extern "C" fn on_call(_user: *mut c_void, domain: u32, _id: u32, data: *const c_void) {
    if domain != DOMAIN_DRIVER_API || data.is_null() || !crate::owner() {
        return;
    }
    // SAFETY: CUPTI hands a `CUpti_CallbackData` for the driver API domain.
    let data = unsafe { &*data.cast::<CallbackData>() };
    if data.correlation_data.is_null() {
        return;
    }
    match data.site {
        SITE_ENTER => {
            // SAFETY: the call's own slot, which CUPTI keeps until its exit.
            unsafe { *data.correlation_data = crate::monotonic_ns() };
        }
        SITE_EXIT => {
            let end = crate::monotonic_ns();
            // SAFETY: the slot the entry wrote.
            let start = unsafe { *data.correlation_data };
            if start == 0 || start > end {
                crate::with_channel(|channel| {
                    channel.frame(wire::OP_API_UNPAIRED, &1u64.to_ne_bytes())
                });
                return;
            }
            let result = if data.function_return_value.is_null() {
                0
            } else {
                // SAFETY: a driver API call returns a `CUresult`, an int.
                unsafe { *data.function_return_value.cast::<i32>() }
            };
            let name: &[u8] = if data.function_name.is_null() {
                b"?"
            } else {
                // SAFETY: CUPTI's static, nul-terminated name of the call.
                unsafe { CStr::from_ptr(data.function_name) }.to_bytes()
            };
            crate::with_channel(|channel| {
                let id = channel.name(name);
                let call = wire::Api {
                    start_ns: start,
                    duration_ns: end - start,
                    name: id,
                    result,
                    tid: tid(),
                };
                channel.frame(wire::OP_API, &call.encode());
            });
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The prefix this reads is laid out as CUPTI's header lays it out on a
    /// 64-bit target: the site, then five pointers, the context uid, the
    /// correlation slot and its id.
    #[test]
    fn the_callback_data_prefix_is_the_header_s() {
        assert_eq!(std::mem::offset_of!(CallbackData, function_name), 8);
        assert_eq!(std::mem::offset_of!(CallbackData, context_uid), 48);
        assert_eq!(std::mem::offset_of!(CallbackData, correlation_data), 56);
        assert_eq!(std::mem::offset_of!(CallbackData, correlation_id), 64);
    }

    #[test]
    fn an_api_frame_round_trips() {
        let call = wire::Api {
            start_ns: 7,
            duration_ns: 4_200,
            name: 3,
            result: -1,
            tid: 99,
        };
        assert_eq!(wire::Api::decode(&call.encode()), Some(call));
    }
}
