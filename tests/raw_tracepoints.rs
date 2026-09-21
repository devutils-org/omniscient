//! The cheap probe and the expensive one must reach the same answer.
//!
//! `raw_tracepoint_support` answers from the kernel's BTF: a tracepoint the
//! kernel defines carries a `btf_trace_<name>` type, and reading that costs
//! nothing. What the attach actually does is `BPF_RAW_TRACEPOINT_OPEN` with the
//! name, and those are not the same question -- which is exactly the gap
//! finding 15 found on the classic path, where a requirement was probed by
//! listing the event's directory while the attach read a root-only file inside
//! it, so a backend was reported available, selected by the resolver, and only
//! refused once the target had been launched.
//!
//! The catalog gained twenty-seven `bpf:raw_tp` backends in one pass, checked
//! name by name against `/sys/kernel/btf/vmlinux` and not against an attach. If
//! BTF presence and attachability ever diverge, every collector written against
//! those backends discovers it one at a time, at attach time, with a target
//! already running.
//!
//! So the expensive question is asked here, of the whole catalog, and required
//! to agree with the cheap one. It is not asked on every `omn list`, because
//! attaching and detaching a tracepoint costs about thirty milliseconds of RCU
//! synchronisation and doing it for every entry takes the listing from 150 ms
//! to 1.7 s. Finding 38 of `docs/open-findings.md` has the measurement.
//!
//! This is an integration test rather than a unit one for the reason `CLAUDE.md`
//! gives: the eBPF path needs `CAP_BPF`, file capabilities do not survive a
//! relink, and an in-process test binary is rebuilt under a fresh hash every
//! run. `target/debug/omn` carries them once `make` has run. A unit version of
//! this test would skip on this machine and always have.

use std::process::{Command, Output};

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

fn omn(args: &[&str]) -> Output {
    Command::new(BINARY)
        .args(args)
        .output()
        .expect("omn should run")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The one line that says what this machine can serve.
fn totals(listing: &str) -> &str {
    listing
        .lines()
        .find(|line| line.contains("sources:") && line.contains("usable here"))
        .expect("a listing ends with what this machine can serve")
}

/// Every raw tracepoint the catalog names attaches, or the probe is optimistic.
///
/// The assertion is an agreement rather than a count, because a count would be
/// a fact about this machine and the agreement is a fact about the tool: whether
/// the question `omn list` asks is the question a run will ask. On a machine
/// without the capabilities both probes refuse for the same reason and the two
/// listings still agree, which is the right answer there too -- and the test
/// says which case it is rather than passing quietly.
#[test]
fn the_btf_probe_and_the_attach_reach_the_same_answer() {
    let cheap = omn(&["list"]);
    assert!(cheap.status.success(), "`omn list` should succeed");
    let thorough = omn(&["list", "--verify-attach"]);
    assert!(
        thorough.status.success(),
        "`omn list --verify-attach` should succeed"
    );

    let cheap = stdout(&cheap);
    let thorough = stdout(&thorough);

    if thorough.contains("needs CAP_BPF") {
        eprintln!(
            "this binary carries no capabilities, so both probes refuse and the agreement is \
             trivial; run `make` and try again to exercise the attach"
        );
    }

    assert_eq!(
        totals(&cheap),
        totals(&thorough),
        "reading the kernel's BTF and attaching for real disagree about what this machine can \
         serve. The catalog declares a raw tracepoint that is in BTF and will not attach, so \
         `raw_tracepoint_support` is now optimistic and should call `raw_tracepoint_attaches`. \
         Run `omn list --verify-attach` and look for a `raw_tracepoint:` requirement reported \
         missing."
    );
}

/// The two listings differ in nothing at all, not merely in their totals.
///
/// The totals could agree while two entries swapped a backend between them. This
/// is the stricter statement, and it is cheap to make because the listing is
/// deterministic: same catalog, same machine, same probe answers.
#[test]
fn verifying_the_attach_changes_no_line_of_the_listing() {
    let cheap = stdout(&omn(&["list"]));
    let thorough = stdout(&omn(&["list", "--verify-attach"]));

    let differing: Vec<String> = cheap
        .lines()
        .zip(thorough.lines())
        .filter(|(a, b)| a != b)
        .map(|(a, b)| format!("  cheap:    {a}\n  verified: {b}"))
        .collect();

    assert!(
        differing.is_empty(),
        "attaching for real changed what the listing says, which means the BTF probe is \
         answering a different question from the attach for at least one source:\n{}",
        differing.join("\n")
    );
}

/// `--verify-attach` is documented where someone would look for it.
///
/// A diagnostic nobody can find is a diagnostic nobody runs, and this one exists
/// precisely for the machine where the two probes *do* disagree -- which is by
/// definition not this one.
#[test]
fn the_switch_says_what_it_costs_and_what_it_buys() {
    let help = stdout(&omn(&["list", "--help"]));
    assert!(
        help.contains("--verify-attach"),
        "the switch has to appear in the help: {help}"
    );
    assert!(
        help.contains("attaching"),
        "the help has to say what it does differently: {help}"
    );
}
