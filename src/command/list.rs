//! `omn list`, `omn formats`, `omn validate` and `omn selftest`.

use super::*;

/// `omn selftest`: every source listed and run briefly by this same binary,
/// as a separate program, so that the answer is the one a user meets.
pub(crate) fn selftest(args: crate::cli::SelftestArgs) -> anyhow::Result<ExitCode> {
    let omn = std::env::current_exe()?;
    let options = crate::selftest::Options {
        cell: args.cell,
        config: args.config,
        duration: std::time::Duration::from_nanos(args.duration.as_nanos()),
        sources: args.sources,
        fixtures: args.fixtures,
    };
    let sweep = crate::selftest::sweep(&omn, &options).map_err(anyhow::Error::msg)?;
    if args.format == "json" {
        println!("{}", serde_json::to_string_pretty(&sweep)?);
    } else {
        print!("{}", sweep.text());
    }
    Ok(if args.strict && !sweep.refused_after_yes().is_empty() {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

pub(crate) fn list(args: ListArgs) -> anyhow::Result<ExitCode> {
    let catalog = catalog(args.catalog.as_deref())?;
    // Before `detect`, which is what probes: the switch changes how the
    // question is asked and must be set before anything asks it.
    if args.verify_attach {
        crate::bpf::verify_attach();
    }
    if args.defaults {
        let defaults: Vec<&crate::catalog::FamilyDefault> = match args.selection.as_deref() {
            None => catalog.defaults().iter().collect(),
            Some(name) => catalog.default_of(name).into_iter().collect(),
        };
        if args.selection.is_some() && defaults.is_empty() {
            eprintln!(
                "error: there is no family `{}`; a default is a family's, not a source's",
                args.selection.as_deref().unwrap_or_default()
            );
            return Ok(ExitCode::from(EXIT_REJECTED));
        }
        match args.format {
            OutputFormat::Text => print!(
                "{}",
                crate::lazy::listing(&catalog, args.selection.as_deref()).trim_start_matches('\n')
            ),
            format if format.renders_documents() => {
                print!("{}", format.serialize_document(&defaults)?)
            }
            format => anyhow::bail!(
                "`{format}` describes results; the defaults list as text, json, yaml or toml"
            ),
        }
        return Ok(ExitCode::SUCCESS);
    }
    let capabilities = Capabilities::detect();
    match args.format {
        OutputFormat::Text => print!(
            "{}{}",
            render::list(
                &catalog,
                args.selection.as_deref(),
                &capabilities,
                args.color.style(),
            ),
            crate::lazy::listing(&catalog, args.selection.as_deref())
        ),
        format if format.renders_documents() => {
            let selected: Vec<_> = match args.selection.as_deref() {
                None => catalog.entries().iter().collect(),
                Some(name) => match name.parse().ok().and_then(|id| catalog.get(&id)) {
                    Some(entry) => vec![entry],
                    None => catalog.family(name),
                },
            };
            // The same sweep the text table's `HERE` and `COLLECTOR` columns
            // show, so a program reading the listing has what a person does:
            // what this machine serves, through which backend, what each
            // backend's requirements answered, and what this build collects.
            let coverage = collect::coverage();
            let listed: Vec<Listed<'_>> = selected
                .into_iter()
                .map(|entry| Listed {
                    here: resolve::availability(entry, &capabilities),
                    defaults: catalog
                        .default_of(&entry.family)
                        .into_iter()
                        .flat_map(|default| default.bands())
                        .flat_map(|(band, requests)| {
                            requests
                                .iter()
                                .filter(|request| request.source == entry.id)
                                .map(move |request| InDefault { band, request })
                        })
                        .collect(),
                    probes: entry
                        .backends
                        .iter()
                        .map(|backend| BackendProbes {
                            emits: entry.backend_emits(backend).to_vec(),
                            backend: backend.id.clone(),
                            collector: coverage
                                .get(entry.id.as_str())
                                .is_some_and(|built| built.contains(&backend.id.as_str())),
                            // The resolver's own sweep, so the listing and
                            // a request can never ask the question twice two
                            // different ways.
                            requirements: resolve::probe_all(&capabilities, backend, None),
                        })
                        .collect(),
                    entry,
                })
                .collect();
            print!("{}", format.serialize_document(&listed)?);
        }
        format => anyhow::bail!(
            "`{format}` describes results; the catalog lists as text, json, yaml or toml"
        ),
    }
    Ok(ExitCode::SUCCESS)
}

/// One catalog entry as `omn list` gives it to a program: the entry, and
/// what this machine and this build make of it.
#[derive(Serialize)]
pub(crate) struct Listed<'a> {
    #[serde(flatten)]
    entry: &'a crate::catalog::Entry,
    /// What the text table's `HERE` column says, with the backend it names.
    here: resolve::Availability,
    /// Every backend of the chain in order, with what each requirement's
    /// probe answered here and whether this build has a collector for it.
    probes: Vec<BackendProbes>,
    /// Where this source is in its family's default, `omn trace <family>`:
    /// the band and the request written there. Empty when it is in none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    defaults: Vec<InDefault<'a>>,
}

/// One place a source holds in its family's default.
#[derive(Serialize)]
pub(crate) struct InDefault<'a> {
    band: crate::catalog::Cost,
    #[serde(flatten)]
    request: &'a crate::catalog::DefaultRequest,
}

#[derive(Serialize)]
pub(crate) struct BackendProbes {
    backend: String,
    /// What this backend produces: its own `emits` when it declares them,
    /// the entry's otherwise.
    emits: Vec<crate::request::EmitKind>,
    collector: bool,
    requirements: Vec<resolve::Requirement>,
}

/// The machine-readable view of one output format.
#[derive(Serialize)]
pub(crate) struct FormatInfo {
    name: &'static str,
    description: &'static str,
    renders_documents: bool,
    emits: Vec<&'static str>,
    implemented: bool,
    planned_in: Option<&'static str>,
}

#[derive(Serialize)]
pub(crate) struct FormatsInfo {
    input: Vec<&'static str>,
    output: Vec<FormatInfo>,
}

pub(crate) fn formats(args: FormatsArgs) -> anyhow::Result<ExitCode> {
    match args.format {
        OutputFormat::Text => print!("{}", render::formats()),
        format if format.renders_documents() => {
            let info = FormatsInfo {
                input: crate::format::InputFormat::ALL
                    .iter()
                    .map(|f| f.as_str())
                    .collect(),
                output: OutputFormat::ALL
                    .iter()
                    .map(|output| FormatInfo {
                        name: output.as_str(),
                        description: output.describe(),
                        renders_documents: output.renders_documents(),
                        emits: crate::request::EmitKind::ALL
                            .iter()
                            .filter(|emit| output.supports_emit(**emit))
                            .map(|emit| emit.as_str())
                            .collect(),
                        implemented: output.status() == FormatStatus::Ready,
                        planned_in: match output.status() {
                            FormatStatus::Planned { phase } => Some(phase),
                            FormatStatus::Ready => None,
                        },
                    })
                    .collect(),
            };
            print!("{}", format.serialize_document(&info)?);
        }
        format => anyhow::bail!("`{format}` describes results, not the format listing"),
    }
    Ok(ExitCode::SUCCESS)
}

pub(crate) fn validate_file(args: ValidateArgs) -> anyhow::Result<ExitCode> {
    let catalog = catalog(args.catalog.as_deref())?;
    let (mut session, origin) = session_file::load(&args.file, args.input_format)?;
    if args.format != OutputFormat::default() {
        session.format = args.format;
    }

    match validate::validate_session(&catalog, &session) {
        Ok(report) => {
            print_session(&catalog, &session, &report, &Defaulted::default(), &[])?;
            eprintln!(
                "\n{origin}: {} request(s) valid against the catalog. \
                 Availability on this machine is the resolver's call, not this one's.",
                session.requests.len()
            );
            Ok(ExitCode::SUCCESS)
        }
        Err(rejections) => {
            eprintln!("{origin}:");
            Ok(reject(&rejections))
        }
    }
}
