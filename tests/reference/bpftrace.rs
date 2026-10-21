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
//!
//! One parser reads `omn`'s side: `omn_hist_usecs`, a histogram's median,
//! which `omn` does not report and a path cannot compute. Every other value
//! `omn` holds at one place, and the pairs name it by a path.

use crate::common::{Parser, Values, INCOMPLETE};

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
        Parser {
            name: "omn_hist_runqlen",
            parse: omn_hist_runqlen,
        },
        Parser {
            name: "omn_hist_usecs",
            parse: omn_hist_usecs,
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
/// and `<map>/p90` (the lower bound of the bucket holding the median and the
/// ninetieth percentile, the resolution a log2 histogram has) and
/// `<map>/<low>` per non-empty bucket.
fn flush_hist(values: &mut Values, map: Option<&str>, buckets: &mut Vec<(f64, f64)>) {
    if let Some(map) = map {
        let total: f64 = buckets.iter().map(|(_, c)| c).sum();
        if total > 0.0 {
            add(values, &format!("{map}/count"), total);
            for (key, share) in [("p50", 0.5), ("p90", 0.9)] {
                let mut seen = 0.0;
                for (low, c) in buckets.iter() {
                    seen += c;
                    if seen >= total * share {
                        add(values, &format!("{map}/{key}"), *low);
                        break;
                    }
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

fn bpftrace_hist(stdout: &str, stderr: &str) -> Result<Values, String> {
    hist_scaled(stdout, 1.0).map_err(|e| unattached(stdout, stderr).unwrap_or(e))
}

/// `biolatency` and `runqlat` histogram microseconds; omn reports
/// nanoseconds, so the bounds are scaled here rather than in the pair.
fn bpftrace_hist_usecs(stdout: &str, stderr: &str) -> Result<Values, String> {
    hist_scaled(stdout, 1000.0).map_err(|e| unattached(stdout, stderr).unwrap_or(e))
}

/// A witness whose probes this kernel does not have: bpftrace warns `could
/// not attach probe` for each, skips it, and prints nothing more than its
/// header. biosnoop.bt and biolatency.bt of bpftrace 0.17 and 0.20 (Debian
/// 12, Ubuntu 24.04, found with distroproof) probe `blk_account_io_*`, which
/// their 6.1 and 6.8 kernels no longer have. Asked only of an output with no
/// data: the tool this machine has, refused, not a pair that failed.
fn unattached(stdout: &str, stderr: &str) -> Option<String> {
    let probes: Vec<&str> = stdout
        .lines()
        .chain(stderr.lines())
        .filter_map(|l| l.trim().strip_prefix("WARNING: could not attach probe "))
        .map(|p| p.trim_end_matches(", skipping."))
        .collect();
    (!probes.is_empty()).then(|| {
        format!(
            "{INCOMPLETE}the tool could not attach {} on this kernel and printed no data",
            probes.join(", ")
        )
    })
}

/// The data lines of a snoop tool: those whose `numeric` column parses as an
/// integer, which drops the banner, the header and bpftrace's own messages.
/// bpftrace's own report that its output buffer overflowed (`Lost N
/// events`, or at exit `Total lost event count: N`, as the bpftrace of
/// distroproof's Fedora 44 (0.24) can say it, on either stream): a tool that
/// printed one line per event
/// printed fewer lines than there were events, so its count is refused
/// rather than compared short.
fn lossless(stdout: &str, stderr: &str) -> Result<(), String> {
    let lines = || stdout.lines().chain(stderr.lines()).map(str::trim);
    // The total at exit, when bpftrace gives one (`WARNING: Total lost event
    // count: 3` beside `Lost 3 events` on Rocky 9's 0.24): the two say the
    // same events, so the total stands alone, and the batches are summed
    // where there is no total.
    let total: Option<u64> = lines()
        .filter_map(|l| {
            l.trim_start_matches("WARNING: ")
                .strip_prefix("Total lost event count:")?
                .trim()
                .parse::<u64>()
                .ok()
        })
        .max();
    let lost = total.unwrap_or_else(|| {
        lines()
            .filter_map(|l| {
                l.strip_prefix("Lost ")?
                    .strip_suffix(" events")?
                    .trim()
                    .parse::<u64>()
                    .ok()
            })
            .sum()
    });
    if lost > 0 {
        // The witness lost events: the pair skips quoting it, as perf trace's
        // does, rather than failing omn for the tool's shortfall (Debian 13
        // and Rocky 9 lost 51 and 164 opens, run with distroproof).
        return Err(format!(
            "{INCOMPLETE}bpftrace lost {lost} events: its output is short by them, so it is no \
             count"
        ));
    }
    Ok(())
}

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

/// `TIME PID PPID ARGS`, or `TIME(ms) PID ARGS` from bpftrace 0.17 (Debian
/// 12, found with distroproof): one line per exec, `""` the total and
/// `argv0:<path>` per program as it was executed. The header says which
/// column the arguments start at.
fn execsnoop(stdout: &str, stderr: &str) -> Result<Values, String> {
    lossless(stdout, stderr)?;
    // The rows are the lines after the header: before it, bpftrace's
    // `Attaching 3 probes...` reads as a row of the three-column shape.
    let header = stdout
        .lines()
        .enumerate()
        .find_map(|(at, line)| Some((at, line.split_whitespace().position(|w| w == "ARGS")?)));
    let (args, data) = match header {
        Some((at, args)) => (
            args,
            stdout.lines().skip(at + 1).collect::<Vec<_>>().join("\n"),
        ),
        None => (3, stdout.to_string()),
    };
    let mut values = vec![(String::new(), 0.0)];
    for words in rows(&data, 1) {
        if words.len() <= args || words[1..args].iter().any(|w| w.parse::<i64>().is_err()) {
            continue;
        }
        values[0].1 += 1.0;
        add(&mut values, &format!("argv0:{}", words[args]), 1.0);
    }
    Ok(values)
}

/// `TIME PID COMM SIG TPID RESULT`: one line per kill/tkill/tgkill sent,
/// counted as `<SIG>` and `<comm>/<SIG>`, the sender's comm.
fn killsnoop(stdout: &str, stderr: &str) -> Result<Values, String> {
    lossless(stdout, stderr)?;
    let mut values = vec![(String::new(), 0.0)];
    for words in rows(stdout, 1) {
        let Some(comm) = comm(&words, 2, 3) else {
            continue;
        };
        let sig = signal_name(words[words.len() - 3]);
        let sig = sig.as_str();
        values[0].1 += 1.0;
        add(&mut values, sig, 1.0);
        add(&mut values, &format!("{comm}/{sig}"), 1.0);
    }
    Ok(values)
}

/// A signal by name: bpftrace's killsnoop.bt prints the name (Arch's) or the
/// number (Debian's and Ubuntu's older bpftrace, found with distroproof:
/// `omnref/12` where `omnref/SIGUSR2` was looked for). Linux's numbers on
/// x86-64 and arm64, which agree for these.
fn signal_name(sig: &str) -> String {
    const NAMES: [&str; 31] = [
        "SIGHUP",
        "SIGINT",
        "SIGQUIT",
        "SIGILL",
        "SIGTRAP",
        "SIGABRT",
        "SIGBUS",
        "SIGFPE",
        "SIGKILL",
        "SIGUSR1",
        "SIGSEGV",
        "SIGUSR2",
        "SIGPIPE",
        "SIGALRM",
        "SIGTERM",
        "SIGSTKFLT",
        "SIGCHLD",
        "SIGCONT",
        "SIGSTOP",
        "SIGTSTP",
        "SIGTTIN",
        "SIGTTOU",
        "SIGURG",
        "SIGXCPU",
        "SIGXFSZ",
        "SIGVTALRM",
        "SIGPROF",
        "SIGWINCH",
        "SIGIO",
        "SIGPWR",
        "SIGSYS",
    ];
    match sig.parse::<usize>() {
        Ok(n @ 1..=31) => NAMES[n - 1].to_owned(),
        _ => sig.to_owned(),
    }
}

/// `PID COMM FD ERR PATH`: one line per open/openat/openat2 returned,
/// counted as `comm:<comm>`; a failed open (`FD` -1) counts too, as omn's
/// proc.syscall counts every call.
fn opensnoop(stdout: &str, stderr: &str) -> Result<Values, String> {
    lossless(stdout, stderr)?;
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
fn biosnoop(stdout: &str, stderr: &str) -> Result<Values, String> {
    lossless(stdout, stderr)?;
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
    if values[0].1 == 0.0 {
        if let Some(refused) = unattached(stdout, stderr) {
            return Err(refused);
        }
    }
    Ok(values)
}

/// `TIME PID COMM SADDR SPORT DADDR DPORT`: one line per active open,
/// counted as `comm:<comm>`.
fn tcpconnect(stdout: &str, stderr: &str) -> Result<Values, String> {
    lossless(stdout, stderr)?;
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
fn tcplife(stdout: &str, stderr: &str) -> Result<Values, String> {
    lossless(stdout, stderr)?;
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

/// `omn`'s side of a latency histogram, under the keys `bpftrace_hist_usecs`
/// gives a tool's `@usecs` map: `@usecs/count`, `@usecs/p50` (the lower
/// bound of the bucket holding the median) and `@usecs/<low>` per non-empty
/// bucket, all in nanoseconds, omn's unit. Every `hist` output of every
/// result is merged, its series' buckets added by their lower bound: one
/// series per device or task, where the tool's map is one histogram.
/// `sched.runqlen`'s histogram, every cpu's merged, under runqlen.bt's
/// `@runqlen` map: the same linear buckets, a queue length each.
fn omn_hist_runqlen(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let values = omn_hist_usecs(stdout, _stderr)?;
    Ok(values
        .into_iter()
        .map(|(key, value)| (key.replacen("@usecs", "@runqlen", 1), value))
        .collect())
}

fn omn_hist_usecs(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let doc: serde_json::Value =
        serde_json::from_str(stdout).map_err(|e| format!("omn's output is not JSON: {e}"))?;
    let mut merged: std::collections::BTreeMap<u64, f64> = Default::default();
    let results = doc["results"]
        .as_array()
        .ok_or("no `results` array in omn's output")?;
    for output in results
        .iter()
        .flat_map(|r| r["outputs"].as_array().into_iter().flatten())
    {
        if output["emit"] != "hist" {
            continue;
        }
        for series in output["series"].as_array().into_iter().flatten() {
            for bucket in series["buckets"].as_array().into_iter().flatten() {
                let low = bucket["low"]
                    .as_u64()
                    .ok_or_else(|| format!("a bucket with no integer `low`: {bucket}"))?;
                let count = bucket["count"]
                    .as_f64()
                    .ok_or_else(|| format!("a bucket with no `count`: {bucket}"))?;
                *merged.entry(low).or_default() += count;
            }
        }
    }
    let mut values = Values::new();
    let mut buckets: Vec<(f64, f64)> = merged.into_iter().map(|(l, c)| (l as f64, c)).collect();
    flush_hist(&mut values, Some("@usecs"), &mut buckets);
    if values.is_empty() {
        return Err("no non-empty `hist` output in omn's results".into());
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

    /// Each tool's real output (2026-10-05, bpftrace 0.24 through `sudo
    /// -n`), run as its pair runs it: `under.sh` over the pair's workload,
    /// or system-wide for 6 s around it (`execsnoop`, `tcplife`). The
    /// system-wide captures are trimmed to the workload's lines and five
    /// others, without the paths of other people's programs; the
    /// `.format.txt` files beside them keep the cases a real run did not
    /// produce (a comm with a space, a sender that is not the workload).
    fn real(tool: &str) -> String {
        crate::common::capture("bpftrace", &format!("{tool}.txt"))
    }

    #[test]
    fn every_parser_reads_its_tools_real_output() {
        let v = bpftrace_map(&real("syscount"), "").unwrap();
        assert_eq!(get(&v, "@process[omnref]"), 21215.0);
        let v = opensnoop(&real("opensnoop"), "").unwrap();
        assert_eq!(get(&v, "comm:omnref"), 624.0);
        let v = killsnoop(&real("killsnoop"), "").unwrap();
        assert_eq!(get(&v, "omnref/SIGUSR2"), 50.0);
        let v = bpftrace_hist(&real("bitesize"), "").unwrap();
        assert_eq!(get(&v, "@[omnref]/count"), 508.0);
        assert_eq!(get(&v, "@[omnref]/4096"), 502.0);
        assert_eq!(get(&v, "@[omnref]/p50"), 4096.0);
        let v = biosnoop(&real("biosnoop"), "").unwrap();
        assert_eq!(get(&v, "comm:omnref"), 509.0);
        assert_eq!(get(&v, "lat_ms:omnref"), 8.0);
        let v = bpftrace_hist_usecs(&real("biolatency"), "").unwrap();
        assert_eq!(get(&v, "@usecs/count"), 509.0);
        assert_eq!(get(&v, "@usecs/p50"), 8_000.0);
        let v = bpftrace_hist_usecs(&real("runqlat"), "").unwrap();
        assert_eq!(get(&v, "@usecs/count"), 46441.0);
        assert_eq!(get(&v, "@usecs/p50"), 1_024_000.0);
        let v = bpftrace_hist(&real("runqlen"), "").unwrap();
        assert_eq!(get(&v, "@runqlen/count"), 2812.0);
        assert_eq!(get(&v, "@runqlen/p50"), 2.0);
        let v = tcpconnect(&real("tcpconnect"), "").unwrap();
        assert_eq!(get(&v, "comm:omnref"), 10.0);
        let v = execsnoop(&real("execsnoop"), "").unwrap();
        assert_eq!(get(&v, "argv0:/bin/true"), 20.0);
        let v = tcplife(&real("tcplife"), "").unwrap();
        assert_eq!(get(&v, "comm:omnref"), 20.0);
        assert_eq!(get(&v, "tx_kb:omnref"), 640.0);
    }

    #[test]
    fn execsnoop_reads_both_shapes_of_its_header() {
        let old = "Attaching 3 probes...\nTIME(ms)   PID   ARGS\n1809       1993  sh execs.sh 20\n1809       1994  /bin/true\n1811       1995  /bin/true\n";
        let v = execsnoop(old, "").unwrap();
        assert_eq!(get(&v, "argv0:/bin/true"), 2.0);
        assert_eq!(get(&v, ""), 3.0);
        let new =
            "TIME            PID     PPID    ARGS\n08:59:18.035895 1986    1985    /bin/true\n";
        assert_eq!(get(&execsnoop(new, "").unwrap(), "argv0:/bin/true"), 1.0);
    }

    #[test]
    fn a_witness_that_attached_nothing_is_the_machine_s_refusal() {
        let stderr = "Attaching 6 probes...\ncannot attach kprobe, probe entry may not exist\n\
            WARNING: could not attach probe kprobe:__blk_account_io_done, skipping.\n\
            WARNING: could not attach probe kprobe:blk_account_io_done, skipping.\n";
        let stdout = "TIME(ms)     DISK    COMM             PID    LAT(ms)\n";
        let e = biosnoop(stdout, stderr).unwrap_err();
        assert!(e.starts_with(INCOMPLETE), "{e}");
        assert!(e.contains("kprobe:blk_account_io_done"), "{e}");
        let e = bpftrace_hist_usecs("Attaching 6 probes...\n", stderr).unwrap_err();
        assert!(e.starts_with(INCOMPLETE), "{e}");
        // An empty output with no such warning is still read, and the pair
        // fails on the missing key.
        assert_eq!(get(&biosnoop(stdout, "").unwrap(), ""), 0.0);
    }

    #[test]
    fn an_output_that_lost_events_is_refused() {
        let text = format!("{}Lost 197 events\n", real("opensnoop"));
        let e = opensnoop(&text, "").unwrap_err();
        assert!(e.contains("lost 197 events"), "{e}");
        assert!(biosnoop(&real("biosnoop"), "Lost 3 events\n").is_err());
        let at_exit = format!("{}Total lost event count: 60\n", real("opensnoop"));
        let e = opensnoop(&at_exit, "").unwrap_err();
        assert!(e.contains("lost 60 events"), "{e}");
        // Both forms of the same loss, as Rocky 9's bpftrace prints them:
        // counted once.
        let both = format!("{}Lost 3 events\n", real("opensnoop"));
        let e = opensnoop(&both, "WARNING: Total lost event count: 3\n").unwrap_err();
        assert!(e.contains("lost 3 events"), "{e}");
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

    /// An older bpftrace prints the signal's number: it is counted by name.
    #[test]
    fn killsnoop_names_a_signal_printed_as_its_number() {
        assert_eq!(signal_name("12"), "SIGUSR2");
        assert_eq!(signal_name("15"), "SIGTERM");
        assert_eq!(signal_name("SIGUSR2"), "SIGUSR2");
        assert_eq!(signal_name("64"), "64");
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

    /// omn's real output for the pair `id`, captured on this machine with the
    /// pair's own `omn` argv (`captures/bpftrace/omn/<id>.json`).
    fn omn_capture(id: &str) -> String {
        crate::common::capture("bpftrace", &format!("omn/{id}.json"))
    }

    #[test]
    fn omn_hist_usecs_merges_every_series_and_finds_the_median() {
        let v = omn_hist_usecs(&omn_capture("biolatency"), "").unwrap();
        // 508 requests on two devices; the median in [32, 64) us.
        assert_eq!(get(&v, "@usecs/count"), 508.0);
        assert_eq!(get(&v, "@usecs/p50"), 32768.0);
        assert!(get(&v, "@usecs/p90") >= get(&v, "@usecs/p50"));
        assert_eq!(get(&v, "@usecs/4096"), 44.0);
        let v = omn_hist_usecs(&omn_capture("runqlat"), "").unwrap();
        assert_eq!(get(&v, "@usecs/count"), 26406.0);
        assert_eq!(get(&v, "@usecs/p50"), 65536.0);
    }

    #[test]
    fn omn_hist_usecs_refuses_an_output_with_no_histogram() {
        assert!(omn_hist_usecs(&omn_capture("syscount"), "").is_err());
        assert!(omn_hist_usecs("not json", "").is_err());
    }

    /// Every compare's `omn` side, path or parser, evaluated on the pair's
    /// captured omn output: what the runner reads from omn where the tool
    /// cannot run (bpftrace needs root). The numbers are what each workload
    /// fixes, or the median omn measured here.
    #[test]
    fn every_omn_side_resolves_on_its_capture() {
        let expected = [
            ("syscount", 21216.0), // 20000 getppid and python3's startup
            ("opensnoop", 623.0),  // 500 opens and about 120 at startup
            ("killsnoop", 50.0),
            ("execsnoop", 20.0),
            ("bitesize", 504.0), // 500 O_DIRECT 4 KiB reads and the setup's
            ("biosnoop", 508.0),
            ("biolatency", 32768.0),
            ("runqlat", 4194304.0),
            ("tcpconnect", 10.0),
            ("tcplife", 20.0),
        ];
        // Pairs whose omn side has no capture yet: their source needs the
        // privileges this checkout's machine does not grant, and a capture
        // is a real run's output, never written by hand. Each is recaptured
        // on the first privileged run.
        let awaiting = ["runqlen"];
        let pairs = crate::runner::load("bpftrace").unwrap();
        assert_eq!(pairs.len(), expected.len() + awaiting.len());
        for pair in &pairs {
            let id = pair.id.as_deref().expect("every bpftrace pair has an id");
            if awaiting.contains(&id) {
                continue;
            }
            let want = expected
                .iter()
                .find(|(i, _)| *i == id)
                .unwrap_or_else(|| panic!("no expected value for `{id}`"))
                .1;
            let text = omn_capture(id);
            for c in pair.compare.all() {
                let got = match (&c.omn, &c.omn_parse) {
                    (Some(path), None) => {
                        let doc = crate::runner::parse_json(&text).unwrap();
                        crate::runner::eval_path(&doc, path)
                            .unwrap_or_else(|e| panic!("{id}: `{path}`: {e}"))
                    }
                    (None, Some(name)) => {
                        let parser = parsers().into_iter().find(|p| p.name == name).unwrap();
                        get(&(parser.parse)(&text, "").unwrap(), &c.key)
                    }
                    _ => panic!("{id}: a compare names exactly one omn side"),
                };
                assert_eq!(got * c.omn_scale, want, "{id}");
            }
        }
    }

    #[test]
    fn tcplife_counts_both_ends_and_sums_their_volume() {
        let v = tcplife(&fixture("tcplife"), "").unwrap();
        assert_eq!(get(&v, "comm:omnref"), 4.0);
        assert_eq!(get(&v, "tx_kb:omnref"), 128.0);
        assert_eq!(get(&v, "rx_kb:omnref"), 128.0);
    }
}
