//! `sched.runqueue` over `/proc/stat` and `/proc/loadavg`.
//!
//! The machine's run queue as `vmstat` and `uptime` report it: how many tasks
//! were runnable at the moment of the read (`procs_running`, vmstat's `r`),
//! how many were blocked in uninterruptible sleep (`procs_blocked`, its `b`),
//! and the kernel's exponentially damped averages of the two together (the
//! load averages). These are instantaneous counts, not durations: a read is
//! one look at the queue, and a series of reads is a sampled run queue length.
//!
//! **One series for the machine.** The kernel keeps a per-cpu `nr_running`
//! and exports none of it outside debugfs, so the per-cpu queue length
//! `runqlen.bt` samples is not here; `procs_running` is their sum at one
//! instant, the reading thread included.

use crate::clock::monotonic_ns;
use crate::collect::{Collector, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event};
use crate::request::Target;

const STAT: &str = "/proc/stat";
const LOADAVG: &str = "/proc/loadavg";

/// One look at the queue.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Reading {
    pub running: u64,
    pub blocked: u64,
    pub load1: f64,
    pub load5: f64,
    pub load15: f64,
    /// Every task on the machine, the denominator of `loadavg`'s fourth field.
    pub tasks: u64,
}

impl Reading {
    /// The two counters out of `/proc/stat`, and the averages and task count
    /// out of `/proc/loadavg`.
    pub(crate) fn parse(stat: &str, loadavg: &str) -> Option<Self> {
        let mut reading = Reading::default();
        let mut seen = (false, false);
        for line in stat.lines() {
            let mut fields = line.split_whitespace();
            match fields.next() {
                Some("procs_running") => {
                    reading.running = fields.next()?.parse().ok()?;
                    seen.0 = true;
                }
                Some("procs_blocked") => {
                    reading.blocked = fields.next()?.parse().ok()?;
                    seen.1 = true;
                }
                _ => {}
            }
        }
        if seen != (true, true) {
            return None;
        }
        let mut fields = loadavg.split_whitespace();
        reading.load1 = fields.next()?.parse().ok()?;
        reading.load5 = fields.next()?.parse().ok()?;
        reading.load15 = fields.next()?.parse().ok()?;
        // `runnable/total`: the runnable half is the same instant's
        // `procs_running` by another count, and the reading's own is kept.
        reading.tasks = fields.next()?.split_once('/')?.1.parse().ok()?;
        Some(reading)
    }
}

/// `poll` and `snapshot` read the same two files the same way.
pub struct Prepare;

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        match target {
            Target::System => Ok(Box::new(RunqueueCollector {
                id: CollectorId::new("sched.runqueue/fs:proc"),
            })),
            other => Err(format!(
                "`{STAT}` counts the machine's runnable tasks, and a {} target is a narrower \
                 scope the file does not split: use --system",
                other.kind()
            )),
        }
    }
}

pub struct RunqueueCollector {
    id: CollectorId,
}

fn read() -> Result<Reading, SampleError> {
    let stat = std::fs::read_to_string(STAT)
        .map_err(|error| SampleError::Transient(format!("{STAT}: {error}")))?;
    let loadavg = std::fs::read_to_string(LOADAVG)
        .map_err(|error| SampleError::Transient(format!("{LOADAVG}: {error}")))?;
    Reading::parse(&stat, &loadavg).ok_or_else(|| {
        SampleError::Transient(format!(
            "no `procs_running` and `procs_blocked` in {STAT}, or no averages in {LOADAVG}"
        ))
    })
}

fn event(reading: Reading, ts_ns: u64, id: &CollectorId, seq: u64) -> Event {
    Event {
        ts_ns,
        ts_err_ns: 0,
        kind: "sched.runqueue".to_owned(),
        keys: Vec::new(),
        attrs: vec![
            Attr::uint("running", reading.running),
            Attr::uint("blocked", reading.blocked),
            Attr::float("load1", reading.load1),
            Attr::float("load5", reading.load5),
            Attr::float("load15", reading.load15),
            Attr::uint("tasks", reading.tasks),
        ],
        collector_id: id.clone(),
        seq,
    }
}

impl Collector for RunqueueCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "running"
    }

    fn start(&mut self) -> Result<(), String> {
        read().map(|_| ()).map_err(|error| format!("{error:?}"))
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let reading = read()?;
        Ok(vec![event(reading, monotonic_ns(), &self.id, seq)])
    }

    fn facts(&self) -> Vec<String> {
        vec![
            "`running` counts the runnable tasks at the instant of each read, this tool's \
             reading thread among them, so an idle machine reads 1; `blocked` is the tasks \
             in uninterruptible sleep, and the load averages are the kernel's damped average \
             of both"
                .to_owned(),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reading_parses_out_of_both_files() {
        let stat = "cpu  1 2 3 4\nctxt 99\nprocs_running 3\nprocs_blocked 1\n";
        let reading = Reading::parse(stat, "0.52 0.48 0.40 2/1311 9999\n").expect("parses");
        assert_eq!(reading.running, 3);
        assert_eq!(reading.blocked, 1);
        assert_eq!(reading.load5, 0.48);
        assert_eq!(reading.tasks, 1311);
        assert!(Reading::parse("ctxt 1\n", "0 0 0 1/1 1").is_none());
    }

    /// Ground truth from a workload of known shape: four threads that only
    /// spin are four runnable tasks the read must see, whatever else the
    /// machine runs.
    #[test]
    fn spinning_threads_are_counted_as_runnable() {
        let idle = read().expect("the files are readable").running;
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let spinners: Vec<_> = (0..4)
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        std::hint::spin_loop();
                    }
                })
            })
            .collect();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let busiest = (0..20)
            .map(|_| read().expect("readable").running)
            .max()
            .expect("twenty reads");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for spinner in spinners {
            spinner.join().expect("a spinner");
        }
        assert!(
            busiest >= 5,
            "four spinners and the reader are five runnable tasks; read {busiest} (idle {idle})"
        );
    }
}
