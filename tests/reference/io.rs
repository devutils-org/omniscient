//! Parsers for the `io` family's reference tools. Each turns a tool's
//! output into the values a pair compares (docs/dev/BRIEF.md, "Decided before this
//! wave").
//!
//! - `fio_json`: fio's `--output-format=json` report, per direction of the
//!   first job: `read.total_ios`, `read.io_bytes`, `read.iops`,
//!   `read.clat_mean_ns`, `read.lat_mean_ns`, `read.clat_p50_ns`,
//!   `read.clat_p99_ns`, and the same under `write.`.
//! - `iostat_json`: `iostat -d -x -o JSON`'s last report, per disk as
//!   `<disk>.<column>` and summed over the disks as `all.<column>` (a mean
//!   column such as `r_await` is weighted by the operations behind it), with
//!   the kB/s columns also in bytes (`rB/s`, `wB/s`) and the await columns
//!   in nanoseconds (`r_await_ns`, `w_await_ns`).
//! - `omn_summary_as_fio`, `omn_summary_as_iostat`: `omn`'s own output (a
//!   pair's `compare.omn_parse`), the `summary` emit of its results, read in
//!   the tool's vocabulary so the two sides' keys agree. The pairs that use
//!   them restrict `omn` to reads (a launched process's `io.latency` sees the
//!   reads it submits; a system-wide one is filtered on `rwbs =~ 'R'`), so
//!   the summary's requests summed over its series are the tool's read count
//!   and their mean weighted by each series' count is the tool's read mean:
//!   `read.total_ios` and `read.clat_mean_ns` for fio, `all.r_await_ns` for
//!   iostat. A mean per series is a mean per device, which the path grammar
//!   can sum but not weight. A result whose `losses` removed records
//!   (`dropped`, `failed_reads`, `undecoded`, `throttled`) is refused with
//!   the counts: a count would come out short and a mean would be over the
//!   survivors, and a pair must not hold on either.

use serde_json::Value;

use crate::common::{Parser, Values};

pub fn parsers() -> Vec<Parser> {
    vec![
        Parser {
            name: "fio_json",
            parse: fio_json,
        },
        Parser {
            name: "iostat_json",
            parse: iostat_json,
        },
        Parser {
            name: "omn_summary_as_fio",
            parse: omn_summary_as_fio,
        },
        Parser {
            name: "omn_summary_as_iostat",
            parse: omn_summary_as_iostat,
        },
        Parser {
            name: "omn_util_busiest",
            parse: omn_util_busiest,
        },
        Parser {
            name: "omn_hist_as_fio",
            parse: omn_hist_as_fio,
        },
        Parser {
            name: "vfsstat_totals",
            parse: vfsstat_totals,
        },
    ]
}

/// `vfsstat 1 N`: one row a second of each VFS function's calls per second,
/// summed over the rows into a count per column, by its name in lower case
/// without the `/s` (`read`, `write`, `open`, ...). A row is one second, so
/// its rate is that second's count.
fn vfsstat_totals(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let mut names: Vec<String> = Vec::new();
    let mut totals: Vec<f64> = Vec::new();
    let mut rows = 0;
    for line in stdout.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.first() == Some(&"TIME") {
            names = words[1..]
                .iter()
                .map(|name| name.trim_end_matches("/s").to_lowercase())
                .collect();
            totals = vec![0.0; names.len()];
            continue;
        }
        if names.is_empty() || words.len() != names.len() + 1 {
            continue;
        }
        for (total, value) in totals.iter_mut().zip(&words[1..]) {
            *total += value
                .parse::<f64>()
                .map_err(|_| format!("a vfsstat rate `{value}` is no number"))?;
        }
        rows += 1;
    }
    if rows == 0 {
        return Err("vfsstat printed no row".into());
    }
    Ok(names.into_iter().zip(totals).collect())
}

/// The JSON document in a tool's output, stdout first: fio may print a
/// warning line before its report, so the document starts at the first `{`.
fn json_document(stdout: &str, stderr: &str) -> Result<Value, String> {
    let text = if stdout.contains('{') { stdout } else { stderr };
    let start = text
        .find('{')
        .ok_or_else(|| "no JSON document in the tool's output".to_string())?;
    serde_json::from_str(&text[start..]).map_err(|e| format!("not JSON: {e}"))
}

fn number(v: &Value, what: &str) -> Result<f64, String> {
    v.as_f64()
        .ok_or_else(|| format!("`{what}` is not a number"))
}

fn fio_json(stdout: &str, stderr: &str) -> Result<Values, String> {
    let doc = json_document(stdout, stderr)?;
    let job = doc
        .get("jobs")
        .and_then(|j| j.get(0))
        .ok_or_else(|| "fio's report has no job".to_string())?;
    if let Some(err) = job.get("error").and_then(Value::as_i64) {
        if err != 0 {
            return Err(format!("fio's job failed with error {err}"));
        }
    }
    let mut out = Values::new();
    for dir in ["read", "write"] {
        let d = job
            .get(dir)
            .ok_or_else(|| format!("fio's job has no `{dir}` section"))?;
        let field = |name: &str| -> Result<f64, String> {
            number(
                d.get(name).unwrap_or(&Value::Null),
                &format!("{dir}.{name}"),
            )
        };
        out.push((format!("{dir}.total_ios"), field("total_ios")?));
        out.push((format!("{dir}.io_bytes"), field("io_bytes")?));
        out.push((format!("{dir}.iops"), field("iops")?));
        for (span, key) in [("clat_ns", "clat"), ("lat_ns", "lat")] {
            let mean = d.get(span).and_then(|s| s.get("mean"));
            out.push((
                format!("{dir}.{key}_mean_ns"),
                number(mean.unwrap_or(&Value::Null), &format!("{dir}.{span}.mean"))?,
            ));
        }
        // Percentiles are present only when the direction did any I/O.
        if let Some(p) = d.get("clat_ns").and_then(|s| s.get("percentile")) {
            for (pct, key) in [("50.000000", "p50"), ("99.000000", "p99")] {
                if let Some(v) = p.get(pct).and_then(Value::as_f64) {
                    out.push((format!("{dir}.clat_{key}_ns"), v));
                }
            }
        }
    }
    Ok(out)
}

fn iostat_json(stdout: &str, stderr: &str) -> Result<Values, String> {
    let doc = json_document(stdout, stderr)?;
    let reports = doc
        .pointer("/sysstat/hosts/0/statistics")
        .and_then(Value::as_array)
        .ok_or_else(|| "iostat's report has no statistics".to_string())?;
    // `iostat INTERVAL COUNT` prints one report per interval; the last is
    // the one over the window the pair observed.
    let disks = reports
        .last()
        .and_then(|r| r.get("disk"))
        .and_then(Value::as_array)
        .ok_or_else(|| "iostat's last report has no disk section".to_string())?;
    if disks.is_empty() {
        return Err("iostat reported no disk".to_string());
    }
    const SUMMED: [&str; 6] = ["r/s", "w/s", "rkB/s", "wkB/s", "aqu-sz", "f/s"];
    // Flushes are counted since Linux 5.5 and sysstat 12: sysstat 11.6
    // (Ubuntu 18.04, 4.15) has no `f/s` column, which is absent, not zero.
    const SINCE_5_5: &str = "f/s";
    // A mean per operation, weighted by the operations of the same row.
    const WEIGHTED: [(&str, &str); 2] = [("r_await", "r/s"), ("w_await", "w/s")];
    let mut out = Values::new();
    let mut sums = vec![0.0; SUMMED.len()];
    let mut present = vec![false; SUMMED.len()];
    let mut weighted = [0.0; WEIGHTED.len()];
    for disk in disks {
        let name = disk
            .get("disk_device")
            .and_then(Value::as_str)
            .ok_or_else(|| "a disk row has no disk_device".to_string())?;
        let col = |c: &str| -> Result<f64, String> {
            number(disk.get(c).unwrap_or(&Value::Null), &format!("{name}.{c}"))
        };
        for (i, c) in SUMMED.iter().enumerate() {
            if *c == SINCE_5_5 && disk.get(c).is_none() {
                continue;
            }
            let v = col(c)?;
            sums[i] += v;
            present[i] = true;
            out.push((format!("{name}.{c}"), v));
        }
        for (i, (c, by)) in WEIGHTED.iter().enumerate() {
            let v = col(c)?;
            weighted[i] += v * col(by)?;
            out.push((format!("{name}.{c}"), v));
        }
        out.push((format!("{name}.util"), col("util")?));
    }
    // A device's busy share is not summed across devices: the busiest one's
    // is what a machine-wide reading names, by no device's name.
    let busiest = out
        .iter()
        .filter(|(k, _)| k.ends_with(".util"))
        .map(|(_, v)| *v)
        .fold(0.0, f64::max);
    out.push(("busiest.util".to_owned(), busiest));
    for (i, c) in SUMMED.iter().enumerate() {
        if present[i] {
            out.push((format!("all.{c}"), sums[i]));
        }
    }
    for (i, (c, by)) in WEIGHTED.iter().enumerate() {
        let ops = sums[SUMMED.iter().position(|s| s == by).unwrap()];
        let mean = if ops > 0.0 { weighted[i] / ops } else { 0.0 };
        out.push((format!("all.{c}"), mean));
    }
    // Units the omn side speaks: bytes and nanoseconds.
    let derived: Vec<(String, f64)> = out
        .iter()
        .filter_map(|(k, v)| {
            if let Some(stem) = k.strip_suffix("kB/s") {
                Some((format!("{stem}B/s"), v * 1024.0))
            } else if k.ends_with("_await") {
                Some((format!("{k}_ns"), v * 1e6))
            } else {
                None
            }
        })
        .collect();
    out.extend(derived);
    Ok(out)
}

/// The `losses` counters that remove records from a summary. `late_ticks`
/// delays a periodic emit and removes nothing from a summary of events.
const RECORD_LOSSES: [&str; 4] = ["dropped", "failed_reads", "undecoded", "throttled"];

/// `omn`'s results, refused when their `losses` removed records.
fn lossless_results(doc: &Value) -> Result<&Vec<Value>, String> {
    let results = doc
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(|| "omn's output has no results".to_string())?;
    let lost: Vec<String> = results
        .iter()
        .filter_map(|r| r.get("losses").and_then(Value::as_object))
        .flat_map(|losses| {
            RECORD_LOSSES.iter().filter_map(|name| {
                let n = losses.get(*name).and_then(Value::as_f64).unwrap_or(0.0);
                (n > 0.0).then(|| format!("{name} {n}"))
            })
        })
        .collect();
    if !lost.is_empty() {
        return Err(format!(
            "omn lost records ({}): its count and mean are over the survivors only",
            lost.join(", ")
        ));
    }
    Ok(results)
}

/// The requests and their mean over every series of every `summary` emit
/// in `omn`'s results: `(count, mean weighted by count)`.
fn omn_summary(stdout: &str, stderr: &str) -> Result<(f64, f64), String> {
    let doc = json_document(stdout, stderr)?;
    let results = lossless_results(&doc)?;
    let mut series_seen = 0;
    let (mut count, mut weighted) = (0.0, 0.0);
    for output in results
        .iter()
        .filter_map(|r| r.get("outputs").and_then(Value::as_array))
        .flatten()
        .filter(|o| o.get("emit").and_then(Value::as_str) == Some("summary"))
    {
        for s in output
            .get("series")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let n = number(s.get("count").unwrap_or(&Value::Null), "series.count")?;
            let mean = number(s.get("mean").unwrap_or(&Value::Null), "series.mean")?;
            series_seen += 1;
            count += n;
            weighted += n * mean;
        }
    }
    if series_seen == 0 {
        return Err("omn's results have no summary series".to_string());
    }
    let mean = if count > 0.0 { weighted / count } else { 0.0 };
    Ok((count, mean))
}

fn omn_summary_as_fio(stdout: &str, stderr: &str) -> Result<Values, String> {
    let (count, mean) = omn_summary(stdout, stderr)?;
    Ok(vec![
        ("read.total_ios".to_string(), count),
        ("read.clat_mean_ns".to_string(), mean),
    ])
}

fn omn_summary_as_iostat(stdout: &str, stderr: &str) -> Result<Values, String> {
    let (count, mean) = omn_summary(stdout, stderr)?;
    Ok(vec![
        ("all.r_ios".to_string(), count),
        ("all.r_await_ns".to_string(), mean),
    ])
}

/// `omn`'s `hist` emits (log2), every series merged bucket by bucket: the
/// median as the low edge of the bucket that holds it, keyed
/// `read.clat_p50_ns` as fio's, for a pair held to `log2_buckets`. The
/// requests are the reads of the launched process, as in
/// `omn_summary_as_fio`.
fn omn_hist_as_fio(stdout: &str, stderr: &str) -> Result<Values, String> {
    let doc = json_document(stdout, stderr)?;
    let results = lossless_results(&doc)?;
    let mut buckets: std::collections::BTreeMap<u64, f64> = Default::default();
    for output in results
        .iter()
        .filter_map(|r| r.get("outputs").and_then(Value::as_array))
        .flatten()
        .filter(|o| o.get("emit").and_then(Value::as_str) == Some("hist"))
    {
        if output.get("layout").and_then(Value::as_str) != Some("log2") {
            return Err("omn's hist is not log2: its buckets are not fio's to compare".into());
        }
        for s in output
            .get("series")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            for b in s
                .get("buckets")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let low = b
                    .get("low")
                    .and_then(Value::as_u64)
                    .ok_or("a bucket without an integer `low`")?;
                *buckets.entry(low).or_default() +=
                    number(b.get("count").unwrap_or(&Value::Null), "bucket.count")?;
            }
        }
    }
    let total: f64 = buckets.values().sum();
    if total <= 0.0 {
        return Err("omn's results have no hist with a request in it".into());
    }
    let mut seen = 0.0;
    let p50 = buckets
        .iter()
        .find(|(_, n)| {
            seen += **n;
            seen >= total / 2.0
        })
        .map(|(low, _)| *low as f64)
        .expect("a non-empty histogram has a median bucket");
    Ok(vec![
        ("read.total_ios".to_string(), total),
        ("read.clat_p50_ns".to_string(), p50),
    ])
}

/// `io.util`'s summary as iostat's `%util`: the busiest device's share of
/// the window, in percent, as `busiest.util` (device names differ between
/// the two: `dev:259+0` against `nvme0n1`).
fn omn_util_busiest(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let doc: Value =
        serde_json::from_str(stdout).map_err(|e| format!("omn's output is not JSON: {e}"))?;
    let series = doc
        .pointer("/results/0/outputs")
        .and_then(Value::as_array)
        .and_then(|outputs| outputs.iter().find(|o| o["emit"] == "summary"))
        .and_then(|o| o["series"].as_array())
        .ok_or("omn's result has no `summary` output")?;
    let busiest = series
        .iter()
        .filter_map(|one| one["last"].as_f64())
        .fold(None, |most: Option<f64>, v| {
            Some(most.map_or(v, |m| m.max(v)))
        })
        .ok_or("omn's summary has no device")?;
    Ok(vec![("busiest.util".to_owned(), busiest * 100.0)])
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIO: &str = include_str!("captures/io/fio-randread.json");
    const IOSTAT: &str = include_str!("captures/io/iostat-dx.json");
    const OMN_LAUNCH: &str = include_str!("captures/io/omn-io-latency-launch.json");
    const OMN_SYSTEM: &str = include_str!("captures/io/omn-io-latency-system.json");

    fn get(values: &Values, key: &str) -> f64 {
        values
            .iter()
            .find(|(k, _)| k == key)
            .unwrap_or_else(|| panic!("no `{key}` in {values:?}"))
            .1
    }

    #[test]
    fn fio_json_reads_the_captured_randread() {
        let v = fio_json(FIO, "").unwrap();
        assert_eq!(get(&v, "read.total_ios"), 27297.0);
        assert_eq!(get(&v, "read.io_bytes"), 27297.0 * 4096.0);
        assert!((get(&v, "read.clat_mean_ns") - 72849.643954).abs() < 1e-3);
        assert!(get(&v, "read.lat_mean_ns") >= get(&v, "read.clat_mean_ns"));
        assert!(get(&v, "read.clat_p50_ns") > 0.0);
        assert_eq!(get(&v, "write.total_ios"), 0.0);
    }

    #[test]
    fn fio_json_finds_the_document_after_a_warning() {
        let noisy = format!("fio: some warning\n{FIO}");
        let v = fio_json("", &noisy).unwrap();
        assert_eq!(get(&v, "read.total_ios"), 27297.0);
    }

    #[test]
    fn fio_json_refuses_what_is_not_a_report() {
        assert!(fio_json("", "fio: file not found").is_err());
        assert!(fio_json("{\"jobs\": []}", "").is_err());
    }

    /// The busiest device on both sides: iostat's `%util` as it printed it,
    /// and a real `io.util --system` run over a 1 GiB direct write.
    #[test]
    fn the_busiest_device_s_util_is_read_on_both_sides() {
        let v = iostat_json(IOSTAT, "").unwrap();
        assert_eq!(get(&v, "busiest.util"), 34.77);
        let o = omn_util_busiest(include_str!("captures/io/omn-io-util-system.json"), "").unwrap();
        assert!((get(&o, "busiest.util") - 14.275569318388723).abs() < 1e-9);
        assert!(omn_util_busiest("{}", "").is_err());
    }

    #[test]
    fn iostat_json_sums_disks_and_converts_units() {
        let v = iostat_json(IOSTAT, "").unwrap();
        // `iostat -d -x -y -o JSON 4 1` over a 2 s fio job started a second
        // in; zram0 read nothing and weighs nothing.
        assert_eq!(get(&v, "nvme0n1.r/s"), 5843.75);
        assert_eq!(get(&v, "all.r/s"), 5843.75);
        assert_eq!(get(&v, "all.rB/s"), 23375.0 * 1024.0);
        assert!((get(&v, "all.r_await_ns") - 60_000.0).abs() < 1e-6);
        assert!((get(&v, "nvme0n1.r_await_ns") - 60_000.0).abs() < 1e-6);
        assert_eq!(get(&v, "nvme0n1.util"), 34.77);
        assert_eq!(get(&v, "zram0.util"), 0.0);
    }

    #[test]
    fn iostat_json_reads_sysstat_11_without_flushes() {
        // sysstat 11.6 (Ubuntu 18.04): no `f/s` column, on a kernel that
        // counts no flushes. The other columns are read; `f/s` is absent.
        let v = iostat_json(
            r#"{"sysstat": {"hosts": [{"statistics": [{"disk": [{"disk_device": "vda",
                "r/s": 10.0, "w/s": 2.0, "rkB/s": 40.0, "wkB/s": 8.0, "aqu-sz": 0.1,
                "r_await": 0.5, "w_await": 1.0, "util": 3.0}]}]}]}}"#,
            "",
        )
        .unwrap();
        assert_eq!(get(&v, "all.r/s"), 10.0);
        assert_eq!(get(&v, "busiest.util"), 3.0);
        assert!(!v.iter().any(|(k, _)| k.ends_with("f/s")));
    }

    #[test]
    fn omn_summary_reads_the_captured_launch_as_fio() {
        // omn launching fio, the run fio-randread.json is the report of.
        let v = omn_summary_as_fio(OMN_LAUNCH, "").unwrap();
        assert_eq!(get(&v, "read.total_ios"), 27297.0);
        assert!((get(&v, "read.clat_mean_ns") - 55908.40792028428).abs() < 1e-6);
    }

    #[test]
    fn omn_summary_reads_the_captured_system_run_as_iostat() {
        // The same four seconds as iostat-dx.json: 23375 reads, which is
        // iostat's 5843.75 r/s over its 4 s.
        let v = omn_summary_as_iostat(OMN_SYSTEM, "").unwrap();
        assert_eq!(get(&v, "all.r_ios"), 23375.0);
        assert!((get(&v, "all.r_await_ns") - 56733.52337967914).abs() < 1e-6);
    }

    #[test]
    fn omn_summary_weights_each_device_by_its_requests() {
        let two = r#"{"results": [{"outputs": [
            {"emit": "summary", "series": [
                {"key": "dev:259+6", "count": 1, "mean": 1000.0},
                {"key": "dev:259+9", "count": 3, "mean": 2000.0}]},
            {"emit": "rate", "series": [{"key": "x", "count": 100, "mean": 9.0}]}]}]}"#;
        let v = omn_summary_as_fio(two, "").unwrap();
        assert_eq!(get(&v, "read.total_ios"), 4.0);
        assert_eq!(get(&v, "read.clat_mean_ns"), 1750.0);
    }

    #[test]
    fn omn_summary_refuses_results_without_a_summary() {
        assert!(omn_summary_as_fio(r#"{"results": []}"#, "").is_err());
        let rate_only = r#"{"results": [{"outputs": [{"emit": "rate", "series": []}]}]}"#;
        assert!(omn_summary_as_iostat(rate_only, "").is_err());
        assert!(omn_summary_as_fio("", "omn: cannot run").is_err());
    }

    #[test]
    fn omn_summary_refuses_a_run_that_lost_records() {
        let lossy = OMN_SYSTEM.replacen("\"dropped\": 0", "\"dropped\": 12", 1);
        assert_ne!(lossy, OMN_SYSTEM);
        let err = omn_summary_as_iostat(&lossy, "").unwrap_err();
        assert!(err.contains("dropped 12"), "{err}");
        let lossy = OMN_LAUNCH.replacen("\"undecoded\": 0", "\"undecoded\": 3", 1);
        let err = omn_summary_as_fio(&lossy, "").unwrap_err();
        assert!(err.contains("undecoded 3"), "{err}");
        // A late tick removes no record: the summary still reads.
        let late = OMN_SYSTEM.replacen("\"late_ticks\": 0", "\"late_ticks\": 1", 1);
        assert_eq!(
            get(&omn_summary_as_iostat(&late, "").unwrap(), "all.r_ios"),
            23375.0
        );
    }

    #[test]
    fn omn_hist_reads_the_median_bucket_of_a_real_run() {
        // `omn trace io.latency -m trace -e hist --format json -o DIR -- fio
        // --output-format=json randread.fio` (2026-10-05, under the wave's
        // load): 19212 requests, 19073 of them in [32768, 65536); fio's own
        // clat p50 that run was 78336 ns, one log2 bucket above.
        let v = omn_hist_as_fio(include_str!("captures/io/omn-io-latency-hist.json"), "").unwrap();
        assert_eq!(get(&v, "read.total_ios"), 19212.0);
        assert_eq!(get(&v, "read.clat_p50_ns"), 32768.0);
        assert!(omn_hist_as_fio(OMN_LAUNCH, "").is_err());
    }

    #[test]
    fn vfsstat_rows_are_summed_per_column() {
        let out = "TIME         READ/s  WRITE/s  FSYNC/s   OPEN/s\n20:04:00:       252      195        0       42\n20:04:01:       184      186        0       31\n";
        let v = vfsstat_totals(out, "").unwrap();
        assert!(v.contains(&("read".to_string(), 436.0)), "{v:?}");
        assert!(v.contains(&("open".to_string(), 73.0)), "{v:?}");
        assert!(vfsstat_totals("TIME READ/s\n", "").is_err());
    }

    #[test]
    fn iostat_json_refuses_an_empty_report() {
        assert!(iostat_json("{\"sysstat\": {\"hosts\": []}}", "").is_err());
        assert!(iostat_json("", "").is_err());
    }
}
