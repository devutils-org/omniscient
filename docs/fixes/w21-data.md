# The twenty-first wave, stream data

Branch `feat/w21-data`, from `main` at `156f1c5` (`git merge main`: already
up to date). `BRIEF.md`, "data -- what results carry"; `STATE.md`, "What
remains", item 4 (the `src/collect/mod.rs` and `src/emit.rs` line, the
`src/catalog.rs` line, the `src/stack.rs` line) and the shared stream's
"not done" (`NamedBy`, the executable of each task). Measured on the
development machine with the other streams of the wave building beside it.

## What landed

| Commit | What |
|---|---|
| `2439ba8` | `feat(data)`: **`KeyKind::Dir`** (`dir`) and `KeyValue::Dir { dir }` over a closed `event::Direction` (`read`, `write`, `discard`, `flush`, `rx`, `tx`), printed `dir:read` and so on. A counter of both directions keyed on it is one series per direction under every aggregating emit. No collector produces it yet. |
| `ec2b229` | `feat(data)`: **`Collector::fields()`** (default empty) → `Collection::fields` → **`Results::fields`** (`emit::Fields`, a `BTreeMap<String, u64>`), serialized only when not empty; read at the end of a run, at a detach (kept in `Detached`) and in each recorded document. **Stack rows keyed by the request's keys**: under `--keys`, `stack` gives one row per series and stack, `StackCount::series` naming the series (absent without keys, so no reader moves); `folded`, `speedscope`, `pprof` and `text` root the chain at the series, `ndjson` writes `"series"`. |
| `1b7773c` | `feat(catalog)`: **`targets: {pid: "no: <reason>"}`**. The loader keeps the `no` in the support map and moves the reason into `Entry::refused`; `check_catalog` refuses a bare `no` (a refusal with nothing to quote) and a reason on a target that is not refused. `Entry::refusal(kind)` follows the fallbacks (a `tid` is refused for its `pid`'s reason). `validate.rs`'s `target_rejection_reason` quotes it first, before the uncore/iface/cpu answers; **the caveat search is gone**. `gpu.kernel`, `gpu.api`, `gpu.counters` and `gpu.profile` carry their `pid` refusal on the line (`catalog/gpu.yaml`). |
| `bcb539f` | `feat(data)`: **`stack::Provenance`** -- `named_by` (each kept stack's frames' `NamedBy`, root first, frame for frame with `Results::stacks`, `null` for an unnamed frame), `perf_maps` (path, device and inode of each perf map that named a frame) and `executables` (the file each keyed task executed, by its key's spelling) -- from `Collector::provenance()` (default empty) through `Collection` into `Results`, flattened beside `stacks`, each only when not empty. `Symbolizer::executable()` (read at creation and at each `refresh`, since an `execve` changes it), `Symbolizer::perf_map_file()`, `stack::executable_of(pid)`, `Stack::labels_root_first()` and `Stack::named_by_root_first()`. **No collector fills it yet**: see "Needs from another stream". |
| this | the handoff |

**The gpu stream's out-of-row lines, read and kept.** `validate.rs`'s
`check_stack` skipping `collect::gpu::DEVICE_STACKS` is right (a device
stack unwinds nothing on the host); it is a list of source ids in code, so
it would read better as catalog data (an entry-level `stacks: device`), but
that is a catalog field for one family and not worth a schema change this
wave. `resolve.rs`'s `collect::gpu::with_clock_states` call in `build` is
one call per resolved request, NVML opened once a run behind a `OnceLock`,
and a non-`gpu` family returns the environment untouched; kept.
`capabilities.rs`'s `Environment::gpu_clocks` (empty and unserialized
otherwise) kept.

**Outside this row**, each because the item could not be done otherwise:

- `src/collect/mod.rs` (stream run's): the two trait methods
  (`fields`, `provenance`), the two `Collection` fields, the two `Detached`
  fields and two helpers (`slot_fields`, `slot_provenance`), +40 lines. Run
  adds `Collector::failure()` to the same trait this wave: expect a textual
  conflict beside `row_stamp`, nothing semantic.
- `src/event.rs` (nobody's): `KeyValue::Dir` and `Direction`.
- `catalog/gpu.yaml` (nobody's this wave): four `targets:` lines and one
  comment, the move the brief asks for.
- `src/triage/diff.rs`: a difference compares keyed stack rows by their
  frames alone (see "The tests").
- `src/triage/latency.rs`, `tests/encoders.rs`: `fields`, `provenance` and
  `series` added to test literals of `Results` and `StackCount`, one line
  each.

## Shown versus reasoned

Shown, on this machine:

- `request::tests::a_direction_is_a_key_spelled_dir`: `dir` parses to
  `KeyKind::Dir`, every `Direction` prints `dir:<name>`, kinds itself `Dir`
  and round-trips through JSON.
- `emit::tests::a_result_carries_the_fields_its_collector_counted`: a
  collection's `fields` reach `Results::fields` and `results.json` by name
  (`fields.never_ended == 3`), read back equal; a collection that counted
  nothing writes no `fields` key.
- `emit::tests::stack_rows_are_keyed_by_the_requests_keys`: four events on
  two threads and two stacks give, under `--keys task`, three rows
  (`task:8` stack 1 = 30, `task:7` stack 1 = 15, `task:8` stack 2 = 1);
  without keys two rows (stack 1 = 45) and no `series` in the JSON.
- `encode::tests::a_keyed_stack_row_is_rooted_at_its_series`: `folded`
  writes `task:7;main;work;read 3000000`; `ndjson` writes
  `"series":"task:7"`.
- `catalog::tests::a_refused_target_carries_its_reason_on_the_targets_line`:
  `pid: "no: <reason>"` is `No` with the reason, a `tid` gets the pid's
  reason, `launch` and `system` none; a bare `"no"` is refused by the loader
  ("`pid: no` without a reason"); every entry-level `no` in the embedded
  catalog has a reason; `gpu.kernel`'s names `CUDA_INJECTION64_PATH`.
- `omn trace gpu.kernel --pid 1 -d 1s`: `error: ... cannot be measured per
  pid: a launched program only -- CUPTI works inside the process that owns
  the CUDA context ...; refused for that reason, not for want of a backend`,
  hint `this source accepts launch`. `tests/gpu_kernel.rs` (11 passed) and
  `tests/catalog_model.rs` (19 passed) hold.
- `stack::tests::a_stacks_namers_read_root_first_beside_its_labels`,
  `a_symbolizer_knows_what_its_process_executes` (this test binary's own
  path through `executable_of(0)`, `executable_of(pid)` and a
  `Symbolizer`; none for a pid that does not exist), and the perf-map test
  now asserting `perf_map_file()` with the file's device and inode.
- `emit::tests::a_result_carries_what_its_stacks_labels_cannot_say`:
  `executables` reach `results.json` flattened; with no `stack` emit,
  `named_by` and `perf_maps` are dropped; read back equal; an empty
  provenance writes nothing.

Reasoned, not shown:

- A real `cpu.profile` or `sched.offcpu` run carrying `named_by`,
  `perf_maps` and `executables`: no collector implements
  `Collector::provenance()` yet.
- A real `net.request` run carrying `fields`: its collector does not
  implement `Collector::fields()` yet (collectors' row).
- `io.bytes`/`net.bytes` one series per direction: no collector writes a
  `dir` key yet, and no catalog entry declares `dir` in `keys:`.

## What I could not do, and why

- **Filling `Collector::provenance()` in `src/collect/cpu/profile.rs`**
  (collectors' row, starting once this merges) and
  **`src/collect/sched/offcpu.rs`** (not in this row): filed below with the
  lines. The plumbing and the `stack.rs` half are done, so each is one
  method.
- **`DEVICE_STACKS` as catalog data**: reasoned above, left.

## Privileged runs wanted

None of this stream's tests needs a capability. Once collectors fill
`provenance()`, after `make`:

```console
target/debug/omn trace cpu.profile -m sample@99hz -e stack --keys task -d 2s \
  -o /tmp/w21-data-prof -- python3 -c 'import sys; sys.activate_stack_trampoline("perf"); [sum(range(10**5)) for _ in range(2000)]'
jq '.results[0] | {executables, perf_maps, named: (.named_by | length)}' /tmp/w21-data-prof/results.json
```

Wanted: `executables` naming `/usr/bin/python3.x` for the program's task,
`perf_maps` naming `/tmp/perf-<pid>.map` with a non-zero inode, and stack
rows carrying `"series": "task:<tid>"`.

## Decision lines for DECISIONS.md

- **A collector's fields are counts** (`Results::fields`, a
  `BTreeMap<String, u64>`): what a caveat says in a sentence as a whole
  number by name; absent when empty. A field that is not a count would need
  another type, and none was asked.
- **Direction is a closed set** (`read`, `write`, `discard`, `flush`, `rx`,
  `tx`), as every other key: a new direction is a code change, so a typo in
  a collector cannot invent a series.
- **A stack row names its series only under `--keys`**: without keys the
  table is the one every reader has always read; with keys a flame is rooted
  at the series, one tower per series.
- **A refused target's reason lives on its `targets:` line**, and an
  entry-level bare `no` is a loader error: an absent kind already means
  `no`, so a written one exists only to say why. The caveat search is gone.
  A backend-level `no` (`sched.yaml`, `launch: "no"`) still needs no reason:
  it narrows a chain, it is not what a refusal quotes.
- **What stack labels cannot say travels beside them, not inside them**:
  `Results::stacks` stays `id -> [label]`, and `named_by` is a parallel
  table, `perf_maps` and `executables` beside it, so no reader of `stacks`
  moves.

## Needs from another stream

- **collectors** (`src/collect/cpu/profile.rs`): `Collector::provenance()`,
  beside `stacks()` and over the same `stack_origins`:
  `named_by.insert(run_id, stack.named_by_root_first())` for each stack it
  exports; `perf_maps` from each unwinder's `Symbolizer::perf_map_file()`;
  `executables` from each unwinder's `Symbolizer::executable()`, under the
  key spelling the profile's events carry (`task:<tid>` for each tid seen
  of that pid). `stacks()` could use `Stack::labels_root_first()`.
- **collectors** (`src/collect/io/diskstats.rs`, `src/collect/net/`,
  `catalog/{io,net}.yaml`): produce `KeyValue::dir(Direction::Read|Write)`
  (and `Discard`, `Flush` where diskstats has them) on `io.bytes`,
  `io.iops`, `io.merges`, and `Rx`/`Tx` on `net.bytes`, one event per
  direction per poll, and declare `dir` in `keys:`.
- **collectors** (`src/collect/net/request.rs`): `Collector::fields()` with
  `never_ended`, `unborn`, `replaced`, `filtered_out`.
- **sched owner / coordinator** (`src/collect/sched/offcpu.rs`): the same
  `provenance()` from its `Symbolizer`s; it holds `stacks` already
  interned as labels, so `named_by` needs the `Stack` at interning time.
- **views** (`view/src/model.rs`): a `Key::Dir { dir }` variant (today a
  `dir` key falls to `Key::Other` and prints as JSON); `Results::fields`,
  `executables`, `named_by`, `perf_maps`, and `StackCount::series` in the
  view's model, so the request view reads `fields` in place of sentences
  and the flame can split by series.
- **docs** (`CATALOG.md`; `docs/contributing.md` is nobody's): the
  `targets: {pid: "no: <reason>"}` form where the `targets:` line is
  described, and `dir` among the keys.
- **triage** (`src/triage/`): read `named_by` and `perf_maps` from the
  results instead of reading `/tmp/perf-<pid>.map` back after the run, once
  a collector fills them.
- **coordinator** (`DECISIONS.md`): the lines above; the nineteenth wave's
  "a refused target's reason comes from a caveat" row is now superseded.

## The tests

- While iterating: `cargo test --lib -- emit:: encode:: request::tests`
  (123 passed), `catalog::tests validate::tests` (35 passed),
  `stack::tests` (23 passed, 1 ignored), `emit::tests` (39 passed);
  `cargo test --test catalog_model --test gpu_kernel` (19 and 11 passed).
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
  `cargo fmt --check`: clean.
- `make check`, once: red on one test, `tests/triage.rs`'s
  `the_function_whose_cost_doubled_is_the_only_difference`, **a defect of
  this branch**: its profiles are taken with `-k task`, so their stack rows
  became per task and `omn diff` compared two runs' different tids as
  different stacks. Fixed in `src/triage/diff.rs` (outside this row, eight
  lines): a difference drops the series and sums a stack's rows back into
  one before folding. The test passes alone after `make`.
- `cargo test --workspace --no-fail-fast` after the fix: 63 test binaries
  green, one lib failure, `collect::tests::the_runner_closes_the_window`
  ("a closing read in `stop` would cover 6120228 ns of a 20000000 ns tick"),
  a timing assertion under the machine's load; it passes alone, rerun once.
  Filed for the run stream, whose file it is, should it recur.
- **For views**: the flame reading a `-k task` profile now gets one row per
  task and stack; until the view's model reads `series`, rows of one stack
  id repeat (see "Needs from another stream").
