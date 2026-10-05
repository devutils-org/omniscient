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
///
/// Between two batches, `pad` -- outside `spin`, so in no branch -- runs a
/// pseudo-random number of the same iterations, uniform below `PAD_MASK + 1`
/// (2^19, about half a millisecond at `-O1`: the 500 us period the share
/// tests sample at). A batch is a fixed million iterations, so without the
/// pad a period near a multiple of the batch samples the same phase of every
/// batch and puts the samples in one branch; with it the phase each sample
/// lands at is spread over a whole period, whatever the period. About a
/// quarter more work per batch, which the batch counts below take back.
/// `-DPAD_MASK=0` builds the program without it.
const CALLTREE: &str = r#"
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

#ifndef PAD_MASK
#define PAD_MASK 0x7ffffULL
#endif

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
__attribute__((noinline)) static void pad(uint64_t n) {
    uint64_t x = 3;
    for (uint64_t i = 0; i < n; i++) x = x * 6364136223846793005ULL + 1442695040888963407ULL;
    sink += x;
}

int main(int argc, char **argv) {
    int batches = argc > 1 ? atoi(argv[1]) : 2000;
    uint64_t state = 0x9e3779b97f4a7c15ULL;
    for (int b = 0; b < batches; b++) {
        run_batch();
        state ^= state << 13; state ^= state >> 7; state ^= state << 17;
        pad(state & PAD_MASK);
    }
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

    /// Batches for about a second of work: `-O0` is about three times slower,
    /// and the pad between batches is about a quarter of one.
    fn batches(self) -> &'static str {
        match self {
            Build::Fp => "320",
            _ => "800",
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
    /// Everything the tool says it spent, by its own account: before the
    /// window, in it, and after it (`Overhead::tool_cpu_ns`).
    reported_tool_ns: u64,
    /// What the target said its own thread used.
    target_cpu_ns: u64,
    /// What `omn` and the program it launched used between them, by the
    /// kernel's account (`wait4`), in microseconds' resolution.
    tree_cpu_ns: u64,
}

impl Profile {
    /// What the tool cost, measured outside it: the kernel's account of the
    /// whole run less what the program says its own thread used.
    fn tool_cpu_ns(&self) -> u64 {
        self.tree_cpu_ns.saturating_sub(self.target_cpu_ns)
    }
}

/// Runs `omn cpu.profile` over a build, with a stack mode (`None`: auto).
fn profile(binary: &Path, batches: &str, stack: Option<&str>, hz: u32) -> Result<Profile, String> {
    profile_emitting(binary, batches, stack, hz, true)
}

/// [`profile`], emitting stacks or, for `--stack none` which refuses them,
/// the hottest functions.
fn profile_emitting(
    binary: &Path,
    batches: &str,
    stack: Option<&str>,
    hz: u32,
    emit_stack: bool,
) -> Result<Profile, String> {
    static RUN: AtomicUsize = AtomicUsize::new(0);
    let out = scratch().join(format!("run-{}", RUN.fetch_add(1, Ordering::SeqCst)));
    let mut command = Command::new(BINARY);
    command.args(["cpu.profile", "--mode", &format!("sample@{hz}hz")]);
    command.args(["--emit", if emit_stack { "stack" } else { "topk" }]);
    command.args(["--format", "json", "-o"]);
    command.arg(&out);
    if let Some(stack) = stack {
        command.args(["--stack", stack]);
    }
    command.arg("--").arg(binary).arg(batches);
    // Waited for with `wait4` rather than `output`, for the kernel's account
    // of what `omn` and the program it launched used between them: the cost
    // the tool reports about itself is held against this, not against itself.
    let mut child = command
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("stderr was piped")
        .read_to_string(&mut stderr)
        .map_err(|error| error.to_string())?;
    let mut status = 0;
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::wait4(child.id() as i32, &mut status, 0, &mut usage) } < 0 {
        return Err(format!("wait4: {}", std::io::Error::last_os_error()));
    }
    if !(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0) {
        return Err(stderr);
    }
    let micros = |time: libc::timeval| time.tv_sec as u64 * 1_000_000 + time.tv_usec as u64;
    let tree_cpu_ns = (micros(usage.ru_utime) + micros(usage.ru_stime)) * 1_000;
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
        reported_tool_ns: [
            "before_window_cpu_ns",
            "collector_cpu_ns",
            "after_window_cpu_ns",
        ]
        .iter()
        .map(|field| cost["overhead"][field].as_u64().unwrap_or(0))
        .sum(),
        target_cpu_ns,
        tree_cpu_ns,
    })
}

/// `perf_event_paranoid is N`, for a skip line to name the machine's reason.
fn paranoid_said() -> String {
    match std::fs::read_to_string("/proc/sys/kernel/perf_event_paranoid") {
        Ok(text) => format!("perf_event_paranoid is {}", text.trim()),
        Err(error) => format!("perf_event_paranoid is unreadable ({error})"),
    }
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

/// A program whose every sample is deeper than the copy, by construction:
/// `spin` runs at the bottom of `descend` recursed `argv[1]` times, each level
/// holding a kilobyte of its own, so from `spin` to `main` is `argv[1]`
/// kilobytes and more of stack.
const DEEP: &str = r#"
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

static volatile uint64_t sink;

__attribute__((noinline)) static void spin(void) {
    uint64_t x = 1;
    for (uint64_t i = 0; i < 400000000ULL; i++) x = x * 6364136223846793005ULL + 1;
    sink += x;
}
__attribute__((noinline)) static uint64_t descend(int depth) {
    volatile char pad[1024];
    pad[0] = (char)depth;
    pad[1023] = pad[0];
    if (depth > 0) return descend(depth - 1) + pad[1023];
    spin();
    return pad[1023];
}

int main(int argc, char **argv) {
    sink += descend(argc > 1 ? atoi(argv[1]) : 24);
    struct timespec cpu;
    clock_gettime(CLOCK_THREAD_CPUTIME_ID, &cpu);
    fprintf(stderr, "cpu_ns=%lld\n", (long long)cpu.tv_sec * 1000000000LL + cpu.tv_nsec);
    return 0;
}
"#;

/// The loss `dwarf` declares: a stack deeper than the bytes copied is cut
/// where the copy ends. The same program recursed 24 levels of a kilobyte each
/// -- three times the 8 KiB copied -- and 2 levels: every sample of the deep
/// run is in `spin` under 24 frames of `descend` the copy cannot hold, so
/// every walk is cut, counted, and said to be; `main` is in none of its
/// stacks. The shallow run fits the copy, and reaches `main` with no such note.
#[test]
fn dwarf_counts_and_says_the_stacks_deeper_than_its_copy() {
    let directory = scratch();
    let source = directory.join("deep.c");
    let binary = directory.join("deep");
    std::fs::write(&source, DEEP).unwrap();
    if !Command::new("cc")
        .args(["-O1", "-fomit-frame-pointer", "-g", "-o"])
        .arg(&binary)
        .arg(&source)
        .status()
        .is_ok_and(|status| status.success())
    {
        eprintln!("skipped: no C compiler (`cc`) to build the deep stack with");
        return;
    }
    let run = |depth: &str| match profile(&binary, depth, Some("dwarf"), 999) {
        Ok(profile) => Some(profile),
        Err(error) if error.contains("perf_event_paranoid") || error.contains("no viable") => {
            eprintln!("skipped: this machine would not profile ({error})");
            None
        }
        Err(error) => panic!("omn cpu.profile --stack dwarf over depth {depth} failed: {error}"),
    };
    let Some(deep) = run("24") else {
        return;
    };
    let in_spin: u64 = deep
        .stacks
        .iter()
        .filter(|(frames, _)| frames.last().is_some_and(|leaf| plain(leaf) == "spin"))
        .map(|(_, events)| events)
        .sum();
    let total: u64 = deep.stacks.iter().map(|(_, events)| events).sum();
    assert!(
        total >= 100 && in_spin * 10 >= total * 9,
        "the deep run was built to live in `spin`: {in_spin} of {total} samples there"
    );
    let reaching_main: u64 = deep
        .stacks
        .iter()
        .filter(|(frames, _)| frames.iter().any(|frame| plain(frame) == "main"))
        .map(|(_, events)| events)
        .sum();
    assert!(
        reaching_main * 10 <= total,
        "{reaching_main} of {total} samples reached `main` through 24 KiB of stack from an \
         8 KiB copy"
    );
    // A cut walk holds what the copy held: some levels of `descend`, never
    // all 24. One whose unwind stopped at the leaf holds none, and is allowed
    // to be the exception, not the rule.
    let mut partial = 0;
    for (frames, events) in &deep.stacks {
        if frames.last().is_some_and(|leaf| plain(leaf) == "spin") {
            let levels = frames
                .iter()
                .filter(|frame| plain(frame) == "descend")
                .count();
            assert!(
                levels < 24,
                "all 24 levels were walked from an 8 KiB copy: {frames:?}"
            );
            if levels > 0 {
                partial += events;
            }
        }
    }
    assert!(
        partial * 10 >= in_spin * 9,
        "{partial} of {in_spin} samples in `spin` kept part of the recursion the copy held: {:?}",
        deep.stacks
    );
    let cut = deep
        .degradations
        .iter()
        .chain(&deep.notes)
        .find(|note| note.contains("deeper than the 8192 bytes copied"))
        .unwrap_or_else(|| {
            panic!(
                "the cut walks are not said: {:?} {:?}",
                deep.degradations, deep.notes
            )
        });
    let counted: u64 = cut
        .split_whitespace()
        .next()
        .and_then(|number| number.parse().ok())
        .unwrap_or_else(|| panic!("the note does not count the cut walks: {cut}"));
    assert!(
        counted >= in_spin * 9 / 10 && counted <= total,
        "the note counts {counted} cut walks where {in_spin} of {total} samples were in `spin` \
         under 24 KiB of stack: {cut}"
    );

    let Some(shallow) = run("2") else {
        return;
    };
    let shallow_total: u64 = shallow.stacks.iter().map(|(_, events)| events).sum();
    let shallow_main: u64 = shallow
        .stacks
        .iter()
        .filter(|(frames, _)| {
            contains_path(frames, &["main", "descend", "descend", "descend", "spin"])
        })
        .map(|(_, events)| events)
        .sum();
    assert!(
        shallow_main * 10 >= shallow_total * 9,
        "2 KiB of stack fits the copy, and only {shallow_main} of {shallow_total} samples \
         reached `main`: {:?}",
        shallow.stacks
    );
    assert!(
        !shallow
            .degradations
            .iter()
            .chain(&shallow.notes)
            .any(|note| note.contains("deeper than the 8192 bytes copied")),
        "a stack that fits the copy was said to be cut: {:?} {:?}",
        shallow.degradations,
        shallow.notes
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
        if stderr.contains("has no viable backend on this machine") {
            // The source itself is not served here, whatever the stack mode:
            // the machine's reasons first, on the skip line.
            let (reasons, rest): (Vec<&str>, Vec<&str>) = stderr
                .lines()
                .filter(|line| !line.trim().is_empty())
                .partition(|line| line.contains("missing") || line.contains("paranoid"));
            eprintln!(
                "skipped: {}\n    {}",
                reasons
                    .iter()
                    .map(|line| line.trim())
                    .collect::<Vec<_>>()
                    .join("; "),
                rest.join("\n    ")
            );
            return;
        }
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
        // The kernel copies what was asked for, or less where the stack ends
        // first: at the top of the `[stack]` mapping, or at the first page
        // above the stack pointer that is not resident, since the copy is
        // made where no page fault may be taken (`copy_from_user_nmi`). Such
        // a page is the gap `exec` leaves between the strings at the top and
        // the argument vectors below them (`arch_align_stack`, up to 8 KiB at
        // random), which nothing ever writes. Both are read from the target's
        // own maps and pagemap, not from the tool.
        let layout = stack_top.expect("the target's stack was read while it ran");
        let expected = |sample: &Sample| {
            let sp = sample.user_regs.unwrap().sp;
            (layout.end_above(sp).saturating_sub(sp) as usize).min(declared)
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

    // The cost of each mode, measured outside the tool: over a steady second
    // and a half of the target's run, the cpu time the kernel accounts to
    // `omn`'s threads, against the cpu time it accounts to the target -- the
    // clock `CLOCK_THREAD_CPUTIME_ID` reads -- which says how many periods,
    // and so how many samples, that was.
    //
    // The two modes run at the same time, side by side, three times over, and
    // are compared pooled: what a sample costs in cpu time moves with the
    // machine's load (a busy sibling hyperthread, a cold cache), by a factor
    // of two between two fp runs a minute apart at load 12 to 18 on 24 cpus,
    // which is the size of the separation itself. Run together, both modes
    // meet the same load; pooled, one unlucky placement does not decide it.
    let mut fp = Vec::new();
    let mut dwarf = Vec::new();
    for _ in 0..3 {
        let (one, other) = std::thread::scope(|scope| {
            let one = scope.spawn(|| steady_cost(binary, "fp", 1999));
            let other = scope.spawn(|| steady_cost(binary, "dwarf", 1999));
            (one.join().unwrap(), other.join().unwrap())
        });
        let (Some(one), Some(other)) = (one, other) else {
            return;
        };
        fp.push(one);
        dwarf.push(other);
    }
    let describe = |runs: &[SteadyCost]| -> String {
        runs.iter()
            .map(|run| {
                format!(
                    "{:.1} us a sample ({:.1} ms of omn over {:.0} ms of the target)",
                    run.per_sample_ns() / 1e3,
                    run.tool_ns as f64 / 1e6,
                    run.target_ns as f64 / 1e6
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    eprintln!("fp: {}; dwarf: {}", describe(&fp), describe(&dwarf));
    let pooled = |runs: &[SteadyCost]| -> f64 {
        let tool: u64 = runs.iter().map(|run| run.tool_ns).sum();
        let samples: f64 = runs.iter().map(SteadyCost::samples).sum();
        tool as f64 / samples.max(1.0)
    };
    assert!(
        pooled(&dwarf) > pooled(&fp),
        "declared high and low, dwarf and fp do not separate, run side by side: fp {:.1} us \
         a sample pooled ({}); dwarf {:.1} us ({})",
        pooled(&fp) / 1e3,
        describe(&fp),
        pooled(&dwarf) / 1e3,
        describe(&dwarf)
    );

    // The samples and the tool's own figure, over whole runs. The target did
    // the same work under both modes, and a sample is a period of its own
    // clock: the samples delivered are what that clock says ran, with a run's
    // partial first and last periods and the load of five other streams well
    // inside a tenth. What the tool reports it cost cannot exceed what the
    // kernel counts for the run beyond the target (`wait4`, whole run, less
    // the target's own clock), at the microsecond resolution of `wait4`.
    for mode in ["fp", "dwarf"] {
        let Some(run) = profile_or_skip(Build::NoFp, Some(mode)) else {
            return;
        };
        let expected = run.target_cpu_ns as f64 / (1e9 / 1999.0);
        let said = format!(
            "{mode}: {} samples, the target's clock says {:.0} ms = {expected:.0} periods; the \
             tool says it cost {:.1} ms in the window and {:.1} ms in all, the kernel counts \
             {:.1} ms beyond the target",
            run.events,
            run.target_cpu_ns as f64 / 1e6,
            run.collector_cpu_ns as f64 / 1e6,
            run.reported_tool_ns as f64 / 1e6,
            run.tool_cpu_ns() as f64 / 1e6
        );
        eprintln!("{said}");
        assert!(
            (run.events as f64) > expected * 0.9 && (run.events as f64) < expected * 1.1,
            "{said}"
        );
        assert!(
            run.collector_cpu_ns <= run.tool_cpu_ns() + 1_000_000,
            "{said}"
        );
        // And the whole account is no longer only a lower bound. The window
        // alone is a twentieth of what `omn` costs a debug run: the rest is
        // its start (the catalog, the probes, the kernel's symbols) and its
        // end, which the cost record carries beside the window since the
        // sixteenth wave. Measured here: 1169.9 ms said against 1178.1 ms
        // counted (fp), 1150.9 against 1159.5 (dwarf); what is left is the
        // results' formatting and the target's own exec and exit outside its
        // thread clock. Half is the separation from the 26 ms the window
        // alone said.
        assert!(
            run.reported_tool_ns <= run.tool_cpu_ns() + 1_000_000
                && run.reported_tool_ns * 2 >= run.tool_cpu_ns(),
            "{said}"
        );
    }
}

/// What `omn` cost over a steady stretch of its target's run, both by the
/// kernel's account.
#[derive(Debug)]
struct SteadyCost {
    /// Cpu time of `omn`'s threads over the stretch.
    tool_ns: u64,
    /// Cpu time of the target over the same stretch.
    target_ns: u64,
    hz: u32,
}

impl SteadyCost {
    /// The tool's cpu per sample: the target's cpu over the stretch is that
    /// many periods of the sampling clock, and each period is one sample.
    fn per_sample_ns(&self) -> f64 {
        self.tool_ns as f64 / self.samples().max(1.0)
    }

    /// The periods of the sampling clock the target's cpu over the stretch
    /// is.
    fn samples(&self) -> f64 {
        self.target_ns as f64 * f64::from(self.hz) / 1e9
    }
}

/// The cpu time of every thread of `pid`, from each one's `schedstat`: the
/// kernel's nanoseconds on a cpu, the same account `CLOCK_THREAD_CPUTIME_ID`
/// reads.
fn cpu_of(pid: u32) -> Option<u64> {
    let mut total = 0;
    for task in std::fs::read_dir(format!("/proc/{pid}/task")).ok()? {
        let text = std::fs::read_to_string(task.ok()?.path().join("schedstat")).ok()?;
        total += text.split_whitespace().next()?.parse::<u64>().ok()?;
    }
    Some(total)
}

/// The child of `parent` whose command name is `name`, once it has exec'd.
fn child_named(parent: u32, name: &str) -> Option<u32> {
    std::fs::read_dir("/proc").ok()?.find_map(|entry| {
        let pid: u32 = entry.ok()?.file_name().to_str()?.parse().ok()?;
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let (head, tail) = stat.rsplit_once(") ")?;
        let comm = head.split_once(" (")?.1;
        let ppid: u32 = tail.split_whitespace().nth(1)?.parse().ok()?;
        (ppid == parent && name.starts_with(comm) && !comm.is_empty()).then_some(pid)
    })
}

/// Runs `omn cpu.profile --stack <stack>` over the nofp build for about three
/// seconds and measures it from outside over a second and a half in the
/// middle, where neither the tool's start nor its symbolizing at the end is.
fn steady_cost(binary: &Path, stack: &str, hz: u32) -> Option<SteadyCost> {
    let out = scratch().join(format!("steady-{stack}-{}", std::process::id()));
    let name = binary.file_name()?.to_str()?.to_owned();
    let mut omn = Command::new(BINARY)
        .args([
            "cpu.profile",
            "--mode",
            &format!("sample@{hz}hz"),
            "--emit",
            "stack",
            "--format",
            "json",
            "--stack",
            stack,
            "-o",
        ])
        .arg(&out)
        .arg("--")
        .arg(binary)
        .arg("3000")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("omn runs");
    let start = std::time::Instant::now();
    let target = loop {
        if let Some(pid) = child_named(omn.id(), &name) {
            break pid;
        }
        if omn.try_wait().expect("wait").is_some() || start.elapsed() > Duration::from_secs(10) {
            let _ = omn.kill();
            let _ = omn.wait();
            eprintln!("skipped: the target of `omn --stack {stack}` was never seen running");
            return None;
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    std::thread::sleep(Duration::from_millis(300));
    let before = (cpu_of(omn.id()), cpu_of(target));
    std::thread::sleep(Duration::from_millis(1500));
    let after = (cpu_of(omn.id()), cpu_of(target));
    let status = omn.wait().expect("wait");
    assert!(status.success(), "omn --stack {stack} failed: {status}");
    match (before, after) {
        ((Some(tool0), Some(target0)), (Some(tool1), Some(target1))) if target1 > target0 => {
            Some(SteadyCost {
                tool_ns: tool1.saturating_sub(tool0),
                target_ns: target1 - target0,
                hz,
            })
        }
        _ => panic!(
            "the target ran out before the stretch ended: {before:?} {after:?}; it is built to \
             run about three seconds"
        ),
    }
}

/// Launches `binary`, samples it with `callchain` from its first instruction
/// until it exits, and hands back the samples.
fn sample_launched(
    binary: &Path,
    callchain: Callchain,
    batches: u32,
) -> Option<(Vec<Sample>, Option<StackLayout>)> {
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
            eprintln!(
                "skipped: this machine will not sample this process with {callchain:?} \
                 ({}): {error}",
                paranoid_said()
            );
            return None;
        }
    };
    event.enable().expect("enable");
    let mut samples = Vec::new();
    let mut stack_top = None;
    loop {
        // Read on every turn and the last kept: a page above the stack
        // pointer that becomes resident later is one the copy then reads.
        stack_top = stack_layout_of(pid).or(stack_top);
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

/// A process's `[stack]` mapping, and which of its pages are resident.
#[derive(Debug, Clone)]
struct StackLayout {
    /// The end of the mapping.
    top: u64,
    /// The start of each page of the mapping that is not resident.
    absent: Vec<u64>,
}

impl StackLayout {
    /// Where a copy from `sp` upwards that may not fault stops: the first
    /// page above `sp`'s own that is not resident, or the top.
    fn end_above(&self, sp: u64) -> u64 {
        self.absent
            .iter()
            .copied()
            .filter(|page| *page > sp)
            .min()
            .unwrap_or(self.top)
            .min(self.top)
    }
}

/// The `[stack]` mapping of a process from its maps, and its resident pages
/// from its pagemap (bit 63 of each page's word: present; readable by the
/// process's owner, with the frame numbers zeroed).
fn stack_layout_of(pid: i32) -> Option<StackLayout> {
    use std::os::unix::fs::FileExt;
    let maps = std::fs::read_to_string(format!("/proc/{pid}/maps")).ok()?;
    let line = maps.lines().find(|line| line.ends_with("[stack]"))?;
    let (start, end) = line.split_whitespace().next()?.split_once('-')?;
    let (start, top) = (
        u64::from_str_radix(start, 16).ok()?,
        u64::from_str_radix(end, 16).ok()?,
    );
    // SAFETY: sysconf reads a constant of this system.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    let pagemap = std::fs::File::open(format!("/proc/{pid}/pagemap")).ok()?;
    let mut words = vec![0u8; ((top - start) / page * 8) as usize];
    pagemap.read_exact_at(&mut words, start / page * 8).ok()?;
    let absent = words
        .chunks_exact(8)
        .enumerate()
        .filter(|(_, word)| u64::from_le_bytes((*word).try_into().unwrap()) >> 63 == 0)
        .map(|(index, _)| start + index as u64 * page)
        .collect();
    Some(StackLayout { top, absent })
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
#include <stdio.h>
#include <stdlib.h>
#include <sys/resource.h>
#include <unistd.h>
int main(int argc, char **argv) {
    long n = argc > 1 ? atol(argv[1]) : 300000;
    int fd = open("/dev/null", O_WRONLY);
    char byte = 0;
    for (long i = 0; i < n; i++) write(fd, &byte, 1);
    struct rusage usage;
    getrusage(RUSAGE_SELF, &usage);
    fprintf(stderr, "user_us=%ld system_us=%ld\n",
            usage.ru_utime.tv_sec * 1000000L + usage.ru_utime.tv_usec,
            usage.ru_stime.tv_sec * 1000000L + usage.ru_stime.tv_usec);
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
        let (samples, stderr) = sample_launched_args(&binary, callchain, "600000")?;
        let field = |name: &str| -> u64 {
            stderr
                .split_whitespace()
                .find_map(|word| word.strip_prefix(name))
                .and_then(|value| value.parse().ok())
                .unwrap_or_else(|| panic!("the program did not report {name}: {stderr}"))
        };
        let own = (field("user_us=") * 1_000, field("system_us=") * 1_000);
        let kernel_frames: usize = samples
            .iter()
            .filter_map(omn::stack::RawStack::of)
            .map(|raw| raw.kernel.len())
            .sum();
        let in_kernel = samples
            .iter()
            .filter(|sample| sample.cpu_mode == CpuMode::Kernel)
            .count();
        Some((samples.len(), kernel_frames, in_kernel, own))
    };
    let Some((user_samples, user_kernel_frames, user_in_kernel, (user_ns, system_ns))) =
        count(Callchain::FramePointers { kernel: false })
    else {
        return;
    };
    // The program's own account (`getrusage`) of where its time went is the
    // reference, at any paranoia: user-only samples cover the user time and
    // leave the system time out. A sample is 100 us of cpu clock; the split
    // between user and system time is the kernel's tick-sampled estimate, so
    // the slack is generous and the assertion is the separation.
    let sampled_ns = user_samples as u64 * 100_000;
    eprintln!(
        "user half: {user_samples} samples = {} ms; the program says {} ms user, {} ms system",
        sampled_ns / 1_000_000,
        user_ns / 1_000_000,
        system_ns / 1_000_000
    );
    assert!(
        system_ns > user_ns * 2,
        "the workload was meant to live in the kernel: {user_ns} ns user, {system_ns} ns system"
    );
    assert!(
        sampled_ns * 2 < user_ns + system_ns,
        "user-only samples covered {sampled_ns} ns of a program that used {user_ns} ns user \
         and {system_ns} ns system: the system time was not left out"
    );
    assert!(
        sampled_ns * 2 > user_ns && sampled_ns < user_ns * 2 + 20_000_000,
        "user-only samples covered {sampled_ns} ns where the program says it used {user_ns} ns \
         in user mode"
    );
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
    let (both_samples, both_kernel_frames, both_in_kernel, _) =
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

/// Launches `binary arg` sampled with `callchain`, and hands back the samples
/// and what the program wrote on its standard error.
fn sample_launched_args(
    binary: &Path,
    callchain: Callchain,
    arg: &str,
) -> Option<(Vec<Sample>, String)> {
    let mut child = Command::new(binary)
        .arg(arg)
        .stderr(Stdio::piped())
        .spawn()
        .expect("the fixture was just built");
    let mut stderr_pipe = child.stderr.take().expect("stderr was piped");
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr_pipe.read_to_string(&mut text);
        text
    });
    let pid = child.id() as i32;
    let spec = omn::perf::SampleSpec::software(CPU_CLOCK, 100_000, callchain);
    let mut event = match SampledEvent::open(&spec, Scope::Process(pid), 512) {
        Ok(event) => event,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            eprintln!(
                "skipped: this machine will not sample this process with {callchain:?} \
                 ({}): {error}",
                paranoid_said()
            );
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
    let stderr = reader.join().unwrap_or_default();
    Some((samples, stderr))
}

/// Whether the kernel lets the `omn` binary profile the kernel, read from what
/// the kernel itself decides by (`perf_allow_kernel`): `perf_event_paranoid`
/// at 1 or below, or a process that holds `CAP_PERFMON` or `CAP_SYS_ADMIN`.
/// Nothing of the tool is asked: the paranoia is read from `/proc`, the
/// capabilities from the binary's `security.capability` attribute and this
/// process's credentials. `Err` is a case this cannot tell, said by the caller.
fn kernel_allowed_to_the_binary() -> Result<(bool, String), String> {
    let paranoid: i32 = std::fs::read_to_string("/proc/sys/kernel/perf_event_paranoid")
        .map_err(|error| format!("cannot read perf_event_paranoid: {error}"))?
        .trim()
        .parse()
        .map_err(|error| format!("perf_event_paranoid is not a number: {error}"))?;
    if paranoid <= 1 {
        return Ok((true, format!("perf_event_paranoid {paranoid}")));
    }
    if unsafe { libc::geteuid() } == 0 {
        return Ok((true, format!("perf_event_paranoid {paranoid}, run as root")));
    }
    const CAP_SYS_ADMIN: u32 = 21;
    const CAP_PERFMON: u32 = 38;
    let status = std::fs::read_to_string("/proc/self/status").map_err(|error| error.to_string())?;
    let bounding = status
        .lines()
        .find_map(|line| line.strip_prefix("CapBnd:"))
        .and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok())
        .ok_or("no CapBnd in /proc/self/status")?;
    let path = std::ffi::CString::new(BINARY).unwrap();
    let name = std::ffi::CString::new("security.capability").unwrap();
    let mut raw = [0u8; 24];
    let len = unsafe {
        libc::getxattr(
            path.as_ptr(),
            name.as_ptr(),
            raw.as_mut_ptr().cast(),
            raw.len(),
        )
    };
    if len < 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENODATA) {
            return Ok((
                false,
                format!("perf_event_paranoid {paranoid}, and {BINARY} carries no capability"),
            ));
        }
        return Err(format!("cannot read {BINARY}'s capabilities: {error}"));
    }
    if len < 20 {
        return Err(format!(
            "{BINARY}'s capability attribute is {len} bytes, not a v2 or v3 one"
        ));
    }
    let word = |at: usize| u32::from_le_bytes(raw[at..at + 4].try_into().unwrap());
    let effective = word(0) & 1 == 1;
    let permitted = u64::from(word(4)) | (u64::from(word(12)) << 32);
    let granted = permitted & bounding;
    let capable = granted & (1 << CAP_PERFMON | 1 << CAP_SYS_ADMIN) != 0;
    if capable && !effective {
        return Err(format!(
            "{BINARY} is permitted CAP_PERFMON or CAP_SYS_ADMIN but not with the effective bit, \
             which only a program that raises its own capabilities uses"
        ));
    }
    Ok((
        capable,
        format!(
            "perf_event_paranoid {paranoid}, {BINARY} {} CAP_PERFMON or CAP_SYS_ADMIN",
            if capable { "holds" } else { "holds neither" }
        ),
    ))
}

/// The resolver's own words, held against the kernel's rule rather than
/// against the probe the chooser asks: where the paranoia and the binary's
/// capabilities refuse the kernel, the choice is user-only and says so, for a
/// mode with stacks and for `--stack none` alike -- a sampler without stacks
/// still has to leave the kernel out to be opened at all. Where they allow it,
/// the choice takes the kernel too and says nothing.
#[test]
fn the_choice_says_user_only_exactly_when_the_machine_refuses_the_kernel() {
    let Some(binary) = build(Build::Leaf) else {
        return;
    };
    let (allowed, why) = match kernel_allowed_to_the_binary() {
        Ok(answer) => answer,
        Err(error) => {
            eprintln!("skipped: cannot tell independently whether the kernel is refused: {error}");
            return;
        }
    };
    eprintln!("kernel profiling allowed to the binary: {allowed} ({why})");
    let paranoid = std::fs::read_to_string("/proc/sys/kernel/perf_event_paranoid")
        .ok()
        .and_then(|text| text.trim().parse::<i32>().ok());
    if let Some(paranoid) = paranoid.filter(|paranoid| !allowed && *paranoid >= 3) {
        // Above 2 (a Debian patch; Ubuntu ships 4) the kernel refuses this
        // user every perf event, user-only ones included: there is no choice
        // of halves to hold the resolver's words to.
        eprintln!(
            "skipped: perf_event_paranoid is {paranoid}, which refuses an unprivileged process \
             every perf event, user-only samples included ({why})"
        );
        return;
    }
    for (stack, words) in [
        (None, "stacks are user-only"),
        (Some("none"), "samples are user-only"),
    ] {
        let profile = match profile_emitting(binary, "50", stack, 999, stack.is_none()) {
            Ok(profile) => profile,
            Err(error) => panic!(
                "cpu.profile --stack {stack:?} was refused where the kernel {} ({why}): {error}",
                if allowed {
                    "is allowed"
                } else {
                    "is refused and user-only samples are what it should take"
                }
            ),
        };
        let said = profile
            .degradations
            .iter()
            .chain(&profile.notes)
            .any(|note| note.contains(words));
        assert_eq!(
            said, !allowed,
            "--stack {stack:?}, {why}: {:?} {:?}",
            profile.degradations, profile.notes
        );
        assert!(profile.events > 0, "--stack {stack:?}: no sample at all");
    }
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
    // The kernel half only where the machine allows it: the drops per ring
    // are the subject, and a user-only chain fills a ring the same way.
    let callchain = Callchain::FramePointers {
        kernel: omn::perf::kernel_callchain_allowed(),
    };
    let spec = omn::perf::SampleSpec::software(CPU_CLOCK, 50_000, callchain);
    let mut sampler = match TaskSampler::open(&spec, pid, true, 8, 8) {
        Ok(sampler) => sampler,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            eprintln!(
                "skipped: this machine will not sample this process ({}): {error}",
                paranoid_said()
            );
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
fn serve(debuginfo: PathBuf) -> (String, Arc<AtomicUsize>, Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let url = format!("http://{}", listener.local_addr().unwrap());
    let asked = Arc::new(AtomicUsize::new(0));
    let all = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (counter, every) = (Arc::clone(&asked), Arc::clone(&all));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut request = [0u8; 2048];
            let read = stream.read(&mut request).unwrap_or(0);
            let request = String::from_utf8_lossy(&request[..read]);
            let path = request.split_whitespace().nth(1).unwrap_or("").to_owned();
            every
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(path.clone());
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

/// The sampling period of [`named_stacks`]: shorter than the shortest branch.
///
/// A batch of the call tree takes about a millisecond of cpu (973 us here),
/// and this was 1 ms: one sample a batch, at a phase that drifts by the
/// difference between the two. When they are close the samples stay in one or
/// two neighbouring branches for the whole run -- the stripped build's "second
/// shape", whose unstripped reference held only `hash_block` and
/// `parse_record`. Matched to the batch on purpose (972 us), one run in seven
/// put 229 of its 299 samples in `hash_block`, built to take 40 %. Below the
/// shortest branch (`checksum`, a tenth of a batch, about 97 us) every
/// branch holds a sample point in every batch whatever the phase; 61 us is
/// that with room for a machine half again as fast, and is no divisor of
/// anything the program does.
const NAMED_PERIOD_NS: u64 = 61_001;

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
    let spec = omn::perf::SampleSpec::software(CPU_CLOCK, NAMED_PERIOD_NS, callchain);
    let mut event = match SampledEvent::open(&spec, Scope::Process(pid), 512) {
        Ok(event) => event,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            eprintln!(
                "skipped: this machine will not sample this process ({}): {error}",
                paranoid_said()
            );
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
    // `asked` counts the requests for the call tree's build id; `all` is the
    // path of every request, which includes the one for a stripped libc this
    // server does not have.
    let (url, asked, all) = serve(unstripped.to_path_buf());
    let cache = scratch().join("debuginfod-cache");
    let _ = std::fs::remove_dir_all(&cache);

    // Not asked: nothing is fetched, and the frames are offsets into the file.
    let Some((local, origins)) = named_stacks(stripped, None) else {
        return;
    };
    assert_eq!(
        all.lock().unwrap().len(),
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
    // Every stack of both runs with its samples, for a failure to be read
    // from: a whole branch missing from one run was seen twice under load,
    // with one sample a batch (`NAMED_PERIOD_NS` says why that was the cause).
    let counted = |stacks: &[Vec<String>]| -> String {
        let mut counts: BTreeMap<&[String], usize> = BTreeMap::new();
        for frames in stacks {
            *counts.entry(frames.as_slice()).or_default() += 1;
        }
        format!("{} samples: {counts:?}", stacks.len())
    };
    let evidence = format!(
        "named: {}\nreference: {}",
        counted(&named),
        counted(&reference)
    );
    // The paths from `main` to `spin`, and the samples whose walk stopped
    // short of `main`. A walk can stop short for a reason that is not the
    // names': one sample in about three hundred did on the CI's cloud VM, as
    // `["spin"]`, in one run of the two. What is held is the naming: the
    // whole paths are the same set, and a short one is the leaf end of a
    // whole one -- a frame named wrongly is the end of none -- and rare.
    let paths = |stacks: &[Vec<String>]| -> (BTreeSet<Vec<String>>, Vec<Vec<String>>) {
        let (whole, short): (Vec<&Vec<String>>, Vec<&Vec<String>>) = stacks
            .iter()
            .filter(|frames| frames.last().map(String::as_str) == Some("spin"))
            .partition(|frames| frames.iter().any(|frame| frame == "main"));
        let whole = whole
            .into_iter()
            .map(|frames| {
                let from = frames.iter().position(|frame| frame == "main").unwrap();
                frames[from..].to_vec()
            })
            .collect();
        (whole, short.into_iter().cloned().collect())
    };
    let ((named_paths, named_short), (reference_paths, reference_short)) =
        (paths(&named), paths(&reference));
    assert!(named_paths.len() >= 4, "{named_paths:?}\n{evidence}");
    assert_eq!(
        named_paths, reference_paths,
        "the stripped build, named from debuginfod, does not read as the unstripped one\n\
         {evidence}"
    );
    for (short, whole, stacks, which) in [
        (&named_short, &named_paths, &named, "the stripped build"),
        (
            &reference_short,
            &reference_paths,
            &reference,
            "the unstripped build",
        ),
    ] {
        for frames in short {
            assert!(
                whole.iter().any(|path| path.ends_with(frames)),
                "{which}: a walk that stopped short of main, {frames:?}, is the leaf end of no \
                 whole path: {whole:?}\n{evidence}"
            );
        }
        assert!(
            short.len() * 20 < stacks.len(),
            "{which}: {} of {} samples stopped short of main: {short:?}\n{evidence}",
            short.len(),
            stacks.len()
        );
    }

    // Asked again: the cache answers, and the server is not -- not for the
    // file it had, and not for the one it did not (the stripped libc). A
    // module that only this run sampled (a library a sample of the first run
    // happened not to land in, which a loaded machine makes likelier) is a
    // first request, so what is held is that no path is asked twice, by the
    // server's own log.
    let before = all.lock().unwrap().clone();
    let (_, origins) = named_stacks(stripped, Some(client)).unwrap();
    let again: Vec<String> = all.lock().unwrap()[before.len()..]
        .iter()
        .filter(|path| before.contains(path))
        .cloned()
        .collect();
    assert!(
        again.is_empty(),
        "asked again of the server, after the first run asked it: {again:?} (the first \
         run asked {before:?})"
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

// ---------------------------------------------------------------------------
// `--symbols`, from the command line and the session file.

/// One `omn` run over the call tree, with its environment set: the stacks
/// root first, and the resolved file. Stderr on failure.
fn profile_named(
    session: Option<&Path>,
    binary: &Path,
    symbols: Option<&str>,
    env: &[(&str, &str)],
) -> Result<(Vec<Vec<String>>, serde_json::Value), String> {
    profile_noted(session, binary, symbols, env).map(|(stacks, resolved, _)| (stacks, resolved))
}

/// Stacks root first, the resolved file, and the profile's notes.
type Noted = (Vec<Vec<String>>, serde_json::Value, Vec<String>);

/// [`profile_named`], with the profile's notes.
fn profile_noted(
    session: Option<&Path>,
    binary: &Path,
    symbols: Option<&str>,
    env: &[(&str, &str)],
) -> Result<Noted, String> {
    static RUN: AtomicUsize = AtomicUsize::new(0);
    let out = scratch().join(format!("named-{}", RUN.fetch_add(1, Ordering::SeqCst)));
    let mut command = Command::new(BINARY);
    command
        .env_remove("DEBUGINFOD_URLS")
        .envs(env.iter().copied());
    match session {
        Some(session) => {
            command.arg("-f").arg(session);
        }
        None => {
            command.args(["cpu.profile", "--mode", "sample@999hz", "--emit", "stack"]);
            if let Some(symbols) = symbols {
                command.args(["--symbols", symbols]);
            }
        }
    }
    command.args(["--format", "json", "-o"]).arg(&out);
    if session.is_none() {
        command.arg("--").arg(binary).arg("300");
    }
    let output = command.output().map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    let read = |name: &str| -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(out.join(name)).expect(name)).expect(name)
    };
    let results = read("results.json");
    let result = &results["results"][0];
    let table = &result["stacks"];
    let stacks = result["outputs"][0]["stacks"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|row| {
            let id = row["stack"].as_u64().expect("a stack id").to_string();
            table[&id]
                .as_array()
                .expect("the table holds every stack an output names")
                .iter()
                .map(|frame| frame.as_str().unwrap_or_default().to_owned())
                .collect()
        })
        .collect();
    let notes = result["notes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|note| note.as_str().map(str::to_owned))
        .collect();
    let resolved = read("resolved.json");
    let resolved = if resolved.is_array() {
        resolved[0].clone()
    } else {
        resolved
    };
    Ok((stacks, resolved, notes))
}

/// The branches of the call tree, known by construction, that some stack of
/// a set carries whole. Held to the tree rather than to another profile's
/// exact set of stacks: under load a few samples come back cut short, in
/// named and unnamed profiles alike, and those are not about the names.
fn branches_named(stacks: &[Vec<String>]) -> BTreeSet<&'static str> {
    BRANCHES
        .iter()
        .filter(|(_, path, _)| stacks.iter().any(|frames| contains_path(frames, path)))
        .map(|(branch, _, _)| *branch)
        .collect()
}

/// `--symbols debuginfod` reaches the profile from the command line and from
/// a session file, and the stripped build comes back in the names of the
/// unstripped one -- the file this test compiled, whose build id the stripped
/// copy shares -- fetched from a server on loopback that counts what it is
/// asked. Without the flag nothing is asked. The resolved file says, before
/// the run, where the names will come from and which server is asked.
#[test]
fn symbols_from_debuginfod_are_asked_for_from_the_command_line_and_the_session_file() {
    let (Some(stripped), Some(unstripped)) = (build(Build::Strip), build(Build::NoFp)) else {
        return;
    };
    let reference = match profile_named(None, unstripped, None, &[]) {
        Ok((stacks, _)) => branches_named(&stacks),
        Err(error) if error.contains("perf_event_paranoid") || error.contains("no viable") => {
            eprintln!("skipped: this machine would not profile ({error})");
            return;
        }
        Err(error) => panic!("the unstripped build would not profile: {error}"),
    };
    assert_eq!(reference.len(), BRANCHES.len(), "{reference:?}");

    let (url, asked, all) = serve(unstripped.to_path_buf());
    let cache = scratch().join("cli-debuginfod-cache");
    let _ = std::fs::remove_dir_all(&cache);
    let cache_text = cache.to_string_lossy().into_owned();
    let env = [
        ("DEBUGINFOD_URLS", url.as_str()),
        ("DEBUGINFOD_CACHE_PATH", cache_text.as_str()),
    ];

    // Not asked, with a server named: nothing is fetched.
    let (local, resolved) = profile_named(None, stripped, None, &env).expect("a local profile");
    assert_eq!(
        all.lock().unwrap().len(),
        0,
        "a server was asked without --symbols"
    );
    assert!(
        !local.iter().flatten().any(|frame| plain(frame) == "spin"),
        "a stripped build named its static functions from nowhere"
    );
    assert!(
        resolved["stack"]["symbols"].is_null(),
        "{}",
        resolved["stack"]
    );

    // Asked on the command line.
    let (named, resolved) =
        profile_named(None, stripped, Some("debuginfod"), &env).expect("a named profile");
    assert_eq!(asked.load(Ordering::SeqCst), 1, "one build id, one request");
    assert_eq!(resolved["request"]["symbols"], "debuginfod");
    assert_eq!(resolved["stack"]["symbols"], "debuginfod");
    assert_eq!(
        resolved["stack"]["symbol_servers"],
        serde_json::json!([url])
    );
    assert_eq!(
        branches_named(&named),
        reference,
        "the stripped build, named from debuginfod through `omn`, does not read as the \
         unstripped one"
    );

    // Asked in a session file, with a cache of its own so the server is asked
    // again rather than the cache answering.
    let fresh = scratch().join("session-debuginfod-cache");
    let _ = std::fs::remove_dir_all(&fresh);
    let fresh_text = fresh.to_string_lossy().into_owned();
    let session = scratch().join("symbols.yml");
    std::fs::write(
        &session,
        format!(
            "requests:\n  - source: cpu.profile\n    target: {{launch: [{}, '300']}}\n    \
             mode: sample@999hz\n    emit: [stack]\n    symbols: debuginfod\n",
            stripped.display()
        ),
    )
    .unwrap();
    let (from_file, resolved) = profile_named(
        Some(&session),
        stripped,
        None,
        &[
            ("DEBUGINFOD_URLS", url.as_str()),
            ("DEBUGINFOD_CACHE_PATH", fresh_text.as_str()),
        ],
    )
    .expect("a profile from the session file");
    assert_eq!(
        asked.load(Ordering::SeqCst),
        2,
        "the session file did not ask"
    );
    assert_eq!(resolved["stack"]["symbols"], "debuginfod");
    assert_eq!(branches_named(&from_file), reference);
}

/// Debuginfod asked for with no server to ask is refused before the program
/// runs, and on a source whose collector would never ask it is refused as
/// this build's gap rather than accepted and ignored.
#[test]
fn symbols_from_debuginfod_with_nowhere_to_ask_are_refused_before_the_program_starts() {
    let marker = scratch().join("symbols-launched");
    let _ = std::fs::remove_file(&marker);
    let output = Command::new(BINARY)
        .env_remove("DEBUGINFOD_URLS")
        .args(["cpu.profile", "--mode", "sample@99hz", "--emit", "stack"])
        .args([
            "--symbols",
            "debuginfod",
            "--",
            "/bin/sh",
            "-c",
            "touch \"$0\"",
        ])
        .arg(&marker)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("no viable backend") {
        eprintln!("skipped: this machine would not profile ({stderr})");
        return;
    }
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("DEBUGINFOD_URLS"), "{stderr}");
    assert!(
        !marker.exists(),
        "the program ran before the refusal: {stderr}"
    );

    let output = Command::new(BINARY)
        .env("DEBUGINFOD_URLS", "http://127.0.0.1:9")
        .args(["io.syscall", "--mode", "trace", "--emit", "summary"])
        .args([
            "--symbols",
            "debuginfod",
            "--",
            "/bin/sh",
            "-c",
            "touch \"$0\"",
        ])
        .arg(&marker)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(3), "{stderr}");
    assert!(
        stderr.contains("`--symbols debuginfod` is not implemented for `io.syscall`"),
        "{stderr}"
    );
    assert!(
        !marker.exists(),
        "the program ran before the refusal: {stderr}"
    );
}

/// A choice the run refuses is refused by `--dump-resolved` too, with the same
/// reason and the same exit code, rather than dumped with no `stack`. The
/// ground truth is the run's own refusal of the same request, made before the
/// program it would profile starts (the program creates a file as its first
/// act, and the file is not there): `--symbols debuginfod` with no
/// DEBUGINFOD_URLS, and a forced `--stack` mode this machine or build cannot
/// serve (`lbr` needs an Intel branch call stack, `sframe` a kernel that
/// unwinds from `.sframe`; whichever this machine refuses is asked).
#[test]
fn a_choice_the_run_refuses_is_refused_by_the_dump_with_the_same_reason() {
    let marker = scratch().join("dump-refused-launched");
    let mut asked = 0;
    let cases: [&[&str]; 3] = [
        &["--symbols", "debuginfod"],
        &["--stack", "lbr"],
        &["--stack", "sframe"],
    ];
    for extra in cases {
        let _ = std::fs::remove_file(&marker);
        let run = |dump: bool| {
            let mut command = Command::new(BINARY);
            command
                .env_remove("DEBUGINFOD_URLS")
                .args(["cpu.profile", "--mode", "sample@99hz", "--emit", "stack"])
                .args(extra);
            if dump {
                command.args(["--format", "json", "--dump-resolved"]);
            }
            command
                .args(["--", "/bin/sh", "-c", "touch \"$0\""])
                .arg(&marker)
                .output()
                .unwrap()
        };
        let ran = run(false);
        let ran_said = String::from_utf8_lossy(&ran.stderr).into_owned();
        if ran_said.contains("no viable backend") {
            eprintln!("skipped: this machine would not profile ({ran_said})");
            return;
        }
        if ran.status.success() {
            // This machine serves the forced mode: nothing refused to hold
            // the dump to.
            eprintln!("not asked: {extra:?} is served here");
            continue;
        }
        assert!(
            !marker.exists(),
            "the program ran before {extra:?} was refused"
        );
        let dumped = run(true);
        let dump_said = String::from_utf8_lossy(&dumped.stderr).into_owned();
        assert_eq!(
            dumped.status.code(),
            ran.status.code(),
            "the dump of {extra:?}: {dump_said}\nthe run: {ran_said}"
        );
        assert!(
            dumped.stdout.is_empty(),
            "a resolved file was printed for a request the run refuses: {}",
            String::from_utf8_lossy(&dumped.stdout)
        );
        let reason = |said: &str| {
            said.lines()
                .find(|line| line.starts_with("error:"))
                .map(str::to_owned)
                .unwrap_or_default()
        };
        assert_eq!(reason(&dump_said), reason(&ran_said), "{extra:?}");
        assert!(
            reason(&dump_said).contains(extra[0]),
            "the reason names {}: {dump_said}",
            extra[0]
        );
        asked += 1;
    }
    assert!(
        asked >= 1,
        "DEBUGINFOD_URLS was unset, and nothing was refused"
    );
}

/// The unwind choice is in the resolved file before anything runs, and it is
/// the choice the run then makes: the mode the dump names is the one the
/// profile's own notes say it used, for a build whose frame pointers this file
/// decided.
#[test]
fn the_resolved_file_says_how_the_stacks_will_be_unwound_before_the_run() {
    let Some(nofp) = build(Build::NoFp) else {
        return;
    };
    let dump = Command::new(BINARY)
        .args(["cpu.profile", "--mode", "sample@999hz", "--emit", "stack"])
        .args(["--format", "json", "--dump-resolved", "--"])
        .arg(nofp)
        .arg("300")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&dump.stderr);
    if stderr.contains("no viable backend") {
        eprintln!("skipped: this machine would not profile ({stderr})");
        return;
    }
    assert!(dump.status.success(), "{stderr}");
    let dumped: serde_json::Value = serde_json::from_slice(&dump.stdout).expect("the dump is json");
    let dumped = if dumped.is_array() {
        dumped[0].clone()
    } else {
        dumped
    };
    let stack = &dumped["stack"];
    let mode = stack["mode"].as_str().expect("the dump names a mode");
    assert!(
        !stack["considered"].as_array().unwrap().is_empty(),
        "{stack}"
    );
    // Built with -fomit-frame-pointer: a mode that walks frame pointers
    // everywhere is not what auto may pick for it.
    assert_ne!(mode, "fp", "{stack}");
    assert_ne!(mode, "fp+leaf", "{stack}");

    // What the collector says it did, in its own notes, after the run.
    let (_, resolved, notes) = profile_noted(None, nofp, None, &[]).expect("a profile");
    let used = format!("stacks unwound by `{mode}`");
    assert!(
        notes.iter().any(|note| note.starts_with(&used)),
        "the dump said `{mode}` and the run says {notes:?}"
    );
    assert_eq!(resolved["stack"]["mode"], mode, "{}", resolved["stack"]);
}

/// A copied stack sampled after its target's `execve` is unwound by the image
/// the target now runs, though the unwinder read the one before it and no
/// tick re-read the maps since.
///
/// The target is a shell waiting on its input, whose maps the unwinder reads
/// first -- the state a launched program's unwinder is in when it is built at
/// the gate and a tick reads the fork -- and which then `exec`s the call tree.
/// No `refresh_symbols` is called after that: the walks must find the new
/// modules themselves. Before they did, every sample until the collector's
/// next tick stopped at `spin`, 5 % of a one-second profile on a loaded
/// machine and all of them here.
#[test]
fn a_copied_stack_sampled_after_an_exec_is_unwound_by_the_image_the_target_runs_now() {
    let Some(binary) = build(Build::NoFp) else {
        return;
    };
    let callchain = Callchain::CopiedStack { kernel: false };
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("read go; exec \"$0\" 100")
        .arg(binary)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("sh runs");
    let pid = child.id() as i32;
    let spec = omn::perf::SampleSpec::software(CPU_CLOCK, NAMED_PERIOD_NS, callchain);
    let mut event = match SampledEvent::open(&spec, Scope::Process(pid), 512) {
        Ok(event) => event,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            eprintln!(
                "skipped: this machine will not sample this process ({}): {error}",
                paranoid_said()
            );
            return;
        }
    };
    let mut unwinder = Unwinder::new(pid, callchain, None);
    assert!(
        unwinder.refresh_symbols(),
        "the shell's maps are readable while it waits"
    );
    event.enable().unwrap();
    child
        .stdin
        .take()
        .expect("piped")
        .write_all(b"go\n")
        .expect("the shell reads its input");
    let mut ids = Vec::new();
    loop {
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
    event.drain(|record| {
        if let Record::Sample(sample) = record {
            ids.extend(unwinder.record(&sample));
        }
    });
    let (mut in_spin, mut whole) = (0, 0);
    for id in &ids {
        let frames: Vec<String> = unwinder
            .table()
            .get(*id)
            .expect("an id the unwinder gave")
            .frames
            .iter()
            .rev()
            .map(|frame| frame.label())
            .collect();
        if frames.last().map(|leaf| plain(leaf)) != Some("spin") {
            continue;
        }
        in_spin += 1;
        if contains_path(&frames, &["main", "run_batch"]) {
            whole += 1;
        }
    }
    assert!(in_spin >= 100, "only {in_spin} samples in spin");
    assert!(
        whole * 100 >= in_spin * 99,
        "{whole} of {in_spin} samples in spin were unwound to main through the image the \
         target ran after its exec: {:?}",
        unwinder.notes()
    );
}
