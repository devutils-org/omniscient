//! A source's counts read from a kernel counter file, for a machine where no
//! program can be loaded.
//!
//! The kernel keeps counters of much of what the eBPF sources trace one event
//! at a time: pages allocated (`/proc/vmstat`), segments retransmitted
//! (`/proc/net/snmp`), time spent in interrupt handlers (`/proc/stat`). Read
//! around a window they give the window's total, which is the `count` mode of
//! those sources with what is lost said: no distribution, no task, the
//! machine's whole. A container, a locked-down kernel and a kernel before
//! `CAP_BPF` all still have these files.
//!
//! A backend of this kind is a [`Spec`]: which file, how it is laid out, which
//! counters make up which attribute, and which attribute is the value. The
//! baseline, the window and the counters that went backwards are
//! `collect::poll`'s.

use std::collections::BTreeMap;

use crate::clock::monotonic_ns;
use crate::collect::poll::{CounterPoll, Interval, Reading};
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::request::{Mode, Target};

/// How a counter file is laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// `name value` per line: `/proc/vmstat`.
    Flat,
    /// A line of names and a line of values under one section prefix:
    /// `/proc/net/snmp`, `/proc/net/netstat`. A counter is `Section:Name`.
    Paired,
    /// `/proc/stat`'s per-cpu lines, `cpuN user nice system idle iowait irq
    /// softirq ...`, in clock ticks; a counter is a column's name, read per
    /// cpu and kept per cpu.
    CpuTicks,
}

/// One backend: a source's counts out of one file.
#[derive(Debug, Clone, Copy)]
pub struct Spec {
    pub source: &'static str,
    pub path: &'static str,
    pub layout: Layout,
    /// Each attribute and the file's counters summed into it.
    pub counters: &'static [(&'static str, &'static [&'static str])],
    /// The attribute that is the measurement.
    pub value: &'static str,
    /// What is measured, for a refusal: "pages allocated".
    pub what: &'static str,
}

/// The `/proc/stat` columns after `cpuN`, in order.
const CPU_COLUMNS: [&str; 10] = [
    "user",
    "nice",
    "system",
    "idle",
    "iowait",
    "irq",
    "softirq",
    "steal",
    "guest",
    "guest_nice",
];

impl Spec {
    /// Whether the spec reads this counter of the file: what it sums into an
    /// attribute. Every other line -- a gauge that goes down, a counter of
    /// something else -- is no business of this source.
    fn reads(&self, name: &str) -> bool {
        self.counters
            .iter()
            .any(|(_, summed)| summed.contains(&name))
    }

    /// Every counter the file holds, by name (and by cpu, for per-cpu files).
    fn read(&self) -> Result<Reading<(Option<u32>, String), u64>, SampleError> {
        let text = std::fs::read_to_string(self.path)
            .map_err(|error| SampleError::Transient(format!("{}: {error}", self.path)))?;
        let mut counters = BTreeMap::new();
        match self.layout {
            Layout::Flat => {
                for line in text.lines() {
                    let mut fields = line.split_whitespace();
                    if let (Some(name), Some(Ok(value))) =
                        (fields.next(), fields.next().map(str::parse::<u64>))
                    {
                        counters.insert((None, name.to_owned()), value);
                    }
                }
            }
            Layout::Paired => {
                for (name, value) in crate::collect::net::procfs::named_counters(self.path)? {
                    counters.insert((None, name), value);
                }
            }
            Layout::CpuTicks => {
                // USER_HZ, which `/proc/stat` counts in on every architecture.
                let tick_ns = 1_000_000_000 / crate::collect::overhead::ticks_per_second();
                for line in text.lines() {
                    let mut fields = line.split_whitespace();
                    let Some(cpu) = fields
                        .next()
                        .and_then(|name| name.strip_prefix("cpu"))
                        .and_then(|number| number.parse::<u32>().ok())
                    else {
                        continue;
                    };
                    for (column, value) in CPU_COLUMNS.iter().zip(fields) {
                        if let Ok(ticks) = value.parse::<u64>() {
                            counters.insert((Some(cpu), (*column).to_owned()), ticks * tick_ns);
                        }
                    }
                }
            }
        }
        Ok(Reading {
            ts_ns: monotonic_ns(),
            counters,
        })
    }

    /// The attributes of one interval, per cpu for a per-cpu file and once
    /// for the machine otherwise.
    fn events(&self, interval: &Interval<(Option<u32>, String), u64>, seq: u64) -> Vec<Event> {
        let mut by_cpu: BTreeMap<Option<u32>, BTreeMap<&str, u64>> = BTreeMap::new();
        for ((cpu, name), increase) in &interval.moved {
            for (attr, summed) in self.counters {
                if summed.contains(&name.as_str()) {
                    *by_cpu.entry(*cpu).or_default().entry(attr).or_insert(0) += increase;
                }
            }
        }
        by_cpu
            .into_iter()
            .filter(|(_, attrs)| attrs.contains_key(self.value))
            .enumerate()
            .map(|(offset, (cpu, attrs))| {
                let mut attributes: Vec<Attr> = attrs
                    .into_iter()
                    .map(|(name, value)| Attr::uint(name, value))
                    .collect();
                attributes.push(Attr::uint("interval_ns", interval.interval_ns));
                let mut keys = Vec::new();
                if let Some(cpu) = cpu {
                    attributes.push(Attr::uint("cpu", u64::from(cpu)));
                    keys.push(KeyValue::cpu(cpu));
                }
                Event {
                    ts_ns: interval.ts_ns,
                    ts_err_ns: 0,
                    kind: self.source.to_owned(),
                    keys,
                    attrs: attributes,
                    collector_id: CollectorId::unstamped(),
                    seq: seq + offset as u64,
                }
            })
            .collect()
    }
}

pub struct Prepare {
    spec: Spec,
    mode: Mode,
}

impl Prepare {
    pub fn new(spec: Spec, mode: Mode) -> Result<Self, String> {
        match mode {
            Mode::Count | Mode::Poll { .. } => Ok(Self { spec, mode }),
            other => Err(format!(
                "{} is a counter of {}: `{other}` would be one event per occurrence, which the \
                 file does not keep; count it over the window",
                spec.path, spec.what
            )),
        }
    }
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        match target {
            Target::System => Ok(Box::new(CounterFile {
                spec: self.spec,
                poll: CounterPoll::new(self.mode),
                losses: Losses::default(),
            })),
            other => Err(format!(
                "{} counts {} for the whole machine, and a {} target asks for a share of it the \
                 kernel does not keep; use --system",
                self.spec.path,
                self.spec.what,
                other.kind()
            )),
        }
    }
}

pub struct CounterFile {
    spec: Spec,
    poll: CounterPoll<(Option<u32>, String), u64>,
    losses: Losses,
}

impl CounterFile {
    fn events(&mut self, interval: Interval<(Option<u32>, String), u64>, seq: u64) -> Vec<Event> {
        // A counter the spec reads that went backwards was replaced, and one
        // that appeared has nothing to be measured against: either leaves a
        // sum short, so that cpu's (or the machine's) interval is not
        // reported, and the loss says which counter.
        let mut short = std::collections::BTreeSet::new();
        for ((cpu, name), went) in interval
            .went_back
            .iter()
            .map(|key| (key, "went backwards, so what it counts was replaced"))
            .chain(interval.appeared.iter().map(|key| {
                (
                    key,
                    "appeared during the run, so it has nothing to be measured against",
                )
            }))
        {
            if !self.spec.reads(name) {
                continue;
            }
            short.insert(*cpu);
            self.losses.undecoded += 1;
            self.losses.note_times(
                format!(
                    "`{name}`{} in {} {went}; that interval is not reported",
                    cpu.map(|cpu| format!(" of cpu{cpu}")).unwrap_or_default(),
                    self.spec.path
                ),
                1,
            );
        }
        let mut events = self.spec.events(&interval, seq);
        if !short.is_empty() {
            events.retain(|event| {
                let cpu = event
                    .attr("cpu")
                    .and_then(|cpu| cpu.as_f64())
                    .map(|cpu| cpu as u32);
                !short.contains(&cpu)
            });
        }
        events
    }
}

impl Collector for CounterFile {
    fn value_attr(&self) -> &'static str {
        self.spec.value
    }

    fn start(&mut self) -> Result<(), String> {
        let spec = self.spec;
        self.poll
            .start(|| spec.read())
            .map_err(|error| format!("{error:?}"))
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let spec = self.spec;
        Ok(self
            .poll
            .tick(|| spec.read())?
            .map(|interval| self.events(interval, seq))
            .unwrap_or_default())
    }

    fn stop(&mut self) -> Vec<Event> {
        let spec = self.spec;
        match self.poll.stop(|| spec.read()) {
            Ok(Some(total)) => self.events(total, 0),
            Ok(None) => Vec::new(),
            Err(error) => {
                self.losses.failed_reads += 1;
                self.losses.note_times(
                    format!("the closing read failed, so the window measured nothing: {error:?}"),
                    1,
                );
                Vec::new()
            }
        }
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interval(moved: Vec<((Option<u32>, &str), u64)>) -> Interval<(Option<u32>, String), u64> {
        let moved: Vec<((Option<u32>, String), u64)> = moved
            .into_iter()
            .map(|((cpu, name), value)| ((cpu, name.to_owned()), value))
            .collect();
        Interval {
            ts_ns: 10,
            interval_ns: 5,
            moved,
            appeared: Vec::new(),
            went_back: Vec::new(),
            after: Reading {
                ts_ns: 10,
                counters: BTreeMap::new(),
            },
        }
    }

    const PAGES: Spec = Spec {
        source: "mem.page_alloc",
        path: "/proc/vmstat",
        layout: Layout::Flat,
        counters: &[
            ("pages", &["pgalloc_dma", "pgalloc_normal"]),
            ("freed_pages", &["pgfree"]),
        ],
        value: "pages",
        what: "pages allocated",
    };

    #[test]
    fn an_attribute_sums_its_counters_and_the_machine_is_one_event() {
        let events = PAGES.events(
            &interval(vec![
                ((None, "pgalloc_dma"), 2),
                ((None, "pgalloc_normal"), 40),
                ((None, "pgfree"), 30),
                ((None, "nr_dirty"), 9),
            ]),
            0,
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].attr("pages").unwrap().as_f64(), Some(42.0));
        assert_eq!(events[0].attr("freed_pages").unwrap().as_f64(), Some(30.0));
        assert!(events[0].keys.is_empty());
    }

    #[test]
    fn a_per_cpu_file_gives_one_event_per_cpu_keyed_on_it() {
        const IRQ: Spec = Spec {
            source: "sched.irq",
            path: "/proc/stat",
            layout: Layout::CpuTicks,
            counters: &[("handler_ns", &["irq", "softirq"])],
            value: "handler_ns",
            what: "time in interrupt handlers",
        };
        let events = IRQ.events(
            &interval(vec![
                ((Some(0), "irq"), 10),
                ((Some(0), "softirq"), 20),
                ((Some(1), "softirq"), 5),
                ((Some(1), "user"), 99),
            ]),
            0,
        );
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].attr("handler_ns").unwrap().as_f64(), Some(30.0));
        assert_eq!(events[1].attr("handler_ns").unwrap().as_f64(), Some(5.0));
    }

    #[test]
    fn a_counter_the_spec_does_not_read_is_no_loss_and_one_it_reads_voids_its_sum() {
        let mut file = CounterFile {
            spec: PAGES,
            poll: CounterPoll::new(Mode::Count),
            losses: Losses::default(),
        };
        let mut gauge_went_down = interval(vec![((None, "pgalloc_normal"), 40)]);
        gauge_went_down.went_back = vec![(None, "nr_free_pages".to_owned())];
        let events = file.events(gauge_went_down, 0);
        assert_eq!(events.len(), 1);
        assert!(file.losses.is_clean(), "{:?}", file.losses);

        let mut zone_replaced = interval(vec![((None, "pgalloc_dma"), 2)]);
        zone_replaced.went_back = vec![(None, "pgalloc_normal".to_owned())];
        assert!(file.events(zone_replaced, 0).is_empty());
        assert_eq!(file.losses.undecoded, 1);
    }

    #[test]
    fn the_files_this_machine_has_read_as_their_layouts_say() {
        let vmstat = PAGES.read().expect("/proc/vmstat");
        assert!(vmstat.counters.contains_key(&(None, "pgfree".to_owned())));
        let snmp = Spec {
            path: "/proc/net/snmp",
            layout: Layout::Paired,
            ..PAGES
        }
        .read()
        .expect("/proc/net/snmp");
        assert!(snmp
            .counters
            .contains_key(&(None, "Tcp:RetransSegs".to_owned())));
        let stat = Spec {
            path: "/proc/stat",
            layout: Layout::CpuTicks,
            ..PAGES
        }
        .read()
        .expect("/proc/stat");
        assert!(stat.counters.contains_key(&(Some(0), "softirq".to_owned())));
    }
}
