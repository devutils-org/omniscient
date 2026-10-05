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
        Parser {
            name: "omn_cpu_util",
            parse: omn_cpu_util,
        },
        Parser {
            name: "omn_mem_swap",
            parse: omn_mem_swap,
        },
        Parser {
            name: "omn_psi",
            parse: omn_psi,
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

// --- omn's side ---------------------------------------------------------

/// The window's opening and every raw event's stamp, of `omn -e raw`'s JSON.
fn window_and_stamps(json: &serde_json::Value) -> Result<(f64, Vec<f64>), String> {
    let result = &json["results"][0];
    let opened = result["window"]["opened_ns"]
        .as_f64()
        .ok_or("omn: no window.opened_ns")?;
    let stamps = raw_events(json)?
        .iter()
        .map(|e| e["ts_ns"].as_f64().ok_or("omn: a raw event has no ts_ns"))
        .collect::<Result<_, _>>()?;
    Ok((opened, stamps))
}

fn raw_events(json: &serde_json::Value) -> Result<&Vec<serde_json::Value>, String> {
    json["results"][0]["outputs"]
        .as_array()
        .and_then(|o| o.iter().find(|o| o["emit"] == "raw"))
        .and_then(|o| o["events"].as_array())
        .ok_or_else(|| "omn: no `raw` output".to_string())
}

/// `omn <source> --system -m poll@1s -e raw --format json`: each numeric
/// attribute's mean over the raw events, keyed by its name. Every event covers
/// a full period -- a poll's first read is its baseline, defect 2 of
/// `docs/fixes/w22-sysstat.md`, fixed in the twenty-fourth wave and held by
/// `poll_has_no_leading_event` -- so with equal windows the mean of the
/// shares is the share of the whole, which is what the tools print.
fn raw_means(stdout: &str) -> Result<Values, String> {
    let json: serde_json::Value =
        serde_json::from_str(stdout).map_err(|e| format!("omn's JSON: {e}"))?;
    let events = raw_events(&json)?;
    let mut sums: Vec<(String, f64, usize)> = Vec::new();
    for event in events {
        for attr in event["attrs"].as_array().into_iter().flatten() {
            let (Some(name), Some(v)) = (attr["name"].as_str(), attr["value"].as_f64()) else {
                continue;
            };
            match sums.iter_mut().find(|(n, _, _)| n == name) {
                Some((_, s, c)) => {
                    *s += v;
                    *c += 1;
                }
                None => sums.push((name.to_string(), v, 1)),
            }
        }
    }
    if sums.is_empty() {
        return Err("omn: no numeric attribute in a raw event".into());
    }
    Ok(sums
        .into_iter()
        .map(|(n, s, c)| (n, s / c as f64))
        .collect())
}

fn renamed(means: &Values, names: &[(&str, &str)]) -> Values {
    names
        .iter()
        .filter_map(|(from, to)| {
            means
                .iter()
                .find(|(k, _)| k == from)
                .map(|(_, v)| (to.to_string(), *v))
        })
        .collect()
}

/// `cpu.util --system`'s busy share, as `busy` (vmstat's key) and `all`
/// (mpstat's, sar's and iostat's).
fn omn_cpu_util(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let means = raw_means(stdout)?;
    Ok(renamed(&means, &[("busy", "busy"), ("busy", "all")]))
}

/// `mem.swap --system`'s flows per tick, under vmstat's keys: at poll@1s a
/// tick is a second, so `in_bytes` is `si_bytes` per second.
fn omn_mem_swap(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let means = raw_means(stdout)?;
    Ok(renamed(
        &means,
        &[("in_bytes", "si_bytes"), ("out_bytes", "so_bytes")],
    ))
}

/// A pressure source's window shares under the PSI file's line names:
/// `cpu.pressure` calls its share `some` (defect 3: unlike its siblings) and
/// has no `full` share; `mem.pressure` and `io.pressure` call them
/// `stall_ratio` and `full_stall_ratio`.
fn omn_psi(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let means = raw_means(stdout)?;
    Ok(renamed(
        &means,
        &[("stall_ratio", "some"), ("full_stall_ratio", "full")],
    ))
}

#[cfg(test)]
mod tests {
    //! Each parser on a committed capture, every one produced on this machine
    //! (sysstat 12.8.0, `LC_ALL=C`; `omn` on 2026-10-04): the tools' under
    //! the commands of `catalog/references/sysstat.yaml`, `omn`'s under the
    //! pairs' `omn` arguments with `-d 2s` (`omn-cpu-util-raw.json` and
    //! `omn-cpu-pressure-raw.json` with `-d 3s` on 2026-10-05, once a poll's
    //! first read became its baseline).

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
        assert!(close(get(&v, "all"), (100.0 - 66.02 - 7.02) / 100.0));
        assert!(close(get(&v, "all.%sys"), 5.16));
    }

    #[test]
    fn sar_u_reads_the_average_row() {
        let v = sar_u(&capture("sar-u.txt"), "").unwrap();
        assert!(close(get(&v, "all"), (100.0 - 66.01 - 7.02) / 100.0));
    }

    #[test]
    fn iostat_cpu_leaves_out_the_since_boot_block() {
        let v = iostat_cpu(&capture("iostat-c.txt"), "").unwrap();
        let want = ((100.0 - 69.00 - 7.30) + (100.0 - 63.24 - 6.71)) / 200.0;
        assert!(close(get(&v, "all"), want));
    }

    #[test]
    fn pidstat_r_takes_the_largest_interval_rss() {
        let v = pidstat_r(&capture("pidstat-r.txt"), "").unwrap();
        assert!(close(get(&v, "python3"), 139840.0 * 1024.0));
    }

    #[test]
    fn pidstat_w_sums_both_kinds() {
        let v = pidstat_w(&capture("pidstat-w.txt"), "").unwrap();
        assert!(close(get(&v, "python3"), 913.0));
        assert!(close(get(&v, "python3.nvcswch/s"), 13.0));
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

    #[test]
    fn omn_cpu_util_means_every_raw_event() {
        // Three events, the first a full second after the window opened.
        let v = omn_cpu_util(&capture("omn-cpu-util-raw.json"), "").unwrap();
        let want = (0.4078838174273859 + 0.11637572734829593 + 0.23035343035343037) / 3.0;
        assert!(close(get(&v, "busy"), want));
        assert!(close(get(&v, "all"), want));
    }

    #[test]
    fn omn_mem_swap_keeps_a_first_event_a_period_in() {
        let v = omn_mem_swap(&capture("omn-mem-swap-raw.json"), "").unwrap();
        assert!(close(get(&v, "si_bytes"), 0.0));
        assert!(close(get(&v, "so_bytes"), 0.0));
    }

    #[test]
    fn omn_psi_maps_the_pressure_names() {
        let cpu = omn_psi(&capture("omn-cpu-pressure-raw.json"), "").unwrap();
        let want = (0.012417910601538748 + 0.010016625949137182 + 0.013626182273969722) / 3.0;
        assert!(close(get(&cpu, "some"), want));
        let io = omn_psi(&capture("omn-io-pressure-raw.json"), "").unwrap();
        assert!(close(
            get(&io, "some"),
            (0.2353519582147253 + 0.08836666667427447) / 2.0
        ));
        assert!(close(get(&io, "full"), 0.18252916028034885 / 2.0));
    }

    #[test]
    fn omn_parsers_refuse_what_is_not_omns_json() {
        assert!(omn_cpu_util("vmstat\n", "").is_err());
        assert!(omn_psi("{\"results\": []}", "").is_err());
    }
}

/// Holds the fix of defect 2 of `docs/fixes/w22-sysstat.md` (a poll's leading
/// raw event covered a few ms, in one run out of six to four out of five):
/// since the twenty-fourth wave a poll's first read is its baseline, so no
/// event is stamped within half a period of the window's opening. This runs
/// `omn` alone, no tool, ten short runs of each PSI and `/proc/stat` poll.
#[cfg(test)]
mod defect_hold {
    use serde_json::Value;
    use std::process::{Command, Stdio};

    /// Whether one short run of `source` emits a leading raw event stamped
    /// within half a period of the window's opening. `None` when the run gave
    /// fewer than two events.
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
        let (opened, stamps) = super::window_and_stamps(&json)?;
        Ok(match stamps[..] {
            [t0, t1, ..] => Some(t0 - opened < (t1 - t0) / 2.0),
            _ => None,
        })
    }

    #[test]
    fn poll_has_no_leading_event() {
        let mut failed = Vec::new();
        for source in ["cpu.util", "cpu.pressure", "mem.pressure", "io.pressure"] {
            for _ in 0..10 {
                match leading_event(source) {
                    Ok(Some(true)) => {
                        failed.push(format!(
                            "{source}: a leading event within half a period (defect 2 is back)"
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
