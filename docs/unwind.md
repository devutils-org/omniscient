# Stream U, fourteenth wave — every way to unwind a stack

A profile's stacks are only as good as the way they were unwound, and until
this wave there was one way: the kernel's frame-pointer walk, which loses the
caller of every function that sets up no frame, and which the kernel's default
`perf_event_paranoid` refused outright because it asked for the kernel half.

**What landed.** The unwind modes are data (`catalog/unwind.yaml` since the fifteenth wave, `src/unwind.yaml` before), probed, ranked
and forced the way backends are (1). Three of them are served, each held to a
C program whose call tree and shares are known by construction: `fp`, `fp+leaf`
(the caller of a frameless leaf recovered from its CFI and marked), and `dwarf`
(the user stack copied with each sample and unwound here through `framehop`);
`lbr` and `sframe` are declared, probed, and refused by name on every machine
this project runs on, with what upstream has written down (2). At the kernel's
default paranoia a profile is served with user-only stacks, and says so (3).
`--symbols debuginfod` fetches a stripped file's names by build id -- the
mechanism, cache and origins are landed and tested; the flag is R's to add (4).
The profile's drops are reported per ring (5). The frame-pointer note names the
object the frameless functions are in and, under `fp+leaf` and `dwarf`, counts
what was recovered (6).

**Left**, each with its measured reason: the `--symbols` flag and the resolved
file's `stack` field are other streams' files (filed below); `lbr` has no
collector because no cpu here offers a branch call stack to exercise one
against; `sframe` has no kernel to ask; the paranoid-2 path is checked here as
far as a machine at paranoid -1 can check it, and by the CI's next run for the
rest; the resolved file does not carry the unwind choice yet (P's and R's
files), so items 1, 3 and 4 say it in the profile's notes and degradations.
The adversarial review of the branch, and what came of each finding, is under
"Review" below.

---

## 1. Unwind modes are data

`catalog/unwind.yaml` declares each mode like a backend: ordered best first, with
`requires` (each answered available, missing or unknown by `unwind::probe`),
`copies` (the bytes of user stack the kernel copies per sample), `cost`, and
`degraded` -- what it gives up relative to the modes above it. The loader
(`unwind::load`) refuses a table that declares a mode twice, declares `auto`,
leaves a concrete mode out, or has a mode below the first that says neither what
it degrades nor `equivalent: true` -- the rule a fallback backend is held to.

| mode | requires | copies | cost | degraded |
|---|---|---|---|---|
| `sframe` | `kernel:sframe`, `target:sframe`, `perf:callchain` | 0 | low | (first) |
| `fp+leaf` | `perf:callchain`, `perf:user_stack`, `target:frame_pointers`, `target:cfi` | 256 | low | only the leaf's caller is recovered |
| `dwarf` | `perf:user_stack`, `target:cfi` | 8192 | high | 8 KiB per sample; a deeper stack is cut where the copy ends |
| `lbr` | `pmu:branch_call_stack` | 0 | low | 16 to 32 calls |
| `fp` | `perf:callchain` | 0 | low | the caller of a frameless function is lost |
| `none` | -- | 0 | free | no stack (`auto: false`) |

**`stack::choose(&Request, &Environment)`** is the one entry point (landed in
the foundation commit; its body is now `unwind::choose`). `--stack auto` -- the
default -- walks the table and takes the first mode whose every requirement is
available; **unknown never counts as available**, so a mode whose target
requirement cannot be checked (a system-wide profile, a pid whose binary this
user cannot read) is passed over with the reason. Each mode looked at is a
`ConsideredMode { mode, verdict }` with the resolver's own `Verdict`
(`Selected`, `Rejected(reasons)`, `Unimplemented`, `NotReached`). `--stack
<mode>` forces one: refused where a requirement is missing, served where one is
only unknown, with `assumed rather than checked -- <requirement>` in its
degradations, as a pinned backend is.

The target requirements are what make the choice the target's as well as the
machine's. `unwind::target_image` reads the binary a launch names (resolved on
`PATH` the way `execvp` does) or `/proc/<pid>/exe`:

- `target:frame_pointers`: `main`'s prologue when there is a `main` to read --
  the program's own code, compiled with its own flags, where the runtime linked
  into it (Rust's standard library, the C start files) was not. Without one (a
  stripped binary) every function `.eh_frame_hdr` indexes is read, and the
  binary keeps them when **more than a quarter** do. A quarter because that is
  what separates the builds, measured here:

  | binary | functions setting up a frame pointer |
  |---|---|
  | calltree `-O1 -fomit-frame-pointer` (and stripped) | 0 of 13 |
  | calltree `-O1 -fno-omit-frame-pointer` | 10 of 13 |
  | `/usr/bin/gzip`, `ls`, `xz`, `sqlite3` (Arch, frame pointers on) | 33/69, 129/332, 48/84, 1319/2735 |
  | `libc.so.6`, `libpython3.14.so` | 1242/3949, 3900/10763 |

  A distribution built with frame pointers sets one up in 36 to 57 % of its
  indexed functions (the rest are `.cold` fragments, leaves and assembly); one
  built without them in none.
- `target:cfi`: a non-empty `.eh_frame` or `.debug_frame`.
- `target:sframe`: a `.sframe` section.

A script (`#!`) is unknown: the program it runs is not known before it runs.

What `auto` picks on this machine: `fp+leaf` for the calltree built with frame
pointers (both builds), for `/bin/sh`, `gzip` and `xz`; `dwarf` for the
calltree built without and its stripped copy, and for `python3`, whose
executable is a two-function stub (`0 of 2`) in front of `libpython`.
System-wide and cgroup profiles land on `fp`: the binaries are not known before
the run.

**Where the choice is written.** Until the resolved file carries it (filed for
R and P below), the profile says it in its results: a note `stacks unwound by
`fp+leaf`, chosen by `--stack auto` over sframe (kernel:sframe: missing: ...)`
and the mode's degradations beside the backend's. `StackChoice` serializes with
the mode, `forced`, `user_only`, `considered`, `degradations` and
`copied_stack_bytes`, ready for the resolved file.

The validator (`check_stack`) reports the forced mode's declared copies and
degradation from the same table, so the validator and the chooser say one
thing. `docs/open-requests.md` row 815 (refuse `dwarf`/`lbr` in `check_stack`)
is resolved the other way: `dwarf` is served now, and `lbr`/`sframe` are
refused by the chooser before the target is launched, which is where a refusal
that depends on the machine belongs -- the validator reads no machine.

## 2. The modes

### `fp`

The kernel's frame-pointer callchain, as before: `Callchain::FramePointers {
kernel }`.

### `fp+leaf`

`Callchain::FramePointersAndLeaf`: the frame-pointer chain, plus
`PERF_SAMPLE_REGS_USER` (ip, sp, bp on x86_64; pc, sp, fp, lr on aarch64) and
`PERF_SAMPLE_STACK_USER` of 256 bytes. For each sample whose user chain starts
at the sampled ip, `unwind::Cfi::caller` runs one step of `framehop` from those
registers over those 256 bytes, using the leaf's own unwind rule from
`.eh_frame`. When the return address it finds is not the chain's second entry,
it is inserted there and the frame is marked `recovered` (`Frame::recovered`,
and `Frame::label` ends with ` [recovered]`). A leaf with no CFI falls back to
the frame pointer inside `framehop`, which returns what the kernel returned, so
nothing is inserted: the mode never invents.

It recovers more than frameless leaves: a function with a frame pointer
sampled in its prologue or epilogue has not built (or has torn down) its frame,
and the kernel skips its caller the same way. On `/bin/sh` 626 of 1142 samples
had a caller put back, 217 of them in a leaf that sets up no frame at all
(`examples/unwind-leaf-caller.yml`).

### `dwarf`

`Callchain::CopiedStack { kernel }`: the registers and 8192 bytes of user stack
(`perf record --call-graph dwarf`'s default), with the kernel's callchain asked
for its own half only (`exclude_callchain_user`). `unwind::Cfi` holds a
`framehop` unwinder over every executable mapping of the process -- `.text`,
`.eh_frame`, `.eh_frame_hdr`, `.got` and `.debug_frame`, each mapping placed at
its load bias -- refreshed with the symbolizer on every tick so a `dlopen`ed
library is found. A walk ends `Complete`, `CutByCopy` (the next return address
lies past the copied bytes), `Failed` or `DepthLimit` (127), and each is
counted and said.

**Why `framehop`**: pure Rust, no system library to find at install time (libdw
would tie the release to elfutils' ABI), `.eh_frame_hdr`-indexed lookups with a
rule cache, correct in prologues and epilogues, and the unwinder `samply` has
used on Linux for years. Added with `default-features = false, features =
["std"]`, which leaves out the Mach-O and PE readers.

### `lbr`

Probed by opening a cycles event that samples `PERF_SAMPLE_BRANCH_STACK` with
`PERF_SAMPLE_BRANCH_USER | PERF_SAMPLE_BRANCH_CALL_STACK` over this process. On
the development machine: **missing**, `perf_event_open: Operation not
supported (os error 95)`, and the refusal names the cpu (`AMD Ryzen 9 3900X`).
AMD's LBR v2 (Zen 4) records branches but has no call-stack mode, so it is
missing there too. No collector decodes branch stacks: where the probe says
available (Intel from Haswell), the verdict is `Unimplemented`, what the
resolver says of a backend with no collector -- a decoder nothing here has run
would be a confident wrong answer the day a machine offers it.

### `sframe`

Established from the kernel source before anything was written, from the
7.2.6 tree installed on the development machine (`/usr/src/linux`):

- `include/linux/unwind_user_types.h` lists the user unwinder's types in
  priority order, and there is one: `UNWIND_USER_TYPE_FP`. Nothing in the tree
  reads a `.sframe` section, and there is no `CONFIG_*SFRAME*` option.
- The infrastructure an SFrame unwinder would plug into is there:
  `unwind_user.h` and `unwind_deferred.h` (the deferred user unwinder, which
  upstream merged in 6.17), and perf's `defer_callchain` attribute bit,
  `PERF_RECORD_CALLCHAIN_DEFERRED` and `PERF_CONTEXT_USER_DEFERRED` in the uapi.
  The SFrame type itself is a patch series that no release through 7.2 carries.
- The other half exists already: gcc 16.2 / binutils on this machine write a
  `.sframe` section by default (the calltree builds all carry one), so
  `target:sframe` is available here and `kernel:sframe` is the missing half.

So `kernel:sframe` answers **missing** on every kernel through 7.2, with that
reason, and **unknown** above 7.2 (`SFRAME_READ_THROUGH` in `src/unwind.rs`):
a newer kernel is not assumed either way. Nothing is collected: when a kernel
gains it, it will arrive as a normal callchain (possibly deferred), and the
mode's collector is the `fp` path with the deferred records merged.

## 3. The kernel's default paranoia

**Confirmed** from the CI's second run (`run2-paranoid2/test.log`, line 1697 on):
`cpu.profile has no viable backend: callchain: missing: perf_event_open refused
a sampling event carrying a callchain (Permission denied)`, on both backends.
The probe opened a callchain with the kernel half, which paranoid 2 refuses to
a user.

Now: `perf::callchain_supported` answers yes when either shape opens, both
halves or the user half alone, and `perf::kernel_callchain_allowed` (asked
once per process) tells them apart. `unwind::choose` sets
`StackChoice::user_only` when the kernel half is refused, and
`StackChoice::callchain()` then drops it: the event opens with
`exclude_kernel` and `exclude_callchain_kernel`. The choice's degradation says
what that costs: *"perf_event_paranoid is 2, which refuses this user the
kernel: stacks are user-only, and a sample taken while the task ran in the
kernel is not delivered, so time spent in system calls is missing from this
profile"*.

**`--stack none` too.** A profile without stacks is still a sampler, and at
paranoid 2 the kernel refuses an unprivileged sampler that includes it
whatever it asks for. The first version set `user_only` only for a mode with a
stack, so `--stack none` opened a cpu-clock event without `exclude_kernel` and
would have been refused with `EACCES` (found by reading the code; this machine
is at -1). Now `choose` sets `user_only` for every mode, `ProfileCollector::spec`
sets `SampleSpec::exclude_kernel` from it, and the degradation says *"samples
are user-only"* for `none`. The unit test
`a_user_only_choice_excludes_the_kernel_even_without_a_stack` holds the event
for `none`, `fp` and `dwarf`, both ways.

The `callchain` requirement string is still answered by P's `capabilities.rs`,
which calls `callchain_supported`, so the probe opens what the collector will
open. Ground truth here (`tests/unwind.rs`,
`user_only_stacks_carry_no_kernel_frame_and_leave_the_system_calls_out`): a
program that makes 600 000 one-byte `write`s to `/dev/null`, sampled both ways.
User-only: no kernel frame and no kernel-mode sample at all, and -- at any
paranoia, so this half runs at paranoid 2 as well -- the user-only samples are
held to the program's own `getrusage`: they cover its user time (within a
factor of two and 20 ms, the user/system split being the kernel's tick-sampled
estimate) and less than half of its user plus system time. Measured: 237
samples = 23 ms, where the program said 17 ms user and 84 ms system. Both
halves, where the machine allows them: most samples in the kernel, and more
than twice the user-only count for the same work.

`the_choice_says_user_only_exactly_when_the_machine_refuses_the_kernel` is held
to the kernel's own rule (`perf_allow_kernel`), read independently of the
chooser: `/proc/sys/kernel/perf_event_paranoid`, the test's euid, the
`security.capability` attribute of `target/debug/omn` (`CAP_PERFMON` or
`CAP_SYS_ADMIN`, with the effective bit, masked by the bounding set). It runs
both `--stack auto` and `--stack none`, and a refusal of either is a failure,
not a skip. **The CI's next run at paranoid 2 is the rest of the check**: the
probe, the choice and the note on a machine that actually refuses.

## 4. Names, from debuginfod

`stack::unwind::SymbolSource { Local, Debuginfod }` (`local` / `debuginfod`,
serde as those strings) and `stack::unwind::Debuginfod`:

- servers from `DEBUGINFOD_URLS` (space separated); `from_env` refuses an
  empty one by name;
- the cache is elfutils' own (`$DEBUGINFOD_CACHE_PATH`, else
  `$XDG_CACHE_HOME/debuginfod_client`, else `~/.cache/debuginfod_client`, each
  file at `<build id>/debuginfo`), so a file `gdb` or `perf` fetched is not
  fetched again, and one fetched here serves them;
- fetched with `curl --fail`, into a partial file renamed into place; a build
  id that is not hex is refused before any URL or path is formed; a build id
  no server had is remembered for the run and not asked again;
- **never asked unless asked**: `Symbolizer::for_process` has no client, and
  `Unwinder::new(pid, callchain, None)` is what every collector builds today.

`Module::read` asks only for a file that has no `.symtab` of its own, by the
build id in its `NT_GNU_BUILD_ID` note, and places the debug file's symbols
through the *mapped* file's load segments. Each module's `NameOrigin` --
`SymbolTable`, `DynamicSymbols`, `Debuginfod { build_id, server }` (`server:
None` when the cache answered) or `None(reason)` -- is kept, and the profile's
facts say *"names came from -- calltree-strip: debuginfod, http://..., build id
...; libc.so.6: its dynamic symbols only"*. A frame still unnamed stays
`lib+0xoffset`.

**Why `curl`**: the public servers are HTTPS, and linking a TLS stack into the
tool for one optional request was the alternative; every machine this tool
installs on has `curl`, and one that does not gets that sentence as the reason.

Ground truth (`a_stripped_build_is_named_from_debuginfod_only_when_asked_and_as_the_unstripped_one_is`):
the stripped calltree, unwound by `dwarf`, against a debuginfod server on
loopback that serves the unstripped build (same build id) by the real protocol
(`GET /buildid/<hex>/debuginfo`) and counts what it is asked. Without a client:
zero requests, no `spin` anywhere, frames `calltree-strip+0x...`. With one: one
request for the call tree's build id, and every `main ... spin` path equal to
the unstripped build's. Again with the same client: zero new requests of any
kind -- the cache answered the one it had, the run remembered the one it did
not (Arch's `libc.so.6` is stripped, so the client asks for it too).

**Wired since the fifteenth wave**: `--symbols` itself, as a flag and a
`symbols:` field of a session file; the profile builds its client from the
unwind choice. See "The fifteenth wave" below.

## 5. The profile's drops, per ring

`TaskSampler::lost_by_task() -> Vec<(i32, u64)>` (every task whose ring lost
anything, including exited tasks, whose losses are kept when their ring is
released) and `ThreadSampler::lost_by_cpu() -> Vec<(u32, u64)>`. `cpu.profile`
reads them at every drain -- `stop` closes the sampler before the losses are
gathered -- and implements `Collector::losses_by_ring` with one `RingLoss` per
ring: `task:<tid>` for a process, `cpu:<n>` for a machine, a core set or a
cgroup, each with a note naming the ring. Each is a part of `losses()`: both
come from the same `PERF_RECORD_LOST` records.

Ground truth (`a_ring_that_overflows_is_charged_to_the_task_it_belongs_to_and_no_other`):
two threads, one burning its cpu and one blocked in `pthread_join`, sampled at
20 kHz into 8-page rings and not drained for 500 ms. The burner reports its own
tid; the per-ring account charges exactly that tid, and its parts add up to the
lost records drained.

**What it changes in `tests/cross_compute.rs` (J's)**: the lossy profile's drops
now land on the series of the task whose ring overflowed, not on every task's;
`tests/runner.rs`'s
`a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed` stops
skipping wherever a ring overflows at 50 kHz.

## 6. What frames were lost, said by the mode that lost them

`Unwinder` counts frameless-leaf samples **by the object the leaf is in** (the
mapping's file name). Under `fp`, the note reads *"229 of 456 samples landed in
a function that sets up no frame pointer (168 in libc.so.6, 61 in bash), so the
frames under the leaf are not its callers: most of them are outside the
target's own code, which is where the callers are lost; `--stack fp+leaf`
recovers the leaf's caller and `--stack dwarf` the whole chain"*. "This target
was built without them" is said only when most frameless leaves are in the
target's executable *and* the executable keeps no frame pointers; a target that
keeps them with frameless leaves (gcc from `-O1`) is said to be that.

Under `fp+leaf` a fact counts what was recovered (*"recovered the caller of the
leaf in 281 of 281 samples ... each is marked `[recovered]`"*) and a note what
was not; under `dwarf` a fact counts the walks that reached their root, and
notes the ones cut by the copy or stopped early.

## Ground truth, and what it measured

`tests/unwind.rs` builds one C program four ways (frame pointers everywhere;
`-O1 -fno-omit-frame-pointer -momit-leaf-frame-pointer`, a frameless `spin`;
`-fomit-frame-pointer`; that, stripped) whose time goes to `spin` in shares
known by construction: parse 20 %, hash 40 %, sort 30 %, checksum 10 %. Each
scenario runs the real binary at 1999 Hz over about a second of work and holds
every sample in `spin` to its branch's whole path from `main` (at most 1 %
broken) and each branch to its share, within four binomial standard deviations
of the samples taken plus two points for the callers' own loops.

| scenario | build | mode | holds |
|---|---|---|---|
| `fp_recovers_the_whole_tree_...` | fp | fp | tree and shares |
| `fp_loses_the_caller_of_a_frameless_leaf_...` | leaf | fp | ≥ 95 % of spin samples lose exactly spin's caller; the note names `calltree-leaf` and `fp+leaf` and does not say "built without" |
| `fp_leaf_recovers_the_caller_...` | leaf | fp+leaf | tree and shares; spin's caller marked, one mark per stack |
| `dwarf_recovers_the_whole_tree_...` | nofp | dwarf | tree and shares; nothing marked |
| `fp_over_a_build_without_frame_pointers_...` | nofp | fp | most spin samples broken; the note says "built without them" |
| `auto_chooses_by_how_the_target_was_built_...` | all four | auto | fp+leaf, fp+leaf, dwarf, dwarf, each with why |
| `a_mode_this_machine_cannot_serve_...` | leaf | lbr, sframe | refused, naming `pmu:branch_call_stack` / `kernel:sframe` |
| `dwarf_counts_and_says_the_stacks_deeper_than_its_copy` | deep (24 levels of 1 KiB), shallow (2) | dwarf | the loss path: deep, ≥ 90 % of samples in `spin`, ≤ 10 % reach `main`, no walk holds all 24 levels, ≥ 90 % keep part of them, and the degradation counts the cut walks (≥ 90 % of the `spin` samples); shallow, ≥ 90 % reach `main` through `descend` ×3 and nothing is said to be cut |
| `each_mode_copies_what_it_declares_...` | nofp | fp+leaf, dwarf | the copy is min(declared, end − sp) in ≥ 90 % of samples, the end being the top of the target's own `[stack]` mapping or the first page above sp its pagemap says is not resident, whichever is lower (below); dwarf costs more per sample than fp, pooled over three runs of the two side by side, measured from outside (below); the samples are the target's own clock divided by the period, within 10 %; the tool's reported cost does not exceed the kernel's count for the run beyond the target |
| `user_only_stacks_...` | writes | fp, user half | no kernel frame; the samples cover the program's own user time and leave its system time out (`getrusage`); fewer than half the samples of both halves |
| `the_choice_says_user_only_exactly_when_...` | leaf | auto, none | the note iff the kernel's rule, read from `/proc` and the binary's capabilities, refuses the kernel; neither refused |
| `a_ring_that_overflows_...` | two threads | fp | per-ring drops are the burner's |
| `a_stripped_build_is_named_from_debuginfod_...` | strip, nofp | dwarf | as in 4; the whole paths from `main` are the same set, and a walk that stopped short of `main` is the leaf end of a whole one, in under 5 % of samples |

**The cost, measured** (debug build, this machine, beside five other streams):
the kernel copies 256 bytes per sample under `fp+leaf` and min(8192, the
stack's depth) under `dwarf` -- 2344 bytes for the calltree's main thread,
whose stack is that deep -- but the ring holds the full 8192 plus registers
either way, which is what makes `dwarf` fill the rings about a hundred times
faster than a ten-frame `fp` chain. What the tool spends per sample is
measured from outside it: over a steady 1.5 s in the middle of a 3 s run, the
cpu time the kernel accounts to `omn`'s threads (`/proc/<pid>/task/*/schedstat`)
against the target's own (the account `CLOCK_THREAD_CPUTIME_ID` reads), whose
nanoseconds divided by the period are the samples taken. Measured: **14.4 and
14.8 µs a sample under `fp`, 27.1 and 22.3 µs under `dwarf`** (42.8 / 44.1 ms
and 80.2 / 66.2 ms of `omn` over about 1485 ms of the target). The target's
own thread time for the same work is 919 ms and 899 ms in whole runs, so the
copy in the target's context is below this measurement's resolution.

**A copy stops at the first page that is not resident.** The kernel copies the
user stack where no page fault may be taken (`copy_from_user_nmi`), so a copy
from sp upwards ends early at a page of the `[stack]` mapping nothing has
touched. One was among the top four pages in three of five runs measured here: `exec` lowers the stack
pointer by up to 8 KiB at random below the strings it writes at the top
(`arch_align_stack`) before it writes the argument vectors, and a page wholly
inside that gap is never written. On the CI's runner, whose environment is
long, every dwarf sample of one run carried about 2600 bytes against a stack
more than 8192 deep, and the test, which held the copy to the mapping's top,
failed on all 432 (the sixteenth wave). It now reads the target's pagemap while
it runs; the frames between sp and `main` are always resident, so what is cut
is the argument vectors, never a frame.

What a sample costs in cpu time moves with the machine's load by as much as
the separation between the modes: two `fp` runs a minute apart at load 12 to
18 on 24 cpus measured 14.9 and 30.4 µs a sample, and the test, which compared
the dearest `fp` run with the cheapest `dwarf` one, failed twice in a row. The
two modes now run side by side, three times, compared pooled: under 14 busy
loops beside the other streams, `fp` 18.6 to 25.1 µs and `dwarf` 30.5 to
61.1 µs, separating in each of three runs.

**The tool's own cost figure is not the tool's cost.** Over whole runs,
`cost.json`'s `collector_cpu_ns` says 26 ms (`fp`) and 46 ms (`dwarf`) where
`wait4` counts 986 ms and 1016 ms of `omn` beyond the target. The same run with
`--stack none --emit topk` costs about 60 ms beyond the target, so the rest
follows the stacks, and most of it comes after the target has exited (wall
time 1.9 s against 0.97 s). The likely cause is symbolizing in a debug build,
but that was not measured. The test holds the reported figure only as a lower
bound. Filed in `docs/open-findings.md` (below).

## The last check

On the review fixes (the commit after `3d6ccf7`): one `make check`, red at the
first binary as always (the session index). Then one `cargo test --workspace
--no-fail-fast`, clippy `--all-targets -D warnings` and `cargo fmt --check`,
built with `CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0` because `/home` was
full (below). 45 test binaries green, clippy and fmt clean, the binary still
carrying `cap_bpf,cap_perfmon`, and exactly two reds:

- `tests/cli.rs` `the_session_indexes_hold_exactly_the_sessions_on_disk`: the
  three sessions below, expected until the coordinator adds their rows.
- `tests/scenario_cpu.rs` `an_unwinder_this_build_lacks_is_refused_before_the_program_starts`
  (P's file): it expects `--stack dwarf` refused as this build's gap, and
  `dwarf` is served now. Red by design; the change it needs is filed for P.

`tests/unwind.rs` 13/13 and `tests/stack_capture.rs` 8/8. The lib tests
filtered to `profile`, `stack`, `unwind` and `perf` pass 99/99.

On `a195314`, the run before, `family_sched`'s #5 and #6 of `docs/flaky.md`
went red once and passed alone. The run before that also went red once
each in `scenario_cpu` `a_counted_window_is_the_same_number_in_every_format_that_carries_it`,
`runner` `a_source_attached_for_one_phase_counts_that_phase_and_nothing_of_the_other`
and `triage` `an_input_this_machine_cannot_serve_is_dropped_by_name_or_fails_the_question`.
Each passed alone, and they are written under "for the flaky tally". None of
them went red in this run.

**The disk filled twice** during the wave (`/home` at 100 %, six worktrees'
targets and the main checkout's 22 GB): `rustc` failed with exit 101 and no
message, then with `No space left on device`. This stream's target was cleared
and the last run built with `CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0`,
which changes no test's behaviour and cuts every test binary to a fraction; the
coordinator's merge run may want the same.

## Review

An adversarial review of `a195314` and `3d6ccf7` raised nine findings.
What came of each:

1. **`Cargo.toml` and `Cargo.lock` edited (framehop).** Real, and kept. Item 2
   of the brief names `framehop` or libdw as the way `dwarf` is served, and a
   dependency lives only in `Cargo.toml`. No stream owns that file. The
   alternative, a DWARF unwinder of our own, is what `ROADMAP.md` rules out.
   The two files change by one dependency line and its lock entries, and
   nothing else. The coordinator is asked to take it at the merge (filed).
2. **`src/unwind.yaml` instead of `catalog/unwind.yaml`.** Real, kept in the
   fourteenth wave, and **closed in the fifteenth**: the table is
   `catalog/unwind.yaml` now (see "The fifteenth wave" below). As it stood:
   `Catalog::load_dir` parses every YAML file of the directory as a list of
   entries. `tests/runner.rs` and `omn --catalog <dir>` load `catalog/` that
   way, so a mode table there breaks them; no YAML shape can be both a mode
   table and an empty entry list. The move needs one line in `catalog.rs`,
   which is R's (filed). Until then the table is data compiled in from
   `src/`, which keeps the build green.
3. **The choice does not reach the resolved file.** Real, open in the
   fourteenth wave, and **closed in the fifteenth**: `ResolvedRequest.stack`
   (see "The fifteenth wave" below). As it stood:
   `ResolvedRequest` is `resolve.rs` (P's), and `resolved.json` is written by
   `main.rs` (R's) from the resolution, before any collector runs. None of U's
   lines can add a field to it. The `StackChoice` serializes and is ready;
   its content reaches the profile's notes and degradations. Filed for P and R
   with the exact field.
4. **`--symbols` cannot be reached.** Real, open in the fourteenth wave,
   and **closed in the fifteenth** (see "The fifteenth wave" below). As it
   stood: The flag is a
   `Request` field and a `cli.rs` argument, both R's. U owns `StackMode` and
   its parsing, and folding a second axis into `--stack` would be a second
   mechanism beside the one the brief asks for. `debuginfod: None` stays until
   R lands the field, and the one-line change is filed with it.
5. **`--stack none` refused at paranoid 2.** Real, and **fixed**. See
   section 3 and `a_user_only_choice_excludes_the_kernel_even_without_a_stack`.
6. **The user-only test agreed with its own probe.** Real, and **fixed**. The
   expectation is now the kernel's rule, read from `/proc` and the binary's
   capabilities. `--stack none` is covered too. At paranoid 2 the user-only
   half of the other test is now held to the program's own `getrusage`, not
   only to the absence of kernel frames. The comparison with both halves
   cannot run where the kernel is refused, and that half remains the CI's.
7. **The cost was the tool's own figure.** Real, and **fixed**. It is now
   measured from outside, per sample, against the target's own clock. The
   tool's figure is held only as a lower bound, and a finding came of it (the
   cost section of "Ground truth", filed).
8. **No scenario for `dwarf`'s loss path.** Real, and **fixed**:
   `dwarf_counts_and_says_the_stacks_deeper_than_its_copy`.
9. **`scenario_cpu`'s refusal test red on this branch.** Real, and kept red.
   The file is P's, and the test asserts that `dwarf` is not served, which
   this stream's work makes false. The change it needs is filed for P, and
   the coordinator's merge takes both at once.

## For `DECISIONS.md`

- **The mode table is `catalog/unwind.yaml`** (superseding the fourteenth
  wave's `src/unwind.yaml`). `Catalog::load_dir` passes over
  `catalog::UNWIND_TABLE`, so the three scenarios that copy the whole
  directory for `--catalog` (`tests/triage.rs`, `scenario_cpu.rs`,
  `scenario_proc.rs`) copy the table with the entries and still load.
- **`auto` is decided by the target as well as the machine.** `fp+leaf` for a
  build with frame pointers, `dwarf` for one without: the cheap mode where it
  loses nothing, the expensive one only where the cheap one would be a profile
  of leaves. So a Rust debug binary (no frame pointers in `main`) is profiled
  with `dwarf` by default now, and a system-wide profile with `fp`.
- **`lbr` and `sframe` are declared and not collected.** No machine this project
  runs on offers either; an untested decoder is worse than a refusal.
- **`framehop` for DWARF**, over libdw: pure Rust, nothing to find at install.
- **At a paranoia that refuses the kernel, every mode is user-only, `none`
  included**: the kernel refuses the sampler itself, not only the stack's
  kernel half.
- **A mode's cost is measured from outside the tool**: the kernel's account of
  `omn` against the target's own clock, over a steady stretch. The tool's own
  figure is held as a lower bound, never as the reference.
- **`curl` for debuginfod**, over a TLS dependency.
- **A recovered frame is marked in its label** (` [recovered]`): the label is
  what reaches the results, flame graphs and `omn diff`, and an inferred frame
  is different evidence from a walked one. `diff` between an `fp+leaf` and a
  `dwarf` profile therefore sees the mark as a different name.
- **The unwind module is `stack::unwind`** (`#[path = "unwind.rs"]` inside
  `stack.rs`), because `src/lib.rs` belongs to nobody; it is also where it
  belongs, beside the symbolizer it shares the ELF reader with.
- **The IBS sources keep frame pointers**: `auto` means `fp` there, and a forced
  mode that needs more is refused by name.

## Filed for another stream

Under "Needs from another stream — the fourteenth wave, stream U" in
`docs/open-findings.md`:

- **R** (`src/request.rs`, `src/cli.rs`): `--symbols local|debuginfod`.
- **R** (`src/main.rs`) and **P** (`src/resolve.rs`): the `StackChoice` in the
  resolved file.
- **R** (`src/catalog.rs`): `load_dir` skips (or loads) `unwind.yaml`, so the
  table can move to `catalog/`.
- **coordinator / D**: `Cargo.toml` gains `framehop` (a file nobody owns).
- **P** (`tests/scenario_cpu.rs`): the refusal test's list becomes `lbr`,
  `sframe`.
- **whoever owns `cost.json`**: a profile with stacks reports about 3 % of the
  cpu it costs (a finding, measured).

## Session index rows

For `examples/README.md` and `GUIDE.md`:

| session | what it shows |
|---|---|
| `unwind-auto.yml` | `--stack auto` choosing the unwind mode by machine and target, and saying what it passed over |
| `unwind-leaf-caller.yml` | the same program under `fp` and `fp+leaf`: the leaf's caller lost, then recovered and marked |
| `unwind-refused.yml` | `--stack lbr` refused before the launch, naming the requirement and the cpu |

## Matrix cells this should move

`scripts/matrix.sh --configs nocaps` (every default kernel: 5.15, 6.1, 6.6,
host): the guests run as an unprivileged user at the kernel's default
`perf_event_paranoid` (2 on the Ubuntu mainline kernels), so `cpu.profile`
goes from **missing** (`callchain: perf_event_open refused ... Permission
denied`) to **available**, served with user-only stacks, and the triage
methods built on a profile with it. `caps` and `root` cells: unchanged
(`CAP_PERFMON` allows the kernel half). No cell should move for `lbr`/`sframe`
(missing everywhere), and `perf:user_stack` is available on every kernel the
matrix boots (it dates from 3.7).

---

## The fifteenth wave: what the fourteenth left, landed (stream "resolver")

The three items filed above for R and P are closed; the handoff is
`docs/fixes/w15-resolver.md`.

- **The table is `catalog/unwind.yaml`.** `Catalog::load_dir` passes over
  `catalog::UNWIND_TABLE`, so every `--catalog <copy of catalog/>` scenario
  copies the table with the entries and still loads. A directory named with
  `--catalog` or `OMN_CATALOG_DIR` carries its own modes:
  `stack::unwind::use_table_in` reads `<dir>/unwind.yaml` when it is there,
  by the same loader rules, and the compiled-in table otherwise. Held by
  `catalog::tests::a_copy_of_the_catalog_directory_loads_without_reading_the_unwind_table_as_entries`
  and `tests/cli.rs::the_unwind_modes_are_read_from_the_catalog_directory_they_sit_in`.
- **`--symbols local|debuginfod`** is a `Request` field (`symbols:` in a
  session file) and a flag. The choice carries it: `StackChoice` gained
  `symbols` and `symbol_servers`, `choose` refuses `debuginfod` with no
  `DEBUGINFOD_URLS` before the launch, and `profile::Prepare` builds its client
  from the choice (`StackChoice::debuginfod`) when none is handed in, so the
  line of `src/collect/cpu/mod.rs` filed for U is no longer needed. A source
  whose collector does not read it (`stack::UNWOUND_BY_CHOICE`, `cpu.profile`
  alone today) is refused with exit 3 rather than accepted and never asked.
  Held, against a server on loopback that counts its requests and serves the
  unstripped build whose build id the stripped copy shares, by
  `tests/unwind.rs::symbols_from_debuginfod_are_asked_for_from_the_command_line_and_the_session_file`
  and `..._with_nowhere_to_ask_are_refused_before_the_program_starts`.
- **The choice is in the resolved file.** `ResolvedRequest.stack`, set by the
  resolver for the collectors that unwind through `choose`, so `--dump-resolved`
  and `resolved.json` say the mode, every mode considered and why, before the
  run. The IBS sources are left out on purpose: they serve frame pointers
  whatever `choose` says, and recording its choice for them would misstate
  their stacks. Held by
  `tests/unwind.rs::the_resolved_file_says_how_the_stacks_will_be_unwound_before_the_run`
  against the profile's own note of the mode it used.
- **A refused choice is a refused request, in the dump as in the run.**
  `stack::recorded_choice` used to turn a refusal of `choose` into an absent
  `stack`, so `--dump-resolved --symbols debuginfod` with no
  `DEBUGINFOD_URLS` printed a resolved file the run then refused. The resolver
  now refuses it (`ResolveError::StackRefused`), with `choose`'s own reason
  and a code from `unwind::Refusal::whose`: the machine's for `debuginfod`
  with no server named and for `auto` with nothing served, the build's for a
  mode nobody reads, the request's for one the table does not declare. A
  forced mode this machine lacks what it needs for is the machine's by
  `Whose`'s definition and is held at the build's code (3), which is what
  `tests/scenario_cpu.rs` asserts; the change is filed. Held by
  `tests/unwind.rs::a_choice_the_run_refuses_is_refused_by_the_dump_with_the_same_reason`,
  against the run's own refusal of the same request.

## The eighteenth wave: what a profile costs (stream "profile")

The handoff is `docs/fixes/w18-profile.md`. The finding was the coordinator's
(`docs/open-findings.md`, "a user-mode-only profile costs 280 % of the
target"): at `perf_event_paranoid` 2 without `CAP_PERFMON`, `cpu.profile`
over `sqlite3`'s commits spent 266 % of the target's cpu and the agent's way of
`tests/cases.rs` was stopped by its 100 % budget.

### What a user-only profile costs

**Which mode, and why that is not the question.** The triage asks for
`cpu.profile` at `sample@997hz` with `--stack fp` forced
(`src/triage/latency.rs`), so no ladder is walked: at paranoid 2 the choice is
`fp`, user-only. Reproduced on this machine (paranoid -1) by a build whose
`kernel_callchain_allowed` answered no -- the patch is in the handoff, not
committed -- because a capability-less copy of `omn` at paranoid -1 still has
the kernel's half: the brief's suggested reproduction does not take the
user-only path here. Over the case's two workloads, debug build, beside nine
other streams:

| run | both halves | user-only |
|---|---|---|
| 500 fsync-bound commits, `--symbols debuginfod`, `--budget 100%` | stopped at 0.1 s: 129 to 179 % | stopped at 0.1 s: 116 to 131 %, once ran out at 9 % |
| the same, no debuginfod, whole run | 20 to 38 % before, 20 to 30 % after | 13 to 58 % before, **7 to 29 % after** |
| 50 000 commits on a WAL, whole run, before | 7.8 % | 4.1 % |

Two costs, neither of them the mode's:

1. **Every tick re-read every process's mappings**, whatever the drain held:
   `/proc/<pid>/maps` twice for a mode with CFI (`Symbolizer::refresh`,
   `Cfi::refresh`), and the executable's link once more. Measured with a
   temporary counter: 0.25 to 0.4 ms a tick of a debug build, 55 to 150 ms of
   `omn` over a 2.4 s run in which the target burnt 0.5 s. A user-only profile
   of a program that lives in system calls gets few samples (21 to 55 against
   500 to 640 with both halves, the same commits) and paid for every tick
   anyway. **Fixed**: `ProfileCollector::drain_into` re-reads only what is
   being sampled. A process the last drain held samples of, or that no drain
   has held samples of yet, is re-read before the drain, as every process
   was; one that was idle and is sampled again, after the drain and before
   its stacks are interned; an idle one, not at all. The first version read
   every sampled process after the drain, and across a launched program's
   `execve` that names the child's samples of this tool's own code against
   the new image: at 100 kHz, 82 to 266 frames fell in no mapping where the
   old rule left 7 to 54 (found by the suite: `tests/triage.rs`'s lossy
   profile carried 81). The corrected rule matches the old one again, 10 to
   223 against 10 to 238 over eight runs each. Cost, user-only, seven pairs
   run alternately at load 22 to 27: 13.2 to 58.2 % before, **7.1 to 28.7 %
   after**, separating in each pair. Both halves did not move measurably:
   there the drain itself dominates, about 150 µs a sample of a debug build,
   most of it naming kernel frames (`src/stack.rs`, filed). Held by
   `a_tick_with_no_sample_of_a_process_does_not_reread_its_mappings`: a child
   that burns and then `exec`s `sleep`, over 80 ticks, is read 27 or 28 times
   where the old rule read it 81, and never in the drains of its sleep.
2. **The first sample in a module named from debuginfod loads that module's
   symbols**, about 25 ms of a debug build for `sqlite3`'s debug file, inside
   the window. The budget's first check, 0.1 s in, weighs it against a target
   that has burnt 13 to 21 ms by then: 116 to 179 %, **both halves and
   user-only alike**. This is the 266 %: what paranoid 2 changes is not the
   cost but which run happened to be stopped. Not fixed here: the load is
   `src/stack.rs`'s, and whether a first check may stop a run on a one-time
   cost against a few milliseconds of target is the budget's
   (`src/collect/mod.rs`); both filed in the handoff.

**What paranoid 2 would add** that this machine cannot show: the kernel's own
refusal path (`probe_open` refused, rather than told no), and a target whose
denominator is unchanged -- the budget reads the scheduler's nanoseconds,
which count the target's kernel time whether the profile may sample it or
not. Reasoned, not run.

**The declared band.** `pmu:sw` declares `low` (under 1 %). The bench
measured 0.61 % for a compute-bound launch; against a program that waits on
its disk, the tool's per-tick work is a share of very little, and a debug
build's measures 6 to 30 % here. The `cost:` line is the bench's; the numbers
are filed there. The choice says it: the user-only degradation now ends
*"a program that spends most of its time there yields few samples, and what
the profile costs it is then mostly the tool's work on each tick, which does
not shrink with them"*.

### The tool's own figure, held whole

`each_mode_copies_what_it_declares_...` held `collector_cpu_ns` only as a lower
bound (fourteenth wave: 26 ms said where `wait4` counted 986 ms). The cost
record has carried `before_window_cpu_ns` and `after_window_cpu_ns` beside it
since the sixteenth wave, and their sum is now held to the kernel's count from
both sides: never above it, and at least half of it. Measured: 1169.9 ms said
against 1178.1 ms counted (`fp`), 1150.9 against 1159.5 (`dwarf`); the window
alone, 55.5 and 72.3 ms.

### The stripped build's second shape

`a_stripped_build_is_named_from_debuginfod_...`: the unstripped reference
missing whole branches, seen once in the sixteenth wave. Not reproduced in 22
runs alone at load 22 to 27, then seen once in a workspace run at load 30 to
48: the reference's 299 samples held only `hash_block` (146) and
`parse_record` (153), no `compare` and no `checksum`, where the stripped run
beside it held all four. A branch absent from a profile of an unchanged
binary is one that did not run while it was sampled, so this reads as the
workload or the window, not the names. Left to the flaky tally with that
message.

### `mem.faults`' sampled rung where paranoid 2 serves it

The budget's ladder moves `mem.faults` from `trace` to `sample` on weighed
samples, and `tests/budget.rs` holds the move where the kernel's half is
allowed, which neither CI cell is. The rung itself on the paranoid-2 path is
now held in `page_faults.rs`:
`sampled_in_user_mode_the_weighed_samples_are_the_faults_the_thread_took`
samples one fault in 16, user mode only, over 4096 pages this thread stores
to, and the weighed samples are the thread's own `RUSAGE_THREAD` fault count
within two periods. Measured three times: 256 samples weighing 4096 faults,
where the thread took 4096.
