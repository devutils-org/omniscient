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

use std::path::Path;

use crate::clock::monotonic_ns;
use crate::collect::{Collector, Prepared, SampleError};
use crate::event::{Attr, AttrValue, CollectorId, Event, KeyValue};
use crate::request::Target;

use super::nvml::{DeviceHandle, Nvml, NvmlError};

/// NVML averages utilization over a window it chooses and does not report.
/// NVIDIA documents that window as at most one second, which is therefore how
/// far from its timestamp such a sample may be.
const UTILIZATION_WINDOW_NS: u32 = 1_000_000_000;

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

/// Holds the choice of metric until the target exists.
pub struct Prepare(pub Metric);

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        // A device figure needs no process, so no target is refused here. What
        // the target decides is `gpu.mem`: with a process it reports that
        // process's VRAM, without one the device's.
        NvmlCollector::new(self.0, target.first_pid())
            .map(|collector| Box::new(collector) as Box<dyn Collector>)
    }
}

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
}

/// Reads one number per device per tick.
pub struct NvmlCollector {
    id: CollectorId,
    metric: Metric,
    nvml: Nvml,
    devices: Vec<Device>,
    /// The driver, which is part of what a number means: NVML's semantics are
    /// the driver's, not a kernel interface's.
    driver: String,
    /// The process this run is scoped to, when there is one. For the
    /// device-wide metrics it only sets the window; for `gpu.mem` it decides
    /// whether the figure is a process's or the device's.
    pid: Option<i32>,
}

impl NvmlCollector {
    fn new(metric: Metric, pid: Option<i32>) -> Result<Self, String> {
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
            });
        }

        let driver = nvml
            .driver_version()
            .unwrap_or_else(|| "unknown".to_owned());
        Ok(Self {
            id: CollectorId::new(format!("{}/vendor:nvml", metric.source())),
            metric,
            nvml,
            devices,
            driver,
            pid,
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

    /// VRAM held: by the process this run observes, or by the device when the
    /// run observes no process.
    ///
    /// The two never share a series. A process figure and a device figure are
    /// both bytes of VRAM and mean entirely different things, so the target
    /// decides which one this is and the `scope` attribute records the answer.
    ///
    /// A process absent from the driver's list holds nothing, which is reported
    /// as zero: an empty series and a series of zeros say different things
    /// about a run, and only one of them is true here.
    fn mem(&self, device: &Device) -> Result<(Vec<Attr>, u32), NvmlError> {
        let memory = self.nvml.memory(device.handle)?;
        let Some(pid) = self.pid else {
            let mut attrs = self.device_attrs(device, "device");
            attrs.push(Attr::uint("bytes", memory.used));
            attrs.push(Attr::uint("device_total_bytes", memory.total));
            return Ok((attrs, 0));
        };

        let processes = self.nvml.processes(device.handle)?;
        let mut held = 0u64;
        let mut kinds: Vec<&'static str> = Vec::new();
        for (kind, process) in processes {
            if process.pid as i32 != pid {
                continue;
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
        if self.metric != Metric::Power {
            return Ok(());
        }
        // An energy counter is the difference between two readings, so the
        // first one is taken before the observed program runs. A device without
        // the counter is refused here rather than served an integral of sampled
        // power under the name of energy.
        for device in &mut self.devices {
            match self.nvml.energy_millijoules(device.handle) {
                Ok(millijoules) => device.energy_base_mj = millijoules,
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
            if !Path::new(&format!("/proc/{pid}")).exists() {
                return Err(SampleError::TargetGone(format!("process {pid} exited")));
            }
        }

        let mut events = Vec::with_capacity(self.devices.len());
        for (offset, device) in self.devices.iter().enumerate() {
            let read = match self.metric {
                Metric::Freq => self.freq(device),
                Metric::Power => self.power(device),
                Metric::Temp => self.temp(device),
                Metric::Mem => self.mem(device),
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

    /// Builds a collector for this process, or explains why this machine
    /// cannot run the test. Most machines have no NVIDIA device and must skip
    /// cleanly: a test that fails there says nothing about the code.
    fn collector(metric: Metric) -> Option<NvmlCollector> {
        match NvmlCollector::new(metric, Some(std::process::id() as i32)) {
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
        let Ok(mut collector) = NvmlCollector::new(Metric::Mem, Some(0)) else {
            eprintln!("skipped: no NVIDIA device on this machine");
            return;
        };
        match collector.sample(0) {
            Err(SampleError::TargetGone(reason)) => assert!(reason.contains("exited"), "{reason}"),
            other => panic!("expected the run to end, got {other:?}"),
        }
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

        // The clock, read at the call, is not averaged and says so.
        let Some(mut clocks) = collector(Metric::Freq) else {
            return;
        };
        assert_eq!(clocks.sample(0).unwrap()[0].ts_err_ns, 0);
    }
}
