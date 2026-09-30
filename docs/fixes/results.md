# Stream R, fourteenth wave — the results say it

Three streams had filed enum variants only R's exhaustive matches could take,
and two had filed things the engine knows that no rendering printed. The
vocabulary landed first, on `main` before the rest of the wave built on it;
then a window that no longer claims what its collector could not read, the
engine's amounts in the text, a container measured as its cgroup, and the
filed smaller ones, each checked against the code first.

**What landed**: the vocabulary (1); every window closes before its
collector's last read (2); `overlap` and `woke` print their amounts, a chain
column that fits two hops, what each evidence level establishes, and the
decomposition given the session's joins (3); `--container` bound to the
cgroup its runtime names (4); the smaller ones and the CI's failures in R's
files (5).

**Left**: a session with a `paired` part is still refused at validation, by
P's `validate.rs` (one line, filed by J last wave); the eleven catalog entries
that say `container: false` where they serve a cgroup (filed for P and U);
the last line of the `psi` move, which is two owners' at once (filed for K and
the coordinator).

---

## 1. The vocabulary — `2e1e534`, on `main` as `32033ca`

`Unit::Watts` (`as_str` "watts", `pprof_unit` `("watts", false)`, the text
units), `KeyKind::Node` and `KeyValue::Node { node: u32 }` (`node:{n}`),
`ResolveError::KeyNotCarried { source_id, backend, key, elsewhere }` with its
arms in `resolution_is` (`Whose::Request`) and `resolution_hint` (none: the
message names `--backend force:`). P and K were told to rebase onto it.

## 2. A window closes before its collector's last read

Stream S measured it against the kernel's own `ru_nvcsw`: `collect::run` set
every window's `closed_ns` once all the collectors had stopped, and a drained
collector's last read is inside its own `stop()`, so what happened between
that read and the last collector's stop was inside the window and in no row --
6 of 930 blocks with `sched.offcpu` alone, 71 with five collectors after it.

The fix is one instant taken **before the first stop**, and every window the
run held to its end closes there; a detached window closes just before its own
stop, at the detach. S asked for a timestamp per collector, just before each
`stop()`. That also never claims a span its collector did not read, and it
gives each source of one session a different window, microseconds to hundreds
of milliseconds apart (`sched.lock`'s stop took 0.8 s here):
`family_cpu::one_session_counts_several_sources_over_one_window` holds a
session's sources to one window, and failed. One instant before the first stop
is before every collector's last read, so it keeps both properties. What a
stop drains after it is still in the rows; it is the window that no longer
claims it.

The join engine reads a `from` window that closes before the run's end as a
detach and ends its open states there (`window_closed`), which turned a kept
allocation's `still_open` into `window_closed` in
`cross_memory::every_large_allocation_is_found_with_its_fate...`. A
`Collection` now says whether it was detached (`Collection::detached`, never
serialised when false), and `main.rs` gives the engine the window of a source
that ran to the end as open to the end. The engine is J's and unchanged.

The cost account of a detached collector still ends after its stop, which it
paid for.

### Ground truth

`tests/runner.rs::a_window_closes_before_its_collector_s_last_read_not_after_the_last_stop`:
a task outside `omn` blocks every 2 ms until told to stop, logging each sleep
on `CLOCK_MONOTONIC` with the voluntary switches the kernel counted for it; the
run holds `sched.offcpu` and four more `sched` sources on it, and the task
keeps blocking past the end. Every logged block that began after the window
opened and ended before it closed has to be one `sched.offcpu` kept or
reported losing.

| | kept | inside the window, by the kernel | since `omn` started |
|---|---|---|---|
| before the fix | 734 | 806 | 947 |
| after | 733 | 725 | 884 |

Before the fix, 72 of the task's blocks were inside the window and in no row --
S's 71 -- and the test fails; after it, none. `kept <= since omn started` is
the ceiling.

## 3. What the engine knows, printed

- **The decomposition takes the session's joins**:
  `join::decompose_with(.., &session.joins)` at both call sites, `main.rs` and
  `triage/engine.rs`. A `paired` part now waits on P's validator alone
  (`check_decomposition_in`), which is refused before the run with "it read
  the decomposition without the session's joins" until it lands.
- **`overlap`** prints what it credited: each interval's share, its
  `FROM..TO` in ms from the first shown, the events it holds, and the
  readings' totals -- credited, what fell before the first reading of its key
  or after the last, and the baselines that covered nothing.
- **`woke`** prints per waker `PAIRS`, `WAITED` (the summed close-to-wakeup
  time, which is what a `paired` part reads) and `MEAN`, and the total under
  the table.
- **The chain column** is as wide as the widest key shown, at least the old
  44: `interrupt (hardirq) -> task:128611 -> task:128610` is 46.
- **What a level establishes**: once per run, under the first join or
  decomposition that shows a level, `evidence LEVEL: <establishes>; it does
  not establish <does_not_establish>` for every level the run's tables use,
  weakest first, and for no other.

```
join waiting-per-interval  proc.threads -> sched.futex  evidence TEMPORAL
  4 of 4 `sched.futex` events overlapped an interval of `proc.threads`'s readings (100.0 %); 0 overlapped no reading's interval
  `total_ns` split by the share of each event's span inside each interval: credited in all +1.00s, and +0ns fell before the first reading of its key or after the last
  1 readings were the first of their key, with nothing before them to cover

  heaviest intervals, in ms from the first shown
    KEY                            FROM         TO       CREDITED   EVENTS
    task:358717                   300.0      400.4      +100.37ms        2
    ...
    task:358717                     0.0      100.0       +40.60ms        1

  evidence TEMPORAL: the two sides overlapped in time, and nothing else ties them: a hypothesis, nothing more; it does not establish that one side caused, served or contains the other: anything else that ran in the same interval fits the same way
```

### Ground truth

`tests/encoders.rs::an_overlap_join_reads_the_same_in_text_as_in_its_json`
runs `examples/join-overlap-waits.yml` once, as JSON, and renders that run as
text: every interval the JSON credits is a row of the text with its amount to
the renderer's two decimals, and the baselines and the level's sentence are
there. The JSON is held to the workload, which waits four times on a condition
nobody signals and brackets each wait on its own clock: 1 000 329 033 ns
waited, 1 000 223 342 ns credited and none uncredited. The `woke`, chain and
sentence renderings have unit tests in `encode.rs` against amounts the test
chose. A `paired` part through `omn` cannot be run on this branch (above).

## 4. A container is its cgroup

`omn::request::find_container(name) -> Result<Container, String>`, and the
signature S filed, `cgroup_of_container(name) -> Result<PathBuf, String>`.
The runtime is asked the way a user would: `docker inspect --format
'{{.State.Pid}}' <name>`, then `podman`, then `nerdctl`, each that is
installed, or only the one `OMN_CONTAINER_RUNTIME` names; the init pid's
`/proc/<pid>/cgroup` unified line, under wherever `mountinfo` says the
cgroup2 hierarchy is, is the cgroup. `main.rs` binds `Target::Container` to
`Target::Cgroup` before resolution, so every collector that serves a cgroup
serves a container, and the resolver and the catalog see the cgroup it is.
The session keeps the container, as asked -- `--dump-request` prints it -- and
the resolved request carries the cgroup, as measured; the run says which it
found: `note: container `web` runs in cgroup /sys/fs/cgroup/... (pid N, from
`docker`), which is what is measured`.

Refused before anything is attached, exit 1: a name no runtime knows, with
what each runtime said (`no container named `x`: `docker` does not know it
(Error: No such object: x)`); a machine with none of them, with `--cgroup` as
what to pass instead; a stopped container, which has no cgroup; a cgroup-v1
machine.

### Ground truth

`tests/cli.rs::a_container_is_measured_as_the_cgroup_its_runtime_names`: a
runtime on `PATH` -- a script answering `docker inspect` -- knows one
container, a task this test moved into a cgroup it made. That is what a
container is to the kernel, and it needs no image or daemon. The task and a
copy of it beside the cgroup each block every 2 ms, logging their sleeps with
the kernel's voluntary-switch count. `sched.offcpu --container <name>`: the
note names exactly the cgroup the test made and the pid the runtime gave;
**388 blocks kept of the task inside, 387 by the kernel's count inside the
window, 0 of the task beside**. `a_container_no_runtime_knows_is_refused_with_what_it_said`
holds both refusals.

What it leaves: eleven entries that serve a cgroup say `container: false`, and
the validator refuses them before the binding (filed for P and U, with the
list). The collectors that refuse `Target::Container` by name are no longer
reached through `omn`.

## 5. The smaller ones, each checked first

| Item | State |
|---|---|
| `pub mod psi;` in `collect/mod.rs` (N) | `pub use mem::psi;`: `crate::collect::psi` is a path now. Declaring the file twice is refused by clippy's `duplicate_mod`, so the declaration moves in one commit across R's and K's files -- filed, with the four lines |
| `Collector::facts` for `net.softirq` (N, K's file) | `facts()` has been on `main` since the thirteenth wave; the one string's move is K's, filed again |
| flaky tally #20, `emit.rs` | **already landed**: `stack_counting` calls `measured`, an untimed load is `unmeasured` |
| one `parse_cpu_list` | `bpf/syscall.rs`'s counts through `capabilities::parse_cpu_list` (still `possible`, which a per-cpu map is sized by, not `online`) |
| two tests sharing one bench report | `report.<pid>.<call>.<tag>.json`, a counter per call; the pid stays first for `sweep_leftovers` |
| `Cost::Extreme`'s doc | "over 20 %, with no bound above", and what the bench measured in it: the shim at +600 % to +1000 %, `proc.syscall` at +296 % |
| `omn list --format json` carries the sweep (M, twelfth wave) | already landed, `the_json_listing_carries_the_sweep_the_text_table_shows` |
| who applied a predicate (W, twelfth wave) | already landed, `the_filter_line_says_who_applied_the_predicate` |

### The CI's failures in R's files (`../omniscient-ci-logs/`, both runs)

| Test | Run | Now |
|---|---|---|
| `collect::tests::a_system_run_with_nothing_to_stop_it_is_refused` | both | asks `cpu.util --system` (`/proc/stat`), not `gpu.temp`; skips with the resolver's reason if even that is refused |
| `bench_capabilities::every_resolved_file_carries_the_prefetcher_state` | both | resolves `proc.threads`, which every machine serves |
| `cli::a_result_format_is_refused_for_a_request_dump` | both | **a defect**: `--dump-request --format folded` resolved first and answered "no viable backend". A dump's format is refused before the machine is asked, exit 2 |
| `cli::a_filter_on_a_field_no_event_carries...` (`power.cstate`) | both | skips with the resolver's sentence |
| `cli::a_collector_that_needs_a_process_refuses_a_system_target...` | 1 | replaced: the container it used is resolved now (4) |
| `encoders`, three `cpu.profile` tests | both | skip with the machine's refusal (paranoid, no viable backend), and run where U's user-only callchain serves |
| `encoders::a_known_allocation_lands_on_the_chrome_trace...` | 1 | **the test was wrong**, twice: zeroing 64 MiB took two polls, so the largest rise was part of it (reproduced here: 43 MB then 24 MB), and the process grew by less than 64 MiB. The rise is every adjacent poll that rose by a twentieth of the program's own growth, and the step is held to that growth, which the program reads off its own `/proc/self/status` |
| `runner`, two `cpu.instructions` phase tests | both | `try_resolved`; skip with "will not count instructions here" |
| `triage`, seven | both | a question refused because its one indispensable input has no viable backend skips with the refusal |
| `aggregation::...second clean run was not clean` | skip | prints the run's loss lines on the skip's own line |
| `budget::what_the_shim_spends_inside_the_target_is_most_of_what_the_program_lost` | 2 | **a defect in the shim**: 610 ms accounted for 396 ms lost. A timed call the thread was preempted in added a scheduler slice, seventeen times over. A span over 200 us -- three orders of magnitude past anything a hook does -- adds nothing; the turn still counts as timed |

## Checks

- While iterating: `cargo test --lib` (1327, green), `tests/{cli,runner,encoders,aggregation,budget,recorder,triage,bench_capabilities}.rs` green, and `cargo test -p omn-preload`.
- `make check` on the final tree: red only on `scenario_sched::an_overflowing_wakeup_ring_says_what_it_could_not_hold`, tally #3, which passed alone at once.
- `cargo test --workspace --no-fail-fast` once: 1986 passed, 23 failed, 8 ignored. Twenty of the 23 are `scenario_io` failing with **"No space left on device"**: `/home` was at 99 % through this stream's run, with six worktrees' `target/` directories and the main checkout's 22 GB on it (this stream ran `cargo clean` twice and built with `CARGO_INCREMENTAL=0` to keep going). The other three passed alone: `scenario_mem::both_huge_page_backends_read_the_same_promotion_counter` (tally #21), `scenario_sched::one_known_number_of_round_trips_is_found_by_four_sources` (tally #1; it failed once more alone, then passed twice), and `scenario_net::a_queue_that_delays_every_packet_is_measured_as_that_delay` ("200 packets ... found 401"), which is in no tally.
- In an earlier `--no-fail-fast` run, three `omn-mcp` protocol tests reported `omn` without `CAP_BPF` while `target/debug/omn` carried it, and passed alone; and `collect::tests::the_runner_closes_the_window` (in the thirteenth wave's notes) failed once in `make check` at a load of 25 and passed alone.
- The commits were cut from the tree `make check` ran on; the intermediate ones were not checked each on its own.

## Index rows for the sessions added

None: this stream added no session.

## For `DECISIONS.md`

- **A run's windows close at one instant before the first collector is
  stopped**, and a detached one just before its own stop. Rather than one per
  collector (S's request): both never claim a span a collector did not read,
  and only this one keeps a session's sources on one window. A join treats a
  source that ran to the end as watching to the end.
- **A container is bound to its cgroup before resolution, by asking its
  runtime** (`docker`, `podman`, `nerdctl`, or `OMN_CONTAINER_RUNTIME`), not by
  reading runtime state files, whose layout is each runtime's own. The session
  keeps the container; the resolved request carries the cgroup; the run says
  which. A container no runtime knows is exit 1 -- a problem with this run,
  which may exist later -- not exit 2.
- **A dump's format is checked before resolution**: whether a format can hold
  a request does not depend on the machine.
- **The shim's account leaves out a timed span over 200 us**: it is a
  preemption, never the hook, and the account is low by the hook's own
  sub-microsecond share of that one call rather than high by a time slice.

## Filed for other streams

Under "Needs from another stream — the fourteenth wave, stream R" in
`docs/open-findings.md`:

- K: the `psi` declaration's other half (and the coordinator, since both
  files change in one commit); `net.softirq`'s count into `facts()`.
- P: `validate.rs`'s container sentence, and J's `check_decomposition_in`
  line, which is now all a `paired` session waits on.
- P and U: `container: false` dropped from the eleven entries that serve a
  cgroup.
- P: `.github/expected-skips.txt`, the cgroup-ancestry skip wording.
- The flaky tally: the triage tail test (changed, see there), and from this
  file's checks, `scenario_net::a_queue_that_delays_every_packet_is_measured_as_that_delay`.

## The matrix cells this should move

No source's probe or collector changed, so no cell of the sweep should move.
What should move is the CI's two cells (`../omniscient-ci-logs/`): on the
next push, every test of R's files listed in section 5 goes from failed to
passed or to a skip the gate expects, in both `run1-paranoid4` and
`run2-paranoid2`. Checked by the coordinator's push; locally,
`scripts/matrix.sh` is not needed for it.
