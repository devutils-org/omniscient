//! The `conf` family: the settings beside a measurement.
//!
//! `conf.machine` and `conf.process` read the table of `catalog/settings.yaml`
//! once; `conf.changed` reads it at every poll and says, at the end, what
//! differs between the first reading and the last. Every one of them only
//! reads (`docs/DECISIONS.md`, boundary 5, and "Settled in the thirty-first
//! wave").

pub mod settings;

use std::collections::BTreeMap;

use super::{Collector, Factory, Prepared, SampleError};
use crate::clock::monotonic_ns;
use crate::event::{Attr, CollectorId, Event};
use crate::request::{Mode, Target};

pub use settings::{Reading, Scope, State};

pub fn factories() -> Vec<Factory> {
    vec![
        Factory::new("conf.machine", "fs:proc", |_| {
            Ok(Box::new(Prepare(Which::Machine)))
        }),
        Factory::new("conf.process", "fs:proc", |_| {
            Ok(Box::new(Prepare(Which::Process)))
        }),
        Factory::new("conf.changed", "fs:proc", |resolved| {
            Ok(Box::new(Prepare(Which::Changed(resolved.request.mode))))
        }),
    ]
}

#[derive(Debug, Clone, Copy)]
enum Which {
    Machine,
    Process,
    Changed(Mode),
}

struct Prepare(Which);

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let pid = match target {
            Target::System => None,
            other => Some(other.first_pid().ok_or_else(|| {
                format!(
                    "a process's settings are read per process, and a {} target names none",
                    other.kind()
                )
            })?),
        };
        Ok(match self.0 {
            Which::Machine => Box::new(Once {
                kind: "conf.machine",
                pid: None,
                done: false,
                settling: Settling::default(),
            }),
            Which::Process => Box::new(Once {
                kind: "conf.process",
                pid: Some(pid.ok_or("`conf.process` needs a process")?),
                done: false,
                settling: Settling::default(),
            }),
            Which::Changed(mode) => Box::new(Changed {
                pid,
                interval: match mode {
                    Mode::Poll { interval } => std::time::Duration::from_nanos(interval.as_nanos()),
                    _ => std::time::Duration::from_secs(1),
                },
                first: BTreeMap::new(),
                last: BTreeMap::new(),
                first_ns: 0,
                changed_at: BTreeMap::new(),
                reads: 0,
                settling: Settling::default(),
            }),
        })
    }
}

/// How long a process's executable must stay the same before it is read,
/// and the longest a read waits for that.
const SETTLED_NS: u64 = 2_000_000;
const SETTLE_AT_MOST_NS: u64 = 250_000_000;

/// Whether `pid` has stopped exec'ing, looked at without waiting: its
/// executable the same as at the last look, [`SETTLED_NS`] or more ago. A
/// launched program is often a chain -- `taskset -c 0 nice -n 7 choom -n
/// 345 -- prog` -- each link setting what the next inherits, and a read in
/// the middle of it reads a wrapper's settings as the program's: the
/// affinity before `taskset` applied it. After [`SETTLE_AT_MOST_NS`], or
/// with the process gone, it is read as it is.
///
/// Not a sleep: a collector that holds up the round holds up every other's
/// reads, and a stall of 2 ms at a run's start was enough to make `omn`
/// the OOM killer's choice in a cgroup it shared with the program.
#[derive(Debug, Default)]
struct Settling {
    first_ns: Option<u64>,
    last: Option<(u64, Option<std::path::PathBuf>)>,
}

impl Settling {
    fn settled(&mut self, pid: i32) -> bool {
        let now = crate::clock::monotonic_ns();
        let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok();
        let first = *self.first_ns.get_or_insert(now);
        if exe.is_none() || now.saturating_sub(first) >= SETTLE_AT_MOST_NS {
            return true;
        }
        match &self.last {
            Some((at, seen)) if *seen == exe => now.saturating_sub(*at) >= SETTLED_NS,
            _ => {
                self.last = Some((now, exe));
                false
            }
        }
    }
}

/// Every setting one read of the target finds: the machine's, and the
/// process's when there is one.
fn read_all(pid: Option<i32>, machine: bool) -> Result<(Vec<Reading>, usize), SampleError> {
    let mut readings = if machine {
        settings::read_machine()
    } else {
        Vec::new()
    };
    let mut unread = 0;
    if let Some(pid) = pid {
        // A process that exited and is not reaped yet still has a directory,
        // and its files no longer describe it: a zombie's status has no
        // `THP_enabled`, and its limits are nobody's.
        let state = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|text| settings::stat_field(&text, 3));
        if !matches!(state.as_deref(), Some(s) if s != "Z" && s != "X") {
            return Err(SampleError::TargetGone(format!("process {pid} is gone")));
        }
        let (process, beyond) = settings::read_process(pid);
        readings.extend(process);
        unread = beyond;
    }
    Ok((readings, unread))
}

/// One reading as an event.
pub fn event_of(kind: &str, reading: &Reading, ts_ns: u64, seq: u64) -> Event {
    let mut attrs = vec![
        Attr::text("setting", reading.setting.clone()),
        Attr::text("scope", reading.scope.as_str()),
        Attr::text("state", reading.state.as_str()),
        Attr::text("read_from", reading.read_from.clone()),
    ];
    if let Some(value) = &reading.value {
        attrs.push(Attr::text("value", value.clone()));
    }
    if let Some(instance) = &reading.instance {
        attrs.push(Attr::text("instance", instance.clone()));
    }
    if let Some(binding) = &reading.binding {
        attrs.push(Attr::text("binding", binding.clone()));
    }
    if let Some(value) = &reading.binding_value {
        attrs.push(Attr::text("binding_value", value.clone()));
    }
    if let Some(hard) = &reading.hard {
        attrs.push(Attr::text("hard", hard.clone()));
    }
    if let Some(reason) = &reading.reason {
        attrs.push(Attr::text("reason", reason.clone()));
    }
    Event {
        ts_ns,
        ts_err_ns: 0,
        kind: kind.to_owned(),
        keys: Vec::new(),
        attrs,
        collector_id: CollectorId::unstamped(),
        seq,
    }
}

/// Reads back what [`event_of`] wrote.
pub fn reading_of(event: &Event) -> Option<Reading> {
    let text = |name: &str| {
        event.attr(name).map(|value| match value {
            crate::event::AttrValue::Text(text) => text.clone(),
            other => other.to_string(),
        })
    };
    let scope = match text("scope")?.as_str() {
        "machine" => Scope::Machine,
        "process" => Scope::Process,
        "thread" => Scope::Thread,
        "cgroup" => Scope::Cgroup,
        _ => return None,
    };
    let state = match text("state")?.as_str() {
        "set" => State::Set,
        "unset" => State::Unset,
        "absent" => State::Absent,
        _ => State::Unknown,
    };
    Some(Reading {
        setting: text("setting")?,
        scope,
        instance: text("instance"),
        state,
        value: text("value"),
        binding: text("binding"),
        binding_value: text("binding_value"),
        hard: text("hard"),
        read_from: text("read_from").unwrap_or_default(),
        reason: text("reason"),
    })
}

/// `conf.machine` and `conf.process`: one read.
struct Once {
    kind: &'static str,
    pid: Option<i32>,
    done: bool,
    settling: Settling,
}

impl Collector for Once {
    fn value_attr(&self) -> &'static str {
        "value"
    }

    fn ready(&mut self) -> bool {
        match self.pid {
            Some(pid) => self.settling.settled(pid),
            None => true,
        }
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        if self.done {
            return Ok(Vec::new());
        }
        self.done = true;
        let (readings, unread) = read_all(self.pid, self.pid.is_none())?;
        let ts_ns = monotonic_ns();
        let mut events: Vec<Event> = readings
            .iter()
            .enumerate()
            .map(|(index, reading)| event_of(self.kind, reading, ts_ns, seq + index as u64))
            .collect();
        if unread > 0 {
            events.push(Event {
                ts_ns,
                ts_err_ns: 0,
                kind: self.kind.to_owned(),
                keys: Vec::new(),
                attrs: vec![
                    Attr::text("setting", "threads_unread"),
                    Attr::text("state", "unknown"),
                    Attr::uint("threads_unread", unread as u64),
                    Attr::text(
                        "reason",
                        format!(
                            "{unread} threads beyond the first {} were not read",
                            settings::THREADS_READ
                        ),
                    ),
                ],
                collector_id: CollectorId::unstamped(),
                seq: seq + events.len() as u64,
            });
        }
        Ok(events)
    }
}

/// `conf.changed`: every read kept as first and last, and the differences
/// emitted at the stop.
struct Changed {
    pid: Option<i32>,
    interval: std::time::Duration,
    first: BTreeMap<(String, Option<String>), Reading>,
    last: BTreeMap<(String, Option<String>), Reading>,
    first_ns: u64,
    /// When each setting was first seen to differ from its first reading.
    changed_at: BTreeMap<(String, Option<String>), u64>,
    reads: u64,
    settling: Settling,
}

impl Changed {
    fn take(&mut self) -> Result<(), SampleError> {
        // The baseline is the program's, not a wrapper's on the way to it: a
        // poll before the chain has settled is skipped.
        if let (Some(pid), true) = (self.pid, self.first.is_empty()) {
            if !self.settling.settled(pid) {
                return Ok(());
            }
        }
        let (readings, _) = read_all(self.pid, true)?;
        let ts_ns = monotonic_ns();
        self.reads += 1;
        if self.first.is_empty() {
            self.first_ns = ts_ns;
            for reading in &readings {
                self.first.insert(reading.key(), reading.clone());
            }
        }
        for reading in readings {
            let key = reading.key();
            if let Some(first) = self.first.get(&key) {
                if differs(first, &reading) {
                    self.changed_at.entry(key.clone()).or_insert(ts_ns);
                }
            }
            self.last.insert(key, reading);
        }
        Ok(())
    }
}

fn differs(a: &Reading, b: &Reading) -> bool {
    a.state != b.state || a.value != b.value || a.binding_value != b.binding_value
}

impl Collector for Changed {
    fn value_attr(&self) -> &'static str {
        "value"
    }

    fn tick(&self) -> Option<std::time::Duration> {
        Some(self.interval)
    }

    fn sample(&mut self, _seq: u64) -> Result<Vec<Event>, SampleError> {
        match self.take() {
            // A launched program that exited ends the process half; the
            // readings already taken stand.
            Err(SampleError::TargetGone(_)) if !self.first.is_empty() => {
                self.pid = None;
                Ok(Vec::new())
            }
            Err(error) => Err(error),
            Ok(()) => Ok(Vec::new()),
        }
    }

    fn stop(&mut self) -> Vec<Event> {
        // The closing read, unless the process is gone: then the last
        // reading taken while it ran is its end.
        let _ = self.take();
        let ts_ns = monotonic_ns();
        let mut events = Vec::new();
        for (key, first) in &self.first {
            let Some(last) = self.last.get(key) else {
                continue;
            };
            if !differs(first, last) || settings::is_computed(&key.0) {
                continue;
            }
            let mut event = event_of("conf.changed", last, ts_ns, events.len() as u64);
            event.attrs.push(Attr::text(
                "before",
                first
                    .effective()
                    .map(str::to_owned)
                    .unwrap_or_else(|| first.state.as_str().to_owned()),
            ));
            event.attrs.push(Attr::text(
                "after",
                last.effective()
                    .map(str::to_owned)
                    .unwrap_or_else(|| last.state.as_str().to_owned()),
            ));
            if let Some(at) = self.changed_at.get(key) {
                event.attrs.push(Attr::uint(
                    "changed_after_ns",
                    at.saturating_sub(self.first_ns),
                ));
            }
            events.push(event);
        }
        let changed = events.len() as u64;
        events.push(Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "conf.changed".to_owned(),
            keys: Vec::new(),
            attrs: vec![
                Attr::text("setting", "watched"),
                Attr::uint("watched", self.first.len() as u64),
                Attr::uint("changed", changed),
                Attr::uint("reads", self.reads),
            ],
            collector_id: CollectorId::unstamped(),
            seq: changed,
        });
        events
    }
}
