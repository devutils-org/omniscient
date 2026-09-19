//! Scenarios for the `power` family: does it answer a real question, and is
//! the answer right?
//!
//! The bar here is not that a measurement came back. It is that a property
//! **the workload chose** shows up in what the tool reported. Three of those
//! properties carry this file:
//!
//! - A spin loop pinned to one core with `taskset` never sleeps, so that core
//!   enters no idle state for the whole window. `power.cstate` must report
//!   `idle_ns=0` on every tick of that core -- zero, not nearly zero -- while
//!   the cores around it report residency.
//! - That same loop retires cycles the core's own counter counts, and cycles
//!   over wall time is the frequency the core actually ran at. `power.freq`
//!   reads `cpuinfo_avg_freq`, which the processor derives from aperf/mperf
//!   and which knows nothing about the cycle counter. Two instruments, one
//!   number, and they have to agree.
//! - Every core spinning is more silicon switching than no core spinning, so a
//!   loaded window must cost visibly more joules than an idle one.
//!
//! Beside those sit the four classes `BRIEF.md` names: the capability sweep
//! over every axis the catalog declares, the backend comparison through
//! `--backend force:`, the loss path, and the three refusals. What this machine
//! cannot serve is skipped by name, never silently, and
//! `docs/scenarios/power.md` carries what no machine can check and the defects
//! these scenarios found.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use omn::request::ModeKind;
use omn::Catalog;

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

/// Usage, or a failure while running -- which is also where a collector that
/// refuses at attach time lands.
const EXIT_FAILED: i32 = 1;
/// The request was refused: it cannot mean what it asks for, or this machine
/// cannot serve it. `BRIEF.md` asks for those two to be told apart; today they
/// are not, and `docs/scenarios/power.md` records it.
const EXIT_REFUSED: i32 = 2;
/// The request is valid and nothing in this build collects it yet.
const EXIT_NO_COLLECTOR: i32 = 3;

fn run(args: &[&str]) -> Output {
    Command::new(BINARY)
        .args(args)
        .output()
        .expect("omn should run")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Runs a session that is not shipped as a file, from standard input.
///
/// `tests/cli.rs::the_shipped_examples_validate` requires every session in
/// `examples/` to validate and to be worth running, which is right; a session
/// written to pin one comparison is neither, and lives here.
fn written_session(session: &str) -> Output {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new(BINARY)
        .args(["-f", "-", "--input-format", "yaml"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("omn should run");
    child
        .stdin
        .as_mut()
        .expect("stdin is piped")
        .write_all(session.as_bytes())
        .expect("writing the session");
    child.wait_with_output().expect("omn should finish")
}

fn example(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join(name)
}

/// One `name=value` field of a `raw` line, matched as a whole token so that
/// `idle_ns=` does not also answer for `idle_ns_total=`.
fn field<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|token| token.strip_prefix(name))
}

/// Whether a source can be served here, asked of the tool rather than guessed
/// from the filesystem.
///
/// A backend whose requirements no probe covers still runs -- it is reported as
/// assumed rather than checked -- so only "no backend is usable" means there is
/// nothing to exercise. `power.temp` is exactly that case here, and reading
/// this as "usable through" would skip it.
fn usable(source: &str) -> bool {
    !stdout(&run(&["list", source])).contains("no backend is usable")
}

fn cores() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

/// A spin loop pinned to one core, started from outside the tool.
///
/// `taskset` calls `sched_setaffinity`; nothing in `omn` chose the core or
/// started the loop, which is what makes "core N was busy for this whole
/// window" an independent fact about the run rather than something the tool
/// reported about itself. The loop never blocks, so the core it is pinned to
/// cannot enter an idle state and cannot be left for want of work.
struct Pinned(Child);

impl Pinned {
    fn to_core(cpu: usize) -> Option<Self> {
        Command::new("taskset")
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
            .ok()
            .map(Self)
    }
}

impl Drop for Pinned {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Every core spinning, unpinned: the load the energy scenario contrasts with
/// an idle machine.
struct Everything(Vec<Child>);

impl Everything {
    fn busy() -> Self {
        let children = (0..cores())
            .filter_map(|_| {
                Command::new("/bin/sh")
                    .args(["-c", "while :; do :; done"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .ok()
            })
            .collect();
        Self(children)
    }
}

impl Drop for Everything {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The scenarios that need the machine to themselves, held one at a time.
///
/// `cargo test` runs the tests of one binary in parallel threads, and two of
/// these put every core under load on purpose. Run at the same time they
/// measure each other: a core "pinned" to one spin loop is shared with
/// twenty-four more, and the energy of an idle window is taken while the
/// machine is not idle. The lock is the scenario's, not the tool's -- what it
/// protects is the property the workload chose, which is the whole of the
/// measurement.
static THE_MACHINE: Mutex<()> = Mutex::new(());

fn alone() -> MutexGuard<'static, ()> {
    THE_MACHINE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn have_taskset() -> bool {
    Command::new("taskset")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// The mode string a `ModeKind` is asked for on the command line. The polled
/// interval is the scenario's choice, not the catalog's: the catalog names the
/// kind and the pace is the reader's.
fn mode_argument(kind: ModeKind) -> &'static str {
    match kind {
        ModeKind::Poll => "poll@50ms",
        ModeKind::Sample => "sample@997hz",
        ModeKind::Count => "count",
        ModeKind::Trace => "trace",
        ModeKind::Snapshot => "snapshot",
    }
}

// ---------------------------------------------------------------------------
// Ground truth: a property the workload chose, found by the tool.
// ---------------------------------------------------------------------------

/// A core held busy for the whole window entered no idle state, and the answer
/// is exactly zero.
///
/// The independent quantity is `sched_setaffinity`, called by `taskset` from
/// outside the tool, and a loop that never blocks. A core running such a loop
/// cannot idle, so `idle_ns` for it is not "small": it is zero, on every tick,
/// and the assertion is an equality rather than a bound.
///
/// The second half matters as much. A collector that had stopped reading, or
/// that read the wrong core, would also report zeroes -- so the same run has to
/// show residency somewhere else. Without that, this test would pass by not
/// measuring.
#[test]
fn a_core_held_busy_never_enters_an_idle_state() {
    if cores() < 2 {
        eprintln!("skipped: one core, so there is no second core to compare against");
        return;
    }
    if !have_taskset() {
        eprintln!("skipped: this machine has no `taskset`, so no core can be held busy");
        return;
    }
    if !usable("power.cstate") {
        eprintln!(
            "skipped: no backend of `power.cstate` is usable here, so cpuidle is out of reach"
        );
        return;
    }
    // Held alone: the second half of this check needs a core that *did* idle,
    // and the energy scenario puts every core under load.
    let _alone = alone();
    let held = cores() - 1;
    let Some(_load) = Pinned::to_core(held) else {
        eprintln!("skipped: `taskset` would not start, so no core can be held busy");
        return;
    };
    // Long enough for the shell to be exec'd and scheduled: a core measured
    // while its load is still starting is a core that was idle.
    std::thread::sleep(Duration::from_millis(400));

    let measured = run(&[
        "power.cstate",
        "--system",
        "-m",
        "poll@150ms",
        "-e",
        "raw",
        "-d",
        "600ms",
    ]);
    assert!(measured.status.success(), "{}", stderr(&measured));
    let out = stdout(&measured);

    let mut ticks_on_the_busy_core = 0;
    let mut a_core_that_idled = None;
    for line in out.lines() {
        let Some(core) = field(line, "cpu=") else {
            continue;
        };
        let idle_ns: u64 = field(line, "idle_ns=")
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("every cstate event carries `idle_ns`: {line}"));
        if core == held.to_string() {
            assert_eq!(
                idle_ns, 0,
                "core {held} ran a spin loop for this whole window, so it entered no idle \
                 state and its residency is zero, not {idle_ns} ns: {line}"
            );
            ticks_on_the_busy_core += 1;
        } else {
            let ratio: f64 = field(line, "idle_ratio=")
                .and_then(|value| value.parse().ok())
                .unwrap_or(0.0);
            if ratio > 0.5 {
                a_core_that_idled = Some(core.to_owned());
            }
        }
    }

    assert!(
        ticks_on_the_busy_core >= 3,
        "a 600 ms window at a 150 ms tick is at least three reads of core {held}, got \
         {ticks_on_the_busy_core}: {out}"
    );
    assert!(
        a_core_that_idled.is_some(),
        "no other core reported any residency, so the zero on core {held} is a collector that \
         read nothing rather than a core that never slept: {out}"
    );
}

/// The frequency of a busy core, against the cycles that core issued.
///
/// Two instruments that share nothing. `power.freq` reads `cpuinfo_avg_freq`,
/// which the processor derives from its aperf/mperf ratio; `cpu.cycles` opened
/// against the core counts what the core's clock actually issued. A core that
/// never halts issues one cycle per cycle of its clock, so that count over the
/// window *is* the frequency -- and the two numbers have to land on each other.
///
/// The precondition is checked rather than assumed, in the same run and by a
/// third source: `power.cstate` has to report that core 1 entered no idle
/// state. Counting against the core rather than against the task is what makes
/// this robust to the core being shared -- what is measured is the core's
/// clock, not one program's share of it.
///
/// This runs the two sessions this family ships, unmodified: one supplies the
/// load, the other measures and carries its own oracle. It is the slowest
/// scenario here, about two seconds, and `docs/scenarios/power.md` says why it
/// cannot be shorter.
#[test]
fn the_frequency_of_a_busy_core_matches_the_cycles_that_core_issued() {
    if cores() < 2 {
        eprintln!("skipped: the sessions pin to core 1, and this machine has one core");
        return;
    }
    if !have_taskset() {
        eprintln!("skipped: this machine has no `taskset`, so the load cannot be pinned");
        return;
    }
    if !usable("power.freq") {
        eprintln!("skipped: no backend of `power.freq` is usable here, so cpufreq is out of reach");
        return;
    }

    let _alone = alone();
    let mut load = Command::new(BINARY)
        .args([
            "-f",
            &example("power-load-on-one-core.yml").to_string_lossy(),
            "--format",
            "json",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("omn should run");
    // The pause the session's own header names: the measured second has to sit
    // wholly inside the loaded two.
    std::thread::sleep(Duration::from_millis(700));
    let measured = run(&[
        "-f",
        &example("power-core-under-load.yml").to_string_lossy(),
        "--format",
        "json",
    ]);
    let _ = load.wait();

    assert!(measured.status.success(), "{}", stderr(&measured));
    let seen: serde_json::Value =
        serde_json::from_str(&stdout(&measured)).expect("the measuring session emits JSON");
    let results = seen["results"]
        .as_array()
        .expect("a session reports one result per request");

    let reported_hz = series_of(results, "core frequency", "summary", "cpu:1")
        .and_then(|series| series["mean"].as_f64())
        .expect("the session reports a mean frequency for core 1");
    let idled = series_of(results, "core idleness", "summary", "cpu:1")
        .and_then(|series| series["max"].as_f64())
        .expect("the session reports core 1's idle residency");
    let issued = series_of(results, "cycles the core issued", "rate", "cpu:1")
        .and_then(|series| series["per_second"].as_f64())
        .expect("the session reports the cycles core 1 issued");

    if idled != 0.0 {
        eprintln!(
            "skipped: core 1 idled for {idled} ns during the window, so the load did not hold \
             it and its cycles are not its clock. Something else on this machine is competing \
             for core 1"
        );
        return;
    }

    let apart = (reported_hz - issued).abs() / issued;
    assert!(
        apart < 0.10,
        "a core that issued {:.3} GHz of cycles without ever halting cannot have run at \
         {:.3} GHz: the two instruments are {:.1} % apart",
        issued / 1e9,
        reported_hz / 1e9,
        apart * 100.0
    );
    assert!(
        reported_hz > 5e8,
        "a core running a spin loop does not sit at {reported_hz} Hz"
    );
}

/// One series of one emit of one labelled request, out of a session's JSON.
fn series_of<'a>(
    results: &'a [serde_json::Value],
    label: &str,
    emit: &str,
    key: &str,
) -> Option<&'a serde_json::Value> {
    results
        .iter()
        .find(|result| result["label"] == label)?
        .get("outputs")?
        .as_array()?
        .iter()
        .find(|output| output["emit"] == emit)?
        .get("series")?
        .as_array()?
        .iter()
        .find(|series| series["key"] == key)
}

/// A machine with every core spinning costs more than one that is not.
///
/// The property the workload chooses is coarse and physical: switching a core
/// draws more power than leaving it idle, so putting a spin loop on every core
/// has to show up at the package. It is also the family's scale test. RAPL
/// counts in units of 2^-32 joules and publishes what one tick is worth beside
/// the counter; a collector that assumed microjoules would be out by ten
/// orders of magnitude, so a package that lands in the tens of watts rather
/// than the nanowatts or the gigawatts is the evidence that the scale was read
/// rather than guessed. That half runs everywhere.
///
/// The contrast needs one thing this scenario cannot arrange: room to add
/// load. So how busy the machine actually was is read from `/proc/stat` over
/// each window -- independently of the tool, and the same accounting
/// `cpu.util` is held against elsewhere in this repository -- and the margin
/// demanded is tied to how much busier the loaded window really was. A
/// machine that was already saturated before the spin loops started cannot
/// answer this question, and says so rather than failing.
///
/// Measured on an otherwise idle machine: 64.4 W against 145.3 W, which is
/// 1 + 1.27 per unit of added busyness. The coefficient asserted is 0.25, a
/// fifth of that.
#[test]
fn a_machine_with_every_core_busy_costs_more_than_an_idle_one() {
    if !usable("power.energy") {
        eprintln!(
            "skipped: no backend of `power.energy` is usable here -- this part publishes no RAPL \
             domain, or `perf_event_paranoid` refuses to open one"
        );
        return;
    }

    let _alone = alone();
    let (quiet, quiet_busyness) = watts_and_busyness();
    let (loaded, loaded_busyness) = {
        let _load = Everything::busy();
        std::thread::sleep(Duration::from_millis(300));
        watts_and_busyness()
    };

    let (Some(quiet), Some(loaded)) = (quiet, loaded) else {
        panic!("`power.energy` is usable here, so a counted window reports a rate");
    };
    assert!(
        (0.5..2000.0).contains(&quiet),
        "a package drawing {quiet} W means the RAPL scale was not read from the kernel"
    );

    // Two different ways there is nothing to contrast, and the first is not
    // the delta. A package has a power limit, and a machine already most of the
    // way to it draws the same watts at 69 % busy as at 100 % -- measured here
    // at 145.5 W for both, while the other nine hundred tests of this suite ran
    // beside it. The baseline has to be an idle package, not merely a machine
    // with a few cores spare.
    if quiet_busyness > 0.25 {
        eprintln!(
            "skipped the contrast: /proc/stat says this machine was already {:.0} % busy before \
             the spin loops started, so the package was near its power limit and adding load \
             cannot raise it. This scenario needs an idle machine",
            quiet_busyness * 100.0
        );
        return;
    }

    let added = loaded_busyness - quiet_busyness;
    if added < 0.15 {
        eprintln!(
            "skipped the contrast: /proc/stat says this machine was {:.0} % busy before the spin \
             loops started and {:.0} % after, so no load was added and there is no change in \
             energy to attribute to one. This scenario needs a machine with cores to spare",
            quiet_busyness * 100.0,
            loaded_busyness * 100.0
        );
        return;
    }
    assert!(
        loaded > quiet * (1.0 + 0.25 * added),
        "the machine went from {:.0} % busy to {:.0} % and the package went from {quiet:.1} W to \
         {loaded:.1} W, which is not the cost of switching that much more silicon",
        quiet_busyness * 100.0,
        loaded_busyness * 100.0
    );
}

/// One counted second of package energy as watts, with how busy `/proc/stat`
/// says the machine was over the same second.
fn watts_and_busyness() -> (Option<f64>, f64) {
    let before = cpu_time();
    let measured = run(&[
        "power.energy",
        "--system",
        "-m",
        "count",
        "-e",
        "rate",
        "-d",
        "1s",
        "--format",
        "json",
    ]);
    let after = cpu_time();

    let busy = match (before, after) {
        (Some((busy_before, total_before)), Some((busy_after, total_after)))
            if total_after > total_before =>
        {
            (busy_after - busy_before) as f64 / (total_after - total_before) as f64
        }
        _ => 0.0,
    };
    if !measured.status.success() {
        return (None, busy);
    }
    let watts = serde_json::from_str::<serde_json::Value>(&stdout(&measured))
        .ok()
        .and_then(|document| {
            document["results"][0]["outputs"][0]["series"][0]["per_second"].as_f64()
        });
    (watts, busy)
}

/// Busy and total jiffies across every cpu, from `/proc/stat`'s own accounting.
fn cpu_time() -> Option<(u64, u64)> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let line = stat.lines().next()?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|field| field.parse().ok())
        .collect();
    if fields.len() < 5 {
        return None;
    }
    let total: u64 = fields.iter().sum();
    // Fields four and five are idle and iowait; everything else is work.
    let idle = fields[3] + fields[4];
    Some((total - idle, total))
}

/// The temperature reported is the hottest *processor* sensor the machine
/// offers, read back independently from `/sys/class/hwmon`.
///
/// The tool's job here is a choice, not an arithmetic: a machine offers a
/// package, an NVMe controller and a wifi chip, and their mean is a number
/// about nothing. So the check walks hwmon itself, keeps the chips whose
/// driver says they are processors, takes the hottest, and holds the tool to
/// it -- including the millidegree conversion, which is where a factor of a
/// thousand would hide.
#[test]
fn the_temperature_is_the_hottest_processor_sensor_hwmon_offers() {
    let Some((driver, before)) = hottest_processor_sensor() else {
        eprintln!(
            "skipped: no chip under /sys/class/hwmon reports a temperature this test can hold \
             the tool against"
        );
        return;
    };
    if !usable("power.temp") {
        eprintln!("skipped: no backend of `power.temp` is usable here");
        return;
    }

    let measured = run(&["power.temp", "--system", "-m", "snapshot", "-e", "raw"]);
    // A package under load moves several degrees a second, so the tool's read
    // is bracketed rather than compared with one of them: it has to land
    // between two independent reads taken around it.
    let after = hottest_processor_sensor().map_or(before, |(_, celsius)| celsius);
    assert!(measured.status.success(), "{}", stderr(&measured));
    let out = stdout(&measured);
    let line = out
        .lines()
        .find(|line| line.contains("celsius="))
        .unwrap_or_else(|| panic!("a snapshot reports one event, carrying a temperature: {out}"));

    let reported: f64 = field(line, "celsius=")
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("the event carries a temperature: {line}"));
    let sensor = field(line, "sensor=")
        .unwrap_or_else(|| panic!("the event names the sensor it read: {line}"));

    assert!(
        sensor.starts_with(&driver),
        "hwmon's hottest processor sensor belongs to `{driver}`, and the tool read `{sensor}`: \
         {line}"
    );
    // Degrees, not millidegrees, and inside the band the two bracketing reads
    // of hwmon itself draw. The band is generous because the quantity moves;
    // what it still catches is everything structural -- a factor of a thousand
    // left in millidegrees, or the nvme controller thirty degrees below.
    //
    // Fifteen degrees, not five, and the difference is the sensor rather than
    // the caution. The hottest processor sensor on this part is a per-CCD one,
    // and a die's temperature swings several degrees between two reads
    // milliseconds apart -- measured here at 62.25 from the tool between two
    // reference reads of 55.375 and 56.75, with `Tctl`, the package average
    // the narrower band was sized for, sitting at 55.5 throughout. A band that
    // fits `Tctl` does not fit `Tccd1`, and the structural failures are all
    // an order of magnitude away from either.
    let floor = before.min(after) - 15.0;
    let ceiling = before.max(after) + 15.0;
    assert!(
        (floor..=ceiling).contains(&reported),
        "hwmon read {before} then {after} celsius around this snapshot, and the tool says \
         {reported}: {line}"
    );
}

/// The hottest sensor of a chip whose driver says it is a processor, straight
/// out of sysfs. Falls back to the hottest chip of any kind, which is what the
/// collector itself promises to do on a machine with no processor sensor.
fn hottest_processor_sensor() -> Option<(String, f64)> {
    const PROCESSORS: [&str; 5] = ["k10temp", "coretemp", "zenpower", "cpu_thermal", "k8temp"];
    let mut best: Option<(String, f64)> = None;
    let mut best_of_any: Option<(String, f64)> = None;
    for chip in std::fs::read_dir("/sys/class/hwmon").ok()?.flatten() {
        let name = std::fs::read_to_string(chip.path().join("name")).unwrap_or_default();
        let name = name.trim().to_owned();
        if name.is_empty() {
            continue;
        }
        for sensor in std::fs::read_dir(chip.path()).ok()?.flatten() {
            let path = sensor.path();
            let file = path.file_name().unwrap_or_default().to_string_lossy();
            if !file.starts_with("temp") || !file.ends_with("_input") {
                continue;
            }
            let Some(millidegrees) = std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| text.trim().parse::<f64>().ok())
            else {
                continue;
            };
            let celsius = millidegrees / 1000.0;
            let seen = (name.clone(), celsius);
            if best_of_any.as_ref().is_none_or(|(_, hot)| celsius > *hot) {
                best_of_any = Some(seen.clone());
            }
            if PROCESSORS.contains(&name.as_str())
                && best.as_ref().is_none_or(|(_, hot)| celsius > *hot)
            {
                best = Some(seen);
            }
        }
    }
    best.or(best_of_any)
}

// ---------------------------------------------------------------------------
// Class A -- the capability sweep: does every axis the catalog declares work?
// ---------------------------------------------------------------------------

/// What the tool did when asked for one combination of axes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    /// It measured.
    Served,
    /// The request cannot mean what it asks for.
    Refused,
    /// Nothing in this build collects it.
    NotBuilt,
    /// A collector was chosen and would not start.
    WouldNotStart,
}

/// Every mode and every emit the catalog declares for this family, asked for
/// in turn.
///
/// The catalog is data and the CLI is a projection of it, so a combination the
/// catalog offers and the build cannot serve is a promise broken in the one
/// place nobody looks. Each answer has to be one of four recognisable shapes,
/// and each of the three refusals has to name the thing that is actually
/// missing -- an emit the source does not have, a backend nothing collects, a
/// file this machine does not expose. An answer of any other shape, or a
/// success that measured nothing, is the failure this sweep exists to catch.
///
/// What it found is in `docs/scenarios/power.md`: `hist` is declared by
/// `power.freq` and `power.cstate` and is reachable through neither, and
/// `trace` on either of them is refused as a run failure rather than as a
/// mechanism this build does not have.
#[test]
fn every_axis_this_family_declares_is_served_or_refused_by_name() {
    let catalog = Catalog::embedded().expect("the embedded catalog loads");
    let mut answers: BTreeMap<String, Vec<(String, String, Answer)>> = BTreeMap::new();

    for entry in catalog.family("power") {
        let source = entry.id.to_string();
        let chain: Vec<String> = entry.backends.iter().map(|b| b.id.clone()).collect();
        for mode in &entry.modes {
            for emit in &entry.emits {
                let mode = mode_argument(*mode);
                let emit = emit.as_str();
                let attempt = run(&[&source, "--system", "-m", mode, "-e", emit, "-d", "200ms"]);
                let out = stdout(&attempt);
                let message = stderr(&attempt);
                let answer = match attempt.status.code() {
                    Some(0) => {
                        assert!(
                            out.contains("events over"),
                            "{source} {mode} {emit} succeeded without saying what it saw: {out}"
                        );
                        let events: usize = out
                            .lines()
                            .find_map(|line| line.split_whitespace().next()?.parse().ok())
                            .unwrap_or(0);
                        assert!(
                            events > 0,
                            "{source} {mode} {emit} reported success and zero events, which is \
                             the quiet nothing this family exists to prevent: {out}"
                        );
                        Answer::Served
                    }
                    Some(EXIT_REFUSED) => {
                        assert!(
                            message.contains(mode.split('@').next().unwrap_or(mode))
                                || message.contains(emit)
                                || message.contains("no viable backend"),
                            "{source} {mode} {emit} was refused without naming the mode, the \
                             emit or the machine: {message}"
                        );
                        Answer::Refused
                    }
                    Some(EXIT_NO_COLLECTOR) => {
                        // Two ways to reach this code, and they are the same
                        // answer: no collector at all for the selected backend,
                        // or a collector that has not written the mode that was
                        // asked for. The second used to exit 1, which is the
                        // fourth row of defect 2 in docs/scenarios/power.md.
                        let no_collector = message.contains("no collector is written");
                        assert!(
                            no_collector || message.contains("cannot start collecting"),
                            "{source} {mode} {emit}: {message}"
                        );
                        assert!(
                            !no_collector || chain.iter().any(|backend| message.contains(backend)),
                            "a build that does not implement something has to name which \
                             backend of {chain:?} it meant: {message}"
                        );
                        Answer::NotBuilt
                    }
                    Some(EXIT_FAILED) => {
                        assert!(
                            message.contains("cannot start collecting"),
                            "{source} {mode} {emit} failed without saying what could not \
                             start: {message}"
                        );
                        Answer::WouldNotStart
                    }
                    other => panic!(
                        "{source} {mode} {emit} answered with exit {other:?}, which is none of \
                         the four things this tool is allowed to say: {message}{out}"
                    ),
                };
                answers.entry(source.clone()).or_default().push((
                    mode.to_owned(),
                    emit.to_owned(),
                    answer,
                ));
            }
        }
    }

    let served: usize = answers
        .values()
        .flatten()
        .filter(|(_, _, answer)| *answer == Answer::Served)
        .count();
    assert!(
        served > 0,
        "no combination of any power source ran here, which no Linux with cpufreq, cpuidle or \
         hwmon does: {answers:#?}"
    );
    // Whatever a source did, it did it the same way for every emit of a given
    // mode -- a mode that resolves for `summary` and not for `timeline` would
    // mean the emit layer, not the mechanism, decided whether the machine can
    // be measured.
    for (source, attempts) in &answers {
        let mut by_mode: BTreeMap<&str, Vec<Answer>> = BTreeMap::new();
        for (mode, _, answer) in attempts {
            by_mode.entry(mode.as_str()).or_default().push(*answer);
        }
        for (mode, seen) in by_mode {
            // A rejection is decided before a backend is ever chosen -- `rate`
            // belongs to `count`, whatever the machine can serve -- so it says
            // nothing about the mechanism and is left out of the comparison.
            let resolved: Vec<Answer> = seen
                .iter()
                .copied()
                .filter(|answer| *answer != Answer::Refused)
                .collect();
            let collected = resolved
                .iter()
                .filter(|answer| **answer == Answer::Served)
                .count();
            assert!(
                collected == 0 || collected == resolved.len(),
                "`{source}` in `{mode}` mode can be collected for some emits and not others, so \
                 the emit decided whether the mechanism exists: {seen:?}"
            );
        }
    }
}

/// Two requests at two rates, in either order, each served at its own.
///
/// Every `power` collector takes the default `tick() == None`, so this family
/// met the runner's fallback head on: where no collector declared a pace, the
/// run took `resolved[0].request.mode` -- the *first* request's interval -- and
/// applied it to everything. `poll@100ms` written second was served at 500 ms;
/// `poll@500ms` written second was served at 100 ms; and the resolved record
/// written beside the results still said what had been asked for. That is
/// defect 4 of `docs/scenarios/power.md`, and the reproduction is the pair of
/// files below.
///
/// The harmful direction was the slow-first one, because the window also
/// overran its own duration by 70 %: the deadline is only checked once a full
/// tick has elapsed, and the tick was five times the one that was asked for.
#[test]
fn two_requests_at_two_rates_are_each_served_at_their_own() {
    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("power-two-rates");
    std::fs::create_dir_all(&scratch).expect("a directory to write sessions into");

    const SLOW: &str = "\x20 - label: slow\n\
                        \x20   source: power.temp\n\
                        \x20   target: system\n\
                        \x20   mode: poll@500ms\n\
                        \x20   emit: [summary]\n";
    const FAST: &str = "\x20 - label: fast\n\
                        \x20   source: power.temp\n\
                        \x20   target: system\n\
                        \x20   mode: poll@100ms\n\
                        \x20   emit: [summary]\n";

    let readings = |name: &str, requests: String| {
        let path = scratch.join(name);
        std::fs::write(
            &path,
            format!("format: json\nduration: 1s\nrequests:\n{requests}"),
        )
        .expect("the test owns this file");
        let output = run(&["-f", &path.to_string_lossy(), "--format", "json"]);
        assert!(output.status.success(), "{name}: {}", stderr(&output));
        let document: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("json out");
        let of = |label: &str| {
            document["results"]
                .as_array()
                .expect("results")
                .iter()
                .find(|result| result["label"] == label)
                .unwrap_or_else(|| panic!("nothing labelled `{label}` in {name}"))["events_seen"]
                .as_u64()
                .expect("an event count")
        };
        (of("slow"), of("fast"))
    };

    for (name, requests) in [
        ("slow-first.yml", format!("{SLOW}{FAST}")),
        ("fast-first.yml", format!("{FAST}{SLOW}")),
    ] {
        let (slow, fast) = readings(name, requests);
        // Over a second: about two readings at 500 ms and about ten at 100 ms.
        // What is asserted is the separation, because a machine running eight
        // suites at once reads late and the run says so when it does.
        assert!(
            fast > slow,
            "{name}: `poll@100ms` took {fast} readings and `poll@500ms` took {slow}, so one \
             request is still being served at the other's rate"
        );
        assert!(
            slow <= 4,
            "{name}: `poll@500ms` over a second is two or three readings, and this took {slow}"
        );
        assert!(
            fast >= 5,
            "{name}: `poll@100ms` over a second is about ten readings, and this took {fast}"
        );
    }

    let _ = std::fs::remove_dir_all(&scratch);
}

/// A `snapshot` beside a poll is a single read, and invents no losses.
///
/// Two things met here and the second is this family's alone. The run was a
/// snapshot only when *every* request was one, so a snapshot written beside a
/// poll was read on every tick; and `power.temp` is the only collector in
/// `src/collect/` whose `tick()` returns `Some(ZERO)` in snapshot mode, so the
/// window's interval -- the minimum over the collectors -- collapsed to zero
/// and every round was then counted as late against a "0ns tick". A request
/// that asked for one read got a hundred and fifty-nine, and the run reported
/// 158 losses that had not happened, which is the one thing this project
/// promises never to do in the other direction. Defect 3 of
/// `docs/scenarios/power.md`.
#[test]
fn a_snapshot_beside_a_poll_is_one_read_and_invents_no_losses() {
    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("power-mixed-modes");
    std::fs::create_dir_all(&scratch).expect("a directory to write a session into");
    let path = scratch.join("mixed.yml");
    std::fs::write(
        &path,
        "format: json\n\
         duration: 600ms\n\
         requests:\n\
         \x20 - label: a\n\
         \x20   source: power.temp\n\
         \x20   target: system\n\
         \x20   mode: poll@200ms\n\
         \x20   emit: [summary]\n\
         \x20 - label: b\n\
         \x20   source: power.temp\n\
         \x20   target: system\n\
         \x20   mode: snapshot\n\
         \x20   emit: [summary]\n",
    )
    .expect("the test owns this file");

    let output = run(&["-f", &path.to_string_lossy(), "--format", "json"]);
    if !output.status.success() {
        let message = stderr(&output);
        assert!(
            message.contains("no viable backend"),
            "the mixed session failed for a reason other than a missing sensor: {message}"
        );
        eprintln!("skipped: this machine exposes no temperature sensor");
        return;
    }
    let document: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("json out");
    let of = |label: &str| {
        document["results"]
            .as_array()
            .expect("results")
            .iter()
            .find(|result| result["label"] == label)
            .unwrap_or_else(|| panic!("nothing labelled `{label}`"))
    };

    assert_eq!(
        of("b")["events_seen"].as_u64(),
        Some(1),
        "the snapshot asked for one read: {document}"
    );
    assert!(
        of("a")["events_seen"].as_u64().unwrap_or(0) >= 2,
        "and the polled request keeps its own 200 ms over 600 ms: {document}"
    );
    for label in ["a", "b"] {
        assert_eq!(
            of(label)["losses"]["late_ticks"].as_u64(),
            Some(0),
            "`{label}` reported ticks that ran late against a cadence nothing asked for: \
             {document}"
        );
    }
    let _ = std::fs::remove_dir_all(&scratch);
}

/// A result survives every output format that claims it, and is refused by
/// name by the ones that cannot carry it.
///
/// A format that claims an emit it cannot express is the same broken promise
/// as a mode that resolves and then refuses, one layer further out. `folded`
/// and `pprof` carry stacks; `speedscope` carries stacks and timelines;
/// `chrome` carries timelines. This family produces no stacks, so the three
/// stack formats have to say so rather than emit an empty profile.
#[test]
fn a_result_survives_every_format_that_claims_it() {
    if !usable("power.freq") {
        eprintln!("skipped: no backend of `power.freq` is usable here");
        return;
    }
    let timeline = ["-m", "poll@100ms", "-e", "timeline", "-d", "300ms"];

    for format in ["json", "yaml", "toml", "ndjson", "speedscope", "chrome"] {
        let mut args = vec!["power.freq", "--system"];
        args.extend_from_slice(&timeline);
        args.extend_from_slice(&["--format", format]);
        let rendered = run(&args);
        assert!(
            rendered.status.success(),
            "`{format}` claims a timeline and would not render one: {}",
            stderr(&rendered)
        );
        let out = stdout(&rendered);
        assert!(!out.trim().is_empty(), "`{format}` rendered nothing at all");
        if matches!(format, "json" | "ndjson" | "speedscope" | "chrome") {
            let first = out.lines().next().unwrap_or_default();
            let parsed = if format == "ndjson" {
                serde_json::from_str::<serde_json::Value>(first).is_ok()
            } else {
                serde_json::from_str::<serde_json::Value>(&out).is_ok()
            };
            assert!(
                parsed,
                "`{format}` produced something that is not JSON: {out}"
            );
        }
    }

    for format in ["folded", "pprof"] {
        let mut args = vec!["power.freq", "--system"];
        args.extend_from_slice(&timeline);
        args.extend_from_slice(&["--format", format]);
        let refused = run(&args);
        assert_eq!(refused.status.code(), Some(EXIT_REFUSED));
        let message = stderr(&refused);
        assert!(
            message.contains(format) && message.contains("timeline"),
            "a format that cannot carry a timeline has to name both: {message}"
        );
    }

    // And the other way: a summary is not a profile, and the four formats that
    // carry stacks or timelines must refuse it rather than render an empty one.
    for format in ["folded", "pprof", "speedscope", "chrome"] {
        let refused = run(&[
            "power.temp",
            "--system",
            "-m",
            "poll@100ms",
            "-e",
            "summary",
            "-d",
            "200ms",
            "--format",
            format,
        ]);
        assert_eq!(
            refused.status.code(),
            Some(EXIT_REFUSED),
            "`{format}` accepted a summary: {}",
            stdout(&refused)
        );
        assert!(
            stderr(&refused).contains("summary"),
            "the refusal has to name the emit it cannot carry: {}",
            stderr(&refused)
        );
    }
}

/// Every session this family ships validates, and then either runs here or
/// refuses by name.
///
/// They are spawned at once rather than in turn: seven sessions with real
/// windows are nine seconds in sequence and about two in parallel, and they do
/// not contend for anything -- three read sysfs, one opens a RAPL counter, one
/// is a refusal.
#[test]
fn every_session_this_family_ships_validates_and_then_runs_or_refuses_by_name() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples");
    let mut sessions: Vec<PathBuf> = std::fs::read_dir(directory)
        .expect("examples/ exists")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().starts_with("power-"))
                .unwrap_or(false)
        })
        .collect();
    sessions.sort();
    assert!(!sessions.is_empty(), "this family ships no session");

    // One of these sessions pins a spin loop to core 1, which is the core the
    // frequency scenario measures.
    let _alone = alone();
    let mut running = Vec::new();
    for session in &sessions {
        let path = session.to_string_lossy().into_owned();
        let validated = run(&["validate", "-f", &path]);
        assert!(
            validated.status.success(),
            "{path} does not validate: {}",
            stderr(&validated)
        );
        let child = Command::new(BINARY)
            .args(["-f", &path])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("omn should run");
        running.push((session.clone(), child));
    }

    for (session, child) in running {
        let finished = child.wait_with_output().expect("omn should finish");
        let name = session.file_name().unwrap_or_default().to_string_lossy();
        let out = stdout(&finished);
        let message = stderr(&finished);
        match finished.status.code() {
            Some(0) => assert!(
                out.contains("events over"),
                "{name} ran without saying what it saw: {out}"
            ),
            Some(EXIT_NO_COLLECTOR) => assert!(
                message.contains("no collector is written"),
                "{name}: {message}"
            ),
            Some(EXIT_REFUSED) => assert!(
                message.contains("no viable backend") || message.contains("cannot"),
                "{name} was refused without a reason: {message}"
            ),
            Some(EXIT_FAILED) => assert!(
                message.contains("cannot start collecting"),
                "{name} failed without saying what could not start: {message}"
            ),
            other => panic!("{name} answered with exit {other:?}: {message}{out}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Class B -- the backend comparison: do two roads to the same number agree?
// ---------------------------------------------------------------------------

/// Defect 1, fixed: a counted energy window is one total, and it means the
/// same thing through either backend of the chain.
///
/// `count` is "one number for the window". Through `pmu` it was not: the
/// collector emitted a per-interval delta whether or not the mode was `count`,
/// so a counted three-second window came back as a series at the runner's
/// default poll interval, and `stop` then took a further closing read that
/// covered microseconds and landed in it as a zero. A `summary` of that said
/// the smallest interval was zero, the last was zero, and the mean was the
/// window's energy divided by one event more than there were intervals --
/// against a `rate` of the same window, which was right.
///
/// `EnergySysfs` already had the intended shape: nothing during the run, and
/// one whole-window delta from a baseline taken in `start`. The same request
/// meant two different things through the two backends of one chain, which is
/// exactly what the backend comparison exists to find.
///
/// The ground truth here is the kernel's own counter read twice over. A RAPL
/// domain is monotone, so the energy of a window is a difference and cannot
/// depend on how many times it was looked at: the counted total of a window
/// and the sum of a polled series over a window of the same length are the
/// same measurement of the same machine, and they are compared as such.
#[test]
fn a_counted_energy_window_is_one_total_and_not_a_series_with_a_zero() {
    if !usable("power.energy") {
        eprintln!("skipped: this machine publishes no RAPL domain at all");
        return;
    }
    let _alone = alone();

    let counted = run(&[
        "power.energy",
        "--system",
        "-m",
        "count",
        "-e",
        "summary",
        "-d",
        "2s",
        "--backend",
        "force:pmu",
        "--format",
        "json",
    ]);
    assert!(counted.status.success(), "{}", stderr(&counted));
    let document: serde_json::Value = serde_json::from_str(&stdout(&counted)).expect("a json run");
    let result = &document["results"][0];
    let series = &result["outputs"][0]["series"][0];

    // One event per domain, and this machine's chain opens one. A counted
    // window that is a series is the defect: four events for three seconds,
    // three of them intervals and the fourth a microsecond.
    let events = result["events_seen"].as_u64().expect("a count of events");
    assert_eq!(
        events, 1,
        "`count` asks for one number for the window and got {events}: {series}"
    );

    // And no part of it is the closing read's zero. min, max, mean and last
    // are one value because there is one value.
    for field in ["min", "max", "mean", "last"] {
        let value = series[field].as_f64().expect("a number");
        assert!(
            value > 0.0,
            "`{field}` of a counted window is {value} J; a zero there is a closing read that \
             covered microseconds"
        );
        assert!(
            (value - series["last"].as_f64().unwrap()).abs() < 1e-9,
            "one event makes every one of these the same number: {series}"
        );
    }

    // The same window asked for as a rate has to agree with it, which is the
    // comparison the defect was found by: `rate` was right and `summary` was
    // not, on one run of one machine.
    let total = series["last"].as_f64().unwrap();
    let window = result["observed_ns"].as_f64().expect("the window") / 1e9;
    let watts = total / window;
    assert!(
        (0.1..2000.0).contains(&watts),
        "a package drawing {watts} W over {window} s means this is not an energy measurement"
    );

    // Ground truth: the same counter, over *the same* window, read as a series
    // instead. A monotone hardware counter differenced once and differenced
    // ten times covers the same joules, and the only way to hold the load
    // still enough for that to be a comparison is to put both requests in one
    // session -- two consecutive two-second windows on a machine running the
    // rest of this suite are two different machines, measured here at 145 W
    // and 102 W with nothing wrong at all.
    //
    // The polled request leads, because a session is driven at its first
    // request's pace and a counted one asks for none: `examples/power-energy.yml`
    // is written the same way and says why. Defect 4.
    let together = written_session(
        "\
name: one window, counted and polled
duration: 2s
format: json

requests:
  - label: polled
    source: power.energy
    backend: force:pmu
    target: system
    mode: poll@200ms
    emit: [timeline]

  - label: counted
    source: power.energy
    backend: force:pmu
    target: system
    mode: count
    emit: [summary]
",
    );
    assert!(together.status.success(), "{}", stderr(&together));
    let together: serde_json::Value = serde_json::from_str(&stdout(&together)).expect("json");
    let results = together["results"].as_array().expect("two results");
    let counted_total = results[1]["outputs"][0]["series"][0]["last"]
        .as_f64()
        .expect("the counted window is one number");
    let counted_window = results[1]["observed_ns"].as_f64().expect("the window") / 1e9;

    let points = results[0]["outputs"][0]["series"][0]["points"]
        .as_array()
        .expect("a polled series")
        .clone();
    assert!(
        points.len() >= 5,
        "a two-second window polled every 200 ms is about ten intervals, and {} cannot be \
         compared with anything",
        points.len()
    );
    // Point `i` carries the joules of the interval that ended at its offset,
    // so the points after the first cover exactly the span between the first
    // offset and the last -- whatever spacing the runner actually used.
    let joules: f64 = points[1..]
        .iter()
        .map(|point| point["value"].as_f64().unwrap_or_default())
        .sum();
    let span = (points[points.len() - 1]["offset_ns"].as_f64().unwrap()
        - points[0]["offset_ns"].as_f64().unwrap())
        / 1e9;
    let polled_watts = joules / span;
    let counted_watts = counted_total / counted_window;
    let apart = (counted_watts - polled_watts).abs() / counted_watts.max(polled_watts);
    assert!(
        apart < 0.15,
        "one window of one machine says {counted_watts:.1} W counted and {polled_watts:.1} W \
         polled. These are one counter differenced two ways over the same seconds"
    );

    // The other half of the defect: the two backends of one chain have to mean
    // the same thing by `count`. `fs:sys` is root-only on this kernel, so what
    // can be checked here is that it is refused for that reason rather than
    // answering with a different shape.
    let through_sysfs = run(&[
        "power.energy",
        "--system",
        "-m",
        "count",
        "-e",
        "summary",
        "-d",
        "300ms",
        "--backend",
        "force:fs:sys",
        "--format",
        "json",
    ]);
    if !through_sysfs.status.success() {
        eprintln!(
            "skipped the backend half: /sys/class/powercap is root-only on this kernel, so \
             only `pmu` can be counted here. Run this suite as root to compare the shapes"
        );
        return;
    }
    let sysfs: serde_json::Value =
        serde_json::from_str(&stdout(&through_sysfs)).expect("a json run");
    assert_eq!(
        sysfs["results"][0]["events_seen"].as_u64(),
        Some(1),
        "`-m count` means one number for the window through either road of this chain"
    );
}

/// `power.energy` is reachable two ways, and where both are open they have to
/// agree.
///
/// `pmu` opens the RAPL domains through `perf_event_open`; `fs:sys` reads the
/// same counters under `/sys/class/powercap`. Same hardware register, two
/// routes, and the catalog exists for exactly this case. The declared
/// degradation is that the polled route averages a spike into the interval
/// that contains it, which over a steady window is no difference at all.
///
/// On an ordinary machine only one of the two is open: `energy_uj` has been
/// root-only since the counter was shown to leak what the machine was
/// computing. So this skips by naming root, and the session stays in
/// `examples/` for a machine that has it.
#[test]
fn the_two_roads_to_rapl_agree_where_both_are_open() {
    if !usable("power.energy") {
        eprintln!("skipped: this machine publishes no RAPL domain at all");
        return;
    }
    let counter = std::path::Path::new("/sys/class/powercap/intel-rapl:0/energy_uj");
    if !counter.exists() {
        eprintln!(
            "skipped: no /sys/class/powercap/intel-rapl:0/energy_uj, so the `fs:sys` road does \
             not exist here and there is nothing to compare the `pmu` road with"
        );
        return;
    }
    if std::fs::read_to_string(counter).is_err() {
        eprintln!(
            "skipped: /sys/class/powercap/intel-rapl:0/energy_uj is readable only by root on \
             this kernel, so the `fs:sys` road is shut and only `pmu` can be measured. Run this \
             suite as root to compare them"
        );
        // The refusal itself is still checked, because it is the half of the
        // comparison this machine *can* answer: a counter that is present and
        // out of reach calls for privileges, not for different hardware.
        let refused = run(&[
            "power.energy",
            "--system",
            "-m",
            "count",
            "-e",
            "rate",
            "-d",
            "300ms",
            "--backend",
            "force:fs:sys",
        ]);
        assert_eq!(refused.status.code(), Some(EXIT_FAILED));
        let message = stderr(&refused);
        assert!(
            message.contains("root"),
            "a counter that is there and out of reach calls for privileges: {message}"
        );
        assert!(
            !message.contains("does not exist"),
            "the file is there; saying otherwise sends the reader looking for other hardware: \
             {message}"
        );
        return;
    }

    let _alone = alone();
    let through_perf = forced_watts("pmu");
    let through_sysfs = forced_watts("fs:sys");
    let (Some(perf), Some(sysfs)) = (through_perf, through_sysfs) else {
        panic!("both roads are open on this machine, so both have to measure");
    };
    let apart = (perf - sysfs).abs() / perf.max(sysfs);
    assert!(
        apart < 0.15,
        "the same RAPL counter read through perf says {perf:.1} W and through powercap \
         {sysfs:.1} W, which no averaging of a spike explains"
    );
}

fn forced_watts(backend: &str) -> Option<f64> {
    let forced = format!("force:{backend}");
    let measured = run(&[
        "power.energy",
        "--system",
        "-m",
        "count",
        "-e",
        "rate",
        "-d",
        "1s",
        "--backend",
        &forced,
        "--format",
        "json",
    ]);
    if !measured.status.success() {
        return None;
    }
    let document: serde_json::Value = serde_json::from_str(&stdout(&measured)).ok()?;
    document["results"][0]["outputs"][0]["series"][0]["per_second"].as_f64()
}

/// Where a chain has one implemented backend and others declared, the
/// comparison is with the refusal: pinning the unimplemented one must say the
/// build does not have it, not that the run failed.
///
/// Three of this family's ten backends are declared and unwritten -- the eBPF
/// route into `power.freq`, `power.cstate` and `power.throttle`, and the
/// aperf/mperf route into `power.freq`. A reader who pins one of them is
/// asking "what would this cost through the other road?", and has to be told
/// which road is missing rather than handed a failure.
#[test]
fn pinning_a_backend_this_build_does_not_have_names_it_and_exits_three() {
    let catalog = Catalog::embedded().expect("the embedded catalog loads");
    let written = omn::collect::coverage();
    let mut checked = 0;

    for entry in catalog.family("power") {
        let source = entry.id.to_string();
        let have = written.get(source.as_str()).cloned().unwrap_or_default();
        for backend in &entry.backends {
            if have.contains(&backend.id.as_str()) {
                continue;
            }
            let forced = format!("force:{}", backend.id);
            let mode = mode_argument(entry.modes[0]);
            let emit = entry.emits[0].as_str();
            let attempt = run(&[
                &source,
                "--system",
                "-m",
                mode,
                "-e",
                emit,
                "-d",
                "200ms",
                "--backend",
                &forced,
            ]);
            let message = stderr(&attempt);
            assert_eq!(
                attempt.status.code(),
                Some(EXIT_NO_COLLECTOR),
                "pinning `{}` on `{source}` is a build that lacks a mechanism, not a run that \
                 broke: {message}",
                backend.id
            );
            assert!(
                message.contains(&backend.id) && message.contains("no collector is written"),
                "the refusal has to name the backend that is missing: {message}"
            );
            assert!(
                message.contains("--dump-request") || message.contains("valid"),
                "and has to say the request itself was fine: {message}"
            );
            assert!(
                stdout(&attempt).is_empty() || !stdout(&attempt).contains("events over"),
                "nothing was collected, so nothing may be reported: {}",
                stdout(&attempt)
            );
            checked += 1;
        }
    }
    assert!(
        checked > 0,
        "every backend of this family is written, so this comparison has nothing to make"
    );
}

/// Pinning a backend that exists in the build but not on the machine names the
/// file that is missing, and does not blame the build.
///
/// The two absences are different answers and the reader acts differently on
/// each: one is a mechanism nobody has written, the other is a machine that
/// does not have the interface. `thermal_throttle` is the second -- an x86
/// thermal-interrupt interface that this AMD part does not implement.
#[test]
fn pinning_a_backend_this_machine_does_not_offer_names_what_is_missing() {
    let exposed = std::path::Path::new("/sys/devices/system/cpu/cpu0/thermal_throttle").exists();
    let attempt = run(&[
        "power.throttle",
        "--system",
        "-m",
        "poll@200ms",
        "-e",
        "summary",
        "-d",
        "400ms",
        "--backend",
        "force:fs:sys",
    ]);
    if exposed {
        assert!(
            attempt.status.success(),
            "this machine exposes the counters, so pinning the sysfs road should measure: {}",
            stderr(&attempt)
        );
        return;
    }
    assert_eq!(attempt.status.code(), Some(EXIT_FAILED));
    let message = stderr(&attempt);
    assert!(
        message.contains("thermal_throttle"),
        "the refusal has to name the interface this part does not implement: {message}"
    );
    assert!(
        !message.contains("no collector is written"),
        "the collector is written; this machine has nowhere for it to read: {message}"
    );
    assert!(
        !stdout(&attempt).contains("events over"),
        "a machine that cannot see throttling must not report a window of none"
    );
}

// ---------------------------------------------------------------------------
// Class C -- the loss path: does the tool say what it did not see?
// ---------------------------------------------------------------------------

/// A tick shorter than a round of reads is counted, named and surfaced.
///
/// This family loses data in one way and it is not a ring overflowing.
/// Everything here polls a filesystem, and a poll asked for a tick shorter
/// than one round of reads takes cannot deliver the series it promises: the
/// spacing quietly becomes whatever the reads cost, and a series whose points
/// are not where it says they are is a wrong number that looks right.
///
/// So the scenario chooses the failure. `power.cstate` reads every idle state
/// of every core, which on a twenty-four core machine is a few milliseconds a
/// round; asked for a millisecond it cannot keep up. The check is a contrast,
/// because a collector that reported loss unconditionally would pass half of
/// it: at a tick it *can* keep the same session has to report none.
#[test]
fn a_tick_shorter_than_a_round_of_reads_is_counted_and_surfaced() {
    if !usable("power.cstate") {
        eprintln!("skipped: no backend of `power.cstate` is usable here");
        return;
    }
    let comfortable = losses_of("power.cstate", "poll@200ms");
    assert_eq!(
        comfortable["late_ticks"].as_u64(),
        Some(0),
        "a 200 ms tick is a pace this collector keeps, so nothing is late"
    );

    let hurried = run(&["-f", &example("power-late-ticks.yml").to_string_lossy()]);
    assert!(hurried.status.success(), "{}", stderr(&hurried));
    let out = stdout(&hurried);

    let structured = losses_of("power.cstate", "poll@1ms");
    let late = structured["late_ticks"].as_u64().unwrap_or(0);
    if late == 0 {
        eprintln!(
            "skipped the contrast: this machine reads every idle state of every core inside one \\
             millisecond, so a 1 ms tick is a pace it keeps. Lower the tick until it cannot, and \\
             the loss must appear at that point and not before"
        );
        return;
    }

    // Counted, and reaching the text a person reads -- not only the JSON.
    assert!(
        out.contains("LOSS:") && out.contains("late tick"),
        "a run that could not keep its own pace has to say so where it is read: {out}"
    );
    // One line for all of them, naming the tick and carrying the count. The
    // duration used to be in the reason itself, so a run that was late two
    // hundred times printed two hundred lines and aggregated none of them.
    assert!(
        out.contains("outran the 1.0ms tick"),
        "and has to name the tick it overran, not just that something was late: {out}"
    );
    assert_eq!(
        out.matches("outran the 1.0ms tick").count(),
        1,
        "the late ticks have to share one line rather than get one each: {out}"
    );
    // The count is that run's own -- the structured figures above come from a
    // second run of the same session, and two runs of a hurried poll are not
    // late the same number of times -- so what is asserted is that the one line
    // carries a count, and that the count is the many rather than one.
    let counted: u64 = out
        .lines()
        .find_map(|line| {
            let (count, _) = line.trim().split_once("x a round of reads outran")?;
            count.parse().ok()
        })
        .unwrap_or_else(|| panic!("the reason line has to carry the count it explains: {out}"));
    assert!(
        counted > 1,
        "a run late {late} times printed a reason counted {counted} time(s), which is the \
         transcript this reason was collapsed to stop being: {out}"
    );
    // Every round was late, not a sample of them: the tick was chosen shorter
    // than a round, so there is no round that could have been on time.
    let events = structured["events"].as_u64().unwrap_or(0);
    let rounds = events / cores().max(1) as u64;
    assert!(
        late + 1 >= rounds,
        "a tick this collector can never keep loses every round: {late} late out of about \\
         {rounds}"
    );
    assert!(
        structured["dropped"].as_u64() == Some(0),
        "nothing was dropped here: what was lost is the spacing, and calling it a dropped \\
         event would send the reader looking for a buffer"
    );
}

/// The losses of one run, and how many events it saw, out of the JSON.
fn losses_of(source: &str, mode: &str) -> serde_json::Value {
    let measured = run(&[
        source, "--system", "-m", mode, "-e", "summary", "-d", "500ms", "--format", "json",
    ]);
    assert!(measured.status.success(), "{}", stderr(&measured));
    let document: serde_json::Value =
        serde_json::from_str(&stdout(&measured)).expect("the run emits JSON");
    let mut losses = document["results"][0]["losses"].clone();
    losses["events"] = document["results"][0]["events_seen"].clone();
    losses
}

/// A run that costs more than its budget stops, and says which ceiling it
/// crossed and what it spent.
///
/// The other half of "say what you did not see": a measurement cut short is a
/// window that does not cover what it claims to. `power.cstate` at a
/// millisecond tick spends a third of a core on itself, so a one percent
/// ceiling is crossed within a tick or two, and the run has to end naming the
/// ceiling, the cost and the fact that nothing cheaper exists to fall back to.
#[test]
fn a_run_that_costs_more_than_its_budget_stops_and_says_which_ceiling() {
    if !usable("power.cstate") {
        eprintln!("skipped: no backend of `power.cstate` is usable here");
        return;
    }
    let measured = run(&[
        "power.cstate",
        "--system",
        "-m",
        "poll@1ms",
        "-e",
        "summary",
        "-b",
        "1",
        "-d",
        "2s",
    ]);
    assert!(measured.status.success(), "{}", stderr(&measured));
    let out = stdout(&measured);
    let notes = stderr(&measured);

    if !out.contains("against a ceiling") {
        eprintln!(
            "skipped: polling every idle state of every core cost less than one percent of this \\
             machine's cpu time, so the ceiling was never crossed and there is nothing to report"
        );
        return;
    }
    assert!(
        out.contains("ceiling of 1 %"),
        "a run stopped by a budget has to name the ceiling it crossed: {out}"
    );
    assert!(
        out.contains("nothing cheaper"),
        "and whether it could have moved to a cheaper backend instead of stopping: {out}"
    );
    assert!(
        notes.contains("--budget 1%"),
        "and the budget has to be explained before the run, not only after it: {notes}"
    );
}

// ---------------------------------------------------------------------------
// Class D -- the refusal: is the reason the right reason?
// ---------------------------------------------------------------------------

/// A request that cannot mean what it asks for is refused, and the message
/// names what it cannot mean.
///
/// Three different impossibilities, and each has a different reader. Energy is
/// a property of a package, so a per-process figure would be an estimate and
/// this tool does not present an estimate as a measurement. A temperature has
/// no transitions to trace. An emit a source does not declare is not a
/// rendering problem. A refusal that sent a reader to the wrong sysctl, the
/// wrong capability or the wrong flag would cost more than no message at all.
#[test]
fn a_request_that_cannot_mean_anything_names_what_it_cannot_mean() {
    let per_process = run(&["power.energy", "--pid", "1", "-m", "poll", "-e", "timeline"]);
    assert_eq!(per_process.status.code(), Some(EXIT_REFUSED));
    let message = stderr(&per_process);
    assert!(
        message.contains("package"),
        "RAPL counts a package, and the refusal has to say so: {message}"
    );

    let traced = run(&["power.temp", "--system", "-m", "trace", "-e", "raw"]);
    assert_eq!(traced.status.code(), Some(EXIT_REFUSED));
    let message = stderr(&traced);
    assert!(
        message.contains("`trace` mode"),
        "the refusal has to name the mode: {message}"
    );
    assert!(
        message.contains("poll") && message.contains("snapshot"),
        "and say which modes this source does have: {message}"
    );

    let unshaped = run(&["power.energy", "--system", "-m", "count", "-e", "raw"]);
    assert_eq!(unshaped.status.code(), Some(EXIT_REFUSED));
    let message = stderr(&unshaped);
    assert!(
        message.contains("cannot emit `raw`") && message.contains("summary, rate, timeline"),
        "the refusal has to say what this source does emit: {message}"
    );

    let unjoinable = run(&[
        "power.energy",
        "--system",
        "-m",
        "count",
        "-e",
        "rate",
        "-k",
        "cpu",
    ]);
    assert_eq!(unjoinable.status.code(), Some(EXIT_REFUSED));
    assert!(
        stderr(&unjoinable).contains("does not carry the `cpu` key"),
        "a key a source cannot carry is a join that cannot happen: {}",
        stderr(&unjoinable)
    );
}

/// The three things the tool says no to are three different answers, and a
/// reader has to be able to tell them apart.
///
/// A request that cannot mean anything, a machine that cannot serve one, and a
/// build that has not implemented one lead to three different next steps:
/// change the request, change the machine, write the collector. This family is
/// the one place all three can be provoked at once, because `power.throttle`
/// is out of reach here in both of the last two ways.
///
/// What it found is in `docs/scenarios/power.md`: the first two share exit 2,
/// so only the message tells them apart. The messages do, which is what is
/// asserted here.
#[test]
fn the_three_refusals_are_told_apart_by_what_they_say() {
    let cannot_mean = run(&["power.energy", "--pid", "1", "-m", "poll", "-e", "timeline"]);
    let not_built = run(&[
        "power.freq",
        "--system",
        "-m",
        "poll@200ms",
        "-e",
        "summary",
        "-d",
        "200ms",
        "--backend",
        "force:bpf:tp",
    ]);

    assert_eq!(cannot_mean.status.code(), Some(EXIT_REFUSED));
    assert_eq!(not_built.status.code(), Some(EXIT_NO_COLLECTOR));
    assert!(
        !stderr(&cannot_mean).contains("no collector"),
        "a request that cannot mean anything is not a missing mechanism: {}",
        stderr(&cannot_mean)
    );
    assert!(
        stderr(&not_built).contains("The request itself is valid"),
        "a missing mechanism has to say the request was fine: {}",
        stderr(&not_built)
    );

    // The third: a machine that cannot serve a valid request. Provoked with a
    // copy of the binary, because copying drops the file capabilities and an
    // unprivileged `omn` is exactly the machine that cannot reach the eBPF
    // route into `power.throttle` -- while the sysfs route is an x86 thermal
    // interface this part does not implement.
    let Some(unprivileged) = without_capabilities() else {
        eprintln!(
            "skipped: the binary could not be copied, so the unprivileged view is out of reach"
        );
        return;
    };
    let cannot_serve = Command::new(&unprivileged)
        .args([
            "power.throttle",
            "--system",
            "-m",
            "poll@200ms",
            "-e",
            "summary",
            "-d",
            "200ms",
        ])
        .output()
        .expect("the copy should run");
    let _ = std::fs::remove_file(&unprivileged);

    if cannot_serve.status.success() {
        eprintln!(
            "skipped: an unprivileged `omn` can serve `power.throttle` here, so this machine \\
             offers no example of a valid request it cannot serve"
        );
        return;
    }
    let message = stderr(&cannot_serve);
    assert!(
        message.contains("no viable backend on this machine"),
        "a machine that cannot serve a valid request has to say it is the machine: {message}"
    );
    assert!(
        message.contains("CAP_BPF") || message.contains("cap_bpf"),
        "and name the privilege the eBPF route wants: {message}"
    );
    assert!(
        message.contains("thermal_throttle"),
        "and the file the sysfs route wanted: {message}"
    );
    assert!(
        message.contains("the request is valid"),
        "and say the request itself was fine, which is what separates it from a rejection: \
         {message}"
    );
    assert!(
        !stdout(&cannot_serve).contains("events over"),
        "and it must not report a window of nothing"
    );
}

/// A copy of the binary, which is the same build without its file
/// capabilities: `std::fs::copy` carries the contents and the mode and not the
/// extended attributes, and a relink drops them for the same reason.
fn without_capabilities() -> Option<PathBuf> {
    let path = std::env::temp_dir().join(format!("omn-unprivileged-{}", std::process::id()));
    std::fs::copy(BINARY, &path).ok()?;
    let mut mode = std::fs::metadata(&path).ok()?.permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut mode, 0o755);
    std::fs::set_permissions(&path, mode).ok()?;
    Some(path)
}
