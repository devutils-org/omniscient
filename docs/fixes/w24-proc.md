# The twenty-fourth wave, stream proc

Branch `feat/w24-proc`. Every item in `BRIEF.md`'s `### proc` landed, the
defect first, apart from what the last section asks of other streams.

**Shared layer, said first.** Three files outside this stream's own:

- `src/event.rs` and `src/request.rs` -- the `KeyKind` append seam:
  `KeyKind::Syscall` and `KeyKind::Comm`, their parse and display arms, and
  the matching `KeyValue::Syscall { name }` and `KeyValue::Comm { comm }`
  (variant, constructor, `kind()` arm, `Display` arm). Appended only. Merged
  with main's `Domain` (power) in `7eaecd1`: `ALL` is 17 long. **If stream io
  also adds a `comm` key** (bpftrace 4 names `io.size` and `io.latency`), the
  merge must keep one `Comm`, not two.
- `src/bpf/mod.rs` -- one `mod proc_calls;` line and one `pub use
  proc_calls::{Call, CallsNarrowing, CallsProgram};`, for the new program file.
  No other change to the eBPF layer's shared files.

## What landed

| Commit | What |
|---|---|
| `c2f5753` | **fix**: `proc.syscall` counts each call under the name it was made with (bpftrace 1, defect). The task's name moved from the kernel row's value to its key (32-byte key), read on every call; a rename starts new rows. Each row also counts the calls that returned an errno (`errors`, perfsched 3). Every event carries `task`, `syscall` and `comm` keys: `-k syscall` is one run's `strace -c` table (perfsched 1), `-k comm` the per-name one (bpftrace 4). |
| `25789e3` | **`proc.tasks`** (top 2, 7): the machine's processes (`--system`) or every thread of a pid or launched program, on `fs:proc`, one event per task per poll at the poll's one timestamp. Value `busy`, the share of one cpu since the previous poll (`top`'s `%CPU` / 100), or over the task's life when first seen (`ps`'s), as `since` says. Carries pid, tid, ppid, comm, state, uid, threads, rss_bytes (`statm`), vsz_bytes, cpu_ns, utime_ns, stime_ns, nice, priority, minflt, majflt, cpu, age_ns; keyed task and comm. **This is the source stream top's process table reads.** |
| `61cc95a` | `proc.signals`' sender: `sender_comm` read in the kernel beside `sender_pid`, both filter fields (bpftrace 6); `comm` as a key on `proc.exec`, `proc.exit`, `proc.signals`. |
| `8fc5a4a` | `proc.exec` per launch (bpftrace 3): each exec carries `ppid` (`real_parent->tgid`, read in the kernel); on a pid or launched target the run keeps the program's execs and its descendants' -- in order, then again for a child whose parent's exec came later (a subshell), with `/proc`'s parents walked for a fork that never exec'd, and the previous drain's undecided execs looked at once more. |
| `36fe274` | **`proc.calls`** (perfsched 3-4, bpftrace 6): one record per syscall, a new pair of programs in `src/bpf/proc_calls.rs` writing a 184-byte ring record per returning call -- began, `lat_ns`, raw `ret`, tid, tgid, nr, comm -- and `openat`'s path read from the caller's memory at exit. Events add `errno`, `error`, and `fd` for a successful `openat`; keyed task, syscall, comm; a `syscall` predicate is pushed into the entry program. |
| `7eaecd1` | merge of `main` (the `KeyKind` seam with power's `Domain`; lazy's `default:` block in `catalog/proc.yaml` kept after this stream's entries). |
| `6e64617` | `docs/scenarios/proc.md`: defect 10 (the comm) written up as fixed, and the wave's additions with their numbers. |

## Shown vs reasoned

Shown, on this machine, against something independent of `omn`:

- **The defect.** A workload of 500 failing opens, a `prctl(PR_SET_NAME)` to
  `omnref-open`, 500 opens and 500 failing opens: `--filter 'syscall ==
  "openat" && comm == "omnref-open"'` counted **1000** (it counted 1 before).
  Keyed `comm,syscall`: `comm:omnref-open syscall:openat n=1000`,
  `comm:python3 syscall:openat n=548`. `errors` summed over `-e raw`: 500 and
  506 (the 500 chosen failures, and six of python's own import probes).
- **`proc.calls` against strace.** `strace -f -e trace=openat` on the same
  workload: 648 opens, 406 ENOENT; `proc.calls --filter 'syscall ==
  "openat"'`: 648 records, 406 with `errno` 2. 200/200/200 for the chosen
  path before the rename, the opened path after it, and the failing one after
  it, each with the right `comm`.
- **`proc.tasks`.** A C workload of three spinning threads and one napping,
  `poll@500ms`, keyed comm: `omn-spin` n=18 min 0.973 mean 0.992 max 1.000,
  `omn-nap` 0.000. The same process at `--system poll@1s`: 2.904-2.979, where
  `top -b -d 1` read 295.7-298.6 %. `rss_bytes` from `statm`: one process read
  642 pages there and 568 in `stat`'s field, and `top` shows `statm`'s; the
  collector reads `statm`. A `--system` poll of ~490 processes cost about
  25 ms of cpu in a debug build (99 ms over four polls).
- **`proc.exec` per launch.** `sh` running five `echo`s, `( /bin/true;
  /bin/true )` and `/bin/true | /bin/cat`: exactly ten execs, three runs of
  three, beside a `/bin/true` loop that added none, on a machine fifteen other
  streams were building on.
- **The sender.** Kills of a `sleep` from `/usr/bin/kill`: `sender_comm`
  `kill`, `sender_pid` the kill's pid.

Reasoned only: the aarch64 path register for `openat` (`regs[1]`, offset 8)
-- this machine is x86_64, where the offset comes from BTF (`pt_regs.si`).

## Tests run

- `cargo test --lib -- proc_syscall proc_events proc_calls collect::proc
  request:: event:: catalog`: all pass (159 in the last run of the wider
  filter).
- `cargo test --test family_proc`: 6 passed (3 new). `cargo test --test
  scenario_proc`: 29 passed (3 new: the rename, the exec tree, the per-call
  records; one refusal test turned round: `proc.exec --pid` is served now, so
  the pid refusal is asserted on `proc.exit`). Run with the capabilities
  (`make` first, no skips printed).
- `cargo test --test catalog_model` 19 passed; `cargo test --test lazy` 12
  passed (the conflict touched `catalog/proc.yaml`).
- `cargo clippy --workspace --all-targets -- -D warnings` clean, `cargo fmt
  --check` clean.
- Under the wave's load, two scenario tests failed once and passed alone
  once each: `a_known_number_of_calls_and_an_injected_delay_are_both_reported_as_chosen`
  ("the workload sleeps ten times") and
  `defect_1_the_first_reading_of_a_launched_target_is_of_the_program` ("the
  filtered and unfiltered sessions poll the same program at the same rate").
  Noted, not widened.

## Not done, and why

1. **`errors` in an aggregating emit.** `strace -c`'s `errors` column is on
   every `proc.syscall` row, but `summary` sums only the value; a summary per
   `syscall` key carries calls and latency, not errors. It needs the results
   layer to sum a named attribute per series (stream emit; below).
2. **`ppid` on the sched sources** is stream sched's, as the brief says.
3. **`proc.calls` has no `bpf:tp` fallback** and reads paths for `openat`
   only (not `open`, `openat2`, `execve`); `proc.exec`'s tree loses a
   descendant whose forking parent never exec'd and had already exited by the
   drain -- counted in `failed_reads` since the review round, but still lost
   (a `sched_process_fork` program would make it exact).
4. **Examples.** The new scenarios are inline commands, not sessions in
   `examples/` (stream triage's); a session per new source would let a reader
   rerun them with `omn -f`.

## Review

Two findings, both fixed (the fix round's commit is on top of this branch):

- **`proc.tasks --tid` widened to the process, with the tid as its pid**
  (`src/collect/proc/tasks.rs`, defect). Reproduced before the fix: `--tid`
  of one of a 4-thread python's threads gave 4 events, every `pid` the tid.
  Now `Target::Tid` is its own `Whose::Thread { pid, tid }`: the pid is the
  thread's `Tgid:` read at attach, the poll reads `/proc/<pid>/task/<tid>`
  only, a thread that exits ends the run as gone, and a tid not on the
  machine is refused at attach. After: 1 event, `tid` the thread and `pid`
  the process. Tests: `a_thread_target_is_that_thread_alone_under_its_process`
  and `a_thread_that_is_not_there_is_refused` (unit),
  `a_thread_target_is_that_thread_under_its_own_process`
  (`tests/family_proc.rs`, through `target/debug/omn`).
- **`proc.exec`'s tree drops of unknown parentage were uncounted**
  (`src/collect/proc/events.rs`, gap). `Tree::admits` now answers a
  `Verdict`: `In`, `Out` (the walk reached pid 1) or `Unknown` (a parent on
  the way had left `/proc`, or the walk ran past its 64 hops). An exec still
  not `In` after its one carry-over, or at the last drain, and `Unknown`, is
  counted in `failed_reads` with the reason "an exec whose parent had exited
  before its parentage could be read ...". The catalog caveat says so. Test:
  `a_parent_gone_from_proc_leaves_the_exec_unknown_not_outside` (a parent
  above `pid_max` is `Unknown`; this test process, whose parents are live up
  to pid 1, is `Out`). The count is reasoned for a real lost parent: no test
  produces the race (a forking parent that never exec'd and exited before
  the drain) on demand.

Tests of the round: `cargo test --lib collect::proc` 50 passed (3 new),
`cargo test --lib catalog` 66 passed, `cargo test --test family_proc` 7
passed (1 new), `cargo test --test scenario_proc` 29 passed, after `make`
(capabilities granted); clippy (`--workspace --all-targets`) and `fmt
--check` clean.

## Privileged runs wanted

None: everything above ran under the capabilities `make` grants.

## Decision lines

- A syscall row's task name is part of the kernel's key, read on every call:
  a rename is two rows, each call under the name it was made with
  (`proc.syscall`, the twenty-fourth wave).
- `proc.tasks` is the machine's process table on `fs:proc`: one event per
  process (`--system`) or per thread (`--pid`, launch), or the one thread of
  `--tid` under its own process, per poll, value `busy`
  as a share of one cpu since the previous poll, or over the task's life when
  first seen (`since` says which); the resident set from `statm`, as `top`
  reads it.
- `proc.exec` on a pid or a launched program is that process and its
  descendants, followed by the parent each exec carries.
- Per-call syscall records are their own source, `proc.calls`, beside the
  bucketed `proc.syscall`, not a mode of it.

## Needs from another stream

- **emit**: sum a named attribute per series in `summary` (here `errors` on
  `proc.syscall`), so `-k syscall -e summary` is `strace -c` with its
  `errors` column in one run.
- **lazy**: `proc.tasks` is `low` and needs no privilege; the family's
  `default:` block could name it (`poll@1s`, `summary`) beside `proc.threads`.
- **triage**: sessions in `examples/` for `proc.tasks`, `proc.calls` and
  `proc.exec` on a launch, if the scenarios should be rerunnable by `-f`.
- **coordinator**: `CATALOG.md` and `man/omn.1` for the two new sources,
  the two new keys, and `proc.exec`'s new targets.
