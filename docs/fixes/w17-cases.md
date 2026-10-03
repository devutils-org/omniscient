# Stream cases, seventeenth wave: a case is data, asked both ways

ROADMAP phase 12, tasks 1 to 3, and the pilot: case 1, the distribution's
stripped `sqlite3`. Everything here is in `cases/**`, `tests/cases.rs` and
`docs/cases.md`; nothing under `src/` changed.

---

## 1. What landed

- **`cases/<name>/case.yml`**, the format (`docs/cases.md` describes every
  field): the program and how it was obtained (`stripped: true` is checked
  against the file's section headers, or the case skips); `needs`, commands
  that must exit 0, each with what it is for; `symbols`; `fetch` (url, sha256,
  file), into `$XDG_CACHE_HOME/omniscient/cases/<name>/`, never the tree;
  `setup`; and per cause: `on` (cpu or off-cpu), `question` and `objective`,
  the `injection` (kind, what it is, and for a `process` the command and the
  line it prints once the cause is in place), the `target`, `own_account`, the
  `reference` (each a named extractor reading the measured run's own account,
  the injecting process's output, or a command of its own), `holds` (when the
  reference says the cause was there to be found), and `expect` (`limit`,
  `state`, `leaf`, `through`, `never_says`, and `agrees`: a row against a
  reference figure with each slack named). `open_findings` lists the checks
  the case shows failing, each with where it was filed.
- **`tests/cases.rs`**: discovers every `cases/*/case.yml` and asks every
  cause both ways.
  - **The person's way**: `omn latency --objective <o> --format json --
    <target>`, read from the verdict (`limit`, `state`, the `named` hottest
    function or block stack, the rows and their resolutions, the notes).
  - **The agent's way**: the same question's session (`omn latency
    --dump-session`, written as JSON) in a directory passed to `omn-mcp
    --allow-launch` -- the person approving it -- then `list_approved` (a
    session this machine cannot serve skips here with `omn`'s reason, before
    anything runs), `run_session`, and the read-back read the way
    `docs/mcp.md` tells a model to: the biggest row of the decomposition, and
    the dominant stack of the request that row came from (by its leaf's own
    samples for `cpu.profile`, by its total for a block).
  - Nine named checks per answer (`docs/cases.md`, "What a case checks"). A
    check listed under `open_findings` prints `FINDING` with where it was
    filed while it fails, and **fails the test once it passes** everywhere it
    was checked, so a fix elsewhere removes the line and a regression of a
    closed finding is a failure.
  - The extractor registry (`fn extractor`): `sqlite3-timer`,
    `sqlite3-stats`, `sqlite3-plan`, `key-value`, each with a unit test on text
    the program prints (`the_extractors_read_what_the_programs_print`).
  - `every_case_is_well_formed`, machine independent: every case parses (deny
    unknown fields), is named after its directory, has a cause on the cpu and
    one off it, names existing extractors and checks, and refers only to
    references it declares.
- **Case 1, `cases/sqlite3-stripped/`**: on the cpu, `SELECT count(*) FROM
  probe p CROSS JOIN t ON t.k + 0 = p.k` on 200 000 rows with `t.k` indexed --
  the plan scans `t` once per probe, 40 M fullscan steps, where `t.k = p.k`
  searches the index (the `indexed` reference holds that); off it,
  `cases/sqlite3-stripped/holder.py` holds `BEGIN EXCLUSIVE` from before the
  target starts until 1.5 s after it sees the target appear in `/proc`, and the
  target waits in its busy handler (`.timeout 20000`). Reference: `.timer`,
  `.stats`, `EXPLAIN QUERY PLAN`, the holder's own `CLOCK_MONOTONIC`.
- **`docs/cases.md`**: the format, the checks, the extractors, the six
  injections with their references, the measured numbers, how to add a case.

## 2. Shown, and reasoned

**Shown** on the development machine (24 cpus, kernel 7.2.7,
`perf_event_paranoid` -1 as the coordinator left it, `omn` with
`cap_bpf,cap_perfmon` from `make`, five other agents running), every check of
both causes both ways, `cargo test --test cases` (3 passed, 13 s):

```
a-plan-that-scans/cli:     on-cpu 1456.4 ms; .timer cpu 1440.3 ms of real 1452.9 ms; 39 999 999 fullscan steps; plan 2 scans 0 searches, indexed 1 search
a-plan-that-scans/mcp:     on-cpu 1417.3 ms; .timer cpu 1402.0 ms of real 1413.2 ms; hottest sqlite3VdbeExec.lto_priv.0 (debuginfod)
a-lock-held-elsewhere/cli: blocked 1530.2 ms (limit sleep, leaf clock_nanosleep); holder 1500.1 ms after it saw the target; .timer off-cpu 1530.3 ms
a-lock-held-elsewhere/mcp: blocked 1529.2 ms (leaf clock_nanosleep); holder 1500.1 ms; .timer off-cpu 1529.4 ms
```

- Both ways name the known cause first, and the same suspect; the leaves are
  compared where both are named (the lock: `clock_nanosleep` both ways; the
  scan: the person's leaf is an address, finding 1).
- Every cited row agrees with its reference within the stated slack: the
  on-cpu row runs about 1 % over `.timer`'s cpu (a time-based sample counts
  the interrupts on the task's cpu, which getrusage leaves out -- the
  profile's own caveat), and the blocked row 29 ms over the holder's clock
  (sqlite's busy handler sleeps up to 100 ms between its last retries).
- The agent's way names a stripped binary's functions from debuginfod and
  says where each name came from (*"names came from -- sqlite3: debuginfod,
  https://debuginfod.archlinux.org, build id 88b37b84..."*). The profile with
  names: `sqlite3VdbeExec` 947 of 1444 samples, `getCellInfo` 121,
  `sqlite3BtreeNext` 105, `sqlite3VdbeSerialGet` 83 -- the scan's loop.
- Each address the person's way could not name, resolved by hand against the
  debug file debuginfod served (`addr2line -f -e
  ~/.cache/debuginfod_client/88b37b84.../debuginfo`, file offset equals
  address in this binary): the hottest `sqlite3+0x90183` is inside
  `sqlite3VdbeExec`; the block's `sqlite3+0x57e9a` is `unixSleep`,
  `+0x120142` `sqliteDefaultBusyCallback`, `+0x70880`
  `btreeInvokeBusyHandler`.

**Reasoned, not run**:

- **At `perf_event_paranoid` 2** (CI's `defaults` cell). With the
  capabilities, nothing the case uses changes. Without them, `omn latency`
  exits 1 with *"no viable backend"* for `sched.runqlat`/`sched.offcpu`, and
  each cause skips both ways with *"SKIPPED case sqlite3-stripped/<cause>/<way>:
  this machine withholds a source it needs: error: ..."*, the `error:` line
  first and the rest indented (`withheld`), which the skip gate's `CAP_BPF`
  pattern matches. I did not change the sysctl; it is in the privileged list.
- **On the CI runner**: `sqlite3` and python3's `sqlite3` module must be there
  (installed by the workflow; a missing workload is not an expected skip).
  Where `DEBUGINFOD_URLS` is unset, the naming checks print `unchecked` with
  that reason; where it is set and the server does not carry the build id,
  the agent's leaf is an address, compared with nothing.

## 3. Findings the case shows, filed (each is an `open_findings` line)

### "the triage cannot ask debuginfod" -- `src/cli.rs` (`LatencyArgs`), `src/triage/latency.rs:162`, `src/triage/saturate.rs:139`

`omn latency` and `omn saturate` build their requests with `symbols: None`
and take no `--symbols`, so a stripped binary's frames are `module+offset`.
Worse than unnamed: the hottest function is ranked by leaf frame, so an
unnamed binary is ranked **per instruction**. The same run named:
`sqlite3VdbeExec` 65 % of the samples; unnamed: *"hottest function
`sqlite3+0x70564`, 70 of 1378 samples (5 %)"*. Ask: a `--symbols` on both
commands, set on the `cpu.profile` request (`on_cpu.symbols`), and the
`--dump-session` carrying it. Check: `the-person-s-way-names-the-hottest-function`
-- `tests/cases.rs` passes `--symbols` itself as soon as `omn latency --help`
lists it. No stream owns `src/triage/` this wave: **for the coordinator**.

### "the verdict drops where its names came from" -- `src/triage/latency.rs:1128` (`notes`)

`notes(run)` copies each result's losses and degradations and not its
`notes`, where the profile says *"names came from -- sqlite3: no names: it
carries no function symbols; libc.so.6: its dynamic symbols only ..."*. The
verdict therefore says how it unwound and never where its names came from.
Ask: copy `results.notes` (at least the `names came from` one) under the
request's label. Check: `the-person-s-way-says-where-names-came-from`. **For
the coordinator.**

### "an off-cpu stack cannot ask debuginfod" -- `src/stack.rs:1409` (`UNWOUND_BY_CHOICE`, `reads_symbols`)

`--symbols` is read by `cpu.profile` alone, and any other source is refused
with exit 3, so the block's stack is `clock_nanosleep <- sqlite3+0x57e9a <-
sqlite3+0x120142 <- sqlite3+0x70880`: the busy handler that names the cause
(`sqliteDefaultBusyCallback`, resolved by hand above) is there and unnamed.
Ask: `sched.offcpu`'s user stacks named through the same `Symbolizer` with a
debuginfod client. Check: `the-block-s-frames-are-named`. **For stream
symbols** (it owns `src/stack.rs`), with the collector side
(`src/collect/sched/offcpu*`) for the coordinator.

### "a polled lock is called not a lock" -- `src/triage/latency.rs:269`

`Limit::Sleep`'s sentence says the task was *"asleep on something that is not
a lock: a timer, a pipe or a socket"*. sqlite's busy handler waits for a file
lock by sleeping and retrying, as many lock implementations do (spin-then-sleep,
`flock` with `LOCK_NB` and a sleep): the claim is confidently wrong for this
case's cause. The task state cannot tell a polled lock from a timer, so the
sentence should not claim what it cannot see: *"asleep, not waiting in the
kernel on a lock: a timer, a pipe, a socket, or a lock polled by sleeping"*,
and the stack beside it is what tells them apart. Check:
`the-sentence-claims-nothing-false`. **For the coordinator.**

### "a waiting program exceeds every budget" -- `mcp/src/server.rs:427`, budget semantics in `src/main.rs`

`run_session` requires a `budget`, a ceiling on the collectors' cpu as a share
of the **target's** cpu. The lock case's target burns 0.6 ms in 1.5 s; the
five collectors spend 6 to 11 ms each, so at `--budget 100%` the run stopped
at 254.7 ms (*"budget: [cpu.profile] --budget 100% was exceeded: 146.32 %
measured over the whole window"*), the target killed before its wait was over:
the agent's way answered another question than the person's, which asks with
no budget. `tests/cases.rs` passes `100000%` (`UNBOUNDED_BUDGET`, with this
reason). Ask: either `budget` optional in `run_session` (bounded by the
server's `--allow-run` duration), or a budget whose denominator is the wall
clock for a target that waits. **For the coordinator.**

### Not filed as a check: the agent's way cannot read the program's stdout

Under `run_session`, `said_by_omn` carries `omn`'s stderr, which the program
inherits; its stdout is in neither stream (a `.timer` line printed on stdout
appeared in neither `omn -f ... -o DIR`'s stdout nor its stderr). A program's
own account is very often on stdout. The case moves it with `sh -c 'exec "$@"
1>&2' sh <target>` (`own_account: stdout`), which keeps the pid and changes
nothing in the program; documented in `docs/cases.md`. Ask (`mcp/src/session.rs`,
or the launch in `-o` mode): hand the program's stdout back as its own field.
**For the coordinator**, low priority.

## 4. What I could not do, and why

- **Run at `perf_event_paranoid` 2**: a machine-wide sysctl the other streams'
  tests read; listed below.
- **`saturate` both ways**: `question` is `latency` only (`every_case_is_well_formed`
  refuses another). The agent's reading of a USE answer from `run_session` is
  not the decomposition's biggest row, and no case needs it yet; stream build's
  `make -j` may. Adding it is a second reader in `tests/cases.rs`.
- **The `tail` objective**: needs `--runs`, which `latency_args` does not pass
  yet; no case asks it.

## 5. Tests

- While iterating: `cargo test --test cases` only.
- Before the last commit, one `make check`: red, in `tests/budget.rs`
  (`the_extra_cpu_a_faster_poll_costs_is_the_extra_cpu_the_shell_observes`,
  `:300`) and before it `tests/recorder.rs`
  (`two_bursts_write_exactly_two_documents_each_holding_its_burst`, `:398`).
  Then one `cargo test --workspace --no-fail-fast` (7 min 47 s): `cases` 3/3
  ok; red, all outside this stream's files (this branch changes nothing under
  `src/`): `budget` (the poll test, `a_preempted_hook_call_is_counted_apart_and_adds_no_time_slice`),
  `scenario_io` (`a_call_whose_exit_the_kernel_skipped_is_counted_as_lost`,
  `a_pipe_wait_is_in_the_syscall_layer_and_not_in_the_file_layer`),
  `scenario_proc` (`the_only_key_proc_fd_declares_is_the_only_one_its_events_carry`),
  `scenario_sched` (`sharing_one_file_is_the_only_difference_between_these_two_runs`,
  `a_blocking_stack_names_the_call_that_blocked`), `triage`
  (`a_join_from_a_held_source_counts_what_fell_outside_its_window`,
  `a_verdict_read_off_a_lossy_profile_says_its_amount_is_a_lower_bound`),
  and `recorder`'s burst test. Each rerun alone once: all passed except
  `scenario_sched::sharing_one_file...` and `triage::a_verdict_read_off_a_lossy_profile...`,
  which failed again alone and then passed on a further run (the latter
  lost 65 % of a 100 kHz profile with five other agents running): load, not
  this branch.

## 6. Privileged runs wanted (the coordinator, alone)

```console
cd /home/moha/repos/omniscient-w17-cases && make
# the CI's defaults cell, with the capabilities: every check, no skip expected
sudo -n /usr/bin/sysctl kernel.perf_event_paranoid=2
cargo test --test cases -- --nocapture 2>&1 | grep -E '^case |^  |SKIPPED|test result'
# and without them: every cause skips both ways with the machine's reason first
touch src/main.rs && cargo build --workspace && getcap target/debug/omn   # (empty)
cargo test --test cases -- --nocapture 2>&1 | grep -E 'SKIPPED|test result'
.github/scripts/skip-gate.sh <that log> .github/expected-skips.txt
sudo -n /usr/bin/sysctl kernel.perf_event_paranoid=-1 && make
```

## 7. Decision lines for `DECISIONS.md`

| Question | Decision | Why |
|---|---|---|
| What a case is | **`cases/<name>/case.yml`, data; extractors a registry in `tests/cases.rs` keyed by name.** A program with an account of a known shape is a directory; one of a new shape adds one extractor beside the tests, never a line under `src/`. | ROADMAP phase 12: adding a program means adding a directory. The account's parser belongs with the reference, not the tool. |
| How the agent asks | **The question's own session (`--dump-session`), approved into `--allow-launch`, run through `run_session`, read as `docs/mcp.md` says: the decomposition's biggest row, then the dominant stack of that row's source.** | The two ways must ask the same question; the server summarises nothing, so the reading is the model's and is written down. |
| What "the same suspect" means | **The same dominant state, and the same leaf where both are named.** An address is never compared with a name. | A name against an address is a difference in symbols, not in the suspect; it is checked separately. |
| A check a case shows failing | **Listed under `open_findings` with where it was filed; reported, not failed, while it fails; a failure once it passes.** | "No workaround in the case": the finding is visible each run, and its fix is noticed by the case that showed it. |
| A cause the reference says was not there | **A skip with the numbers.** | That run asked another question; asserting on it would test the machine's load. |
| A program's account on stdout | **Handed to its stderr by `sh -c 'exec "$@" 1>&2'`, pid unchanged.** | Both ways read stderr; the program is unchanged; filed for `run_session` to carry stdout. |

## 8. Needs from another stream

- **symbols** (`src/stack.rs`): an off-cpu stack named through debuginfod
  ("an off-cpu stack cannot ask debuginfod"). When it lands, remove
  `the-block-s-frames-are-named` from `cases/sqlite3-stripped/case.yml`.
- **coordinator** (`src/cli.rs`, `src/triage/latency.rs`,
  `src/triage/saturate.rs`): `--symbols` on the triage; the names note in the
  verdict; the `Limit::Sleep` sentence. Each, when it lands, removes its line
  from the case's `open_findings` (the test fails until it is removed).
- **coordinator** (`mcp/src/server.rs`, `mcp/src/session.rs`): the budget
  for a waiting target; the program's stdout in `run_session`'s answer.
- **coordinator** (`.github/workflows/ci.yml`): install `sqlite3` (and
  python3's `sqlite3` module, normally present) on the runner, and run
  `cargo test --test cases` in the cell.
- **python, build**: the format and the extractors are in place; a case is a
  directory, and a new account is one extractor in `tests/cases.rs` with its
  unit test (both streams own `cases/<theirs>` and will need to add to
  `tests/cases.rs`'s registry, which this stream owns: send me, or the
  coordinator at the merge, the function). `fetch` is implemented for build's
  sqlite amalgamation (`curl`, sha256-checked, into `{cache}`).
