//! `power.energy` through the powercap interface.
//!
//! RAPL keeps a per-domain energy counter in microjoules. Two things about it
//! decide the shape of this collector.
//!
//! **It wraps.** The range is in `max_energy_range_uj` -- about 65 kilojoules
//! on this machine -- and at a hundred watts that is eleven minutes. A
//! subtraction that ignores the wrap reports a delta of nearly the whole range
//! in the tick that straddles it, which looks like a power spike of tens of
//! kilowatts. So a decrease is read as one wrap, and the event says it
//! happened. A poll interval long enough to wrap twice cannot be recovered at
//! all, and the collector says so rather than reporting the remainder.
//!
//! **Its domains nest.** `intel-rapl:0` is a package and `intel-rapl:0:0` is
//! the cores inside that package; adding both counts the cores twice. Only the
//! top-level domains are summed, and the event names them.
//!
//! On most current kernels `energy_uj` is readable only by root -- a
//! mitigation, after energy counters were shown to leak what the machine was
//! computing. That is reported as what it is: present and out of reach, which
//! calls for privileges, rather than absent, which would call for different
//! hardware.

use std::path::PathBuf;

use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, AttrValue, CollectorId, Event};
use crate::request::{Mode, Target};

use super::sysfs;
use super::sysfs::ReadError;

const POWERCAP: &str = "/sys/class/powercap";

#[derive(Debug)]
pub struct Prepare {
    mode: Mode,
}

impl Prepare {
    pub fn new(mode: Mode) -> Result<Self, String> {
        match mode {
            Mode::Poll { .. } | Mode::Count => Ok(Self { mode }),
            other => Err(format!(
                "energy is a counter that has to be differenced over a window: `{}` has none; \
                 poll it, or count it over a duration",
                other.kind()
            )),
        }
    }
}

impl Prepared for Prepare {
    /// RAPL measures a package. There is no per-process energy in the hardware:
    /// a per-process figure is a share of cycles multiplied by a package
    /// number, which is an estimate and has to be labelled as one, so this
    /// backend refuses to be asked for it.
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        match target {
            Target::System => Ok(Box::new(Energy::new(self.mode, PathBuf::from(POWERCAP)))),
            other => Err(format!(
                "RAPL counts the energy of a whole package, so a {} target has no number here. \
                 Attributing a share of it to one process means multiplying by that process's \
                 share of cycles, which is an estimate this tool does not present as a \
                 measurement",
                other.kind()
            )),
        }
    }
}

/// One top-level RAPL domain.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Domain {
    /// `package-0`, `psys`, as the kernel labels it.
    name: String,
    energy: PathBuf,
    /// The value the counter wraps at, read from the domain itself rather than
    /// assumed: it differs between vendors and between domains.
    range_uj: u64,
}

/// One reading of every domain.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Reading {
    ts_ns: u64,
    /// Domain name to microjoules.
    energy_uj: Vec<(String, u64)>,
}

pub struct Energy {
    id: CollectorId,
    mode: Mode,
    root: PathBuf,
    domains: Vec<Domain>,
    previous: Option<Reading>,
    baseline: Option<Reading>,
    losses: Losses,
}

impl Energy {
    pub fn new(mode: Mode, root: PathBuf) -> Self {
        Self {
            id: CollectorId::new("power.energy/fs:sys"),
            mode,
            root,
            domains: Vec::new(),
            previous: None,
            baseline: None,
            losses: Losses::default(),
        }
    }

    /// The top-level domains: `intel-rapl:0`, never `intel-rapl:0:0`, whose
    /// energy is already inside its parent's.
    fn discover(&self) -> Result<Vec<Domain>, String> {
        let entries = sysfs::entries(&self.root)
            .map_err(|error| format!("cannot list the powercap domains: {error}"))?;
        let mut domains = Vec::new();
        for path in entries {
            let file = path.file_name().unwrap_or_default().to_string_lossy();
            // `intel-rapl` is the control type itself and holds no counter;
            // `intel-rapl:0:0` is a subdomain of `intel-rapl:0`.
            if file.matches(':').count() != 1 {
                continue;
            }
            let name = sysfs::text(&path.join("name")).unwrap_or_else(|_| file.into_owned());
            let Ok(range_uj) = sysfs::number(&path.join("max_energy_range_uj")) else {
                continue;
            };
            domains.push(Domain {
                name,
                energy: path.join("energy_uj"),
                range_uj,
            });
        }
        Ok(domains)
    }

    fn read(&mut self) -> Result<Reading, SampleError> {
        let mut energy_uj = Vec::new();
        for domain in &self.domains {
            match sysfs::number(&domain.energy) {
                Ok(value) => energy_uj.push((domain.name.clone(), value)),
                Err(error) => {
                    self.losses.failed_reads += 1;
                    self.losses
                        .note(format!("domain `{}`: {error}", domain.name));
                }
            }
        }
        if energy_uj.is_empty() {
            return Err(SampleError::TargetGone(
                "no RAPL domain could be read any more".to_owned(),
            ));
        }
        Ok(Reading {
            ts_ns: monotonic_ns(),
            energy_uj,
        })
    }

    fn delta_event(&mut self, before: &Reading, after: &Reading, seq: u64) -> Option<Event> {
        if before.energy_uj.len() != after.energy_uj.len() {
            self.losses.undecoded += 1;
            self.losses.note(
                "the set of RAPL domains changed between two reads, so the energy between them is \
                 not one machine's worth"
                    .to_owned(),
            );
            return None;
        }
        let interval_ns = after.ts_ns.saturating_sub(before.ts_ns);
        if interval_ns == 0 {
            return None;
        }

        let mut total_uj = 0u64;
        let mut wrapped = 0u64;
        let mut per_domain = Vec::new();
        for ((name, after_uj), (_, before_uj)) in after.energy_uj.iter().zip(&before.energy_uj) {
            let range = self
                .domains
                .iter()
                .find(|domain| domain.name == *name)
                .map(|domain| domain.range_uj)
                .unwrap_or(u64::MAX);
            let delta = match after_uj.checked_sub(*before_uj) {
                Some(delta) => delta,
                None => {
                    // The counter wrapped once, which is recoverable. Twice is
                    // not, and nothing in the file says which happened, so the
                    // interval is reported as having wrapped and the reader is
                    // told what that costs.
                    wrapped += 1;
                    (range - before_uj) + after_uj
                }
            };
            total_uj += delta;
            per_domain.push((name.clone(), delta));
        }

        let joules = total_uj as f64 / 1_000_000.0;
        let seconds = interval_ns as f64 / 1e9;
        let float = |name: String, value: f64| Attr {
            name,
            value: AttrValue::Float(value),
        };
        let mut attrs = vec![
            float("joules".to_owned(), joules),
            float("watts".to_owned(), joules / seconds),
            Attr::uint("interval_ns", interval_ns),
            Attr::text(
                "domains",
                per_domain
                    .iter()
                    .map(|(name, _)| name.clone())
                    .collect::<Vec<_>>()
                    .join(","),
            ),
        ];
        for (name, delta_uj) in per_domain {
            attrs.push(float(
                sysfs::attribute_name("domain", &name),
                delta_uj as f64 / 1_000_000.0,
            ));
        }
        if wrapped > 0 {
            attrs.push(Attr::uint("wrapped", wrapped));
            self.losses.note(format!(
                "{wrapped} RAPL domain(s) wrapped during one tick; the increase was recovered \
                 assuming a single wrap, which a tick longer than {} seconds cannot guarantee",
                self.domains
                    .first()
                    .map(|domain| domain.range_uj / 1_000_000)
                    .unwrap_or(0)
            ));
        }

        Some(Event {
            ts_ns: after.ts_ns,
            ts_err_ns: 0,
            kind: "power.energy".to_owned(),
            keys: Vec::new(),
            attrs,
            collector_id: self.id.clone(),
            seq,
        })
    }
}

impl Collector for Energy {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "joules"
    }

    fn start(&mut self) -> Result<(), String> {
        self.domains = self.discover()?;
        if self.domains.is_empty() {
            return Err(format!(
                "no RAPL domain under {}: this processor exposes no energy counter, or the \
                 `intel_rapl` driver is not loaded",
                self.root.display()
            ));
        }
        // Read one domain now rather than failing on the first tick, so a
        // refusal names privileges instead of arriving as a lost sample.
        let first = &self.domains[0];
        if let Err(error) = sysfs::number(&first.energy) {
            return Err(match error {
                ReadError::Restricted(path) => format!(
                    "{} is readable only by root on this kernel -- energy counters were restricted \
                     after they were shown to leak what the machine was computing. Run as root, \
                     or use the `pmu` backend, which reads the same counter through \
                     perf_event_open",
                    path.display()
                ),
                other => other.to_string(),
            });
        }
        if self.mode == Mode::Count {
            self.baseline = Some(
                self.read()
                    .map_err(|error| format!("cannot read the RAPL domains: {error:?}"))?,
            );
        }
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        if self.mode == Mode::Count {
            return Ok(Vec::new());
        }
        let after = self.read()?;
        let before = self.previous.replace(after.clone());
        let event = before.and_then(|before| self.delta_event(&before, &after, seq));
        Ok(event.into_iter().collect())
    }

    fn stop(&mut self) -> Vec<Event> {
        if self.mode != Mode::Count {
            return Vec::new();
        }
        let Some(before) = self.baseline.clone() else {
            return Vec::new();
        };
        match self.read() {
            Ok(after) => self
                .delta_event(&before, &after, u64::MAX / 2)
                .into_iter()
                .collect(),
            Err(error) => {
                self.losses.failed_reads += 1;
                self.losses.note(format!(
                    "the closing read failed ({error:?}), so this run has no total"
                ));
                Vec::new()
            }
        }
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::power::sysfs::scratch;

    fn poll() -> Mode {
        Mode::Poll {
            interval: Mode::DEFAULT_POLL_INTERVAL,
        }
    }

    /// One package, and the cores inside it. Adding both would count the cores
    /// twice, which is the same mistake as adding a partition to its disk.
    fn rapl_tree(name: &str, package_uj: u64, core_uj: u64, range_uj: u64) -> PathBuf {
        let root = scratch::tree(name);
        scratch::write(&root, "intel-rapl:0/name", "package-0");
        scratch::write(&root, "intel-rapl:0/energy_uj", &package_uj.to_string());
        scratch::write(
            &root,
            "intel-rapl:0/max_energy_range_uj",
            &range_uj.to_string(),
        );
        scratch::write(&root, "intel-rapl:0:0/name", "core");
        scratch::write(&root, "intel-rapl:0:0/energy_uj", &core_uj.to_string());
        scratch::write(
            &root,
            "intel-rapl:0:0/max_energy_range_uj",
            &range_uj.to_string(),
        );
        root
    }

    #[test]
    fn a_subdomain_is_not_added_to_the_package_that_contains_it() {
        let root = rapl_tree("rapl", 1_000_000, 400_000, 65_532_610_987);
        let mut collector = Energy::new(poll(), root.clone());
        collector.start().expect("a readable domain");
        assert!(
            collector.sample(0).expect("the first read").is_empty(),
            "the first read is a baseline"
        );

        // The package gains 2 J and the cores inside it 1.5 J of that.
        scratch::write(&root, "intel-rapl:0/energy_uj", "3000000");
        scratch::write(&root, "intel-rapl:0:0/energy_uj", "1900000");
        let events = collector.sample(1).expect("a second read");

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].attr("joules").unwrap().as_f64(), Some(2.0));
        assert_eq!(events[0].attr("domains").unwrap().to_string(), "package-0");
        assert_eq!(
            events[0].attr("domain.package_0").unwrap().as_f64(),
            Some(2.0)
        );
        let watts = events[0].attr("watts").unwrap().as_f64().unwrap();
        assert!(watts > 0.0, "two joules over a real interval is some power");
        std::fs::remove_dir_all(&root).ok();
    }

    /// The counter wraps at a range the domain publishes. Subtracting straight
    /// through a wrap reports nearly the whole range as one tick's energy --
    /// tens of kilowatts, from a machine drawing a hundred watts.
    #[test]
    fn a_wrapped_counter_is_recovered_and_the_event_says_it_wrapped() {
        const RANGE: u64 = 65_532_610_987;
        let root = rapl_tree("rapl-wrap", RANGE - 1_000_000, 0, RANGE);
        let mut collector = Energy::new(poll(), root.clone());
        collector.start().expect("a readable domain");
        collector.sample(0).expect("the baseline read");

        // One joule to the wrap, then half a joule past it.
        scratch::write(&root, "intel-rapl:0/energy_uj", "500000");
        let events = collector.sample(1).expect("a second read");

        assert_eq!(events[0].attr("joules").unwrap().as_f64(), Some(1.5));
        assert_eq!(events[0].attr("wrapped").unwrap().as_f64(), Some(1.0));
        assert!(collector
            .losses()
            .reasons
            .keys()
            .any(|reason| reason.contains("wrapped")));
        std::fs::remove_dir_all(&root).ok();
    }

    /// A counted run reports one total, measured from before the observed
    /// program started.
    #[test]
    fn a_counted_run_reports_one_total_for_the_window() {
        let root = rapl_tree("rapl-count", 1_000_000, 0, 65_532_610_987);
        let mut collector = Energy::new(Mode::Count, root.clone());
        collector.start().expect("a readable domain");
        assert!(collector.sample(0).expect("a tick").is_empty());
        scratch::write(&root, "intel-rapl:0/energy_uj", "9000000");

        let events = collector.stop();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].attr("joules").unwrap().as_f64(), Some(8.0));
        std::fs::remove_dir_all(&root).ok();
    }

    /// Two readings over different sets of domains do not subtract.
    #[test]
    fn a_change_of_domains_voids_the_interval() {
        let root = rapl_tree("rapl-domains", 1_000_000, 0, 65_532_610_987);
        let mut collector = Energy::new(poll(), root.clone());
        collector.start().expect("a readable domain");
        collector.sample(0).expect("the baseline read");

        scratch::write(&root, "intel-rapl:1/name", "psys");
        scratch::write(&root, "intel-rapl:1/energy_uj", "500000");
        scratch::write(&root, "intel-rapl:1/max_energy_range_uj", "65532610987");
        // The domains are discovered once, at the start, so the second reading
        // is taken over the same set -- what must not happen is a total that
        // silently mixes the two.
        let events = collector.sample(1).expect("a second read");
        for event in &events {
            assert_eq!(
                event.attr("domains").unwrap().to_string(),
                "package-0",
                "a domain that appeared after the start must not join the sum"
            );
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// RAPL measures a package; a per-process figure would be an estimate, and
    /// this backend does not offer to make one.
    #[test]
    fn a_process_target_is_refused_with_what_it_would_have_taken() {
        let prepared = Box::new(Prepare::new(poll()).expect("poll is servable"));
        let refused = prepared
            .attach(&Target::Pid(vec![1]))
            .err()
            .expect("a pid has no package energy");
        assert!(refused.contains("share of cycles"), "{refused}");
        assert!(refused.contains("estimate"), "{refused}");
    }

    #[test]
    fn a_mode_that_has_no_window_is_refused() {
        let refused = Prepare::new(Mode::Snapshot).expect_err("one read is not a difference");
        assert!(refused.contains("counter"), "{refused}");
        assert!(Prepare::new(Mode::Count).is_ok());
    }

    #[test]
    fn a_machine_without_rapl_refuses_with_the_reason() {
        let root = scratch::tree("rapl-none");
        let mut collector = Energy::new(poll(), root.clone());
        let refused = collector.start().expect_err("nothing to read");
        assert!(refused.contains("no RAPL domain"), "{refused}");
        std::fs::remove_dir_all(&root).ok();
    }

    /// On a machine where the counter is root-only -- which is most of them
    /// now -- the refusal has to name privileges, not hardware. An earlier
    /// version of this collector let the reader believe the file was absent.
    #[test]
    fn a_counter_this_user_cannot_read_asks_for_privileges_not_for_hardware() {
        let real = PathBuf::from(POWERCAP);
        let mut collector = Energy::new(poll(), real);
        match collector.start() {
            // Running as root, or on a kernel that still opens the counter.
            Ok(()) => {}
            Err(refused) if refused.contains("no RAPL domain") => {}
            Err(refused) => assert!(
                refused.contains("root"),
                "an unreadable counter must ask for privileges: {refused}"
            ),
        }
    }
}
