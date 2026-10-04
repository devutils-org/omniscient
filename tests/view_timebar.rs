//! The timebar's first suspect, held to `omn latency`'s.
//!
//! The view crate cannot call the triage (`omn` links it), so the timebar
//! restates `omn latency`'s rule. A restatement drifts silently: this runs
//! the triage itself (`omn::triage::latency::read`, then `conclude`) on every
//! fixture that carries its decomposition, and the view's verdict on the same
//! table must name the same state -- or name none, as the triage does.

use omn::emit::Output;
use omn::encode::RunOutput;
use omn::request::Target;
use omn::triage::engine::Run;
use omn::triage::latency::{self, Ask};
use omn::triage::Objective;
use omn_view::model::Data;
use omn_view::snapshot::{fixture_names, fixtures_dir};
use omn_view::views::timebar::{self, Verdict, LATENCY_DECOMPOSITION};

/// A fixture's run as the triage reads it: its results, and every raw event
/// they hold in one stream, in time order.
fn run_of(name: &str) -> Run {
    let path = fixtures_dir().join(name).join("results.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    let output: RunOutput =
        serde_json::from_str(&text).unwrap_or_else(|error| panic!("{name}: {error}"));
    let mut events = Vec::new();
    for results in &output.results {
        for out in &results.outputs {
            if let Output::Raw { events: raw } = out {
                events.extend(raw.iter().cloned());
            }
        }
    }
    events.sort_by_key(|event| event.ts_ns);
    Run {
        output,
        events,
        served: Vec::new(),
        warnings: Vec::new(),
        caveats: Vec::new(),
        costs: Vec::new(),
    }
}

#[test]
fn the_timebar_names_the_state_omn_latency_names() {
    let mut held = 0;
    for name in fixture_names() {
        let data = Data::load(&fixtures_dir().join(&name))
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let carries =
            data.run.decompositions.iter().any(|value| {
                value.get("id").and_then(|id| id.as_str()) == Some(LATENCY_DECOMPOSITION)
            });
        if !carries {
            continue;
        }
        let run = run_of(&name);
        let ask = Ask {
            objective: Objective::Latency,
            target: Target::Launch(vec!["the fixture's program".to_owned()]),
            runs: 1,
            duration: None,
            hz: 997,
            symbols: None,
        };
        let reading = latency::read(&run, &ask.target)
            .unwrap_or_else(|error| panic!("{name}: the triage cannot read it: {error:?}"));
        // The triage follows a wait into the task that ran in it; the
        // timebar draws both tasks side by side instead, so the parity is
        // on the task the triage read first.
        let task = reading.task.clone();
        let followed = reading.followed.is_some();
        let answer = latency::conclude(&ask, vec![reading], Vec::new());

        let sheets = timebar::sheets(&data);
        let sheet = sheets
            .iter()
            .find(|sheet| sheet.id == LATENCY_DECOMPOSITION)
            .unwrap_or_else(|| panic!("{name}: the timebar draws no `{LATENCY_DECOMPOSITION}`"));
        let bar = sheet
            .bars
            .iter()
            .find(|bar| bar.key == task)
            .unwrap_or_else(|| panic!("{name}: no bar for {task}"));
        let ours = match &bar.verdict {
            Verdict::Names { part, .. } => Some(bar.parts[*part].state.clone()),
            Verdict::Undecided(_) => None,
        };
        if followed {
            // What the triage names is in the followed task: only the
            // first task's verdict is comparable, and it is not the answer.
            continue;
        }
        assert_eq!(
            ours, answer.verdict.state,
            "{name}: the timebar names {ours:?} on {task}, omn latency names {:?} ({})",
            answer.verdict.state, answer.verdict.sentence
        );
        held += 1;
    }
    assert!(
        held > 0,
        "no fixture carries `{LATENCY_DECOMPOSITION}` unfollowed: the parity holds nothing"
    );
}
