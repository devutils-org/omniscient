//! The lazy mode: `omn trace <family> -- PROGRAM`.
//!
//! What a family's default composes, said in the long form and refused where
//! it cannot mean anything; what the loader refuses of a `default:` block;
//! and one run on this machine checked against what the program itself saw.
//! The other families' runs are in `docs/fixes/w24-lazy.md`, each against a
//! workload of the family's shape.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use omn::catalog::Catalog;

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

fn omn(args: &[&str]) -> Output {
    Command::new(BINARY)
        .args(args)
        .env_remove("OMN_CATALOG_DIR")
        .output()
        .expect("omn runs")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn json(output: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout(output))
        .unwrap_or_else(|error| panic!("{error}: {}\n{}", stdout(output), stderr(output)))
}

/// The requests of a dumped session, as `(source, mode, emits)`.
fn requests(session: &serde_json::Value) -> Vec<(String, String, Vec<String>)> {
    session["requests"]
        .as_array()
        .expect("a session holds requests")
        .iter()
        .map(|request| {
            (
                request["source"].as_str().unwrap().to_owned(),
                request["mode"].as_str().unwrap().to_owned(),
                request["emit"]
                    .as_array()
                    .map(|all| {
                        all.iter()
                            .map(|emit| emit.as_str().unwrap().to_owned())
                            .collect()
                    })
                    .unwrap_or_default(),
            )
        })
        .collect()
}

/// The long forms a lazy run printed.
fn long_forms(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.strip_prefix("note: composed, in the long form: "))
        .map(str::to_owned)
        .collect()
}

/// `omn list --defaults`: the families' defaults alone, as text a person
/// reads or a document a program does; every family is in it, a family with
/// no safe default by its reason; a source is no family and is refused.
#[test]
fn the_defaults_are_listed_on_their_own_as_text_or_a_document() {
    let text = omn(&["list", "--defaults"]);
    assert!(text.status.success(), "{}", stderr(&text));
    assert!(stdout(&text).starts_with("DEFAULTS:"), "{}", stdout(&text));

    let document = omn(&["list", "--defaults", "--format", "json"]);
    assert!(document.status.success(), "{}", stderr(&document));
    let defaults = json(&document);
    let families: Vec<&str> = defaults
        .as_array()
        .expect("a list")
        .iter()
        .map(|default| default["family"].as_str().expect("a family"))
        .collect();
    for family in ["cpu", "mem", "io", "net", "sched"] {
        assert!(families.contains(&family), "{family} in {families:?}");
    }
    for default in defaults.as_array().unwrap() {
        let bands = ["low", "med", "high"]
            .iter()
            .any(|band| default.get(*band).is_some());
        assert!(
            bands != default.get("none").is_some(),
            "a family has bands or the reason it has none: {default}"
        );
    }

    let one = json(&omn(&["list", "--defaults", "io", "--format", "json"]));
    assert_eq!(one.as_array().map(Vec::len), Some(1));
    let source = omn(&["list", "--defaults", "io.latency"]);
    assert_eq!(source.status.code(), Some(2));
    assert!(
        stderr(&source).contains("not a source's"),
        "{}",
        stderr(&source)
    );
}

#[test]
fn a_family_where_a_source_is_expected_composes_its_default() {
    let output = omn(&[
        "trace",
        "cpu",
        "--no-resolve",
        "--format",
        "json",
        "--",
        "true",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        requests(&json(&output)),
        [(
            "cpu.profile".to_owned(),
            "sample@99hz".to_owned(),
            vec!["stack".to_owned()]
        )]
    );
    let text = stderr(&output);
    assert!(text.contains("the cheapest it has"), "{text}");
    assert_eq!(
        long_forms(&text),
        ["omn trace cpu.profile --mode sample@99hz --emit stack -- true"]
    );
}

/// The long form is the point of printing it: typed back, it asks for the
/// same requests the family's default composed.
#[test]
fn the_long_form_printed_asks_for_what_the_default_composed() {
    for family in ["cpu", "mem", "io", "net", "sched", "proc"] {
        for band in ["low", "med", "high"] {
            let lazy = omn(&[
                "trace",
                family,
                "--cost",
                band,
                "--no-resolve",
                "--format",
                "json",
                "--",
                "true",
            ]);
            if !lazy.status.success() {
                // A band under the family's cheapest is refused; said below.
                assert!(
                    stderr(&lazy).contains("cheapest default"),
                    "{}",
                    stderr(&lazy)
                );
                continue;
            }
            let composed = requests(&json(&lazy));
            let typed: Vec<_> = long_forms(&stderr(&lazy))
                .iter()
                .flat_map(|line| {
                    let words: Vec<&str> = line
                        .strip_prefix("omn ")
                        .expect("a command line")
                        .split(' ')
                        .collect();
                    let mut args = words.clone();
                    let at = args.iter().position(|word| *word == "--").unwrap();
                    args.splice(at..at, ["--no-resolve", "--format", "json"]);
                    let output = omn(&args);
                    assert!(output.status.success(), "{line}: {}", stderr(&output));
                    requests(&json(&output))
                })
                .collect();
            assert_eq!(composed, typed, "{family} at {band}");
        }
    }
}

#[test]
fn the_implicit_trace_reaches_a_family_too() {
    let output = omn(&["mem", "--no-resolve", "--format", "json", "--", "true"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(requests(&json(&output))[0].0, "mem.rss");
}

#[test]
fn a_band_above_the_dearest_falls_to_it_and_says_so() {
    let output = omn(&[
        "proc",
        "--cost",
        "high",
        "--no-resolve",
        "--format",
        "json",
        "--",
        "true",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stderr(&output);
    assert!(
        text.contains("declares nothing at `high`, so its `low` default"),
        "{text}"
    );
}

#[test]
fn a_band_under_the_cheapest_is_refused_with_the_band_to_ask() {
    let output = omn(&["io", "--cost", "low", "--no-resolve", "--", "true"]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("`--cost med`"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn a_family_with_no_safe_default_refuses_with_the_reason_and_its_sources() {
    let output = omn(&["runtime", "--", "true"]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    let text = stderr(&output);
    assert!(text.contains("no safe default"), "{text}");
    for source in [
        "runtime.gc",
        "runtime.jit",
        "runtime.alloc",
        "runtime.custom",
    ] {
        assert!(text.contains(source), "{source}: {text}");
    }
}

#[test]
fn what_cannot_mean_anything_beside_a_family_is_refused_by_name() {
    for (args, says) in [
        (&["cpu", "-m", "count", "--", "true"][..], "--mode"),
        (&["cpu", "-e", "topk", "--", "true"], "--emit"),
        (&["cpu", "--stack", "fp", "--", "true"], "--stack"),
        (&["cpu", "--no-resolve"], "needs a target"),
        (&["cpu.profile", "--cost", "low", "--", "true"], "--budget"),
        (
            &["nosuchfamily", "--", "true"],
            "neither a source nor a family",
        ),
        (
            &["-d", "1s", "--system", "nosuchfamily"],
            "neither a source nor a family",
        ),
        (&["power", "--", "true"], "--system"),
        (&["power", "--system"], "--duration"),
        (
            &["cpu", "--cost", "extreme", "--", "true"],
            "not a cost band",
        ),
    ] {
        let output = omn(args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: {}",
            stderr(&output)
        );
        assert!(
            stderr(&output).contains(says),
            "{args:?}: {}",
            stderr(&output)
        );
    }
}

#[test]
fn a_machine_wide_default_runs_on_the_machine() {
    let output = omn(&[
        "power",
        "--system",
        "-d",
        "1s",
        "--no-resolve",
        "--format",
        "json",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let session = json(&output);
    for request in session["requests"].as_array().unwrap() {
        assert_eq!(request["target"], "system", "{request}");
    }
}

#[test]
fn the_listing_shows_every_familys_default() {
    let output = omn(&["list", "--color", "never"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    let defaults = text
        .split("\nDEFAULTS:")
        .nth(1)
        .expect("the listing ends with the defaults");
    let catalog = Catalog::embedded().unwrap();
    for family in catalog.families() {
        assert!(
            defaults
                .lines()
                .any(|line| line.starts_with(&format!("{family:<8} "))),
            "{family}: {defaults}"
        );
    }
    let one = stdout(&omn(&["list", "io", "--color", "never"]));
    assert!(one.contains("io.syscall -m trace"), "{one}");
    assert!(!one.contains("cpu.profile -m"), "{one}");
    let source = stdout(&omn(&["list", "io.syscall", "--color", "never"]));
    assert!(!source.contains("DEFAULTS:"), "{source}");
}

// ----------------------------------------------------------------- the loader

/// A copy of the catalog with the `default:` item of one family's file
/// replaced, loaded the way `--catalog` loads it.
fn with_default(family: &str, block: &str) -> Result<Catalog, String> {
    let dir = scratch(&format!("loader-{family}-{:x}", hash(block)));
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("catalog");
    for entry in std::fs::read_dir(&root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            std::fs::copy(&path, dir.join(path.file_name().unwrap())).unwrap();
        }
    }
    let file = dir.join(format!("{family}.yaml"));
    let text = std::fs::read_to_string(&file).unwrap();
    let at = text.find("\n- default:").expect("the family has a default");
    std::fs::write(&file, format!("{}\n{block}", &text[..at])).unwrap();
    Catalog::load_dir(&dir).map_err(|error| error.to_string())
}

fn hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("omn-lazy-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn the_loader_keeps_the_defaults_as_written() {
    let catalog = with_default(
        "cpu",
        "- default:\n    family: cpu\n    low:\n      - {source: cpu.profile, mode: \
         \"sample@99hz\", emit: [stack]}\n",
    )
    .expect("a default the family can keep loads");
    assert_eq!(catalog.default_of("cpu").unwrap().low.len(), 1);
}

#[test]
fn the_loader_refuses_a_default_the_family_cannot_keep() {
    let cpu =
        |requests: &str| format!("- default:\n    family: cpu\n    low:\n      - {{{requests}}}\n");
    for (block, says) in [
        (
            cpu("source: mem.rss, mode: \"poll@100ms\", emit: [summary]"),
            "not a source of the family",
        ),
        (
            cpu("source: cpu.profile, mode: count, emit: [stack]"),
            "a mode it does not have",
        ),
        (
            cpu("source: cpu.profile, mode: \"sample@99hz\", emit: [timeline]"),
            "an emit",
        ),
        (
            cpu("source: cpu.profile, mode: \"sample@99hz\", emit: [stack], keys: [sock]"),
            "a key it does not attach",
        ),
        (
            cpu("source: cpu.flow, mode: trace, emit: [raw]"),
            "cheapest backend declares `high`",
        ),
        (
            cpu("source: cpu.throttle, mode: \"poll@100ms\", emit: [summary]"),
            "it refuses a `launch`",
        ),
        (
            cpu("source: cpu.profile, mode: \"sample@99hz\", emit: [stack], target: pid"),
            "fixes only `system`",
        ),
        (
            "- default:\n    family: cpu\n    none: \"\"\n".to_owned(),
            "gives no reason",
        ),
        (
            "- default:\n    family: cpu\n".to_owned(),
            "declares no band",
        ),
        (
            format!(
                "{}\n{}",
                cpu("source: cpu.profile, mode: \"sample@99hz\", emit: [stack]"),
                cpu("source: cpu.profile, mode: \"sample@99hz\", emit: [stack]")
            ),
            "two defaults",
        ),
        (
            "- default:\n    family: cpu\n    free:\n      - {source: cpu.profile, mode: \
             \"sample@99hz\", emit: [stack]}\n"
                .to_owned(),
            "invalid `default:` block",
        ),
    ] {
        let refused = with_default("cpu", &block)
            .err()
            .unwrap_or_else(|| panic!("the loader kept:\n{block}"));
        assert!(refused.contains(says), "{says}: {refused}");
    }
}

// ------------------------------------------------------------ on this machine

/// `omn mem -- PROGRAM` against what the program says it held: a program
/// that touches 96 MiB and prints its own high-water mark from
/// `/proc/self/status` before it exits. `mem.rss` polls every 100 ms, so its
/// peak is a sample of the curve the high-water mark bounds: at most the
/// mark, and within one step of it once the program has held the memory for
/// several polls.
#[test]
fn the_memory_default_sees_what_the_program_held() {
    let script = "import time,sys\nb=bytearray(96<<20)\nfor i in range(0,len(b),4096): \
                  b[i]=1\ntime.sleep(0.5)\nprint([l for l in open('/proc/self/status') if \
                  l.startswith('VmHWM')][0].split()[1], file=sys.stderr)\n";
    let output = omn(&["mem", "--format", "json", "--", "python3", "-c", script]);
    if !output.status.success() {
        panic!("{}", stderr(&output));
    }
    let text = stderr(&output);
    let hwm_kib: f64 = text
        .lines()
        .find_map(|line| line.trim().parse().ok())
        .unwrap_or_else(|| panic!("the program printed its high-water mark: {text}"));
    let results = json(&output);
    let summary = results["results"][0]["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|output| output["emit"] == "summary")
        .expect("a summary")["series"][0]
        .clone();
    let max_kib = summary["max"].as_f64().unwrap() / 1024.0;
    assert!(
        max_kib <= hwm_kib * 1.01 && max_kib >= hwm_kib - 4096.0,
        "mem.rss peaked at {max_kib} KiB, the program's high-water mark is {hwm_kib} KiB"
    );
    assert!(max_kib > 96.0 * 1024.0, "{max_kib} KiB");
}
