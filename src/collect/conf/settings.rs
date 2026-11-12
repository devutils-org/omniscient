//! The table of settings the `conf` family reads (`catalog/settings.yaml`),
//! and how one row is read.
//!
//! Every read here is a read: a file opened read-only, or a system call that
//! asks. Nothing in this module writes a setting, and the advice tests hold
//! that (`docs/DECISIONS.md`, boundary 5).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::catalog::Catalog;

const TABLE: &str = include_str!("../../../catalog/settings.yaml");

/// Who a setting belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// The whole machine: a sysctl, a sysfs file, the command line.
    Machine,
    /// One process.
    Process,
    /// Each thread of one process.
    Thread,
    /// The process's cgroup, and its ancestors.
    Cgroup,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Machine => "machine",
            Scope::Process => "process",
            Scope::Thread => "thread",
            Scope::Cgroup => "cgroup",
        }
    }

    /// Whether the setting is the target's rather than the machine's.
    pub fn is_the_targets(self) -> bool {
        !matches!(self, Scope::Machine)
    }
}

/// One row of the table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Setting {
    pub id: String,
    pub scope: Scope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgroup: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmdline: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kconfig: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub syscall: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(default, rename = "as", skip_serializing_if = "Option::is_none")]
    pub read_as: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub per_thread: bool,
    /// A value the kernel computes rather than one anybody set (`oom_score`):
    /// read beside the settings it derives from, and never reported as a
    /// change, since it moves with what the process holds.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub computed: bool,
    pub mechanism: String,
    pub docs: String,
    pub bears_on: Vec<String>,
}

/// How a row is read: exactly one of these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reader<'a> {
    File(&'a str),
    Proc(&'a str),
    Cgroup(&'a str),
    Cmdline(&'a str),
    Kconfig(&'a str),
    Syscall(&'a str),
}

impl Setting {
    pub fn reader(&self) -> Result<Reader<'_>, String> {
        let mut found = Vec::new();
        if let Some(path) = &self.file {
            found.push(Reader::File(path));
        }
        if let Some(path) = &self.proc {
            found.push(Reader::Proc(path));
        }
        if let Some(file) = &self.cgroup {
            found.push(Reader::Cgroup(file));
        }
        if let Some(name) = &self.cmdline {
            found.push(Reader::Cmdline(name));
        }
        if let Some(name) = &self.kconfig {
            found.push(Reader::Kconfig(name));
        }
        if let Some(name) = &self.syscall {
            found.push(Reader::Syscall(name));
        }
        match found.len() {
            1 => Ok(found.remove(0)),
            0 => Err(format!("setting `{}` names no reader", self.id)),
            _ => Err(format!("setting `{}` names more than one reader", self.id)),
        }
    }
}

/// The table, as the binary carries it.
pub fn table() -> &'static [Setting] {
    static TABLE_PARSED: OnceLock<Vec<Setting>> = OnceLock::new();
    TABLE_PARSED.get_or_init(|| load(TABLE).expect("the compiled-in settings table loads"))
}

/// One row by id.
pub fn find(id: &str) -> Option<&'static Setting> {
    table().iter().find(|setting| setting.id == id)
}

/// Parses a table and holds it to its rules, except the catalog's: those are
/// [`check_against`].
pub fn load(text: &str) -> Result<Vec<Setting>, String> {
    let rows: Vec<Setting> = serde_norway::from_str(text).map_err(|error| error.to_string())?;
    let mut ids = BTreeSet::new();
    for row in &rows {
        if !ids.insert(row.id.as_str()) {
            return Err(format!("setting `{}` is declared twice", row.id));
        }
        let reader = row.reader()?;
        match (&reader, row.scope) {
            (Reader::File(_) | Reader::Cmdline(_) | Reader::Kconfig(_), Scope::Machine) => {}
            (Reader::Proc(_) | Reader::Syscall(_), Scope::Process | Scope::Thread) => {}
            (Reader::Cgroup(_), Scope::Cgroup) => {}
            (reader, scope) => {
                return Err(format!(
                    "setting `{}` is read by {reader:?}, which cannot read a {} setting",
                    row.id,
                    scope.as_str()
                ))
            }
        }
        if row.per_thread != (row.scope == Scope::Thread) {
            return Err(format!(
                "setting `{}`: `per_thread` is what makes a setting a thread's, and the two \
                 disagree",
                row.id
            ));
        }
        if let Reader::File(path) = reader {
            if !path.starts_with('/') {
                return Err(format!("setting `{}`: `{path}` is not absolute", row.id));
            }
            if path.matches('*').count() > 1 {
                return Err(format!(
                    "setting `{}`: `{path}` has more than one `*`",
                    row.id
                ));
            }
        }
        if let Some(how) = &row.read_as {
            let known = matches!(
                how.as_str(),
                "bracket" | "cpu_count" | "sched_policy" | "kb"
            ) || how
                .strip_prefix("vmflag:")
                .is_some_and(|flag| flag.len() == 2);
            if !known {
                return Err(format!("setting `{}`: unknown `as: {how}`", row.id));
            }
        }
        if let Reader::Syscall(name) = reader {
            if name != "ioprio" {
                return Err(format!("setting `{}`: unknown syscall `{name}`", row.id));
            }
        }
        if row.bears_on.is_empty() {
            return Err(format!(
                "setting `{}` bears on no source: a setting no reading shows the effect of is \
                 not one this table carries",
                row.id
            ));
        }
    }
    Ok(rows)
}

/// Holds the table to the catalog: every source a row bears on exists.
pub fn check_against(rows: &[Setting], catalog: &Catalog) -> Result<(), String> {
    for row in rows {
        for source in &row.bears_on {
            let known = source
                .parse()
                .ok()
                .is_some_and(|id| catalog.get(&id).is_some());
            if !known {
                return Err(format!(
                    "setting `{}` bears on `{source}`, which the catalog does not have",
                    row.id
                ));
            }
        }
    }
    Ok(())
}

/// What a read found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Read, with a value.
    Set,
    /// Not given: the kernel's built-in default applies (a command-line
    /// parameter nobody passed).
    Unset,
    /// This kernel does not have it.
    Absent,
    /// It could not be read here: permission, a mount, a parse.
    Unknown,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Set => "set",
            State::Unset => "unset",
            State::Absent => "absent",
            State::Unknown => "unknown",
        }
    }
}

/// One setting, read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reading {
    pub setting: String,
    pub scope: Scope,
    /// Which of several: a cpufreq policy, a device, an irq, a thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    pub state: State,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// For a cgroup setting, the level whose value binds: the target's own
    /// cgroup, or the ancestor whose limit is lower.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<String>,
    /// For a cgroup setting, the value at the binding level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding_value: Option<String>,
    /// For a resource limit, the hard limit beside the soft one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hard: Option<String>,
    pub read_from: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Reading {
    fn new(setting: &Setting, read_from: impl Into<String>) -> Self {
        Self {
            setting: setting.id.clone(),
            scope: setting.scope,
            instance: None,
            state: State::Unknown,
            value: None,
            binding: None,
            binding_value: None,
            hard: None,
            read_from: read_from.into(),
            reason: None,
        }
    }

    fn set(mut self, value: impl Into<String>) -> Self {
        self.state = State::Set;
        self.value = Some(value.into());
        self
    }

    fn failed(mut self, state: State, reason: impl Into<String>) -> Self {
        self.state = state;
        self.reason = Some(reason.into());
        self
    }

    /// The value as a number, where it is one: `max` is no number, and a
    /// limit that reads `unlimited` neither.
    pub fn number(&self) -> Option<f64> {
        let value = self.binding_value.as_ref().or(self.value.as_ref())?;
        value.trim().parse::<f64>().ok()
    }

    /// The value that applies: the binding level's for a cgroup setting.
    pub fn effective(&self) -> Option<&str> {
        self.binding_value.as_deref().or(self.value.as_deref())
    }

    /// Identity across two reads of one run.
    pub fn key(&self) -> (String, Option<String>) {
        (self.setting.clone(), self.instance.clone())
    }
}

/// Why a file could not be read, said as the state it leaves.
fn read_failure(reading: Reading, path: &Path, error: &std::io::Error) -> Reading {
    match error.kind() {
        std::io::ErrorKind::NotFound => reading.failed(
            State::Absent,
            format!("{} does not exist here", path.display()),
        ),
        std::io::ErrorKind::PermissionDenied => reading.failed(
            State::Unknown,
            format!("{} is not readable by this user", path.display()),
        ),
        _ => reading.failed(State::Unknown, format!("{}: {error}", path.display())),
    }
}

/// Applies `as:` to the text a file gave.
fn interpret(setting: &Setting, text: &str) -> Result<String, String> {
    let text = text.trim();
    match setting.read_as.as_deref() {
        None => Ok(text.to_owned()),
        Some("bracket") => match (text.find('['), text.find(']')) {
            (Some(open), Some(close)) if close > open => Ok(text[open + 1..close].to_owned()),
            // A file with one choice has no brackets: `none` alone.
            _ if !text.contains(' ') => Ok(text.to_owned()),
            _ => Err(format!("`{text}` names no choice in brackets")),
        },
        Some("cpu_count") => cpu_count(text)
            .map(|count| count.to_string())
            .ok_or_else(|| format!("`{text}` is not a cpu list")),
        Some("kb") => text
            .strip_suffix("kB")
            .unwrap_or(text)
            .trim()
            .parse::<u64>()
            .map(|kib| kib.to_string())
            .map_err(|_| format!("`{text}` is not a size in kB")),
        Some("sched_policy") => text
            .parse::<u32>()
            .map(|policy| policy_name(policy).to_owned())
            .map_err(|_| format!("`{text}` is not a policy number")),
        Some(other) => Err(format!("`as: {other}` does not apply to this text")),
    }
}

/// Counts the cpus of a list like `0-3,8,10-11`.
pub fn cpu_count(list: &str) -> Option<usize> {
    let list = list.trim();
    if list.is_empty() {
        return Some(0);
    }
    let mut count = 0;
    for part in list.split(',') {
        match part.split_once('-') {
            Some((low, high)) => {
                let low: usize = low.trim().parse().ok()?;
                let high: usize = high.trim().parse().ok()?;
                if high < low {
                    return None;
                }
                count += high - low + 1;
            }
            None => {
                part.trim().parse::<usize>().ok()?;
                count += 1;
            }
        }
    }
    Some(count)
}

fn policy_name(policy: u32) -> &'static str {
    match policy {
        0 => "SCHED_OTHER",
        1 => "SCHED_FIFO",
        2 => "SCHED_RR",
        3 => "SCHED_BATCH",
        5 => "SCHED_IDLE",
        6 => "SCHED_DEADLINE",
        7 => "SCHED_EXT",
        _ => "unknown",
    }
}

/// Reads one file into one reading.
fn read_file(setting: &Setting, path: &Path, instance: Option<String>) -> Reading {
    let mut reading = Reading::new(setting, path.display().to_string());
    reading.instance = instance;
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let text = match &setting.field {
                None => Some(text),
                Some(name) => text.lines().find_map(|line| {
                    line.strip_prefix(name.as_str())
                        .and_then(|rest| rest.strip_prefix(':'))
                        .map(|value| value.trim().to_owned())
                }),
            };
            match text.map(|text| interpret(setting, &text)) {
                Some(Ok(value)) => reading.set(value),
                Some(Err(reason)) => reading.failed(State::Unknown, reason),
                None => reading.failed(
                    State::Absent,
                    format!(
                        "{} has no `{}` on this kernel",
                        path.display(),
                        setting.field.as_deref().unwrap_or_default()
                    ),
                ),
            }
        }
        Err(error) => read_failure(reading, path, &error),
    }
}

/// Expands the one `*` of a path into every match, each with what the `*`
/// stood for.
/// The paths one pattern names, each with its instance; or the directory
/// that could not be listed, and why.
type Expanded = Result<Vec<(PathBuf, Option<String>)>, (PathBuf, std::io::Error)>;

fn expand(path: &str) -> Expanded {
    let Some(star) = path.find('*') else {
        return Ok(vec![(PathBuf::from(path), None)]);
    };
    let directory_end = path[..star].rfind('/').unwrap_or(0);
    let directory = &path[..directory_end.max(1)];
    let component_end = path[star..].find('/').map_or(path.len(), |end| star + end);
    let prefix = &path[directory_end + 1..star];
    let suffix = &path[star + 1..component_end];
    let rest = &path[component_end..];
    let entries =
        std::fs::read_dir(directory).map_err(|error| (PathBuf::from(directory), error))?;
    let mut found: Vec<(PathBuf, Option<String>)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let middle = name.strip_prefix(prefix)?.strip_suffix(suffix)?.to_owned();
            let full = PathBuf::from(format!("{directory}/{name}{rest}"));
            full.exists().then_some((full, Some(middle)))
        })
        .collect();
    found.sort_by_key(|one| natural(&one.1));
    Ok(found)
}

/// Orders `policy2` before `policy10`, and `irq 9` before `irq 10`.
fn natural(name: &Option<String>) -> (u64, String) {
    let name = name.clone().unwrap_or_default();
    let digits: String = name.chars().filter(char::is_ascii_digit).collect();
    (digits.parse().unwrap_or(u64::MAX), name)
}

/// Reads a machine setting.
fn read_machine_setting(
    setting: &Setting,
    cmdline: &Option<String>,
    kconfig: &Kconfig,
) -> Vec<Reading> {
    match setting.reader() {
        Ok(Reader::File(path)) => match expand(path) {
            Ok(paths) if paths.is_empty() => vec![Reading::new(setting, path)
                .failed(State::Absent, format!("nothing matches {path} here"))],
            Ok(paths) => paths
                .into_iter()
                .map(|(full, instance)| read_file(setting, &full, instance))
                .collect(),
            Err((directory, error)) => vec![read_failure(
                Reading::new(setting, path),
                &directory,
                &error,
            )],
        },
        Ok(Reader::Cmdline(name)) => {
            let reading = Reading::new(setting, "/proc/cmdline");
            vec![match cmdline {
                None => reading.failed(State::Unknown, "/proc/cmdline could not be read"),
                Some(line) => match parameter(line, name) {
                    Some(value) => reading.set(value),
                    None => {
                        let mut reading = reading;
                        reading.state = State::Unset;
                        reading.reason = Some(format!(
                            "`{name}` is not on the command line: the built-in default applies"
                        ));
                        reading
                    }
                },
            }]
        }
        Ok(Reader::Kconfig(option)) => {
            let reading = Reading::new(setting, kconfig.from.clone());
            vec![match &kconfig.text {
                None => reading.failed(
                    State::Unknown,
                    "neither /proc/config.gz nor /boot/config-<release> could be read",
                ),
                Some(text) => match kconfig_value(text, option) {
                    Some(value) => reading.set(value),
                    None => reading.set("n"),
                },
            }]
        }
        Ok(other) => vec![Reading::new(setting, "").failed(
            State::Unknown,
            format!("{other:?} does not read a machine setting"),
        )],
        Err(reason) => vec![Reading::new(setting, "").failed(State::Unknown, reason)],
    }
}

/// The value of `name=` on a command line, or the bare word.
pub fn parameter(line: &str, name: &str) -> Option<String> {
    line.split_whitespace().rev().find_map(|word| {
        if word == name {
            return Some(String::new());
        }
        word.strip_prefix(name)
            .and_then(|rest| rest.strip_prefix('='))
            .map(str::to_owned)
    })
}

/// `CONFIG_X=y`, `CONFIG_X=m`, a value, or `# CONFIG_X is not set` (`n`).
fn kconfig_value(text: &str, option: &str) -> Option<String> {
    for line in text.lines() {
        if let Some(rest) = line
            .strip_prefix(option)
            .and_then(|rest| rest.strip_prefix('='))
        {
            return Some(rest.trim_matches('"').to_owned());
        }
        if line.trim() == format!("# {option} is not set") {
            return Some("n".to_owned());
        }
    }
    None
}

/// The kernel's build configuration, where one can be read.
struct Kconfig {
    text: Option<String>,
    from: String,
}

impl Kconfig {
    fn read() -> Self {
        // The boot file first: it is plain text. `/proc/config.gz` is one
        // gzip member, inflated here rather than through `zcat`, which is
        // not on every machine and is no business of a measurement tool.
        let release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|text| text.trim().to_owned())
            .unwrap_or_default();
        let boot = format!("/boot/config-{release}");
        if let Ok(text) = std::fs::read_to_string(&boot) {
            return Self {
                text: Some(text),
                from: boot,
            };
        }
        if let Some(text) = std::fs::read("/proc/config.gz")
            .ok()
            .and_then(|bytes| gunzip(&bytes))
            .and_then(|bytes| String::from_utf8(bytes).ok())
        {
            return Self {
                text: Some(text),
                from: "/proc/config.gz".to_owned(),
            };
        }
        Self {
            text: None,
            from: format!("/proc/config.gz, {boot}"),
        }
    }
}

/// The contents of a gzip member (RFC 1952): the header skipped by its
/// flags, the deflate stream inflated, and the trailer's length checked. `None`
/// for anything that is not one well-formed member.
fn gunzip(bytes: &[u8]) -> Option<Vec<u8>> {
    const FHCRC: u8 = 0x02;
    const FEXTRA: u8 = 0x04;
    const FNAME: u8 = 0x08;
    const FCOMMENT: u8 = 0x10;
    if bytes.len() < 18 || bytes[0..3] != [0x1f, 0x8b, 8] {
        return None;
    }
    let flags = bytes[3];
    let mut at = 10;
    if flags & FEXTRA != 0 {
        let extra = usize::from(u16::from_le_bytes([*bytes.get(at)?, *bytes.get(at + 1)?]));
        at += 2 + extra;
    }
    for flag in [FNAME, FCOMMENT] {
        if flags & flag != 0 {
            at += bytes.get(at..)?.iter().position(|&byte| byte == 0)? + 1;
        }
    }
    if flags & FHCRC != 0 {
        at += 2;
    }
    let body = bytes.get(at..bytes.len().checked_sub(8)?)?;
    let inflated = miniz_oxide::inflate::decompress_to_vec(body).ok()?;
    let trailer = &bytes[bytes.len() - 4..];
    let size = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
    // ISIZE is the length modulo 2^32.
    (size == inflated.len() as u32).then_some(inflated)
}

/// Every machine setting of the table.
pub fn read_machine() -> Vec<Reading> {
    // The command line and the build cannot change while the kernel runs:
    // read once per process, not once per poll.
    static CMDLINE: OnceLock<Option<String>> = OnceLock::new();
    static KCONFIG: OnceLock<Kconfig> = OnceLock::new();
    let cmdline = CMDLINE.get_or_init(|| std::fs::read_to_string("/proc/cmdline").ok());
    let kconfig = KCONFIG.get_or_init(Kconfig::read);
    table()
        .iter()
        .filter(|setting| setting.scope == Scope::Machine)
        .flat_map(|setting| read_machine_setting(setting, cmdline, kconfig))
        .collect()
}

/// Whether a setting is one the kernel computes ([`Setting::computed`]).
pub fn is_computed(id: &str) -> bool {
    find(id).is_some_and(|setting| setting.computed)
}

/// The most threads whose settings are read.
pub const THREADS_READ: usize = 64;

/// Every setting of one process, its threads and its cgroup.
///
/// Answers the readings, and the number of threads left unread beyond
/// [`THREADS_READ`].
pub fn read_process(pid: i32) -> (Vec<Reading>, usize) {
    let base = PathBuf::from(format!("/proc/{pid}"));
    let threads = threads_of(pid);
    let unread = threads.len().saturating_sub(THREADS_READ);
    let cgroup = cgroup_of(pid);
    let mut readings = Vec::new();
    for setting in table() {
        match (setting.scope, setting.reader()) {
            (Scope::Process, Ok(Reader::Proc(file))) => {
                readings.push(read_proc(setting, &base.join(file), None));
            }
            (Scope::Thread, Ok(Reader::Proc(file))) => {
                for tid in threads.iter().take(THREADS_READ) {
                    let path = base.join(format!("task/{tid}/{file}"));
                    readings.push(read_proc(setting, &path, Some(tid.to_string())));
                }
            }
            (Scope::Process, Ok(Reader::Syscall("ioprio"))) => {
                readings.push(read_ioprio(setting, pid));
            }
            (Scope::Cgroup, Ok(Reader::Cgroup(file))) => {
                readings.push(read_cgroup(setting, &cgroup, file));
            }
            _ => {}
        }
    }
    (readings, unread)
}

fn threads_of(pid: i32) -> Vec<i32> {
    let mut tids: Vec<i32> = std::fs::read_dir(format!("/proc/{pid}/task"))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
                .collect()
        })
        .unwrap_or_default();
    tids.sort_unstable();
    tids
}

/// Reads a `/proc/<pid>` setting: a whole file, a `Name:` line, a limits
/// line, a field of `stat`, or a count of smaps' flagged mappings.
fn read_proc(setting: &Setting, path: &Path, instance: Option<String>) -> Reading {
    let mut reading = Reading::new(setting, path.display().to_string());
    reading.instance = instance;
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => return read_failure(reading, path, &error),
    };
    if let Some(flag) = setting
        .read_as
        .as_deref()
        .and_then(|how| how.strip_prefix("vmflag:"))
    {
        let count = text
            .lines()
            .filter_map(|line| line.strip_prefix("VmFlags:"))
            .filter(|flags| flags.split_whitespace().any(|one| one == flag))
            .count();
        return reading.set(count.to_string());
    }
    let file = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let raw = match (file, setting.field.as_deref()) {
        (_, None) => Some(text.trim().to_owned()),
        ("stat", Some(field)) => field
            .parse::<usize>()
            .ok()
            .and_then(|index| stat_field(&text, index)),
        ("limits", Some(name)) => match limit_line(&text, name) {
            Some((soft, hard)) => {
                reading.hard = Some(hard);
                Some(soft)
            }
            None => None,
        },
        (_, Some(name)) => text.lines().find_map(|line| {
            line.strip_prefix(name)
                .and_then(|rest| rest.strip_prefix(':'))
                .map(|value| value.trim().to_owned())
        }),
    };
    match raw {
        None => {
            let field = setting.field.clone().unwrap_or_default();
            reading.failed(
                State::Absent,
                format!("{} has no `{field}` on this kernel", path.display()),
            )
        }
        Some(raw) => match interpret(setting, &raw) {
            Ok(value) => reading.set(value),
            Err(reason) => reading.failed(State::Unknown, reason),
        },
    }
}

/// The 1-based field of `/proc/<pid>/stat`, counting the comm (which may hold
/// spaces and parentheses) as the second.
pub fn stat_field(text: &str, index: usize) -> Option<String> {
    let close = text.rfind(')')?;
    match index {
        0 => None,
        1 => text.split_whitespace().next().map(str::to_owned),
        2 => Some(text[text.find('(')? + 1..close].to_owned()),
        n => text[close + 1..]
            .split_whitespace()
            .nth(n - 3)
            .map(str::to_owned),
    }
}

/// The soft and hard limits of one line of `/proc/<pid>/limits`.
fn limit_line(text: &str, name: &str) -> Option<(String, String)> {
    let line = text.lines().find(|line| line.starts_with(name))?;
    let mut words = line[name.len()..].split_whitespace();
    Some((words.next()?.to_owned(), words.next()?.to_owned()))
}

fn read_ioprio(setting: &Setting, pid: i32) -> Reading {
    const IOPRIO_WHO_PROCESS: libc::c_long = 1;
    let reading = Reading::new(setting, "ioprio_get(IOPRIO_WHO_PROCESS)");
    // SAFETY: ioprio_get takes two integers and returns one; it reads
    // nothing from this process's memory.
    let value = unsafe {
        libc::syscall(
            libc::SYS_ioprio_get,
            IOPRIO_WHO_PROCESS,
            libc::c_long::from(pid),
        )
    };
    if value < 0 {
        let error = std::io::Error::last_os_error();
        return reading.failed(State::Unknown, format!("ioprio_get: {error}"));
    }
    let class = (value >> 13) & 0x7;
    let level = value & 0x1fff;
    reading.set(match class {
        0 => "none (from nice)".to_owned(),
        1 => format!("realtime/{level}"),
        2 => format!("best-effort/{level}"),
        3 => "idle".to_owned(),
        other => format!("class {other}/{level}"),
    })
}

/// The cgroup v2 directory a process is in, or why there is none.
fn cgroup_of(pid: i32) -> Result<PathBuf, String> {
    let path = format!("/proc/{pid}/cgroup");
    let text = std::fs::read_to_string(&path).map_err(|error| format!("{path}: {error}"))?;
    let relative = text
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or_else(|| format!("{path} names no cgroup v2 hierarchy"))?;
    Ok(PathBuf::from("/sys/fs/cgroup").join(relative.trim().trim_start_matches('/')))
}

/// Reads a cgroup file at the target's cgroup and every ancestor below the
/// root, and names the level whose value binds: the lowest number, where
/// `max` is none.
fn read_cgroup(setting: &Setting, cgroup: &Result<PathBuf, String>, file: &str) -> Reading {
    let directory = match cgroup {
        Ok(directory) => directory,
        Err(reason) => {
            return Reading::new(setting, file.to_owned()).failed(State::Unknown, reason.clone())
        }
    };
    let own = directory.join(file);
    let mut reading = read_file(setting, &own, None);
    if reading.state != State::Set {
        if directory == Path::new("/sys/fs/cgroup") {
            reading.reason = Some(
                "the process is in the root cgroup, which has no limits of its own".to_owned(),
            );
        }
        return reading;
    }
    let mut binding: Option<(PathBuf, String, f64)> = reading
        .value
        .as_deref()
        .and_then(limit_number)
        .map(|number| {
            (
                directory.clone(),
                reading.value.clone().unwrap_or_default(),
                number,
            )
        });
    let mut level = directory.parent();
    while let Some(parent) = level {
        if parent == Path::new("/sys/fs/cgroup") || !parent.starts_with("/sys/fs/cgroup") {
            break;
        }
        if let Ok(text) = std::fs::read_to_string(parent.join(file)) {
            let value = text.trim().to_owned();
            if let Some(number) = limit_number(&value) {
                if binding.as_ref().is_none_or(|(_, _, low)| number < *low) {
                    binding = Some((parent.to_path_buf(), value, number));
                }
            }
        }
        level = parent.parent();
    }
    if let Some((path, value, _)) = binding {
        if path != *directory {
            reading.binding = Some(path.display().to_string());
            reading.binding_value = Some(value);
        }
    }
    reading
}

/// A limit as a number: a single number, or the first of `quota period`
/// (`cpu.max`); `max` is no limit.
fn limit_number(value: &str) -> Option<f64> {
    let first = value.split_whitespace().next()?;
    if first == "max" {
        return None;
    }
    first.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One member as Python's `gzip` writes it, with a file name in the
    /// header: the name is skipped, the text comes back whole, and a member cut
    /// short or with a wrong length is refused rather than read in part.
    #[test]
    fn a_gzip_member_is_inflated_and_a_damaged_one_refused() {
        const MEMBER: &[u8] = &[
            0x1f, 0x8b, 0x08, 0x08, 0x00, 0x00, 0x00, 0x00, 0x02, 0xff, 0x63, 0x6f, 0x6e, 0x66,
            0x69, 0x67, 0x00, 0x73, 0xf6, 0xf7, 0x73, 0xf3, 0x74, 0x8f, 0x77, 0x0a, 0x70, 0xb3,
            0xad, 0xe4, 0x52, 0x56, 0x70, 0x86, 0x70, 0xbd, 0x1d, 0x83, 0x1d, 0xfd, 0x14, 0x32,
            0x8b, 0x15, 0xf2, 0xf2, 0x4b, 0x14, 0x8a, 0x53, 0x4b, 0xb8, 0x9c, 0xa9, 0xaa, 0x0c,
            0x00, 0xba, 0x91, 0x76, 0x1a, 0x75, 0x00, 0x00, 0x00,
        ];
        let text = String::from_utf8(gunzip(MEMBER).expect("a member")).expect("text");
        assert_eq!(text, "CONFIG_BPF=y\n# CONFIG_KASAN is not set\n".repeat(3));
        assert_eq!(gunzip(&MEMBER[..MEMBER.len() - 10]), None);
        let mut wrong = MEMBER.to_vec();
        *wrong.last_mut().unwrap() = 1;
        assert_eq!(gunzip(&wrong), None);
        assert_eq!(gunzip(b"CONFIG_BPF=y\n"), None);
    }

    /// Where the running kernel serves `/proc/config.gz`, the inflater reads
    /// what `zcat` reads -- `zcat` here only as an independent second reader.
    #[test]
    fn proc_config_gz_reads_as_zcat_reads_it() {
        let Ok(bytes) = std::fs::read("/proc/config.gz") else {
            eprintln!("skipped: this kernel serves no /proc/config.gz");
            return;
        };
        let Ok(zcat) = std::process::Command::new("zcat")
            .arg("/proc/config.gz")
            .output()
        else {
            eprintln!("skipped: no zcat to compare with");
            return;
        };
        assert_eq!(gunzip(&bytes).expect("the kernel's member"), zcat.stdout);
    }

    #[test]
    fn the_table_loads_and_every_source_it_names_is_in_the_catalog() {
        let rows = table();
        assert!(rows.len() >= 60, "{}", rows.len());
        check_against(rows, &Catalog::embedded().expect("the catalog")).expect("the table");
    }

    #[test]
    fn a_row_with_two_readers_or_a_reader_its_scope_cannot_use_is_refused() {
        let two = "- {id: x, scope: machine, file: /a, cmdline: b, mechanism: m, docs: d, bears_on: [mem.oom]}";
        assert!(load(two).unwrap_err().contains("more than one"));
        let wrong =
            "- {id: x, scope: process, file: /a, mechanism: m, docs: d, bears_on: [mem.oom]}";
        assert!(load(wrong).unwrap_err().contains("cannot read"));
        let none = "- {id: x, scope: machine, file: /a, mechanism: m, docs: d, bears_on: []}";
        assert!(load(none).unwrap_err().contains("bears on no source"));
    }

    #[test]
    fn the_readers_parse_what_the_kernel_writes() {
        assert_eq!(cpu_count("0-3,8,10-11"), Some(7));
        assert_eq!(cpu_count("0"), Some(1));
        assert_eq!(cpu_count("3-1"), None);
        assert_eq!(
            parameter("BOOT_IMAGE=/v quiet mitigations=off", "mitigations").as_deref(),
            Some("off")
        );
        assert_eq!(parameter("quiet", "mitigations"), None);
        assert_eq!(parameter("ro quiet", "quiet").as_deref(), Some(""));
        let stat = "42 (a b) c) S 1 42 42 0 -1 4194304 0 0 0 0 0 0 0 0 20 0 1";
        assert_eq!(stat_field(stat, 2).as_deref(), Some("a b) c"));
        assert_eq!(stat_field(stat, 3).as_deref(), Some("S"));
        assert_eq!(stat_field(stat, 19).as_deref(), Some("0"));
        assert_eq!(
            kconfig_value("CONFIG_PSI=y\n# CONFIG_X is not set\n", "CONFIG_X").as_deref(),
            Some("n")
        );
        assert_eq!(limit_number("max 100000"), None);
        assert_eq!(limit_number("50000 100000"), Some(50000.0));
        let thp = find("mm.thp.enabled").unwrap();
        assert_eq!(
            interpret(thp, "always [madvise] never\n").unwrap(),
            "madvise"
        );
    }

    #[test]
    fn this_process_reads_its_own_settings_and_no_setting_is_answered_with_a_guess() {
        let (readings, unread) = read_process(std::process::id() as i32);
        assert_eq!(unread, 0);
        let adj = readings
            .iter()
            .find(|reading| reading.setting == "process.oom_score_adj")
            .unwrap();
        let expected = std::fs::read_to_string("/proc/self/oom_score_adj").unwrap();
        assert_eq!(adj.value.as_deref(), Some(expected.trim()));
        for reading in &readings {
            match reading.state {
                State::Set => assert!(reading.value.is_some(), "{reading:?}"),
                _ => assert!(
                    reading.value.is_none() && reading.reason.is_some(),
                    "{reading:?}"
                ),
            }
        }
        for reading in read_machine() {
            match reading.state {
                State::Set => assert!(reading.value.is_some(), "{reading:?}"),
                _ => assert!(
                    reading.value.is_none() && reading.reason.is_some(),
                    "{reading:?}"
                ),
            }
        }
    }
}
