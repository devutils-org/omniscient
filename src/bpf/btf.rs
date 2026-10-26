// SPDX-License-Identifier: MIT OR GPL-2.0
//! The running kernel's own type information, read at load time.
//!
//! A BPF program that reads `task_struct->pid` needs the offset of that field
//! in the kernel it is about to run on, and that offset moves between versions
//! and between configurations. Compile-time CO-RE solves this by recording a
//! relocation in the object file for libbpf to patch. There is no object file
//! here, so the offset is looked up in `/sys/kernel/btf/vmlinux` and written
//! into the instruction stream while it is being built: the same relocation,
//! done a few milliseconds later, against the kernel that is actually running.
//!
//! The parser is deliberately small. It answers two questions -- where is this
//! field, and does this type exist -- because those are the two the layer asks.
//! A field that is not found is an error naming the type and the field, never a
//! guessed offset: reading a wrong offset out of a `task_struct` produces a
//! plausible number with nothing behind it.

use std::collections::HashMap;

/// `/sys/kernel/btf/vmlinux` is where the kernel publishes its own BTF.
const VMLINUX_BTF: &str = "/sys/kernel/btf/vmlinux";

const BTF_MAGIC: u16 = 0xeb9f;

const KIND_INT: u8 = 1;
const KIND_PTR: u8 = 2;
const KIND_ARRAY: u8 = 3;
const KIND_STRUCT: u8 = 4;
const KIND_UNION: u8 = 5;
const KIND_ENUM: u8 = 6;
const KIND_TYPEDEF: u8 = 8;
const KIND_VOLATILE: u8 = 9;
const KIND_CONST: u8 = 10;
const KIND_RESTRICT: u8 = 11;
const KIND_FUNC_PROTO: u8 = 13;
const KIND_VAR: u8 = 14;
const KIND_DATASEC: u8 = 15;
const KIND_DECL_TAG: u8 = 17;
const KIND_ENUM64: u8 = 19;

/// One type, as far as this parser cares: what it is called, what kind it is,
/// and where its members start in the buffer.
struct TypeEntry {
    kind: u8,
    kind_flag: bool,
    vlen: u16,
    /// Offset of this type's name in the string section, or zero when it is
    /// anonymous.
    name_at: usize,
    /// A size for the kinds that have one, and the referenced type id for the
    /// kinds that point at another -- a pointer, a typedef, a qualifier. BTF
    /// packs both into the same word.
    size_or_type: u32,
    /// Offset of the first member, immediately after the 12-byte header.
    members_at: usize,
}

/// The kernel's BTF, indexed by type name.
///
/// `Debug` prints its size rather than its contents: the blob is several
/// megabytes, and a test that unwraps a `Result<Btf, _>` would otherwise print
/// all of it.
pub struct Btf {
    buffer: Vec<u8>,
    string_offset: usize,
    types: Vec<TypeEntry>,
    by_name: HashMap<String, Vec<usize>>,
}

impl Btf {
    /// Reads and indexes the running kernel's BTF.
    pub fn from_kernel() -> Result<Self, String> {
        let buffer = std::fs::read(VMLINUX_BTF).map_err(|error| {
            format!(
                "cannot read {VMLINUX_BTF} ({error}); without it no field offset can be looked up"
            )
        })?;
        Self::parse(buffer)
    }

    fn parse(buffer: Vec<u8>) -> Result<Self, String> {
        if buffer.len() < 24 {
            return Err("the BTF blob is too short to hold a header".into());
        }
        let magic = u16::from_ne_bytes([buffer[0], buffer[1]]);
        if magic != BTF_MAGIC {
            return Err(format!(
                "{VMLINUX_BTF} does not start with the BTF magic (found {magic:#06x}); \
                 this reader only understands native-endian BTF"
            ));
        }
        let word = |at: usize| -> u32 {
            u32::from_ne_bytes([buffer[at], buffer[at + 1], buffer[at + 2], buffer[at + 3]])
        };
        let header_len = word(4) as usize;
        let type_offset = header_len + word(8) as usize;
        let type_len = word(12) as usize;
        let string_offset = header_len + word(16) as usize;
        if type_offset + type_len > buffer.len() || string_offset > buffer.len() {
            return Err("the BTF header points past the end of the blob".into());
        }

        let mut types = vec![TypeEntry {
            // Type id 0 is the void type and has no entry in the section.
            kind: 0,
            kind_flag: false,
            vlen: 0,
            name_at: 0,
            size_or_type: 0,
            members_at: 0,
        }];
        let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
        let mut at = type_offset;
        let end = type_offset + type_len;
        while at + 12 <= end {
            let name_off = word(at) as usize;
            let info = word(at + 4);
            let size_or_type = word(at + 8);
            let vlen = (info & 0xffff) as u16;
            let kind = ((info >> 24) & 0x1f) as u8;
            let kind_flag = (info >> 31) == 1;
            let members_at = at + 12;
            let extra = match kind {
                KIND_INT | KIND_VAR | KIND_DECL_TAG => 4,
                KIND_ARRAY => 12,
                KIND_STRUCT | KIND_UNION => vlen as usize * 12,
                KIND_ENUM | KIND_FUNC_PROTO => vlen as usize * 8,
                KIND_DATASEC => vlen as usize * 12,
                KIND_ENUM64 => vlen as usize * 12,
                _ => 0,
            };
            let index = types.len();
            types.push(TypeEntry {
                kind,
                kind_flag,
                vlen,
                name_at: name_off,
                size_or_type,
                members_at,
            });
            if name_off != 0 {
                if let Some(name) = read_string(&buffer, string_offset + name_off) {
                    by_name.entry(name).or_default().push(index);
                }
            }
            at = members_at + extra;
        }

        Ok(Self {
            buffer,
            string_offset,
            types,
            by_name,
        })
    }

    /// Whether a type of this name exists.
    ///
    /// Every tracepoint the kernel defines has a `btf_trace_<name>` type here,
    /// which is how a raw tracepoint can be checked for without tracefs and
    /// without loading anything.
    pub fn has_type(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    /// The type id of `btf_trace_<tracepoint>`, the typedef a `tp_btf`
    /// program names as its `attach_btf_id`.
    pub fn tracepoint_id(&self, tracepoint: &str) -> Option<u32> {
        self.by_name
            .get(&format!("btf_trace_{tracepoint}"))?
            .iter()
            .find(|index| self.types[**index].kind == KIND_TYPEDEF)
            .map(|index| *index as u32)
    }

    /// The type id of the `VAR` named `name`, for a `BPF_PSEUDO_BTF_ID` load:
    /// a program that reads a kernel variable -- `__preempt_count`, `pcpu_hot`
    /// -- names it by this id and the kernel resolves the address at load.
    pub fn var_id(&self, name: &str) -> Option<u32> {
        self.by_name
            .get(name)?
            .iter()
            .find(|index| self.types[**index].kind == KIND_VAR)
            .map(|index| *index as u32)
    }

    /// The byte offset of `member` within `struct_name`, descending into the
    /// anonymous structs and unions the kernel uses to overlay one object on
    /// another.
    ///
    /// An anonymous member has no name of its own, so C addresses its fields as
    /// if they were the outer type's. This does the same, which is not an
    /// extension of the model but the model: `folio.mapping` and
    /// `sk_buff.protocol` are declared inside anonymous unions, and a lookup
    /// that stopped at the direct members would report them absent from a
    /// kernel that has them.
    ///
    /// Direct members are searched before anonymous ones, at every level. C
    /// forbids the ambiguity that would make the order observable -- a name in
    /// an anonymous member may not shadow one of the enclosing type -- so the
    /// rule matters only for a malformed blob, where finding the named member
    /// is the better answer.
    ///
    /// A name that is a typedef of a struct or union -- `possible_net_t`,
    /// a typedef of an anonymous struct -- is followed to the type it names,
    /// through any qualifiers. Structs and unions of that name are searched
    /// first, so a name that is both (`typedef struct foo foo`) answers as it
    /// did before typedefs were followed.
    /// Where a syscall's argument `index` (from zero) sits in the saved
    /// registers a syscall wrapper or `sys_enter` is handed.
    ///
    /// The calling convention is the architecture's: x86-64 passes the six in
    /// `di`, `si`, `dx`, `r10`, `r8` and `r9`; aarch64 in `regs[0]` to
    /// `regs[5]`, the array that opens its `pt_regs`. Read from the running
    /// kernel's BTF either way, so a layout this does not know is refused by
    /// name rather than read as another argument.
    pub fn syscall_argument(&self, index: usize) -> Result<i32, String> {
        const X86_64: [&str; 6] = ["di", "si", "dx", "r10", "r8", "r9"];
        if index >= X86_64.len() {
            return Err(format!("a syscall has six arguments, not {}", index + 1));
        }
        if cfg!(target_arch = "x86_64") {
            self.member_offset("pt_regs", X86_64[index])
        } else if cfg!(target_arch = "aarch64") {
            Ok(self.member_offset("pt_regs", "regs")? + 8 * index as i32)
        } else {
            Err(format!(
                "this build knows where a syscall's arguments are saved on x86_64 and aarch64, \
                 not on {}",
                std::env::consts::ARCH
            ))
        }
    }

    /// Where a kernel function's argument `index` is in a kprobe's `pt_regs`:
    /// the C calling convention, which differs from a syscall's in its fourth
    /// (`cx`, where a syscall saves `r10`).
    pub fn function_argument(&self, index: usize) -> Result<i32, String> {
        const X86_64: [&str; 6] = ["di", "si", "dx", "cx", "r8", "r9"];
        if index >= X86_64.len() {
            return Err(format!(
                "a kprobe reads the first six arguments out of registers, not argument {}",
                index + 1
            ));
        }
        if cfg!(target_arch = "x86_64") {
            self.member_offset("pt_regs", X86_64[index])
        } else if cfg!(target_arch = "aarch64") {
            Ok(self.member_offset("pt_regs", "regs")? + 8 * index as i32)
        } else {
            Err(format!(
                "this build knows a kprobe's argument registers on x86_64 and aarch64, not on {}",
                std::env::consts::ARCH
            ))
        }
    }

    /// Where a kretprobe finds the function's return value in `pt_regs`.
    pub fn return_register(&self) -> Result<i32, String> {
        if cfg!(target_arch = "x86_64") {
            self.member_offset("pt_regs", "ax")
        } else if cfg!(target_arch = "aarch64") {
            self.member_offset("pt_regs", "regs")
        } else {
            Err(format!(
                "this build knows a kretprobe's return register on x86_64 and aarch64, not on {}",
                std::env::consts::ARCH
            ))
        }
    }

    /// The size in bytes of the struct named `struct_name`, for a program that
    /// steps through an array of them: `mm_struct.rss_stat` is an array of
    /// `percpu_counter`, whose size depends on the kernel's configuration.
    pub fn struct_size(&self, struct_name: &str) -> Result<i32, String> {
        self.by_name
            .get(struct_name)
            .and_then(|candidates| {
                candidates
                    .iter()
                    .find(|index| self.types[**index].kind == KIND_STRUCT)
            })
            .map(|index| self.types[*index].size_or_type as i32)
            .ok_or_else(|| format!("this kernel's BTF has no struct named `{struct_name}`"))
    }

    pub fn member_offset(&self, struct_name: &str, member: &str) -> Result<i32, String> {
        let candidates = self
            .by_name
            .get(struct_name)
            .ok_or_else(|| format!("this kernel's BTF has no type named `{struct_name}`"))?;
        let direct = candidates.iter().copied().filter(|index| {
            let kind = self.types[*index].kind;
            kind == KIND_STRUCT || kind == KIND_UNION
        });
        let through_typedef = candidates
            .iter()
            .filter(|index| self.types[**index].kind == KIND_TYPEDEF)
            .filter_map(|index| self.composite(*index as u32));
        for index in direct.chain(through_typedef) {
            if let Some(offset) = self.offset_within(index, member, struct_name, 0)? {
                return Ok(offset);
            }
        }
        Err(format!(
            "this kernel's `{struct_name}` has no member named `{member}`, directly or in an \
             anonymous struct or union inside it"
        ))
    }

    /// The offset of `member` inside one composite type, or `None` when it is
    /// not in there at all. `Err` is reserved for a member that was found and
    /// cannot be addressed, which is a different answer from absence.
    ///
    /// The depth bound is the same argument as `describe`'s: BTF can be cyclic
    /// through a struct that points at itself, and a nesting deeper than this
    /// is not a layout any program here reads.
    fn offset_within(
        &self,
        index: usize,
        member: &str,
        struct_name: &str,
        depth: usize,
    ) -> Result<Option<i32>, String> {
        if depth > 8 {
            return Ok(None);
        }
        let entry = &self.types[index];
        let mut anonymous = Vec::new();
        for slot in 0..entry.vlen as usize {
            let at = entry.members_at + slot * 12;
            let name_off = self.word(at) as usize;
            let member_type = self.word(at + 4);
            let offset = self.word(at + 8);
            // With the kind flag set, the low 24 bits are the bit offset and
            // the top 8 are the bitfield width. Without it, the whole word
            // is the bit offset.
            let bit_offset = if entry.kind_flag {
                offset & 0x00ff_ffff
            } else {
                offset
            };
            let Some(name) = read_string(&self.buffer, self.string_offset + name_off) else {
                continue;
            };
            if name == member {
                if bit_offset % 8 != 0 {
                    return Err(format!(
                        "`{struct_name}.{member}` is a bitfield at bit {bit_offset}, which this \
                         reader cannot address"
                    ));
                }
                return Ok(Some((bit_offset / 8) as i32));
            }
            if name.is_empty() {
                anonymous.push((member_type, bit_offset));
            }
        }
        for (member_type, bit_offset) in anonymous {
            let Some(inner) = self.composite(member_type) else {
                continue;
            };
            if bit_offset % 8 != 0 {
                continue;
            }
            if let Some(offset) = self.offset_within(inner, member, struct_name, depth + 1)? {
                return Ok(Some(offset + (bit_offset / 8) as i32));
            }
        }
        Ok(None)
    }

    /// The index of the struct or union `type_id` names, through whatever
    /// typedefs and qualifiers stand in the way. `None` for anything else: a
    /// pointer to a struct is not a member of the outer type, it is a field in
    /// it, and a program reads it with its own load.
    fn composite(&self, type_id: u32) -> Option<usize> {
        let mut id = type_id as usize;
        for _ in 0..8 {
            let entry = self.types.get(id)?;
            match entry.kind {
                KIND_STRUCT | KIND_UNION => return Some(id),
                KIND_TYPEDEF | KIND_CONST | KIND_VOLATILE | KIND_RESTRICT => {
                    id = entry.size_or_type as usize;
                }
                _ => return None,
            }
        }
        None
    }

    /// The name this kernel gives `value` in `enum_name`, when it has one.
    ///
    /// For a field an event carries as the kernel's own number. The numbering
    /// is a property of the kernel that is running: `enum skb_drop_reason`
    /// grows every release and `enum wb_reason` has lost a member, so a table
    /// compiled in here would be a property of the kernel this was built
    /// against. A mislabelled drop reason does not read as wrong, it reads as a
    /// different fault.
    pub fn enum_variant(&self, enum_name: &str, value: i64) -> Option<&str> {
        for index in self.by_name.get(enum_name)? {
            let entry = &self.types[*index];
            // The kind flag on an enum means its values are signed, which is
            // what decides whether the stored word is widened or sign-extended.
            let signed = entry.kind_flag;
            let (stride, width) = match entry.kind {
                KIND_ENUM => (8, 4),
                KIND_ENUM64 => (12, 8),
                _ => continue,
            };
            for slot in 0..entry.vlen as usize {
                let at = entry.members_at + slot * stride;
                let declared = if width == 4 {
                    let word = self.word(at + 4);
                    if signed {
                        i64::from(word as i32)
                    } else {
                        i64::from(word)
                    }
                } else {
                    let raw = u64::from(self.word(at + 4)) | (u64::from(self.word(at + 8)) << 32);
                    if signed {
                        raw as i64
                    } else if raw > i64::MAX as u64 {
                        continue;
                    } else {
                        raw as i64
                    }
                };
                if declared == value {
                    return self.string_at(self.word(at) as usize);
                }
            }
        }
        None
    }

    /// The arguments a raw tracepoint hands its program, as the running kernel
    /// declares them.
    ///
    /// A raw tracepoint's context is an array of `u64`, one per argument, and
    /// nothing in it says what any of them is. A program reads argument two as
    /// a `task_struct *` because the tracepoint's prototype says so -- and that
    /// prototype changes: `sched_switch` gained a fourth argument in 5.18, and
    /// a program written against the older shape reads the wrong slot on a
    /// newer kernel and produces plausible numbers from an address that is not
    /// a task.
    ///
    /// The kernel publishes the prototype in its own BTF, as
    /// `typedef void (*btf_trace_<name>)(void *, args...)`, so a program can
    /// check the shape it assumes against the kernel it is about to run on
    /// instead of assuming. The leading `void *` is the tracepoint's own data
    /// pointer and is not one of the arguments, so it is dropped here.
    pub fn tracepoint_signature(&self, tracepoint: &str) -> Result<Vec<String>, String> {
        let name = format!("btf_trace_{tracepoint}");
        let typedef = self
            .by_name
            .get(&name)
            .and_then(|candidates| {
                candidates
                    .iter()
                    .find(|index| self.types[**index].kind == KIND_TYPEDEF)
            })
            .ok_or_else(|| {
                format!("this kernel's BTF declares no `{name}`, so `{tracepoint}` is not a raw tracepoint on it")
            })?;
        let pointer = self.entry(self.types[*typedef].size_or_type)?;
        if pointer.kind != KIND_PTR {
            return Err(format!(
                "this kernel's `{name}` is not a function pointer, which is the only shape a \
                 tracepoint prototype takes"
            ));
        }
        let index = pointer.size_or_type;
        let proto = self.entry(index)?;
        if proto.kind != KIND_FUNC_PROTO {
            return Err(format!(
                "this kernel's `{name}` does not point at a prototype"
            ));
        }
        // Parameter zero is the tracepoint's own `void *__data`, which the
        // program never sees: its context starts at the first real argument.
        Ok((1..proto.vlen as usize)
            .map(|slot| {
                let at = proto.members_at + slot * 8;
                self.describe(self.word(at + 4), 0)
            })
            .collect())
    }

    /// A type as a person would write it, for a message that has to be read.
    ///
    /// The depth bound is not defensive programming against the kernel: BTF
    /// can legitimately be cyclic through a struct that points at itself, and
    /// a description is not worth recursing forever for.
    fn describe(&self, type_id: u32, depth: usize) -> String {
        if depth > 4 {
            return "...".to_owned();
        }
        let Ok(entry) = self.entry(type_id) else {
            return "?".to_owned();
        };
        match entry.kind {
            0 => "void".to_owned(),
            KIND_PTR => format!("{} *", self.describe(entry.size_or_type, depth + 1)),
            // A qualifier does not change which slot a program reads, so it is
            // stepped through rather than printed.
            KIND_CONST | KIND_VOLATILE | KIND_RESTRICT => {
                self.describe(entry.size_or_type, depth + 1)
            }
            KIND_INT => read_string(&self.buffer, self.string_offset + entry.name_at)
                .filter(|name| !name.is_empty())
                .map(|name| gcc_integer_name(&name).to_owned())
                .unwrap_or_else(|| "?".to_owned()),
            _ => read_string(&self.buffer, self.string_offset + entry.name_at)
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "?".to_owned()),
        }
    }

    /// A name out of the string section, borrowed rather than copied, for the
    /// callers that hand one straight back to their own caller.
    fn string_at(&self, name_off: usize) -> Option<&str> {
        let rest = self.buffer.get(self.string_offset + name_off..)?;
        let end = rest.iter().position(|byte| *byte == 0)?;
        std::str::from_utf8(&rest[..end]).ok()
    }

    fn entry(&self, type_id: u32) -> Result<&TypeEntry, String> {
        self.types
            .get(type_id as usize)
            .ok_or_else(|| format!("this kernel's BTF has no type {type_id}"))
    }

    fn word(&self, at: usize) -> u32 {
        u32::from_ne_bytes([
            self.buffer[at],
            self.buffer[at + 1],
            self.buffer[at + 2],
            self.buffer[at + 3],
        ])
    }
}

impl std::fmt::Debug for Btf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Btf")
            .field("types", &(self.types.len() - 1))
            .field("bytes", &self.buffer.len())
            .finish()
    }
}

/// Reads a NUL-terminated name out of the string section.
/// An integer's name as gcc spells it, which is how every prototype a program
/// is written for spells it.
///
/// The name of a BTF integer is the compiler's, not the C standard's: gcc
/// writes `long int` and `long unsigned int`, clang writes `long` and
/// `unsigned long`, and both are one type of one width. A clang-built kernel
/// (OrbStack's, Android's, ChromeOS's) declares `sys_enter(pt_regs *, long)`,
/// and compared as text against `long int` it refused eight sources whose
/// slots it holds exactly as the program reads them.
pub(crate) fn gcc_integer_name(name: &str) -> &str {
    match name {
        "long" => "long int",
        "unsigned long" => "long unsigned int",
        "long long" => "long long int",
        "unsigned long long" => "long long unsigned int",
        "short" => "short int",
        "unsigned short" => "short unsigned int",
        other => other,
    }
}

fn read_string(buffer: &[u8], at: usize) -> Option<String> {
    let rest = buffer.get(at..)?;
    let end = rest.iter().position(|byte| *byte == 0)?;
    std::str::from_utf8(&rest[..end]).ok().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a BTF blob by hand, so the parser can be tested against input
    /// this machine's kernel does not happen to produce -- a bitfield, a
    /// truncated header, the wrong magic. Reading only the live kernel tests
    /// one layout and no error path.
    struct BtfBuilder {
        types: Vec<u8>,
        strings: Vec<u8>,
        /// Type ids start at one; zero is void and has no entry.
        next_id: u32,
    }

    impl BtfBuilder {
        fn new() -> Self {
            // Offset zero is the empty string, by definition.
            Self {
                types: Vec::new(),
                strings: vec![0],
                next_id: 1,
            }
        }

        /// Writes one 12-byte type header and returns the type's id.
        fn header(&mut self, name_off: u32, info: u32, size_or_type: u32) -> u32 {
            self.types.extend_from_slice(&name_off.to_ne_bytes());
            self.types.extend_from_slice(&info.to_ne_bytes());
            self.types.extend_from_slice(&size_or_type.to_ne_bytes());
            let id = self.next_id;
            self.next_id += 1;
            id
        }

        fn named(&mut self, kind: u8, name: &str, size_or_type: u32) -> u32 {
            let name_off = self.string(name);
            self.header(name_off, (kind as u32) << 24, size_or_type)
        }

        fn pointer(&mut self, target: u32) -> u32 {
            self.header(0, (KIND_PTR as u32) << 24, target)
        }

        /// A named integer, and its id, for use as a parameter type.
        fn scalar(&mut self, name: &str) -> u32 {
            let id = self.named(KIND_INT, name, 4);
            self.types.extend_from_slice(&0u32.to_ne_bytes());
            id
        }

        /// A named struct with no members, which is all a parameter type has
        /// to be to be pointed at.
        fn opaque(&mut self, name: &str) -> u32 {
            self.named(KIND_STRUCT, name, 8)
        }

        /// A tracepoint as the kernel declares one: a typedef to a pointer to
        /// a prototype whose first parameter is the tracepoint's own data.
        fn tracepoint(&mut self, name: &str, arguments: &[u32]) -> &mut Self {
            let void_star = self.pointer(0);
            let name_off = self.string(&format!("btf_trace_{name}"));
            let info = ((KIND_FUNC_PROTO as u32) << 24) | (arguments.len() as u32 + 1);
            let proto = self.header(0, info, 0);
            for parameter in std::iter::once(&void_star).chain(arguments) {
                self.types.extend_from_slice(&0u32.to_ne_bytes());
                self.types.extend_from_slice(&parameter.to_ne_bytes());
            }
            let pointer = self.pointer(proto);
            self.header(name_off, (KIND_TYPEDEF as u32) << 24, pointer);
            self
        }

        fn string(&mut self, text: &str) -> u32 {
            let at = self.strings.len() as u32;
            self.strings.extend_from_slice(text.as_bytes());
            self.strings.push(0);
            at
        }

        /// A struct with `members` of `(name, bit offset)`, `bitfields` set
        /// when the kind flag is on and the offsets carry a width in their top
        /// eight bits.
        fn structure(&mut self, name: &str, members: &[(&str, u32)], bitfields: bool) -> &mut Self {
            let name_off = self.string(name);
            let mut info = (4u32 << 24) | members.len() as u32;
            if bitfields {
                info |= 1 << 31;
            }
            self.header(name_off, info, 8);
            for (member, offset) in members {
                let member_off = self.string(member);
                self.types.extend_from_slice(&member_off.to_ne_bytes());
                self.types.extend_from_slice(&0u32.to_ne_bytes());
                self.types.extend_from_slice(&offset.to_ne_bytes());
            }
            self
        }

        /// A struct or a union whose members carry a type each, and whose name
        /// may be empty -- which is what makes it anonymous, and is the case
        /// the offsets here exist to test.
        fn composite(
            &mut self,
            kind: u8,
            name: &str,
            members: &[(&str, u32, u32)],
            size: u32,
        ) -> u32 {
            let name_off = if name.is_empty() {
                0
            } else {
                self.string(name)
            };
            let info = ((kind as u32) << 24) | members.len() as u32;
            let id = self.header(name_off, info, size);
            for (member, member_type, offset) in members {
                let member_off = if member.is_empty() {
                    0
                } else {
                    self.string(member)
                };
                self.types.extend_from_slice(&member_off.to_ne_bytes());
                self.types.extend_from_slice(&member_type.to_ne_bytes());
                self.types.extend_from_slice(&offset.to_ne_bytes());
            }
            id
        }

        /// An enum as BTF stores one: a header, then `(name, value)` pairs of
        /// eight bytes each. `signed` sets the kind flag.
        fn enumeration(&mut self, name: &str, variants: &[(&str, i32)], signed: bool) -> u32 {
            let name_off = self.string(name);
            let mut info = ((KIND_ENUM as u32) << 24) | variants.len() as u32;
            if signed {
                info |= 1 << 31;
            }
            let id = self.header(name_off, info, 4);
            for (variant, value) in variants {
                let variant_off = self.string(variant);
                self.types.extend_from_slice(&variant_off.to_ne_bytes());
                self.types.extend_from_slice(&value.to_ne_bytes());
            }
            id
        }

        /// A type of a kind this parser skips over rather than indexes, to
        /// check that it walks past the right number of bytes.
        fn integer(&mut self, name: &str) -> &mut Self {
            // Kind 1, an integer, with a four-byte encoding word of its own.
            self.named(KIND_INT, name, 4);
            self.types.extend_from_slice(&0u32.to_ne_bytes());
            self
        }

        fn build(&self) -> Vec<u8> {
            let mut blob = Vec::new();
            blob.extend_from_slice(&BTF_MAGIC.to_ne_bytes());
            blob.push(1); // version
            blob.push(0); // flags
            blob.extend_from_slice(&24u32.to_ne_bytes()); // header length
            blob.extend_from_slice(&0u32.to_ne_bytes()); // type section offset
            blob.extend_from_slice(&(self.types.len() as u32).to_ne_bytes());
            blob.extend_from_slice(&(self.types.len() as u32).to_ne_bytes()); // strings after
            blob.extend_from_slice(&(self.strings.len() as u32).to_ne_bytes());
            blob.extend_from_slice(&self.types);
            blob.extend_from_slice(&self.strings);
            blob
        }
    }

    #[test]
    fn a_member_offset_is_read_in_bytes_from_a_bit_offset() {
        let blob = BtfBuilder::new()
            .structure("task_like", &[("pid", 0), ("comm", 32)], false)
            .build();
        let btf = Btf::parse(blob).expect("a well-formed blob");
        assert_eq!(btf.member_offset("task_like", "pid"), Ok(0));
        assert_eq!(btf.member_offset("task_like", "comm"), Ok(4));
    }

    /// The id is the one the kernel resolves a `BPF_PSEUDO_BTF_ID` load by,
    /// which is the type's position in the blob. Checked against `bpftool`'s
    /// numbering of the running kernel's `runqueues` (vmlinux BTF carries per-cpu variables only), when `bpftool` is here to
    /// ask, since this reader numbering its own types proves nothing.
    #[test]
    fn a_variable_is_found_by_the_id_the_kernel_numbers_it_with() {
        let Ok(btf) = Btf::from_kernel() else {
            eprintln!("skipped: this kernel publishes no BTF");
            return;
        };
        assert!(
            btf.var_id("task_struct").is_none(),
            "a struct is not a variable"
        );
        let id = btf
            .var_id("runqueues")
            .expect("every SMP kernel has `runqueues`");
        let Ok(dump) = std::process::Command::new("bpftool")
            .args(["btf", "dump", "file", VMLINUX_BTF])
            .output()
        else {
            eprintln!("skipped the cross-check: no bpftool on this machine");
            return;
        };
        // What bpftool answered, when it did not dump: Ubuntu's `bpftool` is a
        // wrapper that looks for the tool built for the running kernel and
        // says which package it wants when that kernel is not the one
        // `linux-tools` was installed for (a CI runner's `-azure` kernel).
        let answered = || {
            let said = String::from_utf8_lossy(&dump.stderr);
            let said = said
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or("nothing on stderr");
            format!("{}: {said}", dump.status)
        };
        if !dump.status.success() {
            eprintln!(
                "skipped the cross-check: bpftool could not dump {VMLINUX_BTF} ({})",
                answered()
            );
            return;
        }
        let text = String::from_utf8_lossy(&dump.stdout);
        // The variable's own line, `[<id>] VAR 'runqueues' ...`: the per-cpu
        // DATASEC lists the same name as a member (`type_id=<id> offset=...
        // (VAR 'runqueues')`), and taking that line first is what made this
        // cross-check skip on every machine that has bpftool.
        let expected: Option<u32> = text.lines().find_map(|line| {
            let (id, rest) = line.strip_prefix('[')?.split_once("] ")?;
            rest.starts_with("VAR 'runqueues' ")
                .then(|| id.parse().ok())?
        });
        // bpftool dumped the kernel's BTF and it has no `runqueues` variable,
        // which this reader just found: one of the two is wrong.
        let expected = expected.unwrap_or_else(|| {
            panic!(
                "bpftool dumped {VMLINUX_BTF} ({} lines) with no `VAR 'runqueues'`, which this \
                 reader found as id {id}",
                text.lines().count()
            )
        });
        assert_eq!(id, expected);
    }

    /// A type this parser does not index still has to be stepped over by
    /// exactly its own length, or every type after it is read from the middle
    /// of the previous one.
    #[test]
    fn a_kind_with_trailing_data_is_walked_past_correctly() {
        let blob = BtfBuilder::new()
            .integer("int")
            .structure("after_the_int", &[("field", 64)], false)
            .build();
        let btf = Btf::parse(blob).expect("a well-formed blob");
        assert!(btf.has_type("after_the_int"), "the walk lost its place");
        assert_eq!(btf.member_offset("after_the_int", "field"), Ok(8));
    }

    /// With the kind flag set, the top byte of a member's offset is a bitfield
    /// width and only the low 24 bits are the offset. Reading the whole word
    /// would put the field megabytes into the structure.
    #[test]
    fn a_bitfield_width_is_not_mistaken_for_part_of_the_offset() {
        let width_and_offset = (3u32 << 24) | 64;
        let blob = BtfBuilder::new()
            .structure("flags_like", &[("packed", width_and_offset)], true)
            .build();
        let btf = Btf::parse(blob).expect("a well-formed blob");
        assert_eq!(btf.member_offset("flags_like", "packed"), Ok(8));
    }

    /// A field that does not start on a byte cannot be addressed by a load, and
    /// the answer is to say so rather than to round to the nearest byte and
    /// read a neighbouring field.
    #[test]
    fn a_field_that_starts_mid_byte_is_refused_rather_than_rounded() {
        let blob = BtfBuilder::new()
            .structure("flags_like", &[("third_bit", (1u32 << 24) | 3)], true)
            .build();
        let btf = Btf::parse(blob).expect("a well-formed blob");
        let error = btf.member_offset("flags_like", "third_bit").unwrap_err();
        assert!(error.contains("bitfield at bit 3"), "{error}");
    }

    /// The first parameter of every tracepoint prototype is the tracepoint's
    /// own `void *__data`, which a program never sees: its context starts at
    /// the argument after it. Counting it would put every argument one slot
    /// out, and a `task_struct *` read one slot early is an address that is
    /// not a task -- from which `bpf_probe_read_kernel` returns whatever is
    /// there, as a number that looks like a pid.
    #[test]
    fn a_tracepoints_own_data_pointer_is_not_one_of_its_arguments() {
        let mut builder = BtfBuilder::new();
        let boolean = builder.scalar("_Bool");
        let unsigned = builder.scalar("unsigned int");
        let task = builder.opaque("task_struct");
        let task_pointer = builder.pointer(task);
        let blob = builder
            .tracepoint(
                "sched_switch",
                &[boolean, task_pointer, task_pointer, unsigned],
            )
            .build();
        let btf = Btf::parse(blob).expect("a well-formed blob");
        assert_eq!(
            btf.tracepoint_signature("sched_switch"),
            Ok(vec![
                "_Bool".to_owned(),
                "task_struct *".to_owned(),
                "task_struct *".to_owned(),
                "unsigned int".to_owned(),
            ])
        );
    }

    /// A clang-built kernel names its integers as clang does; the prototype a
    /// program is written for names them as gcc does, and they are one type.
    #[test]
    fn a_clang_kernel_s_integers_read_as_the_gcc_spelling_programs_are_written_for() {
        let mut builder = BtfBuilder::new();
        let regs = builder.opaque("pt_regs");
        let regs_pointer = builder.pointer(regs);
        let long = builder.scalar("long");
        let unsigned_long = builder.scalar("unsigned long");
        let unsigned = builder.scalar("unsigned int");
        let blob = builder
            .tracepoint("sys_enter", &[regs_pointer, long, unsigned_long, unsigned])
            .build();
        let btf = Btf::parse(blob).expect("a well-formed blob");
        assert_eq!(
            btf.tracepoint_signature("sys_enter"),
            Ok(vec![
                "pt_regs *".to_owned(),
                "long int".to_owned(),
                "long unsigned int".to_owned(),
                "unsigned int".to_owned(),
            ])
        );
    }

    #[test]
    fn a_name_that_is_not_a_tracepoint_is_refused_rather_than_answered_empty() {
        let blob = BtfBuilder::new().integer("int").build();
        let error = Btf::parse(blob)
            .expect("a well-formed blob")
            .tracepoint_signature("not_a_tracepoint")
            .unwrap_err();
        assert!(error.contains("btf_trace_not_a_tracepoint"), "{error}");
    }

    /// Ground truth from the kernel that is running, not from this reader.
    /// `sched_switch` takes four arguments since 5.18 and `sched_waking` one,
    /// and the whole point of reading the prototype is that a program can
    /// refuse a kernel whose shape it was not written for.
    #[test]
    fn the_running_kernel_declares_the_tracepoints_this_layer_attaches_to() {
        let Some(btf) = kernel_btf() else { return };
        assert_eq!(
            btf.tracepoint_signature("sched_switch"),
            Ok(vec![
                "bool".to_owned(),
                "task_struct *".to_owned(),
                "task_struct *".to_owned(),
                "unsigned int".to_owned(),
            ]),
            "the offcpu program reads prev, next and prev_state out of these slots"
        );
        assert_eq!(
            btf.tracepoint_signature("sched_waking"),
            Ok(vec!["task_struct *".to_owned()]),
            "the wakeup program reads the woken task out of slot zero"
        );
    }

    #[test]
    fn a_blob_that_is_not_btf_is_refused_by_its_magic() {
        let error = Btf::parse(vec![0xff; 64]).unwrap_err();
        assert!(error.contains("BTF magic"), "{error}");
    }

    #[test]
    fn a_truncated_blob_is_refused_rather_than_read_past_its_end() {
        assert!(Btf::parse(vec![0u8; 8]).unwrap_err().contains("too short"));

        let mut blob = BtfBuilder::new()
            .structure("task_like", &[("pid", 0)], false)
            .build();
        blob.truncate(30);
        let error = Btf::parse(blob).unwrap_err();
        assert!(error.contains("past the end"), "{error}");
    }

    fn kernel_btf() -> Option<Btf> {
        match Btf::from_kernel() {
            Ok(btf) => Some(btf),
            Err(reason) => {
                eprintln!("skipped: {reason}");
                None
            }
        }
    }

    /// The offsets this layer depends on must come out of the kernel that is
    /// running, and they must be sane: inside a structure that is a few
    /// kilobytes, and ordered the way the source orders them.
    #[test]
    fn task_struct_offsets_are_read_from_the_running_kernel() {
        let Some(btf) = kernel_btf() else { return };
        let pid = btf.member_offset("task_struct", "pid").expect("pid");
        let tgid = btf.member_offset("task_struct", "tgid").expect("tgid");
        let comm = btf.member_offset("task_struct", "comm").expect("comm");
        assert!(
            pid > 0 && pid < 16384,
            "pid at an implausible offset: {pid}"
        );
        assert_eq!(tgid, pid + 4, "tgid follows pid in every kernel so far");
        assert!(comm > pid, "comm is declared after pid");
    }

    #[test]
    fn a_missing_field_is_named_rather_than_guessed() {
        let Some(btf) = kernel_btf() else { return };
        let error = btf
            .member_offset("task_struct", "there_is_no_such_field")
            .unwrap_err();
        assert!(error.contains("there_is_no_such_field"), "{error}");
    }

    #[test]
    fn every_tracepoint_has_a_btf_trace_type() {
        let Some(btf) = kernel_btf() else { return };
        assert!(
            btf.has_type("btf_trace_sched_waking"),
            "sched_waking is a tracepoint on every kernel this tool supports"
        );
        assert!(!btf.has_type("btf_trace_not_a_tracepoint"));
    }

    #[test]
    fn a_member_of_an_anonymous_struct_is_addressed_as_if_it_were_the_outer_type_s() {
        // struct outer { u32 head; struct { u32 wanted; }; };
        // The anonymous member starts at byte 8, and `wanted` is 4 bytes into
        // it, so the answer is 12 -- which is what C would compute.
        let mut builder = BtfBuilder::new();
        let scalar = builder.scalar("u32");
        let inner = builder.composite(KIND_STRUCT, "", &[("wanted", scalar, 4 * 8)], 8);
        builder.composite(
            KIND_STRUCT,
            "outer",
            &[("head", scalar, 0), ("", inner, 8 * 8)],
            16,
        );
        let btf = Btf::parse(builder.build()).expect("the blob parses");

        assert_eq!(btf.member_offset("outer", "head"), Ok(0));
        assert_eq!(btf.member_offset("outer", "wanted"), Ok(12));
    }

    #[test]
    fn a_typedef_of_an_anonymous_struct_is_followed_to_its_members() {
        // typedef struct { u32 pad; struct net *net; } possible_like_t;
        // reached through a `const`, as a qualifier may stand in the way.
        let mut builder = BtfBuilder::new();
        let scalar = builder.scalar("u32");
        let pointer = builder.pointer(scalar);
        let inner = builder.composite(
            KIND_STRUCT,
            "",
            &[("pad", scalar, 0), ("net", pointer, 8 * 8)],
            16,
        );
        let constant = builder.header(0, (KIND_CONST as u32) << 24, inner);
        builder.named(KIND_TYPEDEF, "possible_like_t", constant);
        let btf = Btf::parse(builder.build()).expect("the blob parses");

        assert_eq!(btf.member_offset("possible_like_t", "net"), Ok(8));
        let error = btf.member_offset("possible_like_t", "absent").unwrap_err();
        assert!(error.contains("absent"), "{error}");
    }

    #[test]
    fn a_typedef_of_something_not_composite_is_still_refused() {
        let mut builder = BtfBuilder::new();
        let scalar = builder.scalar("u32");
        builder.named(KIND_TYPEDEF, "u32_like_t", scalar);
        let btf = Btf::parse(builder.build()).expect("the blob parses");
        assert!(btf.member_offset("u32_like_t", "anything").is_err());
    }

    #[test]
    fn a_struct_and_a_typedef_of_one_name_answer_from_the_struct() {
        // `typedef struct other shared;` beside `struct shared`: the struct is
        // what callers have always been answered from.
        let mut builder = BtfBuilder::new();
        let scalar = builder.scalar("u32");
        let other = builder.composite(KIND_STRUCT, "other", &[("field", scalar, 4 * 8)], 8);
        builder.named(KIND_TYPEDEF, "shared", other);
        builder.composite(KIND_STRUCT, "shared", &[("field", scalar, 0)], 4);
        let btf = Btf::parse(builder.build()).expect("the blob parses");
        assert_eq!(btf.member_offset("shared", "field"), Ok(0));
    }

    /// Following typedefs must not change an answer any caller already gets:
    /// on the running kernel, no type name a caller passes is also a typedef
    /// naming a different struct or union, or a typedef with no struct of its
    /// own. A name the kernel's BTF does not hold is named, not checked. The list is every literal first
    /// argument of `member_offset` under `src/bpf/` that names a kernel type.
    #[test]
    fn no_caller_s_type_is_shadowed_by_a_typedef_on_the_running_kernel() {
        let Some(btf) = kernel_btf() else { return };
        const CALLERS: &[&str] = &[
            "address_space",
            "backing_dev_info",
            "bdi_writeback",
            "block_device",
            "bpf_prog_info",
            "cgroup",
            "css_set",
            "fdtable",
            "file",
            "files_struct",
            "folio",
            "inet_connection_sock",
            "inet_sock",
            "inode",
            "irqaction",
            "kernfs_node",
            "linux_binprm",
            "net",
            "net_device",
            "netdev_queue",
            "ns_common",
            "pcpu_hot",
            "pt_regs",
            "Qdisc",
            "request",
            "signal_struct",
            "sk_buff",
            "sock",
            "sock_common",
            "socket",
            "socket_alloc",
            "super_block",
            "task_struct",
            "tcp_sock",
            "wb_writeback_work",
        ];
        let mut absent = Vec::new();
        for name in CALLERS {
            let Some(candidates) = btf.by_name.get(*name) else {
                absent.push(*name);
                continue;
            };
            let structs: Vec<usize> = candidates
                .iter()
                .copied()
                .filter(|index| matches!(btf.types[*index].kind, KIND_STRUCT | KIND_UNION))
                .collect();
            for index in candidates {
                if btf.types[*index].kind != KIND_TYPEDEF {
                    continue;
                }
                if let Some(target) = btf.composite(*index as u32) {
                    // A typedef alone, with no struct of the name, would turn
                    // what was a refusal (read as "missing" by the probes)
                    // into an offset: an answer that moved too.
                    assert!(
                        !structs.is_empty(),
                        "`{name}` is only a typedef here, so following it changes a refusal \
                         into an offset"
                    );
                    assert!(
                        structs.contains(&target),
                        "`{name}` is both a struct and a typedef of another type"
                    );
                }
            }
        }
        if !absent.is_empty() {
            eprintln!(
                "not in this kernel's BTF, so not checked here: {}",
                absent.join(", ")
            );
        }
    }

    /// The case that asked for this: `net_device.nd_net` is a
    /// `possible_net_t`, a typedef of an anonymous struct whose only member,
    /// under `CONFIG_NET_NS`, is the `net` pointer.
    #[test]
    fn the_running_kernel_s_possible_net_t_is_read_through_its_typedef() {
        let Some(btf) = kernel_btf() else { return };
        if !btf.has_type("possible_net_t") {
            eprintln!("skipped: this kernel's BTF has no possible_net_t");
            return;
        }
        match btf.member_offset("possible_net_t", "net") {
            Ok(offset) => assert_eq!(offset, 0, "the pointer is the struct's only member"),
            Err(reason) => {
                // Without CONFIG_NET_NS the struct is empty: the refusal must
                // name the member, not the typedef.
                assert!(reason.contains("`net`"), "{reason}");
            }
        }
    }

    #[test]
    fn a_member_two_anonymous_levels_down_carries_both_enclosing_offsets() {
        // This is `struct folio`'s shape: a struct inside a union inside the
        // outer type, which is where `mapping` lives and why a lookup that
        // stopped at the direct members reported it absent.
        let mut builder = BtfBuilder::new();
        let scalar = builder.scalar("u32");
        let deepest = builder.composite(KIND_STRUCT, "", &[("mapping", scalar, 3 * 8)], 8);
        let union = builder.composite(KIND_UNION, "", &[("", deepest, 0)], 8);
        builder.composite(
            KIND_STRUCT,
            "folio_like",
            &[("flags", scalar, 0), ("", union, 8 * 8)],
            16,
        );
        let btf = Btf::parse(builder.build()).expect("the blob parses");

        assert_eq!(btf.member_offset("folio_like", "mapping"), Ok(11));
    }

    #[test]
    fn a_named_member_is_preferred_to_one_inside_an_anonymous_member() {
        let mut builder = BtfBuilder::new();
        let scalar = builder.scalar("u32");
        let inner = builder.composite(KIND_STRUCT, "", &[("field", scalar, 0)], 8);
        builder.composite(
            KIND_STRUCT,
            "outer",
            &[("field", scalar, 0), ("", inner, 8 * 8)],
            16,
        );
        let btf = Btf::parse(builder.build()).expect("the blob parses");

        assert_eq!(btf.member_offset("outer", "field"), Ok(0));
    }

    #[test]
    fn a_member_in_no_anonymous_branch_is_still_refused_by_name() {
        let mut builder = BtfBuilder::new();
        let scalar = builder.scalar("u32");
        let inner = builder.composite(KIND_STRUCT, "", &[("elsewhere", scalar, 0)], 8);
        builder.composite(KIND_STRUCT, "outer", &[("", inner, 0)], 8);
        let btf = Btf::parse(builder.build()).expect("the blob parses");

        let error = btf.member_offset("outer", "absent").unwrap_err();
        assert!(error.contains("absent"), "{error}");
        assert!(error.contains("anonymous"), "{error}");
    }

    #[test]
    fn a_pointer_to_a_struct_is_a_field_rather_than_a_member_to_descend_into() {
        // `folio.mapping` is an `address_space *`, and `address_space.host` is
        // not one of `folio`'s members. Following a pointer would answer an
        // offset in the wrong object.
        let mut builder = BtfBuilder::new();
        let scalar = builder.scalar("u32");
        let pointed = builder.composite(KIND_STRUCT, "pointed", &[("host", scalar, 0)], 8);
        let pointer = builder.pointer(pointed);
        builder.composite(KIND_STRUCT, "outer", &[("", pointer, 0)], 8);
        let btf = Btf::parse(builder.build()).expect("the blob parses");

        assert!(btf.member_offset("outer", "host").is_err());
    }

    #[test]
    fn an_enum_value_is_named_by_the_kernel_rather_than_by_a_table() {
        let mut builder = BtfBuilder::new();
        builder.enumeration(
            "reason_like",
            &[("FIRST", 0), ("SECOND", 1), ("LATER", 47)],
            false,
        );
        let btf = Btf::parse(builder.build()).expect("the blob parses");

        assert_eq!(btf.enum_variant("reason_like", 0), Some("FIRST"));
        assert_eq!(btf.enum_variant("reason_like", 47), Some("LATER"));
        // A value this kernel does not declare has no name, and saying so is
        // the whole point: a number with no name is better than a wrong name.
        assert_eq!(btf.enum_variant("reason_like", 48), None);
        assert_eq!(btf.enum_variant("no_such_enum", 0), None);
    }

    #[test]
    fn a_signed_enum_s_negative_value_is_not_read_as_a_large_positive_one() {
        let mut builder = BtfBuilder::new();
        builder.enumeration("errno_like", &[("OK", 0), ("FAILED", -1)], true);
        let btf = Btf::parse(builder.build()).expect("the blob parses");

        assert_eq!(btf.enum_variant("errno_like", -1), Some("FAILED"));
        assert_eq!(btf.enum_variant("errno_like", 4_294_967_295), None);
    }

    #[test]
    fn the_running_kernel_declares_the_overlaid_fields_three_collectors_could_not_reach() {
        let Some(btf) = kernel_btf() else {
            return;
        };
        // Each of these is inside an anonymous union or struct on every kernel
        // that has it, and each is a field a sixth-wave collector shipped
        // without: `net.drops` carried no interface, and `io.pagecache` could
        // not reach the inode behind a folio.
        let mut offsets = Vec::new();
        for (structure, member) in [
            ("sk_buff", "protocol"),
            ("sk_buff", "skb_iif"),
            ("sock_common", "skc_daddr"),
            ("sock_common", "skc_dport"),
        ] {
            let offset = btf
                .member_offset(structure, member)
                .unwrap_or_else(|error| panic!("{structure}.{member}: {error}"));
            offsets.push((structure, member, offset));
        }
        // What is asserted is that each one resolves, and that they are not all
        // the same field -- not where any of them is. `skc_daddr` is at zero on
        // this kernel, being the first member of the union it lives in, which
        // is why an "overlaid fields are not at zero" assertion would have been
        // asserting the layout rather than the lookup.
        let daddr = offsets[2].2;
        let dport = offsets[3].2;
        assert_ne!(
            daddr, dport,
            "skc_daddr and skc_dport resolved to one offset: {offsets:?}"
        );
    }

    #[test]
    fn the_running_kernel_names_its_own_drop_reasons() {
        let Some(btf) = kernel_btf() else {
            return;
        };
        // Zero is `SKB_NOT_DROPPED_YET` on this kernel and was expected here to
        // be `SKB_DROP_REASON_NOT_SPECIFIED`, which is the argument for reading
        // the table out of the running kernel in one line: the guess was a
        // name, it was wrong, and a wrong name does not read as wrong -- it
        // reads as a different fault.
        let first = btf.enum_variant("skb_drop_reason", 0);
        let first = first.unwrap_or_else(|| panic!("this kernel names no drop reason zero"));
        assert!(first.starts_with("SKB_"), "{first}");

        let far_past_the_end = btf.enum_variant("skb_drop_reason", 100_000);
        assert_eq!(far_past_the_end, None, "{far_past_the_end:?}");
    }
}
