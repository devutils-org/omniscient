//! Each view, drawn on its own workload's recording, shows what that
//! workload's manifest says -- within each figure's stated trust.
//!
//! `view/workloads/<view>/manifest.json` says what each program does, and
//! `tests/view_workloads.rs` holds the sources to it before any view draws
//! them. This file holds the views: the recording `workload-<view>` under
//! `tests/view_fixtures/` (made by `record.sh`, through `omn top --headless`
//! with every view's needs) is drawn, and the numbers are read off the screen
//! the way a user reads them -- the hottest function, the request's cause, the
//! bytes per socket and per file, the phase boundaries. A view that draws a figure the
//! manifest knows and draws it wrong fails here, not in a snapshot nobody
//! reads.
//!
//! Every bound comes from the manifest's `trust` and is a separation, not a
//! threshold tuned on a quiet machine: the recordings were made with nine
//! agents sharing it.

use omn_view::app::{App, Cost};
use omn_view::ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use omn_view::snapshot::{draw_app, fixture, text};
use serde_json::Value;

const MIB: f64 = 1024.0 * 1024.0;

fn manifest(view: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("view/workloads")
        .join(view)
        .join("manifest.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("the manifest")).expect("JSON")
}

fn num(value: &Value) -> f64 {
    value.as_f64().expect("a number in the manifest")
}

/// What `view` draws on its workload's recording at `width` x `height`, after
/// the keys in `keys` (a search, a selection), as text.
fn screen(view: &str, keys: &[KeyCode], width: u16, height: u16) -> String {
    let mut app = App::new(fixture(&format!("workload-{view}")));
    app.cost = Cost::Snapshot;
    app.open(view).unwrap_or_else(|error| panic!("{error}"));
    draw_app(&mut app, width, height);
    for &key in keys {
        app.key(KeyEvent::new(key, KeyModifiers::NONE));
    }
    text(&draw_app(&mut app, width, height))
}

/// The keys that type `text`, then Enter.
fn typed(text: &str) -> Vec<KeyCode> {
    let mut keys: Vec<KeyCode> = text.chars().map(KeyCode::Char).collect();
    keys.push(KeyCode::Enter);
    keys
}

fn trimmed(word: &str) -> &str {
    word.trim_matches(|c: char| c == ',' || c == '(' || c == ')' || c == '~')
}

/// A duration as the views print it (`902ms`, `2.61s`, `431µs`, `0ns`), in
/// seconds.
fn seconds(word: &str) -> Option<f64> {
    let word = trimmed(word);
    [("ns", 1e-9), ("µs", 1e-6), ("ms", 1e-3), ("s", 1.0)]
        .into_iter()
        .find_map(|(unit, scale)| {
            let number = word.strip_suffix(unit)?;
            number.parse::<f64>().ok().map(|n| n * scale)
        })
}

/// A byte count as the views print it (`4.00MiB`, `256KiB`, `12B`), in bytes.
fn bytes(word: &str) -> Option<f64> {
    let word = trimmed(word);
    [
        ("GiB", MIB * 1024.0),
        ("MiB", MIB),
        ("KiB", 1024.0),
        ("B", 1.0),
    ]
    .into_iter()
    .find_map(|(unit, scale)| {
        let number = word.strip_suffix(unit)?;
        number.parse::<f64>().ok().map(|n| n * scale)
    })
}

fn percent(word: &str) -> Option<f64> {
    trimmed(word).strip_suffix('%')?.parse().ok()
}

fn count(word: &str) -> Option<f64> {
    trimmed(word).parse().ok()
}

/// The words of a drawing, its frame's borders taken for spaces.
fn words(text: &str) -> Vec<&str> {
    text.split(|c: char| c.is_whitespace() || c == '│')
        .filter(|word| !word.is_empty())
        .collect()
}

/// The words of the first line containing `needle`, from `needle` on.
fn after<'a>(screen: &'a str, needle: &str) -> Vec<&'a str> {
    let line = screen
        .lines()
        .find(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("no line with `{needle}` in:\n{screen}"));
    let at = line.find(needle).expect("found");
    words(&line[at + needle.len()..])
}

/// The first word after `needle` that reads as `parse` does.
fn first<T>(screen: &str, needle: &str, parse: impl Fn(&str) -> Option<T>) -> T {
    after(screen, needle)
        .into_iter()
        .find_map(parse)
        .unwrap_or_else(|| panic!("nothing to read after `{needle}` in:\n{screen}"))
}

/// The first word of the line containing `needle`, wherever it is, that
/// reads as `parse` does.
fn in_line<T>(screen: &str, needle: &str, parse: impl Fn(&str) -> Option<T>) -> T {
    let line = screen
        .lines()
        .find(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("no line with `{needle}` in:\n{screen}"));
    words(line)
        .into_iter()
        .find_map(parse)
        .unwrap_or_else(|| panic!("nothing to read in `{line}`"))
}

fn within(what: &str, measured: f64, low: f64, high: f64) {
    assert!(
        (low..=high).contains(&measured),
        "{what}: {measured} drawn, the manifest's trust allows {low} to {high}"
    );
}

/// `flame`: the functions of known weight, found by the view's own search.
/// `heavy` 0.80 s and `light` 0.20 s on one thread, `medium` 0.40 s on
/// another: a profile sees each within its sampling error, so the ratios are
/// held, not the shares (the interpreter's start-up is on the cpu too). `nap`
/// sleeps 0.60 s, at least.
#[test]
fn flame_draws_each_functions_known_weight() {
    let truth = manifest("flame");
    let threads = truth["threads"].as_array().expect("threads");
    let on = |name: &str| {
        num(threads
            .iter()
            .find_map(|thread| thread["on_cpu"].get(name))
            .expect("a function the manifest names"))
    };
    let share = |name: &str, side: &str| -> f64 {
        let drawn = screen("flame", &typed(&format!("/{name}")), 200, 12);
        first(&drawn, &format!("{side}:"), percent)
    };
    let (heavy, light, medium) = (
        share("heavy", "on cpu"),
        share("light", "on cpu"),
        share("medium", "on cpu"),
    );
    // About a thousand samples: ±10 % on the light one's ~200 is the
    // sampling error, held at twice that.
    let ratio = on("heavy") / on("light");
    within("heavy / light", heavy / light, 0.8 * ratio, 1.2 * ratio);
    let ratio = on("heavy") / on("medium");
    within("heavy / medium", heavy / medium, 0.8 * ratio, 1.2 * ratio);
    assert!(
        heavy > medium && medium > light,
        "the order: {heavy} {medium} {light}"
    );

    let nap_share = share("nap", "off cpu");
    let plain = screen("flame", &[], 200, 12);
    let off_total = first(&plain, "off cpu:", seconds);
    let nap = nap_share / 100.0 * off_total;
    let asked = num(&threads[1]["off_cpu"]["nap"]);
    // At least what was asked, longer by the scheduler's slack; the share's
    // rounding (0.05 % of the total) under it.
    within("nap, off cpu", nap, asked - 0.001 * off_total, asked * 1.15);
}

/// `timebar`: the worker's bar, selected, splits its wall clock as the
/// manifest's phases do -- 0.60 s computing plus 0.30 s contended on the cpu,
/// 0.40 s asleep and 0.40 s on a lock (both `sleeping` to the scheduler, the
/// lock's part in futex(2) and drawn apart from the nap), and at least half the contended 0.30 s on the
/// run queue.
#[test]
fn timebar_splits_the_workers_wall_clock_as_its_phases() {
    let truth = manifest("timebar");
    let phase = |state: &str| {
        num(&truth["phases"]
            .as_array()
            .expect("phases")
            .iter()
            .find(|phase| phase["state"] == state)
            .expect("a phase")["seconds"])
    };
    let drawn = screen("timebar", &[KeyCode::Down], 160, 50);
    assert!(
        drawn
            .lines()
            .any(|line| line.contains('▸') && line.contains("worker")),
        "the worker's bar is the one selected:\n{drawn}"
    );
    let on_cpu = first(&drawn, "█ on-cpu", seconds);
    let truth_on = phase("on_cpu") + phase("runqueue");
    within("worker on cpu", on_cpu, truth_on * 0.97, truth_on * 1.1);
    // The futex waits come out of the sleep (`sched.futex`): what is left
    // is the nap, and the waits are a lock of their own.
    let asleep = first(&drawn, "░ blocked: sleeping ", seconds);
    within(
        "worker asleep",
        asleep,
        phase("sleeping") * 0.97,
        phase("sleeping") * 1.3,
    );
    let lock = first(&drawn, "blocked: sleeping, on a futex", seconds);
    within(
        "worker on a lock",
        lock,
        phase("futex") * 0.97,
        phase("futex") * 1.3,
    );
    let futex = in_line(&drawn, "waiting in futex(2)", seconds);
    within(
        "worker in futex(2)",
        futex,
        phase("futex") * 0.97,
        phase("futex") * 1.3,
    );
    let runnable = first(&drawn, "▚ runnable", seconds);
    assert!(
        runnable >= phase("runqueue") / 2.0,
        "worker runnable {runnable}, at least half of {}",
        phase("runqueue")
    );
}

/// `wake`: the ring a -> b -> c -> a, each relay woken by the one before it
/// alone, as often as the laps or fewer (a reader still runnable when the
/// byte comes is not woken), and no edge the other way.
#[test]
fn wake_draws_the_ring_and_its_hand_offs() {
    let truth = manifest("wake");
    let laps = num(&truth["laps"]);
    let drawn = screen("wake", &[], 160, 40);
    let all = words(&drawn);
    for edge in truth["edges"].as_array().expect("edges") {
        let waker = edge["waker"].as_str().expect("a waker");
        let wakee = edge["wakee"].as_str().expect("a wakee");
        // `relay-b 1527173     299  relay-a  100%`: the wakee's row.
        let at = all
            .windows(5)
            .position(|w| w[0] == wakee && count(w[1]).is_some() && count(w[2]).is_some())
            .unwrap_or_else(|| panic!("no row for {wakee}:\n{drawn}"));
        let row = &all[at..at + 5];
        // 266 of 300 seen with nine test runs on the machine: the bound is
        // well under that, and never above the laps.
        within(
            &format!("{wakee}'s wakes"),
            count(row[2]).expect("wakes"),
            laps * 0.6,
            laps,
        );
        assert_eq!(row[3], waker, "{wakee} mostly woken by {waker}: {row:?}");
        assert_eq!(percent(row[4]), Some(100.0), "and by nothing else: {row:?}");
    }
}

/// `flows`: the socket's bytes are exact, each way.
#[test]
fn flows_draws_the_sockets_bytes_exactly() {
    let truth = manifest("flows");
    let drawn = screen("flows", &[], 160, 50);
    let sent = first(&drawn, " over ", bytes);
    let received = first(&drawn, "sent,", bytes);
    assert_eq!(sent, num(&truth["socket"]["sent"]), "sent:\n{drawn}");
    assert_eq!(
        received,
        num(&truth["socket"]["received"]),
        "received:\n{drawn}"
    );
}

/// `flows`: each file's bytes are exact, each way, on the descriptor the
/// manifest says it was opened on (`io.syscall`'s rows are per descriptor).
#[test]
fn flows_draws_each_files_bytes_exactly() {
    let truth = manifest("flows");
    let drawn = screen("flows", &[], 160, 50);
    for file in truth["files"].as_array().expect("files") {
        let fd = file["fd"].as_i64().expect("fd");
        let name = file["name"].as_str().expect("name");
        // `fd 40            █     →8.00MiB ←8.00MiB   ┃…`: the row's own
        // words, up to the next column.
        let row: Vec<&str> = after(&drawn, &format!(" fd {fd} "))
            .into_iter()
            .take_while(|word| !word.contains('┃'))
            .collect();
        let way = |arrow: char| {
            row.iter()
                .find_map(|word| word.strip_prefix(arrow).and_then(bytes))
                .unwrap_or(0.0)
        };
        assert_eq!(
            way('→'),
            num(&file["written"]),
            "{name} written, fd {fd}: {row:?}\n{drawn}"
        );
        assert_eq!(
            way('←'),
            num(&file["read"]),
            "{name} read, fd {fd}: {row:?}\n{drawn}"
        );
    }
}

/// `heatmap`: the regimes are the phases -- 600 receives about 1.5 ms apart
/// in two phases, 50 at about 12 ms between them, and that slow phase's span
/// about 50 x 12 ms.
#[test]
fn heatmap_finds_the_phases_regimes() {
    let truth = manifest("heatmap");
    let phases = truth["phases"].as_array().expect("phases");
    let drawn = screen("heatmap", &[], 160, 50);
    let all = words(&drawn);
    // `~1.58ms 46% (600) in 12 bursts`, `~12.1ms 3.8% (50) in bursts ...,
    // 539ms to 1.22s`.
    let regime = |requests: f64| -> (f64, &[&str]) {
        let at = all
            .iter()
            .position(|word| *word == format!("({requests})"))
            .unwrap_or_else(|| panic!("no regime of {requests}:\n{drawn}"));
        let value = seconds(all[at - 2]).expect("a regime's latency");
        (value, &all[at..(at + 12).min(all.len())])
    };
    let fast = num(&phases[0]["requests"]) + num(&phases[2]["requests"]);
    let fast_pause = num(&phases[0]["pause_ms"]) / 1e3;
    let (value, _) = regime(fast);
    within(
        "the fast phases' latency",
        value,
        fast_pause,
        fast_pause * 1.4,
    );
    let slow = num(&phases[1]["requests"]);
    let slow_pause = num(&phases[1]["pause_ms"]) / 1e3;
    let (value, rest) = regime(slow);
    within(
        "the slow phase's latency",
        value,
        slow_pause,
        slow_pause * 1.25,
    );
    let to = rest
        .iter()
        .position(|word| *word == "to")
        .expect("the slow phase's span");
    let span = seconds(rest[to + 1]).expect("its end") - seconds(rest[to - 1]).expect("its start");
    within(
        "the slow phase's span",
        span,
        slow * slow_pause,
        slow * slow_pause * 1.3,
    );
}

/// `ledger`: 0.5 s of cpu or more (the interpreter starts too), 64 MiB
/// touched and held, and the socket's bytes exact.
#[test]
fn ledger_carries_the_manifests_costs() {
    let truth = manifest("ledger");
    let drawn = screen("ledger", &[], 160, 50);
    let cpu = first(&drawn, "cpu time", seconds);
    let cpu_s = num(&truth["cpu_s"]);
    within("cpu time", cpu, cpu_s, cpu_s * 1.5);
    let grown = first(&drawn, "memory held", |word| {
        word.strip_prefix('+').and_then(bytes)
    });
    let held = num(&truth["memory"]["bytes"]);
    within("memory grown", grown, held, held * 1.1);
    // The bytes written through descriptors are the file's (the socket's are
    // the network line's). A few more, the interpreter's own output, may
    // join them; never fewer.
    let written = first(&drawn, "read,", bytes);
    let file = num(&truth["file"]["written"]);
    within("bytes written", written, file, file * 1.01);
    let out = first(&drawn, "moved", bytes);
    let received = first(&drawn, "out,", bytes);
    assert_eq!(out, num(&truth["socket"]["sent"]), "sent:\n{drawn}");
    assert_eq!(
        received,
        num(&truth["socket"]["received"]),
        "received:\n{drawn}"
    );
}

/// `system`: the program and its child each burn 1.0 s on the cpu, at once;
/// the disk moves at least the 16 MiB fsynced, and loopback at least twice
/// the 8 MiB sent over it (a byte counted out and in, a figure each).
#[test]
fn system_shows_the_programs_known_load() {
    let truth = manifest("system");
    let drawn = screen("system", &[], 160, 60);
    let burn = num(&truth["burn_s"]);
    // `│ t1529755  52.0% ▁▁▁▁▆▆██████▂▂▂▁   1.05s ...`: a process's row, its
    // cpu time the first duration after its share.
    let burners = drawn
        .lines()
        .map(words)
        .filter(|row| {
            row.first()
                .is_some_and(|id| id.trim_start_matches('t').parse::<u32>().is_ok())
                && row.get(1).and_then(|share| percent(share)).is_some()
        })
        .filter_map(|row| row.iter().skip(2).find_map(|word| seconds(word)))
        .filter(|on_cpu| (burn * 0.97..=burn * 1.2).contains(on_cpu))
        .count();
    assert_eq!(
        burners, 2,
        "two processes on the cpu {burn} s each:\n{drawn}"
    );
    let disk: f64 = drawn
        .lines()
        .skip_while(|line| !line.contains("disk bytes moved"))
        .skip(1)
        .take_while(|line| !line.contains('├'))
        // `│259:0  ···██···  0B read  16.2MiB written`: every figure on a
        // device's line, read and written alike.
        .flat_map(|line| words(line).into_iter().filter_map(bytes))
        .sum();
    assert!(
        disk >= num(&truth["file"]["written"]),
        "disk {disk} B:\n{drawn}"
    );
    // `│lo  ···██···  8.02MiB in  8.02MiB out`: one figure per direction.
    let lo: f64 = drawn
        .lines()
        .filter(|line| line.starts_with("│lo "))
        .flat_map(|line| words(line).into_iter().filter_map(bytes))
        .sum();
    assert!(
        lo >= 2.0 * num(&truth["loopback"]["bytes"]),
        "loopback {lo} B:\n{drawn}"
    );
}

/// `lanes`: three lanes awake in turn -- each first awake no earlier than the
/// one before it -- for the same share of the span, and the device's lane
/// with its kernels where there is a device.
#[test]
fn lanes_draws_the_schedule_in_its_order() {
    let truth = manifest("lanes");
    let drawn = screen("lanes", &[], 160, 50);
    let mut starts = Vec::new();
    let mut awake = Vec::new();
    for lane in truth["lanes"].as_array().expect("lanes") {
        let name = lane.as_str().expect("a lane");
        let line = drawn
            .lines()
            .find(|line| line.trim_start_matches('│').starts_with(name))
            .unwrap_or_else(|| panic!("no lane {name}:\n{drawn}"));
        let cells = line.split('│').nth(2).expect("the lane's cells");
        starts.push(
            cells
                .chars()
                .position(|c| c == '█')
                .expect("awake somewhere"),
        );
        awake.push(first(line, "awake", percent));
    }
    assert!(
        starts.windows(2).all(|pair| pair[0] <= pair[1]),
        "lanes first awake in order: {starts:?}\n{drawn}"
    );
    let low = awake.iter().copied().fold(f64::MAX, f64::min);
    let high = awake.iter().copied().fold(0.0, f64::max);
    assert!(high <= low * 1.25, "each lane awake as long: {awake:?}");
    if drawn.contains("gpu.kernel") && !drawn.contains("gpu.kernel refused") {
        let launches = num(&truth["gpu"]["launches"]);
        assert_eq!(first(&drawn, "gpu.kernel", count), launches, "{drawn}");
        let kernel = truth["gpu"]["kernel"].as_str().expect("a kernel");
        assert!(
            drawn
                .lines()
                .any(|line| line.contains("gpu0") && line.contains(kernel)),
            "a lane for {kernel}:\n{drawn}"
        );
    }
}

/// `request`: sixteen requests, each drawn with the cause it was given --
/// none (on the cpu, microseconds), a 30 ms lock, 32 cold pages read, a
/// client pausing 40 ms -- in the plan's order.
#[test]
fn request_draws_each_requests_cause() {
    let truth = manifest("request");
    let kinds = truth["requests"].as_array().expect("requests");
    let total: f64 = kinds.iter().map(|kind| num(&kind["count"])).sum();
    let drawn = screen("request", &[], 160, 60);
    assert_eq!(in_line(&drawn, "requests served", count), total, "{drawn}");
    for index in 0..total as usize {
        let kind = &kinds[index % kinds.len()];
        let id = format!("a{:03x}", index + 1);
        // The list's row: the id first, its span next.
        let row: Vec<&str> = drawn
            .lines()
            .map(|line| {
                words(line)
                    .into_iter()
                    .skip_while(|word| *word == "▶")
                    .collect::<Vec<_>>()
            })
            .find(|row| row.first() == Some(&id.as_str()))
            .unwrap_or_else(|| panic!("no row for {id}:\n{drawn}"))[1..]
            .to_vec();
        let span = seconds(row[0]).expect("a span");
        let cause = row[1..]
            .iter()
            .copied()
            .filter(|word| word.chars().all(|c| c.is_alphabetic() || c == '/'))
            .collect::<Vec<_>>()
            .join(" ");
        match kind["cause"].as_str().expect("a cause") {
            "none" => {
                assert!(span < 1e-3, "{id} plain, {span} s");
                assert!(
                    cause.starts_with("on cpu") || cause.starts_with("net in"),
                    "{id}: {cause}"
                );
            }
            "lock_wait" => {
                let lock = num(&kind["lock_ms"]) / 1e3;
                within(&id, span, lock, lock * 1.3);
                assert!(cause.starts_with("lock/sleep"), "{id}: {cause}");
            }
            "block_io" => assert!(cause.starts_with("disk"), "{id}: {cause}"),
            "net_rx" => {
                let gap = num(&kind["gap_ms"]) / 1e3;
                within(&id, span, gap, gap * 1.3);
                assert!(cause.starts_with("net in"), "{id}: {cause}");
            }
            other => panic!("a cause the test does not know: {other}"),
        }
    }
}

/// `process`: its workload's recording lives in the gallery
/// (`docs/view/gallery/process-run/`, made by `record.sh process-run`). One
/// line per resource, each read the way a user reads it: the cpu's peak at
/// the one core `crunch` burns, the resident peak above the 32 MiB touched,
/// the socket's bytes exact over the program's life, and the lock wait named
/// on the time line.
#[test]
fn process_draws_each_line_as_its_workload_loaded_it() {
    let truth = manifest("process");
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/view/gallery/process-run");
    let mut data = omn_view::model::Data::load(&path).expect("the process recording");
    data.origin = "recording process-run".to_owned();
    let mut app = App::new(data);
    app.cost = Cost::Snapshot;
    app.open("process")
        .unwrap_or_else(|error| panic!("{error}"));
    let drawn = text(&draw_app(&mut app, 160, 40));
    let line = |name: &str| -> Vec<&str> {
        drawn
            .lines()
            .map(str::trim_start)
            .find(|line| line.starts_with(&format!("{name} ")))
            .map(words)
            .unwrap_or_else(|| panic!("no {name} line:\n{drawn}"))
    };
    let after = |words: &[&str], label: &str| -> String {
        let at = words
            .iter()
            .position(|word| *word == label)
            .unwrap_or_else(|| panic!("no `{label}` in {words:?}"));
        words[at + 1].to_owned()
    };

    // The header's life: `process-main  pid 41207 · 3 threads · 2.32s · ...`.
    let header = drawn
        .lines()
        .find(|line| line.contains(" pid ") && line.contains(" threads "))
        .unwrap_or_else(|| panic!("no header:\n{drawn}"));
    assert!(
        header.contains(truth["thread"].as_str().unwrap()),
        "{header}"
    );
    let life = words(header)
        .into_iter()
        .filter_map(seconds)
        .next()
        .unwrap_or_else(|| panic!("no life in {header}"));

    // CPU: `crunch` holds one core; the peak poll sees most of it.
    let cpu = line("CPU");
    let peak: f64 = after(&cpu, "peak").parse().expect("cores");
    assert!(
        peak >= 0.8 * num(&truth["phases"][0]["cores"]),
        "cpu peak {peak}:\n{drawn}"
    );

    // Memory: the 32 MiB touched page by page on top of the interpreter.
    let memory = line("Memory");
    let resident = bytes(&after(&memory, "peak")).expect("a size");
    assert!(
        resident >= num(&truth["memory"]["bytes"]),
        "resident peak {resident}:\n{drawn}"
    );

    // Network: the rates over the life are the socket's bytes, to the
    // rounding of three figures.
    let network = line("Network");
    let rate = |arrow: &str| -> f64 {
        let word = after(&network, arrow);
        bytes(word.trim_end_matches("/s")).expect("a rate") * life
    };
    for (arrow, key) in [("↓", "received"), ("↑", "sent")] {
        let moved = rate(arrow);
        let known = num(&truth["socket"][key]);
        assert!(
            (moved - known).abs() <= 0.02 * known,
            "{key}: {moved} B drawn, {known} B moved:\n{drawn}"
        );
    }

    // Time: the lock the holder sleeps under is a state of its own.
    let time = line("Time");
    let lock = percent(&after(&time, "lock")).expect("a share");
    assert!(lock > 0.0, "lock {lock}:\n{drawn}");
}
