# Stream "resolver", fifteenth wave — what the fourteenth filed against the resolver's files

The fourteenth wave left the resolver's files with a debuginfod client nothing
reached, an unwind choice the resolved file did not carry, a mode table that
could not sit in `catalog/`, a refusal variant with no resolver half, and a
"no viable backend" refusal that led with the wrong line. All five are landed,
each held to something the tool does not compute. So are the two roadmap exit
criteria this stream was given, and two more requests filed against
`resolve.rs` and `catalog.rs`. The swap to `join::decompose_within` waits on
stream J's commit. What remains is filed under "Needs from another stream —
the fifteenth wave, stream resolver" in `docs/open-findings.md`.

## What changed, by item

### 1. `--symbols`, the unwind choice in the resolved file, and the table in `catalog/`

- **`Request.symbols: Option<SymbolSource>`** (`symbols: local|debuginfod` in
  a session file), **`--symbols SOURCE`** on the command line, and the manual
  page (the flag, `DEBUGINFOD_URLS`, `DEBUGINFOD_CACHE_PATH`).
- **The choice carries it.** `StackChoice` gained `symbols` and
  `symbol_servers`. `stack::choose` refuses `debuginfod` when
  `DEBUGINFOD_URLS` is unset or empty, before anything is launched, and
  otherwise records the servers. `profile::Prepare` builds its client from
  the choice (`StackChoice::debuginfod`, over elfutils' cache) when none is
  handed in. So U's filed line in `src/collect/cpu/mod.rs` is no longer
  needed, and that file is untouched.
- **A source that would never ask is refused.** `stack::UNWOUND_BY_CHOICE`
  lists `(source, backend)` pairs; today it holds only `cpu.profile` on
  `pmu:sw`. `--symbols debuginfod` on any other source exits 3 before
  resolution. Before this, it would have been accepted and never asked,
  which is a silent degradation.
- **`ResolvedRequest.stack: Option<StackChoice>`** is set by the resolver
  (`resolve::build`, through `stack::recorded_choice`) for the pairs in
  `UNWOUND_BY_CHOICE`. `--dump-resolved` and `resolved.json` therefore state
  the mode, every mode considered with its verdict, and the degradations
  before the run. The IBS sources are left out on purpose: they serve frame
  pointers whatever `choose` picks. A choice that `choose` refuses is left
  `None`, and the factory then refuses it before the launch with its own
  exit code.
- **`catalog/unwind.yaml`** (a `git mv` from `src/unwind.yaml`).
  `Catalog::load_dir` skips `catalog::UNWIND_TABLE`. A directory given to
  `--catalog` or `OMN_CATALOG_DIR` supplies its own modes through
  `stack::unwind::use_table_in`, loaded under the same rules. Without an
  `unwind.yaml` in it, the compiled-in modes are used.

### 2. `KeyNotCarried`'s resolver half

`Backend.keys: Option<Vec<KeyKind>>` has the same shape as `filter_fields`:
absent means the entry's keys, present means a subset. It comes with
`Entry::backend_keys`, `Entry::backends_carrying_key`, and a loader check,
`check_backend_keys`, that works in both directions. A backend cannot attach
a key the entry does not offer, and every key the entry offers must be
attached by some backend.

`resolve::check_keys_carried` runs wherever `check_filter_fields` runs: on the
pinned path, on the walk, and for the fallback candidates. It returns
`ResolveError::KeyNotCarried` (exit 2), which names the first backend that
does attach the key. No shipped entry declares `keys:` yet. The first one is
`io.bytes`, filed for the owner of `catalog/io.yaml`.

### 3. The "no viable backend" refusal leads with the backend passed over

Suppose a chain ends on a backend with no collector, and a backend passed
over on the way is one this build collects and one that serves the request's
target and mode. The first line now names that backend and why it was passed
over: the line a reader can act on. A backend passed over for its target or
mode, or one nobody wrote either, does not lead; the build's own sentence
stays first then, and is the true one. Measured with a copy
of the binary in a directory with no shim:

```
error: `mem.heap.alloc` has no viable backend on this machine: `preload`, which this build collects, was passed over -- preload:shim: missing: the preload shim was not found at <dir>/libomn_preload.so; ...; `bpf:usdt`, the backend left, has no collector written yet
```

The first line is not repeated among the "passed over" lines. There is one
change of answer: when every line leads with a backend this build collects,
and that backend fits the request's target and mode (`resolve::serves`), the
exit code is 1 (the machine's), not 3. The trailing sentence says the build
collects it. "A build that has it" was false in that case: this build has it.

### 4. `decompose_within`

`src/main.rs` calls a local `decompose_within(.., &windows)` with the final
signature. For now its body calls `join::decompose_with` and ignores the
windows, and `check_paired_parts` still refuses every `paired` part, so the
path where the two would differ cannot be reached. The remaining diff is
filed exactly: one body, one call in `triage/engine.rs`, and two
`check_paired_parts` calls to remove.

### 5. The phase 2 and phase 7 audits

- **Phase 2**: `omn --dump-resolved io.latency --pid <a live sleep>` is
  deserialized into `ResolvedRequest`, and re-serializing gives the same
  document, so every field is present and there is nothing else. The dumped
  machine is compared with the machine asked another way:
  - `uname -r` and `-m`, and `nproc`;
  - the clocksource, governor, SMT and THP files;
  - `perf_event_paranoid`;
  - every file of `/sys/devices/system/cpu/vulnerabilities`.

  The chain is the catalog's, in the catalog's order. The selected backend
  is the first one not rejected, every backend above it was rejected with a
  reason, and every backend below it was not reached. Its `btf` and
  `kernel>=` requirements are checked against `/sys/kernel/btf/vmlinux` and
  `uname`.
- **Phase 7**: the workload is a C program this test writes, making exactly
  1000 allocations of 77777 bytes and freeing each one; it prints its own
  count. It is measured twice: from the command line, and from the session
  file that the same command line dumps. Both runs match the program's own
  numbers (count 1000, min = max = 77777, 1000 kept by the filter). The
  replay's series, filtered counts and events seen equal the command line's
  exactly. The shim sees every call, so the workload's resolution is one
  allocation. Both runs resolved the same request onto the same backend.

`ROADMAP.md` is not this stream's file. Both criteria can move to `[x]`,
citing the two tests below.

### 6. Filed items, and more filed against these files

- `pub mod net`, the `psi` declaration and `ProgramStats::for_run` onto
  `bpf::enable_run_time_stats` are all in `src/collect/mod.rs`, which is not
  this stream's file this wave. They are filed with the exact lines.
- **`Cost::Extreme`'s doc comment** (J's request of the fourteenth wave) now
  says "over 20 %, with no bound above", in `worst_case_percent`'s doc as
  well.
- **A target and mode no backend declares together is the request's** (K's
  request, `resolve.rs`). `ResolveError::NoBackendDeclares` exits 2 with a
  hint. It applies only when the entry itself declares both the target and
  the mode; anything else stays the validator's refusal, as before. On this
  machine it gives:

  ```
  error: `sched.switches` cannot serve a `cgroup` target in `trace` mode: no backend of its chain declares the two together:
    pmu:sw: `pmu:sw` cannot attribute per cgroup: ...
    bpf:raw_tp: `bpf:raw_tp` serves `count` mode, not `trace`
    bpf:tp: `bpf:tp` serves `count` mode, not `trace`
  ```

  The pinned path is unchanged: a pin that cannot serve the target still
  reads as `NoViableBackend`, with "pinned" in the line.

## Files outside this stream's list

`Request` gained a field, and Rust has no default for a field left out of a
literal. Thirteen `Request { .. }` literals outside this stream's files each
gained exactly one line, `symbols: None,`, after `stack:`:

- `src/collect/mod.rs` (seven test fixtures)
- `src/render.rs`
- `src/validate.rs`
- `src/triage/latency.rs`
- `src/triage/saturate.rs`
- `tests/catalog_model.rs`
- `tests/runner.rs`

These are declared in `docs/open-findings.md`. A conflict at merge is that
one line. There was no alternative: a field in `Request` was what U asked
for, and any other home for `--symbols` would have broken as many literals
(`Session`) or changed the meaning of an existing field.

## Checks

`make check` was red twice on this branch, both times at load averages of 15
to 30, with six streams and a VM sweep sharing the machine. Every test that
failed passed when rerun alone, once.

- **First run.** Failed:
  - `cross_machine::a_memory_limit_turns_a_cached_read_into_device_reads...`;
  - `--no-fail-fast`: the `scenario_net` pair already in the tally,
    `scenario_proc::defect_1/2`, `recorder::two_bursts...`,
    `cross_memory::the_resident_size...`, three `scenario_mem`, two
    `triage`, `unwind::fp_leaf...` and `dwarf_recovers...`, and
    `profile::tests::a_profile_of_the_machine...`.

  Three were this branch's, and they were fixed before the second run:
  - `family_io`, `scenario_io::each_refusal...` and `scenario_power`'s two
    tests were a refusal lead that also fired for a backend passed over for
    its target or mode. It now fires only for a collected backend that fits
    the request, and says "has no viable backend on this machine" with it.
  - The new debuginfod test compared exact stack sets. Under load a few cut
    stacks broke that, so it now checks the branches of the known tree.
- **Second run.** Failed: `scenario_proc::a_known_number_of_calls...` (9 of 10
  sleeps) and `defect_1`, both poll-timing tests outside this stream's paths.
  Clippy and fmt are clean. This branch's own files all pass (`cli`,
  `unwind`, the lib tests of `catalog`, `resolve`, `stack`, `request`, `cli`
  and `collect::cpu::profile`, and `catalog_model`).

- **Third run, after the review's fixes.** `make check` failed at
  `cross_compute::a_lossy_profile_marks_its_row...` (`sched.runqlat` 16.0 ms
  against the kernel's 25.1 ms), load average 16. The one
  `--no-fail-fast` run then failed `scenario_sched::one_known_number_of_round_trips...`
  (3071 waits for 2000) and `triage::a_verdict_read_off_a_lossy_profile...`,
  and passed `cross_compute`. All three passed alone, once, and none touches a
  path this branch changed. Clippy (`--workspace --all-targets -D warnings`)
  and fmt are clean. While iterating, `unwind::fp_leaf_recovers...` failed once
  in a run of `tests/unwind.rs` and passed alone.

For the flaky tally: `scenario_proc::defect_1_the_first_reading...` failed in
both full runs (8 polls against 7, filtered against unfiltered) and passed
alone. `scenario_proc::a_known_number_of_calls_and_an_injected_delay...`
counted 9 of 10 sleeps once.

## Landed, each with its test

| What | Test |
|---|---|
| `--symbols debuginfod` from the CLI and from a session file, names fetched from a loopback server serving the unstripped build | `tests/unwind.rs::symbols_from_debuginfod_are_asked_for_from_the_command_line_and_the_session_file` |
| no `DEBUGINFOD_URLS`: refused before the program starts; a non-reading source: exit 3 | `tests/unwind.rs::symbols_from_debuginfod_with_nowhere_to_ask_are_refused_before_the_program_starts` |
| `StackChoice` in `--dump-resolved` and `resolved.json`, equal to the mode the profile's notes say it used | `tests/unwind.rs::the_resolved_file_says_how_the_stacks_will_be_unwound_before_the_run` |
| `catalog/unwind.yaml`, skipped by `load_dir` | `catalog::tests::a_copy_of_the_catalog_directory_loads_without_reading_the_unwind_table_as_entries` |
| the unwind modes read from a `--catalog` directory | `tests/cli.rs::the_unwind_modes_are_read_from_the_catalog_directory_they_sit_in` |
| `Backend.keys` and its loader check | `catalog::tests::a_backend_attaching_a_key_its_entry_does_not_offer_is_refused` |
| `KeyNotCarried` from the resolver | `resolve::tests::a_key_the_selected_backend_does_not_attach_is_refused_naming_one_that_does`; `tests/cli.rs::a_request_keyed_on_what_its_backend_does_not_attach_is_refused_with_the_backend_that_does` (held to `/proc/diskstats`, whose events carry no task) |
| `KeyNotCarried` names a backend the request can be moved to, or says what keeps it out: pinnable (exit 2), missing on this machine (exit 1), unwritten in this build (exit 3), or unfit for the target or mode (exit 2) | `resolve::tests::a_backend_that_attaches_the_key_is_named_for_what_keeps_the_request_from_it` (held to `std::fs` on `/proc/diskstats` and on a path that does not exist, and to `fs:cgroup`'s declared targets); the CLI test above now follows the hint it prints and checks the run it leads to |
| a stack choice the run refuses is refused by `--dump-resolved`, with the same reason and exit code | `tests/unwind.rs::a_choice_the_run_refuses_is_refused_by_the_dump_with_the_same_reason` (held to the run's own refusal, made before the program starts) |
| the refusal's first line | `tests/cli.rs::a_refusal_for_an_unwritten_backend_leads_with_the_one_passed_over` (a binary with no shim beside it) |
| phase 2: a complete `ResolvedRequest`, checked against the machine | `tests/cli.rs::the_resolved_request_for_a_live_pid_is_complete_and_says_what_the_machine_says` |
| phase 7: CLI, then its file, gives the same numbers as the program's own | `tests/cli.rs::a_session_dumped_from_the_command_line_measures_what_the_command_line_measured` |
| a (target, mode) pair no backend declares: exit 2 | `tests/cli.rs::a_target_and_mode_no_backend_declares_together_is_refused_as_the_request_s` (the pairs read from the catalog's per-backend declarations) |
| `decompose_within` call shape in `main.rs` | none of its own: it calls `decompose_with` unchanged, so every existing decomposition test covers it |

## Open, with the reason

- **The `decompose_within` swap.** Stream J's commit does not exist during
  this run. The exact diff is filed.
- **The `collect/mod.rs` lines** (`pub mod net`, `pub mod psi`,
  `enable_run_time_stats`). That file is not this stream's this wave; filed.
- **No shipped entry declares `Backend.keys`.** `catalog/io.yaml` is not this
  stream's; the two lines are filed. Until they land, the refusal can only be
  reached with a patched catalog, which is what the CLI test uses.
- **Debuginfod against a real server** was never tried. The rules forbid the
  network. The loopback server speaks the real protocol
  (`GET /buildid/<hex>/debuginfo`); HTTPS goes through `curl`, which was not
  exercised against TLS here.

## Needs privilege

Nothing. The tests above ran with the capabilities `make` grants, at
`perf_event_paranoid` -1. Without the capabilities, the phase 2 test skips
(`io.latency --pid` needs eBPF) with the resolver's refusal. The others run.

## For DECISIONS.md

- **`--symbols debuginfod` on a source whose collector would not ask is
  refused with exit 3**, the build's gap. It is not accepted and ignored,
  because a network request silently not made is a degradation nobody was
  told about. Which collectors read it is `stack::UNWOUND_BY_CHOICE`.
- **The resolved file records the unwind choice only for collectors that use
  it as chosen.** The IBS sources ask `choose` and then serve frame pointers,
  so recording the choice for them would misstate their stacks.
- **A request keyed on a key the selected backend does not attach is
  refused, naming the backend that does attach it and what stands between
  the request and it.** The chain is not walked on to that backend: which
  backend serves is the resolver's decision, and a key is part of the
  question. The `--backend force:` hint is given only for a backend that
  serves the target and mode, that this build collects, and whose
  requirements this machine does not lack; the exit code is that backend's
  gap (request 2, machine 1, build 3). Among several, chain order within the
  best kind: forceable, then missing, then unwritten, then unfit.
- **A stack choice `choose` refuses is a resolver refusal
  (`ResolveError::StackRefused`)**, so the dump refuses what the run would.
  Its code is `unwind::Refusal::whose`; a forced mode this machine cannot
  serve is held at exit 3 until `tests/scenario_cpu.rs` takes the filed
  change to exit 1.
- **A no-collector refusal is the machine's (exit 1) when the backend passed
  over is one this build collects and that fits the request.** In that case
  the build is not what is missing; the machine or the install is.
- **A (target, mode) pair that the entry declares, but no one backend
  declares together, is the request's (exit 2).** A mode or target the entry
  itself refuses remains the validator's refusal. A pinned backend that
  cannot serve the target keeps its "pinned" refusal.
- **The unwind table is `catalog/unwind.yaml`, compiled in, and read from a
  `--catalog` directory when that directory has one.**

## Filed for other streams

Under "Needs from another stream — the fifteenth wave, stream resolver" in
`docs/open-findings.md`:

- the thirteen `symbols: None,` lines, declared;
- `src/collect/mod.rs`: `pub mod net`, `pub mod psi`,
  `ProgramStats::for_run` onto `bpf::enable_run_time_stats`;
- J (`src/join.rs`): `decompose_within`, then the coordinator's swap;
- `src/triage/engine.rs`: `NoBackendDeclares` should map to `Whose::Request`,
  and `KeyNotCarried` / `StackRefused` to the `whose` they carry;
- `tests/scenario_cpu.rs` with `src/unwind.rs`: a forced `--stack` mode this
  machine cannot serve moves from exit 3 to exit 1, in one commit;
- the coordinator: `STATE.md:348` still says `src/unwind.yaml`;
- the coordinator: `CATALOG.md:198` and `DECISIONS.md:443` should name
  `catalog/unwind.yaml`;
- `src/collect/cpu/mod.rs` (optional): the factory could take
  `resolved.stack`;
- `catalog/io.yaml`: `keys:` on `io.bytes`'s `fs:proc` and `fs:cgroup`, and
  on `io.iops`'s `fs:proc`.

## Review, after the first two commits

Six findings. Five were real and are fixed in the third commit; the sixth is
a test run, not a defect.

1. **`KeyNotCarried` named a backend this build cannot collect.** Real. The
   hint was the chain's first backend that attaches the key, so with the
   patched catalog it said `--backend force:bpf:raw_tp`, and following it gave
   "no collector is written for `io.bytes` on backend `bpf:raw_tp`", exit 3.
   `elsewhere` is now a `KeyElsewhere` that says whether the backend can be
   forced, is missing something here, is unwritten, or cannot serve the
   target or mode. Only the first gets a `--backend force:` line, and the exit
   code follows it. The CLI test now follows the hint it is given and holds
   the next run to it.
2. **A refused stack choice vanished from the resolved file.** Real.
   `recorded_choice` returns the refusal now, and the resolver refuses with
   it (`StackRefused`), with `choose`'s own reason. `env -u DEBUGINFOD_URLS
   omn --dump-resolved cpu.profile ... --symbols debuginfod -- true` is
   refused with exit 1 and prints nothing to stdout. The same goes for a forced
   `--stack lbr` or `sframe` this machine cannot serve (exit 3, held; see For
   DECISIONS.md).
3. **`serves` took `cannot_serve_target`'s doc comment.** Real. `serves`
   now sits above that doc block.
4. **"Eleven" `symbols: None,` lines.** Real: thirteen (seven in
   `src/collect/mod.rs`, six elsewhere). Corrected here and in
   `docs/open-findings.md`. The only other edits outside the owned list are
   the ones in `src/collect/cpu/profile.rs`, which is owned.
5. **`docs/unwind.md` said things this stream made false.** Real. Lines 284,
   424, 431, 437 and the "For DECISIONS.md" line now say that each item closed
   in the fifteenth wave, and point to its section. That section gains the
   refusal of item 2. `STATE.md:348` is not this stream's, so it is filed.
6. **The reviewer's test run.** Not a defect. It confirmed that the new tests
   take their full paths with the capabilities present.

Still open from this, in this stream's own file: `FilterFieldNotCarried`
picks its `elsewhere` the old way, as the first backend that carries the
field, with no check that it is collected or runnable here. This is the
same shape as item 1. Nobody has reproduced a wrong hint from it, and this
stream did not survey the shipped catalog for a case that would produce
one. The same helper (`key_elsewhere`) would fix it. It was left unchanged
because there is no case to test it against.

## Session index rows

None. This stream added no session file.
