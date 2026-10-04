//! The NVIDIA management library, opened at runtime.
//!
//! NVIDIA exposes nothing below its own libraries: there is no DRM scheduler
//! tracepoint and no kernel PMU to read, so a device's clock, temperature,
//! energy and VRAM come from `libnvidia-ml.so` or from nowhere. This module
//! calls that library directly, through `dlopen` and `dlsym`, for the same
//! reason the rest of the project calls `perf_event_open` rather than running
//! `perf`: a tool that shells out to `nvidia-smi` and parses its output has a
//! dependency on a text format nobody promised to keep.
//!
//! Opening at runtime rather than linking keeps a machine without the driver
//! buildable and runnable: the library is a fact about the machine, discovered
//! like every other, and its absence is a probe answer rather than a link
//! error.
//!
//! Every entry point is optional except the handful needed to enumerate
//! devices. A symbol that is missing, or a call that answers "not supported on
//! this device", is reported as such and never filled in with a guess -- an
//! energy counter that Turing has and Kepler does not is exactly the kind of
//! difference that must reach the output.

use std::ffi::{c_char, c_int, c_uint, c_void, CStr, CString};
use std::fmt;

/// `nvmlReturn_t` values this code distinguishes. The rest are carried as their
/// number and rendered through the library's own message.
const NVML_SUCCESS: c_int = 0;
const NVML_ERROR_UNINITIALIZED: c_int = 1;
const NVML_ERROR_NOT_SUPPORTED: c_int = 3;
/// `nvmlDeviceGetSamples` with nothing newer than the timestamp it was given.
const NVML_ERROR_NOT_FOUND: c_int = 6;
/// The buffer was too small, and `count` now holds what it would have needed.
const NVML_ERROR_INSUFFICIENT_SIZE: c_int = 7;
const NVML_ERROR_DRIVER_NOT_LOADED: c_int = 9;
const NVML_ERROR_GPU_IS_LOST: c_int = 15;

/// `nvmlSamplingType_t`: the driver's own samples of the occupancy.
const NVML_GPU_UTILIZATION_SAMPLES: c_int = 1;

/// `nvmlClockType_t`.
const NVML_CLOCK_GRAPHICS: c_int = 0;
const NVML_CLOCK_SM: c_int = 1;
const NVML_CLOCK_MEM: c_int = 2;

/// `nvmlTemperatureSensors_t`: the only sensor the API defines.
const NVML_TEMPERATURE_GPU: c_int = 0;

/// `nvmlClockId_t`: the clock the device is set to run at when an application
/// clock is in force, and the one it runs at when none is.
const NVML_CLOCK_ID_APP_CLOCK_TARGET: c_int = 1;
const NVML_CLOCK_ID_APP_CLOCK_DEFAULT: c_int = 2;

/// `nvmlEnableState_t`.
const NVML_FEATURE_ENABLED: c_uint = 1;

/// `NVML_PERF_MODES_BUFFER_SIZE`: the text the performance-mode entry points
/// write is at most this long, terminator included.
const PERF_MODES_BUFFER: usize = 2048;

/// The clock event reasons NVML defines (`nvmlClocksEventReason*` and the
/// `nvmlClocksThrottleReason*` bits it kept), by bit, under the name a reader
/// sees. A bit this table does not know is reported by its number rather than
/// dropped: a driver newer than this table has reasons this table cannot name.
pub const CLOCK_EVENT_REASONS: [(u64, &str); 11] = [
    (0x1, "gpu_idle"),
    (0x2, "applications_clocks_setting"),
    (0x4, "sw_power_cap"),
    (0x8, "hw_slowdown"),
    (0x10, "sync_boost"),
    (0x20, "sw_thermal_slowdown"),
    (0x40, "hw_thermal_slowdown"),
    (0x80, "hw_power_brake_slowdown"),
    (0x100, "display_clock_setting"),
    (0x200, "board_limit"),
    (0x400, "reliability"),
];

/// The library file names, newest ABI first. `.so.1` is what the driver
/// installs; the unversioned name is only there when the development package
/// is, which is not something to depend on.
const LIBRARY_NAMES: [&str; 2] = ["libnvidia-ml.so.1", "libnvidia-ml.so"];

/// A device handle, owned by the library. Copyable because it is a token, not a
/// resource: it stays valid until `nvmlShutdown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceHandle(*mut c_void);

/// What the library answered when it refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NvmlError {
    pub code: c_int,
    pub message: String,
}

impl NvmlError {
    /// This device cannot do this, which is a fact about the hardware rather
    /// than a failure of the call. It must not be retried and must not be
    /// papered over with a substitute number.
    pub fn is_not_supported(&self) -> bool {
        self.code == NVML_ERROR_NOT_SUPPORTED
    }

    /// The device or the driver went away: the run is over, not degraded.
    pub fn is_gone(&self) -> bool {
        matches!(
            self.code,
            NVML_ERROR_GPU_IS_LOST | NVML_ERROR_DRIVER_NOT_LOADED | NVML_ERROR_UNINITIALIZED
        )
    }
}

impl fmt::Display for NvmlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (NVML code {})", self.message, self.code)
    }
}

/// `nvmlMemory_t`, the v1 layout. The v2 struct exists and is a different
/// shape; this code asks for the v1 entry point by name so the two can never be
/// confused.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Memory {
    pub total: u64,
    pub free: u64,
    pub used: u64,
}

/// `nvmlUtilization_t`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Utilization {
    /// Percent of a driver-chosen window during which at least one kernel was
    /// resident. Not SM occupancy.
    pub gpu: c_uint,
    /// Percent of that window during which device memory was read or written.
    pub memory: c_uint,
}

/// `nvmlSample_t`: the driver's timestamp in microseconds, then an 8-byte
/// union whose type the call reports separately. Only the timestamp is read
/// here, so the union is kept as its bits.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Sample {
    pub timestamp_us: u64,
    value: u64,
}

/// `nvmlProcessInfo_v2_t`, which is what the `_v3` entry points take.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessInfo {
    pub pid: c_uint,
    pub used_gpu_memory: u64,
    pub gpu_instance_id: c_uint,
    pub compute_instance_id: c_uint,
}

/// Which list a process appeared on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessKind {
    Compute,
    Graphics,
}

impl ProcessKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProcessKind::Compute => "compute",
            ProcessKind::Graphics => "graphics",
        }
    }
}

type FnInit = unsafe extern "C" fn() -> c_int;
type FnShutdown = unsafe extern "C" fn() -> c_int;
type FnErrorString = unsafe extern "C" fn(c_int) -> *const c_char;
type FnDeviceCount = unsafe extern "C" fn(*mut c_uint) -> c_int;
type FnHandleByIndex = unsafe extern "C" fn(c_uint, *mut *mut c_void) -> c_int;
type FnText = unsafe extern "C" fn(*mut c_void, *mut c_char, c_uint) -> c_int;
type FnDriverVersion = unsafe extern "C" fn(*mut c_char, c_uint) -> c_int;
type FnClock = unsafe extern "C" fn(*mut c_void, c_int, *mut c_uint) -> c_int;
type FnPower = unsafe extern "C" fn(*mut c_void, *mut c_uint) -> c_int;
type FnEnergy = unsafe extern "C" fn(*mut c_void, *mut u64) -> c_int;
type FnTemperature = unsafe extern "C" fn(*mut c_void, c_int, *mut c_uint) -> c_int;
type FnMemory = unsafe extern "C" fn(*mut c_void, *mut Memory) -> c_int;
type FnUtilization = unsafe extern "C" fn(*mut c_void, *mut Utilization) -> c_int;
type FnProcesses = unsafe extern "C" fn(*mut c_void, *mut c_uint, *mut ProcessInfo) -> c_int;
type FnClockOfId = unsafe extern "C" fn(*mut c_void, c_int, c_int, *mut c_uint) -> c_int;
type FnEnableState = unsafe extern "C" fn(*mut c_void, *mut c_uint) -> c_int;
type FnReasons = unsafe extern "C" fn(*mut c_void, *mut u64) -> c_int;
type FnPerfModes = unsafe extern "C" fn(*mut c_void, *mut PerfModes) -> c_int;
type FnSamples =
    unsafe extern "C" fn(*mut c_void, c_int, u64, *mut c_int, *mut c_uint, *mut Sample) -> c_int;

/// `nvmlDevicePerfModes_v1_t`: a version word, then the text.
#[repr(C)]
pub struct PerfModes {
    version: c_uint,
    text: [c_char; PERF_MODES_BUFFER],
}

impl PerfModes {
    /// `NVML_STRUCT_VERSION(DevicePerfModes, 1)`: the struct's size in the
    /// low bits and the version in the top byte, which is how the library
    /// tells this layout from a later one.
    fn new() -> Self {
        Self {
            version: (std::mem::size_of::<Self>() as c_uint) | (1 << 24),
            text: [0; PERF_MODES_BUFFER],
        }
    }
}

/// The library, loaded and initialised.
pub struct Nvml {
    library: *mut c_void,
    shutdown: FnShutdown,
    error_string: Option<FnErrorString>,
    device_count: FnDeviceCount,
    handle_by_index: FnHandleByIndex,
    name: Option<FnText>,
    uuid: Option<FnText>,
    driver_version: Option<FnDriverVersion>,
    clock: Option<FnClock>,
    power: Option<FnPower>,
    energy: Option<FnEnergy>,
    temperature: Option<FnTemperature>,
    memory: Option<FnMemory>,
    utilization: Option<FnUtilization>,
    compute_processes: Option<FnProcesses>,
    graphics_processes: Option<FnProcesses>,
    max_clock: Option<FnClock>,
    clock_of_id: Option<FnClockOfId>,
    persistence: Option<FnEnableState>,
    event_reasons: Option<FnReasons>,
    perf_modes: Option<FnPerfModes>,
    samples: Option<FnSamples>,
}

impl Nvml {
    /// Loads the library, or says which names were tried and what the loader
    /// answered.
    pub fn open() -> Result<Self, String> {
        let mut attempts = Vec::new();
        for name in LIBRARY_NAMES {
            match Self::open_named(name) {
                Ok(nvml) => return Ok(nvml),
                Err(reason) => attempts.push(format!("{name}: {reason}")),
            }
        }
        Err(format!(
            "the NVIDIA management library could not be loaded ({})",
            attempts.join("; ")
        ))
    }

    /// Whether the library is there at all, without initialising it. Used to
    /// answer a probe, where loading is the whole question and a running
    /// session is not wanted.
    pub fn present() -> Result<String, String> {
        let mut attempts = Vec::new();
        for name in LIBRARY_NAMES {
            match dlopen_probe(name) {
                Ok(()) => return Ok(name.to_owned()),
                Err(reason) => attempts.push(format!("{name}: {reason}")),
            }
        }
        Err(attempts.join("; "))
    }

    fn open_named(name: &str) -> Result<Self, String> {
        let c_name = CString::new(name).expect("a literal has no interior nul");
        // SAFETY: the name is a valid nul-terminated string. The handle is kept
        // in the returned value and closed in `Drop`.
        let library = unsafe { libc::dlopen(c_name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if library.is_null() {
            return Err(dl_error());
        }

        let required = |symbol: &str| -> Result<*mut c_void, String> {
            // SAFETY: `library` is a live handle from `dlopen`.
            let address = unsafe { lookup(library, symbol) };
            if address.is_null() {
                Err(format!("`{symbol}` is missing from {name}"))
            } else {
                Ok(address)
            }
        };

        // SAFETY: each address was resolved from this library under the name of
        // the entry point whose C signature the type mirrors.
        let nvml = unsafe {
            let init: FnInit = cast(required("nvmlInit_v2").or_else(|_| required("nvmlInit"))?);
            let shutdown: FnShutdown = cast(required("nvmlShutdown")?);
            let device_count: FnDeviceCount = cast(
                required("nvmlDeviceGetCount_v2").or_else(|_| required("nvmlDeviceGetCount"))?,
            );
            let handle_by_index: FnHandleByIndex = cast(
                required("nvmlDeviceGetHandleByIndex_v2")
                    .or_else(|_| required("nvmlDeviceGetHandleByIndex"))?,
            );

            let code = init();
            if code != NVML_SUCCESS {
                let message = error_string_with(lookup(library, "nvmlErrorString"), code);
                libc::dlclose(library);
                return Err(format!("nvmlInit failed: {message} (NVML code {code})"));
            }

            Self {
                library,
                shutdown,
                error_string: optional(library, "nvmlErrorString"),
                device_count,
                handle_by_index,
                name: optional(library, "nvmlDeviceGetName"),
                uuid: optional(library, "nvmlDeviceGetUUID"),
                driver_version: optional(library, "nvmlSystemGetDriverVersion"),
                clock: optional(library, "nvmlDeviceGetClockInfo"),
                power: optional(library, "nvmlDeviceGetPowerUsage"),
                energy: optional(library, "nvmlDeviceGetTotalEnergyConsumption"),
                temperature: optional(library, "nvmlDeviceGetTemperature"),
                memory: optional(library, "nvmlDeviceGetMemoryInfo"),
                utilization: optional(library, "nvmlDeviceGetUtilizationRates"),
                compute_processes: optional(library, "nvmlDeviceGetComputeRunningProcesses_v3")
                    .or_else(|| optional(library, "nvmlDeviceGetComputeRunningProcesses_v2")),
                graphics_processes: optional(library, "nvmlDeviceGetGraphicsRunningProcesses_v3")
                    .or_else(|| optional(library, "nvmlDeviceGetGraphicsRunningProcesses_v2")),
                max_clock: optional(library, "nvmlDeviceGetMaxClockInfo"),
                clock_of_id: optional(library, "nvmlDeviceGetClock"),
                persistence: optional(library, "nvmlDeviceGetPersistenceMode"),
                // The same call under its newer name, then its older one: the
                // driver renamed "throttle" to "event" and kept both.
                event_reasons: optional(library, "nvmlDeviceGetCurrentClocksEventReasons")
                    .or_else(|| optional(library, "nvmlDeviceGetCurrentClocksThrottleReasons")),
                perf_modes: optional(library, "nvmlDeviceGetPerformanceModes"),
                samples: optional(library, "nvmlDeviceGetSamples"),
            }
        };
        Ok(nvml)
    }

    fn error(&self, code: c_int) -> NvmlError {
        NvmlError {
            code,
            message: error_string_with(
                self.error_string
                    .map(|f| f as *mut c_void)
                    .unwrap_or(std::ptr::null_mut()),
                code,
            ),
        }
    }

    fn check(&self, code: c_int) -> Result<(), NvmlError> {
        if code == NVML_SUCCESS {
            Ok(())
        } else {
            Err(self.error(code))
        }
    }

    fn missing(&self, symbol: &'static str) -> NvmlError {
        NvmlError {
            code: NVML_ERROR_NOT_SUPPORTED,
            message: format!("`{symbol}` is not in this build of the NVIDIA management library"),
        }
    }

    pub fn driver_version(&self) -> Option<String> {
        let entry = self.driver_version?;
        let mut buffer = [0 as c_char; 96];
        // SAFETY: the buffer is ours and its length is passed with it.
        let code = unsafe { entry(buffer.as_mut_ptr(), buffer.len() as c_uint) };
        (code == NVML_SUCCESS).then(|| to_string(&buffer))
    }

    pub fn device_count(&self) -> Result<u32, NvmlError> {
        let mut count: c_uint = 0;
        // SAFETY: `count` is a live local.
        self.check(unsafe { (self.device_count)(&mut count) })?;
        Ok(count as u32)
    }

    pub fn device(&self, index: u32) -> Result<DeviceHandle, NvmlError> {
        let mut handle: *mut c_void = std::ptr::null_mut();
        // SAFETY: `handle` is a live local, filled by the library.
        self.check(unsafe { (self.handle_by_index)(index as c_uint, &mut handle) })?;
        Ok(DeviceHandle(handle))
    }

    pub fn device_name(&self, device: DeviceHandle) -> Result<String, NvmlError> {
        let entry = self.name.ok_or_else(|| self.missing("nvmlDeviceGetName"))?;
        let mut buffer = [0 as c_char; 96];
        // SAFETY: the handle came from this library; the buffer is ours.
        self.check(unsafe { entry(device.0, buffer.as_mut_ptr(), buffer.len() as c_uint) })?;
        Ok(to_string(&buffer))
    }

    pub fn device_uuid(&self, device: DeviceHandle) -> Result<String, NvmlError> {
        let entry = self.uuid.ok_or_else(|| self.missing("nvmlDeviceGetUUID"))?;
        let mut buffer = [0 as c_char; 96];
        // SAFETY: the handle came from this library; the buffer is ours.
        self.check(unsafe { entry(device.0, buffer.as_mut_ptr(), buffer.len() as c_uint) })?;
        Ok(to_string(&buffer))
    }

    fn clock_mhz(&self, device: DeviceHandle, domain: c_int) -> Result<u32, NvmlError> {
        let entry = self
            .clock
            .ok_or_else(|| self.missing("nvmlDeviceGetClockInfo"))?;
        let mut mhz: c_uint = 0;
        // SAFETY: the handle came from this library; `mhz` is a live local.
        self.check(unsafe { entry(device.0, domain, &mut mhz) })?;
        Ok(mhz as u32)
    }

    /// The shader clock, which is the one a kernel runs at.
    pub fn sm_clock_mhz(&self, device: DeviceHandle) -> Result<u32, NvmlError> {
        self.clock_mhz(device, NVML_CLOCK_SM)
    }

    pub fn graphics_clock_mhz(&self, device: DeviceHandle) -> Result<u32, NvmlError> {
        self.clock_mhz(device, NVML_CLOCK_GRAPHICS)
    }

    pub fn memory_clock_mhz(&self, device: DeviceHandle) -> Result<u32, NvmlError> {
        self.clock_mhz(device, NVML_CLOCK_MEM)
    }

    /// The highest shader clock the device can run at: the top of the range a
    /// clock reading is judged against.
    pub fn max_sm_clock_mhz(&self, device: DeviceHandle) -> Result<u32, NvmlError> {
        let entry = self
            .max_clock
            .ok_or_else(|| self.missing("nvmlDeviceGetMaxClockInfo"))?;
        let mut mhz: c_uint = 0;
        // SAFETY: the handle came from this library; `mhz` is a live local.
        self.check(unsafe { entry(device.0, NVML_CLOCK_SM, &mut mhz) })?;
        Ok(mhz as u32)
    }

    /// The application clocks of the graphics and memory domains: the target
    /// in force, and the default it is compared with. A target that differs
    /// from its default is a clock someone set. Consumer boards answer "not
    /// supported", which is a fact about the board: nothing can set one there.
    pub fn application_clocks(&self, device: DeviceHandle) -> Result<[(u32, u32); 2], NvmlError> {
        let entry = self
            .clock_of_id
            .ok_or_else(|| self.missing("nvmlDeviceGetClock"))?;
        let mut answer = [(0, 0); 2];
        for (slot, domain) in answer.iter_mut().zip([NVML_CLOCK_GRAPHICS, NVML_CLOCK_MEM]) {
            let (mut target, mut default): (c_uint, c_uint) = (0, 0);
            // SAFETY: the handle came from this library; both outputs are live
            // locals.
            unsafe {
                self.check(entry(
                    device.0,
                    domain,
                    NVML_CLOCK_ID_APP_CLOCK_TARGET,
                    &mut target,
                ))?;
                self.check(entry(
                    device.0,
                    domain,
                    NVML_CLOCK_ID_APP_CLOCK_DEFAULT,
                    &mut default,
                ))?;
            }
            *slot = (target as u32, default as u32);
        }
        Ok(answer)
    }

    /// Whether the driver keeps the device initialised with no client. Off, the
    /// first client after an idle spell pays the device's initialisation, and
    /// a timing taken then includes it.
    pub fn persistence_mode(&self, device: DeviceHandle) -> Result<bool, NvmlError> {
        let entry = self
            .persistence
            .ok_or_else(|| self.missing("nvmlDeviceGetPersistenceMode"))?;
        let mut state: c_uint = 0;
        // SAFETY: the handle came from this library; the output is a live local.
        self.check(unsafe { entry(device.0, &mut state) })?;
        Ok(state == NVML_FEATURE_ENABLED)
    }

    /// Why the clocks are where they are, as the bit mask NVML returns; the
    /// names are in [`CLOCK_EVENT_REASONS`].
    pub fn clock_event_reasons(&self, device: DeviceHandle) -> Result<u64, NvmlError> {
        let entry = self
            .event_reasons
            .ok_or_else(|| self.missing("nvmlDeviceGetCurrentClocksEventReasons"))?;
        let mut reasons: u64 = 0;
        // SAFETY: the handle came from this library; the output is a live local.
        self.check(unsafe { entry(device.0, &mut reasons) })?;
        Ok(reasons)
    }

    /// The device's performance levels and the clock range of each, as the
    /// text the driver writes (`perf=0, nvclock=300, nvclockmin=300, ...;
    /// perf=1, ...`). [`super::perf_levels`] reads it.
    pub fn performance_modes(&self, device: DeviceHandle) -> Result<String, NvmlError> {
        let entry = self
            .perf_modes
            .ok_or_else(|| self.missing("nvmlDeviceGetPerformanceModes"))?;
        let mut modes = PerfModes::new();
        // SAFETY: the handle came from this library; the struct is the v1
        // layout and carries its own version word.
        self.check(unsafe { entry(device.0, &mut modes) })?;
        // A text that filled the buffer to the last byte has no terminator of
        // its own; this one bounds the read either way.
        modes.text[PERF_MODES_BUFFER - 1] = 0;
        Ok(to_string(&modes.text))
    }

    /// Instantaneous draw, in milliwatts, as the driver last sampled it.
    pub fn power_milliwatts(&self, device: DeviceHandle) -> Result<u32, NvmlError> {
        let entry = self
            .power
            .ok_or_else(|| self.missing("nvmlDeviceGetPowerUsage"))?;
        let mut milliwatts: c_uint = 0;
        // SAFETY: the handle came from this library; the output is a live local.
        self.check(unsafe { entry(device.0, &mut milliwatts) })?;
        Ok(milliwatts as u32)
    }

    /// The device's own energy counter, in millijoules since the driver was
    /// loaded. Volta and later; earlier devices answer "not supported", and
    /// that answer is passed on rather than replaced by an integral of sampled
    /// power.
    pub fn energy_millijoules(&self, device: DeviceHandle) -> Result<u64, NvmlError> {
        let entry = self
            .energy
            .ok_or_else(|| self.missing("nvmlDeviceGetTotalEnergyConsumption"))?;
        let mut millijoules: u64 = 0;
        // SAFETY: the handle came from this library; the output is a live local.
        self.check(unsafe { entry(device.0, &mut millijoules) })?;
        Ok(millijoules)
    }

    pub fn temperature_celsius(&self, device: DeviceHandle) -> Result<u32, NvmlError> {
        let entry = self
            .temperature
            .ok_or_else(|| self.missing("nvmlDeviceGetTemperature"))?;
        let mut celsius: c_uint = 0;
        // SAFETY: the handle came from this library; the output is a live local.
        self.check(unsafe { entry(device.0, NVML_TEMPERATURE_GPU, &mut celsius) })?;
        Ok(celsius as u32)
    }

    pub fn memory(&self, device: DeviceHandle) -> Result<Memory, NvmlError> {
        let entry = self
            .memory
            .ok_or_else(|| self.missing("nvmlDeviceGetMemoryInfo"))?;
        let mut memory = Memory::default();
        // SAFETY: the handle came from this library; the struct matches the v1
        // layout of the entry point asked for by name.
        self.check(unsafe { entry(device.0, &mut memory) })?;
        Ok(memory)
    }

    pub fn utilization(&self, device: DeviceHandle) -> Result<Utilization, NvmlError> {
        let entry = self
            .utilization
            .ok_or_else(|| self.missing("nvmlDeviceGetUtilizationRates"))?;
        let mut utilization = Utilization::default();
        // SAFETY: the handle came from this library; the struct matches
        // `nvmlUtilization_t`.
        self.check(unsafe { entry(device.0, &mut utilization) })?;
        Ok(utilization)
    }

    /// The timestamp, in the driver's microseconds, of the newest occupancy
    /// sample the driver took after `after_us`; `None` when it took none since.
    ///
    /// This is what tells a refreshed occupancy from a repeated one: the
    /// figure itself may read the same twice because the board is steadily
    /// idle or steadily busy, but the driver's sample timestamp only moves
    /// when the driver sampled. `after_us` of 0 asks for the whole buffer the
    /// driver keeps, which is how a first reading finds the newest sample.
    pub fn newest_occupancy_sample_us(
        &self,
        device: DeviceHandle,
        after_us: u64,
    ) -> Result<Option<u64>, NvmlError> {
        let entry = self
            .samples
            .ok_or_else(|| self.missing("nvmlDeviceGetSamples"))?;
        let mut value_type: c_int = 0;
        let mut count: c_uint = 0;
        // SAFETY: a null buffer is how the API is asked for the number of
        // samples it would return; both outputs are live locals.
        let code = unsafe {
            entry(
                device.0,
                NVML_GPU_UTILIZATION_SAMPLES,
                after_us,
                &mut value_type,
                &mut count,
                std::ptr::null_mut(),
            )
        };
        if code == NVML_ERROR_NOT_FOUND {
            return Ok(None);
        }
        self.check(code)?;
        if count == 0 {
            return Ok(None);
        }
        let mut buffer = vec![Sample::default(); count as usize];
        let mut written = count;
        // SAFETY: the buffer holds `count` samples, the number the library
        // just asked for, and `written` says so.
        let code = unsafe {
            entry(
                device.0,
                NVML_GPU_UTILIZATION_SAMPLES,
                after_us,
                &mut value_type,
                &mut written,
                buffer.as_mut_ptr(),
            )
        };
        if code == NVML_ERROR_NOT_FOUND {
            return Ok(None);
        }
        self.check(code)?;
        buffer.truncate((written as usize).min(count as usize));
        Ok(buffer
            .iter()
            .map(|sample| sample.timestamp_us)
            .filter(|&ts| ts > after_us)
            .max())
    }

    /// The processes holding memory on a device, compute and graphics both.
    ///
    /// The two lists are separate in the API and a process can be on either, so
    /// both are asked for and what each one answered is kept: a graphics client
    /// and a CUDA client hold VRAM for different reasons.
    pub fn processes(
        &self,
        device: DeviceHandle,
    ) -> Result<Vec<(ProcessKind, ProcessInfo)>, NvmlError> {
        let mut all = Vec::new();
        for (kind, entry) in [
            (ProcessKind::Compute, self.compute_processes),
            (ProcessKind::Graphics, self.graphics_processes),
        ] {
            let Some(entry) = entry else {
                continue;
            };
            for process in self.process_list(device, entry)? {
                all.push((kind, process));
            }
        }
        Ok(all)
    }

    /// The processes on a device, asked for in the two steps the API demands.
    ///
    /// **The two steps race, and losing the race is ordinary rather than
    /// exceptional.** The first call reports how many entries the second will
    /// need; between them a process can start using the device, and the second
    /// then answers `NVML_ERROR_INSUFFICIENT_SIZE` with `count` raised to what
    /// it now needs. Measured under a suite of thirteen hundred tests sharing
    /// one board, that happened about once a run -- reported as a failed read,
    /// which is what it was: a reading this collector did not take.
    ///
    /// So the answer is to take it. The loop retries with the size the library
    /// just asked for, which is the number it computed rather than a guess, and
    /// gives up after a bounded number of attempts so that a device whose
    /// process list is genuinely churning faster than it can be read reports a
    /// failed read rather than spinning. A device with a stable list settles on
    /// the first retry, and one that never settles says so.
    fn process_list(
        &self,
        device: DeviceHandle,
        entry: FnProcesses,
    ) -> Result<Vec<ProcessInfo>, NvmlError> {
        /// Enough for a list that grew while being read, not enough to spin.
        const ATTEMPTS: usize = 4;

        let mut count: c_uint = 0;
        // SAFETY: a null buffer with a zero count is how the API is asked for a
        // size.
        let code = unsafe { entry(device.0, &mut count, std::ptr::null_mut()) };
        if code == NVML_SUCCESS || count == 0 {
            return Ok(Vec::new());
        }

        let mut last = NVML_ERROR_INSUFFICIENT_SIZE;
        for _ in 0..ATTEMPTS {
            let mut buffer = vec![ProcessInfo::default(); count as usize];
            let mut written = count;
            // SAFETY: the buffer holds `count` entries, which is what the
            // library just asked for.
            last = unsafe { entry(device.0, &mut written, buffer.as_mut_ptr()) };
            if last == NVML_SUCCESS {
                buffer.truncate(written as usize);
                return Ok(buffer);
            }
            if last != NVML_ERROR_INSUFFICIENT_SIZE {
                break;
            }
            // `written` now holds what the next attempt needs. Growing it by
            // one more leaves room for the next arrival rather than racing the
            // same process twice.
            count = written.saturating_add(1);
        }
        self.check(last)?;
        Ok(Vec::new())
    }
}

impl Drop for Nvml {
    fn drop(&mut self) {
        // SAFETY: both calls take the handles this value owns, and it is being
        // destroyed.
        unsafe {
            (self.shutdown)();
            libc::dlclose(self.library);
        }
    }
}

/// # Safety
/// `library` must be a live handle returned by `dlopen`.
pub(super) unsafe fn lookup(library: *mut c_void, symbol: &str) -> *mut c_void {
    let name = CString::new(symbol).expect("a symbol name has no interior nul");
    libc::dlsym(library, name.as_ptr())
}

/// # Safety
/// `library` must be a live handle, and `T` must be the C signature of the
/// symbol being asked for.
unsafe fn optional<T: Copy>(library: *mut c_void, symbol: &str) -> Option<T> {
    let address = lookup(library, symbol);
    (!address.is_null()).then(|| cast(address))
}

/// # Safety
/// `T` must be the function pointer type matching the symbol `address` came
/// from.
pub(super) unsafe fn cast<T: Copy>(address: *mut c_void) -> T {
    assert_eq!(
        std::mem::size_of::<T>(),
        std::mem::size_of::<*mut c_void>(),
        "a function pointer is the size of a data pointer on every platform this builds for"
    );
    std::mem::transmute_copy(&address)
}

/// The library's own message for a code, when the library is there to ask.
fn error_string_with(entry: *mut c_void, code: c_int) -> String {
    if entry.is_null() {
        return "the library reported a failure".to_owned();
    }
    // SAFETY: `entry` is `nvmlErrorString`, which returns a static string.
    unsafe {
        let function: FnErrorString = cast(entry);
        let text = function(code);
        if text.is_null() {
            "the library reported a failure".to_owned()
        } else {
            CStr::from_ptr(text).to_string_lossy().into_owned()
        }
    }
}

/// Whether the dynamic loader can find a library, closing it again at once.
///
/// This is what a requirement such as `nvml` or `cupti` actually asks: not
/// whether a vendor SDK is installed somewhere, but whether this process could
/// call it if it wanted to.
pub fn dlopen_probe(name: &str) -> Result<(), String> {
    let c_name = CString::new(name).map_err(|_| format!("`{name}` is not a library name"))?;
    // SAFETY: the name is a valid nul-terminated string, and the handle is
    // closed before returning.
    let library = unsafe { libc::dlopen(c_name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if library.is_null() {
        return Err(dl_error());
    }
    // SAFETY: `library` came from `dlopen` and is not used afterwards.
    unsafe { libc::dlclose(library) };
    Ok(())
}

pub(super) fn dl_error() -> String {
    // SAFETY: `dlerror` returns a string owned by the loader, valid until the
    // next call on this thread.
    unsafe {
        let text = libc::dlerror();
        if text.is_null() {
            "the dynamic loader gave no reason".to_owned()
        } else {
            CStr::from_ptr(text).to_string_lossy().into_owned()
        }
    }
}

fn to_string(buffer: &[c_char]) -> String {
    // SAFETY: the library nul-terminates what it writes, and the buffer starts
    // zeroed, so a terminator is always within it.
    unsafe { CStr::from_ptr(buffer.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The structs above are handed to a C library that will write through
    /// them. Their layout is not an implementation detail: a field at the wrong
    /// offset reads someone else's bytes and reports them as a measurement.
    /// These are the layouts of `nvmlMemory_t`, `nvmlUtilization_t` and
    /// `nvmlProcessInfo_v2_t`, which are what the entry points asked for by
    /// name expect.
    #[test]
    fn the_structs_handed_to_the_library_have_the_layout_it_writes() {
        use std::mem::{align_of, size_of};

        assert_eq!(size_of::<Memory>(), 24);
        assert_eq!(align_of::<Memory>(), 8);

        assert_eq!(size_of::<Utilization>(), 8);
        assert_eq!(align_of::<Utilization>(), 4);
        assert_eq!(size_of::<Sample>(), 16);

        // `{ u32; u64; u32; u32 }` with C padding: the 64-bit field is aligned
        // to 8, which puts the two instance ids at 16 and 20 and the size at
        // 24. Packing it would move every field after the first.
        assert_eq!(size_of::<ProcessInfo>(), 24);
        assert_eq!(align_of::<ProcessInfo>(), 8);
        let process = ProcessInfo::default();
        let base = &process as *const ProcessInfo as usize;
        assert_eq!(&process.pid as *const _ as usize - base, 0);
        assert_eq!(&process.used_gpu_memory as *const _ as usize - base, 8);
        assert_eq!(&process.gpu_instance_id as *const _ as usize - base, 16);
        assert_eq!(&process.compute_instance_id as *const _ as usize - base, 20);
    }

    /// A library that is not there is a fact about the machine, reported with
    /// what the loader said rather than as a generic failure.
    #[test]
    fn a_library_the_loader_cannot_find_is_reported_with_the_loaders_reason() {
        let error = dlopen_probe("libthis-is-not-a-library.so.99")
            .expect_err("that library exists nowhere");
        assert!(!error.is_empty(), "the loader's reason must be carried");
    }

    /// Whichever machine this runs on, the answer is one of the two checked
    /// ones: the library loaded, or it did not and we know what was tried.
    #[test]
    fn the_library_is_either_loadable_or_says_what_was_tried() {
        match Nvml::present() {
            Ok(name) => assert!(name.starts_with("libnvidia-ml")),
            Err(reason) => {
                for candidate in LIBRARY_NAMES {
                    assert!(reason.contains(candidate), "{reason}");
                }
            }
        }
    }

    /// What one call of each kind costs the calling thread, on the device
    /// this runs on. The figures go into the catalog's caveats, so the test
    /// prints them; what it asserts is the separation the caveats rest on --
    /// the reads the driver answers from a sample it keeps are cheap, and the
    /// ones that go to the board are not -- never a threshold, because the
    /// board is shared with whatever else is running.
    ///
    /// The cost is the thread's own cpu time, not its wall time: a call that
    /// slept in the driver would cost the collector nothing, and the caveats
    /// are about the collector's cpu.
    #[test]
    fn what_each_kind_of_call_costs_the_calling_thread() {
        const CALLS: u32 = 40;
        let Ok(nvml) = Nvml::open() else {
            return eprintln!("skipped: no NVIDIA management library here");
        };
        let Ok(device) = nvml.device(0) else {
            return eprintln!("skipped: the NVIDIA management library sees no device");
        };
        // Forty back-to-back reads of the counter and of the draw starve the
        // counter for every reader (defect 7), so no test that holds the
        // counter against the draw may run meanwhile.
        let _energy = super::super::energy_counter_lock();
        let thread_cpu_ns = || {
            let mut now = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: `now` is a live local the kernel fills.
            unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut now) };
            now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64
        };
        let mut table = Vec::new();
        let mut cost = |kind: &'static str, call: &dyn Fn() -> bool| {
            if !call() {
                return eprintln!("{kind}: not answered on this device");
            }
            let (cpu, wall) = (thread_cpu_ns(), crate::clock::monotonic_ns());
            for _ in 0..CALLS {
                std::hint::black_box(call());
            }
            let cpu = (thread_cpu_ns() - cpu) / CALLS as u64;
            let wall = (crate::clock::monotonic_ns() - wall) / CALLS as u64;
            eprintln!(
                "{kind:<24} cpu {:>8.1} us   wall {:>8.1} us",
                cpu as f64 / 1e3,
                wall as f64 / 1e3
            );
            table.push((kind, cpu));
        };
        cost("temperature", &|| nvml.temperature_celsius(device).is_ok());
        cost("memory", &|| nvml.memory(device).is_ok());
        cost("processes", &|| nvml.processes(device).is_ok());
        cost("utilization", &|| nvml.utilization(device).is_ok());
        let newest = nvml
            .newest_occupancy_sample_us(device, 0)
            .ok()
            .flatten()
            .unwrap_or(0);
        cost("occupancy sample", &|| {
            nvml.newest_occupancy_sample_us(device, newest).is_ok()
        });
        cost("clock (one domain)", &|| nvml.sm_clock_mhz(device).is_ok());
        cost("power (instantaneous)", &|| {
            nvml.power_milliwatts(device).is_ok()
        });
        cost("energy counter", &|| {
            nvml.energy_millijoules(device).is_ok()
        });
        cost("persistence mode", &|| {
            nvml.persistence_mode(device).is_ok()
        });
        cost("clock event reasons", &|| {
            nvml.clock_event_reasons(device).is_ok()
        });
        cost("performance levels", &|| {
            nvml.performance_modes(device).is_ok()
        });

        let of = |kind: &str| table.iter().find(|(k, _)| *k == kind).map(|(_, cpu)| *cpu);
        if let (Some(cheap), Some(dear)) = (of("temperature"), of("energy counter")) {
            assert!(
                cheap * 5 < dear,
                "a temperature read ({cheap} ns of cpu) should cost a small fraction of an \
                 energy-counter read ({dear} ns): the caveats say so, and on this board it \
                 did not hold. What else held the device: {}",
                std::process::Command::new("nvidia-smi")
                    .args(["--query-compute-apps=pid,name", "--format=csv,noheader"])
                    .output()
                    .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
                    .unwrap_or_default()
            );
        }
    }

    /// "This device cannot do that" and "the driver went away" lead to
    /// different places: the first is a fact to report, the second ends the run.
    #[test]
    fn an_error_says_whether_it_is_the_device_or_the_run_that_is_over() {
        let unsupported = NvmlError {
            code: NVML_ERROR_NOT_SUPPORTED,
            message: "Not Supported".into(),
        };
        assert!(unsupported.is_not_supported() && !unsupported.is_gone());

        let lost = NvmlError {
            code: NVML_ERROR_GPU_IS_LOST,
            message: "GPU is lost".into(),
        };
        assert!(lost.is_gone() && !lost.is_not_supported());
        assert!(lost.to_string().contains("NVML code 15"), "{lost}");
    }
}
