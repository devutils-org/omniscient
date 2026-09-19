//! Ground truth for the profile and trace encoders.
//!
//! What an encoder has to get right is not what this tool reads back but what
//! *another* program reads out of it, so the checks here go outside the crate:
//! `gzip` for the container, which verifies the CRC and the length this module
//! wrote into the trailer; a protobuf decoder written against the wire format
//! rather than against the encoder; `go tool pprof` itself, which is the
//! program the format exists for; and the Chrome Trace Event Format's own rules
//! for the trace. A test that decoded a file with the encoder's own vocabulary
//! would pass on a file no tool can open.
//!
//! The profiles and timelines come from real runs. The tool is asked for JSON,
//! and the JSON is read back into the `RunOutput` the encoders take, so what is
//! round-tripped is what a collector measured rather than what a test invented.

use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Output, Stdio};

use omn::encode::{self, RunOutput};
use omn::format::OutputFormat;

const BINARY: &str = env!("CARGO_BIN_EXE_omn");
const MIB: f64 = 1024.0 * 1024.0;

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

fn has_python() -> bool {
    Command::new("python3").arg("--version").output().is_ok()
}

fn has_go() -> bool {
    Command::new("go").arg("version").output().is_ok()
}

fn scratch(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("omn-encoders-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&path).expect("a directory to write into");
    path
}

/// Runs the bytes through the system `gzip`, which checks the CRC-32 and the
/// uncompressed length in the trailer and refuses a stream whose blocks do not
/// add up. A machine without `gzip` is a broken machine rather than one that
/// cannot serve this, so it fails rather than skipping.
fn gunzip(bytes: &[u8]) -> Vec<u8> {
    let mut child = Command::new("gzip")
        .arg("-dc")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("gzip is a coreutil and these tests need it to check the container");
    child
        .stdin
        .as_mut()
        .expect("stdin is piped")
        .write_all(bytes)
        .expect("writing the stream");
    let output = child.wait_with_output().expect("gzip should finish");
    assert!(
        output.status.success(),
        "gzip refused the stream this encoder wrote: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

// ---------------------------------------------------------------------------
// A protobuf reader, written against the wire format and not against the
// encoder. Three wire types are enough for `profile.proto`: varints, which
// carry every number, length-delimited fields, which carry strings, nested
// messages and packed repeated numbers, and nothing else.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Field {
    Varint(u64),
    Bytes(Vec<u8>),
}

impl Field {
    fn varint(&self) -> u64 {
        match self {
            Field::Varint(value) => *value,
            other => panic!("expected a varint, found {other:?}"),
        }
    }

    fn bytes(&self) -> &[u8] {
        match self {
            Field::Bytes(value) => value,
            other => panic!("expected a length-delimited field, found {other:?}"),
        }
    }

    fn text(&self) -> String {
        String::from_utf8(self.bytes().to_vec()).expect("a UTF-8 string")
    }

    /// A packed repeated numeric field, which is a run of varints in a
    /// length-delimited field.
    fn packed(&self) -> Vec<u64> {
        let body = self.bytes();
        let mut values = Vec::new();
        let mut at = 0;
        while at < body.len() {
            values.push(read_varint(body, &mut at));
        }
        values
    }
}

fn read_varint(body: &[u8], at: &mut usize) -> u64 {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
        let byte = body[*at];
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return value;
        }
        shift += 7;
        assert!(shift < 64, "a varint longer than a u64");
    }
}

/// Every field of one message, in the order it was written, keyed by number.
fn message(body: &[u8]) -> BTreeMap<u32, Vec<Field>> {
    let mut fields: BTreeMap<u32, Vec<Field>> = BTreeMap::new();
    let mut at = 0;
    while at < body.len() {
        let tag = read_varint(body, &mut at);
        let number = (tag >> 3) as u32;
        let value = match tag & 7 {
            0 => Field::Varint(read_varint(body, &mut at)),
            2 => {
                let len = read_varint(body, &mut at) as usize;
                let value = Field::Bytes(body[at..at + len].to_vec());
                at += len;
                value
            }
            5 => {
                let value = Field::Varint(u64::from(u32::from_le_bytes(
                    body[at..at + 4].try_into().unwrap(),
                )));
                at += 4;
                value
            }
            1 => {
                let value = Field::Varint(u64::from_le_bytes(body[at..at + 8].try_into().unwrap()));
                at += 8;
                value
            }
            other => panic!("wire type {other} has no place in profile.proto"),
        };
        fields.entry(number).or_default().push(value);
    }
    fields
}

fn only(fields: &BTreeMap<u32, Vec<Field>>, number: u32) -> Option<&Field> {
    fields.get(&number).map(|values| {
        assert_eq!(values.len(), 1, "field {number} was written more than once");
        &values[0]
    })
}

fn repeated(fields: &BTreeMap<u32, Vec<Field>>, number: u32) -> &[Field] {
    fields.get(&number).map(Vec::as_slice).unwrap_or(&[])
}

/// A decoded `profile.proto`: the parts this tool writes, resolved through the
/// string table so a test can compare them with what went in.
struct Profile {
    strings: Vec<String>,
    sample_types: Vec<(String, String)>,
    /// Root-first frame names, the value column, and the `events` label.
    samples: Vec<(Vec<String>, Vec<i64>, Option<i64>)>,
    duration_nanos: u64,
    comments: Vec<String>,
    default_sample_type: String,
}

fn decode_profile(gzipped: &[u8]) -> Profile {
    let profile = message(&gunzip(gzipped));

    let strings: Vec<String> = repeated(&profile, 6).iter().map(Field::text).collect();
    assert_eq!(
        strings.first().map(String::as_str),
        Some(""),
        "profile.proto reserves index zero of the string table for the empty string"
    );
    let string = |index: u64| -> String {
        strings
            .get(index as usize)
            .unwrap_or_else(|| panic!("string index {index} is past a table of {}", strings.len()))
            .clone()
    };

    let sample_types: Vec<(String, String)> = repeated(&profile, 1)
        .iter()
        .map(|field| {
            let value_type = message(field.bytes());
            (
                string(only(&value_type, 1).map(Field::varint).unwrap_or(0)),
                string(only(&value_type, 2).map(Field::varint).unwrap_or(0)),
            )
        })
        .collect();

    // Functions and locations, resolved into names so a sample can be read as
    // the chain of frames it is.
    let mut function_name: BTreeMap<u64, String> = BTreeMap::new();
    for field in repeated(&profile, 5) {
        let function = message(field.bytes());
        let id = only(&function, 1).map(Field::varint).unwrap_or(0);
        assert!(id > 0, "profile.proto numbers functions from one");
        let name = string(only(&function, 2).map(Field::varint).unwrap_or(0));
        assert!(
            function_name.insert(id, name).is_none(),
            "function id {id} was written twice"
        );
    }
    let mut location_name: BTreeMap<u64, String> = BTreeMap::new();
    for field in repeated(&profile, 4) {
        let location = message(field.bytes());
        let id = only(&location, 1).map(Field::varint).unwrap_or(0);
        assert!(id > 0, "profile.proto numbers locations from one");
        let lines = repeated(&location, 4);
        assert_eq!(lines.len(), 1, "location {id} does not name one function");
        let line = message(lines[0].bytes());
        let function = only(&line, 1).map(Field::varint).unwrap_or(0);
        let name = function_name
            .get(&function)
            .unwrap_or_else(|| {
                panic!("location {id} points at function {function}, which is not in the table")
            })
            .clone();
        location_name.insert(id, name);
    }

    let samples = repeated(&profile, 2)
        .iter()
        .map(|field| {
            let sample = message(field.bytes());
            let chain: Vec<String> = only(&sample, 1)
                .map(Field::packed)
                .unwrap_or_default()
                .iter()
                .rev()
                .map(|id| {
                    location_name
                        .get(id)
                        .unwrap_or_else(|| {
                            panic!("a sample names location {id}, which is not in the table")
                        })
                        .clone()
                })
                .collect();
            let values: Vec<i64> = only(&sample, 2)
                .map(Field::packed)
                .unwrap_or_default()
                .iter()
                .map(|value| *value as i64)
                .collect();
            assert_eq!(
                values.len(),
                sample_types.len(),
                "pprof requires one value per sample type"
            );
            let events = repeated(&sample, 3).first().map(|field| {
                let label = message(field.bytes());
                assert_eq!(
                    string(only(&label, 1).map(Field::varint).unwrap_or(0)),
                    "events"
                );
                only(&label, 3).map(Field::varint).unwrap_or(0) as i64
            });
            (chain, values, events)
        })
        .collect();

    Profile {
        duration_nanos: only(&profile, 10).map(Field::varint).unwrap_or(0),
        comments: only(&profile, 13)
            .map(Field::packed)
            .unwrap_or_default()
            .iter()
            .map(|index| string(*index))
            .collect(),
        default_sample_type: string(only(&profile, 14).map(Field::varint).unwrap_or(0)),
        strings,
        sample_types,
        samples,
    }
}

/// The frames a run's own results carry for one stack identity, root first, in
/// the spelling `folded` and this decoder both use.
fn frames_of(run: &RunOutput, results_index: usize, stack: u64) -> Vec<String> {
    run.results[results_index]
        .stacks
        .get(&stack)
        .cloned()
        .unwrap_or_else(|| vec![format!("[unresolved stack {stack:#x}]")])
}

/// A CPU-bound python loop, profiled for real. The interpreter's own evaluation
/// frames are what a profile of it has to contain, and they come from the
/// target's symbol table rather than from anything this tool decided.
fn cpu_profile() -> RunOutput {
    let output = run(&[
        "cpu.profile",
        "-m",
        "sample@997hz",
        "-e",
        "stack",
        "--format",
        "json",
        "--",
        "python3",
        "-c",
        "x = 0\nfor i in range(12_000_000): x += i",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let run: RunOutput = serde_json::from_str(&stdout(&output)).expect("one json document");
    let stacks = match &run.results[0].outputs[0] {
        omn::emit::Output::Stack { stacks, .. } => stacks.len(),
        other => panic!("a stack emit produced {other:?}"),
    };
    assert!(
        stacks > 0,
        "the profile is empty, so nothing below is testing an encoder: {:?}",
        run.results[0]
    );
    run
}

/// Ground truth for the protobuf: every stack the collector measured is in the
/// file, with the frames it had and the weight it had, read back by a decoder
/// that knows the wire format and nothing about this crate.
#[test]
fn a_real_profile_survives_the_round_trip_through_profile_proto() {
    if !has_python() {
        eprintln!("skipped: this machine has no python3 to profile");
        return;
    }
    let run = cpu_profile();
    let bytes = encode::run_output_bytes(OutputFormat::Pprof, &run).expect("a pprof file");
    let profile = decode_profile(&bytes);

    assert_eq!(
        profile.sample_types,
        vec![("cpu.profile".to_owned(), "count".to_owned())],
        "one column per request, named as the request was and in the source's unit"
    );
    assert_eq!(profile.default_sample_type, "cpu.profile");
    assert_eq!(profile.duration_nanos, run.results[0].observed_ns);

    let omn::emit::Output::Stack { stacks, .. } = &run.results[0].outputs[0] else {
        unreachable!("cpu_profile asserted this");
    };
    assert_eq!(
        profile.samples.len(),
        stacks.len(),
        "every stack is a sample and no sample is invented"
    );
    for (measured, (chain, values, events)) in stacks.iter().zip(&profile.samples) {
        assert_eq!(
            *chain,
            frames_of(&run, 0, measured.stack),
            "the frames in the file are not the frames that were measured"
        );
        assert_eq!(values, &vec![measured.total.round() as i64]);
        assert_eq!(*events, Some(measured.events as i64));
    }

    // The profile is of an interpreter running a loop, so the interpreter's own
    // frames have to be in it. The names come from python's symbol table.
    assert!(
        profile.strings.iter().any(|name| name.contains("Py")),
        "a profile of python has no python frame in it: {:?}",
        &profile.strings[..profile.strings.len().min(20)]
    );

    // Losses are counted and surfaced to the user, always -- including into a
    // format whose only place for a sentence is the comment list.
    assert!(
        profile
            .comments
            .iter()
            .any(|line| line.contains("source cpu.profile") && line.contains("via ")),
        "the file does not say which backend produced it: {:?}",
        profile.comments
    );
    for degradation in &run.results[0].degradations {
        assert!(
            profile
                .comments
                .iter()
                .any(|line| line.contains(degradation)),
            "a degradation was dropped on the way into the file: {degradation}"
        );
    }
}

/// The program the format exists for. `go tool pprof` refuses a file whose
/// samples, locations or value columns do not line up, and the totals it
/// computes are its own arithmetic over what it decoded -- so agreeing with
/// them is a check on the file rather than on this crate.
#[test]
fn go_tool_pprof_reads_the_file_and_agrees_with_what_was_measured() {
    if !has_python() {
        eprintln!("skipped: this machine has no python3 to profile");
        return;
    }
    if !has_go() {
        eprintln!(
            "skipped: this machine has no `go`, so the one independent pprof decoder is absent. \
             `a_real_profile_survives_the_round_trip_through_profile_proto` still checks the file."
        );
        return;
    }

    let run = cpu_profile();
    let bytes = encode::run_output_bytes(OutputFormat::Pprof, &run).expect("a pprof file");
    let path = scratch("pprof").join("profile.pb.gz");
    std::fs::write(&path, &bytes).expect("writing the profile");

    let output = Command::new("go")
        .args(["tool", "pprof", "-top", "-nodecount=10"])
        .arg(&path)
        .output()
        .expect("go tool pprof should run");
    let report = format!("{}{}", stdout(&output), stderr(&output));
    assert!(
        output.status.success(),
        "go tool pprof refused it: {report}"
    );

    let omn::emit::Output::Stack { stacks, .. } = &run.results[0].outputs[0] else {
        unreachable!("cpu_profile asserted this");
    };
    let total: i64 = stacks.iter().map(|stack| stack.total.round() as i64).sum();
    assert!(
        report.contains(&format!("Total samples = {total}")),
        "pprof added the file up to something else than what was measured ({total}): {report}"
    );
    assert!(
        report.contains("Type: cpu.profile"),
        "the sample type did not survive: {report}"
    );
    assert!(
        report.contains("Py"),
        "pprof resolved no python frame out of a profile of python: {report}"
    );
}

/// The stored-block boundary. A gzip stream carries at most 65 535 bytes per
/// uncompressed block, and a profile with enough distinct frames crosses it --
/// which is the one thing about this container that cannot be got right by
/// accident. The check is that `gzip` accepts the result and `pprof` still
/// reads the whole of it.
#[test]
fn a_profile_larger_than_one_gzip_block_comes_back_whole() {
    let mut results = omn::emit::Results {
        label: "wide".into(),
        source: "cpu.profile".into(),
        unit: omn::catalog::Unit::Count,
        backend: "pmu".into(),
        outputs: Vec::new(),
        events_seen: 0,
        observed_ns: 1_000_000_000,
        stopped_because: "the requested duration elapsed".into(),
        losses: Default::default(),
        filtered: None,
        unverified: Vec::new(),
        degradations: Vec::new(),
        stacks: Default::default(),
    };
    let mut stacks = Vec::new();
    // Frame names long enough that the string table alone passes 64 KiB.
    for id in 0..2_000u64 {
        results.stacks.insert(
            id,
            vec![
                "root".to_owned(),
                format!("a_function_with_a_name_long_enough_to_fill_a_block_{id:08}"),
            ],
        );
        stacks.push(omn::emit::StackCount {
            stack: id,
            events: 1,
            total: (id + 1) as f64,
        });
    }
    results.events_seen = stacks.len();
    results.outputs = vec![omn::emit::Output::Stack {
        stacks,
        unattributed: 0,
    }];
    let run = RunOutput {
        results: vec![results],
        joins: Vec::new(),
    };

    let bytes = encode::run_output_bytes(OutputFormat::Pprof, &run).expect("a pprof file");
    assert!(
        gunzip(&bytes).len() > 0xffff,
        "this profile no longer crosses a block boundary, so it is not testing one"
    );
    let profile = decode_profile(&bytes);
    assert_eq!(profile.samples.len(), 2_000);
    assert_eq!(profile.samples[1_999].1, vec![2_000]);
    assert_eq!(
        profile.samples[1_999].0[1],
        "a_function_with_a_name_long_enough_to_fill_a_block_00001999"
    );
}

/// pprof scales and labels an axis from a fixed set of unit names, and this
/// tool's units do not all map onto them. Where they do not, the file says so
/// rather than borrowing the nearest name pprof would recognize: energy
/// reported as `count` renders as a count of nothing.
#[test]
fn a_unit_pprof_does_not_know_is_written_under_its_own_name_and_said_so() {
    let mut results = omn::emit::Results {
        label: "draw".into(),
        source: "power.energy".into(),
        unit: omn::catalog::Unit::Joules,
        backend: "pmu:rapl".into(),
        outputs: vec![omn::emit::Output::Stack {
            stacks: vec![omn::emit::StackCount {
                stack: 1,
                events: 4,
                total: 145.0,
            }],
            unattributed: 0,
        }],
        events_seen: 4,
        observed_ns: 1_000_000_000,
        stopped_because: "the requested duration elapsed".into(),
        losses: Default::default(),
        filtered: None,
        unverified: Vec::new(),
        degradations: Vec::new(),
        stacks: Default::default(),
    };
    results.stacks.insert(1, vec!["main".into(), "spin".into()]);
    let run = RunOutput {
        results: vec![results],
        joins: Vec::new(),
    };

    let profile = decode_profile(&encode::run_output_bytes(OutputFormat::Pprof, &run).unwrap());
    assert_eq!(
        profile.sample_types,
        vec![("draw".to_owned(), "joules".to_owned())]
    );
    assert!(
        profile
            .comments
            .iter()
            .any(|line| line.contains("`joules` is not a unit pprof knows")),
        "the file borrowed a unit without saying so: {:?}",
        profile.comments
    );

    // And a unit pprof does know is written under pprof's name for it, with no
    // caveat, because there is nothing to caveat.
    let mut counted = run.clone();
    counted.results[0].unit = omn::catalog::Unit::Ns;
    let profile = decode_profile(&encode::run_output_bytes(OutputFormat::Pprof, &counted).unwrap());
    assert_eq!(profile.sample_types[0].1, "nanoseconds");
    assert!(
        !profile
            .comments
            .iter()
            .any(|line| line.contains("not a unit")),
        "{:?}",
        profile.comments
    );
}

// ---------------------------------------------------------------------------
// Chrome Trace Event Format
// ---------------------------------------------------------------------------

/// The format's own rules, checked on every event: chrome and Perfetto both
/// reject a trace that breaks them, and the ones below are the rules this tool
/// can break -- a phase nothing defines, an instant event with no scope, a
/// counter whose value is not a number.
fn check_trace(document: &serde_json::Value) {
    let events = document["traceEvents"]
        .as_array()
        .unwrap_or_else(|| panic!("a trace is a `traceEvents` array: {document}"));
    assert!(!events.is_empty(), "an empty trace was written");
    for event in events {
        let phase = event["ph"].as_str().unwrap_or_else(|| panic!("{event}"));
        assert!(
            event["name"].is_string(),
            "every event this tool writes is named: {event}"
        );
        assert!(
            event["pid"].is_u64(),
            "chrome keys a track by process: {event}"
        );
        match phase {
            "M" => {
                assert!(
                    ["process_name", "process_labels", "thread_name"]
                        .contains(&event["name"].as_str().unwrap()),
                    "an unknown metadata event: {event}"
                );
                assert!(event["args"].is_object(), "{event}");
            }
            "C" => {
                let args = event["args"]
                    .as_object()
                    .unwrap_or_else(|| panic!("a counter carries its series in args: {event}"));
                assert!(!args.is_empty(), "a counter with no series: {event}");
                for (name, value) in args {
                    let value = value
                        .as_f64()
                        .unwrap_or_else(|| panic!("`{name}` is not a number: {event}"));
                    assert!(value.is_finite(), "`{name}` is not finite: {event}");
                }
                assert!(event["ts"].as_f64().is_some_and(f64::is_finite), "{event}");
            }
            "i" => {
                assert!(
                    ["g", "p", "t"].contains(&event["s"].as_str().unwrap_or("")),
                    "an instant event needs a scope: {event}"
                );
                assert!(event["tid"].is_u64(), "{event}");
                assert!(event["ts"].as_f64().is_some_and(f64::is_finite), "{event}");
            }
            other => panic!("`{other}` is not a phase this tool writes: {event}"),
        }
    }

    // Every thread an event names has to be named by a metadata event, or the
    // trace opens with rows nobody can read.
    let named: Vec<(u64, u64)> = events
        .iter()
        .filter(|event| event["name"] == "thread_name")
        .map(|event| {
            (
                event["pid"].as_u64().unwrap(),
                event["tid"].as_u64().unwrap(),
            )
        })
        .collect();
    for event in events {
        if event["ph"] == "i" {
            let thread = (
                event["pid"].as_u64().unwrap(),
                event["tid"].as_u64().unwrap(),
            );
            assert!(named.contains(&thread), "thread {thread:?} has no name");
        }
    }
}

/// Ground truth for the trace: a known allocation, made at a known moment, has
/// to show up in the counter track at that moment and at that size.
#[test]
fn a_known_allocation_lands_on_the_chrome_trace_at_the_time_it_was_made() {
    if !has_python() {
        eprintln!("skipped: this machine has no python3 to launch");
        return;
    }
    const ALLOCATED: f64 = 64.0 * MIB;
    const AFTER_MS: f64 = 300.0;

    let output = run(&[
        "mem.rss",
        "-m",
        "poll@25ms",
        "-e",
        "timeline",
        "--format",
        "chrome",
        "--",
        "python3",
        "-c",
        "import time\ntime.sleep(0.3)\nb = bytearray(64 * 1024 * 1024)\ntime.sleep(0.3)",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let document: serde_json::Value =
        serde_json::from_str(&stdout(&output)).expect("chrome's own JSON");
    check_trace(&document);

    let counters: Vec<(f64, f64)> = document["traceEvents"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["ph"] == "C")
        .map(|event| {
            let value = event["args"]["mem.rss"]
                .as_f64()
                .unwrap_or_else(|| panic!("the series is named after the request: {event}"));
            (event["ts"].as_f64().unwrap(), value)
        })
        .collect();
    assert!(
        counters.len() > 10,
        "a 600 ms run polled at 25 ms produced {} points",
        counters.len()
    );
    assert!(
        counters.windows(2).all(|pair| pair[0].0 <= pair[1].0),
        "a trace has to be in time order: {counters:?}"
    );

    // The step. Before the allocation the process holds whatever the
    // interpreter needs; after it, 64 MiB more. Both halves are asserted,
    // because a series that was 64 MiB high all along measured nothing.
    let before: Vec<f64> = counters
        .iter()
        .filter(|(at, _)| *at < AFTER_MS * 1000.0)
        .map(|(_, value)| *value)
        .collect();
    let after: Vec<f64> = counters
        .iter()
        .filter(|(at, _)| *at > (AFTER_MS + 100.0) * 1000.0)
        .map(|(_, value)| *value)
        .collect();
    assert!(!before.is_empty() && !after.is_empty(), "{counters:?}");
    let peak_before = before.iter().cloned().fold(f64::MIN, f64::max);
    let floor_after = after.iter().cloned().fold(f64::MAX, f64::min);
    assert!(
        floor_after - peak_before > ALLOCATED * 0.9,
        "the 64 MiB allocation is not the step in the trace: {peak_before} before, \
         {floor_after} after"
    );
    assert!(
        peak_before < ALLOCATED,
        "the process was already holding the allocation before it was made: {peak_before}"
    );

    // Provenance, which a trace viewer has nowhere to draw and a reader of the
    // file still needs.
    let other = &document["otherData"];
    assert_eq!(other["clock"], "CLOCK_MONOTONIC");
    assert_eq!(other["requests"][0]["source"], "mem.rss");
    assert!(other["requests"][0]["losses"].is_object(), "{other}");
}

/// Raw events are the other emit chrome accepts, and they arrive as instants on
/// a thread of their own key. What a raw event carried has to survive: the
/// attributes are why a reader asked for `raw` in the first place.
#[test]
fn raw_events_reach_the_trace_with_what_they_measured() {
    let pid = std::process::id().to_string();
    let output = run(&[
        "mem.rss",
        "--pid",
        &pid,
        "-m",
        "poll@50ms",
        "-d",
        "300ms",
        "-e",
        "raw",
        "--format",
        "chrome",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let document: serde_json::Value =
        serde_json::from_str(&stdout(&output)).expect("chrome's own JSON");
    check_trace(&document);

    let instants: Vec<&serde_json::Value> = document["traceEvents"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["ph"] == "i")
        .collect();
    assert!(instants.len() >= 4, "a 300 ms run at 50 Hz: {instants:?}");
    assert_eq!(instants[0]["name"], "mem.rss");
    assert!(
        instants[0]["args"]["bytes"].as_f64().unwrap_or(0.0) > 0.0,
        "the measurement did not survive into the trace: {}",
        instants[0]
    );
    assert_eq!(
        instants[0]["args"]["key.task"],
        serde_json::Value::from(format!("task:{pid}")),
        "an event's keys are what it can be joined on, and they belong in the trace too: {}",
        instants[0]
    );
    // The axis starts where the run starts, not at the machine's uptime.
    assert_eq!(instants[0]["ts"].as_f64(), Some(0.0));
}

/// The matrix in `format.rs` is a claim about what a file can mean. A profile
/// format cannot hold a timeline and a trace format cannot hold a profile, and
/// both refusals name the emit rather than approximating it.
#[test]
fn the_two_new_formats_refuse_the_emits_they_cannot_express() {
    // Each pair asks a source that genuinely declares the emit, so the refusal
    // is the format's and not the catalog's.
    for (source, mode, emit, format) in [
        ("mem.rss", "poll@1s", "timeline", "pprof"),
        ("cpu.profile", "sample", "stack", "chrome"),
    ] {
        let output = run(&[
            source, "--pid", "1", "-m", mode, "-e", emit, "--format", format,
        ]);
        assert!(
            !output.status.success(),
            "`--emit {emit} --format {format}` was accepted: {}",
            stdout(&output)
        );
        let message = stderr(&output);
        assert!(
            message.contains("cannot carry") && message.contains(emit),
            "the refusal does not name the emit: {message}"
        );
    }
}
