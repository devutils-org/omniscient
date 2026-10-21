//! The commands behind `omn`'s command line: what `src/main.rs` dispatches
//! to, in the library, so that each can be called and tested without the
//! binary.

#![allow(clippy::too_many_arguments)]

pub(crate) use std::collections::BTreeMap;
pub(crate) use std::io::{IsTerminal, Write};
pub(crate) use std::path::{Path, PathBuf};
pub(crate) use std::process::ExitCode;

pub(crate) use serde::Serialize;

pub(crate) use crate::capabilities::Capabilities;
pub(crate) use crate::catalog::Catalog;
pub(crate) use crate::cli::{
    self, Cli, Command, Defaulted, DiffArgs, FormatsArgs, LatencyArgs, ListArgs, SaturateArgs,
    TopArgs, TraceArgs, ValidateArgs,
};
pub(crate) use crate::format::{FormatStatus, OutputFormat};
pub(crate) use crate::request::Session;
pub(crate) use crate::resolve::{self, ResolvedRequest};
pub(crate) use crate::validate::{Rejection, Report};
pub(crate) use crate::{
    collect, emit, encode, join, render, repeat, session as session_file, validate,
};

mod budget;
mod cost;
mod exit;
mod list;
mod run;
mod top;
mod trace;
mod triage;

pub(crate) use self::budget::*;
pub(crate) use self::cost::*;
pub use self::exit::*;
pub(crate) use self::list::*;
pub(crate) use self::run::*;
pub(crate) use self::top::*;
pub(crate) use self::trace::*;
pub(crate) use self::triage::*;

#[cfg(test)]
mod tests;

pub fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    if let Some(refusal) = cli.program_output_refusal() {
        eprintln!("error: {refusal}");
        return Ok(ExitCode::from(EXIT_REJECTED));
    }
    if let Some(path) = &cli.program_stderr {
        // What the variable sets, and read where the variable is: the launch
        // takes it from this process's environment and does not pass it on.
        // Set before any thread is started.
        std::env::set_var(crate::collect::PROGRAM_STDERR_VARIABLE, path);
    }
    if let Some(path) = &cli.program_stdout {
        // As the stderr one, before any thread is started.
        std::env::set_var(crate::collect::PROGRAM_STDOUT_VARIABLE, path);
    }
    if let Some(path) = &cli.envprobe {
        std::env::set_var(crate::encode::ENVPROBE_VARIABLE, path);
    }
    match cli.command {
        Command::Trace(args) => trace(*args),
        Command::List(args) => list(args),
        Command::Formats(args) => formats(args),
        Command::Validate(args) => validate_file(args),
        Command::Latency(args) => latency(args),
        Command::Saturate(args) => saturate(args),
        Command::Diff(args) => diff(args),
        Command::Top(args) => top(*args),
        Command::Selftest(args) => selftest(args),
        Command::Import(args) => match crate::cli::import(&args) {
            Ok((json, said)) => {
                match &args.output {
                    Some(path) => std::fs::write(path, json).map_err(|error| {
                        anyhow::anyhow!("cannot write {}: {error}", path.display())
                    })?,
                    None => print!("{json}"),
                }
                eprintln!("{said}");
                Ok(ExitCode::SUCCESS)
            }
            Err(error) => {
                eprintln!("error: {error}");
                Ok(ExitCode::from(EXIT_REJECTED))
            }
        },
        Command::Join(args) => match crate::cli::join(&args) {
            Ok(out) => {
                print!("{out}");
                Ok(ExitCode::SUCCESS)
            }
            Err(error) => {
                eprintln!("error: {error}");
                Ok(ExitCode::from(EXIT_REJECTED))
            }
        },
    }
}
