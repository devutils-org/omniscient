//! `power.energy` through the `power` performance unit.
//!
//! The same RAPL counters the sysfs backend reads, reached the other way. It
//! matters because the two are not equally available: `/sys/class/powercap/.../
//! energy_uj` is root-only on a normal machine, while the kernel will open the
//! same domain as a perf event for an ordinary user wherever
//! `perf_event_paranoid` allows it. On the development machine that is the
//! difference between a source that works and one that asks for root.
//!
//! What a tick is worth is read, not known: RAPL counts in units of 2^-32
//! joules and publishes that in a `.scale` file beside the event. A collector
//! that assumed microjoules would be wrong by ten orders of magnitude.

use std::collections::BTreeMap;

use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event};
use crate::perf::{Counter, CounterGroup, Scope};
use crate::request::{Mode, Target};

/// The unit that publishes the RAPL domains.
const PMU: &str = "power";

/// The domains worth asking for, in the order they are most useful. Whichever
/// the machine publishes are opened; the rest are not an error, because which
/// exist depends on the vendor and the part.
const DOMAINS: [(&str, &str); 4] = [
    ("energy-pkg", "package"),
    ("energy-cores", "cores"),
    ("energy-ram", "dram"),
    ("energy-gpu", "integrated-gpu"),
];

pub struct Prepare {
    mode: Mode,
}

impl Prepare {
    pub fn new(mode: Mode) -> Result<Self, String> {
        match mode {
            Mode::Poll { .. } | Mode::Count => Ok(Self { mode }),
            other => Err(format!(
                "energy is a counter that has to be differenced over a window: `{}` has none; \
                 poll it, or count it over a duration",
                other.kind()
            )),
        }
    }
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        match target {
            Target::System => Ok(Box::new(EnergyPmu {
                id: CollectorId::new("power.energy/pmu"),
                mode: self.mode,
                packages: package_leaders(),
                opened: Vec::new(),
                previous: BTreeMap::new(),
                baseline: BTreeMap::new(),
                losses: Losses::default(),
            })),
            other => Err(format!(
                "RAPL counts the energy of a whole package, so a {} target has no number here. \
                 Attributing a share of it to one process means multiplying by that process's \
                 share of cycles, which is an estimate this tool does not present as a \
                 measurement",
                other.kind()
            )),
        }
    }
}

/// The first cpu of each physical package.
///
/// A package counter is opened against one cpu of the package it belongs to, so
/// a two-socket machine needs two of them. Falling back to cpu 0 alone is right
/// for the single-socket case and wrong only in that it would under-report a
/// machine whose topology could not be read -- which is why the fallback is
/// noted on the events rather than assumed away.
fn package_leaders() -> Vec<(u32, u32)> {
    let mut leaders: BTreeMap<u32, u32> = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir("/sys/devices/system/cpu") else {
        return vec![(0, 0)];
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(cpu) = name
            .to_str()
            .and_then(|name| name.strip_prefix("cpu"))
            .and_then(|digits| digits.parse::<u32>().ok())
        else {
            continue;
        };
        let topology = entry.path().join("topology/physical_package_id");
        let Ok(package) = std::fs::read_to_string(topology) else {
            continue;
        };
        let Ok(package) = package.trim().parse::<u32>() else {
            continue;
        };
        leaders
            .entry(package)
            .and_modify(|first| *first = (*first).min(cpu))
            .or_insert(cpu);
    }
    if leaders.is_empty() {
        return vec![(0, 0)];
    }
    leaders.into_iter().collect()
}

struct Opened {
    package: u32,
    domain: &'static str,
    label: &'static str,
    group: CounterGroup,
    /// What one tick of this counter is worth, and in what unit.
    scale: f64,
    unit: String,
}

/// One reading of one domain's counter.
#[derive(Debug, Clone, Copy)]
struct Reading {
    ticks: u64,
    ts_ns: u64,
}

pub struct EnergyPmu {
    id: CollectorId,
    mode: Mode,
    packages: Vec<(u32, u32)>,
    opened: Vec<Opened>,
    /// Last raw reading per (package, domain), for the difference.
    previous: BTreeMap<(u32, &'static str), Reading>,
    /// The opening reading of each domain, kept only in `count` mode: a counted
    /// window is one difference, taken from here to the closing read.
    baseline: BTreeMap<(u32, &'static str), Reading>,
    losses: Losses,
}

impl EnergyPmu {
    /// Reads every open domain once, timestamping the round rather than each
    /// counter: a difference is only as good as the window it is divided by,
    /// and four counters read in a microsecond share one instant.
    fn read_all(&mut self) -> BTreeMap<(u32, &'static str), Reading> {
        let ts_ns = monotonic_ns();
        let mut readings = BTreeMap::new();
        for opened in &self.opened {
            let Ok(values) = opened.group.read() else {
                self.losses.failed_reads += 1;
                self.losses.note(format!(
                    "the `{}` energy counter of package {} could not be read",
                    opened.domain, opened.package
                ));
                continue;
            };
            let Some(value) = values.first() else {
                continue;
            };
            readings.insert(
                (opened.package, opened.domain),
                Reading {
                    ticks: value.value,
                    ts_ns,
                },
            );
        }
        readings
    }

    /// The energy between two readings of one domain, or nothing and a loss.
    fn delta_event(
        id: &CollectorId,
        losses: &mut Losses,
        opened: &Opened,
        before: Reading,
        after: Reading,
        seq: u64,
    ) -> Option<Event> {
        // The counter wraps; the kernel hands back a 64-bit accumulation, so a
        // decrease means the event was re-opened rather than that energy was
        // returned.
        if after.ticks < before.ticks {
            losses.failed_reads += 1;
            losses.note(format!(
                "the `{}` counter of package {} went backwards, so that interval is not measured",
                opened.domain, opened.package
            ));
            return None;
        }
        let ticks = after.ticks - before.ticks;
        // The window each difference covers, so that a reader can divide by it
        // without assuming the tick was the one requested. The sysfs backend
        // has always carried it; the two agree now.
        let interval_ns = after.ts_ns.saturating_sub(before.ts_ns);
        Some(Event {
            ts_ns: after.ts_ns,
            ts_err_ns: 0,
            kind: "power.energy".to_owned(),
            keys: Vec::new(),
            attrs: vec![
                Attr::float("joules", ticks as f64 * opened.scale),
                Attr::text("domain", opened.label),
                Attr::uint("package", opened.package as u64),
                Attr::uint("ticks", ticks),
                Attr::uint("interval_ns", interval_ns),
                Attr::text("unit", opened.unit.clone()),
            ],
            collector_id: id.clone(),
            seq,
        })
    }
}

impl Collector for EnergyPmu {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "joules"
    }

    fn start(&mut self) -> Result<(), String> {
        for (package, leader) in self.packages.clone() {
            for (domain, label) in DOMAINS {
                let Some(counter) = Counter::from_sysfs(label, PMU, domain) else {
                    continue;
                };
                let Ok(group) = CounterGroup::open(&[counter], Scope::Cpu(leader)) else {
                    // A domain this part does not implement is not an error;
                    // which ones exist depends on the vendor.
                    continue;
                };
                if group.enable().is_err() {
                    continue;
                }
                let (scale, unit) = crate::perf::sysfs_scale(PMU, domain)
                    .unwrap_or_else(|| (1.0, "count".to_owned()));
                self.opened.push(Opened {
                    package,
                    domain,
                    label,
                    group,
                    scale,
                    unit,
                });
            }
        }

        if self.opened.is_empty() {
            return Err(format!(
                "this machine publishes no RAPL domain through the `{PMU}` performance unit, or \
                 refuses to open one; check /proc/sys/kernel/perf_event_paranoid"
            ));
        }

        // A counted window is one difference over the whole run, and it starts
        // here -- the same convention `EnergySysfs` follows, so that `-m count`
        // means the same thing through either road of one chain.
        if self.mode == Mode::Count {
            self.baseline = self.read_all();
        }
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        // In `count` mode there is no series: the run's answer is the one
        // difference `stop` takes. Emitting a per-interval delta here is what
        // turned a counted three-second window into four events, of which the
        // last covered microseconds and read as zero energy.
        if self.mode == Mode::Count {
            return Ok(Vec::new());
        }

        let readings = self.read_all();
        let mut events = Vec::new();
        let mut index = seq;
        for opened in &self.opened {
            let key = (opened.package, opened.domain);
            let Some(reading) = readings.get(&key).copied() else {
                continue;
            };
            let Some(previous) = self.previous.insert(key, reading) else {
                // A counter's first reading is a baseline, not a measurement.
                continue;
            };
            if let Some(event) =
                Self::delta_event(&self.id, &mut self.losses, opened, previous, reading, index)
            {
                events.push(event);
                index += 1;
            }
        }
        Ok(events)
    }

    fn stop(&mut self) -> Vec<Event> {
        // A counted window's measurement *is* the closing read, so it is taken
        // here; a polled series already has its last interval from the tick
        // before the stop, and a further read there would cover microseconds
        // and land in the series as a zero.
        if self.mode != Mode::Count {
            return Vec::new();
        }
        let closing = self.read_all();
        let mut events = Vec::new();
        let mut index = u64::MAX / 2;
        for opened in &self.opened {
            let key = (opened.package, opened.domain);
            let (Some(baseline), Some(reading)) =
                (self.baseline.get(&key).copied(), closing.get(&key).copied())
            else {
                continue;
            };
            if let Some(event) =
                Self::delta_event(&self.id, &mut self.losses, opened, baseline, reading, index)
            {
                events.push(event);
                index += 1;
            }
        }
        events
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collector() -> Option<Box<dyn Collector>> {
        let prepare = Box::new(
            Prepare::new(Mode::Poll {
                interval: Mode::DEFAULT_POLL_INTERVAL,
            })
            .ok()?,
        );
        let mut collector = prepare.attach(&Target::System).ok()?;
        match collector.start() {
            Ok(()) => Some(collector),
            Err(reason) => {
                eprintln!("skipped: {reason}");
                None
            }
        }
    }

    #[test]
    fn a_package_reports_a_plausible_power_draw() {
        let Some(mut collector) = collector() else {
            return;
        };
        // The first read is a baseline.
        assert!(collector.sample(0).unwrap().is_empty());

        let window = std::time::Duration::from_millis(300);
        std::thread::sleep(window);
        let events = collector.sample(0).unwrap();
        assert!(
            !events.is_empty(),
            "a domain was opened and produced nothing"
        );

        let joules = events[0].attr("joules").unwrap().as_f64().unwrap();
        let watts = joules / window.as_secs_f64();
        // An idle desktop package draws tens of watts; nothing draws a
        // microwatt or ten kilowatts, and either would mean the scale was not
        // applied.
        assert!(
            (0.1..2000.0).contains(&watts),
            "a package drawing {watts} W means the scale was read wrong"
        );
    }

    #[test]
    fn the_scale_comes_from_the_kernel_and_not_from_a_constant() {
        let Some((scale, unit)) = crate::perf::sysfs_scale(PMU, "energy-pkg") else {
            eprintln!("skipped: this machine publishes no RAPL energy-pkg event");
            return;
        };
        assert_eq!(unit, "Joules");
        // 2^-32 J is what RAPL counts in on every part that implements it; a
        // collector that hardcoded microjoules would be out by ten orders of
        // magnitude.
        assert!(
            (scale - 2.0f64.powi(-32)).abs() < 1e-18,
            "the kernel says one tick is {scale} J"
        );
    }

    #[test]
    fn a_process_target_is_refused_rather_than_estimated() {
        let prepare = Box::new(Prepare::new(Mode::Count).unwrap());
        let Err(reason) = prepare.attach(&Target::Pid(vec![1])) else {
            panic!("there is no per-process energy in the hardware");
        };
        assert!(reason.contains("share of cycles"), "{reason}");
    }
}
