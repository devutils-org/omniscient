//! `net.qdisc`: how long a packet waited in a queueing discipline.
//!
//! Two tracepoints, and the measurement is the gap between them. `qdisc_enqueue`
//! says a packet was handed to a queueing discipline; `qdisc_dequeue` says the
//! discipline gave it back to the driver, and neither says anything on its own.
//! So one program writes the moment the packet was queued, keyed by the buffer,
//! and the other reads it back.
//!
//! **This is the only place transmit latency is visible at all.** A packet held
//! by `fq_codel` behind a full queue, shaped by `tbf`, or delayed on purpose by
//! `netem` is a packet the application has already handed to the kernel: it is
//! gone from every socket-level measurement, and the device counters see it only
//! when it finally leaves. `net.iface` counts what the device lost; this counts
//! what the queue held.
//!
//! **The catalog names only `qdisc_dequeue`**, and this needs both. That is a
//! gap in the entry rather than in the kernel -- every kernel with one has the
//! other -- and the request to add the requirement is under `## Needs from
//! another stream` in `docs/open-findings.md`. A kernel that really has only
//! one of them is refused by name here rather than reporting the dequeue rate
//! as a latency.
//!
//! **The histogram is built in the kernel.** A dequeue happens once per packet
//! transmitted, which on a loaded link is a hundred thousand times a second,
//! and one ring record per packet would make the measurement cost grow with the
//! traffic. What the kernel keeps is a count and an exact total per
//! `(interface, discipline, log2 bucket)`.

use std::os::fd::{AsRawFd, OwnedFd};

use super::btf::Btf;
use super::insn::{helper, Insn, Program, BPF_DW, BPF_W, R10};
use super::map::{map_failure, Counters, Hash};
use super::net_program::{
    bucket_low_ns, count_drop, expect_signature, load, push_log2_bucket, read_field, read_narrow,
    read_pointer,
};
use super::syscall;

/// The prototypes these programs are written against.
const ENQUEUE_SIGNATURE: [&str; 3] = ["Qdisc *", "netdev_queue *", "sk_buff *"];
const DEQUEUE_SIGNATURE: [&str; 4] = ["Qdisc *", "netdev_queue *", "int", "sk_buff *"];

/// Arguments, as byte offsets into each tracepoint's context.
const ENQUEUE_ARG_SKB: i16 = 16;
const ARG_QDISC: i16 = 0;
const ARG_TXQ: i16 = 8;
const DEQUEUE_ARG_SKB: i16 = 24;

/// How many packets may be inside queueing disciplines at once before the
/// program stops being able to time new ones. A shaped link holding ten
/// thousand packets is a badly shaped link, and beyond that the losses are
/// counted.
const MAX_QUEUED: u32 = 65536;

/// How many distinct `(interface, discipline, bucket)` rows one interval may
/// hold. A handful of interfaces times a few dozen buckets, whatever the
/// traffic does, which is the point of counting in the kernel.
const MAX_ROWS: u32 = 16384;

/// Slots of the counter map.
const DROP_QUEUED: u32 = 0;
const DROP_ROWS: u32 = 1;
const UNPAIRED_DEQUEUES: u32 = 2;
const COUNTER_SLOTS: u32 = 3;

/// Where each map's key and value live on the program's stack.
const ROW_VALUE: i16 = -64;
const ROW_KEY: i16 = -80;
const QUEUED_VALUE: i16 = -96;
const QUEUED_KEY: i16 = -104;
const DROP_KEY: i16 = -108;
const SCRATCH: i16 = -120;

/// The layout of a row key.
const KEY_IFINDEX: i16 = 0;
/// The discipline's own handle, which is what `tc` prints as `1:` or `8001:`.
/// In the key because a device can have a hierarchy of them and the whole
/// question is which one held the packet.
const KEY_HANDLE: i16 = 4;
const KEY_BUCKET: i16 = 8;
const ROW_KEY_BYTES: usize = 12;

/// The layout of a row value.
const VALUE_COUNT: i16 = 0;
const VALUE_TOTAL_NS: i16 = 8;
/// The interface's name, read once when the row is created. The index alone is
/// a number nobody recognises, and an index is only meaningful inside the
/// network namespace that assigned it.
const VALUE_IFNAME: i16 = 16;
const IFNAME_BYTES: usize = 16;
const ROW_VALUE_BYTES: usize = 32;

const QUEUED_KEY_BYTES: usize = 8;
const QUEUED_VALUE_BYTES: usize = 8;

/// One `(interface, discipline, bucket)` row's packets over one drain interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QdiscWait {
    /// The kernel's interface index, in the network namespace the packet was
    /// queued in -- which is not necessarily this process's.
    pub ifindex: u32,
    /// The discipline's handle, as `tc` prints its major number.
    pub handle: u32,
    /// The log2 bucket, in `emit::BucketLayout::Log2`'s numbering.
    pub bucket: u32,
    pub count: u64,
    /// The exact total of the waits in this bucket.
    pub total_ns: u64,
    pub ifname: String,
}

impl QdiscWait {
    /// The low bound of this row's bucket, which is the value on the event.
    pub fn bucket_low_ns(&self) -> u64 {
        bucket_low_ns(self.bucket)
    }

    fn decode(key: &[u8], value: &[u8]) -> Option<Self> {
        if key.len() < ROW_KEY_BYTES || value.len() < ROW_VALUE_BYTES {
            return None;
        }
        let word =
            |bytes: &[u8], at: usize| u32::from_ne_bytes(bytes[at..at + 4].try_into().expect("4"));
        let long =
            |bytes: &[u8], at: usize| u64::from_ne_bytes(bytes[at..at + 8].try_into().expect("8"));
        let name = &value[VALUE_IFNAME as usize..VALUE_IFNAME as usize + IFNAME_BYTES];
        let end = name
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(name.len());
        Some(Self {
            ifindex: word(key, KEY_IFINDEX as usize),
            handle: word(key, KEY_HANDLE as usize),
            bucket: word(key, KEY_BUCKET as usize),
            count: long(value, VALUE_COUNT as usize),
            total_ns: long(value, VALUE_TOTAL_NS as usize),
            ifname: String::from_utf8_lossy(&name[..end]).into_owned(),
        })
    }
}

/// The program on `qdisc_enqueue`: a packet has just entered a discipline.
fn enqueue_instructions(btf: &Btf, queued_fd: i32, drops_fd: i32) -> Result<Vec<Insn>, String> {
    expect_signature(btf, "qdisc_enqueue", &[&ENQUEUE_SIGNATURE])?;

    let mut program = Program::new();
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::ldx(BPF_DW, 7, 6, ENQUEUE_ARG_SKB));
    // A discipline can be handed nothing; the buffer is the key and there is no
    // wait without one.
    program.jump_to("done", Insn::jeq_imm(7, 0, 0));

    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::stx(BPF_DW, R10, QUEUED_VALUE, 0));
    program.push(Insn::stx(BPF_DW, R10, QUEUED_KEY, 7));

    program.extend(Insn::load_map_fd(1, queued_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, QUEUED_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, QUEUED_VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_QUEUED, "done");

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// The program on `qdisc_dequeue`: the discipline has released the packet.
fn dequeue_instructions(
    btf: &Btf,
    queued_fd: i32,
    rows_fd: i32,
    drops_fd: i32,
    only_ifindex: Option<u32>,
) -> Result<Vec<Insn>, String> {
    expect_signature(btf, "qdisc_dequeue", &[&DEQUEUE_SIGNATURE])?;
    let dev_at = btf.member_offset("netdev_queue", "dev")?;
    let ifindex_at = btf.member_offset("net_device", "ifindex")?;
    let ifname_at = btf.member_offset("net_device", "name")?;
    let handle_at = btf.member_offset("Qdisc", "handle")?;

    let mut program = Program::new();
    // r6 keeps the context, r7 whichever structure is being read, r8 the moment
    // the packet left and r9 its wait; the last three survive helper calls.
    program.push(Insn::mov64_reg(6, 1));
    program.push(Insn::ldx(BPF_DW, 7, 6, DEQUEUE_ARG_SKB));
    // A dequeue that produced no packet: the queue was empty and the discipline
    // said so. There is no wait to close.
    program.jump_to("done", Insn::jeq_imm(7, 0, 0));
    program.push(Insn::stx(BPF_DW, R10, QUEUED_KEY, 7));

    program.push(Insn::call(helper::KTIME_GET_NS));
    program.push(Insn::mov64_reg(8, 0));

    program.extend(Insn::load_map_fd(1, queued_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, QUEUED_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    // A packet this run never saw enqueued: it was already in the queue when
    // the programs attached, or its enqueue was lost. Counted apart from a
    // loss, because at the start of every run there are some.
    program.jump_to("unpaired", Insn::jeq_imm(0, 0, 0));
    program.push(Insn::ldx(BPF_DW, 9, 0, 0));
    program.extend(Insn::load_map_fd(1, queued_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, QUEUED_KEY as i32));
    program.push(Insn::call(helper::MAP_DELETE_ELEM));

    // r9 = now - when it was queued, through r1, which nothing between here and
    // the next call reads.
    program.push(Insn::mov64_reg(1, 8));
    program.push(Insn::sub64_reg(1, 9));
    program.push(Insn::mov64_reg(9, 1));

    // The interface, through the transmit queue the discipline belongs to.
    program.push(Insn::ldx(BPF_DW, 7, 6, ARG_TXQ));
    read_pointer(&mut program, SCRATCH, 7, 7, dev_at);
    program.push(Insn::st_imm(BPF_W, R10, ROW_KEY + KEY_IFINDEX, 0));
    program.push(Insn::st_imm(BPF_DW, R10, ROW_VALUE + VALUE_IFNAME, 0));
    program.push(Insn::st_imm(BPF_DW, R10, ROW_VALUE + VALUE_IFNAME + 8, 0));
    program.jump_to("named", Insn::jeq_imm(7, 0, 0));
    read_narrow(&mut program, ROW_KEY + KEY_IFINDEX, 4, 7, ifindex_at);
    read_field(
        &mut program,
        ROW_VALUE + VALUE_IFNAME,
        IFNAME_BYTES as i32,
        7,
        ifname_at,
    );
    program.label("named");

    if let Some(ifindex) = only_ifindex {
        // A `--iface` target, compared in the kernel: the row is keyed by the
        // interface, so a packet on another one is dropped before it is
        // counted rather than after it is drained.
        program.push(Insn::ldx(BPF_W, 1, R10, ROW_KEY + KEY_IFINDEX));
        program.jump_to("done", Insn::jne_imm(1, ifindex as i32, 0));
    }

    program.push(Insn::ldx(BPF_DW, 7, 6, ARG_QDISC));
    program.push(Insn::st_imm(BPF_W, R10, ROW_KEY + KEY_HANDLE, 0));
    program.jump_to("handled", Insn::jeq_imm(7, 0, 0));
    read_narrow(&mut program, ROW_KEY + KEY_HANDLE, 4, 7, handle_at);
    program.label("handled");

    push_log2_bucket(&mut program, 9);
    program.push(Insn::stx(BPF_W, R10, ROW_KEY + KEY_BUCKET, 3));

    program.extend(Insn::load_map_fd(1, rows_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, ROW_KEY as i32));
    program.push(Insn::call(helper::MAP_LOOKUP_ELEM));
    program.jump_to("first", Insn::jeq_imm(0, 0, 0));
    // One allocation shared by every cpu, so both increments have to be ones
    // the other cpus cannot interleave with.
    program.push(Insn::mov64_imm(1, 1));
    program.push(Insn::atomic_add(BPF_DW, 0, VALUE_COUNT, 1));
    program.push(Insn::atomic_add(BPF_DW, 0, VALUE_TOTAL_NS, 9));
    program.jump_to("done", Insn::ja());

    program.label("first");
    program.push(Insn::st_imm(BPF_DW, R10, ROW_VALUE + VALUE_COUNT, 1));
    program.push(Insn::stx(BPF_DW, R10, ROW_VALUE + VALUE_TOTAL_NS, 9));
    program.extend(Insn::load_map_fd(1, rows_fd));
    program.push(Insn::mov64_reg(2, R10));
    program.push(Insn::add64_imm(2, ROW_KEY as i32));
    program.push(Insn::mov64_reg(3, R10));
    program.push(Insn::add64_imm(3, ROW_VALUE as i32));
    program.push(Insn::mov64_imm(4, 0));
    program.push(Insn::call(helper::MAP_UPDATE_ELEM));
    program.jump_to("done", Insn::jsge_imm(0, 0, 0));
    count_drop(&mut program, drops_fd, DROP_KEY, DROP_ROWS, "done");
    program.jump_to("done", Insn::ja());

    program.label("unpaired");
    count_drop(&mut program, drops_fd, DROP_KEY, UNPAIRED_DEQUEUES, "done");

    program.label("done");
    program.push(Insn::mov64_imm(0, 0));
    program.push(Insn::exit());

    let instructions = program.build();
    super::insn::audit(&instructions)?;
    Ok(instructions)
}

/// Two loaded, attached programs and the maps they share.
pub struct QdiscProgram {
    rows: Hash,
    queued: Hash,
    counters: Counters,
    enqueue_program: OwnedFd,
    _enqueue_link: OwnedFd,
    dequeue_program: OwnedFd,
    _dequeue_link: OwnedFd,
}

/// What one drain found, beside the measurements.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct QdiscDrain {
    pub rows: Vec<QdiscWait>,
    pub vanished: u64,
    pub truncated: bool,
}

impl QdiscProgram {
    /// Loads and attaches both programs, or says why the kernel refused.
    ///
    /// `only_ifindex` narrows the measurement to one interface, compared inside
    /// the program so that a packet on another one is never counted.
    pub fn load(only_ifindex: Option<u32>) -> Result<Self, String> {
        let btf = super::kernel_btf()?;

        let queued = Hash::create(
            QUEUED_KEY_BYTES,
            QUEUED_VALUE_BYTES,
            MAX_QUEUED,
            "omn_qd_wait",
        )
        .map_err(|error| map_failure("queued packet map", error))?;
        let rows = Hash::create(ROW_KEY_BYTES, ROW_VALUE_BYTES, MAX_ROWS, "omn_qd_hist")
            .map_err(|error| map_failure("qdisc histogram map", error))?;
        let counters = Counters::create(COUNTER_SLOTS, "omn_qd_lost")
            .map_err(|error| map_failure("qdisc counters", error))?;

        let enqueue = enqueue_instructions(btf, queued.as_raw_fd(), counters.as_raw_fd())?;
        let dequeue = dequeue_instructions(
            btf,
            queued.as_raw_fd(),
            rows.as_raw_fd(),
            counters.as_raw_fd(),
            only_ifindex,
        )?;
        let enqueue_program = load(&enqueue, "omn_qd_in")?;
        let dequeue_program = load(&dequeue, "omn_qd_out")?;

        // The enqueue program is attached first, on purpose. A wait opened by a
        // program whose partner is not attached yet is never closed and ages
        // out of the map; a dequeue seen before any enqueue was recorded would
        // close a wait nobody opened, and the lookup that fails is the only
        // thing standing between that and a duration measured from nothing.
        let enqueue_link =
            syscall::raw_tracepoint_open("qdisc_enqueue", enqueue_program.as_raw_fd()).map_err(
                |error| format!("cannot attach to the `qdisc_enqueue` tracepoint: {error}"),
            )?;
        let dequeue_link =
            syscall::raw_tracepoint_open("qdisc_dequeue", dequeue_program.as_raw_fd()).map_err(
                |error| format!("cannot attach to the `qdisc_dequeue` tracepoint: {error}"),
            )?;

        Ok(Self {
            rows,
            queued,
            counters,
            enqueue_program,
            _enqueue_link: enqueue_link,
            dequeue_program,
            _dequeue_link: dequeue_link,
        })
    }

    /// Takes every row the kernel has accumulated since the last drain.
    pub fn drain(&self) -> Result<QdiscDrain, String> {
        let drained = self
            .rows
            .drain()
            .map_err(|error| format!("cannot read the qdisc histogram: {error}"))?;
        let mut result = QdiscDrain {
            rows: Vec::with_capacity(drained.entries.len()),
            vanished: drained.vanished,
            truncated: drained.truncated,
        };
        for (key, value) in &drained.entries {
            match QdiscWait::decode(key, value) {
                Some(row) => result.rows.push(row),
                None => result.vanished += 1,
            }
        }
        Ok(result)
    }

    /// Packets inside a queueing discipline that have not come out.
    ///
    /// Their wait is real and is in no bucket, because it has no end yet.
    pub fn still_queued(&self) -> usize {
        self.queued.len().unwrap_or(0)
    }

    /// Runs of these programs the kernel skipped because one was already
    /// running on that cpu; see `BlockProgram::skipped`, where it was measured.
    pub fn skipped(&self) -> u64 {
        crate::bpf::attach::fentry::skipped([&self.enqueue_program, &self.dequeue_program])
    }

    /// What the programs could not record, as `(packets the map had no room
    /// for, rows the histogram had no room for)`.
    pub fn dropped(&self) -> (u64, u64) {
        (
            self.counters.read(DROP_QUEUED),
            self.counters.read(DROP_ROWS),
        )
    }

    /// Dequeues whose enqueue this run never saw: packets already in a queue
    /// when the programs attached. Not a loss.
    pub fn unpaired(&self) -> u64 {
        self.counters.read(UNPAIRED_DEQUEUES)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The running kernel's BTF, read once for the whole process.
    ///
    /// `Btf::from_kernel` parses six megabytes, and there are a dozen tests in
    /// this family that want it. Reading it once is what `crate::bpf::
    /// kernel_btf` is for, and a test suite that parsed it per test loaded the
    /// machine enough to unsettle the cpu-accounting tests running beside it.
    fn kernel_btf() -> Option<&'static Btf> {
        match super::super::kernel_btf() {
            Ok(btf) => Some(btf),
            Err(reason) => {
                eprintln!("skipped: {reason}");
                None
            }
        }
    }

    /// The program writes a row at stack offsets and `decode` reads it at byte
    /// offsets in the drained bytes.
    #[test]
    fn the_row_the_program_writes_is_the_row_the_reader_decodes() {
        let mut key = vec![0u8; ROW_KEY_BYTES];
        key[KEY_IFINDEX as usize..][..4].copy_from_slice(&7u32.to_ne_bytes());
        key[KEY_HANDLE as usize..][..4].copy_from_slice(&0x8001_0000u32.to_ne_bytes());
        key[KEY_BUCKET as usize..][..4].copy_from_slice(&26u32.to_ne_bytes());
        let mut value = vec![0u8; ROW_VALUE_BYTES];
        value[VALUE_COUNT as usize..][..8].copy_from_slice(&5u64.to_ne_bytes());
        value[VALUE_TOTAL_NS as usize..][..8].copy_from_slice(&250_000_000u64.to_ne_bytes());
        value[VALUE_IFNAME as usize..][..7].copy_from_slice(b"dummy0\0");

        let row = QdiscWait::decode(&key, &value).expect("a full row");
        assert_eq!(row.ifindex, 7);
        assert_eq!(row.handle, 0x8001_0000);
        assert_eq!(row.count, 5);
        assert_eq!(row.total_ns, 250_000_000);
        assert_eq!(row.ifname, "dummy0");
        assert_eq!(row.bucket_low_ns(), 1 << 25, "bucket 26 starts at 2^25");
    }

    #[test]
    fn a_row_shorter_than_the_layout_is_refused_rather_than_padded() {
        assert!(QdiscWait::decode(&[0u8; 4], &[0u8; 32]).is_none());
        assert!(QdiscWait::decode(&[0u8; 12], &[0u8; 8]).is_none());
    }

    /// Both programs build against this kernel, and the dequeue half is the
    /// longer one: closing a wait, naming an interface and bucketing a duration
    /// is most of the work.
    #[test]
    fn both_programs_are_built_against_the_running_kernel() {
        let Some(btf) = kernel_btf() else { return };
        let enqueue = enqueue_instructions(btf, 3, 4).expect("an enqueue program");
        let dequeue = dequeue_instructions(btf, 3, 4, 5, None).expect("a dequeue program");
        assert_eq!(enqueue.last().copied(), Some(Insn::exit()));
        assert_eq!(dequeue.last().copied(), Some(Insn::exit()));
        assert!(dequeue.len() > enqueue.len());
    }

    /// An interface target is compared inside the program, so a packet on
    /// another link never reaches the histogram. A filter applied after the
    /// drain would have counted it first, which is the degradation this
    /// project calls the worst shape a bug takes.
    #[test]
    fn an_interface_target_is_compared_in_the_kernel() {
        let Some(btf) = kernel_btf() else { return };
        let all = dequeue_instructions(btf, 3, 4, 5, None).expect("a program");
        let one = dequeue_instructions(btf, 3, 4, 5, Some(4242)).expect("a program");
        assert!(one.iter().any(|insn| insn.imm == 4242));
        assert!(!all.iter().any(|insn| insn.imm == 4242));
        assert_eq!(one.len(), all.len() + 2, "one load and one comparison");
    }

    /// Invariant 6: one clock domain, for both ends of the wait.
    #[test]
    fn the_programs_read_the_monotonic_clock_and_no_other() {
        let Some(btf) = kernel_btf() else { return };
        const KTIME_GET_BOOT_NS: i32 = 125;
        for program in [
            enqueue_instructions(btf, 3, 4).expect("an enqueue program"),
            dequeue_instructions(btf, 3, 4, 5, None).expect("a dequeue program"),
        ] {
            let called: Vec<i32> = program
                .iter()
                .filter(|insn| insn.code == 0x85)
                .map(|insn| insn.imm)
                .collect();
            assert!(called.contains(&helper::KTIME_GET_NS));
            assert!(!called.contains(&KTIME_GET_BOOT_NS));
        }
    }
}
