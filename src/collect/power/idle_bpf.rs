//! `power.cstate` through `bpf:raw_tp`: every idle period, from the
//! `power:cpu_idle` announcements.
//!
//! cpuidle's counters say how long each state was occupied over an interval;
//! the tracepoint says when each period began and how long it lasted, which is
//! the transition timeline the counters cannot give. Each exit is paired with
//! the entry before it on the same cpu, and one period is one event. What
//! cannot be paired is counted, never guessed at: an exit whose entry came
//! before the window, and an entry with no exit by the end, are both at the
//! window's edges and said so; an entry that follows another with no exit
//! between them means an announcement was lost, which is a loss.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration as StdDuration;

use crate::bpf::{IdleEdge, IdleProgram, IDLE_EXIT};
use crate::collect::kernel::{KernelCounts, Lost};
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::request::{Mode, Target};

use super::sysfs;

/// A busy machine idles thousands of times a second across its cpus.
const DRAIN_INTERVAL: StdDuration = StdDuration::from_millis(50);

/// Room for a few hundred milliseconds of announcements at that rate.
const RING_BYTES: usize = 1024 * 1024;

const CPU_ROOT: &str = "/sys/devices/system/cpu";

pub struct Prepare {
    mode: Mode,
}

impl Prepare {
    pub fn new(mode: Mode) -> Result<Self, String> {
        match mode {
            Mode::Trace => Ok(Self { mode }),
            other => Err(format!(
                "the tracepoint gives each idle period as it ends; `{}` residency is cpuidle's \
                 totals, which the `fs:sys` backend reads",
                other.kind()
            )),
        }
    }
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let _ = self.mode;
        let cpus = match target {
            Target::System => None,
            Target::Cpu(set) => Some(set.cpus().to_vec()),
            other => {
                return Err(format!(
                    "an idle state belongs to a core, not to a {}; use --cpu or --system",
                    other.kind()
                ))
            }
        };
        Ok(Box::new(IdlePeriods::new(
            cpus,
            state_names(Path::new(CPU_ROOT)),
        )))
    }
}

/// Each cpu's state names, by index, from cpuidle's sysfs: what the
/// tracepoint numbers, cpuidle names (`C1`, `C2`, `POLL`).
fn state_names(root: &Path) -> BTreeMap<(u32, u32), String> {
    let mut names = BTreeMap::new();
    let Ok(cpus) = sysfs::online_cpus(root) else {
        return names;
    };
    for cpu in cpus {
        let directory = root.join(format!("cpu{cpu}")).join("cpuidle");
        for state in sysfs::entries(&directory).unwrap_or_default() {
            let file = state.file_name().unwrap_or_default().to_string_lossy();
            let Some(index) = file
                .strip_prefix("state")
                .and_then(|index| index.parse::<u32>().ok())
            else {
                continue;
            };
            if let Ok(name) = sysfs::text(&state.join("name")) {
                names.insert((cpu, index), name);
            }
        }
    }
    names
}

pub struct IdlePeriods {
    cpus: Option<Vec<u32>>,
    names: BTreeMap<(u32, u32), String>,
    program: Option<IdleProgram>,
    /// Each cpu's open period: the state it entered, and when.
    open: BTreeMap<u32, (u32, u64)>,
    losses: Losses,
    counts: KernelCounts,
    /// Exits whose entry came before the window opened.
    before_window: u64,
    /// Announcements for cpus outside a `--cpu` target.
    off_target: u64,
    periods: u64,
    idle_total_ns: u64,
}

impl IdlePeriods {
    fn new(cpus: Option<Vec<u32>>, names: BTreeMap<(u32, u32), String>) -> Self {
        Self {
            cpus,
            names,
            program: None,
            open: BTreeMap::new(),
            losses: Losses::default(),
            counts: KernelCounts::default(),
            before_window: 0,
            off_target: 0,
            periods: 0,
            idle_total_ns: 0,
        }
    }

    /// Pairs one announcement with the cpu's open period, returning the
    /// period it closes.
    fn edge(&mut self, edge: IdleEdge) -> Option<Event> {
        if self
            .cpus
            .as_ref()
            .is_some_and(|cpus| !cpus.contains(&edge.cpu))
        {
            self.off_target += 1;
            return None;
        }
        if edge.state != IDLE_EXIT {
            if self
                .open
                .insert(edge.cpu, (edge.state, edge.ts_ns))
                .is_some()
            {
                self.losses.dropped += 1;
                self.losses.note(
                    "a cpu announced an idle entry while a period was still open on it: the exit \
                     between them was lost, and so is that period",
                );
            }
            return None;
        }
        let Some((state, entered_ns)) = self.open.remove(&edge.cpu) else {
            self.before_window += 1;
            return None;
        };
        let idle_ns = edge.ts_ns.saturating_sub(entered_ns);
        self.periods += 1;
        self.idle_total_ns += idle_ns;
        let mut attrs = vec![
            Attr::uint("idle_ns", idle_ns),
            Attr::uint("state", u64::from(state)),
            Attr::uint("cpu", u64::from(edge.cpu)),
            Attr::uint("entered_ns", entered_ns),
        ];
        if let Some(name) = self.names.get(&(edge.cpu, state)) {
            attrs.push(Attr::text("state_name", name.clone()));
        }
        Some(Event {
            ts_ns: entered_ns,
            ts_err_ns: 0,
            kind: "power.cstate".to_owned(),
            keys: vec![KeyValue::cpu(edge.cpu)],
            attrs,
            collector_id: CollectorId::unstamped(),
            seq: 0,
        })
    }

    fn drain(&mut self, seq: u64) -> Vec<Event> {
        let Some(program) = self.program.as_mut() else {
            return Vec::new();
        };
        let mut edges = Vec::new();
        let mut undecoded = 0;
        program.drain(|edge| edges.push(edge), &mut undecoded);
        if undecoded > 0 {
            self.losses.undecoded += undecoded;
            self.losses.note_times(
                "a record in the idle ring was shorter than one announcement",
                undecoded,
            );
        }
        let lost = [
            Lost::skipped_runs(program.skipped()),
            Lost::dropped(
                "ring",
                program.dropped(),
                "the idle ring was full: the cpus announced faster than they were drained",
            ),
        ];
        self.counts.absorb(&mut self.losses, lost);
        let mut events = Vec::new();
        for edge in edges {
            if let Some(mut event) = self.edge(edge) {
                event.seq = seq + events.len() as u64;
                events.push(event);
            }
        }
        events
    }
}

impl Collector for IdlePeriods {
    fn value_attr(&self) -> &'static str {
        "idle_ns"
    }

    fn start(&mut self) -> Result<(), String> {
        self.program = Some(IdleProgram::load(RING_BYTES)?);
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        Ok(self.drain(seq))
    }

    fn stop(&mut self) -> Vec<Event> {
        let events = self.drain(u64::MAX / 2);
        self.program = None;
        events
    }

    fn tick(&self) -> Option<StdDuration> {
        Some(DRAIN_INTERVAL)
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    fn fields(&self) -> crate::emit::Fields {
        crate::emit::Fields::from([
            ("periods".to_owned(), self.periods),
            ("idle_total_ns".to_owned(), self.idle_total_ns),
        ])
    }

    fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if self.before_window > 0 {
            notes.push(format!(
                "{} idle period{} began before the window opened and {} in no event: only the \
                 end of {} was seen",
                self.before_window,
                if self.before_window == 1 { "" } else { "s" },
                if self.before_window == 1 { "is" } else { "are" },
                if self.before_window == 1 {
                    "it"
                } else {
                    "them"
                },
            ));
        }
        if !self.open.is_empty() {
            notes.push(format!(
                "{} cpu{} still idle when the window closed; the period {} in has no end yet \
                 and is in no event",
                self.open.len(),
                if self.open.len() == 1 {
                    " was"
                } else {
                    "s were"
                },
                if self.open.len() == 1 {
                    "it was"
                } else {
                    "they were"
                },
            ));
        }
        if self.off_target > 0 {
            notes.push(format!(
                "{} announcements came from cpus outside the target and were set aside",
                self.off_target
            ));
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(ts_ns: u64, state: u32, cpu: u32) -> IdleEdge {
        IdleEdge { ts_ns, state, cpu }
    }

    #[test]
    fn an_exit_closes_the_period_its_cpu_opened() {
        let names = BTreeMap::from([((1, 2), "C2".to_owned())]);
        let mut periods = IdlePeriods::new(None, names);
        assert!(periods.edge(edge(100, 2, 1)).is_none());
        assert!(periods.edge(edge(150, 1, 0)).is_none());
        let event = periods.edge(edge(400, IDLE_EXIT, 1)).expect("a period");
        assert_eq!(event.attr("idle_ns").unwrap().as_f64(), Some(300.0));
        assert_eq!(event.ts_ns, 100);
        assert!(event.attrs.iter().any(|attr| attr.name == "state_name"));
        assert_eq!(periods.open.len(), 1, "cpu 0 is still idle");
        assert_eq!(periods.idle_total_ns, 300);
    }

    #[test]
    fn what_cannot_be_paired_is_counted_never_guessed() {
        let mut periods = IdlePeriods::new(Some(vec![0]), BTreeMap::new());
        // An exit with no entry: the period began before the window.
        assert!(periods.edge(edge(10, IDLE_EXIT, 0)).is_none());
        // Two entries in a row: the exit between them was lost.
        periods.edge(edge(20, 1, 0));
        periods.edge(edge(30, 2, 0));
        // Another cpu, outside the target.
        periods.edge(edge(40, 1, 3));
        assert_eq!(periods.before_window, 1);
        assert_eq!(periods.losses.dropped, 1);
        assert_eq!(periods.off_target, 1);
        let notes = periods.notes().join(" ");
        assert!(notes.contains("began before the window"), "{notes}");
        assert!(notes.contains("still idle"), "{notes}");
    }
}
