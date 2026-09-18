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
const KIND_ARRAY: u8 = 3;
const KIND_STRUCT: u8 = 4;
const KIND_UNION: u8 = 5;
const KIND_ENUM: u8 = 6;
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
    /// Offset of the first member, immediately after the 12-byte header.
    members_at: usize,
}

/// The kernel's BTF, indexed by type name.
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
            let _ = size_or_type;
            let index = types.len();
            types.push(TypeEntry {
                kind,
                kind_flag,
                vlen,
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

    /// The byte offset of `member` within `struct_name`.
    pub fn member_offset(&self, struct_name: &str, member: &str) -> Result<i32, String> {
        let candidates = self
            .by_name
            .get(struct_name)
            .ok_or_else(|| format!("this kernel's BTF has no type named `{struct_name}`"))?;
        for index in candidates {
            let entry = &self.types[*index];
            if entry.kind != KIND_STRUCT && entry.kind != KIND_UNION {
                continue;
            }
            for slot in 0..entry.vlen as usize {
                let at = entry.members_at + slot * 12;
                let name_off = self.word(at) as usize;
                let offset = self.word(at + 8);
                let Some(name) = read_string(&self.buffer, self.string_offset + name_off) else {
                    continue;
                };
                if name != member {
                    continue;
                }
                // With the kind flag set, the low 24 bits are the bit offset and
                // the top 8 are the bitfield width. Without it, the whole word
                // is the bit offset.
                let bit_offset = if entry.kind_flag {
                    offset & 0x00ff_ffff
                } else {
                    offset
                };
                if bit_offset % 8 != 0 {
                    return Err(format!(
                        "`{struct_name}.{member}` is a bitfield at bit {bit_offset}, which this \
                         reader cannot address"
                    ));
                }
                return Ok((bit_offset / 8) as i32);
            }
        }
        Err(format!(
            "this kernel's `{struct_name}` has no member named `{member}`"
        ))
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

/// Reads a NUL-terminated name out of the string section.
fn read_string(buffer: &[u8], at: usize) -> Option<String> {
    let rest = buffer.get(at..)?;
    let end = rest.iter().position(|byte| *byte == 0)?;
    std::str::from_utf8(&rest[..end]).ok().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
