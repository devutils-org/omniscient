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

/// The cost the tool reports moves with the work it was asked to do.
///
/// This is the injected ground truth: the two runs differ in one thing, the
/// poll interval, and one of them reads `/proc` a hundred times as often as the
/// other over the same window against the same program. A tool reporting a
/// plausible constant would pass every other check in this file and fail this
/// one.
#[test]
fn what_the_tool_says_it_spent_moves_with_how_often_it_was_asked_to_read() {
    let (_, often) = measured("often", &["-m", "poll@1ms"]);
    let (_, rarely) = measured("rarely", &["-m", "poll@100ms"]);

    let often_ns = often["overhead"]["collector_cpu_ns"].as_u64().unwrap();
    let rarely_ns = rarely["overhead"]["collector_cpu_ns"].as_u64().unwrap();
    assert!(
        often_ns > rarely_ns * 5,
        "polling a hundred times as often cost {often_ns}ns against {rarely_ns}ns, which is not \
         the difference between a hundred reads and ten thousand"
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
#[test]
fn the_extra_cpu_a_faster_poll_costs_is_the_extra_cpu_the_shell_observes() {
    let cpu_ns = |interval: &str, directory: &Path| {
        let out = directory.to_string_lossy().into_owned();
        let script = format!(
            "\"{BINARY}\" mem.rss -e summary -m poll@{interval} -d 500ms -o \"{out}\" \
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
        let (_, cost) = measured(
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
        cost["stopped_because"]
            .as_str()
            .expect("a reason")
            .to_owned()
    };

    let over = stopped("0.05%");
    assert!(
        over.contains("ceiling of 0.05") && over.contains("nothing cheaper"),
        "a run the budget ended says which ceiling and that there was nowhere to go: {over}"
    );

    let under = stopped("10000%");
    assert!(
        under.contains("duration") || under.contains("exited"),
        "a run inside its ceiling ends for the ordinary reason: {under}"
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
        "trace",
        "-e",
        "hist",
        "--budget",
        "0.1%",
        "--dump-request",
    ]));
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
