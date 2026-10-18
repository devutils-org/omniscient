//! `gpu.profile`: where time goes inside kernels, by CUPTI's PC sampling in
//! the injection library (`cupti/src/pcsampling.rs`) -- what `cpu.profile`
//! is for the cpu.
//!
//! Every SM samples the program counter of one of its warps every `2^n`
//! cycles, with the reason that warp was stalled; CUPTI aggregates the
//! samples per program counter and reason, and the library drains them and
//! resolves each program counter to its source line when the kernel's module
//! carries line information. **One event per program counter and stall
//! reason per drain**, weighed by `samples`, stacked as
//! `kernel;file:line;stall` -- or `kernel;+0x<offset>;stall` for a kernel
//! without line information -- so `-e stack` is a flame of the device's time
//! and `topk:10@line` ranks the lines.
//!
//! **A sample is a cycle count, not a time.** The sampling interval is in SM
//! cycles, so a share of samples is a share of the cycles the SMs spent, and
//! the clock the cycles were counted at is in the facts.
//!
//! **Stall reasons are counted once.** CUPTI reports, beside each reason, the
//! same reason restricted to the samples where no instruction issued
//! (`..._not_issued`): a subset of the first, which would count those samples
//! twice. They are left out here.
//!
//! **A configuration accepted and not applied is caught by its effect.**
//! The library reads back what CUPTI applied and counts the kernels launched;
//! a period or a mode that differs from what was asked, or a run whose
//! kernels took no sample at all, are said -- the latter as a lost read,
//! because it is the whole profile missing. One kernel without a sample among
//! others that have some is not a loss: a kernel shorter than the period on
//! every SM is expected to take none.
//!
//! **Every sample CUPTI counted is accounted for.** Its own count over the
//! program's kernels (all samples less those in kernels that are not the
//! program's) is held to the samples the program counters carried: a
//! shortfall beyond what CUPTI said it dropped is counted as dropped, with
//! the reason.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use super::counters::{Common, Inside};
use super::kernel::{wire, Socket};
use crate::collect::{Collector, Losses, SampleError};
use crate::event::{Attr, CollectorId, Event};

const DRAIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

/// What a stall reason is called once CUPTI's metric prefix is taken off:
/// `smsp__pcsamp_warps_issue_stalled_long_scoreboard` is `long_scoreboard`.
pub fn stall_name(raw: &str) -> String {
    let short = raw
        .strip_prefix("smsp__pcsamp_warps_issue_stalled_")
        .or_else(|| raw.strip_prefix("smsp__pcsamp_"))
        .unwrap_or(raw);
    short.to_owned()
}

/// What a process's PC sampling was configured with on one device.
#[derive(Debug, Clone, Copy)]
struct Config {
    asked: u32,
    applied: u32,
    mode: u32,
    reasons: u32,
}

/// The last drain a process announced: what the program counters after it
/// are stamped with.
#[derive(Debug, Clone, Copy, Default)]
struct Drain {
    at_ns: u64,
    since_ns: u64,
}

#[derive(Debug, Default)]
struct Totals {
    /// CUPTI's own count of samples, and of those in kernels that are not
    /// the program's.
    cupti: u64,
    non_user: u64,
    /// The samples reported per program counter, and those of them with a
    /// source line.
    reported: u64,
    with_line: u64,
    launches: u64,
    /// CUPTI's count of the samples it dropped, already counted as lost.
    dropped: u64,
    /// Samples CUPTI counted in the program's kernels that no program
    /// counter carried, counted as lost at the end.
    shortfall: u64,
}

pub struct ProfileCollector {
    id: CollectorId,
    common: Common,
    configs: BTreeMap<(u32, u32), Config>,
    stalls: BTreeMap<(u32, u32), String>,
    drains: BTreeMap<u32, Drain>,
    totals: Totals,
    /// Kernels whose samples carried no source line.
    without_lines: BTreeMap<String, u64>,
    /// Stack frames by identity, and identities by frames.
    stacks: BTreeMap<u64, Vec<String>>,
    stack_ids: BTreeMap<String, u64>,
}

impl ProfileCollector {
    pub(super) fn new(socket: Rc<RefCell<Socket>>, reader: usize, launched: i32) -> Self {
        Self {
            id: CollectorId::new("gpu.profile/vendor:cupti"),
            common: Common::new(Inside::Profile, socket, reader, launched),
            configs: BTreeMap::new(),
            stalls: BTreeMap::new(),
            drains: BTreeMap::new(),
            totals: Totals::default(),
            without_lines: BTreeMap::new(),
            stacks: BTreeMap::new(),
            stack_ids: BTreeMap::new(),
        }
    }

    fn stack_id(&mut self, frames: Vec<String>) -> (u64, String) {
        let joined = frames.join(";");
        if let Some(id) = self.stack_ids.get(&joined) {
            return (*id, joined);
        }
        let id = self.stack_ids.len() as u64 + 1;
        self.stack_ids.insert(joined.clone(), id);
        self.stacks.insert(id, frames);
        (id, joined)
    }

    fn drain(&mut self, seq: u64) -> Vec<Event> {
        let mut events = Vec::new();
        for (pid, op, payload) in self.common.take() {
            match op {
                wire::OP_STALL if payload.len() >= 4 => {
                    let index = u32::from_ne_bytes(payload[..4].try_into().expect("4 bytes"));
                    let raw = String::from_utf8_lossy(&payload[4..])
                        .trim_end_matches('\0')
                        .to_owned();
                    self.stalls.insert((pid, index), raw);
                }
                wire::OP_PC_CONFIG => match wire::PcConfig::decode(&payload) {
                    Some(config) => {
                        self.configs.insert(
                            (pid, config.device),
                            Config {
                                asked: config.period_asked,
                                applied: config.period_applied,
                                mode: config.mode_applied,
                                reasons: config.stall_reasons,
                            },
                        );
                    }
                    None => self.common.losses.undecoded += 1,
                },
                wire::OP_PC_DRAIN => match wire::PcDrain::decode(&payload) {
                    Some(drain) => {
                        self.drains.insert(
                            pid,
                            Drain {
                                at_ns: drain.at_ns,
                                since_ns: drain.since_ns,
                            },
                        );
                        self.totals.cupti += drain.total;
                        self.totals.non_user += drain.non_user;
                        self.totals.dropped += drain.dropped;
                        self.totals.launches = self.totals.launches.max(drain.launches);
                    }
                    None => self.common.losses.undecoded += 1,
                },
                wire::OP_PC => match wire::PcSample::decode(&payload) {
                    Some(sample) => {
                        if let Some(event) =
                            self.sample_event(pid, &sample, seq + events.len() as u64)
                        {
                            events.push(event);
                        }
                    }
                    None => self.common.losses.undecoded += 1,
                },
                wire::OP_INSIDE_LOST => self.common.inside_lost(&payload),
                _ => {}
            }
        }
        events
    }

    fn sample_event(&mut self, pid: u32, sample: &wire::PcSample, seq: u64) -> Option<Event> {
        let raw = self
            .stalls
            .get(&(pid, sample.stall))
            .cloned()
            .unwrap_or_else(|| format!("stall {}", sample.stall));
        if raw.ends_with("_not_issued") {
            return None;
        }
        let stall = stall_name(&raw);
        let sender = self.common.senders.entry(pid).or_default();
        let kernel = sender.name(sample.function);
        let file = (sample.file != 0).then(|| sender.name(sample.file));
        let place = match (&file, sample.line) {
            (Some(file), line) if line > 0 => format!("{file}:{line}"),
            _ => format!("+0x{:x}", sample.pc_offset),
        };
        let samples = u64::from(sample.samples);
        self.totals.reported += samples;
        if sample.line > 0 {
            self.totals.with_line += samples;
        } else {
            *self.without_lines.entry(kernel.clone()).or_default() += samples;
        }
        let (stack_id, stack) = self.stack_id(vec![kernel.clone(), place.clone(), stall.clone()]);
        let mut attrs = vec![
            Attr::uint("samples", samples),
            Attr::text("kernel", kernel),
            Attr::text("source", place),
            Attr::text("stall", stall),
            Attr::uint("pc", sample.pc_offset),
            Attr::uint("device", u64::from(sample.device)),
            Attr::uint("pid", u64::from(pid)),
            Attr::uint(crate::emit::STACK_ID_ATTR, stack_id),
            Attr::text(crate::emit::STACK_ATTR, stack),
        ];
        if let Some(file) = file {
            attrs.push(Attr::text("file", file));
        }
        if sample.line > 0 {
            attrs.push(Attr::uint("line", u64::from(sample.line)));
        }
        // Stamped at the drain that took them, in the program's own
        // CLOCK_MONOTONIC; the samples were taken since the drain before,
        // and that window is the error.
        let drain = self.drains.get(&pid).copied().unwrap_or_default();
        let window = drain.at_ns.saturating_sub(drain.since_ns);
        Some(
            self.common
                .event(pid, drain.at_ns, window, attrs, &self.id, seq),
        )
    }
}

impl ProfileCollector {
    /// Holds CUPTI's count of the program's samples to those the program
    /// counters carried. Whether CUPTI's total includes the samples it
    /// dropped is not documented, so the shortfall past those -- already
    /// counted -- is what is added: a loss is never counted twice, and
    /// whichever reading holds, what is missing is counted.
    fn account_for_samples(&mut self) {
        let expected = self.totals.cupti.saturating_sub(self.totals.non_user);
        let missing = expected.saturating_sub(self.totals.reported);
        let shortfall = missing.saturating_sub(self.totals.dropped);
        if shortfall > 0 {
            self.totals.shortfall = shortfall;
            self.common.losses.dropped += shortfall;
            self.common.losses.note_times(
                format!(
                    "CUPTI counted {expected} samples in the program's kernels and the program \
                 counters carried {}: {shortfall} were not reported -- a drain that did not fit \
                 its buffer, or a batch that never reached `omn`",
                    self.totals.reported
                ),
                shortfall,
            );
        }
    }
}

impl Collector for ProfileCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "samples"
    }

    fn start(&mut self) -> Result<(), String> {
        self.common.start();
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let events = self.drain(seq);
        self.common.look_for_others();
        Ok(events)
    }

    fn stop(&mut self) -> Vec<Event> {
        let events = self.drain(u64::MAX / 2);
        self.common.look_now();
        self.common.unfinished();
        if self.totals.launches > 0 && self.totals.cupti == 0 {
            self.common.losses.failed_reads += 1;
            self.common.losses.note_times(
                format!(
                    "{} kernel launches ran and PC sampling took no sample at all: CUPTI accepted \
                 a configuration it did not apply, so this profile is missing, not empty",
                    self.totals.launches
                ),
                1,
            );
        }
        self.account_for_samples();
        events
    }

    fn tick(&self) -> Option<std::time::Duration> {
        Some(DRAIN_INTERVAL)
    }

    fn losses(&self) -> Losses {
        self.common.losses.clone()
    }

    fn failure(&self) -> Option<String> {
        self.common.failure()
    }

    fn stacks(&self) -> BTreeMap<u64, Vec<String>> {
        self.stacks.clone()
    }

    fn facts(&self) -> Vec<String> {
        let mut facts = Vec::new();
        for ((pid, device), config) in &self.configs {
            facts.push(format!(
                "pid {pid}, device {device}: a sample every 2^{} cycles of each SM, \
                 continuously, with {} stall reasons",
                config.applied, config.reasons
            ));
        }
        if self.totals.cupti > 0 || self.totals.reported > 0 {
            let lined = if self.totals.reported > 0 {
                100.0 * self.totals.with_line as f64 / self.totals.reported as f64
            } else {
                0.0
            };
            facts.push(format!(
                "{} samples over {} launches by CUPTI's count, {} reported per program counter, \
                 {lined:.1} % of them on a source line; {} in kernels that are not the program's",
                self.totals.cupti, self.totals.launches, self.totals.reported, self.totals.non_user
            ));
        }
        if !self.without_lines.is_empty() {
            let kernels: Vec<&str> = self.without_lines.keys().map(String::as_str).collect();
            facts.push(format!(
                "no line information in {}: their samples are placed by offset in the kernel; \
                 build with `-lineinfo` (or JIT with CU_JIT_GENERATE_LINE_INFO) to have lines",
                kernels.join(", ")
            ));
        }
        facts.extend(self.common.facts());
        facts.extend(super::timing_clock_facts());
        facts
    }

    fn notes(&self) -> Vec<String> {
        let mut notes = self.common.notes();
        for ((pid, device), config) in &self.configs {
            if config.applied != config.asked {
                notes.push(format!(
                    "pid {pid}, device {device}: CUPTI accepted a sampling period of 2^{} cycles \
                     and applied 2^{}: the samples are that much {} than asked",
                    config.asked,
                    config.applied,
                    if config.applied > config.asked {
                        "sparser"
                    } else {
                        "denser"
                    }
                ));
            }
            if config.mode != 1 {
                notes.push(format!(
                    "pid {pid}, device {device}: CUPTI accepted continuous collection and applied \
                     mode {}: kernels may have been serialised",
                    config.mode
                ));
            }
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stall_reason_loses_cupti_s_prefix() {
        assert_eq!(
            stall_name("smsp__pcsamp_warps_issue_stalled_long_scoreboard"),
            "long_scoreboard"
        );
        assert_eq!(stall_name("smsp__pcsamp_sample_count"), "sample_count");
        assert_eq!(stall_name("something_else"), "something_else");
    }

    use crate::collect::gpu::counters::tests::{lost, name, pair, send};

    fn stall(index: u32, text: &str) -> (u8, Vec<u8>) {
        let mut payload = index.to_ne_bytes().to_vec();
        payload.extend_from_slice(text.as_bytes());
        (wire::OP_STALL, payload)
    }

    fn drain(at_ns: u64, since_ns: u64, total: u64, launches: u64) -> (u8, Vec<u8>) {
        let drain = wire::PcDrain {
            at_ns,
            since_ns,
            total,
            dropped: 0,
            non_user: 0,
            pcs: 2,
            device: 0,
            launches,
        };
        (wire::OP_PC_DRAIN, drain.encode().as_slice().to_vec())
    }

    fn pc(offset: u64, stall: u32, samples: u32, line: u32) -> (u8, Vec<u8>) {
        let sample = wire::PcSample {
            cubin_crc: 9,
            pc_offset: offset,
            function: 1,
            stall,
            samples,
            line,
            file: if line > 0 { 2 } else { 0 },
            device: 0,
        };
        (wire::OP_PC, sample.encode().as_slice().to_vec())
    }

    fn config(asked: u32, applied: u32, mode: u32) -> (u8, Vec<u8>) {
        let config = wire::PcConfig {
            device: 0,
            period_asked: asked,
            period_applied: applied,
            mode_applied: mode,
            stall_reasons: 2,
            pad: 0,
        };
        (wire::OP_PC_CONFIG, config.encode().as_slice().to_vec())
    }

    fn text(event: &Event, name: &str) -> String {
        event
            .attr(name)
            .map(|value| value.to_string())
            .unwrap_or_default()
    }

    /// Every program counter and reason is an event weighed by its samples,
    /// on a stack of kernel, line and reason, stamped at its drain with the
    /// window as its error; the `_not_issued` subset is not counted again.
    #[test]
    fn a_sampled_line_is_an_event_on_its_stack_and_counted_once() {
        let (write, socket, reader) = pair(Inside::Profile);
        let mut collector = ProfileCollector::new(socket, reader, 8);
        send(
            write,
            8,
            &[
                (wire::OP_HELLO, vec![0; 24]),
                stall(3, "smsp__pcsamp_warps_issue_stalled_wait"),
                stall(4, "smsp__pcsamp_warps_issue_stalled_wait_not_issued"),
                config(12, 12, 1),
                name(1, "hotloop"),
                name(2, "hotloop.cu"),
                drain(5_000, 1_000, 900, 3),
                pc(0x340, 3, 800, 20),
                pc(0x340, 4, 700, 20),
                pc(0x9a0, 3, 100, 0),
                (wire::OP_END, vec![0; 24]),
            ],
        );
        let mut events = collector.sample(0).unwrap();
        events.extend(collector.stop());
        assert_eq!(events.len(), 2, "the _not_issued subset is not an event");
        let hot = &events[0];
        assert_eq!(text(hot, "samples"), "800");
        assert_eq!(text(hot, "source"), "hotloop.cu:20");
        assert_eq!(text(hot, "stall"), "wait");
        assert_eq!(text(hot, "line"), "20");
        assert_eq!((hot.ts_ns, hot.ts_err_ns), (5_000, 4_000));
        let stack = hot
            .attr(crate::emit::STACK_ID_ATTR)
            .unwrap()
            .as_f64()
            .unwrap() as u64;
        assert_eq!(
            collector.stacks()[&stack],
            vec!["hotloop", "hotloop.cu:20", "wait"]
        );
        assert_eq!(text(&events[1], "source"), "+0x9a0");
        let facts = collector.facts().join("\n");
        assert!(facts.contains("no line information in hotloop"), "{facts}");
        assert!(collector.losses().is_clean());
        assert!(collector.notes().is_empty(), "{:?}", collector.notes());
    }

    /// Samples CUPTI counted and no program counter carried are a loss,
    /// past those it said it dropped.
    #[test]
    fn samples_cupti_counted_and_no_program_counter_carried_are_lost() {
        let (write, socket, reader) = pair(Inside::Profile);
        let mut collector = ProfileCollector::new(socket, reader, 8);
        let mut account = drain(5_000, 1_000, 1_000, 3);
        // 1 000 counted, 100 of them not the program's, 50 dropped by
        // CUPTI's own count: 850 should arrive, and 800 do.
        let mut decoded = wire::PcDrain::decode(&account.1).unwrap();
        decoded.non_user = 100;
        decoded.dropped = 50;
        account.1 = decoded.encode().as_slice().to_vec();
        send(
            write,
            8,
            &[
                (wire::OP_HELLO, vec![0; 24]),
                stall(3, "smsp__pcsamp_warps_issue_stalled_wait"),
                config(12, 12, 1),
                name(1, "hotloop"),
                name(2, "hotloop.cu"),
                account,
                lost(50, wire::LOST_SAMPLES),
                pc(0x340, 3, 800, 20),
                (wire::OP_END, vec![0; 24]),
            ],
        );
        collector.sample(0).unwrap();
        collector.stop();
        let losses = collector.losses();
        assert_eq!(losses.dropped, 50 + 50);
        let reasons = format!("{:?}", losses.reasons);
        assert!(reasons.contains("50 were not reported"), "{reasons}");
    }

    /// A configuration CUPTI accepted and did not apply is said, and kernels
    /// that ran without a single sample are a lost read.
    #[test]
    fn a_configuration_accepted_and_not_applied_is_caught_by_its_effect() {
        let (write, socket, reader) = pair(Inside::Profile);
        let mut collector = ProfileCollector::new(socket, reader, 8);
        send(
            write,
            8,
            &[
                (wire::OP_HELLO, vec![0; 24]),
                config(12, 16, 2),
                drain(5_000, 1_000, 0, 40),
                lost(7, wire::LOST_SAMPLES),
                (wire::OP_END, vec![0; 24]),
            ],
        );
        collector.sample(0).unwrap();
        collector.stop();
        let notes = collector.notes().join("\n");
        assert!(notes.contains("applied 2^16"), "{notes}");
        assert!(notes.contains("applied mode 2"), "{notes}");
        let losses = collector.losses();
        assert_eq!(losses.failed_reads, 1);
        assert_eq!(losses.dropped, 7);
        let reasons = format!("{:?}", losses.reasons);
        assert!(reasons.contains("40 kernel launches ran"), "{reasons}");
    }
}
