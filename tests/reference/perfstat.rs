//! Parsers for the `perfstat` family's reference tools. Each turns a tool's
//! output into the values a pair compares (`catalog/references/README.md`).
//! Every value of `omn`'s side is a path into its JSON, so there is no
//! parser of `omn`'s output here.
//!
//! `perf stat -x,` writes one line per event on stderr:
//! `value,unit,event,run-time,enabled-pct,metric-value,metric-unit`. The
//! value is already scaled by perf when the event was multiplexed (enabled
//! below 100 %), which is an estimate, not a count; the parser keeps it and
//! also reports the share under `<event>.enabled_pct` so a pair can refuse an
//! estimate. An event perf could not count (`<not counted>`,
//! `<not supported>`) is left out, so a pair keyed on it fails with the
//! missing key rather than comparing against zero.

use crate::common::{Parser, Values};

pub fn parsers() -> Vec<Parser> {
    vec![
        Parser {
            name: "perf_stat_csv",
            parse: perf_stat_csv,
        },
        Parser {
            name: "perf_stat_human",
            parse: perf_stat_human,
        },
    ]
}

/// The event name a pair keys on: a hybrid machine's `cpu_core/cycles/` and
/// `cpu_atom/cycles/` both become `cycles`, and are summed, because omn's
/// count of a process is over every core it ran on.
fn event_key(raw: &str) -> String {
    let raw = raw.trim();
    let bare = match raw.split_once('/') {
        Some((_pmu, rest)) => rest.trim_end_matches('/'),
        None => raw,
    };
    // perf prints modifiers after a colon (`cycles:u`); omn counts the
    // same event, so the modifier is not part of the key.
    bare.split(':').next().unwrap_or(bare).to_string()
}

fn add(values: &mut Values, key: &str, v: f64) {
    match values.iter_mut().find(|(k, _)| k == key) {
        Some((_, acc)) => *acc += v,
        None => values.push((key.to_string(), v)),
    }
}

fn get(values: &Values, key: &str) -> Option<f64> {
    values.iter().find(|(k, _)| k == key).map(|(_, v)| *v)
}

/// The derived values perf prints beside the counts, recomputed from them so
/// a pair compares them whatever perf's metric columns held.
fn derive(values: &mut Values) {
    if let (Some(i), Some(c)) = (get(values, "instructions"), get(values, "cycles")) {
        if c > 0.0 {
            values.push(("ipc".to_string(), i / c));
        }
    }
    // With task-clock beside them, every count also as a rate per on-cpu
    // nanosecond: what two windows of different length over the same
    // process still agree on (an attach by pid, where perf's window is
    // omn's plus omn's own start and exit).
    if let Some(ms) = get(values, "task-clock").filter(|ms| *ms > 0.0) {
        let per: Vec<(String, f64)> = values
            .iter()
            .filter(|(k, _)| k != "task-clock" && k != "ipc" && !k.contains('.'))
            .map(|(k, v)| (format!("{k}.per_cpu_ns"), v / (ms * 1e6)))
            .collect();
        values.extend(per);
    }
}

/// `perf stat -x, -e a,b,c -- prog`: the counts on stderr, the program's own
/// output on stdout (ignored).
fn perf_stat_csv(_stdout: &str, stderr: &str) -> Result<Values, String> {
    let mut values = Values::new();
    let mut lowest_pct: Vec<(String, f64)> = Vec::new();
    for line in stderr.lines() {
        let fields: Vec<&str> = line.split(',').collect();
        if fields.len() < 3 {
            continue;
        }
        let event = event_key(fields[2]);
        if event.is_empty() {
            continue;
        }
        let raw = fields[0].trim();
        if raw.starts_with('<') {
            // <not counted> / <not supported>: absent, never zero.
            continue;
        }
        let Ok(v) = raw.parse::<f64>() else {
            continue;
        };
        add(&mut values, &event, v);
        // `-G cgroup` inserts the cgroup's path after the event name, which
        // moves the run time and the enabled share one column right.
        let pct_at = match fields.get(3).map(|f| f.trim().parse::<f64>()) {
            Some(Err(_)) if !fields[3].trim().is_empty() => 5,
            _ => 4,
        };
        if let Some(pct) = fields
            .get(pct_at)
            .and_then(|p| p.trim().parse::<f64>().ok())
        {
            match lowest_pct.iter_mut().find(|(k, _)| *k == event) {
                Some((_, p)) => *p = p.min(pct),
                None => lowest_pct.push((event, pct)),
            }
        }
    }
    if values.is_empty() {
        return Err(format!(
            "no counted event in perf stat's output: {}",
            stderr.lines().take(3).collect::<Vec<_>>().join(" | ")
        ));
    }
    for (event, pct) in lowest_pct {
        values.push((format!("{event}.enabled_pct"), pct));
    }
    derive(&mut values);
    Ok(values)
}

/// `perf stat -e a,b -- prog` without `-x`: the table a person reads. Kept
/// so a capture pasted from documentation or a bug report can be checked.
fn perf_stat_human(_stdout: &str, stderr: &str) -> Result<Values, String> {
    let mut values = Values::new();
    for line in stderr.lines() {
        let mut words = line.split_whitespace();
        let (Some(count), Some(unit_or_event)) = (words.next(), words.next()) else {
            continue;
        };
        let digits: String = count.chars().filter(|c| *c != ',').collect();
        let Ok(v) = digits.parse::<f64>() else {
            continue;
        };
        match unit_or_event {
            // "0.52 seconds time elapsed", "0.18 seconds user".
            "seconds" => {
                if line.contains("time elapsed") {
                    values.push(("elapsed_s".to_string(), v));
                }
            }
            // "188.55 msec task-clock".
            "msec" => add(&mut values, &event_key(words.next().unwrap_or("")), v),
            event => add(&mut values, &event_key(event), v),
        }
    }
    if values.is_empty() {
        return Err("no counted event in perf stat's output".to_string());
    }
    derive(&mut values);
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPIN_CSV: &str = include_str!("captures/perfstat/spin-csv.txt");
    const SPIN_HUMAN: &str = include_str!("captures/perfstat/spin-human.txt");
    const MULTIPLEXED: &str = include_str!("captures/perfstat/multiplexed-csv.txt");
    const CGROUP: &str = include_str!("captures/perfstat/cgroup-csv.txt");
    const SYSTEM: &str = include_str!("captures/perfstat/system-csv.txt");
    const ATTACH: &str = include_str!("captures/perfstat/attach-pid-csv.txt");
    const CGROUP_RELATIVE: &str = include_str!("captures/perfstat/cgroup-relative-csv.txt");

    #[test]
    fn csv_reads_every_counted_event() {
        let v = perf_stat_csv("", SPIN_CSV).unwrap();
        assert_eq!(get(&v, "cycles"), Some(3_551_169_769.0));
        assert_eq!(get(&v, "instructions"), Some(7_892_862_465.0));
        assert_eq!(get(&v, "branches"), Some(1_324_605_092.0));
        assert_eq!(get(&v, "branch-misses"), Some(1_016_642.0));
        assert_eq!(get(&v, "cache-misses"), Some(4_077_704.0));
        assert_eq!(get(&v, "context-switches"), Some(323.0));
        assert_eq!(get(&v, "cpu-migrations"), Some(8.0));
        assert_eq!(get(&v, "page-faults"), Some(825.0));
        assert_eq!(get(&v, "cycles.enabled_pct"), Some(100.0));
        let ipc = get(&v, "ipc").unwrap();
        assert!((ipc - 7_892_862_465.0 / 3_551_169_769.0).abs() < 1e-12);
    }

    #[test]
    fn csv_leaves_out_what_perf_could_not_count() {
        let v = perf_stat_csv("", MULTIPLEXED).unwrap();
        assert_eq!(get(&v, "ref-cycles"), None);
        assert_eq!(get(&v, "bus-cycles"), None);
        // Multiplexed: the scaled estimate is kept, with its share.
        assert_eq!(get(&v, "cycles"), Some(258_526_229.0));
        assert_eq!(get(&v, "cycles.enabled_pct"), Some(83.0));
    }

    #[test]
    fn csv_reads_a_cgroup_column_and_a_system_wide_run() {
        let v = perf_stat_csv("", CGROUP).unwrap();
        assert_eq!(get(&v, "instructions"), Some(14_911_896_832.0));
        assert_eq!(get(&v, "cycles"), Some(4_620_956_933.0));
        assert_eq!(get(&v, "instructions.enabled_pct"), Some(100.0));
        let v = perf_stat_csv("", SYSTEM).unwrap();
        assert_eq!(get(&v, "cycles"), Some(27_118_179_167.0));
        assert_eq!(get(&v, "context-switches"), Some(13_223.0));
    }

    /// The pair `cpu.instructions.cgroup` names its cgroup through a link in
    /// the run's scratch directory, `-G ../../../tmp/<run>/cg`: the column
    /// is a path with no number in it, read as the cgroup's.
    #[test]
    fn csv_reads_a_cgroup_named_relative_to_the_mount() {
        let v = perf_stat_csv("", CGROUP_RELATIVE).unwrap();
        assert_eq!(get(&v, "instructions"), Some(4_749_361_858.0));
        assert_eq!(get(&v, "cycles"), Some(1_542_347_321.0));
        assert_eq!(get(&v, "instructions.enabled_pct"), Some(100.0));
    }

    #[test]
    fn csv_with_task_clock_gives_rates_per_cpu_ns() {
        let v = perf_stat_csv("", ATTACH).unwrap();
        assert_eq!(get(&v, "task-clock"), Some(1653.92));
        let rate = get(&v, "instructions.per_cpu_ns").unwrap();
        assert!((rate - 14_482_860_075.0 / 1_653_920_000.0).abs() < 1e-9);
        assert_eq!(get(&v, "task-clock.per_cpu_ns"), None);
    }

    #[test]
    fn csv_sums_a_hybrid_machines_core_types() {
        let hybrid = "100,,cpu_core/cycles/,10,100.00,,\n\
                      50,,cpu_atom/cycles/,10,100.00,,\n\
                      300,,cpu_core/instructions/,10,100.00,,\n\
                      <not counted>,,cpu_atom/instructions/,0,0.00,,\n";
        let v = perf_stat_csv("", hybrid).unwrap();
        assert_eq!(get(&v, "cycles"), Some(150.0));
        assert_eq!(get(&v, "instructions"), Some(300.0));
        assert_eq!(get(&v, "ipc"), Some(2.0));
    }

    #[test]
    fn csv_with_nothing_counted_is_an_error() {
        let err = perf_stat_csv("", "<not supported>,,cycles,0,100.00,,\n").unwrap_err();
        assert!(err.contains("no counted event"), "{err}");
    }

    #[test]
    fn human_table_reads_counts_and_elapsed() {
        let v = perf_stat_human("", SPIN_HUMAN).unwrap();
        assert_eq!(get(&v, "cycles"), Some(798_682_901.0));
        assert_eq!(get(&v, "instructions"), Some(1_779_155_355.0));
        assert_eq!(get(&v, "elapsed_s"), Some(0.526701422));
        assert!(get(&v, "ipc").unwrap() > 2.0);
    }

    #[test]
    fn modifiers_and_pmu_prefixes_are_not_part_of_the_key() {
        assert_eq!(event_key("cycles:u"), "cycles");
        assert_eq!(event_key("cpu_core/instructions/"), "instructions");
        assert_eq!(event_key(" page-faults "), "page-faults");
    }
}
