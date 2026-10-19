//! Collectors for the `gpu` family.
//!
//! The landscape is asymmetric and the code shows it. AMD and Intel expose a
//! DRM scheduler, kernel PMUs and hwmon, so their numbers come from the kernel
//! like every other family's. NVIDIA exposes none of that: below the driver
//! there is nothing to read, so the same numbers come from `libnvidia-ml.so`,
//! opened at runtime. That is the whole of the difference, and it is why this
//! directory has a `vendor` layer no other family needs.

mod api;
mod counters;
mod cupti_api;
mod device;
#[cfg(test)]
mod fixture;
mod job;
mod kernel;
mod nvml;
mod profile;

use serde::{Deserialize, Serialize};

use super::Factory;
use crate::capabilities::Probe;

pub use counters::launch_refusals;

/// The collectors this family provides.
///
/// The sysfs backends of these same sources -- `fs:sys:amdgpu`, `fs:sys:i915`
/// -- are not here yet: they cannot be written honestly on a machine that has
/// no such device to check them against, and an unexercised reader of a sysfs
/// layout is a guess with a file path in it.
pub fn factories() -> Vec<Factory> {
    use device::{Metric, Prepare};
    vec![
        // The one collector in this family that does not go through a vendor
        // library: the DRM scheduler's own tracepoints, which AMD and Intel
        // publish and NVIDIA does not. `docs/families/gpu.md` says what that
        // means for where it has been exercised.
        Factory::new("gpu.job", "bpf:tp", |resolved| {
            Ok(Box::new(job::Prepare {
                mode: resolved.request.mode,
                filter: resolved.request.filter.clone(),
            }))
        }),
        // The host side of submission, through the preload shim the heap and
        // mutex sources share; `api.rs` says which programs it can see.
        Factory::new("gpu.api", "preload", |_| api::prepare()),
        // Every driver API call, through CUPTI's callbacks in the program.
        Factory::new("gpu.api", "vendor:cupti", |_| cupti_api::prepare()),
        // The device's side of the same calls, through CUPTI loaded into the
        // program by the driver; `kernel.rs` says why only at exec.
        Factory::new("gpu.kernel", "vendor:cupti", |_| kernel::prepare()),
        // Inside kernels, through the same library and the same socket:
        // `counters.rs` and `profile.rs` say what each reads.
        Factory::new("gpu.counters", "vendor:cupti", |_| {
            counters::prepare_counters()
        }),
        Factory::new("gpu.profile", "vendor:cupti", |_| {
            counters::prepare_profile()
        }),
        Factory::new("gpu.freq", "vendor:nvml", |resolved| {
            Ok(Box::new(Prepare::new(Metric::Freq, resolved.request.mode)))
        }),
        Factory::new("gpu.power", "vendor:nvml", |resolved| {
            Ok(Box::new(Prepare::new(Metric::Power, resolved.request.mode)))
        }),
        Factory::new("gpu.pcie", "vendor:nvml", |resolved| {
            Ok(Box::new(Prepare::new(Metric::Pcie, resolved.request.mode)))
        }),
        Factory::new("gpu.temp", "vendor:nvml", |resolved| {
            Ok(Box::new(Prepare::new(Metric::Temp, resolved.request.mode)))
        }),
        Factory::new("gpu.mem", "vendor:nvml", |resolved| {
            Ok(Box::new(Prepare::new(Metric::Mem, resolved.request.mode)))
        }),
        Factory::new("gpu.mem_freq", "vendor:nvml", |resolved| {
            Ok(Box::new(Prepare::new(
                Metric::MemFreq,
                resolved.request.mode,
            )))
        }),
        Factory::new("gpu.throttle", "vendor:nvml", |resolved| {
            Ok(Box::new(Prepare::new(
                Metric::Throttle,
                resolved.request.mode,
            )))
        }),
        // The mode reaches this one because `count` has no cadence of its own:
        // without it the run falls back to the default poll interval and a
        // program's exit is noticed a whole second late. See `device.rs`.
        Factory::new("gpu.engine_busy", "vendor:nvml", |resolved| {
            Ok(Box::new(Prepare::new(
                Metric::EngineBusy,
                resolved.request.mode,
            )))
        }),
    ]
}

/// Answers the requirement classes this family introduces.
///
/// `None` means "not mine", so the caller goes on to whatever else it knows.
/// The answers are deliberately narrow: a vendor library is reported present
/// when the dynamic loader can find it, because that is exactly what the
/// requirement says, and the profiling *binaries* -- `ncu`, `rocprofv3` -- stay
/// unanswered rather than being guessed at from a name on `PATH`.
///
/// `Capabilities::probe` calls this before falling back to "no probe is written
/// for that yet", so a `gpu` source reads `yes` or `no` in `omn list` rather
/// than `?`.
pub fn probe(requirement: &str) -> Option<Probe> {
    if let Some(name) = requirement.strip_prefix("hwmon:") {
        return Some(hwmon(name));
    }
    if requirement == "nvidia:profiling" {
        return Some(nvidia_profiling());
    }
    let library = match requirement {
        "nvml" => return Some(nvml_probe()),
        "cupti" => "libcupti.so",
        "rocprofiler" => "librocprofiler64.so",
        "level-zero" => "libze_loader.so",
        _ => return None,
    };
    Some(loadable(requirement, library))
}

/// Whether the NVIDIA driver lets this process read the GPU's performance
/// counters, which CUPTI's profiler and PC sampling use: the
/// `nvidia:profiling` requirement. The driver's default
/// (`RmProfilingAdminOnly: 1` in `/proc/driver/nvidia/params`) keeps them to
/// root and CAP_SYS_ADMIN, and CUPTI then answers
/// CUPTI_ERROR_INSUFFICIENT_PRIVILEGES once the program runs: `gpu.profile`
/// and `gpu.counters` were listed usable and every run of them came back
/// empty, found when CUDA was installed on the development machine.
fn nvidia_profiling() -> Probe {
    let params = match std::fs::read_to_string("/proc/driver/nvidia/params") {
        Ok(params) => params,
        Err(error) => {
            return Probe::Unknown(format!(
                "/proc/driver/nvidia/params cannot be read ({error}), so whether the driver lets \
                 this process read the GPU's performance counters is not known"
            ))
        }
    };
    profiling_allowed(&params, privileged())
}

fn profiling_allowed(params: &str, privileged: bool) -> Probe {
    let admin_only = params
        .lines()
        .find_map(|line| line.strip_prefix("RmProfilingAdminOnly:"))
        .map(|value| value.trim() != "0");
    match admin_only {
        Some(false) => Probe::Available,
        Some(true) if privileged => Probe::Available,
        Some(true) => Probe::Missing(
            "the NVIDIA driver keeps the GPU's performance counters to root and CAP_SYS_ADMIN \
             (`RmProfilingAdminOnly: 1` in /proc/driver/nvidia/params), and CUPTI's profiler \
             would answer CUPTI_ERROR_INSUFFICIENT_PRIVILEGES: `options nvidia \
             NVreg_RestrictProfilingToAdminUsers=0` in /etc/modprobe.d lifts it at the driver's \
             next load"
                .to_owned(),
        ),
        None => Probe::Unknown(
            "/proc/driver/nvidia/params does not say `RmProfilingAdminOnly`, so whether this \
             process may read the GPU's performance counters is not known"
                .to_owned(),
        ),
    }
}

/// Root, or CAP_SYS_ADMIN in this process's effective set.
fn privileged() -> bool {
    if unsafe { libc::geteuid() } == 0 {
        return true;
    }
    const CAP_SYS_ADMIN: u32 = 21;
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("CapEff:"))
                .and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok())
        })
        .is_some_and(|effective| effective & (1 << CAP_SYS_ADMIN) != 0)
}

/// Whether NVML initialises here and sees a device: the `nvml` requirement.
///
/// Loading the library is not the question. A container given the userspace
/// library and no driver, a guest that sees the host's files, a host after a
/// driver upgrade and before its reboot: each loads `libnvidia-ml.so` and then
/// fails `nvmlInit` with "Driver Not Loaded" or a version mismatch, and until
/// the twelfth wave every `vendor:nvml` source was listed usable there and
/// refused at start (P6 in `docs/portability.md`). So the probe initialises it,
/// counts the devices and shuts it down, once per process: `nvmlInit` costs
/// 3 to 5 ms, and it is paid only by a process that asks about a GPU source.
fn nvml_probe() -> Probe {
    static ANSWER: std::sync::OnceLock<Probe> = std::sync::OnceLock::new();
    ANSWER
        .get_or_init(|| {
            if let Err(reason) = nvml::Nvml::present() {
                return Probe::Missing(format!(
                    "the NVIDIA management library is not loadable by this omn ({reason})"
                ));
            }
            let library = match nvml::Nvml::open() {
                Ok(library) => library,
                Err(reason) => {
                    return Probe::Missing(format!(
                        "libnvidia-ml loads and does not initialise, so no NVIDIA driver is \
                         serving it: {reason}"
                    ))
                }
            };
            match library.device_count() {
                Ok(0) => Probe::Missing(
                    "libnvidia-ml initialises and sees no device on this machine".to_owned(),
                ),
                Ok(_) => Probe::Available,
                Err(error) => Probe::Missing(format!(
                    "libnvidia-ml initialises and cannot count its devices: {error}"
                )),
            }
        })
        .clone()
}

fn loadable(requirement: &str, library: &str) -> Probe {
    match nvml::dlopen_probe(library) {
        Ok(()) => Probe::Available,
        Err(reason) => Probe::Missing(format!("`{requirement}` needs {library}: {reason}")),
    }
}

/// A hwmon device is named in a file beside its readings, and the name is what
/// a requirement such as `hwmon:amdgpu` refers to.
fn hwmon(name: &str) -> Probe {
    let Ok(entries) = std::fs::read_dir("/sys/class/hwmon") else {
        return Probe::Missing("/sys/class/hwmon is not readable on this machine".to_owned());
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        if let Ok(text) = std::fs::read_to_string(entry.path().join("name")) {
            let text = text.trim().to_owned();
            if text == name {
                return Probe::Available;
            }
            found.push(text);
        }
    }
    found.sort();
    Probe::Missing(format!(
        "no hwmon device is named `{name}` (this machine has {})",
        if found.is_empty() {
            "none".to_owned()
        } else {
            found.join(", ")
        }
    ))
}

/// The state of a device's clocks, as read and never changed.
///
/// An unlocked GPU runs its shader clock wherever the driver puts it, between
/// an idle floor and a boost ceiling, and where that is depends on what ran
/// before: temperature, the power budget, how long the device has been busy.
/// A kernel timed on such a device is timed on a clock nobody chose. Locking
/// the clocks needs root and does not survive a reboot, so `omn` does not do
/// it: it reads what is set, records it beside the run, and says so on every
/// GPU timing taken while no lock was shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClockState {
    /// The device's index, which is only meaningful within this boot.
    pub device: u32,
    pub device_name: String,
    /// The identity that survives a reboot and a change of index.
    pub device_uuid: String,
    /// Whether the shader clock is held at one frequency.
    pub shader_clock: ClockLock,
    /// The application clocks: the older way of setting a target clock, which
    /// datacenter boards honour and consumer boards refuse.
    pub application_clocks: ApplicationClocks,
    /// Whether the driver keeps the device initialised with no client.
    pub persistence: Persistence,
    /// Why the clocks were where they were when this was read, by the names in
    /// `nvml::CLOCK_EVENT_REASONS`.
    pub event_reasons: EventReasons,
}

/// Whether the shader clock is held at one frequency, and how that is known.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum ClockLock {
    /// One frequency, from `how`: the application clocks in force, or every
    /// performance level of the driver's reduced to a single shader clock.
    Locked { mhz: u32, how: String },
    /// The driver's performance levels let the shader clock range, at the
    /// highest level, from `min_mhz` to `max_mhz` -- narrower than the
    /// device's own range when someone restricted it without pinning it.
    ///
    /// Not "unlocked": NVML has no reader for a lock set with
    /// `nvidia-smi -lgc` (`nvmlDeviceSetGpuLockedClocks`), and that such a lock
    /// shows in these levels is not yet shown. So a range here means the lock
    /// was not shown, which is said as such and never as a free clock.
    NotShownLocked {
        min_mhz: u32,
        max_mhz: u32,
        device_max_mhz: Option<u32>,
    },
    /// This driver gave nothing to decide from; never read as unlocked or as
    /// locked.
    Unknown { reason: String },
}

/// The application clocks, as the driver answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum ApplicationClocks {
    /// A target differs from its default: someone set one.
    Set {
        graphics_mhz: u32,
        memory_mhz: u32,
    },
    /// The targets are the defaults.
    Default,
    /// The board does not have them, which on a consumer board is the answer.
    NotSupported,
    Unreadable {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum Persistence {
    On,
    Off,
    Unreadable { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum EventReasons {
    /// The reasons active when read; empty means the clocks were as high as
    /// the device allows.
    Read {
        active: Vec<String>,
    },
    Unreadable {
        reason: String,
    },
}

impl ClockState {
    /// Reads one device's clock state through an open library.
    pub(crate) fn read(
        nvml: &nvml::Nvml,
        device: nvml::DeviceHandle,
        index: u32,
        name: &str,
        uuid: &str,
    ) -> Self {
        let application_clocks = match nvml.application_clocks(device) {
            Ok([(graphics, graphics_default), (memory, memory_default)]) => {
                if graphics != graphics_default || memory != memory_default {
                    ApplicationClocks::Set {
                        graphics_mhz: graphics,
                        memory_mhz: memory,
                    }
                } else {
                    ApplicationClocks::Default
                }
            }
            Err(error) if error.is_not_supported() => ApplicationClocks::NotSupported,
            Err(error) => ApplicationClocks::Unreadable {
                reason: error.to_string(),
            },
        };
        let device_max_mhz = nvml.max_sm_clock_mhz(device).ok();
        let shader_clock = match &application_clocks {
            ApplicationClocks::Set { graphics_mhz, .. } => ClockLock::Locked {
                mhz: *graphics_mhz,
                how: "application clocks".to_owned(),
            },
            _ => match nvml.performance_modes(device) {
                Ok(text) => shader_lock(&perf_levels(&text), device_max_mhz),
                Err(error) => ClockLock::Unknown {
                    reason: format!("the driver's performance levels are unreadable: {error}"),
                },
            },
        };
        let persistence = match nvml.persistence_mode(device) {
            Ok(true) => Persistence::On,
            Ok(false) => Persistence::Off,
            Err(error) => Persistence::Unreadable {
                reason: error.to_string(),
            },
        };
        let event_reasons = Self::event_reasons(nvml, device);
        Self {
            device: index,
            device_name: name.to_owned(),
            device_uuid: uuid.to_owned(),
            shader_clock,
            application_clocks,
            persistence,
            event_reasons,
        }
    }

    /// Why the clocks are where they are now, alone: the one part of the
    /// state that moves without anyone setting it.
    pub(crate) fn event_reasons(nvml: &nvml::Nvml, device: nvml::DeviceHandle) -> EventReasons {
        match nvml.clock_event_reasons(device) {
            Ok(mask) => EventReasons::Read {
                active: reason_names(mask),
            },
            Err(error) => EventReasons::Unreadable {
                reason: error.to_string(),
            },
        }
    }

    /// `Some(true)` when the shader clock is shown held at one frequency,
    /// `None` otherwise -- which is never counted as locked. Nothing here can
    /// show a clock free (see [`ClockLock::NotShownLocked`]), so `Some(false)`
    /// is never answered; it is kept for the reader that will.
    pub fn locked(&self) -> Option<bool> {
        match self.shader_clock {
            ClockLock::Locked { .. } => Some(true),
            ClockLock::NotShownLocked { .. } | ClockLock::Unknown { .. } => None,
        }
    }

    /// The state in one line, for a run's facts.
    pub fn fact(&self) -> String {
        let clock = match &self.shader_clock {
            ClockLock::Locked { mhz, how } => format!("shader clock locked at {mhz} MHz ({how})"),
            ClockLock::NotShownLocked {
                min_mhz,
                max_mhz,
                device_max_mhz,
            } => {
                let narrowed = match device_max_mhz {
                    Some(top) if top > max_mhz => format!(", below the device's {top} MHz"),
                    _ => String::new(),
                };
                format!(
                    "shader clock not shown locked (the driver's performance levels let it run \
                     from {min_mhz} to {max_mhz} MHz{narrowed}; a lock set with `nvidia-smi -lgc` \
                     is not known to show there)"
                )
            }
            ClockLock::Unknown { reason } => format!("shader clock lock state unknown ({reason})"),
        };
        let application = match &self.application_clocks {
            ApplicationClocks::Set {
                graphics_mhz,
                memory_mhz,
            } => format!("application clocks set to {graphics_mhz}/{memory_mhz} MHz"),
            ApplicationClocks::Default => "application clocks at their defaults".to_owned(),
            ApplicationClocks::NotSupported => {
                "no application clocks (this board does not have them)".to_owned()
            }
            ApplicationClocks::Unreadable { reason } => {
                format!("application clocks unreadable ({reason})")
            }
        };
        let persistence = match &self.persistence {
            Persistence::On => "persistence mode on".to_owned(),
            Persistence::Off => "persistence mode off".to_owned(),
            Persistence::Unreadable { reason } => format!("persistence mode unreadable ({reason})"),
        };
        let reasons = match &self.event_reasons {
            EventReasons::Read { active } if active.is_empty() => {
                "clock event reasons: none".to_owned()
            }
            EventReasons::Read { active } => format!("clock event reasons: {}", active.join(", ")),
            EventReasons::Unreadable { reason } => {
                format!("clock event reasons unreadable ({reason})")
            }
        };
        format!(
            "vendor:nvml: device {} ({}): {clock}, {application}, {persistence}, {reasons}",
            self.device, self.device_name
        )
    }

    /// What a GPU *timing* taken on this device has to say about its clock,
    /// or nothing when the clock was held. Unknown says so too: a timing is
    /// only free of this line when the lock was shown.
    pub fn timing_fact(&self) -> Option<String> {
        let state = match self.locked() {
            Some(true) => return None,
            Some(false) => "unlocked clocks",
            None => "clocks not shown to be locked",
        };
        Some(format!(
            "GPU timings on device {} ({}) were taken on {state}: the shader clock ran wherever \
             the driver put it, so a duration depends on what the device did before it; \
             `omn` reads clock state and never changes it",
            self.device, self.device_name
        ))
    }
}

/// The environment a request of `family` is resolved with: a `gpu`
/// request's carries the devices' clock states, read once a run, or says in
/// `not_captured` why they could not be read. Any other family's is as it was.
pub fn with_clock_states(
    family: &str,
    mut environment: crate::capabilities::Environment,
) -> crate::capabilities::Environment {
    if family != "gpu" {
        return environment;
    }
    static STATES: std::sync::OnceLock<Result<Vec<ClockState>, String>> =
        std::sync::OnceLock::new();
    match STATES.get_or_init(clock_states) {
        Ok(states) => environment.gpu_clocks = states.clone(),
        Err(reason) => environment
            .not_captured
            .push(format!("the GPU clock states: {reason}")),
    }
    environment
}

/// The clock state of every NVIDIA device on this machine, for a collector
/// that does not already hold the library open -- a GPU timing's facts line.
/// Opening it costs `nvmlInit`'s 3 to 5 ms, paid once by a run that asked for
/// a GPU source.
pub fn clock_states() -> Result<Vec<ClockState>, String> {
    let library = nvml::Nvml::open()?;
    let count = library
        .device_count()
        .map_err(|error| format!("cannot count the NVIDIA devices: {error}"))?;
    let mut states = Vec::with_capacity(count as usize);
    for index in 0..count {
        let device = library
            .device(index)
            .map_err(|error| format!("cannot open NVIDIA device {index}: {error}"))?;
        let name = library
            .device_name(device)
            .unwrap_or_else(|_| format!("device {index}"));
        let uuid = library
            .device_uuid(device)
            .unwrap_or_else(|error| format!("unknown ({error})"));
        states.push(ClockState::read(&library, device, index, &name, &uuid));
    }
    Ok(states)
}

/// The lines a GPU timing source puts in its facts: each device's state, and,
/// for each device whose clock was not shown locked, that the timing was taken
/// on a free clock. A machine whose clock state cannot be read says that
/// instead, because silence would read as "nothing to declare".
pub fn timing_clock_facts() -> Vec<String> {
    match clock_states() {
        Ok(states) => states
            .iter()
            .flat_map(|state| std::iter::once(state.fact()).chain(state.timing_fact()))
            .collect(),
        Err(reason) => vec![format!(
            "GPU timings were taken on clocks whose state could not be read: {reason}"
        )],
    }
}

/// One performance level of the driver's, as `nvmlDeviceGetPerformanceModes`
/// writes it: `perf=2, nvclock=300, nvclockmin=300, nvclockmax=2205, ...`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerfLevel {
    pub perf: u32,
    pub shader_min_mhz: u32,
    pub shader_max_mhz: u32,
}

/// Reads the driver's performance-level text. A level without the three
/// fields this needs is skipped rather than guessed at; the caller decides
/// what an empty answer means.
pub fn perf_levels(text: &str) -> Vec<PerfLevel> {
    let mut levels = Vec::new();
    for (position, level) in text.split(';').enumerate() {
        let mut perf = None;
        let mut min = None;
        let mut max = None;
        for pair in level.split(',') {
            let Some((key, value)) = pair.split_once('=') else {
                continue;
            };
            let Ok(value) = value.trim().parse::<u32>() else {
                continue;
            };
            match key.trim() {
                "perf" => perf = Some(value),
                "nvclockmin" => min = Some(value),
                "nvclockmax" => max = Some(value),
                _ => {}
            }
        }
        // The current-level form of the same text has no `perf` token; its
        // position stands in for it.
        if let (Some(min), Some(max)) = (min, max) {
            levels.push(PerfLevel {
                perf: perf.unwrap_or(position as u32),
                shader_min_mhz: min,
                shader_max_mhz: max,
            });
        }
    }
    levels
}

/// The lock verdict from the driver's performance levels: locked when every
/// level runs the shader clock at one and the same frequency, not shown locked
/// otherwise, with the range of the highest level -- the one a busy device
/// runs in.
fn shader_lock(levels: &[PerfLevel], device_max_mhz: Option<u32>) -> ClockLock {
    let Some(top) = levels.iter().max_by_key(|level| level.perf) else {
        return ClockLock::Unknown {
            reason: "the driver listed no performance level with a shader clock range".to_owned(),
        };
    };
    let pinned = levels.iter().all(|level| {
        level.shader_min_mhz == level.shader_max_mhz && level.shader_min_mhz == top.shader_min_mhz
    });
    if pinned {
        ClockLock::Locked {
            mhz: top.shader_min_mhz,
            how: "every performance level runs the shader clock at one frequency".to_owned(),
        }
    } else {
        ClockLock::NotShownLocked {
            min_mhz: top.shader_min_mhz,
            max_mhz: top.shader_max_mhz,
            device_max_mhz,
        }
    }
}

/// The names of the clock event reasons set in `mask`; a bit this build has no
/// name for is kept by its number.
pub fn reason_names(mask: u64) -> Vec<String> {
    let mut names = Vec::new();
    let mut known = 0u64;
    for (bit, name) in nvml::CLOCK_EVENT_REASONS {
        known |= bit;
        if mask & bit != 0 {
            names.push(name.to_owned());
        }
    }
    let unknown = mask & !known;
    if unknown != 0 {
        names.push(format!("unnamed:{unknown:#x}"));
    }
    names
}

/// Held by every in-process test that reads the energy counter or the draw.
/// Reading either fast makes the counter under-count for every reader of the
/// board (`docs/scenarios/gpu.md`, defect 7), so a test that reads them back
/// to back -- the repeat counter, the cost of each call -- and a test that
/// holds the counter against the draw cannot run side by side. One lock for
/// the whole test binary, because `nvml.rs`'s tests and `device.rs`'s share it.
#[cfg(test)]
pub(crate) fn energy_counter_lock() -> std::sync::MutexGuard<'static, ()> {
    static ENERGY: std::sync::Mutex<()> = std::sync::Mutex::new(());
    ENERGY.lock().unwrap_or_else(|held| held.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The driver's profiling restriction is read off its parameters, and
    /// only root or CAP_SYS_ADMIN passes it.
    #[test]
    fn the_driver_s_profiling_restriction_is_a_requirement() {
        let restricted = "ResmanDebugLevel: 4294967295\nRmProfilingAdminOnly: 1\n";
        let open = "RmProfilingAdminOnly: 0\n";
        assert!(matches!(profiling_allowed(restricted, false),
            Probe::Missing(why) if why.contains("NVreg_RestrictProfilingToAdminUsers=0")));
        assert!(matches!(profiling_allowed(restricted, true), Probe::Available));
        assert!(matches!(profiling_allowed(open, false), Probe::Available));
        assert!(matches!(profiling_allowed("", false), Probe::Unknown(_)));
    }

    #[test]
    fn a_requirement_from_another_family_is_left_alone() {
        assert!(probe("tracefs").is_none());
        assert!(probe("pmu:PERF_COUNT_SW_PAGE_FAULTS").is_none());
    }

    /// Whatever this machine has, the answer must be one of the two checked
    /// ones -- never "unverified", which is what a missing probe means.
    #[test]
    fn the_vendor_library_probe_answers_yes_or_no_and_says_which() {
        let probe = probe("nvml").expect("`nvml` is this family's requirement");
        assert!(!probe.is_unknown(), "{probe}");
        if let Probe::Missing(reason) = &probe {
            assert!(reason.contains("libnvidia-ml"), "{reason}");
        }
    }

    /// Available means NVML initialised and counted a device, which is the
    /// first thing every `vendor:nvml` collector does. Checked against the
    /// library itself rather than against the probe's own code path: where
    /// the probe says yes, an independent `Nvml::open` must succeed and see a
    /// device, and where it says no, one of them must fail.
    #[test]
    fn the_nvml_probe_says_yes_exactly_when_a_collector_could_start() {
        let probe = probe("nvml").expect("`nvml` is this family's requirement");
        let starts = nvml::Nvml::open()
            .ok()
            .and_then(|library| library.device_count().ok())
            .is_some_and(|count| count > 0);
        assert_eq!(probe.is_available(), starts, "{probe}");
    }

    /// The text as the driver on the development machine wrote it, and the
    /// example NVML's own header gives: the parser is held to both.
    const THIS_BOARD: &str = "perf=0, nvclock=300, nvclockmin=300, nvclockmax=645, \
        nvclockeditable=1, memclock=405, memclockmin=405, memclockmax=405, memclockeditable=1 ; \
        perf=1, nvclock=300, nvclockmin=300, nvclockmax=2100, nvclockeditable=1, memclock=810 ; \
        perf=4, nvclock=300, nvclockmin=300, nvclockmax=2205, nvclockeditable=1, memclock=7070";
    const HEADER_EXAMPLE: &str = "perf=0, nvclock=324, nvclockmin=324, nvclockmax=324, \
        nvclockeditable=0, memclock=324, memclockmin=324, memclockmax=324, memclockeditable=0, \
        memtransferrate=648, memtransferratemin=648, memtransferratemax=648, \
        memtransferrateeditable=0 ; perf=1, nvclock=324, nvclockmin=324, nvclockmax=640, \
        nvclockeditable=0, memclock=810, memclockmin=810, memclockmax=810, memclockeditable=0";

    #[test]
    fn the_performance_levels_are_read_from_the_drivers_text() {
        let levels = perf_levels(THIS_BOARD);
        assert_eq!(levels.len(), 3, "{levels:?}");
        assert_eq!(
            levels[2],
            PerfLevel {
                perf: 4,
                shader_min_mhz: 300,
                shader_max_mhz: 2205
            }
        );
        let header = perf_levels(HEADER_EXAMPLE);
        assert_eq!(header.len(), 2);
        assert_eq!(
            (header[0].shader_min_mhz, header[0].shader_max_mhz),
            (324, 324)
        );
        // The current-level form carries no `perf` token and is still a level.
        let current = perf_levels("nvclock=300, nvclockmin=300, nvclockmax=645, memclock=405");
        assert_eq!(current.len(), 1);
        assert!(perf_levels("").is_empty());
        assert!(perf_levels("perf=0, memclock=405").is_empty());
    }

    /// The verdict rests on the highest level, and "locked" needs every level
    /// to agree: an idle level pinned at its floor is not a lock, which is
    /// what the header's own example shows at perf 0.
    #[test]
    fn a_lock_is_one_frequency_at_every_level_and_nothing_less() {
        assert_eq!(
            shader_lock(&perf_levels(THIS_BOARD), Some(2205)),
            ClockLock::NotShownLocked {
                min_mhz: 300,
                max_mhz: 2205,
                device_max_mhz: Some(2205)
            }
        );
        assert!(matches!(
            shader_lock(&perf_levels(HEADER_EXAMPLE), None),
            ClockLock::NotShownLocked { .. }
        ));
        let pinned = "perf=0, nvclockmin=1500, nvclockmax=1500 ; perf=1, nvclockmin=1500, \
                      nvclockmax=1500";
        assert!(matches!(
            shader_lock(&perf_levels(pinned), Some(2205)),
            ClockLock::Locked { mhz: 1500, .. }
        ));
        assert!(matches!(
            shader_lock(&[], Some(2205)),
            ClockLock::Unknown { .. }
        ));
    }

    #[test]
    fn every_clock_event_reason_is_named_and_an_unknown_one_is_kept() {
        assert_eq!(reason_names(0), Vec::<String>::new());
        assert_eq!(reason_names(0x1), ["gpu_idle"]);
        assert_eq!(reason_names(0x5), ["gpu_idle", "sw_power_cap"]);
        assert_eq!(
            reason_names(0x400 | 0x8000),
            ["reliability", "unnamed:0x8000"]
        );
    }

    /// Only a lock that was shown silences the timing line. A range in the
    /// performance levels is not shown free -- a `-lgc` lock may not show
    /// there -- so it reads as "not shown to be locked", exactly as unknown
    /// does, and never as "unlocked".
    #[test]
    fn a_timing_says_its_clocks_were_free_unless_a_lock_was_shown() {
        let state = |shader_clock| ClockState {
            device: 0,
            device_name: "board".into(),
            device_uuid: "GPU-0".into(),
            shader_clock,
            application_clocks: ApplicationClocks::NotSupported,
            persistence: Persistence::Off,
            event_reasons: EventReasons::Read { active: vec![] },
        };
        let locked = state(ClockLock::Locked {
            mhz: 1500,
            how: "application clocks".into(),
        });
        assert_eq!(locked.timing_fact(), None);
        assert!(locked.fact().contains("locked at 1500 MHz"));
        let ranged = state(ClockLock::NotShownLocked {
            min_mhz: 300,
            max_mhz: 2205,
            device_max_mhz: Some(2205),
        });
        assert_eq!(ranged.locked(), None);
        let fact = ranged.fact();
        assert!(fact.contains("not shown locked"), "{fact}");
        assert!(!fact.contains("not locked"), "{fact}");
        let timing = ranged.timing_fact().unwrap();
        assert!(timing.contains("not shown to be locked"), "{timing}");
        assert!(!timing.contains("unlocked"), "{timing}");
        let unknown = state(ClockLock::Unknown {
            reason: "no levels".into(),
        });
        assert!(unknown
            .timing_fact()
            .unwrap()
            .contains("not shown to be locked"));
    }

    /// On a machine with a device, the state read through the library agrees
    /// with the vendor's own tool where that tool has an answer: persistence
    /// mode, and the top of the shader clock's range. Neither moves while
    /// another stream uses the board, which is why they are the ones compared.
    #[test]
    fn the_clock_state_agrees_with_the_vendors_own_tool() {
        let states = match clock_states() {
            Ok(states) => states,
            Err(reason) => return eprintln!("skipped: {reason}"),
        };
        let smi = |field: &str| {
            std::process::Command::new("nvidia-smi")
                .args([
                    &format!("--query-gpu={field}"),
                    "--format=csv,noheader,nounits",
                ])
                .output()
                .ok()
                .filter(|out| out.status.success())
                .map(|out| {
                    String::from_utf8_lossy(&out.stdout)
                        .lines()
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_owned()
                })
        };
        let state = &states[0];
        if let Some(persistence) = smi("persistence_mode") {
            let expected = match persistence.as_str() {
                "Enabled" => Persistence::On,
                "Disabled" => Persistence::Off,
                other => return eprintln!("skipped: nvidia-smi answers `{other}`"),
            };
            assert_eq!(state.persistence, expected, "{}", state.fact());
        }
        if let (Some(max), ClockLock::NotShownLocked { device_max_mhz, .. }) =
            (smi("clocks.max.sm"), &state.shader_clock)
        {
            assert_eq!(device_max_mhz.map(|mhz| mhz.to_string()), Some(max));
        }
        assert!(
            state.fact().starts_with("vendor:nvml: device 0 ("),
            "{}",
            state.fact()
        );
    }

    #[test]
    fn a_hwmon_name_no_device_carries_is_missing_and_lists_what_is_there() {
        if std::fs::read_dir("/sys/class/hwmon").is_err() {
            eprintln!(
                "skipped: /sys/class/hwmon is not readable here, so there is nothing to list"
            );
            return;
        }
        let probe = probe("hwmon:definitely-not-a-sensor").unwrap();
        assert!(probe.is_missing(), "{probe}");
        assert!(probe.to_string().contains("this machine has"), "{probe}");
    }
}
