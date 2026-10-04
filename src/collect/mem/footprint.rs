//! `mem.rss`, `mem.pss` and `mem.uss` over procfs.
//!
//! Three numbers, one file, and the difference between them is the whole point
//! of the family's cardinal rule. Resident size counts a shared page in full in
//! every process that maps it, proportional size divides it by the number of
//! mappers, and private size counts only what dies with the process. Summing
//! the resident size of every process on a machine therefore double-counts
//! libc; summing the proportional size does not. A tool that answered "memory"
//! with one of the three would be right for one question and wrong for the
//! other two, so the metric is named in the request and carried on every event.
//!
//! These are files, so this collector polls: there is no event to wait for,
//! only a counter to look at. Which file it read is recorded on every event,
//! because `smaps_rollup`, `smaps` and `statm` do not agree to the byte -- the
//! first two sum the VMAs the kernel walks at that instant, the last reports a
//! counter maintained on fault -- and a series that silently mixed them would
//! look like noise.

use std::path::PathBuf;
use std::time::Duration;

use super::cadence::Cadence;
use crate::clock::monotonic_ns;
use crate::collect::task::Comm;
use crate::collect::{Collector, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::request::Mode;
use crate::request::Target;

/// Which footprint is being measured.
///
/// Never inferred from anything: a request names its accounting metric, and the
/// validator refuses one that does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    /// Resident: every page in RAM, shared ones counted in full.
    Rss,
    /// Proportional: each shared page divided by the number of processes
    /// mapping it.
    Pss,
    /// Private: what would be freed if the process exited now.
    Uss,
}

impl Metric {
    fn source_id(self) -> &'static str {
        match self {
            Metric::Rss => "mem.rss",
            Metric::Pss => "mem.pss",
            Metric::Uss => "mem.uss",
        }
    }

    /// The `smaps` fields that add up to this metric.
    ///
    /// Unique shared size is the kernel's two private counters added together;
    /// there is no `Uss:` line, which is exactly why the tool that reports it
    /// has to say where it came from.
    fn fields(self) -> &'static [&'static str] {
        match self {
            Metric::Rss => &["Rss:"],
            Metric::Pss => &["Pss:"],
            Metric::Uss => &["Private_Clean:", "Private_Dirty:"],
        }
    }
}

/// Which file the number is read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// One pre-summed block, computed by the kernel over every mapping.
    SmapsRollup,
    /// Every mapping, summed here. The same numbers at a higher price.
    Smaps,
    /// Seven counters, resident size among them, and nothing else.
    Statm,
}

impl Source {
    fn path(self, pid: i32) -> PathBuf {
        match self {
            Source::SmapsRollup => PathBuf::from(format!("/proc/{pid}/smaps_rollup")),
            Source::Smaps => PathBuf::from(format!("/proc/{pid}/smaps")),
            Source::Statm => PathBuf::from(format!("/proc/{pid}/statm")),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Source::SmapsRollup => "smaps_rollup",
            Source::Smaps => "smaps",
            Source::Statm => "statm",
        }
    }
}

/// Holds the choice of metric and file until the target exists.
pub struct Prepare {
    metric: Metric,
    source: Source,
    interval: Duration,
}

impl Prepare {
    pub fn new(metric: Metric, source: Source, mode: Mode) -> Box<Self> {
        let interval = match mode {
            Mode::Poll { interval } => Duration::from_nanos(interval.as_nanos()),
            // A snapshot is one read, so the grid never comes into play.
            Mode::Snapshot => Duration::ZERO,
            _ => Duration::from_nanos(Mode::DEFAULT_POLL_INTERVAL.as_nanos()),
        };
        Box::new(Self {
            metric,
            source,
            interval,
        })
    }
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let pid = target.first_pid().ok_or_else(|| {
            format!(
                "a footprint is read per process, and a {} target names none",
                target.kind()
            )
        })?;
        Ok(Box::new(FootprintCollector::new(
            pid,
            self.metric,
            self.source,
            self.interval,
        )))
    }
}

/// Reads one footprint of one process.
pub struct FootprintCollector {
    id: CollectorId,
    pid: i32,
    /// The process across containers (`KeyValue::proc_of`), read once: a
    /// process does not change pid namespace, and an exec keeps it. `None`
    /// when the namespace link is closed to this user.
    proc_key: Option<KeyValue>,
    metric: Metric,
    source: Source,
    path: PathBuf,
    page_size: u64,
    /// `comm` is a declared filter field on all three footprint sources.
    comm: Comm,
    /// When to publish, which is not how often the run asks: a session whose
    /// fastest collector drains a ring every ten milliseconds asks everyone
    /// else just as often.
    cadence: Cadence,
}

impl FootprintCollector {
    pub fn new(pid: i32, metric: Metric, source: Source, interval: Duration) -> Self {
        Self {
            id: CollectorId::new(format!("{}/fs:proc:{}", metric.source_id(), source.label())),
            pid,
            proc_key: KeyValue::proc_of(pid),
            metric,
            source,
            path: source.path(pid),
            page_size: page_size(),
            comm: Comm::of(pid),
            cadence: Cadence::new(interval),
        }
    }

    /// The file's text, from `open_and_read` (a fresh open each call): once,
    /// and once more when the first answer is ESRCH from a process that is
    /// still running.
    ///
    /// `smaps_rollup` and `smaps` are bound at their open to the address space
    /// the process had then, and an `execve` between the open and the read
    /// leaves them bound to one that no longer exists: the read answers ESRCH
    /// although the process is alive and has a new address space to measure.
    /// A launched program is in `execve` right when the run's first reads are
    /// taken, and a read that straddled it ended the run as *"process exited"*
    /// with nothing measured. A fresh open is bound to the new address space.
    /// A second failure is the first's verdict: transient from a running
    /// process, the end from a gone one.
    fn text(
        &self,
        open_and_read: impl Fn() -> std::io::Result<String>,
    ) -> Result<String, SampleError> {
        match open_and_read() {
            Ok(text) => Ok(text),
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) && still_running(self.pid) => {
                open_and_read().map_err(|error| self.gone_or_transient(error))
            }
            Err(error) => Err(self.gone_or_transient(error)),
        }
    }

    /// A process that dies between two reads is the end of the run, not a
    /// lost sample. procfs reports it as ENOENT when the directory is already
    /// gone and as ESRCH when it is going -- or when the process is a zombie,
    /// whose address space is gone too. An ESRCH from a process that is still
    /// running is neither: it is a read that failed, counted as one.
    fn gone_or_transient(&self, error: std::io::Error) -> SampleError {
        let gone = error.kind() == std::io::ErrorKind::NotFound
            || (error.raw_os_error() == Some(libc::ESRCH) && !still_running(self.pid));
        if gone {
            SampleError::TargetGone(format!("process {} exited", self.pid))
        } else if error.kind() == std::io::ErrorKind::PermissionDenied {
            SampleError::TargetGone(format!(
                "{} is no longer readable: {error}",
                self.path.display()
            ))
        } else {
            SampleError::Transient(format!("{}: {error}", self.path.display()))
        }
    }

    fn read_bytes(&self) -> Result<u64, SampleError> {
        let text = self.text(|| std::fs::read_to_string(&self.path))?;

        match self.source {
            // `size resident shared text lib data dt`, in pages. Resident size
            // and nothing else, which is why it is the fallback and not the
            // first choice.
            Source::Statm => {
                let mut fields = text.split_whitespace();
                let parse = |value: Option<&str>| value.and_then(|value| value.parse::<u64>().ok());
                let (Some(size), Some(resident)) = (parse(fields.next()), parse(fields.next()))
                else {
                    return Err(SampleError::Transient(format!(
                        "cannot parse {}",
                        self.path.display()
                    )));
                };
                // A zombie keeps its `statm` and it reads `0 0 0 0 0 0 0`: the
                // address space is gone, and the file has no way to say so.
                // The same process's `smaps_rollup` answers ESRCH, so the
                // rollup backend ends the run where this one used to parse the
                // zero and publish it as a measurement -- the one answer a
                // footprint must never invent. A process with an address space
                // always has a non-zero total size, so the total is the tell
                // and no history is needed to read it.
                if size == 0 {
                    return Err(SampleError::TargetGone(format!(
                        "process {} has no address space left to measure",
                        self.pid
                    )));
                }
                Ok(resident * self.page_size)
            }
            // `Rss:   12345 kB`, once in the rollup and once per mapping in
            // `smaps`. Summing both the same way is what makes the two backends
            // comparable.
            Source::SmapsRollup | Source::Smaps => sum_fields(&text, self.metric.fields())
                .ok_or_else(|| {
                    SampleError::Transient(format!(
                        "no {} line in {}",
                        self.metric.fields().join(" or "),
                        self.path.display()
                    ))
                }),
        }
    }
}

impl Collector for FootprintCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "bytes"
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        if !self.cadence.due(monotonic_ns()) {
            return Ok(Vec::new());
        }
        // The timestamp is taken after the read, so it bounds the read rather
        // than preceding it: the value existed at or before this instant.
        let bytes = self.read_bytes()?;
        let ts_ns = monotonic_ns();
        let mut attrs = vec![
            Attr::uint("bytes", bytes),
            Attr::text("read_from", self.source.label()),
        ];
        if let Some(comm) = self.comm.get() {
            attrs.push(Attr::text("comm", comm));
        }
        Ok(vec![Event {
            ts_ns,
            ts_err_ns: 0,
            kind: self.metric.source_id().to_owned(),
            keys: std::iter::once(KeyValue::task(self.pid))
                .chain(self.proc_key.clone())
                .collect(),
            attrs,
            collector_id: self.id.clone(),
            seq,
        }])
    }
}

/// Adds up every occurrence of the named `smaps` fields, in bytes.
///
/// Returns `None` when not one of them appears, which means the file is not
/// what this collector thinks it is -- a kernel without `smaps_rollup`
/// support answers with an empty block rather than an error.
/// Whether `pid` is a process with an address space: present in procfs and
/// neither a zombie nor dead. A zombie's `smaps_rollup` answers ESRCH too, and
/// that one is the end of the run. The same test as `wss.rs`'s.
fn still_running(pid: i32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // The state is the first field after the command, which is in
    // parentheses and may itself hold spaces and parentheses.
    stat.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .is_some_and(|state| !matches!(state, "Z" | "X" | "x"))
}

fn sum_fields(text: &str, fields: &[&str]) -> Option<u64> {
    let mut total = 0u64;
    let mut seen = false;
    for line in text.lines() {
        for field in fields {
            let Some(rest) = line.strip_prefix(field) else {
                continue;
            };
            // `Pss:` must not match `Pss_Dirty:`; the prefix ends at the colon,
            // so what follows is whitespace and then the number.
            let Some(kilobytes) = rest
                .split_whitespace()
                .next()
                .and_then(|value| value.parse::<u64>().ok())
            else {
                continue;
            };
            total += kilobytes * 1024;
            seen = true;
        }
    }
    seen.then_some(total)
}

/// The resident size of one process, for a collector that reports another
/// number and wants to say how it compares.
///
/// Returns `None` when the file cannot be read or does not hold the line, which
/// for a caller that only wants scale is not worth an error.
pub(super) fn resident_bytes(pid: i32) -> Option<u64> {
    let text = std::fs::read_to_string(Source::SmapsRollup.path(pid)).ok()?;
    sum_fields(&text, Metric::Rss.fields())
}

fn page_size() -> u64 {
    // SAFETY: `sysconf` takes an integer and returns one; no memory is touched.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if size > 0 {
        size as u64
    } else {
        4096
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(metric: Metric, source: Source) -> Option<f64> {
        let mut collector =
            FootprintCollector::new(std::process::id() as i32, metric, source, Duration::ZERO);
        let events = collector.sample(0).ok()?;
        events[0].attr("bytes").unwrap().as_f64()
    }

    /// A footprint carries the process across containers after the thread,
    /// and it is the one `proc.fd` and `proc.threads` carry for the same
    /// process: a join `via: proc` between them meets.
    #[test]
    fn a_footprint_carries_the_process_after_the_thread() {
        let me = std::process::id() as i32;
        let mut collector = FootprintCollector::new(me, Metric::Rss, Source::Statm, Duration::ZERO);
        let events = collector.sample(0).expect("our own footprint");
        let keys = &events[0].keys;
        assert_eq!(keys[0], KeyValue::task(me), "{keys:?}");
        let Some(KeyValue::Proc { pidns, .. }) = keys.get(1) else {
            panic!("no `proc` key: {keys:?}");
        };
        // The namespace as the link's text names it, `pid:[<inode>]`: read
        // another way than `proc_of` reads it, by a `stat`.
        let link = std::fs::read_link("/proc/self/ns/pid").expect("our own namespace link");
        assert_eq!(link.to_string_lossy(), format!("pid:[{pidns}]"));
        assert_eq!(Some(&keys[1]), KeyValue::proc_of(me).as_ref());
    }

    /// A zombie's `statm` reads `0 0 0 0 0 0 0`, and that is not a footprint.
    ///
    /// The zombie is made rather than described: a child that exits and is not
    /// waited for. Nothing about the read fails -- the file opens, parses and
    /// yields a number -- which is why the distinction has to be made on the
    /// content, and why the collector used to publish a resident set of zero
    /// bytes for a process that had already gone.
    #[test]
    fn a_process_with_no_address_space_left_ends_the_run_rather_than_reading_zero() {
        // SAFETY: the child does nothing but `_exit`, which is
        // async-signal-safe, so forking from a threaded test process is sound.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            // SAFETY: the only call the child makes.
            unsafe { libc::_exit(0) };
        }

        let mut collector =
            FootprintCollector::new(pid, Metric::Rss, Source::Statm, Duration::ZERO);
        let mut outcome = collector.sample(0);
        for _ in 0..500 {
            if matches!(outcome, Err(SampleError::TargetGone(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
            outcome = collector.sample(0);
        }

        // Reaped before anything is asserted, so a failure here does not also
        // leave a zombie behind for the rest of the suite.
        // SAFETY: waiting on a child of this process.
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };

        match outcome {
            Err(SampleError::TargetGone(reason)) => {
                assert!(reason.contains("address space"), "{reason}")
            }
            Ok(events) => panic!(
                "a departed process was measured as {:?}, which is the one answer a footprint \
                 must never invent",
                events[0].attr("bytes")
            ),
            other => panic!("expected the run to end, got {other:?}"),
        }
    }

    #[test]
    fn both_backends_read_a_plausible_and_similar_resident_size() {
        let statm = read(Metric::Rss, Source::Statm).expect("statm is always readable for us");
        assert!(
            statm > 512.0 * 1024.0,
            "a running process holds more than 512 KiB"
        );

        // `smaps_rollup` may be restricted; when it reads, it must broadly agree.
        if let Some(rollup) = read(Metric::Rss, Source::SmapsRollup) {
            let ratio = rollup / statm;
            assert!(
                (0.5..2.0).contains(&ratio),
                "smaps_rollup said {rollup} and statm said {statm}"
            );
        }
    }

    /// The rollup is the kernel's own sum over the same mappings this walks, so
    /// the two must agree -- but they cannot be read at the same instant, and
    /// reading them faults in pages of their own. So the walk is bracketed
    /// between a rollup taken before it and one taken after: it has to land
    /// inside what the kernel itself reported on either side.
    ///
    /// The first measurement of this disagreed by 20 %, and the cause was the
    /// reads: the first read of each file grows this process by a few hundred
    /// kilobytes. Ordering, not arithmetic.
    #[test]
    fn the_rollup_agrees_with_the_per_mapping_walk() {
        // Reading each file once first, so that the pages the reads themselves
        // need are already resident when the comparison starts.
        for metric in [Metric::Rss, Metric::Pss, Metric::Uss] {
            for source in [Source::SmapsRollup, Source::Smaps] {
                let _ = read(metric, source);
            }
        }

        // Other threads of this test binary keep allocating while this runs,
        // and shared pages come and go in other processes, which moves PSS.
        const SLACK: f64 = 512.0 * 1024.0;

        for metric in [Metric::Rss, Metric::Pss, Metric::Uss] {
            // The other threads of this test binary allocate and free while
            // this runs, and a process that moves by megabytes between two
            // reads cannot have its two readings compared at all. So the
            // comparison is attempted until it lands in a quiet moment, and
            // gives up rather than pretending a busy one proved something.
            const ATTEMPTS: usize = 10;
            let mut last = None;
            let agreed = (0..ATTEMPTS).any(|_| {
                let (Some(before), Some(walked), Some(after)) = (
                    read(metric, Source::SmapsRollup),
                    read(metric, Source::Smaps),
                    read(metric, Source::SmapsRollup),
                ) else {
                    return false;
                };
                last = Some((before, walked, after));
                let low = before.min(after) - SLACK;
                let high = before.max(after) + SLACK;
                let quiet = (before - after).abs() <= SLACK;
                if !quiet {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                quiet && (low..=high).contains(&walked)
            });

            let Some((before, walked, after)) = last else {
                return; // `smaps_rollup` is restricted here; nothing to check.
            };
            // One disagreement proves nothing while the other threads of this
            // binary allocate and free megabytes around the reads. Ten in a row
            // is not churn.
            assert!(
                agreed,
                "{metric:?}: in {ATTEMPTS} tries the walk never landed between the rollups \
                 around it; last was {walked} against {before}..{after}"
            );
        }
    }

    /// Private memory is a subset of proportional memory, which is a subset of
    /// resident memory. The inequality holds by construction in the kernel, so
    /// a violation here means this code picked up the wrong lines.
    ///
    /// The three numbers come out of one read of the file. Reading it three
    /// times would compare three different instants of a process that grows
    /// while it reads, and the ordering would break for a reason that has
    /// nothing to do with the metrics.
    #[test]
    fn the_three_metrics_stay_in_their_order() {
        let path = Source::SmapsRollup.path(std::process::id() as i32);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return; // `smaps_rollup` is restricted here; nothing to check.
        };
        let read = |metric: Metric| sum_fields(&text, metric.fields()).expect("a rollup block");
        let (rss, pss, uss) = (read(Metric::Rss), read(Metric::Pss), read(Metric::Uss));
        assert!(uss <= pss, "uss {uss} exceeds pss {pss}");
        assert!(pss <= rss, "pss {pss} exceeds rss {rss}");
    }

    /// Ground truth for what PSS and USS *mean*.
    ///
    /// A shared mapping is touched by this process alone, then a second process
    /// starts mapping the same pages. Nothing is allocated and nothing is
    /// freed: the same physical pages are simply mapped twice. Resident size
    /// must not move, because every page is still in memory for this process;
    /// proportional size must halve for that region, because it is now divided
    /// between two mappers; unique size must lose it entirely, because it no
    /// longer dies with this process.
    ///
    /// That is the whole reason the family refuses to answer "memory" with one
    /// number, and it is checkable rather than merely plausible.
    #[test]
    fn a_second_mapper_halves_the_proportional_size_and_leaves_the_resident_one() {
        const SIZE: usize = 16 * 1024 * 1024;
        let page = page_size() as usize;

        // SAFETY: an anonymous shared mapping of our own, unmapped below.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(base, libc::MAP_FAILED, "the mapping must succeed");
        // SAFETY: writing inside the mapping we just made, to make it resident.
        unsafe { std::ptr::write_bytes(base.cast::<u8>(), 1, SIZE) };

        let base_address = base as u64;
        let Some(before) = footprints_of(base_address) else {
            // SAFETY: unmapping what was mapped above.
            unsafe { libc::munmap(base, SIZE) };
            return; // `smaps_rollup` is restricted here; nothing to check.
        };

        // SAFETY: the child touches the shared mapping and then waits. It calls
        // nothing that could deadlock on a lock held by another thread at fork.
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork must succeed");
        if child == 0 {
            // A shared anonymous mapping is not copied into the child's page
            // tables at fork; the pages become shared when the child touches
            // them. Reading one byte per page is what makes it a second mapper.
            let mut total = 0u8;
            for offset in (page..SIZE).step_by(page) {
                // SAFETY: reading inside the inherited mapping.
                total =
                    total.wrapping_add(unsafe { base.cast::<u8>().add(offset).read_volatile() });
            }
            // The first byte is the handshake: the parent measures once it
            // appears, and `total` keeps the reads from being optimised away.
            // SAFETY: writing the first byte of the inherited mapping.
            unsafe { base.cast::<u8>().write_volatile(total | 0x80) };
            // SAFETY: waiting to be killed, so the mapping stays shared.
            unsafe {
                libc::pause();
                libc::_exit(0);
            }
        }

        // Wait for the handshake rather than for a duration.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        // SAFETY: reading the first byte of our own mapping.
        while unsafe { base.cast::<u8>().read_volatile() } & 0x80 == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the child never finished touching the shared mapping"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let after = footprints_of(base_address);

        // SAFETY: the child is ours, and the mapping is ours.
        unsafe {
            libc::kill(child, libc::SIGKILL);
            libc::waitpid(child, std::ptr::null_mut(), 0);
            libc::munmap(base, SIZE);
        }
        let after = after.expect("the file was readable a moment ago");

        let size = SIZE as f64;
        assert!(
            before.0 >= 0.9 * size,
            "the mapping was written to, so it should be resident: rss {} of {size}",
            before.0
        );
        let resident_change = (after.0 - before.0).abs();
        assert!(
            resident_change < 0.1 * size,
            "the pages are still resident for this process: the mapping's rss went from {} to {}",
            before.0,
            after.0
        );
        let proportional_drop = before.1 - after.1;
        assert!(
            (0.4 * size..0.6 * size).contains(&proportional_drop),
            "a second mapper halves this region's share: pss dropped by {proportional_drop}"
        );
        let unique_drop = before.2 - after.2;
        assert!(
            unique_drop >= 0.9 * size,
            "what two processes map does not die with one of them: uss dropped by {unique_drop}"
        );
    }

    /// The three metrics of one mapping, from the per-mapping walk.
    ///
    /// Process totals cannot answer this: the rest of the test binary allocates
    /// and frees in other threads while this runs, and the claim under test is
    /// about one mapping rather than about the process. `smaps` publishes a
    /// block per mapping, and the fields inside it are the ones the collector
    /// sums.
    fn footprints_of(base: u64) -> Option<(f64, f64, f64)> {
        let text = std::fs::read_to_string(Source::Smaps.path(std::process::id() as i32)).ok()?;
        let block = mapping_block(&text, base)?;
        let read = |metric: Metric| sum_fields(block, metric.fields()).unwrap_or(0) as f64;
        Some((read(Metric::Rss), read(Metric::Pss), read(Metric::Uss)))
    }

    /// The `smaps` block of the mapping that starts at `base`.
    ///
    /// A block opens with `<start>-<end> perms ...` and runs to the next line
    /// of that shape.
    fn mapping_block(text: &str, base: u64) -> Option<&str> {
        let header_address = |line: &str| -> Option<u64> {
            let (start, rest) = line.split_once('-')?;
            // `7f..-7f.. rw-p 00000000 00:00 0` -- a field line such as
            // `Rss:  12 kB` has no dash before its first space.
            if start.contains(char::is_whitespace) || !rest.contains(' ') {
                return None;
            }
            u64::from_str_radix(start, 16).ok()
        };

        let mut opened_at = None;
        let mut offset = 0;
        for line in text.lines() {
            match (header_address(line), opened_at) {
                (Some(address), None) if address == base => opened_at = Some(offset),
                (Some(_), Some(start)) => return Some(&text[start..offset]),
                _ => {}
            }
            offset += line.len() + 1;
        }
        opened_at.map(|start| &text[start..])
    }

    /// A session ticks as fast as its fastest collector needs, which is not
    /// what the request asked for. The reads happen on the tick; the points
    /// appear on the grid.
    #[test]
    fn the_series_has_the_spacing_the_request_asked_for_and_not_the_runs() {
        let mut collector = FootprintCollector::new(
            std::process::id() as i32,
            Metric::Rss,
            Source::Statm,
            Duration::from_millis(50),
        );

        let started = std::time::Instant::now();
        let mut published = 0;
        // A tenth of a second of ten-millisecond ticks, as a run with a shim in
        // it would produce.
        while started.elapsed() < Duration::from_millis(100) {
            published += collector.sample(0).expect("statm is readable").len();
            std::thread::sleep(Duration::from_millis(10));
        }

        assert!(
            (2..=4).contains(&published),
            "a hundred milliseconds at fifty is two or three points, not {published}"
        );
    }

    #[test]
    fn a_prefix_does_not_match_a_longer_field_name() {
        let text = "Pss:  100 kB\nPss_Dirty:  40 kB\nPss_Anon:  40 kB\n";
        assert_eq!(sum_fields(text, &["Pss:"]), Some(100 * 1024));
        assert_eq!(sum_fields(text, &["Rss:"]), None);
    }

    #[test]
    fn unique_size_is_the_two_private_counters_added_up() {
        let text = "Private_Clean:  48 kB\nPrivate_Dirty:  108 kB\n";
        assert_eq!(sum_fields(text, Metric::Uss.fields()), Some(156 * 1024));
    }

    #[test]
    fn every_event_records_which_file_it_came_from() {
        let mut collector = FootprintCollector::new(
            std::process::id() as i32,
            Metric::Rss,
            Source::Statm,
            Duration::ZERO,
        );
        let events = collector.sample(3).unwrap();
        assert_eq!(events[0].attr("read_from").unwrap().to_string(), "statm");
        assert_eq!(events[0].kind, "mem.rss");
        assert_eq!(events[0].seq, 3);
        assert!(events[0].ts_ns > 0);
    }

    /// The second read, driven: an ESRCH from a running process is read once
    /// more and the second answer stands; an ESRCH that persists is a failed
    /// read, not the end, and is not read a third time; any other error is not
    /// read again; from a zombie it is the end at once, with no second read.
    #[test]
    fn an_esrch_from_a_running_process_is_read_once_more_and_never_ends_the_run() {
        let esrch = || std::io::Error::from_raw_os_error(libc::ESRCH);
        let alive = FootprintCollector::new(
            std::process::id() as i32,
            Metric::Rss,
            Source::SmapsRollup,
            Duration::ZERO,
        );

        let calls = std::cell::Cell::new(0);
        let text = alive.text(|| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                Err(esrch())
            } else {
                Ok("Rss: 4 kB\n".to_string())
            }
        });
        assert_eq!(
            calls.get(),
            2,
            "an ESRCH from a running process is read again"
        );
        assert!(text.is_ok_and(|t| t.contains("Rss")));

        calls.set(0);
        let verdict = alive.text(|| {
            calls.set(calls.get() + 1);
            Err(esrch())
        });
        assert_eq!(calls.get(), 2, "read twice, not more");
        assert!(
            matches!(verdict, Err(SampleError::Transient(_))),
            "a persisting ESRCH from a running process is a failed read: {verdict:?}"
        );

        calls.set(0);
        let other = alive.text(|| {
            calls.set(calls.get() + 1);
            Err(std::io::Error::from_raw_os_error(libc::EIO))
        });
        assert_eq!(calls.get(), 1, "only ESRCH is read again");
        assert!(matches!(other, Err(SampleError::Transient(_))), "{other:?}");

        // SAFETY: the child only exits.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork must succeed");
        if pid == 0 {
            // SAFETY: the only call the child makes.
            unsafe { libc::_exit(0) };
        }
        // Wait for the zombie without reaping it.
        let mut zombie = false;
        for _ in 0..1000 {
            if !still_running(pid) {
                zombie = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let gone = FootprintCollector::new(pid, Metric::Rss, Source::SmapsRollup, Duration::ZERO);
        calls.set(0);
        let verdict = gone.text(|| {
            calls.set(calls.get() + 1);
            Err(esrch())
        });
        // SAFETY: reaping a child of this process.
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
        assert!(zombie, "the child never became a zombie");
        assert_eq!(calls.get(), 1, "a zombie's ESRCH is not read again");
        assert!(
            matches!(verdict, Err(SampleError::TargetGone(_))),
            "a zombie has no address space left: {verdict:?}"
        );
    }

    #[test]
    fn a_process_that_does_not_exist_ends_the_run_rather_than_failing_a_read() {
        // pid 0 never names a readable process directory.
        let mut collector = FootprintCollector::new(0, Metric::Rss, Source::Statm, Duration::ZERO);
        match collector.sample(0) {
            Err(SampleError::TargetGone(_)) => {}
            other => panic!("expected the run to end, got {other:?}"),
        }
    }
}
