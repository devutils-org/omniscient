//! Parsers for the `net` family's reference tools. Each turns a tool's
//! output into the values a pair compares (BRIEF.md, "Decided before this
//! wave").
//!
//! The tools here print totals since boot (`ip -s link`, `nstat -a`), the
//! state of the moment (`ss -ti`) or a tally on interrupt (`tcpdump`), so
//! each pair's reference command goes through a wrapper in
//! `tests/reference/workloads/net/` that turns the tool into a measure of
//! one run, and the parsers read what the wrapper prints.
//!
//! Every net source compared is system-wide or per interface, which omn
//! refuses beside a program to launch. The pairs say so with `alongside`,
//! and until the shared runner knows that field, `net_pairs_against_the_tools`
//! below runs them: omn for its `--duration`, the reference command started
//! `lead_ms` into it.

use crate::common::{Parser, Values};

pub fn parsers() -> Vec<Parser> {
    vec![
        Parser {
            name: "ip_s_link_delta",
            parse: ip_s_link_delta,
        },
        Parser {
            name: "nstat_delta",
            parse: nstat_delta,
        },
        Parser {
            name: "ss_ti_rtt",
            parse: ss_ti_rtt,
        },
        Parser {
            name: "tcpdump_count",
            parse: tcpdump_count,
        },
    ]
}

/// The line `around.py` prints between the reading before the workload and
/// the one after.
const MARKER: &str = "==== omn-reference: the workload ran here ====";

fn before_after(stdout: &str) -> Result<(&str, &str), String> {
    stdout
        .split_once(MARKER)
        .ok_or_else(|| format!("no `{MARKER}` line: not the output of around.py"))
}

fn delta(before: &Values, after: &Values) -> Result<Values, String> {
    after
        .iter()
        .map(|(k, a)| {
            let b = before
                .iter()
                .find(|(bk, _)| bk == k)
                .map(|(_, v)| *v)
                .ok_or_else(|| format!("`{k}` read after the workload but not before"))?;
            Ok((k.clone(), a - b))
        })
        .collect()
}

/// One `ip -s link show dev <if>` reading: the `RX:` and `TX:` header lines
/// name the columns of the line under them, so a column iproute2 renames
/// (`missed` was `overrun`) keeps its own name, prefixed `rx_` or `tx_`.
/// `packets` is rx + tx, what `net.packets` counts.
fn ip_s_link(text: &str) -> Result<Values, String> {
    let mut out = Values::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let t = line.trim();
        let dir = if t.starts_with("RX:") {
            "rx"
        } else if t.starts_with("TX:") {
            "tx"
        } else {
            continue;
        };
        let names: Vec<&str> = t[3..].split_whitespace().collect();
        let values = lines
            .next()
            .ok_or_else(|| format!("`{t}` with no line of values under it"))?;
        let values: Vec<f64> = values
            .split_whitespace()
            .map(|v| v.parse::<f64>().map_err(|e| format!("`{v}`: {e}")))
            .collect::<Result<_, _>>()?;
        if values.len() < names.len() {
            return Err(format!(
                "{} names over {} values",
                names.len(),
                values.len()
            ));
        }
        for (n, v) in names.iter().zip(values) {
            out.push((format!("{dir}_{n}"), v));
        }
    }
    if out.is_empty() {
        return Err("no RX: or TX: block: not `ip -s link` output".into());
    }
    let get = |k: &str| out.iter().find(|(n, _)| n == k).map(|(_, v)| *v);
    if let (Some(rx), Some(tx)) = (get("rx_packets"), get("tx_packets")) {
        out.push(("packets".into(), rx + tx));
    }
    Ok(out)
}

/// `around.py ip -s link show dev <if> -- <workload>`: the increase of every
/// column over the run.
fn ip_s_link_delta(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let (b, a) = before_after(stdout)?;
    delta(&ip_s_link(b)?, &ip_s_link(a)?)
}

/// One `nstat -saz [names]` reading: `#kernel`, then `Name value rate`.
fn nstat(text: &str) -> Result<Values, String> {
    let mut out = Values::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut f = line.split_whitespace();
        let (Some(name), Some(value)) = (f.next(), f.next()) else {
            return Err(format!("`{line}`: not `name value rate`"));
        };
        let v = value.parse::<f64>().map_err(|e| format!("`{line}`: {e}"))?;
        out.push((name.to_string(), v));
    }
    if out.is_empty() {
        return Err("no counter: not `nstat` output".into());
    }
    Ok(out)
}

/// `around.py nstat -saz <names> -- <workload>`: each counter's increase.
fn nstat_delta(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let (b, a) = before_after(stdout)?;
    delta(&nstat(b)?, &nstat(a)?)
}

/// `sample.py <s> ss -tin ... -- <workload>`: every `rtt:<srtt>/<rttvar>`
/// in milliseconds (not `minrtt:`), as `rtt_ns_mean`, `rtt_ns_min`,
/// `rtt_ns_max` and `readings`.
fn ss_ti_rtt(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let mut rtts = Vec::new();
    for word in stdout.split_whitespace() {
        let Some(v) = word.strip_prefix("rtt:") else {
            continue;
        };
        let srtt = v.split('/').next().unwrap_or(v);
        let ms = srtt.parse::<f64>().map_err(|e| format!("`{word}`: {e}"))?;
        rtts.push(ms * 1e6);
    }
    if rtts.is_empty() {
        return Err("no `rtt:` field: the connection was not found by ss".into());
    }
    let n = rtts.len() as f64;
    Ok(vec![
        ("rtt_ns_mean".into(), rtts.iter().sum::<f64>() / n),
        (
            "rtt_ns_min".into(),
            rtts.iter().copied().fold(f64::MAX, f64::min),
        ),
        (
            "rtt_ns_max".into(),
            rtts.iter().copied().fold(f64::MIN, f64::max),
        ),
        ("readings".into(), n),
    ])
}

/// tcpdump's tally on SIGINT, on stderr: `N packets captured`, `N packets
/// received by filter`, `N packets dropped by kernel`, keyed `captured`,
/// `received`, `dropped`.
fn tcpdump_count(_stdout: &str, stderr: &str) -> Result<Values, String> {
    let mut out = Values::new();
    for line in stderr.lines() {
        let mut f = line.split_whitespace();
        let (Some(n), Some("packets"), Some(what)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        let Ok(n) = n.parse::<f64>() else { continue };
        let key = match what {
            "captured" => "captured",
            "received" => "received",
            "dropped" => "dropped",
            _ => continue,
        };
        out.push((key.into(), n));
    }
    if !out.iter().any(|(k, _)| k == "captured") {
        return Err("no `N packets captured` line: tcpdump was not interrupted cleanly".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(v: &Values, k: &str) -> f64 {
        v.iter()
            .find(|(n, _)| n == k)
            .unwrap_or_else(|| panic!("no `{k}` in {v:?}"))
            .1
    }

    #[test]
    fn ip_s_link_delta_reads_both_blocks_and_subtracts() {
        let v = ip_s_link_delta(include_str!("captures/net/ip_s_link_lo.txt"), "").unwrap();
        assert_eq!(get(&v, "rx_bytes"), 424604632529.0 - 424587831637.0);
        assert_eq!(get(&v, "tx_packets"), 35250790.0 - 35250335.0);
        assert_eq!(get(&v, "packets"), 2.0 * (35250790.0 - 35250335.0));
        assert_eq!(get(&v, "rx_errors"), 0.0);
        assert_eq!(get(&v, "tx_collsns"), 0.0);
        // 16 MiB pushed, plus the headers: at least the payload each way.
        assert!(get(&v, "rx_bytes") >= 16.0 * 1048576.0);
    }

    #[test]
    fn ip_s_link_refuses_what_is_not_its_output() {
        assert!(ip_s_link_delta("no marker", "").is_err());
        assert!(ip_s_link_delta(&format!("x\n{MARKER}\ny"), "").is_err());
    }

    #[test]
    fn nstat_delta_reads_the_named_counters() {
        let v = nstat_delta(include_str!("captures/net/nstat_saz_backlog.txt"), "").unwrap();
        assert_eq!(get(&v, "TcpExtListenOverflows"), 62.0);
        assert_eq!(get(&v, "TcpExtListenDrops"), 62.0);
        assert_eq!(get(&v, "TcpRetransSegs"), 0.0);
    }

    #[test]
    fn ss_ti_rtt_takes_srtt_and_not_minrtt() {
        let v = ss_ti_rtt(include_str!("captures/net/ss_tin_pingpong.txt"), "").unwrap();
        assert_eq!(get(&v, "readings"), 3.0);
        assert!((get(&v, "rtt_ns_min") - 1_072_000.0).abs() < 1.0);
        assert!((get(&v, "rtt_ns_max") - 1_097_000.0).abs() < 1.0);
        assert!(ss_ti_rtt("ESTAB 0 0 minrtt:0.018", "").is_err());
    }

    #[test]
    fn tcpdump_count_reads_the_tally_on_stderr() {
        let v = tcpdump_count("", include_str!("captures/net/tcpdump_count.stderr.txt")).unwrap();
        assert_eq!(get(&v, "captured"), 3621.0);
        assert_eq!(get(&v, "received"), 7242.0);
        assert_eq!(get(&v, "dropped"), 0.0);
        assert!(tcpdump_count("", "tcpdump: lo: You don't have permission").is_err());
    }

    #[test]
    fn every_pair_names_a_parser_of_this_module() {
        let names: Vec<_> = parsers().iter().map(|p| p.name).collect();
        for pair in runner::pairs() {
            let p = pair["reference"]["parse"].as_str().unwrap();
            assert!(names.contains(&p), "`{p}` is not a net parser");
        }
    }

    /// Runs every pair of `catalog/references/net.yaml` the family's way
    /// (omn alongside the tool, see the module's doc), and holds each to its
    /// tolerance. A pair whose tool, privilege or omn backend is missing on
    /// this machine is skipped with that reason.
    #[test]
    fn net_pairs_against_the_tools() {
        let mut failures = Vec::new();
        for pair in runner::pairs() {
            let name = format!(
                "{} / {} [{}]",
                pair["source"].as_str().unwrap_or("?"),
                pair["tool"].as_str().unwrap_or("?"),
                pair["compare"]["key"].as_str().unwrap_or("")
            );
            match runner::run(&pair) {
                runner::Outcome::Pass(omn, tool) => {
                    eprintln!("net pair PASS  {name}: omn {omn} tool {tool}")
                }
                runner::Outcome::Skip(why) => eprintln!("net pair SKIP  {name}: {why}"),
                runner::Outcome::Fail(why) => {
                    eprintln!("net pair FAIL  {name}: {why}");
                    failures.push(format!("{name}: {why}"));
                }
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    mod runner {
        use serde_json::Value;
        use std::path::Path;
        use std::process::{Command, Stdio};
        use std::time::Duration;

        pub enum Outcome {
            Pass(f64, f64),
            Skip(String),
            Fail(String),
        }

        fn root() -> &'static Path {
            Path::new(env!("CARGO_MANIFEST_DIR"))
        }

        pub fn pairs() -> Vec<Value> {
            let text = std::fs::read_to_string(root().join("catalog/references/net.yaml"))
                .expect("catalog/references/net.yaml");
            serde_norway::from_str(&text).expect("net.yaml parses")
        }

        fn strings(v: &Value) -> Vec<String> {
            v.as_array()
                .map(|a| {
                    a.iter()
                        .map(|s| s.as_str().expect("argv strings").to_string())
                        .collect()
                })
                .unwrap_or_default()
        }

        fn on_path(bin: &str) -> bool {
            std::env::var_os("PATH")
                .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
                .unwrap_or(false)
        }

        fn output<'a>(result: &'a Value, emit: &str) -> Result<&'a Value, String> {
            result["outputs"]
                .as_array()
                .and_then(|o| o.iter().find(|o| o["emit"] == emit))
                .ok_or_else(|| format!("omn's JSON has no `{emit}` output"))
        }

        /// A counter's whole increase, a mean, or a count, read from the
        /// first result of omn's JSON (the paths net.yaml's header lists).
        fn omn_value(json: &Value, path: &str) -> Result<f64, String> {
            let result = &json["results"][0];
            if let Some(rest) = path.strip_prefix("series[") {
                let (key, field) = rest
                    .split_once("].")
                    .ok_or_else(|| format!("`{path}`: not series[<key>].<field>"))?;
                let series: Vec<&Value> = output(result, "summary")?["series"]
                    .as_array()
                    .map(|s| s.iter().filter(|s| key == "*" || s["key"] == key).collect())
                    .unwrap_or_default();
                if series.is_empty() {
                    return Err(format!("no summary series `{key}` in omn's output"));
                }
                let f = |s: &Value, k: &str| s[k].as_f64().unwrap_or(0.0);
                let total: f64 = series.iter().map(|s| f(s, "mean") * f(s, "count")).sum();
                return match field {
                    "sum" => Ok(total),
                    "mean" => Ok(total / series.iter().map(|s| f(s, "count")).sum::<f64>()),
                    _ => Err(format!("`{path}`: unknown field `{field}`")),
                };
            }
            if let Some(rest) = path.strip_prefix("events.") {
                let events = output(result, "raw")?["events"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                if rest == "count" {
                    return Ok(events.len() as f64);
                }
                let attr = rest
                    .strip_suffix(".sum")
                    .ok_or_else(|| format!("`{path}`: not events.<attr>.sum"))?;
                return Ok(events
                    .iter()
                    .flat_map(|e| e["attrs"].as_array().cloned().unwrap_or_default())
                    .filter(|a| a["name"] == attr)
                    .filter_map(|a| a["value"].as_f64())
                    .sum());
            }
            Err(format!("`{path}`: unknown omn path"))
        }

        fn sudo_granted() -> bool {
            Command::new("sudo")
                .args(["-n", "true"])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        }

        pub fn run(pair: &Value) -> Outcome {
            let reference = &pair["reference"];
            for bin in strings(&reference["requires"]) {
                if !on_path(&bin) {
                    return Outcome::Skip(format!("`{bin}` is not installed on this machine"));
                }
            }
            let privileged = reference["privileged"].as_bool().unwrap_or(false);
            if privileged && !sudo_granted() {
                return Outcome::Skip("needs root, and `sudo -n` does not grant it".into());
            }
            let lead = pair["alongside"]["lead_ms"].as_u64().unwrap_or(1500);
            let omn = Command::new(env!("CARGO_BIN_EXE_omn"))
                .args(strings(&pair["omn"]))
                .current_dir(root())
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn omn");
            std::thread::sleep(Duration::from_millis(lead));
            let mut argv = strings(&reference["command"]);
            argv.extend(strings(&pair["workload"]));
            if privileged {
                argv.splice(0..0, ["sudo".to_string(), "-n".to_string()]);
            }
            let tool = Command::new(&argv[0])
                .args(&argv[1..])
                .current_dir(root())
                .stdin(Stdio::null())
                .output()
                .expect("spawn the reference command");
            let omn = omn.wait_with_output().expect("omn exits");
            let omn_err = String::from_utf8_lossy(&omn.stderr);
            if !omn.status.success() {
                if omn_err.contains("no viable backend on this machine") {
                    let why = omn_err.lines().take(2).collect::<Vec<_>>().join(" ");
                    return Outcome::Skip(format!("omn: {why}"));
                }
                return Outcome::Fail(format!("omn failed: {omn_err}"));
            }
            let tool_out = String::from_utf8_lossy(&tool.stdout);
            let tool_err = String::from_utf8_lossy(&tool.stderr);
            if !tool.status.success() {
                return Outcome::Fail(format!("the reference command failed: {tool_err}"));
            }
            let parse_name = reference["parse"].as_str().unwrap_or("");
            let Some(parser) = super::super::parsers()
                .into_iter()
                .find(|p| p.name == parse_name)
            else {
                return Outcome::Fail(format!("no parser `{parse_name}`"));
            };
            let values = match (parser.parse)(&tool_out, &tool_err) {
                Ok(v) => v,
                Err(e) => return Outcome::Fail(format!("{parse_name}: {e}")),
            };
            let compare = &pair["compare"];
            let key = compare["key"].as_str().unwrap_or("");
            let Some(want) = values.iter().find(|(k, _)| k == key).map(|(_, v)| *v) else {
                return Outcome::Fail(format!("{parse_name} gave no `{key}`: {values:?}"));
            };
            let json: Value = match serde_json::from_slice(&omn.stdout) {
                Ok(j) => j,
                Err(e) => return Outcome::Fail(format!("omn's JSON: {e}")),
            };
            let got = match omn_value(&json, compare["omn"].as_str().unwrap_or("")) {
                Ok(v) => v,
                Err(e) => return Outcome::Fail(e),
            };
            let tol = &compare["tolerance"];
            let bound = match tol["rel"].as_f64() {
                Some(rel) => rel * want.abs(),
                None => tol["abs"].as_f64().unwrap_or(0.0),
            };
            if (got - want).abs() <= bound {
                Outcome::Pass(got, want)
            } else {
                Outcome::Fail(format!(
                    "omn {got} against the tool's {want}, outside {tol}"
                ))
            }
        }
    }
}
