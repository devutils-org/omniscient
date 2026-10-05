//! `-r N`: one request run N times, one after another, and each value's mean
//! and spread across the runs, as `perf stat -r N` reports them.
//!
//! The values are the `summary` emit's: per series, its count, min, max,
//! mean and last, and whatever else a summary carries. A summary is what a
//! repetition can be read against -- one number per series per run -- where
//! a histogram or a timeline is a shape, and a mean of shapes is not one of
//! them. So a repeated run that asks for no summary is refused before it
//! starts, rather than run N times to report nothing.
//!
//! Every run's own results are kept beside the spread, unchanged: a spread
//! hides which run was the odd one, and the runs say.

use serde::Serialize;

/// One value's spread across the runs.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Spread {
    /// The request's label, as its results carry it.
    pub label: String,
    pub source: String,
    /// The series' key, as the summary names it.
    pub key: String,
    /// The summary's field: `mean`, `count`, `max`, ...
    pub field: String,
    /// How many runs had this series. Fewer than the runs when a series is
    /// missing from some of them -- a pid that did not exist every time --
    /// and the mean is then of those alone.
    pub runs: usize,
    pub mean: f64,
    /// The sample standard deviation across the runs (n - 1); zero for one.
    pub stddev: f64,
    /// The standard deviation of the mean, relative to it, in percent: what
    /// `perf stat -r` prints after `+-`. Absent when the mean is zero.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rel_stderr_percent: Option<f64>,
    pub min: f64,
    pub max: f64,
}

/// The document a repeated run writes: the spread first, then every run.
#[derive(Debug, Serialize)]
pub struct Repeated<'a> {
    pub repeat: u32,
    pub spread: &'a [Spread],
    pub runs: &'a [omn::encode::RunOutput],
}

/// Why a repetition cannot mean anything here, if it cannot.
pub fn refusal(session: &omn::request::Session, repeat: u32) -> Option<String> {
    if repeat == 0 {
        return Some("-r 0 runs nothing: ask for one run or more".to_owned());
    }
    if session.record.is_some() {
        return Some(
            "-r repeats a run with one end, and a recording ends when its trigger has written \
             its documents: one or the other"
                .to_owned(),
        );
    }
    let summarized = session
        .requests
        .iter()
        .any(|request| request.emit.contains(&omn::request::Emit::Summary));
    (repeat > 1 && !summarized).then(|| {
        "-r reports each summary value's mean and spread across the runs, and no request asks \
         for a summary: add `-e summary`"
            .to_owned()
    })
}

/// Each summary value's spread, from the runs' results in order.
///
/// `launched` names the requests whose program each run launched anew: a
/// series keyed by one of its tasks is keyed by a new pid every run, so the
/// pid is replaced by its rank among the run's task keys -- `task:#1` is the
/// lowest pid, the program itself when it started the others -- and the
/// runs' series line up.
pub fn spread(runs: &[omn::encode::RunOutput], launched: &[String]) -> Vec<Spread> {
    // (label, source, key, field) -> values, in first-seen order.
    let mut seen: Vec<(Series, Vec<f64>)> = Vec::new();
    for run in runs {
        let Ok(value) = serde_json::to_value(run) else {
            continue;
        };
        let results = value["results"].as_array().cloned().unwrap_or_default();
        for result in &results {
            let label = result["label"].as_str().unwrap_or_default().to_owned();
            let source = result["source"].as_str().unwrap_or_default().to_owned();
            for output in result["outputs"].as_array().into_iter().flatten() {
                if output["emit"].as_str() != Some("summary") {
                    continue;
                }
                let all = output["series"].as_array().cloned().unwrap_or_default();
                let key_of = |series: &serde_json::Value| {
                    series["key"]
                        .as_str()
                        .map_or_else(|| series["key"].to_string(), str::to_owned)
                };
                let mut tasks: Vec<i64> = all
                    .iter()
                    .filter_map(|series| task_pid(&key_of(series)))
                    .collect();
                tasks.sort_unstable();
                for series in &all {
                    let mut key = key_of(series);
                    if launched.contains(&label) {
                        if let Some(rank) = task_pid(&key)
                            .and_then(|pid| tasks.iter().position(|known| *known == pid))
                        {
                            key = format!("task:#{}", rank + 1);
                        }
                    }
                    let Some(fields) = series.as_object() else {
                        continue;
                    };
                    for (field, number) in fields {
                        let Some(number) = number.as_f64() else {
                            continue;
                        };
                        let id = (label.clone(), source.clone(), key.clone(), field.clone());
                        match seen.iter_mut().find(|(known, _)| *known == id) {
                            Some((_, values)) => values.push(number),
                            None => seen.push((id, vec![number])),
                        }
                    }
                }
            }
        }
    }
    seen.into_iter()
        .map(|((label, source, key, field), values)| {
            let n = values.len() as f64;
            let mean = values.iter().sum::<f64>() / n;
            let stddev = if values.len() > 1 {
                (values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0)).sqrt()
            } else {
                0.0
            };
            Spread {
                label,
                source,
                key,
                field,
                runs: values.len(),
                mean,
                stddev,
                rel_stderr_percent: (mean != 0.0).then(|| 100.0 * stddev / n.sqrt() / mean.abs()),
                min: values.iter().copied().fold(f64::INFINITY, f64::min),
                max: values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            }
        })
        .collect()
}

/// The spread as a terminal reads it: one line per series' mean, the other
/// fields being in the structured formats.
pub fn text(repeat: u32, spread: &[Spread]) -> String {
    use std::fmt::Write;
    let mut out = format!(
        "over {repeat} runs of the same session, each value's mean +- its spread (a launched \
         program's tasks numbered by pid within each run):\n"
    );
    for one in spread.iter().filter(|one| one.field == "mean") {
        let rel = one
            .rel_stderr_percent
            .map_or_else(String::new, |rel| format!("  ( +- {rel:.2} % )"));
        let missing = if one.runs < repeat as usize {
            format!("  in {} of the {repeat} runs", one.runs)
        } else {
            String::new()
        };
        let _ = writeln!(
            out,
            "  {} {}  mean {}  stddev {}{rel}  min {}  max {}{missing}",
            one.label,
            one.key,
            number(one.mean),
            number(one.stddev),
            number(one.min),
            number(one.max),
        );
    }
    out
}

/// One value of one series: its request's label and source, its key, and
/// the summary's field.
type Series = (String, String, String, String);

/// The pid of a series keyed `task:<pid>`.
fn task_pid(key: &str) -> Option<i64> {
    key.strip_prefix("task:")?.parse().ok()
}

fn number(value: f64) -> String {
    if value.abs() >= 1e3 || value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.4}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(mean: f64) -> omn::encode::RunOutput {
        serde_json::from_value(serde_json::json!({
            "results": [{
                "label": "cpu.instructions",
                "source": "cpu.instructions",
                "unit": "count",
                "backend": "pmu",
                "outputs": [{"emit": "summary", "series": [
                    {"key": "task:1", "count": 1, "min": mean, "max": mean, "mean": mean, "last": mean}
                ]}],
                "events_seen": 1,
                "observed_ns": 1,
                "stopped_because": "x",
                "losses": {"dropped": 0, "failed_reads": 0, "late_ticks": 0, "undecoded": 0, "throttled": 0}
            }]
        }))
        .expect("a run output")
    }

    #[test]
    fn the_spread_is_the_mean_and_sample_deviation_across_runs() {
        let runs = [run(10.0), run(12.0), run(14.0)];
        let spread = spread(&runs, &["cpu.instructions".to_owned()]);
        let mean = spread
            .iter()
            .find(|one| one.field == "mean")
            .expect("the mean's spread");
        assert_eq!(mean.runs, 3);
        assert!((mean.mean - 12.0).abs() < 1e-9);
        assert!((mean.stddev - 2.0).abs() < 1e-9, "{}", mean.stddev);
        assert_eq!((mean.min, mean.max), (10.0, 14.0));
        let rel = mean.rel_stderr_percent.expect("a relative error");
        assert!((rel - 100.0 * 2.0 / 3f64.sqrt() / 12.0).abs() < 1e-9);
        assert!(text(3, &spread).contains("cpu.instructions task:#1  mean 12"));
    }
}
