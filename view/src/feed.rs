//! A live run, as it reaches the view.
//!
//! `omn top` watches its runner between rounds -- the same events the encoders
//! will read at the end, never a second collection -- and sends what is new as
//! [`Update`]s. While the run goes, each request's result holds its `raw`
//! events only: the aggregated emits, the losses, the cost and the names of
//! stacks are what the runner says when it ends, and every figure until then
//! is drawn provisional. When it ends, [`Update::Ended`] replaces it all with
//! the run as `results.json` would hold it.

use crate::model::{Data, Event, Live, Output};

/// The most events a live request keeps on screen. Older ones are let go and
/// counted as evicted, with the reason, so the figure built from what is left
/// is drawn as the lower bound it is.
pub const LIVE_KEEP: usize = 200_000;

/// One thing a live run says.
#[derive(Debug, Clone)]
pub enum Update {
    /// New events of request `index`, in arrival order, and the runner's
    /// clock when it handed them over.
    Events {
        index: usize,
        events: Vec<Event>,
        now_ns: u64,
    },
    /// The runner's clock, when nothing new arrived.
    Tick { now_ns: u64 },
    /// The run is over: the whole of it.
    Ended(Box<Data>),
    /// The run starts over with these requests: the runner refused some
    /// before anything was launched, and they are now among the refusals.
    Restart(Box<Data>),
    /// The run could not go on: the view closes, and `omn` says why on the
    /// terminal it gives back.
    Abort,
}

/// Applies one update to what the view holds.
pub fn apply(data: &mut Data, update: Update) {
    match update {
        Update::Events {
            index,
            events,
            now_ns,
        } => {
            if let Some(live) = data.live.as_mut() {
                live.now_ns = live.now_ns.max(now_ns);
            }
            let Some(results) = data.run.results.get_mut(index) else {
                return;
            };
            results.events_seen += events.len() as u64;
            if !results
                .outputs
                .iter()
                .any(|output| matches!(output, Output::Raw { .. }))
            {
                results
                    .outputs
                    .insert(0, Output::Raw { events: Vec::new() });
            }
            let Some(Output::Raw { events: held }) = results
                .outputs
                .iter_mut()
                .find(|output| matches!(output, Output::Raw { .. }))
            else {
                return;
            };
            held.extend(events);
            if held.len() > LIVE_KEEP {
                let over = held.len() - LIVE_KEEP;
                held.drain(..over);
                results.losses.evicted += over as u64;
                *results
                    .losses
                    .reasons
                    .entry(format!(
                        "omn top keeps the last {LIVE_KEEP} events of a live request on screen"
                    ))
                    .or_insert(0) += over as u64;
            }
        }
        Update::Tick { now_ns } => {
            if let Some(live) = data.live.as_mut() {
                live.now_ns = live.now_ns.max(now_ns);
            }
        }
        Update::Restart(restart) => *data = *restart,
        Update::Abort => {}
        Update::Ended(ended) => {
            let origin = std::mem::take(&mut data.origin);
            *data = *ended;
            if data.origin.is_empty() {
                data.origin = origin;
            }
            data.live = None;
        }
    }
}

/// A recorded run as it would have looked live at `until_ns`: each result
/// reduced to its `raw` events up to then, with nothing the runner says only
/// at the end -- no aggregated emits, no losses, no cost, no stack names.
///
/// What a view is tested on to show it draws a live run, which is not what it
/// draws at the end, from the same recording.
pub fn as_live_until(data: &Data, until_ns: u64) -> Data {
    let mut live = data.clone();
    for results in &mut live.run.results {
        let events: Vec<Event> = results
            .events()
            .iter()
            .filter(|event| event.ts_ns <= until_ns)
            .cloned()
            .collect();
        results.events_seen = events.len() as u64;
        results.outputs = vec![Output::Raw { events }];
        results.losses = Default::default();
        results.overhead = Default::default();
        results.stacks.clear();
        results.stopped_because.clear();
        results.window.closed_ns = until_ns;
    }
    live.run.joins.clear();
    live.run.decompositions.clear();
    live.live = Some(Live {
        opened_ns: data.opened_ns(),
        now_ns: until_ns,
    });
    live
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Results;

    fn event(ts_ns: u64) -> Event {
        Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "x".into(),
            keys: vec![],
            attrs: vec![],
            collector_id: String::new(),
            seq: ts_ns,
        }
    }

    fn live() -> Data {
        Data {
            run: crate::model::Run {
                results: vec![Results {
                    label: "x".into(),
                    source: "x".into(),
                    ..Results::default()
                }],
                ..Default::default()
            },
            live: Some(Live::default()),
            ..Data::default()
        }
    }

    #[test]
    fn events_arrive_into_the_raw_output() {
        let mut data = live();
        apply(
            &mut data,
            Update::Events {
                index: 0,
                events: vec![event(1), event(2)],
                now_ns: 5,
            },
        );
        assert_eq!(data.run.results[0].events().len(), 2);
        assert_eq!(data.run.results[0].events_seen, 2);
        assert_eq!(data.live.as_ref().unwrap().now_ns, 5);
    }

    #[test]
    fn what_a_live_request_lets_go_is_counted() {
        let mut data = live();
        apply(
            &mut data,
            Update::Events {
                index: 0,
                events: (0..LIVE_KEEP as u64 + 7).map(event).collect(),
                now_ns: 0,
            },
        );
        let results = &data.run.results[0];
        assert_eq!(results.events().len(), LIVE_KEEP);
        assert_eq!(results.losses.evicted, 7);
        assert_eq!(results.events()[0].ts_ns, 7, "the oldest go first");
    }

    #[test]
    fn the_end_replaces_the_provisional_run() {
        let mut data = live();
        data.origin = "live".into();
        apply(&mut data, Update::Ended(Box::default()));
        assert!(data.live.is_none());
        assert_eq!(data.origin, "live");
    }
}
