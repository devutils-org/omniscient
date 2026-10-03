//! Every way to unwind a stack, chosen the way backends are.
//!
//! A profile's stacks are only as good as the way they were unwound, and there
//! is more than one way: the kernel's frame-pointer walk, that walk with the
//! caller of a frameless leaf recovered from its call frame information, the
//! user stack copied with each sample and unwound here from `.eh_frame`, the
//! cpu's own branch records, and the kernel reading a binary's `.sframe`. They
//! differ in what they cost and in what they lose, and which is best depends
//! on the machine *and* on how the target was built -- which is exactly the
//! shape of a backend chain.
//!
//! So the modes are declared as data (`catalog/unwind.yaml`), each with
//! requirements a probe answers available, missing or unknown, what it copies
//! with every sample, and what it gives up. [`choose`] walks them for `--stack
//! auto`, records each with the resolver's own [`Verdict`], and forces one for
//! `--stack <mode>` -- refused with the reason where a requirement is missing,
//! never degraded to another mode silently.
//!
//! The rest of this module is what the modes need that the kernel does not
//! do: [`Cfi`], the unwinder over a process's `.eh_frame` (through
//! `framehop`), and [`Debuginfod`], names for a stripped binary fetched by
//! build id when, and only when, `--symbols debuginfod` asks for them.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::OnceLock;

use serde::Deserialize;

use super::{read_maps, ConsideredMode, Elf, Region, StackChoice};
use crate::capabilities::{Environment, Probe};
use crate::perf::UserRegs;
use crate::request::{Request, StackMode, Target};
use crate::resolve::Verdict;

/// The modes, as this build was compiled with them.
const EMBEDDED: &str = include_str!("../catalog/unwind.yaml");

/// One unwind mode, as `catalog/unwind.yaml` declares it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModeEntry {
    pub mode: StackMode,
    pub summary: String,
    /// Requirement strings, each answered by [`probe`].
    #[serde(default)]
    pub requires: Vec<String>,
    /// Bytes of user stack the kernel copies with every sample.
    #[serde(default)]
    pub copies: u32,
    /// The same coarse scale a backend's cost is on.
    pub cost: String,
    /// What this mode gives up relative to the modes above it. Required of
    /// every mode but the first, unless it declares `equivalent`.
    #[serde(default)]
    pub degraded: Option<String>,
    #[serde(default)]
    pub equivalent: bool,
    /// Whether `--stack auto` may choose it. `none` is never what `auto` means.
    #[serde(default = "yes")]
    pub auto: bool,
}

fn yes() -> bool {
    true
}

/// Reads a mode table and checks what the chooser relies on: every concrete
/// mode declared once, `auto` never, and every mode after the first saying
/// what it gives up -- the rule a fallback backend is held to.
pub fn load(text: &str) -> Result<Vec<ModeEntry>, String> {
    let entries: Vec<ModeEntry> = serde_norway::from_str(text)
        .map_err(|error| format!("the unwind mode table is invalid: {error}"))?;
    let mut seen = BTreeSet::new();
    for (index, entry) in entries.iter().enumerate() {
        if !entry.mode.is_concrete() {
            return Err(format!(
                "the unwind mode table declares `{}`, which is the chooser and not a mode",
                entry.mode
            ));
        }
        if !seen.insert(entry.mode) {
            return Err(format!(
                "the unwind mode table declares `{}` twice",
                entry.mode
            ));
        }
        if index > 0 && entry.degraded.is_none() && !entry.equivalent {
            return Err(format!(
                "unwind mode `{}` is below `{}` and says neither what it degrades nor that it \
                 is equivalent",
                entry.mode, entries[0].mode
            ));
        }
        if !matches!(entry.cost.as_str(), "free" | "low" | "medium" | "high") {
            return Err(format!(
                "unwind mode `{}` has cost `{}` (expected free, low, medium or high)",
                entry.mode, entry.cost
            ));
        }
    }
    if let Some(missing) = StackMode::CONCRETE.iter().find(|mode| !seen.contains(mode)) {
        return Err(format!(
            "the unwind mode table does not declare `{missing}`"
        ));
    }
    Ok(entries)
}

/// The mode table this process chooses from: the one compiled in, or the one
/// beside a catalog read with `--catalog` ([`use_table_in`]).
static MODES: OnceLock<Vec<ModeEntry>> = OnceLock::new();

/// The modes this process chooses from, best first: the table compiled into
/// this build unless [`use_table_in`] read another first.
pub fn modes() -> &'static [ModeEntry] {
    MODES.get_or_init(|| {
        load(EMBEDDED).unwrap_or_else(|error| panic!("catalog/unwind.yaml is compiled in: {error}"))
    })
}

/// Reads the unwind table of a catalog directory, `<dir>/unwind.yaml`, when it
/// has one, so that a catalog iterated on with `--catalog` carries its modes
/// the way it carries its entries. A directory without the file keeps the
/// compiled-in table: every catalog copied before the table moved there.
///
/// Answers whether the directory's table is the one in use. Refused when the
/// table is invalid -- by the same rules as the compiled-in one -- or when the
/// modes were already chosen from another table in this process.
pub fn use_table_in(dir: &Path) -> Result<bool, String> {
    let path = dir.join(crate::catalog::UNWIND_TABLE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };
    let entries = load(&text).map_err(|error| format!("{}: {error}", path.display()))?;
    match MODES.set(entries) {
        Ok(()) => Ok(true),
        Err(entries) if MODES.get() == Some(&entries) => Ok(true),
        Err(_) => Err(format!(
            "{} was read after the modes had been chosen from another table",
            path.display()
        )),
    }
}

/// One mode's declaration.
pub fn entry(mode: StackMode) -> Option<&'static ModeEntry> {
    modes().iter().find(|entry| entry.mode == mode)
}

/// Whether this build can serve a mode the machine offers.
///
/// `lbr` and `sframe` are declared and probed but not collected: no machine
/// this project runs on offers either (`docs/unwind.md`), and a decoder
/// nothing has ever exercised would be a confident wrong answer the day one
/// does. A machine that offers one reads it as [`Verdict::Unimplemented`],
/// which is what the resolver says of a backend with no collector.
pub fn implemented(mode: StackMode) -> bool {
    matches!(
        mode,
        StackMode::Fp | StackMode::FpLeaf | StackMode::Dwarf | StackMode::None
    )
}

// ---------------------------------------------------------------------------
// The target, as far as it can be read before it runs.

/// The binary a request's target runs, read for what an unwinder needs.
#[derive(Debug, Clone, PartialEq)]
pub struct TargetImage {
    pub path: PathBuf,
    /// Whether the target's own code sets up frame pointers, and how that was
    /// told: `(answer, evidence)`.
    pub frame_pointers: Option<(bool, String)>,
    /// `.eh_frame` or `.debug_frame`, non-empty.
    pub cfi: bool,
    /// A `.sframe` section.
    pub sframe: bool,
}

/// What can be known about the target's binary, or why nothing can.
pub fn target_image(target: &Target) -> Result<TargetImage, String> {
    let path = match target {
        Target::Launch(argv) => {
            let program = argv
                .first()
                .ok_or_else(|| "the launch names no program".to_owned())?;
            resolve_program(program)?
        }
        Target::Pid(pids) | Target::Tid(pids) => {
            let pid = pids
                .first()
                .ok_or_else(|| "the target names no process".to_owned())?;
            std::fs::read_link(format!("/proc/{pid}/exe"))
                .map_err(|error| format!("cannot read which binary process {pid} runs: {error}"))?
        }
        other => {
            return Err(format!(
                "a {} target runs binaries that are not known before the run",
                other.kind()
            ))
        }
    };
    inspect(&path)
}

/// A program name as `execvp` would find it.
fn resolve_program(program: &str) -> Result<PathBuf, String> {
    if program.contains('/') {
        return Ok(PathBuf::from(program));
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|directory| directory.join(program))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| format!("`{program}` is not on PATH"))
}

/// Reads a binary for its unwind information and its frame pointers.
pub fn inspect(path: &Path) -> Result<TargetImage, String> {
    let bytes =
        std::fs::read(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if bytes.starts_with(b"#!") {
        return Err(format!(
            "{} is a script: the program it runs is not known before it runs",
            path.display()
        ));
    }
    let elf = Elf::parse(&bytes)
        .ok_or_else(|| format!("{} is not a 64-bit little-endian ELF", path.display()))?;
    let non_empty = |name: &str| elf.section(name).is_some_and(|section| section.size > 0);
    Ok(TargetImage {
        path: path.to_path_buf(),
        frame_pointers: frame_pointers(&elf, &bytes),
        cfi: non_empty(".eh_frame") || non_empty(".debug_frame"),
        sframe: non_empty(".sframe"),
    })
}

/// Whether a binary's own code keeps frame pointers.
///
/// `main`'s prologue when there is a `main` to read: it is the program's own
/// code, compiled with the program's flags, where the runtime linked into it
/// -- a Rust program's standard library, a C program's start files -- was
/// compiled with someone else's. A binary with no `main` in its symbols (a
/// stripped one) is read by every function its `.eh_frame_hdr` indexes, and
/// keeps them when more than a quarter of those do. A quarter, because that
/// is what separates the two builds in practice (`docs/unwind.md`): a
/// distribution build with frame pointers sets one up in 36 to 57 % of its
/// indexed functions -- the rest are `.cold` fragments, leaves and assembly
/// -- and one built without them in none.
fn frame_pointers(elf: &Elf, bytes: &[u8]) -> Option<(bool, String)> {
    if let Some(offset) = elf.function_offset("main") {
        let keeps = super::sets_up_frame_pointer(bytes, offset)?;
        return Some((
            keeps,
            format!(
                "`main` sets up {} frame pointer",
                if keeps { "a" } else { "no" }
            ),
        ));
    }
    let starts = elf.fde_starts()?;
    let answers: Vec<bool> = starts
        .iter()
        .filter_map(|vaddr| elf.file_offset_of(*vaddr))
        .filter_map(|offset| super::sets_up_frame_pointer(bytes, offset))
        .collect();
    if answers.is_empty() {
        return None;
    }
    let kept = answers.iter().filter(|keeps| **keeps).count();
    Some((
        kept * 4 > answers.len(),
        format!(
            "{kept} of the {} functions its unwind table indexes set up a frame pointer",
            answers.len()
        ),
    ))
}

// ---------------------------------------------------------------------------
// Requirements.

/// Answers one requirement of an unwind mode, for this machine and this
/// target. `target` is the image, or why there is none.
pub fn probe(
    requirement: &str,
    environment: &Environment,
    target: &Result<TargetImage, String>,
) -> Probe {
    let on_target = |answer: &dyn Fn(&TargetImage) -> Probe| match target {
        Ok(image) => answer(image),
        Err(reason) => Probe::Unknown(reason.clone()),
    };
    match requirement {
        "perf:callchain" => match crate::perf::SampledEvent::callchain_supported() {
            Ok(()) => Probe::Available,
            Err(error) => Probe::Missing(format!(
                "perf_event_open refused a sampling event carrying a callchain ({error}); \
                 perf_event_paranoid is {}",
                paranoid(environment)
            )),
        },
        "perf:user_stack" => match crate::perf::user_stack_supported() {
            Ok(()) => Probe::Available,
            Err(error) => Probe::Missing(format!(
                "perf_event_open refused to copy the user registers and stack with a sample \
                 ({error})"
            )),
        },
        "pmu:branch_call_stack" => match crate::perf::branch_call_stack_supported() {
            Ok(()) => Probe::Available,
            Err(error) => Probe::Missing(format!(
                "this cpu offers no branch call stack to this user ({error}): LBR call-stack \
                 mode is Intel's, from Haswell; {}",
                cpu_model().unwrap_or_else(|| "the cpu model is not readable".to_owned())
            )),
        },
        "kernel:sframe" => kernel_sframe(environment),
        "target:frame_pointers" => on_target(&|image| match &image.frame_pointers {
            Some((true, _)) => Probe::Available,
            Some((false, evidence)) => Probe::Missing(format!(
                "{} was built without frame pointers: {evidence}",
                image.path.display()
            )),
            None => Probe::Unknown(format!(
                "cannot tell whether {} keeps frame pointers on {}",
                image.path.display(),
                std::env::consts::ARCH
            )),
        }),
        "target:cfi" => on_target(&|image| {
            if image.cfi {
                Probe::Available
            } else {
                Probe::Missing(format!(
                    "{} carries no .eh_frame or .debug_frame to unwind from",
                    image.path.display()
                ))
            }
        }),
        "target:sframe" => on_target(&|image| {
            if image.sframe {
                Probe::Available
            } else {
                Probe::Missing(format!(
                    "{} carries no .sframe section (binutils 2.40 and later write one with \
                     `-Wa,--gsframe`)",
                    image.path.display()
                ))
            }
        }),
        other => Probe::Unknown(format!(
            "no probe is written for unwind requirement `{other}`"
        )),
    }
}

fn paranoid(environment: &Environment) -> String {
    environment
        .perf_event_paranoid
        .map_or_else(|| "unreadable".to_owned(), |value| value.to_string())
}

fn cpu_model() -> Option<String> {
    let text = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    let line = text.lines().find(|line| line.starts_with("model name"))?;
    Some(format!("this is {}", line.split_once(':')?.1.trim()))
}

/// The newest kernel whose source was read for SFrame support, and found none.
///
/// Read from the 7.2.6 tree of the development machine (`docs/unwind.md`): its
/// user unwinder (`include/linux/unwind_user_types.h`, since 6.17) knows one
/// type, `UNWIND_USER_TYPE_FP`, and nothing reads a `.sframe` section. A
/// kernel newer than this is not assumed either way.
const SFRAME_READ_THROUGH: (u32, u32) = (7, 2);

fn kernel_sframe(environment: &Environment) -> Probe {
    let Some(version) = environment.kernel_version else {
        return Probe::Unknown(format!(
            "cannot read the kernel version from `{}`",
            environment.kernel_release
        ));
    };
    let (major, minor) = (version.major, version.minor);
    if (major, minor) <= SFRAME_READ_THROUGH {
        Probe::Missing(format!(
            "kernel {major}.{minor} does not unwind user stacks from .sframe: its user unwinder \
             (unwind_user, since 6.17) knows frame pointers only, and no release through \
             {}.{} adds SFrame",
            SFRAME_READ_THROUGH.0, SFRAME_READ_THROUGH.1
        ))
    } else {
        Probe::Unknown(format!(
            "kernel {major}.{minor} is newer than the last whose source was read for SFrame \
             support ({}.{})",
            SFRAME_READ_THROUGH.0, SFRAME_READ_THROUGH.1
        ))
    }
}

// ---------------------------------------------------------------------------
// The choice.

/// Why [`choose`] refused, and whose gap it is.
///
/// The collectors read the sentence alone, as they always have; the resolver
/// reads both, so that a choice refused here is refused before the dump and
/// the run as well, with the exit code the gap deserves, rather than leaving
/// the resolved file without a `stack` and saying nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub whose: crate::collect::Whose,
    pub reason: String,
}

impl Refusal {
    fn machine(reason: impl Into<String>) -> Self {
        Self {
            whose: crate::collect::Whose::Machine,
            reason: reason.into(),
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason)
    }
}

/// Chooses how the stacks of `request` are unwound. See [`super::choose`].
pub fn choose(request: &Request, environment: &Environment) -> Result<StackChoice, String> {
    choose_classified(request, environment).map_err(|refusal| refusal.reason)
}

/// [`choose`], with whose gap a refusal is. A mode the catalog does not
/// declare is the request's; `auto` with nothing this machine serves, or a
/// `debuginfod` with no server named in the environment, the machine's; one
/// the machine would serve and nothing in this build reads, the build's. A
/// forced mode this machine lacks what it needs for is the machine's too, and
/// is held at the build's code for now (see [`forced`]).
pub fn choose_classified(
    request: &Request,
    environment: &Environment,
) -> Result<StackChoice, Refusal> {
    let requested = request.stack.unwrap_or(StackMode::Auto);
    let target = target_image(&request.target);
    // Asked for every mode, `none` included: the paranoia that refuses the
    // kernel half of a stack refuses a sample taken in the kernel too, and a
    // profile without stacks is still a sampler that has to exclude it.
    let user_only = !crate::perf::kernel_callchain_allowed();
    let mut choice = if requested.is_concrete() {
        forced(requested, environment, &target)?
    } else {
        ranked(environment, &target)?
    };
    choice.user_only = user_only;
    // Where the names come from is decided here too, so that a debuginfod
    // asked for with no server to ask is refused before the launch, and the
    // resolved file says which servers a stripped file's names are asked of.
    if request.symbols == Some(SymbolSource::Debuginfod) {
        let client = Debuginfod::from_env().map_err(Refusal::machine)?;
        choice.symbols = SymbolSource::Debuginfod;
        choice.symbol_servers = client.servers().to_vec();
    }
    if choice.user_only {
        let what = if choice.mode == StackMode::None {
            "samples are user-only"
        } else {
            "stacks are user-only"
        };
        choice.degradations.push(format!(
            "perf_event_paranoid is {}, which refuses this user the kernel: {what}, and a \
             sample taken while the task ran in the kernel is not delivered, so time spent \
             in system calls is missing from this profile",
            paranoid(environment)
        ));
    }
    Ok(choice)
}

/// Every requirement of a mode, probed.
fn verdicts(
    entry: &ModeEntry,
    environment: &Environment,
    target: &Result<TargetImage, String>,
) -> (Vec<String>, Vec<String>) {
    let mut missing = Vec::new();
    let mut unknown = Vec::new();
    for requirement in &entry.requires {
        match probe(requirement, environment, target) {
            Probe::Available => {}
            Probe::Missing(reason) => missing.push(format!("{requirement}: missing: {reason}")),
            Probe::Unknown(reason) => unknown.push(format!("{requirement}: unknown: {reason}")),
        }
    }
    (missing, unknown)
}

fn forced(
    mode: StackMode,
    environment: &Environment,
    target: &Result<TargetImage, String>,
) -> Result<StackChoice, Refusal> {
    let entry = entry(mode).ok_or_else(|| Refusal {
        whose: crate::collect::Whose::Request,
        reason: format!("`--stack {mode}` is not declared"),
    })?;
    let (missing, unknown) = verdicts(entry, environment, target);
    if !missing.is_empty() {
        // The machine's gap, by `Whose`'s own definition: the request is fine
        // and this machine cannot serve it (exit 1).
        return Err(Refusal {
            whose: crate::collect::Whose::Machine,
            reason: format!(
                "`--stack {mode}` cannot be served here: {}",
                missing.join("; ")
            ),
        });
    }
    if !implemented(mode) {
        return Err(Refusal {
            whose: crate::collect::Whose::Build,
            reason: format!(
                "`--stack {mode}` is not implemented: this machine would serve it, and this \
                 build has no collector that reads it (docs/unwind.md)"
            ),
        });
    }
    let mut degradations: Vec<String> = entry.degraded.iter().cloned().collect();
    // Forced over an unknown is served, as a pinned backend is, with what was
    // assumed written down rather than checked.
    degradations.extend(
        unknown
            .into_iter()
            .map(|reason| format!("assumed rather than checked -- {reason}")),
    );
    Ok(StackChoice {
        mode,
        forced: true,
        user_only: false,
        considered: Vec::new(),
        degradations,
        copied_stack_bytes: entry.copies,
        symbols: SymbolSource::Local,
        symbol_servers: Vec::new(),
    })
}

fn ranked(
    environment: &Environment,
    target: &Result<TargetImage, String>,
) -> Result<StackChoice, Refusal> {
    let mut considered = Vec::new();
    let mut selected: Option<&ModeEntry> = None;
    for entry in modes().iter().filter(|entry| entry.auto) {
        let verdict = if selected.is_some() {
            Verdict::NotReached
        } else {
            let (mut missing, unknown) = verdicts(entry, environment, target);
            // Unknown never counts as available: `auto` passes over a mode it
            // could not check, and says which requirement it could not.
            missing.extend(unknown);
            if !missing.is_empty() {
                Verdict::Rejected(missing)
            } else if !implemented(entry.mode) {
                Verdict::Unimplemented
            } else {
                selected = Some(entry);
                Verdict::Selected
            }
        };
        considered.push(ConsideredMode {
            mode: entry.mode,
            verdict,
        });
    }
    let entry = selected.ok_or_else(|| {
        let reasons: Vec<String> = considered
            .iter()
            .filter_map(|considered| match &considered.verdict {
                Verdict::Rejected(reasons) => {
                    Some(format!("{}: {}", considered.mode, reasons.join("; ")))
                }
                _ => None,
            })
            .collect();
        Refusal::machine(format!(
            "`--stack auto` found no unwind mode this machine serves: {}",
            reasons.join(" | ")
        ))
    })?;
    Ok(StackChoice {
        mode: entry.mode,
        forced: false,
        user_only: false,
        considered,
        degradations: entry.degraded.iter().cloned().collect(),
        copied_stack_bytes: entry.copies,
        symbols: SymbolSource::Local,
        symbol_servers: Vec::new(),
    })
}

// ---------------------------------------------------------------------------
// Call frame information: the unwinder `fp+leaf` and `dwarf` share.

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
type Native = framehop::UnwinderNative<Vec<u8>, framehop::MayAllocateDuringUnwind>;
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
type NativeCache = framehop::CacheNative<framehop::MayAllocateDuringUnwind>;

/// The deepest stack a copied-stack unwind walks, the kernel's own default
/// for a callchain (`perf_event_max_stack`).
const MAX_DEPTH: usize = 127;

/// How one copied-stack unwind ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkEnd {
    /// The unwinder found the root: `_start`, or a frame that ends the chain.
    Complete,
    /// The next frame's return address lies beyond the bytes copied.
    CutByCopy,
    /// The unwind information ran out, or described something the stack did
    /// not hold.
    Failed,
    /// [`MAX_DEPTH`] frames and still going.
    DepthLimit,
}

/// One unwind of a copied stack: the instruction pointer, then each return
/// address, leaf first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Walk {
    pub frames: Vec<u64>,
    pub end: WalkEnd,
}

/// Unwinds one process's stacks from their call frame information.
///
/// Built from the process's executable mappings, each module's `.eh_frame`
/// (indexed by `.eh_frame_hdr`), `.debug_frame` and `.text` handed to
/// `framehop`, which also handles a sample taken inside a prologue or an
/// epilogue, where the frame is half built. An address in no module, or in a
/// module with no unwind information, is walked by frame pointers, which is
/// `framehop`'s own fallback and never worse than the kernel's.
pub struct Cfi {
    pid: i32,
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    unwinder: Native,
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    cache: NativeCache,
    /// The start address of every module added, by mapping.
    loaded: BTreeSet<u64>,
    /// Modules whose file could not be read, by path: not retried.
    unreadable: BTreeSet<PathBuf>,
}

impl Cfi {
    pub fn for_process(pid: i32) -> Self {
        let cfi = Self {
            pid,
            #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
            unwinder: Native::new(),
            #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
            cache: NativeCache::new(),
            loaded: BTreeSet::new(),
            unreadable: BTreeSet::new(),
        };
        // Not read now: a launched target is still a fork of this process
        // waiting at the gate when its unwinder is built, and its maps are
        // this tool's own. The first walk, or the first tick, reads them.
        cfi
    }

    /// Brings the modules up to date with the process's mappings: adds the
    /// ones mapped since the last read, and drops the ones that are gone -- an
    /// `execve` replaces every one of them, and a module left registered at an
    /// address the new image reuses would unwind it by the wrong rules.
    pub fn refresh(&mut self) {
        let regions = read_maps(self.pid);
        if regions.is_empty() {
            return;
        }
        let current: BTreeSet<u64> = regions.iter().map(|region| region.start).collect();
        let gone: Vec<u64> = self.loaded.difference(&current).copied().collect();
        for start in gone {
            #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
            {
                use framehop::Unwinder as _;
                self.unwinder.remove_module(start);
            }
            self.loaded.remove(&start);
        }
        for region in &regions {
            self.add(region);
        }
    }

    /// How many modules carry unwind information here.
    pub fn modules(&self) -> usize {
        self.loaded.len()
    }

    fn add(&mut self, region: &Region) {
        if self.loaded.contains(&region.start) {
            return;
        }
        let Some(path) = region.path.clone() else {
            return;
        };
        if self.unreadable.contains(&path) {
            return;
        }
        match module(region, &path) {
            Some(module) => {
                #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
                {
                    use framehop::Unwinder as _;
                    self.unwinder.add_module(module);
                }
                #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
                let _ = module;
                self.loaded.insert(region.start);
            }
            None => {
                self.unreadable.insert(path);
            }
        }
    }

    /// The return address the leaf at `regs` will return to, read from the
    /// copied top of its stack by the leaf's own unwind rule. `None` when the
    /// rule points outside the copy, or there is no rule and no frame pointer
    /// to fall back on.
    pub fn caller(&mut self, regs: UserRegs, stack: &[u8]) -> Option<u64> {
        let walk = self.walk_to(regs, stack, 2);
        walk.frames.get(1).copied()
    }

    /// Unwinds a copied stack from the registers it was copied with.
    pub fn walk(&mut self, regs: UserRegs, stack: &[u8]) -> Walk {
        self.walk_to(regs, stack, MAX_DEPTH)
    }

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    fn walk_to(&mut self, regs: UserRegs, stack: &[u8], depth: usize) -> Walk {
        use framehop::Unwinder as _;
        if self.loaded.is_empty() {
            self.refresh();
        }
        let mut beyond = false;
        let mut read_stack = |addr: u64| -> Result<u64, ()> {
            let offset = addr.checked_sub(regs.sp).ok_or(())? as usize;
            match stack.get(offset..offset + 8) {
                Some(bytes) => Ok(u64::from_le_bytes(bytes.try_into().map_err(|_| ())?)),
                None => {
                    beyond = true;
                    Err(())
                }
            }
        };
        #[cfg(target_arch = "x86_64")]
        let start = framehop::x86_64::UnwindRegsX86_64::new(regs.ip, regs.sp, regs.fp);
        #[cfg(target_arch = "aarch64")]
        let start = framehop::aarch64::UnwindRegsAarch64::new(regs.lr, regs.sp, regs.fp);
        let mut frames = Vec::new();
        let end = {
            let mut iterator =
                self.unwinder
                    .iter_frames(regs.ip, start, &mut self.cache, &mut read_stack);
            loop {
                if frames.len() >= depth {
                    break WalkEnd::DepthLimit;
                }
                match iterator.next() {
                    Ok(Some(frame)) => frames.push(frame.address()),
                    Ok(None) => break WalkEnd::Complete,
                    Err(_) => break WalkEnd::Failed,
                }
            }
        };
        let end = if end == WalkEnd::Failed && beyond {
            WalkEnd::CutByCopy
        } else {
            end
        };
        Walk { frames, end }
    }

    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    fn walk_to(&mut self, regs: UserRegs, _stack: &[u8], _depth: usize) -> Walk {
        Walk {
            frames: vec![regs.ip],
            end: WalkEnd::Failed,
        }
    }
}

/// A mapping's unwind sections, for `framehop`.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn module(region: &Region, path: &Path) -> Option<framehop::Module<Vec<u8>>> {
    let bytes = std::fs::read(path).ok()?;
    let elf = Elf::parse(&bytes)?;
    // What the loader added to every virtual address of this file: the
    // mapping's start less the address its file offset has in the file.
    let bias = region
        .start
        .checked_sub(elf.vaddr_of_offset(region.offset)?)?;
    let range = |name: &str| {
        elf.section(name)
            .filter(|section| section.size > 0)
            .map(|section| section.addr..section.addr + section.size)
    };
    let data = |name: &str| {
        elf.section(name)
            .and_then(|section| elf.section_data(&section))
            .filter(|data| !data.is_empty())
            .map(<[u8]>::to_vec)
    };
    let info = framehop::ExplicitModuleSectionInfo {
        base_svma: 0,
        text_svma: range(".text"),
        text: data(".text"),
        got_svma: range(".got"),
        eh_frame_svma: range(".eh_frame"),
        eh_frame: data(".eh_frame"),
        eh_frame_hdr_svma: range(".eh_frame_hdr"),
        eh_frame_hdr: data(".eh_frame_hdr"),
        debug_frame: data(".debug_frame"),
        ..Default::default()
    };
    Some(framehop::Module::new(
        path.display().to_string(),
        region.start..region.end,
        bias,
        info,
    ))
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn module(_region: &Region, _path: &Path) -> Option<()> {
    None
}

// ---------------------------------------------------------------------------
// Names: where they come from.

/// Where a stack's names are looked for: `--symbols`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum SymbolSource {
    /// The files the target mapped, and the perf map it wrote for code no
    /// file holds (`/tmp/perf-<pid>.map`), and nothing else. The default: it
    /// asks nothing of anyone -- the map is a file the target itself wrote on
    /// this machine, which is why it needs no rung of its own.
    #[default]
    Local,
    /// Those files first, then the servers in `DEBUGINFOD_URLS` for a file
    /// that carries no symbol table, by its build id. A network request about
    /// what this machine runs, so never made unless asked.
    Debuginfod,
}

impl SymbolSource {
    pub const ALL: [SymbolSource; 2] = [SymbolSource::Local, SymbolSource::Debuginfod];

    pub fn as_str(self) -> &'static str {
        match self {
            SymbolSource::Local => "local",
            SymbolSource::Debuginfod => "debuginfod",
        }
    }
}

impl FromStr for SymbolSource {
    type Err = crate::ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        SymbolSource::ALL
            .into_iter()
            .find(|source| source.as_str() == s.trim())
            .ok_or_else(|| {
                crate::ParseError::new(format!(
                    "unknown symbol source `{s}` (expected local or debuginfod)"
                ))
            })
    }
}

impl fmt::Display for SymbolSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

serde_as_string!(SymbolSource);

/// Where one module's names came from, for the record that travels with the
/// stacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameOrigin {
    /// The file's own `.symtab`.
    SymbolTable,
    /// Only its `.dynsym`: the exported functions are named, the rest are
    /// `module+offset`.
    DynamicSymbols,
    /// A debug file fetched by build id, or found in the cache it is kept in.
    Debuginfod {
        build_id: String,
        /// The server it came from, or `None` when the cache already held it.
        server: Option<String>,
    },
    /// No names at all: an unreadable file, or none in it.
    None(String),
    /// The target's `/tmp/perf-<pid>.map`, for code in anonymous executable
    /// memory: what a JIT or an interpreter's trampolines say they generated.
    /// Keyed by the map's own path, since the code it names has no file.
    PerfMap {
        /// Lines the map held when it last named a frame.
        lines: u64,
    },
}

impl fmt::Display for NameOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NameOrigin::SymbolTable => f.write_str("its own symbol table"),
            NameOrigin::DynamicSymbols => {
                f.write_str("its dynamic symbols only: its internal functions are unnamed")
            }
            NameOrigin::Debuginfod {
                build_id,
                server: Some(server),
            } => write!(f, "debuginfod, {server}, build id {build_id}"),
            NameOrigin::Debuginfod {
                build_id,
                server: None,
            } => write!(f, "the debuginfod cache, build id {build_id}"),
            NameOrigin::None(reason) => write!(f, "no names: {reason}"),
            NameOrigin::PerfMap { lines } => write!(
                f,
                "perf-map, the {lines} lines the program wrote for code no file holds"
            ),
        }
    }
}

/// A debuginfod client: the servers, the cache, and what this run already
/// asked for.
///
/// Fetched with `curl`, which every machine this tool installs on has and which
/// speaks the HTTPS the public servers require; linking a TLS stack into the
/// tool for one optional request was the alternative (`docs/unwind.md`). The
/// cache is the one elfutils, `gdb` and `perf` share, laid out the way they
/// lay it out, so a file any of them fetched is not fetched again.
#[derive(Debug)]
pub struct Debuginfod {
    servers: Vec<String>,
    cache: PathBuf,
    /// Build ids asked for in this run that no server had, with why.
    failed: std::sync::Mutex<HashMap<String, String>>,
}

impl Debuginfod {
    /// From `DEBUGINFOD_URLS` (space separated) and elfutils' cache location.
    pub fn from_env() -> Result<Self, String> {
        let urls = std::env::var("DEBUGINFOD_URLS").unwrap_or_default();
        let servers: Vec<String> = urls.split_whitespace().map(str::to_owned).collect();
        if servers.is_empty() {
            return Err(
                "`--symbols debuginfod` needs DEBUGINFOD_URLS, the servers to ask, and it is \
                 unset or empty"
                    .to_owned(),
            );
        }
        Ok(Self::new(servers, default_cache()))
    }

    pub fn new(servers: Vec<String>, cache: PathBuf) -> Self {
        Self {
            servers,
            cache,
            failed: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub fn servers(&self) -> &[String] {
        &self.servers
    }

    /// The debug file for a build id: from the cache, or fetched into it.
    pub fn debuginfo(&self, build_id: &str) -> Result<(PathBuf, NameOrigin), String> {
        if build_id.is_empty() || !build_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!("`{build_id}` is not a build id"));
        }
        let directory = self.cache.join(build_id);
        let file = directory.join("debuginfo");
        if file.metadata().is_ok_and(|meta| meta.len() > 0) {
            return Ok((
                file,
                NameOrigin::Debuginfod {
                    build_id: build_id.to_owned(),
                    server: None,
                },
            ));
        }
        if let Some(reason) = self
            .failed
            .lock()
            .ok()
            .and_then(|failed| failed.get(build_id).cloned())
        {
            return Err(reason);
        }
        let result = self.fetch(build_id, &directory, &file);
        if let Err(reason) = &result {
            if let Ok(mut failed) = self.failed.lock() {
                failed.insert(build_id.to_owned(), reason.clone());
            }
        }
        result
    }

    fn fetch(
        &self,
        build_id: &str,
        directory: &Path,
        file: &Path,
    ) -> Result<(PathBuf, NameOrigin), String> {
        std::fs::create_dir_all(directory).map_err(|error| {
            format!(
                "cannot create the debuginfod cache {}: {error}",
                directory.display()
            )
        })?;
        let partial = directory.join(format!(".debuginfo.{}", std::process::id()));
        let mut refusals = Vec::new();
        for server in &self.servers {
            let url = format!(
                "{}/buildid/{build_id}/debuginfo",
                server.trim_end_matches('/')
            );
            let output = std::process::Command::new("curl")
                .args(["--fail", "--silent", "--show-error", "--location"])
                .args(["--max-time", "60", "--output"])
                .arg(&partial)
                .arg(&url)
                .output();
            match output {
                Ok(output) if output.status.success() => {
                    std::fs::rename(&partial, file).map_err(|error| {
                        format!("cannot keep {} in the cache: {error}", file.display())
                    })?;
                    return Ok((
                        file.to_path_buf(),
                        NameOrigin::Debuginfod {
                            build_id: build_id.to_owned(),
                            server: Some(server.clone()),
                        },
                    ));
                }
                Ok(output) => refusals.push(format!(
                    "{server}: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )),
                Err(error) => {
                    let _ = std::fs::remove_file(&partial);
                    return Err(format!(
                        "debuginfod is fetched with `curl`, which could not be run: {error}"
                    ));
                }
            }
        }
        let _ = std::fs::remove_file(&partial);
        let _ = std::fs::remove_dir(directory);
        Err(format!(
            "no server had build id {build_id}: {}",
            refusals.join("; ")
        ))
    }
}

/// `DEBUGINFOD_CACHE_PATH`, else `$XDG_CACHE_HOME/debuginfod_client`, else
/// `~/.cache/debuginfod_client`: elfutils' order.
pub fn default_cache() -> PathBuf {
    if let Some(path) = std::env::var_os("DEBUGINFOD_CACHE_PATH").filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    if let Some(path) = std::env::var_os("XDG_CACHE_HOME").filter(|path| !path.is_empty()) {
        return PathBuf::from(path).join("debuginfod_client");
    }
    let home = std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into());
    PathBuf::from(home).join(".cache/debuginfod_client")
}

/// A build id as its hex spelling.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Per module, where its names came from, in a stable order.
pub type Origins = BTreeMap<String, NameOrigin>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_compiled_in_table_declares_every_mode_once_best_first() {
        let order: Vec<StackMode> = modes().iter().map(|entry| entry.mode).collect();
        assert_eq!(
            order,
            [
                StackMode::Sframe,
                StackMode::FpLeaf,
                StackMode::Dwarf,
                StackMode::Lbr,
                StackMode::Fp,
                StackMode::None
            ]
        );
        assert!(!entry(StackMode::None).unwrap().auto);
        assert_eq!(
            entry(StackMode::Dwarf).unwrap().copies,
            crate::perf::COPIED_STACK_BYTES
        );
        assert_eq!(
            entry(StackMode::FpLeaf).unwrap().copies,
            crate::perf::LEAF_STACK_BYTES
        );
    }

    /// The rule a fallback backend is held to, held to a mode.
    #[test]
    fn a_table_that_breaks_the_rules_is_refused_with_the_rule() {
        let base = EMBEDDED;
        let silent = base.replacen(
            "  degraded: \"8 KiB of user stack",
            "  equivalent: false\n  x_degraded: \"8 KiB of user stack",
            1,
        );
        assert!(load(&silent).is_err());
        let without = base.replace(
            "- mode: lbr",
            "- mode: fp\n  summary: dup\n  cost: low\n  degraded: x\n- mode: lbr2",
        );
        assert!(load(&without).is_err());
        let twice = format!("{base}\n- mode: fp\n  summary: again\n  cost: low\n  degraded: x\n");
        assert!(load(&twice).unwrap_err().contains("twice"));
        let auto = format!("{base}\n- mode: auto\n  summary: no\n  cost: low\n  degraded: x\n");
        assert!(load(&auto).unwrap_err().contains("chooser"));
        let degraded_less = base.replace(
            "  degraded: \"the branch records hold 16 to 32 calls: a deeper stack is cut at\n    its root end\"\n",
            "",
        );
        assert!(load(&degraded_less).unwrap_err().contains("lbr"));
    }

    #[test]
    fn a_symbol_source_reads_back_from_its_own_spelling() {
        for source in SymbolSource::ALL {
            assert_eq!(source.as_str().parse::<SymbolSource>().unwrap(), source);
            let json = serde_json::to_string(&source).unwrap();
            assert_eq!(serde_json::from_str::<SymbolSource>(&json).unwrap(), source);
        }
        assert!("remote".parse::<SymbolSource>().is_err());
        assert_eq!(SymbolSource::default(), SymbolSource::Local);
    }

    /// No server is asked for anything that is not a build id: the id goes
    /// into a URL and a path.
    #[test]
    fn a_build_id_that_is_not_hex_is_refused_before_anything_is_asked() {
        let client = Debuginfod::new(vec!["http://127.0.0.1:9".into()], std::env::temp_dir());
        for bad in ["", "../../etc/passwd", "abc/def", "12 34"] {
            assert!(client.debuginfo(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_kernel_is_read_for_sframe_through_the_release_whose_source_was_read() {
        let mut environment = crate::capabilities::Capabilities::detect()
            .environment()
            .clone();
        for (release, missing) in [("6.17.0", true), ("7.2.6", true), ("7.3.0", false)] {
            environment.kernel_version = crate::capabilities::KernelVersion::parse(release);
            let probe = kernel_sframe(&environment);
            assert_eq!(probe.is_missing(), missing, "{release}: {probe:?}");
            if !missing {
                assert!(probe.is_unknown(), "{release}: {probe:?}");
            }
        }
    }
}
