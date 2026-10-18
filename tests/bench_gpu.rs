//! What a GPU tracer costs, and the GPU bands held to it.
//!
//! The measurement (`#[ignore]`, minutes on the device): every `gpu`
//! (source, backend) this build serves for a launched program, on three
//! workload regimes (`examples/bench/gpu.c`), round-robin with the workload
//! bare and with the CUDA injection library loaded and disarmed -- the
//! negative control. `nsys` rides along as a reference reader when it is
//! installed, measured and never served. It writes `table.md` and
//! `table.json` under `target/bench/gpu-<stamp>/`, and the block to paste
//! into `docs/bench.md` between the `gpu-bands` markers.
//!
//! The rest runs in `make check` and measures nothing: the rule each row is
//! judged by, on synthetic rows, and the record in `docs/bench.md` held to
//! the catalog -- the same way `tests/bench_bands.rs` holds the backends
//! record. A GPU band cannot leave its measurement without a test failing,
//! and a GPU tracer this build serves cannot go unmeasured without its row
//! saying why.

#[path = "../examples/bench/harness.rs"]
mod harness;

#[path = "../examples/bench/gpu.rs"]
#[allow(dead_code)]
mod gpu;

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;
use std::time::Instant;

use gpu::*;
use harness::{on_gpu_workloads, repetitions, round_robin, say, Omn, MIN_REPETITIONS, WARMUP};
use omn::catalog::{Catalog, Cost};
use serde_json::{json, Value};

const BENCH_MD: &str = include_str!("../docs/bench.md");

// ------------------------------------------------------- the measurement

#[test]
#[ignore = "measures the GPU for several minutes: run it alone, on a quiet device"]
fn every_gpu_tracer_is_costed_against_a_negative_control() {
    if let Err(reason) = gpu_ready() {
        eprintln!("skipped: {reason}");
        return;
    }
    let started = Instant::now();
    let omn = Omn::locate();
    let repetitions = repetitions();
    let smoke = repetitions < MIN_REPETITIONS;
    say(format!("measuring {}", omn.describe()));
    let before = device_state();
    say(format!("the device before: {before}"));

    let library = injection_library(&omn);
    let (planned, unplanned) = gpu_plan(&omn);
    for pair in &unplanned {
        say(format!(
            "not measured: {} through {}: {}",
            pair.source, pair.backend, pair.why
        ));
    }
    let nsys = which("nsys");
    let mut arms = vec![Arm::Bare];
    if let Some(library) = &library {
        arms.push(Arm::Control(library.clone()));
    } else {
        say("no injection library beside the binary: no control, and no vendor:cupti row");
    }
    arms.extend(planned.iter().map(|pair| Arm::Served(pair.method.clone())));
    if let Some(nsys) = &nsys {
        arms.push(Arm::Nsys(nsys.clone()));
    }
    say(format!(
        "{} arms on {} workloads, {repetitions} repetitions each after {WARMUP} warmups",
        arms.len(),
        gpu_workloads().len()
    ));

    let workloads = gpu_workloads();
    let mut series: BTreeMap<(usize, &str), Series> = BTreeMap::new();
    // The bare run and the control at a workload's `replayed` size, which
    // the replaying arms are measured against.
    let mut scaled_series: BTreeMap<(usize, &str), Series> = BTreeMap::new();
    let mut failures: BTreeMap<String, String> = BTreeMap::new();
    let replaying = arms.iter().any(Arm::replays);
    for workload in &workloads {
        let scaled = workload.scaled().filter(|_| replaying);
        let full: Vec<usize> = (0..arms.len())
            .filter(|&index| scaled.is_none() || !arms[index].replays())
            .collect();
        for (index, measured) in measure(&omn, &arms, &full, workload, repetitions, &mut failures) {
            series.insert((index, workload.name), measured);
        }
        if let Some(scaled) = &scaled {
            let subset: Vec<usize> = (0..arms.len())
                .filter(|&index| {
                    arms[index].replays() || matches!(arms[index], Arm::Bare | Arm::Control(_))
                })
                .collect();
            for (index, measured) in
                measure(&omn, &arms, &subset, scaled, repetitions, &mut failures)
            {
                if arms[index].replays() {
                    series.insert((index, workload.name), measured);
                } else {
                    scaled_series.insert((index, workload.name), measured);
                }
            }
        }
        say(format!("{} done after {}", workload.name, since(started)));
    }
    let after = device_state();

    let empty = Series::default();
    let control_index = arms.iter().position(|arm| matches!(arm, Arm::Control(_)));
    let mut rows = Vec::new();
    let mut baselines = Vec::new();
    let mut references = Vec::new();
    for workload in &workloads {
        let bare = series.get(&(0, workload.name)).unwrap_or(&empty);
        let control = control_index.and_then(|index| series.get(&(index, workload.name)));
        let resolution = workload_resolution(bare, control);
        baselines.push(json!({
            "workload": workload.name,
            "regime": workload.regime,
            "resolves_percent": resolution,
            "bare_elapsed_ms": stats_ms(&bare.elapsed),
            "bare_init_ms": stats_ms(&bare.init),
            "bare_others_cpu_ms": stats_ms(&bare.others_cpu),
            "control_elapsed_dilation": control.and_then(|c| dilation(&c.elapsed, &bare.elapsed)),
            "control_cpu_dilation": control.and_then(|c| dilation(&c.section_cpu, &bare.section_cpu)),
            "control_thread_cpu_dilation": control.and_then(|c| dilation(&c.thread_cpu, &bare.thread_cpu)),
            "control_others_cpu_ms": control.and_then(|c| difference_ms(&c.others_cpu, &bare.others_cpu)),
            "control_init_ms": control.and_then(|c| difference_ms(&c.init, &bare.init)),
        }));
        for (offset, pair) in planned.iter().enumerate() {
            let index = arms
                .iter()
                .position(|arm| *arm == Arm::Served(pair.method.clone()))
                .unwrap_or(offset);
            let Some(measured) = series.get(&(index, workload.name)) else {
                continue;
            };
            // A replaying arm ran the workload's smaller size, and is held to
            // the bare run and the control of that size.
            let scaled_bare = arms[index]
                .replays()
                .then(|| scaled_series.get(&(0, workload.name)))
                .flatten();
            let row = match scaled_bare {
                Some(scaled_bare) => {
                    let scaled_control =
                        control_index.and_then(|index| scaled_series.get(&(index, workload.name)));
                    let resolution = workload_resolution(scaled_bare, scaled_control);
                    row(
                        pair,
                        workload.name,
                        measured,
                        scaled_bare,
                        scaled_control,
                        resolution,
                    )
                }
                None => row(pair, workload.name, measured, bare, control, resolution),
            };
            note_row(&row);
            rows.push(row);
        }
        if let Some(index) = arms.iter().position(|arm| matches!(arm, Arm::Nsys(_))) {
            if let Some(measured) = series.get(&(index, workload.name)) {
                references.push(json!({
                    "workload": workload.name,
                    "dilation": dilation(&measured.elapsed, &bare.elapsed),
                    "cpu_dilation": dilation(&measured.section_cpu, &bare.section_cpu),
                    "thread_cpu_dilation": dilation(&measured.thread_cpu, &bare.thread_cpu),
                    "others_cpu_ms": difference_ms(&measured.others_cpu, &bare.others_cpu),
                    "init_ms": difference_ms(&measured.init, &bare.init),
                    "exit_ms": difference_ms(&measured.exit, &bare.exit),
                }));
            }
        }
    }

    let dir = harness::run_dir("gpu");
    let table = render_table(
        &omn,
        &baselines,
        &rows,
        &references,
        &failures,
        &before,
        &after,
        repetitions,
    );
    std::fs::write(dir.join("table.md"), &table).expect("table.md");
    let record = render_record(&dir, &baselines, &rows);
    std::fs::write(dir.join("record.md"), &record).expect("record.md");
    let document = json!({
        "environment": harness::environment_record(&omn),
        "device_before": before,
        "device_after": after,
        "repetitions": repetitions,
        "baselines": baselines,
        "rows": rows.iter().map(row_json).collect::<Vec<_>>(),
        "references": references,
        "series": raw_series(&arms, &series),
        "scaled_series": raw_series(&arms, &scaled_series),
        "failures": failures,
        "unplanned": unplanned,
    });
    std::fs::write(
        dir.join("table.json"),
        serde_json::to_string_pretty(&document).expect("json"),
    )
    .expect("table.json");
    eprintln!("{table}");
    eprintln!("{record}");
    say(format!("wrote {} after {}", dir.display(), since(started)));

    let mut wrong = gpu_table_failures(&planned, &unplanned, &rows, workloads.len());
    for ((index, workload), measured) in series.iter().chain(&scaled_series) {
        wrong.extend(series_failures(&arms[*index], workload, measured));
    }
    wrong.extend(
        failures
            .iter()
            .map(|(what, error)| format!("{what} failed: {}", harness::first_line(error))),
    );
    if smoke {
        say(format!(
            "SMOKE RUN ({repetitions} repetitions): nothing written is a measurement"
        ));
    }
    assert!(
        wrong.is_empty(),
        "the GPU table is wrong:\n{}",
        wrong.join("\n")
    );
}

fn which(program: &str) -> Option<std::path::PathBuf> {
    let output = Command::new("sh")
        .args(["-c", &format!("command -v {program}")])
        .output()
        .ok()?;
    let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (output.status.success() && !path.is_empty()).then(|| path.into())
}

/// Every repetition of every arm, so a table can be judged again by
/// another rule without measuring again.
/// Warms up and measures `subset` of the arms on `workload`, round robin:
/// each arm's series, by its index in `arms`. A failed run is kept in
/// `failures` under the arm and the workload.
fn measure(
    omn: &Omn,
    arms: &[Arm],
    subset: &[usize],
    workload: &GpuWorkload,
    repetitions: usize,
    failures: &mut BTreeMap<String, String>,
) -> BTreeMap<usize, Series> {
    let mut series: BTreeMap<usize, Series> = BTreeMap::new();
    let mut failed = |index: usize, error: String| {
        failures.insert(
            format!(
                "{} on {} ({})",
                arms[index].label(),
                workload.name,
                workload.args.join(" ")
            ),
            error,
        );
    };
    for &index in subset {
        for _ in 0..WARMUP {
            if let Err(error) = run_gpu(omn, &arms[index], workload) {
                failed(index, error);
            }
        }
    }
    for slot in round_robin(subset.len(), repetitions) {
        let index = subset[slot];
        match run_gpu(omn, &arms[index], workload) {
            Ok(run) => series.entry(index).or_default().push(&run),
            Err(error) => failed(index, error),
        }
    }
    series
}

fn raw_series(arms: &[Arm], series: &BTreeMap<(usize, &str), Series>) -> Value {
    series
        .iter()
        .map(|((index, workload), series)| {
            json!({
                "arm": arms[*index].label(),
                "workload": workload,
                "elapsed_ns": series.elapsed,
                "section_cpu_ns": series.section_cpu,
                "section_thread_cpu_ns": series.thread_cpu,
                "others_cpu_ns": series.others_cpu,
                "threads_on_launch_cpu": series.sharing,
                "injection": series.injection,
                "losses": series.losses,
                "losses_by_kind": series.losses_by_kind,
                "init_ns": series.init,
                "exit_ns": series.exit,
                "share_percent": series.share,
                "collector_cpu_ns": series.collector,
            })
        })
        .collect()
}

fn stats_ms(values: &[f64]) -> Value {
    match harness::Stats::of(values) {
        Some(stats) => {
            json!({"median": stats.median / 1e6, "iqr": stats.iqr() / 1e6, "n": stats.n})
        }
        None => Value::Null,
    }
}

fn row_json(row: &GpuRow) -> Value {
    json!({
        "source": row.source,
        "backend": row.backend,
        "mode": row.mode,
        "workload": row.workload,
        "share_percent": row.share,
        "dilation_percent": row.dilation,
        "cpu_dilation_percent": row.cpu_dilation,
        "thread_cpu_dilation_percent": row.thread_cpu_dilation,
        "others_cpu_ms": row.others_cpu_ms,
        "against": row.against,
        "init_ms": row.init_ms,
        "exit_ms": row.exit_ms,
        "resolves_percent": row.resolution,
        "losses": row.losses,
        "events_total": row.events_total,
        "events": row.events,
        "served_by": row.served_by,
        "declared": row.declared.as_str(),
        "cost": row.cost().map(|(cost, from)| json!({"percent": cost, "from": from})),
    })
}

#[allow(clippy::too_many_arguments)]
fn render_table(
    omn: &Omn,
    baselines: &[Value],
    rows: &[GpuRow],
    references: &[Value],
    failures: &BTreeMap<String, String>,
    before: &Value,
    after: &Value,
    repetitions: usize,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# What a GPU tracer costs\n\n{}; {repetitions} repetitions, round-robin.\n\n",
        omn.describe()
    ));
    out.push_str(&format!(
        "The device before:\n\n```\n{}\n{}\n```\n\nand after:\n\n```\n{}\n{}\n```\n\n",
        before["device"].as_str().unwrap_or("?"),
        before["other_tenants"].as_str().unwrap_or("?"),
        after["device"].as_str().unwrap_or("?"),
        after["other_tenants"].as_str().unwrap_or("?"),
    ));
    out.push_str(
        "| workload | regime | bare section | resolves | control: dilation | control: cpu | \
         control: launching thread | control: other threads | control: init |\n\
         |---|---|---|---|---|---|---|---|---|\n",
    );
    for baseline in baselines {
        let number = |key: &str| baseline.get(key).and_then(Value::as_f64);
        out.push_str(&format!(
            "| {} | {} | {:.1} ms | {:.2} % | {} | {} | {} | {} | {} |\n",
            baseline["workload"].as_str().unwrap_or("?"),
            baseline["regime"].as_str().unwrap_or("?"),
            baseline
                .pointer("/bare_elapsed_ms/median")
                .and_then(Value::as_f64)
                .unwrap_or(f64::NAN),
            number("resolves_percent").unwrap_or(f64::NAN),
            percent_cell(number("control_elapsed_dilation")),
            percent_cell(number("control_cpu_dilation")),
            percent_cell(number("control_thread_cpu_dilation")),
            ms_cell(number("control_others_cpu_ms")),
            ms_cell(number("control_init_ms")),
        ));
    }
    out.push_str(
        "\n| source | backend | mode | workload | % of target | dilation | cpu dilation | \
         launching thread | other threads | against | init | exit | events | losses | declared | band |\n\
         |---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|\n",
    );
    for row in rows {
        let band = match row.cost() {
            Some((cost, from)) if cost <= row.declared.worst_case_percent() => {
                format!("within: {cost:.2} % ({from})")
            }
            Some((cost, from)) => format!(
                "**over**: {cost:.2} % ({from}), `{}` would hold it",
                band_for(cost).as_str()
            ),
            None => "**missing**".into(),
        };
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            row.source,
            row.backend,
            row.mode,
            row.workload,
            share_cell(row.share),
            percent_cell(row.dilation),
            percent_cell(row.cpu_dilation),
            percent_cell(row.thread_cpu_dilation),
            ms_cell(row.others_cpu_ms),
            row.against,
            ms_cell(row.init_ms),
            ms_cell(row.exit_ms),
            row.events
                .map_or("-".into(), |events| format!("{events:.0}")),
            row.losses,
            row.declared.as_str(),
            band
        ));
    }
    if !references.is_empty() {
        out.push_str(
            "\n`nsys profile --trace=cuda`, against bare, measured and never served:\n\n\
             | workload | dilation | cpu dilation | launching thread | other threads | init | exit |\n\
             |---|---|---|---|---|---|---|\n",
        );
        for reference in references {
            let number = |key: &str| reference.get(key).and_then(Value::as_f64);
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} |\n",
                reference["workload"].as_str().unwrap_or("?"),
                percent_cell(number("dilation")),
                percent_cell(number("cpu_dilation")),
                percent_cell(number("thread_cpu_dilation")),
                ms_cell(number("others_cpu_ms")),
                ms_cell(number("init_ms")),
                ms_cell(number("exit_ms")),
            ));
        }
    }
    if !failures.is_empty() {
        out.push_str("\nFailed runs:\n\n");
        for (what, error) in failures {
            out.push_str(&format!("- {what}: {}\n", harness::first_line(error)));
        }
    }
    out
}

/// The block pasted into `docs/bench.md`, with `was` and `declared` both the
/// catalog's band today and the verdict the measurement gives it: the paste
/// is where a person corrects the band and writes `corrected`.
fn render_record(dir: &std::path::Path, baselines: &[Value], rows: &[GpuRow]) -> String {
    let name = dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut out = format!("<!-- gpu-bands:begin {name} rows={} -->\n", rows.len());
    out.push_str("| workload | regime | resolves |\n|---|---|---|\n");
    for baseline in baselines {
        out.push_str(&format!(
            "| {} | {} | {:.2} % |\n",
            baseline["workload"].as_str().unwrap_or("?"),
            baseline["regime"].as_str().unwrap_or("?"),
            baseline["resolves_percent"].as_f64().unwrap_or(f64::NAN),
        ));
    }
    out.push_str(
        "\n| source | backend | mode | workload | % of target | dilation | cpu dilation | against | losses | was | declared | verdict |\n\
         |---|---|---|---|---|---|---|---|---|---|---|---|\n",
    );
    for row in rows {
        let verdict = match row.cost() {
            Some((cost, _)) if cost <= row.declared.worst_case_percent() => "within".to_owned(),
            Some((cost, _)) => format!("over: `{}` would hold it", band_for(cost).as_str()),
            None => "missing".into(),
        };
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} of {} | {} | {} | {} |\n",
            row.source,
            row.backend,
            row.mode,
            row.workload,
            share_cell(row.share),
            percent_cell(row.dilation),
            percent_cell(row.cpu_dilation),
            row.against,
            row.losses,
            row.events_total,
            row.declared.as_str(),
            row.declared.as_str(),
            verdict
        ));
    }
    out.push_str("<!-- gpu-bands:end -->\n");
    out
}

// ------------------------------------------------------------ the record

/// One row of the GPU record.
#[derive(Debug)]
struct Recorded {
    source: String,
    backend: String,
    workload: String,
    share: Option<f64>,
    dilation: Option<f64>,
    cpu_dilation: Option<f64>,
    /// Records the tracer dropped over every repetition, of every event it
    /// saw (`N of M`); `None` (`-`) on a pending row only.
    losses: Option<(u64, u64)>,
    /// `None` (`-`) on a pending row only.
    was: Option<Cost>,
    declared: Option<Cost>,
    verdict: String,
}

struct GpuRecord {
    expected_rows: Option<usize>,
    resolves: BTreeMap<String, f64>,
    rows: Vec<Recorded>,
}

fn cost(name: &str) -> Cost {
    match name {
        "free" => Cost::Free,
        "low" => Cost::Low,
        "med" => Cost::Med,
        "high" => Cost::High,
        "extreme" => Cost::Extreme,
        other => panic!("`{other}` is not a cost band"),
    }
}

fn percent(cell: &str) -> Option<f64> {
    if cell == "-" {
        return None;
    }
    let number = cell
        .strip_suffix('%')
        .unwrap_or_else(|| panic!("`{cell}` is not a percentage"))
        .trim();
    Some(
        number
            .parse()
            .unwrap_or_else(|_| panic!("`{cell}` is not a percentage")),
    )
}

fn cells(line: &str) -> Vec<&str> {
    line.trim()
        .trim_matches('|')
        .split('|')
        .map(str::trim)
        .collect()
}

fn parse(text: &str) -> GpuRecord {
    let begin = text
        .find("<!-- gpu-bands:begin")
        .expect("docs/bench.md has no `gpu-bands:begin` marker");
    let end = text[begin..]
        .find("<!-- gpu-bands:end -->")
        .expect("docs/bench.md has no `gpu-bands:end` marker after its `gpu-bands:begin`")
        + begin;
    let marker = text[begin..].lines().next().unwrap_or_default();
    let expected_rows = marker
        .split_whitespace()
        .find_map(|word| word.strip_prefix("rows="))
        .map(|count| {
            count
                .parse()
                .unwrap_or_else(|_| panic!("`rows={count}` is not a count"))
        });
    let mut resolves = BTreeMap::new();
    let mut rows = Vec::new();
    for line in text[begin..end].lines().skip(1) {
        let line = line.trim();
        if !line.starts_with('|') || line.starts_with("|---") {
            continue;
        }
        let cells = cells(line);
        match cells.len() {
            3 if cells[0] != "workload" => {
                resolves.insert(
                    cells[0].to_owned(),
                    percent(cells[2]).expect("a workload's resolution"),
                );
            }
            12 if cells[0] != "source" => rows.push(Recorded {
                source: cells[0].to_owned(),
                backend: cells[1].to_owned(),
                workload: cells[3].to_owned(),
                share: percent(cells[4]),
                dilation: percent(cells[5]),
                cpu_dilation: percent(cells[6]),
                losses: (cells[8] != "-").then(|| {
                    let count = |text: &str| -> u64 {
                        text.trim()
                            .parse()
                            .unwrap_or_else(|_| panic!("`{}` is not `<lost> of <seen>`", cells[8]))
                    };
                    let (lost, seen) = cells[8]
                        .split_once(" of ")
                        .unwrap_or_else(|| panic!("`{}` is not `<lost> of <seen>`", cells[8]));
                    (count(lost), count(seen))
                }),
                was: (cells[9] != "-").then(|| cost(cells[9])),
                declared: (cells[10] != "-").then(|| cost(cells[10])),
                verdict: cells[11].to_owned(),
            }),
            3 | 12 => {}
            other => panic!("a GPU record line with {other} cells: {line}"),
        }
    }
    GpuRecord {
        expected_rows,
        resolves,
        rows,
    }
}

fn pending(row: &Recorded) -> bool {
    row.verdict.starts_with("pending:")
}

fn label(row: &Recorded) -> String {
    format!("{} through {} on {}", row.source, row.backend, row.workload)
}

/// What is wrong with the record as a record: its row count, a workload with
/// no resolution, a verdict that is not one of the four, a reason left empty,
/// a pending row that carries a measurement or a measured one that carries
/// none, a measured row with losses (or no count of them), and a pair
/// measured on some regimes and not on others.
fn record_faults(record: &GpuRecord) -> Vec<String> {
    let mut faults = Vec::new();
    if record.expected_rows != Some(record.rows.len()) {
        faults.push(format!(
            "the `gpu-bands:begin` marker says rows={:?} and the record holds {}",
            record.expected_rows,
            record.rows.len()
        ));
    }
    if record.rows.is_empty() {
        faults.push("the GPU record holds no row".to_owned());
    }
    let mut regimes: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for row in &record.rows {
        let measured = row.share.is_some()
            || row.dilation.is_some()
            || row.cpu_dilation.is_some()
            || row.losses.is_some();
        let reason = row
            .verdict
            .split_once(':')
            .map(|(_, reason)| reason.trim())
            .unwrap_or("");
        let known = ["within", "corrected"].contains(&row.verdict.as_str())
            || row.verdict.starts_with("stands")
            || pending(row);
        if !known {
            faults.push(format!(
                "{}: `{}` is not a verdict",
                label(row),
                row.verdict
            ));
        } else if (row.verdict.starts_with("stands") || pending(row)) && reason.is_empty() {
            faults.push(format!("{}: `{}` gives no reason", label(row), row.verdict));
        }
        if pending(row) {
            if measured {
                faults.push(format!(
                    "{} is pending and carries a measurement",
                    label(row)
                ));
            }
            continue;
        }
        if !measured {
            faults.push(format!(
                "{} carries no measurement and is not pending",
                label(row)
            ));
        }
        if row.was.is_none() || row.declared.is_none() {
            faults.push(format!("{} gives no band and is not pending", label(row)));
        }
        match row.losses {
            None => faults.push(format!(
                "{} gives no count of losses and is not pending",
                label(row)
            )),
            Some((lost, seen)) if lossy(lost, seen) => faults.push(format!(
                "{} lost {lost} of {seen} records, more than one in {:.0}: a tracer that \
                 drops records understates its own cost, and its row cannot hold a band",
                label(row),
                1.0 / MAX_LOSS_SHARE
            )),
            Some(_) => {}
        }
        if !record.resolves.contains_key(&row.workload) {
            faults.push(format!(
                "{}: the record gives no resolution for `{}`",
                label(row),
                row.workload
            ));
        }
        regimes
            .entry((row.source.clone(), row.backend.clone()))
            .or_default()
            .insert(row.workload.clone());
    }
    let all: BTreeSet<String> = record.resolves.keys().cloned().collect();
    for ((source, backend), measured) in regimes {
        if measured != all {
            faults.push(format!(
                "{source} through {backend} is measured on {measured:?}, not on every regime \
                 the record has ({all:?})"
            ));
        }
    }
    faults
}

/// The record's rule for one row: `judged_cost`, on the record's columns and
/// its workload's resolution.
fn recorded_cost(record: &GpuRecord, row: &Recorded) -> Option<f64> {
    let resolution = *record.resolves.get(&row.workload)?;
    judged_cost(row.share, row.dilation, row.cpu_dilation, resolution).map(|(cost, _)| cost)
}

/// What is wrong between the record and the catalog: a row whose `declared`
/// is not the catalog's band, a pair whose worst regime is over its band with
/// no `stands` reason, a `stands` reason no longer needed, a `corrected` with
/// no change of band, and a `within` whose band changed.
fn band_faults(record: &GpuRecord, catalog: &Catalog) -> Vec<String> {
    let mut faults = Vec::new();
    let mut worst: BTreeMap<(String, String), (f64, String)> = BTreeMap::new();
    // A pending row measured nothing and holds no band: what it may name is
    // a collector that has not landed, under a backend the catalog does not
    // have yet. `record_faults` holds what it must carry.
    let rows: Vec<&Recorded> = record.rows.iter().filter(|row| !pending(row)).collect();
    for row in rows.iter().copied() {
        let (Some(was), Some(recorded)) = (row.was, row.declared) else {
            continue;
        };
        let declared = find(catalog, &row.source)
            .and_then(|entry| entry.backends.iter().find(|b| b.id == row.backend))
            .map(|backend| backend.cost);
        match declared {
            None => faults.push(format!("{}: the catalog has no such backend", label(row))),
            Some(band) if band != recorded => faults.push(format!(
                "{}: the record says `{}` and the catalog declares `{}`",
                label(row),
                recorded.as_str(),
                band.as_str()
            )),
            Some(_) => {}
        }
        if row.verdict == "corrected" && was == recorded {
            faults.push(format!(
                "{} says `corrected` and its band did not change",
                label(row)
            ));
        }
        if row.verdict == "within" && was != recorded {
            faults.push(format!(
                "{} says `within` and its band moved from `{}` to `{}`: that is `corrected`",
                label(row),
                was.as_str(),
                recorded.as_str()
            ));
        }
        if let Some(cost) = recorded_cost(record, row) {
            let key = (row.source.clone(), row.backend.clone());
            if worst.get(&key).is_none_or(|(kept, _)| cost > *kept) {
                worst.insert(key, (cost, row.workload.clone()));
            }
        }
    }
    for ((source, backend), (cost, workload)) in worst {
        let rows: Vec<&&Recorded> = rows
            .iter()
            .filter(|row| row.source == source && row.backend == backend)
            .collect();
        let Some(declared) = rows[0].declared else {
            continue;
        };
        let stands = rows.iter().any(|row| row.verdict.starts_with("stands"));
        let over = cost > declared.worst_case_percent();
        if over && !stands {
            faults.push(format!(
                "{source} through {backend} costs {cost:.2} % on `{workload}`, over `{}`'s {} %, \
                 and no row says why it stands",
                declared.as_str(),
                declared.worst_case_percent()
            ));
        }
        if !over && stands {
            faults.push(format!(
                "{source} through {backend} is within `{}` on every regime, so its `stands` \
                 reason is no longer needed",
                declared.as_str()
            ));
        }
    }
    faults
}

/// Every `gpu` tracer this build serves for a launched program alone -- the
/// pairs `harness::plan` leaves to this bench -- that the record neither
/// measured nor lists as pending.
/// Tracers written and not yet benched, each with why: the bench needs what
/// the machine that wrote them did not have. Each is removed when its row
/// lands, and the coverage rule fails if a row lands while it is still here.
const AWAITING_BENCH: [(&str, &str, &str); 1] = [(
    "gpu.api",
    "vendor:cupti",
    "written in the twenty-fifth wave's finishing pass on a machine without libcupti",
)];

fn coverage_faults(record: &GpuRecord, catalog: &Catalog) -> Vec<String> {
    let recorded: BTreeSet<(&str, &str)> = record
        .rows
        .iter()
        .map(|row| (row.source.as_str(), row.backend.as_str()))
        .collect();
    let mut faults = Vec::new();
    for entry in catalog.entries() {
        if !on_gpu_workloads(entry) {
            continue;
        }
        for backend in &entry.backends {
            let source = entry.id.to_string();
            let awaiting = AWAITING_BENCH
                .iter()
                .any(|(id, road, _)| *id == source && *road == backend.id);
            let in_record = recorded.contains(&(source.as_str(), backend.id.as_str()));
            if awaiting {
                if in_record {
                    faults.push(format!(
                        "{source} through {} has its row now: take it out of AWAITING_BENCH",
                        backend.id
                    ));
                }
                continue;
            }
            if omn::collect::implements(&source, &backend.id) && !in_record {
                faults.push(format!(
                    "{source} through {} has a collector, is measured only on the GPU \
                     workloads, and the GPU record has no row for it",
                    backend.id
                ));
            }
        }
    }
    faults
}

fn find<'a>(catalog: &'a Catalog, id: &str) -> Option<&'a omn::catalog::Entry> {
    catalog
        .entries()
        .iter()
        .find(|entry| entry.id.to_string() == id)
}

fn catalog() -> Catalog {
    Catalog::embedded().expect("the embedded catalog loads")
}

#[test]
fn the_gpu_record_is_whole_and_reads_one_way() {
    let faults = record_faults(&parse(BENCH_MD));
    assert!(faults.is_empty(), "{}", faults.join("\n"));
}

#[test]
fn every_gpu_band_holds_its_worst_regime_or_says_why_it_stands() {
    let faults = band_faults(&parse(BENCH_MD), &catalog());
    assert!(faults.is_empty(), "{}", faults.join("\n"));
}

#[test]
fn every_gpu_tracer_served_on_a_launch_alone_is_in_the_record() {
    let faults = coverage_faults(&parse(BENCH_MD), &catalog());
    assert!(faults.is_empty(), "{}", faults.join("\n"));
}

// ---------------------------------------------------- the rule, shown failing

fn synthetic(rows: &[&str]) -> GpuRecord {
    let mut text = format!(
        "<!-- gpu-bands:begin synthetic rows={} -->\n\
         | workload | regime | resolves |\n|---|---|---|\n\
         | launch | launch-bound | 0.50 % |\n| compute | compute-bound | 0.50 % |\n\n\
         | source | backend | mode | workload | % of target | dilation | cpu dilation | against | losses | was | declared | verdict |\n\
         |---|---|---|---|---|---|---|---|---|---|---|---|\n",
        rows.len()
    );
    for row in rows {
        text.push_str(row);
        text.push('\n');
    }
    text.push_str("<!-- gpu-bands:end -->\n");
    parse(&text)
}

fn says(faults: &[String], what: &str) {
    assert!(
        faults.iter().any(|fault| fault.contains(what)),
        "expected a fault saying `{what}`, got {faults:?}"
    );
}

/// The band `gpu.api` through `preload` declares today, read from the
/// catalog, so the synthetic rows hold whatever it is.
fn api_band() -> &'static str {
    let catalog = catalog();
    let band = find(&catalog, "gpu.api")
        .and_then(|entry| entry.backends.iter().find(|b| b.id == "preload"))
        .map(|backend| backend.cost)
        .expect("gpu.api through preload");
    band.as_str()
}

#[test]
fn the_rule_takes_the_worst_column_and_reads_a_dilation_under_the_resolution_as_zero() {
    assert_eq!(
        judged_cost(Some(0.3), Some(0.4), Some(0.2), 0.5),
        Some((0.3, "share of the target's cpu"))
    );
    assert_eq!(
        judged_cost(Some(0.3), Some(4.0), Some(9.0), 0.5),
        Some((9.0, "cpu dilation"))
    );
    assert_eq!(
        judged_cost(None, Some(-3.0), None, 0.5),
        Some((0.0, "dilation"))
    );
    assert_eq!(judged_cost(None, None, None, 0.5), None);
    assert_eq!(band_for(0.05), Cost::Free);
    assert_eq!(band_for(0.8), Cost::Low);
    assert_eq!(band_for(65.0), Cost::Extreme);
}

#[test]
fn one_held_up_repetition_does_not_move_the_floor() {
    // Twenty repetitions within 1 % of 100 ms, then the same with one a
    // tenant held up for a second: the medians the table compares barely
    // move, and so must the floor that says what they resolve.
    let steady: Vec<f64> = (0..20).map(|i| 100.0e6 + (i % 5) as f64 * 0.25e6).collect();
    let mut held = steady.clone();
    held[7] = 1_100.0e6;
    let (calm, noisy) = (
        resolves_percent(&steady).unwrap(),
        resolves_percent(&held).unwrap(),
    );
    assert!(calm < 1.0, "a 1 % spread resolves under 1 %: {calm}");
    assert!(
        noisy < 2.0 * calm,
        "one outlier moved the floor from {calm} to {noisy}"
    );
    let by_deviation = harness::Stats::of(&held).unwrap().resolution() * 100.0;
    assert!(
        by_deviation > 10.0 * noisy,
        "the deviation's floor: {by_deviation}"
    );
}

#[test]
fn a_gpu_record_wrong_in_each_way_is_named() {
    let band = api_band();
    let ok = |workload: &str| {
        format!("| gpu.api | preload | trace | {workload} | 0.10 % | +0.20 % | +0.10 % | bare | 0 of 1000 | {band} | {band} | within |")
    };
    // Whole, it is clean.
    let clean = synthetic(&[&ok("launch"), &ok("compute")]);
    assert!(
        record_faults(&clean).is_empty(),
        "{:?}",
        record_faults(&clean)
    );
    assert!(
        band_faults(&clean, &catalog()).is_empty(),
        "{:?}",
        band_faults(&clean, &catalog())
    );

    // A regime missing.
    says(
        &record_faults(&synthetic(&[&ok("launch")])),
        "not on every regime",
    );
    // A row count the marker does not say.
    let mut short = synthetic(&[&ok("launch"), &ok("compute")]);
    short.expected_rows = Some(3);
    says(&record_faults(&short), "rows=Some(3)");
    // A verdict that is none of the four, and one without its reason.
    let odd = ok("compute").replace("| within |", "| fine |");
    says(
        &record_faults(&synthetic(&[&ok("launch"), &odd])),
        "is not a verdict",
    );
    let bare = ok("compute").replace("| within |", "| stands: |");
    says(
        &record_faults(&synthetic(&[&ok("launch"), &bare])),
        "gives no reason",
    );
    // A pending row with a number, and a measured row without one.
    let pending =
        "| gpu.api | preload | - | launch | 1.00 % | - | - | - | - | low | low | pending: later |";
    says(
        &record_faults(&synthetic(&[pending])),
        "pending and carries a measurement",
    );
    let empty = ok("compute").replace(
        "0.10 % | +0.20 % | +0.10 % | bare | 0 of 1000",
        "- | - | - | bare | -",
    );
    says(
        &record_faults(&synthetic(&[&ok("launch"), &empty])),
        "carries no measurement",
    );

    // Over its band on one regime, with no reason.
    let over = ok("launch").replace("+0.20 %", "+65.00 %");
    says(
        &band_faults(&synthetic(&[&over, &ok("compute")]), &catalog()),
        "no row says why it stands",
    );
    // ... which a `stands` reason answers, and which an unneeded one is named for.
    let standing = over.replace("| within |", "| stands on purpose: a reason |");
    assert!(band_faults(&synthetic(&[&standing, &ok("compute")]), &catalog()).is_empty());
    let unneeded = ok("launch").replace("| within |", "| stands on purpose: a reason |");
    says(
        &band_faults(&synthetic(&[&unneeded, &ok("compute")]), &catalog()),
        "no longer needed",
    );
    // A dilation inside the resolution is not over anything.
    let noise = ok("launch").replace("0.10 % | +0.20 %", "0.05 % | +0.45 %");
    assert!(
        band_faults(&synthetic(&[&noise, &ok("compute")]), &catalog())
            .iter()
            .all(|fault| !fault.contains("costs"))
    );
    // A declared band the catalog does not declare, and a `corrected` that changed nothing.
    let other = if band == "extreme" { "high" } else { "extreme" };
    let drifted = ok("launch").replace(
        &format!("| {band} | {band} |"),
        &format!("| {band} | {other} |"),
    );
    says(
        &band_faults(&synthetic(&[&drifted, &ok("compute")]), &catalog()),
        "the catalog declares",
    );
    let unchanged = ok("launch").replace("| within |", "| corrected |");
    says(
        &band_faults(&synthetic(&[&unchanged, &ok("compute")]), &catalog()),
        "did not change",
    );
    // A pending row names a collector that has not landed, under a backend
    // the catalog may not have yet: nothing to hold to a band.
    let later =
        "| gpu.profile | vendor:cupti | - | - | - | - | - | - | - | - | - | pending: lands later |";
    let waiting = synthetic(&[&ok("launch"), &ok("compute"), later]);
    assert!(
        record_faults(&waiting).is_empty(),
        "{:?}",
        record_faults(&waiting)
    );
    assert!(
        band_faults(&waiting, &catalog()).is_empty(),
        "{:?}",
        band_faults(&waiting, &catalog())
    );
    // A measured row with no band.
    let unbanded = ok("compute").replace(&format!("| {band} | {band} |"), "| - | - |");
    says(
        &record_faults(&synthetic(&[&ok("launch"), &unbanded])),
        "gives no band",
    );
    // A measured row that lost records, and one that does not say.
    let lossy = ok("compute").replace("| bare | 0 of 1000 |", "| bare | 12 of 1000 |");
    says(
        &record_faults(&synthetic(&[&ok("launch"), &lossy])),
        "lost 12 of 1000 records",
    );
    // One in a thousand is shown and not refused.
    let rare = ok("compute").replace("| bare | 0 of 1000 |", "| bare | 1 of 1000 |");
    assert!(record_faults(&synthetic(&[&ok("launch"), &rare])).is_empty());
    let uncounted = ok("compute").replace("| bare | 0 of 1000 |", "| bare | - |");
    says(
        &record_faults(&synthetic(&[&ok("launch"), &uncounted])),
        "no count of losses",
    );
    // A `within` whose band moved.
    let other_band = if band == "low" { "high" } else { "low" };
    let moved = ok("launch").replace(
        &format!("| {band} | {band} | within |"),
        &format!("| {other_band} | {band} | within |"),
    );
    says(
        &band_faults(&synthetic(&[&moved, &ok("compute")]), &catalog()),
        "that is `corrected`",
    );
    // A backend the catalog does not have.
    let unknown = ok("launch").replace("| preload |", "| vendor:none |");
    says(
        &band_faults(&synthetic(&[&unknown]), &catalog()),
        "no such backend",
    );
}

#[test]
fn a_gpu_tracer_missing_from_the_record_is_named() {
    let band = api_band();
    let only_api = synthetic(&[
        &format!("| gpu.api | preload | trace | launch | 0.10 % | +0.20 % | +0.10 % | bare | 0 of 1000 | {band} | {band} | within |"),
        &format!("| gpu.api | preload | trace | compute | 0.10 % | +0.20 % | +0.10 % | bare | 0 of 1000 | {band} | {band} | within |"),
    ]);
    says(
        &coverage_faults(&only_api, &catalog()),
        "gpu.kernel through vendor:cupti",
    );
}

#[test]
fn the_gpu_tracers_are_left_out_of_the_backends_table_and_the_polls_are_not() {
    let catalog = catalog();
    let entry = |id: &str| find(&catalog, id).unwrap_or_else(|| panic!("{id} in the catalog"));
    assert!(on_gpu_workloads(entry("gpu.kernel")));
    assert!(on_gpu_workloads(entry("gpu.api")));
    assert!(!on_gpu_workloads(entry("gpu.freq")));
    assert!(!on_gpu_workloads(entry("cpu.cycles")));
}

#[test]
fn the_gpu_workload_compiles_without_a_driver() {
    // Syntax and types only: linking needs `libcuda.so.1`, which the CI does
    // not have, and the measurement builds it where there is one.
    let output = Command::new("cc")
        .args(["-fsyntax-only", "-Wall", "-Wextra", "-Werror"])
        .arg(harness::root().join("examples/bench/gpu.c"))
        .output()
        .expect("cc runs: it is the linker cargo uses");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn the_record_rows_are_the_table_rows() {
    // The record `render_record` writes is the record `parse` reads: a table
    // pasted as written passes the record checks it can pass.
    let band = Cost::Low;
    let planned = GpuPlanned {
        method: harness::Method {
            source: "gpu.api".into(),
            backend: Some("preload".into()),
            mode: "trace".into(),
            emit: "summary".into(),
            filter: None,
            system: false,
        },
        declared: band,
    };
    let mut bare = Series::default();
    let mut traced = Series::default();
    for value in [100.0e6, 101.0e6, 99.0e6, 100.5e6] {
        bare.elapsed.push(value);
        bare.section_cpu.push(value);
        traced.elapsed.push(value * 1.002);
        traced.section_cpu.push(value * 1.002);
        traced.share.push(0.2);
    }
    let rows: Vec<GpuRow> = ["launch", "compute"]
        .iter()
        .map(|workload| row(&planned, workload, &traced, &bare, None, 2.0))
        .collect();
    let baselines = vec![
        json!({"workload": "launch", "regime": "launch-bound", "resolves_percent": 2.0}),
        json!({"workload": "compute", "regime": "compute-bound", "resolves_percent": 2.0}),
    ];
    let text = render_record(std::path::Path::new("gpu-0"), &baselines, &rows);
    let record = parse(&text);
    assert_eq!(record.rows.len(), 2);
    assert!(
        record_faults(&record).is_empty(),
        "{:?}",
        record_faults(&record)
    );
    assert_eq!(record.rows[0].share, Some(0.2));
    assert_eq!(record.rows[0].verdict, "within");
}

#[test]
fn a_gpu_table_that_lost_records_or_left_a_pair_out_or_misplaced_a_run_fails() {
    let method = |source: &str, backend: &str| harness::Method {
        source: source.into(),
        backend: Some(backend.into()),
        mode: "trace".into(),
        emit: "summary".into(),
        filter: None,
        system: false,
    };
    let cupti = GpuPlanned {
        method: method("gpu.kernel", "vendor:cupti"),
        declared: Cost::Extreme,
    };
    let mut bare = Series::default();
    let mut armed = Series::default();
    for value in [100.0e6, 101.0e6, 99.0e6, 100.5e6] {
        bare.elapsed.push(value);
        bare.section_cpu.push(value);
        armed.elapsed.push(value * 1.7);
        armed.section_cpu.push(value * 1.7);
        armed.share.push(9.0);
    }
    let control = bare.clone();
    let clean = row(&cupti, "launch", &armed, &bare, Some(&control), 2.0);
    assert_eq!(clean.against, "control");
    let planned = [cupti.clone()];
    assert!(
        gpu_table_failures(&planned, &[], std::slice::from_ref(&clean), 1).is_empty(),
        "{:?}",
        gpu_table_failures(&planned, &[], std::slice::from_ref(&clean), 1)
    );

    // Records dropped under the load.
    let mut lossy = clean.clone();
    lossy.losses = 40;
    lossy.events_total = 1000;
    says(
        &gpu_table_failures(&planned, &[], &[lossy.clone()], 1),
        "lost 40 of 1000 records",
    );
    lossy.losses = 4;
    assert!(gpu_table_failures(&planned, &[], &[lossy], 1).is_empty());
    // No control: the CUPTI row falls back to bare, which is not its baseline.
    let uncontrolled = row(&cupti, "launch", &armed, &bare, None, 2.0);
    says(
        &gpu_table_failures(&planned, &[], &[uncontrolled], 1),
        "not the control",
    );
    // A pair the tool accepted whose trial run failed has no row, and is named.
    let failed = Unplanned {
        source: "gpu.kernel".into(),
        backend: "vendor:cupti".into(),
        why: "the trial run failed".into(),
        failed_its_trial: true,
    };
    says(
        &gpu_table_failures(&[], &[failed], std::slice::from_ref(&clean), 1),
        "failed its trial run",
    );
    // One the machine cannot serve is not a failure.
    let refused = Unplanned {
        source: "gpu.kernel".into(),
        backend: "vendor:rocprof".into(),
        why: "no collector is written for it".into(),
        failed_its_trial: false,
    };
    assert!(gpu_table_failures(&planned, &[refused], &[clean], 1).is_empty());

    // A control that never loaded its library, a bare run that did, and a run
    // whose threads shared the launching thread's cpu.
    let library = std::path::PathBuf::from("libomn_cupti.so");
    let mut unloaded = control.clone();
    unloaded.injection = vec![1, 0, 1, 1];
    says(
        &series_failures(&Arm::Control(library.clone()), "launch", &unloaded),
        "did not load",
    );
    let mut loaded = control.clone();
    loaded.injection = vec![1, 1, 1, 1];
    assert!(series_failures(&Arm::Control(library), "launch", &loaded).is_empty());
    let mut injected = bare.clone();
    injected.injection = vec![-1, -1, 1, -1];
    says(
        &series_failures(&Arm::Bare, "launch", &injected),
        "go without",
    );
    let mut shared = bare.clone();
    shared.injection = vec![-1; 4];
    shared.sharing = 3;
    says(
        &series_failures(&Arm::Bare, "launch", &shared),
        "launching thread's cpu",
    );
}

/// A replaying arm runs the launch regime at its smaller size, and only it:
/// the other regimes and every other arm keep their arguments.
#[test]
fn only_a_replaying_arm_runs_the_launch_regime_smaller() {
    let method = |source: &str| harness::Method {
        source: source.into(),
        backend: Some("vendor:cupti".into()),
        mode: "trace".into(),
        emit: "summary".into(),
        filter: None,
        system: false,
    };
    assert!(Arm::Served(method("gpu.counters")).replays());
    assert!(!Arm::Served(method("gpu.kernel")).replays());
    assert!(!Arm::Bare.replays());

    let workloads = gpu_workloads();
    let launch = workloads
        .iter()
        .find(|w| w.name == "launch")
        .expect("a launch regime");
    let scaled = launch
        .scaled()
        .expect("the launch regime has a replayed size");
    assert_eq!(scaled.name, launch.name);
    assert_eq!(scaled.regime, launch.regime);
    assert_eq!(scaled.args, ["launches", &REPLAYED_LAUNCHES.to_string()]);
    let full: u32 = launch.args[1].parse().unwrap();
    assert!(
        REPLAYED_LAUNCHES * 10 <= full,
        "{REPLAYED_LAUNCHES} against {full}"
    );
    for other in workloads.iter().filter(|w| w.name != "launch") {
        assert!(
            other.scaled().is_none(),
            "{} has no replayed size",
            other.name
        );
    }
}
