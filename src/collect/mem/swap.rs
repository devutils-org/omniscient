//! `mem.swap` over `/proc/vmstat` and a cgroup's `memory.stat`.
//!
//! What is paged out is not what is free, and neither is what moved. Three
//! different numbers get called "swap":
//!
//! - the **flow**: pages the kernel wrote to, or read back from, a swap device
//!   during a window. That is what this source measures, because it is the one
//!   that costs time -- a swap-in is a fault that has to wait for a disk.
//! - the **stock**: how much of the process, or the cgroup, is sitting on a
//!   swap device right now. Carried beside the flow where the kernel publishes
//!   it, because a cgroup that swapped a gigabyte out and read it all back has
//!   a flow of two gigabytes and a stock of nothing.
//! - free swap space, which says nothing about either and is not here.
//!
//! Both backends report the flow, which is what makes them interchangeable:
//! `/proc/vmstat` counts it for the machine, a cgroup's `memory.stat` counts
//! the same event for the tasks charged to that cgroup.
//!
//! A trap worth naming, because it silently scales a number by 256: the
//! counters in `/proc/vmstat` are not all in the same unit. `pgpgin` and
//! `pgpgout` are in kilobytes; `pswpin` and `pswpout`, which are the ones read
//! here, are in **pages**. So are the cgroup's, while almost every other field
//! of `memory.stat` is in bytes.
//!
//! And a second one, which makes the flow and the stock disagree for a reason
//! that is not a bug: **a zero-filled page is never written to a swap device.**
//! The kernel notices, records that the page was zero, and counts it in
//! `swpout_zero` instead of `pswpout` -- so a process that allocates a
//! gigabyte and leaves it untouched grows the cgroup's `memory.swap.current`
//! by a gigabyte without a single page of traffic. Both counters are carried,
//! under names that say which is which.

use std::path::PathBuf;

use super::cgroup::{stat_field, stat_fields, Cgroup};
use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::request::{Mode, Target};

const VMSTAT: &str = "/proc/vmstat";

/// Where the swap counters are read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// `/proc/vmstat`: every swap device, for the whole machine.
    Machine,
    /// `<cgroup>/memory.stat`: the tasks charged to one cgroup.
    Cgroup,
}

impl Scope {
    fn backend(self) -> &'static str {
        match self {
            Scope::Machine => "fs:proc",
            Scope::Cgroup => "fs:cgroup",
        }
    }

    fn read_from(self) -> &'static str {
        match self {
            Scope::Machine => "/proc/vmstat",
            Scope::Cgroup => "memory.stat",
        }
    }
}

/// The two counters, in pages, as they stood at one instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Counters {
    ts_ns: u64,
    in_pages: u64,
    out_pages: u64,
    /// Pages the kernel accounted as swapped without writing them anywhere,
    /// because they were full of zeros. Absent on a kernel that does not
    /// separate them, where they are simply not counted at all.
    in_zero_pages: Option<u64>,
    out_zero_pages: Option<u64>,
}

/// Holds the mode and the scope until the target is known.
pub struct Prepare {
    mode: Mode,
    scope: Scope,
}

impl Prepare {
    pub fn new(scope: Scope, mode: Mode) -> Result<Self, String> {
        match mode {
            Mode::Poll { .. } | Mode::Snapshot | Mode::Count => Ok(Self { mode, scope }),
            other => Err(format!(
                "swap traffic is a counter that is read, not a stream of events: `{}` cannot be \
                 served from {}; poll it, count it over the window, or take one snapshot",
                other.kind(),
                scope.read_from()
            )),
        }
    }
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        match self.scope {
            Scope::Machine => match target {
                Target::System => Ok(Box::new(Swap::new(
                    self.mode,
                    Scope::Machine,
                    PathBuf::from(VMSTAT),
                    Vec::new(),
                ))),
                other => Err(format!(
                    "{VMSTAT} counts every swap device of the machine, and a {} target is not \
                     the machine: the same event for one cgroup is counted in its `memory.stat`, \
                     which is the `fs:cgroup` backend",
                    other.kind()
                )),
            },
            Scope::Cgroup => {
                let cgroup = Cgroup::resolve(target)?;
                let file = cgroup.file("memory.stat");
                let text = std::fs::read_to_string(&file)
                    .map_err(|error| format!("cannot read {}: {error}", file.display()))?;
                // Refused rather than answered with the stock: `memory.swap.current`
                // is a different measurement, and reporting it under this name
                // would make a cgroup that swapped a gigabyte and read it back
                // look like one that never swapped at all.
                if stat_field(&stat_fields(&text), "pswpout").is_none() {
                    return Err(format!(
                        "{} carries no `pswpin`/`pswpout`, so this kernel does not count swap \
                         traffic per cgroup; `mem.swap --system` reads the machine's own \
                         counters out of {VMSTAT}",
                        file.display()
                    ));
                }
                let keys = vec![cgroup.key()];
                Ok(Box::new(Swap::new(self.mode, Scope::Cgroup, file, keys)))
            }
        }
    }
}

/// Reads one pair of swap counters.
pub struct Swap {
    id: CollectorId,
    mode: Mode,
    scope: Scope,
    path: PathBuf,
    keys: Vec<KeyValue>,
    /// The level file that sits beside the counters, where there is one.
    stock: Option<PathBuf>,
    page_size: u64,
    previous: Option<Counters>,
    /// `count` only: the first read of the run, which the one event it
    /// reports is measured from.
    opened: Option<Counters>,
    losses: Losses,
}

impl Swap {
    pub fn new(mode: Mode, scope: Scope, path: PathBuf, keys: Vec<KeyValue>) -> Self {
        let stock = match scope {
            Scope::Cgroup => path.parent().map(|dir| dir.join("memory.swap.current")),
            // `/proc/vmstat` publishes no stock, and `/proc/meminfo`'s is the
            // machine's free swap rather than anyone's residency. Absent.
            Scope::Machine => None,
        };
        Self {
            id: CollectorId::new(format!("mem.swap/{}", scope.backend())),
            mode,
            scope,
            path,
            keys,
            stock,
            page_size: page_size(),
            previous: None,
            opened: None,
            losses: Losses::default(),
        }
    }

    fn read(&self) -> Result<Counters, SampleError> {
        let text = std::fs::read_to_string(&self.path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                SampleError::TargetGone(format!("{} is gone", self.path.display()))
            } else {
                SampleError::Transient(format!("{}: {error}", self.path.display()))
            }
        })?;
        let fields = stat_fields(&text);
        let (Some(in_pages), Some(out_pages)) = (
            stat_field(&fields, "pswpin"),
            stat_field(&fields, "pswpout"),
        ) else {
            return Err(SampleError::Transient(format!(
                "{} holds no `pswpin`/`pswpout` line",
                self.path.display()
            )));
        };
        Ok(Counters {
            ts_ns: monotonic_ns(),
            in_pages,
            out_pages,
            in_zero_pages: stat_field(&fields, "swpin_zero"),
            out_zero_pages: stat_field(&fields, "swpout_zero"),
        })
    }

    /// How much is on a swap device right now, where the kernel says.
    fn stock_bytes(&self) -> Option<u64> {
        let text = std::fs::read_to_string(self.stock.as_ref()?).ok()?;
        text.trim().parse().ok()
    }

    fn flow_event(&mut self, before: &Counters, after: &Counters, seq: u64) -> Option<Event> {
        let interval_ns = after.ts_ns.saturating_sub(before.ts_ns);
        let (Some(in_pages), Some(out_pages)) = (
            after.in_pages.checked_sub(before.in_pages),
            after.out_pages.checked_sub(before.out_pages),
        ) else {
            self.losses.undecoded += 1;
            self.losses.note(format!(
                "a swap counter in {} went backwards, so this interval cannot be attributed",
                self.path.display()
            ));
            return None;
        };

        let in_bytes = in_pages * self.page_size;
        let out_bytes = out_pages * self.page_size;
        let mut attrs = vec![
            Attr::uint("bytes", in_bytes + out_bytes),
            Attr::uint("in_bytes", in_bytes),
            Attr::uint("out_bytes", out_bytes),
            Attr::uint("interval_ns", interval_ns),
            Attr::text("read_from", self.scope.read_from()),
            Attr::text("basis", "the swap counters' increase over this tick"),
        ];
        for (name, after, before) in [
            ("in_zero_bytes", after.in_zero_pages, before.in_zero_pages),
            (
                "out_zero_bytes",
                after.out_zero_pages,
                before.out_zero_pages,
            ),
        ] {
            if let Some(pages) = after
                .zip(before)
                .and_then(|(after, before)| after.checked_sub(before))
            {
                attrs.push(Attr::uint(name, pages * self.page_size));
            }
        }
        if let Some(stock) = self.stock_bytes() {
            attrs.push(Attr::uint("swapped_bytes", stock));
        }
        Some(self.event(after.ts_ns, attrs, seq))
    }

    /// A single read observes no window, so it reports the counters themselves
    /// rather than a flow it did not measure.
    fn snapshot_event(&self, now: &Counters, seq: u64) -> Event {
        let mut attrs = vec![
            Attr::uint("in_bytes_since_boot", now.in_pages * self.page_size),
            Attr::uint("out_bytes_since_boot", now.out_pages * self.page_size),
            Attr::text("read_from", self.scope.read_from()),
            Attr::text(
                "basis",
                "the counters as they stand: a snapshot observes no window of its own",
            ),
        ];
        if let Some(stock) = self.stock_bytes() {
            attrs.push(Attr::uint("swapped_bytes", stock));
        }
        self.event(now.ts_ns, attrs, seq)
    }

    fn event(&self, ts_ns: u64, attrs: Vec<Attr>, seq: u64) -> Event {
        Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "mem.swap".to_owned(),
            keys: self.keys.clone(),
            attrs,
            collector_id: self.id.clone(),
            seq,
        }
    }
}

impl Collector for Swap {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    /// A poll measures traffic over its own window; a snapshot can only report
    /// what the counter stands at. Different numbers, different names.
    fn value_attr(&self) -> &'static str {
        match self.mode {
            Mode::Snapshot => "out_bytes_since_boot",
            _ => "bytes",
        }
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let now = self.read()?;
        if self.mode == Mode::Snapshot {
            return Ok(vec![self.snapshot_event(&now, seq)]);
        }
        if self.mode == Mode::Count {
            // The window's two ends and nothing between: the first read opens
            // it, every later one moves its close, and `stop` reports the one
            // flow over all of it.
            self.opened.get_or_insert(now);
            self.previous = Some(now);
            return Ok(Vec::new());
        }
        let before = self.previous.replace(now);
        let event = before.and_then(|before| self.flow_event(&before, &now, seq));
        Ok(event.into_iter().collect())
    }

    /// Reads nothing: the runner's last round of `sample` calls lands when the
    /// stop condition is met, so the window is already closed by the read it
    /// took. A counted run reports its one event here, from the first read to
    /// that last one, carrying the window it covers.
    fn stop(&mut self) -> Vec<Event> {
        if self.mode != Mode::Count {
            return Vec::new();
        }
        let (Some(opened), Some(closed)) = (self.opened, self.previous) else {
            return Vec::new();
        };
        if closed.ts_ns == opened.ts_ns {
            self.losses.undecoded += 1;
            self.losses.note(format!(
                "{} was read once, so a counted run has no window to report a flow over",
                self.path.display()
            ));
            return Vec::new();
        }
        self.flow_event(&opened, &closed, u64::MAX / 2)
            .map(|mut event| {
                for attr in &mut event.attrs {
                    if attr.name == "basis" {
                        *attr = Attr::text("basis", "the swap counters' increase over the window");
                    }
                }
                event
            })
            .into_iter()
            .collect()
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }
}

fn page_size() -> u64 {
    // SAFETY: `sysconf` takes an integer and returns one.
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
    use crate::collect::mem::testing::Scratch;
    use std::time::Duration;

    /// The limit that pushes a child's written pages out to swap.
    ///
    /// `memory.high`, not `memory.max`. Both make the kernel reclaim from the
    /// cgroup as it grows, but `max` calls the OOM killer when reclaim falls
    /// behind, and reclaim of a written page waits for the swap device. On
    /// the CI's `defaults` cell -- a swap file on a cloud disk, with 320 MiB
    /// free and more, so past every swap check above -- `max` killed both
    /// children that wrote their pages (run 37155975223: `memory.events`
    /// *"max 387 oom 1 oom_kill 1"* and *"max 154 oom 1 oom_kill 1"*), while
    /// the zero-filled child of the same cell, whose pages need no write,
    /// passed under the same `max`. `pressure.rs` met the same kill first and
    /// moved to `high` for it. Over `high`, a child that outruns reclaim is
    /// throttled instead, and [`THROTTLED_FOR`] bounds how long it is waited
    /// for.
    const PUSHED_BY: &str = "memory.high";

    /// How long a child throttled over [`PUSHED_BY`] is given to finish
    /// touching; what it touched by then is what a test judges.
    const THROTTLED_FOR: Duration = Duration::from_secs(60);

    fn poll() -> Mode {
        Mode::Poll {
            interval: Mode::DEFAULT_POLL_INTERVAL,
        }
    }

    fn scratch_file(name: &str, in_pages: u64, out_pages: u64) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "omn-mem-swap-{}-{}-{name}",
            std::process::id(),
            monotonic_ns()
        ));
        write_counters(&path, in_pages, out_pages);
        path
    }

    fn write_counters(path: &std::path::Path, in_pages: u64, out_pages: u64) {
        std::fs::write(
            path,
            format!("nr_free_pages 1234\npswpin {in_pages}\npswpout {out_pages}\n"),
        )
        .expect("writing the fake vmstat");
    }

    /// A kernel that does not separate zero-filled pages publishes no such
    /// field, and the collector must then report none rather than zero.
    #[test]
    fn zero_filled_pages_are_absent_where_the_kernel_does_not_count_them() {
        let path = scratch_file("no-zero", 1, 1);
        let mut collector = Swap::new(poll(), Scope::Machine, path.clone(), Vec::new());
        collector.sample(0).expect("the baseline read");
        write_counters(&path, 2, 3);
        let events = collector.sample(1).expect("a second read");
        assert!(
            events[0].attr("out_zero_bytes").is_none(),
            "{:?}",
            events[0]
        );
        std::fs::remove_file(&path).ok();
    }

    /// The counters are in pages and the source's unit is bytes. Getting that
    /// conversion wrong is a number that is wrong by a factor of the page size
    /// and looks entirely plausible.
    #[test]
    fn pages_become_bytes_and_the_two_directions_stay_apart() {
        let path = scratch_file("flow", 10, 20);
        let mut collector = Swap::new(poll(), Scope::Machine, path.clone(), Vec::new());
        assert!(
            collector.sample(0).expect("the first read").is_empty(),
            "the first read is a baseline"
        );
        write_counters(&path, 13, 120);
        let events = collector.sample(1).expect("a second read");

        let page = page_size();
        let event = &events[0];
        assert_eq!(
            event.attr("in_bytes").unwrap().as_f64(),
            Some(3.0 * page as f64)
        );
        assert_eq!(
            event.attr("out_bytes").unwrap().as_f64(),
            Some(100.0 * page as f64)
        );
        assert_eq!(
            event.attr("bytes").unwrap().as_f64(),
            Some(103.0 * page as f64)
        );
        std::fs::remove_file(&path).ok();
    }

    /// A counted run is one event over the whole window: the first read to
    /// the last, however many reads there were between, and it carries the
    /// window's length -- which is what `rate` divides by.
    #[test]
    fn a_counted_run_reports_one_flow_over_its_whole_window() {
        let path = scratch_file("count", 10, 20);
        let mut collector = Swap::new(Mode::Count, Scope::Machine, path.clone(), Vec::new());
        assert!(collector.sample(0).expect("the opening read").is_empty());
        write_counters(&path, 11, 25);
        assert!(collector.sample(1).expect("a read inside").is_empty());
        write_counters(&path, 14, 70);
        std::thread::sleep(Duration::from_millis(5));
        assert!(collector.sample(2).expect("the closing read").is_empty());
        let events = collector.stop();
        assert_eq!(events.len(), 1, "one window, one event");

        let page = page_size() as f64;
        let event = &events[0];
        assert_eq!(event.attr("in_bytes").unwrap().as_f64(), Some(4.0 * page));
        assert_eq!(event.attr("out_bytes").unwrap().as_f64(), Some(50.0 * page));
        assert!(event.attr("interval_ns").unwrap().as_f64().unwrap() >= 5e6);
        assert!(event
            .attr("basis")
            .unwrap()
            .to_string()
            .contains("over the window"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_counter_that_goes_backwards_is_a_counted_loss() {
        let path = scratch_file("backwards", 100, 100);
        let mut collector = Swap::new(poll(), Scope::Machine, path.clone(), Vec::new());
        collector.sample(0).expect("the baseline read");
        write_counters(&path, 1, 1);
        assert!(collector.sample(1).expect("a read").is_empty());
        assert_eq!(collector.losses().undecoded, 1);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_snapshot_reports_the_counters_rather_than_a_flow_it_did_not_measure() {
        let path = scratch_file("snapshot", 4, 8);
        let mut collector = Swap::new(Mode::Snapshot, Scope::Machine, path.clone(), Vec::new());
        let events = collector.sample(0).expect("one read");
        assert_eq!(collector.value_attr(), "out_bytes_since_boot");
        assert!(events[0].attr("bytes").is_none());
        assert_eq!(
            events[0].attr("out_bytes_since_boot").unwrap().as_f64(),
            Some(8.0 * page_size() as f64)
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_per_process_target_is_refused_rather_than_answered_machine_wide() {
        let refused = Box::new(Prepare::new(Scope::Machine, poll()).expect("poll is servable"))
            .attach(&Target::Pid(vec![1]))
            .err()
            .expect("vmstat counts the machine");
        assert!(refused.contains("fs:cgroup"), "{refused}");
    }

    /// Ground truth, against two other kernel accountings of the same event.
    ///
    /// A cgroup is limited to far less than a process inside it is about to
    /// touch, and swap is where the difference has to go. Three numbers are
    /// then compared, and none of them is this collector's own arithmetic:
    ///
    /// - the flow this collector reports has to be positive, and
    /// - the stock the kernel publishes in `memory.swap.current` has to be
    ///   positive too, and
    /// - `/proc/<pid>/smaps_rollup` has to agree, from the process's side,
    ///   that roughly that much of *it* is on a swap device.
    ///
    /// The last is the strongest of the three: it is the same fact counted by a
    /// different subsystem, through a different file, keyed on the process
    /// rather than on the cgroup.
    ///
    /// The limit is `memory.high`, not `memory.max` (see [`PUSHED_BY`]).
    #[test]
    fn memory_pushed_out_of_a_limited_cgroup_is_counted_as_swap() {
        const LIMIT: u64 = 32 * 1024 * 1024;
        const TOUCHED: u64 = 192 * 1024 * 1024;

        let Some(scratch) = Scratch::cgroup("swap") else {
            eprintln!("skipped: this user cannot create a cgroup under its own slice");
            return;
        };
        if !scratch.path().join("memory.swap.current").exists() {
            eprintln!("skipped: this kernel accounts no swap per cgroup");
            return;
        }
        if std::fs::read_to_string("/proc/swaps")
            .map(|text| text.lines().count() <= 1)
            .unwrap_or(true)
        {
            eprintln!("skipped: this machine has no swap device, so nothing can be paged out");
            return;
        }

        let mut collector = Swap::new(
            poll(),
            Scope::Cgroup,
            scratch.path().join("memory.stat"),
            Vec::new(),
        );
        if collector.read().is_err() {
            eprintln!("skipped: this kernel counts no swap traffic per cgroup");
            return;
        }

        // The same file counted over the same window, as `--mode count` reads
        // it: one event at the end, from the first read to the last.
        let mut counted = Swap::new(
            Mode::Count,
            Scope::Cgroup,
            scratch.path().join("memory.stat"),
            Vec::new(),
        );

        if let Some(reason) = scratch.no_swap_room(2 * TOUCHED.saturating_sub(LIMIT)) {
            eprintln!("skipped: {reason}");
            return;
        }
        assert!(scratch.write(PUSHED_BY, &LIMIT.to_string()));
        counted.sample(0).expect("the counted run's opening read");
        collector.sample(0).expect("the baseline read");
        let mut child = scratch.spawn_throttled_toucher(TOUCHED, THROTTLED_FOR);
        let finished = child.finished_touching();
        // Read before the counters, so that it is a floor on what the child
        // had written by the time they were read.
        let written = child.bytes_written();

        // The swap-out happens while the child faults; the counter is read
        // after it has finished touching, so the window contains all of it.
        let events = collector.sample(1).expect("a second read");
        counted.sample(1).expect("the counted run's closing read");
        let counted_out = counted
            .stop()
            .first()
            .and_then(|event| event.attr("out_bytes").and_then(|value| value.as_f64()))
            .expect("a counted run reports its window");
        let event = events.first().expect("a second read produces a point");
        let out_bytes = event.attr("out_bytes").unwrap().as_f64().unwrap();
        let stock = event
            .attr("swapped_bytes")
            .and_then(|value| value.as_f64())
            .expect("memory.swap.current sits beside memory.stat");

        // The process's own view of the same fact, before it is killed.
        let rollup = std::fs::read_to_string(format!("/proc/{}/smaps_rollup", child.pid()))
            .unwrap_or_default();
        child.stop();

        // What had to leave the cgroup: everything past the limit of what the
        // child wrote inside it, which is all of it once the child finished,
        // and otherwise what the child itself says it had written. Not its
        // `smaps_rollup`: that also holds the pages it inherited from this
        // test binary, which are charged elsewhere.
        let touched = if finished { TOUCHED } else { written };
        if touched < 2 * LIMIT {
            eprintln!(
                "skipped: the child wrote {touched} of its {TOUCHED} bytes in {THROTTLED_FOR:?} \
                 over a {LIMIT}-byte {PUSHED_BY}, throttled by reclaim, which is too little \
                 past the limit to judge a swap-out by"
            );
            return;
        }
        if !finished {
            eprintln!(
                "the child was still writing its {TOUCHED} bytes after {THROTTLED_FOR:?} over a \
                 {LIMIT}-byte {PUSHED_BY}, throttled by reclaim; it had written {touched}, and \
                 that is what is judged"
            );
        }
        let expected = touched.saturating_sub(LIMIT) as f64;
        assert!(
            out_bytes >= 0.5 * expected,
            "a cgroup limited to {LIMIT} bytes with {touched} touched inside it has to page out \
             about {expected} bytes; this run counted {out_bytes}"
        );
        assert!(
            stock > 0.0,
            "memory.swap.current is zero although {out_bytes} bytes were paged out"
        );
        // The counted window contains the polled one -- it opened before and
        // closed after -- so it holds at least as much, and the workload's
        // floor with it.
        assert!(
            counted_out >= out_bytes && counted_out >= 0.5 * expected,
            "the counted window paged out {counted_out} bytes, the poll inside it {out_bytes}, \
             and the workload had to push out about {expected}"
        );

        let process_swap = rollup
            .lines()
            .find_map(|line| line.strip_prefix("Swap:"))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse::<f64>().ok())
            .map(|kilobytes| kilobytes * 1024.0);
        if let Some(process_swap) = process_swap {
            let ratio = process_swap / stock;
            assert!(
                (0.5..2.0).contains(&ratio),
                "the process says {process_swap} bytes of it are swapped and the cgroup says \
                 {stock}"
            );
        }
    }

    /// Ground truth for the difference between what moved and what is out.
    ///
    /// Two children, one limited cgroup each, the same size touched and the
    /// same reclaim forced. The only difference is what they wrote: one fills
    /// its pages with a byte, the other leaves them full of zeros. The kernel
    /// never writes a zero-filled page to a swap device -- it records that the
    /// page was zero -- so the second child grows `memory.swap.current` by as
    /// much as the first and produces almost no swap traffic at all.
    ///
    /// That is the whole reason this source reports the flow and carries the
    /// stock beside it under another name. It also fixes a test: a first
    /// version of the swap ground truth allocated a Python `bytearray`, which
    /// is zeros, and would have measured a collector reading the wrong field as
    /// correct.
    #[test]
    fn a_page_of_zeros_is_swapped_without_being_written() {
        const LIMIT: u64 = 32 * 1024 * 1024;
        const TOUCHED: u64 = 192 * 1024 * 1024;

        let Some(scratch) = Scratch::cgroup("swap-zero") else {
            eprintln!("skipped: this user cannot create a cgroup under its own slice");
            return;
        };
        if !scratch.path().join("memory.swap.current").exists() {
            eprintln!("skipped: this kernel accounts no swap per cgroup");
            return;
        }
        if std::fs::read_to_string("/proc/swaps")
            .map(|text| text.lines().count() <= 1)
            .unwrap_or(true)
        {
            eprintln!("skipped: this machine has no swap device, so nothing can be paged out");
            return;
        }

        let mut collector = Swap::new(
            poll(),
            Scope::Cgroup,
            scratch.path().join("memory.stat"),
            Vec::new(),
        );
        let Ok(baseline) = collector.read() else {
            eprintln!("skipped: this kernel counts no swap traffic per cgroup");
            return;
        };
        if baseline.out_zero_pages.is_none() {
            eprintln!("skipped: this kernel does not count zero-filled pages apart");
            return;
        }

        if let Some(reason) = scratch.no_swap_room(2 * TOUCHED.saturating_sub(LIMIT)) {
            eprintln!("skipped: {reason}");
            return;
        }
        assert!(scratch.write("memory.max", &LIMIT.to_string()));
        let vmstat_before = zero_and_huge_page_counters();
        collector.sample(0).expect("the baseline read");
        let mut child = scratch.spawn_zero_toucher(TOUCHED);
        let events = collector.sample(1).expect("a second read");
        let stock = std::fs::read_to_string(scratch.path().join("memory.swap.current"))
            .ok()
            .and_then(|text| text.trim().parse::<f64>().ok())
            .expect("the stock the kernel publishes");
        // What the kernel says happened to the pages, for a failure to be read
        // against rather than guessed at: the CI's `defaults` cell once held
        // 22 MiB on swap of the 160 MiB that had left the cgroup.
        let resident =
            std::fs::read_to_string(scratch.path().join("memory.current")).unwrap_or_default();
        let moved: Vec<String> = zero_and_huge_page_counters()
            .into_iter()
            .map(|(name, after)| {
                let before = vmstat_before
                    .iter()
                    .find(|(seen, _)| *seen == name)
                    .map_or(0, |(_, value)| *value);
                format!("{name} +{}", after.saturating_sub(before))
            })
            .collect();
        let said = format!(
            "memory.current {}, /proc/vmstat over the run: {}",
            resident.trim(),
            moved.join(", ")
        );
        child.stop();

        let event = events.first().expect("a second read produces a point");
        let written = event.attr("out_bytes").unwrap().as_f64().unwrap();
        let zeroed = event
            .attr("out_zero_bytes")
            .and_then(|value| value.as_f64())
            .expect("the kernel separates zero-filled pages on this machine");
        let expected = TOUCHED.saturating_sub(LIMIT) as f64;

        assert!(
            stock >= 0.5 * expected,
            "a cgroup limited to {LIMIT} with {TOUCHED} touched inside it holds about \
             {expected} bytes on a swap device; memory.swap.current said {stock} ({said})"
        );
        assert!(
            zeroed >= 0.5 * expected,
            "those pages were full of zeros, so the kernel should have counted about \
             {expected} bytes as swapped-without-writing; it counted {zeroed}"
        );
        assert!(
            written < 0.25 * zeroed,
            "nothing was written to a device, and yet {written} bytes of traffic were reported \
             beside {zeroed} bytes of zero pages"
        );
    }

    /// The machine's counters of what became of a page on its way out: zero
    /// pages recorded instead of written, huge pages split or swapped whole.
    /// The names this kernel publishes, with their values; absent ones are
    /// left out.
    fn zero_and_huge_page_counters() -> Vec<(String, u64)> {
        const NAMES: [&str; 6] = [
            "pswpout",
            "swpout_zero",
            "thp_split_page",
            "thp_underused_split_page",
            "thp_swpout",
            "thp_swpout_fallback",
        ];
        std::fs::read_to_string(VMSTAT)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| {
                let (name, value) = line.split_once(' ')?;
                if !NAMES.contains(&name) {
                    return None;
                }
                Some((name.to_owned(), value.trim().parse().ok()?))
            })
            .collect()
    }

    /// The machine counted at least what the cgroup did.
    ///
    /// Two independent counters over one window, one of which is a strict
    /// subset of the other: whatever the limited cgroup pushed out is part of
    /// what `/proc/vmstat` saw, and the rest of the machine can only add to it.
    /// A backend that read the wrong field, or scaled pages by the wrong
    /// number, breaks this inequality immediately. The limit is
    /// [`PUSHED_BY`], for the reason given there.
    #[test]
    fn the_machines_counter_sees_at_least_what_one_cgroup_did() {
        const LIMIT: u64 = 32 * 1024 * 1024;
        const TOUCHED: u64 = 192 * 1024 * 1024;

        let Some(scratch) = Scratch::cgroup("swap-system") else {
            eprintln!("skipped: this user cannot create a cgroup under its own slice");
            return;
        };
        if std::fs::read_to_string("/proc/swaps")
            .map(|text| text.lines().count() <= 1)
            .unwrap_or(true)
        {
            eprintln!("skipped: this machine has no swap device, so nothing can be paged out");
            return;
        }

        let mut machine = Swap::new(poll(), Scope::Machine, PathBuf::from(VMSTAT), Vec::new());
        let mut cgroup = Swap::new(
            poll(),
            Scope::Cgroup,
            scratch.path().join("memory.stat"),
            Vec::new(),
        );
        if cgroup.read().is_err() {
            eprintln!("skipped: this kernel counts no swap traffic per cgroup");
            return;
        }

        if let Some(reason) = scratch.no_swap_room(2 * TOUCHED.saturating_sub(LIMIT)) {
            eprintln!("skipped: {reason}");
            return;
        }
        assert!(scratch.write(PUSHED_BY, &LIMIT.to_string()));
        machine.sample(0).expect("vmstat reads");
        cgroup.sample(0).expect("memory.stat reads");
        let mut child = scratch.spawn_throttled_toucher(TOUCHED, THROTTLED_FOR);
        std::thread::sleep(Duration::from_millis(50));
        let by_cgroup = cgroup.sample(1).expect("a second read");
        let by_machine = machine.sample(1).expect("a second read");
        child.stop();

        let out = |events: &[Event]| {
            events
                .first()
                .and_then(|event| event.attr("out_bytes"))
                .and_then(|value| value.as_f64())
                .unwrap_or(0.0)
        };
        let (charged, machine_wide) = (out(&by_cgroup), out(&by_machine));
        assert!(
            charged > 0.0,
            "the cgroup was limited to {LIMIT} with {TOUCHED} touched inside it and paged \
             nothing out"
        );
        assert!(
            machine_wide >= charged,
            "the machine counted {machine_wide} bytes paged out while one cgroup inside it \
             counted {charged}"
        );
    }
}
