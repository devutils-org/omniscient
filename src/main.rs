//! `omn`: the command line entry point.
//!
//! Everything here is parsing, dispatch and exit codes. The commands live in
//! the library (`omn::command`), which is also what a future API would call.

use std::ffi::OsString;
use std::process::ExitCode;

use clap::Parser;

use omn::cli::{Cli, SUBCOMMANDS};
use omn::command::{
    restore_own_dumpability, run, said_which, Unavailable, EXIT_REJECTED, EXIT_UNAVAILABLE,
    RUN_STARTED, STARTED_NS,
};

fn main() -> ExitCode {
    let _ = STARTED_NS.set(omn::clock::monotonic_ns());
    restore_own_dumpability();
    // Before any probe: a kernel that charges eBPF maps to RLIMIT_MEMLOCK
    // refuses them at a distribution's 64 KiB, root's runs included.
    omn::bpf::lift_memlock();
    // A launched program's orphans are reparented here, where a stop can
    // still end them (`collect::adopt_orphans`). This process starts nothing
    // else that outlives the call that started it.
    omn::collect::adopt_orphans();
    let cli = match Cli::try_parse_from(omn::lazy::spell_family(normalize(std::env::args_os()))) {
        Ok(cli) => cli,
        Err(error) => {
            let _ = error.print();
            // `--help` and `--version` are not failures. Everything else the
            // parser refuses -- a flag that does not exist, a value that does
            // not parse, two flags that cannot both be meant -- is a request
            // that cannot mean anything, which is the same answer the validator
            // gives one layer in and now carries the same code. It used to be
            // flattened to 1, so `--mode` misspelled and `--mode` missing
            // exited differently while being the same mistake.
            return match error.exit_code() {
                0 => ExitCode::SUCCESS,
                _ => ExitCode::from(EXIT_REJECTED),
            };
        }
    };

    match run(cli) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error:#}");
            // A request missing an axis the catalog cannot fill in -- no
            // `--mode` where a source declares two, no target where it accepts
            // several -- cannot mean anything, which is exit 2. It used to fall
            // through here to the generic failure code and tell a script to go
            // and look at the machine. `--emit`, raised one layer up by the
            // validator, has always exited 2, and the two are the same answer.
            match error.downcast_ref::<omn::error::ParseError>() {
                Some(_) => ExitCode::from(EXIT_REJECTED),
                None => {
                    said_which(if RUN_STARTED.load(std::sync::atomic::Ordering::SeqCst) {
                        Unavailable::FailedUnwritten
                    } else {
                        Unavailable::Machine
                    });
                    ExitCode::from(EXIT_UNAVAILABLE)
                }
            }
        }
    }
}

/// Inserts the implicit `trace` subcommand, so `omn io.latency --pid 1` and
/// `omn trace io.latency --pid 1` are the same command.
fn normalize(args: impl Iterator<Item = OsString>) -> Vec<OsString> {
    let mut args: Vec<OsString> = args.collect();
    let Some(first) = args.get(1).and_then(|arg| arg.to_str()) else {
        return args;
    };
    let is_subcommand = SUBCOMMANDS.contains(&first);
    let is_global_flag = matches!(first, "-h" | "--help" | "-V" | "--version");
    if !is_subcommand && !is_global_flag {
        args.insert(1, OsString::from("trace"));
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn normalized(args: &[&str]) -> Vec<String> {
        normalize(args.iter().map(OsString::from))
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_bare_source_gets_the_implicit_trace_subcommand() {
        assert_eq!(
            normalized(&["omn", "io.latency", "--pid", "1"]),
            ["omn", "trace", "io.latency", "--pid", "1"]
        );
    }

    #[test]
    fn real_subcommands_and_global_flags_are_left_alone() {
        assert_eq!(normalized(&["omn", "list", "io"]), ["omn", "list", "io"]);
        assert_eq!(normalized(&["omn", "--help"]), ["omn", "--help"]);
        assert_eq!(normalized(&["omn"]), ["omn"]);
    }
}
