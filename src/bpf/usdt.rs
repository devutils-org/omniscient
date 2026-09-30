// SPDX-License-Identifier: MIT OR GPL-2.0
//! USDT: the probes a program was built to publish, found in its own ELF notes.
//!
//! A statically defined tracepoint in userspace is a `nop` the compiler left at
//! the probe site, plus a note in `.note.stapsdt` saying where the `nop` is,
//! what the probe is called, and where each of its arguments lives at that
//! instruction. Nothing runs there until something puts a breakpoint on the
//! `nop`, which is what a uprobe is -- so attaching one is `attach::uprobe` at
//! the right offset, and everything this file adds is *finding* the offset and
//! reading the arguments once the probe fires.
//!
//! Three things are read per probe:
//!
//! - **The location**, a virtual address in the object. The kernel wants a file
//!   offset, so it goes through the `PT_LOAD` segment that maps it. If the
//!   object was prelinked, the note's recorded base differs from where
//!   `.stapsdt.base` actually is, and the difference is added first.
//! - **The semaphore**, when the probe has one: a two-byte counter in the
//!   object's data that the program reads before doing the work of preparing
//!   the probe's arguments. Zero means nobody is listening, and a guarded probe
//!   then never reaches its `nop` at all. The kernel maintains it, through
//!   the link's `ref_ctr_offsets`; see "the attach" below.
//! - **The argument descriptors**: `-4@%eax`, `8@-80(%rbx)`, `8@$-7`. A size
//!   (negative for signed), and an operand in the assembler's own syntax --
//!   a register, an immediate, or memory at an offset from a register.
//!
//! **This is not the cheap path, and nothing here pretends it is.** A uprobe
//! is a breakpoint trap into the kernel on every hit: finding 6 of
//! `docs/open-findings.md` measured 740 ns per event against 132 ns for the
//! preload shim on the same event, in isolation; with its shared ring the shim
//! writes a record in about 70 ns (finding 47). What USDT buys is reach -- a
//! program this
//! tool did not launch and cannot preload into -- and names: the program says
//! which events it has, rather than having them guessed from symbol names.
//!
//! **Only x86-64 little-endian ELF64 is read.** Argument descriptors are in the
//! target architecture's assembler syntax, and the registers they name are
//! fields of that architecture's `pt_regs`. Anything else is refused by name
//! rather than parsed as if it were x86.

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};

use super::attach::{self, PerfAttachment, Scope};
use super::btf::Btf;
use super::insn::{self, helper, Insn, Program, BPF_DW, BPF_W, R10};
use super::map::{map_failure, Counters};
use super::ring::{ring_bytes, RingBuffer};
use super::syscall::{self, BPF_MAP_TYPE_RINGBUF};
use super::Support;

// ------------------------------------------------------------------ the ELF

const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const EM_X86_64: u16 = 62;
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;
const SHT_NOTE: u32 = 7;
const NT_STAPSDT: u32 = 3;
const DT_NULL: u64 = 0;
const DT_NEEDED: u64 = 1;
const DT_STRTAB: u64 = 5;
const DT_RPATH: u64 = 15;
const DT_RUNPATH: u64 = 29;

#[derive(Debug, Clone)]
struct Section {
    name: String,
    kind: u32,
    addr: u64,
    offset: u64,
    size: u64,
}

#[derive(Debug, Clone, Copy)]
struct Segment {
    kind: u32,
    offset: u64,
    vaddr: u64,
    filesz: u64,
}

/// An ELF object, read whole.
///
/// Objects are read once per run and the largest one a runtime maps is tens of
/// megabytes, so reading the file into memory is simpler than seeking around it
/// and costs nothing that matters at the moment it is paid: before the target
/// runs.
pub struct Object {
    path: PathBuf,
    data: Vec<u8>,
    sections: Vec<Section>,
    segments: Vec<Segment>,
}

impl Object {
    /// Reads an object, refusing anything this file cannot read correctly.
    pub fn read(path: &Path) -> Result<Self, String> {
        let data =
            fs::read(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        Self::parse(path, data)
    }

    fn parse(path: &Path, data: Vec<u8>) -> Result<Self, String> {
        let shown = path.display();
        if data.len() < 64 || &data[..4] != b"\x7fELF" {
            return Err(format!("{shown} is not an ELF object"));
        }
        if data[4] != ELFCLASS64 || data[5] != ELFDATA2LSB {
            return Err(format!(
                "{shown} is not a 64-bit little-endian ELF object, and USDT argument descriptors \
                 are read here for x86-64 only"
            ));
        }
        let machine = u16::from_le_bytes([data[18], data[19]]);
        if machine != EM_X86_64 {
            return Err(format!(
                "{shown} is built for ELF machine {machine}, not x86-64 (62). Its probes' argument \
                 descriptors name another architecture's registers, and reading them as x86 would \
                 produce numbers rather than an error"
            ));
        }
        let mut object = Self {
            path: path.to_owned(),
            data,
            sections: Vec::new(),
            segments: Vec::new(),
        };
        object.segments = object.read_segments()?;
        object.sections = object.read_sections()?;
        Ok(object)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn u16_at(&self, at: u64) -> Option<u16> {
        let at = usize::try_from(at).ok()?;
        Some(u16::from_le_bytes(
            self.data.get(at..at + 2)?.try_into().ok()?,
        ))
    }

    fn u32_at(&self, at: u64) -> Option<u32> {
        let at = usize::try_from(at).ok()?;
        Some(u32::from_le_bytes(
            self.data.get(at..at + 4)?.try_into().ok()?,
        ))
    }

    fn u64_at(&self, at: u64) -> Option<u64> {
        let at = usize::try_from(at).ok()?;
        Some(u64::from_le_bytes(
            self.data.get(at..at + 8)?.try_into().ok()?,
        ))
    }

    fn cstr_at(&self, at: u64) -> Option<String> {
        let at = usize::try_from(at).ok()?;
        let rest = self.data.get(at..)?;
        let end = rest.iter().position(|byte| *byte == 0)?;
        Some(String::from_utf8_lossy(&rest[..end]).into_owned())
    }

    fn truncated(&self, what: &str) -> String {
        format!(
            "{} is truncated: its {what} runs past the end of the file",
            self.path.display()
        )
    }

    fn read_segments(&self) -> Result<Vec<Segment>, String> {
        let phoff = self.u64_at(32).ok_or_else(|| self.truncated("header"))?;
        let phentsize = u64::from(self.u16_at(54).ok_or_else(|| self.truncated("header"))?);
        let phnum = u64::from(self.u16_at(56).ok_or_else(|| self.truncated("header"))?);
        (0..phnum)
            .map(|index| {
                let at = phoff + index * phentsize;
                Some(Segment {
                    kind: self.u32_at(at)?,
                    offset: self.u64_at(at + 8)?,
                    vaddr: self.u64_at(at + 16)?,
                    filesz: self.u64_at(at + 32)?,
                })
            })
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| self.truncated("program header table"))
    }

    fn read_sections(&self) -> Result<Vec<Section>, String> {
        let shoff = self.u64_at(40).ok_or_else(|| self.truncated("header"))?;
        let shentsize = u64::from(self.u16_at(58).ok_or_else(|| self.truncated("header"))?);
        let shnum = u64::from(self.u16_at(60).ok_or_else(|| self.truncated("header"))?);
        let shstrndx = u64::from(self.u16_at(62).ok_or_else(|| self.truncated("header"))?);
        if shoff == 0 || shnum == 0 {
            // A stripped-to-the-bone object may carry no section table at all.
            // It can still run, and it carries no notes this file can find.
            return Ok(Vec::new());
        }
        let raw: Vec<(u32, u32, u64, u64, u64)> = (0..shnum)
            .map(|index| {
                let at = shoff + index * shentsize;
                Some((
                    self.u32_at(at)?,
                    self.u32_at(at + 4)?,
                    self.u64_at(at + 16)?,
                    self.u64_at(at + 24)?,
                    self.u64_at(at + 32)?,
                ))
            })
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| self.truncated("section header table"))?;
        let names_at = raw
            .get(shstrndx as usize)
            .map(|(_, _, _, offset, _)| *offset)
            .ok_or_else(|| self.truncated("section name table"))?;
        Ok(raw
            .into_iter()
            .map(|(name, kind, addr, offset, size)| Section {
                name: self.cstr_at(names_at + u64::from(name)).unwrap_or_default(),
                kind,
                addr,
                offset,
                size,
            })
            .collect())
    }

    fn section(&self, name: &str) -> Option<&Section> {
        self.sections.iter().find(|section| section.name == name)
    }

    /// The file offset a virtual address is loaded from, through the `PT_LOAD`
    /// segment that maps it. `None` for an address no segment maps from the
    /// file -- `.bss`, or nonsense.
    pub fn file_offset(&self, vaddr: u64) -> Option<u64> {
        self.segments
            .iter()
            .filter(|segment| segment.kind == PT_LOAD)
            .find(|segment| vaddr >= segment.vaddr && vaddr < segment.vaddr + segment.filesz)
            .map(|segment| vaddr - segment.vaddr + segment.offset)
    }

    /// Every USDT note in the object, with its location and semaphore already
    /// adjusted for prelinking.
    pub fn notes(&self) -> Result<Vec<Note>, String> {
        let Some(section) = self
            .sections
            .iter()
            .find(|section| section.name == ".note.stapsdt" && section.kind == SHT_NOTE)
        else {
            return Ok(Vec::new());
        };
        // The note records where `.stapsdt.base` was when it was linked. If
        // the object was prelinked since, the section moved and every address
        // in every note moved with it by the same amount.
        let actual_base = self.section(".stapsdt.base").map(|base| base.addr);

        let mut notes = Vec::new();
        let mut at = section.offset;
        let end = section.offset + section.size;
        while at + 12 <= end {
            let truncated = || self.truncated("`.note.stapsdt` section");
            let namesz = u64::from(self.u32_at(at).ok_or_else(truncated)?);
            let descsz = u64::from(self.u32_at(at + 4).ok_or_else(truncated)?);
            let kind = self.u32_at(at + 8).ok_or_else(truncated)?;
            let name_at = at + 12;
            let desc_at = name_at + align4(namesz);
            let next = desc_at + align4(descsz);
            if next > end {
                return Err(truncated());
            }
            let owner = self.cstr_at(name_at).unwrap_or_default();
            if owner == "stapsdt" && kind == NT_STAPSDT && descsz >= 24 {
                notes.push(self.note(desc_at, descsz, actual_base)?);
            }
            at = next;
        }
        Ok(notes)
    }

    fn note(&self, at: u64, size: u64, actual_base: Option<u64>) -> Result<Note, String> {
        let truncated = || self.truncated("USDT note");
        let pc = self.u64_at(at).ok_or_else(truncated)?;
        let recorded_base = self.u64_at(at + 8).ok_or_else(truncated)?;
        let semaphore = self.u64_at(at + 16).ok_or_else(truncated)?;
        let strings_end = usize::try_from(at + size).map_err(|_| truncated())?;
        let strings_at = usize::try_from(at + 24).map_err(|_| truncated())?;
        let strings = self
            .data
            .get(strings_at..strings_end)
            .ok_or_else(truncated)?;
        let mut parts = strings
            .split(|byte| *byte == 0)
            .map(|part| String::from_utf8_lossy(part).into_owned());
        let provider = parts.next().unwrap_or_default();
        let name = parts.next().unwrap_or_default();
        let arguments = parts.next().unwrap_or_default();

        let shift = actual_base.map_or(0, |actual| actual.wrapping_sub(recorded_base));
        Ok(Note {
            provider,
            name,
            pc: pc.wrapping_add(shift),
            semaphore: (semaphore != 0).then(|| semaphore.wrapping_add(shift)),
            arguments,
        })
    }

    /// What the object carries, in words, for a refusal that has to say what
    /// the ELF actually contained rather than only that it lacked something.
    pub fn describe(&self) -> String {
        let shown = self.path.display();
        match self.notes() {
            Err(error) => error,
            Ok(notes) if notes.is_empty() => {
                if self.sections.is_empty() {
                    format!("{shown}: no section table, so no `.note.stapsdt` to read")
                } else {
                    format!(
                        "{shown}: {} sections and no `.note.stapsdt` -- built without USDT probes",
                        self.sections.len()
                    )
                }
            }
            Ok(notes) => {
                // Every name, not the first few: this is what a reader writes
                // a filter from, and a list cut at eight hides the ninth.
                let names: BTreeSet<String> = notes.iter().map(Note::qualified).collect();
                format!(
                    "{shown}: {} probe site{} under {} name{} ({})",
                    notes.len(),
                    plural(notes.len()),
                    names.len(),
                    plural(names.len()),
                    names.into_iter().collect::<Vec<_>>().join(", ")
                )
            }
        }
    }

    /// The libraries the dynamic linker will load for this object, by the
    /// names it asks for, and the search paths the object itself names.
    pub fn dynamic(&self) -> Dynamic {
        let mut dynamic = Dynamic::default();
        if let Some(interp) = self
            .segments
            .iter()
            .find(|segment| segment.kind == PT_INTERP)
        {
            dynamic.interpreter = self.cstr_at(interp.offset).map(PathBuf::from);
        }
        let Some(segment) = self
            .segments
            .iter()
            .find(|segment| segment.kind == PT_DYNAMIC)
        else {
            return dynamic;
        };
        let mut entries = Vec::new();
        let mut at = segment.offset;
        while at + 16 <= segment.offset + segment.filesz {
            let (Some(tag), Some(value)) = (self.u64_at(at), self.u64_at(at + 8)) else {
                break;
            };
            if tag == DT_NULL {
                break;
            }
            entries.push((tag, value));
            at += 16;
        }
        let Some(strtab) = entries
            .iter()
            .find(|(tag, _)| *tag == DT_STRTAB)
            .and_then(|(_, vaddr)| self.file_offset(*vaddr))
        else {
            return dynamic;
        };
        for (tag, value) in entries {
            let text = || self.cstr_at(strtab + value).unwrap_or_default();
            match tag {
                DT_NEEDED => dynamic.needed.push(text()),
                DT_RPATH | DT_RUNPATH => dynamic.search.extend(
                    text()
                        .split(':')
                        .filter(|part| !part.is_empty())
                        .map(str::to_owned),
                ),
                _ => {}
            }
        }
        dynamic
    }
}

/// What the dynamic section of an object asks the linker for.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Dynamic {
    pub interpreter: Option<PathBuf>,
    pub needed: Vec<String>,
    /// `DT_RUNPATH` and `DT_RPATH`, unexpanded: `$ORIGIN` is the caller's.
    pub search: Vec<String>,
}

fn align4(value: u64) -> u64 {
    (value + 3) & !3
}

fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

/// One `.note.stapsdt` entry, as the object records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub provider: String,
    pub name: String,
    /// The probe site, as a virtual address in the object.
    pub pc: u64,
    /// The semaphore, as a virtual address in the object, when there is one.
    pub semaphore: Option<u64>,
    /// The argument descriptors, verbatim.
    pub arguments: String,
}

impl Note {
    /// `provider:name`, the way every tool that reads these spells a probe.
    pub fn qualified(&self) -> String {
        format!("{}:{}", self.provider, self.name)
    }
}

// ------------------------------------------------------ argument descriptors

/// Where one of a probe's arguments is, at the probe site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operand {
    /// A register, as the `pt_regs` member that holds it and the byte its
    /// value starts at -- one for `%ah` and its siblings, zero otherwise.
    Register {
        member: &'static str,
        width: u8,
        shift: u8,
    },
    /// A constant the compiler folded into the descriptor.
    Immediate(i64),
    /// User memory at a register plus a constant.
    Memory { base: &'static str, offset: i64 },
    /// An operand this build cannot read, with why. The probe still attaches;
    /// this argument is reported as unread on every hit rather than invented.
    Unreadable(String),
}

/// One argument descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arg {
    /// In bytes: 1, 2, 4 or 8.
    pub size: u8,
    pub signed: bool,
    pub operand: Operand,
    /// The descriptor as the note spelled it, for anything that reports it.
    pub text: String,
}

/// How many arguments a record carries. `sys/sdt.h` defines probes of up to
/// twelve.
pub const MAX_ARGS: usize = 12;

/// Reads a probe's whole argument string.
///
/// A descriptor this cannot parse is not a refusal of the probe: its hit is
/// still an event, and the argument is carried as unread with the reason. An
/// argument string with more than twelve descriptors is refused, because the
/// record has nowhere to put the thirteenth.
pub fn parse_arguments(text: &str) -> Result<Vec<Arg>, String> {
    let args: Vec<Arg> = text.split_whitespace().map(parse_argument).collect();
    if args.len() > MAX_ARGS {
        return Err(format!(
            "`{text}` describes {} arguments and `sys/sdt.h` defines at most {MAX_ARGS}",
            args.len()
        ));
    }
    Ok(args)
}

/// Reads one descriptor: `[-]size@operand`, or a bare operand in the oldest
/// notes, which then means an unsigned eight-byte value.
pub fn parse_argument(descriptor: &str) -> Arg {
    let (size, signed, operand_text) = match descriptor.split_once('@') {
        Some((size, operand)) => {
            let (signed, digits) = match size.strip_prefix('-') {
                Some(rest) => (true, rest),
                None => (false, size),
            };
            match digits.parse::<u8>() {
                Ok(bytes @ (1 | 2 | 4 | 8)) => (bytes, signed, operand),
                _ => {
                    return Arg {
                        size: 8,
                        signed: false,
                        operand: Operand::Unreadable(format!(
                            "`{size}` is not an operand size of 1, 2, 4 or 8 bytes"
                        )),
                        text: descriptor.to_owned(),
                    }
                }
            }
        }
        None => (8, false, descriptor),
    };
    Arg {
        size,
        signed,
        operand: parse_operand(operand_text),
        text: descriptor.to_owned(),
    }
}

fn parse_operand(text: &str) -> Operand {
    if let Some(value) = text.strip_prefix('$') {
        return match parse_integer(value) {
            Some(value) => Operand::Immediate(value),
            None => Operand::Unreadable(format!(
                "`{text}` is a constant this build cannot read as an integer"
            )),
        };
    }
    if let Some(name) = text.strip_prefix('%') {
        return match register(name) {
            Some((member, width, shift)) => Operand::Register {
                member,
                width,
                shift,
            },
            None => Operand::Unreadable(format!("`{text}` is not a register this build knows")),
        };
    }
    // `offset(%base)`, `(%base)`, and the forms this does not read: an index
    // register, or a symbol whose address only the loaded image knows.
    let Some((offset_text, rest)) = text.split_once('(') else {
        return Operand::Unreadable(format!("`{text}` is not an operand this build can read"));
    };
    let Some(inside) = rest.strip_suffix(')') else {
        return Operand::Unreadable(format!("`{text}` is not an operand this build can read"));
    };
    if inside.contains(',') {
        return Operand::Unreadable(format!(
            "`{text}` addresses memory through an index register, which this build does not \
             compute"
        ));
    }
    let Some((member, 8, 0)) = inside.strip_prefix('%').and_then(register) else {
        return Operand::Unreadable(format!(
            "`{text}` addresses memory through `{inside}`, which is not a 64-bit register"
        ));
    };
    let offset = if offset_text.is_empty() {
        Some(0)
    } else {
        parse_integer(offset_text)
    };
    match offset {
        // The common case of this shape is a global the compiler reached
        // `rip`-relative: `counter(%rip)`. The symbol is what matters there.
        None => Operand::Unreadable(format!(
            "`{text}` is at `{offset_text}` from a register, which is a symbol rather than a \
             number; its address is known only to the loaded image and this build does not \
             resolve it"
        )),
        Some(_) if member == "ip" => Operand::Unreadable(format!(
            "`{text}` is relative to the instruction pointer, whose value at the probe site is \
             not the one the assembler meant; this build does not resolve it"
        )),
        Some(offset) => Operand::Memory {
            base: member,
            offset,
        },
    }
}

fn parse_integer(text: &str) -> Option<i64> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let magnitude = match digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        Some(hex) => u64::from_str_radix(hex, 16).ok()?,
        None => digits.parse::<u64>().ok()?,
    };
    Some(if negative {
        (magnitude as i64).wrapping_neg()
    } else {
        magnitude as i64
    })
}

/// An x86-64 register name, as the `pt_regs` member holding it, the width the
/// name selects, and the byte it starts at.
fn register(name: &str) -> Option<(&'static str, u8, u8)> {
    const LEGACY: [(&str, &str, &str, &str, &str, &str); 8] = [
        // member, 64, 32, 16, low 8, high 8
        ("ax", "rax", "eax", "ax", "al", "ah"),
        ("bx", "rbx", "ebx", "bx", "bl", "bh"),
        ("cx", "rcx", "ecx", "cx", "cl", "ch"),
        ("dx", "rdx", "edx", "dx", "dl", "dh"),
        ("si", "rsi", "esi", "si", "sil", ""),
        ("di", "rdi", "edi", "di", "dil", ""),
        ("bp", "rbp", "ebp", "bp", "bpl", ""),
        ("sp", "rsp", "esp", "sp", "spl", ""),
    ];
    for (member, r64, r32, r16, r8, high) in LEGACY {
        match name {
            _ if name == r64 => return Some((member, 8, 0)),
            _ if name == r32 => return Some((member, 4, 0)),
            _ if name == r16 => return Some((member, 2, 0)),
            _ if name == r8 => return Some((member, 1, 0)),
            _ if !high.is_empty() && name == high => return Some((member, 1, 1)),
            _ => {}
        }
    }
    if name == "rip" {
        return Some(("ip", 8, 0));
    }
    const NUMBERED: [&str; 8] = ["r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15"];
    for member in NUMBERED {
        let Some(suffix) = name.strip_prefix(member) else {
            continue;
        };
        let width = match suffix {
            "" => 8,
            "d" => 4,
            "w" => 2,
            "b" | "l" => 1,
            _ => continue,
        };
        return Some((member, width, 0));
    }
    None
}

// ------------------------------------------------------------------ probes

/// A probe site, ready to attach: the file, the offset into it, and what the
/// program must read there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    pub object: PathBuf,
    pub provider: String,
    pub name: String,
    /// Into the file, which is what the `uprobe` PMU takes.
    pub offset: u64,
    /// The semaphore, as a virtual address in the object and as an offset into
    /// the file. Both, because the userspace protocol pokes the running image
    /// and needs the first, and the kernel's reference-counter protocol takes
    /// the second.
    pub semaphore: Option<Semaphore>,
    pub args: Vec<Arg>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Semaphore {
    pub vaddr: u64,
    pub offset: u64,
}

impl Site {
    pub fn qualified(&self) -> String {
        format!("{}:{}", self.provider, self.name)
    }
}

/// Every probe site in an object.
///
/// A note whose location no segment maps is refused rather than skipped: it
/// would be a probe the object says it has and the run silently lacks.
pub fn sites(object: &Object) -> Result<Vec<Site>, String> {
    object
        .notes()?
        .into_iter()
        .map(|note| {
            let offset = object.file_offset(note.pc).ok_or_else(|| {
                format!(
                    "{}: `{}` is at {:#x}, which no loadable segment maps from the file",
                    object.path().display(),
                    note.qualified(),
                    note.pc
                )
            })?;
            let semaphore = match note.semaphore {
                None => None,
                Some(vaddr) => Some(Semaphore {
                    vaddr,
                    offset: object.file_offset(vaddr).ok_or_else(|| {
                        format!(
                            "{}: the semaphore of `{}` is at {vaddr:#x}, which no loadable \
                             segment maps from the file",
                            object.path().display(),
                            note.qualified()
                        )
                    })?,
                }),
            };
            let args = parse_arguments(&note.arguments).map_err(|error| {
                format!(
                    "{}: `{}`: {error}",
                    object.path().display(),
                    note.qualified()
                )
            })?;
            Ok(Site {
                object: object.path().to_owned(),
                provider: note.provider,
                name: note.name,
                offset,
                semaphore,
                args,
            })
        })
        .collect()
}

// ------------------------------------------------------------- the program

/// One probe hit, as the program writes it and userspace reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// `bpf_ktime_get_ns`, which is `CLOCK_MONOTONIC`.
    pub ts_ns: u64,
    pub tid: u32,
    pub tgid: u32,
    /// Which site fired, as its index in the slice the program was loaded
    /// with.
    pub site: u32,
    /// One bit per argument whose read from user memory failed. Those
    /// arguments are zero in `args` and must not be read as zero.
    pub unread: u32,
    pub comm: String,
    pub args: [u64; MAX_ARGS],
}

/// The record layout, shared by the program and `Hit::decode`.
const REC_TS: i16 = 0;
const REC_TID: i16 = 8;
const REC_TGID: i16 = 12;
const REC_SITE: i16 = 16;
const REC_UNREAD: i16 = 20;
const REC_COMM: i16 = 24;
const COMM_BYTES: i16 = 16;
const REC_ARGS: i16 = 40;
const RECORD_BYTES: usize = 40 + 8 * MAX_ARGS;

/// Where things live on the program's stack.
const RECORD: i16 = -(RECORD_BYTES as i16) - 8;
const SCRATCH: i16 = RECORD - 8;
const DROP_KEY: i16 = SCRATCH - 8;

// The regions stay inside the 512-byte frame and do not overlap, checked where
// a mistake would be made rather than when a test happens to run.
const _: () =
    assert!(DROP_KEY >= -512 && SCRATCH + 8 <= RECORD && RECORD + RECORD_BYTES as i16 <= 0);

/// One label per argument, because a label is a `&'static str` and two
/// arguments cannot share one.
const READ_OK: [&str; MAX_ARGS] = [
    "arg0_read",
    "arg1_read",
    "arg2_read",
    "arg3_read",
    "arg4_read",
    "arg5_read",
    "arg6_read",
    "arg7_read",
    "arg8_read",
    "arg9_read",
    "arg10_read",
    "arg11_read",
];

impl Hit {
    fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < RECORD_BYTES {
            return None;
        }
        let word = |at: i16| {
            u32::from_ne_bytes(bytes[at as usize..at as usize + 4].try_into().expect("4"))
        };
        let long = |at: usize| u64::from_ne_bytes(bytes[at..at + 8].try_into().expect("8"));
        let comm = &bytes[REC_COMM as usize..(REC_COMM + COMM_BYTES) as usize];
        let end = comm
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(comm.len());
        let mut args = [0u64; MAX_ARGS];
        for (index, arg) in args.iter_mut().enumerate() {
            *arg = long(REC_ARGS as usize + 8 * index);
        }
        Some(Self {
            ts_ns: long(REC_TS as usize),
            tid: word(REC_TID),
            tgid: word(REC_TGID),
            site: word(REC_SITE),
            unread: word(REC_UNREAD),
            comm: String::from_utf8_lossy(&comm[..end]).into_owned(),
            args,
        })
    }
}

/// Where each register lives in `struct pt_regs`, read from the kernel's BTF
/// rather than assumed: the program reads user registers out of the context a
/// uprobe hands it, and that context is this structure.
fn register_offset(btf: &Btf, member: &str) -> Result<i16, String> {
    let offset = btf.member_offset("pt_regs", member)?;
    i16::try_from(offset)
        .map_err(|_| format!("`pt_regs.{member}` is at {offset}, past any load offset"))
}

/// Leaves the argument in `r1` sign- or zero-extended from its declared size.
fn extend(program: &mut Program, size: u8, signed: bool) {
    if size >= 8 {
        return;
    }
    let shift = 64 - 8 * i32::from(size);
    program.push(Insn::lsh64_imm(1, shift));
    program.push(if signed {
        Insn::arsh64_imm(1, shift)
    } else {
        Insn::rsh64_imm(1, shift)
    });
}

/// What a program does with a hit: hand it up whole, or add one to its site's
/// count in the kernel.
///
/// The choice is about volume, as everywhere in this layer. A probe on a hot
/// path fires faster than a ring can be drained, and a count that streams every
/// hit to userspace to add them up there loses the ones the ring could not
/// hold -- conserved and reported, but lost. Counting in the kernel loses none,
/// and costs the trap and one increment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Stream,
    Count,
}

/// The program for one probe site.
///
/// `r6` holds the context -- the user registers at the probe -- and `r8` the
/// mask of arguments whose memory could not be read; both survive the helper
/// calls in between.
fn instructions(
    btf: &Btf,
    site: &Site,
    index: u32,
    tgid: Option<u32>,
    ring_fd: i32,
    drops_fd: i32,
    count_fd: Option<i32>,
) -> Result<Vec<Insn>, String> {
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));

    program.push(Insn::call(helper::GET_CURRENT_PID_TGID));
    program.push(Insn::stx(BPF_W, R10, RECORD + REC_TID, 0));
    program.push(Insn::rsh64_imm(0, 32));
    program.push(Insn::stx(BPF_W, R10, RECORD + REC_TGID, 0));
    // The perf event already follows one process's memory map, so this is a
    // second fence rather than the only one: the file is shared by every
    // process that maps it, and a hit in another one must not be counted.
    if let Some(tgid) = tgid {
        program.jump_to("done", Insn::jne_imm(0, tgid as i32, 0));
    }

    if let Some(count_fd) = count_fd {
        // Per-cpu, so the increment needs no atomic; the slot is this site's.
        insn::count_drop(&mut program, count_fd, DROP_KEY, index, "done");
        program.label("done");
        program.push(Insn::mov64_imm(0, 0));
        program.push(Insn::exit());
        let instructions = program.build();
        insn::audit(&instructions)?;
        return Ok(instructions);
    }

    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, RECORD + REC_TS, 0));
    program.push(Insn::st_imm(BPF_W, R10, RECORD + REC_SITE, index as i32));

    program.push(Insn::mov64_reg(1, R10));
    program.push(Insn::add64_imm(1, (RECORD + REC_COMM) as i32));
    program.push(Insn::mov64_imm(2, COMM_BYTES as i32));
    program.push(Insn::call(helper::GET_CURRENT_COMM));

    program.push(Insn::mov64_imm(8, 0));
    for (slot, read_ok) in READ_OK.iter().enumerate() {
        let into = RECORD + REC_ARGS + 8 * slot as i16;
        let Some(arg) = site.args.get(slot) else {
            program.push(Insn::st_imm(BPF_DW, R10, into, 0));
            continue;
        };
        match &arg.operand {
            Operand::Register {
                member,
                width,
                shift,
            } => {
                program.push(Insn::ldx(BPF_DW, 1, 6, register_offset(btf, member)?));
                if *shift > 0 {
                    program.push(Insn::rsh64_imm(1, 8 * i32::from(*shift)));
                }
                extend(&mut program, arg.size.min(*width), arg.signed);
            }
            Operand::Immediate(value) => {
                program.extend(Insn::ld_imm64(1, *value));
                extend(&mut program, arg.size, arg.signed);
            }
            Operand::Memory { base, offset } => {
                program.push(Insn::ldx(BPF_DW, 3, 6, register_offset(btf, base)?));
                if let Ok(small) = i32::try_from(*offset) {
                    program.push(Insn::add64_imm(3, small));
                } else {
                    program.extend(Insn::ld_imm64(2, *offset));
                    program.push(Insn {
                        // `r3 += r2`
                        code: 0x0f,
                        regs: 3 | (2 << 4),
                        off: 0,
                        imm: 0,
                    });
                }
                // Cleared first, so a short read leaves zeros above it rather
                // than whatever the stack held.
                program.push(Insn::st_imm(BPF_DW, R10, SCRATCH, 0));
                program.push(Insn::mov64_reg(1, R10));
                program.push(Insn::add64_imm(1, SCRATCH as i32));
                program.push(Insn::mov64_imm(2, i32::from(arg.size)));
                program.push(Insn::call(helper::PROBE_READ_USER));
                program.jump_to(read_ok, Insn::jeq_imm(0, 0, 0));
                program.push(Insn::or64_imm(8, 1 << slot));
                program.label(read_ok);
                program.push(Insn::ldx(BPF_DW, 1, R10, SCRATCH));
                extend(&mut program, arg.size, arg.signed);
            }
            Operand::Unreadable(_) => {
                program.push(Insn::mov64_imm(1, 0));
                program.push(Insn::or64_imm(8, 1 << slot));
            }
        }
        program.push(Insn::stx(BPF_DW, R10, into, 1));
    }
    program.push(Insn::stx(BPF_W, R10, RECORD + REC_UNREAD, 8));

    program.extend(Insn::load_map_fd(1, ring_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, RECORD as i32));
    program.push(Insn::mov64_imm(3, RECORD_BYTES as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::RINGBUF_OUTPUT));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    insn::count_drop(&mut program, drops_fd, DROP_KEY, 0, "done");

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let instructions = program.build();
    insn::audit(&instructions)?;
    Ok(instructions)
}

// ------------------------------------------------------------ the attach

// Everything in this section belongs in `attach.rs` and `syscall.rs`, which
// are not this stream's files; `docs/open-findings.md` asks for it to move
// there under the signatures given. It is here because without it nothing in
// this file attaches on the machine this project is developed on, and a
// request that stops the stream delivers nothing.
//
// **Why not `attach::uprobe`.** That opens the `uprobe` PMU through
// `perf_event_open`, and the kernel's `perf_uprobe_event_init` requires
// `CAP_SYS_ADMIN` -- not `CAP_PERFMON`, which is enough for its kprobe twin.
// Measured here: with `cap_bpf,cap_perfmon+ep` effective, the kprobe PMU opens
// and the uprobe PMU answers `EACCES`, for this process, for another one and
// for a cpu. A uprobe writes a breakpoint into another program's text, and the
// kernel keeps that behind the capability this project deliberately does not
// ask for.
//
// A `BPF_TRACE_UPROBE_MULTI` link (Linux 6.6) reaches the same breakpoint with
// the program's own privileges -- `CAP_BPF` and `CAP_PERFMON` -- and does two
// things the perf path cannot: it follows a *process* rather than a thread,
// and it takes each site's semaphore as a `ref_ctr_offset`, so the kernel
// raises the counter in every image of the file the link covers, including one
// that is mapped after the link exists. That last part is what makes a guarded
// probe in a launched program observable from its first instruction.

/// `bpf_attach_type::BPF_TRACE_UPROBE_MULTI`.
const BPF_TRACE_UPROBE_MULTI: u32 = 48;
const LICENSE: &str = "Dual MIT/GPL";

/// Loads a `BPF_PROG_TYPE_KPROBE` program declared for a uprobe-multi link:
/// `syscall::prog_load_for`, with this file's refusal wording on top.
fn load_for_multi(instructions: &[Insn]) -> Result<OwnedFd, String> {
    syscall::prog_load_for(
        syscall::BPF_PROG_TYPE_KPROBE,
        BPF_TRACE_UPROBE_MULTI,
        instructions,
        "omn_usdt",
        LICENSE,
    )
    .map_err(|error| {
        if error.is_permission() {
            return super::permission_advice("loading a USDT program");
        }
        format!(
            "the kernel refused the USDT program ({}); the verifier said:\n{}",
            error.error, error.verifier_log
        )
    })
}

/// Whether this kernel makes a uprobe-multi link for this process: the
/// `uprobe_multi` requirement, and the first half of `uprobe`.
///
/// Asked of the kernel rather than read off its version. The smallest program
/// is loaded for `BPF_TRACE_UPROBE_MULTI` and linked to a file that does not
/// exist: a kernel that knows the link looks the path up and answers `ENOENT`,
/// one older than 6.6 does not know the attach type and answers `EINVAL` --
/// which is exactly what the first real site used to be told, after the target
/// was launched. Nothing is registered either way, because there is no file.
pub fn uprobe_multi_support() -> &'static Support {
    static SUPPORT: std::sync::OnceLock<Support> = std::sync::OnceLock::new();
    SUPPORT.get_or_init(|| {
        let smallest = [Insn::mov64_imm(0, 0), Insn::exit()];
        let program = match syscall::prog_load_for(
            syscall::BPF_PROG_TYPE_KPROBE,
            BPF_TRACE_UPROBE_MULTI,
            &smallest,
            "omn_umulti_probe",
            LICENSE,
        ) {
            Ok(program) => program,
            Err(error) if error.is_permission() => {
                return Support::Unchecked(
                    "whether this kernel makes a uprobe-multi link cannot be asked without \
                     loading a program, which needs CAP_BPF and CAP_PERFMON"
                        .to_owned(),
                )
            }
            Err(error) => {
                return Support::No(format!(
                    "this kernel refused a program loaded for a uprobe-multi link ({}), so it \
                     has no uprobe-multi link, which Linux 6.6 introduced",
                    error.error
                ))
            }
        };
        let nowhere = Path::new("/proc/self/omn-uprobe-multi-probe");
        match syscall::link_uprobe_multi(
            program.as_raw_fd(),
            nowhere,
            0,
            None,
            std::process::id() as i32,
        ) {
            Ok(_) => Support::Yes,
            Err(error) => match error.raw_os_error() {
                Some(libc::ENOENT) | Some(libc::EBADF) => Support::Yes,
                Some(libc::EINVAL) => Support::No(format!(
                    "this kernel refused a uprobe-multi link ({error}), which Linux 6.6 \
                     introduced"
                )),
                Some(libc::EOPNOTSUPP) => Support::No(format!(
                    "this kernel knows the uprobe-multi link and was built without uprobes \
                     ({error})"
                )),
                Some(libc::EPERM) | Some(libc::EACCES) => Support::Unchecked(format!(
                    "the kernel refused to link a uprobe-multi program on privilege grounds \
                     ({error}), so whether it could is not known"
                )),
                _ => Support::Unchecked(format!(
                    "a uprobe-multi link to a file that does not exist was refused with \
                     {error}, which says nothing about whether one to a real file would be"
                )),
            },
        }
    })
}

/// Links a loaded program to one offset of a file, for one process, with the
/// site's semaphore handed to the kernel to maintain.
fn link_multi(program: &OwnedFd, site: &Site, pid: i32) -> io::Result<OwnedFd> {
    syscall::link_uprobe_multi(
        program.as_raw_fd(),
        &site.object,
        site.offset,
        site.semaphore.map(|semaphore| semaphore.offset),
        pid,
    )
}

/// What keeps one site attached, by whichever route attached it.
enum Attachment {
    Link { _link: OwnedFd, _program: OwnedFd },
    Perf(#[allow(dead_code)] PerfAttachment),
}

/// Loaded, attached programs for a set of probe sites, and the ring they
/// share.
pub struct UsdtProgram {
    ring: RingBuffer,
    drops: Counters,
    /// Hits per site, when the programs count rather than stream.
    counts: Option<Counters>,
    _attachments: Vec<Attachment>,
    /// Why the uprobe-multi link was not used, when it was not.
    pub fallback: Option<String>,
}

impl UsdtProgram {
    /// Loads one program per site and attaches each to its site, for the
    /// process `pid` and every thread in it.
    ///
    /// One program per site rather than one for all: two sites of the same
    /// probe can keep the same argument in different registers, because each
    /// is wherever the compiler had it at that instruction.
    pub fn load(
        sites: &[Site],
        pid: i32,
        shape: Shape,
        requested_ring_bytes: usize,
    ) -> Result<Self, String> {
        let btf = super::kernel_btf()?;
        let bytes = ring_bytes(requested_ring_bytes);
        let ring_map = syscall::map_create(BPF_MAP_TYPE_RINGBUF, 0, 0, bytes as u32, "omn_usdt")
            .map_err(|error| map_failure("USDT ring buffer", error))?;
        let drops = Counters::create(1, "omn_usdt_drop")
            .map_err(|error| map_failure("USDT drop counter", error))?;
        let counts = match shape {
            Shape::Stream => None,
            Shape::Count => Some(
                Counters::create(sites.len().max(1) as u32, "omn_usdt_hits")
                    .map_err(|error| map_failure("USDT hit counter", error))?,
            ),
        };

        let mut attachments = Vec::with_capacity(sites.len());
        let mut fallback: Option<String> = None;
        for (index, site) in sites.iter().enumerate() {
            let where_ = || format!("`{}` in {}", site.qualified(), site.object.display());
            let instructions = instructions(
                btf,
                site,
                index as u32,
                Some(pid as u32),
                ring_map.as_raw_fd(),
                drops.as_raw_fd(),
                counts.as_ref().map(Counters::as_raw_fd),
            )?;
            if fallback.is_none() {
                let program = load_for_multi(&instructions)
                    .map_err(|error| format!("{}: {error}", where_()))?;
                match link_multi(&program, site, pid) {
                    Ok(link) => {
                        attachments.push(Attachment::Link {
                            _link: link,
                            _program: program,
                        });
                        continue;
                    }
                    // A kernel older than 6.6 does not know the attach type
                    // and says so as `EINVAL` on the first link. Every later
                    // site takes the older route with it.
                    Err(error) if index == 0 && error.raw_os_error() == Some(libc::EINVAL) => {
                        fallback = Some(format!(
                            "this kernel refused a uprobe-multi link ({error}), which Linux 6.6 \
                             introduced; the sites were attached through the `uprobe` PMU \
                             instead, which needs CAP_SYS_ADMIN and does not maintain semaphores"
                        ));
                    }
                    Err(error) => {
                        return Err(format!(
                            "{}: the kernel refused to link the program to {:#x} ({error})",
                            where_(),
                            site.offset
                        ))
                    }
                }
            }
            // Asked before the attach, so that a refusal leads with what is
            // missing. The attach's own refusal on privilege grounds is the
            // shared advice to grant CAP_BPF and CAP_PERFMON -- to a binary
            // that, here, carries both (P3 in `docs/portability.md`).
            if let Err(pmu) = attach::uprobe_pmu_permitted() {
                return Err(format!(
                    "{}: this kernel has no uprobe-multi link (Linux 6.6), and the uprobe PMU \
                     it falls back to {pmu}",
                    where_(),
                ));
            }
            // The older route follows one thread, not a process: the kernel's
            // filter compares memory maps, so every thread of the process is
            // still covered, and the program's own tgid check is the fence.
            let attachment = attach::uprobe(
                &site.object,
                site.offset,
                false,
                &instructions,
                "omn_usdt",
                Scope::Task(pid),
            )
            .map_err(|error| {
                format!(
                    "{}: {error}{}",
                    where_(),
                    fallback
                        .as_deref()
                        .map(|why| format!(" ({why})"))
                        .unwrap_or_default()
                )
            })?;
            attachments.push(Attachment::Perf(attachment));
        }

        let ring = RingBuffer::map(ring_map, bytes)
            .map_err(|error| format!("cannot map the USDT ring buffer: {error}"))?;
        Ok(Self {
            ring,
            drops,
            counts,
            _attachments: attachments,
            fallback,
        })
    }

    /// Hands every complete hit to `handle`, and counts records that were not
    /// the size this reader expects rather than reinterpreting them.
    pub fn drain(&mut self, mut handle: impl FnMut(Hit), undecoded: &mut u64) {
        self.ring.drain(|bytes| match Hit::decode(bytes) {
            Some(hit) => handle(hit),
            None => *undecoded += 1,
        });
    }

    /// Hits the ring could not hold, cumulative.
    pub fn dropped(&self) -> u64 {
        self.drops.read(0)
    }

    /// Hits of site `index` so far, when the programs count. `None` when they
    /// stream, because then the count is whatever was drained.
    pub fn counted(&self, index: usize) -> Option<u64> {
        self.counts.as_ref().map(|counts| counts.read(index as u32))
    }
}

// ------------------------------------------------------------ memory maps

/// One line of `/proc/<pid>/maps`, the parts of it this needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapping {
    pub start: u64,
    pub end: u64,
    pub offset: u64,
    pub path: PathBuf,
}

/// Reads `/proc/<pid>/maps` into its file-backed mappings.
pub fn parse_maps(text: &str) -> Vec<Mapping> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let range = fields.next()?;
            let _perms = fields.next()?;
            let offset = u64::from_str_radix(fields.next()?, 16).ok()?;
            let _dev = fields.next()?;
            let _inode = fields.next()?;
            let path = fields.collect::<Vec<_>>().join(" ");
            if !path.starts_with('/') {
                return None;
            }
            let (start, end) = range.split_once('-')?;
            Some(Mapping {
                start: u64::from_str_radix(start, 16).ok()?,
                end: u64::from_str_radix(end, 16).ok()?,
                offset,
                path: PathBuf::from(path),
            })
        })
        .collect()
}

impl fmt::Display for Operand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Operand::Register { member, shift, .. } if *shift > 0 => {
                write!(f, "pt_regs.{member} byte {shift}")
            }
            Operand::Register { member, .. } => write!(f, "pt_regs.{member}"),
            Operand::Immediate(value) => write!(f, "the constant {value}"),
            Operand::Memory { base, offset } => {
                write!(f, "user memory at pt_regs.{base}{offset:+}")
            }
            Operand::Unreadable(why) => write!(f, "unread: {why}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_descriptors_glibc_writes_are_read_the_way_they_are_meant() {
        // From `libc:pthread_start` and `libc:setjmp` on this project's
        // development machine, verbatim.
        let args = parse_arguments("8@%r9 8@1600(%r9) -4@%esi").expect("three descriptors");
        assert_eq!(
            args[0].operand,
            Operand::Register {
                member: "r9",
                width: 8,
                shift: 0
            }
        );
        assert_eq!(
            args[1].operand,
            Operand::Memory {
                base: "r9",
                offset: 1600
            }
        );
        assert_eq!((args[2].size, args[2].signed), (4, true));
        assert_eq!(
            args[2].operand,
            Operand::Register {
                member: "si",
                width: 4,
                shift: 0
            }
        );
    }

    #[test]
    fn a_constant_a_negative_offset_and_a_bare_base_are_all_read() {
        let args = parse_arguments("-8@$-7 8@-80(%rbx) 8@(%rdi) -2@%cx 1@%ah").expect("five");
        assert_eq!(args[0].operand, Operand::Immediate(-7));
        assert_eq!(
            args[1].operand,
            Operand::Memory {
                base: "bx",
                offset: -80
            }
        );
        assert_eq!(
            args[2].operand,
            Operand::Memory {
                base: "di",
                offset: 0
            }
        );
        assert_eq!(
            args[3].operand,
            Operand::Register {
                member: "cx",
                width: 2,
                shift: 0
            }
        );
        assert_eq!(
            args[4].operand,
            Operand::Register {
                member: "ax",
                width: 1,
                shift: 1
            }
        );
    }

    /// An operand this build cannot compute is carried as unread with the
    /// reason, never as a zero that looks like a value.
    #[test]
    fn what_cannot_be_read_says_why_rather_than_reading_zero() {
        for (descriptor, reason) in [
            ("-4@counter(%rip)", "symbol"),
            ("8@16(%rip)", "instruction pointer"),
            ("8@(%rax,%rbx,8)", "index register"),
            ("8@%xmm0", "register"),
            ("3@%rax", "operand size"),
        ] {
            let arg = parse_argument(descriptor);
            let Operand::Unreadable(why) = &arg.operand else {
                panic!("`{descriptor}` read as {:?}", arg.operand);
            };
            assert!(why.contains(reason), "`{descriptor}`: {why}");
        }
    }

    #[test]
    fn every_register_width_maps_onto_its_pt_regs_member() {
        for (name, member, width) in [
            ("rax", "ax", 8),
            ("eax", "ax", 4),
            ("ax", "ax", 2),
            ("al", "ax", 1),
            ("sil", "si", 1),
            ("r8", "r8", 8),
            ("r8d", "r8", 4),
            ("r15w", "r15", 2),
            ("r11b", "r11", 1),
            ("rsp", "sp", 8),
        ] {
            assert_eq!(register(name), Some((member, width, 0)), "{name}");
        }
        assert_eq!(register("r16"), None);
    }

    #[test]
    fn a_thirteenth_argument_is_refused_rather_than_dropped() {
        let text = vec!["8@%rdi"; 13].join(" ");
        assert!(parse_arguments(&text).is_err());
        assert_eq!(
            parse_arguments(&["8@%rdi"; 12].join(" ")).map(|a| a.len()),
            Ok(12)
        );
    }

    /// A minimal ELF64 with one note, a `.stapsdt.base` that moved by 0x100
    /// since the note was written, and two `PT_LOAD` segments at different
    /// file-to-address distances.
    fn synthetic_object() -> Vec<u8> {
        let mut note = Vec::new();
        let owner = b"stapsdt\0";
        let mut desc = Vec::new();
        desc.extend_from_slice(&0x1050u64.to_le_bytes()); // pc
        desc.extend_from_slice(&0x2000u64.to_le_bytes()); // recorded base
        desc.extend_from_slice(&0x4010u64.to_le_bytes()); // semaphore
        desc.extend_from_slice(b"prov\0tick\0-4@%eax 8@$3\0");
        note.extend_from_slice(&(owner.len() as u32).to_le_bytes());
        note.extend_from_slice(&(desc.len() as u32).to_le_bytes());
        note.extend_from_slice(&NT_STAPSDT.to_le_bytes());
        note.extend_from_slice(owner);
        note.extend_from_slice(&desc);
        while note.len() % 4 != 0 {
            note.push(0);
        }

        let names = b"\0.note.stapsdt\0.stapsdt.base\0.shstrtab\0";
        let note_at = 0x200u64;
        let names_at = note_at + note.len() as u64;
        let shoff = (names_at + names.len() as u64 + 7) & !7;
        let phoff = 64u64;

        let mut data = vec![0u8; shoff as usize + 4 * 64];
        data[..4].copy_from_slice(b"\x7fELF");
        data[4] = ELFCLASS64;
        data[5] = ELFDATA2LSB;
        data[18..20].copy_from_slice(&EM_X86_64.to_le_bytes());
        data[32..40].copy_from_slice(&phoff.to_le_bytes());
        data[40..48].copy_from_slice(&shoff.to_le_bytes());
        data[54..56].copy_from_slice(&56u16.to_le_bytes());
        data[56..58].copy_from_slice(&2u16.to_le_bytes());
        data[58..60].copy_from_slice(&64u16.to_le_bytes());
        data[60..62].copy_from_slice(&4u16.to_le_bytes());
        data[62..64].copy_from_slice(&3u16.to_le_bytes());

        // Text: file 0x1000 at 0x1000. Data: file 0x3000 at 0x4000.
        for (index, (offset, vaddr)) in [(0x1000u64, 0x1000u64), (0x3000, 0x4000)]
            .iter()
            .enumerate()
        {
            let at = phoff as usize + index * 56;
            data[at..at + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
            data[at + 8..at + 16].copy_from_slice(&offset.to_le_bytes());
            data[at + 16..at + 24].copy_from_slice(&vaddr.to_le_bytes());
            data[at + 32..at + 40].copy_from_slice(&0x1000u64.to_le_bytes());
        }

        data[note_at as usize..note_at as usize + note.len()].copy_from_slice(&note);
        data[names_at as usize..names_at as usize + names.len()].copy_from_slice(names);
        let section = |data: &mut Vec<u8>,
                       index: usize,
                       name: u32,
                       kind: u32,
                       addr: u64,
                       offset: u64,
                       size: u64| {
            let at = shoff as usize + index * 64;
            data[at..at + 4].copy_from_slice(&name.to_le_bytes());
            data[at + 4..at + 8].copy_from_slice(&kind.to_le_bytes());
            data[at + 16..at + 24].copy_from_slice(&addr.to_le_bytes());
            data[at + 24..at + 32].copy_from_slice(&offset.to_le_bytes());
            data[at + 32..at + 40].copy_from_slice(&size.to_le_bytes());
        };
        section(&mut data, 1, 1, SHT_NOTE, 0, note_at, note.len() as u64);
        section(&mut data, 2, 15, 1, 0x2100, 0, 1);
        section(&mut data, 3, 29, 3, 0, names_at, names.len() as u64);
        data
    }

    #[test]
    fn a_note_is_read_and_moved_by_however_far_its_base_moved() {
        let object = Object::parse(Path::new("synthetic"), synthetic_object()).expect("an object");
        let notes = object.notes().expect("notes");
        assert_eq!(notes.len(), 1);
        let note = &notes[0];
        assert_eq!(
            (note.provider.as_str(), note.name.as_str()),
            ("prov", "tick")
        );
        assert_eq!(note.arguments, "-4@%eax 8@$3");
        // `.stapsdt.base` is at 0x2100 and the note says 0x2000.
        assert_eq!(note.pc, 0x1150);
        assert_eq!(note.semaphore, Some(0x4110));

        let sites = sites(&object).expect("sites");
        // The text segment maps file to address one to one; the data segment
        // is 0x1000 further on in memory than in the file.
        assert_eq!(sites[0].offset, 0x1150);
        assert_eq!(
            sites[0].semaphore,
            Some(Semaphore {
                vaddr: 0x4110,
                offset: 0x3110
            })
        );
        assert_eq!(sites[0].args.len(), 2);
    }

    #[test]
    fn an_object_for_another_machine_is_refused_by_name() {
        let mut data = synthetic_object();
        data[18..20].copy_from_slice(&183u16.to_le_bytes());
        let error = Object::parse(Path::new("arm"), data)
            .err()
            .expect("aarch64 is refused");
        assert!(error.contains("183") && error.contains("x86-64"), "{error}");
        let error = Object::parse(Path::new("text"), b"#!/bin/sh\n".repeat(10))
            .err()
            .expect("a script");
        assert!(error.contains("not an ELF"), "{error}");
    }

    #[test]
    fn a_maps_line_names_its_file_and_where_it_is() {
        let maps = "\
55d0c0a00000-55d0c0a01000 r--p 00000000 00:1f 123 /tmp/fixture
55d0c0a03000-55d0c0a04000 rw-p 00002000 00:1f 123 /tmp/with space
7ffd00000000-7ffd00021000 rw-p 00000000 00:00 0 [stack]
";
        let mappings = parse_maps(maps);
        assert_eq!(mappings.len(), 2, "{mappings:?}");
        assert_eq!(mappings[1].path, PathBuf::from("/tmp/with space"));
        assert_eq!(
            (mappings[1].start, mappings[1].offset),
            (0x55d0c0a03000, 0x2000)
        );
    }

    fn site(args: &str) -> Site {
        Site {
            object: PathBuf::from("/x"),
            provider: "p".into(),
            name: "n".into(),
            offset: 0x10,
            semaphore: None,
            args: parse_arguments(args).expect("args"),
        }
    }

    #[test]
    fn the_program_passes_the_audit_for_every_operand_kind() {
        let Ok(btf) = super::super::kernel_btf() else {
            eprintln!("skipped: this kernel publishes no BTF, so pt_regs cannot be laid out");
            return;
        };
        let full = [
            "8@%rdi",
            "-4@%esi",
            "8@$-7",
            "8@-80(%rbx)",
            "1@%ah",
            "-4@sym(%rip)",
        ]
        .join(" ");
        let program = instructions(btf, &site(&full), 3, Some(42), 5, 6, None).expect("a program");
        let counting =
            instructions(btf, &site(&full), 3, Some(42), 5, 6, Some(7)).expect("a program");
        // A counting program reads no argument and publishes nothing.
        assert!(!counting
            .iter()
            .any(|insn| insn.code == 0x85 && insn.imm == helper::RINGBUF_OUTPUT));
        assert!(!counting
            .iter()
            .any(|insn| insn.code == 0x85 && insn.imm == helper::PROBE_READ_USER));
        // The user-memory read is the one helper this program cannot do
        // without, and the kernel-memory one would read the wrong space.
        let calls: Vec<i32> = program
            .iter()
            .filter(|insn| insn.code == 0x85)
            .map(|insn| insn.imm)
            .collect();
        assert!(calls.contains(&helper::PROBE_READ_USER));
        assert!(!calls.contains(&helper::PROBE_READ_KERNEL));
        assert!(calls.contains(&helper::KTIME_GET_NS));
    }

    /// binutils' `readelf -n` is an independent reader of the same notes. On
    /// every object both can read, the two must agree on every probe: its
    /// provider, name, location, semaphore and argument string.
    #[test]
    fn the_notes_agree_with_readelf_on_every_probe_of_libc() {
        let libc = [
            "/usr/lib/libc.so.6",
            "/lib/x86_64-linux-gnu/libc.so.6",
            "/lib64/libc.so.6",
        ]
        .into_iter()
        .map(Path::new)
        .find(|path| path.exists());
        let Some(libc) = libc else {
            eprintln!("skipped: no libc.so.6 at any of the usual paths");
            return;
        };
        let Ok(output) = std::process::Command::new("readelf")
            .arg("-n")
            .arg(libc)
            .output()
        else {
            eprintln!("skipped: no `readelf` to compare against");
            return;
        };
        let text = String::from_utf8_lossy(&output.stdout);
        let mut theirs = Vec::new();
        let mut current: Option<(String, String)> = None;
        let mut location = (0u64, 0u64);
        for line in text.lines().map(str::trim) {
            if let Some(provider) = line.strip_prefix("Provider: ") {
                current = Some((provider.to_owned(), String::new()));
            } else if let Some(name) = line.strip_prefix("Name: ") {
                if let Some(current) = current.as_mut() {
                    current.1 = name.to_owned();
                }
            } else if let Some(rest) = line.strip_prefix("Location: ") {
                let field = |key: &str| {
                    let at = rest.find(key).expect("a readelf field") + key.len();
                    let hex = rest[at..]
                        .split(',')
                        .next()
                        .unwrap()
                        .trim()
                        .trim_start_matches("0x");
                    u64::from_str_radix(hex, 16).expect("hex")
                };
                location = (
                    u64::from_str_radix(
                        rest.split(',').next().unwrap().trim_start_matches("0x"),
                        16,
                    )
                    .unwrap(),
                    field("Semaphore: "),
                );
            } else if let Some(arguments) = line.strip_prefix("Arguments: ") {
                let (provider, name) = current.take().expect("a probe before its arguments");
                theirs.push((provider, name, location.0, location.1, arguments.to_owned()));
            }
        }
        let object = Object::read(libc).expect("libc is an object this reads");
        let ours: Vec<_> = object
            .notes()
            .expect("notes")
            .into_iter()
            .map(|note| {
                (
                    note.provider,
                    note.name,
                    note.pc,
                    note.semaphore.unwrap_or(0),
                    note.arguments,
                )
            })
            .collect();
        if theirs.is_empty() && ours.is_empty() {
            eprintln!("skipped: this libc carries no USDT notes, so there is nothing to compare");
            return;
        }
        // readelf prints a probe with no arguments with no `Arguments:` line,
        // so it is compared on the probes it does print.
        for probe in &theirs {
            assert!(
                ours.contains(probe),
                "readelf has {probe:?} and this reader does not"
            );
        }
        assert!(
            theirs.len() * 10 >= ours.len() * 9,
            "{} against {}",
            theirs.len(),
            ours.len()
        );
    }

    #[test]
    fn a_record_round_trips_through_its_layout() {
        let mut bytes = vec![0u8; RECORD_BYTES];
        bytes[..8].copy_from_slice(&123u64.to_ne_bytes());
        bytes[8..12].copy_from_slice(&7u32.to_ne_bytes());
        bytes[12..16].copy_from_slice(&6u32.to_ne_bytes());
        bytes[16..20].copy_from_slice(&2u32.to_ne_bytes());
        bytes[20..24].copy_from_slice(&0b100u32.to_ne_bytes());
        bytes[24..28].copy_from_slice(b"fixt");
        bytes[40 + 8..40 + 16].copy_from_slice(&(-9i64).to_ne_bytes());
        let hit = Hit::decode(&bytes).expect("a full record");
        assert_eq!(
            (hit.ts_ns, hit.tid, hit.tgid, hit.site, hit.unread),
            (123, 7, 6, 2, 0b100)
        );
        assert_eq!(hit.comm, "fixt");
        assert_eq!(hit.args[1] as i64, -9);
        assert!(Hit::decode(&bytes[..RECORD_BYTES - 1]).is_none());
    }
}
