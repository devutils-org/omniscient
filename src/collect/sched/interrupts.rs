//! `sched.interrupts` over `/proc/interrupts` and `/proc/softirqs`.
//!
//! How many interrupts each cpu took, per line and per softirq vector, as
//! `vmstat`'s `in`, `mpstat -I` and `/proc/interrupts` itself count them.
//! `sched.irq` says how long the handlers ran and costs a program on every
//! one; this says how many there were and costs two file reads a window,
//! which is the question a reader asks first.
//!
//! The files are cumulative since boot, so a window's count is the
//! difference of two reads. A row whose count did not move in a window is
//! not reported: a machine has hundreds of lines times its cpus, and most of
//! them are silent.

use std::collections::BTreeMap;

use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::request::{Mode, Target};

const HARD: &str = "/proc/interrupts";
const SOFT: &str = "/proc/softirqs";

/// One row of one file: which half, the label the file gives it (`16`,
/// `LOC`, `TIMER`), the handler's or the vector's name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Line {
    pub soft: bool,
    pub label: String,
    pub name: String,
}

/// Every row's count per cpu, as one read of both files gave them.
pub(crate) type Counts = BTreeMap<(Line, u32), u64>;

/// The cpus of a header line, in column order: `CPU0 CPU1 CPU3` on a machine
/// with an offline cpu names the columns the counts are in.
fn columns(header: &str) -> Vec<u32> {
    header
        .split_whitespace()
        .filter_map(|column| column.strip_prefix("CPU")?.parse().ok())
        .collect()
}

/// Parses one of the two files into `counts`.
///
/// A hard row is `label: counts... chip hwirq-type name`; a named row such as
/// `LOC: counts... Local timer interrupts` is named by its label, which is
/// how `mpstat -I` names it. A row with fewer counts than there are cpus
/// (`ERR:` and `MIS:` are one machine-wide number) is not per cpu and is left
/// out.
pub(crate) fn parse(text: &str, soft: bool, counts: &mut Counts) {
    let mut lines = text.lines();
    let Some(header) = lines.next() else {
        return;
    };
    let cpus = columns(header);
    for row in lines {
        let Some((label, rest)) = row.split_once(':') else {
            continue;
        };
        let label = label.trim().to_owned();
        let mut fields = rest.split_whitespace();
        let values: Vec<u64> = fields
            .by_ref()
            .take(cpus.len())
            .map_while(|field| field.parse().ok())
            .collect();
        if values.len() != cpus.len() {
            continue;
        }
        let tail: Vec<&str> = fields.collect();
        let numbered = label.parse::<u32>().is_ok();
        let name = if soft || !numbered {
            label.clone()
        } else {
            // The chip and the hardware irq with its trigger come first.
            tail.get(2..)
                .map(|words| words.join(" "))
                .unwrap_or_default()
        };
        let line = Line { soft, label, name };
        for (cpu, value) in cpus.iter().zip(values) {
            counts.insert((line.clone(), *cpu), value);
        }
    }
}

fn read() -> Result<Counts, SampleError> {
    let mut counts = Counts::new();
    for (path, soft) in [(HARD, false), (SOFT, true)] {
        let text = std::fs::read_to_string(path)
            .map_err(|error| SampleError::Transient(format!("{path}: {error}")))?;
        parse(&text, soft, &mut counts);
    }
    Ok(counts)
}

/// Holds the mode until the target exists.
pub struct Prepare(pub Mode);

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if !matches!(self.0, Mode::Poll { .. } | Mode::Count) {
            return Err(format!(
                "`{}` mode cannot count interrupts from `{HARD}`: the file is a counter, read \
                 at the edges of a window (`count`) or of each interval (`poll`)",
                self.0
            ));
        }
        let cpus = match target {
            Target::System => None,
            Target::Cpu(set) => Some(set.cpus().to_vec()),
            other => {
                return Err(format!(
                    "a {} target names a process, and an interrupt belongs to no process: it \
                     is taken by a cpu and charged to nobody. Use --system, or --cpu",
                    other.kind()
                ))
            }
        };
        Ok(Box::new(InterruptsCollector {
            counting: self.0 == Mode::Count,
            cpus,
            previous: None,
            losses: Losses::default(),
            wraps: 0,
        }))
    }
}

pub struct InterruptsCollector {
    counting: bool,
    cpus: Option<Vec<u32>>,
    /// The last read and when it was taken.
    previous: Option<(u64, Counts)>,
    losses: Losses,
    /// Per-cpu counters that wrapped past 2^32 in a window, and were counted
    /// across the wrap.
    wraps: u64,
}

/// How far a per-cpu counter moved from `before` to `now`, and whether that is
/// across a wrap.
///
/// The kernel keeps these counts in an `unsigned int` (`kstat_irqs_cpu`,
/// `kstat_softirqs_cpu`, the arch's `irq_stat`) and prints them as such, so a
/// count that went backwards from the upper half of that range wrapped: `LOC`
/// at 1 kHz does after about fifty days, a busy queue's line within hours.
/// Anything else that went backwards was reset (a line freed and taken again
/// under the same name), and counts from zero: what it counted before the
/// reset is not known.
fn moved(before: u64, now: u64) -> (u64, Moved) {
    const RANGE: u64 = 1 << 32;
    if now >= before {
        (now - before, Moved::Forward)
    } else if (RANGE / 2..RANGE).contains(&before) {
        (now + RANGE - before, Moved::Wrapped)
    } else {
        (now, Moved::Reset)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Moved {
    Forward,
    Wrapped,
    Reset,
}

impl InterruptsCollector {
    /// The events for what moved between the previous read and `now`.
    fn window(&mut self, now: Counts, ts_ns: u64, seq: u64) -> Vec<Event> {
        let Some((opened_ns, before)) = self.previous.replace((ts_ns, now.clone())) else {
            return Vec::new();
        };
        let window_ns = ts_ns.saturating_sub(opened_ns);
        let mut events = Vec::new();
        for ((line, cpu), value) in &now {
            if self.cpus.as_ref().is_some_and(|cpus| !cpus.contains(cpu)) {
                continue;
            }
            // A line that appeared in the window (a device bound) counts
            // from zero.
            let (moved, how) = moved(
                before.get(&(line.clone(), *cpu)).copied().unwrap_or(0),
                *value,
            );
            match how {
                Moved::Forward => {}
                Moved::Wrapped => self.wraps += 1,
                Moved::Reset => {
                    self.losses.failed_reads += 1;
                    self.losses.note_times(
                        "a per-cpu interrupt counter went backwards without wrapping, the \
                         line reset under the same name: its window counts from the reset, \
                         and what it counted before is lost",
                        1,
                    );
                }
            }
            if moved == 0 {
                continue;
            }
            let mut attrs = vec![
                Attr::uint("count", moved),
                Attr::uint("window_ns", window_ns),
                Attr::uint("cpu", u64::from(*cpu)),
                Attr::text("kind", if line.soft { "soft" } else { "hard" }),
                Attr::text("line", line.label.clone()),
                Attr::text("name", line.name.clone()),
            ];
            if let Ok(irq) = line.label.parse::<u64>() {
                attrs.push(Attr::uint("irq", irq));
            }
            events.push(Event {
                ts_ns: opened_ns,
                ts_err_ns: 0,
                kind: "sched.interrupts".to_owned(),
                keys: vec![KeyValue::cpu(*cpu)],
                attrs,
                collector_id: CollectorId::unstamped(),
                seq: seq + events.len() as u64,
            });
        }
        events
    }
}

impl Collector for InterruptsCollector {
    fn value_attr(&self) -> &'static str {
        "count"
    }

    fn start(&mut self) -> Result<(), String> {
        let counts = read().map_err(|error| format!("{error:?}"))?;
        // A poll's first interval opens at its first read, which the runner
        // takes as the window opens: a baseline from here would make the
        // first event the few milliseconds between the two, the leading
        // event `docs/fixes/w22-sysstat.md` (defect 2) found on `cpu.util`.
        if self.counting {
            self.previous = Some((monotonic_ns(), counts));
        }
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        if self.counting {
            return Ok(Vec::new());
        }
        let counts = read()?;
        Ok(self.window(counts, monotonic_ns(), seq))
    }

    fn stop(&mut self) -> Vec<Event> {
        if !self.counting {
            return Vec::new();
        }
        match read() {
            Ok(counts) => self.window(counts, monotonic_ns(), 0),
            Err(error) => {
                self.losses.failed_reads += 1;
                self.losses.note_times(
                    format!(
                        "the closing read of the interrupt counters failed, so the window has no \
                     count: {error:?}"
                    ),
                    1,
                );
                Vec::new()
            }
        }
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    fn facts(&self) -> Vec<String> {
        let mut facts = vec![
            "a row is one interrupt line (`kind` hard, `line` its number or its \
             `/proc/interrupts` label such as `LOC`) or one softirq vector (`kind` soft) on \
             one cpu, counted between two reads of the files; a row that did not move is not \
             reported, and `ERR` and `MIS`, which are not per cpu, are left out"
                .to_owned(),
        ];
        if self.wraps > 0 {
            facts.push(format!(
                "{} per-cpu counters wrapped past 2^32 within a window, the width the kernel \
                 keeps them in, and were counted across the wrap",
                self.wraps
            ));
        }
        facts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HARD_TEXT: &str = "            CPU0       CPU1
   0:         35          0  IR-IO-APIC    2-edge      timer
  68:         34          2  IR-PCI-MSIX-0000:01:00.0    0-edge      nvme0q0
 LOC:     482318     452034   Local timer interrupts
 ERR:          0
";
    const SOFT_TEXT: &str = "                    CPU0       CPU1
          HI:          0          2
       TIMER:      30639      10518
";

    #[test]
    fn both_files_parse_into_rows_per_cpu() {
        let mut counts = Counts::new();
        parse(HARD_TEXT, false, &mut counts);
        parse(SOFT_TEXT, true, &mut counts);
        let get = |soft: bool, label: &str, cpu: u32| {
            counts
                .iter()
                .find(|((line, at), _)| line.soft == soft && line.label == label && *at == cpu)
                .map(|((line, _), value)| (line.name.clone(), *value))
        };
        assert_eq!(get(false, "68", 1), Some(("nvme0q0".to_owned(), 2)));
        assert_eq!(get(false, "LOC", 0), Some(("LOC".to_owned(), 482_318)));
        assert_eq!(get(true, "TIMER", 1), Some(("TIMER".to_owned(), 10_518)));
        assert_eq!(get(false, "ERR", 0), None);
    }

    #[test]
    fn a_window_is_the_difference_and_silent_rows_are_left_out() {
        let mut before = Counts::new();
        parse(HARD_TEXT, false, &mut before);
        let mut collector = InterruptsCollector {
            counting: false,
            cpus: Some(vec![1]),
            previous: Some((1_000, before)),
            losses: Losses::default(),
            wraps: 0,
        };
        let later = HARD_TEXT.replace("482318     452034", "482318     452134");
        let mut after = Counts::new();
        parse(&later, false, &mut after);
        let events = collector.window(after, 3_000, 0);
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(
            events[0].attr("count").and_then(|v| v.as_f64()),
            Some(100.0)
        );
        assert_eq!(
            events[0].attr("window_ns").and_then(|v| v.as_f64()),
            Some(2_000.0)
        );
    }

    /// A counter that wrapped past 2^32 is counted across the wrap, and one
    /// that went backwards otherwise is counted from its reset, as a loss.
    #[test]
    fn a_wrap_is_counted_and_a_reset_is_a_loss() {
        assert_eq!(moved(10, 25), (15, Moved::Forward));
        assert_eq!(moved(u64::from(u32::MAX) - 4, 10), (15, Moved::Wrapped));
        assert_eq!(moved(1_000, 7), (7, Moved::Reset));
        let line = Line {
            soft: false,
            label: "LOC".to_owned(),
            name: "LOC".to_owned(),
        };
        let counts = |loc: u64, nvme: u64| {
            let mut counts = Counts::new();
            counts.insert((line.clone(), 0), loc);
            let nvme_line = Line {
                label: "68".to_owned(),
                name: "nvme0q0".to_owned(),
                ..line.clone()
            };
            counts.insert((nvme_line, 0), nvme);
            counts
        };
        let mut collector = InterruptsCollector {
            counting: false,
            cpus: None,
            previous: Some((0, counts(u64::from(u32::MAX) - 99, 5_000))),
            losses: Losses::default(),
            wraps: 0,
        };
        let events = collector.window(counts(100, 40), 1_000, 0);
        let count_of = |label: &str| {
            events
                .iter()
                .find(|event| event.attr("line").map(ToString::to_string).as_deref() == Some(label))
                .and_then(|event| event.attr("count").and_then(|v| v.as_f64()))
        };
        assert_eq!(count_of("LOC"), Some(200.0));
        assert_eq!(count_of("68"), Some(40.0));
        assert_eq!(collector.wraps, 1);
        assert_eq!(collector.losses().failed_reads, 1);
        assert!(collector
            .facts()
            .iter()
            .any(|fact| fact.contains("wrapped")));
    }
}
