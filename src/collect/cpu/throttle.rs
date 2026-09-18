//! `cpu.throttle` over a cgroup's `cpu.stat`.
//!
//! A throttled run is not a slower run: it is a different run. The CFS
//! bandwidth controller stops a cgroup dead for the rest of its period once the
//! quota is spent, so latency acquires a floor that has nothing to do with the
//! work, and averaging such a run in with the others hides it in the spread.
//! This collector exists so that a run can be told apart rather than averaged.
//!
//! `nr_throttled` and `throttled_usec` appear in `cpu.stat` only once a quota
//! is set. Their absence means the cgroup has no `cpu.max` and therefore cannot
//! be throttled, which is a different statement from "was not throttled" -- and
//! the difference is reported rather than flattened into a zero.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::request::{Mode, Target};

const ROOT: &str = "/sys/fs/cgroup";

/// Resolves a cgroup target to the directory that holds its `cpu.stat`.
///
/// A target may be written the way `/proc/self/cgroup` writes it, rooted at the
/// hierarchy, or as the mounted path. Both name the same cgroup, and a tool
/// that accepted only one of them would refuse the form its own output uses.
fn directory(target: &Path) -> PathBuf {
    if target.starts_with(ROOT) {
        target.to_path_buf()
    } else {
        PathBuf::from(ROOT).join(target.strip_prefix("/").unwrap_or(target))
    }
}

/// The throttling counters of one `cpu.stat`, when the file has them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Throttling {
    periods: u64,
    throttled: u64,
    throttled_us: u64,
}

impl Throttling {
    /// Reads the three counters, or nothing when the cgroup has no quota and
    /// the file therefore has no throttling lines at all.
    fn parse(text: &str) -> Option<Self> {
        let fields: BTreeMap<&str, u64> = text
            .lines()
            .filter_map(|line| line.split_once(' '))
            .filter_map(|(name, value)| Some((name, value.trim().parse().ok()?)))
            .collect();
        Some(Throttling {
            periods: *fields.get("nr_periods")?,
            throttled: *fields.get("nr_throttled")?,
            throttled_us: *fields.get("throttled_usec")?,
        })
    }
}

/// Holds the mode until the target is known.
pub struct Prepare(pub Mode);

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        match target {
            Target::Cgroup(path) => {
                let directory = directory(path);
                let file = directory.join("cpu.stat");
                if !file.exists() {
                    return Err(format!(
                        "{} does not exist: either the cgroup is not there, or the cpu \
                         controller is not enabled on it",
                        file.display()
                    ));
                }
                Ok(Box::new(ThrottleCollector::new(directory, self.0)))
            }
            Target::Container(_) => Err(
                "a container has to be resolved to its cgroup first, which is not built yet; \
                 pass `--cgroup` with the path"
                    .to_owned(),
            ),
            other => Err(format!(
                "throttling is a property of a cgroup, and a {} target names none: the CFS \
                 quota that throttles is set per cgroup",
                other.kind()
            )),
        }
    }
}

/// Reads one cgroup's throttling counters.
pub struct ThrottleCollector {
    id: CollectorId,
    directory: PathBuf,
    mode: Mode,
    previous: Option<Throttling>,
    losses: Losses,
    notes: Vec<String>,
}

impl ThrottleCollector {
    pub fn new(directory: PathBuf, mode: Mode) -> Self {
        Self {
            id: CollectorId::new("cpu.throttle/fs:cgroup"),
            directory,
            mode,
            previous: None,
            losses: Losses::default(),
            notes: Vec::new(),
        }
    }

    fn read(&self) -> Result<Option<Throttling>, SampleError> {
        let file = self.directory.join("cpu.stat");
        let text = std::fs::read_to_string(&file).map_err(|error| {
            // A cgroup that goes away mid-run ends the run; it is not a read
            // that happened to fail.
            if error.kind() == std::io::ErrorKind::NotFound {
                SampleError::TargetGone(format!("{} is gone", self.directory.display()))
            } else {
                SampleError::Transient(format!("{}: {error}", file.display()))
            }
        })?;
        Ok(Throttling::parse(&text))
    }
}

impl Collector for ThrottleCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "throttled_ns"
    }

    fn start(&mut self) -> Result<(), String> {
        let baseline = self.read().map_err(|error| format!("{error:?}"))?;
        // A snapshot of these counters is a statement about the cgroup's whole
        // life, which is the only thing a single read can honestly be.
        if self.mode == Mode::Snapshot {
            if baseline.is_none() {
                self.notes.push(format!(
                    "{}/cpu.stat has no throttling counters: no cpu.max quota is set, so this \
                     cgroup cannot be throttled -- which is not the same as not having been",
                    self.directory.display()
                ));
            }
            return Ok(());
        }
        if baseline.is_none() {
            self.notes.push(format!(
                "{}/cpu.stat has no throttling counters: no cpu.max quota is set, so this \
                 cgroup cannot be throttled -- which is not the same as not having been",
                self.directory.display()
            ));
        }
        self.previous = baseline;
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let Some(now) = self.read()? else {
            // No quota, so nothing to report. Said once, in `start`, rather
            // than counted as a failure on every tick.
            return Ok(Vec::new());
        };
        let ts_ns = monotonic_ns();
        let (window, previous) = match self.previous.replace(now) {
            Some(previous) => ("since the previous read", previous),
            None => ("since the cgroup was created", Throttling::default()),
        };

        let throttled_us = now.throttled_us.saturating_sub(previous.throttled_us);
        let periods = now.periods.saturating_sub(previous.periods);
        let throttled = now.throttled.saturating_sub(previous.throttled);

        Ok(vec![Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "cpu.throttle".to_owned(),
            keys: vec![KeyValue::Cgroup {
                id: cgroup_id(&self.directory),
            }],
            attrs: vec![
                Attr::uint("throttled_ns", throttled_us * 1_000),
                Attr::uint("nr_throttled", throttled),
                Attr::uint("nr_periods", periods),
                // The share of periods that ended early is what says whether
                // the quota is a ceiling the workload occasionally touches or a
                // wall it is living against.
                Attr::float(
                    "throttled_periods",
                    if periods == 0 {
                        0.0
                    } else {
                        throttled as f64 / periods as f64
                    },
                ),
                Attr::text("window", window),
            ],
            collector_id: self.id.clone(),
            seq,
        }])
    }

    fn tick(&self) -> Option<StdDuration> {
        None
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    fn notes(&self) -> Vec<String> {
        self.notes.clone()
    }
}

/// The cgroup's inode number, which is the id the kernel itself uses for it.
fn cgroup_id(directory: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(directory)
        .map(|metadata| metadata.ino())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cgroup_without_a_quota_has_no_throttling_counters() {
        // The kernel writes these three only once `cpu.max` is set.
        assert!(Throttling::parse("usage_usec 262054076\nuser_usec 222854039\n").is_none());
        let parsed =
            Throttling::parse("usage_usec 10\nnr_periods 7\nnr_throttled 3\nthrottled_usec 4200\n")
                .expect("a quota'd cgroup");
        assert_eq!(
            (parsed.periods, parsed.throttled, parsed.throttled_us),
            (7, 3, 4200)
        );
    }

    #[test]
    fn a_target_written_either_way_names_the_same_cgroup() {
        assert_eq!(
            directory(Path::new("/user.slice/app.scope")),
            PathBuf::from("/sys/fs/cgroup/user.slice/app.scope")
        );
        assert_eq!(
            directory(Path::new("/sys/fs/cgroup/user.slice/app.scope")),
            PathBuf::from("/sys/fs/cgroup/user.slice/app.scope")
        );
    }

    /// Ground truth: a cgroup given a tenth of a cpu and a program that wants a
    /// whole one has to come back throttled, and by an amount the kernel agrees
    /// with.
    ///
    /// The cgroup is created under the first ancestor of this process's own
    /// cgroup that delegates the cpu controller. Without delegation -- most
    /// machines, and every one where this runs as a plain user under a manager
    /// that does not delegate -- the test says so and stops.
    #[test]
    fn a_cgroup_capped_below_what_it_wants_reports_being_throttled() {
        let Some(parent) = delegated_parent() else {
            eprintln!("skipped: no cgroup here delegates the cpu controller to this user");
            return;
        };
        let cgroup = parent.join(format!("omn-throttle-test-{}", std::process::id()));
        if std::fs::create_dir(&cgroup).is_err() {
            eprintln!("skipped: cannot create a cgroup under {}", parent.display());
            return;
        }
        let _cleanup = Cleanup(cgroup.clone());

        // A tenth of one cpu, in periods of 100 ms.
        if std::fs::write(cgroup.join("cpu.max"), "10000 100000").is_err() {
            eprintln!("skipped: cpu.max is not writable in the new cgroup");
            return;
        }

        let child = std::process::Command::new("/bin/sh")
            .args(["-c", "while :; do :; done"])
            .spawn();
        let Ok(mut child) = child else {
            eprintln!("skipped: cannot launch a spinner");
            return;
        };
        std::fs::write(cgroup.join("cgroup.procs"), child.id().to_string())
            .expect("moving the spinner into the cgroup");

        let mut collector = ThrottleCollector::new(
            cgroup.clone(),
            Mode::Poll {
                interval: Mode::DEFAULT_POLL_INTERVAL,
            },
        );
        collector.start().expect("cpu.stat is readable");
        // Three periods, so there is something to have been stopped during.
        std::thread::sleep(StdDuration::from_millis(400));
        let events = collector.sample(0).expect("a second read");
        let _ = child.kill();
        let _ = child.wait();

        assert!(collector.notes().is_empty(), "{:?}", collector.notes());
        let event = &events[0];
        let throttled_ns = event.attr("throttled_ns").unwrap().as_f64().unwrap();
        let periods = event.attr("nr_periods").unwrap().as_f64().unwrap();
        let throttled = event.attr("nr_throttled").unwrap().as_f64().unwrap();
        assert!(
            throttled >= 1.0,
            "a spinner capped at a tenth of a cpu was never throttled over {periods} periods"
        );
        // Each 100 ms period gives 10 ms of runtime, so roughly 90 ms of every
        // period is spent stopped. Half of that is a floor no correct reading
        // can fall below.
        assert!(
            throttled_ns > 0.5 * throttled * 90_000_000.0,
            "{throttled} throttled periods should hold far more than {throttled_ns}ns of \
             stopped time"
        );
    }

    /// The first ancestor cgroup of this process that lets this user create a
    /// child with the cpu controller in it.
    #[cfg(test)]
    fn delegated_parent() -> Option<PathBuf> {
        let own = std::fs::read_to_string("/proc/self/cgroup").ok()?;
        let path = own.lines().find_map(|line| line.rsplit(':').next())?;
        let mut candidate = directory(Path::new(path));
        loop {
            let controllers =
                std::fs::read_to_string(candidate.join("cgroup.subtree_control")).ok();
            let delegates = controllers
                .map(|controllers| {
                    controllers
                        .split_whitespace()
                        .any(|controller| controller == "cpu")
                })
                .unwrap_or(false);
            if delegates && std::fs::metadata(&candidate).is_ok() {
                return Some(candidate);
            }
            candidate = candidate.parent()?.to_path_buf();
            if !candidate.starts_with(ROOT) {
                return None;
            }
        }
    }

    /// Removes the test cgroup whatever the test does, because a cgroup left
    /// behind outlives the process that made it.
    struct Cleanup(PathBuf);

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir(&self.0);
        }
    }
}
