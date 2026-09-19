//! `power.freq` through `cpufreq` in sysfs.
//!
//! There are two different numbers called "the frequency" and telling them
//! apart is the whole of this collector. The *requested* frequency is what the
//! governor asked for. The *effective* frequency is what the core ran at, which
//! is lower whenever the part was power or thermally limited, and lower again
//! under an instruction mix that carries a frequency licence such as AVX-512.
//! Reporting one as the other hides exactly the effect this family exists to
//! expose.
//!
//! Which one sysfs gives depends on the driver, so the file is chosen at the
//! start and every event says which it read:
//!
//! - `cpuinfo_avg_freq`, where the driver provides it, is an average over the
//!   interval since the previous read, computed by the processor's own
//!   performance counters. That is the effective frequency, and it is the only
//!   one of the three that describes a window rather than an instant.
//! - `cpuinfo_cur_freq` is read back from the hardware at the instant of the
//!   read.
//! - `scaling_cur_freq` is what the kernel believes the frequency is. With
//!   some drivers that is a hardware reading and with others it is the last
//!   setpoint, so the driver's name travels with the number.

use std::path::PathBuf;

use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, AttrValue, CollectorId, Event, KeyValue};
use crate::request::{Mode, Target};

use super::sysfs;

const CPU_ROOT: &str = "/sys/devices/system/cpu";

/// Which file a cpu's frequency is read from, best first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Average,
    Hardware,
    Believed,
}

impl Source {
    const ORDER: [Source; 3] = [Source::Average, Source::Hardware, Source::Believed];

    fn file(self) -> &'static str {
        match self {
            Source::Average => "cpuinfo_avg_freq",
            Source::Hardware => "cpuinfo_cur_freq",
            Source::Believed => "scaling_cur_freq",
        }
    }

    fn basis(self, driver: &str) -> String {
        match self {
            Source::Average => "cpuinfo_avg_freq: the effective frequency, averaged over the \
                                interval by the processor's own performance counters"
                .to_owned(),
            Source::Hardware => {
                "cpuinfo_cur_freq: the frequency read back from the hardware at this instant"
                    .to_owned()
            }
            Source::Believed => format!(
                "scaling_cur_freq: what the kernel believes the frequency is; with the `{driver}` \
                 driver this may be the requested frequency rather than the effective one"
            ),
        }
    }

    /// Whether the number is the frequency the core ran at, as opposed to the
    /// one it was asked to run at.
    fn is_effective(self) -> bool {
        matches!(self, Source::Average | Source::Hardware)
    }
}

#[derive(Debug)]
pub struct Prepare {
    mode: Mode,
}

impl Prepare {
    pub fn new(mode: Mode) -> Result<Self, String> {
        match mode {
            Mode::Poll { .. } => Ok(Self { mode }),
            Mode::Trace => Err(
                "a frequency change is an event, and events come from the `power:cpu_frequency` \
                 tracepoint: sysfs only holds the current value, so poll it"
                    .to_owned(),
            ),
            other => Err(format!(
                "cpufreq holds a value to be read, not a counter: `{}` has nothing to accumulate; \
                 poll it",
                other.kind()
            )),
        }
    }
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let root = PathBuf::from(CPU_ROOT);
        let cpus = match target {
            Target::System => sysfs::online_cpus(&root)
                .map_err(|error| format!("cannot tell which cpus are online: {error}"))?,
            Target::Cpu(set) => set.cpus().to_vec(),
            other => {
                return Err(format!(
                    "a frequency belongs to a core, not to a {}: the cores a process ran on are \
                     what `sched` reports; use --cpu or --system",
                    other.kind()
                ))
            }
        };
        Ok(Box::new(Frequencies::new(self.mode, root, cpus)))
    }
}

pub struct Frequencies {
    id: CollectorId,
    #[allow(dead_code)]
    mode: Mode,
    root: PathBuf,
    cpus: Vec<u32>,
    /// Chosen once, at the start: the same file for every cpu, so one event
    /// never mixes an effective frequency with a requested one.
    source: Option<Source>,
    driver: String,
    losses: Losses,
}

impl Frequencies {
    pub fn new(mode: Mode, root: PathBuf, cpus: Vec<u32>) -> Self {
        Self {
            id: CollectorId::new("power.freq/fs:sys"),
            mode,
            root,
            cpus,
            source: None,
            driver: "unknown".to_owned(),
            losses: Losses::default(),
        }
    }

    fn cpufreq(&self, cpu: u32) -> PathBuf {
        self.root.join(format!("cpu{cpu}")).join("cpufreq")
    }
}

impl Collector for Frequencies {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "hz"
    }

    fn start(&mut self) -> Result<(), String> {
        let Some(&first) = self.cpus.first() else {
            return Err("no cpu to read".to_owned());
        };
        let directory = self.cpufreq(first);
        if !directory.exists() {
            return Err(format!(
                "{} does not exist: this kernel has no cpufreq driver for cpu{first}, which is \
                 the ordinary case in a virtual machine",
                directory.display()
            ));
        }
        self.driver =
            sysfs::text(&directory.join("scaling_driver")).unwrap_or_else(|_| "unknown".to_owned());
        self.source = Source::ORDER
            .into_iter()
            .find(|source| sysfs::number(&directory.join(source.file())).is_ok());
        match self.source {
            Some(_) => Ok(()),
            None => Err(format!(
                "none of {} is readable under {}",
                Source::ORDER
                    .iter()
                    .map(|source| source.file())
                    .collect::<Vec<_>>()
                    .join(", "),
                directory.display()
            )),
        }
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let source = self.source.expect("start chose a source");
        let mut per_cpu: Vec<(u32, f64)> = Vec::new();
        for &cpu in &self.cpus {
            match sysfs::number(&self.cpufreq(cpu).join(source.file())) {
                // cpufreq counts in kilohertz.
                Ok(kilohertz) => per_cpu.push((cpu, kilohertz as f64 * 1_000.0)),
                Err(error) => {
                    self.losses.failed_reads += 1;
                    self.losses.note(format!("cpu{cpu}: {error}"));
                }
            }
        }
        if per_cpu.is_empty() {
            return Err(SampleError::TargetGone(
                "no cpu's frequency could be read any more".to_owned(),
            ));
        }
        let ts_ns = monotonic_ns();

        let float = |name: &str, value: f64| Attr {
            name: name.to_owned(),
            value: AttrValue::Float(value),
        };
        let basis = source.basis(&self.driver);
        let measures = if source.is_effective() {
            "effective"
        } else {
            "requested"
        };

        // One event per cpu, keyed on it. The mean of twenty-four cores
        // describes none of them -- one boosted core among twenty-three idle
        // ones reads as a machine at a middling clock -- and a reader asking
        // which core is hot cannot get there from a single number. The mean is
        // still available, because averaging a series is something the emit
        // layer does and splitting one apart is not.
        Ok(per_cpu
            .iter()
            .enumerate()
            .map(|(offset, (cpu, hertz))| Event {
                ts_ns,
                ts_err_ns: 0,
                kind: "power.freq".to_owned(),
                keys: vec![KeyValue::cpu(*cpu)],
                attrs: vec![
                    float("hz", *hertz),
                    Attr::uint("cpu", u64::from(*cpu)),
                    Attr::text("driver", self.driver.clone()),
                    Attr::text("basis", basis.clone()),
                    Attr::text("measures", measures),
                ],
                collector_id: self.id.clone(),
                seq: seq + offset as u64,
            })
            .collect())
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }
}

/// The frequency bounds the kernel itself declares for a cpu, so a reading can
/// be checked against something this collector did not compute.
#[cfg(test)]
fn declared_range(root: &std::path::Path, cpu: u32) -> Option<(f64, f64)> {
    let directory = root.join(format!("cpu{cpu}")).join("cpufreq");
    let min = sysfs::number(&directory.join("cpuinfo_min_freq")).ok()? as f64 * 1_000.0;
    let max = sysfs::number(&directory.join("cpuinfo_max_freq")).ok()? as f64 * 1_000.0;
    Some((min, max))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::power::sysfs::scratch;

    fn poll() -> Mode {
        Mode::Poll {
            interval: Mode::DEFAULT_POLL_INTERVAL,
        }
    }

    fn cpu_tree(name: &str, driver: &str, files: &[(&str, &str)]) -> PathBuf {
        let root = scratch::tree(name);
        scratch::write(&root, "online", "0-1");
        for cpu in 0..2 {
            scratch::write(&root, &format!("cpu{cpu}/cpufreq/scaling_driver"), driver);
            for (file, value) in files {
                scratch::write(&root, &format!("cpu{cpu}/cpufreq/{file}"), value);
            }
        }
        root
    }

    /// The whole point of the source: the same field name can hold the
    /// frequency the core ran at or the one it was asked to run at, and the
    /// event has to say which.
    #[test]
    fn the_file_it_read_decides_whether_the_number_is_effective() {
        let root = cpu_tree(
            "freq-avg",
            "amd_pstate",
            &[
                ("cpuinfo_avg_freq", "4274225"),
                ("scaling_cur_freq", "4400000"),
            ],
        );
        let mut collector = Frequencies::new(poll(), root.clone(), vec![0, 1]);
        collector.start().expect("a cpufreq directory");
        let events = collector.sample(0).expect("a read");

        assert_eq!(
            events[0].attr("hz").unwrap().as_f64(),
            Some(4_274_225_000.0)
        );
        assert_eq!(events[0].attr("measures").unwrap().to_string(), "effective");
        assert!(events[0]
            .attr("basis")
            .unwrap()
            .to_string()
            .starts_with("cpuinfo_avg_freq"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_driver_that_offers_only_its_setpoint_is_reported_as_requested() {
        let root = cpu_tree(
            "freq-scaling",
            "acpi-cpufreq",
            &[("scaling_cur_freq", "2200000")],
        );
        let mut collector = Frequencies::new(poll(), root.clone(), vec![0, 1]);
        collector.start().expect("a cpufreq directory");
        let events = collector.sample(0).expect("a read");

        assert_eq!(
            events[0].attr("hz").unwrap().as_f64(),
            Some(2_200_000_000.0)
        );
        assert_eq!(events[0].attr("measures").unwrap().to_string(), "requested");
        assert!(events[0]
            .attr("basis")
            .unwrap()
            .to_string()
            .contains("acpi-cpufreq"));
        assert_eq!(
            events[0].attr("driver").unwrap().to_string(),
            "acpi-cpufreq"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// Two cores at different frequencies are two series, not one mean.
    ///
    /// This used to report `hz = 3.3 GHz` with the spread beside it, which is
    /// a frequency neither core ran at. The mean is still obtainable -- the
    /// emit layer averages a series -- and the per-core answer is not
    /// obtainable from a mean, which is why the split goes this way round.
    #[test]
    fn two_cores_at_different_frequencies_are_two_series() {
        let root = scratch::tree("freq-spread");
        scratch::write(&root, "cpu0/cpufreq/scaling_driver", "amd_pstate");
        scratch::write(&root, "cpu0/cpufreq/cpuinfo_avg_freq", "4400000");
        scratch::write(&root, "cpu1/cpufreq/scaling_driver", "amd_pstate");
        scratch::write(&root, "cpu1/cpufreq/cpuinfo_avg_freq", "2200000");

        let mut collector = Frequencies::new(poll(), root.clone(), vec![0, 1]);
        collector.start().expect("a cpufreq directory");
        let events = collector.sample(0).expect("a read");

        assert_eq!(events.len(), 2, "one event per cpu");
        assert_eq!(events[0].keys[0].to_string(), "cpu:0");
        assert_eq!(events[0].attr("hz").unwrap().as_f64(), Some(4.4e9));
        assert_eq!(events[1].keys[0].to_string(), "cpu:1");
        assert_eq!(events[1].attr("hz").unwrap().as_f64(), Some(2.2e9));
        // `cpu` is a declared filter field, and a key is not a filter field.
        assert_eq!(events[1].attr("cpu").unwrap().as_f64(), Some(1.0));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_machine_without_cpufreq_refuses_with_the_reason() {
        let root = scratch::tree("freq-none");
        let mut collector = Frequencies::new(poll(), root.clone(), vec![0]);
        let refused = collector.start().expect_err("nothing to read");
        assert!(refused.contains("no cpufreq driver"), "{refused}");
        std::fs::remove_dir_all(&root).ok();
    }

    /// `--cpu` is how to ask about a core rather than about a mean, so the
    /// collector must read those cores and only those.
    #[test]
    fn a_cpu_target_narrows_the_reading_to_those_cores() {
        let root = scratch::tree("freq-cpus");
        scratch::write(&root, "online", "0-3");
        for cpu in 0..4 {
            scratch::write(
                &root,
                &format!("cpu{cpu}/cpufreq/scaling_driver"),
                "amd_pstate",
            );
            scratch::write(
                &root,
                &format!("cpu{cpu}/cpufreq/cpuinfo_avg_freq"),
                &format!("{}", 1_000_000 + cpu * 1_000_000),
            );
        }

        let mut collector = Frequencies::new(poll(), root.clone(), vec![2, 3]);
        collector.start().expect("a cpufreq directory");
        let events = collector.sample(0).expect("a read");

        assert_eq!(events.len(), 2, "two cpus were asked for");
        assert_eq!(events[0].keys[0].to_string(), "cpu:2");
        assert_eq!(events[0].attr("hz").unwrap().as_f64(), Some(3e9));
        assert_eq!(events[1].keys[0].to_string(), "cpu:3");
        assert_eq!(events[1].attr("hz").unwrap().as_f64(), Some(4e9));
        assert!(
            events
                .iter()
                .all(|event| event.keys[0].to_string() != "cpu:0"),
            "a cpu that was not asked for must not be in the reading"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// A cpu going offline mid-run is a read that failed, counted and
    /// explained; the other cpus still give a number.
    #[test]
    fn a_cpu_that_stops_answering_is_a_counted_loss_not_a_silent_gap() {
        let root = cpu_tree(
            "freq-offline",
            "amd_pstate",
            &[("cpuinfo_avg_freq", "3000000")],
        );
        let mut collector = Frequencies::new(poll(), root.clone(), vec![0, 1]);
        collector.start().expect("a cpufreq directory");

        std::fs::remove_file(root.join("cpu1/cpufreq/cpuinfo_avg_freq")).expect("cpu1 goes away");
        let events = collector.sample(0).expect("cpu0 still answers");

        assert_eq!(events.len(), 1, "only cpu0 still answers");
        assert_eq!(events[0].keys[0].to_string(), "cpu:0");
        let losses = collector.losses();
        assert_eq!(losses.failed_reads, 1);
        assert!(!losses.is_clean());
        assert!(losses.reasons.keys().any(|reason| reason.contains("cpu1")));

        // And when the last one goes, the run ends rather than reporting a
        // frequency nobody read.
        std::fs::remove_file(root.join("cpu0/cpufreq/cpuinfo_avg_freq")).expect("cpu0 goes away");
        match collector.sample(1) {
            Err(SampleError::TargetGone(_)) => {}
            other => panic!("expected the run to end, got {other:?}"),
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// A frequency belongs to a core. Asked about a process, this backend says
    /// so rather than reporting the machine's mean under that process's name.
    #[test]
    fn a_process_target_is_refused_with_what_would_answer_it() {
        let prepared = Box::new(Prepare::new(poll()).expect("poll is servable"));
        let refused = prepared
            .attach(&Target::Pid(vec![1]))
            .err()
            .expect("a pid has no frequency of its own");
        assert!(refused.contains("--cpu"), "{refused}");
        assert!(refused.contains("sched"), "{refused}");
    }

    #[test]
    fn a_mode_cpufreq_cannot_serve_is_refused_with_the_reason() {
        let refused = Prepare::new(Mode::Trace).expect_err("sysfs holds no events");
        assert!(refused.contains("cpu_frequency"), "{refused}");
        let refused = Prepare::new(Mode::Count).expect_err("there is nothing to accumulate");
        assert!(refused.contains("poll it"), "{refused}");
    }

    /// Ground truth against the kernel's own declaration: whatever this machine
    /// reports, it has to lie inside the range the driver publishes for that
    /// cpu. A unit error -- kilohertz left as hertz -- fails this by three
    /// orders of magnitude.
    #[test]
    fn every_reading_lies_inside_the_range_this_kernel_declares() {
        let root = PathBuf::from(CPU_ROOT);
        let Ok(cpus) = sysfs::online_cpus(&root) else {
            return;
        };
        let mut collector = Frequencies::new(poll(), root.clone(), cpus.clone());
        if collector.start().is_err() {
            return;
        }
        let events = collector.sample(0).expect("a read");
        for cpu in cpus {
            let Some((min, max)) = declared_range(&root, cpu) else {
                continue;
            };
            let Some(hertz) = events[0]
                .attr(&format!("cpu{cpu}"))
                .and_then(|v| v.as_f64())
            else {
                continue;
            };
            assert!(
                hertz >= min * 0.9 && hertz <= max * 1.15,
                "cpu{cpu} reported {hertz} Hz, outside the declared {min}..{max}"
            );
        }
    }
}
