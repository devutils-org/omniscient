# The eighteenth wave — stream resolver

Branch `feat/w18-resolver`, from `main` at `cc3adc1` (phase A merged). The
files: `src/resolve.rs`, and the test lines `1aa114f` moves
(`src/collect/mod.rs`'s one test, `tests/{scenario_proc,scenario_sched,
scenario_power,scenario_mem,scenario_gpu,scenario_io,cli}.rs`), plus the
pinned `net.hwts` twin in `tests/scenario_net.rs`.

| Item | State | Commit |
|---|---|---|
| `1aa114f` cherry-picked: the pin and the key hint ask the machine first | landed | `e04ef9a` |
| The key hint ranks an unwritten carrier this machine can run above a missing one | landed | `e04ef9a` |
| The eleven tests of `docs/fixes/w17-core.md` §2, in the same commit | landed | `e04ef9a` |
| `scenario_io`'s two rows | landed (in the eleven) | `e04ef9a` |
| The pinned `net.hwts` twin | landed | `267842a` |
| `docs/DECISIONS.md`'s exit-3 line, re-worded | below, for the coordinator | -- |
| Review round: the power skip line, the rank's unit test, the cli hint, the sched comment | landed | see "Review" |

## 1. One order (`src/resolve.rs`)

`1aa114f` applied cleanly over `cc3adc1`, unchanged: a pinned backend that
lacks a requirement here is refused for the machine (exit 1) whether or not
this build collects it, and `key_elsewhere` probes before it asks the build.
A pinned backend that lacks nothing and has no collector still resolves, and
the runner refuses it as the build's (exit 3).

**One change beyond `1aa114f`: the hint's ranking.** `KeyElsewhere::rank`
put `Missing` (1) ahead of `Unwritten` (2). That order was written when
`Missing` meant "collected, and this machine lacks something", so a reader
told to fix the machine got a working run. After `1aa114f`, `Missing` also
covers backends nobody wrote, and the ranking then disagreed with the walk:
with the capabilities, `io.bytes --system -k task` named `bpf:tp` ("this
machine cannot run it", tracefs unreadable, exit 1) although `bpf:raw_tp`,
earlier in the chain and runnable here, is merely unwritten. The walk, over
the same chain, passes over a backend that lacks something and stops at the
first one this machine could run (`unimplemented_candidate`), so its answer is
the build's whenever such a backend exists. The rank is now `Forceable` 0,
`Unwritten` 1, `Missing` 2, `Unfit` 3, ties to the chain's order. With it,
`scenario_io`'s and `cli`'s key tests hold unchanged with the capabilities,
and their new machine branch holds without them.

## 2. The eleven tests

Each now branches on what `omn list --format json` probes for the pinned
backend (a `lacks_here(source, backend)` helper in each file, or the gpu
listing it already reads): **lacks something** → exit 1, "no viable backend
on this machine", the backend named as pinned, each missing requirement named
as `<needs>: missing`, and never "no collector is written"; **lacks nothing**
→ exit 3 and "no collector is written", as before.

| Test | Was | Is |
|---|---|---|
| `collect::tests::a_filter_the_backend_pushed_down_is_not_run_a_second_time` | pinned `io.latency` `bpf:tp` | w17-core's patch: the backends' `requires` cleared, so the subject is the placement |
| `scenario_proc::a_source_this_build_does_not_implement_is_refused_in_the_chains_order` | `..._exits_three_and_names_the_backend` | the branch; `--dump-request` asserted on the build branch |
| `scenario_sched::pinning_a_backend_this_build_lacks_is_refused_in_the_chains_order` | `..._blames_the_build_and_not_the_machine` | the branch over the session's five pins, in session order; "all five named" on the build branch only (see §4) |
| `scenario_power::pinning_a_backend_this_build_does_not_have_names_it_in_the_chains_order` | `..._names_it_and_exits_three` | the branch, per unwritten backend |
| `scenario_power::the_three_refusals_are_told_apart_by_what_they_say` | `power.freq force:bpf:tp` for the build's refusal | the first unwritten power backend `omn list` says lacks nothing here (`power.freq` `bpf:raw_tp` with the capabilities); skipped with a reason where none does |
| `scenario_mem::forcing_a_backend_with_no_collector_names_it_in_the_chains_order` | `..._exits_three_and_names_it` | the branch (`assert_refused_in_the_chains_order`) |
| `scenario_mem::the_kernel_sources_tell_their_three_refusals_apart` | the four `bpf:tp` pins exit 3 | the same helper |
| `scenario_gpu::forcing_a_backend_this_build_does_not_implement_names_it_in_the_chains_order` | `..._exits_three_and_names_it` | the branch; "is pinned" or "(pinned with --backend force:" both count as saying so |
| `scenario_io::forcing_a_backend_this_build_has_not_written_names_it_in_the_chains_order` | `..._names_it_and_exits_three` | an exit-1 arm that requires the listing to say it lacks something; the exit-3 arm requires it to lack nothing |
| `scenario_io::a_request_keyed_on_what_the_counter_files_do_not_name_...` | exit 3 on every machine | exit 1 naming the carrier "this machine cannot run" where both carriers lack something; otherwise exit 3, naming a carrier that lacks nothing |
| `cli::a_request_keyed_on_what_its_backend_does_not_attach_...` | exit 3 | the same two answers, each held to what pinning the named carrier does (exit 1 for both, or exit 3 "no collector is written") |
| `scenario_net::the_trace_leg_of_the_sweep_...` | (request) | the pinned twin: `--backend force:so_timestamping` exits what the walk exits, naming `phc: missing` or "no collector is written" |

## Shown, and reasoned

**Shown**, on this machine (tracefs mounted and unreadable by uid 1000, a
readable PTP clock):

- Before the test changes, over `1aa114f` with the capabilities, the same
  eleven failed that w17-core listed, at the lines it gave; `net.hwts` passed.
- After them, all eleven and the twin pass **with the capabilities** and
  **without them** (the same binary, copied without its file capabilities and
  hard-linked over `target/debug/deps/omn-*`, since cargo re-links
  `target/debug/omn` from there before each test run; `make` regranted them
  after). Without them, `scenario_power`'s build half printed its skip line,
  and every other test took its machine branch.
- `cargo test --lib resolve` (28) and `cargo test --lib collect::tests::` (50)
  pass.
- `net.hwts --mode trace -- true` exits 3 here by the walk and by the pin.

**Reasoned, not shown**:

- The twin's exit-1 branch (no readable PTP clock): this machine has one. The
  CI runners do not (`/dev/ptp1` is root's), and that branch is what the twin
  is for.
- `omn list` probes without a target, the resolver with one. For every pin
  these tests make the two agree here; a backend whose requirement depends on
  the target (`symbols:malloc`, `phc`) could make them disagree, and the test
  would then fail loudly, naming both, rather than pass wrongly.
- The gpu test's count (`forced == unwritten`) now counts a written backend
  refused for the machine as well; on a machine with a GPU and a written
  backend lacking something, the equality would fail and name the counts.

## The suite

At `267842a`, ten streams' worth of load on the machine:

- `make check`: red on `cases::every_case_names_its_known_cause_both_ways`
  alone (`sqlite3-durability/commits-that-wait-for-nothing`,
  `both-ways-name-the-same-suspect`: on-cpu leaf `_raw_spin_unlock_irqrestore`
  one way, `bpf_trace_run2` the other -- the flat-profile flip of the tally's
  cpython line); passed when rerun alone once.
- `cargo test --workspace --no-fail-fast`: 2270 passed, 4 failed, 11 ignored.
  Every test this stream touched passed. Rerun alone once:
  - `family_sched::a_launched_programs_children_are_followed_and_wait_what_their_schedstat_says`
    (6.5 us of 273 us uncounted): passed.
  - `scenario_io::a_pipe_wait_is_in_the_syscall_layer_and_not_in_the_file_layer`:
    passed.
  - `unwind::a_stripped_build_is_named_from_debuginfod_only_when_asked_and_as_the_unstripped_one_is`
    (297 vs 299 samples, a different mix): passed.
  - `runner::a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed`:
    **failed again** (75165 dropped against 74914-ish expected; "the kernel
    throttled this profile 3866 times and its rings account for 0 of them ...
    filed for stream profile", in the test's own message). Nothing here
    touches `cpu.profile`, `emit.rs` or the runner; it is the merged phase A's,
    for profile and launcher.

## Could not do

- **A session's machine refusal names one request at a time.** The pin now
  takes the resolver's error path for a backend that lacks something, and
  `src/main.rs` (launcher's) returns at the first resolution error, so
  `examples/sched-forced-backends.yml` names only `sched.offcpu` here, where
  the build's refusal named all five (defect 8 of `docs/scenarios/sched.md`,
  which this is the machine-side twin of). Not mine to fix; filed below.
- **The scenario documents** that cite the renamed tests are not this
  stream's (below).

## Privileged runs wanted

None. Nothing here needs root, a sysctl or a module. The unprivileged view
was taken without privilege (a copy of the binary carries no file
capabilities), as described under "Shown"; the CI's `stock` cell is the same
view on a runner without a readable PTP clock, and is where the twin's exit-1
branch will first run.

## Decision lines for `docs/DECISIONS.md`

- **One order for "unbuilt" and "missing"** (row at `docs/DECISIONS.md:770`,
  "Decided, not yet in the code" → settled): *A backend both unbuilt in this
  build and missing a requirement here is the machine's (exit 1) on every
  path -- the walk, a pin, and the key hint. Landed in the eighteenth wave
  (`e04ef9a`, `1aa114f` cherry-picked with its tests). The key hint ranks a
  carrier this machine can run and nobody wrote above one this machine cannot
  run, as the walk stops at the first backend it could run.* Reason: one
  request exited 1 or 3 by the path that reached it.
- **The exit-3 line** (`docs/DECISIONS.md:127`), re-worded: *"Valid, and not
  built" is exit 3, wherever it is discovered, on a machine that lacks nothing
  the backend needs* -- and the italic seventeenth-wave note replaced by:
  *Eighteenth wave: a backend both unbuilt and missing a requirement here is
  the machine's on every path ("one order for unbuilt and missing"); a test
  that wants the build's refusal pins a backend whose requirements hold, or
  branches on what `omn list` probes for it.*

## Needs from another stream

- **launcher** (`src/main.rs`, the resolution loop near line 282): a session
  whose requests are refused at resolution should name every refused request,
  as the build's refusal does (defect 8 of `docs/scenarios/sched.md`), rather
  than return at the first. Since `1aa114f`, a session of pinned backends this
  machine cannot serve reaches that path; `scenario_sched`'s pin test asserts
  all five names only on its build branch and says why in a comment.
- **engine** (`examples/sched-forced-backends.yml`, its header): it still
  says `echo $?   # 3` and that the exit is 3 and not 1 because "every
  requirement of every backend below probes available here". Since `e04ef9a`
  the session exits 1 on a machine where a pinned backend lacks something
  (this project's own, with tracefs unreadable by uid 1000) and 3 only where
  each lacks nothing. The header should say both, as `proc-not-built.yml`'s
  text needs to (below).
- **Coordinator** (`docs/scenarios/{sched,power,mem,gpu}.md`,
  `docs/families/proc.md`, `docs/GUIDE.md`, `examples/README.md`): the renamed tests
  and the changed answer.
  - `docs/scenarios/sched.md`: `pinning_a_backend_this_build_lacks_blames_the_build_and_not_the_machine` → `pinning_a_backend_this_build_lacks_is_refused_in_the_chains_order`
  - `docs/scenarios/power.md:200`: `pinning_a_backend_this_build_does_not_have_names_it_and_exits_three` → `pinning_a_backend_this_build_does_not_have_names_it_in_the_chains_order`
  - `docs/scenarios/mem.md`: `forcing_a_backend_with_no_collector_exits_three_and_names_it` → `forcing_a_backend_with_no_collector_names_it_in_the_chains_order`
  - `docs/scenarios/gpu.md`: `forcing_a_backend_this_build_does_not_implement_exits_three_and_names_it` → `forcing_a_backend_this_build_does_not_implement_names_it_in_the_chains_order`
  - `scenario_proc`'s `a_source_this_build_does_not_implement_exits_three_and_names_the_backend` → `a_source_this_build_does_not_implement_is_refused_in_the_chains_order` (cited only in `docs/fixes/`, which are history)
  - `docs/scenarios/proc.md:161,182`, `docs/GUIDE.md:362`, `examples/README.md:191`:
    `proc-not-built.yml` is exit 1 on a machine whose tracefs this uid cannot
    read, exit 3 elsewhere.
- **io** (`docs/scenarios/io.md`): `forcing_a_backend_this_build_has_not_written_names_it_and_exits_three` → `forcing_a_backend_this_build_has_not_written_names_it_in_the_chains_order`.

## Review

The fix round, over `a4bdf0e`:

- **`scenario_power`'s skip line would have failed the skip gate (defect).**
  `the_three_refusals_are_told_apart_by_what_they_say` skipped its build half
  without capabilities with a reason no pattern of `.github/expected-skips*.txt`
  matches. The skip line now ends with every unwritten power backend's missing
  requirements, each with the first sentence of its probe's reason
  (`power.freq bpf:raw_tp: bpf: missing: loading a tracing program needs
  CAP_BPF and CAP_PERFMON (...)`, `... tracefs is mounted but not readable
  ...`): the machine's own reason, which the common list already accepts
  (`CAP_BPF|...`, `tracefs`, `unprivileged_bpf_disabled`). **Shown**: run
  without the capabilities (the copy hard-linked over `target/debug/omn` and
  its `deps/` twin, as under "Shown" above), its log through
  `.github/scripts/skip-gate.sh` with the common list: "every skip names a
  privilege or a device this cell does not have", three classes counted.
  **Reasoned**: the CI cells word the bpf probe's first sentence the same way
  (`src/capabilities.rs` puts "needs CAP_BPF and CAP_PERFMON" first on every
  path), so the gate matches there too.
- **The rank had no machine-independent test (gap).**
  `resolve::tests::a_filter_field_another_backend_carries_is_named_for_what_keeps_the_request_from_it`
  gains a half: every `rwbs` carrier renamed unwritten and runnable, then the
  chain's first given a requirement that cannot hold (a path under a directory
  that does not exist); the hint is `Unwritten("fs:unwritten1")`, the later
  carrier, on any machine. It fails under the old rank (`Missing` first).
- **`examples/sched-forced-backends.yml`'s header (gap).** Filed for engine
  under "Needs from another stream"; `examples/**` is not this stream's.
- **`cli`'s key test assumed `bpf:raw_tp` (nit, fixed).** It now reads the
  carrier the hint names off the message and pins that one, so a machine where
  the hint names `bpf:tp` (raw_tp lacking BTF or kernel 5.8, `bpf:tp`
  runnable) is held to the same two answers. Passes with and without the
  capabilities here.
- **`scenario_sched`'s doc comment (nit, fixed).** It no longer claims the
  machine branch holds what the build branch does: it says defect 8's
  five-name coverage is held on the build branch only, and why, pointing at
  the launcher item above. No test can hold it on the machine branch until
  the launcher names every refused request.

The suite at the review round's commit (other streams' suites running on the
machine at the same time):

- `make check`: red on `runner::a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed`
  alone, the same failure as above (78225 dropped against its ring's count,
  throttled 4116 times, "filed for stream profile" in its own message). It
  failed again when rerun alone; it is phase A's, for profile and launcher.
  `make check` stops at the test step, so clippy (`--workspace --all-targets
  -D warnings`) and `cargo fmt --check` were run after it: both clean.
- `cargo test --workspace --no-fail-fast`: the log was shared by mistake with
  another stream's run of the same command (one scratchpad file), so its
  totals are not this branch's alone. The failures it shows were each rerun
  alone once on this branch: `scenario_runtime::a_probe_follows_every_thread_of_the_process`,
  `unwind::dwarf_recovers_the_whole_tree_of_a_build_without_frame_pointers`
  and `scenario_mem::a_walk_that_fits_in_l1_is_never_timed_and_one_that_does_not_waits_on_memory`
  passed; `runner::a_profile_s_losses_...` failed, as above. `cargo test
  --lib` passed whole (1482).
