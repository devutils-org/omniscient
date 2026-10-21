//! The budget's ladder: which cheaper rung a request can step to, and what
//! a ceiling says before the run.

use super::*;

/// One step a budget can take below the backend and mode serving a request:
/// a backend of its chain, and the mode it would go on in.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Rung {
    backend: String,
    cost: crate::catalog::Cost,
    pub(crate) mode: crate::request::Mode,
}

impl Rung {
    /// How the rung reads in a note, beside the mode the series began in.
    fn describe(&self, from: crate::request::Mode) -> String {
        if self.mode == from {
            format!("`{}` ({})", self.backend, self.cost)
        } else {
            format!(
                "`{}` in `{}` ({}, every event then standing for its period)",
                self.backend, self.mode, self.cost
            )
        }
    }
}

/// The rungs below what serves `resolved`, in the order a budget takes them.
///
/// First the backends below the serving one in the chain, in the same mode,
/// whose declared band fits the ceiling: each keeps every event. Then each
/// cheaper mode ([`crate::request::Mode::cheaper`], phase 6's `trace` ->
/// `sample`) on the serving backend and on those below it that fit, when the
/// request's emits can take a series made of the two modes
/// ([`crate::request::mode_move`]), and only on a backend whose collector
/// weighs each sampled event by its period ([`weighs_its_samples`]). The
/// serving backend is offered in the
/// cheaper mode whatever its band: the band is the backend's, and a sampled
/// mode of it costs less than the traced one it was measured over.
///
/// The ones above were rejected by this machine and a budget cannot make them
/// reachable; the ones below were never probed, because a better backend was
/// selected first -- so each is resolved for real before it is offered.
pub(crate) fn rungs(catalog: &Catalog, resolved: &ResolvedRequest, ceiling: f64) -> Vec<Rung> {
    let Some(entry) = catalog.get(&resolved.request.source) else {
        return Vec::new();
    };
    let mode = resolved.request.mode;
    let serving = entry.backend(&resolved.backend.id);
    let below: Vec<&crate::catalog::Backend> = entry
        .backends
        .iter()
        .skip_while(|backend| backend.id != resolved.backend.id)
        .skip(1)
        .filter(|backend| worst_case_percent(backend.cost) <= ceiling)
        .collect();
    let rung = |backend: &crate::catalog::Backend, mode| Rung {
        backend: backend.id.clone(),
        cost: backend.cost,
        mode,
    };
    let mut rungs: Vec<Rung> = below
        .iter()
        .filter(|backend| entry.backend_modes(backend).contains(&mode.kind()))
        .map(|backend| rung(backend, mode))
        .collect();
    for cheaper in mode.cheaper() {
        if crate::request::mode_move(mode.kind(), cheaper.kind(), &resolved.request.emit).is_err() {
            continue;
        }
        rungs.extend(
            serving
                .into_iter()
                .chain(below.iter().copied())
                .filter(|backend| entry.backend_modes(backend).contains(&cheaper.kind()))
                .filter(|backend| {
                    collect::weighs_its_samples(resolved.request.source.as_str(), &backend.id)
                })
                .map(|backend| rung(backend, cheaper)),
        );
    }
    rungs
}

/// The mode rungs a budget passes over because the collector's sampled events
/// would not be weighed, as one sentence for the budget's note.
pub(crate) fn unweighed_mode_rungs(
    catalog: &Catalog,
    resolved: &ResolvedRequest,
) -> Option<String> {
    unweighed_mode_rungs_given(catalog, resolved, collect::weighs_its_samples)
}

/// [`unweighed_mode_rungs`], with which collectors weigh their samples given.
pub(crate) fn unweighed_mode_rungs_given(
    catalog: &Catalog,
    resolved: &ResolvedRequest,
    weighs: fn(&str, &str) -> bool,
) -> Option<String> {
    let entry = catalog.get(&resolved.request.source)?;
    let mode = resolved.request.mode;
    let passed: Vec<String> = mode
        .cheaper()
        .into_iter()
        .filter(|cheaper| {
            crate::request::mode_move(mode.kind(), cheaper.kind(), &resolved.request.emit).is_ok()
        })
        .flat_map(|cheaper| {
            entry
                .backends
                .iter()
                .skip_while(|backend| backend.id != resolved.backend.id)
                .filter(move |backend| entry.backend_modes(backend).contains(&cheaper.kind()))
                .filter(|backend| {
                    collect::implements(resolved.request.source.as_str(), &backend.id)
                        && !weighs(resolved.request.source.as_str(), &backend.id)
                })
                .map(move |backend| format!("`{}` in `{cheaper}`", backend.id))
        })
        .collect();
    (!passed.is_empty()).then(|| {
        format!(
            "{}, is passed over: its sampled events carry no `{}`, so each would count once \
             where it stood for its period and every total after the move would be short by \
             that factor",
            passed.join(" and "),
            crate::emit::WEIGHT_ATTR
        )
    })
}

/// Refuses a budget that could move a request onto a mode its emits cannot
/// be continued in, before anything runs.
///
/// A `raw` list -- or a `timeline`, or a `stack` count -- of a series that is
/// traced until the budget moves it and sampled after would present two kinds
/// of event as one: a traced event is an occurrence, a sampled one stands for
/// a period. The destination mode produces the emit, so the rung would be
/// taken; the combination is what is invalid, and it is refused with the
/// reason rather than degraded silently at the move. An emit the cheaper mode
/// cannot produce at all only takes that rung out of the ladder, which
/// [`budget_note`] says.
pub(crate) fn budget_refusal(catalog: &Catalog, resolved: &ResolvedRequest) -> Option<String> {
    budget_refusal_given(catalog, resolved, collect::weighs_its_samples)
}

/// [`budget_refusal`], with which collectors weigh their samples given: a
/// sampled rung whose events are not weighed is passed over
/// ([`weighs_its_samples`]), so it cannot mix two kinds of event and there is
/// nothing to refuse.
pub(crate) fn budget_refusal_given(
    catalog: &Catalog,
    resolved: &ResolvedRequest,
    weighs: fn(&str, &str) -> bool,
) -> Option<String> {
    let budget = resolved.request.budget?;
    let entry = catalog.get(&resolved.request.source)?;
    let mode = resolved.request.mode;
    let collected = |backend: &crate::catalog::Backend| {
        collect::implements(resolved.request.source.as_str(), &backend.id)
    };
    for cheaper in mode.cheaper() {
        let Err(why @ crate::request::ModeMove::Mixed(emit)) =
            crate::request::mode_move(mode.kind(), cheaper.kind(), &resolved.request.emit)
        else {
            continue;
        };
        let Some(onto) = entry
            .backends
            .iter()
            .skip_while(|backend| backend.id != resolved.backend.id)
            .find(|backend| {
                entry.backend_modes(backend).contains(&cheaper.kind())
                    && collected(backend)
                    && weighs(resolved.request.source.as_str(), &backend.id)
            })
        else {
            continue;
        };
        return Some(format!(
            "[{}] --budget {budget} may move this collection from `{mode}` to `{cheaper}` on \
             `{}`: {why}. Drop `{emit}` or the budget, or ask for `-m {}` from the start",
            resolved.request.source,
            onto.id,
            cheaper.kind(),
        ));
    }
    None
}

/// What `--budget` implies before anything has been measured.
///
/// The flag is now enforced against a measurement, and the measurement does not
/// exist until the run is over -- so this is the part that can be said in
/// advance: what the selected backend *declares* it costs, whether that claim
/// fits the ceiling, and what the run will do if the measurement agrees. It is
/// printed on the dumps too, where the reader is deciding whether the request
/// says what they meant.
///
/// The comparison is against the *worst case* of the declared band: `med` means
/// "1 to 5 %", so it fits a 5 % budget and cannot be promised against a 2 % one.
/// Erring the other way would let a budget look satisfied by a backend that may
/// well exceed it.
pub(crate) fn budget_note(catalog: &Catalog, resolved: &ResolvedRequest) -> Option<String> {
    let budget = resolved.request.budget?;
    let source = &resolved.request.source;
    let selected = &resolved.backend;
    let ceiling = budget.percent();

    if worst_case_percent(selected.cost) <= ceiling {
        return Some(format!(
            "[{source}] --budget {budget}: `{}` declares `{}` ({}), which is within it. \
             What it actually cost is measured over the run and reported when it ends",
            selected.id,
            selected.cost,
            band(selected.cost),
        ));
    }

    let regimes = catalog
        .get(source)
        .and_then(|entry| entry.backend(&selected.id))
        .and_then(|backend| by_regime(backend, ceiling))
        .unwrap_or_default();
    let rungs = rungs(catalog, resolved, ceiling);
    let others = rungs.iter().any(|rung| rung.backend != selected.id);
    let passed = unweighed_mode_rungs(catalog, resolved)
        .map(|why| format!(". The ladder's cheaper mode, {why}"))
        .unwrap_or_default();
    let cheaper: Vec<String> = rungs
        .iter()
        .map(|rung| rung.describe(resolved.request.mode))
        .collect();

    Some(match cheaper.as_slice() {
        [] => format!(
            "[{source}] --budget {budget}: `{}` declares `{}` ({}), and nothing below it in the \
             chain declares less, so a run that exceeds the ceiling will stop rather than \
             degrade{passed}{regimes}",
            selected.id,
            selected.cost,
            band(selected.cost),
        ),
        candidates => format!(
            "[{source}] --budget {budget}: `{}` declares `{}` ({}), so a run that exceeds the \
             ceiling will degrade onto {}{}{}{regimes}",
            selected.id,
            selected.cost,
            band(selected.cost),
            candidates.join(" or "),
            // A rung on the serving backend was probed with it; one below it
            // was not, because a better backend was selected first.
            if others {
                " -- which was never probed, because a better backend was selected first"
            } else {
                ""
            },
            passed,
        ),
    })
}

/// What a backend's `cost_by_regime` says against a ceiling: its band in
/// each regime, and which regimes fit, since a band past the ceiling on the
/// worst regime may well be within it on the program at hand.
pub(crate) fn by_regime(backend: &crate::catalog::Backend, ceiling: f64) -> Option<String> {
    if backend.cost_by_regime.is_empty() {
        return None;
    }
    let bands: Vec<String> = backend
        .cost_by_regime
        .iter()
        .map(|(regime, cost)| format!("{regime} `{cost}`"))
        .collect();
    let fit: Vec<&str> = backend
        .cost_by_regime
        .iter()
        .filter(|(_, cost)| worst_case_percent(**cost) <= ceiling)
        .map(|(regime, _)| regime.as_str())
        .collect();
    Some(format!(
        ". Its band is its worst regime's; by regime it declares {}{}",
        bands.join(", "),
        if fit.is_empty() {
            ", none within the ceiling".to_owned()
        } else {
            format!(
                ", so a {}-bound program is within the ceiling and the measurement says which \
                 this one is",
                fit.join("- or ")
            )
        }
    ))
}

/// The rungs a budget may step onto, resolved against this machine.
///
/// The runner knows it is over budget and nothing else. What is cheaper than
/// the backend and mode in use is a question about the catalog, and whether
/// this machine can serve it is a question for the resolver; both live here,
/// which is what keeps `src/collect/` from reaching back into either.
///
/// See [`rungs`] for the order. Each rung is resolved for real before it is
/// offered, and one this build has no collector for is not offered at all.
pub(crate) struct Ladder<'a> {
    catalog: &'a Catalog,
    capabilities: &'a Capabilities,
    /// The resolved request each collection is being served by, replaced when a
    /// budget moves it. This, not the file written before the run, is what
    /// names the backend that produced the numbers.
    serving: Vec<ResolvedRequest>,
    /// Every request offered per collection, in order. A move that is taken
    /// adds a backend to the collection's `served_by`, and a move that cannot
    /// be started ends the run, so the `n`th offer is what served after the
    /// `n`th move -- which a backend id alone cannot tell when the move kept
    /// the backend and changed the mode.
    offered: Vec<Vec<ResolvedRequest>>,
}

impl<'a> Ladder<'a> {
    pub(crate) fn new(
        catalog: &'a Catalog,
        capabilities: &'a Capabilities,
        resolved: &[ResolvedRequest],
    ) -> Self {
        Self {
            catalog,
            capabilities,
            serving: resolved.to_vec(),
            offered: vec![Vec::new(); resolved.len()],
        }
    }

    /// The resolved request that was serving collection `index` when the run
    /// ended, which is the one to keep beside its numbers.
    ///
    /// `served_by` is the collection's own record of the backends it ran
    /// through: the only thing that knows how many moves were taken, since a
    /// move that could not be started leaves the run where it was.
    pub(crate) fn record(
        &self,
        index: usize,
        served_by: &[String],
        original: &ResolvedRequest,
    ) -> ResolvedRequest {
        let moves = served_by.len().saturating_sub(1);
        let backend = served_by.last().map(String::as_str).unwrap_or_default();
        if moves == 0 && (backend.is_empty() || original.backend.id == backend) {
            return original.clone();
        }
        if let Some(offered) = self
            .offered
            .get(index)
            .and_then(|offered| offered.get(moves.checked_sub(1)?))
        {
            if offered.backend.id == backend {
                return offered.clone();
            }
        }
        let mut amended = original.clone();
        amended.backend.id = backend.to_owned();
        amended
    }

    /// The modes a collection ran in, in order, as the cost line names them:
    /// one mode, or `trace then sample@99hz` across a move between modes.
    pub(crate) fn modes(
        &self,
        index: usize,
        served_by: &[String],
        original: &ResolvedRequest,
    ) -> String {
        let moves = served_by.len().saturating_sub(1);
        let mut modes = vec![original.request.mode];
        for offered in self.offered.get(index).into_iter().flatten().take(moves) {
            if modes.last() != Some(&offered.request.mode) {
                modes.push(offered.request.mode);
            }
        }
        modes
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" then ")
    }
}

impl collect::Cheaper for Ladder<'_> {
    fn next(
        &mut self,
        index: usize,
        _spent: &collect::Overhead,
        ceiling: f64,
    ) -> Option<ResolvedRequest> {
        let serving = self.serving.get(index)?.clone();
        let source = serving.request.source.clone();
        for rung in rungs(self.catalog, &serving, ceiling) {
            if !collect::implements(source.as_str(), &rung.backend) {
                continue;
            }
            let mut request = serving.request.clone();
            request.mode = rung.mode;
            // Resolved for real: a backend further down the chain was never
            // probed, and offering one this machine cannot serve would turn a
            // budget into a failed run. Onto the rung, not pinned to it: the
            // ladder chose it, and the user pinned nothing.
            let Ok(mut resolved) =
                resolve::resolve_onto(self.catalog, self.capabilities, &request, &rung.backend)
            else {
                continue;
            };
            if rung.mode != serving.request.mode {
                resolved.degradations.push(format!(
                    "the budget moved this collection from `{}` to `{}` on `{}`: up to the move \
                     every event is one occurrence, and after it each stands for its period, \
                     which the emits weigh by the event's `{}`",
                    serving.request.mode,
                    rung.mode,
                    rung.backend,
                    crate::emit::WEIGHT_ATTR
                ));
            }
            self.serving[index] = resolved.clone();
            self.offered[index].push(resolved.clone());
            return Some(resolved);
        }
        None
    }
}

/// The top of the overhead band a declared cost stands for: the catalog's
/// own answer, which `omn-mcp` reads too.
pub(crate) fn worst_case_percent(cost: crate::catalog::Cost) -> f64 {
    cost.worst_case_percent()
}

pub(crate) fn band(cost: crate::catalog::Cost) -> &'static str {
    use crate::catalog::Cost;
    match cost {
        Cost::Free => "about zero, under 0.1 %",
        Cost::Low => "under 1 %",
        Cost::Med => "1 to 5 %",
        Cost::High => "5 to 20 %",
        Cost::Extreme => "over 20 %, with no bound above",
    }
}

/// Whether a recording ended because the request of `source` under `ceiling`
/// was over its rolling budget.
pub(crate) fn over_budget(stopped_because: &str, source: &str, ceiling: f64) -> bool {
    collect::stopped_by_budget(stopped_because, source, ceiling)
        && stopped_because.contains(collect::ROLLING_BUDGET)
}

/// The budget's ladder, shared with a recording's documents.
pub(crate) struct Shared<'l, 'a>(pub(crate) &'l std::cell::RefCell<Ladder<'a>>);

impl collect::Cheaper for Shared<'_, '_> {
    fn next(
        &mut self,
        index: usize,
        spent: &collect::Overhead,
        ceiling: f64,
    ) -> Option<ResolvedRequest> {
        self.0.borrow_mut().next(index, spent, ceiling)
    }
}
