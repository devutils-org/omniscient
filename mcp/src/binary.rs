//! The `omn` binary, which answers every question that depends on the machine.
//!
//! This server probes nothing itself. A probe answers for the process that
//! asks it: `CAP_BPF` and `CAP_PERFMON` are file capabilities of `omn`, and the
//! preload shim is found beside it, so the same library linked into this
//! server answers a different sweep -- measured on the development machine,
//! a copy of `omn` without them serves 52 sources where `omn` serves 93 --
//! and nothing in the answer would say so. That is the silent failure `CLAUDE.md` warns about for
//! a relinked `omn`, one binary over. So the sweep, the resolution and the run
//! are asked of the binary that would run the session, and every answer says
//! which binary that was and which capabilities it carried.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use omn::Catalog;
use serde_json::{json, Value};

/// The binary and what it carries, read once per question.
#[derive(Debug, Clone)]
pub struct Binary {
    pub path: PathBuf,
}

/// What came back from one invocation.
#[derive(Debug)]
pub struct Output {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// Set when the invocation was killed for outliving its bound.
    pub killed_after: Option<Duration>,
}

impl Binary {
    /// `omn` beside this server's own executable, which is where `cargo` and
    /// `make` put both.
    pub fn beside_this_server() -> PathBuf {
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join("omn")))
            .unwrap_or_else(|| PathBuf::from("omn"))
    }

    /// The part of every machine-dependent answer that says who answered it.
    pub fn answered_by(&self) -> Value {
        let caps = file_capabilities(&self.path);
        json!({
            "binary": self.path.display().to_string(),
            "file_capabilities": caps.as_json(),
        })
    }

    /// What an answer from this binary is missing because of its capabilities.
    pub fn capability_shortfall(&self) -> Vec<String> {
        let caps = file_capabilities(&self.path);
        let mut missing = Vec::new();
        match caps {
            FileCaps::Unreadable(reason) => missing.push(format!(
                "the capabilities of {} could not be read ({reason}), so whether its eBPF and \
                 per-cpu perf answers are privileged ones is unknown",
                self.path.display()
            )),
            FileCaps::Read { bpf, perfmon, .. } if !(bpf && perfmon) => missing.push(format!(
                "{} carries {}; every eBPF and per-cpu perf backend reads missing for that \
                 reason and not for the kernel's. `make` grants them, and every relink drops them",
                self.path.display(),
                match (bpf, perfmon) {
                    (false, false) => "neither CAP_BPF nor CAP_PERFMON",
                    (false, true) => "CAP_PERFMON but not CAP_BPF",
                    _ => "CAP_BPF but not CAP_PERFMON",
                }
            )),
            FileCaps::Read { .. } => {}
        }
        missing
    }

    /// Runs `omn` with `args`, feeding `stdin`, killed after `bound`.
    pub fn invoke(
        &self,
        args: &[String],
        stdin: Option<&str>,
        bound: Duration,
    ) -> Result<Output, String> {
        let mut child = Command::new(&self.path)
            .args(args)
            .env("NO_COLOR", "1")
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                format!(
                    "cannot run {}: {error}; start the server with --omn PATH",
                    self.path.display()
                )
            })?;
        if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
            // A session is a few kilobytes; the pipe holds it.
            let _ = pipe.write_all(text.as_bytes());
        }
        let mut stdout_pipe = child.stdout.take().expect("piped");
        let mut stderr_pipe = child.stderr.take().expect("piped");
        let out_reader = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = stdout_pipe.read_to_string(&mut text);
            text
        });
        let err_reader = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr_pipe.read_to_string(&mut text);
            text
        });

        let started = Instant::now();
        let mut killed_after = None;
        let status: ExitStatus = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if started.elapsed() > bound => {
                    let _ = child.kill();
                    killed_after = Some(started.elapsed());
                    break child.wait().map_err(|error| error.to_string())?;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(error) => return Err(format!("waiting for omn: {error}")),
            }
        };
        Ok(Output {
            status: status.code(),
            stdout: out_reader.join().unwrap_or_default(),
            stderr: err_reader.join().unwrap_or_default(),
            killed_after,
        })
    }

    /// `omn list`'s capability sweep, one row per catalog source.
    pub fn sweep(&self, catalog: &Catalog) -> Result<Value, String> {
        let output = self.invoke(
            &["list".into(), "--color".into(), "never".into()],
            None,
            Duration::from_secs(30),
        )?;
        if output.status != Some(0) {
            return Err(format!(
                "`omn list` exited {:?}: {}",
                output.status,
                output.stderr.trim()
            ));
        }
        parse_sweep(&output.stdout, catalog)
    }

    /// `omn list <source>` in full, as the binary renders it: every backend,
    /// every requirement, and the probe's answer for each.
    pub fn listing(&self, source: &str) -> Result<String, String> {
        let output = self.invoke(
            &[
                "list".into(),
                "--color".into(),
                "never".into(),
                source.into(),
            ],
            None,
            Duration::from_secs(30),
        )?;
        match output.status {
            Some(0) => Ok(output.stdout),
            status => Err(format!(
                "`omn list {source}` exited {status:?}: {}",
                output.stderr.trim()
            )),
        }
    }
}

/// A binary's file capabilities, read from `security.capability`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileCaps {
    Read {
        bpf: bool,
        perfmon: bool,
        net_raw: bool,
        effective: bool,
    },
    Unreadable(String),
}

impl FileCaps {
    fn as_json(&self) -> Value {
        match self {
            FileCaps::Read {
                bpf,
                perfmon,
                net_raw,
                effective,
            } => json!({
                "cap_bpf": bpf,
                "cap_perfmon": perfmon,
                "cap_net_raw": net_raw,
                "effective": effective,
            }),
            FileCaps::Unreadable(reason) => json!({"unknown": reason}),
        }
    }
}

const CAP_NET_RAW: u32 = 13;
const CAP_PERFMON: u32 = 38;
const CAP_BPF: u32 = 39;
const VFS_CAP_FLAGS_EFFECTIVE: u32 = 0x1;

/// Reads the `security.capability` attribute: a `vfs_cap_data`, whose first
/// word is the revision and the effective flag, followed by (permitted,
/// inheritable) pairs for capabilities 0-31 and 32-63.
pub fn file_capabilities(path: &Path) -> FileCaps {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return FileCaps::Unreadable("the path holds a NUL byte".into());
    };
    let name = c"security.capability";
    let mut buffer = [0u8; 64];
    // SAFETY: both strings are NUL-terminated and the buffer's length is
    // passed with it.
    let length = unsafe {
        libc::getxattr(
            c_path.as_ptr(),
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
        )
    };
    if length < 0 {
        let error = std::io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ENODATA) => FileCaps::Read {
                bpf: false,
                perfmon: false,
                net_raw: false,
                effective: false,
            },
            _ => FileCaps::Unreadable(error.to_string()),
        };
    }
    let word = |index: usize| -> u32 {
        let at = index * 4;
        buffer
            .get(at..at + 4)
            .map_or(0, |bytes| u32::from_le_bytes(bytes.try_into().unwrap()))
    };
    let length = length as usize;
    if length < 12 {
        return FileCaps::Unreadable(format!("a {length}-byte capability attribute"));
    }
    let permitted_low = word(1);
    let permitted_high = if length >= 20 { word(3) } else { 0 };
    let has = |cap: u32| {
        if cap < 32 {
            permitted_low & (1 << cap) != 0
        } else {
            permitted_high & (1 << (cap - 32)) != 0
        }
    };
    FileCaps::Read {
        bpf: has(CAP_BPF),
        perfmon: has(CAP_PERFMON),
        net_raw: has(CAP_NET_RAW),
        effective: word(0) & VFS_CAP_FLAGS_EFFECTIVE != 0,
    }
}

/// Reads `omn list`'s table, strictly.
///
/// The table is text because `omn list --format json` does not carry the
/// sweep (filed for the stream that owns `src/main.rs`). Text is read here
/// only under checks that make a drift loud rather than wrong: the header
/// names the columns, every row is a catalog source in catalog order, every
/// HERE is one of the three markers, and the tallies match the footer the
/// binary printed. Anything else is refused, never guessed.
pub fn parse_sweep(text: &str, catalog: &Catalog) -> Result<Value, String> {
    let mut lines = text.lines();
    let header = lines.next().ok_or("`omn list` printed nothing")?;
    let column = |name: &str| {
        header
            .find(name)
            .ok_or_else(|| format!("`omn list`'s header has no {name} column: `{header}`"))
    };
    let (here_at, collector_at, summary_at) =
        (column("HERE")?, column("COLLECTOR")?, column("SUMMARY")?);
    if !header.starts_with("SOURCE") || !(here_at < collector_at && collector_at < summary_at) {
        return Err(format!(
            "`omn list`'s header is not the one this server reads: `{header}`"
        ));
    }

    let mut rows = Vec::new();
    let mut tally = (0usize, 0usize, 0usize);
    let expected: Vec<&str> = catalog.ids().collect();
    for line in lines.by_ref() {
        if line.trim().is_empty() {
            break;
        }
        let id = line.split_whitespace().next().unwrap_or_default();
        let field = |from: usize, to: usize| {
            line.get(from..to)
                .map(str::trim)
                .ok_or_else(|| format!("row `{line}` is shorter than the header"))
        };
        let here = field(here_at, collector_at)?;
        let collector = field(collector_at, summary_at)?;
        let (state, meaning) = match here {
            "yes" => {
                tally.0 += 1;
                (
                    "available",
                    "a backend whose every requirement was checked and found",
                )
            }
            "?" => {
                tally.1 += 1;
                (
                    "unverified",
                    "the best reachable backend has a requirement no probe covers yet: not the \
                     same as available, and never counted as it",
                )
            }
            "no" => {
                tally.2 += 1;
                (
                    "unavailable",
                    "every backend is missing something on this machine",
                )
            }
            other => return Err(format!("`{id}` has HERE `{other}`, not yes, ? or no")),
        };
        rows.push(json!({
            "source": id,
            "here": state,
            "here_means": meaning,
            "collector": collector,
        }));
    }
    let ids: Vec<&str> = rows
        .iter()
        .map(|row| row["source"].as_str().unwrap_or_default())
        .collect();
    if ids != expected {
        return Err(format!(
            "`omn list` names {} sources and this server's catalog {}; the binary and the \
             server were not built from the same catalog",
            ids.len(),
            expected.len()
        ));
    }

    let footer: Vec<&str> = lines.filter(|line| !line.trim().is_empty()).collect();
    let counts = footer
        .first()
        .map(|line| {
            line.split(|c: char| !c.is_ascii_digit())
                .filter(|part| !part.is_empty())
                .filter_map(|part| part.parse::<usize>().ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if counts.get(0..4) != Some(&[rows.len(), tally.0, tally.1, tally.2][..]) {
        return Err(format!(
            "`omn list`'s footer `{}` does not match its own rows ({} sources: {} yes, {} ?, {} \
             no)",
            footer.first().unwrap_or(&""),
            rows.len(),
            tally.0,
            tally.1,
            tally.2
        ));
    }
    Ok(json!({
        "sources": rows,
        "totals": {
            "sources": rows.len(),
            "available": tally.0,
            "unverified": tally.1,
            "unavailable": tally.2,
        },
        "said_by_omn": footer,
    }))
}
