//! Ground truth for the unwind modes.
//!
//! One C program whose call tree is known because this file wrote it, and
//! whose time splits across its branches in shares known by construction:
//! `spin` is the only function that does work, and each branch calls it for a
//! fixed number of iterations -- parse 20 %, hash 40 %, sort 30 %, checksum
//! 10 % of every batch. The same source is built four ways, on purpose:
//!
//! - `fp`: `-O0 -fno-omit-frame-pointer`, a frame pointer in every function;
//! - `leaf`: `-O1 -fno-omit-frame-pointer -momit-leaf-frame-pointer`, frame
//!   pointers everywhere but in `spin`, as gcc builds leaves from `-O1`;
//! - `nofp`: `-O1 -fomit-frame-pointer`, none at all;
//! - `strip`: `nofp`, stripped of its symbol table.
//!
//! Each mode is held to the tree it should recover from the build it should
//! serve, and to each branch's share of the samples in `spin`, with the
//! binomial resolution of that many samples as the slack. The contrast is held
//! too: the mode that should *not* recover a build is checked to lose exactly
//! the frames it is documented to lose.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use omn::perf::{Callchain, CpuMode, Record, Sample, SampledEvent, Scope, TaskSampler};
use omn::stack::unwind::{Debuginfod, NameOrigin};
use omn::stack::{Unwinder, RECOVERED_MARK};

use perf_event_open_sys as sys;

const BINARY: &str = env!("CARGO_BIN_EXE_omn");
const CPU_CLOCK: u64 = sys::bindings::PERF_COUNT_SW_CPU_CLOCK as u64;

/// The program. `spin` is the leaf every branch ends in, and the iteration
/// counts are the shares. At exit it prints the cpu time its own thread used,
/// from `CLOCK_THREAD_CPUTIME_ID`, which is what the cost is measured against.
const CALLTREE: &str = r#"
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

static volatile uint64_t sink;

__attribute__((noinline)) static void spin(uint64_t n) {
    uint64_t x = 1;
    for (uint64_t i = 0; i < n; i++) x = x * 6364136223846793005ULL + 1442695040888963407ULL;
    sink += x;
}
__attribute__((noinline)) static void parse_record(void) { spin(2000); }
__attribute__((noinline)) static void parse(void) { for (int i = 0; i < 100; i++) parse_record(); }
__attribute__((noinline)) static void hash_block(void) { spin(4000); }
__attribute__((noinline)) static void hash(void) { for (int i = 0; i < 100; i++) hash_block(); }
__attribute__((noinline)) static void compare(void) { spin(1000); }
__attribute__((noinline)) static void sort(void) { for (int i = 0; i < 300; i++) compare(); }
__attribute__((noinline)) static void checksum(void) { spin(100000); }
__attribute__((noinline)) static void compute(void) { hash(); sort(); }
__attribute__((noinline)) static void run_batch(void) { parse(); compute(); checksum(); }

int main(int argc, char **argv) {
    int batches = argc > 1 ? atoi(argv[1]) : 2000;
    for (int b = 0; b < batches; b++) run_batch();
    struct timespec cpu;
    clock_gettime(CLOCK_THREAD_CPUTIME_ID, &cpu);
    fprintf(stderr, "cpu_ns=%lld\n", (long long)cpu.tv_sec * 1000000000LL + cpu.tv_nsec);
    return 0;
}
"#;

/// Each branch, by the function that calls `spin`, with the path from
/// `run_batch` down to it and its share by construction.
const BRANCHES: [(&str, &[&str], f64); 4] = [
    (
        "parse_record",
        &["run_batch", "parse", "parse_record", "spin"],
        0.20,
    ),
    (
        "hash_block",
        &["run_batch", "compute", "hash", "hash_block", "spin"],
        0.40,
    ),
    (
        "compare",
        &["run_batch", "compute", "sort", "compare", "spin"],
        0.30,
    ),
    ("checksum", &["run_batch", "checksum", "spin"], 0.10),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Build {
    Fp,
    Leaf,
    NoFp,
    Strip,
}

impl Build {
    fn name(self) -> &'static str {
        match self {
            Build::Fp => "fp",
            Build::Leaf => "leaf",
            Build::NoFp => "nofp",
            Build::Strip => "strip",
        }
    }

    /// Batches for about a second of work: `-O0` is about three times slower.
    fn batches(self) -> &'static str {
        match self {
            Build::Fp => "400",
            _ => "1000",
        }
    }
}

fn scratch() -> PathBuf {
    let directory = std::env::temp_dir().join(format!("omn-unwind-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    directory
}

/// The four builds, made once per test binary. `None` without a C compiler,
/// which is a machine that cannot run this file, said loudly by the caller.
fn builds() -> Option<&'static BTreeMap<&'static str, PathBuf>> {
    static BUILDS: OnceLock<Option<BTreeMap<&'static str, PathBuf>>> = OnceLock::new();
    BUILDS
        .get_or_init(|| {
            let directory = scratch();
            let source = directory.join("calltree.c");
            std::fs::write(&source, CALLTREE).ok()?;
            let mut built = BTreeMap::new();
            for (build, flags) in [
                (Build::Fp, &["-O0", "-fno-omit-frame-pointer"][..]),
                (
                    Build::Leaf,
                    &[
                        "-O1",
                        "-fno-omit-frame-pointer",
                        "-momit-leaf-frame-pointer",
                    ][..],
                ),
                (Build::NoFp, &["-O1", "-fomit-frame-pointer"][..]),
            ] {
                let binary = directory.join(format!("calltree-{}", build.name()));
                let status = Command::new("cc")
                    .args(flags)
                    .arg("-g")
                    .arg("-o")
                    .arg(&binary)
                    .arg(&source)
                    .status()
                    .ok()?;
                if !status.success() {
                    return None;
                }
                built.insert(build.name(), binary);
            }
            let stripped = directory.join("calltree-strip");
            std::fs::copy(&built["nofp"], &stripped).ok()?;
            let status = Command::new("strip").arg(&stripped).status().ok()?;
            if !status.success() {
                return None;
            }
            built.insert("strip", stripped);
            Some(built)
        })
        .as_ref()
}

fn build(build: Build) -> Option<&'static Path> {
    match builds() {
        Some(builds) => Some(builds[build.name()].as_path()),
        None => {
            eprintln!("skipped: no C compiler (`cc`) or `strip` to build the call tree with");
            None
        }
    }
}

/// One profile, as the binary writes it.
#[derive(Debug)]
struct Profile {
    /// Root first, with their sample counts.
    stacks: Vec<(Vec<String>, u64)>,
    notes: Vec<String>,
    degradations: Vec<String>,
    /// The unwind mode the notes say was used.
    events: u64,
    collector_cpu_ns: u64,
    /// What the target said its own thread used.
    target_cpu_ns: u64,
}

/// Runs `omn cpu.profile` over a build, with a stack mode (`None`: auto).
fn profile(binary: &Path, batches: &str, stack: Option<&str>, hz: u32) -> Result<Profile, String> {
    static RUN: AtomicUsize = AtomicUsize::new(0);
    let out = scratch().join(format!("run-{}", RUN.fetch_add(1, Ordering::SeqCst)));
    let mut command = Command::new(BINARY);
    command.args([
        "cpu.profile",
        "--mode",
        &format!("sample@{hz}hz"),
        "--emit",
        "stack",
        "--format",
        "json",
        "-o",
    ]);
    command.arg(&out);
    if let Some(stack) = stack {
        command.args(["--stack", stack]);
    }
    command.arg("--").arg(binary).arg(batches);
    let output = command
        .stdout(Stdio::null())
        .output()
        .map_err(|error| error.to_string())?;
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success() {
        return Err(stderr);
    }
    let target_cpu_ns = stderr
        .lines()
        .find_map(|line| line.strip_prefix("cpu_ns="))
        .and_then(|value| value.trim().parse().ok())
        .ok_or_else(|| format!("the target did not report its cpu time: {stderr}"))?;
    let results: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(out.join("results.json")).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let result = &results["results"][0];
    let table = &result["stacks"];
    let mut stacks = Vec::new();
    for row in result["outputs"][0]["stacks"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let id = row["stack"].as_u64().expect("a stack id").to_string();
        let frames: Vec<String> = table[&id]
            .as_array()
            .expect("the table holds every stack an output names")
            .iter()
            .map(|frame| frame.as_str().unwrap_or_default().to_owned())
            .collect();
        stacks.push((frames, row["events"].as_u64().unwrap_or(0)));
    }
    let strings = |value: &serde_json::Value| -> Vec<String> {
        value
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| item.as_str().map(str::to_owned))
            .collect()
    };
    let cost: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(out.join("cost.json")).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let cost = if cost.is_array() { &cost[0] } else { &cost };
    Ok(Profile {
        stacks,
        notes: strings(&result["notes"]),
        degradations: strings(&result["degradations"]),
        events: result["events_seen"].as_u64().unwrap_or(0),
        collector_cpu_ns: cost["overhead"]["collector_cpu_ns"].as_u64().unwrap_or(0),
        target_cpu_ns,
    })
}

/// Profiles, or says loudly why this machine cannot.
fn profile_or_skip(build: Build, stack: Option<&str>) -> Option<Profile> {
    let binary = self::build(build)?;
    match profile(binary, build.batches(), stack, 1999) {
        Ok(profile) => Some(profile),
        Err(error) if error.contains("perf_event_paranoid") || error.contains("no viable") => {
            eprintln!("skipped: this machine would not profile ({error})");
            None
        }
        Err(error) => panic!("omn cpu.profile --stack {stack:?} on {build:?} failed: {error}"),
    }
}

/// A frame's name without the mark a recovered frame carries.
fn plain(frame: &str) -> &str {
    frame.strip_suffix(RECOVERED_MARK).unwrap_or(frame)
}

/// Whether `path` appears in `frames`, contiguous and in order.
fn contains_path(frames: &[String], path: &[&str]) -> bool {
    let names: Vec<&str> = frames.iter().map(|frame| plain(frame)).collect();
    names.windows(path.len()).any(|window| window == path)
}

/// The samples whose leaf is `spin`, by the branch whose whole path is in the
/// stack, and those whose path is not whole.
fn branches(profile: &Profile) -> (BTreeMap<&'static str, u64>, u64, u64) {
    let mut by_branch = BTreeMap::new();
    let mut broken = 0;
    let mut in_spin = 0;
    for (frames, count) in &profile.stacks {
        if frames.last().map(|leaf| plain(leaf)) != Some("spin") {
            continue;
        }
        in_spin += count;
        let found = BRANCHES.iter().find(|(_, path, _)| {
            contains_path(frames, path) && contains_path(frames, &["main", "run_batch"])
        });
        match found {
            Some((name, _, _)) => *by_branch.entry(*name).or_default() += count,
            None => broken += count,
        }
    }
    (by_branch, broken, in_spin)
}

/// Every sample in `spin` carries its branch's whole path, and each branch has
/// its constructed share within the resolution of that many samples.
fn assert_the_tree(profile: &Profile, what: &str) {
    let (by_branch, broken, in_spin) = branches(profile);
    assert!(
        in_spin >= 500,
        "{what}: only {in_spin} samples landed in spin: {profile:#?}"
    );
    // A stack sampled in the one or two instructions of a caller between two
    // calls of `spin` is not in `spin`; one in `spin` whose path is not whole
    // is the defect. Allowed: the samples an unwinder may lose to a sample
    // taken mid-prologue, at most 1 %.
    assert!(
        broken * 100 <= in_spin,
        "{what}: {broken} of {in_spin} samples in spin did not carry their branch's whole path: \
         {:#?}",
        profile.stacks
    );
    let whole = (in_spin - broken) as f64;
    for (name, _, share) in BRANCHES {
        let measured = by_branch.get(name).copied().unwrap_or(0) as f64 / whole;
        // Four standard deviations of a binomial share over this many samples,
        // and two points for what construction does not control: the
        // callers' own loops, and `spin`'s entry and exit.
        let slack = 4.0 * (share * (1.0 - share) / whole).sqrt() + 0.02;
        assert!(
            (measured - share).abs() <= slack,
            "{what}: {name} took {:.1} % of the samples in spin, built to take {:.0} % (slack \
             {:.1} points over {whole} samples): {by_branch:?}",
            measured * 100.0,
            share * 100.0,
            slack * 100.0
        );
    }
    eprintln!("{what}: {by_branch:?} of {in_spin} samples in spin, {broken} broken");
}

// ---------------------------------------------------------------------------
// fp

#[test]
fn fp_recovers_the_whole_tree_of_a_build_with_frame_pointers_everywhere() {
    let Some(profile) = profile_or_skip(Build::Fp, Some("fp")) else {
        return;
    };
    assert_the_tree(&profile, "fp over the fp build");
}

/// Under `fp`, the leaf's caller is exactly what is lost from the `leaf`
/// build: `spin` sets up no frame, so the walk goes from `spin` to its
/// caller's caller. The note names the object and the modes that recover it.
#[test]
fn fp_loses_the_caller_of_a_frameless_leaf_and_names_the_object_it_is_in() {
    let Some(profile) = profile_or_skip(Build::Leaf, Some("fp")) else {
        return;
    };
    let mut skipped = 0;
    let mut in_spin = 0;
    for (frames, count) in &profile.stacks {
        if frames.last().map(String::as_str) != Some("spin") {
            continue;
        }
        in_spin += count;
        let parent = &frames[frames.len() - 2];
        if ["parse", "hash", "sort", "run_batch"].contains(&parent.as_str()) {
            skipped += count;
        }
    }
    assert!(in_spin >= 500, "{profile:#?}");
    assert!(
        skipped * 100 >= in_spin * 95,
        "only {skipped} of {in_spin} samples in spin lost exactly the leaf's caller"
    );
    let note = profile
        .degradations
        .iter()
        .chain(&profile.notes)
        .find(|note| note.contains("sets up no frame pointer"))
        .unwrap_or_else(|| panic!("no frame-pointer note: {profile:#?}"));
    assert!(note.contains("calltree-leaf"), "{note}");
    assert!(note.contains("fp+leaf"), "{note}");
    assert!(
        note.contains("keeps frame pointers") && !note.contains("built without them"),
        "the leaf build keeps frame pointers and its note must not say otherwise: {note}"
    );
}

// ---------------------------------------------------------------------------
// fp+leaf

#[test]
fn fp_leaf_recovers_the_caller_of_a_frameless_leaf_and_marks_it() {
    let Some(profile) = profile_or_skip(Build::Leaf, Some("fp+leaf")) else {
        return;
    };
    assert_the_tree(&profile, "fp+leaf over the leaf build");
    // The recovered frame is the one the kernel skipped: `spin`'s caller.
    for (frames, count) in &profile.stacks {
        if frames.last().map(String::as_str) == Some("spin") && *count > 2 {
            let caller = &frames[frames.len() - 2];
            assert!(
                caller.ends_with(RECOVERED_MARK),
                "spin's caller was not marked recovered: {frames:?}"
            );
        }
        let marked = frames
            .iter()
            .filter(|frame| frame.ends_with(RECOVERED_MARK))
            .count();
        assert!(marked <= 1, "more than one frame recovered in {frames:?}");
    }
    assert!(
        profile
            .notes
            .iter()
            .any(|note| note.contains("`fp+leaf` recovered the caller")),
        "{:?}",
        profile.notes
    );
}

// ---------------------------------------------------------------------------
// dwarf

#[test]
fn dwarf_recovers_the_whole_tree_of_a_build_without_frame_pointers() {
    let Some(profile) = profile_or_skip(Build::NoFp, Some("dwarf")) else {
        return;
    };
    assert_the_tree(&profile, "dwarf over the nofp build");
    assert!(
        !profile
            .stacks
            .iter()
            .flat_map(|(frames, _)| frames)
            .any(|frame| frame.ends_with(RECOVERED_MARK)),
        "dwarf walks every frame; none is inferred"
    );
}

/// The contrast: frame pointers over the same build come back short. Not a
/// profile of the tree, and said to be one of leaf functions.
#[test]
fn fp_over_a_build_without_frame_pointers_says_it_is_a_profile_of_leaves() {
    let Some(profile) = profile_or_skip(Build::NoFp, Some("fp")) else {
        return;
    };
    let (by_branch, broken, in_spin) = branches(&profile);
    assert!(
        broken * 2 > in_spin,
        "frame pointers recovered {by_branch:?} of {in_spin} from a build that has none"
    );
    assert!(
        profile
            .degradations
            .iter()
            .any(|note| note.contains("built without them")),
        "{:?}",
        profile.degradations
    );
}

// ---------------------------------------------------------------------------
// auto

/// `auto` takes the best mode this machine and *this target* serve: the
/// cheap one for a build with frame pointers, the copied stack for one
/// without, and says what it passed over.
#[test]
fn auto_chooses_by_how_the_target_was_built_and_says_why() {
    for (build, expected) in [
        (Build::Leaf, "fp+leaf"),
        (Build::Fp, "fp+leaf"),
        (Build::NoFp, "dwarf"),
        (Build::Strip, "dwarf"),
    ] {
        let Some(binary) = self::build(build) else {
            return;
        };
        let profile = match profile(binary, "100", None, 999) {
            Ok(profile) => profile,
            Err(error) => {
                eprintln!("skipped: this machine would not profile ({error})");
                return;
            }
        };
        let chosen = profile
            .notes
            .iter()
            .find(|note| note.starts_with("stacks unwound by"))
            .unwrap_or_else(|| panic!("{build:?}: no note says how: {profile:#?}"));
        assert!(
            chosen.starts_with(&format!("stacks unwound by `{expected}`")),
            "{build:?}: {chosen}"
        );
        assert!(
            chosen.contains("chosen by `--stack auto` over sframe"),
            "{chosen}"
        );
        if expected == "dwarf" {
            assert!(
                chosen.contains("fp+leaf (target:frame_pointers: missing"),
                "{build:?}: {chosen}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// lbr, sframe: declared, probed, refused by name.

#[test]
fn a_mode_this_machine_cannot_serve_is_refused_with_the_requirement_it_lacks() {
    let Some(binary) = build(Build::Leaf) else {
        return;
    };
    for (mode, requirement) in [
        ("lbr", "pmu:branch_call_stack"),
        ("sframe", "kernel:sframe"),
    ] {
        let output = Command::new(BINARY)
            .args(["cpu.profile", "--mode", "sample@99hz", "--emit", "stack"])
            .args(["--stack", mode, "--"])
            .arg(binary)
            .arg("1")
            .output()
            .expect("omn runs");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "--stack {mode} was served: {stderr}"
        );
        if stderr.contains("not implemented") {
            // A machine that offers it: the probe said yes and the refusal is
            // that this build has no collector for it, which is also by name.
            eprintln!("--stack {mode}: this machine offers it: {stderr}");
            continue;
        }
        assert!(
            stderr.contains(&format!("--stack {mode}")) && stderr.contains(requirement),
            "--stack {mode} was refused without naming {requirement}: {stderr}"
        );
    }
}

// ---------------------------------------------------------------------------
// The cost each mode declares.

/// What the kernel copies per sample is what each mode declares, read from
/// the samples themselves; and unwinding a copied stack here costs this
/// process more per sample than frame pointers do -- two runs over the same
/// work, differing in the mode, and separating.
#[test]
fn each_mode_copies_what_it_declares_and_dwarf_costs_more_per_sample_than_fp() {
    let Some(binary) = build(Build::NoFp) else {
        return;
    };
    for (callchain, declared) in [
        (Callchain::FramePointersAndLeaf { kernel: false }, 256usize),
        (Callchain::CopiedStack { kernel: false }, 8192usize),
    ] {
        let Some((samples, stack_top)) = sample_launched(binary, callchain, 300) else {
            return;
        };
        let copied: Vec<usize> = samples
            .iter()
            .filter_map(|sample| sample.user_stack.as_ref().map(Vec::len))
            .collect();
        assert!(
            copied.len() >= 50,
            "{callchain:?}: {} samples",
            copied.len()
        );
        assert_eq!(
            omn::stack::unwind::entry(match callchain {
                Callchain::CopiedStack { .. } => omn::request::StackMode::Dwarf,
                _ => omn::request::StackMode::FpLeaf,
            })
            .unwrap()
            .copies as usize,
            declared
        );
        // The kernel copies what was asked for, or the whole stack when it is
        // shallower: from the stack pointer to the top of the `[stack]`
        // mapping, which is read from the target's maps and not from the tool.
        let top = stack_top.expect("the target's stack was read while it ran");
        let expected = |sample: &Sample| {
            let sp = sample.user_regs.unwrap().sp;
            (top.saturating_sub(sp) as usize).min(declared)
        };
        let exact = samples
            .iter()
            .filter(|sample| sample.user_stack.as_ref().map(Vec::len) == Some(expected(sample)))
            .count();
        let mismatched: Vec<(usize, usize)> = samples
            .iter()
            .filter_map(|sample| {
                let got = sample.user_stack.as_ref()?.len();
                (got != expected(sample)).then(|| (expected(sample), got))
            })
            .take(10)
            .collect();
        assert!(
            exact * 10 >= copied.len() * 9,
            "{callchain:?}: {exact} of {} samples carried min({declared}, stack top - sp) bytes; \
             (expected, copied) of the first that did not: {mismatched:?}",
            copied.len(),
        );
        assert!(samples.iter().all(|sample| sample.user_regs.is_some()));
    }

    // Two runs of each, alternated so that the machine's load falls on both:
    // the modes separate when the cheaper dwarf run still cost more per
    // sample than the dearer fp run.
    let mut fp = Vec::new();
    let mut dwarf = Vec::new();
    for _ in 0..2 {
        let (Some(one), Some(other)) = (
            profile_or_skip(Build::NoFp, Some("fp")),
            profile_or_skip(Build::NoFp, Some("dwarf")),
        ) else {
            return;
        };
        fp.push(one);
        dwarf.push(other);
    }
    let per_sample =
        |profile: &Profile| profile.collector_cpu_ns as f64 / profile.events.max(1) as f64 / 1e3;
    let describe = |runs: &[Profile]| -> String {
        runs.iter()
            .map(|run| {
                format!(
                    "{:.1} us a sample over {} (target thread {:.0} ms)",
                    per_sample(run),
                    run.events,
                    run.target_cpu_ns as f64 / 1e6
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    eprintln!("fp: {}; dwarf: {}", describe(&fp), describe(&dwarf));
    assert!(fp.iter().chain(&dwarf).all(|run| run.events > 500));
    let dearest_fp = fp.iter().map(per_sample).fold(0.0, f64::max);
    let cheapest_dwarf = dwarf.iter().map(per_sample).fold(f64::MAX, f64::min);
    assert!(
        cheapest_dwarf > dearest_fp,
        "declared high and low, dwarf and fp do not separate: fp {}; dwarf {}",
        describe(&fp),
        describe(&dwarf)
    );
}

/// Launches `binary`, samples it with `callchain` from its first instruction
/// until it exits, and hands back the samples.
fn sample_launched(
    binary: &Path,
    callchain: Callchain,
    batches: u32,
) -> Option<(Vec<Sample>, Option<u64>)> {
    // A stopped child: sampled from its exec, the way the tool's own launch
    // gate does it, without the gate.
    let mut child = Command::new(binary)
        .arg(batches.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the fixture was just built");
    let pid = child.id() as i32;
    let spec = omn::perf::SampleSpec::software(CPU_CLOCK, 1_000_000, callchain);
    let mut event = match SampledEvent::open(&spec, Scope::Process(pid), 512) {
        Ok(event) => event,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            eprintln!("skipped: cannot sample with {callchain:?}: {error}");
            return None;
        }
    };
    event.enable().expect("enable");
    let mut samples = Vec::new();
    let mut stack_top = None;
    loop {
        stack_top = stack_top.or_else(|| stack_top_of(pid));
        event.drain(|record| {
            if let Record::Sample(sample) = record {
                samples.push(sample);
            }
        });
        if child.try_wait().expect("wait").is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    event.drain(|record| {
        if let Record::Sample(sample) = record {
            samples.push(sample);
        }
    });
    Some((samples, stack_top))
}

/// The end of a process's `[stack]` mapping.
fn stack_top_of(pid: i32) -> Option<u64> {
    let maps = std::fs::read_to_string(format!("/proc/{pid}/maps")).ok()?;
    let line = maps.lines().find(|line| line.ends_with("[stack]"))?;
    let range = line.split_whitespace().next()?;
    u64::from_str_radix(range.split_once('-')?.1, 16).ok()
}

// ---------------------------------------------------------------------------
// The kernel's default paranoia: user-only stacks.

/// A program that spends most of its time in the kernel -- one-byte writes to
/// `/dev/null` -- sampled with both halves and with the user half only. The
/// second must carry no kernel frame at all, and must have been delivered far
/// fewer samples for the same work: the time in system calls is what
/// `exclude_kernel` leaves out, and what the choice says it leaves out.
#[test]
fn user_only_stacks_carry_no_kernel_frame_and_leave_the_system_calls_out() {
    let directory = scratch();
    let source = directory.join("writes.c");
    let binary = directory.join("writes");
    std::fs::write(
        &source,
        r#"
#include <fcntl.h>
#include <stdlib.h>
#include <unistd.h>
int main(int argc, char **argv) {
    long n = argc > 1 ? atol(argv[1]) : 300000;
    int fd = open("/dev/null", O_WRONLY);
    char byte = 0;
    for (long i = 0; i < n; i++) write(fd, &byte, 1);
    return 0;
}
"#,
    )
    .unwrap();
    if !Command::new("cc")
        .args(["-O1", "-fno-omit-frame-pointer", "-o"])
        .arg(&binary)
        .arg(&source)
        .status()
        .is_ok_and(|status| status.success())
    {
        eprintln!("skipped: no C compiler");
        return;
    }
    let count = |callchain| {
        let samples = sample_launched_args(&binary, callchain, "600000")?;
        let kernel_frames: usize = samples
            .iter()
            .filter_map(omn::stack::RawStack::of)
            .map(|raw| raw.kernel.len())
            .sum();
        let in_kernel = samples
            .iter()
            .filter(|sample| sample.cpu_mode == CpuMode::Kernel)
            .count();
        Some((samples.len(), kernel_frames, in_kernel))
    };
    let Some((user_samples, user_kernel_frames, user_in_kernel)) =
        count(Callchain::FramePointers { kernel: false })
    else {
        return;
    };
    assert_eq!(
        user_kernel_frames, 0,
        "a user-only stack carried kernel frames"
    );
    assert_eq!(
        user_in_kernel, 0,
        "a user-only sample was taken in the kernel"
    );
    if !omn::perf::kernel_callchain_allowed() {
        eprintln!(
            "this machine refuses the kernel half, so the comparison with both halves is the \
             CI's: {user_samples} user-only samples"
        );
        return;
    }
    let (both_samples, both_kernel_frames, both_in_kernel) =
        count(Callchain::FramePointers { kernel: true }).expect("both halves open here");
    eprintln!(
        "both halves: {both_samples} samples, {both_in_kernel} in the kernel, {both_kernel_frames} \
         kernel frames; user half: {user_samples} samples"
    );
    assert!(
        both_in_kernel * 2 > both_samples,
        "the workload was meant to live in the kernel"
    );
    assert!(
        user_samples * 2 < both_samples,
        "user-only took {user_samples} samples of the same work both halves took {both_samples} of"
    );
}

fn sample_launched_args(binary: &Path, callchain: Callchain, arg: &str) -> Option<Vec<Sample>> {
    let mut child = Command::new(binary)
        .arg(arg)
        .spawn()
        .expect("the fixture was just built");
    let pid = child.id() as i32;
    let spec = omn::perf::SampleSpec::software(CPU_CLOCK, 100_000, callchain);
    let mut event = match SampledEvent::open(&spec, Scope::Process(pid), 512) {
        Ok(event) => event,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            eprintln!("skipped: cannot sample with {callchain:?}: {error}");
            return None;
        }
    };
    event.enable().expect("enable");
    let mut samples = Vec::new();
    loop {
        event.drain(|record| {
            if let Record::Sample(sample) = record {
                samples.push(sample);
            }
        });
        if child.try_wait().expect("wait").is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    event.drain(|record| {
        if let Record::Sample(sample) = record {
            samples.push(sample);
        }
    });
    Some(samples)
}

/// The resolver's own words, at the kernel's default: where the machine
/// refuses the kernel half, the choice is user-only and says so. On a machine
/// that allows it, the choice takes both halves and says nothing.
#[test]
fn the_choice_says_user_only_exactly_when_the_machine_refuses_the_kernel() {
    let Some(binary) = build(Build::Leaf) else {
        return;
    };
    let Ok(profile) = profile(binary, "50", None, 999) else {
        return;
    };
    let said = profile
        .degradations
        .iter()
        .chain(&profile.notes)
        .any(|note| note.contains("stacks are user-only"));
    assert_eq!(
        said,
        !omn::perf::kernel_callchain_allowed(),
        "{:?} {:?}",
        profile.degradations,
        profile.notes
    );
}

// ---------------------------------------------------------------------------
// The profile's drops, per ring.

/// Two threads, one burning its cpu and one blocked: sampled at 20 kHz into
/// the smallest ring and not drained for half a second, the burning thread's
/// ring overflows and the blocked one's cannot, since it takes no samples. So
/// every drop is the burner's, and the per-ring account says exactly that.
#[test]
fn a_ring_that_overflows_is_charged_to_the_task_it_belongs_to_and_no_other() {
    let directory = scratch();
    let source = directory.join("two.c");
    let binary = directory.join("two");
    std::fs::write(
        &source,
        r#"
#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>
#include <stdint.h>
#include <unistd.h>
#include <sys/syscall.h>
static volatile uint64_t sink;
static void *burn(void *arg) {
    printf("%ld\n", (long)syscall(SYS_gettid));
    fflush(stdout);
    for (uint64_t i = 0; i < 4000000000ULL; i++) sink += i;
    return arg;
}
int main(void) {
    pthread_t thread;
    pthread_create(&thread, 0, burn, 0);
    pthread_join(thread, 0);
    return 0;
}
"#,
    )
    .unwrap();
    if !Command::new("cc")
        .args(["-O1", "-pthread", "-o"])
        .arg(&binary)
        .arg(&source)
        .status()
        .is_ok_and(|status| status.success())
    {
        eprintln!("skipped: no C compiler");
        return;
    }
    let mut child = Command::new(&binary)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let burner: i32 = line.trim().parse().expect("the burner says its tid");
    let pid = child.id() as i32;
    let spec = omn::perf::SampleSpec::software(CPU_CLOCK, 50_000, Callchain::BOTH_HALVES);
    let mut sampler = match TaskSampler::open(&spec, pid, true, 8, 8) {
        Ok(sampler) => sampler,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            eprintln!("skipped: {error}");
            return;
        }
    };
    sampler.enable().unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let mut lost_records = 0;
    let mut by_task: BTreeMap<u32, u64> = BTreeMap::new();
    // The kernel writes its `PERF_RECORD_LOST` into the first room the ring
    // has after the overflow, so the count is in the drain after the one that
    // made room.
    for _ in 0..2 {
        sampler.drain(|record| match record {
            Record::Lost { count } => lost_records += count,
            Record::Sample(sample) => *by_task.entry(sample.tid).or_default() += 1,
            _ => {}
        });
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    let per_ring = sampler.lost_by_task();
    eprintln!("samples by task {by_task:?}, lost {lost_records}, per ring {per_ring:?}");
    assert!(
        lost_records > 0,
        "an 8-page ring at 20 kHz undrained for 500 ms lost nothing"
    );
    let charged: BTreeSet<i32> = per_ring.iter().map(|(tid, _)| *tid).collect();
    assert_eq!(charged, BTreeSet::from([burner]), "{per_ring:?}");
    assert_eq!(
        per_ring.iter().map(|(_, lost)| lost).sum::<u64>(),
        lost_records,
        "the rings' parts do not add up to the whole"
    );
}

// ---------------------------------------------------------------------------
// Names, from debuginfod.

/// A debuginfod server for one file, on loopback, counting what it is asked.
///
/// The protocol is the real one -- `GET /buildid/<hex>/debuginfo` -- and the
/// file it serves is the unstripped build, whose build id the stripped one
/// shares; what is under test is the client, not a server.
/// Answers with the server's url, the requests for the served build id, and
/// every request.
fn serve(debuginfo: PathBuf) -> (String, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let url = format!("http://{}", listener.local_addr().unwrap());
    let asked = Arc::new(AtomicUsize::new(0));
    let all = Arc::new(AtomicUsize::new(0));
    let (counter, every) = (Arc::clone(&asked), Arc::clone(&all));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            every.fetch_add(1, Ordering::SeqCst);
            let mut request = [0u8; 2048];
            let read = stream.read(&mut request).unwrap_or(0);
            let request = String::from_utf8_lossy(&request[..read]);
            let path = request.split_whitespace().nth(1).unwrap_or("").to_owned();
            let wanted = build_id_of(&debuginfo);
            if path == format!("/buildid/{wanted}/debuginfo") {
                counter.fetch_add(1, Ordering::SeqCst);
                let body = std::fs::read(&debuginfo).unwrap();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(&body);
            } else {
                let _ = write!(
                    stream,
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
            }
        }
    });
    (url, asked, all)
}

/// A file's build id, as `readelf -n` prints it.
fn build_id_of(path: &Path) -> String {
    let output = Command::new("readelf")
        .arg("-n")
        .arg(path)
        .output()
        .expect("readelf");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.trim().strip_prefix("Build ID: ").map(str::to_owned))
        .expect("the build carries a build id")
}

/// Stacks root first, and where each module's names came from.
type NamedStacks = (Vec<Vec<String>>, BTreeMap<String, NameOrigin>);

/// Unwinds the samples of one run of `binary` and names them, with or without
/// a debuginfod client: the labels root first, and where each module's names
/// came from.
fn named_stacks(binary: &Path, debuginfod: Option<Arc<Debuginfod>>) -> Option<NamedStacks> {
    let callchain = Callchain::CopiedStack { kernel: false };
    let mut child = Command::new(binary)
        .arg("300")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id() as i32;
    let spec = omn::perf::SampleSpec::software(CPU_CLOCK, 1_000_000, callchain);
    let mut event = match SampledEvent::open(&spec, Scope::Process(pid), 512) {
        Ok(event) => event,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            eprintln!("skipped: {error}");
            return None;
        }
    };
    event.enable().unwrap();
    let mut unwinder = Unwinder::new(pid, callchain, debuginfod);
    let mut ids = Vec::new();
    loop {
        // Unwound while the target is alive, which is when its maps can be read.
        unwinder.refresh_symbols();
        event.drain(|record| {
            if let Record::Sample(sample) = record {
                ids.extend(unwinder.record(&sample));
            }
        });
        if child.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let stacks = ids
        .iter()
        .filter_map(|id| unwinder.table().get(*id))
        .map(|stack| {
            stack
                .frames
                .iter()
                .rev()
                .map(|frame| frame.label())
                .collect()
        })
        .collect();
    let origins = unwinder
        .name_origins()
        .iter()
        .map(|(path, origin)| {
            (
                path.file_name().unwrap().to_string_lossy().into_owned(),
                origin.clone(),
            )
        })
        .collect();
    Some((stacks, origins))
}

#[test]
fn a_stripped_build_is_named_from_debuginfod_only_when_asked_and_as_the_unstripped_one_is() {
    let (Some(stripped), Some(unstripped)) = (build(Build::Strip), build(Build::NoFp)) else {
        return;
    };
    assert_eq!(build_id_of(stripped), build_id_of(unstripped));
    // `asked` counts the requests for the call tree's build id; `all` every
    // request, which includes the one for a stripped libc this server does
    // not have.
    let (url, asked, all) = serve(unstripped.to_path_buf());
    let cache = scratch().join("debuginfod-cache");
    let _ = std::fs::remove_dir_all(&cache);

    // Not asked: nothing is fetched, and the frames are offsets into the file.
    let Some((local, origins)) = named_stacks(stripped, None) else {
        return;
    };
    assert_eq!(
        all.load(Ordering::SeqCst),
        0,
        "a server was asked without --symbols debuginfod"
    );
    assert!(
        !local.iter().flatten().any(|frame| frame == "spin"),
        "a stripped build named its static functions from nowhere"
    );
    assert!(local
        .iter()
        .flatten()
        .any(|frame| frame.starts_with("calltree-strip+0x")));
    assert!(
        matches!(origins.get("calltree-strip"), Some(NameOrigin::None(_))),
        "{origins:?}"
    );

    // Asked: the tree comes back in the unstripped build's names.
    let client = Arc::new(Debuginfod::new(vec![url.clone()], cache.clone()));
    let (named, origins) = named_stacks(stripped, Some(Arc::clone(&client))).unwrap();
    assert_eq!(asked.load(Ordering::SeqCst), 1, "one build id, one request");
    match origins.get("calltree-strip") {
        Some(NameOrigin::Debuginfod {
            server: Some(server),
            ..
        }) => assert_eq!(server, &url),
        other => panic!("the stripped build's names came from {other:?}"),
    }
    let (reference, _) = named_stacks(unstripped, None).unwrap();
    let paths = |stacks: &[Vec<String>]| -> BTreeSet<Vec<String>> {
        stacks
            .iter()
            .filter(|frames| frames.last().map(String::as_str) == Some("spin"))
            .map(|frames| {
                let from = frames.iter().position(|frame| frame == "main").unwrap_or(0);
                frames[from..].to_vec()
            })
            .collect()
    };
    let (named_paths, reference_paths) = (paths(&named), paths(&reference));
    assert!(named_paths.len() >= 4, "{named_paths:?}");
    assert_eq!(
        named_paths, reference_paths,
        "the stripped build, named from debuginfod, does not read as the unstripped one"
    );

    // Asked again: the cache answers, and the server is not -- not for the
    // file it had, and not for the one it did not (the stripped libc).
    let before = all.load(Ordering::SeqCst);
    let (_, origins) = named_stacks(stripped, Some(client)).unwrap();
    assert_eq!(
        all.load(Ordering::SeqCst),
        before,
        "a build id no server had was asked again"
    );
    assert_eq!(
        asked.load(Ordering::SeqCst),
        1,
        "a cached debug file was fetched again"
    );
    assert!(
        matches!(
            origins.get("calltree-strip"),
            Some(NameOrigin::Debuginfod { server: None, .. })
        ),
        "{origins:?}"
    );
}
