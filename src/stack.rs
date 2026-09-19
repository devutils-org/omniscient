//! Call stacks: what the kernel walked, and what its addresses mean.
//!
//! A sample says *when* and *how much*. A stack is what makes it say *where*,
//! and it arrives in three pieces that are easy to confuse for one.
//!
//! The kernel walks the stack in the interrupt handler, with the sampled task's
//! registers still live, and writes a flat list of addresses with markers in
//! it. Nothing afterwards can recover a stack that was not taken then, which is
//! why the walk is a property of the event and not a post-processing choice.
//! [`RawStack`] is that list, split back into the halves the markers delimit.
//!
//! Then the addresses have to be turned into names. An address means nothing
//! without the mapping it fell in: the same number is a different function in
//! two processes, and in one process after a `dlopen`. [`Symbolizer`] reads the
//! target's own `/proc/<pid>/maps` and the symbol tables of the files behind
//! it, and `/proc/kallsyms` for the kernel half.
//!
//! Finally, a profile of ten thousand samples holds a few dozen distinct
//! stacks. [`StackTable`] stores each one once and hands back a [`StackId`];
//! the id travels on the event as an attribute, the table travels beside the
//! results. Aggregating by id rather than by frame list is what keeps a profile
//! of a deep stack cheaper to sum than it was to collect.
//!
//! What is deliberately not here is a DWARF unwinder. Writing one is a project,
//! not a function, and a wrong one produces confident nonsense; `--stack dwarf`
//! says it is not implemented rather than quietly unwinding a single frame and
//! calling it a profile.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::perf::{Callchain, CpuMode, Sample};
use crate::request::StackMode;

/// Which side of the privilege boundary a frame ran on.
///
/// Kept apart rather than concatenated because the difference is the whole
/// answer: a stack that is five kernel frames under `read` says the program is
/// waiting, and the same stack presented as one list says it is slow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Context {
    Kernel,
    User,
}

impl Context {
    pub fn as_str(self) -> &'static str {
        match self {
            Context::Kernel => "kernel",
            Context::User => "user",
        }
    }
}

impl fmt::Display for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Anything at or above this is a context marker, not an address. The kernel's
/// own test, and the reason a callchain cannot simply be read as a list of
/// instruction pointers.
const PERF_CONTEXT_MAX: u64 = perf_event_open_sys::bindings::PERF_CONTEXT_MAX;
const PERF_CONTEXT_KERNEL: u64 = perf_event_open_sys::bindings::PERF_CONTEXT_KERNEL;
const PERF_CONTEXT_USER: u64 = perf_event_open_sys::bindings::PERF_CONTEXT_USER;

/// One sample's callchain, split at the markers the kernel put in it.
///
/// Both halves are ordered leaf first: `kernel[0]` is where the task was when
/// the sample landed, and the last entry of `user` is closest to `main`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawStack {
    pub kernel: Vec<u64>,
    pub user: Vec<u64>,
    /// Frames the kernel attributed to a context this tool does not model: a
    /// hypervisor, or a guest. Counted rather than dropped, because a stack
    /// that quietly loses three frames is a stack that reads as shallower than
    /// it was.
    pub elsewhere: Vec<u64>,
}

impl RawStack {
    /// Splits a callchain, using the sample's own mode for the frames that
    /// precede the first marker.
    ///
    /// The kernel normally writes a marker before anything else, but the frames
    /// it writes ahead of one belong to the mode the sample was taken in, and
    /// filing them as user code would put kernel addresses through a process's
    /// symbol tables and name them after whatever function happens to live at
    /// that offset.
    pub fn split(callchain: &[u64], cpu_mode: CpuMode) -> Self {
        let mut stack = RawStack::default();
        let mut context = match cpu_mode {
            CpuMode::Kernel => Some(Context::Kernel),
            CpuMode::User => Some(Context::User),
            CpuMode::Unknown => None,
        };
        for &ip in callchain {
            if ip >= PERF_CONTEXT_MAX {
                context = match ip {
                    PERF_CONTEXT_KERNEL => Some(Context::Kernel),
                    PERF_CONTEXT_USER => Some(Context::User),
                    _ => None,
                };
                continue;
            }
            match context {
                Some(Context::Kernel) => stack.kernel.push(ip),
                Some(Context::User) => stack.user.push(ip),
                None => stack.elsewhere.push(ip),
            }
        }
        stack
    }

    /// Splits the callchain of a sample, if it has one.
    pub fn of(sample: &Sample) -> Option<Self> {
        sample
            .callchain
            .as_deref()
            .map(|chain| RawStack::split(chain, sample.cpu_mode))
    }

    pub fn depth(&self) -> usize {
        self.kernel.len() + self.user.len() + self.elsewhere.len()
    }

    pub fn is_empty(&self) -> bool {
        self.depth() == 0
    }
}

/// One frame of a symbolized stack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frame {
    /// The address the kernel reported, kept whatever symbolization found. A
    /// name is an interpretation; the address is the measurement.
    pub addr: u64,
    pub context: Context,
    /// The file the address fell in, as the target's own map names it. `None`
    /// when no mapping covered it, which is what an address from a region that
    /// was unmapped between the sample and the read looks like.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    /// The function the address fell in. `None` when the module carries no
    /// symbol for it: a stripped library, or a kernel whose addresses this
    /// process is not allowed to see.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// How far into the symbol the address is, or -- when there is no symbol
    /// but there is a module -- how far into that file.
    ///
    /// The second is what `addr2line` takes, so an unnamed frame is still a
    /// frame someone can look up, where the raw address it was sampled at is
    /// only meaningful inside the process that has since exited.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub offset: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl Frame {
    /// The frame with nothing resolved, which is what an address with no
    /// mapping honestly reduces to.
    fn unresolved(addr: u64, context: Context) -> Self {
        Self {
            addr,
            context,
            module: None,
            symbol: None,
            offset: 0,
        }
    }

    /// What a renderer prints for this frame: the symbol if there is one, the
    /// address if there is not. Never a fabricated name.
    pub fn label(&self) -> String {
        match &self.symbol {
            Some(symbol) => symbol.clone(),
            None => match &self.module {
                Some(module) => format!("{}+{:#x}", basename(module), self.offset),
                None => format!("{:#x}", self.addr),
            },
        }
    }
}

impl fmt::Display for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.symbol {
            Some(symbol) if self.offset > 0 => write!(f, "{symbol}+{:#x}", self.offset)?,
            Some(symbol) => f.write_str(symbol)?,
            None if self.module.is_some() => write!(f, "+{:#x}", self.offset)?,
            None => write!(f, "{:#x}", self.addr)?,
        }
        match (&self.module, self.context) {
            (Some(module), _) => write!(f, " ({})", basename(module)),
            (None, Context::Kernel) => f.write_str(" ([kernel])"),
            (None, Context::User) => Ok(()),
        }
    }
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// One distinct call stack, symbolized once however often it was sampled.
///
/// Leaf first, kernel frames before user frames, which is the order the kernel
/// walked them in. A renderer that wants root-first reverses it; storing it
/// twice would be storing an opinion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stack {
    pub frames: Vec<Frame>,
}

impl Stack {
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn depth(&self) -> usize {
        self.frames.len()
    }

    /// The frames of one context, leaf first.
    pub fn frames_in(&self, context: Context) -> impl Iterator<Item = &Frame> {
        self.frames
            .iter()
            .filter(move |frame| frame.context == context)
    }
}

impl fmt::Display for Stack {
    /// Root first, semicolon separated: the order a person reads a stack in,
    /// and the order the collapsed-stack format writes one.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let labels: Vec<String> = self.frames.iter().rev().map(Frame::label).collect();
        f.write_str(&labels.join(";"))
    }
}

/// A stack's place in the table that holds it.
///
/// An index, not a hash: it is stable for one run, which is all a side table
/// beside one run's results has to be, and it costs four characters in JSON
/// where a hash costs twenty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StackId(pub u64);

impl fmt::Display for StackId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Every distinct stack a run captured, stored once each.
///
/// This is the side table. A captured stack does not go on the event -- an
/// event is a measurement and a stack is a hundred bytes of context that
/// repeats -- so the event carries [`STACK_ID_ATTR`] and the table travels
/// beside the results.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "Vec<Stack>", into = "Vec<Stack>")]
pub struct StackTable {
    stacks: Vec<Stack>,
    /// The raw callchain, markers and all, to the id it was given. Interning on
    /// the addresses rather than on the frames is what makes symbolization
    /// happen once per distinct stack instead of once per sample.
    by_callchain: HashMap<Vec<u64>, StackId>,
}

/// The attribute an event carries to name its stack in the table.
pub const STACK_ID_ATTR: &str = "stack_id";

impl StackTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.stacks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stacks.is_empty()
    }

    pub fn get(&self, id: StackId) -> Option<&Stack> {
        self.stacks.get(id.0 as usize)
    }

    pub fn iter(&self) -> impl Iterator<Item = (StackId, &Stack)> {
        self.stacks
            .iter()
            .enumerate()
            .map(|(index, stack)| (StackId(index as u64), stack))
    }

    /// Interns a callchain, symbolizing it only if it has not been seen before.
    pub fn intern(
        &mut self,
        callchain: &[u64],
        cpu_mode: CpuMode,
        symbols: &mut Symbolizer,
    ) -> StackId {
        if let Some(id) = self.by_callchain.get(callchain) {
            return *id;
        }
        let raw = RawStack::split(callchain, cpu_mode);
        let id = StackId(self.stacks.len() as u64);
        self.stacks.push(symbols.symbolize(&raw));
        self.by_callchain.insert(callchain.to_vec(), id);
        id
    }
}

impl From<Vec<Stack>> for StackTable {
    fn from(stacks: Vec<Stack>) -> Self {
        // A table read back from a result file is complete and will not be
        // added to, so the intern index is not rebuilt: it exists to make a
        // second sighting of a stack cheap, and there is no second sighting.
        Self {
            stacks,
            by_callchain: HashMap::new(),
        }
    }
}

impl From<StackTable> for Vec<Stack> {
    fn from(table: StackTable) -> Self {
        table.stacks
    }
}

/// Address to name, through the target's own maps.
///
/// Built while the target is alive and reused for the whole run: a process's
/// maps are gone the moment it exits, and a symbolizer created afterwards can
/// only report addresses. A collector that wants names has to intern its stacks
/// as it drains, not once at the end.
pub struct Symbolizer {
    /// The process whose maps are being read.
    pid: i32,
    regions: Vec<Region>,
    /// Parsed once per file, by the path the map named. `None` for a file that
    /// could not be read or carries no symbols, so it is not retried per frame.
    modules: HashMap<PathBuf, Option<Module>>,
    /// Addresses that fell in no mapping.
    unmapped: u64,
    /// Whether a re-read has already found the mapping an address fell outside
    /// of, and how many have been attempted without finding one.
    ///
    /// Two counters rather than one flag, because the two failures are
    /// different. A re-read that returns mappings has answered the question
    /// and there is nothing more to learn from another; a re-read that returns
    /// none has answered nothing -- the process was inside `execve`, or it has
    /// exited -- and the next frame deserves another try. Bounded, so that a
    /// target that really is gone costs a handful of reads rather than one per
    /// frame.
    remapped: bool,
    failed_rereads: u32,
}

/// How many times a symbolizer goes back to a `/proc/<pid>/maps` that gave it
/// nothing before it stops asking.
const MAX_FAILED_REREADS: u32 = 8;

impl Symbolizer {
    /// A symbolizer for one process: its mappings, and the kernel's.
    ///
    /// Pid zero is this process, which is what it means to `perf_event_open`
    /// and therefore what a caller that opened `Scope::Process(0)` has in hand.
    /// `/proc/0` is nothing at all, so taking it literally would give a
    /// symbolizer that resolves every address to an address.
    pub fn for_process(pid: i32) -> Self {
        Self {
            pid,
            regions: read_maps(pid),
            modules: HashMap::new(),
            unmapped: 0,
            remapped: false,
            failed_rereads: 0,
        }
    }

    /// Re-reads the target's mappings, and says whether there were any.
    ///
    /// A collector calls this while the target is alive, because the moment a
    /// symbolizer is built is rarely the moment its maps are worth reading. A
    /// launched program is inside `execve` when the first tick comes round --
    /// its old image torn down and its new one not yet mapped -- so the
    /// snapshot taken then holds `[vsyscall]` and nothing else, and every frame
    /// of every stack afterwards comes back as an address.
    ///
    /// An empty read is never adopted. It is not a process with no mappings: it
    /// is a process caught mid-`execve`, or one that has exited, and replacing
    /// a usable snapshot with it is the same defect from the other end.
    pub fn refresh(&mut self) -> bool {
        let fresh = read_maps(self.pid);
        if fresh.is_empty() {
            return false;
        }
        self.regions = fresh;
        true
    }

    /// Whether this machine let the kernel's symbols be read at all.
    ///
    /// `kptr_restrict` publishes `/proc/kallsyms` with every address zeroed
    /// rather than refusing it, so the file reads fine and resolves everything
    /// to the same symbol. That has to be detected, not discovered in a profile
    /// where the whole kernel is called `_stext`.
    pub fn kernel_symbols_available(&self) -> bool {
        kernel_symbols().is_some_and(KernelSymbols::is_usable)
    }

    /// Addresses no mapping covered.
    pub fn unmapped(&self) -> u64 {
        self.unmapped
    }

    /// Turns a split callchain into frames.
    pub fn symbolize(&mut self, raw: &RawStack) -> Stack {
        let mut frames = Vec::with_capacity(raw.depth());
        for &addr in &raw.kernel {
            frames.push(self.kernel_frame(addr));
        }
        for &addr in &raw.user {
            frames.push(self.user_frame(addr));
        }
        // A frame from a context this build does not model still happened. It
        // is carried with its address and no name rather than dropped, so a
        // stack's depth is the depth the kernel walked.
        for &addr in &raw.elsewhere {
            frames.push(Frame::unresolved(addr, Context::User));
        }
        Stack { frames }
    }

    fn kernel_frame(&mut self, addr: u64) -> Frame {
        let named = kernel_symbols()
            .filter(|symbols| symbols.is_usable())
            .and_then(|symbols| symbols.lookup(addr));
        match named {
            Some((symbol, offset)) => Frame {
                addr,
                context: Context::Kernel,
                module: None,
                symbol: Some(symbol),
                offset,
            },
            None => Frame::unresolved(addr, Context::Kernel),
        }
    }

    fn user_frame(&mut self, addr: u64) -> Frame {
        let Some(region) = self.region_of(addr) else {
            self.unmapped += 1;
            return Frame::unresolved(addr, Context::User);
        };
        // Copied out of the region before the module cache is touched: the
        // lookup below needs the symbolizer mutably, and the region is borrowed
        // from it.
        let name = region.name.clone();
        let path = region.path.clone();
        // Where in the file the sampled instruction sits. A mapping states its
        // own file offset, so this holds whatever the loader chose to put the
        // object at, which is the whole difference between a symbolized
        // position-independent executable and a page of addresses.
        let file_offset = addr - region.start + region.offset;
        let symbol = path.and_then(|path| {
            self.module(&path)
                .and_then(|module| module.lookup(file_offset))
        });
        match symbol {
            Some((symbol, symbol_offset)) => Frame {
                addr,
                context: Context::User,
                module: Some(name),
                symbol: Some(symbol),
                offset: symbol_offset,
            },
            None => Frame {
                addr,
                context: Context::User,
                module: Some(name),
                symbol: None,
                offset: file_offset,
            },
        }
    }

    /// Whether the function this address is in sets up a frame pointer.
    ///
    /// `None` when there is nothing to read it from -- an address in no
    /// mapping, a file with no symbols, an architecture whose prologue this
    /// build does not recognise -- and `None` is not `false`: a machine that
    /// cannot tell and a target that does not keep them are different answers,
    /// and only the second is a degradation.
    pub fn keeps_frame_pointer(&mut self, addr: u64) -> Option<bool> {
        let region = self.region_of(addr)?;
        let path = region.path.clone()?;
        let file_offset = addr - region.start + region.offset;
        self.module(&path)?.frame_pointer(file_offset)
    }

    /// The executable mapping an address falls in, re-reading the maps once if
    /// nothing covers it.
    ///
    /// A process that loads a library after the first read has mappings the
    /// snapshot never saw, and a profile of a plugin that was `dlopen`ed
    /// halfway through would be all addresses and no names.
    fn region_of(&mut self, addr: u64) -> Option<&Region> {
        let may_reread = !self.remapped && self.failed_rereads < MAX_FAILED_REREADS;
        if may_reread && !self.regions.iter().any(|region| region.contains(addr)) {
            if self.refresh() {
                self.remapped = true;
            } else {
                self.failed_rereads += 1;
            }
        }
        self.regions.iter().find(|region| region.contains(addr))
    }

    fn module(&mut self, path: &Path) -> Option<&Module> {
        self.modules
            .entry(path.to_path_buf())
            .or_insert_with(|| Module::read(path))
            .as_ref()
    }
}

/// One executable mapping of the target.
#[derive(Debug, Clone)]
struct Region {
    start: u64,
    end: u64,
    /// Where in the file the mapping begins.
    offset: u64,
    /// The mapping's name as `maps` writes it, including the bracketed
    /// pseudo-files: `[vdso]` is where a `clock_gettime` frame comes from and
    /// naming it that is more use than an address.
    name: String,
    /// The file to read symbols from, when the name is one.
    path: Option<PathBuf>,
}

impl Region {
    fn contains(&self, addr: u64) -> bool {
        (self.start..self.end).contains(&addr)
    }
}

/// The executable mappings of a process, in address order.
fn read_maps(pid: i32) -> Vec<Region> {
    let who = if pid <= 0 {
        "self".to_owned()
    } else {
        pid.to_string()
    };
    let text = std::fs::read_to_string(format!("/proc/{who}/maps")).unwrap_or_default();
    let mut regions: Vec<Region> = text.lines().filter_map(parse_map_line).collect();
    regions.sort_by_key(|region| region.start);
    regions
}

/// One line of `/proc/<pid>/maps`, kept only when it is executable.
///
/// `7f0b1c000000-7f0b1c028000 r-xp 00001000 00:20 1234 /usr/lib/libc.so.6`
fn parse_map_line(line: &str) -> Option<Region> {
    let mut fields = line.split_whitespace();
    let range = fields.next()?;
    let perms = fields.next()?;
    if !perms.contains('x') {
        return None;
    }
    let offset = u64::from_str_radix(fields.next()?, 16).ok()?;
    let _dev = fields.next()?;
    let _inode = fields.next()?;
    let name = fields.collect::<Vec<&str>>().join(" ");
    let (start, end) = range.split_once('-')?;
    let start = u64::from_str_radix(start, 16).ok()?;
    let end = u64::from_str_radix(end, 16).ok()?;
    // A file that was replaced or deleted while mapped keeps running from the
    // inode the process still holds, but the path no longer names it. The
    // mapping is still worth reporting; its symbols are not readable.
    let path = if name.starts_with('/') && !name.ends_with("(deleted)") {
        Some(PathBuf::from(&name))
    } else {
        None
    };
    Some(Region {
        start,
        end,
        offset,
        name: if name.is_empty() {
            format!("{start:#x}")
        } else {
            name
        },
        path,
    })
}

/// The kernel's own symbol table, read at most once for the whole run.
///
/// `/proc/kallsyms` is 395 318 lines on the development machine, and parsing it
/// took about a second of a debug build. Every sampling collector used to pay
/// that when it built its symbolizer, whether or not a kernel frame was ever
/// looked up: a profile cost a second of wall clock before it sampled anything,
/// and `sched.offcpu --system --emit stack` -- one symbolizer per process that
/// blocked -- did not finish at all.
///
/// So it is read lazily and shared. Lazily, because the table describes the
/// kernel and a run that never symbolizes a kernel frame never needs it:
/// `sched.offcpu` asks the kernel for `BPF_F_USER_STACK` and deliberately never
/// consults it. Shared, because it describes the *machine* rather than any one
/// process, and a second copy of it is a second copy of the same answer.
///
/// `None` means the file could not be read, which is a machine that does not
/// publish it rather than a failure worth retrying: one attempt per run.
static KERNEL_SYMBOLS: std::sync::OnceLock<Option<KernelSymbols>> = std::sync::OnceLock::new();

fn kernel_symbols() -> Option<&'static KernelSymbols> {
    KERNEL_SYMBOLS.get_or_init(KernelSymbols::read).as_ref()
}

/// Pays for the table now, before a window opens, if this run will want it.
///
/// Lazy is right and *lazy inside a drain* is not. The runner reads its
/// collectors in one serial round, so a collector that spends nine hundred
/// milliseconds parsing `/proc/kallsyms` is nine hundred milliseconds in which
/// every other collector of that session is not read -- measured here as a
/// session whose `cpu.oncpu_time` request got one reading of its baseline and
/// nothing else, beside a `cpu.profile` that had to symbolize a kernel frame.
///
/// A fixed cost belongs before the window, which is where the target's own
/// mappings are already read and for the same reason. A collector that will
/// never look at a kernel frame -- `sched.offcpu` asks for `BPF_F_USER_STACK`
/// and deliberately never consults this -- must not call it.
pub fn prime_kernel_symbols() {
    let _ = kernel_symbols();
}

/// The kernel's own symbol table.
struct KernelSymbols {
    /// `(address, name)`, sorted by address.
    symbols: Vec<(u64, String)>,
}

impl KernelSymbols {
    fn read() -> Option<Self> {
        let text = std::fs::read_to_string("/proc/kallsyms").ok()?;
        let mut symbols: Vec<(u64, String)> = text
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                let addr = u64::from_str_radix(fields.next()?, 16).ok()?;
                let kind = fields.next()?;
                // Text symbols only: a data symbol at a lower address than the
                // function an instruction pointer is in would win the search
                // and name the frame after a variable.
                if !matches!(kind, "t" | "T" | "w" | "W") {
                    return None;
                }
                Some((addr, fields.next()?.to_owned()))
            })
            .collect();
        symbols.sort_by_key(|(addr, _)| *addr);
        Some(Self { symbols })
    }

    /// Whether the addresses in the file are real.
    ///
    /// `kptr_restrict` zeroes every one of them rather than refusing the read,
    /// so a usable-looking table resolves the whole kernel to whichever symbol
    /// sorts first.
    fn is_usable(&self) -> bool {
        self.symbols.iter().any(|(addr, _)| *addr != 0)
    }

    fn lookup(&self, addr: u64) -> Option<(String, u64)> {
        let index = self
            .symbols
            .partition_point(|(symbol, _)| *symbol <= addr)
            .checked_sub(1)?;
        let (start, name) = &self.symbols[index];
        Some((name.clone(), addr - start))
    }
}

/// A mapped file's function symbols, by file offset.
struct Module {
    /// `(file offset, size, name)` of every function, sorted by offset.
    ///
    /// Indexed by file offset rather than by virtual address because that is
    /// what a mapping gives directly: a map line says where in the file it
    /// starts, and converting through the load segments once, here, keeps every
    /// lookup a subtraction.
    functions: Vec<Function>,
}

struct Function {
    offset: u64,
    size: u64,
    name: String,
    /// Whether this function's prologue sets up a frame pointer, as far as the
    /// first few bytes of it can say. `None` where the question cannot be
    /// answered on this architecture.
    frame_pointer: Option<bool>,
}

impl Module {
    fn read(path: &Path) -> Option<Self> {
        let bytes = std::fs::read(path).ok()?;
        let elf = Elf::parse(&bytes)?;
        let mut functions = Vec::new();
        // `.symtab` first and `.dynsym` as well: a stripped library has only
        // the second, an unstripped binary has both, and the union names more
        // than either. Duplicates are harmless -- the same address twice is the
        // same answer twice.
        for (symbols, strings) in elf.symbol_tables() {
            for symbol in elf.symbols(symbols, strings) {
                if let Some(offset) = elf.file_offset_of(symbol.value) {
                    functions.push(Function {
                        frame_pointer: sets_up_frame_pointer(&bytes, offset),
                        offset,
                        size: symbol.size,
                        name: symbol.name,
                    });
                }
            }
        }
        if functions.is_empty() {
            return None;
        }
        functions.sort_by(|left, right| {
            left.offset
                .cmp(&right.offset)
                // A sized symbol beats a zero-sized one at the same address:
                // `_start` and a marker label often share an offset, and only
                // one of them bounds anything.
                .then(right.size.cmp(&left.size))
        });
        Some(Self { functions })
    }

    fn lookup(&self, file_offset: u64) -> Option<(String, u64)> {
        let function = self.function_at(file_offset)?;
        Some((function.name.clone(), file_offset - function.offset))
    }

    fn frame_pointer(&self, file_offset: u64) -> Option<bool> {
        self.function_at(file_offset)?.frame_pointer
    }

    fn function_at(&self, file_offset: u64) -> Option<&Function> {
        let index = self
            .functions
            .partition_point(|function| function.offset <= file_offset)
            .checked_sub(1)?;
        let function = &self.functions[index];
        // A symbol with a size bounds itself, and an address past its end is
        // not in it: padding between two functions belongs to neither, and
        // naming it after the one before invents a frame.
        if function.size > 0 && file_offset >= function.offset + function.size {
            return None;
        }
        Some(function)
    }
}

/// Whether the function at this file offset opens by establishing a frame
/// pointer.
///
/// This is what makes the degradation a measurement rather than a suspicion. A
/// frame-pointer walk through a function that never set one up does not stop:
/// it follows whatever the register happened to hold, and on a Rust or C
/// program that is usually the frame of whoever last did set one up -- the C
/// runtime -- so the profile comes back short, plausible, and describing calls
/// that never happened. Nothing in the addresses says so. The prologue does.
///
/// Recognised per architecture, and unrecognised architectures answer `None`
/// rather than `false`.
fn sets_up_frame_pointer(bytes: &[u8], offset: u64) -> Option<bool> {
    let code = bytes.get(offset as usize..)?;
    // Enough of a prologue to recognise one. Less than this is the end of the
    // file, not a function that omits the frame pointer.
    if code.len() < 8 {
        return None;
    }
    let code = &code[..code.len().min(16)];
    #[cfg(target_arch = "x86_64")]
    {
        // `endbr64` comes first where the kernel enforces indirect-branch
        // tracking, and is not part of the answer.
        let code = match code {
            [0xf3, 0x0f, 0x1e, 0xfa, rest @ ..] => rest,
            rest => rest,
        };
        // push %rbp ; mov %rsp,%rbp
        Some(matches!(code, [0x55, 0x48, 0x89, 0xe5, ..]))
    }
    #[cfg(target_arch = "aarch64")]
    {
        // `mov x29, sp`, which follows the `stp x29, x30` that saved them.
        // Within the first four instructions: a prologue may allocate first.
        Some(
            code.chunks_exact(4)
                .take(4)
                .any(|word| word == [0xfd, 0x03, 0x00, 0x91]),
        )
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        let _ = code;
        None
    }
}

/// Just enough ELF to find a function name.
///
/// Not a general reader: it looks at the load segments and the symbol tables
/// and nothing else. A crate would do more, and this is the whole of what is
/// needed to turn an address into a name for a file that carries symbols.
struct Elf<'a> {
    bytes: &'a [u8],
    sections: Vec<Section>,
    /// `(p_offset, p_filesz, p_vaddr)` of every `PT_LOAD`.
    loads: Vec<(u64, u64, u64)>,
}

#[derive(Debug, Clone, Copy)]
struct Section {
    kind: u32,
    offset: u64,
    size: u64,
    link: u32,
    entry_size: u64,
}

struct ElfSymbol {
    value: u64,
    size: u64,
    name: String,
}

const SHT_SYMTAB: u32 = 2;
const SHT_DYNSYM: u32 = 11;
const PT_LOAD: u32 = 1;
const STT_FUNC: u8 = 2;

impl<'a> Elf<'a> {
    fn parse(bytes: &'a [u8]) -> Option<Self> {
        // 64-bit, little-endian only. A big-endian or 32-bit object on this
        // machine would be something the target could not be running, and
        // guessing at its layout would produce names, not correct ones.
        if bytes.len() < 64 || &bytes[0..4] != b"\x7fELF" || bytes[4] != 2 || bytes[5] != 1 {
            return None;
        }
        let phoff = read_u64(bytes, 32)?;
        let shoff = read_u64(bytes, 40)?;
        let phentsize = read_u16(bytes, 54)? as usize;
        let phnum = read_u16(bytes, 56)? as usize;
        let shentsize = read_u16(bytes, 58)? as usize;
        let shnum = read_u16(bytes, 60)? as usize;

        let mut loads = Vec::new();
        for index in 0..phnum {
            let at = phoff as usize + index * phentsize;
            if read_u32(bytes, at)? != PT_LOAD {
                continue;
            }
            loads.push((
                read_u64(bytes, at + 8)?,
                read_u64(bytes, at + 32)?,
                read_u64(bytes, at + 16)?,
            ));
        }

        let mut sections = Vec::with_capacity(shnum);
        for index in 0..shnum {
            let at = shoff as usize + index * shentsize;
            sections.push(Section {
                kind: read_u32(bytes, at + 4)?,
                offset: read_u64(bytes, at + 24)?,
                size: read_u64(bytes, at + 32)?,
                link: read_u32(bytes, at + 40)?,
                entry_size: read_u64(bytes, at + 56)?,
            });
        }
        Some(Self {
            bytes,
            sections,
            loads,
        })
    }

    /// Every symbol table, with the string table it names its symbols from.
    fn symbol_tables(&self) -> Vec<(Section, Section)> {
        self.sections
            .iter()
            .filter(|section| matches!(section.kind, SHT_SYMTAB | SHT_DYNSYM))
            .filter_map(|section| {
                let strings = *self.sections.get(section.link as usize)?;
                Some((*section, strings))
            })
            .collect()
    }

    fn symbols(&self, table: Section, strings: Section) -> Vec<ElfSymbol> {
        if table.entry_size == 0 {
            return Vec::new();
        }
        let count = (table.size / table.entry_size) as usize;
        (0..count)
            .filter_map(|index| {
                let at = table.offset as usize + index * table.entry_size as usize;
                let info = *self.bytes.get(at + 4)?;
                if info & 0xf != STT_FUNC {
                    return None;
                }
                let value = read_u64(self.bytes, at + 8)?;
                if value == 0 {
                    // An undefined symbol: the name of something this file
                    // calls, not of anything it contains.
                    return None;
                }
                let name = read_str(
                    self.bytes,
                    strings.offset as usize + read_u32(self.bytes, at)? as usize,
                )?;
                Some(ElfSymbol {
                    value,
                    size: read_u64(self.bytes, at + 16)?,
                    name,
                })
            })
            .collect()
    }

    /// Where in the file a virtual address lives.
    ///
    /// This is the conversion that makes a position-independent executable
    /// symbolizable at all: the mapping gives a file offset, the symbol table
    /// gives a virtual address, and the load segments are the only thing that
    /// relates the two.
    fn file_offset_of(&self, vaddr: u64) -> Option<u64> {
        self.loads
            .iter()
            .find(|(_, filesz, addr)| (*addr..addr + filesz).contains(&vaddr))
            .map(|(offset, _, addr)| vaddr - addr + offset)
    }
}

fn read_u16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn read_u64(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

fn read_str(bytes: &[u8], at: usize) -> Option<String> {
    let rest = bytes.get(at..)?;
    let end = rest.iter().position(|byte| *byte == 0)?;
    std::str::from_utf8(&rest[..end]).ok().map(str::to_owned)
}

/// What the kernel should be asked to do for an unwind mode.
///
/// Two of the four modes are not implemented, and both say so. Falling back to
/// frame pointers when `dwarf` was asked for would answer a different question
/// than the one that was put: a program without frame pointers is exactly the
/// program someone asks for `dwarf` about, and the fallback would report its
/// leaf frames as a profile.
pub fn callchain_for(mode: StackMode) -> Result<Callchain, String> {
    match mode {
        StackMode::Fp => Ok(Callchain::FramePointers),
        StackMode::None => Ok(Callchain::Off),
        StackMode::Dwarf => Err(
            "`--stack dwarf` is not implemented: it needs a real unwinder (blazesym or libdw) \
             reading the target's debug information, and frame pointers instead would report \
             the leaf frames of a program that has none as though they were its stacks"
                .to_owned(),
        ),
        StackMode::Lbr => Err(
            "`--stack lbr` is not implemented: it needs the branch stack, which is a different \
             sampling shape and hardware this build does not probe for"
                .to_owned(),
        ),
    }
}

/// Captures stacks for one run: the table, the symbols, and what it could not
/// do.
///
/// A collector holds one of these and hands it every sample. What it gives back
/// is an id to put on the event; what it keeps is the table to put beside the
/// results, and the counts behind [`Unwinder::notes`].
pub struct Unwinder {
    table: StackTable,
    symbols: Symbolizer,
    samples: u64,
    /// Samples whose user half was a single frame: a walk that could not take
    /// even its first step.
    single_frame: u64,
    /// Samples whose leaf function sets up no frame pointer, so the frames
    /// under it are whatever the register held and not its callers.
    unwalkable_leaf: u64,
    /// Samples whose leaf function does set one up. Counted beside the others
    /// because the note below is about the share, and a target that is half
    /// instrumented is a different statement from one that is not at all.
    walkable_leaf: u64,
    /// Samples that arrived with no callchain at all.
    empty: u64,
    /// Frames the kernel attributed to itself, over every sample.
    ///
    /// Counted so that the note about `kptr_restrict` is only made by a run
    /// that has kernel frames to lose names from. It is also what keeps
    /// `/proc/kallsyms` unread by a run that never needed it: asking whether
    /// the kernel's symbols are readable is what parses them, and a profile
    /// used to ask at the end whatever it had collected -- a second of wall
    /// clock, flat, for an answer about frames it did not have.
    kernel_frames: u64,
}

impl Unwinder {
    pub fn for_process(pid: i32) -> Self {
        Self {
            table: StackTable::new(),
            symbols: Symbolizer::for_process(pid),
            samples: 0,
            single_frame: 0,
            unwalkable_leaf: 0,
            walkable_leaf: 0,
            empty: 0,
            kernel_frames: 0,
        }
    }

    /// Interns the sample's stack and returns the id to put on its event.
    ///
    /// `None` when the sample carries no callchain, which is what every sample
    /// of an event opened with [`Callchain::Off`] looks like.
    pub fn record(&mut self, sample: &Sample) -> Option<StackId> {
        let callchain = sample.callchain.as_deref()?;
        self.samples += 1;
        let raw = RawStack::split(callchain, sample.cpu_mode);
        self.kernel_frames += raw.kernel.len() as u64;
        if raw.is_empty() {
            self.empty += 1;
        } else if raw.user.len() == 1 {
            self.single_frame += 1;
        }
        if let Some(&leaf) = raw.user.first() {
            match self.symbols.keeps_frame_pointer(leaf) {
                Some(true) => self.walkable_leaf += 1,
                Some(false) => self.unwalkable_leaf += 1,
                None => {}
            }
        }
        Some(
            self.table
                .intern(callchain, sample.cpu_mode, &mut self.symbols),
        )
    }

    /// Re-reads the target's mappings, keeping the ones it has when there are
    /// none to be had.
    ///
    /// Called once per tick by the collectors that follow a process, because
    /// the moment an unwinder is built is not a moment its target's maps are
    /// worth reading: a launched program has not execed yet at `attach`, and by
    /// the time its last samples are drained it has often exited. See
    /// [`Symbolizer::refresh`].
    pub fn refresh_symbols(&mut self) -> bool {
        self.symbols.refresh()
    }

    pub fn table(&self) -> &StackTable {
        &self.table
    }

    pub fn into_table(self) -> StackTable {
        self.table
    }

    /// The share of samples whose leaf function could be walked out of.
    ///
    /// `None` when nothing could be read either way, which is the answer on an
    /// architecture whose prologue this build does not recognise.
    pub fn walkable_share(&self) -> Option<f64> {
        let known = self.walkable_leaf + self.unwalkable_leaf;
        (known > 0).then(|| self.walkable_leaf as f64 / known as f64)
    }

    /// What this capture gave up, in the words a fallback backend uses.
    ///
    /// The one worth reading twice is the frame-pointer note. A profile taken
    /// of a target built without them does not come back empty: it comes back
    /// short and confident, naming callers that never called anything, because
    /// the walk followed a register holding something else. So it is reported
    /// as a degradation of the measurement, which is what it is, rather than
    /// left to be noticed in a flame graph that looks fine.
    pub fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if let Some(share) = self.walkable_share() {
            if share < 0.5 {
                notes.push(format!(
                    "{} of {} samples landed in a function that sets up no frame pointer, so the \
                     frames under the leaf are not its callers: this target was built without \
                     them and what is here is a profile of leaf functions",
                    self.unwalkable_leaf,
                    self.walkable_leaf + self.unwalkable_leaf
                ));
            }
        }
        if self.samples > 0 && self.single_frame * 2 > self.samples {
            notes.push(format!(
                "{} of {} samples unwound to a single user frame: there was no chain to walk at \
                 all",
                self.single_frame, self.samples
            ));
        }
        if self.empty > 0 {
            notes.push(format!(
                "{} of {} samples carried no frame at all",
                self.empty, self.samples
            ));
        }
        if self.symbols.unmapped() > 0 {
            notes.push(format!(
                "{} frames fell in no mapping of the target and are reported by address",
                self.symbols.unmapped()
            ));
        }
        // Asked only of a run that walked into the kernel. The answer costs a
        // parse of `/proc/kallsyms`, and a run with no kernel frames is not
        // degraded by names it was never going to print.
        if self.kernel_frames > 0 && !self.symbols.kernel_symbols_available() {
            notes.push(
                "kernel addresses are hidden by `kernel.kptr_restrict`, so kernel frames carry \
                 no names"
                    .to_owned(),
            );
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A callchain as the kernel writes one: a marker, the frames that belong
    /// to it, another marker, the frames that belong to that.
    #[test]
    fn a_callchain_is_split_where_the_kernel_put_its_markers() {
        let chain = [
            PERF_CONTEXT_KERNEL,
            0xffff_8000_0000_1000,
            0xffff_8000_0000_2000,
            PERF_CONTEXT_USER,
            0x5555_0000_1000,
            0x5555_0000_2000,
        ];
        let stack = RawStack::split(&chain, CpuMode::Kernel);
        assert_eq!(stack.kernel, [0xffff_8000_0000_1000, 0xffff_8000_0000_2000]);
        assert_eq!(stack.user, [0x5555_0000_1000, 0x5555_0000_2000]);
        assert!(stack.elsewhere.is_empty());
        // The markers are not frames, so they are not counted as depth.
        assert_eq!(stack.depth(), 4);
    }

    /// Frames ahead of the first marker belong to the mode the sample was taken
    /// in. Filing them by guess is how a kernel address ends up looked up in a
    /// process's symbol table and named after an unrelated function.
    #[test]
    fn frames_before_the_first_marker_belong_to_the_mode_the_sample_was_taken_in() {
        let chain = [0x1000, PERF_CONTEXT_USER, 0x2000];

        let from_kernel = RawStack::split(&chain, CpuMode::Kernel);
        assert_eq!(from_kernel.kernel, [0x1000]);
        assert_eq!(from_kernel.user, [0x2000]);

        let from_user = RawStack::split(&chain, CpuMode::User);
        assert!(from_user.kernel.is_empty());
        assert_eq!(from_user.user, [0x1000, 0x2000]);

        // And a mode the header did not name is not guessed at: those frames
        // are set aside, counted, and reported without a side.
        let unknown = RawStack::split(&chain, CpuMode::Unknown);
        assert_eq!(unknown.elsewhere, [0x1000]);
        assert_eq!(unknown.user, [0x2000]);
        assert_eq!(unknown.depth(), 2);
    }

    /// A marker for a context this tool does not model -- a hypervisor, a guest
    /// -- must not silently take the frames under it with it.
    #[test]
    fn frames_in_a_context_this_tool_does_not_model_are_kept_rather_than_dropped() {
        let chain = [
            PERF_CONTEXT_USER,
            0x1000,
            perf_event_open_sys::bindings::PERF_CONTEXT_GUEST,
            0x2000,
        ];
        let stack = RawStack::split(&chain, CpuMode::User);
        assert_eq!(stack.user, [0x1000]);
        assert_eq!(stack.elsewhere, [0x2000]);
        assert_eq!(stack.depth(), 2);
    }

    #[test]
    fn a_stack_reads_root_first_the_way_a_person_reads_one() {
        let frame = |name: &str, context| Frame {
            addr: 0x1000,
            context,
            module: Some("/usr/lib/libc.so.6".to_owned()),
            symbol: Some(name.to_owned()),
            offset: 0x20,
        };
        let stack = Stack {
            frames: vec![
                frame("schedule", Context::Kernel),
                frame("read", Context::User),
                frame("main", Context::User),
            ],
        };
        assert_eq!(stack.to_string(), "main;read;schedule");
        assert_eq!(stack.frames_in(Context::Kernel).count(), 1);
        assert_eq!(
            frame("read", Context::User).to_string(),
            "read+0x20 (libc.so.6)"
        );
    }

    /// An address with no name is still an address. What it must never be is a
    /// name: a renderer that printed the nearest symbol regardless of distance
    /// would attribute a stripped library's whole text to whatever function
    /// happens to be exported first.
    #[test]
    fn a_frame_with_no_symbol_is_reported_by_its_offset_into_the_file() {
        let frame = Frame {
            addr: 0x7f00_0000_1234,
            context: Context::User,
            module: Some("/usr/lib/libfoo.so".to_owned()),
            symbol: None,
            offset: 0x1234,
        };
        assert_eq!(frame.label(), "libfoo.so+0x1234");
        assert_eq!(frame.to_string(), "+0x1234 (libfoo.so)");

        let nowhere = Frame::unresolved(0xdead_beef, Context::Kernel);
        assert_eq!(nowhere.label(), "0xdeadbeef");
        assert_eq!(nowhere.to_string(), "0xdeadbeef ([kernel])");
    }

    #[test]
    fn a_map_line_is_read_only_when_it_holds_code() {
        let executable =
            parse_map_line("7f0b1c000000-7f0b1c028000 r-xp 00001000 00:20 1234 /usr/lib/libc.so.6")
                .expect("an executable mapping");
        assert_eq!(executable.start, 0x7f0b_1c00_0000);
        assert_eq!(executable.end, 0x7f0b_1c02_8000);
        assert_eq!(executable.offset, 0x1000);
        assert_eq!(
            executable.path.as_deref(),
            Some(Path::new("/usr/lib/libc.so.6"))
        );
        assert!(executable.contains(0x7f0b_1c00_0001));
        assert!(!executable.contains(0x7f0b_1c02_8000));

        // Data is not code, and an instruction pointer never falls in it.
        assert!(parse_map_line(
            "7f0b1c029000-7f0b1c02a000 rw-p 00028000 00:20 1234 /usr/lib/libc.so.6"
        )
        .is_none());

        // A mapping with no file behind it is still where an address is, and
        // `[vdso]` is a name worth more than a number.
        let vdso = parse_map_line("7ffd1b9f4000-7ffd1b9f6000 r-xp 00000000 00:00 0 [vdso]")
            .expect("the vdso is executable");
        assert_eq!(vdso.name, "[vdso]");
        assert!(vdso.path.is_none());

        // A file replaced while mapped keeps running from an inode the path no
        // longer names, so its symbols must not be read from whatever is there
        // now.
        let replaced =
            parse_map_line("400000-401000 r-xp 00000000 00:20 99 /tmp/prog (deleted)").unwrap();
        assert!(replaced.path.is_none());
        assert_eq!(replaced.name, "/tmp/prog (deleted)");
    }

    /// The mappings a symbolizer holds must survive the process they describe.
    ///
    /// Reading `/proc/<pid>/maps` of a process that has exited returns nothing,
    /// and nothing is not "this process had no mappings": it is the file being
    /// gone. Adopting it throws away the only snapshot a launched program's
    /// stacks can be resolved against -- and the drain that carries the last
    /// block of a program that blocks and then exits happens after it is gone,
    /// which is exactly when this is asked.
    #[test]
    fn a_symbolizer_keeps_the_mappings_it_has_when_the_target_is_gone() {
        let Ok(mut child) = std::process::Command::new("/bin/sleep").arg("30").spawn() else {
            eprintln!("skipped: this machine has no `/bin/sleep` to read the mappings of");
            return;
        };
        let mut symbols = Symbolizer::for_process(child.id() as i32);
        assert!(
            symbols.refresh(),
            "a live process has executable mappings to read"
        );
        let held = symbols.regions.len();
        assert!(held > 0, "a running program maps at least its own text");

        let _ = child.kill();
        let _ = child.wait();
        assert!(
            !symbols.refresh(),
            "a process that has been reaped reported mappings"
        );
        assert_eq!(
            symbols.regions.len(),
            held,
            "the mappings read while the target lived were replaced by the empty read that \
             followed its exit"
        );
    }

    /// `kptr_restrict` publishes the file with every address zeroed rather than
    /// refusing it, so a table that reads fine can still name the whole kernel
    /// after one symbol.
    #[test]
    fn a_kallsyms_whose_addresses_are_hidden_is_not_a_usable_one() {
        let hidden = KernelSymbols {
            symbols: vec![(0, "_stext".to_owned()), (0, "schedule".to_owned())],
        };
        assert!(!hidden.is_usable());

        let real = KernelSymbols {
            symbols: vec![
                (0xffff_ffff_8100_0000, "_stext".to_owned()),
                (0xffff_ffff_8100_1000, "schedule".to_owned()),
            ],
        };
        assert!(real.is_usable());
        assert_eq!(
            real.lookup(0xffff_ffff_8100_1010),
            Some(("schedule".to_owned(), 0x10))
        );
        // Below the first symbol there is nothing to name it with.
        assert_eq!(real.lookup(0xffff_ffff_80ff_ffff), None);
    }

    /// The prologue is what makes the frame-pointer degradation a measurement.
    #[test]
    fn a_prologue_is_read_for_what_it_does_with_the_frame_pointer() {
        if cfg!(not(target_arch = "x86_64")) {
            return;
        }
        // push %rbp ; mov %rsp,%rbp
        assert_eq!(
            sets_up_frame_pointer(&[0x55, 0x48, 0x89, 0xe5, 0x90, 0x90, 0x90, 0x90], 0),
            Some(true)
        );
        // The same, behind the indirect-branch-tracking landing pad.
        assert_eq!(
            sets_up_frame_pointer(&[0xf3, 0x0f, 0x1e, 0xfa, 0x55, 0x48, 0x89, 0xe5], 0),
            Some(true)
        );
        // sub $0x48,%rsp: a frame, but not a frame pointer.
        assert_eq!(
            sets_up_frame_pointer(&[0x48, 0x83, 0xec, 0x48, 0x90, 0x90, 0x90, 0x90], 0),
            Some(false)
        );
        // Past the end of the file there is nothing to read, and nothing to
        // read is not "no frame pointer".
        assert_eq!(sets_up_frame_pointer(&[0x55, 0x48], 0), None);
        assert_eq!(sets_up_frame_pointer(&[0u8; 32], 30), None);
    }

    /// The table exists so a stack seen ten thousand times is symbolized once.
    #[test]
    fn a_table_read_back_out_of_a_result_file_holds_what_was_put_in_it() {
        let stack = Stack {
            frames: vec![Frame::unresolved(0x1000, Context::User)],
        };
        let table = StackTable::from(vec![stack.clone()]);
        let json = serde_json::to_string(&table).expect("serialize");
        // It serializes as its stacks and nothing else: the intern index is a
        // way of building the table, not part of what it is.
        assert!(json.starts_with('['), "the table serialized as {json}");
        let read: StackTable = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(read.get(StackId(0)), Some(&stack));
        assert_eq!(read.len(), 1);
    }
}
