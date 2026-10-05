//! Device state through the NVIDIA management library.
//!
//! Five sources share one collector because they share one mechanism: open the
//! library, enumerate the devices, and read one number per device per tick.
//! What differs between them is which number, and that is the `Metric` below.
//!
//! Two rules shape the events.
//!
//! One series carries one meaning. A clock series that mixed the shader,
//! graphics and memory domains, or a VRAM series that mixed a process with its
//! device, would aggregate into an average of unlike things -- so the value
//! attribute holds a single kind of number and the rest ride along under names
//! of their own.
//!
//! A device figure is a device figure. With a launched program these sources
//! measure the GPU during that program's window, not that program's share of
//! the GPU: nothing in NVML attributes a clock or a temperature to a process.
//! Every event says so in its `scope` attribute, so the distinction survives
//! into the output file rather than living in the reader's memory.

use std::time::Duration as StdDuration;

use crate::clock::monotonic_ns;
use crate::collect::{Collector, Prepared, SampleError};
use crate::event::{Attr, AttrValue, CollectorId, Event, KeyValue};
use crate::request::{Mode, Target};

use super::nvml::{DeviceHandle, Nvml, NvmlError};
use super::ClockState;

/// NVML averages utilization over a window it chooses and does not report.
/// NVIDIA documents that window as at most one second, which is therefore how
/// far from its timestamp such a sample may be.
const UTILIZATION_WINDOW_NS: u32 = 1_000_000_000;

/// How often a `count` run is asked for events.
///
/// It has almost none to give: a `count` has no cadence of its own, so the
/// reads stay at [`Mode::DEFAULT_POLL_INTERVAL`], which is also about as often
/// as the driver refreshes what these sources read. The tick exists so that the
/// *run* notices a launched program that exited, and so that the window it
/// reports is the program's life rather than that life rounded up to the next
/// whole second -- which is what `rate`, reachable on `gpu.engine_busy` only
/// through `count`, divides by. The same tick, for the same reason, is in
/// `collect/cpu/counters.rs`.
const COUNT_TICK: StdDuration = StdDuration::from_millis(10);

/// Which number a collector reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    Freq,
    Power,
    Temp,
    Mem,
    EngineBusy,
}

impl Metric {
    /// The catalog source this metric serves.
    fn source(self) -> &'static str {
        match self {
            Metric::Freq => "gpu.freq",
            Metric::Power => "gpu.power",
            Metric::Temp => "gpu.temp",
            Metric::Mem => "gpu.mem",
            Metric::EngineBusy => "gpu.engine_busy",
        }
    }

    /// The driver figure whose repeat this collector counts, named for the
    /// note; `None` for a figure whose repeat cannot be told from a steady
    /// value. See [`Refresh`].
    fn refreshed_figure(self) -> Option<&'static str> {
        match self {
            Metric::Power => Some("energy counter"),
            Metric::EngineBusy => Some("occupancy"),
            Metric::Freq | Metric::Temp | Metric::Mem => None,
        }
    }

    /// What identifies one driver sample of the energy counter, from the
    /// attributes of a reading: the counter itself, which only ever rises
    /// while the board draws power. The occupancy has no such figure -- an
    /// idle or saturated board reads the same pair sample after sample -- so
    /// its identity is the driver's own sample timestamp, read beside it
    /// ([`occupancy_fingerprint`]), never its value.
    fn fingerprint(self, attrs: &[Attr]) -> Option<u64> {
        match self {
            Metric::Power => attrs
                .iter()
                .find(|attr| attr.name == "joules")
                .and_then(|attr| attr.value.as_f64())
                .map(f64::to_bits),
            _ => None,
        }
    }

    /// The attribute holding the measurement, named rather than inferred from
    /// the unit: these events carry several numbers and only one of them is the
    /// series.
    fn value_attr(self) -> &'static str {
        match self {
            Metric::Freq => "hz",
            Metric::Power => "joules",
            Metric::Temp => "celsius",
            Metric::Mem => "bytes",
            Metric::EngineBusy => "ratio",
        }
    }
}

/// The identity of the occupancy sample a reading saw: the timestamp of the
/// newest sample the driver had taken, asked for after the one the previous
/// reading saw. No newer sample means the reading is the previous one again,
/// whatever its value; a newer one means the driver refreshed, even if the
/// board is so steadily idle or busy that the figure did not move.
///
/// `None` -- the reading is not counted either way -- when the driver keeps no
/// samples or the call failed: a repeat that cannot be shown is not guessed
/// from the value.
///
/// The two calls are not atomic: a sample the driver takes between the
/// occupancy read and this one is credited to this reading, and the next
/// reading, which does carry it, is counted as a repeat. That is one reading
/// in a refresh period at most, and only when a refresh falls inside the
/// millisecond between two calls.
fn occupancy_fingerprint(
    nvml: &Nvml,
    handle: DeviceHandle,
    seen_us: &mut Vec<Option<u64>>,
    device: usize,
) -> Option<u64> {
    if seen_us.len() <= device {
        seen_us.resize(device + 1, None);
    }
    let after_us = seen_us[device].unwrap_or(0);
    match nvml.newest_occupancy_sample_us(handle, after_us) {
        Ok(Some(newest)) => {
            seen_us[device] = Some(newest);
            Some(newest)
        }
        Ok(None) => seen_us[device],
        Err(_) => None,
    }
}

/// Holds the choice of metric and the mode until the target exists.
pub struct Prepare {
    metric: Metric,
    mode: Mode,
}

impl Prepare {
    pub fn new(metric: Metric, mode: Mode) -> Self {
        Self { metric, mode }
    }
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        // A device figure needs no process, so no target is refused here. What
        // the target decides is `gpu.mem`: with a process it reports that
        // process's VRAM, without one the device's.
        NvmlCollector::new(self.metric, self.mode, target.first_pid())
            .map(|collector| Box::new(collector) as Box<dyn Collector>)
    }
}

/// The sequence number of the reading that closes a counted window.
const CLOSING: u64 = u64::MAX / 2;

/// One device, as this run will refer to it.
struct Device {
    handle: DeviceHandle,
    index: u32,
    name: String,
    /// The identity that survives a reboot, a driver reload and a change of
    /// index. Two runs are comparable only if this is the same device.
    uuid: String,
    /// Energy counter reading when the run started, so every event reports the
    /// energy of this run rather than of everything since the driver loaded.
    energy_base_mj: u64,
    /// When that reading was taken: the start of a counted window, whose
    /// length is what `rate` divides the energy by to give watts.
    energy_base_ns: u64,
}

/// Reads one number per device per tick.
pub struct NvmlCollector {
    id: CollectorId,
    metric: Metric,
    /// The mode the request named. Only `count` changes anything here: it has
    /// no cadence of its own, so the collector supplies both the cadence of its
    /// reads and the tick that lets the run end when its target does.
    mode: Mode,
    /// When the last read was taken, so a `count` run keeps reading at the
    /// interval it would have had rather than at the tick.
    last_read_ns: Option<u64>,
    nvml: Nvml,
    devices: Vec<Device>,
    /// The driver, which is part of what a number means: NVML's semantics are
    /// the driver's, not a kernel interface's.
    driver: String,
    /// The process this run is scoped to, when there is one. For the
    /// device-wide metrics it only sets the window; for `gpu.mem` it decides
    /// whether the figure is a process's or the device's.
    pid: Option<i32>,
    /// Each device's clock state, read before the observed program runs and
    /// again when the run stops. Read, never set: see [`ClockState`].
    clocks_at_start: Vec<ClockState>,
    clocks_at_end: Vec<ClockState>,
    /// How many readings found the driver's figure where the previous one
    /// left it. See [`Refresh`].
    refresh: Refresh,
    /// Per device, the timestamp of the newest occupancy sample the driver
    /// had taken at the last reading. See [`occupancy_fingerprint`].
    occupancy_seen_us: Vec<Option<u64>>,
    /// The energy counter held against the draw read beside it. See
    /// [`CounterCheck`].
    counter_check: CounterCheck,
    /// The reads the driver made wait. See [`HeldReads`].
    held: HeldReads,
}

/// **NVML makes a read wait while a CUDA context is created or destroyed on
/// the device.** Measured on the board this was written on
/// (`docs/scenarios/gpu.md`, defect 10): one NVML call every 10 ms beside
/// `sleep` answered in 1 to 17 ms; beside the GPU bench's launch-bound
/// program, the first call of a round took 60 to 80 ms once or twice per run,
/// at the context's creation and teardown. A 100 ms poll whose read waits
/// that long behind a late one outruns its tick, which the run counts as a
/// late tick -- the two "lost records" of `gpu.freq` in the GPU bench.
///
/// Nothing here can make the driver answer sooner. What the collector does is
/// say it: every event's `ts_err_ns` is the span its read took, and a run
/// whose reads were held says how many, how long, and why.
#[derive(Debug, Default)]
struct HeldReads {
    reads: u64,
    held: u64,
    worst_ns: u64,
}

impl HeldReads {
    /// A read longer than this was made to wait: an idle board answers the
    /// slowest call this collector makes (the power draw) in under 10 ms, and
    /// the longest seen beside a program that does not touch the device was
    /// 17 ms.
    const HELD_NS: u64 = 25_000_000;

    fn record(&mut self, read_ns: u64) {
        self.reads += 1;
        if read_ns > Self::HELD_NS {
            self.held += 1;
            self.worst_ns = self.worst_ns.max(read_ns);
        }
    }

    fn note(&self) -> Option<String> {
        (self.held > 0).then(|| {
            format!(
                "vendor:nvml: the driver made {} of {} reads wait longer than {} ms (worst {:.1} \
                 ms); NVML holds a read while a CUDA context is created or destroyed on the \
                 device, so a late tick beside this is the driver's wait rather than this \
                 collector's work, and each event's ts_err_ns is how long its read took",
                self.held,
                self.reads,
                Self::HELD_NS / 1_000_000,
                self.worst_ns as f64 / 1e6
            )
        })
    }
}

/// **Reading the energy counter often makes it under-count.** Measured on the
/// board this was written on (`docs/scenarios/gpu.md`, defect 7): two reads
/// four seconds apart give the idle board's 23 W; the same window with the
/// counter -- or the instantaneous draw -- read every 100 ms gives about half
/// that, and every 10 ms about 3 W, less than the board can draw at all. The
/// draw itself goes *up* meanwhile, because the reads keep the board out of
/// its deepest idle state. Another process polling the board does the same to
/// this one's counter, which nothing here can see.
///
/// What a poll can see is its own case: the energy the counter accumulated
/// over the run against the mean of the draw sampled beside it. Where the
/// counter implies less than half the draw, the joules of this run are low,
/// and the run says so with both figures.
#[derive(Debug, Default)]
struct CounterCheck {
    /// Per device: the sum and count of the draws read, the last cumulative
    /// joules and the window they cover.
    devices: Vec<(f64, u64, f64, u64)>,
}

impl CounterCheck {
    fn record(&mut self, device: usize, attrs: &[Attr], base_ns: u64, now_ns: u64) {
        let value = |name: &str| {
            attrs
                .iter()
                .find(|attr| attr.name == name)
                .and_then(|attr| attr.value.as_f64())
        };
        let (Some(joules), Some(watts)) = (value("joules"), value("watts")) else {
            return;
        };
        if self.devices.len() <= device {
            self.devices.resize(device + 1, (0.0, 0, 0.0, 0));
        }
        let entry = &mut self.devices[device];
        entry.0 += watts;
        entry.1 += 1;
        entry.2 = joules;
        entry.3 = now_ns.saturating_sub(base_ns);
    }

    /// The note for each device whose counter fell below half its draw.
    fn notes(&self, poll_ns: u64) -> Vec<String> {
        let mut notes = Vec::new();
        for (index, &(sum, count, joules, span_ns)) in self.devices.iter().enumerate() {
            // Too few draws, or too short a window, to call anything.
            if count < 3 || span_ns < 200_000_000 {
                continue;
            }
            let counter_w = joules / (span_ns as f64 / 1e9);
            let draw_w = sum / count as f64;
            if counter_w * 2.0 < draw_w {
                notes.push(format!(
                    "vendor:nvml: device {index}'s energy counter implies {counter_w:.1} W over \
                     this run while the draw read beside it averaged {draw_w:.1} W: on this \
                     driver, reading the counter or the draw more often than about once a second \
                     makes the counter under-count, and this run read both every {} ms, so its \
                     joules are low; take energy with `--mode count`, which reads the counter \
                     twice",
                    poll_ns as f64 / 1e6
                ));
            }
        }
        notes
    }
}

/// The driver refreshes what NVML returns at its own rate, and a poll faster
/// than that rate returns the same driver sample several times over: points
/// on a timeline that look like measurements and are one measurement repeated.
///
/// Measured on the board this was written on (`docs/scenarios/gpu.md`, "What a
/// poll can get from the driver"): the energy counter and the instantaneous
/// draw move every 100 ms, the occupancy about once a second. Neither number
/// is assumed here. A repeat is *observed* -- the reading carries the driver
/// sample the previous one carried: the same counter bit for bit, or the same
/// occupancy sample timestamp -- and counted, and the note says how often the
/// figure actually moved in this run.
///
/// Only the two figures whose repeat can be told from a steady value are
/// counted. The energy counter only ever rises while the board draws power,
/// so an unchanged counter is an unrefreshed one. The occupancy's value is not
/// such a figure -- a steadily idle or saturated board reads 0.02 or 1.0 sample
/// after sample -- so its repeat is told by the driver's own sample timestamp,
/// read beside it, and not by the value. A clock or a temperature that reads
/// the same twice may simply be steady, the driver gives no sample timestamp
/// for either, and counting them would be counting a guess.
#[derive(Debug, Default)]
struct Refresh {
    /// The last reading's fingerprint, per device.
    previous: Vec<Option<u64>>,
    readings: u64,
    repeats: u64,
    first_ns: Option<u64>,
    last_ns: u64,
}

impl Refresh {
    /// Records one reading of one device, and answers whether it repeated the
    /// previous one.
    fn record(&mut self, device: usize, fingerprint: u64, now_ns: u64) -> bool {
        if self.previous.len() <= device {
            self.previous.resize(device + 1, None);
        }
        let repeated = self.previous[device] == Some(fingerprint);
        self.previous[device] = Some(fingerprint);
        self.readings += 1;
        if repeated {
            self.repeats += 1;
        }
        self.first_ns.get_or_insert(now_ns);
        self.last_ns = now_ns;
        repeated
    }

    /// The note a run with repeats carries, or nothing.
    fn note(&self, figure: &str, poll_ns: Option<u64>, devices: usize) -> Option<String> {
        if self.repeats == 0 {
            return None;
        }
        let span_ns = self
            .last_ns
            .saturating_sub(self.first_ns.unwrap_or(self.last_ns));
        // Each device's first reading is new by definition and is not a move.
        let moves = self
            .readings
            .saturating_sub(self.repeats)
            .saturating_sub(devices as u64);
        // Per device: the moves are summed over every device read.
        let moved = if let Some(every_ns) = span_ns
            .saturating_mul(devices.max(1) as u64)
            .checked_div(moves)
        {
            format!(
                "it moved {moves} {} in {} ms, about every {} ms",
                if moves == 1 { "time" } else { "times" },
                span_ns / 1_000_000,
                every_ns / 1_000_000
            )
        } else {
            format!("it did not move once in {} ms", span_ns / 1_000_000)
        };
        let polled = match poll_ns {
            Some(ns) => format!("this run read it every {} ms", ns as f64 / 1e6),
            None => "this run read it faster than that".to_owned(),
        };
        Some(format!(
            "vendor:nvml: {} of {} readings found the {figure} where the previous reading left \
             it, so those points are one driver sample read again, not new measurements: {moved} \
             and {polled}; poll no faster than the driver refreshes it",
            self.repeats, self.readings
        ))
    }
}

impl NvmlCollector {
    fn new(metric: Metric, mode: Mode, pid: Option<i32>) -> Result<Self, String> {
        let nvml = Nvml::open()?;
        let count = nvml
            .device_count()
            .map_err(|error| format!("cannot count the NVIDIA devices: {error}"))?;
        if count == 0 {
            return Err(
                "the NVIDIA management library loaded but reports no device on this machine"
                    .to_owned(),
            );
        }

        let mut devices = Vec::with_capacity(count as usize);
        for index in 0..count {
            let handle = nvml
                .device(index)
                .map_err(|error| format!("cannot open NVIDIA device {index}: {error}"))?;
            let name = nvml
                .device_name(handle)
                .unwrap_or_else(|_| format!("device {index}"));
            let uuid = nvml
                .device_uuid(handle)
                .unwrap_or_else(|error| format!("unknown ({error})"));
            devices.push(Device {
                handle,
                index,
                name,
                uuid,
                energy_base_mj: 0,
                energy_base_ns: 0,
            });
        }

        let driver = nvml
            .driver_version()
            .unwrap_or_else(|| "unknown".to_owned());
        Ok(Self {
            id: CollectorId::new(format!("{}/vendor:nvml", metric.source())),
            metric,
            mode,
            last_read_ns: None,
            nvml,
            devices,
            driver,
            pid,
            clocks_at_start: Vec::new(),
            clocks_at_end: Vec::new(),
            refresh: Refresh::default(),
            occupancy_seen_us: Vec::new(),
            counter_check: CounterCheck::default(),
            held: HeldReads::default(),
        })
    }

    /// The attributes every event of this collector carries, whatever the
    /// metric: which device produced it, and what the number is a figure for.
    fn device_attrs(&self, device: &Device, scope: &'static str) -> Vec<Attr> {
        // Every event says which device it came from, under which driver, and
        // what it is a figure for. An event that needs a note elsewhere to be
        // read is an event that will be read wrong.
        vec![
            Attr::uint("device", device.index as u64),
            Attr::text("device_name", device.name.clone()),
            Attr::text("device_uuid", device.uuid.clone()),
            Attr::text("driver", self.driver.clone()),
            Attr::text("scope", scope),
        ]
    }

    fn freq(&self, device: &Device) -> Result<(Vec<Attr>, u32), NvmlError> {
        let sm = self.nvml.sm_clock_mhz(device.handle)?;
        let mut attrs = self.device_attrs(device, "device");
        attrs.push(Attr::uint("hz", mhz_to_hz(sm)));
        // The other domains are reported beside the series rather than in it: a
        // memory clock averaged together with a shader clock is a number of
        // nothing.
        if let Ok(graphics) = self.nvml.graphics_clock_mhz(device.handle) {
            attrs.push(Attr::uint("graphics_hz", mhz_to_hz(graphics)));
        }
        if let Ok(memory) = self.nvml.memory_clock_mhz(device.handle) {
            attrs.push(Attr::uint("memory_hz", mhz_to_hz(memory)));
        }
        Ok((attrs, 0))
    }

    fn power(&self, device: &Device) -> Result<(Vec<Attr>, u32), NvmlError> {
        let millijoules = self
            .nvml
            .energy_millijoules(device.handle)?
            .saturating_sub(device.energy_base_mj);
        let mut attrs = self.device_attrs(device, "device");
        attrs.push(float("joules", millijoules as f64 / 1000.0));
        // A counted window says how long it was: the energy over that, not
        // over the run's wall clock, is the mean draw in watts.
        if self.mode == Mode::Count {
            attrs.push(Attr::uint(
                "enabled_ns",
                monotonic_ns().saturating_sub(device.energy_base_ns),
            ));
        }
        // The instantaneous draw is what the energy is not: a rate at one
        // instant, useful for seeing a spike, useless for adding up. Both are
        // reported, under names that cannot be confused.
        if let Ok(milliwatts) = self.nvml.power_milliwatts(device.handle) {
            attrs.push(float("watts", milliwatts as f64 / 1000.0));
        }
        Ok((attrs, 0))
    }

    fn temp(&self, device: &Device) -> Result<(Vec<Attr>, u32), NvmlError> {
        let celsius = self.nvml.temperature_celsius(device.handle)?;
        let mut attrs = self.device_attrs(device, "device");
        attrs.push(Attr::uint("celsius", celsius as u64));
        Ok((attrs, 0))
    }

    fn engine_busy(&self, device: &Device) -> Result<(Vec<Attr>, u32), NvmlError> {
        let utilization = self.nvml.utilization(device.handle)?;
        let mut attrs = self.device_attrs(device, "device");
        attrs.push(float("ratio", utilization.gpu as f64 / 100.0));
        attrs.push(float("memory_ratio", utilization.memory as f64 / 100.0));
        // NVML has no per-engine breakdown, so the engine is named for what it
        // is: the whole device. The i915 and amdgpu PMUs do have one, and their
        // events will say which engine they came from.
        attrs.push(Attr::text("engine", "device"));
        Ok((attrs, UTILIZATION_WINDOW_NS))
    }

    /// VRAM held: by the process tree this run observes, or by the device when
    /// the run observes no process.
    ///
    /// The tree, not the process alone: a launched program is often a wrapper
    /// (`sh`, `python -m`, a launcher script) whose CUDA work happens in a
    /// child, and the driver lists the child. Reading the launched pid alone
    /// gave 0 for a tree holding 412 MiB (`docs/scenarios/gpu.md`, defect 9).
    /// `tree` is the observed process and every descendant it has at this
    /// reading, walked once per reading for every device.
    ///
    /// The two never share a series. A process figure and a device figure are
    /// both bytes of VRAM and mean entirely different things, so the target
    /// decides which one this is and the `scope` attribute records the answer.
    ///
    /// A process absent from the driver's list holds nothing, which is reported
    /// as zero: an empty series and a series of zeros say different things
    /// about a run, and only one of them is true here.
    fn mem(&self, device: &Device, tree: &[i32]) -> Result<(Vec<Attr>, u32), NvmlError> {
        let memory = self.nvml.memory(device.handle)?;
        if self.pid.is_none() {
            let mut attrs = self.device_attrs(device, "device");
            attrs.push(Attr::uint("bytes", memory.used));
            attrs.push(Attr::uint("device_total_bytes", memory.total));
            return Ok((attrs, 0));
        }

        let processes = self.nvml.processes(device.handle)?;
        let mut held = 0u64;
        let mut kinds: Vec<&'static str> = Vec::new();
        let mut holders: Vec<i32> = Vec::new();
        for (kind, process) in processes {
            let pid = process.pid as i32;
            if !tree.contains(&pid) {
                continue;
            }
            // A process listed twice, as compute and as graphics, holds one
            // arena the driver reports under each kind: counted per entry, as
            // the driver lists it, and as one holder.
            if !holders.contains(&pid) {
                holders.push(pid);
            }
            held = held.saturating_add(process.used_gpu_memory);
            let kind = kind.as_str();
            if !kinds.contains(&kind) {
                kinds.push(kind);
            }
        }

        let mut attrs = self.device_attrs(device, "process");
        attrs.push(Attr::uint("bytes", held));
        attrs.push(Attr::text(
            "held_as",
            if kinds.is_empty() {
                "none".to_owned()
            } else {
                kinds.join("+")
            },
        ));
        // How many processes of the tree the figure is the sum of: 1 for a
        // program that is its own CUDA process, more for one that forks
        // workers, 0 for a tree that holds nothing yet.
        attrs.push(Attr::uint("processes", holders.len() as u64));
        attrs.push(Attr::uint("device_used_bytes", memory.used));
        attrs.push(Attr::uint("device_total_bytes", memory.total));
        Ok((attrs, 0))
    }
}

impl Collector for NvmlCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        self.metric.value_attr()
    }

    fn start(&mut self) -> Result<(), String> {
        // The clocks as the observed program will find them, before it runs.
        self.clocks_at_start = self.clock_states();
        if self.metric != Metric::Power {
            return Ok(());
        }
        // An energy counter is the difference between two readings, so the
        // first one is taken before the observed program runs. A device without
        // the counter is refused here rather than served an integral of sampled
        // power under the name of energy.
        for device in &mut self.devices {
            match self.nvml.energy_millijoules(device.handle) {
                Ok(millijoules) => {
                    device.energy_base_mj = millijoules;
                    device.energy_base_ns = monotonic_ns();
                }
                Err(error) if error.is_not_supported() => {
                    return Err(format!(
                        "{} has no energy counter (that needs Volta or later); NVML reports \
                         instantaneous power only, and integrating a sampled draw is not energy",
                        device.name
                    ));
                }
                Err(error) => {
                    return Err(format!(
                        "cannot read the energy counter of {}: {error}",
                        device.name
                    ));
                }
            }
        }
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        // A process target that has gone ends the run rather than producing a
        // series of zeros that looks like a program holding no memory.
        if let (Metric::Mem, Some(pid)) = (self.metric, self.pid) {
            if let Some(gone) = departed(pid) {
                return Err(SampleError::TargetGone(gone));
            }
        }

        // A `count` request names no cadence, so the collector keeps its own:
        // the reads stay a second apart, which is about as often as the driver
        // refreshes them, while the run is ticked far more finely so that it
        // ends when its target does. Without the split, either the window is
        // rounded up to a whole second or the driver is asked a hundred times
        // for one sample.
        //
        // Energy is the exception: a counted window of it is one reading, at
        // the end, of the energy since the baseline -- a reading a second
        // would be a running total, and `rate` sums what it is given.
        if self.mode == Mode::Count && self.metric == Metric::Power && seq != CLOSING {
            return Ok(Vec::new());
        }
        if self.mode == Mode::Count {
            let now_ns = monotonic_ns();
            let due = self.last_read_ns.is_none_or(|last| {
                now_ns.saturating_sub(last) >= Mode::DEFAULT_POLL_INTERVAL.as_nanos()
            });
            if !due {
                return Ok(Vec::new());
            }
            self.last_read_ns = Some(now_ns);
        }

        // The observed process tree, walked once for every device's reading.
        let tree: Vec<i32> = match (self.metric, self.pid) {
            (Metric::Mem, Some(pid)) => crate::collect::task::with_descendants(&[pid]),
            _ => Vec::new(),
        };
        let mut events = Vec::with_capacity(self.devices.len());
        for (offset, device) in self.devices.iter().enumerate() {
            let read_started_ns = monotonic_ns();
            let read = match self.metric {
                Metric::Freq => self.freq(device),
                Metric::Power => self.power(device),
                Metric::Temp => self.temp(device),
                Metric::Mem => self.mem(device, &tree),
                Metric::EngineBusy => self.engine_busy(device),
            };
            let (attrs, ts_err_ns) = match read {
                Ok(read) => read,
                Err(error) if error.is_gone() => {
                    return Err(SampleError::TargetGone(format!(
                        "{} is no longer reachable: {error}",
                        device.name
                    )));
                }
                Err(error) => {
                    return Err(SampleError::Transient(format!(
                        "{} on {}: {error}",
                        self.metric.source(),
                        device.name
                    )));
                }
            };
            // Taken after the read, so it bounds it: the value existed at or
            // before this instant.
            let ts_ns = monotonic_ns();
            // And the read's start bounds it from below: the driver's figure
            // is from somewhere in between, so that span is the timestamp's
            // error. It is a few milliseconds, except while the driver makes
            // the read wait (see [`HeldReads`]).
            let read_ns = ts_ns.saturating_sub(read_started_ns);
            self.held.record(read_ns);
            let ts_err_ns = ts_err_ns.max(u32::try_from(read_ns).unwrap_or(u32::MAX));
            let mut attrs = attrs;
            if self.metric == Metric::Power {
                self.counter_check
                    .record(offset, &attrs, device.energy_base_ns, ts_ns);
            }
            let fingerprint = if self.metric == Metric::EngineBusy {
                occupancy_fingerprint(
                    &self.nvml,
                    device.handle,
                    &mut self.occupancy_seen_us,
                    offset,
                )
            } else {
                self.metric.fingerprint(&attrs)
            };
            if let Some(fingerprint) = fingerprint {
                let repeated = self.refresh.record(offset, fingerprint, ts_ns);
                attrs.push(Attr::text(
                    "driver_sample",
                    if repeated { "repeat" } else { "new" },
                ));
            }
            let mut keys = Vec::new();
            if let (Metric::Mem, Some(pid)) = (self.metric, self.pid) {
                keys.push(KeyValue::task(pid));
            }
            events.push(Event {
                ts_ns,
                ts_err_ns,
                kind: self.metric.source().to_owned(),
                keys,
                attrs,
                collector_id: self.id.clone(),
                seq: seq + offset as u64,
            });
        }
        Ok(events)
    }

    /// How often the run asks this collector for events.
    ///
    /// Only in `count`, and not because there is anything to read that often:
    /// the reads are throttled in `sample`. It is the *run* that needs the
    /// tick, so that a launched program's exit is noticed when it happens
    /// rather than at the next second -- the window is what `rate` divides by,
    /// and `rate` is reachable on `gpu.engine_busy` only through this mode.
    fn tick(&self) -> Option<StdDuration> {
        (self.mode == Mode::Count).then_some(COUNT_TICK)
    }

    /// A `count` window is closed by a reading, not by the tick that happened
    /// to be last: the run stops the moment its target does, and the occupancy
    /// at that moment belongs to the series.
    fn stop(&mut self) -> Vec<Event> {
        // The clocks as the observed program left them: a thermal or power
        // limit reached during the run shows here and not at the start.
        self.clocks_at_end = self.clock_states();
        if self.mode != Mode::Count {
            return Vec::new();
        }
        self.last_read_ns = None;
        self.sample(CLOSING).unwrap_or_default()
    }

    /// What this collector had to give up, in the one place every emit and
    /// every format carries it.
    ///
    /// `gpu.engine_busy` is the only source here whose samples do not happen
    /// when they are timestamped: NVML averages occupancy over a window it
    /// chooses and does not report, documented as at most a second wide, and
    /// every event says so in its `ts_err_ns`. Only `raw` serialises that
    /// field, and this source does not declare `raw` -- so a reader of a
    /// timeline sees points 200 ms apart with nothing to say they may all be
    /// one driver sample. Until a timeline point can carry its own error bar,
    /// the error bar is stated once, here, where it reaches the reader whatever
    /// they asked for.
    fn notes(&self) -> Vec<String> {
        let poll_ns = match self.mode {
            Mode::Poll { interval } => Some(interval.as_nanos()),
            Mode::Count => Some(Mode::DEFAULT_POLL_INTERVAL.as_nanos()),
            _ => None,
        };
        let mut notes: Vec<String> = self
            .metric
            .refreshed_figure()
            .and_then(|figure| self.refresh.note(figure, poll_ns, self.devices.len()))
            .into_iter()
            .collect();
        if let (Metric::Power, Mode::Poll { interval }) = (self.metric, self.mode) {
            notes.extend(self.counter_check.notes(interval.as_nanos()));
        }
        notes.extend(self.held.note());
        if self.metric != Metric::EngineBusy {
            return notes;
        }
        notes.push(format!(
            "vendor:nvml: occupancy is averaged by the driver over a window it chooses, \
             documented as up to {} ms wide, so every timestamp in this series is good only to \
             that much and two points closer together than it may be one sample read twice",
            UTILIZATION_WINDOW_NS / 1_000_000
        ));
        notes
    }

    /// The clocks of every device, at the start of the run and, where the
    /// clock event reasons moved, at its end. A fact, not a concession: this
    /// collector reads the clocks and changes nothing.
    fn facts(&self) -> Vec<String> {
        let mut facts: Vec<String> = self.clocks_at_start.iter().map(ClockState::fact).collect();
        for end in &self.clocks_at_end {
            let start = self
                .clocks_at_start
                .iter()
                .find(|start| start.device_uuid == end.device_uuid);
            if start.is_none_or(|start| start.event_reasons != end.event_reasons) {
                facts.push(format!("at the end of the run: {}", end.fact()));
            }
        }
        facts
    }
}

impl NvmlCollector {
    fn clock_states(&self) -> Vec<ClockState> {
        self.devices
            .iter()
            .map(|device| {
                ClockState::read(
                    &self.nvml,
                    device.handle,
                    device.index,
                    &device.name,
                    &device.uuid,
                )
            })
            .collect()
    }
}

/// Whether a process this run is following has finished, and how it is known.
///
/// Asking whether `/proc/<pid>` exists is not the same question. A process that
/// has exited and whose parent has not reaped it keeps its entry -- a supervisor
/// or a test harness that defers the `wait` leaves one in exactly that state --
/// and following it produces the series of zeros this check exists to prevent.
/// The state letter in `/proc/<pid>/stat` is what tells the two apart, and it is
/// the third field, counted from the last `)` because the command name is
/// parenthesised and may contain spaces and parentheses of its own.
fn departed(pid: i32) -> Option<String> {
    let path = format!("/proc/{pid}/stat");
    let Ok(stat) = std::fs::read_to_string(&path) else {
        return Some(format!("process {pid} exited"));
    };
    let state = stat
        .rfind(')')
        .and_then(|end| stat[end + 1..].split_whitespace().next())
        .unwrap_or("");
    // `Z` is exited and unreaped; `X` is the brief state of one being torn down.
    match state {
        "Z" => Some(format!(
            "process {pid} exited and has not been reaped, so its video memory is already gone"
        )),
        "X" => Some(format!("process {pid} exited")),
        _ => None,
    }
}

fn mhz_to_hz(mhz: u32) -> u64 {
    mhz as u64 * 1_000_000
}

fn float(name: &'static str, value: f64) -> Attr {
    Attr {
        name: name.to_owned(),
        value: AttrValue::Float(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::gpu::fixture::CudaArena;

    // One lock for the whole test binary: see `energy_counter_lock` and `CounterCheck`.
    use crate::collect::gpu::energy_counter_lock as energy_lock;

    /// A read the driver held is counted and said, with the worst; a run of
    /// prompt reads says nothing.
    #[test]
    fn a_held_read_is_counted_and_said_and_a_prompt_one_is_not() {
        let mut held = HeldReads::default();
        for read_ns in [2_000_000, 9_000_000, 17_000_000] {
            held.record(read_ns);
        }
        assert_eq!(held.note(), None);
        held.record(78_500_000);
        held.record(31_000_000);
        let note = held.note().expect("two reads were held");
        assert!(note.contains("2 of 5 reads"), "{note}");
        assert!(note.contains("worst 78.5 ms"), "{note}");
        assert!(note.contains("CUDA context"), "{note}");
    }

    /// Builds a collector for this process, or explains why this machine
    /// cannot run the test. Most machines have no NVIDIA device and must skip
    /// cleanly: a test that fails there says nothing about the code.
    fn collector(metric: Metric) -> Option<NvmlCollector> {
        let polling = Mode::Poll {
            interval: Mode::DEFAULT_POLL_INTERVAL,
        };
        match NvmlCollector::new(metric, polling, Some(std::process::id() as i32)) {
            Ok(collector) => Some(collector),
            Err(reason) => {
                eprintln!("skipped: {reason}");
                None
            }
        }
    }

    fn value(collector: &mut NvmlCollector) -> f64 {
        let events = collector.sample(0).expect("a device that is there answers");
        events[0]
            .attr(collector.metric.value_attr())
            .and_then(crate::event::AttrValue::as_f64)
            .expect("the value attribute is the one the collector names")
    }

    /// Every metric this collector serves must name a source the catalog has,
    /// and a value attribute that the events actually carry. The second half is
    /// checked against real events in `tests/family_gpu.rs`, which needs a
    /// device; this half needs nothing.
    #[test]
    fn every_metric_names_a_source_in_the_catalog() {
        let catalog = crate::catalog::Catalog::embedded().unwrap();
        for metric in [
            Metric::Freq,
            Metric::Power,
            Metric::Temp,
            Metric::Mem,
            Metric::EngineBusy,
        ] {
            let source = metric.source().parse().unwrap();
            let entry = catalog
                .get(&source)
                .unwrap_or_else(|| panic!("{} is not in the catalog", metric.source()));
            assert_eq!(entry.family, "gpu");
            assert!(
                entry.backend("vendor:nvml").is_some(),
                "{} has no vendor:nvml backend",
                metric.source()
            );
        }
    }

    #[test]
    fn megahertz_become_hertz_exactly() {
        assert_eq!(mhz_to_hz(1_350), 1_350_000_000);
        assert_eq!(mhz_to_hz(0), 0);
    }
    /// The ground truth: a quantity the test chose, allocated through the CUDA
    /// driver, must come back through NVML as that same quantity. Anything
    /// derived from the collector's own arithmetic would prove only that the
    /// arithmetic is consistent with itself.
    #[test]
    fn a_known_allocation_is_reported_as_exactly_that_many_bytes() {
        const SIZE: usize = 256 << 20;
        let Some(mut collector) = collector(Metric::Mem) else {
            return;
        };
        let mut arena = match CudaArena::open() {
            Ok(arena) => arena,
            Err(reason) => {
                eprintln!("skipped: {reason}");
                return;
            }
        };

        // The context itself holds VRAM, so the measurement is a difference:
        // what this process held before the allocation, and after it.
        let before = value(&mut collector);
        arena.allocate(SIZE).expect("the device has 256 MiB free");
        let after = value(&mut collector);

        assert_eq!(
            after - before,
            SIZE as f64,
            "NVML reported {after} bytes held after allocating {SIZE} on top of {before}"
        );
    }

    /// The energy counter and the instantaneous draw are two different numbers
    /// from the same driver, and this checks that they describe the same
    /// device in the same units: the joules accumulated over a window must
    /// imply roughly the watts sampled across it. Only roughly -- how far apart
    /// they actually run, and why polling faster makes the integral worse, is
    /// measured in `docs/families/gpu.md`.
    #[test]
    fn the_energy_counter_reads_in_joules_of_the_draw_beside_it() {
        let _energy = energy_lock();
        let Some(mut collector) = collector(Metric::Power) else {
            return;
        };
        if let Err(reason) = collector.start() {
            eprintln!("skipped: {reason}");
            return;
        }

        // The draw is sampled across the same window, not once before it: the
        // GPU may be idle at the first read and busy a moment later, which is
        // exactly what an energy counter is for.
        let started_ns = monotonic_ns();
        let mut draws = Vec::new();
        for _ in 0..12 {
            let events = collector.sample(0).expect("the device answers");
            draws.push(
                events[0]
                    .attr("watts")
                    .and_then(crate::event::AttrValue::as_f64)
                    .expect("NVML reports instantaneous draw beside the counter"),
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let joules = value(&mut collector);
        let elapsed = (monotonic_ns() - started_ns) as f64 / 1e9;

        let mean_draw = draws.iter().sum::<f64>() / draws.len() as f64;
        assert!(mean_draw > 0.5, "a powered GPU draws more than half a watt");
        let implied = joules / elapsed;
        assert!(
            implied > mean_draw / 2.5 && implied < mean_draw * 2.5,
            "the counter implies {implied:.1} W over {elapsed:.2} s while the draw averaged \
             {mean_draw:.1} W"
        );
    }

    /// Whatever the metric, an event has to be readable on its own: which
    /// device, which driver, and what the number is a figure for.
    #[test]
    fn every_event_says_which_device_and_which_scope_it_is_for() {
        let Some(mut collector) = collector(Metric::Temp) else {
            return;
        };
        let events = collector.sample(7).expect("the device answers");
        assert_eq!(events.len(), collector.devices.len());
        for (offset, event) in events.iter().enumerate() {
            assert_eq!(event.kind, "gpu.temp");
            assert_eq!(event.seq, 7 + offset as u64);
            assert!(event.ts_ns > 0);
            assert!(event.attr("device").is_some(), "{event:?}");
            assert_eq!(event.attr("scope").unwrap().to_string(), "device");
            assert!(!event.attr("driver").unwrap().to_string().is_empty());
            let celsius = event.attr("celsius").unwrap().as_f64().unwrap();
            assert!(
                (1.0..=125.0).contains(&celsius),
                "a working GPU is between 1 and 125 C, not {celsius}"
            );
        }
    }

    /// The energy counter runs since the driver loaded, so a run that reported
    /// it raw would say a GPU drew three hundred kilojoules during a two-second
    /// measurement. The baseline is taken in `start`, before the observed
    /// program is released, and every event is the energy of this run.
    #[test]
    fn energy_is_counted_from_the_start_of_the_run_not_of_the_driver() {
        let _energy = energy_lock();
        let Some(mut collector) = collector(Metric::Power) else {
            return;
        };
        if let Err(reason) = collector.start() {
            eprintln!("skipped: {reason}");
            return;
        }

        // The bound is loose on purpose. What it separates is two readings
        // orders of magnitude apart: the energy of a run that has barely
        // started, and the counter since the driver loaded, which is hundreds
        // of kilojoules on a machine that has been up a while. A tight bound
        // would instead measure how long this thread waited for a core under a
        // parallel test run.
        let first = value(&mut collector);
        assert!(
            (0.0..1000.0).contains(&first),
            "the first reading should be this run's energy, not the driver's: {first} J"
        );

        std::thread::sleep(std::time::Duration::from_millis(300));
        let second = value(&mut collector);
        assert!(
            second >= first,
            "energy since the start of a run cannot go down: {first} then {second}"
        );
    }

    /// A field the catalog offers to filter on that no event carries is a
    /// filter that silently matches nothing -- the shape of bug the userspace
    /// filter surfaced in another family. Every `filter_fields` entry of a
    /// source this collector serves has to be on its events.
    #[test]
    fn every_filter_field_the_catalog_offers_is_on_the_events() {
        let catalog = crate::catalog::Catalog::embedded().unwrap();
        for metric in [
            Metric::Freq,
            Metric::Power,
            Metric::Temp,
            Metric::Mem,
            Metric::EngineBusy,
        ] {
            let Some(mut collector) = collector(metric) else {
                return;
            };
            // The energy counter needs its baseline before it can be read.
            if let Err(reason) = collector.start() {
                eprintln!("skipped {}: {reason}", metric.source());
                continue;
            }
            let events = collector.sample(0).expect("the device answers");
            let entry = catalog.get(&metric.source().parse().unwrap()).unwrap();
            for field in &entry.filter_fields {
                assert!(
                    events[0].attr(field).is_some(),
                    "`{}` offers `{field}` as a filter field and its events do not carry it",
                    metric.source()
                );
            }
        }
    }

    /// A process target that has gone ends the run. Reporting zero bytes for a
    /// process that no longer exists would be a series saying the program
    /// released its memory, when what happened is that it died.
    #[test]
    fn a_process_that_no_longer_exists_ends_the_run_rather_than_reporting_zero() {
        // pid 0 never names a readable process directory.
        let polling = Mode::Poll {
            interval: Mode::DEFAULT_POLL_INTERVAL,
        };
        let Ok(mut collector) = NvmlCollector::new(Metric::Mem, polling, Some(0)) else {
            eprintln!("skipped: no NVIDIA device on this machine");
            return;
        };
        match collector.sample(0) {
            Err(SampleError::TargetGone(reason)) => assert!(reason.contains("exited"), "{reason}"),
            other => panic!("expected the run to end, got {other:?}"),
        }
    }

    /// A process that has exited and not been reaped still has a `/proc` entry,
    /// so asking whether the directory is there answers yes about a process
    /// that is gone. The ground truth is a child this test leaves unreaped on
    /// purpose, which is the state a supervisor or a test harness leaves one
    /// in, and the state letter the kernel writes for it.
    #[test]
    fn a_zombie_is_departed_although_its_proc_entry_is_still_there() {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("`true` should start");
        let pid = child.id() as i32;
        // Waiting for the state rather than for a duration: the exit is the
        // event, and a sleep long enough to be safe is a sleep in every run.
        let stat = format!("/proc/{pid}/stat");
        for _ in 0..1000 {
            if std::fs::read_to_string(&stat).is_ok_and(|text| {
                text.rfind(')')
                    .and_then(|end| text[end + 1..].split_whitespace().next())
                    == Some("Z")
            }) {
                break;
            }
            std::thread::sleep(StdDuration::from_millis(1));
        }
        assert!(
            std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "an unreaped child keeps its /proc entry, which is what made this a defect"
        );
        let gone = departed(pid).expect("an unreaped child has exited");
        assert!(gone.contains("reaped"), "{gone}");
        child.wait().expect("reaping the child");
    }

    /// The repeat count is a count of what was observed, per device: a
    /// fingerprint seen again on the same device is a repeat, the same value
    /// on another device is not, and the note says how often the figure
    /// actually moved.
    #[test]
    fn a_reading_that_repeats_the_one_before_it_is_counted_per_device() {
        let mut refresh = Refresh::default();
        assert!(!refresh.record(0, 7, 0));
        assert!(!refresh.record(1, 7, 0));
        assert!(refresh.record(0, 7, 10_000_000));
        assert!(refresh.record(1, 7, 10_000_000));
        assert!(!refresh.record(0, 8, 100_000_000));
        assert!(!refresh.record(1, 9, 100_000_000));
        assert_eq!((refresh.readings, refresh.repeats), (6, 2));
        let note = refresh
            .note("energy counter", Some(10_000_000), 2)
            .expect("a run with repeats says so");
        assert!(note.contains("2 of 6 readings"), "{note}");
        assert!(
            note.contains("moved 2 times in 100 ms, about every 100 ms"),
            "{note}"
        );
        assert!(note.contains("every 10 ms"), "{note}");

        assert_eq!(Refresh::default().note("occupancy", None, 1), None);
    }

    /// The counter is held against the draw read beside it, per device, and
    /// only a gap of more than half is called: the draw moves on its own
    /// between two reads, and a check that fired on that would fire always.
    #[test]
    fn a_counter_below_half_the_draw_beside_it_is_said_and_a_close_one_is_not() {
        let reading =
            |joules: f64, watts: f64| vec![float("joules", joules), float("watts", watts)];
        let mut check = CounterCheck::default();
        for step in 1..=10u64 {
            let ns = step * 100_000_000;
            // Device 0: 3 W of counter against a 30 W draw. Device 1: 25 W
            // against 30 W, which is the ordinary disagreement of two figures.
            check.record(0, &reading(3.0 * ns as f64 / 1e9, 30.0), 0, ns);
            check.record(1, &reading(25.0 * ns as f64 / 1e9, 30.0), 0, ns);
        }
        let notes = check.notes(100_000_000);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].contains("device 0's energy counter implies 3.0 W"),
            "{notes:?}"
        );
        assert!(notes[0].contains("averaged 30.0 W"), "{notes:?}");
        assert!(notes[0].contains("every 100 ms"), "{notes:?}");

        // Two reads are too few to call anything.
        let mut short = CounterCheck::default();
        short.record(0, &reading(0.1, 30.0), 0, 500_000_000);
        short.record(0, &reading(0.2, 30.0), 0, 1_000_000_000);
        assert!(short.notes(500_000_000).is_empty());
    }

    /// The ground truth is the driver's own pace, taken from the counter that
    /// shows it: polled several times inside one refresh, the energy counter
    /// comes back unmoved most of the time; polled slower than a refresh, it
    /// almost never does. Asserted as that separation, not as a period: the
    /// board is shared, and the period is what the note reports.
    #[test]
    fn a_poll_faster_than_the_driver_refreshes_repeats_and_a_slower_one_does_not() {
        let _energy = energy_lock();
        let fraction = |pause_ms: u64, reads: u32| -> Option<f64> {
            let mut power = collector(Metric::Power)?;
            if let Err(reason) = power.start() {
                eprintln!("skipped: {reason}");
                return None;
            }
            for seq in 0..reads {
                power.sample(seq as u64).expect("the device answers");
                std::thread::sleep(StdDuration::from_millis(pause_ms));
            }
            Some(power.refresh.repeats as f64 / power.refresh.readings as f64)
        };
        let Some(fast) = fraction(5, 60) else {
            return;
        };
        let Some(slow) = fraction(300, 6) else {
            return;
        };
        assert!(
            fast > 0.5 && slow < fast / 2.0,
            "polled every 5 ms the counter repeated on {:.0} % of reads, every 300 ms on {:.0} %",
            fast * 100.0,
            slow * 100.0
        );
    }

    /// The clocks are read at the start, before the observed program runs,
    /// and every device's state reaches the run's facts.
    #[test]
    fn the_clock_state_of_every_device_is_in_the_facts() {
        let Some(mut temp) = collector(Metric::Temp) else {
            return;
        };
        temp.start().expect("a temperature source starts");
        let facts = temp.facts();
        assert_eq!(facts.len(), temp.devices.len(), "{facts:?}");
        assert!(facts[0].contains("shader clock"), "{facts:?}");
        assert!(facts[0].contains("persistence mode"), "{facts:?}");
    }

    /// A device figure is averaged over a window the driver chooses and does
    /// not report, so its timestamp carries that uncertainty rather than
    /// pretending the value was taken at an instant.
    #[test]
    fn a_driver_averaged_figure_carries_its_uncertainty() {
        let Some(mut busy) = collector(Metric::EngineBusy) else {
            return;
        };
        let events = busy.sample(0).expect("the device answers");
        for event in &events {
            assert_eq!(event.ts_err_ns, UTILIZATION_WINDOW_NS);
            let ratio = event.attr("ratio").unwrap().as_f64().unwrap();
            assert!((0.0..=1.0).contains(&ratio), "{ratio} is not a ratio");
        }

        // The clock, read at the call, is not averaged: its error is the span
        // of its own read, bounded by the time the whole sample took, and far
        // under the averaging window.
        let Some(mut clocks) = collector(Metric::Freq) else {
            return;
        };
        let before = monotonic_ns();
        let event = clocks.sample(0).unwrap().remove(0);
        let spent = monotonic_ns() - before;
        assert!(
            u64::from(event.ts_err_ns) <= spent,
            "{} ns of error for a read inside {spent} ns",
            event.ts_err_ns
        );
        assert!(u64::from(event.ts_err_ns) < u64::from(UTILIZATION_WINDOW_NS));
    }
}
