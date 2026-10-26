# The run model — stream R of the eighth wave

Three things the seventh wave's crossings could not do in one run, all of them
in the runner rather than in any collector: a workload and the machine it ran on
in one session, the launched program's lifetime, and several shim-backed sources
on one launch. All three work, each with a scenario in `tests/runner.rs` checked
against something the workload knows independently. What is *not* done is at the
end, with the reason, and so are the lines this stream needs from files it does
not own.

Files touched: `src/collect/mod.rs`, `src/collect/launch.rs`,
`src/collect/mem/heap/mod.rs`, `tests/runner.rs`. `src/collect/task.rs` needed
nothing.

---

## 1. A workload beside the machine, in one run

**What a run may hold now:** any number of requests on *machine* targets
(`system`, `cpu`, `iface`) beside requests on at most **one workload** target
(`launch`, `pid`, `tid`, `cgroup`, `container`). This is the minimum the
crossings asked for, and exactly that. Two different workloads are still refused
by `CollectError::MixedTargets`, exit 2, with a message that now says what a run
may hold:

```
one run observes one workload -- a program, a process, a cgroup -- beside any
number of machine targets (`system`, `cpu`, `iface`), but this session asks for
`launch:[/bin/true]` and `pid:1`; use one session per workload
```

Two workloads stay refused on purpose. They would have to share a launch, a stop
condition and an overhead denominator, and none of those has a single answer —
whose exit ends the run is the first of them.

What "only the workload drives the run" means, concretely, per request:

| | The workload's requests | A machine target's requests |
|---|---|---|
| bound to | the launched pid, or the target it named | its own target: a core is the core |
| given to the launched program's environment | yes | no — a machine collector has no business in the program's `LD_PRELOAD` |
| `requires_launch` refusal | only if the workload is not a launch | always: a shim cannot be given to a core |
| the stop condition | its exit, or `--duration` | never. With no workload in the session every target is the run's own, as before |
| its target going away | ends the run | stops that collector only, and its collection says when and why, as a loss reason |
| what its overhead is a percentage of | the workload's cpu account | its own target's: the core's busy time, the machine's |
| a budget replacement is attached to | the workload | its own target |

The last two were a single field before (`Spending::meter`, `Spending::bound`),
which would have weighed a core's counter against the program's cpu time. They
are one per request now.

### Measured — `a_program_and_the_core_it_ran_on_are_counted_apart_in_one_run`

The confounder check the machine crossing had to run as two `omn` processes. The
program is a shell loop of 300 000 turns pinned with `taskset` to the last core
this process may use; a stranger spinning `while :; do :; done` is pinned to the
same core for the whole run. One session asks for `cpu.instructions` on the
launch and on `cpu: <core>`.

Ground truth is the program's count taken **alone** first, in a session with no
stranger and no core request. Three runs on the development machine (Ryzen 9
3900X, core 23):

```
the program retired 7397111979 beside the stranger and 7393767734 alone (1.0005); the core retired 15625129615, 2.11x the program
the program retired 7395968050 beside the stranger and 7393344191 alone (1.0004); the core retired 15438272869, 2.09x the program
the program retired 7396349474 beside the stranger and 7393974626 alone (1.0003); the core retired 15514571017, 2.10x the program
```

What is asserted is the **separation**, not the stranger's share: the program's
count within 5 % of its solo count (measured: 0.05 %), and the core's count more
than 1.25× the program's (measured 2.1×; CFS gives two pinned spinners half each,
so the bound is a factor of two below what the scheduler does). Both results say
`the launched program exited`: the core has no end of its own, and the program
ended the run for it.

That scenario needs `CAP_PERFMON` on `target/debug/omn` for the per-cpu counter,
which `make` grants. Without it the test skips and says so, with the refusal.

---

## 2. The launched program's lifetime — finding 4 of the compute crossing

`Collection::lifetime: Option<Lifetime>`, exactly the shape P2 asked for plus one
flag:

```rust
pub struct Lifetime {
    pub released_ns: u64,     // CLOCK_MONOTONIC just before the write that opens the gate
    pub exited_ns: u64,       // CLOCK_MONOTONIC when the wait found the program gone
    pub exited_err_ns: u64,   // the exit is in [exited_ns - exited_err_ns, exited_ns]
    pub terminated: bool,     // the run killed it: exited_ns is the tool's deadline
}
```

It is on **every** collection of a run that launched a program, the machine
targets' included, because they share its window and it is the whole they would
be read against. `None` when nothing was launched — not zero.

**The run now ends at the exit rather than at the next tick.** The launched
program has a pidfd (`pidfd_open`, kernel 5.3, below the floor), and the runner
sleeps in `ppoll` on it instead of `thread::sleep`, so a program that exits
between two ticks wakes the run. Measured with a 1 s tick and a program that
lives 0.2 s: the exit is seen **1.9–2.4 ms** after the program's own last clock
read — which includes the interpreter's teardown after that read — where, with
the pidfd disabled, the same test measured **792 ms**. The test fails at 500 ms,
which is the separation between the two, not a calibration of either.

**What `exited_err_ns` is, and why it is wide.** It is the time since the
program was last *known* alive — the last `waitpid(WNOHANG)` that found it
running — and it is the honest bracket: a blocking wait says when the exit was
noticed and nothing about when the program was last alive. So in the run above
it is **~210 ms**, the whole time the runner was asleep in `ppoll`, even though
`exited_ns` itself was 2 ms late. The two numbers answer different questions:
`exited_ns` is a good point estimate whenever the pidfd is there, and
`exited_err_ns` is the interval nothing can contradict. P2 anticipated this —
"`exited_err_ns` is the runner's tick unless the exit is taken from a pidfd" —
and the wakeup latency is exactly what cannot be bounded from userspace: it is
the scheduler's latency for this process, and the kernel does not stamp it
anywhere a pidfd reader can see. Reporting the wakeup latency as a bound would
be asserting something the tool did not measure. **An exact exit** is
`sched_process_exit`, which is an eBPF program and therefore a collector, not a
runner change; it is written down rather than done.

`released_ns` is exact in the direction that matters: it is read before the
write that releases the child, so the program's first instruction is after it.
Measured: 7.8–8.0 ms before the program's first statement, which is `execve`
plus a Python interpreter starting.

### Measured — `the_launched_program_s_lifetime_brackets_its_own_clock`

The program is Python reading `CLOCK_MONOTONIC` as its first statement and as its
last. The inequalities that hold whatever the machine does are asserted:
`released_ns <= first read`, `last read <= exited_ns`, and
`exited_err_ns <= exited_ns - released_ns`. A `cpu.util` request on `system`
beside it carries the same `Lifetime` and says `the launched program exited`.

`a_program_the_run_outlived_is_marked_terminated`: `sleep 30` under a 300 ms
duration is `terminated`, lived at least 300 ms, and stopped because the
duration elapsed. `a_run_that_launched_nothing_has_no_lifetime`: `None`.

**It does not reach the output yet**, and that is the one piece of this item
that is not done: `emit::Results` is built from a `Collection` field by field in
`src/emit.rs`, which no stream owns, and `lifetime` is not one of those fields.
Everything in `src/collect/` carries it; the request for the two lines that
publish it is in `docs/open-findings.md`. The decomposition J is building takes
its `whole` from exactly here.

---

## 3. One shim stream per launch — D3 of the memory crossing

**The call: one pipe shared, not a better refusal.** The refusal was honest, but
it refused a request with an obvious meaning — allocations and retained memory
of one program — for a reason that was entirely the build's: every heap
collector opened its own pipe, and a program has one `OMN_HEAP_FD`.

How it works:

- `Prepared::share_launch(&mut self, earlier: &mut dyn Prepared) -> bool`, a new
  default-`false` hook. The runner offers each launch collector every earlier
  one, in order, and stops at the first that takes it. What "can share" means is
  the collector's own answer, which is why `earlier` arrives as itself rather
  than as a descriptor: a collector that shares answers
  `Prepared::shareable() -> Option<&mut dyn Any>`, and the other downcasts it.
  (Not `Prepared: Any` with an upcast, which needs Rust 1.86 against this
  crate's declared 1.82.)
- A `HeapPrepared` takes an earlier `HeapPrepared` with the same shim path, and
  from then on both hold one `Rc<RefCell<Pipe>>`. Its own pipe closes as the last
  handle to it goes. Both now put the same descriptor in the environment, so the
  merge sees agreement rather than a conflict.
- **Bytes are fanned out, not records.** Whichever collector drains first reads
  the pipe and appends the bytes to every reader's buffer, so each reassembles
  split records, counts `OP_DROPPED`, and decodes as if the pipe were its own. A
  drop the shim reports costs every source it cost, and each says so.

`CollectError::ConflictingEnvironment` is **`Whose::Build`, exit 3**, whatever
is left of it. Every heap collector shares, so nothing in this build produces it
today; the next preload collector (`sched.mutex`, `gpu.api` are the candidates in
`docs/dev/STATE.md`) that does not answer `share_launch` will, and exit 3 is the right
answer for it: each collector asked for something real, and the build cannot put
two values in one variable.

### Measured — `two_heap_sources_on_one_launch_share_the_shim_and_each_sees_all_of_it`

A Python program allocates 512 blocks of 96 KiB through `libc.malloc`, frees
every other one, and exits. `mem.heap.alloc` is run alone, then beside
`mem.heap.live` on the same launch. Asserted: both runs see at least 512
allocations of exactly 98 304 bytes and **the same number**, nothing dropped on
either source, and the live source's peak in `[48 MiB, 72 MiB)` and last value
about half. The unit test `every_reader_of_one_pipe_sees_every_record_once`
splits a record across two drains taken by different readers.

**This turns one test outside this stream red, by design.**
`tests/cross_memory.rs`'s
`d3_two_shim_sources_on_one_launch_are_refused_as_a_request_rather_than_as_the_build`
is a tripwire written to fail when D3 is fixed — its own message says to update
it, move D3 in `docs/scenarios/cross-memory.md`, and fold the crossing's two
sessions into one. Neither file is this stream's. The replacement assertion is in
`docs/open-findings.md`.

---

## The session-versus-daemon line, left open

Nothing here makes a run something other than **one window and one document**.
A machine target beside a workload is read over the workload's window and ends
with it; a run with only machine targets still needs `--duration` or a snapshot.
The pidfd changes when the window closes, not whether it does. `Lifetime` is a
fact about the program inside the window, not a second window.

Two things this work brushed against that belong to that decision rather than to
this stream:

- **A machine target that outlives its workload** — the core read for a second
  after the program exits, to see the machine settle — would be a window that is
  not the workload's. It is the first step of a flight recorder's "keep reading
  around the interesting moment", and it is not done.
- **Two workloads** would mean either two windows in one document, or one window
  with two stop conditions. Either is a new answer to "what is a run", and the
  refusal stays until that is decided.

---

## What is not done, and why

| | Why |
|---|---|
| `lifetime` in the output | `src/emit.rs` has no owner; requested with the exact lines |
| `Target::is_machine` on `Target` | `src/request.rs` is J's; it lives as a private `is_machine` in `collect/mod.rs` until then, and the request is filed |
| an exact exit time | `sched_process_exit` is an eBPF collector, not a runner change |
| a machine target losing its target, end to end | a core going offline or an interface disappearing needs root to create; covered by the unit test `a_machine_target_going_away_does_not_end_the_workload_s_run`, whose fake collector is gone after a known number of reads |
| the D3 tripwire and the stale "one run observes one target" comments | in files other streams own; listed in `docs/open-findings.md` |

## A red test that is not this stream's

`collect::mem::footprint::tests::the_rollup_agrees_with_the_per_mapping_walk`
failed once in a parallel `cargo test --lib` here and passed on re-run:
`Pss: in 10 tries the walk never landed between the rollups around it; last was
77206528 against 76980224..77545472` — a value inside the range it reports
missing, so the message is printing a range other than the one it tested. It is
`mem`'s, it does not touch the runner, and it is left for that family.

And `tests/scenario_io.rs`'s `pressure_is_a_share_of_its_own_window_and_full_never_exceeds_some`
failed once in the full suite (`the window that held it reports 0.002 against
0.016 for an idle one`) and passed three times of three alone. It runs on a
`--system` target, which this stream's changes leave as it was, and it is the
`scenario_io` flapping `docs/dev/BRIEF.md` warns about: an "idle" window on a machine
the rest of the suite is loading.

And `tests/scenario_cpu.rs`'s `a_known_number_of_retired_instructions_is_counted_as_that_number`
failed once in the full suite and once in three runs alone:
`1000000000 instructions was counted at 1006884276 (+0.688 %); the runs were
1013851439 and 6967163`. Because it counts a launched program, which is the path
this stream changed, it was run against the parent commit: **4 of 25 failed on
the parent and 4 of 25 on this branch**, interleaved, with half the cores
spinning. It is not this stream's. The shape is worth the `cpu` family's time:
the excess is almost exactly twice the startup run's 6.97 M, as if the large run
had counted a startup more than once.
