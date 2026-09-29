//! Counting a cpu rather than a task.
//!
//! `perf_event_open` follows one of two things and never both: a task, on every
//! cpu it runs on, or a cpu, for every task that runs on it. The first is what
//! `counters.rs` does and it is the right question for "what did this program
//! cost". The second is the only way to answer "what is this core doing", and
//! it is a genuinely different measurement -- it includes the kernel, the
//! interrupt handlers, and every other tenant of the machine.
//!
//! Three things follow from that, and each of them is a place this could have
//! quietly lied instead.
//!
//! **A cpu target is one event per cpu**, not one event that covers them.
//! There is no machine-wide counter in the interface, so `--system` opens a
//! group on each online cpu.
//!
//! **The cpus are kept apart.** Each one produces its own event, keyed on its
//! own number. A sum over twenty-four cores describes no core -- one saturated
//! among twenty-three idle ones reads exactly like a machine at four per cent
//! -- and a reader who wants the total can ask the emit layer for it, which is
//! the direction that does not lose anything.
//!
//! **The privileges are not optional.** Counting a cpu means counting other
//! users' work, so the kernel asks for `perf_event_paranoid` at zero or below,
//! or `CAP_PERFMON`. Where that is refused this says which of the two is
//! missing and stops, because the alternative -- counting the calling process
//! instead -- would answer a different question under the same name.

use std::time::Duration as StdDuration;

use super::counters::{measured, measured_tick, multiplexing_notes, Spec, Ticks};
use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::perf::{PerCpuGroups, Reading};

/// How often the collector is asked for events. It has none to give until the
/// window closes; the tick only keeps the observed window from being rounded up
/// to whatever the runner would otherwise wait for.
const TICK: StdDuration = StdDuration::from_millis(10);

/// One source's counters, opened on every cpu of a set and read at the end.
pub struct PerCpuCounters {
    id: CollectorId,
    spec: &'static Spec,
    /// The cpus as the request named them, kept so a refusal can say which one
    /// it was refused on before any group exists.
    cpus: Vec<u32>,
    groups: Option<PerCpuGroups>,
    losses: Losses,
    notes: Vec<String>,
    /// A poll's tick and each cpu's previous reading; `None` counts the
    /// window once.
    poll: Option<StdDuration>,
    ticks: std::collections::BTreeMap<u32, Ticks>,
    opened_ns: u64,
    seq: u64,
}

impl PerCpuCounters {
    /// Reads the counters every `poll` while they run, one event per cpu per
    /// tick, instead of once for the window.
    pub fn polled(mut self, poll: Option<StdDuration>) -> Self {
        self.poll = poll;
        self
    }

    /// One cpu's reading as an event: the window's, or a tick's.
    fn event(&mut self, cpu: u32, readings: Vec<Reading>, ts_ns: u64) -> Option<Event> {
        let mut attrs = if self.poll.is_some() {
            let (delta, interval_ns) =
                self.ticks
                    .entry(cpu)
                    .or_default()
                    .next(self.opened_ns, ts_ns, readings);
            measured_tick(self.spec, &delta, interval_ns, &mut self.losses)?
        } else {
            self.notes
                .extend(multiplexing_notes(&readings, &format!("cpu {cpu}")));
            measured(self.spec, &readings, &mut self.losses)?
        };
        // `cpu` is a filter field the catalog declares, and a key is not a
        // filter field: a predicate cannot read one.
        attrs.push(Attr::uint("cpu", u64::from(cpu)));
        let seq = self.seq;
        self.seq += 1;
        Some(Event {
            ts_ns,
            ts_err_ns: 0,
            kind: self.spec.source.to_owned(),
            keys: vec![KeyValue::cpu(cpu)],
            attrs,
            collector_id: self.id.clone(),
            seq,
        })
    }

    pub fn new(cpus: Vec<u32>, spec: &'static Spec) -> Self {
        Self {
            id: CollectorId::new(format!("{}/{}", spec.source, spec.backend)),
            spec,
            cpus,
            groups: None,
            losses: Losses::default(),
            notes: Vec::new(),
            poll: None,
            ticks: std::collections::BTreeMap::new(),
            opened_ns: 0,
            seq: 0,
        }
    }
}

impl Collector for PerCpuCounters {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        self.spec.value_attr()
    }

    fn start(&mut self) -> Result<(), String> {
        let groups = PerCpuGroups::open(self.spec.counters, &self.cpus)
            .map_err(|reason| format!("`{}`: {reason}", self.spec.source))?;
        if groups.kernel_excluded() {
            // Worth saying loudly here rather than in passing: the whole reason
            // to count a cpu is to see everything that ran on it, and a
            // user-mode-only counter is missing exactly the part -- interrupt
            // handlers, softirqs, the kernel's own work -- that a per-task
            // count could never have shown either.
            self.notes.push(format!(
                "this machine would only open `{}` for user mode, so the kernel's own work \
                 on these cpus is not in the count -- which is most of what counting a cpu \
                 rather than a task is for",
                self.spec.source
            ));
        }
        groups.enable()?;
        self.opened_ns = monotonic_ns();
        self.groups = Some(groups);
        Ok(())
    }

    fn sample(&mut self, _seq: u64) -> Result<Vec<Event>, SampleError> {
        // A cpu does not exit, so unlike the per-task collector there is
        // nothing to notice here: a counted window has one value and it is read
        // when the window closes, and a poll reads every cpu on its tick.
        if self.poll.is_none() {
            return Ok(Vec::new());
        }
        let Some(readings) = self.groups.as_ref().map(PerCpuGroups::read) else {
            return Ok(Vec::new());
        };
        let ts_ns = monotonic_ns();
        let mut events = Vec::new();
        for (cpu, reading) in readings {
            match reading {
                Ok(readings) => events.extend(self.event(cpu, readings, ts_ns)),
                Err(error) => {
                    self.losses.failed_reads += 1;
                    self.losses
                        .note(format!("reading the counters on cpu {cpu} failed: {error}"));
                }
            }
        }
        Ok(events)
    }

    fn stop(&mut self) -> Vec<Event> {
        let Some(groups) = self.groups.take() else {
            return Vec::new();
        };
        let ts_ns = monotonic_ns();
        let mut events = Vec::with_capacity(self.cpus.len());
        for (cpu, reading) in groups.read_final() {
            let readings = match reading {
                Ok(readings) => readings,
                Err(error) => {
                    // A cpu taken offline mid-run is this, and it is a failed
                    // read rather than a failed run: the other cpus counted a
                    // real window, and dropping them would hide a measurement
                    // to report a problem.
                    self.losses.failed_reads += 1;
                    self.losses
                        .note(format!("reading the counters on cpu {cpu} failed: {error}"));
                    continue;
                }
            };
            events.extend(self.event(cpu, readings, ts_ns));
        }
        events
    }

    fn tick(&self) -> Option<StdDuration> {
        Some(self.poll.unwrap_or(TICK))
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    fn notes(&self) -> Vec<String> {
        self.notes.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::cpu::counters::spec_for;
    use std::process::{Child, Command};

    fn spec(source: &str, backend: &str) -> &'static Spec {
        spec_for(source, backend).expect("the source is in the table")
    }

    /// Runs a shell loop of `iterations` turns, pinned to one cpu.
    ///
    /// The pinning is the point: the work has to be somewhere known, and it has
    /// to be in another process, because a per-task counter opened on this
    /// process would see none of it. That is the difference this whole module
    /// exists for, and it is what the test below measures.
    fn loop_pinned_to(cpu: u32, iterations: u32) -> Option<Child> {
        use std::os::unix::process::CommandExt;
        let script = format!("i=0; while [ $i -lt {iterations} ]; do i=$((i+1)); done");
        // SAFETY: `sched_setaffinity` between fork and exec touches only this
        // child's own affinity mask, which is async-signal-safe.
        unsafe {
            Command::new("/bin/sh")
                .args(["-c", &script])
                .pre_exec(move || {
                    let mut set: libc::cpu_set_t = std::mem::zeroed();
                    libc::CPU_ZERO(&mut set);
                    libc::CPU_SET(cpu as usize, &mut set);
                    if libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) < 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                })
                .spawn()
        }
        .ok()
    }

    /// The count of one run of that loop, measured per task -- which is what
    /// the process itself retired, and therefore a floor for what any counter
    /// watching the cpu it ran on must have seen.
    fn instructions_of_the_loop(iterations: u32) -> Option<u64> {
        use crate::perf::{CounterGroup, Scope};
        let mut child = loop_pinned_to(0, iterations)?;
        let group = CounterGroup::open_user_mode(
            spec("cpu.instructions", "pmu").counters,
            Scope::Process(child.id() as i32),
        )
        .ok()?;
        group.enable().ok()?;
        let _ = child.wait();
        let readings = group.read_final().ok()?;
        Some(readings[0].value)
    }

    /// The ground truth of this module: a per-cpu counter counts a process
    /// that is not this one.
    ///
    /// The work is a shell loop pinned to one cpu, and how many instructions it
    /// retires is measured first, per task, by the mechanism that already
    /// worked. A counter opened on that cpu must see at least that many. A
    /// per-task counter opened on this process would see none of them, and a
    /// counter that ignored the cpu it was given would be counting somewhere
    /// else -- both of which this catches, and neither of which a test against
    /// the collector's own output would notice.
    #[test]
    fn a_cpu_counter_counts_a_process_that_is_not_this_one() {
        const ITERATIONS: u32 = 300_000;
        let Some(expected) = instructions_of_the_loop(ITERATIONS) else {
            eprintln!("skipped: this machine refuses unprivileged perf counting");
            return;
        };
        assert!(
            expected > 0,
            "the loop retired nothing, so there is no truth to check against"
        );

        let mut collector = PerCpuCounters::new(vec![0], spec("cpu.instructions", "pmu"));
        if let Err(reason) = collector.start() {
            assert!(
                reason.contains("perf_event_paranoid") || reason.contains("CAP_PERFMON"),
                "a per-cpu counter was refused for a reason that is not about privileges: {reason}"
            );
            eprintln!("skipped: {reason}");
            return;
        }
        let mut child = loop_pinned_to(0, ITERATIONS).expect("the loop starts");
        let _ = child.wait();
        let events = collector.stop();

        assert_eq!(events.len(), 1, "one cpu was asked for, so one event");
        let event = &events[0];
        assert_eq!(event.keys, vec![KeyValue::cpu(0)]);
        let counted = event.attr("count").unwrap().as_f64().unwrap();
        assert!(
            counted >= expected as f64,
            "cpu 0 ran a program that retired {expected} instructions and the counter \
             watching that cpu saw {counted}: it is not counting the cpu"
        );
        assert!(collector.losses().is_clean(), "{:?}", collector.losses());
    }

    /// Several cpus are several events, each carrying the number it describes.
    /// A set that came back as one event, or as several without their cpu,
    /// would be a mean of cores wearing a plausible name.
    #[test]
    fn every_cpu_of_a_set_reports_separately_and_says_which_it_is() {
        let online = crate::perf::online_cpus().expect("online cpus");
        if online.len() < 2 {
            eprintln!("skipped: this machine has one cpu, so there is no set to keep apart");
            return;
        }
        let asked: Vec<u32> = online.iter().copied().take(2).collect();

        let mut collector = PerCpuCounters::new(asked.clone(), spec("cpu.cycles", "pmu"));
        if let Err(reason) = collector.start() {
            eprintln!("skipped: {reason}");
            return;
        }
        std::thread::sleep(StdDuration::from_millis(50));
        let events = collector.stop();

        assert_eq!(events.len(), asked.len());
        for (event, cpu) in events.iter().zip(&asked) {
            assert_eq!(event.keys, vec![KeyValue::cpu(*cpu)]);
            // The key names the cpu for a join; the attribute names it for a
            // filter. Both are needed and they are not the same thing.
            assert_eq!(event.attr("cpu").unwrap().as_f64(), Some(f64::from(*cpu)));
            assert!(event.attr("enabled_ns").unwrap().as_f64().unwrap() > 0.0);
            assert_eq!(event.kind, "cpu.cycles");
        }
        // Distinct sequence numbers, or the events collapse into one another
        // wherever a series is built from them.
        assert_eq!(events[0].seq, 0);
        assert_eq!(events[1].seq, 1);
    }

    /// A cpu this machine does not have is not a permission problem, and
    /// answering with the permission message would send the reader to a sysctl
    /// that was never in the way. The refusal names the source too: a session
    /// runs several at once and "cpu 1023 is not online" belongs to one of them.
    #[test]
    fn a_cpu_that_does_not_exist_is_refused_as_absent_rather_than_as_forbidden() {
        let absent = crate::perf::online_cpus()
            .expect("online cpus")
            .iter()
            .max()
            .copied()
            .expect("at least one cpu")
            + 1_000;

        let mut collector = PerCpuCounters::new(vec![absent], spec("cpu.cycles", "pmu"));
        let reason = collector
            .start()
            .expect_err("a cpu that does not exist cannot be counted");
        assert!(reason.contains("not online"), "{reason}");
        assert!(reason.contains("cpu.cycles"), "{reason}");
    }
}
