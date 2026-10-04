//! Parsers for the `power` family's reference tools. Each turns a tool's
//! output into the values a pair compares (BRIEF.md, "Decided before this
//! wave").
//!
//! - `power_perf_stat_energy`: `perf stat -x, -e power/energy-pkg/`, the
//!   package's joules under the key `""`.
//! - `power_turbostat_summary`: `turbostat --Summary`'s one line, keyed by
//!   column (`Pkg_J`, `PkgWatt`, `Bzy_MHz`, `C2%`, ...), every `*_MHz` also
//!   as `*_Hz` (omn's unit), plus `seconds`, the interval turbostat measured.
//! - `power_cpupower_monitor`: `cpupower monitor`'s per-cpu table, keyed
//!   `cpu<N>/<column>` and `<column>` for the mean across cpus (Mperf's
//!   `Freq`, in MHz, also as `cpu<N>/Freq_Hz`); plus, when
//!   it ran a command, `seconds`, and under `Idle_Stats` alone `cpu:<N>`,
//!   the cpu's idle residency in nanoseconds over that command (all states
//!   summed), which is what `power.cstate`'s one-tick summary reports.
//! - `power_rapl_window`: the two `energy_uj` readings
//!   `workloads/power/rapl_window.sh` prints, as joules under `""`.
//! - `power_sensors_json`: `sensors -j`, keyed `<chip>/<label>` in degrees,
//!   plus `max_cpu`, the hottest processor sensor (what `power.temp` reports).

use crate::common::{Parser, Values};

pub fn parsers() -> Vec<Parser> {
    vec![
        Parser {
            name: "power_perf_stat_energy",
            parse: perf_stat_energy,
        },
        Parser {
            name: "power_turbostat_summary",
            parse: turbostat_summary,
        },
        Parser {
            name: "power_cpupower_monitor",
            parse: cpupower_monitor,
        },
        Parser {
            name: "power_rapl_window",
            parse: rapl_window,
        },
        Parser {
            name: "power_sensors_json",
            parse: sensors_json,
        },
    ]
}

/// `perf stat -x,` writes one line per event to stderr:
/// `value,unit,event,run time,share,...`. The energy events are summed when
/// several packages each report one.
fn perf_stat_energy(_stdout: &str, stderr: &str) -> Result<Values, String> {
    let mut total = None;
    for line in stderr.lines() {
        let fields: Vec<&str> = line.split(',').collect();
        if fields.len() < 3 || !fields[2].starts_with("power/energy-pkg") {
            continue;
        }
        let value: f64 = fields[0].parse().map_err(|_| {
            format!(
                "perf stat could not count {}: `{}` (paranoid setting, or no RAPL PMU)",
                fields[2], fields[0]
            )
        })?;
        if fields[1] != "Joules" {
            return Err(format!("energy-pkg in `{}`, expected Joules", fields[1]));
        }
        *total.get_or_insert(0.0) += value;
    }
    total
        .map(|joules| vec![(String::new(), joules)])
        .ok_or_else(|| "no power/energy-pkg/ line in perf stat's output".to_string())
}

/// turbostat prints `<seconds> sec`, then a tab-separated header and one
/// summary line under `--Summary`. Both go to stderr when it forks a command;
/// stdout is read too, for `--out /dev/stdout` and the interval mode.
fn turbostat_summary(stdout: &str, stderr: &str) -> Result<Values, String> {
    let text = format!("{stderr}\n{stdout}");
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    let mut values = Values::new();
    if let Some(secs) = lines.iter().find_map(|l| l.strip_suffix(" sec")) {
        let secs: f64 = secs
            .trim()
            .parse()
            .map_err(|_| format!("turbostat's interval `{secs}` is not a number"))?;
        values.push(("seconds".to_string(), secs));
    }
    // The header is the last line whose columns are names and whose next line
    // has as many numbers: turbostat repeats the header in interval mode, and
    // the last summary is the one covering the whole command.
    let summary = lines.windows(2).rev().find_map(|pair| {
        let names: Vec<&str> = pair[0].split('\t').map(str::trim).collect();
        let nums: Vec<f64> = pair[1]
            .split('\t')
            .map(|v| v.trim().parse::<f64>())
            .collect::<Result<_, _>>()
            .ok()?;
        let header = names.len() == nums.len()
            && names
                .iter()
                .all(|n| n.parse::<f64>().is_err() && !n.is_empty());
        header.then_some((names, nums))
    });
    let (names, nums) = summary.ok_or_else(|| {
        let why = lines
            .iter()
            .find(|l| l.contains("root") || l.contains("Permission"))
            .copied()
            .unwrap_or("no header and summary line");
        format!(
            "turbostat printed no summary: {why} (with --no-msr it reads through cpu-wide \
             perf events, which need perf_event_paranoid at most 0 or CAP_PERFMON)"
        )
    })?;
    for (name, v) in names.into_iter().zip(nums) {
        if let Some(stem) = name.strip_suffix("_MHz") {
            values.push((format!("{stem}_Hz"), v * 1e6));
        }
        values.push((name.to_string(), v));
    }
    Ok(values)
}

/// `cpupower monitor` prints a monitor-name line, then ` CPU| <col> | ...`
/// and one row per cpu; several monitors share a row, separated by `||`.
/// Running a command, it first prints `<cmd> took <S> seconds and exited
/// ...`; lines that are neither (a wrapped command's own output) are skipped.
fn cpupower_monitor(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let seconds = stdout.lines().find_map(|l| {
        let (_, rest) = l.split_once(" took ")?;
        rest.split_whitespace().next()?.parse::<f64>().ok()
    });
    let idle_stats_only = stdout
        .lines()
        .take_while(|l| !l.trim_start().starts_with("CPU|"))
        .last()
        .is_some_and(|l| l.trim().trim_start_matches('|').trim() == "Idle_Stats");
    let split = |l: &str| -> Vec<String> {
        l.split('|')
            .map(|f| f.trim().to_string())
            .filter(|f| !f.is_empty())
            .collect()
    };
    let mut lines = stdout.lines();
    let header = lines
        .find(|l| l.trim_start().starts_with("CPU|"))
        .ok_or("no ` CPU|` header in cpupower monitor's output")?;
    let columns = split(header);
    let mut values = Values::new();
    let mut sums = vec![0.0; columns.len()];
    let mut rows = 0usize;
    for line in lines {
        let fields = split(line);
        let Some(cpu) = fields.first().and_then(|c| c.parse::<u32>().ok()) else {
            continue;
        };
        if fields.len() != columns.len() {
            return Err(format!(
                "cpu {cpu}: {} fields under {} columns",
                fields.len(),
                columns.len()
            ));
        }
        let mut idle_percent = 0.0;
        for (i, (name, field)) in columns.iter().zip(&fields).enumerate().skip(1) {
            let v: f64 = field
                .parse()
                .map_err(|_| format!("cpu {cpu}, {name}: `{field}` is not a number"))?;
            values.push((format!("cpu{cpu}/{name}"), v));
            if name == "Freq" {
                values.push((format!("cpu{cpu}/Freq_Hz"), v * 1e6));
            }
            sums[i] += v;
            idle_percent += v;
        }
        if let (true, Some(s)) = (idle_stats_only, seconds) {
            values.push((format!("cpu:{cpu}"), idle_percent / 100.0 * s * 1e9));
        }
        rows += 1;
    }
    if rows == 0 {
        return Err("cpupower monitor printed a header and no cpu".to_string());
    }
    for (name, sum) in columns.iter().zip(&sums).skip(1) {
        values.push((name.clone(), sum / rows as f64));
    }
    if let Some(s) = seconds {
        values.push(("seconds".to_string(), s));
    }
    Ok(values)
}

/// `energy_uj_before=A energy_uj_after=B max_energy_range_uj=M`, on stderr
/// so the wrapped command keeps its stdout: the difference in joules,
/// reading a decrease as one wrap at `M`.
fn rapl_window(stdout: &str, stderr: &str) -> Result<Values, String> {
    let line = stderr
        .lines()
        .chain(stdout.lines())
        .find(|l| l.starts_with("energy_uj_before="))
        .ok_or_else(|| {
            let why = stderr.lines().next().unwrap_or("no reading");
            format!("the RAPL files were not read: {why}")
        })?;
    let field = |key: &str| -> Result<f64, String> {
        line.split_whitespace()
            .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
            .ok_or_else(|| format!("no {key} in `{line}`"))?
            .parse()
            .map_err(|_| format!("{key} is not a number in `{line}`"))
    };
    let (before, after, max) = (
        field("energy_uj_before")?,
        field("energy_uj_after")?,
        field("max_energy_range_uj")?,
    );
    let delta = if after >= before {
        after - before
    } else {
        max - before + after
    };
    Ok(vec![(String::new(), delta / 1e6)])
}

/// `sensors -j`: `{chip: {"Adapter": .., label: {"tempN_input": v, ..}}}`.
/// Only `temp*_input` fields are temperatures.
fn sensors_json(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let root: serde_json::Value =
        serde_json::from_str(stdout).map_err(|e| format!("sensors -j: {e}"))?;
    let chips = root.as_object().ok_or("sensors -j: not an object")?;
    let mut values = Values::new();
    let mut max_cpu: Option<f64> = None;
    for (chip, labels) in chips {
        let processor = ["k10temp", "coretemp", "zenpower", "cpu_thermal"]
            .iter()
            .any(|p| chip.starts_with(p));
        for (label, fields) in labels.as_object().into_iter().flatten() {
            for (field, v) in fields.as_object().into_iter().flatten() {
                let is_temp = field.starts_with("temp") && field.ends_with("_input");
                let Some(v) = v.as_f64().filter(|_| is_temp) else {
                    continue;
                };
                values.push((format!("{chip}/{label}"), v));
                if processor {
                    max_cpu = Some(max_cpu.map_or(v, |m: f64| m.max(v)));
                }
            }
        }
    }
    if let Some(m) = max_cpu {
        values.push(("max_cpu".to_string(), m));
    }
    if values.is_empty() {
        return Err("sensors -j reported no temperature".to_string());
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIR: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/reference/captures/power"
    );

    fn capture(name: &str) -> String {
        std::fs::read_to_string(format!("{DIR}/{name}"))
            .unwrap_or_else(|e| panic!("capture {name}: {e}"))
    }

    fn get(values: &Values, key: &str) -> f64 {
        values
            .iter()
            .find(|(k, _)| k == key)
            .unwrap_or_else(|| panic!("no key `{key}` in {values:?}"))
            .1
    }

    #[test]
    fn perf_stat_energy_reads_the_package_joules() {
        let v = perf_stat_energy("", &capture("perf_stat_energy.csv")).unwrap();
        assert_eq!(v, vec![(String::new(), 149.37)]);
    }

    #[test]
    fn perf_stat_energy_refuses_a_counter_it_could_not_read() {
        let err = perf_stat_energy("", "<not supported>,Joules,power/energy-pkg/,0,100.00,,\n")
            .unwrap_err();
        assert!(err.contains("could not count"), "{err}");
        assert!(perf_stat_energy("", "").is_err());
    }

    #[test]
    fn turbostat_summary_is_keyed_by_column() {
        let v = turbostat_summary(
            &capture("turbostat_summary.stdout"),
            &capture("turbostat_summary.stderr"),
        )
        .unwrap();
        assert_eq!(get(&v, "seconds"), 1.012169);
        assert_eq!(get(&v, "Pkg_J"), 146.24);
        assert_eq!(get(&v, "Bzy_MHz"), 4078.0);
        assert_eq!(get(&v, "Bzy_Hz"), 4.078e9);
        assert_eq!(get(&v, "C2%"), 0.07);
    }

    #[test]
    fn turbostat_without_a_summary_says_why() {
        let err = turbostat_summary("", "turbostat: must be root\n").unwrap_err();
        assert!(err.contains("must be root"), "{err}");
    }

    #[test]
    fn cpupower_idle_stats_gives_each_cpu_and_the_mean() {
        let v = cpupower_monitor(&capture("cpupower_idle_stats.txt"), "").unwrap();
        assert_eq!(get(&v, "cpu0/C2"), 1.57);
        assert_eq!(get(&v, "cpu12/POLL"), 0.44);
        let per_cpu: Vec<f64> = v
            .iter()
            .filter(|(k, _)| k.starts_with("cpu") && k.ends_with("/C2"))
            .map(|(_, x)| *x)
            .collect();
        assert_eq!(per_cpu.len(), 24);
        let mean = per_cpu.iter().sum::<f64>() / 24.0;
        assert!((get(&v, "C2") - mean).abs() < 1e-9);
        // `sleep 1` took 1.00451 s; cpu 0 sat 0.32 + 0.65 + 1.57 % of it idle.
        assert_eq!(get(&v, "seconds"), 1.00451);
        let ns = get(&v, "cpu:0");
        assert!((ns - 0.0254 * 1.00451e9).abs() < 1.0, "{ns}");
    }

    #[test]
    fn cpupower_skips_the_wrapped_commands_output() {
        let wrapped = format!(
            "{{\n  \"results\": []\n}}\n{}",
            capture("cpupower_idle_stats.txt")
        );
        let v = cpupower_monitor(&wrapped, "").unwrap();
        assert_eq!(get(&v, "cpu0/C2"), 1.57);
    }

    #[test]
    fn cpupower_mperf_reads_the_frequency_column() {
        let v = cpupower_monitor(&capture("cpupower_mperf.txt"), "").unwrap();
        assert_eq!(get(&v, "cpu1/Freq"), 4071.0);
        assert_eq!(get(&v, "cpu1/Freq_Hz"), 4.071e9);
        assert!(v.iter().all(|(k, _)| !k.starts_with("cpu:")), "{v:?}");
        assert_eq!(get(&v, "Freq"), (4078.0 + 4071.0) / 2.0);
    }

    #[test]
    fn rapl_window_converts_microjoules_and_one_wrap() {
        let v = rapl_window("{}", &capture("rapl_window.txt")).unwrap();
        assert_eq!(v, vec![(String::new(), 90.0)]);
        let v = rapl_window(&capture("rapl_window_wrapped.txt"), "").unwrap();
        let joules = v[0].1;
        assert!((joules - 87.734443).abs() < 1e-6, "{joules}");
        let err = rapl_window("", "cat: energy_uj: Permission denied\n").unwrap_err();
        assert!(err.contains("Permission denied"), "{err}");
    }

    #[test]
    fn sensors_json_finds_the_hottest_processor_sensor() {
        let v = sensors_json(&capture("sensors.json"), "").unwrap();
        assert_eq!(get(&v, "max_cpu"), 71.25);
        assert_eq!(get(&v, "k10temp-pci-00c3/Tccd1"), 66.0);
        assert_eq!(get(&v, "nvme-pci-0100/Composite"), 35.85);
        assert!(v.iter().all(|(k, _)| !k.contains("in0")));
    }
}
