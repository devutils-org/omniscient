//! `gpu.counters`: counters per kernel launch, by CUPTI's range profiler.
//!
//! The range profiler is set up on every context the program creates, in
//! **kernel replay** with **automatic ranges**: every kernel launch is a
//! range, and CUPTI runs it as many times as the counters asked need passes,
//! saving and restoring the memory it touches between them. That is what
//! makes a counter per launch possible, and what makes the launch's time
//! meaningless: a replayed kernel runs several times, serialised, with
//! CUPTI's own work between the runs. So a run that asks for counters does
//! not time kernels (`omn` refuses `gpu.kernel` beside it).
//!
//! The counters of a launch are read back in batches: the counter data image
//! holds [`RANGES`] ranges, and every [`BATCH`] launches -- and when a
//! context is destroyed, and at exit -- the library decodes it, evaluates
//! every range into the counters' values, sends one [`wire::OP_COUNTER`] per
//! counter and launch, and starts the image over. A range is paired with the
//! launch it measured by order: launches are recorded at the exit of their
//! launch call, in the order the ranges are. Anything that does not pair --
//! a range the decode dropped, a launch with no range -- is counted
//! ([`wire::OP_INSIDE_LOST`]) rather than guessed at.
//!
//! **Clocks are left alone.** CUPTI since 13.4 locks the device's clocks only
//! when asked to (`cuptiClockControlLock`), and this library never asks: the
//! device's clocks are reported, never changed. A counter that depends on
//! the clock -- cycles -- is then as variable as the clock, which the
//! per-counter verdict `omn` computes shows rather than hides.
//!
//! **Every call's result is checked**, and a failure is an
//! [`wire::OP_ERROR`] naming the call, after which the context is left
//! unmeasured and its launches are counted as undecoded.

use std::collections::VecDeque;
use std::ffi::{c_char, c_void, CStr, CString};
use std::mem::{offset_of, size_of};
use std::sync::{Mutex, OnceLock};

use crate::api::Cupti;
use crate::wire;

/// Ranges the counter data image holds, and how many launches are let
/// accumulate before it is decoded. The image is decoded before it is full,
/// so a launch the program makes between two decodes always has its range.
const RANGES: usize = 64;
const BATCH: usize = 32;

/// `CUpti_ProfilerRange`, `CUpti_ProfilerReplayMode` and
/// `CUpti_ProfilerType` values this module uses.
const AUTO_RANGE: u32 = 1;
const KERNEL_REPLAY: u32 = 2;
const RANGE_PROFILER: u32 = 0;
/// `CUpti_Profiler_API::CUPTI_PROFILER_RANGE_PROFILING`, and
/// `CUpti_Profiler_Support_Level::CUPTI_PROFILER_CONFIGURATION_SUPPORTED`.
const API_RANGE_PROFILING: u32 = 0;
const SUPPORTED: u32 = 3;

/// The callback domains and ids this module subscribes to
/// (`cupti_callbacks.h`, `cupti_driver_cbid.h`).
pub(crate) const DOMAIN_DRIVER_API: u32 = 1;
pub(crate) const DOMAIN_RESOURCE: u32 = 3;
pub(crate) const CONTEXT_CREATED: u32 = 1;
pub(crate) const CONTEXT_DESTROY_STARTING: u32 = 2;
pub(crate) const MODULE_LOADED: u32 = 6;
pub(crate) const API_EXIT: u32 = 1;
/// Every driver entry point that launches a kernel: `cuLaunchKernel`,
/// its per-thread-stream form, the cooperative and extended launches.
/// A graph launch is not among them: its kernels are ranges the library
/// cannot name from the call, and are counted as unpaired.
pub(crate) const LAUNCHES: [u32; 6] = [307, 442, 477, 478, 652, 653];

type ParamsFn = unsafe extern "C" fn(*mut c_void) -> u32;
type DeviceIdFn = unsafe extern "C" fn(*mut c_void, *mut u32) -> u32;
pub(crate) type CallbackFn = extern "C" fn(*mut c_void, u32, u32, *const c_void);
type SubscribeFn = unsafe extern "C" fn(*mut *mut c_void, CallbackFn, *mut c_void) -> u32;
type EnableCallbackFn = unsafe extern "C" fn(u32, *mut c_void, u32, u32) -> u32;

/// The size CUPTI checks a parameter block by: up to the end of its last
/// field, without trailing padding (`CUPTI_PROFILER_STRUCT_SIZE`).
macro_rules! struct_size {
    ($type:ty, $last:ident, $last_type:ty) => {
        offset_of!($type, $last) + size_of::<$last_type>()
    };
}

#[repr(C)]
#[derive(Default)]
struct Plain {
    struct_size: usize,
    priv_: usize,
}

#[repr(C)]
#[derive(Default)]
struct DeviceSupported {
    struct_size: usize,
    priv_: usize,
    device: i32,
    is_supported: u32,
    architecture: u32,
    sli: u32,
    vgpu: u32,
    confidential_compute: u32,
    cmp: u32,
    wsl: u32,
    api: u32,
    sku: u32,
}

#[repr(C)]
struct ChipName {
    struct_size: usize,
    priv_: usize,
    device_index: usize,
    chip: *const c_char,
}

#[repr(C)]
struct CounterAvailability {
    struct_size: usize,
    priv_: usize,
    ctx: *mut c_void,
    image_size: usize,
    image: *mut u8,
    device_level: u8,
}

#[repr(C)]
struct HostInitialize {
    struct_size: usize,
    priv_: usize,
    profiler_type: u32,
    chip: *const c_char,
    availability: *const u8,
    host: *mut c_void,
    single_pass_set: *const c_char,
}

#[repr(C)]
struct HostObject {
    struct_size: usize,
    priv_: usize,
    host: *mut c_void,
}

#[repr(C)]
struct AddMetrics {
    struct_size: usize,
    priv_: usize,
    host: *mut c_void,
    names: *const *const c_char,
    count: usize,
}

#[repr(C)]
struct ConfigImageSize {
    struct_size: usize,
    priv_: usize,
    host: *mut c_void,
    size: usize,
}

#[repr(C)]
struct ConfigImage {
    struct_size: usize,
    priv_: usize,
    host: *mut c_void,
    size: usize,
    image: *mut u8,
}

#[repr(C)]
struct NumOfPasses {
    struct_size: usize,
    priv_: usize,
    size: usize,
    image: *mut u8,
    passes: usize,
}

#[repr(C)]
struct Evaluate {
    struct_size: usize,
    priv_: usize,
    host: *mut c_void,
    image: *const u8,
    image_size: usize,
    range: usize,
    names: *const *const c_char,
    count: usize,
    values: *mut f64,
}

#[repr(C)]
struct Enable {
    struct_size: usize,
    priv_: usize,
    ctx: *mut c_void,
    object: *mut c_void,
    cache_control: u32,
}

#[repr(C)]
struct Object {
    struct_size: usize,
    priv_: usize,
    object: *mut c_void,
}

#[repr(C)]
struct Stop {
    struct_size: usize,
    priv_: usize,
    object: *mut c_void,
    pass_index: usize,
    target_nesting_level: usize,
    all_passes_submitted: u8,
}

#[repr(C)]
struct SetConfig {
    struct_size: usize,
    priv_: usize,
    object: *mut c_void,
    config_size: usize,
    config: *const u8,
    image_size: usize,
    image: *mut u8,
    range: u32,
    replay: u32,
    max_ranges_per_pass: usize,
    nesting_levels: u16,
    min_nesting_level: u16,
    pass_index: usize,
    target_nesting_level: u16,
}

#[repr(C)]
struct Decode {
    struct_size: usize,
    priv_: usize,
    object: *mut c_void,
    ranges_dropped: usize,
}

#[repr(C)]
struct CounterDataSize {
    struct_size: usize,
    priv_: usize,
    object: *mut c_void,
    names: *const *const c_char,
    count: usize,
    max_ranges: usize,
    max_tree_nodes: u32,
    size: usize,
}

#[repr(C)]
struct ImageInitialize {
    struct_size: usize,
    priv_: usize,
    object: *mut c_void,
    size: usize,
    image: *mut u8,
}

#[repr(C)]
struct CounterDataInfo {
    struct_size: usize,
    priv_: usize,
    image: *const u8,
    size: usize,
    ranges: usize,
}

/// CUPTI's entry points this module calls, resolved once at arm.
struct Api {
    describe: &'static Cupti,
    initialize: ParamsFn,
    device_supported: ParamsFn,
    chip_name: ParamsFn,
    availability: ParamsFn,
    host_initialize: ParamsFn,
    host_deinitialize: ParamsFn,
    add_metrics: ParamsFn,
    config_size: ParamsFn,
    config_image: ParamsFn,
    passes: ParamsFn,
    evaluate: ParamsFn,
    enable: ParamsFn,
    disable: ParamsFn,
    start: ParamsFn,
    stop: ParamsFn,
    set_config: ParamsFn,
    decode: ParamsFn,
    data_size: ParamsFn,
    image_initialize: ParamsFn,
    data_info: ParamsFn,
    device_id: DeviceIdFn,
    subscribe: SubscribeFn,
    enable_callback: EnableCallbackFn,
}

// SAFETY: function pointers into a library that is never unloaded.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

static API: OnceLock<Api> = OnceLock::new();
static STATE: Mutex<State> = Mutex::new(State {
    metrics: Vec::new(),
    contexts: Vec::new(),
});

struct State {
    metrics: Vec<CString>,
    contexts: Vec<Context>,
}

/// The range profiler on one context.
struct Context {
    ctx: usize,
    device: u32,
    /// The metrics CUPTI accepted, the host object that evaluates them, and
    /// their interned names.
    metrics: Vec<CString>,
    metric_ids: Vec<u32>,
    evaluator: usize,
    object: usize,
    image: Vec<u8>,
    /// The config image CUPTI was handed: it must outlive the session.
    config: Vec<u8>,
    /// Launches since the image was last decoded, in order.
    pending: VecDeque<Launch>,
    /// Where [`wire::WITNESS_METRIC`] is among the metrics, when the chip
    /// has it.
    witness: Option<usize>,
    /// Set up and not torn down.
    active: bool,
}

// SAFETY: the context and objects are opaque handles CUPTI hands out, used
// only under `STATE`'s lock.
unsafe impl Send for Context {}

/// One launch, as its call said it: the kernel's interned name, CUPTI's
/// correlation id, and the blocks of its grid (zero when the call's
/// parameters were not read).
#[derive(Debug, Clone, Copy)]
struct Launch {
    kernel: u32,
    correlation: u32,
    blocks: u64,
}

/// `cuLaunchKernel`, `cuLaunchCooperativeKernel` and their per-thread
/// stream forms take the function, then the grid's three dimensions;
/// `cuLaunchKernelEx` takes a `CUlaunchConfig` whose first three fields are
/// the grid's (`cuda.h`, `generated_cuda_meta.h`).
const LAUNCH_KERNEL_EX: [u32; 2] = [652, 653];

/// The blocks of a launch's grid, from its call's parameters.
///
/// # Safety
/// `params` is the parameter block of the launch entry point `id`.
unsafe fn grid_blocks(id: u32, params: *const c_void) -> u64 {
    if params.is_null() {
        return 0;
    }
    let grid: *const u32 = if LAUNCH_KERNEL_EX.contains(&id) {
        let config: *const c_void = read(params, 0);
        if config.is_null() {
            return 0;
        }
        config.cast()
    } else {
        params.cast::<u8>().add(8).cast()
    };
    (0..3)
        .map(|axis| u64::from(std::ptr::read_unaligned(grid.add(axis))))
        .product()
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
        initialize: symbol!("cuptiProfilerInitialize"),
        device_supported: symbol!("cuptiProfilerDeviceSupported"),
        chip_name: symbol!("cuptiDeviceGetChipName"),
        availability: symbol!("cuptiProfilerGetCounterAvailability"),
        host_initialize: symbol!("cuptiProfilerHostInitialize"),
        host_deinitialize: symbol!("cuptiProfilerHostDeinitialize"),
        add_metrics: symbol!("cuptiProfilerHostConfigAddMetrics"),
        config_size: symbol!("cuptiProfilerHostGetConfigImageSize"),
        config_image: symbol!("cuptiProfilerHostGetConfigImage"),
        passes: symbol!("cuptiProfilerHostGetNumOfPasses"),
        evaluate: symbol!("cuptiProfilerHostEvaluateToGpuValues"),
        enable: symbol!("cuptiRangeProfilerEnable"),
        disable: symbol!("cuptiRangeProfilerDisable"),
        start: symbol!("cuptiRangeProfilerStart"),
        stop: symbol!("cuptiRangeProfilerStop"),
        set_config: symbol!("cuptiRangeProfilerSetConfig"),
        decode: symbol!("cuptiRangeProfilerDecodeData"),
        data_size: symbol!("cuptiRangeProfilerGetCounterDataSize"),
        image_initialize: symbol!("cuptiRangeProfilerCounterDataImageInitialize"),
        data_info: symbol!("cuptiRangeProfilerGetCounterDataInfo"),
        device_id: symbol!("cuptiGetDeviceId", DeviceIdFn),
        subscribe: symbol!("cuptiSubscribe", SubscribeFn),
        enable_callback: symbol!("cuptiEnableCallback", EnableCallbackFn),
    })
}

impl Api {
    /// Calls an entry point that takes a parameter block, naming it in the
    /// error.
    fn call<T>(&self, what: &str, function: ParamsFn, params: &mut T) -> Result<(), String> {
        // SAFETY: `params` is the `#[repr(C)]` block `function` takes, its
        // size field set by the caller.
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

/// The metrics asked: [`wire::METRICS_VARIABLE`], or the defaults.
fn metrics_asked() -> Vec<CString> {
    let asked = std::env::var(wire::METRICS_VARIABLE).unwrap_or_default();
    let named: Vec<&str> = asked
        .split([',', ' ', ';'])
        .filter(|name| !name.is_empty())
        .collect();
    let mut names: Vec<&str> = if named.is_empty() {
        wire::DEFAULT_METRICS.to_vec()
    } else {
        named
    };
    // The witness of every launch's window, whatever else is asked.
    if !names.contains(&wire::WITNESS_METRIC) {
        names.push(wire::WITNESS_METRIC);
    }
    names
        .into_iter()
        .filter_map(|name| CString::new(name).ok())
        .collect()
}

/// Called from `InitializeInjection`, before any context exists: resolves
/// the entry points, initialises the profiler, and subscribes to context
/// creation and kernel launches.
pub fn arm(cupti: &'static Cupti) -> Result<(), String> {
    let api = resolve(cupti)?;
    let api = API.get_or_init(|| api);
    let mut initialize = Plain {
        struct_size: struct_size!(Plain, priv_, usize),
        priv_: 0,
    };
    api.call("cuptiProfilerInitialize", api.initialize, &mut initialize)?;
    lock().metrics = metrics_asked();
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
    for launch in LAUNCHES {
        enable(DOMAIN_DRIVER_API, launch)?;
    }
    Ok(())
}

fn lock() -> std::sync::MutexGuard<'static, State> {
    STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `CUpti_CallbackData`'s fields this module reads.
mod callback_data {
    pub const SITE: usize = 0;
    pub const PARAMS: usize = 16;
    pub const SYMBOL: usize = 32;
    pub const CONTEXT: usize = 40;
    pub const CORRELATION: usize = 64;
}

/// # Safety
/// `data` points at a block at least `offset + size_of::<T>()` long.
pub(crate) unsafe fn read<T: Copy>(data: *const c_void, offset: usize) -> T {
    std::ptr::read_unaligned(data.cast::<u8>().add(offset).cast::<T>())
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
            let metrics = state.metrics.clone();
            match set_up(api, ctx, metrics) {
                Ok(context) => state.contexts.push(context),
                Err(reason) => crate::error(&format!(
                    "gpu.counters: the range profiler could not be set up on a context, so \
                     none of its launches is measured: {reason}"
                )),
            }
        }
        (DOMAIN_RESOURCE, CONTEXT_DESTROY_STARTING) => {
            // SAFETY: as above.
            let ctx: *mut c_void = unsafe { read(data, 0) };
            let mut state = lock();
            if let Some(context) = state.contexts.iter_mut().find(|c| c.ctx == ctx as usize) {
                tear_down(api, context);
            }
            crate::send_now();
        }
        (DOMAIN_DRIVER_API, launch) if LAUNCHES.contains(&launch) => {
            // SAFETY: a `CUpti_CallbackData`, read within its 72 bytes.
            let (site, params, symbol, ctx, correlation) = unsafe {
                (
                    read::<u32>(data, callback_data::SITE),
                    read::<*const c_void>(data, callback_data::PARAMS),
                    read::<*const c_char>(data, callback_data::SYMBOL),
                    read::<*mut c_void>(data, callback_data::CONTEXT),
                    read::<u32>(data, callback_data::CORRELATION),
                )
            };
            if site != API_EXIT {
                return;
            }
            // SAFETY: the parameter block of this very launch entry point,
            // valid for the callback.
            let blocks = unsafe { grid_blocks(launch, params) };
            let name = if symbol.is_null() {
                b"?".to_vec()
            } else {
                // SAFETY: a nul-terminated name CUPTI owns for the call.
                unsafe { CStr::from_ptr(symbol) }.to_bytes().to_vec()
            };
            let mut state = lock();
            let Some(context) = state.contexts.iter_mut().find(|c| c.ctx == ctx as usize) else {
                // A context the profiler could not be set up on, said
                // when it failed: its launches are counted, not measured.
                lost(1, wire::LOST_UNDECODED);
                return;
            };
            if !context.active {
                lost(1, wire::LOST_UNDECODED);
                return;
            }
            let mut id = 0;
            crate::with_channel(|channel| id = channel.name(&name));
            context.pending.push_back(Launch {
                kernel: id,
                correlation,
                blocks,
            });
            if context.pending.len() >= BATCH {
                if let Err(reason) = decode(api, context) {
                    crate::error(&format!("gpu.counters: {reason}"));
                }
            }
        }
        _ => {}
    }
}

fn lost(count: u64, kind: u32) {
    let mut fields = wire::Fields::<16>::default();
    fields.u64(count).u32(kind).u32(0);
    crate::with_channel(|channel| channel.frame(wire::OP_INSIDE_LOST, fields.as_slice()));
}

fn names(metrics: &[CString]) -> Vec<*const c_char> {
    metrics.iter().map(|name| name.as_ptr()).collect()
}

fn host(api: &Api, chip: *const c_char, availability: &[u8]) -> Result<*mut c_void, String> {
    let mut params = HostInitialize {
        struct_size: struct_size!(HostInitialize, single_pass_set, *const c_char),
        priv_: 0,
        profiler_type: RANGE_PROFILER,
        chip,
        availability: availability.as_ptr(),
        host: std::ptr::null_mut(),
        single_pass_set: std::ptr::null(),
    };
    api.call(
        "cuptiProfilerHostInitialize",
        api.host_initialize,
        &mut params,
    )?;
    Ok(params.host)
}

fn release_host(api: &Api, host: *mut c_void) {
    let mut params = HostObject {
        struct_size: struct_size!(HostObject, host, *mut c_void),
        priv_: 0,
        host,
    };
    // A failure here leaks a host object at teardown; nothing to report.
    let _ = api.call(
        "cuptiProfilerHostDeinitialize",
        api.host_deinitialize,
        &mut params,
    );
}

fn add_metrics(api: &Api, host: *mut c_void, metrics: &[CString]) -> Result<(), String> {
    let pointers = names(metrics);
    let mut params = AddMetrics {
        struct_size: struct_size!(AddMetrics, count, usize),
        priv_: 0,
        host,
        names: pointers.as_ptr(),
        count: pointers.len(),
    };
    api.call(
        "cuptiProfilerHostConfigAddMetrics",
        api.add_metrics,
        &mut params,
    )
}

/// The metrics this chip accepts, each tried alone when the whole list is
/// refused, and the ones it does not, named.
fn accepted(
    api: &Api,
    chip: *const c_char,
    availability: &[u8],
    metrics: Vec<CString>,
) -> Result<(Vec<CString>, Vec<String>), String> {
    let probe = host(api, chip, availability)?;
    let all = add_metrics(api, probe, &metrics);
    release_host(api, probe);
    if all.is_ok() {
        return Ok((metrics, Vec::new()));
    }
    let mut kept = Vec::new();
    let mut refused = Vec::new();
    for metric in metrics {
        let probe = host(api, chip, availability)?;
        let one = add_metrics(api, probe, std::slice::from_ref(&metric));
        release_host(api, probe);
        match one {
            Ok(()) => kept.push(metric),
            Err(_) => refused.push(metric.to_string_lossy().into_owned()),
        }
    }
    Ok((kept, refused))
}

fn set_up(api: &Api, ctx: *mut c_void, metrics: Vec<CString>) -> Result<Context, String> {
    let mut device = 0u32;
    // SAFETY: CUPTI writes one u32 we own.
    api.check("cuptiGetDeviceId", unsafe {
        (api.device_id)(ctx, &mut device)
    })?;

    let mut support = DeviceSupported {
        struct_size: struct_size!(DeviceSupported, sku, u32),
        device: device as i32,
        api: API_RANGE_PROFILING,
        ..Default::default()
    };
    api.call(
        "cuptiProfilerDeviceSupported",
        api.device_supported,
        &mut support,
    )?;
    if support.is_supported != SUPPORTED {
        let parts = [
            ("the architecture", support.architecture),
            ("SLI", support.sli),
            ("vGPU", support.vgpu),
            ("confidential computing", support.confidential_compute),
            ("a mining processor", support.cmp),
            ("WSL", support.wsl),
            ("the SKU", support.sku),
        ];
        let why: Vec<&str> = parts
            .iter()
            .filter(|(_, level)| *level != SUPPORTED)
            .map(|(what, _)| *what)
            .collect();
        return Err(format!(
            "device {device} does not support CUPTI's range profiler (not supported: {})",
            why.join(", ")
        ));
    }

    let mut chip = ChipName {
        struct_size: struct_size!(ChipName, chip, *const c_char),
        priv_: 0,
        device_index: device as usize,
        chip: std::ptr::null(),
    };
    api.call("cuptiDeviceGetChipName", api.chip_name, &mut chip)?;

    let mut availability = CounterAvailability {
        struct_size: struct_size!(CounterAvailability, device_level, u8),
        priv_: 0,
        ctx,
        image_size: 0,
        image: std::ptr::null_mut(),
        device_level: 0,
    };
    api.call(
        "cuptiProfilerGetCounterAvailability",
        api.availability,
        &mut availability,
    )?;
    let mut available = vec![0u8; availability.image_size];
    availability.image = available.as_mut_ptr();
    api.call(
        "cuptiProfilerGetCounterAvailability",
        api.availability,
        &mut availability,
    )?;

    let (metrics, refused) = accepted(api, chip.chip, &available, metrics)?;
    if !refused.is_empty() {
        crate::error(&format!(
            "gpu.counters: this chip has no counter named {}; the others are measured",
            refused.join(", ")
        ));
    }
    if metrics.is_empty() {
        return Err("none of the counters asked exists on this chip".to_owned());
    }

    // The configuration: built on a host object of its own, as CUPTI's
    // samples do, and released once the image is out.
    let scheduler = host(api, chip.chip, &available)?;
    let built = config_image(api, scheduler, &metrics);
    release_host(api, scheduler);
    let mut config = built?;
    let mut passes = NumOfPasses {
        struct_size: struct_size!(NumOfPasses, passes, usize),
        priv_: 0,
        size: config.len(),
        image: config.as_mut_ptr(),
        passes: 0,
    };
    api.call("cuptiProfilerHostGetNumOfPasses", api.passes, &mut passes)?;
    let evaluator = host(api, chip.chip, &available)?;

    let mut enable = Enable {
        struct_size: struct_size!(Enable, cache_control, u32),
        priv_: 0,
        ctx,
        object: std::ptr::null_mut(),
        // No cache control: a replay pass sees the cache the pass before it
        // left, as the program's own launches do. Whether a counter is then
        // reproducible is what the verdict measures.
        cache_control: 0,
    };
    api.call("cuptiRangeProfilerEnable", api.enable, &mut enable)?;
    let object = enable.object;

    let pointers = names(&metrics);
    let mut size = CounterDataSize {
        struct_size: struct_size!(CounterDataSize, size, usize),
        priv_: 0,
        object,
        names: pointers.as_ptr(),
        count: pointers.len(),
        max_ranges: RANGES,
        max_tree_nodes: RANGES as u32,
        size: 0,
    };
    api.call(
        "cuptiRangeProfilerGetCounterDataSize",
        api.data_size,
        &mut size,
    )?;
    let mut image = vec![0u8; size.size];
    initialize_image(api, object, &mut image)?;

    let mut set = SetConfig {
        struct_size: struct_size!(SetConfig, target_nesting_level, u16),
        priv_: 0,
        object,
        config_size: config.len(),
        config: config.as_ptr(),
        image_size: image.len(),
        image: image.as_mut_ptr(),
        range: AUTO_RANGE,
        replay: KERNEL_REPLAY,
        max_ranges_per_pass: RANGES,
        nesting_levels: 1,
        min_nesting_level: 1,
        pass_index: 0,
        target_nesting_level: 0,
    };
    api.call("cuptiRangeProfilerSetConfig", api.set_config, &mut set)?;
    let mut start = Object {
        struct_size: struct_size!(Object, object, *mut c_void),
        priv_: 0,
        object,
    };
    api.call("cuptiRangeProfilerStart", api.start, &mut start)?;

    let mut metric_ids = Vec::new();
    let mut chip_id = 0;
    crate::with_channel(|channel| {
        // SAFETY: a nul-terminated name CUPTI keeps for the process.
        chip_id = channel.name(unsafe { CStr::from_ptr(chip.chip) }.to_bytes());
        for metric in &metrics {
            metric_ids.push(channel.name(metric.as_bytes()));
        }
        let setup = wire::CounterSetup {
            device,
            passes: passes.passes as u32,
            chip: chip_id,
            metrics: metrics.len() as u32,
        };
        channel.frame(wire::OP_COUNTER_SETUP, setup.encode().as_slice());
    });
    crate::send_now();
    let witness = metrics
        .iter()
        .position(|metric| metric.as_bytes() == wire::WITNESS_METRIC.as_bytes());

    Ok(Context {
        ctx: ctx as usize,
        device,
        metrics,
        metric_ids,
        evaluator: evaluator as usize,
        object: object as usize,
        image,
        config,
        pending: VecDeque::new(),
        witness,
        active: true,
    })
}

fn config_image(api: &Api, host: *mut c_void, metrics: &[CString]) -> Result<Vec<u8>, String> {
    add_metrics(api, host, metrics)?;
    let mut size = ConfigImageSize {
        struct_size: struct_size!(ConfigImageSize, size, usize),
        priv_: 0,
        host,
        size: 0,
    };
    api.call(
        "cuptiProfilerHostGetConfigImageSize",
        api.config_size,
        &mut size,
    )?;
    let mut image = vec![0u8; size.size];
    let mut params = ConfigImage {
        struct_size: struct_size!(ConfigImage, image, *mut u8),
        priv_: 0,
        host,
        size: image.len(),
        image: image.as_mut_ptr(),
    };
    api.call(
        "cuptiProfilerHostGetConfigImage",
        api.config_image,
        &mut params,
    )?;
    Ok(image)
}

fn initialize_image(api: &Api, object: *mut c_void, image: &mut [u8]) -> Result<(), String> {
    let mut params = ImageInitialize {
        struct_size: struct_size!(ImageInitialize, image, *mut u8),
        priv_: 0,
        object,
        size: image.len(),
        image: image.as_mut_ptr(),
    };
    api.call(
        "cuptiRangeProfilerCounterDataImageInitialize",
        api.image_initialize,
        &mut params,
    )
}

/// Decodes what the image holds, sends each launch's counters, and starts
/// the image over. Whatever happens, the launches pending are accounted
/// for: measured, or counted as lost with the reason.
fn decode(api: &Api, context: &mut Context) -> Result<(), String> {
    let pending = std::mem::take(&mut context.pending);
    let result = decode_into(api, context, &pending);
    if result.is_err() {
        lost(pending.len() as u64, wire::LOST_UNDECODED);
    }
    crate::send_now();
    result
}

fn decode_into(api: &Api, context: &mut Context, pending: &VecDeque<Launch>) -> Result<(), String> {
    let object = context.object as *mut c_void;
    let mut decode = Decode {
        struct_size: struct_size!(Decode, ranges_dropped, usize),
        priv_: 0,
        object,
        ranges_dropped: 0,
    };
    api.call("cuptiRangeProfilerDecodeData", api.decode, &mut decode)?;
    if decode.ranges_dropped > 0 {
        lost(decode.ranges_dropped as u64, wire::LOST_RANGES);
    }
    let mut info = CounterDataInfo {
        struct_size: struct_size!(CounterDataInfo, ranges, usize),
        priv_: 0,
        image: context.image.as_ptr(),
        size: context.image.len(),
        ranges: 0,
    };
    api.call(
        "cuptiRangeProfilerGetCounterDataInfo",
        api.data_info,
        &mut info,
    )?;
    let pointers = names(&context.metrics);
    let mut values = vec![0f64; pointers.len()];
    let paired = info.ranges.min(pending.len());
    for (range, launch) in pending.iter().take(paired).enumerate() {
        let mut evaluate = Evaluate {
            struct_size: struct_size!(Evaluate, values, *mut f64),
            priv_: 0,
            host: context.evaluator as *mut c_void,
            image: context.image.as_ptr(),
            image_size: context.image.len(),
            range,
            names: pointers.as_ptr(),
            count: pointers.len(),
            values: values.as_mut_ptr(),
        };
        if let Err(reason) = api.call(
            "cuptiProfilerHostEvaluateToGpuValues",
            api.evaluate,
            &mut evaluate,
        ) {
            lost(1, wire::LOST_UNDECODED);
            crate::error(&format!("gpu.counters: {reason}"));
            continue;
        }
        let witness = match context.witness {
            Some(at) if launch.blocks > 0 => {
                if values[at] == launch.blocks as f64 {
                    wire::WITNESS_CLEAN
                } else {
                    wire::WITNESS_DISTURBED
                }
            }
            _ => wire::WITNESS_UNKNOWN,
        };
        crate::with_channel(|channel| {
            for (metric, value) in context.metric_ids.iter().zip(&values) {
                let counter = wire::Counter {
                    kernel: launch.kernel,
                    metric: *metric,
                    correlation: launch.correlation,
                    device: context.device,
                    value: *value,
                    witness,
                    pad: 0,
                };
                channel.frame(wire::OP_COUNTER, counter.encode().as_slice());
            }
        });
    }
    let unpaired = info.ranges.abs_diff(pending.len());
    if unpaired > 0 {
        lost(unpaired as u64, wire::LOST_UNPAIRED);
    }
    initialize_image(api, object, &mut context.image)
}

/// Decodes what is left on a context, stops and disables its profiler.
fn tear_down(api: &Api, context: &mut Context) {
    if !context.active {
        return;
    }
    context.active = false;
    let object = context.object as *mut c_void;
    let mut stop = Stop {
        struct_size: struct_size!(Stop, all_passes_submitted, u8),
        priv_: 0,
        object,
        pass_index: 0,
        target_nesting_level: 0,
        all_passes_submitted: 0,
    };
    if let Err(reason) = api.call("cuptiRangeProfilerStop", api.stop, &mut stop) {
        crate::error(&format!("gpu.counters: {reason}"));
    }
    if let Err(reason) = decode(api, context) {
        crate::error(&format!("gpu.counters: {reason}"));
    }
    let mut disable = Object {
        struct_size: struct_size!(Object, object, *mut c_void),
        priv_: 0,
        object,
    };
    if let Err(reason) = api.call("cuptiRangeProfilerDisable", api.disable, &mut disable) {
        crate::error(&format!("gpu.counters: {reason}"));
    }
    release_host(api, context.evaluator as *mut c_void);
    // The config image may be released once the session is over.
    context.config = Vec::new();
}

/// At exit: everything still pending on every context.
pub fn finish() {
    let Some(api) = API.get() else {
        return;
    };
    let mut state = lock();
    for context in &mut state.contexts {
        tear_down(api, context);
    }
    crate::send_now();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The parameter blocks, against `sizeof` and the `_STRUCT_SIZE` macros
    /// of CUPTI 13.4's headers, measured with a C compiler.
    #[test]
    fn the_parameter_blocks_have_the_sizes_cupti_s_headers_give() {
        assert_eq!(size_of::<SetConfig>(), 96);
        assert_eq!(offset_of!(SetConfig, range), 56);
        assert_eq!(offset_of!(SetConfig, target_nesting_level), 88);
        assert_eq!(size_of::<Enable>(), 40);
        assert_eq!(size_of::<Stop>(), 48);
        assert_eq!(size_of::<Decode>(), 32);
        assert_eq!(size_of::<CounterDataSize>(), 64);
        assert_eq!(offset_of!(CounterDataSize, max_tree_nodes), 48);
        assert_eq!(size_of::<ImageInitialize>(), 40);
        assert_eq!(size_of::<CounterDataInfo>(), 40);
        assert_eq!(size_of::<HostInitialize>(), 56);
        assert_eq!(offset_of!(HostInitialize, single_pass_set), 48);
        assert_eq!(size_of::<AddMetrics>(), 40);
        assert_eq!(size_of::<ConfigImageSize>(), 32);
        assert_eq!(size_of::<ConfigImage>(), 40);
        assert_eq!(size_of::<NumOfPasses>(), 40);
        assert_eq!(size_of::<Evaluate>(), 72);
        assert_eq!(size_of::<CounterAvailability>(), 48);
        assert_eq!(offset_of!(CounterAvailability, device_level), 40);
        assert_eq!(size_of::<DeviceSupported>(), 56);
        assert_eq!(offset_of!(DeviceSupported, api), 48);
        assert_eq!(size_of::<ChipName>(), 32);
        assert_eq!(struct_size!(SetConfig, target_nesting_level, u16), 90);
    }

    /// The defaults, and the witness of every launch's window after them,
    /// whatever was asked.
    #[test]
    fn the_metrics_asked_default_to_instructions_memory_and_cache_and_the_witness() {
        if std::env::var_os(wire::METRICS_VARIABLE).is_some() {
            return;
        }
        let metrics = metrics_asked();
        assert_eq!(metrics.len(), wire::DEFAULT_METRICS.len() + 1);
        assert_eq!(
            metrics.last().unwrap().to_str().unwrap(),
            wire::WITNESS_METRIC
        );
    }

    /// The blocks of a grid, read where each launch entry point keeps them.
    #[test]
    fn a_launch_s_blocks_are_read_from_its_call() {
        // `cuLaunchKernel_params`: the function, then the grid.
        let mut params = [0u32; 8];
        params[2] = 272;
        params[3] = 2;
        params[4] = 1;
        // SAFETY: a block laid out as the entry point's.
        let blocks = unsafe { grid_blocks(307, params.as_ptr().cast()) };
        assert_eq!(blocks, 544);
        // `cuLaunchKernelEx_params`: a pointer to the config, whose first
        // three fields are the grid.
        let config = [68u32, 1, 1, 128, 1, 1];
        let ex: [*const u32; 4] = [
            config.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
        ];
        // SAFETY: as above.
        let blocks = unsafe { grid_blocks(652, ex.as_ptr().cast()) };
        assert_eq!(blocks, 68);
        // SAFETY: a null block is answered, not read.
        assert_eq!(unsafe { grid_blocks(307, std::ptr::null()) }, 0);
    }
}
