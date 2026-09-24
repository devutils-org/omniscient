//! The observer effect: does a collector serialize what it observes?
//! (`ROADMAP.md` phase 8, task 6.)
//!
//! An asynchronous sequence -- four threads on four cores, each doing its own
//! work with nothing shared -- takes about as long as one thread doing one
//! thread's share of it. The ratio `elapsed(4 threads) / elapsed(1 thread)`,
//! at equal work per thread,
//! is near 1 when nothing serializes the threads and near 4 when something
//! makes them take turns. A collector that takes a lock, or writes to one
//! pipe, on the observed path moves that ratio; one that does not, does not.
//!
//! The metric is checked first against a sequence serialized **on purpose**:
//! the same workload with a global mutex around each operation. That check
//! needs no gate, because the separation it asserts is a factor of two or
//! more. The collectors' comparison does stand behind the gate: the effect it
//! looks for is a few percent.
//!
//! `#[ignore]`, and run through `scripts/bench.sh`.

#[path = "../examples/bench/harness.rs"]
mod harness;

use harness::*;

const THREADS: usize = 4;

/// The scaling ratio of `kind` over `repetitions` round-robin pairs of runs,
/// bare or under `method`, as median and IQR of the per-repetition ratios.
fn scaling(
    omn: &Omn,
    kind: &str,
    per_thread: u64,
    method: Option<&Method>,
    repetitions: usize,
) -> Result<Stats, String> {
    let one = Workload::new(kind, &[kind, "1", &per_thread.to_string()], 1);
    let many = Workload::new(
        kind,
        &[kind, &THREADS.to_string(), &per_thread.to_string()],
        THREADS,
    );
    let run = |workload: &Workload| -> Result<f64, String> {
        Ok(match method {
            Some(method) => run_traced(omn, method, workload)?.run.elapsed_ns(),
            None => run_bare(workload)?.elapsed_ns(),
        } as f64)
    };
    let mut evictor = Evictor::new();
    let mut ratios = Vec::new();
    for _ in 0..WARMUP {
        run(&one)?;
        run(&many)?;
    }
    for repetition in 0..repetitions {
        // Alternate which goes first, for the same reason round_robin does.
        let (a, b) = if repetition % 2 == 0 {
            (&one, &many)
        } else {
            (&many, &one)
        };
        evictor.evict();
        let first = run(a)?;
        evictor.evict();
        let second = run(b)?;
        let (single, parallel) = if repetition % 2 == 0 {
            (first, second)
        } else {
            (second, first)
        };
        ratios.push(parallel / single);
    }
    Stats::of(&ratios).ok_or_else(|| "no ratio".into())
}

/// Ground truth for the metric: a sequence the workload serializes itself.
#[test]
#[ignore = "measures the machine: run through scripts/bench.sh"]
fn the_observer_metric_flags_a_sequence_serialized_on_purpose() {
    let omn = Omn::locate();
    let free = scaling(&omn, "async", 200_000, None, 10).expect("async");
    let serial = scaling(&omn, "async-serial", 200_000, None, 10).expect("async-serial");
    say(format!(
        "{THREADS} threads over 1, at equal work per thread: independent {:.2} [IQR {:.2}], \
         serialized on purpose {:.2} [IQR {:.2}]",
        free.median,
        free.iqr(),
        serial.median,
        serial.iqr()
    ));
    assert!(
        serial.q1 > 2.0 * free.q3,
        "the metric must separate a serialized sequence from an independent one: \
         serialized {serial:?}, independent {free:?}"
    );
}

/// The collectors on the path of an asynchronous sequence: syscalls under
/// `proc.syscall`, allocations under the preload shim.
#[test]
#[ignore = "measures the machine: run through scripts/bench.sh"]
fn the_collectors_on_an_asynchronous_sequence() {
    let omn = Omn::locate();
    let repetitions = repetitions();
    let smoke = repetitions < MIN_REPETITIONS;
    say(format!("measuring {}", omn.describe()));
    let (baselines, _) = harness::baselines(
        &[
            Workload::new("async", &["async", "4", "200000"], THREADS),
            Workload::new("async-malloc", &["async-malloc", "4", "1000000"], THREADS),
        ],
        repetitions,
    )
    .expect("the baselines");
    eprint!("{}", gate_table(&baselines));
    let gate = judge(&baselines, &Conditions::read());
    match (&gate, smoke) {
        (Err(refusal), false) => panic!("{refusal}"),
        (Err(refusal), true) => say(format!(
            "SMOKE RUN ({repetitions} repetitions): the refusal is recorded, not obeyed\n{refusal}"
        )),
        (Ok(admitted), _) => {
            say("the gate passed");
            for line in admitted.exclusions() {
                say(format!(
                    "excluded, and measured anyway, so read with its resolution: {line}"
                ));
            }
        }
    }

    let method = |source: &str, backend: &str| Method {
        source: source.into(),
        backend: Some(backend.into()),
        mode: "trace".into(),
        emit: "summary".into(),
        filter: None,
        system: false,
    };
    let cases = [
        ("async", 200_000, method("proc.syscall", "bpf:raw_tp")),
        (
            "async-malloc",
            1_000_000,
            method("mem.heap.alloc", "preload"),
        ),
    ];
    let dir = run_dir(if smoke { "smoke-observer" } else { "observer" });
    let mut table = String::new();
    if smoke || gate.is_err() {
        table.push_str("> **NOT A MEASUREMENT.** A smoke run of the harness.\n\n");
    }
    table.push_str(&format!(
        "# Observer effect\n\nMeasured with {}. The ratio is elapsed({THREADS} threads) / \
         elapsed(1 thread) at equal work per thread, median [IQR] over {repetitions} \
         repetitions; the index is the traced ratio over the bare one, 1.00 meaning the \
         collector serializes nothing.\n\n| source | backend | workload | bare ratio | traced \
         ratio | index |\n|---|---|---|---|---|---|\n",
        omn.describe()
    ));
    for (kind, per_thread, method) in cases {
        if let Err(refused) = resolves(&omn, &method) {
            say(format!(
                "SKIPPED {}: {}",
                method.label(),
                refused.lines().next().unwrap_or("")
            ));
            continue;
        }
        let bare = scaling(&omn, kind, per_thread, None, repetitions).expect("bare");
        let traced = scaling(&omn, kind, per_thread, Some(&method), repetitions).expect("traced");
        table.push_str(&format!(
            "| {} | {} | {kind} | {:.2} [{:.2}] | {:.2} [{:.2}] | {:.2} |\n",
            method.source,
            method.backend.as_deref().unwrap_or(""),
            bare.median,
            bare.iqr(),
            traced.median,
            traced.iqr(),
            traced.median / bare.median
        ));
    }
    std::fs::write(dir.join("observer.md"), &table).unwrap();
    eprintln!("{table}");
    say(format!("written to {}", dir.display()));
}
