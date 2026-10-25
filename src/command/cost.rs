//! What a run measured itself spending, against the band its backend
//! declares, and the comparison of two backends of one source.

use super::*;

/// What one collection cost, as the record written beside the numbers.
#[derive(Serialize)]
pub(crate) struct MeasuredCost {
    pub(crate) label: String,
    pub(crate) source: String,
    /// How the source was observed. It belongs beside the price because it is
    /// most of what sets it: the same backend polled every millisecond and
    /// every second costs a thousandfold different, and only one of those
    /// numbers can be what a band declared per backend meant.
    pub(crate) mode: String,
    /// The backend that produced the events, which is not the one the resolved
    /// file names when a budget moved the collection mid-run.
    pub(crate) backend: String,
    /// Every backend the collection ran through, in order. Longer than one only
    /// when a budget degraded it part way through the window.
    pub(crate) served_by: Vec<String>,
    /// The band the catalog declares for that backend, so the claim and the
    /// measurement sit side by side.
    pub(crate) declared: &'static str,
    /// The backend's band per regime, when the catalog gives one: which
    /// programs pay `declared` and which pay less.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) declared_by_regime: BTreeMap<String, &'static str>,
    pub(crate) declared_worst_case_percent: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) budget_percent: Option<f64>,
    /// Whether the measurement came in under the ceiling. Absent when there was
    /// no ceiling, or no workload to be a percentage of.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) within_budget: Option<bool>,
    /// What the run cost as a share of the work the target did of its own:
    /// the tool's cpu and what the collector spent inside the target
    /// (`overhead.in_target_cpu_ns`), when that is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) measured_percent: Option<f64>,
    /// The tool's own cpu alone, as a share of the target's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) own_percent: Option<f64>,
    pub(crate) overhead: collect::Overhead,
    pub(crate) stopped_because: String,
    /// The budget of this source is what ended the run: the stop is said on
    /// a line of its own (`MeasuredCost::stop`), whatever the whole window's
    /// figure came to.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) stopped_by_budget: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) notes: Vec<String>,
    /// Set for a recording, whose budget is weighed over a rolling span
    /// rather than the whole window: that span, in nanoseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) weighed_over_ns: Option<u64>,
}

impl MeasuredCost {
    /// The one line a reader gets for free at the end of every run.
    pub(crate) fn line(&self) -> String {
        let overhead = &self.overhead;
        let against = match (overhead.observed_cpu_ns, self.measured_percent) {
            (Some(observed), Some(percent)) => format!(
                "against the target's {} ({percent:.2} %, declared `{}`)",
                span(observed),
                self.declared
            ),
            (Some(_), None) => format!(
                "against a target that burned no cpu at all, so there is nothing for it to be \
                 a percentage of ({})",
                overhead.observed_from
            ),
            (None, _) => format!("against nothing measurable: {}", overhead.observed_from),
        };
        let inside = match (overhead.in_target_cpu_ns, overhead.in_target_runs) {
            (Some(0), None) => String::new(),
            (Some(ns), Some(runs)) if overhead.in_target_left_out > 0 => format!(
                ", and {} inside the target over {runs} run(s) of its own code, {} of them \
                 priced at the mean of the others because a preemption fell in their timed \
                 span",
                span(ns),
                overhead.in_target_left_out
            ),
            (Some(ns), Some(runs)) => format!(
                ", and {} inside the target over {runs} run(s) of its own code",
                span(ns)
            ),
            (Some(ns), None) => format!(", and {} inside the target", span(ns)),
            (None, _) => " and an unknown amount inside the target".to_owned(),
        };
        let unknown = if overhead.in_target_known() {
            String::new()
        } else {
            format!(
                ". What it spent inside the target is not in that percentage: {}",
                overhead.in_target_from
            )
        };
        // What this process spent before the window opened -- its startup,
        // the probes, the tables a run reads first, such as the kernel's
        // symbols -- dilated nothing and is in no percentage, and once it was
        // a second against a window's 80 ms with nothing on this line to say
        // so. Named when it outweighs the window. It is the process's cpu at
        // this collection's opening, so a collection attached mid-run also
        // holds the other collections' windows: it is said per collection,
        // and never added up across them.
        let before = if overhead.before_window_cpu_ns > overhead.attributed_cpu_ns {
            format!(
                "; and {} before this collection's window opened (startup, probes, the tables \
                 the run reads first), which is in no percentage here",
                span(overhead.before_window_cpu_ns)
            )
        } else {
            String::new()
        };
        // The same on the other side: what it spent after the window closed
        // -- an fentry collector unlinking its trampolines, about 69 ms for
        // `io.fsync`, the stacks resolved, the other requests' tails -- is
        // in no percentage either, and is named when it outweighs the
        // window, per collection.
        let after = if overhead.after_window_cpu_ns > overhead.attributed_cpu_ns {
            format!(
                "; and {} after it closed (detaching, resolving what it read), which is in no \
                 percentage either",
                span(overhead.after_window_cpu_ns)
            )
        } else {
            String::new()
        };
        // A table loaded once inside the window is in the cpu said, and not
        // in the percentage, which is the rate a budget weighs.
        let once = if overhead.one_time_cpu_ns > 0 {
            format!(
                " ({} of it once, loading symbol tables, weighed apart from the percentage)",
                span(overhead.one_time_cpu_ns)
            )
        } else {
            String::new()
        };
        format!(
            "{} ({}) via `{}` spent {} of cpu{once}{inside} {against}, over {} of wall \
             time{before}{after}; measuring that cost {} across {} read(s){unknown}",
            self.source,
            self.mode,
            self.served_by.join("` then `"),
            span(overhead.attributed_cpu_ns),
            span(overhead.wall_ns),
            span(overhead.measurement_ns),
            overhead.measurements,
        )
    }

    /// The line that says the budget ended the run, naming the source and
    /// the percentage that broke the ceiling.
    ///
    /// On its own line because the verdict is about the whole window, and a
    /// run stopped part way through can come out under the ceiling over the
    /// window, or not checkable whole, while the check that stopped it was
    /// over: a run ended 941 ms in was reported as `held` and `could not be
    /// checked whole` with nothing saying it had been stopped
    /// (`docs/fixes/w17-python.md`).
    fn stop(&self) -> Option<String> {
        let budget = self.budget_percent?;
        self.stopped_by_budget.then(|| {
            format!(
                "[{}] --budget {budget}% was exceeded, and the run was stopped there, before \
                 its own end: {}",
                self.source, self.stopped_because
            )
        })
    }

    /// The reason the run stopped, as the verdict's tail: nothing when the
    /// budget stopped it, which [`MeasuredCost::stop`] says on its own line.
    fn stopped_tail(&self) -> &str {
        if self.stopped_by_budget {
            "the run was stopped by this budget, as the line above says"
        } else {
            &self.stopped_because
        }
    }

    /// What the ceiling made of it, when there was one.
    fn verdict(&self) -> Option<String> {
        let budget = self.budget_percent?;
        if let Some(rolling_ns) = self.weighed_over_ns {
            let over = span(rolling_ns);
            return Some(match (self.within_budget, self.measured_percent) {
                (Some(false), _) => format!(
                    "[{}] --budget {budget}% was exceeded over a {over} span, which is what a \
                     recording is weighed over: {}",
                    self.source,
                    self.stopped_tail()
                ),
                (_, Some(percent)) => format!(
                    "[{}] --budget {budget}% held over every {over} span the run checked; \
                     {percent:.2} % over the whole window",
                    self.source
                ),
                (_, None) => format!(
                    "[{}] --budget {budget}% could not be checked: {}",
                    self.source, self.overhead.observed_from
                ),
            });
        }
        Some(match (self.within_budget, self.measured_percent) {
            (Some(true), Some(percent)) => format!(
                "[{}] --budget {budget}% held: {percent:.2} % measured",
                self.source
            ),
            (None, Some(percent)) if !self.overhead.in_target_known() => format!(
                "[{}] --budget {budget}% could not be checked whole: the tool's own cpu came to \
                 {percent:.2} %, and what the collector spent inside the target is unknown: {}",
                self.source, self.overhead.in_target_from
            ),
            (Some(false), Some(percent)) => format!(
                "[{}] --budget {budget}% was exceeded: {percent:.2} % measured over the whole \
                 window; {}",
                self.source,
                self.stopped_tail()
            ),
            _ => format!(
                "[{}] --budget {budget}% could not be checked: {}",
                self.source, self.overhead.observed_from
            ),
        })
    }
}

/// Nanoseconds a human can read, down to the microsecond: what a budget check
/// costs is measured there and rounding it away would hide the answer.
pub(crate) fn span(ns: u64) -> String {
    match ns {
        0 => "nothing".to_owned(),
        ns if ns < 1_000 => format!("{ns}ns"),
        ns if ns < 1_000_000 => format!("{:.1}us", ns as f64 / 1e3),
        ns if ns < 1_000_000_000 => format!("{:.1}ms", ns as f64 / 1e6),
        ns => format!("{:.2}s", ns as f64 / 1e9),
    }
}

/// What the measurement cost, on every run rather than only on the ones that
/// asked. A tool that reports what a program spends and never its own price
/// is asking to be trusted about the one number it never shows.
pub(crate) fn report_costs(costs: &[MeasuredCost], run: &encode::RunOutput) {
    report_costs_display(costs, run, true);
}

/// A live view returns a compact summary unless details were requested.
/// Budget stops and process-lifetime warnings remain visible in both modes.
pub(crate) fn report_costs_display(costs: &[MeasuredCost], run: &encode::RunOutput, details: bool) {
    if details {
        report_budgets(costs);
    } else {
        eprint!("{}", cost_summary(costs));
        let mut said = Vec::new();
        for cost in costs {
            if let Some(stop) = cost.stop() {
                if !said.contains(&stop) {
                    eprintln!("budget: {stop}");
                    said.push(stop);
                }
            }
            if let Some(verdict) = cost.verdict() {
                eprintln!("budget: {verdict}");
            }
        }
    }
    // Every collection carries the one program's lifetime; said once.
    let left = run
        .results
        .iter()
        .find_map(|results| results.lifetime)
        .map_or(0, |life| life.left_running);
    if left > 0 {
        eprintln!(
            "warning: the stop left {left} process(es) the program started running: they \
             could not be signalled (a setuid child refuses this process's signals)"
        );
    }
    let lifetime = run.results.iter().find_map(|results| results.lifetime);
    let adopted = lifetime.map_or(0, |life| life.adopted_ended);
    if adopted > 0 {
        // Stopped, or exited on its own leaving them running: either way the
        // run ended them, and says which end it was.
        let end = if lifetime.is_some_and(|life| life.terminated) {
            "the stop also ended"
        } else {
            "the program exited, and the end of the run then ended"
        };
        eprintln!(
            "note: {end} {adopted} process(es) the program started that had left its tree \
             before it (orphaned, their parent gone first, and adopted by omn)"
        );
    }
    if let Some(comparison) = compare_backends(costs, &run.results) {
        eprint!("{comparison}");
    }
}

/// Rank attributed tool CPU only: summing collector percentages would mix
/// windows and denominators, and cannot measure whole-program slowdown.
pub(crate) fn cost_summary(costs: &[MeasuredCost]) -> String {
    use std::fmt::Write;
    let mut out = format!("\nCOLLECTION COST / {} collectors\n", costs.len());
    let mut ranked: Vec<_> = costs.iter().collect();
    ranked.sort_by_key(|cost| std::cmp::Reverse(cost.overhead.attributed_cpu_ns));
    let _ = writeln!(
        out,
        "  {:<26} {:>12} {:>12}",
        "Largest tool CPU shares", "CPU time", "% target CPU"
    );
    for cost in ranked.into_iter().take(3) {
        let share = cost
            .own_percent
            .map_or_else(|| "unknown".to_owned(), |v| format!("{v:.2}%"));
        let _ = writeln!(
            out,
            "  {:<26} {:>12} {:>12}",
            cost.source,
            span(cost.overhead.attributed_cpu_ns),
            share
        );
    }
    let unknown = costs
        .iter()
        .filter(|cost| !cost.overhead.in_target_known())
        .count();
    if unknown > 0 {
        let _ = writeln!(out, "  In-target cost unmeasured for {unknown} collectors; these shares are not total slowdown.");
    }
    let _ = writeln!(
        out,
        "  Full diagnostics: --cost-details; saved in cost.json with --out."
    );
    out
}

/// Each collection's cost and verdict, and the budget's stop on a line of its
/// own when the budget ended the run: said once, before the verdicts, however
/// many collections of the source that stopped it there were.
pub(crate) fn report_budgets(costs: &[MeasuredCost]) {
    let mut said: Vec<String> = Vec::new();
    for stop in costs.iter().filter_map(MeasuredCost::stop) {
        if !said.contains(&stop) {
            eprintln!("budget: {stop}");
            said.push(stop);
        }
    }
    for cost in costs {
        eprintln!("cost: {}", cost.line());
        if let Some(verdict) = cost.verdict() {
            eprintln!("budget: {verdict}");
        }
    }
}

/// The backend a collection ended on, which is the one its numbers came from.
pub(crate) fn served_last(collection: &collect::Collection) -> &str {
    collection
        .served_by
        .last()
        .map(String::as_str)
        .unwrap_or_default()
}

/// Pairs what one collection cost with what its backend claimed it would.
pub(crate) fn measured_cost(
    catalog: &Catalog,
    resolved: &ResolvedRequest,
    collection: &collect::Collection,
) -> MeasuredCost {
    let served = catalog
        .get(&resolved.request.source)
        .and_then(|entry| entry.backend(served_last(collection)));
    let declared = served
        .map(|backend| backend.cost)
        .unwrap_or(resolved.backend.cost);
    let declared_by_regime = served
        .map(|backend| {
            backend
                .cost_by_regime
                .iter()
                .map(|(regime, cost)| (regime.clone(), cost.as_str()))
                .collect()
        })
        .unwrap_or_default();
    let measured_percent = collection.overhead.percent();
    let budget_percent = resolved.request.budget.map(|budget| budget.percent());
    MeasuredCost {
        label: resolved
            .request
            .label
            .clone()
            .unwrap_or_else(|| resolved.request.source.to_string()),
        source: resolved.request.source.to_string(),
        mode: resolved.request.mode.to_string(),
        backend: served_last(collection).to_owned(),
        served_by: collection.served_by.clone(),
        declared: declared.as_str(),
        declared_by_regime,
        declared_worst_case_percent: worst_case_percent(declared),
        budget_percent,
        // Over the ceiling on what is known is over it; under it is "held"
        // only when nothing spent inside the target is missing from the sum.
        within_budget: budget_percent.and_then(|ceiling| {
            measured_percent
                .map(|percent| percent <= ceiling)
                .filter(|within| !within || collection.overhead.in_target_known())
        }),
        measured_percent,
        own_percent: collection.overhead.own_percent(),
        overhead: collection.overhead.clone(),
        stopped_because: collection.stopped_because.clone(),
        stopped_by_budget: budget_percent.is_some_and(|ceiling| {
            collect::stopped_by_budget(
                &collection.stopped_because,
                resolved.request.source.as_str(),
                ceiling,
            )
        }),
        notes: collection.notes.clone(),
        weighed_over_ns: None,
    }
}

/// The self-benchmark: one source measured through several backends at once,
/// and what each of them cost.
///
/// Running one backend, then another, and comparing the two is *not* this. Two
/// sequential runs observe two different workloads, and most of the difference
/// between them is the difference between the moments. A session that names the
/// same source twice with `--backend force:` puts both backends on the same
/// target, in the same window, against the same clock -- which is the only
/// arrangement in which "one cost more" and "they disagreed" mean anything.
pub(crate) fn compare_backends(
    costs: &[MeasuredCost],
    results: &[emit::Results],
) -> Option<String> {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fmt::Write;

    let mut by_source: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (index, cost) in costs.iter().enumerate() {
        by_source
            .entry(cost.source.as_str())
            .or_default()
            .push(index);
    }

    let mut out = String::new();
    for (source, indices) in by_source {
        let backends: BTreeSet<&str> = indices
            .iter()
            .map(|index| costs[*index].backend.as_str())
            .collect();
        if backends.len() < 2 {
            continue;
        }
        let _ = writeln!(
            out,
            "self-benchmark: `{source}` through {} backends, one target, one window",
            backends.len()
        );
        let _ = writeln!(
            out,
            "  {:<24} {:>8} {:>10} {:>8} {:>14}",
            "BACKEND", "DECLARED", "MEASURED", "EVENTS", "MEAN"
        );
        for index in &indices {
            let cost = &costs[*index];
            let _ = writeln!(
                out,
                "  {:<24} {:>8} {:>10} {:>8} {:>14}",
                cost.backend,
                cost.declared,
                cost.measured_percent
                    .map_or_else(|| "--".to_owned(), |percent| format!("{percent:.3} %")),
                results[*index].events_seen,
                summary_mean(&results[*index])
                    .map_or_else(|| "--".to_owned(), |mean| format!("{mean:.4e}")),
            );
        }

        let means: Vec<f64> = indices
            .iter()
            .filter_map(|index| summary_mean(&results[*index]))
            .collect();
        match means.as_slice() {
            [] | [_] => {
                let _ = writeln!(
                    out,
                    "  whether they agree was not checked: that needs `--emit summary` on each"
                );
            }
            means => {
                let low = means.iter().cloned().fold(f64::INFINITY, f64::min);
                let high = means.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                let spread = if high.abs() > 0.0 {
                    (high - low) / high.abs() * 100.0
                } else {
                    0.0
                };
                let _ = writeln!(
                    out,
                    "  the backends agree to within {spread:.2} % on the value they report"
                );
            }
        }

        // The declared band is a claim; this is the line that checks it.
        for index in &indices {
            let cost = &costs[*index];
            if let Some(percent) = cost.measured_percent {
                if percent > cost.declared_worst_case_percent {
                    let _ = writeln!(
                        out,
                        "  `{}` declares `{}` and measured {percent:.3} % at {}: a band is \
                         declared per backend, and what a poll costs is set by how often it is \
                         asked",
                        cost.backend, cost.declared, cost.mode
                    );
                }
            }
        }
    }

    (!out.is_empty()).then_some(out)
}

/// One number for a whole result, so two backends can be asked whether they
/// agree: the count-weighted mean over every series a `summary` produced.
pub(crate) fn summary_mean(results: &emit::Results) -> Option<f64> {
    let series = results.outputs.iter().find_map(|output| match output {
        emit::Output::Summary { series } => Some(series),
        _ => None,
    })?;
    let count: usize = series.iter().map(|one| one.count).sum();
    (count > 0).then(|| {
        series
            .iter()
            .map(|one| one.mean * one.count as f64)
            .sum::<f64>()
            / count as f64
    })
}
