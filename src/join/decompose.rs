//! A decomposition: a whole, the parts the joins attribute to it, and the
//! signed residue -- the second engine beside the join rules, which reads their
//! attributions.

use super::*;

/// A whole, the parts it is made of, and what is left over.
///
/// Declared in a session beside the joins:
///
/// ```yaml
/// decompose:
///   - id: where-the-time-went
///     whole: window
///     parts:
///       - {state: on-cpu,   from: cpu.profile,   value: samples}
///       - {state: runnable, from: sched.runqlat, value: total_ns}
///       - {state: blocked,  from: sched.offcpu,  value: total_ns}
/// ```
///
/// The output is one table per key, each row carrying what kind of number it
/// is, the evidence that ties it to the key, its resolution and its shortfall,
/// and under them a residue that is signed and never rounded away: negative is
/// something counted twice, positive is a state no row covers. Both have
/// turned out to be findings every time one was measured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decomposition {
    pub id: String,
    pub whole: Whole,
    pub parts: Vec<Part>,
    /// The key every row is read per, so that the rows of one table are about
    /// one thing. `task` unless said otherwise.
    #[serde(default = "default_by")]
    pub by: KeyKind,
    /// How many keys to report, heaviest first.
    #[serde(default = "default_tables")]
    pub top: usize,
}

pub(super) fn default_by() -> KeyKind {
    KeyKind::Task
}

pub(super) fn default_tables() -> usize {
    3
}

/// What the parts are parts of.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    untagged,
    expecting = "`window`, or a measurement: {from: <source>, value: <attribute>, reading: \
                 level|delta}"
)]
pub enum Whole {
    Span(Span),
    Measured(Measure),
}

/// A whole the run itself defines rather than a source measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Span {
    /// The run's window, from its start to the last drain. An upper bound on
    /// the life of anything measured in it, and a loose one: it is late by
    /// however late that drain was, which the compute crossing measured at 14
    /// to 146 ms.
    Window,
    /// The launched program's own life, from the gate that released it to the
    /// reap that found it gone.
    ///
    /// The exact whole of a time decomposition, where `window` is only a bound
    /// on one. It is not exact about the exit, and says by how much: the reap
    /// learns of the exit when it next asks, so the exit is known to lie in
    /// `[exited_ns - exited_err_ns, exited_ns]`, and that bracket is the whole's
    /// resolution. It goes into the residue's resolution with the parts',
    /// because a residue smaller than the bracket is not a measurement of
    /// anything.
    ///
    /// Refused at validation in a session that launches nothing: there is no
    /// life to be the whole of.
    Lifetime,
}

/// A whole read off a source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Measure {
    pub from: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub less: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reading: Option<Reading>,
    /// Which of the source's events it is read from, as for [`Part`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<FilterExpr>,
}

/// One row of a decomposition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Part {
    /// What the row is called: `on-cpu`, `retained`.
    pub state: String,
    /// The source it is read from, which the session has to collect.
    pub from: String,
    /// The attribute read off each event, or `samples` for a sampled source,
    /// which is the count of samples times the period.
    pub value: String,
    /// An attribute subtracted from `value`, for a row that contains another:
    /// `total_ns` less `queued_ns`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub less: Option<String>,
    /// What one reading of `value` is. Required for a polled source, for the
    /// reason [`Reading`] gives; a traced or sampled one is always a delta.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reading: Option<Reading>,
    /// Which of the source's events the row is read from; every one, when
    /// absent. A level's change is its last reading less its first, and the
    /// first poll of a launched program can land before it exec'd -- a
    /// reading of this tool's own forked image, which `live > 0` leaves out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<FilterExpr>,
    /// An attribute whose value the row names beside its amount: who released
    /// a wait (`holder_tid`), which device a request went to (`dev`). The most
    /// frequent value among the row's events for the key, with how many of
    /// them carried it -- because "the lock was released by 8821" and "by 8821
    /// three times of four" are different claims, and a row that names one
    /// should not read as the other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub names: Option<String>,
    /// The key the row's value was carried through before it reached the
    /// decomposition's `by`: `sock` for a row the socket's cookie tied to the
    /// request. The row is rated the weaker of the two keys -- a request's id
    /// read back off a socket some other thread's program stamped is as good
    /// as the socket, not as the id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<KeyKind>,
}

impl Part {
    /// What the row reads, without what it is called.
    fn measure(&self) -> Measure {
        Measure {
            from: self.from.clone(),
            value: self.value.clone(),
            less: self.less.clone(),
            reading: self.reading,
            filter: self.filter.clone(),
        }
    }
}

/// The `value` that means a sample count rather than an attribute.
pub const SAMPLES: &str = "samples";

/// The prefix of a sampling collector's `fields` that carry, per key, the cpu
/// a task had used before the sampler found it (`unsampled_ns:task:4242`):
/// its `schedstat`, read at the tick that found it. A `samples` row adds it,
/// because no sample of the run covers that time and the kernel counted it.
pub const UNSAMPLED_NS: &str = "unsampled_ns:";

/// The `value` that means a part is read from a join rather than a source:
/// its `from` names one of the session's `joins`, and the row is the time
/// between the two ends of every pair that join made, summed per key.
///
/// The row the I/O crossing could not declare: *completion -> wakeup* is not
/// an attribute of any event, it is the distance between two events of two
/// sources, and only a join knows which two belong together. Only `woke`
/// makes pairs whose distance means something -- a close and the wakeup that
/// ended it -- so it is the one match a part may read; an `eq` state never
/// ends on its match, and the distance from an allocation to a fault inside
/// it is not a duration of anything.
///
/// ```yaml
/// joins:
///   - {id: completion-released, from: io.latency, to: sched.wakeup, via: task,
///      match: woke, woken: wakee_tid, waker: waker_tid}
/// decompose:
///   - id: one-read
///     whole: {from: io.syscall, value: total_ns}
///     parts:
///       - {state: device, from: io.latency, value: latency_ns}
///       - {state: completion -> wakeup, from: completion-released, value: paired}
/// ```
pub const PAIRED: &str = "paired";

/// What kind of number a row is, which is what a reader needs before adding
/// two of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A count of samples, each standing for one period: exact about the
    /// samples, and a count's plus or minus one about the time.
    Sampled,
    /// Read on a tick: short by whatever happened after the last reading.
    Polled,
    /// Every occurrence, summed: exact.
    Traced,
    /// A counter read once over the run: exact.
    Counted,
    /// The run's own window: a bound, not a measurement.
    Window,
    /// A launched program's life, gate to reap: exact at the start and
    /// bracketed at the end, with the bracket as its resolution.
    Lifetime,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Sampled => "sampled",
            Kind::Polled => "polled",
            Kind::Traced => "traced",
            Kind::Counted => "counted",
            Kind::Window => "window",
            Kind::Lifetime => "lifetime",
        }
    }

    /// A span that contains the key's events rather than a reading tied to
    /// the key: the only kind of row that has no evidence level.
    pub fn is_bracket(self) -> bool {
        matches!(self, Kind::Window | Kind::Lifetime)
    }

    fn of(mode: Mode) -> Self {
        match mode {
            Mode::Sample { .. } => Kind::Sampled,
            Mode::Poll { .. } | Mode::Snapshot => Kind::Polled,
            Mode::Trace => Kind::Traced,
            Mode::Count => Kind::Counted,
        }
    }
}

/// What a decomposition produced over one run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecompositionOutput {
    pub id: String,
    pub by: KeyKind,
    /// The unit every row is in. The validator refuses a decomposition whose
    /// parts are not all in one.
    pub unit: Unit,
    /// The heaviest keys, one table each.
    pub tables: Vec<Breakdown>,
    /// How many keys the parts' events named, of which `tables` holds the
    /// heaviest. A decomposition of one task that finds three has found the
    /// children it forked.
    pub keys_seen: usize,
}

/// One key's decomposition.
///
/// Every row carries its evidence level, and so does a whole that was read
/// from a source: a line of the table that does not say what ties it to the
/// key reads as a finding it is not. The engine builds every such row with
/// one ([`Readout`] rates each by the table's key and the part's `via`), and a
/// document that arrives without one -- a `results.json` edited, or written by
/// something else -- is refused when it is read, rather than shown with a `-`.
/// Only a whole that is a bracket (the run's window, the program's life) has
/// none, because nothing joins it to the key: it contains the key's events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "UncheckedBreakdown")]
pub struct Breakdown {
    pub key: String,
    pub whole: Row,
    pub rows: Vec<Row>,
    /// The rows added up. Absent when one of them could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parts: Option<f64>,
    /// `whole - parts`, signed, and never rounded. Negative is something
    /// counted twice; positive is a state no row covers. Absent when the whole
    /// or a part could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub residue: Option<f64>,
    /// How far the residue may be from zero on resolution alone: the sum of
    /// every row's. Absent when one of them has none that can be stated in
    /// the decomposition's unit, which is every polled level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub residue_resolution: Option<f64>,
}

/// A [`Breakdown`] as it is read, before its rows' evidence is checked.
#[derive(Deserialize)]
pub(super) struct UncheckedBreakdown {
    key: String,
    whole: Row,
    rows: Vec<Row>,
    #[serde(default)]
    parts: Option<f64>,
    #[serde(default)]
    residue: Option<f64>,
    #[serde(default)]
    residue_resolution: Option<f64>,
}

impl TryFrom<UncheckedBreakdown> for Breakdown {
    type Error = String;

    fn try_from(table: UncheckedBreakdown) -> Result<Self, Self::Error> {
        if let Some(row) = table.rows.iter().find(|row| row.evidence.is_none()) {
            return Err(format!(
                "the `{}` row of the table for `{}` carries no evidence level: every row says what ties it to the key",
                row.state, table.key
            ));
        }
        if table.whole.evidence.is_none() && !table.whole.kind.is_bracket() {
            return Err(format!(
                "the whole of the table for `{}` was read from `{}` and carries no evidence level: only a window or a life, which bracket the key, can go without one",
                table.key, table.whole.source
            ));
        }
        Ok(Breakdown {
            key: table.key,
            whole: table.whole,
            rows: table.rows,
            parts: table.parts,
            residue: table.residue,
            residue_resolution: table.residue_resolution,
        })
    }
}

/// One row of a decomposition, or its whole.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub state: String,
    pub source: String,
    /// The attribute read, `samples`, or `observed_ns` for the window.
    pub value: String,
    pub kind: Kind,
    /// What ties the row to the table's key. KEY for a row whose events carry
    /// it; absent only for a whole that is a bracket rather than a join (the
    /// window, the program's life), which [`Breakdown`] enforces when read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Evidence>,
    /// Absent when it could not be read, and `missing` says why: a row that
    /// reads as zero because no event carried its attribute would be a
    /// measurement of nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount: Option<f64>,
    /// In the decomposition's unit: one period for a sample count, zero for a
    /// traced sum, one poll for a polled delta in nanoseconds. Absent where it
    /// cannot be stated in that unit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<f64>,
    /// The events the row was read from.
    pub readings: u64,
    /// The source's run was lossy, so the amount is a lower bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short: Option<Shortfall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing: Option<String>,
    /// How the amount was obtained, when that is more than a sum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// What the part asked the row to name, and how firmly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub named: Option<Named>,
}

/// The value a row names, and how many of its events agreed on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Named {
    pub attr: String,
    pub value: String,
    /// Events that carried this value.
    pub events: u64,
    /// Events that carried the attribute at all. `events < of` is a row whose
    /// events disagreed, and the name is only the most frequent answer.
    pub of: u64,
}

/// The most frequent value of `attr` among `events`, ties broken by the value
/// so the answer does not depend on the order events arrived in.
pub(super) fn most_frequent(events: &[&Event], attr: &str) -> Option<Named> {
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    for event in events {
        if let Some(value) = event.attr(attr) {
            *counts.entry(value.to_string()).or_default() += 1;
        }
    }
    let of = counts.values().sum();
    counts
        .into_iter()
        .max_by(|(left_value, left), (right_value, right)| {
            left.cmp(right).then_with(|| right_value.cmp(left_value))
        })
        .map(|(value, events)| Named {
            attr: attr.to_owned(),
            value,
            events,
            of,
        })
}

/// Whether a decomposition can be computed from what a session collects.
///
/// Every refusal here is a table that would otherwise be printed and be wrong:
/// a part whose source is not collected reads as zero, a part about another
/// target is a row about a different program, a sample count of a traced
/// source multiplies by a period that does not exist, and two units added
/// together are a number of nothing.
pub fn check_decomposition(
    decomposition: &Decomposition,
    requests: &[Request],
    catalog: &Catalog,
) -> Result<(), JoinError> {
    check_decomposition_in(decomposition, requests, &[], catalog)
}

/// [`check_decomposition`], knowing the session's joins, which a part read
/// from a join ([`PAIRED`]) names.
pub fn check_decomposition_in(
    decomposition: &Decomposition,
    requests: &[Request],
    joins: &[Rule],
    catalog: &Catalog,
) -> Result<(), JoinError> {
    let refuse = |reason: String| JoinError::Decomposition {
        id: decomposition.id.clone(),
        reason,
    };
    if decomposition.parts.is_empty() {
        return Err(refuse(
            "has no parts, so there is nothing to decompose into".to_owned(),
        ));
    }

    let request_of = |source: &str, role: &str| -> Result<&Request, JoinError> {
        let mut found = requests
            .iter()
            .filter(|request| request.source.as_str() == source);
        match (found.next(), found.next()) {
            (Some(request), None) => Ok(request),
            (None, _) => Err(refuse(format!(
                "reads {role} from `{source}`, which this session does not collect"
            ))),
            (Some(_), Some(_)) => Err(refuse(format!(
                "reads {role} from `{source}`, which this session collects twice: a row has to \
                 come from one request"
            ))),
        }
    };

    // The unit and the target every row has to share, set by the whole when a
    // source measures it and by the first part when the whole is the window.
    let (unit, mut target): ((Unit, String), Option<(&Target, String)>) = match &decomposition.whole
    {
        Whole::Measured(measure) => {
            let request = request_of(&measure.from, "its whole")?;
            let what = format!("the whole, `{}`", measure.from);
            check_read(
                decomposition,
                request,
                &measure.value,
                measure.less.is_some(),
                measure.reading,
                &what,
            )?;
            (
                (unit_of(catalog, request, &measure.value), what.clone()),
                Some((&request.target, what)),
            )
        }
        Whole::Span(Span::Window) => ((Unit::Ns, "the window".to_owned()), None),
        Whole::Span(Span::Lifetime) => {
            let launches = decomposition.parts.iter().any(|part| {
                requests.iter().any(|request| {
                    request.source.as_str() == part.from
                        && matches!(request.target, Target::Launch(_))
                })
            });
            if !launches {
                return Err(refuse(
                    "takes `lifetime` as its whole, and no part reads from a program this session \
                     launches: a lifetime is the life of a launched program, from the gate to the \
                     reap, and there is none here to be the whole of. `window` is the whole of a \
                     run that attaches to something already running"
                        .to_owned(),
                ));
            }
            ((Unit::Ns, "the lifetime".to_owned()), None)
        }
    };

    for part in &decomposition.parts {
        let what = format!("`{}`, from `{}`", part.state, part.from);
        // A part read from a join is checked against the join's `from`
        // source, which is whose keys its pairs carry and whose target they
        // are about.
        let (request, source) = if part.value == PAIRED {
            let rule = paired_rule(decomposition, part, joins)?;
            request_of(&rule.to, &format!("`{}`'s other end", part.state))?;
            (
                request_of(&rule.from, &format!("`{}`", part.state))?,
                rule.from.as_str(),
            )
        } else {
            let request = request_of(&part.from, &format!("`{}`", part.state))?;
            check_read(
                decomposition,
                request,
                &part.value,
                part.less.is_some(),
                part.reading,
                &what,
            )?;
            (request, part.from.as_str())
        };

        match &target {
            Some((reference, whose)) if *reference != &request.target => {
                return Err(refuse(format!(
                    "reads {what} for {}, and {whose} for {}: a row about another target is a \
                     row about a different program",
                    request.target, reference
                )))
            }
            Some(_) => {}
            None => target = Some((&request.target, what.clone())),
        }

        let part_unit = if part.value == PAIRED {
            Unit::Ns
        } else {
            unit_of(catalog, request, &part.value)
        };
        let (reference, whose) = &unit;
        if *reference != part_unit {
            return Err(refuse(format!(
                "reads {what} in {part_unit}, and {whose} is in {reference}: added together \
                 they are a quantity of nothing"
            )));
        }

        if let Some(entry) = source.parse().ok().and_then(|id| catalog.get(&id)) {
            if !entry.keys.contains(&decomposition.by) {
                return Err(refuse(format!(
                    "is read per `{}`, which `{source}` does not carry",
                    decomposition.by
                )));
            }
            if let Some(via) = part.via.filter(|via| !entry.keys.contains(via)) {
                return Err(refuse(format!(
                    "reads {what} through `{via}`, which `{source}` does not carry: a row cannot \
                     be rated by a key its events never had"
                )));
            }
        }
    }
    Ok(())
}

/// The join a [`PAIRED`] part reads, refused when it is not one a part can.
pub(super) fn paired_rule<'a>(
    decomposition: &Decomposition,
    part: &Part,
    joins: &'a [Rule],
) -> Result<&'a Rule, JoinError> {
    let refuse = |reason: String| JoinError::Decomposition {
        id: decomposition.id.clone(),
        reason,
    };
    let Some(rule) = joins.iter().find(|rule| rule.id == part.from) else {
        return Err(refuse(format!(
            "reads `{}` as `paired`, the pairs of a join, and `{}` names no join {}",
            part.state,
            part.from,
            if joins.is_empty() {
                "this caller passed: it read the decomposition without the session's joins"
                    .to_owned()
            } else {
                format!(
                    "of this session's: it declares {}",
                    joins
                        .iter()
                        .map(|rule| format!("`{}`", rule.id))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        )));
    };
    if rule.match_kind != MatchKind::Woke {
        return Err(refuse(format!(
            "reads `{}` as the pairs of `{}`, which matches by `{}`: only `woke` ends a state on \
             the event it is paired with, so only its pairs have a distance that is a duration. \
             An `{}` state is still open after its match, and the time from it to one event it \
             matched is not the time of anything",
            part.state, rule.id, rule.match_kind, rule.match_kind
        )));
    }
    if part.less.is_some() || part.reading.is_some() {
        return Err(refuse(format!(
            "reads `{}` as the pairs of `{}` and gives it `less` or `reading`: a pair's distance \
             is one duration, with nothing in it to subtract and no reading to be a level of",
            part.state, rule.id
        )));
    }
    Ok(rule)
}

/// Whether one row's `value` can be read the way its request collects it.
pub(super) fn check_read(
    decomposition: &Decomposition,
    request: &Request,
    value: &str,
    less: bool,
    reading: Option<Reading>,
    what: &str,
) -> Result<(), JoinError> {
    let refuse = |reason: String| JoinError::Decomposition {
        id: decomposition.id.clone(),
        reason,
    };
    let sampled = matches!(request.mode, Mode::Sample { .. });
    if value == SAMPLES && !sampled {
        return Err(refuse(format!(
            "reads {what} as `samples`, and `{}` is collected in `{}` mode: a sample count is a \
             count times a period, and there is no period here. Name the attribute to add up",
            request.source,
            request.mode.kind()
        )));
    }
    if value == SAMPLES && less {
        return Err(refuse(format!(
            "reads {what} as `samples` less an attribute, and a sample has no attribute worth \
             subtracting from its period"
        )));
    }
    if matches!(request.mode, Mode::Poll { .. }) && reading.is_none() {
        return Err(refuse(format!(
            "reads {what} from a poll, and a polled number is either a level or the change since \
             the poll before; nothing in the reading says which. Say `reading: level` or \
             `reading: delta`"
        )));
    }
    Ok(())
}

/// The unit a row comes out in: time, for a sample count, and the source's own
/// unit for an attribute of it.
pub(super) fn unit_of(catalog: &Catalog, request: &Request, value: &str) -> Unit {
    if value == SAMPLES {
        return Unit::Ns;
    }
    catalog
        .get(&request.source)
        .map(|entry| entry.unit)
        .unwrap_or(Unit::Count)
}

/// Computes a decomposition over one run.
///
/// `requests` are the requests that served each of `results`, in the same
/// order -- the served ones, since a budget can lower a sampling rate and the
/// period a sample stands for has to be the one it was taken at. `events` is
/// the run's merged stream, the one the joins read.
///
/// Infallible on purpose: [`check_decomposition`] has refused everything that
/// cannot mean anything, and what is left that the run did not supply -- a
/// source that produced no event carrying the attribute -- is a row saying so,
/// not an error that takes every other result of the run with it.
pub fn decompose(
    decomposition: &Decomposition,
    requests: &[Request],
    results: &[Results],
    events: &[Event],
) -> DecompositionOutput {
    decompose_with(decomposition, requests, results, events, &[])
}

/// [`decompose`], knowing the session's joins, which a part read from a join
/// ([`PAIRED`]) names, and pairing over the windows the results report.
///
/// A caller that gave [`apply_within`] other windows than the results' own --
/// `main.rs` gives a source that ran to the end of the run `closed_ns =
/// u64::MAX`, since what its stop drained is stamped after its window closed
/// -- calls [`decompose_within`] with them instead, or a paired part's pairs
/// are not the join's.
pub fn decompose_with(
    decomposition: &Decomposition,
    requests: &[Request],
    results: &[Results],
    events: &[Event],
    joins: &[Rule],
) -> DecompositionOutput {
    decompose_within(
        decomposition,
        requests,
        results,
        events,
        joins,
        &windows_of(results),
    )
}

/// [`decompose_with`], pairing a [`PAIRED`] part over `windows`: the map
/// [`apply_within`] was given for the session's joins, so the pairs a part
/// reads are the ones the run's own correlation counted. The results' windows
/// still say how long each source watched (the window row, and whether a key
/// it never saw had nothing to see), where the real close is the right one.
/// An empty map watches everything, as [`apply`] does.
pub fn decompose_within(
    decomposition: &Decomposition,
    requests: &[Request],
    results: &[Results],
    events: &[Event],
    joins: &[Rule],
    windows: &BTreeMap<String, Watched>,
) -> DecompositionOutput {
    let served = |source: &str| {
        requests
            .iter()
            .zip(results)
            .find(|(request, _)| request.source.as_str() == source)
    };
    // The pairs every joined part reads, made before any readout borrows
    // them: one event per pair, the `from` event's keys and the pair's
    // distance as `paired`.
    let pairs: Vec<Option<Paired>> = decomposition
        .parts
        .iter()
        .map(|part| {
            (part.value == PAIRED)
                .then(|| paired_events(decomposition, part, joins, events, windows))
        })
        .collect();

    // Each row's events, per key, in time order.
    let mut per_key: BTreeMap<String, ()> = BTreeMap::new();
    let mut read_parts: Vec<Readout> = Vec::new();
    // The span the run watched, to tell a source that watched all of it from
    // one a window narrowed: only the first can say a key it never saw had
    // nothing to see.
    let run_closed_ns = results
        .iter()
        .map(|result| result.window.closed_ns)
        .max()
        .unwrap_or(0);
    for (part, pairs) in decomposition.parts.iter().zip(&pairs) {
        let mut readout = match pairs {
            None => Readout::new(
                &part.measure(),
                decomposition.by,
                served(&part.from),
                events,
                run_closed_ns,
            ),
            Some(Ok((rule, paired))) => {
                let mut readout = Readout::new(
                    &part.measure(),
                    decomposition.by,
                    served(&rule.from),
                    paired,
                    run_closed_ns,
                );
                readout.unit = Unit::Ns;
                readout.carried = true;
                readout.paired = Some((
                    evidence(rule),
                    format!(
                        "each `{}` to the next `{}` of its task, as `{}` paired them",
                        rule.from, rule.to, rule.id
                    ),
                ));
                // Either side's losses are the row's: a lost wakeup is a
                // completion that pairs with a later one, or with none.
                if let Some((_, other)) = served(&rule.to) {
                    let records = events.iter().filter(|event| event.kind == rule.to).count();
                    let short = Shortfall::of(&other.losses, records as u64, false);
                    readout.absent_is_not_zero = readout.absent_is_not_zero.or_else(|| {
                        absent_is_not_zero(&rule.to, other, short.as_ref(), run_closed_ns)
                    });
                    readout.short = readout.short.or(short);
                } else {
                    readout.collected = false;
                }
                readout
            }
            Some(Err(why)) => {
                let mut readout =
                    Readout::new(&part.measure(), decomposition.by, None, &[], run_closed_ns);
                readout.unreadable = Some(why.clone());
                readout.unit = Unit::Ns;
                readout
            }
        };
        readout.via = part.via;
        per_key.extend(readout.keys().map(|key| (key, ())));
        read_parts.push(readout);
    }
    let whole_readout = match &decomposition.whole {
        Whole::Measured(measure) => Some(Readout::new(
            measure,
            decomposition.by,
            served(&measure.from),
            events,
            run_closed_ns,
        )),
        Whole::Span(_) => None,
    };
    // A key the whole carries and no part does is still a table: a request
    // that waited on the disk and whose id no block request carries -- a page
    // lock, a metadata read in a kworker -- has a whole and no parts, and the
    // answer for it is a residue that is all of it, not silence.
    if let Some(readout) = &whole_readout {
        per_key.extend(readout.keys().map(|key| (key, ())));
    }
    let window_ns = decomposition
        .parts
        .iter()
        .filter_map(|part| served(&part.from))
        .map(|(_, result)| result.observed_ns)
        .max()
        .unwrap_or(0);

    let unit = read_parts
        .first()
        .map(|readout| readout.unit)
        .unwrap_or(Unit::Ns);

    // One life per run, published on every result of it: the run model stamps
    // it at the gate and the reap, and the emit layer carries it.
    let lifetime = results.iter().find_map(|result| result.lifetime);

    let mut tables: Vec<Breakdown> = per_key
        .keys()
        .map(|key| {
            let rows: Vec<Row> = decomposition
                .parts
                .iter()
                .zip(&read_parts)
                .map(|(part, readout)| {
                    let mut row = readout.row(&part.state, key);
                    // A row that measured nothing had nothing released and went
                    // nowhere, and naming a value for it -- a holder of `0`, a
                    // device nobody was sent to -- would read as a finding.
                    let measured = row.amount.is_some_and(|amount| amount != 0.0);
                    if let (Some(attr), true) = (&part.names, measured) {
                        let events = readout.events.get(key).map(Vec::as_slice).unwrap_or(&[]);
                        row.named = most_frequent(events, attr);
                    }
                    row
                })
                .collect();
            let whole = match (&whole_readout, &decomposition.whole) {
                (Some(readout), _) => readout.row("whole", key),
                (None, Whole::Span(Span::Lifetime)) => lifetime_row(lifetime),
                (None, _) => Row {
                    state: "whole".to_owned(),
                    source: "the run's window".to_owned(),
                    value: "observed_ns".to_owned(),
                    kind: Kind::Window,
                    evidence: None,
                    amount: Some(window_ns as f64),
                    resolution: None,
                    readings: 0,
                    short: None,
                    missing: None,
                    note: Some(
                        "an upper bound: the window runs to the last drain, and the life of \
                         what is decomposed may end well before it"
                            .to_owned(),
                    ),
                    named: None,
                },
            };
            let parts: Option<f64> = rows.iter().map(|row| row.amount).sum();
            let residue = whole.amount.zip(parts).map(|(whole, parts)| whole - parts);
            let residue_resolution: Option<f64> = rows
                .iter()
                .chain(std::iter::once(&whole).filter(|whole| whole.kind != Kind::Window))
                .map(|row| row.resolution)
                .sum();
            Breakdown {
                key: key.clone(),
                whole,
                rows,
                parts,
                residue,
                residue_resolution,
            }
        })
        .collect();

    let keys_seen = tables.len();
    let weight = |table: &Breakdown| -> f64 {
        table
            .rows
            .iter()
            .filter_map(|row| row.amount)
            .map(f64::abs)
            .sum()
    };
    tables.sort_by(|left, right| {
        weight(right)
            .total_cmp(&weight(left))
            .then_with(|| left.key.cmp(&right.key))
    });
    tables.truncate(decomposition.top.max(1));

    DecompositionOutput {
        id: decomposition.id.clone(),
        by: decomposition.by,
        unit,
        tables,
        keys_seen,
    }
}

/// When each source of a run was watching, from its results' own windows: two
/// results of one source by their union. `main.rs` starts from the same map
/// and then opens to the end of the run every window that closed with it,
/// which is why it calls [`decompose_within`] with its own.
pub(super) fn windows_of(results: &[Results]) -> BTreeMap<String, Watched> {
    let mut windows: BTreeMap<String, Watched> = BTreeMap::new();
    for result in results {
        windows
            .entry(result.source.clone())
            .and_modify(|watched| match (watched.attached, result.window.attached) {
                (_, false) => {}
                (false, true) => *watched = result.window,
                (true, true) => {
                    watched.opened_ns = watched.opened_ns.min(result.window.opened_ns);
                    watched.closed_ns = watched.closed_ns.max(result.window.closed_ns);
                }
            })
            .or_insert(result.window);
    }
    // A run that never said when a source watched said nothing to narrow it.
    windows.retain(|_, watched| *watched != Watched::default());
    windows
}

/// The pairs a [`PAIRED`] part reads, as events of the part's own kind: the
/// `from` event of each pair, carrying its keys, with the pair's distance as
/// `paired`.
pub(super) fn paired_events<'r>(
    decomposition: &Decomposition,
    part: &Part,
    joins: &'r [Rule],
    events: &[Event],
    windows: &BTreeMap<String, Watched>,
) -> Paired<'r> {
    let rule = paired_rule(decomposition, part, joins).map_err(|error| error.to_string())?;
    let mut ordered: Vec<&Event> = events
        .iter()
        .filter(|event| event.kind == rule.from || event.kind == rule.to)
        .collect();
    ordered.sort_by_key(|event| (event.ts_ns, u8::from(event.kind != rule.from)));
    let mut watch = Watch::of(rule, windows);
    let (_, pairs) = woke(rule, &ordered, &mut watch)
        .map_err(|error| format!("the join `{}` failed on this run: {error}", rule.id))?;
    Ok((
        rule,
        pairs
            .into_iter()
            .map(|(from, distance)| {
                let mut event = from.clone();
                event.kind = part.from.clone();
                event.attrs.push(crate::event::Attr::uint(PAIRED, distance));
                event
            })
            .collect(),
    ))
}

/// The whole of a decomposition whose whole is the launched program's life.
///
/// Exact at the start, where this process wrote the gate, and bracketed at the
/// end, where it learned of the exit by asking: the bracket is the resolution,
/// and it is what the residue's resolution has to include, because a residue
/// inside it says nothing. A program this tool killed at the deadline has a
/// life that ended at the tool's choosing, which the note says, since a
/// decomposition of a program that did not finish is a decomposition of the
/// deadline.
pub(super) fn lifetime_row(lifetime: Option<crate::collect::Lifetime>) -> Row {
    let base = Row {
        state: "whole".to_owned(),
        source: "the program's life".to_owned(),
        value: "gate to reap".to_owned(),
        kind: Kind::Lifetime,
        evidence: None,
        amount: None,
        resolution: None,
        readings: 0,
        short: None,
        missing: None,
        note: None,
        named: None,
    };
    match lifetime {
        None => Row {
            missing: Some(
                "the run carried no lifetime: the program was never released, or the run \
                 failed before it was reaped"
                    .to_owned(),
            ),
            ..base
        },
        Some(life) => Row {
            amount: Some(life.exited_ns.saturating_sub(life.released_ns) as f64),
            resolution: Some(life.exited_err_ns as f64),
            note: Some(if life.terminated {
                "the program did not finish: this tool ended it at the run's deadline, so its \
                 life is the deadline's and not its own"
                    .to_owned()
            } else {
                format!(
                    "exact at the gate; the exit is bracketed to the last {}, which is how long \
                     before the reap the program was last seen alive",
                    period_label(life.exited_err_ns as f64)
                )
            }),
            ..base
        },
    }
}

/// One row's source, read once for every key.
pub(super) struct Readout<'a> {
    source: String,
    value: String,
    less: Option<String>,
    reading: Reading,
    kind: Kind,
    unit: Unit,
    /// One period, for a sample count.
    period_ns: Option<f64>,
    /// One poll, for a polled delta.
    poll_ns: Option<f64>,
    /// Per key, the events in time order.
    events: BTreeMap<String, Vec<&'a Event>>,
    /// Whether any event of the source carried each attribute, before the
    /// part's filter: a row that finds none reads as missing rather than as
    /// zero. Counted before the filter because a filter that kept nothing is
    /// the source answering, not the source unable to.
    carried: bool,
    /// The keys the source produced any event for, before the part's filter.
    seen: BTreeSet<String>,
    /// The part asked for a subset of the source's events.
    filtered: bool,
    /// Why a key the source produced nothing for cannot be read as zero, when
    /// it cannot: the source lost events, or watched less than the run.
    absent_is_not_zero: Option<String>,
    short: Option<Shortfall>,
    collected: bool,
    /// The key the rows are read by. A request identity is propagated -- the
    /// event carries what the request's birth put there -- so a row read by it
    /// is IDENTITY, the same rating `evidence` gives a join on `req`. Every
    /// other key is shared by two sides without being carried from one to the
    /// other, which is KEY.
    by: KeyKind,
    /// The key the value was carried through before it reached `by`; the row
    /// is rated the weaker of the two.
    via: Option<KeyKind>,
    /// For a part read from a join: the join's evidence, which the row is
    /// rated no better than, and the note that says what was summed.
    paired: Option<(Evidence, String)>,
    /// Why the row could not be read at all, when that is decided before any
    /// event is: a join a part reads that failed on this run's events.
    unreadable: Option<String>,
    /// For a sample count, per key, the cpu the task had used before the
    /// sampler found it, in ns ([`UNSAMPLED_NS`]).
    unsampled: BTreeMap<String, f64>,
    /// For a sample count, per key, the samples its own ring lost: taken by
    /// the kernel, which counted them, and never read.
    lost: BTreeMap<String, u64>,
}

impl<'a> Readout<'a> {
    fn new(
        measure: &Measure,
        by: KeyKind,
        served: Option<(&Request, &Results)>,
        events: &'a [Event],
        run_closed_ns: u64,
    ) -> Self {
        let (source, value, less) = (
            measure.from.as_str(),
            measure.value.as_str(),
            measure.less.as_deref(),
        );
        let (reading, filter) = (measure.reading, measure.filter.as_ref());
        let mut per_key: BTreeMap<String, Vec<&Event>> = BTreeMap::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut ordered: Vec<&Event> = events.iter().filter(|event| event.kind == source).collect();
        ordered.sort_by_key(|event| event.ts_ns);
        let mut carried = value == SAMPLES;
        let mut records = 0u64;
        let mut many = false;
        for event in ordered {
            records += 1;
            many |= stands_for_many(event);
            if value != SAMPLES {
                let has_value = event.attr(value).is_some();
                let has_less = less.is_none_or(|less| event.attr(less).is_some());
                carried |= has_value && has_less;
            }
            let Some(key) = event.key(by) else {
                continue;
            };
            let key = key.to_string();
            if filter.is_none_or(|predicate| crate::filter::matches(predicate, event)) {
                per_key.entry(key.clone()).or_default().push(event);
            }
            seen.insert(key);
        }

        let (kind, period_ns, poll_ns) = match served.map(|(request, _)| request.mode) {
            Some(Mode::Sample { freq }) => {
                (Kind::Sampled, Some(1e9 / freq.as_hz().max(1) as f64), None)
            }
            Some(Mode::Poll { interval }) => (Kind::Polled, None, Some(interval.as_nanos() as f64)),
            Some(mode) => (Kind::of(mode), None, None),
            None => (Kind::Traced, None, None),
        };
        let unit = if value == SAMPLES {
            Unit::Ns
        } else {
            served.map(|(_, result)| result.unit).unwrap_or(Unit::Count)
        };
        let reading = match kind {
            Kind::Polled => reading.unwrap_or(Reading::Level),
            _ => reading.unwrap_or(Reading::Delta),
        };
        let short = served.and_then(|(_, result)| Shortfall::of(&result.losses, records, many));
        let absent_is_not_zero = served.and_then(|(_, result)| {
            absent_is_not_zero(source, result, short.as_ref(), run_closed_ns)
        });

        // A ring per key: what each lost is that key's samples, taken and
        // counted by the kernel and never read, so the row adds them. When the
        // rings account for every loss of the run and nothing was throttled or
        // unread, the row is whole again and not a lower bound.
        let lost: BTreeMap<String, u64> = match served {
            Some((_, result)) if value == SAMPLES => result
                .by_ring
                .iter()
                .filter(|ring| ring.losses.dropped > 0)
                .map(|ring| (ring.key.to_string(), ring.losses.dropped))
                .collect(),
            _ => BTreeMap::new(),
        };
        let short = match served {
            Some((_, result))
                if value == SAMPLES
                    && result.losses.dropped > 0
                    && lost.values().sum::<u64>() == result.losses.dropped
                    && result.losses.throttled == 0
                    && result.losses.undecoded == 0
                    && result.losses.failed_reads == 0 =>
            {
                None
            }
            _ => short,
        };
        let unsampled = match served {
            Some((_, result)) if value == SAMPLES => result
                .fields
                .iter()
                .filter_map(|(name, ns)| {
                    Some((name.strip_prefix(UNSAMPLED_NS)?.to_owned(), *ns as f64))
                })
                .collect(),
            _ => BTreeMap::new(),
        };
        Self {
            source: source.to_owned(),
            value: value.to_owned(),
            less: less.map(str::to_owned),
            reading,
            kind,
            unit,
            period_ns,
            poll_ns,
            events: per_key,
            carried,
            seen,
            filtered: filter.is_some(),
            absent_is_not_zero,
            short,
            collected: served.is_some(),
            by,
            via: None,
            paired: None,
            unreadable: None,
            unsampled,
            lost,
        }
    }

    fn keys(&self) -> impl Iterator<Item = String> + '_ {
        self.events.keys().cloned()
    }

    fn row(&self, state: &str, key: &str) -> Row {
        let events = self.events.get(key).map(Vec::as_slice).unwrap_or(&[]);
        let missing = if let Some(why) = &self.unreadable {
            Some(why.clone())
        } else if !self.collected {
            Some(format!("the run collected no `{}`", self.source))
        } else if !self.carried {
            Some(match self.less.as_deref() {
                Some(less) => format!(
                    "no `{}` event carried both `{}` and `{less}`",
                    self.source, self.value
                ),
                None => format!("no `{}` event carried `{}`", self.source, self.value),
            })
        } else if events.is_empty()
            && !(self.filtered && self.seen.contains(key))
            && !self.unsampled.contains_key(key)
            && !self.lost.contains_key(key)
        {
            // The source had nothing at all for this key. That is a zero only
            // from a source that watched the whole run and lost nothing.
            self.absent_is_not_zero.clone()
        } else {
            None
        };

        let read = |attr: &str| -> f64 {
            let values = events
                .iter()
                .filter_map(|event| event.attr(attr).and_then(AttrValue::as_f64));
            match self.reading {
                Reading::Delta => values.sum(),
                Reading::Level => {
                    let values: Vec<f64> = values.collect();
                    match (values.first(), values.last()) {
                        (Some(first), Some(last)) => last - first,
                        _ => 0.0,
                    }
                }
            }
        };
        let (amount, note) = if missing.is_some() {
            (None, None)
        } else if let (Some((_, note)), false) = (&self.paired, events.is_empty()) {
            (
                Some(read(&self.value)),
                Some(format!("{note}, {} pairs", events.len())),
            )
        } else if self.value == SAMPLES
            && (self.unsampled.contains_key(key) || self.lost.contains_key(key))
        {
            let period = self.period_ns.unwrap_or(0.0);
            let before = self.unsampled.get(key).copied().unwrap_or(0.0);
            let lost = self.lost.get(key).copied().unwrap_or(0);
            let mut note = format!("{} samples x {}", events.len(), period_label(period));
            if lost > 0 {
                note.push_str(&format!(
                    ", {lost} more its ring lost, taken and counted by the kernel and never read"
                ));
            }
            if before > 0.0 {
                note.push_str(&format!(
                    ", and {} the kernel counted (`schedstat`) before the profile found the \
                     task, which no sample covers",
                    period_label(before)
                ));
            }
            (
                Some((events.len() as u64 + lost) as f64 * period + before),
                Some(note),
            )
        } else if events.is_empty() {
            (
                Some(0.0),
                Some(if self.seen.contains(key) {
                    "no event of this key passed the filter".to_owned()
                } else {
                    format!(
                        "no `{}` event of this key, from a source that watched the whole run \
                         and lost nothing",
                        self.source
                    )
                }),
            )
        } else if self.value == SAMPLES {
            let period = self.period_ns.unwrap_or(0.0);
            (
                Some(events.len() as f64 * period),
                Some(format!(
                    "{} samples x {}",
                    events.len(),
                    period_label(period)
                )),
            )
        } else {
            let value = read(&self.value) - self.less.as_deref().map_or(0.0, read);
            let note = match (self.reading, self.less.as_deref()) {
                (Reading::Level, Some(less)) => Some(format!(
                    "`{}` less `{less}`, last reading minus first",
                    self.value
                )),
                (Reading::Level, None) => Some("last reading minus first".to_owned()),
                (Reading::Delta, Some(less)) => {
                    Some(format!("`{}` less `{less}`, summed", self.value))
                }
                (Reading::Delta, None) => None,
            };
            (Some(value), note)
        };

        let resolution = match (self.kind, self.reading) {
            (Kind::Sampled, _) => self.period_ns,
            (Kind::Traced | Kind::Counted, _) => Some(0.0),
            (Kind::Polled, Reading::Delta) if self.unit == Unit::Ns => self.poll_ns,
            _ => None,
        };

        Row {
            state: state.to_owned(),
            source: self.source.to_owned(),
            value: self.value.to_owned(),
            kind: self.kind,
            evidence: Some({
                let by = match self.via {
                    Some(via) => rate(self.by).min(rate(via)),
                    None => rate(self.by),
                };
                self.paired
                    .as_ref()
                    .map_or(by, |(joined, _)| by.min(*joined))
            }),
            amount,
            resolution,
            readings: events.len() as u64,
            short: self.short.clone(),
            missing,
            note,
            named: None,
        }
    }
}

/// What a row read by one key is worth: a propagated request identity is
/// IDENTITY, every other key KEY -- the same ladder `evidence` gives a join.
pub(super) fn rate(key: KeyKind) -> Evidence {
    if key == KeyKind::Req {
        Evidence::Identity
    } else {
        Evidence::Key
    }
}

/// Why a key a source produced no event for cannot be read as a zero, or
/// `None` when it can.
///
/// A source that watched the whole run and lost nothing, and saw nothing of a
/// key, measured that key doing nothing -- a task that never blocked has no
/// off-cpu row, and its blocked time is zero, not unknown. A lossy source may
/// have lost exactly that key's events, and one a window narrowed was not
/// watching for part of the run; neither can say the same, and a zero from
/// either would be a measurement of what it did not see.
pub(super) fn absent_is_not_zero(
    source: &str,
    result: &Results,
    short: Option<&Shortfall>,
    run_closed_ns: u64,
) -> Option<String> {
    let window = result.window;
    if window == Watched::default() {
        return Some(format!(
            "no `{source}` event of this key, and the run did not say when `{source}` was \
             watching, so its absence is not a zero"
        ));
    }
    if !window.attached {
        return Some(format!(
            "no `{source}` event of this key: `{source}` was never attached"
        ));
    }
    if window.opened_ns > window.run_opened_ns || window.closed_ns < run_closed_ns {
        return Some(format!(
            "no `{source}` event of this key, and `{source}` watched only part of the run, so \
             its absence is not a zero"
        ));
    }
    if short.is_some() || !result.losses.is_clean() {
        return Some(format!(
            "no `{source}` event of this key, and `{source}` lost events, so its absence is not \
             a zero"
        ));
    }
    None
}

pub(super) fn period_label(period_ns: f64) -> String {
    format!("{:.3} ms", period_ns / 1e6)
}
