//! The backend comparison: `ROADMAP.md` phase 8, task 4, and its first two
//! exit criteria -- a cost, resolution and loss table for every (source,
//! backend) this machine serves, and the resolver's choices confirmed by it or
//! corrected.
//!
//! Every row is a backend forced with `--backend force:<id>`, run round-robin
//! against the same workload with no tracer at all, twenty times after a
//! warmup, with the LLC evicted before each run. What a row reports:
//!
//! - **collector cpu**: what the tool says it spent (`cost.json`), median and
//!   IQR over the repetitions;
//! - **dilation**: how much longer the workload's own timed section took
//!   under the tool than bare -- measured by the workload, not by the tool;
//! - **omn RSS**: the tool's own peak resident size, polled from outside, so
//!   the agent's cost is measured apart from the target's (the third exit
//!   criterion);
//! - **losses**, as the tool counted them.
//!
//! The variance gate runs first, and a refusal ends the test with the numbers
//! and no table. That is the gate working: a table from a noisy machine is
//! worse than none.
//!
//! All of it is `#[ignore]` and runs through `scripts/bench.sh`.

#[path = "../examples/bench/harness.rs"]
mod harness;

use std::collections::BTreeMap;

use harness::*;
use serde_json::{json, Value};

/// `OMN_BENCH_ONLY=io.` restricts the plan to sources with that prefix.
fn only() -> Option<String> {
    std::env::var("OMN_BENCH_ONLY")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Every repetition of one row.
#[derive(Default)]
struct Samples {
    elapsed: Vec<f64>,
    collector: Vec<f64>,
    percent: Vec<f64>,
    rss_kb: Vec<f64>,
    events: Vec<f64>,
    losses: u64,
    throttled: usize,
    served_by: Vec<String>,
}

fn median(values: &[f64]) -> Option<f64> {
    Stats::of(values).map(|stats| stats.median)
}

fn cell(stats: Option<Stats>, format: impl Fn(f64) -> String) -> String {
    match stats {
        Some(stats) => format!("{} [{}]", format(stats.median), format(stats.iqr())),
        None => "-".into(),
    }
}

#[test]
#[ignore = "measures the machine for 15 to 30 minutes: run through scripts/bench.sh"]
fn every_served_backend_is_costed_and_the_resolver_is_judged_by_it() {
    let omn = Omn::locate();
    let repetitions = repetitions();
    let smoke = repetitions < MIN_REPETITIONS;
    say(format!("measuring {}", omn.describe()));
    prepare_io_file().expect("the io file");

    // 1. The plan: every (source, backend) this build serves here, made first
    // so the gate can judge every workload it measures against.
    let (planned, unplanned) = plan(&omn, only().as_deref());

    // 2. The gate, before anything is measured, on the reference workloads
    // and on every other workload the plan measures against -- `malloc` for
    // the heap sources. A workload with no baseline has no resolution to
    // judge, and the first real run excluded the heap sources for exactly
    // that.
    let mut workloads = reference_workloads();
    for row in &planned {
        if !workloads.iter().any(|w| w.name == row.workload.name) {
            workloads.push(row.workload.clone());
        }
    }
    let (baselines, _) = harness::baselines(&workloads, repetitions).expect("the baselines");
    eprint!("{}", gate_table(&baselines));
    let gate = judge(&baselines, &Conditions::read());
    match (&gate, smoke) {
        (Err(refusal), false) => panic!("{refusal}"),
        (Err(refusal), true) => say(format!(
            "SMOKE RUN ({repetitions} repetitions): the gate's refusal is recorded, not obeyed, \
             and nothing written is a measurement\n{refusal}"
        )),
        (Ok(admitted), _) => {
            say("the gate passed");
            for line in admitted.exclusions() {
                say(format!("excluded, and not measured: {line}"));
            }
        }
    }
    let admitted = gate.as_ref().ok().cloned().unwrap_or_default();

    say(format!(
        "{} (source, backend) pairs to measure, {} not served here",
        planned.len(),
        unplanned.len()
    ));
    let mut by_source: BTreeMap<String, Vec<Planned>> = BTreeMap::new();
    for row in planned {
        by_source
            .entry(row.method.source.clone())
            .or_default()
            .push(row);
    }

    // 3. Each source's backends round-robin against its bare workload.
    let mut evictor = Evictor::new();
    let mut rows: Vec<(Planned, Samples)> = Vec::new();
    let mut bare_elapsed: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut failures: Vec<String> = Vec::new();
    let mut excluded: Vec<(String, String)> = Vec::new();
    for (source, group) in &by_source {
        let workload = group[0].workload.clone();
        // A workload the gate excluded cannot resolve the overhead this table
        // is for, so its sources are listed under "not measured" with why. A
        // smoke run measures everything at its own count, and says so.
        let repetitions = if smoke || gate.is_err() {
            repetitions
        } else {
            match admitted.repetitions(&workload.name, repetitions) {
                Some(needed) => needed,
                None => {
                    let why = admitted
                        .excluded
                        .get(&workload.name)
                        .cloned()
                        .unwrap_or_else(|| "its workload has no baseline".into());
                    excluded.push((source.clone(), format!("`{}` {why}", workload.name)));
                    continue;
                }
            }
        };
        let methods = group.len() + 1; // index 0 is the bare run
        let mut samples: Vec<Samples> = (0..group.len()).map(|_| Samples::default()).collect();
        let bare = bare_elapsed.entry(source.clone()).or_default();
        say(format!(
            "{source}: {} backend(s) against `{}`",
            group.len(),
            workload.name
        ));
        for _ in 0..WARMUP {
            let _ = run_bare(&workload);
            for row in group {
                let _ = run_traced(&omn, &row.method, &workload);
            }
        }
        for index in round_robin(methods, repetitions) {
            evictor.evict();
            if index == 0 {
                match run_bare(&workload) {
                    Ok(run) if !run.throttled => bare.push(run.elapsed_ns() as f64),
                    Ok(_) => {}
                    Err(error) => failures.push(format!("{source} bare: {error}")),
                }
                continue;
            }
            let row = &group[index - 1];
            let one = &mut samples[index - 1];
            match run_traced(&omn, &row.method, &workload) {
                Ok(traced) if traced.run.throttled => one.throttled += 1,
                Ok(traced) => {
                    one.elapsed.push(traced.run.elapsed_ns() as f64);
                    if let Some(cpu) = traced.collector_cpu_ns() {
                        one.collector.push(cpu as f64);
                    }
                    if let Some(percent) = traced.measured_percent() {
                        one.percent.push(percent);
                    }
                    if let Some(rss) = traced.omn_peak_rss_kb {
                        one.rss_kb.push(rss as f64);
                    }
                    one.events.push(traced.events_seen() as f64);
                    one.losses += traced.losses();
                    if let Some(backend) = traced.backend() {
                        if !one.served_by.iter().any(|b| b == backend) {
                            one.served_by.push(backend.to_owned());
                        }
                    }
                }
                Err(error) => failures.push(format!("{}: {error}", row.method.label())),
            }
        }
        rows.extend(group.iter().cloned().zip(samples));
    }

    // 4. The table, the verdicts, and what was not measured.
    let dir = run_dir(if smoke { "smoke-backends" } else { "backends" });
    let mut table = String::new();
    if smoke || gate.is_err() {
        table.push_str(
            "> **NOT A MEASUREMENT.** A smoke run of the harness: fewer than twenty \
             repetitions, or a machine the gate refused. It shows the mechanics work; its \
             numbers must not be quoted.\n\n",
        );
    }
    table.push_str(&format!(
        "# Backend comparison\n\nMeasured with {}, at least {repetitions} repetitions round-robin \
         (more for a workload whose spread needed them to resolve 1 %, as the gate table says) \
         after {WARMUP} warmup runs, the LLC evicted before each. Collector cpu and dilation \
         are median [IQR]. A poll is measured at 100 ms and a sample at 997 Hz.\n\n",
        omn.describe()
    ));
    table.push_str(&format!("```\n{}```\n", gate_table(&baselines)));
    table.push_str(
        "\n| source | backend | resolver | mode | target | workload | collector cpu | % of \
         target | dilation | omn peak RSS | events | losses | dropped (throttled) | band |\n\
         |---|---|---|---|---|---|---|---|---|---|---|---|---|---|\n",
    );
    let mut json_rows = Vec::new();
    let mut flagged: Vec<String> = Vec::new();
    let mut verdicts: BTreeMap<String, Vec<Measured>> = BTreeMap::new();
    for (row, one) in &rows {
        let source = &row.method.source;
        let backend = row.method.backend.clone().unwrap_or_default();
        if !one.served_by.is_empty() && one.served_by != [backend.clone()] {
            failures.push(format!(
                "{source}: forced `{backend}` and was served by {:?}",
                one.served_by
            ));
        }
        let bare = bare_elapsed.get(source).and_then(|b| median(b));
        let dilation = match (bare, Stats::of(&one.elapsed)) {
            (Some(bare), Some(traced)) if bare > 0.0 && !row.method.system => Some(Stats {
                q1: traced.q1 / bare - 1.0,
                median: traced.median / bare - 1.0,
                q3: traced.q3 / bare - 1.0,
                ..traced
            }),
            _ => None,
        };
        let collector = Stats::of(&one.collector);
        let band = judge_band(
            row.declared,
            one.elapsed.len(),
            collector,
            Stats::of(&one.percent),
            dilation,
        );
        match &band {
            Band::Missing(why) => failures.push(format!(
                "{source} through `{backend}`: the row is missing a column: {why}"
            )),
            Band::Over(why) => flagged.push(format!("{source} through `{backend}`: {why}")),
            Band::Within(_) => {}
        }
        if let Some(cost) = collector {
            verdicts.entry(source.clone()).or_default().push(Measured {
                backend: backend.clone(),
                setting: format!(
                    "{}, {}",
                    row.method.mode,
                    if row.method.system {
                        "system"
                    } else {
                        "launch"
                    }
                ),
                chosen: row.chosen,
                degraded: row.degraded.clone(),
                equivalent: row.equivalent,
                cost,
                losses: one.losses,
            });
        }
        table.push_str(&format!(
            "| {source} | {backend} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            if row.chosen { "chosen" } else { "" },
            row.method.mode,
            if row.method.system {
                "system"
            } else {
                "launch"
            },
            row.workload.name,
            cell(collector, span),
            cell(Stats::of(&one.percent), |p| format!("{p:.2} %")),
            cell(dilation, |d| format!("{:+.2} %", d * 100.0)),
            median(&one.rss_kb).map_or("-".into(), |kb| format!("{:.1} MiB", kb / 1024.0)),
            median(&one.events).map_or("-".into(), |e| format!("{e:.0}")),
            one.losses,
            one.throttled,
            band.cell().replace('|', "/"),
        ));
        json_rows.push(json!({
            "source": source,
            "backend": backend,
            "chosen": row.chosen,
            "mode": row.method.mode,
            "emit": row.method.emit,
            "system": row.method.system,
            "workload": row.workload.name,
            "collector_cpu_ns": one.collector,
            "measured_percent": one.percent,
            "elapsed_ns": one.elapsed,
            "bare_elapsed_ns": bare_elapsed.get(source),
            "omn_peak_rss_kb": one.rss_kb,
            "events": one.events,
            "losses": one.losses,
            "throttled": one.throttled,
            "declared": row.declared.as_str(),
            "band": band.cell(),
        }));
    }
    if !flagged.is_empty() {
        table.push_str(&format!(
            "\n## Over the band the catalog declares\n\n{}\n",
            flagged
                .iter()
                .map(|line| format!("- {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }

    table.push_str("\n## The resolver's choices\n\n| source | verdict |\n|---|---|\n");
    let mut corrected = Vec::new();
    for (source, measured) in &verdicts {
        let line = match verdict(measured) {
            Verdict::Alone => continue,
            Verdict::Confirmed(why) => format!(
                "confirmed{}",
                if why.is_empty() {
                    String::new()
                } else {
                    format!(": {why}")
                }
            ),
            Verdict::Corrected(why) => {
                corrected.push(format!("{source}: {why}"));
                format!("**corrected**: {why}")
            }
        };
        table.push_str(&format!("| {source} | {line} |\n"));
    }
    table.push_str("\n## Not measured here\n\n| source | backend | why |\n|---|---|---|\n");
    for one in &unplanned {
        table.push_str(&format!(
            "| {} | {} | {} |\n",
            one.source,
            one.backend,
            one.why.replace('|', "/")
        ));
    }
    for (source, why) in &excluded {
        table.push_str(&format!(
            "| {source} | every backend | the gate excluded its workload: {} |\n",
            why.replace('|', "/")
        ));
    }

    std::fs::write(dir.join("table.md"), &table).unwrap();
    std::fs::write(
        dir.join("table.json"),
        serde_json::to_string_pretty(&json!({
            "smoke": smoke,
            "gate": gate.as_ref().err().map(|refusal| refusal.to_string()),
            "repetitions": repetitions,
            "environment": environment_record(&omn),
            "rows": json_rows,
            "not_measured": unplanned.iter().map(|u| json!({"source": u.source, "backend": u.backend, "why": u.why})).collect::<Vec<Value>>(),
            "excluded_by_the_gate": excluded.iter().map(|(source, why)| json!({"source": source, "why": why})).collect::<Vec<Value>>(),
        }))
        .unwrap(),
    )
    .unwrap();
    eprintln!("{table}");
    say(format!("written to {}", dir.display()));
    for correction in &corrected {
        say(format!(
            "the resolver is corrected by measurement: {correction}"
        ));
    }
    for line in &flagged {
        say(format!("over its declared band, and flagged: {line}"));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The rows measure something: three forced backends against counts the
/// workload knows exactly. Not behind the gate, because a count is not a
/// duration and load does not change it.
///
/// - `proc.syscall` on 200 000 getppid calls, filtered to that syscall by name;
/// - `proc.exec` on a workload that execs exactly 200 children, counted here by
///   the `filename` each event carries;
/// - `mem.heap.alloc` on 20 000 64-byte allocations, few enough that the
///   shim's pipe keeps up; the cost table's million loses most of theirs,
///   and says so.
///
/// Two of the three had to work around the tool, and each workaround is a
/// finding filed under stream X in `docs/open-findings.md`: `syscall == 110`
/// is pushed into the program as getppid and then drops every call it kept,
/// and `proc.exec` accepts a filter on `filename` or `comm` and applies
/// neither.
/// How a case reads its count out of a run.
type Count<'a> = &'a dyn Fn(&Traced) -> u64;

#[test]
#[ignore = "launches the tool: run through scripts/bench.sh"]
fn the_forced_backends_count_what_the_workload_did() {
    let omn = Omn::locate();
    let spawned = spawn_path().expect("the spawn link");
    let method = |source: &str, backend: &str, emit: &str, filter: Option<&str>| Method {
        source: source.into(),
        backend: Some(backend.into()),
        mode: "trace".into(),
        emit: emit.into(),
        filter: filter.map(str::to_owned),
        system: false,
    };
    let summary = |traced: &Traced| traced.summary_count().unwrap_or(0);
    let execs_of_the_link = |traced: &Traced| {
        let link = spawn_path().unwrap();
        traced
            .raw_events()
            .into_iter()
            .filter(|event| attr(event, "filename").and_then(Value::as_str) == link.to_str())
            .count() as u64
    };
    let cases: [(Method, Workload, Count); 3] = [
        (
            method(
                "proc.syscall",
                "bpf:raw_tp",
                "summary",
                Some("syscall == \"getppid\""),
            ),
            Workload::new("syscall", &["syscall", "200000"], 1),
            &summary,
        ),
        (
            // System-wide: `proc.exec` follows no launch. The children exec a
            // hard link only this test process names, because
            // `/proc/self/exe` counted 266 of 200: other programs on this
            // machine exec it too.
            Method {
                system: true,
                ..method("proc.exec", "bpf:raw_tp", "raw", None)
            },
            Workload::new("launch", &["spawn", "200"], 1),
            &execs_of_the_link,
        ),
        (
            method("mem.heap.alloc", "preload", "summary", Some("bytes == 64")),
            Workload::new("malloc", &["async-malloc", "1", "20000"], 1),
            &summary,
        ),
    ];
    say(format!("the spawn children exec {}", spawned.display()));
    let mut checked = 0;
    for (method, workload, count) in cases {
        if let Err(refused) = resolves(&omn, &method) {
            say(format!(
                "SKIPPED {}: not served here -- {}",
                method.label(),
                refused.lines().next().unwrap_or("")
            ));
            continue;
        }
        let traced = run_traced(&omn, &method, &workload)
            .unwrap_or_else(|error| panic!("{}: {error}", method.label()));
        let expected = traced.run.get("ops").expect("the workload's count");
        let counted = count(&traced);
        say(format!(
            "{}: the workload did {expected}, the tool counted {counted}, lost {}",
            method.label(),
            traced.losses()
        ));
        if traced.losses() > 0 {
            assert!(
                counted <= expected,
                "{}: a lossy run may count less, never more: {counted} > {expected}",
                method.label()
            );
        } else {
            assert_eq!(counted, expected, "{}\n{}", method.label(), traced.stderr);
        }
        checked += 1;
    }
    assert!(checked > 0, "no case could be served on this machine");
}
