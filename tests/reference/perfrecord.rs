//! Parsers for the `perfrecord` family's reference tools. Each turns a tool's
//! output into the values a pair compares (BRIEF.md, "Decided before this
//! wave").
//!
//! The tools here are `perf record`/`report`, `perf top`, `perf mem` and
//! `perf c2c`. Their omn counterparts do not print one value at a JSON path:
//! a profile's top functions are its stacks' leaves summed, a latency mean is
//! a histogram's sum over its count, a line's share of the HITMs is a top-k
//! row over the run's total. So this module also parses omn's side, under
//! names a pair gives as `compare.omn_parse`.
//!
//! A function's key is its symbol where the tool names it, and its object's
//! name where it prints an address (`libcrypto.so.3` for any unnamed address
//! in it), so a stripped library is compared as one bucket on both sides.

use crate::common::{Parser, Values};
use serde_json::Value;

pub fn parsers() -> Vec<Parser> {
    vec![
        Parser {
            name: "perf_report_self",
            parse: perf_report_self,
        },
        Parser {
            name: "perf_top_self",
            parse: perf_top_self,
        },
        Parser {
            name: "perf_script_weight_mean",
            parse: perf_script_weight_mean,
        },
        Parser {
            name: "perf_c2c_hitm_share",
            parse: perf_c2c_hitm_share,
        },
        Parser {
            name: "omn_profile_self",
            parse: omn_profile_self,
        },
        Parser {
            name: "omn_hist_mean",
            parse: omn_hist_mean,
        },
        Parser {
            name: "omn_topk_share",
            parse: omn_topk_share,
        },
    ]
}

fn add(values: &mut Values, key: &str, value: f64) {
    match values.iter_mut().find(|(k, _)| k == key) {
        Some((_, v)) => *v += value,
        None => values.push((key.to_string(), value)),
    }
}

fn percent(text: &str) -> Option<f64> {
    text.strip_suffix('%')?.parse::<f64>().ok()
}

/// The key a function is compared under: its name, or, when the tool printed
/// an address (`0x...`, `lib.so+0x...`), the object it lies in.
fn function_key(object: &str, symbol: &str) -> String {
    let symbol = symbol.trim().trim_end_matches(" [recovered]").trim();
    if symbol.starts_with("0x") {
        return object.trim().to_string();
    }
    match symbol.rsplit_once("+0x") {
        Some((object, _)) => object.to_string(),
        None => symbol.to_string(),
    }
}

/// `perf report --stdio --no-children -g none --sort dso,sym`: each row's
/// self overhead, as a share of the whole profile (0..1).
fn perf_report_self(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let mut values = Values::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let Some((share, rest)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let Some(share) = percent(share) else {
            continue;
        };
        // `<dso>  [.] <symbol>`, the dso padded with blanks; a kernel row
        // reads `[kernel.kallsyms]  [k] <symbol>`.
        let rest = rest.trim();
        let (object, symbol) = ["[.]", "[k]", "[g]", "[u]", "[H]"]
            .iter()
            .find_map(|marker| rest.split_once(&format!(" {marker} ")))
            .ok_or_else(|| format!("no `[.]`/`[k]` column in `{line}`"))?;
        add(&mut values, &function_key(object, symbol), share / 100.0);
    }
    if values.is_empty() {
        return Err("no row with an overhead in perf report's output".into());
    }
    Ok(values)
}

/// `perf top --stdio -s dso,sym --no-children`: the last screen it drew,
/// read like `perf report`'s rows.
fn perf_top_self(stdout: &str, stderr: &str) -> Result<Values, String> {
    let has_row = |screen: &&str| {
        screen
            .lines()
            .any(|l| l.split_whitespace().next().and_then(percent).is_some())
    };
    let last = stdout
        .split("PerfTop:")
        .filter(has_row)
        .last()
        .ok_or("perf top drew no screen with a sampled row")?;
    // The screen's header line has no leading percentage, so it is no row.
    perf_report_self(last, stderr)
}

/// `perf script -F weight` on a `perf mem record`: the mean weight (the
/// load's latency in core cycles) over the loads the unit timed. A zero is a
/// sample with no timed load, which `perf mem report` weighs as nothing.
fn perf_script_weight_mean(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let (mut count, mut sum) = (0u64, 0f64);
    for line in stdout.lines() {
        let Some(field) = line.split_whitespace().next() else {
            continue;
        };
        let weight: f64 = field
            .parse()
            .map_err(|_| format!("`{field}` is not a weight"))?;
        if weight > 0.0 {
            count += 1;
            sum += weight;
        }
    }
    if count == 0 {
        return Err("no timed load in perf script's output".into());
    }
    Ok(vec![(String::new(), sum / count as f64)])
}

/// `perf c2c report --stdio`, its "Shared Data Cache Line Table": each
/// line's share of the HITMs (0..1) under its address, and `top` for the
/// line ranked first.
fn perf_c2c_hitm_share(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let table = stdout
        .split_once("Shared Data Cache Line Table")
        .ok_or("no Shared Data Cache Line Table in perf c2c's output")?
        .1;
    let mut values = Values::new();
    for line in table.lines() {
        if line.contains("Shared Cache Line Distribution") {
            break;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        // Index, address, node, PA count, Tot Hitm share, ...
        if fields.len() < 5 || fields[0].parse::<u64>().is_err() {
            continue;
        }
        let share = percent(fields[4])
            .ok_or_else(|| format!("`{}` is not a share in `{line}`", fields[4]))?
            / 100.0;
        let address = u64::from_str_radix(fields[1].trim_start_matches("0x"), 16)
            .map_err(|_| format!("`{}` is not an address", fields[1]))?;
        if values.is_empty() {
            values.push(("top".into(), share));
        }
        add(&mut values, &format!("{address:#x}"), share);
    }
    if values.is_empty() {
        return Err("perf c2c found no shared cache line".into());
    }
    Ok(values)
}

fn omn_result(stdout: &str) -> Result<Value, String> {
    let doc: Value =
        serde_json::from_str(stdout).map_err(|e| format!("omn's output is not JSON: {e}"))?;
    doc.pointer("/results/0")
        .cloned()
        .ok_or_else(|| "omn's output has no result".into())
}

fn emitted<'a>(result: &'a Value, emit: &str) -> Result<&'a Value, String> {
    result["outputs"]
        .as_array()
        .and_then(|outputs| outputs.iter().find(|o| o["emit"] == emit))
        .ok_or_else(|| format!("omn's result has no `{emit}` output"))
}

/// `omn trace cpu.profile -e stack --format json`: each stack's events added
/// to its leaf, as a share of every event the profile counted (attributed or
/// not), keyed like `perf report`'s rows.
fn omn_profile_self(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let result = omn_result(stdout)?;
    let output = emitted(&result, "stack")?;
    let mut values = Values::new();
    let mut total = output["unattributed"].as_f64().unwrap_or(0.0);
    for row in output["stacks"].as_array().ok_or("no stacks")? {
        let events = row["events"].as_f64().ok_or("a stack without events")?;
        total += events;
        let id = row["stack"].to_string();
        let leaf = result["stacks"][&id]
            .as_array()
            .and_then(|frames| frames.last())
            .and_then(Value::as_str)
            .ok_or_else(|| format!("stack {id} has no frames"))?;
        add(&mut values, &function_key("", leaf), events);
    }
    if total == 0.0 {
        return Err("omn's profile counted no event".into());
    }
    for (_, v) in values.iter_mut() {
        *v /= total;
    }
    Ok(values)
}

/// `omn trace <source> -e hist --format json`: the mean of every value the
/// histogram holds, across its series.
fn omn_hist_mean(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let result = omn_result(stdout)?;
    let output = emitted(&result, "hist")?;
    let (mut count, mut sum) = (0f64, 0f64);
    for series in output["series"].as_array().ok_or("no series")? {
        count += series["count"].as_f64().unwrap_or(0.0);
        sum += series["sum"].as_f64().unwrap_or(0.0);
    }
    if count == 0.0 {
        return Err("omn's histogram holds no value".into());
    }
    Ok(vec![(String::new(), sum / count)])
}

/// `omn trace <source> -e topk --format json`: each row's share of the
/// events (the rows and the rest), under its key -- a cache line's
/// `mem:[0x4040c0,0x404100)` is keyed by its start, `0x4040c0` -- and `top`
/// for the row ranked first.
fn omn_topk_share(stdout: &str, _stderr: &str) -> Result<Values, String> {
    let result = omn_result(stdout)?;
    let output = emitted(&result, "topk")?;
    let rows = output["series"].as_array().ok_or("no series")?;
    let total: f64 = rows
        .iter()
        .map(|r| r["events"].as_f64().unwrap_or(0.0))
        .sum::<f64>()
        + output["rest"]["events"].as_f64().unwrap_or(0.0);
    if total == 0.0 {
        return Err("omn's top-k counted no event".into());
    }
    let mut values = Values::new();
    for row in rows {
        let key = row["key"].as_str().unwrap_or("");
        let key = key
            .strip_prefix("mem:[")
            .and_then(|k| k.split(',').next())
            .unwrap_or(key);
        let share = row["events"].as_f64().unwrap_or(0.0) / total;
        if values.is_empty() {
            values.push(("top".into(), share));
        }
        add(&mut values, key, share);
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    fn root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    fn capture(name: &str) -> String {
        let path = root()
            .join("tests/reference/captures/perfrecord")
            .join(name);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    fn get(values: &Values, key: &str) -> f64 {
        values
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| *v)
            .unwrap_or_else(|| panic!("no `{key}` in {values:?}"))
    }

    #[test]
    fn perf_report_rows_are_keyed_by_symbol() {
        let v = perf_report_self(&capture("perf-report-hot.txt"), "").unwrap();
        assert!((get(&v, "hot_beta") - 0.4684).abs() < 1e-9);
        assert!((get(&v, "hot_gamma") - 0.3542).abs() < 1e-9);
        assert!((get(&v, "hot_alpha") - 0.1723).abs() < 1e-9);
    }

    #[test]
    fn perf_report_unnamed_addresses_fold_into_their_object() {
        let v = perf_report_self(&capture("perf-report-split.txt"), "").unwrap();
        let crypto = get(&v, "libcrypto.so.3");
        assert!(crypto > 0.4 && crypto < 0.8, "{crypto}");
        assert!(get(&v, "adler32_z") > 0.0);
        assert!(v.iter().all(|(k, _)| !k.starts_with("0x")), "{v:?}");
    }

    #[test]
    fn perf_top_reads_its_last_screen() {
        let v = perf_top_self(&capture("perf-top-hot.txt"), "").unwrap();
        assert!((get(&v, "hot_beta") - 0.4973).abs() < 1e-9);
        assert!((get(&v, "hot_alpha") - 0.2031).abs() < 1e-9);
        assert!((get(&v, "propagate_entity_load_avg") - 0.0006).abs() < 1e-9);
    }

    #[test]
    fn perf_mem_weights_average_over_timed_loads() {
        let v = perf_script_weight_mean(&capture("perf-script-weight-chase.txt"), "").unwrap();
        let mean = get(&v, "");
        assert!(mean > 50.0 && mean < 2000.0, "{mean}");
        assert!(perf_script_weight_mean("0\n0\n", "").is_err());
        assert!(perf_script_weight_mean("x\n", "").is_err());
    }

    #[test]
    fn perf_c2c_lines_are_keyed_by_address() {
        let v = perf_c2c_hitm_share(&capture("perf-c2c-share.txt"), "").unwrap();
        assert!((get(&v, "top") - 0.9964).abs() < 1e-9);
        assert!((get(&v, "0x4040c0") - 0.9964).abs() < 1e-9);
        assert!(perf_c2c_hitm_share("nothing", "").is_err());
    }

    #[test]
    fn omn_profile_leaves_sum_to_the_profile() {
        let v = omn_profile_self(&capture("omn-profile-hot.json"), "").unwrap();
        let sum: f64 = v.iter().map(|(_, s)| s).sum();
        assert!((sum - 1.0).abs() < 1e-9, "{sum}");
        assert!(get(&v, "hot_beta") > get(&v, "hot_gamma"));
        assert!(get(&v, "hot_gamma") > get(&v, "hot_alpha"));
    }

    #[test]
    fn omn_histogram_mean_and_topk_share() {
        let mean = get(
            &omn_hist_mean(&capture("omn-latency-chase.json"), "").unwrap(),
            "",
        );
        assert!(mean > 50.0 && mean < 2000.0, "{mean}");
        let v = omn_topk_share(&capture("omn-sharing-share.json"), "").unwrap();
        assert!(get(&v, "top") > 0.9);
        assert_eq!(get(&v, "top"), get(&v, "0x4040c0"));
    }

    #[test]
    fn function_keys() {
        assert_eq!(function_key("libz.so.1", "0x00000000007c19"), "libz.so.1");
        assert_eq!(
            function_key("", "libcrypto.so.3+0x3f666b"),
            "libcrypto.so.3"
        );
        assert_eq!(function_key("", "main [recovered]"), "main");
        assert_eq!(
            function_key("[kernel.kallsyms]", "psi_account_irqtime"),
            "psi_account_irqtime"
        );
    }

    fn strings(v: &Value) -> Vec<String> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn on_path(binary: &str) -> bool {
        std::env::var_os("PATH")
            .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(binary).is_file()))
            .unwrap_or(false)
    }

    fn run(argv: &[String]) -> (String, String) {
        let out = Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(root())
            .output()
            .unwrap_or_else(|e| panic!("{}: {e}", argv[0]));
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// Every pair in `catalog/references/perfrecord.yaml`, run here with this
    /// module's parsers on both sides, until the shared runner reads
    /// `compare.omn_parse`. Ignored by default: it runs every workload twice
    /// (some 40 s). `cargo test --test reference perfrecord -- --ignored
    /// --nocapture` prints every pair's two values.
    #[test]
    #[ignore]
    fn pairs_hold_on_this_machine() {
        let path = root().join("catalog/references/perfrecord.yaml");
        let pairs: Vec<Value> =
            serde_norway::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let parser = |name: &str| {
            parsers()
                .into_iter()
                .find(|p| p.name == name)
                .unwrap_or_else(|| panic!("no parser `{name}`"))
                .parse
        };
        let omn = root().join("target/debug/omn").display().to_string();
        let mut failed = Vec::new();
        for pair in &pairs {
            let key = pair["compare"]["key"].as_str().unwrap_or("");
            let label = format!("{} vs {} [{key}]", pair["source"], pair["tool"]);
            let missing: Vec<String> = strings(&pair["reference"]["requires"])
                .into_iter()
                .filter(|b| !on_path(b))
                .collect();
            if !missing.is_empty() {
                eprintln!("SKIP {label}: not installed: {missing:?}");
                continue;
            }
            let workload = strings(&pair["workload"]);
            let mut tool = strings(&pair["reference"]["command"]);
            tool.extend(workload.iter().cloned());
            let (out, err) = run(&tool);
            let theirs = parser(pair["reference"]["parse"].as_str().unwrap())(&out, &err);
            let mut ours = vec![omn.clone()];
            ours.extend(strings(&pair["omn"]));
            ours.push("--".into());
            ours.extend(workload.iter().cloned());
            let (out, err) = run(&ours);
            let ours = parser(pair["compare"]["omn_parse"].as_str().unwrap())(&out, &err);
            let (theirs, ours) = match (theirs, ours) {
                (Ok(t), Ok(o)) => (t, o),
                (t, o) => {
                    failed.push(format!("{label}: tool {:?}, omn {:?}", t.err(), o.err()));
                    continue;
                }
            };
            let find = |v: &Values| {
                v.iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, x)| *x)
                    .unwrap_or(0.0)
            };
            let (t, o) = (find(&theirs), find(&ours));
            let tolerance = &pair["compare"]["tolerance"];
            let ok = match (tolerance["abs"].as_f64(), tolerance["rel"].as_f64()) {
                (Some(abs), _) => (t - o).abs() <= abs,
                (None, Some(rel)) => (t - o).abs() <= rel * t.abs(),
                _ => panic!("{label}: no tolerance"),
            };
            let verdict = if ok { "PASS" } else { "FAIL" };
            eprintln!("{verdict} {label}: tool {t:.4}, omn {o:.4}, tolerance {tolerance}");
            if !ok {
                failed.push(label);
            }
        }
        assert!(failed.is_empty(), "{failed:#?}");
    }
}
