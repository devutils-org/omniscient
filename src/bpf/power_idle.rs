// SPDX-License-Identifier: MIT OR GPL-2.0
//! Every idle transition, from `power:cpu_idle`.
//!
//! The cpuidle framework announces a cpu entering an idle state with the
//! state's index, and leaving it with `PWR_EVENT_EXIT` (`u32::MAX`), both on
//! the cpu that idles. The program writes one record per announcement --
//! when, which state, which cpu -- and the collector pairs each exit with the
//! entry before it on the same cpu: an idle period, which is what cpuidle's
//! `time` counters add up, and what they cannot say: when, and how long each
//! one was.
//!
//! The arguments are two numbers, so the raw tracepoint's slots and the
//! classic record's fields carry the same facts; this program reads the raw
//! ones.

use super::btf::Btf;
use super::insn::{helper, Insn, Program, BPF_DW, BPF_W, R10};
use super::proc_events::{publish, EventStream};

/// `cpu_idle(state, cpu_id)` as BTF spells it.
pub(super) const SIGNATURE: [&str; 2] = ["unsigned int", "unsigned int"];

/// The state a cpu announces when it leaves idle.
pub const EXIT: u32 = u32::MAX;

/// One announcement, as the program writes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdleEdge {
    pub ts_ns: u64,
    /// The state entered, or [`EXIT`].
    pub state: u32,
    pub cpu: u32,
}

const EDGE_BYTES: usize = 16;

impl IdleEdge {
    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < EDGE_BYTES {
            return None;
        }
        let word = |at: usize| u32::from_ne_bytes(bytes[at..at + 4].try_into().expect("4"));
        Some(Self {
            ts_ns: u64::from_ne_bytes(bytes[0..8].try_into().expect("8")),
            state: word(8),
            cpu: word(12),
        })
    }
}

const EDGE: i16 = -16;
const STATE: i16 = EDGE + 8;
const CPU: i16 = EDGE + 12;
const DROP_KEY: i16 = EDGE - 8;

fn instructions(btf: &Btf, ring_fd: i32, drops_fd: i32) -> Result<Vec<Insn>, String> {
    super::program::prototype(btf, "cpu_idle", &[&SIGNATURE], "the state and the cpu")?;
    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, EDGE, 0));
    // The raw tracepoint hands each argument in an eight-byte slot.
    program.push(Insn::ldx(BPF_DW, 1, 6, 0));
    program.push(Insn::stx(BPF_W, R10, STATE, 1));
    program.push(Insn::ldx(BPF_DW, 1, 6, 8));
    program.push(Insn::stx(BPF_W, R10, CPU, 1));
    publish(&mut program, ring_fd, drops_fd, EDGE, EDGE_BYTES, DROP_KEY);
    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());
    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// The `cpu_idle` stream.
pub struct IdleProgram(EventStream);

impl IdleProgram {
    /// See `EventStream::skipped`.
    pub fn skipped(&self) -> u64 {
        self.0.skipped()
    }

    pub fn load(requested_ring_bytes: usize) -> Result<Self, String> {
        EventStream::load(
            "cpu_idle",
            "omn_cpu_idle",
            requested_ring_bytes,
            instructions,
        )
        .map(Self)
    }

    pub fn drain(&mut self, handle: impl FnMut(IdleEdge), undecoded: &mut u64) {
        self.0.drain(IdleEdge::from_bytes, handle, undecoded);
    }

    /// Announcements the program could not publish because the ring was full.
    pub fn dropped(&self) -> u64 {
        self.0.dropped()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_decodes_and_a_short_one_does_not() {
        let mut bytes = vec![0u8; EDGE_BYTES];
        bytes[0..8].copy_from_slice(&99u64.to_ne_bytes());
        bytes[8..12].copy_from_slice(&EXIT.to_ne_bytes());
        bytes[12..16].copy_from_slice(&7u32.to_ne_bytes());
        assert_eq!(
            IdleEdge::from_bytes(&bytes),
            Some(IdleEdge {
                ts_ns: 99,
                state: EXIT,
                cpu: 7
            })
        );
        assert_eq!(IdleEdge::from_bytes(&bytes[..EDGE_BYTES - 1]), None);
    }

    #[test]
    fn the_program_builds_against_this_kernel() {
        let Ok(btf) = Btf::from_kernel() else {
            eprintln!("skipped: no kernel BTF");
            return;
        };
        let program = instructions(&btf, 3, 4).expect("a program");
        assert!(program
            .iter()
            .any(|insn| insn.code == 0x85 && insn.imm == helper::RINGBUF_OUTPUT));
    }
}
