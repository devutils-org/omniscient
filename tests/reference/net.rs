//! Parsers for the `net` family: the tools' outputs, and omn's where a
//! value is not at one place in its JSON (`compare.omn_parse`).
//!
//! The tools here print totals since boot (`ip -s link`, `nstat -a`), the
//! state of the moment (`ss -ti`) or a tally on interrupt (`tcpdump`), so
//! each pair's reference command goes through a wrapper in
//! `tests/reference/workloads/net/` that turns the tool into a measure of
//! one window, and the tool's parsers read what the wrapper prints. Every
//! pair runs `alongside` (`catalog/references/net.yaml`, its header).

use crate::common::{Parser, Values};
use serde_json::Value as Json;

pub fn parsers() -> Vec<Parser> {
    vec![
        Parser {
            name: "libbpf_tcplife",
            parse: libbpf_tcplife,
        },
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
        Parser {
            name: "omn_ip_link",
            parse: omn_ip_link,
        },
        Parser {
            name: "omn_total",
            parse: omn_total,
        },
        Parser {
            name: "omn_rtt",
            parse: omn_rtt,
        },
        Parser {
            name: "omn_captured",
            parse: omn_captured,
        },
        Parser {
            name: "ss_s_transport",
            parse: ss_s_transport,
        },
        Parser {
            name: "ss_tuan_count",
            parse: ss_tuan_count,
        },
        Parser {
            name: "omn_sockstat",
            parse: omn_sockstat,
        },
    ]
}

/// `ss -tuan`: one line per TCP or UDP socket under its header, as
/// `sockets`, and per netid (`tcp`, `udp`).
fn ss_tuan_count(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let mut lines = stdout.lines();
    let header = lines.next().unwrap_or_default();
    if !header.starts_with("Netid") {
        return Err("no `Netid` header: not the output of `ss -tuan`".into());
    }
    let (mut tcp, mut udp) = (0.0, 0.0);
    for line in lines {
        match line.split_whitespace().next() {
            Some("tcp") => tcp += 1.0,
            Some("udp") => udp += 1.0,
            _ => {}
        }
    }
    Ok(vec![
        ("sockets".into(), tcp + udp),
        ("tcp".into(), tcp),
        ("udp".into(), udp),
    ])
}

/// `ss -s`: `Total: N`, and its transport table's Total column, one key per
/// row (`TCP`, `UDP`, `RAW`, `INET`, `FRAG`).
fn ss_s_transport(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let mut out = Values::new();
    for line in stdout.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            ["Total:", total, ..] => out.push((
                "Total".into(),
                total.parse().map_err(|_| format!("`{line}`"))?,
            )),
            [name @ ("TCP" | "UDP" | "RAW" | "INET" | "FRAG"), total, _ip, _ipv6] => {
                out.push((
                    (*name).into(),
                    total.parse().map_err(|_| format!("`{line}`"))?,
                ));
            }
            _ => {}
        }
    }
    if out.iter().all(|(key, _)| key != "TCP") {
        return Err("no transport table: not the output of `ss -s`".into());
    }
    Ok(out)
}

/// `net.sockstat`'s raw events, one per transport: `inuse` by `counter`, and
/// `sockets` as `Total`, the keys `ss_s_transport` gives.
fn omn_sockstat(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let result = omn_result(stdout)?;
    let events = result["outputs"]
        .as_array()
        .and_then(|o| o.iter().find(|o| o["emit"] == "raw"))
        .and_then(|o| o["events"].as_array())
        .ok_or("omn's result has no `raw` output")?;
    let mut out = Values::new();
    for event in events {
        let (Some(counter), Some(inuse)) = (
            attr(event, "counter"),
            event["attrs"]
                .as_array()
                .and_then(|attrs| attrs.iter().find(|a| a["name"] == "inuse"))
                .and_then(|a| a["value"].as_f64()),
        ) else {
            continue;
        };
        let key = if counter == "sockets" {
            "Total"
        } else {
            counter
        };
        out.push((key.into(), inuse));
    }
    Ok(out)
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
    // What omn's `net.packets` and `net.iface` count: both directions
    // summed, and the transmit-only columns under omn's names.
    let get = |k: &str| out.iter().find(|(n, _)| n == k).map(|(_, v)| *v);
    let mut more = Values::new();
    for (name, rx, tx) in [
        ("packets", "rx_packets", "tx_packets"),
        ("errors", "rx_errors", "tx_errors"),
        ("drops", "rx_dropped", "tx_dropped"),
    ] {
        if let (Some(rx), Some(tx)) = (get(rx), get(tx)) {
            more.push((name.into(), rx + tx));
        }
    }
    for (name, tx) in [("carrier", "tx_carrier"), ("collisions", "tx_collsns")] {
        if let Some(tx) = get(tx) {
            more.push((name.into(), tx));
        }
    }
    out.extend(more);
    Ok(out)
}

/// `around.py <s> ip -s link show dev <if>`: the increase of every column
/// over the window.
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

/// `around.py <s> nstat -saz <names>`: each counter's increase.
fn nstat_delta(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let (b, a) = before_after(stdout)?;
    delta(&nstat(b)?, &nstat(a)?)
}

/// `sample.py <interval> <s> ss -tin ...`: every `rtt:<srtt>/<rttvar>`
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

/// omn's JSON, its first result.
fn omn_result(stdout: &str) -> Result<Json, String> {
    let doc: Json = serde_json::from_str(stdout).map_err(|e| format!("omn's JSON: {e}"))?;
    doc["results"]
        .get(0)
        .cloned()
        .ok_or_else(|| "omn's JSON has no result".into())
}

/// The series of omn's `summary` output.
fn summary_series(result: &Json) -> Result<Vec<Json>, String> {
    result["outputs"]
        .as_array()
        .and_then(|o| o.iter().find(|o| o["emit"] == "summary"))
        .and_then(|o| o["series"].as_array())
        .cloned()
        .ok_or_else(|| "omn's result has no `summary` output".into())
}

fn field(series: &Json, name: &str) -> Result<f64, String> {
    series[name]
        .as_f64()
        .ok_or_else(|| format!("a series with no numeric `{name}`: {series}"))
}

/// A counter series' whole increase: its summary keeps one value per poll,
/// so the increase is the mean times the count.
fn total(series: &Json) -> Result<f64, String> {
    Ok(field(series, "mean")? * field(series, "count")?)
}

fn add(out: &mut Values, key: String, v: f64) {
    match out.iter_mut().find(|(k, _)| *k == key) {
        Some((_, sum)) => *sum += v,
        None => out.push((key, v)),
    }
}

/// libbpf-tools' `tcplife -p PID`: `PID COMM LADDR LPORT RADDR RPORT TX_KB
/// RX_KB MS`, one row per TCP session the process closed, counted under
/// `sessions` and its volumes summed under `tx_kb` and `rx_kb`.
fn libbpf_tcplife(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let mut out = Values::new();
    let mut sessions = 0.0;
    for line in stdout.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.len() < 9 || words[0].parse::<u64>().is_err() {
            continue;
        }
        let n = words.len();
        let kb = |at: usize| {
            words[at]
                .parse::<f64>()
                .map_err(|_| format!("`{}` is no size", words[at]))
        };
        add(&mut out, "tx_kb".into(), kb(n - 3)?);
        add(&mut out, "rx_kb".into(), kb(n - 2)?);
        sessions += 1.0;
    }
    add(&mut out, "sessions".into(), sessions);
    Ok(out)
}

/// omn's `net.bytes`, `net.packets` or `net.iface` summary in `ip -s
/// link`'s keys: a series keyed `dir:rx` is `rx_<value_attr>`
/// (`rx_bytes`), one with no direction is `<value_attr>` (`packets`, both
/// directions), and `net.iface`'s `<iface>/<kind>` is `<kind>` (`errors`,
/// `drops`, `carrier`, `collisions`). Series of several interfaces add up.
fn omn_ip_link(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let result = omn_result(stdout)?;
    let attr = result["value_attr"]
        .as_str()
        .ok_or("omn's result has no `value_attr`")?
        .to_string();
    let mut out = Values::new();
    for s in summary_series(&result)? {
        let key = s["key"].as_str().ok_or("a series with no key")?;
        let name = if let Some((_, kind)) = key.split_once('/') {
            kind.to_string()
        } else if let Some(dir) = key.split_whitespace().find_map(|k| k.strip_prefix("dir:")) {
            format!("{dir}_{attr}")
        } else {
            attr.clone()
        };
        add(&mut out, name, total(&s)?);
    }
    if out.is_empty() {
        return Err("omn's summary has no series".into());
    }
    Ok(out)
}

/// The whole increase over every series of omn's summary, under every key a
/// tool may name it by (`TcpRetransSegs`) and as `""`: a counter split per
/// socket or task by omn and kept whole by the tool.
fn omn_total(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let series = summary_series(&omn_result(stdout)?)?;
    let sum = series.iter().map(total).sum::<Result<f64, String>>()?;
    Ok(vec![("".into(), sum), ("TcpRetransSegs".into(), sum)])
}

/// `net.tcp.rtt`'s mean over every poll of every series, weighted by each
/// series' count, as `rtt_ns_mean` (the unit is ns).
fn omn_rtt(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let series = summary_series(&omn_result(stdout)?)?;
    let n = series
        .iter()
        .map(|s| field(s, "count"))
        .sum::<Result<f64, String>>()?;
    if n == 0.0 {
        return Err("omn read no rtt: no series, or none with a poll".into());
    }
    let sum = series.iter().map(total).sum::<Result<f64, String>>()?;
    Ok(vec![("rtt_ns_mean".into(), sum / n)])
}

/// An event's string attribute, if it has one by that name.
fn attr<'a>(event: &'a Json, name: &str) -> Option<&'a str> {
    event["attrs"]
        .as_array()?
        .iter()
        .find(|a| a["name"] == name)?["value"]
        .as_str()
}

/// The events of omn's `raw` output that `tcpdump` would have kept, as
/// `captured` (what `tcpdump` names them) and `""`. On loopback every frame
/// reaches a packet socket twice, once `outgoing` and once `host`
/// (`docs/open-findings.md`, "Loopback carries every frame twice"); omn's
/// `net.capture` keeps both, and libpcap drops the `outgoing` copy on a
/// loopback device (`pcap-linux.c`), so a frame on `lo` counts once here,
/// as tcpdump counts it. An event with no `direction` (another source's
/// raw output) counts as itself.
fn omn_captured(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let result = omn_result(stdout)?;
    let events = result["outputs"]
        .as_array()
        .and_then(|o| o.iter().find(|o| o["emit"] == "raw"))
        .and_then(|o| o["events"].as_array())
        .ok_or("omn's result has no `raw` output")?;
    let n = events
        .iter()
        .filter(|e| !(attr(e, "iface") == Some("lo") && attr(e, "direction") == Some("outgoing")))
        .count() as f64;
    Ok(vec![("".into(), n), ("captured".into(), n)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ss_tuan_counts_sockets_by_netid() {
        let ss = "Netid State  Recv-Q Send-Q Local Address:Port Peer Address:Port\ntcp   LISTEN 0 128 127.0.0.1:631 0.0.0.0:*\nudp   UNCONN 0 0 0.0.0.0:5353 0.0.0.0:*\ntcp   ESTAB 0 0 [::1]:22 [::1]:4242\n";
        let v = ss_tuan_count(ss, "").unwrap();
        assert_eq!(get(&v, "sockets"), 3.0);
        assert_eq!(get(&v, "udp"), 1.0);
        assert!(ss_tuan_count("nothing", "").is_err());
    }

    /// `ss -s` on this machine, 2026-10-18, and `net.sockstat`'s snapshot of
    /// the same files.
    #[test]
    fn ss_s_and_omn_sockstat_name_the_same_rows() {
        let ss = "Total: 284\nTCP:   38 (estab 8, closed 27, orphaned 1, timewait 26)\n\nTransport Total     IP        IPv6\nRAW\t  1         0         1        \nUDP\t  4         2         2        \nTCP\t  11        5         6        \nINET\t  16        7         9        \nFRAG\t  0         0         0        \n";
        let v = ss_s_transport(ss, "").unwrap();
        assert_eq!(get(&v, "TCP"), 11.0);
        assert_eq!(get(&v, "Total"), 284.0);
        assert_eq!(get(&v, "RAW"), 1.0);
        let omn = r#"{"results":[{"outputs":[{"emit":"raw","events":[
            {"attrs":[{"name":"inuse","value":285},{"name":"counter","value":"sockets"}]},
            {"attrs":[{"name":"inuse","value":12},{"name":"counter","value":"TCP"}]}]}]}]}"#;
        let o = omn_sockstat(omn, "").unwrap();
        assert_eq!(get(&o, "TCP"), 12.0);
        assert_eq!(get(&o, "Total"), 285.0);
        assert!(ss_s_transport("nothing", "").is_err());
    }

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
        for k in ["errors", "drops", "carrier", "collisions"] {
            assert_eq!(get(&v, k), 0.0);
        }
        // 16 MiB pushed, plus the headers: at least the payload each way.
        assert!(get(&v, "rx_bytes") >= 16.0 * 1048576.0);
    }

    #[test]
    fn ip_s_link_sums_both_directions_under_omns_names() {
        let one = |rx: u64, tx: u64| {
            format!(
                "    RX:  bytes packets errors dropped  missed   mcast\n    0 0 {rx} {rx} 0 0\n    TX:  bytes packets errors dropped carrier collsns\n    0 0 {tx} {tx} {tx} {tx}\n"
            )
        };
        let out = format!("{}{MARKER}\n{}", one(1, 2), one(4, 7));
        let v = ip_s_link_delta(&out, "").unwrap();
        assert_eq!(get(&v, "errors"), 3.0 + 5.0);
        assert_eq!(get(&v, "drops"), 3.0 + 5.0);
        assert_eq!(get(&v, "carrier"), 5.0);
        assert_eq!(get(&v, "collisions"), 5.0);
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
        assert_eq!(get(&v, "captured"), 1804.0);
        assert_eq!(get(&v, "received"), 3608.0);
        assert_eq!(get(&v, "dropped"), 0.0);
        assert!(tcpdump_count("", "tcpdump: lo: You don't have permission").is_err());
    }

    #[test]
    fn omn_ip_link_reads_a_counters_increase_per_direction() {
        // omn net.bytes --iface lo --keys iface,dir, over a 16 MiB push.
        let v = omn_ip_link(include_str!("captures/net/omn_net_bytes.json"), "").unwrap();
        assert!((get(&v, "rx_bytes") - 16799488.0).abs() < 1.0);
        assert!((get(&v, "tx_bytes") - 16799488.0).abs() < 1.0);
        // 16 MiB pushed, plus the headers.
        assert!(get(&v, "rx_bytes") >= 16.0 * 1048576.0);
        let v = omn_ip_link(include_str!("captures/net/omn_net_packets.json"), "").unwrap();
        assert_eq!(get(&v, "packets"), 16348.0);
        let v = omn_ip_link(include_str!("captures/net/omn_net_iface.json"), "").unwrap();
        for k in ["errors", "drops", "fifo", "carrier", "collisions"] {
            assert_eq!(get(&v, k), 0.0, "{k}");
        }
        assert!(omn_ip_link("{}", "").is_err());
        assert!(omn_ip_link("not json", "").is_err());
    }

    #[test]
    fn omn_total_adds_every_sockets_series() {
        let v = omn_total(include_str!("captures/net/omn_net_tcp_retransmit.json"), "").unwrap();
        // 62 sockets, each a SYN retransmitted twice.
        assert_eq!(get(&v, "TcpRetransSegs"), 124.0);
        assert_eq!(get(&v, ""), 124.0);
    }

    #[test]
    fn omn_rtt_weights_each_series_by_its_polls() {
        let v = omn_rtt(include_str!("captures/net/omn_net_tcp_rtt.json"), "").unwrap();
        assert!((get(&v, "rtt_ns_mean") - 1_116_083.333).abs() < 1.0);
        let none = r#"{"results":[{"outputs":[{"emit":"summary","series":[]}]}]}"#;
        assert!(omn_rtt(none, "").is_err());
    }

    #[test]
    fn omn_captured_counts_raw_events_with_no_direction_as_themselves() {
        let v = omn_captured(include_str!("captures/net/omn_net_backlog.json"), "").unwrap();
        assert_eq!(get(&v, "captured"), 5.0);
        let summary = include_str!("captures/net/omn_net_tcp_rtt.json");
        assert!(omn_captured(summary, "").is_err());
    }

    /// The pair's own source: `net.capture`'s raw output, captured with
    /// CAP_NET_RAW (`make CAPTURE=1`) over a 2 ms pingpong on `lo`. Every
    /// packet the filter kept is one event, nothing capped or split (the
    /// result's `events_seen` and its filter's `kept` are the 14 events),
    /// and the 14 are 7 frames seen twice, `outgoing` and `host`: tcpdump
    /// counts 7.
    #[test]
    fn omn_captured_counts_each_loopback_frame_once_as_tcpdump_does() {
        let stdout = include_str!("captures/net/omn_net_capture.json");
        let result = omn_result(stdout).unwrap();
        assert_eq!(result["events_seen"].as_f64(), Some(14.0));
        assert_eq!(result["filtered"]["kept"].as_f64(), Some(14.0));
        let v = omn_captured(stdout, "").unwrap();
        assert_eq!(get(&v, "captured"), 7.0);
        assert_eq!(get(&v, ""), 7.0);
    }
}
