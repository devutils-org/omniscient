//! `power.temp` through hwmon.
//!
//! Temperature is in this catalog for one reason: it is the confounder. A run
//! that thermally throttled is not a slower run, it is a different run, and
//! averaging it in with the others is the failure this family exists to
//! prevent. So the number that matters is the one the hardware throttles on --
//! the processor's own sensor -- and not the average of everything `/sys`
//! happens to expose.
//!
//! That is why this collector does not report "the temperature". A laptop here
//! offers a wifi chip at 52 °C, an NVMe controller at 44 °C and a processor at
//! 56 °C; the mean of the three is a number about nothing. Every sensor is
//! reported as its own attribute, and the value of the event is the hottest
//! *processor* sensor, with `sensor` naming which one it was and `basis` saying
//! how it was chosen.

use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, AttrValue, CollectorId, Event};
use crate::request::{Mode, Target};

use super::sysfs;

const HWMON: &str = "/sys/class/hwmon";

/// What a super-I/O chip (`nct67xx`, `it87`) reads on an input no diode is
/// wired to: -128 C, the 8-bit register's 0x80. It is no temperature, and
/// taken as one it would be the coldest sensor of the machine.
const UNWIRED_MILLIDEGREES: i64 = -128_000;

/// hwmon chips that are the processor's own thermal sensors.
///
/// Matched by the driver name rather than by the label, because the labels
/// differ per vendor -- `Tctl` and `Tdie` on AMD, `Package id 0` and `Core N`
/// on Intel -- while the driver is what says whose temperature it is.
const CPU_CHIPS: [&str; 5] = [
    "k10temp",
    "coretemp",
    "zenpower",
    "cpu_thermal",
    "cpu-thermal",
];

#[derive(Debug)]
pub struct Prepare {
    mode: Mode,
}

impl Prepare {
    pub fn new(mode: Mode) -> Result<Self, String> {
        match mode {
            Mode::Poll { .. } | Mode::Snapshot => Ok(Self { mode }),
            other => Err(format!(
                "a temperature is a value that is read, not a stream of events: `{}` cannot be \
                 served from hwmon; poll it, or take one snapshot",
                other.kind()
            )),
        }
    }
}

impl Prepared for Prepare {
    /// hwmon sensors belong to the machine, not to a process: the chip measures
    /// silicon, and silicon does not know which pid heated it.
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        match target {
            Target::System => Ok(Box::new(Temperatures::new(self.mode, PathBuf::from(HWMON)))),
            other => Err(format!(
                "hwmon reports what the hardware measures, and a {} target asks for a share of \
                 it that no sensor breaks down; use --system",
                other.kind()
            )),
        }
    }
}

/// One `tempN_input` file, with what it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Sensor {
    /// The driver: `k10temp`, `coretemp`, `nvme`.
    chip: String,
    /// The label the chip gives this input, or `tempN` when it gives none.
    label: String,
    input: PathBuf,
    /// Whether this is a processor sensor, which is the one throttling
    /// follows.
    is_cpu: bool,
}

impl Sensor {
    fn attribute(&self) -> String {
        sysfs::attribute_name(&self.chip, &self.label)
    }

    fn name(&self) -> String {
        format!("{}/{}", self.chip, self.label)
    }
}

pub struct Temperatures {
    mode: Mode,
    root: PathBuf,
    sensors: Vec<Sensor>,
    losses: Losses,
    /// The inputs that read [`UNWIRED_MILLIDEGREES`], left out and said.
    unwired: BTreeSet<String>,
}

impl Temperatures {
    pub fn new(mode: Mode, root: PathBuf) -> Self {
        Self {
            mode,
            root,
            sensors: Vec::new(),
            losses: Losses::default(),
            unwired: BTreeSet::new(),
        }
    }

    /// Walks hwmon once, at the start: chips do not appear and disappear during
    /// a run, and re-walking on every tick would cost more than the reads.
    fn discover(&self) -> Vec<Sensor> {
        let mut sensors = Vec::new();
        let Ok(chips) = sysfs::entries(&self.root) else {
            return sensors;
        };
        for chip_path in chips {
            let chip = sysfs::text(&chip_path.join("name")).unwrap_or_else(|_| {
                chip_path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "hwmon".to_owned())
            });
            let is_cpu = CPU_CHIPS.contains(&chip.as_str());
            let Ok(files) = sysfs::entries(&chip_path) else {
                continue;
            };
            for file in files {
                let name = file.file_name().unwrap_or_default().to_string_lossy();
                let Some(index) = name
                    .strip_prefix("temp")
                    .and_then(|rest| rest.strip_suffix("_input"))
                else {
                    continue;
                };
                let label = sysfs::text(&chip_path.join(format!("temp{index}_label")))
                    .unwrap_or_else(|_| format!("temp{index}"));
                sensors.push(Sensor {
                    chip: chip.clone(),
                    label,
                    input: file.clone(),
                    is_cpu,
                });
            }
        }
        sensors
    }

    /// Reads every sensor, keeping what was read and counting what was not.
    fn read(&mut self) -> Vec<(Sensor, f64)> {
        let mut readings = Vec::new();
        for sensor in &self.sensors {
            match sysfs::signed(&sensor.input) {
                Ok(UNWIRED_MILLIDEGREES) => {
                    self.unwired.insert(sensor.name());
                }
                // hwmon reports thousandths of a degree.
                Ok(millidegrees) => readings.push((sensor.clone(), millidegrees as f64 / 1000.0)),
                Err(error) => {
                    self.losses.failed_reads += 1;
                    self.losses
                        .note(format!("{} could not be read: {error}", sensor.name()));
                }
            }
        }
        readings
    }
}

impl Collector for Temperatures {
    fn value_attr(&self) -> &'static str {
        "celsius"
    }

    fn start(&mut self) -> Result<(), String> {
        self.sensors = self.discover();
        if self.sensors.is_empty() {
            return Err(format!(
                "no temperature sensor was found under {}: this machine exposes none, or the \
                 driver for its chip is not loaded",
                self.root.display()
            ));
        }
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let readings = self.read();
        if readings.is_empty() {
            return Err(SampleError::TargetGone(
                "every temperature sensor stopped reading".to_owned(),
            ));
        }
        let ts_ns = monotonic_ns();

        // The processor's sensors decide the value; everything else is carried
        // but never averaged in. A machine without one says so rather than
        // quietly answering with a disk controller's temperature.
        let cpu_readings: Vec<&(Sensor, f64)> = readings
            .iter()
            .filter(|(sensor, _)| sensor.is_cpu)
            .collect();
        let (hottest, basis) = if let Some(hottest) = hottest(cpu_readings.into_iter()) {
            (
                hottest,
                "the hottest processor sensor, which is what throttling follows",
            )
        } else {
            (
                hottest(readings.iter()).expect("readings is not empty"),
                "no processor sensor was found, so this is the hottest sensor of any kind",
            )
        };

        let mut attrs = vec![
            Attr {
                name: "celsius".to_owned(),
                value: AttrValue::Float(hottest.1),
            },
            Attr::text("sensor", hottest.0.name()),
            Attr::text("basis", basis),
        ];
        for (sensor, celsius) in &readings {
            attrs.push(Attr {
                name: sensor.attribute(),
                value: AttrValue::Float(*celsius),
            });
        }

        Ok(vec![Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "power.temp".to_owned(),
            keys: Vec::new(),
            attrs,
            collector_id: CollectorId::unstamped(),
            seq,
        }])
    }

    fn tick(&self) -> Option<std::time::Duration> {
        match self.mode {
            Mode::Snapshot => Some(std::time::Duration::ZERO),
            _ => None,
        }
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    fn notes(&self) -> Vec<String> {
        self.unwired
            .iter()
            .map(|name| {
                format!(
                    "{name} reads -128 C, what its chip says of an input no sensor is wired \
                     to: left out"
                )
            })
            .collect()
    }
}

/// The hottest of a set of readings, cloned out of the borrow.
fn hottest<'a>(readings: impl Iterator<Item = &'a (Sensor, f64)>) -> Option<(Sensor, f64)> {
    readings
        .fold(None::<&(Sensor, f64)>, |best, reading| match best {
            Some(best) if best.1 >= reading.1 => Some(best),
            _ => Some(reading),
        })
        .cloned()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::collect::power::sysfs::scratch;

    fn poll() -> Mode {
        Mode::Poll {
            interval: Mode::DEFAULT_POLL_INTERVAL,
        }
    }

    fn celsius(event: &Event) -> f64 {
        event.attr("celsius").unwrap().as_f64().unwrap()
    }

    /// A machine offers a processor, a disk controller and a wifi chip. The
    /// hottest of them is usually not the processor, and throttling follows the
    /// processor.
    #[test]
    fn the_value_is_the_processors_sensor_even_when_something_else_is_hotter() {
        let root = scratch::tree("hwmon");
        scratch::write(&root, "hwmon0/name", "nvme");
        scratch::write(&root, "hwmon0/temp1_input", "84000");
        scratch::write(&root, "hwmon0/temp1_label", "Composite");
        scratch::write(&root, "hwmon1/name", "k10temp");
        scratch::write(&root, "hwmon1/temp1_input", "55625");
        scratch::write(&root, "hwmon1/temp1_label", "Tctl");
        scratch::write(&root, "hwmon1/temp3_input", "57500");
        scratch::write(&root, "hwmon1/temp3_label", "Tccd1");

        let mut collector = Temperatures::new(poll(), root.clone());
        collector.start().expect("sensors are there");
        let events = collector.sample(0).expect("a read");

        assert_eq!(celsius(&events[0]), 57.5, "Tccd1 is the hottest cpu sensor");
        assert_eq!(
            events[0].attr("sensor").unwrap().to_string(),
            "k10temp/Tccd1"
        );
        assert!(events[0]
            .attr("basis")
            .unwrap()
            .to_string()
            .contains("processor sensor"));
        // The disk is still reported; it is simply not the value.
        assert_eq!(
            events[0].attr("nvme.composite").unwrap().as_f64(),
            Some(84.0)
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// With nothing to call a processor sensor, the fallback is stated rather
    /// than passed off as the same measurement.
    #[test]
    fn without_a_processor_sensor_the_event_says_what_it_fell_back_to() {
        let root = scratch::tree("hwmon-nocpu");
        scratch::write(&root, "hwmon0/name", "nvme");
        scratch::write(&root, "hwmon0/temp1_input", "44000");

        let mut collector = Temperatures::new(poll(), root.clone());
        collector.start().expect("one sensor is there");
        let events = collector.sample(0).expect("a read");

        assert_eq!(celsius(&events[0]), 44.0);
        assert_eq!(events[0].attr("sensor").unwrap().to_string(), "nvme/temp1");
        assert!(events[0]
            .attr("basis")
            .unwrap()
            .to_string()
            .contains("no processor sensor"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_machine_with_no_sensor_at_all_refuses_rather_than_reporting_nothing() {
        let root = scratch::tree("hwmon-empty");
        let mut collector = Temperatures::new(poll(), root.clone());
        let refused = collector.start().expect_err("there is nothing to read");
        assert!(refused.contains("no temperature sensor"), "{refused}");
        std::fs::remove_dir_all(&root).ok();
    }

    /// A chip whose module is unloaded mid-run is a read that failed, counted
    /// and explained, while the remaining sensors still give a temperature.
    #[test]
    fn a_sensor_that_stops_answering_is_a_counted_loss() {
        let root = scratch::tree("hwmon-vanishing");
        scratch::write(&root, "hwmon0/name", "k10temp");
        scratch::write(&root, "hwmon0/temp1_input", "55000");
        scratch::write(&root, "hwmon1/name", "nvme");
        scratch::write(&root, "hwmon1/temp1_input", "44000");

        let mut collector = Temperatures::new(poll(), root.clone());
        collector.start().expect("sensors are there");
        collector.sample(0).expect("a first read");

        std::fs::remove_file(root.join("hwmon1/temp1_input")).expect("the disk chip goes away");
        let events = collector.sample(1).expect("the processor still answers");
        assert_eq!(celsius(&events[0]), 55.0);
        let losses = collector.losses();
        assert_eq!(losses.failed_reads, 1);
        assert!(losses.reasons.keys().any(|reason| reason.contains("nvme")));

        // When the last one goes, the run ends rather than reporting nothing.
        std::fs::remove_file(root.join("hwmon0/temp1_input")).expect("the processor goes away");
        match collector.sample(2) {
            Err(SampleError::TargetGone(_)) => {}
            other => panic!("expected the run to end, got {other:?}"),
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// A snapshot is one read and then the run is over: the collector says so
    /// through its tick rather than leaving the runner to wait out an interval.
    #[test]
    fn a_snapshot_asks_to_be_read_once_and_immediately() {
        let root = scratch::tree("hwmon-snapshot");
        scratch::write(&root, "hwmon0/name", "k10temp");
        scratch::write(&root, "hwmon0/temp1_input", "50000");

        let polled = Temperatures::new(poll(), root.clone());
        let snapshot = Temperatures::new(Mode::Snapshot, root.clone());
        assert_eq!(snapshot.tick(), Some(std::time::Duration::ZERO));
        assert_eq!(
            polled.tick(),
            None,
            "a poll takes its interval from the request"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// Silicon does not know which process heated it, so a target that names
    /// one is refused rather than answered with the machine's temperature.
    #[test]
    fn a_process_target_is_refused_rather_than_answered_machine_wide() {
        let prepared = Box::new(Prepare::new(poll()).expect("poll is servable"));
        let refused = prepared
            .attach(&Target::Pid(vec![1]))
            .err()
            .expect("a pid has no temperature");
        assert!(refused.contains("--system"), "{refused}");
    }

    /// Ground truth: the thermal zone interface exposes some of the same chips
    /// as hwmon, by a different path. Where a zone names a chip this collector
    /// read, the two must agree -- they are the same silicon.
    #[test]
    fn a_reading_agrees_with_the_thermal_zone_for_the_same_chip() {
        let mut collector = Temperatures::new(poll(), PathBuf::from(HWMON));
        if collector.start().is_err() {
            return;
        }
        let readings = collector.read();
        let mut compared = 0;

        let Ok(zones) = sysfs::entries(Path::new("/sys/class/thermal")) else {
            return;
        };
        for zone in zones {
            let Ok(kind) = sysfs::text(&zone.join("type")) else {
                continue;
            };
            let Ok(millidegrees) = sysfs::signed(&zone.join("temp")) else {
                continue;
            };
            let zone_celsius = millidegrees as f64 / 1000.0;
            for (sensor, celsius) in &readings {
                if sensor.chip != kind {
                    continue;
                }
                assert!(
                    (celsius - zone_celsius).abs() < 5.0,
                    "hwmon says {} is at {celsius} and {} says {zone_celsius}",
                    sensor.name(),
                    zone.display()
                );
                compared += 1;
            }
        }
        // Nothing to compare is not a failure: most machines expose no zone for
        // their processor. The assertion above is the point.
        let _ = compared;
    }

    /// A super-I/O input with nothing wired to it reads -128 C: left out of
    /// the readings, so it is neither a temperature nor a failed read, and
    /// said in a note.
    #[test]
    fn an_unwired_input_is_left_out_and_said() {
        let root = scratch::tree("temp-unwired");
        scratch::write(&root, "hwmon0/name", "nct6797");
        scratch::write(&root, "hwmon0/temp1_input", "31000");
        scratch::write(&root, "hwmon0/temp1_label", "CPUTIN");
        scratch::write(&root, "hwmon0/temp2_input", "-128000");
        scratch::write(&root, "hwmon0/temp2_label", "AUXTIN1");

        let mut collector = Temperatures::new(poll(), root.clone());
        collector.start().expect("sensors are there");
        let events = collector.sample(0).expect("a read");

        assert_eq!(celsius(&events[0]), 31.0);
        assert!(events[0]
            .attr(
                &Sensor {
                    chip: "nct6797".to_owned(),
                    label: "AUXTIN1".to_owned(),
                    input: PathBuf::new(),
                    is_cpu: false,
                }
                .attribute()
            )
            .is_none());
        assert_eq!(collector.losses().failed_reads, 0);
        let notes = collector.notes();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("nct6797/AUXTIN1"), "{notes:?}");
        std::fs::remove_dir_all(&root).ok();
    }

    /// Whatever the machine, a sensor reading has to be a temperature.
    #[test]
    fn this_machines_sensors_report_plausible_temperatures() {
        let mut collector = Temperatures::new(poll(), PathBuf::from(HWMON));
        if collector.start().is_err() {
            return;
        }
        for (sensor, celsius) in collector.read() {
            assert!(
                (-40.0..150.0).contains(&celsius),
                "{} reported {celsius} C",
                sensor.name()
            );
        }
    }
}
