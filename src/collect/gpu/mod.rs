//! Collectors for the `gpu` family.
//!
//! The landscape is asymmetric and the code shows it. AMD and Intel expose a
//! DRM scheduler, kernel PMUs and hwmon, so their numbers come from the kernel
//! like every other family's. NVIDIA exposes none of that: below the driver
//! there is nothing to read, so the same numbers come from `libnvidia-ml.so`,
//! opened at runtime. That is the whole of the difference, and it is why this
//! directory has a `vendor` layer no other family needs.

mod device;
#[cfg(test)]
mod fixture;
mod nvml;

use super::Factory;
use crate::capabilities::Probe;

/// The collectors this family provides.
///
/// The sysfs backends of these same sources -- `fs:sys:amdgpu`, `fs:sys:i915`
/// -- are not here yet: they cannot be written honestly on a machine that has
/// no such device to check them against, and an unexercised reader of a sysfs
/// layout is a guess with a file path in it.
pub fn factories() -> Vec<Factory> {
    use device::{Metric, Prepare};
    vec![
        Factory::new("gpu.freq", "vendor:nvml", |_| {
            Ok(Box::new(Prepare(Metric::Freq)))
        }),
        Factory::new("gpu.power", "vendor:nvml", |_| {
            Ok(Box::new(Prepare(Metric::Power)))
        }),
        Factory::new("gpu.temp", "vendor:nvml", |_| {
            Ok(Box::new(Prepare(Metric::Temp)))
        }),
        Factory::new("gpu.mem", "vendor:nvml", |_| {
            Ok(Box::new(Prepare(Metric::Mem)))
        }),
        Factory::new("gpu.engine_busy", "vendor:nvml", |_| {
            Ok(Box::new(Prepare(Metric::EngineBusy)))
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
    let library = match requirement {
        "nvml" => return Some(nvml_probe()),
        "cupti" => "libcupti.so",
        "rocprofiler" => "librocprofiler64.so",
        "level-zero" => "libze_loader.so",
        _ => return None,
    };
    Some(loadable(requirement, library))
}

fn nvml_probe() -> Probe {
    match nvml::Nvml::present() {
        Ok(_) => Probe::Available,
        Err(reason) => Probe::Missing(format!(
            "the NVIDIA management library is not loadable on this machine ({reason})"
        )),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn a_hwmon_name_no_device_carries_is_missing_and_lists_what_is_there() {
        let probe = probe("hwmon:definitely-not-a-sensor").unwrap();
        assert!(probe.is_missing(), "{probe}");
        assert!(probe.to_string().contains("this machine has"), "{probe}");
    }
}
