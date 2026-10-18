//! The view's mirror of a run, held to `omn`'s own types.
//!
//! `omn top` reads results through `omn_view::model`, a field-for-field mirror
//! of what `omn` writes (the view crate cannot depend on `omn`, which links
//! it). A mirror drifts silently: a field `omn` adds is dropped on the view's
//! side, and a view that needed it draws nothing and says nothing. So every
//! fixture recording is read through both, written back out by both, and the
//! two documents must be the same.

use std::path::Path;

use omn::encode::RunOutput;
use omn_view::model::{Data, Run};
use omn_view::snapshot::{fixture_names, fixtures_dir};

/// Every fixture there is; the suite is meaningless without them.
fn fixtures() -> Vec<String> {
    let names = fixture_names();
    assert!(
        !names.is_empty(),
        "no fixture recording under {}: tests/view_fixtures/record.sh writes them",
        fixtures_dir().display()
    );
    names
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// A document without the fields that hold their default -- `null`, zero,
/// `false`, empty -- which one side writes and the other skips: what is left
/// is what either says, and a field the mirror drops is still missing.
fn normal(value: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(field, value)| (field, normal(value)))
                .filter(|(_, value)| match value {
                    Value::Null => false,
                    Value::Bool(flag) => *flag,
                    Value::Number(number) => number.as_f64() != Some(0.0),
                    Value::String(text) => !text.is_empty(),
                    Value::Array(items) => !items.is_empty(),
                    Value::Object(fields) => !fields.is_empty(),
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(normal).collect()),
        other => other,
    }
}

#[test]
fn the_mirror_holds_every_field_omn_writes() {
    for name in fixtures() {
        let path = fixtures_dir().join(&name).join("results.json");
        let text = read(&path);
        let theirs: RunOutput = serde_json::from_str(&text)
            .unwrap_or_else(|error| panic!("{name}: omn cannot read its own fixture: {error}"));
        let ours: Run = serde_json::from_str(&text)
            .unwrap_or_else(|error| panic!("{name}: the view cannot read it: {error}"));
        let theirs = normal(serde_json::to_value(&theirs).expect("omn writes it back"));
        let ours = normal(serde_json::to_value(&ours).expect("the view writes it back"));
        if theirs != ours {
            let (theirs_results, ours_results) = (&theirs["results"], &ours["results"]);
            for index in 0..theirs_results.as_array().map_or(0, Vec::len) {
                let (one, two) = (&theirs_results[index], &ours_results[index]);
                if one != two {
                    for (field, value) in one.as_object().into_iter().flatten() {
                        assert_eq!(
                            Some(value),
                            two.get(field),
                            "{name}: result {index} ({}): field `{field}` differs between omn \
                             and the view's mirror",
                            one["label"]
                        );
                    }
                }
            }
            panic!("{name}: the view's mirror writes a different document than omn");
        }
    }
}

#[test]
fn every_fixture_carries_what_omn_top_writes_beside_it() {
    for name in fixtures() {
        let data = Data::load(&fixtures_dir().join(&name))
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert!(!data.run.results.is_empty(), "{name} holds no result");
        assert!(
            !data.facts.is_empty(),
            "{name} has no view.json facts: record it with `omn top -o`"
        );
        for results in &data.run.results {
            assert!(
                data.facts.contains_key(&results.source),
                "{name}: no catalog facts for `{}`",
                results.source
            );
        }
        for refusal in &data.refused {
            assert!(
                !refusal.reason.trim().is_empty(),
                "{name}: `{}` refused without a reason",
                refusal.label
            );
        }
    }
}

#[test]
fn a_flight_recorder_document_reads_with_its_trigger() {
    let fixture = fixtures_dir().join(&fixtures()[0]).join("results.json");
    let run: RunOutput = serde_json::from_str(&read(&fixture)).expect("a run");
    let fired = omn::collect::Fired {
        number: 1,
        of_at_most: 5,
        predicate: "value >= 150".to_owned(),
        on: "descriptors".to_owned(),
        fired_ns: 2_000,
        event_ts_ns: 1_990,
        value_attr: "value".to_owned(),
        value: Some(300.0),
        fields: Default::default(),
        run_opened_ns: 0,
        opened_ns: 1_000,
        closed_ns: 2_600,
        retain_ns: 1_000,
        after_ns: 600,
        cut_short: None,
        fired_again: 0,
        evicted: Vec::new(),
    };
    // As `omn` writes `trigger-<n>.json`.
    let bytes = omn::encode::triggered_bytes(omn::format::OutputFormat::Json, &fired, &run)
        .expect("a recorder document");
    let dir = std::env::temp_dir().join(format!("omn-view-trigger-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    std::fs::write(dir.join("trigger-1.json"), &bytes).expect("written");
    // A directory with no results.json opens on its first document.
    let data = Data::load(&dir).expect("a recorder directory loads");
    std::fs::remove_dir_all(&dir).ok();
    let trigger = data.trigger.expect("the trigger is kept");
    assert_eq!(trigger["predicate"], "value >= 150");
    assert_eq!(trigger["number"], 1);
    assert_eq!(data.run.results.len(), run.results.len());
}

#[test]
fn the_readme_lists_what_each_view_needs() {
    let readme = read(&Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/view/README.md"));
    for view in omn_view::views::registry() {
        for need in view.needs() {
            // The whole row: what the view draws from the source (`why`)
            // drifts as easily as its mode.
            let mut emits = need.emit.join(",");
            if !need.keys.is_empty() {
                emits.push_str(&format!(" (keys {})", need.keys.join(",")));
            }
            if let Some(stack) = need.stack {
                emits.push_str(&format!(" (stack {stack})"));
            }
            let scope = match need.scope {
                omn_view::views::Scope::Target => "target",
                omn_view::views::Scope::System => "system",
            };
            let row = format!(
                "| {} | `{}` | {} | {emits} | {scope} | {} |",
                view.name(),
                need.source,
                need.mode,
                need.why
            );
            assert!(
                readme.lines().any(|line| line == row),
                "docs/view/README.md does not list `{row}`: the table of needs is out of date"
            );
        }
    }
}

/// The flame view's comparison re-states `src/triage/diff.rs` (the view
/// links nothing of `omn`): on the two CPython recordings both name the same
/// functions, in the same order, in the same unit. A change to either side's
/// arithmetic that the other did not follow fails here.
#[test]
fn the_flame_view_s_difference_is_the_triage_s() {
    let dir = fixtures_dir();
    let (before, after) = ("cpython-hot", "cpython-hot-heavier");
    // The triage compares one profile per run: the on-cpu one, as the
    // view's on-cpu pane does.
    let on_cpu = |name: &str| {
        let mut run: RunOutput =
            serde_json::from_str(&read(&dir.join(name).join("results.json"))).unwrap();
        run.results
            .retain(|results| results.source == "cpu.profile");
        let period = run.results.first().and_then(|results| results.period_ns);
        omn::triage::diff::Profile::from_run(
            name.to_owned(),
            &run,
            period.map(|ns| ns as f64),
            None,
        )
        .expect("the triage reads the run")
    };
    for (one, other) in [(before, after), (after, before)] {
        let triage = omn::triage::diff::diff(on_cpu(one), on_cpu(other));
        let (unit, named) = omn_view::views::flame::difference(
            &omn_view::snapshot::fixture(one),
            &omn_view::snapshot::fixture(other),
        )
        .expect("both recordings hold an on-cpu profile");
        assert_eq!(unit, triage.unit, "{one} against {other}: the unit");
        assert!(
            !named.is_empty(),
            "{one} against {other}: the heavier run moved nothing, and the test holds nothing"
        );
        assert_eq!(
            named, triage.difference,
            "{one} against {other}: the functions named"
        );
    }
}

/// What `omn` writes of a drained source -- how it stamped its rows, and its
/// drops placed at the drains that read them -- is what the view reads.
#[test]
fn the_view_reads_a_result_s_row_stamp_and_drains_as_omn_writes_them() {
    let fixture = fixtures_dir().join(&fixtures()[0]).join("results.json");
    let run: RunOutput = serde_json::from_str(&read(&fixture)).expect("a run");
    let mut written = run.results[0].clone();
    written.row_stamp = Some(omn::collect::RowStamp::DrainClose);
    written.drains = vec![
        omn::collect::DrainDrop {
            ts_ns: 1_000,
            dropped: 3,
        },
        omn::collect::DrainDrop {
            ts_ns: 2_000,
            dropped: 4,
        },
    ];
    let json = serde_json::to_value(&written).expect("serialized");
    let read: omn_view::model::Results = serde_json::from_value(json).expect("read back");
    assert_eq!(read.row_stamp.as_deref(), Some("drain_close"));
    let drains: Vec<(u64, u64)> = read
        .drains
        .iter()
        .map(|drain| (drain.ts_ns, drain.dropped))
        .collect();
    assert_eq!(drains, [(1_000, 3), (2_000, 4)]);
    // The first-event spelling, too.
    written.row_stamp = Some(omn::collect::RowStamp::FirstEvent);
    let read: omn_view::model::Results =
        serde_json::from_value(serde_json::to_value(&written).unwrap()).unwrap();
    assert_eq!(read.row_stamp.as_deref(), Some("first_event"));
}

/// A launched program's life read through the view's typed mirror and
/// written back is the life `omn` wrote, every field kept, `adopted_ended`
/// among them.
#[test]
fn the_lifetime_mirror_keeps_every_field() {
    let written = omn::collect::Lifetime {
        released_ns: 10,
        exited_ns: 2_000,
        exited_err_ns: 7,
        terminated: true,
        cpu_ns: Some(1_234),
        left_running: 1,
        adopted_ended: 3,
        exit_code: None,
        signal: Some(9),
    };
    let value = serde_json::to_value(written).unwrap();
    let results = omn_view::model::Results {
        lifetime: Some(value),
        ..Default::default()
    };
    let read = results.launched().expect("a lifetime");
    assert_eq!(read.adopted_ended, 3);
    let back: omn::collect::Lifetime =
        serde_json::from_value(serde_json::to_value(&read).unwrap()).unwrap();
    assert_eq!(back, written);
    assert_eq!(read.signal, Some(9));
    assert_eq!(read.ending().as_deref(), Some("stopped by the run"));
    let exited = omn_view::model::Lifetime {
        exit_code: Some(3),
        ..read.clone()
    };
    assert_eq!(
        omn_view::model::Lifetime {
            terminated: false,
            ..exited
        }
        .ending()
        .as_deref(),
        Some("exited 3")
    );
}
