# Stream U, fourteenth wave — every way to unwind a stack

A profile's stacks are only as good as the way they were unwound, and until
this wave there was one way: the kernel's frame-pointer walk, which loses the
caller of every function that sets up no frame, and which the kernel's default
`perf_event_paranoid` refused outright because it asked for the kernel half.

**What landed.** The unwind modes are data (`src/unwind.yaml`), probed, ranked
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
rest.

---

## 1. Unwind modes are data

`src/unwind.yaml` declares each mode like a backend: ordered best first, with
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

The `callchain` requirement string is still answered by P's `capabilities.rs`,
which calls `callchain_supported`, so the probe opens what the collector will
open. Ground truth here (`tests/unwind.rs`,
`user_only_stacks_carry_no_kernel_frame_and_leave_the_system_calls_out`): a
program that makes 600 000 one-byte `write`s to `/dev/null`, sampled both ways.
User-only: no kernel frame and no kernel-mode sample at all. Both halves: most
samples in the kernel, and more than twice the user-only count for the same
work. **The CI's next run at paranoid 2 is the rest of the check**: the probe,
the choice and the note on a machine that actually refuses.

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

**Not wired**: `--symbols` itself. The request field and the flag are R's
(`src/request.rs`, `src/cli.rs`); filed below with the one line in
`src/collect/cpu/mod.rs` (mine) that reads it, which is `debuginfod: None`
until then.

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
| `each_mode_copies_what_it_declares_...` | nofp | fp+leaf, dwarf | the copy is min(declared, stack top − sp) in ≥ 90 % of samples, the stack top read from the target's own `[stack]` mapping; dwarf costs more per sample than fp, in two alternated runs each |
| `user_only_stacks_...` | writes | fp, user half | no kernel frame; fewer than half the samples of both halves |
| `the_choice_says_user_only_exactly_when_...` | leaf | auto | the note iff the machine refuses the kernel |
| `a_ring_that_overflows_...` | two threads | fp | per-ring drops are the burner's |
| `a_stripped_build_is_named_from_debuginfod_...` | strip, nofp | dwarf | as in 4 |

**The cost, measured** (debug build, this machine, beside five other streams):
the kernel copies 256 bytes per sample under `fp+leaf` and min(8192, the
stack's depth) under `dwarf` -- 2344 bytes for the calltree's main thread,
whose stack is that deep -- but the ring holds the full 8192 plus registers
either way, which is what makes `dwarf` fill the rings about a hundred times
faster than a ten-frame `fp` chain. What the collector spends per sample,
fixed costs included: 15.6 µs under `fp`, 25.6 µs under `dwarf` over 1840
samples; the target's own thread time for the same work 913 ms and 911 ms, so
the copy in the target's context is below this measurement's resolution.

## The last check

On `a195314`, the steps of `make check` with the tests run `--no-fail-fast`
(`make` only stops at the first red binary, and the session index is always the
first): clippy `--all-targets -D warnings` clean, `cargo fmt --check` clean, and
the tests red in exactly these:

- `tests/cli.rs` `the_session_indexes_hold_exactly_the_sessions_on_disk`: the
  three sessions below, expected until the coordinator adds their rows.
- `tests/scenario_cpu.rs` `an_unwinder_this_build_lacks_is_refused_before_the_program_starts`
  (P's file): it expects `--stack dwarf` refused as this build's gap, and
  `dwarf` is served now. Red by design; the change it needs is filed for P.
- `tests/family_sched.rs`: `a_task_moved_between_two_cpus_is_counted_once_per_move`
  and `tasks_sharing_one_cpu_wait_the_share_the_scheduler_owes_them` (#5 and #6
  of `docs/flaky.md`), and two tests of the same file that panicked on the
  mutex those two poisoned. The file passed whole when rerun alone.

The run before it, on the same code less the last fixes, also went red once
each in `scenario_cpu` `a_counted_window_is_the_same_number_in_every_format_that_carries_it`
(1 038 991 485 instructions for a loop that retires 10⁹), `runner`
`a_source_attached_for_one_phase_counts_that_phase_and_nothing_of_the_other`
(windows 11 766 instructions apart) and `triage`
`an_input_this_machine_cannot_serve_is_dropped_by_name_or_fails_the_question`
(`cpu_queue` where `cpu` was built, load average 12 to 20); each passed alone,
none is in a tally, none touches a stack. Written under "for the flaky tally".

**The disk filled twice** during the wave (`/home` at 100 %, six worktrees'
targets and the main checkout's 22 GB): `rustc` failed with exit 101 and no
message, then with `No space left on device`. This stream's target was cleared
and the last run built with `CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0`,
which changes no test's behaviour and cuts every test binary to a fraction; the
coordinator's merge run may want the same.

## For `DECISIONS.md`

- **The mode table is `src/unwind.yaml`, not `catalog/unwind.yaml`.**
  `Catalog::load_dir` parses every YAML file in the catalog directory as catalog
  entries, and three scenarios copy the whole directory for `--catalog`
  (`tests/triage.rs`, `scenario_cpu.rs`, `scenario_proc.rs`); a mode table
  there breaks all three. Moving it is one line in `catalog.rs` (filed).
- **`auto` is decided by the target as well as the machine.** `fp+leaf` for a
  build with frame pointers, `dwarf` for one without: the cheap mode where it
  loses nothing, the expensive one only where the cheap one would be a profile
  of leaves. So a Rust debug binary (no frame pointers in `main`) is profiled
  with `dwarf` by default now, and a system-wide profile with `fp`.
- **`lbr` and `sframe` are declared and not collected.** No machine this project
  runs on offers either; an untested decoder is worse than a refusal.
- **`framehop` for DWARF**, over libdw: pure Rust, nothing to find at install.
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
