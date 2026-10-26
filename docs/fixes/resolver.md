# Stream W -- what the resolver promises

The twelfth wave's resolver-side half of the portability matrix's findings: a
backend chosen for a mode, a scope or a filter that its collector then refused
or did not apply. P7, P8's `cpu.profile` and `mem.faults` half, and P9 of
`docs/portability.md`; where a predicate actually runs (stream S's request);
the tests that failed rather than skipped without capabilities; and three
requests from the coordinator, stream M and the flaky-tally audit.

Files: `src/resolve.rs`, `src/catalog.rs`, `src/filter.rs`, `src/perf.rs`,
`src/collect/cpu/{mod,oncpu,profile,util,counters}.rs`,
`src/collect/mem/page_faults.rs`, one `filter_in_program` key in
`catalog/proc.yaml`, `tests/{scenario_cpu,scenario_proc,vertical}.rs`, and the
stream-W section of `docs/open-findings.md`. Nothing outside the stream's
paths changed.

---

## 1. P7 -- a backend that does not serve the request's mode

```
cpu.oncpu_time -m trace   -> fs:proc   "`cpu.oncpu_time` through `/proc` re-reads a total, which is `--mode poll`"
```

The resolver walked past an unimplemented backend to a fallback whose
collector refused the mode at attach -- after the target had been launched.

**The rule, landed.** A backend may declare its own `modes`, the way it may
declare its own `targets`: absent means the entry's, present replaces them for
that backend. The loader refuses a backend mode the entry does not declare, an
empty list, and an entry mode no backend serves (`check_backend_modes`). The
resolver passes over a backend whose modes leave the request's out, with
`` `fs:proc` serves `poll` mode, not `trace` `` in the chain, on the walk and
under a pin (`cannot_serve_mode`). The walk then goes on: for
`cpu.oncpu_time -m trace` on this machine it reaches `bpf:raw_tp`, which the
machine serves and the build does not, and the runner refuses that before the
launch as the build's gap (exit 3); without capabilities, the whole chain is
refused with each reason. Either way nothing is launched.

**The data, filed.** Backend lines are stream V's this wave, so the `modes`
keys are a request: `docs/open-findings.md`, "the twelfth wave, stream W",
with the exact lines for `cpu.oncpu_time` (`fs:proc`, `fs:cgroup`) and
`power.cstate` (`fs:sys`). Until they land:

- `cpu.oncpu_time`'s factory refuses a mode other than `poll` itself
  (`oncpu::Prepare::new`), so the refusal comes before the launch, as the
  build's answer, instead of after it.
- `sched.switches -m count` onto `pmu:sw` is stream D's collector; the counted
  spec it needs (`counters::SCHED_SWITCHES`) is written, in W's file, for D
  to hand a counted request to, the way `mem.faults` does.
- `mem.faults -m count` was already fixed by stream S at the eleventh wave's
  merge: the host cell here serves it (below).

Ground truth:

- `resolve::tests::a_backend_whose_modes_leave_the_requests_out_is_passed_over_with_the_reason`
  and `a_pinned_backend_that_does_not_serve_the_mode_is_refused_with_the_reason`.
- `catalog::tests::a_backend_promising_a_field_or_a_mode_its_entry_cannot_have_is_refused`.
- `tests/scenario_cpu.rs::a_backend_that_cannot_serve_the_mode_is_passed_over_before_the_launch`:
  the real binary, the matrix cell's request, and a launched program that
  would create a file. The resolved chain says why `fs:proc` was passed over,
  the run is refused, the file is never created, and the refusal is not the
  collector's. It runs on the embedded catalog once V's line is there, and on
  a copy carrying exactly that line (`OMN_CATALOG_DIR`) until then.

## 2. P8 -- `system: native`, refused by `cpu.profile` and `mem.faults`

```
cpu.profile --system -> "profiling a whole machine needs one sampling event per cpu and the privileges for them"
mem.faults  --system -> "sampling page faults machine-wide needs per-cpu events and the privileges for them"
```

**Fixed by serving it**, not by taking `system` out of the catalog: a machine
or a core target is now one sampling event per cpu, over every task that runs
there. `perf::ThreadSampler::open_cpus(&spec, &cpus, ring_pages)` opens them,
not inherited (a cpu event already sees every task), and every cpu must open:
the first refusal is the answer, in `per_cpu_refusal`'s words. Each sample is
given the cpu of the ring it came from. `cpu.profile` and `mem.faults` take it
for `Target::System` and `Target::Cpu`.

A cpu-clock event per cpu does not have the problem that keeps a process
profile off inherited per-cpu events -- the cpu's clock runs whoever is on it,
so no period is left unfinished by a task moving on. Samples of the idle task
are left out and counted in the notes; a sample's process mappings are read
only when it carries a stack, and re-read only for processes sampled since
the last tick, because over a machine re-reading every process on every tick
cost more than the profile. The run says what it followed: *"every task on 24
cpus was sampled, one event per cpu; samples came from 28 tasks in 13
processes, and 9030 landed in the idle task and are not in the profile"*.

Ground truth, both in `tests/scenario_cpu.rs` (the file `page_faults.rs`'s
scenario would otherwise go to is D's):

- `a_profile_of_the_machine_finds_each_thread_for_the_cpu_it_used`: the
  machine profiled at 997 Hz for 4 s while three threads, started once the run
  is attached, each burn 200 ms of their own cpu and write what their clocks
  read. Each thread's samples are about 997 per second of that, within the
  thirty per cent plus one sample the per-process profile carries -- on a
  machine busy with four other streams, whose samples must not be credited to
  these.
- `a_trace_of_the_machines_faults_holds_every_fault_a_thread_counted`: one
  thread touches 4096 fresh pages between two clock readings and writes what
  its own `getrusage(RUSAGE_THREAD)` fault count moved by. The machine-wide
  trace holds that many faults of that thread inside that interval (plus one
  per cent: the event and the counter are two hooks of the fault path), and
  the ones inside the touched block are the same number.
- `profile::tests::a_profile_of_the_machine_finds_this_thread_for_the_cpu_it_burned`,
  the same in-process, which skips with the sysctl's name where the unit test
  binary may not open a cpu.

Both scenarios skip, with the reason, when the machine is too loaded to give
the workload its cpu inside the window, or cannot open a cpu event.

**Not done**: the `cgroup` and `container` targets the same entries declare
(and the counting sources) are refused by name, *"is not built"* -- P8's shape
one kind over, which the matrix does not run. `perf::Scope::CgroupOnCpu`
exists; filed in `docs/open-findings.md` against the `targets` keys until the
collectors are written.

## 3. P9 -- a per-task probe for a per-cpu event

A `pmu:` probe opens its event over this process, which `perf_event_paranoid`
at 2 allows, and a system or core target is one event per cpu, which it does
not. The probe is V's and answers for the listing, which knows no target; the
resolver knows the target. So `resolve::probe_all` now adds one requirement to
a perf backend asked about a `System` or `Cpu` target:
`per-cpu perf events (the target observes whole cpus)`, answered by
`perf::per_cpu_support()` -- a disabled software clock opened on the first
online cpu and closed, since the kernel's check (`perf_allow_cpu`) is the same
whatever the event -- and refused in the words the collector already used:
*"counting cpu 0 observes every task on it, which needs
/proc/sys/kernel/perf_event_paranoid at 0 or below (it is 2 here) or
CAP_PERFMON on this binary"*. It is in the resolved file with its answer. A
package counter (`pmu:power/...`) is already probed per cpu and is not asked
twice; an eBPF backend opens no perf event and is not asked.

Ground truth: `resolve::tests::a_perf_backend_asked_about_whole_cpus_is_probed_for_whole_cpus`
(the requirement is there for `System` and `Cpu`, absent for a pid, and its
answer is the kernel's), and
`tests/scenario_cpu.rs::a_system_target_asks_whether_this_process_may_count_a_cpu`,
which reads the requirement off the resolved file and runs an uncapped copy of
the binary. **On this host it skips**, loudly: `perf_event_paranoid` is -1, so
any process may count a cpu and the refusal cannot be created here. The
matrix's `nocaps` guest cells (paranoid 2) are where it is checked -- see
"The matrix cells" below.

## 4. Where a predicate actually runs (stream S's request)

`resolve::placement` said `"placement": "kernel"` for every backend whose id
starts with `bpf`. An audit of every eBPF collector found **one** comparison
that runs in a program: `proc.syscall`'s `bpf:raw_tp`, on `syscall`, with `==`
and one value, at top level or inside `&&` (`src/bpf/proc_syscall.rs`, the
`jne` before the pending-map write). Every other collector that receives a
predicate applies it to what its program delivered, and three (`sched.wakeup`,
`sched.offcpu`, `sched.migrations`) still apply none -- that is stream S's
request to the `sched` collectors, D's files this wave, and not repeated.

**The key.** A backend lists `filter_in_program`, the fields its program
compares; the loader checks it is a subset of what the backend carries. What a
field there promises is the shape a program compares against a constant: a
conjunction of `==` with one value, one comparison per field.
`filter::FilterExpr::{conjuncts, constant_equality}` read that shape, and
`resolve::pushed_down(backend, predicate)` is the part a program carries. The
one line: `filter_in_program: [syscall]` on `proc.syscall`'s `bpf:raw_tp`.

**The placement.** `kernel` only when every part of the predicate is pushed
down. Otherwise an eBPF backend's collector applies it (`userspace`, said so,
and naming the part that narrowed the program first, if one did), and any
other backend's records are filtered by the runner, as before.

**Why the Rust shape is what it is.** The runner decides whether to filter on
the variant (`collect::apply_filter`: `Userspace` means it filters), and
`src/render.rs` matches the variants exhaustively; both are R's. Turning a
collector-applied predicate into `Userspace` would make the runner filter it a
second time, which is wrong wherever the collector's events cannot be
re-filtered: a count-mode `io.latency` event carries none of the filter
fields, a histogram row's `bytes` is a sum, and `proc.syscall` compares a
number with `nr`, not with the event's `syscall` name. So `FilterPlacement`
keeps `Kernel` (the runner leaves the events alone) and `Userspace` (it
filters), and carries `Placed { by: AppliedBy, reason }`, `by` being
`Program`, `Collector` or `Runner`. The resolved file is written from `by`:

```json
{"placement": "kernel",    "applied_by": "program",   "reason": "`bpf:raw_tp` compares `syscall == 'getppid'` in the program, ..."}
{"placement": "userspace", "applied_by": "collector", "reason": "`bpf:raw_tp`'s program compares only `syscall`, with `==` and one value, so the collector applies ..."}
{"placement": "userspace", "applied_by": "runner",    "reason": "`fs:proc:statm` returns whole records, ..."}
```

and read back to the same variant. A file from before `applied_by` reads as
it was run. `render.rs` still heads the `Kernel` variant `pushed into the
kernel:`, so `Placed`'s `Display` prefixes a collector's reason with `no;`
until R prints from `by` -- the exact change is filed.

Ground truth, `tests/scenario_proc.rs::a_predicate_the_program_carries_is_placed_in_the_kernel_and_one_it_does_not_is_not`:
a Python loop of 2000 `getppid` calls, launched, under two predicates with the
same answer. `syscall == 'getppid'` resolves to `kernel`/`program`, counts the
2000 calls, and the collector -- which applies the whole predicate again to
what was drained -- removes **0**: the others were dropped in the kernel.
`syscall in (getppid)` resolves to `userspace`/`collector`, counts the same
2000, and the collector's own count says it removed **352** calls of the
start-up here. The program keeps no count of what its comparison skipped
(`DROP_PENDING` and `DROP_ROWS` count full maps), so the collector's count is
the witness; a `SKIPPED_BY_PREDICATE` slot is filed for D. Unit:
`only_a_predicate_the_program_carries_is_placed_in_the_kernel` (every shape
that is not pushed: `in`, `!=`, `||`, `!`, two comparisons of one field, a
field the program does not compare) and
`a_placement_is_written_where_it_ran_and_read_back_as_it_was`.

## 5. The suite without capabilities

Rehearsed as stream P did: both hard links of the binary replaced by copies
(`target/debug/omn` and `deps/omn-<hash>`), which carry no capability.

| Test | Was | Now |
|---|---|---|
| `scenario_proc::the_syscall_filter_the_resolved_file_claims_is_the_one_that_ran` | asserted the `--dump-resolved` succeeded | skips with the resolver's refusal when every eBPF backend is refused for its capability |
| `scenario_proc::the_combinations_these_sources_do_not_have_are_refused_with_the_reason` | the misspelled-syscall refusal comes from the collector, which an unprivileged machine never reaches | that one check skips with the refusal; the three before it still run |
| `vertical::a_program_that_declares_no_request_is_refused_by_name` | same shape: the probe names are looked for once a backend is chosen | skips with the refusal when the machine refuses first |

`tests/vertical.rs`'s skip reason (three sites and the session's) printed the
first line of stderr, `note: mode defaulted to trace, ...`; `refusal(stderr)`
prints the `error:` line and the indented lines under it, where the resolver
names each backend's missing requirement.

With the binary stripped, the six W test files: all pass
(cross_compute 8, family_cpu 17, family_proc 3, scenario_cpu 31,
scenario_proc 25, vertical 15), and the skips name `CAP_BPF` or the resolver's
*"has no viable backend on this machine"*, both classes of
`.github/expected-skips.txt`.

A gap left: a misspelled syscall is refused by the collector, so an
unprivileged machine answers *"this machine cannot"* for a request no machine
can serve. The validator could refuse it before probing, but the syscall table
is in `collect::proc`, which `src/collect/mod.rs` (R's) keeps private.

## 6. From the coordinator and stream M

- **`cpu.util`'s negative busy.** `busy` was `1 - idle/total - iowait/total`
  in floating point, and a core idle apart from some iowait read -2.8e-17; it
  is `(total - idle - iowait) / total` in ticks now, saturating.
  `a_core_that_only_idled_and_waited_is_exactly_not_busy` asserts 0.0 exactly
  on seven ticks of idle and three of iowait, and asserts first that the old
  arithmetic does not cancel on those numbers. This is the likely cause of
  `per_cpu_utilisation_reports_one_series_for_each_core_asked_for` in the
  flaky tally.
- **Stream M**: `resolve::probe_all(&capabilities, backend, None)` is public
  with the signature M's `Listed` asked for (the `Option<&Target>` is the
  request's target, `None` for the listing), and `Cost::worst_case_percent`
  is in `src/catalog.rs`. `src/main.rs` (R) still has its own copy; switching
  it is R's one-line change.

## The matrix cells

Two cells, the ones the classes name, with this branch's binary; the kernels
were the matrix worktree's, linked rather than fetched again. The cells were
moved off `target/matrix/cells/` afterwards, because `tests/portability.rs`
judges whatever is there and its entries are V's to narrow.

- **`scripts/matrix.sh here`** (the host, 7.2.6, `caps`): 101 sources, 93
  available, swept in 131 s, 121 of 140 runs attached. `cpu.profile
  --system` and `mem.faults --system` attach; `mem.faults` on a launched
  program attaches (`count`, stream S's fix). The judge names exactly those
  three as *"recorded as refusing here and it attached; narrow or remove the
  entry"*. `cpu.oncpu_time -m trace` on a launched program now refuses with
  exit 3 at the factory, before the launch, and still in the words the P7
  entry records -- it goes when V's `modes` line does. `sched.switches` (both
  targets) and `power.cstate` refuse as before: D's and V's.
- **`scripts/matrix.sh v6.6.157 --configs nocaps`** (`perf_event_paranoid`
  2): 101 sources, 52 available, 41 of 140 runs attached. Every one of P9's
  ten counting sources on `--system` is now refused by the resolver, before
  anything starts, with *"`cpu.cycles` has no viable backend on this machine:
  pmu: per-cpu perf events (the target observes whole cpus): missing: counting
  cpu 0 observes every task on it, which needs
  /proc/sys/kernel/perf_event_paranoid at 0 or below (it is 2 here) or
  CAP_PERFMON on this binary"* -- 18 system runs in the cell carry that
  requirement's refusal. The first run of the cell found the refusal worded as
  *"this cpu is online, so it is the counter ..."*: `open_fd` wrapped the
  error as `ErrorKind::Other` and `per_cpu_refusal` could not see the
  `EACCES`; it keeps the kind now, which also corrects the same message in
  every per-cpu collector. The judge's only violation there is `mem.faults
  (launch)`, P7's retired entry again.

## Which `KNOWN_DEFECTS` entries this retires

In `tests/portability.rs` (V's file):

- **P7, `mem.faults`, `launch`** -- retired (fixed by stream S's count
  mode; this stream measured it in both cells).
- **P8, `cpu.profile`, `system`** -- retired.
- **P8, `mem.faults`, `system`** -- retired.
- **P9, the ten counting sources, `system`** -- the defect is gone (the
  resolver refuses the request, with the reason, before anything starts), and
  the entry still *matches*, because the judge reads "listed usable" off
  `omn list`, which knows no target, and the refusal text still says
  `perf_event_paranoid`. The entry should become an expected per-request
  refusal rather than a defect: V's call, filed.
- **P7, `cpu.oncpu_time`** and **P7, `power.cstate`** -- retired when V's
  `modes` lines land (the rule is in, the data is filed).
- **P7 and P8, `sched.switches`** -- D's (`counters::SCHED_SWITCHES` and
  `ThreadSampler::open_cpus` are ready for it).

## The commit's check

One `make check`, with the capabilities on the binary and four other streams
building beside it: red at `tests/cli.rs`, so one
`cargo test --workspace --no-fail-fast` followed -- **1844 passed, 5 failed,
8 ignored**, in 5 min 20 s; `cargo clippy --all-targets -- -D warnings` and
`cargo fmt --check` clean.

- `cli::a_collector_that_needs_a_process_refuses_a_system_target_with_the_reason`
  asserts P8 itself: that `mem.faults --system -m sample` is refused with
  *"per-cpu events ... names no process"*. It is served now, and the test is
  nobody's this wave, so it is left as it is and filed for the coordinator
  with its replacement (`docs/open-findings.md`, stream W). **Expected red
  until then.**
- Four in `tests/scenario_mem.rs` (D's), each passing alone at once:
  `both_huge_page_backends_read_the_same_promotion_counter` (in the flaky
  tally already), and three not yet in it --
  `a_gibibyte_of_reads_costs_one_l1_miss_per_cache_line` (288 160 TLB misses
  with THP `always`), `a_cgroup_squeezed_until_it_reclaims_reports_what_the_kernel_counted`
  (no reclaim episode seen for 37 700 pages) and
  `a_walk_that_fits_in_l1_is_never_timed_and_one_that_does_not_waits_on_memory`
  (13 of 40 small-walk loads timed). All four measure the machine's memory
  under a parallel suite at load 10-12; none touches a file of this stream.
  Added to the tally in `docs/open-findings.md`.

The six W test files, alone, with the capabilities and without: all green.

## Index rows

None: this stream adds no session to `examples/`.

## For `docs/DECISIONS.md`

- **A backend may declare the modes it serves**, as it may declare its
  targets: the list replaces the entry's for that backend, the loader checks it
  is a non-empty subset that leaves no entry mode unserved, and the resolver
  passes over a backend whose modes leave the request's out, with the reason in
  the chain. A mode is a property of a mechanism (a file holding a total
  cannot trace), so it is catalog data and not a collector's declaration.
- **`kernel` placement is a promise about cost, and is made only for what a
  program compares**: `filter_in_program` per backend, and a predicate is
  `kernel` only when every part of it is a one-value `==` over those fields,
  one per field. Everything a collector applies after delivery is
  `userspace`, whatever the backend's id says, and the resolved file says who
  applied it (`applied_by`).
- **A system or core target on a perf backend is probed per cpu** by the
  resolver, as a requirement of its own in the resolved file, because the
  listing's per-task probe cannot answer it and the target is the resolver's
  to know.
- **A machine is sampled with one event per cpu, not inherited**, and the
  idle task's samples are left out of a profile and counted.

## Filed for other streams

All in `docs/open-findings.md`, "Needs from another stream — the twelfth
wave, stream W":

- **V** (`catalog/cpu.yaml`, `catalog/power.yaml`): the `modes` keys for P7,
  exact lines given; and the `cgroup`/`container` targets of `cpu.profile`,
  `mem.faults` and the counting sources, which nothing serves.
- **D** (`src/collect/sched/switches.rs`): `sched.switches -m count` through
  `counters::SCHED_SWITCHES` (P7's fourth cell), and `ThreadSampler::open_cpus`
  as the shape of P8's `sched` half.
- **D** (`src/bpf/proc_syscall.rs`): the program's own count of what its
  pushed-down comparison skipped.
- **R** (`src/render.rs`): print a filter's placement from `Placed::by`; then
  the `no;` prefix goes. And `main.rs` calling `Cost::worst_case_percent`.

**Merge note**: `catalog/proc.yaml`'s `bpf:raw_tp` line of `proc.syscall`
gains `filter_in_program: [syscall]`; V's P1 may rewrite the same line's
`raw_tracepoint:` requirements, which is a one-line conflict to take both
sides of.
