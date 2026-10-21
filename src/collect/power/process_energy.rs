//! `power.process_energy`: a process's share of the package's energy, an
//! estimate, and named one on every event.
//!
//! RAPL counts a whole package; no counter says which process spent which
//! joule. What powertop prints per process is an attribution: the package's
//! energy over a window times the process's share of the cpu time the machine
//! spent busy in it. That is what this computes, from `power.energy`'s own
//! counted window and two readings of `/proc`: the process's `utime + stime`
//! (its reaped children's included) and the machine's non-idle time in
//! `/proc/stat`. It leaves out what makes one busy cycle cost more than
//! another -- frequency, vector units, the uncore a memory-bound loop keeps
//! awake -- and the idle floor the package draws whoever runs, which is shared
//! in proportion like the rest. So the event's value is `joules` beside
//! `estimate`, and the measured package total travels with it.
//!
//! Both cpu times are in clock ticks (`USER_HZ`, 100 a second): a window
//! shorter than a few ticks has no share worth the name, and says so.

use std::path::PathBuf;

use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::request::{Mode, Target};

/// What the value is, on every event.
const ESTIMATE: &str = "the package's energy times this process's share of the machine's busy cpu time: an attribution, not a measurement";

/// The machine's busy ticks: every column of `/proc/stat`'s `cpu` line but
/// `idle` and `iowait` (and `guest`, `guest_nice`, already inside `user`).
fn machine_busy(stat: &str) -> Option<u64> {
    let fields: Vec<u64> = stat
        .lines()
        .find(|line| line.starts_with("cpu "))?
        .split_whitespace()
        .skip(1)
        .map(|field| field.parse().ok())
        .collect::<Option<_>>()?;
    let total: u64 = fields.iter().take(8).sum();
    Some(total - fields.get(3)? - fields.get(4)?)
}

/// A process's ticks: `utime + stime + cutime + cstime` from its `stat`.
fn process_ticks(stat: &str) -> Option<u64> {
    let (_, rest) = stat.rsplit_once(')')?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // After the command: state is field 3, so utime (14) is index 11.
    [11, 12, 13, 14]
        .iter()
        .map(|at| fields.get(*at)?.parse::<u64>().ok())
        .sum()
}

/// The share and the estimate, or why there is none.
fn estimate(package_joules: f64, process: u64, machine: u64) -> Result<(f64, f64), String> {
    if machine == 0 {
        return Err(
            "the machine spent no busy tick in the window, so there is no share to take".to_owned(),
        );
    }
    let share = (process as f64 / machine as f64).min(1.0);
    Ok((share, package_joules * share))
}

pub struct Prepare {
    pub inner: Box<dyn Prepared>,
    pub mode: Mode,
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if self.mode != Mode::Count {
            return Err(format!(
                "a share of a window's energy needs the window: `{}` cannot serve it; count it",
                self.mode
            ));
        }
        let pid = target.first_pid().ok_or_else(|| {
            format!(
                "an energy share is a process's, and a {} target names none; the machine's \
                 energy is `power.energy --system`",
                target.kind()
            )
        })?;
        Ok(Box::new(ProcessEnergy {
            pid,
            package: self.inner.attach(&Target::System)?,
            baseline: None,
            losses: Losses::default(),
            note: None,
        }))
    }
}

pub struct ProcessEnergy {
    pid: i32,
    /// `power.energy`'s counted window, over the machine.
    package: Box<dyn Collector>,
    /// The process's ticks and the machine's busy ticks at the start.
    baseline: Option<(u64, u64)>,
    losses: Losses,
    note: Option<String>,
}

impl ProcessEnergy {
    fn ticks(&self) -> Option<(u64, u64)> {
        let stat =
            std::fs::read_to_string(PathBuf::from(format!("/proc/{}/stat", self.pid))).ok()?;
        let machine = std::fs::read_to_string("/proc/stat").ok()?;
        Some((process_ticks(&stat)?, machine_busy(&machine)?))
    }
}

impl Collector for ProcessEnergy {
    fn value_attr(&self) -> &'static str {
        "joules"
    }

    fn start(&mut self) -> Result<(), String> {
        self.package.start()?;
        self.baseline = Some(self.ticks().ok_or_else(|| {
            format!(
                "cannot read process {}'s or the machine's cpu time from /proc",
                self.pid
            )
        })?);
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        self.package.sample(seq).map(|_| Vec::new())
    }

    fn stop(&mut self) -> Vec<Event> {
        let package: f64 = self
            .package
            .stop()
            .iter()
            .filter_map(|event| event.attr("joules")?.as_f64())
            .sum();
        let (Some((process_before, machine_before)), Some((process_after, machine_after))) =
            (self.baseline, self.ticks())
        else {
            self.losses.failed_reads += 1;
            self.losses.note_times(
                format!(
                "process {}'s cpu time could not be read at the close, so the window has no share",
                self.pid
            ),
                1,
            );
            return Vec::new();
        };
        let process = process_after.saturating_sub(process_before);
        let machine = machine_after.saturating_sub(machine_before);
        let (share, joules) = match estimate(package, process, machine) {
            Ok(pair) => pair,
            Err(reason) => {
                self.note = Some(reason);
                return Vec::new();
            }
        };
        if machine < 10 {
            self.note = Some(format!(
                "the machine spent {machine} busy ticks (of 10 ms) in the window: a share over so \
                 few ticks is mostly rounding"
            ));
        }
        let tick_ns = 1_000_000_000 / clock_ticks();
        vec![Event {
            ts_ns: monotonic_ns(),
            ts_err_ns: 0,
            kind: "power.process_energy".to_owned(),
            keys: vec![KeyValue::task(self.pid)],
            attrs: vec![
                Attr::float("joules", joules),
                Attr::float("package_joules", package),
                Attr::float("share", share),
                Attr::uint("process_cpu_ns", process * tick_ns),
                Attr::uint("machine_busy_ns", machine * tick_ns),
                Attr::uint("pid", self.pid as u64),
                Attr::text("estimate", ESTIMATE),
            ],
            collector_id: CollectorId::unstamped(),
            seq: u64::MAX / 2,
        }]
    }

    fn losses(&self) -> Losses {
        let mut losses = self.package.losses();
        losses.failed_reads += self.losses.failed_reads;
        for (reason, count) in &self.losses.reasons {
            *losses.reasons.entry(reason.clone()).or_insert(0) += count;
        }
        losses
    }

    fn notes(&self) -> Vec<String> {
        let mut notes = self.package.notes();
        notes.extend(self.note.clone());
        notes
    }
}

fn clock_ticks() -> u64 {
    // SAFETY: `sysconf` takes an integer and returns one; no memory is touched.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks > 0 {
        ticks as u64
    } else {
        100
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_machine_s_busy_time_leaves_out_idle_and_iowait() {
        let stat = "cpu  100 5 50 1000 20 3 2 0 0 0\ncpu0 1 1 1 1 1 1 1 0 0 0\n";
        assert_eq!(machine_busy(stat), Some(160));
    }

    #[test]
    fn a_process_s_time_counts_its_reaped_children() {
        let stat = "42 (a (weird) name) S 1 42 42 0 -1 4194560 100 0 0 0 30 7 4 2 20 0 1 0";
        assert_eq!(process_ticks(stat), Some(43));
        let own = std::fs::read_to_string("/proc/self/stat").unwrap();
        assert!(process_ticks(&own).is_some());
    }

    #[test]
    fn the_estimate_is_the_package_times_the_share() {
        let (share, joules) = estimate(40.0, 25, 100).unwrap();
        assert_eq!(share, 0.25);
        assert_eq!(joules, 10.0);
        assert!(estimate(40.0, 1, 0).is_err());
        assert_eq!(estimate(40.0, 200, 100).unwrap().0, 1.0);
    }
}
