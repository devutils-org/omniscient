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
//! - `cpuinfo_avg_freq`, where the driver provides it, is the processor's own
//!   APERF/MPERF ratio over the **last scheduler tick** before the read -- a
//!   few milliseconds, not the poll interval. That is the effective frequency
//!   while the core was running. On a cpu with no tick in the last 20 ms (an
//!   idle one, under NOHZ) the kernel has no ratio and falls back to the
//!   driver's current setpoint, which is not a frequency the core ran at; the
//!   event says `measures=setpoint` when the number it read is exactly one of
//!   the setpoints the driver declares.
//! - `cpuinfo_cur_freq` is read back from the hardware at the instant of the
//!   read.
//! - `scaling_cur_freq` is what the kernel believes the frequency is. With
//!   some drivers that is a hardware reading and with others it is the last
//!   setpoint, so the driver's name travels with the number.
//!
//! Each event covers the interval since the previous read: the read at window
//! open is a baseline and produces no event, because it describes the machine
//! before anything launched beside the run. Beside the frequency it carries
//! the interval's `busy` and `idle` shares of that cpu from `/proc/stat` and
//! `avg_hz`, busy times the frequency -- turbostat's `Busy%` and `Avg_MHz`,
//! estimated. The `pmu` backend measures the same three exactly.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, AttrValue, CollectorId, Event, KeyValue};
use crate::request::{Mode, Target};

use super::sysfs;

const CPU_ROOT: &str = "/sys/devices/system/cpu";
const PROC_STAT: &str = "/proc/stat";

/// The files a driver declares a setpoint in. `cpuinfo_avg_freq` on an idle
/// cpu falls back to the policy's current setpoint, which is one of these.
const SETPOINTS: [&str; 7] = [
    "scaling_min_freq",
    "scaling_max_freq",
    "cpuinfo_min_freq",
    "cpuinfo_max_freq",
    "amd_pstate_lowest_nonlinear_freq",
    "amd_pstate_max_freq",
    "base_frequency",
];

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
            Source::Average => "cpuinfo_avg_freq: the effective frequency, the processor's \
                                APERF/MPERF over the last scheduler tick before the read (a few \
                                milliseconds, not the poll interval); busy, idle and avg_hz are \
                                the interval's, from /proc/stat"
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
    /// Where each cpu's time is accounted, `/proc/stat` outside the tests.
    stat: PathBuf,
    /// The previous read: each event covers the interval since it.
    previous: Option<(u64, BTreeMap<u32, sysfs::CpuTime>)>,
    /// Per cpu, the setpoints its driver declares, in kHz, by file.
    setpoints: BTreeMap<u32, Vec<(&'static str, u64)>>,
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
            stat: PathBuf::from(PROC_STAT),
            previous: None,
            setpoints: BTreeMap::new(),
            losses: Losses::default(),
        }
    }

    /// Reads cpu time from `path` instead of `/proc/stat`.
    #[cfg(test)]
    fn with_stat(mut self, path: PathBuf) -> Self {
        self.stat = path;
        self
    }

    /// The declared setpoint `kilohertz` is exactly equal to, if any: a ratio
    /// of two counters times the base clock lands on one to the kilohertz
    /// essentially never, so equality is the kernel's fallback showing.
    fn setpoint(&self, cpu: u32, kilohertz: u64) -> Option<&'static str> {
        self.setpoints
            .get(&cpu)?
            .iter()
            .find(|(_, value)| *value == kilohertz)
            .map(|(file, _)| *file)
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
        if self.source == Some(Source::Average) {
            for &cpu in &self.cpus {
                let directory = self.cpufreq(cpu);
                let declared = SETPOINTS
                    .into_iter()
                    .filter_map(|file| Some((file, sysfs::number(&directory.join(file)).ok()?)))
                    .collect();
                self.setpoints.insert(cpu, declared);
            }
        }
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
        let mut per_cpu: Vec<(u32, u64)> = Vec::new();
        for &cpu in &self.cpus {
            match sysfs::number(&self.cpufreq(cpu).join(source.file())) {
                Ok(kilohertz) => per_cpu.push((cpu, kilohertz)),
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
        let times = match sysfs::cpu_times(&self.stat) {
            Ok(times) => times,
            Err(error) => {
                self.losses.failed_reads += 1;
                self.losses
                    .note(format!("the busy and idle shares are missing: {error}"));
                BTreeMap::new()
            }
        };

        // The read at window open describes the machine before the run: before
        // a program launched beside it started, before the load it was asked
        // about. It is the baseline the first interval is measured from, and
        // nothing else.
        let Some((before_ns, before)) = self.previous.replace((ts_ns, times.clone())) else {
            return Ok(Vec::new());
        };
        let interval_ns = ts_ns.saturating_sub(before_ns);

        let float = |name: &str, value: f64| Attr {
            name: name.to_owned(),
            value: AttrValue::Float(value),
        };
        let basis = source.basis(&self.driver);

        // One event per cpu, keyed on it. The mean of twenty-four cores
        // describes none of them -- one boosted core among twenty-three idle
        // ones reads as a machine at a middling clock -- and a reader asking
        // which core is hot cannot get there from a single number. The mean is
        // still available, because averaging a series is something the emit
        // layer does and splitting one apart is not.
        Ok(per_cpu
            .iter()
            .enumerate()
            .map(|(offset, &(cpu, kilohertz))| {
                // cpufreq counts in kilohertz.
                let hertz = kilohertz as f64 * 1_000.0;
                let fallback = match source {
                    Source::Average => self.setpoint(cpu, kilohertz),
                    _ => None,
                };
                let (measures, basis) = match fallback {
                    Some(file) => (
                        "setpoint",
                        format!(
                            "cpuinfo_avg_freq fell back to the driver's setpoint: it holds \
                             {kilohertz} kHz, exactly {file}, which is what the kernel returns \
                             when this cpu had no scheduler tick in the last 20 ms -- it was \
                             idle -- so this is not a frequency the core ran at"
                        ),
                    ),
                    None if source.is_effective() => ("effective", basis.clone()),
                    None => ("requested", basis.clone()),
                };
                let mut attrs = vec![
                    float("hz", hertz),
                    Attr::uint("cpu", u64::from(cpu)),
                    Attr::text("driver", self.driver.clone()),
                    Attr::text("basis", basis),
                    Attr::text("measures", measures),
                    Attr::uint("interval_ns", interval_ns),
                ];
                let shares = times
                    .get(&cpu)
                    .zip(before.get(&cpu))
                    .and_then(|(after, before)| after.shares_since(*before));
                if let Some((busy, idle)) = shares {
                    attrs.push(float("busy", busy));
                    attrs.push(float("idle", idle));
                    attrs.push(float("avg_hz", busy * hertz));
                }
                Event {
                    ts_ns,
                    ts_err_ns: 0,
                    kind: "power.freq".to_owned(),
                    keys: vec![KeyValue::cpu(cpu)],
                    attrs,
                    collector_id: self.id.clone(),
                    seq: seq + offset as u64,
                }
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
        assert!(collector.sample(0).expect("the baseline").is_empty());
        let events = collector.sample(1).expect("a read");

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
        assert!(collector.sample(0).expect("the baseline").is_empty());
        let events = collector.sample(1).expect("a read");

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
        assert!(collector.sample(0).expect("the baseline").is_empty());
        let events = collector.sample(1).expect("a read");

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
        assert!(collector.sample(0).expect("the baseline").is_empty());
        let events = collector.sample(1).expect("a read");

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
        assert!(collector.sample(0).expect("the baseline").is_empty());

        std::fs::remove_file(root.join("cpu1/cpufreq/cpuinfo_avg_freq")).expect("cpu1 goes away");
        let events = collector.sample(1).expect("cpu0 still answers");

        assert_eq!(events.len(), 1, "only cpu0 still answers");
        assert_eq!(events[0].keys[0].to_string(), "cpu:0");
        let losses = collector.losses();
        assert_eq!(losses.failed_reads, 1);
        assert!(!losses.is_clean());
        assert!(losses.reasons.keys().any(|reason| reason.contains("cpu1")));

        // And when the last one goes, the run ends rather than reporting a
        // frequency nobody read.
        std::fs::remove_file(root.join("cpu0/cpufreq/cpuinfo_avg_freq")).expect("cpu0 goes away");
        match collector.sample(2) {
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

    /// The read at window open is taken before anything launched beside the
    /// run has started, so it is a baseline and not a reading: the first event
    /// is the first interval's, and it carries that interval's busy and idle
    /// shares from the kernel's own cpu-time accounting.
    #[test]
    fn the_read_at_window_open_is_a_baseline_and_each_event_carries_its_intervals_shares() {
        let root = cpu_tree(
            "freq-shares",
            "amd-pstate-epp",
            &[("cpuinfo_avg_freq", "4000000")],
        );
        scratch::write(
            &root,
            "stat",
            "cpu0 0 0 0 100 0 0 0 0 0 0\ncpu1 0 0 0 100 0 0 0 0 0 0",
        );
        let mut collector =
            Frequencies::new(poll(), root.clone(), vec![0, 1]).with_stat(root.join("stat"));
        collector.start().expect("a cpufreq directory");
        assert!(
            collector.sample(0).expect("the opening read").is_empty(),
            "the opening read describes the machine before the run"
        );

        // cpu0 busy for 75 of 100 jiffies, cpu1 for none.
        scratch::write(
            &root,
            "stat",
            "cpu0 50 0 25 125 0 0 0 0 0 0\ncpu1 0 0 0 150 50 0 0 0 0 0",
        );
        let events = collector.sample(1).expect("a read");
        assert_eq!(events.len(), 2);
        let get = |index: usize, name: &str| events[index].attr(name).unwrap().as_f64().unwrap();
        assert!((get(0, "busy") - 0.75).abs() < 1e-12);
        assert!((get(0, "idle") - 0.25).abs() < 1e-12);
        assert!((get(0, "avg_hz") - 3e9).abs() < 1.0);
        assert_eq!(get(1, "busy"), 0.0);
        assert_eq!(get(1, "idle"), 1.0, "iowait is idle time");
        assert!(get(0, "interval_ns") > 0.0);
        std::fs::remove_dir_all(&root).ok();
    }

    /// What `cpuinfo_avg_freq` holds on an idle cpu, measured here: exactly
    /// `scaling_min_freq`, 1745645 kHz, on every idle core, while turbostat
    /// says the same cores ran at 4 GHz when they ran. The event has to say
    /// it read a setpoint rather than present it as the core's clock.
    #[test]
    fn an_idle_cpus_fallback_to_a_setpoint_is_named_as_one() {
        let root = cpu_tree(
            "freq-idle",
            "amd-pstate-epp",
            &[
                ("scaling_min_freq", "1745645"),
                ("scaling_max_freq", "4673823"),
                ("cpuinfo_avg_freq", "1745645"),
            ],
        );
        scratch::write(&root, "cpu1/cpufreq/cpuinfo_avg_freq", "4198535");
        let mut collector = Frequencies::new(poll(), root.clone(), vec![0, 1]);
        collector.start().expect("a cpufreq directory");
        collector.sample(0).expect("the baseline");
        let events = collector.sample(1).expect("a read");

        assert_eq!(events[0].attr("measures").unwrap().to_string(), "setpoint");
        let basis = events[0].attr("basis").unwrap().to_string();
        assert!(
            basis.contains("scaling_min_freq") && basis.contains("idle"),
            "{basis}"
        );
        assert_eq!(events[1].attr("measures").unwrap().to_string(), "effective");
        std::fs::remove_dir_all(&root).ok();
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
        assert!(collector.sample(0).expect("the baseline").is_empty());
        let events = collector.sample(1).expect("a read");
        assert_eq!(events.len(), cpus.len(), "one event per online cpu");
        for event in &events {
            let cpu = event.attr("cpu").and_then(|v| v.as_f64()).expect("a cpu") as u32;
            let Some((min, max)) = declared_range(&root, cpu) else {
                continue;
            };
            let hertz = event
                .attr("hz")
                .and_then(|v| v.as_f64())
                .expect("a frequency");
            assert!(
                hertz >= min * 0.9 && hertz <= max * 1.15,
                "cpu{cpu} reported {hertz} Hz, outside the declared {min}..{max}"
            );
        }
    }
}
