//! The lazy mode: a family and a target, nothing else.
//!
//! `omn trace cpu -- prog` names a family where a source is expected, and
//! the run composes the rest -- the sources, the modes, the emits -- from
//! that family's default, the `- default:` item of its catalog file. A family
//! name has no dot, so it never collides with a source (`docs/DECISIONS.md`, "the
//! lazy mode"). `--cost low|med|high` picks the band of the default, in the
//! cost words the catalog's chains already declare; absent, the cheapest band
//! the family has.
//!
//! Nothing silent comes in with the convenience. The run prints the request
//! it composed in the long form, so the person learns what to type next
//! time; what it chose, degraded and lost is said as on any run; a band this
//! machine cannot keep is said once the backends are known; and a family with
//! no safe default refuses with the reason and the sources to pick from.

use std::ffi::OsString;

use crate::catalog::{Catalog, Cost, DefaultRequest, FamilyDefault};
use crate::cli::{Defaulted, TraceArgs};
use crate::error::{one_of, ParseError};
use crate::request::{Request, Session, Target, TargetKind};
use crate::resolve::ResolvedRequest;

/// The hidden flag a family name is carried in, from the command line to the
/// parsed arguments: what [`spell_family`] writes.
pub const FAMILY_FLAG: &str = "--family";

/// What a family's default composed for one run.
#[derive(Debug)]
pub struct Composed {
    pub session: Session,
    /// What the composition decided, said first as `note:` lines, before
    /// anything is resolved: the band, and each request in the long form.
    pub defaulted: Defaulted,
    /// The band the requests were taken from.
    pub band: Cost,
}

/// Parses `--cost`: one of the bands a default may name.
pub fn parse_band(text: &str) -> Result<Cost, String> {
    FamilyDefault::BANDS
        .into_iter()
        .find(|band| band.as_str() == text.trim())
        .ok_or_else(|| {
            format!(
                "`{text}` is not a cost band: one of {}",
                one_of(FamilyDefault::BANDS.iter().map(|band| band.as_str()))
            )
        })
}

/// Rewrites `omn trace <family> ...` into `omn trace --family <family> ...`,
/// so the parser keeps its one positional for a source.
///
/// A word with no dot where a source is expected is taken for a family; one
/// that is not a family is refused by [`compose`], naming the families and
/// what a source looks like, so a source misspelt without its dot is told
/// both. Where a source is expected is the first positional of `trace`,
/// wherever it stands: `omn trace --pid 1 -d 1s cpu` names a family as much
/// as `omn trace cpu --pid 1 -d 1s` does, so the flags before it, and the
/// values they take, are skipped the way the parser would. Expects the
/// arguments after the implicit `trace` was inserted.
pub fn spell_family(mut args: Vec<OsString>) -> Vec<OsString> {
    if args.get(1).and_then(|arg| arg.to_str()) != Some("trace") {
        return args;
    }
    let Some(at) = first_positional(&args) else {
        return args;
    };
    let family = args[at]
        .to_str()
        .filter(|word| {
            !word.is_empty()
                && word
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                && word != &"help"
        })
        .map(str::to_owned);
    if let Some(family) = family {
        args.splice(
            at..at + 1,
            [OsString::from(FAMILY_FLAG), OsString::from(family)],
        );
    }
    args
}

/// The index of the first positional word after `omn trace`, skipping the
/// flags before it and the values they take, as the parser of `trace` reads
/// them; `None` when `--` or the end comes first, or a flag is not one
/// `trace` knows (the parser then refuses it, which says more than a guess).
fn first_positional(args: &[OsString]) -> Option<usize> {
    use clap::CommandFactory;
    let mut command = crate::cli::Cli::command();
    command.build();
    let trace = command.find_subcommand("trace")?;
    let takes_value = |arg: &clap::Arg| arg.get_action().takes_values();
    let mut at = 2;
    while let Some(word) = args.get(at) {
        let word = word.to_str()?;
        if word == "--" {
            return None;
        }
        if let Some(long) = word.strip_prefix("--") {
            if !long.contains('=') {
                let arg = trace.get_arguments().find(|arg| {
                    arg.get_long() == Some(long)
                        || arg.get_all_aliases().is_some_and(|all| all.contains(&long))
                })?;
                if takes_value(arg) {
                    at += 1;
                }
            }
        } else if let Some(shorts) = word.strip_prefix('-').filter(|rest| !rest.is_empty()) {
            for (i, short) in shorts.char_indices() {
                let arg = trace
                    .get_arguments()
                    .find(|arg| arg.get_short() == Some(short))?;
                if takes_value(arg) {
                    if i + short.len_utf8() == shorts.len() {
                        at += 1;
                    }
                    break;
                }
            }
        } else {
            return Some(at);
        }
        at += 1;
    }
    None
}

/// Why `--cost` cannot mean anything on this command line, when it cannot:
/// beside a source or a session file there is no default to pick a band of.
pub fn cost_refusal(args: &TraceArgs) -> Option<String> {
    let band = args.cost?;
    (args.family.is_none()).then(|| {
        format!(
            "--cost {band} picks a band of a family's default (`omn trace cpu --cost {band} -- \
             PROGRAM`), and this command names {}: there, `--budget` is the ceiling",
            if args.file.is_some() {
                "a session file"
            } else {
                "a source"
            }
        )
    })
}

/// The same composition, from the words of an `omn trace` command line
/// after `trace` (`["cpu", "--cost", "med", "--pid", "42"]`): what a caller
/// without the parser -- `omn-mcp` -- hands over to compose a family's
/// default exactly as the command would.
pub fn compose_words(catalog: &Catalog, words: &[String]) -> Result<Composed, ParseError> {
    use clap::Parser;
    let mut argv: Vec<OsString> = vec!["omn".into(), "trace".into()];
    argv.extend(words.iter().map(OsString::from));
    let argv = spell_family(argv);
    let cli = crate::cli::Cli::try_parse_from(argv)
        .map_err(|error| ParseError::new(error.render().to_string().trim().to_owned()))?;
    let crate::cli::Command::Trace(args) = cli.command else {
        return Err(ParseError::new("not an `omn trace` command line"));
    };
    let Some(family) = args.family.clone() else {
        return Err(ParseError::new(
            "these words name no family: the first one is a family, as `cpu` or `io`",
        ));
    };
    compose(&args, catalog, &family)
}

/// Composes the session `omn trace <family>` runs, from the family's default
/// and the band asked.
pub fn compose(args: &TraceArgs, catalog: &Catalog, family: &str) -> Result<Composed, ParseError> {
    let families = catalog.families();
    if !families.contains(&family) {
        return Err(ParseError::new(format!(
            "`{family}` is neither a source nor a family: a source is `<family>.<metric>`, for \
             instance `io.latency`, and a family is one of {}",
            one_of(families.iter().copied())
        )));
    }
    if let Some(flag) = per_request_flag(args) {
        return Err(ParseError::new(format!(
            "`omn trace {family}` composes its requests from the family's default, so {flag} \
             has nothing to apply to: to choose it, name a source (`omn list {family}` lists \
             them, and the run prints the long form it composed)"
        )));
    }
    let sources = || one_of(catalog.family(family).iter().map(|entry| entry.id.as_str()));
    let Some(default) = catalog.default_of(family) else {
        return Err(ParseError::new(format!(
            "family `{family}` declares no default in this catalog: name one of its sources, {}",
            sources()
        )));
    };
    if let Some(reason) = &default.none {
        return Err(ParseError::new(format!(
            "family `{family}` has no safe default: {}. Its sources are {}",
            reason.trim(),
            sources()
        )));
    }

    let mut defaulted = Defaulted::default();
    let (band, requests) = choose_band(default, args.cost)?;
    let declared: Vec<&str> = default.bands().map(|(band, _)| band.as_str()).collect();
    defaulted.record(match args.cost {
        None => format!(
            "`omn trace {family}` asks the family's first question -- {} -- with its `{band}` \
             default, the cheapest it has{}",
            question(default),
            match declared.as_slice() {
                [_] => String::new(),
                more => format!(
                    " ({} asks for more)",
                    more.iter()
                        .skip(1)
                        .map(|band| format!("`--cost {band}`"))
                        .collect::<Vec<_>>()
                        .join(" or ")
                ),
            }
        ),
        Some(asked) if asked == band => format!(
            "`omn trace {family} --cost {band}` asks the family's first question -- {} -- with \
             its `{band}` default",
            question(default)
        ),
        Some(asked) => format!(
            "family `{family}` declares nothing at `{asked}`, so its `{band}` default, the \
             dearest under it, is what this composes; it asks {}",
            question(default)
        ),
    });

    let machine_only = requests.iter().all(|request| request.target.is_some());
    let target = crate::cli::target_from(args);
    let person = match (machine_only, target) {
        (true, None | Some(Target::System)) => {
            if args.duration.is_none() {
                return Err(ParseError::new(format!(
                    "`omn trace {family}` reads the whole machine, which never ends on its own: \
                     give --duration, for instance `omn trace {family} --system -d 5s`"
                )));
            }
            Target::System
        }
        (true, Some(other)) => {
            return Err(ParseError::new(format!(
                "family `{family}`'s default reads the whole machine, and this names a `{}` \
                 target: its sources are measured for the machine only, so give --system and \
                 --duration (`omn trace {family} --system -d 5s`), and run the program beside it",
                other.kind()
            )));
        }
        (false, Some(target)) => target,
        (false, None) => {
            return Err(ParseError::new(format!(
                "`omn trace {family}` needs a target: a program after `--` (`omn trace {family} \
                 -- ./prog`), or --pid PID with --duration"
            )));
        }
    };

    let built: Vec<Request> = requests
        .iter()
        .map(|request| Request {
            source: request.source.clone(),
            target: match request.target {
                Some(TargetKind::System) => Target::System,
                _ => person.clone(),
            },
            mode: request.mode,
            emit: request.emit.clone(),
            keys: request.keys.clone(),
            filter: None,
            duration: args.duration,
            budget: args.budget,
            stack: request.stack,
            symbols: None,
            backend: None,
            label: request.label.clone(),
            window: None,
            value: None,
            sum: Vec::new(),
        })
        .collect();
    for request in &built {
        defaulted.record(format!(
            "composed, in the long form: {}",
            long_form(request)
        ));
    }
    if built.len() > 1 {
        defaulted.record(format!(
            "the {} requests run together, over one window: `--dump-request --format yaml` \
             writes them as a session file for `omn -f`",
            built.len()
        ));
    }

    let mut built = built.into_iter();
    let first = built.next().expect("a declared band holds a request");
    let mut session = Session::single(first, args.format, args.out.clone());
    session.requests.extend(built);
    Ok(Composed {
        session,
        defaulted,
        band,
    })
}

/// The band to compose from: the one asked when the family declares it, the
/// dearest under it when it does not, refused when every band is dearer.
fn choose_band(
    default: &FamilyDefault,
    asked: Option<Cost>,
) -> Result<(Cost, &[DefaultRequest]), ParseError> {
    let mut bands = default.bands();
    let Some(asked) = asked else {
        return Ok(bands
            .next()
            .expect("the loader keeps a default with a band"));
    };
    if let Some(under) = default.bands().filter(|(band, _)| *band <= asked).last() {
        return Ok(under);
    }
    let (cheapest, _) = bands
        .next()
        .expect("the loader keeps a default with a band");
    Err(ParseError::new(format!(
        "family `{}`'s cheapest default costs `{cheapest}`, above the `{asked}` asked: ask \
         `--cost {cheapest}`, or name a source whose band fits (`omn list {}` gives each one's \
         cost)",
        default.family, default.family
    )))
}

fn question(default: &FamilyDefault) -> String {
    default
        .question
        .as_deref()
        .map(|question| format!("\"{}\"", question.trim()))
        .unwrap_or_else(|| "what it measures".to_owned())
}

/// The first flag given that describes one request, which a default
/// composes itself.
fn per_request_flag(args: &TraceArgs) -> Option<&'static str> {
    [
        (args.mode.is_some(), "--mode"),
        (!args.emit.is_empty(), "--emit"),
        (!args.keys.is_empty(), "--keys"),
        (args.filter.is_some(), "--filter"),
        (args.stack.is_some(), "--stack"),
        (args.symbols.is_some(), "--symbols"),
        (args.backend.is_some(), "--backend"),
        (args.window.is_some(), "--window"),
        (args.until.is_some(), "--until"),
        (args.label.is_some(), "--label"),
    ]
    .into_iter()
    .find_map(|(given, flag)| given.then_some(flag))
}

/// One request as the command line that asks for it alone.
pub fn long_form(request: &Request) -> String {
    let mut words = vec![
        "omn trace".to_owned(),
        request.source.to_string(),
        format!("--mode {}", request.mode),
    ];
    if !request.emit.is_empty() {
        words.push(format!("--emit {}", join(&request.emit)));
    }
    if !request.keys.is_empty() {
        words.push(format!("--keys {}", join(&request.keys)));
    }
    if let Some(stack) = request.stack {
        words.push(format!("--stack {stack}"));
    }
    if let Some(duration) = request.duration {
        words.push(format!("--duration {duration}"));
    }
    if let Some(budget) = request.budget {
        words.push(format!("--budget {budget}"));
    }
    words.push(match &request.target {
        Target::Launch(argv) => format!(
            "-- {}",
            argv.iter()
                .map(|arg| quoted(arg))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        Target::Pid(pids) => format!("--pid {}", join(pids)),
        Target::Tid(tids) => format!("--tid {}", join(tids)),
        Target::Cgroup(path) => format!("--cgroup {}", quoted(&path.display().to_string())),
        Target::Cpu(set) => format!("--cpu {set}"),
        Target::Iface(name) => format!("--iface {}", quoted(name)),
        Target::Container(name) => format!("--container {}", quoted(name)),
        Target::System => "--system".to_owned(),
    });
    words.join(" ")
}

fn join<T: std::fmt::Display>(items: &[T]) -> String {
    items
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// A word as a shell would read it back: quoted when it holds anything but
/// the characters a command line passes untouched.
fn quoted(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:,@%+".contains(c));
    if plain {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// Moves each request of a composed default that this machine serves above
/// the band asked onto the first later backend of its chain that declares
/// the band or less and resolves here: `mem.rss` at `--cost low` is served
/// by `fs:proc:smaps_rollup` (`med`) first, and by `fs:proc:statm` (`free`)
/// once the band is what chooses. Returns what moved, in words; a request no
/// backend can keep the band for is left as it was, for [`band_notes`] to
/// say.
pub fn keep_band(
    catalog: &Catalog,
    capabilities: &crate::capabilities::Capabilities,
    band: Cost,
    resolutions: &mut [ResolvedRequest],
) -> Vec<String> {
    let mut moved = Vec::new();
    for resolved in resolutions.iter_mut() {
        if resolved.backend.cost <= band || resolved.request.backend.is_some() {
            continue;
        }
        let Some(entry) = catalog.get(&resolved.request.source) else {
            continue;
        };
        let after = entry
            .backends
            .iter()
            .position(|backend| backend.id == resolved.backend.id)
            .map_or(0, |at| at + 1);
        let cheaper = entry.backends[after..]
            .iter()
            .filter(|backend| backend.cost <= band)
            .find_map(|backend| {
                let mut request = resolved.request.clone();
                request.backend = backend.id.parse().ok();
                crate::resolve::resolve(catalog, capabilities, &request).ok()
            });
        let Some(cheaper) = cheaper else {
            continue;
        };
        let request = &resolved.request;
        moved.push(format!(
            "the `{band}` band was asked, so `{}` is served through `{}`, which declares `{}`, \
             in place of `{}` (`{}`), the first of its chain{}",
            request.label.as_deref().map_or_else(
                || request.source.to_string(),
                |label| format!("{} ({label})", request.source)
            ),
            cheaper.backend.id,
            cheaper.backend.cost,
            resolved.backend.id,
            resolved.backend.cost,
            entry
                .backend(&cheaper.backend.id)
                .and_then(|backend| backend.degraded.as_deref())
                .map(|degraded| format!(": it gives up {}", degraded.trim()))
                .unwrap_or_default()
        ));
        *resolved = cheaper;
    }
    moved
}

/// What a band asked cannot be kept to on this machine, once the backends
/// are known: a request served here by a backend that declares more than the
/// band. Said, not refused: the band chose the requests, the machine chose
/// the backend, and the cost line after the run measures what it spent.
pub fn band_notes(band: Cost, resolutions: &[ResolvedRequest]) -> Vec<String> {
    resolutions
        .iter()
        .filter(|resolved| resolved.backend.cost > band)
        .map(|resolved| {
            let request = &resolved.request;
            format!(
                "the `{band}` band was asked, and this machine serves `{}` through `{}`, which \
                 declares `{}`: the run costs more here than the band, and its cost line says \
                 what it spent",
                request.label.as_deref().map_or_else(
                    || request.source.to_string(),
                    |label| format!("{} ({label})", request.source)
                ),
                resolved.backend.id,
                resolved.backend.cost
            )
        })
        .collect()
}

/// The defaults as `omn list` shows them, after the table: every family's
/// with no selection, one family's when it is selected, nothing for a
/// source.
pub fn listing(catalog: &Catalog, selection: Option<&str>) -> String {
    let defaults: Vec<&FamilyDefault> = match selection {
        None => catalog.defaults().iter().collect(),
        Some(name) => catalog.default_of(name).into_iter().collect(),
    };
    if defaults.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "\nDEFAULTS: what `omn trace <family> -- PROGRAM` composes, cheapest band first; \
         `--cost low|med|high` picks the band (docs/lazy.md).\n",
    );
    for default in defaults {
        let family = &default.family;
        match &default.none {
            Some(reason) => out.push_str(&format!(
                "{family:<8} no safe default: {}\n",
                reason.split_whitespace().collect::<Vec<_>>().join(" ")
            )),
            None => {
                out.push_str(&format!("{family:<8} {}\n", question(default)));
                for (band, requests) in default.bands() {
                    for (at, request) in requests.iter().enumerate() {
                        let lead = if at == 0 { band.as_str() } else { "" };
                        out.push_str(&format!("  {lead:<6} {}\n", short_form(request)));
                    }
                }
            }
        }
    }
    out
}

fn short_form(request: &DefaultRequest) -> String {
    let mut words = vec![
        request.source.to_string(),
        format!("-m {}", request.mode),
        format!("-e {}", join(&request.emit)),
    ];
    if !request.keys.is_empty() {
        words.push(format!("-k {}", join(&request.keys)));
    }
    if let Some(stack) = request.stack {
        words.push(format!("--stack {stack}"));
    }
    if request.target.is_some() {
        words.push("--system".to_owned());
    }
    words.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    #[test]
    fn a_family_where_a_source_is_expected_is_carried_in_the_hidden_flag() {
        assert_eq!(
            spell_family(os(&["omn", "trace", "cpu", "--", "true"])),
            os(&["omn", "trace", "--family", "cpu", "--", "true"])
        );
        assert_eq!(
            spell_family(os(&["omn", "trace", "--pid", "1", "-d", "1s", "cpu"])),
            os(&["omn", "trace", "--pid", "1", "-d", "1s", "--family", "cpu"])
        );
        assert_eq!(
            spell_family(os(&["omn", "trace", "--pid=1", "-d1s", "--system", "mem"])),
            os(&["omn", "trace", "--pid=1", "-d1s", "--system", "--family", "mem"])
        );
        for unchanged in [
            &["omn", "trace", "cpu.profile", "--pid", "1"][..],
            &["omn", "trace", "-f", "x.yml"],
            &["omn", "list", "cpu"],
            &["omn", "trace", "help"],
            &["omn", "trace", "--pid", "1", "cpu.profile"],
            &["omn", "trace", "-d", "1s", "--", "cpu"],
        ] {
            assert_eq!(spell_family(os(unchanged)), os(unchanged));
        }
    }

    #[test]
    fn the_bands_are_the_catalogs_cost_words_a_person_would_choose() {
        assert_eq!(parse_band("low"), Ok(Cost::Low));
        assert_eq!(parse_band("med"), Ok(Cost::Med));
        assert_eq!(parse_band("high"), Ok(Cost::High));
        for refused in ["free", "extreme", "medium", "2%"] {
            assert!(parse_band(refused).is_err(), "{refused}");
        }
    }

    #[test]
    fn a_word_a_shell_would_split_is_quoted_in_the_long_form() {
        assert_eq!(quoted("./prog"), "./prog");
        assert_eq!(quoted("sleep 1"), "'sleep 1'");
        assert_eq!(quoted("it's"), r"'it'\''s'");
    }

    #[test]
    fn every_family_of_the_embedded_catalog_has_a_default_or_says_why_not() {
        let catalog = Catalog::embedded().expect("the embedded catalog loads");
        for family in catalog.families() {
            let default = catalog
                .default_of(family)
                .unwrap_or_else(|| panic!("family `{family}` declares no default"));
            assert!(
                default.none.is_some() || default.bands().next().is_some(),
                "{family}"
            );
        }
    }

    #[test]
    fn a_band_under_the_cheapest_is_refused_and_one_above_the_dearest_falls_to_it() {
        let catalog = Catalog::embedded().expect("the embedded catalog loads");
        let io = catalog.default_of("io").expect("io has a default");
        let refused = choose_band(io, Some(Cost::Low)).unwrap_err().to_string();
        assert!(refused.contains("`med`"), "{refused}");
        let proc = catalog.default_of("proc").expect("proc has a default");
        assert_eq!(
            choose_band(proc, Some(Cost::High))
                .ok()
                .map(|(band, _)| band),
            Some(Cost::Low)
        );
        assert_eq!(
            choose_band(proc, None).ok().map(|(band, _)| band),
            Some(Cost::Low)
        );
    }
}
