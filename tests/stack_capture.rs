//! Ground truth for stack capture.
//!
//! A stack is the one measurement in this tool whose correctness cannot be
//! argued from the code: an unwinder that walks the wrong pointer produces a
//! chain of plausible names in a plausible order, and nothing about it looks
//! wrong. So the truth here is a program whose call chain is known because this
//! file wrote it, compiled twice on purpose -- once with frame pointers and
//! once without -- so that both what the capture does and what it cannot do are
//! checked against a program that states its own shape.
//!
//! The second half of the truth is the kernel's. A page fault is taken in
//! kernel mode, so its stack must cross the privilege boundary, and a kernel
//! that dispatches a fault through something other than its fault handler would
//! be a kernel nobody has. That gives a name to check against which this
//! process did not choose.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

use omn::perf::{Callchain, CpuMode, Record, Sample, SampledEvent, Scope};
use omn::request::StackMode;
use omn::stack::{Context, RawStack, StackTable, Symbolizer, Unwinder};

use perf_event_open_sys as sys;

const CPU_CLOCK: u64 = sys::bindings::PERF_COUNT_SW_CPU_CLOCK as u64;
const CONTEXT_SWITCHES: u64 = sys::bindings::PERF_COUNT_SW_CONTEXT_SWITCHES as u64;

/// The program whose call chain this file knows: `a` calls `b` calls `c`, and
/// `c` is where all the time goes.
///
/// `#[no_mangle]` so the symbols are the names written here rather than a
/// mangling scheme, and `#[inline(never)]` so the three frames are three
/// frames. Nothing else about it matters.
const FIXTURE: &str = r#"
use std::io::Write;
use std::time::{Duration, Instant};

#[no_mangle]
#[inline(never)]
pub fn omn_fixture_c(deadline: Instant) -> u64 {
    let mut total = 0u64;
    loop {
        // A `while` over operators, not a `for` over a range: unoptimised, a
        // range iterator is three function calls per turn, and the samples then
        // land in `Iterator::next` rather than in the function this file is
        // claiming spends the time.
        let mut index = 0u64;
        while index < 2_000_000 {
            total = (total << 1) ^ index;
            index += 1;
        }
        std::hint::black_box(total);
        if Instant::now() >= deadline {
            return total;
        }
    }
}

#[no_mangle]
#[inline(never)]
pub fn omn_fixture_b(deadline: Instant) -> u64 {
    std::hint::black_box(omn_fixture_c(deadline))
}

#[no_mangle]
#[inline(never)]
pub fn omn_fixture_a(deadline: Instant) -> u64 {
    std::hint::black_box(omn_fixture_b(deadline))
}

fn main() {
    let seconds: u64 = std::env::args()
        .nth(1)
        .and_then(|argument| argument.parse().ok())
        .unwrap_or(5);
    let deadline = Instant::now() + Duration::from_secs(seconds);
    // Announced before the chain is entered, so the sampler attaches to a
    // process that is already spinning rather than to one still starting.
    println!("ready");
    std::io::stdout().flush().expect("stdout");
    std::hint::black_box(omn_fixture_a(deadline));
}
"#;

/// Builds the fixture, with or without frame pointers, once per test binary.
///
/// `rustc` rather than a C compiler because a machine that ran `cargo test` has
/// one by definition, and the flag is the point: frame pointers are a property
/// of a build, and a test that could not control them would be measuring this
/// repository's profile settings instead of the unwinder.
fn fixture(frame_pointers: bool) -> Option<&'static Path> {
    static WITH: OnceLock<Option<PathBuf>> = OnceLock::new();
    static WITHOUT: OnceLock<Option<PathBuf>> = OnceLock::new();
    let slot = if frame_pointers { &WITH } else { &WITHOUT };
    slot.get_or_init(|| build_fixture(frame_pointers))
        .as_deref()
}

fn build_fixture(frame_pointers: bool) -> Option<PathBuf> {
    let suffix = if frame_pointers { "fp" } else { "nofp" };
    let directory = std::env::temp_dir().join(format!("omn-stack-fixture-{}", std::process::id()));
    std::fs::create_dir_all(&directory).ok()?;
    let source = directory.join(format!("chain-{suffix}.rs"));
    let binary = directory.join(format!("chain-{suffix}"));
    std::fs::write(&source, FIXTURE).ok()?;
    let status = Command::new("rustc")
        .args(["--edition", "2021", "-C", "opt-level=0", "-g"])
        .arg(if frame_pointers {
            "-Cforce-frame-pointers=yes"
        } else {
            "-Cforce-frame-pointers=no"
        })
        .arg("-o")
        .arg(&binary)
        .arg(&source)
        .status()
        .ok()?;
    status.success().then_some(binary)
}

/// Starts the fixture and waits until it says it is in the chain.
fn spawn_fixture(binary: &Path) -> Child {
    let mut child = Command::new(binary)
        .arg("5")
        .stdout(Stdio::piped())
        .spawn()
        .expect("the fixture was just built, so it runs");
    let stdout = child.stdout.take().expect("piped");
    let mut line = String::new();
    BufReader::new(stdout)
        .read_line(&mut line)
        .expect("the fixture announces itself");
    assert_eq!(line.trim(), "ready");
    child
}

/// Samples a running process for a while and hands back what arrived.
///
/// `None` when the machine refuses to open the event at all, which is the one
/// answer that is a reason to stop quietly: a machine that will not sample is
/// not a machine that sampled wrongly.
fn sample_process(pid: i32, for_ms: u64) -> Option<Vec<Sample>> {
    let mut event = SampledEvent::open_software_with(
        CPU_CLOCK,
        1_000_000,
        Scope::Process(pid),
        64,
        Callchain::FramePointers,
    )
    .ok()?;
    event.enable().expect("enable");
    std::thread::sleep(Duration::from_millis(for_ms));
    event.disable().expect("disable");

    let mut samples = Vec::new();
    event.drain(|record| {
        if let Record::Sample(sample) = record {
            samples.push(sample);
        }
    });
    Some(samples)
}

/// Where the three fixture frames sit in a stack rendered root first.
fn chain_positions(rendered: &str) -> Option<(usize, usize, usize)> {
    let frames: Vec<&str> = rendered.split(';').collect();
    let find = |name: &str| frames.iter().position(|frame| frame.starts_with(name));
    Some((
        find("omn_fixture_a")?,
        find("omn_fixture_b")?,
        find("omn_fixture_c")?,
    ))
}

#[test]
fn a_known_call_chain_is_captured_in_the_order_the_program_calls_it() {
    let Some(binary) = fixture(true) else {
        panic!("the fixture did not build, so nothing was checked about unwinding");
    };
    let mut child = spawn_fixture(binary);
    let pid = child.id() as i32;

    let samples = sample_process(pid, 400);
    let mut unwinder = Unwinder::for_process(pid);
    let ids: Vec<_> = samples
        .iter()
        .flatten()
        .filter_map(|sample| unwinder.record(sample))
        .collect();
    // Symbolization is done; the process has served its purpose.
    let _ = child.kill();
    let _ = child.wait();

    let Some(samples) = samples else {
        eprintln!(
            "skipped: this machine refuses to sample another process, so the chain \
             a -> b -> c was never checked"
        );
        return;
    };
    assert!(
        !samples.is_empty(),
        "a process spinning for 400ms produced no cpu-clock sample at all"
    );

    let notes = unwinder.notes();
    let table = unwinder.into_table();
    let rendered: Vec<String> = ids
        .iter()
        .filter_map(|id| table.get(*id))
        .map(|stack| stack.to_string())
        .collect();

    let with_chain: Vec<&String> = rendered
        .iter()
        .filter(|stack| stack.contains("omn_fixture_c"))
        .collect();
    assert!(
        !with_chain.is_empty(),
        "no stack named the function the program spends all its time in; \
         of {} samples the stacks were: {:?}",
        rendered.len(),
        rendered.iter().take(4).collect::<Vec<_>>()
    );

    // The program calls a, then b, then c, and nothing else calls any of them.
    // Rendered root first, that is the order they must appear in, adjacent,
    // every single time -- a chain that is merely present in some order is what
    // an unwinder that walks a stale frame pointer also produces.
    for stack in &with_chain {
        let (a, b, c) = chain_positions(stack)
            .unwrap_or_else(|| panic!("a stack holds c without b or a above it: {stack}"));
        assert_eq!(
            (b, c),
            (a + 1, a + 2),
            "the chain is a -> b -> c and this stack has them at {a}, {b}, {c}: {stack}"
        );
    }

    // And the leaf is c: the sample landed in the function doing the work,
    // which is what a profile is for.
    let leaf_is_c = with_chain
        .iter()
        .filter(|stack| {
            stack
                .rsplit(';')
                .next()
                .is_some_and(|leaf| leaf.starts_with("omn_fixture_c"))
        })
        .count();
    assert!(
        leaf_is_c * 2 > with_chain.len(),
        "only {leaf_is_c} of {} stacks were sampled inside c, which is where the program spends \
         its time",
        with_chain.len()
    );

    // Nothing this run captured is a degradation: the fixture has frame
    // pointers, so the unwinder has nothing to complain about.
    assert!(
        !notes.iter().any(|note| note.contains("single")),
        "a build with frame pointers was reported as one without: {notes:?}"
    );
}

#[test]
fn a_program_without_frame_pointers_is_reported_as_unwound_rather_than_profiled() {
    let Some(binary) = fixture(false) else {
        panic!("the fixture did not build, so nothing was checked about the degradation");
    };
    let mut child = spawn_fixture(binary);
    let pid = child.id() as i32;

    let samples = sample_process(pid, 400);
    let mut unwinder = Unwinder::for_process(pid);
    let ids: Vec<_> = samples
        .iter()
        .flatten()
        .filter_map(|sample| unwinder.record(sample))
        .collect();
    let _ = child.kill();
    let _ = child.wait();

    let Some(samples) = samples else {
        eprintln!(
            "skipped: this machine refuses to sample another process, so the missing chain \
             was never checked"
        );
        return;
    };
    assert!(!samples.is_empty(), "the fixture produced no sample at all");

    let notes = unwinder.notes();
    let table = unwinder.into_table();
    let rendered: Vec<String> = ids
        .iter()
        .filter_map(|id| table.get(*id))
        .map(|stack| stack.to_string())
        .collect();

    // The same source, built the same way but for one flag. The leaf is still
    // found -- an instruction pointer needs no unwinding -- and the callers are
    // gone, which is exactly the failure this mode has.
    assert!(
        rendered.iter().any(|stack| stack.contains("omn_fixture")),
        "not even the leaf frame was symbolized: {:?}",
        rendered.iter().take(4).collect::<Vec<_>>()
    );
    let recovered = rendered
        .iter()
        .filter(|stack| chain_positions(stack).is_some())
        .count();
    assert_eq!(
        recovered,
        0,
        "the chain a -> b -> c was recovered from a program built without frame pointers, \
         so the unwinder is following something that is not a frame pointer: {:?}",
        rendered
            .iter()
            .find(|stack| chain_positions(stack).is_some())
    );

    // And it says so, rather than presenting leaf frames as a profile.
    assert!(
        notes.iter().any(|note| note.contains("no frame pointer")),
        "a run that recovered no caller reported nothing: {notes:?}"
    );
}

#[test]
fn a_blocked_task_shows_the_syscall_it_blocked_in_on_the_other_side_of_the_boundary() {
    // A context switch is taken in kernel mode, which is what makes it the
    // event that proves the two halves are kept apart. A page fault is not: the
    // kernel records it against the user registers that took it, so its
    // callchain has no kernel side at all -- measured here before this test was
    // written the other way round.
    let Ok(mut event) = SampledEvent::open_software_with(
        CONTEXT_SWITCHES,
        1,
        Scope::Process(0),
        64,
        Callchain::FramePointers,
    ) else {
        eprintln!(
            "skipped: this machine refuses unprivileged perf sampling, so the kernel half \
             of a blocked task's stack was never checked"
        );
        return;
    };
    event.enable().expect("enable");
    // Five sleeps: this thread leaves the cpu five times, through a syscall
    // this test named and the kernel will name back.
    for _ in 0..5 {
        std::thread::sleep(Duration::from_millis(2));
    }
    event.disable().expect("disable");

    let mut samples = Vec::new();
    event.drain(|record| {
        if let Record::Sample(sample) = record {
            samples.push(sample);
        }
    });
    assert!(
        !samples.is_empty(),
        "a thread that slept five times produced no context-switch sample"
    );

    // Taken in the kernel, and the record says so. Anything else means `misc`
    // is being read wrongly, and every kernel address would then go through
    // this process's own symbol tables and come back named after whatever
    // function sits at that offset.
    let blocked = samples
        .iter()
        .find(|sample| sample.cpu_mode == CpuMode::Kernel)
        .expect("a context switch is taken in kernel mode");

    let stack = RawStack::of(blocked).expect("the event asked for a callchain");
    assert!(
        !stack.kernel.is_empty() && !stack.user.is_empty(),
        "a task that entered the kernel to sleep has both halves; this one had \
         {} kernel and {} user frames",
        stack.kernel.len(),
        stack.user.len()
    );

    // The halves are the two halves of the address space. Independently known,
    // and the one check here that does not take the markers this code just
    // parsed at their word: on Linux the kernel lives in the top half and a
    // user mapping never does.
    for &addr in &stack.kernel {
        assert_eq!(addr >> 63, 1, "a kernel frame at a user address: {addr:#x}");
    }
    for &addr in &stack.user {
        assert_eq!(addr >> 63, 0, "a user frame at a kernel address: {addr:#x}");
    }

    let mut symbols = Symbolizer::for_process(0);
    if !symbols.kernel_symbols_available() {
        eprintln!(
            "skipped the naming half: `kernel.kptr_restrict` hides kernel addresses here, \
             so there is nothing to name them with"
        );
        return;
    }
    let named = symbols.symbolize(&stack);
    let kernel: Vec<String> = named
        .frames_in(Context::Kernel)
        .map(|frame| frame.symbol.clone().unwrap_or_default())
        .collect();
    assert!(
        !kernel.is_empty() && kernel.iter().all(|name| !name.is_empty()),
        "a kernel frame went unnamed although kallsyms is readable: {kernel:?}"
    );

    // What this thread did is `clock_nanosleep`, and what the kernel does with
    // one is put the task on a timer and call the scheduler. Both names are the
    // kernel's, not this test's, and neither could come from an unwind that
    // followed the wrong pointer.
    assert!(
        kernel.iter().any(|name| name.contains("nanosleep")),
        "the kernel side of a sleeping task names no sleep: {kernel:?}"
    );
    assert!(
        kernel.iter().any(|name| name.contains("schedule")),
        "a task that left the cpu did so without the scheduler: {kernel:?}"
    );
    // And the kernel side sits under the user side, leaf first, so a renderer
    // that prints a stack root first prints the program before the kernel it
    // called into.
    let frames: Vec<&str> = named
        .frames
        .iter()
        .map(|frame| frame.context.as_str())
        .collect();
    assert!(
        frames.windows(2).all(|pair| pair != ["user", "kernel"]),
        "the two halves are interleaved: {frames:?}"
    );
}

/// The symbol this test looks itself up by. Nothing calls it; its address is
/// the measurement.
#[no_mangle]
#[inline(never)]
pub fn omn_stack_test_landmark() -> u64 {
    std::hint::black_box(0x5EA1_u64)
}

#[test]
fn an_address_in_this_process_is_named_by_the_symbol_that_owns_it() {
    // Independently known because the linker chose it and this process can ask
    // for it: whatever address `omn_stack_test_landmark` has, symbolizing it
    // must give that name back, and symbolizing a byte inside it must give the
    // same name with the offset.
    let address = omn_stack_test_landmark as *const () as u64;
    let mut symbols = Symbolizer::for_process(0);
    let stack = symbols.symbolize(&RawStack {
        kernel: Vec::new(),
        user: vec![address, address + 1],
        elsewhere: Vec::new(),
    });

    assert_eq!(stack.depth(), 2);
    assert_eq!(
        stack.frames[0].symbol.as_deref(),
        Some("omn_stack_test_landmark"),
        "the frame came back as {}",
        stack.frames[0]
    );
    assert_eq!(stack.frames[0].offset, 0);
    assert_eq!(
        stack.frames[1].symbol.as_deref(),
        Some("omn_stack_test_landmark")
    );
    assert_eq!(stack.frames[1].offset, 1);
    assert!(
        stack.frames[0]
            .module
            .as_deref()
            .is_some_and(|module| module.contains("stack_capture")),
        "the frame was attributed to {:?}, not to this test binary",
        stack.frames[0].module
    );
}

#[test]
fn a_stack_seen_twice_is_stored_once() {
    // The point of the table: a profile of ten thousand samples of one loop
    // holds one stack. Interning on the addresses is what makes the second
    // sighting free, so the same callchain must not produce a second entry.
    let address = omn_stack_test_landmark as *const () as u64;
    let mut symbols = Symbolizer::for_process(0);
    let mut table = StackTable::new();

    let first = table.intern(&[address, address + 1], CpuMode::User, &mut symbols);
    let again = table.intern(&[address, address + 1], CpuMode::User, &mut symbols);
    let other = table.intern(&[address], CpuMode::User, &mut symbols);

    assert_eq!(first, again);
    assert_ne!(first, other);
    assert_eq!(table.len(), 2);
    assert_eq!(table.get(first).map(|stack| stack.depth()), Some(2));

    // And it reads back out of a result file as the same table.
    let json = serde_json::to_string(&table).expect("serialize");
    let read: StackTable = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(read.len(), 2);
    assert_eq!(read.get(first), table.get(first));
}

#[test]
fn an_unwind_mode_that_is_not_implemented_says_so_rather_than_unwinding_one_frame() {
    assert_eq!(
        omn::stack::callchain_for(StackMode::Fp),
        Ok(Callchain::FramePointers)
    );
    assert_eq!(
        omn::stack::callchain_for(StackMode::None),
        Ok(Callchain::Off)
    );
    for mode in [StackMode::Dwarf, StackMode::Lbr] {
        let error = omn::stack::callchain_for(mode)
            .expect_err("neither is implemented, and silently doing frame pointers is the bug");
        assert!(
            error.contains("not implemented"),
            "`{mode}` failed with something other than an admission: {error}"
        );
    }
}
