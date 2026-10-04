//! Parsers for the `gpu` family's reference tools. Each turns a tool's
//! output into the values a pair compares (BRIEF.md, "Decided before this
//! wave").
//!
//! Every value is put in the unit `omn` reports, so a pair compares numbers
//! and never converts: durations in nanoseconds, clocks in hertz, power in
//! watts, memory in bytes, utilisation as a fraction of one. Names are put in
//! `omn`'s spelling too: nsys's `[CUDA memcpy Host-to-Device]` is `omn`'s
//! `[memcpy HtoD]`, and a driver call loses its ABI suffix (`cuMemAlloc_v2`
//! is `cuMemAlloc`), as `gpu.api` names it.

use crate::common::{Parser, Values};

pub fn parsers() -> Vec<Parser> {
    vec![
        Parser {
            name: "nsys_gpu_sum",
            parse: nsys_gpu_sum,
        },
        Parser {
            name: "nsys_api_sum",
            parse: nsys_api_sum,
        },
        Parser {
            name: "ncu_metrics_csv",
            parse: ncu_metrics_csv,
        },
        Parser {
            name: "smi_query_gpu",
            parse: smi_query_gpu,
        },
        Parser {
            name: "smi_dmon",
            parse: smi_dmon,
        },
        Parser {
            name: "smi_compute_apps",
            parse: smi_compute_apps,
        },
    ]
}

/// Splits one CSV line, honouring double quotes (ncu quotes every field, and
/// a demangled kernel name or a thousands-separated number holds commas).
fn fields(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => out.push(std::mem::take(&mut field)),
            _ => field.push(c),
        }
    }
    out.push(field);
    out.into_iter().map(|f| f.trim().to_string()).collect()
}

/// A number as the tools print it: `808,192` (ncu), `37.57`; `[N/A]` and `-`
/// are no number.
fn number(text: &str) -> Option<f64> {
    text.replace(',', "").trim().parse().ok()
}

fn column(header: &[String], name: &str) -> Result<usize, String> {
    header
        .iter()
        .position(|h| h == name)
        .ok_or_else(|| format!("no `{name}` column in {header:?}"))
}

/// nsys's operation names in `gpu.kernel`'s spelling.
fn nsys_operation(name: &str) -> String {
    match name {
        "[CUDA memcpy Host-to-Device]" => "[memcpy HtoD]".into(),
        "[CUDA memcpy Device-to-Host]" => "[memcpy DtoH]".into(),
        "[CUDA memcpy Device-to-Device]" => "[memcpy DtoD]".into(),
        "[CUDA memcpy Host-to-Host]" => "[memcpy HtoH]".into(),
        "[CUDA memcpy Peer-to-Peer]" => "[memcpy PtoP]".into(),
        "[CUDA memset]" => "[memset]".into(),
        other => other.into(),
    }
}

/// A driver call without its ABI suffix: `cuMemcpyHtoD_v2` is `cuMemcpyHtoD`.
fn api_name(name: &str) -> String {
    match name.rsplit_once("_v") {
        Some((stem, version))
            if !version.is_empty() && version.chars().all(|c| c.is_ascii_digit()) =>
        {
            stem.into()
        }
        _ => name.into(),
    }
}

type Table = (Vec<String>, Vec<Vec<String>>);

/// The `nsys stats --format csv` tables in `text`, each a header and its
/// rows, separated by blank lines.
fn nsys_tables(text: &str) -> Vec<Table> {
    let mut tables = Vec::new();
    for block in text.split("\n\n") {
        let mut lines = block.lines().filter(|l| !l.trim().is_empty());
        let Some(header) = lines.next() else { continue };
        if !header.starts_with("Time (%)") {
            continue;
        }
        tables.push((fields(header), lines.map(fields).collect()));
    }
    tables
}

/// Each row of a table as `<name>/count` and `<name>/total_ns`.
fn counts_and_totals(
    values: &mut Values,
    (header, rows): Table,
    count: &str,
    name: &str,
    rename: fn(&str) -> String,
) -> Result<(), String> {
    let total = column(&header, "Total Time (ns)")?;
    let (count, name) = (column(&header, count)?, column(&header, name)?);
    for row in rows {
        let parse = |i: usize| {
            row.get(i)
                .and_then(|f| number(f))
                .ok_or_else(|| format!("not a number in column {i}: {row:?}"))
        };
        let key = rename(&row[name]);
        values.push((format!("{key}/count"), parse(count)?));
        values.push((format!("{key}/total_ns"), parse(total)?));
    }
    Ok(())
}

/// `nsys stats --report cuda_gpu_kern_sum --report cuda_gpu_mem_time_sum
/// --format csv`: per kernel or memory operation, `<name>/count` and
/// `<name>/total_ns` -- what `gpu.kernel -e topk@kernel` gives as a series'
/// `events` and `total`.
fn nsys_gpu_sum(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let mut values = Values::new();
    for table in nsys_tables(stdout) {
        if table.0.iter().any(|h| h == "Instances") {
            counts_and_totals(&mut values, table, "Instances", "Name", nsys_operation)?;
        } else if table.0.iter().any(|h| h == "Operation") {
            counts_and_totals(&mut values, table, "Count", "Operation", nsys_operation)?;
        }
    }
    if values.is_empty() {
        return Err("no cuda_gpu_kern_sum or cuda_gpu_mem_time_sum table in nsys's output".into());
    }
    Ok(values)
}

/// `nsys stats --report cuda_api_sum --format csv`: per driver call,
/// `<call>/count` and `<call>/total_ns`, the call named as `gpu.api` names it.
fn nsys_api_sum(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let mut values = Values::new();
    for table in nsys_tables(stdout) {
        if table.0.iter().any(|h| h == "Num Calls") {
            counts_and_totals(&mut values, table, "Num Calls", "Name", api_name)?;
        }
    }
    if values.is_empty() {
        return Err("no cuda_api_sum table in nsys's output".into());
    }
    Ok(values)
}

fn median(mut samples: Vec<f64>) -> f64 {
    samples.sort_by(f64::total_cmp);
    let n = samples.len();
    if n % 2 == 1 {
        samples[n / 2]
    } else {
        (samples[n / 2 - 1] + samples[n / 2]) / 2.0
    }
}

/// `ncu --csv --metrics ...`: per kernel and metric, `<kernel>/<metric>` the
/// median over the kernel's launches -- the statistic `gpu.counters` reports
/// as `value` -- and `<kernel>/launches`. ncu's own `==PROF==` lines share
/// standard output and are skipped.
fn ncu_metrics_csv(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let mut lines = stdout.lines().filter(|l| l.starts_with('"'));
    let header = fields(lines.next().ok_or("no CSV header in ncu's output")?);
    let (id, kernel, metric, value) = (
        column(&header, "ID")?,
        column(&header, "Kernel Name")?,
        column(&header, "Metric Name")?,
        column(&header, "Metric Value")?,
    );
    let mut samples: Vec<(String, Vec<f64>)> = Vec::new();
    let mut launches: Vec<(String, Vec<String>)> = Vec::new();
    for line in lines {
        let row = fields(line);
        if row.len() != header.len() {
            return Err(format!(
                "{} fields under a {}-column header: {line:?}",
                row.len(),
                header.len()
            ));
        }
        let v = number(&row[value]).ok_or_else(|| format!("not a number: {row:?}"))?;
        let key = format!("{}/{}", row[kernel], row[metric]);
        match samples.iter_mut().find(|(k, _)| *k == key) {
            Some((_, s)) => s.push(v),
            None => samples.push((key, vec![v])),
        }
        match launches.iter_mut().find(|(k, _)| *k == row[kernel]) {
            Some((_, ids)) if !ids.contains(&row[id]) => ids.push(row[id].clone()),
            Some(_) => {}
            None => launches.push((row[kernel].clone(), vec![row[id].clone()])),
        }
    }
    if samples.is_empty() {
        return Err("ncu profiled no kernel".into());
    }
    let mut values: Values = samples.into_iter().map(|(k, s)| (k, median(s))).collect();
    values.extend(
        launches
            .into_iter()
            .map(|(k, ids)| (format!("{k}/launches"), ids.len() as f64)),
    );
    Ok(values)
}

/// Columns of samples, each already in `omn`'s unit, as `<name>.mean`,
/// `<name>.max` and `<name>.min`, then `samples`: how many rows were read.
fn summarise(columns: Vec<(&str, Vec<f64>)>, rows: usize) -> Result<Values, String> {
    let mut values = Values::new();
    for (name, samples) in columns {
        if samples.is_empty() {
            continue;
        }
        let mean = samples.iter().sum::<f64>() / samples.len() as f64;
        let max = samples.iter().copied().fold(f64::MIN, f64::max);
        let min = samples.iter().copied().fold(f64::MAX, f64::min);
        values.push((format!("{name}.mean"), mean));
        values.push((format!("{name}.max"), max));
        values.push((format!("{name}.min"), min));
    }
    if values.is_empty() {
        return Err("no sample in nvidia-smi's output".into());
    }
    values.push(("samples".into(), rows as f64));
    Ok(values)
}

/// The lines `smi_during.sh` sampled, from whichever stream the runner
/// hands over: standard output when the tool's command ran alone, the
/// launched program's standard error when `omn` launched the wrapper. The
/// workload's own notes are mixed in there; each parser keeps only the lines
/// shaped as its samples.
fn sampled<'a>(stdout: &'a str, stderr: &'a str) -> impl Iterator<Item = &'a str> {
    stdout
        .lines()
        .chain(stderr.lines())
        .filter(|l| !l.trim().is_empty())
}

/// `nvidia-smi --query-gpu=power.draw,clocks.sm,temperature.gpu,
/// utilization.gpu,memory.used --format=csv,noheader,nounits -lms N`, as
/// `workloads/gpu/smi_during.sh gpu` samples it over a workload: W, Hz, C, a
/// fraction, bytes.
fn smi_query_gpu(stdout: &str, stderr: &str) -> Result<Values, String> {
    const COLUMNS: [(&str, f64); 5] = [
        ("power.draw", 1.0),
        ("clocks.sm", 1e6),
        ("temperature.gpu", 1.0),
        ("utilization.gpu", 0.01),
        ("memory.used", 1048576.0),
    ];
    let mut columns: Vec<(&str, Vec<f64>)> =
        COLUMNS.iter().map(|(n, _)| (*n, Vec::new())).collect();
    let mut rows = 0;
    for line in sampled(stdout, stderr) {
        let row = fields(line);
        // A sample is five fields, each a number or `[N/A]` (a column the
        // board does not report); anything else is the workload talking.
        let sample =
            row.len() == COLUMNS.len() && row.iter().all(|f| number(f).is_some() || f == "[N/A]");
        if !sample {
            continue;
        }
        rows += 1;
        for (i, (_, scale)) in COLUMNS.iter().enumerate() {
            if let Some(v) = number(&row[i]) {
                columns[i].1.push(v * scale);
            }
        }
    }
    summarise(columns, rows)
}

/// `nvidia-smi dmon -s puc`: its `pwr`, `gtemp`, `sm`, `mem`, `mclk` and
/// `pclk` columns over the samples, in W, C, fractions and Hz. The header is
/// read rather than assumed: dmon prints only the columns the board has.
fn smi_dmon(stdout: &str, stderr: &str) -> Result<Values, String> {
    const WANTED: [(&str, f64); 6] = [
        ("pwr", 1.0),
        ("gtemp", 1.0),
        ("sm", 0.01),
        ("mem", 0.01),
        ("mclk", 1e6),
        ("pclk", 1e6),
    ];
    let header: Vec<&str> = sampled(stdout, stderr)
        .find(|l| l.starts_with('#'))
        .ok_or("no `#` header in dmon's output")?
        .trim_start_matches('#')
        .split_whitespace()
        .collect();
    let mut columns: Vec<(&str, Vec<f64>)> = WANTED.iter().map(|(n, _)| (*n, Vec::new())).collect();
    let mut rows = 0;
    for line in sampled(stdout, stderr).filter(|l| !l.starts_with('#')) {
        let row: Vec<&str> = line.split_whitespace().collect();
        // A sample has the header's width and starts with the device index;
        // `-` is a reading the board does not have.
        let sample =
            row.len() == header.len() && row.iter().all(|f| number(f).is_some() || *f == "-");
        if !sample {
            continue;
        }
        rows += 1;
        for (i, (name, scale)) in WANTED.iter().enumerate() {
            let Some(at) = header.iter().position(|h| h == name) else {
                continue;
            };
            if let Some(v) = number(row[at]) {
                columns[i].1.push(v * scale);
            }
        }
    }
    summarise(columns, rows)
}

/// `nvidia-smi --query-compute-apps=pid,used_memory --format=csv,noheader,
/// nounits`, one process's rows over a workload (`smi_during.sh apps`):
/// `used_memory.max` and `.min` in bytes -- what `gpu.mem`'s per-process
/// summary gives as `max`, and its first non-zero sample.
fn smi_compute_apps(stdout: &str, stderr: &str) -> Result<Values, String> {
    let used: Vec<f64> = sampled(stdout, stderr)
        .map(fields)
        .filter(|row| row.len() == 2 && row.iter().all(|f| number(f).is_some()))
        .map(|row| number(&row[1]).unwrap_or_default() * 1048576.0)
        .collect();
    let rows = used.len();
    summarise(vec![("used_memory", used)], rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(values: &Values, key: &str) -> f64 {
        values
            .iter()
            .find(|(k, _)| k == key)
            .unwrap_or_else(|| panic!("no `{key}` in {values:?}"))
            .1
    }

    #[test]
    fn every_parser_is_registered_under_its_own_name() {
        let names: Vec<_> = parsers().iter().map(|p| p.name).collect();
        assert_eq!(
            names,
            [
                "nsys_gpu_sum",
                "nsys_api_sum",
                "ncu_metrics_csv",
                "smi_query_gpu",
                "smi_dmon",
                "smi_compute_apps"
            ]
        );
    }

    #[test]
    fn nsys_gpu_sum_reads_kernels_and_memory_operations_in_omns_names() {
        let v = nsys_gpu_sum(include_str!("captures/gpu/nsys_gpu_sum.csv"), "").unwrap();
        assert_eq!(get(&v, "copy/count"), 5.0);
        assert_eq!(get(&v, "copy/total_ns"), 310335.0);
        assert_eq!(get(&v, "[memcpy HtoD]/count"), 1.0);
        assert_eq!(get(&v, "[memcpy HtoD]/total_ns"), 1560666.0);
        assert_eq!(get(&v, "[memcpy DtoH]/total_ns"), 1427898.0);
        assert_eq!(get(&v, "[memset]/count"), 1.0);
        assert_eq!(v.len(), 8);
    }

    #[test]
    fn nsys_gpu_sum_refuses_an_output_without_its_tables() {
        assert!(nsys_gpu_sum("Generating SQLite file\n", "").is_err());
        assert!(nsys_api_sum("", "").is_err());
    }

    #[test]
    fn nsys_api_sum_drops_the_abi_suffix_as_gpu_api_does() {
        let v = nsys_api_sum(include_str!("captures/gpu/nsys_api_sum.csv"), "").unwrap();
        assert_eq!(get(&v, "cuLaunchKernel/count"), 5.0);
        assert_eq!(get(&v, "cuMemAlloc/count"), 3.0);
        assert_eq!(get(&v, "cuMemAlloc/total_ns"), 268194.0);
        assert_eq!(get(&v, "cuMemcpyHtoD/total_ns"), 1523569.0);
        assert_eq!(get(&v, "cuCtxSynchronize/count"), 1.0);
        assert!(v.iter().all(|(k, _)| !k.contains("_v2")));
    }

    #[test]
    fn api_name_strips_only_a_numeric_version() {
        assert_eq!(api_name("cuMemAlloc_v2"), "cuMemAlloc");
        assert_eq!(api_name("cuLaunchKernel"), "cuLaunchKernel");
        assert_eq!(api_name("cuFoo_vx"), "cuFoo_vx");
    }

    #[test]
    fn ncu_metrics_csv_takes_the_median_over_launches_and_skips_prof_lines() {
        let v = ncu_metrics_csv(include_str!("captures/gpu/ncu_metrics.csv"), "").unwrap();
        assert_eq!(get(&v, "copy/launches"), 5.0);
        assert_eq!(get(&v, "copy/smsp__inst_executed.sum"), 808192.0);
        assert_eq!(get(&v, "copy/sm__ctas_launched.sum"), 272.0);
        assert_eq!(get(&v, "copy/l1tex__t_bytes.sum"), 33554432.0);
        // Five launches whose DRAM reads differ: the median, which the
        // 18.5 MB launch does not pull up as it would a mean.
        let dram = get(&v, "copy/dram__bytes_read.sum");
        assert!((16_780_000.0..16_900_000.0).contains(&dram), "{dram}");
    }

    #[test]
    fn fields_keep_quoted_commas() {
        assert_eq!(fields(r#""a, b","808,192",c"#), ["a, b", "808,192", "c"]);
        assert_eq!(number("808,192"), Some(808192.0));
        assert_eq!(number("[N/A]"), None);
        assert_eq!(number("-"), None);
    }

    #[test]
    fn smi_query_gpu_converts_to_omns_units() {
        let v = smi_query_gpu(include_str!("captures/gpu/smi_query_gpu.csv"), "").unwrap();
        assert_eq!(get(&v, "samples"), 16.0);
        assert_eq!(get(&v, "clocks.sm.max"), 1965e6);
        assert_eq!(get(&v, "utilization.gpu.max"), 1.0);
        assert_eq!(get(&v, "memory.used.max"), 355.0 * 1048576.0);
        let power = get(&v, "power.draw.mean");
        assert!((200.0..215.0).contains(&power), "{power}");
    }

    #[test]
    fn smi_dmon_reads_the_columns_its_header_names() {
        let v = smi_dmon(include_str!("captures/gpu/smi_dmon.txt"), "").unwrap();
        assert_eq!(get(&v, "samples"), 4.0);
        assert_eq!(get(&v, "pclk.max"), 1935e6);
        assert_eq!(get(&v, "sm.min"), 0.68);
        assert_eq!(get(&v, "pwr.mean"), (177.0 + 236.0 + 241.0 + 240.0) / 4.0);
        assert!(v.iter().all(|(k, _)| !k.starts_with("mtemp")));
    }

    #[test]
    fn smi_compute_apps_reports_the_process_peak_in_bytes() {
        let v = smi_compute_apps(include_str!("captures/gpu/smi_compute_apps.csv"), "").unwrap();
        assert_eq!(get(&v, "used_memory.max"), 412.0 * 1048576.0);
        assert_eq!(get(&v, "used_memory.min"), 156.0 * 1048576.0);
        assert_eq!(get(&v, "samples"), 9.0);
    }

    #[test]
    fn the_sampler_parsers_skip_the_workloads_notes_on_standard_error() {
        // Captured with omn launching the wrapper (`--program-stderr`): the
        // samples and the workload's notes share one stream.
        let query = include_str!("captures/gpu/smi_query_gpu_mixed.txt");
        assert!(query.contains("kernels over"));
        let v = smi_query_gpu("", query).unwrap();
        assert_eq!(get(&v, "samples"), 16.0);
        assert_eq!(get(&v, "clocks.sm.min"), 300e6);

        let apps = include_str!("captures/gpu/smi_compute_apps_mixed.txt");
        assert!(apps.contains("holding 268435456 bytes"));
        let v = smi_compute_apps("", apps).unwrap();
        assert_eq!(get(&v, "samples"), 9.0);
        assert_eq!(get(&v, "used_memory.max"), 412.0 * 1048576.0);

        let mut dmon = include_str!("captures/gpu/smi_dmon.txt").to_string();
        dmon.push_str("184 kernels over 3000 ms\n");
        let v = smi_dmon("", &dmon).unwrap();
        assert_eq!(get(&v, "samples"), 4.0);
    }

    /// Runs every pair of `catalog/references/gpu.yaml` on this machine,
    /// with the family's own parsers and `compare.omn_jq`, until the shared
    /// runner (stream reference) reads `same_run` and `compare.omn`. Ignored
    /// by default: it drives the GPU for about a minute. Run it alone with
    /// `cargo test --test reference gpu_pairs -- --ignored --nocapture`;
    /// `OMN_GPU_PAIRS=<text>` keeps the pairs whose `use` contains it.
    #[test]
    #[ignore = "drives the GPU for about a minute; the shared runner is the default"]
    fn gpu_pairs_hold_on_this_machine() {
        use serde_norway::Value as Yaml;
        use std::process::Command;
        let root = env!("CARGO_MANIFEST_DIR");
        let omn = env!("CARGO_BIN_EXE_omn");
        let text = std::fs::read_to_string(format!("{root}/catalog/references/gpu.yaml")).unwrap();
        let pairs: Vec<Yaml> = serde_norway::from_str(&text).unwrap();
        let strings = |v: &Yaml| -> Vec<String> {
            v.as_sequence()
                .map(|s| s.iter().map(|x| x.as_str().unwrap().to_string()).collect())
                .unwrap_or_default()
        };
        let have = |tool: &str| {
            Command::new("sh")
                .args(["-c", &format!("command -v {tool}")])
                .output()
                .is_ok_and(|o| o.status.success())
        };
        let scratch = std::env::temp_dir().join(format!("omn-gpu-pairs-{}", std::process::id()));
        std::fs::create_dir_all(&scratch).unwrap();
        let mut failures = Vec::new();
        for (n, pair) in pairs.iter().enumerate() {
            let use_ = pair["use"].as_str().unwrap();
            if let Ok(only) = std::env::var("OMN_GPU_PAIRS") {
                if !use_.contains(&only) {
                    continue;
                }
            }
            let reference = &pair["reference"];
            let missing: Vec<_> = strings(&reference["requires"])
                .into_iter()
                .filter(|t| !have(t))
                .collect();
            if !missing.is_empty() {
                println!("SKIP {use_}: {missing:?} not installed");
                continue;
            }
            let workload = strings(&pair["workload"]);
            let command = strings(&reference["command"]);
            let same_run = pair["same_run"].as_str() == Some("omn_launches_tool");
            let program_stderr = scratch.join(format!("{n}.stderr"));
            let mut omn_args = strings(&pair["omn"]);
            if same_run {
                omn_args.push("--program-stderr".into());
                omn_args.push(program_stderr.to_str().unwrap().into());
                omn_args.push("--".into());
                omn_args.extend(command.iter().cloned());
            } else {
                omn_args.push("--".into());
            }
            omn_args.extend(workload.iter().cloned());
            let omn_out = Command::new(omn)
                .current_dir(root)
                .args(&omn_args)
                .output()
                .unwrap();
            assert!(
                omn_out.status.success(),
                "{use_}: omn failed: {}",
                String::from_utf8_lossy(&omn_out.stderr)
            );
            let (stdout, stderr) = if same_run {
                (
                    String::new(),
                    std::fs::read_to_string(&program_stderr).unwrap(),
                )
            } else {
                let out = Command::new(&command[0])
                    .current_dir(root)
                    .args(&command[1..])
                    .args(&workload)
                    .output()
                    .unwrap();
                assert!(out.status.success(), "{use_}: the tool failed");
                (
                    String::from_utf8_lossy(&out.stdout).into_owned(),
                    String::from_utf8_lossy(&out.stderr).into_owned(),
                )
            };
            let name = reference["parse"].as_str().unwrap();
            let parser = parsers().into_iter().find(|p| p.name == name).unwrap();
            let values = (parser.parse)(&stdout, &stderr).unwrap();
            let key = pair["compare"]["key"].as_str().unwrap();
            let tool = get(&values, key);
            let jq = Command::new("jq")
                .arg(pair["compare"]["omn_jq"].as_str().unwrap())
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .and_then(|mut child| {
                    use std::io::Write;
                    child.stdin.take().unwrap().write_all(&omn_out.stdout)?;
                    child.wait_with_output()
                })
                .unwrap();
            let ours: f64 = String::from_utf8_lossy(&jq.stdout)
                .trim()
                .parse()
                .unwrap_or_else(|_| panic!("{use_}: no omn value for {key}"));
            let tolerance = &pair["compare"]["tolerance"];
            let (within, band) = match (tolerance["rel"].as_f64(), tolerance["abs"].as_f64()) {
                (Some(rel), _) => (
                    (ours - tool).abs() <= rel * tool.abs(),
                    format!("rel {rel}"),
                ),
                (_, Some(abs)) => ((ours - tool).abs() <= abs, format!("abs {abs}")),
                _ => panic!("{use_}: no tolerance"),
            };
            let verdict = if within { "PASS" } else { "FAIL" };
            println!("{verdict} {use_}: omn {ours} tool {tool} ({band})");
            if !within {
                failures.push(use_.to_string());
            }
        }
        let _ = std::fs::remove_dir_all(&scratch);
        assert!(failures.is_empty(), "outside their tolerance: {failures:?}");
    }

    #[test]
    fn a_sampler_with_no_sample_is_an_error_not_a_zero() {
        assert!(smi_query_gpu("", "context open\n").is_err());
        assert!(smi_compute_apps("", "").is_err());
        assert!(smi_dmon("", "").is_err());
    }
}
