# Stream green, eighteenth wave: the CI's two cells green

`BRIEF.md`, "green". The run read is 37155975223 (`b37a925`), both cells red;
its logs are in `../omniscient-ci-logs/37155975223/`. Files changed:
`.github/workflows/ci.yml`, `src/collect/mem/{swap,wss}.rs`,
`tests/encoders.rs`, `docs/flaky.md`, and the `mem.wss` caveat in
`catalog/mem.yaml` (a non-`cost:` line of a source whose collector this
stream owns). `src/collect/mem/testing.rs` needed no change.

Every red of that run, and where it went:

| Cell | Red | Cause | Where |
|---|---|---|---|
| both | skip: *"bpftool not found for kernel 6.17.0-1022"* | the azure kernel's `linux-tools` ships no bpftool | section 1, fixed in the workflow |
| `defaults` | skip: *"no ancestor of /omn-ci/job delegates the memory controller"* | two tests of `tests/cross_machine.rs` make one cgroup name at once, and the cell had one delegating level to fall back to | section 2, worked around in the workflow; the test's defect filed |
| `defaults` | `swap::memory_pushed_out_of_a_limited_cgroup_is_counted_as_swap`, `swap::the_machines_counter_sees_at_least_what_one_cgroup_did` | `memory.max` OOM-kills when written pages wait on a cloud disk; the runner **has** swap | section 3, fixed in the tests |
| `defaults` | `wss::a_process_that_holds_much_and_touches_little_reports_the_little` | a clear of the referenced bits does not flush the TLB; 4 096 small pages fit the runner's | section 4, fixed in the test, caveat corrected |
| `stock` | `encoders::a_known_allocation_lands_on_the_chrome_trace_...` | **suspected**, not shown: an ESRCH from a `smaps_rollup` read straddling the program's `execve` ends the run | section 5; open. The same read fixed in `wss.rs`, `footprint.rs`'s fix filed. A second, local failure of the same test found and fixed |
| both | `scenario_io::a_burst_of_writes_shows_up_as_a_queue_...` | stream io's | not here |

---

## 1. bpftool, from upstream's pinned static build

`linux-tools-6.17.0-1022-azure` was already installed on the image and put no
`bpftool` anywhere under `/usr/lib` (the step's own warning, both cells);
`/usr/sbin/bpftool` is Ubuntu's wrapper, which answers *"WARNING: bpftool not
found for kernel 6.17.0-1022"*. The workflow's new `bpftool` step:

- downloads `libbpf/bpftool` `v7.7.0`'s `bpftool-v7.7.0-amd64.tar.gz`,
  pinned by version and sha256 (`09150596...a268c`, the release's own
  `.sha256sum`, checked here). It is a static binary: no library to miss,
  which matters -- this machine's own `/usr/bin/bpftool` fails with
  *"libLLVM.so.23.1: cannot open shared object file"*;
- takes it if it answers `btf dump file /sys/kernel/btf/vmlinux`;
- otherwise -- not downloaded, checksum wrong, or downloaded and unable to
  dump (fix round) -- installs `linux-tools-$(uname -r)` and
  `linux-tools-generic` and takes the first `/usr/lib/linux-tools*/*/bpftool`
  that dumps;
- installs the one taken as `/usr/local/bin/bpftool`, checks
  it is the one on `PATH` (ahead of the wrapper in `/usr/sbin`), and checks
  the dump has the line the cross-check reads (`[<id>] VAR 'runqueues' `);
- **fails the job** with `::error::` when no candidate dumps, or the dump has
  no such line.

The old step's 35-second `find /usr/lib` is gone. Not cached: 12 MB from a
GitHub release per cell.

**Shown here:**
- the step's script, extracted from the YAML and run with a shimmed `sudo`
  into a scratch prefix: checksum OK, `bpftool v7.7.0`, `[127584] VAR
  'runqueues' type_id=127130, linkage=global`, exit 0;
- the same with a wrong checksum and `apt-get` failing: *"::error::no bpftool
  here dumps /sys/kernel/btf/vmlinux (tried: none)"*, exit 1;
- `bpf::btf::tests::a_variable_is_found_by_the_id_the_kernel_numbers_it_with`
  with that binary first on `PATH`: passes with no skip line. With this
  machine's own bpftool it skips (*"exit status: 127: ... libLLVM.so.23.1"*).
- every step's script passes `bash -n`; the YAML parses.

**Only a run shows:** that the runner reaches `github.com` releases (it does
for `actions/checkout`), and that 7.7.0 dumps the azure kernel's BTF.

## 2. The cell, two delegating levels deep

The brief asked that "Configure the cell" hand `memory` down to `/omn-ci/job`.
A cgroup that holds processes cannot enable a controller below itself, and
`job` holds the job's shell, so it never can. What the CI log shows instead:
`omn-ci` **did** delegate `cpu io memory pids` (*"omn-ci delegates: cpu io
memory pids"*), and the swap tests made limited cgroups under it in the same
run. The skip came from `tests/cross_machine.rs`'s `Cgroup::create`, which
names its cgroup `omn-cross-machine-<pid>.scope` -- the same name for the two
tests that call it (`a_memory_limit_turns_a_cached_read_...` and
`a_drained_reclaim_row_is_credited_whole_...`), both before their `alone()`
lock. In the CI log the second of them made the cgroup and went on to skip for
`CAP_BPF`; the first found the name taken (`EEXIST`) under `omn-ci`, walked up,
found no other delegating ancestor, and reported *"no ancestor ... delegates
the memory controller"*. On a workstation the walk finds a second delegating
level (`user@<uid>.service` above `app.slice`), and the collision never shows.

The step now makes that shape: `omn-ci` and `omn-ci/app` both hand down `cpu
memory io pids`, the shell is in `omn-ci/app/job`, and the step **fails** if
either level lacks one of them. `OMN_CI_CGROUP` follows. The test's own
defect is filed below.

**Reasoned, not shown:** this machine cannot make a cgroup tree whose only
delegating ancestor is the one under test (its own session's ancestors all
delegate). The skip gate, run over 37155975223's two logs with this skip and
the bpftool skip removed, passes both cells (365 and 428 skip lines, every one
expected).

**Only a run shows:** what `a_memory_limit_turns_a_cached_read_into_device_reads_...`
does once it gets its cgroup on the runner. Read from the code it limits the
cgroup, churns 96 MiB of page cache through it, and skips on the session's
`CAP_BPF` refusal, which the gate expects (*"has no viable backend on this
machine"*).

## 3. The swap tests push with `memory.high`

Not a runner without swap: both tests passed `/proc/swaps`' check and
`no_swap_room` (more than 320 MiB free), and in the same run, cell and
`memory.max`, `a_page_of_zeros_is_swapped_without_being_written` **passed**
(it ran, with no skip line -- the sixteenth wave's open item, closed). The
difference is the write: a zero page is recorded rather than written, a
`0xA5` page has to reach a swap file on a cloud disk before reclaim can free
it, and `memory.max` calls the OOM killer when reclaim falls behind
(`memory.events`: *"max 387 oom 1 oom_kill 1"*, *"max 154 oom 1 oom_kill
1"*). `pressure.rs` met the same kill on the same cell in the sixteenth wave
and moved to `memory.high`, and passed in this run.

`memory_pushed_out_of_a_limited_cgroup_is_counted_as_swap` and
`the_machines_counter_sees_at_least_what_one_cgroup_did` now set
`memory.high` (`PUSHED_BY`) and spawn a throttled toucher given 60 s
(`THROTTLED_FOR`). A child still touching then is not a failure: the first
test judges what the child says it had written (`Toucher::bytes_written`,
fix round: the child fills its block a MiB at a time and publishes the count
in the handshake page), read before the counters, and says so on stderr. Not
the child's `smaps_rollup`, which held the pages it inherited from the test
binary, charged elsewhere. A child that wrote less than twice the limit
leaves too little to judge and the test prints a `skipped:` line, which the
CI's skip gate surfaces. The
zero-page test keeps `memory.max`, which passed. Swap checks before the limit
were already there and are unchanged.

**Shown here:** the module's nine tests pass, the three cgroup ones running
(no skip line), on a zram swap. **Reasoned:** that `high` throttles where
`max` killed on the runner -- the kernel's documented behaviour and
`pressure.rs`'s passing on that cell; this machine's swap is too fast to make
reclaim fall behind.

## 4. The working set the TLB holds

*"the child touches 16777216 bytes every few milliseconds and the working set
reads 8351744"*: half. Writing `1` to `clear_refs` clears the referenced bits
and flushes no translation, and a page whose translation the TLB still holds
is touched without a page-table walk, so its bit is never set again. Measured
here with a C walker (N small pages, `MADV_NOHUGEPAGE`, three walks after each
clear, read back from `smaps_rollup`):

| Pages walked | Referenced |
|---|---|
| 64 | 2 %, 8 % |
| 512 | 103 %, 132 % |
| 1 024 | 37 %, 99 % |
| 2 048 | 111 %, 102 % |
| 4 096 | 100 %, 100 % |
| 16 384 | 100 %, 101 % |

The test walked 4 096 pages, which this Ryzen 9 3900X does not keep across a
clear and the runner's cpu did, by half. The test's working set is now 128 MiB
(32 768 small pages) inside 1 GiB held, beyond any second-level TLB's reach,
coalescing included; the ratio asserted is unchanged in kind (working set
under a quarter of resident). Five runs green alone.

The tool cannot see a working set the TLB holds, and its caveat said small
pages "do not fit" and read in full. `catalog/mem.yaml` `mem.wss` now gives
the numbers above and says the undercount is of the hottest pages, largest for
a small working set over short windows.

## 5. The chrome trace that had no event

*"`chrome` carries a timeline and this run has none to write: a timeline was
asked for and no event arrived in the window"* (`stock`). From the log's
timestamps the test panicked 49 ms after its binary started -- including a
`python3 --version` and the tool's own startup -- so the run ended before
the program's first `time.sleep(0.3)` was over: `mem.rss` stopped at its first
reads.

**Shown here:** a `smaps_rollup` opened before an `execve` and read after it
answers ESRCH from a process that is alive and has a new address space, and a
fresh open reads it (a C program, then
`wss::tests::a_rollup_opened_across_an_exec_answers_esrch_from_a_running_process`).
`src/collect/mem/footprint.rs`'s `read_bytes` takes any ESRCH as
`TargetGone("process exited")` -- and `PermissionDenied` as gone too -- and a
gone target ends the run. A launched program is in `execve` exactly when the
window opens and the first reads are taken; `read_to_string` opens and reads,
and a preemption between the two across the exec is that ESRCH.

**Reasoned, not reproduced:** 600 runs of the same request here (`mem.rss -m
poll@25ms -e timeline --format chrome -- sleep 0.2`, 40 at a time, load
average about 20) never hit the window between the open and the read. No
other path I found ends a run that fast with no event; an exec failure (127)
was the other candidate, and nothing on that cell makes `python3` fail.

**Landed in `wss.rs`, which reads `smaps_rollup` the same way:** an ESRCH from
a process still running (`/proc/<pid>/stat` present, state not `Z`/`X`) is
read once more with a fresh open; an ESRCH that persists from a running
process is a counted failed read, not the end; from a zombie, the end
(`an_esrch_ends_the_run_only_when_the_process_is_gone`). In the fix round
the retry became `WorkingSet::rollup_text`, which takes the open-and-read as
a closure, so it is driven:
`an_esrch_from_a_running_process_is_read_once_more_and_never_ends_the_run`
checks a first ESRCH is read again and the second answer stands, a
persisting ESRCH is `Transient` after exactly two reads, EIO is not read
again, and a zombie's ESRCH is `TargetGone` with no second read; and
`a_rollup_opened_across_an_exec_...` now hands the kernel's real straddling
ESRCH to `rollup_text` as its first answer and checks the collector made the
second, fresh read itself. Dropping the re-read, or turning the second
ESRCH into `TargetGone`, fails both. The same change for
`footprint.rs` is filed below; that file is nobody's this wave.

`tests/encoders.rs` on that CI run: the test is right to fail, and only
its failure message changed -- it now says whether the program had
reported its own sizes, which tells "cut short before it ran" from "ran and
the trace is wrong". **The CI failure stays open** until `footprint.rs`
reads past the ESRCH and a CI run reads green; its cause is suspected, not
established (`docs/flaky.md` #27 says so).

**The local red, captured and fixed.** The first round's one uncaptured red
(13 of 14) was chased in the fix round: 30 runs of `cargo test --test
encoders` at load 27, every log kept, gave two reds of this test, both
*"the allocation is not the step in the trace: 9981952 before, 10444800
after, and the program itself grew by 67506176"* -- not the CI's failure.
The trace was right: the program exits normally, the interpreter frees its
`bytearray` while it finalizes, and a poll landing then reads the process at
its size before the allocation, inside the "after" side. The same program
under `omn mem.rss -m poll@5ms -e timeline --format chrome`: the series
ended below 50 MB in 24 of 50 runs, and in 0 of 50 once the program ends
with `os._exit(0)`, which the test's program now does. The same 30 runs
also failed `encoders::a_woke_join_reads_the_same_in_text_as_in_its_json`
once (*"304 paired of 598"* completions for 300 reads: the device's other
I/O counted), filed as `docs/flaky.md` #28, not fixed here.

---

## Shown versus reasoned

- **Shown on this machine:** the bpftool step's both paths and the BTF
  cross-check passing with its binary; the skip gate passing over the run's
  logs with the two skips removed; the TLB table; the ESRCH across `execve`;
  the swap, wss and encoders tests green (`cargo test --lib
  collect::mem::{swap,wss}`, `cargo test --test encoders`).
- **Shown in the fix round:** the `wss.rs` re-read driven through
  `rollup_text`; the local encoders red captured, its cause measured (24 of
  50 against 0 of 50), and 30 of 30 runs of `tests/encoders.rs` green after;
  the bpftool fallback taken when upstream's binary cannot dump; the swap
  and mem collector tests green (`cargo test --lib collect::mem::`, 127).
- **Reasoned from the CI log:** the `cross_machine` name collision (which test
  made the cgroup, which did not); that the runner has swap (both data tests
  passed the swap checks, the zero-page test passed); the 49 ms.
- **Reasoned, not shown:** that `memory.high` keeps the two swap children
  alive on the runner; that the ESRCH race is what ended the `stock` run (suspected, open);
  that a throttled swap child's own count is a floor on what left the cgroup
  (the unfinished branch never ran here: the children finish);
  that 128 MiB is beyond the runner's TLB.

## What only a CI run shows

The cells green, or red on what follows: the release download and 7.7.0 on
the azure kernel's BTF; `cross_machine`'s memory-limit test past its cgroup;
the swap children finishing (or the stderr note saying how far they got)
under `memory.high` on the cloud disk; the walker reading at least half of
128 MiB on the runner's cpu. `scenario_io`'s polled queue stays red on both
cells until stream io lands.

## Review (fix round)

The adversarial review's six points, and what became of them:

1. **#27's cause stated as fact; the local red not read.** Agreed. The
   tally and this handoff now say *suspected* for the CI failure and keep it
   open. The local red was chased: captured (2 of 30), a different failure,
   a test defect (the interpreter freeing the allocation at exit), fixed and
   30 of 30 green after. Section 5.
2. **The `wss.rs` re-read never driven by a test.** Agreed; `rollup_text`
   and two tests now drive it. Section 5.
3. **The unfinished swap child judged by a rollup that held inherited
   pages; a vacuous floor.** Agreed; the child's own count, and a `skipped:`
   line below twice the limit. Section 3.
4. **`docs/flaky.md`'s count line stale.** Fixed: (a) 12, (b) 7, (c) 6,
   (d) 3 over 28 rows (#28 new, #27 counted (d) until shown).
5. **The bpftool fallback only on a failed download.** Fixed: upstream's
   binary that cannot dump falls back too. Driven with a shimmed `curl`
   serving a checksummed binary that cannot dump: *"does not dump"*, the
   fallback's `apt-get` tried, *"::error::no bpftool here dumps ... (tried:
   .../bpftool)"*, exit 1; and the real v7.7.0 download taken (the run
   reached the install, which the shimmed `sudo` refused).
6. **Two delegating levels serve exactly two colliding callers.** Agreed,
   and left: it is a workaround for `tests/cross_machine.rs`'s shared name,
   whose fix (a per-call name, `EEXIST` read as "taken") is filed below. A
   third caller, or a per-pid name elsewhere, would skip with a reason that
   is not the machine's, and the skip gate would name it.

## The suite, at the end

One `make check` at 7235ac7 (load average 20 to 28, ten streams): red at
`tests/cases.rs` (`sqlite3-durability/commits-that-wait-for-nothing`: the
two ways name different on-cpu leaves, `_raw_spin_unlock_irqrestore` against
`bpf_trace_run2`). Then one `cargo test --workspace --no-fail-fast`: clippy
and fmt clean; 8 binaries red, none of them this stream's files --
`cases` (the same), `runner`
(`a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed`),
`triage` (three: Little's law depth, a contended mutex, a lossy profile),
`unwind` (three, debuginfod and the dwarf share), and `budget`,
`cross_machine`, `scenario_mem`, `scenario_net`, which printed no failing
test. Those four rerun alone once: `cross_machine`, `scenario_mem`,
`scenario_net` green, `budget` red once (21 of 22) and green on the next
run. The lib's 1 422 unit tests, `encoders` and the `mem` tests passed.

## Privileged runs wanted

None for this stream's changes. The CI run itself, on the merged branch, is
the coordinator's (`git push` of `main`, then `gh run watch`, then
`.github/scripts/skip-gate.sh` over each cell's `target/test.log` if a cell is
red on a skip).

## Decision lines for `DECISIONS.md`

- **The CI's bpftool is upstream's static build, pinned by version and
  sha256** (`libbpf/bpftool` v7.7.0), with Ubuntu's `linux-tools` as the
  fallback; the step fails when none dumps `/sys/kernel/btf/vmlinux`. Because
  the azure kernel's `linux-tools` ships no bpftool at all, a static binary has
  no library to miss, and dumping a BTF blob does not depend on the kernel the
  tool was built beside. (Moves "the CI installs bpftool" from "Open" to
  settled.)
- **The CI's `defaults` cell is two delegating levels deep** (`omn-ci`,
  `omn-ci/app`, shell in `omn-ci/app/job`), the shape of a systemd user
  session. A leaf holding processes can never hand a controller down, so the
  brief's "down to `/omn-ci/job`" is met one level up, and the step fails if a
  level lacks `cpu`, `memory`, `io` or `pids`.
- **A test that pushes written pages to swap limits with `memory.high`, not
  `memory.max`.** `max` kills when reclaim waits on a slow swap device;
  `high` throttles, and a throttled child is judged on what it touched. A
  zero-page test may keep `max`: its pages are never written.
- **An ESRCH from a procfs file of a process that is still running is not the
  end of the run.** A file opened on the address space an `execve` replaced
  answers ESRCH; the collector reads again, and only a process gone or a
  zombie ends the run.

## Needs from another stream

### To the coordinator (`src/collect/mem/footprint.rs`, nobody's this wave): the `execve` race ends `mem.rss` at its first read

`FootprintCollector::read_bytes` maps `NotFound`, `ESRCH` and
`PermissionDenied` to `SampleError::TargetGone`. A `smaps_rollup` (or
`smaps`) read that straddles the launched program's `execve` answers ESRCH
from a live process (section 5), and the run ends with no event -- the likely
cause of the `stock` cell's empty chrome trace. The fix `wss.rs` carries:
on ESRCH, if `/proc/<pid>/stat` exists and its state is not `Z`/`X`, read
once more with a fresh open; only a gone or zombie process is `TargetGone`
(`still_running` and `replaced_under_the_read` in
`src/collect/mem/wss.rs` are the shape; a shared helper in
`src/collect/task.rs` would serve both, and `numa.rs`'s and `thp.rs`'s
per-process reads if they map errors the same way). Keep the
`statm` zero-size rule as is. `PermissionDenied` straddling an exec into a
setuid program is a real end; one into a plain program should not occur.

### To the coordinator (`tests/cross_machine.rs`, nobody's): two tests make one cgroup name

`Cgroup::create` names `omn-cross-machine-{pid}.scope`, and
`a_memory_limit_turns_a_cached_read_into_device_reads_and_each_byte_carries_its_rung`
and `a_drained_reclaim_row_is_credited_whole_inside_one_second_of_pressure`
both call it before taking `alone()`. Name it per call (a static
`AtomicU32` counter, or the test's name) and treat `EEXIST` as "taken", not
as "not delegated" -- the error message now reads as a machine that cannot
serve, which it was not.

### To the owner of `tests/family_mem.rs`: the same TLB, through Python

`the_working_set_is_what_was_touched_and_not_what_was_held` (the
seventeenth wave's tally: 2 of 12 alone on the engine branch) walks 16 MiB
of a `bytearray`, which THP `always` may put on eight huge pages -- the
caveat's own example of a working set read as 4 KiB -- and which on small
pages is 4 096 of them, which the CI's cpu keeps half of. Walk at least
128 MiB, from an `mmap.mmap` advised `MADV_NOHUGEPAGE` (`mmap.madvise`,
Python 3.8+), as `wss.rs`'s test now does.

### To the coordinator: this machine's bpftool

`/usr/bin/bpftool` fails to start (*"libLLVM.so.23.1: cannot open shared
object file"*), so `btf.rs`'s cross-check skips in every local run. A
reinstall of `bpftool` (or of the LLVM it was built against) brings it back.
