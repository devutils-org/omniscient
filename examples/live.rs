//! Events as they arrive, from a program that embeds `omn`.
//!
//!   cargo build --example live
//!   sudo setcap cap_bpf,cap_perfmon+ep target/debug/examples/live
//!   target/debug/examples/live [CGROUP] [DURATION]
//!
//! Watches what the tasks of a cgroup v2 (`/`, the whole machine, by default)
//! start, open and connect to, and prints each event as one JSON line on
//! standard output the moment the runner hands it over: `proc.exec`,
//! `proc.calls` narrowed in the kernel to `openat`, and `net.tcp.connect`. What
//! the machine cannot serve is said on standard error and left out; the rest
//! still runs. At the end, what each source lost and cost is said there too.
//!
//! The capabilities are the binary's own, as they are `omn`'s (`CLAUDE.md`), and
//! a relink drops them: set them again after every build.

use std::process::ExitCode;
use std::time::Duration;

use omn::capabilities::Capabilities;
use omn::collect::live::{Flow, Live};
use omn::collect::{self, StopWhenOverBudget};
use omn::format::InputFormat;
use omn::{resolve, validate, Catalog, Session};

fn session(cgroup: &str, duration: &str) -> String {
    let target = format!("{{cgroup: '{cgroup}'}}");
    format!(
        "duration: {duration}
requests:
  - {{label: exec, source: proc.exec, target: {target}, mode: trace, emit: [raw]}}
  - {{label: openat, source: proc.calls, target: {target}, mode: trace, emit: [raw],
     filter: \"syscall == 'openat'\"}}
  - {{label: connect, source: net.tcp.connect, target: {target}, mode: trace, emit: [raw]}}
"
    )
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let cgroup = args.next().unwrap_or_else(|| "/".to_owned());
    let duration = args.next().unwrap_or_else(|| "30s".to_owned());
    match run(&cgroup, &duration) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cgroup: &str, duration: &str) -> Result<(), String> {
    let catalog = Catalog::embedded().map_err(|error| error.to_string())?;
    let mut session: Session = InputFormat::Yaml
        .parse(&session(cgroup, duration))
        .map_err(|error| error.to_string())?;
    session.apply_defaults();
    if let Err(rejections) = validate::validate_session(&catalog, &session) {
        let reasons: Vec<String> = rejections.iter().map(ToString::to_string).collect();
        return Err(reasons.join("; "));
    }

    // Each request is resolved alone: one the machine cannot serve loses its
    // events, not the run.
    let capabilities = Capabilities::detect();
    let mut labels = Vec::new();
    let mut resolutions = Vec::new();
    for request in &session.requests {
        let label = request.label.clone().unwrap_or_default();
        match resolve::resolve(&catalog, &capabilities, request) {
            Ok(resolved) => {
                labels.push(label);
                resolutions.push(resolved);
            }
            Err(error) => eprintln!("note: {label} left out: {error}"),
        }
    }
    if resolutions.is_empty() {
        return Err("nothing in the session can be served on this machine".to_owned());
    }

    // A source the runner refuses at its start -- a target its program cannot
    // narrow to, a failed attach -- is set aside with the runner's reason and
    // the rest are run again, as `omn top` does.
    let collections = loop {
        let mut live = Live::new(
            resolutions.len(),
            Duration::ZERO,
            |index, events, _now_ns| {
                for event in events {
                    let line = serde_json::json!({ "request": labels[index], "event": event });
                    println!("{line}");
                }
                Flow::Continue
            },
        );
        let error = match collect::run_all_escalating(
            &resolutions,
            &mut StopWhenOverBudget,
            Some(&mut live),
        ) {
            Ok(collections) => break collections,
            Err(error) => error,
        };
        let refused: Vec<(String, String)> = error
            .each()
            .iter()
            .filter_map(|one| match one {
                collect::CollectError::NoCollector { source_id, .. }
                | collect::CollectError::UnsupportedTarget { source_id, .. }
                | collect::CollectError::Start { source_id, .. } => {
                    Some((source_id.clone(), one.to_string()))
                }
                _ => None,
            })
            .collect();
        let before = resolutions.len();
        let mut kept = (Vec::new(), Vec::new());
        for (label, resolved) in labels.drain(..).zip(resolutions.drain(..)) {
            let source = resolved.request.source.to_string();
            match refused.iter().find(|(refused, _)| *refused == source) {
                Some((_, reason)) => eprintln!("note: {label} left out: {reason}"),
                None => {
                    kept.0.push(label);
                    kept.1.push(resolved);
                }
            }
        }
        (labels, resolutions) = kept;
        if resolutions.is_empty() || resolutions.len() == before {
            return Err(error.to_string());
        }
    };

    for (label, collection) in labels.iter().zip(&collections) {
        let lost = if collection.losses.is_clean() {
            "nothing lost".to_owned()
        } else {
            format!("lost: {:?}", collection.losses)
        };
        let cost = match collection.overhead.percent() {
            Some(percent) => format!("{percent:.2} % of the target's cpu"),
            None => "cost not measured".to_owned(),
        };
        eprintln!(
            "{label}: {} events, {lost}, {cost}, stopped because {}",
            collection.events.len(),
            collection.stopped_because
        );
    }
    Ok(())
}
