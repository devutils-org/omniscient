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
use crate::request::Target;

const OVERFLOWS: &str = "TcpExt:ListenOverflows";
const DROPS: &str = "TcpExt:ListenDrops";
const SYNCOOKIES_SENT: &str = "TcpExt:SyncookiesSent";
const SYNCOOKIES_FAILED: &str = "TcpExt:SyncookiesFailed";

pub struct Prepare;

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        match target {
            Target::System => Ok(Box::new(BacklogCollector {
                id: CollectorId::new("net.backlog/fs:proc"),
                deltas: Deltas::default(),
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
    id: CollectorId,
    deltas: Deltas,
    losses: Losses,
}

impl Collector for BacklogCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "drops"
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let counters = procfs::named_counters(procfs::NETSTAT)?;
        let ts_ns = monotonic_ns();

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
            collector_id: self.id.clone(),
            seq,
        }])
    }

    fn stop(&mut self) -> Vec<Event> {
        self.sample(u64::MAX / 2).unwrap_or_default()
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_read_is_a_baseline_and_the_second_measures() {
        let mut collector = Box::new(Prepare).attach(&Target::System).expect("system");
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
        let mut collector = Box::new(Prepare).attach(&Target::System).expect("system");
        collector.sample(0).unwrap();
        let events = collector.sample(0).unwrap();
        assert!(events[0].attr("overflows").is_some());
        assert!(events[0].attr("drops").is_some());
    }

    #[test]
    fn a_process_target_is_refused_because_the_kernel_keeps_no_such_share() {
        let Err(error) = Box::new(Prepare).attach(&Target::Pid(vec![1])) else {
            panic!("the accept queue counters are machine-wide");
        };
        assert!(error.contains("--system"), "{error}");
    }
}
