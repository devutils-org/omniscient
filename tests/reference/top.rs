//! Parsers for the `top` family's reference tools. Each turns a tool's
//! output into the values a pair compares (BRIEF.md, "Decided before this
//! wave"). Sizes come out in bytes and times in nanoseconds, the units omn
//! reports them in; a utilization comes out as a ratio in [0, 1].
//!
//! The tools that take a pid (`top -p`, `pmap`, `smem -P`) run under the
//! runner's `attach`, through `tests/reference/workloads/top/at.sh`, which
//! waits for the workload's phase before it execs the tool. `omn_*` parsers
//! read `omn`'s own JSON, for the values `omn` does not hold at one place
//! (`compare.omn_parse`).

use crate::common::{Parser, Values, INCOMPLETE};
use serde_json::Value;

pub fn parsers() -> Vec<Parser> {
    vec![
        Parser {
            name: "top_batch_pid",
            parse: top_batch_pid,
        },
        Parser {
            name: "top_batch_cpus",
            parse: top_batch_cpus,
        },
        Parser {
            name: "pmap_x_total",
            parse: pmap_x_total,
        },
        Parser {
            name: "pmap_XX_total",
            parse: pmap_xx_total,
        },
        Parser {
            name: "slabtop_cache_size",
            parse: slabtop_cache_size,
        },
        Parser {
            name: "omn_slab_bytes",
            parse: omn_slab_bytes,
        },
        Parser {
            name: "smem_uss_pss_rss",
            parse: smem_uss_pss_rss,
        },
        Parser {
            name: "omn_cpu_total",
            parse: omn_cpu_total,
        },
        Parser {
            name: "omn_cpu_share",
            parse: omn_cpu_share,
        },
        Parser {
            name: "omn_top_batch_busy",
            parse: omn_top_batch_busy,
        },
        Parser {
            name: "omn_cpu_util_polls",
            parse: omn_cpu_util_polls,
        },
    ]
}

/// One tick of the kernel's cpu accounting (`USER_HZ`, 100 wherever
/// `/proc/stat` is read), in ns.
const TICK_NS: f64 = 10_000_000.0;

/// `cpu.util --system`'s raw events in `omn`'s JSON: the mean `busy` over
/// the reads that cover an interval, keyed `busy` as `top`'s `%Cpu(s)`.
///
/// A read less than one tick after the window opened is refused, not left
/// out: it is the leading read of defect 2 of `docs/fixes/w22-sysstat.md`,
/// a first poll taken at once after the baseline read, whose share is a few
/// ticks over a fraction of a millisecond -- 0, 0.5 or 1 -- which a mean then
/// weighs like a whole second. Stream cpu fixed it in the twenty-fourth wave
/// (`c13eb51`), so the pair fails, naming the defect, if it comes back,
/// rather than passing on the reads around it. Every other read covers one
/// poll interval, so their plain mean is the share over their span.
/// `omn top --batch`'s table: the machine's `cpu.util` row, its mean busy
/// share over the run, as `busy`.
fn omn_top_batch_busy(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let row = stdout
        .lines()
        .find(|line| line.starts_with("machine cpu.util "))
        .ok_or("no `machine cpu.util` row: not the output of `omn top --batch`")?;
    let mean = row
        .split_whitespace()
        .find_map(|cell| cell.strip_prefix("mean="))
        .ok_or_else(|| format!("no mean on `{row}`"))?
        .parse::<f64>()
        .map_err(|_| format!("the mean on `{row}` is not a number"))?;
    Ok(vec![("busy".into(), mean)])
}

fn omn_cpu_util_polls(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let doc: Value =
        serde_json::from_str(stdout).map_err(|e| format!("omn's output is not JSON: {e}"))?;
    let result = doc["results"]
        .as_array()
        .and_then(|r| r.iter().find(|r| r["source"] == "cpu.util"))
        .ok_or("no cpu.util result in omn's output")?;
    let opened = result["window"]["opened_ns"]
        .as_f64()
        .ok_or("no window.opened_ns in omn's cpu.util result")?;
    let events = result["outputs"]
        .as_array()
        .and_then(|o| o.iter().find(|o| o["emit"] == "raw"))
        .and_then(|o| o["events"].as_array())
        .ok_or("no raw events in omn's cpu.util result")?;
    for e in events {
        let ts = e["ts_ns"]
            .as_f64()
            .ok_or_else(|| format!("a cpu.util event with no ts_ns: {e}"))?;
        if ts - opened < TICK_NS {
            return Err(format!(
                "a cpu.util read {:.3} ms after the window opened, less than a tick: \
                 the leading read of defect 2 (docs/fixes/w22-sysstat.md) is back",
                (ts - opened) / 1e6
            ));
        }
    }
    let busy: Vec<f64> = events
        .iter()
        .map(|e| {
            e["attrs"]
                .as_array()
                .and_then(|a| a.iter().find(|a| a["name"] == "busy"))
                .and_then(|a| a["value"].as_f64())
                .ok_or_else(|| format!("a cpu.util event with no busy: {e}"))
        })
        .collect::<Result<_, _>>()?;
    if busy.is_empty() {
        return Err("no cpu.util read covering an interval in omn's output".into());
    }
    Ok(vec![(
        "busy".into(),
        busy.iter().sum::<f64>() / busy.len() as f64,
    )])
}

/// `cpu.oncpu_time`'s summary in `omn`'s JSON: the sum over its series of
/// `count * mean`, which is the sum of the per-poll deltas of a cumulative
/// counter (one series per thread through `bpf:raw_tp`, the process through
/// `fs:proc`), and the result's `observed_ns`.
fn omn_cpu(stdout: &str) -> Result<(f64, f64), String> {
    let doc: Value =
        serde_json::from_str(stdout).map_err(|e| format!("omn's output is not JSON: {e}"))?;
    let result = doc["results"]
        .as_array()
        .and_then(|r| r.iter().find(|r| r["source"] == "cpu.oncpu_time"))
        .ok_or("no cpu.oncpu_time result in omn's output")?;
    let series = result["outputs"]
        .as_array()
        .and_then(|o| o.iter().find(|o| o["emit"] == "summary"))
        .and_then(|o| o["series"].as_array())
        .ok_or("no summary series in omn's cpu.oncpu_time result")?;
    if series.is_empty() {
        return Err("omn's cpu.oncpu_time summary has no series".into());
    }
    let mut total = 0.0;
    for s in series {
        let (Some(count), Some(mean)) = (s["count"].as_f64(), s["mean"].as_f64()) else {
            return Err(format!("a series without count and mean: {s}"));
        };
        total += count * mean;
    }
    let observed = result["observed_ns"]
        .as_f64()
        .filter(|o| *o > 0.0)
        .ok_or("no positive observed_ns in omn's cpu.oncpu_time result")?;
    Ok((total, observed))
}

/// The cpu `omn` saw the target spend, in ns, keyed `time` as `top`'s TIME+.
fn omn_cpu_total(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let (total, _) = omn_cpu(stdout)?;
    Ok(vec![("time".into(), total)])
}

/// That cpu over the time `omn` observed, a ratio of one cpu, keyed `cpu`
/// as `top`'s `%CPU`.
fn omn_cpu_share(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let (total, observed) = omn_cpu(stdout)?;
    Ok(vec![("cpu".into(), total / observed)])
}

const KIB: f64 = 1024.0;

/// A size as `top` prints it in its task area: KiB by default, or scaled
/// with a suffix once it no longer fits its column (`1.2g`, `512.0m`).
fn top_size(field: &str) -> Result<f64, String> {
    let (number, scale) = match field.chars().last() {
        Some('k') => (&field[..field.len() - 1], 1.0),
        Some('m') => (&field[..field.len() - 1], KIB),
        Some('g') => (&field[..field.len() - 1], KIB * KIB),
        Some('t') => (&field[..field.len() - 1], KIB * KIB * KIB),
        Some('p') => (&field[..field.len() - 1], KIB * KIB * KIB * KIB),
        _ => (field, 1.0),
    };
    let kib: f64 = number
        .parse()
        .map_err(|_| format!("not a top size: {field:?}"))?;
    Ok(kib * scale * KIB)
}

/// `TIME+` as `top` prints it: `M:SS.hh`, or `MMMM:SS` once minutes run
/// long (the hundredths dropped).
fn top_time_plus(field: &str) -> Result<f64, String> {
    let bad = || format!("not a TIME+ value: {field:?}");
    let (minutes, seconds) = field.split_once(':').ok_or_else(bad)?;
    let minutes: f64 = minutes.parse().map_err(|_| bad())?;
    let seconds: f64 = seconds.parse().map_err(|_| bad())?;
    Ok((minutes * 60.0 + seconds) * 1e9)
}

/// `top -b -p PID`: the task line of the last iteration, under its own
/// header. Keys: `res` and `virt` and `shr` (bytes), `time` (ns, `TIME+`),
/// `cpu` (`%CPU` as a ratio of one cpu).
fn top_batch_pid(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let mut header: Option<Vec<&str>> = None;
    let mut last: Option<Vec<&str>> = None;
    for line in stdout.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.first() == Some(&"PID") {
            header = Some(fields);
        } else if header.is_some() && fields.first().is_some_and(|f| f.parse::<u32>().is_ok()) {
            last = Some(fields);
        }
    }
    let header = header.ok_or("no task header (`PID USER ...`) in top's output")?;
    let task = last.ok_or("no task line under top's header")?;
    let column = |name: &str| -> Result<&str, String> {
        let at = header
            .iter()
            .position(|h| *h == name)
            .ok_or_else(|| format!("no {name} column in top's header"))?;
        task.get(at)
            .copied()
            .ok_or_else(|| format!("the task line has no {name} field"))
    };
    let cpu: f64 = column("%CPU")?
        .parse()
        .map_err(|_| "%CPU is not a number".to_string())?;
    Ok(vec![
        ("res".into(), top_size(column("RES")?)?),
        ("virt".into(), top_size(column("VIRT")?)?),
        ("shr".into(), top_size(column("SHR")?)?),
        ("time".into(), top_time_plus(column("TIME+")?)?),
        ("cpu".into(), cpu / 100.0),
    ])
}

/// `top -b -n 2 -d 1`: the summary area's last `%Cpu(s)` line, which covers
/// the delay between the two iterations (the first covers the time since
/// boot). Keys: each state as a ratio (`us`, `sy`, `ni`, `id`, `wa`, `hi`,
/// `si`, `st`), and `busy`, one less idle and iowait -- what omn's
/// `cpu.util` reads as busy.
fn top_batch_cpus(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let line = stdout
        .lines()
        .rfind(|l| l.starts_with("%Cpu(s):"))
        .ok_or("no `%Cpu(s):` line in top's output")?;
    let mut values = Values::new();
    for part in line["%Cpu(s):".len()..].split(',') {
        let mut words = part.split_whitespace();
        let (Some(number), Some(state)) = (words.next(), words.next()) else {
            return Err(format!("not a `N.N state` pair: {part:?}"));
        };
        let percent: f64 = number
            .parse()
            .map_err(|_| format!("not a percentage: {number:?}"))?;
        values.push((state.to_string(), percent / 100.0));
    }
    let get = |k: &str| values.iter().find(|(s, _)| s == k).map(|(_, v)| *v);
    let (Some(id), Some(wa)) = (get("id"), get("wa")) else {
        return Err("the `%Cpu(s):` line has no `id` or no `wa`".into());
    };
    values.push(("busy".into(), 1.0 - id - wa));
    Ok(values)
}

/// `pmap -x PID`: the `total kB` line's resident column, in bytes.
fn pmap_x_total(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let line = stdout
        .lines()
        .find(|l| l.starts_with("total kB"))
        .ok_or("no `total kB` line in pmap's output")?;
    let fields: Vec<&str> = line.split_whitespace().collect();
    // total kB <Kbytes> <RSS> <Dirty>
    let rss: f64 = fields
        .get(3)
        .and_then(|f| f.parse().ok())
        .ok_or_else(|| format!("no RSS on pmap's total line: {line:?}"))?;
    Ok(vec![(String::new(), rss * KIB)])
}

/// `pmap -XX PID`: the totals line under the per-mapping table, matched to
/// the header's numeric columns (`Size` through `THPeligible`). Keys: every
/// such column lowercased, in bytes (`rss`, `pss`, `private_dirty`, ...),
/// and `uss`, `Private_Clean + Private_Dirty`, what dies with the process.
fn pmap_xx_total(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let lines: Vec<&str> = stdout.lines().collect();
    let header: Vec<&str> = lines
        .iter()
        .find(|l| l.split_whitespace().next() == Some("Address"))
        .ok_or("no `Address ...` header in pmap's output")?
        .split_whitespace()
        .collect();
    let totals: Vec<f64> = lines
        .iter()
        .rev()
        .find(|l| l.trim_end().ends_with("KB"))
        .ok_or("no totals line ending in `KB` in pmap's output")?
        .split_whitespace()
        .filter_map(|f| f.parse().ok())
        .collect();
    let first = header
        .iter()
        .position(|h| *h == "Size")
        .ok_or("no Size column in pmap's header")?;
    let numeric = &header[first..];
    if numeric.len() < totals.len() {
        return Err(format!(
            "{} totals for {} columns from Size on",
            totals.len(),
            numeric.len()
        ));
    }
    let mut values: Values = numeric
        .iter()
        .zip(&totals)
        .map(|(name, kib)| (name.to_lowercase(), kib * KIB))
        .collect();
    let get = |values: &Values, k: &str| values.iter().find(|(s, _)| s == k).map(|(_, v)| *v);
    if get(&values, "rss").is_none() {
        return Err("no Rss column among pmap's totals".into());
    }
    if let (Some(clean), Some(dirty)) =
        (get(&values, "private_clean"), get(&values, "private_dirty"))
    {
        values.push(("uss".into(), clean + dirty));
    }
    Ok(values)
}
/// `slabtop -o`: the sum of the table's CACHE SIZE column, in bytes, under
/// `cache_size` -- each cache's slabs times its pages, free objects included,
/// which is what `mem.slab`'s `bytes` are. Not the header's "Total Size",
/// which is objects times their size, 1.4 % less here.
fn slabtop_cache_size(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let mut kib = 0.0;
    let mut rows = 0;
    for line in stdout.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        // OBJS ACTIVE USE OBJ_SIZE SLABS OBJ/SLAB CACHE_SIZE NAME
        if words.len() < 8 || words[0].parse::<u64>().is_err() {
            continue;
        }
        kib += words[6]
            .trim_end_matches('K')
            .parse::<f64>()
            .map_err(|_| format!("a slabtop row's CACHE SIZE `{}` is no number", words[6]))?;
        rows += 1;
    }
    if rows == 0 {
        return Err("slabtop printed no cache row".into());
    }
    // `Active / Total Caches (% used) : 77 / 120`: procps-ng 3.3's slabtop
    // stops at its screen's height even with `-o`, 16 of 120 caches on
    // Ubuntu 18.04 (2026-10-21), and a sum of those is no machine's total.
    let total = stdout
        .lines()
        .find(|l| l.contains("Total Caches"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.split('/').nth(1))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|v| v.parse::<usize>().ok());
    if let Some(total) = total.filter(|&t| rows < t) {
        return Err(format!(
            "{INCOMPLETE}slabtop printed {rows} of its {total} caches: an older slabtop \
             stops at its screen's height"
        ));
    }
    Ok(vec![("cache_size".into(), kib * 1024.0)])
}

/// `omn trace mem.slab -m snapshot -e raw`: every cache's `bytes`, summed,
/// under `cache_size`.
fn omn_slab_bytes(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let doc: serde_json::Value =
        serde_json::from_str(stdout).map_err(|e| format!("omn's output is not JSON: {e}"))?;
    let events = doc["results"][0]["outputs"][0]["events"]
        .as_array()
        .ok_or("omn's mem.slab has no raw events")?;
    let bytes: f64 = events
        .iter()
        .filter_map(|event| {
            event["attrs"]
                .as_array()?
                .iter()
                .find(|attr| attr["name"] == "bytes")?["value"]
                .as_f64()
        })
        .sum();
    if events.is_empty() {
        return Err("omn read no slab cache".into());
    }
    Ok(vec![("cache_size".into(), bytes)])
}

/// `smem -H -c "uss pss rss" -P REGEX`: one row per matched process, three
/// columns in KiB. Keys `uss`, `pss`, `rss` in bytes; a regex that matched
/// more or fewer than one process is an error, never a sum.
fn smem_uss_pss_rss(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let rows: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    let [row] = rows.as_slice() else {
        return Err(format!(
            "smem matched {} processes, the pair wants exactly one",
            rows.len()
        ));
    };
    let fields: Vec<f64> = row
        .split_whitespace()
        .map(|f| f.parse().map_err(|_| format!("not a KiB count: {f:?}")))
        .collect::<Result<_, _>>()?;
    let [uss, pss, rss] = fields.as_slice() else {
        return Err(format!("expected uss pss rss, got {row:?}"));
    };
    Ok(vec![
        ("uss".into(), uss * KIB),
        ("pss".into(), pss * KIB),
        ("rss".into(), rss * KIB),
    ])
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_slabtop_cut_at_its_screen_is_incomplete() {
        let head = " Active / Total Caches (% used)     : 2 / 3 (66.7%)\n  OBJS ACTIVE  USE OBJ SIZE  SLABS OBJ/SLAB CACHE SIZE NAME\n";
        let rows = "16900  16722   0%    0.60K   1300       13     10400K inode_cache\n  1904   1722   0%    0.50K    119       16       952K kmalloc-512\n";
        let cut = slabtop_cache_size(&format!("{head}{rows}"), "").unwrap_err();
        assert!(
            cut.starts_with(INCOMPLETE) && cut.contains("2 of its 3"),
            "{cut}"
        );
        let all = format!(
            "{head}{rows}    10     10 100%    0.10K      1       39         4K buffer_head\n"
        );
        assert_eq!(
            slabtop_cache_size(&all, "").unwrap()[0].1,
            (10400.0 + 952.0 + 4.0) * 1024.0
        );
    }

    use super::*;

    #[test]
    fn omn_top_batch_reads_the_machine_s_cpu_row() {
        let table = "# omn top --batch: live: the machine\n# span_ns 2019292252\nmachine cpu.util system mean=0.0755 peak=0.1049 last=0.0630 total=0.6037\nmachine io.bytes 259+0 mean=0 peak=0 last=0 total=0\n";
        let v = omn_top_batch_busy(table, "").unwrap();
        assert_eq!(v, vec![("busy".to_owned(), 0.0755)]);
        assert!(omn_top_batch_busy("nothing", "").is_err());
    }

    fn capture(name: &str) -> String {
        let path = format!(
            "{}/tests/reference/captures/top/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    fn get(values: &Values, key: &str) -> f64 {
        values
            .iter()
            .find(|(k, _)| k == key)
            .unwrap_or_else(|| panic!("no {key:?} in {values:?}"))
            .1
    }

    #[test]
    fn every_parser_is_named_once() {
        let names: Vec<_> = parsers().iter().map(|p| p.name).collect();
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(names.len(), unique.len(), "{names:?}");
    }

    #[test]
    fn top_reads_the_task_line_of_a_captured_run() {
        let v = top_batch_pid(&capture("top-b-pid.txt"), "").unwrap();
        assert_eq!(get(&v, "res"), 75604.0 * KIB);
        assert_eq!(get(&v, "virt"), 83156.0 * KIB);
        assert_eq!(get(&v, "shr"), 6836.0 * KIB);
        assert!((get(&v, "time") - 1.99e9).abs() < 1.0);
        assert_eq!(get(&v, "cpu"), 0.0);
    }

    #[test]
    fn top_reads_scaled_sizes_and_long_times() {
        assert_eq!(top_size("1.5g").unwrap(), 1.5 * KIB * KIB * KIB);
        assert_eq!(top_size("512.0m").unwrap(), 512.0 * KIB * KIB);
        assert_eq!(top_size("75604").unwrap(), 75604.0 * KIB);
        assert!(top_size("lots").is_err());
        assert_eq!(
            top_time_plus("1234:56").unwrap(),
            (1234.0 * 60.0 + 56.0) * 1e9
        );
        assert!(top_time_plus("1.99").is_err());
    }

    #[test]
    fn top_refuses_an_output_with_no_task() {
        let header_only =
            "    PID USER      PR  NI    VIRT    RES    SHR S  %CPU  %MEM     TIME+ COMMAND\n";
        assert!(top_batch_pid(header_only, "").is_err());
        assert!(top_batch_pid("", "").is_err());
    }

    #[test]
    fn top_reads_the_last_cpu_line_as_the_interval() {
        let v = top_batch_cpus(&capture("top-b-system.txt"), "").unwrap();
        // The second `%Cpu(s):` line, not the first (since boot).
        assert!((get(&v, "us") - 0.105).abs() < 1e-9);
        assert!((get(&v, "id") - 0.766).abs() < 1e-9);
        assert!((get(&v, "busy") - (1.0 - 0.766 - 0.069)).abs() < 1e-9);
        assert!(top_batch_cpus("no summary here", "").is_err());
    }

    #[test]
    fn pmap_x_reads_the_resident_total() {
        let v = pmap_x_total(&capture("pmap-x.txt"), "").unwrap();
        assert_eq!(v, vec![(String::new(), 75380.0 * KIB)]);
        assert!(pmap_x_total("2926018:   python3\n", "").is_err());
    }

    #[test]
    fn pmap_xx_matches_totals_to_their_columns() {
        let v = pmap_xx_total(&capture("pmap-XX.txt"), "").unwrap();
        assert_eq!(get(&v, "size"), 83160.0 * KIB);
        assert_eq!(get(&v, "rss"), 75392.0 * KIB);
        assert_eq!(get(&v, "pss"), 70058.0 * KIB);
        assert_eq!(get(&v, "anonhugepages"), 65536.0 * KIB);
        assert_eq!(get(&v, "uss"), (28.0 + 68752.0) * KIB);
        assert!(pmap_xx_total("2926018:   python3\n", "").is_err());
    }

    #[test]
    // Captured from `omn trace cpu.oncpu_time -m poll -e summary -d 6s --pid
    // PID` attached to `hold.py 64 2 1` stopped before its exec, the TIME+
    // pair's shape: four polls of one thread through `bpf:raw_tp`.
    fn omn_cpu_total_sums_the_series_deltas() {
        let v = omn_cpu_total(&capture("omn-cpu-oncpu-time-pid.json"), "").unwrap();
        assert_eq!(v.len(), 1);
        assert!((get(&v, "time") - 4.0 * 500322610.25).abs() < 1.0);
        assert!(omn_cpu_total("not json", "").is_err());
        assert!(omn_cpu_total(r#"{"results": []}"#, "").is_err());
    }

    #[test]
    // Captured from the %CPU pair's shape: `--window 1s..3s` on `hold.py 16
    // 5 0.5` while it spins.
    fn omn_cpu_share_divides_by_the_observed_time() {
        let text = capture("omn-cpu-oncpu-time-window.json");
        let v = omn_cpu_share(&text, "").unwrap();
        let share = get(&v, "cpu");
        assert!((share - 0.9902962096348015).abs() < 1e-9, "{share}");
        let empty = r#"{"results": [{"source": "cpu.oncpu_time", "observed_ns": 0,
            "outputs": [{"emit": "summary", "series": [{"count": 1, "mean": 5}]}]}]}"#;
        assert!(omn_cpu_share(empty, "").is_err());
    }

    #[test]
    fn omn_cpu_util_polls_refuses_a_read_at_the_windows_opening() {
        // A real `omn trace cpu.util --system -m poll -e raw -d 3s --window
        // 1s..3s` from before stream cpu's fix (2026-10-05): reads at +0.3 ms
        // (busy 1.0, a few ticks), +1.0004 s (0.846) and +1.9998 s (0.814)
        // past the window's opening. The sub-tick read fails the parse.
        let raw = capture("omn-cpu-util-raw-window.json");
        let err = omn_cpu_util_polls(&raw, "").unwrap_err();
        assert!(err.contains("defect 2"), "{err}");
        // The same run without that read, which is what today's `omn` gives.
        let mut doc: Value = serde_json::from_str(&raw).unwrap();
        doc["results"][0]["outputs"]
            .as_array_mut()
            .and_then(|o| o.iter_mut().find(|o| o["emit"] == "raw"))
            .and_then(|o| o["events"].as_array_mut())
            .unwrap()
            .remove(0);
        let values = omn_cpu_util_polls(&doc.to_string(), "").unwrap();
        let busy = get(&values, "busy");
        assert!((busy - (0.846 + 0.814) / 2.0).abs() < 0.001, "{busy}");
        assert!(omn_cpu_util_polls("{\"results\": []}", "").is_err());
    }

    #[test]
    // Captured from smem 1.5 (the AUR's smem 1.5-8, installed) through at.sh
    // on `hold.py 64 2 1` at `hold-done`, the pairs' exact command: the
    // regex matched the workload alone.
    fn smem_reads_one_row_and_refuses_many() {
        let v = smem_uss_pss_rss(&capture("smem-H-c-uss-pss-rss.txt"), "").unwrap();
        assert_eq!(get(&v, "uss"), 68812.0 * KIB);
        assert_eq!(get(&v, "pss"), 70210.0 * KIB);
        assert_eq!(get(&v, "rss"), 75620.0 * KIB);
        let two = "    68780    70058    75392 \n     9000    10000    12000 \n";
        assert!(smem_uss_pss_rss(two, "").is_err());
        assert!(smem_uss_pss_rss("", "").is_err());
    }
}
