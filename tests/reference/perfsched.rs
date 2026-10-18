//! Parsers for the `perfsched` family's reference tools. Each turns a tool's
//! output into the values a pair compares (BRIEF.md, "Decided before this
//! wave").
//!
//! Keys are `<what>.<field>`: a syscall, a command name or a lock type, then
//! the field. Times are always nanoseconds, whatever unit the tool printed,
//! and a table that lists one name several times (one row per process) is
//! summed into one key, so a pair compares a name rather than a pid.

use crate::common::{Parser, Values, INCOMPLETE};
use std::collections::BTreeMap;

pub fn parsers() -> Vec<Parser> {
    vec![
        Parser {
            name: "strace_c",
            parse: strace_c,
        },
        Parser {
            name: "strace_t",
            parse: strace_t,
        },
        Parser {
            name: "perf_trace_summary",
            parse: perf_trace_summary,
        },
        Parser {
            name: "perf_sched_latency",
            parse: perf_sched_latency,
        },
        Parser {
            name: "perf_sched_timehist_rows",
            parse: perf_sched_timehist_rows,
        },
        Parser {
            name: "perf_lock_contention",
            parse: perf_lock_contention,
        },
    ]
}

/// Values keyed and summed in one place, emitted in key order.
#[derive(Default)]
struct Acc(BTreeMap<String, f64>);

impl Acc {
    fn add(&mut self, key: &str, field: &str, v: f64) {
        *self.0.entry(format!("{key}.{field}")).or_insert(0.0) += v;
    }
    fn max(&mut self, key: &str, field: &str, v: f64) {
        let e = self
            .0
            .entry(format!("{key}.{field}"))
            .or_insert(f64::NEG_INFINITY);
        *e = e.max(v);
    }
    fn get(&self, key: &str, field: &str) -> f64 {
        self.0
            .get(&format!("{key}.{field}"))
            .copied()
            .unwrap_or(0.0)
    }
    fn set(&mut self, key: &str, field: &str, v: f64) {
        self.0.insert(format!("{key}.{field}"), v);
    }
    /// The names that carry `field`, to derive a second field from.
    fn names_with(&self, field: &str) -> Vec<String> {
        let suffix = format!(".{field}");
        self.0
            .keys()
            .filter_map(|k| k.strip_suffix(&suffix).map(str::to_owned))
            .collect()
    }
    /// `out = num / den` for every name, 0 where `den` is 0.
    fn ratio(&mut self, num: &str, den: &str, out: &str) {
        for n in self.names_with(den) {
            let d = self.get(&n, den);
            let r = if d > 0.0 { self.get(&n, num) / d } else { 0.0 };
            self.set(&n, out, r);
        }
    }
    fn into_values(self, what: &str) -> Result<Values, String> {
        if self.0.is_empty() {
            return Err(format!("no {what} rows in the tool's output"));
        }
        Ok(self.0.into_iter().collect())
    }
}

fn num(s: &str) -> Result<f64, String> {
    s.trim_end_matches('%')
        .parse::<f64>()
        .map_err(|e| format!("`{s}` is not a number: {e}"))
}

/// A duration printed as a value and its unit (`40.21 ms`, `812 ns`), in
/// nanoseconds.
fn duration_ns(value: &str, unit: &str) -> Result<f64, String> {
    let scale = match unit {
        "ns" => 1.0,
        "us" => 1e3,
        "ms" => 1e6,
        "s" => 1e9,
        u => return Err(format!("unknown time unit `{u}`")),
    };
    Ok(num(value)? * scale)
}

/// `strace -c` (and `-c -w`): the per-syscall table, on stderr, or in the
/// file `-o` wrote and the runner hands over as stdout. Keys `<syscall>.
/// {calls,errors,seconds,usecs_per_call}`, and `total.*` from the footer.
/// With `-w` the seconds are wall time spent in the call; without it they
/// are the system cpu time the call cost, which for a sleep is nearly
/// nothing.
fn strace_c(stdout: &str, stderr: &str) -> Result<Values, String> {
    let text = if stderr.contains("% time") {
        stderr
    } else {
        stdout
    };
    let mut acc = Acc::default();
    let mut in_table = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("% time") {
            in_table = true;
            continue;
        }
        if !in_table || t.is_empty() || t.starts_with("---") {
            continue;
        }
        let f: Vec<&str> = t.split_whitespace().collect();
        // pct seconds usecs/call calls [errors] name
        let (name, errors) = match f.len() {
            5 => (f[4], 0.0),
            6 => (f[5], num(f[4])?),
            _ => return Err(format!("unexpected strace -c row: `{t}`")),
        };
        acc.add(name, "seconds", num(f[1])?);
        acc.add(name, "usecs_per_call", num(f[2])?);
        acc.add(name, "calls", num(f[3])?);
        acc.add(name, "errors", errors);
    }
    acc.into_values("strace -c")
}

/// `strace -T` (with or without `-f`): one line per call ending in
/// `<seconds>`, a call another thread's line split rejoined at its
/// `<... name resumed>`. Keys `<syscall>.{calls,total_ns,mean_ns,max_ns}`.
fn strace_t(stdout: &str, stderr: &str) -> Result<Values, String> {
    let text = if stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    let mut acc = Acc::default();
    for line in text.lines() {
        let t = line.trim_end();
        let Some(open) = t.rfind('<') else { continue };
        if !t.ends_with('>') || t.contains("+++") {
            continue;
        }
        let Ok(secs) = t[open + 1..t.len() - 1].parse::<f64>() else {
            continue;
        };
        // Strip a `-f` pid prefix, then take the name before `(`, or the
        // one a resumed line names.
        let body = t
            .split_once(' ')
            .filter(|(pid, _)| pid.chars().all(|c| c.is_ascii_digit()))
            .map_or(t, |(_, rest)| rest.trim_start());
        let name = match body.strip_prefix("<... ") {
            Some(r) => r.split_whitespace().next().unwrap_or(""),
            None => body.split('(').next().unwrap_or(""),
        };
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let ns = secs * 1e9;
        acc.add(name, "calls", 1.0);
        acc.add(name, "total_ns", ns);
        acc.max(name, "max_ns", ns);
    }
    acc.ratio("total_ns", "calls", "mean_ns");
    acc.into_values("strace -T")
}

/// `perf trace -s` (or `--summary-only`): one table per process, summed by
/// syscall across them. Keys `<syscall>.{calls,errors,total_ns,max_ns,
/// mean_ns}`; `mean_ns` is recomputed from the sums, since perf's own `avg`
/// is per process and printed to the microsecond.
fn perf_trace_summary(stdout: &str, stderr: &str) -> Result<Values, String> {
    let text = if stderr.contains("Summary of events") {
        stderr
    } else {
        stdout
    };
    // `LOST N events!`: perf's ring buffer overflowed, and the summary is
    // over the events that survived (2337 of 5000 getppid calls once under
    // the twenty-fourth wave's load, the row missing entirely another time).
    let lost: u64 = stdout
        .lines()
        .chain(stderr.lines())
        .filter_map(|l| {
            l.trim()
                .strip_prefix("LOST ")?
                .strip_suffix(" events!")?
                .trim()
                .parse::<u64>()
                .ok()
        })
        .sum();
    if lost > 0 {
        return Err(format!(
            "{INCOMPLETE}perf trace lost {lost} events: its summary counts the survivors only"
        ));
    }
    let mut acc = Acc::default();
    let mut in_table = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("syscall") && t.contains("calls") {
            in_table = true;
            continue;
        }
        if t.is_empty() {
            in_table = false;
            continue;
        }
        if !in_table || t.starts_with('(') || t.starts_with("---") {
            continue;
        }
        let f: Vec<&str> = t.split_whitespace().collect();
        // syscall calls errors total min avg max stddev
        if f.len() != 8 {
            return Err(format!("unexpected perf trace -s row: `{t}`"));
        }
        let name = f[0];
        acc.add(name, "calls", num(f[1])?);
        acc.add(name, "errors", num(f[2])?);
        acc.add(name, "total_ns", num(f[3])? * 1e6);
        acc.max(name, "max_ns", num(f[6])? * 1e6);
    }
    acc.ratio("total_ns", "calls", "mean_ns");
    acc.into_values("perf trace -s")
}

/// `perf sched latency`: one row per task, `comm:pid` (or `comm:(N)` when
/// perf merged N of them). Keys `<comm>.{runtime_ns,switches,max_delay_ns,
/// avg_delay_ns}` summed by command, the average weighted by the switches
/// it is over, and `total.{runtime_ns,switches}` from the footer.
fn perf_sched_latency(stdout: &str, _stderr: &str) -> Result<Values, String> {
    // A task is one row per name it ran under, `name:pid`, and which name
    // carries a renamed workload's switches is perf's version: 7.2 files them
    // under the last name, Debian 12's 6.1 under the first (one switch under
    // `omnref-sleep`, a hundred under `python3`, found with distroproof). So
    // every row of a pid that ever named itself `omnref*` counts under that
    // name, whichever perf this is.
    let mut renamed: BTreeMap<&str, &str> = BTreeMap::new();
    for line in stdout.lines() {
        let task = line.split('|').next().unwrap_or_default().trim();
        if let Some((comm, pid)) = task.rsplit_once(':') {
            if comm.starts_with("omnref") {
                renamed.insert(pid, comm);
            }
        }
    }
    let mut acc = Acc::default();
    for line in stdout.lines() {
        let cols: Vec<&str> = line.split('|').map(str::trim).collect();
        if cols.len() < 3 || cols[0] == "Task" {
            continue;
        }
        let runtime = cols[1]
            .strip_suffix("ms")
            .ok_or_else(|| format!("runtime without `ms`: `{line}`"))?
            .trim();
        let switches = num(cols[2])?;
        if cols[0] == "TOTAL:" {
            acc.set("total", "runtime_ns", num(runtime)? * 1e6);
            acc.set("total", "switches", switches);
            continue;
        }
        let comm = match cols[0].rsplit_once(':') {
            Some((comm, pid)) => renamed.get(pid).copied().unwrap_or(comm),
            None => cols[0],
        };
        let field = |i: usize, label: &str| -> Result<f64, String> {
            let v = cols
                .get(i)
                .and_then(|c| c.strip_prefix(label))
                .and_then(|c| c.trim().strip_suffix("ms"))
                .ok_or_else(|| format!("no `{label}` column: `{line}`"))?;
            Ok(num(v.trim())? * 1e6)
        };
        acc.add(comm, "runtime_ns", num(runtime)? * 1e6);
        acc.add(comm, "switches", switches);
        acc.add(comm, "delay_sum_ns", field(3, "avg:")? * switches);
        acc.max(comm, "max_delay_ns", field(4, "max:")?);
    }
    acc.ratio("delay_sum_ns", "switches", "avg_delay_ns");
    acc.0.retain(|k, _| !k.ends_with(".delay_sum_ns"));
    acc.0.remove("total.avg_delay_ns");
    acc.into_values("perf sched latency")
}

/// `perf sched timehist -M`: one row per time a task ran (`time [cpu]
/// comm[tid] wait sch run`), and one `migrated: comm[tid] cpu A => B` line per
/// migration, summed by command. Keys `<comm>.{sched_in,run_ns,migrations}`.
/// Not `-s`: its "Runtime summary" leaves out a task that exits before the
/// recording ends (perf 7.2), which is every workload here.
fn perf_sched_timehist_rows(stdout: &str, _stderr: &str) -> Result<Values, String> {
    fn comm(task: &str) -> &str {
        task.rsplit_once('[').map_or(task, |(c, _)| c)
    }
    let mut acc = Acc::default();
    let mut rows = 0;
    for line in stdout.lines() {
        let t = line.trim();
        // A row starts with its timestamp; the header and rule do not.
        if !t.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        if let Some((_, moved)) = t.split_once("migrated: ") {
            let task = moved.split_whitespace().next().unwrap_or("");
            acc.add(comm(task), "migrations", 1.0);
            rows += 1;
            continue;
        }
        let f: Vec<&str> = t.split_whitespace().collect();
        // time, [cpu], task (no spaces in `comm[tid]` but a comm may hold
        // them), then wait, sch delay and run time.
        if f.len() < 6 {
            return Err(format!("unexpected timehist row: `{t}`"));
        }
        let task = f[2..f.len() - 3].join(" ");
        acc.add(comm(&task), "sched_in", 1.0);
        acc.add(comm(&task), "run_ns", num(f[f.len() - 1])? * 1e6);
        // `sch delay`: from the wakeup (or the preemption) to this switch in.
        acc.add(comm(&task), "delay_ns", num(f[f.len() - 2])? * 1e6);
        rows += 1;
    }
    if rows == 0 {
        return Err("no perf sched timehist rows in the tool's output".into());
    }
    let mut values = acc.into_values("perf sched timehist -M")?;
    // The mean scheduling delay per command, which a sum over a different
    // number of slices cannot be compared by.
    let means: Vec<(String, f64)> = values
        .iter()
        .filter_map(|(key, delay)| {
            let stem = key.strip_suffix(".delay_ns")?;
            let slices = values
                .iter()
                .find(|(k, _)| *k == format!("{stem}.sched_in"))?
                .1;
            (slices > 0.0).then(|| (format!("{stem}.delay_mean_ns"), delay / slices))
        })
        .collect();
    values.extend(means);
    Ok(values)
}

/// `perf lock contention` (tracepoints or `-b`): one row per caller (or per
/// lock with `-l`), its waits printed with their unit. Keys
/// `<type>.{contended,wait_ns}` summed by lock type (`rwsem:W`,
/// `spinlock`, ...), `total.{contended,wait_ns,max_wait_ns}` over every row.
fn perf_lock_contention(stdout: &str, stderr: &str) -> Result<Values, String> {
    let text = if stdout.contains("contended") {
        stdout
    } else {
        stderr
    };
    let mut acc = Acc::default();
    let mut in_table = false;
    let mut per_thread = false;
    for line in text.lines() {
        let t = line.trim();
        // perf appends `=== output for debug ===` (`(BPF)` with `-b`) when
        // it lost or could not pair events; its histogram has a
        // `contended: N` line of its own, so the table ends there.
        if t.starts_with("===") {
            break;
        }
        // `-t` prints one row per thread, `pid comm` where the default prints
        // `type caller`.
        if t.starts_with("contended") && (t.contains("caller") || t.contains("comm")) {
            in_table = true;
            per_thread = !t.contains("caller");
            continue;
        }
        if !in_table || t.is_empty() {
            continue;
        }
        let f: Vec<&str> = t.split_whitespace().collect();
        // contended, total, max and avg each with a unit, type, caller
        if f.len() < 8 {
            return Err(format!("unexpected perf lock contention row: `{t}`"));
        }
        let contended = num(f[0])?;
        let total = duration_ns(f[1], f[2])?;
        let max = duration_ns(f[3], f[4])?;
        // A comm may hold spaces (`Thread-5 (churn`); a type never does.
        let row = if per_thread {
            format!("comm:{}", f[8..].join(" "))
        } else {
            f[7].to_string()
        };
        acc.add(&row, "contended", contended);
        acc.add(&row, "wait_ns", total);
        acc.add("total", "contended", contended);
        acc.add("total", "wait_ns", total);
        acc.max("total", "max_wait_ns", max);
    }
    acc.into_values("perf lock contention")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(name: &str) -> String {
        let path = format!(
            "{}/tests/reference/captures/perfsched/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    fn value(v: &Values, key: &str) -> f64 {
        v.iter()
            .find(|(k, _)| k == key)
            .unwrap_or_else(|| panic!("no `{key}` in {v:?}"))
            .1
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-6 * b.abs().max(1.0)
    }

    #[test]
    fn strace_c_reads_calls_errors_and_the_footer() {
        // The workload: 5000 getppid, 20 sleeps of 10 ms.
        let v = strace_c(&capture("strace-c.txt"), "").unwrap();
        assert_eq!(value(&v, "getppid.calls"), 5000.0);
        assert_eq!(value(&v, "clock_nanosleep.calls"), 20.0);
        assert_eq!(value(&v, "total.calls"), 5375.0);
        assert_eq!(value(&v, "total.errors"), 47.0);
        // Without -w, a sleep costs the cpu almost nothing.
        assert!(value(&v, "clock_nanosleep.usecs_per_call") < 1000.0);
    }

    #[test]
    fn strace_c_w_reports_wall_time_in_the_call() {
        let v = strace_c("", &capture("strace-c-w.txt")).unwrap();
        assert_eq!(value(&v, "clock_nanosleep.calls"), 20.0);
        let per_call = value(&v, "clock_nanosleep.usecs_per_call");
        assert!((9_000.0..12_000.0).contains(&per_call), "{per_call}");
    }

    #[test]
    fn strace_t_sums_per_call_durations() {
        let v = strace_t("", &capture("strace-T.txt")).unwrap();
        assert_eq!(value(&v, "getppid.calls"), 3.0);
        assert_eq!(value(&v, "clock_nanosleep.calls"), 3.0);
        assert!(close(value(&v, "clock_nanosleep.max_ns"), 10_052_000.0));
        let mean = (7_398_000.0 + 10_052_000.0 + 10_021_000.0) / 3.0;
        assert!(close(value(&v, "clock_nanosleep.mean_ns"), mean));
        assert!(!v.iter().any(|(k, _)| k.starts_with("exited")));
    }

    #[test]
    fn strace_t_rejoins_a_resumed_call() {
        let out = "\
101 clock_nanosleep(CLOCK_MONOTONIC, 0, {tv_sec=0, tv_nsec=10000000},  <unfinished ...>
102 getppid()                       = 100 <0.000004>
101 <... clock_nanosleep resumed>NULL) = 0 <0.010100>
";
        let v = strace_t("", out).unwrap();
        assert_eq!(value(&v, "clock_nanosleep.calls"), 1.0);
        assert!(close(value(&v, "clock_nanosleep.total_ns"), 10_100_000.0));
        assert_eq!(value(&v, "getppid.calls"), 1.0);
    }

    #[test]
    fn perf_trace_summary_reads_the_table() {
        // A real `sudo -n perf trace -s -o FILE -- python3 syscalls.py 5000
        // 20 10` (2026-10-05, under the wave's load): two of the 5000 getppid
        // calls are missing from perf's own table, which is why the pair's
        // band lets omn read above only.
        let v = perf_trace_summary("", &capture("perf-trace-s.txt")).unwrap();
        assert_eq!(value(&v, "getppid.calls"), 4998.0);
        assert_eq!(value(&v, "newfstatat.errors"), 19.0);
        assert_eq!(value(&v, "clock_nanosleep.calls"), 20.0);
        assert!(close(value(&v, "clock_nanosleep.total_ns"), 201_399_000.0));
        assert!(close(value(&v, "clock_nanosleep.mean_ns"), 10_069_950.0));
        assert!(close(value(&v, "clock_nanosleep.max_ns"), 10_086_000.0));
    }

    #[test]
    fn perf_trace_summary_refuses_a_run_that_lost_events() {
        let text = format!("LOST 628 events!\n{}", capture("perf-trace-s.txt"));
        let e = perf_trace_summary("", &text).unwrap_err();
        assert!(e.contains("lost 628 events"), "{e}");
        // Which the runner reads as a skip, not as omn's failure.
        assert!(e.starts_with(INCOMPLETE), "{e}");
    }

    #[test]
    fn perf_trace_summary_sums_processes() {
        let one = "\n   syscall            calls  errors  total       min       avg       max       stddev\n   --------------- --------  ------ -------- --------- --------- ---------     ------\n   getppid              10      0     0.010     0.001     0.001     0.002      1.00%\n\n";
        let v = perf_trace_summary(&format!(" Summary of events:\n{one}{one}"), "").unwrap();
        assert_eq!(value(&v, "getppid.calls"), 20.0);
    }

    #[test]
    fn perf_sched_latency_reads_tasks_and_total() {
        // A real `perf sched record -- python3 sleeper.py 100 2`, then `perf
        // sched latency -f` (2026-10-05, through `sudo -n`): the machine's
        // tasks beside the workload's, renamed `omnref-sleep`.
        let v = perf_sched_latency(&capture("perf-sched-latency.txt"), "").unwrap();
        assert_eq!(value(&v, "omnref-sleep.switches"), 102.0);
        assert!(close(value(&v, "omnref-sleep.runtime_ns"), 11_180_000.0));
        assert!(close(value(&v, "omnref-sleep.avg_delay_ns"), 5_000.0));
        assert!(close(value(&v, "omnref-sleep.max_delay_ns"), 12_000.0));
        // A command with a colon in it keeps it; only the pid is cut.
        assert_eq!(value(&v, "kworker/10:1-mm.switches"), 2.0);
        assert_eq!(value(&v, "total.switches"), 949.0);
    }

    /// Debian 12's perf 6.1 files a renamed task's switches under its first
    /// name: the rows of one pid are summed under the workload's name.
    #[test]
    fn perf_sched_latency_sums_a_renamed_task_s_rows_by_pid() {
        let out = "\
 -------------------------------------------------------------------------------------------------------------------------------------------
  Task                  |   Runtime ms  | Switches | Avg delay ms    | Max delay ms    | Max delay start           | Max delay end          |
 -------------------------------------------------------------------------------------------------------------------------------------------
  python3:4242          |      9.000 ms |      100 | avg:    0.005 ms | max:    0.012 ms | max start:   100.000000 s | max end:   100.000012 s
  omnref-sleep:4242     |      1.000 ms |        1 | avg:    0.005 ms | max:    0.004 ms | max start:   101.000000 s | max end:   101.000004 s
  python3:5000          |      2.000 ms |        7 | avg:    0.001 ms | max:    0.001 ms | max start:   102.000000 s | max end:   102.000001 s
 -----------------------------------------------------------------------------------------------------------------
  TOTAL:                |     12.000 ms |      108 |
";
        let v = perf_sched_latency(out, "").unwrap();
        assert_eq!(value(&v, "omnref-sleep.switches"), 101.0);
        assert_eq!(value(&v, "python3.switches"), 7.0);
    }

    #[test]
    fn perf_sched_timehist_rows_counts_runs_and_migrations_by_command() {
        let v = perf_sched_timehist_rows(&capture("perf-sched-timehist-M.txt"), "").unwrap();
        // migrate.py 200, renamed `omnref-migrate`: 200 pins and its exit.
        assert_eq!(value(&v, "omnref-migrate.migrations"), 201.0);
        assert_eq!(value(&v, "omnref-migrate.sched_in"), 202.0);
        assert_eq!(value(&v, "python3.sched_in"), 2.0);
        // A `migrated:` line names the moved task, not the migration thread.
        assert!(v.iter().all(|(k, _)| k != "migration/0.migrations"));
        assert!(value(&v, "omnref-migrate.run_ns") > 0.0);
        // The scheduling delay, summed and as a mean per slice.
        assert!(value(&v, "omnref-migrate.delay_ns") >= 0.0);
        let mean = value(&v, "omnref-migrate.delay_mean_ns");
        assert!(
            (mean - value(&v, "omnref-migrate.delay_ns") / 202.0).abs() < 1e-6,
            "{mean}"
        );
    }

    #[test]
    fn perf_lock_contention_per_thread_sums_by_command() {
        let v = perf_lock_contention(&capture("perf-lock-contention-t.txt"), "").unwrap();
        // contend.py 8 2, every thread named `omnref-lock`, beside the rest of
        // the machine `-- prog` does not leave out.
        assert_eq!(value(&v, "comm:omnref-lock.contended"), 7953.0);
        assert!(value(&v, "total.contended") > 7953.0);
        assert!(value(&v, "comm:omnref-lock.wait_ns") > 0.0);
    }

    #[test]
    fn perf_lock_contention_converts_units_and_sums_by_type() {
        let v = perf_lock_contention(&capture("perf-lock-contention.txt"), "").unwrap();
        assert_eq!(value(&v, "rwsem:W.contended"), 2161.0);
        assert!(close(value(&v, "rwsem:W.wait_ns"), 70_080_000.0));
        assert_eq!(value(&v, "total.contended"), 2202.0);
        assert!(close(value(&v, "total.max_wait_ns"), 1_080_000.0));
        assert!(close(value(&v, "spinlock.wait_ns"), 211_400.0 + 812.0));
    }

    #[test]
    fn perf_lock_contention_stops_at_the_debug_block() {
        // What perf prints after the table when some events were bad:
        // tools/perf/builtin-lock.c, print_bad_events() and its BPF twin.
        let text = capture("perf-lock-contention.txt")
            + "\n=== output for debug ===\n\nbad: 3, total: 2205\nbad rate: 0.14 %\n\
               histogram of events caused bad sequence\n    acquire: 0\n   acquired: 0\n\
               \x20 contended: 3\n    release: 0\n\n=== output for debug (BPF) ===\n\
               total: 2205, bad: 3, bad rate: 0.14 %\n  task: 0\n  stack: 3\n";
        let v = perf_lock_contention(&text, "").unwrap();
        assert_eq!(value(&v, "total.contended"), 2202.0);
    }

    #[test]
    fn an_empty_output_is_an_error_not_zero() {
        for p in parsers() {
            assert!((p.parse)("", "").is_err(), "{}", p.name);
        }
    }
}
