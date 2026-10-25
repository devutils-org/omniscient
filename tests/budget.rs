//! What a measurement costs, and what a ceiling does about it.
//!
//! `--budget` was parsed, carried into the resolved request and consulted by
//! nothing: a caller who passed `--budget 1%` and got numbers back had been
//! told, by silence, that the overhead was under one percent. It is enforced
//! now, against a measurement the run takes of itself, and these are the tests
//! that the measurement is a measurement.
//!
//! Ground truth here is injected rather than asked for. A poll at one
//! millisecond does a hundred times the work of a poll at a hundred, and the
//! tool's own account of what it spent has to move with that. A launched
//! program's cpu time is read back from the program itself. And what the tool
//! says it spent is checked against what the shell says it spent, through
//! `times`, which is a different account kept by a different process.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

/// A shell loop that burns cpu without forking, so everything it spends is its
/// own and a `/proc` read of it is a complete account.
const BURN: &str = "i=0; while [ $i -lt 100000000 ]; do i=$((i+1)); done";

fn scratch(name: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!("omn-cost-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    directory
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn run(args: &[&str]) -> Output {
    Command::new(BINARY)
        .args(args)
        .output()
        .expect("omn should run")
}

/// What the tool wrote down about what it cost, beside the numbers.
fn cost_record(directory: &Path) -> serde_json::Value {
    let text = std::fs::read_to_string(directory.join("cost.json"))
        .expect("a run with --out writes what it cost beside what it measured");
    let costs: serde_json::Value = serde_json::from_str(&text).expect("cost.json is json");
    costs
        .as_array()
        .and_then(|costs| costs.first().cloned())
        .expect("one request, one cost")
}

/// One run of `mem.rss` over a cpu-burning program, with whatever flags the
/// test wants, and the record it wrote of what it cost.
fn measured(name: &str, extra: &[&str]) -> (Output, serde_json::Value) {
    let directory = scratch(name);
    let out = directory.to_string_lossy().into_owned();
    let mut args = vec![
        "mem.rss",
        "-e",
        "summary",
        "-d",
        "500ms",
        "-o",
        out.as_str(),
    ];
    args.extend_from_slice(extra);
    args.extend_from_slice(&["--", "/bin/sh", "-c", BURN]);
    let output = run(&args);
    assert!(
        output.status.success(),
        "the run itself has to succeed: {}",
        stderr(&output)
    );
    let cost = cost_record(&directory);
    let _ = std::fs::remove_dir_all(&directory);
    (output, cost)
}

fn percent(cost: &serde_json::Value) -> f64 {
    cost["measured_percent"]
        .as_f64()
        .expect("a launched program burns cpu, so there is something to be a percentage of")
}

/// How many reads a `mem.rss` run actually made: one event each, from the
/// results it wrote in JSON.
fn reads_made(name: &str, extra: &[&str]) -> (serde_json::Value, u64) {
    let directory = scratch(name);
    let out = directory.to_string_lossy().into_owned();
    let mut args = vec![
        "mem.rss",
        "-e",
        "summary",
        "-d",
        "500ms",
        "--format",
        "json",
        "-o",
        out.as_str(),
    ];
    args.extend_from_slice(extra);
    args.extend_from_slice(&["--", "/bin/sh", "-c", BURN]);
    let output = run(&args);
    assert!(output.status.success(), "{}", stderr(&output));
    let cost = cost_record(&directory);
    let results: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(directory.join("results.json")).expect("results.json"),
    )
    .expect("json");
    let _ = std::fs::remove_dir_all(&directory);
    let reads = results["results"][0]["events_seen"]
        .as_u64()
        .expect("the run says how many reads it made");
    (cost, reads)
}

/// The cost the tool reports moves with the work it was asked to do.
///
/// This is the injected ground truth: the two runs differ in one thing, the
/// poll interval, and one of them reads `/proc` a hundred times as often as the
/// other over the same window against the same program. A tool reporting a
/// plausible constant would pass every other check in this file and fail this
/// one.
///
/// Weighed by the reads each run *made*, which it reports, rather than by the
/// ones it was asked for: under load a round that overruns re-bases the
/// schedule, so a poll at one millisecond makes far fewer than five hundred
/// reads in half a second, and a fixed ratio of the two costs then asserted
/// what the machine gave the fast run (the flaky tally, #13).
#[test]
fn what_the_tool_says_it_spent_moves_with_how_often_it_was_asked_to_read() {
    let (often, often_reads) = reads_made("often", &["-m", "poll@1ms"]);
    let (rarely, rarely_reads) = reads_made("rarely", &["-m", "poll@100ms"]);

    let often_ns = often["overhead"]["collector_cpu_ns"].as_u64().unwrap();
    let rarely_ns = rarely["overhead"]["collector_cpu_ns"].as_u64().unwrap();
    if often_reads < rarely_reads * 5 {
        eprintln!(
            "SKIP: this machine gave the fast poll {often_reads} reads against the slow one's \
             {rarely_reads}, too few apart for their costs to separate; they cost {often_ns}ns \
             and {rarely_ns}ns"
        );
        return;
    }
    // A read of `/proc/<pid>/smaps_rollup` is an open, a read and a close:
    // three system calls, and never under a microsecond of this process's cpu.
    const A_READ_AT_LEAST_NS: u64 = 1_000;
    assert!(
        often_ns > rarely_ns + (often_reads - rarely_reads) * A_READ_AT_LEAST_NS,
        "{often_reads} reads cost {often_ns}ns and {rarely_reads} cost {rarely_ns}ns: the \
         reads the faster poll made in excess are not in its account"
    );
    assert!(
        percent(&often) > percent(&rarely),
        "and the percentage has to move the same way: {} against {}",
        percent(&often),
        percent(&rarely)
    );
}

/// The denominator is the program's own cpu time, and the program agrees.
///
/// The launched shell reads its own `/proc/self/stat` when the loop ends and
/// writes it out with builtins only, so nothing is forked and nothing is left
/// out. That number and the one `omn` reports are two independent readings of
/// the same quantity -- one taken by the program about itself, one taken by the
/// tool through `getrusage` once the child was reaped.
#[test]
fn the_cpu_the_tool_charges_the_program_is_the_cpu_the_program_charges_itself() {
    let directory = scratch("account");
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    let own_account = directory.join("stat");
    let program = format!(
        "i=0; while [ $i -lt 3000000 ]; do i=$((i+1)); done; read s < /proc/self/stat; \
         echo \"$s\" > {}",
        own_account.display()
    );
    let out = directory.to_string_lossy().into_owned();
    let output = run(&[
        "mem.rss",
        "-e",
        "summary",
        "-m",
        "poll@50ms",
        "-o",
        out.as_str(),
        "--",
        "/bin/sh",
        "-c",
        &program,
    ]);
    assert!(output.status.success(), "{}", stderr(&output));

    let stat = std::fs::read_to_string(&own_account)
        .expect("the program reports its own state, which is the whole point of this check");
    // utime, stime, cutime, cstime: fields 14 to 17, counted from after the
    // parenthesised command name.
    let fields: Vec<&str> = stat[stat.rfind(')').expect("a comm") + 1..]
        .split_whitespace()
        .collect();
    let ticks: u64 = fields[11..15]
        .iter()
        .map(|field| field.parse::<u64>().unwrap_or(0))
        .sum();
    let by_itself_ns = ticks * 10_000_000;

    let cost = cost_record(&directory);
    let by_the_tool_ns = cost["overhead"]["observed_cpu_ns"].as_u64().unwrap();
    let _ = std::fs::remove_dir_all(&directory);

    assert!(
        by_itself_ns > 50_000_000,
        "the program has to have burned something for this to compare anything: {by_itself_ns}ns"
    );
    // The program's own reading is in ten-millisecond ticks and is taken just
    // before it exits; the tool's covers the exit as well. Two ticks either way
    // is the whole of the disagreement that can be legitimate.
    assert!(
        by_the_tool_ns.abs_diff(by_itself_ns) <= 30_000_000,
        "the tool charged the program {by_the_tool_ns}ns and the program charged itself \
         {by_itself_ns}ns"
    );
}

/// What the tool says it spent is what the shell says it spent.
///
/// `times` is a POSIX shell builtin that reports the cpu time of the children
/// the shell has waited for -- the same account `time` prints, kept by a
/// different process and reached through a different interface. It covers the
/// whole of `omn`, including loading the catalog and probing the machine, which
/// the reported cost deliberately excludes; so the two cannot be compared
/// directly. The *difference* between two runs can: the startup is the same in
/// both, and what is left is the polling.
///
/// Over two seconds, so that the polling is most of the difference: over half
/// a second, the catalog load and the probe of two separate runs differed under
/// load by more than the forty milliseconds the faster poll added (the flaky
/// tally, #14).
#[test]
fn the_extra_cpu_a_faster_poll_costs_is_the_extra_cpu_the_shell_observes() {
    let cpu_ns = |interval: &str, directory: &Path| {
        let out = directory.to_string_lossy().into_owned();
        let script = format!(
            "\"{BINARY}\" mem.rss -e summary -m poll@{interval} -d 2s -o \"{out}\" \
             -- /bin/sh -c '{BURN}' >/dev/null 2>&1; times"
        );
        let shell = Command::new("/bin/sh")
            .args(["-c", &script])
            .output()
            .expect("a shell");
        let printed = String::from_utf8_lossy(&shell.stdout).into_owned();
        // `times` prints two lines: this shell, then its children. The second
        // is the one that covers `omn`.
        let children = printed.lines().nth(1).unwrap_or_default().to_owned();
        let observed_ns: u64 = children
            .split_whitespace()
            .map(|field| {
                let (minutes, seconds) = field.split_once('m').unwrap_or(("0", "0s"));
                let minutes: f64 = minutes.parse().unwrap_or(0.0);
                let seconds: f64 = seconds.trim_end_matches('s').parse().unwrap_or(0.0);
                ((minutes * 60.0 + seconds) * 1e9) as u64
            })
            .sum();
        let reported_ns = cost_record(directory)["overhead"]["collector_cpu_ns"]
            .as_u64()
            .expect("a reported cost");
        assert!(
            observed_ns >= reported_ns,
            "the shell saw {observed_ns}ns and the tool claimed {reported_ns}ns of it, which is \
             more than the whole"
        );
        (observed_ns, reported_ns)
    };

    let often = scratch("times-often");
    let rarely = scratch("times-rarely");
    let (observed_often_ns, reported_often_ns) = cpu_ns("1ms", &often);
    let (observed_rarely_ns, reported_rarely_ns) = cpu_ns("100ms", &rarely);
    let _ = std::fs::remove_dir_all(&often);
    let _ = std::fs::remove_dir_all(&rarely);

    let observed_extra_ns = observed_often_ns.saturating_sub(observed_rarely_ns) as f64;
    let reported_extra_ns = reported_often_ns.saturating_sub(reported_rarely_ns) as f64;
    assert!(
        reported_extra_ns > 1_000_000.0,
        "the faster poll has to have cost something: {reported_extra_ns}ns"
    );
    // `times` rounds to ten milliseconds and the shell is doing a little of its
    // own work, so this is an order-of-magnitude agreement rather than an
    // equality -- which is still enough to catch a number that is made up.
    //
    // The rounding is carried explicitly rather than folded into the ratio.
    // Two runs are two roundings, so the difference between them is a tenth of
    // a second wide before anything else happens; measured under a loaded
    // machine at 9 ms observed against 52 ms reported, which a pure ratio reads
    // as a sixfold disagreement and is one quantum of the reference.
    const TIMES_QUANTUM_NS: f64 = 10_000_000.0;
    let slack = 2.0 * TIMES_QUANTUM_NS;
    assert!(
        observed_extra_ns > reported_extra_ns * 0.4 - slack
            && observed_extra_ns < reported_extra_ns * 3.0 + slack,
        "the shell saw {observed_extra_ns}ns more cpu in the faster run and the tool reported \
         {reported_extra_ns}ns more, which is further apart than the shell's own \
         ten-millisecond resolution explains"
    );
}

/// A ceiling the run stays under is reported as met, and one it exceeds is not.
///
/// The two runs differ only in the ceiling, and the ceilings are placed either
/// side of what the same measurement costs, so the verdicts have to differ. A
/// tool that reported "held" whatever happened would pass the first half.
#[test]
fn a_ceiling_is_met_or_exceeded_according_to_what_the_run_measured() {
    let (_, cost) = measured("reference", &["-m", "poll@2ms"]);
    let reference = percent(&cost);
    assert!(
        reference > 0.05,
        "this test needs a poll that costs something measurable: {reference} %"
    );

    let generous = format!("{}%", reference * 20.0);
    let (loose, loose_cost) = measured("loose", &["-m", "poll@2ms", "--budget", &generous]);
    assert_eq!(
        loose_cost["within_budget"],
        serde_json::Value::Bool(true),
        "a run well inside its ceiling: {}",
        stderr(&loose)
    );
    assert!(
        stderr(&loose).contains("held"),
        "and the reader is told so: {}",
        stderr(&loose)
    );

    let tight = format!("{}%", reference / 20.0);
    let (strict, strict_cost) = measured("strict", &["-m", "poll@2ms", "--budget", &tight]);
    assert_eq!(
        strict_cost["within_budget"],
        serde_json::Value::Bool(false),
        "and one well outside it: {}",
        stderr(&strict)
    );
    assert!(
        stderr(&strict).contains("was exceeded"),
        "and is told that too: {}",
        stderr(&strict)
    );
}

/// Over budget, with something cheaper below it in the chain, the run moves
/// onto it rather than ending -- and says when it moved.
///
/// `mem.rss` is the chain this can be shown on: `fs:proc:smaps_rollup` first,
/// `fs:proc:statm` below it, both with a collector. The note has to name the
/// moment, because the series is then two mechanisms end to end and a reader
/// comparing its two halves is comparing two different measurements.
#[test]
fn a_run_over_its_ceiling_moves_onto_a_cheaper_backend_and_says_when() {
    // The ceiling has to be one `fs:proc:statm` can promise -- it declares
    // `free`, which is "under 0.1 %" -- or there would be no rung to step onto
    // and the run would stop instead, which is the test below this one.
    let (output, cost) = measured("degrade", &["-m", "poll@1ms", "--budget", "0.5%"]);
    let served: Vec<String> = cost["served_by"]
        .as_array()
        .expect("a list of backends")
        .iter()
        .map(|backend| backend.as_str().unwrap_or_default().to_owned())
        .collect();

    assert_eq!(
        served,
        vec![
            "fs:proc:smaps_rollup".to_owned(),
            "fs:proc:statm".to_owned()
        ],
        "the run had to end on the cheaper backend: {}",
        stderr(&output)
    );
    let notes = cost["notes"].as_array().expect("notes").clone();
    let moved = notes
        .iter()
        .filter_map(|note| note.as_str())
        .find(|note| note.contains("over budget at"))
        .expect("the moment it moved is the part a reader needs");
    assert!(
        moved.contains("fs:proc:smaps_rollup") && moved.contains("fs:proc:statm"),
        "and which way it moved: {moved}"
    );
}

/// Over budget with nothing cheaper below it, the run stops and the reason
/// names the ceiling rather than the clock.
///
/// The backend is pinned to the bottom of the chain, so the ladder is empty by
/// construction. Both sides are checked: the same request with a ceiling
/// nothing can exceed ends for the reason it was going to end for anyway.
#[test]
fn a_run_over_its_ceiling_with_nowhere_to_go_stops_and_says_so() {
    let stopped = |budget: &str| {
        let (output, cost) = measured(
            "stop",
            &[
                "-m",
                "poll@1ms",
                "--backend",
                "force:fs:proc:statm",
                "--budget",
                budget,
            ],
        );
        let line = stderr(&output)
            .lines()
            .find(|line| line.contains("and the run was stopped there"))
            .map(str::to_owned);
        (
            cost["stopped_because"]
                .as_str()
                .expect("a reason")
                .to_owned(),
            cost["stopped_by_budget"].as_bool().unwrap_or(false),
            line,
            stderr(&output),
        )
    };

    let (over, flagged, line, said) = stopped("0.05%");
    assert!(
        over.contains("ceiling of 0.05") && over.contains("nothing cheaper"),
        "a run the budget ended says which ceiling and that there was nowhere to go: {over}"
    );
    // And says it was stopped on a line of its own, naming the source and
    // the percentage, whatever the whole window's verdict came to.
    let line = line.unwrap_or_else(|| panic!("no line says the run was stopped: {said}"));
    assert!(
        line.starts_with("budget: [mem.rss] --budget 0.05% was exceeded")
            && line.contains("% of the target's cpu time"),
        "{line}"
    );
    assert!(flagged, "and the cost record says so too");

    let (under, flagged, line, said) = stopped("10000%");
    assert!(
        under.contains("duration") || under.contains("exited"),
        "a run inside its ceiling ends for the ordinary reason: {under}"
    );
    assert!(
        line.is_none() && !flagged,
        "a run the budget did not stop is not said to have been: {said}"
    );
}

/// A program that waits while a process it started does the work is weighed
/// with that process, and a stop ends the process too.
///
/// A compiler driver spent 4 ms in `wait4` while `cc1` spent seconds, and the
/// budget weighed the tool against the driver alone: *"8581.01 % of the
/// target's cpu time"* (`docs/fixes/w17-build.md`). And the stop killed the
/// driver, leaving `cc1` running, reparented. Here a shell waits on a child
/// shell that burns: the target's cpu has to hold the child's, which is two
/// orders of magnitude over what the waiting shell spends of its own, and the
/// child -- which says its pid before it burns -- is not running once the run
/// has returned.
#[test]
fn a_waiting_program_is_weighed_with_the_process_it_started_and_its_stop_ends_both() {
    let directory = scratch("tree");
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    let pid_file = directory.join("child.pid");
    let inner = format!("echo $$ > {}; {BURN}", pid_file.display());
    let program = format!("/bin/sh -c '{inner}'; :");
    let out = directory.join("out").to_string_lossy().into_owned();
    let output = run(&[
        "mem.rss",
        "-e",
        "summary",
        "-m",
        "poll@50ms",
        "-d",
        "1s",
        "-o",
        out.as_str(),
        "--",
        "/bin/sh",
        "-c",
        &program,
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let cost = cost_record(Path::new(&out));
    let child: i32 = std::fs::read_to_string(&pid_file)
        .expect("the child says its pid before it burns")
        .trim()
        .parse()
        .expect("a pid");
    // Gone, or a zombie waiting for whoever inherited it: either way it runs
    // no more. Given a moment, since the reap is not this process's to make.
    let running = || {
        std::fs::read_to_string(format!("/proc/{child}/stat")).is_ok_and(|stat| {
            stat.rfind(')')
                .and_then(|end| stat[end + 1..].split_whitespace().next())
                .is_some_and(|state| state != "Z" && state != "X")
        })
    };
    let mut still = running();
    for _ in 0..100 {
        if !still {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
        still = running();
    }
    if still {
        // SAFETY: ending the child this test's program started, which the run
        // should have ended.
        unsafe { libc::kill(child, libc::SIGKILL) };
    }
    let _ = std::fs::remove_dir_all(&directory);

    let observed = cost["overhead"]["observed_cpu_ns"]
        .as_u64()
        .expect("a launched program has a cpu account");
    assert!(
        observed >= 50_000_000,
        "a second of a burning child came to {observed} ns of the target's cpu, which is the \
         waiting shell's alone: {}",
        cost["overhead"]["observed_from"]
    );
    assert!(
        !still,
        "the child {child} the program started is still running after the run returned"
    );
}

/// An orphan the program left -- its parent gone first, in a session of its
/// own, adopted by `omn` -- is in the program's denominator while it runs
/// (`DECISIONS.md`, an adopted orphan's cpu). The program here sleeps two
/// seconds while its orphan burns one: weighed against the sleeping shell
/// alone, `mem.rss`'s cost read hundreds of percent mid-run and a 5 % budget
/// moved it off `smaps_rollup` onto `statm` in each of three runs; weighed
/// with the orphan it is under 1 %, and the request is served as asked.
#[test]
fn an_orphan_the_program_left_is_weighed_with_it_while_it_runs() {
    if Command::new("python3").arg("-V").output().is_err() {
        eprintln!("skipped: no python3 to burn a second of cpu in the orphan");
        return;
    }
    let directory = scratch("orphan");
    let out = directory.join("out").to_string_lossy().into_owned();
    let program = "( setsid python3 -c 'import time\nt = time.process_time()\n\
                   while time.process_time() - t < 1.0: pass\ntime.sleep(5)' & ); sleep 2";
    let output = run(&[
        "mem.rss",
        "-e",
        "summary",
        "-m",
        "poll@100ms",
        "--budget",
        "5%",
        "-o",
        out.as_str(),
        "--",
        "/bin/sh",
        "-c",
        program,
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let cost = cost_record(Path::new(&out));
    let _ = std::fs::remove_dir_all(&directory);
    let observed = cost["overhead"]["observed_cpu_ns"].as_u64().unwrap_or(0);
    assert!(
        observed >= 500_000_000,
        "the orphan's second of cpu is not in the target's {observed} ns: {}",
        cost["overhead"]["observed_from"]
    );
    assert_eq!(
        cost["served_by"],
        serde_json::json!(["fs:proc:smaps_rollup"]),
        "the budget moved the request although the program's tree, orphan included, spent a \
         second: {}",
        stderr(&output)
    );
}

/// The help and the run tell the same story about `--budget`.
///
/// The help once read "the resolver degrades to stay under it", describing a
/// tool that did not exist, and was then corrected to say it was not enforced
/// at all. Both places are checked against each other so that whoever changes
/// what the flag does has to change what it says, or this fails.
#[test]
fn the_help_and_the_run_agree_that_a_budget_is_enforced() {
    let help = String::from_utf8_lossy(&run(&["trace", "--help"]).stdout).into_owned();
    assert!(
        !help.contains("Not yet enforced"),
        "the flag is enforced; the help must not still say otherwise: {help}"
    );
    assert!(
        help.contains("Enforced"),
        "and must say what it does: {help}"
    );

    let note = stderr(&run(&[
        "io.latency",
        "--system",
        "-m",
        "count",
        "-e",
        "summary",
        "--budget",
        "0.1%",
        "--dump-request",
    ]));
    // Counted, because `/proc/diskstats` serves only a count: a traced run has
    // nothing free below it to degrade onto.
    // Which of the two answers comes back depends on what this machine can
    // serve, and asserting only one of them is asserting the machine. Where the
    // chain's best backend costs more than the ceiling, the note names what it
    // would degrade onto. Where tracefs is unreadable and `io.latency` falls
    // through to `/proc/diskstats`, the selected backend is already free and
    // there is nothing to plan. Both are the flag being enforced.
    assert!(
        note.contains("will degrade onto") || note.contains("which is within it"),
        "before the run, a budget says either what exceeding it would do or that \
         the backend selected is already under it: {note}"
    );
    assert!(
        !note.contains("not yet enforced"),
        "and no longer says it does nothing: {note}"
    );
}

/// A chain with nothing cheaper below the selected backend says so in advance.
///
/// Saying "it would degrade onto" there would name a backend that does not
/// exist; the honest answer is that exceeding the ceiling can only end the run.
#[test]
fn a_budget_no_backend_in_the_chain_can_meet_says_that_rather_than_inventing_one() {
    let output = run(&[
        "mem.heap.alloc",
        "--pid",
        "1",
        "-e",
        "summary",
        "--budget",
        "0.1%",
        "--dump-request",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let note = stderr(&output);
    assert!(
        note.contains("stop rather than degrade"),
        "every backend of `mem.heap.alloc` declares med or high, so a 0.1 % budget has nowhere \
         to go: {note}"
    );
}

/// A request with no budget is told nothing about budgets.
///
/// The advance note exists because a silence was misleading; adding a different
/// silence-shaped line to every other run would be its own noise.
#[test]
fn a_request_without_a_budget_is_not_told_about_budgets() {
    let output = run(&[
        "io.latency",
        "--system",
        "-m",
        "trace",
        "-e",
        "hist",
        "--dump-request",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!stderr(&output).contains("budget"), "{}", stderr(&output));
}

/// Every run says what it cost, whether or not it was asked.
///
/// A tool that reports what a program spends and never its own price is asking
/// to be trusted about the one number it never shows.
#[test]
fn a_run_reports_its_own_price_without_being_asked() {
    let (output, _) = measured("unasked", &["-m", "poll@10ms"]);
    let said = stderr(&output);
    assert!(
        said.contains("cost: mem.rss") && said.contains("of cpu"),
        "the price belongs on every run: {said}"
    );
    assert!(
        !said.contains("budget"),
        "and a run that asked for no ceiling is told about none: {said}"
    );
}

/// The self-benchmark: two backends of one source, over one target, in one
/// window, with what each cost and whether they agreed.
///
/// Running one backend, then the other, and comparing would measure two
/// different moments; the whole value of this arrangement is that it does not.
/// `fs:proc:smaps` walks every mapping and `fs:proc:smaps_rollup` reads the
/// kernel's own total, so they must agree closely on the number and differ
/// clearly on the price -- which is the claim the catalog's cost ordering makes
/// and which nothing had ever checked.
#[test]
fn two_backends_of_one_source_are_measured_against_each_other_in_one_window() {
    let directory = scratch("bench");
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    let session = directory.join("bench.yml");
    std::fs::write(
        &session,
        format!(
            "requests:\n\
             {}\n\
             {}\n",
            request("rollup", "fs:proc:smaps_rollup"),
            request("walk", "fs:proc:smaps"),
        ),
    )
    .expect("a session file");

    let output = run(&[
        "trace",
        "-f",
        &session.to_string_lossy(),
        "-o",
        &directory.to_string_lossy(),
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let said = stderr(&output);
    assert!(
        said.contains("self-benchmark: `mem.pss` through 2 backends, one target, one window"),
        "two backends of one source is a comparison, and the tool has to present it as one: \
         {said}"
    );

    let costs: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(directory.join("cost.json")).expect("cost.json"),
    )
    .expect("json");
    let costs = costs.as_array().expect("two records").clone();
    let _ = std::fs::remove_dir_all(&directory);
    assert_eq!(costs.len(), 2);

    let by = |id: &str| {
        costs
            .iter()
            .find(|cost| cost["backend"] == id)
            .unwrap_or_else(|| panic!("a record for {id}"))
            .clone()
    };
    let rollup_ns = by("fs:proc:smaps_rollup")["overhead"]["attributed_cpu_ns"]
        .as_u64()
        .unwrap();
    let walk_ns = by("fs:proc:smaps")["overhead"]["attributed_cpu_ns"]
        .as_u64()
        .unwrap();
    assert!(
        walk_ns > rollup_ns,
        "walking every mapping cannot cost less than reading the kernel's rollup of them, but \
         the walk came to {walk_ns}ns against {rollup_ns}ns"
    );

    // Both were weighed against the same target over the same window, which is
    // what makes the two prices comparable at all.
    assert_eq!(
        by("fs:proc:smaps_rollup")["overhead"]["observed_cpu_ns"],
        by("fs:proc:smaps")["overhead"]["observed_cpu_ns"],
        "one window, one target, one denominator"
    );
}

fn request(label: &str, backend: &str) -> String {
    format!(
        "  - label: {label}\n    \
         source: mem.pss\n    \
         target: {{launch: [\"/bin/sh\", \"-c\", \"{BURN}\"]}}\n    \
         mode: poll@20ms\n    \
         emit: [summary]\n    \
         duration: 500ms\n    \
         backend: \"force:{backend}\"",
    )
}

/// A request attached for part of the run pays for, and is weighed against,
/// its own window.
///
/// Ground truth is a program that burns one cpu from its first instruction to
/// the end of the run: over any span, the cpu it spends is at most that span's
/// wall time, and on a machine that lets it run it is most of it. So the
/// denominator of a request attached for 250 ms of a 750 ms run is a third of
/// the whole run's, whatever the machine is doing -- asserted as a separation,
/// not a ratio. And the attach itself, made while the program ran, is named in
/// the record rather than left inside a number.
#[test]
fn a_request_attached_for_part_of_the_run_is_weighed_over_its_own_window() {
    let directory = scratch("window");
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    let session = directory.join("window.yml");
    let request = |label: &str, window: &str| {
        format!(
            "  - label: {label}\n    source: mem.rss\n    \
             target: {{launch: [\"/bin/sh\", \"-c\", \"{BURN}\"]}}\n    \
             mode: poll@20ms\n    emit: [summary]\n    duration: 750ms\n{window}"
        )
    };
    std::fs::write(
        &session,
        format!(
            "requests:\n{}\n{}\n",
            request("whole", ""),
            request("part", "    window: 250ms..500ms"),
        ),
    )
    .expect("a session file");
    let output = run(&[
        "-f",
        &session.to_string_lossy(),
        "-o",
        &directory.to_string_lossy(),
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let costs: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(directory.join("cost.json")).expect("cost.json"),
    )
    .expect("json");
    let _ = std::fs::remove_dir_all(&directory);
    let by = |label: &str| {
        costs
            .as_array()
            .expect("two records")
            .iter()
            .find(|cost| cost["label"] == label)
            .unwrap_or_else(|| panic!("a record for {label}"))
            .clone()
    };
    let (whole, part) = (by("whole"), by("part"));
    let ns = |cost: &serde_json::Value, field: &str| {
        cost["overhead"][field]
            .as_u64()
            .unwrap_or_else(|| panic!("`{field}` in {cost}"))
    };
    eprintln!(
        "whole: {} ns of wall, the program spent {} ns; part: {} ns of wall, the program spent \
         {} ns",
        ns(&whole, "wall_ns"),
        ns(&whole, "observed_cpu_ns"),
        ns(&part, "wall_ns"),
        ns(&part, "observed_cpu_ns"),
    );
    assert!(
        (230_000_000..400_000_000).contains(&ns(&part, "wall_ns")),
        "a window of 250 ms is weighed over its own wall time: {part}"
    );
    assert!(ns(&whole, "wall_ns") >= 750_000_000);
    // A program cannot spend more cpu than wall time on one thread, and the
    // window holds a third of the run's.
    assert!(
        ns(&part, "observed_cpu_ns") <= ns(&part, "wall_ns") + 10_000_000,
        "the denominator is the program's cpu over the window, not over the run: {part}"
    );
    assert!(
        ns(&part, "observed_cpu_ns") * 2 < ns(&whole, "observed_cpu_ns"),
        "the window's denominator is a fraction of the run's: {part} against {whole}"
    );
    let notes: Vec<&str> = part["notes"]
        .as_array()
        .expect("notes")
        .iter()
        .filter_map(|note| note.as_str())
        .collect();
    assert!(
        notes
            .iter()
            .any(|note| note.contains("from 0.25") && note.contains("attaching it took")),
        "the attach made while the program ran is in the record: {notes:?}"
    );
    assert!(
        part["stopped_because"]
            .as_str()
            .is_some_and(|why| why.contains("window closed")),
        "{part}"
    );
}

// ------------------------------------------------ what a run costs the target

/// The bench's reference workloads (`examples/bench/workload.c`), built once
/// per test process: programs that time their own work and report it, which
/// is what a cost inside the target is checked against.
fn workload() -> Option<PathBuf> {
    use std::sync::OnceLock;
    static BUILT: OnceLock<Option<PathBuf>> = OnceLock::new();
    BUILT
        .get_or_init(|| {
            let directory = std::env::temp_dir().join(format!("omn-cost-{}", std::process::id()));
            std::fs::create_dir_all(&directory).ok()?;
            let path = directory.join("workload");
            let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/bench/workload.c");
            let built = Command::new("cc")
                .args(["-O2", "-pthread", "-o"])
                .arg(&path)
                .arg(source)
                .output()
                .ok()?;
            if !built.status.success() {
                eprintln!(
                    "SKIP: the reference workload did not build: {}",
                    String::from_utf8_lossy(&built.stderr)
                );
                return None;
            }
            Some(path)
        })
        .clone()
}

/// One run of the workload, bare or under `omn`: what the workload says it
/// did and spent, and what `omn` said it cost when it was there.
struct Timed {
    /// The workload's own report: `elapsed_ns`, `cpu_ns`, `ops`.
    report: serde_json::Value,
    cost: Option<serde_json::Value>,
    output: Option<Output>,
}

impl Timed {
    fn cpu_ns(&self) -> u64 {
        self.report["cpu_ns"]
            .as_u64()
            .expect("the workload reports its cpu")
    }
}

fn timed(name: &str, omn: Option<&[&str]>, workload_args: &[&str]) -> Timed {
    timed_under(name, omn, &[], workload_args)
}

/// [`timed`], with the workload started through `wrapper` (`taskset -c 3`),
/// under `omn` as bare.
fn timed_under(
    name: &str,
    omn: Option<&[&str]>,
    wrapper: &[&str],
    workload_args: &[&str],
) -> Timed {
    let directory = scratch(name);
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    let report = directory.join("report.json");
    let program = workload().expect("the workload was built");
    let program = program.to_string_lossy().into_owned();
    let mut command: Vec<&str> = wrapper.to_vec();
    command.push(&program);
    command.extend_from_slice(workload_args);
    let output = match omn {
        None => {
            let status = Command::new(command[0])
                .args(&command[1..])
                .env("BENCH_REPORT", &report)
                .status()
                .expect("the workload runs");
            assert!(status.success());
            None
        }
        Some(request) => {
            let out = directory.join("out");
            let out = out.to_string_lossy().into_owned();
            let mut args: Vec<&str> = request.to_vec();
            args.extend_from_slice(&["-o", &out, "--"]);
            args.extend_from_slice(&command);
            let output = Command::new(BINARY)
                .args(&args)
                .env("BENCH_REPORT", &report)
                .output()
                .expect("omn runs");
            Some(output)
        }
    };
    let text = std::fs::read_to_string(&report).unwrap_or_default();
    let cost = output
        .as_ref()
        .filter(|output| output.status.success())
        .map(|_| cost_record(&directory.join("out")));
    let _ = std::fs::remove_dir_all(&directory);
    Timed {
        report: serde_json::from_str(text.trim()).unwrap_or(serde_json::Value::Null),
        cost,
        output,
    }
}

fn median(mut values: Vec<u64>) -> u64 {
    values.sort_unstable();
    values[values.len() / 2]
}

/// Finding 47's own case: `proc.syscall` in `count` on a program that does
/// nothing but syscalls. The tool's cpu is a fraction of a percent of the
/// program's; the program's timed loop runs about four times slower, because
/// the eBPF programs run on every syscall entry and exit in its context.
///
/// Two answers are right, and which one this machine gives depends on
/// whether the kernel is timing eBPF programs (`kernel.bpf_stats_enabled`, or
/// `BPF_ENABLE_STATS`, which needs `CAP_SYS_ADMIN`). Where it is not, the
/// in-target cost is **unknown**, the record says why, and the percentage is
/// the tool's alone -- never a zero standing in for what was not measured.
/// Where it is, the kernel's own count of the programs' runs is checked
/// against the calls the workload says it made (two a call: entry and exit),
/// and the run time against what the program itself lost, which it bounds from
/// below.
#[test]
fn what_an_ebpf_program_spends_inside_the_target_is_measured_or_said_to_be_unknown() {
    if workload().is_none() {
        return;
    }
    const CALLS: u64 = 200_000;
    let calls = CALLS.to_string();
    let request: &[&str] = &["proc.syscall", "-m", "count", "-e", "summary"];
    let traced = timed("syscall-traced", Some(request), &["syscall", &calls]);
    let output = traced.output.as_ref().expect("omn ran");
    let said = stderr(output);
    if !output.status.success() {
        assert!(
            said.contains("CAP_BPF") || said.contains("not available") || said.contains("no "),
            "a run that cannot trace syscalls says why: {said}"
        );
        eprintln!("SKIP: `proc.syscall` cannot be served here: {said}");
        return;
    }
    let cost = traced.cost.as_ref().expect("a cost record");
    let overhead = &cost["overhead"];
    let from = overhead["in_target_from"].as_str().unwrap_or_default();

    let Some(inside_ns) = overhead["in_target_cpu_ns"].as_u64() else {
        // Unknown: said, explained, and left out of the percentage.
        assert!(
            overhead["in_target_cpu_ns"].is_null(),
            "unknown is null, never zero: {overhead}"
        );
        assert!(
            from.contains("bpf_stats_enabled") && from.contains("BPF_ENABLE_STATS"),
            "and the record says what would make it measurable: {from}"
        );
        assert_eq!(
            cost["measured_percent"], cost["own_percent"],
            "with nothing known inside the target, the percentage is the tool's own: {cost}"
        );
        assert!(
            said.contains("unknown amount inside the target"),
            "the line a reader gets says so too: {said}"
        );
        // And a ceiling cannot be said to have held over what was not weighed.
        let budgeted = timed(
            "syscall-budget",
            Some(&[
                "proc.syscall",
                "-m",
                "count",
                "-e",
                "summary",
                "--budget",
                "1000%",
            ]),
            &["syscall", &calls],
        );
        let budget_cost = budgeted.cost.expect("a cost record");
        assert!(
            budget_cost["within_budget"].is_null(),
            "a budget over an unknown in-target cost is neither held nor exceeded: {budget_cost}"
        );
        assert!(
            stderr(budgeted.output.as_ref().unwrap()).contains("could not be checked whole"),
            "{}",
            stderr(budgeted.output.as_ref().unwrap())
        );
        eprintln!(
            "SKIP (the measured half): the kernel is not timing eBPF programs on this machine \
             (`kernel.bpf_stats_enabled` = {}), so what `proc.syscall` spent inside the target \
             is unknown, as the run said: {from}",
            std::fs::read_to_string("/proc/sys/kernel/bpf_stats_enabled")
                .unwrap_or_default()
                .trim()
        );
        return;
    };

    // Measured: the kernel's run count against the workload's own count.
    let ops = traced.report["ops"].as_u64().expect("ops");
    let runs = overhead["in_target_runs"]
        .as_u64()
        .expect("runs are counted");
    assert!(
        runs >= 2 * ops,
        "the programs run on every entry and every exit of the {ops} calls the workload made, \
         and the kernel counted {runs} runs"
    );
    // And the run time against what the program lost: three bare runs, three
    // traced, the program's own cpu time each.
    let bare: Vec<u64> = (0..3)
        .map(|i| timed(&format!("syscall-bare-{i}"), None, &["syscall", &calls]).cpu_ns())
        .collect();
    let mut runs_traced = vec![traced];
    for i in 1..3 {
        runs_traced.push(timed(
            &format!("syscall-traced-{i}"),
            Some(request),
            &["syscall", &calls],
        ));
    }
    let lost: Vec<u64> = runs_traced
        .iter()
        .map(|run| run.cpu_ns().saturating_sub(median(bare.clone())))
        .collect();
    let spread = bare.iter().max().unwrap() - bare.iter().min().unwrap()
        + lost.iter().max().unwrap()
        - lost.iter().min().unwrap();
    let inside: Vec<u64> = runs_traced
        .iter()
        .filter_map(|run| run.cost.as_ref()?["overhead"]["in_target_cpu_ns"].as_u64())
        .collect();
    eprintln!("bare cpu {bare:?}, lost {lost:?}, reported inside {inside:?}, spread {spread}");
    assert!(
        inside_ns > 0
            && inside
                .iter()
                .all(|ns| *ns <= lost.iter().max().unwrap() + spread),
        "what the kernel timed of the programs is a part of what the program lost, never more: \
         {inside:?} against {lost:?}"
    );
}

/// The preload shim, on the bench's allocation workload: a million
/// `malloc`/`free` pairs, timed by the program itself. The shim times one of
/// its calls in seventeen, less the real allocator's time, and publishes the
/// account in the ring it shares with `omn`; the run reports it scaled up.
///
/// Two independent checks. The count: the program made two million heap
/// calls, and the account's call count is that within its sampling -- the
/// few dozen more are the C library's own at startup. And the time: what the
/// shim says it spent is a part of what the program lost against three bare
/// runs, bounded above by the loss plus the spread of the runs -- the
/// jump into the hook and the ring's effect on the caches are not in it -- and
/// is most of it: in a release build it comes within the spread, and a debug
/// build's hooks, which are unoptimised function calls before the clock
/// starts, still leave it above half.
#[test]
fn what_the_shim_spends_inside_the_target_is_most_of_what_the_program_lost() {
    if workload().is_none() {
        return;
    }
    const PAIRS: u64 = 1_000_000;
    let pairs = PAIRS.to_string();
    let args = ["async-malloc", "1", pairs.as_str()];
    let request: &[&str] = &["mem.heap.alloc", "-e", "summary"];
    let bare: Vec<u64> = (0..3)
        .map(|i| timed(&format!("malloc-bare-{i}"), None, &args).cpu_ns())
        .collect();
    let traced: Vec<Timed> = (0..3)
        .map(|i| timed(&format!("malloc-shim-{i}"), Some(request), &args))
        .collect();
    let Some(first) = traced[0].cost.as_ref() else {
        let said = stderr(traced[0].output.as_ref().unwrap());
        eprintln!("SKIP: `mem.heap.alloc` cannot be served here: {said}");
        return;
    };
    let from = first["overhead"]["in_target_from"]
        .as_str()
        .unwrap_or_default();
    let inside: Vec<u64> = traced
        .iter()
        .map(|run| {
            run.cost.as_ref().expect("every run records its cost")["overhead"]["in_target_cpu_ns"]
                .as_u64()
                .unwrap_or_else(|| panic!("the shim's account is read from its ring: {from}"))
        })
        .collect();
    for run in &traced {
        let calls = run.cost.as_ref().unwrap()["overhead"]["in_target_runs"]
            .as_u64()
            .expect("the shim counts its calls");
        assert!(
            calls.abs_diff(2 * PAIRS) < 2 * PAIRS / 100,
            "the program made {} heap calls, and the shim's account holds {calls}",
            2 * PAIRS
        );
    }
    let floor = median(bare.clone());
    let lost: Vec<u64> = traced
        .iter()
        .map(|run| run.cpu_ns().saturating_sub(floor))
        .collect();
    let spread = bare.iter().max().unwrap() - bare.iter().min().unwrap()
        + lost.iter().max().unwrap()
        - lost.iter().min().unwrap();
    eprintln!(
        "bare cpu {bare:?} ns; lost under the shim {lost:?} ns; the shim's account {inside:?} \
         ns; spread {spread} ns"
    );
    let most = *lost.iter().max().unwrap();
    let least = *lost.iter().min().unwrap();
    assert!(
        inside.iter().all(|ns| *ns <= most + spread),
        "the shim's account cannot exceed what the program lost: {inside:?} against {lost:?}"
    );
    assert!(
        inside.iter().all(|ns| *ns * 2 >= least),
        "and it is most of it: {inside:?} against {lost:?}"
    );
    // The percentage weighs it: the shim's time against what the program did
    // of its own is far over what the reader's cpu alone would say.
    let measured = first["measured_percent"].as_f64().expect("a percentage");
    let own = first["own_percent"].as_f64().expect("the tool's own share");
    assert!(
        measured > own,
        "what the shim spent inside the program is in the percentage: {measured} against {own}"
    );
}

/// The CI's defect, made on purpose: a thread that is preempted inside a timed
/// hook call. The workload is pinned to one cpu beside a spinning task pinned
/// to the same one, so the scheduler takes the cpu from it every slice, and
/// some of those preemptions land between a timed call's two clock reads.
/// Before the fix each one added a whole slice, scaled by seventeen, to the
/// shim's account: on a two-cpu runner, 610 ms for 396 ms the program lost.
///
/// Ground truth is the program's own cpu time, which a preemption does not
/// add to: what the shim says it spent can be no more than what the program
/// lost against three bare runs under the same neighbour, plus their spread.
/// And the calls a preemption fell in are counted, not dropped unsaid: with
/// the kernel's own count of the program's involuntary switches in the
/// hundreds, which is what shows the case was created here, some runs have
/// to report some (`in_target_left_out`). Fewer preemptions skip, with the
/// numbers.
#[test]
fn a_preempted_hook_call_is_counted_apart_and_adds_no_time_slice() {
    if workload().is_none() {
        return;
    }
    let allowed = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            let line = status
                .lines()
                .find(|line| line.starts_with("Cpus_allowed_list:"))?
                .to_owned();
            let last = line
                .rsplit([',', '-', ':', '\t', ' '])
                .next()?
                .trim()
                .to_owned();
            last.parse::<u32>().ok()
        });
    let (Some(cpu), true) = (
        allowed,
        Command::new("taskset")
            .arg("-V")
            .output()
            .is_ok_and(|output| output.status.success()),
    ) else {
        eprintln!("SKIP: no `taskset`, or no cpu list to pin to: {allowed:?}");
        return;
    };
    let cpu = cpu.to_string();
    let pin: &[&str] = &["taskset", "-c", &cpu];
    let mut neighbour = Command::new("taskset")
        .args(["-c", &cpu, "/bin/sh", "-c", "while :; do :; done"])
        .spawn()
        .expect("a spinning neighbour");
    const PAIRS: u64 = 1_000_000;
    let pairs = PAIRS.to_string();
    let args = ["async-malloc", "1", pairs.as_str()];
    let request: &[&str] = &["mem.heap.alloc", "-e", "summary"];
    let bare: Vec<u64> = (0..3)
        .map(|i| timed_under(&format!("preempted-bare-{i}"), None, pin, &args).cpu_ns())
        .collect();
    let traced: Vec<Timed> = (0..3)
        .map(|i| timed_under(&format!("preempted-shim-{i}"), Some(request), pin, &args))
        .collect();
    let _ = neighbour.kill();
    let _ = neighbour.wait();
    let Some(first) = traced[0].cost.as_ref() else {
        let said = stderr(traced[0].output.as_ref().unwrap());
        eprintln!("SKIP: `mem.heap.alloc` cannot be served here: {said}");
        return;
    };
    let from = first["overhead"]["in_target_from"]
        .as_str()
        .unwrap_or_default();
    let overhead = |run: &Timed| -> serde_json::Value {
        run.cost.as_ref().expect("every run records its cost")["overhead"].clone()
    };
    let inside: Vec<u64> = traced
        .iter()
        .map(|run| {
            overhead(run)["in_target_cpu_ns"]
                .as_u64()
                .unwrap_or_else(|| panic!("the shim's account is read from its ring: {from}"))
        })
        .collect();
    let left_out: Vec<u64> = traced
        .iter()
        .map(|run| overhead(run)["in_target_left_out"].as_u64().unwrap_or(0))
        .collect();
    let calls: Vec<u64> = traced
        .iter()
        .map(|run| overhead(run)["in_target_runs"].as_u64().unwrap_or(0))
        .collect();
    let floor = median(bare.clone());
    let lost: Vec<u64> = traced
        .iter()
        .map(|run| run.cpu_ns().saturating_sub(floor))
        .collect();
    let spread = bare.iter().max().unwrap() - bare.iter().min().unwrap()
        + lost.iter().max().unwrap()
        - lost.iter().min().unwrap();
    eprintln!(
        "pinned to cpu {cpu} beside a spinning task: bare cpu {bare:?} ns; lost under the shim \
         {lost:?} ns; the shim's account {inside:?} ns over {calls:?} calls, {left_out:?} of \
         them priced because a preemption fell in them; spread {spread} ns"
    );
    // The kernel's own count of the preemptions, which the case needs: a
    // timed span is about one call in seventeen of the hook's time, so a few
    // hundred preemptions put some inside one.
    let preempted: Vec<u64> = traced
        .iter()
        .map(|run| run.report["involuntary"].as_u64().unwrap_or(0))
        .collect();
    eprintln!("the kernel preempted the traced runs {preempted:?} times");
    if preempted.iter().sum::<u64>() < 300 {
        eprintln!(
            "SKIP: the kernel preempted the pinned program {preempted:?} times in three runs, \
             too few to put one inside a timed hook call (numbers above)"
        );
        return;
    }
    let most = *lost.iter().max().unwrap();
    assert!(
        inside.iter().all(|ns| *ns <= most + spread),
        "a preemption inside a timed call adds nothing of the scheduler's to the shim's \
         account: {inside:?} against {lost:?} lost, spread {spread}"
    );
    for (count, left) in calls.iter().zip(&left_out) {
        assert!(
            left <= count,
            "the calls priced at the mean are some of the calls: {left} of {count}"
        );
    }
    assert!(
        left_out.iter().any(|left| *left > 0),
        "{preempted:?} preemptions and no timed call said to have had one: the calls left \
         out of the account are counted, never dropped unsaid"
    );
}

/// A collector that reads from outside its target spends nothing inside it,
/// and says so as a measured zero; one whose mechanism runs inside and keeps no
/// account says unknown. `mem.rss` reads `/proc`; `cpu.cycles` is a perf
/// counter the kernel switches on every context switch of the target.
#[test]
fn a_reader_from_outside_costs_the_target_nothing_and_a_counter_inside_it_is_unknown() {
    let (_, outside) = measured("outside", &["-m", "poll@100ms"]);
    assert_eq!(
        outside["overhead"]["in_target_cpu_ns"],
        serde_json::json!(0),
        "{outside}"
    );
    assert!(outside["overhead"]["in_target_from"]
        .as_str()
        .is_some_and(|from| from.contains("nothing of this collector runs")));

    let directory = scratch("counter");
    let out = directory.to_string_lossy().into_owned();
    let output = run(&[
        "cpu.cycles",
        "-m",
        "count",
        "-e",
        "summary",
        "-d",
        "300ms",
        "-o",
        &out,
        "--",
        "/bin/sh",
        "-c",
        BURN,
    ]);
    if !output.status.success() {
        eprintln!(
            "SKIP (the counter half): `cpu.cycles` is not served here: {}",
            stderr(&output)
        );
        return;
    }
    let cost = cost_record(&directory);
    let _ = std::fs::remove_dir_all(&directory);
    assert!(cost["overhead"]["in_target_cpu_ns"].is_null(), "{cost}");
    assert!(
        cost["overhead"]["in_target_from"]
            .as_str()
            .is_some_and(|from| from.contains("context switch")),
        "{cost}"
    );
}

/// The lock storm of finding 47, bounded: four threads, a million round trips
/// each on one mutex, under `sched.mutex` through the preload shim. The shim
/// reports every acquisition, far faster than `omn` reads, and before the
/// bound `omn` held 3.9 GiB of what it had read.
///
/// Ground truth is the workload's own count -- it increments a counter under
/// the lock, four million times, and reports it -- against which every
/// acquisition the run saw, plus every one it dropped and counted, must come
/// to exactly that; and the high-water mark of `omn`'s own memory, read from
/// `/proc/<pid>/status` while it runs, stays under what holding every record
/// would take.
#[test]
fn a_lock_storm_is_read_bounded_and_every_acquisition_is_accounted_for() {
    let Some(program) = workload() else {
        return;
    };
    const THREADS: u64 = 4;
    const ROUNDS: u64 = 1_000_000;
    let directory = scratch("storm");
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    let report = directory.join("report.json");
    let out = directory.join("out");
    let mut child = Command::new(BINARY)
        .args([
            "sched.mutex",
            "-m",
            "trace",
            "-e",
            "summary",
            "--format",
            "json",
            "-o",
        ])
        .arg(&out)
        .arg("--")
        .arg(&program)
        .args(["contended", &THREADS.to_string(), &ROUNDS.to_string()])
        .env("BENCH_REPORT", &report)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("omn runs");
    // The high-water mark, read until the process is gone: the last reading
    // before it exits is its peak, whenever that was.
    let status = format!("/proc/{}/status", child.id());
    let mut peak_kb = 0u64;
    while child.try_wait().expect("waiting").is_none() {
        if let Ok(text) = std::fs::read_to_string(&status) {
            if let Some(kb) = text
                .lines()
                .find_map(|line| line.strip_prefix("VmHWM:"))
                .and_then(|value| value.trim().trim_end_matches("kB").trim().parse().ok())
            {
                peak_kb = kb;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let output = child.wait_with_output().expect("omn ends");
    let said = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success() {
        let _ = std::fs::remove_dir_all(&directory);
        assert!(
            said.contains("preload") || said.contains("shim"),
            "a storm this machine cannot serve says why: {said}"
        );
        eprintln!("SKIP: `sched.mutex` cannot be served here: {said}");
        return;
    }
    let workload: serde_json::Value = serde_json::from_str(
        std::fs::read_to_string(&report)
            .expect("the workload's report")
            .trim(),
    )
    .expect("json");
    let results: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(out.join("results.json")).expect("results.json"),
    )
    .expect("json");
    let _ = std::fs::remove_dir_all(&directory);
    let result = &results["results"][0];
    let counted = workload["counter"]
        .as_u64()
        .expect("the workload's counter");
    assert_eq!(
        counted,
        THREADS * ROUNDS,
        "the workload did what it was asked"
    );
    let seen = result["events_seen"].as_u64().expect("events");
    let dropped = result["losses"]["dropped"].as_u64().expect("dropped");
    eprintln!(
        "storm: {counted} acquisitions, {seen} seen, {dropped} dropped and counted, omn's peak \
         {} MiB",
        peak_kb / 1024
    );
    assert_eq!(
        seen + dropped,
        counted,
        "every acquisition the program made is either in the run or counted as lost: {}",
        result["losses"]
    );
    // What the run holds is bounded, whatever the program does: the bound is
    // on records, and what `omn` needs besides them is tens of megabytes.
    const HELD_RECORDS: u64 = 1 << 19;
    assert!(
        seen <= HELD_RECORDS,
        "the run kept {seen} events, past the {HELD_RECORDS} it holds"
    );
    if seen == HELD_RECORDS {
        assert!(
            result["losses"]["reasons"]
                .as_object()
                .is_some_and(|reasons| reasons
                    .keys()
                    .any(|why| why.contains("than this run holds"))),
            "a run that reached its bound says so: {}",
            result["losses"]
        );
    }
    // Before the bound, 3.9 GiB for 1.9 million events. Holding the four
    // million this program made would be about eight.
    assert!(
        peak_kb < 3 * 1024 * 1024,
        "omn's own memory peaked at {} MiB, which is not a bounded reader",
        peak_kb / 1024
    );
}

/// Ground truth for what the run cost the machine: the kernel's account of the
/// tool, read by `wait4` from outside it, less what the target says of itself.
///
/// A profile with stacks reads the kernel's symbol table before its window
/// opens, so that no drain pays for it, and a debug build spends the better
/// part of a second parsing it. `cost.json` weighed only the window and said
/// 26 ms where the kernel counted 986 ms of `omn` beyond its target (stream
/// U's finding, fourteenth wave). The record now carries the process's cpu
/// before and after the window, and the three add up to the kernel's account,
/// less what formatting the results costs once the record was taken -- held
/// here to within a fifth of it, and never above it.
#[test]
fn what_a_profile_with_stacks_says_it_cost_adds_up_to_what_the_kernel_counted() {
    // The target burns half a second and says, at its exit, the cpu it
    // spent over its whole life -- the same kernel account `wait4` sums.
    const TARGET: &str = "import resource, time\n\
         t = time.process_time()\n\
         while time.process_time() - t < 0.5: pass\n\
         u = resource.getrusage(resource.RUSAGE_SELF)\n\
         import sys; sys.stderr.write('target_cpu_ns=%d\\n' % int((u.ru_utime + u.ru_stime) * 1e9))\n";
    if Command::new("python3").arg("--version").output().is_err() {
        eprintln!("SKIP: this machine has no python3 to launch");
        return;
    }
    let directory = scratch("profile-stacks");
    let out = directory.to_string_lossy().into_owned();
    // Reaped with `wait4` below, for the kernel's account of what it used.
    #[allow(clippy::zombie_processes)]
    let mut child = Command::new(BINARY)
        .args(["cpu.profile", "--mode", "sample@999hz", "--emit", "stack"])
        .args(["--format", "json", "-o", out.as_str()])
        .args(["--", "python3", "-c", TARGET])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("omn runs");
    let pid = child.id() as i32;
    let mut said = String::new();
    std::io::Read::read_to_string(
        &mut child.stderr.take().expect("stderr is piped"),
        &mut said,
    )
    .expect("reading omn's stderr");
    let mut status = 0;
    // SAFETY: `wait4` fills a struct this test owns.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    assert!(unsafe { libc::wait4(pid, &mut status, 0, &mut usage) } == pid);
    if said.contains("has no viable backend on this machine") {
        eprintln!("SKIP: this machine does not serve cpu.profile: {said}");
        return;
    }
    assert!(
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        "{said}"
    );
    let micros = |time: libc::timeval| time.tv_sec as u64 * 1_000_000 + time.tv_usec as u64;
    let tree_ns = (micros(usage.ru_utime) + micros(usage.ru_stime)) * 1_000;
    let target_ns: u64 = said
        .lines()
        .find_map(|line| line.strip_prefix("target_cpu_ns="))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or_else(|| panic!("the target said what it spent: {said}"));
    let kernel_ns = tree_ns.saturating_sub(target_ns);

    let overhead = &cost_record(&directory)["overhead"];
    let _ = std::fs::remove_dir_all(&directory);
    let field = |name: &str| {
        overhead[name]
            .as_u64()
            .unwrap_or_else(|| panic!("cost.json carries `{name}`: {overhead}"))
    };
    let (before, during, after) = (
        field("before_window_cpu_ns"),
        field("collector_cpu_ns"),
        field("after_window_cpu_ns"),
    );
    let reported = before + during + after;
    let account = format!(
        "cost.json says {:.1} ms before the window, {:.1} ms in it and {:.1} ms after, {:.1} ms \
         in all; the kernel counts {:.1} ms of omn beyond the target's own {:.1} ms",
        before as f64 / 1e6,
        during as f64 / 1e6,
        after as f64 / 1e6,
        reported as f64 / 1e6,
        kernel_ns as f64 / 1e6,
        target_ns as f64 / 1e6,
    );
    eprintln!("{account}");
    // `wait4` counts in microseconds; the target's figure and the tool's are
    // two reads of the same kernel accounts, a clock tick apart at most.
    assert!(
        reported <= kernel_ns + 5_000_000,
        "more than the kernel counted: {account}"
    );
    assert!(
        reported * 5 >= kernel_ns * 4,
        "the record leaves out more than a fifth of what the run cost: {account}"
    );

    // And the one line every run prints says it, where it outweighs the
    // window: it used to show the window's 80 ms and nothing of the second
    // before it. The figure it names is the one held to the kernel above.
    let line = said
        .lines()
        .find(|line| line.starts_with("cost: cpu.profile"))
        .unwrap_or_else(|| panic!("a cost line: {said}"));
    if before > during {
        let named = line
            .split_once("; and ")
            .and_then(|(_, rest)| rest.split_once(" before this collection's window opened"))
            .map(|(figure, _)| figure)
            .unwrap_or_else(|| panic!("the line names the cpu before the window: {line}"));
        let ns = |text: &str| -> Option<f64> {
            [("ms", 1e6), ("us", 1e3), ("ns", 1.0), ("s", 1e9)]
                .iter()
                .find_map(|(unit, scale)| {
                    text.strip_suffix(unit)
                        .and_then(|number| number.parse::<f64>().ok())
                        .map(|number| number * scale)
                })
        };
        let named_ns = ns(named).unwrap_or_else(|| panic!("a duration: {named:?} in {line}"));
        assert!(
            (named_ns - before as f64).abs() <= before as f64 * 0.01 + 1e5,
            "the line says {named} before the window and cost.json {before} ns: {line}"
        );
        assert!(
            named_ns <= kernel_ns as f64 + 5e6,
            "the line names more than the kernel counted: {line}; {account}"
        );
    } else {
        assert!(
            !line.contains("before this collection's window opened"),
            "named although the window outweighs it: {line}"
        );
    }
}

/// A task that blocks every fifth of a millisecond until it is told to stop,
/// and logs, on `CLOCK_MONOTONIC`, the kernel's own count of every switch it
/// made -- voluntary and not -- before and after each sleep.
const SWITCHING_WORKLOAD: &str = r#"
import json, os, resource, signal, time
mono = lambda: time.clock_gettime_ns(time.CLOCK_MONOTONIC)
def switched():
    u = resource.getrusage(resource.RUSAGE_THREAD)
    return u.ru_nvcsw + u.ru_nivcsw
done = []
signal.signal(signal.SIGUSR1, lambda *_: done.append(1))
points = []
print("ready", os.getpid(), flush=True)
end = mono() + 60 * 10**9
while not done and mono() < end:
    points.append((mono(), switched())); time.sleep(0.0002); points.append((mono(), switched()))
print(json.dumps(points), flush=True)
"#;

/// One run of `sched.switches` over a task that switches at a known rate,
/// with `budget`, and the task's own log of what it did meanwhile.
struct Switched {
    output: Output,
    cost: serde_json::Value,
    results: serde_json::Value,
    /// `(CLOCK_MONOTONIC, switches so far)`, from the task.
    points: Vec<(u64, u64)>,
}

fn switched_under(budget: &str) -> Option<Switched> {
    use std::io::{BufRead, Read};

    if Command::new("python3").arg("-V").output().is_err() {
        eprintln!("SKIP: the workload is python3, which this machine does not have");
        return None;
    }
    let mut task = Command::new("python3")
        .args(["-c", SWITCHING_WORKLOAD])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("python3 starts");
    let mut stdout = std::io::BufReader::new(task.stdout.take().expect("a pipe"));
    let mut line = String::new();
    stdout
        .read_line(&mut line)
        .expect("the workload says it is ready");
    let pid: i32 = line
        .trim()
        .strip_prefix("ready ")
        .and_then(|pid| pid.parse().ok())
        .unwrap_or_else(|| panic!("the workload did not start: {line:?}"));

    let directory = scratch(&format!("switches-{budget}"));
    let out = directory.to_string_lossy().into_owned();
    let pid_text = pid.to_string();
    let output = run(&[
        "sched.switches",
        "-m",
        "count",
        "-e",
        "rate",
        "--format",
        "json",
        "--budget",
        budget,
        "-d",
        "3s",
        "-o",
        out.as_str(),
        "--pid",
        pid_text.as_str(),
    ]);
    // SAFETY: a signal to the child this test started and has not reaped.
    unsafe { libc::kill(pid, libc::SIGUSR1) };
    let mut rest = String::new();
    stdout.read_to_string(&mut rest).expect("the log");
    let _ = task.wait();

    if !output.status.success() {
        let _ = std::fs::remove_dir_all(&directory);
        let said = stderr(&output);
        if said.contains("has no viable backend on this machine") {
            eprintln!("SKIP: `sched.switches` is not served here: {said}");
            return None;
        }
        panic!("the run failed: {said}");
    }
    let cost = cost_record(&directory);
    let results: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(directory.join("results.json")).expect("results.json"),
    )
    .expect("results.json is json");
    let _ = std::fs::remove_dir_all(&directory);
    let points = serde_json::from_str(rest.trim())
        .unwrap_or_else(|error| panic!("the workload's log is not json ({error}): {rest}"));
    Some(Switched {
        output,
        cost,
        results,
        points,
    })
}

impl Switched {
    fn served(&self) -> Vec<String> {
        self.cost["served_by"]
            .as_array()
            .expect("served_by")
            .iter()
            .filter_map(|backend| backend.as_str().map(str::to_owned))
            .collect()
    }

    /// The task's switches from its last logged instant at or before `from`
    /// to its first at or after `to`, and from its first at or after `from`
    /// to its last at or before `to`: the most and the least the span held.
    fn bracket(&self, from: u64, to: u64) -> (u64, u64) {
        let at_or_before = |ts: u64| {
            self.points
                .iter()
                .rev()
                .find(|(at, _)| *at <= ts)
                .map(|point| point.1)
        };
        let at_or_after = |ts: u64| self.points.iter().find(|(at, _)| *at >= ts).map(|p| p.1);
        match (
            at_or_after(from),
            at_or_before(to),
            at_or_before(from),
            at_or_after(to),
        ) {
            (Some(first_in), Some(last_in), Some(before), Some(after)) => (
                last_in.saturating_sub(first_in),
                after.saturating_sub(before),
            ),
            _ => panic!("the workload's log does not straddle {from}..{to}"),
        }
    }
}

/// Ground truth for the drain across a budget's move: every switch the task
/// made inside the window is counted, by one backend or the other, or
/// reported lost -- none dropped at the seam -- and a switch counted by both
/// is one inside the overlap the run announces, and nowhere else.
///
/// `sched.switches` counts through `pmu:sw` first and `bpf:raw_tp` below it.
/// A ceiling of 1 % is more than a counter read every tick keeps to against a
/// task that mostly sleeps, and `bpf:raw_tp` declares `low`, which fits it, so
/// the run moves once and then stops with nowhere cheaper to go. The
/// replacement is counting before the outgoing counter's last read -- a gap
/// would lose events with nothing to show it -- so the two overlap, and the
/// run says from when to when. The task's own `getrusage` count, logged on
/// the run's clock, brackets what the window held and what the overlap held.
/// The same run with a ceiling nothing reaches is the control: one backend,
/// inside the bracket with no allowance at all.
#[test]
fn a_budget_s_move_between_backends_drops_nothing_and_says_where_both_counted() {
    let Some(control) = switched_under("10000%") else {
        return;
    };
    let window = |run: &Switched| {
        let result = &run.results["results"][0];
        let opened = result["window"]["opened_ns"].as_u64().expect("a window");
        let closed = result["window"]["closed_ns"].as_u64().expect("a window");
        let counted: u64 = result["outputs"][0]["series"]
            .as_array()
            .expect("a rate")
            .iter()
            .map(|series| series["total"].as_f64().unwrap_or(0.0) as u64)
            .sum();
        let lost = result["losses"]["dropped"].as_u64().unwrap_or(0);
        (opened, closed, counted, lost)
    };
    let (opened, closed, counted, lost) = window(&control);
    let (least, most) = control.bracket(opened, closed);
    assert_eq!(control.served(), ["pmu:sw"], "{}", stderr(&control.output));
    assert!(
        counted + lost >= least && counted <= most,
        "one backend, no move: {counted} counted and {lost} lost, the task made {least} to \
         {most}"
    );

    let Some(moved) = switched_under("1%") else {
        return;
    };
    let served = moved.served();
    if served.len() < 2 {
        // The move needs `bpf:raw_tp`, which needs CAP_BPF; without it the run
        // stops on its first backend, which the tests above hold.
        eprintln!(
            "SKIP: the run never moved ({served:?}, {}); `make` grants the capabilities the \
             second backend needs",
            moved.cost["stopped_because"]
        );
        return;
    }
    assert_eq!(
        served,
        ["pmu:sw", "bpf:raw_tp"],
        "{}",
        stderr(&moved.output)
    );
    // The ladder chose `bpf:raw_tp`; the user pinned nothing.
    let notes = moved.results["results"][0]["degradations"].to_string();
    assert!(
        !notes.contains("was pinned"),
        "the ladder's step is not the user's pin: {notes}"
    );
    let (opened, closed, counted, lost) = window(&moved);
    let (least, most) = moved.bracket(opened, closed);
    let degradations = moved.results["results"][0]["degradations"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let seam = degradations
        .iter()
        .filter_map(|note| note.as_str())
        .find(|note| note.contains("both counted"))
        .unwrap_or_else(|| panic!("the overlap is announced: {degradations:?}"));
    // "... both counted from 430.120 ms to 430.556 ms into the run ..."
    let instants: Vec<f64> = seam
        .split_once("both counted from ")
        .map_or("", |(_, rest)| rest)
        .split_whitespace()
        .collect::<Vec<_>>()
        .windows(2)
        .filter(|pair| pair[1].trim_end_matches(',') == "ms")
        .filter_map(|pair| pair[0].parse().ok())
        .collect();
    let [from_ms, to_ms] = instants[..] else {
        panic!("the overlap names two instants: {seam}");
    };
    let run_opened = moved.results["results"][0]["window"]["run_opened_ns"]
        .as_u64()
        .expect("the run's opening");
    // The note rounds to a microsecond; widen by one either way.
    let from = run_opened + (from_ms * 1e6) as u64 - 1_000;
    let to = run_opened + (to_ms * 1e6) as u64 + 1_000;
    let (_, twice_at_most) = moved.bracket(from, to);
    let account = format!(
        "the tool counted {counted} switches across {served:?} and lost {lost}; the task's own \
         count puts {least} to {most} inside the window and at most {twice_at_most} in the \
         overlap: {seam}"
    );
    eprintln!("{account}");
    assert!(least > 200, "too few switches to judge: {account}");
    assert!(
        counted + lost >= least,
        "switches dropped at the seam: {account}"
    );
    assert!(
        counted <= most + twice_at_most,
        "switches counted twice outside the announced overlap: {account}"
    );
}

/// A task that takes page faults at a steady rate until it is told to stop,
/// and logs, on `CLOCK_MONOTONIC`, the kernel's own count of the faults it
/// took (`ru_minflt + ru_majflt`) before every round of 1024 fresh pages.
/// Huge pages are refused on the mapping, so a page is a fault.
const FAULTING_WORKLOAD: &str = r#"
import json, mmap, os, resource, signal, time
mono = lambda: time.clock_gettime_ns(time.CLOCK_MONOTONIC)
def faulted():
    u = resource.getrusage(resource.RUSAGE_SELF)
    return u.ru_minflt + u.ru_majflt
done = []
signal.signal(signal.SIGUSR1, lambda *_: done.append(1))
size = 1 << 22
m = mmap.mmap(-1, size)
m.madvise(mmap.MADV_NOHUGEPAGE)
points = []
print("ready", os.getpid(), flush=True)
end = mono() + 60 * 10**9
while not done and mono() < end:
    points.append((mono(), faulted()))
    for i in range(0, size, 4096):
        m[i] = 1
    m.madvise(mmap.MADV_DONTNEED)
points.append((mono(), faulted()))
print(json.dumps(points), flush=True)
"#;

/// Ground truth for phase 6's ladder across modes, as it stands: a traced
/// series over its ceiling is not moved onto sampling while the sampled
/// events would not say what they stand for, and every fault the window held
/// is an event of its own, the task's own count.
///
/// `mem.faults` traces every fault through `pmu:sw` and samples one in a
/// period through the same backend, the one entry of the catalog whose
/// backend serves both. Traced, the tool spends about as much cpu as a task
/// that does nothing but fault, so a 5 % ceiling is over at the first check.
/// The ladder's only rung below would be the same backend sampled, and its
/// sampled events carry no `weight` (filed for `page_faults.rs`): the rung is
/// passed over rather than taken and flagged after the fact, so the run stops
/// rather than degrades, and says the rung was passed over and why. `hist` is
/// asked because it is an emit both modes produce. The task's `getrusage` log
/// brackets what the window held.
///
/// Once `page_faults.rs` weighs its samples and its factory declares it
/// (`collect::Factory::weighing_its_samples`), the run moves, and the weighed
/// half below holds the whole series to the task's count across the move.
#[test]
fn a_traced_series_over_its_ceiling_counts_what_the_task_did_and_moves_only_onto_weighed_samples() {
    use std::io::{BufRead, Read};

    if Command::new("python3").arg("-V").output().is_err() {
        eprintln!("SKIP: the workload is python3, which this machine does not have");
        return;
    }
    let mut task = Command::new("python3")
        .args(["-c", FAULTING_WORKLOAD])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("python3 starts");
    let mut stdout = std::io::BufReader::new(task.stdout.take().expect("a pipe"));
    let mut line = String::new();
    stdout
        .read_line(&mut line)
        .expect("the workload says it is ready");
    let pid: i32 = line
        .trim()
        .strip_prefix("ready ")
        .and_then(|pid| pid.parse().ok())
        .unwrap_or_else(|| panic!("the workload did not start: {line:?}"));

    let directory = scratch("mode-ladder");
    let out = directory.to_string_lossy().into_owned();
    let pid_text = pid.to_string();
    let output = run(&[
        "mem.faults",
        "-m",
        "trace",
        "-e",
        "hist",
        "--format",
        "json",
        "--budget",
        "5%",
        "-d",
        "2s",
        "-o",
        out.as_str(),
        "--pid",
        pid_text.as_str(),
    ]);
    // SAFETY: a signal to the child this test started and has not reaped.
    unsafe { libc::kill(pid, libc::SIGUSR1) };
    let mut rest = String::new();
    stdout.read_to_string(&mut rest).expect("the log");
    let _ = task.wait();

    let said = stderr(&output);
    if !output.status.success() {
        let _ = std::fs::remove_dir_all(&directory);
        if said.contains("has no viable backend on this machine")
            || said.contains("cannot start collecting `mem.faults`")
        {
            eprintln!("SKIP: `mem.faults` is not served here: {said}");
            return;
        }
        panic!("the run failed: {said}");
    }
    let cost = cost_record(&directory);
    let results: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(directory.join("results.json")).expect("results.json"),
    )
    .expect("results.json is json");
    let _ = std::fs::remove_dir_all(&directory);
    let points: Vec<(u64, u64)> = serde_json::from_str(rest.trim())
        .unwrap_or_else(|error| panic!("the workload's log is not json ({error}): {rest}"));
    let bracket = |from: u64, to: u64| -> (u64, u64) {
        let at_or_before = |ts: u64| points.iter().rev().find(|(at, _)| *at <= ts).map(|p| p.1);
        let at_or_after = |ts: u64| points.iter().find(|(at, _)| *at >= ts).map(|p| p.1);
        match (
            at_or_after(from),
            at_or_before(to),
            at_or_before(from),
            at_or_after(to),
        ) {
            (Some(first_in), Some(last_in), Some(before), Some(after)) => (
                last_in.saturating_sub(first_in),
                after.saturating_sub(before),
            ),
            _ => panic!("the workload's log does not straddle {from}..{to}"),
        }
    };

    let result = &results["results"][0];
    let degradations: Vec<String> = result["degradations"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|note| note.as_str().map(str::to_owned))
        .collect();
    let mode = cost["mode"].as_str().unwrap_or_default().to_owned();
    let stopped = cost["stopped_because"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let counted: u64 = result["outputs"][0]["series"]
        .as_array()
        .expect("a histogram")
        .iter()
        .map(|series| series["count"].as_u64().unwrap_or(0))
        .sum();
    let lost = result["losses"]["dropped"].as_u64().unwrap_or(0);
    let opened = result["window"]["opened_ns"].as_u64().expect("a window");
    let closed = result["window"]["closed_ns"].as_u64().expect("a window");
    let (least, most) = bracket(opened, closed);

    if !mode.contains(" then ") {
        // Not moved: the series is traced end to end, every fault an event.
        let account = format!(
            "the series counts {counted} faults and lost {lost} records; the task took \
             {least} to {most} inside the window; mode {mode}, stopped because {stopped:?}; \
             {said}"
        );
        eprintln!("{account}");
        // The run stops at its first check, a tenth of a second in, and a
        // loaded machine fits only a few rounds of 1024 faults into it: two
        // whole rounds inside the window are enough for an exact count.
        assert!(least >= 2048, "too few faults to judge: {account}");
        assert!(
            counted + lost >= least,
            "faults the traced series saw were dropped: {account}"
        );
        assert!(
            counted <= most,
            "the traced series counts more faults than the task took: {account}"
        );
        assert!(
            !degradations
                .iter()
                .any(|note| note.contains("carries a `weight`")),
            "no move, so nothing to flag after the fact: {degradations:?}"
        );
        if stopped.contains("nothing cheaper") {
            // Over its ceiling: the budget's note said why the sampled rung
            // was not one.
            assert!(
                degradations.iter().any(|note| {
                    note.contains("the budget stopped the run")
                        && note.contains("`pmu:sw` in `sample@99hz`, is passed over")
                        && note.contains("carry no `weight`")
                }),
                "the passed-over rung is named beside the stop: {degradations:?}"
            );
        } else {
            eprintln!(
                "note: the run held its 5 % ceiling ({stopped}), so the ladder was not put to \
                 the test"
            );
        }
        return;
    }
    // Moved: only onto a collector that weighs its samples.
    assert_eq!(mode, "trace then sample@99hz", "{said}");
    assert!(
        degradations
            .iter()
            .any(|note| note.contains("from `trace` to `sample@99hz` on `pmu:sw`")),
        "the change of mode is announced: {degradations:?}"
    );
    assert!(
        !degradations.iter().any(|note| note.contains("was pinned")),
        "the ladder's step is not the user's pin: {degradations:?}"
    );
    assert!(
        !degradations
            .iter()
            .any(|note| note.contains("carries a `weight`")),
        "a rung is taken only when its sampled events are weighed: {degradations:?}"
    );
    let seam = degradations
        .iter()
        .find(|note| note.contains("both counted"))
        .unwrap_or_else(|| panic!("the overlap is announced: {degradations:?}"));
    let instants: Vec<f64> = seam
        .split_once("both counted from ")
        .map_or("", |(_, rest)| rest)
        .split_whitespace()
        .collect::<Vec<_>>()
        .windows(2)
        .filter(|pair| pair[1].trim_end_matches(',') == "ms")
        .filter_map(|pair| pair[0].parse().ok())
        .collect();
    let [from_ms, to_ms] = instants[..] else {
        panic!("the overlap names two instants: {seam}");
    };
    let run_opened = result["window"]["run_opened_ns"]
        .as_u64()
        .expect("the run's opening");
    let overlap_from = run_opened + (from_ms * 1e6) as u64 - 1_000;
    let overlap_to = run_opened + (to_ms * 1e6) as u64 + 1_000;
    let (traced_least, _) = bracket(opened, overlap_from);
    let (_, overlap_most) = bracket(overlap_from, overlap_to);
    let account = format!(
        "the series counts {counted} faults and lost {lost} records; the task took at least \
         {traced_least} before the overlap, {least} to {most} inside the window and at most \
         {overlap_most} in the overlap: {seam}"
    );
    eprintln!("{account}");
    assert!(least > 20_000, "too few faults to judge: {account}");
    // A counter per cpu the task ran on, each short by less than a period at
    // the end.
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get()) as u64;
    let slack = 99 * cpus;
    assert!(
        counted + lost * 99 + slack >= least,
        "the weighed series counts fewer faults than the task took: {account}"
    );
    assert!(
        counted <= most + overlap_most,
        "the weighed series counts faults twice outside the announced overlap: {account}"
    );
}

/// A budget refuses up front only a move it could take: `mem.faults`'s
/// sampled events carry their period as `weight`, so the `trace` -> `sample`
/// rung is offered, a `raw` list under a budget would mix two kinds of event
/// and is refused with the reason, and `hist` is told it will degrade onto
/// the weighed samples. (`main.rs`'s unit test
/// `a_sampled_rung_whose_events_are_not_weighed_is_passed_over_not_taken`
/// holds the other branch: an unweighed collector's rung is passed over.)
///
/// Held to the matrix the validator itself answers with: `-m sample -e raw`
/// is a valid request (exit 0 on the dump) and `-m sample -e summary` is not
/// (exit 2), so what the budget says is about the move and not about either
/// mode.
#[test]
fn a_budget_takes_no_mode_rung_whose_sampled_events_would_not_be_weighed() {
    // This process, which any machine that serves `mem.faults` at all lets
    // its own user observe.
    let own = std::process::id().to_string();
    let dump = |args: &[&str]| {
        let mut all = vec!["mem.faults", "--pid", own.as_str()];
        all.extend_from_slice(args);
        all.push("--dump-request");
        run(&all)
    };
    let raw = dump(&["-m", "trace", "-e", "raw", "--budget", "0.01%"]);
    let said = stderr(&raw);
    if said.contains("has no viable backend on this machine") {
        eprintln!("SKIP: `mem.faults` is not served here: {said}");
        return;
    }
    assert_eq!(raw.status.code(), Some(2), "{said}");
    assert!(
        said.contains("`raw` would hold the events of both modes as one kind"),
        "{said}"
    );
    // Each mode serves `raw` alone, without a budget.
    for mode in ["trace", "sample"] {
        let alone = dump(&["-m", mode, "-e", "raw"]);
        assert_eq!(alone.status.code(), Some(0), "{}", stderr(&alone));
    }
    // `summary` is not a sampled emit: the rung is passed over, and the note
    // says the run would stop rather than degrade.
    let sampled_summary = dump(&["-m", "sample", "-e", "summary"]);
    assert_eq!(
        sampled_summary.status.code(),
        Some(2),
        "{}",
        stderr(&sampled_summary)
    );
    let summary = dump(&["-m", "trace", "-e", "summary", "--budget", "0.01%"]);
    assert_eq!(summary.status.code(), Some(0), "{}", stderr(&summary));
    assert!(
        stderr(&summary).contains("stop rather than degrade"),
        "{}",
        stderr(&summary)
    );
    // `hist` weighs each event, and the sampled events carry what they stand
    // for: the rung is offered, and nothing is passed over.
    let hist = dump(&["-m", "trace", "-e", "hist", "--budget", "0.01%"]);
    let said = stderr(&hist);
    assert_eq!(hist.status.code(), Some(0), "{said}");
    assert!(
        said.contains("degrade onto `pmu:sw` in `sample@99hz`")
            && said.contains("every event then standing for its period")
            && !said.contains("is passed over"),
        "{said}"
    );
}

/// A task that sleeps a millisecond at a time until it is told to stop, and
/// logs, on `CLOCK_MONOTONIC`, its own process cpu clock before every sleep:
/// a few microseconds a round, far under the 10 ms `/proc/<pid>/stat` counts
/// in.
const SLEEPING_WORKLOAD: &str = r#"
import json, os, signal, time
mono = lambda: time.clock_gettime_ns(time.CLOCK_MONOTONIC)
done = []
signal.signal(signal.SIGUSR1, lambda *_: done.append(1))
points = []
print("ready", os.getpid(), flush=True)
end = mono() + 60 * 10**9
while not done and mono() < end:
    points.append((mono(), time.process_time_ns())); time.sleep(0.001)
points.append((mono(), time.process_time_ns()))
print(json.dumps(points), flush=True)
"#;

/// Ground truth for the budget's denominator: a target that burns under a
/// clock tick between two checks is weighed in the scheduler's nanoseconds,
/// so the check sees it and acts, and the figure it weighed against is the
/// task's own clock over the window.
///
/// `mem.rss` polled every millisecond costs the tool far more than a task
/// that only sleeps spends, so a 5 % ceiling is over at the first check that
/// can see the task at all. Read in ticks it never could: the task's cpu
/// moved by 0 ticks between checks, the percentage stayed "not knowable yet",
/// and a run like it (`sched.offcpu --budget 5%` over a sleeping loop) ended
/// 16 % over its ceiling without ever being moved. The task's own log
/// brackets what it spent inside the window, to the microsecond.
#[test]
fn a_target_that_burns_under_a_tick_per_check_is_weighed_and_the_budget_acts_on_it() {
    use std::io::{BufRead, Read};

    if Command::new("python3").arg("-V").output().is_err() {
        eprintln!("SKIP: the workload is python3, which this machine does not have");
        return;
    }
    let mut task = Command::new("python3")
        .args(["-c", SLEEPING_WORKLOAD])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("python3 starts");
    let mut stdout = std::io::BufReader::new(task.stdout.take().expect("a pipe"));
    let mut line = String::new();
    stdout
        .read_line(&mut line)
        .expect("the workload says it is ready");
    let pid: i32 = line
        .trim()
        .strip_prefix("ready ")
        .and_then(|pid| pid.parse().ok())
        .unwrap_or_else(|| panic!("the workload did not start: {line:?}"));

    let directory = scratch("sleeping");
    let out = directory.to_string_lossy().into_owned();
    let pid_text = pid.to_string();
    let output = run(&[
        "mem.rss",
        "-m",
        "poll@1ms",
        "-e",
        "summary",
        "--format",
        "json",
        "--budget",
        "5%",
        "-d",
        "3s",
        "-o",
        out.as_str(),
        "--pid",
        pid_text.as_str(),
    ]);
    // SAFETY: a signal to the child this test started and has not reaped.
    unsafe { libc::kill(pid, libc::SIGUSR1) };
    let mut rest = String::new();
    stdout.read_to_string(&mut rest).expect("the log");
    let _ = task.wait();
    assert!(output.status.success(), "{}", stderr(&output));
    let cost = cost_record(&directory);
    let results: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(directory.join("results.json")).expect("results.json"),
    )
    .expect("results.json is json");
    let _ = std::fs::remove_dir_all(&directory);
    let points: Vec<(u64, u64)> = serde_json::from_str(rest.trim())
        .unwrap_or_else(|error| panic!("the workload's log is not json ({error}): {rest}"));

    let window = &results["results"][0]["window"];
    let opened = window["opened_ns"].as_u64().expect("a window");
    let closed = window["closed_ns"].as_u64().expect("a window");
    let at_or_before = |ts: u64| points.iter().rev().find(|(at, _)| *at <= ts).map(|p| p.1);
    let at_or_after = |ts: u64| points.iter().find(|(at, _)| *at >= ts).map(|p| p.1);
    let (Some(first_in), Some(last_in), Some(before), Some(after)) = (
        at_or_after(opened),
        at_or_before(closed),
        at_or_before(opened),
        at_or_after(closed),
    ) else {
        panic!("the workload's log does not straddle the window");
    };
    let (least, most) = (
        last_in.saturating_sub(first_in),
        after.saturating_sub(before),
    );
    let observed = cost["overhead"]["observed_cpu_ns"]
        .as_u64()
        .unwrap_or_else(|| panic!("the target's cpu is weighed: {cost}"));
    let served = cost["served_by"].as_array().map_or(0, Vec::len);
    let stopped = cost["stopped_because"].as_str().unwrap_or_default();
    let account = format!(
        "the run weighed the task at {observed} ns and the task's clock puts {least} to {most} ns \
         inside the window, which lasted {} ms; served by {served} backend(s), stopped because \
         {stopped}; read from {}",
        (closed - opened) / 1_000_000,
        cost["overhead"]["observed_from"]
    );
    eprintln!("{account}");
    assert!(
        observed + 100_000 >= least && observed <= most + 100_000,
        "the denominator is not the task's own cpu: {account}"
    );
    assert!(
        served > 1 || stopped.contains("ceiling"),
        "the check saw a run over its ceiling and did nothing: {account}"
    );
}

/// A session of requests this machine cannot serve is refused with every one
/// of them named, not the first: `examples/sched-forced-backends.yml` pins
/// five sched sources to their eBPF backends, and the resolution loop used to
/// return at the first it could not resolve (`docs/scenarios/sched.md`
/// defect 8). On a machine that serves them the run is a run, and there is
/// nothing here to check.
#[test]
fn a_session_refused_at_resolution_names_every_request_it_could_not_resolve() {
    let session = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/sched-forced-backends.yml");
    let output = run(&[
        "trace",
        "-f",
        session.to_str().expect("a utf-8 path"),
        "-d",
        "100ms",
    ]);
    let said = stderr(&output);
    if output.status.success() || !said.contains("has no viable backend") {
        eprintln!("SKIP: this machine resolves the pinned sched backends: {said}");
        return;
    }
    let refused: Vec<&str> = said
        .lines()
        .filter(|line| line.starts_with("error: `") && line.contains("has no viable backend"))
        .collect();
    for source in [
        "sched.switches",
        "sched.wakeup",
        "sched.migrations",
        "sched.offcpu",
    ] {
        assert!(
            refused
                .iter()
                .any(|line| line.contains(&format!("`{source}`"))),
            "`{source}` is named among the refused: {said}"
        );
    }
    assert!(
        said.contains(&format!(
            "{} of the session's 5 requests cannot be resolved here",
            refused.len()
        )),
        "and the count is said: {said}"
    );
    assert_eq!(output.status.code(), Some(1), "the machine's gap: {said}");
}

/// A stop ends what the program started even when it left the program's
/// tree first: a grandchild whose parent exited, in a session of its own
/// (`setsid`), was reparented away from the program and outlived the run.
/// `omn` adopts such orphans now (`PR_SET_CHILD_SUBREAPER`), and its stop
/// ends them and says how many.
#[test]
fn a_stop_ends_an_orphan_that_left_the_program_s_tree_and_group() {
    // A sleep no other test starts, so finding it afterwards finds this one.
    let marker = format!(
        "{}.{}",
        40 + std::process::id() % 50,
        std::process::id() % 997
    );
    let script = format!("(setsid sleep {marker} &) ; sleep 30");
    let output = run(&[
        "trace",
        "proc.fd",
        "-m",
        "poll@50ms",
        "-e",
        "summary",
        "-d",
        "600ms",
        "--",
        "sh",
        "-c",
        &script,
    ]);
    let said = stderr(&output);
    assert!(output.status.success(), "{said}");
    let survivors = Command::new("pgrep")
        .args(["-f", &format!("^sleep {marker}$")])
        .output()
        .map(|found| String::from_utf8_lossy(&found.stdout).trim().to_owned());
    if let Ok(pids) = &survivors {
        for pid in pids.lines() {
            let _ = Command::new("kill").args(["-9", pid]).status();
        }
        assert!(
            pids.is_empty(),
            "the orphan outlived the run: {pids}; {said}"
        );
    }
    assert!(
        said.contains(
            "the stop also ended 1 process(es) the program started that had left its tree"
        ),
        "and the run says it ended it: {said}"
    );
}

/// The same orphan when the program exits on its own, before the duration:
/// the run ends it too. The double-forked daemon is this case -- the program
/// returns at once and its daemon runs on -- and the stop alone did not reach
/// it, so it outlived the run, and a caller reading `omn`'s stderr through a
/// pipe waited on it for as long as it lived.
#[test]
fn a_program_that_exits_on_its_own_does_not_leave_its_orphan_running() {
    let marker = format!(
        "{}.{}",
        100 + std::process::id() % 50,
        std::process::id() % 997
    );
    let script = format!("(setsid sleep {marker} &) ; (setsid sleep 0.05 &) ; sleep 0.3");
    let output = run(&[
        "trace",
        "proc.fd",
        "-m",
        "poll@50ms",
        "-e",
        "summary",
        "-d",
        "5s",
        "--",
        "sh",
        "-c",
        &script,
    ]);
    let said = stderr(&output);
    assert!(output.status.success(), "{said}");
    let survivors = Command::new("pgrep")
        .args(["-f", &format!("^sleep {marker}$")])
        .output()
        .map(|found| String::from_utf8_lossy(&found.stdout).trim().to_owned());
    if let Ok(pids) = &survivors {
        for pid in pids.lines() {
            let _ = Command::new("kill").args(["-9", pid]).status();
        }
        assert!(
            pids.is_empty(),
            "the orphan outlived the run: {pids}; {said}"
        );
    }
    // One ended: the orphan that had exited during the run is reaped, and a
    // zombie is not counted as a process the run ended.
    assert!(
        said.contains(
            "the program exited, and the end of the run then ended 1 process(es) the program \
             started that had left its tree"
        ),
        "and the run says it ended it, and only it: {said}"
    );
}
