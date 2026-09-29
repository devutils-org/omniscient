//! The compute crossing: where one task's wall clock went.
//!
//! Three sources that each collect and are each checked on their own --
//! `cpu.profile`, `sched.runqlat`, `sched.offcpu` -- put in one table for one
//! task, with `sched.wakeup -> sched.switches` for what released each run.
//! Nothing here is new mechanism. What is new is the arithmetic that adds the
//! three up, and the honesty about what that sum is missing, and
//! `docs/scenarios/cross-compute.md` is the prose half of this file.
//!
//! **Where the ground truth comes from.** Every workload here reports the
//! kernel's own accounting of itself on stderr before it exits, from
//! `/proc/self/schedstat` and `/proc/self/status`: nanoseconds on a cpu,
//! nanoseconds waiting for one, timeslices, voluntary switches, and its own
//! clock around the part that matters. The tool reads none of those files and
//! none of that line. Each row of the decomposition is checked against the
//! kernel's figure for the same quantity, with the slack each reference's own
//! resolution calls for and nothing calibrated to this machine.
//!
//! **What the tool cannot yet say, and these tests therefore compute.** There is
//! no decomposition in the engine, and a launched program's lifetime is in no
//! output; both are requests in `docs/open-findings.md` under stream P2. So the
//! whole is bracketed -- the workload's own clock below, the run's window above
//! -- and the residue is printed as a range. [`Decomposition::render`] is the
//! table the document shows.
//!
//! **Every session here runs alone.** Two of them pin a spinning shell to a cpu
//! on purpose, and a crossing is two sources on a shared machine at the same
//! time: running them beside each other would make each the other's confounder.
//! [`one_at_a_time`] holds them apart within this binary.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Mutex, MutexGuard};

use serde_json::Value;

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

/// Round trips and spins per answer in the half-blocked sessions. The driver
/// is on the cpu for its own spins and blocked for the responder's, equal by
/// construction.
const ROUND_TRIPS: u64 = 600;
const SPINS: u64 = 100;

/// The profile's rate in the clean sessions, and in the lossy one.
const CLEAN_HZ: f64 = 997.0;
const LOSSY_HZ: f64 = 100_000.0;

/// Sleeps, and the length of each, in `cross-compute-queued-after-wakeup.yml`.
const SLEEPS: u64 = 100;
const EACH_MS: u64 = 5;

/// The poll interval of `cpu.oncpu_time`, and the tick the kernel credits cpu
/// time in, which together are that row's resolution.
const POLL_NS: f64 = 50e6;
const TICK_NS: f64 = 10e6;

/// How far `sched.runqlat` may sit from the kernel's `run_delay`, per wait.
///
/// Both measure the same interval with different clocks read at different
/// points: the kernel stamps a wait with its run queue clock, and after a
/// wakeup that preempts it deliberately skips the clock update before the
/// switch (`rq_clock_skip_update`), so its figure ends a little early; the
/// tool stamps it with `bpf_ktime_get_ns` at the two tracepoints. Measured
/// here the difference is 0.3 µs per wait when every wait is behind another
/// task on one cpu, and 1.7 µs per wait when every wakeup lands on an idle
/// cpu. Five microseconds is that difference's order, not a calibration.
const PER_WAIT_NS: f64 = 5_000.0;

/// How long the kernel may overshoot one requested sleep: the default timer
/// slack of 50 µs, doubled for the wakeup that follows it.
const SLACK_PER_SLEEP_NS: f64 = 100_000.0;

fn one_at_a_time() -> MutexGuard<'static, ()> {
    static MACHINE: Mutex<()> = Mutex::new(());
    MACHINE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ------------------------------------------------------------------ running

/// Everything the run said, on either stream. A refusal is on stderr and a
/// measurement is on stdout, and reading one of them only is how a test
/// confuses a refusal with a silence.
fn spoken(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr)
}

/// Whether the machine refused for want of the privileges the eBPF backends
/// need: a machine's limit, and the only refusal allowed to stop a test.
fn unprivileged(text: &str) -> bool {
    text.contains("CAP_BPF") || text.contains("CAP_PERFMON")
}

fn have(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// One run of a session: what the tool measured, and what the workload says
/// the kernel measured.
struct Run {
    results: Vec<Value>,
    joins: Vec<Value>,
    truth: BTreeMap<String, u64>,
    text: String,
}

/// Runs a session as JSON, or says why it could not and returns nothing.
///
/// A missing program or missing privileges is a skip, and it is said out loud
/// with what would have been checked. Anything else that is not a clean run is
/// a failure: a session this crossing ships that the tool refuses is broken.
fn run(file: &str, needs: &[&str], would_have_checked: &str) -> Option<Run> {
    for program in needs {
        if !have(program) {
            eprintln!(
                "skipped: {file} needs `{program}`, which this machine does not have. It would \
                 have checked {would_have_checked}"
            );
            return None;
        }
    }
    let path = format!("examples/{file}");
    assert!(
        Path::new(&path).exists(),
        "{path} is part of this crossing and is missing"
    );
    let output = Command::new(BINARY)
        .args(["-f", &path, "--format", "json"])
        .output()
        .expect("omn should run");
    let text = spoken(&output);
    if unprivileged(&text) && !output.status.success() {
        eprintln!(
            "skipped: {file} needs CAP_BPF and CAP_PERFMON, which this binary does not carry \
             (run `make`, which grants them). It would have checked {would_have_checked}"
        );
        return None;
    }
    assert_eq!(
        output.status.code(),
        Some(0),
        "{file} did not run cleanly:\n{text}"
    );

    let document: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!("{file} did not produce one json document ({error}):\n{text}")
    });
    let stderr = String::from_utf8_lossy(&output.stderr);
    let line = stderr
        .lines()
        .find_map(|line| line.strip_prefix("omn-truth "))
        .unwrap_or_else(|| {
            panic!("the workload of {file} did not report the kernel's figures:\n{text}")
        });
    let truth = line
        .split_whitespace()
        .filter_map(|pair| {
            let (name, value) = pair.split_once('=')?;
            Some((name.to_owned(), value.parse().ok()?))
        })
        .collect();

    Some(Run {
        results: document["results"].as_array().cloned().unwrap_or_default(),
        joins: document["joins"].as_array().cloned().unwrap_or_default(),
        truth,
        text,
    })
}

impl Run {
    fn labelled(&self, label: &str) -> &Value {
        self.results
            .iter()
            .find(|result| result["label"] == label)
            .unwrap_or_else(|| panic!("no request labelled `{label}` in this session"))
    }

    fn truth(&self, name: &str) -> u64 {
        *self
            .truth
            .get(name)
            .unwrap_or_else(|| panic!("the workload reported no `{name}`:\n{}", self.text))
    }

    /// The task the workload says it is, which every row is read for.
    fn task(&self) -> u64 {
        self.truth("tid")
    }
}

fn outputs<'a>(result: &'a Value, emit: &'a str) -> impl Iterator<Item = &'a Value> {
    result["outputs"]
        .as_array()
        .expect("a result carries outputs")
        .iter()
        .filter(move |output| output["emit"] == emit)
}

fn raw_events(result: &Value) -> impl Iterator<Item = &Value> {
    outputs(result, "raw").flat_map(|output| {
        output["events"]
            .as_array()
            .expect("a raw output carries events")
            .iter()
    })
}

fn attr(event: &Value, name: &str) -> Option<u64> {
    event["attrs"]
        .as_array()?
        .iter()
        .find(|attr| attr["name"] == name)
        .and_then(|attr| attr["value"].as_u64())
}

fn tid_of(event: &Value) -> Option<u64> {
    event["keys"]
        .as_array()?
        .iter()
        .find_map(|key| key["task"]["tid"].as_u64())
}

/// Adds one attribute over every raw event of one task.
///
/// `sched.offcpu` and `sched.runqlat` both count in the kernel, so an event is
/// a total over a drain interval, not one block or one wait. What the workload
/// did is the sum of the attribute, never the number of events.
fn total(result: &Value, task: u64, attribute: &str) -> u64 {
    raw_events(result)
        .filter(|event| tid_of(event) == Some(task))
        .filter_map(|event| attr(event, attribute))
        .sum()
}

fn series_of<'a>(result: &'a Value, emit: &'a str, task: u64) -> Option<&'a Value> {
    let key = format!("task:{task}");
    outputs(result, emit)
        .flat_map(|output| output["series"].as_array().into_iter().flatten())
        .find(|series| series["key"] == key.as_str())
}

fn losses(result: &Value, which: &str) -> u64 {
    result["losses"][which].as_u64().unwrap_or(0)
}

// ------------------------------------------------------------ the arithmetic

/// One task's wall clock, split across the three states, from one run.
///
/// Every field is read off the tool except `whole_low_ns`, which is the
/// workload's own clock: the lifetime of a launched program is in no output,
/// so the whole can only be bracketed.
struct Decomposition {
    task: u64,
    /// The workload's own clock around its work: a lower bound on its life.
    whole_low_ns: f64,
    /// The run's window, from releasing the program to the last drain: an
    /// upper bound, loose by however late the last drain was.
    whole_high_ns: f64,
    /// A sample count: `samples` times one period.
    samples: u64,
    /// Samples the ring reported losing, which the mark on the profile counts.
    dropped: u64,
    period_ns: f64,
    /// The profile's own statement of what it is missing, when it is.
    short: Option<String>,
    /// A polled counter, in ticks: the second opinion on the first row.
    on_cpu_polled_ns: f64,
    /// Traced durations, summed exactly in the kernel.
    runnable_ns: f64,
    waits: u64,
    off_cpu_ns: f64,
    /// The part of `off_cpu_ns` spent queued after each wakeup.
    queued_ns: f64,
    blocks: u64,
}

impl Decomposition {
    fn from(run: &Run, hz: f64) -> Self {
        let task = run.task();
        let profile = run.labelled("on-cpu");
        let samples = series_of(profile, "topk", task)
            .and_then(|series| series["events"].as_u64())
            .unwrap_or(0);
        let short = series_of(profile, "hist", task)
            .and_then(|series| series["short"]["note"].as_str())
            .map(str::to_owned);
        let polled = series_of(run.labelled("on-cpu-polled"), "summary", task)
            .map(|series| {
                series["count"].as_f64().unwrap_or(0.0) * series["mean"].as_f64().unwrap_or(0.0)
            })
            .unwrap_or(0.0);
        let runnable = run.labelled("runnable");
        let blocked = run.labelled("blocked");
        Self {
            task,
            whole_low_ns: run.truth("wall_ns") as f64,
            whole_high_ns: profile["observed_ns"].as_f64().expect("a window"),
            samples,
            dropped: losses(profile, "dropped"),
            period_ns: 1e9 / hz,
            short,
            on_cpu_polled_ns: polled,
            runnable_ns: total(runnable, task, "total_ns") as f64,
            waits: total(runnable, task, "count"),
            off_cpu_ns: total(blocked, task, "total_ns") as f64,
            queued_ns: total(blocked, task, "queued_ns") as f64,
            blocks: total(blocked, task, "blocks"),
        }
    }

    fn on_cpu_ns(&self) -> f64 {
        self.samples as f64 * self.period_ns
    }

    /// How far the sampled row may sit from the kernel's own on-cpu figure: a
    /// count's own ±1, a few times over, and the interrupts that ran while the
    /// task held its cpu. A `cpu-clock` sample counts that time and the
    /// kernel's figure leaves it out; at 997 Hz it measured 0.7 % here, and it
    /// grows with the rate because the interrupts become the profiler's own
    /// (`cross-compute-lossy-profile.yml` has the sweep).
    fn on_cpu_slack_ns(&self) -> f64 {
        0.05 * self.on_cpu_ns() + 3.0 * self.period_ns
    }

    /// The time the dropped samples stood for, had they been read.
    fn dropped_ns(&self) -> f64 {
        self.dropped as f64 * self.period_ns
    }

    fn parts_ns(&self) -> f64 {
        self.on_cpu_ns() + self.runnable_ns + self.off_cpu_ns
    }

    /// The whole minus the parts, as a range, because the whole is one.
    fn residue_ns(&self) -> (f64, f64) {
        (
            self.whole_low_ns - self.parts_ns(),
            self.whole_high_ns - self.parts_ns(),
        )
    }

    /// The table `docs/scenarios/cross-compute.md` shows, beside the kernel's
    /// own figures for the same task.
    fn render(&self, run: &Run) -> String {
        let ms = |ns: f64| ns / 1e6;
        let share = |ns: f64| 100.0 * ns / self.whole_low_ns;
        let (low, high) = self.residue_ns();
        let mut text = format!(
            "where the time of task:{} went\n\
             \x20 STATE      SOURCE                       KIND      EVIDENCE        MS  SHARE\n\
             \x20 on cpu     cpu.profile x {:.3} ms     sampled   KEY       {:>8.1}  {:>4.1} %\n\
             \x20 runnable   sched.runqlat                traced    KEY       {:>8.1}  {:>4.1} %\n\
             \x20 off cpu    sched.offcpu                 traced    KEY       {:>8.1}  {:>4.1} %\n\
             \x20            of which queued after a wakeup, and also counted as runnable:\n\
             \x20            between 0 and {:.1} ms; no output says how much\n\
             \x20 parts                                                      {:>8.1}\n\
             \x20 whole      the workload's clock .. the run's window        {:.1} .. {:.1}\n\
             \x20 RESIDUE    whole - parts                                   {:+.1} .. {:+.1}\n\
             \x20 second opinion on the first row: cpu.oncpu_time, polled every 50 ms in 10 ms \
             ticks, {:.1} ms\n\
             \x20 interrupted: the sampled row counts the interrupts that ran while the task \
             held its cpu and the kernel's on-cpu figure does not; the difference is {:+.1} ms\n",
            self.task,
            ms(self.period_ns),
            ms(self.on_cpu_ns()),
            share(self.on_cpu_ns()),
            ms(self.runnable_ns),
            share(self.runnable_ns),
            ms(self.off_cpu_ns),
            share(self.off_cpu_ns),
            ms(self.runnable_ns),
            ms(self.parts_ns()),
            ms(self.whole_low_ns),
            ms(self.whole_high_ns),
            ms(low),
            ms(high),
            ms(self.on_cpu_polled_ns),
            ms(self.on_cpu_ns() - run.truth("oncpu_ns") as f64),
        );
        if let Some(note) = &self.short {
            text.push_str(&format!("  the profile says: {note}\n"));
            text.push_str(&format!(
                "  with the {} dropped samples counted back in, the parts are {:.1} ms and the \
                 residue {:+.1} .. {:+.1}\n",
                self.dropped,
                ms(self.parts_ns() + self.dropped_ns()),
                ms(low - self.dropped_ns()),
                ms(high - self.dropped_ns()),
            ));
        }
        text.push_str(&format!(
            "  the kernel's own: on cpu {:.1} ms, runnable {:.2} ms, {} voluntary switches; \
             the tool saw {} blocks and {} waits\n",
            ms(run.truth("oncpu_ns") as f64),
            ms(run.truth("runq_ns") as f64),
            run.truth("voluntary"),
            self.blocks,
            self.waits,
        ));
        text
    }

    /// The rows against the kernel's figures for the same task, each with its
    /// own reference's resolution as slack.
    fn check_against_the_kernel(&self, run: &Run) {
        let table = self.render(run);
        let kernel_on_cpu = run.truth("oncpu_ns") as f64;
        let kernel_runnable = run.truth("runq_ns") as f64;

        assert!(
            (self.on_cpu_ns() - kernel_on_cpu).abs() <= self.on_cpu_slack_ns(),
            "{} samples of {:.3} ms are {:.1} ms on cpu, and the kernel says {:.1} ms:\n{table}",
            self.samples,
            self.period_ns / 1e6,
            self.on_cpu_ns() / 1e6,
            kernel_on_cpu / 1e6
        );

        // A polled counter read in ticks, which misses whatever the task spent
        // after the last poll: short by at most a poll and a tick, and never
        // long by more than a tick.
        assert!(
            self.on_cpu_polled_ns <= kernel_on_cpu + TICK_NS
                && self.on_cpu_polled_ns >= kernel_on_cpu - POLL_NS - TICK_NS,
            "the polled row says {:.1} ms on cpu against the kernel's {:.1} ms, which is more \
             than one poll and one tick apart:\n{table}",
            self.on_cpu_polled_ns / 1e6,
            kernel_on_cpu / 1e6
        );

        let runnable_slack = 0.02 * kernel_runnable + self.waits as f64 * PER_WAIT_NS;
        assert!(
            (self.runnable_ns - kernel_runnable).abs() <= runnable_slack,
            "`sched.runqlat` summed {:.3} ms over {} waits and the kernel's run_delay is {:.3} \
             ms:\n{table}",
            self.runnable_ns / 1e6,
            self.waits,
            kernel_runnable / 1e6
        );

        // A voluntary switch is a block. The few that differ are the edge
        // `src/bpf/offcpu.rs` names: a task about to sleep with a signal
        // pending is read as blocked and then put back.
        let voluntary = run.truth("voluntary");
        let blocks_slack = voluntary / 100 + 3;
        assert!(
            self.blocks.abs_diff(voluntary) <= blocks_slack,
            "`sched.offcpu` counted {} blocks and the kernel {voluntary} voluntary \
             switches:\n{table}",
            self.blocks
        );
    }

    /// The parts against the whole. Neither bound can be exact, so both are
    /// asserted with what each one is known to be off by.
    fn check_against_the_whole(&self) {
        // The parts can exceed the lifetime by the overlap and by no more, and
        // the overlap is at most the runnable row.
        assert!(
            self.parts_ns() - self.runnable_ns - self.on_cpu_slack_ns() <= self.whole_high_ns,
            "even with every runnable nanosecond taken as counted twice, the parts ({:.1} ms) \
             exceed the run's window ({:.1} ms): something is counted that the task did not \
             live",
            self.parts_ns() / 1e6,
            self.whole_high_ns / 1e6
        );
        // And they cannot fall short of the workload's own clock: every
        // nanosecond of it was spent in one of the three states.
        assert!(
            self.parts_ns() + self.on_cpu_slack_ns() >= self.whole_low_ns,
            "the parts ({:.1} ms) fall short of the workload's own clock ({:.1} ms): a state is \
             missing from the decomposition",
            self.parts_ns() / 1e6,
            self.whole_low_ns / 1e6
        );
    }
}

// ------------------------------------------------- the numbers and the files

/// The constants above are ground truth only while the sessions still contain
/// them. A test and a session that disagree about the workload check nothing,
/// and would do it in green.
#[test]
fn the_sessions_still_say_what_these_tests_assert() {
    let halves = [format!("$i -lt {ROUND_TRIPS}"), format!("$j -lt {SPINS}")];
    let expected: Vec<(&str, String)> = vec![
        ("cross-compute-where-the-time-went.yml", halves[0].clone()),
        ("cross-compute-where-the-time-went.yml", halves[1].clone()),
        (
            "cross-compute-where-the-time-went.yml",
            format!("sample@{CLEAN_HZ}hz"),
        ),
        ("cross-compute-three-states.yml", halves[0].clone()),
        (
            "cross-compute-three-states.yml",
            format!("sample@{CLEAN_HZ}hz"),
        ),
        ("cross-compute-lossy-profile.yml", halves[0].clone()),
        (
            "cross-compute-lossy-profile.yml",
            format!("sample@{LOSSY_HZ}hz"),
        ),
        ("cross-compute-drained-wakeup.yml", halves[0].clone()),
        (
            "cross-compute-queued-after-wakeup.yml",
            format!("SLEEPS, EACH = {SLEEPS}, {}", EACH_MS as f64 / 1000.0),
        ),
    ];
    for (file, needle) in expected {
        let path = format!("examples/{file}");
        let text = std::fs::read_to_string(&path).expect("a session this crossing owns");
        assert!(
            text.contains(&needle),
            "{path} no longer contains `{needle}`, which tests/cross_compute.rs asserts against"
        );
    }
}

/// Every session this crossing ships validates, and there is no session with
/// the prefix that this file does not know about.
#[test]
fn every_session_of_this_crossing_validates_and_is_checked_here() {
    let known = [
        "cross-compute-drained-wakeup.yml",
        "cross-compute-lossy-profile.yml",
        "cross-compute-queued-after-wakeup.yml",
        "cross-compute-three-states.yml",
        "cross-compute-where-the-time-went.yml",
    ];
    let mut shipped: Vec<String> = std::fs::read_dir("examples")
        .expect("the examples directory")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("cross-compute-"))
        .collect();
    shipped.sort();
    assert_eq!(
        shipped, known,
        "a session was added or removed without being checked here"
    );
    for file in known {
        let output = Command::new(BINARY)
            .args(["validate", "-f", &format!("examples/{file}")])
            .output()
            .expect("omn should run");
        assert!(output.status.success(), "{file}: {}", spoken(&output));
    }
}

// ------------------------------------------------------------ the crossings

/// The half-blocked program, decomposed, and every row checked against the
/// kernel's accounting of the same task.
///
/// The property the workload was built for: the driver's on-cpu time and its
/// blocked time are equal by construction, because the same loop bound is
/// written for both ends. The rows must find both halves; on an idle machine
/// the runnable row is a fraction of a percent, and nothing here asserts that,
/// because it is a fact about the machine and not about the tool.
#[test]
fn a_half_blocked_task_decomposes_into_what_the_kernel_says_it_did() {
    let _alone = one_at_a_time();
    let Some(run) = run(
        "cross-compute-where-the-time-went.yml",
        &[],
        "that on cpu, runnable and blocked each match the kernel's schedstat for the same task, \
         and that the three bracket the task's wall clock",
    ) else {
        return;
    };
    let decomposition = Decomposition::from(&run, CLEAN_HZ);
    eprintln!("{}", decomposition.render(&run));

    decomposition.check_against_the_kernel(&run);
    decomposition.check_against_the_whole();

    // Half blocked by construction, checked against the kernel rather than as
    // a share of the wall clock: the responder spins only after the driver
    // has written and before it answers, and in that time the driver is
    // blocked on the fifo -- or, on a loaded machine, runnable and waiting
    // for a cpu, preempted between its write and its read. So the driver's
    // blocked time less its queue wait, plus every wait for a cpu the kernel
    // counted for it, holds at least the responder's on-cpu time by the
    // kernel's count, less the few microseconds a round the two can run at
    // once (the driver's syscall around the write, the responder's around its
    // answer). A share of the window moved with whatever else the machine
    // ran; this does not.
    let responder = run.truth("responder_oncpu_ns") as f64;
    let blocked_less_queued = decomposition.off_cpu_ns - decomposition.queued_ns;
    let waited_for_a_cpu = run.truth("runq_ns") as f64;
    let per_round = ROUND_TRIPS as f64 * PER_WAIT_NS;
    eprintln!(
        "blocked less queued {:.1} ms, waiting for a cpu {:.1} ms, the responder on the cpu \
         {:.1} ms",
        blocked_less_queued / 1e6,
        waited_for_a_cpu / 1e6,
        responder / 1e6
    );
    let lost = losses(run.labelled("blocked"), "dropped");
    assert!(
        lost > 0 || blocked_less_queued + waited_for_a_cpu >= responder - per_round,
        "the driver was blocked {:.1} ms less {:.1} ms queued, and waited {:.1} ms for a cpu, \
         while the responder, which runs only while the driver is not, was on the cpu {:.1} ms \
         by the kernel's count:\n{}",
        decomposition.off_cpu_ns / 1e6,
        decomposition.queued_ns / 1e6,
        waited_for_a_cpu / 1e6,
        responder / 1e6,
        decomposition.render(&run)
    );
    // And the mark is there exactly when the profile lost samples: a clean
    // run gains none, and a lossy one under load is marked, as it must be.
    assert_eq!(
        decomposition.short.is_some(),
        decomposition.dropped > 0,
        "the profile dropped {} samples and its mark is {:?}",
        decomposition.dropped,
        decomposition.short
    );
}

/// What released each run of the task, joined per switch at WAKEUP.
///
/// Ground truth: `sched.switches` is one event per switch, and the kernel
/// counts the task's timeslices itself; the two must agree, and the join must
/// account for every switch it was given. And every chain it reports must end
/// at the task that was switched out -- a chain ending anywhere else is a state
/// the engine invented.
#[test]
fn every_switch_of_the_task_is_credited_to_the_wakeup_that_released_it() {
    let _alone = one_at_a_time();
    let Some(run) = run(
        "cross-compute-where-the-time-went.yml",
        &[],
        "that every switch of the driver is attributed to a recorded wakeup at WAKEUP, and that \
         the switches match the kernel's own timeslice count",
    ) else {
        return;
    };
    let task = run.task();
    let switches = run.labelled("switched-out")["events_seen"]
        .as_u64()
        .expect("an event count");
    let slices = run.truth("slices");
    assert!(
        switches.abs_diff(slices) <= slices / 100 + 3,
        "`sched.switches` saw {switches} switches and the kernel counts {slices} timeslices"
    );
    assert!(
        switches >= ROUND_TRIPS,
        "{ROUND_TRIPS} round trips are at least {ROUND_TRIPS} switches, saw {switches}"
    );

    let join = run
        .joins
        .iter()
        .find(|join| join["rule"] == "what-released-each-run")
        .expect("the session declares the per-switch join");
    assert_eq!(join["evidence"], "WAKEUP", "{join}");
    let matched = join["matched"].as_u64().expect("a count");
    let unmatched = join["unmatched"].as_u64().expect("a count");
    // And the two the run counts apart: a switch with no task key, and one
    // stamped outside the wakeup source's window, which on a loaded machine
    // the run reports and which is not a switch nothing released.
    let apart =
        join["keyless"].as_u64().unwrap_or(0) + join["outside_window"].as_u64().unwrap_or(0);
    assert_eq!(
        matched + unmatched + apart,
        switches,
        "every switch is either attributed, counted as unattributed, or counted outside the \
         wakeup source's window ({matched} + {unmatched} + {apart})"
    );
    // A switch before the first wakeup the run saw has no releaser: the gate
    // the program is held at, and nothing else.
    assert!(
        unmatched <= 3,
        "{unmatched} switches followed no wakeup, and only the first few can"
    );

    let tail = format!("task:{task}");
    for attribution in join["top"].as_array().expect("attributions") {
        let chain = attribution["key"].as_str().expect("a chain");
        assert!(
            chain.ends_with(&tail),
            "a chain that does not end at the switched-out task is an invented state: {chain}"
        );
    }

    // Every block of the driver ends in a wakeup, and the wakeup source's own
    // count per woken task says how many without the join. The reference is
    // the kernel's count of the driver's blocks, its voluntary switches, not
    // the 600 round trips: on a loaded machine an answer can arrive before
    // the driver has gone to sleep for it, and then nothing needs waking --
    // one run beside a spinning load counted 523.
    let wakeups = run.labelled("released-by");
    let driver_woken = series_of(wakeups, "topk", task)
        .and_then(|series| series["events"].as_u64())
        .unwrap_or(0);
    let voluntary = run.truth("voluntary");
    let lost = losses(wakeups, "dropped");
    assert!(
        driver_woken.abs_diff(voluntary) <= voluntary / 100 + 3 + lost
            && driver_woken <= ROUND_TRIPS + ROUND_TRIPS / 10,
        "the driver blocked {voluntary} times by the kernel's count, and was woken \
         {driver_woken} times, with {lost} wakeups counted lost"
    );
}

/// All three states large at once, and the rows still checked against the
/// kernel.
///
/// The workload creates the property: the driver, its responder and a
/// spinning shell share one cpu, so the driver waits for a turn after every
/// wakeup. Where it does not -- a scheduler that found another cpu anyway --
/// the test says so with the numbers and stops, because a decomposition with
/// an empty row cannot show the rows overlapping.
#[test]
fn three_states_at_once_still_match_the_kernel_and_their_residue_is_printed() {
    let _alone = one_at_a_time();
    let Some(run) = run(
        "cross-compute-three-states.yml",
        &["taskset", "timeout"],
        "that with all three states large, each still matches the kernel's schedstat and the \
         parts still bracket the wall clock",
    ) else {
        return;
    };
    let decomposition = Decomposition::from(&run, CLEAN_HZ);
    let table = decomposition.render(&run);
    eprintln!("{table}");

    if decomposition.runnable_ns < 0.10 * decomposition.whole_low_ns {
        eprintln!(
            "skipped: the spinning shell was meant to make the driver wait for its cpu, and it \
             waited {:.1} ms of {:.1} ms -- under a tenth. The kernel's own run_delay is {:.1} \
             ms, so this is the scheduler finding room, not the tool missing waits:\n{table}",
            decomposition.runnable_ns / 1e6,
            decomposition.whole_low_ns / 1e6,
            run.truth("runq_ns") as f64 / 1e6
        );
        return;
    }
    for (state, ns) in [
        ("on cpu", decomposition.on_cpu_ns()),
        ("off cpu", decomposition.off_cpu_ns),
    ] {
        assert!(
            ns >= 0.10 * decomposition.whole_low_ns,
            "{state} is under a tenth of the task's life, and the workload makes all three \
             large:\n{table}"
        );
    }
    decomposition.check_against_the_kernel(&run);
    decomposition.check_against_the_whole();
}

/// The overlap between `sched.offcpu` and `sched.runqlat`, measured against a
/// known amount of sleep.
///
/// A task sleeps 100 times for 5 ms, in `SCHED_BATCH` beside a spinning shell
/// on one cpu, so every wakeup waits for the next tick. The kernel reports the
/// loop's own queue wait. Ground truth, all of it from the workload:
///
/// - the task was asleep at least 500 ms, and at most that plus the timer's
///   slack;
/// - `sched.offcpu` must be the loop's clock minus the loop's cpu, which is
///   sleep plus queue: the queue wait is inside it;
/// - and the part of it that is not sleep must be the kernel's queue wait,
///   which `sched.runqlat` reports too.
#[test]
fn off_cpu_contains_the_queue_wait_that_runqlat_also_counts() {
    let _alone = one_at_a_time();
    let Some(run) = run(
        "cross-compute-queued-after-wakeup.yml",
        &["taskset", "timeout", "python3"],
        "that sched.offcpu reports 500 ms of requested sleep plus exactly the kernel's queue \
         wait after each wakeup, which sched.runqlat also counts",
    ) else {
        return;
    };
    let task = run.task();
    let requested = run.truth("requested_ns") as f64;
    let loop_ns = run.truth("loop_ns") as f64;
    let loop_on_cpu = run.truth("loop_oncpu_ns") as f64;
    let loop_queued = run.truth("loop_runq_ns") as f64;
    let slack = SLEEPS as f64 * SLACK_PER_SLEEP_NS;

    let blocked = run.labelled("blocked");
    let sleeping: Vec<&Value> = raw_events(blocked)
        .filter(|event| tid_of(event) == Some(task))
        .filter(|event| {
            event["attrs"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|attr| attr["name"] == "state" && attr["value"] == "sleeping")
        })
        .collect();
    let off_cpu: f64 = sleeping
        .iter()
        .filter_map(|event| attr(event, "total_ns"))
        .sum::<u64>() as f64;
    // What the source now says the wait was, rather than what this test used to
    // derive by subtraction. The eighth wave's coordinator pass added it:
    // `sched.offcpu` stamps the wakeup into its in-flight entry from the same
    // tracepoint `sched.runqlat` measures from, so the two talk about one wait.
    let queued: f64 = sleeping
        .iter()
        .filter_map(|event| attr(event, "queued_ns"))
        .sum::<u64>() as f64;
    let blocked_only: f64 = sleeping
        .iter()
        .filter_map(|event| attr(event, "blocked_ns"))
        .sum::<u64>() as f64;
    let blocks: u64 = sleeping
        .iter()
        .filter_map(|event| attr(event, "blocks"))
        .sum();
    let runnable = total(run.labelled("runnable"), task, "total_ns") as f64;
    let ms = |ns: f64| ns / 1e6;
    let summary = format!(
        "requested {:.1} ms; sched.offcpu {:.1} ms over {blocks} blocks; the loop's clock {:.1} \
         ms, of which {:.2} ms on cpu and {:.1} ms queued by the kernel's count; sched.runqlat \
         {:.2} ms for the process against the kernel's {:.2} ms",
        ms(requested),
        ms(off_cpu),
        ms(loop_ns),
        ms(loop_on_cpu),
        ms(loop_queued),
        ms(runnable),
        ms(run.truth("runq_ns") as f64)
    );
    eprintln!("{summary}");

    if loop_queued < 4.0 * slack {
        eprintln!(
            "skipped: the spinning shell was meant to make every wakeup wait for a cpu, and the \
             kernel says the loop waited {:.1} ms in all, which the timer's own slack of {:.1} \
             ms could hide. Nothing about the overlap can be shown from that: {summary}",
            ms(loop_queued),
            ms(slack)
        );
        return;
    }

    // Every sleep is a block; the interpreter's own start adds a few short
    // ones before the loop, which is why the count is bounded and not exact.
    assert!(
        (SLEEPS..=SLEEPS + 10).contains(&blocks),
        "{SLEEPS} sleeps are {SLEEPS} blocks, and a few more from starting python: {summary}"
    );
    assert!(
        off_cpu >= requested,
        "a task cannot be off the cpu for less than it asked to sleep: {summary}"
    );
    // The row is the loop's wall clock minus its cpu. The slack is the loop's
    // own edges: the first `sched_setscheduler` and the last clock read are
    // inside the loop's clock and are not blocks.
    let off_cpu_by_the_loop = loop_ns - loop_on_cpu;
    assert!(
        (off_cpu - off_cpu_by_the_loop).abs() <= slack,
        "sched.offcpu should be the loop's clock minus its cpu, sleep and queue together: \
         {summary}"
    );
    // What is not sleep is at least the queue wait. It used to be asserted as
    // *equal* to it within the timer's slack, and that was the assertion this
    // test failed on under load: the excess is the queue wait plus however far
    // past 5 ms each sleep actually ran, and on a loaded machine the second
    // term outgrows a slack calibrated for an idle one. It was a proxy for a
    // number the tool could not report; the tool reports it now, and the
    // direct comparison below is both stricter and load-independent.
    let excess = off_cpu - requested;
    assert!(
        excess >= loop_queued - slack,
        "sched.offcpu exceeds the requested sleep by {:.1} ms, which cannot be less than the \
         {:.1} ms the kernel says the loop spent queued: {summary}",
        ms(excess),
        ms(loop_queued)
    );
    // And the source says it itself now, which is the part this test used to
    // have to derive. Checked against the kernel's own `run_delay` for the
    // loop, which is an independent counter and not a rearrangement of the
    // tool's own numbers: 93.6 ms against the kernel's 96.5 ms when this was
    // written, a difference of under 30 microseconds per wakeup.
    assert!(
        (queued - loop_queued).abs() <= slack,
        "the queue wait `sched.offcpu` reports must be the kernel's own: {:.1} ms against \
         {:.1} ms, slack {:.1} ms: {summary}",
        ms(queued),
        ms(loop_queued),
        ms(slack)
    );
    // The split is arithmetic and cannot drift from the total it came out of.
    assert!(
        (blocked_only + queued - off_cpu).abs() < 1.0,
        "blocked + queued must be the whole block: {:.1} + {:.1} != {:.1}",
        ms(blocked_only),
        ms(queued),
        ms(off_cpu)
    );
    // Which is what makes the row a decomposition can use: taking the queue
    // wait out moves the block towards the sleep that was asked for, and does
    // not take it past. Asserted as a direction rather than as a distance --
    // how far past 5 ms a sleep runs belongs to the machine, and that it is
    // closer once the queue wait is out belongs to the tool.
    assert!(
        blocked_only >= requested - slack,
        "a task cannot have slept less than it asked once the queue wait is out: {:.1} ms \
         against {:.1} ms: {summary}",
        ms(blocked_only),
        ms(requested)
    );
    assert!(
        (blocked_only - requested).abs() < (off_cpu - requested).abs(),
        "taking the queue wait out has to bring the block closer to the requested sleep: \
         {:.1} ms against {:.1} ms, both against {:.1} ms: {summary}",
        ms(blocked_only),
        ms(off_cpu),
        ms(requested)
    );

    // Which sched.runqlat counts as well, for the whole process.
    let kernel_runnable = run.truth("runq_ns") as f64;
    let waits = total(run.labelled("runnable"), task, "count");
    assert!(
        (runnable - kernel_runnable).abs() <= 0.02 * kernel_runnable + waits as f64 * PER_WAIT_NS,
        "sched.runqlat should match the kernel's run_delay for the process: {summary}"
    );
    assert!(
        runnable >= loop_queued - slack,
        "the loop's queue wait is in sched.runqlat as well as in sched.offcpu, so the first \
         cannot be smaller than it: {summary}"
    );
}

/// A lossy run's decomposition beside a clean one: what the mark changes.
///
/// The profile at 100 kHz is past what the kernel will deliver, so samples
/// are dropped and throttled and the run must say so; the traced rows keep
/// their totals in the kernel and must still match it. Where this machine
/// delivers every sample even at that rate, there is no loss to show and the
/// test stops, with the numbers.
///
/// What is deliberately not asserted: that the dropped samples close the
/// residue exactly. They close it to within the windows the kernel throttled,
/// and how much time those held is recorded nowhere; the document shows the
/// residue before and after, and a test that asserted the second would be
/// asserting how long this kernel's throttling lasted.
#[test]
fn a_lossy_profile_marks_its_row_and_leaves_the_traced_rows_alone() {
    let _alone = one_at_a_time();
    let Some(clean) = run(
        "cross-compute-where-the-time-went.yml",
        &[],
        "that a clean run's decomposition carries no mark",
    ) else {
        return;
    };
    let Some(lossy) = run(
        "cross-compute-lossy-profile.yml",
        &[],
        "that a lossy profile marks its on-cpu row and the traced rows still match the kernel",
    ) else {
        return;
    };
    let clean_table = Decomposition::from(&clean, CLEAN_HZ);
    let lossy_table = Decomposition::from(&lossy, LOSSY_HZ);
    eprintln!("clean:\n{}", clean_table.render(&clean));
    eprintln!("lossy:\n{}", lossy_table.render(&lossy));

    let profile = lossy.labelled("on-cpu");
    let dropped = losses(profile, "dropped");
    let throttled = losses(profile, "throttled");
    if dropped == 0 && throttled == 0 {
        eprintln!(
            "skipped: {} samples at {LOSSY_HZ} Hz and none dropped or throttled -- this kernel \
             delivers more than `kernel.perf_event_max_sample_rate` usually allows, so there is \
             no lossy run to put beside the clean one",
            lossy_table.samples
        );
        return;
    }

    assert!(
        clean_table.short.is_none(),
        "a clean run gains nothing, or the mark is noise: {:?}",
        clean_table.short
    );
    let short = series_of(profile, "hist", lossy.task())
        .map(|series| series["short"].clone())
        .unwrap_or(Value::Null);
    assert!(
        !short.is_null(),
        "the run dropped {dropped} samples and was throttled {throttled} times, and its \
         histogram does not say so:\n{}",
        lossy.text
    );
    assert_eq!(short["dropped"].as_u64(), Some(dropped), "{short}");
    assert_eq!(short["throttled"].as_u64(), Some(throttled), "{short}");

    // The samples the mark says were dropped are time some sampled task spent
    // holding its cpu. Counted back in, they must still fit in the room the
    // sampled tasks' windows leave -- a mark that overstated the loss would
    // push them past it.
    //
    // Which task a drop belonged to is not in the output: the profile follows
    // the processes the target starts (the workload's `{ ... } &` half is one),
    // each with a ring of its own, and the mark is run-wide by design
    // (`emit::Shortfall::records`). So the drops are weighed against every
    // sampled task's room at once: the decomposed task's window less its
    // parts, and each other task's window less what its own samples hold.
    // When the target was the only task sampled, that is the old bound
    // exactly: every drop was its time.
    let others_ns: Vec<f64> = outputs(profile, "topk")
        .flat_map(|output| output["series"].as_array().into_iter().flatten())
        .filter(|series| series["key"] != format!("task:{}", lossy.task()).as_str())
        .map(|series| series["events"].as_f64().unwrap_or(0.0) * lossy_table.period_ns)
        .collect();
    let own_room =
        lossy_table.whole_high_ns + lossy_table.runnable_ns + lossy_table.on_cpu_slack_ns()
            - lossy_table.parts_ns();
    let room = own_room
        + others_ns
            .iter()
            .map(|held| (lossy_table.whole_high_ns - held).max(0.0))
            .sum::<f64>();
    assert!(
        lossy_table.dropped_ns() <= room,
        "the {dropped} dropped samples stand for {:.1} ms, and the {} sampled tasks' windows \
         leave room for {:.1} ms ({:.1} ms of it the decomposed task's):\n{}",
        lossy_table.dropped_ns() / 1e6,
        others_ns.len() + 1,
        room / 1e6,
        own_room / 1e6,
        lossy_table.render(&lossy)
    );

    // The traced rows are totals kept in the kernel: nothing the profile's
    // ring lost is missing from them.
    for table in [&clean_table, &lossy_table] {
        let run = if std::ptr::eq(table, &clean_table) {
            &clean
        } else {
            &lossy
        };
        let voluntary = run.truth("voluntary");
        assert!(
            table.blocks.abs_diff(voluntary) <= voluntary / 100 + 3,
            "`sched.offcpu` counted {} blocks against {voluntary} voluntary switches",
            table.blocks
        );
        let kernel_runnable = run.truth("runq_ns") as f64;
        assert!(
            (table.runnable_ns - kernel_runnable).abs()
                <= 0.02 * kernel_runnable + table.waits as f64 * PER_WAIT_NS,
            "`sched.runqlat` summed {:.3} ms against the kernel's {:.3} ms",
            table.runnable_ns / 1e6,
            kernel_runnable / 1e6
        );
    }
}

/// A wakeup join onto a drained total is refused, and the reason is measured.
///
/// This pinned finding 5 while the engine accepted the rule: eleven drained
/// events standing for six hundred blocks, reported as `11 of 11 attributed`
/// at WAKEUP. The eighth wave's stream J made it a refusal before the run
/// (`docs/families/join.md`), so the test now asks for the rule the session
/// keeps as a comment and requires the refusal, and still runs the session to
/// measure what the refusal rests on: each event holds many blocks.
#[test]
fn a_wakeup_join_onto_drained_totals_is_refused_because_each_event_holds_many_blocks() {
    let _alone = one_at_a_time();
    let text =
        std::fs::read_to_string("examples/cross-compute-drained-wakeup.yml").expect("the session");
    let rule: String = text
        .lines()
        .skip_while(|line| !line.starts_with("# joins:"))
        .take_while(|line| line.starts_with('#'))
        .map(|line| line.strip_prefix("# ").unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        rule.contains("to: sched.offcpu"),
        "the session keeps the refused rule as a comment: {rule}"
    );
    let directory = std::env::temp_dir().join(format!("omn-drained-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    let path = directory.join("drained.yml");
    std::fs::write(&path, format!("{text}\n{rule}\n")).expect("the session with its join");
    let output = Command::new(BINARY)
        .args(["validate", "-f", path.to_str().expect("a utf-8 path")])
        .output()
        .expect("omn should run");
    let _ = std::fs::remove_dir_all(&directory);
    let said = spoken(&output);
    assert_eq!(output.status.code(), Some(2), "{said}");
    assert!(said.contains("drained"), "{said}");
    assert!(said.contains("sched.switches"), "{said}");

    let Some(run) = run(
        "cross-compute-drained-wakeup.yml",
        &[],
        "that each drained sched.offcpu event stands for many blocks, which is why a wakeup \
         join onto it is refused",
    ) else {
        return;
    };
    let task = run.task();
    let blocked = run.labelled("blocked");
    let drained = blocked["events_seen"].as_u64().expect("an event count");
    let blocks = total(blocked, task, "blocks");
    let voluntary = run.truth("voluntary");
    assert!(
        blocks.abs_diff(voluntary) <= voluntary / 100 + 3,
        "the blocks are the kernel's voluntary switches: {blocks} against {voluntary}"
    );
    assert!(
        run.joins.is_empty(),
        "the session no longer declares the refused join"
    );
    assert!(
        blocks >= 10 * drained,
        "{ROUND_TRIPS} round trips drained every 100 ms put many blocks in each event; {blocks} \
         blocks in {drained} events"
    );
}
