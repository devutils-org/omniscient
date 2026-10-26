//! `net.backlog` over `/proc/net/netstat`.
//!
//! Two counters that answer one question: is this machine refusing connections
//! it was offered? `ListenOverflows` counts accepts refused because the
//! application was not keeping up with its accept queue, and `ListenDrops`
//! counts every SYN dropped for any reason at all.
//!
//! They are reported separately because they mean different things. An overflow
//! is an application too slow to accept; a drop without an overflow is usually
//! the SYN queue or a cookie, which is a different fix.

use super::procfs::{self, Deltas};
use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event};
use crate::request::{Mode, Target};

const OVERFLOWS: &str = "TcpExt:ListenOverflows";
const DROPS: &str = "TcpExt:ListenDrops";
const SYNCOOKIES_SENT: &str = "TcpExt:SyncookiesSent";
const SYNCOOKIES_FAILED: &str = "TcpExt:SyncookiesFailed";

pub struct Prepare(pub Mode);

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        match target {
            Target::System => Ok(Box::new(BacklogCollector {
                mode: self.0,
                deltas: Deltas::default(),
                pass_first_tick: false,
                losses: Losses::default(),
            })),
            other => Err(format!(
                "the accept queue counters are machine-wide, and a {} target asks for a share \
                 of them that the kernel does not keep; use --system",
                other.kind()
            )),
        }
    }
}

pub struct BacklogCollector {
    /// A poll and a count measure an interval; a snapshot cannot, and reports
    /// the counters as they stand instead.
    mode: Mode,
    deltas: Deltas,
    /// The first tick after the baseline taken at `start`, passed over: its
    /// interval is the microseconds since the program's release.
    pass_first_tick: bool,
    losses: Losses,
}

impl Collector for BacklogCollector {
    /// A poll and a count measure refusals over a window; a snapshot can only
    /// report what the counters stand at. Different numbers, different names.
    fn value_attr(&self) -> &'static str {
        match self.mode {
            Mode::Snapshot => "drops_since_boot",
            _ => "drops",
        }
    }

    /// The baseline, before the observed program is released, so its first
    /// frames are in the first interval; the runner's first tick, which
    /// follows the release by microseconds, is then passed over.
    fn start(&mut self) -> Result<(), String> {
        if self.mode == Mode::Snapshot {
            return Ok(());
        }
        match self.sample(0) {
            Ok(_) => {}
            // What is not there yet is the first tick's to say.
            Err(SampleError::TargetGone(_)) => return Ok(()),
            Err(error) => return Err(format!("the baseline read failed: {error:?}")),
        }
        self.pass_first_tick = self.mode != Mode::Count;
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        if std::mem::take(&mut self.pass_first_tick) {
            return Ok(Vec::new());
        }
        let counters = procfs::named_counters(procfs::NETSTAT)?;
        let ts_ns = monotonic_ns();

        // A snapshot observes no window of its own: the increment between its
        // one read and the run's close spans a few hundred microseconds, which
        // on anything short of a saturated machine is zero however hard the
        // accept queue is being hammered. The totals since boot are the number
        // a single read can honestly report, and they carry their own names so
        // that nobody reads them as this run's.
        if self.mode == Mode::Snapshot {
            let standing = |name: &str| counters.get(name).copied().unwrap_or(0);
            return Ok(vec![Event {
                ts_ns,
                ts_err_ns: 0,
                kind: "net.backlog".to_owned(),
                keys: Vec::new(),
                attrs: vec![
                    Attr::uint("drops_since_boot", standing(DROPS)),
                    Attr::uint("overflows_since_boot", standing(OVERFLOWS)),
                    Attr::uint("syncookies_sent_since_boot", standing(SYNCOOKIES_SENT)),
                    Attr::uint("syncookies_failed_since_boot", standing(SYNCOOKIES_FAILED)),
                    Attr::text(
                        "basis",
                        "the counters as they stand: a snapshot observes no window of its own",
                    ),
                ],
                collector_id: CollectorId::unstamped(),
                seq,
            }]);
        }

        let mut read = |name: &str| {
            counters
                .get(name)
                .and_then(|current| self.deltas.since(name, *current, &mut self.losses))
        };

        // The first read establishes every baseline at once, so a missing
        // increment on the first counter means the whole tick is a baseline.
        let Some(drops) = read(DROPS) else {
            let _ = read(OVERFLOWS);
            let _ = read(SYNCOOKIES_SENT);
            let _ = read(SYNCOOKIES_FAILED);
            return Ok(Vec::new());
        };
        let overflows = read(OVERFLOWS).unwrap_or(0);
        let syncookies_sent = read(SYNCOOKIES_SENT).unwrap_or(0);
        let syncookies_failed = read(SYNCOOKIES_FAILED).unwrap_or(0);

        Ok(vec![Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "net.backlog".to_owned(),
            keys: Vec::new(),
            attrs: vec![
                Attr::uint("drops", drops),
                // Kept apart from drops: an overflow is an application too slow
                // to accept, a drop without one is usually the SYN queue.
                Attr::uint("overflows", overflows),
                Attr::uint("syncookies_sent", syncookies_sent),
                Attr::uint("syncookies_failed", syncookies_failed),
            ],
            collector_id: CollectorId::unstamped(),
            seq,
        }])
    }

    /// A count's closing read. A poll's last interval is the runner's last
    /// tick (`docs/DECISIONS.md`, "the runner closes the window"): a read here
    /// covered the microseconds since and put a zero point at the previous
    /// one's offset into every series. A snapshot has no window to close.
    fn stop(&mut self) -> Vec<Event> {
        if self.mode != Mode::Count {
            return Vec::new();
        }
        self.sample(u64::MAX / 2).unwrap_or_default()
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn polling() -> Mode {
        Mode::Poll {
            interval: Mode::DEFAULT_POLL_INTERVAL,
        }
    }

    /// The baseline is read at `start`; the runner's first tick passes
    /// over, the second measures, and a poll reads nothing at `stop`, whose
    /// interval would be the microseconds since the last tick.
    #[test]
    fn a_poll_measures_from_the_start_and_reads_nothing_at_the_close() {
        let mut collector = Box::new(Prepare(polling()))
            .attach(&Target::System)
            .expect("readable");
        collector.start().expect("the baseline");
        assert!(
            collector.sample(0).unwrap().is_empty(),
            "the first tick passes over"
        );
        assert!(
            !collector.sample(1).unwrap().is_empty(),
            "the second measures"
        );
        assert!(collector.stop().is_empty());
    }

    #[test]
    fn the_first_read_is_a_baseline_and_the_second_measures() {
        let mut collector = Box::new(Prepare(polling()))
            .attach(&Target::System)
            .expect("system");
        assert!(collector.sample(0).unwrap().is_empty());
        let events = collector.sample(0).unwrap();
        assert_eq!(events.len(), 1, "one machine, one series");
        // An idle machine refuses nothing.
        assert_eq!(
            events[0].attr("drops").and_then(|value| value.as_f64()),
            Some(0.0)
        );
    }

    #[test]
    fn overflows_and_drops_stay_separate() {
        let mut collector = Box::new(Prepare(polling()))
            .attach(&Target::System)
            .expect("system");
        collector.sample(0).unwrap();
        let events = collector.sample(0).unwrap();
        assert!(events[0].attr("overflows").is_some());
        assert!(events[0].attr("drops").is_some());
    }

    #[test]
    fn a_process_target_is_refused_because_the_kernel_keeps_no_such_share() {
        let Err(error) = Box::new(Prepare(polling())).attach(&Target::Pid(vec![1])) else {
            panic!("the accept queue counters are machine-wide");
        };
        assert!(error.contains("--system"), "{error}");
    }

    /// A snapshot of the accept queue reports the counters, not an increment
    /// over the microseconds between its read and the run's close.
    ///
    /// The ground truth is `/proc/net/netstat` itself, read independently: a
    /// snapshot has to answer with what is in the file, and the old behaviour
    /// answered with the difference between two reads a few hundred
    /// microseconds apart, which was structurally zero.
    #[test]
    fn a_snapshot_reports_the_counters_as_they_stand() {
        let file = procfs::named_counters(procfs::NETSTAT).expect("/proc/net/netstat");
        let mut collector = Box::new(Prepare(Mode::Snapshot))
            .attach(&Target::System)
            .expect("system");
        let events = collector.sample(0).expect("one read");
        assert_eq!(events.len(), 1, "one machine, one reading");
        assert_eq!(
            events[0]
                .attr("drops_since_boot")
                .and_then(|value| value.as_f64()),
            Some(file.get(DROPS).copied().unwrap_or(0) as f64),
            "a snapshot has to answer with the counter the kernel holds"
        );
        assert!(
            events[0].attr("drops").is_none(),
            "an interval's name on a number that is not one would be the lie this fixes"
        );
        assert!(
            collector.stop().is_empty(),
            "a snapshot has no window to close, and a closing read would replace its answer"
        );
    }
}
