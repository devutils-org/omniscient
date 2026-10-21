use super::*;

#[test]
fn kernel_access_refusals_point_to_atfq() {
    assert!(super::kernel_access_refused(
        "`perf:task` needs perf_event_paranoid <= 1, this machine has 2"
    ));
    assert!(super::kernel_access_refused(
        "needs CAP_BPF on the omn binary"
    ));
    assert!(!super::kernel_access_refused(
        "`fs:sys:amdgpu`: /sys/bus/pci/drivers/amdgpu does not exist"
    ));
}

/// `gpu.kernel` through CUPTI is `extreme` on a launch-bound program and
/// `low` on a memory-bound one: a 5 % ceiling says which regimes fit.
#[test]
fn a_band_per_regime_says_which_programs_fit_the_ceiling() {
    let catalog = Catalog::embedded().expect("the embedded catalog loads");
    let entry = catalog.get(&"gpu.kernel".parse().unwrap()).unwrap();
    let cupti = entry.backend("vendor:cupti").unwrap();
    let note = by_regime(cupti, 5.0).expect("cupti gives its regimes");
    assert!(note.contains("launch `extreme`"), "{note}");
    assert!(
        note.contains("a compute- or memory-bound program"),
        "{note}"
    );
    let note = by_regime(cupti, 0.5).unwrap();
    assert!(note.contains("none within the ceiling"), "{note}");
    assert_eq!(by_regime(&entry.backends[1], 5.0), None);
}

/// `mem.faults` traced under a budget, resolved on this machine, or why it
/// cannot be.
fn traced_faults(emit: &str) -> Result<(Catalog, ResolvedRequest), String> {
    let catalog = Catalog::embedded().expect("the embedded catalog loads");
    let request = crate::request::Request {
        source: "mem.faults".parse().unwrap(),
        target: crate::request::Target::Pid(vec![std::process::id() as i32]),
        mode: crate::request::Mode::Trace,
        emit: vec![emit.parse().unwrap()],
        keys: Vec::new(),
        filter: None,
        duration: None,
        budget: Some("0.01%".parse().unwrap()),
        stack: None,
        symbols: None,
        backend: None,
        label: None,
        window: None,
        value: None,
        sum: Vec::new(),
    };
    let capabilities = Capabilities::detect();
    resolve::resolve(&catalog, &capabilities, &request)
        .map(|resolved| (catalog, resolved))
        .map_err(|error| error.to_string())
}

/// The rung `trace` -> `sample` is offered only on a collector that
/// weighs its samples, and `raw` is refused only when it could be taken:
/// with `mem.faults`'s collector weighing (as `page_faults.rs` does), a
/// `raw` list is refused up front and a `hist` is not; with it unweighed,
/// the rung is passed over, nothing can mix, and the note names the rung
/// and why.
#[test]
fn a_sampled_rung_whose_events_are_not_weighed_is_passed_over_not_taken() {
    let (catalog, raw) = match traced_faults("raw") {
        Ok(resolved) => resolved,
        Err(why) => {
            eprintln!("SKIP: `mem.faults` traced is not served here: {why}");
            return;
        }
    };
    let (_, hist) = traced_faults("hist").expect("hist resolves where raw does");
    if raw.backend.id != "pmu:sw" {
        eprintln!(
            "SKIP: `mem.faults` traced is served by `{}`, not `pmu:sw`",
            raw.backend.id
        );
        return;
    }
    let refused = budget_refusal_given(&catalog, &raw, |_, _| true)
        .expect("a weighing collector makes the move reachable, and raw cannot cross it");
    assert!(
        refused.contains("`raw` would hold the events of both modes as one kind"),
        "{refused}"
    );
    assert_eq!(budget_refusal_given(&catalog, &hist, |_, _| true), None);

    // The shipped collector weighs its samples: the same answers.
    assert!(collect::weighs_its_samples("mem.faults", "pmu:sw"));
    assert_eq!(budget_refusal(&catalog, &raw), Some(refused));
    assert_eq!(budget_refusal(&catalog, &hist), None);
    assert!(
        rungs(&catalog, &hist, 0.01)
            .iter()
            .any(|rung| rung.mode != hist.request.mode),
        "a weighed collector offers the mode rung"
    );

    assert_eq!(unweighed_mode_rungs(&catalog, &hist), None);

    // An unweighed one: the rung is passed over, and the note says why.
    assert_eq!(budget_refusal_given(&catalog, &raw, |_, _| false), None);
    let passed = unweighed_mode_rungs_given(&catalog, &hist, |_, _| false)
        .expect("an unweighed collector's mode rung is passed over");
    assert!(
        passed.contains("`pmu:sw` in `sample@99hz`, is passed over")
            && passed.contains("carry no `weight`"),
        "{passed}"
    );
}

/// The view's cpu inside a collection's window is taken out of it, and
/// what it spent before and after out of those: the three still add up
/// to the process's account, less the view.
#[test]
fn the_view_s_cpu_is_taken_out_of_the_collectors_account() {
    let view_cpu = ViewCpu {
        drawing: None,
        handover: std::sync::Arc::default(),
        drawn_ns: 0,
        // At 100 ms of process cpu the view had spent 10; at 300, 50;
        // at 500, 90.
        samples: vec![(100, 10), (300, 50), (500, 90)],
    };
    assert_eq!(view_cpu.at(200), 30);
    assert_eq!(view_cpu.at(50), 10, "before the first sample: all of it");
    assert_eq!(view_cpu.at(900), 90);
    let mut overhead = collect::Overhead {
        before_window_cpu_ns: 100,
        collector_cpu_ns: 200,
        attributed_cpu_ns: 100,
        after_window_cpu_ns: 200,
        ..Default::default()
    };
    view_cpu.take_out(&mut overhead);
    assert_eq!(
        overhead.collector_cpu_ns, 160,
        "40 of the window was the view's"
    );
    assert_eq!(
        overhead.attributed_cpu_ns, 80,
        "the share keeps its proportion"
    );
    assert_eq!(overhead.before_window_cpu_ns, 90);
    assert_eq!(overhead.after_window_cpu_ns, 160);
}

/// The cpu spent after a collection's window closed -- an fentry unlink,
/// about 69 ms for `io.fsync` -- is named in the cost line when it
/// outweighs the window, as the cpu before it opened is.
#[test]
fn the_cost_line_names_the_cpu_spent_after_the_window_closed() {
    let cost = |after_window_cpu_ns| MeasuredCost {
        label: "io.fsync".to_owned(),
        source: "io.fsync".to_owned(),
        mode: "trace".to_owned(),
        backend: "bpf:fentry".to_owned(),
        served_by: vec!["bpf:fentry".to_owned()],
        declared: "low",
        declared_by_regime: BTreeMap::new(),
        declared_worst_case_percent: 5.0,
        budget_percent: None,
        within_budget: None,
        measured_percent: None,
        own_percent: None,
        overhead: collect::Overhead {
            attributed_cpu_ns: 5_000_000,
            after_window_cpu_ns,
            ..Default::default()
        },
        stopped_because: String::new(),
        stopped_by_budget: false,
        notes: Vec::new(),
        weighed_over_ns: None,
    };
    let line = cost(69_000_000).line();
    assert!(
        line.contains("and 69.0ms after it closed (detaching"),
        "{line}"
    );
    let line = cost(1_000_000).line();
    assert!(!line.contains("after it closed"), "{line}");
}

/// A batch with one event that cannot be handed over loses that one,
/// counted with the reason, and not the rest.
#[test]
fn one_event_that_does_not_convert_is_counted_and_the_rest_handed_over() {
    let event = |value: f64| crate::event::Event {
        ts_ns: 1,
        ts_err_ns: 0,
        kind: "x".to_owned(),
        keys: Vec::new(),
        attrs: vec![crate::event::Attr::float("value", value)],
        collector_id: crate::event::CollectorId::new("x"),
        seq: 0,
    };
    let (converted, unconverted) = to_view_events(&[event(1.0), event(f64::NAN), event(2.0)]);
    assert_eq!(converted.len(), 2);
    let (count, why) = unconverted.expect("the NaN is counted");
    assert_eq!(count, 1, "{why}");
}

#[test]
fn a_key_the_backend_cannot_attach_is_the_request_s_to_change() {
    let error = resolve::ResolveError::KeyNotCarried {
        source_id: "io.bytes".into(),
        backend: "fs:proc".into(),
        key: crate::request::KeyKind::Task,
        elsewhere: Some(resolve::KeyElsewhere::Forceable("tracepoint".into())),
    };
    assert_eq!(resolution_is(&error), crate::collect::Whose::Request);
    // No hint: the message already names what would serve it.
    assert_eq!(resolution_hint(&error), None);
    let message = error.to_string();
    assert!(message.contains("`task`"), "{message}");
    assert!(message.contains("--backend force:tracepoint"), "{message}");

    let nowhere = resolve::ResolveError::KeyNotCarried {
        source_id: "io.bytes".into(),
        backend: "fs:proc".into(),
        key: crate::request::KeyKind::Task,
        elsewhere: None,
    };
    assert!(
        nowhere
            .to_string()
            .contains("no backend of `io.bytes` attaches it"),
        "{nowhere}"
    );
    assert_eq!(resolution_is(&nowhere), crate::collect::Whose::Request);

    // A backend nothing collects is the build's gap, and one the machine
    // cannot run is the machine's; neither is offered as a pin.
    let unwritten = resolve::ResolveError::KeyNotCarried {
        source_id: "io.bytes".into(),
        backend: "fs:proc".into(),
        key: crate::request::KeyKind::Task,
        elsewhere: Some(resolve::KeyElsewhere::Unwritten("bpf:raw_tp".into())),
    };
    assert_eq!(resolution_is(&unwritten), crate::collect::Whose::Build);
    assert!(
        !unwritten.to_string().contains("--backend force:"),
        "{unwritten}"
    );
    let missing = resolve::ResolveError::KeyNotCarried {
        source_id: "io.bytes".into(),
        backend: "fs:proc".into(),
        key: crate::request::KeyKind::Task,
        elsewhere: Some(resolve::KeyElsewhere::Missing {
            backend: "bpf:raw_tp".into(),
            reason: "bpf: missing".into(),
        }),
    };
    assert_eq!(resolution_is(&missing), crate::collect::Whose::Machine);
    assert!(
        !missing.to_string().contains("--backend force:"),
        "{missing}"
    );
}

/// A collection as a run leaves it, carrying `failure` when given.
fn collection(failure: Option<&str>) -> collect::Collection {
    let mut collection: collect::Collection = serde_json::from_value(serde_json::json!({
        "events": [],
        "value_attr": "value",
        "losses": collect::Losses::default(),
        "observed_ns": 0,
        "stopped_because": "the test",
    }))
    .expect("a bare collection deserializes");
    collection.failure = failure.map(str::to_owned);
    collection
}

/// A collector that broke the program it measured makes the run's
/// outcome a failure, said once however many of its collections carry
/// it; a run without one exits 0, and a collection without one writes no
/// `failure` at all.
#[test]
fn a_collector_that_broke_its_program_fails_the_run_once() {
    let killed = "pid 7 died inside omn's injection library";
    let collections = [
        collection(None),
        collection(Some(killed)),
        collection(Some(killed)),
    ];
    assert_eq!(failures(&collections), vec![killed.to_owned()]);
    assert_eq!(outcome(&failures(&collections)), ExitCode::FAILURE);
    assert_eq!(outcome(&failures(&[collection(None)])), ExitCode::SUCCESS);
    let written = serde_json::to_value(collection(None)).unwrap();
    assert!(written.get("failure").is_none(), "{written}");
    let written = serde_json::to_value(collection(Some(killed))).unwrap();
    assert_eq!(written["failure"], killed);
}

/// A live view is given what each collector says of its events -- the
/// value attribute, the row stamp, the drops at their drains -- once per
/// change: as the opening data while no event was handed over, and as an
/// `Update::Described` of the changed result after, since a restart then
/// would lose what it holds.
#[test]
fn a_live_view_is_told_how_to_read_its_results_whenever_that_changes() {
    let mut shell = omn_view::model::Data::default();
    shell.run.results = vec![omn_view::model::Results::default(); 2];
    let (sender, _receiver) = std::sync::mpsc::channel();
    let quit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut tap = Tap::new(shell, sender, quit, None);
    let drains = [collect::DrainDrop {
        ts_ns: 40,
        dropped: 3,
    }];
    let said = [
        collect::Described {
            value_attr: "latency_ns",
            row_stamp: Some(collect::RowStamp::DrainClose),
            drains: &drains,
        },
        collect::Described {
            value_attr: "",
            row_stamp: None,
            drains: &[],
        },
    ];
    let mut updates = tap.described_updates(&said);
    assert_eq!(updates.len(), 1, "one restart");
    let Some(omn_view::feed::Update::Restart(described)) = updates.pop() else {
        panic!("the first description is the opening data again");
    };
    let first = &described.run.results[0];
    assert_eq!(first.value_attr.as_deref(), Some("latency_ns"));
    assert_eq!(first.row_stamp.as_deref(), Some("drain_close"));
    assert_eq!(first.drains.len(), 1);
    assert_eq!(first.drains[0].dropped, 3);
    assert_eq!(described.run.results[1].value_attr, None);
    assert!(
        tap.described_updates(&said).is_empty(),
        "an unchanged one is not"
    );
    let went_over = crate::event::Event {
        ts_ns: 0,
        ts_err_ns: 0,
        kind: "net.tcp.connect".to_owned(),
        keys: Vec::new(),
        attrs: Vec::new(),
        collector_id: crate::event::CollectorId::new("test"),
        seq: 0,
    };
    let events = [Vec::new(), vec![went_over; 5]];
    let standing = [collect::Standing::Attached, collect::Standing::Attached];
    tap.handover.fresh(&collect::Progress {
        opened_ns: 0,
        now_ns: 0,
        events: &events,
        standing: &standing,
        bound: &[],
    });
    let mut later = said;
    later[1].value_attr = "bytes";
    let updates = tap.described_updates(&later);
    assert_eq!(updates.len(), 1, "only the changed result");
    match &updates[0] {
        omn_view::feed::Update::Described {
            index, value_attr, ..
        } => {
            assert_eq!(*index, 1);
            assert_eq!(value_attr.as_deref(), Some("bytes"));
        }
        _ => panic!("once events went over, a description keeps them"),
    }
    assert!(tap.described_updates(&later).is_empty());
}
