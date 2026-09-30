//! The machine as a confounder: does the machine's own state explain what was
//! just measured?
//!
//! Two crossings, each checked against a condition this file creates on
//! purpose, so that what the tool attributes can be compared with what was
//! actually done:
//!
//! - **The clock under an instruction rate.** `examples/cross-machine-
//!   frequency.yml` collects `cpu.instructions`, `cpu.cycles` and `power.freq`
//!   on one core, and a second session beside it counts the loop's own
//!   instructions and cycles by pid. An instruction rate is `IPC x frequency x
//!   residue`, and the test changes the same spin loop's rate by two different
//!   means: loading every *other* physical core, which takes the package's
//!   boost budget and lowers this core's clock without touching its pipeline,
//!   and releasing its SMT *sibling*, which gives it back its issue width at
//!   the same clock. The tool must put the first change on the frequency and
//!   the second on the IPC. A `power.freq` that read the requested frequency,
//!   or a decomposition that attributed every change to the clock, fails one
//!   half.
//!
//! - **The I/O a memory limit made.** `examples/cross-machine-reclaim-io.yml`
//!   collects device bytes, memory pressure, reclaim and every block request.
//!   A program writes 96 MiB and reads it back, once in a cgroup capped at
//!   32 MiB and once uncapped. Same code, same system calls: capped, the read
//!   goes to the disk; uncapped, it never leaves the page cache. The program
//!   reports its own `/proc/self/io` and the cgroup's `memory.stat` is read
//!   before and after, so each rung of the evidence ladder is checked against
//!   a reference the tool did not produce.
//!
//! What cannot be created here is skipped by name, with the numbers that show
//! why: a machine whose clock does not respond to package load has no
//! frequency confounder to separate, and one with no delegated memory
//! controller cannot be given a limit. `docs/scenarios/cross-machine.md` is the
//! record of what was measured and what is missing.

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use serde_json::Value;

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

fn example(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join(name)
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Runs a session from standard input, as JSON.
///
/// The shipped sessions are read and rewritten rather than copied here, so a
/// change to one of them is a change to what this file checks.
fn run_session(session: &str, extra: &[&str]) -> Output {
    spawn_session(session, extra)
        .wait_with_output()
        .expect("omn should finish")
}

fn spawn_session(session: &str, extra: &[&str]) -> Child {
    use std::io::Write;

    let mut child = Command::new(BINARY)
        .args(["-f", "-", "--input-format", "yaml", "--format", "json"])
        .args(extra)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(session.as_bytes())
        .expect("writing the session");
    child
}

fn document(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "the session did not print JSON ({error}); it said:\n{}",
            stderr(output)
        )
    })
}

fn result<'a>(document: &'a Value, label: &str) -> &'a Value {
    document["results"]
        .as_array()
        .expect("a document carries results")
        .iter()
        .find(|result| result["label"] == label)
        .unwrap_or_else(|| panic!("no result labelled `{label}`"))
}

fn output_of<'a>(result: &'a Value, emit: &str) -> &'a Value {
    result["outputs"]
        .as_array()
        .expect("a result carries outputs")
        .iter()
        .find(|output| output["emit"] == emit)
        .unwrap_or_else(|| panic!("`{}` has no `{emit}` output", result["label"]))
}

fn raw_events(result: &Value) -> Vec<&Value> {
    output_of(result, "raw")["events"]
        .as_array()
        .map(|events| events.iter().collect())
        .unwrap_or_default()
}

fn attr<'a>(event: &'a Value, name: &str) -> Option<&'a Value> {
    event["attrs"]
        .as_array()?
        .iter()
        .find(|attr| attr["name"] == name)
        .map(|attr| &attr["value"])
}

fn attr_u64(event: &Value, name: &str) -> u64 {
    attr(event, name).and_then(Value::as_u64).unwrap_or(0)
}

fn attr_str<'a>(event: &'a Value, name: &str) -> &'a str {
    attr(event, name).and_then(Value::as_str).unwrap_or("")
}

/// Whether anything a total is made of went missing. A late tick is not in the
/// list: a counter keeps counting while its reader is late, so the total read
/// at the end is whole, and a polled series that ran late is checked where it
/// matters rather than here.
fn nothing_went_missing(result: &Value) -> bool {
    ["dropped", "failed_reads", "undecoded", "throttled"]
        .iter()
        .all(|loss| result["losses"][loss].as_u64().unwrap_or(0) == 0)
}

fn mib(bytes: f64) -> f64 {
    bytes / (1u64 << 20) as f64
}

/// The crossings that load the machine on purpose, held one at a time.
///
/// Both put load where another scenario in this file measures: the frequency
/// conditions spin most of the machine's cores, and a reclaiming cgroup writes
/// to the disk the uncapped run is watching. The lock protects the condition
/// the test created, which is the whole of what is being checked.
static THE_MACHINE: Mutex<()> = Mutex::new(());

fn alone() -> MutexGuard<'static, ()> {
    THE_MACHINE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// The clock under an instruction rate
// ---------------------------------------------------------------------------

/// Spin loops pinned to named cpus, started from outside the tool.
///
/// `taskset` calls `sched_setaffinity`; nothing in `omn` chose the cores. A
/// loop that never blocks keeps its core out of every idle state, which is
/// what makes "this core was busy for the whole window" a fact about the run
/// rather than something the tool reported about itself.
struct Spinners(Vec<Child>);

impl Spinners {
    fn on(cpus: &[usize]) -> Option<Self> {
        // Held from the start, so a spawn that fails halfway kills the loops
        // already running rather than leaving them spinning.
        let mut spinners = Self(Vec::new());
        for cpu in cpus {
            let child = Command::new("taskset")
                .args([
                    "-c",
                    &cpu.to_string(),
                    "/bin/sh",
                    "-c",
                    "while :; do :; done",
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok()?;
            spinners.0.push(child);
        }
        Some(spinners)
    }
}

impl Drop for Spinners {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// A cpu list as sysfs writes it: `0-3,8,10-11`.
fn cpu_list(text: &str) -> Vec<usize> {
    let mut cpus = Vec::new();
    for part in text.trim().split(',').filter(|part| !part.is_empty()) {
        match part.split_once('-') {
            Some((low, high)) => {
                if let (Ok(low), Ok(high)) = (low.parse::<usize>(), high.parse::<usize>()) {
                    cpus.extend(low..=high);
                }
            }
            None => cpus.extend(part.parse::<usize>().ok()),
        }
    }
    cpus
}

fn read(path: impl AsRef<Path>) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// The online cpus, grouped into physical cores by their SMT siblings.
fn physical_cores() -> Vec<Vec<usize>> {
    let online = read("/sys/devices/system/cpu/online")
        .map(|text| cpu_list(&text))
        .unwrap_or_default();
    let mut cores: Vec<Vec<usize>> = Vec::new();
    for cpu in online {
        let siblings = read(format!(
            "/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list"
        ))
        .map(|text| cpu_list(&text))
        .unwrap_or_else(|| vec![cpu]);
        if !cores.contains(&siblings) {
            cores.push(siblings);
        }
    }
    cores
}

/// One run of the frequency crossing, reduced to its three factors.
///
/// Two sessions over the same second. The shipped one is the core's: its
/// clock by `power.freq`, and its cycles by the PMU, which is how the two
/// instruments are held against each other. The second is the loop's own
/// counters, by pid, because a counter on a core counts whatever ran there:
/// on a shared machine with nowhere idle to go, a stranger time-shares the
/// measured core and a core-wide count adds its instructions to the loop's.
/// Two runs because a run used to observe one target. It now observes one
/// workload beside any number of machine targets, so the loop's pid could
/// join the core's session as two more requests; the test keeps its two
/// sessions, whose windows it already holds against each other.
#[derive(Debug, Clone, Copy)]
struct Factors {
    /// The loop's own retired instructions and cycles, and the window they
    /// were counted over.
    instructions: f64,
    cycles: f64,
    wall_s: f64,
    /// The mean of `power.freq`'s readings over the window, in Hz.
    hz: f64,
    /// How far apart the readings were, as a share of their mean: the
    /// frequency instrument's own resolution over this run.
    spread: f64,
    /// The core's cycles over its own window, from the shipped session.
    core_cycles: f64,
    core_wall_s: f64,
}

impl Factors {
    fn rate(&self) -> f64 {
        self.instructions / self.wall_s
    }

    fn ipc(&self) -> f64 {
        self.instructions / self.cycles
    }

    /// The share of the window the loop spent on its core at the clock sysfs
    /// reported. One when it had the core to itself, less when something else
    /// was given a turn, and printed whatever it is: a decomposition whose
    /// parts do not multiply back to the whole is hiding a state.
    fn residue(&self) -> f64 {
        self.cycles / (self.wall_s * self.hz)
    }

    /// The core's cycles over wall time against the clock sysfs reported. On a
    /// core held busy these are one number read by two instruments.
    fn agreement(&self) -> f64 {
        self.core_cycles / (self.core_wall_s * self.hz)
    }
}

enum Measured {
    Factors(Factors),
    /// The machine cannot serve this crossing, and why.
    Unservable(String),
}

fn frequency_session(cpu: usize) -> String {
    let text = std::fs::read_to_string(example("cross-machine-frequency.yml"))
        .expect("the frequency session ships");
    assert!(
        text.contains("cpu: \"2\""),
        "the shipped session no longer names core 2, so this test cannot point it elsewhere"
    );
    text.replace("cpu: \"2\"", &format!("cpu: \"{cpu}\""))
}

fn loop_session(pid: u32) -> String {
    format!(
        "\
name: the loop's own counters
requests:
  - label: instructions
    source: cpu.instructions
    target: {{pid: [{pid}]}}
    mode: count
    emit: [rate]
  - label: cycles
    source: cpu.cycles
    target: {{pid: [{pid}]}}
    mode: count
    emit: [rate]
"
    )
}

/// A counter's total and the window it was read over, in seconds.
fn count(document: &Value, label: &str) -> (f64, f64) {
    let result = result(document, label);
    assert!(
        nothing_went_missing(result),
        "`{label}` is missing part of its total: {}",
        result["losses"]
    );
    let series = &output_of(result, "rate")["series"][0];
    (
        series["total"].as_f64().expect("a rate carries a total"),
        series["window_ns"]
            .as_f64()
            .expect("a rate carries its window")
            / 1e9,
    )
}

fn measure_frequency(cpu: usize, pid: u32) -> Measured {
    let core = spawn_session(&frequency_session(cpu), &["-d", "1s"]);
    let own = spawn_session(&loop_session(pid), &["-d", "1s"]);
    let core = core.wait_with_output().expect("omn should finish");
    let own = own.wait_with_output().expect("omn should finish");
    for (what, output) in [("the core's", &core), ("the loop's", &own)] {
        if !output.status.success() {
            return Measured::Unservable(format!(
                "{what} session did not run on core {cpu}: {}",
                stderr(output).lines().next().unwrap_or("").trim()
            ));
        }
    }
    let core = document(&core);
    let own = document(&own);
    let (instructions, wall_s) = count(&own, "instructions");
    let (cycles, _) = count(&own, "cycles");
    let (core_cycles, core_wall_s) = count(&core, "cycles");

    let readings = raw_events(result(&core, "frequency"));
    if let Some(requested) = readings
        .iter()
        .find(|event| attr_str(event, "measures") != "effective")
    {
        return Measured::Unservable(format!(
            "`power.freq` reads the {} frequency here ({}), which is what the governor asked \
             for rather than what the core ran at, so it cannot explain a slowdown",
            attr_str(requested, "measures"),
            attr_str(requested, "basis"),
        ));
    }
    let hz: Vec<f64> = readings
        .iter()
        .filter_map(|event| attr(event, "hz").and_then(Value::as_f64))
        .collect();
    assert!(
        hz.len() >= 5,
        "a one-second window polled every 100 ms gave {} frequency readings",
        hz.len()
    );
    let mean = hz.iter().sum::<f64>() / hz.len() as f64;
    let (low, high) = hz.iter().fold((f64::MAX, f64::MIN), |(low, high), &v| {
        (low.min(v), high.max(v))
    });
    Measured::Factors(Factors {
        instructions,
        cycles,
        wall_s,
        hz: mean,
        spread: (high - low) / mean,
        core_cycles,
        core_wall_s,
    })
}

/// How the change from one run to another splits across the three factors,
/// in logarithms so that the parts add: `ln rate = ln ipc + ln hz + ln residue`
/// exactly, and printing all four is what shows nothing was rounded away.
struct Split {
    rate: f64,
    ipc: f64,
    hz: f64,
    residue: f64,
}

impl Split {
    fn between(reference: &Factors, run: &Factors) -> Self {
        Self {
            rate: (run.rate() / reference.rate()).ln(),
            ipc: (run.ipc() / reference.ipc()).ln(),
            hz: (run.hz / reference.hz).ln(),
            residue: (run.residue() / reference.residue()).ln(),
        }
    }
}

fn verdict_line(name: &str, run: &Factors, split: Option<&Split>) -> String {
    let mut line = format!(
        "  {name:<24} {:>6.3} GHz  IPC {:>5.3}  {:>6.3} Ginstr/s  residue {:.3}  \
         (core cycles / clock {:.3})",
        run.hz / 1e9,
        run.ipc(),
        run.rate() / 1e9,
        run.residue(),
        run.agreement(),
    );
    if let Some(split) = split {
        line.push_str(&format!(
            "  | rate {:+6.1} % = clock {:+6.1} % + IPC {:+6.1} % + residue {:+5.1} %",
            split.rate * 100.0,
            split.hz * 100.0,
            split.ipc * 100.0,
            split.residue * 100.0,
        ));
    }
    line
}

/// The core the frequency crossing measures: one with an SMT sibling, and
/// neither cpu 0, which takes more of the machine's interrupts than any other.
fn measured_core(cores: &[Vec<usize>]) -> Option<(usize, usize)> {
    cores
        .iter()
        .filter(|core| core.len() >= 2 && !core.contains(&0))
        .map(|core| (core[0], core[1]))
        .next()
}

fn have_taskset() -> bool {
    Command::new("taskset")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The machine's load average, for a skip message that has to say whether the
/// machine was already busy.
fn load_average() -> String {
    read("/proc/loadavg")
        .and_then(|text| text.split_whitespace().next().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Every run must stand on two instruments agreeing about the clock, and on a
/// loop that was not on its core for longer than the window.
fn assert_premise(name: &str, cpu: usize, run: &Factors) {
    let slack = 0.05 + run.spread;
    assert!(
        (run.agreement() - 1.0).abs() <= slack,
        "{name}: core {cpu} was held busy for the whole window, so its cycles over wall time are \
         its clock: {:.3} GHz by the cycle counter against {:.3} GHz by `power.freq`, {:.3} \
         where the readings themselves spread by {:.1} %. Either the core idled, which a loop \
         that never blocks cannot let it do, or the two instruments disagree about the clock \
         and no decomposition built on them means anything",
        run.core_cycles / run.core_wall_s / 1e9,
        run.hz / 1e9,
        run.agreement(),
        run.spread * 100.0,
    );
    assert!(
        run.residue() <= 1.0 + slack,
        "{name}: the loop counted {:.3} Gcycles in {:.3} s at {:.3} GHz, a share of {:.3} of the \
         window -- more of the core than there was",
        run.cycles / 1e9,
        run.wall_s,
        run.hz / 1e9,
        run.residue()
    );
}

/// On a core held busy, the cycle counter and the frequency instrument agree
/// about the clock.
///
/// Two instruments that share nothing: the cycle count is the PMU's, and
/// `cpuinfo_avg_freq` is derived by the processor from aperf and mperf. Over
/// the same second on a core that never idles, cycles over wall time *is* the
/// frequency, so the two must agree -- within the frequency readings' own
/// spread, which is the reference's resolution.
///
/// This is the premise of the whole crossing. If it fails, the tool's two
/// halves disagree about the clock and no verdict built on them means anything.
#[test]
fn the_cycle_counter_and_the_frequency_agree_on_a_core_held_busy() {
    if !have_taskset() {
        eprintln!("skipped: this machine has no `taskset`, so no core can be held busy");
        return;
    }
    let cores = physical_cores();
    let Some((cpu, _)) = measured_core(&cores).or_else(|| {
        cores
            .iter()
            .flatten()
            .copied()
            .find(|&cpu| cpu != 0)
            .map(|cpu| (cpu, cpu))
    }) else {
        eprintln!("skipped: one cpu, and cpu 0 is the one every interrupt lands on");
        return;
    };
    let _alone = alone();
    let Some(spin) = Spinners::on(&[cpu]) else {
        eprintln!("skipped: `taskset` would not start, so no core can be held busy");
        return;
    };
    std::thread::sleep(Duration::from_millis(200));
    let run = match measure_frequency(cpu, spin.0[0].id()) {
        Measured::Factors(run) => run,
        Measured::Unservable(why) => {
            eprintln!("skipped: {why}");
            return;
        }
    };
    eprintln!(
        "{}",
        verdict_line(&format!("core {cpu}, held busy"), &run, None)
    );
    assert_premise("held busy", cpu, &run);
}

/// A slowdown the clock caused is put on the clock, and one it did not cause is
/// not.
///
/// The same spin loop on the same core, four times, and what changed in each
/// is known because this test changed it:
///
/// - **Paired** is the reference: the loop and a second one on its SMT sibling.
///   Holding the sibling is deliberate. On a shared machine, whatever else is
///   running goes to the idle hardware threads, and an idle sibling is one of
///   them -- a first version of this test lost fifteen percent of the loop's
///   IPC to a stranger on the sibling and read it as a frequency effect.
/// - **Every other core loaded** adds a loop on both threads of every other
///   physical core but one, which is left idle for the rest of the machine.
///   That takes the package's boost budget and leaves this core's pipeline
///   exactly as it was, so the loop retires the same instructions per cycle at
///   a lower clock: the frequency factor must move and the per-cycle factor
///   must not.
/// - **Sibling released** stops the sibling's loop. The clock barely moves and
///   the core's issue width goes back to one thread: the per-cycle factor must
///   carry the speed-up and the frequency factor must *not*.
///
/// The residue -- the share of the window the loop had its core -- is printed
/// for every run and asserted only where it is physical: a loop cannot have had
/// more of its core than the window. On a busy machine it is below one, and
/// that is a fourth state the decomposition names rather than hides.
///
/// The paired run is taken first and last, and the difference between the two
/// is the run-to-run movement of each factor: the slack every comparison
/// carries. A machine whose clock does not move under package load -- a
/// fixed-frequency part, boost switched off, a virtual machine, or a machine
/// already so loaded that the reference ran at the all-core clock -- has no
/// frequency confounder to separate, and that half is skipped with the numbers
/// that show it.
#[test]
fn a_slowdown_from_the_clock_is_put_on_the_clock_and_one_from_the_sibling_is_not() {
    if !have_taskset() {
        eprintln!("skipped: this machine has no `taskset`, so no core can be loaded on purpose");
        return;
    }
    let cores = physical_cores();
    let Some((cpu, sibling)) = measured_core(&cores) else {
        eprintln!(
            "skipped: no physical core here has two hardware threads ({} cores, largest {} \
             thread(s)), so the SMT half of this crossing cannot be created",
            cores.len(),
            cores.iter().map(Vec::len).max().unwrap_or(0)
        );
        return;
    };
    let other_cores: Vec<&Vec<usize>> = cores.iter().filter(|core| !core.contains(&cpu)).collect();
    if other_cores.len() < 3 {
        eprintln!(
            "skipped: {} other physical core(s), too few to draw on the package's boost budget",
            other_cores.len()
        );
        return;
    }
    // Every hardware thread of every other core but the last, which is left
    // idle for whatever else this machine is running: with nowhere idle to go,
    // it would time-share the measured core.
    let (spare, loaded_cores) = other_cores.split_last().expect("three or more");
    let others: Vec<usize> = loaded_cores
        .iter()
        .flat_map(|core| core.iter().copied())
        .collect();

    let _alone = alone();
    let measure = |pinned: &[usize]| -> Result<Factors, String> {
        let spin = Spinners::on(pinned).ok_or_else(|| "`taskset` would not start".to_string())?;
        // The clock follows load within a few milliseconds; this is the settling
        // the power scenarios use, and a little more.
        std::thread::sleep(Duration::from_millis(300));
        match measure_frequency(cpu, spin.0[0].id()) {
            Measured::Factors(run) => Ok(run),
            Measured::Unservable(why) => Err(why),
        }
    };
    let paired = [cpu, sibling];
    let mut everything = paired.to_vec();
    everything.extend_from_slice(&others);
    let runs = (|| -> Result<_, String> {
        Ok((
            measure(&paired)?,
            measure(&everything)?,
            measure(&[cpu])?,
            measure(&paired)?,
        ))
    })();
    let (first, loaded, released, last) = match runs {
        Ok(runs) => runs,
        Err(why) => {
            eprintln!("skipped: {why}");
            return;
        }
    };

    // The reference is the mean of the two paired runs; their difference is
    // the run-to-run movement of each factor, and every comparison carries it.
    let reference = Factors {
        instructions: (first.instructions + last.instructions) / 2.0,
        cycles: (first.cycles + last.cycles) / 2.0,
        wall_s: (first.wall_s + last.wall_s) / 2.0,
        hz: (first.hz + last.hz) / 2.0,
        spread: first.spread.max(last.spread),
        core_cycles: (first.core_cycles + last.core_cycles) / 2.0,
        core_wall_s: (first.core_wall_s + last.core_wall_s) / 2.0,
    };
    let drift = Split::between(&first, &last);
    let noise_hz = drift.hz.abs() + 0.01;
    let noise_ipc = drift.ipc.abs() + 0.01;
    let to_loaded = Split::between(&reference, &loaded);
    let to_released = Split::between(&reference, &released);

    eprintln!(
        "the clock under an instruction rate, core {cpu} (sibling {sibling}), load average {}:",
        load_average()
    );
    eprintln!("{}", verdict_line("paired with its sibling", &first, None));
    eprintln!(
        "{}",
        verdict_line(
            &format!("and {} other cores", loaded_cores.len()),
            &loaded,
            Some(&to_loaded)
        )
    );
    eprintln!(
        "{}",
        verdict_line("sibling released", &released, Some(&to_released))
    );
    eprintln!("{}", verdict_line("paired, again", &last, Some(&drift)));

    for (name, run) in [
        ("paired", &first),
        ("every other core loaded", &loaded),
        ("sibling released", &released),
        ("paired, again", &last),
    ] {
        assert_premise(name, cpu, run);
    }

    // The sibling half. Created by this test on every machine that has SMT.
    let smt_effect = to_released.ipc.abs();
    if smt_effect < 3.0 * noise_ipc + 0.05 {
        eprintln!(
            "skipped, SMT half: releasing sibling {sibling} moved the loop's IPC by {:+.1} %, \
             within three times the {:.1} % two identical runs moved it, with the machine's \
             load average at {}. Either this core does not share its pipeline with that thread, \
             or something else took the sibling the moment it was released; either way there \
             was no change in contention to attribute",
            to_released.ipc * 100.0,
            noise_ipc * 100.0,
            load_average()
        );
    } else {
        assert!(
            to_released.hz.abs() <= smt_effect / 2.0 + noise_hz,
            "releasing the sibling moved the loop's IPC by {:+.1} % and the clock by {:+.1} %: \
             a change that happened at the same clock has been put on the clock",
            to_released.ipc * 100.0,
            to_released.hz * 100.0
        );
    }

    // The frequency half. The load is created here; whether the clock answers
    // it is the machine's, and a machine whose clock did not move is reported
    // rather than failed.
    let clock_effect = -to_loaded.hz;
    if clock_effect < 2.0 * noise_hz + 0.02 {
        eprintln!(
            "skipped, frequency half: {} other cores spinning (core {spare:?} left idle) moved \
             core {cpu}'s clock by {:+.1} % ({:.3} -> {:.3} GHz), within twice the {:.1} % two \
             identical runs moved it, with the machine's load average at {}. Either this \
             machine's clock does not answer package load, or the machine was already loaded \
             enough that the reference ran near the all-core clock; either way there is no \
             frequency confounder here to separate",
            loaded_cores.len(),
            to_loaded.hz * 100.0,
            reference.hz / 1e9,
            loaded.hz / 1e9,
            noise_hz * 100.0,
            load_average()
        );
        return;
    }
    assert!(
        to_loaded.ipc.abs() <= clock_effect / 2.0 + noise_ipc,
        "loading the other cores lowered core {cpu}'s clock by {:.1} % and the loop's own IPC \
         moved by {:+.1} %: its pipeline was left alone by construction, so a per-cycle change \
         this large is something the clock should have explained",
        clock_effect * 100.0,
        to_loaded.ipc * 100.0
    );
}

/// A guardrail this machine cannot serve refuses the whole crossing.
///
/// `power.throttle` is the source whose job is to invalidate a run. Beside an
/// instruction count on a machine where no backend can see throttling, the
/// session must not run the count and quietly drop the guardrail: a result
/// with instructions and no throttle series reads exactly like a run that did
/// not throttle, which is the most expensive thing the tool can say wrongly.
/// So the session is refused -- as unservable or unbuilt, not as meaningless,
/// because the request is valid -- and prints no results at all.
///
/// On a machine that can see throttling the same session must carry both.
#[test]
fn a_guardrail_this_machine_cannot_serve_refuses_the_whole_crossing() {
    let session = "\
name: a count beside its guardrail
duration: 300ms
requests:
  - label: instructions
    source: cpu.instructions
    target: system
    mode: count
    emit: [rate]
  - label: throttle
    source: power.throttle
    target: system
    mode: poll@100ms
    emit: [timeline]
";
    let output = run_session(session, &[]);
    if output.status.success() {
        let document = document(&output);
        let throttle = result(&document, "throttle");
        assert!(
            output_of(throttle, "timeline")["series"]
                .as_array()
                .is_some_and(|series| !series.is_empty()),
            "the guardrail ran and produced no series, which reads as a run that did not \
             throttle"
        );
        eprintln!("this machine serves `power.throttle`, and the crossing carried both halves");
        return;
    }
    let code = output.status.code();
    assert!(
        matches!(code, Some(1) | Some(3)),
        "a valid request this machine cannot serve is a machine's answer (1) or a build's (3), \
         never a refusal of the request itself; got {code:?}: {}",
        stderr(&output)
    );
    assert!(
        output.stdout.is_empty(),
        "the session was refused and still printed results -- the instruction count without \
         the guardrail beside it:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let message = stderr(&output);
    if !message.contains("power.throttle") {
        // The refusal named something else first. That is the property only
        // if the count itself is served here: at `perf_event_paranoid` 4 (the
        // CI's first run) `cpu.instructions` is refused before the guardrail
        // is reached, and the crossing this test is about -- a count served
        // beside a guardrail that is not -- cannot be created. Asked alone, so
        // the machine says which.
        let count_alone = run_session(
            "\
name: the count alone
duration: 100ms
requests:
  - {label: instructions, source: cpu.instructions, target: system, mode: count, emit: [rate]}
",
            &["--dump-resolved"],
        );
        assert!(
            !count_alone.status.success(),
            "the count is served here, and the refusal of the crossing did not name the \
             guardrail it could not serve: {message}"
        );
        // Only the machine's refusal of the count is a reason to skip: exit 1,
        // naming the count, and saying it has no backend here or that
        // `perf_event_paranoid` withholds it. Any other failure of the probe
        // is a failure.
        let alone = stderr(&count_alone);
        assert!(
            count_alone.status.code() == Some(1)
                && alone.contains("cpu.instructions")
                && (alone.contains("no viable backend on this machine")
                    || alone.contains("perf_event_paranoid")),
            "the crossing's refusal did not name the guardrail ({message}), and the count alone \
             failed for a reason that is not the machine's refusal of it ({:?}): {alone}",
            count_alone.status.code()
        );
        eprintln!(
            "skipped: this machine refuses the count itself, so a count served beside a \
             guardrail it cannot serve is not a crossing it can create. The machine's reason: {}",
            stderr(&count_alone)
                .lines()
                .take(3)
                .collect::<Vec<_>>()
                .join(" / ")
        );
        return;
    }
    eprintln!(
        "refused whole, as it must be: {}",
        message.lines().next().unwrap_or("").trim()
    );
}

// ---------------------------------------------------------------------------
// The I/O a memory limit made
// ---------------------------------------------------------------------------

/// The program: write 96 MiB, pause, read it back, and say what the kernel
/// charged it for.
///
/// Fresh random bytes for every mebibyte, so no filesystem can compress the
/// file into less I/O than the test believes it wrote. The pause is for the
/// join: `mem.reclaim` emits a window's reclaim when the window closes, and a
/// read issued before any window of the write phase has closed has no open
/// state to be attributed to. `/proc/self/io`'s `read_bytes` is the kernel's
/// count of the bytes this process caused to be fetched from storage, which is
/// the reference for the IDENTITY rung.
const CHURN: &str = r#"
import os, sys, time
path, mib = sys.argv[1], int(sys.argv[2])
t0 = time.monotonic_ns()
with open(path, "wb") as f:
    for _ in range(mib):
        f.write(os.urandom(1 << 20))
time.sleep(0.3)
t1 = time.monotonic_ns()
with open(path, "rb") as f:
    while f.read(1 << 20):
        pass
t2 = time.monotonic_ns()
io = dict(line.split(": ") for line in open("/proc/self/io").read().splitlines())
os.unlink(path)
print("omn-cross pid", os.getpid(), "write", t0, "read", t1, t2,
      "read_bytes", io["read_bytes"].strip(), file=sys.stderr)
"#;

const WRITTEN_MIB: u64 = 96;
const LIMIT_MIB: u64 = 32;

/// A cgroup of this test's making, removed when dropped.
struct Cgroup {
    dir: PathBuf,
}

impl Cgroup {
    /// Under the nearest ancestor of this process's own cgroup that delegates
    /// the memory controller: a cgroup whose parent does not cannot be given a
    /// limit.
    fn create() -> Result<Self, String> {
        let own = read("/proc/self/cgroup")
            .and_then(|text| {
                text.lines()
                    .find_map(|line| line.strip_prefix("0::").map(str::to_owned))
            })
            .ok_or("this process is not in a cgroup2 hierarchy")?;
        let root = Path::new("/sys/fs/cgroup");
        let mut dir = root.join(own.trim_start_matches('/'));
        while dir != root {
            let delegated = read(dir.join("cgroup.subtree_control"))
                .is_some_and(|controllers| controllers.split_whitespace().any(|c| c == "memory"));
            if delegated {
                let ours = dir.join(format!("omn-cross-machine-{}.scope", std::process::id()));
                if std::fs::create_dir(&ours).is_ok() {
                    return Ok(Self { dir: ours });
                }
            }
            if !dir.pop() {
                break;
            }
        }
        Err(format!(
            "no ancestor of {own} delegates the memory controller to this user"
        ))
    }

    fn limit(&self, value: &str) {
        std::fs::write(self.dir.join("memory.max"), value)
            .unwrap_or_else(|error| panic!("setting memory.max to {value}: {error}"));
    }

    /// The id the kernel gives a BPF program for this cgroup, which is the id
    /// on `mem.reclaim`'s events.
    fn id(&self) -> u64 {
        std::fs::metadata(&self.dir)
            .expect("the cgroup exists")
            .ino()
    }

    fn stat(&self, name: &str) -> u64 {
        read(self.dir.join("memory.stat"))
            .and_then(|text| {
                text.lines().find_map(|line| {
                    line.strip_prefix(name)
                        .and_then(|rest| rest.strip_prefix(' '))
                        .and_then(|value| value.trim().parse().ok())
                })
            })
            .unwrap_or(0)
    }

    /// The cgroup's own memory stall, `some`, in microseconds.
    fn stalled_us(&self) -> u64 {
        read(self.dir.join("memory.pressure"))
            .and_then(|text| {
                text.lines()
                    .find(|line| line.starts_with("some"))
                    .and_then(|line| line.split("total=").nth(1))
                    .and_then(|total| total.trim().parse().ok())
            })
            .unwrap_or(0)
    }
}

impl Drop for Cgroup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// Where the file goes, and the block devices that hold it as the tool names
/// them: the partition, which `io.latency` reports, and the whole disk, which
/// is all `io.bytes` reports because a partition's bytes are counted again on
/// the disk.
struct Scratch {
    dir: PathBuf,
    partition: Option<String>,
    disk: Option<String>,
}

/// A disk-backed directory: a tmpfs file is memory, charged to the same cgroup,
/// and unreclaimable without swap, which turns a reclaim scenario into an
/// out-of-memory kill.
fn scratch() -> Result<Scratch, String> {
    let table = read("/proc/self/mountinfo").ok_or("/proc/self/mountinfo is unreadable")?;
    let mut candidates: Vec<PathBuf> = vec![PathBuf::from("/var/tmp")];
    candidates.extend(std::env::var_os("TMPDIR").map(PathBuf::from));
    candidates.push(PathBuf::from(env!("CARGO_TARGET_TMPDIR")));
    let mut seen = Vec::new();
    for candidate in candidates {
        let Ok(dir) = candidate.canonicalize() else {
            continue;
        };
        // The mount a path is on is the longest mount point that prefixes it.
        let mount = table
            .lines()
            .filter_map(|line| {
                let fields: Vec<&str> = line.split(' ').collect();
                let point = *fields.get(4)?;
                let tail = line.split(" - ").nth(1)?;
                let mut tail = tail.split(' ');
                Some((point, tail.next()?, tail.next()?))
            })
            .filter(|(point, _, _)| dir.starts_with(point))
            .max_by_key(|(point, _, _)| point.len());
        let Some((_, fstype, source)) = mount else {
            continue;
        };
        if matches!(fstype, "tmpfs" | "ramfs") {
            seen.push(format!("{} is {fstype}", dir.display()));
            continue;
        }
        let partition = Path::new(source)
            .canonicalize()
            .ok()
            .filter(|device| device.starts_with("/dev"))
            .and_then(|device| device.file_name()?.to_str().map(str::to_owned));
        let disk = partition.as_ref().and_then(|name| {
            let class = Path::new("/sys/class/block").join(name);
            if !class.exists() {
                return None;
            }
            if class.join("partition").exists() {
                let parent = class.canonicalize().ok()?.parent()?.to_path_buf();
                parent.file_name()?.to_str().map(str::to_owned)
            } else {
                Some(name.clone())
            }
        });
        return Ok(Scratch {
            dir,
            partition,
            disk,
        });
    }
    Err(format!(
        "no disk-backed directory to write through ({})",
        seen.join(", ")
    ))
}

/// What the program said about itself.
#[derive(Debug, Default)]
struct Churned {
    pid: u64,
    write_ns: u64,
    read_ns: u64,
    end_ns: u64,
    read_bytes: u64,
}

fn churn_in(cgroup: &Cgroup, file: &Path) -> Result<Churned, String> {
    let output = Command::new("sh")
        .args([
            "-c",
            r#"echo $$ > "$1/cgroup.procs" && exec python3 -c "$2" "$3" "$4""#,
            "sh",
        ])
        .arg(&cgroup.dir)
        .arg(CHURN)
        .arg(file)
        .arg(WRITTEN_MIB.to_string())
        .output()
        .map_err(|error| format!("sh would not start: {error}"))?;
    let _ = std::fs::remove_file(file);
    let text = stderr(&output);
    let line = text
        .lines()
        .find(|line| line.starts_with("omn-cross "))
        .ok_or_else(|| {
            // What it did say, or that it said nothing, with how it ended: a
            // `cgroup.procs` the shell could not write, a python3 that is
            // missing, and a program that died on a signal all print nothing
            // here and differ in the status.
            let out = String::from_utf8_lossy(&output.stdout);
            let said = match (text.trim(), out.trim()) {
                ("", "") => "it printed nothing".to_owned(),
                (err, "") => format!("it printed on stderr: {err}"),
                ("", out) => format!("it printed on stdout only: {out}"),
                (err, out) => format!("it printed on stderr: {err}; on stdout: {out}"),
            };
            format!(
                "the program did not report itself: {said}, and exited {} (moved into {})",
                output.status,
                cgroup.dir.display()
            )
        })?;
    let fields: Vec<&str> = line.split_whitespace().collect();
    let number = |at: usize| -> u64 { fields.get(at).and_then(|f| f.parse().ok()).unwrap_or(0) };
    Ok(Churned {
        pid: number(2),
        write_ns: number(4),
        read_ns: number(6),
        end_ns: number(7),
        read_bytes: number(9),
    })
}

/// One run of the reclaim session with the program beside it.
struct ReclaimRun {
    document: Value,
    churned: Churned,
    /// Pages the cgroup reclaimed, by its own `memory.stat`.
    pgsteal: u64,
    /// The cgroup's own memory stall over the run, in nanoseconds.
    stalled_ns: u64,
}

/// The programs are loaded and attached in `Collector::start`, and a request
/// completed before that happened is one the kernel never told anyone about.
/// Every rung checked below is a lower bound, so starting the program early
/// fails the assertion rather than weakening it.
const ATTACH_MARGIN: Duration = Duration::from_millis(700);

/// The session's two joins: the reads of a reclaiming task (KEY), and the
/// reclaim laid across the machine's pressure intervals with no key (TEMPORAL).
const KEY_JOIN: &str = "reads-of-a-reclaiming-task";
const OVERLAP_JOIN: &str = "reclaim-per-pressure-interval";

fn run_reclaim(cgroup: &Cgroup, scratch: &Scratch, limit: &str) -> Result<ReclaimRun, String> {
    cgroup.limit(limit);
    let session = std::fs::read_to_string(example("cross-machine-reclaim-io.yml"))
        .expect("the reclaim session ships");
    let stolen_before = cgroup.stat("pgsteal_direct") + cgroup.stat("pgsteal_kswapd");
    let stalled_before = cgroup.stalled_us();
    let child = spawn_session(&session, &[]);
    std::thread::sleep(ATTACH_MARGIN);
    let file = scratch
        .dir
        .join(format!("omn-cross-machine-{}.churn", std::process::id()));
    let churned = churn_in(cgroup, &file);
    let output = child.wait_with_output().expect("omn should finish");
    let churned = churned?;
    if !output.status.success() {
        let message = stderr(&output);
        return Err(
            if message.contains("CAP_BPF") || message.contains("CAP_PERFMON") {
                format!(
                    "`mem.reclaim` and `io.latency` need CAP_BPF and CAP_PERFMON, which `make` \
                 grants and every `cargo build` drops: {}",
                    message.lines().next().unwrap_or("").trim()
                )
            } else {
                format!(
                    "the reclaim session did not run: {}",
                    message.lines().next().unwrap_or("").trim()
                )
            },
        );
    }
    Ok(ReclaimRun {
        document: document(&output),
        churned,
        pgsteal: cgroup.stat("pgsteal_direct") + cgroup.stat("pgsteal_kswapd") - stolen_before,
        stalled_ns: (cgroup.stalled_us() - stalled_before) * 1000,
    })
}

/// What one run's events say, read the way the crossing reads them.
#[derive(Debug, Default)]
struct Ladder {
    /// IDENTITY: reads the block layer says the program submitted.
    identity_bytes: u64,
    identity_requests: u64,
    /// The program's reads issued after it started reading back, which is
    /// after a window of its reclaim had closed: the ones the join can match.
    identity_reading_back: u64,
    /// KEY: of the program's reads, how many the join tied to a reclaiming
    /// task, and whether that task was the program.
    key_matched: u64,
    /// Reclaim the tool saw in this cgroup, by the cgroup key on the event.
    reclaimed_pages: u64,
    reclaim_episodes: u64,
    /// TEMPORAL: writes to the file's device while the program ran, by whom.
    writes_meanwhile: u64,
    writes_by_program: u64,
    writers: BTreeMap<String, u64>,
    /// What `io.bytes` saw the disk move, over the whole window.
    device_read: u64,
    device_write: u64,
    /// The machine's memory stall over the window, from the tool.
    stalled_ns: u64,
    /// Whether the device poller's ticks bracket the program.
    contained: bool,
    /// Every page the tool saw reclaimed, whichever cgroup paid.
    reclaimed_machine_wide: u64,
    /// The keyless overlap: pages laid across the machine's pressure
    /// intervals, those in intervals the program's brackets overlap, and those
    /// it left uncredited.
    overlap_credited: f64,
    overlap_in_brackets: f64,
    overlap_uncredited: f64,
}

fn ladder(run: &ReclaimRun, scratch: &Scratch, cgroup_id: u64) -> Ladder {
    let document = &run.document;
    let churned = &run.churned;
    let mut ladder = Ladder::default();

    for event in raw_events(result(document, "requests")) {
        let bytes = attr_u64(event, "bytes");
        let rwbs = attr_str(event, "rwbs");
        let tid = attr_u64(event, "tid");
        let ts = event["ts_ns"].as_u64().unwrap_or(0);
        if tid == churned.pid && rwbs.starts_with('R') {
            ladder.identity_bytes += bytes;
            ladder.identity_requests += 1;
            if ts >= churned.read_ns {
                ladder.identity_reading_back += 1;
            }
        }
        let on_the_file = [&scratch.partition, &scratch.disk]
            .iter()
            .any(|name| name.as_deref() == Some(attr_str(event, "dev")));
        if on_the_file && rwbs.starts_with('W') && ts >= churned.write_ns && ts <= churned.end_ns {
            ladder.writes_meanwhile += bytes;
            if tid == churned.pid {
                ladder.writes_by_program += bytes;
            }
            *ladder
                .writers
                .entry(attr_str(event, "comm").to_owned())
                .or_default() += bytes;
        }
    }

    let own_key = format!("task:{}", churned.pid);
    let joins = document["joins"].as_array().into_iter().flatten();
    for join in joins.filter(|join| join["rule"] == KEY_JOIN) {
        assert_eq!(
            join["evidence"], "KEY",
            "an equal task on both sides is KEY evidence, and the join must say so"
        );
        for attribution in join["top"].as_array().into_iter().flatten() {
            if attribution["key"] == own_key.as_str() {
                ladder.key_matched += attribution["matched"].as_u64().unwrap_or(0);
            }
        }
    }

    for event in raw_events(result(document, "reclaim")) {
        if attr_u64(event, "cgroup_id") == cgroup_id {
            ladder.reclaimed_pages += attr_u64(event, "pages_reclaimed");
            ladder.reclaim_episodes += attr_u64(event, "count");
        }
    }

    let ticks = raw_events(result(document, "device"));
    let mut first = u64::MAX;
    let mut last = 0;
    for event in &ticks {
        let ts = event["ts_ns"].as_u64().unwrap_or(0);
        first = first.min(ts);
        last = last.max(ts);
        if scratch.disk.as_deref() == Some(attr_str(event, "dev")) {
            ladder.device_read += attr_u64(event, "read_bytes");
            ladder.device_write += attr_u64(event, "write_bytes");
        }
    }
    // A poll reports what happened since the tick before, so the tick after
    // the program ended is the one that carries its last bytes.
    ladder.contained = first <= churned.write_ns && last >= churned.end_ns + 100_000_000;

    ladder.stalled_ns = raw_events(result(document, "pressure"))
        .iter()
        .map(|event| attr_u64(event, "stalled_ns"))
        .sum();

    // The keyless overlap: each interval of the machine's pressure and the
    // reclaim laid across it.
    ladder.reclaimed_machine_wide = raw_events(result(document, "reclaim"))
        .iter()
        .map(|event| attr_u64(event, "pages_reclaimed"))
        .sum();
    let overlap = document["joins"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|join| join["rule"] == OVERLAP_JOIN)
        .unwrap_or_else(|| panic!("the session's `{OVERLAP_JOIN}` join reported nothing"));
    assert_eq!(overlap["evidence"], "TEMPORAL", "{overlap}");
    assert_eq!(
        overlap["keyless"].as_u64().unwrap_or(0),
        0,
        "with no key, no event can lack it: {overlap}"
    );
    for row in overlap["top"].as_array().into_iter().flatten() {
        assert_eq!(
            row["key"],
            omn::join::MACHINE,
            "one series, the machine's: {row}"
        );
        let opened = row["opened_ns"].as_u64().unwrap_or(0);
        let closed = row["closed_ns"].as_u64().unwrap_or(0);
        let pages = row["change"]["delta"].as_f64().unwrap_or(0.0);
        ladder.overlap_credited += pages;
        // A poll reports what happened since the tick before: the interval
        // (opened, closed] holds the program if it overlaps its brackets.
        if closed > churned.write_ns && opened < churned.end_ns {
            ladder.overlap_in_brackets += pages;
        }
    }
    ladder.overlap_uncredited = overlap["readings"]["uncredited"].as_f64().unwrap_or(0.0);
    ladder
}

fn print_ladder(title: &str, run: &ReclaimRun, ladder: &Ladder, scratch: &Scratch) {
    let disk = scratch.disk.as_deref().unwrap_or("its disk");
    let other = (ladder.device_read + ladder.device_write)
        .saturating_sub(ladder.identity_bytes + ladder.writes_meanwhile);
    eprintln!("{title}");
    eprintln!(
        "  program      wrote {WRITTEN_MIB} MiB, read it back; the kernel charged it {:.1} MiB \
         of storage reads",
        mib(run.churned.read_bytes as f64)
    );
    eprintln!(
        "  cgroup       reclaimed {} pages by its own memory.stat, stalled {:.1} ms",
        run.pgsteal,
        run.stalled_ns as f64 / 1e6
    );
    eprintln!(
        "  IDENTITY     {:>7.1} MiB in {} reads submitted by the program itself",
        mib(ladder.identity_bytes as f64),
        ladder.identity_requests
    );
    eprintln!(
        "  KEY          {:>7} of those tied by the join to a task that was reclaiming; \
         {} pages in {} episodes reclaimed in this cgroup",
        ladder.key_matched, ladder.reclaimed_pages, ladder.reclaim_episodes
    );
    eprintln!(
        "  TEMPORAL     {:>7.1} MiB written to the file's device while the program ran, {:.1} MiB \
         of it submitted by the program; by {:?}",
        mib(ladder.writes_meanwhile as f64),
        mib(ladder.writes_by_program as f64),
        ladder
            .writers
            .iter()
            .map(|(comm, bytes)| format!("{comm} {:.1} MiB", mib(*bytes as f64)))
            .collect::<Vec<_>>()
    );
    eprintln!(
        "  device       {disk}: {:.1} MiB read, {:.1} MiB written over the window; the \
         machine's memory stall {:.1} ms",
        mib(ladder.device_read as f64),
        mib(ladder.device_write as f64),
        ladder.stalled_ns as f64 / 1e6
    );
    eprintln!(
        "  no key       {:.0} pages laid across the machine's pressure intervals ({:.0} in those \
         the program overlapped), {:.0} uncredited (buckets of several episodes), of {} the \
         tool saw reclaimed",
        ladder.overlap_credited,
        ladder.overlap_in_brackets,
        ladder.overlap_uncredited,
        ladder.reclaimed_machine_wide
    );
    eprintln!(
        "  residue      {:>7.1} MiB of the device's bytes are on no rung above: other programs, \
         metadata, or I/O outside the program's window",
        mib(other as f64)
    );
}

/// A memory limit turns a cached read into device reads, and the crossing
/// says which rung of evidence ties each byte to it.
///
/// One program, run twice. It writes 96 MiB and reads it straight back. In a
/// cgroup capped at 32 MiB the pages it wrote are evicted before it reads them,
/// so the read goes to the disk; uncapped, the read never leaves the page
/// cache. What the program asked for is identical, and the difference in what
/// the device did is the memory limit's.
///
/// The references are the kernel's, read by this test: the program's own
/// `/proc/self/io` and the cgroup's own `memory.stat` and `memory.pressure`.
/// Against them:
///
/// - **IDENTITY**: the reads `io.latency` attributes to the program's tid are
///   the bytes the kernel charged it for fetching, within ten percent.
/// - **KEY**: the join ties those reads to a task that was reclaiming, and
///   that task is the program -- found under the program's own key.
/// - The reclaim `mem.reclaim` saw in this cgroup, by the cgroup key on its
///   events, is what the cgroup's `memory.stat` counted.
/// - **TEMPORAL**: to stay within 32 MiB the cgroup had to write back at least
///   64 of the 96 dirty mebibytes before it could drop them, so at least that
///   much reached the file's device while the program ran -- and the block
///   layer does not say it was the program's.
///
/// Uncapped, the same program reclaims nothing and reads nothing from the
/// device, and the crossing ties none of the device's bytes to it: whatever
/// else the disk did in that window is somebody else's, and says so.
#[test]
fn a_memory_limit_turns_a_cached_read_into_device_reads_and_each_byte_carries_its_rung() {
    if Command::new("python3")
        .arg("--version")
        .output()
        .map_or(true, |output| !output.status.success())
    {
        eprintln!("skipped: no `python3`, which is the program this crossing runs");
        return;
    }
    let cgroup = match Cgroup::create() {
        Ok(cgroup) => cgroup,
        Err(why) => {
            eprintln!("skipped: the limit cannot be created here: {why}");
            return;
        }
    };
    let scratch = match scratch() {
        Ok(scratch) => scratch,
        Err(why) => {
            eprintln!("skipped: {why}");
            return;
        }
    };
    let _alone = alone();

    let capped = match run_reclaim(&cgroup, &scratch, &(LIMIT_MIB << 20).to_string()) {
        Ok(run) => run,
        Err(why) => {
            eprintln!("skipped: {why}");
            return;
        }
    };
    let uncapped = match run_reclaim(&cgroup, &scratch, "max") {
        Ok(run) => run,
        Err(why) => {
            eprintln!("skipped: {why}");
            return;
        }
    };
    let id = cgroup.id();
    let under = ladder(&capped, &scratch, id);
    let free = ladder(&uncapped, &scratch, id);
    print_ladder(
        &format!("the I/O a memory limit made -- capped at {LIMIT_MIB} MiB:"),
        &capped,
        &under,
        &scratch,
    );
    print_ladder("the same program, uncapped:", &uncapped, &free, &scratch);

    // The conditions, checked against the kernel's own counters before any of
    // the tool's numbers are looked at.
    let evicted_at_least = (WRITTEN_MIB - LIMIT_MIB) << 20;
    if capped.churned.read_bytes < evicted_at_least / 2 {
        eprintln!(
            "skipped: capped at {LIMIT_MIB} MiB, the program was charged only {:.1} MiB of \
             storage reads for re-reading {WRITTEN_MIB} MiB, so the limit did not evict what it \
             wrote and there is no memory-made I/O to attribute",
            mib(capped.churned.read_bytes as f64)
        );
        return;
    }
    if uncapped.churned.read_bytes > 4 << 20 {
        eprintln!(
            "skipped: uncapped, the program was still charged {:.1} MiB of storage reads, so this \
             machine is short of memory without any limit and there is no clean run to set the \
             capped one against",
            mib(uncapped.churned.read_bytes as f64)
        );
        return;
    }
    for (name, run, ladder) in [("capped", &capped, &under), ("uncapped", &uncapped, &free)] {
        assert!(
            ladder.contained,
            "{name}: the device poller's ticks do not bracket the program ({} to {} ns), so the \
             window did not contain what it is being checked against",
            run.churned.write_ns, run.churned.end_ns
        );
    }

    // IDENTITY, against the kernel's per-process count.
    let charged = capped.churned.read_bytes;
    assert!(
        under.identity_bytes >= charged - charged / 10
            && under.identity_bytes <= charged + charged / 10 + (1 << 20),
        "the kernel charged the program {:.1} MiB of storage reads and the block layer's \
         submitter was the program for {:.1} MiB of them; a read carries its submitter, so the \
         two must agree",
        mib(charged as f64),
        mib(under.identity_bytes as f64)
    );

    // KEY: the program's reads tied to a reclaiming task, under its own tid.
    assert!(
        under.key_matched >= under.identity_reading_back - under.identity_reading_back / 10,
        "the program reclaimed and read, and the join tied {} of the {} reads it issued while \
         reading back to a reclaiming task under its own key. The program sleeps 300 ms between \
         writing and reading so a window of reclaim has closed before the first read",
        under.key_matched,
        under.identity_reading_back
    );

    // The reclaim, scoped by the cgroup key, against the cgroup's own count.
    assert!(
        under.reclaimed_pages >= capped.pgsteal - capped.pgsteal / 10,
        "the cgroup's memory.stat counted {} pages reclaimed and the events carrying its id \
         account for {}",
        capped.pgsteal,
        under.reclaimed_pages
    );
    assert!(
        under.reclaimed_pages <= capped.pgsteal + capped.pgsteal / 20 + 1024,
        "the events carrying the cgroup's id account for {} pages reclaimed and its memory.stat \
         for {}: more than the kernel counted is reclaim from somewhere else under this key",
        under.reclaimed_pages,
        capped.pgsteal
    );

    // The keyless overlap, against the cgroup's own count. The machine's
    // pressure is one series with no task on it, which no keyed rule can read
    // (`MissingKey`, unit-tested). Every page the cgroup's memory.stat says it
    // reclaimed was reclaimed while the program ran, so each is either laid
    // across an interval the program's own brackets overlap or counted
    // uncredited -- a drained bucket of several episodes has no one window to
    // place, and the engine says so rather than spreading it -- and nothing is
    // accounted that the tool did not see reclaimed somewhere.
    assert!(
        under.overlap_in_brackets + under.overlap_uncredited
            >= (capped.pgsteal - capped.pgsteal / 10) as f64,
        "the cgroup's memory.stat counted {} pages reclaimed while the program ran, and the \
         overlap laid {:.0} across the intervals the program overlapped and left {:.0} \
         uncredited",
        capped.pgsteal,
        under.overlap_in_brackets,
        under.overlap_uncredited
    );
    let accounted = under.overlap_credited + under.overlap_uncredited;
    assert!(
        (accounted - under.reclaimed_machine_wide as f64).abs() <= 1.0,
        "the overlap accounts for {accounted:.0} pages ({:.0} credited, {:.0} uncredited) and \
         the tool saw {} reclaimed: a page is credited or counted, never both or neither",
        under.overlap_credited,
        under.overlap_uncredited,
        under.reclaimed_machine_wide
    );
    assert!(
        (under.overlap_credited - under.overlap_in_brackets).abs() <= 1.0
            || under.reclaimed_machine_wide > capped.pgsteal + capped.pgsteal / 20 + 1024,
        "every page the tool saw reclaimed is the cgroup's, and {:.0} of the {:.0} credited \
         lie in intervals the program did not overlap",
        under.overlap_credited - under.overlap_in_brackets,
        under.overlap_credited
    );

    // The machine's PSI is not the cgroup's. The root's `some` is an average
    // over every cpu weighted by how busy each was, not a sum over groups, so a
    // stall confined to the one cpu the program ran on is diluted by every
    // other busy cpu: measured here, 12.5 ms in the cgroup read as 6.0 ms at the
    // root. What does hold is that a stall anywhere is a stall at the root.
    if capped.stalled_ns >= 1_000_000 {
        assert!(
            under.stalled_ns > 0,
            "the cgroup stalled {:.1} ms on memory by its own memory.pressure and the machine's \
             PSI, which contains every stall on the machine, shows none",
            capped.stalled_ns as f64 / 1e6
        );
    }

    // TEMPORAL: the writeback the limit forced into the window.
    if scratch.partition.is_some() {
        assert!(
            under.writes_meanwhile >= evicted_at_least - evicted_at_least / 10,
            "to stay within {LIMIT_MIB} MiB the cgroup had to write back at least {} MiB of the \
             {WRITTEN_MIB} it dirtied before it could drop them, and {:.1} MiB reached the \
             file's device while the program ran",
            WRITTEN_MIB - LIMIT_MIB,
            mib(under.writes_meanwhile as f64)
        );
    }

    // The device, as a lower bound: it moved at least the program's reads.
    match scratch.disk {
        Some(_) => assert!(
            under.device_read >= charged - charged / 20,
            "the program alone was charged {:.1} MiB of storage reads and `io.bytes` saw the \
             disk read {:.1} MiB over a window that contained it",
            mib(charged as f64),
            mib(under.device_read as f64)
        ),
        None => eprintln!(
            "not checked: {} is on a device `io.bytes` does not report by itself (stacked, or \
             not a block device), so the disk's own count cannot be read against the program's",
            scratch.dir.display()
        ),
    }

    // Uncapped: nothing ties the device to the program.
    assert_eq!(
        free.reclaimed_pages, 0,
        "uncapped, the cgroup reclaimed nothing by its own count ({} pages), and the tool found \
         reclaim under its id",
        uncapped.pgsteal
    );
    assert_eq!(
        free.key_matched, 0,
        "uncapped, the program never reclaimed, so no join state can have opened under its key"
    );
    assert!(
        free.identity_bytes <= uncapped.churned.read_bytes + (1 << 20),
        "uncapped, the kernel charged the program {:.1} MiB of storage reads and the block layer \
         attributes {:.1} MiB to it",
        mib(uncapped.churned.read_bytes as f64),
        mib(free.identity_bytes as f64)
    );
    assert!(
        under.identity_bytes >= free.identity_bytes + evicted_at_least / 2,
        "the same program read {:.1} MiB from the device capped and {:.1} MiB uncapped: the \
         difference is the memory limit's, and it has to show",
        mib(under.identity_bytes as f64),
        mib(free.identity_bytes as f64)
    );
}

// ---------------------------------------------------------------------------
// The sessions themselves
// ---------------------------------------------------------------------------

/// Every session this crossing ships validates, and runs or says what it
/// needs.
///
/// Both are rewritten and run by the scenarios above, but a reader runs them as
/// shipped, and a session that only works as a test's input is not one. A
/// machine that cannot serve one is an answer (1 or 3) that names the source;
/// a refusal of the request itself (2) is a session that means nothing.
#[test]
fn every_cross_machine_session_validates_and_runs_or_names_what_it_needs() {
    let mut sessions: Vec<PathBuf> = std::fs::read_dir(example(""))
        .expect("examples/ is readable")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("cross-machine-") && name.ends_with(".yml"))
        })
        .collect();
    sessions.sort();
    assert!(
        sessions.len() >= 2,
        "only {} cross-machine sessions were found",
        sessions.len()
    );
    for session in &sessions {
        let path = session.to_str().expect("a printable path");
        let validated = Command::new(BINARY)
            .args(["validate", "-f", path])
            .output()
            .expect("omn should run");
        assert!(
            validated.status.success(),
            "{path} does not validate: {}",
            stderr(&validated)
        );
        let ran = Command::new(BINARY)
            .args(["-f", path, "-d", "300ms"])
            .output()
            .expect("omn should run");
        match ran.status.code() {
            Some(0) => {}
            Some(1) | Some(3) => {
                let message = stderr(&ran);
                assert!(
                    message.contains("error:"),
                    "{path} could not run here and did not say why: {message}"
                );
                eprintln!(
                    "{path}: not served here: {}",
                    message
                        .lines()
                        .find(|line| line.contains("error:"))
                        .unwrap_or("")
                );
            }
            code => panic!(
                "{path} exited {code:?}, which is neither a run nor a machine's answer: {}",
                stderr(&ran)
            ),
        }
    }
}
