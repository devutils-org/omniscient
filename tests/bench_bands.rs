//! The catalog's cost bands, held to the table the bench last measured.
//!
//! `docs/bench.md` keeps, between its `bands:begin` and `bands:end` markers,
//! the record of the last backends table: the gate's resolution per workload,
//! and per (source, backend) the two columns a band is judged on, the band the
//! catalog declared when the table ran, the band it declares now and the
//! verdict. The `bands:begin` marker says how many rows the table had, so a
//! row cannot leave the record without a test failing. These tests read that
//! record and the embedded catalog, in `make check`, without measuring
//! anything: a band cannot drift from the measurement without one of them
//! failing, and a new table pasted into the record names every band it puts
//! outside.
//!
//! The rule is `harness::judge_band`'s -- a row's cost is the larger of its
//! share of the target's cpu and its dilation -- with one difference written
//! in `docs/bench.md`: a dilation no larger than what the gate says the
//! workload resolves cannot be told from zero, and reads as zero.
//!
//! One rule holds the bands the table did not measure: a backend declared
//! `equivalent` declares no cheaper band than a backend above it in the chain
//! that serves a target it serves. `--budget` degrades onto what is below and
//! declares less; an equivalent one is the same measurement, often through the
//! same mechanism (`bpf:tp` beside `bpf:raw_tp`), and a cheaper band on it
//! would promise a saving the move cannot deliver.
//!
//! Each judgement is a function returning what it found wrong, run on the
//! real record by one test and on synthetic rows, one per way of being wrong,
//! by another: a check never seen to fail is not shown to check.

use std::collections::{BTreeMap, BTreeSet};

use omn::catalog::{Backend, Catalog, Cost, Entry, TargetSupport};
use omn::request::TargetKind;

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
    /// The number of rows the table had, from the `bands:begin` marker's
    /// `rows=`; `None` when the marker does not say.
    expected_rows: Option<usize>,
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
    let marker = text[begin..].lines().next().unwrap_or_default();
    let expected_rows = marker
        .split_whitespace()
        .find_map(|word| word.strip_prefix("rows="))
        .map(|count| {
            count
                .parse()
                .unwrap_or_else(|_| panic!("`rows={count}` in `{marker}` is not a count"))
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
    Record {
        expected_rows,
        resolves,
        rows,
    }
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

/// What a verdict says.
#[derive(Debug, PartialEq)]
enum Verdict<'a> {
    /// The measurement is inside the band the catalog declared.
    Within,
    /// The band moved to the measurement.
    Corrected,
    /// The band stands over the measurement: `stands <on what>: <why>`.
    Stands { reason: &'a str },
}

fn verdict(text: &str) -> Option<Verdict<'_>> {
    match text {
        "within" => Some(Verdict::Within),
        "corrected" => Some(Verdict::Corrected),
        _ => {
            let rest = text.strip_prefix("stands")?;
            let reason = rest.split_once(':').map_or("", |(_, reason)| reason.trim());
            Some(Verdict::Stands { reason })
        }
    }
}

/// What makes the record unreadable as one: a row missing or twice, a
/// workload with no resolution, a verdict that is not one, `corrected` on a
/// band that did not move, or a moved band that does not say it.
fn record_faults(record: &Record) -> Vec<String> {
    let mut wrong = Vec::new();
    match record.expected_rows {
        None => wrong.push(
            "the `bands:begin` marker does not say how many rows the table had (`rows=N`)"
                .to_string(),
        ),
        Some(expected) if expected != record.rows.len() => wrong.push(format!(
            "the table had {expected} rows and the record holds {}: a row left the record, \
             or one came in without the marker",
            record.rows.len()
        )),
        Some(_) => {}
    }
    let mut pairs = BTreeSet::new();
    for row in &record.rows {
        if !pairs.insert((row.source.as_str(), row.backend.as_str())) {
            wrong.push(format!("{} is recorded twice", label(row)));
        }
        if !record.resolves.contains_key(&row.workload) {
            wrong.push(format!(
                "{} ran on `{}`, whose resolution the record does not give",
                label(row),
                row.workload
            ));
        }
        let changed = row.was != row.declared;
        match verdict(&row.verdict) {
            None => wrong.push(format!(
                "{}: `{}` is not a verdict; a row is `within`, `corrected` or \
                 `stands ...: <reason>`",
                label(row),
                row.verdict
            )),
            Some(Verdict::Corrected) if !changed => wrong.push(format!(
                "{} says `corrected` and declares `{}` as it did when the table ran",
                label(row),
                row.declared
            )),
            Some(Verdict::Within | Verdict::Stands { .. }) if changed => wrong.push(format!(
                "{} moved from `{}` to `{}` and its verdict, `{}`, does not say `corrected`",
                label(row),
                row.was,
                row.declared,
                row.verdict
            )),
            Some(_) => {}
        }
    }
    wrong
}

/// Every row whose band does not hold what it measured: over its band with
/// no reason, standing with no reason or with one no longer needed, or
/// standing on a dilation that has left the band.
fn band_faults(record: &Record) -> Vec<String> {
    let mut wrong = Vec::new();
    for row in &record.rows {
        let Some(&resolution) = record.resolves.get(&row.workload) else {
            // `record_faults` names it.
            continue;
        };
        let ceiling = row.declared.worst_case_percent();
        let Some(cost) = measured(row.share, row.dilation, resolution) else {
            wrong.push(format!(
                "{}: neither a share nor a dilation to hold to a band",
                label(row)
            ));
            continue;
        };
        let over = cost > ceiling;
        match verdict(&row.verdict) {
            Some(Verdict::Stands { reason }) => {
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
            _ if over => wrong.push(format!(
                "{}: measured {cost:.2} % against `{}`'s {ceiling} % on `{}`; correct the band \
                 or say in docs/bench.md why it stands",
                label(row),
                row.declared,
                row.workload
            )),
            _ => {}
        }
    }
    wrong
}

/// The targets `backend` serves in `entry`'s chain.
fn served(entry: &Entry, backend: &Backend) -> BTreeSet<TargetKind> {
    backend
        .targets
        .as_ref()
        .unwrap_or(&entry.targets)
        .iter()
        .filter(|(_, support)| **support != TargetSupport::No)
        .map(|(kind, _)| *kind)
        .collect()
}

/// Every backend declared `equivalent` that declares a cheaper band than a
/// backend above it in the chain serving a target it serves.
fn cheaper_equivalents(entries: &[Entry]) -> Vec<String> {
    let mut wrong = Vec::new();
    for entry in entries {
        for (at, fallback) in entry.backends.iter().enumerate() {
            if !fallback.equivalent {
                continue;
            }
            let theirs = served(entry, fallback);
            for above in &entry.backends[..at] {
                if fallback.cost < above.cost && !served(entry, above).is_disjoint(&theirs) {
                    wrong.push(format!(
                        "`{}` through `{}` is declared equivalent and declares `{}`, under \
                         `{}`'s `{}`: a budget would degrade onto it and keep paying the same",
                        entry.id.as_str(),
                        fallback.id,
                        fallback.cost,
                        above.id,
                        above.cost
                    ));
                }
            }
        }
    }
    wrong
}

fn record() -> Record {
    let record = parse(BENCH_MD);
    assert!(
        !record.rows.is_empty(),
        "docs/bench.md's band record has no row"
    );
    record
}

fn catalog() -> Catalog {
    Catalog::embedded().expect("the embedded catalog must load")
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
    let faults = record_faults(&record());
    assert!(faults.is_empty(), "{}", faults.join("\n"));
}

#[test]
fn every_measured_pair_declares_the_band_its_row_records() {
    let catalog = catalog();
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
    let faults = band_faults(&record());
    assert!(faults.is_empty(), "{}", faults.join("\n"));
}

#[test]
fn no_equivalent_backend_declares_less_than_what_it_stands_in_for() {
    let faults = cheaper_equivalents(catalog().entries());
    assert!(faults.is_empty(), "{}", faults.join("\n"));
}

/// A record of one workload, `io`, resolving 0.85 %, with `rows` as its
/// table and `marker_rows` as the count its marker gives.
fn synthetic(marker_rows: Option<usize>, rows: &[&str]) -> Record {
    let marker = marker_rows.map_or(String::new(), |count| format!(" rows={count}"));
    let text = format!(
        "<!-- bands:begin test{marker} -->\n\
         | workload | resolves |\n|---|---|\n| io | 0.85 % |\n\n\
         | source | backend | mode | target | workload | % of target | dilation | was | declared | verdict |\n\
         |---|---|---|---|---|---|---|---|---|---|\n\
         {}\n<!-- bands:end -->\n",
        rows.join("\n")
    );
    parse(&text)
}

/// Holds that `judge`, on a record of `row` alone, finds one fault, naming
/// the row and saying `says`.
fn one_fault(judge: fn(&Record) -> Vec<String>, row: &str, says: &str) {
    let faults = judge(&synthetic(Some(1), &[row]));
    assert_eq!(faults.len(), 1, "{row}: {faults:?}");
    assert!(
        faults[0].contains("`a` through `x`") && faults[0].contains(says),
        "{row}: `{}` does not say `{says}`",
        faults[0]
    );
}

#[test]
fn a_band_over_its_measurement_and_every_bad_reason_is_named() {
    // Within the band, corrected to it, standing on a dilation inside it:
    // nothing to say.
    let fine = synthetic(
        Some(3),
        &[
            "| a.within | x | count | launch | io | 0.05 % | +0.80 % | free | free | within |",
            "| a.fixed | x | count | launch | io | 0.30 % | +0.50 % | free | low | corrected |",
            "| a.stands | x | trace | launch | io | 142.00 % | +6.31 % | high | high | stands on its dilation: attach |",
        ],
    );
    assert_eq!(band_faults(&fine), Vec::<String>::new());
    assert_eq!(record_faults(&fine), Vec::<String>::new());

    // Over `free` on its share; over `low` on a dilation the workload resolves.
    one_fault(
        band_faults,
        "| a | x | count | launch | io | 0.30 % | -0.10 % | free | free | within |",
        "against `free`",
    );
    one_fault(
        band_faults,
        "| a | x | count | launch | io | 0.02 % | +1.20 % | low | low | within |",
        "measured 1.20 %",
    );
    // Corrected, and still over the band it was corrected to.
    one_fault(
        band_faults,
        "| a | x | count | launch | io | 1.50 % | -0.10 % | free | low | corrected |",
        "correct the band",
    );
    // A reason no longer needed.
    one_fault(
        band_faults,
        "| a | x | trace | launch | io | 0.30 % | +0.10 % | low | low | stands on its dilation: attach |",
        "no longer needed",
    );
    // A reason that is not one.
    for empty in ["stands", "stands on its dilation", "stands:"] {
        one_fault(
            band_faults,
            &format!("| a | x | trace | launch | io | 142.00 % | +0.10 % | low | low | {empty} |"),
            "no reason",
        );
    }
    // A standing row whose dilation has left its band.
    one_fault(
        band_faults,
        "| a | x | trace | launch | io | 142.00 % | +3.00 % | low | low | stands on its dilation: attach |",
        "dilated its target 3.00 %",
    );
}

#[test]
fn a_record_missing_a_row_or_misreading_one_is_named() {
    let within = "| a | x | count | launch | io | 0.05 % | - | free | free | within |";
    // A row gone from a table that had two.
    let faults = record_faults(&synthetic(Some(2), &[within]));
    assert_eq!(faults.len(), 1, "{faults:?}");
    assert!(faults[0].contains("had 2 rows"), "{}", faults[0]);
    // A marker that does not say how many.
    let faults = record_faults(&synthetic(None, &[within]));
    assert_eq!(faults.len(), 1, "{faults:?}");
    assert!(faults[0].contains("rows=N"), "{}", faults[0]);
    // A row recorded twice.
    assert_eq!(
        record_faults(&synthetic(Some(2), &[within, within])),
        vec!["`a` through `x` is recorded twice".to_string()]
    );

    one_fault(
        record_faults,
        "| a | x | count | launch | io | 0.05 % | - | free | free | fine |",
        "is not a verdict",
    );
    one_fault(
        record_faults,
        "| a | x | count | launch | io | 0.05 % | - | free | free | corrected |",
        "says `corrected`",
    );
    one_fault(
        record_faults,
        "| a | x | count | launch | io | 0.05 % | - | free | low | within |",
        "does not say `corrected`",
    );
    one_fault(
        record_faults,
        "| a | x | count | launch | memory | 0.05 % | - | free | free | within |",
        "whose resolution",
    );
}

#[test]
fn an_equivalent_backend_cheaper_than_what_it_stands_in_for_is_named() {
    let catalog = catalog();
    let io_size = catalog
        .entries()
        .iter()
        .find(|entry| entry.id.as_str() == "io.size")
        .expect("the catalog has `io.size`");
    assert_eq!(io_size.backends[0].id, "bpf:raw_tp");
    assert!(io_size.backends[1].equivalent);
    let chain = |fallback: Cost, equivalent: bool, disjoint: bool| {
        let mut entry = io_size.clone();
        entry.backends[0].cost = Cost::High;
        entry.backends[1].cost = fallback;
        entry.backends[1].equivalent = equivalent;
        if disjoint {
            entry.backends[0].targets =
                Some(BTreeMap::from([(TargetKind::System, TargetSupport::Native)]));
            entry.backends[1].targets =
                Some(BTreeMap::from([(TargetKind::Cgroup, TargetSupport::BpfOnly)]));
        }
        cheaper_equivalents(&[entry])
    };

    let faults = chain(Cost::Low, true, false);
    assert_eq!(faults.len(), 1, "{faults:?}");
    assert!(
        faults[0].contains("`io.size` through `bpf:tp`")
            && faults[0].contains("`bpf:raw_tp`'s `high`"),
        "{}",
        faults[0]
    );
    // As dear, or dearer: nothing to say.
    assert_eq!(chain(Cost::High, true, false), Vec::<String>::new());
    assert_eq!(chain(Cost::Extreme, true, false), Vec::<String>::new());
    // Cheaper and degraded rather than equivalent: what a budget's ladder is
    // for.
    assert_eq!(chain(Cost::Low, false, false), Vec::<String>::new());
    // Cheaper, and serving no target the one above serves: not a step a
    // budget can take for the same request.
    assert_eq!(chain(Cost::Low, true, true), Vec::<String>::new());
}
