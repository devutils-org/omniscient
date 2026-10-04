//! Parsers for the `sysstat` family's reference tools. Each turns a tool's
//! output into the values a pair compares (BRIEF.md, "Decided before this
//! wave").
//!
//! Every busy share here is `(100 - %idle - %iowait) / 100`, a ratio like
//! `cpu.util`'s: what each tool calls user, system, irq or steal is a
//! different split of the same busy time, and iowait is idle by the kernel's
//! own account.

use crate::common::{Parser, Values};

pub fn parsers() -> Vec<Parser> {
    vec![
        Parser {
            name: "vmstat",
            parse: vmstat,
        },
        Parser {
            name: "numastat_p",
            parse: numastat_p,
        },
        Parser {
            name: "pidstat_r",
            parse: pidstat_r,
        },
        Parser {
            name: "pidstat_w",
            parse: pidstat_w,
        },
        Parser {
            name: "mpstat",
            parse: mpstat,
        },
        Parser {
            name: "sar_u",
            parse: sar_u,
        },
        Parser {
            name: "iostat_cpu",
            parse: iostat_cpu,
        },
        Parser {
            name: "psi_window",
            parse: psi_window,
        },
    ]
}

fn num(token: &str, what: &str) -> Result<f64, String> {
    token
        .parse::<f64>()
        .map_err(|_| format!("{what}: `{token}` is not a number"))
}

/// `vmstat -n N COUNT`: the header row names the columns; the first data row
/// is the average since boot and is left out when there is another. Each
/// column's mean, plus `busy` (us + sy as a ratio) and `si_bytes`/`so_bytes`
/// (kB/s as bytes/s).
fn vmstat(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let mut lines = stdout.lines();
    let header: Vec<&str> = lines
        .by_ref()
        .find(|l| {
            let t: Vec<&str> = l.split_whitespace().collect();
            t.first() == Some(&"r") && t.contains(&"us")
        })
        .ok_or("vmstat: no header row with `r` and `us`")?
        .split_whitespace()
        .collect();
    let mut rows = Vec::new();
    for line in lines {
        let t: Vec<&str> = line.split_whitespace().collect();
        if t.len() != header.len() {
            continue;
        }
        let row = t
            .iter()
            .map(|v| num(v, "vmstat"))
            .collect::<Result<Vec<f64>, String>>()?;
        rows.push(row);
    }
    if rows.len() > 1 {
        rows.remove(0);
    }
    if rows.is_empty() {
        return Err("vmstat: no data row".into());
    }
    let n = rows.len() as f64;
    let mut out: Values = header
        .iter()
        .enumerate()
        .map(|(i, name)| (name.to_string(), rows.iter().map(|r| r[i]).sum::<f64>() / n))
        .collect();
    let get = |k: &str| out.iter().find(|(n, _)| n == k).map(|(_, v)| *v);
    let (us, sy) = (get("us").unwrap_or(0.0), get("sy").unwrap_or(0.0));
    let si = get("si");
    let so = get("so");
    out.push(("busy".into(), (us + sy) / 100.0));
    if let Some(si) = si {
        out.push(("si_bytes".into(), si * 1024.0));
    }
    if let Some(so) = so {
        out.push(("so_bytes".into(), so * 1024.0));
    }
    Ok(out)
}

/// `numastat -p PID`: the `Total` row, per node, in 4 KiB pages (`node:<n>`,
/// the key `mem.numa` gives its series) and in all (`total`). numastat
/// prints MB.
fn numastat_p(stdout: &str, _stderr: &str) -> Result<Values, String> {
    const PAGES_PER_MB: f64 = 1048576.0 / 4096.0;
    let mut nodes: Vec<String> = Vec::new();
    let mut total_row = None;
    for line in stdout.lines() {
        let t: Vec<&str> = line.split_whitespace().collect();
        if t.first() == Some(&"Node") && nodes.is_empty() {
            let mut i = 0;
            while i < t.len() {
                if t[i] == "Node" && i + 1 < t.len() {
                    nodes.push(format!("node:{}", t[i + 1]));
                    i += 2;
                } else {
                    i += 1;
                }
            }
        } else if t.first() == Some(&"Total") && t.len() > 1 {
            total_row = Some(t[1..].to_vec());
        }
    }
    if nodes.is_empty() {
        return Err("numastat -p: no `Node` header".into());
    }
    let row = total_row.ok_or("numastat -p: no `Total` row")?;
    if row.len() != nodes.len() + 1 {
        return Err(format!(
            "numastat -p: {} nodes but {} values in `Total`",
            nodes.len(),
            row.len()
        ));
    }
    let mut out = Values::new();
    for (node, v) in nodes.iter().zip(&row) {
        out.push((node.clone(), num(v, "numastat -p")? * PAGES_PER_MB));
    }
    out.push((
        "total".into(),
        num(row[nodes.len()], "numastat -p")? * PAGES_PER_MB,
    ));
    Ok(out)
}

/// A sysstat table (`mpstat`, `sar`, `pidstat`, run with `LC_ALL=C`): every
/// header row starts with a time or `Average:` and names the columns that
/// follow; every data row lines up with the last header. Returns, per data
/// row, whether it is an `Average:` row and its cells by column name.
type Row = (bool, Vec<(String, String)>);

fn sysstat_rows(stdout: &str, tool: &str) -> Result<Vec<Row>, String> {
    let mut header: Option<Vec<String>> = None;
    let mut rows = Vec::new();
    for line in stdout.lines() {
        let t: Vec<&str> = line.split_whitespace().collect();
        if t.len() < 2 || t[0] == "Linux" {
            continue;
        }
        if t[1..].iter().any(|c| *c == "CPU" || *c == "UID") {
            header = Some(t[1..].iter().map(|s| s.to_string()).collect());
            continue;
        }
        let Some(h) = &header else { continue };
        if t.len() - 1 != h.len() {
            continue;
        }
        let cells = h
            .iter()
            .cloned()
            .zip(t[1..].iter().map(|s| s.to_string()))
            .collect();
        rows.push((t[0] == "Average:", cells));
    }
    if rows.is_empty() {
        return Err(format!("{tool}: no data row under a header"));
    }
    Ok(rows)
}

fn cell<'a>(cells: &'a [(String, String)], name: &str) -> Option<&'a str> {
    cells
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

/// The busy share of one cpu row: 100 minus %idle and %iowait, as a ratio.
fn busy_of(cells: &[(String, String)], tool: &str) -> Result<f64, String> {
    let idle = num(
        cell(cells, "%idle").ok_or(format!("{tool}: no %idle"))?,
        tool,
    )?;
    let iowait = num(
        cell(cells, "%iowait").ok_or(format!("{tool}: no %iowait"))?,
        tool,
    )?;
    Ok((100.0 - idle - iowait) / 100.0)
}

/// The `Average:` rows of a per-cpu table: `<cpu>` (`all`, `0`, ...) is the
/// busy share and `<cpu>.<column>` each column as printed.
fn cpu_averages(stdout: &str, tool: &str) -> Result<Values, String> {
    let mut out = Values::new();
    for (avg, cells) in sysstat_rows(stdout, tool)? {
        if !avg {
            continue;
        }
        let cpu = cell(&cells, "CPU").ok_or(format!("{tool}: no CPU column"))?;
        out.push((cpu.to_string(), busy_of(&cells, tool)?));
        for (name, v) in &cells {
            if name != "CPU" {
                out.push((format!("{cpu}.{name}"), num(v, tool)?));
            }
        }
    }
    if out.is_empty() {
        return Err(format!("{tool}: no `Average:` row"));
    }
    Ok(out)
}

/// `mpstat [-P ALL] N COUNT`.
fn mpstat(stdout: &str, _stderr: &str) -> Result<Values, String> {
    cpu_averages(stdout, "mpstat")
}

/// `sar -u N COUNT`, live.
fn sar_u(stdout: &str, _stderr: &str) -> Result<Values, String> {
    cpu_averages(stdout, "sar -u")
}

/// `pidstat -r N -e PROGRAM`: per command, the largest RSS any interval
/// printed, in bytes (pidstat prints kB). The `Average:` row is a mean over a
/// ramp and is not the footprint.
fn pidstat_r(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let mut out: Values = Vec::new();
    for (avg, cells) in sysstat_rows(stdout, "pidstat -r")? {
        if avg {
            continue;
        }
        let cmd = cell(&cells, "Command").ok_or("pidstat -r: no Command column")?;
        let rss = num(
            cell(&cells, "RSS").ok_or("pidstat -r: no RSS column")?,
            "pidstat -r",
        )? * 1024.0;
        match out.iter_mut().find(|(k, _)| k == cmd) {
            Some((_, v)) => *v = v.max(rss),
            None => out.push((cmd.to_string(), rss)),
        }
    }
    if out.is_empty() {
        return Err("pidstat -r: no interval row".into());
    }
    Ok(out)
}

/// `pidstat -w N -e PROGRAM`: per command, the `Average:` switches per
/// second, voluntary and not summed (`<cmd>`), and each as printed.
fn pidstat_w(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let mut out = Values::new();
    for (avg, cells) in sysstat_rows(stdout, "pidstat -w")? {
        if !avg {
            continue;
        }
        let cmd = cell(&cells, "Command").ok_or("pidstat -w: no Command column")?;
        let v = num(
            cell(&cells, "cswch/s").ok_or("pidstat -w: no cswch/s")?,
            "pidstat -w",
        )?;
        let nv = num(
            cell(&cells, "nvcswch/s").ok_or("pidstat -w: no nvcswch/s")?,
            "pidstat -w",
        )?;
        out.push((cmd.to_string(), v + nv));
        out.push((format!("{cmd}.cswch/s"), v));
        out.push((format!("{cmd}.nvcswch/s"), nv));
    }
    if out.is_empty() {
        return Err("pidstat -w: no `Average:` row".into());
    }
    Ok(out)
}

/// `iostat -c N COUNT`: the `avg-cpu` blocks, the first (since boot) left
/// out when there is another; `all` is the mean busy share.
fn iostat_cpu(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let mut header: Option<Vec<String>> = None;
    let mut shares = Vec::new();
    for line in stdout.lines() {
        let t: Vec<&str> = line.split_whitespace().collect();
        if t.first() == Some(&"avg-cpu:") {
            header = Some(t[1..].iter().map(|s| s.to_string()).collect());
            continue;
        }
        if let Some(h) = header.take() {
            if t.len() != h.len() {
                return Err(format!(
                    "iostat -c: {} columns, {} values",
                    h.len(),
                    t.len()
                ));
            }
            let cells: Vec<(String, String)> =
                h.into_iter().zip(t.iter().map(|s| s.to_string())).collect();
            shares.push(busy_of(&cells, "iostat -c")?);
        }
    }
    if shares.len() > 1 {
        shares.remove(0);
    }
    if shares.is_empty() {
        return Err("iostat -c: no avg-cpu block".into());
    }
    let mean = shares.iter().sum::<f64>() / shares.len() as f64;
    Ok(vec![("all".into(), mean)])
}

/// Two reads of a `/proc/pressure/<resource>` file, each after a
/// `date +%s%N` line: `some` and `full` are the share of the window between
/// the two dates that each total grew by (totals are in microseconds).
fn psi_window(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let mut stamps = Vec::new();
    let mut totals: Vec<(String, f64)> = Vec::new();
    for line in stdout.lines() {
        let t: Vec<&str> = line.split_whitespace().collect();
        match t.first() {
            Some(&kind @ ("some" | "full")) => {
                let total = t
                    .iter()
                    .find_map(|f| f.strip_prefix("total="))
                    .ok_or("psi: no total= field")?;
                totals.push((kind.to_string(), num(total, "psi")?));
            }
            Some(stamp) if t.len() == 1 => stamps.push(num(stamp, "psi date")?),
            _ => {}
        }
    }
    if stamps.len() != 2 {
        return Err(format!("psi: {} timestamps, two expected", stamps.len()));
    }
    let window_ns = stamps[1] - stamps[0];
    if window_ns <= 0.0 {
        return Err("psi: the window is empty".into());
    }
    let mut out = Values::new();
    for kind in ["some", "full"] {
        let seen: Vec<f64> = totals
            .iter()
            .filter(|(k, _)| k == kind)
            .map(|(_, v)| *v)
            .collect();
        if let [before, after] = seen[..] {
            out.push((kind.to_string(), (after - before) * 1000.0 / window_ns));
        }
    }
    if out.is_empty() {
        return Err("psi: no line read twice".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    //! Each parser on a committed capture. The captures of `vmstat`,
    //! `numastat -p` and the PSI window were produced on this machine; the
    //! sysstat tools are not installed here, so `mpstat`, `sar -u`,
    //! `iostat -c` and `pidstat -r`/`-w` are written in sysstat 12's
    //! documented `LC_ALL=C` layout, with numbers chosen to check the
    //! arithmetic.

    use super::*;

    fn capture(name: &str) -> String {
        let path = format!(
            "{}/tests/reference/captures/sysstat/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    fn get(values: &Values, key: &str) -> f64 {
        values
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| *v)
            .unwrap_or_else(|| panic!("no `{key}` in {values:?}"))
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9 * a.abs().max(b.abs()).max(1.0)
    }

    #[test]
    fn every_parser_is_named_once() {
        let names: Vec<&str> = parsers().iter().map(|p| p.name).collect();
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "{names:?}");
    }

    #[test]
    fn vmstat_leaves_out_the_since_boot_row() {
        let v = vmstat(&capture("vmstat.txt"), "").unwrap();
        // Rows 2 and 3 of the capture: us 91 and 91, sy 9 and 9.
        assert!(close(get(&v, "us"), 91.0));
        assert!(close(get(&v, "busy"), 1.0));
        assert!(close(get(&v, "cs"), (20758.0 + 20515.0) / 2.0));
        assert!(close(get(&v, "si_bytes"), 0.0));
    }

    #[test]
    fn vmstat_refuses_what_is_not_vmstat() {
        assert!(vmstat("hello\n1 2 3\n", "").is_err());
    }

    #[test]
    fn numastat_p_converts_megabytes_to_pages() {
        let v = numastat_p(&capture("numastat-p.txt"), "").unwrap();
        assert!(close(get(&v, "node:0"), 4.23 * 256.0));
        assert!(close(get(&v, "total"), 4.23 * 256.0));
    }

    #[test]
    fn mpstat_reads_the_average_rows() {
        let v = mpstat(&capture("mpstat.txt"), "").unwrap();
        assert!(close(get(&v, "all"), (100.0 - 48.75 - 0.25) / 100.0));
        assert!(close(get(&v, "0"), 1.0));
        assert!(close(get(&v, "1.%sys"), 1.5));
    }

    #[test]
    fn sar_u_reads_the_average_row() {
        let v = sar_u(&capture("sar-u.txt"), "").unwrap();
        assert!(close(get(&v, "all"), (100.0 - 49.0 - 0.25) / 100.0));
    }

    #[test]
    fn iostat_cpu_leaves_out_the_since_boot_block() {
        let v = iostat_cpu(&capture("iostat-c.txt"), "").unwrap();
        let want = ((100.0 - 49.0 - 0.25) + (100.0 - 49.0 - 0.25)) / 200.0;
        assert!(close(get(&v, "all"), want));
    }

    #[test]
    fn pidstat_r_takes_the_largest_interval_rss() {
        let v = pidstat_r(&capture("pidstat-r.txt"), "").unwrap();
        assert!(close(get(&v, "python3"), 140944.0 * 1024.0));
    }

    #[test]
    fn pidstat_w_sums_both_kinds() {
        let v = pidstat_w(&capture("pidstat-w.txt"), "").unwrap();
        assert!(close(get(&v, "python3"), 943.0));
        assert!(close(get(&v, "python3.nvcswch/s"), 2.0));
    }

    #[test]
    fn psi_window_divides_the_growth_by_the_window() {
        let v = psi_window(&capture("psi-cpu.txt"), "").unwrap();
        let window = 1791135351139810781.0 - 1791135350136947216.0;
        assert!(close(
            get(&v, "some"),
            (2419047371.0 - 2419045073.0) * 1000.0 / window
        ));
        assert!(close(get(&v, "full"), 0.0));
    }

    #[test]
    fn psi_window_refuses_one_read() {
        assert!(psi_window("123\nsome avg10=0 total=5\n", "").is_err());
    }
}

/// This family's pairs, run until the shared runner (stream reference) knows
/// `target: system` and the `raw.<attr>` path: every pair in
/// `catalog/references/sysstat.yaml`, run, parsed, compared, or skipped with
/// the machine's reason. A failure lists every pair that missed, at once.
#[cfg(test)]
mod pairs {
    use super::parsers;
    use serde::Deserialize;
    use serde_json::Value;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    #[derive(Deserialize)]
    struct Pair {
        source: String,
        #[serde(rename = "use")]
        use_: String,
        #[serde(default)]
        workload: Vec<String>,
        omn: Vec<String>,
        #[serde(default)]
        target: Option<String>,
        reference: Reference,
        compare: Compare,
        #[serde(default)]
        known_defect: Option<String>,
    }

    #[derive(Deserialize)]
    struct Reference {
        command: Vec<String>,
        parse: String,
        #[serde(default)]
        requires: Vec<String>,
        #[serde(default)]
        privileged: bool,
    }

    #[derive(Deserialize)]
    struct Compare {
        key: String,
        omn: String,
        #[serde(default)]
        omn_key: Option<String>,
        #[serde(default)]
        scale: Option<f64>,
        tolerance: Tolerance,
    }

    #[derive(Deserialize)]
    struct Tolerance {
        #[serde(default)]
        rel: Option<f64>,
        #[serde(default)]
        abs: Option<f64>,
    }

    fn on_path(bin: &str) -> bool {
        std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
            .unwrap_or(false)
    }

    fn omn_value(json: &Value, path: &str, key: Option<&str>) -> Result<f64, String> {
        let result = &json["results"][0];
        let (emit, field) = path.split_once('.').ok_or(format!("`{path}`: no emit"))?;
        let output = result["outputs"]
            .as_array()
            .and_then(|o| o.iter().find(|o| o["emit"] == emit))
            .ok_or(format!("no `{emit}` output in omn's results"))?;
        if emit == "raw" {
            let events = output["events"].as_array().ok_or("raw: no events")?;
            // A leading event stamped less than half a period after the
            // window opened covers a few ms, not a period: cpu.util and
            // cpu.pressure emit one (docs/fixes/w22-sysstat.md), mem.pressure
            // and io.pressure do not. It is left out wherever it appears.
            let opened = result["window"]["opened_ns"].as_f64().unwrap_or(0.0);
            let ts = |i: usize| events.get(i).and_then(|e| e["ts_ns"].as_f64());
            let skip = match (ts(0), ts(1)) {
                (Some(t0), Some(t1)) if t0 - opened < (t1 - t0) / 2.0 => 1,
                _ => 0,
            };
            let values: Vec<f64> = events
                .iter()
                .skip(skip)
                .filter_map(|e| {
                    e["attrs"].as_array()?.iter().find(|a| a["name"] == field)?["value"].as_f64()
                })
                .collect();
            if values.is_empty() {
                return Err(format!("raw: no `{field}` after the first event"));
            }
            return Ok(values.iter().sum::<f64>() / values.len() as f64);
        }
        let series = output["series"].as_array().ok_or("no series")?;
        let one = match key {
            Some(k) => series.iter().find(|s| s["key"] == k),
            None => series.first(),
        }
        .ok_or(format!("no series {key:?} in `{emit}`"))?;
        one[field]
            .as_f64()
            .ok_or(format!("`{emit}` series has no `{field}`"))
    }

    fn run(pair: &Pair, root: &Path) -> Result<String, String> {
        for bin in &pair.reference.requires {
            if !on_path(bin) {
                return Err(format!("SKIP: `{bin}` is not installed"));
            }
        }
        if !pair.workload.is_empty() && !on_path(&pair.workload[0]) {
            return Err(format!("SKIP: `{}` is not installed", pair.workload[0]));
        }
        let mut reference = pair.reference.command.clone();
        if pair.reference.privileged {
            let granted = Command::new("sudo")
                .args(["-n", "true"])
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !granted {
                return Err("SKIP: privileged, and `sudo -n` is not granted".into());
            }
            reference.splice(0..0, ["sudo".to_string(), "-n".to_string()]);
        }
        let omn_bin = env!("CARGO_BIN_EXE_omn");
        let system = pair.target.as_deref() == Some("system");
        let mut omn_args = pair.omn.clone();
        let (omn_out, ref_out) = if system {
            let mut background = if pair.workload.is_empty() {
                None
            } else {
                let child = Command::new(&pair.workload[0])
                    .args(&pair.workload[1..])
                    .current_dir(root)
                    .stdout(Stdio::null())
                    .spawn()
                    .map_err(|e| format!("workload: {e}"))?;
                std::thread::sleep(Duration::from_millis(300));
                Some(child)
            };
            let omn = Command::new(omn_bin)
                .args(&omn_args)
                .current_dir(root)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| format!("omn: {e}"))?;
            let tool = Command::new(&reference[0])
                .args(&reference[1..])
                .current_dir(root)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| format!("{}: {e}", reference[0]))?;
            let o = omn.wait_with_output().map_err(|e| e.to_string())?;
            let t = tool.wait_with_output().map_err(|e| e.to_string())?;
            if let Some(child) = background.as_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
            (o, t)
        } else {
            omn_args.push("--".into());
            omn_args.extend(pair.workload.iter().cloned());
            reference.extend(pair.workload.iter().cloned());
            let o = Command::new(omn_bin)
                .args(&omn_args)
                .current_dir(root)
                .stdin(Stdio::null())
                .output()
                .map_err(|e| format!("omn: {e}"))?;
            let t = Command::new(&reference[0])
                .args(&reference[1..])
                .current_dir(root)
                .stdin(Stdio::null())
                .output()
                .map_err(|e| format!("{}: {e}", reference[0]))?;
            (o, t)
        };
        let omn_stderr = String::from_utf8_lossy(&omn_out.stderr);
        if !omn_out.status.success() {
            return Err(format!("omn failed: {}", omn_stderr.trim()));
        }
        let json: Value =
            serde_json::from_slice(&omn_out.stdout).map_err(|e| format!("omn's JSON: {e}"))?;
        let parser = parsers()
            .into_iter()
            .find(|p| p.name == pair.reference.parse)
            .ok_or(format!("no parser `{}`", pair.reference.parse))?;
        let values = (parser.parse)(
            &String::from_utf8_lossy(&ref_out.stdout),
            &String::from_utf8_lossy(&ref_out.stderr),
        )?;
        let theirs = values
            .iter()
            .find(|(k, _)| *k == pair.compare.key)
            .map(|(_, v)| *v)
            .ok_or(format!("the tool gave no `{}`", pair.compare.key))?;
        let ours = omn_value(&json, &pair.compare.omn, pair.compare.omn_key.as_deref())?
            * pair.compare.scale.unwrap_or(1.0);
        let diff = (ours - theirs).abs();
        let tol = &pair.compare.tolerance;
        let within = tol.abs.is_some_and(|a| diff <= a)
            || tol
                .rel
                .is_some_and(|r| diff <= r * ours.abs().max(theirs.abs()));
        let line = format!("omn {ours:.6} vs tool {theirs:.6} (|diff| {diff:.6})");
        if within {
            Ok(line)
        } else {
            Err(format!("outside the tolerance: {line}"))
        }
    }

    #[test]
    fn sysstat_pairs_agree_or_skip_with_the_reason() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(root.join("catalog/references/sysstat.yaml"))
            .expect("catalog/references/sysstat.yaml");
        let pairs: Vec<Pair> = serde_norway::from_str(&text).expect("sysstat.yaml parses");
        let mut failed = Vec::new();
        for pair in &pairs {
            let outcome = run(pair, root);
            if let Some(defect) = &pair.known_defect {
                match outcome {
                    Err(reason) if reason.starts_with("outside the tolerance") => {
                        eprintln!("KNOWN {} / {}: {reason} ({defect})", pair.source, pair.use_)
                    }
                    Err(reason) if reason.starts_with("SKIP") => {
                        eprintln!("{reason}: {} / {}", pair.source, pair.use_)
                    }
                    Ok(line) => failed.push(format!(
                        "{} / {}: agrees now ({line}); drop its known_defect",
                        pair.source, pair.use_
                    )),
                    Err(reason) => {
                        failed.push(format!("{} / {}: {reason}", pair.source, pair.use_))
                    }
                }
                continue;
            }
            match outcome {
                Ok(line) => eprintln!("PASS {} / {}: {line}", pair.source, pair.use_),
                Err(reason) if reason.starts_with("SKIP") => {
                    eprintln!("{reason}: {} / {}", pair.source, pair.use_)
                }
                Err(reason) => {
                    eprintln!("FAIL {} / {}: {reason}", pair.source, pair.use_);
                    failed.push(format!("{} / {}: {reason}", pair.source, pair.use_));
                }
            }
        }
        assert!(failed.is_empty(), "{failed:#?}");
    }

    /// Whether one short run of `source` emits a leading raw event stamped
    /// within half a period of the window's opening: defect 2 of
    /// docs/fixes/w22-sysstat.md, which `omn_value`'s `raw.<attr>` path
    /// leaves out. `None` when the run gave fewer than two events.
    fn leading_event(source: &str) -> Result<Option<bool>, String> {
        let out = Command::new(env!("CARGO_BIN_EXE_omn"))
            .args([source, "--system", "-m", "poll@50ms", "-e", "raw"])
            .args(["-d", "175ms", "--format", "json"])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("omn: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "omn {source} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let json: Value =
            serde_json::from_slice(&out.stdout).map_err(|e| format!("omn's JSON: {e}"))?;
        let result = &json["results"][0];
        let opened = result["window"]["opened_ns"]
            .as_f64()
            .ok_or("no window.opened_ns")?;
        let events = result["outputs"]
            .as_array()
            .and_then(|o| o.iter().find(|o| o["emit"] == "raw"))
            .and_then(|o| o["events"].as_array())
            .ok_or("no raw events")?;
        let ts = |i: usize| events.get(i).and_then(|e| e["ts_ns"].as_f64());
        Ok(match (ts(0), ts(1)) {
            (Some(t0), Some(t1)) => Some(t0 - opened < (t1 - t0) / 2.0),
            _ => None,
        })
    }

    /// Holds defect 2 the way `known_defect` holds a pair, which no pair can
    /// do for it: the leading event appears in one run out of six to four out
    /// of five (measured), and its share is 0, 0.5, 0.67 or 1, so a
    /// `summary.mean` pair would miss or agree by chance. Instead: `cpu.util`
    /// and `cpu.pressure` must still show the leading event within forty runs,
    /// or the defect is fixed and `omn_value`'s skip and this test go;
    /// `mem.pressure` and `io.pressure` must not show it in five runs, or the
    /// defect has spread to them.
    #[test]
    fn poll_leading_event_is_held_as_a_known_defect() {
        let mut failed = Vec::new();
        for source in ["cpu.util", "cpu.pressure"] {
            let mut seen = false;
            for _ in 0..40 {
                match leading_event(source) {
                    Ok(Some(true)) => {
                        seen = true;
                        break;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        failed.push(e);
                        break;
                    }
                }
            }
            if seen {
                eprintln!("KNOWN {source}: a leading event within half a period (defect 2)");
            } else if failed.is_empty() {
                failed.push(format!(
                    "{source}: no leading event in forty runs; defect 2 looks fixed, \
                     drop omn_value's skip and this test"
                ));
            }
        }
        for source in ["mem.pressure", "io.pressure"] {
            for _ in 0..5 {
                match leading_event(source) {
                    Ok(Some(true)) => {
                        failed.push(format!(
                            "{source}: a leading event within half a period; defect 2 \
                             has spread to it"
                        ));
                        break;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        failed.push(e);
                        break;
                    }
                }
            }
        }
        assert!(failed.is_empty(), "{failed:#?}");
    }
}
