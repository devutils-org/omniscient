//! The memory crossing: which allocation cost memory, and did it stay.
//!
//! Four sources answer that between them, and no two of them answer it with the
//! same grade of evidence. A fault is tied to the range it fell inside by a key
//! both carry, and the engine says KEY. Retained bytes are addresses inside
//! ranges the shim recorded as freed, which is a key too, although it is
//! computed inside one collector and never reaches the engine. And a resident
//! size is a total for the process, read on a tick: nothing on it names an
//! allocation, so a rise in it after a range opened is two things happening at
//! the same time, which is TEMPORAL and nothing more.
//!
//! The ground truth is the program, which is the same in both sessions and
//! reports on itself. Each large block prints its address and the minor faults
//! `getrusage` counted across its own write: the kernel's count for exactly that
//! allocation, which the join is held to range by range, whatever transparent
//! huge pages did to it. The sizes and the fates are in its source. And for the
//! arena blocks it frees it prints how many whole pages they cover, which is
//! what a retained scan that counts a shared page as live can find at most.
//!
//! Every tolerance below is a resolution the reference really has, named where
//! it is used: a huge page for a resident size, the interpreter's own churn for
//! the shim's counts. None of them is a threshold fitted to what this machine
//! happened to do.
//!
//! `docs/scenarios/cross-memory.md` is the prose half of this file: the table
//! these checks are made of, and the four defects the crossing found. The last
//! three tests pin three of them rather than route around them; the fourth is
//! the wording of a note in a file another stream owns.

use std::collections::BTreeMap;
use std::process::{Command, Output, Stdio};

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

const MIB: i64 = 1 << 20;
const PAGE: i64 = 4096;

/// What `mem.rss` can move by without the program doing anything: khugepaged
/// collapsing a range into one huge page, which is the caveat the catalog
/// writes on the source.
const HUGE_PAGE: i64 = 2 * MIB;

/// What the interpreter itself allocates, frees and faults between two readings
/// while the program is between phases. Measured here at under 25 KiB; this is
/// sixteen pages, so that a slower interpreter is not mistaken for a wrong tool.
const INTERPRETER: i64 = 16 * PAGE;

/// How long the program is quiet after each phase.
const QUIET_NS: u64 = 300_000_000;

// ---------------------------------------------------------------------------
// Running the tool
// ---------------------------------------------------------------------------

fn example(name: &str) -> String {
    format!("{}/examples/{name}", env!("CARGO_MANIFEST_DIR"))
}

/// Runs a shipped session, in JSON whatever the file says.
///
/// The sessions are written for a reader, in text; the checks read the same
/// run as a document. Running the file rather than a string built here is what
/// keeps the two from drifting.
fn run_example(name: &str) -> Output {
    Command::new(BINARY)
        .args(["-f", &example(name), "--format", "json"])
        .output()
        .expect("omn should run")
}

/// Runs a session that is not shipped, from standard input.
///
/// The three defect sessions are not in `examples/`: each is a rule a reader
/// should not copy, and one of them exits 1 after measuring.
fn run_written(args: &[&str], session: &str) -> Output {
    use std::io::Write;

    let mut child = Command::new(BINARY)
        .args(args)
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

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn code(output: &Output) -> i32 {
    output.status.code().expect("omn exits rather than signals")
}

fn json(output: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout(output)).unwrap_or_else(|error| {
        panic!(
            "a run with `--format json` prints one json document ({error})\nstdout: {}\n\
             stderr: {}",
            stdout(output),
            stderr(output)
        )
    })
}

fn has_python() -> bool {
    Command::new("python3").arg("--version").output().is_ok()
}

/// Why this machine cannot run a scenario, if it cannot.
///
/// A missing shim is not one of the reasons. It is a build that was not run
/// through `make`, and letting it skip would mean the heap half of the crossing
/// measures nothing on exactly the machine where it matters.
fn withheld(output: &Output) -> Option<String> {
    let message = stderr(output);
    assert!(
        !message.contains("heap shim was not found"),
        "the heap sources need the preload shim: build with `make`, not `cargo build`\n{message}"
    );
    let withheld = message.contains("perf_event_paranoid")
        || message.contains("not readable")
        || message.contains("no such file");
    withheld.then_some(message)
}

/// Runs a shipped session, or says loudly why this machine cannot.
fn measured(name: &str) -> Option<Output> {
    if !has_python() {
        eprintln!("SKIPPED {name}: this machine has no python3 to launch the workload");
        return None;
    }
    let output = run_example(name);
    if let Some(reason) = withheld(&output) {
        eprintln!("SKIPPED {name}: this machine withholds a source it needs\n{reason}");
        return None;
    }
    assert!(
        output.status.success(),
        "{name} exited {}:\n{}",
        code(&output),
        stderr(&output)
    );
    Some(output)
}

// ---------------------------------------------------------------------------
// What the program says about itself
// ---------------------------------------------------------------------------

/// One large block, as the program reported it.
#[derive(Debug)]
struct Block {
    fate: String,
    address: u64,
    size: u64,
    /// `ru_minflt` across the block's own write: the kernel's count.
    faults: u64,
}

/// The arena blocks the program freed, as it reported them.
#[derive(Debug)]
struct Arena {
    blocks: i64,
    bytes: i64,
    /// Bytes of the pages lying wholly inside a freed block. A page shared with
    /// a live neighbour is not among them.
    whole_pages: i64,
}

fn blocks(output: &Output) -> Vec<Block> {
    stderr(output)
        .lines()
        .filter_map(|line| line.strip_prefix("BLOCK "))
        .map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            Block {
                fate: fields[0].to_owned(),
                address: fields[1].parse().expect("an address"),
                size: fields[2].parse().expect("a size"),
                faults: fields[3].parse().expect("a fault count"),
            }
        })
        .collect()
}

fn arena(output: &Output) -> Arena {
    let line = stderr(output)
        .lines()
        .find_map(|line| line.strip_prefix("ARENA ").map(str::to_owned))
        .expect("the workload reports the arena blocks it freed");
    let fields: Vec<i64> = line
        .split_whitespace()
        .map(|field| field.parse().expect("a number"))
        .collect();
    Arena {
        blocks: fields[0],
        bytes: fields[1],
        whole_pages: fields[2],
    }
}

// ---------------------------------------------------------------------------
// Reading a run back
// ---------------------------------------------------------------------------

fn result<'a>(run: &'a serde_json::Value, label: &str) -> &'a serde_json::Value {
    run["results"]
        .as_array()
        .expect("a session produces results")
        .iter()
        .find(|result| result["label"] == label)
        .unwrap_or_else(|| panic!("no request labelled `{label}` in the run"))
}

fn raw(result: &serde_json::Value) -> Vec<Reading> {
    result["outputs"]
        .as_array()
        .expect("a result carries its outputs")
        .iter()
        .find(|output| output["emit"] == "raw")
        .expect("the session asks for raw events")["events"]
        .as_array()
        .expect("raw events")
        .iter()
        .map(|event| Reading {
            ts_ns: event["ts_ns"].as_u64().expect("a timestamp"),
            attrs: event["attrs"]
                .as_array()
                .expect("attributes")
                .iter()
                .filter_map(|attr| {
                    Some((attr["name"].as_str()?.to_owned(), attr["value"].as_i64()?))
                })
                .collect(),
        })
        .collect()
}

/// A loss of any kind on a result, with its numbers, or nothing if it saw
/// everything.
fn lossy(result: &serde_json::Value) -> Option<String> {
    let losses = &result["losses"];
    let lost: u64 = ["dropped", "throttled", "late_ticks"]
        .iter()
        .map(|kind| losses[kind].as_u64().unwrap_or(0))
        .sum();
    (lost > 0).then(|| losses.to_string())
}

struct Reading {
    ts_ns: u64,
    attrs: BTreeMap<String, i64>,
}

impl Reading {
    fn get(&self, name: &str) -> i64 {
        *self
            .attrs
            .get(name)
            .unwrap_or_else(|| panic!("a reading without `{name}`: {:?}", self.attrs))
    }
}

/// The last reading taken at or before an instant.
fn at(readings: &[Reading], ts_ns: u64) -> &Reading {
    readings
        .iter()
        .rev()
        .find(|reading| reading.ts_ns <= ts_ns)
        .unwrap_or_else(|| panic!("no reading at or before {ts_ns}"))
}

fn mib(bytes: i64) -> String {
    format!("{:+.2} MiB", bytes as f64 / MIB as f64)
}

// ---------------------------------------------------------------------------
// Which allocation: faults by KEY, residency by TEMPORAL
// ---------------------------------------------------------------------------

/// Every large allocation is found, with the fate the program gave it, and the
/// faults attributed to it are the kernel's own count for its write.
///
/// That count is 515 for an 8 MiB mapping that covers three aligned huge pages
/// and 5 for one that lands where it covers four, and which one a run gets
/// depends on where the mapping lands. The check does not care: the kernel's
/// count is the reference, whatever it is, and a join that attributed by size
/// rather than by address would miss the second kind by two orders of
/// magnitude.
#[test]
fn every_large_allocation_is_found_with_its_fate_and_the_faults_the_kernel_counted_for_it() {
    let Some(output) = measured("cross-memory-which-allocation.yml") else {
        return;
    };
    let run = json(&output);
    let program = blocks(&output);
    assert_eq!(
        program.len(),
        8,
        "the workload reports eight large blocks:\n{}",
        stderr(&output)
    );

    let join = &run["joins"][0];
    assert_eq!(join["rule"], "pages-of-allocation");
    assert_eq!(join["match_kind"], "interval");
    assert_eq!(
        join["evidence"], "KEY",
        "a fault and a range share `mem:[base,len)`, and the join has to say so"
    );

    let faults_lost = lossy(result(&run, "faults"));
    let ranges = join["top"].as_array().expect("attributions");
    let mut table = Vec::new();
    for block in &program {
        let range = ranges
            .iter()
            .find(|range| range["base"].as_u64() == Some(block.address))
            .unwrap_or_else(|| {
                panic!(
                    "the program made a {} block at {:#x} and no range starts there: {ranges:?}",
                    block.fate, block.address
                )
            });
        assert_eq!(
            range["len"].as_u64(),
            Some(block.size),
            "the range at {:#x} is the size the program asked for",
            block.address
        );

        // Kept blocks are never freed; returned ones are, before the run ends.
        let outcome = range["outcome"].as_str().expect("an outcome");
        let expected = if block.fate == "kept" {
            "still_open"
        } else {
            "closed"
        };
        assert_eq!(
            outcome, expected,
            "a {} block at {:#x} has to end the run {expected}",
            block.fate, block.address
        );

        // The kernel counted every fault in the write window, including any the
        // interpreter took on its own pages between the two `getrusage` calls:
        // the join can never find more than that, and on a run that lost
        // nothing it must find all but those.
        let attributed = range["matched"].as_u64().expect("a count");
        assert!(
            attributed <= block.faults,
            "{attributed} faults attributed to the {} block at {:#x}, and the kernel counted only \
             {} across its whole write",
            block.fate,
            block.address,
            block.faults
        );
        if faults_lost.is_none() {
            assert!(
                attributed + 2 >= block.faults,
                "the kernel counted {} faults across the write of the {} block at {:#x}, and the \
                 join attributed {attributed} to it",
                block.faults,
                block.fate,
                block.address
            );
        }
        table.push(format!(
            "  {:#x}  {:>8}  {:>4} faults, the kernel counted {:>4}  {outcome}",
            block.address, block.fate, attributed, block.faults
        ));
    }
    if let Some(losses) = faults_lost {
        eprintln!(
            "NOT CHECKED: that no fault was missed. The fault ring lost events on this run, so \
             the join is a lower bound and only its upper bound was held: {losses}"
        );
    }
    eprintln!("the eight large ranges, evidence KEY\n{}", table.join("\n"));
}

/// The resident size moves by the bytes the program wrote, in the interval the
/// ranges opened or closed in, and that is all it can say.
///
/// This is the TEMPORAL half of the crossing, and the check is phrased the way
/// the claim is: the reading before a phase and the reading after it differ by
/// what the phase wrote, give or take one huge page. Nothing here says which
/// range the bytes belong to, because a total for the process cannot.
#[test]
fn the_resident_size_moves_by_what_each_phase_wrote_in_the_interval_it_ran_in() {
    let Some(output) = measured("cross-memory-which-allocation.yml") else {
        return;
    };
    let run = json(&output);
    let program = blocks(&output);
    let resident = raw(result(&run, "resident"));
    let ranges = run["joins"][0]["top"].as_array().expect("attributions");

    let bounds = |fate: &str, field: &str| -> (u64, u64, i64) {
        let times: Vec<u64> = program
            .iter()
            .filter(|block| block.fate == fate)
            .filter_map(|block| {
                ranges
                    .iter()
                    .find(|range| range["base"].as_u64() == Some(block.address))
            })
            .map(|range| range[field].as_u64().expect("a timestamp"))
            .collect();
        let written: i64 = program
            .iter()
            .filter(|block| block.fate == fate)
            .map(|block| block.size as i64)
            .sum();
        assert_eq!(times.len(), 4, "four {fate} ranges with a `{field}`");
        (
            *times.iter().min().unwrap(),
            *times.iter().max().unwrap(),
            written,
        )
    };

    // Before: the last reading before the first range of the phase. After: a
    // reading inside the quiet that follows the phase, a poll clear of both
    // ends of it. The workload's writes take milliseconds; its quiet is 300.
    let step = |first: u64, last: u64| -> i64 {
        let before = at(&resident, first.saturating_sub(1));
        let after = at(&resident, last + QUIET_NS - 100_000_000);
        after.get("bytes") - before.get("bytes")
    };

    let mut table = Vec::new();
    for (fate, field, sign) in [
        ("kept", "opened_ns", 1),
        ("returned", "opened_ns", 1),
        ("returned", "closed_ns", -1),
    ] {
        let (first, last, written) = bounds(fate, field);
        let moved = step(first, last);
        let expected = sign * written;
        assert!(
            (moved - expected).abs() <= HUGE_PAGE,
            "the {fate} blocks {} {} MiB, and the resident size moved by {} over the interval \
             they did it in",
            if sign > 0 { "wrote" } else { "gave back" },
            written / MIB,
            mib(moved)
        );
        table.push(format!(
            "  {fate:>8} ranges {}  rss {}  (written {})",
            if sign > 0 { "opened" } else { "freed " },
            mib(moved),
            mib(expected)
        ));
    }
    eprintln!(
        "residency beside the ranges, evidence TEMPORAL\n{}",
        table.join("\n")
    );
}

/// The two sessions measure one program, byte for byte.
///
/// They have to be two sessions -- D3 in the document -- and what makes their
/// tables comparable is that the program is the same. A workload edited in one
/// and not the other would make the second half of the crossing about a
/// different program.
#[test]
fn both_halves_of_the_crossing_launch_the_same_program() {
    let program = |name: &str| -> serde_norway::Value {
        let text = std::fs::read_to_string(example(name)).expect("the session ships");
        let session: serde_norway::Value = serde_norway::from_str(&text).expect("yaml");
        let requests = session["requests"].as_sequence().expect("requests");
        let first = requests[0]["target"].clone();
        for request in requests {
            assert_eq!(
                request["target"], first,
                "every request of {name} follows one launch"
            );
        }
        first
    };
    assert_eq!(
        program("cross-memory-which-allocation.yml"),
        program("cross-memory-did-it-stay.yml"),
        "the two halves of the crossing measure different programs"
    );
}

// ---------------------------------------------------------------------------
// Did it stay: retained by address, residency beside it, and the residue
// ---------------------------------------------------------------------------

/// The readings a phase ends on, found by the shim's own count of live bytes.
///
/// The session has no allocation events to align on -- it cannot, D3 -- so the
/// phases are found by the one number the program fixes exactly: the bytes it
/// asked for and has not freed. Each is the last reading of the plateau at that
/// level, taken after the previous one, so a phase whose frees straddle a poll
/// is read once it has settled.
fn settled<'a>(readings: &'a [Reading], levels: &[i64]) -> Vec<&'a Reading> {
    let mut found = Vec::new();
    let mut from = 0;
    for &level in levels {
        let start = readings[from..]
            .iter()
            .position(|reading| (reading.get("live") - level).abs() <= INTERPRETER)
            .unwrap_or_else(|| {
                panic!(
                    "no reading after #{from} with {} live; the live series was {:?}",
                    mib(level),
                    readings.iter().map(|r| r.get("live")).collect::<Vec<_>>()
                )
            })
            + from;
        let end = readings[start..]
            .iter()
            .position(|reading| (reading.get("live") - level).abs() > INTERPRETER)
            .map_or(readings.len(), |offset| start + offset);
        found.push(&readings[end - 1]);
        from = end;
    }
    found
}

/// One `free` returns memory and the other keeps it, and the tool tells them
/// apart by address.
///
/// The four 8 MiB blocks were mappings of their own and `free` unmapped them:
/// the resident size falls by 32 MiB and nothing is retained. The 256 arena
/// blocks sit between live ones and glibc keeps every page: the resident size
/// does not move and the retained bytes rise by exactly the whole pages the
/// program says its freed blocks cover. The separation is the check -- a
/// 32 MiB fall against no rise, and no fall against a 15 MiB rise.
#[test]
fn one_free_gives_memory_back_and_the_other_keeps_it() {
    let Some(output) = measured("cross-memory-did-it-stay.yml") else {
        return;
    };
    let run = json(&output);
    let freed = arena(&output);
    let readings = raw(result(&run, "retained"));

    // The first reading is taken as the program starts, and may be of the
    // process before it became the program.
    let baseline = readings
        .iter()
        .find(|reading| reading.get("live") > 0)
        .expect("a reading of the running program")
        .get("live");
    let levels = [
        baseline,
        baseline + 32 * MIB,
        baseline + 64 * MIB,
        baseline + 32 * MIB,
        baseline + 64 * MIB,
        baseline + 64 * MIB - freed.bytes,
    ];
    let phase = settled(&readings, &levels);
    let delta = |from: usize, to: usize, name: &str| phase[to].get(name) - phase[from].get(name);

    // The mappings, unmapped.
    let (rss, retained) = (delta(2, 3, "rss"), delta(2, 3, "bytes"));
    assert!(
        (rss + 32 * MIB).abs() <= HUGE_PAGE,
        "freeing four 8 MiB mappings has to give 32 MiB back, and the resident size moved by {}",
        mib(rss)
    );
    assert!(
        retained.abs() <= INTERPRETER,
        "an unmapped block cannot be retained, and the retained bytes moved by {}",
        mib(retained)
    );
    let unmapped = format!(
        "  4 mappings freed      rss {}  retained {}",
        mib(rss),
        mib(retained)
    );

    // The arena, kept.
    let (rss, retained) = (delta(4, 5, "rss"), delta(4, 5, "bytes"));
    assert!(
        rss.abs() <= HUGE_PAGE,
        "glibc keeps the pages of an arena block between two live ones, and the resident size \
         moved by {} when {} of them were freed",
        mib(rss),
        freed.blocks
    );
    assert!(
        (retained - freed.whole_pages).abs() <= INTERPRETER,
        "the {} freed arena blocks cover {} of whole pages, and the retained bytes rose by {}",
        freed.blocks,
        mib(freed.whole_pages),
        mib(retained)
    );
    assert!(
        retained <= freed.bytes,
        "more retained ({}) than was freed ({})",
        mib(retained),
        mib(freed.bytes)
    );
    eprintln!(
        "one free, two fates, evidence KEY for retained and TEMPORAL for rss\n{unmapped}\n  {} \
         arena blocks freed  rss {}  retained {}  (whole pages {})",
        freed.blocks,
        mib(rss),
        mib(retained),
        mib(freed.whole_pages)
    );
}

/// The rise in the resident size is the live bytes plus the retained ones plus
/// a residue, and the residue is printed and bounded rather than rounded away.
///
/// All three numbers come off one event, read at one instant of one process, so
/// the sum mixes no clocks and no runs. Every byte the program asked for it
/// wrote, so live bytes are resident bytes here; and the retained scan counts a
/// page shared with a live neighbour as live, so the freed bytes on such pages
/// are in the resident size and in neither part. The program says how many
/// those are. The residue is them, plus mapping headers and the interpreter's
/// own growth, plus at most a huge page the kernel collapsed on its own.
#[test]
fn the_resident_size_is_live_plus_retained_plus_a_residue_that_is_printed() {
    let Some(output) = measured("cross-memory-did-it-stay.yml") else {
        return;
    };
    let run = json(&output);
    let freed = arena(&output);
    let readings = raw(result(&run, "retained"));

    let start = readings
        .iter()
        .find(|reading| reading.get("live") > 0)
        .expect("a reading of the running program");
    let end = readings.last().expect("a last reading");
    let rise = |name: &str| end.get(name) - start.get(name);
    let (rss, live, retained) = (rise("rss"), rise("live"), rise("bytes"));
    let residue = rss - live - retained;
    let predicted = freed.bytes - freed.whole_pages;

    // Four kept mappings of 8 MiB, and the 512 arena blocks less those freed.
    let held = 4 * 8 * MIB + 512 * 64 * 1024 - freed.bytes;
    assert!(
        (live - held).abs() <= INTERPRETER,
        "the program ends holding {} it asked for, and the live bytes rose by {}",
        mib(held),
        mib(live)
    );
    eprintln!(
        "decomposition of the rise in rss over the run, one event at each end\n  \
         rss       {}\n  live      {}\n  retained  {}\n  residue   {}  of which {} is freed bytes on \
         pages shared with a live block, {} unexplained",
        mib(rss),
        mib(live),
        mib(retained),
        mib(residue),
        mib(predicted),
        mib(residue - predicted)
    );
    assert!(
        residue >= -INTERPRETER,
        "a residue of {} means more was accounted for than is resident: every byte the program \
         asked for it wrote",
        mib(residue)
    );
    assert!(
        residue <= predicted + HUGE_PAGE,
        "a residue of {} is more than the {} the program predicts plus one huge page: something \
         resident is in neither part",
        mib(residue),
        mib(predicted)
    );
}

// ---------------------------------------------------------------------------
// The defects the crossing found, pinned
// ---------------------------------------------------------------------------

/// D1, now fixed: a join on `task` credits each reading to one allocation by
/// recency, and says so.
///
/// `eq` on `task` from an allocation to a resident size validates and runs.
/// That the reading and the allocation belong to one process is a shared key,
/// and KEY is right for it -- `tests/join_engine.rs` holds `proc.threads ->
/// proc.fd` to exactly that. What is not KEY is the attribution the output
/// lists: a task names the process, not the allocation, so every allocator call
/// replaces the state before it, only one is ever open, and each reading goes
/// to whichever call came last. The correlation carried KEY alone until the
/// eighth wave's stream J added `within_key`: it now says KEY for the
/// association and TEMPORAL for the crediting (`docs/families/join.md`).
#[test]
fn d1_a_join_on_task_credits_each_reading_to_the_latest_allocation_and_says_temporal_within_it() {
    const SESSION: &str = "\
name: an allocation joined to a resident size by task
format: json
requests:
  - source: mem.heap.alloc
    target: &program {launch: [python3, -c, \"import time; b = bytearray(8 << 20); time.sleep(0.4)\"]}
    mode: trace
    emit: [summary]
  - source: mem.rss
    target: *program
    mode: poll@100ms
    emit: [summary]
joins:
  - id: task-eq
    from: mem.heap.alloc
    to: mem.rss
    via: task
    match: eq
    window: 60s
";
    if !has_python() {
        eprintln!("SKIPPED: this machine has no python3 to launch the workload");
        return;
    }
    let output = run_written(&[], SESSION);
    assert!(output.status.success(), "{}", stderr(&output));
    let join = &json(&output)["joins"][0];

    assert_eq!(
        join["states"]["peak_open"], 1,
        "a task key files every allocation of a process under one state: {join}"
    );
    assert!(
        join["states"]["replaced"].as_u64().unwrap_or(0) > 100,
        "an interpreter makes hundreds of allocator calls, and each replaces the last: {join}"
    );
    assert!(
        join["matched"].as_u64().unwrap_or(0) >= 3,
        "every reading after the first allocation is credited to one of them: {join}"
    );
    assert_eq!(join["evidence"], "KEY", "the two sides do share a task");
    assert_eq!(
        join["within_key"], "TEMPORAL",
        "which replaced state a reading was credited to is the clock's choice, and the \
         correlation has to say so beside the KEY: {join}"
    );
}

/// D2, now fixed: a join on `mem` onto a resident size is refused before
/// anything is measured.
///
/// The catalog used to declare `mem` on `mem.rss`, so the validator accepted
/// the rule, the run was paid for in full, and the engine threw it away at the
/// join -- exit 1 with nothing on standard output. This crossing filed that as
/// D2, the seventh wave's coordinator pass dropped the key from the three
/// entries no collector emits it for, and the refusal is now where
/// `join::check` was always supposed to make it: at validation, naming the keys
/// the source does carry.
#[test]
fn d2_a_join_on_a_key_no_event_carries_is_refused_before_the_run_is_paid_for() {
    const SESSION: &str = "\
name: an allocation joined to a resident size by address
format: json
requests:
  - source: mem.heap.alloc
    target: &program {launch: [python3, -c, \"import time; b = bytearray(8 << 20); time.sleep(0.3)\"]}
    mode: trace
    emit: [summary]
  - source: mem.rss
    target: *program
    mode: poll@100ms
    emit: [summary]
joins:
  - id: pages-of-allocation
    from: mem.heap.alloc
    to: mem.rss
    via: mem
    match: interval
    opens: \"op == 'alloc'\"
    closes: \"op == 'free'\"
    window: 60s
";
    // The validator is the whole test now, and it needs no workload: a rule
    // that cannot match is refused without a machine, which is the difference
    // the fix made.
    let validated = run_written(&["validate"], SESSION);
    assert_eq!(
        code(&validated),
        2,
        "a join on a key the source does not carry is a rejected request: {}{}",
        stdout(&validated),
        stderr(&validated)
    );
    assert!(
        stderr(&validated).contains("which `mem.rss` does not carry"),
        "the refusal names the key: {}",
        stderr(&validated)
    );
    assert!(
        stderr(&validated).contains("`mem.rss` carries task"),
        "and the keys the source does have, so the rule can be rewritten: {}",
        stderr(&validated)
    );

    if !has_python() {
        eprintln!("SKIPPED: this machine has no python3, so the run itself is not checked");
        return;
    }
    // And nothing is measured: the same session run for real is refused with
    // the same code, not after the collectors have stopped.
    let output = run_written(&[], SESSION);
    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(
        stdout(&output).trim().is_empty(),
        "the run was refused and printed results anyway"
    );
}

/// D3. Allocations and retained memory cannot be asked of one launched program,
/// and the refusal blames the request.
///
/// Each shim-backed source opens its own pipe and puts its descriptor in the
/// child's environment, so two of them disagree about `OMN_HEAP_FD` and the run
/// is refused before it starts. That is honest -- neither pipe wins silently --
/// and it exits 2, "the request cannot mean anything", where the request means
/// something and the build cannot serve it: `DECISIONS.md` makes that exit 3.
#[test]
fn d3_two_shim_sources_on_one_launch_are_refused_as_a_request_rather_than_as_the_build() {
    const SESSION: &str = "\
name: allocations and retained memory of one program
format: json
requests:
  - source: mem.heap.alloc
    target: &program {launch: [python3, -c, \"pass\"]}
    mode: trace
    emit: [summary]
  - source: mem.heap.retained
    target: *program
    mode: poll@100ms
    emit: [summary]
";
    let validated = run_written(&["validate"], SESSION);
    assert!(
        validated.status.success(),
        "the session is valid against the catalog: {}",
        stderr(&validated)
    );
    let output = run_written(&[], SESSION);
    assert!(
        stderr(&output).contains("OMN_HEAP_FD"),
        "the refusal names what the two collectors disagree about: {}",
        stderr(&output)
    );
    assert_ne!(
        code(&output),
        0,
        "two pipes for one program cannot both win"
    );
    assert_eq!(
        code(&output),
        2,
        "D3 is fixed, or half fixed. Exit 3 is the build refusing, and a run that succeeds is \
         the shim shared: either way update this assertion, move D3 in \
         docs/scenarios/cross-memory.md, and fold the two sessions of the crossing into one"
    );
}
