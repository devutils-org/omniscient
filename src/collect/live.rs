//! A run's events, handed out while it goes.
//!
//! The runner keeps every event it reads in [`Progress::events`] and shows them
//! to its [`Escalate`] after each round. Something that wants the events as
//! they arrive -- a live view, a program reacting to what a process does -- is
//! an escalation that attaches nothing: it takes what is new each round and
//! lets the run go on. [`Handover`] is the part that knows what is new;
//! [`Live`] is the whole escalation, around a closure.
//!
//! Nothing here is a second collection. The events handed out are the ones the
//! encoders read at the end, and a source that drains a ring hands them out on
//! its drain tick, not as they happen.

use std::time::{Duration, Instant};

use super::{Escalate, Progress, Standing, Switch};
use crate::event::Event;

/// What each request has handed out so far, so that each round hands out
/// only what arrived since.
#[derive(Debug, Clone, Default)]
pub struct Handover {
    sent: Vec<usize>,
}

impl Handover {
    pub fn new(requests: usize) -> Self {
        Self {
            sent: vec![0; requests],
        }
    }

    /// The events of each request that arrived since the last call, as pairs
    /// of the request's index and its new events, in arrival order. A request
    /// with nothing new is left out.
    pub fn fresh<'a>(&mut self, run: &Progress<'a>) -> Vec<(usize, &'a [Event])> {
        if self.sent.len() < run.events.len() {
            self.sent.resize(run.events.len(), 0);
        }
        let mut fresh = Vec::new();
        for (index, events) in run.events.iter().enumerate() {
            let sent = &mut self.sent[index];
            // A buffer only grows while the run goes; one that shrank was let
            // go by the runner, and what is in it now is all new.
            if events.len() < *sent {
                *sent = 0;
            }
            if events.len() > *sent {
                fresh.push((index, &events[*sent..]));
                *sent = events.len();
            }
        }
        fresh
    }

    /// Whether any event was handed out yet.
    pub fn any(&self) -> bool {
        self.sent.iter().any(|sent| *sent > 0)
    }
}

/// Detaches every request still attached, each with `why`: what ends a run
/// from the escalation's side.
pub fn detach_all(run: &Progress<'_>, why: &str) -> Vec<Switch> {
    run.standing
        .iter()
        .enumerate()
        .filter(|(_, standing)| matches!(standing, Standing::Attached))
        .map(|(index, _)| Switch {
            index,
            attach: false,
            why: why.to_owned(),
        })
        .collect()
}

/// What the closure of a [`Live`] wants after a batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Flow {
    Continue,
    /// End the run: every request is detached, with this reason.
    Stop(String),
}

/// An escalation that attaches nothing and hands each round's new events to a
/// closure, as `(request index, events, now_ns)`.
///
/// Run it with [`run_all_escalating`](super::run_all_escalating). The closure
/// runs on the runner's thread, between two rounds: what it spends is time the
/// rings are not drained, so a slow consumer sends the events to a channel
/// rather than working on them here.
pub struct Live<F> {
    handover: Handover,
    every: Duration,
    last: Option<Instant>,
    on_events: F,
    stopping: Option<String>,
}

impl<F> Live<F>
where
    F: FnMut(usize, &[Event], u64) -> Flow,
{
    /// Hands out at most once every `every`; `Duration::ZERO` hands out every
    /// round.
    pub fn new(requests: usize, every: Duration, on_events: F) -> Self {
        Self {
            handover: Handover::new(requests),
            every,
            last: None,
            on_events,
            stopping: None,
        }
    }
}

impl<F> Escalate for Live<F>
where
    F: FnMut(usize, &[Event], u64) -> Flow,
{
    fn decide(&mut self, run: &Progress<'_>) -> Vec<Switch> {
        if let Some(why) = &self.stopping {
            return detach_all(run, why);
        }
        if self.last.is_some_and(|last| last.elapsed() < self.every) {
            return Vec::new();
        }
        self.last = Some(Instant::now());
        for (index, events) in self.handover.fresh(run) {
            if let Flow::Stop(why) = (self.on_events)(index, events, run.now_ns) {
                let switches = detach_all(run, &why);
                self.stopping = Some(why);
                return switches;
            }
        }
        Vec::new()
    }

    fn shed(&mut self, _index: usize, _over: &str) -> Option<String> {
        None
    }

    fn close(&mut self, _run: &Progress<'_>) -> Vec<(usize, String)> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::CollectorId;

    fn event(seq: u64) -> Event {
        Event {
            ts_ns: seq,
            ts_err_ns: 0,
            kind: "proc.exec".to_owned(),
            keys: Vec::new(),
            attrs: Vec::new(),
            collector_id: CollectorId::new("test"),
            seq,
        }
    }

    fn progress<'a>(events: &'a [Vec<Event>], standing: &'a [Standing]) -> Progress<'a> {
        Progress {
            opened_ns: 0,
            now_ns: 1,
            events,
            standing,
            bound: &[],
        }
    }

    /// Each round hands out what arrived since the one before, per request,
    /// and a buffer the runner let go is read again from its start.
    #[test]
    fn a_handover_gives_each_event_once() {
        let standing = [Standing::Attached, Standing::Attached];
        let mut handover = Handover::new(2);
        let first = [vec![event(0), event(1)], Vec::new()];
        let fresh = handover.fresh(&progress(&first, &standing));
        assert_eq!(fresh.len(), 1);
        assert_eq!((fresh[0].0, fresh[0].1.len()), (0, 2));
        assert!(handover.any());

        let second = [vec![event(0), event(1), event(2)], vec![event(0)]];
        let fresh = handover.fresh(&progress(&second, &standing));
        let seen: Vec<(usize, Vec<u64>)> = fresh
            .iter()
            .map(|(index, events)| (*index, events.iter().map(|e| e.seq).collect()))
            .collect();
        assert_eq!(seen, vec![(0, vec![2]), (1, vec![0])]);

        assert!(handover.fresh(&progress(&second, &standing)).is_empty());

        let evicted = [vec![event(7)], vec![event(0)]];
        let fresh = handover.fresh(&progress(&evicted, &standing));
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].1[0].seq, 7);
    }

    /// A closure that says stop ends the run: what is attached is detached
    /// with its reason that round, and again on any round after.
    #[test]
    fn a_live_run_stops_when_its_closure_says_so() {
        let standing = [Standing::Attached, Standing::Waiting];
        let mut seen = 0;
        let mut live = Live::new(2, Duration::ZERO, |_, events: &[Event], _| {
            seen += events.len();
            Flow::Stop("enough".to_owned())
        });
        let events = [vec![event(0)], Vec::new()];
        let switches = live.decide(&progress(&events, &standing));
        assert_eq!(switches.len(), 1);
        assert_eq!(switches[0].index, 0);
        assert!(!switches[0].attach);
        assert_eq!(switches[0].why, "enough");
        let again = live.decide(&progress(&events, &standing));
        assert_eq!(again.len(), 1);
        drop(live);
        assert_eq!(seen, 1);
    }
}
