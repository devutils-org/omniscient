//! Parsers for the `bpftrace` family's reference tools. Each turns a tool's
//! output into the values a pair compares (BRIEF.md, "Decided before this
//! wave").
//!
//! Two kinds of output come out of `/usr/share/bpftrace/tools/`: the maps
//! bpftrace prints itself at the end or on an interval (`@name[key]: N`, and
//! the `hist()`/`lhist()` ASCII histograms), and the per-event lines a
//! `*snoop` tool `printf`s. The map parsers are generic; the snoop parsers
//! count lines by the column a pair keys on, which is the comm wherever the
//! tool prints one, because every tool here is system-wide and the workloads
//! name themselves (`tests/reference/workloads/bpftrace/act.py`).

use crate::common::{Parser, Values};

pub fn parsers() -> Vec<Parser> {
    vec![
        Parser {
            name: "bpftrace_map",
            parse: bpftrace_map,
        },
        Parser {
            name: "bpftrace_hist",
            parse: bpftrace_hist,
        },
        Parser {
            name: "bpftrace_hist_usecs",
            parse: bpftrace_hist_usecs,
        },
        Parser {
            name: "execsnoop",
            parse: execsnoop,
        },
        Parser {
            name: "killsnoop",
            parse: killsnoop,
        },
        Parser {
            name: "opensnoop",
            parse: opensnoop,
        },
        Parser {
            name: "biosnoop",
            parse: biosnoop,
        },
        Parser {
            name: "tcpconnect",
            parse: tcpconnect,
        },
        Parser {
            name: "tcplife",
            parse: tcplife,
        },
    ]
}

/// Adds `by` to `key`, keeping first-seen order so a failure prints stably.
fn add(values: &mut Values, key: &str, by: f64) {
    match values.iter_mut().find(|(k, _)| k == key) {
        Some((_, v)) => *v += by,
        None => values.push((key.to_string(), by)),
    }
}

/// `@name[key]: N` and `@name: N` lines, summed over every time the map was
/// printed: `vfsstat` and `swapin` print and clear theirs each second, so the
/// sum is the run's total. A composite key (`@[comm, pid]`) is also summed
/// under its first component alone (`@[comm]`), since a pid differs between
/// two runs and a comm does not.
fn bpftrace_map(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let mut values = Values::new();
    for line in stdout.lines() {
        let line = line.trim();
        if !line.starts_with('@') {
            continue;
        }
        let Some((key, n)) = line.rsplit_once(": ") else {
            continue;
        };
        let Ok(n) = n.trim().parse::<f64>() else {
            // A histogram's or a struct's header: not a counter.
            continue;
        };
        add(&mut values, key, n);
        if let (Some(open), true) = (key.find('['), key.ends_with(']')) {
            let inner = &key[open + 1..key.len() - 1];
            if let Some((first, _)) = inner.split_once(", ") {
                add(&mut values, &format!("{}[{first}]", &key[..open]), n);
            }
        }
    }
    if values.is_empty() {
        return Err("no `@map: N` line in the output".into());
    }
    Ok(values)
}

/// One bucket bound as bpftrace prints it: `0`, `4`, `1K`, `16M`, powers of
/// 1024 for the suffixes.
fn bound(text: &str) -> Result<f64, String> {
    let text = text.trim();
    let (digits, scale) = match text.chars().last() {
        Some('K') => (&text[..text.len() - 1], 1024f64),
        Some('M') => (&text[..text.len() - 1], 1024f64.powi(2)),
        Some('G') => (&text[..text.len() - 1], 1024f64.powi(3)),
        Some('T') => (&text[..text.len() - 1], 1024f64.powi(4)),
        Some('P') => (&text[..text.len() - 1], 1024f64.powi(5)),
        _ => (text, 1.0),
    };
    let n: f64 = digits
        .trim()
        .parse()
        .map_err(|_| format!("`{text}` is not a bucket bound"))?;
    Ok(n * scale)
}

/// Writes one histogram's values: `<map>/count` (its events), `<map>/p50`
/// (the lower bound of the bucket holding the median, the resolution a log2
/// histogram has) and `<map>/<low>` per non-empty bucket.
fn flush_hist(values: &mut Values, map: Option<&str>, buckets: &mut Vec<(f64, f64)>) {
    if let Some(map) = map {
        let total: f64 = buckets.iter().map(|(_, c)| c).sum();
        if total > 0.0 {
            add(values, &format!("{map}/count"), total);
            let mut seen = 0.0;
            for (low, c) in buckets.iter() {
                seen += c;
                if seen * 2.0 >= total {
                    add(values, &format!("{map}/p50"), *low);
                    break;
                }
            }
            for (low, c) in buckets.iter().filter(|(_, c)| *c > 0.0) {
                add(values, &format!("{map}/{low}"), *c);
            }
        }
    }
    buckets.clear();
}

/// Every histogram in the output (see [`flush_hist`]), bounds multiplied by
/// `scale` so a tool's `@usecs` lands in the nanoseconds omn reports.
fn hist_scaled(stdout: &str, scale: f64) -> Result<Values, String> {
    let mut values = Values::new();
    let mut map: Option<String> = None;
    let mut buckets: Vec<(f64, f64)> = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.starts_with('@') && line.ends_with(':') {
            flush_hist(&mut values, map.as_deref(), &mut buckets);
            map = Some(line.trim_end_matches(':').to_string());
            continue;
        }
        if !line.starts_with('[') {
            // A blank line or anything else ends the histogram in progress.
            flush_hist(&mut values, map.as_deref(), &mut buckets);
            map = None;
            continue;
        }
        if map.is_none() {
            continue;
        }
        // `[2, 4)    6 |@@@@   |`, `[0]  5 |@@ |`, `[100, ...)  1 |`
        let Some(close) = line.find([']', ')']) else {
            continue;
        };
        let low = bound(line[1..close].split(',').next().unwrap_or(""))?;
        let count = line[close + 1..]
            .split_whitespace()
            .next()
            .ok_or_else(|| format!("no count on `{line}`"))?;
        let count: f64 = count
            .parse()
            .map_err(|_| format!("`{count}` is not a count on `{line}`"))?;
        buckets.push((low * scale, count));
    }
    flush_hist(&mut values, map.as_deref(), &mut buckets);
    if values.is_empty() {
        return Err("no non-empty histogram in the output".into());
    }
    Ok(values)
}

fn bpftrace_hist(stdout: &str, _stderr: &str) -> Result<Values, String> {
    hist_scaled(stdout, 1.0)
}

/// `biolatency` and `runqlat` histogram microseconds; omn reports
/// nanoseconds, so the bounds are scaled here rather than in the pair.
fn bpftrace_hist_usecs(stdout: &str, _stderr: &str) -> Result<Values, String> {
    hist_scaled(stdout, 1000.0)
}

/// The data lines of a snoop tool: those whose `numeric` column parses as an
/// integer, which drops the banner, the header and bpftrace's own messages.
fn rows(stdout: &str, numeric: usize) -> impl Iterator<Item = Vec<&str>> {
    stdout.lines().filter_map(move |line| {
        let words: Vec<&str> = line.split_whitespace().collect();
        words.get(numeric)?.parse::<i64>().ok()?;
        Some(words)
    })
}

/// A comm is the words between two fixed column runs: it may hold a space
/// (`Web Content`).
fn comm(words: &[&str], before: usize, after: usize) -> Option<String> {
    (words.len() > before + after).then(|| words[before..words.len() - after].join(" "))
}

/// `TIME PID PPID ARGS`: one line per exec, `""` the total and
/// `argv0:<path>` per program as it was executed.
fn execsnoop(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let mut values = vec![(String::new(), 0.0)];
    for words in rows(stdout, 1) {
        if words.len() < 4 || words[2].parse::<i64>().is_err() {
            continue;
        }
        values[0].1 += 1.0;
        add(&mut values, &format!("argv0:{}", words[3]), 1.0);
    }
    Ok(values)
}

/// `TIME PID COMM SIG TPID RESULT`: one line per kill/tkill/tgkill sent,
/// counted as `<SIG>` and `<comm>/<SIG>`, the sender's comm.
fn killsnoop(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let mut values = vec![(String::new(), 0.0)];
    for words in rows(stdout, 1) {
        let Some(comm) = comm(&words, 2, 3) else {
            continue;
        };
        let sig = words[words.len() - 3];
        values[0].1 += 1.0;
        add(&mut values, sig, 1.0);
        add(&mut values, &format!("{comm}/{sig}"), 1.0);
    }
    Ok(values)
}

/// `PID COMM FD ERR PATH`: one line per open/openat/openat2 returned,
/// counted as `comm:<comm>`; a failed open (`FD` -1) counts too, as omn's
/// proc.syscall counts every call.
fn opensnoop(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let mut values = vec![(String::new(), 0.0)];
    for words in rows(stdout, 0) {
        if words.len() < 4 || words[2].parse::<i64>().is_err() {
            continue;
        }
        values[0].1 += 1.0;
        add(&mut values, &format!("comm:{}", words[1]), 1.0);
    }
    Ok(values)
}

/// `TIME(ms) MAJOR MINOR COMM PID LAT(ms)`: one line per completed request,
/// counted as `comm:<comm>` (the issuer's) and summed as `lat_ms:<comm>`.
fn biosnoop(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let mut values = vec![(String::new(), 0.0)];
    for words in rows(stdout, 0) {
        if words.len() < 6 || words[1].parse::<i64>().is_err() {
            continue;
        }
        let Some(comm) = comm(&words, 3, 2) else {
            continue;
        };
        let lat: f64 = words[words.len() - 1].parse().unwrap_or(0.0);
        values[0].1 += 1.0;
        add(&mut values, &format!("comm:{comm}"), 1.0);
        add(&mut values, &format!("lat_ms:{comm}"), lat);
    }
    Ok(values)
}

/// `TIME PID COMM SADDR SPORT DADDR DPORT`: one line per active open,
/// counted as `comm:<comm>`.
fn tcpconnect(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let mut values = vec![(String::new(), 0.0)];
    for words in rows(stdout, 1) {
        let Some(comm) = comm(&words, 2, 4) else {
            continue;
        };
        values[0].1 += 1.0;
        add(&mut values, &format!("comm:{comm}"), 1.0);
    }
    Ok(values)
}

/// `PID COMM LADDR LPORT RADDR RPORT TX_KB RX_KB MS`: one line per socket
/// closed, both ends of a local connection included, counted as
/// `comm:<comm>` and summed as `tx_kb:<comm>`, `rx_kb:<comm>`, `ms:<comm>`.
fn tcplife(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let mut values = vec![(String::new(), 0.0)];
    for words in rows(stdout, 0) {
        if words.len() < 9 || words[words.len() - 1].parse::<f64>().is_err() {
            continue;
        }
        let Some(comm) = comm(&words, 1, 7) else {
            continue;
        };
        let n = |i: usize| words[words.len() - i].parse::<f64>().unwrap_or(0.0);
        values[0].1 += 1.0;
        add(&mut values, &format!("comm:{comm}"), 1.0);
        add(&mut values, &format!("tx_kb:{comm}"), n(3));
        add(&mut values, &format!("rx_kb:{comm}"), n(2));
        add(&mut values, &format!("ms:{comm}"), n(1));
    }
    Ok(values)
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

    /// A format fixture, not a capture: each file under
    /// `captures/bpftrace/` is written by hand from the tool's `printf`
    /// formats and bpftrace's map and histogram layout, with synthetic
    /// numbers that agree with no tool and no omn run. These tests check the
    /// parsers, never a pair; the pairs need the privileged run
    /// (docs/fixes/w22-bpftrace.md), whose real output goes beside these as
    /// `<tool>.txt`.
    fn fixture(tool: &str) -> String {
        let path = format!(
            "{}/tests/reference/captures/bpftrace/{tool}.format.txt",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    #[test]
    fn bpftrace_parsers_have_unique_names() {
        let names: Vec<_> = parsers().iter().map(|p| p.name).collect();
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(names.len(), sorted.len(), "{names:?}");
    }

    #[test]
    fn syscount_counts_per_process_and_per_syscall() {
        let v = bpftrace_map(&fixture("syscount"), "").unwrap();
        assert_eq!(get(&v, "@process[omnref]"), 7313.0);
        assert_eq!(get(&v, "@syscall[110]"), 7001.0);
    }

    #[test]
    fn vfsstat_sums_its_per_second_maps() {
        let v = bpftrace_map(&fixture("vfsstat"), "").unwrap();
        assert_eq!(get(&v, "@[vfs_read]"), 2958.0 + 3112.0);
        assert_eq!(get(&v, "@[vfs_open]"), 120.0 + 98.0);
    }

    #[test]
    fn swapin_keys_by_comm_whatever_the_pid() {
        let v = bpftrace_map(&fixture("swapin"), "").unwrap();
        assert_eq!(get(&v, "@[omnref, 4242]"), 12.0);
        assert_eq!(get(&v, "@[omnref]"), 12.0 + 30.0);
    }

    #[test]
    fn a_map_parser_refuses_an_output_with_no_map() {
        assert!(bpftrace_map("Attaching 2 probes...\n", "").is_err());
    }

    #[test]
    fn bitesize_reads_one_histogram_per_comm() {
        let v = bpftrace_hist(&fixture("bitesize"), "").unwrap();
        assert_eq!(get(&v, "@[omnref]/count"), 308.0);
        assert_eq!(get(&v, "@[omnref]/4096"), 301.0);
        assert_eq!(get(&v, "@[omnref]/131072"), 2.0);
        assert_eq!(get(&v, "@[omnref]/p50"), 4096.0);
        assert_eq!(get(&v, "@[kworker/u33:2]/count"), 3.0);
    }

    #[test]
    fn runqlat_reads_usecs_as_nanoseconds() {
        let v = bpftrace_hist_usecs(&fixture("runqlat"), "").unwrap();
        assert_eq!(get(&v, "@usecs/count"), 10747.0);
        // The median falls in [1K, 2K) usecs: 1024 * 1000 ns.
        assert_eq!(get(&v, "@usecs/p50"), 1_024_000.0);
        assert_eq!(get(&v, "@usecs/0"), 43.0);
    }

    #[test]
    fn biolatency_reads_its_single_histogram() {
        let v = bpftrace_hist_usecs(&fixture("biolatency"), "").unwrap();
        assert_eq!(get(&v, "@usecs/count"), 333.0);
        assert_eq!(get(&v, "@usecs/p50"), 8_000.0);
    }

    #[test]
    fn runqlen_reads_a_linear_histogram() {
        let v = bpftrace_hist(&fixture("runqlen"), "").unwrap();
        assert_eq!(get(&v, "@runqlen/count"), 1980.0 + 205.0 + 12.0);
        assert_eq!(get(&v, "@runqlen/p50"), 0.0);
        assert_eq!(get(&v, "@runqlen/2"), 12.0);
    }

    #[test]
    fn a_hist_parser_refuses_an_output_with_no_histogram() {
        assert!(bpftrace_hist("Attaching 3 probes...\n\n@usecs:\n", "").is_err());
    }

    #[test]
    fn execsnoop_counts_what_was_executed() {
        let v = execsnoop(&fixture("execsnoop"), "").unwrap();
        assert_eq!(get(&v, ""), 8.0);
        assert_eq!(get(&v, "argv0:/bin/true"), 6.0);
        assert_eq!(get(&v, "argv0:sh"), 1.0);
    }

    #[test]
    fn killsnoop_counts_signals_by_sender() {
        let v = killsnoop(&fixture("killsnoop"), "").unwrap();
        assert_eq!(get(&v, "omnref/SIGUSR2"), 12.0);
        assert_eq!(get(&v, "SIGUSR2"), 12.0);
        assert_eq!(get(&v, "Web Content/SIGTERM"), 1.0);
        assert_eq!(get(&v, ""), 13.0);
    }

    #[test]
    fn opensnoop_counts_failed_opens_too() {
        let v = opensnoop(&fixture("opensnoop"), "").unwrap();
        assert_eq!(get(&v, "comm:omnref"), 6.0);
        assert_eq!(get(&v, "comm:systemd-journal"), 1.0);
    }

    #[test]
    fn biosnoop_counts_and_sums_latency_by_issuer() {
        let v = biosnoop(&fixture("biosnoop"), "").unwrap();
        assert_eq!(get(&v, "comm:omnref"), 5.0);
        assert_eq!(get(&v, "lat_ms:omnref"), 1.0);
        assert_eq!(get(&v, "comm:kworker/u33:2"), 1.0);
    }

    #[test]
    fn tcpconnect_counts_active_opens_by_comm() {
        let v = tcpconnect(&fixture("tcpconnect"), "").unwrap();
        assert_eq!(get(&v, "comm:omnref"), 3.0);
        assert_eq!(get(&v, ""), 4.0);
    }

    #[test]
    fn tcplife_counts_both_ends_and_sums_their_volume() {
        let v = tcplife(&fixture("tcplife"), "").unwrap();
        assert_eq!(get(&v, "comm:omnref"), 4.0);
        assert_eq!(get(&v, "tx_kb:omnref"), 128.0);
        assert_eq!(get(&v, "rx_kb:omnref"), 128.0);
    }
}
