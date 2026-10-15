//! `omn top` from the command line: live without a terminal, replay, gallery,
//! and what it refuses.
//!
//! A live run here is headless (`--headless -o DIR`): the same session, the
//! same runner and the same hand-over to the view as on a terminal, with the
//! results written where `--replay` reads them. On a machine without the
//! capabilities `make` grants, the privileged sources are refused, and that
//! is what is checked of them: each refusal is in `view.json` with its reason.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BINARY: &str = env!("CARGO_BIN_EXE_omn");
const EXIT_REJECTED: i32 = 2;

fn run(args: &[&str]) -> Output {
    Command::new(BINARY)
        .args(args)
        .output()
        .expect("omn should run")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("omn-view-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/view_fixtures/http-server")
}

#[test]
fn a_headless_live_run_writes_what_replay_draws() {
    let out = scratch("live");
    let output = run(&[
        "top",
        "--headless",
        "-d",
        "3s",
        "-o",
        out.to_str().unwrap(),
        "--",
        "sh",
        "-c",
        "i=0; while [ $i -lt 20000 ]; do i=$((i+1)); done; sleep 0.2",
    ]);
    assert!(
        output.status.success(),
        "omn top --headless failed:\n{}",
        text(&output.stderr)
    );
    for file in ["results.json", "resolved.json", "cost.json", "view.json"] {
        assert!(out.join(file).is_file(), "{file} was not written");
    }
    let data = omn_view::model::Data::load(&out).expect("the live run loads");
    // Every view's need is served or refused with its reason: none is
    // dropped without a word.
    for need in omn_view::views::all_needs().expect("the views' needs merge") {
        let served = data
            .run
            .results
            .iter()
            .any(|results| results.label == need.label);
        let refused = data
            .refused
            .iter()
            .find(|refusal| refusal.label == need.label);
        assert!(
            served || refused.is_some(),
            "`{}` is neither in the results nor among the refusals",
            need.label
        );
        assert!(
            !(served && refused.is_some()),
            "`{}` is both served and refused",
            need.label
        );
        if let Some(refusal) = refused {
            assert!(
                !refusal.reason.trim().is_empty(),
                "`{}` was refused without a reason",
                need.label
            );
        }
    }
    // The catalog's facts came with it.
    for results in &data.run.results {
        assert!(
            data.facts.contains_key(&results.source),
            "no facts for `{}`",
            results.source
        );
    }
    // A process's own cpu time is read from /proc, which needs no privilege:
    // the run cannot have served nothing of the target.
    assert!(
        data.run
            .results
            .iter()
            .any(|results| results.source == "cpu.oncpu_time"),
        "cpu.oncpu_time, which any machine serves, is missing:\n{}",
        text(&output.stderr)
    );

    // And `--replay` draws it, as text, without a terminal.
    let replay = run(&["top", "--replay", out.to_str().unwrap(), "--headless"]);
    assert!(replay.status.success(), "{}", text(&replay.stderr));
    let screen = text(&replay.stdout);
    assert!(screen.contains("omn top"), "{screen}");
    assert!(screen.contains(" 1 system "), "{screen}");
    std::fs::remove_dir_all(&out).ok();
}

#[test]
fn a_gallery_prints_every_view() {
    let output = run(&[
        "top",
        "--gallery",
        fixture().to_str().unwrap(),
        "--headless",
        "--size",
        "100x20",
    ]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let screen = text(&output.stdout);
    let views = omn_view::views::registry();
    for (index, view) in views.iter().enumerate() {
        // Each view has its own page. A view that draws no frame of its own
        // (`process`, a header line then one line per resource) is known by
        // its page, since the tab line at 100 columns is cut before it.
        let page = format!("gallery {}/{} ", index + 1, views.len());
        let named = screen.contains(&format!("┌ {} -- ", view.name()))
            || screen.contains(&format!(" {} -- ", view.name()))
            || screen.contains(view.title());
        assert!(
            screen.contains(&page),
            "the gallery has no page for `{}`:\n{screen}",
            view.name()
        );
        assert!(
            named || view.name() == "process",
            "the gallery does not show `{}`:\n{screen}",
            view.name()
        );
    }
    // The process view's page is its own: its header names the target's pid.
    assert!(screen.contains(" pid "), "{screen}");
    // Each screen is the size asked.
    let first: Vec<&str> = screen.lines().take(20).collect();
    assert!(first.iter().all(|line| line.chars().count() <= 100));
}

#[test]
fn the_session_a_live_run_would_ask_for_is_printed() {
    let output = run(&["top", "--dump-session", "-d", "2s", "--", "true"]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let session = text(&output.stdout);
    for need in omn_view::views::all_needs().expect("the views' needs merge") {
        let asked = session.contains(&format!("label: {}\n", need.label));
        let refused = text(&output.stderr).contains(&format!("`{}` is not asked", need.label));
        assert!(
            asked || refused,
            "`{}` is not in the session:\n{session}",
            need.label
        );
    }
    // The file is a session `omn trace -f` reads.
    let dir = scratch("dump");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("top.yml");
    std::fs::write(&path, &session).unwrap();
    let validated = run(&["validate", "-f", path.to_str().unwrap()]);
    assert!(
        validated.status.success(),
        "{}{}",
        text(&validated.stdout),
        text(&validated.stderr)
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn what_cannot_mean_anything_is_refused() {
    let unknown = run(&[
        "top",
        "--replay",
        fixture().to_str().unwrap(),
        "--view",
        "nope",
    ]);
    assert_eq!(unknown.status.code(), Some(EXIT_REJECTED));
    assert!(text(&unknown.stderr).contains("the views are system, flame"));

    let against = run(&["top", "--against", fixture().to_str().unwrap()]);
    assert_eq!(against.status.code(), Some(EXIT_REJECTED));
    assert!(text(&against.stderr).contains("needs --replay or --gallery"));

    let blind = run(&["top", "--headless", "--", "true"]);
    assert_eq!(blind.status.code(), Some(EXIT_REJECTED));
    assert!(text(&blind.stderr).contains("needs --out"));

    let missing = run(&["top", "--replay", "/nonexistent/run", "--headless"]);
    assert!(!missing.status.success());
    assert!(text(&missing.stderr).contains("does not exist"));
}

/// `omn top --batch`: the `system` view as a table on stdout, `top -b`'s
/// use, from a recording here and needing no terminal; the sort is one of
/// the view's and asks for `--batch`.
#[test]
fn a_batch_prints_the_system_view_as_a_table_sorted_as_asked() {
    let fixture = fixture();
    let fixture = fixture.to_str().unwrap();
    let by = |sort: &str| {
        let output = run(&["top", "--batch", "--replay", fixture, "--sort", sort]);
        assert!(output.status.success(), "{}", text(&output.stderr));
        text(&output.stdout)
    };
    let table = by("cpu");
    assert!(table.starts_with("# omn top --batch: "), "{table}");
    let header = table
        .lines()
        .find(|line| line.starts_with("ID "))
        .expect("the tasks' heading");
    assert!(
        header.contains(" CPU ") && header.contains(" RSS_BYTES "),
        "{header}"
    );
    // Every line is a comment, a machine series or a row of the table.
    for line in table.lines() {
        assert!(
            line.starts_with('#')
                || line.starts_with("machine ")
                || line.starts_with("ID ")
                || line.split_whitespace().count() == header.split_whitespace().count(),
            "{line}"
        );
    }
    assert_ne!(by("memory"), table, "the sort reorders the tasks");

    let unbatched = run(&["top", "--replay", fixture, "--sort", "io"]);
    assert!(!unbatched.status.success());
    assert!(text(&unbatched.stderr).contains("--batch"));
    let unknown = run(&["top", "--batch", "--replay", fixture, "--sort", "bogus"]);
    assert!(!unknown.status.success());
    assert!(text(&unknown.stderr).contains("runqueue"));
}

/// `omn top` without a controlling terminal: in a session of its own, as an
/// agent shell, CI or `nohup` runs it.
fn without_a_terminal(args: &[&str]) -> (Output, std::time::Duration) {
    use std::os::unix::process::CommandExt;
    let mut command = Command::new(BINARY);
    command.args(args).stdin(std::process::Stdio::null());
    // SAFETY: `setsid` is async-signal-safe, and nothing else runs between
    // the fork and the exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let started = std::time::Instant::now();
    let output = command.output().expect("omn should run");
    (output, started.elapsed())
}

/// A live run with no terminal to draw on is refused before it collects
/// anything, with the way to run it instead -- not after its whole duration,
/// with nothing written.
#[test]
fn a_live_run_with_no_terminal_is_refused_at_once() {
    let out = scratch("no-terminal");
    let (output, took) = without_a_terminal(&[
        "top",
        "-d",
        "20s",
        "-o",
        out.to_str().unwrap(),
        "--",
        "sleep",
        "30",
    ]);
    let stderr = text(&output.stderr);
    assert_eq!(output.status.code(), Some(EXIT_REJECTED), "{stderr}");
    assert!(stderr.contains("--headless"), "{stderr}");
    assert!(
        took < std::time::Duration::from_secs(10),
        "refused after {took:?}: it collected first"
    );
    assert!(!out.join("results.json").exists());

    let (output, _) = without_a_terminal(&["top", "--replay", fixture().to_str().unwrap()]);
    let stderr = text(&output.stderr);
    assert_eq!(output.status.code(), Some(EXIT_REJECTED), "{stderr}");
    assert!(stderr.contains("--headless"), "{stderr}");
}
