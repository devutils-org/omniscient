//! `omn latency`, `omn saturate` and `omn diff`, wrapped for the command line.

use super::*;

/// A triage request refused before anything ran: it cannot mean anything as
/// asked, which is the validator's answer and carries its code.
pub(crate) fn refuse_triage(refusal: &crate::triage::Refusal) -> ExitCode {
    eprintln!("error: {refusal}");
    ExitCode::from(EXIT_REJECTED)
}

/// Prints a triage answer in the format asked for. Text is the answer's own
/// report; the structured formats are the answer itself, every row included.
pub(crate) fn print_answer<T: Serialize>(
    format: OutputFormat,
    value: &T,
    text: impl FnOnce() -> String,
) -> anyhow::Result<()> {
    match format {
        OutputFormat::Text => print!("{}", text()),
        format if format.renders_documents() => print!("{}", format.serialize_document(value)?),
        format => anyhow::bail!(
            "`{format}` describes a measurement's results, not a triage answer: use text, json, \
             yaml or toml, and `omn -f` on the --dump-session file for the results themselves"
        ),
    }
    Ok(())
}

/// Prints the session a triage question is asked through.
pub(crate) fn print_triage_session(format: OutputFormat, session: &Session) -> anyhow::Result<()> {
    let format = match format {
        OutputFormat::Text => OutputFormat::Yaml,
        other => other,
    };
    print!("{}", format.serialize_document(session)?);
    Ok(())
}

pub(crate) fn latency(args: LatencyArgs) -> anyhow::Result<ExitCode> {
    use crate::request::Target;
    use crate::triage::{latency, Method, TriageError};

    let objective = match Method::Latency.require(args.objective) {
        Ok(objective) => objective,
        Err(refusal) => return Ok(refuse_triage(&refusal)),
    };
    let target =
        match (args.pid, args.program.is_empty()) {
            (Some(pid), _) => Target::Pid(vec![pid]),
            (None, false) => Target::Launch(args.program.clone()),
            (None, true) => return Ok(refuse_triage(&crate::triage::Refusal(
                "`omn latency` needs a program: `-- PROGRAM [ARGS]` to launch one, or `--pid PID` \
                 with --duration for one already running"
                    .to_owned(),
            ))),
        };
    let ask = latency::Ask {
        objective,
        target,
        runs: args.runs,
        duration: args.duration,
        hz: args.hz,
        symbols: args.symbols,
    };
    if let Err(refusal) = ask.check() {
        return Ok(refuse_triage(&refusal));
    }
    if args.dump_session {
        print_triage_session(args.format, &ask.session())?;
        return Ok(ExitCode::SUCCESS);
    }
    let catalog = catalog(args.catalog.as_deref())?;
    let capabilities = Capabilities::detect();
    match latency::answer(&catalog, &capabilities, &ask) {
        Ok(answer) => {
            print_answer(args.format, &answer, || answer.text())?;
            Ok(outcome(&answer.failures))
        }
        Err(TriageError::Refused(refusal)) => Ok(refuse_triage(&refusal)),
        Err(error) => {
            eprintln!("error: {error}");
            Ok(ExitCode::from(exit_for(error.whose())))
        }
    }
}

pub(crate) fn saturate(args: SaturateArgs) -> anyhow::Result<ExitCode> {
    use crate::request::Target;
    use crate::triage::{saturate, Method, TriageError};

    let objective = match Method::Saturate.require(args.objective) {
        Ok(objective) => objective,
        Err(refusal) => return Ok(refuse_triage(&refusal)),
    };
    let target = match (args.pid, args.program.is_empty()) {
        (Some(pid), _) => Target::Pid(vec![pid]),
        (None, false) => Target::Launch(args.program.clone()),
        (None, true) => {
            return Ok(refuse_triage(&crate::triage::Refusal(
                "`omn saturate` needs a workload: `-- PROGRAM [ARGS]` to launch one, or \
                 `--pid PID` with --duration for one already running"
                    .to_owned(),
            )))
        }
    };
    let ask = saturate::Ask {
        objective,
        target,
        duration: args.duration,
        symbols: args.symbols,
    };
    if let Err(refusal) = ask.check() {
        return Ok(refuse_triage(&refusal));
    }
    if args.dump_session {
        print_triage_session(args.format, &ask.session())?;
        return Ok(ExitCode::SUCCESS);
    }
    let catalog = catalog(args.catalog.as_deref())?;
    let capabilities = Capabilities::detect();
    match saturate::answer(&catalog, &capabilities, &ask) {
        Ok(answer) => {
            print_answer(args.format, &answer, || answer.text())?;
            Ok(outcome(&answer.failures))
        }
        Err(TriageError::Refused(refusal)) => Ok(refuse_triage(&refusal)),
        Err(error) => {
            eprintln!("error: {error}");
            Ok(ExitCode::from(exit_for(error.whose())))
        }
    }
}

pub(crate) fn diff(args: DiffArgs) -> anyhow::Result<ExitCode> {
    use crate::triage::diff::{self, Profile};

    let load = |path: &Path| Profile::load(path);
    let (before, after) = match (load(&args.before), load(&args.after)) {
        (Ok(before), Ok(after)) => (before, after),
        (Err(error), _) | (_, Err(error)) => {
            eprintln!("error: {error}");
            return Ok(ExitCode::from(EXIT_REJECTED));
        }
    };
    let result = diff::diff(before, after);
    match args.format {
        OutputFormat::Folded => print!("{}", result.folded()),
        format => print_answer(format, &result, || result.text(args.top))?,
    }
    Ok(ExitCode::SUCCESS)
}
