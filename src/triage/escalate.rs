//! The escalation: a run that decides, from its own cheap numbers, which
//! layer's detailed sources to pay for.
//!
//! `docs/ROADMAP.md` phase 10, item 3. `omn latency` pays for every detailed layer
//! for the whole run -- the on-cpu profile, the blocking stacks, the lock
//! sources -- because it does not know in advance where the time will go. An
//! escalation runs a cheap pass instead, and once the cheap pass names the
//! state a task's wall clock is going to, attaches that state's detailed
//! sources and lets go of the rest. The budget holds because the run pays
//! dearly for one layer at a time.
//!
//! **Declared, then decided.** A session holds its detailed sources with a
//! window written `on <layer>` and names the objective in `escalate:`. Every
//! held request is validated, resolved and prepared before the program is
//! launched, like any other; what the run decides is only *when* each one is
//! attached and detached. So an escalation can never reach a backend the
//! resolver did not choose, or a source that answers a question the session
//! did not ask -- a layer this machine cannot serve is reported, with the
//! resolver's reasons, and the cheap answer stands.
//!
//! **The runner asks, this answers.** The runner knows the
//! [`Escalate`](crate::collect::Escalate) trait and nothing behind it: once a
//! round, after its reads, it shows [`Policy`] what has been collected, and
//! applies what comes back -- detaches first, so two detailed layers are never
//! paid for at once.
//!
//! **The cheap pass** is two sources the session runs for the whole of it:
//!
//! - `sched.offcpu`, which says where the main thread blocked and in which
//!   state -- `sleeping` for a lock, a condition or a timer, `uninterruptible`
//!   for a device -- and how long it then waited for a cpu (`queued_ns`);
//! - `cpu.oncpu_time`, which says how much cpu the process burned.
//!
//! The main thread is the one whose id is the process id, for the reason `omn
//! latency` gives: a program's latency is its main thread's life.
//!
//! **A decision** is read every [`DECISION_NS`] over what arrived since the
//! last one. A state is *named* when it holds more than half of the interval
//! and more than any other state; a layer is escalated to when the same state
//! is named twice in a row, so one interval straddling a phase change does not
//! attach a layer for the phase that just ended.

use std::fmt::Write as _;

use crate::collect::{Escalate, Progress, Standing, Switch};
use crate::error::ParseError;
use crate::event::Event;
use crate::request::{Layer, Target};
use crate::resolve::ResolvedRequest;

use super::objective::{list, Objective};
use super::{span, Refusal};

/// How often the cheap pass is read for a decision.
///
/// Two drains of `sched.offcpu` (it drains every 100 ms), so each reading
/// holds whole drains, and short enough that a phase of a second is named
/// within its first half.
pub const DECISION_NS: u64 = 200_000_000;

/// How many readings in a row must name the same state before its layer is
/// escalated to.
pub const READINGS: u32 = 2;

/// The source that says where the main thread blocked, and in which state.
pub const BLOCKED: &str = "sched.offcpu";
/// The source that says how much cpu the process burned.
pub const ON_CPU: &str = "cpu.oncpu_time";

/// The objectives an escalation decides for.
pub const DECIDES_FOR: [Objective; 1] = [Objective::Latency];

/// The objective a session's escalation named, checked. No objective is
/// refused with the list, never defaulted.
pub fn decides_for(objective: Option<Objective>) -> Result<Objective, ParseError> {
    let Some(objective) = objective else {
        return Err(ParseError::new(format!(
            "an escalation needs an objective, and does not pick one: the layer that dominates a \
             task's wall clock is the suspect for its latency, and not for its throughput or its \
             energy. Say which with `escalate: {{objective: ...}}`:\n{}",
            list(&DECIDES_FOR).trim_end()
        )));
    };
    if DECIDES_FOR.contains(&objective) {
        return Ok(objective);
    }
    let why = match objective {
        Objective::Tail => {
            "the tail is what the slowest runs spent that the median did not, which takes several \
             runs to see and cannot be decided inside one; `omn latency --objective tail` runs them"
        }
        Objective::Throughput => {
            "throughput is limited by what the work queues for, across every task, and the cheap \
             pass reads one task's wall clock; `omn saturate --objective throughput` walks the \
             queues"
        }
        Objective::Energy => {
            "a blocked nanosecond costs the cpu no energy, so the layer that dominates a wall \
             clock is not the one that dominates an energy bill; `omn saturate --objective energy` \
             reads the meters"
        }
        Objective::Latency => unreachable!("decided for above"),
    };
    Err(ParseError::new(format!(
        "an escalation does not decide for `{objective}`: {why}. It decides for:\n{}",
        list(&DECIDES_FOR).trim_end()
    )))
}

/// A held request, as the policy knows it.
#[derive(Debug, Clone)]
struct Detailed {
    index: usize,
    layer: Layer,
    label: String,
    /// Set when this machine could not serve it, with the resolver's reason:
    /// it has no index in the run and is never attached.
    unserved: Option<String>,
}

/// One reading of the cheap pass: where the main thread's time went over an
/// interval, as far as the cheap sources can say.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Reading {
    /// How long the interval was.
    pub interval_ns: u64,
    /// The main thread's blocks that ended in the interval, by state. A block
    /// is credited in full when it ends, so a long one can exceed the
    /// interval it ended in.
    pub sleeping_ns: u64,
    pub uninterruptible_ns: u64,
    /// What those blocks then waited for a cpu, after their wakeup.
    pub queued_ns: u64,
    /// What the whole process burned on a cpu, every thread's.
    pub process_cpu_ns: u64,
}

impl Reading {
    /// The main thread's time on a cpu, bounded from both sides the cheap
    /// pass has: no more than the process burned, and no more than the
    /// interval less the main thread's own blocks and waits.
    pub fn on_cpu_ns(&self) -> u64 {
        self.process_cpu_ns.min(
            self.interval_ns
                .saturating_sub(self.sleeping_ns + self.uninterruptible_ns + self.queued_ns),
        )
    }

    fn of(&self, layer: Layer) -> u64 {
        match layer {
            Layer::Cpu => self.on_cpu_ns(),
            Layer::Queue => self.queued_ns,
            Layer::Disk => self.uninterruptible_ns,
            Layer::Lock => self.sleeping_ns,
        }
    }

    /// The state that holds more than half of the interval and more than any
    /// other, when one does.
    pub fn named(&self) -> Option<Layer> {
        let mut ranked: Vec<(Layer, u64)> = Layer::ALL
            .iter()
            .map(|layer| (*layer, self.of(*layer)))
            .collect();
        ranked.sort_by_key(|(_, ns)| std::cmp::Reverse(*ns));
        let (first, ns) = ranked[0];
        let next = ranked[1].1;
        (ns * 2 > self.interval_ns && ns > next).then_some(first)
    }

    /// The reading in one line, the named state first.
    pub fn describe(&self, main: i32) -> String {
        let share = |ns: u64| {
            if self.interval_ns == 0 {
                0.0
            } else {
                ns as f64 * 100.0 / self.interval_ns as f64
            }
        };
        let mut layers = Layer::ALL.to_vec();
        layers.sort_by_key(|layer| std::cmp::Reverse(self.of(*layer)));
        let states: Vec<String> = layers
            .iter()
            .map(|layer| {
                let ns = self.of(*layer);
                let qualifier = if *layer == Layer::Cpu { "at most " } else { "" };
                format!(
                    "{} {qualifier}{} ({:.0} %)",
                    layer.state(),
                    span(ns as f64),
                    share(ns)
                )
            })
            .collect();
        format!(
            "over the last {}, the main thread (task {main}) was {}; the process burned {} on a \
             cpu, every thread's",
            span(self.interval_ns as f64),
            states.join(", "),
            span(self.process_cpu_ns as f64),
        )
    }
}

/// One decision the escalation took.
#[derive(Debug, Clone)]
struct Decision {
    at_s: f64,
    layer: Layer,
    reading: String,
    attached: Vec<String>,
    detached: Vec<String>,
    /// Held requests of the named layer that could not be attached, and why.
    unserved: Vec<String>,
}

/// The escalation's policy: thread state analysis over the cheap pass.
#[derive(Debug)]
pub struct Policy {
    objective: Objective,
    blocked: usize,
    on_cpu: usize,
    detailed: Vec<Detailed>,
    /// How many events of each request have been read into a reading.
    consumed: Vec<usize>,
    last_ns: Option<u64>,
    streak: (Option<Layer>, u32),
    current: Option<Layer>,
    decisions: Vec<Decision>,
    readings: u32,
    shed: Vec<String>,
}

impl Policy {
    /// The policy for a run of these requests, or why the escalation cannot
    /// decide anything from them.
    pub fn for_run(resolved: &[ResolvedRequest]) -> Result<Policy, Refusal> {
        Self::for_run_with(resolved, &[])
    }

    /// The same, told of held requests this machine could not serve: they
    /// are layers it lacks, reported with the resolver's reason if their
    /// layer is named, and never attached.
    pub fn for_run_with(
        resolved: &[ResolvedRequest],
        unserved: &[crate::collect::Unserved],
    ) -> Result<Policy, Refusal> {
        let held: Vec<(usize, &crate::request::Request, Layer, Option<Objective>)> = resolved
            .iter()
            .enumerate()
            .filter_map(|(index, one)| {
                let held = one.request.window?.on?;
                Some((index, &one.request, held.layer, held.objective))
            })
            .collect();
        let lacking: Vec<(&crate::request::Request, Layer, Option<Objective>, &str)> = unserved
            .iter()
            .filter_map(|one| {
                let held = one.request.window?.on?;
                Some((
                    &one.request,
                    held.layer,
                    held.objective,
                    one.reason.as_str(),
                ))
            })
            .collect();
        let objective = held
            .iter()
            .map(|(_, _, _, objective)| *objective)
            .chain(lacking.iter().map(|(_, _, objective, _)| *objective))
            .find_map(|objective| objective)
            .map_or_else(
                || decides_for(None),
                |objective| decides_for(Some(objective)),
            )
            .map_err(|error| Refusal(error.message().to_owned()))?;

        let workload = &held
            .first()
            .map(|(_, request, _, _)| *request)
            .or_else(|| lacking.first().map(|(request, _, _, _)| *request))
            .ok_or_else(|| Refusal("no request is held for the escalation".to_owned()))?
            .target;
        match workload {
            Target::Launch(_) => {}
            Target::Pid(pids) if pids.len() == 1 => {}
            other => {
                return Err(Refusal(format!(
                "it decides from one task's wall clock -- the main thread of one process -- and \
                     a `{}` target names no main thread: launch the program with `-- PROGRAM`, or \
                     name one process with `--pid`",
                other.kind()
            )))
            }
        }

        // The cheap pass: requests of the workload that run for the whole of
        // it, neither held nor windowed, since a reading over a span one of
        // them did not watch would read a gap as an idle thread.
        let cheap = |source: &str| {
            resolved.iter().position(|one| {
                one.request.source.as_str() == source
                    && one.request.window.is_none()
                    && &one.request.target == workload
            })
        };
        let (blocked, on_cpu) = match (cheap(BLOCKED), cheap(ON_CPU)) {
            (Some(blocked), Some(on_cpu)) => (blocked, on_cpu),
            (blocked, on_cpu) => {
                let missing: Vec<&str> = [(blocked, BLOCKED), (on_cpu, ON_CPU)]
                    .into_iter()
                    .filter_map(|(found, source)| found.is_none().then_some(source))
                    .collect();
                return Err(Refusal(format!(
                    "it decides from a cheap pass over the whole run -- `{BLOCKED}`, where the main \
                     thread blocked and in which state, and `{ON_CPU}`, how much cpu the process \
                     burned -- and this session does not run `{}` on the workload for the whole \
                     run, without a window",
                    missing.join("` or `")
                )));
            }
        };

        let label = |request: &crate::request::Request| {
            request
                .label
                .clone()
                .unwrap_or_else(|| request.source.to_string())
        };
        let detailed = held
            .iter()
            .map(|(index, request, layer, _)| Detailed {
                index: *index,
                layer: *layer,
                label: label(request),
                unserved: None,
            })
            .chain(lacking.iter().map(|(request, layer, _, reason)| Detailed {
                index: usize::MAX,
                layer: *layer,
                label: label(request),
                unserved: Some(format!("this machine cannot serve it: {reason}")),
            }))
            .collect();
        Ok(Policy {
            objective,
            blocked,
            on_cpu,
            detailed,
            consumed: vec![0; resolved.len()],
            last_ns: None,
            streak: (None, 0),
            current: None,
            decisions: Vec::new(),
            readings: 0,
            shed: Vec::new(),
        })
    }

    /// The main thread: the process the workload is bound to.
    fn main(&self, run: &Progress<'_>) -> Option<i32> {
        run.bound.get(self.blocked).and_then(Target::first_pid)
    }

    /// Reads what arrived since the last reading.
    fn read(&mut self, run: &Progress<'_>, main: i32, interval_ns: u64) -> Reading {
        let mut reading = Reading {
            interval_ns,
            ..Reading::default()
        };
        for event in fresh(run.events, &mut self.consumed, self.blocked) {
            if uint(event, "tid") != Some(main as u64) {
                continue;
            }
            let blocked_ns = uint(event, "blocked_ns").unwrap_or(0);
            let state = match event.attr("state") {
                Some(crate::event::AttrValue::Text(state)) => Some(state.as_str()),
                _ => None,
            };
            match state {
                Some("sleeping") => reading.sleeping_ns += blocked_ns,
                Some("uninterruptible") => reading.uninterruptible_ns += blocked_ns,
                _ => {}
            }
            reading.queued_ns += uint(event, "queued_ns").unwrap_or(0);
        }
        for event in fresh(run.events, &mut self.consumed, self.on_cpu) {
            reading.process_cpu_ns += uint(event, "ns").unwrap_or(0);
        }
        reading
    }

    /// What escalating to `layer` asks of the runner, and the decision it is.
    fn escalate(&mut self, run: &Progress<'_>, layer: Layer, reading: String) -> Vec<Switch> {
        let at_s = run.now_ns.saturating_sub(run.opened_ns) as f64 / 1e9;
        let objective = self.objective;
        let mut switches = Vec::new();
        let mut decision = Decision {
            at_s,
            layer,
            reading: reading.clone(),
            attached: Vec::new(),
            detached: Vec::new(),
            unserved: Vec::new(),
        };
        for one in &self.detailed {
            if let Some(why) = &one.unserved {
                if one.layer == layer {
                    decision.unserved.push(format!("`{}`: {why}", one.label));
                }
                continue;
            }
            let standing = run.standing.get(one.index);
            if one.layer != layer {
                if standing == Some(&Standing::Attached) {
                    decision.detached.push(one.label.clone());
                    switches.push(Switch {
                        index: one.index,
                        attach: false,
                        why: format!(
                            "for {objective}, the cheap pass named the `{layer}` layer ({reading}), \
                             so the `{}` layer's detailed sources were let go: the run pays for \
                             one detailed layer at a time",
                            one.layer
                        ),
                    });
                }
                continue;
            }
            match standing {
                Some(Standing::Waiting) => {
                    decision.attached.push(one.label.clone());
                    switches.push(Switch {
                        index: one.index,
                        attach: true,
                        why: format!(
                            "for {objective}, the cheap pass named the `{layer}` layer in {READINGS} \
                             readings in a row -- {reading}"
                        ),
                    });
                }
                Some(Standing::Attached) => {}
                Some(Standing::Detached) => decision.unserved.push(format!(
                    "`{}` was attached once already and let go, and a request has one window",
                    one.label
                )),
                Some(Standing::Missed(why)) => {
                    decision.unserved.push(format!("`{}`: {why}", one.label))
                }
                None => {}
            }
        }
        self.decisions.push(decision);
        switches
    }
}

/// The events of request `index` that have arrived since the last call.
fn fresh<'a>(events: &'a [Vec<Event>], consumed: &mut [usize], index: usize) -> &'a [Event] {
    let all = events.get(index).map_or(&[][..], Vec::as_slice);
    let from = consumed[index].min(all.len());
    consumed[index] = all.len();
    &all[from..]
}

fn uint(event: &Event, name: &str) -> Option<u64> {
    event
        .attr(name)
        .and_then(|value| value.as_f64())
        .map(|value| value as u64)
}

impl Escalate for Policy {
    fn decide(&mut self, run: &Progress<'_>) -> Vec<Switch> {
        let Some(last_ns) = self.last_ns else {
            // The first reading is over the run's first interval, from the
            // moment it opened.
            self.last_ns = Some(run.opened_ns);
            return Vec::new();
        };
        let interval_ns = run.now_ns.saturating_sub(last_ns);
        if interval_ns < DECISION_NS {
            return Vec::new();
        }
        let Some(main) = self.main(run) else {
            return Vec::new();
        };
        self.last_ns = Some(run.now_ns);
        let reading = self.read(run, main, interval_ns);
        self.readings += 1;
        let named = reading.named();
        self.streak = match self.streak {
            (layer, count) if layer == named => (layer, count + 1),
            _ => (named, 1),
        };
        match self.streak {
            (Some(layer), count) if count >= READINGS && self.current != Some(layer) => {
                self.current = Some(layer);
                let described = reading.describe(main);
                self.escalate(run, layer, described)
            }
            _ => Vec::new(),
        }
    }

    fn shed(&mut self, index: usize, over: &str) -> Option<String> {
        let one = self.detailed.iter().find(|one| one.index == index)?;
        let why = format!(
            "the `{}` layer's `{}` was attached by the escalation and {over}; it was let go and \
             the cheap pass goes on, so the cheap answer stands for the rest of the run",
            one.layer, one.label
        );
        self.shed.push(one.label.clone());
        Some(why)
    }

    fn close(&mut self, _run: &Progress<'_>) -> Vec<(usize, String)> {
        let objective = self.objective;
        let mut summary = String::new();
        if self.decisions.is_empty() {
            let _ = write!(
                summary,
                "the escalation, for {objective}, named no layer in {} reading(s) of the cheap \
                 pass: no state of the main thread held more than half of {READINGS} readings in a \
                 row, so no detailed source was attached and the cheap answer is the whole answer",
                self.readings
            );
        } else {
            let _ = write!(summary, "the escalation, for {objective}, named ");
            let named: Vec<String> = self
                .decisions
                .iter()
                .map(|decision| {
                    let mut one = format!("`{}` at {:.3} s", decision.layer, decision.at_s);
                    if !decision.attached.is_empty() {
                        let _ = write!(one, ", attaching `{}`", decision.attached.join("`, `"));
                    }
                    if !decision.detached.is_empty() {
                        let _ = write!(
                            one,
                            " and letting go of `{}`",
                            decision.detached.join("`, `")
                        );
                    }
                    if !decision.unserved.is_empty() && !decision.attached.is_empty() {
                        let _ = write!(one, " (not served: {})", decision.unserved.join("; "));
                    }
                    one
                })
                .collect();
            summary.push_str(&named.join("; then "));
        }
        let mut notes = vec![(self.blocked, summary)];
        for decision in &self.decisions {
            if !decision.attached.is_empty() {
                continue;
            }
            let why = if !self.detailed.iter().any(|one| one.layer == decision.layer) {
                "this session holds no detailed source for it".to_owned()
            } else if decision.unserved.is_empty() {
                continue;
            } else {
                decision.unserved.join("; ")
            };
            notes.push((
                self.blocked,
                format!(
                    "the `{}` layer, named at {:.3} s, could not be served: {why}. The cheap answer \
                     stands, and it is only what the cheap pass can say: {}",
                    decision.layer, decision.at_s, decision.reading
                ),
            ));
        }
        for label in &self.shed {
            notes.push((
                self.blocked,
                format!("`{label}` was let go to hold the budget, and the cheap pass went on"),
            ));
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(sleeping_ms: u64, disk_ms: u64, queued_ms: u64, cpu_ms: u64) -> Reading {
        Reading {
            interval_ns: 200_000_000,
            sleeping_ns: sleeping_ms * 1_000_000,
            uninterruptible_ns: disk_ms * 1_000_000,
            queued_ns: queued_ms * 1_000_000,
            process_cpu_ns: cpu_ms * 1_000_000,
        }
    }

    #[test]
    fn a_state_is_named_only_when_it_holds_more_than_half_and_more_than_any_other() {
        assert_eq!(reading(0, 0, 0, 200).named(), Some(Layer::Cpu));
        assert_eq!(reading(180, 0, 2, 200).named(), Some(Layer::Lock));
        assert_eq!(reading(0, 150, 0, 40).named(), Some(Layer::Disk));
        assert_eq!(reading(0, 0, 120, 70).named(), Some(Layer::Queue));
        // Half is not more than half.
        assert_eq!(reading(100, 0, 0, 100).named(), None);
        // Nothing measured is nothing named.
        assert_eq!(reading(0, 0, 0, 0).named(), None);
    }

    #[test]
    fn a_busy_process_is_not_a_busy_main_thread() {
        // Another thread spins a whole cpu while the main thread sleeps: the
        // process burned 200 ms, and the main thread was on a cpu at most for
        // what its own blocks left of the interval.
        let other_thread_spins = reading(190, 0, 1, 200);
        assert_eq!(other_thread_spins.on_cpu_ns(), 9_000_000);
        assert_eq!(other_thread_spins.named(), Some(Layer::Lock));
    }

    #[test]
    fn an_escalation_without_an_objective_is_refused_with_the_list() {
        let error = decides_for(None).unwrap_err();
        assert!(error.message().contains("needs an objective"), "{error}");
        assert!(error.message().contains("latency"), "{error}");
    }

    #[test]
    fn an_objective_the_escalation_cannot_decide_names_the_command_that_answers_it() {
        for (objective, command) in [
            (Objective::Tail, "omn latency --objective tail"),
            (Objective::Throughput, "omn saturate --objective throughput"),
            (Objective::Energy, "omn saturate --objective energy"),
        ] {
            let error = decides_for(Some(objective)).unwrap_err();
            assert!(error.message().contains(command), "{error}");
        }
        assert_eq!(
            decides_for(Some(Objective::Latency)),
            Ok(Objective::Latency)
        );
    }

    /// A held request this machine could not serve is a layer it lacks: when
    /// the cheap pass names that layer, the escalation attaches what it can
    /// of it and says which could not be served, in the resolver's words.
    #[test]
    fn a_named_layer_this_machine_cannot_serve_is_reported_with_the_resolver_s_reason() {
        let mut policy = Policy {
            objective: Objective::Latency,
            blocked: 0,
            on_cpu: 1,
            detailed: vec![
                Detailed {
                    index: 2,
                    layer: Layer::Cpu,
                    label: "profile".to_owned(),
                    unserved: None,
                },
                Detailed {
                    index: usize::MAX,
                    layer: Layer::Cpu,
                    label: "flow".to_owned(),
                    unserved: Some(
                        "this machine cannot serve it: no `intel_pt` performance unit".to_owned(),
                    ),
                },
            ],
            consumed: vec![0; 3],
            last_ns: None,
            streak: (None, 0),
            current: None,
            decisions: Vec::new(),
            readings: 0,
            shed: Vec::new(),
        };
        let events = vec![Vec::new(), Vec::new(), Vec::new()];
        let standing = vec![Standing::Attached, Standing::Attached, Standing::Waiting];
        let bound = vec![Target::Pid(vec![1]); 3];
        let run = Progress {
            opened_ns: 0,
            now_ns: 1_000_000_000,
            events: &events,
            standing: &standing,
            bound: &bound,
        };
        let switches = policy.escalate(&run, Layer::Cpu, "on a cpu".to_owned());
        assert_eq!(
            switches.len(),
            1,
            "the served request is attached, and only it"
        );
        assert_eq!(switches[0].index, 2);
        let notes = policy.close(&run);
        let summary = &notes[0].1;
        assert!(
            summary.contains("not served") && summary.contains("intel_pt"),
            "{summary}"
        );
    }
}
