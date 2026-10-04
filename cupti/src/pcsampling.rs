//! `gpu.profile`: where time goes inside kernels, by CUPTI's PC sampling.
//!
//! On every context the program creates, PC sampling is enabled and
//! configured in **continuous** mode: every SM samples the program counter of
//! one of its warps every `2^n` cycles ([`wire::PC_PERIOD_VARIABLE`]) with the
//! reason the warp was stalled, and CUPTI aggregates the samples per program
//! counter and reason. Continuous mode does not serialise the program's
//! kernels, unlike the kernel-serialised mode, so the program runs as it
//! would; a program counter names its function, so a sample still belongs to
//! one kernel.
//!
//! The library drains CUPTI's aggregates ([`drain`]) every [`DRAIN_EVERY`]
//! launches, at every module load (CUPTI's rule: a program counter is only
//! unique between two of them), when a context is destroyed, and at exit;
//! each drain is an [`wire::OP_PC_DRAIN`] saying when, over what window, and
//! what CUPTI counted -- samples taken, dropped, and spent in kernels that
//! are not the program's -- followed by one [`wire::OP_PC`] per program
//! counter and stall reason that took samples.
//!
//! **Source lines.** Every module loaded is kept (its cubin, by the CRC
//! CUPTI tags program counters with), and a program counter is resolved to
//! its file and line by `cuptiGetSassToSourceCorrelation` when the module
//! carries line information (`-lineinfo`, or a JIT with
//! `CU_JIT_GENERATE_LINE_INFO`); otherwise the line is zero and `omn` says
//! the kernel has none.
//!
//! **The ordering constraints are where PC sampling fails silently.** CUPTI
//! takes a configuration on a context only between `cuptiPCSamplingEnable`
//! and the first launch, and answers success for a configuration it then
//! does not apply. So every attribute's own status is checked, what CUPTI
//! applied is read back and sent beside what was asked
//! ([`wire::OP_PC_CONFIG`]), and the launches the library saw are sent with
//! every drain: a run whose kernels took no sample at all is a
//! configuration accepted and not applied, which `omn` says. (One kernel
//! without a sample among others is not: shorter than the period, it takes
//! none.)

use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr};
use std::mem::{offset_of, size_of};
use std::sync::{Mutex, OnceLock};

use crate::api::Cupti;
use crate::profiler::{
    read, CallbackFn, API_EXIT, CONTEXT_CREATED, CONTEXT_DESTROY_STARTING, DOMAIN_DRIVER_API,
    DOMAIN_RESOURCE, LAUNCHES, MODULE_LOADED,
};
use crate::wire;

/// Program counters one drain takes from CUPTI at once; a drain asks again
/// until CUPTI has none left.
const PCS: usize = 4096;
/// Launches between two drains.
const DRAIN_EVERY: u64 = 256;
/// The bound on asks within one drain: past it, what CUPTI still holds is
/// left for the next drain rather than spun on.
const ASKS: usize = 1024;
/// `CUPTI_STALL_REASON_STRING_SIZE`.
const STALL_NAME_BYTES: usize = 128;

/// `CUpti_PCSamplingConfigurationAttributeType` values this module sets.
const ATTR_SAMPLING_PERIOD: u32 = 1;
const ATTR_STALL_REASON: u32 = 2;
const ATTR_COLLECTION_MODE: u32 = 5;
const ATTR_START_STOP_CONTROL: u32 = 6;
const ATTR_OUTPUT_FORMAT: u32 = 7;
const ATTR_SAMPLING_DATA_BUFFER: u32 = 8;
const MODE_CONTINUOUS: u32 = 1;
const FORMAT_PARSED: u32 = 1;
/// `CUPTI_ERROR_OUT_OF_MEMORY`: `cuptiPCSamplingGetData`'s answer when the
/// hardware buffer filled.
const ERROR_OUT_OF_MEMORY: u32 = 13;

type ParamsFn = unsafe extern "C" fn(*mut c_void) -> u32;
type DeviceIdFn = unsafe extern "C" fn(*mut c_void, *mut u32) -> u32;
type SubscribeFn = unsafe extern "C" fn(*mut *mut c_void, CallbackFn, *mut c_void) -> u32;
type EnableCallbackFn = unsafe extern "C" fn(u32, *mut c_void, u32, u32) -> u32;

macro_rules! struct_size {
    ($type:ty, $last:ident, $last_type:ty) => {
        offset_of!($type, $last) + size_of::<$last_type>()
    };
}

/// `CUpti_PCSamplingStallReason`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct StallReason {
    index: u32,
    samples: u32,
}

/// `CUpti_PCSamplingPCData` (packed and 8-aligned in the header; every field
/// is naturally aligned, so `repr(C)` lays it out the same).
#[repr(C)]
struct PcData {
    size: usize,
    cubin_crc: u64,
    pc_offset: u64,
    function_index: u32,
    pad: u32,
    function_name: *mut c_char,
    stall_reason_count: usize,
    stall_reason: *mut StallReason,
    correlation: u32,
}

/// `CUpti_PCSamplingData`.
#[repr(C)]
struct SamplingData {
    size: usize,
    collect_num_pcs: usize,
    total_samples: u64,
    dropped_samples: u64,
    total_num_pcs: usize,
    remaining_num_pcs: usize,
    range_id: u64,
    pc_data: *mut PcData,
    non_user_samples: u64,
    hardware_buffer_full: u8,
}

/// `CUpti_PCSamplingConfigurationInfo`: a type, a status, and a union of
/// at most three words.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ConfigInfo {
    attribute: u32,
    status: u32,
    data: [u64; 3],
}

#[repr(C)]
struct ConfigParams {
    size: usize,
    priv_: usize,
    ctx: *mut c_void,
    count: usize,
    info: *mut ConfigInfo,
}

#[repr(C)]
struct ContextParams {
    size: usize,
    priv_: usize,
    ctx: *mut c_void,
}

#[repr(C)]
struct GetDataParams {
    size: usize,
    priv_: usize,
    ctx: *mut c_void,
    data: *mut c_void,
}

#[repr(C)]
struct NumStallParams {
    size: usize,
    priv_: usize,
    ctx: *mut c_void,
    count: *mut usize,
}

#[repr(C)]
struct StallParams {
    size: usize,
    priv_: usize,
    ctx: *mut c_void,
    count: usize,
    index: *mut u32,
    names: *mut *mut c_char,
}

#[repr(C)]
struct SourceParams {
    size: usize,
    cubin: *const c_void,
    function: *const c_char,
    cubin_size: usize,
    line: u32,
    pc_offset: u64,
    file: *mut c_char,
    dir: *mut c_char,
}

#[repr(C)]
struct CrcParams {
    size: usize,
    cubin_size: usize,
    cubin: *const c_void,
    crc: u64,
}

struct Api {
    describe: &'static Cupti,
    enable: ParamsFn,
    disable: ParamsFn,
    set_attribute: ParamsFn,
    get_attribute: ParamsFn,
    get_data: ParamsFn,
    stall_count: ParamsFn,
    stalls: ParamsFn,
    source: ParamsFn,
    crc: ParamsFn,
    device_id: DeviceIdFn,
    subscribe: SubscribeFn,
    enable_callback: EnableCallbackFn,
}

// SAFETY: function pointers into a library that is never unloaded.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

impl Api {
    fn call<T>(&self, what: &str, function: ParamsFn, params: &mut T) -> Result<(), String> {
        // SAFETY: `params` is the `#[repr(C)]` block `function` takes.
        let code = unsafe { function((params as *mut T).cast()) };
        self.check(what, code)
    }

    fn check(&self, what: &str, code: u32) -> Result<(), String> {
        if code == wire::cupti::SUCCESS {
            Ok(())
        } else {
            Err(format!("{what} failed: {}", self.describe.describe(code)))
        }
    }
}

static API: OnceLock<Api> = OnceLock::new();
static STATE: Mutex<State> = Mutex::new(State {
    period: wire::DEFAULT_PC_PERIOD,
    contexts: Vec::new(),
    modules: None,
    lines: None,
    names: std::collections::BTreeSet::new(),
});

/// Program counters already resolved: (crc, function, offset) to the file
/// and line, the file absent and the line zero for a module without line
/// information.
type Lines = HashMap<(u64, Vec<u8>, u64), (Option<Vec<u8>>, u32)>;

struct State {
    period: u32,
    contexts: Vec<Context>,
    /// Every module loaded, by the CRC CUPTI tags its program counters with.
    modules: Option<HashMap<u64, Vec<u8>>>,
    lines: Option<Lines>,
    /// Every function name CUPTI handed over, freed once, at [`finish`]:
    /// CUPTI shares one name across every record of a function, the next
    /// drains' included, so a name freed after one drain comes back in the
    /// next one dangling, and freeing it again is a double free (the
    /// nineteenth wave's abort at exit).
    names: std::collections::BTreeSet<usize>,
}

/// One buffer CUPTI fills: the block, its program counters, and the stall
/// reason arrays they point into.
struct Buffer {
    data: Box<SamplingData>,
    pcs: Vec<PcData>,
    reasons: Vec<StallReason>,
}

impl Buffer {
    fn new(reasons: usize) -> Self {
        let mut stalls = vec![StallReason::default(); PCS * reasons.max(1)];
        let mut pcs: Vec<PcData> = (0..PCS)
            .map(|index| PcData {
                size: size_of::<PcData>(),
                cubin_crc: 0,
                pc_offset: 0,
                function_index: 0,
                pad: 0,
                function_name: std::ptr::null_mut(),
                stall_reason_count: reasons,
                stall_reason: stalls[index * reasons.max(1)..].as_mut_ptr(),
                correlation: 0,
            })
            .collect();
        let data = Box::new(SamplingData {
            size: size_of::<SamplingData>(),
            collect_num_pcs: PCS,
            total_samples: 0,
            dropped_samples: 0,
            total_num_pcs: 0,
            remaining_num_pcs: 0,
            range_id: 0,
            pc_data: pcs.as_mut_ptr(),
            non_user_samples: 0,
            hardware_buffer_full: 0,
        });
        Self {
            data,
            pcs,
            reasons: stalls,
        }
    }

    /// Ready for CUPTI to fill again: every program counter's stall
    /// reason array back to its full length.
    fn reset(&mut self, reasons: usize) {
        self.data.total_num_pcs = 0;
        self.data.remaining_num_pcs = 0;
        self.data.total_samples = 0;
        self.data.dropped_samples = 0;
        self.data.non_user_samples = 0;
        self.data.hardware_buffer_full = 0;
        for (index, pc) in self.pcs.iter_mut().enumerate() {
            pc.stall_reason_count = reasons;
            pc.stall_reason = self.reasons[index * reasons.max(1)..].as_mut_ptr();
            pc.function_name = std::ptr::null_mut();
        }
    }
}

struct Context {
    ctx: usize,
    device: u32,
    reasons: usize,
    /// The buffer handed to CUPTI at configuration, which it fills as it
    /// tears down, and the one each drain asks it to fill.
    configured: Buffer,
    drained: Buffer,
    launches: u64,
    since_drain: u64,
    last_drain_ns: u64,
    active: bool,
}

// SAFETY: raw pointers into buffers this context owns, used only under
// `STATE`'s lock.
unsafe impl Send for Context {}

fn lock() -> std::sync::MutexGuard<'static, State> {
    STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn resolve(cupti: &'static Cupti) -> Result<Api, String> {
    macro_rules! symbol {
        ($name:literal) => {
            symbol!($name, ParamsFn)
        };
        ($name:literal, $type:ty) => {
            // SAFETY: the symbol is CUPTI's, with the signature its header
            // declares and the field's type repeats.
            unsafe { std::mem::transmute::<*mut c_void, $type>(cupti.symbol($name)?) }
        };
    }
    Ok(Api {
        describe: cupti,
        enable: symbol!("cuptiPCSamplingEnable"),
        disable: symbol!("cuptiPCSamplingDisable"),
        set_attribute: symbol!("cuptiPCSamplingSetConfigurationAttribute"),
        get_attribute: symbol!("cuptiPCSamplingGetConfigurationAttribute"),
        get_data: symbol!("cuptiPCSamplingGetData"),
        stall_count: symbol!("cuptiPCSamplingGetNumStallReasons"),
        stalls: symbol!("cuptiPCSamplingGetStallReasons"),
        source: symbol!("cuptiGetSassToSourceCorrelation"),
        crc: symbol!("cuptiGetCubinCrc"),
        device_id: symbol!("cuptiGetDeviceId", DeviceIdFn),
        subscribe: symbol!("cuptiSubscribe", SubscribeFn),
        enable_callback: symbol!("cuptiEnableCallback", EnableCallbackFn),
    })
}

/// The period [`wire::PC_PERIOD_VARIABLE`] asks for, and why it was not used when it was not:
/// a setting set and ignored is said, never replaced by the default
/// silently -- the read-back would then show the default as both asked and
/// applied.
fn period_from(setting: Option<&str>) -> (u32, Option<String>) {
    let Some(setting) = setting else {
        return (wire::DEFAULT_PC_PERIOD, None);
    };
    match setting.trim().parse::<u32>() {
        Ok(period) if (5..=31).contains(&period) => (period, None),
        _ => (
            wire::DEFAULT_PC_PERIOD,
            Some(format!(
                "gpu.profile: {}={setting:?} is not a period CUPTI takes (2^5 to 2^31 cycles,                  given as the exponent): it was ignored, and the default 2^{} used",
                wire::PC_PERIOD_VARIABLE,
                wire::DEFAULT_PC_PERIOD
            )),
        ),
    }
}

pub fn arm(cupti: &'static Cupti) -> Result<(), String> {
    let api = resolve(cupti)?;
    let api = API.get_or_init(|| api);
    {
        let mut state = lock();
        let (period, ignored) =
            period_from(std::env::var(wire::PC_PERIOD_VARIABLE).ok().as_deref());
        if let Some(reason) = ignored {
            crate::error(&reason);
        }
        state.period = period;
        state.modules = Some(HashMap::new());
        state.lines = Some(HashMap::new());
    }
    let mut subscriber: *mut c_void = std::ptr::null_mut();
    // SAFETY: CUPTI writes a handle we own; the callback has the signature
    // `CUpti_CallbackFunc` declares.
    api.check("cuptiSubscribe", unsafe {
        (api.subscribe)(&mut subscriber, callback, std::ptr::null_mut())
    })?;
    let enable = |domain: u32, id: u32| {
        // SAFETY: enabling one callback id of a domain on our subscriber.
        api.check("cuptiEnableCallback", unsafe {
            (api.enable_callback)(1, subscriber, domain, id)
        })
    };
    enable(DOMAIN_RESOURCE, CONTEXT_CREATED)?;
    enable(DOMAIN_RESOURCE, CONTEXT_DESTROY_STARTING)?;
    enable(DOMAIN_RESOURCE, MODULE_LOADED)?;
    for launch in LAUNCHES {
        enable(DOMAIN_DRIVER_API, launch)?;
    }
    Ok(())
}

extern "C" fn callback(_user: *mut c_void, domain: u32, id: u32, data: *const c_void) {
    if data.is_null() || !crate::owner() {
        return;
    }
    let Some(api) = API.get() else {
        return;
    };
    match (domain, id) {
        (DOMAIN_RESOURCE, CONTEXT_CREATED) => {
            // SAFETY: a `CUpti_ResourceData`, whose first field is the context.
            let ctx: *mut c_void = unsafe { read(data, 0) };
            let mut state = lock();
            let period = state.period;
            match set_up(api, ctx, period) {
                Ok(context) => state.contexts.push(context),
                Err(reason) => crate::error(&format!(
                    "gpu.profile: PC sampling could not be set up on a context, so none of its \
                     kernels is sampled: {reason}"
                )),
            }
            crate::send_now();
        }
        (DOMAIN_RESOURCE, CONTEXT_DESTROY_STARTING) => {
            // SAFETY: as above.
            let ctx: *mut c_void = unsafe { read(data, 0) };
            let mut state = lock();
            tear_down_one(api, &mut state, ctx as usize);
            crate::send_now();
        }
        (DOMAIN_RESOURCE, MODULE_LOADED) => {
            // SAFETY: a `CUpti_ResourceData`: the context, a handle, then
            // the `CUpti_ModuleResourceData` it describes.
            let (ctx, module): (*mut c_void, *const c_void) =
                unsafe { (read(data, 0), read(data, 16)) };
            if module.is_null() {
                return;
            }
            // SAFETY: `CUpti_ModuleResourceData`: id, size, then the cubin.
            let (size, cubin): (usize, *const u8) = unsafe { (read(module, 8), read(module, 16)) };
            let mut state = lock();
            if !cubin.is_null() && size > 0 {
                // SAFETY: CUPTI hands the module's image for the callback.
                let bytes = unsafe { std::slice::from_raw_parts(cubin, size) }.to_vec();
                let mut crc = CrcParams {
                    size: struct_size!(CrcParams, crc, u64),
                    cubin_size: bytes.len(),
                    cubin: bytes.as_ptr().cast(),
                    crc: 0,
                };
                match api.call("cuptiGetCubinCrc", api.crc, &mut crc) {
                    Ok(()) => {
                        if let Some(modules) = state.modules.as_mut() {
                            modules.insert(crc.crc, bytes);
                        }
                    }
                    Err(reason) => crate::error(&format!(
                        "gpu.profile: a module's program counters cannot be resolved to source \
                         lines: {reason}"
                    )),
                }
            }
            // CUPTI's rule: a program counter is unique only between two
            // module loads, so what it holds is taken now.
            drain_one(api, &mut state, ctx as usize);
            crate::send_now();
        }
        (DOMAIN_DRIVER_API, launch) if LAUNCHES.contains(&launch) => {
            // SAFETY: a `CUpti_CallbackData`: the site, then the context.
            let (site, ctx): (u32, *mut c_void) = unsafe { (read(data, 0), read(data, 40)) };
            if site != API_EXIT {
                return;
            }
            let mut state = lock();
            let Some(context) = state.contexts.iter_mut().find(|c| c.ctx == ctx as usize) else {
                return;
            };
            context.launches += 1;
            context.since_drain += 1;
            if context.since_drain >= DRAIN_EVERY {
                drain_one(api, &mut state, ctx as usize);
                crate::send_now();
            }
        }
        _ => {}
    }
}

fn set_up(api: &Api, ctx: *mut c_void, period: u32) -> Result<Context, String> {
    let mut device = 0u32;
    // SAFETY: CUPTI writes one u32 we own.
    api.check("cuptiGetDeviceId", unsafe {
        (api.device_id)(ctx, &mut device)
    })?;
    let mut enable = ContextParams {
        size: struct_size!(ContextParams, ctx, *mut c_void),
        priv_: 0,
        ctx,
    };
    api.call("cuptiPCSamplingEnable", api.enable, &mut enable)?;

    let mut count = 0usize;
    let mut stall_count = NumStallParams {
        size: struct_size!(NumStallParams, count, *mut usize),
        priv_: 0,
        ctx,
        count: &mut count,
    };
    api.call(
        "cuptiPCSamplingGetNumStallReasons",
        api.stall_count,
        &mut stall_count,
    )?;
    let mut indexes = vec![0u32; count];
    let mut storage = vec![[0 as c_char; STALL_NAME_BYTES]; count];
    let mut names: Vec<*mut c_char> = storage.iter_mut().map(|name| name.as_mut_ptr()).collect();
    let mut stalls = StallParams {
        size: struct_size!(StallParams, names, *mut *mut c_char),
        priv_: 0,
        ctx,
        count,
        index: indexes.as_mut_ptr(),
        names: names.as_mut_ptr(),
    };
    api.call("cuptiPCSamplingGetStallReasons", api.stalls, &mut stalls)?;
    crate::with_channel(|channel| {
        for (index, name) in indexes.iter().zip(&storage) {
            // SAFETY: CUPTI wrote a nul-terminated name into each slot.
            let name = unsafe { CStr::from_ptr(name.as_ptr()) }.to_bytes();
            let mut payload = index.to_ne_bytes().to_vec();
            payload.extend_from_slice(name);
            channel.frame(wire::OP_STALL, &payload);
        }
    });

    let mut context = Context {
        ctx: ctx as usize,
        device,
        reasons: count,
        configured: Buffer::new(count),
        drained: Buffer::new(count),
        launches: 0,
        since_drain: 0,
        last_drain_ns: crate::monotonic_ns(),
        active: true,
    };

    let mut infos = [
        attribute(ATTR_SAMPLING_PERIOD, [u64::from(period), 0, 0]),
        attribute(
            ATTR_STALL_REASON,
            [count as u64, indexes.as_mut_ptr() as u64, 0],
        ),
        attribute(
            ATTR_SAMPLING_DATA_BUFFER,
            [
                &mut *context.configured.data as *mut SamplingData as u64,
                0,
                0,
            ],
        ),
        attribute(ATTR_COLLECTION_MODE, [u64::from(MODE_CONTINUOUS), 0, 0]),
        attribute(ATTR_START_STOP_CONTROL, [0, 0, 0]),
        attribute(ATTR_OUTPUT_FORMAT, [u64::from(FORMAT_PARSED), 0, 0]),
    ];
    let mut set = ConfigParams {
        size: struct_size!(ConfigParams, info, *mut ConfigInfo),
        priv_: 0,
        ctx,
        count: infos.len(),
        info: infos.as_mut_ptr(),
    };
    api.call(
        "cuptiPCSamplingSetConfigurationAttribute",
        api.set_attribute,
        &mut set,
    )?;
    // The call's own answer covers the call; each attribute carries its own,
    // and one CUPTI refused under a successful call is still refused.
    for info in &infos {
        api.check(
            &format!("PC sampling attribute {}", info.attribute),
            info.status,
        )?;
    }

    // What CUPTI applied, asked back rather than assumed.
    let mut back = [
        attribute(ATTR_SAMPLING_PERIOD, [0; 3]),
        attribute(ATTR_COLLECTION_MODE, [0; 3]),
    ];
    let mut get = ConfigParams {
        size: struct_size!(ConfigParams, info, *mut ConfigInfo),
        priv_: 0,
        ctx,
        count: back.len(),
        info: back.as_mut_ptr(),
    };
    api.call(
        "cuptiPCSamplingGetConfigurationAttribute",
        api.get_attribute,
        &mut get,
    )?;
    let config = wire::PcConfig {
        device,
        period_asked: period,
        period_applied: back[0].data[0] as u32,
        mode_applied: back[1].data[0] as u32,
        stall_reasons: count as u32,
        pad: 0,
    };
    crate::with_channel(|channel| channel.frame(wire::OP_PC_CONFIG, config.encode().as_slice()));
    // The stall reason indexes were read by CUPTI during the call; the
    // names were sent. Neither is needed past here.
    drop((indexes, storage, names));
    Ok(context)
}

fn attribute(kind: u32, data: [u64; 3]) -> ConfigInfo {
    ConfigInfo {
        attribute: kind,
        status: 0,
        data,
    }
}

/// Takes what CUPTI holds for one context, until it holds nothing or
/// [`ASKS`] asks have been made.
fn drain_one(api: &Api, state: &mut State, ctx: usize) {
    let State {
        contexts,
        modules,
        lines,
        names,
        ..
    } = state;
    let Some(context) = contexts.iter_mut().find(|c| c.ctx == ctx && c.active) else {
        return;
    };
    drain(api, context, modules, lines, names);
}

fn drain(
    api: &Api,
    context: &mut Context,
    modules: &Option<HashMap<u64, Vec<u8>>>,
    lines: &mut Option<Lines>,
    names: &mut std::collections::BTreeSet<usize>,
) {
    context.since_drain = 0;
    for _ in 0..ASKS {
        context.drained.reset(context.reasons);
        let mut get = GetDataParams {
            size: struct_size!(GetDataParams, data, *mut c_void),
            priv_: 0,
            ctx: context.ctx as *mut c_void,
            data: (&mut *context.drained.data as *mut SamplingData).cast(),
        };
        // SAFETY: the block and the buffer it points into are ours.
        let code = unsafe { (api.get_data)((&mut get as *mut GetDataParams).cast()) };
        if code != wire::cupti::SUCCESS {
            if code == ERROR_OUT_OF_MEMORY || context.drained.data.hardware_buffer_full != 0 {
                lost(1, wire::LOST_HW_BUFFER_FULL);
            } else {
                crate::error(&format!(
                    "gpu.profile: {}",
                    api.check("cuptiPCSamplingGetData", code)
                        .err()
                        .unwrap_or_default()
                ));
            }
            return;
        }
        let taken = context.drained.data.total_num_pcs;
        report(api, context, false, modules, lines, names);
        if taken == 0 && context.drained.data.remaining_num_pcs == 0 {
            return;
        }
    }
}

/// Sends one drain's account and its program counters, from the drained
/// buffer or, at teardown, from the configured one.
fn report(
    api: &Api,
    context: &mut Context,
    configured: bool,
    modules: &Option<HashMap<u64, Vec<u8>>>,
    lines: &mut Option<Lines>,
    names: &mut std::collections::BTreeSet<usize>,
) {
    let now = crate::monotonic_ns();
    let buffer = if configured {
        &context.configured
    } else {
        &context.drained
    };
    let data = &buffer.data;
    let taken = data.total_num_pcs.min(PCS);
    if data.dropped_samples > 0 {
        lost(data.dropped_samples, wire::LOST_SAMPLES);
    }
    if data.hardware_buffer_full != 0 {
        lost(1, wire::LOST_HW_BUFFER_FULL);
    }
    let drain = wire::PcDrain {
        at_ns: now,
        since_ns: context.last_drain_ns,
        total: data.total_samples,
        dropped: data.dropped_samples,
        non_user: data.non_user_samples,
        pcs: taken as u32,
        device: context.device,
        launches: context.launches,
    };
    let mut rows = Vec::new();
    for pc in &buffer.pcs[..taken] {
        let function = if pc.function_name.is_null() {
            b"?".to_vec()
        } else {
            // SAFETY: a nul-terminated name CUPTI wrote for this drain.
            unsafe { CStr::from_ptr(pc.function_name) }
                .to_bytes()
                .to_vec()
        };
        let place = source(api, modules, lines, pc.cubin_crc, &function, pc.pc_offset);
        let reasons = pc.stall_reason_count.min(context.reasons);
        // SAFETY: CUPTI filled `stall_reason_count` entries of the array
        // this buffer handed it.
        let stalls = unsafe { std::slice::from_raw_parts(pc.stall_reason, reasons) };
        for stall in stalls.iter().filter(|stall| stall.samples > 0) {
            rows.push((
                pc.cubin_crc,
                pc.pc_offset,
                function.clone(),
                *stall,
                place.clone(),
            ));
        }
        if !pc.function_name.is_null() {
            names.insert(pc.function_name as usize);
        }
    }
    context.last_drain_ns = now;
    crate::with_channel(|channel| {
        channel.frame(wire::OP_PC_DRAIN, drain.encode().as_slice());
        for (crc, offset, function, stall, (file, line)) in rows {
            let sample = wire::PcSample {
                cubin_crc: crc,
                pc_offset: offset,
                function: channel.name(&function),
                stall: stall.index,
                samples: stall.samples,
                line,
                file: file.map(|file| channel.name(&file)).unwrap_or(0),
                device: context.device,
            };
            channel.frame(wire::OP_PC, sample.encode().as_slice());
        }
    });
}

/// The file and line of a program counter, when its module carries line
/// information; cached, because the same counter comes back every drain.
fn source(
    api: &Api,
    modules: &Option<HashMap<u64, Vec<u8>>>,
    lines: &mut Option<Lines>,
    crc: u64,
    function: &[u8],
    offset: u64,
) -> (Option<Vec<u8>>, u32) {
    let key = (crc, function.to_vec(), offset);
    if let Some(known) = lines.as_ref().and_then(|lines| lines.get(&key)) {
        return known.clone();
    }
    let Some(cubin) = modules.as_ref().and_then(|modules| modules.get(&crc)) else {
        return (None, 0);
    };
    let Ok(name) = std::ffi::CString::new(function) else {
        return (None, 0);
    };
    let mut params = SourceParams {
        size: struct_size!(SourceParams, dir, *mut c_char),
        cubin: cubin.as_ptr().cast(),
        function: name.as_ptr(),
        cubin_size: cubin.len(),
        line: 0,
        pc_offset: offset,
        file: std::ptr::null_mut(),
        dir: std::ptr::null_mut(),
    };
    // A module without line information answers an error: no line, which
    // is an answer, not a failure to report.
    let found = api
        .call("cuptiGetSassToSourceCorrelation", api.source, &mut params)
        .is_ok();
    let mut file = None;
    if found && !params.file.is_null() {
        // SAFETY: CUPTI allocated both strings with `malloc` for the caller.
        unsafe {
            let mut path = Vec::new();
            if !params.dir.is_null() {
                let dir = CStr::from_ptr(params.dir).to_bytes();
                if !dir.is_empty() {
                    path.extend_from_slice(dir);
                    path.push(b'/');
                }
            }
            path.extend_from_slice(CStr::from_ptr(params.file).to_bytes());
            file = Some(path);
        }
    }
    // SAFETY: as above; `free` of a null pointer does nothing.
    unsafe {
        libc::free(params.file.cast());
        libc::free(params.dir.cast());
    }
    let line = if found { params.line } else { 0 };
    if let Some(lines) = lines.as_mut() {
        lines.insert(key, (file.clone(), line));
    }
    (file, line)
}

fn lost(count: u64, kind: u32) {
    let mut fields = wire::Fields::<16>::default();
    fields.u64(count).u32(kind).u32(0);
    crate::with_channel(|channel| channel.frame(wire::OP_INSIDE_LOST, fields.as_slice()));
}

/// The last drain of a context, then its teardown: what CUPTI puts in the
/// configured buffer as it disables is reported, and what did not fit is
/// counted.
fn tear_down_one(api: &Api, state: &mut State, ctx: usize) {
    let State {
        contexts,
        modules,
        lines,
        names,
        ..
    } = state;
    let Some(context) = contexts.iter_mut().find(|c| c.ctx == ctx && c.active) else {
        return;
    };
    drain(api, context, modules, lines, names);
    context.active = false;
    context.configured.reset(context.reasons);
    let mut disable = ContextParams {
        size: struct_size!(ContextParams, ctx, *mut c_void),
        priv_: 0,
        ctx: ctx as *mut c_void,
    };
    if let Err(reason) = api.call("cuptiPCSamplingDisable", api.disable, &mut disable) {
        crate::error(&format!("gpu.profile: {reason}"));
        return;
    }
    if context.configured.data.total_num_pcs > 0 {
        report(api, context, true, modules, lines, names);
    }
    if context.configured.data.remaining_num_pcs > 0 {
        lost(
            context.configured.data.remaining_num_pcs as u64,
            wire::LOST_PCS,
        );
    }
}

/// At exit: every context still sampled.
pub fn finish() {
    let Some(api) = API.get() else {
        return;
    };
    let mut state = lock();
    let contexts: Vec<usize> = state.contexts.iter().map(|context| context.ctx).collect();
    for ctx in contexts {
        tear_down_one(api, &mut state, ctx);
    }
    // CUPTI allocates a function's name with `malloc` and leaves it to the
    // caller once every record is read -- every drain's: one name is shared
    // by every program counter of the function, in this drain and the next.
    // So the names are freed here, after the last drain of every context,
    // each once.
    for name in std::mem::take(&mut state.names) {
        // SAFETY: a pointer CUPTI handed over, freed once, after its last use.
        unsafe { libc::free(name as *mut c_void) };
    }
    crate::send_now();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The records and parameter blocks, against `sizeof` and `offsetof`
    /// from CUPTI 13.4's headers, measured with a C compiler.
    #[test]
    fn the_records_have_the_layout_cupti_s_headers_give() {
        assert_eq!(size_of::<StallReason>(), 8);
        assert_eq!(size_of::<PcData>(), 64);
        assert_eq!(offset_of!(PcData, function_name), 32);
        assert_eq!(offset_of!(PcData, stall_reason), 48);
        assert_eq!(offset_of!(PcData, correlation), 56);
        assert_eq!(size_of::<SamplingData>(), 80);
        assert_eq!(offset_of!(SamplingData, pc_data), 56);
        assert_eq!(offset_of!(SamplingData, hardware_buffer_full), 72);
        assert_eq!(size_of::<ConfigInfo>(), 32);
        assert_eq!(offset_of!(ConfigInfo, data), 8);
        assert_eq!(size_of::<ConfigParams>(), 40);
        assert_eq!(size_of::<GetDataParams>(), 32);
        assert_eq!(size_of::<ContextParams>(), 24);
        assert_eq!(size_of::<NumStallParams>(), 32);
        assert_eq!(size_of::<StallParams>(), 48);
        assert_eq!(size_of::<SourceParams>(), 64);
        assert_eq!(offset_of!(SourceParams, line), 32);
        assert_eq!(offset_of!(SourceParams, pc_offset), 40);
        assert_eq!(size_of::<CrcParams>(), 32);
    }

    #[test]
    fn a_period_outside_cupti_s_range_falls_back_to_the_default_and_says_so() {
        assert_eq!(period_from(None), (wire::DEFAULT_PC_PERIOD, None));
        assert_eq!(period_from(Some("14")), (14, None));
        for ignored in ["4", "32", "fast"] {
            let (period, reason) = period_from(Some(ignored));
            assert_eq!(period, wire::DEFAULT_PC_PERIOD);
            assert!(reason.unwrap().contains("ignored"), "{ignored}");
        }
    }
}
