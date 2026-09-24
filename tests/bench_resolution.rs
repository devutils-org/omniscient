//! Effective resolution: are two events X us apart told apart? (`ROADMAP.md`
//! phase 8, task 5.)
//!
//! The workload produces pairs of marker events -- two signals, two marked
//! allocations, two first touches of a page -- with a busy wait of X between
//! them, and stamps each on `CLOCK_MONOTONIC` immediately before it happens.
//! The tool's timestamps of the same events are compared with those stamps
//! pair by pair. The comparison is with the gap each pair *actually* had, not
//! the one asked for, so a pair the scheduler stretched is still judged
//! correctly: that is why this sweep, unlike the cost table, does not stand
//! behind the variance gate. Load moves the gaps; it does not move the
//! difference between two clocks reading the same two moments.
//!
//! `#[ignore]`: it launches the tool about a hundred times. Run it through
//! `scripts/bench.sh resolution`.

#[path = "../examples/bench/harness.rs"]
mod harness;

use harness::*;
use serde_json::Value;

/// The sweep, 0.5 to 50 us, as the roadmap asks.
const GAPS_NS: [u64; 7] = [500, 1_000, 2_000, 5_000, 10_000, 20_000, 50_000];

/// Pairs per gap. Forty gives the 95 % criterion a resolution of one pair in
/// forty, and keeps the whole sweep under a minute per backend.
const PAIRS: u64 = 40;

/// One backend that timestamps every event, and how its marker events are
/// picked out of everything else it reports.
struct Probe {
    workload: &'static str,
    method: Method,
    marker: fn(&Value, &Value) -> bool,
}

fn number(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_u64)
}

fn probes() -> Vec<Probe> {
    let method = |source: &str, backend: &str, mode: &str| Method {
        source: source.into(),
        backend: Some(backend.into()),
        mode: mode.into(),
        emit: "raw".into(),
        filter: None,
        system: false,
    };
    vec![
        Probe {
            workload: "pairs-signal",
            method: method("proc.signals", "bpf:raw_tp", "trace"),
            marker: |event, report| {
                number(attr(event, "sig")) == Some(libc::SIGUSR1 as u64)
                    && number(attr(event, "pid")) == report.get("pid").and_then(Value::as_u64)
            },
        },
        Probe {
            workload: "pairs-malloc",
            method: method("mem.heap.alloc", "preload", "trace"),
            marker: |event, report| {
                attr(event, "op").and_then(Value::as_str) == Some("alloc")
                    && [report.get("mark_first"), report.get("mark_second")]
                        .iter()
                        .any(|mark| number(attr(event, "bytes")) == mark.and_then(|m| m.as_u64()))
            },
        },
        Probe {
            workload: "pairs-fault",
            method: method("mem.faults", "pmu:sw", "trace"),
            marker: |event, report| {
                let (Some(region), Some(page), Some(ops), Some(addr)) = (
                    number(report.get("region")),
                    number(report.get("page")),
                    number(report.get("ops")),
                    number(attr(event, "addr")),
                ) else {
                    return false;
                };
                (region..region + ops * page).contains(&addr)
            },
        },
    ]
}

/// One gap, one backend: the workload run once under the tool.
fn measure(omn: &Omn, probe: &Probe, gap: u64) -> Result<(Resolved, u64), String> {
    let workload = Workload::new(
        probe.workload,
        &[probe.workload, &gap.to_string(), &PAIRS.to_string()],
        1,
    );
    let traced = run_traced(omn, &probe.method, &workload)?;
    let report = &traced.run.report;
    let stamps: Vec<u64> = report
        .get("stamps")
        .and_then(Value::as_array)
        .ok_or("the workload reported no stamps")?
        .iter()
        .filter_map(Value::as_u64)
        .collect();
    let tool: Vec<u64> = traced
        .raw_events()
        .into_iter()
        .filter(|event| (probe.marker)(event, report))
        .filter_map(|event| event.get("ts_ns").and_then(Value::as_u64))
        .collect();
    // An event cannot precede the stamp taken before it; 20 us of slack
    // before the first covers a timestamp the kernel took on another cpu's
    // view of the same clock. After it, anything inside the 2 ms between
    // pairs belongs to this one.
    let pairs = match_pairs(&stamps, &tool, 20_000, 1_500_000);
    Ok((Resolved::from_pairs(gap, &pairs), traced.losses()))
}

#[test]
#[ignore = "launches the tool about a hundred times: run through scripts/bench.sh"]
fn every_timestamping_backend_resolves_fifty_microseconds_and_the_sweep_says_how_far_below() {
    let omn = Omn::locate();
    say(format!("measuring {}", omn.describe()));
    let dir = run_dir("resolution");
    let mut table = String::from(
        "| source | backend | gap | pairs | seen | apart | median error | median bias | resolves |\n\
         |---|---|---|---|---|---|---|---|---|\n",
    );
    let mut summary = Vec::new();
    let mut failures = Vec::new();
    for probe in probes() {
        if let Err(refused) = resolves(&omn, &probe.method) {
            say(format!(
                "SKIPPED {}: this machine cannot serve it -- {}",
                probe.method.label(),
                refused.lines().next().unwrap_or("")
            ));
            summary.push(format!(
                "| {} | {} | not served here | {} |",
                probe.method.source,
                probe.method.backend.as_deref().unwrap_or(""),
                refused.lines().next().unwrap_or("")
            ));
            continue;
        }
        let mut sweep = Vec::new();
        let mut lost = 0;
        for gap in GAPS_NS {
            let (resolved, losses) = measure(&omn, &probe, gap)
                .unwrap_or_else(|error| panic!("{} at {gap} ns: {error}", probe.method.label()));
            lost += losses;
            table.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
                probe.method.source,
                probe.method.backend.as_deref().unwrap_or(""),
                span(gap as f64),
                resolved.pairs,
                resolved.seen,
                resolved.distinguished,
                resolved
                    .median_error_ns
                    .map(span)
                    .unwrap_or_else(|| "-".into()),
                resolved
                    .median_bias_ns
                    .map(|bias| format!("{}{}", if bias < 0.0 { "-" } else { "+" }, span(bias)))
                    .unwrap_or_else(|| "-".into()),
                if resolved.resolves() { "yes" } else { "no" }
            ));
            sweep.push(resolved);
        }
        let effective = effective_resolution(&sweep);
        say(format!(
            "{}: effective resolution {}",
            probe.method.label(),
            effective.map_or("none in 0.5-50 us".into(), |ns| span(ns as f64))
        ));
        summary.push(format!(
            "| {} | {} | {} | {} |",
            probe.method.source,
            probe.method.backend.as_deref().unwrap_or(""),
            effective.map_or("none in 0.5-50 us".into(), |ns| span(ns as f64)),
            if lost > 0 {
                format!("{lost} events lost over the sweep")
            } else {
                "nothing lost".into()
            }
        ));
        let widest = sweep.last().expect("a sweep");
        if !widest.resolves() {
            if lost > 0 {
                say(format!(
                    "{} did not resolve 50 us, and lost {lost} events saying so: a lossy run is \
                     reported, not judged",
                    probe.method.label()
                ));
            } else {
                failures.push(format!("{}: {widest:?}", probe.method.label()));
            }
        }
    }
    let document = format!(
        "# Effective resolution\n\nMeasured with {}.\n\n| source | backend | effective \
         resolution | losses |\n|---|---|---|---|\n{}\n\n{table}",
        omn.describe(),
        summary.join("\n")
    );
    std::fs::write(dir.join("resolution.md"), &document).unwrap();
    eprintln!("{document}");
    say(format!("written to {}", dir.display()));
    assert!(
        failures.is_empty(),
        "a backend that timestamps every event must tell two events 50 us apart:\n{}",
        failures.join("\n")
    );
}
