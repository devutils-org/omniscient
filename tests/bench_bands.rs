//! The catalog's cost bands, held to the table the bench last measured.
//!
//! `docs/bench.md` keeps, between its `bands:begin` and `bands:end` markers,
//! the record of the last backends table: the gate's resolution per workload,
//! and per (source, backend) the two columns a band is judged on, the band the
//! catalog declared when the table ran, the band it declares now and the
//! verdict. These tests read that record and the embedded catalog, in
//! `make check`, without measuring anything: a band cannot drift from the
//! measurement without one of them failing, and a new table pasted into the
//! record names every band it puts outside.
//!
//! The rule is `harness::judge_band`'s -- a row's cost is the larger of its
//! share of the target's cpu and its dilation -- with one difference written
//! in `docs/bench.md`: a dilation no larger than what the gate says the
//! workload resolves cannot be told from zero, and reads as zero.

use std::collections::{BTreeMap, BTreeSet};

use omn::catalog::{Catalog, Cost};

const BENCH_MD: &str = include_str!("../docs/bench.md");

/// One row of the record.
#[derive(Debug)]
struct Row {
    source: String,
    backend: String,
    workload: String,
    /// The median share of the target's cpu, in percent; `None` for `-`.
    share: Option<f64>,
    /// The median dilation, in percent; `None` for `-`.
    dilation: Option<f64>,
    /// The band the catalog declared when the table ran.
    was: Cost,
    /// The band the catalog declares now, as the record says.
    declared: Cost,
    verdict: String,
}

struct Record {
    resolves: BTreeMap<String, f64>,
    rows: Vec<Row>,
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

/// `0.17 %`, `+6.31 %`, `-0.40 %` or `-`.
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

fn parse(text: &str) -> Record {
    let begin = text
        .find("<!-- bands:begin")
        .expect("docs/bench.md has no `bands:begin` marker");
    let end = text[begin..]
        .find("<!-- bands:end -->")
        .expect("docs/bench.md has no `bands:end` marker after its `bands:begin`")
        + begin;
    let mut resolves = BTreeMap::new();
    let mut rows = Vec::new();
    for line in text[begin..end].lines().skip(1) {
        let line = line.trim();
        if !line.starts_with('|') || line.starts_with("|---") {
            continue;
        }
        let cells = cells(line);
        match cells.len() {
            2 if cells[0] == "workload" => {}
            2 => {
                let resolution = percent(cells[1])
                    .unwrap_or_else(|| panic!("workload `{}` has no resolution", cells[0]));
                resolves.insert(cells[0].to_string(), resolution);
            }
            10 if cells[0] == "source" => {}
            10 => rows.push(Row {
                source: cells[0].to_string(),
                backend: cells[1].to_string(),
                workload: cells[4].to_string(),
                share: percent(cells[5]),
                dilation: percent(cells[6]),
                was: cost(cells[7]),
                declared: cost(cells[8]),
                verdict: cells[9].to_string(),
            }),
            n => panic!("a line of the record has {n} cells: {line}"),
        }
    }
    Record { resolves, rows }
}

/// The dilation the bench can tell from zero: over the workload's resolution.
fn counted_dilation(dilation: Option<f64>, resolution: f64) -> Option<f64> {
    dilation.filter(|dilation| *dilation > resolution)
}

/// A row's measured cost, in percent of the target's cpu.
fn measured(share: Option<f64>, dilation: Option<f64>, resolution: f64) -> Option<f64> {
    match (share, counted_dilation(dilation, resolution)) {
        (Some(share), Some(dilation)) => Some(share.max(dilation)),
        (Some(share), None) => Some(share),
        (None, Some(dilation)) => Some(dilation),
        // A dilation measured, and within what the workload resolves.
        (None, None) if dilation.is_some() => Some(0.0),
        (None, None) => None,
    }
}

fn label(row: &Row) -> String {
    format!("`{}` through `{}`", row.source, row.backend)
}

fn record() -> Record {
    let record = parse(BENCH_MD);
    assert!(
        !record.rows.is_empty(),
        "docs/bench.md's band record has no row"
    );
    record
}

#[test]
fn the_rule_reads_a_dilation_under_the_resolution_as_zero() {
    // Over the resolution, the larger column is the cost.
    assert_eq!(measured(Some(0.5), Some(6.31), 0.85), Some(6.31));
    assert_eq!(measured(Some(142.0), Some(6.31), 0.85), Some(142.0));
    // Under it, or negative, the dilation is not a measurement.
    assert_eq!(measured(Some(0.02), Some(0.21), 0.39), Some(0.02));
    assert_eq!(measured(Some(0.17), Some(-0.40), 0.23), Some(0.17));
    // A system-wide row has no dilation.
    assert_eq!(measured(Some(0.30), None, 0.85), Some(0.30));
    assert_eq!(measured(None, None, 0.85), None);
}

#[test]
fn the_record_is_whole_and_reads_one_way() {
    let record = record();
    let mut pairs = BTreeSet::new();
    for row in &record.rows {
        assert!(
            pairs.insert((row.source.as_str(), row.backend.as_str())),
            "{} is recorded twice",
            label(row)
        );
        assert!(
            record.resolves.contains_key(&row.workload),
            "{} ran on `{}`, whose resolution the record does not give",
            label(row),
            row.workload
        );
        let changed = row.was != row.declared;
        if row.verdict == "corrected" {
            assert!(
                changed,
                "{} says `corrected` and declares `{}` as it did when the table ran",
                label(row),
                row.declared
            );
        } else {
            assert!(
                !changed,
                "{} moved from `{}` to `{}` and its verdict, `{}`, does not say `corrected`",
                label(row),
                row.was,
                row.declared,
                row.verdict
            );
        }
    }
}

#[test]
fn every_measured_pair_declares_the_band_its_row_records() {
    let catalog = Catalog::embedded().expect("the embedded catalog must load");
    let mut wrong = Vec::new();
    for row in record().rows {
        let Some(entry) = catalog
            .entries()
            .iter()
            .find(|entry| entry.id.as_str() == row.source)
        else {
            wrong.push(format!("{}: the catalog has no such source", label(&row)));
            continue;
        };
        let Some(backend) = entry.backend(&row.backend) else {
            wrong.push(format!("{}: the source has no such backend", label(&row)));
            continue;
        };
        if backend.cost != row.declared {
            wrong.push(format!(
                "{}: the catalog declares `{}` and docs/bench.md's record `{}`; a cost line \
                 changes with its row, and the row with its reason",
                label(&row),
                backend.cost,
                row.declared
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

#[test]
fn every_band_holds_what_the_table_measured_or_says_why_it_stands() {
    let record = record();
    let mut wrong = Vec::new();
    for row in &record.rows {
        let resolution = record.resolves[&row.workload];
        let ceiling = row.declared.worst_case_percent();
        let Some(cost) = measured(row.share, row.dilation, resolution) else {
            wrong.push(format!(
                "{}: neither a share nor a dilation to hold to a band",
                label(row)
            ));
            continue;
        };
        let over = cost > ceiling;
        match row.verdict.strip_prefix("stands") {
            Some(reason) => {
                let reason = reason.trim_start_matches(':').trim();
                if reason.is_empty() {
                    wrong.push(format!("{} stands with no reason", label(row)));
                }
                if !over {
                    wrong.push(format!(
                        "{} stands, and measured {cost:.2} % inside `{}`: the reason is no \
                         longer needed",
                        label(row),
                        row.declared
                    ));
                }
                // What a standing row is held to is the column its reason
                // names: what the target paid.
                if let Some(dilation) = counted_dilation(row.dilation, resolution) {
                    if dilation > ceiling {
                        wrong.push(format!(
                            "{} stands on its dilation, and dilated its target {dilation:.2} % \
                             against `{}`'s {ceiling} %",
                            label(row),
                            row.declared
                        ));
                    }
                }
            }
            None if over => wrong.push(format!(
                "{}: measured {cost:.2} % against `{}`'s {ceiling} % on `{}`; correct the band \
                 or say in docs/bench.md why it stands",
                label(row),
                row.declared,
                row.workload
            )),
            None => {}
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

#[test]
fn a_band_over_its_measurement_is_named_and_a_stale_reason_too() {
    let text = "<!-- bands:begin test -->\n\
        | workload | resolves |\n|---|---|\n| io | 0.85 % |\n\n\
        | source | backend | mode | target | workload | % of target | dilation | was | declared | verdict |\n\
        |---|---|---|---|---|---|---|---|---|---|\n\
        | a.over | x | count | launch | io | 0.30 % | -0.10 % | free | free | within |\n\
        | a.fine | x | count | launch | io | 0.30 % | +0.50 % | free | low | corrected |\n\
        | a.stale | x | trace | launch | io | 0.30 % | +0.10 % | low | low | stands: attach |\n\
        <!-- bands:end -->\n";
    let record = parse(text);
    assert_eq!(record.rows.len(), 3);
    let judged: Vec<(String, bool)> = record
        .rows
        .iter()
        .map(|row| {
            let cost = measured(row.share, row.dilation, record.resolves[&row.workload]).unwrap();
            (row.source.clone(), cost > row.declared.worst_case_percent())
        })
        .collect();
    assert_eq!(
        judged,
        vec![
            ("a.over".to_string(), true),
            ("a.fine".to_string(), false),
            ("a.stale".to_string(), false),
        ]
    );
}
