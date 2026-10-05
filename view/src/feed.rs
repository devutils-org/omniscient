//! A live run, as it reaches the view.
//!
//! `omn top` watches its runner between rounds -- the same events the encoders
//! will read at the end, never a second collection -- and sends what is new as
//! [`Update`]s. While the run goes, each request's result holds its `raw`
//! events only: the aggregated emits, the losses, the cost and the names of
//! stacks are what the runner says when it ends, and every figure until then
//! is drawn provisional. When it ends, [`Update::Ended`] replaces it all with
//! the run as `results.json` would hold it.

use crate::model::{Data, Drain, Event, Live, Output};

/// The most events a live request keeps on screen. Older ones are let go and
/// counted as evicted, with the reason, so the figure built from what is left
/// is drawn as the lower bound it is.
pub const LIVE_KEEP: usize = 200_000;

/// One thing a live run says.
#[derive(Debug, Clone)]
pub enum Update {
    /// New events of request `index`, in arrival order, and the runner's
    /// clock when it handed them over. `unconverted` counts the events read
    /// that could not be handed over, with the first reason: they are
    /// counted as undecoded, never let go silently.
    Events {
        index: usize,
        events: Vec<Event>,
        now_ns: u64,
        unconverted: Option<(u64, String)>,
    },
    /// The runner's clock, when nothing new arrived.
    Tick { now_ns: u64 },
    /// What the collector of request `index` says of its events, on every
    /// change, even after its first events were drawn: the attribute its
    /// value is in, the stamp its rows carry, and the drains that found
    /// drops so far. A source attached late, or a drop found mid-run, reaches
    /// the screen when it happens rather than at the end; the result's events
    /// stay as they are.
    Described {
        index: usize,
        value_attr: Option<String>,
        row_stamp: Option<String>,
        drains: Vec<Drain>,
    },
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
            unconverted,
        } => {
            if let Some(live) = data.live.as_mut() {
                live.now_ns = live.now_ns.max(now_ns);
            }
            let Some(results) = data.run.results.get_mut(index) else {
                return;
            };
            results.events_seen += events.len() as u64;
            if let Some((count, why)) = unconverted {
                results.losses.undecoded += count;
                *results
                    .losses
                    .reasons
                    .entry(format!(
                        "omn top could not hand these events to the view: {why}"
                    ))
                    .or_insert(0) += count;
            }
            let ts_err = events.iter().map(|event| event.ts_err_ns).max();
            if let Some(ts_err) = ts_err {
                results.ts_err_ns = Some(results.ts_err_ns.unwrap_or(0).max(ts_err));
            }
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
        Update::Described {
            index,
            value_attr,
            row_stamp,
            drains,
        } => {
            if let Some(results) = data.run.results.get_mut(index) {
                results.value_attr = value_attr;
                results.row_stamp = row_stamp;
                // A drain the view holds and the runner no longer lists is
                // kept: drops are never forgotten once they were drawn.
                for drain in drains {
                    if !results.drains.contains(&drain) {
                        results.drains.push(drain);
                    }
                }
                results.drains.sort_by_key(|drain| drain.ts_ns);
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
        // A source the recording kept without `raw` has no events to hand
        // over: replayed as live it arrives empty, where a live run would
        // have handed its events over as they came. Said, so an empty panel
        // is not read as a quiet source.
        let kept_raw = results.outputs.iter().any(|output| output.emit() == "raw");
        if !kept_raw && !results.outputs.is_empty() {
            let kept: Vec<&str> = results.outputs.iter().map(|output| output.emit()).collect();
            results.degradations.insert(
                0,
                format!(
                    "this recording kept `{}` as `{}` only, without `raw`: replayed as live it \
                     arrives empty, where a live run hands its events over as they come",
                    results.label,
                    kept.join("`, `")
                ),
            );
        }
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

    /// A result described after its first events keeps them, takes the
    /// description, and adds each drain once: the update a late source and a
    /// mid-run drop reach the screen by.
    #[test]
    fn a_description_after_the_first_events_keeps_them_and_adds_its_drains() {
        let mut data = live();
        apply(
            &mut data,
            Update::Events {
                index: 0,
                events: vec![event(1), event(2)],
                now_ns: 2,
                unconverted: None,
            },
        );
        let drain = |ts_ns, dropped| Drain { ts_ns, dropped };
        apply(
            &mut data,
            Update::Described {
                index: 0,
                value_attr: Some("bytes".into()),
                row_stamp: Some("end".into()),
                drains: vec![drain(5, 3)],
            },
        );
        apply(
            &mut data,
            Update::Described {
                index: 0,
                value_attr: Some("bytes".into()),
                row_stamp: Some("end".into()),
                drains: vec![drain(9, 1), drain(5, 3)],
            },
        );
        let results = &data.run.results[0];
        assert_eq!(results.events().len(), 2, "the events stay");
        assert_eq!(results.value_attr.as_deref(), Some("bytes"));
        assert_eq!(results.row_stamp.as_deref(), Some("end"));
        assert_eq!(results.drains, vec![drain(5, 3), drain(9, 1)]);
        // An index the run does not have changes nothing.
        apply(
            &mut data,
            Update::Described {
                index: 7,
                value_attr: None,
                row_stamp: None,
                drains: Vec::new(),
            },
        );
        assert_eq!(data.run.results.len(), 1);
    }

    #[test]
    fn a_source_kept_without_raw_says_it_arrives_empty() {
        let mut data = Data::default();
        data.run.results.push(Results {
            label: "cpu.profile".into(),
            source: "cpu.profile".into(),
            outputs: vec![Output::Timeline {
                series: Vec::new(),
                first_ts_ns: None,
            }],
            ..Default::default()
        });
        data.run.results.push(Results {
            label: "io.bytes".into(),
            source: "io.bytes".into(),
            outputs: vec![Output::Raw { events: Vec::new() }],
            ..Default::default()
        });
        let live = as_live_until(&data, 10);
        assert!(
            live.run.results[0].degradations[0].contains("without `raw`"),
            "{:?}",
            live.run.results[0].degradations
        );
        assert!(live.run.results[1].degradations.is_empty());
    }

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
                unconverted: None,
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
                unconverted: None,
            },
        );
        let results = &data.run.results[0];
        assert_eq!(results.events().len(), LIVE_KEEP);
        assert_eq!(results.losses.evicted, 7);
        assert_eq!(results.events()[0].ts_ns, 7, "the oldest go first");
    }

    #[test]
    fn events_that_could_not_be_handed_over_are_counted_with_why() {
        let mut data = live();
        apply(
            &mut data,
            Update::Events {
                index: 0,
                events: vec![event(1)],
                now_ns: 0,
                unconverted: Some((3, "a float that is not a number".into())),
            },
        );
        let results = &data.run.results[0];
        assert_eq!(results.losses.undecoded, 3);
        assert!(results
            .losses
            .reasons
            .keys()
            .any(|why| why.contains("a float that is not a number")));
    }

    #[test]
    fn a_live_result_knows_its_time_error_from_its_events() {
        let mut data = live();
        let mut late = event(1);
        late.ts_err_ns = 40;
        apply(
            &mut data,
            Update::Events {
                index: 0,
                events: vec![event(0), late],
                now_ns: 0,
                unconverted: None,
            },
        );
        assert_eq!(data.run.results[0].max_ts_err_ns(), Some(40));
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
