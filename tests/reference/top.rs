//! Parsers for the `top` family's reference tools. Each turns a tool's
//! output into the values a pair compares (BRIEF.md, "Decided before this
//! wave"). Sizes come out in bytes and times in nanoseconds, the units omn
//! reports them in; a utilization comes out as a ratio in [0, 1].
//!
//! The tools that take a pid (`top -p`, `pmap`, `smem -P`) are run through
//! `tests/reference/workloads/top/onpid.py`, which launches the workload and
//! hands the tool its pid once the workload says it is ready.

use crate::common::{Parser, Values};

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
            name: "smem_uss_pss_rss",
            parse: smem_uss_pss_rss,
        },
    ]
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
    use super::*;

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
    // Captured from smem 1.5 (Debian's smem_1.5-3, the script run in place;
    // no Arch package) through onpid.py on `hold.py 64 2 1`, the pairs'
    // exact command: the regex matched the workload alone.
    fn smem_reads_one_row_and_refuses_many() {
        let v = smem_uss_pss_rss(&capture("smem-H-c-uss-pss-rss.txt"), "").unwrap();
        assert_eq!(get(&v, "uss"), 68800.0 * KIB);
        assert_eq!(get(&v, "pss"), 69803.0 * KIB);
        assert_eq!(get(&v, "rss"), 75484.0 * KIB);
        let two = "    68780    70058    75392 \n     9000    10000    12000 \n";
        assert!(smem_uss_pss_rss(two, "").is_err());
        assert!(smem_uss_pss_rss("", "").is_err());
    }
}
