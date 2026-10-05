//! `omn top`'s batch output: the `system` view as text a script parses,
//! `top -b`'s use.
//!
//! Each number is checked against something the view did not compute: the
//! recording's own summaries, read here from its JSON, and, on a live run,
//! the program's own cpu clock.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use omn_view::model::Data;
use omn_view::views::system;

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/view_fixtures")
        .join(name)
}

/// The table: its heading, then each row by its id and kind.
struct Table {
    head: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl Table {
    fn parse(text: &str) -> Table {
        let mut lines = text
            .lines()
            .filter(|line| !line.starts_with('#') && !line.starts_with("machine "));
        let head: Vec<String> = lines
            .next()
            .expect("the table's heading")
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        let rows: Vec<Vec<String>> = lines
            .map(|line| line.split_whitespace().map(str::to_owned).collect())
            .collect();
        for row in &rows {
            assert_eq!(row.len(), head.len(), "a row of every column: {row:?}");
        }
        Table { head, rows }
    }

    fn column(&self, name: &str) -> usize {
        self.head
            .iter()
            .position(|head| head == name)
            .unwrap_or_else(|| panic!("no column {name} in {:?}", self.head))
    }

    fn number(&self, row: &[String], name: &str) -> Option<f64> {
        let cell = &row[self.column(name)];
        (cell != "-").then(|| cell.parse().expect("a number or -"))
    }
}

/// Per task, the cpu time a recording's `cpu.oncpu_time` summary says:
/// `count` x `mean` over its series keyed by that task, read off the JSON.
fn summary_cpu(dir: &Path) -> BTreeMap<i64, f64> {
    let text = std::fs::read_to_string(dir.join("results.json")).expect("results.json");
    let json: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    let mut totals = BTreeMap::new();
    for results in json["results"].as_array().expect("results") {
        if results["source"] != "cpu.oncpu_time"
            || results["label"]
                .as_str()
                .is_some_and(|label| label.ends_with("@system"))
        {
            continue;
        }
        for output in results["outputs"].as_array().expect("outputs") {
            if output["emit"] != "summary" {
                continue;
            }
            for series in output["series"].as_array().expect("series") {
                let key = series["key"].as_str().unwrap_or_default();
                let Some(tid) = key
                    .split([',', ' '])
                    .find_map(|part| part.strip_prefix("task:"))
                    .and_then(|tid| tid.parse::<i64>().ok())
                else {
                    continue;
                };
                let count = series["count"].as_f64().unwrap_or(0.0);
                let mean = series["mean"].as_f64().unwrap_or(0.0);
                *totals.entry(tid).or_insert(0.0) += count * mean;
            }
        }
    }
    totals
}

#[test]
fn the_batch_table_is_the_recording_s_cpu_per_task_sorted() {
    for name in ["machine", "workload-system", "http-server"] {
        let dir = fixture(name);
        let data = Data::load(&dir).expect("the fixture loads");
        let text = system::batch(&data, "cpu").expect("the batch");
        let table = Table::parse(&text);
        let truth = summary_cpu(&dir);
        assert!(!table.rows.is_empty(), "{name}: no task listed\n{text}");
        let mut checked = 0;
        for row in &table.rows {
            let id: i64 = row[table.column("ID")].parse().expect("an id");
            let Some(on_cpu) = table.number(row, "ON_CPU_NS") else {
                continue;
            };
            if let Some(expected) = truth.get(&id) {
                assert!(
                    (on_cpu - expected).abs() <= expected.abs() * 1e-6 + 1.0,
                    "{name}: task {id} on cpu {on_cpu} ns, the summary says {expected}"
                );
                checked += 1;
            }
        }
        assert_eq!(
            checked,
            truth.len(),
            "{name}: every task the summary holds is a row\n{text}"
        );
        // Sorted by the share of one cpu, largest first.
        let shares: Vec<f64> = table
            .rows
            .iter()
            .map(|row| table.number(row, "CPU").unwrap_or(f64::NEG_INFINITY))
            .collect();
        assert!(
            shares.windows(2).all(|pair| pair[0] >= pair[1]),
            "{name}: sorted by CPU: {shares:?}"
        );
    }
}

#[test]
fn the_batch_says_the_machine_and_what_it_cannot_be_taken_for() {
    let data = Data::load(&fixture("machine")).expect("the fixture loads");
    let text = system::batch(&data, "memory").expect("the batch");
    let busy = text
        .lines()
        .find(|line| line.starts_with("machine cpu.util "))
        .unwrap_or_else(|| panic!("the cpu's line\n{text}"));
    let mean: f64 = busy
        .split_whitespace()
        .find_map(|word| word.strip_prefix("mean="))
        .and_then(|mean| mean.parse().ok())
        .expect("the cpu's mean");
    assert!((0.0..=1.0).contains(&mean), "a share: {busy}");
    assert!(
        text.lines()
            .any(|line| line.starts_with("machine io.bytes ")),
        "{text}"
    );
    // Sorted by memory held when asked: the RSS column descends where held.
    let table = Table::parse(&text);
    let rss: Vec<f64> = table
        .rows
        .iter()
        .filter_map(|row| table.number(row, "RSS_BYTES"))
        .collect();
    assert!(rss.windows(2).all(|pair| pair[0] >= pair[1]), "{rss:?}");
    assert!(system::batch(&data, "colour").is_err());
}

/// A live run of a program that burns a known amount of cpu and writes its
/// own cpu clock (`time.process_time`, the kernel's account of the process)
/// when it ends: the batch's rows add up to it.
#[test]
fn a_live_run_s_batch_holds_the_program_s_own_cpu_clock() {
    let out = std::env::temp_dir().join(format!("omn-view-top-batch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).unwrap();
    let clock = out.join("clock");
    let program = format!(
        "import time\n\
         end = time.process_time() + 0.8\n\
         while time.process_time() < end: sum(range(1000))\n\
         time.sleep(0.3)\n\
         open({clock:?}, 'w').write(repr(time.process_time()))\n",
        clock = clock.to_str().unwrap()
    );
    let output = Command::new(BINARY)
        .args(["top", "--headless", "-d", "30s", "-o"])
        .arg(out.join("run"))
        .args(["--", "python3", "-c", &program])
        .output()
        .expect("omn runs");
    assert!(
        output.status.success(),
        "omn top --headless failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let own: f64 = std::fs::read_to_string(&clock)
        .expect("the program wrote its clock")
        .trim()
        .parse()
        .expect("seconds");
    let data = Data::load(&out.join("run")).expect("the run loads");
    let text = system::batch(&data, "cpu").expect("the batch");
    let table = Table::parse(&text);
    // A process's row is its own account; a thread's adds up to it. Every
    // row is the program's: it is the run's only target.
    let processes: f64 = table
        .rows
        .iter()
        .filter(|row| row[table.column("KIND")] == "process")
        .filter_map(|row| table.number(row, "ON_CPU_NS"))
        .sum();
    let threads: f64 = table
        .rows
        .iter()
        .filter(|row| row[table.column("KIND")] == "thread")
        .filter_map(|row| table.number(row, "ON_CPU_NS"))
        .sum();
    let read = if processes > 0.0 { processes } else { threads } / 1e9;
    eprintln!("the program's own clock {own:.4} s, the batch {read:.4} s\n{text}");
    assert!(
        (read - own).abs() <= own * 0.05 + 0.03,
        "the batch holds {read:.4} s of cpu, the program's own clock {own:.4} s\n{text}"
    );
    let _ = std::fs::remove_dir_all(&out);
}
