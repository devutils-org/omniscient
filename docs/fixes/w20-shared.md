# The twentieth wave, stream shared

Branch `feat/w20-shared`, from `main` at `444900b`. `BRIEF.md`, "shared --
the shared layers and the files nobody owned (item 9)"; `STATE.md`, "What
remains", item 9. Measured on the development machine with the other streams
of the wave building and testing beside it.

## What landed

| Commit | What |
|---|---|
| `00d3597` | `fix(mem)`: `footprint.rs` reads `smaps_rollup`/`smaps` once more after an ESRCH from a process that is still running (an `execve` replaced the address space the open was bound to); only a gone or zombie process ends the run. The shape `wss.rs` already had. The `statm` zero-size rule and `PermissionDenied` as the end are unchanged. |
| `60691d2` | `test(cli)`: `scratch()` returns a guard that removes its directory when the test ends, passing or not, and sweeps the `omn-cli-<pid>-*` directories of test processes that are gone. The 154 MB `no-shim` copy no longer outlives its test. |
| `a5e078b` | `test(cross_machine)`: `Cgroup::create` names its cgroup `omn-cross-machine-<pid>-<n>.scope`, per call, and a name that exists is said as taken rather than read as a machine that does not delegate. |
| `4ac7844` | `feat(catalog)`: the loader refuses a backend declared `equivalent` that declares a cheaper band than a backend above it serving a target it serves (`check_equivalent_costs`), so `--catalog <dir>` is held to it, not only the embedded catalog. The embedded catalog loads. |
| `3c9b1b0` | `feat(cli)`: `omn join -f <rules> <results>` applies every rule of a session's `joins:`, a list of rules or one rule, to a stored run (`results.json` or its `-o` directory) through `join::apply_stored`, and prints the correlations as a run does (`--format text|json|yaml|toml`). A rule the stored run cannot answer refuses the command with exit 2 and the reason. `man/omn.1`: the command in the synopsis, under COMMANDS and in EXAMPLES, and what a rule's `tier:` means under Joins. |

`3c9b1b0` touches `src/main.rs` (stream budget's) by one dispatch arm,
`Command::Join(args) => ...`: a new `Command` variant does not compile
without it. The rest of the command lives in `src/cli.rs` (`omn::cli::join`).
At the merge, that arm is the only line of this branch in `main.rs`.

## Shown versus reasoned

Shown, on this machine:

- `footprint::tests::an_esrch_from_a_running_process_is_read_once_more_and_never_ends_the_run`:
  an ESRCH from this (running) process is read a second time and the second
  answer stands; a persisting one is `Transient` after exactly two reads; EIO
  is read once; a zombie child's ESRCH is `TargetGone` with no second read.
  The twelve tests of `collect::mem::footprint` pass.
- `cli::a_scratch_directory_does_not_outlive_its_test`: the directory is gone
  once its guard is, and a planted `omn-cli-4194305-leaked` (a pid above the
  kernel's maximum) is swept by the next one made. After `cargo test --test
  cli` (27 passed), `/tmp` holds no `omn-cli-*`.
- `tests/cross_machine.rs`: 6 passed, both cgroup tests ran (neither
  skipped) in one binary.
- `catalog::tests::an_equivalent_backend_cheaper_than_what_it_stands_in_for_is_refused`:
  `io.size`'s `bpf:tp` made `low` under a `high` `bpf:raw_tp` is refused
  with both bands named; as dear, dearer, degraded rather than equivalent,
  or on a disjoint target (head on `system`, the equivalent on every other)
  it loads. `tests/bench_bands.rs` still passes (8).
- `join_engine::a_rule_moved_to_the_query_tier_answers_from_the_stored_run_what_the_agent_did`,
  extended: `omn join -f session.yml <dir>` over the run's stored results
  answers both rules (agent and query tier) field for field what the agent
  tier did; a rule naming `io.latency`, which the run did not store, exits 2
  with "has no result in the stored run". Run by hand too: the text output is
  the run's join text.

Reasoned, not shown:

- That the footprint fix is what ended the `stock` CI cell's chrome trace
  (flaky tally entry 27): the cause was suspected, not shown, in the
  eighteenth wave, and stays so until a CI run reads green with it.
- A sweep deleting another live user's directory: it only removes
  `omn-cli-<pid>-*` whose `/proc/<pid>` is absent, and only in this user's
  `temp_dir`.

## Not done, and why

All three live in files this stream does not own; each is filed below.

- **`assemble` skipping a query-tier rule, and cloning only
  `join::sources_read`**: `src/main.rs`, stream budget's.
- **`NamedBy` kept in `Results::stacks`, and stack rows keyed by the
  request's keys**: `src/emit.rs`, stream view's.
- **The executable of each task a result keys**: `src/stack.rs` (budget) and
  `src/collect/sched/offcpu.rs` (sched).
- **`net_qdisc`'s namespace offset through `member_offset`**: in `STATE.md`'s
  item 9 but not in this stream's section; `src/bpf/net_*.rs` is stream
  net's.
- **Removing `cheaper_equivalents` from `tests/bench_bands.rs`**: the loader
  now holds the rule, so the test's copy is redundant; the file is nobody's
  this wave, so it is left as a second reader.

## The suite

One `make check` (this branch at `3c9b1b0`, the capabilities granted, load
average 15 to 20 with the other streams): `cargo fmt --check` and clippy
clean; the lib tests stopped it on `perf::tests::an_ibs_sample_says_which_level_answered_the_load`
(passes alone). `cargo test --workspace --no-fail-fast` then: 15 failures in
11 binaries, each rerun alone once. Fourteen pass alone (`cases`,
`cross_io`, `family_cpu`, `family_sched`, `join_engine`'s
`a_program_s_life_is_a_whole_its_three_states_account_for`, `runner`'s
`a_source_attached_for_one_phase_...`, two `scenario_gpu`, `scenario_net`,
`scenario_sched`, three `unwind`, the lib's `cpu::profile` machine test).
One fails again: `runner::a_profile_s_losses_land_on_the_series_of_the_task_whose_ring_overflowed`
(75102 against 73963; "the kernel throttled this profile 3800 times and its
rings account for 0 of them") -- the known red of `STATE.md` item 4, stream
rings'. None of the fifteen reads a file this branch changed except
`join_engine`'s, whose failing test is a decomposition this branch does not
touch and passes alone.

## Privileged runs wanted

None of this branch needs one beyond `make` (which granted the capabilities
here). The footprint fix is best read on the CI's `stock` cell:

```console
gh run watch <the run of the merged main>   # read encoders::a_known_allocation_lands_on_the_chrome_trace_at_the_time_it_was_made
```

## Decision lines

| Decision | Choice | Reason |
|---|---|---|
| Where "an equivalent declares no cheaper band than what it stands in for" is held | **In the catalog loader**, beside the other chain invariants. | A `--catalog <dir>` that broke it loaded; the test only read the embedded catalog. |
| Which rules `omn join` applies | **Every rule in the file, whatever its `tier:`**; a `kernel` one is refused by `apply_stored`'s own check. | Over a stored run every rule is at the query tier; the agent tier's answer over the same events is the same (shown). |
| A rule `omn join` cannot answer | **Refuses the whole command with exit 2 and the reason**, never prints the others and drops it. | An invalid combination is refused with the reason, never degraded silently. |
| An ESRCH from a procfs footprint read | **Read once more if the process is running; the end only for a gone or zombie process.** | The same rule as `wss.rs`; the ESRCH across `execve` is not the process ending. |

## Needs from another stream

### To stream budget (`src/main.rs`, `assemble`): skip a query-tier rule, and clone only what is read

The eighteenth wave's diff (`docs/fixes/w18-engine.md`, "To stream launcher")
still applies:

```diff
     let mut bounds = vec![0];
+    // Only the sources a join or a decomposition reads: every other
+    // collection's events are consumed by `aggregate` and never cloned.
+    let read = join::sources_read(&session.joins, &session.decompose);
@@
-        merged.extend(collection.events.iter().cloned());
+        if read.contains(resolved[index].request.source.as_str()) {
+            merged.extend(collection.events.iter().cloned());
+        }
         bounds.push(merged.len());
@@
-    for rule in &session.joins {
+    // A query-tier rule is applied to the stored run (`omn join`), not here.
+    for rule in session.joins.iter().filter(|rule| rule.tier == join::Tier::Agent) {
```

`join_engine`'s query-tier test holds whether or not the run also applies the
query-tier rule, so it survives this. Also: keep the `Command::Join` arm this
branch adds to `run`.

### To stream view (`src/emit.rs`): a frame's `NamedBy`, and stack rows keyed by the request's keys

`Results::stacks` is `BTreeMap<u64, Vec<String>>`: the label survives,
`stack::Frame::named_by` does not, so the triage reads
`/tmp/perf-<pid>.map` back after the run. Wanted: a per-result set of the
labels a perf map named (with the map's device and inode), or the frame's
`NamedBy` beside its label. And `keys: [task]` keys the summaries, not the
`stack` output: stack rows keyed by the request's keys, so a followed task's
stack is readable without asking `raw` beside it.

### To streams budget (`src/stack.rs`) and sched (`src/collect/sched/offcpu.rs`): the executable of each task

No result carries the executable of each task it keys (`comm` is cut at 15
bytes, `/proc/<pid>/exe` is gone after the run), so a followed child's own
frames are reached only when its executable is the launched one.

### To the coordinator (`tests/bench_bands.rs`)

`cheaper_equivalents` there can go, or stay as a second reader of the
embedded catalog; the loader's test now holds the rule.
