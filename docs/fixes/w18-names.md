# The eighteenth wave, stream names: what the verdict names

`STATE.md` item 3, its last two bullets: `--symbols` on `omn latency` and
`omn saturate`, the verdict's names note, the hottest function under a perf
map, a flat interpreter profile, and a block's stack printed to the program's
own frame (`docs/fixes/w17-python.md` findings 1 to 3,
`docs/fixes/w17-cases.md` "the triage cannot ask debuginfod" and "the verdict
drops where its names came from").

**Read this first.** The functions that print the names -- `hottest`,
`blocking_stack` and `notes` -- are in `src/triage/latency.rs`, which stream
triage owns this wave, and the two `Ask` literals are in `src/main.rs`, which
stream launcher owns. So the rule lives in a file this stream owns
(`src/triage/diff.rs`), and the calls into it are a patch for the merge:
**`docs/fixes/w18-names-glue.patch`** (`src/main.rs`, `src/triage/latency.rs`,
`src/triage/saturate.rs`; 202 lines, `git apply --check` clean against this
branch). `saturate.rs` is this stream's file, but its new `Ask` field does not
compile without `main.rs`'s line, so it travels with the patch. **Until the
patch is applied, `--symbols` is parsed by both commands and read by
nothing**: apply it in the same merge as this branch, after triage's and
launcher's, re-fitting the latency hunks to triage's rewrite where they
conflict (the hunks are calls, not logic).

## 1. What landed

Commits `f7cc7d0`, `7bd8b69`, `6745273`, and this handoff.

- **`--symbols` on both questions** (`src/cli.rs`): `LatencyArgs::symbols`
  and `SaturateArgs::symbols`, the same `SymbolSource` as `omn trace`'s, with
  a parser test (`both_questions_take_symbols`). With the patch, each mode's
  `session()` calls `diff::ask_symbols`, which sets it on every request whose
  collector reads it (`stack::reads_symbols`: `cpu.profile` today, and
  `sched.offcpu` the day stream sched adds it to `UNWOUND_BY_CHOICE`) and on
  no other, so `--dump-session` carries it and a source that would refuse
  `debuginfod` is never handed it. `omn saturate` asks no stack, so with
  `--symbols` its answer says nothing read it and points at `omn latency`.
- **One rule for what a sample is charged to** (`src/triage/diff.rs`,
  `charged_to`): the leaf, or, where a perf map named a frame of the stack,
  the innermost such frame. Results keep labels, not provenance, so which
  frames the map named is read back from the maps the run's names note cites
  (`PerfMapNames::of_run`: `names came from -- ...; perf-4242.map: perf-map,
  ...` -> `/tmp/perf-4242.map`, every line's symbol). A cited map that is
  gone is said (`PerfMapNames::said`), and its samples fall back to their
  leaf. The maps are taken from every request's notes, because
  `cpu.profile`'s unwinder writes the note and `sched.offcpu`'s stacks pass
  through the same map's frames.
- **The hottest function** (`diff::hottest`): ranked by `charged_to`; under a
  perf map the sentence keeps its `hottest function `NAME`, N of M samples`
  head and adds that it was charged to the innermost perf-map frame and how
  many samples had one.
- **A flat interpreter profile said as one**: when the program is an
  interpreter -- by its name or a frame's module, from the list
  `diff::INTERPRETERS` (python, node, java/jvm, ruby, perl, php, lua; a
  version or suffix may follow, `node_exporter` is not node) -- no frame was
  charged through a perf map, and no function holds half the samples
  (`diff::FLAT`), the sentence says the time is spread over the interpreter's
  own functions, lists the top three with shares, says none is the suspect,
  and names what writes a perf map for that runtime. No backquotes in it: the
  cases read a verdict's first backquoted name as its suspect. An
  interpreter's single dominant C function (a C extension doing the work) is
  still named.
- **A block's stack to the program's own frame** (`diff::leafward`): leaf
  first, at least 4 frames, on to the innermost frame a perf map named or one
  in the launched executable (`sqlite3+0x70880`), at most 16.
- **The names note** (`diff::names_notes`): every result's `names came
  from` line, under its request's label (`[on-cpu] names came from -- ...`);
  with the patch, `latency::notes` appends them, so `omn saturate`'s notes
  carry them too (it names no stack, so it has none).
- **`omn diff`** charges its own cost by the same rule: a profile read from a
  run carries the perf maps its note cites, and the own table's title says
  when it charged through one. A folded file carries no note and is ranked by
  leaf, as before.
- **`docs/mcp.md`**: "Naming what dominates: the function, and how its name
  is known" -- the four readings for a model, in the walk-through.
  **`man/omn.1`**: `--symbols` under both commands, a "what a verdict names"
  paragraph under Verdicts, `omn diff`'s own cost.

## 2. Shown, and reasoned

**Shown** on this machine (kernel 7.2.7, `omn` with `cap_bpf,cap_perfmon`
from `make`, nine other streams running, load average about 20), **with the
glue patch applied**, then reverted:

```
omn latency --objective latency -- python3 -X perf cases/cpython-interpreter/hot.py input.txt
  hottest function `py::score:/.../hot.py`, 1146 of 1162 samples (99 %), charged to the
  innermost frame the program's perf map named (1151 of the 1162 samples had one; ...)
  note: [on-cpu] names came from -- ...; perf-149987.map: perf-map, the 176 lines ...
omn latency --objective latency -- python3 cases/cpython-interpreter/hot.py input.txt
  the time is spread over the python interpreter's own functions, none on half the samples
  (__tls_get_addr 11 %, _Py_Dealloc 7 %, PyLong_FromLong 4 % of 1157): no frame names the
  program's functions, so none of these is its suspect. A perf map would name them: ...
omn latency --objective latency -- python3 -X perf cases/cpython-interpreter/reader.py fifo 3000000
  leaf first: __read <- libpython3.14.so.1.0+0x2ebff6 <- +0x3335ca <- +0x5dfc1
              <- py::wait_for_input:/.../reader.py
omn latency --objective latency --symbols debuginfod --dump-session -- true
  symbols: debuginfod on `on-cpu` only
omn saturate --objective throughput --symbols debuginfod -- sleep 0.2
  note: `--symbols debuginfod` names stacks, and no request of `omn saturate` reads one ...
```

The w17 numbers were `__tls_get_addr`, 100 of 1120 samples (9 %), for the
first, and four frames with no Python function for the third.

`cargo test --test cases every_case_names_its_known_cause_both_ways` with
the patch applied (282 s; `DEBUGINFOD_URLS` set here): these `open_findings`
lines now **pass everywhere they were checked**, so the test asks for their
removal (stream cases' files):

- `cpython-interpreter`: `the-sentence-claims-nothing-false`
- `cpython-perf-map`: `the-block-s-frames-are-named`,
  `the-person-s-way-says-where-names-came-from`
- `make-sqlite`, `sqlite3-durability`: `the-person-s-way-says-where-names-came-from`
- `sqlite3-stripped`: `the-person-s-way-names-the-hottest-function` (leaf
  `sqlite3VdbeExec.lto_priv.0` through debuginfod: `tests/cases.rs` passes
  `--symbols` as soon as `omn latency --help` lists it),
  `the-person-s-way-says-where-names-came-from`

And two checks **fail** that the patch is the cause of, both
`both-ways-name-the-same-suspect`, because the agent's way
(`tests/cases.rs`, `read_mcp`) still ranks by leaf: `cpython-perf-map` (the
person's `py::score`, the agent's `__tls_get_addr`), and `cpython-interpreter`
(that run read the hint `python3 -X perf` as the suspect; `6745273` took the
backquotes out, so the person's way now names nothing there). Both are
`read_mcp`'s to follow (section 5).

Unit tests (`cargo test --lib triage::diff`, 17; `cli::`, 12): the perf map
cited by a note whose other origins hold `; `; a map read back and a missing
one said; the w17-python sample counts reproduced (1 043 of 1 056 under
`py::score`, the leaf ranking naming `__tls_get_addr`); a flat interpreter
profile recognised by its library alone; a C program's flat profile still
ranked; a dominant C function in an interpreter named; interpreters by name
and not prefix; the fifth frame reached, a stripped executable's frame
reached, 4 and 16 held; the names note under its label; `--symbols` set on
`cpu.profile` and not `sched.runqlat`; `omn diff` naming the Python function
that doubled.

**The suite, on this branch without the patch**: `make check` red, then
`cargo test --workspace --no-fail-fast`: 2 533 passed, 22 failed, 12 ignored,
in 8 targets (`budget`, `cases`, `cross_machine`, `runner`, `scenario_mem`,
`scenario_net`, `triage`, `unwind`), at a load average of about 20 with the
other streams building and testing. Every library test passes, and none of
the failures is in a file this branch changes or reads a function it changed:
without the patch nothing outside `omn diff` calls the new code, and `omn
diff` has no integration test among them. They are timing and sampling
under load (the unwind tests' 300 samples missing a function, `triage`'s
100 kHz profile leaving 268 ms in no row, `budget` 14 in `docs/flaky.md`).
`tests/triage.rs` fails 3 of 25 either way: with the patch,
`a_verdict_read_off_a_lossy_profile...` (alone, twice), `a_program_cpu_bound_then_lock_bound...`
and `an_escalating_run_holds_its_budget...`; without it,
`a_verdict_read_off_a_lossy_profile...`, `four_threads_on_one_cpu_queue...`
and `the_same_program_is_cpu_bound_for_latency...`. None of them asserts on a
name; the first is the undecided limit of a loaded machine in both runs.

**Reasoned, not run**:

- **A map gone before the answer is read**: CPython and V8 leave
  `/tmp/perf-<pid>.map` after exit, and the answer is read right after the
  run, so the read-back is what the collector read. A tmp cleaner between the
  two makes the verdict say so and rank by leaf (unit-tested). A recycled pid
  writing a new map in those seconds could add names; its symbols would have
  to equal labels of this run's stacks to matter.
- **node and a JVM**: the rule is the same file and convention; not run,
  since no case covers them. V8's map names builtins and stubs too, so under
  node the innermost mapped frame can be a builtin: the map's naming, said as
  such.
- **`--symbols debuginfod` on `sched.offcpu`**: set by `ask_symbols` the day
  `reads_symbols("sched.offcpu")` is true (stream sched), with no change here.

## 3. What I could not do, and why

- **Wire it in-branch**: `latency.rs` and `main.rs` are other streams' files;
  the patch is the wiring (top of this file).
- **Name perf-map frames from the results alone**: the labels lose
  `NamedBy`. Reading the map back works and is said when it cannot; carrying
  provenance in the results is a request (section 5).
- **`man/omn.1` for triage's new behaviour** (following a wait into the
  child, naming the busiest other thread): I own the page, but the behaviour
  is not landed; the Verdicts paragraph still says "it answers for one task".
  Triage's handoff should send the sentence; it is a one-paragraph edit at
  the merge.

## 4. Privileged runs wanted

None of this needs a sysctl. One run worth having alone, at
`perf_event_paranoid` 2 without the capabilities, with the patch merged:

```console
sudo sysctl kernel.perf_event_paranoid=2
sudo setcap -r target/debug/omn
target/debug/omn latency --objective latency -- python3 -X perf cases/cpython-interpreter/hot.py <input>
make   # restores the capabilities
sudo sysctl kernel.perf_event_paranoid=<previous>
```

Expected: `cpu.profile` refused or degraded with the machine's reason, and the
verdict's names note absent rather than invented.

## 5. Needs from another stream

- **coordinator, at the merge**: apply `docs/fixes/w18-names-glue.patch`
  (re-fitting the `latency.rs` hunks to triage's version: `Ask::symbols`,
  `diff::ask_symbols` in `session()`, `hottest` and `blocking_stack` calling
  `diff::hottest` and `diff::leafward` with `PerfMapNames::of_run`, the
  `program(run)` helper, `notes` appending `diff::names_notes`, and
  `symbols: None` in the test `ask()`s). Then `cargo test --lib triage` and
  `cargo test --test triage`.
- **triage** (`src/triage/latency.rs`): the same, if you prefer to take it
  in your branch; and when a row names a child or another thread, call
  `diff::hottest`/`diff::leafward` for that task too, and send the man page's
  sentence.
- **launcher** (`src/main.rs`): `symbols: args.symbols` in the
  `latency::Ask` and `saturate::Ask` literals (in the patch).
- **cases** (`tests/cases.rs`, `read_mcp`; `cases/*/case.yml`): read the
  agent's hottest function as `docs/mcp.md` now says (innermost perf-map
  frame, then leaf; a flat interpreter profile names none), which turns
  `both-ways-name-the-same-suspect` on both CPython cases; remove the
  `open_findings` lines listed in section 2 once the patch is merged.
- **sched** (`src/stack.rs`, `src/collect/sched/offcpu.rs`): (a) the names
  note on `sched.offcpu`'s results too, so a run whose profile did not run
  still cites its map; (b) "dynamic symbols only" in the facts line
  (`cpython-interpreter` still fails `the-person-s-way-says-where-names-came-from`
  for that); (c) longer term, the frame's `NamedBy` kept beside the label in
  `Results::stacks` (or a per-result set of perf-map-named labels), so the
  triage stops reading `/tmp` back -- `emit.rs`, so the coordinator's call.

## 6. Decision lines for `DECISIONS.md`

| Decision | Choice | Reason |
|---|---|---|
| What a sample is charged to (eighteenth wave, names) | **Its innermost perf-map frame where its stack has one, else its leaf**, one rule (`src/triage/diff.rs`, `charged_to`) for `omn diff`'s own cost and the triage's hottest function. | Under a perf map the leaf is the runtime's on every sample: `py::score` was under 99 % of them and never a leaf. |
| How a perf-map frame is known | **By reading back the maps the run's names note cites**, not by the spelling of a runtime's names; a map gone is said, and its samples go to their leaves. | Results keep labels, not where each came from; the note and the file are the evidence. A prefix list would guess, and miss every runtime it did not list. |
| A flat interpreter profile | **Said as one, naming no C function**, when the program or a frame's module is an interpreter (a declared list), nothing was charged through a perf map, and no function holds half the samples; with what writes a perf map for that runtime. | The interpreter's own work is in every program it runs; naming `__tls_get_addr` presents it as this one's suspect. Half keeps a C extension that does the work nameable. |
| `--symbols` on the triage | **Set on every request whose collector reads it, on no other**; `omn saturate` takes it and says no request read it. | The question asks several sources at once; handing `debuginfod` to one that refuses it would refuse the whole question. |
| A block's stack | **Leaf first to the program's own frame (a perf-map frame, or the executable's), 4 to 16 frames.** | Four printed none of the Python function, the fifth frame. |
