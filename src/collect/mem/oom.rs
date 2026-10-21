//! `mem.oom` over `/proc/vmstat` and a cgroup's `memory.events`.
//!
//! An OOM kill is the one memory event a program cannot miss and a person
//! reading its exit status can: it ends in SIGKILL, with nothing written by
//! the program itself. The kernel counts every kill, machine-wide in
//! `/proc/vmstat`'s `oom_kill` and per cgroup in `memory.events`, where it
//! also counts the times the cgroup ran out (`oom`, which a successful reclaim
//! can end without a kill) and the times it touched its hard limit (`max`).
//!
//! These are counts. Which task died and what it held are in the
//! `oom:mark_victim` tracepoint, which `victims.rs` reads in trace mode; these
//! name no victim.
//!
//! `memory.events` is hierarchical: a cgroup's counts include every cgroup
//! below it, which is what a target that names a service wants. The root has
//! no `memory.events`, and being the top of that hierarchy its kills are the
//! machine's: a target in the root cgroup is read from `/proc/vmstat`, and
//! says so, with `oom` and `max` not counted.

use std::path::PathBuf;

use super::cgroup::{stat_field, stat_fields, Cgroup};
use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::request::{Mode, Target};

const VMSTAT: &str = "/proc/vmstat";

/// Where the kills are counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Machine,
    Cgroup,
}

impl Scope {
    /// The counters read, the kill count first.
    fn fields(self) -> &'static [&'static str] {
        match self {
            Scope::Machine => &["oom_kill"],
            Scope::Cgroup => &["oom_kill", "oom", "oom_group_kill", "max"],
        }
    }
}

pub struct Prepare {
    mode: Mode,
    scope: Scope,
}

impl Prepare {
    pub fn new(scope: Scope, mode: Mode) -> Result<Self, String> {
        match mode {
            Mode::Poll { .. } | Mode::Count => Ok(Self { mode, scope }),
            other => Err(format!(
                "OOM kills are counted by the kernel and read here: `{}` cannot be served \
                 from a counter; poll it or count it over the window",
                other.kind()
            )),
        }
    }
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let mut note = None;
        let mut scope = self.scope;
        let (path, keys) = match self.scope {
            Scope::Machine => match target {
                Target::System => (PathBuf::from(VMSTAT), Vec::new()),
                other => {
                    return Err(format!(
                        "{VMSTAT} counts every kill on the machine, and a {} target is not the \
                         machine: the kills of one cgroup are in its `memory.events`, which is \
                         the `fs:cgroup` backend",
                        other.kind()
                    ))
                }
            },
            Scope::Cgroup => {
                let cgroup = Cgroup::resolve(target)?;
                if cgroup.is_root() {
                    scope = Scope::Machine;
                    note = Some(format!(
                        "the {} target sits in the root cgroup, which has no `memory.events`: its \
                         kills are the machine's, read from {VMSTAT}'s `oom_kill`, and `oom`, \
                         `oom_group_kill` and `max` are not counted",
                        target.kind()
                    ));
                    (PathBuf::from(VMSTAT), vec![cgroup.key()])
                } else {
                    let file = cgroup.file("memory.events");
                    let text = std::fs::read_to_string(&file)
                        .map_err(|error| format!("cannot read {}: {error}", file.display()))?;
                    if stat_field(&stat_fields(&text), "oom_kill").is_none() {
                        return Err(format!(
                            "{} carries no `oom_kill` line, so this kernel does not count kills \
                         per cgroup",
                            file.display()
                        ));
                    }
                    (file, vec![cgroup.key()])
                }
            }
        };
        Ok(Box::new(Oom {
            mode: self.mode,
            scope,
            path,
            keys,
            note,
            previous: None,
            opened: None,
            losses: Losses::default(),
        }))
    }
}

/// The counters at one instant, in [`Scope::fields`] order.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Reading {
    ts_ns: u64,
    values: Vec<Option<u64>>,
}

pub struct Oom {
    mode: Mode,
    scope: Scope,
    path: PathBuf,
    keys: Vec<KeyValue>,
    /// Said once per run when the cgroup asked for is read another way.
    note: Option<String>,
    previous: Option<Reading>,
    opened: Option<Reading>,
    losses: Losses,
}

impl Oom {
    fn read(&self) -> Result<Reading, SampleError> {
        let text = std::fs::read_to_string(&self.path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                SampleError::TargetGone(format!("{} is gone", self.path.display()))
            } else {
                SampleError::Transient(format!("{}: {error}", self.path.display()))
            }
        })?;
        let fields = stat_fields(&text);
        Ok(Reading {
            ts_ns: monotonic_ns(),
            values: self
                .scope
                .fields()
                .iter()
                .map(|name| stat_field(&fields, name))
                .collect(),
        })
    }

    fn event(&mut self, before: &Reading, after: &Reading, seq: u64, basis: &str) -> Option<Event> {
        let mut attrs = Vec::with_capacity(self.scope.fields().len() + 3);
        for (index, name) in self.scope.fields().iter().enumerate() {
            let (Some(Some(now)), Some(Some(then))) =
                (after.values.get(index), before.values.get(index))
            else {
                continue;
            };
            let Some(moved) = now.checked_sub(*then) else {
                self.losses.undecoded += 1;
                self.losses.note_times(
                    format!(
                        "{name} in {} went backwards, so that interval is not reported",
                        self.path.display()
                    ),
                    1,
                );
                return None;
            };
            let attr = if *name == "oom_kill" { "kills" } else { name };
            attrs.push(Attr::uint(attr, moved));
        }
        attrs.push(Attr::uint(
            "interval_ns",
            after.ts_ns.saturating_sub(before.ts_ns),
        ));
        attrs.push(Attr::text("basis", basis));
        Some(Event {
            ts_ns: after.ts_ns,
            ts_err_ns: 0,
            kind: "mem.oom".to_owned(),
            keys: self.keys.clone(),
            attrs,
            collector_id: CollectorId::unstamped(),
            seq,
        })
    }
}

impl Collector for Oom {
    fn value_attr(&self) -> &'static str {
        "kills"
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let now = self.read()?;
        if self.mode == Mode::Count {
            self.opened.get_or_insert_with(|| now.clone());
            self.previous = Some(now);
            return Ok(Vec::new());
        }
        let before = self.previous.replace(now.clone());
        Ok(before
            .and_then(|before| {
                self.event(
                    &before,
                    &now,
                    seq,
                    "the kill counter's increase over this tick",
                )
            })
            .into_iter()
            .collect())
    }

    /// A counted run's one event, from the first read to the last; a final
    /// read here as well, because the kill that ends a launched program lands
    /// after the last tick saw it alive.
    fn stop(&mut self) -> Vec<Event> {
        if self.mode != Mode::Count {
            return Vec::new();
        }
        if let Ok(now) = self.read() {
            self.previous = Some(now);
        }
        let (Some(opened), Some(closed)) = (self.opened.clone(), self.previous.clone()) else {
            return Vec::new();
        };
        self.event(
            &opened,
            &closed,
            u64::MAX / 2,
            "the kill counter's increase over the window",
        )
        .into_iter()
        .collect()
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    fn notes(&self) -> Vec<String> {
        self.note.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::mem::testing::Scratch;
    use std::time::Duration;

    fn counted(scope: Scope, path: PathBuf) -> Oom {
        Oom {
            mode: Mode::Count,
            scope,
            path,
            keys: Vec::new(),
            note: None,
            previous: None,
            opened: None,
            losses: Losses::default(),
        }
    }

    /// The root cgroup has no `memory.events`; a target placed there (a
    /// launched program on a machine where nothing moved it) is the machine's
    /// kills, read from `/proc/vmstat`, and the run says the cgroup's other
    /// counters are not there.
    #[test]
    fn a_target_in_the_root_cgroup_reads_the_machine_s_kills_and_says_so() {
        let prepared = Box::new(Prepare {
            mode: Mode::Count,
            scope: Scope::Cgroup,
        });
        let collector = prepared
            .attach(&Target::Cgroup(PathBuf::from("/")))
            .expect("the root cgroup is served");
        let notes = collector.notes();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].contains("root cgroup") && notes[0].contains(VMSTAT),
            "{notes:?}"
        );
    }

    #[test]
    fn the_machine_counter_reads_and_a_quiet_window_counts_none() {
        let mut collector = counted(Scope::Machine, PathBuf::from(VMSTAT));
        collector.sample(0).expect("vmstat");
        let events = collector.stop();
        // Another process on the machine could be killed in between; what is
        // held is that the counter is found and the window is reported.
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(events[0].attr("kills").is_some());
    }

    #[test]
    fn a_counter_that_goes_backwards_is_a_loss_not_a_negative() {
        let mut collector = counted(Scope::Cgroup, PathBuf::from("/nonexistent"));
        let before = Reading {
            ts_ns: 0,
            values: vec![Some(5), Some(5), Some(0), Some(9)],
        };
        let after = Reading {
            ts_ns: 1,
            values: vec![Some(3), Some(5), Some(0), Some(9)],
        };
        assert!(collector.event(&before, &after, 0, "test").is_none());
        assert_eq!(collector.losses.undecoded, 1);
    }

    /// A child that touches more than its cgroup may hold, with no swap to
    /// push it to, is killed by the kernel. Held against the child's own end:
    /// it died of SIGKILL, which nothing in this test sends before the count.
    #[test]
    fn a_child_killed_for_memory_is_one_kill_in_its_cgroup() {
        let Some(scratch) = Scratch::cgroup("oom") else {
            eprintln!("skipped: no delegated cgroup with the memory controller");
            return;
        };
        if !scratch.write("memory.max", &(32u64 << 20).to_string()) {
            eprintln!("skipped: cannot limit the scratch cgroup");
            return;
        }
        scratch.write("memory.swap.max", "0");
        let target = Target::Cgroup(scratch.path().to_path_buf());
        let mut collector = Box::new(Prepare::new(Scope::Cgroup, Mode::Count).expect("count"))
            .attach(&target)
            .expect("the scratch cgroup's memory.events");
        collector.sample(0).expect("the first read");
        let mut child = scratch.spawn_doomed_toucher(128 << 20);
        let signal = child.wait_for_end(Duration::from_secs(30));
        let events = collector.stop();
        drop(child);
        assert_eq!(signal, Some(libc::SIGKILL), "the child was not killed");
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].attr("kills").and_then(|v| v.as_f64()), Some(1.0));
        assert!(
            events[0]
                .attr("max")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0)
                >= 1.0,
            "{:?}",
            events[0]
        );
    }
}
