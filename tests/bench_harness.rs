//! The bench's harness, and its variance gate.
//!
//! The tests that are not `#[ignore]` are cheap and run with the suite: the
//! statistics on samples whose answer is known, the gate's refusal on a series
//! whose CV is known, the round-robin order, the verdict and the resolution
//! logic on synthetic rows, and the workloads counting what they were asked to
//! do. None of them measures the machine.
//!
//! The two that are `#[ignore]` measure it, and run through `scripts/bench.sh`:
//! the gate on this machine as it is, and the gate on this machine loaded on
//! purpose, which must refuse. `docs/bench.md` says what each found.

#[path = "../examples/bench/harness.rs"]
mod harness;

use harness::*;

fn performance() -> Conditions {
    Conditions {
        governors: vec!["performance".into()],
        cpus: 24,
        ..Conditions::default()
    }
}

/// `n` samples around `mean`, alternating `+deviation` and `-deviation`: a
/// series whose sample standard deviation is known in closed form,
/// `deviation * sqrt(n / (n - 1))`.
fn alternating(n: usize, mean: f64, deviation: f64) -> Vec<f64> {
    (0..n)
        .map(|i| {
            if i % 2 == 0 {
                mean + deviation
            } else {
                mean - deviation
            }
        })
        .collect()
}

fn baseline(name: &str, samples: &[f64]) -> Baseline {
    Baseline {
        workload: name.into(),
        stats: Stats::of(samples).expect("samples"),
    }
}

// ------------------------------------------------------------- statistics

/// Against numpy's default quantiles, computed outside this code:
/// `numpy.percentile([1..10], [25, 50, 75])` is 3.25, 5.5, 7.75, and for
/// `[7, 15, 36, 39, 40, 41]` it is 20.25, 37.5, 39.75.
#[test]
fn quartiles_are_the_ones_numpy_gives() {
    let one_to_ten: Vec<f64> = (1..=10).map(f64::from).collect();
    let stats = Stats::of(&one_to_ten).unwrap();
    assert_eq!((stats.q1, stats.median, stats.q3), (3.25, 5.5, 7.75));
    assert_eq!(stats.iqr(), 4.5);

    let shuffled = [40.0, 7.0, 39.0, 15.0, 41.0, 36.0];
    let stats = Stats::of(&shuffled).unwrap();
    assert_eq!((stats.q1, stats.median, stats.q3), (20.25, 37.5, 39.75));
    assert_eq!((stats.min, stats.max), (7.0, 41.0));
}

#[test]
fn the_coefficient_of_variation_is_the_closed_form_one() {
    let stats = Stats::of(&alternating(20, 1000.0, 10.0)).unwrap();
    let expected = 10.0 * (20.0f64 / 19.0).sqrt() / 1000.0;
    assert!((stats.cv() - expected).abs() < 1e-12, "{}", stats.cv());
    assert_eq!(stats.median, 1000.0);
}

#[test]
fn a_series_of_nothing_has_no_statistics_and_a_zero_mean_no_finite_cv() {
    assert!(Stats::of(&[]).is_none());
    assert!(Stats::of(&[1.0, f64::NAN]).is_none());
    assert!(Stats::of(&[0.0; 20]).unwrap().cv().is_infinite());
}

// ------------------------------------------------------------------ gate

/// The bound is on the median's resolution, from both sides. Twenty runs at a
/// CV of 1.03 % resolve 0.82 % and pass -- the old gate refused them for a
/// spread no median of twenty has. Twenty at 1.54 % resolve 1.22 % and are
/// excluded, with every number that decided it.
#[test]
fn a_workload_is_judged_by_what_its_median_resolves() {
    let moderate = baseline("launch", &alternating(20, 1000.0, 10.0));
    let resolved = moderate.stats.resolution();
    assert!((resolved - 0.00818).abs() < 0.0001, "{resolved}");
    let gate = judge(&[moderate], &performance()).expect("a median that resolves 0.82 %");
    assert_eq!(gate.repetitions("launch", 20), Some(20));

    let noisy = baseline("io", &alternating(20, 1000.0, 15.0));
    let quiet = baseline("compute", &alternating(20, 1000.0, 1.0));
    let gate = judge(&[quiet, noisy], &performance()).expect("compute can still be measured");
    assert_eq!(gate.repetitions("io", 20), None);
    let why = &gate.excluded["io"];
    for number in ["1.54 %", "1.22 %", "30 repetitions"] {
        assert!(
            why.contains(number),
            "the exclusion must carry `{number}`: {why}"
        );
    }
}

/// The repetitions a spread needs are what the formula says, and never fewer
/// than the floor.
#[test]
fn the_repetitions_a_spread_needs_are_the_ones_that_resolve_the_target() {
    let series = Stats::of(&alternating(20, 1000.0, 22.7)).unwrap();
    let needed = series.repetitions_to_resolve(RESOLUTION_TARGET);
    assert!(
        resolution_at(series.cv(), needed) <= RESOLUTION_TARGET
            && resolution_at(series.cv(), needed - 1) > RESOLUTION_TARGET,
        "{needed} repetitions for a CV of {:.2} %",
        series.cv() * 100.0
    );
    let quiet = Stats::of(&alternating(20, 1000.0, 1.0)).unwrap();
    assert_eq!(
        quiet.repetitions_to_resolve(RESOLUTION_TARGET),
        MIN_REPETITIONS
    );
}

/// A steady load hides from every spread, so it is refused on what it costs
/// the other cpus.
#[test]
fn a_machine_busy_elsewhere_is_refused_whatever_the_spread() {
    let quiet = baseline("compute", &alternating(20, 1000.0, 1.0));
    let busy = Conditions {
        foreign_busy_cpus: Some(9.5),
        ..performance()
    };
    let text = judge(std::slice::from_ref(&quiet), &busy)
        .unwrap_err()
        .to_string();
    assert!(text.contains("9.5 cpus busy"), "{text}");
    let calm = Conditions {
        foreign_busy_cpus: Some(1.2),
        ..performance()
    };
    judge(&[quiet], &calm).expect("the harness's own cpu is allowed for");
}

/// A noisy workload excludes itself and nothing else; only a run where
/// nothing is left to measure is refused.
#[test]
fn only_a_run_with_nothing_left_to_measure_is_refused() {
    let noisy = baseline("io", &alternating(20, 1000.0, 50.0));
    let refusal = judge(&[noisy], &performance()).unwrap_err();
    let text = refusal.to_string();
    assert!(text.contains("every workload was excluded"), "{text}");
    assert!(text.contains("`io`"), "{text}");
    assert!(text.contains("nothing was"), "{text}");
}

#[test]
fn the_gate_refuses_fewer_than_twenty_repetitions_however_quiet() {
    let refusal = judge(&[baseline("compute", &[1000.0; 19])], &performance()).unwrap_err();
    assert!(
        refusal.reasons[0].contains("at least 20"),
        "{:?}",
        refusal.reasons
    );
    judge(&[baseline("compute", &[1000.0; 20])], &performance()).expect("twenty identical");
}

/// The governor is read and never changed: a refusal says what to run.
#[test]
fn the_gate_refuses_a_governor_it_will_not_change_and_says_how_to() {
    let conditions = Conditions {
        governors: vec!["performance".into(), "powersave".into()],
        ..performance()
    };
    let refusal = judge(&[baseline("compute", &[1000.0; 20])], &conditions).unwrap_err();
    let text = refusal.to_string();
    assert!(text.contains("powersave"), "{text}");
    assert!(
        text.contains("cpupower frequency-set -g performance"),
        "{text}"
    );
}

#[test]
fn the_gate_refuses_when_nothing_was_measured() {
    assert!(judge(&[], &performance()).is_err());
}

// -------------------------------------------------------------- schedule

#[test]
fn round_robin_runs_every_method_once_per_repetition_and_rotates_who_goes_first() {
    let (methods, repetitions) = (4, 20);
    let order = round_robin(methods, repetitions);
    assert_eq!(order.len(), methods * repetitions);

    for (repetition, round) in order.chunks(methods).enumerate() {
        let mut seen = round.to_vec();
        seen.sort();
        assert_eq!(
            seen,
            (0..methods).collect::<Vec<_>>(),
            "repetition {repetition}"
        );
    }
    // Every method holds every position equally often.
    for position in 0..methods {
        let mut counts = vec![0; methods];
        for round in order.chunks(methods) {
            counts[round[position]] += 1;
        }
        assert!(
            counts.iter().all(|&count| count == repetitions / methods),
            "position {position}: {counts:?}"
        );
    }
    assert_eq!(round_robin(1, 3), vec![0, 0, 0]);
}

// --------------------------------------------------------------- verdict

fn measured(backend: &str, chosen: bool, median: f64, degraded: Option<&str>) -> Measured {
    let samples: Vec<f64> = (0..20)
        .map(|i| median * (0.95 + 0.005 * i as f64))
        .collect();
    Measured {
        backend: backend.into(),
        setting: "poll@100ms, launch".into(),
        chosen,
        degraded: degraded.map(str::to_owned),
        equivalent: false,
        cost: Stats::of(&samples).unwrap(),
        losses: 0,
    }
}

#[test]
fn a_cheaper_fallback_that_degrades_does_not_overturn_the_choice() {
    let rows = [
        measured("fs:proc:smaps_rollup", true, 400_000.0, None),
        measured("fs:proc:statm", false, 40_000.0, Some("no PSS")),
    ];
    match verdict(&rows) {
        Verdict::Confirmed(note) => assert!(note.contains("degrades"), "{note}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_clearly_cheaper_equivalent_corrects_the_choice() {
    let mut cheaper = measured("pmu:generic", false, 40_000.0, None);
    cheaper.equivalent = true;
    let rows = [measured("pmu", true, 400_000.0, None), cheaper];
    assert!(
        matches!(verdict(&rows), Verdict::Corrected(_)),
        "{:?}",
        verdict(&rows)
    );
}

#[test]
fn overlapping_costs_or_more_losses_confirm_the_choice() {
    let mut close = measured("b", false, 396_000.0, None);
    close.equivalent = true;
    assert!(matches!(
        verdict(&[measured("a", true, 400_000.0, None), close]),
        Verdict::Confirmed(_)
    ));

    let mut lossy = measured("b", false, 10_000.0, None);
    lossy.losses = 3;
    assert!(matches!(
        verdict(&[measured("a", true, 400_000.0, None), lossy]),
        Verdict::Confirmed(_)
    ));
    assert_eq!(verdict(&[measured("a", true, 1.0, None)]), Verdict::Alone);
}

/// A backend measured system-wide is not cheaper than one measured per
/// launch: they observed different things.
#[test]
fn backends_measured_under_different_settings_are_not_compared() {
    let mut elsewhere = measured("fs:proc", false, 1_000.0, None);
    elsewhere.setting = "poll@100ms, system".into();
    elsewhere.equivalent = true;
    match verdict(&[measured("fs:cgroup", true, 400_000.0, None), elsewhere]) {
        Verdict::Confirmed(note) => assert!(note.contains("not compared"), "{note}"),
        other => panic!("{other:?}"),
    }
}

// ------------------------------------------------------------ resolution

fn pairs(own: i64, tool: &[Option<i64>]) -> Vec<Pair> {
    tool.iter()
        .map(|tool| Pair {
            own_gap_ns: own,
            tool_gap_ns: *tool,
        })
        .collect()
}

#[test]
fn a_gap_is_resolved_only_when_every_pair_is_seen_apart_and_close_to_its_own_gap() {
    let exact = Resolved::from_pairs(1000, &pairs(1000, &[Some(1000); 20]));
    assert!(exact.resolves(), "{exact:?}");

    let missing = Resolved::from_pairs(1000, &pairs(1000, &[Some(1000), None]));
    assert!(
        !missing.resolves(),
        "a pair not seen is not resolved: {missing:?}"
    );

    // Two events stamped alike: seen, but not told apart.
    let merged = Resolved::from_pairs(1000, &pairs(1000, &[Some(0); 20]));
    assert_eq!(merged.distinguished, 0);
    assert!(!merged.resolves());

    // Apart, but 900 ns off a 1000 ns gap: the order is right, the distance
    // is not a measurement of it.
    let far = Resolved::from_pairs(1000, &pairs(1000, &[Some(1900); 20]));
    assert!(!far.resolves(), "{far:?}");
}

#[test]
fn the_effective_resolution_is_the_smallest_gap_above_which_everything_resolves() {
    let at = |gap: u64, ok: bool| {
        Resolved::from_pairs(
            gap,
            &pairs(gap as i64, &[Some(if ok { gap as i64 } else { 0 }); 10]),
        )
    };
    let sweep = [
        at(500, false),
        at(1000, true),
        at(2000, true),
        at(5000, true),
    ];
    assert_eq!(effective_resolution(&sweep), Some(1000));
    // A hole above the smallest success: only what is above the hole counts.
    let holed = [at(500, true), at(1000, false), at(2000, true)];
    assert_eq!(effective_resolution(&holed), Some(2000));
    assert_eq!(effective_resolution(&[at(500, false)]), None);
}

// ------------------------------------------------------------- workloads

/// The workloads are the ground truth the bench checks the tool against, so
/// they are checked first against the kernel: a thousand getppid calls report
/// a thousand operations, and the syscall number is the one the kernel's
/// header gives.
#[test]
fn the_workload_counts_what_it_was_asked_to_do() {
    let run = run_bare(&Workload::new("syscall", &["syscall", "1000"], 1)).expect("a run");
    assert_eq!(run.get("ops"), Some(1000));
    assert_eq!(run.get("syscall_nr"), Some(libc::SYS_getppid as u64));
    assert!(run.elapsed_ns() > 0);

    let threads = run_bare(&Workload::new("contended", &["contended", "3", "500"], 3)).unwrap();
    assert_eq!(
        threads.get("counter"),
        Some(1500),
        "a mutex that loses increments"
    );
}

/// A cold page cache without root: after `POSIX_FADV_DONTNEED`, the file's
/// pages are counted by `mincore`, a second account from the kernel, before
/// the read starts.
#[test]
fn the_io_workload_reads_from_a_cold_cache_without_root() {
    let path = bench_dir().join(format!("cold.{}.dat", std::process::id()));
    std::fs::create_dir_all(bench_dir()).unwrap();
    let prepare = std::process::Command::new(workload_binary().unwrap())
        .args(["io-prepare", &path.to_string_lossy(), "4"])
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(prepare.success());
    let run = run_bare(&Workload::new("io", &["io", &path.to_string_lossy()], 1)).unwrap();
    let _ = std::fs::remove_file(&path);
    assert_eq!(run.get("bytes"), Some(4 << 20));
    assert_eq!(
        run.get("pages"),
        Some((4 << 20) / run.get("page").unwrap_or(4096))
    );
    let resident = run.get("resident_before").unwrap();
    assert!(
        resident * 10 <= run.get("pages").unwrap(),
        "{resident} of {} pages still cached after DONTNEED",
        run.get("pages").unwrap()
    );
}

#[test]
fn the_evictor_overflows_every_cache_the_kernel_describes() {
    let largest = llc_bytes().expect("this kernel describes its caches");
    assert!(Evictor::new().bytes() >= 2 * largest);
}

// ------------------------------------------------------ the gate, for real

/// The first step of every real measurement: the gate on this machine as it
/// is now. It prints the table either way; it fails with the refusal when the
/// machine is too noisy, which is the gate working and the reason no table is
/// produced from it.
#[test]
#[ignore = "measures the machine: run through scripts/bench.sh"]
fn the_gate_on_this_machine() {
    prepare_io_file().expect("the io file");
    let (baselines, throttled) =
        harness::baselines(&reference_workloads(), MIN_REPETITIONS).expect("the baselines");
    say(format!(
        "gate, at least {} repetitions round-robin, LLC evicted before each run:",
        MIN_REPETITIONS
    ));
    eprint!("{}", gate_table(&baselines));
    for (workload, count) in &throttled {
        say(format!(
            "{count} run(s) of `{workload}` throttled and dropped"
        ));
    }
    let conditions = Conditions::read();
    if let Some(busy) = conditions.foreign_busy_cpus {
        say(format!(
            "the unpinned cpus were {busy:.2} cpus busy meanwhile"
        ));
    }
    match judge(&baselines, &conditions) {
        Err(refusal) => panic!("{refusal}"),
        Ok(gate) => {
            for line in gate.exclusions() {
                say(format!("excluded, and not measured: {line}"));
            }
        }
    }
}

/// Ground truth for the gate: noise the test itself injects. One thread per
/// cpu alternates random bursts of spinning and sleeping, and one more
/// streams through 64 MiB, so the pinned workload shares its core and its
/// cache with a load whose timing nothing can predict. The gate must refuse.
///
/// This is also what the other streams building on this machine look like to
/// it, which is why the real measurement is run alone.
#[test]
#[ignore = "measures the machine: run through scripts/bench.sh"]
fn the_gate_refuses_a_machine_loaded_on_purpose() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let stop = Arc::new(AtomicBool::new(false));
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let mut hogs = Vec::new();
    for index in 0..cpus {
        let stop = Arc::clone(&stop);
        hogs.push(std::thread::spawn(move || {
            let mut seed = 0x2545f4914f6cdd1du64 ^ (index as u64 + 1);
            let mut random = move || {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed
            };
            while !stop.load(Ordering::Relaxed) {
                let busy = std::time::Duration::from_micros(1000 + random() % 15_000);
                let started = std::time::Instant::now();
                while started.elapsed() < busy {
                    std::hint::spin_loop();
                }
                std::thread::sleep(std::time::Duration::from_micros(1000 + random() % 15_000));
            }
        }));
    }
    {
        let stop = Arc::clone(&stop);
        hogs.push(std::thread::spawn(move || {
            let mut buffer = vec![0u8; 64 << 20];
            let mut pass = 0u8;
            while !stop.load(Ordering::Relaxed) {
                pass = pass.wrapping_add(1);
                for line in buffer.chunks_mut(64) {
                    line[0] = pass;
                }
            }
        }));
    }

    let workloads: Vec<Workload> = reference_workloads()
        .into_iter()
        .filter(|w| w.name == "compute" || w.name == "memory")
        .collect();
    let result = harness::baselines(&workloads, MIN_REPETITIONS);
    stop.store(true, Ordering::Relaxed);
    for hog in hogs {
        hog.join().unwrap();
    }
    let (baselines, _) = result.expect("the baselines");
    eprint!("{}", gate_table(&baselines));
    let conditions = Conditions {
        foreign_busy_cpus: Conditions::read().foreign_busy_cpus,
        ..performance()
    };
    match judge(&baselines, &conditions) {
        Err(refusal) => say(format!("refused, as it must:\n{refusal}")),
        Ok(_) => panic!(
            "the gate passed a machine loaded on purpose with bursts on all {cpus} cpus:\n{}",
            gate_table(&baselines)
        ),
    }
}

#[test]
fn pairs_are_matched_by_absolute_time_and_a_merged_or_lost_event_is_not_a_pair() {
    // Three pairs 2 ms apart, 5 us gaps. The tool saw the first exactly, the
    // second as one event, and the third with a stray third event inside.
    let stamps = [
        1_000_000, 1_005_000, 3_000_000, 3_005_000, 5_000_000, 5_005_000,
    ];
    let tool = [
        1_000_100, 1_005_100, // pair 0
        3_000_200, // pair 1: one event
        5_000_100, 5_002_000, 5_005_100, // pair 2: three events
        9_000_000, // outside every window
    ];
    let matched = match_pairs(&stamps, &tool, 1_000, 1_000_000);
    assert_eq!(matched.len(), 3);
    assert_eq!(
        matched[0],
        Pair {
            own_gap_ns: 5_000,
            tool_gap_ns: Some(5_000)
        }
    );
    assert_eq!(matched[1].tool_gap_ns, None);
    assert_eq!(matched[2].tool_gap_ns, None);
}

fn stats(values: &[f64]) -> Option<Stats> {
    Stats::of(values)
}

/// A row is held to its declared band on the larger of its two costs: an eBPF
/// program's cost is in the target's dilation, not in the tool's cpu, so a row
/// whose tool cpu is a tenth of a percent and whose target ran 3 % slower is
/// over `low`, and flagged.
#[test]
fn a_row_is_held_to_its_band_on_the_larger_of_its_share_and_its_dilation() {
    use omn::catalog::Cost;
    let cpu = stats(&[1e6, 1e6, 1e6]);
    assert!(matches!(
        judge_band(Cost::Low, 3, cpu, stats(&[0.1, 0.1, 0.1]), stats(&[0.03, 0.03, 0.03]), None),
        Band::Over(why) if why.contains("dilation") && why.contains("3.00 %")
    ));
    assert!(matches!(
        judge_band(Cost::Low, 3, cpu, stats(&[0.5, 0.5, 0.5]), stats(&[0.002, 0.002, 0.002]), None),
        Band::Within(why) if why.contains("share")
    ));
    // `extreme` has no bound to break.
    assert!(matches!(
        judge_band(Cost::Extreme, 3, cpu, stats(&[900.0; 3]), None, None),
        Band::Within(_)
    ));
}

/// A dilation no larger than the workload's resolution is one the bench cannot
/// tell from zero, and reads as zero, as `tests/bench_bands.rs` reads the
/// record: a `free` row whose target ran 0.4 % slower on a workload that
/// resolves 0.6 % is within its band, and the same dilation on one that
/// resolves 0.2 % is over it. Above the resolution the larger column still
/// decides.
#[test]
fn a_dilation_within_the_workloads_resolution_reads_as_zero() {
    use omn::catalog::Cost;
    let cpu = stats(&[1e6, 1e6, 1e6]);
    let dilated = stats(&[0.004, 0.004, 0.004]);
    assert!(matches!(
        judge_band(Cost::Free, 3, cpu, None, dilated, Some(0.006)),
        Band::Within(why) if why.contains("0.00 %") && why.contains("0.60 % resolution")
    ));
    assert!(matches!(
        judge_band(Cost::Free, 3, cpu, None, dilated, Some(0.002)),
        Band::Over(why) if why.contains("0.40 % (dilation)")
    ));
    assert!(matches!(
        judge_band(Cost::Free, 3, cpu, None, dilated, None),
        Band::Over(_)
    ));
    // Within the resolution the share alone decides, whichever is larger.
    assert!(matches!(
        judge_band(Cost::Low, 3, cpu, stats(&[0.2; 3]), stats(&[0.009; 3]), Some(0.01)),
        Band::Within(why) if why.contains("0.20 % (share")
    ));
    // Above it the dilation still counts when it is the larger.
    assert!(matches!(
        judge_band(Cost::Low, 3, cpu, stats(&[0.2; 3]), stats(&[0.03; 3]), Some(0.01)),
        Band::Over(why) if why.contains("3.00 % (dilation)")
    ));
}

/// What makes the table wrong, rather than a band broken: a row with no
/// completed repetition, no collector cpu, or nothing to hold to a band. The
/// bench fails on these; it flags the others.
#[test]
fn a_row_with_a_column_missing_is_refused_rather_than_judged() {
    use omn::catalog::Cost;
    let cpu = stats(&[1e6, 1e6]);
    assert!(matches!(
        judge_band(Cost::Low, 0, None, None, None, None),
        Band::Missing(_)
    ));
    assert!(matches!(
        judge_band(Cost::Low, 2, None, stats(&[0.5, 0.5]), None, None),
        Band::Missing(why) if why.contains("collector cpu")
    ));
    assert!(matches!(
        judge_band(Cost::Free, 2, cpu, None, None, None),
        Band::Missing(_)
    ));
}

/// The backends table is wrong, and its test fails, when it measured nothing,
/// lost a planned row without listing it, holds a row another backend served,
/// or a row missing a column; a row over its band alone is flagged and the
/// table stands. Synthetic rows, each wrong in one way the table test has
/// to catch on the real one.
#[test]
fn a_wrong_backends_table_fails_and_a_row_over_its_band_alone_does_not() {
    fn row<'a>(forced: &'a str, served_by: &'a [String], band: &'a Band) -> TableRow<'a> {
        TableRow {
            label: format!("io.latency through `{forced}`"),
            forced,
            served_by,
            band,
        }
    }
    let within = &Band::Within("0.10 % against `low`'s 1 %".into());
    let over = &Band::Over("7.00 % (dilation) against `low`'s 1 %".into());
    let missing = &Band::Missing("no repetition completed, so no column is measured".into());
    let raw_tp = &["bpf:raw_tp".to_owned()][..];
    let tp = &["bpf:tp".to_owned()][..];

    let good = [row("bpf:raw_tp", raw_tp, within), row("bpf:tp", tp, over)];
    assert_eq!(table_failures(2, 0, &good), Vec::<String>::new());
    // One pair excluded by the gate and listed: still a table.
    assert_eq!(table_failures(3, 1, &good), Vec::<String>::new());

    let empty = table_failures(2, 0, &[]);
    assert!(empty.iter().any(|why| why.contains("no row")), "{empty:?}");
    let lost = table_failures(3, 0, &good);
    assert!(
        lost.iter().any(|why| why.contains("went missing")),
        "{lost:?}"
    );
    let other = table_failures(1, 0, &[row("bpf:raw_tp", tp, within)]);
    assert!(
        other
            .iter()
            .any(|why| why.contains("forced `bpf:raw_tp` and was served by")),
        "{other:?}"
    );
    let unmeasured = table_failures(1, 0, &[row("bpf:raw_tp", raw_tp, missing)]);
    assert!(
        unmeasured
            .iter()
            .any(|why| why.contains("missing a column")),
        "{unmeasured:?}"
    );
}
