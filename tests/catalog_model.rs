//! The catalog model: what a *backend* can serve, rather than what an entry
//! promises across its whole chain.
//!
//! An entry describes a source; a chain describes several mechanisms for
//! obtaining it, and those mechanisms do not agree about who they can attribute
//! to or what they can be filtered on. `/proc/diskstats` counts the machine and
//! `io.stat` counts a cgroup; `/proc/net/dev` counts an interface and an eBPF
//! program on `sock_sendmsg` counts a process. Letting either stand in for the
//! other produces a number about something else, presented under the name of
//! the thing that was asked for.
//!
//! What is checked here is checked against the kernel's own files wherever a
//! claim can be: `/sys/block/*/dev` and `/proc/diskstats` for what a device is
//! called, `/proc/net/dev` for which columns exist, `/sys/devices/system/cpu`
//! for how many cpus there are.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Output};

use omn::capabilities::Capabilities;
use omn::catalog::{Catalog, TargetSupport};
use omn::event::KeyValue;
use omn::request::{Emit, KeyKind, Mode, Request, SourceId, Target, TargetKind};
use omn::resolve::{resolve, ResolveError};

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

/// A request was refused because it cannot mean what it asks for.
const EXIT_REJECTED: i32 = 2;
/// A request that means something, on a machine that can serve it, through a
/// backend this build has no collector for.
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

fn catalog() -> Catalog {
    Catalog::embedded().expect("the embedded catalog must load")
}

fn request(source: &str, target: Target) -> Request {
    Request {
        source: source.parse::<SourceId>().expect("a catalog id"),
        target,
        mode: Mode::Poll {
            interval: Mode::DEFAULT_POLL_INTERVAL,
        },
        emit: vec![Emit::Raw],
        keys: Vec::new(),
        filter: None,
        duration: None,
        budget: None,
        stack: None,
        symbols: None,
        backend: None,
        label: None,
        window: None,
    }
}

/// A target of the right shape for a kind, so the whole catalog can be swept.
fn some_target(kind: TargetKind) -> Target {
    match kind {
        TargetKind::Launch => Target::Launch(vec!["/bin/true".to_owned()]),
        TargetKind::Pid => Target::Pid(vec![std::process::id() as i32]),
        TargetKind::Tid => Target::Tid(vec![std::process::id() as i32]),
        TargetKind::Cgroup => Target::Cgroup("/sys/fs/cgroup".into()),
        TargetKind::Cpu => Target::Cpu("0".parse().expect("cpu 0 exists")),
        TargetKind::Iface => Target::Iface("lo".to_owned()),
        TargetKind::Container => Target::Container("none".to_owned()),
        TargetKind::System => Target::System,
    }
}

// ---------------------------------------------------------------------------
// The case the brief names: two scopes, two files, two backends.
// ---------------------------------------------------------------------------

/// `io.bytes` can be read from `/proc/diskstats`, which knows the machine and
/// no cgroup, or from `io.stat`, which knows a cgroup and no machine. Before
/// a backend could say which targets it serves, the resolver walked the chain
/// on availability alone and handed a `--cgroup` request to diskstats: the
/// whole machine's I/O, reported under one cgroup's name.
///
/// Ground truth is the two files themselves. Their scopes are a property of
/// the kernel, not of this tool, so the test reads them and asserts the
/// resolver's choice follows.
#[test]
fn a_system_request_and_a_cgroup_request_resolve_onto_different_backends() {
    assert!(
        Path::new("/proc/diskstats").exists(),
        "every Linux machine has /proc/diskstats; without it this test would be \
         checking nothing"
    );

    let system = resolved_backend(&["io.bytes", "--system"]);
    let cgroup = resolved_backend(&["io.bytes", "--cgroup", "/sys/fs/cgroup"]);

    assert_ne!(
        system, cgroup,
        "one file counts the machine and the other counts a cgroup: the same \
         backend cannot be the right answer to both"
    );
    assert_eq!(
        system, "fs:proc",
        "a system-wide byte count comes from /proc/diskstats"
    );
    assert_ne!(
        cgroup, "fs:proc",
        "/proc/diskstats has no notion of a cgroup, so answering a --cgroup \
         request from it reports the whole machine under the cgroup's name"
    );
}

/// The refusal has to say *what* the passed-over backend would have answered
/// with. "no viable backend" sends a reader looking for a missing privilege;
/// "it would answer with a number scoped to system" names the actual reason.
#[test]
fn the_chain_says_what_the_backend_it_passed_over_would_have_answered() {
    let dumped = run(&[
        "io.bytes",
        "--cgroup",
        "/sys/fs/cgroup",
        "-m",
        "poll",
        "-e",
        "timeline",
        "--dump-resolved",
        "--format",
        "json",
    ]);
    assert!(dumped.status.success(), "{}", stderr(&dumped));
    let document: serde_json::Value =
        serde_json::from_str(&stdout(&dumped)).expect("the resolved record parses");

    let diskstats = document[0]["chain"]
        .as_array()
        .expect("the chain is recorded")
        .iter()
        .find(|considered| considered["id"] == "fs:proc")
        .expect("the chain names every backend it considered")
        .clone();

    let detail = diskstats["verdict"]["detail"].to_string();
    assert!(
        detail.contains("cannot attribute per cgroup"),
        "the record has to say why diskstats was passed over: {diskstats}"
    );
    assert!(
        detail.contains("scoped to system"),
        "and what it would have answered with instead: {diskstats}"
    );
}

/// The invariant behind the two above, stated as a number rather than as a
/// backend name: the machine-wide byte count must never be what a `--cgroup`
/// request returns.
///
/// The system side has to succeed for this to mean anything -- a suite in which
/// both requests fail would pass it while checking nothing -- so that is
/// asserted first.
#[test]
fn a_cgroup_request_is_never_answered_with_the_machines_own_number() {
    let system = run(&[
        "io.bytes",
        "--system",
        "-m",
        "poll@100ms",
        "-e",
        "summary",
        "-d",
        "300ms",
        "--format",
        "json",
    ]);
    assert!(
        system.status.success(),
        "a system-wide byte count is servable from /proc/diskstats here: {}",
        stderr(&system)
    );
    let document: serde_json::Value =
        serde_json::from_str(&stdout(&system)).expect("the json output parses");
    assert_eq!(document["results"][0]["backend"], "fs:proc");

    let cgroup = run(&[
        "io.bytes",
        "--cgroup",
        "/sys/fs/cgroup",
        "-m",
        "poll@100ms",
        "-e",
        "summary",
        "-d",
        "300ms",
        "--format",
        "json",
    ]);
    if cgroup.status.success() {
        let document: serde_json::Value =
            serde_json::from_str(&stdout(&cgroup)).expect("the json output parses");
        assert_ne!(
            document["results"][0]["backend"], "fs:proc",
            "a cgroup's I/O was answered with the machine's"
        );
        return;
    }

    // No collector serves the cgroup backend in this build. That is a
    // different answer from "this machine cannot serve it", and the exit code
    // has to tell them apart: refusing here is correct, and refusing for the
    // wrong reason would not be.
    assert_eq!(
        cgroup.status.code(),
        Some(EXIT_NO_COLLECTOR),
        "the request means something and this machine can serve it; what is \
         missing is a collector: {}",
        stderr(&cgroup)
    );
    assert!(
        stdout(&cgroup).is_empty(),
        "no number at all beats the wrong one: {}",
        stdout(&cgroup)
    );
}

// ---------------------------------------------------------------------------
// The sweep: no backend is ever handed a target it cannot attribute to.
// ---------------------------------------------------------------------------

/// Whatever the chain, whatever the machine, the backend the resolver settles
/// on must be one that can attribute at the granularity that was asked for.
///
/// This is the invariant the per-backend `targets` map exists to enforce, and
/// it is swept over every entry rather than spot-checked: the bug it prevents
/// was found in one source and was latent in several.
#[test]
fn the_resolver_never_selects_a_backend_that_cannot_serve_the_target() {
    let catalog = catalog();
    let capabilities = Capabilities::detect();
    let mut checked = 0;
    let mut withheld = 0;

    for entry in catalog.entries() {
        for kind in entry.supported_targets() {
            let resolved = match resolve(
                &catalog,
                &capabilities,
                &request(entry.id.as_str(), some_target(kind)),
            ) {
                Ok(resolved) => resolved,
                // A machine that cannot serve any backend of a chain is a
                // legitimate outcome here and says nothing about the model.
                Err(ResolveError::NoViableBackend { .. }) => {
                    withheld += 1;
                    continue;
                }
                // A target served only in a mode other than the one asked
                // (net.hwts per launch is `trace` alone) is refused before
                // any backend is selected, so there is nothing to check.
                Err(ResolveError::NoBackendDeclares { .. }) => continue,
                Err(other) => panic!("`{}` per {kind}: {other}", entry.id),
            };
            let backend = entry
                .backend(&resolved.backend.id)
                .unwrap_or_else(|| panic!("`{}` selected an unknown backend", entry.id));
            assert_ne!(
                entry.backend_target_support(backend, kind),
                TargetSupport::No,
                "`{}` per {kind} resolved onto `{}`, which cannot attribute at that \
                 granularity and would answer about something else",
                entry.id,
                resolved.backend.id
            );
            checked += 1;
        }
    }

    // The floor is on what was asked, not on what this machine serves: a
    // machine that withholds every counter (`perf_event_paranoid` 4, the CI's
    // first run) resolves fewer pairs, and each one it withholds is a
    // resolver's answer, not a pair the sweep missed.
    assert!(
        checked + withheld > 100,
        "only {} source-and-target pairs were asked, which is too few for this sweep to have \
         covered the catalog",
        checked + withheld
    );
    assert!(
        checked > 0,
        "this machine served none of {withheld} source-and-target pairs, so the sweep says \
         nothing about the resolver's choices"
    );
    eprintln!("checked {checked} pairs; {withheld} withheld by this machine");
}

/// A pin bypasses the resolver, so it bypasses the check above too. A pin
/// exists to compare backends against each other on the same question, which
/// is meaningless between one that answers per cgroup and one that answers for
/// the machine -- so the validator refuses it before anything is opened.
#[test]
fn pinning_a_backend_that_cannot_attribute_per_target_is_refused() {
    let refused = run(&[
        "io.bytes",
        "--cgroup",
        "/sys/fs/cgroup",
        "-m",
        "poll",
        "-e",
        "timeline",
        "--backend",
        "force:fs:proc",
    ]);
    assert_eq!(
        refused.status.code(),
        Some(EXIT_REJECTED),
        "{}",
        stderr(&refused)
    );
    let message = stderr(&refused);
    assert!(
        message.contains("cannot attribute per cgroup"),
        "the refusal has to name the granularity: {message}"
    );
    assert!(
        message.contains("system"),
        "and what that backend does serve: {message}"
    );
}

// ---------------------------------------------------------------------------
// Filter fields: what the selected backend actually puts on an event.
// ---------------------------------------------------------------------------

/// `/proc/net/dev` has one line per interface and no column naming a process.
/// The file's own header is the witness: the test reads it rather than taking
/// the catalog's word, because the catalog is exactly what is under test.
///
/// Before a backend could say which fields it carries, a `comm` predicate over
/// the procfs backend was accepted, ran in userspace against events that never
/// carried `comm`, and emptied the series. An empty series reads as "nothing
/// matched"; what happened was "this could never have matched".
#[test]
fn a_predicate_the_selected_backend_cannot_carry_is_refused_rather_than_emptied() {
    let file = std::fs::read_to_string("/proc/net/dev").expect("/proc/net/dev is readable");
    let header: String = file.lines().take(2).collect::<Vec<_>>().join(" ");
    assert!(
        !header.contains("comm") && !header.contains("pid"),
        "this test assumes /proc/net/dev names no process; its header says: {header}"
    );

    let refused = run(&[
        "net.bytes",
        "--system",
        "-m",
        "poll@100ms",
        "-e",
        "summary",
        "-d",
        "200ms",
        "--filter",
        "comm =~ 'omn'",
    ]);
    assert_ne!(
        refused.status.code(),
        Some(0),
        "a predicate no event can satisfy must not be answered with an empty series"
    );
    let message = stderr(&refused);
    assert!(
        message.contains("comm") && message.contains("fs:proc"),
        "the refusal has to name the field and the backend that lacks it: {message}"
    );
    assert!(
        message.contains("bpf:fentry"),
        "and the backend of the chain that does carry it: {message}"
    );
}

/// The other half, so the test above is about the field and not about
/// filtering: `iface` is a column of that same file, and a predicate over it
/// runs.
#[test]
fn a_predicate_the_selected_backend_does_carry_is_accepted() {
    let accepted = run(&[
        "net.bytes",
        "--system",
        "-m",
        "poll@100ms",
        "-e",
        "summary",
        "-d",
        "300ms",
        "--filter",
        "iface == 'lo'",
    ]);
    assert!(
        accepted.status.success(),
        "`iface` is a column of /proc/net/dev: {}",
        stderr(&accepted)
    );
}

/// A backend that declares no fields of its own carries the entry's, which is
/// the common case and the one that keeps the catalog readable: the per-backend
/// list is for the fields that differ, not for restating the ones that do not.
#[test]
fn a_backend_that_declares_no_filter_fields_carries_the_entrys() {
    let catalog = catalog();
    let entry = catalog
        .get(&"net.bytes".parse().unwrap())
        .expect("net.bytes is in the catalog");

    let procfs = entry.backend("fs:proc").expect("the procfs backend");
    assert_eq!(entry.backend_filter_fields(procfs), ["iface"]);

    let counters = catalog
        .get(&"cpu.cycles".parse().unwrap())
        .expect("cpu.cycles is in the catalog");
    let pmu = counters.backend("pmu").expect("the pmu backend");
    assert!(
        pmu.filter_fields.is_none(),
        "this backend declares none, which is what makes it the case worth checking"
    );
    assert_eq!(
        counters.backend_filter_fields(pmu),
        counters.filter_fields.as_slice(),
        "a backend that declares nothing carries everything the entry offers"
    );
}

// ---------------------------------------------------------------------------
// The loader: the two maps cannot drift from the entry they belong to.
// ---------------------------------------------------------------------------

#[test]
fn a_backend_claiming_a_target_the_entry_refuses_is_refused() {
    let mut entry = catalog().get(&"io.bytes".parse().unwrap()).unwrap().clone();
    // `io.bytes` declares no `iface` target, so no request could ever arrive
    // asking for one and this backend's claim is unreachable.
    entry.backends[2].targets = Some(
        [(TargetKind::Iface, TargetSupport::Native)]
            .into_iter()
            .collect(),
    );
    let error = Catalog::from_entries(vec![entry]).unwrap_err();
    assert!(
        error.message().contains("the entry itself refuses"),
        "{error}"
    );
}

#[test]
fn a_target_the_entry_promises_and_no_backend_serves_is_refused() {
    let mut entry = catalog().get(&"io.bytes".parse().unwrap()).unwrap().clone();
    // Narrow every backend to a cgroup: the entry still promises `system`, and
    // a request for one would now be accepted and then resolve onto nothing.
    for backend in &mut entry.backends {
        backend.targets = Some(
            [(TargetKind::Cgroup, TargetSupport::Native)]
                .into_iter()
                .collect(),
        );
    }
    let error = Catalog::from_entries(vec![entry]).unwrap_err();
    assert!(
        error.message().contains("no backend in its chain serves"),
        "{error}"
    );
}

#[test]
fn a_backend_carrying_a_field_the_entry_does_not_offer_is_refused() {
    let mut entry = catalog()
        .get(&"net.bytes".parse().unwrap())
        .unwrap()
        .clone();
    entry.backends[0].filter_fields = Some(vec!["iface".to_owned(), "daddr".to_owned()]);
    let error = Catalog::from_entries(vec![entry]).unwrap_err();
    assert!(
        error.message().contains("which the entry does not offer"),
        "{error}"
    );
}

#[test]
fn a_field_the_entry_offers_and_no_backend_carries_is_refused() {
    let mut entry = catalog()
        .get(&"net.bytes".parse().unwrap())
        .unwrap()
        .clone();
    // Take `comm` away from the one backend that has it. The entry still
    // offers the field, so the validator would accept a predicate over it and
    // the run would then report an empty series.
    for backend in &mut entry.backends {
        backend.filter_fields = Some(vec!["iface".to_owned()]);
    }
    let error = Catalog::from_entries(vec![entry]).unwrap_err();
    assert!(
        error.message().contains("no backend in its chain carries"),
        "{error}"
    );
    assert!(error.message().contains("comm"), "{error}");
}

// ---------------------------------------------------------------------------
// The two keys that were missing.
// ---------------------------------------------------------------------------

/// A device key has to render the numbers the kernel uses, or a series labelled
/// `dev:...` names something nobody can look up.
///
/// Ground truth is two kernel files that must agree: `/sys/block/<name>/dev`
/// writes `major:minor` and the first two columns of `/proc/diskstats` write
/// the same pair for the same device.
#[test]
fn a_device_key_carries_the_numbers_the_kernel_writes() {
    let mut devices = std::fs::read_dir("/sys/block")
        .expect("/sys/block exists on any machine with a block device")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    devices.sort();
    let name = devices
        .first()
        .expect("a machine running this suite has at least one block device");

    let written = std::fs::read_to_string(format!("/sys/block/{name}/dev"))
        .expect("the kernel writes major:minor here");
    let (major, minor) = written
        .trim()
        .split_once(':')
        .expect("the kernel writes it as major:minor");
    let (major, minor): (u32, u32) = (major.parse().unwrap(), minor.parse().unwrap());

    let key = KeyValue::dev(major, minor);
    assert_eq!(key.kind(), KeyKind::Dev);
    assert_eq!(key.to_string(), format!("dev:{major}+{minor}"));

    // The same pair, from an independent file, for the same device.
    let diskstats =
        std::fs::read_to_string("/proc/diskstats").expect("/proc/diskstats is readable");
    let line = diskstats
        .lines()
        .find(|line| line.split_whitespace().nth(2) == Some(name.as_str()))
        .unwrap_or_else(|| panic!("`{name}` is in /sys/block but not in /proc/diskstats"));
    let mut columns = line.split_whitespace();
    assert_eq!(
        columns.next().and_then(|c| c.parse::<u32>().ok()),
        Some(major),
        "the two files disagree about `{name}`: {line}"
    );
    assert_eq!(
        columns.next().and_then(|c| c.parse::<u32>().ok()),
        Some(minor),
        "the two files disagree about `{name}`: {line}"
    );

    // Different devices are different keys, which is the whole point: summing
    // an NVMe and a spinning disk into one number is what having no key forced.
    assert_ne!(key, KeyValue::dev(major, minor + 1));
}

/// A cpu key has to name a cpu the machine has, and each one has to be its own
/// key: a mean across twenty-four cores describes no core.
#[test]
fn a_cpu_key_names_one_of_the_machines_own_cpus() {
    let online = std::fs::read_to_string("/sys/devices/system/cpu/online")
        .expect("the kernel lists its online cpus here");
    let highest: u32 = online
        .trim()
        .rsplit(['-', ','])
        .next()
        .and_then(|last| last.parse().ok())
        .expect("the list ends with a cpu number");

    let keys: BTreeSet<String> = (0..=highest)
        .map(|cpu| KeyValue::cpu(cpu).to_string())
        .collect();
    assert_eq!(
        keys.len() as u32,
        highest + 1,
        "every cpu has to be its own key, or a per-cpu series collapses into one"
    );
    assert_eq!(KeyValue::cpu(highest).kind(), KeyKind::Cpu);
    assert_eq!(KeyValue::cpu(7).to_string(), "cpu:7");
}

/// A source that accepts `--cpu` is being asked about one core, and a request
/// narrowed to several cores has to come back as several series. That needs a
/// key, so the catalog cannot offer the target without offering the key.
#[test]
fn every_source_that_accepts_a_cpu_target_can_key_a_series_by_cpu() {
    let catalog = catalog();
    let mut checked = 0;
    for entry in catalog.entries() {
        if entry.target_support(TargetKind::Cpu) == TargetSupport::No {
            continue;
        }
        assert!(
            entry.keys.contains(&KeyKind::Cpu),
            "`{}` accepts --cpu but carries no `cpu` key, so narrowing to four \
             cores could only come back as one averaged number: {:?}",
            entry.id,
            entry.keys
        );
        checked += 1;
    }
    assert!(checked >= 20, "only {checked} sources accept a cpu target");
}

/// Both new keys survive the round trip every event takes on its way to a
/// result file, or a series cannot be read back.
#[test]
fn the_new_keys_survive_a_round_trip() {
    for key in [KeyValue::dev(259, 0), KeyValue::cpu(7)] {
        let json = serde_json::to_string(&key).expect("a key serializes");
        assert_eq!(
            serde_json::from_str::<KeyValue>(&json).expect("and reads back"),
            key
        );
        let yaml = serde_norway::to_string(&key).expect("a key serializes to yaml");
        assert_eq!(
            serde_norway::from_str::<KeyValue>(&yaml).expect("and reads back"),
            key
        );
    }
}

/// Every key kind the model has must be writable in a session file, or a
/// request naming one cannot be replayed.
#[test]
fn every_key_kind_parses_back_from_what_it_prints() {
    for kind in KeyKind::ALL {
        assert_eq!(
            kind.as_str().parse::<KeyKind>().expect("a key kind parses"),
            kind
        );
    }
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// The backend `omn` settles on for a request, read out of the resolved record
/// rather than guessed from the output.
fn resolved_backend(args: &[&str]) -> String {
    let mut full = args.to_vec();
    full.extend_from_slice(&[
        "-m",
        "poll",
        "-e",
        "timeline",
        "--dump-resolved",
        "--format",
        "json",
    ]);
    let dumped = run(&full);
    assert!(
        dumped.status.success(),
        "resolving {args:?} failed: {}",
        stderr(&dumped)
    );
    let document: serde_json::Value =
        serde_json::from_str(&stdout(&dumped)).expect("the resolved record parses");
    document[0]["backend"]["id"]
        .as_str()
        .expect("the record names the backend")
        .to_owned()
}

// ---------------------------------------------------------------------------
// The loader refuses what no build could keep
// ---------------------------------------------------------------------------

/// The embedded catalog, as entries a test may break one at a time.
fn entries() -> Vec<omn::catalog::Entry> {
    catalog().entries().to_vec()
}

fn refusal_of(entries: Vec<omn::catalog::Entry>) -> String {
    match Catalog::from_entries(entries) {
        Ok(_) => panic!("the loader accepted a catalog it should refuse"),
        Err(error) => error.to_string(),
    }
}

/// A copy of the catalog carrying each shape the loader refuses, refused with
/// the reason: an emit no declared mode produces, `rate` or `stack` over a
/// unit that does not add up, and a requirement written as a pattern. A
/// catalog loaded with `--catalog <dir>` goes through the same loader.
#[test]
fn a_catalog_carrying_a_promise_no_build_can_keep_is_refused_when_it_is_loaded() {
    let ipc = |entries: &mut Vec<omn::catalog::Entry>| -> usize {
        entries
            .iter()
            .position(|entry| entry.id.as_str() == "cpu.ipc")
            .expect("cpu.ipc is in the catalog")
    };

    let mut broken = entries();
    let at = ipc(&mut broken);
    broken[at].modes = vec![omn::request::ModeKind::Snapshot];
    broken[at].emits = vec![omn::request::EmitKind::Timeline];
    let refusal = refusal_of(broken);
    assert!(
        refusal.contains("cpu.ipc") && refusal.contains("none of the modes"),
        "{refusal}"
    );

    for emit in [omn::request::EmitKind::Rate, omn::request::EmitKind::Stack] {
        let mut broken = entries();
        let at = ipc(&mut broken);
        broken[at].emits.push(emit);
        broken[at].modes.push(omn::request::ModeKind::Sample);
        let refusal = refusal_of(broken);
        assert!(
            refusal.contains("cpu.ipc")
                && refusal.contains(&format!("`{emit}` over a `ratio`"))
                && refusal.contains("does not add up"),
            "{refusal}"
        );
    }

    let mut broken = entries();
    let at = ipc(&mut broken);
    broken[at].backends[0].requires = vec!["pmu:topdown-*".to_owned()];
    let refusal = refusal_of(broken);
    assert!(
        refusal.contains("cpu.ipc") && refusal.contains("`pmu:topdown-*`, which is a pattern"),
        "{refusal}"
    );

    // A raw tracepoint's prototype is not a pattern, though it has a `*` in it.
    let mut kept = entries();
    let at = ipc(&mut kept);
    kept[at].backends[0]
        .requires
        .push("raw_tracepoint:sched_switch(bool, task_struct *, task_struct *)".to_owned());
    assert!(Catalog::from_entries(kept).is_ok());
}

/// A (target, mode) pair that an entry accepts one half at a time and that no
/// backend of its chain declares together is the request's, exit 2, with the
/// reason -- not the machine's "no viable backend", exit 1, which would send
/// the reader looking for a kernel or a capability that no machine has.
///
/// The truth is the catalog's own data: the pairs are found here from each
/// backend's declared targets and modes, independently of the validator, and
/// every one is run through the binary. `sched.switches` per cgroup in
/// `trace` mode is one such pair today (`pmu:sw` traces and does not follow a
/// cgroup; the two programs follow one and only count), so the set is not
/// empty.
#[test]
fn a_target_and_mode_no_backend_declares_together_is_refused_as_the_request() {
    let catalog = Catalog::embedded().expect("the embedded catalog loads");
    let own_pid = std::process::id().to_string();
    let target_args = |kind: TargetKind| -> Option<Vec<String>> {
        Some(match kind {
            TargetKind::Pid => vec!["--pid".into(), own_pid.clone()],
            TargetKind::Cgroup => vec!["--cgroup".into(), "/sys/fs/cgroup".into()],
            TargetKind::Cpu => vec!["--cpu".into(), "0".into()],
            TargetKind::Iface => vec!["--iface".into(), "lo".into()],
            TargetKind::System => vec!["--system".into()],
            _ => return None,
        })
    };
    let mut pairs = Vec::new();
    for entry in catalog.entries() {
        for kind in TargetKind::ALL {
            if entry.target_support(kind) == TargetSupport::No {
                continue;
            }
            for mode in &entry.modes {
                let together = entry.backends.iter().any(|backend| {
                    entry.backend_target_support(backend, kind) != TargetSupport::No
                        && entry.backend_modes(backend).contains(mode)
                });
                if !together {
                    pairs.push((entry, kind, *mode));
                }
            }
        }
    }
    assert!(
        pairs
            .iter()
            .any(|(entry, kind, mode)| entry.id.as_str() == "sched.switches"
                && *kind == TargetKind::Cgroup
                && mode.to_string() == "trace"),
        "the catalog no longer has the pair this test was written on: {:?}",
        pairs
            .iter()
            .map(|(e, k, m)| format!("{} {k} {m}", e.id))
            .collect::<Vec<_>>()
    );
    let mut ran = 0;
    for (entry, kind, mode) in &pairs {
        let Some(target) = target_args(*kind) else {
            continue;
        };
        let emit = entry.emits[0].to_string();
        let mode = mode.to_string();
        let mut args: Vec<&str> = vec!["trace", entry.id.as_str()];
        args.extend(target.iter().map(String::as_str));
        args.extend(["-m", &mode, "-e", &emit, "-d", "100ms"]);
        let output = run(&args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(EXIT_REJECTED),
            "omn {}: {stderr}",
            args.join(" ")
        );
        assert!(
            stderr.contains("no backend of its chain declares both"),
            "omn {}: the refusal has to say it is the pair's: {stderr}",
            args.join(" ")
        );
        ran += 1;
    }
    assert!(ran > 0, "no pair had a target this test can write");
    eprintln!("{ran} pair(s) no backend declares, each refused as the request");
}
