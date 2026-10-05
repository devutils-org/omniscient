//! Parsers for the `heap` family's reference tools. Each turns a tool's
//! output into the values a pair compares (`catalog/references/README.md`).
//! Every `omn` side of this family is a dotted path into `results.json`,
//! so no parser of `omn`'s output is needed here.
//!
//! - `dhat_summary`: `valgrind --tool=dhat`'s closing lines on stderr --
//!   `total_bytes`, `total_blocks`, `gmax_bytes`, `gmax_blocks`,
//!   `end_bytes`, `end_blocks`.
//! - `massif_peak`: a massif output file written to stdout
//!   (`--massif-out-file=/dev/stdout`) -- `peak` (`mem_heap_B` of the
//!   snapshot massif marks `heap_tree=peak`), `peak_extra`, and `max`, the
//!   largest `mem_heap_B` of any snapshot.
//! - `heaptrack_summary`: `heaptrack_print`'s closing lines -- `calls`,
//!   `temporary`, `peak_bytes`, `leaked_bytes`.
//! - `memleak_outstanding`: `memleak` (bcc's or libbpf-tools'), its last
//!   report -- `bytes` and `allocations` outstanding, summed over every
//!   stack.

use crate::common::{Parser, Values};

pub fn parsers() -> Vec<Parser> {
    vec![
        Parser {
            name: "dhat_summary",
            parse: dhat_summary,
        },
        Parser {
            name: "massif_peak",
            parse: massif_peak,
        },
        Parser {
            name: "heaptrack_summary",
            parse: heaptrack_summary,
        },
        Parser {
            name: "memleak_outstanding",
            parse: memleak_outstanding,
        },
    ]
}

/// An integer as valgrind and bcc print it, thousands separators allowed.
fn integer(text: &str) -> Result<f64, String> {
    let digits: String = text.trim().chars().filter(|c| *c != ',').collect();
    digits
        .parse::<u64>()
        .map(|v| v as f64)
        .map_err(|_| format!("`{}` is not an integer", text.trim()))
}

/// `<N> bytes in <M> blocks`, the shape of each of dhat's summary lines.
fn bytes_in_blocks(text: &str) -> Result<(f64, f64), String> {
    let (bytes, rest) = text
        .split_once("bytes in")
        .ok_or_else(|| format!("`{}` is not `N bytes in M blocks`", text.trim()))?;
    let blocks = rest
        .trim()
        .trim_end_matches("blocks")
        .trim_end_matches("block");
    Ok((integer(bytes)?, integer(blocks)?))
}

fn dhat_summary(_stdout: &str, stderr: &str) -> Result<Values, String> {
    let mut values = Values::new();
    for line in stderr.lines() {
        // `==PID== Total:     73,637,188 bytes in 3,180 blocks`
        let body = match line.strip_prefix("==") {
            Some(rest) => rest.split_once("== ").map(|(_, b)| b).unwrap_or(rest),
            None => line,
        };
        let Some((name, rest)) = body.split_once(':') else {
            continue;
        };
        let prefix = match name.trim() {
            "Total" => "total",
            "At t-gmax" => "gmax",
            "At t-end" => "end",
            _ => continue,
        };
        let (bytes, blocks) = bytes_in_blocks(rest)?;
        values.push((format!("{prefix}_bytes"), bytes));
        values.push((format!("{prefix}_blocks"), blocks));
    }
    if values.len() != 6 {
        return Err(format!(
            "dhat's summary not found on stderr (wanted Total, At t-gmax and At t-end, got {} of them)",
            values.len() / 2
        ));
    }
    Ok(values)
}

fn massif_peak(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let mut heap = None;
    let mut extra = None;
    let mut peak = None;
    let mut max: Option<f64> = None;
    let mut snapshots = 0usize;
    for line in stdout.lines() {
        if line.starts_with("snapshot=") {
            snapshots += 1;
            heap = None;
            extra = None;
        } else if let Some(v) = line.strip_prefix("mem_heap_B=") {
            let v = integer(v)?;
            max = Some(max.map_or(v, |m| m.max(v)));
            heap = Some(v);
        } else if let Some(v) = line.strip_prefix("mem_heap_extra_B=") {
            extra = Some(integer(v)?);
        } else if line == "heap_tree=peak" {
            peak = Some((
                heap.ok_or("the peak snapshot has no mem_heap_B")?,
                extra.unwrap_or(0.0),
            ));
        }
    }
    if snapshots == 0 {
        return Err(
            "no massif snapshot on stdout (was --massif-out-file=/dev/stdout given?)".into(),
        );
    }
    let (peak, peak_extra) =
        peak.ok_or("massif marked no snapshot as the peak (heap_tree=peak)")?;
    Ok(vec![
        ("peak".into(), peak),
        ("peak_extra".into(), peak_extra),
        ("max".into(), max.unwrap_or(0.0)),
    ])
}

/// A size as `heaptrack_print` formats it: `68.91M`, `256B`, `4.61K`.
/// Base 1000, as this machine's heaptrack 1.5 prints it (68.98M for
/// 68,9xx,xxx bytes); the pairs' tolerance absorbs the rounding to two
/// decimals of the unit.
fn heaptrack_size(text: &str) -> Result<f64, String> {
    let t = text.trim();
    let t = t.strip_suffix('B').unwrap_or(t);
    let (number, scale) = match t.chars().last() {
        Some('K') => (&t[..t.len() - 1], 1e3),
        Some('M') => (&t[..t.len() - 1], 1e6),
        Some('G') => (&t[..t.len() - 1], 1e9),
        Some('T') => (&t[..t.len() - 1], 1e12),
        _ => (t, 1.0),
    };
    number
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
        .map(|v| v * scale)
        .ok_or_else(|| format!("`{}` is not a heaptrack size", text.trim()))
}

fn heaptrack_summary(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let mut values = Values::new();
    for line in stdout.lines() {
        let Some((name, rest)) = line.split_once(": ") else {
            continue;
        };
        // `calls to allocation functions: 3183 (7578/s)`
        let count = || integer(rest.split(" (").next().unwrap_or(rest));
        match name {
            "calls to allocation functions" => values.push(("calls".into(), count()?)),
            "temporary memory allocations" => values.push(("temporary".into(), count()?)),
            "peak heap memory consumption" => {
                values.push(("peak_bytes".into(), heaptrack_size(rest)?))
            }
            "total memory leaked" => values.push(("leaked_bytes".into(), heaptrack_size(rest)?)),
            _ => {}
        }
    }
    for wanted in ["calls", "peak_bytes", "leaked_bytes"] {
        if !values.iter().any(|(k, _)| k == wanted) {
            return Err(format!("heaptrack_print's summary has no `{wanted}` line"));
        }
    }
    Ok(values)
}

fn memleak_outstanding(stdout: &str, _stderr: &str) -> Result<Values, String> {
    // Every report starts `[HH:MM:SS] Top N stacks with outstanding
    // allocations:`; only the last one is the program's end state.
    let last = stdout
        .rfind("stacks with outstanding allocations:")
        .ok_or("memleak printed no report")?;
    let mut bytes = 0.0;
    let mut allocations = 0.0;
    for line in stdout[last..].lines().skip(1) {
        // bcc: `\t4194304 bytes in 4 allocations from stack`; libbpf-tools:
        // the same line without the tab.
        let line = line.trim();
        if let Some(head) = line.strip_suffix("allocations from stack") {
            let (b, n) = head
                .split_once("bytes in")
                .ok_or_else(|| format!("`{line}` is not `N bytes in M allocations`"))?;
            bytes += integer(b)?;
            allocations += integer(n)?;
        }
    }
    Ok(vec![
        ("bytes".into(), bytes),
        ("allocations".into(), allocations),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/reference/captures/heap")
            .join(name);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    fn get(values: &Values, key: &str) -> f64 {
        values
            .iter()
            .find(|(k, _)| k == key)
            .unwrap_or_else(|| panic!("no `{key}` in {values:?}"))
            .1
    }

    #[test]
    fn dhat_reads_its_three_summary_lines() {
        let v = dhat_summary("", &capture("dhat.stderr")).unwrap();
        assert_eq!(get(&v, "total_bytes"), 73_637_188.0);
        assert_eq!(get(&v, "total_blocks"), 3_180.0);
        assert_eq!(get(&v, "gmax_bytes"), 68_908_064.0);
        assert_eq!(get(&v, "gmax_blocks"), 1_921.0);
        assert_eq!(get(&v, "end_bytes"), 4_605_465.0);
        assert_eq!(get(&v, "end_blocks"), 52.0);
    }

    #[test]
    fn dhat_without_its_summary_is_an_error_not_zero() {
        assert!(dhat_summary("", "==1== DHAT, a dynamic heap analysis tool\n").is_err());
    }

    #[test]
    fn massif_reads_the_snapshot_marked_peak() {
        let v = massif_peak(&capture("massif.out"), "").unwrap();
        assert_eq!(get(&v, "peak"), 68_908_064.0);
        assert_eq!(get(&v, "max"), 68_908_064.0);
        assert!(get(&v, "peak_extra") > 0.0);
    }

    #[test]
    fn massif_without_a_peak_is_an_error() {
        let out = "snapshot=0\nmem_heap_B=10\nmem_heap_extra_B=0\nheap_tree=empty\n";
        assert!(massif_peak(out, "").is_err());
        assert!(massif_peak("", "").is_err());
    }

    #[test]
    fn heaptrack_reads_its_closing_summary() {
        let v = heaptrack_summary(&capture("heaptrack_print.txt"), "").unwrap();
        assert_eq!(get(&v, "calls"), 3_180.0);
        assert_eq!(get(&v, "temporary"), 613.0);
        assert_eq!(get(&v, "peak_bytes"), 68_980_000.0);
        assert_eq!(get(&v, "leaked_bytes"), 4_610_000.0);
    }

    #[test]
    fn heaptrack_sizes_in_every_unit() {
        assert_eq!(heaptrack_size("0B").unwrap(), 0.0);
        assert_eq!(heaptrack_size("256B").unwrap(), 256.0);
        assert_eq!(heaptrack_size("4.61K").unwrap(), 4_610.0);
        assert_eq!(heaptrack_size("1.50G").unwrap(), 1.5e9);
        assert!(heaptrack_size("lots").is_err());
    }

    #[test]
    fn memleak_sums_every_stack_of_its_last_report() {
        // A format fixture, not a capture: written by hand from libbpf-tools
        // memleak's format strings, with synthetic numbers that agree with
        // no other tool. It checks the parser, never the pair; the pair
        // needs a privileged run (`memleak.txt`, docs/fixes/w22-heap.md).
        let v = memleak_outstanding(&capture("memleak.format.txt"), "").unwrap();
        assert_eq!(get(&v, "bytes"), 321.0);
        assert_eq!(get(&v, "allocations"), 6.0);
        let two = "[1] Top 9 stacks with outstanding allocations:\n\t5 bytes in 1 allocations from stack\n\
                   [2] Top 9 stacks with outstanding allocations:\n\t7 bytes in 2 allocations from stack\n";
        let v = memleak_outstanding(two, "").unwrap();
        assert_eq!(get(&v, "bytes"), 7.0);
        assert!(memleak_outstanding("Attaching to pid 1\n", "").is_err());
    }

    #[test]
    fn memleak_reads_its_real_report() {
        // libbpf-tools' memleak through `sudo -n`, the pair's own command over
        // `blocks.py 64 1048576 4 0.3` (2026-10-05): one report after the
        // program exited, 368 stacks. More than dhat's end because memleak
        // also counts mmap'd regions (the pair's `gap`): the first two stacks
        // are 3064432 bytes each.
        let v = memleak_outstanding(&capture("memleak.txt"), "").unwrap();
        assert_eq!(get(&v, "bytes"), 7726413.0);
        assert_eq!(get(&v, "allocations"), 564.0);
    }
}
