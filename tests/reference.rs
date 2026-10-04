//! omn against the tools it replaces: every pair in `catalog/references/`
//! run on the same workload and held to its declared tolerance. The runner
//! is shared (`reference/runner.rs`); each family fills its own module.
//!
//! One test per family, `pairs_<family>`, so a family runs alone with
//! `cargo test --test reference pairs_perfstat -- --nocapture`; one pair
//! alone with `OMN_REFERENCE_PAIR=<label>` as well. Each ends on its tally
//! line, and `reference_tally` reads them across families afterwards. The
//! schema of a pair is `catalog/references/README.md`.

#[path = "reference/common.rs"]
mod common;
#[path = "reference/runner.rs"]
mod runner;

#[path = "reference/bpftrace.rs"]
mod bpftrace;
#[path = "reference/gpu.rs"]
mod gpu;
#[path = "reference/heap.rs"]
mod heap;
#[path = "reference/io.rs"]
mod io;
#[path = "reference/net.rs"]
mod net;
#[path = "reference/perfrecord.rs"]
mod perfrecord;
#[path = "reference/perfsched.rs"]
mod perfsched;
#[path = "reference/perfstat.rs"]
mod perfstat;
#[path = "reference/power.rs"]
mod power;
#[path = "reference/sysstat.rs"]
mod sysstat;
#[path = "reference/top.rs"]
mod top;

use common::Parser;
use std::collections::BTreeSet;

/// Every family, with its parsers. A family is a YAML file, a module, and a
/// table in `docs/replaces/`; `families_are_whole` holds the three together.
fn families() -> Vec<(&'static str, Vec<Parser>)> {
    vec![
        ("perfstat", perfstat::parsers()),
        ("perfrecord", perfrecord::parsers()),
        ("perfsched", perfsched::parsers()),
        ("bpftrace", bpftrace::parsers()),
        ("sysstat", sysstat::parsers()),
        ("top", top::parsers()),
        ("heap", heap::parsers()),
        ("net", net::parsers()),
        ("power", power::parsers()),
        ("gpu", gpu::parsers()),
        ("io", io::parsers()),
    ]
}

fn parsers_of(family: &str) -> Vec<Parser> {
    families()
        .into_iter()
        .find(|(f, _)| *f == family)
        .map(|(_, p)| p)
        .unwrap_or_else(|| panic!("no family `{family}`"))
}

macro_rules! family_pairs {
    ($($test:ident => $family:literal),* $(,)?) => {$(
        #[test]
        fn $test() {
            runner::run_family($family, &parsers_of($family));
        }
    )*};
}

family_pairs! {
    pairs_perfstat => "perfstat",
    pairs_perfrecord => "perfrecord",
    pairs_perfsched => "perfsched",
    pairs_bpftrace => "bpftrace",
    pairs_sysstat => "sysstat",
    pairs_top => "top",
    pairs_heap => "heap",
    pairs_net => "net",
    pairs_power => "power",
    pairs_gpu => "gpu",
    pairs_io => "io",
}

fn names_in(dir: &str, extension: &str) -> BTreeSet<String> {
    std::fs::read_dir(common::root().join(dir))
        .unwrap_or_else(|e| panic!("{dir}: {e}"))
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            name.strip_suffix(extension).map(String::from)
        })
        .collect()
}

/// A family is three files that name each other: its pairs, its parsers,
/// its table. One without the others is a family half written.
#[test]
fn families_are_whole() {
    let known: BTreeSet<String> = families().iter().map(|(f, _)| f.to_string()).collect();
    assert_eq!(
        names_in("catalog/references", ".yaml"),
        known,
        "catalog/references/*.yaml and the families of tests/reference.rs differ"
    );
    assert_eq!(
        names_in("docs/replaces", ".md"),
        known,
        "docs/replaces/*.md and the families of tests/reference.rs differ"
    );
    let index = std::fs::read_to_string(common::root().join("docs/replaces.md")).unwrap();
    for family in &known {
        assert!(
            index.contains(&format!("replaces/{family}.md")),
            "docs/replaces.md does not link docs/replaces/{family}.md"
        );
    }
}

/// Every pair is valid data before anything runs: it parses, its parser
/// exists, its tolerance and its reason are written, and so on
/// (`Pair::problems`). Runs nothing, so it holds on any machine.
#[test]
fn every_pair_is_well_formed() {
    let mut problems = Vec::new();
    for (family, parsers) in families() {
        let names: Vec<&str> = parsers.iter().map(|p| p.name).collect();
        let unique: BTreeSet<&str> = names.iter().copied().collect();
        if unique.len() != names.len() {
            problems.push(format!("{family}: two parsers share a name"));
        }
        let pairs = match runner::load(family) {
            Ok(p) => p,
            Err(e) => {
                problems.push(e);
                continue;
            }
        };
        let mut ids = BTreeSet::new();
        for pair in &pairs {
            if let Some(id) = &pair.id {
                if !ids.insert(id.clone()) {
                    problems.push(format!("{family}: id `{id}` is given twice"));
                }
            }
            for p in pair.problems(&parsers) {
                problems.push(format!("{family} {}: {p}", pair.label()));
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// The tally of every family whose `pairs_<family>` ran unfiltered since the
/// last build, read back after the fact: one line per family, a total, and a
/// failure when any family failed a pair. Ignored by default, because it
/// reads what the other tests leave and cargo runs tests in any order:
///
/// ```console
/// cargo test --test reference pairs_ -- --nocapture
/// cargo test --test reference reference_tally -- --ignored --nocapture
/// ```
#[test]
#[ignore = "reads the tallies the pairs_<family> tests leave; run it after them"]
fn reference_tally() {
    let mut total = runner::Tally::default();
    let mut failed = Vec::new();
    let mut missing = Vec::new();
    for (family, _) in families() {
        let Ok(line) = std::fs::read_to_string(runner::tally_file(family)) else {
            missing.push(family);
            continue;
        };
        let line = line.trim();
        eprintln!("{line}");
        let count = |what: &str| -> usize {
            line.split(", ")
                .find_map(|part| {
                    let part = part.rsplit(": ").next().unwrap_or(part);
                    part.strip_suffix(what)?.trim().parse().ok()
                })
                .unwrap_or(0)
        };
        total.held += count("held");
        total.skipped += count("skipped");
        total.gaps += count("gaps");
        total.known_defects += count("known defects");
        let f = count("failed");
        total.failed += f;
        if f > 0 {
            failed.push(family);
        }
    }
    if !missing.is_empty() {
        eprintln!("not run: {}", missing.join(", "));
    }
    eprintln!("{}", total.line("all"));
    assert!(
        missing.len() < families().len(),
        "no family left a tally: run `cargo test --test reference pairs_` first"
    );
    assert!(
        failed.is_empty(),
        "families with a pair that did not hold: {}",
        failed.join(", ")
    );
}

/// One table row: its cells, trimmed.
fn rows(markdown: &str) -> Vec<Vec<String>> {
    markdown
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with('|'))
        .skip(2) // the header and its separator
        .map(|l| {
            l.trim_matches('|')
                .split('|')
                .map(|c| c.trim().to_string())
                .collect()
        })
        .collect()
}

/// Whether a row of a family's table is backed by a pair of that family,
/// or says why it has none.
fn row_is_backed(row: &[String], pairs: &[runner::Pair]) -> bool {
    let text = row.join(" | ");
    if text.to_lowercase().contains("no pair") {
        return true;
    }
    let first = row.first().map(String::as_str).unwrap_or("");
    pairs.iter().any(|p| {
        p.id.as_deref()
            .is_some_and(|id| text.contains(&format!("`{id}`")))
            || (text.contains(&p.source) && first.contains(&p.tool))
    })
}

/// Every row of `docs/replaces/<family>.md` names a pair -- its `id` in
/// backquotes, or its `source` anywhere in the row and its `tool` in the
/// first cell -- or says `no pair` and why (README, "The tables").
#[test]
fn every_row_names_a_pair_or_why_none() {
    let mut orphans = Vec::new();
    for (family, _) in families() {
        let path = common::root().join(format!("docs/replaces/{family}.md"));
        let text = std::fs::read_to_string(&path).unwrap();
        let pairs = runner::load(family).unwrap_or_default();
        for row in rows(&text) {
            if !row_is_backed(&row, &pairs) {
                orphans.push(format!("{family}: {}", row.join(" | ")));
            }
        }
    }
    assert!(
        orphans.is_empty(),
        "rows naming no pair and saying no reason (`no pair: ...`):\n  {}",
        orphans.join("\n  ")
    );
}

#[test]
fn a_row_is_backed_by_its_pair_or_its_reason() {
    let pair: runner::Pair = serde_norway::from_str(
        r#"
source: cpu.instructions
tool: perf stat
use: "perf stat -e instructions -- prog"
omn: [cpu.instructions]
reference: {command: [perf, stat], parse: x}
compare: {omn: a, tolerance: {rel: 0.02}, why: "w"}
"#,
    )
    .unwrap();
    let table = "\
| Tool and use | omn equivalent | Covered | Same numbers | Missing |
|---|---|---|---|---|
| `perf stat -e instructions` | `omn cpu.instructions -m count` | full | within 2 % | nothing |
| `perf stat -e cycles` | `omn cpu.cycles` | full | yes | nothing |
| `perf stat --topdown` | none | none | no pair: omn has no top-down level 2 | top-down |
";
    let backed: Vec<bool> = rows(table)
        .iter()
        .map(|r| row_is_backed(r, std::slice::from_ref(&pair)))
        .collect();
    assert_eq!(backed, [true, false, true]);
}
